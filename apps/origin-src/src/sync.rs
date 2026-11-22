//! Sync: sharing records with a [remote](crate::remote).
//!
//! - **Fetching** the records of changes compares them with the remote's,
//!   by id, and fetches those missing, with the records they refer to, and
//!   the records of the changes those belong to.
//! - Snapshots don't come with records: [`Sync::need`] fetches one when
//!   something reads it, such as a checkout. A snapshot is marked while it's
//!   fetched ([`Store::fetching`]), so one whose fetch was interrupted is
//!   fetched again rather than taken for whole.
//!
//! Transfers are bounded: a fetch's answers stop at
//! [`Pack::LIMIT`](origin_shared::pack::Pack::LIMIT), with the rest asked
//! for again (see [`Part`]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem;

use origin_platform::Native;
use origin_shared::api::{FetchRecords, FetchSnapshot, InventoryRequest};
use origin_shared::object::DecodeError;
use origin_shared::pack::Part;
use origin_shared::{
    ChangeId, EncodedObject, ObjectId, ObjectKind, Principal, PrincipalName, Revision, TreeId,
};

use crate::annotations::Annotations;
use crate::remote::{Remote, RemoteError};
use crate::service::Dependencies;
use crate::store::{Batch, Store, StoreError};

/// A store, and the remote it syncs with.
pub struct Sync<'a> {
    store: &'a Store,
    remote: &'a Remote,
    /// This node, which flags the divergence fetched revisions reveal.
    node: Principal,
}

/// What fetching records did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fetched {
    /// The revisions fetched.
    pub revisions: usize,
    /// The statements fetched.
    pub statements: usize,
}

impl<'a> Sync<'a> {
    /// Syncs `store` with `remote`, as the node `node`.
    pub fn new(store: &'a Store, remote: &'a Remote, node: PrincipalName) -> Self {
        Self {
            store,
            remote,
            node: Principal::Node(node),
        }
    }

    /// Fetches the records of `changes` the remote has and this node
    /// doesn't: their revisions and the statements about them, with the
    /// records they refer to, and then the records of the changes the
    /// revisions fetched belong to, so every change this node learns of
    /// comes whole. Flags the changes that diverged.
    ///
    /// # Errors
    ///
    /// Fails if the remote or the store fails.
    pub fn fetch_changes(&self, changes: &[ChangeId]) -> Result<Fetched, SyncError> {
        let mut fetched = Fetched::default();
        let mut asked: HashSet<ChangeId> = changes.iter().copied().collect();
        let mut next: Vec<ChangeId> = asked.iter().copied().collect();
        let mut revised = BTreeSet::new();
        // Objects found stored stay until what refers to them is written.
        let _held = self.store.hold_objects()?;
        while !next.is_empty() {
            let inventories = self.remote.inventory(&InventoryRequest {
                changes: mem::take(&mut next),
            })?;
            let ids = inventories.iter().flat_map(|inventory| {
                let revisions = inventory.revisions.iter().map(|&id| ObjectId::from(id));
                revisions.chain(inventory.statements.iter().map(|&id| id.into()))
            });
            let held = self.store.batch();
            let records = self.closure(&held, ids, &HashSet::new())?;
            held.commit()?;
            for change in Self::revised(&records)? {
                fetched.revisions += 1;
                revised.insert(change);
                if asked.insert(change) {
                    next.push(change);
                }
            }
            fetched.statements += records
                .iter()
                .filter(|record| record.kind().ok() == Some(ObjectKind::Statement))
                .count();
            if !records.is_empty() {
                self.store.write_all(&records)?;
            }
        }
        if !revised.is_empty() {
            let annotations = Annotations::open(self.store)?;
            for change in revised {
                annotations.flag_divergence(change, self.node.clone())?;
            }
        }
        Ok(fetched)
    }

    /// The change of each revision of `records`, one for each.
    fn revised(records: &[EncodedObject]) -> Result<Vec<ChangeId>, SyncError> {
        let mut changes = Vec::new();
        for record in records {
            if record.kind().ok() == Some(ObjectKind::Revision) {
                let revision: Revision =
                    record.decode_as().map_err(|source| SyncError::Malformed {
                        id: record.id(),
                        source,
                    })?;
                changes.push(revision.change);
            }
        }
        Ok(changes)
    }

