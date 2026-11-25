//! Provides `OriginService`, the one API over a repository. The HTTP routes
//! translate requests into its methods, and the command line calls them
//! in-process, so a local repository and a remote one answer the same way.
//! Plugins use it in the same two ways.
//!
//! A service lives as long as its process. It keeps the record index open and
//! catches it up with the op log before each use, and wakes the event
//! long-polls waiting on it whenever it writes. Calls don't wait for each
//! other: each takes one of the open indexes, or opens a new one, up to
//! [`OriginService::MAX_INDEXES`]; past that, calls wait for one, since each
//! index holds everything in memory. Landings on one target still run one at a
//! time, under the target's queue lock.
//!
//! Every method takes the [`Caller`], but nothing checks it yet: every caller
//! may read and write everything.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use origin_platform::Native;
use origin_shared::api::{
    Annotation, ErrorCode, EventsQuery, LandRequest, Landings, LogEntry, NewTarget, RevisionEntry,
    Revisions, Stored, TargetInfo,
};
use origin_shared::event::{Filter, Page};
use origin_shared::landing::{Pending, Proposal};
use origin_shared::object::DecodeError;
use origin_shared::pack::Pack;
use origin_shared::{
    Base, ChangeId, Head, NodeKey, Object, ObjectId, ObjectKind, Principal, PrincipalName,
    RecordId, Statement, StatementId, TargetName,
};
use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::annotations::{AnnotateError, Annotations};
use crate::events::{Events, Wakeup};
use crate::index::Index;
use crate::policy::{Policy, PolicyError};
use crate::queue::{LandError, Queue};
use crate::store::{Store, StoreError};

/// Who is calling the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// The node's own user, through the command line.
    Owner,
    /// A client of a server that sent no credentials.
    Anonymous,
}

/// A node: its name, and the key it signs acceptances with as the authority of
/// the targets its repositories' policies name it for. All repositories on a
/// server share one node.
#[derive(Clone, Debug)]
pub struct Node {
    /// The node's name.
    pub name: PrincipalName,
    /// The node's signing key.
    pub key: Arc<NodeKey>,
}

impl Node {
    /// Returns the node as a principal: the issuer of the statements it makes.
    fn principal(&self) -> Principal {
        Principal::Node(self.name.clone())
    }
}

/// The API over one repository, served by its node.
#[derive(Debug)]
pub struct OriginService {
    store: Store,
    node: Node,
    /// The record indexes kept open for calls to use.
    indexes: Indexes,
    wakeup: Wakeup,
}

impl OriginService {
    /// The most events a page holds, which is also the default.
    const MAX_EVENTS: u32 = 1024;

    /// The longest an event long-poll waits.
    pub const MAX_WAIT: Duration = Duration::from_mins(1);

    /// The most indexes open at once, in use or kept free for calls.
    pub const MAX_INDEXES: usize = 8;

    /// Creates the service of the repository in `store`, served by `node`.
    #[must_use]
    pub fn new(store: Store, node: Node) -> Self {
        Self {
            store,
            node,
            indexes: Indexes::default(),
            wakeup: Wakeup::default(),
        }
    }

    /// Returns the service with `index`, just read, as its open index.
    #[must_use]
    pub fn with_index(self, index: Index) -> Self {
        Self {
            indexes: Indexes::with(index),
            ..self
        }
    }

    /// Creates a repository without a working copy, as a server hosts them, in
    /// `root`, which must not exist or must be empty. The repository gets its store
    /// and two targets: `policy`, holding `policy` as its `policy.toml`, and
    /// `main`.
    ///
    /// # Errors
    ///
    /// Fails if `root` isn't empty, if the policy doesn't make `node` the authority
    /// of both targets, or if the repository cannot be written.
    pub fn create(root: PathBuf, node: Node, policy: &str) -> Result<Self, ServiceError> {
        let not_empty = std::fs::read_dir(&root).is_ok_and(|mut entries| entries.next().is_some());
        if not_empty {
            return Err(ServiceError::NotEmpty(root));
        }
        let store = Store::new(root, Native::default());
        store.init()?;
        Queue::new(&store, &node.key, node.name.clone()).init(policy)?;
        Ok(Self::new(store, node))
    }

