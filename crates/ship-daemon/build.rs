//! Embeds the built UI (`web/dist`, from `make web`) into the daemon.
//!
//! Writes `$OUT_DIR/ui_assets.rs`, a sorted table of `(url path, bytes,
//! content type)` with one `include_bytes!` per file, which `src/ui.rs`
//! includes. If `web/dist` is missing the table is empty and the daemon
//! answers UI paths with a 503 naming `make web`; `SHIP_REQUIRE_UI=1` makes
//! that a build error instead, for release builds.

use std::path::{Path, PathBuf};

fn main() {
    let manifest = env_path("CARGO_MANIFEST_DIR");
    let dist = manifest.join("../../web/dist");
    println!("cargo::rerun-if-changed={}", dist.display());
    println!("cargo::rerun-if-env-changed=SHIP_REQUIRE_UI");
    let require = std::env::var_os("SHIP_REQUIRE_UI").is_some_and(|v| v == "1");

    let mut files = Vec::new();
    if dist.is_dir() {
        let root = dist.canonicalize().expect("canonicalise web/dist");
        walk(&root, &root, &mut files);
    }
    files.sort();
    let has_index = files.iter().any(|(url, _)| url == "/index.html");
    if require && !has_index {
        panic!("SHIP_REQUIRE_UI=1 but web/dist has no index.html; run `make web` first");
    }
    if !has_index && !files.is_empty() {
        println!("cargo::warning=web/dist has no index.html; serving the UI as not built");
        files.clear();
    }

    let mut out = String::from(
        "/// The embedded UI: `(url path, bytes, content type)`, sorted by path.\n\
         pub(crate) static ASSETS: &[(&str, &[u8], &str)] = &[\n",
    );
    for (url, path) in &files {
        let path = path.to_str().expect("web/dist paths are UTF-8");
        let kind = content_type(url);
        out.push_str(&format!(
            "    ({url:?}, include_bytes!({path:?}), {kind:?}),\n"
        ));
    }
    out.push_str("];\n");

    let target = env_path("OUT_DIR").join("ui_assets.rs");
    std::fs::write(target, out).expect("write ui_assets.rs");
}

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).expect(name))
}

/// Every regular file under `dir`, as (`/`-rooted URL path, absolute path).
fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    let entries = std::fs::read_dir(dir).expect("read web/dist");
    for entry in entries {
        let path = entry.expect("read web/dist entry").path();
        let meta = std::fs::metadata(&path).expect("stat web/dist entry");
        if meta.is_dir() {
            walk(root, &path, files);
        } else if meta.is_file() {
            let rel = path.strip_prefix(root).expect("under web/dist");
            let mut url = String::new();
            for part in rel.components() {
                let name = part.as_os_str().to_str();
                url.push('/');
                url.push_str(name.expect("web/dist names are UTF-8"));
            }
            files.push((url, path));
        }
    }
}

fn content_type(url: &str) -> &'static str {
    let ext = url.rsplit_once('.').map_or("", |(_, e)| e);
    match ext.to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}
