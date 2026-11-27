use std::io::Write;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use origin_platform::Native;
use origin_shared::{AttachmentId, Base, Inputs, Kind, Record, Tree, Verdict};

use super::*;
use crate::oplog::FramePart;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    (dir, store)
}

fn change(seed: u8) -> ChangeId {
    ChangeId::from_bytes([seed; 16])
}

/// A revision of `change` evolving from `predecessors`.
fn revision(change: ChangeId, predecessors: &[&EncodedObject], seed: &str) -> EncodedObject {
    Revision {
        change,
        predecessors: predecessors.iter().map(|p| p.id().into()).collect(),
        base: Base::Acceptance(ObjectId::from_bytes([0; 32]).into()),
        deps: Vec::new(),
        snapshot: Tree::default().encode().id().into(),
        description: AttachmentId::from(EncodedObject::attachment(seed.as_bytes()).id()),
        author: "user:alice".parse().unwrap(),
    }
    .encode()
}

/// A statement about `subject`.
fn statement(subject: RecordId, kind: &str) -> EncodedObject {
    Statement {
        subject,
        kind: Kind::new(kind).unwrap(),
        verdict: Some(Verdict::Pass),
        issuer: "node:ci-1".parse().unwrap(),
        inputs: None,
        payload: None,
        supersedes: None,
        promoted_from: None,
    }
    .encode()
}

fn revision_id(object: &EncodedObject) -> RevisionId {
    object.id().into()
}

#[test]
fn detects_divergence_and_convergence() {
    let (_dir, store) = store();
    let c = change(1);
    let first = revision(c, &[], "first");
    let left = revision(c, &[&first], "left");
    let right = revision(c, &[&first], "right");
    let merged = revision(c, &[&left, &right], "merged");

    store.write_all(slice::from_ref(&first)).unwrap();
    let index = Index::open(&store).unwrap();
    assert_eq!(index.heads(c), [revision_id(&first)]);
    assert!(!index.is_divergent(c));

    store.write_all(slice::from_ref(&left)).unwrap();
    store.write_all(slice::from_ref(&right)).unwrap();
    let index = Index::open(&store).unwrap();
    assert_eq!(index.heads(c), [revision_id(&left), revision_id(&right)]);
    assert!(
        index.is_divergent(c),
        "two agents revised the same revision"
    );

    store.write_all(slice::from_ref(&merged)).unwrap();
    let index = Index::open(&store).unwrap();
    assert_eq!(index.heads(c), [revision_id(&merged)]);
    assert!(!index.is_divergent(c));
    assert_eq!(index.revisions(c).len(), 4);
    assert_eq!(index.heads(change(2)), [], "unknown changes have none");
}

#[test]
fn finds_changes_by_unique_prefix() {
    let (_dir, store) = store();
    let ab = ChangeId::from_bytes([0xab; 16]);
    let ac = ChangeId::from_bytes([0xac; 16]);
    store
        .write_all(&[revision(ab, &[], "ab"), revision(ac, &[], "ac")])
        .unwrap();
    let index = Index::open(&store).unwrap();

    assert_eq!(index.find_change("ab"), Ok(ab));
    assert_eq!(index.find_change("AC"), Ok(ac), "either case");
    assert_eq!(index.find_change(&ab.to_hex()), Ok(ab));
    assert_eq!(
        index.find_change("a"),
        Err(FindChangeError::Ambiguous {
            prefix: "a".to_owned(),
            matches: vec![ab, ac]
        })
    );
    assert!(matches!(
        index.find_change("ff"),
        Err(FindChangeError::NotFound(_))
    ));
    for invalid in ["", "xyz"] {
        assert!(matches!(
            index.find_change(invalid),
            Err(FindChangeError::Invalid(_))
        ));
    }
}

