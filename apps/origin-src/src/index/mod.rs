//! The record index: what reading every record would tell, without reading
//! them. It knows each change's revisions and heads (more than one head means
//! the change diverged), the statements about each record and the statements
//! they claim to supersede, the promotable statements by kind, issuer and
//! declared inputs, and the order this node received its records in. It also
//! caches the input digests of promotable statements, which take reading their
//! subjects' trees to compute.
//!
//! The index is derived from the store and never trusted over it. A missing or
//! corrupt index is rebuilt from the store's records ([`Index::rebuild`]). A
//! rebuild skips the objects that are corrupt or don't decode, and reports
//! them ([`Skipped`]).
//!
//! On disk it is one append-only file of [frames](crate::frames), `index/log`.
//! A frame holds either the records of some op log entries, with the sequence
//! number of the last of them, or input digests.
//!
//! Writers don't update the index. Each [op log](crate::oplog) entry holds
//! what the index keeps of the records it wrote. Opening the index catches up:
//! it reads the op log entries after the last one indexed, adds the records
//! that are published, and appends them to `index/log` without syncing.
//! Records of entries that haven't settled yet, because their write is still
//! in progress, are kept in memory only, and appended once their entry
//! settles. A frame lost to a crash is caught up again from the op log. The
//! index is rebuilt if the op log no longer has the entries it needs, or if
//! `index/log` is damaged other than by a partial last frame.
//!
//! A rebuild holds `index/lock` exclusively and starts from the op log's last
//! settled entry, so it misses no record. It keeps the arrival order the old
//! log knew, and puts other records after them.
//!
//! The directory also holds `index/supersession`, a separate lock:
//! [`Annotations::supersede`](crate::annotations::Annotations::supersede)
//! holds it to read an issuer's current statement and issue the next.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;

use origin_platform::{LockMode, LockedFile, Platform};
use origin_shared::object::{DecodeError, IdMismatch, Reader};
use origin_shared::{
    AcceptanceId, ChangeId, Digest, EncodedObject, ObjectId, ObjectKind, RecordId, Revision,
    RevisionId, Statement, StatementId, TreeId,
};

use crate::frames::{Frames, Tail, Writer};
use crate::oplog::Effect;
use crate::store::{Store, StoreError};

/// The records of a store, by the fields they're looked up by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index {
    /// Every record indexed, in the order this node received them.
    arrivals: Vec<RecordId>,
    /// The records in `arrivals`, to skip duplicates.
    known: HashSet<RecordId>,
    /// The revisions of each change.
    changes: HashMap<ChangeId, Change>,
    /// The statements about each record, in arrival order.
    statements: HashMap<RecordId, Vec<StatementId>>,
    /// The statements claiming to supersede each statement. Only those from
    /// the same issuer do, which reading them tells.
    superseders: HashMap<StatementId, Vec<StatementId>>,
    /// The promotable statements, by their
    /// [promotion group](Statement::promotion_group), in arrival order.
    promotable: HashMap<Digest, Vec<StatementId>>,
    /// The input digests of promotable statements computed so far, see
    /// [`Index::remember_digests`].
    digests: HashMap<StatementId, Digest>,
    /// The sequence number of the last op log entry indexed.
    applied: u64,
    /// The records [`Index::add`] added that `index/log` doesn't have yet.
    /// Catching up appends them later.
    added: HashSet<RecordId>,
}

/// The revisions of one change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Change {
    /// In arrival order.
    revisions: Vec<RevisionId>,
    /// The revisions another revision of the change evolves from: all but
    /// the heads.
    superseded: HashSet<RevisionId>,
}

/// A rebuilt index, and the objects the rebuild skipped.
#[derive(Debug)]
pub struct Rebuilt {
    /// The index.
    pub index: Index,
    /// The objects skipped, also reported on stderr.
    pub skipped: Vec<Skipped>,
}

/// An object a rebuild skips, since readers can't use it either.
#[derive(Debug, thiserror::Error)]
pub enum Skipped {
    /// Its content doesn't hash to its id.
    #[error("the index skips a corrupt object: {0}")]
    Corrupt(IdMismatch),
    /// Its file doesn't decompress.
    #[error("the index skips object {id}, which doesn't decompress: {error}")]
    Undecompressable {
        /// The object.
        id: ObjectId,
        /// Why decompressing failed.
        error: io::Error,
    },
    /// It's a record that doesn't decode.
    #[error("the index skips record {id}, which doesn't decode: {error}")]
    Undecodable {
        /// The record.
        id: ObjectId,
        /// Why it doesn't decode.
        error: DecodeError,
    },
}

