//! The HTTP API: routes that translate requests into [`OriginService`] calls on
//! the repository they name. `utoipa` documents every route and generates the
//! OpenAPI document; `origin-client` is the Rust SDK for it.

use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use origin_shared::api::{
    ApiError, ErrorCode, FetchRecords, LogEntry, LogQuery, RepoName, RepoSummary, Stored,
    TargetInfo,
};
use origin_shared::pack::{Pack, PackError};
use serde::Serialize;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::server::Repositories;
use crate::service::{Caller, OriginService, ServiceError};

#[derive(OpenApi)]
#[openapi(info(
    title = "origin API",
    version = "v1",
    description = "The repositories an origin server hosts: their records, targets, landing \
                   queues and events.",
    license(name = "Apache-2.0")
))]
struct ApiDoc;

type Repos = State<Arc<Repositories>>;

/// The HTTP API.
pub struct Api;

impl Api {
    /// The largest request body accepted, which caps a batch of records put.
    const MAX_REQUEST: usize = 1 << 30;

    fn documented() -> OpenApiRouter<Arc<Repositories>> {
        OpenApiRouter::with_openapi(ApiDoc::openapi())
            .routes(routes!(list_repos))
            .routes(routes!(fetch_records))
            .routes(routes!(put_records))
            .routes(routes!(targets))
            .routes(routes!(log))
            .routes(routes!(openapi_document))
    }

    /// Returns the router serving `repositories`.
    pub fn router(repositories: Repositories) -> axum::Router {
        axum::Router::from(Self::documented().with_state(Arc::new(repositories)))
            .layer(DefaultBodyLimit::max(Self::MAX_REQUEST))
    }

    /// Returns the OpenAPI document, which `docs/openapi.json` keeps a copy of.
    #[must_use]
    pub fn openapi() -> utoipa::openapi::OpenApi {
        Self::documented().into_openapi()
    }
}

/// Makes every request anonymous: nothing authenticates callers yet.
impl FromRequestParts<Arc<Repositories>> for Caller {
    type Rejection = Infallible;

    fn from_request_parts(
        _parts: &mut Parts,
        _repositories: &Arc<Repositories>,
    ) -> impl Future<Output = Result<Self, Infallible>> + Send {
        std::future::ready(Ok(Self::Anonymous))
    }
}

/// Declares extractors with the same names as axum's, so `utoipa` documents
/// them the same way, but which reject a request with an [`ApiError`], like
/// every other failure, instead of axum's plain text.
macro_rules! extractors {
    ($(
        $(#[$doc:meta])*
        $name:ident: $trait:ident::$method:ident($input:ty)
            via $($axum:ident)::+ refusing with $rejection:ty;
    )*) => {$(
        $(#[$doc])*
        struct $name<T>(T);

        impl<S: Send + Sync, T> $trait<S> for $name<T>
        where
            $($axum)::+<T>: $trait<S, Rejection = $rejection>,
        {
            type Rejection = HttpError;

            async fn $method(input: $input, state: &S) -> Result<Self, HttpError> {
                let $($axum)::+(value) = <$($axum)::+<T>>::$method(input, state).await?;
                Ok(Self(value))
            }
        }

        impl From<$rejection> for HttpError {
            fn from(rejection: $rejection) -> Self {
                Self::Rejected(rejection.status(), rejection.body_text())
            }
        }
    )*};
}

extractors! {
    /// The request's path parameters.
    Path: FromRequestParts::from_request_parts(&mut Parts)
        via axum::extract::Path refusing with PathRejection;
    /// The request's query.
    Query: FromRequestParts::from_request_parts(&mut Parts)
        via axum::extract::Query refusing with QueryRejection;
    /// The request's JSON body; also a JSON response.
    Json: FromRequest::from_request(Request)
        via axum::Json refusing with JsonRejection;
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// Runs `call` on the service of repository `repo`, on the blocking thread
/// pool: service calls do synchronous file system work, and event long-polls
/// wait.
async fn call<T: Send + 'static>(
    State(repositories): Repos,
    repo: RepoName,
    caller: Caller,
    call: impl FnOnce(&OriginService, &Caller) -> Result<T, ServiceError> + Send + 'static,
) -> Result<T, HttpError> {
    tokio::task::spawn_blocking(move || call(repositories.get(&caller, &repo)?, &caller))
        .await
        .map_err(|error| HttpError::Panicked(error.to_string()))?
        .map_err(HttpError::Service)
}

/// A failed request, rendered as an [`ApiError`].
enum HttpError {
    Service(ServiceError),
    Pack(PackError),
    /// axum rejected the request's path, query or body, with this status. It is the
    /// client's error, unless a route doesn't match its handler.
    Rejected(StatusCode, String),
    Panicked(String),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (code, message) = match self {
            Self::Service(error) if error.code() == ErrorCode::Internal => {
                tracing::error!(error = %anyhow::Error::from(error), "request failed");
                (ErrorCode::Internal, "internal server error".to_owned())
            }
            Self::Service(error) => (error.code(), error.to_string()),
            Self::Pack(error) => (ErrorCode::BadRequest, format!("invalid pack: {error}")),
            Self::Rejected(status, message) if status.is_server_error() => {
                tracing::error!(%message, "request rejected");
                (ErrorCode::Internal, "internal server error".to_owned())
            }
            Self::Rejected(_, message) => (ErrorCode::BadRequest, message),
            Self::Panicked(message) => {
                tracing::error!(%message, "request panicked");
                (ErrorCode::Internal, "internal server error".to_owned())
            }
        };
        let status = match code {
            ErrorCode::BadRequest => StatusCode::BAD_REQUEST,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Conflict => StatusCode::CONFLICT,
            ErrorCode::Expired => StatusCode::GONE,
            ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(ApiError { code, message })).into_response()
    }
}

/// Marks binary content (a pack) in the OpenAPI document, which `utoipa` would
/// otherwise describe as an array of numbers.
struct Binary;

impl utoipa::PartialSchema for Binary {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::Type::String)
            .content_media_type("application/octet-stream")
            .into()
    }
}

