//! Enforces what callers may read and put, as their [`Rights`] say; landings
//! check their changes in the [queue](crate::queue). A caller who reads no
//! path does not see the repository. Otherwise they see the targets,
//! acceptances, revisions that change a path they read or nothing, and
//! statements about what they see; snapshots withhold the other paths
//! ([`Store::added`]). Every record put must be the caller's, of a kind they
//! may annotate, about what they see, with its attachments ([`Batch`]). A
//! revision must extend its change's history, keep its predecessors' effect
//! where its writer may not write, and hold only content the caller supplies
//! ([`Store::unsupplied`]). `docs/design.md` has the rules.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::iter;

use origin_platform::Platform;
use origin_shared::access::{Capability, PathAccess, Rights};
use origin_shared::event::EventKind;
use origin_shared::{
    Base, ChangeId, Kind, Object, ObjectId, Principal, RecordId, RepoPath, Revision, RevisionId,
    Statement, StatementId, TargetName, TreeId,
};

use crate::annotations::{Annotations, Claim, RevisionView};
use crate::conflict::Conflicts;
use crate::store::{Store, StoreError};
use crate::tree::FileChange;

/// Who calls the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// The node's own user, through the command line or a remote on a local
    /// path. The owner may do everything.
    Owner,
    /// A client of a server that sent no credentials.
    Anonymous,
    /// An authenticated client of a server.
    Principal {
        /// Who the client is.
        principal: Principal,
        /// The scope their token narrows their rights to, if the token is scoped.
        scope: Option<Scope>,
    },
}

/// The rights a scoped token narrows its bearer to, and when it expires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    /// The capabilities of its bearer's rights that the token keeps.
    pub capabilities: Vec<Capability>,
    /// When the token expires, in seconds since the Unix epoch. A token issued
    /// by its bearer expires no later.
    pub expires: u64,
}

impl Caller {
    /// The caller's principal, or `None` for the owner or an anonymous caller.
    #[must_use]
    pub fn principal(&self) -> Option<&Principal> {
        match self {
            Self::Principal { principal, .. } => Some(principal),
            Self::Owner | Self::Anonymous => None,
        }
    }
}

/// Decides what a caller sees of a repository's records, caching each
/// decision.
pub struct Reader<'a, P> {
    store: &'a Store<P>,
    /// What the caller may do with each path.
    paths: &'a PathAccess,
    seen: HashMap<RecordId, bool>,
}

impl<'a, P: Platform> Reader<'a, P> {
    /// Creates a reader for a caller with `paths` access to `store`.
    pub fn new(store: &'a Store<P>, paths: &'a PathAccess) -> Self {
        Self {
            store,
            paths,
            seen: HashMap::new(),
        }
    }

    /// Whether the caller sees the repository: true if they read some path.
    #[must_use]
    pub fn sees_repository(&self) -> bool {
        self.paths.reads_under(&RepoPath::root())
    }

    /// Whether the caller sees every record.
    fn sees_all(&self) -> bool {
        self.paths.reads_all()
    }

    /// Whether the caller sees `record`, by the rules in the module docs. A
    /// record this node lacks, or whose base it lacks, is not seen.
    ///
    /// # Errors
    ///
    /// Fails if a record or tree cannot be read.
    pub fn sees(&mut self, record: RecordId) -> Result<bool, StoreError> {
        if self.sees_all() {
            return Ok(true);
        }
        if !self.sees_repository() {
            return Ok(false);
        }
        if let Some(&seen) = self.seen.get(&record) {
            return Ok(seen);
        }
        let seen = match record {
            RecordId::Revision(id) => self.sees_revision(id),
            RecordId::Statement(id) => match self.store.get(id) {
                Ok(statement) => self.sees(statement.subject),
                Err(error) => Err(error),
            },
            RecordId::Acceptance(_) | RecordId::Snapshot(_) => Ok(true),
        };
        let seen = match seen {
            Err(StoreError::NotFound(_)) => false,
            seen => seen?,
        };
        self.seen.insert(record, seen);
        Ok(seen)
    }

