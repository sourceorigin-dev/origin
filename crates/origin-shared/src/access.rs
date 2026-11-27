//! What a caller may do: the capabilities a policy grants and tokens narrow,
//! the [`Rights`] they resolve to, and the scoped [`Token`]s a server signs.
//!
//! A capability is written `read src`, `write src`, `annotate ci.*`, `land main`,
//! `land *` or `admin`. The design's "Permissions and credentials" section says
//! what each allows.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::id::hex;
use crate::object::{DecodeError, Field, Reader, Signature, fields};
use crate::policy::KindPattern;
use crate::{
    InvalidName, Kind, NodeKey, NodePublicKey, Principal, PrincipalName, RepoPath, TargetName,
    TreeEntry,
};

/// What a caller may do with a path.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// No access: the content is withheld.
    None,
    /// Read the content.
    Read,
    /// Read the content, and propose revisions that change it.
    Write,
}

impl FromStr for Level {
    type Err = String;

    fn from_str(level: &str) -> Result<Self, Self::Err> {
        match level {
            "none" => Ok(Self::None),
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            _ => Err(format!("`{level}` is not `none`, `read` or `write`")),
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::None => "none",
            Self::Read => "read",
            Self::Write => "write",
        })
    }
}

/// What a caller may do with every path. A path gets the level of the most
/// specific listed path at or above it. The root is always listed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(
    try_from = "BTreeMap<RepoPath, Level>",
    into = "BTreeMap<RepoPath, Level>"
)]
#[schema(value_type = BTreeMap<String, Level>)]
pub struct PathAccess(BTreeMap<RepoPath, Level>);

impl PathAccess {
    /// Grants the same level everywhere.
    #[must_use]
    pub fn uniform(level: Level) -> Self {
        Self([(RepoPath::root(), level)].into())
    }

    /// Grants `default` everywhere except under the paths of `rules`, where the
    /// most specific rule decides.
    #[must_use]
    pub fn new(default: Level, rules: impl IntoIterator<Item = (RepoPath, Level)>) -> Self {
        let mut access = Self::uniform(default);
        access.0.extend(rules);
        access
    }

    /// Returns the level at `path`.
    #[must_use]
    pub fn level(&self, path: &RepoPath) -> Level {
        self.0
            .iter()
            .filter(|(rule, _)| path.starts_with(rule))
            .max_by_key(|(rule, _)| rule.as_str().len())
            .map_or(Level::None, |(_, &level)| level)
    }

    /// Returns whether the caller may read `path`.
    #[must_use]
    pub fn reads(&self, path: &RepoPath) -> bool {
        self.level(path) >= Level::Read
    }

    /// Returns whether the caller may write `path`.
    #[must_use]
    pub fn writes(&self, path: &RepoPath) -> bool {
        self.level(path) >= Level::Write
    }

    /// Returns the level of everything at and under `path`, if it is the same
    /// everywhere there, that is, if no rule is under `path`.
    #[must_use]
    pub fn settled(&self, path: &RepoPath) -> Option<Level> {
        let below = self
            .0
            .keys()
            .any(|rule| rule != path && rule.starts_with(path));
        (!below).then(|| self.level(path))
    }

    /// Returns whether the level is at least `level` at `path` and everywhere
    /// under it.
    #[must_use]
    pub fn everywhere_under(&self, path: &RepoPath, level: Level) -> bool {
        self.level(path) >= level
            && self
                .0
                .iter()
                .all(|(rule, &at)| !rule.starts_with(path) || at >= level)
    }

    /// Returns whether the caller may read `path` or something under it, such
    /// as a directory on the way to what they may read.
    #[must_use]
    pub fn reads_under(&self, path: &RepoPath) -> bool {
        self.reads(path)
            || self
                .0
                .iter()
                .any(|(rule, &level)| level >= Level::Read && rule.starts_with(path))
    }

    /// Returns whether the caller may read the entry at `path`: a file they may
    /// read, or a directory with something under it they may read.
    #[must_use]
    pub fn reads_entry(&self, path: &RepoPath, entry: &TreeEntry) -> bool {
        if entry.kind.is_tree() {
            self.reads_under(path)
        } else {
            self.reads(path)
        }
    }

    /// Returns whether the caller may read everything.
    #[must_use]
    pub fn reads_all(&self) -> bool {
        self.0.values().all(|&level| level >= Level::Read)
    }

    /// Returns whether the caller may write everything.
    #[must_use]
    pub fn writes_all(&self) -> bool {
        self.0.values().all(|&level| level >= Level::Write)
    }

