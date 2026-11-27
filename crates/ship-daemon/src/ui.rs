//! Serving the embedded UI (OVERVIEW §2.3, §3.1).
//!
//! The router's fallback: every path no API route claims comes here, after
//! the origin and Host guards. `/api/*` keeps its JSON 404. Otherwise only
//! `GET` and `HEAD` are served: an exact asset path with its content type
//! (`/assets/*` hashed and immutable, a missing one 404), and any other
//! path the SPA's `index.html`, uncached. Every UI response carries a CSP
//! that allows nothing but this origin (ADR-0012: no third-party origin).
//!
//! Built without `web/dist`, the table is empty and UI paths answer 503
//! naming `make web`.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));
}

/// `(url path, bytes, content type)`, sorted by path.
pub(crate) type Assets = &'static [(&'static str, &'static [u8], &'static str)];

// The UI embedded at build time.
use embedded::ASSETS;

/// The policy on every UI response.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
     style-src 'self'; img-src 'self' data:; connect-src 'self'; font-src 'self'; \
     base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// The body of a UI path when the daemon was built without the UI.
pub const NOT_BUILT: &str = "UI not built: run `make web`\n";

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";
const ASSETS_PREFIX: &str = "/assets/";
const INDEX: &str = "/index.html";

/// Whether this build embeds the UI.
pub fn is_built() -> bool {
    !ASSETS.is_empty()
}

/// What a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// Under `/api`: the API's JSON 404.
    Api,
    /// The daemon has no UI.
    NotBuilt,
    /// An embedded file, by index into the table.
    Asset(usize),
    /// A missing `/assets/*` file.
    MissingAsset,
    /// A client route: `index.html`.
    Index(usize),
}

pub(crate) fn resolve(assets: Assets, path: &str) -> Resolved {
    if path == "/api" || path.starts_with("/api/") {
        return Resolved::Api;
    }
    let find = |p: &str| assets.binary_search_by(|(a, _, _)| (*a).cmp(p)).ok();
    let Some(index) = find(INDEX) else {
        return Resolved::NotBuilt;
    };
    if let Some(i) = find(path) {
        return Resolved::Asset(i);
    }
    if path.starts_with(ASSETS_PREFIX) {
        return Resolved::MissingAsset;
    }
    Resolved::Index(index)
}

/// The fallback handler.
pub(crate) async fn fallback(request: Request) -> Response {
    respond(ASSETS, request.method(), request.uri().path())
}

pub(crate) fn respond(assets: Assets, method: &Method, path: &str) -> Response {
    let resolved = resolve(assets, path);
    if resolved == Resolved::Api {
        return crate::http::not_found();
    }
    let mut response = if *method != Method::GET && *method != Method::HEAD {
        let mut r = text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
        r.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        r
    } else {
        match resolved {
            Resolved::Api => unreachable!("handled above"),
            Resolved::NotBuilt => text(StatusCode::SERVICE_UNAVAILABLE, NOT_BUILT),
            Resolved::MissingAsset => text(StatusCode::NOT_FOUND, "not found\n"),
            Resolved::Asset(i) => {
                let cache = if path.starts_with(ASSETS_PREFIX) {
                    IMMUTABLE
                } else {
                    NO_CACHE
                };
                file(assets[i], cache)
            }
            Resolved::Index(i) => file(assets[i], NO_CACHE),
        }
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn text(status: StatusCode, body: &'static str) -> Response {
    let mut r = (status, body).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_CACHE));
    r
}

