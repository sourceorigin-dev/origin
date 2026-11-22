//! The op log: what this node applied, in order. It is local, never synced
//! with other nodes, and shared by every workspace of the repository, so
//! each entry says which workspace, and which command, it came from. Its
//! entries record:
//!
//! - records written: revisions, statements and acceptances, with what the
//!   [index](crate::index) keeps of them, so also every landing on a target;
//! - targets advanced: a head naming a new acceptance;
//! - landings left waiting in a queue.
//!
//! Each entry has a sequence number, one more than the entry before it,
//! which never changes. The entries one command run appends form an
//! operation, numbered by its first entry's sequence number.
//!
//! ```text
//! .origin/oplog/
//!   lock                       held exclusively while appending
//!   <first sequence number>    segments of entries, 16 hex digits each
//! ```
//!
//! A segment is a file of [frames](crate::frames), an entry each. Entries are
//! appended to the last segment until it's full, then to a new one, and the
//! oldest segments are deleted beyond a cap: the log keeps the latest 16 MiB
//! of entries, and a cursor older than that has expired.
//!
//! **Writing.** Each write appends its entries ([`Store::commit_logged`])
//! just before it's committed, and they're synced with it
//! ([`DurableWrite::sync_with`]): records and waiting landings with their
//! files, and a target's head with its new acceptance.
//!
//! **Partial appends.** A crash in the middle of an append leaves part of a
//! frame at the end of the last segment. It never was an entry: readers
//! skip it, and the next append cuts it off.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::iter;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use origin_platform::{DurableWrite, LockMode, LockedFile, Platform};
use origin_shared::object::{DecodeError, Reader};
use origin_shared::{AcceptanceId, ChangeId, ObjectId, TargetName};

use crate::frames::{Frames, Writer};
use crate::index;
use crate::queue::Proposal;
use crate::store::{Store, StoreError};

/// Who a store writes for, as the op log records it: a command, and the
/// workspace running it, if any.
#[derive(Clone, Debug, Default)]
pub struct Actor {
    workspace: Option<PathBuf>,
    command: String,
    /// The operation: the sequence number of the first entry appended for
    /// it, once there is one. Clones share it.
    op: Arc<OnceLock<u64>>,
}

impl Actor {
    /// A new operation running `command`, such as `land 3fa2`, outside any
    /// workspace.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            ..Self::default()
        }
    }

    /// The same operation, in the workspace at `root`.
    #[must_use]
    pub fn in_workspace(&self, root: &Path) -> Self {
        Self {
            workspace: Some(root.to_owned()),
            ..self.clone()
        }
    }

    /// The workspace, if any.
    #[must_use]
    pub fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }
}

/// Entries read from the op log, oldest first.
#[derive(Debug, Default)]
pub struct Read {
    /// The entries.
    pub entries: Vec<Entry>,
}

/// An entry of the op log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its sequence number: the one before it, plus one.
    pub seq: u64,
    /// The operation it belongs to: its first entry's sequence number.
    pub op: u64,
    /// The root of the workspace it came from, if any.
    pub workspace: Option<PathBuf>,
    /// The command that appended it, such as `land 3fa2`.
    pub command: String,
    /// What happened.
    pub effect: Effect,
}

/// What an entry records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Records were written: revisions, statements and acceptances, with
    /// what the index keeps of them.
    Wrote(Vec<index::Entry>),
    /// A landing waits in a queue, for evidence or a conflict fix.
    Waiting {
        /// The target it lands on.
        onto: TargetName,
        /// What lands.
        proposal: Proposal,
    },
    /// A target's head named a new acceptance.
    Advanced {
        /// The target.
        target: TargetName,
        /// Its position on the target.
        seq: u64,
        /// The acceptance.
        acceptance: AcceptanceId,
    },
}

impl Entry {
    const WROTE: u8 = 1;
    const WAITING: u8 = 2;
    const ADVANCED: u8 = 4;

    /// Encodes the entry's frame: its sequence number, operation, workspace
    /// and command, then a tag and the effect's fields.
    fn encode(&self) -> Vec<u8> {
        Frames::frame(|out| {
            out.u64(self.seq);
            out.u64(self.op);
            match &self.workspace {
                None => out.u8(0),
                Some(root) => {
                    out.u8(1);
                    out.prefixed(root.as_os_str().as_bytes());
                }
            }
            out.prefixed(self.command.as_bytes());
            self.effect.write(out);
        })
    }

    fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(payload);
        let seq = reader.u64()?;
        let op = reader.u64()?;
        let workspace =
            reader.option(|reader| Ok(PathBuf::from(OsStr::from_bytes(reader.prefixed()?))))?;
        let command = reader.prefixed_str()?.to_owned();
        let effect = Effect::read(&mut reader)?;
        reader.finish()?;
        Ok(Self {
            seq,
            op,
            workspace,
            command,
            effect,
        })
    }
}

impl Effect {
    fn write(&self, out: &mut Writer) {
        match self {
            Self::Wrote(records) => {
                out.u8(Entry::WROTE);
                out.len(records.len());
                for record in records {
                    record.write(out);
                }
            }
            Self::Waiting { onto, proposal } => {
                out.u8(Entry::WAITING);
                out.prefixed(onto.as_str().as_bytes());
                match proposal {
                    Proposal::Change(change) => {
                        out.u8(0);
                        out.bytes(change.as_bytes());
                    }
                    Proposal::Target(target) => {
                        out.u8(1);
                        out.prefixed(target.as_str().as_bytes());
                    }
                }
            }
            Self::Advanced {
                target,
                seq,
                acceptance,
            } => {
                out.u8(Entry::ADVANCED);
                out.prefixed(target.as_str().as_bytes());
                out.u64(*seq);
                out.bytes(acceptance.as_bytes());
            }
        }
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let target = |reader: &mut Reader<'_>| -> Result<TargetName, DecodeError> {
            Ok(TargetName::new(reader.prefixed_str()?.to_owned())?)
        };
        let unknown = |field, tag| Err(DecodeError::UnknownTag { field, tag });
        Ok(match reader.u8()? {
            Entry::WROTE => {
                let count = reader.read_len()?;
                let records = iter::repeat_with(|| index::Entry::read(reader))
                    .take(count)
                    .collect::<Result<_, _>>()?;
                Self::Wrote(records)
            }
            Entry::WAITING => Self::Waiting {
                onto: target(reader)?,
                proposal: match reader.u8()? {
                    0 => Proposal::Change(ChangeId::from_bytes(reader.array()?)),
                    1 => Proposal::Target(target(reader)?),
                    tag => return unknown("proposal", tag),
                },
            },
            Entry::ADVANCED => Self::Advanced {
                target: target(reader)?,
                seq: reader.u64()?,
                acceptance: AcceptanceId::from(ObjectId::from_bytes(reader.array()?)),
            },
            tag => return unknown("op log effect", tag),
        })
    }
}

/// The op log of a store, for reading.
#[derive(Debug)]
pub struct OpLog<'a, P> {
    store: &'a Store<P>,
}

impl<'a, P: Platform> OpLog<'a, P> {
    /// The format of a segment, and its version.
    const FRAMES: Frames = Frames::new(b"origin oplog 1\n");

    /// A segment this large is full: the next entry starts a new one.
    const SEGMENT_BYTES: u64 = if cfg!(test) { 1024 } else { 256 << 10 };

    /// The most segments kept, 16 MiB of entries, about 100,000.
    const SEGMENTS: usize = if cfg!(test) { 4 } else { 64 };

    /// The lock held while appending.
    const APPENDING: &str = "lock";

    /// The op log of `store`.
    pub fn new(store: &'a Store<P>) -> Self {
        Self { store }
    }

    fn dir(&self) -> PathBuf {
        self.store.root().join("oplog")
    }

    fn segment(&self, first: u64) -> PathBuf {
        self.dir().join(format!("{first:016x}"))
    }

    /// Creates the log's directory and first segment, durably, so the first
    /// entry costs no more than the next. Repositories made before the op
    /// log existed get them with their first entry.
    pub(crate) fn init(&self) -> Result<(), StoreError> {
        let dir = self.dir();
        fs::create_dir_all(&dir).map_err(|source| StoreError::io(&dir, source))?;
        self.start_segment(1, &[])?;
        Ok(())
    }

    /// Opens and locks one of the log's lock files, creating the log's
    /// directory, durably, if it's missing.
    fn lock(&self, name: &str, mode: LockMode) -> Result<LockedFile, StoreError> {
        let dir = self.dir();
        if !dir.is_dir() {
            fs::create_dir_all(&dir).map_err(|source| StoreError::io(&dir, source))?;
            self.store.sync_dir(self.store.root())?;
        }
        Ok(LockedFile::open(
            &self.store.platform,
            &dir.join(name),
            mode,
        )?)
    }

