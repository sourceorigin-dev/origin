//! Conflicts: the paths a merge could not combine.
//!
//! When both sides of a merge into a working copy, such as `origin converge`,
//! changed a text file, the merge writes both versions between markers naming
//! the sides:
//!
//! ```text
//! <<<<<<< 3f1c9a2e07b4
//! our version
//! =======
//! their version
//! >>>>>>> 8d02e5b1c4f9
//! ```
//!
//! The merge then issues a `core.conflict` statement about the new revision,
//! listing each conflicted path and the entry it wrote there. That list is what
//! counts, not the markers: a path stays unresolved while a revision still has
//! that entry there. Binary files, carrying paths forward and the landing queue's
//! own statement are in `docs/design.md`, under "Conflicts".

use origin_platform::Platform;
use origin_shared::{
    EncodedObject, Kind, ObjectId, Principal, RecordId, RepoPath, Revision, Statement, StatementId,
    TreeId,
};

use crate::annotations::Claim;
use crate::store::{Store, StoreError};
use crate::tree::Snapshots;

/// A path a merge left conflicted, and the entry it wrote there.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Conflicted {
    /// The path.
    pub path: RepoPath,
    /// The id of the entry written there: both versions between markers,
    /// or the side kept. `None` if the merge left nothing there.
    pub written: Option<ObjectId>,
}

/// Conflicted paths, sorted, each at most once: those a merge left, or a
/// revision's unresolved ones.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conflicts(Vec<Conflicted>);

impl Conflicts {
    /// The marker that starts our side, followed by its label.
    const OURS: &str = "<<<<<<< ";
    /// The marker between the two sides.
    const BETWEEN: &str = "=======";
    /// The marker that ends their side, followed by its label.
    const THEIRS: &str = ">>>>>>> ";

    /// Builds the conflicts from `conflicted`, sorted by path. When a path
    /// appears more than once, the last entry wins.
    #[must_use]
    pub fn new(mut conflicted: Vec<Conflicted>) -> Self {
        conflicted.reverse();
        conflicted.sort_by(|a, b| a.path.cmp(&b.path));
        conflicted.dedup_by(|a, b| a.path == b.path);
        Self(conflicted)
    }

    /// Adds `conflicted`, whose path must sort after every path already
    /// listed.
    pub(crate) fn push(&mut self, conflicted: Conflicted) {
        debug_assert!(self.0.last().is_none_or(|last| last.path < conflicted.path));
        self.0.push(conflicted);
    }