fn file(
    (_, bytes, content_type): (&'static str, &'static [u8], &'static str),
    cache: &'static str,
) -> Response {
    let mut r = Response::new(Body::from(bytes));
    let headers = r.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    const TABLE: Assets = &[
        (
            "/assets/app-abc123.js",
            b"console.log(1)",
            "text/javascript; charset=utf-8",
        ),
        ("/assets/app-def456.css", b"p{}", "text/css; charset=utf-8"),
        ("/favicon.svg", b"<svg/>", "image/svg+xml"),
        (
            "/index.html",
            b"<!doctype html>",
            "text/html; charset=utf-8",
        ),
    ];

    fn header(r: &Response, name: header::HeaderName) -> Option<&str> {
        r.headers().get(name).and_then(|v| v.to_str().ok())
    }

    async fn body(r: Response) -> String {
        let bytes = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn table_is_sorted() {
        assert!(TABLE.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(ASSETS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn paths_resolve() {
        assert_eq!(resolve(TABLE, "/"), Resolved::Index(3));
        assert_eq!(resolve(TABLE, "/projects/x"), Resolved::Index(3));
        assert_eq!(resolve(TABLE, "/index.html"), Resolved::Asset(3));
        assert_eq!(resolve(TABLE, "/assets/app-abc123.js"), Resolved::Asset(0));
        assert_eq!(resolve(TABLE, "/favicon.svg"), Resolved::Asset(2));
        assert_eq!(resolve(TABLE, "/assets/x.js"), Resolved::MissingAsset);
        assert_eq!(resolve(TABLE, "/assets/"), Resolved::MissingAsset);
        assert_eq!(
            resolve(TABLE, "/assets/../index.html"),
            Resolved::MissingAsset
        );
        assert_eq!(resolve(TABLE, "/api"), Resolved::Api);
        assert_eq!(resolve(TABLE, "/api/nope"), Resolved::Api);
        assert_eq!(resolve(TABLE, "/apis"), Resolved::Index(3));
        assert_eq!(resolve(&[], "/api/nope"), Resolved::Api);
        assert_eq!(resolve(&[], "/"), Resolved::NotBuilt);
        assert_eq!(resolve(&[], "/assets/x.js"), Resolved::NotBuilt);
    }

    #[tokio::test]
    async fn not_built_is_503_text() {
        for path in ["/", "/projects/x", "/assets/x.js"] {
            let r = respond(&[], &Method::GET, path);
            assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert_eq!(
                header(&r, header::CONTENT_TYPE),
                Some("text/plain; charset=utf-8")
            );
            assert_eq!(
                header(&r, header::CONTENT_SECURITY_POLICY),
                Some(CONTENT_SECURITY_POLICY)
            );
            assert_eq!(body(r).await, NOT_BUILT);
        }
    }

    #[tokio::test]
    async fn assets_and_index_carry_types_and_cache_headers() {
        let r = respond(TABLE, &Method::GET, "/assets/app-abc123.js");
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            header(&r, header::CONTENT_TYPE),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(header(&r, header::CACHE_CONTROL), Some(IMMUTABLE));
        assert_eq!(body(r).await, "console.log(1)");

        let r = respond(TABLE, &Method::HEAD, "/projects/x");
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            header(&r, header::CONTENT_TYPE),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(header(&r, header::CACHE_CONTROL), Some(NO_CACHE));
        assert_eq!(header(&r, header::X_CONTENT_TYPE_OPTIONS), Some("nosniff"));
        assert_eq!(header(&r, header::REFERRER_POLICY), Some("no-referrer"));

        let r = respond(TABLE, &Method::GET, "/favicon.svg");
        assert_eq!(header(&r, header::CACHE_CONTROL), Some(NO_CACHE));

        let r = respond(TABLE, &Method::GET, "/assets/x.js");
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            header(&r, header::CONTENT_SECURITY_POLICY),
            Some(CONTENT_SECURITY_POLICY)
        );
    }

    #[tokio::test]
    async fn only_get_and_head() {
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            let r = respond(TABLE, &method, "/");
            assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED, "{method}");
            assert_eq!(header(&r, header::ALLOW), Some("GET, HEAD"));
            assert_eq!(
                header(&r, header::CONTENT_SECURITY_POLICY),
                Some(CONTENT_SECURITY_POLICY)
            );
        }
    }

    #[tokio::test]
    async fn api_paths_keep_the_json_404() {
        for method in [Method::GET, Method::POST] {
            let r = respond(TABLE, &method, "/api/nope");
            assert_eq!(r.status(), StatusCode::NOT_FOUND);
            assert_eq!(header(&r, header::CONTENT_TYPE), Some("application/json"));
            assert_eq!(header(&r, header::CONTENT_SECURITY_POLICY), None);
            let v: serde_json::Value = serde_json::from_str(&body(r).await).unwrap();
            assert_eq!(v["code"], "not_found");
        }
    }
}
