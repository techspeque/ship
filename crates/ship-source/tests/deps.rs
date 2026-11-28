//! `ship-source` is a leaf: of the workspace crates it may depend only on
//! `ship-core` (OVERVIEW §2.3).

const MANIFEST: &str = include_str!("../Cargo.toml");
const WORKSPACE_CRATES: &[&str] = &[
    "ship",
    "ship-daemon",
    "ship-docker",
    "ship-source",
    "ship-store",
    "ship-net",
];

#[test]
fn depends_on_no_workspace_crate_but_ship_core() {
    let mut section = String::new();
    for line in MANIFEST.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.to_string();
            continue;
        }
        if !section.contains("dependencies") {
            continue;
        }
        let Some((name, _)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().trim_matches('"');
        if name == "ship-core" {
            continue;
        }
        assert!(
            !WORKSPACE_CRATES.contains(&name),
            "{section} names workspace crate {name}"
        );
        assert!(
            !line.contains("path ="),
            "{section} has a path dependency: {line}"
        );
    }
}
