//! The HTTP API, driven through the SDK against a real server: every SDK
//! method, over the server's own repositories.

use std::sync::Arc;

use origin_client::{Client, Error, Repo};
use origin_shared::api::{ErrorCode, FetchRecords, LogQuery, RepoName};
use origin_shared::pack::Pack;
use origin_shared::{
    Base, BlobId, ChangeId, Digest, EncodedObject, EntryKind, EntryName, Inputs, Kind, NodeKey,
    Record, RecordId, Revision, RevisionId, Statement, TargetName, Tree, TreeEntry, Verdict,
};
use origin_src::http::Api;
use origin_src::policy::Policy;
use origin_src::server::Repositories;
use origin_src::service::{Node, OriginService};

/// A server hosting `demo`, whose policy requires nothing, and `strict`,
/// whose `main` requires a passing `ci.test` from `user:ci`. The server's
/// node, `hosted`, orders both.
struct Server {
    _dir: tempfile::TempDir,
    client: Client,
    demo: Repo,
    strict: Repo,
}

impl Server {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let node = Node {
            name: "hosted".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[7; 32])),
        };
        let open = Policy::initial(&node.name, &node.key.public());
        let strict = format!(
            "[nodes]\nhosted = \"{}\"\n[targets.policy]\nauthority = \"node:hosted\"\n\
             [targets.main]\nauthority = \"node:hosted\"\n\
             require = [{{ kind = \"ci.test\", verdict = \"pass\", issuer = \"user:ci\" }}]\n",
            node.key.public()
        );
        let mut repositories = Repositories::default();
        for (name, policy) in [("demo", open), ("strict", strict)] {
            let service =
                OriginService::create(dir.path().join(name), node.clone(), &policy).unwrap();
            repositories.insert(name.parse().unwrap(), service);
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, Api::router(repositories)).await });
        let client = Client::new(&format!("http://{address}"));
        Self {
            _dir: dir,
            demo: client.repo("demo".parse().unwrap()),
            strict: client.repo("strict".parse().unwrap()),
            client,
        }
    }
}

/// A revision of `change` with `files` and a description, made on `base`,
/// and every object it needs, records last.
struct Made {
    id: RevisionId,
    objects: Vec<EncodedObject>,
}

impl Made {
    fn revision(
        change: ChangeId,
        base: Base,
        predecessors: Vec<RevisionId>,
        files: &[(&str, &str)],
    ) -> Self {
        let mut objects: Vec<_> = files
            .iter()
            .map(|(_, content)| EncodedObject::blob(content.as_bytes()))
            .collect();
        let tree: Tree = files
            .iter()
            .zip(&objects)
            .map(|((name, _), blob)| {
                let entry = TreeEntry {
                    kind: EntryKind::File,
                    id: blob.id(),
                };
                (EntryName::new(*name).unwrap(), entry)
            })
            .collect();
        let tree = tree.encode();
        let description = EncodedObject::blob(b"A change");
        let revision = Revision {
            change,
            predecessors,
            base,
            deps: Vec::new(),
            snapshot: tree.id().into(),
            description: BlobId::from(description.id()),
            author: "user:ada".parse().unwrap(),
        }
        .encode();
        let id = revision.id().into();
        objects.extend([tree, description, revision]);
        Self { id, objects }
    }

    fn pack(&self) -> Pack {
        Pack {
            objects: self.objects.clone(),
        }
    }
}

/// The tip of `main`, to make revisions on.
async fn main_tip(repo: &Repo) -> Base {
    let targets = repo.targets().await.unwrap();
    let main = targets
        .iter()
        .find(|target| target.name == TargetName::MAIN)
        .unwrap();
    Base::Acceptance(main.head.acceptance)
}

fn change(seed: u8) -> ChangeId {
    ChangeId::from_bytes([seed; 16])
}