    fn sees_revision(&self, id: RevisionId) -> Result<bool, StoreError> {
        let revision: Revision = self.store.get(id)?;
        let changes = self
            .store
            .diff(self.store.base_snapshot(revision.base)?, revision.snapshot)?;
        Ok(changes.is_empty() || changes.iter().any(|change| self.paths.reads(&change.path)))
    }

    /// Whether the caller sees what `event` is about.
    ///
    /// # Errors
    ///
    /// Fails like [`Reader::sees`].
    pub fn sees_event(&mut self, event: &EventKind) -> Result<bool, StoreError> {
        match event {
            EventKind::CheckRequested { subject, .. } => self.sees(*subject),
            event => self.sees(event.record()),
        }
    }
}

/// The records a batch puts. They are read before those of the store the
/// batch is put in, so a record may refer to another record of the same
/// batch.
pub struct Batch<'a, P> {
    store: &'a Store<P>,
    /// Every object in the batch.
    supplied: HashSet<ObjectId>,
    revisions: HashMap<RevisionId, &'a Revision>,
    /// How many revisions of each change in the batch have no predecessors.
    roots: HashMap<ChangeId, usize>,
    statements: HashMap<StatementId, &'a Statement>,
    /// The statements of the batch about each record the store lacks, in
    /// batch order. Statements about a record the store has are left out:
    /// putting them again doesn't change where they were published.
    about: HashMap<RecordId, Vec<&'a Statement>>,
}

impl<'a, P: Platform> Batch<'a, P> {
    /// Creates the batch of objects `supplied`, whose decoded records are
    /// `decoded`, to be put in `store`.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot say which statements it has.
    pub fn new(
        store: &'a Store<P>,
        supplied: HashSet<ObjectId>,
        decoded: &'a [(ObjectId, Object)],
    ) -> Result<Self, StoreError> {
        let mut batch = Self {
            store,
            supplied,
            revisions: HashMap::new(),
            roots: HashMap::new(),
            statements: HashMap::new(),
            about: HashMap::new(),
        };
        for (id, object) in decoded {
            match object {
                Object::Revision(revision) => {
                    batch.revisions.insert((*id).into(), revision);
                    if revision.predecessors.is_empty() {
                        *batch.roots.entry(revision.change).or_default() += 1;
                    }
                }
                Object::Statement(statement) => {
                    batch.statements.insert((*id).into(), statement);
                    if !store.contains(*id)? {
                        let about = batch.about.entry(statement.subject).or_default();
                        about.push(statement);
                    }
                }
                _ => {}
            }
        }
        Ok(batch)
    }

    /// Whether the batch holds `record`.
    fn holds(&self, record: RecordId) -> bool {
        self.supplied.contains(&record.into())
    }

    /// Returns the statement `id`, from the batch or the store.
    fn statement(&self, id: StatementId) -> Result<Statement, StoreError> {
        match self.statements.get(&id) {
            Some(&statement) => Ok(statement.clone()),
            None => self.store.get(id),
        }
    }
}

impl<P: Platform> RevisionView for Batch<'_, P> {
    fn revision(&self, id: RevisionId) -> Result<Revision, StoreError> {
        match self.revisions.get(&id) {
            Some(&revision) => Ok(revision.clone()),
            None => self.store.get(id),
        }
    }
}

/// Checks the records of a batch an authenticated caller puts, by the
/// rules in the module docs.
pub struct Writer<'a, P> {
    batch: &'a Batch<'a, P>,
    /// The store's statements and revisions, as its index knows them.
    annotations: &'a Annotations<'a, P>,
    principal: &'a Principal,
    rights: &'a Rights,
    reader: Reader<'a, P>,
    effects: Effects<'a, P, Batch<'a, P>>,
}

