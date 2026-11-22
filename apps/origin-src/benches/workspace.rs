//! Checking a 20k-file tree out into an empty directory, durably, by
//! writing every file from the store.

use std::sync::LazyLock;

use divan::Bencher;
use divan::counter::ItemsCount;
use origin_platform::Native;
use origin_shared::{Record, Tree, TreeId};
use origin_src::fixture::FixtureTree;
use origin_src::store::Store;
use origin_src::tree::FileChange;
use origin_src::worktree::{StatCache, Worktree};

fn main() {
    divan::main();
}

const TREE: FixtureTree = FixtureTree::LARGE;

/// The main working copy: the fixture, snapshotted.
struct Main {
    dir: tempfile::TempDir,
    /// Every file of the snapshot, as checking it out adds them.
    changes: Vec<FileChange>,
}

static MAIN: LazyLock<Main> = LazyLock::new(|| {
    let dir = tempfile::tempdir().unwrap();
    TREE.write(dir.path()).unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    let empty = Tree::default().encode();
    store.write_all(std::slice::from_ref(&empty)).unwrap();
    let (root, _) = Worktree::new(dir.path(), &store)
        .snapshot(&StatCache::default())
        .unwrap();
    let changes = store.diff(TreeId::from(empty.id()), root).unwrap();
    Main { dir, changes }
});

/// Every file read from the store, decompressed, verified and written.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn from_store(bencher: Bencher<'_, '_>) {
    let store = Store::new(MAIN.dir.path().join(".origin"), Native::default());
    bencher
        .counter(ItemsCount::new(MAIN.changes.len()))
        .with_inputs(|| tempfile::tempdir().unwrap())
        .bench_local_refs(|workspace| {
            Worktree::new(workspace.path(), &store)
                .checkout(&MAIN.changes, None)
                .unwrap();
        });
}
