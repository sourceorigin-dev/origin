use origin_shared::{EncodedObject, Kind, Verdict};

use super::*;
use crate::annotations::AnnotateError;
use crate::annotations::tests::{FILES, check, issue, principal, promote, revision, store};
use crate::index::Index;

#[test]
fn promotes_results_whose_inputs_are_unchanged() {
    let (_dir, store) = store();
    let first = revision(&store, 1, &[], FILES);
    let lint = issue(
        &store,
        &check(
            first,
            "lint",
            Verdict::Pass,
            "node:ci",
            Some((&["src"], false)),
        ),
    );
    let docs = check(
        first,
        "docs",
        Verdict::Pass,
        "node:ci",
        Some((&["src", "docs"], false)),
    );
    issue(&store, &docs);
    let links = check(
        first,
        "links",
        Verdict::Pass,
        "node:ci",
        Some((&["docs"], true)),
    );
    issue(&store, &links);
    issue(
        &store,
        &check(first, "review", Verdict::Pass, "user:bob", None),
    );

    // A docs-only change keeps lint, and nothing that reads docs.
    let docs_only = revision(
        &store,
        2,
        &[],
        &[("src/lib.rs", "v1"), ("docs/guide.md", "v2")],
    );
    assert_eq!(
        promote(&store, docs_only),
        [("lint".to_owned(), Some(lint))]
    );
    assert_eq!(promote(&store, docs_only), [], "promoted once");

    // An unchanged snapshot keeps every promotable result, never the
    // external one or the review.
    let same = revision(&store, 3, &[], FILES);
    let kinds: Vec<_> = promote(&store, same)
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, ["docs", "lint"]);
    assert_eq!(promote(&store, first), [], "it has its own results");

    let source = revision(
        &store,
        4,
        &[],
        &[("src/lib.rs", "v2"), ("docs/guide.md", "v1")],
    );
    assert_eq!(promote(&store, source), []);

    // Promoted results aren't promoted again: their originals are.
    let again = revision(&store, 5, &[], FILES);
    let originals: Vec<_> = promote(&store, again).into_iter().map(|(_, o)| o).collect();
    assert!(originals.contains(&Some(lint)), "{originals:?}");
}

#[test]
fn direct_results_win_over_promotion() {
    let (_dir, store) = store();
    let inputs = Some((&["src"][..], false));
    let first = revision(&store, 1, &[], FILES);
    issue(
        &store,
        &check(first, "lint", Verdict::Pass, "node:ci", inputs),
    );
    let second = revision(&store, 2, &[], FILES);
    issue(
        &store,
        &check(second, "lint", Verdict::Fail, "node:ci", inputs),
    );
    assert_eq!(promote(&store, second), [], "it ran there, and failed");

    // Of several matching results, the latest is promoted.
    let third = revision(&store, 3, &[], FILES);
    let promoted = promote(&store, third);
    let second_result = Annotations::open(&store)
        .unwrap()
        .current(
            RecordId::Revision(second),
            &Kind::new("lint").unwrap(),
            &principal("node:ci"),
        )
        .unwrap();
    assert_eq!(promoted, [("lint".to_owned(), second_result)]);
}

#[test]
fn results_there_replace_promoted_ones_whatever_came_first() {
    let (_dir, store) = store();
    let inputs = Some((&["src"][..], false));
    let first = revision(&store, 1, &[], FILES);
    issue(
        &store,
        &check(first, "lint", Verdict::Pass, "node:ci", inputs),
    );
    let verdicts = |revision| -> Vec<_> {
        let annotations = Annotations::open(&store).unwrap();
        let claims = annotations.claims(RecordId::Revision(revision)).unwrap();
        claims
            .iter()
            .map(|claim| (claim.original().is_some(), claim.claimed().verdict))
            .collect()
    };

    // A failure there, after a promoted pass.
    let second = revision(&store, 2, &[], FILES);
    assert_eq!(promote(&store, second).len(), 1);
    issue(
        &store,
        &check(second, "lint", Verdict::Fail, "node:ci", inputs),
    );
    assert_eq!(verdicts(second), [(false, Some(Verdict::Fail))]);

    // A pass there, then taken back, after a promoted pass.
    let third = revision(&store, 3, &[], FILES);
    assert_eq!(promote(&store, third).len(), 1);
    let pass = check(third, "lint", Verdict::Pass, "node:ci", inputs);
    let id = issue(&store, &pass);
    let withdrawal = Statement {
        verdict: None,
        inputs: None,
        supersedes: Some(id),
        ..pass
    };
    issue(&store, &withdrawal);
    assert_eq!(verdicts(third), [(false, None)]);
}

