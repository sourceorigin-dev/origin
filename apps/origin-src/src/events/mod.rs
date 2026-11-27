//! Events: what happened on this node, for plugins and agents to react to.
//! They are derived from the [op log](crate::oplog), the records its entries
//! name, and the queues' waiting landings, so nothing extra is stored. The
//! events are:
//!
//! - `statement.issued`, for every statement written, including conflicts,
//!   divergence and promotions: its subject, kind, verdict and issuer;
//! - `revision.created`, for every revision written;
//! - `target.advanced`, for every acceptance appended to a target;
//! - `check.requested`, for each statement a waiting landing lacks.
//!
//! An event's cursor is the sequence number of the op log entry it comes from.
//! It never changes, even across restarts and index rebuilds, and is never
//! reused. Reading the events after a cursor returns the next cursor to read
//! from.
//!
//! Events are delivered in the op log's order, at least once: reading from an
//! earlier cursor delivers its events again, and the events of one entry are
//! always delivered together. Consumers deduplicate by the [`EventKind`],
//! which is the whole event except its cursor.
//!
//! An entry whose write is in progress holds back the events after it until
//! the write is published. An entry whose writer crashed delivers what that
//! write published (see [`OpLog::settled`](crate::oplog::OpLog::settled)).
//! Entries lost to damage deliver nothing, and the damage is reported on
//! stderr.

use std::thread;
use std::time::{Duration, Instant};

use origin_platform::{Native, Platform};
use origin_shared::event::{Event, EventKind, Filter, Page};
use origin_shared::landing::{Blocked, Pending};
use origin_shared::{Object, ObjectId};
use parking_lot::{Condvar, Mutex};

use crate::index;
use crate::oplog::{Effect, Entry};
use crate::queue::Queue;
use crate::store::{Store, StoreError};

/// Reads the events of a store.
#[derive(Debug)]
pub struct Events<'a, P = Native> {
    store: &'a Store<P>,
    /// Wakes [`Events::wait`] as soon as this process writes, if set.
    /// Otherwise it only polls.
    wakeup: Option<&'a Wakeup>,
}

/// Wakes the readers in this process that wait for events as soon as the
/// process writes, instead of at their next poll of the op log. Writes from
/// other processes are still seen only at the next poll.
#[derive(Debug, Default)]
pub struct Wakeup {
    /// How many writes this process has made.
    writes: Mutex<u64>,
    written: Condvar,
}

impl Wakeup {
    /// Wakes every waiting reader. Call it after this process writes.
    pub fn notify(&self) {
        *self.writes.lock() += 1;
        self.written.notify_all();
    }

    /// How many writes this process has made so far.
    fn writes(&self) -> u64 {
        *self.writes.lock()
    }

    /// Waits up to `timeout`, or until the write count is no longer `seen`.
    fn sleep(&self, seen: u64, timeout: Duration) {
        let mut writes = self.writes.lock();
        self.written
            .wait_while_for(&mut writes, |writes| *writes == seen, timeout);
    }
}

impl<'a, P: Platform> Events<'a, P> {
    /// How often [`Events::wait`] reads the op log.
    pub const POLL: Duration = Duration::from_millis(50);

    /// Creates a reader of the events of `store`.
    pub fn new(store: &'a Store<P>) -> Self {
        Self {
            store,
            wakeup: None,
        }
    }

    /// Returns these events with [`Events::wait`] also woken by `wakeup`.
    #[must_use]
    pub fn waking_on(self, wakeup: &'a Wakeup) -> Self {
        Self {
            wakeup: Some(wakeup),
            ..self
        }
    }

    /// Returns the cursor after every event so far. Reading from it delivers
    /// only later events.
    ///
    /// # Errors
    ///
    /// Fails if the op log cannot be read.
    pub fn end(&self) -> Result<u64, StoreError> {
        self.store.oplog().settled()
    }

    /// Returns the events after `cursor` that pass `filter`, oldest first. It
    /// reads whole entries until it has at least `limit` events.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Expired`] if the op log no longer keeps the
    /// entries after `cursor`, or if the op log or a record cannot be read.
    pub fn after(&self, cursor: u64, filter: &Filter, limit: usize) -> Result<Page, StoreError> {
        let mut page = Page {
            events: Vec::new(),
            next: cursor,
            ..Page::default()
        };
        let oplog = self.store.oplog();
        let read = oplog.after(cursor)?;
        if read.entries.is_empty() {
            return Ok(page);
        }
        let settled = oplog.settled()?;
        let mut waiting = None;
        for entry in read.entries.iter().take_while(|entry| entry.seq <= settled) {
            page.events.extend(
                self.derive(entry, &mut waiting)?
                    .into_iter()
                    .filter(|kind| filter.matches(kind))
                    .map(|kind| Event {
                        cursor: entry.seq,
                        kind,
                    }),
            );
            page.next = entry.seq;
            if page.events.len() >= limit {
                break;
            }
        }
        Ok(page)
    }