    /// Opens the repository without a working copy in `root`, as
    /// [`OriginService::create`] made it.
    ///
    /// # Errors
    ///
    /// Fails if `root` holds no repository, or is on a network file system.
    pub fn open(root: PathBuf, node: Node) -> Result<Self, ServiceError> {
        let store = Store::open_writable(root, Native::default())?;
        if store.head(&TargetName::POLICY)?.is_none() {
            return Err(ServiceError::NotARepository(store.root().to_owned()));
        }
        Ok(Self::new(store, node))
    }

    /// The repository's storage.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Runs `call` on the repository's statements, through one of the open indexes,
    /// caught up first, or a new one if all are in use. Waits while
    /// [`OriginService::MAX_INDEXES`] are open and none is free.
    fn annotations<'s, T>(
        &'s self,
        call: impl FnOnce(&mut Annotations<'s>) -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        let mut taken = self.indexes.take(Self::MAX_INDEXES);
        // A failed refresh drops the index, to read whole next time.
        let index = match taken.index.take() {
            Some(mut index) => {
                index.refresh(&self.store)?;
                index
            }
            None => Index::open(&self.store)?,
        };
        let mut annotations = Annotations::new(&self.store, index);
        let result = call(&mut annotations);
        taken.index = Some(annotations.into_index());
        result
    }

    /// Returns the objects `ids` name, in that order, as a pack.
    ///
    /// # Errors
    ///
    /// Fails if the repository doesn't have one, or cannot read it.
    pub fn records(&self, _caller: &Caller, ids: &[ObjectId]) -> Result<Pack, ServiceError> {
        let objects = ids
            .iter()
            .map(|&id| self.store.read(id))
            .collect::<Result<_, _>>()?;
        Ok(Pack { objects })
    }