impl<'a, P: Platform> Writer<'a, P> {
    /// Checks `batch` for `principal`, with `rights`, looking up what the
    /// store has through `annotations`.
    #[must_use]
    pub fn new(
        batch: &'a Batch<'a, P>,
        annotations: &'a Annotations<'a, P>,
        principal: &'a Principal,
        rights: &'a Rights,
    ) -> Self {
        Self {
            batch,
            annotations,
            principal,
            rights,
            reader: Reader::new(batch.store, &rights.paths),
            effects: Effects::new(batch.store, batch, &rights.paths),
        }
    }

    /// Whether the caller sees `record`. They see the records the batch adds,
    /// but a record the store already had only if they saw it before.
    fn sees(&mut self, record: RecordId) -> Result<bool, StoreError> {
        let new = self.batch.holds(record) && !self.batch.store.contains(record.into())?;
        Ok(new || self.reader.sees(record)?)
    }

    /// Checks that `attachment`, which a `record` carries, comes in the batch
    /// if the store already has it. Callers who read every path skip this
    /// check.
    fn brings(&self, record: &'static str, attachment: ObjectId) -> Result<(), PutError> {
        if self.rights.paths.reads_all()
            || self.batch.supplied.contains(&attachment)
            || !self.batch.store.contains(attachment)?
        {
            return Ok(());
        }
        Err(Forbidden::UnsuppliedAttachment { record, attachment }.into())
    }

    /// Whether the caller may write every path, in which case effects are not
    /// checked.
    fn writes_all(&self) -> bool {
        self.rights.paths.writes_all()
    }

    /// Checks a statement being put.
    ///
    /// # Errors
    ///
    /// Fails if the caller isn't its issuer, may not issue its kind, or
    /// doesn't see what it is about or the original it promotes. Also fails if
    /// it replaces a `core.conflict` about a revision, by being one or by
    /// superseding one, and doesn't list exactly the conflicts the revision
    /// carries at paths the caller may not write, or if a record cannot be
    /// read.
    pub fn statement(&mut self, statement: &Statement) -> Result<(), PutError> {
        if statement.issuer != *self.principal {
            return Err(Forbidden::NotYours {
                record: "statement",
                by: statement.issuer.clone(),
                caller: self.principal.clone(),
            }
            .into());
        }
        if !statement.kind.is_reserved() && !self.rights.may_annotate(&statement.kind) {
            return Err(Forbidden::Annotate(statement.kind.clone()).into());
        }
        let original = statement.promoted_from.map(RecordId::Statement);
        for record in iter::once(statement.subject).chain(original) {
            if !self.sees(record)? {
                return Err(PutError::Unseen(record));
            }
        }
        if original.is_none()
            && let Some(payload) = statement.payload
        {
            self.brings("statement", payload.into())?;
        }
        if !self.writes_all() {
            self.keeps_listing(statement)?;
        }
        Ok(())
    }

    /// Checks that `statement`, if it replaces its issuer's `core.conflict`
    /// about a revision (by being one or by superseding one), changes nothing
    /// the revision lists at paths the caller may not write. For a revision
    /// the store has, its listing must stay as it is. For a revision of the
    /// batch, it must list exactly the conflicts it carries. A statement only
    /// supersedes its issuer's own statements, so a claim to supersede
    /// someone else's is ignored, and its subject is never read.
    fn keeps_listing(&mut self, statement: &Statement) -> Result<(), PutError> {
        let superseded = match statement.supersedes {
            Some(id) => match self.batch.statement(id) {
                Ok(superseded) if superseded.issuer == statement.issuer => Some((id, superseded)),
                Ok(_) | Err(StoreError::NotFound(_)) => None,
                Err(error) => return Err(error.into()),
            },
            None => None,
        };
        let replaced = iter::once(statement)
            .chain(superseded.as_ref().map(|(_, superseded)| superseded))
            .filter(|replaced| replaced.kind == Kind::CONFLICT);
        for replaced in replaced {
            let subject = replaced.subject;
            let RecordId::Revision(id) = subject else {
                continue;
            };
            if !self.sees(subject)? {
                return Err(PutError::Unseen(subject));
            }
            let revision = self.batch.revision(id)?;
            let store = self.batch.store;
            let about = (statement.subject == subject).then_some(statement);
            if store.contains(id.into())? {
                let claims = self.annotations.claims(subject)?;
                let before = claims.iter().map(Claim::statement);
                let after = claims
                    .iter()
                    .filter(|claim| superseded.as_ref().is_none_or(|(id, _)| claim.id() != *id))
                    .map(Claim::statement)
                    .chain(about);
                let before = Conflicts::latest(store, before, &revision)?;
                self.keeps(&before, &Conflicts::latest(store, after, &revision)?)?;
            } else {
                let listed = Conflicts::latest(store, about, &revision)?;
                self.keeps_conflicts(&revision, &listed)?;
            }
        }
        Ok(())
    }

