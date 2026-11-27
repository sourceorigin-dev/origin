//! `origin-ci`, origin's CI plugin. It runs the checks a revision's
//! `checks.toml` lists and issues their results as statements, which the
//! landing queue takes as evidence.

pub mod checks;
pub mod state;
