//! Enforces the crate dependency rule of OVERVIEW §2.3 (ADR-0002 rule 4).
//!
//! `ship` and `ship-daemon` are top level and may depend on anything in the
//! workspace. `ship-core` depends on no workspace crate. Every other crate is
//! a leaf and may depend on `ship-core` only, so a newly added crate is held
//! to the leaf rule until this file names it otherwise.
//!
//! Manifests are scanned as text with no TOML dependency: inside any
//! `dependencies`, `dev-dependencies` or `build-dependencies` section
//! (including `target.*` and `[dependencies.<name>]` subtables, with keys
//! bare or quoted), any
//! whole-word mention of a workspace crate outside a comment counts as a
//! dependency. That covers plain keys, dotted `name.workspace = true` keys,
//! subtable headers and `package = "<name>"` renames.

use std::fs;
use std::path::{Path, PathBuf};

const TOP_LEVEL: &[&str] = &["ship", "ship-daemon"];
const CORE: &str = "ship-core";
const EXPECTED_CRATES: &[&str] = &[
    "ship",
    "ship-core",
    "ship-daemon",
    "ship-docker",
    "ship-net",
    "ship-source",
    "ship-store",
];
const DEPENDENCY_SEGMENTS: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

/// A workspace crate: its package name and the text of its `Cargo.toml`.
struct Manifest {
    name: String,
    text: String,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn workspace_manifests() -> Vec<Manifest> {
    let crates_dir = workspace_root().join("crates");
    let mut manifests: Vec<Manifest> = fs::read_dir(&crates_dir)
        .unwrap_or_else(|err| panic!("read {}: {err}", crates_dir.display()))
        .map(|entry| entry.expect("crates/ entry").path().join("Cargo.toml"))
        .filter(|path| path.is_file())
        .map(|path| {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
            let name = package_name(&text)
                .unwrap_or_else(|| panic!("no [package] name in {}", path.display()));
            Manifest { name, text }
        })
        .collect();
    manifests.sort_by(|a, b| a.name.cmp(&b.name));
    manifests
}

/// Drops a trailing `#` comment, ignoring `#` inside quoted strings.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if ch == '\\' => escaped = true,
            Some(open) if ch == open => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '#' => return &line[..index],
            None => {}
        }
    }
    line
}

/// The table path of a `[header]` or `[[header]]` line, if it is one.
fn table_header(line: &str) -> Option<&str> {
    let inner = line.strip_prefix('[')?.trim_start_matches('[');
    let inner = inner.strip_suffix(']')?.trim_end_matches(']');
    Some(inner.trim())
}

fn package_name(text: &str) -> Option<String> {
    let mut in_package = false;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if let Some(header) = table_header(line) {
            in_package = header == "package";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "name" {
            return Some(
                value
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string(),
            );
        }
    }
    None
}

/// The keys of a dotted table path, split on `.` outside quotes, each trimmed
/// and with surrounding `"` or `'` removed, so `"dependencies"` reads as
/// `dependencies` and `'cfg(a.b)'` stays one key.
fn table_keys(header: &str) -> Vec<&str> {
    let mut keys = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0;
    for (index, ch) in header.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if ch == '\\' => escaped = true,
            Some(open) if ch == open => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '.' => {
                keys.push(&header[start..index]);
                start = index + 1;
            }
            None => {}
        }
    }
    keys.push(&header[start..]);
    keys.into_iter()
        .map(|key| {
            let key = key.trim();
            key.strip_prefix('"')
                .and_then(|k| k.strip_suffix('"'))
                .or_else(|| key.strip_prefix('\'').and_then(|k| k.strip_suffix('\'')))
                .unwrap_or(key)
        })
        .collect()
}

fn is_dependency_table(header: &str) -> bool {
    table_keys(header)
        .iter()
        .any(|key| DEPENDENCY_SEGMENTS.contains(key))
}

fn is_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'
}

/// Whether `name` occurs in `line` with no crate-name character on either side.
fn mentions(line: &str, name: &str) -> bool {
    line.match_indices(name).any(|(start, _)| {
        let before = line[..start].chars().next_back();
        let after = line[start + name.len()..].chars().next();
        !before.is_some_and(is_name_char) && !after.is_some_and(is_name_char)
    })
}

/// Workspace crates from `candidates` that `text` declares as dependencies.
fn declared_dependencies(text: &str, candidates: &[&str]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut in_dependencies = false;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if let Some(header) = table_header(line) {
            in_dependencies = is_dependency_table(header);
        }
        if !in_dependencies {
            continue;
        }
        for candidate in candidates {
            if mentions(line, candidate) && !found.iter().any(|name| name == candidate) {
                found.push((*candidate).to_string());
            }
        }
    }
    found
}

/// Every breach of the dependency rule, one message per edge.
fn violations(manifests: &[Manifest]) -> Vec<String> {
    let mut messages = Vec::new();
    for manifest in manifests {
        if TOP_LEVEL.contains(&manifest.name.as_str()) {
            continue;
        }
        let others: Vec<&str> = manifests
            .iter()
            .map(|other| other.name.as_str())
            .filter(|other| *other != manifest.name)
            .collect();
        for dependency in declared_dependencies(&manifest.text, &others) {
            if manifest.name == CORE {
                messages.push(format!(
                    "{CORE} must not depend on any workspace crate, but depends on {dependency}"
                ));
            } else if dependency != CORE {
                messages.push(format!(
                    "leaf crate {} may depend only on {CORE}, but depends on {dependency}",
                    manifest.name
                ));
            }
        }
    }
    messages
}

