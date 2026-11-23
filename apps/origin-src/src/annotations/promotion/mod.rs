//! Promotion: carrying results over to records with the same input digest.
//!
//! A promotion claims what its original claims, but counts only once
//! [`Store::claim`] has checked it against the original; only that check
//! makes a [`Claim`]. Results carry over within a target, never from one
//! target to another, since their checks may differ. Readers, such as the
//! landing queue, read claims through [`Annotations::claims`].
//!
//! Promotion runs only when asked: `origin promote`, and the landing queue
//! when it checks evidence, call [`Annotations::promote`].

use std::collections::HashSet;

use origin_platform::Platform;
use origin_shared::api::Annotation;
use origin_shared::{
    Acceptance, Base, Principal, Record, RecordId, Statement, StatementId, TargetId, TreeId,
};

use super::Annotations;
use crate::store::{Store, StoreError};
use crate::tree::Snapshots;

/// The claim a statement makes, checked: only [`Store::claim`] makes one.
/// A statement claims for itself, unless it promotes another. Then it makes
/// its original's claim, kind, verdict and issuer included, which holds
/// because their subjects' input digests are equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    id: StatementId,
    statement: Statement,
    /// The original a promotion carries, checked.
    original: Option<(StatementId, Statement)>,
}

impl Claim {
    /// The statement making the claim, which evidence cites.
    #[must_use]
    pub fn id(&self) -> StatementId {
        self.id
    }

    /// The statement making the claim.
    #[must_use]
    pub fn statement(&self) -> &Statement {
        &self.statement
    }

    /// What is claimed: the statement, or for a promotion its original,
    /// whose kind, verdict and issuer count.
    #[must_use]
    pub fn claimed(&self) -> &Statement {
        self.original
            .as_ref()
            .map_or(&self.statement, |(_, original)| original)
    }

    /// The original the statement promotes, if it's a promotion.
    #[must_use]
    pub fn original(&self) -> Option<StatementId> {
        self.original.as_ref().map(|&(id, _)| id)
    }
}

