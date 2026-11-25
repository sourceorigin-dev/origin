//! Statements: issuing them, reading them, promoting them by input digest,
//! and dropping their payloads once their retention ends.
//!
//! Core acts only on the reserved kinds, [`Kind::ABANDON`],
//! [`Kind::CONFLICT`] and [`Kind::DIVERGENT`], and only when they come from
//! the issuers [`Store::authorizes`] allows. Any other kind means whatever
//! its issuer means by it. Supersession only counts within one issuer: if a
//! statement names another issuer's statement as the one it supersedes,
//! that statement still stands. [`Annotations::supersede`] supersedes an
//! issuer's current statement atomically, so two calls at once on one store
//! never supersede the same statement.

mod promotion;
mod retention;

use std::collections::HashSet;

use origin_platform::{Native, Platform};
use origin_shared::api::Annotation;
use origin_shared::{
    ChangeId, EncodedObject, Kind, Principal, Record, RecordId, RevisionId, Statement, StatementId,
};

use crate::index::Index;
use crate::store::{Store, StoreError};

pub use promotion::{Claim, InvalidPromotion};
pub use retention::Dropped;

/// The statements of a store, through its index.
#[derive(Debug)]
pub struct Annotations<'a, P = Native> {
    store: &'a Store<P>,
    index: Index,
}

impl<'a, P: Platform> Annotations<'a, P> {
    /// The statements of `store`, as `index` knows them.
    pub fn new(store: &'a Store<P>, index: Index) -> Self {
        Self { store, index }
    }

    /// The statements of `store`, opening its index.
    ///
    /// # Errors
    ///
    /// Fails like [`Index::open`].
    pub fn open(store: &'a Store<P>) -> Result<Self, StoreError> {
        Ok(Self::new(store, Index::open(store)?))
    }

    /// The index the statements are found through.
    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Returns the index, so the next user can reuse it.
    #[must_use]
    pub fn into_index(self) -> Index {
        self.index
    }

    /// Brings the index up to date with the records written since it was read.
    /// See [`Index::refresh`].
    ///
    /// # Errors
    ///
    /// Fails like [`Index::refresh`].
    pub fn refresh(&mut self) -> Result<(), StoreError> {
        self.index.refresh(self.store)
    }

    /// Writes a statement and its payload. Issuing a statement the store
    /// has is a no-op.
    ///
    /// # Errors
    ///
    /// Fails if the statement supersedes one from another issuer, is a
    /// promotion that [`Store::claim`] refuses, is of a reserved kind its
    /// issuer may not issue, or the records cannot be read or written.
    pub fn issue(
        &self,
        statement: &Statement,
        payload: Option<EncodedObject>,
    ) -> Result<StatementId, AnnotateError> {
        if !self.store.authorizes(statement)? {
            return Err(AnnotateError::Reserved {
                kind: statement.kind.clone(),
                issuer: statement.issuer.clone(),
            });
        }
        if statement.promoted_from.is_some() {
            self.store
                .claim(statement.encode().id().into(), statement.clone())??;
        }
        if let Some(superseded) = statement.supersedes {
            let issuer = self.store.get(superseded)?.issuer;
            if issuer != statement.issuer {
                return Err(AnnotateError::ForeignSupersession {
                    superseded,
                    issuer,
                    by: statement.issuer.clone(),
                });
            }
        }
        Ok(self.write(statement, payload)?)
    }

