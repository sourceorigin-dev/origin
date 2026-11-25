//! Tests for landing on `policy`, and for garbage collection while landings
//! wait.

use std::thread;

use origin_shared::{AttachmentId, Statement};

use super::*;

#[test]
fn landings_wait_for_collection() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    repo.change_on_main(1, &[("a", "a")]);
    let collecting = repo
        .store
        .lock_for_collection()
        .expect("`lock_for_collection()` succeeds");
    let (done, landed) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            done.send(repo.land(1))
                .expect("`send(repo.land(1))` succeeds");
        });
        thread::sleep(std::time::Duration::from_millis(100));
        assert!(landed.try_recv().is_err(), "the landing waits");
        drop(collecting);
        assert!(matches!(
            landed.recv().expect("`recv()` succeeds")[..],
            [Landing::Landed { .. }]
        ));
    });
}

#[test]
fn collection_applies_a_policy_landed_while_it_waits() {
    let rules = |retention: &str| {
        format!(
            "[targets.main]\nauthority = \"node:queue\"\n[retention]\n\"ci.*\" = \"{retention}\"\n"
        )
    };
    let repo = Repo::new(&rules("1h"));
    let subject = repo.change_on_main(1, &[("a", "a")]);
    let payload = EncodedObject::blob(b"a log");
    let statement = Statement {
        payload: Some(payload.id().into()),
        ..ci_test(subject, "node:ci")
    };
    Annotations::open(&repo.store)
        .expect("`Annotations::open(&repo.store)` succeeds")
        .issue(&statement, Some(payload.clone()))
        .expect("`issue(&statement, Some(payload.clone()))` succeeds");

    let held = repo
        .store
        .hold_objects()
        .expect("`hold_objects()` succeeds");
    let (done, collected) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let later = std::time::SystemTime::now() + std::time::Duration::from_hours(2);
            let retention = || Ok::<_, PolicyError>(Policy::load(&repo.store)?.retention);
            let dropped = Annotations::open(&repo.store)
                .expect("`Annotations::open(&repo.store)` succeeds")
                .drop_expired_payloads(retention, later)
                .expect("`drop_expired_payloads(retention, later)` succeeds");
            done.send(dropped).expect("`send(dropped)` succeeds");
        });
        thread::sleep(std::time::Duration::from_millis(100));
        assert!(collected.try_recv().is_err(), "collection waits");
        let tip = Base::Acceptance(repo.tip(&TargetName::POLICY).acceptance);
        let longer = repo.policy_toml(&rules("1000d"));
        repo.revision(2, &[], tip, &[("policy.toml", &longer)]);
        let landed = repo
            .queue()
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([2; 16]),
                &TargetName::POLICY,
            )
            .expect("`land_change` succeeds");
        assert!(matches!(landed[..], [Landing::Landed { .. }]), "{landed:?}");
        drop(held);
        assert_eq!(collected.recv().expect("`recv()` succeeds").payloads, 0);
    });
    assert!(
        repo.store
            .contains(payload.id())
            .expect("`contains(payload.id())` succeeds")
    );
}

#[test]
fn policy_landings_keep_a_valid_policy() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let policy = TargetName::POLICY;
    let tip = Base::Acceptance(repo.tip(&policy).acceptance);
    for (change, toml) in [(1, "not = [valid"), (2, "[nodes]\n")] {
        repo.revision(change, &[], tip, &[("policy.toml", toml)]);
        let result = repo.queue().land_change(
            &mut repo.annotations(),
            ChangeId::from_bytes([change; 16]),
            &policy,
        );
        assert!(
            matches!(
                result,
                Err(LandError::InvalidPolicy(_) | LandError::NoAuthority(_))
            ),
            "{result:?}"
        );
    }
    let requiring = repo.policy_toml(
        "[targets.main]\nauthority = \"node:queue\"\n\
         require = [{ kind = \"review\", verdict = \"pass\", issuer = \"user:bob\" }]\n",
    );
    let encoded = Revision {
        change: ChangeId::from_bytes([3; 16]),
        predecessors: Vec::new(),
        base: tip,
        deps: Vec::new(),
        snapshot: repo.files(&[("policy.toml", &requiring)]),
        description: AttachmentId::from(EncodedObject::attachment(b"").id()),
        author: "user:ada".parse().unwrap(),
    }
    .encode();
    repo.store
        .write_all(std::slice::from_ref(&encoded))
        .expect("`write_all(std::slice::from_ref(&encoded))` succeeds");
    let landings = repo
        .queue()
        .land_change(
            &mut repo.annotations(),
            ChangeId::from_bytes([3; 16]),
            &policy,
        )
        .expect("`land_change` succeeds");
    assert!(
        matches!(landings[..], [Landing::Landed { .. }]),
        "{landings:?}"
    );
    repo.change_on_main(4, &[("a", "a")]);
    assert!(matches!(repo.land(4)[..], [Landing::Pending(_)]));
}

#[test]
fn each_policy_landing_is_governed_by_the_one_before() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let policy = TargetName::POLICY;
    let requiring = repo
        .policy_toml("[targets.main]\nauthority = \"node:queue\"\n")
        .replace(
            "[targets.policy]\nauthority = \"node:queue\"\n",
            "[targets.policy]\nauthority = \"node:queue\"\n\
         require = [{ kind = \"review\", verdict = \"pass\", issuer = \"user:bob\" }]\n",
        );
    let first = repo.revision(
        1,
        &[],
        Base::Acceptance(repo.tip(&policy).acceptance),
        &[("policy.toml", &requiring)],
    );
    let open = repo.policy_toml("[targets.main]\nauthority = \"node:queue\"\n");
    repo.revision(2, &[], Base::Revision(first), &[("policy.toml", &open)]);
    let landings = repo
        .queue()
        .land_change(
            &mut repo.annotations(),
            ChangeId::from_bytes([2; 16]),
            &policy,
        )
        .expect("`land_change` succeeds");
    assert_eq!(landed(&landings[0]), first);
    assert!(matches!(landings[1], Landing::Pending(_)), "{landings:?}");

    let log = repo.store.log(&policy).expect("`log(&policy)` succeeds");
    for (seq, &id) in log.iter().enumerate() {
        let body = repo
            .store
            .get(id)
            .expect("`get(id)` succeeds")
            .body()
            .clone();
        let governing =
            Policy::governing(id, &body).expect("`Policy::governing(id, &body)` succeeds");
        assert_eq!(governing, if seq == 0 { id } else { log[seq - 1] });
    }
    let mut body = repo
        .store
        .get(log[1])
        .expect("`get(log[1])` succeeds")
        .body()
        .clone();
    body.policy = Some(log[1]);
    assert!(matches!(
        Policy::governing(log[1], &body),
        Err(PolicyError::NotPredecessor(1))
    ));
}
