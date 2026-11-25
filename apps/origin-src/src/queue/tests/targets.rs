//! Tests for landing targets on each other, and the merges that takes.

use origin_shared::Statement;

use super::*;

#[test]
fn targets_land_on_each_other() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let (main, overhaul) = (
        TargetName::MAIN,
        TargetName::new("overhaul").expect("`TargetName::new()` succeeds"),
    );
    repo.change_on_main(1, &[("shared", "v1")]);
    repo.land(1);
    let queue = repo.queue();
    queue
        .create_target(&overhaul, &main)
        .expect("`create_target(&overhaul, &main)` succeeds");
    assert!(matches!(
        queue.create_target(&overhaul, &main),
        Err(LandError::TargetExists(_))
    ));
    assert_eq!(repo.state(&overhaul), repo.state(&main));

    let on_overhaul = Base::Acceptance(repo.tip(&overhaul).acceptance);
    repo.revision(2, &[], on_overhaul, &[("shared", "v2"), ("new", "n")]);
    queue
        .land_change(
            &mut repo.annotations(),
            ChangeId::from_bytes([2; 16]),
            &overhaul,
        )
        .expect("`land_change` succeeds");
    repo.change_on_main(3, &[("other", "o")]);
    repo.land(3);

    assert!(matches!(
        queue
            .land_target(&mut repo.annotations(), &main, &overhaul)
            .expect("`land_target()` succeeds"),
        Landing::Landed { .. }
    ));
    let landing = queue
        .land_target(&mut repo.annotations(), &overhaul, &main)
        .expect("`land_target()` succeeds");
    assert!(matches!(
        landing,
        Landing::Landed {
            source: Source::Target(_),
            ..
        }
    ));
    let expected = repo.files(&[("shared", "v2"), ("new", "n"), ("other", "o")]);
    assert_eq!(repo.state(&main), expected);
    assert_eq!(repo.state(&overhaul), expected);
    assert_eq!(
        queue
            .land_target(&mut repo.annotations(), &overhaul, &main)
            .expect("`land_target()` succeeds"),
        Landing::Unchanged(Proposal::Target(overhaul.clone()))
    );
    assert!(matches!(
        queue.land_target(&mut repo.annotations(), &main, &main),
        Err(LandError::SameTarget(_))
    ));
}

#[test]
fn sibling_targets_merge_from_where_they_forked() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    repo.change_on_main(1, &[("keep", "k"), ("delete", "d")]);
    repo.land(1);
    let (a, b) = (
        TargetName::new("a").expect("`TargetName::new()` succeeds"),
        TargetName::new("b").expect("`TargetName::new()` succeeds"),
    );
    let queue = repo.queue();
    queue
        .create_target(&a, &TargetName::MAIN)
        .expect("`create_target(&a, &TargetName::MAIN)` succeeds");
    queue
        .create_target(&b, &TargetName::MAIN)
        .expect("`create_target(&b, &TargetName::MAIN)` succeeds");
    let on = |target| Base::Acceptance(repo.tip(target).acceptance);
    repo.revision(2, &[], on(&a), &[("keep", "k")]);
    queue
        .land_change(&mut repo.annotations(), ChangeId::from_bytes([2; 16]), &a)
        .expect("`a)` succeeds");
    repo.revision(
        3,
        &[],
        on(&b),
        &[("keep", "k"), ("delete", "d"), ("other", "o")],
    );
    queue
        .land_change(&mut repo.annotations(), ChangeId::from_bytes([3; 16]), &b)
        .expect("`b)` succeeds");
    assert!(matches!(
        queue
            .land_target(&mut repo.annotations(), &a, &b)
            .expect("`land_target(&mut repo.annotations(), &a, &b)` succeeds"),
        Landing::Landed { .. }
    ));
    assert_eq!(repo.state(&b), repo.files(&[("keep", "k"), ("other", "o")]));
}

