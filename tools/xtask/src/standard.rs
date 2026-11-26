//! `cargo xtask standard`: the rules of the engineering standard, which the
//! `sourceorigin-standard` crate checks. Its README lists them; `standard.toml`
//! and `budget.toml` hold origin's settings, deviations and exception budgets.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use sourceorigin_standard::rules;

/// What `cargo xtask standard` does.
pub enum Standard {
    /// Checks the rules, after making the fixes that need no judgement if
    /// `fix` is set.
    Check {
        /// Whether to fix first.
        fix: bool,
    },
    /// Prints each rule's violations and deviation as JSON.
    Report,
    /// Lists the rules.
    Rules,
    /// Writes the standard's shared skills into `.agents/skills`.
    Skills,
}

impl Standard {
    /// Parses the arguments after `standard`.
    ///
    /// # Errors
    ///
    /// Fails on anything but `--fix`, `report`, `rules` or `skills`.
    pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let standard = match args.next().as_deref() {
            None => Self::Check { fix: false },
            Some("--fix") => Self::Check { fix: true },
            Some("report") => Self::Report,
            Some("rules") => Self::Rules,
            Some("skills") => Self::Skills,
            Some(other) => bail!("unknown argument {other}: {}", Self::USAGE),
        };
        if let Some(extra) = args.next() {
            bail!("unexpected argument {extra}: {}", Self::USAGE);
        }
        Ok(standard)
    }

    const USAGE: &str = "usage: cargo xtask standard [--fix | report | rules | skills]";

    /// Runs on the repository at `root`, printing what it finds or fixes.
    /// Fails the exit code if a rule is violated.
    ///
    /// # Errors
    ///
    /// Fails if the repository cannot be read or written.
    pub fn run(self, root: &Path) -> Result<ExitCode> {
        match self {
            Self::Check { fix } => {
                if fix {
                    for fixed in rules::fix(root).context("fix the standard's rules")? {
                        println!("fixed {fixed}");
                    }
                }
                let violations = rules::check(root).context("check the standard's rules")?;
                for violation in &violations {
                    eprintln!("{violation}");
                }
                if !violations.is_empty() {
                    return Ok(ExitCode::FAILURE);
                }
            }
            Self::Report => {
                let report = rules::report(root).context("report on the standard's rules")?;
                println!("{}", report.to_json().context("serialize the report")?);
            }
            Self::Rules => print!("{}", rules::describe()),
            Self::Skills => {
                for synced in rules::sync_skills(root).context("sync the standard's skills")? {
                    println!("synced {synced}");
                }
            }
        }
        Ok(ExitCode::SUCCESS)
    }
}