#[test]
fn workspace_has_expected_crates() {
    let names: Vec<String> = workspace_manifests().into_iter().map(|m| m.name).collect();
    assert_eq!(names, EXPECTED_CRATES);
}

#[test]
fn workspace_respects_dependency_rule() {
    let found = violations(&workspace_manifests());
    assert!(
        found.is_empty(),
        "OVERVIEW §2.3 dependency rule broken:\n  {}",
        found.join("\n  ")
    );
}

// The checker itself, against synthetic manifests.

const LEAF_ON_DOCKER: &str =
    "leaf crate ship-store may depend only on ship-core, but depends on ship-docker";

/// Violations for a workspace of the real crate names where `subject` has
/// the manifest `text` and every other crate has no dependencies.
fn check(subject: &str, text: &str) -> Vec<String> {
    let manifests: Vec<Manifest> = EXPECTED_CRATES
        .iter()
        .map(|name| Manifest {
            name: (*name).to_string(),
            text: if *name == subject {
                text.to_string()
            } else {
                format!("[package]\nname = \"{name}\"\n")
            },
        })
        .collect();
    violations(&manifests)
}

fn check_leaf(text: &str) -> Vec<String> {
    check("ship-store", text)
}

#[test]
fn detects_leaf_to_leaf_plain_dependency() {
    let text = "[dependencies]\nship-docker = { path = \"../ship-docker\" }\n";
    assert_eq!(check_leaf(text), [LEAF_ON_DOCKER]);
}

#[test]
fn detects_leaf_to_leaf_workspace_key() {
    let text = "[dependencies]\nship-docker.workspace = true\n";
    assert_eq!(check_leaf(text), [LEAF_ON_DOCKER]);
}

#[test]
fn detects_leaf_to_leaf_subtable() {
    let text = "[dependencies.ship-docker]\npath = \"../ship-docker\"\n";
    assert_eq!(check_leaf(text), [LEAF_ON_DOCKER]);
}

#[test]
fn detects_leaf_to_leaf_package_rename() {
    let text = "[dependencies]\ndocker = { package = \"ship-docker\", path = \"x\" }\n";
    assert_eq!(check_leaf(text), [LEAF_ON_DOCKER]);
}

#[test]
fn detects_leaf_to_leaf_dev_dependency() {
    let text = "[dev-dependencies]\nship-docker = { path = \"../ship-docker\" }\n";
    assert_eq!(check_leaf(text), [LEAF_ON_DOCKER]);
}

#[test]
fn detects_leaf_to_leaf_build_dependency() {
    let text = "[build-dependencies]\nship-docker = { path = \"../ship-docker\" }\n";
    assert_eq!(check_leaf(text), [LEAF_ON_DOCKER]);
}

#[test]
fn detects_leaf_to_leaf_target_dependency() {
    let text = "[target.'cfg(unix)'.dependencies]\nship-docker = { path = \"x\" }\n";
    assert_eq!(check_leaf(text), [LEAF_ON_DOCKER]);
}

#[test]
fn detects_leaf_to_leaf_quoted_section_names() {
    for header in [
        "[\"dependencies\"]",
        "['dependencies']",
        "[ \"dev-dependencies\" ]",
        "['build-dependencies']",
        "[target.\"cfg(unix)\".\"dependencies\"]",
        "[target.'cfg(any(a.b))'.'dev-dependencies']",
        "[\"target\".\"cfg(unix)\".\"build-dependencies\"]",
    ] {
        let text = format!("{header}\nship-docker = {{ path = \"../ship-docker\" }}\n");
        assert_eq!(check_leaf(&text), [LEAF_ON_DOCKER], "header {header}");
    }
}

#[test]
fn detects_leaf_to_leaf_quoted_subtable() {
    let text = "[\"dependencies\".\"ship-docker\"]\npath = \"../ship-docker\"\n";
    assert_eq!(check_leaf(text), [LEAF_ON_DOCKER]);
}

#[test]
fn detects_leaf_to_top_level_dependency() {
    let text = "[dependencies]\nship-daemon = { path = \"../ship-daemon\" }\n";
    let expected = "leaf crate ship-store may depend only on ship-core, but depends on ship-daemon";
    assert_eq!(check_leaf(text), [expected]);
}

#[test]
fn allows_leaf_on_core() {
    let text = "[dependencies]\nship-core = { path = \"../ship-core\" }\n";
    assert!(check_leaf(text).is_empty());
}

#[test]
fn rejects_core_on_any_workspace_crate() {
    let text = "[dependencies]\nship-store = { path = \"../ship-store\" }\n";
    let expected = "ship-core must not depend on any workspace crate, but depends on ship-store";
    assert_eq!(check("ship-core", text), [expected]);
}

#[test]
fn ignores_comments_and_non_dependency_sections() {
    let text = "[package]\nname = \"ship-store\"\ndescription = \"uses ship-docker\"\n\
                [dependencies]\n# ship-docker = { path = \"../ship-docker\" }\n\
                serde = \"1\" # not ship-docker\n";
    assert!(check_leaf(text).is_empty());
}