/// Records arriving in several batches, out of order and some twice.
fn write_history(store: &Store) {
    let (a, b) = (change(1), change(2));
    let a1 = revision(a, &[], "a1");
    let a2 = revision(a, &[&a1], "a2");
    let b1 = revision(b, &[], "b1");
    let b2 = revision(b, &[&b1], "b2");
    let on_a2 = statement(RecordId::Revision(a2.id().into()), "ci.test");
    let on_statement = statement(RecordId::Statement(on_a2.id().into()), "flaky");
    let on_snapshot = statement(
        RecordId::Snapshot(Tree::default().encode().id().into()),
        "lint",
    );
    // A promotable result, and its rerun superseding it.
    let mut result: Statement = statement(RecordId::Revision(b1.id().into()), "ci.test")
        .decode_as()
        .unwrap();
    result.inputs = Some(Inputs {
        paths: Vec::new(),
        definition: Digest::of(b"cargo test"),
        toolchain: Digest::of(b"rustc"),
        external: false,
    });
    let rerun = Statement {
        supersedes: Some(result.encode().id().into()),
        ..result.clone()
    };
    let batches = [
        // A successor before its predecessor, as a fetch can deliver.
        vec![a2, on_a2, EncodedObject::blob(b"file")],
        vec![a1.clone(), b1, on_statement, rerun.encode()],
        vec![b2, on_snapshot, a1, result.encode()],
    ];
    for batch in &batches {
        store.write_all(batch).unwrap();
    }
}

#[test]
fn rebuilding_equals_updating() {
    let (_dir, store) = store();
    write_history(&store);
    let updated = Index::open(&store).unwrap();
    assert_eq!(updated.arrivals().len(), 9, "every record once, no blob");
    assert_eq!(updated.heads(change(1)).len(), 1);
    let promotable: Vec<_> = updated.promotable().collect();
    assert_eq!(promotable.len(), 1);
    assert_eq!(promotable[0].1.len(), 2, "a result and its rerun");
    assert_eq!(
        updated.superseders(promotable[0].1[1]),
        [promotable[0].1[0]]
    );

    assert_eq!(Index::rebuild(&store).unwrap().index, updated);
    assert_eq!(Index::open(&store).unwrap(), updated, "rebuilt on disk");

    // Without the old index, only the arrival order is lost.
    fs::remove_file(store.index_log()).unwrap();
    assert_eq!(sorted(Index::open(&store).unwrap()), sorted(updated));
}

/// The index with every list sorted: what's left without arrival order.
fn sorted(mut index: Index) -> Index {
    index.arrivals.sort();
    for change in index.changes.values_mut() {
        change.revisions.sort();
    }
    let lists = index
        .statements
        .values_mut()
        .chain(index.superseders.values_mut());
    for statements in lists.chain(index.promotable.values_mut()) {
        statements.sort();
    }
    index
}

#[test]
fn damage_rebuilds_the_index() {
    let (_dir, store) = store();
    write_history(&store);
    Index::open(&store).unwrap();
    let last = revision(change(3), &[], "last");
    store.write_all(slice::from_ref(&last)).unwrap();
    let index = Index::open(&store).unwrap();
    let log = store.index_log();
    let bytes = fs::read(&log).unwrap();

    // Damage past the first frame keeps the arrival order before it:
    // here, the frame catching up with the last record.
    let mut late = bytes.clone();
    *late.last_mut().unwrap() ^= 1;
    fs::write(&log, late).unwrap();
    assert_eq!(Index::open(&store).unwrap(), index);
    assert_eq!(
        Log::parse(&fs::read(&log).unwrap()).1,
        Tail::Clean,
        "repaired on disk"
    );

    let mut early = bytes.clone();
    early[Log::FRAMES.header().len() + 10] ^= 1;
    let mut versioned = bytes;
    versioned[Log::FRAMES.header().len() - 2] = b'9';
    for damaged in [early, versioned, Vec::new()] {
        fs::write(&log, damaged).unwrap();
        assert_eq!(sorted(Index::open(&store).unwrap()), sorted(index.clone()));
        assert_eq!(
            Log::parse(&fs::read(&log).unwrap()).1,
            Tail::Clean,
            "repaired on disk"
        );
    }
}

