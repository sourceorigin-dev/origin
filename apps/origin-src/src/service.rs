//! Provides `OriginService`, the one API over a repository. The HTTP routes
//! translate requests into its methods, and the command line calls them
//! in-process, so a local repository and a remote one answer the same way.
//! Plugins use it in the same two ways.
//!
//! A service lives as long as its process; its calls run concurrently, up to
//! [`OriginService::MAX_INDEXES`] (see "The landing queue" in `docs/design.md`).
//!
//! Every method takes the [`Caller`], and does only what their [`Rights`] under
//! the policy in force allow, as [`permissions`](crate::permissions) explains:
//! it filters what they read, and checks what they write.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::iter;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use origin_platform::Native;
use origin_shared::access::{PathAccess, Rights, Token};
use origin_shared::api::{
    Annotation, At, BrowseQuery, ErrorCode, EventsQuery, Expected, FetchSnapshot, Inventory,
    InventoryRequest, IssuedToken, LandRequest, Landings, Listed, Listing, LogEntry, LogQuery,
    NewTarget, RevisionEntry, Revisions, Stored, TargetInfo, TokenRequest,
};
use origin_shared::event::{EventKind, Filter, Page};
use origin_shared::landing::{Pending, Proposal};
use origin_shared::object::DecodeError;
use origin_shared::pack::{Pack, Packer, Part};
use origin_shared::{
    AcceptanceId, Base, ChangeId, EncodedObject, Head, Kind, NodeKey, Object, ObjectId, ObjectKind,
    Principal, PrincipalName, RecordId, RepoPath, Statement, StatementId, TargetName, TreeEntry,
    TreeId,
};
use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::annotations::{AnnotateError, Annotations};
use crate::events::{Events, Wakeup};
use crate::index::Index;
pub use crate::permissions::Caller;
use crate::permissions::{Batch, Forbidden, PutError, Reader, Writer};
use crate::policy::{Policy, PolicyError};
use crate::queue::{LandError, Queue};
use crate::store::{Store, StoreError};
use crate::tree::Snapshots;

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
    /// The policy in force, and the acceptance of `policy` that holds it, once
    /// read.
    policy: Mutex<Option<(AcceptanceId, Arc<Policy>)>>,
    wakeup: Wakeup,
    /// The most bytes a fetch's answer holds: [`Pack::LIMIT`], except in tests.
    pack_limit: usize,
}

impl OriginService {
    /// The most events a page holds, which is also the default.
    const MAX_EVENTS: u32 = 1024;

    /// The longest an issued token lasts.
    pub const MAX_TOKEN_TTL: Duration = Duration::from_hours(7 * 24);

    /// The longest an event long-poll waits.
    pub const MAX_WAIT: Duration = Duration::from_mins(1);

    /// The most indexes open at once, in use or kept free for calls, but for one
    /// more that a call holding the push lock may open. Each holds everything in
    /// memory, so past it calls wait for one. Landings on one target still run one
    /// at a time, under the target's queue lock.
    pub const MAX_INDEXES: usize = 8;

    /// Creates the service of the repository in `store`, served by `node`.
    #[must_use]
    pub fn new(store: Store, node: Node) -> Self {
        Self {
            store,
            node,
            indexes: Indexes::default(),
            policy: Mutex::default(),
            wakeup: Wakeup::default(),
            pack_limit: Pack::LIMIT,
        }
    }

