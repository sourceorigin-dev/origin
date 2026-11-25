use super::*;

/// A `Durability` that records syncs instead of performing them, and fails
/// batch syncs when `fail_batches` is set.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<Call>>,
    fail_batches: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Call {
    Dir(PathBuf),
    /// A batch sync, with how many of its files existed when it ran: staged
    /// files not yet renamed, or files written in place.
    Batch(usize),
}

impl Recorder {
    fn record(&self, call: Call) {
        self.calls.lock().push(call);
    }

    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut self.calls.lock())
    }
}

impl Durability for Recorder {
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.record(Call::Dir(dir.to_owned()));
        Ok(())
    }

    fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error> {
        if self.fail_batches {
            let source = io::Error::other("batch failed");
            return Err(Error::new(Op::Sync, Path::new("batch"), source));
        }
        let unpublished = files.into_iter().filter(|temp| temp.exists()).count();
        self.record(Call::Batch(unpublished));
        Ok(())
    }
}

/// Returns the names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .expect("the directory lists")
        .map(|entry| {
            entry
                .expect("the entry reads")
                .file_name()
                .into_string()
                .expect("the name is UTF-8")
        })
        .collect();
    names.sort();
    names
}

#[test]
fn syncs_the_batch_before_publishing_and_each_directory_once() {
    let root = tempfile::tempdir().expect("a temporary directory is created");
    let root = root.path();
    fs::create_dir(root.join("a")).expect("the directory is created");
    let recorder = Recorder::default();
    let write = DurableWrite::new(&recorder);

    write
        .stage(root.join("a/1"), b"one")
        .expect("the file is staged");
    write
        .stage(root.join("a/2"), b"two")
        .expect("the file is staged");
    assert_eq!(recorder.take(), [], "staging syncs nothing");
    write
        .stage(root.join("b/c/3"), b"three")
        .expect("the file is staged");
    assert_eq!(
        recorder.take(),
        [
            Call::Dir(root.parent().expect("the path has a parent").to_owned()),
            Call::Dir(root.to_owned()),
            Call::Dir(root.join("b"))
        ],
        "but the directories it creates, at once, after the one holding \
             the directory they're created in"
    );
    let staged = names(&root.join("a"));
    assert_eq!(staged.len(), 2);
    assert!(
        staged
            .iter()
            .all(|name| DurableWrite::<Recorder>::is_temporary(name.as_ref())),
        "files are staged beside their destination, under temporary names: {staged:?}"
    );

    write.commit().expect("the write commits");
    assert_eq!(
        recorder.take(),
        [
            Call::Batch(3),
            Call::Dir(root.to_owned()),
            Call::Dir(root.join("a")),
            Call::Dir(root.join("b/c")),
        ],
        "the whole batch is synced before any file is renamed, then each \
             changed directory once, and the one holding `a`, which it found"
    );
    assert_eq!(fs::read(root.join("a/1")).expect("the file reads"), b"one");
    assert_eq!(
        fs::read(root.join("b/c/3")).expect("the file reads"),
        b"three"
    );
    assert_eq!(
        names(&root.join("a")),
        ["1", "2"],
        "no temporary file is left"
    );
    assert_eq!(names(&root.join("b/c")), ["3"]);
}

#[test]
fn directories_stay_durable_when_their_write_is_abandoned() {
    let root = tempfile::tempdir().expect("a temporary directory is created");
    let recorder = Recorder::default();
    let abandoned = DurableWrite::new(&recorder);
    abandoned
        .stage(root.path().join("new/file"), b"old")
        .expect("the file is staged");
    drop(abandoned);
    assert_eq!(
        recorder.take(),
        [
            Call::Dir(
                root.path()
                    .parent()
                    .expect("the path has a parent")
                    .to_owned()
            ),
            Call::Dir(root.path().to_owned())
        ]
    );
}

#[test]
fn writes_into_a_directory_another_made_sync_what_holds_it() {
    let root = tempfile::tempdir().expect("a temporary directory is created");
    let root = root.path();
    fs::create_dir(root.join("new")).expect("the directory is created");
    let recorder = Recorder::default();
    let write = DurableWrite::new(&recorder);
    write
        .stage(root.join("new/file"), b"file")
        .expect("the file is staged");
    assert_eq!(recorder.take(), []);
    write
        .stage(root.join("new/sub/file"), b"deeper")
        .expect("the file is staged");
    assert_eq!(
        recorder.take(),
        [Call::Dir(root.to_owned()), Call::Dir(root.join("new"))],
        "before creating a directory in it"
    );
    write.commit().expect("the write commits");
    assert_eq!(
        recorder.take(),
        [
            Call::Batch(2),
            Call::Dir(root.to_owned()),
            Call::Dir(root.join("new")),
            Call::Dir(root.join("new/sub")),
        ],
        "or with the batch"
    );
}

