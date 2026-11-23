use std::collections::BTreeMap;

use origin_platform::Native;
use origin_shared::{EncodedObject, EntryKind, Record};

use super::*;

/// Builds nested trees from files inserted by path.
#[derive(Debug, Default)]
struct TreeBuilder {
    entries: BTreeMap<EntryName, Node>,
}

#[derive(Debug)]
enum Node {
    File(TreeEntry),
    Dir(TreeBuilder),
}

impl TreeBuilder {
    /// Adds a file at `path`, creating parent directories.
    fn insert(&mut self, path: &str, entry: TreeEntry) {
        let (dir, name) = match path.rsplit_once('/') {
            Some((dir, name)) => (Some(dir), name),
            None => (None, path),
        };
        let mut builder = self;
        for component in dir.into_iter().flat_map(|dir| dir.split('/')) {
            let node = builder
                .entries
                .entry(EntryName::new(component).unwrap())
                .or_insert_with(|| Node::Dir(Self::default()));
            let Node::Dir(child) = node else {
                panic!("{component} is a file")
            };
            builder = child;
        }
        builder
            .entries
            .insert(EntryName::new(name).unwrap(), Node::File(entry));
    }

    /// Writes every tree to the store and returns the root's id.
    fn write(self, store: &Store) -> TreeId {
        let tree: Tree = self
            .entries
            .into_iter()
            .map(|(name, node)| {
                let entry = match node {
                    Node::File(entry) => entry,
                    Node::Dir(dir) => TreeEntry {
                        kind: EntryKind::Tree,
                        id: dir.write(store).into(),
                    },
                };
                (name, entry)
            })
            .collect();
        let encoded = tree.encode();
        store.write_all(std::slice::from_ref(&encoded)).unwrap();
        encoded.id().into()
    }
}

fn file(content: &str) -> TreeEntry {
    TreeEntry {
        kind: EntryKind::File,
        id: EncodedObject::blob(content.as_bytes()).id(),
    }
}

pub(crate) fn build(store: &Store, files: &[(&str, &str)]) -> TreeId {
    let mut builder = TreeBuilder::default();
    for (path, content) in files {
        builder.insert(path, file(content));
    }
    builder.write(store)
}

#[test]
fn diffs_files_across_directories() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    let old = build(
        &store,
        &[
            ("same", "s"),
            ("edit", "1"),
            ("gone", "g"),
            ("dir/inner", "i"),
            ("swap", "f"),
        ],
    );
    let new = build(
        &store,
        &[
            ("same", "s"),
            ("edit", "2"),
            ("added/deep", "d"),
            ("swap/now-dir", "n"),
        ],
    );

    let changes = store.diff(old, new).unwrap();
    let summary: Vec<_> = changes
        .iter()
        .map(|c| (c.path.as_str(), c.old.is_some(), c.new.is_some()))
        .collect();
    assert_eq!(
        summary,
        [
            ("added/deep", false, true),
            ("dir/inner", true, false),
            ("edit", true, true),
            ("gone", true, false),
            ("swap", true, false),
            ("swap/now-dir", false, true),
        ]
    );
    assert_eq!(store.diff(old, old).unwrap(), []);
    let empty = build(&store, &[]);
    assert_eq!(store.diff(empty, old).unwrap().len(), 5);
}

#[test]
fn lists_what_the_haves_lack() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    let old = build(
        &store,
        &[
            ("same/a", "1"),
            ("edit/b", "2"),
            ("file", "3"),
            ("docs/x", "4"),
        ],
    );
    let new = build(
        &store,
        &[
            ("same/a", "1"),
            ("edit/b", "changed"),
            ("file/now-dir", "4"),
            ("docs/x", "4"),
            ("docs/copy", "4"),
        ],
    );
    let paths = |placed: Vec<Placed>| -> Vec<String> {
        placed
            .iter()
            .map(|placed| placed.path.to_string())
            .collect()
    };

    assert_eq!(
        paths(store.added(new, &[old], &[]).unwrap()),
        ["/", "docs", "docs/copy", "edit", "edit/b", "file"],
        "`file/now-dir` has the content of `docs/copy`, listed first"
    );
    assert_eq!(
        store.added(new, &[], &[]).unwrap().len(),
        8,
        "5 trees, and 5 files of 3 contents"
    );
    assert_eq!(store.added(new, &[new], &[]).unwrap(), []);
    let empty = build(&store, &[]);
    assert_eq!(
        paths(store.added(new, &[empty, old], &[]).unwrap()),
        paths(store.added(new, &[old], &[]).unwrap()),
        "any have holding an object is enough"
    );

    let only = |path: &str| [RepoPath::new(path).unwrap()];
    assert_eq!(
        paths(store.added(new, &[], &only("edit/b")).unwrap()),
        ["/", "edit", "edit/b"],
        "the trees on the way, then the path"
    );
    assert_eq!(
        paths(store.added(new, &[old], &only("same")).unwrap()),
        ["/"]
    );
}