    /// Returns the service, answering fetches in parts of at most `limit` bytes.
    #[cfg(test)]
    pub(crate) fn with_pack_limit(self, limit: usize) -> Self {
        Self {
            pack_limit: limit,
            ..self
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
        self.annotations_up_to(Self::MAX_INDEXES, call)
    }

    /// Runs `call` like [`OriginService::annotations`], for a call holding the push
    /// lock ([`Store::lock_pushes`]): it may open one index past the cap instead of
    /// waiting, since landings wait for that lock while holding indexes of their own.
    fn annotations_pushing<'s, T>(
        &'s self,
        call: impl FnOnce(&mut Annotations<'s>) -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        self.annotations_up_to(Self::MAX_INDEXES + 1, call)
    }

    /// Runs `call` through one of at most `most` open indexes.
    fn annotations_up_to<'s, T>(
        &'s self,
        most: usize,
        call: impl FnOnce(&mut Annotations<'s>) -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        let mut taken = self.indexes.take(most);
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

    /// Returns the policy in force, reading it again only after `policy` moved.
    fn policy(&self) -> Result<Arc<Policy>, ServiceError> {
        let mut kept = self.policy.lock();
        let head = self.store.head(&TargetName::POLICY)?;
        if let (Some(head), Some((at, policy))) = (head, &*kept)
            && head.acceptance == *at
        {
            return Ok(Arc::clone(policy));
        }
        let (at, policy) = match Policy::in_force(&self.store) {
            Ok((at, policy)) => (at, Arc::new(policy)),
            Err(PolicyError::Missing) => return Ok(Arc::new(Policy::default())),
            Err(error) => return Err(error.into()),
        };
        *kept = Some((at, Arc::clone(&policy)));
        Ok(policy)
    }

    /// Returns what `caller` may do under the policy in force.
    ///
    /// # Errors
    ///
    /// Fails if the policy cannot be read.
    pub fn rights(&self, caller: &Caller) -> Result<Rights, ServiceError> {
        Ok(match caller {
            Caller::Owner => Rights::all(),
            Caller::Anonymous => self.policy()?.rights(None),
            Caller::Principal { principal, scope } => {
                let rights = self.policy()?.rights(Some(principal));
                match scope {
                    Some(scope) => rights.narrowed(scope.capabilities.clone()),
                    None => rights,
                }
            }
        })
    }

    /// Returns whether the caller sees the repository at all, which they do if they
    /// may read some path. A caller who doesn't sees none of its records, targets
    /// or events.
    ///
    /// # Errors
    ///
    /// Fails if the policy cannot be read.
    pub fn sees_repository(&self, caller: &Caller) -> Result<bool, ServiceError> {
        Ok(self.rights(caller)?.paths.reads_under(&RepoPath::root()))
    }

    /// `whoami`: returns who the caller is, and what they may do.
    ///
    /// # Errors
    ///
    /// Fails if the policy cannot be read.
    pub fn whoami(&self, caller: &Caller) -> Result<Rights, ServiceError> {
        self.rights(caller)
    }

    /// Issues a token acting for the caller, with the request's scope, which their
    /// rights must cover. It lasts for the request's `ttl`, at most
    /// [`OriginService::MAX_TOKEN_TTL`], and never past the expiry of the caller's
    /// own token if that is a scoped one, so no token can renew itself.
    ///
    /// # Errors
    ///
    /// Fails if the caller isn't signed in, or the scope goes beyond their rights.
    pub fn issue_token(
        &self,
        caller: &Caller,
        request: &TokenRequest,
    ) -> Result<IssuedToken, ServiceError> {
        let Some(principal) = caller.principal() else {
            return Err(Forbidden::NoPrincipal.into());
        };
        let rights = self.rights(caller)?;
        if let Some(beyond) = request
            .scope
            .iter()
            .find(|&capability| !rights.cover(std::slice::from_ref(capability)))
        {
            return Err(Forbidden::Scope(beyond.clone()).into());
        }
        let mut token = Token::new(
            principal.clone(),
            request.scope.clone(),
            request.ttl.duration().min(Self::MAX_TOKEN_TTL),
        );
        if let Caller::Principal {
            scope: Some(scope), ..
        } = caller
        {
            token.expires = token.expires.min(scope.expires);
        }
        Ok(IssuedToken {
            token: token.sign(&self.node.key),
            principal: token.principal,
            expires: token.expires,
        })
    }

    /// Returns the records `ids` name, in that order, as a pack: the revisions,
    /// statements and acceptances that the caller sees, each followed by the
    /// attachment it carries. Snapshots are fetched with [`OriginService::snapshot`].
    /// It stops short at [`Pack::LIMIT`], before a record that doesn't fit with its
    /// attachment: ask again for the ids after the last record it holds.
    ///
    /// # Errors
    ///
    /// Fails if the repository doesn't have a record, if one is a blob, a tree or
    /// an attachment, or if a record cannot be read.
    pub fn records(&self, caller: &Caller, ids: &[ObjectId]) -> Result<Part, ServiceError> {
        let rights = self.rights(caller)?;
        let mut reader = Reader::new(&self.store, &rights.paths);
        let mut packer = Packer::new(self.pack_limit);
        for &id in ids {
            let object = self.store.read(id)?;
            let (record, carried) = match object
                .decode()
                .map_err(|source| StoreError::Decode { id, source })?
            {
                Object::Blob(_) | Object::Tree(_) | Object::Attachment(_) => {
                    return Err(ServiceError::NotARecord(id));
                }
                Object::Revision(revision) => {
                    (RecordId::Revision(id.into()), Some(revision.description))
                }
                Object::Statement(statement) => (RecordId::Statement(id.into()), statement.payload),
                Object::Acceptance(_) => (RecordId::Acceptance(id.into()), None),
            };
            if !reader.sees(record)? {
                continue;
            }
            let carried = carried
                .map(|id| self.store.try_read(id.into()))
                .transpose()?
                .flatten()
                .filter(|object| object.kind().ok() == Some(ObjectKind::Attachment));
            let group: Vec<_> = iter::once(object).chain(carried).collect();
            if !packer.fits(&group) {
                return Ok(Part {
                    pack: packer.finish(),
                    more: true,
                });
            }
            packer.extend(group);
        }
        Ok(Part {
            pack: packer.finish(),
            more: false,
        })
    }

    /// Returns the snapshot of a record the caller sees, if it is a revision or an
    /// acceptance.
    fn snapshot_of(
        &self,
        reader: &mut Reader<'_, Native>,
        record: RecordId,
    ) -> Result<TreeId, ServiceError> {
        let unknown = || ServiceError::UnknownRecord(record);
        let snapshot = match record {
            RecordId::Revision(id) => self.store.get(id).map(|revision| revision.snapshot),
            RecordId::Acceptance(id) => {
                self.store.get(id).map(|acceptance| acceptance.body().state)
            }
            RecordId::Statement(_) | RecordId::Snapshot(_) => {
                return Err(ServiceError::NotASnapshot(record));
            }
        };
        let snapshot = match snapshot {
            Err(StoreError::NotFound(_) | StoreError::Decode { .. }) => return Err(unknown()),
            snapshot => snapshot?,
        };
        if !reader.sees(record)? {
            return Err(unknown());
        }
        Ok(snapshot)
    }

    /// Returns the objects of the snapshot of the record the request names that the
    /// caller lacks, as a pack, parents before children. The caller lacks an object
    /// if none of the snapshots it has holds it at the same path. Only the paths
    /// the request asks for are included, and the first `skip` objects are left out
    /// (see [`Store::added`]). The content of paths the caller may not read is
    /// withheld. The pack stops short at [`Pack::LIMIT`]: to get the rest, ask
    /// again, skipping what came.
    ///
    /// # Errors
    ///
    /// Fails if the record isn't a revision or an acceptance the caller sees, or
    /// the repository cannot read its snapshot.
    pub fn snapshot(&self, caller: &Caller, request: &FetchSnapshot) -> Result<Part, ServiceError> {
        let rights = self.rights(caller)?;
        let snapshot =
            self.snapshot_of(&mut Reader::new(&self.store, &rights.paths), request.of)?;
        let mut have = Vec::new();
        for &id in &request.have {
            let object = self.store.try_read(id.into())?;
            if object.is_some_and(|object| object.kind().ok() == Some(ObjectKind::Tree)) {
                have.push(id);
            }
        }
        let added = self.store.added(
            snapshot,
            &have,
            &request.paths,
            self.filter(&rights, request.of)?,
        )?;
        let skip = usize::try_from(request.skip).unwrap_or(usize::MAX);
        let ids: Vec<ObjectId> = added
            .iter()
            .skip(skip)
            .map(|placed| placed.entry.id)
            .collect();
        let mut packer = Packer::new(self.pack_limit);
        let mut more = false;
        self.store.read_each(&ids, |object| {
            more = !packer.fits([&object]);
            if !more {
                packer.extend([object]);
            }
            Ok::<_, StoreError>(!more)
        })?;
        Ok(Part {
            pack: packer.finish(),
            more,
        })
    }

    /// Returns what a walk of the snapshot of `record` withholds from a caller with
    /// `rights`, or `None` if nothing. Everyone who sees the repository reads the
    /// snapshots of `policy` in full: they hold what each caller may do, and the
    /// keys that sign acceptances, which everyone needs to verify every target.
    fn filter<'r>(
        &self,
        rights: &'r Rights,
        record: RecordId,
    ) -> Result<Option<&'r PathAccess>, ServiceError> {
        if rights.paths.reads_all() {
            return Ok(None);
        }
        if let RecordId::Acceptance(id) = record
            && self.store.get(id)?.body().target == TargetName::POLICY.id()
        {
            return Ok(None);
        }
        Ok(Some(&rights.paths))
    }

