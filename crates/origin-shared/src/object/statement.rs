use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use super::{DecodeError, Field, ObjectKind, Reader, Record, fields, tagged};
use crate::name::names;
use crate::{
    AcceptanceId, AttachmentId, Digest, InvalidName, ObjectId, ParseIdError, Principal, RepoPath,
    RevisionId, StatementId, TreeEntry, TreeId,
};

/// A claim about another record, attributed to its issuer, such as a check
/// result, a review, a conflict or an abandonment.
///
/// Statements are permanent. An issuer revises a statement by issuing
/// another that supersedes it. A promoted statement carries another
/// statement over to a record on the same target with the same input
/// digest. It points at the original instead of copying its issuer.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Statement {
    /// The record the claim is about.
    pub subject: RecordId,
    /// What kind of claim it is.
    pub kind: Kind,
    /// The outcome. It is the only field origin itself interprets.
    pub verdict: Option<Verdict>,
    /// Who makes the claim.
    pub issuer: Principal,
    /// What the claim depended on. Statements with inputs can be promoted to
    /// revisions with the same input digest.
    pub inputs: Option<Inputs>,
    /// Free-form details, defined by whoever owns the kind.
    pub payload: Option<AttachmentId>,
    /// A statement from the same issuer that this one replaces.
    pub supersedes: Option<StatementId>,
    /// The statement this one promotes. Its kind, verdict, inputs and
    /// payload also hold here, because the input digests are equal. The issuer
    /// is whoever promoted it; the original keeps its own issuer.
    pub promoted_from: Option<StatementId>,
}

fields!(Statement {
    subject,
    kind,
    verdict,
    issuer,
    inputs,
    payload,
    supersedes,
    promoted_from,
});

impl Record for Statement {
    const KIND: ObjectKind = ObjectKind::Statement;
}

impl Statement {
    /// Removes the promotions that a direct statement replaces. `claims` are
    /// the unsuperseded statements about one record. Each promotion is removed
    /// if `claims` also holds a statement that isn't a promotion (a result or a
    /// withdrawal) with the kind and issuer of the promotion's original: a
    /// result that ran on the record wins over one promoted to it.
    ///
    /// `pair` returns a claim's statement and the statement whose claim it
    /// makes: itself, or a promotion's original. `claims` must not hold a
    /// superseded statement, or a promotion that still holds could be removed.
    pub fn drop_replaced_promotions<T>(
        claims: &mut Vec<T>,
        pair: impl Fn(&T) -> (&Statement, &Statement),
    ) {
        let direct: HashSet<_> = claims
            .iter()
            .map(&pair)
            .filter(|(statement, _)| statement.promoted_from.is_none())
            .map(|(statement, _)| (statement.kind.clone(), statement.issuer.clone()))
            .collect();
        claims.retain(|claim| {
            let (statement, claimed) = pair(claim);
            statement.promoted_from.is_none()
                || !direct.contains(&(claimed.kind.clone(), claimed.issuer.clone()))
        });
    }

    /// The BLAKE3 key derivation context for promotion groups.
    const GROUP_CONTEXT: &str = "origin 2026-10 promotion group";

    /// Whether the statement withdraws the one it supersedes, claiming
    /// nothing instead: it supersedes a statement and has no verdict. A
    /// statement of a reserved kind is never a withdrawal, since those have no
    /// verdict anyway.
    #[must_use]
    pub fn withdraws(&self) -> bool {
        self.verdict.is_none() && self.supersedes.is_some() && !self.kind.is_reserved()
    }

    /// Whether the statement can be promoted to other records. It must:
    ///
    /// - declare inputs that aren't `external`;
    /// - not be about a statement, which has no snapshot to take the input
    ///   digest of;
    /// - not be a promotion (its original is promoted instead) or a withdrawal
    ///   (it claims nothing);
    /// - not have a reserved kind: abandonments, conflicts and divergence are
    ///   about one revision, whatever its inputs.
    #[must_use]
    pub fn is_promotable(&self) -> bool {
        self.promoted_from.is_none()
            && !self.withdraws()
            && !self.kind.is_reserved()
            && !matches!(self.subject, RecordId::Statement(_))
            && self.inputs.as_ref().is_some_and(|inputs| !inputs.external)
    }

