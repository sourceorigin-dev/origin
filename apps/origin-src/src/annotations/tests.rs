use std::sync::Barrier;
use std::thread;

use origin_shared::{
    AcceptanceBody, AttachmentId, Base, Digest, Inputs, NodeKey, RepoPath, Revision, RevisionId,
    TargetName, Verdict,
};

use super::*;
use crate::tree::tests::build;

pub(super) fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    (dir, store)
}

pub(super) fn principal(name: &str) -> Principal {
    name.parse().unwrap()
}

/// Writes a revision of `change` on `main` with `files`, evolving from
/// `predecessors`.
pub(super) fn revision(
    store: &Store,
    change: u8,
    predecessors: &[RevisionId],
    files: &[(&str, &str)],
) -> RevisionId {
    revision_on(store, &TargetName::MAIN, change, predecessors, files)
}

/// Writes a revision of `change` on `target`'s first acceptance with
/// `files`, evolving from `predecessors`.
pub(super) fn revision_on(
    store: &Store,
    target: &TargetName,
    change: u8,
    predecessors: &[RevisionId],
    files: &[(&str, &str)],
) -> RevisionId {
    revision_on_state(store, target, &[], change, predecessors, files)
}

/// Writes a revision like [`revision_on`], on a first acceptance whose
/// state holds `state`.
pub(super) fn revision_on_state(
    store: &Store,
    target: &TargetName,
    state: &[(&str, &str)],
    change: u8,
    predecessors: &[RevisionId],
    files: &[(&str, &str)],
) -> RevisionId {
    let acceptance = AcceptanceBody {
        target: target.id(),
        seq: 0,
        prev: None,
        state: build(store, state),
        landed: Vec::new(),
        evidence: Vec::new(),
        policy: None,
    }
    .sign(&NodeKey::from_bytes(&[1; 32]))
    .encode();
    let encoded = Revision {
        change: ChangeId::from_bytes([change; 16]),
        predecessors: predecessors.to_vec(),
        base: Base::Acceptance(acceptance.id().into()),
        deps: Vec::new(),
        snapshot: build(store, files),
        description: AttachmentId::from(EncodedObject::attachment(b"").id()),
        author: principal("user:alice"),
    }
    .encode();
    store.write_all(&[acceptance, encoded.clone()]).unwrap();
    encoded.id().into()
}

/// A check result about `revision` from `issuer`.
pub(super) fn check(
    revision: RevisionId,
    kind: &str,
    verdict: Verdict,
    issuer: &str,
    inputs: Option<(&[&str], bool)>,
) -> Statement {
    Statement {
        subject: RecordId::Revision(revision),
        kind: Kind::new(kind).unwrap(),
        verdict: Some(verdict),
        issuer: principal(issuer),
        inputs: inputs.map(|(paths, external)| Inputs {
            paths: paths
                .iter()
                .map(|path| RepoPath::new(path).unwrap())
                .collect(),
            target_paths: Vec::new(),
            definition: Digest::of(kind.as_bytes()),
            toolchain: Digest::of(b"rustc 1.99.0"),
            external,
        }),
        payload: None,
        supersedes: None,
        promoted_from: None,
    }
}

/// Issues `statement` through a fresh index.
pub(super) fn issue(store: &Store, statement: &Statement) -> StatementId {
    Annotations::open(store)
        .unwrap()
        .issue(statement, None)
        .unwrap()
}

/// The kinds and originals of what `promote` issues to `revision`.
pub(super) fn promote(store: &Store, revision: RevisionId) -> Vec<(String, Option<StatementId>)> {
    let promoter = principal("node:queue");
    Annotations::open(store)
        .unwrap()
        .promote(RecordId::Revision(revision), &promoter, |_| Ok(true))
        .unwrap()
        .into_iter()
        .map(|(_, statement)| {
            assert_eq!(statement.subject, RecordId::Revision(revision));
            assert_eq!(statement.issuer, promoter);
            (statement.kind.to_string(), statement.promoted_from)
        })
        .collect()
}

pub(super) const FILES: &[(&str, &str)] = &[("src/lib.rs", "v1"), ("docs/guide.md", "v1")];

