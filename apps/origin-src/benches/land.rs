//! Benchmarks the landing queue. It measures the latency of one landing,
//! with the change made on the target's tip or rebased onto it, with and
//! without a requirement, and of landing one target on another after long
//! histories. It also measures the throughput of several proposers landing
//! different changes on one target at once. Run it on a real disk: syncs are
//! most of the cost.

use std::thread;

use divan::Bencher;
use divan::counter::ItemsCount;
use origin_platform::Native;
use origin_shared::landing::Landing;
use origin_shared::{
    AcceptanceId, Base, ChangeId, EncodedObject, EntryKind, EntryName, Kind, NodeKey,
    PrincipalName, Record, RecordId, Revision, Statement, TargetName, Tree, TreeEntry, Verdict,
};
use origin_src::annotations::Annotations;
use origin_src::queue::Queue;
use origin_src::store::Store;

fn main() {
    divan::main();
}

/// A repository in which the node `queue` orders the `policy` and `main`
/// targets.
struct Repo {
    _dir: tempfile::TempDir,
    store: Store,
    key: NodeKey,
}

impl Repo {
    /// Creates a repository where landing on `main` requires `require`, a TOML
    /// array.
    fn new(require: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let repo = Self {
            _dir: dir,
            store,
            key: NodeKey::from_bytes(&[7; 32]),
        };
        let policy = format!(
            "[nodes]\nqueue = \"{}\"\n[groups]\nrunners = [\"node:ci\"]\n\
             [targets.policy]\nauthority = \"node:queue\"\n\
             [targets.feature]\nauthority = \"node:queue\"\n\
             [targets.main]\nauthority = \"node:queue\"\nrequire = {require}\n",
            repo.key.public()
        );
        repo.queue().init(&policy).unwrap();
        repo
    }

    fn queue(&self) -> Queue<'_> {
        Queue::new(&self.store, &self.key, PrincipalName::new("queue").unwrap())
    }

    fn tip(&self) -> AcceptanceId {
        self.tip_of(&TargetName::MAIN)
    }

    fn tip_of(&self, target: &TargetName) -> AcceptanceId {
        self.store.head(target).unwrap().unwrap().acceptance
    }

    /// Writes a change made on `base`, a tip of `main`, that adds the file
    /// `file-<n>`.
    fn change(&self, n: usize, base: AcceptanceId) -> ChangeId {
        let mut tree: Tree = self
            .store
            .get(self.store.get(base).unwrap().body().state)
            .unwrap();
        let content = EncodedObject::blob(format!("fn f{n}() {{}}\n").as_bytes());
        tree.insert(
            EntryName::new(format!("file-{n}")).unwrap(),
            TreeEntry {
                kind: EntryKind::File,
                id: content.id(),
            },
        );
        let tree = tree.encode();
        let mut id = [0; 16];
        id[..8].copy_from_slice(&(n as u64).to_le_bytes());
        let change = ChangeId::from_bytes(id);
        let revision = Revision {
            change,
            predecessors: Vec::new(),
            base: Base::Acceptance(base),
            deps: Vec::new(),
            snapshot: tree.id().into(),
            description: EncodedObject::blob(b"").id().into(),
            author: "user:bench".parse().unwrap(),
        }
        .encode();
        let description = EncodedObject::blob(b"");
        self.store.write_all(&[content, tree, description]).unwrap();
        self.store.write_all(&[revision]).unwrap();
        change
    }

    fn land(&self, change: ChangeId) {
        self.land_on(change, &TargetName::MAIN);
    }

    fn land_on(&self, change: ChangeId, onto: &TargetName) {
        let landings = self.queue().land_change(change, onto).unwrap();
        assert!(
            matches!(landings[..], [Landing::Landed { .. }]),
            "{landings:?}"
        );
    }
}

/// Lands one change made on the tip, with no rebase and no requirement.
#[divan::bench(sample_count = 50)]
fn land_on_tip(bencher: Bencher<'_, '_>) {
    let repo = Repo::new("[]");
    let mut n = 0;
    bencher
        .with_inputs(|| {
            n += 1;
            repo.change(n, repo.tip())
        })
        .bench_local_values(|change| repo.land(change));
}

