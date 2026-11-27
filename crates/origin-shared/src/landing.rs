//! Landings: what a target's queue is asked to land, and the outcome.

use std::fmt;

use crate::policy::Requirement;
use crate::{ChangeId, Head, RecordId, RepoPath, Source, StatementId, TargetName};

/// What a landing lands.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Proposal {
    /// A change, as of its head.
    Change(ChangeId),
    /// Another target, as of its tip.
    Target(TargetName),
}

/// The outcome of a landing.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Landing {
    /// It landed.
    Landed {
        /// What landed.
        proposal: Proposal,
        /// The revision or acceptance the acceptance names.
        source: Source,
        /// The acceptance.
        acceptance: Head,
    },
    /// The target already has it, so nothing landed.
    Unchanged(Proposal),
    /// It waits, and the queue keeps it.
    Pending(Pending),
}

/// A landing that waits, kept in the queue to be listed and retried.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Pending {
    /// The target it lands on.
    pub onto: TargetName,
    /// What lands.
    pub proposal: Proposal,
    /// The record the evidence must be about: the revision that would land, or
    /// for a target, the revision of the merge on the tip it lands on. For a
    /// conflict, the revision or acceptance that didn't merge.
    pub subject: RecordId,
    /// Why it waits.
    pub blocked: Blocked,
    /// When the submitter's token expires, in seconds since the Unix epoch, if
    /// it is a scoped token. The queue retries the landing on the submitter's
    /// behalf until then. After that, the landing waits until it is submitted
    /// again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
}

/// Why a landing waits.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Blocked {
    /// The subject lacks these statements, which the target requires.
    Evidence(Vec<Requirement>),
    /// Both sides changed these paths. The `core.conflict` statement lists
    /// them too.
    Conflict {
        /// The statement.
        statement: StatementId,
        /// The paths, sorted.
        paths: Vec<RepoPath>,
    },
}

impl Pending {
    /// Returns whether the submitter's token expired, so the landing is no
    /// longer retried.
    #[must_use]
    pub fn expired(&self) -> bool {
        self.expires
            .is_some_and(|expires| expires <= crate::access::Token::now())
    }
}

impl Landing {
    /// Returns what the landing lands.
    #[must_use]
    pub fn proposal(&self) -> &Proposal {
        match self {
            Self::Landed { proposal, .. }
            | Self::Unchanged(proposal)
            | Self::Pending(Pending { proposal, .. }) => proposal,
        }
    }
}

impl fmt::Display for Proposal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Change(change) => write!(f, "change {}", change.short()),
            Self::Target(target) => write!(f, "`{target}`"),
        }
    }
}
