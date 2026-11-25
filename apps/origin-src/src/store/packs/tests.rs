use std::slice;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use origin_platform::{CloneFile, Durability, Error, FileLock, FileSystem, FsKind, Native, Op};
use origin_shared::EncodedObject;
use parking_lot::Mutex;

use super::super::tests::{Recorder, Synced, store};
use super::*;

/// `count` distinct small objects, starting from `seed`.
fn objects(seed: u32, count: u32) -> Vec<EncodedObject> {
    (seed..seed + count)
        .map(|i| EncodedObject::blob(format!("statement {i}").as_bytes()))
        .collect()
}

fn assert_readable<P: Platform>(store: &Store<P>, objects: &[EncodedObject]) {
    for object in objects {
        assert_eq!(&store.read(object.id()).unwrap(), object);
    }
}

/// The files of the packs directory, sorted.
fn pack_files<P: Platform>(store: &Store<P>) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(store.packs_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn packs_small_objects_and_leaves_large_ones_loose() {
    let (_dir, store) = store();
    let small = objects(0, 100);
    // Hashes don't compress, so this stays above the limit.
    let large = EncodedObject::blob(
        &(0..1024_u32)
            .flat_map(|i| *Digest::of(&i.to_le_bytes()).as_bytes())
            .collect::<Vec<_>>(),
    );
    store.write_all(&small).unwrap();
    store.write_all(slice::from_ref(&large)).unwrap();

    let compaction = store.compact().unwrap().unwrap();
    assert_eq!(
        compaction,
        Compaction {
            packed: 100,
            merged: 0,
            duplicates: 0
        }
    );
    assert!(small.iter().all(|object| !store.path(object.id()).exists()));
    assert!(store.path(large.id()).exists(), "large objects stay loose");
    assert_eq!(
        pack_files(&store).len(),
        4,
        "a pack, its index, manifest, lock"
    );

    // A store opened afresh finds them too.
    let reopened = Store::new(&store.root, Native::default());
    assert_readable(&reopened, &small);
    assert!(reopened.contains(small[0].id()).unwrap());
    assert!(
        !reopened
            .contains(EncodedObject::blob(b"absent").id())
            .unwrap()
    );
    assert_eq!(
        reopened.write_all(&small).unwrap(),
        0,
        "packed objects aren't written again"
    );
    assert_eq!(reopened.read(large.id()).unwrap(), large);
}

#[test]
fn merges_packs_geometrically() {
    let (_dir, store) = store();
    let compact = |seed, count| {
        store.write_all(&objects(seed, count)).unwrap();
        let compaction = store.compact().unwrap().unwrap();
        let packs: Vec<_> = store.packs.loaded().iter().map(|pack| pack.len()).collect();
        (compaction.merged, packs)
    };
    assert_eq!(compact(0, 10), (0, vec![10]));
    assert_eq!(compact(10, 6), (1, vec![16]), "10 < 2 × 6: merged");
    assert_eq!(compact(16, 3), (0, vec![16, 3]), "16 ≥ 2 × 3: kept");
    assert_eq!(compact(19, 2), (1, vec![16, 5]), "only the smaller one");
    assert_eq!(compact(21, 20), (2, vec![41]), "both");
    assert_readable(&store, &objects(0, 41));
    assert_eq!(pack_files(&store).len(), 4, "merged packs are removed");
}

#[test]
fn removes_loose_duplicates_of_packed_objects() {
    let (_dir, store) = store();
    let objects = objects(0, 10);
    store.write_all(&objects).unwrap();

    // A batch that read the manifest before a compaction packed them
    // writes them again.
    let unaware = Store::new(&store.root, Native::default());
    let batch = unaware.batch();
    assert!(batch.write(&EncodedObject::blob(b"first")).unwrap());
    store.compact().unwrap();
    for object in &objects[..3] {
        assert!(batch.write(object).unwrap());
    }
    batch.commit().unwrap();
    let compaction = store.compact().unwrap().unwrap();
    assert_eq!(compaction.duplicates, 3);
    assert_eq!(compaction.packed, 1);
    assert!(!store.path(objects[0].id()).exists());
    assert_readable(&store, &objects);

    // A later batch reads the manifest, even in a process that never
    // loaded the packs.
    let fresh = Store::new(&store.root, Native::default());
    assert_eq!(fresh.write_all(&objects).unwrap(), 0);
    assert!(!store.path(objects[0].id()).exists());
}

#[test]
fn lets_go_of_packs_another_process_retired() {
    let (_dir, store) = store();
    let first = objects(0, 10);
    store.write_all(&first).unwrap();
    store.compact().unwrap();
    let other = Store::new(&store.root, Native::default());
    assert_readable(&other, &first);
    store.write_all(&objects(10, 10)).unwrap();
    assert_eq!(store.compact().unwrap().unwrap().merged, 1);

    let ids =
        |store: &Store| -> Vec<_> { store.packs.loaded().iter().map(|pack| pack.id).collect() };
    assert_readable(&other, &first);
    assert_ne!(ids(&other), ids(&store), "still reading the retired pack");
    *other.packs.read_at.lock() = Instant::now().checked_sub(Packs::RELOAD_AFTER);
    assert_readable(&other, &first);
    assert_eq!(ids(&other), ids(&store));
}

#[test]
fn compacts_once_enough_loose_objects_pile_up() {
    let (_dir, store) = store();
    let few = objects(0, 100);
    store.write_all(&few).unwrap();
    assert_eq!(store.compact_if_due().unwrap(), None);
    // Enough for their estimate from one shard to be past the threshold.
    let many = objects(
        100,
        u32::try_from(3 * Store::<Native>::COMPACT_AFTER).unwrap(),
    );
    store.write_all(&many).unwrap();
    let compaction = store.compact_if_due().unwrap().unwrap();
    assert_eq!(compaction.packed, few.len() + many.len());
    assert_eq!(store.compact_if_due().unwrap(), None, "nothing left loose");
}

#[test]
fn rewriting_a_packed_object_syncs_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Recorder::default());
    store.init().unwrap();
    let objects = objects(0, 10);
    store.write_all(&objects).unwrap();
    store.compact().unwrap();
    store.platform.0.lock().clear();

    // The compactor may not have synced the manifest naming it yet.
    assert_eq!(store.write_all(&objects[..1]).unwrap(), 0);
    let packs = store.packs_dir();
    assert_eq!(
        store.platform.0.into_inner(),
        [Synced::Batch(0), Synced::Dir(packs)]
    );
}

