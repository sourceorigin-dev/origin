//! Landing one candidate: rebasing it onto the target's tip, checking
//! the evidence the policy requires, and appending its acceptance, or
//! keeping it to wait.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use origin_platform::{LockedFile, Platform};
use origin_shared::landing::{Blocked, Landing, Pending, Proposal};
use origin_shared::policy::Requirement;
use origin_shared::{
    AcceptanceBody, AcceptanceId, Base, ChangeId, EncodedObject, Head, Kind, Record, RecordId,
    Revision, RevisionId, Source, StatementId, TargetName, TreeId,
};

use crate::ancestry::Ancestry;
use crate::annotations::{AnnotateError, Annotations, Claim};
use crate::conflict::Conflicts;
use crate::oplog::Effect;
use crate::policy::{Policy, PolicyError};
use crate::store::{Store, StoreError};

use super::pending::PendingFile;
use super::{LandError, Queue};

/// Landings onto one target, holding its queue's lock.
pub(super) struct Session<'q, 'a, P> {
    pub(super) queue: &'q Queue<'a, P>,
    pub(super) onto: TargetName,
    pub(super) policy: Policy,
    /// The acceptance of `policy` holding `policy`.
    pub(super) policy_head: AcceptanceId,
    /// The target's latest acceptance, and its snapshot.
    pub(super) tip: Head,
    pub(super) state: TreeId,
    pub(super) annotations: Annotations<'a, P>,
    pub(super) _lock: LockedFile,
}

/// What a session is about to land.
pub(super) struct Candidate {
    /// What lands.
    pub(super) proposal: Proposal,
    /// What the queue keeps if it waits: the landing of the stack it's in.
    pub(super) kept_as: Proposal,
    pub(super) source: Source,
    /// What the evidence must be about.
    pub(super) subject: RecordId,
    /// The target's snapshot once it lands.
    pub(super) state: TreeId,
    /// The trees the merge made, to write first.
    pub(super) trees: Vec<EncodedObject>,
    /// The rebased revision, to write after the trees, if it is one.
    pub(super) revision: Option<EncodedObject>,
}

impl<P: Platform> Session<'_, '_, P> {
    fn store(&self) -> &Store<P> {
        self.queue.store
    }

    /// The heads to land for `change`, bottom first: the change's head, and
    /// below it, following each one's base, the heads of the changes the
    /// target doesn't have yet.
    pub(super) fn stack(&self, change: ChangeId) -> Result<Vec<(RevisionId, Revision)>, LandError> {
        let index = self.annotations.index();
        let mut stack = Vec::new();
        let mut seen = HashSet::new();
        let mut next = Some(change);
        while let Some(change) = next.take() {
            if !seen.insert(change) {
                break;
            }
            let head = match index.heads(change)[..] {
                [head] => head,
                [] => return Err(LandError::UnknownChange(change)),
                ref heads => {
                    return Err(LandError::Divergent {
                        change,
                        heads: heads.to_vec(),
                    });
                }
            };
            let revision: Revision = self.store().get(head)?;
            let landing = self.store().landing(head, &revision)?;
            if landing.is_some_and(|landing| landing.body().target == self.onto.id()) {
                break;
            }
            let claims = self.annotations.claims(RecordId::Revision(head))?;
            for claim in &claims {
                let statement = claim.statement();
                if statement.kind == Kind::ABANDON && self.store().authorizes(statement)? {
                    return Err(LandError::Abandoned(change));
                }
            }
            let conflicts = Conflicts::of(self.store(), &claims, &revision)?;
            if !conflicts.is_empty() {
                return Err(LandError::Unresolved {
                    change,
                    paths: conflicts.paths(),
                });
            }
            if let Base::Revision(below) = revision.base {
                next = Some(self.store().get(below)?.change);
            }
            stack.push((head, revision));
        }
        stack.reverse();
        Ok(stack)
    }

