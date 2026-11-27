//! Tests for landing changes: rebasing, stacks, who may land, and keeping a
//! landing that waits.

use std::thread;

use origin_shared::{Statement, Verdict};
use parking_lot::Mutex;

use super::*;

#[test]
fn lands_rebases_and_skips_what_the_target_has() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let a = repo.change_on_main(1, &[("a", "a")]);
    let b = repo.change_on_main(2, &[("b", "b")]);

    let [landing] = &repo.land(1)[..] else {
        panic!()
    };
    assert_eq!(landed(landing), a, "made on the tip, it lands as it is");
    let acceptance = repo
        .store
        .get(repo.tip(&TargetName::MAIN).acceptance)
        .unwrap();
    acceptance.verify(&repo.key.public()).unwrap();
    assert_eq!(
        acceptance.body().policy,
        Some(repo.tip(&TargetName::POLICY).acceptance)
    );

    let [landing] = &repo.land(2)[..] else {
        panic!()
    };
    let rebased = landed(landing);
    assert_ne!(rebased, b, "made on an older tip, it is rebased");
    let revision = repo.store.get(rebased).unwrap();
    assert_eq!(revision.predecessors, [b]);
    assert_eq!(revision.author, "node:queue".parse().unwrap());
    let log = repo.store.log(&TargetName::MAIN).unwrap();
    assert_eq!(
        revision.base,
        Base::Acceptance(log[1]),
        "on the tip it landed after"
    );
    assert_eq!(
        repo.state(&TargetName::MAIN),
        repo.files(&[("a", "a"), ("b", "b")])
    );

    assert_eq!(repo.land(1), [Landing::Unchanged(change(1))]);
    assert_eq!(repo.land(2), [Landing::Unchanged(change(2))]);
    assert_eq!(repo.tip(&TargetName::MAIN).seq, 2);
}

#[test]
fn stacks_land_from_the_bottom() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let below = repo.change_on_main(1, &[("a", "a")]);
    let above = repo.revision(2, &[], Base::Revision(below), &[("a", "a"), ("b", "b")]);
    let landings = repo.land(2);
    assert_eq!(landings.len(), 2, "{landings:?}");
    assert_eq!(landed(&landings[0]), below);
    assert_ne!(landed(&landings[1]), above, "rebased onto the first");
    assert_eq!(
        repo.state(&TargetName::MAIN),
        repo.files(&[("a", "a"), ("b", "b")])
    );
}

#[test]
fn refuses_divergent_and_abandoned_changes() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let first = repo.change_on_main(1, &[("a", "a")]);
    let base = Base::Acceptance(repo.tip(&TargetName::MAIN).acceptance);
    repo.revision(1, &[first], base, &[("a", "left")]);
    repo.revision(1, &[first], base, &[("a", "right")]);
    let main = TargetName::MAIN;
    let error = repo
        .queue()
        .land_change(
            &mut repo.annotations(),
            ChangeId::from_bytes([1; 16]),
            &main,
        )
        .unwrap_err();
    assert!(matches!(error, LandError::Divergent { .. }), "{error}");

    let given_up = repo.change_on_main(2, &[("b", "b")]);
    let abandon = Statement {
        subject: RecordId::Revision(given_up),
        kind: Kind::ABANDON,
        verdict: None,
        issuer: "user:ada".parse().unwrap(),
        inputs: None,
        payload: None,
        supersedes: None,
        promoted_from: None,
    };
    repo.store.write_all(&[abandon.encode()]).unwrap();
    let error = repo
        .queue()
        .land_change(
            &mut repo.annotations(),
            ChangeId::from_bytes([2; 16]),
            &main,
        )
        .unwrap_err();
    assert!(matches!(error, LandError::Abandoned(_)), "{error}");
}

#[test]
fn only_the_authority_lands() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    repo.change_on_main(1, &[("a", "a")]);
    let change = ChangeId::from_bytes([1; 16]);
    let other = Queue::new(&repo.store, &repo.key, PrincipalName::new("other").unwrap());
    let error = other
        .land_change(&mut repo.annotations(), change, &TargetName::MAIN)
        .unwrap_err();
    assert!(matches!(error, LandError::NotAuthority { .. }), "{error}");

    let impostor = NodeKey::from_bytes(&[8; 32]);
    let queue = Queue::new(&repo.store, &impostor, PrincipalName::new("queue").unwrap());
    let error = queue
        .land_change(&mut repo.annotations(), change, &TargetName::MAIN)
        .unwrap_err();
    assert!(matches!(error, LandError::WrongKey(_)), "{error}");
    assert_eq!(repo.tip(&TargetName::MAIN).seq, 0);
}

#[test]
fn concurrent_landings_serialize() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let (proposers, each) = (6_u8, 3_u8);
    let changes: Vec<u8> = (0..proposers * each).map(|change| change + 1).collect();
    for &change in &changes {
        repo.change_on_main(change, &[(&format!("file-{change}"), "x")]);
    }
    let landed = Mutex::new(0);
    thread::scope(|scope| {
        for chunk in changes.chunks(usize::from(each)) {
            let (repo, landed) = (&repo, &landed);
            scope.spawn(move || {
                for &change in chunk {
                    assert!(matches!(repo.land(change)[..], [Landing::Landed { .. }]));
                    *landed.lock() += 1;
                }
            });
        }
    });
    assert_eq!(*landed.lock(), changes.len());
    let log = repo.store.log(&TargetName::MAIN).unwrap();
    assert_eq!(log.len(), changes.len() + 1);
    for pair in log.windows(2) {
        assert_eq!(repo.store.get(pair[1]).unwrap().body().prev, Some(pair[0]));
    }
    assert_eq!(repo.main_files().len(), changes.len(), "every file landed");
}