    /// Writes the objects of `pack`: snapshots, revisions and statements.
    ///
    /// Each object must be well formed, and everything it needs must be in the
    /// repository or the pack, of the right kind: a tree's entries (trees or blobs,
    /// as each entry says), a revision's snapshot (a tree), description, base and
    /// dependencies, and a statement's subject and the statements it supersedes or
    /// promotes. So the repository never holds a record it cannot use. Acceptances
    /// are refused: only a target's authority writes them, by landing.
    ///
    /// Blobs and trees are made durable before the records that refer to them, and
    /// statements before the revisions they are about. Promotions come last: each
    /// is checked against its original by [`Store::claim`], once the original and
    /// both subjects are stored. Issues `core.divergent` for each change the new
    /// revisions made diverge.
    ///
    /// # Errors
    ///
    /// Fails if an object is malformed or is an acceptance, if it needs an object
    /// that neither the pack nor the repository has or that is of another kind, if
    /// a promotion doesn't match its original (the other records stay stored), or
    /// if the objects cannot be written.
    pub fn put_records(&self, _caller: &Caller, pack: Pack) -> Result<Stored, ServiceError> {
        // Objects this finds stored stay until what refers to them is.
        let _held = self.store.hold_objects()?;
        let held: HashMap<ObjectId, Option<ObjectKind>> = pack
            .objects
            .iter()
            .map(|object| (object.id(), object.kind().ok()))
            .collect();
        // Makes what the repository has of the dependencies durable before
        // anything refers to them: their writers may not have yet.
        let dependencies = self.store.batch();
        // The dependencies the repository has, whose kinds are read once
        // every object is checked otherwise.
        let mut stored: HashMap<ObjectId, (ObjectId, ObjectKind)> = HashMap::new();
        let mut snapshots = Vec::new();
        let mut statements = Vec::new();
        let mut revisions = Vec::new();
        let mut promotions = Vec::new();
        let mut changes = HashSet::new();
        for object in pack.objects {
            let id = object.id();
            let decoded = object
                .decode()
                .map_err(|source| ServiceError::Malformed { id, source })?;
            for (needed, kind) in Dependencies::of(&decoded) {
                match held.get(&needed) {
                    Some(&held) if held != Some(kind) => {
                        return Err(ServiceError::WrongKind { id, needed, kind });
                    }
                    Some(_) => {}
                    // Two objects needing it as different kinds can't both
                    // be right.
                    None if dependencies.depend(needed)? => match stored.entry(needed) {
                        Entry::Occupied(entry) if entry.get().1 != kind => {
                            return Err(ServiceError::WrongKind { id, needed, kind });
                        }
                        Entry::Occupied(_) => {}
                        Entry::Vacant(entry) => {
                            entry.insert((id, kind));
                        }
                    },
                    None => return Err(ServiceError::Missing { id, needed }),
                }
            }
            match decoded {
                Object::Acceptance(_) => return Err(ServiceError::Acceptance(id.into())),
                Object::Blob(_) | Object::Tree(_) => snapshots.push(object),
                Object::Revision(revision) => {
                    if !revision.predecessors.is_empty() {
                        changes.insert(revision.change);
                    }
                    revisions.push(object);
                }
                Object::Statement(statement) if statement.promoted_from.is_some() => {
                    promotions.push((object, statement));
                }
                Object::Statement(_) => statements.push(object),
            }
        }
        self.check_kinds(&stored)?;
        dependencies.commit()?;
        let mut new = 0;
        // Each layer durable before the next, as a workspace writes them:
        // statements before the revisions they're about, so a crash never
        // publishes a revision without its `core.conflict`.
        for layer in [snapshots, statements, revisions] {
            if !layer.is_empty() {
                new += self.store.write_all(&layer)?;
            }
        }
        // An original is never a promotion, so it's stored by now.
        let mut checked = Vec::new();
        for (object, statement) in promotions {
            let id = object.id().into();
            self.store
                .claim(id, statement)?
                .map_err(|invalid| ServiceError::Statement {
                    id,
                    source: Box::new(invalid.into()),
                })?;
            checked.push(object);
        }
        if !checked.is_empty() {
            new += self.store.write_all(&checked)?;
        }
        if !changes.is_empty() {
            self.annotations(|annotations| {
                for &change in &changes {
                    annotations.flag_divergence(change, self.node.principal())?;
                }
                Ok(())
            })?;
        }
        self.wakeup.notify();
        Ok(Stored { new: new as u64 })
    }

    /// Checks that each object of `stored`, which the repository has, is of the
    /// kind the pack object needing it expects. Reads them on every core.
    fn check_kinds(
        &self,
        stored: &HashMap<ObjectId, (ObjectId, ObjectKind)>,
    ) -> Result<(), ServiceError> {
        let stored: Vec<_> = stored.iter().collect();
        let checked = Store::<Native>::on_every_core(&stored, |chunk| {
            for &(&needed, &(id, kind)) in chunk {
                let object = self.store.read(needed)?;
                if object.kind().ok() != Some(kind) {
                    return Err(ServiceError::WrongKind { id, needed, kind });
                }
            }
            Ok(())
        });
        checked.into_iter().collect()
    }

    /// Returns the statements about `subject`, in the order the repository received
    /// them.
    ///
    /// # Errors
    ///
    /// Fails if the index or a statement cannot be read.
    pub fn statements(
        &self,
        _caller: &Caller,
        subject: RecordId,
    ) -> Result<Vec<Annotation>, ServiceError> {
        self.annotations(|annotations| Ok(annotations.about(subject)?))
    }

