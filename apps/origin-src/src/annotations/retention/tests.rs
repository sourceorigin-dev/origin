use std::thread;
use std::time::Duration;

use origin_platform::Native;
use origin_shared::{EncodedObject, Record, Revision, Verdict};

use super::*;
use crate::annotations::tests::{FILES, check, issue, revision, store};
use crate::store::Store;

#[test]
fn retention_drops_payloads_and_keeps_statements() {
    let (_dir, store) = store();
    let first = revision(&store, 1, &[], &[("empty", "")]);
    let with_payload = |kind: &str, payload: &[u8]| {
        let payload = EncodedObject::attachment(payload);
        let mut statement = check(first, kind, Verdict::Pass, "node:ci", None);
        statement.payload = Some(payload.id().into());
        let annotations = Annotations::open(&store).unwrap();
        let id = annotations
            .issue(&statement, Some(payload.clone()))
            .unwrap();
        (id, payload.id())
    };
    let (log_statement, log) = with_payload("ci.test", b"a long log");
    let (_, empty) = with_payload("ci.lint", b"");
    let (_, review) = with_payload("review", b"looks good");
    let (_, shared) = with_payload("ci.bench", b"shared");
    let mut kept = check(first, "keep", Verdict::Pass, "node:ci", None);
    kept.payload = Some(shared.into());
    issue(&store, &kept);

    let retention: Retention = toml::from_str::<crate::policy::Policy>(
        "[retention]\n\"ci.*\" = \"1d\"\n\"keep\" = \"1000d\"\n",
    )
    .unwrap()
    .retention;
    let mut annotations = Annotations::open(&store).unwrap();
    let now = SystemTime::now();
    assert_eq!(
        annotations
            .drop_expired_payloads(|| Ok::<_, StoreError>(retention.clone()), now)
            .unwrap(),
        Dropped::default(),
        "not a day old yet"
    );
    let later = now + Duration::from_hours(48);
    let dropped = annotations
        .drop_expired_payloads(|| Ok::<_, StoreError>(retention.clone()), later)
        .unwrap();
    assert_eq!(dropped.payloads, 1);
    assert!(!store.contains(log).unwrap());
    for stays in [empty, review, shared] {
        assert!(store.contains(stays).unwrap(), "{stays}");
    }
    assert!(store.get(log_statement).is_ok(), "statements are permanent");
}

#[test]
fn without_rules_collection_waits_for_no_writer() {
    let (_dir, store) = store();
    let held = store.hold_objects().unwrap();
    let (done, collected) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let none = || Ok::<_, StoreError>(Retention::default());
            let dropped = Annotations::open(&store)
                .unwrap()
                .drop_expired_payloads(none, SystemTime::now())
                .unwrap();
            done.send(dropped).unwrap();
        });
        let finished = collected.recv_timeout(Duration::from_secs(2));
        drop(held);
        assert_eq!(finished, Ok(Dropped::default()), "it waited for the writer");
    });
}

#[test]
fn collection_waits_for_writers_holding_objects() {
    let (_dir, store) = store();
    let first = revision(&store, 1, &[], FILES);
    let payload = EncodedObject::attachment(b"a log, and a description");
    let mut statement = check(first, "ci.test", Verdict::Pass, "node:ci", None);
    statement.payload = Some(payload.id().into());
    Annotations::open(&store)
        .unwrap()
        .issue(&statement, Some(payload.clone()))
        .unwrap();
    let retention = toml::from_str::<crate::policy::Policy>("[retention]\n\"ci.*\" = \"1s\"\n")
        .unwrap()
        .retention;

    let held = store.hold_objects().unwrap();
    assert_eq!(store.write_all(std::slice::from_ref(&payload)).unwrap(), 0);
    let (done, collected) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let later = SystemTime::now() + Duration::from_hours(1);
            let dropped = Annotations::open(&store)
                .unwrap()
                .drop_expired_payloads(|| Ok::<_, StoreError>(retention.clone()), later)
                .unwrap();
            done.send(dropped).unwrap();
        });
        thread::sleep(Duration::from_millis(100));
        assert!(collected.try_recv().is_err(), "collection waits");
        let reusing = Revision {
            description: payload.id().into(),
            ..store.get(first).unwrap()
        };
        store.write_all(&[reusing.encode()]).unwrap();
        drop(held);
        assert_eq!(collected.recv().unwrap(), Dropped::default());
    });
    assert!(store.contains(payload.id()).unwrap());
}

#[test]
fn promotions_and_collection_exclude_each_other() {
    let (_dir, store) = store();
    let first = revision(&store, 1, &[], FILES);
    let payload = EncodedObject::attachment(b"a log to promote");
    let inputs = Some((&["src"][..], false));
    let mut result = check(first, "ci.test", Verdict::Pass, "node:ci", inputs);
    result.payload = Some(payload.id().into());
    Annotations::open(&store)
        .unwrap()
        .issue(&result, Some(payload.clone()))
        .unwrap();
    let second = RecordId::Revision(revision(&store, 2, &[], FILES));

    let collecting = store.lock_for_collection().unwrap();
    let (done, promoted) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let issuer = "node:queue".parse().unwrap();
            let promotions = Annotations::open(&store)
                .unwrap()
                .promote(second, &issuer, |_| Ok(true))
                .unwrap();
            done.send(promotions.len()).unwrap();
        });
        thread::sleep(Duration::from_millis(100));
        assert!(promoted.try_recv().is_err(), "promotion waits");
        drop(collecting);
        assert_eq!(promoted.recv().unwrap(), 1);
    });
    assert!(store.contains(payload.id()).unwrap());
}

#[test]
fn writers_never_trust_a_pack_collection_rewrote() {
    let (dir, store) = store();
    let first = revision(&store, 1, &[], FILES);
    let payload = EncodedObject::blob(b"expired, then needed again");
    let mut statement = check(first, "ci.test", Verdict::Pass, "node:ci", None);
    statement.payload = Some(payload.id().into());
    Annotations::open(&store)
        .unwrap()
        .issue(&statement, Some(payload.clone()))
        .unwrap();
    store.compact().unwrap();
    let writer = Store::new(dir.path().join(".origin"), Native::default());
    assert!(writer.contains(payload.id()).unwrap());

    let retention = toml::from_str::<crate::policy::Policy>("[retention]\n\"ci.*\" = \"1s\"\n")
        .unwrap()
        .retention;
    let later = SystemTime::now() + Duration::from_hours(1);
    let dropped = Annotations::open(&store)
        .unwrap()
        .drop_expired_payloads(|| Ok::<_, StoreError>(retention.clone()), later)
        .unwrap();
    assert_eq!(dropped.payloads, 1, "its pack is rewritten without it");
    let _held = writer.hold_objects().unwrap();
    assert_eq!(
        writer.write_all(std::slice::from_ref(&payload)).unwrap(),
        1,
        "so the writer writes it again"
    );
    assert!(store.contains(payload.id()).unwrap());
}
