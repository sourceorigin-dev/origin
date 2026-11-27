use std::sync::Arc;

use origin_platform::Native;
use origin_shared::event::{EventKind, Filter};
use origin_shared::pack::Pack;
use origin_shared::{AcceptanceBody, Base, NodeKey, RepoPath};

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
        let node = Node {
            name: "queue".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[7; 32])),
        };
        let service = OriginService::new(server.store.clone(), node).with_pack_limit(limit);
        let remote = Remote::Local(Box::new(service));
        Self {
            _dir: dir,
            store,
            remote,
        }
    }

    fn sync(&self) -> Sync<'_> {
        Sync::new(&self.store, &self.remote, "laptop".parse().unwrap())
    }
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
    assert!(client.sync().need(state, &[]).unwrap());
    assert!(!client.sync().need(state, &[]).unwrap(), "once");
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
    let snapshot = client.store.get(revision).unwrap().snapshot;
    let empty = build(&client.store, &[]);

    let batch = client.store.batch();
    let fetching = client.store.fetching(snapshot, &batch).unwrap();
    batch
        .stage_all(&[server.store.read(snapshot.into()).unwrap()])
        .unwrap();
    batch.commit().unwrap();
    drop(fetching);
    assert!(!client.store.holds(snapshot).unwrap());
    assert!(client.sync().need(snapshot, &[]).unwrap(), "fetched again");
    assert!(client.store.holds(snapshot).unwrap());
    assert_eq!(client.store.diff(empty, snapshot).unwrap().len(), 1);
    let src = Snapshots::new(&client.store)
        .entry_at(snapshot, &RepoPath::new("src").unwrap())
        .unwrap()
        .unwrap();
    client.store.remove(&[src.id]).unwrap();
    assert!(client.store.diff(empty, snapshot).is_err());
    client.sync().repair(snapshot).unwrap();
    assert_eq!(client.store.diff(empty, snapshot).unwrap().len(), 1);
}
