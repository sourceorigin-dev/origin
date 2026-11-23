//! The landing queue: how a target's authority lands changes on the target,
//! one acceptance at a time.
//!
//! Landing a change lands its stack: the head of each change below it that
//! the target does not have yet, from the bottom up, each with its own
//! acceptance. A divergent change is refused. Landing stops at the first
//! change that waits. For each revision:
//!
//! 1. **Rebase.** The changes the revision made to its base are merged onto
//!    the target's tip, file by file ([`Store::merge`]). A revision made on
//!    the tip lands as it is; this node rebases any other into a new
//!    revision of its change, on the tip. If the merge would leave the
//!    target unchanged, nothing lands: the target already has the change.
//! 2. **Conflicts.** If both sides changed a path, a `core.conflict`
//!    statement about the revision lists the paths, and the change waits
//!    until a new revision of it merges.
//! 3. **Policy.** The policy in force must name this node as the target's
//!    authority.
//! 4. **Otherwise** this node signs an acceptance naming what landed and
//!    the policy, and appends it to the target's chain.
//!
//! Each target's queue lives in `queue/<target id>/`. It holds a `lock`,
//! taken while landing so landings on one target never race.

mod session;

use std::fmt;
use std::fs;
use std::path::PathBuf;

use origin_platform::{LockMode, LockedFile, Native, Platform};
use origin_shared::{
    AcceptanceBody, AcceptanceId, ChangeId, EncodedObject, NodeKey, Principal, PrincipalName,
    Record, RecordId, RepoPath, RevisionId, Source, StatementId, TargetId, TargetName, Tree,
    TreeId,
};

use crate::annotations::{AnnotateError, Annotations};
use crate::policy::{Policy, PolicyError};
use crate::store::{Store, StoreError};
use crate::targets::Head;

use session::Session;

/// The landing queues of a store's targets, run by one node.
#[derive(Debug)]
pub struct Queue<'a, P = Native> {
    store: &'a Store<P>,
    key: &'a NodeKey,
    node: PrincipalName,
}

/// What a landing lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proposal {
    /// A change, as of its head.
    Change(ChangeId),
}

/// The outcome of a landing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Landing {
    /// It landed.
    Landed {
        /// What landed.
        proposal: Proposal,
        /// The revision the acceptance names.
        source: Source,
        /// The acceptance.
        acceptance: Head,
    },
    /// The target already has it, so nothing landed.
    Unchanged(Proposal),
    /// It waits.
    Pending(Pending),
}

/// A landing that waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// The target it lands on.
    pub onto: TargetName,
    /// What lands.
    pub proposal: Proposal,
    /// The revision that didn't merge.
    pub subject: RecordId,
    /// Why it waits.
    pub blocked: Blocked,
}

/// Why a landing waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocked {
    /// Both sides changed these paths. The `core.conflict` statement lists
    /// them too.
    Conflict {
        /// The statement.
        statement: StatementId,
        /// The paths, sorted.
        paths: Vec<RepoPath>,
    },
}

impl<'a, P: Platform> Queue<'a, P> {
    /// Returns the queues of `store`, run by the node `node`, which signs with
    /// `key`.
    pub fn new(store: &'a Store<P>, key: &'a NodeKey, node: PrincipalName) -> Self {
        Self { store, key, node }
    }

    /// Returns this node as a principal: the issuer of the statements and the
    /// author of the revisions the queue makes.
    fn principal(&self) -> Principal {
        Principal::Node(self.node.clone())
    }

    /// Starts a repository's history: creates the `policy` target, holding
    /// `policy` as its `policy.toml`, and an empty `main`. The policy must make
    /// this node the authority of both. Returns `main`'s first acceptance.
    ///
    /// # Errors
    ///
    /// Fails if the policy is invalid or does not make this node the authority
    /// of both targets, if either target exists, or if the records cannot be
    /// written.
    pub fn init(&self, policy: &str) -> Result<AcceptanceId, LandError> {
        let (file, tree) = Policy::snapshot(policy);
        let empty = Tree::default().encode();
        self.store.write_all(&[file, tree.clone(), empty.clone()])?;
        let policy = Self::check_policy(self.store, tree.id().into())?;
        for target in [TargetName::POLICY, TargetName::MAIN] {
            self.authorize(&policy, &target)?;
        }
        let genesis = |target: &TargetName, state: &EncodedObject, policy| {
            AcceptanceBody {
                target: target.id(),
                seq: 0,
                prev: None,
                state: state.id().into(),
                landed: Vec::new(),
                evidence: Vec::new(),
                policy,
            }
            .sign(self.key)
        };
        let policy = self.store.append(
            &TargetName::POLICY,
            &genesis(&TargetName::POLICY, &tree, None),
        )?;
        Ok(self.store.append(
            &TargetName::MAIN,
            &genesis(&TargetName::MAIN, &empty, Some(policy)),
        )?)
    }

