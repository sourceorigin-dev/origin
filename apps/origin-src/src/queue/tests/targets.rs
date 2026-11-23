//! Tests for landing targets on each other, and the merges that takes.

use origin_shared::Statement;

use super::*;

#[test]
fn targets_land_on_each_other() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let (main, overhaul) = (TargetName::MAIN, TargetName::new("overhaul").unwrap());
    repo.change_on_main(1, &[("shared", "v1")]);
    repo.land(1);
    let queue = repo.queue();
    queue.create_target(&overhaul, &main).unwrap();
    assert!(matches!(
        queue.create_target(&overhaul, &main),
        Err(LandError::TargetExists(_))
    ));
    assert_eq!(repo.state(&overhaul), repo.state(&main));

    // Work lands on the overhaul while `main` moves on elsewhere.
    let on_overhaul = Base::Acceptance(repo.tip(&overhaul).acceptance);
    repo.revision(2, &[], on_overhaul, &[("shared", "v2"), ("new", "n")]);
    queue
        .land_change(ChangeId::from_bytes([2; 16]), &overhaul)
        .unwrap();
    repo.change_on_main(3, &[("other", "o")]);
    repo.land(3);

    // Catching up merges `main`'s change since the fork; finishing
    // lands the overhaul on `main`.
    assert!(matches!(
        queue.land_target(&main, &overhaul).unwrap(),
        Landing::Landed { .. }
    ));
    let landing = queue.land_target(&overhaul, &main).unwrap();
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
        queue.land_target(&overhaul, &main).unwrap(),
        Landing::Unchanged(Proposal::Target(overhaul.clone()))
    );
    assert!(matches!(
        queue.land_target(&main, &main),
        Err(LandError::SameTarget(_))
    ));
}

#[test]
fn sibling_targets_merge_from_where_they_forked() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    repo.change_on_main(1, &[("keep", "k"), ("delete", "d")]);
    repo.land(1);
    let (a, b) = (TargetName::new("a").unwrap(), TargetName::new("b").unwrap());
    let queue = repo.queue();
    queue.create_target(&a, &TargetName::MAIN).unwrap();
    queue.create_target(&b, &TargetName::MAIN).unwrap();
    // `a` deletes a file, `b` adds one: neither ever landed the other.
    let on = |target| Base::Acceptance(repo.tip(target).acceptance);
    repo.revision(2, &[], on(&a), &[("keep", "k")]);
    queue
        .land_change(ChangeId::from_bytes([2; 16]), &a)
        .unwrap();
    repo.revision(
        3,
        &[],
        on(&b),
        &[("keep", "k"), ("delete", "d"), ("other", "o")],
    );
    queue
        .land_change(ChangeId::from_bytes([3; 16]), &b)
        .unwrap();
    assert!(matches!(
        queue.land_target(&a, &b).unwrap(),
        Landing::Landed { .. }
    ));
    assert_eq!(repo.state(&b), repo.files(&[("keep", "k"), ("other", "o")]));
}

#[test]
fn a_merge_that_changes_nothing_still_records_the_tip() {
    // `b` requires tests, which a landing changing nothing doesn't.
    let repo = Repo::new(
        "[targets.b]\nauthority = \"node:queue\"\n\
         require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"user:ci\" }]\n",
    );
    let queue = repo.queue();
    let annotations = Annotations::open(&repo.store).unwrap();
    // Results without inputs, which are never promoted to a merge.
    let pass = |subject| {
        let statement = Statement {
            subject,
            inputs: None,
            ..ci_test(RevisionId::from(EncodedObject::blob(b"").id()), "user:ci")
        };
        annotations.issue(&statement, None).unwrap();
    };
    let (a, b) = (TargetName::new("a").unwrap(), TargetName::new("b").unwrap());
    let land = |change: u8, target: &TargetName, files: &[(&str, &str)]| {
        let on = Base::Acceptance(repo.tip(target).acceptance);
        pass(RecordId::Revision(repo.revision(change, &[], on, files)));
        queue
            .land_change(ChangeId::from_bytes([change; 16]), target)
            .unwrap();
    };
    queue.create_target(&a, &TargetName::MAIN).unwrap();
    queue.create_target(&b, &TargetName::MAIN).unwrap();
    // Both add the same file: `a`'s tip lands on `b`, changing nothing.
    land(1, &a, &[("f", "f")]);
    land(2, &b, &[("f", "f")]);
    let state = repo.state(&b);
    assert!(matches!(
        queue.land_target(&a, &b).unwrap(),
        Landing::Landed { .. }
    ));
    assert_eq!(repo.state(&b), state);
    assert_eq!(
        queue.land_target(&a, &b).unwrap(),
        Landing::Unchanged(Proposal::Target(a.clone()))
    );

    // So once `a` deletes the file, landing it on `b` deletes it there.
    land(3, &a, &[]);
    let landing = queue.land_target(&a, &b).unwrap();
    let Landing::Pending(pending) = landing else {
        panic!("a deletion waits for its evidence: {landing:?}")
    };
    pass(pending.subject);
    assert!(matches!(
        queue.land_target(&a, &b).unwrap(),
        Landing::Landed { .. }
    ));
    assert_eq!(repo.state(&b), repo.files(&[]));

    // A target and the one it was made from, neither changing: landing
    // either on the other records nothing, however often.
    let c = TargetName::new("c").unwrap();
    queue.create_target(&c, &TargetName::MAIN).unwrap();
    for _ in 0..2 {
        for (from, onto) in [(&c, &TargetName::MAIN), (&TargetName::MAIN, &c)] {
            assert_eq!(
                queue.land_target(from, onto).unwrap(),
                Landing::Unchanged(Proposal::Target(from.clone()))
            );
        }
    }
}

