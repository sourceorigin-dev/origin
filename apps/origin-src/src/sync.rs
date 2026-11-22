//! Sync: sharing records with a [remote](crate::remote).
//!
//! - **Fetching** brings the chain of each target this node follows, those
//!   it holds and those asked for, from the last acceptance it holds on.
//!   Each acceptance must follow the one before it, and is verified against
//!   the key of the authority named by the policy governing it
//!   ([`Policy::governing`]): for an acceptance of `policy`, the one before
//!   it, and for any other, the policy it names, which must be on the chain
//!   of `policy` as this node verified it. Only the first acceptance of
//!   `policy` is checked against its own. Acceptances this node merely
//!   stores, such as the base of a revision fetched, never govern anything.
//!   Each acceptance comes with what it landed, and the records of the
//!   changes that brings.
//! - Then the records of each change that moved are compared with the
//!   remote's, by id, and those missing fetched, with the records they
//!   refer to, and the records of the changes those belong to. The changes
//!   that moved are those the remote's events since the last fetch name
//!   ([`Sync::moved`]), so a fetch that brings nothing costs the same
//!   however long the history: only without a cursor, or once it expired,
//!   are all the changes this node knows compared.
//! - Snapshots don't come with records: [`Sync::need`] fetches one when
//!   something reads it, such as a checkout. A snapshot is marked while it's
//!   fetched ([`Store::fetching`]), so one whose fetch was interrupted is
//!   fetched again rather than taken for whole.
//!
//! Transfers are bounded: a fetch's answers stop at
//! [`Pack::LIMIT`](origin_shared::pack::Pack::LIMIT), with the rest asked
//! for again (see [`Part`]). Fetched records are written before the chains
//! that refer to them.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem;

use origin_platform::Native;
use origin_shared::api::{
    ErrorCode, EventsQuery, FetchRecords, FetchSnapshot, InventoryRequest, List, LogQuery,
};
use origin_shared::event::EventKind;
use origin_shared::object::{BadSignature, DecodeError};
use origin_shared::pack::Part;
use origin_shared::{
    Acceptance, AcceptanceId, ChangeId, EncodedObject, Head, Object, ObjectId, ObjectKind,
    Principal, PrincipalName, Record, RecordId, Revision, TargetName, TreeId,
};

use crate::annotations::Annotations;
use crate::index::Index;
use crate::policy::{Policy, PolicyError};
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

/// A target whose chain a fetch extended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advanced {
    /// The target.
    pub target: TargetName,
    /// Its head before, if this node followed it.
    pub from: Option<Head>,
    /// Its head now.
    pub to: Head,
}

/// What fetching records did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fetched {
    /// The revisions fetched.
    pub revisions: usize,
    /// The statements fetched.
    pub statements: usize,
}

/// What a fetch did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetch {
    /// The targets whose chains it extended.
    pub advanced: Vec<Advanced>,
    /// The records it fetched.
    pub fetched: Fetched,
    /// The remote's event cursor to fetch from next time.
    pub cursor: Cursor,
}

/// Where the remote's events are read from: after a sequence number of
/// its op log, which only means something in the log of its generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// The generation of the remote's op log.
    pub generation: String,
    /// The sequence number to read after.
    pub next: u64,
}

/// What the remote's events say moved since a cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    /// The remote's cursor after those events: where the next fetch reads
    /// from.
    pub cursor: Cursor,
    /// The changes that have revisions or statements the events name, or
    /// `None` if the events since the cursor are unknown: there was none,
    /// the remote no longer keeps them, or it's of another op log.
    pub changes: Option<HashSet<ChangeId>>,
}

/// The new acceptances of a target, verified and about to be appended.
struct Chain {
    target: TargetName,
    /// This node's head of the target, if it has one.
    from: Option<Head>,
    /// The remote's head: the last of `acceptances`.
    to: Head,
    acceptances: Vec<(AcceptanceId, Acceptance)>,
}

impl<'a> Sync<'a> {
    /// The most events a page of [`Sync::moved`] asks for: as many as a
    /// server sends.
    const EVENTS: u32 = 1024;

    /// Syncs `store` with `remote`, as the node `node`.
    pub fn new(store: &'a Store, remote: &'a Remote, node: PrincipalName) -> Self {
        Self {
            store,
            remote,
            node: Principal::Node(node),
        }
    }