impl<P: Platform> Annotations<'_, P> {
    /// The claims about `subject` that hold, in arrival order: those of
    /// the statements nothing superseded, that [`Store::claim`] accepts with
    /// the records this node has, and for a promotion, whose original
    /// nothing superseded either, and that no statement about `subject`
    /// of the kind from the original's issuer replaces, whatever came
    /// first: a result that ran there wins, and so does one taken back
    /// there (see [`Statement::drop_replaced_promotions`]).
    ///
    /// # Errors
    ///
    /// Fails if a record or a tree cannot be read.
    pub fn claims(&self, subject: RecordId) -> Result<Vec<Claim>, StoreError> {
        self.claims_of(&self.listed(subject)?)
    }

    /// Returns the claims that hold among `listed`, the statements about one
    /// record, as [`Annotations::claims`] decides.
    ///
    /// # Errors
    ///
    /// Fails if a record or a tree cannot be read.
    pub(super) fn claims_of(&self, listed: &[Annotation]) -> Result<Vec<Claim>, StoreError> {
        let mut claims = Vec::new();
        for annotation in listed {
            if annotation.superseded_by.is_some() {
                continue;
            }
            let claim = match self
                .store
                .claim(annotation.id, annotation.statement.clone())
            {
                Ok(Ok(claim)) => claim,
                // A clone may lack the trees a promotion is checked with.
                Ok(Err(_)) | Err(StoreError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            if let Some((id, original)) = &claim.original
                && self.superseded_by(*id, original)?.is_some()
            {
                continue;
            }
            claims.push(claim);
        }
        Statement::drop_replaced_promotions(&mut claims, |claim| {
            (&claim.statement, claim.claimed())
        });
        Ok(claims)
    }

    /// Computes and remembers the input digest of every promotable
    /// statement the index has none for, from every core. Those whose
    /// subject or trees aren't in the store yet are left for a later search.
    fn compute_digests(&mut self) -> Result<(), StoreError> {
        let missing: Vec<StatementId> = self
            .index
            .promotable()
            .flat_map(|(_, statements)| statements)
            .filter(|&&id| self.index.digest(id).is_none())
            .copied()
            .collect();
        let chunks = Store::<P>::on_every_core(&missing, |ids| {
            let mut snapshots = Snapshots::new(self.store);
            let mut digests = Vec::new();
            for &id in ids {
                let statement = self.store.get(id)?;
                let Some(inputs) = &statement.inputs else {
                    continue;
                };
                let digest = match self.store.input_trees(statement.subject) {
                    Ok(Some(mut trees)) => snapshots.input_digest(&mut trees, inputs),
                    Ok(None) => Ok(None),
                    Err(error) => Err(error),
                };
                match digest {
                    Ok(Some(digest)) => digests.push((id, digest)),
                    Ok(None) | Err(StoreError::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(digests)
        });
        let digests = chunks.into_iter().collect::<Result<Vec<_>, _>>()?;
        self.index.remember_digests(self.store, &digests.concat())
    }

    /// Returns the statements that would promote results to `subject`, as
    /// `issuer` would issue them, without writing them, sorted by kind. For
    /// each [promotion group](Statement::promotion_group) the subject has no
    /// result of, it picks the latest unsuperseded statement with the subject's
    /// input digest, about another record on the same target that `sees`
    /// says the caller can read.
    ///
    /// # Errors
    ///
    /// Fails if a record or tree cannot be read, or the digests cannot be saved.
    pub fn promotions(
        &mut self,
        subject: RecordId,
        issuer: &Principal,
        mut sees: impl FnMut(RecordId) -> Result<bool, StoreError>,
    ) -> Result<Vec<Statement>, StoreError> {
        let (Some(mut trees), Some(target)) = (
            self.store.input_trees(subject)?,
            self.store.target_of(subject)?,
        ) else {
            return Ok(Vec::new());
        };
        self.compute_digests()?;

        // The results the revision has that hold, by kind and original
        // issuer.
        let held: HashSet<_> = self
            .claims(subject)?
            .iter()
            .map(|claim| (claim.claimed().kind.clone(), claim.claimed().issuer.clone()))
            .collect();

        let mut snapshots = Snapshots::new(self.store);
        let mut promotions = Vec::new();
        for (_, statements) in self.index.promotable() {
            // A group shares its kind, issuer and inputs.
            let first = self.store.get(statements[0])?;
            let Some(inputs) = &first.inputs else {
                continue;
            };
            if held.contains(&(first.kind.clone(), first.issuer.clone())) {
                continue;
            }
            let Some(digest) = snapshots.input_digest(&mut trees, inputs)? else {
                continue;
            };
            for &id in statements.iter().rev() {
                if self.index.digest(id) != Some(digest) {
                    continue;
                }
                let statement = self.store.get(id)?;
                if self.superseded_by(id, &statement)?.is_none()
                    && self.store.target_of(statement.subject)? == Some(target)
                    && sees(statement.subject)?
                {
                    promotions.push(Statement {
                        subject,
                        issuer: issuer.clone(),
                        supersedes: None,
                        promoted_from: Some(id),
                        ..statement
                    });
                    break;
                }
            }
        }
        promotions
            .sort_unstable_by(|a, b| (&a.kind, a.promoted_from).cmp(&(&b.kind, b.promoted_from)));
        Ok(promotions)
    }

    /// Issues the [`promotions`](Self::promotions) to `subject` as `issuer`,
    /// for a caller who `sees` some records, and returns them. A promotion
    /// refers to its original's payload without writing it, so this [holds the
    /// objects](Store::hold_objects) from finding the originals until the
    /// promotions are written.
    ///
    /// # Errors
    ///
    /// Fails like [`Annotations::promotions`], or if the statements cannot be
    /// written.
    pub fn promote(
        &mut self,
        subject: RecordId,
        issuer: &Principal,
        sees: impl FnMut(RecordId) -> Result<bool, StoreError>,
    ) -> Result<Vec<(StatementId, Statement)>, StoreError> {
        let _held = self.store.hold_objects()?;
        let promotions = self.promotions(subject, issuer, sees)?;
        let encoded: Vec<_> = promotions.iter().map(Statement::encode).collect();
        self.store.write_all(&encoded)?;
        Ok(encoded
            .iter()
            .map(|encoded| encoded.id().into())
            .zip(promotions)
            .collect())
    }
}

impl<P: Platform> Store<P> {
    /// Checks the claim that `statement`, with id `id`, makes: its own, unless
    /// it promotes another. A promotion makes its original's claim if the
    /// original is stored and [promotable](Statement::is_promotable), their
    /// kind, verdict, inputs and payload match, and the subjects share a
    /// [target](Store::target_of) and an input digest. Otherwise the inner
    /// error says why. Readers check supersession, see [`Annotations::claims`].
    ///
    /// # Errors
    ///
    /// Fails if a record or a tree cannot be read.
    pub fn claim(
        &self,
        id: StatementId,
        statement: Statement,
    ) -> Result<Result<Claim, InvalidPromotion>, StoreError> {
        let Some(original_id) = statement.promoted_from else {
            return Ok(Ok(Claim {
                id,
                statement,
                original: None,
            }));
        };
        let original: Statement = match self.get(original_id) {
            Ok(original) => original,
            Err(StoreError::NotFound(_)) => {
                return Ok(Err(InvalidPromotion::UnknownOriginal(original_id)));
            }
            Err(error) => return Err(error),
        };
        if !original.is_promotable() {
            return Ok(Err(InvalidPromotion::NotPromotable(original_id)));
        }
        let differs = [
            ("kind", statement.kind != original.kind),
            ("verdict", statement.verdict != original.verdict),
            ("inputs", statement.inputs != original.inputs),
            ("payload", statement.payload != original.payload),
        ];
        if let Some(&(field, _)) = differs.iter().find(|(_, differs)| *differs) {
            return Ok(Err(InvalidPromotion::Differs(field)));
        }
        let target = self.target_of(statement.subject)?;
        if target.is_none() || target != self.target_of(original.subject)? {
            return Ok(Err(InvalidPromotion::OtherTarget));
        }
        let (Some(mut here), Some(mut there), Some(inputs)) = (
            self.input_trees(statement.subject)?,
            self.input_trees(original.subject)?,
            &original.inputs,
        ) else {
            return Ok(Err(InvalidPromotion::NoSnapshot));
        };
        let mut snapshots = Snapshots::new(self);
        let here = snapshots.input_digest(&mut here, inputs)?;
        if here.is_none() || here != snapshots.input_digest(&mut there, inputs)? {
            return Ok(Err(InvalidPromotion::InputsDiffer));
        }
        Ok(Ok(Claim {
            id,
            statement,
            original: Some((original_id, original)),
        }))
    }

    /// Returns the target `record` is on, for promotion: an acceptance's own
    /// target, or for a revision, the target of the acceptance it is based on,
    /// following the revisions below it. Returns `None` for a snapshot, a
    /// statement, or a record whose base this store lacks.
    ///
    /// # Errors
    ///
    /// Fails if a record cannot be read.
    pub fn target_of(&self, record: RecordId) -> Result<Option<TargetId>, StoreError> {
        let base = match record {
            RecordId::Acceptance(id) => Base::Acceptance(id),
            RecordId::Revision(id) => Base::Revision(id),
            RecordId::Snapshot(_) | RecordId::Statement(_) => return Ok(None),
        };
        Ok(self
            .base_acceptance(base)?
            .map(|acceptance| acceptance.body().target))
    }

    /// Returns the state of the target that a revision based on `base` is on:
    /// the state of the acceptance `base` leads to, as [`Store::target_of`]
    /// finds it. Returns `None` if the store lacks a record on the way.
    ///
    /// # Errors
    ///
    /// Fails if a record cannot be read.
    pub fn base_state(&self, base: Base) -> Result<Option<TreeId>, StoreError> {
        Ok(self
            .base_acceptance(base)?
            .map(|acceptance| acceptance.body().state))
    }

    /// Returns the acceptance `base` leads to, following the revisions below
    /// it, or `None` if the store lacks a record on the way.
    fn base_acceptance(&self, mut base: Base) -> Result<Option<Acceptance>, StoreError> {
        let missing = |error| match error {
            StoreError::NotFound(_) => Ok(None),
            error => Err(error),
        };
        loop {
            base = match base {
                Base::Acceptance(id) => return self.get(id).map(Some).or_else(missing),
                Base::Revision(below) => match self.get(below) {
                    Ok(below) => below.base,
                    Err(error) => return missing(error),
                },
            };
        }
    }
}

/// Why a promotion makes no claim.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidPromotion {
    /// The store doesn't have the original.
    #[error("it promotes statement {}, which this repository doesn't have", .0.short())]
    UnknownOriginal(StatementId),
    /// The original declares no inputs, depends on external ones, is
    /// reserved, about a statement, or a promotion itself.
    #[error("it promotes statement {}, which isn't promotable", .0.short())]
    NotPromotable(StatementId),
    /// A field differs from the original's.
    #[error("its {0} differs from its original's")]
    Differs(&'static str),
    /// Its subject is a statement, which has no snapshot.
    #[error("it is about a statement, which has no inputs")]
    NoSnapshot,
    /// Its subject's input digest differs from the original's subject's.
    #[error("its subject's inputs differ from its original's")]
    InputsDiffer,
    /// Its subject is not on the same target as its original's subject.
    #[error("its subject isn't on the target its original's is on")]
    OtherTarget,
}

#[cfg(test)]
mod tests;