    /// Whether there are no conflicts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The conflicted paths, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<RepoPath> {
        self.0
            .iter()
            .map(|conflicted| conflicted.path.clone())
            .collect()
    }

    /// Returns these conflicts merged with `newer`, whose entries replace those
    /// on the same paths. This is what a merge of a revision with these
    /// conflicts leaves.
    #[must_use]
    pub fn with(mut self, newer: Self) -> Self {
        self.0.extend(newer.0);
        Self::new(self.0)
    }

    /// Removes the conflicts on `paths` and returns the paths that had one.
    pub fn resolve(&mut self, paths: &[RepoPath]) -> Vec<RepoPath> {
        let mut resolved = Vec::new();
        self.0.retain(|conflicted| {
            let listed = paths.contains(&conflicted.path);
            if listed {
                resolved.push(conflicted.path.clone());
            }
            !listed
        });
        resolved
    }

    /// Returns the content of a file both sides changed: `ours`, then
    /// `theirs`, between conflict markers labeled with `labels`. A side that
    /// removed the file counts as empty. Returns `None` if either side is
    /// binary, holding a NUL byte, since markers would corrupt it.
    #[must_use]
    pub fn mark(ours: &[u8], theirs: &[u8], labels: [&str; 2]) -> Option<Vec<u8>> {
        if ours.contains(&0) || theirs.contains(&0) {
            return None;
        }
        let mut marked = Vec::with_capacity(ours.len() + theirs.len() + 64);
        let side = |marked: &mut Vec<u8>, content: &[u8]| {
            marked.extend_from_slice(content);
            if !content.is_empty() && !content.ends_with(b"\n") {
                marked.push(b'\n');
            }
        };
        marked.extend_from_slice(format!("{}{}\n", Self::OURS, labels[0]).as_bytes());
        side(&mut marked, ours);
        marked.extend_from_slice(format!("{}\n", Self::BETWEEN).as_bytes());
        side(&mut marked, theirs);
        marked.extend_from_slice(format!("{}{}\n", Self::THEIRS, labels[1]).as_bytes());
        Some(marked)
    }

    /// Builds the `core.conflict` statement about `subject` from `issuer`, and
    /// the payload listing these conflicts, and returns both. Each conflict is
    /// written as the entry id (or `-` for none), a space and the path, ended
    /// by a NUL byte.
    #[must_use]
    pub fn statement(
        &self,
        subject: RecordId,
        issuer: Principal,
        supersedes: Option<StatementId>,
    ) -> (EncodedObject, Statement) {
        let mut list = String::new();
        for conflicted in &self.0 {
            match conflicted.written {
                Some(id) => list.push_str(&id.to_string()),
                None => list.push('-'),
            }
            list.push(' ');
            list.push_str(conflicted.path.as_str());
            list.push('\0');
        }
        let payload = EncodedObject::attachment(list.as_bytes());
        let statement = Statement {
            subject,
            kind: Kind::CONFLICT,
            verdict: None,
            issuer,
            inputs: None,
            payload: Some(payload.id().into()),
            supersedes,
            promoted_from: None,
        };
        (payload, statement)
    }

    /// Reads the conflicts listed in a `core.conflict` statement's payload. A
    /// payload missing from the store is an error, not an empty list, because
    /// retention keeps the payload of every reserved kind.
    ///
    /// # Errors
    ///
    /// Fails if the payload is missing or cannot be read.
    pub fn listed<P: Platform>(
        store: &Store<P>,
        statement: &Statement,
    ) -> Result<Self, StoreError> {
        let Some(payload) = statement.payload else {
            return Ok(Self::default());
        };
        let content = store
            .try_attachment(payload)?
            .ok_or(StoreError::NotFound(payload.into()))?;
        let conflicted = String::from_utf8_lossy(&content)
            .split_terminator('\0')
            .filter_map(|line| {
                let (written, path) = line.split_once(' ')?;
                let written = match written {
                    "-" => None,
                    id => Some(id.parse().ok()?),
                };
                let path = RepoPath::new(path).ok()?;
                Some(Conflicted { path, written })
            })
            .collect();
        Ok(Self::new(conflicted))
    }

    /// Returns the unresolved conflicts of `revision`, given `claims`, the
    /// claims about it that hold. These are the paths listed in the latest
    /// `core.conflict` from the revision's author where the revision still has
    /// the written entry. Returns none if the author is a node: a node's
    /// `core.conflict` only means a landing queue could not rebase it.
    ///
    /// # Errors
    ///
    /// Fails if the statement's payload is missing, or if it or a tree on the
    /// way to a path cannot be read.
    pub fn of<P: Platform>(
        store: &Store<P>,
        claims: &[Claim],
        revision: &Revision,
    ) -> Result<Self, StoreError> {
        Self::latest(store, claims.iter().map(Claim::statement), revision)
    }

    /// Returns the unresolved conflicts of `revision`, like [`Conflicts::of`],
    /// from `statements` about it in arrival order.
    ///
    /// # Errors
    ///
    /// Fails like [`Conflicts::of`].
    pub fn latest<'s, P: Platform>(
        store: &Store<P>,
        statements: impl IntoIterator<Item = &'s Statement>,
        revision: &Revision,
    ) -> Result<Self, StoreError> {
        if matches!(revision.author, Principal::Node(_)) {
            return Ok(Self::default());
        }
        let latest = statements.into_iter().filter(|statement| {
            statement.kind == Kind::CONFLICT && statement.issuer == revision.author
        });
        let Some(statement) = latest.last() else {
            return Ok(Self::default());
        };
        Self::listed(store, statement)?.remaining(store, revision.snapshot)
    }

    /// Returns the conflicts that `snapshot` still has: those whose path still
    /// holds the entry the merge wrote.
    ///
    /// # Errors
    ///
    /// Fails if a tree on the way to a path cannot be read.
    pub fn remaining<P: Platform>(
        mut self,
        store: &Store<P>,
        snapshot: TreeId,
    ) -> Result<Self, StoreError> {
        let mut snapshots = Snapshots::new(store);
        let mut remaining = Vec::with_capacity(self.0.len());
        for conflicted in self.0.drain(..) {
            let entry = snapshots.entry_at(snapshot, &conflicted.path)?;
            if entry.map(|entry| entry.id) == conflicted.written {
                remaining.push(conflicted);
            }
        }
        Ok(Self(remaining))
    }
}

#[cfg(test)]
mod tests;
