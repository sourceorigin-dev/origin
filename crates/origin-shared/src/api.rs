//! Types of the HTTP API shared by the server and the SDK: the requests,
//! responses and errors that aren't records or events.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::access::Capability;
use crate::landing::{Landing, Proposal};
use crate::policy::{KindPattern, Requirement};
use crate::text::Period;
use crate::{
    Acceptance, AcceptanceId, ChangeId, Head, ObjectId, Principal, PrincipalName, RecordId,
    RepoPath, Revision, RevisionId, Statement, StatementId, TargetName, TreeId,
};

/// The name of a repository on a server: ASCII letters, digits, `.`, `_`
/// and `-`, not starting with `.`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(try_from = "String", into = "String")]
#[schema(value_type = String, example = "origin")]
pub struct RepoName(String);

impl RepoName {
    /// Creates a repository name, checking it follows the rules above.
    ///
    /// # Errors
    ///
    /// Fails if the name contains other characters, or starts with `.`.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidRepoName> {
        let name = name.into();
        let valid = !name.is_empty()
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
        if valid {
            Ok(Self(name))
        } else {
            Err(InvalidRepoName(name))
        }
    }

    /// The name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RepoName {
    type Error = InvalidRepoName;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl From<RepoName> for String {
    fn from(name: RepoName) -> Self {
        name.0
    }
}

impl FromStr for RepoName {
    type Err = InvalidRepoName;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::new(name)
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// A repository name that [`RepoName::new`] rejected.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid repository name {0:?}")]
pub struct InvalidRepoName(pub String);

/// A repository, as the server lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RepoSummary {
    /// The repository's name.
    pub name: RepoName,
}

/// The response header set when a fetch stopped short of what it asked for,
/// to stay within [`Pack::LIMIT`](crate::pack::Pack::LIMIT). See
/// [`Part`](crate::pack::Part).
pub const MORE: &str = "origin-more";

/// A request for records by id: revisions, statements and acceptances. Each
/// record comes with its attachment, if the repository has it: a revision's
/// description or a statement's payload. Fetch snapshots with
/// [`FetchSnapshot`] instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FetchRecords {
    /// The ids of the records, which are returned in this order, each followed
    /// by its attachment. A response with the [`MORE`] header stops short. Ask
    /// again for the records after the last one it holds.
    pub ids: Vec<ObjectId>,
}

/// A request for the trees and blobs of a snapshot that the caller lacks,
/// walked from its root. This is the only way blobs and trees leave a
/// repository, so the server can check what each caller may read by path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FetchSnapshot {
    /// The snapshot.
    pub snapshot: TreeId,
    /// Snapshots the caller holds in full. Anything one of them holds at the
    /// same path is skipped, so only what changed is sent. Snapshots the
    /// repository doesn't have are ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub have: Vec<TreeId>,
    /// Limits the fetch to these files and directories, everything under them,
    /// and the trees on the way to them. Defaults to everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<RepoPath>,
    /// How many objects to skip, because earlier parts brought them. A response
    /// that stops short has the [`MORE`] header.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skip: u64,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// A request for what a repository holds of some changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InventoryRequest {
    /// The changes.
    pub changes: Vec<ChangeId>,
}

/// What a repository holds of a change: the ids of its records, to compare
/// with another node's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Inventory {
    /// The change.
    pub change: ChangeId,
    /// The change's revisions, in the order the repository received them. Empty
    /// if the repository has never seen the change.
    pub revisions: Vec<RevisionId>,
    /// The revisions that no other revision of the change evolves from.
    pub heads: Vec<RevisionId>,
    /// The statements about the change's revisions, and about those statements,
    /// in no particular order.
    pub statements: Vec<StatementId>,
}

/// Makes storing a batch of objects conditional on the named changes having
/// the heads the caller saw. This is a compare-and-swap: a push made from a
/// stale view of a change is refused instead of making the change diverge.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PutQuery {
    /// The heads of the changes as the caller saw them, comma-separated. Each is
    /// `<change>=<head>+<head>`, or `<change>=` for a change the repository
    /// didn't have. The batch is refused if any change's heads differ now.
    #[serde(default, skip_serializing_if = "List::is_empty")]
    #[param(value_type = Option<String>)]
    pub expect: List<Expected>,
}

