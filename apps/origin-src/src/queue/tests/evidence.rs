//! The evidence a policy requires: results, promoted or not.

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

    // A user outside the group doesn't count; the runner does.
    let annotations = Annotations::open(&repo.store).unwrap();
    annotations
        .issue(&ci_test(code, "user:mallory"), None)
        .unwrap();
    assert_eq!(repo.land(1), waiting);
    let result = annotations.issue(&ci_test(code, "node:ci"), None).unwrap();
    let [landing] = &repo.land(1)[..] else {
        panic!()
    };
    let acceptance = repo
        .store
        .get(repo.tip(&TargetName::MAIN).acceptance)
        .unwrap();
    assert_eq!(landed(landing), code);
    assert_eq!(acceptance.body().evidence, [result]);

    // The docs change is rebased onto `src` as tested, so the result is
    // promoted to it, and counts as the runner's.
    let [landing] = &repo.land(2)[..] else {
        panic!()
    };
    let rebased = landed(landing);
    assert_ne!(rebased, docs);
    let acceptance = repo
        .store
        .get(repo.tip(&TargetName::MAIN).acceptance)
        .unwrap();
    let [evidence] = acceptance.body().evidence[..] else {
        panic!()
    };
    let promoted = repo.store.get(evidence).unwrap();
    assert_eq!(promoted.subject, RecordId::Revision(rebased));
    assert_eq!(promoted.promoted_from, Some(result));
    assert_eq!(promoted.issuer, "node:queue".parse().unwrap());
}

#[test]
fn forged_promotions_are_no_evidence() {
    let repo = requiring_ci();
    let failed = repo.change_on_main(1, &[("src/lib.rs", "bad")]);
    let mut original = ci_test(failed, "user:ci");
    original.verdict = Some(Verdict::Fail);
    let id = Annotations::open(&repo.store)
        .unwrap()
        .issue(&original, None)
        .unwrap();
    // Eve wraps the trusted failure as a pass, written as a fetch would.
    let forged = Statement {
        verdict: Some(Verdict::Pass),
        issuer: "user:eve".parse().unwrap(),
        promoted_from: Some(id),
        ..original.clone()
    };
    // And carries a result that read the network to another change.
    let old = repo.change_on_main(3, &[("src/lib.rs", "bad")]);
    let docs = repo.change_on_main(2, &[("src/lib.rs", "bad"), ("docs", "d")]);
    let mut external = ci_test(old, "user:ci");
    external.inputs.as_mut().unwrap().external = true;
    let external_id: StatementId = external.encode().id().into();
    let reused = Statement {
        subject: RecordId::Revision(docs),
        issuer: "user:eve".parse().unwrap(),
        promoted_from: Some(external_id),
        ..external.clone()
    };
    repo.store.write_all(&[external.encode()]).unwrap();
    repo.store
        .write_all(&[forged.encode(), reused.encode()])
        .unwrap();
    assert!(matches!(repo.land(1)[..], [Landing::Pending(_)]));
    assert!(matches!(repo.land(2)[..], [Landing::Pending(_)]));
}

#[test]
fn replaced_results_are_promoted_again() {
    let repo = requiring_ci();
    let old = repo.change_on_main(1, &[("src/lib.rs", "code")]);
    let new = repo.change_on_main(2, &[("src/lib.rs", "code"), ("docs", "new")]);
    let original = ci_test(old, "user:ci");
    let annotations = Annotations::open(&repo.store).unwrap();
    let id = annotations.issue(&original, None).unwrap();
    Annotations::open(&repo.store)
        .unwrap()
        .promote(RecordId::Revision(new), &"node:queue".parse().unwrap())
        .unwrap();
    // The same passing result runs again, replacing the one promoted.
    let rerun = Statement {
        supersedes: Some(id),
        ..original
    };
    annotations.issue(&rerun, None).unwrap();
    let landings = repo.land(2);
    assert!(
        matches!(landings[..], [Landing::Landed { .. }]),
        "{landings:?}"
    );
}
