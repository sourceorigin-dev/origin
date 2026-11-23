//! Remotes: the repositories a node shares its work with.
//!
//! A remote is a server over HTTP, `http://host[:port]/<repository>`, or a
//! repository on a local path (a server's repository or a working copy's). This
//! node serves a local one with an in-process [`OriginService`], so both kinds
//! answer the same calls and [`sync`](crate::sync) treats them the same. Calls
//! block: an HTTP remote runs the SDK on its own runtime.

use std::fmt;
use std::path::{self, Path, PathBuf};

use origin_client::Client;
use origin_shared::Head;
use origin_shared::api::{
    ErrorCode, EventsQuery, FetchRecords, FetchSnapshot, Inventory, InventoryRequest, LandRequest,
    Landings, LogEntry, LogQuery, NewTarget, PutQuery, RepoName, Stored, TargetInfo,
};
use origin_shared::event::Page;
use origin_shared::landing::Pending;
use origin_shared::pack::{Pack, Part};

use crate::service::{Caller, Node, OriginService, ServiceError};
use crate::workspace::Workspace;

/// A repository to share work with.
#[derive(Debug)]
pub enum Remote {
    /// A repository on a server.
    Http {
        /// The runtime the SDK's requests run on.
        runtime: tokio::runtime::Runtime,
        /// The repository.
        repo: origin_client::Repo,
    },
    /// A repository on a local path, served in-process.
    Local(Box<OriginService>),
}

/// Defines the remote's calls. Each is answered over HTTP by an SDK method, or
/// in-process by a service method called as the node's owner.
macro_rules! calls {
    ($(
        $(#[doc = $doc:literal])*
        fn $name:ident($($arg:ident: $type:ty),*) -> $output:ty =
            $http:ident($($http_arg:expr),*), $local:ident($($local_arg:expr),*);
    )*) => {
        impl Remote {$(
            $(#[doc = $doc])*
            ///
            /// # Errors
            ///
            /// Fails if the remote cannot be reached or refuses.
            pub fn $name(&self, $($arg: $type),*) -> Result<$output, RemoteError> {
                match self {
                    Self::Http { runtime, repo } => {
                        Ok(runtime.block_on(repo.$http($($http_arg),*))?)
                    }
                    Self::Local(service) => Ok(service.$local(&Caller::Owner, $($local_arg),*)?),
                }
            }
        )*}
    };
}

calls! {
    /// Lists every target, with its tip, authority and requirements.
    fn targets() -> Vec<TargetInfo> = targets(), targets();
    /// Returns the acceptances of a target, oldest first.
    fn log(query: &LogQuery) -> Vec<LogEntry> = log(query), log(query);
    /// Fetches records by id, each followed by the blob it carries. A part that
    /// stops short holds the records up to its last one.
    fn records(request: &FetchRecords) -> Part = fetch_records(request), records(&request.ids);
    /// Fetches, in parts, the trees and blobs of a snapshot that the request's
    /// snapshots lack.
    fn snapshot(request: &FetchSnapshot) -> Part = fetch_snapshot(request), snapshot(request);
    /// Returns what the remote holds of some changes.
    fn inventory(request: &InventoryRequest) -> Vec<Inventory> =
        inventory(request), inventory(request);
    /// Puts a batch of objects.
    fn put_records(pack: Pack) -> Stored = put_records(&pack), put_records(pack);
    /// Puts a batch of objects, if the changes the query names still have the heads
    /// it gives.
    fn push(pack: Pack, query: &PutQuery) -> Stored =
        push_records(&pack, query), push(pack, &query.expect.0);
    /// Submits a landing to the remote's queue.
    fn land(request: &LandRequest) -> Landings = land(request), land(request);
    /// Lists the landings waiting on the remote.
    fn landings() -> Vec<Pending> = landings(), landings();
    /// Creates a target on the remote, with the remote as its authority.
    fn create_target(request: &NewTarget) -> Head = create_target(request), create_target(request);
    /// Returns the events after a cursor, waiting for some to arrive.
    fn events(query: &EventsQuery) -> Page = events(query), events(query);
}

impl Remote {
    /// Opens the remote at `url`: an `http://` URL whose last path segment
    /// names the repository, or the path of a repository, which `node`
    /// serves in-process.
    ///
    /// # Errors
    ///
    /// Fails if the URL names no repository, or the path holds none.
    pub fn open(url: &RemoteUrl, node: Node) -> Result<Self, RemoteError> {
        match url {
            RemoteUrl::Http { base, repo } => Ok(Self::Http {
                runtime: tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(RemoteError::Runtime)?,
                repo: Client::new(base).repo(repo.clone()),
            }),
            RemoteUrl::Path(path) => {
                let meta = path.join(Workspace::META_DIR);
                let root = if meta.is_dir() { meta } else { path.clone() };
                let service = OriginService::open(root, node)
                    .map_err(|source| RemoteError::Open(Box::new(source)))?;
                Ok(Self::Local(Box::new(service)))
            }
        }
    }
}

/// Where a remote is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteUrl {
    /// A repository on a server.
    Http {
        /// The server, such as `http://127.0.0.1:7878`.
        base: String,
        /// The repository.
        repo: RepoName,
    },
    /// A repository on a local path, given as an absolute path.
    Path(PathBuf),
}

impl RemoteUrl {
    /// Parses a remote URL: `http://host[:port]/<repository>`, or a path, which it
    /// makes absolute so the URL works from any directory.
    ///
    /// # Errors
    ///
    /// Fails if an HTTP URL names no repository, or is HTTPS.
    pub fn parse(url: &str) -> Result<Self, RemoteError> {
        if url.starts_with("https://") {
            return Err(RemoteError::HttpsUnsupported);
        }
        let Some(rest) = url.strip_prefix("http://") else {
            let path = path::absolute(Path::new(url))
                .map_err(|_| RemoteError::InvalidUrl(url.to_owned()))?;
            return Ok(Self::Path(path));
        };
        let invalid = || RemoteError::InvalidUrl(url.to_owned());
        let (host, name) = rest
            .trim_end_matches('/')
            .rsplit_once('/')
            .ok_or_else(invalid)?;
        Ok(Self::Http {
            base: format!("http://{host}"),
            repo: RepoName::new(name).map_err(|_| invalid())?,
        })
    }

    /// The name a clone's directory takes by default: the repository's, or
    /// the path's last component.
    #[must_use]
    pub fn name(&self) -> Option<String> {
        match self {
            Self::Http { repo, .. } => Some(repo.to_string()),
            Self::Path(path) => Some(path.file_name()?.to_string_lossy().into_owned()),
        }
    }
}

impl fmt::Display for RemoteUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http { base, repo } => write!(f, "{base}/{repo}"),
            Self::Path(path) => write!(f, "{}", path.display()),
        }
    }
}