    /// The first sequence number of each segment, oldest first.
    fn segments(&self) -> Result<Vec<u64>, StoreError> {
        let dir = self.dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(StoreError::io(&dir, source)),
        };
        let mut segments = Vec::new();
        for entry in entries {
            let name = entry
                .map_err(|source| StoreError::io(&dir, source))?
                .file_name();
            // Anything else, such as the locks, isn't a segment.
            segments.extend(
                name.to_str()
                    .filter(|name| name.len() == 16)
                    .and_then(|name| u64::from_str_radix(name, 16).ok()),
            );
        }
        segments.sort_unstable();
        Ok(segments)
    }

    /// The entries of the segment starting at `first`, or `None` if
    /// retention deleted it. Those after `after` at least: when they're at
    /// its end, which is usual, only they are read, backwards from the end.
    fn read_segment(&self, first: u64, after: u64) -> Result<Option<Read>, StoreError> {
        let path = self.segment(first);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StoreError::io(&path, source)),
        };
        if let Some(entries) =
            Self::FRAMES.parse_back(&bytes, Entry::decode, |entry| entry.seq > after)
        {
            return Ok(Some(Read { entries }));
        }
        let (frames, _) = Self::FRAMES.parse(&bytes, Entry::decode);
        let entries = frames.into_iter().map(|(_, entry)| entry).collect();
        Ok(Some(Read { entries }))
    }

    /// The sequence number of the latest entry, or 0 if there's none.
    ///
    /// # Errors
    ///
    /// Fails if the log cannot be read.
    pub fn last_seq(&self) -> Result<u64, StoreError> {
        let Some(&first) = self.segments()?.last() else {
            return Ok(0);
        };
        let path = self.segment(first);
        let peeked = match File::open(&path).and_then(|file| Self::FRAMES.peek_last(&file)) {
            Ok(peeked) => peeked,
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(StoreError::io(&path, source)),
        };
        let last = match peeked {
            Some(Some(payload)) => Reader::new(&payload).u64().ok(),
            Some(None) => Some(first - 1),
            // A partial or damaged end: read up to it.
            None => None,
        };
        if let Some(last) = last {
            return Ok(last);
        }
        Ok(self
            .read_segment(first, 0)?
            .and_then(|read| read.entries.last().map(|entry| entry.seq))
            .unwrap_or(first - 1))
    }

    /// The entries after the sequence number `after`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Expired`] if the log no longer keeps the
    /// entry after `after`, or if it cannot be read.
    pub fn after(&self, after: u64) -> Result<Read, StoreError> {
        // The common case of a poll: nothing new, known from the last frame.
        if self.last_seq()? <= after {
            return Ok(Read::default());
        }
        let segments = self.segments()?;
        let expired = |oldest| StoreError::Expired { after, oldest };
        if let Some(&oldest) = segments.first()
            && oldest > after + 1
        {
            return Err(expired(oldest));
        }
        // The segment holding the entry after `after`, and those after it.
        let start = segments.partition_point(|&first| first <= after + 1);
        let mut read = Read::default();
        for &first in &segments[start.saturating_sub(1)..] {
            let segment = self
                .read_segment(first, after)?
                .ok_or_else(|| expired(first))?;
            read.entries.extend(
                segment
                    .entries
                    .into_iter()
                    .filter(|entry| entry.seq > after),
            );
        }
        Ok(read)
    }

    /// Every entry the log keeps, newest first, read a segment at a time.
    ///
    /// # Errors
    ///
    /// Fails if the segments cannot be listed, or, as an item, if one cannot
    /// be read.
    pub fn newest_first(
        &self,
    ) -> Result<impl Iterator<Item = Result<Entry, StoreError>> + '_, StoreError> {
        let segments = self.segments()?;
        Ok(segments
            .into_iter()
            .rev()
            .flat_map(|first| match self.read_segment(first, 0) {
                Ok(read) => read
                    .map(|read| read.entries)
                    .unwrap_or_default()
                    .into_iter()
                    .rev()
                    .map(Ok)
                    .collect(),
                Err(error) => vec![Err(error)],
            }))
    }

    /// Appends an entry for `effect`, as the store's actor, without syncing
    /// it, returning its sequence number and the segment it's in.
    fn append(&self, effect: &Effect) -> Result<(u64, PathBuf), StoreError> {
        let _lock = self.lock(Self::APPENDING, LockMode::Exclusive)?;
        let segments = self.segments()?;
        let (seq, open) = match segments.last() {
            None => (1, None),
            Some(&first) => self.tail(first)?,
        };
        let segment = match open {
            Some(segment) => segment,
            None => self.start_segment(seq, &segments)?,
        };
        let actor = &self.store.actor;
        let entry = Entry {
            seq,
            op: actor.op.get().copied().unwrap_or(seq),
            workspace: actor.workspace.clone(),
            command: actor.command.clone(),
            effect: effect.clone(),
        };
        Self::FRAMES
            .append(&self.store.platform, &segment.file, &entry.encode())
            .map_err(|source| StoreError::io(&segment.path, source))?;
        actor.op.get_or_init(|| seq);
        Ok((seq, segment.path))
    }

    /// The next sequence number after the last segment, which starts at
    /// `first`, and that segment, opened for appending, unless it's full,
    /// and the next entry starts a new one.
    fn tail(&self, first: u64) -> Result<(u64, Option<Segment>), StoreError> {
        let path = self.segment(first);
        let io_error = |source| StoreError::io(&path, source);
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .map_err(io_error)?;
        let last = match Self::FRAMES.last(&self.store.platform, &file) {
            Ok(Some(payload)) => {
                Reader::new(&payload)
                    .u64()
                    .map_err(|source| StoreError::DecodeFile {
                        path: path.clone(),
                        source,
                    })?
            }
            Ok(None) => first - 1,
            Err(source) => return Err(io_error(source)),
        };
        let full = file.metadata().map_err(io_error)?.len() >= Self::SEGMENT_BYTES;
        Ok((last + 1, (!full).then_some(Segment { path, file })))
    }

    /// Creates the segment starting at `first`, holding only its header,
    /// durably, and deletes the oldest of `segments` beyond the cap. It's
    /// published whole, with its header, so no reader ever finds a segment
    /// shorter than its header, nor does a crash leave one: only a
    /// temporary file, which isn't a segment, and is swept. It never
    /// replaces a segment: those holding the appending lock create them,
    /// and `init` creates the first, of only a header, before anyone
    /// appends.
    fn start_segment(&self, first: u64, segments: &[u64]) -> Result<Segment, StoreError> {
        let path = self.segment(first);
        let io_error = |source| StoreError::io(&path, source);
        if path.symlink_metadata().is_ok() {
            return Err(io_error(io::ErrorKind::AlreadyExists.into()));
        }
        // Its entries are synced with their writes; its name, and its
        // header, now, so the first entry's sync costs no more than the
        // next's.
        let write = self.store.durable_write();
        write
            .stage(&path, Self::FRAMES.header())
            .and_then(|()| write.commit())
            .map_err(StoreError::from)?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .map_err(io_error)?;
        let excess = (segments.len() + 1).saturating_sub(Self::SEGMENTS);
        for &old in &segments[..excess] {
            // Best effort: a segment left behind is deleted with the next.
            let _ = fs::remove_file(self.segment(old));
        }
        Ok(Segment { path, file })
    }
}