/// The heads of a change as the caller saw them, written
/// `<change>=<head>+<head>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expected {
    /// The change.
    pub change: ChangeId,
    /// The change's heads. Empty if the repository didn't have the change.
    pub heads: Vec<RevisionId>,
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}=", self.change)?;
        for (i, head) in self.heads.iter().enumerate() {
            let separator = if i == 0 { "" } else { "+" };
            write!(f, "{separator}{head}")?;
        }
        Ok(())
    }
}

impl FromStr for Expected {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let invalid = |error: crate::ParseIdError| format!("invalid heads {input:?}: {error}");
        let (change, heads) = input
            .split_once('=')
            .ok_or_else(|| format!("invalid heads {input:?}: expected <change>=<head>+<head>"))?;
        let heads = heads
            .split('+')
            .filter(|head| !head.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(invalid)?;
        Ok(Self {
            change: change.parse().map_err(invalid)?,
            heads,
        })
    }
}

/// The result of storing a batch of records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Stored {
    /// How many of the objects were new to the repository.
    pub new: u64,
}

/// A request for the statements about a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatementsQuery {
    /// The record, such as `revision:<id>`.
    #[param(value_type = String)]
    pub subject: RecordId,
}

/// A statement, whether its issuer superseded it, and whether its claim
/// holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Annotation {
    /// The statement's id.
    pub id: StatementId,
    /// The statement.
    pub statement: Statement,
    /// The first statement from the same issuer that supersedes it.
    pub superseded_by: Option<StatementId>,
    /// Whether the landing queue would count the claim as evidence. It must not
    /// be superseded. A promotion must also be valid, its original must not be
    /// superseded, and no result about the record may replace it.
    pub holds: bool,
}

/// The revisions of a change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Revisions {
    /// Every revision, in the order the repository received them.
    pub revisions: Vec<RevisionEntry>,
    /// The revisions that no other revision of the change evolves from. More
    /// than one means the change diverged.
    pub heads: Vec<RevisionId>,
}

/// A revision and its id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RevisionEntry {
    /// The revision's id.
    pub id: RevisionId,
    /// The revision.
    pub revision: Revision,
}

/// A target, its location, and the node that orders it under the policy in
/// force.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TargetInfo {
    /// The target's name.
    pub name: TargetName,
    /// The target's latest acceptance.
    pub head: Head,
    /// The node that orders the target, if the policy names one.
    pub authority: Option<PrincipalName>,
    /// The statements that landing on the target requires.
    pub requirements: Vec<Requirement>,
}

/// A request for the acceptances of a target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LogQuery {
    /// The target.
    #[param(value_type = String)]
    pub target: TargetName,
    /// The position of the first acceptance to return. Defaults to 0, the first.
    #[serde(default)]
    pub since: u64,
    /// The most acceptances to return. Defaults to all.
    pub limit: Option<u64>,
}

/// One acceptance in a target's log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LogEntry {
    /// The revision's id.
    pub id: AcceptanceId,
    /// The acceptance. Its `seq` is its position in the log.
    pub acceptance: Acceptance,
}

/// A request for the queue to land something.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LandRequest {
    /// What to land.
    pub proposal: Proposal,
    /// The target to land on. For a change, defaults to the target its stack is
    /// on. Landing a target requires it.
    pub onto: Option<TargetName>,
}

/// The outcome of a landing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Landings {
    /// The target landed on.
    pub onto: TargetName,
    /// The outcome of each landing: one for each change of the stack that the
    /// target didn't have, from the bottom, up to the first one that waits.
    pub landings: Vec<Landing>,
}

/// A request to create a target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NewTarget {
    /// The new target's name.
    pub name: TargetName,
    /// The target whose tip the new target starts at.
    pub from: TargetName,
}

