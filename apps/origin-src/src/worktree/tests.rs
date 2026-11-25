use origin_platform::{Durability, Error, FileLock, FileSystem, FsKind, Native, ReadAt};

use std::slice;

use super::*;

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        Self { dir, store }
    }

    fn worktree(&self) -> Worktree<'_, Native> {
        Worktree::new(self.dir.path(), &self.store)
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Takes a snapshot, returning the cache to use next.
    fn snapshot(&self, cache: &StatCache) -> (TreeId, StatCache) {
        let (root, next) = self.worktree().snapshot(cache).unwrap();
        (root, next.unwrap_or_else(|| cache.clone()))
    }

    fn entry(&self, root: TreeId, path: &str) -> TreeEntry {
        let mut entry = TreeEntry {
            kind: EntryKind::Tree,
            id: root.into(),
        };
        for component in path.split('/') {
            let tree = self.store.get(TreeId::from(entry.id)).unwrap();
            entry = *tree.get(component).unwrap();
        }
        entry
    }
}

#[test]
fn records_files_kinds_and_skips_metadata() {
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "lib");
    fixture.write("run.sh", "#!/bin/sh");
    fixture.write(".git/HEAD", "ignored");
    let run = fixture.dir.path().join("run.sh");
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    symlink("src/lib.rs", fixture.dir.path().join("link")).unwrap();

    let (root, cache) = fixture.snapshot(&StatCache::default());
    let tree = fixture.store.get(root).unwrap();
    let names: Vec<_> = tree.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["link", "run.sh", "src"]);
    assert_eq!(fixture.entry(root, "run.sh").kind, EntryKind::Executable);
    let link = fixture.entry(root, "link");
    assert_eq!(link.kind, EntryKind::Symlink);
    assert_eq!(
        fixture.store.read(link.id).unwrap().blob_content().unwrap(),
        b"src/lib.rs"
    );
    assert_eq!(
        fixture.entry(root, "src/lib.rs").id,
        EncodedObject::blob(b"lib").id()
    );
    assert_eq!(cache.files.len(), 3);
}

#[test]
fn empty_working_copies_are_empty_trees() {
    let fixture = Fixture::new();
    let (root, _) = fixture.snapshot(&StatCache::default());
    assert_eq!(fixture.store.get(root).unwrap(), Tree::default());
}

#[test]
fn same_size_rewrites_are_detected() {
    let fixture = Fixture::new();
    fixture.write("file", "aaaa");
    let (first, cache) = fixture.snapshot(&StatCache::default());
    // Rewritten right away, possibly within the same mtime tick.
    fixture.write("file", "bbbb");
    let (second, _) = fixture.snapshot(&cache);
    assert_ne!(first, second);
}

#[test]
fn restored_modification_times_do_not_hide_changes() {
    let fixture = Fixture::new();
    fixture.write("file", "aaaa");
    let path = fixture.dir.path().join("file");
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let (first, mut cache) = fixture.snapshot(&StatCache::default());
    // Trust every entry, as if the file had settled long ago.
    cache.scanned_at = i64::MAX;
    fixture.write("file", "bbbb");
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let (second, _) = fixture.snapshot(&cache);
    assert_ne!(first, second);
}

#[test]
fn cache_hits_skip_reading() {
    let fixture = Fixture::new();
    fixture.write("file", "content");
    fixture.write("other", "content");
    let (root, mut cache) = fixture.snapshot(&StatCache::default());
    // Pretend the scan happened long after the write, so the entries are
    // trusted, and point one at a different blob: a hit returns the
    // cached id without reading the file.
    cache.scanned_at = i64::MAX;
    cache.root = None;
    let fake = EncodedObject::blob(b"cached");
    fixture
        .store
        .write_all(std::slice::from_ref(&fake))
        .unwrap();
    cache.files.get_mut("file").unwrap().1 = fake.id();

    let (cached_root, _) = fixture.snapshot(&cache);
    assert_ne!(cached_root, root);
    assert_eq!(fixture.entry(cached_root, "file").id, fake.id());
}

#[test]
fn unchanged_files_reuse_the_cached_root() {
    let fixture = Fixture::new();
    fixture.write("file", "content");
    let (root, mut cache) = fixture.snapshot(&StatCache::default());
    cache.scanned_at = i64::MAX;
    let worktree = fixture.worktree();
    assert_eq!(worktree.snapshot(&cache).unwrap(), (root, None));

    fixture.write("other", "new file");
    let (changed, next) = worktree.snapshot(&cache).unwrap();
    assert_ne!(changed, root);
    assert_eq!(next.unwrap().root, Some(changed));
}