#[test]
fn a_merge_that_changes_nothing_still_records_the_tip() {
    let repo = Repo::new(
        "[targets.b]\nauthority = \"node:queue\"\n\
         require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"user:ci\" }]\n",
    );
    let queue = repo.queue();
    let annotations =
        Annotations::open(&repo.store).expect("`Annotations::open(&repo.store)` succeeds");
    let pass = |subject| {
        let statement = Statement {
            subject,
            inputs: None,
            ..ci_test(RevisionId::from(EncodedObject::blob(b"").id()), "user:ci")
        };
        annotations
            .issue(&statement, None)
            .expect("`issue(&statement, None)` succeeds");
    };
    let (a, b) = (
        TargetName::new("a").expect("`TargetName::new()` succeeds"),
        TargetName::new("b").expect("`TargetName::new()` succeeds"),
    );
    let land = |change: u8, target: &TargetName, files: &[(&str, &str)]| {
        let on = Base::Acceptance(repo.tip(target).acceptance);
        pass(RecordId::Revision(repo.revision(change, &[], on, files)));
        queue
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([change; 16]),
                target,
            )
            .expect("`land_change` succeeds");
    };
    queue
        .create_target(&a, &TargetName::MAIN)
        .expect("`create_target(&a, &TargetName::MAIN)` succeeds");
    queue
        .create_target(&b, &TargetName::MAIN)
        .expect("`create_target(&b, &TargetName::MAIN)` succeeds");
    land(1, &a, &[("f", "f")]);
    land(2, &b, &[("f", "f")]);
    let state = repo.state(&b);
    assert!(matches!(
        queue
            .land_target(&mut repo.annotations(), &a, &b)
            .expect("`land_target(&mut repo.annotations(), &a, &b)` succeeds"),
        Landing::Landed { .. }
    ));
    assert_eq!(repo.state(&b), state);
    assert_eq!(
        queue
            .land_target(&mut repo.annotations(), &a, &b)
            .expect("`land_target(&mut repo.annotations(), &a, &b)` succeeds"),
        Landing::Unchanged(Proposal::Target(a.clone()))
    );

    land(3, &a, &[]);
    let landing = queue
        .land_target(&mut repo.annotations(), &a, &b)
        .expect("`land_target(&mut repo.annotations(), &a, &b)` succeeds");
    let Landing::Pending(pending) = landing else {
        panic!("a deletion waits for its evidence: {landing:?}")
    };
    pass(pending.subject);
    assert!(matches!(
        queue
            .land_target(&mut repo.annotations(), &a, &b)
            .expect("`land_target(&mut repo.annotations(), &a, &b)` succeeds"),
        Landing::Landed { .. }
    ));
    assert_eq!(repo.state(&b), repo.files(&[]));

    let c = TargetName::new("c").expect("`TargetName::new()` succeeds");
    queue
        .create_target(&c, &TargetName::MAIN)
        .expect("`create_target(&c, &TargetName::MAIN)` succeeds");
    for _ in 0..2 {
        for (from, onto) in [(&c, &TargetName::MAIN), (&TargetName::MAIN, &c)] {
            assert_eq!(
                queue
                    .land_target(&mut repo.annotations(), from, onto)
                    .expect("`land_target(&mut repo.annotations(), from, onto)` succeeds"),
                Landing::Unchanged(Proposal::Target(from.clone()))
            );
        }
    }
}

#[test]
fn targets_merge_from_their_newest_shared_acceptance() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let queue = repo.queue();
    let [a, b, c, d] = ["a", "b", "c", "d"]
        .map(|name| TargetName::new(name).expect("`TargetName::new(name)` succeeds"));
    let land = |change: u8, target: &TargetName, files: &[(&str, &str)]| {
        let on = Base::Acceptance(repo.tip(target).acceptance);
        repo.revision(change, &[], on, files);
        queue
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([change; 16]),
                target,
            )
            .expect("`land_change` succeeds");
    };
    queue
        .create_target(&c, &TargetName::MAIN)
        .expect("`create_target(&c, &TargetName::MAIN)` succeeds");
    queue
        .create_target(&d, &TargetName::MAIN)
        .expect("`create_target(&d, &TargetName::MAIN)` succeeds");
    repo.change_on_main(1, &[("delete", "d")]);
    repo.land(1);
    queue
        .create_target(&a, &TargetName::MAIN)
        .expect("`create_target(&a, &TargetName::MAIN)` succeeds");
    queue
        .create_target(&b, &TargetName::MAIN)
        .expect("`create_target(&b, &TargetName::MAIN)` succeeds");
    for n in 0..4 {
        let counter = n.to_string();
        land(10 + n, &a, &[("a", &counter)]);
        land(20 + n, &b, &[("delete", "d"), ("b", &counter)]);
    }
    land(30, &c, &[("c", "c")]);
    land(31, &d, &[("d", "d")]);
    assert!(matches!(
        queue
            .land_target(&mut repo.annotations(), &c, &a)
            .expect("`land_target(&mut repo.annotations(), &c, &a)` succeeds"),
        Landing::Landed { .. }
    ));
    assert!(matches!(
        queue
            .land_target(&mut repo.annotations(), &d, &b)
            .expect("`land_target(&mut repo.annotations(), &d, &b)` succeeds"),
        Landing::Landed { .. }
    ));

    assert!(matches!(
        queue
            .land_target(&mut repo.annotations(), &a, &b)
            .expect("`land_target(&mut repo.annotations(), &a, &b)` succeeds"),
        Landing::Landed { .. }
    ));
    assert_eq!(
        repo.state(&b),
        repo.files(&[("a", "3"), ("b", "3"), ("c", "c"), ("d", "d")]),
        "the deletion stands"
    );
}