    /// Lands `change` and the changes below it that `onto` does not have yet,
    /// from the bottom of the stack up, and returns the outcome of each. Stops
    /// at the first one that waits.
    ///
    /// # Errors
    ///
    /// Fails if this node is not the target's authority, a change in the stack
    /// is divergent, or the records cannot be read or written.
    pub fn land_change(
        &self,
        change: ChangeId,
        onto: &TargetName,
    ) -> Result<Vec<Landing>, LandError> {
        let mut session = self.session(onto)?;
        let stack = session.stack(change)?;
        if stack.is_empty() {
            return Ok(vec![Landing::Unchanged(Proposal::Change(change))]);
        }
        let proposal = Proposal::Change(change);
        let mut landings = Vec::new();
        for (id, revision) in stack {
            let landing = session.land_revision(&proposal, id, revision)?;
            let waits = matches!(landing, Landing::Pending(_));
            landings.push(landing);
            if waits {
                break;
            }
        }
        Ok(landings)
    }

    /// Returns the policy in `state`, which is about to come into force, if it
    /// is valid and names an authority for `policy`. Without one, no one could
    /// ever land on `policy` again.
    fn check_policy(store: &Store<P>, state: TreeId) -> Result<Policy, LandError> {
        let policy = Policy::of_snapshot(store, state)
            .and_then(|policy| policy.ok_or(PolicyError::Missing))
            .map_err(LandError::InvalidPolicy)?;
        if policy.authority(&TargetName::POLICY).is_none() {
            return Err(LandError::NoAuthority(TargetName::POLICY));
        }
        Ok(policy)
    }

    /// The name of the lock file in each target's queue.
    const LOCK: &str = "lock";

    /// Returns the directory of `target`'s queue, durably creating it if it is
    /// missing.
    fn queue_dir(&self, target: TargetId) -> Result<PathBuf, StoreError> {
        let root = self.store.queue_dir();
        let dir = root.join(target.to_hex());
        if !dir.is_dir() {
            fs::create_dir_all(&dir).map_err(|source| StoreError::io(&dir, source))?;
            for parent in [&root, root.parent().unwrap_or(&root)] {
                self.store.sync_dir(parent)?;
            }
        }
        Ok(dir)
    }

    /// Locks the queue of `target`, waiting for any landing in progress to
    /// finish.
    fn lock(&self, target: &TargetName) -> Result<LockedFile, StoreError> {
        let path = self.queue_dir(target.id())?.join(Self::LOCK);
        Ok(LockedFile::open(
            &self.store.platform,
            &path,
            LockMode::Exclusive,
        )?)
    }

    /// Checks that `policy` makes this node the authority of `target`, with
    /// this node's key.
    fn authorize(&self, policy: &Policy, target: &TargetName) -> Result<(), LandError> {
        let authority = policy
            .authority(target)
            .ok_or_else(|| LandError::NoAuthority(target.clone()))?;
        if *authority != self.node {
            return Err(LandError::NotAuthority {
                target: target.clone(),
                authority: authority.clone(),
                node: self.node.clone(),
            });
        }
        if policy.node_key(authority) != Some(&self.key.public()) {
            return Err(LandError::WrongKey(self.node.clone()));
        }
        Ok(())
    }

    /// Starts landing on `onto`: locks its queue, then reads the policy in
    /// force and the target's tip.
    fn session(&self, onto: &TargetName) -> Result<Session<'_, 'a, P>, LandError> {
        let lock = self.lock(onto)?;
        let tip = self
            .store
            .head(onto)?
            .ok_or_else(|| LandError::UnknownTarget(onto.clone()))?;
        let (policy_head, policy) = Policy::in_force(self.store)?;
        self.authorize(&policy, onto)?;
        Ok(Session {
            queue: self,
            onto: onto.clone(),
            policy_head,
            state: self.store.get(tip.acceptance)?.body().state,
            tip,
            annotations: Annotations::open(self.store)?,
            _lock: lock,
        })
    }
}

impl fmt::Display for Proposal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Change(change) => write!(f, "change {}", change.short()),
        }
    }
}

/// A failure to land.
#[derive(Debug, thiserror::Error)]
pub enum LandError {
    /// No target has this name.
    #[error("no target is named `{0}`")]
    UnknownTarget(TargetName),
    /// The policy names no authority for the target.
    #[error("policy.toml names no authority for `{0}`: give it, or `policy`, a [targets] table")]
    NoAuthority(TargetName),
    /// Another node is the target's authority.
    #[error(
        "node:{authority} is the authority of `{target}`, so only it lands there, and this is \
         node:{node}"
    )]
    NotAuthority {
        /// The target.
        target: TargetName,
        /// Its authority.
        authority: PrincipalName,
        /// This node.
        node: PrincipalName,
    },
    /// The policy lists a different key for this node.
    #[error("policy.toml lists another key for node:{0} than this node's")]
    WrongKey(PrincipalName),
    /// No revision of the change is known.
    #[error("change {} has no revisions", .0.short())]
    UnknownChange(ChangeId),
    /// The change has several heads.
    #[error(
        "change {} is divergent, so it cannot land: its heads are {}. Run `origin converge {}` \
         first",
        change.short(),
        heads.iter().map(RevisionId::short).collect::<Vec<_>>().join(", "),
        change.short()
    )]
    Divergent {
        /// The change.
        change: ChangeId,
        /// Its heads.
        heads: Vec<RevisionId>,
    },
    /// Landing on `policy` would leave it without a valid policy.
    #[error("landing would leave `policy` without a valid policy.toml")]
    InvalidPolicy(#[source] PolicyError),
    /// The policy in force cannot be read.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// A conflict cannot be recorded.
    #[error(transparent)]
    Annotate(#[from] AnnotateError),
    /// Records cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests;