    /// Checks a revision being put, whose objects are already written.
    ///
    /// # Errors
    ///
    /// Fails if the caller isn't its author; it is one of several roots of a
    /// change in the batch, or evolves from a revision of another change; they
    /// don't see a revision it evolves from; it has an effect they may not
    /// have; it holds content they neither supplied nor share with a snapshot
    /// it refers to; or a tree cannot be read. This doesn't check whether a
    /// root's change already has revisions: [`OriginService::push`] does that
    /// as it publishes them, so two pushes never both give a change a root.
    ///
    /// [`OriginService::push`]: crate::service::OriginService::push
    pub fn revision(&mut self, id: RevisionId, revision: &Revision) -> Result<(), PutError> {
        if revision.author != *self.principal {
            return Err(Forbidden::NotYours {
                record: "revision",
                by: revision.author.clone(),
                caller: self.principal.clone(),
            }
            .into());
        }
        let roots = self.batch.roots.get(&revision.change).copied();
        if revision.predecessors.is_empty() && roots > Some(1) {
            return Err(Forbidden::History(revision.change).into());
        }
        let mut references = vec![self.effects.base_snapshot(revision.base)?];
        for &id in &revision.predecessors {
            if !self.sees(RecordId::Revision(id))? {
                return Err(PutError::Unseen(RecordId::Revision(id)));
            }
            let predecessor = self.batch.revision(id)?;
            if predecessor.change != revision.change {
                return Err(Forbidden::History(revision.change).into());
            }
            references.push(predecessor.snapshot);
        }
        if let Some(forbidden) = self.effects.check(revision)? {
            return Err(forbidden.into());
        }
        if !self.writes_all() {
            let listed = self.conflicts(id, revision)?;
            self.keeps_conflicts(revision, &listed)?;
        }
        self.brings("revision", revision.description.into())?;
        if !self.rights.paths.reads_all() {
            let store = self.batch.store;
            let unsupplied =
                store.unsupplied(revision.snapshot, &references, &self.batch.supplied)?;
            if let Some(path) = unsupplied {
                return Err(Forbidden::Unsupplied { revision: id, path }.into());
            }
        }
        Ok(())
    }

    /// Returns the unresolved conflicts of `revision`, whose id is `id`, as
    /// its author's latest `core.conflict` about it lists them. Statements in
    /// the store come before those in the batch.
    fn conflicts(&self, id: RevisionId, revision: &Revision) -> Result<Conflicts, StoreError> {
        let subject = RecordId::Revision(id);
        let stored = self.annotations.claims(subject)?;
        let batched = self
            .batch
            .about
            .get(&subject)
            .into_iter()
            .flatten()
            .copied();
        let statements = stored.iter().map(Claim::statement).chain(batched);
        Conflicts::latest(self.batch.store, statements, revision)
    }

