//! The queue's tests, grouped by topic, and the test repository they share.

mod changes;
mod conflicts;
mod evidence;
mod policy;
mod targets;

use origin_platform::Native;
use origin_shared::{
    AcceptanceBody, Base, BlobId, ChangeId, Digest, EncodedObject, Inputs, Kind, NodeKey,
    PrincipalName, Record, RecordId, RepoPath, Revision, RevisionId, Source, Statement,
    StatementId, TargetName, Tree, TreeId, Verdict,
};

use super::*;
use crate::annotations::Annotations;
use crate::conflict::Conflicts;
use crate::policy::{Policy, PolicyError, Requirement};
use crate::store::Store;
use crate::targets::Head;
use crate::tree::tests::build;

/// A test repository whose `policy` and `main` targets the node `queue`
/// orders. `Repo::new` adds `extra` to its `policy.toml`.
struct Repo {
    _dir: tempfile::TempDir,
    store: Store,
    key: NodeKey,
}

impl Repo {
    fn new(extra: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let key = NodeKey::from_bytes(&[7; 32]);
        let repo = Self {
            _dir: dir,
            store,
            key,
        };
        repo.queue().init(&repo.policy_toml(extra)).unwrap();
        repo
    }

    /// Returns a `policy.toml` that makes this node the authority of `policy`,
    /// followed by `extra`.
    fn policy_toml(&self, extra: &str) -> String {
        format!(
            "[nodes]\nqueue = \"{}\"\n[targets.policy]\nauthority = \"node:queue\"\n{extra}",
            self.key.public()
        )
    }

    /// Writes a snapshot of `files`, with their contents, and returns its tree.
    fn files(&self, files: &[(&str, &str)]) -> TreeId {
        let blobs: Vec<_> = files
            .iter()
            .map(|(_, content)| EncodedObject::blob(content.as_bytes()))
            .collect();
        self.store.write_all(&blobs).unwrap();
        build(&self.store, files)
    }

    fn queue(&self) -> Queue<'_> {
        Queue::new(&self.store, &self.key, PrincipalName::new("queue").unwrap())
    }

    fn tip(&self, target: &TargetName) -> Head {
        self.store.head(target).unwrap().unwrap()
    }

    fn state(&self, target: &TargetName) -> TreeId {
        self.store
            .get(self.tip(target).acceptance)
            .unwrap()
            .body()
            .state
    }

    /// Writes a revision of `change` with `files`, made on `base`.
    fn revision(
        &self,
        change: u8,
        predecessors: &[RevisionId],
        base: Base,
        files: &[(&str, &str)],
    ) -> RevisionId {
        let encoded = Revision {
            change: ChangeId::from_bytes([change; 16]),
            predecessors: predecessors.to_vec(),
            base,
            deps: Vec::new(),
            snapshot: self.files(files),
            description: BlobId::from(EncodedObject::blob(b"").id()),
            author: "user:ada".parse().unwrap(),
        }
        .encode();
        self.store
            .write_all(std::slice::from_ref(&encoded))
            .unwrap();
        encoded.id().into()
    }

    /// Writes a revision of the new change `change` on the tip of `main`,
    /// adding `files` to `main`'s files.
    fn change_on_main(&self, change: u8, files: &[(&str, &str)]) -> RevisionId {
        let base = Base::Acceptance(self.tip(&TargetName::MAIN).acceptance);
        let mut all = self.main_files();
        all.extend(
            files
                .iter()
                .map(|&(path, content)| (path.to_owned(), content.to_owned())),
        );
        let all: Vec<_> = all.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
        self.revision(change, &[], base, &all)
    }

    /// Returns the files of `main`'s tip, with their contents.
    fn main_files(&self) -> Vec<(String, String)> {
        let empty = build(&self.store, &[]);
        self.store
            .diff(empty, self.state(&TargetName::MAIN))
            .unwrap()
            .into_iter()
            .map(|change| {
                let content = self
                    .store
                    .try_blob(change.new.unwrap().id)
                    .unwrap()
                    .unwrap();
                (
                    change.path.as_str().to_owned(),
                    String::from_utf8(content).unwrap(),
                )
            })
            .collect()
    }

    fn land(&self, change: u8) -> Vec<Landing> {
        self.queue()
            .land_change(ChangeId::from_bytes([change; 16]), &TargetName::MAIN)
            .unwrap()
    }
}

fn change(id: u8) -> Proposal {
    Proposal::Change(ChangeId::from_bytes([id; 16]))
}

/// Returns the revision a landing landed. Panics if it landed no revision.
fn landed(landing: &Landing) -> RevisionId {
    match landing {
        Landing::Landed {
            source: Source::Revision(revision),
            ..
        } => *revision,
        other => panic!("expected a landed revision, got {other:?}"),
    }
}

/// Returns a passing `ci.test` result about `subject` from `issuer`, whose
/// inputs are `src`.
fn ci_test(subject: RevisionId, issuer: &str) -> Statement {
    Statement {
        subject: RecordId::Revision(subject),
        kind: Kind::new("ci.test").unwrap(),
        verdict: Some(Verdict::Pass),
        issuer: issuer.parse().unwrap(),
        inputs: Some(Inputs {
            paths: vec![RepoPath::new("src").unwrap()],
            definition: Digest::of(b"cargo test"),
            toolchain: Digest::of(b"rustc"),
            external: false,
        }),
        payload: None,
        supersedes: None,
        promoted_from: None,
    }
}

/// Returns a repository whose `main` requires a passing `ci.test` from
/// `user:ci`.
fn requiring_ci() -> Repo {
    Repo::new(
        "[targets.main]\nauthority = \"node:queue\"\n\
         require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"user:ci\" }]\n",
    )
}