    /// Rebases the revision `id` onto the tip and lands it, as part of the
    /// landing of `stack`, which is kept if it waits.
    pub(super) fn land_revision(
        &mut self,
        stack: &Proposal,
        id: RevisionId,
        revision: Revision,
    ) -> Result<Landing, LandError> {
        let proposal = Proposal::Change(revision.change);
        let base = match revision.base {
            Base::Acceptance(id) => self.store().get(id)?.body().state,
            Base::Revision(id) => self.store().get(id)?.snapshot,
        };
        let merge = self
            .store()
            .merge(Some(base), revision.snapshot, self.state)?;
        if !merge.conflicts.is_empty() {
            return self.block(stack.clone(), RecordId::Revision(id), &merge.conflicts);
        }
        if merge.tree == self.state {
            self.forget(&proposal)?;
            return Ok(Landing::Unchanged(proposal));
        }
        let tip = Base::Acceptance(self.tip.acceptance);
        if revision.base == tip {
            return self.submit(Candidate {
                proposal,
                kept_as: stack.clone(),
                source: Source::Revision(id),
                subject: RecordId::Revision(id),
                state: merge.tree,
                trees: merge.trees,
                revision: None,
            });
        }
        let rebased = Revision {
            predecessors: vec![id],
            base: tip,
            snapshot: merge.tree,
            author: self.queue.principal(),
            ..revision
        }
        .encode();
        let rebased_id = RevisionId::from(rebased.id());
        self.submit(Candidate {
            proposal,
            kept_as: stack.clone(),
            source: Source::Revision(rebased_id),
            subject: RecordId::Revision(rebased_id),
            state: merge.tree,
            trees: merge.trees,
            revision: Some(rebased),
        })
    }

    /// Checks the policy for `candidate`, and lands it if its evidence is
    /// there, or keeps it waiting. Writes the records in layers, each
    /// durable before the next refers to it: the trees, then the rebased
    /// revision, then the promoted statements and the acceptance. The
    /// promotions refer to their originals' payloads without writing them,
    /// so it [holds the objects](Store::hold_objects) throughout.
    pub(super) fn submit(&mut self, candidate: Candidate) -> Result<Landing, LandError> {
        let store = self.queue.store;
        let _held = store.hold_objects()?;
        if !candidate.trees.is_empty() {
            store.write_all(&candidate.trees)?;
        }
        // What a landing on `policy` lands governs the next one, which
        // this node must still be the authority of.
        let landed_policy = if self.onto == TargetName::POLICY {
            self.queue.authorize(&self.policy, &self.onto)?;
            Some(Queue::check_policy(store, candidate.state)?)
        } else {
            None
        };
        let mut records: Vec<EncodedObject> = candidate.revision.into_iter().collect();
        // A landing that changes nothing, which only records another
        // target's tip, needs no evidence.
        let requirements = if candidate.state == self.state {
            Vec::new()
        } else {
            self.policy.requirements(&self.onto).to_vec()
        };
        let mut evidence = Vec::new();
        let mut missing = Vec::new();
        if !requirements.is_empty() {
            // Promotion reads the revision.
            store.write_all(&records)?;
            records.clear();
            let promotions = self
                .annotations
                .promotions(candidate.subject, &self.queue.principal())?;
            let mut claims = self.annotations.claims(candidate.subject)?;
            for promotion in promotions {
                let encoded = promotion.encode();
                let claim = store.claim(encoded.id().into(), promotion)?;
                claims.push(claim.map_err(AnnotateError::from)?);
                records.push(encoded);
            }
            for requirement in requirements {
                match self.evidence(&claims, &requirement) {
                    Some(id) if !evidence.contains(&id) => evidence.push(id),
                    Some(_) => {}
                    None => missing.push(requirement),
                }
            }
        }
        if !missing.is_empty() {
            store.write_all(&records)?;
            let pending = Pending {
                onto: self.onto.clone(),
                proposal: candidate.kept_as,
                subject: candidate.subject,
                blocked: Blocked::Evidence(missing),
            };
            self.keep(&pending)?;
            return Ok(Landing::Pending(pending));
        }

        let acceptance = AcceptanceBody {
            target: self.onto.id(),
            seq: self.tip.seq + 1,
            prev: Some(self.tip.acceptance),
            state: candidate.state,
            landed: vec![candidate.source],
            evidence,
            policy: Some(self.policy_head),
        }
        .sign(self.queue.key);
        let encoded = acceptance.encode();
        if Policy::governing(encoded.id().into(), acceptance.body())? != self.policy_head {
            return Err(PolicyError::NotPredecessor(acceptance.body().seq).into());
        }
        records.push(encoded);
        store.write_all(&records)?;
        let id = store.append(&self.onto, &acceptance)?;
        self.tip = Head {
            seq: acceptance.body().seq,
            acceptance: id,
        };
        self.state = candidate.state;
        if let Some(policy) = landed_policy {
            self.policy = policy;
            self.policy_head = id;
        }
        self.forget(&candidate.proposal)?;
        Ok(Landing::Landed {
            proposal: candidate.proposal,
            source: candidate.source,
            acceptance: self.tip,
        })
    }