#[test]
fn removes_packed_objects_and_keeps_when_each_was_stored() {
    let (_dir, store) = store();
    let first = objects(0, 10);
    store.write_all(&first).unwrap();
    let secs = |id| {
        store
            .stored_at(id)
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let stored = secs(first[0].id());
    store.compact().unwrap();
    store.write_all(&objects(10, 10)).unwrap();
    assert_eq!(store.compact().unwrap().unwrap().merged, 1);
    assert_eq!(secs(first[0].id()), stored, "packing keeps it, merging too");

    let loose = EncodedObject::blob(b"loose");
    store.write_all(slice::from_ref(&loose)).unwrap();
    let doomed = [first[0].id(), first[1].id(), loose.id()];
    assert!(store.remove(&doomed).unwrap() > 0);
    let fresh = Store::new(&store.root, Native::default());
    for id in doomed {
        assert!(!fresh.contains(id).unwrap(), "{id}");
    }
    assert_readable(&fresh, &first[2..]);
    assert_readable(&fresh, &objects(10, 10));
    assert_eq!(pack_files(&store).len(), 4, "the old pack is removed");
    assert_eq!(store.remove(&doomed).unwrap(), 0, "already gone");
}

#[test]
fn unreadable_objects_fail_the_compaction() {
    let (_dir, store) = store();
    let objects = objects(0, 10);
    store.write_all(&objects).unwrap();
    // Reading a directory fails.
    let unreadable = store.path(objects[3].id());
    fs::remove_file(&unreadable).unwrap();
    fs::create_dir(&unreadable).unwrap();

    let error = store.compact().unwrap_err();
    assert!(
        matches!(&error, StoreError::Io { path, .. } if *path == unreadable),
        "{error}"
    );
    assert!(store.packs.loaded().is_empty(), "nothing is published");
    assert_readable(&store, &objects[4..]);
}

#[test]
fn one_compactor_at_a_time() {
    let (_dir, store) = store();
    let lock = store.packs_dir().join(Store::<Native>::LOCK);
    let _held = LockedFile::open(&store.platform, &lock, LockMode::Exclusive).unwrap();
    assert_eq!(store.compact().unwrap(), None);
}

#[test]
fn rejects_corrupt_indexes() {
    let (_dir, store) = store();
    let objects = objects(0, 10);
    store.write_all(&objects).unwrap();
    store.compact().unwrap();
    let index = Pack::file_path(&store.packs_dir(), store.packs.loaded()[0].id, "idx");
    let mut bytes = fs::read(&index).unwrap();
    // Swap the first two ids, which keeps them valid but unsorted.
    let ids = Pack::INDEX_MAGIC.len()..Pack::INDEX_MAGIC.len() + 2 * ObjectId::LEN;
    bytes[ids].rotate_left(ObjectId::LEN);
    fs::write(&index, bytes).unwrap();

    let reopened = Store::new(&store.root, Native::default());
    let error = reopened.read(objects[0].id()).unwrap_err();
    assert!(
        matches!(&error, StoreError::Malformed(path) if *path == index),
        "{error}"
    );
}

/// The native platform, failing one batch sync once armed with
/// [`Crashing::crash_after`], as if the process died there.
#[derive(Default)]
struct Crashing(Mutex<Option<usize>>);

impl Crashing {
    /// Fails the batch sync after the next `batches`.
    fn crash_after(&self, batches: usize) {
        *self.0.lock() = Some(batches);
    }
}

impl Durability for Crashing {
    fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error> {
        let mut left = self.0.lock();
        match *left {
            Some(0) => {
                *left = None;
                let source = io::Error::other("crash");
                return Err(Error {
                    op: Op::Sync,
                    path: PathBuf::from("batch"),
                    source,
                });
            }
            Some(batches) => *left = Some(batches - 1),
            None => {}
        }
        Native::default().sync_batch(files)
    }
}

impl FileSystem for Crashing {
    fn file_system(&self, path: &Path) -> io::Result<FsKind> {
        Native::default().file_system(path)
    }
}

impl FileLock for Crashing {}

impl ReadAt for Crashing {}

impl CloneFile for Crashing {
    fn cannot_clone(&self, error: &io::Error) -> bool {
        Native::default().cannot_clone(error)
    }
}

#[test]
fn a_failed_compaction_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Crashing::default());
    store.init().unwrap();
    let first = objects(0, 10);
    store.write_all(&first).unwrap();
    store.compact().unwrap();
    let second = objects(10, 10);
    store.write_all(&second).unwrap();

    // The new pack and its index are durable, the manifest isn't.
    store.platform.crash_after(1);
    assert!(store.compact().is_err());
    let fresh = Store::new(&store.root, Native::default());
    assert_readable(&fresh, &first);
    assert_readable(&fresh, &second);
    assert_eq!(pack_files(&store).len(), 6, "an unpublished pack is left");

    let compaction = store.compact().unwrap().unwrap();
    assert_eq!((compaction.packed, compaction.merged), (10, 1));
    assert_eq!(pack_files(&store).len(), 4, "and removed by the next one");
    assert_readable(&fresh, &first);
    assert_readable(&fresh, &second);
}

