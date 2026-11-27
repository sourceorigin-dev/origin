//! Sync: sharing records with a [remote](crate::remote); `docs/design.md`
//! has the details.
//!
//! - **Fetching** brings each followed target's chain after the last
//!   acceptance this node holds, verified against its governing policy, then
//!   the records of the changes that moved since the last fetch
//!   ([`Sync::moved`]). [`Sync::need`] fetches a snapshot when it is read,
//!   without the paths the user may not read ([`Store::with_withheld`]).
//! - **Pushing** shares revisions, with their snapshots, and the user's
//!   statements, refusing a stale change or one the user may not write.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::{iter, mem};

use origin_platform::Native;
use origin_shared::access::{Level, PathAccess};
use origin_shared::api::{
    ErrorCode, EventsQuery, Expected, FetchRecords, FetchSnapshot, Inventory, InventoryRequest,
    List, LogQuery, PutQuery,
};
use origin_shared::event::EventKind;
use origin_shared::object::{BadSignature, DecodeError};
use origin_shared::pack::{Pack, Packer, Part};
use origin_shared::{
    Acceptance, AcceptanceId, Base, ChangeId, EncodedObject, Head, Object, ObjectId, ObjectKind,
    Principal, PrincipalName, Record, RecordId, RepoPath, Revision, RevisionId, Statement,
    StatementId, TargetName, TreeId,
};

use crate::annotations::Annotations;
use crate::index::Index;
use crate::permissions::{Effects, Forbidden};
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
    /// The user, whose statements pushes share.
    user: Principal,
    /// The paths the user may write on the remote, as of the last fetch.
    writes: PathAccess,
    /// The most bytes one request of a push holds: [`Pack::LIMIT`], except in
    /// tests.
    pack_limit: usize,
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

/// A position in the remote's events: a sequence number of its op log. It
/// only means something in the op log of the same generation.
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
    /// The changes whose revisions or statements the events name, or `None` if
    /// the events since the cursor are unknown: there was no cursor, the remote
    /// no longer has those events, or the cursor is from another op log.
    pub changes: Option<HashSet<ChangeId>>,
}

/// What a push shared.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pushed {
    /// The number of revisions the remote didn't have.
    pub revisions: usize,
    /// The number of statements the remote didn't have.
    pub statements: usize,
    /// The number of objects sent: those revisions and statements, and the
    /// blobs and trees of their snapshots that the remote lacked.
    pub objects: usize,
    /// The user's statements left unsent: they need a record the remote
    /// neither has nor received (such as a statement about another change that
    /// they supersede, or the original they promote), or an unsent statement
    /// supersedes them.
    pub unsent: Vec<StatementId>,
}

/// The statements a push sends, in order, and those it leaves unsent.
struct Statements {
    statements: Vec<(StatementId, Statement)>,
    unsent: Vec<StatementId>,
}

/// What a push sends, and what each of its requests expects of the
/// remote.
#[derive(Default)]
struct Transfer {
    /// The objects, each after the objects it needs.
    ids: Vec<ObjectId>,
    /// The objects that go in the same request as the object before them.
    joined: HashSet<ObjectId>,
    /// The change of each revision sent, and the revisions it evolves from.
    revisions: HashMap<ObjectId, (ChangeId, Vec<RevisionId>)>,
    /// The heads of each change on the remote, as of the requests sent so
    /// far.
    heads: HashMap<ChangeId, Vec<RevisionId>>,
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

/// A union-find over items by position: joins items into groups, each
/// named by its first item.
struct Groups(Vec<usize>);

impl Groups {
    /// `len` items, each in its own group.
    fn new(len: usize) -> Self {
        Self((0..len).collect())
    }

    /// Returns the first item of the group holding item `at`.
    fn root(&mut self, mut at: usize) -> usize {
        while self.0[at] != at {
            self.0[at] = self.0[self.0[at]];
            at = self.0[at];
        }
        at
    }

    /// Joins the groups of `a` and `b`.
    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.root(a), self.root(b));
        self.0[a.max(b)] = a.min(b);
    }
}

