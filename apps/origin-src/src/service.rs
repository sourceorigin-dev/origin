//! `OriginService`: everything a repository can be asked, independent of
//! transport.
//!
//! It is the one API over a repository. The HTTP routes only translate
//! requests into its methods, and the command line calls them in-process
//! for the repository it runs in, so a local repository and a remote one
//! answer the same way. Plugins use it the same two ways.
//!
//! A service is meant to live as long as its process: it keeps the record
//! index open, catching it up with the op log before each use instead of
//! reading it whole. Calls don't wait for each other to use the index: each takes
//! one of the indexes kept open, or opens another if they're all in use.
//! Landings on one target still go one at a time, through the target's
//! queue lock.
//!
//! Every method takes the [`Caller`]. Nothing is checked yet: every caller
//! may read and write everything.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use origin_platform::Native;
use origin_shared::api::{
    Annotation, ErrorCode, LandRequest, Landings, LogEntry, NewTarget, RevisionEntry, Revisions,
    TargetInfo,
};
use origin_shared::landing::{Pending, Proposal};
use origin_shared::{Base, ChangeId, Head, NodeKey, PrincipalName, RecordId, TargetName};

use crate::annotations::Annotations;
use crate::index::Index;
use crate::policy::{Policy, PolicyError};
use crate::queue::{LandError, Queue};
use crate::store::{Store, StoreError};

/// Who calls the service. Permissions will decide what each may do by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// The node's own user, through the command line.
    Owner,
    /// A client of a server that didn't authenticate.
    Anonymous,
}

/// A node: its name, and the key it signs acceptances with as the
/// authority of the targets its repositories' policies name it for. A
/// server's repositories share their node.
#[derive(Clone, Debug)]
pub struct Node {
    /// The node's name.
    pub name: PrincipalName,
    /// Its key.
    pub key: Arc<NodeKey>,
}

/// One repository, served by its node.
#[derive(Debug)]
pub struct OriginService {
    store: Store,
    node: Node,
    /// The record indexes kept open, but those in use: see the module.
    indexes: Mutex<Vec<Index>>,
}

impl OriginService {
    /// The most indexes kept open for calls to take.
    const KEPT_INDEXES: usize = 8;

    /// Serves the repository in `store` as `node`.
    #[must_use]
    pub fn new(store: Store, node: Node) -> Self {
        Self {
            store,
            node,
            indexes: Mutex::default(),
        }
    }

    /// The service, with `index`, just read, as its index.
    #[must_use]
    pub fn with_index(self, index: Index) -> Self {
        Self {
            indexes: Mutex::new(vec![index]),
            ..self
        }
    }

    /// Creates a repository without a working copy, as a server hosts them,
    /// in `root`, which must not exist or be empty: its store, and the
    /// targets `policy`, holding `policy` as its `policy.toml`, and `main`.
    ///
    /// # Errors
    ///
    /// Fails if `root` isn't empty, the policy doesn't make `node` the
    /// authority of both targets, or the repository cannot be written.
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

    /// Opens the repository without a working copy in `root`, which
    /// [`OriginService::create`] made.
    ///
    /// # Errors
    ///
    /// Fails if `root` has no repository, or is on a network file system.
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

    /// Runs `call` with the statements of the repository, through one of the
    /// indexes kept open, caught up first, or a new one if they're all in
    /// use.
    fn annotations<'s, T>(
        &'s self,
        call: impl FnOnce(&mut Annotations<'s>) -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        let kept = self.kept_indexes().pop();
        // A failed refresh drops the index, to read whole next time.
        let index = match kept {
            Some(mut index) => {
                index.refresh(&self.store)?;
                index
            }
            None => Index::open(&self.store)?,
        };
        let mut annotations = Annotations::new(&self.store, index);
        let result = call(&mut annotations);
        let mut kept = self.kept_indexes();
        if kept.len() < Self::KEPT_INDEXES {
            kept.push(annotations.into_index());
        }
        result
    }

    fn kept_indexes(&self) -> MutexGuard<'_, Vec<Index>> {
        self.indexes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The statements about `subject`, in the order the repository received
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

    /// The revisions of `change`, and its heads.
    ///
    /// # Errors
    ///
    /// Fails if the repository has no revision of the change, or cannot
    /// read one.
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

    /// Every target, by name, with its tip, and its authority and
    /// requirements under the policy in force.
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

    /// The acceptances of `target` from position `since` on, oldest first.
    ///
    /// # Errors
    ///
    /// Fails if the target doesn't exist, or its log or an acceptance
    /// cannot be read.
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

    /// Lands what `request` proposes, with this node as the authority: a
    /// change with the changes below it the target doesn't have yet, or
    /// another target's tip. A change lands by default on the target its
    /// stack is on.
    ///
    /// # Errors
    ///
    /// Fails if the change or a target doesn't exist, a target was given
    /// no target to land on, this node isn't the target's authority, the
    /// change can't land as it is (see [`LandError`]), or the records
    /// cannot be read or written.
    pub fn land(&self, _caller: &Caller, request: &LandRequest) -> Result<Landings, ServiceError> {
        let queue = Queue::new(&self.store, &self.node.key, self.node.name.clone());
        self.annotations(|annotations| {
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
        })
    }

    /// The target the stack of `change` is on: that of the acceptance at
    /// the bottom of its head's stack.
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

    /// Every landing that waits, on every target, and why.
    ///
    /// # Errors
    ///
    /// Fails if the queues cannot be read.
    pub fn landings(&self, _caller: &Caller) -> Result<Vec<Pending>, ServiceError> {
        Ok(Queue::pending(&self.store)?)
    }

    /// Creates a target at the tip of another, with this node as its
    /// authority.
    ///
    /// # Errors
    ///
    /// Fails if the name is taken, the other target doesn't exist, the
    /// policy wouldn't make this node the new target's authority, or the
    /// records cannot be written.
    pub fn create_target(
        &self,
        _caller: &Caller,
        request: &NewTarget,
    ) -> Result<Head, ServiceError> {
        let queue = Queue::new(&self.store, &self.node.key, self.node.name.clone());
        Ok(queue.create_target(&request.name, &request.from)?)
    }
}

/// A failed service call.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// A directory holds no repository.
    #[error("{} is not an origin repository", .0.display())]
    NotARepository(PathBuf),
    /// `create` was pointed at a directory with files in it.
    #[error("{} is not empty", .0.display())]
    NotEmpty(PathBuf),
    /// No revision of the change is known.
    #[error("change {} has no revisions", .0.short())]
    UnknownChange(ChangeId),
    /// No target has this name.
    #[error("no target is named `{0}`")]
    UnknownTarget(TargetName),
    /// A target was proposed for landing without a target to land on.
    #[error("landing a target needs a target to land on")]
    NoTarget,
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
    /// What kind of failure it is, for clients to tell apart.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::UnknownChange(_) | Self::UnknownTarget(_) => ErrorCode::NotFound,
            Self::NoTarget => ErrorCode::BadRequest,
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
    use super::*;

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
        assert_eq!(service.kept_indexes().len(), 2, "both kept for later calls");
    }
}
