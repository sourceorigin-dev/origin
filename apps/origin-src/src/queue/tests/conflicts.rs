//! Landings that conflict, and what lets them through.

use super::*;

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
