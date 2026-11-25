//! The embedded UI as a running daemon serves it, over 127.0.0.1 and the
//! unix socket: `index.html` at `/` and on client routes, hashed assets
//! with their types and cache headers, the CSP on every UI response, and
//! `/api/*` unchanged.
//!
//! Needs a daemon built after `make web`; skipped with a message otherwise
//! (`make verify` always builds the UI first). The not-built 503 is covered
//! by `ui.rs`'s unit tests.

mod common;

use common::{Response, Via, get, send};
use ship_daemon::ui::{CONTENT_SECURITY_POLICY, is_built};

fn header<'a>(r: &'a Response, name: &str) -> Option<&'a str> {
    r.head.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn assert_ui_headers(r: &Response, what: &str) {
    assert_eq!(
        header(r, "content-security-policy"),
        Some(CONTENT_SECURITY_POLICY),
        "{what}"
    );
    assert_eq!(
        header(r, "x-content-type-options"),
        Some("nosniff"),
        "{what}"
    );
    assert_eq!(header(r, "referrer-policy"), Some("no-referrer"), "{what}");
}

/// `src="…"` and `href="…"` values in `html`.
fn references(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for attr in ["src=\"", "href=\""] {
        let mut rest = html;
        while let Some(i) = rest.find(attr) {
            rest = &rest[i + attr.len()..];
            let end = rest.find('"').unwrap_or(rest.len());
            out.push(rest[..end].to_owned());
            rest = &rest[end..];
        }
    }
    out
}

#[tokio::test]
async fn daemon_serves_the_ui() {
    if !is_built() {
        eprintln!("skipped: this daemon was built without web/dist; run `make web` first");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let running = ship_daemon::start(&common::config(dir.path()))
        .await
        .unwrap();
    let tcp = Via::Tcp(running.tcp_addr());
    let unix = Via::Unix(running.socket_path().to_path_buf());

    // index.html at / and on a client route, on both listeners.
    let index = get(&tcp, "/").await;
    assert_eq!(index.status, 200, "{}", index.head);
    assert_eq!(
        header(&index, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(header(&index, "cache-control"), Some("no-cache"));
    assert_ui_headers(&index, "/");
    for (via, path) in [(&tcp, "/projects/x"), (&unix, "/"), (&unix, "/projects/x")] {
        let r = get(via, path).await;
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.body, index.body, "{path}");
        assert_ui_headers(&r, path);
    }

    // No remote URL; every asset it names is served with its type.
    let refs = references(&index.body);
    let mut scripts = 0;
    let mut styles = 0;
    for r in &refs {
        assert!(
            !r.starts_with("http://") && !r.starts_with("https://") && !r.starts_with("//"),
            "remote URL in index.html: {r}"
        );
        if !r.starts_with("/assets/") {
            continue;
        }
        let asset = get(&tcp, r).await;
        assert_eq!(asset.status, 200, "{r}");
        assert_eq!(
            header(&asset, "cache-control"),
            Some("public, max-age=31536000, immutable"),
            "{r}"
        );
        assert_ui_headers(&asset, r);
        let kind = header(&asset, "content-type").unwrap_or_default();
        if r.ends_with(".js") {
            scripts += 1;
            assert_eq!(kind, "text/javascript; charset=utf-8", "{r}");
        } else if r.ends_with(".css") {
            styles += 1;
            assert_eq!(kind, "text/css; charset=utf-8", "{r}");
            assert!(
                asset.body.contains("prefers-color-scheme:dark")
                    || asset.body.contains("prefers-color-scheme: dark"),
                "no dark-scheme rule in {r}"
            );
        }
    }
    assert!(scripts >= 1 && styles >= 1, "{refs:?}");

    // Missing asset, API 404, HEAD, POST.
    let missing = get(&tcp, "/assets/x.js").await;
    assert_eq!(missing.status, 404);
    assert_ui_headers(&missing, "/assets/x.js");

    let api = get(&tcp, "/api/nope").await;
    assert_eq!(api.status, 404);
    assert_eq!(api.json()["code"], "not_found");
    assert_eq!(header(&api, "content-security-policy"), None);
    assert_eq!(header(&api, "ship-api-version"), Some("v1"));

    let head = send(&tcp, "HEAD", "/", &["Host: localhost"]).await;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());

    let post = send(&tcp, "POST", "/", &["Host: localhost"]).await;
    assert_eq!(post.status, 405);
    assert_eq!(header(&post, "allow"), Some("GET, HEAD"));
    assert_ui_headers(&post, "POST /");

    // The guards still run first: a foreign Host is refused before the UI.
    let foreign = send(&tcp, "GET", "/", &["Host: evil.example"]).await;
    assert_eq!(foreign.status, 403);

    running.shutdown().await.unwrap();
}
