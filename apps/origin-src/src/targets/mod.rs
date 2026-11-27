//! Target chains: the acceptances of each target, in order, in `targets/<id>/`,
//! where the [`TargetId`] is the hash of its name, so names never become file
//! names. Readers go by each target's head, so they only see durable
//! acceptances; the "Target chains" section of `docs/design.md` describes the
//! files and how an append repairs what a crash left.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::slice;

use origin_platform::{DurableWrite, LockMode, LockedFile, Platform};
use origin_shared::{
    Acceptance, AcceptanceId, Base, EncodedObject, Head, Record, Revision, RevisionId, Source,
    TargetId, TargetName,
};

use crate::oplog::Effect;
use crate::store::{Store, StoreError};

/// Describes a target's head for error messages, or `nothing` if it has
/// none.
pub(crate) fn describe(head: Option<Head>) -> String {
    head.map_or_else(|| "nothing".to_owned(), |head| head.to_string())
}

/// The files of one target.
struct TargetFiles {
    dir: PathBuf,
}

impl TargetFiles {
    /// The length of a log line: an id in hex and a newline.
    const LINE: usize = 2 * AcceptanceId::LEN + 1;

    fn name(&self) -> PathBuf {
        self.dir.join("name")
    }

    fn log(&self) -> PathBuf {
        self.dir.join("log")
    }

    fn head(&self) -> PathBuf {
        self.dir.join("head")
    }

    /// Reads a file, or `None` if it doesn't exist.
    fn read(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StoreError::io(path, source)),
        }
    }

    /// Reads the published head, or `None` if the target has none.
    fn read_head(&self) -> Result<Option<Head>, StoreError> {
        let path = self.head();
        let Some(bytes) = Self::read(&path)? else {
            return Ok(None);
        };
        str::from_utf8(&bytes)
            .ok()
            .and_then(Self::parse_head)
            .map(Some)
            .ok_or(StoreError::Malformed(path))
    }

    /// Parses `head`'s `<seq> <id>` line.
    fn parse_head(line: &str) -> Option<Head> {
        let (seq, acceptance) = line.strip_suffix('\n')?.split_once(' ')?;
        Some(Head {
            seq: seq.parse().ok()?,
            acceptance: acceptance.parse().ok()?,
        })
    }

    /// Parses the complete lines of a log into acceptance ids, ignoring a
    /// partial last line.
    fn parse_log(&self, bytes: &[u8]) -> Result<Vec<AcceptanceId>, StoreError> {
        bytes
            .as_chunks::<{ Self::LINE }>()
            .0
            .iter()
            .map(|line| {
                str::from_utf8(line)
                    .ok()
                    .and_then(|line| line.strip_suffix('\n')?.parse().ok())
                    .ok_or_else(|| StoreError::Malformed(self.log()))
            })
            .collect()
    }

    /// Reads the lines `range` of the log. The log must hold them all, or it
    /// counts as malformed.
    fn read_lines(&self, range: Range<u64>) -> Result<Vec<AcceptanceId>, StoreError> {
        let path = self.log();
        let line = Self::LINE as u64;
        let len = usize::try_from((range.end - range.start) * line)
            .map_err(|_| StoreError::Malformed(path.clone()))?;
        let mut bytes = vec![0; len];
        File::open(&path)
            .and_then(|log| log.read_exact_at(&mut bytes, range.start * line))
            .map_err(|source| match source.kind() {
                io::ErrorKind::UnexpectedEof => StoreError::Malformed(path.clone()),
                _ => StoreError::io(&path, source),
            })?;
        self.parse_log(&bytes)
    }

    /// Reads the complete entries of the locked `log` from position `since` on.
    fn entries_since(&self, log: &File, since: u64) -> Result<Vec<Head>, StoreError> {
        let mut bytes = Vec::new();
        let mut reader = log;
        reader
            .seek(SeekFrom::Start(since * Self::LINE as u64))
            .and_then(|_| reader.read_to_end(&mut bytes))
            .map_err(|source| StoreError::io(&self.log(), source))?;
        Ok((since..)
            .zip(self.parse_log(&bytes)?)
            .map(|(seq, acceptance)| Head { seq, acceptance })
            .collect())
    }

    /// Returns the last complete entry of a locked log, first cutting off a
    /// partial last line left by a crash.
    fn last_entry(&self, log: &File) -> Result<Option<Head>, StoreError> {
        let io_error = |source| StoreError::io(&self.log(), source);
        let len = log.metadata().map_err(io_error)?.len();
        let line = Self::LINE as u64;
        let entries = len / line;
        if len % line != 0 {
            log.set_len(entries * line).map_err(io_error)?;
        }
        let Some(seq) = entries.checked_sub(1) else {
            return Ok(None);
        };
        let mut bytes = [0; Self::LINE];
        let mut reader = log;
        reader
            .seek(SeekFrom::Start(seq * line))
            .and_then(|_| reader.read_exact(&mut bytes))
            .map_err(io_error)?;
        let acceptance = self.parse_log(&bytes)?[0];
        Ok(Some(Head { seq, acceptance }))
    }
}