    /// Returns the revisions of `change`, and its heads.
    ///
    /// # Errors
    ///
    /// Fails if the repository has no revision of the change, or cannot read one.
    pub fn revisions(&self, _caller: &Caller, change: ChangeId) -> Result<Revisions, ServiceError> {
        let (ids, heads) = self.annotations(|annotations| {
            let index = annotations.index();
            Ok((index.revisions(change).to_vec(), index.heads(change)))
        })?;
        if ids.is_empty() {
            return Err(ServiceError::UnknownChange(change));
        }
        let revisions = ids
            .into_iter()
            .map(|id| {
                Ok(RevisionEntry {
                    id,
                    revision: self.store.get(id)?,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(Revisions { revisions, heads })
    }

    /// Lists every target with its tip, and its authority and requirements under
    /// the policy in force.
    ///
    /// # Errors
    ///
    /// Fails if the targets or the policy cannot be read.
    pub fn targets(&self, _caller: &Caller) -> Result<Vec<TargetInfo>, ServiceError> {
        let policy = Policy::load(&self.store)?;
        let mut names = self.store.targets()?;
        names.sort();
        let mut targets = Vec::new();
        for name in names {
            let Some(head) = self.store.head(&name)? else {
                continue;
            };
            targets.push(TargetInfo {
                authority: policy.authority(&name).cloned(),
                requirements: policy.requirements(&name).to_vec(),
                head,
                name,
            });
        }
        Ok(targets)
    }

    /// Returns the acceptances of `target` from position `since` on, oldest first.
    ///
    /// # Errors
    ///
    /// Fails if the target doesn't exist, or if its log or an acceptance cannot be
    /// read.
    pub fn log(
        &self,
        _caller: &Caller,
        target: &TargetName,
        since: u64,
    ) -> Result<Vec<LogEntry>, ServiceError> {
        if self.store.head(target)?.is_none() {
            return Err(ServiceError::UnknownTarget(target.clone()));
        }
        self.store
            .log_since(target, since)?
            .into_iter()
            .map(|id| {
                Ok(LogEntry {
                    id,
                    acceptance: self.store.get(id)?,
                })
            })
            .collect()
    }

    /// Lands what `request` proposes, with this node as the authority: a change,
    /// with the changes below it that the target doesn't have yet, or another
    /// target's tip. By default, a change lands on the target its stack is on.
    ///
    /// # Errors
    ///
    /// Fails if the change or a target doesn't exist, if a target's tip is proposed
    /// without a target to land on, if this node isn't the target's authority, if
    /// the change can't land as it is (see [`LandError`]), or if the records cannot
    /// be read or written.
    pub fn land(&self, _caller: &Caller, request: &LandRequest) -> Result<Landings, ServiceError> {
        let queue = Queue::new(&self.store, &self.node.key, self.node.name.clone());
        let landed = self.annotations(|annotations| {
            let landed = match &request.proposal {
                Proposal::Change(change) => {
                    let onto = match &request.onto {
                        Some(onto) => onto.clone(),
                        None => self.target_of(annotations.index(), *change)?,
                    };
                    let landings = queue.land_change(annotations, *change, &onto)?;
                    Landings { onto, landings }
                }
                Proposal::Target(source) => {
                    let onto = request.onto.clone().ok_or(ServiceError::NoTarget)?;
                    let landing = queue.land_target(annotations, source, &onto)?;
                    Landings {
                        onto,
                        landings: vec![landing],
                    }
                }
            };
            Ok(landed)
        });
        // Even a failed landing may have written records, such as a
        // conflict.
        self.wakeup.notify();
        landed
    }

    /// Returns the target the stack of `change` is on: the target of the acceptance
    /// at the bottom of its head's stack.
    fn target_of(&self, index: &Index, change: ChangeId) -> Result<TargetName, ServiceError> {
        let head = match index.heads(change)[..] {
            [head] => head,
            [] => return Err(ServiceError::UnknownChange(change)),
            ref heads => {
                return Err(LandError::Divergent {
                    change,
                    heads: heads.to_vec(),
                }
                .into());
            }
        };
        let mut base = self.store.get(head)?.base;
        let acceptance = loop {
            match base {
                Base::Revision(id) => base = self.store.get(id)?.base,
                Base::Acceptance(id) => break id,
            }
        };
        let target = self.store.get(acceptance)?.body().target;
        Ok(self
            .store
            .target_name(target)?
            .ok_or(StoreError::NotFound(acceptance.into()))?)
    }

    /// Lists every landing waiting on every target, and why it waits.
    ///
    /// # Errors
    ///
    /// Fails if the queues cannot be read.
    pub fn landings(&self, _caller: &Caller) -> Result<Vec<Pending>, ServiceError> {
        Ok(Queue::pending(&self.store)?)
    }

    /// Creates a target at the tip of another, with this node as its authority.
    ///
    /// # Errors
    ///
    /// Fails if the name is taken, if the other target doesn't exist, if the policy
    /// wouldn't make this node the new target's authority, or if the records cannot
    /// be written.
    pub fn create_target(
        &self,
        _caller: &Caller,
        request: &NewTarget,
    ) -> Result<Head, ServiceError> {
        let queue = Queue::new(&self.store, &self.node.key, self.node.name.clone());
        let head = queue.create_target(&request.name, &request.from)?;
        self.wakeup.notify();
        Ok(head)
    }

    /// Returns the events after the query's cursor that pass its filter. Waits up
    /// to the query's `wait`, at most [`OriginService::MAX_WAIT`], for some to
    /// arrive. Without a cursor, returns only events from now on.
    ///
    /// # Errors
    ///
    /// Fails if the op log no longer keeps the events after the cursor, or if the
    /// op log or a record cannot be read.
    pub fn events(&self, _caller: &Caller, query: &EventsQuery) -> Result<Page, ServiceError> {
        let events = Events::new(&self.store).waking_on(&self.wakeup);
        let cursor = match query.cursor {
            Some(cursor) => cursor,
            None => events.end()?,
        };
        let filter = Filter {
            kinds: query.kind.0.clone(),
            subjects: query.subject.0.clone(),
        };
        let limit = query
            .limit
            .unwrap_or(Self::MAX_EVENTS)
            .min(Self::MAX_EVENTS) as usize;
        let wait = query
            .wait
            .map_or(Duration::ZERO, origin_shared::Period::duration);
        Ok(events.wait(cursor, &filter, limit, wait.min(Self::MAX_WAIT))?)
    }
}

/// The record indexes a service has open: those free for calls to take, and how
/// many calls hold one.
#[derive(Debug, Default)]
struct Indexes {
    state: Mutex<IndexesState>,
    /// Signalled whenever a call gives its index back.
    returned: Condvar,
}

#[derive(Debug, Default)]
struct IndexesState {
    kept: Vec<Index>,
    taken: usize,
}

impl Indexes {
    /// Returns indexes holding `index`, free to take.
    fn with(index: Index) -> Self {
        Self {
            state: Mutex::new(IndexesState {
                kept: vec![index],
                taken: 0,
            }),
            returned: Condvar::new(),
        }
    }

    /// Takes a free index, or a slot to open a new one. Waits while `most` are open
    /// and none is free. The index goes back when the result is dropped.
    fn take(&self, most: usize) -> Taken<'_> {
        let mut state = self.state();
        while state.kept.is_empty() && state.taken >= most {
            self.returned.wait(&mut state);
        }
        state.taken += 1;
        Taken {
            indexes: self,
            index: state.kept.pop(),
        }
    }

    fn state(&self) -> MutexGuard<'_, IndexesState> {
        self.state.lock()
    }
}

/// An index a call took from [`Indexes`], or a slot to open one; a call that
/// opens an index puts it in `index` to keep it. On drop, it gives both back,
/// even if the call panicked.
struct Taken<'a> {
    indexes: &'a Indexes,
    index: Option<Index>,
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        let mut state = self.indexes.state();
        state.taken -= 1;
        state.kept.extend(self.index.take());
        self.indexes.returned.notify_one();
    }
}

