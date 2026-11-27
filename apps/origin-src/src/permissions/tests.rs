use origin_platform::Native;
use origin_shared::api::{FetchSnapshot, LandRequest};
use origin_shared::landing::{Landing, Proposal};
use origin_shared::pack::Pack;
use origin_shared::{
    AttachmentId, Base, ChangeId, EncodedObject, EntryKind, EntryName, NodeKey, ObjectKind, Record,
    Revision, Statement, Tree, TreeEntry, TreeId,
};

use super::*;
use crate::conflict::Conflicts;
use crate::queue::tests::Repo;
use crate::service::{Node, OriginService, ServiceError};
use crate::tree::Snapshots;
use crate::tree::tests::build;

/// Ada writes everything and changes the rules. Eve reads all but
/// `secrets`, writes only `src`, and issues reviews and checks. Both
/// land on `main`.
const RULES: &str = r#"
[targets.main]
authority = "node:queue"

[grants]
"user:ada" = ["admin", "land *"]
"user:eve" = ["land *", "annotate review", "annotate ci.*"]

[[paths]]
path = "/"
grants = { all = "read", "user:ada" = "write" }

[[paths]]
path = "src"
grants = { "user:eve" = "write" }

[[paths]]
path = "secrets"
grants = { all = "none", "user:ada" = "write" }
"#;

/// A repository whose `main` holds a README, a library, a secret and a
/// protected file, with its service.
struct Fixture {
    repo: Repo,
    service: OriginService,
}

impl Fixture {
    fn new() -> Self {
        let repo = Repo::new(RULES);
        repo.change_on_main(
            1,
            &[
                ("README.md", "hello"),
                ("src/lib.rs", "fn a() {}"),
                ("secrets/key", "hunter2"),
                ("protected/file", "keep"),
            ],
        );
        repo.land(1);
        let node = Node::new("queue".parse().unwrap(), NodeKey::from_bytes(&[7; 32]));
        let service = OriginService::new(repo.store.clone(), node);
        Self { repo, service }
    }

    fn tip(&self, target: &TargetName) -> Base {
        Base::Acceptance(self.repo.tip(target).acceptance)
    }

    fn state(&self, target: &TargetName) -> TreeId {
        self.repo.store.base_snapshot(self.tip(target)).unwrap()
    }

    /// Returns the objects of a snapshot of `files`, and its id, without
    /// writing them.
    fn objects(files: &[(&str, &str)]) -> (TreeId, Vec<EncodedObject>) {
        let dir = tempfile::tempdir().unwrap();
        let scratch = Store::new(dir.path().join(".origin"), Native::default());
        scratch.init().unwrap();
        let blobs: Vec<_> = files
            .iter()
            .map(|(_, content)| EncodedObject::blob(content.as_bytes()))
            .collect();
        scratch.write_all(&blobs).unwrap();
        let tree = build(&scratch, files);
        (tree, scratch.read_all(&scratch.ids().unwrap()).unwrap())
    }

    /// A revision of `change` by `author`, with `snapshot` on `base`, and its
    /// description.
    fn revision(
        change: u8,
        author: &str,
        base: Base,
        predecessors: Vec<RevisionId>,
        snapshot: TreeId,
    ) -> (RevisionId, Vec<EncodedObject>) {
        let description = EncodedObject::attachment(b"work");
        let revision = Revision {
            change: ChangeId::from_bytes([change; 16]),
            predecessors,
            base,
            deps: Vec::new(),
            snapshot,
            description: description.id().into(),
            author: author.parse().unwrap(),
        }
        .encode();
        (revision.id().into(), vec![description, revision])
    }

    fn put(&self, caller: &Caller, objects: Vec<EncodedObject>) -> Result<(), ServiceError> {
        self.service
            .put_records(caller, Pack { objects })
            .map(|_| ())
    }

    fn land(&self, caller: &Caller, proposal: Proposal) -> Result<Vec<Landing>, ServiceError> {
        let request = LandRequest {
            proposal,
            onto: Some(TargetName::MAIN),
        };
        Ok(self.service.land(caller, &request)?.landings)
    }

    /// What `main` holds at `path`.
    fn at_main(&self, path: &str) -> Option<TreeEntry> {
        Snapshots::new(&self.repo.store)
            .entry_at(self.state(&TargetName::MAIN), &RepoPath::new(path).unwrap())
            .unwrap()
    }
}

fn caller(principal: &str) -> Caller {
    Caller::Principal {
        principal: principal.parse().unwrap(),
        scope: None,
    }
}