    /// Checks that `listed`, the unresolved conflicts of `revision`, are
    /// exactly those it carries at paths the caller may not write: its
    /// predecessors' unresolved conflicts, or for a root, its base revision's.
    /// `listed` only holds conflicts whose entry `revision` still has, so a
    /// revision on a base that changed a conflicted entry is refused.
    fn keeps_conflicts(&self, revision: &Revision, listed: &Conflicts) -> Result<(), PutError> {
        let from = match (&revision.predecessors[..], revision.base) {
            ([], Base::Revision(base)) => vec![base],
            (predecessors, _) => predecessors.to_vec(),
        };
        let mut carried = Conflicts::default();
        for id in from {
            let from = self.batch.revision(id)?;
            carried = carried.with(self.conflicts(id, &from)?);
        }
        self.keeps(&carried, listed)
    }

    /// Checks that `listed` has a conflict at exactly the paths where `kept`
    /// has one, among the paths the caller may not write. Only a writer of a
    /// path may resolve its conflict or add one.
    fn keeps(&self, kept: &Conflicts, listed: &Conflicts) -> Result<(), PutError> {
        let unwritable = |conflicts: &Conflicts| -> BTreeSet<RepoPath> {
            let paths = conflicts.paths().into_iter();
            paths
                .filter(|path| !self.rights.paths.writes(path))
                .collect()
        };
        let (carried, listed) = (unwritable(kept), unwritable(listed));
        if let Some(path) = carried.difference(&listed).next() {
            return Err(Forbidden::Resolve(path.clone()).into());
        }
        if let Some(path) = listed.difference(&carried).next() {
            return Err(Forbidden::Conflict(path.clone()).into());
        }
        Ok(())
    }
}

/// Checks what revisions change at paths their writer may not write, by
/// the rules in the module docs. The service runs it on every revision a
/// signed-in caller puts, and a client on those it pushes, before sending
/// anything.
pub struct Effects<'a, P, V> {
    store: &'a Store<P>,
    /// Where revisions are read from: the batch first, then the store.
    revisions: &'a V,
    /// What the writer may do with each path.
    paths: &'a PathAccess,
}

impl<'a, P: Platform, V: RevisionView> Effects<'a, P, V> {
    /// Creates the checker for revisions of `store`, read from `revisions`,
    /// for a writer with `paths` access.
    pub fn new(store: &'a Store<P>, revisions: &'a V, paths: &'a PathAccess) -> Self {
        Self {
            store,
            revisions,
            paths,
        }
    }

    /// Returns the snapshot of `base`.
    fn base_snapshot(&self, base: Base) -> Result<TreeId, StoreError> {
        match base {
            Base::Revision(id) => Ok(self.revisions.revision(id)?.snapshot),
            Base::Acceptance(_) => self.store.base_snapshot(base),
        }
    }

    /// Returns the effect of `revision`: each file it changes at a path the
    /// writer may not write, from its base's entry to its own, sorted by path.
    fn effect(&self, revision: &Revision) -> Result<Vec<FileChange>, StoreError> {
        let base = self.base_snapshot(revision.base)?;
        let mut changes = self.store.diff(base, revision.snapshot)?;
        changes.retain(|change| !self.paths.writes(&change.path));
        Ok(changes)
    }

    /// Returns what `revision` does that the writer may not do, if anything.
    /// A root may have no effect. A successor must have exactly each
    /// predecessor's effect and, if it sits on another change, its
    /// predecessors must sit on that same change.
    ///
    /// # Errors
    ///
    /// Fails if a revision or tree cannot be read.
    pub fn check(&self, revision: &Revision) -> Result<Option<Forbidden>, StoreError> {
        if self.paths.writes_all() {
            return Ok(None);
        }
        let ours = self.effect(revision)?;
        if revision.predecessors.is_empty() {
            return Ok(ours
                .first()
                .map(|change| Forbidden::Write(change.path.clone())));
        }
        let below = match revision.base {
            Base::Revision(below) => Some(self.revisions.revision(below)?.change),
            Base::Acceptance(_) => None,
        };
        for &id in &revision.predecessors {
            let predecessor = self.revisions.revision(id)?;
            if let Some(below) = below {
                let theirs = match predecessor.base {
                    Base::Revision(theirs) => Some(self.revisions.revision(theirs)?.change),
                    Base::Acceptance(_) => None,
                };
                if theirs != Some(below) {
                    return Ok(Some(Forbidden::Below(below)));
                }
            }
            let theirs = self.effect(&predecessor)?;
            // INVARIANT: both are sorted by path, so the first that differs is a path one lacks, or changes otherwise.
            let differs = ours
                .iter()
                .zip(&theirs)
                .find(|(ours, theirs)| ours != theirs)
                .map(|(ours, theirs)| ours.path.clone().min(theirs.path.clone()))
                .or_else(|| {
                    let longer = if ours.len() > theirs.len() {
                        &ours
                    } else {
                        &theirs
                    };
                    let path = longer.get(ours.len().min(theirs.len()));
                    path.map(|change| change.path.clone())
                });
            if let Some(path) = differs {
                return Ok(Some(Forbidden::Write(path)));
            }
        }
        Ok(None)
    }
}

