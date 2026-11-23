//! The repositories a server hosts.

use std::collections::BTreeMap;

use origin_shared::api::{RepoName, RepoSummary};

use crate::service::{Caller, OriginService, ServiceError};

/// The repositories a server hosts, each served by the server's node and kept
/// open while the server runs.
#[derive(Debug, Default)]
pub struct Repositories {
    repos: BTreeMap<RepoName, OriginService>,
}

impl Repositories {
    /// Hosts `service` as `name`.
    pub fn insert(&mut self, name: RepoName, service: OriginService) {
        self.repos.insert(name, service);
    }

    /// Lists the repositories, by name.
    #[must_use]
    pub fn list(&self, _caller: &Caller) -> Vec<RepoSummary> {
        self.repos
            .keys()
            .map(|name| RepoSummary { name: name.clone() })
            .collect()
    }

    /// Returns the repository `name`.
    ///
    /// # Errors
    ///
    /// Fails if the server hosts no repository by that name.
    pub fn get(&self, _caller: &Caller, name: &RepoName) -> Result<&OriginService, ServiceError> {
        self.repos
            .get(name)
            .ok_or_else(|| ServiceError::UnknownRepository(name.clone()))
    }
}