impl Index {
    /// Reads the index of `store` and catches up with the op log. Rebuilds the
    /// index if it is missing, corrupt, or too far behind.
    ///
    /// # Errors
    ///
    /// Fails if the index, the op log or the store cannot be read, or a
    /// rebuilt index cannot be written.
    pub fn open<P: Platform>(store: &Store<P>) -> Result<Self, StoreError> {
        let (mut index, intact) = Self::read(store)?;
        if intact && index.catch_up(store)? {
            return Ok(index);
        }
        let _lock = store.lock_index(LockMode::Exclusive)?;
        // Another process may have rebuilt it while this one waited.
        let (mut index, intact) = Self::read(store)?;
        if intact && index.catch_up(store)? {
            return Ok(index);
        }
        Ok(Self::rebuild_locked(store, &index)?.index)
    }

    /// Catches up with the op log of `store`, as [`Index::open`] does, for a
    /// process that keeps the index open. It reads only the op log entries
    /// since the last catch-up. Reopens the index if the op log no longer has
    /// those entries.
    ///
    /// # Errors
    ///
    /// Fails like [`Index::open`].
    pub fn refresh<P: Platform>(&mut self, store: &Store<P>) -> Result<(), StoreError> {
        if !self.catch_up(store)? {
            *self = Self::open(store)?;
        }
        Ok(())
    }

    /// Reconstructs the index of `store` from its records, and replaces the
    /// one on disk. Records the old index knew keep their arrival order.
    /// Objects that are corrupt or don't decode are skipped, and reported on
    /// stderr.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read or the index cannot be written.
    pub fn rebuild<P: Platform>(store: &Store<P>) -> Result<Rebuilt, StoreError> {
        let _lock = store.lock_index(LockMode::Exclusive)?;
        let (old, _) = Self::read(store)?;
        Self::rebuild_locked(store, &old)
    }

    /// Rebuilds the index while holding its lock exclusively, keeping the
    /// arrival order of `old`.
    fn rebuild_locked<P: Platform>(store: &Store<P>, old: &Self) -> Result<Rebuilt, StoreError> {
        // Every entry up to it is as published as it will be, so the store
        // has their records; later ones are caught up.
        let applied = store.oplog().settled()?;
        let order: HashMap<RecordId, usize> = old
            .arrivals
            .iter()
            .enumerate()
            .map(|(position, &record)| (record, position))
            .collect();
        let (mut entries, skipped) = store.entries()?;
        for skipped in &skipped {
            eprintln!("warning: {skipped}");
        }
        entries.sort_unstable_by_key(|entry| {
            let record = entry.record();
            (order.get(&record).copied().unwrap_or(usize::MAX), record)
        });
        let mut index = Self {
            applied,
            ..Self::default()
        };
        // Records of entries after it are kept in memory, not written:
        // catching up writes them once their entries settle.
        let mut later = HashSet::new();
        for logged in store.oplog().after(applied)?.entries {
            if let Effect::Wrote(written) = logged.effect {
                later.extend(written.iter().map(Entry::record));
            }
        }
        for entry in &entries {
            index.insert(entry.clone());
        }
        let (entries, unsettled): (Vec<_>, Vec<_>) = entries
            .into_iter()
            .partition(|entry| !later.contains(&entry.record()));
        index.added.extend(unsettled.iter().map(Entry::record));
        // Digests never change, so those of statements still there stay.
        let mut digests: Vec<_> = old
            .digests
            .iter()
            .filter(|(id, _)| index.known.contains(&RecordId::Statement(**id)))
            .map(|(&id, &digest)| (id, digest))
            .collect();
        digests.sort_unstable();
        store.write_index(&entries, &digests, applied)?;
        index.digests.extend(digests);
        Ok(Rebuilt { index, skipped })
    }

