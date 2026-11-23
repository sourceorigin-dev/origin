//! The landing queue: how a target's authority lands changes on the target,
//! one acceptance at a time. [`Queue::init`] starts a repository's history
//! and checks the policy it begins with.

use origin_platform::{Native, Platform};
use origin_shared::{
    AcceptanceBody, AcceptanceId, EncodedObject, NodeKey, PrincipalName, Record, TargetName, Tree,
    TreeId,
};

use crate::policy::{Policy, PolicyError};
use crate::store::{Store, StoreError};

/// The landing queues of a store's targets, run by one node.
#[derive(Debug)]
pub struct Queue<'a, P = Native> {
    store: &'a Store<P>,
    key: &'a NodeKey,
    node: PrincipalName,
}

impl<'a, P: Platform> Queue<'a, P> {
    /// Returns the queues of `store`, run by the node `node`, which signs with
    /// `key`.
    pub fn new(store: &'a Store<P>, key: &'a NodeKey, node: PrincipalName) -> Self {
        Self { store, key, node }
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
}

/// A failure to land.
#[derive(Debug, thiserror::Error)]
pub enum LandError {
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
    /// Landing on `policy` would leave it without a valid policy.
    #[error("landing would leave `policy` without a valid policy.toml")]
    InvalidPolicy(#[source] PolicyError),
    /// Records cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}