/// A request for the events after a cursor. If there are none yet, the
/// server waits for some (a long-poll).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EventsQuery {
    /// The cursor to read after: the `next` of the previous page. Defaults to
    /// the end of the stream, so only new events are returned.
    pub cursor: Option<u64>,
    /// The generation of the op log the cursor belongs to, as its page gave it.
    /// A cursor from another generation, such as from before the repository was
    /// restored, has expired.
    pub generation: Option<String>,
    /// How long to wait for events, such as `30s`. Without it, the server
    /// answers at once, possibly with no events.
    #[param(value_type = Option<String>)]
    pub wait: Option<Period>,
    /// Roughly how many events to return. All the events of one op log entry are
    /// returned together, so a page can exceed it.
    pub limit: Option<u32>,
    /// Only return events with these names or statement kinds, comma-separated,
    /// such as `check.requested,ci.*`.
    #[serde(default, skip_serializing_if = "List::is_empty")]
    #[param(value_type = Option<String>)]
    pub kind: List<KindPattern>,
    /// Only return events whose subject starts with one of these,
    /// comma-separated, such as a revision id prefix, `change:3fa2` or `main`.
    #[serde(default, skip_serializing_if = "List::is_empty")]
    #[param(value_type = Option<String>)]
    pub subject: List<String>,
}

/// A list written as one comma-separated string, the way query parameters
/// take lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct List<T>(pub Vec<T>);

impl<T> Default for List<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> List<T> {
    /// Whether the list has no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T: fmt::Display> Serialize for List<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let values: Vec<_> = self.0.iter().map(ToString::to_string).collect();
        serializer.serialize_str(&values.join(","))
    }
}

impl<'de, T> Deserialize<'de> for List<T>
where
    T: FromStr<Err: fmt::Display>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.split(',')
            .filter(|value| !value.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// A request for a token with a narrower scope, for an agent or a CI runner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TokenRequest {
    /// What the token may do: a subset of what the caller may do.
    pub scope: Vec<Capability>,
    /// How long the token lasts, such as `1h`.
    #[schema(value_type = String)]
    pub ttl: Period,
}

/// A token a server issued.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssuedToken {
    /// The token, to send as `Authorization: Bearer <token>`.
    pub token: String,
    /// Who it acts for.
    pub principal: Principal,
    /// When it expires, in seconds since the Unix epoch.
    pub expires: u64,
}

/// The error a request failed with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ApiError {
    /// The kind of failure.
    pub code: ErrorCode,
    /// A human-readable explanation.
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// The kind of an [`ApiError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request is malformed, or names records the repository can't accept.
    BadRequest,
    /// The repository, target, change or record does not exist.
    NotFound,
    /// The request is valid, but the repository's state refuses it, such as a
    /// change that diverged, a target that already exists, or a target another
    /// node orders.
    Conflict,
    /// The event cursor is older than any event the server still keeps.
    Expired,
    /// The credentials are invalid or expired.
    Unauthorized,
    /// The caller may not do this.
    Forbidden,
    /// The server failed.
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_are_validated() {
        for valid in ["origin", "my-repo.v2", "a_b"] {
            assert!(RepoName::new(valid).is_ok(), "{valid:?}");
        }
        for invalid in ["", ".hidden", "a/b", "sp ace"] {
            assert!(RepoName::new(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn expected_heads_round_trip() {
        let change = ChangeId::from_bytes([1; 16]);
        let heads = [
            RevisionId::from_bytes([2; 32]),
            RevisionId::from_bytes([3; 32]),
        ];
        for heads in [&heads[..], &heads[..1], &[]] {
            let expected = Expected {
                change,
                heads: heads.to_vec(),
            };
            assert_eq!(expected.to_string().parse::<Expected>(), Ok(expected));
        }
        assert!("nonsense".parse::<Expected>().is_err());
    }

    #[test]
    fn lists_are_comma_separated() {
        let list: List<KindPattern> = serde_json::from_str("\"ci.*,review\"").unwrap();
        assert_eq!(list.0.len(), 2);
        assert_eq!(serde_json::to_string(&list).unwrap(), "\"ci.*,review\"");
        let empty: List<String> = serde_json::from_str("\"\"").unwrap();
        assert!(empty.is_empty());
        assert!(serde_json::from_str::<List<KindPattern>>("\"CI\"").is_err());
    }
}
