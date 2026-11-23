//! Benchmarks permission checks on a 20k-file tree: walking `main`'s tip for
//! a clone, and landing a change that edits one file. Each runs as the
//! owner, who isn't checked, as a user under a policy without path rules,
//! and as a user whom path rules withhold one directory from and keep from
//! writing another. Landings sync, so run it on a real disk.

use std::sync::{Arc, LazyLock};

use divan::Bencher;
use origin_shared::api::{FetchSnapshot, LandRequest};
use origin_shared::landing::{Landing, Proposal};
use origin_shared::pack::Pack;
use origin_shared::{
    AcceptanceId, Base, ChangeId, EncodedObject, EntryKind, EntryName, NodeKey, Record, RecordId,
    Revision, TargetName, Tree, TreeEntry, TreeId,
};
use origin_src::fixture::FixtureTree;
use origin_src::policy::Policy;
use origin_src::service::{Caller, Node, OriginService};
use origin_src::worktree::{StatCache, Worktree};

fn main() {
    divan::main();
}

const TREE: FixtureTree = FixtureTree::LARGE;

/// Rules that withhold `dir3` and make `dir5/sub1` read-only for everyone
/// except `user:ada`. They leave `dir0`, where the benchmark edits, alone.
const RULES: &str = r#"
[[paths]]
path = "dir3"
grants = { all = "none", "user:ada" = "write" }

[[paths]]
path = "dir5/sub1"
grants = { all = "read", "user:ada" = "write" }
"#;

/// Who makes the calls.
#[derive(Clone, Copy, Debug)]
enum Who {
    /// The node's own user.
    Owner,
    /// A user, under a policy without path rules.
    NoRules,
    /// A user, under [`RULES`].
    Rules,
}

/// Two repositories whose `main` holds the fixture: one under a policy
/// without path rules, and one under [`RULES`].
struct Setup {
    _dirs: [tempfile::TempDir; 2],
    open: OriginService,
    ruled: OriginService,
}

static SETUP: LazyLock<Setup> = LazyLock::new(|| {
    let files = tempfile::tempdir().unwrap();
    TREE.write(files.path()).unwrap();
    let repos = tempfile::tempdir().unwrap();
    let node = Node {
        name: "hosted".parse().unwrap(),
        key: Arc::new(NodeKey::from_bytes(&[7; 32])),
    };
    let initial = Policy::initial(&node.name, &node.key.public(), None);
    let create = |name: &str, policy: &str| {
        OriginService::create(repos.path().join(name), node.clone(), policy).unwrap()
    };
    let open = create("open", &initial);
    let ruled = create("ruled", &format!("{initial}{RULES}"));
    for service in [&open, &ruled] {
        let (tree, _) = Worktree::new(files.path(), service.store())
            .snapshot(&StatCache::default())
            .unwrap();
        let change = put(service, tree);
        land(service, &Caller::Owner, change);
    }
    Setup {
        _dirs: [files, repos],
        open,
        ruled,
    }
});

impl Setup {
    fn service(&self, who: Who) -> &OriginService {
        match who {
            Who::Owner | Who::NoRules => &self.open,
            Who::Rules => &self.ruled,
        }
    }
}

impl Who {
    fn caller(self) -> Caller {
        match self {
            Self::Owner => Caller::Owner,
            Self::NoRules | Self::Rules => Caller::Principal {
                principal: "user:bench".parse().unwrap(),
                scope: None,
            },
        }
    }
}

/// Returns the tip of `main`.
fn tip(service: &OriginService) -> AcceptanceId {
    service
        .store()
        .head(&TargetName::MAIN)
        .unwrap()
        .unwrap()
        .acceptance
}

/// Puts a revision of a new change with `snapshot` on `main`'s tip.
fn put(service: &OriginService, snapshot: TreeId) -> ChangeId {
    let description = EncodedObject::attachment(b"bench");
    let revision = Revision {
        change: ChangeId::random().unwrap(),
        predecessors: Vec::new(),
        base: Base::Acceptance(tip(service)),
        deps: Vec::new(),
        snapshot,
        description: description.id().into(),
        author: "user:bench".parse().unwrap(),
    };
    let pack = Pack {
        objects: vec![description, revision.encode()],
    };
    service.put_records(&Caller::Owner, pack).unwrap();
    revision.change
}

fn land(service: &OriginService, caller: &Caller, change: ChangeId) {
    let request = LandRequest {
        proposal: Proposal::Change(change),
        onto: None,
    };
    let landed = service.land(caller, &request).unwrap();
    assert!(matches!(landed.landings[..], [Landing::Landed { .. }]));
}

/// Walks the whole snapshot of `main`'s tip, as a clone fetches it.
#[divan::bench(args = [Who::Owner, Who::NoRules, Who::Rules], sample_count = 10)]
fn walk(bencher: Bencher<'_, '_>, who: Who) {
    let setup = &*SETUP;
    let service = setup.service(who);
    let request = FetchSnapshot {
        of: RecordId::Acceptance(tip(service)),
        have: Vec::new(),
        paths: Vec::new(),
        skip: 0,
    };
    let caller = who.caller();
    bencher.bench(|| service.snapshot(&caller, &request).unwrap());
}

/// Lands a change made on the tip that edits one file.
#[divan::bench(args = [Who::Owner, Who::NoRules, Who::Rules], sample_count = 20)]
fn land_one_edit(bencher: Bencher<'_, '_>, who: Who) {
    let setup = &*SETUP;
    let service = setup.service(who);
    let store = service.store();
    let mut version = 0_u64;
    bencher
        .with_inputs(|| {
            version += 1;
            let state = store.get(tip(service)).unwrap().body().state;
            let content = EncodedObject::blob(format!("version {version}\n").as_bytes());
            let file = TreeEntry {
                kind: EntryKind::File,
                id: content.id(),
            };
            store.write_all(&[content]).unwrap();
            let snapshot = replace(service, state, &["dir0", "sub0", "file0.rs"], file);
            put(service, snapshot)
        })
        .bench_local_values(|change| land(service, &who.caller(), change));
}

/// Returns `tree` with `entry` at the path of `components`, writing the new
/// trees on the way to it.
fn replace(service: &OriginService, tree: TreeId, components: &[&str], entry: TreeEntry) -> TreeId {
    let store = service.store();
    let mut tree: Tree = store.get(tree).unwrap();
    let (name, rest) = components.split_first().unwrap();
    let entry = if rest.is_empty() {
        entry
    } else {
        let subtree = tree.get(name).unwrap().subtree().unwrap();
        TreeEntry {
            kind: EntryKind::Tree,
            id: replace(service, subtree, rest, entry).into(),
        }
    };
    tree.insert(EntryName::new(*name).unwrap(), entry);
    let encoded = tree.encode();
    store.write_all(std::slice::from_ref(&encoded)).unwrap();
    encoded.id().into()
}

/// Walks the snapshot on the repository with rules, listing the ids without
/// reading the objects. With `checked`, filters the walk by the user's read
/// rights; without it, does not check.
#[divan::bench(args = [false, true], sample_count = 20)]
fn walk_ids(bencher: Bencher<'_, '_>, checked: bool) {
    let service = &SETUP.ruled;
    let store = service.store();
    let state = store.get(tip(service)).unwrap().body().state;
    let rights = service.rights(&Who::Rules.caller()).unwrap();
    let access = checked.then_some(&rights.paths);
    bencher.bench(|| store.added(state, &[], &[], access).unwrap().len());
}
