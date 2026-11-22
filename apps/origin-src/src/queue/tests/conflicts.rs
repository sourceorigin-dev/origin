//! Landings that conflict, and what lets them through.

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
    let Blocked::Conflict { statement, paths } = &pending.blocked;
    assert_eq!(paths, &[RepoPath::new("shared").unwrap()]);
    let conflict = repo.store.get(*statement).unwrap();
    assert_eq!(conflict.kind, Kind::CONFLICT);
    assert_eq!(conflict.subject, RecordId::Revision(two));
    assert_eq!(repo.land(2), blocked, "retrying issues the same statement");

    // A new revision that takes `main`'s version merges.
    let tip = Base::Acceptance(repo.tip(&TargetName::MAIN).acceptance);
    repo.revision(2, &[two], tip, &[("shared", "one"), ("more", "m")]);
    assert!(matches!(repo.land(2)[..], [Landing::Landed { .. }]));
}

#[test]
fn unresolved_conflicts_never_land() {
    let repo = Repo::new("[targets.main]\nauthority = \"node:queue\"\n");
    let marked = "<<<<<<< a\n=======\n>>>>>>> b\n";
    let revision = repo.change_on_main(1, &[("file", marked)]);
    let path = RepoPath::new("file").unwrap();
    let conflicts = |content: &str| {
        Conflicts::new(vec![Conflicted {
            path: path.clone(),
            written: Some(EncodedObject::blob(content.as_bytes()).id()),
        }])
    };
    // From someone else, it says nothing about the revision's files.
    let foreign = conflicts(marked).statement(
        RecordId::Revision(revision),
        "user:eve".parse().unwrap(),
        None,
    );
    // From its author, it does.
    let (payload, statement) = conflicts(marked).statement(
        RecordId::Revision(revision),
        "user:ada".parse().unwrap(),
        None,
    );
    repo.store
        .write_all(&[foreign.0, foreign.1.encode()])
        .unwrap();
    let mut annotations = Annotations::open(&repo.store).unwrap();
    assert_eq!(
        annotations
            .claims(RecordId::Revision(revision))
            .unwrap()
            .len(),
        1
    );
    annotations.issue(&statement, Some(payload)).unwrap();
    annotations = Annotations::open(&repo.store).unwrap();
    assert_eq!(
        annotations
            .claims(RecordId::Revision(revision))
            .unwrap()
            .len(),
        2
    );
    let error = repo
        .queue()
        .land_change(ChangeId::from_bytes([1; 16]), &TargetName::MAIN)
        .unwrap_err();
    assert!(
        matches!(&error, LandError::Unresolved { paths, .. } if *paths == [path.clone()]),
        "{error}"
    );

    // A revision whose file is no longer what the conflict wrote there
    // is resolved, whatever the file holds.
    let edited = repo.change_on_main(2, &[("file", "Heading\n=======\n")]);
    let (payload, statement) = conflicts(marked).statement(
        RecordId::Revision(edited),
        "user:ada".parse().unwrap(),
        None,
    );
    annotations.issue(&statement, Some(payload)).unwrap();
    assert!(matches!(repo.land(2)[..], [Landing::Landed { .. }]));
}
