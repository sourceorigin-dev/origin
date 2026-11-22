//! What the remote lets the user do: `whoami`, `permissions` and `token
//! issue`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use origin_shared::Period;
use origin_shared::access::{Capability, Rights};
use origin_shared::api::TokenRequest;

use super::{Workspace, WorkspaceError};

impl Workspace {
    /// `origin whoami`: prints who the remote thinks the user is.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached.
    pub fn whoami(&self) -> Result<(), WorkspaceError> {
        let Some(remote) = self.remote()? else {
            println!("{}, the owner of this repository", self.config.user());
            return Ok(());
        };
        let rights = remote.whoami()?;
        match &rights.principal {
            None if rights.paths.writes_all() => {
                println!("The owner: the remote is a local repository.");
            }
            None => println!("Anonymous: set a token for the remote to sign in."),
            Some(principal) if rights.groups.is_empty() => println!("{principal}"),
            Some(principal) => {
                let groups: Vec<_> = rights.groups.iter().map(ToString::to_string).collect();
                println!("{principal} (groups: {})", groups.join(", "));
            }
        }
        if let Some(scope) = &rights.scope {
            let scope: Vec<_> = scope.iter().map(ToString::to_string).collect();
            println!("Token scope: {}", scope.join(", "));
        }
        Ok(())
    }

    /// `origin permissions`: prints what the user may do on the remote.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached.
    pub fn permissions(&self) -> Result<(), WorkspaceError> {
        let rights = match self.remote()? {
            Some(remote) => remote.whoami()?,
            None => Rights::all(),
        };
        for (path, level) in rights.paths.rules() {
            println!("{level:<5} {path}");
        }
        let list = |items: Vec<String>| {
            if items.is_empty() {
                "nothing".to_owned()
            } else {
                items.join(", ")
            }
        };
        println!(
            "annotate: {}",
            list(rights.annotate.iter().map(ToString::to_string).collect())
        );
        println!(
            "land: {}",
            list(rights.land.iter().map(ToString::to_string).collect())
        );
        println!("admin: {}", if rights.admin { "yes" } else { "no" });
        Ok(())
    }

    /// `origin token issue`: asks the remote for a token acting for the
    /// user, with `scope`, valid for `ttl`, and prints it.
    ///
    /// # Errors
    ///
    /// Fails if there's no remote, it cannot be reached, or it refuses:
    /// the scope must be part of the user's rights.
    pub fn issue_token(&self, scope: Vec<Capability>, ttl: Period) -> Result<(), WorkspaceError> {
        let issued = self
            .require_remote()?
            .issue_token(&TokenRequest { scope, ttl })?;
        let left = Duration::from_secs(issued.expires).saturating_sub(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default(),
        );
        println!("{}", issued.token);
        eprintln!(
            "Acts for {} for {} minutes: send it as ORIGIN_TOKEN, or add it to [tokens] in \
             config.toml",
            issued.principal,
            left.as_secs().div_ceil(60)
        );
        Ok(())
    }
}
