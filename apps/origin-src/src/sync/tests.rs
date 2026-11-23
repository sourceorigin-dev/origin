use origin_platform::Native;
use origin_shared::event::Filter;
use origin_shared::{AcceptanceBody, Kind, NodeKey, Verdict};

use super::*;
use crate::events::Events;
use crate::queue::tests::{Repo, ci_test};
use crate::service::{Node, OriginService};
use crate::tree::Snapshots;
use crate::tree::tests::build;

const MAIN_RULES: &str = "[targets.main]\nauthority = \"node:queue\"\n";

/// A node's own empty repository, syncing in-process with the queue's
/// repository of `server`.
struct Client {
    _dir: tempfile::TempDir,
    store: Store,
    remote: Remote,
}

impl Client {
    fn new(server: &Repo) -> Self {
        Self::answered_in_parts(server, Pack::LIMIT)
    }

    /// A client of a server that answers in parts of `limit` bytes.
    fn answered_in_parts(server: &Repo, limit: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let node = Node::new("queue".parse().unwrap(), NodeKey::from_bytes(&[7; 32]));
        let service = OriginService::new(server.store.clone(), node).with_pack_limit(limit);
        let remote = Remote::Local(Box::new(service));
        Self {
            _dir: dir,
            store,
            remote,
        }
    }

    fn sync(&self) -> Sync<'_> {
        Sync::new(
            &self.store,
            &self.remote,
            "laptop".parse().unwrap(),
            "user:ada".parse().unwrap(),
        )
    }

    /// Writes a revision of `change` with `files` on `base`, with every object
    /// it needs.
    fn revision(
        &self,
        change: u8,
        predecessors: &[RevisionId],
        base: Base,
        files: &[(&str, &str)],
    ) -> RevisionId {
        let mut objects: Vec<_> = files
            .iter()
            .map(|(_, content)| EncodedObject::blob(content.as_bytes()))
            .collect();
        let description = EncodedObject::attachment(b"work");
        let revision = Revision {
            change: ChangeId::from_bytes([change; 16]),
            predecessors: predecessors.to_vec(),
            base,
            deps: Vec::new(),
            snapshot: build(&self.store, files),
            description: description.id().into(),
            author: "user:ada".parse().unwrap(),
        }
        .encode();
        objects.extend([description, revision.clone()]);
        self.store.write_all(&objects).unwrap();
        revision.id().into()
    }
}

fn main_tip(store: &Store) -> Base {
    Base::Acceptance(store.head(&TargetName::MAIN).unwrap().unwrap().acceptance)
}

/// Writes to `store` a snapshot of a `policy.toml` that makes `key` the key
/// of `node:queue`, the authority of `policy`.
fn policy_snapshot(store: &Store, key: &NodeKey) -> TreeId {
    let policy = format!(
        "[nodes]\nqueue = \"{}\"\n[targets.policy]\nauthority = \"node:queue\"\n{MAIN_RULES}",
        key.public()
    );
    let (file, tree) = Policy::snapshot(&policy);
    store.write_all(&[file, tree.clone()]).unwrap();
    tree.id().into()
}

#[test]
fn fetches_verified_chains_and_what_they_landed_but_not_snapshots() {
    let server = Repo::new(MAIN_RULES);
    let landed = server.change_on_main(1, &[("a", "a")]);
    let result = server
        .annotations()
        .issue(&ci_test(landed, "node:ci"), None)
        .unwrap();
    server.land(1);
    let client = Client::new(&server);

    let advanced = client.sync().fetch_chains(&[]).unwrap();
    assert_eq!(advanced.len(), 1, "`policy` is always followed");
    let advanced = client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
    assert_eq!(advanced[0].to, server.tip(&TargetName::MAIN));
    assert_eq!(
        client.store.log(&TargetName::MAIN).unwrap(),
        server.store.log(&TargetName::MAIN).unwrap()
    );
    assert!(
        client.store.contains(landed.into()).unwrap(),
        "what landed comes"
    );
    assert!(
        client.store.contains(result.into()).unwrap(),
        "with the records of its change"
    );
    assert!(Policy::in_force(&client.store).is_ok(), "the policy too");

    let state = client
        .store
        .get(server.tip(&TargetName::MAIN).acceptance)
        .unwrap()
        .body()
        .state;
    assert!(
        !client.store.contains(state.into()).unwrap(),
        "snapshots are lazy"
    );
    let tip = RecordId::Acceptance(server.tip(&TargetName::MAIN).acceptance);
    assert!(client.sync().need(tip, &[]).unwrap());
    assert!(!client.sync().need(tip, &[]).unwrap(), "once");
    assert_eq!(client.store.diff(state, state).unwrap(), []);
    assert_eq!(client.sync().fetch_chains(&[]).unwrap(), []);
}

