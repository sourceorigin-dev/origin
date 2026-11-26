use std::fs;

use origin_shared::event::Subject;
use origin_shared::{Base, ChangeId, ObjectId, RecordId, TargetName};

use super::*;
use crate::annotations::Annotations;
use crate::index::Index;
use crate::queue::tests::{Repo, ci_test};

/// A repository where landing on `main` requires a `ci.test` pass from
/// `node:ci`.
fn repo() -> Repo {
    Repo::new(
        "[groups]\nrunners = [\"node:ci\"]\n[targets.main]\nauthority = \"node:queue\"\n\
             require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"group:runners\" }]\n",
    )
}

/// Reads every event after `cursor` that passes `filter`, a page at a
/// time.
fn all(events: &Events<'_>, mut cursor: u64, filter: &Filter) -> Vec<Event> {
    let mut all = Vec::new();
    loop {
        let page = events.after(cursor, filter, 1).unwrap();
        if page.next == cursor {
            return all;
        }
        all.extend(page.events);
        cursor = page.next;
    }
}

/// The name of each event, with the kind of the statement it issues or
/// requests, if any.
fn summary(events: &[Event]) -> Vec<(&'static str, Option<String>)> {
    events
        .iter()
        .map(|event| {
            let kind = match &event.kind {
                EventKind::StatementIssued { kind, .. } => Some(kind.to_string()),
                EventKind::CheckRequested { requirement, .. } => Some(requirement.kind.to_string()),
                _ => None,
            };
            (event.kind.name(), kind)
        })
        .collect()
}

#[test]
fn every_statement_revision_landing_and_request_fires_an_event() {
    let repo = repo();
    let events = Events::new(&repo.store);
    let start = events.end().unwrap();

    let code = repo.change_on_main(1, &[("src/lib.rs", "a")]);
    let docs = repo.change_on_main(2, &[("docs/guide.md", "# Guide")]);
    let clash = repo.change_on_main(3, &[("src/lib.rs", "b")]);
    repo.land(1);
    let requested = all(&events, start, &Filter::default());
    assert_eq!(
        summary(&requested).last(),
        Some(&("check.requested", Some("ci.test".to_owned())))
    );
    let annotations = Annotations::open(&repo.store).unwrap();
    annotations.issue(&ci_test(code, "node:ci"), None).unwrap();
    repo.land(1);
    // Rebased onto the tested code, the docs change gets its result
    // promoted; the other change conflicts.
    repo.land(2);
    repo.land(3);
    // Two revisions of the docs change from the same one: divergence.
    let base = Base::Acceptance(repo.tip(&TargetName::MAIN).acceptance);
    repo.revision(2, &[docs], base, &[("docs/guide.md", "left")]);
    repo.revision(2, &[docs], base, &[("docs/guide.md", "right")]);
    Annotations::open(&repo.store)
        .unwrap()
        .flag_divergence(ChangeId::from_bytes([2; 16]), "node:queue".parse().unwrap())
        .unwrap();

    let all = all(&events, start, &Filter::default());
    let summary = summary(&all);
    let named = |name, kind: Option<&str>| {
        summary
            .iter()
            .filter(|(n, k)| *n == name && k.as_deref() == kind)
            .count()
    };
    assert_eq!(named("revision.created", None), 6, "{summary:?}");
    assert_eq!(
        named("check.requested", Some("ci.test")),
        0,
        "the landing is no longer waiting, so nothing is left to check"
    );
    assert_eq!(
        named("statement.issued", Some("ci.test")),
        2,
        "issued and promoted"
    );
    assert_eq!(named("statement.issued", Some("core.conflict")), 1);
    assert_eq!(named("statement.issued", Some("core.divergent")), 1);
    assert_eq!(named("target.advanced", None), 2);
    assert!(all.windows(2).all(|pair| pair[0].cursor <= pair[1].cursor));
    let conflict = all
        .iter()
        .find(|event| summary_kind(event) == Some("core.conflict"))
        .unwrap();
    assert_eq!(
        conflict.kind.subject(),
        Subject::Record(RecordId::Revision(clash))
    );
}

fn summary_kind(event: &Event) -> Option<&str> {
    match &event.kind {
        EventKind::StatementIssued { kind, .. } => Some(kind.as_str()),
        _ => None,
    }
}

