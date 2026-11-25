//! The daemon's HTTP surface: routes and the origin guard.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

use ship_core::api::{HOST_PATH, PROJECTS_PATH};
use ship_core::{
    API_VERSION, API_VERSION_HEADER, ApiError, ErrorCode, HostInfo, ProjectList, ProjectSummary,
    SHIP_VERSION_HEADER,
};
use ship_docker::Runner;
use ship_store::Store;

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
}

/// The daemon's router over `store`, inspecting the host through `host`.
pub fn router(store: Store, host: HostProbe) -> Router {
    Router::new()
        .route(HEALTHZ_PATH, get(healthz))
        .route(HOST_PATH, get(host_info))
        .route(PROJECTS_PATH, get(projects))
        .fallback(not_found)
        .with_state(AppState {
            store,
            host: Arc::new(host),
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

async fn not_found() -> Response {
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
            forbidden(reason)
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
    let headers = response.headers_mut();
    headers.insert(API_VERSION_HEADER, HeaderValue::from_static(API_VERSION));
    headers.insert(SHIP_VERSION_HEADER, HeaderValue::from_static(SHIP_VERSION));
    response
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
