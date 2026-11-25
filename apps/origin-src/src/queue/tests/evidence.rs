//! Tests for the evidence a policy requires: results, promoted or not.

use origin_shared::{Statement, Verdict};

use super::*;

#[test]
fn missing_evidence_waits_and_promoted_results_count() {
    let repo = Repo::new(
        "[groups]\nrunners = [\"node:ci\"]\n[targets.main]\nauthority = \"node:queue\"\n\
         require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"group:runners\" }]\n",
    );
    let code = repo.change_on_main(1, &[("src/lib.rs", "fn a() {}")]);
    let docs = repo.change_on_main(2, &[("docs/guide.md", "# Guide")]);
    let waiting = repo.land(1);
    let [Landing::Pending(pending)] = &waiting[..] else {
        panic!("{waiting:?}")
    };
    assert!(matches!(&pending.blocked, Blocked::Evidence(missing) if missing.len() == 1));
    assert_eq!(
        Queue::pending(&repo.store).expect("`Queue::pending(&repo.store)` succeeds"),
        std::slice::from_ref(pending)
    );

    let annotations =
        Annotations::open(&repo.store).expect("`Annotations::open(&repo.store)` succeeds");
    annotations
        .issue(&ci_test(code, "user:mallory"), None)
        .expect("`issue()` succeeds");
    assert_eq!(repo.land(1), waiting);
    let result = annotations
        .issue(&ci_test(code, "node:ci"), None)
        .expect("`issue()` succeeds");
    let [landing] = &repo.land(1)[..] else {
        panic!()
    };
    let acceptance = repo
        .store
        .get(repo.tip(&TargetName::MAIN).acceptance)
        .expect("`get(repo.tip(&TargetName::MAIN).acceptance)` succeeds");
    assert_eq!(landed(landing), code);
    assert_eq!(acceptance.body().evidence, [result]);
    assert_eq!(
        Queue::pending(&repo.store).expect("`Queue::pending(&repo.store)` succeeds"),
        []
    );

    let [landing] = &repo.land(2)[..] else {
        panic!()
    };
    let rebased = landed(landing);
    assert_ne!(rebased, docs);
    let acceptance = repo
        .store
        .get(repo.tip(&TargetName::MAIN).acceptance)
        .expect("`get(repo.tip(&TargetName::MAIN).acceptance)` succeeds");
    let [evidence] = acceptance.body().evidence[..] else {
        panic!()
    };
    let promoted = repo.store.get(evidence).expect("`get(evidence)` succeeds");
    assert_eq!(promoted.subject, RecordId::Revision(rebased));
    assert_eq!(promoted.promoted_from, Some(result));
    assert_eq!(
        promoted.issuer,
        "node:queue".parse().expect("`parse()` succeeds")
    );
}

#[test]
fn results_from_another_target_never_count() {
    let require =
        "require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"group:runners\" }]\n";
    let repo = Repo::new(&format!(
        "[groups]\nrunners = [\"node:ci\"]\n[targets.main]\nauthority = \"node:queue\"\n\
         {require}[targets.release]\nauthority = \"node:queue\"\n{require}"
    ));
    let (main, overhaul) = (TargetName::MAIN, TargetName::new("overhaul").unwrap());
    let release = TargetName::new("release").unwrap();
    let queue = repo.queue();
    queue.create_target(&overhaul, &main).unwrap();
    queue.create_target(&release, &main).unwrap();
    let on_overhaul = Base::Acceptance(repo.tip(&overhaul).acceptance);
    let tested = repo.revision(2, &[], on_overhaul, &[("src/lib.rs", "fn a() {}")]);
    let change = ChangeId::from_bytes([2; 16]);
    queue
        .land_change(&mut repo.annotations(), change, &overhaul)
        .unwrap();
    let issue = |statement: &Statement| {
        Annotations::open(&repo.store)
            .unwrap()
            .issue(statement, None)
            .unwrap();
    };
    issue(&ci_test(tested, "node:ci"));
    let waits = |landing: &Landing| matches!(landing, Landing::Pending(pending) if matches!(pending.blocked, Blocked::Evidence(_)));

    let Landing::Pending(on_release) = queue
        .land_target(&mut repo.annotations(), &overhaul, &release)
        .unwrap()
    else {
        panic!("the overhaul's own result doesn't count on release")
    };
    issue(&Statement {
        subject: on_release.subject,
        ..ci_test(tested, "node:ci")
    });

    let landing = queue
        .land_target(&mut repo.annotations(), &overhaul, &main)
        .unwrap();
    assert!(waits(&landing), "{landing:?}");
}