#[test]
fn digests_are_remembered_across_rebuilds() {
    let (_dir, store) = store();
    let first = revision(&store, 1, &[], FILES);
    let lint = issue(
        &store,
        &check(
            first,
            "lint",
            Verdict::Pass,
            "node:ci",
            Some((&["src"], false)),
        ),
    );
    promote(&store, revision(&store, 2, &[], FILES));

    let index = Index::open(&store).unwrap();
    assert!(index.digest(lint).is_some());
    assert_eq!(Index::rebuild(&store).unwrap().index, index);
}

#[test]
fn promotions_must_match_their_original() {
    let (_dir, store) = store();
    let inputs = Some((&["src"][..], false));
    let first = revision(&store, 1, &[], FILES);
    let mut failed = check(first, "lint", Verdict::Fail, "node:ci", inputs);
    let failure = issue(&store, &failed);
    let second = revision(&store, 2, &[], FILES);
    let promotion = |original: StatementId, statement: &Statement| Statement {
        subject: RecordId::Revision(second),
        issuer: principal("user:eve"),
        promoted_from: Some(original),
        ..statement.clone()
    };
    let annotations = Annotations::open(&store).unwrap();
    let refused = |statement: &Statement| match annotations.issue(statement, None) {
        Err(AnnotateError::InvalidPromotion(why)) => why,
        other => panic!("{other:?}"),
    };

    // Turning a failure into a pass.
    let forged = Statement {
        verdict: Some(Verdict::Pass),
        ..promotion(failure, &failed)
    };
    assert_eq!(refused(&forged), InvalidPromotion::Differs("verdict"));
    // Carrying it where the inputs differ.
    let changed = revision(&store, 3, &[], &[("src/lib.rs", "v2")]);
    let moved = Statement {
        subject: RecordId::Revision(changed),
        ..promotion(failure, &failed)
    };
    assert_eq!(refused(&moved), InvalidPromotion::InputsDiffer);
    // Carrying a result that read the network.
    failed.inputs.as_mut().unwrap().external = true;
    let external = issue(&store, &failed);
    assert_eq!(
        refused(&promotion(external, &failed)),
        InvalidPromotion::NotPromotable(external)
    );
    let unknown = EncodedObject::blob(b"nothing").id().into();
    assert_eq!(
        refused(&promotion(unknown, &failed)),
        InvalidPromotion::UnknownOriginal(unknown)
    );

    // Written anyway, as a fetch could, none of them claims anything,
    // and a valid one claims as the original's issuer.
    let valid = promotion(
        failure,
        &check(first, "lint", Verdict::Fail, "node:ci", inputs),
    );
    let records: Vec<_> = [forged, moved, valid]
        .iter()
        .map(Statement::encode)
        .collect();
    store.write_all(&records).unwrap();
    let claims = |subject| Annotations::open(&store).unwrap().claims(subject).unwrap();
    assert_eq!(claims(RecordId::Revision(changed)), []);
    let [claim] = &claims(RecordId::Revision(second))[..] else {
        panic!("only the valid promotion claims")
    };
    assert_eq!(claim.original(), Some(failure));
    assert_eq!(claim.claimed().issuer, principal("node:ci"));
    assert_eq!(claim.claimed().verdict, Some(Verdict::Fail));
}

#[test]
fn superseded_promotions_hold_nothing() {
    let (_dir, store) = store();
    let inputs = Some((&["src"][..], false));
    let first = revision(&store, 1, &[], FILES);
    let original = check(first, "lint", Verdict::Pass, "node:ci", inputs);
    let id = issue(&store, &original);
    let second = revision(&store, 2, &[], FILES);
    assert_eq!(promote(&store, second), [("lint".to_owned(), Some(id))]);

    // The rerun replaces the original, so the promotion of it no longer
    // holds, and the rerun is promoted in its place.
    let rerun = issue(
        &store,
        &Statement {
            supersedes: Some(id),
            ..original
        },
    );
    let annotations = Annotations::open(&store).unwrap();
    assert_eq!(annotations.claims(RecordId::Revision(second)).unwrap(), []);
    let about = annotations.about(RecordId::Revision(second)).unwrap();
    assert!(
        about.iter().all(|annotation| !annotation.holds),
        "{about:?}"
    );
    assert_eq!(promote(&store, second), [("lint".to_owned(), Some(rerun))]);
}