    /// Issues `statement` as its issuer's latest of its kind about its
    /// subject, superseding the [current](Self::current) one, and returns
    /// it with its id. One without a verdict, of a kind that isn't
    /// reserved, withdraws the current one. It reads the current one and
    /// issues under a lock, from the index read again, so two statements
    /// issued at once in this store never supersede the same one: the
    /// second supersedes the first. Statements issued in other stores and
    /// put here chose what they supersede there, so two of one issuer's can
    /// still both supersede the same one. Afterwards these annotations hold
    /// the index as read before the issue, without the new statement.
    ///
    /// # Errors
    ///
    /// Fails like [`Annotations::issue`], or with
    /// [`AnnotateError::NothingToWithdraw`] if a withdrawal finds no
    /// current statement with a verdict.
    pub fn supersede(
        &mut self,
        mut statement: Statement,
        payload: Option<EncodedObject>,
    ) -> Result<(StatementId, Statement), AnnotateError> {
        let _lock = self.store.lock_supersession()?;
        self.index = Index::open(self.store)?;
        statement.supersedes =
            self.current(statement.subject, &statement.kind, &statement.issuer)?;
        if statement.verdict.is_none() && !statement.kind.is_reserved() {
            // A withdrawal takes back a statement with a verdict.
            let withdrawable = match statement.supersedes {
                Some(current) => !self.store.get(current)?.withdraws(),
                None => false,
            };
            if !withdrawable {
                return Err(AnnotateError::NothingToWithdraw {
                    kind: statement.kind,
                    subject: statement.subject,
                });
            }
        }
        let id = self.issue(&statement, payload)?;
        Ok((id, statement))
    }

    /// Writes `payload`, then `statement`, and returns the statement's id. The
    /// payload is durable before anything refers to it. It holds objects while
    /// writing, so garbage collection cannot delete the payload before the
    /// statement refers to it.
    fn write(
        &self,
        statement: &Statement,
        payload: Option<EncodedObject>,
    ) -> Result<StatementId, StoreError> {
        let _held = self.store.hold_objects()?;
        if let Some(payload) = payload {
            self.store.write_all(&[payload])?;
        }
        let encoded = statement.encode();
        self.store.write_all(std::slice::from_ref(&encoded))?;
        Ok(encoded.id().into())
    }

    /// The first statement superseding `statement`, `id`, that its issuer
    /// issued.
    fn superseded_by(
        &self,
        id: StatementId,
        statement: &Statement,
    ) -> Result<Option<StatementId>, StoreError> {
        for &superseder in self.index.superseders(id) {
            if self.store.get(superseder)?.issuer == statement.issuer {
                return Ok(Some(superseder));
            }
        }
        Ok(None)
    }

    /// Returns the statements about `subject`, in arrival order, each marked
    /// with whether its claim holds, as [`Annotations::claims`] decides. This
    /// lets readers such as CI runners count results the same way the landing
    /// queue does.
    ///
    /// # Errors
    ///
    /// Fails if a statement, or a record or tree needed to check the claims,
    /// cannot be read.
    pub fn about(&self, subject: RecordId) -> Result<Vec<Annotation>, StoreError> {
        let mut listed = self.listed(subject)?;
        let holding: HashSet<StatementId> =
            self.claims_of(&listed)?.iter().map(Claim::id).collect();
        for annotation in &mut listed {
            annotation.holds = holding.contains(&annotation.id);
        }
        Ok(listed)
    }

    /// Returns the statements about `subject`, in arrival order, without
    /// checking which ones hold.
    ///
    /// # Errors
    ///
    /// Fails if a statement cannot be read.
    fn listed(&self, subject: RecordId) -> Result<Vec<Annotation>, StoreError> {
        self.index
            .statements(subject)
            .iter()
            .map(|&id| {
                let statement = self.store.get(id)?;
                Ok(Annotation {
                    id,
                    superseded_by: self.superseded_by(id, &statement)?,
                    statement,
                    holds: false,
                })
            })
            .collect()
    }

    /// The latest statement of `kind` from `issuer` about `subject` that
    /// nothing superseded: the one a new such statement supersedes.
    ///
    /// # Errors
    ///
    /// Fails if a statement cannot be read.
    pub fn current(
        &self,
        subject: RecordId,
        kind: &Kind,
        issuer: &Principal,
    ) -> Result<Option<StatementId>, StoreError> {
        Ok(self
            .listed(subject)?
            .into_iter()
            .rev()
            .find(|annotation| {
                annotation.superseded_by.is_none()
                    && annotation.statement.kind == *kind
                    && annotation.statement.issuer == *issuer
            })
            .map(|annotation| annotation.id))
    }