    /// Fetches the records `ids` names that this node lacks, other than
    /// those of `skip`, with the records they refer to, recursively: a
    /// revision's base and dependencies, a statement's subject and the
    /// statements it supersedes or promotes, but not their snapshots. Each
    /// comes with the blob it carries. Those this node has are made durable
    /// by `held`, which must commit before anything refers to them.
    fn closure(
        &self,
        held: &Batch<'_, Native>,
        ids: impl IntoIterator<Item = ObjectId>,
        skip: &HashSet<ObjectId>,
    ) -> Result<Vec<EncodedObject>, SyncError> {
        let mut seen = HashSet::new();
        let mut wanted = Vec::new();
        let mut want = |id: ObjectId, wanted: &mut Vec<ObjectId>| {
            if !skip.contains(&id) && !held.depend(id)? && seen.insert(id) {
                wanted.push(id);
            }
            Ok::<_, StoreError>(())
        };
        for id in ids {
            want(id, &mut wanted)?;
        }
        let mut objects = Vec::new();
        while !wanted.is_empty() {
            let mut request = FetchRecords {
                ids: mem::take(&mut wanted),
            };
            while !request.ids.is_empty() {
                let Part { pack, more } = self.remote.records(&request)?;
                // A part that stops short holds the records up to the last
                // it has: the rest come from asking again.
                let rest = if more {
                    let position: HashMap<ObjectId, usize> = request
                        .ids
                        .iter()
                        .enumerate()
                        .map(|(position, &id)| (id, position))
                        .collect();
                    let last = pack
                        .objects
                        .iter()
                        .filter_map(|object| position.get(&object.id()))
                        .max();
                    last.map_or_else(Vec::new, |&last| request.ids.split_off(last + 1))
                } else {
                    Vec::new()
                };
                for object in pack.objects {
                    if matches!(
                        object.kind(),
                        Ok(ObjectKind::Revision | ObjectKind::Statement)
                    ) {
                        let decoded = object.decode().map_err(|source| SyncError::Malformed {
                            id: object.id(),
                            source,
                        })?;
                        for (id, _) in Dependencies::of(&decoded).records {
                            want(id, &mut wanted)?;
                        }
                    }
                    objects.push(object);
                }
                request.ids = rest;
            }
        }
        Ok(objects)
    }

    /// Fetches the trees and blobs of `snapshot`, unless the store holds it
    /// whole, skipping what the snapshots `have`, which it holds whole,
    /// hold at the same path. Returns whether it fetched.
    ///
    /// # Errors
    ///
    /// Fails if the remote doesn't have the snapshot, or the remote or the
    /// store fails.
    pub fn need(&self, snapshot: TreeId, have: &[TreeId]) -> Result<bool, SyncError> {
        if self.store.holds(snapshot)? {
            return Ok(false);
        }
        let request = FetchSnapshot {
            snapshot,
            have: self.store.held_whole(have.iter().copied())?,
            paths: Vec::new(),
            skip: 0,
        };
        self.fetch_parts(request, Some(snapshot))?;
        Ok(true)
    }

    /// Fetches `snapshot` whole again, whatever the store holds: what the
    /// store lacks of a snapshot it has the root of, which reading it
    /// found.
    ///
    /// # Errors
    ///
    /// Fails like [`Sync::need`].
    pub fn repair(&self, snapshot: TreeId) -> Result<(), SyncError> {
        let request = FetchSnapshot {
            snapshot,
            have: Vec::new(),
            paths: Vec::new(),
            skip: 0,
        };
        self.fetch_parts(request, Some(snapshot))
    }

    /// Fetches what `request` asks for, part after part, in one batch. With
    /// `whole`, the snapshot it fetches whole, which is marked until the
    /// batch is durable.
    fn fetch_parts(
        &self,
        mut request: FetchSnapshot,
        whole: Option<TreeId>,
    ) -> Result<(), SyncError> {
        // Objects found stored stay until the trees referring to them are.
        let _held = self.store.hold_objects()?;
        let batch = self.store.batch();
        let fetching = whole
            .map(|snapshot| self.store.fetching(snapshot, &batch))
            .transpose()?;
        loop {
            let Part { pack, more } = self.remote.snapshot(&request)?;
            batch.stage_all(&pack.objects)?;
            if !more || pack.objects.is_empty() {
                break;
            }
            request.skip += pack.objects.len() as u64;
        }
        batch.commit()?;
        if let Some(fetching) = fetching {
            fetching.done()?;
        }
        Ok(())
    }
}