/// What an object needs the repository to have to be usable, as
/// [`OriginService::put_records`] lists, and the kind each must be. A
/// revision's predecessors and a statement's payload are not needed: they are
/// history and detail, and retention may have dropped a payload.
struct Dependencies;

impl Dependencies {
    fn of(object: &Object) -> Vec<(ObjectId, ObjectKind)> {
        match object {
            Object::Blob(_) | Object::Acceptance(_) => Vec::new(),
            Object::Tree(tree) => tree
                .iter()
                .map(|(_, entry)| (entry.id, entry.kind.object_kind()))
                .collect(),
            Object::Revision(revision) => {
                let base = match revision.base {
                    Base::Acceptance(id) => (id.into(), ObjectKind::Acceptance),
                    Base::Revision(id) => (id.into(), ObjectKind::Revision),
                };
                let mut needed = vec![
                    (revision.snapshot.into(), ObjectKind::Tree),
                    (revision.description.into(), ObjectKind::Blob),
                    base,
                ];
                needed.extend(
                    revision
                        .deps
                        .iter()
                        .map(|&id| (id.into(), ObjectKind::Revision)),
                );
                needed
            }
            Object::Statement(statement) => Self::of_statement(statement),
        }
    }

    /// Returns what a statement needs: its subject, and the statements it
    /// supersedes or promotes.
    fn of_statement(statement: &Statement) -> Vec<(ObjectId, ObjectKind)> {
        let subject = (statement.subject.into(), statement.subject.kind());
        [Some(subject)]
            .into_iter()
            .chain(
                [statement.supersedes, statement.promoted_from]
                    .map(|id| id.map(|id| (id.into(), ObjectKind::Statement))),
            )
            .flatten()
            .collect()
    }
}