#[test]
fn readers_never_miss_an_object_during_compaction() {
    let (_dir, store) = store();
    let rounds = 20;
    let written: Vec<_> = (0..rounds).map(|round| objects(round * 50, 50)).collect();
    let published = AtomicUsize::new(0);
    thread::scope(|scope| {
        // Readers in this process, sharing the compactor's packs, and
        // as another process would, with packs of their own.
        for reader in [store.clone(), Store::new(&store.root, Native::default())] {
            let (written, published) = (&written, &published);
            scope.spawn(move || {
                while published.load(Ordering::SeqCst) < written.len() {
                    let available = published.load(Ordering::SeqCst);
                    for object in written[..available].iter().flatten() {
                        assert_eq!(&reader.read(object.id()).unwrap(), object);
                    }
                }
            });
        }
        for objects in &written {
            store.write_all(objects).unwrap();
            published.fetch_add(1, Ordering::SeqCst);
            store.compact().unwrap();
        }
    });
    assert!(store.packs.loaded().len() < 6, "merged as it went");
}

#[test]
fn listing_never_misses_an_object_during_compaction() {
    let (_dir, store) = store();
    let written = objects(0, 4000);
    store.write_all(&written).unwrap();
    let compacted = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            while !compacted.load(Ordering::SeqCst) {
                assert_eq!(store.ids().unwrap().len(), written.len());
            }
        });
        store.compact().unwrap();
        compacted.store(true, Ordering::SeqCst);
    });
}
