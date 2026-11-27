//! The daemon's HTTP surface: routes and the origin guard. Paths no API
//! route claims fall through to the embedded UI ([`crate::ui`]).
//!
//! Operations (ADR-0011): `GET /api/v1/operations/{id}` and its event
//! stream over SSE. Mutation routes call [`accept`], which holds the
//! idempotency-key rule in one place.

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

use ship_core::api::{
    HOST_PATH, IDEMPOTENCY_KEY_HEADER, LAST_EVENT_ID_HEADER, OperationEventView,
    OperationFailureView, OperationView, PROJECTS_PATH,
};
use ship_core::{
    API_VERSION, API_VERSION_HEADER, ApiError, EnvironmentId, ErrorCode, HostInfo, OperationId,
    OperationPhase, ProjectList, ProjectSummary, ReceivedCode, SHIP_VERSION_HEADER,
};
use ship_docker::Runner;
use ship_store::{Operation, OperationEvent, Store, StoreError};

use crate::queue::{Context, JobFailure, Queue, QueueError, Submission};

/// `GET`: [`OperationView`].
const OPERATION_ROUTE: &str = "/api/v1/operations/{id}";
/// `GET`: SSE stream of [`OperationEventView`].
const OPERATION_EVENTS_ROUTE: &str = "/api/v1/operations/{id}/events";

/// How often an event stream re-reads the store when nothing announced a
/// new event.
const FOLLOW_POLL: Duration = Duration::from_secs(1);
/// Quiet polls between SSE keep-alive comments, which also reveal a client
/// that went away.
const KEEPALIVE_POLLS: u32 = 15;

/// Path of the liveness probe. Reveals nothing about the host.
pub const HEALTHZ_PATH: &str = "/healthz";

/// The daemon's Ship version, sent on every `/api/` response.
const SHIP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Host names a request may address. Any port is accepted, because
/// `ship open` reaches the daemon through an ssh-forwarded local port.
const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "localhost", "[::1]"];

/// What `/api/v1/host` inspects.
#[derive(Debug, Clone)]
pub struct HostProbe {
    /// Runs the Docker preflight.
    pub docker: Runner,
    /// Free disk is measured on this directory's filesystem.
    pub state_dir: PathBuf,
}

#[derive(Clone)]
struct AppState {
    store: Store,
    host: Arc<HostProbe>,
    queue: Queue,
}

/// The daemon's router over `store`, inspecting the host through `host`
/// and following operations of `queue`.
pub fn router(store: Store, host: HostProbe, queue: Queue) -> Router {
    Router::new()
        .route(HEALTHZ_PATH, get(healthz))
        .route(HOST_PATH, get(host_info))
        .route(PROJECTS_PATH, get(projects))
        .route(OPERATION_ROUTE, get(operation))
        .route(OPERATION_EVENTS_ROUTE, get(operation_events))
        // Everything else is the UI, which keeps `/api/*`'s JSON 404.
        .fallback(crate::ui::fallback)
        .with_state(AppState {
            store,
            host: Arc::new(host),
            queue,
        })
        .layer(middleware::from_fn(api_version))
        .layer(middleware::from_fn(origin_guard))
}

/// The response for an [`ApiError`]: its code's status and the error as JSON.
pub fn error_response(error: ApiError) -> Response {
    let status =
        StatusCode::from_u16(error.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(error)).into_response()
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// Always `200`: Docker being unusable is part of the answer, not an error,
/// and never stops the daemon.
async fn host_info(State(state): State<AppState>) -> Json<HostInfo> {
    let info = crate::host::host_info(&state.host.docker, &state.host.state_dir).await;
    if let Some(message) = &info.docker.message {
        tracing::warn!(status = %info.docker.status, message, "docker preflight");
    }
    Json(info)
}

async fn projects(State(state): State<AppState>) -> Response {
    match state.store.list_projects().await {
        Ok(list) => Json(ProjectList {
            projects: list
                .into_iter()
                .map(|p| ProjectSummary {
                    id: p.id,
                    slug: p.slug,
                    created_at: p.created_at,
                })
                .collect(),
        })
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "listing projects failed");
            error_response(ApiError::new(
                ErrorCode::Internal,
                "the daemon could not read its database",
            ))
        }
    }
}

// ---- operations ----