impl utoipa::ToSchema for Binary {}

/// Lists the repositories on the server.
#[utoipa::path(
    get,
    path = "/api/v1/repos",
    operation_id = "list_repos",
    responses((status = 200, description = "The repositories, by name", body = Vec<RepoSummary>))
)]
async fn list_repos(State(repositories): Repos, caller: Caller) -> Json<Vec<RepoSummary>> {
    Json(repositories.list(&caller))
}

/// Fetches objects by id, as a pack.
#[utoipa::path(
    post,
    path = "/api/v1/repos/{repo}/records/fetch",
    operation_id = "fetch_records",
    params(("repo" = String, Path, description = "The repository")),
    request_body = FetchRecords,
    responses(
        (status = 200, description = "The objects, in the order asked, as a pack: each a 32-byte id, a little-endian u64 length and the object's encoding", content_type = "application/octet-stream", body = Binary),
        (status = 404, description = "The repository or an object does not exist", body = ApiError)
    )
)]
async fn fetch_records(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Json(request): Json<FetchRecords>,
) -> Result<Response, HttpError> {
    let pack = call(repos, repo, caller, move |service, caller| {
        service.records(caller, &request.ids)
    })
    .await?;
    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        pack.encode(),
    )
        .into_response())
}

/// Puts a batch of objects as a pack: snapshots, revisions and statements. Each
/// may only need objects that the batch or the repository has, of the right
/// kinds.
#[utoipa::path(
    put,
    path = "/api/v1/repos/{repo}/records",
    operation_id = "put_records",
    params(("repo" = String, Path, description = "The repository")),
    request_body(content = Binary, content_type = "application/octet-stream", description = "The objects, as a pack"),
    responses(
        (status = 200, description = "The objects are durable", body = Stored),
        (status = 400, description = "The pack is invalid, holds an acceptance, or needs objects the repository doesn't have, or has as another kind", body = ApiError),
        (status = 404, description = "The repository does not exist", body = ApiError)
    )
)]
async fn put_records(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    body: Bytes,
) -> Result<Json<Stored>, HttpError> {
    let pack = Pack::decode(&body).map_err(HttpError::Pack)?;
    call(repos, repo, caller, move |service, caller| {
        service.put_records(caller, pack)
    })
    .await
    .map(Json)
}

/// Lists the targets, with their tips, authorities and requirements.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/targets",
    operation_id = "targets",
    params(("repo" = String, Path, description = "The repository")),
    responses(
        (status = 200, description = "The targets, by name", body = Vec<TargetInfo>),
        (status = 404, description = "The repository does not exist", body = ApiError)
    )
)]
async fn targets(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
) -> Result<Json<Vec<TargetInfo>>, HttpError> {
    call(repos, repo, caller, OriginService::targets)
        .await
        .map(Json)
}

/// Lists the acceptances of a target, oldest first.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/log",
    operation_id = "log",
    params(("repo" = String, Path, description = "The repository"), LogQuery),
    responses(
        (status = 200, description = "The acceptances from `since` on", body = Vec<LogEntry>),
        (status = 404, description = "The repository or the target does not exist", body = ApiError)
    )
)]
async fn log(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Query(query): Query<LogQuery>,
) -> Result<Json<Vec<LogEntry>>, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.log(caller, &query.target, query.since)
    })
    .await
    .map(Json)
}

/// Returns the OpenAPI document of this API.
#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    operation_id = "openapi_document",
    responses((status = 200, description = "The OpenAPI document", body = Object))
)]
async fn openapi_document() -> Json<utoipa::openapi::OpenApi> {
    Json(Api::openapi())
}
