//! Events: what happened on a node, for plugins and agents to react to.
//! Each event carries the cursor of the op log entry it comes from. Reading
//! the events after a cursor returns a [`Page`] and the next cursor. Events
//! are delivered at least once, so consumers deduplicate them by their
//! [`EventKind`], which is everything in an event except its cursor.

use std::fmt;

use crate::policy::{KindPattern, Requirement};
use crate::{
    AcceptanceId, ChangeId, Kind, Principal, RecordId, RevisionId, StatementId, TargetName, Verdict,
};

/// An event, with its position in the op log.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Event {
    /// The sequence number of the op log entry the event comes from. Reading
    /// from this cursor delivers the events of later entries.
    pub cursor: u64,
    /// What happened.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// What happened: the event without its cursor. Consumers deduplicate events
/// by it. In JSON, the `event` field holds its name, as [`EventKind::name`]
/// returns it.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(tag = "event")]
pub enum EventKind {
    /// A statement was issued.
    #[serde(rename = "statement.issued")]
    StatementIssued {
        /// The statement.
        statement: StatementId,
        /// The record it is about.
        subject: RecordId,
        /// The statement's kind.
        kind: Kind,
        /// Its verdict.
        verdict: Option<Verdict>,
        /// Who issued it.
        issuer: Principal,
    },
    /// A revision was created.
    #[serde(rename = "revision.created")]
    RevisionCreated {
        /// The revision.
        revision: RevisionId,
        /// The revision's change.
        change: ChangeId,
        /// The revision's author.
        author: Principal,
    },
    /// A target advanced.
    #[serde(rename = "target.advanced")]
    TargetAdvanced {
        /// The target.
        target: TargetName,
        /// The new acceptance's position in the target's log.
        seq: u64,
        /// The acceptance.
        acceptance: AcceptanceId,
    },
    /// A waiting landing lacks a statement it requires.
    #[serde(rename = "check.requested")]
    CheckRequested {
        /// The target it lands on.
        onto: TargetName,
        /// The record the required statement must be about.
        subject: RecordId,
        /// The statement the landing requires.
        requirement: Requirement,
    },
}

/// What an event is about. The `--subject` option matches against it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    /// A record.
    Record(RecordId),
    /// A change. Revision events are about their change.
    Change(ChangeId),
    /// A target.
    Target(TargetName),
}

impl EventKind {
    /// The event's name, such as `statement.issued`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::StatementIssued { .. } => "statement.issued",
            Self::RevisionCreated { .. } => "revision.created",
            Self::TargetAdvanced { .. } => "target.advanced",
            Self::CheckRequested { .. } => "check.requested",
        }
    }

    /// The record the event is about: the statement, revision or acceptance,
    /// or for a check request, the record to check.
    #[must_use]
    pub fn record(&self) -> RecordId {
        match *self {
            Self::StatementIssued { statement, .. } => RecordId::Statement(statement),
            Self::RevisionCreated { revision, .. } => RecordId::Revision(revision),
            Self::TargetAdvanced { acceptance, .. } => RecordId::Acceptance(acceptance),
            Self::CheckRequested { subject, .. } => subject,
        }
    }

    /// What the event is about: the subject of a statement or check request,
    /// the change of a revision, or the target that advanced.
    #[must_use]
    pub fn subject(&self) -> Subject {
        match self {
            Self::StatementIssued { subject, .. } | Self::CheckRequested { subject, .. } => {
                Subject::Record(*subject)
            }
            Self::RevisionCreated { change, .. } => Subject::Change(*change),
            Self::TargetAdvanced { target, .. } => Subject::Target(target.clone()),
        }
    }

    /// Returns the subjects a [`Filter`] matches the event against: its
    /// [subject](Self::subject), plus the revision for a revision created
    /// event.
    #[must_use]
    pub fn subjects(&self) -> Vec<Subject> {
        let subject = self.subject();
        match *self {
            Self::RevisionCreated { revision, .. } => {
                vec![subject, Subject::Record(RecordId::Revision(revision))]
            }
            _ => vec![subject],
        }
    }

    /// Returns the kinds a [`Filter`] matches the event against: its name, plus
    /// the kind of the statement issued or requested.
    ///
    /// # Panics
    ///
    /// Never: event names are valid kinds.
    #[must_use]
    #[expect(clippy::expect_used, reason = "Event names are valid kinds.")]
    pub fn kinds(&self) -> Vec<Kind> {
        let name = Kind::new(self.name()).expect("event names are valid kinds");
        match self {
            Self::StatementIssued { kind, .. } => vec![name, kind.clone()],
            Self::CheckRequested { requirement, .. } => vec![name, requirement.kind.clone()],
            Self::RevisionCreated { .. } | Self::TargetAdvanced { .. } => vec![name],
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(record) => record.fmt(f),
            Self::Change(id) => write!(f, "change:{id}"),
            Self::Target(name) => write!(f, "target:{name}"),
        }
    }
}