#[test]
fn hashes_many_files_in_parallel() {
    let fixture = Fixture::new();
    let count = Worktree::<Native>::PARALLEL_THRESHOLD * 2;
    for i in 0..count {
        fixture.write(&format!("dir{}/file{i}", i % 7), &i.to_string());
    }
    let (root, cache) = fixture.snapshot(&StatCache::default());
    assert_eq!(cache.files.len(), count);
    assert_eq!(
        fixture.entry(root, "dir3/file10").id,
        EncodedObject::blob(b"10").id()
    );
}

#[test]
fn cache_round_trips() {
    let fixture = Fixture::new();
    fixture.write("a", "1");
    fixture.write("b/c", "2");
    let (_, cache) = fixture.snapshot(&StatCache::default());
    let path = fixture.dir.path().join(".origin/state");
    cache.save(&path).unwrap();
    assert_eq!(StatCache::load(&path), cache);
    assert_eq!(
        StatCache::decode(b"garbage"),
        Err(DecodeError::NonCanonical("stat cache header"))
    );
}

#[test]
fn checkouts_reproduce_snapshots() {
    let fixture = Fixture::new();
    fixture.write("keep", "same");
    fixture.write("edit", "old");
    fixture.write("gone/deep/file", "removed");
    fixture.write("swap", "a file that becomes a directory");
    fixture.write("dir/inner", "a directory that becomes a file");
    let (old, cache) = fixture.snapshot(&StatCache::default());

    for path in ["edit", "swap", "gone/deep/file", "dir/inner"] {
        fs::remove_file(fixture.dir.path().join(path)).unwrap();
    }
    fs::remove_dir_all(fixture.dir.path().join("gone")).unwrap();
    fs::remove_dir(fixture.dir.path().join("dir")).unwrap();
    fixture.write("edit", "new");
    fixture.write("swap/inner", "now inside");
    fixture.write("dir", "now a file");
    fixture.write("run.sh", "#!/bin/sh");
    let run = fixture.dir.path().join("run.sh");
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    symlink("keep", fixture.dir.path().join("link")).unwrap();
    let (new, cache) = fixture.snapshot(&cache);

    // Back to the old snapshot, then forward to the new one.
    let worktree = fixture.worktree();
    worktree
        .checkout(&fixture.store.diff(new, old).unwrap(), &[], None)
        .unwrap();
    assert!(!fixture.dir.path().join("swap/inner").exists());
    let (back, cache) = fixture.snapshot(&cache);
    assert_eq!(back, old);
    worktree
        .checkout(&fixture.store.diff(old, new).unwrap(), &[], None)
        .unwrap();
    assert!(!fixture.dir.path().join("gone").exists(), "emptied dirs go");
    assert_eq!(fixture.snapshot(&cache).0, new);
}

#[test]
fn checkouts_never_overwrite_edits_made_since_the_snapshot() {
    let fixture = Fixture::new();
    fixture.write("kept", "kept");
    fixture.write("changed", "before");
    let (old, _) = fixture.snapshot(&StatCache::default());
    fixture.write("changed", "after");
    fixture.write("added", "added");
    let (new, _) = fixture.snapshot(&StatCache::default());
    let back = fixture.store.diff(new, old).unwrap();
    let worktree = fixture.worktree();

    // Snapshotted, then edited again before the checkout.
    let (_, cache) = fixture.snapshot(&StatCache::default());
    fixture.write("changed", "edited since");
    let error = worktree.checkout(&back, &[], Some(&cache)).unwrap_err();
    assert!(
        matches!(&error, SnapshotError::Edited(path) if path.ends_with("changed")),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("added")).unwrap(),
        "added",
        "nothing changed"
    );

    // Edited back, it is as snapshotted: its content tells.
    fixture.write("changed", "after");
    worktree.checkout(&back, &[], Some(&cache)).unwrap();
    assert_eq!(fixture.snapshot(&StatCache::default()).0, old);

    // A file created where the checkout writes one.
    let (_, cache) = fixture.snapshot(&StatCache::default());
    fixture.write("added", "created since");
    let forward = fixture.store.diff(old, new).unwrap();
    assert!(matches!(
        worktree.checkout(&forward, &[], Some(&cache)),
        Err(SnapshotError::Edited(_))
    ));
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("added")).unwrap(),
        "created since"
    );
}

#[test]
fn checkouts_stop_at_a_file_edited_while_they_run() {
    let fixture = Fixture::new();
    fixture.write("file", "before");
    let (old, _) = fixture.snapshot(&StatCache::default());
    fixture.write("file", "after");
    let (new, cache) = fixture.snapshot(&StatCache::default());
    let back = fixture.store.diff(new, old).unwrap();
    let worktree = fixture.worktree();

    // Found unedited, then edited before the checkout replaces it.
    let seen: Vec<_> = back
        .iter()
        .map(|change| worktree.check_unchanged(change, &cache).unwrap())
        .collect();
    fixture.write("file", "edited meanwhile");
    let error = worktree.apply(&back, Some(&seen), &[]).unwrap_err();
    assert!(matches!(error, SnapshotError::Edited(_)), "{error}");
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("file")).unwrap(),
        "edited meanwhile"
    );
}