    /// Resolves `query` to a directory or file of a snapshot the caller sees.
    /// Returns what the snapshot withholds from the caller, the snapshot, and the
    /// entry at the path.
    fn browse(
        &self,
        caller: &Caller,
        query: &BrowseQuery,
    ) -> Result<(Option<PathAccess>, TreeId, TreeEntry), ServiceError> {
        let rights = self.rights(caller)?;
        let record = match &query.at {
            At::Target(target) => {
                let head = self
                    .store
                    .head(target)?
                    .ok_or_else(|| ServiceError::UnknownTarget(target.clone()))?;
                RecordId::Acceptance(head.acceptance)
            }
            &At::Record(record) => record,
        };
        let snapshot = self.snapshot_of(&mut Reader::new(&self.store, &rights.paths), record)?;
        let withheld = self.filter(&rights, record)?.cloned();
        let entry = Snapshots::new(&self.store)
            .entry_at(snapshot, &query.path)?
            .ok_or_else(|| ServiceError::NoSuchPath(query.path.clone()))?;
        if withheld
            .as_ref()
            .is_some_and(|access| !access.reads_entry(&query.path, &entry))
        {
            return Err(Forbidden::Read(query.path.clone()).into());
        }
        Ok((withheld, snapshot, entry))
    }

    /// Lists a directory of a snapshot: of a target's tip, or of a revision or an
    /// acceptance the caller sees. Entries whose content is withheld from the
    /// caller keep their place, marked as withheld.
    ///
    /// # Errors
    ///
    /// Fails if the target or record doesn't exist or the caller doesn't see it, if
    /// the path isn't a directory, if the caller reads nothing under it, or if a
    /// tree cannot be read.
    pub fn tree(&self, caller: &Caller, query: &BrowseQuery) -> Result<Listing, ServiceError> {
        let (withheld, snapshot, entry) = self.browse(caller, query)?;
        let tree = entry
            .subtree()
            .ok_or_else(|| ServiceError::NotADirectory(query.path.clone()))?;
        let entries = self
            .store
            .get(tree)?
            .iter()
            .map(|(name, &entry)| Listed {
                name: name.clone(),
                kind: entry.kind,
                id: entry.id,
                withheld: withheld
                    .as_ref()
                    .is_some_and(|access| !access.reads_entry(&query.path.join(name), &entry)),
            })
            .collect();
        Ok(Listing {
            snapshot,
            path: query.path.clone(),
            entries,
        })
    }

    /// Returns the content of a file of a snapshot, found the way
    /// [`OriginService::tree`] finds a directory. The caller must be allowed to
    /// read the file.
    ///
    /// # Errors
    ///
    /// Fails like [`OriginService::tree`], or if the path isn't a file.
    pub fn file(&self, caller: &Caller, query: &BrowseQuery) -> Result<Vec<u8>, ServiceError> {
        let (_, _, entry) = self.browse(caller, query)?;
        if entry.kind.is_tree() {
            return Err(ServiceError::NotAFile(query.path.clone()));
        }
        Ok(self
            .store
            .try_blob(entry.id)?
            .ok_or(StoreError::NotFound(entry.id))?)
    }

