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
        let annotations = Annotations::open(&store).expect("`Annotations::open(&store)` succeeds");
        let id = annotations
            .issue(&statement, Some(payload.clone()))
            .expect("`issue(&statement, Some(payload.clone()))` succeeds");
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
    .expect("`toml::from_str()` succeeds")
    .retention;
    let mut annotations = Annotations::open(&store).expect("`Annotations::open(&store)` succeeds");
    let now = SystemTime::now();
    assert_eq!(
        annotations
            .drop_expired_payloads(|| Ok::<_, StoreError>(retention.clone()), now)
            .expect("`drop_expired_payloads()` succeeds"),
        Dropped::default(),
        "not a day old yet"
    );
    let later = now + Duration::from_hours(48);
    let dropped = annotations
        .drop_expired_payloads(|| Ok::<_, StoreError>(retention.clone()), later)
        .expect("`drop_expired_payloads()` succeeds");
    assert_eq!(dropped.payloads, 1);
    assert!(!store.contains(log).expect("`contains(log)` succeeds"));
    for stays in [empty, review, shared] {
        assert!(
            store.contains(stays).expect("`contains(stays)` succeeds"),
            "{stays}"
        );
    }
    assert!(store.get(log_statement).is_ok(), "statements are permanent");
}

#[test]
fn without_rules_collection_waits_for_no_writer() {
    let (_dir, store) = store();
    let held = store.hold_objects().expect("`hold_objects()` succeeds");
    let (done, collected) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let none = || Ok::<_, StoreError>(Retention::default());
            let dropped = Annotations::open(&store)
                .expect("`Annotations::open(&store)` succeeds")
                .drop_expired_payloads(none, SystemTime::now())
                .expect("`drop_expired_payloads(none, SystemTime::now())` succeeds");
            done.send(dropped).expect("`send(dropped)` succeeds");
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
        .expect("`Annotations::open(&store)` succeeds")
        .issue(&statement, Some(payload.clone()))
        .expect("`issue(&statement, Some(payload.clone()))` succeeds");
    let retention = toml::from_str::<crate::policy::Policy>("[retention]\n\"ci.*\" = \"1s\"\n")
        .expect("`toml::from_str()` succeeds")
        .retention;

    let held = store.hold_objects().expect("`hold_objects()` succeeds");
    assert_eq!(
        store
            .write_all(std::slice::from_ref(&payload))
            .expect("`write_all(std::slice::from_ref(&payload))` succeeds"),
        0
    );
    let (done, collected) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let later = SystemTime::now() + Duration::from_hours(1);
            let dropped = Annotations::open(&store)
                .expect("`Annotations::open(&store)` succeeds")
                .drop_expired_payloads(|| Ok::<_, StoreError>(retention.clone()), later)
                .expect("`drop_expired_payloads()` succeeds");
            done.send(dropped).expect("`send(dropped)` succeeds");
        });
        thread::sleep(Duration::from_millis(100));
        assert!(collected.try_recv().is_err(), "collection waits");
        let reusing = Revision {
            description: payload.id().into(),
            ..store.get(first).unwrap()
        };
        store.write_all(&[reusing.encode()]).unwrap();
        drop(held);
        assert_eq!(
            collected.recv().expect("`recv()` succeeds"),
            Dropped::default()
        );
    });
    assert!(
        store
            .contains(payload.id())
            .expect("`contains(payload.id())` succeeds")
    );
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
        .expect("`Annotations::open(&store)` succeeds")
        .issue(&result, Some(payload.clone()))
        .expect("`issue(&result, Some(payload.clone()))` succeeds");
    let second = RecordId::Revision(revision(&store, 2, &[], FILES));

    let collecting = store
        .lock_for_collection()
        .expect("`lock_for_collection()` succeeds");
    let (done, promoted) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let issuer = "node:queue".parse().expect("`parse()` succeeds");
            let promotions = Annotations::open(&store)
                .unwrap()
                .promote(second, &issuer, |_| Ok(true))
                .unwrap();
            done.send(promotions.len()).unwrap();
        });
        thread::sleep(Duration::from_millis(100));
        assert!(promoted.try_recv().is_err(), "promotion waits");
        drop(collecting);
        assert_eq!(promoted.recv().expect("`recv()` succeeds"), 1);
    });
    assert!(
        store
            .contains(payload.id())
            .expect("`contains(payload.id())` succeeds")
    );
}

#[test]
fn writers_never_trust_a_pack_collection_rewrote() {
    let (dir, store) = store();
    let first = revision(&store, 1, &[], FILES);
    let payload = EncodedObject::blob(b"expired, then needed again");
    let mut statement = check(first, "ci.test", Verdict::Pass, "node:ci", None);
    statement.payload = Some(payload.id().into());
    Annotations::open(&store)
        .expect("`Annotations::open(&store)` succeeds")
        .issue(&statement, Some(payload.clone()))
        .expect("`issue(&statement, Some(payload.clone()))` succeeds");
    store.compact().expect("`compact()` succeeds");
    let writer = Store::new(dir.path().join(".origin"), Native::default());
    assert!(
        writer
            .contains(payload.id())
            .expect("`contains(payload.id())` succeeds")
    );

    let retention = toml::from_str::<crate::policy::Policy>("[retention]\n\"ci.*\" = \"1s\"\n")
        .expect("`toml::from_str()` succeeds")
        .retention;
    let later = SystemTime::now() + Duration::from_hours(1);
    let dropped = Annotations::open(&store)
        .expect("`Annotations::open(&store)` succeeds")
        .drop_expired_payloads(|| Ok::<_, StoreError>(retention.clone()), later)
        .expect("`drop_expired_payloads()` succeeds");
    assert_eq!(dropped.payloads, 1, "its pack is rewritten without it");
    let _held = writer.hold_objects().expect("`hold_objects()` succeeds");
    assert_eq!(
        writer
            .write_all(std::slice::from_ref(&payload))
            .expect("`write_all(std::slice::from_ref(&payload))` succeeds"),
        1,
        "so the writer writes it again"
    );
    assert!(
        store
            .contains(payload.id())
            .expect("`contains(payload.id())` succeeds")
    );
}