impl Transfer {
    /// Adds `revisions`, each in one request with the reserved statements of
    /// `statements` about it (such as its conflicts or its abandonment) and the
    /// statements those need, so the remote never has a revision without them.
    /// Those statements go after the last revision they need, in one request
    /// with every revision from the first one they are about. Returns the other
    /// statements.
    fn add_revisions(
        &mut self,
        revisions: &[(RevisionId, Revision)],
        statements: Vec<(StatementId, Statement)>,
    ) -> Vec<(StatementId, Statement)> {
        let sent: HashMap<RecordId, usize> = revisions
            .iter()
            .enumerate()
            .map(|(at, &(id, _))| (RecordId::Revision(id), at))
            .collect();
        let position = Self::positions(&statements);
        let mut with: Vec<bool> = statements
            .iter()
            .map(|(_, statement)| {
                statement.kind.is_reserved() && sent.contains_key(&statement.subject)
            })
            .collect();
        let mut groups = Groups::new(statements.len());
        for at in (0..statements.len()).rev() {
            if !with[at] {
                continue;
            }
            for (needed, _) in Dependencies::of_statement(&statements[at].1).records {
                if let Some(&other) = position.get(&needed) {
                    with[other] = true;
                    groups.join(at, other);
                }
            }
        }
        let mut spans: HashMap<usize, (usize, usize)> = HashMap::new();
        for (at, (_, statement)) in statements.iter().enumerate() {
            if let Some(&revision) = sent.get(&statement.subject)
                && with[at]
            {
                let span = spans.entry(groups.root(at)).or_insert((usize::MAX, 0));
                if statement.kind.is_reserved() {
                    span.0 = span.0.min(revision);
                }
                span.1 = span.1.max(revision);
            }
        }
        let mut joined = vec![false; revisions.len()];
        let mut after = vec![Vec::new(); revisions.len()];
        for &(first, last) in spans.values() {
            joined[first + 1..=last].fill(true);
        }
        for at in (0..statements.len()).filter(|&at| with[at]) {
            after[spans[&groups.root(at)].1].push(statements[at].0);
        }
        for (((id, revision), joined), after) in revisions.iter().zip(joined).zip(after) {
            if joined {
                self.joined.insert((*id).into());
            }
            self.ids.push((*id).into());
            self.revisions.insert(
                (*id).into(),
                (revision.change, revision.predecessors.clone()),
            );
            for statement in after {
                self.joined.insert(statement.into());
                self.ids.push(statement.into());
            }
        }
        statements
            .into_iter()
            .zip(with)
            .filter_map(|(statement, with)| (!with).then_some(statement))
            .collect()
    }

    /// Adds `statements`, each after those of them it needs. The statements
    /// about one record, with those they need or that need them, go in one
    /// request. So a landing waiting on the record never takes a result whose
    /// replacement, or the change's abandonment, is still to come.
    fn add_statements(&mut self, statements: &[(StatementId, Statement)]) {
        let position = Self::positions(statements);
        let mut groups = Groups::new(statements.len());
        let mut about: HashMap<RecordId, usize> = HashMap::new();
        for (at, (_, statement)) in statements.iter().enumerate() {
            let same_subject = *about.entry(statement.subject).or_insert(at);
            let needed = Dependencies::of_statement(statement)
                .records
                .into_iter()
                .filter_map(|(id, _)| position.get(&id).copied());
            for other in iter::once(same_subject).chain(needed) {
                groups.join(at, other);
            }
        }
        let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for at in 0..statements.len() {
            members.entry(groups.root(at)).or_default().push(at);
        }
        for members in members.into_values() {
            for (nth, at) in members.into_iter().enumerate() {
                let id = statements[at].0.into();
                if nth > 0 {
                    self.joined.insert(id);
                }
                self.ids.push(id);
            }
        }
    }

    /// Returns the position of each of `statements` in the list.
    fn positions(statements: &[(StatementId, Statement)]) -> HashMap<ObjectId, usize> {
        statements
            .iter()
            .enumerate()
            .map(|(at, &(id, _))| (id.into(), at))
            .collect()
    }

    /// Returns the heads that the changes of `revisions`, sent in one request,
    /// must have on the remote before it: the heads left by the earlier
    /// requests. Then updates those heads as the request will.
    fn expect(&mut self, revisions: &[ObjectId]) -> Vec<Expected> {
        let mut expect: Vec<Expected> = Vec::new();
        for id in revisions {
            let (change, predecessors) = &self.revisions[id];
            let heads = self.heads.entry(*change).or_default();
            if !expect.iter().any(|expected| expected.change == *change) {
                expect.push(Expected {
                    change: *change,
                    heads: heads.clone(),
                });
            }
            heads.retain(|head| !predecessors.contains(head));
            heads.push((*id).into());
        }
        expect
    }
}