/// The API view of a stored operation.
pub fn operation_view(op: &Operation) -> OperationView {
    let failure = match (&op.failure_code, &op.failure_message, op.failure_phase) {
        (Some(code), Some(message), Some(phase)) => Some(OperationFailureView {
            code: ReceivedCode::from(code.clone()),
            phase,
            message: message.clone(),
        }),
        _ => None,
    };
    OperationView {
        id: op.id,
        kind: op.kind.clone(),
        environment_id: op.environment_id,
        phase: op.phase,
        status: op.status,
        failure,
        log_bytes: u64::try_from(op.log_cursor).unwrap_or(0),
        created_at: op.created_at,
        updated_at: op.updated_at,
    }
}

fn event_view(e: &OperationEvent) -> OperationEventView {
    OperationEventView {
        operation_id: e.operation_id,
        sequence: u64::try_from(e.sequence).unwrap_or(0),
        phase: e.phase,
        event: e.event_type.clone(),
        message: e.message.clone(),
        ts: e.ts,
    }
}

/// Runs a mutation through the queue and answers for it (ADR-0011 rule 1).
///
/// The request must carry an `Idempotency-Key` header of 1 to 128 visible
/// ASCII characters, else `400 invalid_request`. A new key answers `202`
/// with the [`OperationView`]; a replayed key answers the same for the
/// original operation and `job` never runs; a key used before for another
/// kind or environment is `422 idempotency_conflict`. An operation the
/// queue refused at admission answers its failure code's status
/// (`429 queue_full`, `507 insufficient_storage`) with `operation_id` set,
/// on every replay alike.
pub async fn accept<F, Fut>(
    queue: &Queue,
    headers: &HeaderMap,
    kind: &str,
    environment_id: Option<EnvironmentId>,
    job: F,
) -> Response
where
    F: FnOnce(Context) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), JobFailure>> + Send + 'static,
{
    let Some(key) = headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
    else {
        return error_response(QueueError::InvalidKey.to_api());
    };
    let submission = Submission {
        idempotency_key: key,
        kind,
        environment_id,
    };
    match queue.submit(submission, job).await {
        Ok(submitted) => submitted_response(&submitted.operation),
        Err(e) => {
            if let QueueError::Store(err) = &e {
                tracing::error!(error = %err, "operation could not be created");
            }
            error_response(e.to_api())
        }
    }
}

/// `202` with the view, or the admission refusal recorded on `op`.
pub fn submitted_response(op: &Operation) -> Response {
    match admission_refusal(op) {
        Some(error) => error_response(error),
        None => (StatusCode::ACCEPTED, Json(operation_view(op))).into_response(),
    }
}

/// The error an operation failed with at admission, if it did: failed
/// from `queued` with `queue_full` or `insufficient_storage`.
fn admission_refusal(op: &Operation) -> Option<ApiError> {
    if op.phase != OperationPhase::Failed || op.failure_phase != Some(OperationPhase::Queued) {
        return None;
    }
    let code: ErrorCode = op.failure_code.as_deref()?.parse().ok()?;
    matches!(code, ErrorCode::QueueFull | ErrorCode::InsufficientStorage).then(|| {
        ApiError::new(code, op.failure_message.clone().unwrap_or_default()).with_operation(op.id)
    })
}

/// The operation named by a path segment, or the error response for it.
async fn find_operation(store: &Store, id: &str) -> Result<Operation, Response> {
    let id: OperationId = id
        .parse()
        .map_err(|e: ship_core::IdError| error_response(e.into()))?;
    match store.get_operation(id).await {
        Ok(op) => Ok(op),
        Err(StoreError::NotFound { .. }) => Err(error_response(ApiError::new(
            ErrorCode::NotFound,
            "no such operation",
        ))),
        Err(e) => {
            tracing::error!(error = %e, "reading an operation failed");
            Err(error_response(ApiError::new(
                ErrorCode::Internal,
                "the daemon could not read its database",
            )))
        }
    }
}

async fn operation(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match find_operation(&state.store, &id).await {
        Ok(op) => Json(operation_view(&op)).into_response(),
        Err(response) => response,
    }
}

