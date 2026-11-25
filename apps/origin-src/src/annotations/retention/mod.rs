//! Dropping the payloads of statements once their retention ends.

use std::collections::HashSet;
use std::time::SystemTime;

use origin_platform::Platform;
use origin_shared::{ObjectId, RecordId};

use super::Annotations;
use crate::index::Index;
use crate::policy::Retention;
use crate::store::StoreError;

/// What garbage collection deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    /// The number of payloads deleted.
    pub payloads: usize,
    /// The bytes freed on disk.
    pub bytes: u64,
}

impl<P: Platform> Annotations<'_, P> {
    /// Deletes the payloads whose retention ended by `now`, counting from when
    /// this node stored their statement. Statements themselves stay. A payload
    /// stays while any statement whose retention has not ended refers to it, or
    /// while a revision or a tree refers to the same blob (an empty file and an
    /// empty payload, for example).
    ///
    /// Waits for the writers [holding
    /// objects](crate::store::Store::hold_objects), landings on `policy` among
    /// them, then reads the rules from `retention` and the index again. This
    /// way no writer refers to a payload deleted under it, and a policy landed
    /// in the meantime applies. If there are no rules, it returns without
    /// waiting or reading anything.
    ///
    /// # Errors
    ///
    /// Fails if `retention` fails, a record cannot be read, or a payload cannot
    /// be deleted.
    pub fn drop_expired_payloads<E: From<StoreError>>(
        &mut self,
        retention: impl Fn() -> Result<Retention, E>,
        now: SystemTime,
    ) -> Result<Dropped, E> {
        if retention()?.is_empty() {
            return Ok(Dropped::default());
        }
        let _collecting = self.store.lock_for_collection()?;
        let retention = retention()?;
        if retention.is_empty() {
            return Ok(Dropped::default());
        }
        self.index = Index::open(self.store)?;
        let mut expired = HashSet::new();
        let mut live: HashSet<ObjectId> = HashSet::new();
        let mut roots = Vec::new();
        for &record in self.index.arrivals() {
            match record {
                RecordId::Revision(id) => {
                    let revision = self.store.get(id)?;
                    roots.push(revision.snapshot);
                    live.insert(revision.description.into());
                }
                RecordId::Acceptance(id) => roots.push(self.store.get(id)?.body().state),
                RecordId::Statement(id) => {
                    let statement = self.store.get(id)?;
                    if let RecordId::Snapshot(tree) = statement.subject {
                        roots.push(tree);
                    }
                    let Some(payload) = statement.payload else {
                        continue;
                    };
                    let ended = match retention.period(&statement.kind) {
                        Some(period) => {
                            let stored = self.store.stored_at(id.into())?;
                            now.duration_since(stored).is_ok_and(|age| age >= period)
                        }
                        None => false,
                    };
                    if ended {
                        expired.insert(payload.into());
                    } else {
                        live.insert(payload.into());
                    }
                }
                RecordId::Snapshot(_) => {}
            }
        }
        if expired.is_empty() {
            return Ok(Dropped::default());
        }
        self.store.reach(roots, &mut live)?;
        let mut doomed = Vec::new();
        for &id in expired.difference(&live) {
            if self.store.contains(id)? {
                doomed.push(id);
            }
        }
        Ok(Dropped {
            payloads: doomed.len(),
            bytes: self.store.remove(&doomed)?,
        })
    }
}

#[cfg(test)]
mod tests;