    /// Fetches as the module explains: the chains of the targets this node
    /// follows and of `targets`, then the records of `changes` and of those
    /// this node knows that moved since `cursor`, the remote's event cursor
    /// as of the last fetch, or of all those it knows without one.
    ///
    /// # Errors
    ///
    /// Fails like [`Sync::fetch_chains`] and [`Sync::fetch_changes`].
    pub fn fetch(
        &self,
        targets: &[TargetName],
        changes: &[ChangeId],
        cursor: Option<&Cursor>,
    ) -> Result<Fetch, SyncError> {
        // Read first, so what moves while this fetches is fetched again.
        let moved = self.moved(cursor)?;
        let (advanced, landed) = self.chains(targets)?;
        let mut wanted: Vec<ChangeId> = match &moved.changes {
            // Nothing moved: the index needn't even be read.
            Some(moved) if moved.is_empty() => Vec::new(),
            Some(moved) => {
                let index = Index::open(self.store)?;
                moved
                    .iter()
                    .copied()
                    .filter(|&change| !index.revisions(change).is_empty())
                    .collect()
            }
            None => Index::open(self.store)?.changes().collect(),
        };
        wanted.extend(changes.iter().chain(&landed));
        wanted.sort_unstable();
        wanted.dedup();
        Ok(Fetch {
            advanced,
            fetched: self.fetch_changes(&wanted)?,
            cursor: moved.cursor,
        })
    }

    /// Fetches the chains of `policy`, of the targets this node holds, and
    /// of `more`, with what they landed, as the module explains. Returns
    /// the targets that moved, `policy` first. Learns the names of all the
    /// remote's targets, so acceptances of those this node doesn't follow
    /// can be described.
    ///
    /// # Errors
    ///
    /// Fails if the remote doesn't have a target of `more`, a target's
    /// chain differs from the remote's, an acceptance doesn't verify, or
    /// the remote or the store fails.
    pub fn fetch_chains(&self, more: &[TargetName]) -> Result<Vec<Advanced>, SyncError> {
        let (advanced, landed) = self.chains(more)?;
        self.fetch_changes(&landed)?;
        Ok(advanced)
    }

    /// [`Sync::fetch_chains`], but for the records of the changes that
    /// landed, which it returns, for the caller to fetch.
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
            // Targets only this node has stay as they are.
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
        // Objects found stored stay until the chains refer to them.
        let _held = self.store.hold_objects()?;
        let held = self.store.batch();
        let records = self.closure(&held, landed, &appending)?;
        held.commit()?;