    /// The digest of the group of statements this one is interchangeable
    /// with when promoting: those of its kind, from its issuer, declaring the
    /// same inputs. A record needs only one result per group. Returns `None` if
    /// the statement isn't promotable.
    ///
    /// It hashes the [`Inputs::spec`], then the encoded kind and issuer.
    #[must_use]
    pub fn promotion_group(&self) -> Option<Digest> {
        let inputs = self.inputs.as_ref().filter(|_| self.is_promotable())?;
        let mut encoded = inputs.spec().as_bytes().to_vec();
        self.kind.write(&mut encoded);
        self.issuer.write(&mut encoded);
        Some(Digest::from_bytes(blake3::derive_key(
            Self::GROUP_CONTEXT,
            &encoded,
        )))
    }
}

/// A record a statement can be about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RecordId {
    /// A snapshot.
    Snapshot(TreeId),
    /// A revision.
    Revision(RevisionId),
    /// Another statement, such as a failed check marked flaky.
    Statement(StatementId),
    /// An acceptance.
    Acceptance(AcceptanceId),
}

impl RecordId {
    /// Describes the record for humans by kind and short id, such as
    /// `revision 3f1c9a2e07b4`.
    #[must_use]
    pub fn describe(&self) -> String {
        let (kind, id) = match self {
            Self::Snapshot(id) => ("snapshot", id.short()),
            Self::Revision(id) => ("revision", id.short()),
            Self::Statement(id) => ("statement", id.short()),
            Self::Acceptance(id) => ("acceptance", id.short()),
        };
        format!("{kind} {id}")
    }

    /// Returns the kind of the record's object. A snapshot's object is a tree.
    #[must_use]
    pub fn kind(self) -> ObjectKind {
        match self {
            Self::Snapshot(_) => ObjectKind::Tree,
            Self::Revision(_) => ObjectKind::Revision,
            Self::Statement(_) => ObjectKind::Statement,
            Self::Acceptance(_) => ObjectKind::Acceptance,
        }
    }
}

impl From<RecordId> for ObjectId {
    fn from(record: RecordId) -> Self {
        match record {
            RecordId::Snapshot(id) => id.into(),
            RecordId::Revision(id) => id.into(),
            RecordId::Statement(id) => id.into(),
            RecordId::Acceptance(id) => id.into(),
        }
    }
}

/// Displays the record's kind and full id, such as `revision:<id>`. A
/// snapshot displays as `snapshot:<id>`.
impl fmt::Display for RecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Snapshot(_) => "snapshot",
            Self::Revision(_) => "revision",
            Self::Statement(_) => "statement",
            Self::Acceptance(_) => "acceptance",
        };
        write!(f, "{kind}:{}", ObjectId::from(*self))
    }
}

/// Parses the form [`RecordId`] displays as.
impl FromStr for RecordId {
    type Err = ParseIdError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let invalid = || ParseIdError {
            what: "record",
            input: input.to_owned(),
        };
        let (kind, id) = input.split_once(':').ok_or_else(invalid)?;
        let id: ObjectId = id.parse().map_err(|_| invalid())?;
        Ok(match kind {
            "snapshot" => Self::Snapshot(id.into()),
            "revision" => Self::Revision(id.into()),
            "statement" => Self::Statement(id.into()),
            "acceptance" => Self::Acceptance(id.into()),
            _ => return Err(invalid()),
        })
    }
}

tagged!(RecordId {
    Snapshot = 2,
    Revision = 3,
    Statement = 4,
    Acceptance = 5,
});

names! {
    /// The kind of a statement, namespaced by its owner, such as `ci.test`,
    /// `review` or `core.conflict`: dot-separated segments of lowercase ASCII
    /// letters, digits, `_` and `-`.
    Kind("statement kind");
}

