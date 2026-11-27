//! Sync: sharing records with a [remote](crate::remote).
//!
//! - **Fetching** brings each followed target's chain after the last
//!   acceptance this node holds, verified against its governing policy
//!   (`docs/design.md`). It then compares the records of changes with the
//!   remote's, by id, and fetches the missing ones and what they refer to.
//! - Snapshots are not fetched with records: [`Sync::need`] fetches one when
//!   something reads it, such as a checkout. [`Store::fetching`] marks it
//!   meanwhile, so an interrupted fetch is redone, not taken for a whole one.
//!
//! Each answer holds at most [`Pack::LIMIT`](origin_shared::pack::Pack::LIMIT)
//! bytes; the rest is asked for again (see [`Part`]).

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem;

use origin_platform::Native;
use origin_shared::api::{FetchRecords, FetchSnapshot, InventoryRequest, LogQuery};
use origin_shared::object::{BadSignature, DecodeError};
use origin_shared::pack::Part;
use origin_shared::{
    Acceptance, AcceptanceId, ChangeId, EncodedObject, Head, Object, ObjectId, ObjectKind,
    Principal, PrincipalName, Record, Revision, TargetName, TreeId,
};

use crate::annotations::Annotations;
use crate::policy::{Policy, PolicyError};
use crate::remote::{Remote, RemoteError};
use crate::service::Dependencies;
use crate::store::{Batch, Store, StoreError};

/// A store, and the remote it syncs with.
pub struct Sync<'a> {
    store: &'a Store,
    remote: &'a Remote,
    /// This node, which flags the changes that fetched revisions show
    /// diverged.
    node: Principal,
}

/// A target whose chain a fetch extended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advanced {
    /// The target.
    pub target: TargetName,
    /// Its head before the fetch, if this node followed it.
    pub from: Option<Head>,
    /// Its head now.
    pub to: Head,
}

/// What fetching records did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fetched {
    /// The number of revisions fetched.
    pub revisions: usize,
    /// The number of statements fetched.
    pub statements: usize,
}

/// A target's new acceptances, verified and ready to be appended.
struct Chain {
    target: TargetName,
    /// This node's head of the target, if it has one.
    from: Option<Head>,
    /// The remote's head: the last of `acceptances`.
    to: Head,
    acceptances: Vec<(AcceptanceId, Acceptance)>,
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

    /// Fetches the chains of `policy`, of the targets this node holds, and of
    /// `more`, with what they landed, as the module docs describe. Returns the
    /// targets that moved, `policy` first. Also learns the names of all the
    /// remote's targets, so it can describe acceptances of targets this node
    /// doesn't follow.
    ///
    /// # Errors
    ///
    /// Fails if the remote doesn't have a target of `more`, a target's chain
    /// differs from the remote's, an acceptance doesn't verify, or the remote or
    /// the store fails.
    pub fn fetch_chains(&self, more: &[TargetName]) -> Result<Vec<Advanced>, SyncError> {
        let (advanced, landed) = self.chains(more)?;
        self.fetch_changes(&landed)?;
        Ok(advanced)
    }

    /// Does what [`Sync::fetch_chains`] does, except fetching the records of
    /// the changes that landed: it returns those changes for the caller to
    /// fetch.
    fn chains(&self, more: &[TargetName]) -> Result<(Vec<Advanced>, Vec<ChangeId>), SyncError> {
        let remote: HashMap<TargetName, Head> = self
            .remote
            .targets()?
            .into_iter()
            .map(|target| (target.name, target.head))
            .collect();
        for name in remote.keys() {
            self.store.name_target(name)?;
        }
        let mut follow: BTreeSet<TargetName> = self.store.targets()?.into_iter().collect();
        for target in more {
            if !remote.contains_key(target) {
                return Err(SyncError::UnknownTarget(target.clone()));
            }
            follow.insert(target.clone());
        }
        follow.remove(&TargetName::POLICY);
        let mut chains = Vec::new();
        for target in [TargetName::POLICY].into_iter().chain(follow) {
            if let Some(&head) = remote.get(&target) {
                chains.extend(self.chain(target, head)?);
            }
        }
        self.verify(&chains)?;
        let appending: HashSet<ObjectId> = chains
            .iter()
            .flat_map(|chain| &chain.acceptances)
            .map(|&(id, _)| id.into())
            .collect();
        let landed = chains
            .iter()
            .flat_map(|chain| &chain.acceptances)
            .flat_map(|(_, acceptance)| &acceptance.body().landed)
            .map(|&source| source.id());
        let _held = self.store.hold_objects()?;
        let held = self.store.batch();
        let records = self.closure(&held, landed, &appending)?;
        held.commit()?;

        let mut revised = Self::revised(&records)?;
        revised.sort_unstable();
        revised.dedup();
        self.write(records)?;
        let mut advanced = Vec::new();
        for chain in chains {
            let acceptances: Vec<_> = chain.acceptances.into_iter().map(|(_, a)| a).collect();
            self.store.append_all(&chain.target, &acceptances, &[])?;
            advanced.push(Advanced {
                target: chain.target,
                from: chain.from,
                to: chain.to,
            });
        }
        Ok((advanced, revised))
    }