/// A failure to sync with a remote.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The remote sent an object that doesn't decode.
    #[error("the remote sent a malformed object {id}")]
    Malformed {
        /// The object.
        id: ObjectId,
        /// What is wrong.
        source: DecodeError,
    },
    /// The remote failed.
    #[error(transparent)]
    Remote(#[from] RemoteError),
    /// Objects cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use origin_platform::Native;
    use origin_shared::pack::Pack;
    use origin_shared::{NodeKey, RepoPath};

    use super::*;
    use crate::queue::tests::{Repo, ci_test};
    use crate::service::{Node, OriginService};
    use crate::tree::Snapshots;
    use crate::tree::tests::build;

    const MAIN_RULES: &str = "[targets.main]\nauthority = \"node:queue\"\n";

    /// A node's own repository, empty, syncing with the queue's repository
    /// of `server` in-process.
    struct Client {
        _dir: tempfile::TempDir,
        store: Store,
        remote: Remote,
    }

    impl Client {
        fn new(server: &Repo) -> Self {
            Self::answered_in_parts(server, Pack::LIMIT)
        }

        /// A client of a server answering in parts of `limit` bytes.
        fn answered_in_parts(server: &Repo, limit: usize) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path().join(".origin"), Native::default());
            store.init().unwrap();
            let node = Node {
                name: "queue".parse().unwrap(),
                key: Arc::new(NodeKey::from_bytes(&[7; 32])),
            };
            let service = OriginService::new(server.store.clone(), node).with_pack_limit(limit);
            let remote = Remote::Local(Box::new(service));
            Self {
                _dir: dir,
                store,
                remote,
            }
        }

        fn sync(&self) -> Sync<'_> {
            Sync::new(&self.store, &self.remote, "laptop".parse().unwrap())
        }
    }

    #[test]
    fn fetches_the_records_of_changes() {
        let server = Repo::new(MAIN_RULES);
        let first = server.change_on_main(1, &[("a", "a")]);
        let annotations = server.annotations();
        annotations.issue(&ci_test(first, "node:ci"), None).unwrap();
        let client = Client::new(&server);

        let fetched = client
            .sync()
            .fetch_changes(&[ChangeId::from_bytes([1; 16])])
            .unwrap();
        assert_eq!(
            fetched,
            Fetched {
                revisions: 1,
                statements: 1
            }
        );
        assert!(client.store.contains(first.into()).unwrap());
        let again = client
            .sync()
            .fetch_changes(&[ChangeId::from_bytes([1; 16])])
            .unwrap();
        assert_eq!(again, Fetched::default());
    }

    #[test]
    fn interrupted_snapshot_fetches_are_fetched_again() {
        let server = Repo::new(MAIN_RULES);
        let revision = server.change_on_main(1, &[("src/a", "a")]);
        let client = Client::new(&server);
        client
            .sync()
            .fetch_changes(&[ChangeId::from_bytes([1; 16])])
            .unwrap();
        let snapshot = client.store.get(revision).unwrap().snapshot;
        let empty = build(&client.store, &[]);

        // A fetch cut short once the root was published, not what it holds.
        let batch = client.store.batch();
        let fetching = client.store.fetching(snapshot, &batch).unwrap();
        batch
            .stage_all(&[server.store.read(snapshot.into()).unwrap()])
            .unwrap();
        batch.commit().unwrap();
        drop(fetching);
        assert!(!client.store.holds(snapshot).unwrap());
        assert!(client.sync().need(snapshot, &[]).unwrap(), "fetched again");
        assert!(client.store.holds(snapshot).unwrap());
        assert_eq!(client.store.diff(empty, snapshot).unwrap().len(), 1);

        // One that lacks a tree, unmarked, is repaired whole.
        let src = Snapshots::new(&client.store)
            .entry_at(snapshot, &RepoPath::new("src").unwrap())
            .unwrap()
            .unwrap();
        client.store.remove(&[src.id]).unwrap();
        assert!(client.store.diff(empty, snapshot).is_err());
        client.sync().repair(snapshot).unwrap();
        assert_eq!(client.store.diff(empty, snapshot).unwrap().len(), 1);
    }
}