impl Kind {
    /// The author gave up the change. The payload says why.
    pub const ABANDON: Self = Self(Cow::Borrowed("core.abandon"));
    /// A landing could not rebase the change. The payload lists the
    /// conflicting paths.
    pub const CONFLICT: Self = Self(Cow::Borrowed("core.conflict"));
    /// The change has more than one head.
    pub const DIVERGENT: Self = Self(Cow::Borrowed("core.divergent"));

    /// Whether the kind is reserved for origin itself: it starts with
    /// `core.`.
    #[must_use]
    pub fn is_reserved(&self) -> bool {
        self.0.starts_with("core.")
    }

    fn is_valid(name: &str) -> bool {
        name.split('.').all(|segment| {
            !segment.is_empty()
                && segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
                })
        })
    }
}

/// The outcome of a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Verdict {
    /// The check passed, or the reviewer approves.
    Pass = 1,
    /// The check failed, or the reviewer objects.
    Fail = 2,
    /// Neither: informational.
    Neutral = 3,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Neutral => "neutral",
        })
    }
}

/// Parses the names [`Verdict`] displays as: `pass`, `fail` and `neutral`.
impl FromStr for Verdict {
    type Err = InvalidName;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "pass" => Ok(Self::Pass),
            "fail" => Ok(Self::Fail),
            "neutral" => Ok(Self::Neutral),
            _ => Err(InvalidName {
                what: "verdict",
                name: input.to_owned(),
            }),
        }
    }
}

impl Field for Verdict {
    fn write(&self, out: &mut Vec<u8>) {
        out.push(*self as u8);
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match reader.u8()? {
            1 => Ok(Self::Pass),
            2 => Ok(Self::Fail),
            3 => Ok(Self::Neutral),
            tag => Err(DecodeError::UnknownTag {
                field: "Verdict",
                tag,
            }),
        }
    }
}

/// The inputs a statement depended on.
///
/// A record's input digest hashes the ids that its snapshot stores at
/// `paths`, the ids that the state of its target stores at `target_paths`,
/// `definition` and `toolchain`. A statement that declares inputs and isn't
/// `external` is promoted to every revision on the same target with the
/// same input digest.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Inputs {
    /// The files and directories read. Each covers everything under it.
    pub paths: Vec<RepoPath>,
    /// The files and directories to read from the state of the record's target
    /// rather than from its snapshot, such as the definition of the checks in
    /// force there. A revision is on the target of the acceptance it's based
    /// on, directly or through the revisions below it. An acceptance is on its
    /// own target.
    #[serde(default)]
    pub target_paths: Vec<RepoPath>,
    /// The hash of the check's command and configuration.
    pub definition: Digest,
    /// The hash of the tools that ran the check, such as the output of
    /// `rustc -V`.
    pub toolchain: Digest,
    /// Whether the claim also depended on something outside the snapshot,
    /// such as the network. Such a statement is never promoted.
    pub external: bool,
}

fields!(Inputs {
    paths,
    target_paths,
    definition,
    toolchain,
    external,
});

impl Inputs {
    /// The BLAKE3 key derivation contexts, which keep these hashes distinct
    /// from every other hash. The date is a version for what they hash: 2026-11
    /// added the target paths.
    const SPEC_CONTEXT: &str = "origin 2026-11 input spec";
    const DIGEST_CONTEXT: &str = "origin 2026-11 input digest";

    /// Returns the paths read from each source, in this order, each list sorted
    /// and without duplicates, so the order they're declared in doesn't matter.
    fn sorted_paths(&self) -> [(InputSource, Vec<&RepoPath>); 2] {
        [
            (InputSource::Snapshot, Self::sorted(&self.paths)),
            (InputSource::Target, Self::sorted(&self.target_paths)),
        ]
    }

