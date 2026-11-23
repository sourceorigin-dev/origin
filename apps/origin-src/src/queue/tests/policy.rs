//! Tests for landing on `policy`, and for garbage collection while landings
//! wait.

use origin_shared::BlobId;

use super::*;

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