#[test]
fn cursors_survive_index_rebuilds_and_filters_select() {
    let repo = repo();
    let events = Events::new(&repo.store);
    let code = repo.change_on_main(1, &[("src/lib.rs", "a")]);
    Annotations::open(&repo.store)
        .unwrap()
        .issue(&ci_test(code, "node:ci"), None)
        .unwrap();
    let before = all(&events, 0, &Filter::default());
    fs::remove_dir_all(repo.store.root().join("index")).unwrap();
    Index::rebuild(&repo.store).unwrap();
    assert_eq!(all(&events, 0, &Filter::default()), before);

    let kinds = |pattern: &str| Filter {
        kinds: vec![pattern.parse().unwrap()],
        ..Filter::default()
    };
    assert_eq!(
        summary(&all(&events, 0, &kinds("ci.*"))),
        [("statement.issued", Some("ci.test".to_owned()))]
    );
    assert_eq!(all(&events, 0, &kinds("target.advanced")).len(), 2);
    let about = |start: String| Filter {
        subjects: vec![start],
        ..Filter::default()
    };
    // The revision's creation, and the statement about it.
    let code_hex = code.to_hex();
    assert_eq!(all(&events, 0, &about(code_hex[..8].to_owned())).len(), 2);
    assert_eq!(
        all(&events, 0, &about(format!("revision:{}", &code_hex[..8]))).len(),
        2
    );
    let change = repo.store.get(code).unwrap().change.to_hex();
    let created = all(&events, 0, &about(format!("change:{}", &change[..8])));
    assert_eq!(summary(&created), [("revision.created", None)]);
    assert_eq!(all(&events, 0, &about("main".to_owned())).len(), 1);
}

#[test]
fn unpublished_entries_hold_back_later_events_while_writes_are_in_progress() {
    let repo = repo();
    let events = Events::new(&repo.store);
    // An entry for a revision that was never published, as a crashed
    // write leaves.
    let missing = index::Entry::Revision {
        id: ObjectId::from_bytes([9; 32]).into(),
        change: ChangeId::from_bytes([9; 16]),
        predecessors: Vec::new(),
    };
    let (crashed, writing) = repo
        .store
        .oplog()
        .append_in_progress(&Effect::Wrote(vec![missing]))
        .unwrap();
    repo.change_on_main(1, &[("a", "a")]);

    let held = events
        .after(crashed - 1, &Filter::default(), usize::MAX)
        .unwrap();
    assert_eq!(
        held,
        Page {
            events: Vec::new(),
            next: crashed - 1
        },
        "it may be in progress"
    );
    assert_eq!(events.end().unwrap(), crashed - 1, "no cursor passes it");
    // Its writer crashes.
    drop(writing);
    let page = events
        .after(crashed - 1, &Filter::default(), usize::MAX)
        .unwrap();
    assert_eq!(summary(&page.events), [("revision.created", None)]);
    assert!(page.next > crashed);
}

#[test]
fn a_landing_waiting_again_after_a_conflict_requests_its_checks() {
    let repo = repo();
    let events = Events::new(&repo.store);
    let code = repo.change_on_main(1, &[("src/lib.rs", "a")]);
    let clash = repo.change_on_main(2, &[("src/lib.rs", "b")]);
    Annotations::open(&repo.store)
        .unwrap()
        .issue(&ci_test(code, "node:ci"), None)
        .unwrap();
    repo.land(1);
    repo.land(2);
    let start = events.end().unwrap();
    // While the landing waits again, its entry is in the log before its
    // file replaces the conflict's: readers wait for both.
    let again = Effect::Waiting {
        onto: TargetName::MAIN,
        proposal: origin_shared::landing::Proposal::Change(ChangeId::from_bytes([2; 16])),
    };
    let (_, writing) = repo.store.oplog().append_in_progress(&again).unwrap();
    assert_eq!(
        events.after(start, &Filter::default(), 10).unwrap().next,
        start
    );
    drop(writing);

    let base = Base::Acceptance(repo.tip(&TargetName::MAIN).acceptance);
    repo.revision(2, &[clash], base, &[("src/lib.rs", "a and b")]);
    repo.land(2);
    let requested = all(&events, start, &Filter::default());
    assert_eq!(
        summary(&requested).last(),
        Some(&("check.requested", Some("ci.test".to_owned()))),
        "{requested:?}"
    );
}

#[test]
fn waiting_returns_as_soon_as_an_event_arrives() {
    let repo = repo();
    let events = Events::new(&repo.store);
    let end = events.end().unwrap();
    let empty = events
        .wait(end, &Filter::default(), 10, Events::<Native>::POLL)
        .unwrap();
    assert_eq!(
        empty,
        Page {
            events: Vec::new(),
            next: end
        }
    );
    thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Events::<Native>::POLL * 2);
            repo.change_on_main(1, &[("a", "a")]);
        });
        let page = events
            .wait(end, &Filter::default(), 10, Duration::from_secs(10))
            .unwrap();
        assert_eq!(summary(&page.events), [("revision.created", None)]);
    });
}
