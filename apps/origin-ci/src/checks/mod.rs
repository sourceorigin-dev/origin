//! `checks.toml`, at the root of a snapshot, lists the checks to run on
//! it. A worker reads it from the tip of a target, and runs those checks
//! on what lands there.
//!
//! ```toml
//! [checks.lint]
//! command   = "cargo clippy --workspace -- -D warnings"
//! inputs    = ["src", "Cargo.toml", "Cargo.lock"]
//! toolchain = "rustc -V"
//!
//! [checks.links]
//! command  = "lychee docs"
//! inputs   = ["docs"]
//! external = true          # reads the network: never promoted
//! ```
//!
//! Each check issues statements of the kind `ci.<name>`. A result is promoted
//! to every revision with the same `inputs` (default: the whole snapshot),
//! command and `toolchain` output, unless the check is `external`.
//! `checks.toml` is itself always an input, read from where the checks are in
//! force (see [`Defined`]), so results of different definitions never match.

use std::collections::BTreeMap;

use origin_shared::{Digest, Inputs, InvalidName, Kind, RepoPath};
use serde::Deserialize;

/// Where the `checks.toml` that defines a check is read from. Its results
/// declare that file among their inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Defined {
    /// From the snapshot being checked, as `origin ci run` reads it: one of
    /// the paths read from the snapshot.
    InSnapshot,
    /// From the target the checked record is on, as a worker reads it: one of
    /// the paths read from the target's state.
    OnTarget,
}

/// The checks of a snapshot, by kind.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "ChecksFile")]
pub struct Checks(BTreeMap<Kind, Check>);

/// One check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// The kind of the statements it issues: `ci.<name>`.
    pub kind: Kind,
    /// The shell command that runs the check in a checkout of the snapshot.
    /// The check passes if it exits with 0.
    pub command: String,
    /// The files and directories it reads. For a check
    /// [defined](Defined::InSnapshot) in the snapshot, this includes
    /// `checks.toml`.
    pub inputs: Vec<RepoPath>,
    /// The paths it reads from the state of the target the record is on. For
    /// a check [defined](Defined::OnTarget) there, this is `checks.toml`.
    pub target_inputs: Vec<RepoPath>,
    /// The shell command whose output identifies its tools, if any.
    pub toolchain: Option<String>,
    /// Whether it also reads something outside the snapshot, such as the
    /// network. Results of such a check are never promoted.
    pub external: bool,
}

impl Checks {
    /// The file holding the checks, at the root of a snapshot.
    pub const FILE: &str = "checks.toml";

    /// The namespace of the kinds checks issue.
    pub const NAMESPACE: &str = "ci";

    /// Parses `checks.toml`, read from where `defined` says. Each check
    /// declares the file among its inputs.
    ///
    /// # Errors
    ///
    /// Fails if the file isn't valid, or a check's name doesn't make a valid
    /// kind.
    pub fn parse(text: &str, defined: Defined) -> Result<Self, toml::de::Error> {
        let mut checks: Self = toml::from_str(text)?;
        let file = Self::path();
        for check in checks.0.values_mut() {
            match defined {
                Defined::InSnapshot
                    if !check.inputs.iter().any(|input| file.starts_with(input)) =>
                {
                    check.inputs.push(file.clone());
                }
                Defined::InSnapshot => {}
                Defined::OnTarget => check.target_inputs.push(file.clone()),
            }
        }
        Ok(checks)
    }

    /// Returns [`Checks::FILE`] as a path.
    #[expect(clippy::expect_used, reason = "`checks.toml` is a valid path.")]
    fn path() -> RepoPath {
        RepoPath::new(Self::FILE).expect("a valid path")
    }

    /// Returns the check that issues `kind`, if there is one.
    #[must_use]
    pub fn get(&self, kind: &Kind) -> Option<&Check> {
        self.0.get(kind)
    }

    /// Iterates over every check, by kind.
    pub fn iter(&self) -> impl Iterator<Item = &Check> {
        self.0.values()
    }

    /// Returns the kind of the statements the check `name` issues:
    /// `ci.<name>`.
    ///
    /// # Errors
    ///
    /// Fails if the name doesn't make a valid kind.
    pub fn kind(name: &str) -> Result<Kind, InvalidName> {
        Kind::new(format!("{}.{name}", Self::NAMESPACE))
    }
}

impl Check {
    /// Returns the inputs a result of the check declares it depended on,
    /// including the output of its toolchain command, if it has one.
    #[must_use]
    pub fn inputs(&self, toolchain: &[u8]) -> Inputs {
        Inputs {
            paths: self.inputs.clone(),
            target_paths: self.target_inputs.clone(),
            definition: Digest::of(self.command.as_bytes()),
            toolchain: Digest::of(toolchain),
            external: self.external,
        }
    }
}

/// `checks.toml` as written, before parsing.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChecksFile {
    #[serde(default)]
    checks: BTreeMap<String, CheckFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckFile {
    command: String,
    inputs: Option<Vec<RepoPath>>,
    toolchain: Option<String>,
    #[serde(default)]
    external: bool,
}

impl TryFrom<ChecksFile> for Checks {
    type Error = InvalidName;

    fn try_from(file: ChecksFile) -> Result<Self, Self::Error> {
        file.checks
            .into_iter()
            .map(|(name, check)| {
                let kind = Self::kind(&name)?;
                let check = Check {
                    kind: kind.clone(),
                    command: check.command,
                    inputs: check.inputs.unwrap_or_else(|| vec![RepoPath::root()]),
                    target_inputs: Vec::new(),
                    toolchain: check.toolchain,
                    external: check.external,
                };
                Ok((kind, check))
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }
}

#[cfg(test)]
mod tests;