/// Where an event stream resumes: after the `Last-Event-ID` header's
/// sequence, else after the `after` query parameter, else from the start.
fn resume_point(headers: &HeaderMap, query: Option<&str>) -> Result<i64, &'static str> {
    if let Some(value) = headers.get(LAST_EVENT_ID_HEADER) {
        let value = value
            .to_str()
            .map_err(|_| "Last-Event-ID must be a sequence number")?;
        return parse_sequence(value).ok_or("Last-Event-ID must be a sequence number");
    }
    let after = query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.strip_prefix("after="));
    match after {
        Some(value) => parse_sequence(value).ok_or("after must be a sequence number"),
        None => Ok(0),
    }
}

/// A sequence number: decimal digits, or empty for "none yet".
fn parse_sequence(value: &str) -> Option<i64> {
    if value.is_empty() {
        return Some(0);
    }
    if !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse::<i64>().ok()
}

/// SSE stream of an operation's events after the resume point, in order,
/// each once; it ends after the operation's terminal event, or when the
/// daemon stops.
async fn operation_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let after = match resume_point(&headers, uri.query()) {
        Ok(after) => after,
        Err(message) => {
            return error_response(ApiError::new(ErrorCode::InvalidRequest, message));
        }
    };
    let op = match find_operation(&state.store, &id).await {
        Ok(op) => op,
        Err(response) => return response,
    };
    let follow = Follow {
        store: state.store.clone(),
        operation: op.id,
        after,
        pending: VecDeque::new(),
        done: false,
        events: state.queue.subscribe_events(),
        closing: state.queue.subscribe_closing(),
        quiet: 0,
    };
    let stream = futures_util::stream::unfold(follow, next_frame);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| error_response(ApiError::new(ErrorCode::Internal, "could not stream")))
}

/// One event stream's position.
struct Follow {
    store: Store,
    operation: OperationId,
    /// The last sequence sent (or the resume point).
    after: i64,
    pending: VecDeque<OperationEventView>,
    /// The terminal event has been read; send what is pending and end.
    done: bool,
    events: watch::Receiver<u64>,
    closing: watch::Receiver<bool>,
    quiet: u32,
}

impl Follow {
    fn take(&mut self, events: Vec<OperationEvent>) {
        for e in events {
            self.after = e.sequence;
            self.pending.push_back(event_view(&e));
        }
    }
}

/// One SSE frame: `id:` is the sequence, `data:` the event as JSON on one
/// line.
fn sse_frame(event: &OperationEventView) -> Bytes {
    let data = serde_json::to_string(event).expect("an event always serialises");
    Bytes::from(format!("id: {}\ndata: {data}\n\n", event.sequence))
}

async fn next_frame(mut f: Follow) -> Option<(Result<Bytes, Infallible>, Follow)> {
    loop {
        if let Some(event) = f.pending.pop_front() {
            return Some((Ok(sse_frame(&event)), f));
        }
        if f.done || *f.closing.borrow() {
            return None;
        }
        // Marked seen before reading, so an event stored after the read
        // wakes the wait below.
        f.events.borrow_and_update();
        match f.store.events_after(f.operation, f.after).await {
            Ok(events) if !events.is_empty() => {
                f.quiet = 0;
                f.take(events);
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error = %e, "reading operation events failed");
                return None;
            }
        }
        match f.store.get_operation(f.operation).await {
            Ok(op) if op.phase.is_terminal() => {
                // The terminal event is stored before the terminal phase,
                // so this read finds every remaining event.
                match f.store.events_after(f.operation, f.after).await {
                    Ok(events) => f.take(events),
                    Err(e) => {
                        tracing::error!(error = %e, "reading operation events failed");
                        return None;
                    }
                }
                f.done = true;
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error = %e, "reading an operation failed");
                return None;
            }
        }
        tokio::select! {
            changed = f.events.changed() => {
                if changed.is_err() {
                    return None;
                }
            }
            changed = f.closing.changed() => {
                if changed.is_err() {
                    return None;
                }
            }
            _ = tokio::time::sleep(FOLLOW_POLL) => {
                f.quiet += 1;
                if f.quiet >= KEEPALIVE_POLLS {
                    f.quiet = 0;
                    return Some((Ok(Bytes::from_static(b": keep-alive\n\n")), f));
                }
            }
        }
    }
}

/// The API's answer for a path no route claims.
pub(crate) fn not_found() -> Response {
    error_response(ApiError::new(ErrorCode::NotFound, "no such endpoint"))
}

