//! Tests for landing on `policy`, and for garbage collection while landings
//! wait.

use std::thread;

use origin_shared::{BlobId, Statement};

use super::*;

#[test]
fn landings_wait_for_collection() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    repo.change_on_main(1, &[("a", "a")]);
    // Its promotions refer to payloads they don't write.
    let collecting = repo.store.lock_for_collection().unwrap();
    let (done, landed) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| done.send(repo.land(1)).unwrap());
        thread::sleep(std::time::Duration::from_millis(100));
        assert!(landed.try_recv().is_err(), "the landing waits");
        drop(collecting);
        assert!(matches!(
            landed.recv().unwrap()[..],
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
        .unwrap()
        .issue(&statement, Some(payload.clone()))
        .unwrap();

    // Collection starts, two hours on, under a writer holding objects;
    // meanwhile a policy lands that keeps the payload longer.
    let held = repo.store.hold_objects().unwrap();
    let (done, collected) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let later = std::time::SystemTime::now() + std::time::Duration::from_hours(2);
            let retention = || Ok::<_, PolicyError>(Policy::load(&repo.store)?.retention);
            let dropped = Annotations::open(&repo.store)
                .unwrap()
                .drop_expired_payloads(retention, later)
                .unwrap();
            done.send(dropped).unwrap();
        });
        thread::sleep(std::time::Duration::from_millis(100));
        assert!(collected.try_recv().is_err(), "collection waits");
        let tip = Base::Acceptance(repo.tip(&TargetName::POLICY).acceptance);
        let longer = repo.policy_toml(&rules("1000d"));
        repo.revision(2, &[], tip, &[("policy.toml", &longer)]);
        let landed = repo
            .queue()
            .land_change(ChangeId::from_bytes([2; 16]), &TargetName::POLICY)
            .unwrap();
        assert!(matches!(landed[..], [Landing::Landed { .. }]), "{landed:?}");
        drop(held);
        assert_eq!(collected.recv().unwrap().payloads, 0);
    });
    assert!(repo.store.contains(payload.id()).unwrap());
}

#[test]
fn policy_landings_keep_a_valid_policy() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let policy = TargetName::POLICY;
    let tip = Base::Acceptance(repo.tip(&policy).acceptance);
    for (change, toml) in [(1, "not = [valid"), (2, "[nodes]\n")] {
        repo.revision(change, &[], tip, &[("policy.toml", toml)]);
        let result = repo
            .queue()
            .land_change(ChangeId::from_bytes([change; 16]), &policy);
        assert!(
            matches!(
                result,
                Err(LandError::InvalidPolicy(_) | LandError::NoAuthority(_))
            ),
            "{result:?}"
        );
    }
    // A valid edit lands, and later landings are under it.
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
        description: BlobId::from(EncodedObject::blob(b"").id()),
        author: "user:ada".parse().unwrap(),
    }
    .encode();
    repo.store
        .write_all(std::slice::from_ref(&encoded))
        .unwrap();
    let landings = repo
        .queue()
        .land_change(ChangeId::from_bytes([3; 16]), &policy)
        .unwrap();
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
    // The first edit requires a review to land on `policy`; the second,
    // stacked on it, must then wait for one.
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
        .land_change(ChangeId::from_bytes([2; 16]), &policy)
        .unwrap();
    assert_eq!(landed(&landings[0]), first);
    assert!(matches!(landings[1], Landing::Pending(_)), "{landings:?}");

    let log = repo.store.log(&policy).unwrap();
    for (seq, &id) in log.iter().enumerate() {
        let body = repo.store.get(id).unwrap().body().clone();
        let governing = Policy::governing(id, &body).unwrap();
        assert_eq!(governing, if seq == 0 { id } else { log[seq - 1] });
    }
    // An acceptance naming any other policy than the one before it is
    // refused, its own included.
    let mut body = repo.store.get(log[1]).unwrap().body().clone();
    body.policy = Some(log[1]);
    assert!(matches!(
        Policy::governing(log[1], &body),
        Err(PolicyError::NotPredecessor(1))
    ));
}