    /// Returns the acceptances of `target` after the last one this node holds,
    /// up to `theirs`, the remote's head. Checks that each matches its id and
    /// target, and follows the one before it. Returns `None` if there are
    /// none.
    fn chain(&self, target: TargetName, theirs: Head) -> Result<Option<Chain>, SyncError> {
        let ours = self.store.head(&target)?;
        if ours == Some(theirs) {
            return Ok(None);
        }
        let query = LogQuery {
            target: target.clone(),
            since: ours.map_or(0, |ours| ours.seq),
            limit: None,
        };
        let mut entries = self.remote.log(&query)?.into_iter();
        if let Some(ours) = ours
            && entries
                .next()
                .is_none_or(|entry| entry.id != ours.acceptance)
        {
            return Err(SyncError::Diverged(target));
        }
        let mut acceptances = Vec::new();
        let mut before = ours;
        for entry in entries {
            let body = entry.acceptance.body();
            let unverified = |why| SyncError::Unverified {
                target: target.clone(),
                seq: body.seq,
                why,
            };
            if entry.acceptance.encode().id() != entry.id.into() || body.target != target.id() {
                return Err(unverified(Unverified::Id(entry.id)));
            }
            if body.seq != before.map_or(0, |before| before.seq + 1)
                || body.prev != before.map(|before| before.acceptance)
            {
                return Err(unverified(Unverified::Order));
            }
            before = Some(Head {
                seq: body.seq,
                acceptance: entry.id,
            });
            acceptances.push((entry.id, entry.acceptance));
        }
        let Some(to) = before.filter(|_| !acceptances.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(Chain {
            target,
            from: ours,
            to,
            acceptances,
        }))
    }

    /// Checks that each acceptance is signed by the authority its governing
    /// policy names, as the module docs describe. `chains` starts with
    /// `policy`'s chain, whose acceptances, once verified, may govern the
    /// acceptances after them. Fetches the policies' snapshots as needed, and
    /// always those of `policy`'s new acceptances, to read the policy in
    /// force.
    fn verify(&self, chains: &[Chain]) -> Result<(), SyncError> {
        let mut policies: HashMap<AcceptanceId, Policy> = HashMap::new();
        let mut verified: HashMap<AcceptanceId, TreeId> = HashMap::new();
        for chain in chains {
            for (id, acceptance) in &chain.acceptances {
                let body = acceptance.body();
                let unverified = |why| SyncError::Unverified {
                    target: chain.target.clone(),
                    seq: body.seq,
                    why,
                };
                if chain.target == TargetName::POLICY {
                    self.need(body.state, &[])?;
                }
                let governing = Policy::governing(*id, body)
                    .map_err(|error| unverified(Unverified::Policy(error)))?;
                let policy = match policies.entry(governing) {
                    Entry::Occupied(policy) => policy.into_mut(),
                    Entry::Vacant(vacant) => {
                        let state = if governing == *id {
                            body.state
                        } else if let Some(&state) = verified.get(&governing) {
                            state
                        } else {
                            self.on_policy_chain(governing)?
                                .ok_or_else(|| unverified(Unverified::OffChain(governing)))?
                        };
                        self.need(state, &[])?;
                        let policy = Policy::of_snapshot(self.store, state)?
                            .ok_or(unverified(Unverified::NoPolicy))?;
                        vacant.insert(policy)
                    }
                };
                let authority = policy
                    .authority(&chain.target)
                    .ok_or(unverified(Unverified::NoAuthority))?;
                let key = policy
                    .node_key(authority)
                    .ok_or_else(|| unverified(Unverified::NoKey(authority.clone())))?;
                acceptance
                    .verify(key)
                    .map_err(|error| unverified(Unverified::Signature(error)))?;
                if chain.target == TargetName::POLICY {
                    verified.insert(*id, body.state);
                }
            }
        }
        Ok(())
    }