    /// Reads as much of the index as is intact, returning whether all of it
    /// was. A missing index is empty and not intact.
    fn read<P: Platform>(store: &Store<P>) -> Result<(Self, bool), StoreError> {
        let path = store.index_log();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((Self::default(), false));
            }
            Err(source) => return Err(StoreError::io(&path, source)),
        };
        let (frames, tail) = Log::parse(&bytes);
        let mut index = Self::default();
        for (_, frame) in frames {
            match frame {
                Frame::Records { applied, entries } => {
                    for entry in entries.into_owned() {
                        index.insert(entry);
                    }
                    // Frames from processes catching up at once can come in
                    // any order.
                    index.applied = index.applied.max(applied);
                }
                Frame::Digests(digests) => index.digests.extend(digests.iter().copied()),
            }
        }
        Ok((index, tail != Tail::Damaged))
    }

    /// Adds the records among `objects`, which this process just wrote, so this
    /// index knows them without being read again.
    ///
    /// # Errors
    ///
    /// Fails if a record doesn't decode.
    pub(crate) fn add<'o>(
        &mut self,
        objects: impl IntoIterator<Item = &'o EncodedObject>,
    ) -> Result<(), StoreError> {
        for object in objects {
            let entry = Entry::of(object).map_err(|source| StoreError::Decode {
                id: object.id(),
                source,
            })?;
            if let Some(entry) = entry
                && !self.known.contains(&entry.record())
            {
                self.added.insert(entry.record());
                self.insert(entry);
            }
        }
        Ok(())
    }

    /// Adds the published records of the op log entries after the last one
    /// indexed, and appends them to `index/log`, as the module docs describe.
    /// Returns `false` if the op log no longer has those entries, or damage
    /// lost some of them: then only a rebuild can catch up.
    fn catch_up<P: Platform>(&mut self, store: &Store<P>) -> Result<bool, StoreError> {
        let oplog = store.oplog();
        let read = match oplog.after(self.applied) {
            Err(StoreError::Expired { .. }) => return Ok(false),
            read => read?,
        };
        if read.damage.is_some() {
            return Ok(false);
        }
        let Some(last) = read.entries.last() else {
            return Ok(true);
        };
        // The index stays before the first entry whose write may still be
        // in progress, to look again next time, but takes the records
        // published after it, such as this process's own: in memory only,
        // since a power loss may take them back before their entry settles.
        let applied = last.seq.min(oplog.settled()?).max(self.applied);
        let mut caught = Vec::new();
        for logged in &read.entries {
            if let Effect::Wrote(written) = &logged.effect {
                for entry in written {
                    let record = entry.record();
                    let unlogged = !self.known.contains(&record) || self.added.contains(&record);
                    if !unlogged || !store.contains(entry.id())? {
                        continue;
                    }
                    if logged.seq <= applied {
                        self.added.remove(&record);
                        caught.push(entry.clone());
                    } else {
                        self.added.insert(record);
                        self.insert(entry.clone());
                    }
                }
            }
        }
        if applied == self.applied && caught.is_empty() {
            return Ok(true);
        }
        for entry in &caught {
            self.insert(entry.clone());
        }
        self.applied = applied;
        // Not synced: a lost frame is caught up again from the op log.
        store.append_to_index(&Frame::Records {
            applied,
            entries: Cow::Owned(caught),
        })?;
        Ok(true)
    }

    /// Adds a record, unless it's already indexed.
    fn insert(&mut self, entry: Entry) {
        if !self.known.insert(entry.record()) {
            return;
        }
        self.arrivals.push(entry.record());
        match entry {
            Entry::Revision {
                id,
                change,
                predecessors,
            } => {
                let change = self.changes.entry(change).or_default();
                change.revisions.push(id);
                change.superseded.extend(predecessors);
            }
            Entry::Statement {
                id,
                subject,
                supersedes,
                group,
            } => {
                self.statements.entry(subject).or_default().push(id);
                if let Some(superseded) = supersedes {
                    self.superseders.entry(superseded).or_default().push(id);
                }
                if let Some(group) = group {
                    self.promotable.entry(group).or_default().push(id);
                }
            }
            Entry::Acceptance(_) => {}
        }
    }

    /// Every record indexed, in the order this node received them. A
    /// position in it is a cursor: records received later come after it.
    #[must_use]
    pub fn arrivals(&self) -> &[RecordId] {
        &self.arrivals
    }

    /// Every change with a revision, in no particular order.
    pub fn changes(&self) -> impl Iterator<Item = ChangeId> {
        self.changes.keys().copied()
    }

    /// The revisions of `change`, in arrival order.
    #[must_use]
    pub fn revisions(&self, change: ChangeId) -> &[RevisionId] {
        self.changes
            .get(&change)
            .map_or(&[], |change| &change.revisions)
    }

    /// The heads of `change`, in arrival order: its revisions that no other
    /// revision of it evolves from.
    #[must_use]
    pub fn heads(&self, change: ChangeId) -> Vec<RevisionId> {
        self.changes.get(&change).map_or_else(Vec::new, |change| {
            change
                .revisions
                .iter()
                .filter(|revision| !change.superseded.contains(revision))
                .copied()
                .collect()
        })
    }

    /// Whether `change` has more than one head.
    #[must_use]
    pub fn is_divergent(&self, change: ChangeId) -> bool {
        self.heads(change).len() > 1
    }

    /// The statements about `subject`, in arrival order.
    #[must_use]
    pub fn statements(&self, subject: RecordId) -> &[StatementId] {
        self.statements.get(&subject).map_or(&[], Vec::as_slice)
    }

    /// The statements claiming to supersede `statement`, in arrival order.
    /// Only those from its issuer do.
    #[must_use]
    pub fn superseders(&self, statement: StatementId) -> &[StatementId] {
        self.superseders.get(&statement).map_or(&[], Vec::as_slice)
    }

    /// The promotable statements, by their
    /// [promotion group](Statement::promotion_group), each group in arrival
    /// order.
    pub fn promotable(&self) -> impl Iterator<Item = (Digest, &[StatementId])> {
        self.promotable
            .iter()
            .map(|(&group, statements)| (group, statements.as_slice()))
    }

    /// The input digest of a promotable statement, if it was computed and
    /// remembered.
    #[must_use]
    pub fn digest(&self, statement: StatementId) -> Option<Digest> {
        self.digests.get(&statement).copied()
    }

    /// Remembers the input digests of promotable statements, so they're
    /// computed once. Since the index is a cache, they aren't synced: a
    /// crash only costs computing them again.
    ///
    /// # Errors
    ///
    /// Fails if the index cannot be appended to.
    pub fn remember_digests<P: Platform>(
        &mut self,
        store: &Store<P>,
        digests: &[(StatementId, Digest)],
    ) -> Result<(), StoreError> {
        if digests.is_empty() {
            return Ok(());
        }
        let _lock = store.lock_index(LockMode::Shared)?;
        store.append_to_index(&Frame::Digests(Cow::Borrowed(digests)))?;
        self.digests.extend(digests.iter().copied());
        Ok(())
    }

    /// The one change whose id starts with `prefix`, a few hex digits.
    ///
    /// # Errors
    ///
    /// Fails if no change, or more than one, has an id starting with
    /// `prefix`.
    pub fn find_change(&self, prefix: &str) -> Result<ChangeId, FindChangeError> {
        ChangeId::find(prefix, self.changes.keys().copied())
    }

    /// The one revision whose id starts with `prefix`, a few hex digits.
    ///
    /// # Errors
    ///
    /// Fails if no revision, or more than one, has an id starting with
    /// `prefix`.
    pub fn find_revision(&self, prefix: &str) -> Result<RevisionId, FindError<RevisionId>> {
        let revisions = self.changes.values().flat_map(|change| &change.revisions);
        RevisionId::find(prefix, revisions.copied())
    }

    /// The one statement whose id starts with `prefix`, a few hex digits.
    ///
    /// # Errors
    ///
    /// Fails if no statement, or more than one, has an id starting with
    /// `prefix`.
    pub fn find_statement(&self, prefix: &str) -> Result<StatementId, FindError<StatementId>> {
        StatementId::find(prefix, self.statements.values().flatten().copied())
    }
}