fn forbidden(message: &str) -> Response {
    error_response(ApiError::new(ErrorCode::Forbidden, message))
}

/// Rejects requests a browser could have been tricked into sending
/// (ADR-0003 rule 6, OVERVIEW §5). Runs before routing, on every listener.
///
/// - `Host`, when present, must name a loopback host; this stops DNS
///   rebinding, where a hostile page's own origin resolves to 127.0.0.1.
/// - A mutating request must not come from another origin: `Origin`, when
///   present, must be `http://<Host>`, and `Sec-Fetch-Site`, when present,
///   must be `same-origin` or `none`. A client with no `Origin` (the CLI)
///   passes.
async fn origin_guard(request: Request, next: Next) -> Response {
    match check_origin(request.method(), request.headers()) {
        Ok(()) => next.run(request).await,
        Err(reason) => {
            tracing::warn!(
                method = %request.method(),
                path = request.uri().path(),
                reason,
                "request rejected by origin guard"
            );
            let mut response = forbidden(reason);
            if request.uri().path().starts_with("/api/") {
                add_version_headers(&mut response);
            }
            response
        }
    }
}

/// The API version handshake (OVERVIEW §8) on every `/api/` path.
///
/// Every response carries the daemon's API and Ship versions. A request
/// naming a different API version is refused with `426
/// unsupported_api_version` before routing, with both versions in the
/// details so the client can say which side to upgrade. A request naming
/// none (curl, a browser) is served. `/healthz` is left alone: it reveals
/// nothing about the daemon.
async fn api_version(request: Request, next: Next) -> Response {
    if !request.uri().path().starts_with("/api/") {
        return next.run(request).await;
    }
    let mut response = match request.headers().get(API_VERSION_HEADER) {
        Some(value) if value.as_bytes() != API_VERSION.as_bytes() => {
            let client = String::from_utf8_lossy(value.as_bytes()).into_owned();
            tracing::warn!(
                client_api_version = %client,
                daemon_api_version = API_VERSION,
                "request refused: unsupported API version"
            );
            error_response(
                ApiError::new(
                    ErrorCode::UnsupportedApiVersion,
                    format!("this daemon speaks API {API_VERSION}, the client speaks {client}"),
                )
                .with_details(json!({
                    "client_api_version": client,
                    "daemon_api_version": API_VERSION,
                    "daemon_version": SHIP_VERSION,
                })),
            )
        }
        _ => next.run(request).await,
    };
    add_version_headers(&mut response);
    response
}

/// Adds the daemon's API and Ship versions to an `/api/` response,
/// including one the origin guard refused before `api_version` ran.
fn add_version_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(API_VERSION_HEADER, HeaderValue::from_static(API_VERSION));
    headers.insert(SHIP_VERSION_HEADER, HeaderValue::from_static(SHIP_VERSION));
}

fn is_mutating(method: &Method) -> bool {
    !matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    )
}

/// `Ok` when the request may proceed; otherwise the reason it may not.
pub(crate) fn check_origin(method: &Method, headers: &HeaderMap) -> Result<(), &'static str> {
    let host = match headers.get(header::HOST) {
        None => None,
        Some(value) => {
            let host = value.to_str().map_err(|_| "malformed Host header")?;
            if !is_loopback_host(host) {
                return Err("Host must name a loopback address");
            }
            Some(host)
        }
    };

    if !is_mutating(method) {
        return Ok(());
    }

    if let Some(site) = headers.get("sec-fetch-site") {
        let site = site.to_str().unwrap_or("");
        if !site.eq_ignore_ascii_case("same-origin") && !site.eq_ignore_ascii_case("none") {
            return Err("cross-site request refused");
        }
    }

    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin = origin.to_str().map_err(|_| "malformed Origin header")?;
        let Some(host) = host else {
            return Err("cross-origin request refused");
        };
        let expected = format!("http://{host}");
        if !origin.eq_ignore_ascii_case(&expected) {
            return Err("cross-origin request refused");
        }
    }
    Ok(())
}

