//! The file that keeps a waiting landing, in the queue's directory, until
//! the landing can continue.

use origin_shared::RecordId;
use origin_shared::landing::{Blocked, Pending, Proposal};
use origin_shared::policy::Requirement;

use crate::permissions::{Caller, Scope};

use super::Waiting;

/// The format of the files that keep waiting landings.
pub(super) struct PendingFile;

impl PendingFile {
    /// The first line of every file keeping a waiting landing.
    const HEADER: &str = "origin pending 1";

    /// Encodes the landing as lines: the header, `onto <target>`, `change <id>`
    /// or `target <name>`, `subject <kind> <id>`, and `by owner` or
    /// `by <principal>` for its submitter. If the submitter's token is scoped,
    /// an `expires <seconds>` line and a `scope <capability>` line for each of
    /// its capabilities follow. Then either a
    /// `missing <kind> <verdict> <issuer>` line for each requirement the
    /// landing lacks, or a `conflict <statement>` line naming the statement
    /// whose payload lists the conflicting paths.
    pub(super) fn encode(pending: &Pending, submitter: &Caller) -> String {
        let mut lines = vec![Self::HEADER.to_owned(), format!("onto {}", pending.onto)];
        lines.push(match &pending.proposal {
            Proposal::Change(change) => format!("change {change}"),
            Proposal::Target(target) => format!("target {target}"),
        });
        let (kind, id) = match pending.subject {
            RecordId::Snapshot(id) => ("snapshot", id.to_hex()),
            RecordId::Revision(id) => ("revision", id.to_hex()),
            RecordId::Statement(id) => ("statement", id.to_hex()),
            RecordId::Acceptance(id) => ("acceptance", id.to_hex()),
        };
        lines.push(format!("subject {kind} {id}"));
        match submitter {
            Caller::Principal { principal, scope } => {
                lines.push(format!("by {principal}"));
                if let Some(scope) = scope {
                    lines.push(format!("expires {}", scope.expires));
                    let capabilities = scope.capabilities.iter();
                    lines.extend(capabilities.map(|capability| format!("scope {capability}")));
                }
            }
            Caller::Owner | Caller::Anonymous => lines.push("by owner".to_owned()),
        }
        match &pending.blocked {
            Blocked::Evidence(missing) => lines.extend(missing.iter().map(|requirement| {
                format!(
                    "missing {} {} {}",
                    requirement.kind, requirement.verdict, requirement.issuer
                )
            })),
            Blocked::Conflict { statement, .. } => lines.push(format!("conflict {statement}")),
        }
        lines.join("\n") + "\n"
    }

    /// Decodes what [`PendingFile::encode`] wrote, or returns `None` if it is
    /// malformed. A conflict comes back with no paths, since the file holds
    /// only the statement listing them.
    pub(super) fn decode(text: &str) -> Option<Waiting> {
        let mut lines = text.lines();
        if lines.next()? != Self::HEADER {
            return None;
        }
        let onto = lines.next()?.strip_prefix("onto ")?.parse().ok()?;
        let proposal = match lines.next()?.split_once(' ')? {
            ("change", id) => Proposal::Change(id.parse().ok()?),
            ("target", name) => Proposal::Target(name.parse().ok()?),
            _ => return None,
        };
        let subject = match lines.next()?.strip_prefix("subject ")?.split_once(' ')? {
            ("snapshot", id) => RecordId::Snapshot(id.parse().ok()?),
            ("revision", id) => RecordId::Revision(id.parse().ok()?),
            ("statement", id) => RecordId::Statement(id.parse().ok()?),
            ("acceptance", id) => RecordId::Acceptance(id.parse().ok()?),
            _ => return None,
        };
        let principal = match lines.next()?.strip_prefix("by ")? {
            "owner" => None,
            principal => Some(principal.parse().ok()?),
        };
        let mut expires = None;
        let mut capabilities = Vec::new();
        let mut missing = Vec::new();
        let mut conflict = None;
        for line in lines {
            match line.split_once(' ')? {
                ("expires", at) => expires = Some(at.parse().ok()?),
                ("scope", capability) => capabilities.push(capability.parse().ok()?),
                ("missing", requirement) => {
                    let mut fields = requirement.split(' ');
                    missing.push(Requirement {
                        kind: fields.next()?.parse().ok()?,
                        verdict: fields.next()?.parse().ok()?,
                        issuer: fields.next()?.parse().ok()?,
                    });
                }
                ("conflict", id) => conflict = Some(id.parse().ok()?),
                _ => return None,
            }
        }
        let blocked = match conflict {
            Some(statement) => Blocked::Conflict {
                statement,
                paths: Vec::new(),
            },
            None => Blocked::Evidence(missing),
        };
        if expires.is_none() && !capabilities.is_empty() {
            return None;
        }
        let scope = expires.map(|expires| Scope {
            capabilities,
            expires,
        });
        let submitter = match principal {
            Some(principal) => Caller::Principal { principal, scope },
            None => Caller::Owner,
        };
        Some(Waiting {
            pending: Pending {
                onto,
                proposal,
                subject,
                blocked,
                expires,
            },
            submitter,
        })
    }
}