/// Ids that can be named by a unique prefix of their hex digits.
pub trait Prefixed: Copy + Ord + fmt::Display {
    /// What the ids identify, for errors.
    const WHAT: &str;

    /// The one id of `ids` starting with `prefix`.
    ///
    /// # Errors
    ///
    /// Fails if `prefix` isn't hex digits, or no id or more than one starts
    /// with it.
    fn find(prefix: &str, ids: impl Iterator<Item = Self>) -> Result<Self, FindError<Self>> {
        let prefix = prefix.to_ascii_lowercase();
        if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(FindError::Invalid(prefix));
        }
        let mut matches: Vec<Self> = ids
            .filter(|id| id.to_string().starts_with(&prefix))
            .collect();
        match matches.len() {
            0 => Err(FindError::NotFound(prefix)),
            1 => Ok(matches[0]),
            _ => {
                matches.sort_unstable();
                Err(FindError::Ambiguous { prefix, matches })
            }
        }
    }
}

impl Prefixed for ChangeId {
    const WHAT: &str = "change";
}

impl Prefixed for RevisionId {
    const WHAT: &str = "revision";
}

impl Prefixed for StatementId {
    const WHAT: &str = "statement";
}

/// An id prefix that doesn't name exactly one change.
pub type FindChangeError = FindError<ChangeId>;