#[test]
fn statements_issued_at_once_supersede_one_another() {
    let (_dir, store) = store();
    let revision = revision(&store, 1, &[], FILES);
    let rounds = 50;
    for _ in 0..rounds {
        // Both prepared before either thread starts, so a failure fails
        // the test rather than leaving a thread at the barrier. Different
        // verdicts, so the two never make one statement.
        let racers = [Verdict::Pass, Verdict::Fail].map(|verdict| {
            let annotations = Annotations::open(&store).unwrap();
            (
                annotations,
                check(revision, "review", verdict, "user:alice", None),
            )
        });
        let barrier = Barrier::new(2);
        thread::scope(|scope| {
            for (mut annotations, statement) in racers {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    annotations.supersede(statement, None).unwrap();
                });
            }
        });
    }
    let about = Annotations::open(&store)
        .unwrap()
        .about(RecordId::Revision(revision))
        .unwrap();
    assert_eq!(about.len(), 2 * rounds);
    for pair in about.windows(2) {
        assert_eq!(pair[1].statement.supersedes, Some(pair[0].id));
    }
}

#[test]
fn only_the_issuer_supersedes_its_results() {
    let (_dir, store) = store();
    let first = revision(&store, 1, &[], FILES);
    let inputs = Some((&["src"][..], false));
    let pass = issue(
        &store,
        &check(first, "lint", Verdict::Pass, "node:ci", inputs),
    );
    let target = revision(&store, 2, &[], FILES);

    // Another issuer's claim to supersede it doesn't count.
    let mut foreign = check(first, "lint", Verdict::Fail, "node:other", None);
    foreign.supersedes = Some(pass);
    let annotations = Annotations::open(&store).unwrap();
    assert!(matches!(
        annotations.issue(&foreign, None),
        Err(AnnotateError::ForeignSupersession { .. })
    ));
    let written = foreign.encode();
    store.write_all(std::slice::from_ref(&written)).unwrap();
    assert_eq!(
        Annotations::open(&store)
            .unwrap()
            .current(
                RecordId::Revision(first),
                &Kind::new("lint").unwrap(),
                &principal("node:ci")
            )
            .unwrap(),
        Some(pass),
        "a fetched one stands as a statement of its own"
    );

    // The issuer's own does: the rerun's failure is promoted instead.
    let mut rerun = check(first, "lint", Verdict::Fail, "node:ci", inputs);
    rerun.supersedes = Some(pass);
    let fail = issue(&store, &rerun);
    let about = Annotations::open(&store)
        .unwrap()
        .about(RecordId::Revision(first))
        .unwrap();
    assert_eq!(about[0].superseded_by, Some(fail));
    assert_eq!(promote(&store, target), [("lint".to_owned(), Some(fail))]);
}

#[test]
fn flags_each_divergence_once() {
    let (_dir, store) = store();
    let node = principal("node:laptop");
    let first = revision(&store, 1, &[], FILES);
    assert_eq!(
        Annotations::open(&store)
            .unwrap()
            .flag_divergence(ChangeId::from_bytes([1; 16]), node.clone())
            .unwrap(),
        None
    );
    let left = revision(&store, 1, &[first], &[("a", "left")]);
    let right = revision(&store, 1, &[first], &[("a", "right")]);
    let flag = || {
        Annotations::open(&store)
            .unwrap()
            .flag_divergence(ChangeId::from_bytes([1; 16]), node.clone())
            .unwrap()
            .unwrap()
    };
    let flagged = flag();
    assert_eq!(flag(), flagged, "the same heads, the same statement");

    let subject = RecordId::Revision(left.min(right));
    let about = Annotations::open(&store).unwrap().about(subject).unwrap();
    assert_eq!(about.len(), 1);
    assert_eq!(about[0].statement.kind, Kind::DIVERGENT);
    let heads = store
        .try_attachment(about[0].statement.payload.unwrap())
        .unwrap()
        .unwrap();
    let mut expected = [left, right];
    expected.sort();
    assert_eq!(
        heads,
        format!("{}\n{}\n", expected[0], expected[1]).into_bytes()
    );
}