    /// Returns the snapshot of `id`, if `id` is on this node's chain of
    /// `policy`, which it verified when it appended it.
    fn on_policy_chain(&self, id: AcceptanceId) -> Result<Option<TreeId>, SyncError> {
        let acceptance = match self.store.get(id) {
            Ok(acceptance) => acceptance,
            Err(StoreError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let body = acceptance.body();
        let on_chain = body.target == TargetName::POLICY.id()
            && self.store.acceptance_at(body.target, body.seq)? == Some(id);
        Ok(on_chain.then_some(body.state))
    }

    /// Fetches the records of `changes` that the remote has and this node
    /// doesn't: their revisions and the statements about them, with the records
    /// they refer to. Then fetches the records of the changes those revisions
    /// belong to, so every change this node learns of is complete. Flags the
    /// changes that diverged.
    ///
    /// # Errors
    ///
    /// Fails if the remote or the store fails.
    pub fn fetch_changes(&self, changes: &[ChangeId]) -> Result<Fetched, SyncError> {
        let mut fetched = Fetched::default();
        let mut asked: HashSet<ChangeId> = changes.iter().copied().collect();
        let mut next: Vec<ChangeId> = asked.iter().copied().collect();
        let mut revised = BTreeSet::new();
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
            self.write(records)?;
        }
        if !revised.is_empty() {
            let annotations = Annotations::open(self.store)?;
            for change in revised {
                annotations.flag_divergence(change, self.node.clone())?;
            }
        }
        Ok(fetched)
    }

    /// Writes fetched `records` in [`Sync::layers`], each durable before the next.
    fn write(&self, records: Vec<EncodedObject>) -> Result<(), StoreError> {
        for layer in Self::layers(records) {
            if !layer.is_empty() {
                self.store.write_all(&layer)?;
            }
        }
        Ok(())
    }

    /// Splits fetched `records` into layers, as a push writes them: the records
    /// others need, then statements, then revisions. So a crash never publishes
    /// a revision without what it needs or its `core.conflict`.
    fn layers(records: Vec<EncodedObject>) -> [Vec<EncodedObject>; 3] {
        let mut layers = [Vec::new(), Vec::new(), Vec::new()];
        for record in records {
            let layer = match record.kind() {
                Ok(ObjectKind::Statement) => 1,
                Ok(ObjectKind::Revision) => 2,
                _ => 0,
            };
            layers[layer].push(record);
        }
        layers
    }

    /// Returns the change of each revision in `records`.
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

    /// Fetches the records `ids` names that this node lacks (except those in
    /// `skip`), and the records they refer to, recursively: a revision's base
    /// and dependencies, and a statement's subject and the statements it
    /// supersedes or promotes, but not their snapshots. Each record comes with
    /// the blob it carries. Records this node already has are made durable
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
                        Ok(ObjectKind::Revision | ObjectKind::Statement | ObjectKind::Acceptance)
                    ) {
                        let decoded = object.decode().map_err(|source| SyncError::Malformed {
                            id: object.id(),
                            source,
                        })?;
                        for (id, _) in Dependencies::of(&decoded).records {
                            want(id, &mut wanted)?;
                        }
                        if let Object::Acceptance(acceptance) = &decoded {
                            for source in &acceptance.body().landed {
                                want(source.id(), &mut wanted)?;
                            }
                        }
                    }
                    objects.push(object);
                }
                request.ids = rest;
            }
        }
        Ok(objects)
    }

    /// Fetches the trees and blobs of `snapshot`, unless the store already
    /// holds it whole. Skips what the snapshots `have` (which the store holds
    /// whole) hold at the same path. Returns whether it fetched.
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

    /// Fetches all of `snapshot` again, whatever the store holds. Used when
    /// reading a snapshot whose root the store has finds objects missing.
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
    /// `whole`, the snapshot is fetched whole and marked as being fetched until
    /// the batch is durable.
    fn fetch_parts(
        &self,
        mut request: FetchSnapshot,
        whole: Option<TreeId>,
    ) -> Result<(), SyncError> {
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

/// Why a fetched acceptance doesn't verify.
#[derive(Debug, thiserror::Error)]
pub enum Unverified {
    /// It doesn't match its id, or belongs to another target.
    #[error("it isn't acceptance {0} of the target")]
    Id(AcceptanceId),
    /// It doesn't follow the acceptance before it.
    #[error("it doesn't follow the acceptance before it")]
    Order,
    /// It names no policy, or a policy other than the one governing it.
    #[error("it names the wrong policy")]
    Policy(#[source] PolicyError),
    /// The policy it names isn't on the chain of `policy`.
    #[error("its policy {} isn't on the chain of `policy`", .0.short())]
    OffChain(AcceptanceId),
    /// Its policy has no `policy.toml`.
    #[error("its policy has no policy.toml")]
    NoPolicy,
    /// Its policy names no authority for the target.
    #[error("its policy names no authority for the target")]
    NoAuthority,
    /// Its policy lists no key for the authority.
    #[error("its policy lists no key for node:{0}")]
    NoKey(PrincipalName),
    /// The authority didn't sign it.
    #[error("its authority didn't sign it")]
    Signature(#[source] BadSignature),
}

/// A failure to sync with a remote.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The remote has no such target.
    #[error("the remote has no target `{0}`")]
    UnknownTarget(TargetName),
    /// A target's chain here isn't a prefix of the remote's.
    #[error("the history of `{0}` here differs from the remote's")]
    Diverged(TargetName),
    /// A fetched acceptance doesn't verify.
    #[error("acceptance #{seq} of `{target}` from the remote doesn't verify: {why}")]
    Unverified {
        /// Its target.
        target: TargetName,
        /// Its position on the chain.
        seq: u64,
        /// Why it doesn't verify.
        #[source]
        why: Unverified,
    },
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
    /// A fetched policy cannot be read.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// Objects cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests;