#[test]
fn targets_merge_from_their_newest_shared_acceptance() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let queue = repo.queue();
    let [a, b, c, d] = ["a", "b", "c", "d"].map(|name| TargetName::new(name).unwrap());
    let land = |change: u8, target: &TargetName, files: &[(&str, &str)]| {
        let on = Base::Acceptance(repo.tip(target).acceptance);
        repo.revision(change, &[], on, files);
        queue
            .land_change(ChangeId::from_bytes([change; 16]), target)
            .unwrap();
    };
    // `c` and `d` fork from an empty `main`, `a` and `b` from the next
    // `main`, which adds a file.
    queue.create_target(&c, &TargetName::MAIN).unwrap();
    queue.create_target(&d, &TargetName::MAIN).unwrap();
    repo.change_on_main(1, &[("delete", "d")]);
    repo.land(1);
    queue.create_target(&a, &TargetName::MAIN).unwrap();
    queue.create_target(&b, &TargetName::MAIN).unwrap();
    // `a` deletes it, `b` keeps it, and both move on.
    for n in 0..4 {
        let counter = n.to_string();
        land(10 + n, &a, &[("a", &counter)]);
        land(20 + n, &b, &[("delete", "d"), ("b", &counter)]);
    }
    // Then each lands a side line: the empty `main` is nearer.
    land(30, &c, &[("c", "c")]);
    land(31, &d, &[("d", "d")]);
    assert!(matches!(
        queue.land_target(&c, &a).unwrap(),
        Landing::Landed { .. }
    ));
    assert!(matches!(
        queue.land_target(&d, &b).unwrap(),
        Landing::Landed { .. }
    ));

    assert!(matches!(
        queue.land_target(&a, &b).unwrap(),
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
    let [a, b] = ["a", "b"].map(|name| TargetName::new(name).unwrap());
    let land = |change: u8, target: &TargetName, files: &[(&str, &str)]| {
        let on = Base::Acceptance(repo.tip(target).acceptance);
        repo.revision(change, &[], on, files);
        queue
            .land_change(ChangeId::from_bytes([change; 16]), target)
            .unwrap();
    };
    queue.create_target(&a, &TargetName::MAIN).unwrap();
    queue.create_target(&b, &TargetName::MAIN).unwrap();
    land(1, &a, &[("a", "0"), ("shared", "a")]);
    land(2, &b, &[("b", "0"), ("shared", "b")]);
    // Each lands the other's tip at once, resolving `shared` its own
    // way: two merge bases, which disagree on it.
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
        repo.store.append(target, &merged).unwrap();
    }
    // Then each changes another file.
    land(3, &a, &[("a", "1"), ("b", "0"), ("shared", "a")]);
    land(4, &b, &[("a", "0"), ("b", "1"), ("shared", "b")]);

    let Landing::Pending(pending) = queue.land_target(&a, &b).unwrap() else {
        panic!("the bases disagree on `shared`, so it can't be merged blindly");
    };
    assert!(
        matches!(&pending.blocked, Blocked::Conflict { paths, .. } if paths.iter().map(RepoPath::as_str).eq(["shared"])),
        "{pending:?}"
    );
    // Once `a` takes `b`'s side, the disjoint edits merge.
    land(5, &a, &[("a", "1"), ("b", "0"), ("shared", "b")]);
    assert!(matches!(
        queue.land_target(&a, &b).unwrap(),
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
            .land_change(ChangeId::from_bytes([change; 16]), target)
            .unwrap();
    };
    let [a, b] = ["a", "b"].map(|name| TargetName::new(name).unwrap());
    queue.create_target(&a, &TargetName::MAIN).unwrap();
    queue.create_target(&b, &TargetName::MAIN).unwrap();
    land(1, &a, &[("p", "file")]);
    land(2, &b, &[("p/delete", "original")]);
    // Each lands the other's tip at once, both choosing the directory:
    // two merge bases, one with a file at `p`, the other a directory.
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
        repo.store.append(target, &merged).unwrap();
    }
    // `a` deletes `p/delete`, `b` keeps it and adds beside it.
    land(3, &a, &[("a", "new")]);
    land(4, &b, &[("p/delete", "original"), ("p/new", "new")]);

    // What `p` was is unknown, so the deletion can't be told from an
    // addition: it conflicts, rather than bring `p/delete` back.
    let Landing::Pending(pending) = queue.land_target(&a, &b).unwrap() else {
        panic!("the bases disagree on `p`, so it can't be merged blindly");
    };
    assert!(
        matches!(&pending.blocked, Blocked::Conflict { paths, .. } if paths.iter().map(RepoPath::as_str).eq(["p"])),
        "{pending:?}"
    );
}