    /// Returns what the repository holds of each change the caller sees: its
    /// revisions, its heads, and the statements about them, by id. A change it has
    /// never heard of has none.
    ///
    /// # Errors
    ///
    /// Fails if the index or a record cannot be read.
    pub fn inventory(
        &self,
        caller: &Caller,
        request: &InventoryRequest,
    ) -> Result<Vec<Inventory>, ServiceError> {
        let rights = self.rights(caller)?;
        let mut reader = Reader::new(&self.store, &rights.paths);
        self.annotations(|annotations| {
            let index = annotations.index();
            let mut inventories = Vec::new();
            for &change in &request.changes {
                let mut revisions = Vec::new();
                for &id in index.revisions(change) {
                    if reader.sees(RecordId::Revision(id))? {
                        revisions.push(id);
                    }
                }
                let mut heads = index.heads(change);
                heads.retain(|head| revisions.contains(head));
                let mut subjects: Vec<_> =
                    revisions.iter().copied().map(RecordId::Revision).collect();
                let mut statements = Vec::new();
                while let Some(subject) = subjects.pop() {
                    for &statement in index.statements(subject) {
                        statements.push(statement);
                        subjects.push(RecordId::Statement(statement));
                    }
                }
                inventories.push(Inventory {
                    change,
                    heads,
                    revisions,
                    statements,
                });
            }
            Ok(inventories)
        })
    }

    /// Writes the objects of `pack`: snapshots, attachments, revisions and
    /// statements, all of which must be well formed with everything they need in
    /// the repository or the pack, so it never holds a record it cannot use.
    /// Other callers put only what [`permissions`](crate::permissions) allow.
    /// Issues `core.divergent` for each diverging change. See `docs/design.md`.
    ///
    /// # Errors
    ///
    /// Fails if the caller may not put a record, if an object is malformed,
    /// refused or lacks what it needs, or if the objects cannot be written.
    pub fn put_records(&self, caller: &Caller, pack: Pack) -> Result<Stored, ServiceError> {
        self.push(caller, pack, &[])
    }

