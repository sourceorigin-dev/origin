//! Benchmarks the server. It measures request latency over HTTP on
//! localhost, through the SDK: putting and fetching a batch of records, and
//! an event long-poll woken by a write. It also compares landing through a
//! service that keeps its index open with one that reads the index on each
//! landing. Run it on a real disk: syncs are most of the cost of a put.

#![expect(clippy::expect_used, reason = "A benchmark fails by panicking.")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use divan::Bencher;
use divan::counter::ItemsCount;
use origin_client::Repo;
use origin_shared::api::{EventsQuery, FetchRecords, LandRequest, List};
use origin_shared::landing::{Landing, Proposal};
use origin_shared::pack::Pack;
use origin_shared::{
    AttachmentId, Base, ChangeId, EncodedObject, EntryKind, EntryName, Kind, NodeKey, Period,
    Record, RecordId, Revision, RevisionId, Statement, TargetName, Tree, TreeEntry, Verdict,
};
use origin_src::auth::{Credentials, StaticTokens};
use origin_src::http::Api;
use origin_src::policy::Policy;
use origin_src::server::Repositories;
use origin_src::service::{Caller, Node, OriginService};
use origin_src::store::Store;
use tokio::runtime::Runtime;

fn main() {
    divan::main();
}

/// A repository with one revision on `main`, for statements to be about.
struct Hosted {
    dir: tempfile::TempDir,
    node: Node,
    revision: RevisionId,
}

impl Hosted {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new("hosted".parse().unwrap(), NodeKey::from_bytes(&[7; 32]));
        let policy = Policy::initial(&node.name, &node.key().unwrap().public(), None);
        let service =
            OriginService::create(dir.path().join("demo"), node.clone(), &policy).unwrap();
        let revision = Self::put_change(&service, 0);
        Self {
            dir,
            node,
            revision,
        }
    }

    fn service(&self) -> OriginService {
        OriginService::open(self.dir.path().join("demo"), self.node.clone())
            .expect("`OriginService::open()` succeeds")
    }

    /// Puts a change on the tip of `main` that adds one file, `file-<n>`.
    fn put_change(service: &OriginService, n: u64) -> RevisionId {
        let store = service.store();
        let tip = store
            .head(&TargetName::MAIN)
            .expect("`head(&TargetName::MAIN)` succeeds")
            .expect("the main target has a head")
            .acceptance;
        let mut tree: Tree = store
            .get(store.get(tip).expect("`get(tip)` succeeds").body().state)
            .expect("the tip revision names a stored tree");
        let content = EncodedObject::blob(format!("fn f{n}() {{}}\n").as_bytes());
        tree.insert(
            EntryName::new(format!("file-{n}")).expect("the file name is valid"),
            TreeEntry {
                kind: EntryKind::File,
                id: content.id(),
            },
        );
        let tree = tree.encode();
        let description = EncodedObject::attachment(b"");
        let mut change = [0; 16];
        change[..8].copy_from_slice(&n.to_le_bytes());
        let revision = Revision {
            change: ChangeId::from_bytes(change),
            predecessors: Vec::new(),
            base: Base::Acceptance(tip),
            deps: Vec::new(),
            snapshot: tree.id().into(),
            description: AttachmentId::from(description.id()),
            author: "user:bench".parse().unwrap(),
        }
        .encode();
        let id = revision.id().into();
        let pack = Pack {
            objects: vec![content, tree, description, revision],
        };
        service
            .put_records(&Caller::Owner, pack)
            .expect("`put_records(&Caller::Owner, pack)` succeeds");
        id
    }

    /// Starts serving the repository. Returns the runtime that runs the server
    /// and the SDK's handle on the repository.
    fn serve(&self) -> (Runtime, Repo) {
        let runtime = Runtime::new().unwrap();
        let tokens: StaticTokens = toml::from_str("\"node:ci\" = \"bench\"").unwrap();
        let credentials = Credentials::new(self.node.key().unwrap().public(), Box::new(tokens));
        let mut repositories = Repositories::new(credentials);
        repositories.insert("demo".parse().unwrap(), self.service());
        let url = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("`tokio::net::TcpListener::bind()` succeeds");
            let address = listener.local_addr().expect("`local_addr()` succeeds");
            tokio::spawn(async move { axum::serve(listener, Api::router(repositories)).await });
            format!("http://{address}")
        });
        let repo = origin_client::Client::new(&url)
            .with_token("bench")
            .repo("demo".parse().unwrap());
        (runtime, repo)
    }
}

/// Creates `count` statements about `subject`, numbered from `start`, each
/// of its own kind.
fn statements(subject: RevisionId, start: usize, count: usize) -> Vec<EncodedObject> {
    (start..start + count)
        .map(|n| {
            Statement {
                subject: RecordId::Revision(subject),
                kind: Kind::new(format!("bench.k{n}")).expect("the kind is valid"),
                verdict: Some(Verdict::Pass),
                issuer: "node:ci".parse().expect("`parse()` succeeds"),
                inputs: None,
                payload: None,
                supersedes: None,
                promoted_from: None,
            }
            .encode()
        })
        .collect()
}

/// Puts a batch of new statements, durably, in one request.
#[divan::bench(args = [1, 100, 1000], sample_count = 30)]
fn put_records(bencher: Bencher<'_, '_>, batch: usize) {
    let hosted = Hosted::new();
    let (runtime, repo) = hosted.serve();
    let next = AtomicUsize::new(0);
    bencher
        .counter(ItemsCount::new(batch))
        .with_inputs(|| Pack {
            objects: statements(
                hosted.revision,
                next.fetch_add(batch, Ordering::Relaxed),
                batch,
            ),
        })
        .bench_local_refs(|pack| {
            runtime
                .block_on(repo.put_records(pack))
                .expect("`block_on(repo.put_records(pack))` succeeds")
        });
}

