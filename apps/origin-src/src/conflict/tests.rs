use origin_platform::Native;
use origin_shared::{EntryKind, EntryName, Record, Tree, TreeEntry};

use super::*;

fn path(path: &str) -> RepoPath {
    RepoPath::new(path).expect("`RepoPath::new(path)` succeeds")
}

#[test]
fn a_path_resolves_once_its_entry_changes() {
    let dir = tempfile::tempdir().expect("`tempfile::tempdir()` succeeds");
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().expect("`init()` succeeds");
    let marked = EncodedObject::blob(b"<<<<<<< a\nours\n=======\ntheirs\n>>>>>>> b\n");
    let resolved = EncodedObject::blob(b"Heading\n=======\nresolved\n");
    let snapshot = |file: &EncodedObject| {
        let mut tree = Tree::default();
        tree.insert(
            EntryName::new("file").expect("`EntryName::new()` succeeds"),
            TreeEntry {
                kind: EntryKind::File,
                id: file.id(),
            },
        );
        let tree = tree.encode();
        store
            .write_all(&[file.clone(), tree.clone()])
            .expect("`write_all(&[file.clone(), tree.clone()])` succeeds");
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
        "user:ada".parse().expect("`parse()` succeeds"),
        None,
    );
    assert!(matches!(
        Conflicts::listed(&store, &statement),
        Err(StoreError::NotFound(id)) if id == payload.id()
    ));
    store
        .write_all(&[payload])
        .expect("`write_all(&[payload])` succeeds");
    assert_eq!(
        Conflicts::listed(&store, &statement)
            .expect("`Conflicts::listed(&store, &statement)` succeeds"),
        conflicts
    );

    let unchanged = conflicts.clone().remaining(&store, snapshot(&marked));
    assert_eq!(unchanged.expect("`unchanged` succeeds"), conflicts);
    let remaining = conflicts
        .remaining(&store, snapshot(&resolved))
        .expect("`remaining(&store, snapshot(&resolved))` succeeds");
    assert_eq!(remaining.paths(), [path("removed")]);
}

#[test]
fn only_a_user_author_leaves_a_revision_unresolved() {
    let dir = tempfile::tempdir().expect("`tempfile::tempdir()` succeeds");
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().expect("`init()` succeeds");
    let tree = Tree::default().encode();
    store
        .write_all(std::slice::from_ref(&tree))
        .expect("`write_all(std::slice::from_ref(&tree))` succeeds");
    let revision = |author: &str| Revision {
        change: origin_shared::ChangeId::from_bytes([1; 16]),
        predecessors: Vec::new(),
        base: origin_shared::Base::Acceptance(EncodedObject::blob(b"base").id().into()),
        deps: Vec::new(),
        snapshot: tree.id().into(),
        description: EncodedObject::blob(b"").id().into(),
        author: author.parse().expect("`parse()` succeeds"),
    };
    let subject = RecordId::Revision(revision("user:ada").encode().id().into());
    let conflicts = Conflicts::new(vec![Conflicted {
        path: path("file"),
        written: None,
    }]);
    let claims = |issuer: &str| {
        let (payload, statement) =
            conflicts.statement(subject, issuer.parse().expect("`parse()` succeeds"), None);
        store
            .write_all(&[payload])
            .expect("`write_all(&[payload])` succeeds");
        let id = statement.encode().id().into();
        [store
            .claim(id, statement)
            .expect("`claim(id, statement)` succeeds")
            .expect("`claim(id, statement)` yields a value")]
    };
    let ada = revision("user:ada");
    assert_eq!(
        Conflicts::of(&store, &claims("user:ada"), &ada).expect("`Conflicts::of()` succeeds"),
        conflicts
    );
    assert!(
        Conflicts::of(&store, &claims("user:eve"), &ada)
            .expect("`Conflicts::of()` succeeds")
            .is_empty()
    );
    let queue = revision("node:queue");
    assert!(
        Conflicts::of(&store, &claims("node:queue"), &queue)
            .expect("`Conflicts::of()` succeeds")
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
    let marked = Conflicts::mark(b"ours\n", b"theirs", ["a", "b"])
        .expect("`Conflicts::mark()` yields a value");
    assert_eq!(
        String::from_utf8(marked).expect("`String::from_utf8(marked)` succeeds"),
        "<<<<<<< a\nours\n=======\ntheirs\n>>>>>>> b\n"
    );
    assert_eq!(
        Conflicts::mark(b"", b"kept\n", ["a", "b"]).expect("`Conflicts::mark()` yields a value"),
        b"<<<<<<< a\n=======\nkept\n>>>>>>> b\n",
        "a removed side is empty"
    );
    assert_eq!(Conflicts::mark(b"\0binary", b"text", ["a", "b"]), None);
}