#[test]
fn syncs_the_directories_of_files_relied_on() {
    let root = tempfile::tempdir().expect("a temporary directory is created");
    let recorder = Recorder::default();
    let write = DurableWrite::new(&recorder);
    write.sync_dir_with(root.path().join("other"));
    write
        .stage(root.path().join("file"), b"contents")
        .expect("the file is staged");
    write.commit().expect("the write commits");
    assert_eq!(
        recorder.take(),
        [
            Call::Batch(1),
            Call::Dir(
                root.path()
                    .parent()
                    .expect("the path has a parent")
                    .to_owned()
            ),
            Call::Dir(root.path().to_owned()),
            Call::Dir(root.path().join("other")),
        ]
    );
}

#[test]
fn a_failed_batch_sync_publishes_nothing() {
    let root = tempfile::tempdir().expect("a temporary directory is created");
    let recorder = Recorder {
        fail_batches: true,
        ..Recorder::default()
    };
    let write = DurableWrite::new(&recorder);
    write
        .stage(root.path().join("file"), b"contents")
        .expect("the file is staged");

    assert_eq!(
        write.commit().expect_err("the operation fails").op,
        Op::Sync
    );
    assert_eq!(names(root.path()), Vec::<String>::new(), "nothing is left");
    assert_eq!(recorder.take(), [], "no directory is synced");
}

#[test]
fn layers_are_published_in_order() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    for dir in ["a", "b", "c"] {
        fs::create_dir(root.join(dir)).unwrap();
    }
    let recorder = Recorder::default();
    let write = DurableWrite::new(&recorder);
    write
        .stage_in(1, root.join("b/2"), |file| file.write_all(b"two"))
        .unwrap();
    write.stage(root.join("a/1"), b"one").unwrap();
    write.commit().unwrap();
    assert_eq!(
        recorder.take(),
        [
            Call::Batch(2),
            Call::Dir(root.to_owned()),
            Call::Dir(root.join("a")),
            Call::Dir(root.to_owned()),
            Call::Dir(root.join("b")),
        ],
        "the batch is synced once, then each layer's directories"
    );

    let write = DurableWrite::new(&recorder);
    write.stage(root.join("a/blocked"), b"one").unwrap();
    write
        .stage_in(1, root.join("c/after"), |file| file.write_all(b"two"))
        .unwrap();
    fs::create_dir_all(root.join("a/blocked/full")).unwrap();
    assert_eq!(write.commit().unwrap_err().op, Op::Publish);
    assert_eq!(names(&root.join("c")), Vec::<String>::new());
}

#[test]
fn files_written_in_place_are_synced_with_the_batch() {
    let root = tempfile::tempdir().expect("a temporary directory is created");
    let log = root.path().join("log");
    fs::write(&log, b"appended").expect("the file is written");
    let recorder = Recorder::default();
    let write = DurableWrite::new(&recorder);
    write
        .stage(root.path().join("file"), b"contents")
        .expect("the file is staged");
    write.sync_with(&log);
    write.commit().expect("the write commits");

    assert_eq!(
        recorder.take(),
        [
            Call::Batch(2),
            Call::Dir(
                root.path()
                    .parent()
                    .expect("the path has a parent")
                    .to_owned()
            ),
            Call::Dir(root.path().to_owned())
        ],
        "the log is synced in the batch, and its directory isn't"
    );
    assert_eq!(
        fs::read(&log).expect("the file reads"),
        b"appended",
        "it stays in place"
    );
}

#[test]
fn replaces_existing_files() {
    let root = tempfile::tempdir().expect("a temporary directory is created");
    let path = root.path().join("head");
    fs::write(&path, b"old").expect("the file is written");
    let recorder = Recorder::default();
    let write = DurableWrite::new(&recorder);
    write.stage(&path, b"new").expect("the file is staged");
    write.commit().expect("the write commits");
    assert_eq!(fs::read(&path).expect("the file reads"), b"new");
}

#[test]
fn streamed_files_are_staged_whole_or_not_at_all() {
    let root = tempfile::tempdir().expect("a temporary directory is created");
    let recorder = Recorder::default();
    let write = DurableWrite::new(&recorder);
    write
        .stage_with(root.path().join("whole"), |file| {
            file.write_all(b"one ")?;
            file.write_all(b"two")
        })
        .expect("the staged file is written");
    let failed = write.stage_with(root.path().join("partial"), |file| {
        file.write_all(b"one ")?;
        Err(Error::new(
            Op::Write,
            Path::new("source"),
            io::Error::other("gone"),
        ))
    });
    assert_eq!(
        failed.expect_err("the operation fails").path,
        Path::new("source")
    );
    write.commit().expect("the write commits");
    assert_eq!(names(root.path()), ["whole"], "the failed file is removed");
    assert_eq!(
        fs::read(root.path().join("whole")).expect("the file reads"),
        b"one two"
    );
}

#[test]
fn dropping_an_uncommitted_write_publishes_nothing() {
    let root = tempfile::tempdir().expect("a temporary directory is created");
    let recorder = Recorder::default();
    let write = DurableWrite::new(&recorder);
    write
        .stage(root.path().join("file"), b"contents")
        .expect("the file is staged");
    drop(write);

    assert_eq!(names(root.path()), Vec::<String>::new(), "nothing is left");
    assert_eq!(recorder.take(), []);
}