fn forbidden(result: Result<impl std::fmt::Debug, ServiceError>) -> Forbidden {
    match result {
        Err(
            ServiceError::Forbidden(forbidden)
            | ServiceError::Land(crate::queue::LandError::Forbidden(forbidden)),
        ) => forbidden,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn path(path: &str) -> RepoPath {
    RepoPath::new(path).unwrap()
}

#[test]
fn records_never_carry_a_file_as_their_attachment() {
    let fixture = Fixture::new();
    let eve = caller("user:eve");
    let secret = fixture.at_main("secrets/key").unwrap().id;

    let base = fixture.tip(&TargetName::MAIN);
    let crafted = Revision {
        change: ChangeId::from_bytes([2; 16]),
        predecessors: Vec::new(),
        base,
        deps: Vec::new(),
        snapshot: fixture.state(&TargetName::MAIN),
        description: AttachmentId::from(secret),
        author: "user:eve".parse().unwrap(),
    }
    .encode();
    let error = fixture.put(&eve, vec![crafted.clone()]).unwrap_err();
    assert!(
        matches!(
            error,
            ServiceError::WrongKind {
                kind: ObjectKind::Attachment,
                ..
            }
        ),
        "{error}"
    );
    let statement = Statement {
        subject: RecordId::Revision(fixture.repo.change_on_main(3, &[])),
        kind: Kind::new("review").unwrap(),
        verdict: None,
        issuer: "user:eve".parse().unwrap(),
        inputs: None,
        payload: Some(AttachmentId::from(secret)),
        supersedes: None,
        promoted_from: None,
    }
    .encode();
    let error = fixture.put(&eve, vec![statement]).unwrap_err();
    assert!(
        matches!(
            error,
            ServiceError::WrongKind {
                kind: ObjectKind::Attachment,
                ..
            }
        ),
        "{error}"
    );

    fixture
        .repo
        .store
        .write_all(std::slice::from_ref(&crafted))
        .unwrap();
    let fetched = fixture.service.records(&eve, &[crafted.id()]).unwrap();
    assert_eq!(fetched.pack.objects, [crafted]);
}

#[test]
fn pushes_cannot_place_unreadable_objects_where_they_read() {
    let fixture = Fixture::new();
    let eve = caller("user:eve");
    let base = fixture.tip(&TargetName::MAIN);
    let state = fixture.state(&TargetName::MAIN);
    let secret = fixture.at_main("secrets/key").unwrap();
    let store = &fixture.repo.store;

    let mut src: Tree = store
        .get(fixture.at_main("src").unwrap().subtree().unwrap())
        .unwrap();
    src.insert(EntryName::new("stolen").unwrap(), secret);
    let src = src.encode();
    let mut root: Tree = store.get(state).unwrap();
    let entry = TreeEntry {
        kind: EntryKind::Tree,
        id: src.id(),
    };
    root.insert(EntryName::new("src").unwrap(), entry);
    let root = root.encode();
    let (id, records) = Fixture::revision(2, "user:eve", base, Vec::new(), root.id().into());
    let mut objects = vec![src, root];
    objects.extend(records);
    let error = forbidden(fixture.put(&eve, objects));
    assert_eq!(
        error,
        Forbidden::Unsupplied {
            revision: id,
            path: path("src/stolen"),
        }
    );
    assert!(!store.contains(id.into()).unwrap());

    let request = FetchSnapshot {
        of: RecordId::Snapshot(fixture.at_main("secrets").unwrap().subtree().unwrap()),
        have: Vec::new(),
        paths: Vec::new(),
        skip: 0,
    };
    assert!(matches!(
        fixture.service.snapshot(&eve, &request),
        Err(ServiceError::NotASnapshot(_))
    ));

    let mut files = vec![("README.md", "hello"), ("src/lib.rs", "fn a() {}")];
    files.push(("src/own.rs", "hunter2"));
    let (tree, mut objects) = Fixture::objects(&files);
    let (tree, trees) = {
        let mut root: Tree = objects
            .iter()
            .find(|object| object.id() == tree.into())
            .unwrap()
            .decode_as()
            .unwrap();
        for kept in ["secrets", "protected"] {
            root.insert(
                EntryName::new(kept).unwrap(),
                fixture.at_main(kept).unwrap(),
            );
        }
        let root = root.encode();
        (TreeId::from(root.id()), vec![root])
    };
    objects.extend(trees);
    let (_, records) = Fixture::revision(3, "user:eve", base, Vec::new(), tree);
    objects.extend(records);
    fixture.put(&eve, objects).unwrap();
}

#[test]
fn landings_never_change_what_the_submitter_cannot_write() {
    let fixture = Fixture::new();
    let (ada, eve) = (caller("user:ada"), caller("user:eve"));
    let first = fixture.tip(&TargetName::MAIN);
    let old_state = fixture.state(&TargetName::MAIN);
    let repo = &fixture.repo;
    repo.change_on_main(2, &[("protected/file", "v2")]);
    repo.land(2);

    let mut files = vec![
        ("README.md", "hello"),
        ("src/lib.rs", "fn a() {}"),
        ("secrets/key", "hunter2"),
        ("protected/file", "keep"),
    ];
    files.push(("src/new.rs", "fn b() {}"));
    let (old_base, _) = (repo.revision(3, &[], first, &files), old_state);
    let landed = fixture
        .land(&eve, Proposal::Change(ChangeId::from_bytes([3; 16])))
        .unwrap();
    assert!(
        matches!(landed[..], [Landing::Landed { .. }]),
        "{old_base} {landed:?}"
    );
    let v2 = EncodedObject::blob(b"v2").id();
    assert_eq!(fixture.at_main("protected/file").unwrap().id, v2);

    files.retain(|(path, _)| *path != "protected/file");
    repo.revision(4, &[], fixture.tip(&TargetName::MAIN), &files);
    let change = Proposal::Change(ChangeId::from_bytes([4; 16]));
    assert_eq!(
        forbidden(fixture.land(&eve, change.clone())),
        Forbidden::Write(path("protected/file"))
    );
    assert!(fixture.at_main("protected/file").is_some());

    let side = TargetName::new("side").unwrap();
    repo.queue()
        .create_target(&side, &TargetName::MAIN)
        .unwrap();
    repo.queue()
        .land_change(
            &mut repo.annotations(),
            ChangeId::from_bytes([4; 16]),
            &side,
        )
        .unwrap();
    let from_side = Proposal::Target(side);
    assert_eq!(
        forbidden(fixture.land(&eve, from_side.clone())),
        Forbidden::Write(path("protected/file"))
    );
    let landed = fixture.land(&ada, from_side).unwrap();
    assert!(matches!(landed[..], [Landing::Landed { .. }]), "{landed:?}");
    assert!(fixture.at_main("protected/file").is_none());
}

#[test]
fn converging_cannot_smuggle_others_changes() {
    let fixture = Fixture::new();
    let eve = caller("user:eve");
    let repo = &fixture.repo;
    let base = fixture.tip(&TargetName::MAIN);
    let files = [
        ("README.md", "hello"),
        ("src/lib.rs", "fn a() {}"),
        ("secrets/key", "hunter2"),
    ];
    let first = repo.revision(
        5,
        &[],
        base,
        &[&files[..], &[("protected/file", "keep")]].concat(),
    );
    let ada_head = repo.revision(
        5,
        &[first],
        base,
        &[&files[..], &[("protected/file", "ada")]].concat(),
    );
    let mut eves = files.to_vec();
    eves[1] = ("src/lib.rs", "fn eve() {}");
    eves.push(("protected/file", "keep"));
    let eve_head = repo.revision(5, &[first], base, &eves);

    let merged = repo
        .store
        .merge(
            Some(repo.store.get(first).unwrap().snapshot),
            repo.store.get(eve_head).unwrap().snapshot,
            repo.store.get(ada_head).unwrap().snapshot,
        )
        .unwrap();
    let (id, records) =
        Fixture::revision(5, "user:eve", base, vec![ada_head, eve_head], merged.tree);
    let mut objects = merged.trees.clone();
    objects.extend(records.clone());
    assert_eq!(
        forbidden(fixture.put(&eve, objects)),
        Forbidden::Write(path("protected/file"))
    );

    repo.store.write_all(&merged.trees).unwrap();
    repo.store.write_all(&records).unwrap();
    assert_eq!(
        repo.annotations()
            .index()
            .heads(ChangeId::from_bytes([5; 16])),
        [id]
    );
    assert_eq!(
        forbidden(fixture.land(&eve, Proposal::Change(ChangeId::from_bytes([5; 16])))),
        Forbidden::Write(path("protected/file"))
    );
}

/// `main`'s files, with `edits` applied.
fn main_with(edits: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    let main = [
        ("README.md", "hello"),
        ("src/lib.rs", "fn a() {}"),
        ("secrets/key", "hunter2"),
        ("protected/file", "keep"),
    ];
    main.iter()
        .map(|&(at, content)| {
            let edit = edits.iter().find(|(edited, _)| *edited == at);
            edit.copied().unwrap_or((at, content))
        })
        .collect()
}

#[test]
fn successors_have_their_predecessors_effect_where_their_writer_cannot_write() {
    let fixture = Fixture::new();
    let (eve, repo) = (caller("user:eve"), &fixture.repo);
    let base = fixture.tip(&TargetName::MAIN);
    let succeed = |predecessors: Vec<RevisionId>, base, files: &[(&str, &str)]| {
        let (tree, mut objects) = Fixture::objects(files);
        let (id, records) = Fixture::revision(30, "user:eve", base, predecessors, tree);
        objects.extend(records);
        fixture.put(&eve, objects).map(|()| id)
    };

    let edits = [("README.md", "ada"), ("src/lib.rs", "fn ada() {}")];
    let adas = repo.revision(30, &[], base, &main_with(&edits));
    let undone = main_with(&[("src/lib.rs", "fn ada() {}")]);
    assert_eq!(
        forbidden(succeed(vec![adas], base, &undone)),
        Forbidden::Write(path("README.md"))
    );

    let edits = [("README.md", "ada"), ("src/lib.rs", "fn eve() {}")];
    let taken = succeed(vec![adas], base, &main_with(&edits)).unwrap();

    repo.change_on_main(31, &[("protected/file", "newer")]);
    repo.land(31);
    let tip = fixture.tip(&TargetName::MAIN);
    let rebased = [&edits[..], &[("protected/file", "newer")]].concat();
    let rebased = succeed(vec![taken], tip, &main_with(&rebased)).unwrap();

    let edits = [("README.md", "ada"), ("src/lib.rs", "fn other() {}")];
    let other = succeed(vec![taken], base, &main_with(&edits)).unwrap();
    let edits = [("README.md", "ada"), ("src/lib.rs", "fn both() {}")];
    succeed(vec![rebased, other], base, &main_with(&edits)).unwrap();
    let landed = fixture
        .land(
            &caller("user:ada"),
            Proposal::Change(ChangeId::from_bytes([30; 16])),
        )
        .unwrap();
    assert!(matches!(landed[..], [Landing::Landed { .. }]), "{landed:?}");
    assert_eq!(
        fixture.at_main("README.md").unwrap().id,
        EncodedObject::blob(b"ada").id()
    );
}

/// Probes from the third review: a successor that keeps ada's files, but
/// on a base that already has her edits, so it changes nothing.
#[test]
fn no_base_makes_a_successor_drop_its_predecessors_effect() {
    let fixture = Fixture::new();
    let repo = &fixture.repo;
    let reader = scoped("user:eve", &["read /"], in_a_week());
    let base = fixture.tip(&TargetName::MAIN);
    let state = fixture.state(&TargetName::MAIN);
    let edits = [("README.md", "ada"), ("src/lib.rs", "fn ada() {}")];
    let adas = repo.revision(40, &[], base, &main_with(&edits));
    let snapshot = repo.store.get(adas).unwrap().snapshot;
    let succeed = |base, predecessors| {
        let (id, records) = Fixture::revision(40, "user:eve", base, predecessors, snapshot);
        fixture.put(&reader, records).map(|()| id)
    };

    assert_eq!(
        forbidden(succeed(Base::Revision(adas), vec![adas])),
        Forbidden::Below(ChangeId::from_bytes([40; 16]))
    );
    let (on_adas, records) =
        Fixture::revision(41, "user:eve", Base::Revision(adas), Vec::new(), snapshot);
    fixture.put(&reader, records).unwrap();
    assert_eq!(
        forbidden(succeed(Base::Revision(on_adas), vec![adas])),
        Forbidden::Below(ChangeId::from_bytes([41; 16]))
    );
    repo.change_on_main(42, &edits);
    repo.land(42);
    let tip = fixture.tip(&TargetName::MAIN);
    assert_eq!(
        forbidden(succeed(tip, vec![adas])),
        Forbidden::Write(path("README.md"))
    );

    let taken = succeed(base, vec![adas]).unwrap();
    let (_, records) = Fixture::revision(40, "user:eve", base, vec![taken], state);
    assert_eq!(
        forbidden(fixture.put(&reader, records)),
        Forbidden::Write(path("README.md"))
    );
}

#[test]
fn changes_keep_their_history() {
    let fixture = Fixture::new();
    let repo = &fixture.repo;
    let reader = scoped("user:eve", &["read /"], in_a_week());
    let base = fixture.tip(&TargetName::MAIN);
    let state = fixture.state(&TargetName::MAIN);
    let change = ChangeId::from_bytes([43; 16]);
    let adas = repo.revision(43, &[], base, &main_with(&[("README.md", "ada")]));
    let snapshot = repo.store.get(adas).unwrap().snapshot;

    let (_, records) = Fixture::revision(43, "user:eve", base, Vec::new(), state);
    assert_eq!(
        forbidden(fixture.put(&reader, records)),
        Forbidden::History(change)
    );
    let (eves, records) = Fixture::revision(44, "user:eve", base, Vec::new(), state);
    fixture.put(&reader, records).unwrap();
    let (_, records) = Fixture::revision(43, "user:eve", base, vec![adas, eves], snapshot);
    assert_eq!(
        forbidden(fixture.put(&reader, records)),
        Forbidden::History(change)
    );
    let (_, mut records) = Fixture::revision(45, "user:ada", base, Vec::new(), state);
    records.extend(Fixture::revision(45, "user:ada", base, Vec::new(), snapshot).1);
    assert_eq!(
        forbidden(fixture.put(&caller("user:ada"), records)),
        Forbidden::History(ChangeId::from_bytes([45; 16]))
    );

    let abandon = statement(RecordId::Revision(adas), "core.abandon", "user:ada").encode();
    fixture.put(&caller("user:ada"), vec![abandon]).unwrap();
    let landed = fixture.land(&caller("user:ada"), Proposal::Change(change));
    assert!(
        matches!(
            landed,
            Err(ServiceError::Land(crate::queue::LandError::Abandoned(_)))
        ),
        "{landed:?}"
    );
}

#[test]
fn successors_resolve_no_conflict_their_writer_cannot_write() {
    let fixture = Fixture::new();
    let (eve, repo) = (caller("user:eve"), &fixture.repo);
    let base = fixture.tip(&TargetName::MAIN);
    let marked = main_with(&[("README.md", "<<<<<<< ours")]);
    let adas = repo.revision(46, &[], base, &marked);
    let snapshot = repo.store.get(adas).unwrap().snapshot;
    let readme = Snapshots::new(&repo.store)
        .entry_at(snapshot, &path("README.md"))
        .unwrap();
    let conflicts = Conflicts::new(vec![crate::conflict::Conflicted {
        path: path("README.md"),
        written: readme.map(|entry| entry.id),
    }]);
    let listing = |subject, issuer: &str, supersedes| {
        let (payload, statement) = conflicts.statement(
            RecordId::Revision(subject),
            issuer.parse().unwrap(),
            supersedes,
        );
        (payload, statement.encode())
    };
    let (payload, adas_listing) = listing(adas, "user:ada", None);
    repo.store.write_all(&[payload, adas_listing]).unwrap();

    let (taken, records) = Fixture::revision(46, "user:eve", base, vec![adas], snapshot);
    assert_eq!(
        forbidden(fixture.put(&eve, records.clone())),
        Forbidden::Resolve(path("README.md"))
    );
    let (payload, kept) = listing(taken, "user:eve", None);
    let kept_id = StatementId::from(kept.id());
    fixture
        .put(&eve, [vec![payload, kept], records].concat())
        .unwrap();

    let (payload, empty) = Conflicts::default().statement(
        RecordId::Revision(taken),
        "user:eve".parse().unwrap(),
        Some(kept_id),
    );
    assert_eq!(
        forbidden(fixture.put(&eve, vec![payload, empty.encode()])),
        Forbidden::Resolve(path("README.md"))
    );
    let review = Statement {
        supersedes: Some(kept_id),
        ..statement(RecordId::Revision(taken), "review", "user:eve")
    };
    assert_eq!(
        forbidden(fixture.put(&eve, vec![review.encode()])),
        Forbidden::Resolve(path("README.md"))
    );
    let landed = fixture.land(
        &caller("user:ada"),
        Proposal::Change(ChangeId::from_bytes([46; 16])),
    );
    assert!(
        matches!(
            landed,
            Err(ServiceError::Land(
                crate::queue::LandError::Unresolved { .. }
            ))
        ),
        "{landed:?}"
    );
}

#[test]
fn revisions_add_no_conflict_their_writer_cannot_write() {
    let fixture = Fixture::new();
    let (eve, base) = (caller("user:eve"), fixture.tip(&TargetName::MAIN));
    let mallory = scoped("user:mallory", &["read /"], in_a_week());
    let change = ChangeId::from_bytes([47; 16]);
    let (tree, mut objects) = Fixture::objects(&main_with(&[("src/lib.rs", "fn eve() {}")]));
    let (eves, records) = Fixture::revision(47, "user:eve", base, Vec::new(), tree);
    objects.extend(records);
    fixture.put(&eve, objects).unwrap();
    let listing = |subject, issuer: &str| {
        let readme = fixture.at_main("README.md").map(|entry| entry.id);
        let conflicts = Conflicts::new(vec![crate::conflict::Conflicted {
            path: path("README.md"),
            written: readme,
        }]);
        let (payload, statement) =
            conflicts.statement(RecordId::Revision(subject), issuer.parse().unwrap(), None);
        vec![payload, statement.encode()]
    };
    let (mallorys, mut records) = Fixture::revision(47, "user:mallory", base, vec![eves], tree);
    records.extend(listing(mallorys, "user:mallory"));
    assert_eq!(
        forbidden(fixture.put(&mallory, records)),
        Forbidden::Conflict(path("README.md"))
    );
    let (root, mut records) = Fixture::revision(48, "user:eve", base, Vec::new(), tree);
    records.extend(listing(root, "user:eve"));
    assert_eq!(
        forbidden(fixture.put(&eve, records)),
        Forbidden::Conflict(path("README.md"))
    );
    let landed = fixture.land(&eve, Proposal::Change(change)).unwrap();
    assert!(matches!(landed[..], [Landing::Landed { .. }]), "{landed:?}");

    let marked = main_with(&[("README.md", "<<<<<<< ours")]);
    let adas = fixture.repo.revision(49, &[], base, &marked);
    let snapshot = fixture.repo.store.get(adas).unwrap().snapshot;
    let readme = Snapshots::new(&fixture.repo.store)
        .entry_at(snapshot, &path("README.md"))
        .unwrap();
    let conflicts = Conflicts::new(vec![crate::conflict::Conflicted {
        path: path("README.md"),
        written: readme.map(|entry| entry.id),
    }]);
    let carry = |subject, issuer: &str| {
        let (payload, statement) =
            conflicts.statement(RecordId::Revision(subject), issuer.parse().unwrap(), None);
        vec![payload, statement.encode()]
    };
    fixture
        .repo
        .store
        .write_all(&carry(adas, "user:ada"))
        .unwrap();
    let on_adas = Base::Revision(adas);
    let (root, records) = Fixture::revision(50, "user:eve", on_adas, Vec::new(), snapshot);
    assert_eq!(
        forbidden(fixture.put(&eve, records.clone())),
        Forbidden::Resolve(path("README.md"))
    );
    fixture
        .put(&eve, [records, carry(root, "user:eve")].concat())
        .unwrap();
}

/// A `core.conflict` by `issuer` about `subject`, superseding
/// `supersedes`, that lists a conflict at each of `paths` with the entry
/// `snapshot` has there. Returns its id, then its payload and itself,
/// encoded.
fn listing(
    store: &Store<Native>,
    snapshot: TreeId,
    paths: &[&str],
    (subject, issuer): (RevisionId, &str),
    supersedes: Option<StatementId>,
) -> (StatementId, Vec<EncodedObject>) {
    let mut snapshots = Snapshots::new(store);
    let conflicted = paths.iter().map(|&at| crate::conflict::Conflicted {
        path: path(at),
        written: snapshots
            .entry_at(snapshot, &path(at))
            .unwrap()
            .map(|entry| entry.id),
    });
    let (payload, statement) = Conflicts::new(conflicted.collect()).statement(
        RecordId::Revision(subject),
        issuer.parse().unwrap(),
        supersedes,
    );
    let statement = statement.encode();
    (statement.id().into(), vec![payload, statement])
}

#[test]
fn superseding_another_issuers_listing_reads_nothing_of_it() {
    let fixture = Fixture::new();
    let repo = &fixture.repo;
    let base = fixture.tip(&TargetName::MAIN);
    let marked = main_with(&[("secrets/key", "<<<<<<< ours")]);
    let first = repo.revision(52, &[], base, &marked);
    let second = repo.revision(52, &[first], base, &marked);
    let snapshot = repo.store.get(first).unwrap().snapshot;
    let at = ["secrets/key"];
    let (_, firsts) = listing(&repo.store, snapshot, &at, (first, "user:ada"), None);
    let (seconds, records) = listing(&repo.store, snapshot, &at, (second, "user:ada"), None);
    repo.store.write_all(&[firsts, records].concat()).unwrap();

    let visible = repo.change_on_main(53, &[("src/lib.rs", "fn b() {}")]);
    let review = Statement {
        supersedes: Some(seconds),
        ..statement(RecordId::Revision(visible), "review", "user:eve")
    };
    fixture
        .put(&caller("user:eve"), vec![review.encode()])
        .unwrap();
}

#[test]
fn narrowed_tokens_keep_their_bearers_listings() {
    let fixture = Fixture::new();
    let repo = &fixture.repo;
    let base = fixture.tip(&TargetName::MAIN);
    let adas = repo.revision(54, &[], base, &main_with(&[("README.md", "<<<<<<< ours")]));
    let snapshot = repo.store.get(adas).unwrap().snapshot;
    let readme = ["README.md"];
    let (listed, records) = listing(&repo.store, snapshot, &readme, (adas, "user:ada"), None);
    repo.store.write_all(&records).unwrap();

    let narrowed = scoped("user:ada", &["read /", "write src"], in_a_week());
    let (_, empty) = listing(&repo.store, snapshot, &[], (adas, "user:ada"), Some(listed));
    assert_eq!(
        forbidden(fixture.put(&narrowed, empty)),
        Forbidden::Resolve(path("README.md"))
    );
}

#[test]
fn listings_put_again_change_nothing() {
    let fixture = Fixture::new();
    let repo = &fixture.repo;
    let base = fixture.tip(&TargetName::MAIN);
    let adas = repo.revision(55, &[], base, &main_with(&[("README.md", "<<<<<<< ours")]));
    let snapshot = repo.store.get(adas).unwrap().snapshot;
    let readme = ["README.md"];
    let (listed, old) = listing(&repo.store, snapshot, &readme, (adas, "user:ada"), None);
    let (_, resolved) = listing(&repo.store, snapshot, &[], (adas, "user:ada"), Some(listed));
    repo.store
        .write_all(&[old.clone(), resolved].concat())
        .unwrap();

    let eve = caller("user:eve");
    let (taken, records) = Fixture::revision(55, "user:eve", base, vec![adas], snapshot);
    let (_, carried) = listing(&repo.store, snapshot, &readme, (taken, "user:eve"), None);
    let carrying = [old.clone(), records.clone(), carried].concat();
    assert_eq!(
        forbidden(fixture.put(&eve, carrying)),
        Forbidden::Conflict(path("README.md"))
    );
    fixture.put(&eve, [old, records].concat()).unwrap();
}

#[test]
fn rebases_keep_the_conflicts_they_carry() {
    let fixture = Fixture::new();
    let repo = &fixture.repo;
    let base = fixture.tip(&TargetName::MAIN);
    let adas = repo.revision(56, &[], base, &main_with(&[("src/lib.rs", "fn ada() {}")]));
    let snapshot = repo.store.get(adas).unwrap().snapshot;
    let (_, records) = listing(
        &repo.store,
        snapshot,
        &["README.md"],
        (adas, "user:ada"),
        None,
    );
    repo.store.write_all(&records).unwrap();

    repo.change_on_main(57, &[("README.md", "newer")]);
    repo.land(57);
    let tip = fixture.tip(&TargetName::MAIN);
    let edits = [("README.md", "newer"), ("src/lib.rs", "fn ada() {}")];
    let (tree, mut objects) = Fixture::objects(&main_with(&edits));
    objects.extend(Fixture::revision(56, "user:eve", tip, vec![adas], tree).1);
    assert_eq!(
        forbidden(fixture.put(&caller("user:eve"), objects)),
        Forbidden::Resolve(path("README.md"))
    );
}

#[test]
fn concurrent_roots_never_make_a_change_diverge() {
    let fixture = Fixture::new();
    let (ada, base) = (&caller("user:ada"), fixture.tip(&TargetName::MAIN));
    let state = fixture.state(&TargetName::MAIN);
    let change = ChangeId::from_bytes([58; 16]);
    let (_, first) = Fixture::revision(58, "user:ada", base, Vec::new(), state);
    let (tree, mut second) = Fixture::objects(&main_with(&[("README.md", "ada")]));
    second.extend(Fixture::revision(58, "user:ada", base, Vec::new(), tree).1);

    let publishing = fixture.repo.store.lock_pushes().unwrap();
    let fixture = &fixture;
    let pushed: Vec<_> = std::thread::scope(|scope| {
        let push = |objects| scope.spawn(move || fixture.put(ada, objects));
        let pushes = [first, second].map(push);
        std::thread::sleep(std::time::Duration::from_millis(100));
        drop(publishing);
        pushes.map(|push| push.join().unwrap()).into()
    });
    let refused: Vec<_> = pushed.into_iter().filter_map(Result::err).collect();
    let [refused] = &refused[..] else {
        panic!("expected one push refused, got {refused:?}")
    };
    assert!(
        matches!(refused, ServiceError::Forbidden(Forbidden::History(id)) if *id == change),
        "{refused:?}"
    );
}

#[test]
fn only_those_who_see_a_revision_succeed_it() {
    let fixture = Fixture::new();
    let adas = fixture
        .repo
        .change_on_main(30, &[("secrets/key", "rotated")]);
    let base = fixture.tip(&TargetName::MAIN);
    let state = fixture.state(&TargetName::MAIN);
    let (eves, mut objects) = Fixture::revision(30, "user:eve", base, vec![adas], state);
    objects.push(statement(RecordId::Revision(eves), "core.abandon", "user:eve").encode());
    let blind = scoped("user:eve", &[], in_a_week());
    assert!(fixture.put(&blind, objects.clone()).is_err());
    let put = fixture.put(&caller("user:eve"), objects);
    assert!(
        matches!(put, Err(ServiceError::UnknownRecord(record)) if record == RecordId::Revision(adas)),
        "{put:?}"
    );
    let change = Proposal::Change(ChangeId::from_bytes([30; 16]));
    let landed = fixture.land(&caller("user:ada"), change).unwrap();
    assert!(matches!(landed[..], [Landing::Landed { .. }]), "{landed:?}");
}

#[test]
fn tokens_narrowed_out_of_admin_abandon_nothing() {
    let fixture = Fixture::new();
    let base = fixture.tip(&TargetName::MAIN);
    let state = fixture.state(&TargetName::MAIN);
    let (eves, records) = Fixture::revision(31, "user:eve", base, Vec::new(), state);
    fixture.put(&caller("user:eve"), records).unwrap();
    let abandon = statement(RecordId::Revision(eves), "core.abandon", "user:ada").encode();
    let reader = scoped("user:ada", &["read /"], in_a_week());
    let refused = fixture.put(&reader, vec![abandon.clone()]).unwrap_err();
    assert_eq!(refused.code(), origin_shared::api::ErrorCode::Forbidden);
    let admin = scoped("user:ada", &["admin", "read /"], in_a_week());
    fixture.put(&admin, vec![abandon]).unwrap();
}

/// A statement of `kind` by `issuer` about `subject`.
fn statement(subject: RecordId, kind: &str, issuer: &str) -> Statement {
    Statement {
        subject,
        kind: Kind::new(kind).unwrap(),
        verdict: None,
        issuer: issuer.parse().unwrap(),
        inputs: None,
        payload: None,
        supersedes: None,
        promoted_from: None,
    }
}

/// A passing result of `kind` by ada about `subject`. It reads only the
/// README, so every revision of `main` shares its input digest.
fn readme_result(subject: RecordId, kind: &str) -> Statement {
    Statement {
        verdict: Some(origin_shared::Verdict::Pass),
        inputs: Some(origin_shared::Inputs {
            paths: vec![path("README.md")],
            target_paths: Vec::new(),
            definition: origin_shared::Digest::of(b"test"),
            toolchain: origin_shared::Digest::of(b""),
            external: false,
        }),
        ..statement(subject, kind, "user:ada")
    }
}

/// A caller for `principal`, with a token narrowing them to `scope` that
/// expires at `expires`.
fn scoped(principal: &str, scope: &[&str], expires: u64) -> Caller {
    Caller::Principal {
        principal: principal.parse().unwrap(),
        scope: Some(Scope {
            capabilities: scope.iter().map(|c| c.parse().unwrap()).collect(),
            expires,
        }),
    }
}

/// A week from now, in seconds since the Unix epoch.
fn in_a_week() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    (now.unwrap() + OriginService::MAX_TOKEN_TTL).as_secs()
}

#[test]
fn statements_are_attributed_to_their_issuer() {
    let fixture = Fixture::new();
    let revision = RecordId::Revision(fixture.repo.change_on_main(2, &[]));
    let statement = |issuer: &str, kind: &str| statement(revision, kind, issuer).encode();
    let eve = caller("user:eve");
    assert!(matches!(
        forbidden(fixture.put(&eve, vec![statement("user:ada", "review")])),
        Forbidden::NotYours { .. }
    ));
    fixture
        .put(&eve, vec![statement("user:eve", "review")])
        .unwrap();
    let scoped = scoped("user:eve", &["annotate ci.*"], in_a_week());
    assert_eq!(
        forbidden(fixture.put(&scoped, vec![statement("user:eve", "lint")])),
        Forbidden::Annotate(Kind::new("lint").unwrap())
    );
    assert_eq!(
        forbidden(fixture.put(&Caller::Anonymous, vec![statement("user:eve", "x")])),
        Forbidden::Anonymous
    );
}

#[test]
fn statements_are_about_what_their_issuer_sees_and_annotates() {
    let fixture = Fixture::new();
    let repo = &fixture.repo;
    let hidden = RecordId::Revision(repo.change_on_main(2, &[("secrets/key", "new")]));
    let visible = RecordId::Revision(repo.change_on_main(3, &[("src/lib.rs", "fn b() {}")]));
    let eve = caller("user:eve");
    let unseen = |result: Result<(), ServiceError>| match result {
        Err(ServiceError::UnknownRecord(record)) => record,
        other => panic!("expected an unknown record, got {other:?}"),
    };
    assert_eq!(
        unseen(fixture.put(&eve, vec![statement(hidden, "review", "user:eve").encode()])),
        hidden
    );

    let result = |subject| readme_result(subject, "ci.test").encode();
    let (of_hidden, of_visible) = (result(hidden), result(visible));
    repo.store
        .write_all(&[of_hidden.clone(), of_visible.clone()])
        .unwrap();
    let mine = RecordId::Revision(repo.change_on_main(4, &[("src/lib.rs", "fn c() {}")]));
    let promotion = |original: &EncodedObject| {
        Statement {
            subject: mine,
            issuer: "user:eve".parse().unwrap(),
            promoted_from: Some(original.id().into()),
            ..original.decode_as().unwrap()
        }
        .encode()
    };

    let reviewer = scoped("user:eve", &["annotate review"], in_a_week());
    assert_eq!(
        forbidden(fixture.put(&reviewer, vec![promotion(&of_visible)])),
        Forbidden::Annotate(Kind::new("ci.test").unwrap())
    );
    assert_eq!(
        unseen(fixture.put(&eve, vec![promotion(&of_hidden)])),
        RecordId::Statement(of_hidden.id().into())
    );
    fixture.put(&eve, vec![promotion(&of_visible)]).unwrap();

    let review = |result: &EncodedObject| {
        let subject = RecordId::Statement(result.id().into());
        statement(subject, "review", "user:eve").encode()
    };
    assert_eq!(
        unseen(fixture.put(&eve, vec![review(&of_hidden)])),
        RecordId::Statement(of_hidden.id().into())
    );
    fixture.put(&eve, vec![review(&of_visible)]).unwrap();
}

#[test]
fn promoting_copies_nothing_the_caller_cannot_see() {
    let fixture = Fixture::new();
    let repo = &fixture.repo;
    let hidden = RecordId::Revision(repo.change_on_main(2, &[("secrets/key", "new")]));
    let visible = RecordId::Revision(repo.change_on_main(3, &[("src/lib.rs", "fn b() {}")]));
    let payload = EncodedObject::attachment(b"a review of the new secret");
    let review = Statement {
        payload: Some(payload.id().into()),
        ..readme_result(hidden, "review")
    };
    repo.store.write_all(&[payload, review.encode()]).unwrap();
    let promote = |who: &str| fixture.service.promote(&caller(who), visible).unwrap();
    assert_eq!(promote("user:eve").len(), 0);
    let [promoted] = &promote("user:ada")[..] else {
        panic!("ada sees the review")
    };
    assert_eq!(promoted.statement.payload, review.payload);

    let eve = caller("user:eve");
    let naming = Statement {
        payload: review.payload,
        ..statement(visible, "review", "user:eve")
    };
    assert!(matches!(
        forbidden(fixture.put(&eve, vec![naming.encode()])),
        Forbidden::UnsuppliedAttachment { .. }
    ));
    let main = TargetName::MAIN;
    let (_, records) = Fixture::revision(
        5,
        "user:eve",
        fixture.tip(&main),
        Vec::new(),
        fixture.state(&main),
    );
    let mut revision: Revision = records[1].decode_as().unwrap();
    revision.description = review.payload.unwrap();
    assert!(matches!(
        forbidden(fixture.put(&eve, vec![revision.encode()])),
        Forbidden::UnsuppliedAttachment { .. }
    ));
    let promotion = Statement {
        subject: visible,
        issuer: "user:eve".parse().unwrap(),
        promoted_from: Some(review.encode().id().into()),
        ..review.clone()
    };
    match fixture.put(&eve, vec![review.encode(), promotion.encode()]) {
        Err(ServiceError::UnknownRecord(record)) => {
            assert_eq!(record, RecordId::Statement(review.encode().id().into()));
        }
        other => panic!("expected an unknown record, got {other:?}"),
    }
}

#[test]
fn reserved_kinds_come_from_their_authors_whatever_the_caller_annotates() {
    let fixture = Fixture::new();
    let theirs = RecordId::Revision(fixture.repo.change_on_main(2, &[]));
    let base = fixture.tip(&TargetName::MAIN);
    let (mine, records) = Fixture::revision(
        3,
        "user:eve",
        base,
        Vec::new(),
        fixture.state(&TargetName::MAIN),
    );
    let abandon = |subject| statement(subject, "core.abandon", "user:eve").encode();
    let eve = caller("user:eve");
    let error = fixture.put(&eve, vec![abandon(theirs)]).unwrap_err();
    assert!(
        matches!(&error, ServiceError::Statement { source, .. }
            if matches!(**source, crate::annotations::AnnotateError::Reserved { .. })),
        "{error}"
    );
    assert_eq!(error.code(), origin_shared::api::ErrorCode::Forbidden);
    let mut batch = records;
    batch.push(abandon(RecordId::Revision(mine)));
    fixture.put(&eve, batch).unwrap();
}

#[test]
fn batches_hold_stacks_and_statements_about_them() {
    let fixture = Fixture::new();
    let main = fixture.tip(&TargetName::MAIN);
    let files = [
        ("README.md", "hello"),
        ("src/lib.rs", "fn a() {}"),
        ("secrets/key", "hunter2"),
        ("protected/file", "keep"),
    ];
    let (lower_tree, mut objects) = Fixture::objects(&[&files[..], &[("src/a.rs", "a")]].concat());
    let (lower, records) = Fixture::revision(2, "user:eve", main, Vec::new(), lower_tree);
    objects.extend(records);
    let (upper_tree, more) =
        Fixture::objects(&[&files[..], &[("src/a.rs", "a"), ("src/b.rs", "b")]].concat());
    objects.extend(more);
    let (upper, records) =
        Fixture::revision(3, "user:eve", Base::Revision(lower), Vec::new(), upper_tree);
    objects.extend(records);
    objects.push(statement(RecordId::Revision(upper), "review", "user:eve").encode());
    fixture.put(&caller("user:eve"), objects).unwrap();
}

#[test]
fn callers_who_read_nothing_see_nothing() {
    let fixture = Fixture::new();
    let revision = fixture.repo.change_on_main(2, &[]);
    let blind = scoped("user:eve", &["annotate review"], in_a_week());
    let service = &fixture.service;
    assert!(!service.sees_repository(&blind).unwrap());
    let records = service.records(&blind, &[revision.into()]).unwrap();
    assert!(
        records.pack.objects.is_empty(),
        "not even a revision changing nothing"
    );
    assert_eq!(service.targets(&blind).unwrap(), []);
    let log = origin_shared::api::LogQuery {
        target: TargetName::MAIN,
        since: 0,
        limit: None,
    };
    assert!(matches!(
        service.log(&blind, &log),
        Err(ServiceError::UnknownTarget(_))
    ));
    assert_eq!(service.log(&caller("user:eve"), &log).unwrap().len(), 2);
}

#[test]
fn tokens_issued_from_a_token_expire_with_it() {
    let fixture = Fixture::new();
    let expires = in_a_week() - 6 * 24 * 3600;
    let parent = scoped("user:eve", &["annotate review", "land *"], expires);
    let request = origin_shared::api::TokenRequest {
        scope: vec!["annotate review".parse().unwrap()],
        ttl: origin_shared::Period::from_secs(OriginService::MAX_TOKEN_TTL.as_secs()),
    };
    let issued = fixture.service.issue_token(&parent, &request).unwrap();
    assert_eq!(issued.expires, expires);
    let issued = fixture
        .service
        .issue_token(&caller("user:eve"), &request)
        .unwrap();
    assert!(issued.expires > expires, "a static token's last a week");
}