impl<P: Platform> Store<P> {
    fn target_files(&self, id: TargetId) -> TargetFiles {
        TargetFiles {
            dir: self.targets_dir().join(id.to_hex()),
        }
    }

    /// Returns the latest acceptance of `target`, or `None` if it doesn't
    /// exist.
    ///
    /// # Errors
    ///
    /// Fails if the head cannot be read or is malformed.
    pub fn head(&self, target: &TargetName) -> Result<Option<Head>, StoreError> {
        self.target_files(target.id()).read_head()
    }

    /// Returns the published acceptances of `target`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails if the target's files cannot be read or are malformed.
    pub fn log(&self, target: &TargetName) -> Result<Vec<AcceptanceId>, StoreError> {
        self.log_since(target, 0, None)
    }

    /// Returns the acceptances of `target` from position `since` on, oldest
    /// first, at most `limit` of them. Reads only those lines of the log, and
    /// the head's line, which must match the head.
    ///
    /// # Errors
    ///
    /// Fails if the target's files cannot be read or are malformed.
    pub fn log_since(
        &self,
        target: &TargetName,
        since: u64,
        limit: Option<u64>,
    ) -> Result<Vec<AcceptanceId>, StoreError> {
        let files = self.target_files(target.id());
        let Some(head) = files.read_head()? else {
            return Ok(Vec::new());
        };
        if since > head.seq {
            return Ok(Vec::new());
        }
        // The log can be ahead of the head while an append is in progress:
        // read only up to the head.
        let end = limit.map_or(head.seq + 1, |limit| {
            since.saturating_add(limit).min(head.seq + 1)
        });
        let log = files.read_lines(since..end)?;
        let at_head = if end > head.seq {
            log.last().copied()
        } else {
            files.read_lines(head.seq..head.seq + 1)?.first().copied()
        };
        if at_head != Some(head.acceptance) {
            return Err(StoreError::Malformed(files.log()));
        }
        Ok(log)
    }

    /// Returns the acceptance at position `seq` of `target`'s log, or `None` if
    /// none is published there yet. Reads one line of the log.
    ///
    /// # Errors
    ///
    /// Fails if the target's files cannot be read or are malformed.
    pub fn acceptance_at(
        &self,
        target: TargetId,
        seq: u64,
    ) -> Result<Option<AcceptanceId>, StoreError> {
        let files = self.target_files(target);
        let Some(head) = files.read_head()? else {
            return Ok(None);
        };
        if seq >= head.seq {
            return Ok((seq == head.seq).then_some(head.acceptance));
        }
        Ok(files.read_lines(seq..seq + 1)?.first().copied())
    }

    /// Returns the acceptance that landed `revision`, whose id is `id`, if one
    /// did. A revision can only land right after the acceptance it is based
    /// on, so only the next acceptance is checked.
    ///
    /// # Errors
    ///
    /// Fails if the acceptances or the target's files cannot be read.
    pub fn landing(
        &self,
        id: RevisionId,
        revision: &Revision,
    ) -> Result<Option<Acceptance>, StoreError> {
        let Base::Acceptance(base) = revision.base else {
            return Ok(None);
        };
        let base = self.get(base)?;
        let Some(next) = self.acceptance_at(base.body().target, base.body().seq + 1)? else {
            return Ok(None);
        };
        let next = self.get(next)?;
        Ok(next
            .body()
            .landed
            .contains(&Source::Revision(id))
            .then_some(next))
    }

