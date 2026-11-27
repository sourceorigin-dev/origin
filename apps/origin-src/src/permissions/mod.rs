//! Enforces what callers may do, as their [`Rights`] say: what they read
//! of a repository's records, and what the records they put may hold.
//!
//! A caller who reads no path does not see the repository at all. Otherwise
//! they see the targets, acceptances, revisions that change a path they read
//! or nothing, and statements about what they see; snapshots withhold the
//! content of the other paths (see [`Store::added`]). Every record put must be
//! the caller's, of a kind they may annotate, about what they see, and bring the
//! attachments it carries (see [`Batch`]). `docs/design.md` lists the rules.

use std::collections::{HashMap, HashSet};
use std::iter;

use origin_platform::Platform;
use origin_shared::access::{Capability, PathAccess, Rights};
use origin_shared::event::EventKind;
use origin_shared::{
    Kind, Object, ObjectId, Principal, RecordId, RepoPath, Revision, RevisionId, Statement,
};

use crate::annotations::RevisionView;
use crate::store::{Store, StoreError};

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
}

impl<'a, P: Platform> Batch<'a, P> {
    /// Creates the batch of objects `supplied`, whose decoded records are
    /// `decoded`, to be put in `store`.
    pub fn new(
        store: &'a Store<P>,
        supplied: HashSet<ObjectId>,
        decoded: &'a [(ObjectId, Object)],
    ) -> Self {
        let mut batch = Self {
            store,
            supplied,
            revisions: HashMap::new(),
        };
        for (id, object) in decoded {
            if let Object::Revision(revision) = object {
                batch.revisions.insert((*id).into(), revision);
            }
        }
        batch
    }

    /// Whether the batch holds `record`.
    fn holds(&self, record: RecordId) -> bool {
        self.supplied.contains(&record.into())
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
    principal: &'a Principal,
    rights: &'a Rights,
    reader: Reader<'a, P>,
}

impl<'a, P: Platform> Writer<'a, P> {
    /// Checks `batch` for `principal`, with `rights`.
    #[must_use]
    pub fn new(batch: &'a Batch<'a, P>, principal: &'a Principal, rights: &'a Rights) -> Self {
        Self {
            batch,
            principal,
            rights,
            reader: Reader::new(batch.store, &rights.paths),
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

    /// Checks a statement being put.
    ///
    /// # Errors
    ///
    /// Fails if the caller isn't its issuer, may not issue its kind, or
    /// doesn't see what it is about or the original it promotes, or if a
    /// record cannot be read.
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
        Ok(())
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
    /// A path the caller may not read.
    #[error("you may not read `{0}`")]
    Read(RepoPath),
    /// A record carries an attachment the repository has, as a revision's
    /// description or a statement's payload, but its batch doesn't bring it.
    #[error("the {record} carries attachment {attachment}, which it doesn't bring")]
    UnsuppliedAttachment {
        /// The kind of record.
        record: &'static str,
        /// The attachment.
        attachment: ObjectId,
    },
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
