//! The landing queue: how a target's authority lands changes and other
//! targets on it, one acceptance at a time.
//!
//! Landing a change lands its stack: the head of each change below it that
//! isn't on the target yet, from the bottom, with an acceptance each. A
//! change that diverged, that its creator abandoned, or whose head has
//! unresolved conflicts (see [`conflict`](crate::conflict)) is refused. A
//! stack that waits is kept whole, so retrying it lands the rest once the
//! change it waits on can. For each revision:
//!
//! 1. **Rebase.** The changes it made to its base are merged onto the
//!    target's tip, file by file ([`Store::merge`]). A revision made on the
//!    tip lands as it is; any other is rebased into a new revision of its
//!    change, on the tip, by this node. A landing that would leave the
//!    target as it is lands nothing: the target already has the change.
//! 2. **Conflicts.** If both sides changed a path, a `core.conflict`
//!    statement about the revision lists the paths, and the change waits,
//!    blocked, until a new revision of it merges.
//! 3. **Policy.** The policy in force now must name this node as the
//!    target's authority. On `policy`, that's the policy of the acceptance
//!    before, never the one landing ([`Policy::governing`]). Each of the
//!    target's requirements needs a [claim](crate::annotations::Claim)
//!    about the revision that lands: results with the same input digest
//!    are promoted to it first, and a valid promotion claims what its
//!    original does, as its original's issuer.
//! 4. **Missing evidence** leaves the landing waiting, until the statements
//!    are issued and the landing is retried.
//! 5. **Otherwise** this node signs an acceptance naming what landed, the
//!    evidence and the policy, and appends it to the target's chain.
//!
//! Landing another target works the same way: its tip is merged from the
//! newest acceptances both targets' histories share, and the requirements
//! apply to the merged snapshot. A merge that changes nothing lands
//! nothing if either tip is in the other's history: the target has the
//! source's already, or the source has nothing more. Otherwise it lands,
//! with no evidence then, so the target's history records the source's tip
//! and later merges start from it.
//!
//! The queue of each target lives in `queue/<target id>/`: a `lock` held
//! while landing, so landings on one target never race, and a file for each
//! landing that waits, `change-<id>` or `target-<target id>`, so waiting
//! landings can be listed and retried.

mod pending;
mod session;

use std::fs;
use std::io;
use std::path::PathBuf;

use origin_platform::{DurableWrite, LockMode, LockedFile, Native, Platform};
use origin_shared::landing::{Blocked, Landing, Pending, Proposal};
use origin_shared::{
    AcceptanceBody, AcceptanceId, ChangeId, EncodedObject, Head, NodeKey, Principal, PrincipalName,
    Record, RecordId, RepoPath, RevisionId, Source, TargetId, TargetName, Tree, TreeId,
};

use crate::annotations::{AnnotateError, Annotations};
use crate::conflict::Conflicts;
use crate::policy::{Policy, PolicyError};
use crate::store::{Store, StoreError};

use pending::PendingFile;
use session::{Candidate, Session};

/// The landing queues of a store's targets, as run by one node.
#[derive(Debug)]
pub struct Queue<'a, P = Native> {
    store: &'a Store<P>,
    key: &'a NodeKey,
    node: PrincipalName,
}

impl<'a, P: Platform> Queue<'a, P> {
    /// The queues of `store`, run by the node `node`, which signs with
    /// `key`.
    pub fn new(store: &'a Store<P>, key: &'a NodeKey, node: PrincipalName) -> Self {
        Self { store, key, node }
    }

    /// This node, which issues the statements and revisions the queue
    /// makes.
    fn principal(&self) -> Principal {
        Principal::Node(self.node.clone())
    }

    /// Starts a repository's history: the `policy` target, holding `policy`
    /// as its `policy.toml`, and an empty `main`, both of which the policy
    /// must have this node order. Returns `main`'s first acceptance.
    ///
    /// # Errors
    ///
    /// Fails if the policy is invalid or doesn't make this node the
    /// authority of both targets, either target exists, or the records
    /// cannot be written.
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

