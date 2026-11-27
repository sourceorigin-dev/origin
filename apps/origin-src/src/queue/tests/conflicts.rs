//! Tests for landings that conflict, and for what lets them through.

use super::*;
use crate::conflict::Conflicted;

#[test]
fn conflicts_block_until_a_revision_merges() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    repo.change_on_main(1, &[("shared", "one")]);
    let two = repo.change_on_main(2, &[("shared", "two")]);
    repo.land(1);

    let blocked = repo.land(2);
    let [Landing::Pending(pending)] = &blocked[..] else {
        panic!("{blocked:?}")
    };
    assert_eq!(pending.subject, RecordId::Revision(two));
    let Blocked::Conflict { statement, paths } = &pending.blocked else {
        panic!("{pending:?}")
    };
    assert_eq!(
        paths,
        &[RepoPath::new("shared").expect("`RepoPath::new()` succeeds")]
    );
    let conflict = repo
        .store
        .get(*statement)
        .expect("`get(*statement)` succeeds");
    assert_eq!(conflict.kind, Kind::CONFLICT);
    assert_eq!(conflict.subject, RecordId::Revision(two));
    assert_eq!(repo.land(2), blocked, "retrying issues the same statement");
    assert_eq!(
        Queue::pending(&repo.store).expect("`Queue::pending(&repo.store)` succeeds"),
        std::slice::from_ref(pending)
    );

    let tip = Base::Acceptance(repo.tip(&TargetName::MAIN).acceptance);
    repo.revision(2, &[two], tip, &[("shared", "one"), ("more", "m")]);
    assert!(matches!(repo.land(2)[..], [Landing::Landed { .. }]));
    assert_eq!(
        Queue::pending(&repo.store).expect("`Queue::pending(&repo.store)` succeeds"),
        []
    );
}

#[test]
fn unresolved_conflicts_never_land() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let marked = "<<<<<<< a\n=======\n>>>>>>> b\n";
    let revision = repo.change_on_main(1, &[("file", marked)]);
    let path = RepoPath::new("file").expect("`RepoPath::new()` succeeds");
    let conflicts = |content: &str| {
        Conflicts::new(vec![Conflicted {
            path: path.clone(),
            written: Some(EncodedObject::blob(content.as_bytes()).id()),
        }])
    };
    let foreign = conflicts(marked).statement(
        RecordId::Revision(revision),
        "user:eve".parse().expect("`parse()` succeeds"),
        None,
    );
    let (payload, statement) = conflicts(marked).statement(
        RecordId::Revision(revision),
        "user:ada".parse().expect("`parse()` succeeds"),
        None,
    );
    repo.store
        .write_all(&[foreign.0, foreign.1.encode()])
        .expect("`write_all(&[foreign.0, foreign.1.encode()])` succeeds");
    let mut annotations =
        Annotations::open(&repo.store).expect("`Annotations::open(&repo.store)` succeeds");
    assert_eq!(
        annotations
            .claims(RecordId::Revision(revision))
            .expect("`claims(RecordId::Revision(revision))` succeeds")
            .len(),
        1
    );
    annotations
        .issue(&statement, Some(payload))
        .expect("`issue(&statement, Some(payload))` succeeds");
    annotations =
        Annotations::open(&repo.store).expect("`Annotations::open(&repo.store)` succeeds");
    assert_eq!(
        annotations
            .claims(RecordId::Revision(revision))
            .expect("`claims(RecordId::Revision(revision))` succeeds")
            .len(),
        2
    );
    let error = repo
        .queue()
        .land_change(
            &mut repo.annotations(),
            ChangeId::from_bytes([1; 16]),
            &TargetName::MAIN,
        )
        .expect_err("the operation fails");
    assert!(
        matches!(&error, LandError::Unresolved { paths, .. } if *paths == [path.clone()]),
        "{error}"
    );

    let edited = repo.change_on_main(2, &[("file", "Heading\n=======\n")]);
    let (payload, statement) = conflicts(marked).statement(
        RecordId::Revision(edited),
        "user:ada".parse().expect("`parse()` succeeds"),
        None,
    );
    annotations
        .issue(&statement, Some(payload))
        .expect("`issue(&statement, Some(payload))` succeeds");
    assert!(matches!(repo.land(2)[..], [Landing::Landed { .. }]));
}
