use std::thread;

use origin_platform::Native;
use origin_shared::{AcceptanceBody, NodeKey, Tree, TreeId};
use parking_lot::Mutex;

use super::*;
use crate::store::tests::{Recorder, Synced};

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    (dir, store)
}

fn key() -> NodeKey {
    NodeKey::from_bytes(&[42; 32])
}

/// An acceptance of `target` following `prev`.
fn acceptance(target: &TargetName, prev: Option<Head>) -> Acceptance {
    AcceptanceBody {
        target: target.id(),
        seq: prev.map_or(0, |prev| prev.seq + 1),
        prev: prev.map(|prev| prev.acceptance),
        state: TreeId::from(Tree::default().encode().id()),
        landed: Vec::new(),
        evidence: Vec::new(),
        policy: None,
    }
    .sign(&key())
}

/// Appends `count` acceptances to `target`, returning their ids.
fn land<P: Platform>(store: &Store<P>, target: &TargetName, count: usize) -> Vec<AcceptanceId> {
    (0..count)
        .map(|_| {
            let head = store.head(target).unwrap();
            store.append(target, &acceptance(target, head)).unwrap()
        })
        .collect()
}

#[test]
fn appending_several_acceptances_creates_the_target() {
    let (_dir, store) = store();
    let main = TargetName::MAIN;
    let first = acceptance(&main, None);
    let second = acceptance(
        &main,
        Some(Head {
            seq: 0,
            acceptance: first.encode().id().into(),
        }),
    );
    store.append_all(&main, &[first, second], &[]).unwrap();
    assert_eq!(store.targets().unwrap(), [TargetName::MAIN]);
    assert_eq!(store.head(&main).unwrap().unwrap().seq, 1);
}

#[test]
fn appends_form_a_chain() {
    let (_dir, store) = store();
    let main = TargetName::MAIN;
    assert_eq!(store.head(&main).unwrap(), None);
    assert_eq!(store.log(&main).unwrap(), Vec::<AcceptanceId>::new());

    let ids = land(&store, &main, 3);
    assert_eq!(
        store.head(&main).unwrap(),
        Some(Head {
            seq: 2,
            acceptance: ids[2]
        })
    );
    assert_eq!(store.log(&main).unwrap(), ids);
    for (seq, &id) in (0..).zip(&ids) {
        assert_eq!(store.acceptance_at(main.id(), seq).unwrap(), Some(id));
    }
    assert_eq!(store.acceptance_at(main.id(), 3).unwrap(), None);
    assert_eq!(store.log_since(&main, 1, None).unwrap(), ids[1..]);
    assert_eq!(store.log_since(&main, 0, Some(2)).unwrap(), ids[..2]);
    assert_eq!(store.log_since(&main, 1, Some(9)).unwrap(), ids[1..]);
    assert_eq!(store.log_since(&main, 0, Some(0)).unwrap(), []);
    assert_eq!(store.log_since(&main, 3, None).unwrap(), []);
    let last = store.get(ids[2]).unwrap();
    assert_eq!(last.body().prev, Some(ids[1]));
    last.verify(&key().public()).unwrap();
    assert_eq!(store.targets().unwrap(), [main]);
}

#[test]
fn policy_heads_wait_for_collection() {
    let (_dir, store) = store();
    let collecting = store.lock_for_collection().unwrap();
    // Other targets don't.
    land(&store, &TargetName::MAIN, 1);
    let (done, landed) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| done.send(land(&store, &TargetName::POLICY, 1)).unwrap());
        thread::sleep(std::time::Duration::from_millis(100));
        assert!(landed.try_recv().is_err(), "the policy waits");
        drop(collecting);
        assert_eq!(landed.recv().unwrap().len(), 1);
    });
}

#[test]
fn appends_must_follow_the_latest_acceptance() {
    let (_dir, store) = store();
    let main = TargetName::MAIN;
    let ids = land(&store, &main, 2);
    let latest = store.head(&main).unwrap();

    let genesis = Head {
        seq: 0,
        acceptance: ids[0],
    };
    for stale in [None, Some(genesis)] {
        let error = store.append(&main, &acceptance(&main, stale)).unwrap_err();
        assert!(
            matches!(error, StoreError::Conflict { actual, .. } if actual == latest),
            "{error}"
        );
    }
    let error = store
        .append(&TargetName::POLICY, &acceptance(&main, None))
        .unwrap_err();
    assert!(matches!(error, StoreError::WrongTarget(_)), "{error}");
    assert_eq!(store.log(&main).unwrap(), ids, "nothing was appended");
}

#[test]
fn concurrent_appends_serialize() {
    let (_dir, store) = store();
    let main = TargetName::MAIN;
    let landed = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                for _ in 0..3 {
                    // Retry on conflict, as a landing queue would.
                    loop {
                        let head = store.head(&main).unwrap();
                        match store.append(&main, &acceptance(&main, head)) {
                            Ok(id) => break landed.lock().push(id),
                            Err(StoreError::Conflict { .. }) => {}
                            Err(error) => panic!("{error}"),
                        }
                    }
                }
            });
        }
    });
    let log = store.log(&main).unwrap();
    assert_eq!(log.len(), 24);
    let mut landed = landed.into_inner();
    landed.sort();
    let mut sorted = log.clone();
    sorted.sort();
    assert_eq!(sorted, landed, "every success is in the log once");
    for pair in log.windows(2) {
        assert_eq!(store.get(pair[1]).unwrap().body().prev, Some(pair[0]));
    }
}