/// An id prefix that doesn't name exactly one id.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FindError<I: Prefixed> {
    /// It isn't hex digits.
    #[error("`{0}` is not a {what} id", what = I::WHAT)]
    Invalid(String),
    /// No id starts with it.
    #[error("no {what} has an id starting with `{0}`", what = I::WHAT)]
    NotFound(String),
    /// Several do.
    #[error(
        "`{prefix}` could be any of the {what}s {}",
        matches.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
        what = I::WHAT
    )]
    Ambiguous {
        /// The prefix.
        prefix: String,
        /// The ids starting with it.
        matches: Vec<I>,
    },
}

/// What the index keeps of one record: its id, and the fields it is looked
/// up by. Op log entries hold lists of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A revision.
    Revision {
        /// The revision's id.
        id: RevisionId,
        /// The revision's change.
        change: ChangeId,
        /// The revisions it evolves from.
        predecessors: Vec<RevisionId>,
    },
    /// A statement.
    Statement {
        /// The revision's id.
        id: StatementId,
        /// The record it is about.
        subject: RecordId,
        /// The statement it claims to supersede.
        supersedes: Option<StatementId>,
        /// Its promotion group, if it's promotable.
        group: Option<Digest>,
    },
    /// An acceptance.
    Acceptance(AcceptanceId),
}

impl Entry {
    /// The entry of `object`, or `None` if it isn't indexed: blobs and
    /// trees.
    ///
    /// # Errors
    ///
    /// Fails if the object is a record that doesn't decode.
    pub(crate) fn of(object: &EncodedObject) -> Result<Option<Self>, DecodeError> {
        let id = object.id();
        Ok(match object.kind()? {
            ObjectKind::Blob | ObjectKind::Tree => None,
            ObjectKind::Revision => {
                let revision: Revision = object.decode_as()?;
                Some(Self::Revision {
                    id: id.into(),
                    change: revision.change,
                    predecessors: revision.predecessors,
                })
            }
            ObjectKind::Statement => {
                let statement: Statement = object.decode_as()?;
                Some(Self::Statement {
                    id: id.into(),
                    subject: statement.subject,
                    supersedes: statement.supersedes,
                    group: statement.promotion_group(),
                })
            }
            ObjectKind::Acceptance => {
                // Decoding checks it's well formed, as for other records.
                object.decode_as::<origin_shared::Acceptance>()?;
                Some(Self::Acceptance(id.into()))
            }
        })
    }

    /// The record.
    #[must_use]
    pub fn record(&self) -> RecordId {
        match *self {
            Self::Revision { id, .. } => RecordId::Revision(id),
            Self::Statement { id, .. } => RecordId::Statement(id),
            Self::Acceptance(id) => RecordId::Acceptance(id),
        }
    }

    fn id(&self) -> ObjectId {
        ObjectId::from(self.record())
    }

    /// Appends the entry: its record's kind and id, then for a revision its
    /// change and a `u32` count of predecessors and their ids, and for a
    /// statement its subject's kind and id, and the statement it supersedes
    /// and its promotion group, each a 0 byte, or a 1 byte and the id.
    pub(crate) fn write(&self, out: &mut Writer) {
        Log::write_record(self.record(), out);
        match self {
            Self::Revision {
                change,
                predecessors,
                ..
            } => {
                out.bytes(change.as_bytes());
                out.len(predecessors.len());
                for predecessor in predecessors {
                    out.bytes(predecessor.as_bytes());
                }
            }
            Self::Statement {
                subject,
                supersedes,
                group,
                ..
            } => {
                Log::write_record(*subject, out);
                out.option(supersedes.as_ref().map(StatementId::as_bytes));
                out.option(group.as_ref().map(Digest::as_bytes));
            }
            Self::Acceptance(_) => {}
        }
    }

    pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(match Log::read_record(reader)? {
            RecordId::Revision(id) => {
                let change = ChangeId::from_bytes(reader.array()?);
                let count = reader.read_len()?;
                let predecessors = (0..count)
                    .map(|_| reader.array().map(|id| ObjectId::from_bytes(id).into()))
                    .collect::<Result<_, _>>()?;
                Self::Revision {
                    id,
                    change,
                    predecessors,
                }
            }
            RecordId::Statement(id) => Self::Statement {
                id,
                subject: Log::read_record(reader)?,
                supersedes: reader
                    .option(Reader::array)?
                    .map(|id| ObjectId::from_bytes(id).into()),
                group: reader.option(Reader::array)?.map(Digest::from_bytes),
            },
            RecordId::Acceptance(id) => Self::Acceptance(id),
            RecordId::Snapshot(_) => {
                return Err(DecodeError::UnknownTag {
                    field: "index entry",
                    tag: ObjectKind::Tree as u8,
                });
            }
        })
    }
}

/// A frame of the log.
#[derive(Debug)]
enum Frame<'a> {
    /// The published records of op log entries, up to the entry `applied`.
    Records {
        applied: u64,
        entries: Cow<'a, [Entry]>,
    },
    /// Input digests of promotable statements, computed from the store.
    Digests(Cow<'a, [(StatementId, Digest)]>),
}

impl Frame<'_> {
    const RECORDS: u8 = 1;
    const DIGESTS: u8 = 2;

    /// Encodes the frame: a tag, then its contents.
    fn encode(&self) -> Vec<u8> {
        Frames::frame(|out| match self {
            Self::Records { applied, entries } => {
                out.u8(Self::RECORDS);
                out.u64(*applied);
                for entry in entries.iter() {
                    entry.write(out);
                }
            }
            Self::Digests(digests) => {
                out.u8(Self::DIGESTS);
                for (statement, digest) in digests.iter() {
                    out.bytes(statement.as_bytes());
                    out.bytes(digest.as_bytes());
                }
            }
        })
    }

    fn decode(payload: &[u8]) -> Result<Frame<'static>, DecodeError> {
        let mut reader = Reader::new(payload);
        let frame = match reader.u8()? {
            Self::RECORDS => {
                let applied = reader.u64()?;
                let mut entries = Vec::new();
                while reader.remaining() > 0 {
                    entries.push(Entry::read(&mut reader)?);
                }
                Frame::Records {
                    applied,
                    entries: Cow::Owned(entries),
                }
            }
            Self::DIGESTS => {
                let mut digests = Vec::new();
                while reader.remaining() > 0 {
                    let statement = ObjectId::from_bytes(reader.array()?).into();
                    digests.push((statement, Digest::from_bytes(reader.array()?)));
                }
                Frame::Digests(Cow::Owned(digests))
            }
            tag => {
                return Err(DecodeError::UnknownTag {
                    field: "index frame",
                    tag,
                });
            }
        };
        reader.finish()?;
        Ok(frame)
    }
}

/// The format of `index/log`.
struct Log;

impl Log {
    /// The format of `index/log`, whose header names its version.
    const FRAMES: Frames = Frames::new(b"origin index 3\n");

    /// Encodes a log holding `entries`, up to the op log entry `applied`, and
    /// `digests`.
    fn encode(entries: &[Entry], digests: &[(StatementId, Digest)], applied: u64) -> Vec<u8> {
        let mut log = Self::FRAMES.header().to_vec();
        log.extend(
            Frame::Records {
                applied,
                entries: Cow::Borrowed(entries),
            }
            .encode(),
        );
        if !digests.is_empty() {
            log.extend(Frame::Digests(Cow::Borrowed(digests)).encode());
        }
        log
    }