#[test]
fn catches_up_with_what_crashed_writes_published() {
    let (_dir, store) = store();
    let c = change(1);
    let published = revision(c, &[], "published");
    let unpublished = revision(c, &[], "unpublished");
    store.write_all(slice::from_ref(&published)).unwrap();
    let caught_up = Index::open(&store).unwrap();

    // A batch whose writer crashed after logging it: one record made it
    // into the store, one didn't.
    let entries = [&published, &unpublished].map(|object| Entry::of(object).unwrap().unwrap());
    store
        .commit_logged(store.durable_write(), &[Effect::Wrote(entries.to_vec())])
        .unwrap();
    // Then a crash in the middle of catching up.
    let frame = Frame::Records {
        applied: 99,
        entries: Cow::Borrowed(&entries),
    }
    .encode();
    let mut log = OpenOptions::new()
        .append(true)
        .open(store.index_log())
        .unwrap();
    log.write_all(&frame[..frame.len() / 2]).unwrap();

    let (index, intact) = Index::read(&store).unwrap();
    assert!(intact, "a partial last frame is no damage");
    assert_eq!(index, caught_up);
    let index = Index::open(&store).unwrap();
    assert_eq!(
        index.heads(c),
        [revision_id(&published)],
        "what was published"
    );
    assert_eq!(index.applied, store.oplog().last_seq().unwrap());
    assert_eq!(
        Log::parse(&fs::read(store.index_log()).unwrap()).1,
        Tail::Clean
    );

    // Frames lost to a crash are caught up again from the op log, and
    // the next record lands on top.
    let next = revision(c, &[&published], "next");
    store.write_all(slice::from_ref(&next)).unwrap();
    let reopened = Index::open(&store).unwrap();
    fs::write(store.index_log(), Log::encode(&[], &[], 0)).unwrap();
    assert_eq!(Index::open(&store).unwrap(), reopened);
    assert_eq!(reopened.heads(c), [revision_id(&next)]);
}

#[test]
fn writes_in_progress_hold_back_none_published_after_them() {
    let (_dir, store) = store();
    let c = change(1);
    let slow = revision(c, &[], "slow");
    let quick = revision(change(2), &[], "quick");
    // A write whose records aren't published yet, then one that is.
    let entry = Entry::of(&slow).unwrap().unwrap();
    let (held, writing) = store
        .oplog()
        .append_in_progress(&Effect::Wrote(vec![entry]))
        .unwrap();
    store.write_all(slice::from_ref(&quick)).unwrap();

    let index = Index::open(&store).unwrap();
    assert_eq!(index.heads(change(2)), [revision_id(&quick)]);
    assert_eq!(index.applied, held - 1, "it looks at the slow one again");
    assert_eq!(
        Index::open(&store).unwrap(),
        index,
        "and appends nothing twice"
    );

    store.write_all(slice::from_ref(&slow)).unwrap();
    drop(writing);
    let index = Index::open(&store).unwrap();
    assert_eq!(index.heads(c), [revision_id(&slow)]);
    assert_eq!(index.applied, store.oplog().last_seq().unwrap());
}

#[test]
fn records_added_before_catching_up_are_still_logged() {
    let (_dir, store) = store();
    let mut index = Index::open(&store).unwrap();
    let written = revision(change(1), &[], "added");
    store.write_all(slice::from_ref(&written)).unwrap();
    index.add([&written]).unwrap();
    assert!(index.catch_up(&store).unwrap());
    let reopened = Index::open(&store).unwrap();
    assert_eq!(reopened.heads(change(1)), [revision_id(&written)]);
}

#[test]
fn op_log_damage_rebuilds_the_index() {
    let (_dir, store) = store();
    Index::open(&store).unwrap();
    let written: Vec<_> = (1..=3)
        .map(|seed| revision(change(seed), &[], "lost"))
        .collect();
    for revision in &written {
        store.write_all(slice::from_ref(revision)).unwrap();
    }
    // The entry of the second revision: the store has it, the op log
    // no longer says so.
    store
        .oplog()
        .damage(store.oplog().last_seq().unwrap() - 1, FramePart::Check);
    let index = Index::open(&store).unwrap();
    for (seed, revision) in (1..).zip(&written) {
        assert_eq!(index.heads(change(seed)), [revision_id(revision)]);
    }
}

#[test]
fn rebuilds_once_the_op_log_dropped_what_it_missed() {
    let (_dir, store) = store();
    // The test op log keeps a few kilobytes: far fewer entries than this.
    let written: Vec<_> = (0..100)
        .map(|i| {
            let object = revision(change(1), &[], &format!("{i}"));
            store.write_all(slice::from_ref(&object)).unwrap();
            revision_id(&object)
        })
        .collect();
    assert!(matches!(
        store.oplog().after(0),
        Err(StoreError::Expired { .. })
    ));
    let index = Index::open(&store).unwrap();
    assert_eq!(index.revisions(change(1)).len(), written.len());
}