#[test]
fn promotes_only_what_the_submitter_sees() {
    let repo = Repo::new(
        "[targets.main]\nauthority = \"node:queue\"\n\
         require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"node:ci\" }]\n",
    );
    let hidden = repo.change_on_main(1, &[("secrets/key", "hunter2")]);
    repo.annotations()
        .issue(&ci_test(hidden, "node:ci"), None)
        .unwrap();
    repo.change_on_main(2, &[("docs/guide.md", "# Guide")]);
    let secrets = RepoPath::new("secrets").unwrap();
    let blind = PathAccess::new(Level::Write, [(secrets, Level::None)]);
    let waiting = repo
        .queue()
        .submitted_by(
            Caller::Principal {
                principal: "user:eve".parse().unwrap(),
                scope: None,
            },
            blind,
        )
        .land_change(
            &mut repo.annotations(),
            ChangeId::from_bytes([2; 16]),
            &TargetName::MAIN,
        )
        .unwrap();
    assert!(matches!(waiting[..], [Landing::Pending(_)]), "{waiting:?}");
    assert!(matches!(repo.land(2)[..], [Landing::Landed { .. }]));
}

#[test]
fn forged_promotions_are_no_evidence() {
    let repo = requiring_ci();
    let failed = repo.change_on_main(1, &[("src/lib.rs", "bad")]);
    let mut original = ci_test(failed, "user:ci");
    original.verdict = Some(Verdict::Fail);
    let id = Annotations::open(&repo.store)
        .expect("`Annotations::open(&repo.store)` succeeds")
        .issue(&original, None)
        .expect("`issue(&original, None)` succeeds");
    let forged = Statement {
        verdict: Some(Verdict::Pass),
        issuer: "user:eve".parse().expect("`parse()` succeeds"),
        promoted_from: Some(id),
        ..original.clone()
    };
    let old = repo.change_on_main(3, &[("src/lib.rs", "bad")]);
    let docs = repo.change_on_main(2, &[("src/lib.rs", "bad"), ("docs", "d")]);
    let mut external = ci_test(old, "user:ci");
    external
        .inputs
        .as_mut()
        .expect("`as_mut()` yields a value")
        .external = true;
    let external_id: StatementId = external.encode().id().into();
    let reused = Statement {
        subject: RecordId::Revision(docs),
        issuer: "user:eve".parse().expect("`parse()` succeeds"),
        promoted_from: Some(external_id),
        ..external.clone()
    };
    repo.store
        .write_all(&[external.encode()])
        .expect("`write_all(&[external.encode()])` succeeds");
    repo.store
        .write_all(&[forged.encode(), reused.encode()])
        .expect("`write_all(&[forged.encode(), reused.encode()])` succeeds");
    assert!(matches!(repo.land(1)[..], [Landing::Pending(_)]));
    assert!(matches!(repo.land(2)[..], [Landing::Pending(_)]));
}

#[test]
fn replaced_results_are_promoted_again() {
    let repo = requiring_ci();
    let old = repo.change_on_main(1, &[("src/lib.rs", "code")]);
    let new = repo.change_on_main(2, &[("src/lib.rs", "code"), ("docs", "new")]);
    let original = ci_test(old, "user:ci");
    let annotations =
        Annotations::open(&repo.store).expect("`Annotations::open(&repo.store)` succeeds");
    let id = annotations
        .issue(&original, None)
        .expect("`issue(&original, None)` succeeds");
    Annotations::open(&repo.store)
        .unwrap()
        .promote(
            RecordId::Revision(new),
            &"node:queue".parse().unwrap(),
            |_| Ok(true),
        )
        .unwrap();
    let rerun = Statement {
        supersedes: Some(id),
        ..original
    };
    annotations
        .issue(&rerun, None)
        .expect("`issue(&rerun, None)` succeeds");
    let landings = repo.land(2);
    assert!(
        matches!(landings[..], [Landing::Landed { .. }]),
        "{landings:?}"
    );
}