    /// The frames of a log and their offsets, up to the first broken one,
    /// and how it ends after them.
    fn parse(bytes: &[u8]) -> (Vec<(u64, Frame<'static>)>, Tail) {
        Self::FRAMES.parse(bytes, Frame::decode)
    }

    /// Appends a record's kind and id.
    fn write_record(record: RecordId, out: &mut Writer) {
        out.u8(record.kind() as u8);
        out.bytes(ObjectId::from(record).as_bytes());
    }

    /// Reads what [`Log::write_record`] wrote.
    fn read_record(reader: &mut Reader<'_>) -> Result<RecordId, DecodeError> {
        let kind = reader.u8()?;
        let id = ObjectId::from_bytes(reader.array()?);
        Ok(match kind {
            k if k == ObjectKind::Tree as u8 => RecordId::Snapshot(TreeId::from(id)),
            k if k == ObjectKind::Revision as u8 => RecordId::Revision(id.into()),
            k if k == ObjectKind::Statement as u8 => RecordId::Statement(id.into()),
            k if k == ObjectKind::Acceptance as u8 => RecordId::Acceptance(id.into()),
            tag => {
                return Err(DecodeError::UnknownTag {
                    field: "record kind",
                    tag,
                });
            }
        })
    }
}

impl<P: Platform> Store<P> {
    fn index_log(&self) -> PathBuf {
        self.index_dir().join("log")
    }

    /// Locks the index: shared while appending digests, exclusive for a
    /// rebuild.
    fn lock_index(&self, mode: LockMode) -> Result<LockedFile, StoreError> {
        self.lock_in_index("lock", mode)
    }

    /// Locks supersession, exclusively: see [`Annotations::supersede`]. It
    /// uses a file of its own, not the index lock, since reading the index
    /// while holding it may rebuild the index, which takes the index lock.
    ///
    /// [`Annotations::supersede`]: crate::annotations::Annotations::supersede
    pub(crate) fn lock_supersession(&self) -> Result<LockedFile, StoreError> {
        self.lock_in_index("supersession", LockMode::Exclusive)
    }

    /// Locks the file `name` in the index's directory in `mode`.
    fn lock_in_index(&self, name: &str, mode: LockMode) -> Result<LockedFile, StoreError> {
        let dir = self.index_dir();
        fs::create_dir_all(&dir).map_err(|source| StoreError::io(&dir, source))?;
        Ok(LockedFile::open(&self.platform, &dir.join(name), mode)?)
    }

    /// Durably replaces the index with one holding `entries` and `digests`, up
    /// to the op log entry `applied`.
    pub(crate) fn write_index(
        &self,
        entries: &[Entry],
        digests: &[(StatementId, Digest)],
        applied: u64,
    ) -> Result<(), StoreError> {
        let write = self.durable_write();
        write.stage(self.index_log(), &Log::encode(entries, digests, applied))?;
        Ok(write.commit()?)
    }

    /// Appends `frame` to `index/log` without syncing it. Does nothing if there
    /// is no log, since opening the index will rebuild it from the store.
    fn append_to_index(&self, frame: &Frame<'_>) -> Result<(), StoreError> {
        let path = self.index_log();
        let io_error = |source| StoreError::io(&path, source);
        let log = match OpenOptions::new().read(true).append(true).open(&path) {
            Ok(log) => log,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(io_error(source)),
        };
        // Closing the log releases the lock.
        self.platform
            .lock(&log, LockMode::Exclusive)
            .map_err(io_error)?;
        Log::FRAMES
            .append(&self.platform, &log, &frame.encode())
            .map_err(io_error)?;
        Ok(())
    }

    /// The entry of every record in the store, read from every core, and
    /// the objects skipped because they are corrupt or don't decode.
    fn entries(&self) -> Result<(Vec<Entry>, Vec<Skipped>), StoreError> {
        let ids = self.ids()?;
        let chunks = Self::on_every_core(&ids, |ids| {
            let (mut entries, mut skipped) = (Vec::new(), Vec::new());
            for &id in ids {
                match self.try_read(id) {
                    Ok(Some(object)) => match Entry::of(&object) {
                        Ok(entry) => entries.extend(entry),
                        Err(error) => skipped.push(Skipped::Undecodable { id, error }),
                    },
                    Ok(None) => {}
                    Err(StoreError::Corrupt(mismatch)) => skipped.push(Skipped::Corrupt(mismatch)),
                    Err(StoreError::Decompress { id, source, .. }) => {
                        skipped.push(Skipped::Undecompressable { id, error: source });
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok((entries, skipped))
        });
        let (entries, skipped): (Vec<Vec<_>>, Vec<Vec<_>>) =
            chunks.into_iter().collect::<Result<_, _>>()?;
        Ok((entries.concat(), skipped.into_iter().flatten().collect()))
    }
}

#[cfg(test)]
mod tests;