    /// Returns what both `self` and `other` allow, path by path.
    #[must_use]
    pub fn min(&self, other: &Self) -> Self {
        let paths = self.0.keys().chain(other.0.keys());
        Self(
            paths
                .map(|path| (path.clone(), self.level(path).min(other.level(path))))
                .collect(),
        )
    }

    /// Returns the listed paths and their levels, sorted by path.
    pub fn rules(&self) -> impl Iterator<Item = (&RepoPath, Level)> {
        self.0.iter().map(|(path, &level)| (path, level))
    }
}

impl TryFrom<BTreeMap<RepoPath, Level>> for PathAccess {
    type Error = &'static str;

    fn try_from(rules: BTreeMap<RepoPath, Level>) -> Result<Self, Self::Error> {
        if rules.contains_key(&RepoPath::root()) {
            Ok(Self(rules))
        } else {
            Err("path access must list the root, `/`")
        }
    }
}

impl From<PathAccess> for BTreeMap<RepoPath, Level> {
    fn from(access: PathAccess) -> Self {
        access.0
    }
}

/// The targets a `land` capability covers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Targets {
    /// `*`: every target except `policy`.
    Any,
    /// One target, which is never `policy`.
    One(TargetName),
}

impl Targets {
    /// Returns whether this covers `target`.
    #[must_use]
    pub fn covers(&self, target: &TargetName) -> bool {
        match self {
            Self::Any => *target != TargetName::POLICY,
            Self::One(one) => one == target,
        }
    }

    /// Returns the targets both cover, if any.
    fn and(&self, other: &Self) -> Option<Self> {
        match (self, other) {
            (Self::Any, narrower) | (narrower, Self::Any) => Some(narrower.clone()),
            (Self::One(a), Self::One(b)) => (a == b).then(|| self.clone()),
        }
    }
}

impl FromStr for Targets {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match format!("land {input}").parse()? {
            Capability::Land(targets) => Ok(targets),
            _ => unreachable!("`land` parses as a land capability"),
        }
    }
}

impl fmt::Display for Targets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("*"),
            Self::One(target) => target.fmt(f),
        }
    }
}

/// One capability, written as the module's table shows. A path holds no
/// control characters, so a capability is always one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Capability {
    /// Reading the content under a path.
    Read(RepoPath),
    /// Proposing revisions that change a path, and reading it.
    Write(RepoPath),
    /// Issuing statements of matching kinds.
    Annotate(KindPattern),
    /// Submitting to target queues.
    Land(Targets),
    /// Landing on `policy`.
    Admin,
}

impl FromStr for Capability {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (verb, written) = input.split_once(' ').unwrap_or((input, ""));
        let argument = written.trim();
        let invalid = |reason: &dyn fmt::Display| format!("invalid capability `{input}`: {reason}");
        let path = || {
            if written.contains(char::is_control) {
                return Err(invalid(&"a path holds no control characters"));
            }
            RepoPath::new(written).map_err(|error| invalid(&error))
        };
        match verb {
            "admin" if argument.is_empty() => Ok(Self::Admin),
            "read" => path().map(Self::Read),
            "write" => path().map(Self::Write),
            "annotate" => argument
                .parse()
                .map(Self::Annotate)
                .map_err(|error: InvalidName| invalid(&error)),
            "land" if argument == "*" => Ok(Self::Land(Targets::Any)),
            "land" if argument == TargetName::POLICY.as_str() => {
                Err(invalid(&"landing on `policy` is `admin`"))
            }
            "land" => argument
                .parse()
                .map(|target| Self::Land(Targets::One(target)))
                .map_err(|error: InvalidName| invalid(&error)),
            _ => Err(invalid(
                &"expected `read <path>`, `write <path>`, `annotate <kind>`, \
                  `land <target>` or `admin`",
            )),
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(path) => write!(f, "read {path}"),
            Self::Write(path) => write!(f, "write {path}"),
            Self::Annotate(kinds) => write!(f, "annotate {kinds}"),
            Self::Land(targets) => write!(f, "land {targets}"),
            Self::Admin => f.write_str("admin"),
        }
    }
}

/// Encoded as the text it displays as.
impl Field for Capability {
    fn write(&self, out: &mut Vec<u8>) {
        Reader::write_str(&self.to_string(), out);
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        reader
            .prefixed_str()?
            .parse()
            .map_err(|_| DecodeError::NonCanonical("capability"))
    }
}

