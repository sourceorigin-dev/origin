//! Conflicts: what a merge leaves where both sides changed a file.
//!
//! A [marked](crate::store::Store::merge_marked) merge writes both
//! versions of each such text file, between markers naming the sides:
//!
//! ```text
//! <<<<<<< 3f1c9a2e07b4
//! our version
//! =======
//! their version
//! >>>>>>> 8d02e5b1c4f9
//! ```
//!
//! A binary file, or a path a directory is on either side of, keeps one
//! side instead.

use origin_shared::{ObjectId, RepoPath};

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