/// Formats the event as one line: the cursor, the name, then `key=value`
/// fields with full ids.
impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.cursor, self.kind.name())?;
        let verdict = |verdict: &Option<Verdict>| {
            verdict.map_or_else(|| "none".to_owned(), |verdict| verdict.to_string())
        };
        match &self.kind {
            EventKind::StatementIssued {
                statement,
                subject,
                kind,
                verdict: issued,
                issuer,
            } => write!(
                f,
                " statement={statement} subject={} kind={kind} verdict={} issuer={issuer}",
                Subject::Record(*subject),
                verdict(issued)
            ),
            EventKind::RevisionCreated {
                revision,
                change,
                author,
            } => write!(f, " revision={revision} change={change} author={author}"),
            EventKind::TargetAdvanced {
                target,
                seq,
                acceptance,
            } => write!(f, " target={target} seq={seq} acceptance={acceptance}"),
            EventKind::CheckRequested {
                onto,
                subject,
                requirement,
            } => write!(
                f,
                " onto={onto} subject={} kind={} verdict={} issuer={}",
                Subject::Record(*subject),
                requirement.kind,
                requirement.verdict,
                requirement.issuer
            ),
        }
    }
}

/// Which events to deliver. An empty list matches everything.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Event names or statement kinds to match, such as `check.requested`,
    /// `core.conflict` or `ci.*`. An event matches if its name, or the kind of
    /// the statement it issues or requests, matches one of them.
    pub kinds: Vec<KindPattern>,
    /// Subject prefixes to match, such as `change:3fa2`, `main` or the start of
    /// a revision id. An event matches if its [subject](EventKind::subject),
    /// or for `revision.created` its revision, starts with one of them. The
    /// subject can be written with its type, such as `revision:<id>`, or as the
    /// bare id or name.
    pub subjects: Vec<String>,
}

impl Filter {
    /// Whether `event` passes the filter.
    #[must_use]
    pub fn matches(&self, event: &EventKind) -> bool {
        let kind = self.kinds.is_empty()
            || event
                .kinds()
                .iter()
                .any(|kind| self.kinds.iter().any(|pattern| pattern.matches(kind)));
        kind && (self.subjects.is_empty()
            || event.subjects().iter().any(|subject| {
                let subject = subject.to_string();
                let bare = subject.split_once(':').map_or("", |(_, bare)| bare);
                self.subjects.iter().any(|start| {
                    subject.starts_with(start.as_str()) || bare.starts_with(start.as_str())
                })
            }))
    }
}

/// A batch of events, and the cursor to read the next batch from.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
pub struct Page {
    /// The events, oldest first.
    pub events: Vec<Event>,
    /// The cursor after the entries read. Pass it to the next read.
    pub next: u64,
    /// The generation of the op log the cursor belongs to. Pass it along with
    /// the cursor: a cursor means nothing in another generation.
    #[serde(default)]
    pub generation: String,
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::EncodedObject;

    #[test]
    fn check_requests_about_one_record_are_told_apart() {
        let subject = RecordId::Revision(EncodedObject::blob(b"a revision").id().into());
        let request = |onto: &str, kind: &str| EventKind::CheckRequested {
            onto: TargetName::new(onto).unwrap(),
            subject,
            requirement: Requirement {
                kind: Kind::new(kind).unwrap(),
                verdict: Verdict::Pass,
                issuer: "group:runners".parse().unwrap(),
            },
        };
        let requests = [
            request("main", "ci.test"),
            request("main", "ci.lint"),
            request("release", "ci.test"),
            request("main", "ci.test"),
        ];
        assert!(requests.iter().all(|request| request.record() == subject));
        let distinct: HashSet<_> = requests.iter().collect();
        assert_eq!(distinct.len(), 3, "the same request twice is one");
    }
}
