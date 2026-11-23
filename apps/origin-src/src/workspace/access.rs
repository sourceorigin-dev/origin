//! What the remote lets the user do: the `whoami`, `permissions` and `token
//! issue` commands, and withholding the paths the user may not read.
//!
//! Every clone and fetch saves the user's rights on the remote in the
//! repository's `.origin/access/`, keyed by who the remote says they are and
//! their token's scope, so workspaces sharing a repository never check their
//! work against each other's rights. Paths the user may not read are withheld
//! (see [`Store::with_withheld`]): their content never reaches the repository
//! or the working copy. A push that changes a path the user may not write
//! fails before sending anything. `docs/design.md` has the rest.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use origin_shared::access::{Capability, PathAccess, Rights};
use origin_shared::api::TokenRequest;
use origin_shared::{Base, Digest, Period, RepoPath};

use super::{Workspace, WorkspaceError};
use crate::config::UserConfig;
use crate::remote::RemoteUrl;
use crate::store::Store;
use crate::tree::{FileChange, Snapshots};
use crate::worktree::Worktree;

impl Workspace {
    /// The directory in the repository's `.origin` that holds what each user who
    /// synced with the remote knows of it. Each user has a directory named by the
    /// hash of the remote's URL and their [holder name](Workspace::holder_name). It
    /// holds `rights`, their rights on the remote as JSON, and the remote's event
    /// cursor as of their last fetch, which skips the events their rights hide.
    /// `tokens/<hash>` holds the holder name of a token, by the hash of the
    /// remote's URL and the token, so a new token for the same user, with the same
    /// scope, keeps their cursor. `clone/rights` holds the rights of whoever cloned
    /// the repository, for the users who haven't synced yet.
    const ACCESS: &str = "access";

    /// The directory in [`Workspace::ACCESS`] that names the holder of each token.
    const TOKENS: &str = "tokens";

    /// Returns the file in [`Workspace::ACCESS`] that names who the remote at `url`
    /// says the user of `config`, with their token, is.
    fn token_file(store: &Store, url: &RemoteUrl, config: &UserConfig) -> PathBuf {
        let token = url.token(config).unwrap_or_default();
        let token = Digest::of(format!("{url}\n{token}").as_bytes());
        let tokens = store.root().join(Self::ACCESS).join(Self::TOKENS);
        tokens.join(token.to_hex())
    }

    /// Returns the name [`Workspace::ACCESS`] gives the user who has `rights`: who
    /// the remote says they are, or `anonymous`, followed by their token's scope,
    /// sorted, if the token is scoped.
    fn holder_name(rights: &Rights) -> String {
        let principal = rights.principal.as_ref();
        let mut name = principal.map_or_else(|| "anonymous".to_owned(), ToString::to_string);
        if let Some(scope) = &rights.scope {
            let mut scope: Vec<_> = scope.iter().map(ToString::to_string).collect();
            scope.sort_unstable();
            scope.dedup();
            name = format!("{name} [{}]", scope.join(", "));
        }
        name
    }

    /// Returns the directory in [`Workspace::ACCESS`] of the user named `name` (see
    /// [`Workspace::holder_name`]), on the remote at `url`.
    fn principal_dir(store: &Store, url: &RemoteUrl, name: &str) -> PathBuf {
        let user = Digest::of(format!("{url}\n{name}").as_bytes());
        store.root().join(Self::ACCESS).join(user.to_hex())
    }