    /// Puts `pack` like [`OriginService::put_records`], but only if each change in
    /// `expect` still has the heads it names, among those the caller sees, when the
    /// records are published. This is a compare-and-swap that every other
    /// publication of revisions waits for, puts and the landing queue's rebases
    /// alike. So of two pushes made from the same view of a change, or a push and a
    /// rebase, the second is refused instead of making the change diverge.
    ///
    /// # Errors
    ///
    /// Fails like [`OriginService::put_records`], or if a change in `expect` moved,
    /// in which case only the snapshots are written, and no record.
    pub fn push(
        &self,
        caller: &Caller,
        pack: Pack,
        expect: &[Expected],
    ) -> Result<Stored, ServiceError> {
        if *caller == Caller::Anonymous {
            return Err(Forbidden::Anonymous.into());
        }
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
        let mut snapshots = SnapshotLayers::default();
        let mut statements = Vec::new();
        let mut revisions = Vec::new();
        let mut promotions = Vec::new();
        let mut decoded = Vec::new();
        let mut changes = HashSet::new();
        for object in pack.objects {
            let id = object.id();
            let record = object
                .decode()
                .map_err(|source| ServiceError::Malformed { id, source })?;
            for (needed, kind) in Dependencies::of(&record).all() {
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
            Self::check_payload(id, &record, |id| match held.get(&id) {
                Some(&kind) => Ok(kind),
                None if dependencies.depend(id)? => Ok(self
                    .store
                    .try_read(id)?
                    .and_then(|object| object.kind().ok())),
                None => Ok(None),
            })?;
            match record {
                Object::Acceptance(_) => return Err(ServiceError::Acceptance(id.into())),
                Object::Blob(_) | Object::Tree(_) | Object::Attachment(_) => {
                    snapshots.add(object, &record);
                }
                Object::Revision(ref revision) => {
                    if !revision.predecessors.is_empty() {
                        changes.insert(revision.change);
                    }
                    revisions.push(object);
                    decoded.push((id, record));
                }
                Object::Statement(ref statement) if statement.promoted_from.is_some() => {
                    promotions.push((object, statement.clone()));
                    decoded.push((id, record));
                }
                Object::Statement(_) => {
                    statements.push(object);
                    decoded.push((id, record));
                }
            }
        }
        self.check_kinds(&stored)?;
        dependencies.commit()?;
        let mut new = self.store.write_layers(&snapshots.layers())?;
        self.check_writes(caller, &held, &decoded)?;
        // INVARIANT: held until the records are published; see `Store::lock_pushes`.
        let _pushing = if !revisions.is_empty() || !expect.is_empty() {
            let lock = self.store.lock_pushes()?;
            self.check_heads(caller, expect)?;
            Some(lock)
        } else {
            None
        };
        // Each layer durable before the next, as a workspace writes them:
        // statements before the revisions they're about, so a crash never
        // publishes a revision without its `core.conflict`.
        for layer in [statements, revisions] {
            if !layer.is_empty() {
                new += self.store.write_all(&layer)?;
            }
        }
        new += self.put_promotions(promotions)?;
        if !changes.is_empty() {
            self.annotations_pushing(|annotations| {
                for &change in &changes {
                    annotations.flag_divergence(change, self.node.principal())?;
                }
                Ok(())
            })?;
        }
        self.wakeup.notify();
        Ok(Stored { new: new as u64 })
    }

    /// Checks that the caller may put the records of `decoded`, which come in a
    /// batch of the objects `held`. A statement of a reserved kind must come from
    /// whom [`Policy::authorizes`] names, whoever puts it. A caller other than the
    /// owner may only put records that are theirs, as
    /// [`permissions`](crate::permissions) explains.
    fn check_writes(
        &self,
        caller: &Caller,
        held: &HashMap<ObjectId, Option<ObjectKind>>,
        decoded: &[(ObjectId, Object)],
    ) -> Result<(), ServiceError> {
        let policy = self.policy()?;
        let rights = self.rights(caller)?;
        let batch = Batch::new(&self.store, held.keys().copied().collect(), decoded);
        let Some(principal) = caller.principal() else {
            return self.check_records(&batch, decoded, None, |issuer| {
                policy.rights(Some(issuer)).admin
            });
        };
        let writer = Writer::new(&batch, principal, &rights);
        self.check_records(&batch, decoded, Some(writer), |_| rights.admin)
    }

    /// Checks the records of `batch` (`decoded`), as
    /// [`OriginService::check_writes`] describes. Runs `writer`'s checks too if the
    /// caller is signed in. `admin` tells whether a statement's issuer is an admin.
    fn check_records(
        &self,
        batch: &Batch<'_, Native>,
        decoded: &[(ObjectId, Object)],
        mut writer: Option<Writer<'_, Native>>,
        admin: impl Fn(&Principal) -> bool,
    ) -> Result<(), ServiceError> {
        for (id, record) in decoded {
            if self.store.contains(*id)? {
                continue;
            }
            match record {
                Object::Revision(revision) => {
                    if let Some(writer) = &mut writer {
                        writer.revision((*id).into(), revision)?;
                    }
                }
                Object::Statement(statement) => {
                    if !Policy::authorizes_admin(batch, statement, admin(&statement.issuer))? {
                        return Err(ServiceError::Statement {
                            id: (*id).into(),
                            source: Box::new(AnnotateError::Reserved {
                                kind: statement.kind.clone(),
                                issuer: statement.issuer.clone(),
                            }),
                        });
                    }
                    if let Some(writer) = &mut writer {
                        writer.statement(statement)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
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

    /// Checks that the payload `record` (`id`) carries, if any, is an attachment,
    /// as far as `kind` knows the object. The repository may lack a payload, since
    /// it is a detail, except a `core.conflict`'s: reading the conflict needs its
    /// list (see [`Conflicts::listed`](crate::conflict::Conflicts::listed)).
    fn check_payload(
        id: ObjectId,
        record: &Object,
        kind: impl Fn(ObjectId) -> Result<Option<ObjectKind>, StoreError>,
    ) -> Result<(), ServiceError> {
        let Object::Statement(statement) = record else {
            return Ok(());
        };
        let Some(payload) = statement.payload else {
            return Ok(());
        };
        let needed = payload.into();
        match kind(needed)? {
            None if statement.kind == Kind::CONFLICT => Err(ServiceError::Missing { id, needed }),
            Some(kind) if kind != ObjectKind::Attachment => Err(ServiceError::WrongKind {
                id,
                needed,
                kind: ObjectKind::Attachment,
            }),
            _ => Ok(()),
        }
    }

    /// Checks that each change in `expect` has the heads it names, among those the
    /// caller sees.
    fn check_heads(&self, caller: &Caller, expect: &[Expected]) -> Result<(), ServiceError> {
        let rights = self.rights(caller)?;
        let mut reader = Reader::new(&self.store, &rights.paths);
        self.annotations_pushing(|annotations| {
            for expected in expect {
                let mut heads = Vec::new();
                for head in annotations.index().heads(expected.change) {
                    if reader.sees(RecordId::Revision(head))? {
                        heads.push(head);
                    }
                }
                let now: HashSet<_> = heads.iter().collect();
                if now != expected.heads.iter().collect() {
                    return Err(ServiceError::Moved {
                        change: expected.change,
                        heads,
                    });
                }
            }
            Ok(())
        })
    }

    /// Writes the `promotions` that [`OriginService::put_records`] puts, each once
    /// [`Store::claim`] accepts it, and returns how many were new. Their originals
    /// are never promotions, so they are already stored.
    fn put_promotions(
        &self,
        promotions: Vec<(EncodedObject, Statement)>,
    ) -> Result<usize, ServiceError> {
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
        if checked.is_empty() {
            return Ok(0);
        }
        Ok(self.store.write_all(&checked)?)
    }

    /// Returns the statements about `subject`, in the order the repository received
    /// them, or none if the caller doesn't see the subject.
    ///
    /// # Errors
    ///
    /// Fails if the index or a statement cannot be read.
    pub fn statements(
        &self,
        caller: &Caller,
        subject: RecordId,
    ) -> Result<Vec<Annotation>, ServiceError> {
        let rights = self.rights(caller)?;
        if !Reader::new(&self.store, &rights.paths).sees(subject)? {
            return Ok(Vec::new());
        }
        self.annotations(|annotations| Ok(annotations.about(subject)?))
    }

    /// Returns the revisions of `change` that the caller sees, and its heads among
    /// them.
    ///
    /// # Errors
    ///
    /// Fails if the repository has no revision of the change that the caller sees,
    /// or cannot read one.
    pub fn revisions(&self, caller: &Caller, change: ChangeId) -> Result<Revisions, ServiceError> {
        let rights = self.rights(caller)?;
        let mut reader = Reader::new(&self.store, &rights.paths);
        let (mut ids, mut heads) = self.annotations(|annotations| {
            let index = annotations.index();
            Ok((index.revisions(change).to_vec(), index.heads(change)))
        })?;
        let mut seen = Vec::with_capacity(ids.len());
        for id in ids.drain(..) {
            if reader.sees(RecordId::Revision(id))? {
                seen.push(id);
            }
        }
        heads.retain(|head| seen.contains(head));
        if seen.is_empty() {
            return Err(ServiceError::UnknownChange(change));
        }
        let revisions = seen
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
    /// the policy in force. Lists none if the caller doesn't see the repository.
    ///
    /// # Errors
    ///
    /// Fails if the targets or the policy cannot be read.
    pub fn targets(&self, caller: &Caller) -> Result<Vec<TargetInfo>, ServiceError> {
        if !self.sees_repository(caller)? {
            return Ok(Vec::new());
        }
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

    /// Returns the acceptances of the query's target from position `since` on,
    /// oldest first, up to its `limit`.
    ///
    /// # Errors
    ///
    /// Fails if the target doesn't exist, if the caller doesn't see the repository,
    /// or if its log or an acceptance cannot be read.
    pub fn log(&self, caller: &Caller, query: &LogQuery) -> Result<Vec<LogEntry>, ServiceError> {
        let target = &query.target;
        if !self.sees_repository(caller)? || self.store.head(target)?.is_none() {
            return Err(ServiceError::UnknownTarget(target.clone()));
        }
        self.store
            .log_since(target, query.since, query.limit)?
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

    /// Returns the target the stack of `change` is on: the target of its head's
    /// stack.
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
        Ok(self.store.stack_target(head)?)
    }

    /// Lists every landing waiting on every target, and why it waits, keeping those
    /// about what the caller sees.
    ///
    /// # Errors
    ///
    /// Fails if the queues or a record cannot be read.
    pub fn landings(&self, caller: &Caller) -> Result<Vec<Pending>, ServiceError> {
        let rights = self.rights(caller)?;
        let mut reader = Reader::new(&self.store, &rights.paths);
        let mut seen = Vec::new();
        for pending in Queue::pending(&self.store)? {
            if reader.sees(pending.subject)? {
                seen.push(pending);
            }
        }
        Ok(seen)
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

    /// Returns the events after the query's cursor that pass its filter and are
    /// about what the caller sees. Waits up to the query's `wait`, at most
    /// [`OriginService::MAX_WAIT`], for some to arrive. Without a cursor, returns
    /// only events from now on.
    ///
    /// # Errors
    ///
    /// Fails if the op log no longer keeps the events after the cursor, or belongs
    /// to another generation than the query names, or if the op log or a record
    /// cannot be read.
    pub fn events(&self, caller: &Caller, query: &EventsQuery) -> Result<Page, ServiceError> {
        let generation = self.store.oplog().generation()?;
        if let Some(theirs) = &query.generation
            && *theirs != generation
        {
            return Err(ServiceError::OtherLog(theirs.clone()));
        }
        let rights = self.rights(caller)?;
        let mut reader = Reader::new(&self.store, &rights.paths);
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
        let accept = |event: &EventKind| Ok(filter.matches(event) && reader.sees_event(event)?);
        let page = events.wait(cursor, accept, limit, wait.min(Self::MAX_WAIT))?;
        Ok(Page { generation, ..page })
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
/// even if the call panicked, but keeps no more than
/// [`OriginService::MAX_INDEXES`] open: the one past the cap is dropped.
struct Taken<'a> {
    indexes: &'a Indexes,
    index: Option<Index>,
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        let mut state = self.indexes.state();
        state.taken -= 1;
        if state.kept.len() + state.taken < OriginService::MAX_INDEXES {
            state.kept.extend(self.index.take());
        }
        self.indexes.returned.notify_one();
    }
}

/// The blobs, trees and attachments of a pack, which [`OriginService::push`]
/// writes in layers: blobs and attachments first, then each tree after the
/// trees it holds that the pack has. So neither a crash nor a failed write
/// leaves a tree in the repository without its contents, which a later pack
/// needing the tree relies on.
#[derive(Debug, Default)]
struct SnapshotLayers {
    objects: Vec<EncodedObject>,
    /// The subtrees of each tree in `objects`.
    subtrees: HashMap<ObjectId, Vec<ObjectId>>,
}

impl SnapshotLayers {
    /// Adds `object`, which decodes as `record`.
    fn add(&mut self, object: EncodedObject, record: &Object) {
        if let Object::Tree(tree) = record {
            let subtrees = tree
                .iter()
                .filter_map(|(_, entry)| entry.subtree())
                .map(ObjectId::from)
                .collect();
            self.subtrees.insert(object.id(), subtrees);
        }
        self.objects.push(object);
    }

    /// Returns the objects by layer. Blobs and attachments are in the first layer;
    /// a tree is one layer past the highest of its subtrees in the pack.
    fn layers(self) -> Vec<Vec<EncodedObject>> {
        let mut heights: HashMap<ObjectId, usize> = HashMap::new();
        let mut stack: Vec<ObjectId> = self.subtrees.keys().copied().collect();
        while let Some(&id) = stack.last() {
            if heights.contains_key(&id) {
                stack.pop();
                continue;
            }
            let subtrees = &self.subtrees[&id];
            let before = stack.len();
            stack.extend(subtrees.iter().filter(|&subtree| {
                self.subtrees.contains_key(subtree) && !heights.contains_key(subtree)
            }));
            if stack.len() == before {
                let highest = subtrees
                    .iter()
                    .filter_map(|subtree| heights.get(subtree))
                    .max();
                heights.insert(id, highest.map_or(1, |height| height + 1));
                stack.pop();
            }
        }
        let mut layers: Vec<Vec<_>> = iter::repeat_with(Vec::new)
            .take(heights.values().max().map_or(1, |height| height + 1))
            .collect();
        for object in self.objects {
            layers[heights.get(&object.id()).copied().unwrap_or(0)].push(object);
        }
        layers
    }
}

/// What an object needs the repository to have to be usable, as
/// [`OriginService::put_records`] lists, and the kind each must be: the records
/// it refers to, and its content (trees and blobs). A revision's predecessors
/// and a statement's payload are not needed: they are history and detail, and
/// retention may have dropped a payload. An acceptance needs nothing here: its
/// chain brings what it refers to.
#[derive(Debug, Default)]
pub(crate) struct Dependencies {
    /// Revisions, statements and acceptances.
    pub records: Vec<(ObjectId, ObjectKind)>,
    /// Trees, blobs and attachments.
    pub content: Vec<(ObjectId, ObjectKind)>,
}

impl Dependencies {
    pub(crate) fn of(object: &Object) -> Self {
        match object {
            Object::Blob(_) | Object::Attachment(_) | Object::Acceptance(_) => Self::default(),
            Object::Tree(tree) => Self {
                records: Vec::new(),
                content: tree
                    .iter()
                    .map(|(_, entry)| (entry.id, entry.kind.object_kind()))
                    .collect(),
            },
            Object::Revision(revision) => {
                let base = match revision.base {
                    Base::Acceptance(id) => (id.into(), ObjectKind::Acceptance),
                    Base::Revision(id) => (id.into(), ObjectKind::Revision),
                };
                let mut records = vec![base];
                records.extend(
                    revision
                        .deps
                        .iter()
                        .map(|&id| (id.into(), ObjectKind::Revision)),
                );
                Self {
                    records,
                    content: vec![
                        (revision.snapshot.into(), ObjectKind::Tree),
                        (revision.description.into(), ObjectKind::Attachment),
                    ],
                }
            }
            Object::Statement(statement) => Self::of_statement(statement),
        }
    }

    /// Returns what a statement needs: its subject, and the statements it
    /// supersedes or promotes.
    pub(crate) fn of_statement(statement: &Statement) -> Self {
        let mut dependencies = Self::default();
        let subject = (statement.subject.into(), statement.subject.kind());
        match statement.subject {
            RecordId::Snapshot(_) => dependencies.content.push(subject),
            _ => dependencies.records.push(subject),
        }
        dependencies.records.extend(
            [statement.supersedes, statement.promoted_from]
                .into_iter()
                .flatten()
                .map(|id| (id.into(), ObjectKind::Statement)),
        );
        dependencies
    }

    /// Returns every object needed, and the kind it must be.
    pub(crate) fn all(&self) -> impl Iterator<Item = (ObjectId, ObjectKind)> {
        self.records.iter().chain(&self.content).copied()
    }
}

/// A failed service call.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The server hosts no repository by that name.
    #[error("repository `{0}` not found")]
    UnknownRepository(origin_shared::api::RepoName),
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
    /// The repository has no such snapshot.
    #[error("snapshot {0} not found")]
    UnknownSnapshot(TreeId),
    /// A blob, a tree or an attachment was fetched as a record.
    #[error("{0} is not a record: fetch snapshots by walking them")]
    NotARecord(ObjectId),
    /// The repository has no such record, or the caller doesn't see it.
    #[error("record {0} not found")]
    UnknownRecord(RecordId),
    /// A snapshot was asked of a record that has none.
    #[error("{0} has no snapshot: name a revision or an acceptance")]
    NotASnapshot(RecordId),
    /// A snapshot has nothing at a path.
    #[error("nothing is at `{0}`")]
    NoSuchPath(RepoPath),
    /// A path to list is a file.
    #[error("`{0}` is not a directory")]
    NotADirectory(RepoPath),
    /// A path to read is a directory.
    #[error("`{0}` is not a file")]
    NotAFile(RepoPath),
    /// A put object refers to another as the wrong kind: a snapshot that isn't a
    /// tree, a file that is one, or a description that isn't an attachment.
    #[error("object {id} refers to {needed} as a {kind}, which it is not")]
    WrongKind {
        /// The object put.
        id: ObjectId,
        /// The object it refers to.
        needed: ObjectId,
        /// The kind it must be.
        kind: ObjectKind,
    },
    /// A push expected a change's heads to be where they no longer are.
    #[error(
        "change {} moved on the remote since it was fetched: run `origin fetch`, then \
         `origin converge {}` if it diverged",
        change.short(),
        change.short()
    )]
    Moved {
        /// The change.
        change: ChangeId,
        /// The change's heads now.
        heads: Vec<origin_shared::RevisionId>,
    },
    /// The caller may not do this.
    #[error(transparent)]
    Forbidden(#[from] Forbidden),
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
    /// An events cursor belongs to another op log than the repository's.
    #[error("the cursor is of another op log, generation {0}: read from the end")]
    OtherLog(String),
    /// A put statement the repository doesn't accept: a promotion that doesn't
    /// match its original, or a statement of a reserved kind from an issuer who may
    /// not issue it.
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

impl From<PutError> for ServiceError {
    fn from(error: PutError) -> Self {
        match error {
            PutError::Forbidden(error) => Self::Forbidden(error),
            PutError::Unseen(record) => Self::UnknownRecord(record),
            PutError::Store(error) => Self::Store(error),
        }
    }
}

impl ServiceError {
    /// Returns the kind of failure, which clients use to tell failures apart.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::UnknownRepository(_)
            | Self::UnknownChange(_)
            | Self::UnknownTarget(_)
            | Self::UnknownSnapshot(_)
            | Self::UnknownRecord(_)
            | Self::NoSuchPath(_) => ErrorCode::NotFound,
            Self::NoTarget
            | Self::NotARecord(_)
            | Self::NotASnapshot(_)
            | Self::NotADirectory(_)
            | Self::NotAFile(_)
            | Self::WrongKind { .. }
            | Self::Malformed { .. }
            | Self::Acceptance(_)
            | Self::Missing { .. } => ErrorCode::BadRequest,
            Self::Statement { source, .. } => match &**source {
                AnnotateError::Store(error) => Self::store_code(error),
                AnnotateError::Reserved { .. } => ErrorCode::Forbidden,
                _ => ErrorCode::BadRequest,
            },
            Self::Forbidden(_) => ErrorCode::Forbidden,
            Self::Moved { .. } => ErrorCode::Conflict,
            Self::OtherLog(_) => ErrorCode::Expired,
            Self::Land(error) => match error {
                LandError::UnknownTarget(_) | LandError::UnknownChange(_) => ErrorCode::NotFound,
                LandError::TargetExists(_)
                | LandError::SameTarget(_)
                | LandError::NoAuthority(_)
                | LandError::NotAuthority { .. }
                | LandError::WrongKey(_)
                | LandError::Divergent { .. }
                | LandError::Moved(_)
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
    use std::sync::Barrier;
    use std::thread;

    use origin_shared::{EncodedObject, EntryKind, Record, RepoPath, Tree};

    use super::*;

    /// Landings hold indexes while they wait for the push lock, so a call holding
    /// it gets one even when every other index is taken.
    #[test]
    fn a_call_holding_the_push_lock_never_waits_for_an_index() {
        let repo = crate::queue::tests::Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let node = Node {
            name: "queue".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[7; 32])),
        };
        let service = OriginService::new(repo.store.clone(), node);
        let held = Barrier::new(OriginService::MAX_INDEXES + 1);
        let released = Barrier::new(OriginService::MAX_INDEXES + 1);
        thread::scope(|scope| {
            for _ in 0..OriginService::MAX_INDEXES {
                scope.spawn(|| {
                    service.annotations(|_| {
                        held.wait();
                        released.wait();
                        Ok(())
                    })
                });
            }
            held.wait();
            service.annotations_pushing(|_| Ok(())).unwrap();
            released.wait();
        });
        assert!(service.indexes.state().kept.len() <= OriginService::MAX_INDEXES);
        thread::scope(|scope| {
            for _ in 0..OriginService::MAX_INDEXES {
                scope.spawn(|| {
                    service.annotations(|_| {
                        held.wait();
                        released.wait();
                        Ok(())
                    })
                });
            }
            held.wait();
            service.annotations_pushing(|_| Ok(())).unwrap();
            released.wait();
        });
    }

    /// Returns a service of `repo`, and the objects that put a revision with a
    /// conflict: its description, the conflict's payload, the revision and the
    /// conflict.
    fn conflicted(repo: &crate::queue::tests::Repo) -> (OriginService, [EncodedObject; 4]) {
        let node = Node {
            name: "queue".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[7; 32])),
        };
        let service = OriginService::new(repo.store.clone(), node);
        let description = EncodedObject::attachment(b"");
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
        (service, [description, payload, revision, conflict.encode()])
    }

    #[test]
    fn pushes_publish_statements_before_the_revisions_they_are_about() {
        let repo = crate::queue::tests::Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let (service, objects) = conflicted(&repo);
        let [_, _, revision, conflict] = objects.clone();
        let subject = RecordId::Revision(revision.id().into());
        let oplog = repo.store.oplog();
        let before = oplog.last_seq().unwrap();
        let objects = objects.to_vec();
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
    fn conflicts_come_with_the_list_they_carry() {
        let repo = crate::queue::tests::Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
        let (service, [description, payload, revision, conflict]) = conflicted(&repo);
        let without = vec![description, revision, conflict.clone()];
        let refused = service.put_records(
            &Caller::Owner,
            Pack {
                objects: without.clone(),
            },
        );
        assert!(
            matches!(refused, Err(ServiceError::Missing { id, needed })
                if id == conflict.id() && needed == payload.id()),
            "{refused:?}"
        );
        assert!(!repo.store.contains(conflict.id()).unwrap());
        repo.store.write_all(&[payload]).unwrap();
        service
            .put_records(&Caller::Owner, Pack { objects: without })
            .unwrap();
        assert!(repo.store.contains(conflict.id()).unwrap());
    }

    #[test]
    fn trees_are_written_after_the_trees_they_hold() {
        let tree = |entries: &[(&str, EntryKind, ObjectId)]| {
            let mut tree = Tree::default();
            for &(name, kind, id) in entries {
                tree.insert(name.parse().unwrap(), TreeEntry { kind, id });
            }
            tree.encode()
        };
        let blob = EncodedObject::blob(b"file");
        let leaf = tree(&[("file", EntryKind::File, blob.id())]);
        let middle = tree(&[("leaf", EntryKind::Tree, leaf.id())]);
        let stored = Tree::default().encode();
        let root = tree(&[
            ("leaf", EntryKind::Tree, leaf.id()),
            ("middle", EntryKind::Tree, middle.id()),
            ("stored", EntryKind::Tree, stored.id()),
        ]);

        let mut snapshots = SnapshotLayers::default();
        for object in [&root, &blob, &middle, &leaf] {
            snapshots.add(object.clone(), &object.decode().unwrap());
        }
        assert_eq!(
            snapshots.layers(),
            [vec![blob], vec![leaf], vec![middle], vec![root]]
        );
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