    /// Issues a `core.divergent` statement as `issuer` if `change` has more
    /// than one head, returning it. It is about the head with the smallest
    /// id, and its payload lists every head, one id per line, so the same
    /// heads always make the same statement: flagging them again writes
    /// nothing.
    ///
    /// # Errors
    ///
    /// Fails if the statement cannot be written.
    pub fn flag_divergence(
        &self,
        change: ChangeId,
        issuer: Principal,
    ) -> Result<Option<StatementId>, StoreError> {
        let mut heads = self.index.heads(change);
        if heads.len() < 2 {
            return Ok(None);
        }
        heads.sort_unstable();
        let list = heads
            .iter()
            .map(|head| head.to_hex() + "\n")
            .collect::<String>();
        let payload = EncodedObject::blob(list.as_bytes());
        let statement = Statement {
            subject: RecordId::Revision(heads[0]),
            kind: Kind::DIVERGENT,
            verdict: None,
            issuer,
            inputs: None,
            payload: Some(payload.id().into()),
            supersedes: None,
            promoted_from: None,
        };
        self.write(&statement, Some(payload)).map(Some)
    }
}

impl<P: Platform> Store<P> {
    /// Whether `statement`'s issuer may issue a statement of its kind, if
    /// that kind is reserved:
    ///
    /// - `core.abandon`, about a revision, only the change's creator: the
    ///   author of its first revision, so no one who took it over, or
    ///   rebased it, gives it up for them;
    /// - `core.conflict`, a node, as a landing queue that could not rebase
    ///   the record, or the author of the revision it's about, listing
    ///   the conflicts its files still have;
    /// - `core.divergent`, a node.
    ///
    /// Reserved kinds are never promoted. Other kinds are anyone's, as far
    /// as core is concerned.
    ///
    /// # Errors
    ///
    /// Fails if a revision cannot be read.
    pub fn authorizes(&self, statement: &Statement) -> Result<bool, StoreError> {
        let issuer = &statement.issuer;
        if !statement.kind.is_reserved() {
            return Ok(true);
        }
        if statement.promoted_from.is_some() {
            return Ok(false);
        }
        let is_node = matches!(issuer, Principal::Node(_));
        let RecordId::Revision(subject) = statement.subject else {
            return Ok(statement.kind == Kind::CONFLICT && is_node);
        };
        let kind = &statement.kind;
        Ok(if *kind == Kind::ABANDON {
            self.created(subject, issuer)?
        } else if *kind == Kind::CONFLICT {
            is_node || self.get(subject)?.author == *issuer
        } else {
            *kind == Kind::DIVERGENT && is_node
        })
    }

    /// Whether `author` created the change of `revision`: made each
    /// revision it evolves from that evolves from none. Records being
    /// immutable, there is always one, and no one adds another later.
    fn created(&self, revision: RevisionId, author: &Principal) -> Result<bool, StoreError> {
        let mut seen = HashSet::new();
        let mut next = vec![revision];
        while let Some(id) = next.pop() {
            if !seen.insert(id) {
                continue;
            }
            let revision = self.get(id)?;
            if revision.predecessors.is_empty() && revision.author != *author {
                return Ok(false);
            }
            next.extend(revision.predecessors);
        }
        Ok(true)
    }
}

/// A failure to issue a statement.
#[derive(Debug, thiserror::Error)]
pub enum AnnotateError {
    /// The statement supersedes one from another issuer.
    #[error("{by} cannot supersede statement {}, which {issuer} issued", superseded.short())]
    ForeignSupersession {
        /// The statement it supersedes.
        superseded: StatementId,
        /// Who issued that one.
        issuer: Principal,
        /// Who issues the new one.
        by: Principal,
    },
    /// A promotion doesn't match its original.
    #[error("invalid promotion")]
    InvalidPromotion(#[from] InvalidPromotion),
    /// A reserved kind from an issuer that may not issue it.
    #[error("{issuer} may not issue `{kind}` statements about this record")]
    Reserved {
        /// The kind.
        kind: Kind,
        /// Who issued it.
        issuer: Principal,
    },
    /// A withdrawal found no statement of its issuer's to take back.
    #[error(
        "you have no `{kind}` statement about {} to withdraw",
        subject.describe()
    )]
    NothingToWithdraw {
        /// The kind.
        kind: Kind,
        /// What it would be about: a revision or a statement.
        subject: RecordId,
    },
    /// Records cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests;