    /// Like [`Events::after`], but waits up to `timeout` for events. It reads
    /// the op log every [`Events::POLL`], and also as soon as the [`Wakeup`],
    /// if any, reports a write by this process. Returns an empty page if none
    /// came.
    ///
    /// # Errors
    ///
    /// Fails like [`Events::after`].
    pub fn wait(
        &self,
        cursor: u64,
        filter: &Filter,
        limit: usize,
        timeout: Duration,
    ) -> Result<Page, StoreError> {
        let start = Instant::now();
        let mut cursor = cursor;
        loop {
            // Read before the op log, so no write after it is missed.
            let seen = self.wakeup.map(Wakeup::writes);
            let page = self.after(cursor, filter, limit)?;
            let left = timeout.saturating_sub(start.elapsed());
            if !page.events.is_empty() || left.is_zero() {
                return Ok(page);
            }
            // Entries with no matching events are read past.
            cursor = page.next;
            let pause = Self::POLL.min(left);
            match (self.wakeup, seen) {
                (Some(wakeup), Some(seen)) => wakeup.sleep(seen, pause),
                _ => thread::sleep(pause),
            }
        }
    }

    /// Returns the events of a settled entry, from what its write published.
    /// `waiting` caches the queues' waiting landings, read the first time an
    /// entry needs them.
    fn derive(
        &self,
        entry: &Entry,
        waiting: &mut Option<Vec<Pending>>,
    ) -> Result<Vec<EventKind>, StoreError> {
        Ok(match &entry.effect {
            Effect::Wrote(records) => self.wrote(records)?,
            Effect::Waiting { onto, proposal } => {
                let waiting = match waiting {
                    Some(waiting) => waiting,
                    None => waiting.insert(Queue::pending(self.store)?),
                };
                // A landing no longer waiting landed since, or was never
                // kept, if its writer crashed: either way there's nothing
                // to check.
                let pending = waiting
                    .iter()
                    .find(|pending| pending.onto == *onto && pending.proposal == *proposal);
                match pending {
                    Some(Pending {
                        subject,
                        blocked: Blocked::Evidence(missing),
                        ..
                    }) => missing
                        .iter()
                        .map(|requirement| EventKind::CheckRequested {
                            onto: onto.clone(),
                            subject: *subject,
                            requirement: requirement.clone(),
                        })
                        .collect(),
                    _ => Vec::new(),
                }
            }
            Effect::Moved(_) => Vec::new(),
            // A crash may have cut the move short.
            &Effect::Advanced {
                ref target,
                seq,
                acceptance,
            } => {
                let landed = self.store.acceptance_at(target.id(), seq)? == Some(acceptance);
                landed
                    .then(|| EventKind::TargetAdvanced {
                        target: target.clone(),
                        seq,
                        acceptance,
                    })
                    .into_iter()
                    .collect()
            }
        })
    }

    /// Returns the events of the records among `records` that the store has,
    /// which are those whose write was published.
    fn wrote(&self, records: &[index::Entry]) -> Result<Vec<EventKind>, StoreError> {
        let mut events = Vec::new();
        for record in records {
            let id = ObjectId::from(record.record());
            let Some(object) = self.store.try_read(id)? else {
                continue;
            };
            let object = object
                .decode()
                .map_err(|source| StoreError::Decode { id, source })?;
            events.extend(match object {
                Object::Statement(statement) => Some(EventKind::StatementIssued {
                    statement: id.into(),
                    subject: statement.subject,
                    kind: statement.kind,
                    verdict: statement.verdict,
                    issuer: statement.issuer,
                }),
                Object::Revision(revision) => Some(EventKind::RevisionCreated {
                    revision: id.into(),
                    change: revision.change,
                    author: revision.author,
                }),
                // Its event comes once a target advances to it.
                Object::Acceptance(_) | Object::Blob(_) | Object::Tree(_) => None,
            });
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests;