/// A segment open for appending.
struct Segment {
    path: PathBuf,
    file: File,
}

impl<P: Platform> Store<P> {
    /// The op log of the store.
    #[must_use]
    pub fn oplog(&self) -> OpLog<'_, P> {
        OpLog::new(self)
    }

    /// Commits `write`, logging `effects` as entries synced with it, at no
    /// extra sync, returning the last entry's sequence number.
    ///
    /// # Errors
    ///
    /// Fails if an entry cannot be appended or the write committed.
    pub(crate) fn commit_logged(
        &self,
        write: DurableWrite<'_, P>,
        effects: &[Effect],
    ) -> Result<u64, StoreError> {
        let mut seq = 0;
        let mut segments = Vec::new();
        for effect in effects {
            let (appended, segment) = self.oplog().append(effect)?;
            seq = appended;
            if !segments.contains(&segment) {
                segments.push(segment);
            }
        }
        for segment in &segments {
            write.sync_with(segment);
        }
        write.commit()?;
        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::slice;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use origin_platform::Native;
    use origin_shared::{RecordId, StatementId};

    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default())
            .with_actor(Actor::new("test").in_workspace(dir.path()));
        store.init().unwrap();
        (dir, store)
    }

    /// Logs `effect` with an empty write.
    fn log(store: &Store, effect: &Effect) -> u64 {
        store
            .commit_logged(store.durable_write(), slice::from_ref(effect))
            .unwrap()
    }

    fn wrote(seed: u8) -> Effect {
        Effect::Wrote(vec![index::Entry::Statement {
            id: StatementId::from(ObjectId::from_bytes([seed; 32])),
            subject: RecordId::Statement(ObjectId::from_bytes([seed; 32]).into()),
            supersedes: None,
            group: None,
        }])
    }

    #[test]
    fn entries_round_trip() {
        let effects = [
            wrote(1),
            Effect::Waiting {
                onto: TargetName::MAIN,
                proposal: Proposal::Change(ChangeId::from_bytes([3; 16])),
            },
            Effect::Waiting {
                onto: TargetName::new("release/v3").unwrap(),
                proposal: Proposal::Target(TargetName::MAIN),
            },
            Effect::Advanced {
                target: TargetName::MAIN,
                seq: 3,
                acceptance: AcceptanceId::from(ObjectId::from_bytes([6; 32])),
            },
        ];
        for (seq, effect) in (1..).zip(effects) {
            let entry = Entry {
                seq,
                op: 1,
                workspace: (seq % 2 == 0).then(|| PathBuf::from("/work/ws")),
                command: "land 3fa2".to_owned(),
                effect,
            };
            let frame = entry.encode();
            let payload = &frame[4..frame.len() - Frames::FRAMING + 4];
            assert_eq!(Entry::decode(payload), Ok(entry));
        }
    }

    #[test]
    fn sequence_numbers_continue_across_segments_and_retention() {
        let (_dir, store) = store();
        let oplog = store.oplog();
        assert_eq!(oplog.last_seq().unwrap(), 0);
        assert_eq!(oplog.after(0).unwrap().entries, []);
        let seqs: Vec<u64> = (0..200).map(|i| log(&store, &wrote(i))).collect();
        assert_eq!(seqs, (1..=200).collect::<Vec<_>>());
        assert_eq!(oplog.last_seq().unwrap(), 200);

        let segments = oplog.segments().unwrap();
        assert_eq!(
            segments.len(),
            OpLog::<Native>::SEGMENTS,
            "the oldest were deleted"
        );
        let oldest = segments[0];
        assert!(matches!(
            oplog.after(0),
            Err(StoreError::Expired { after: 0, oldest: o }) if o == oldest
        ));
        let kept = oplog.after(oldest - 1).unwrap().entries;
        assert_eq!(kept.first().unwrap().seq, oldest);
        assert!(kept.windows(2).all(|pair| pair[1].seq == pair[0].seq + 1));
        let tail = oplog.after(197).unwrap().entries;
        assert_eq!(
            tail.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
            [198, 199, 200]
        );
        assert_eq!(tail[0].effect, wrote(197));

        let newest: Vec<_> = oplog.newest_first().unwrap().map(Result::unwrap).collect();
        assert_eq!(newest.len(), kept.len());
        assert_eq!(newest[0].seq, 200);
        assert!(newest.iter().all(|entry| entry.op == 1), "one operation");
        assert_eq!(newest[0].command, "test");
    }

    #[test]
    fn partial_appends_are_cut() {
        let (_dir, store) = store();
        let oplog = store.oplog();
        log(&store, &wrote(1));
        let segment = oplog.segment(1);
        let mut file = OpenOptions::new().append(true).open(&segment).unwrap();
        // A crash in the middle of an append.
        let frame = Entry {
            seq: 2,
            op: 2,
            workspace: None,
            command: String::new(),
            effect: wrote(2),
        }
        .encode();
        file.write_all(&frame[..frame.len() / 2]).unwrap();
        assert_eq!(oplog.last_seq().unwrap(), 1, "readers skip it");
        assert_eq!(log(&store, &wrote(3)), 2, "the next append cuts it off");
        assert_eq!(oplog.after(0).unwrap().entries.len(), 2);
    }

    #[test]
    fn readers_never_find_a_segment_without_its_header() {
        let (_dir, store) = store();
        let header = OpLog::<Native>::FRAMES.header().len() as u64;
        let done = AtomicBool::new(false);
        thread::scope(|scope| {
            // Readers that list the segments, as every read does, and look
            // at the newest, which a rollover may be creating.
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        let oplog = store.oplog();
                        while !done.load(Ordering::Relaxed) {
                            let Some(&newest) = oplog.segments().unwrap().last() else {
                                continue;
                            };
                            if let Ok(found) = fs::metadata(oplog.segment(newest)) {
                                assert!(found.len() >= header, "{} bytes", found.len());
                            }
                            // Tests keep few small segments, so retention may
                            // delete one mid-read: that read is skipped.
                            match oplog.after(oplog.last_seq().unwrap().saturating_sub(1)) {
                                Ok(_) | Err(StoreError::Expired { .. }) => {}
                                Err(error) => panic!("{error}"),
                            }
                        }
                    })
                })
                .collect();
            // Enough entries to roll over to new segments many times.
            for _ in 0..400 {
                log(&store, &wrote(1));
            }
            done.store(true, Ordering::Relaxed);
            for reader in readers {
                reader.join().unwrap();
            }
        });
    }
}