#[test]
fn pending_landings_round_trip() {
    let pending = Pending {
        onto: TargetName::new("release/v3").unwrap(),
        proposal: Proposal::Target(TargetName::MAIN),
        subject: RecordId::Snapshot(Tree::default().encode().id().into()),
        blocked: Blocked::Evidence(vec![Requirement {
            kind: Kind::new("ci.test").unwrap(),
            verdict: Verdict::Pass,
            issuer: "group:runners".parse().unwrap(),
        }]),
        expires: None,
    };
    let owner = Waiting {
        pending: pending.clone(),
        submitter: Caller::Owner,
    };
    assert_eq!(
        PendingFile::decode(&PendingFile::encode(&pending, &Caller::Owner)),
        Some(owner)
    );
    let conflict = Pending {
        proposal: change(9),
        blocked: Blocked::Conflict {
            statement: EncodedObject::blob(b"conflict").id().into(),
            paths: Vec::new(),
        },
        ..pending
    };
    let scoped = |capabilities: &[&str]| Waiting {
        pending: Pending {
            expires: Some(1_900_000_000),
            ..conflict.clone()
        },
        submitter: Caller::Principal {
            principal: "user:agent".parse().unwrap(),
            scope: Some(Scope {
                capabilities: capabilities.iter().map(|c| c.parse().unwrap()).collect(),
                expires: 1_900_000_000,
            }),
        },
    };
    for scoped in [scoped(&["write src", "land *"]), scoped(&[])] {
        assert_eq!(
            PendingFile::decode(&PendingFile::encode(&scoped.pending, &scoped.submitter)),
            Some(scoped)
        );
    }
    assert_eq!(PendingFile::decode("origin pending 2\n"), None);
}

#[test]
fn only_the_creator_of_a_change_abandons_it() {
    let repo = Repo::new(
        "[targets.main]\nauthority = \"node:queue\"\n\
         [grants]\nall = [\"land *\", \"annotate *\"]\n\"user:root\" = [\"admin\"]\n",
    );
    let revision = repo.change_on_main(1, &[("file", "work")]);
    let given_up = repo.change_on_main(2, &[("other", "work")]);
    let abandon = |issuer: &str, revision| Statement {
        subject: RecordId::Revision(revision),
        kind: Kind::ABANDON,
        verdict: None,
        issuer: issuer.parse().unwrap(),
        inputs: None,
        payload: None,
        supersedes: None,
        promoted_from: None,
    };
    repo.store
        .write_all(&[
            abandon("user:eve", revision).encode(),
            abandon("user:root", given_up).encode(),
        ])
        .unwrap();
    assert!(matches!(repo.land(1)[..], [Landing::Landed { .. }]));
    let error = repo
        .queue()
        .land_change(
            &mut repo.annotations(),
            ChangeId::from_bytes([2; 16]),
            &TargetName::MAIN,
        )
        .unwrap_err();
    assert!(
        matches!(error, LandError::Abandoned(_)),
        "an admin may: {error}"
    );
}

#[test]
fn a_waiting_stack_lands_whole_once_it_can() {
    let repo = requiring_ci();
    let lower = repo.change_on_main(1, &[("a", "a")]);
    repo.revision(2, &[], Base::Revision(lower), &[("a", "a"), ("b", "b")]);
    let [Landing::Pending(pending)] = &repo.land(2)[..] else {
        panic!("the lower change waits")
    };
    assert_eq!(pending.subject, RecordId::Revision(lower));
    assert_eq!(
        Queue::pending(&repo.store).unwrap(),
        [Pending {
            proposal: change(2),
            ..pending.clone()
        }],
        "kept as the landing of the whole stack"
    );

    let annotations = Annotations::open(&repo.store).unwrap();
    annotations.issue(&ci_test(lower, "user:ci"), None).unwrap();
    let landings = repo.land(2);
    assert_eq!(landed(&landings[0]), lower);
    assert!(
        matches!(landings[1], Landing::Landed { proposal: ref p, .. } if *p == change(2)),
        "{landings:?}"
    );
    assert_eq!(Queue::pending(&repo.store).unwrap(), []);
}

#[test]
fn an_expired_submitter_lands_nothing() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    repo.change_on_main(1, &[("src/lib.rs", "v2")]);
    let expired = Caller::Principal {
        principal: "user:eve".parse().unwrap(),
        scope: Some(Scope {
            capabilities: Vec::new(),
            expires: 1,
        }),
    };
    let landed = repo
        .queue()
        .submitted_by(expired, PathAccess::new(Level::Write, []))
        .land_change(
            &mut repo.annotations(),
            ChangeId::from_bytes([1; 16]),
            &TargetName::MAIN,
        );
    assert!(
        matches!(landed, Err(LandError::Forbidden(Forbidden::Expired))),
        "{landed:?}"
    );
    assert!(matches!(repo.land(1)[..], [Landing::Landed { .. }]));
}