impl<'a> Sync<'a> {
    /// The most events one page of [`Sync::moved`] asks for: as many as a
    /// server sends.
    const EVENTS: u32 = 1024;

    /// Syncs `store` with `remote`, as the node `node`, for `user`. The user
    /// may write everything until [`Sync::writing`] says otherwise.
    pub fn new(store: &'a Store, remote: &'a Remote, node: PrincipalName, user: Principal) -> Self {
        Self {
            store,
            remote,
            node: Principal::Node(node),
            user,
            writes: PathAccess::uniform(Level::Write),
            pack_limit: Pack::LIMIT,
        }
    }

    /// Returns this sync, knowing the user may only write `paths`, so a push
    /// the remote would refuse fails before sending anything.
    #[must_use]
    pub fn writing(self, paths: PathAccess) -> Self {
        Self {
            writes: paths,
            ..self
        }
    }

    /// Returns this sync, pushing requests of at most `limit` bytes.
    #[cfg(test)]
    fn with_pack_limit(self, limit: usize) -> Self {
        Self {
            pack_limit: limit,
            ..self
        }
    }

    /// Fetches as the module docs describe: the chains of the targets this node
    /// follows and of `targets`, then the records of `changes` and of the
    /// changes this node knows that moved since `cursor` (the remote's event
    /// cursor as of the last fetch). Without a cursor, fetches the records of
    /// every change it knows.
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
        // INVARIANT: read first, so what moves while this fetches is fetched again.
        let moved = self.moved(cursor)?;
        let (advanced, landed) = self.chains(targets)?;
        let mut wanted: Vec<ChangeId> = match &moved.changes {
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
                    self.fetch_snapshot(RecordId::Acceptance(*id), body.state, &[])?;
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
                        self.fetch_snapshot(RecordId::Acceptance(governing), state, &[])?;
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

    /// Returns the remote's cursor after its events since `since`, and the
    /// changes those events say moved: those with a new revision, or a
    /// statement issued about them or about a statement about them. Without
    /// `since`, or if the remote no longer has those events, or its op log is
    /// of another generation (as after a restore), returns the remote's current
    /// cursor and no changes: every change may have moved.
    ///
    /// # Errors
    ///
    /// Fails if the remote or the store fails.
    pub fn moved(&self, since: Option<&Cursor>) -> Result<Moved, SyncError> {
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

    /// Returns the changes `events` name, as [`Sync::moved`] describes. A
    /// statement is traced to its subject through the events, then through this
    /// node's records. A statement about a record this node doesn't know is
    /// about no change it knows.
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

    /// Returns the record `id` names, if this node has it.
    fn stored<I: origin_shared::TypedId>(&self, id: I) -> Result<Option<I::Record>, StoreError> {
        match self.store.get(id) {
            Ok(record) => Ok(Some(record)),
            Err(StoreError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
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
    /// the attachment it carries. Records this node already has are made durable
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

    /// Fetches the trees and blobs of the snapshot of `record` (a revision or
    /// an acceptance this node has), unless the store already holds it whole.
    /// Skips what the snapshots `have` (which the store holds whole) hold at the
    /// same path. Returns whether it fetched.
    ///
    /// # Errors
    ///
    /// Fails if the record has no snapshot, the remote doesn't have it, or the
    /// remote or the store fails.
    pub fn need(&self, record: RecordId, have: &[TreeId]) -> Result<bool, SyncError> {
        let snapshot = self
            .store
            .snapshot_of(record)?
            .ok_or(SyncError::NoSnapshot(record))?;
        self.fetch_snapshot(record, snapshot, have)
    }

    /// Fetches the whole snapshot of `record` again, whatever the store holds.
    /// Used when reading a snapshot whose root the store has finds objects
    /// missing.
    ///
    /// # Errors
    ///
    /// Fails like [`Sync::need`].
    pub fn repair(&self, record: RecordId) -> Result<(), SyncError> {
        let snapshot = self
            .store
            .snapshot_of(record)?
            .ok_or(SyncError::NoSnapshot(record))?;
        let request = FetchSnapshot {
            of: record,
            have: Vec::new(),
            paths: Vec::new(),
            skip: 0,
        };
        self.fetch_parts(request, Some(snapshot))
    }

    /// Does what [`Sync::need`] does, given the snapshot of `record`.
    fn fetch_snapshot(
        &self,
        record: RecordId,
        snapshot: TreeId,
        have: &[TreeId],
    ) -> Result<bool, SyncError> {
        if self.store.holds(snapshot)? {
            return Ok(false);
        }
        let request = FetchSnapshot {
            of: record,
            have: self.store.held_whole(have.iter().copied())?,
            paths: Vec::new(),
            skip: 0,
        };
        self.fetch_parts(request, Some(snapshot))?;
        Ok(true)
    }

    /// Fetches what the snapshot of `record` holds under `paths`, whatever the
    /// store has. Used for files a checkout lacks, such as those withheld until
    /// the user could read them.
    ///
    /// # Errors
    ///
    /// Fails if the remote doesn't have the record, or the remote or the store
    /// fails.
    pub fn need_paths(&self, record: RecordId, paths: Vec<RepoPath>) -> Result<(), SyncError> {
        let request = FetchSnapshot {
            of: record,
            have: Vec::new(),
            paths,
            skip: 0,
        };
        self.fetch_parts(request, None)
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

    /// Shares `changes`, and the changes below them, with the remote, as the
    /// module docs describe.
    ///
    /// # Errors
    ///
    /// Fails if a change has no revision here, is stale, or is based on an
    /// acceptance the remote doesn't have, or if the remote or the store fails.
    /// Nothing is sent then, unless the change moved on the remote while the
    /// push was sending; then the snapshots and records already sent stay on
    /// the remote.
    pub fn push(&self, changes: &[ChangeId]) -> Result<Pushed, SyncError> {
        let index = Index::open(self.store)?;
        let changes = self.stack(&index, changes)?;
        let inventories = self.inventories(&index, &changes)?;
        let mut shared: HashSet<ObjectId> = HashSet::new();
        Self::share(&mut shared, &inventories);

        let mut revisions = Vec::new();
        for &change in &changes {
            for &id in index.revisions(change) {
                if !shared.contains(&id.into()) {
                    revisions.push((id, self.store.get(id)?));
                }
            }
        }
        let revisions = Self::bottom_up(revisions);
        let mut checked = HashSet::new();
        for (id, revision) in &revisions {
            if let Base::Acceptance(base) = revision.base
                && checked.insert(base)
            {
                self.check_shared(*id, revision, base)?;
            }
            self.check_writes(revision)?;
        }

        let mut content = Vec::new();
        let mut seen = HashSet::new();
        for (id, revision) in &revisions {
            let mut have = vec![self.store.base_snapshot(revision.base)?];
            for &predecessor in &revision.predecessors {
                if shared.contains(&predecessor.into()) {
                    have.push(self.store.get(predecessor)?.snapshot);
                }
            }
            let have = self.store.held_whole(have)?;
            for placed in self
                .store
                .added(revision.snapshot, &have, &[], None)?
                .into_iter()
                .rev()
            {
                if seen.insert(placed.entry.id) {
                    content.push(placed.entry.id);
                }
            }
            if seen.insert(revision.description.into()) {
                content.push(revision.description.into());
            }
            shared.insert((*id).into());
        }
        let Statements { statements, unsent } = self.statements(&index, &changes, &mut shared)?;
        for (_, statement) in &statements {
            if let Some(payload) = statement.payload
                && self.store.contains(payload.into())?
                && seen.insert(payload.into())
            {
                content.push(payload.into());
            }
        }
        let objects = content.len() + revisions.len() + statements.len();
        let pushed_statements = statements.len();
        let mut transfer = Transfer {
            ids: content,
            heads: inventories
                .into_iter()
                .map(|inventory| (inventory.change, inventory.heads))
                .collect(),
            ..Transfer::default()
        };
        let statements = transfer.add_revisions(&revisions, statements);
        transfer.add_statements(&statements);
        if let Err(error) = self.send(transfer) {
            if let SyncError::Remote(remote) = &error
                && remote.code() == Some(ErrorCode::Conflict)
            {
                self.inventories(&Index::open(self.store)?, &changes)?;
            }
            return Err(error);
        }
        Ok(Pushed {
            revisions: revisions.len(),
            statements: pushed_statements,
            objects,
            unsent,
        })
    }

    /// Returns what the remote holds of `changes`. Fails if the remote has a
    /// head of one that this node lacks: the change is stale.
    fn inventories(
        &self,
        index: &Index,
        changes: &[ChangeId],
    ) -> Result<Vec<Inventory>, SyncError> {
        let inventories = self.remote.inventory(&InventoryRequest {
            changes: changes.to_vec(),
        })?;
        for inventory in &inventories {
            let local = index.revisions(inventory.change);
            if let Some(&head) = inventory.heads.iter().find(|head| !local.contains(head)) {
                return Err(SyncError::Stale {
                    change: inventory.change,
                    head,
                });
            }
        }
        Ok(inventories)
    }

    /// Returns `revisions` sorted so each comes after those of them it evolves
    /// from, is based on, or depends on.
    fn bottom_up(revisions: Vec<(RevisionId, Revision)>) -> Vec<(RevisionId, Revision)> {
        let position: HashMap<RevisionId, usize> = revisions
            .iter()
            .enumerate()
            .map(|(position, &(id, _))| (id, position))
            .collect();
        let mut placed = vec![false; revisions.len()];
        let mut order = Vec::with_capacity(revisions.len());
        for start in 0..revisions.len() {
            let mut stack = vec![(start, false)];
            while let Some((at, below_placed)) = stack.pop() {
                if placed[at] {
                    continue;
                }
                if below_placed {
                    placed[at] = true;
                    order.push(at);
                    continue;
                }
                stack.push((at, true));
                let revision = &revisions[at].1;
                let base = match revision.base {
                    Base::Revision(base) => Some(base),
                    Base::Acceptance(_) => None,
                };
                let below = base.iter().chain(&revision.deps);
                for below in below.chain(&revision.predecessors) {
                    if let Some(&below) = position.get(below)
                        && !placed[below]
                    {
                        stack.push((below, false));
                    }
                }
            }
        }
        let mut revisions: Vec<_> = revisions.into_iter().map(Some).collect();
        order
            .into_iter()
            .filter_map(|at| revisions[at].take())
            .collect()
    }

    /// Adds the records of `inventories`, which the remote has, to `shared`.
    fn share(shared: &mut HashSet<ObjectId>, inventories: &[Inventory]) {
        for inventory in inventories {
            shared.extend(inventory.revisions.iter().map(|&id| ObjectId::from(id)));
            shared.extend(inventory.statements.iter().map(|&id| ObjectId::from(id)));
        }
    }

    /// Returns the user's statements about the revisions of `changes`, and
    /// about statements about them, that the remote lacks, each after those of
    /// them it needs, and adds each to `shared`. Also returns the statements
    /// that need a record not in `shared`, and those they supersede (see
    /// [`Sync::hold_back`]); those stay unsent. A needed record about another
    /// change is first looked for on the remote. Other users' statements are
    /// theirs to share, and the remote issues this node's own, such as
    /// divergence, itself.
    fn statements(
        &self,
        index: &Index,
        changes: &[ChangeId],
        shared: &mut HashSet<ObjectId>,
    ) -> Result<Statements, SyncError> {
        let mut subjects: Vec<RecordId> = changes
            .iter()
            .flat_map(|&change| index.revisions(change))
            .map(|&id| RecordId::Revision(id))
            .collect();
        let mut waiting = Vec::new();
        let mut seen = HashSet::new();
        while let Some(subject) = subjects.pop() {
            for &id in index.statements(subject) {
                if !seen.insert(id) {
                    continue;
                }
                subjects.push(RecordId::Statement(id));
                if shared.contains(&id.into()) {
                    continue;
                }
                let statement = self.store.get(id)?;
                if statement.issuer == self.user {
                    waiting.push((id, statement));
                }
            }
        }
        let mut ordered = Vec::new();
        let mut asked: HashSet<ChangeId> = changes.iter().copied().collect();
        loop {
            Self::order(&mut waiting, &mut ordered, shared);
            let mut more = Vec::new();
            for (_, statement) in &waiting {
                for (needed, kind) in Dependencies::of_statement(statement).records {
                    if !shared.contains(&needed)
                        && let Some(change) = self.change_of(needed, kind)?
                        && asked.insert(change)
                    {
                        more.push(change);
                    }
                }
            }
            if more.is_empty() {
                break;
            }
            let inventories = self.remote.inventory(&InventoryRequest { changes: more })?;
            Self::share(shared, &inventories);
        }
        Self::hold_back(&mut ordered, &mut waiting, shared);
        Ok(Statements {
            statements: ordered,
            unsent: waiting.into_iter().map(|(id, _)| id).collect(),
        })
    }

    /// Moves each statement of `waiting` whose needs are all in `shared` to
    /// `ordered`, after those it needs, and adds it to `shared`.
    fn order(
        waiting: &mut Vec<(StatementId, Statement)>,
        ordered: &mut Vec<(StatementId, Statement)>,
        shared: &mut HashSet<ObjectId>,
    ) {
        loop {
            let before = waiting.len();
            waiting.retain(|(id, statement)| {
                let ready = Dependencies::of_statement(statement)
                    .records
                    .iter()
                    .all(|(needed, _)| shared.contains(needed));
                if ready {
                    shared.insert((*id).into());
                    ordered.push((*id, statement.clone()));
                }
                !ready
            });
            if waiting.len() == before {
                break;
            }
        }
    }

    /// Moves back from `ordered` to `waiting`, and out of `shared`, each
    /// statement that one of `waiting` supersedes, and each statement that
    /// needs one moved back. Otherwise the remote could take a result without
    /// the failure that replaces it, and land with it.
    fn hold_back(
        ordered: &mut Vec<(StatementId, Statement)>,
        waiting: &mut Vec<(StatementId, Statement)>,
        shared: &mut HashSet<ObjectId>,
    ) {
        loop {
            let held: HashSet<ObjectId> = waiting
                .iter()
                .flat_map(|(id, statement)| {
                    iter::once(ObjectId::from(*id)).chain(statement.supersedes.map(Into::into))
                })
                .collect();
            let before = waiting.len();
            ordered.retain(|(id, statement)| {
                let stays = held.contains(&(*id).into())
                    || Dependencies::of_statement(statement)
                        .records
                        .iter()
                        .any(|(needed, _)| held.contains(needed));
                if stays {
                    shared.remove(&(*id).into());
                    waiting.push((*id, statement.clone()));
                }
                !stays
            });
            if waiting.len() == before {
                break;
            }
        }
    }

    /// Returns the change of the record `id` of kind `kind`: a revision's
    /// change, or the change of what a statement is about, following statements
    /// about statements. Returns `None` for other records, or if this node
    /// lacks a record on the way.
    fn change_of(
        &self,
        mut id: ObjectId,
        mut kind: ObjectKind,
    ) -> Result<Option<ChangeId>, SyncError> {
        while kind == ObjectKind::Statement {
            let Some(statement) = self.stored(StatementId::from(id))? else {
                return Ok(None);
            };
            (id, kind) = (statement.subject.into(), statement.subject.kind());
        }
        if kind != ObjectKind::Revision {
            return Ok(None);
        }
        Ok(self
            .stored(RevisionId::from(id))?
            .map(|revision| revision.change))
    }

    /// Sends the objects of `transfer`, in order, in requests of at most
    /// [`Pack::LIMIT`] bytes, keeping joined objects in the same request.
    fn send(&self, mut transfer: Transfer) -> Result<(), SyncError> {
        let ids = mem::take(&mut transfer.ids);
        let mut packer = Packer::new(self.pack_limit);
        let mut together = Vec::new();
        let mut filling = Vec::new();
        self.store.read_each(&ids, |object| {
            if !transfer.joined.contains(&object.id()) {
                let objects = mem::take(&mut together);
                self.add(&mut packer, objects, &mut filling, &mut transfer)?;
            }
            together.push(object);
            Ok::<_, SyncError>(true)
        })?;
        self.add(&mut packer, together, &mut filling, &mut transfer)?;
        if !packer.is_empty() {
            self.put(packer.finish(), &filling, &mut transfer)?;
        }
        Ok(())
    }

    /// Adds `together` to `packer`, all in one pack. If they don't fit, first
    /// sends the full pack, whose revisions are `filling`.
    fn add(
        &self,
        packer: &mut Packer,
        together: Vec<EncodedObject>,
        filling: &mut Vec<ObjectId>,
        transfer: &mut Transfer,
    ) -> Result<(), SyncError> {
        let revisions: Vec<ObjectId> = together
            .iter()
            .map(EncodedObject::id)
            .filter(|id| transfer.revisions.contains_key(id))
            .collect();
        if let Some(full) = packer.add_together(together) {
            self.put(full, &mem::take(filling), transfer)?;
        }
        filling.extend(revisions);
        Ok(())
    }

    /// Sends `pack`, whose revisions are `revisions`, on condition that their
    /// changes have the heads `transfer` expects.
    fn put(
        &self,
        pack: Pack,
        revisions: &[ObjectId],
        transfer: &mut Transfer,
    ) -> Result<(), SyncError> {
        let query = PutQuery {
            expect: List(transfer.expect(revisions)),
        };
        self.remote.push(pack, &query)?;
        Ok(())
    }

    /// Checks that the user may make the changes `revision` makes (see
    /// [`Effects`]).
    fn check_writes(&self, revision: &Revision) -> Result<(), SyncError> {
        match Effects::new(self.store, self.store, &self.writes).check(revision)? {
            Some(forbidden) => Err(forbidden.into()),
            None => Ok(()),
        }
    }

    /// Returns `changes` and every change below their revisions, following
    /// their bases and dependencies.
    fn stack(&self, index: &Index, changes: &[ChangeId]) -> Result<Vec<ChangeId>, SyncError> {
        let mut stack = Vec::new();
        let mut seen = HashSet::new();
        let mut next = changes.to_vec();
        while let Some(change) = next.pop() {
            if !seen.insert(change) {
                continue;
            }
            let revisions = index.revisions(change);
            if revisions.is_empty() {
                return Err(SyncError::UnknownChange(change));
            }
            stack.push(change);
            for &id in revisions {
                let revision = self.store.get(id)?;
                let base = match revision.base {
                    Base::Revision(base) => Some(base),
                    Base::Acceptance(_) => None,
                };
                for below in base.into_iter().chain(revision.deps) {
                    if self.store.contains(below.into())? {
                        next.push(self.store.get(below)?.change);
                    }
                }
            }
        }
        Ok(stack)
    }

    /// Checks that the remote has `base`, the acceptance the revision `id` is
    /// based on.
    fn check_shared(
        &self,
        id: RevisionId,
        revision: &Revision,
        base: AcceptanceId,
    ) -> Result<(), SyncError> {
        let body = self.store.get(base)?.body().clone();
        let unshared = |target| SyncError::Unshared {
            change: revision.change,
            revision: id,
            target,
            seq: body.seq,
        };
        let Some(target) = self.store.target_name(body.target)? else {
            return Err(unshared(None));
        };
        let query = LogQuery {
            target: target.clone(),
            since: body.seq,
            limit: Some(1),
        };
        match self.remote.log(&query) {
            Ok(entries) if entries.first().is_some_and(|entry| entry.id == base) => Ok(()),
            Ok(_) => Err(unshared(Some(target))),
            Err(error) if error.code() == Some(ErrorCode::NotFound) => Err(unshared(Some(target))),
            Err(error) => Err(error.into()),
        }
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
    /// A snapshot was needed of a record that has none.
    #[error("{0} has no snapshot")]
    NoSnapshot(RecordId),
    /// A push would change something the user may not.
    #[error("the remote would refuse the push, as of your rights when you last fetched")]
    Forbidden(#[from] Forbidden),
    /// The change has no revision to push.
    #[error("change {} has no revisions", .0.short())]
    UnknownChange(ChangeId),
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
    /// The remote has a revision of a change that this node lacks: the change
    /// moved on the remote.
    #[error(
        "change {} is stale: the remote has revision {} of it, which this repository doesn't. \
         Run `origin fetch`, then `origin converge {}`",
        change.short(),
        head.short(),
        change.short()
    )]
    Stale {
        /// The change.
        change: ChangeId,
        /// The remote's head that this node lacks.
        head: RevisionId,
    },
    /// A revision to push is based on an acceptance the remote doesn't have.
    #[error(
        "revision {} of change {} is based on {} #{seq}, which the remote doesn't have: \
         start the change on a target the remote has, with `origin new --on`",
        revision.short(),
        change.short(),
        target.as_ref().map_or_else(|| "a target".to_owned(), |target| format!("`{target}`"))
    )]
    Unshared {
        /// The change.
        change: ChangeId,
        /// The revision.
        revision: RevisionId,
        /// The base's target, if this node knows its name.
        target: Option<TargetName>,
        /// The base's position on its chain.
        seq: u64,
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
