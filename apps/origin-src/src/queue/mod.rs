//! The landing queue: how a target's authority lands changes and other
//! targets on the target, one acceptance at a time.
//!
//! Landing a change lands its stack, bottom up, one acceptance per change
//! whose head the target lacks, and a change that waits keeps the whole stack
//! waiting. Each revision is refused, rebased onto the tip ([`Store::merge`]),
//! held on conflict ([`conflict`](crate::conflict)), checked against the policy
//! ([`Policy::governing`]) and its [claims](crate::annotations::Claim), and
//! otherwise signed as an acceptance. Waiting landings live in
//! `queue/<target id>/`. See `docs/design.md`.

mod pending;
mod session;

use std::fs;
use std::io;
use std::path::PathBuf;

use origin_platform::{DurableWrite, LockMode, LockedFile, Native, Platform};
use origin_shared::access::{Level, PathAccess};
use origin_shared::landing::{Blocked, Landing, Pending, Proposal};
use origin_shared::{
    AcceptanceBody, AcceptanceId, ChangeId, EncodedObject, Head, NodeKey, Principal, PrincipalName,
    Record, RecordId, RepoPath, RevisionId, Source, TargetId, TargetName, Tree, TreeId,
};

use crate::annotations::{AnnotateError, Annotations};
use crate::conflict::Conflicts;
use crate::permissions::Forbidden;
use crate::policy::{Policy, PolicyError};
use crate::store::{Store, StoreError};

use pending::PendingFile;
use session::{Candidate, Session};

/// The landing queues of a store's targets, run by one node.
#[derive(Debug)]
pub struct Queue<'a, P = Native> {
    store: &'a Store<P>,
    key: &'a NodeKey,
    node: PrincipalName,
    /// What the submitter may do with each path.
    submitter: PathAccess,
}

impl<'a, P: Platform> Queue<'a, P> {
    /// Returns the queues of `store`, run by the node `node`, which signs with
    /// `key`.
    pub fn new(store: &'a Store<P>, key: &'a NodeKey, node: PrincipalName) -> Self {
        Self {
            store,
            key,
            node,
            submitter: PathAccess::uniform(Level::Write),
        }
    }

    /// Returns these queues, landing for a submitter whose path access is
    /// `paths`. By default the submitter may do everything.
    #[must_use]
    pub fn submitted_by(self, paths: PathAccess) -> Self {
        Self {
            submitter: paths,
            ..self
        }
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
    /// at the first one that waits, which the queue keeps as a landing of
    /// `change`, so retrying it lands the rest of the stack once that one can.
    /// Statements are read through `annotations`, whose index is brought up to
    /// date first, so a caller that keeps one open avoids reading it again.
    ///
    /// # Errors
    ///
    /// Fails if this node is not the target's authority, a change in the stack
    /// is divergent, abandoned or has unresolved conflicts, or the records
    /// cannot be read or written.
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

    /// Lands the tip of `source` on `onto`, reading statements through
    /// `annotations` as [`Queue::land_change`] does.
    ///
    /// # Errors
    ///
    /// Fails if either target does not exist or they are the same, if this node
    /// is not the authority of `onto`, or if the records cannot be read or
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
        let base = history.merge_base(self.store, &tips, |_| Ok::<_, StoreError>(()))?;
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

    /// Creates the target `name` at the tip of `from`, with a first acceptance
    /// that lands `from`'s tip, and returns the new target's head.
    ///
    /// # Errors
    ///
    /// Fails if `name` exists, `from` does not, this node would not be the new
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

    /// Returns every waiting landing, on every target, sorted by target.
    ///
    /// # Errors
    ///
    /// Fails if the queues cannot be listed, or if a landing cannot be read or
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

    /// Starts landing on `onto`: locks its queue, reads the policy in force and
    /// the target's tip, and brings `annotations` up to date.
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
    /// A revision of the change was published while the queue was rebasing it.
    #[error("change {} moved while it was landing: land it again", .0.short())]
    Moved(ChangeId),
    /// The change was abandoned.
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
    /// The submitter may not change what the landing changes.
    #[error(transparent)]
    Forbidden(#[from] Forbidden),
    /// A conflict cannot be recorded.
    #[error(transparent)]
    Annotate(#[from] AnnotateError),
    /// Records cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
pub(crate) mod tests;