/// A failed service call.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// A directory holds no repository.
    #[error("{} is not an origin repository", .0.display())]
    NotARepository(PathBuf),
    /// `create` was given a directory with files in it.
    #[error("{} is not empty", .0.display())]
    NotEmpty(PathBuf),
    /// No revision of the change is known.
    #[error("change {} has no revisions", .0.short())]
    UnknownChange(ChangeId),
    /// No target has this name.
    #[error("no target is named `{0}`")]
    UnknownTarget(TargetName),
    /// A target's tip was proposed for landing without a target to land on.
    #[error("landing a target needs a target to land on")]
    NoTarget,
    /// A put object isn't well formed.
    #[error("object {id} is malformed")]
    Malformed {
        /// The object.
        id: ObjectId,
        /// What is wrong.
        source: DecodeError,
    },
    /// An acceptance was put.
    #[error(
        "acceptance {0} cannot be put: only a target's authority writes acceptances, by landing"
    )]
    Acceptance(origin_shared::AcceptanceId),
    /// A put object needs one that neither the pack nor the repository has.
    #[error("object {id} needs object {needed}, which neither the batch nor the repository has")]
    Missing {
        /// The object put.
        id: ObjectId,
        /// The object it needs.
        needed: ObjectId,
    },
    /// A put object refers to another as the wrong kind: a snapshot that isn't a
    /// tree, or a file that is one.
    #[error("object {id} refers to {needed} as a {kind}, which it is not")]
    WrongKind {
        /// The object put.
        id: ObjectId,
        /// The object it refers to.
        needed: ObjectId,
        /// The kind it must be.
        kind: ObjectKind,
    },
    /// A put statement the repository doesn't accept: a promotion that doesn't
    /// match its original.
    #[error("statement {} cannot be put", .id.short())]
    Statement {
        /// The statement.
        id: StatementId,
        /// Why it was refused, boxed because it is large and rare.
        source: Box<AnnotateError>,
    },
    /// A landing failed.
    #[error(transparent)]
    Land(#[from] LandError),
    /// The policy cannot be read.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// Records cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl ServiceError {
    /// Returns the kind of failure, which clients use to tell failures apart.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::UnknownChange(_) | Self::UnknownTarget(_) => ErrorCode::NotFound,
            Self::NoTarget
            | Self::Malformed { .. }
            | Self::Acceptance(_)
            | Self::Missing { .. }
            | Self::WrongKind { .. } => ErrorCode::BadRequest,
            Self::Statement { source, .. } => match &**source {
                AnnotateError::Store(error) => Self::store_code(error),
                _ => ErrorCode::BadRequest,
            },
            Self::Land(error) => match error {
                LandError::UnknownTarget(_) | LandError::UnknownChange(_) => ErrorCode::NotFound,
                LandError::TargetExists(_)
                | LandError::SameTarget(_)
                | LandError::NoAuthority(_)
                | LandError::NotAuthority { .. }
                | LandError::WrongKey(_)
                | LandError::Divergent { .. }
                | LandError::Abandoned(_)
                | LandError::Unresolved { .. }
                | LandError::InvalidPolicy(_) => ErrorCode::Conflict,
                LandError::Store(error) => Self::store_code(error),
                LandError::Policy(_) | LandError::Annotate(_) => ErrorCode::Internal,
            },
            Self::Store(error) => Self::store_code(error),
            Self::NotARepository(_) | Self::NotEmpty(_) | Self::Policy(_) => ErrorCode::Internal,
        }
    }

    fn store_code(error: &StoreError) -> ErrorCode {
        match error {
            StoreError::NotFound(_) => ErrorCode::NotFound,
            StoreError::Expired { .. } => ErrorCode::Expired,
            StoreError::Conflict { .. } => ErrorCode::Conflict,
            _ => ErrorCode::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use origin_shared::{EncodedObject, Record, RepoPath};

    use super::*;

    #[test]
    fn pushes_publish_statements_before_the_revisions_they_are_about() {
        let repo = crate::queue::tests::Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let node = Node {
            name: "queue".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[7; 32])),
        };
        let service = OriginService::new(repo.store.clone(), node);
        let description = EncodedObject::blob(b"");
        let revision = origin_shared::Revision {
            change: origin_shared::ChangeId::from_bytes([1; 16]),
            predecessors: Vec::new(),
            base: origin_shared::Base::Acceptance(repo.tip(&TargetName::MAIN).acceptance),
            deps: Vec::new(),
            snapshot: repo.files(&[("a", "<<<<<<<")]),
            description: description.id().into(),
            author: "user:ada".parse().unwrap(),
        }
        .encode();
        let conflicts = crate::conflict::Conflicts::new(vec![crate::conflict::Conflicted {
            path: RepoPath::new("a").unwrap(),
            written: None,
        }]);
        let subject = RecordId::Revision(revision.id().into());
        let (payload, conflict) = conflicts.statement(subject, "user:ada".parse().unwrap(), None);
        let conflict = conflict.encode();
        let oplog = repo.store.oplog();
        let before = oplog.last_seq().unwrap();
        let objects = vec![description, payload, revision, conflict.clone()];
        service
            .put_records(&Caller::Owner, Pack { objects })
            .unwrap();
        // A crash between the two finds the conflict alone, never the
        // revision without it.
        let entries = oplog.after(before).unwrap().entries;
        let written = |record: RecordId| {
            let written = |entry: &&crate::oplog::Entry| match &entry.effect {
                crate::oplog::Effect::Wrote(written) => {
                    written.iter().any(|entry| entry.record() == record)
                }
                _ => false,
            };
            entries.iter().find(written).unwrap().seq
        };
        let conflict = RecordId::Statement(conflict.id().into());
        assert!(written(conflict) < written(subject));
    }

    #[test]
    fn calls_using_the_index_do_not_wait_for_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node {
            name: "hosted".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[7; 32])),
        };
        let policy = Policy::initial(&node.name, &node.key.public());
        let service = OriginService::create(dir.path().join("repo"), node, &policy).unwrap();
        // A landing holds an index all along: a read meanwhile takes another.
        service
            .annotations(|_| service.annotations(|_| Ok(())))
            .unwrap();
        assert_eq!(
            service.indexes.state().kept.len(),
            2,
            "both kept for later calls"
        );
    }
}