/// Something a caller may not do.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Forbidden {
    /// An anonymous caller tried to write.
    #[error("anonymous callers only read: sign in with a token")]
    Anonymous,
    /// The owner has no principal to issue a token for.
    #[error("only a signed-in caller gets a token, for themselves")]
    NoPrincipal,
    /// A record put is attributed to someone else.
    #[error("the {record} is by {by}, but you are {caller}")]
    NotYours {
        /// The kind of record.
        record: &'static str,
        /// Who the record names as its author or issuer.
        by: Principal,
        /// The caller.
        caller: Principal,
    },
    /// A statement of a kind the caller may not issue.
    #[error("you may not issue `{0}` statements")]
    Annotate(Kind),
    /// A change to a path the caller may not write.
    #[error("you may not change `{0}`")]
    Write(RepoPath),
    /// A revision or statement of the caller's would resolve a conflict at a
    /// path the caller may not write.
    #[error("you may not resolve the conflict at `{0}`")]
    Resolve(RepoPath),
    /// A revision or statement of the caller's would list a conflict at a path
    /// the caller may not write, though no revision it evolves from lists one.
    #[error("you may not add a conflict at `{0}`")]
    Conflict(RepoPath),
    /// A revision would add a root to a change that already has revisions, or
    /// evolve from a revision of another change.
    #[error(
        "a new revision of change {} must evolve from its revisions, and only from them",
        .0.short()
    )]
    History(ChangeId),
    /// A successor was moved onto a change its predecessors weren't on, so it
    /// would land with that change.
    #[error("you may not move a change you took over onto change {}", .0.short())]
    Below(ChangeId),
    /// A path the caller may not read.
    #[error("you may not read `{0}`")]
    Read(RepoPath),
    /// A revision holds content its writer didn't supply, at a path where no
    /// snapshot it refers to has that content.
    #[error(
        "revision {} holds content at `{path}` that it neither brings nor shares with a \
         snapshot it refers to",
        revision.short()
    )]
    Unsupplied {
        /// The revision.
        revision: RevisionId,
        /// The path.
        path: RepoPath,
    },
    /// A record carries an attachment the repository has, as a revision's
    /// description or a statement's payload, but its batch doesn't bring it.
    #[error("the {record} carries attachment {attachment}, which it doesn't bring")]
    UnsuppliedAttachment {
        /// The kind of record.
        record: &'static str,
        /// The attachment.
        attachment: ObjectId,
    },
    /// A target the caller may not land on.
    #[error("you may not land on `{0}`")]
    Land(TargetName),
    /// A token's scope reaches beyond its issuer's rights.
    #[error("a token may only narrow your rights, which don't include `{0}`")]
    Scope(Capability),
}

/// A failure to check a record being put.
#[derive(Debug, thiserror::Error)]
pub enum PutError {
    /// The caller may not put it.
    #[error(transparent)]
    Forbidden(#[from] Forbidden),
    /// A statement is about, or promotes, a record the caller doesn't see. To
    /// the caller, that record doesn't exist.
    #[error("record {0} not found")]
    Unseen(RecordId),
    /// A record or tree cannot be read.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests;
