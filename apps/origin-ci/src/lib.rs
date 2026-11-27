//! `origin-ci`, origin's CI plugin. It runs the checks a revision's
//! `checks.toml` lists and issues their results as statements, which the
//! landing queue takes as evidence.
//!
//! As a tool, `origin ci run` checks a revision of the repository it runs
//! in, as the user, and `origin ci exec` runs a given command as a check
//! the same way. It talks to origin only through the public API: the SDK
//! over HTTP, or for a repository on a local path, the same service
//! in-process.

pub mod checks;
pub mod runner;
pub mod state;