/// The error code of a refused request.
fn code(error: Error) -> ErrorCode {
    match error {
        Error::Api(error) => error.code,
        error => panic!("expected an API error, got {error}"),
    }
}

#[tokio::test]
async fn hosts_several_repositories() {
    let server = Server::start().await;
    let names: Vec<_> = server.client.repos().await.unwrap();
    let names: Vec<_> = names.iter().map(|repo| repo.name.as_str()).collect();
    assert_eq!(names, ["demo", "strict"]);

    let missing = server.client.repo(RepoName::new("missing").unwrap());
    assert_eq!(
        code(missing.targets().await.unwrap_err()),
        ErrorCode::NotFound
    );

    // Records put in one repository aren't in the other.
    let made = Made::revision(change(1), main_tip(&server.demo).await, vec![], &[]);
    server.demo.put_records(&made.pack()).await.unwrap();
    let request = FetchRecords {
        ids: vec![made.id.into()],
    };
    assert!(server.demo.fetch_records(&request).await.is_ok());
    let error = server.strict.fetch_records(&request).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::NotFound);
}

#[tokio::test]
async fn puts_and_fetches_records() {
    let server = Server::start().await;
    let repo = &server.demo;
    let made = Made::revision(
        change(1),
        main_tip(repo).await,
        vec![],
        &[("a.txt", "a"), ("b.txt", "b")],
    );
    let stored = repo.put_records(&made.pack()).await.unwrap();
    assert_eq!(
        stored.new, 5,
        "two blobs, the tree, the description and the revision"
    );
    let again = repo.put_records(&made.pack()).await.unwrap();
    assert_eq!(
        again.new, 0,
        "records are immutable, so putting them again does nothing"
    );

    let wanted = [made.objects[4].clone(), made.objects[0].clone()];
    let fetched = repo
        .fetch_records(&FetchRecords {
            ids: wanted.iter().map(EncodedObject::id).collect(),
        })
        .await
        .unwrap();
    assert_eq!(fetched.objects, wanted, "verified, in the order asked");

    // A record is refused unless the repository can use it.
    let orphan = Made::revision(change(2), main_tip(repo).await, vec![], &[("c.txt", "c")]);
    let revision_only = Pack {
        objects: vec![orphan.objects.last().unwrap().clone()],
    };
    let error = repo.put_records(&revision_only).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::BadRequest);
    let log = repo
        .log(&LogQuery {
            target: TargetName::MAIN,
            since: 0,
        })
        .await
        .unwrap();
    let acceptance = log[0].acceptance.encode();
    let error = repo
        .put_records(&Pack {
            objects: vec![acceptance],
        })
        .await
        .unwrap_err();
    assert_eq!(
        code(error),
        ErrorCode::BadRequest,
        "only the authority lands"
    );
}