/// The files of `tree`, by path, with their contents' ids.
fn files(store: &Store, tree: TreeId) -> Vec<(String, ObjectId)> {
    let empty = build(store, &[]);
    store
        .diff(empty, tree)
        .unwrap()
        .into_iter()
        .map(|change| (change.path.as_str().to_owned(), change.new.unwrap().id))
        .collect()
}

#[test]
fn merges_changes_to_different_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    let base = build(
        &store,
        &[
            ("src/a", "a"),
            ("src/b", "b"),
            ("gone/x", "x"),
            ("same", "s"),
        ],
    );
    // Ours edits a file and removes a directory; theirs edits another
    // file of the same directory, adds one, and makes the same edit.
    let ours = build(&store, &[("src/a", "a2"), ("src/b", "b"), ("same", "s2")]);
    let theirs = build(
        &store,
        &[
            ("src/a", "a"),
            ("src/b", "b2"),
            ("gone/x", "x"),
            ("new/deep", "n"),
            ("same", "s2"),
        ],
    );
    let merge = store.merge(Some(base), ours, theirs).unwrap();
    assert_eq!(merge.conflicts, Conflicts::default());
    let expected = build(
        &store,
        &[
            ("src/a", "a2"),
            ("src/b", "b2"),
            ("new/deep", "n"),
            ("same", "s2"),
        ],
    );
    assert_eq!(files(&store, merge.tree), files(&store, expected));
    assert_eq!(merge.tree, expected, "trees are canonical");
    assert!(
        merge.trees.iter().any(|tree| tree.id() == expected.into()),
        "the merged trees are returned to be written"
    );

    // Merging onto an unchanged base reads nothing and makes no tree.
    let unchanged = store.merge(Some(base), ours, base).unwrap();
    assert_eq!((unchanged.tree, unchanged.trees), (ours, Vec::new()));
}

#[test]
fn conflicts_keep_ours() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    let base = build(
        &store,
        &[("edit", "1"), ("dir/removed", "r"), ("file", "f")],
    );
    let ours = build(
        &store,
        &[("edit", "ours"), ("dir/removed", "r2"), ("file", "f")],
    );
    let theirs = build(
        &store,
        &[("edit", "theirs"), ("file/now-dir", "d"), ("other", "o")],
    );
    let merge = store.merge(Some(base), ours, theirs).unwrap();
    let paths: Vec<_> = merge
        .conflicts
        .paths()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        paths,
        ["dir/removed", "edit"],
        "both edited `edit`; ours edited what theirs removed"
    );
    let expected = build(
        &store,
        &[
            ("edit", "ours"),
            ("dir/removed", "r2"),
            ("file/now-dir", "d"),
            ("other", "o"),
        ],
    );
    assert_eq!(files(&store, merge.tree), files(&store, expected));

    // Without a base, both sides added everything.
    let unrelated = store.merge(None, ours, theirs).unwrap();
    let paths: Vec<_> = unrelated
        .conflicts
        .paths()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(paths, ["edit", "file"]);
}

#[test]
fn looks_entries_up_by_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    let root = build(&store, &[("src/lib.rs", "lib"), ("README", "r")]);
    let mut snapshots = Snapshots::new(&store);
    let mut at = |path: &str| {
        snapshots
            .entry_at(root, &RepoPath::new(path).unwrap())
            .unwrap()
    };

    assert_eq!(at("src/lib.rs"), Some(file("lib")));
    assert_eq!(at("").unwrap().id, root.into(), "the root is the snapshot");
    assert!(at("src").unwrap().kind.is_tree());
    for missing in ["nope", "src/nope", "README/inside"] {
        assert_eq!(at(missing), None, "{missing}");
    }
}
