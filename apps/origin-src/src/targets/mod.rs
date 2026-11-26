//! Target chains: the acceptances of each target, in order.
//!
//! A target lives in `targets/<id>/`, where the [`TargetId`] is the hash of
//! its name, so names never become file names:
//!
//! - `name`: the target's name.
//! - `log`: the id of each acceptance in hex, one line each, append-only.
//!   Appenders hold an exclusive lock on it, which the kernel releases if
//!   the process dies, and it is never replaced, so the lock always
//!   protects it.
//! - `head`: `<seq> <id>`, the latest published acceptance, replaced
//!   atomically.
//!
//! Appending writes the acceptance durably, then appends its id to the log
//! and syncs it, and only then publishes the new head. Readers go by the
//! head, so they only ever see durable acceptances. If a crash leaves the
//! log ahead of the head, or a partial line at its end, the next append
//! repairs it first, syncing the log before publishing what it holds.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::slice;

use origin_platform::{DurableWrite, LockMode, LockedFile, Platform};
use origin_shared::{
    Acceptance, AcceptanceId, Base, Head, Record, Revision, RevisionId, Source, TargetId,
    TargetName,
};

use crate::oplog::Effect;
use crate::store::{Store, StoreError};

/// Describes where a target is, for error messages.
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

    /// The published head.
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

    /// Parses the complete lines of a log, ignoring a partial last one.
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

    /// The last complete entry of a locked log, after cutting off a partial
    /// line left by a crash.
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

    /// The latest acceptance of `target`, or `None` if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Fails if the head cannot be read or is malformed.
    pub fn head(&self, target: &TargetName) -> Result<Option<Head>, StoreError> {
        self.target_files(target.id()).read_head()
    }

    /// The acceptances of `target`, oldest first.
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

    /// The names of every target, in no particular order.
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

    /// Appends `acceptance` to the chain of `target`, creating the target
    /// with its first acceptance.
    ///
    /// This is a compare-and-swap: the acceptance must follow the target's
    /// latest one, with the next sequence number and naming it as `prev`.
    /// Concurrent appends are serialized, and all but one of those following
    /// the same acceptance fail. Verifying the signature is the caller's job,
    /// since only it knows the target's authority.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Conflict`] if the acceptance doesn't follow
    /// the latest one, [`StoreError::WrongTarget`] if it belongs to another
    /// target, or if the target's files cannot be locked, read or written.
    pub fn append(
        &self,
        target: &TargetName,
        acceptance: &Acceptance,
    ) -> Result<AcceptanceId, StoreError> {
        let body = acceptance.body();
        if body.target != target.id() {
            return Err(StoreError::WrongTarget(target.clone()));
        }
        // A new policy changes which payloads collection keeps, so it never
        // lands while collection, which reads the policy first, runs.
        let _held = (*target == TargetName::POLICY)
            .then(|| self.hold_objects())
            .transpose()?;
        let encoded = acceptance.encode();
        // The acceptance is durable before the log refers to it.
        self.write_all(slice::from_ref(&encoded))?;
        let id = AcceptanceId::from(encoded.id());

        let files = self.target_files(body.target);
        fs::create_dir_all(&files.dir).map_err(|source| StoreError::io(&files.dir, source))?;
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
                    self.publish(&files, target, latest)?;
                }
                _ => return Err(StoreError::Malformed(files.log())),
            }
        }
        let follows = match latest {
            None => body.seq == 0 && body.prev.is_none(),
            Some(latest) => body.seq == latest.seq + 1 && body.prev == Some(latest.acceptance),
        };
        if !follows {
            return Err(StoreError::Conflict {
                target: target.clone(),
                expected: body.seq,
                actual: latest,
            });
        }

        let mut writer = &*log;
        writer
            .seek(SeekFrom::Start(body.seq * TargetFiles::LINE as u64))
            .and_then(|_| writeln!(writer, "{id}"))
            .map_err(log_error)?;
        self.platform.sync_file(&log).map_err(log_error)?;

        let head = Head {
            seq: body.seq,
            acceptance: id,
        };
        self.publish(&files, target, head)?;
        Ok(id)
    }

    /// Durably replaces the head of a target, and logs that it advanced. The
    /// target's `target.advanced` event comes from that entry. Publishing the
    /// first acceptance creates the target: its name is written too, and its
    /// directory made durable.
    fn publish(
        &self,
        files: &TargetFiles,
        target: &TargetName,
        head: Head,
    ) -> Result<(), StoreError> {
        let write = self.durable_write();
        let line = format!("{} {}\n", head.seq, head.acceptance);
        write.stage(files.head(), line.as_bytes())?;
        let created = head.seq == 0;
        if created {
            write.stage(files.name(), target.as_str().as_bytes())?;
        }
        let advanced = Effect::Advanced {
            target: target.clone(),
            seq: head.seq,
            acceptance: head.acceptance,
        };
        self.commit_logged(write, &[advanced])?;
        if created {
            self.sync_dir(&self.targets_dir())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