#[test]
fn criss_crossed_targets_merge_what_their_bases_agree_on() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let queue = repo.queue();
    let [a, b] =
        ["a", "b"].map(|name| TargetName::new(name).expect("`TargetName::new(name)` succeeds"));
    let land = |change: u8, target: &TargetName, files: &[(&str, &str)]| {
        let on = Base::Acceptance(repo.tip(target).acceptance);
        repo.revision(change, &[], on, files);
        queue
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([change; 16]),
                target,
            )
            .expect("`land_change` succeeds");
    };
    queue
        .create_target(&a, &TargetName::MAIN)
        .expect("`create_target(&a, &TargetName::MAIN)` succeeds");
    queue
        .create_target(&b, &TargetName::MAIN)
        .expect("`create_target(&b, &TargetName::MAIN)` succeeds");
    land(1, &a, &[("a", "0"), ("shared", "a")]);
    land(2, &b, &[("b", "0"), ("shared", "b")]);
    let tips = [repo.tip(&a), repo.tip(&b)];
    let policy = Some(repo.tip(&TargetName::POLICY).acceptance);
    for (target, prev, other, shared) in [(&a, tips[0], tips[1], "a"), (&b, tips[1], tips[0], "b")]
    {
        let merged = AcceptanceBody {
            target: target.id(),
            seq: prev.seq + 1,
            prev: Some(prev.acceptance),
            state: repo.files(&[("a", "0"), ("b", "0"), ("shared", shared)]),
            landed: vec![Source::Target(other.acceptance)],
            evidence: vec![],
            policy,
        }
        .sign(&repo.key);
        repo.store
            .append(target, &merged)
            .expect("`append(target, &merged)` succeeds");
    }
    land(3, &a, &[("a", "1"), ("b", "0"), ("shared", "a")]);
    land(4, &b, &[("a", "0"), ("b", "1"), ("shared", "b")]);

    let Landing::Pending(pending) = queue
        .land_target(&mut repo.annotations(), &a, &b)
        .expect("`land_target(&mut repo.annotations(), &a, &b)` succeeds")
    else {
        panic!("the bases disagree on `shared`, so it can't be merged blindly");
    };
    assert!(
        matches!(&pending.blocked, Blocked::Conflict { paths, .. } if paths.iter().map(RepoPath::as_str).eq(["shared"])),
        "{pending:?}"
    );
    land(5, &a, &[("a", "1"), ("b", "0"), ("shared", "b")]);
    assert!(matches!(
        queue
            .land_target(&mut repo.annotations(), &a, &b)
            .expect("`land_target(&mut repo.annotations(), &a, &b)` succeeds"),
        Landing::Landed { .. }
    ));
    assert_eq!(
        repo.state(&b),
        repo.files(&[("a", "1"), ("b", "1"), ("shared", "b")])
    );
}

#[test]
fn bases_that_disagree_on_a_kind_conflict_on_any_change_under_it() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let queue = repo.queue();
    let land = |change: u8, target: &TargetName, files: &[(&str, &str)]| {
        let on = Base::Acceptance(repo.tip(target).acceptance);
        repo.revision(change, &[], on, files);
        queue
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([change; 16]),
                target,
            )
            .expect("`land_change` succeeds");
    };
    let [a, b] =
        ["a", "b"].map(|name| TargetName::new(name).expect("`TargetName::new(name)` succeeds"));
    queue
        .create_target(&a, &TargetName::MAIN)
        .expect("`create_target(&a, &TargetName::MAIN)` succeeds");
    queue
        .create_target(&b, &TargetName::MAIN)
        .expect("`create_target(&b, &TargetName::MAIN)` succeeds");
    land(1, &a, &[("p", "file")]);
    land(2, &b, &[("p/delete", "original")]);
    let tips = [repo.tip(&a), repo.tip(&b)];
    let policy = Some(repo.tip(&TargetName::POLICY).acceptance);
    for (target, prev, other) in [(&a, tips[0], tips[1]), (&b, tips[1], tips[0])] {
        let merged = AcceptanceBody {
            target: target.id(),
            seq: prev.seq + 1,
            prev: Some(prev.acceptance),
            state: repo.files(&[("p/delete", "original")]),
            landed: vec![Source::Target(other.acceptance)],
            evidence: vec![],
            policy,
        }
        .sign(&repo.key);
        repo.store
            .append(target, &merged)
            .expect("`append(target, &merged)` succeeds");
    }
    land(3, &a, &[("a", "new")]);
    land(4, &b, &[("p/delete", "original"), ("p/new", "new")]);

    let Landing::Pending(pending) = queue
        .land_target(&mut repo.annotations(), &a, &b)
        .expect("`land_target(&mut repo.annotations(), &a, &b)` succeeds")
    else {
        panic!("the bases disagree on `p`, so it can't be merged blindly");
    };
    assert!(
        matches!(&pending.blocked, Blocked::Conflict { paths, .. } if paths.iter().map(RepoPath::as_str).eq(["p"])),
        "{pending:?}"
    );
}