    /// Lands `change`, and the changes below it that `onto` doesn't have
    /// yet, from the bottom of the stack. Stops at the first that waits,
    /// which the queue keeps as a landing of `change`: retrying it lands
    /// the rest of the stack once that one can.
    /// The statements are found through `annotations`, whose index is
    /// caught up first, so callers that keep one open don't read it again.
    ///
    /// # Errors
    ///
    /// Fails if this node isn't the target's authority, a change in the
    /// stack diverged, was abandoned or has unresolved conflicts, or the
    /// records cannot be read or written.
    pub fn land_change(
        &self,
        annotations: &mut Annotations<'a, P>,
        change: ChangeId,
        onto: &TargetName,
    ) -> Result<Vec<Landing>, LandError> {
        let mut session = self.session(annotations, onto)?;
        let stack = session.stack(change)?;
        if stack.is_empty() {
            session.forget(&Proposal::Change(change))?;
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

    /// Lands the tip of `source` on `onto`, finding statements through
    /// `annotations` as [`Queue::land_change`] does.
    ///
    /// # Errors
    ///
    /// Fails if either target doesn't exist or they're the same, this node
    /// isn't the authority of `onto`, or the records cannot be read or
    /// written.
    pub fn land_target(
        &self,
        annotations: &mut Annotations<'a, P>,
        source: &TargetName,
        onto: &TargetName,
    ) -> Result<Landing, LandError> {
        if source == onto {
            return Err(LandError::SameTarget(onto.clone()));
        }
        let mut session = self.session(annotations, onto)?;
        let tip = self
            .store
            .head(source)?
            .ok_or_else(|| LandError::UnknownTarget(source.clone()))?;
        let state = self.store.get(tip.acceptance)?.body().state;
        let tips = [session.tip.acceptance, tip.acceptance];
        let history = session.history(&tips)?;
        let base = history.merge_base(self.store, &tips)?;
        let merge = self.store.merge(base, state, session.state)?;
        let proposal = Proposal::Target(source.clone());
        if !merge.conflicts.is_empty() {
            let subject = RecordId::Acceptance(tip.acceptance);
            return session.block(proposal, subject, &merge.conflicts);
        }
        // A merge that changes nothing has nothing to record either when
        // one tip is in the other's history: the target has the source's
        // already, or the source has nothing the target lacks.
        let [ours, theirs] = tips;
        let either = history.descends(ours, theirs) || history.descends(theirs, ours);
        if merge.tree == session.state && either {
            session.forget(&proposal)?;
            return Ok(Landing::Unchanged(proposal));
        }
        // Even a merge that changes nothing lands, so that the tip is in
        // the target's history, and later merges start from it.
        session.submit(Candidate {
            kept_as: proposal.clone(),
            proposal,
            source: Source::Target(tip.acceptance),
            subject: RecordId::Snapshot(merge.tree),
            state: merge.tree,
            trees: merge.trees,
            revision: None,
        })
    }

    /// Creates the target `name` at the tip of `from`, with a first
    /// acceptance landing it.
    ///
    /// # Errors
    ///
    /// Fails if `name` exists, `from` doesn't, this node wouldn't be the new
    /// target's authority, or the records cannot be read or written.
    pub fn create_target(&self, name: &TargetName, from: &TargetName) -> Result<Head, LandError> {
        let _lock = self.lock(name)?;
        if self.store.head(name)?.is_some() {
            return Err(LandError::TargetExists(name.clone()));
        }
        let (policy_head, policy) = Policy::in_force(self.store)?;
        self.authorize(&policy, name)?;
        let from = self
            .store
            .head(from)?
            .ok_or_else(|| LandError::UnknownTarget(from.clone()))?;
        let acceptance = AcceptanceBody {
            target: name.id(),
            seq: 0,
            prev: None,
            state: self.store.get(from.acceptance)?.body().state,
            landed: vec![Source::Target(from.acceptance)],
            evidence: Vec::new(),
            policy: Some(policy_head),
        }
        .sign(self.key);
        let acceptance = self.store.append(name, &acceptance)?;
        Ok(Head { seq: 0, acceptance })
    }

    /// Every landing that waits, on every target, sorted by target.
    ///
    /// # Errors
    ///
    /// Fails if the queues cannot be listed, or a landing cannot be read or
    /// is malformed.
    pub fn pending(store: &Store<P>) -> Result<Vec<Pending>, StoreError> {
        let root = store.queue_dir();
        let list = |dir: &PathBuf| match fs::read_dir(dir) {
            Ok(entries) => entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<_>>>()
                .map_err(|source| StoreError::io(dir, source)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(StoreError::io(dir, source)),
        };
        let mut pending = Vec::new();
        for dir in list(&root)? {
            for path in list(&dir)? {
                let name = path.file_name().unwrap_or_default();
                if name == Self::LOCK || DurableWrite::<P>::is_temporary(name) {
                    continue;
                }
                // Read without the lock: a landing that just ended forgot it.
                let text = match fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(source) => return Err(StoreError::io(&path, source)),
                };
                let mut landing = PendingFile::decode(&text)
                    .ok_or_else(|| StoreError::Malformed(path.clone()))?;
                if let Blocked::Conflict { statement, paths } = &mut landing.blocked {
                    *paths = Conflicts::listed(store, &store.get(*statement)?)?.paths();
                }
                pending.push(landing);
            }
        }
        pending.sort_by(|a, b| a.onto.cmp(&b.onto));
        Ok(pending)
    }

    /// The policy in `state`, about to become the policy in force, if it's
    /// a valid one that names an authority for `policy`, so the target
    /// never locks up.
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

    /// The directory of `target`'s queue, created durably if it's missing.
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

    /// Locks the queue of `target`, waiting for the landing in progress.
    fn lock(&self, target: &TargetName) -> Result<LockedFile, StoreError> {
        let path = self.queue_dir(target.id())?.join(Self::LOCK);
        Ok(LockedFile::open(
            &self.store.platform,
            &path,
            LockMode::Exclusive,
        )?)
    }

    /// Checks that this node is the authority of `target` under `policy`,
    /// with the key it lists.
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
    /// force and the target's tip, and catches `annotations` up.
    fn session<'q>(
        &'q self,
        annotations: &'q mut Annotations<'a, P>,
        onto: &TargetName,
    ) -> Result<Session<'q, 'a, P>, LandError> {
        let lock = self.lock(onto)?;
        annotations.refresh()?;
        let tip = self
            .store
            .head(onto)?
            .ok_or_else(|| LandError::UnknownTarget(onto.clone()))?;
        // A landing on `policy` is governed by the acceptance before it: its
        // tip, whatever the policy it lands says.
        let (policy_head, policy) = if *onto == TargetName::POLICY {
            let state = self.store.get(tip.acceptance)?.body().state;
            (tip.acceptance, Self::check_policy(self.store, state)?)
        } else {
            Policy::in_force(self.store)?
        };
        self.authorize(&policy, onto)?;
        Ok(Session {
            queue: self,
            onto: onto.clone(),
            policy,
            policy_head,
            state: self.store.get(tip.acceptance)?.body().state,
            tip,
            annotations,
            _lock: lock,
        })
    }
}

/// A failure to land.
#[derive(Debug, thiserror::Error)]
pub enum LandError {
    /// No target has this name.
    #[error("no target is named `{0}`")]
    UnknownTarget(TargetName),
    /// `target new` was given a name already taken.
    #[error("a target is already named `{0}`")]
    TargetExists(TargetName),
    /// A target was landed on itself.
    #[error("cannot land `{0}` on itself")]
    SameTarget(TargetName),
    /// The policy names no authority for the target.
    #[error("policy.toml names no authority for `{0}`: give it, or `policy`, a [targets] table")]
    NoAuthority(TargetName),
    /// Another node orders the target.
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
    /// The policy lists another key for this node.
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
    /// The change was given up.
    #[error("change {} was abandoned, so it cannot land", .0.short())]
    Abandoned(ChangeId),
    /// The change's head has unresolved conflicts.
    #[error(
        "change {} has unresolved conflicts, so it cannot land: edit {}, or accept them as they \
         are with `origin resolve`",
        change.short(),
        paths.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
    )]
    Unresolved {
        /// The change.
        change: ChangeId,
        /// The unresolved paths.
        paths: Vec<RepoPath>,
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
pub(crate) mod tests;