        let mut revised = Self::revised(&records)?;
        revised.sort_unstable();
        revised.dedup();
        if !records.is_empty() {
            self.store.write_all(&records)?;
        }
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
        // The changes that landed come whole, so the next fetch only looks
        // for what moved since.
        Ok((advanced, revised))
    }

    /// The acceptances of `target` after the last this node holds, up to
    /// the remote's head, `theirs`, each checked against its id and target,
    /// and to follow the one before it. `None` if there are none.
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
        // The remote's log goes through this node's head.
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

    /// Checks that the authority each acceptance's governing policy names
    /// signed it, as the module explains. `chains` start with `policy`'s,
    /// whose acceptances may govern those after them once verified. The
    /// policies' snapshots are fetched as needed, and those of `policy`'s
    /// new acceptances always, so the policy in force can be read.
    fn verify(&self, chains: &[Chain]) -> Result<(), SyncError> {
        let mut policies: HashMap<AcceptanceId, Policy> = HashMap::new();
        // The acceptances of `policy` verified so far, and their snapshots.
        let mut verified: HashMap<AcceptanceId, TreeId> = HashMap::new();
        for chain in chains {
            for (id, acceptance) in &chain.acceptances {
                let body = acceptance.body();
                let unverified = |why| SyncError::Unverified {
                    target: chain.target.clone(),
                    seq: body.seq,
                    why,
                };
                // Small, and the policy in force once it's the tip.
                if chain.target == TargetName::POLICY {
                    self.need(body.state, &[])?;
                }
                let governing = Policy::governing(*id, body)
                    .map_err(|error| unverified(Unverified::Policy(error)))?;
                let policy = match policies.entry(governing) {
                    Entry::Occupied(policy) => policy.into_mut(),
                    Entry::Vacant(vacant) => {
                        // Only the first acceptance of `policy` governs
                        // itself: `chain` has it follow nothing.
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

    /// The snapshot of `id`, if it's on this node's chain of `policy`,
    /// which it verified as it appended it.
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

    /// The remote's cursor after its events since `since`, and the changes
    /// they say moved: those with a revision created or a statement issued
    /// about them, or about a statement about them. Without `since`, or if
    /// the remote no longer keeps its events, or its op log is of another
    /// generation, as once it was restored, the remote's current cursor,
    /// and no changes: then every change may have moved.
    ///
    /// # Errors
    ///
    /// Fails if the remote or the store fails.
    pub fn moved(&self, since: Option<&Cursor>) -> Result<Moved, SyncError> {
        // Should a name not parse, every event comes, and those of other
        // kinds name no change.
        let kinds = ["revision.created", "statement.issued"]
            .into_iter()
            .filter_map(|kind| kind.parse().ok())
            .collect();
        let mut query = EventsQuery {
            kind: List(kinds),
            limit: Some(Self::EVENTS),
            ..EventsQuery::default()
        };
        let everything = |query: &mut EventsQuery| {
            query.cursor = None;
            query.generation = None;
            let page = self.remote.events(query)?;
            Ok(Moved {
                cursor: Cursor {
                    generation: page.generation,
                    next: page.next,
                },
                changes: None,
            })
        };
        let Some(since) = since else {
            return everything(&mut query);
        };
        let mut cursor = since.clone();
        let mut events = Vec::new();
        loop {
            query.cursor = Some(cursor.next);
            query.generation = Some(cursor.generation.clone());
            let page = match self.remote.events(&query) {
                Err(error) if error.code() == Some(ErrorCode::Expired) => {
                    return everything(&mut query);
                }
                page => page?,
            };
            cursor.next = page.next;
            // A page short of the limit holds every event there was.
            let last = page.events.len() < Self::EVENTS as usize;
            events.extend(page.events.into_iter().map(|event| event.kind));
            if last {
                break;
            }
        }
        Ok(Moved {
            cursor,
            changes: Some(self.changes_moved(&events)?),
        })
    }

    /// The changes `events` name, as [`Sync::moved`] says. A statement is
    /// traced to its subject through the events, then this node's records:
    /// one about a record it knows nothing of is about no change it knows.
    fn changes_moved(&self, events: &[EventKind]) -> Result<HashSet<ChangeId>, SyncError> {
        let mut changes = HashSet::new();
        let mut revisions = HashMap::new();
        let mut subjects = HashMap::new();
        for event in events {
            match *event {
                EventKind::RevisionCreated {
                    revision, change, ..
                } => {
                    changes.insert(change);
                    revisions.insert(revision, change);
                }
                EventKind::StatementIssued {
                    statement, subject, ..
                } => {
                    subjects.insert(statement, subject);
                }
                _ => {}
            }
        }
        for &subject in subjects.values() {
            let mut subject = subject;
            // A chain of statements ends at a record, or one unknown.
            let change = loop {
                match subject {
                    RecordId::Revision(id) => match revisions.get(&id) {
                        Some(&change) => break Some(change),
                        None => break self.stored(id)?.map(|revision| revision.change),
                    },
                    RecordId::Statement(id) => match subjects.get(&id) {
                        Some(&next) if next != subject => subject = next,
                        _ => match self.stored(id)? {
                            Some(statement) => subject = statement.subject,
                            None => break None,
                        },
                    },
                    RecordId::Acceptance(_) | RecordId::Snapshot(_) => break None,
                }
            };
            changes.extend(change);
        }
        Ok(changes)
    }

    /// The record `id` names, if this node has it.
    fn stored<I: origin_shared::TypedId>(&self, id: I) -> Result<Option<I::Record>, StoreError> {
        match self.store.get(id) {
            Ok(record) => Ok(Some(record)),
            Err(StoreError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
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
                        Ok(ObjectKind::Revision | ObjectKind::Statement | ObjectKind::Acceptance)
                    ) {
                        let decoded = object.decode().map_err(|source| SyncError::Malformed {
                            id: object.id(),
                            source,
                        })?;
                        for (id, _) in Dependencies::of(&decoded).records {
                            want(id, &mut wanted)?;
                        }
                        // Another target's acceptance, landed on this one:
                        // what it landed comes too.
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

/// Why an acceptance fetched doesn't verify.
#[derive(Debug, thiserror::Error)]
pub enum Unverified {
    /// It doesn't match its id, or belongs to another target.
    #[error("it isn't acceptance {0} of the target")]
    Id(AcceptanceId),
    /// It doesn't follow the acceptance before it.
    #[error("it doesn't follow the acceptance before it")]
    Order,
    /// It names no policy, or another than the one governing it.
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
    /// A target's chain here isn't a start of the remote's.
    #[error("the history of `{0}` here differs from the remote's")]
    Diverged(TargetName),
    /// An acceptance fetched doesn't verify.
    #[error("acceptance #{seq} of `{target}` from the remote doesn't verify: {why}")]
    Unverified {
        /// Its target.
        target: TargetName,
        /// Its position.
        seq: u64,
        /// Why.
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
    /// A policy fetched cannot be read.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// Objects cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use origin_platform::Native;
    use origin_shared::event::Filter;
    use origin_shared::pack::Pack;
    use origin_shared::{AcceptanceBody, Base, NodeKey, RepoPath, Statement};

    use super::*;
    use crate::events::Events;
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

    fn main_tip(store: &Store) -> Base {
        Base::Acceptance(store.head(&TargetName::MAIN).unwrap().unwrap().acceptance)
    }

    /// Writes a snapshot of `policy.toml` making `key` the key of
    /// `node:queue`, the authority of `policy`, to `store`.
    fn policy_snapshot(store: &Store, key: &NodeKey) -> TreeId {
        let policy = format!(
            "[nodes]\nqueue = \"{}\"\n[targets.policy]\nauthority = \"node:queue\"\n{MAIN_RULES}",
            key.public()
        );
        let (file, tree) = Policy::snapshot(&policy);
        store.write_all(&[file, tree.clone()]).unwrap();
        tree.id().into()
    }

    #[test]
    fn fetches_verified_chains_and_what_they_landed_but_not_snapshots() {
        let server = Repo::new(MAIN_RULES);
        let landed = server.change_on_main(1, &[("a", "a")]);
        let result = server
            .annotations()
            .issue(&ci_test(landed, "node:ci"), None)
            .unwrap();
        server.land(1);
        let client = Client::new(&server);

        let advanced = client.sync().fetch_chains(&[]).unwrap();
        assert_eq!(advanced.len(), 1, "`policy` is always followed");
        let advanced = client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        assert_eq!(advanced[0].to, server.tip(&TargetName::MAIN));
        assert_eq!(
            client.store.log(&TargetName::MAIN).unwrap(),
            server.store.log(&TargetName::MAIN).unwrap()
        );
        assert!(
            client.store.contains(landed.into()).unwrap(),
            "what landed comes"
        );
        assert!(
            client.store.contains(result.into()).unwrap(),
            "with the records of its change"
        );
        assert!(Policy::in_force(&client.store).is_ok(), "the policy too");

        let state = client
            .store
            .get(server.tip(&TargetName::MAIN).acceptance)
            .unwrap()
            .body()
            .state;
        assert!(
            !client.store.contains(state.into()).unwrap(),
            "snapshots are lazy"
        );
        assert!(client.sync().need(state, &[]).unwrap());
        assert!(!client.sync().need(state, &[]).unwrap(), "once");
        assert_eq!(client.store.diff(state, state).unwrap(), []);
        assert_eq!(client.sync().fetch_chains(&[]).unwrap(), []);
    }

    #[test]
    fn acceptances_that_do_not_verify_are_refused() {
        let server = Repo::new(MAIN_RULES);
        let tip = server.tip(&TargetName::MAIN);
        let state = server.store.get(tip.acceptance).unwrap().body().state;
        let forged = AcceptanceBody {
            target: TargetName::MAIN.id(),
            seq: 1,
            prev: Some(tip.acceptance),
            state,
            landed: Vec::new(),
            evidence: Vec::new(),
            policy: Some(server.tip(&TargetName::POLICY).acceptance),
        }
        .sign(&NodeKey::from_bytes(&[8; 32]));
        server.store.append(&TargetName::MAIN, &forged).unwrap();
        let client = Client::new(&server);

        let error = client.sync().fetch_chains(&[TargetName::MAIN]).unwrap_err();
        assert!(
            matches!(
                &error,
                SyncError::Unverified {
                    seq: 1,
                    why: Unverified::Signature(_),
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(
            client.store.head(&TargetName::MAIN).unwrap(),
            None,
            "nothing of the chain was appended"
        );
    }

    #[test]
    fn only_the_chain_of_policy_governs() {
        let server = Repo::new(MAIN_RULES);
        let client = Client::new(&server);
        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        // An acceptance of `policy` off its chain, signed by another key,
        // which the client stores as the base of a revision it fetches.
        let attacker = NodeKey::from_bytes(&[8; 32]);
        let fake = AcceptanceBody {
            target: TargetName::POLICY.id(),
            seq: 40,
            prev: None,
            state: policy_snapshot(&server.store, &attacker),
            landed: Vec::new(),
            evidence: Vec::new(),
            policy: None,
        }
        .sign(&attacker);
        let fake_id: AcceptanceId = fake.encode().id().into();
        server.store.write_all(&[fake.encode()]).unwrap();
        server.revision(9, &[], Base::Acceptance(fake_id), &[("a", "a")]);
        client
            .sync()
            .fetch_changes(&[ChangeId::from_bytes([9; 16])])
            .unwrap();
        assert!(client.store.contains(fake_id.into()).unwrap());

        let tip = server.tip(&TargetName::MAIN);
        let forged = AcceptanceBody {
            target: TargetName::MAIN.id(),
            seq: tip.seq + 1,
            prev: Some(tip.acceptance),
            state: server.store.get(tip.acceptance).unwrap().body().state,
            landed: Vec::new(),
            evidence: Vec::new(),
            policy: Some(fake_id),
        }
        .sign(&attacker);
        server.store.append(&TargetName::MAIN, &forged).unwrap();
        let error = client.sync().fetch_chains(&[]).unwrap_err();
        assert!(
            matches!(
                error,
                SyncError::Unverified { why: Unverified::OffChain(id), .. } if id == fake_id
            ),
            "{error}"
        );
        assert_eq!(client.store.head(&TargetName::MAIN).unwrap(), Some(tip));
    }

    #[test]
    fn each_acceptance_of_policy_is_governed_by_the_one_before() {
        // The policy replaces the authority's key, then the old key extends
        // it, naming the policy it was replaced in, or the one before it.
        let extended_by_the_old_key = |names_genesis: bool| {
            let server = Repo::new(MAIN_RULES);
            let client = Client::new(&server);
            client.sync().fetch_chains(&[]).unwrap();
            let genesis = server.tip(&TargetName::POLICY).acceptance;
            let old = NodeKey::from_bytes(&[7; 32]);
            let rotation = AcceptanceBody {
                target: TargetName::POLICY.id(),
                seq: 1,
                prev: Some(genesis),
                state: policy_snapshot(&server.store, &NodeKey::from_bytes(&[8; 32])),
                landed: Vec::new(),
                evidence: Vec::new(),
                policy: Some(genesis),
            }
            .sign(&old);
            server.store.append(&TargetName::POLICY, &rotation).unwrap();
            client.sync().fetch_chains(&[]).unwrap();
            let rotation: AcceptanceId = rotation.encode().id().into();
            let reverted = AcceptanceBody {
                target: TargetName::POLICY.id(),
                seq: 2,
                prev: Some(rotation),
                state: server.store.get(genesis).unwrap().body().state,
                landed: Vec::new(),
                evidence: Vec::new(),
                policy: Some(if names_genesis { genesis } else { rotation }),
            }
            .sign(&old);
            server.store.append(&TargetName::POLICY, &reverted).unwrap();
            client.sync().fetch_chains(&[]).unwrap_err()
        };
        let error = extended_by_the_old_key(true);
        assert!(
            matches!(
                error,
                SyncError::Unverified {
                    seq: 2,
                    why: Unverified::Policy(PolicyError::NotPredecessor(2)),
                    ..
                }
            ),
            "{error}"
        );
        let error = extended_by_the_old_key(false);
        assert!(
            matches!(
                error,
                SyncError::Unverified {
                    seq: 2,
                    why: Unverified::Signature(_),
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn acceptances_fetched_before_their_chain_advance_it_once_appended() {
        let server = Repo::new(MAIN_RULES);
        server.change_on_main(1, &[("a", "a")]);
        server.land(1);
        let release = TargetName::new("release").unwrap();
        server
            .queue()
            .create_target(&release, &TargetName::MAIN)
            .unwrap();
        let main_tip = server.tip(&TargetName::MAIN).acceptance;
        let client = Client::new(&server);
        let events = Events::new(&client.store);

        // `release` landed `main`'s tip, which comes as a record.
        client.sync().fetch_chains(&[release]).unwrap();
        assert!(client.store.contains(main_tip.into()).unwrap());
        let advanced = |cursor| {
            let page = events
                .after(cursor, &Filter::default(), usize::MAX)
                .unwrap();
            let main: Vec<u64> = page
                .events
                .iter()
                .filter_map(|event| match &event.kind {
                    EventKind::TargetAdvanced { target, seq, .. }
                        if *target == TargetName::MAIN =>
                    {
                        Some(*seq)
                    }
                    _ => None,
                })
                .collect();
            (main, page.next)
        };
        let (main, cursor) = advanced(0);
        assert!(main.is_empty(), "not followed, so nothing advanced");

        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        assert_eq!(
            advanced(cursor).0,
            [0, 1],
            "the acceptance held before advances `main` too, once appended"
        );
    }

    #[test]
    fn fetches_the_records_of_changes() {
        let server = Repo::new(MAIN_RULES);
        let first = server.change_on_main(1, &[("a", "a")]);
        let annotations = server.annotations();
        annotations.issue(&ci_test(first, "node:ci"), None).unwrap();
        let client = Client::new(&server);
        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();

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
        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
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

    #[test]
    fn the_remote_events_say_which_changes_moved() {
        let server = Repo::new(MAIN_RULES);
        let first = server.change_on_main(1, &[("a", "a")]);
        let second = server.change_on_main(2, &[("b", "b")]);
        let client = Client::new(&server);
        let both = [1, 2].map(|change| ChangeId::from_bytes([change; 16]));
        client.sync().fetch_changes(&both).unwrap();
        let moved = client.sync().moved(None).unwrap();
        assert_eq!(moved.changes, None, "without a cursor, any change may have");
        let cursor = moved.cursor;
        let nothing = client.sync().moved(Some(&cursor)).unwrap();
        assert_eq!(nothing.changes, Some(HashSet::new()));
        assert_eq!(nothing.cursor, cursor);

        // A statement about a statement about the second change, and a
        // revision of the first.
        let annotations = server.annotations();
        let result = annotations
            .issue(&ci_test(second, "node:ci"), None)
            .unwrap();
        let flaky = Statement {
            subject: RecordId::Statement(result),
            kind: "flaky".parse().unwrap(),
            verdict: None,
            inputs: None,
            ..ci_test(second, "user:ada")
        };
        annotations.issue(&flaky, None).unwrap();
        let tip = main_tip(&server.store);
        server.revision(1, &[first], tip, &[("a", "again")]);
        let moved = client.sync().moved(Some(&cursor)).unwrap();
        assert_eq!(moved.changes, Some(HashSet::from(both)));
        assert!(moved.cursor.next > cursor.next);
    }

    #[test]
    fn cursors_of_another_op_log_read_everything() {
        let server = Repo::new(MAIN_RULES);
        let first = server.change_on_main(1, &[("a", "a")]);
        let change = ChangeId::from_bytes([1; 16]);
        let client = Client::new(&server);
        let fetch = client
            .sync()
            .fetch(&[TargetName::MAIN], &[change], None)
            .unwrap();
        // The remote was restored, with a new op log, and has a revision
        // since: a cursor of the old log, even ahead of the new one's
        // end, finds it.
        let cursor = Cursor {
            generation: "restored".to_owned(),
            next: fetch.cursor.next + 10_000,
        };
        let newer = server.revision(1, &[first], main_tip(&server.store), &[("a", "new")]);
        let fetch = client.sync().fetch(&[], &[], Some(&cursor)).unwrap();
        assert!(client.store.contains(newer.into()).unwrap());
        assert_ne!(fetch.cursor.generation, cursor.generation);
    }
}