#[test]
fn appends_repair_what_a_crash_left() {
    let (_dir, store) = store();
    let main = TargetName::MAIN;
    let ids = land(&store, &main, 2);
    let files = store.target_files(main.id());

    // A crash after syncing a log entry, before publishing the head.
    let unpublished = acceptance(
        &main,
        Some(Head {
            seq: 1,
            acceptance: ids[1],
        }),
    );
    let unpublished_id = AcceptanceId::from(unpublished.encode().id());
    store.write_all(&[unpublished.encode()]).unwrap();
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(files.log())
        .unwrap();
    writeln!(log, "{unpublished_id}").unwrap();
    // Then a crash in the middle of writing the next line.
    log.write_all(b"0123").unwrap();
    assert_eq!(store.log(&main).unwrap(), ids, "readers go by the head");

    // Landing on the published head finds the target moved.
    let stale = acceptance(&main, store.head(&main).unwrap());
    let error = store.append(&main, &stale).unwrap_err();
    let repaired = Head {
        seq: 2,
        acceptance: unpublished_id,
    };
    assert!(
        matches!(error, StoreError::Conflict { actual, .. } if actual == Some(repaired)),
        "{error}"
    );
    assert_eq!(store.head(&main).unwrap(), Some(repaired));
    land(&store, &main, 1);
    assert_eq!(store.log(&main).unwrap().len(), 4);
}

#[test]
fn repairs_advance_the_target_to_every_acceptance_left_unpublished() {
    let (_dir, store) = store();
    let main = TargetName::MAIN;
    let ids = land(&store, &main, 1);
    let before = store.oplog().last_seq().unwrap();
    let files = store.target_files(main.id());
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(files.log())
        .unwrap();
    let mut head = Head {
        seq: 0,
        acceptance: ids[0],
    };
    for _ in 0..3 {
        let next = acceptance(&main, Some(head)).encode();
        store.write_all(slice::from_ref(&next)).unwrap();
        head = Head {
            seq: head.seq + 1,
            acceptance: next.id().into(),
        };
        writeln!(log, "{}", head.acceptance).unwrap();
    }

    let stale = acceptance(&main, store.head(&main).unwrap());
    assert!(store.append(&main, &stale).is_err(), "repaired first");
    let advanced: Vec<_> = store
        .oplog()
        .after(before)
        .unwrap()
        .entries
        .into_iter()
        .filter_map(|entry| match entry.effect {
            Effect::Advanced { seq, .. } => Some(seq),
            _ => None,
        })
        .collect();
    assert_eq!(advanced, [1, 2, 3], "one event per acceptance");
    assert_eq!(store.head(&main).unwrap(), Some(head));
}

#[test]
fn repairs_sync_the_log_before_publishing_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Recorder::default());
    store.init().unwrap();
    let main = TargetName::MAIN;
    let ids = land(&store, &main, 1);
    // A crash after appending a log entry, before syncing it.
    let unsynced = acceptance(
        &main,
        Some(Head {
            seq: 0,
            acceptance: ids[0],
        }),
    );
    store.write_all(&[unsynced.encode()]).unwrap();
    let log = store.target_files(main.id()).log();
    let mut file = fs::OpenOptions::new().append(true).open(log).unwrap();
    writeln!(file, "{}", unsynced.encode().id()).unwrap();
    store.platform.0.lock().clear();

    assert!(store.append(&main, &unsynced).is_err(), "it's there now");
    let targets = store.targets_dir();
    let target = targets.join(main.id().to_hex());
    let shard = &unsynced.encode().id().to_hex()[..2];
    assert_eq!(
        store.platform.0.into_inner(),
        [
            Synced::Batch(0),
            Synced::Dir(dir.path().join(".origin/objects/loose").join(shard)),
            Synced::File,
            // The head, and the op log entry saying it advanced.
            Synced::Batch(2),
            Synced::Dir(targets),
            Synced::Dir(target),
        ],
        "the log, then the head, with its op log entry"
    );
}

#[test]
fn targets_are_named_before_their_first_head() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Recorder::default());
    store.init().unwrap();
    let main = TargetName::MAIN;
    store.platform.0.lock().clear();

    store.append(&main, &acceptance(&main, None)).unwrap();
    let targets = Synced::Dir(store.targets_dir());
    let synced = store.platform.0.into_inner();
    let named = synced.iter().position(|synced| *synced == targets);
    // The head, and the op log entry saying it advanced.
    let published = synced
        .iter()
        .rposition(|synced| *synced == Synced::Batch(2));
    assert!(named.unwrap() < published.unwrap(), "{synced:?}");
}

#[test]
fn acceptances_are_durable_before_the_log_refers_to_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Recorder::default());
    store.init().unwrap();
    let main = TargetName::MAIN;
    land(&store, &main, 1);
    store.platform.0.lock().clear();

    let id = land(&store, &main, 1)[0];
    let hex = id.to_hex();
    let targets = store.targets_dir();
    let target = targets.join(main.id().to_hex());
    assert_eq!(
        store.platform.0.into_inner(),
        [
            // The acceptance and its op log entry, which costs no sync
            // of its own.
            Synced::Batch(2),
            Synced::Dir(dir.path().join(".origin/objects/loose")),
            Synced::Dir(dir.path().join(".origin/objects/loose").join(&hex[..2])),
            Synced::File,
            Synced::Batch(2),
            Synced::Dir(targets),
            Synced::Dir(target),
        ],
        "the acceptance and its shard, then the log, then the head"
    );
}
