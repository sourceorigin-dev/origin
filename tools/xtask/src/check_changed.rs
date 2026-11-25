//! `cargo xtask check-changed`: CI's quick checks, for only what changed since
//! the merge base with `origin/main`. The table below describes
//! `.github/workflows/ci.yml`: the filters of `.github/filters.yml` that start
//! each job, and the recipes it runs.

use sourceorigin_standard::check_changed::{self, Ci, Job, Recipe, Run, Trigger};

/// A filter of `.github/filters.yml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Filter {
    /// Rust sources and what builds or checks them.
    Rust,
    /// What cargo-deny reads.
    Deny,
}

impl check_changed::Filter for Filter {
    const ALL: &'static [Self] = &[Self::Rust, Self::Deny];

    fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Deny => "deny",
        }
    }
}

/// A recipe that `run`s as given.
const fn recipe(command: &'static [&'static str], run: Run) -> Recipe {
    Recipe { command, run }
}

/// The jobs of `.github/workflows/ci.yml`. It ignores `changes`, which runs the
/// filters, and `ci`, the summary.
pub const CI: Ci<Filter> = Ci {
    jobs: &[
        Job {
            id: "checks",
            trigger: Trigger::AnyOf(&[Filter::Rust, Filter::Deny]),
            recipes: &[
                recipe(&["just", "fmt-check"], Run::Workspace),
                recipe(&["just", "audit"], Run::Workspace),
                recipe(&["just", "unused"], Run::Workspace),
            ],
        },
        Job {
            id: "rust",
            trigger: Trigger::AnyOf(&[Filter::Rust]),
            recipes: &[
                recipe(
                    &["just", "lint"],
                    Run::Crates {
                        libraries_only: false,
                    },
                ),
                recipe(
                    &["just", "test"],
                    Run::Crates {
                        libraries_only: false,
                    },
                ),
                recipe(
                    &["just", "doc-test"],
                    Run::Crates {
                        libraries_only: true,
                    },
                ),
            ],
        },
        Job {
            id: "msrv",
            trigger: Trigger::AnyOf(&[Filter::Rust]),
            recipes: &[],
        },
        Job {
            id: "standard",
            trigger: Trigger::EveryChange,
            recipes: &[recipe(&["just", "standard"], Run::Workspace)],
        },
    ],
    ignored: &["changes", "ci"],
};

#[cfg(test)]
mod tests {
    use super::CI;

    /// Fails until the table and ci.yml change together.
    #[test]
    fn ci_matches_the_workflow() {
        CI.check_workflow(&crate::root())
            .expect("the CI table matches ci.yml and filters.yml");
    }
}