    /// Returns `paths` sorted and without duplicates.
    fn sorted(paths: &[RepoPath]) -> Vec<&RepoPath> {
        let mut paths: Vec<_> = paths.iter().collect();
        paths.sort_unstable();
        paths.dedup();
        paths
    }

    /// Returns a hash of what the inputs declare. It is equal for statements
    /// that read the same paths from the same sources with the same definition
    /// and toolchain, in any order. It hashes the definition, the toolchain,
    /// the paths, then the target paths, each list as its `u32` count followed
    /// by each path's `u32` length and bytes.
    #[must_use]
    pub fn spec(&self) -> Digest {
        let mut hasher = blake3::Hasher::new_derive_key(Self::SPEC_CONTEXT);
        hasher.update(self.definition.as_bytes());
        hasher.update(self.toolchain.as_bytes());
        for (_, paths) in self.sorted_paths() {
            let mut encoded = Vec::new();
            Reader::write_len(paths.len(), &mut encoded);
            for path in paths {
                Reader::write_str(path.as_str(), &mut encoded);
            }
            hasher.update(&encoded);
        }
        Digest::from_bytes(*hasher.finalize().as_bytes())
    }

    /// Returns the input digest of a record. It hashes the [`Inputs::spec`],
    /// then for each path and then each target path, in their order, a 0 byte
    /// if nothing is there, or a 1 byte followed by the entry's kind and id.
    /// `entry_at` looks up what the record's snapshot, or its target's state,
    /// stores at a path. At the root, that is the whole tree.
    ///
    /// Records with equal digests agree on everything the inputs cover, so a
    /// promotable statement about one holds for the other.
    ///
    /// # Errors
    ///
    /// Fails if `entry_at` fails.
    pub fn digest<E>(
        &self,
        mut entry_at: impl FnMut(InputSource, &RepoPath) -> Result<Option<TreeEntry>, E>,
    ) -> Result<Digest, E> {
        let mut hasher = blake3::Hasher::new_derive_key(Self::DIGEST_CONTEXT);
        hasher.update(self.spec().as_bytes());
        for (source, paths) in self.sorted_paths() {
            for path in paths {
                match entry_at(source, path)? {
                    None => hasher.update(&[0]),
                    Some(entry) => hasher
                        .update(&[1, entry.kind as u8])
                        .update(entry.id.as_bytes()),
                };
            }
        }
        Ok(Digest::from_bytes(*hasher.finalize().as_bytes()))
    }
}

