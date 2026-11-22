//! Landing one candidate: rebasing it onto the target's tip, and appending
//! its acceptance, or keeping it to wait on a conflict.

use std::collections::HashSet;

use origin_platform::{LockedFile, Platform};
use origin_shared::{
    AcceptanceBody, AcceptanceId, Base, ChangeId, EncodedObject, Kind, Record, RecordId, Revision,
    RevisionId, Source, TargetName, TreeId,
};

use crate::annotations::Annotations;
use crate::conflict::Conflicts;
use crate::store::{Store, StoreError};
use crate::targets::Head;

use super::{Blocked, LandError, Landing, Pending, Proposal, Queue};

/// Landings onto one target, holding its queue's lock.
pub(super) struct Session<'q, 'a, P> {
    pub(super) queue: &'q Queue<'a, P>,
    pub(super) onto: TargetName,
    /// The acceptance of `policy` holding the policy in force.
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
    pub(super) source: Source,
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
            if self.has_landed(head, &revision)? {
                break;
            }
            if let Base::Revision(below) = revision.base {
                next = Some(self.store().get(below)?.change);
            }
            stack.push((head, revision));
        }
        stack.reverse();
        Ok(stack)
    }

    /// Whether `revision`, `id`, landed on the target. A revision only lands
    /// on the tip it's based on, so the acceptance after that tip tells.
    fn has_landed(&self, id: RevisionId, revision: &Revision) -> Result<bool, StoreError> {
        let Base::Acceptance(base) = revision.base else {
            return Ok(false);
        };
        let base = self.store().get(base)?;
        if base.body().target != self.onto.id() {
            return Ok(false);
        }
        let Some(next) = self
            .store()
            .acceptance_at(&self.onto, base.body().seq + 1)?
        else {
            return Ok(false);
        };
        Ok(self
            .store()
            .get(next)?
            .body()
            .landed
            .contains(&Source::Revision(id)))
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
            return Ok(Landing::Unchanged(proposal));
        }
        let tip = Base::Acceptance(self.tip.acceptance);
        if revision.base == tip {
            return self.submit(Candidate {
                proposal,
                source: Source::Revision(id),
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
            source: Source::Revision(rebased_id),
            state: merge.tree,
            trees: merge.trees,
            revision: Some(rebased),
        })
    }

    /// Lands `candidate`. Writes the records in layers, each durable before
    /// the next refers to it: the trees, then the rebased revision and the
    /// acceptance.
    pub(super) fn submit(&mut self, candidate: Candidate) -> Result<Landing, LandError> {
        let store = self.queue.store;
        if !candidate.trees.is_empty() {
            store.write_all(&candidate.trees)?;
        }
        let mut records: Vec<EncodedObject> = candidate.revision.into_iter().collect();
        let acceptance = AcceptanceBody {
            target: self.onto.id(),
            seq: self.tip.seq + 1,
            prev: Some(self.tip.acceptance),
            state: candidate.state,
            landed: vec![candidate.source],
            evidence: Vec::new(),
            policy: Some(self.policy_head),
        }
        .sign(self.queue.key);
        records.push(acceptance.encode());
        store.write_all(&records)?;
        let id = store.append(&self.onto, &acceptance)?;
        self.tip = Head {
            seq: acceptance.body().seq,
            acceptance: id,
        };
        self.state = candidate.state;
        Ok(Landing::Landed {
            proposal: candidate.proposal,
            source: candidate.source,
            acceptance: self.tip,
        })
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
        Ok(Landing::Pending(pending))
    }
}