#[test]
fn rebuilds_skip_and_report_corrupt_records() {
    let (_dir, store) = store();
    let c = change(1);
    let intact = revision(c, &[], "intact");
    let corrupt = revision(c, &[], "corrupt");
    let garbled = revision(c, &[], "garbled");
    store
        .write_all(&[intact.clone(), corrupt.clone(), garbled.clone()])
        .unwrap();
    let other = zstd::bulk::compress(intact.bytes(), 1).unwrap();
    fs::write(store.path(corrupt.id()), other).unwrap();
    fs::write(store.path(garbled.id()), b"not zstd").unwrap();

    let Rebuilt { index, skipped } = Index::rebuild(&store).unwrap();
    assert_eq!(index.heads(c), [revision_id(&intact)]);
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(
        skipped.iter().any(
            |skip| matches!(skip, Skipped::Corrupt(mismatch) if mismatch.expected == corrupt.id())
        ),
        "{skipped:?}"
    );
    assert!(
        skipped.iter().any(
            |skip| matches!(skip, Skipped::Undecompressable { id, .. } if *id == garbled.id())
        ),
        "{skipped:?}"
    );
}

#[test]
fn refreshing_catches_up_an_index_kept_open() {
    let (_dir, store) = store();
    let mut index = Index::open(&store).unwrap();
    let first = revision(change(1), &[], "first");
    store.write_all(slice::from_ref(&first)).unwrap();
    index.refresh(&store).unwrap();
    assert_eq!(index, Index::open(&store).unwrap());

    // Once the op log dropped what it missed, it reads the index again.
    for i in 0..100 {
        let object = revision(change(2), &[], &format!("{i}"));
        store.write_all(slice::from_ref(&object)).unwrap();
    }
    index.refresh(&store).unwrap();
    assert_eq!(index.revisions(change(2)).len(), 100);
    assert_eq!(index.heads(change(1)), [revision_id(&first)]);
}

#[test]
fn rejects_records_that_do_not_decode() {
    let (_dir, store) = store();
    let bytes = vec![ObjectKind::Revision as u8, 1, 2, 3];
    let id = ObjectId::from_bytes(*Digest::of(&bytes).as_bytes());
    let malformed = EncodedObject::verify(id, bytes).unwrap();
    let error = store.write_all(&[malformed]).unwrap_err();
    assert!(matches!(error, StoreError::Decode { .. }), "{error}");
    assert!(!store.contains(id).unwrap());
}

#[test]
fn rebuilds_never_miss_concurrent_writes() {
    let (_dir, store) = store();
    let done = AtomicBool::new(false);
    let written: Vec<_> = thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                Index::rebuild(&store).unwrap();
            }
        });
        let writers: Vec<_> = (0..4)
            .map(|writer| {
                let store = &store;
                scope.spawn(move || {
                    (0..20)
                        .map(|i| {
                            let object = revision(change(writer), &[], &format!("{i}"));
                            store.write_all(slice::from_ref(&object)).unwrap();
                            object.id()
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let written = writers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect();
        done.store(true, Ordering::Relaxed);
        written
    });
    let index = Index::open(&store).unwrap();
    for id in written {
        assert!(index.known.contains(&RecordId::Revision(id.into())), "{id}");
    }
}

#[test]
fn records_written_after_an_unsettled_entry_are_not_logged_yet() {
    let (_dir, store) = store();
    let (_, writing) = store
        .oplog()
        .append_in_progress(&Effect::Wrote(Vec::new()))
        .unwrap();
    let written = revision(change(1), &[], "after the entry in progress");
    store.write_all(slice::from_ref(&written)).unwrap();
    let index = Index::open(&store).unwrap();
    let record = RecordId::Revision(written.id().into());
    assert!(index.known.contains(&record), "known in memory");
    // A power loss takes the record back before its entry settles.
    fs::remove_file(store.path(written.id())).unwrap();
    let (logged, _) = Index::read(&store).unwrap();
    assert!(!logged.known.contains(&record));
    drop(writing);
    assert!(!Index::open(&store).unwrap().known.contains(&record));
}

#[test]
fn rebuilds_keep_records_of_unsettled_entries_in_memory() {
    let (_dir, store) = store();
    let (_, _writing) = store
        .oplog()
        .append_in_progress(&Effect::Wrote(Vec::new()))
        .unwrap();
    let written = revision(change(1), &[], "after the entry in progress");
    store.write_all(slice::from_ref(&written)).unwrap();
    let record = RecordId::Revision(written.id().into());
    let rebuilt = Index::rebuild(&store).unwrap();
    assert!(rebuilt.index.known.contains(&record), "known in memory");
    let (logged, _) = Index::read(&store).unwrap();
    assert!(!logged.known.contains(&record), "not written yet");
}