/// A failure to talk to a remote.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The remote URL cannot be used.
    #[error("invalid remote `{0}`: expected http://host[:port]/<repository> or a path")]
    InvalidUrl(String),
    /// The client has no TLS support yet.
    #[error("HTTPS remotes are not supported yet")]
    HttpsUnsupported,
    /// The runtime for HTTP requests cannot be started.
    #[error("cannot start the runtime for HTTP requests")]
    Runtime(#[source] std::io::Error),
    /// A local path holds no repository to serve.
    #[error("cannot open the remote repository")]
    Open(#[source] Box<ServiceError>),
    /// The server cannot be reached, or refused the call.
    #[error(transparent)]
    Http(#[from] origin_client::Error),
    /// The repository on a local path refused the call.
    #[error(transparent)]
    Local(#[from] ServiceError),
}

impl RemoteError {
    /// Returns the kind of refusal, if the remote refused.
    #[must_use]
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Http(origin_client::Error::Api(error)) => Some(error.code),
            Self::Local(error) => Some(error.code()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let url = RemoteUrl::parse("http://localhost:7878/demo/").unwrap();
        assert_eq!(
            url,
            RemoteUrl::Http {
                base: "http://localhost:7878".to_owned(),
                repo: RepoName::new("demo").unwrap(),
            }
        );
        assert_eq!(url.to_string(), "http://localhost:7878/demo");
        assert_eq!(url.name().as_deref(), Some("demo"));
        assert!(RemoteUrl::parse("http://localhost").is_err());
        assert!(RemoteUrl::parse("http://localhost/.hidden").is_err());
        assert!(RemoteUrl::parse("https://localhost/demo").is_err());
        let RemoteUrl::Path(path) = RemoteUrl::parse("server/demo").unwrap() else {
            panic!("expected a path");
        };
        assert!(path.is_absolute() && path.ends_with("server/demo"));
    }
}