#[test]
fn acceptances_that_do_not_verify_are_refused() {
    let server = Repo::new(MAIN_RULES);
    let tip = server.tip(&TargetName::MAIN);
    let state = server.store.get(tip.acceptance).unwrap().body().state;
    let forged = AcceptanceBody {
        target: TargetName::MAIN.id(),
        seq: 1,
        prev: Some(tip.acceptance),
        state,
        landed: Vec::new(),
        evidence: Vec::new(),
        policy: Some(server.tip(&TargetName::POLICY).acceptance),
    }
    .sign(&NodeKey::from_bytes(&[8; 32]));
    server.store.append(&TargetName::MAIN, &forged).unwrap();
    let client = Client::new(&server);

    let error = client.sync().fetch_chains(&[TargetName::MAIN]).unwrap_err();
    assert!(
        matches!(
            &error,
            SyncError::Unverified {
                seq: 1,
                why: Unverified::Signature(_),
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(
        client.store.head(&TargetName::MAIN).unwrap(),
        None,
        "nothing of the chain was appended"
    );
}

#[test]
fn only_the_chain_of_policy_governs() {
    let server = Repo::new(MAIN_RULES);
    let client = Client::new(&server);
    client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
    let attacker = NodeKey::from_bytes(&[8; 32]);
    let fake = AcceptanceBody {
        target: TargetName::POLICY.id(),
        seq: 40,
        prev: None,
        state: policy_snapshot(&server.store, &attacker),
        landed: Vec::new(),
        evidence: Vec::new(),
        policy: None,
    }
    .sign(&attacker);
    let fake_id: AcceptanceId = fake.encode().id().into();
    server.store.write_all(&[fake.encode()]).unwrap();
    server.revision(9, &[], Base::Acceptance(fake_id), &[("a", "a")]);
    client
        .sync()
        .fetch_changes(&[ChangeId::from_bytes([9; 16])])
        .unwrap();
    assert!(client.store.contains(fake_id.into()).unwrap());

    let tip = server.tip(&TargetName::MAIN);
    let forged = AcceptanceBody {
        target: TargetName::MAIN.id(),
        seq: tip.seq + 1,
        prev: Some(tip.acceptance),
        state: server.store.get(tip.acceptance).unwrap().body().state,
        landed: Vec::new(),
        evidence: Vec::new(),
        policy: Some(fake_id),
    }
    .sign(&attacker);
    server.store.append(&TargetName::MAIN, &forged).unwrap();
    let error = client.sync().fetch_chains(&[]).unwrap_err();
    assert!(
        matches!(
            error,
            SyncError::Unverified { why: Unverified::OffChain(id), .. } if id == fake_id
        ),
        "{error}"
    );
    assert_eq!(client.store.head(&TargetName::MAIN).unwrap(), Some(tip));
}

#[test]
fn each_acceptance_of_policy_is_governed_by_the_one_before() {
    let extended_by_the_old_key = |names_genesis: bool| {
        let server = Repo::new(MAIN_RULES);
        let client = Client::new(&server);
        client.sync().fetch_chains(&[]).unwrap();
        let genesis = server.tip(&TargetName::POLICY).acceptance;
        let old = NodeKey::from_bytes(&[7; 32]);
        let rotation = AcceptanceBody {
            target: TargetName::POLICY.id(),
            seq: 1,
            prev: Some(genesis),
            state: policy_snapshot(&server.store, &NodeKey::from_bytes(&[8; 32])),
            landed: Vec::new(),
            evidence: Vec::new(),
            policy: Some(genesis),
        }
        .sign(&old);
        server.store.append(&TargetName::POLICY, &rotation).unwrap();
        client.sync().fetch_chains(&[]).unwrap();
        let rotation: AcceptanceId = rotation.encode().id().into();
        let reverted = AcceptanceBody {
            target: TargetName::POLICY.id(),
            seq: 2,
            prev: Some(rotation),
            state: server.store.get(genesis).unwrap().body().state,
            landed: Vec::new(),
            evidence: Vec::new(),
            policy: Some(if names_genesis { genesis } else { rotation }),
        }
        .sign(&old);
        server.store.append(&TargetName::POLICY, &reverted).unwrap();
        client.sync().fetch_chains(&[]).unwrap_err()
    };
    let error = extended_by_the_old_key(true);
    assert!(
        matches!(
            error,
            SyncError::Unverified {
                seq: 2,
                why: Unverified::Policy(PolicyError::NotPredecessor(2)),
                ..
            }
        ),
        "{error}"
    );
    let error = extended_by_the_old_key(false);
    assert!(
        matches!(
            error,
            SyncError::Unverified {
                seq: 2,
                why: Unverified::Signature(_),
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn acceptances_fetched_before_their_chain_advance_it_once_appended() {
    let server = Repo::new(MAIN_RULES);
    server.change_on_main(1, &[("a", "a")]);
    server.land(1);
    let release = TargetName::new("release").unwrap();
    server
        .queue()
        .create_target(&release, &TargetName::MAIN)
        .unwrap();
    let main_tip = server.tip(&TargetName::MAIN).acceptance;
    let client = Client::new(&server);
    let events = Events::new(&client.store);

    client.sync().fetch_chains(&[release]).unwrap();
    assert!(client.store.contains(main_tip.into()).unwrap());
    let advanced = |cursor| {
        let page = events
            .after(cursor, &Filter::default(), usize::MAX)
            .unwrap();
        let main: Vec<u64> = page
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::TargetAdvanced { target, seq, .. } if *target == TargetName::MAIN => {
                    Some(*seq)
                }
                _ => None,
            })
            .collect();
        (main, page.next)
    };
    let (main, cursor) = advanced(0);
    assert!(main.is_empty(), "not followed, so nothing advanced");

    client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
    assert_eq!(
        advanced(cursor).0,
        [0, 1],
        "the acceptance held before advances `main` too, once appended"
    );
}

#[test]
fn fetches_the_records_of_changes() {
    let server = Repo::new(MAIN_RULES);
    let first = server.change_on_main(1, &[("a", "a")]);
    let annotations = server.annotations();
    annotations.issue(&ci_test(first, "node:ci"), None).unwrap();
    let client = Client::new(&server);
    client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();

    let fetched = client
        .sync()
        .fetch_changes(&[ChangeId::from_bytes([1; 16])])
        .unwrap();
    assert_eq!(
        fetched,
        Fetched {
            revisions: 1,
            statements: 1
        }
    );
    assert!(client.store.contains(first.into()).unwrap());
    let again = client
        .sync()
        .fetch_changes(&[ChangeId::from_bytes([1; 16])])
        .unwrap();
    assert_eq!(again, Fetched::default());
}

#[test]
fn pushes_revisions_snapshots_and_statements() {
    let server = Repo::new(MAIN_RULES);
    let client = Client::new(&server);
    client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
    let base = main_tip(&client.store);
    let first = client.revision(1, &[], base, &[("a", "1"), ("b", "1")]);
    let second = client.revision(1, &[first], base, &[("a", "2"), ("b", "1")]);
    let statement = ci_test(second, "user:ada").encode();
    client
        .store
        .write_all(std::slice::from_ref(&statement))
        .unwrap();

    let change = ChangeId::from_bytes([1; 16]);
    let pushed = client.sync().push(&[change]).unwrap();
    assert_eq!((pushed.revisions, pushed.statements), (2, 1), "{pushed:?}");
    assert_eq!(
        pushed.objects,
        2 + 1 + 1 + 2 + 2,
        "the revisions, their description, the statement, two trees and two blobs"
    );
    for id in [first.into(), second.into(), statement.id()] {
        assert!(server.store.contains(id).unwrap());
    }
    assert_eq!(client.sync().push(&[change]).unwrap(), Pushed::default());
}

#[test]
fn pushes_statements_after_those_they_need() {
    let server = Repo::new(MAIN_RULES);
    let client = Client::new(&server);
    client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
    let base = main_tip(&client.store);
    let first = client.revision(1, &[], base, &[("a", "1")]);
    let second = client.revision(1, &[first], base, &[("a", "2")]);
    let pass = ci_test(first, "user:ada");
    let fail = Statement {
        subject: RecordId::Revision(second),
        verdict: Some(Verdict::Fail),
        supersedes: Some(pass.encode().id().into()),
        ..pass.clone()
    };
    client
        .store
        .write_all(&[fail.encode(), pass.encode()])
        .unwrap();

    let change = ChangeId::from_bytes([1; 16]);
    let pushed = client.sync().push(&[change]).unwrap();
    assert_eq!((pushed.statements, &pushed.unsent[..]), (2, &[][..]));
    for statement in [&pass, &fail] {
        assert!(server.store.contains(statement.encode().id()).unwrap());
    }

    let other = client.revision(2, &[], base, &[("b", "1")]);
    let old = ci_test(other, "user:ada");
    let newer = Statement {
        supersedes: Some(old.encode().id().into()),
        ..fail.clone()
    };
    client
        .store
        .write_all(&[old.encode(), newer.encode()])
        .unwrap();
    let pushed = client.sync().push(&[change]).unwrap();
    assert_eq!(pushed.statements, 0);
    assert_eq!(pushed.unsent, [StatementId::from(newer.encode().id())]);

    client
        .sync()
        .push(&[ChangeId::from_bytes([2; 16])])
        .unwrap();
    let pushed = client.sync().push(&[change]).unwrap();
    assert_eq!((pushed.statements, &pushed.unsent[..]), (1, &[][..]));
    assert!(server.store.contains(newer.encode().id()).unwrap());
}

#[test]
fn a_push_never_sends_a_result_without_its_replacement() {
    let server = Repo::new(MAIN_RULES);
    let client = Client::new(&server);
    client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
    let base = main_tip(&client.store);
    let revision = client.revision(1, &[], base, &[("a", "1")]);
    let pass = ci_test(revision, "user:ada");
    let other = client.revision(2, &[], base, &[("b", "1")]);
    let original = Statement {
        verdict: Some(Verdict::Fail),
        ..ci_test(other, "user:ada")
    };
    let fail = Statement {
        subject: RecordId::Revision(revision),
        supersedes: Some(pass.encode().id().into()),
        promoted_from: Some(original.encode().id().into()),
        ..original.clone()
    };
    client
        .store
        .write_all(&[pass.encode(), original.encode(), fail.encode()])
        .unwrap();

    let change = ChangeId::from_bytes([1; 16]);
    let pushed = client.sync().push(&[change]).unwrap();
    assert_eq!(pushed.statements, 0, "{pushed:?}");
    let mut unsent = pushed.unsent;
    unsent.sort_unstable();
    let mut held: Vec<StatementId> = [&fail, &pass]
        .map(|statement| statement.encode().id().into())
        .into();
    held.sort_unstable();
    assert_eq!(unsent, held);
    assert!(!server.store.contains(pass.encode().id()).unwrap());
}

#[test]
fn transfers_go_in_parts_in_dependency_order() {
    let server = Repo::new(MAIN_RULES);
    let limit = 200;
    let client = Client::answered_in_parts(&server, limit);
    client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
    let base = main_tip(&client.store);
    let files: Vec<(String, String)> = (0..20)
        .map(|i| (format!("src/{}/{i}", i % 3), format!("content {i}")))
        .collect();
    let files: Vec<_> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let first = client.revision(1, &[], base, &files);
    let second = client.revision(1, &[first], base, &files[1..]);
    let statement = ci_test(second, "user:ada").encode();
    client
        .store
        .write_all(std::slice::from_ref(&statement))
        .unwrap();
    let change = ChangeId::from_bytes([1; 16]);
    let pushed = client
        .sync()
        .with_pack_limit(limit)
        .push(&[change])
        .unwrap();
    assert_eq!((pushed.revisions, pushed.statements), (2, 1));

    let other = Client::answered_in_parts(&server, limit);
    other.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
    let fetched = other.sync().fetch_changes(&[change]).unwrap();
    assert_eq!(
        fetched,
        Fetched {
            revisions: 2,
            statements: 1
        }
    );
    let record = RecordId::Revision(first);
    let request = FetchSnapshot {
        of: record,
        have: Vec::new(),
        paths: Vec::new(),
        skip: 0,
    };
    let part = other.remote.snapshot(&request).unwrap();
    assert!(part.more && part.pack.objects.len() < 10, "{part:?}");
    assert!(other.sync().need(record, &[]).unwrap());
    let snapshot = other.store.snapshot_of(record).unwrap().unwrap();
    let empty = build(&other.store, &[]);
    assert_eq!(other.store.diff(empty, snapshot).unwrap().len(), 20);
}

#[test]
fn fetched_revisions_are_written_after_what_they_need() {
    let server = Repo::new(MAIN_RULES);
    let first = server.change_on_main(1, &[("a", "a")]);
    let revision = server.store.read(first.into()).unwrap();
    let records = [
        revision,
        ci_test(first, "node:ci").encode(),
        EncodedObject::blob(b"x"),
    ];
    let layers = Sync::layers(records.to_vec())
        .map(|layer| layer.iter().map(EncodedObject::id).collect::<Vec<_>>());
    let [revision, statement, needed] = records.map(|record| vec![record.id()]);
    assert_eq!(layers, [needed, statement, revision]);
}

#[test]
fn interrupted_snapshot_fetches_are_fetched_again() {
    let server = Repo::new(MAIN_RULES);
    let revision = server.change_on_main(1, &[("src/a", "a")]);
    let client = Client::new(&server);
    client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
    client
        .sync()
        .fetch_changes(&[ChangeId::from_bytes([1; 16])])
        .unwrap();
    let record = RecordId::Revision(revision);
    let snapshot = client.store.snapshot_of(record).unwrap().unwrap();
    let empty = build(&client.store, &[]);

    let batch = client.store.batch();
    let fetching = client.store.fetching(snapshot, &batch).unwrap();
    batch
        .stage_all(&[server.store.read(snapshot.into()).unwrap()])
        .unwrap();
    batch.commit().unwrap();
    drop(fetching);
    assert!(!client.store.holds(snapshot).unwrap());
    assert!(client.sync().need(record, &[]).unwrap(), "fetched again");
    assert!(client.store.holds(snapshot).unwrap());
    assert_eq!(client.store.diff(empty, snapshot).unwrap().len(), 1);
    let src = Snapshots::new(&client.store)
        .entry_at(snapshot, &RepoPath::new("src").unwrap())
        .unwrap()
        .unwrap();
    client.store.remove(&[src.id]).unwrap();
    assert!(client.store.diff(empty, snapshot).is_err());
    client.sync().repair(record).unwrap();
    assert_eq!(client.store.diff(empty, snapshot).unwrap().len(), 1);
}

#[test]
fn the_remote_events_say_which_changes_moved() {
    let server = Repo::new(MAIN_RULES);
    let first = server.change_on_main(1, &[("a", "a")]);
    let second = server.change_on_main(2, &[("b", "b")]);
    let client = Client::new(&server);
    let both = [1, 2].map(|change| ChangeId::from_bytes([change; 16]));
    client.sync().fetch_changes(&both).unwrap();
    let moved = client.sync().moved(None).unwrap();
    assert_eq!(moved.changes, None, "without a cursor, any change may have");
    let cursor = moved.cursor;
    let nothing = client.sync().moved(Some(&cursor)).unwrap();
    assert_eq!(nothing.changes, Some(HashSet::new()));
    assert_eq!(nothing.cursor, cursor);

    let annotations = server.annotations();
    let result = annotations
        .issue(&ci_test(second, "node:ci"), None)
        .unwrap();
    let flaky = Statement {
        subject: RecordId::Statement(result),
        kind: "flaky".parse().unwrap(),
        verdict: None,
        inputs: None,
        ..ci_test(second, "user:ada")
    };
    annotations.issue(&flaky, None).unwrap();
    let tip = main_tip(&server.store);
    server.revision(1, &[first], tip, &[("a", "again")]);
    let moved = client.sync().moved(Some(&cursor)).unwrap();
    assert_eq!(moved.changes, Some(HashSet::from(both)));
    assert!(moved.cursor.next > cursor.next);
}

#[test]
fn pushes_refuse_stale_changes_and_bases_the_remote_lacks() {
    let server = Repo::new(MAIN_RULES);
    let shared = server.change_on_main(1, &[("a", "a")]);
    let client = Client::new(&server);
    client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
    let change = ChangeId::from_bytes([1; 16]);
    client.sync().fetch_changes(&[change]).unwrap();

    let base = main_tip(&client.store);
    let ours = client.revision(1, &[shared], base, &[("a", "ours")]);
    let theirs = server.revision(1, &[shared], base, &[("a", "theirs")]);
    let error = client.sync().push(&[change]).unwrap_err();
    assert!(
        matches!(error, SyncError::Stale { change: c, head } if c == change && head == theirs),
        "{error}"
    );
    assert!(
        !server.store.contains(ours.into()).unwrap(),
        "nothing was sent"
    );

    let local = TargetName::new("local").unwrap();
    let key = NodeKey::from_bytes(&[9; 32]);
    let genesis = AcceptanceBody {
        target: local.id(),
        seq: 0,
        prev: None,
        state: build(&client.store, &[]),
        landed: Vec::new(),
        evidence: Vec::new(),
        policy: None,
    }
    .sign(&key);
    client.store.append(&local, &genesis).unwrap();
    let on_local = Base::Acceptance(genesis.encode().id().into());
    client.revision(2, &[], on_local, &[("b", "b")]);
    let error = client
        .sync()
        .push(&[ChangeId::from_bytes([2; 16])])
        .unwrap_err();
    assert!(
        matches!(&error, SyncError::Unshared { target: Some(target), .. } if *target == local),
        "{error}"
    );
}

#[test]
fn each_part_of_a_push_expects_the_heads_the_parts_before_left() {
    let change = ChangeId::from_bytes([1; 16]);
    let [r0, r1, r2] = [1, 2, 3].map(|n| RevisionId::from(ObjectId::from_bytes([n; 32])));
    let mut transfer = Transfer {
        heads: HashMap::from([(change, vec![r0])]),
        revisions: HashMap::from([
            (r1.into(), (change, vec![r0])),
            (r2.into(), (change, vec![r1])),
        ]),
        ..Transfer::default()
    };
    let expected = |heads: Vec<RevisionId>| vec![Expected { change, heads }];
    assert_eq!(transfer.expect(&[r1.into()]), expected(vec![r0]));
    assert_eq!(transfer.expect(&[r2.into()]), expected(vec![r1]));
    assert_eq!(transfer.expect(&[]), []);
}

#[test]
fn revisions_go_with_their_reserved_statements() {
    let [first, second] = [1, 2].map(|n| RevisionId::from(ObjectId::from_bytes([n; 32])));
    let revision = |change| Revision {
        change: ChangeId::from_bytes([change; 16]),
        predecessors: Vec::new(),
        base: Base::Revision(first),
        deps: Vec::new(),
        snapshot: TreeId::from(ObjectId::from_bytes([3; 32])),
        description: ObjectId::from_bytes([4; 32]).into(),
        author: "user:ada".parse().unwrap(),
    };
    let conflict = Statement {
        kind: Kind::CONFLICT,
        ..ci_test(first, "user:ada")
    };
    let result = ci_test(first, "user:ada");
    let abandon = Statement {
        kind: Kind::ABANDON,
        ..ci_test(second, "user:ada")
    };
    let listing = Statement {
        supersedes: Some(conflict.encode().id().into()),
        ..Statement {
            kind: Kind::CONFLICT,
            ..ci_test(second, "user:ada")
        }
    };
    let statements: Vec<_> = [conflict, result, abandon, listing]
        .into_iter()
        .map(|statement| (statement.encode().id().into(), statement))
        .collect();
    let [conflict, result, abandon, listing] =
        [0, 1, 2, 3].map(|at| ObjectId::from(statements[at].0));
    let mut transfer = Transfer::default();
    let others = transfer.add_revisions(&[(first, revision(1)), (second, revision(2))], statements);
    assert_eq!(
        others
            .iter()
            .map(|&(id, _)| id.into())
            .collect::<Vec<ObjectId>>(),
        [result]
    );
    assert_eq!(
        transfer.ids,
        [first.into(), second.into(), conflict, abandon, listing]
    );
    assert_eq!(
        transfer.joined,
        HashSet::from([second.into(), conflict, abandon, listing])
    );
    let alone = |statement: Statement| (statement.encode().id().into(), statement);
    let statements = vec![
        alone(Statement {
            kind: Kind::ABANDON,
            ..ci_test(first, "user:ada")
        }),
        alone(Statement {
            kind: Kind::DIVERGENT,
            ..ci_test(second, "user:ada")
        }),
    ];
    let [abandon, divergent] = [0, 1].map(|at| ObjectId::from(statements[at].0));
    let mut transfer = Transfer::default();
    transfer.add_revisions(&[(first, revision(1)), (second, revision(2))], statements);
    assert_eq!(
        transfer.ids,
        [first.into(), abandon, second.into(), divergent]
    );
    assert_eq!(transfer.joined, HashSet::from([abandon, divergent]));
}

#[test]
fn statements_about_one_record_go_in_one_request() {
    let [first, second] = [1, 2].map(|n| RevisionId::from(ObjectId::from_bytes([n; 32])));
    let pass = ci_test(first, "user:ada");
    let pass_id = StatementId::from(pass.encode().id());
    let fail = Statement {
        verdict: Some(Verdict::Fail),
        supersedes: Some(pass_id),
        ..pass.clone()
    };
    let other = ci_test(second, "user:ada");
    let flaky = Statement {
        subject: RecordId::Statement(pass_id),
        kind: "flaky".parse().unwrap(),
        ..other.clone()
    };
    let statements: Vec<_> = [pass, other, fail, flaky]
        .into_iter()
        .map(|statement| (statement.encode().id().into(), statement))
        .collect();
    let mut transfer = Transfer::default();
    transfer.add_statements(&statements);
    let [pass, other, fail, flaky] = [0, 1, 2, 3].map(|at| ObjectId::from(statements[at].0));
    assert_eq!(transfer.ids, [pass, fail, flaky, other]);
    assert_eq!(transfer.joined, HashSet::from([fail, flaky]));
}

#[test]
fn a_push_in_parts_never_lands_a_result_its_replacement_follows() {
    let server = Repo::new(
        "[targets.main]\nauthority = \"node:queue\"\n\
             require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"user:ada\" }]\n",
    );
    let revision = server.change_on_main(1, &[("src/a", "a")]);
    let change = ChangeId::from_bytes([1; 16]);
    let waits = server.land(1);
    assert!(matches!(
        waits[..],
        [origin_shared::landing::Landing::Pending(_)]
    ));
    let client = Client::new(&server);
    client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
    client.sync().fetch_changes(&[change]).unwrap();
    let pass = ci_test(revision, "user:ada");
    let fail = Statement {
        verdict: Some(Verdict::Fail),
        supersedes: Some(pass.encode().id().into()),
        ..pass.clone()
    };
    client
        .store
        .write_all(&[pass.encode(), fail.encode()])
        .unwrap();
    let tip = server.tip(&TargetName::MAIN);
    let pushed = client.sync().with_pack_limit(1).push(&[change]).unwrap();
    assert_eq!(pushed.statements, 2);
    assert_eq!(server.tip(&TargetName::MAIN), tip);
}

#[test]
fn a_push_in_parts_never_lands_an_abandoned_revision() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for fill in [false, true] {
        let server = Repo::new(MAIN_RULES);
        let client = Client::new(&server);
        client.sync().fetch_chains(&[TargetName::MAIN]).unwrap();
        let revision = if fill {
            let mut revision = Revision {
                change: ChangeId::from_bytes([1; 16]),
                predecessors: Vec::new(),
                base: main_tip(&client.store),
                deps: Vec::new(),
                snapshot: build(&client.store, &[("a", "gone")]),
                description: EncodedObject::attachment(b"").id().into(),
                author: "user:ada".parse().unwrap(),
            };
            client
                .store
                .write_all(&[EncodedObject::blob(b"gone")])
                .unwrap();
            let wire = |objects: Vec<EncodedObject>| Pack { objects }.encode().len();
            let content = client
                .store
                .added(revision.snapshot, &[], &[], None)
                .unwrap()
                .iter()
                .map(|placed| client.store.read(placed.entry.id).unwrap())
                .collect();
            let room = Pack::LIMIT
                - wire(content)
                - wire(vec![revision.encode(), EncodedObject::attachment(b"")]);
            let description = EncodedObject::attachment(&vec![b'x'; room]);
            revision.description = description.id().into();
            let revision = revision.encode();
            client
                .store
                .write_all(&[description, revision.clone()])
                .unwrap();
            revision.id().into()
        } else {
            client.revision(1, &[], main_tip(&client.store), &[("a", "gone")])
        };
        let abandonment = Statement {
            kind: Kind::ABANDON,
            verdict: None,
            ..ci_test(revision, "user:ada")
        }
        .encode();
        client
            .store
            .write_all(std::slice::from_ref(&abandonment))
            .unwrap();
        let hex = abandonment.id().to_hex();
        let shard = server.store.root().join("objects/loose").join(&hex[..2]);
        std::fs::create_dir_all(&shard).unwrap();
        let root = std::fs::metadata(&shard).unwrap().uid() == 0;
        std::fs::set_permissions(&shard, std::fs::Permissions::from_mode(0o555)).unwrap();
        let limit = if fill { Pack::LIMIT } else { 1 };
        let pushed = client
            .sync()
            .with_pack_limit(limit)
            .push(&[ChangeId::from_bytes([1; 16])]);
        std::fs::set_permissions(&shard, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !root {
            assert!(pushed.is_err(), "{pushed:?}");
            assert!(!server.store.contains(revision.into()).unwrap());
        }
        let change = ChangeId::from_bytes([1; 16]);
        client
            .sync()
            .with_pack_limit(limit)
            .push(&[change])
            .unwrap();
        let landed =
            server
                .queue()
                .land_change(&mut server.annotations(), change, &TargetName::MAIN);
        assert!(
            matches!(landed, Err(crate::queue::LandError::Abandoned(_))),
            "{landed:?}"
        );
    }
}

#[test]
fn queue_rebases_and_pushes_never_make_a_change_diverge() {
    let server = Repo::new(MAIN_RULES);
    server.change_on_main(1, &[("a", "a")]);
    let original = server.change_on_main(2, &[("b", "b")]);
    server.land(1);
    let change = ChangeId::from_bytes([2; 16]);
    let pushing = server.store.lock_pushes().unwrap();
    let description = EncodedObject::blob(b"concurrent edit");
    let successor = Revision {
        predecessors: vec![original],
        description: description.id().into(),
        ..server.store.get(original).unwrap()
    }
    .encode();
    let landed = std::thread::scope(|scope| {
        let landing = scope.spawn(|| {
            server
                .queue()
                .land_change(&mut server.annotations(), change, &TargetName::MAIN)
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        server
            .store
            .write_all(&[description, successor.clone()])
            .unwrap();
        drop(pushing);
        landing.join().unwrap()
    });
    let heads = Index::open(&server.store).unwrap().heads(change);
    assert_eq!(heads.len(), 1, "{landed:?}");
    assert!(
        matches!(landed, Err(crate::queue::LandError::Moved(moved)) if moved == change)
            || landed.is_ok(),
        "{landed:?}"
    );
}

#[test]
fn cursors_of_another_op_log_read_everything() {
    let server = Repo::new(MAIN_RULES);
    let first = server.change_on_main(1, &[("a", "a")]);
    let change = ChangeId::from_bytes([1; 16]);
    let client = Client::new(&server);
    let fetch = client
        .sync()
        .fetch(&[TargetName::MAIN], &[change], None)
        .unwrap();
    let cursor = Cursor {
        generation: "restored".to_owned(),
        next: fetch.cursor.next + 10_000,
    };
    let newer = server.revision(1, &[first], main_tip(&server.store), &[("a", "new")]);
    let fetch = client.sync().fetch(&[], &[], Some(&cursor)).unwrap();
    assert!(client.store.contains(newer.into()).unwrap());
    assert_ne!(fetch.cursor.generation, cursor.generation);
}