/// Whether `host` (a `Host` header value) names a loopback host, with an
/// optional numeric port.
fn is_loopback_host(host: &str) -> bool {
    let (name, port) = if host.starts_with('[') {
        match host.find(']') {
            Some(end) => (&host[..=end], &host[end + 1..]),
            None => return false,
        }
    } else {
        match host.rfind(':') {
            Some(i) => (&host[..i], &host[i..]),
            None => (host, ""),
        }
    };
    let port_ok = port.is_empty()
        || port.strip_prefix(':').is_some_and(|p| {
            !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())
        });
    port_ok && LOOPBACK_HOSTS.iter().any(|h| name.eq_ignore_ascii_case(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn resume_points() {
        let none = HeaderMap::new();
        assert_eq!(resume_point(&none, None), Ok(0));
        assert_eq!(resume_point(&none, Some("")), Ok(0));
        assert_eq!(resume_point(&none, Some("after=7")), Ok(7));
        assert_eq!(resume_point(&none, Some("x=1&after=12")), Ok(12));
        assert_eq!(resume_point(&none, Some("after=")), Ok(0));
        for bad in [
            "after=-1",
            "after=1.5",
            "after=x",
            "after=99999999999999999999",
        ] {
            assert!(resume_point(&none, Some(bad)).is_err(), "{bad}");
        }
        // The header wins over the query.
        let h = headers(&[("last-event-id", "3")]);
        assert_eq!(resume_point(&h, Some("after=9")), Ok(3));
        assert_eq!(
            resume_point(&headers(&[("last-event-id", "")]), None),
            Ok(0)
        );
        assert!(resume_point(&headers(&[("last-event-id", "abc")]), None).is_err());
        assert!(resume_point(&headers(&[("last-event-id", " 3")]), None).is_err());
    }

    #[test]
    fn loopback_hosts() {
        for ok in [
            "127.0.0.1",
            "127.0.0.1:7180",
            "localhost",
            "LOCALHOST:1",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(is_loopback_host(ok), "{ok}");
        }
        for bad in [
            "",
            "evil.example",
            "evil.example:7180",
            "127.0.0.1.evil.example",
            "localhost.evil.example",
            "127.0.0.2",
            "0.0.0.0",
            "localhost:",
            "localhost:abc",
            "localhost:123456",
            "[::1",
            "[::2]:80",
            "user@localhost",
        ] {
            assert!(!is_loopback_host(bad), "{bad}");
        }
    }

    #[test]
    fn reads_need_only_a_loopback_host() {
        assert!(check_origin(&Method::GET, &headers(&[])).is_ok());
        assert!(
            check_origin(
                &Method::GET,
                &headers(&[("host", "localhost"), ("origin", "http://evil.example")])
            )
            .is_ok()
        );
        assert!(check_origin(&Method::GET, &headers(&[("host", "evil.example")])).is_err());
    }

    #[test]
    fn mutations_need_same_origin() {
        let post = Method::POST;
        assert!(check_origin(&post, &headers(&[])).is_ok());
        assert!(check_origin(&post, &headers(&[("host", "127.0.0.1:7180")])).is_ok());
        assert!(
            check_origin(
                &post,
                &headers(&[
                    ("host", "127.0.0.1:7180"),
                    ("origin", "http://127.0.0.1:7180")
                ])
            )
            .is_ok()
        );
        assert!(
            check_origin(
                &post,
                &headers(&[
                    ("host", "localhost:9000"),
                    ("sec-fetch-site", "same-origin")
                ])
            )
            .is_ok()
        );
        for bad in [
            headers(&[
                ("host", "127.0.0.1:7180"),
                ("origin", "http://evil.example"),
            ]),
            headers(&[
                ("host", "127.0.0.1:7180"),
                ("origin", "http://127.0.0.1:9999"),
            ]),
            headers(&[
                ("host", "127.0.0.1:7180"),
                ("origin", "https://127.0.0.1:7180"),
            ]),
            headers(&[("host", "127.0.0.1:7180"), ("origin", "null")]),
            headers(&[("origin", "http://127.0.0.1:7180")]),
            headers(&[("host", "localhost"), ("sec-fetch-site", "cross-site")]),
            headers(&[("host", "localhost"), ("sec-fetch-site", "same-site")]),
            headers(&[("host", "evil.example"), ("origin", "http://evil.example")]),
        ] {
            for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
                assert!(check_origin(&method, &bad).is_err(), "{method} {bad:?}");
            }
        }
    }
}
