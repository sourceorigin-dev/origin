//! Enforcing what callers may do, as their
//! [`Rights`](origin_shared::access::Rights) say: what they read
//! of a repository's records.
//!
//! **Reading.** A caller who reads no path doesn't see the repository at
//! all. Otherwise they see its targets and acceptances, every revision that
//! changes a path they read, or changes nothing, and the statements about
//! what they see. Snapshots withhold the content of the paths they don't
//! read (see [`Store::added`]).

use std::collections::HashMap;

use origin_platform::Platform;
use origin_shared::access::{Capability, PathAccess};
use origin_shared::event::EventKind;
use origin_shared::{Principal, RecordId, RepoPath, Revision, RevisionId};

use crate::store::{Store, StoreError};

/// Who calls the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// The node's own user, through the command line or a remote on a
    /// local path: they may do everything.
    Owner,
    /// A client of a server that didn't authenticate.
    Anonymous,
    /// An authenticated client of a server.
    Principal {
        /// Who they are.
        principal: Principal,
        /// What their token narrows their rights to, if it's a scoped one.
        scope: Option<Scope>,
    },
}

/// What a scoped token narrows its bearer's rights to, and until when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    /// The capabilities it keeps of its bearer's rights.
    pub capabilities: Vec<Capability>,
    /// When it expires, in seconds since the Unix epoch: a token issued
    /// with it expires no later.
    pub expires: u64,
}

impl Caller {
    /// Who the caller is, unless the owner or anonymous.
    #[must_use]
    pub fn principal(&self) -> Option<&Principal> {
        match self {
            Self::Principal { principal, .. } => Some(principal),
            Self::Owner | Self::Anonymous => None,
        }
    }
}

/// What a caller sees of a repository's records, remembering what it
/// decided.
pub struct Reader<'a, P> {
    store: &'a Store<P>,
    /// What the caller may do with each path.
    paths: &'a PathAccess,
    seen: HashMap<RecordId, bool>,
}

impl<'a, P: Platform> Reader<'a, P> {
    /// What a caller who may do `paths` sees of `store`.
    pub fn new(store: &'a Store<P>, paths: &'a PathAccess) -> Self {
        Self {
            store,
            paths,
            seen: HashMap::new(),
        }
    }

    /// Whether the caller sees the repository: they read some path.
    #[must_use]
    pub fn sees_repository(&self) -> bool {
        self.paths.reads_under(&RepoPath::root())
    }

    /// Whether the caller sees every record.
    fn sees_all(&self) -> bool {
        self.paths.reads_all()
    }

    /// Whether the caller sees `record`, as the module explains. A record
    /// this node lacks, or whose base it lacks, is not seen.
    ///
    /// # Errors
    ///
    /// Fails if a record or tree cannot be read.
    pub fn sees(&mut self, record: RecordId) -> Result<bool, StoreError> {
        if self.sees_all() {
            return Ok(true);
        }
        if !self.sees_repository() {
            return Ok(false);
        }
        if let Some(&seen) = self.seen.get(&record) {
            return Ok(seen);
        }
        let seen = match record {
            RecordId::Revision(id) => self.sees_revision(id),
            RecordId::Statement(id) => match self.store.get(id) {
                Ok(statement) => self.sees(statement.subject),
                Err(error) => Err(error),
            },
            RecordId::Acceptance(_) | RecordId::Snapshot(_) => Ok(true),
        };
        let seen = match seen {
            Err(StoreError::NotFound(_)) => false,
            seen => seen?,
        };
        self.seen.insert(record, seen);
        Ok(seen)
    }

    fn sees_revision(&self, id: RevisionId) -> Result<bool, StoreError> {
        let revision: Revision = self.store.get(id)?;
        let changes = self
            .store
            .diff(self.store.base_snapshot(revision.base)?, revision.snapshot)?;
        Ok(changes.is_empty() || changes.iter().any(|change| self.paths.reads(&change.path)))
    }

    /// Whether the caller sees what `event` is about.
    ///
    /// # Errors
    ///
    /// Fails like [`Reader::sees`].
    pub fn sees_event(&mut self, event: &EventKind) -> Result<bool, StoreError> {
        match event {
            EventKind::CheckRequested { subject, .. } => self.sees(*subject),
            event => self.sees(event.record()),
        }
    }
}

/// Something a caller may not do.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Forbidden {
    /// The owner has no principal to issue a token for.
    #[error("only a signed-in caller gets a token, for themselves")]
    NoPrincipal,
    /// A token's scope reaches beyond its issuer's rights.
    #[error("a token may only narrow your rights, which don't include `{0}`")]
    Scope(Capability),
}

#[cfg(test)]
mod tests {
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

    /// The repository, with `main` holding a README, a library, a secret
    /// and a protected file, and its service.
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

    /// `caller`, with a token narrowing them to `scope`, expiring at
    /// `expires`.
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
            ttl: origin_shared::Period(OriginService::MAX_TOKEN_TTL),
        };
        let issued = fixture.service.issue_token(&parent, &request).unwrap();
        assert_eq!(issued.expires, expires);
        let issued = fixture
            .service
            .issue_token(&caller("user:eve"), &request)
            .unwrap();
        assert!(issued.expires > expires, "a static token's last a week");
    }
}