#[tokio::test]
async fn puts_refuse_objects_named_as_another_kind() {
    let server = Server::start().await;
    let repo = &server.demo;
    let base = main_tip(repo).await;
    let refused = |objects: Vec<EncodedObject>| async move {
        code(repo.put_records(&Pack { objects }).await.unwrap_err())
    };

    // A revision whose snapshot is a blob, in the batch.
    let blob = EncodedObject::blob(b"not a tree");
    let description = EncodedObject::blob(b"A change");
    let revision = Revision {
        change: change(1),
        predecessors: Vec::new(),
        base,
        deps: Vec::new(),
        snapshot: blob.id().into(),
        description: BlobId::from(description.id()),
        author: "user:ada".parse().unwrap(),
    }
    .encode();
    let objects = vec![blob, description, revision];
    assert_eq!(refused(objects).await, ErrorCode::BadRequest);

    // Trees naming a stored blob as a directory, and a stored tree as a
    // file.
    let made = Made::revision(change(2), base, Vec::new(), &[("lib.rs", "fn a() {}")]);
    repo.put_records(&made.pack()).await.unwrap();
    let (file, tree) = (made.objects[0].id(), made.objects[1].id());
    for (kind, id) in [(EntryKind::Tree, file), (EntryKind::File, tree)] {
        let tree: Tree = [(EntryName::new("x").unwrap(), TreeEntry { kind, id })]
            .into_iter()
            .collect();
        assert_eq!(refused(vec![tree.encode()]).await, ErrorCode::BadRequest);
    }

    // One stored object needed as two kinds, the right one last: a
    // revision's snapshot and description, and a tree's directory and
    // file.
    let stored = EncodedObject::blob(b"stored");
    repo.put_records(&Pack {
        objects: vec![stored.clone()],
    })
    .await
    .unwrap();
    let both = Revision {
        change: change(3),
        predecessors: Vec::new(),
        base,
        deps: Vec::new(),
        snapshot: stored.id().into(),
        description: BlobId::from(stored.id()),
        author: "user:ada".parse().unwrap(),
    };
    assert_eq!(refused(vec![both.encode()]).await, ErrorCode::BadRequest);
    let entry = |kind| TreeEntry {
        kind,
        id: stored.id(),
    };
    let tree: Tree = [
        (EntryName::new("a").unwrap(), entry(EntryKind::Tree)),
        (EntryName::new("b").unwrap(), entry(EntryKind::File)),
    ]
    .into_iter()
    .collect();
    assert_eq!(refused(vec![tree.encode()]).await, ErrorCode::BadRequest);
}

#[tokio::test]
async fn promotions_must_match_their_original() {
    let server = Server::start().await;
    let repo = &server.demo;
    let made = Made::revision(change(1), main_tip(repo).await, vec![], &[("a.txt", "a")]);
    repo.put_records(&made.pack()).await.unwrap();
    // Even when they come together.
    let same = Made::revision(change(3), main_tip(repo).await, vec![], &[("a.txt", "a")]);
    repo.put_records(&same.pack()).await.unwrap();
    let original = Statement {
        subject: RecordId::Revision(made.id),
        kind: Kind::new("ci.test").unwrap(),
        verdict: Some(Verdict::Pass),
        issuer: "user:ci".parse().unwrap(),
        inputs: Some(Inputs {
            paths: vec!["a.txt".parse().unwrap()],
            definition: Digest::of(b"test"),
            toolchain: Digest::of(b"rustc"),
            external: false,
        }),
        payload: None,
        supersedes: None,
        promoted_from: None,
    };
    let promotion = Statement {
        subject: RecordId::Revision(same.id),
        issuer: "user:eve".parse().unwrap(),
        promoted_from: Some(original.encode().id().into()),
        ..original.clone()
    };
    let forged = Statement {
        verdict: Some(Verdict::Fail),
        ..promotion.clone()
    };
    let error = repo
        .put_records(&Pack {
            objects: vec![original.encode(), forged.encode()],
        })
        .await
        .unwrap_err();
    assert_eq!(code(error), ErrorCode::BadRequest);
    let stored = repo
        .put_records(&Pack {
            objects: vec![promotion.encode(), original.encode()],
        })
        .await
        .unwrap();
    assert_eq!(stored.new, 1, "the original was stored with the forgery");
}

#[tokio::test]
async fn serves_its_openapi_document() {
    let server = Server::start().await;
    let document = server.client.openapi().await.unwrap();
    assert_eq!(document, serde_json::to_value(Api::openapi()).unwrap());
}

/// `docs/openapi.json` documents the API for other languages. Run `just
/// openapi` to rewrite it after changing the API.
#[test]
fn checked_in_openapi_document_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/openapi.json");
    let document = format!("{}\n", Api::openapi().to_pretty_json().unwrap());
    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(path, document).unwrap();
    } else {
        let checked_in = std::fs::read_to_string(path).unwrap();
        assert!(
            checked_in == document,
            "docs/openapi.json is stale: run `just openapi`"
        );
    }
}