#[test]
fn reserved_kinds_have_authors() {
    let (_dir, store) = store();
    let first = revision(&store, 1, &[], FILES);
    // The queue rebases it: its author is the queue, but alice created
    // the change.
    let rebased = Revision {
        predecessors: vec![first],
        author: principal("node:queue"),
        ..store.get(first).unwrap()
    }
    .encode();
    store.write_all(std::slice::from_ref(&rebased)).unwrap();
    let reserved = |kind: Kind, issuer: &str| Statement {
        subject: RecordId::Revision(rebased.id().into()),
        kind,
        verdict: None,
        issuer: principal(issuer),
        inputs: None,
        payload: None,
        supersedes: None,
        promoted_from: None,
    };
    let authorizes = |statement: &Statement| store.authorizes(statement).unwrap();
    assert!(authorizes(&reserved(Kind::ABANDON, "user:alice")));
    assert!(
        !authorizes(&reserved(Kind::ABANDON, "node:queue")),
        "it only rebased the change"
    );
    assert!(!authorizes(&reserved(Kind::ABANDON, "user:eve")));
    assert!(authorizes(&reserved(Kind::CONFLICT, "node:queue")));
    assert!(
        !authorizes(&reserved(Kind::CONFLICT, "user:alice")),
        "not her revision"
    );
    assert!(authorizes(&reserved(Kind::DIVERGENT, "node:laptop")));
    assert!(!authorizes(&reserved(Kind::DIVERGENT, "user:alice")));
    let other = reserved(Kind::new("core.other").unwrap(), "node:queue");
    assert!(!authorizes(&other));

    let annotations = Annotations::open(&store).unwrap();
    let foreign = reserved(Kind::ABANDON, "user:eve");
    assert!(matches!(
        annotations.issue(&foreign, None),
        Err(AnnotateError::Reserved { .. })
    ));
    let mut promotable = reserved(Kind::ABANDON, "user:alice");
    promotable.inputs = check(
        first,
        "x",
        Verdict::Pass,
        "node:ci",
        Some((&["src"], false)),
    )
    .inputs;
    assert!(!promotable.is_promotable(), "reserved kinds never promote");
}

#[test]
fn withdrawals_have_nothing_to_promote() {
    let (_dir, store) = store();
    let first = revision(&store, 1, &[], FILES);
    let inputs = Some((&["src"][..], false));
    let result = check(first, "ci.test", Verdict::Pass, "node:ci", inputs);
    assert!(result.is_promotable() && !result.withdraws());
    let withdrawal = Statement {
        verdict: None,
        supersedes: Some(result.encode().id().into()),
        ..result
    };
    assert!(withdrawal.withdraws());
    assert!(!withdrawal.is_promotable());
}

#[test]
fn only_the_creator_of_a_change_abandons_it() {
    let (_dir, store) = store();
    let first = revision(&store, 1, &[], FILES);
    // Eve takes the change over, then another root of it of hers
    // merges in: alice still created the change, and eve never did.
    let write = |predecessors: Vec<RevisionId>, author: &str| {
        let encoded = Revision {
            predecessors,
            author: principal(author),
            ..store.get(first).unwrap()
        }
        .encode();
        store.write_all(std::slice::from_ref(&encoded)).unwrap();
        RevisionId::from(encoded.id())
    };
    let taken = write(vec![first], "user:eve");
    let root = write(Vec::new(), "user:eve");
    let merged = write(vec![taken, root], "user:eve");
    let abandons = |subject, issuer: &str| {
        let statement = Statement {
            subject: RecordId::Revision(subject),
            kind: Kind::ABANDON,
            verdict: None,
            issuer: principal(issuer),
            inputs: None,
            payload: None,
            supersedes: None,
            promoted_from: None,
        };
        store.authorizes(&statement).unwrap()
    };
    assert!(abandons(taken, "user:alice"));
    assert!(!abandons(taken, "user:eve"), "she took it over");
    assert!(!abandons(merged, "user:eve"), "alice created it");
    assert!(!abandons(merged, "user:alice"), "eve created it too");
}
