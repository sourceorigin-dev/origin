//! Enforces what callers may do, as their
//! [`Rights`](origin_shared::access::Rights) say: what they read of a
//! repository's records.
//!
//! **Reading.** A caller who reads no path does not see the repository at
//! all. Otherwise they see its targets and acceptances, every revision that
//! changes a path they read or changes nothing, and the statements about
//! what they see. Snapshots withhold the content of the paths they don't
//! read (see [`Store::added`]).

use std::collections::HashMap;

use origin_platform::Platform;
use origin_shared::access::{Capability, PathAccess};
use origin_shared::event::EventKind;
use origin_shared::{Principal, RecordId, RepoPath, Revision, RevisionId};

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

/// Something a caller may not do.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Forbidden {
    /// The owner has no principal to issue a token for.
    #[error("only a signed-in caller gets a token, for themselves")]
    NoPrincipal,
    /// A path the caller may not read.
    #[error("you may not read `{0}`")]
    Read(RepoPath),
    /// A token's scope reaches beyond its issuer's rights.
    #[error("a token may only narrow your rights, which don't include `{0}`")]
    Scope(Capability),
}

#[cfg(test)]
mod tests;
