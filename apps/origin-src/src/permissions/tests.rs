use std::sync::Arc;

use origin_platform::Native;
use origin_shared::api::{FetchSnapshot, LandRequest};
use origin_shared::landing::{Landing, Proposal};
use origin_shared::pack::Pack;
use origin_shared::{
    AttachmentId, Base, ChangeId, EncodedObject, EntryKind, EntryName, NodeKey, ObjectKind, Record,
    Revision, Statement, Tree, TreeEntry, TreeId,
};

use super::*;
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
        let node = Node {
            name: "queue".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[7; 32])),
        };
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

    let result = |subject| {
        Statement {
            verdict: Some(origin_shared::Verdict::Pass),
            inputs: Some(origin_shared::Inputs {
                paths: vec![path("README.md")],
                definition: origin_shared::Digest::of(b"test"),
                toolchain: origin_shared::Digest::of(b""),
                external: false,
            }),
            ..statement(subject, "ci.test", "user:ada")
        }
        .encode()
    };
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