    /// Returns the directory in [`Workspace::ACCESS`] of the user of `config`, for
    /// the remote at `url`, once the remote said who they are.
    fn user_dir(
        store: &Store,
        url: &RemoteUrl,
        config: &UserConfig,
    ) -> Result<Option<PathBuf>, WorkspaceError> {
        let path = Self::token_file(store, url, config);
        match fs::read_to_string(&path) {
            Ok(name) => Ok(Some(Self::principal_dir(store, url, name.trim_end()))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(WorkspaceError::io(&path, source)),
        }
    }

    /// Returns the file named `name` in the [`Workspace::ACCESS`] directory of the
    /// user of `config`, if the repository has a remote and the user synced with
    /// it.
    pub(super) fn user_file(
        store: &Store,
        config: &UserConfig,
        name: &str,
    ) -> Result<Option<PathBuf>, WorkspaceError> {
        let Some(url) = Self::remote_url(store)? else {
            return Ok(None);
        };
        Ok(Self::user_dir(store, &url, config)?.map(|dir| dir.join(name)))
    }

    /// Returns the rights a file of [`Workspace::ACCESS`] holds, if the file
    /// exists.
    fn read_access(path: &Path) -> Result<Option<Rights>, WorkspaceError> {
        let content = match fs::read(path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(WorkspaceError::io(path, source)),
        };
        serde_json::from_slice(&content)
            .map(Some)
            .map_err(|_| WorkspaceError::Malformed(path.to_owned()))
    }

    /// Returns the rights of the user of `config` on the remote, as of their last
    /// sync, if they synced with it.
    fn own_access(store: &Store, config: &UserConfig) -> Result<Option<Rights>, WorkspaceError> {
        match Self::user_file(store, config, "rights")? {
            Some(file) => Self::read_access(&file),
            None => Ok(None),
        }
    }

    /// Returns the rights of the user of `config` on the remote, as of their last
    /// sync, or else as of the clone, if the repository has a remote.
    pub(super) fn access(
        store: &Store,
        config: &UserConfig,
    ) -> Result<Option<Rights>, WorkspaceError> {
        if Self::remote_url(store)?.is_none() {
            return Ok(None);
        }
        match Self::own_access(store, config)? {
            Some(rights) => Ok(Some(rights)),
            None => Self::read_access(&store.root().join(Self::ACCESS).join("clone/rights")),
        }
    }

    /// Returns the user's rights on the remote that a push is checked against: as
    /// of their last sync, or, if they never synced, as the remote says now. Never
    /// the clone's rights, since another user may have made the clone.
    pub(super) fn push_access(&mut self) -> Result<Option<Rights>, WorkspaceError> {
        if Self::remote_url(&self.store)?.is_some()
            && Self::own_access(&self.store, &self.config)?.is_none()
        {
            self.refresh_access()?;
        }
        Self::own_access(&self.store, &self.config)
    }

    /// Returns `store`, withholding what the remote withholds from the user of
    /// `config`.
    pub(super) fn withholding(store: Store, config: &UserConfig) -> Result<Store, WorkspaceError> {
        Ok(match Self::access(&store, config)? {
            Some(rights) => store.with_withheld(rights.paths),
            None => store,
        })
    }

    /// Saves the rights of the user of `config` on the remote at `url` in `store`,
    /// and also as the clone's rights if `cloning`.
    #[expect(clippy::expect_used, reason = "Rights always serialize to JSON.")]
    pub(super) fn save_access(
        store: &Store,
        url: &RemoteUrl,
        config: &UserConfig,
        rights: &Rights,
        cloning: bool,
    ) -> Result<(), WorkspaceError> {
        let json = serde_json::to_vec_pretty(rights).expect("rights serialize to JSON");
        let name = Self::holder_name(rights);
        let write = store.durable_write();
        let mut dirs = vec![Self::principal_dir(store, url, &name)];
        if cloning {
            dirs.push(store.root().join(Self::ACCESS).join("clone"));
        }
        dirs.into_iter()
            .try_for_each(|dir| write.stage(dir.join("rights"), &json))
            .and_then(|()| {
                let file = Self::token_file(store, url, config);
                write.stage(file, format!("{name}\n").as_bytes())
            })
            .and_then(|()| write.commit())
            .map_err(crate::store::StoreError::from)?;
        Ok(())
    }

    /// Saves the user's rights on the remote, and withholds what they may not read
    /// from now on. Restores the working copy's files that they can read now but
    /// couldn't before.
    pub(super) fn refresh_access(&mut self) -> Result<(), WorkspaceError> {
        let rights = self.require_remote()?.whoami()?;
        let url = Self::remote_url(&self.store)?.ok_or(WorkspaceError::NoRemote)?;
        let name = Self::holder_name(&rights);
        let dir = Self::principal_dir(&self.store, &url, &name);
        let saved = Self::read_access(&dir.join("rights"))?;
        let withheld = Self::access(&self.store, &self.config)?;
        let known = Self::user_dir(&self.store, &url, &self.config)?.as_ref() == Some(&dir);
        if known && saved.as_ref() == Some(&rights) {
            return Ok(());
        }
        Self::save_access(&self.store, &url, &self.config, &rights, false)?;
        if saved.as_ref() != Some(&rights) {
            self.forget_cursor()?;
        }
        if withheld.as_ref() == Some(&rights) {
            return Ok(());
        }
        self.store = self.store.clone().with_withheld(rights.paths.clone());
        match withheld {
            Some(old) => self.restore_readable(&old.paths, &rights.paths),
            None => Ok(()),
        }
    }

    /// Fetches and writes the working copy's files under the paths that `now` can
    /// read and `before` couldn't.
    fn restore_readable(
        &self,
        before: &PathAccess,
        now: &PathAccess,
    ) -> Result<(), WorkspaceError> {
        let mut paths: Vec<RepoPath> = before
            .rules()
            .chain(now.rules())
            .map(|(path, _)| path.clone())
            .filter(|path| now.reads(path) && !before.reads(path))
            .collect();
        paths.sort_unstable();
        paths.dedup_by(|inner, outer| inner.starts_with(outer));
        if paths.is_empty() {
            return Ok(());
        }
        let snapshot = self.store.get(self.revision)?.snapshot;
        let mut base = self.store.get(self.revision)?.base;
        while let Base::Revision(below) = base {
            base = self.store.get(below)?.base;
        }
        self.sync(self.require_remote()?)
            .need_paths(base.into(), paths.clone())?;
        let mut restored = Vec::new();
        for path in &paths {
            let at = Snapshots::new(&self.store).entry_at(snapshot, path)?;
            let (tree, file) = match at {
                Some(entry) => (entry.subtree(), entry.file()),
                None => continue,
            };
            if let Some(file) = file {
                restored.push(FileChange {
                    path: path.clone(),
                    old: None,
                    new: Some(file),
                });
            }
            restored.extend(self.store.diff_at(path, None, tree)?);
        }
        restored
            .retain(|change| fs::symlink_metadata(self.root.join(change.path.as_str())).is_err());
        Ok(Worktree::new(&self.root, &self.store).checkout(&restored, &[], None)?)
    }

    /// `origin whoami`: prints who the remote says the user is.
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

    /// `origin token issue`: asks the remote for a token that acts for the user,
    /// with `scope`, valid for `ttl`, and prints it.
    ///
    /// # Errors
    ///
    /// Fails if there's no remote, it cannot be reached, or it refuses: the scope
    /// must be within the user's rights.
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

#[cfg(test)]
mod tests {
    use origin_shared::access::Level;

    use super::*;

    #[test]
    fn rights_round_trip_through_the_access_file() {
        let rights = Rights {
            paths: PathAccess::new(
                Level::Read,
                [(RepoPath::new("secrets").unwrap(), Level::None)],
            ),
            ..Rights::all()
        };
        let json = serde_json::to_vec(&rights).unwrap();
        assert_eq!(serde_json::from_slice::<Rights>(&json).unwrap(), rights);
    }
}