/// Where the input digest reads a path of [`Inputs`] from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSource {
    /// The record's snapshot: [`Inputs::paths`].
    Snapshot,
    /// The state of the target the record is on: [`Inputs::target_paths`].
    Target,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::convert::Infallible;

    use super::*;
    use crate::{EncodedObject, EntryKind};

    fn inputs(paths: &[&str], definition: &str) -> Inputs {
        Inputs {
            paths: paths
                .iter()
                .map(|path| RepoPath::new(path).expect("the name is valid"))
                .collect(),
            target_paths: Vec::new(),
            definition: Digest::of(definition.as_bytes()),
            toolchain: Digest::of(b"rustc 1.99.0"),
            external: false,
        }
    }

    /// Returns the entries of `files`, by path.
    fn entries(files: &[(&str, &str)]) -> HashMap<RepoPath, TreeEntry> {
        files
            .iter()
            .map(|&(path, content)| {
                let entry = TreeEntry {
                    kind: EntryKind::File,
                    id: EncodedObject::blob(content.as_bytes()).id(),
                };
                (RepoPath::new(path).expect("the name is valid"), entry)
            })
            .collect()
    }

    /// Returns the digest of `inputs` over a snapshot holding `files`, on a
    /// target holding `target`.
    fn digest_on(inputs: &Inputs, files: &[(&str, &str)], target: &[(&str, &str)]) -> Digest {
        let (files, target) = (entries(files), entries(target));
        inputs
            .digest(|source, path| {
                let tree = match source {
                    InputSource::Snapshot => &files,
                    InputSource::Target => &target,
                };
                Ok::<_, Infallible>(tree.get(path).copied())
            })
            .unwrap()
    }

    /// The digest of `inputs` over a snapshot holding `files`.
    fn digest(inputs: &Inputs, files: &[(&str, &str)]) -> Digest {
        digest_on(inputs, files, &[])
    }

    #[test]
    fn digests_cover_exactly_the_inputs() {
        let lint = inputs(&["src", "Cargo.toml"], "cargo clippy");
        let base = digest(&lint, &[("src", "v1"), ("Cargo.toml", "a"), ("docs", "x")]);
        assert_eq!(
            digest(&lint, &[("src", "v1"), ("Cargo.toml", "a"), ("docs", "y")]),
            base,
            "docs isn't an input"
        );
        assert_ne!(
            digest(&lint, &[("src", "v2"), ("Cargo.toml", "a")]),
            base,
            "src is"
        );
        assert_ne!(
            digest(&lint, &[("src", "v1")]),
            base,
            "a missing path differs from any content"
        );
        let reordered = inputs(&["Cargo.toml", "src", "src"], "cargo clippy");
        assert_eq!(
            reordered.spec(),
            lint.spec(),
            "order and repeats don't matter"
        );
        assert_eq!(
            digest(&reordered, &[("src", "v1"), ("Cargo.toml", "a")]),
            base
        );

        let test = inputs(&["src", "Cargo.toml"], "cargo test");
        assert_ne!(test.spec(), lint.spec());
        assert_ne!(digest(&test, &[("src", "v1"), ("Cargo.toml", "a")]), base);
    }

    #[test]
    fn target_paths_are_read_from_the_target() {
        let checks = Inputs {
            target_paths: vec![RepoPath::new("checks.toml").unwrap()],
            ..inputs(&["src"], "cargo test")
        };
        let files = [("src", "v1"), ("checks.toml", "old")];
        let base = digest_on(&checks, &files, &[("checks.toml", "new")]);
        assert_ne!(
            digest_on(&checks, &files, &[("checks.toml", "newer")]),
            base,
            "the target's checks are an input"
        );
        assert_eq!(
            digest_on(&checks, &[("src", "v1")], &[("checks.toml", "new")]),
            base,
            "the snapshot's aren't"
        );
        assert_eq!(
            digest_on(&checks, &files, &[("checks.toml", "new"), ("src", "v2")]),
            base,
            "nor is the rest of the target"
        );
        let on_target = Inputs {
            target_paths: vec![RepoPath::new("checks.toml").unwrap()],
            ..inputs(&[], "cargo test")
        };
        assert_ne!(
            inputs(&["checks.toml"], "cargo test").spec(),
            on_target.spec(),
            "a path read from the target isn't one read from the snapshot"
        );
    }

    #[test]
    fn kinds_are_validated() {
        for valid in ["ci.test", "review", "semantic.diff", "a_b.c-d.e1"] {
            assert!(Kind::new(valid).is_ok(), "{valid:?}");
        }
        for invalid in [
            "", "ci.", ".ci", "ci..test", "CI.test", "ci test", "ci/test",
        ] {
            assert!(Kind::new(invalid).is_err(), "{invalid:?}");
        }
        for reserved in [Kind::ABANDON, Kind::CONFLICT, Kind::DIVERGENT] {
            assert!(Kind::is_valid(reserved.as_str()), "{reserved:?}");
            assert!(reserved.is_reserved(), "{reserved:?}");
        }
        assert!(
            !Kind::new("corex.test")
                .expect("the name is valid")
                .is_reserved()
        );
    }

    #[test]
    fn verdicts_parse_as_they_display() {
        for verdict in [Verdict::Pass, Verdict::Fail, Verdict::Neutral] {
            assert_eq!(
                verdict
                    .to_string()
                    .parse::<Verdict>()
                    .expect("the text parses"),
                verdict
            );
        }
        assert!("Pass".parse::<Verdict>().is_err());
    }
}
