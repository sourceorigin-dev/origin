use std::sync::Arc;

use origin_shared::{NodeKey, TargetName};

use super::*;
use crate::queue::tests::Repo;
use crate::service::{Node, OriginService, ServiceError};

/// Ada writes everything and changes the rules. Eve reads all but
/// `secrets`, writes only `src`, and issues reviews and checks. Both
/// land on `main`.
const RULES: &str = r#"
[targets.main]
authority = "node:queue"

[grants]
"user:ada" = ["admin", "land *"]
"user:eve" = ["land *", "annotate review", "annotate ci.*"]

[[paths]]
path = "/"
grants = { all = "read", "user:ada" = "write" }

[[paths]]
path = "src"
grants = { "user:eve" = "write" }

[[paths]]
path = "secrets"
grants = { all = "none", "user:ada" = "write" }
"#;

/// A repository whose `main` holds a README, a library, a secret and a
/// protected file, with its service.
struct Fixture {
    repo: Repo,
    service: OriginService,
}

impl Fixture {
    fn new() -> Self {
        let repo = Repo::new(RULES);
        repo.change_on_main(
            1,
            &[
                ("README.md", "hello"),
                ("src/lib.rs", "fn a() {}"),
                ("secrets/key", "hunter2"),
                ("protected/file", "keep"),
            ],
        );
        repo.land(1);
        let node = Node {
            name: "queue".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[7; 32])),
        };
        let service = OriginService::new(repo.store.clone(), node);
        Self { repo, service }
    }
}

fn caller(principal: &str) -> Caller {
    Caller::Principal {
        principal: principal.parse().unwrap(),
        scope: None,
    }
}

/// A caller for `principal`, with a token narrowing them to `scope` that
/// expires at `expires`.
fn scoped(principal: &str, scope: &[&str], expires: u64) -> Caller {
    Caller::Principal {
        principal: principal.parse().unwrap(),
        scope: Some(Scope {
            capabilities: scope.iter().map(|c| c.parse().unwrap()).collect(),
            expires,
        }),
    }
}

/// A week from now, in seconds since the Unix epoch.
fn in_a_week() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    (now.unwrap() + OriginService::MAX_TOKEN_TTL).as_secs()
}

#[test]
fn callers_who_read_nothing_see_nothing() {
    let fixture = Fixture::new();
    let revision = fixture.repo.change_on_main(2, &[]);
    let blind = scoped("user:eve", &["annotate review"], in_a_week());
    let service = &fixture.service;
    assert!(!service.sees_repository(&blind).unwrap());
    let records = service.records(&blind, &[revision.into()]).unwrap();
    assert!(
        records.pack.objects.is_empty(),
        "not even a revision changing nothing"
    );
    assert_eq!(service.targets(&blind).unwrap(), []);
    let log = origin_shared::api::LogQuery {
        target: TargetName::MAIN,
        since: 0,
        limit: None,
    };
    assert!(matches!(
        service.log(&blind, &log),
        Err(ServiceError::UnknownTarget(_))
    ));
    assert_eq!(service.log(&caller("user:eve"), &log).unwrap().len(), 2);
}

#[test]
fn tokens_issued_from_a_token_expire_with_it() {
    let fixture = Fixture::new();
    let expires = in_a_week() - 6 * 24 * 3600;
    let parent = scoped("user:eve", &["annotate review", "land *"], expires);
    let request = origin_shared::api::TokenRequest {
        scope: vec!["annotate review".parse().unwrap()],
        ttl: origin_shared::Period::from_secs(OriginService::MAX_TOKEN_TTL.as_secs()),
    };
    let issued = fixture.service.issue_token(&parent, &request).unwrap();
    assert_eq!(issued.expires, expires);
    let issued = fixture
        .service
        .issue_token(&caller("user:eve"), &request)
        .unwrap();
    assert!(issued.expires > expires, "a static token's last a week");
}
