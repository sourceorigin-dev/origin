//! Conflicts: the paths a merge could not combine.
//!
//! When both sides of a merge into a working copy, such as
//! `origin converge`, changed a text file, the merge writes both versions
//! between markers naming the sides:
//!
//! ```text
//! <<<<<<< 3f1c9a2e07b4
//! our version
//! =======
//! their version
//! >>>>>>> 8d02e5b1c4f9
//! ```
//!
//! For a binary file, or a path that is a directory on either side, it
//! keeps one side instead. The merge then issues a `core.conflict`
//! statement about the new revision, as its author (a user), listing each
//! conflicted path and the entry it wrote there. That list is what counts,
//! not the markers: a path stays unresolved while a revision still has that
//! entry there. Editing the file resolves it, and so does `origin resolve`,
//! which accepts the file as it is.
//!
//! A revision made from one with unresolved paths (by a snapshot,
//! `describe`, a new change on top, or a merge) gets its own
//! `core.conflict` listing the paths still unresolved. It is published
//! before the revision, so the revision is never seen without it. The
//! landing queue refuses a revision that still has unresolved paths.
//!
//! The landing queue also issues a `core.conflict`, as a node, when a
//! revision does not rebase onto its target. That statement only blocks
//! landing on that target; each retry merges again.

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
    /// The line starting our side, then its label.
    const OURS: &str = "<<<<<<< ";
    /// The line between the sides.
    const BETWEEN: &str = "=======";
    /// The line ending their side, then its label.
    const THEIRS: &str = ">>>>>>> ";

    /// Builds the conflicts from `conflicted`, sorted by path. When a path
    /// appears more than once, the last entry wins.
    #[must_use]
    pub fn new(mut conflicted: Vec<Conflicted>) -> Self {
        // Stable, so the latest of each path comes last, and dedup keeps the
        // first of equal ones: reverse around it.
        conflicted.reverse();
        conflicted.sort_by(|a, b| a.path.cmp(&b.path));
        conflicted.dedup_by(|a, b| a.path == b.path);
        Self(conflicted)
    }

    /// Adds `conflicted`, which must come after every path listed.
    pub(crate) fn push(&mut self, conflicted: Conflicted) {
        debug_assert!(self.0.last().is_none_or(|last| last.path < conflicted.path));
        self.0.push(conflicted);
    }

    /// Whether there are none.
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

    /// The content of a file both sides changed: `ours`, then `theirs`,
    /// between markers naming them with `labels`. A side that removed the
    /// file is empty. `None` if either side is binary, holding a NUL byte:
    /// markers would only corrupt it.
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
        let payload = EncodedObject::blob(list.as_bytes());
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
    /// the repository always keeps it.
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
            .try_blob(payload.into())?
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
        if matches!(revision.author, Principal::Node(_)) {
            return Ok(Self::default());
        }
        let Some(claim) = claims.iter().rev().find(|claim| {
            claim.statement().kind == Kind::CONFLICT && claim.statement().issuer == revision.author
        }) else {
            return Ok(Self::default());
        };
        Self::listed(store, claim.statement())?.remaining(store, revision.snapshot)
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
mod tests {
    use origin_platform::Native;
    use origin_shared::{EntryKind, EntryName, Record, Tree, TreeEntry};

    use super::*;

    fn path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    #[test]
    fn a_path_resolves_once_its_entry_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let marked = EncodedObject::blob(b"<<<<<<< a\nours\n=======\ntheirs\n>>>>>>> b\n");
        let resolved = EncodedObject::blob(b"Heading\n=======\nresolved\n");
        let snapshot = |file: &EncodedObject| {
            let mut tree = Tree::default();
            tree.insert(
                EntryName::new("file").unwrap(),
                TreeEntry {
                    kind: EntryKind::File,
                    id: file.id(),
                },
            );
            let tree = tree.encode();
            store.write_all(&[file.clone(), tree.clone()]).unwrap();
            TreeId::from(tree.id())
        };
        let conflicts = Conflicts::new(vec![
            Conflicted {
                path: path("file"),
                written: Some(marked.id()),
            },
            Conflicted {
                path: path("removed"),
                written: None,
            },
        ]);
        let (payload, statement) = conflicts.statement(
            RecordId::Snapshot(snapshot(&marked)),
            "user:ada".parse().unwrap(),
            None,
        );
        // Without its payload, the statement lists nothing it can trust.
        assert!(matches!(
            Conflicts::listed(&store, &statement),
            Err(StoreError::NotFound(id)) if id == payload.id()
        ));
        store.write_all(&[payload]).unwrap();
        assert_eq!(Conflicts::listed(&store, &statement).unwrap(), conflicts);