/// What a caller may do, resolved from the policy in force and narrowed by
/// the scope of their token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Rights {
    /// Who the caller is. `None` for an anonymous caller and for the node's own
    /// user.
    pub principal: Option<Principal>,
    /// The policy's groups the caller is a member of.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<PrincipalName>,
    /// The token's scope, if the caller used a scoped token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<Capability>>,
    /// What the caller may do with each path.
    pub paths: PathAccess,
    /// The kinds of statements the caller may issue.
    pub annotate: Vec<KindPattern>,
    /// The targets the caller may land on, not counting `policy`.
    pub land: Vec<Targets>,
    /// Whether the caller may land on `policy`.
    pub admin: bool,
}

impl Rights {
    /// Returns every right, as the node's own user has.
    #[must_use]
    pub fn all() -> Self {
        Self {
            principal: None,
            groups: Vec::new(),
            scope: None,
            paths: PathAccess::uniform(Level::Write),
            annotate: vec![KindPattern::Any],
            land: vec![Targets::Any],
            admin: true,
        }
    }

    /// Returns whether the caller may issue statements of `kind`.
    #[must_use]
    pub fn may_annotate(&self, kind: &Kind) -> bool {
        self.annotate.iter().any(|pattern| pattern.matches(kind))
    }

    /// Returns whether the caller may land on `target`. Landing on `policy`
    /// takes `admin`.
    #[must_use]
    pub fn may_land(&self, target: &TargetName) -> bool {
        if *target == TargetName::POLICY {
            return self.admin;
        }
        self.land.iter().any(|targets| targets.covers(target))
    }

    /// Returns these rights narrowed to what `scope` allows. A scope never adds
    /// a right.
    #[must_use]
    pub fn narrowed(self, scope: Vec<Capability>) -> Self {
        let mut paths = Vec::new();
        let (mut annotate, mut land, mut admin) = (Vec::new(), Vec::new(), false);
        for capability in &scope {
            match capability {
                Capability::Read(path) => paths.push((path.clone(), Level::Read)),
                Capability::Write(path) => paths.push((path.clone(), Level::Write)),
                Capability::Annotate(kinds) => annotate.extend(
                    self.annotate
                        .iter()
                        .filter_map(|granted| granted.and(kinds)),
                ),
                Capability::Land(targets) => {
                    land.extend(self.land.iter().filter_map(|granted| granted.and(targets)));
                }
                Capability::Admin => admin = self.admin,
            }
        }
        Self {
            paths: self.paths.min(&Self::scope_paths(&paths)),
            annotate,
            land,
            admin,
            scope: Some(scope),
            ..self
        }
    }

    /// Returns the path access that a scope's `read` and `write` capabilities
    /// grant on their own. Each path gets the highest level among the
    /// capabilities that cover it.
    fn scope_paths(granted: &[(RepoPath, Level)]) -> PathAccess {
        let level = |path: &RepoPath| {
            granted
                .iter()
                .filter(|(at, _)| path.starts_with(at))
                .map(|&(_, level)| level)
                .max()
                .unwrap_or(Level::None)
        };
        let rules: Vec<_> = granted
            .iter()
            .map(|(path, _)| (path.clone(), level(path)))
            .collect();
        PathAccess::new(level(&RepoPath::root()), rules)
    }

    /// Returns whether these rights include every capability of `scope`. A token
    /// issued with these rights may only narrow them.
    #[must_use]
    pub fn cover(&self, scope: &[Capability]) -> bool {
        scope.iter().all(|capability| match capability {
            Capability::Read(path) => self.paths.everywhere_under(path, Level::Read),
            Capability::Write(path) => self.paths.everywhere_under(path, Level::Write),
            Capability::Annotate(kinds) => self
                .annotate
                .iter()
                .any(|granted| granted.and(kinds).as_ref() == Some(kinds)),
            Capability::Land(targets) => self
                .land
                .iter()
                .any(|granted| granted.and(targets).as_ref() == Some(targets)),
            Capability::Admin => self.admin,
        })
    }
}

impl KindPattern {
    /// Returns the kinds both patterns match, if any. Two patterns are always
    /// either nested or disjoint.
    fn and(&self, other: &Self) -> Option<Self> {
        let covers = |wide: &Self, narrow: &Self| match (wide, narrow) {
            (Self::Any, _) => true,
            (Self::Prefix(_), Self::Any) | (Self::Exact(_), Self::Any | Self::Prefix(_)) => false,
            (Self::Prefix(prefix), Self::Prefix(kind)) => kind == prefix || wide.matches(kind),
            (Self::Prefix(_), Self::Exact(kind)) => wide.matches(kind),
            (Self::Exact(a), Self::Exact(b)) => a == b,
        };
        if covers(self, other) {
            Some(other.clone())
        } else if covers(other, self) {
            Some(self.clone())
        } else {
            None
        }
    }
}