/// Lands one change made on an older tip, which the queue rebases into a new
/// revision.
#[divan::bench(sample_count = 50)]
fn land_rebased(bencher: Bencher<'_, '_>) {
    let repo = Repo::new("[]");
    let first = repo.tip();
    let mut n = 0;
    bencher
        .with_inputs(|| {
            n += 1;
            repo.change(n, first)
        })
        .bench_local_values(|change| repo.land(change));
}

/// Lands one change made on the tip, with the `ci.test` result that `main`
/// requires.
#[divan::bench(sample_count = 50)]
fn land_with_requirement(bencher: Bencher<'_, '_>) {
    let repo = Repo::new(r#"[{ kind = "ci.test", verdict = "pass", issuer = "group:runners" }]"#);
    let mut n = 0;
    bencher
        .with_inputs(|| {
            n += 1;
            let change = repo.change(n, repo.tip());
            let annotations = Annotations::open(&repo.store).unwrap();
            let head = annotations.index().heads(change)[0];
            let result = Statement {
                subject: RecordId::Revision(head),
                kind: Kind::new("ci.test").unwrap(),
                verdict: Some(Verdict::Pass),
                issuer: "node:ci".parse().unwrap(),
                inputs: None,
                payload: None,
                supersedes: None,
                promoted_from: None,
            };
            annotations.issue(&result, None).unwrap();
            change
        })
        .bench_local_values(|change| repo.land(change));
}

/// How many changes each target lands before the targets merge.
const HISTORY: usize = 1_000;

/// Lands the tip of `feature` on `main` after each target landed `HISTORY`
/// changes since `feature` forked, and `feature` landed one more. Finding
/// the merge base reads both histories in full.
#[divan::bench(sample_count = 20, sample_size = 1)]
fn land_target_after_long_histories(bencher: Bencher<'_, '_>) {
    let repo = Repo::new("[]");
    let feature = TargetName::new("feature").unwrap();
    repo.queue()
        .create_target(&feature, &TargetName::MAIN)
        .unwrap();
    for n in 0..HISTORY {
        repo.land(repo.change(n, repo.tip()));
        let change = repo.change(HISTORY + n, repo.tip_of(&feature));
        repo.land_on(change, &feature);
    }
    let mut n = 2 * HISTORY;
    bencher
        .with_inputs(|| {
            n += 1;
            let change = repo.change(n, repo.tip_of(&feature));
            repo.land_on(change, &feature);
        })
        .bench_local_values(|()| {
            let landing = repo
                .queue()
                .land_target(&feature, &TargetName::MAIN)
                .unwrap();
            assert!(matches!(landing, Landing::Landed { .. }), "{landing:?}");
        });
}

/// How many changes each proposer lands.
const EACH: usize = 8;

/// Lands `EACH` changes on `main` from each of `proposers` threads at once.
/// All the changes are made on the first tip of `main`, so all but the first
/// are rebased. Each thread takes the queue's lock the way a separate process
/// would, because the lock belongs to the open file.
#[divan::bench(args = [1, 2, 4, 8], sample_count = 5, sample_size = 1)]
fn concurrent_proposers(bencher: Bencher<'_, '_>, proposers: usize) {
    bencher
        .counter(ItemsCount::new(proposers * EACH))
        .with_inputs(|| {
            let repo = Repo::new("[]");
            let first = repo.tip();
            let changes: Vec<_> = (0..proposers * EACH)
                .map(|n| repo.change(n, first))
                .collect();
            (repo, changes)
        })
        .bench_local_values(|(repo, changes)| {
            thread::scope(|scope| {
                for changes in changes.chunks(EACH) {
                    let repo = &repo;
                    scope.spawn(move || {
                        for &change in changes {
                            repo.land(change);
                        }
                    });
                }
            });
            (repo, changes)
        });
}