    /// The first of `claims` that meets `requirement`: of the kind and
    /// verdict required, from an issuer the policy admits, which for a
    /// promotion is its original's.
    fn evidence(&self, claims: &[Claim], requirement: &Requirement) -> Option<StatementId> {
        claims
            .iter()
            .find(|claim| {
                let claimed = claim.claimed();
                claimed.kind == requirement.kind
                    && claimed.verdict == Some(requirement.verdict)
                    && self.policy.admits(&requirement.issuer, &claimed.issuer)
            })
            .map(Claim::id)
    }

    /// Blocks a landing on `conflicts`: issues a `core.conflict` statement
    /// about `subject` listing them, unless this node's latest one says
    /// the same, and keeps the landing waiting.
    pub(super) fn block(
        &mut self,
        proposal: Proposal,
        subject: RecordId,
        conflicts: &Conflicts,
    ) -> Result<Landing, LandError> {
        let issuer = self.queue.principal();
        let current = self
            .annotations
            .current(subject, &Kind::CONFLICT, &issuer)?;
        let (payload, conflict) = conflicts.statement(subject, issuer, current);
        let unchanged = match current {
            Some(id) => self.store().get(id)?.payload == conflict.payload,
            None => false,
        };
        let statement = match current {
            Some(id) if unchanged => id,
            _ => self.annotations.issue(&conflict, Some(payload))?,
        };
        let pending = Pending {
            onto: self.onto.clone(),
            proposal,
            subject,
            blocked: Blocked::Conflict {
                statement,
                paths: conflicts.paths(),
            },
        };
        self.keep(&pending)?;
        Ok(Landing::Pending(pending))
    }

    /// The file keeping a waiting landing of `proposal`.
    fn pending_path(&self, proposal: &Proposal) -> Result<PathBuf, StoreError> {
        let name = match proposal {
            Proposal::Change(change) => format!("change-{change}"),
            Proposal::Target(target) => format!("target-{}", target.id()),
        };
        Ok(self.queue.queue_dir(self.onto.id())?.join(name))
    }

    /// Durably keeps a waiting landing, replacing the last attempt's, and
    /// logs that it waits.
    fn keep(&self, pending: &Pending) -> Result<(), StoreError> {
        let write = self.store().durable_write();
        write.stage(
            self.pending_path(&pending.proposal)?,
            PendingFile::encode(pending).as_bytes(),
        )?;
        let waiting = Effect::Waiting {
            onto: pending.onto.clone(),
            proposal: pending.proposal.clone(),
        };
        self.store().commit_logged(write, &[waiting])?;
        Ok(())
    }

    /// Durably forgets a landing that waited, if one did.
    pub(super) fn forget(&self, proposal: &Proposal) -> Result<(), StoreError> {
        let path = self.pending_path(proposal)?;
        match fs::remove_file(&path) {
            Ok(()) => self.store().sync_dir(path.parent().unwrap_or(&path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::io(&path, source)),
        }
    }

    /// The history of `tips`, the target's and another target's, to merge
    /// that one from: the state of the newest acceptance both histories
    /// share, or the [merge](Ancestry::merge_base) of several, if each
    /// lacks another, as after targets landed each other. An acceptance's
    /// history is the one before it, and the acceptances of other targets
    /// it landed: a target's first lands the one it was created from, so
    /// targets created from the same one share it.
    ///
    /// The nearest shared acceptance can be an older one, through a target
    /// that forked early and landed recently, so both histories are read
    /// whole, each acceptance once.
    pub(super) fn history(
        &self,
        tips: &[AcceptanceId],
    ) -> Result<Ancestry<AcceptanceId>, StoreError> {
        Ancestry::walk(tips, |id| {
            let acceptance = self.store().get(id)?;
            let body = acceptance.body();
            let landed = body.landed.iter().filter_map(|source| match *source {
                Source::Target(landed) => Some(landed),
                Source::Revision(_) => None,
            });
            Ok::<_, StoreError>((landed.chain(body.prev).collect(), body.state))
        })
    }
}