    /// Returns the target the stack of `revision` is on: the target of the
    /// acceptance at the bottom of the stack, found by following bases.
    ///
    /// # Errors
    ///
    /// Fails if a record of the stack cannot be read, or the store doesn't
    /// know the target's name.
    pub fn stack_target(&self, revision: RevisionId) -> Result<TargetName, StoreError> {
        let mut base = self.get(revision)?.base;
        let acceptance = loop {
            match base {
                Base::Revision(id) => base = self.get(id)?.base,
                Base::Acceptance(id) => break id,
            }
        };
        let target = self.get(acceptance)?.body().target;
        self.target_name(target)?
            .ok_or(StoreError::NotFound(acceptance.into()))
    }

    /// The name of the target `id`, or `None` if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Fails if its name cannot be read or is malformed.
    pub fn target_name(&self, id: TargetId) -> Result<Option<TargetName>, StoreError> {
        let path = self.target_files(id).name();
        TargetFiles::read(&path)?
            .map(|bytes| {
                String::from_utf8(bytes)
                    .ok()
                    .and_then(|name| TargetName::new(name).ok())
                    .ok_or(StoreError::Malformed(path))
            })
            .transpose()
    }

    /// Returns the names of every target, in no particular order.
    ///
    /// # Errors
    ///
    /// Fails if the targets directory or a name cannot be read.
    pub fn targets(&self) -> Result<Vec<TargetName>, StoreError> {
        let dir = self.targets_dir();
        let entries = fs::read_dir(&dir).map_err(|source| StoreError::io(&dir, source))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::io(&dir, source))?;
            if DurableWrite::<P>::is_temporary(&entry.file_name()) {
                continue;
            }
            let files = TargetFiles { dir: entry.path() };
            // A target is created by publishing its first head.
            if files.read_head()?.is_none() {
                continue;
            }
            let path = files.name();
            let name = TargetFiles::read(&path)?
                .and_then(|bytes| TargetName::new(String::from_utf8(bytes).ok()?).ok())
                .ok_or(StoreError::Malformed(path))?;
            names.push(name);
        }
        Ok(names)
    }

    /// Appends `acceptance` to the chain of `target`, creating the target with
    /// its first acceptance. See [`Store::append_all`].
    ///
    /// # Errors
    ///
    /// Fails like [`Store::append_all`].
    pub fn append(
        &self,
        target: &TargetName,
        acceptance: &Acceptance,
    ) -> Result<AcceptanceId, StoreError> {
        self.append_all(target, slice::from_ref(acceptance), &[])?;
        Ok(acceptance.encode().id().into())
    }

    /// Appends `acceptances`, in order, to the chain of `target`, creating the
    /// target with its first acceptance. The records in `with` are written in
    /// the same batch, at no extra sync.
    ///
    /// A compare-and-swap: the first acceptance must follow the target's
    /// latest, with the next sequence number and naming it as `prev`, and each
    /// later one the one before it. Appends are serialized, so all but one of
    /// those following the same acceptance fail. The caller verifies signatures.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] if an acceptance doesn't follow the one before
    /// it, [`StoreError::WrongTarget`] if one belongs to another target, or I/O.
    pub fn append_all(
        &self,
        target: &TargetName,
        acceptances: &[Acceptance],
        with: &[EncodedObject],
    ) -> Result<(), StoreError> {
        let Some(first) = acceptances.first() else {
            return Ok(());
        };
        // A new policy changes which payloads collection keeps, so it never
        // lands while collection, which reads the policy first, runs.
        let _held = (*target == TargetName::POLICY)
            .then(|| self.hold_objects())
            .transpose()?;
        let mut records = with.to_vec();
        let mut ids = Vec::with_capacity(acceptances.len());
        for acceptance in acceptances {
            if acceptance.body().target != target.id() {
                return Err(StoreError::WrongTarget(target.clone()));
            }
            let encoded = acceptance.encode();
            ids.push(AcceptanceId::from(encoded.id()));
            records.push(encoded);
        }
        // INVARIANT: the acceptances are durable before the log refers to them.
        self.write_all(&records)?;

        let files = self.target_files(target.id());
        // INVARIANT: named before its first head, so a target that exists is never nameless.
        self.name_target(target)?;
        let log = LockedFile::open(&self.platform, &files.log(), LockMode::Exclusive)?;
        let latest = files.last_entry(&log)?;
        let head = files.read_head()?;
        let log_error = |source| StoreError::io(&files.log(), source);
        if head != latest {
            match latest {
                // A crash left the log ahead of the head. Its entries name
                // durable acceptances, but may not be durable themselves.
                Some(latest) if head.is_none_or(|head| head.seq < latest.seq) => {
                    self.platform.sync_file(&log).map_err(log_error)?;
                    let next = head.map_or(0, |head| head.seq + 1);
                    let advanced = files.entries_since(&log, next)?;
                    self.publish(&files, target, &advanced)?;
                }
                _ => return Err(StoreError::Malformed(files.log())),
            }
        }
        let mut previous = latest;
        for (acceptance, &id) in acceptances.iter().zip(&ids) {
            let body = acceptance.body();
            let follows = match previous {
                None => body.seq == 0 && body.prev.is_none(),
                Some(previous) => {
                    body.seq == previous.seq + 1 && body.prev == Some(previous.acceptance)
                }
            };
            if !follows {
                return Err(StoreError::Conflict {
                    target: target.clone(),
                    expected: body.seq,
                    actual: latest,
                });
            }
            previous = Some(Head {
                seq: body.seq,
                acceptance: id,
            });
        }

        let mut lines = String::with_capacity(ids.len() * TargetFiles::LINE);
        for id in &ids {
            lines.push_str(&id.to_hex());
            lines.push('\n');
        }
        let mut writer = &*log;
        writer
            .seek(SeekFrom::Start(first.body().seq * TargetFiles::LINE as u64))
            .and_then(|_| writer.write_all(lines.as_bytes()))
            .map_err(log_error)?;
        self.platform.sync_file(&log).map_err(log_error)?;

        let advanced: Vec<Head> = acceptances
            .iter()
            .zip(&ids)
            .map(|(acceptance, &id)| Head {
                seq: acceptance.body().seq,
                acceptance: id,
            })
            .collect();
        self.publish(&files, target, &advanced)
    }

    /// Saves the name of `target`, so the store can describe the acceptances
    /// of it that it holds, such as another target's landings. Call it before
    /// appending the target's first acceptance, or for a target this store
    /// doesn't follow. The target doesn't exist for this store until its first
    /// acceptance is appended.
    ///
    /// # Errors
    ///
    /// Fails if the name cannot be written.
    pub fn name_target(&self, target: &TargetName) -> Result<(), StoreError> {
        let files = self.target_files(target.id());
        if files.name().exists() {
            return Ok(());
        }
        fs::create_dir_all(&files.dir).map_err(|source| StoreError::io(&files.dir, source))?;
        let write = self.durable_write();
        write.stage(files.name(), target.as_str().as_bytes())?;
        write.commit()?;
        self.sync_dir(&self.targets_dir())
    }

    /// Durably replaces the head of a target with the last of `advanced`, the
    /// acceptances just appended, and logs that the target advanced to each.
    /// Their events come from those entries. Publishing the first head, which
    /// may name a later acceptance than the first, creates the target, which
    /// [`Store::name_target`] named before.
    fn publish(
        &self,
        files: &TargetFiles,
        target: &TargetName,
        advanced: &[Head],
    ) -> Result<(), StoreError> {
        let Some(&head) = advanced.last() else {
            return Ok(());
        };
        let write = self.durable_write();
        let line = format!("{} {}\n", head.seq, head.acceptance);
        write.stage(files.head(), line.as_bytes())?;
        let advanced: Vec<_> = advanced
            .iter()
            .map(|advanced| Effect::Advanced {
                target: target.clone(),
                seq: advanced.seq,
                acceptance: advanced.acceptance,
            })
            .collect();
        self.commit_logged(write, &advanced)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