/// A short-lived token that a server signs with its node key, so the server
/// can verify it without storing it. It holds who it acts for, what it may
/// do, and when it expires. It acts with the rights its principal has under
/// the policy in force, narrowed to its scope.
///
/// Written `origin1.<claims>.<signature>`, both in hex. The claims are the
/// fields in record encoding. The signature covers a domain prefix followed
/// by the claims, so no other signed bytes can pass for a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// Who it acts for.
    pub principal: Principal,
    /// What it may do.
    pub scope: Vec<Capability>,
    /// When it expires, in seconds since the Unix epoch.
    pub expires: u64,
}

fields!(Token {
    principal,
    scope,
    expires,
});

impl Token {
    /// The prefix of every token.
    const PREFIX: &str = "origin1.";

    /// The domain prefix the signature covers before the claims.
    const DOMAIN: &[u8] = b"origin token 1\0";

    /// Creates a token for `principal` with `scope`, valid for `ttl` from now.
    #[must_use]
    pub fn new(principal: Principal, scope: Vec<Capability>, ttl: Duration) -> Self {
        Self {
            principal,
            scope,
            expires: Self::now().saturating_add(ttl.as_secs()),
        }
    }

    /// Returns the current time in seconds since the Unix epoch, the unit
    /// expiry times use.
    #[must_use]
    pub fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs())
    }

    fn signed_bytes(claims: &[u8]) -> Vec<u8> {
        [Self::DOMAIN, claims].concat()
    }

    /// Signs the token with `key` and returns it as text.
    #[must_use]
    pub fn sign(&self, key: &NodeKey) -> String {
        let mut claims = Vec::new();
        self.write(&mut claims);
        let signature = key.sign(&Self::signed_bytes(&claims));
        format!(
            "{}{}.{}",
            Self::PREFIX,
            hex::string(&claims),
            hex::string(&signature.to_bytes())
        )
    }

    /// Returns whether `text` looks like a signed token rather than a static
    /// one.
    #[must_use]
    pub fn is_signed(text: &str) -> bool {
        text.starts_with(Self::PREFIX)
    }

    /// Returns the token in `text`, if `key` signed it and it hasn't expired.
    ///
    /// # Errors
    ///
    /// Fails if the token is malformed, `key` didn't sign it, or it expired.
    pub fn verify(text: &str, key: &NodePublicKey) -> Result<Self, InvalidToken> {
        let (claims, signature) = text
            .strip_prefix(Self::PREFIX)
            .and_then(|rest| rest.split_once('.'))
            .ok_or(InvalidToken::Malformed)?;
        let claims = hex::bytes(claims).ok_or(InvalidToken::Malformed)?;
        let signature = hex::decode::<{ ed25519_dalek::SIGNATURE_LENGTH }>(signature)
            .map(|bytes| Signature::from_bytes(&bytes))
            .ok_or(InvalidToken::Malformed)?;
        if !key.verifies(&Self::signed_bytes(&claims), &signature) {
            return Err(InvalidToken::Signature);
        }
        let mut reader = Reader::new(&claims);
        let token = Self::read(&mut reader)?;
        reader.finish()?;
        if token.expires <= Self::now() {
            return Err(InvalidToken::Expired);
        }
        Ok(token)
    }
}

