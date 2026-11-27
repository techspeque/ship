//! `ship-core` performs no I/O (OVERVIEW §2.3): its sources name no
//! filesystem, network, process, environment, clock or async runtime API.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "std::fs",
    "std::net",
    "std::process",
    "std::env",
    "std::io",
    "std::os",
    "std::thread",
    "SystemTime",
    "Instant",
    "tokio",
    "env!",
    "option_env!",
    "include_str!",
    "include_bytes!",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn sources_use_no_io_apis() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(
        !files.is_empty(),
        "no sources found under {}",
        src.display()
    );

    let mut hits = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("read source");
        for (n, line) in text.lines().enumerate() {
            for pat in FORBIDDEN {
                if line.contains(pat) {
                    hits.push(format!("{}:{}: {pat}", file.display(), n + 1));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "I/O APIs in ship-core:\n{}",
        hits.join("\n")
    );
}