#[test]
fn checkouts_turn_files_into_directories_and_back() {
    let fixture = Fixture::new();
    fixture.write("a", "file");
    let (file, _) = fixture.snapshot(&StatCache::default());
    fs::remove_file(fixture.dir.path().join("a")).unwrap();
    fixture.write("a/b", "in a directory");
    let (dir, cache) = fixture.snapshot(&StatCache::default());
    let worktree = fixture.worktree();

    let back = fixture.store.diff(dir, file).unwrap();
    worktree.checkout(&back, &[], Some(&cache)).unwrap();
    let (found, cache) = fixture.snapshot(&StatCache::default());
    assert_eq!(found, file);
    let forth = fixture.store.diff(file, dir).unwrap();
    worktree.checkout(&forth, &[], Some(&cache)).unwrap();
    assert_eq!(fixture.snapshot(&StatCache::default()).0, dir);
}

#[test]
fn restoring_reads_every_content_before_changing_a_file() {
    let fixture = Fixture::new();
    fixture.write("removed", "old");
    let (old, _) = fixture.snapshot(&StatCache::default());
    fs::remove_file(fixture.dir.path().join("removed")).unwrap();
    fixture.write("lost", "content the store no longer has");
    let (new, _) = fixture.snapshot(&StatCache::default());
    let changes = fixture.store.diff(old, new).unwrap();
    let lost = EncodedObject::blob(b"content the store no longer has");
    fs::remove_file(fixture.store.path(lost.id())).unwrap();

    // The files as the move found them: it fails before removing one.
    fs::remove_file(fixture.dir.path().join("lost")).unwrap();
    fixture.write("removed", "old");
    assert!(fixture.worktree().restore(&changes).is_err());
    let removed = fs::read_to_string(fixture.dir.path().join("removed"));
    assert_eq!(removed.unwrap(), "old", "untouched");
}

#[test]
fn restoring_keeps_files_edited_since() {
    let fixture = Fixture::new();
    for path in ["unmoved", "moved", "edited"] {
        fixture.write(path, "old");
    }
    let (old, _) = fixture.snapshot(&StatCache::default());
    for path in ["unmoved", "moved", "edited", "added"] {
        fixture.write(path, "new");
    }
    let (new, _) = fixture.snapshot(&StatCache::default());
    let changes = fixture.store.diff(old, new).unwrap();

    // A move cut short, then edits.
    fixture.write("unmoved", "old");
    fixture.write("edited", "edited");
    fixture.write("added", "created");
    fixture.worktree().restore(&changes).unwrap();
    let read = |path| fs::read_to_string(fixture.dir.path().join(path)).unwrap();
    assert_eq!(read("unmoved"), "new");
    assert_eq!(read("moved"), "new");
    assert_eq!(read("edited"), "edited");
    assert_eq!(read("added"), "created");
}

#[test]
fn checkouts_read_only_the_stat_caches_they_need() {
    let fixture = Fixture::new();
    fixture.write("file", "content");
    let (root, mut cache) = fixture.snapshot(&StatCache::default());
    cache.scanned_at = i64::MAX;
    let empty = Tree::default().encode();
    fixture.store.write_all(slice::from_ref(&empty)).unwrap();
    let other = tempfile::tempdir().unwrap();
    let sources = [
        CloneSource::new(fixture.dir.path().to_owned(), cache),
        CloneSource::saved(other.path().to_owned(), other.path().join("state")),
    ];
    let worktree = Worktree::new(other.path(), &fixture.store);
    worktree.checkout(&[], &sources, None).unwrap();
    worktree
        .checkout(
            &fixture.store.diff(empty.id().into(), root).unwrap(),
            &sources,
            None,
        )
        .unwrap();
    assert!(
        sources[1].cache.get().is_none(),
        "the first source had every file"
    );
    assert_eq!(
        fs::read_to_string(other.path().join("file")).unwrap(),
        "content"
    );
}

/// The native platform, except that it counts clones and refuses them, as
/// ext4 does. With `content_only`, it makes clones with the original's
/// content but not its mode.
#[derive(Debug, Default)]
struct FakeClones {
    attempts: AtomicUsize,
    content_only: bool,
}

impl Durability for FakeClones {
    fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error> {
        Native::default().sync_batch(files)
    }
}

impl FileLock for FakeClones {}