/// Why a token failed to verify.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidToken {
    /// The text isn't a token.
    #[error("the token is malformed")]
    Malformed,
    /// The signature is valid, but the claims don't decode. A server of another
    /// version signed it.
    #[error("the token's claims are malformed")]
    Claims(#[from] DecodeError),
    /// This server didn't sign it.
    #[error("the token wasn't signed by this server")]
    Signature,
    /// The token expired.
    #[error("the token expired")]
    Expired,
    /// No user has this static token.
    #[error("the token is unknown")]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn capabilities(scope: &[&str]) -> Vec<Capability> {
        scope
            .iter()
            .map(|capability| capability.parse().unwrap())
            .collect()
    }

    /// Path access where everyone may read, except under `secrets`, which is
    /// withheld, while `secrets/public` may be read.
    fn access() -> PathAccess {
        PathAccess::new(
            Level::Read,
            [
                (path("secrets"), Level::None),
                (path("secrets/public"), Level::Read),
                (path("src"), Level::Write),
            ],
        )
    }

    #[test]
    fn the_most_specific_path_decides() {
        let access = access();
        assert_eq!(access.level(&path("README.md")), Level::Read);
        assert_eq!(access.level(&path("src/lib.rs")), Level::Write);
        assert_eq!(access.level(&path("secrets/key")), Level::None);
        assert_eq!(access.level(&path("secrets/public/a")), Level::Read);
        assert_eq!(access.level(&path("srcx")), Level::Read, "whole components");
        assert!(access.reads_under(&path("secrets")));
        assert!(!access.reads_under(&path("secrets/other")));
        assert_eq!(access.settled(&path("src")), Some(Level::Write));
        assert_eq!(access.settled(&path("secrets")), None);
        assert!(!access.reads_all() && !access.writes_all());
    }

    #[test]
    fn minimums_hold_path_by_path() {
        let scope = PathAccess::new(Level::None, [(path("src/net"), Level::Write)]);
        let both = access().min(&scope);
        assert_eq!(both.level(&path("src/net/a.rs")), Level::Write);
        assert_eq!(both.level(&path("src/lib.rs")), Level::None);
        assert_eq!(both.level(&path("secrets/public/a")), Level::None);
    }

    #[test]
    fn scopes_only_narrow() {
        let rights = Rights {
            principal: Some("user:ada".parse().unwrap()),
            groups: Vec::new(),
            scope: None,
            paths: access(),
            annotate: vec!["ci.*".parse().unwrap()],
            land: vec![Targets::One(TargetName::MAIN)],
            admin: false,
        };
        let narrowed = rights.clone().narrowed(capabilities(&[
            "write /",
            "annotate *",
            "annotate ci.test",
            "land *",
            "admin",
        ]));
        assert_eq!(
            narrowed.paths, rights.paths,
            "writing everything adds nothing"
        );
        assert!(narrowed.may_annotate(&Kind::new("ci.test").unwrap()));
        assert!(
            narrowed.may_annotate(&Kind::new("ci.lint").unwrap()),
            "`*` and `ci.*`"
        );
        assert!(!narrowed.may_annotate(&Kind::new("review").unwrap()));
        assert!(narrowed.may_land(&TargetName::MAIN));
        assert!(!narrowed.may_land(&TargetName::new("release").unwrap()));
        assert!(!narrowed.admin);

        let agent = rights.clone().narrowed(capabilities(&["write src"]));
        assert!(agent.paths.writes(&path("src/lib.rs")));
        assert!(!agent.paths.reads(&path("README.md")));
        assert!(agent.annotate.is_empty() && agent.land.is_empty());

        assert!(rights.cover(&capabilities(&[
            "write src",
            "read docs",
            "annotate ci.test",
            "land main"
        ])));
        for wider in [
            "write /",
            "read secrets",
            "annotate review",
            "annotate ci",
            "land *",
            "admin",
        ] {
            assert!(!rights.cover(&capabilities(&[wider])), "{wider}");
        }
    }

    #[test]
    fn capabilities_parse_as_they_display() {
        for valid in [
            "read /",
            "write src/net",
            "annotate ci.*",
            "land main",
            "land *",
            "admin",
            "read a ",
        ] {
            assert_eq!(valid.parse::<Capability>().unwrap().to_string(), valid);
        }
        for invalid in [
            "land policy",
            "admin x",
            "delete src",
            "annotate CI",
            "read a//b",
            "read x\nexpires 9",
            "write a\rb",
        ] {
            assert!(invalid.parse::<Capability>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn tokens_verify_only_against_their_signer_until_they_expire() {
        let key = NodeKey::from_bytes(&[3; 32]);
        let token = Token::new(
            "user:ada".parse().unwrap(),
            capabilities(&["write src", "land main"]),
            Duration::from_mins(5),
        );
        let text = token.sign(&key);
        assert!(Token::is_signed(&text));
        assert_eq!(Token::verify(&text, &key.public()), Ok(token.clone()));
        let other = NodeKey::from_bytes(&[4; 32]).public();
        assert_eq!(Token::verify(&text, &other), Err(InvalidToken::Signature));

        let forged = text.replacen("origin1.01", "origin1.02", 1);
        assert_ne!(forged, text);
        assert_eq!(
            Token::verify(&forged, &key.public()),
            Err(InvalidToken::Signature)
        );
        assert_eq!(
            Token::verify("origin1.zz", &key.public()),
            Err(InvalidToken::Malformed)
        );

        let expired = Token {
            expires: Token::now() - 1,
            ..token
        };
        assert_eq!(
            Token::verify(&expired.sign(&key), &key.public()),
            Err(InvalidToken::Expired)
        );
    }
}