/// Fetches a batch of statements by id in one request.
#[divan::bench(args = [1, 100, 1000], sample_count = 30)]
fn fetch_records(bencher: Bencher<'_, '_>, batch: usize) {
    let hosted = Hosted::new();
    let (runtime, repo) = hosted.serve();
    let objects = statements(hosted.revision, 0, batch);
    let request = FetchRecords {
        ids: objects.iter().map(EncodedObject::id).collect(),
    };
    runtime
        .block_on(repo.put_records(&Pack { objects }))
        .expect("`put_records` succeeds");
    bencher.counter(ItemsCount::new(batch)).bench(|| {
        runtime
            .block_on(repo.fetch_records(&request))
            .expect("`block_on(repo.fetch_records(&request))` succeeds")
    });
}

/// Measures an idle long-poll woken by a statement put, from the start of
/// the put to the poll's answer. The put alone (`put_records` with 1) is
/// most of the time.
#[divan::bench(sample_count = 30)]
fn long_poll_wakeup(bencher: Bencher<'_, '_>) {
    let hosted = Hosted::new();
    let (runtime, repo) = hosted.serve();
    let next = AtomicUsize::new(0);
    bencher
        .with_inputs(|| {
            let cursor = runtime
                .block_on(repo.events(&EventsQuery::default()))
                .expect("`block_on(repo.events(&EventsQuery::default()))` succeeds")
                .next;
            let query = EventsQuery {
                cursor: Some(cursor),
                wait: Some(Period::from_secs(30)),
                kind: List(vec![
                    "statement.issued".parse().expect("`parse()` succeeds"),
                ]),
                ..EventsQuery::default()
            };
            let poller = repo.clone();
            let poll = runtime.spawn(async move {
                poller
                    .events(&query)
                    .await
                    .expect("`events(&query)` succeeds")
            });
            // wall-clock: long enough for the poll to be waiting.
            std::thread::sleep(Duration::from_millis(20));
            let pack = Pack {
                objects: statements(hosted.revision, next.fetch_add(1, Ordering::Relaxed), 1),
            };
            (poll, pack)
        })
        .bench_local_values(|(poll, pack)| {
            runtime.block_on(async {
                repo.put_records(&pack)
                    .await
                    .expect("`put_records(&pack)` succeeds");
                let page = poll.await.expect("`poll` succeeds");
                assert_eq!(page.events.len(), 1);
            });
        });
}

/// Measures an idle long-poll woken by a statement that another process
/// wrote. The server only sees it at its next read of the op log, which
/// happens every 50 ms.
#[divan::bench(sample_count = 10)]
fn long_poll_wakeup_from_another_process(bencher: Bencher<'_, '_>) {
    let hosted = Hosted::new();
    let (runtime, repo) = hosted.serve();
    let other = Store::new(
        hosted.dir.path().join("demo"),
        origin_platform::Native::default(),
    );
    let next = AtomicUsize::new(0);
    bencher
        .with_inputs(|| {
            let cursor = runtime
                .block_on(repo.events(&EventsQuery::default()))
                .expect("`block_on(repo.events(&EventsQuery::default()))` succeeds")
                .next;
            let query = EventsQuery {
                cursor: Some(cursor),
                wait: Some(Period::from_secs(30)),
                kind: List(vec![
                    "statement.issued".parse().expect("`parse()` succeeds"),
                ]),
                ..EventsQuery::default()
            };
            let poller = repo.clone();
            let poll = runtime.spawn(async move {
                poller
                    .events(&query)
                    .await
                    .expect("`events(&query)` succeeds")
            });
            std::thread::sleep(Duration::from_millis(20));
            (
                poll,
                statements(hosted.revision, next.fetch_add(1, Ordering::Relaxed), 1),
            )
        })
        .bench_local_values(|(poll, objects)| {
            other
                .write_all(&objects)
                .expect("`write_all(&objects)` succeeds");
            let page = runtime.block_on(poll).expect("`block_on(poll)` succeeds");
            assert_eq!(page.events.len(), 1);
        });
}

/// How many records the repository holds for `land_with_large_index`.
const RECORDS: usize = 200_000;

/// Lands a change made on the tip in a repository of 200k statements. With
/// `kept`, it uses one service that keeps its index open. Without it, it
/// uses a new service each time, which reads the whole index, as a command
/// does.
#[divan::bench(args = [true, false], sample_count = 10)]
fn land_with_large_index(bencher: Bencher<'_, '_>, kept: bool) {
    let hosted = Hosted::new();
    let service = hosted.service();
    for batch in statements(hosted.revision, 0, RECORDS).chunks(1000) {
        service
            .store()
            .write_all(batch)
            .expect("`write_all(batch)` succeeds");
    }
    let land = |service: &OriginService, change: ChangeId| {
        let request = LandRequest {
            proposal: Proposal::Change(change),
            onto: None,
        };
        let landed = service
            .land(&Caller::Owner, &request)
            .expect("`land(&Caller::Owner, &request)` succeeds");
        assert!(matches!(landed.landings[..], [Landing::Landed { .. }]));
    };
    service
        .statements(&Caller::Owner, RecordId::Revision(hosted.revision))
        .expect("`statements()` succeeds");
    let mut n = 0;
    bencher
        .with_inputs(|| {
            n += 1;
            let revision = Hosted::put_change(&service, n);
            service
                .store()
                .get(revision)
                .expect("`get(revision)` succeeds")
                .change
        })
        .bench_local_values(|change| {
            if kept {
                land(&service, change);
            } else {
                land(&hosted.service(), change);
            }
        });
}