impl ReadAt for FakeClones {}

impl CloneFile for FakeClones {
    fn cannot_clone(&self, _: &io::Error) -> bool {
        !self.content_only
    }

    fn clone_file(&self, from: &Path, to: &Path) -> Result<(), CloneError> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        let (from, to) = (from.to_owned(), to.to_owned());
        if self.content_only {
            return fs::read(&from)
                .and_then(|content| fs::write(&to, content))
                .map_err(|source| CloneError::Io { from, to, source });
        }
        Err(CloneError::Unsupported {
            from,
            to,
            source: io::ErrorKind::Unsupported.into(),
        })
    }
}

impl FileSystem for FakeClones {
    fn file_system(&self, path: &Path) -> io::Result<FsKind> {
        Native::default().file_system(path)
    }
}

#[test]
fn checkouts_clone_only_unmodified_files() {
    let fixture = Fixture::new();
    fixture.write("same", "same");
    fixture.write("edited", "before");
    fixture.write("run.sh", "#!/bin/sh");
    let run = fixture.dir.path().join("run.sh");
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    symlink("same", fixture.dir.path().join("link")).unwrap();
    // Enough files that every writer thread has several.
    let files = 64;
    for i in 0..files {
        fixture.write(&format!("copies/{i}"), &i.to_string());
    }
    let (root, mut cache) = fixture.snapshot(&StatCache::default());
    cache.scanned_at = i64::MAX;
    fixture.write("edited", "after!");
    let empty = Tree::default().encode();
    fixture.store.write_all(slice::from_ref(&empty)).unwrap();

    let other = tempfile::tempdir().unwrap();
    let store = Store::new(fixture.dir.path().join(".origin"), FakeClones::default());
    let source = CloneSource::new(fixture.dir.path().to_owned(), cache);
    Worktree::new(other.path(), &store)
        .checkout(
            &store.diff(empty.id().into(), root).unwrap(),
            slice::from_ref(&source),
            None,
        )
        .unwrap();
    // Each writer thread tries once at most: the first refusal it sees
    // makes it, and every thread after, copy.
    let threads = thread::available_parallelism().map_or(1, usize::from);
    let attempts = store.platform.attempts.load(Ordering::Relaxed);
    assert!(
        (1..=threads.min(files)).contains(&attempts),
        "a refused clone falls back to copies for the rest: {attempts} attempts"
    );
    let read = |path| fs::read_to_string(other.path().join(path)).unwrap();
    assert_eq!(read("edited"), "before", "a modified original isn't copied");
    assert_eq!(read("same"), "same");
    let (copied, _) = Worktree::new(other.path(), &fixture.store)
        .snapshot(&StatCache::default())
        .unwrap();
    assert_eq!(copied, root, "kinds and links are reproduced");
}

#[test]
fn clones_take_the_mode_their_entry_names() {
    let fixture = Fixture::new();
    fixture.write("run.sh", "#!/bin/sh");
    let run = fixture.dir.path().join("run.sh");
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    let (root, mut cache) = fixture.snapshot(&StatCache::default());
    cache.scanned_at = i64::MAX;
    let script = fixture.entry(root, "run.sh");
    let empty = Tree::default().encode();
    // The script's content as an entry of `kind`, cloned from it: the
    // mode bits for executing it.
    let checked_out = |kind, platform: FakeClones| {
        let mut tree = Tree::default();
        let name = origin_shared::EntryName::new("file").unwrap();
        tree.insert(
            name,
            TreeEntry {
                kind,
                id: script.id,
            },
        );
        let tree = tree.encode();
        fixture
            .store
            .write_all(&[empty.clone(), tree.clone()])
            .unwrap();
        let other = tempfile::tempdir().unwrap();
        let store = Store::new(fixture.dir.path().join(".origin"), platform);
        let source = CloneSource::new(fixture.dir.path().to_owned(), cache.clone());
        Worktree::new(other.path(), &store)
            .checkout(
                &store.diff(empty.id().into(), tree.id().into()).unwrap(),
                slice::from_ref(&source),
                None,
            )
            .unwrap();
        assert_eq!(store.platform.attempts.load(Ordering::Relaxed), 1);
        let file = fs::metadata(other.path().join("file")).unwrap();
        file.permissions().mode() & 0o111
    };
    // A clone of the content alone, as Linux makes, of the executable.
    let content_only = FakeClones {
        content_only: true,
        ..FakeClones::default()
    };
    let executable = checked_out(EntryKind::Executable, content_only);
    assert_eq!(executable, 0o111, "still executable");
    // A copy, as where cloning is refused, keeps the original's mode.
    let regular = checked_out(EntryKind::File, FakeClones::default());
    assert_eq!(regular, 0, "not executable");
}
