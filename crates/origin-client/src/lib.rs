//! The origin SDK: a typed client for origin servers.
//!
//! ```no_run
//! use origin_client::Client;
//! use origin_shared::api::RepoName;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::new("http://127.0.0.1:7878");
//! let repo = client.repo(RepoName::new("demo")?);
//! for target in repo.targets().await? {
//!     println!("{} is at #{}", target.name, target.head.seq);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Requests and responses use the same `origin-shared` types as the server.
//! The server also describes its API in `docs/openapi.json`, for other
//! languages.

use origin_shared::api::{
    ApiError, FetchRecords, LogEntry, LogQuery, RepoName, RepoSummary, Stored, TargetInfo,
};
use origin_shared::pack::{Pack, PackError};
use reqwest::{Method, RequestBuilder};
use serde::de::DeserializeOwned;

/// Defines endpoints as methods. Each method sends one request and reads the
/// response with a [`Response`] method. The request is built from an HTTP
/// method and a path relative to the owner's URL, which can name the
/// arguments as `format!` does. An optional [`RequestBuilder`] method
/// (`query`, `json` or `body`) adds a body.
macro_rules! endpoints {
    ($(
        impl $owner:ident {$(
            $(#[doc = $doc:literal])*
            fn $name:ident($($arg:ident: $type:ty),*) -> $output:ty =
                $method:ident $path:literal $(, $send:ident($body:expr))? => $read:ident;
        )*}
    )*) => {$(
        impl $owner {$(
            $(#[doc = $doc])*
            ///
            /// # Errors
            ///
            /// Fails if the server cannot be reached or refuses the request.
            pub async fn $name(&self, $($arg: $type),*) -> Result<$output, Error> {
                let request = self.request(Method::$method, &format!($path))$(.$send($body))?;
                Response::send(request).await?.$read().await
            }
        )*}
    )*};
}

endpoints! {
    impl Client {
        /// Lists the repositories on the server.
        fn repos() -> Vec<RepoSummary> = GET "repos" => json;
        /// Returns the server's OpenAPI document.
        fn openapi() -> serde_json::Value = GET "openapi.json" => json;
    }

    impl Repo {
        /// Fetches objects by id, in the order requested.
        fn fetch_records(request: &FetchRecords) -> Pack =
            POST "records/fetch", json(request) => pack;
        /// Stores a batch of objects: snapshots, revisions and statements. Each
        /// object may only depend on objects in the batch or in the repository.
        fn put_records(pack: &Pack) -> Stored = PUT "records", body(pack.encode()) => json;
        /// Lists every target, with its tip, authority and requirements.
        fn targets() -> Vec<TargetInfo> = GET "targets" => json;
        /// Returns the acceptances of a target, oldest first.
        fn log(query: &LogQuery) -> Vec<LogEntry> = GET "log", query(query) => json;
    }
}

/// A client for an origin server. Cloning it is cheap, and clones share
/// their connections.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    /// The API's base URL, ending with `/`.
    api: String,
}

impl Client {
    /// Creates a client for the server at `url`, such as
    /// `http://127.0.0.1:7878`.
    #[must_use]
    pub fn new(url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            api: format!("{}/api/v1/", url.trim_end_matches('/')),
        }
    }

    /// Returns a handle on a repository on the server. Sends no request.
    #[must_use]
    pub fn repo(&self, name: RepoName) -> Repo {
        Repo {
            client: self.clone(),
            name,
        }
    }

    /// Starts a request to `path`, relative to the API's base URL.
    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http.request(method, format!("{}{path}", self.api))
    }
}

/// A repository on a server.
#[derive(Clone, Debug)]
pub struct Repo {
    client: Client,
    name: RepoName,
}

impl Repo {
    /// The repository's name.
    #[must_use]
    pub fn name(&self) -> &RepoName {
        &self.name
    }

    /// Starts a request to `path`, relative to the repository's URL.
    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let path = format!("repos/{}/{path}", self.name.as_str());
        self.client.request(method, &path)
    }
}

/// A successful response, read in the form its endpoint returns.
struct Response(reqwest::Response);

impl Response {
    /// Sends a request, and turns error statuses into errors.
    async fn send(request: RequestBuilder) -> Result<Self, Error> {
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            return Ok(Self(response));
        }
        Err(match response.json().await {
            Ok(error) => Error::Api(error),
            Err(_) => Error::Status(status.as_u16()),
        })
    }

    async fn json<T: DeserializeOwned>(self) -> Result<T, Error> {
        Ok(self.0.json().await?)
    }

    async fn pack(self) -> Result<Pack, Error> {
        Ok(Pack::decode(&self.0.bytes().await?)?)
    }
}

/// A failed request.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server refused the request, and said why.
    #[error("the server refused: {0}")]
    Api(ApiError),
    /// The server answered with an error status, without saying why.
    #[error("the server answered with status {0}")]
    Status(u16),
    /// The server could not be reached, or its response could not be read.
    #[error("cannot reach the server")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The server sent invalid objects.
    #[error("the server sent invalid objects")]
    Pack(#[from] PackError),
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(Box::new(error))
    }
}