        let unchanged = conflicts.clone().remaining(&store, snapshot(&marked));
        assert_eq!(unchanged.unwrap(), conflicts);
        // Even to content that looks like a marker.
        let remaining = conflicts.remaining(&store, snapshot(&resolved)).unwrap();
        assert_eq!(remaining.paths(), [path("removed")]);
    }

    #[test]
    fn only_a_user_author_leaves_a_revision_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let tree = Tree::default().encode();
        store.write_all(std::slice::from_ref(&tree)).unwrap();
        let revision = |author: &str| Revision {
            change: origin_shared::ChangeId::from_bytes([1; 16]),
            predecessors: Vec::new(),
            base: origin_shared::Base::Acceptance(EncodedObject::blob(b"base").id().into()),
            deps: Vec::new(),
            snapshot: tree.id().into(),
            description: EncodedObject::blob(b"").id().into(),
            author: author.parse().unwrap(),
        };
        let subject = RecordId::Revision(revision("user:ada").encode().id().into());
        let conflicts = Conflicts::new(vec![Conflicted {
            path: path("file"),
            written: None,
        }]);
        let claims = |issuer: &str| {
            let (payload, statement) = conflicts.statement(subject, issuer.parse().unwrap(), None);
            store.write_all(&[payload]).unwrap();
            let id = statement.encode().id().into();
            [store.claim(id, statement).unwrap().unwrap()]
        };
        let ada = revision("user:ada");
        assert_eq!(
            Conflicts::of(&store, &claims("user:ada"), &ada).unwrap(),
            conflicts
        );
        assert!(
            Conflicts::of(&store, &claims("user:eve"), &ada)
                .unwrap()
                .is_empty()
        );
        // A queue that rebased a revision, then could not rebase it elsewhere.
        let queue = revision("node:queue");
        assert!(
            Conflicts::of(&store, &claims("node:queue"), &queue)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn newer_conflicts_replace_older_ones() {
        let older = Conflicts::new(vec![
            Conflicted {
                path: path("a"),
                written: None,
            },
            Conflicted {
                path: path("b"),
                written: None,
            },
        ]);
        let id = EncodedObject::blob(b"newer").id();
        let newer = Conflicts::new(vec![Conflicted {
            path: path("b"),
            written: Some(id),
        }]);
        let mut both = older.with(newer);
        assert_eq!(
            both,
            Conflicts::new(vec![
                Conflicted {
                    path: path("a"),
                    written: None,
                },
                Conflicted {
                    path: path("b"),
                    written: Some(id),
                },
            ])
        );
        assert_eq!(both.resolve(&[path("a"), path("c")]), [path("a")]);
        assert_eq!(both.paths(), [path("b")]);
    }

    #[test]
    fn marks_both_sides() {
        let marked = Conflicts::mark(b"ours\n", b"theirs", ["a", "b"]).unwrap();
        assert_eq!(
            String::from_utf8(marked).unwrap(),
            "<<<<<<< a\nours\n=======\ntheirs\n>>>>>>> b\n"
        );
        assert_eq!(
            Conflicts::mark(b"", b"kept\n", ["a", "b"]).unwrap(),
            b"<<<<<<< a\n=======\nkept\n>>>>>>> b\n",
            "a removed side is empty"
        );
        assert_eq!(Conflicts::mark(b"\0binary", b"text", ["a", "b"]), None);
    }
}
