//! Snapshot pack against real trees in temporary directories (ADR-0005
//! rules 1, 2, 3, 4, 6 and 8).

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use ship_source::{
    EntryKind, Exclusion, Limits, Options, PackError, ScanError, Selection, SkipReason, pack, scan,
};
use tempfile::TempDir;

/// Options whose home directory is unrelated to any test tree.
fn opts() -> Options {
    Options {
        home: Some(PathBuf::from("/nonexistent-ship-test-home")),
        ..Options::default()
    }
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn paths(sel: &Selection) -> Vec<String> {
    sel.entries().iter().map(|e| e.path.clone()).collect()
}

fn excluded(sel: &Selection) -> Vec<(String, Exclusion)> {
    sel.excluded()
        .iter()
        .map(|e| (e.path.clone(), e.kind))
        .collect()
}

/// Archive entries by path: content (`None` for a directory) and mode.
type Archive = BTreeMap<String, (Option<Vec<u8>>, u32)>;

/// Pack and decode: returns the hash and every archive entry.
fn pack_and_read(sel: &Selection) -> (String, Archive) {
    let mut bytes = Vec::new();
    let packed = pack(sel, &mut bytes).unwrap();
    assert_eq!(packed.compressed_bytes, bytes.len() as u64);
    assert_eq!(packed.sha256.len(), 64);
    assert!(
        packed
            .sha256
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );

    let decoder = zstd::stream::read::Decoder::new(&bytes[..]).unwrap();
    let mut archive = tar::Archive::new(decoder);
    let mut out = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let h = entry.header();
        assert_eq!(h.mtime().unwrap(), 0);
        assert_eq!(h.uid().unwrap(), 0);
        assert_eq!(h.gid().unwrap(), 0);
        let mode = h.mode().unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let path = path.trim_end_matches('/').to_string();
        let content = if h.entry_type().is_dir() {
            None
        } else {
            assert!(h.entry_type().is_file(), "{path}");
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).unwrap();
            Some(buf)
        };
        out.insert(path, (content, mode));
    }
    (packed.sha256, out)
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=ship-test",
            "-c",
            "user.email=ship-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn working_tree_files_are_selected_and_ignored_files_are_not() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "*.log\n/build/\n");
    write(root, "src/main.rs", "fn main() {}\n");
    write(root, "README.md", "committed\n");

    if git_available() {
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "init"]);
    } else {
        fs::create_dir_all(root.join(".git")).unwrap();
        write(root, ".git/HEAD", "ref: refs/heads/main\n");
    }
    // Modified after the commit, untracked, and ignored.
    write(root, "README.md", "modified, not committed\n");
    write(root, "notes.txt", "untracked\n");
    write(root, "debug.log", "ignored\n");
    write(root, "build/out.bin", "ignored directory\n");

    let sel = scan(root, &opts()).unwrap();
    let (_, archive) = pack_and_read(&sel);

    assert_eq!(
        archive["README.md"].0.as_deref(),
        Some(&b"modified, not committed\n"[..])
    );
    assert!(archive.contains_key("notes.txt"));
    assert!(archive.contains_key("src/main.rs"));
    assert!(archive.contains_key(".gitignore"));
    assert!(!archive.contains_key("debug.log"));
    assert!(!archive.contains_key("build"));
    assert!(!archive.contains_key("build/out.bin"));
    assert!(
        !archive
            .keys()
            .any(|k| k == ".git" || k.starts_with(".git/"))
    );
    assert_eq!(
        excluded(&sel),
        vec![(".git".to_string(), Exclusion::GitMetadata)]
    );
}

#[test]
fn a_tree_without_git_uses_gitignore_the_same_way() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "*.log\n");
    write(root, "app.py", "print()\n");
    write(root, "x.log", "ignored\n");

    let sel = scan(root, &opts()).unwrap();
    assert_eq!(paths(&sel), [".gitignore", "app.py"]);
}

#[test]
fn shipignore_adds_and_overrides_rules() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "*.log\n");
    write(root, ".shipignore", "docs/\n!keep.log\n");
    write(root, "keep.log", "re-included by .shipignore\n");
    write(root, "drop.log", "ignored by .gitignore\n");
    write(root, "docs/guide.md", "ignored by .shipignore\n");
    write(root, "main.go", "package main\n");

    let sel = scan(root, &opts()).unwrap();
    assert_eq!(
        paths(&sel),
        [".gitignore", ".shipignore", "keep.log", "main.go"]
    );
}

#[test]
fn nested_ignore_files_apply_to_their_subtree() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "*.tmp\n");
    write(root, "a/.gitignore", "secret.txt\n!keep.tmp\n");
    write(root, "a/secret.txt", "ignored in a/\n");
    write(root, "a/keep.tmp", "re-included in a/\n");
    write(root, "a/drop.tmp", "ignored from the root\n");
    write(root, "b/secret.txt", "not ignored in b/\n");

    let sel = scan(root, &opts()).unwrap();
    assert_eq!(
        paths(&sel),
        [
            ".gitignore",
            "a",
            "b",
            "a/.gitignore",
            "a/keep.tmp",
            "b/secret.txt"
        ]
    );
}

#[test]
fn dockerignore_is_sent_and_not_applied() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".dockerignore", "*.md\ntests/\n");
    write(root, "README.md", "readme\n");
    write(root, "tests/t.sh", "#!/bin/sh\n");

    let sel = scan(root, &opts()).unwrap();
    assert_eq!(
        paths(&sel),
        [".dockerignore", "README.md", "tests", "tests/t.sh"]
    );
}

#[test]
fn sensitive_files_are_excluded_by_default_and_reported() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".env", "SECRET=1\n");
    write(root, ".env.local", "SECRET=2\n");
    write(root, ".env.example", "SECRET=\n");
    write(root, "deploy/id_ed25519", "PRIVATE\n");
    write(root, "deploy/id_ed25519.pub", "public\n");
    write(root, "certs/tls.key", "PRIVATE\n");
    write(root, ".npmrc", "//registry/:_authToken=x\n");
    write(root, ".aws/credentials", "[default]\n");
    write(root, "node_modules/left-pad/index.js", "\n");
    write(root, "target/debug/app", "\n");
    write(root, "app.js", "\n");
    // Also ignored: still reported as a default exclusion.
    write(root, ".gitignore", ".env\n");

    let sel = scan(root, &opts()).unwrap();
    let (_, archive) = pack_and_read(&sel);
    let names: Vec<&str> = archive.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            ".env.example",
            ".gitignore",
            "app.js",
            "certs",
            "deploy",
            "deploy/id_ed25519.pub"
        ]
    );
    assert_eq!(
        excluded(&sel),
        vec![
            (".aws".to_string(), Exclusion::CredentialStore),
            (".env".to_string(), Exclusion::EnvFile),
            (".env.local".to_string(), Exclusion::EnvFile),
            (".npmrc".to_string(), Exclusion::CredentialStore),
            ("node_modules".to_string(), Exclusion::BuildDirectory),
            ("target".to_string(), Exclusion::BuildDirectory),
            ("certs/tls.key".to_string(), Exclusion::PrivateKey),
            ("deploy/id_ed25519".to_string(), Exclusion::PrivateKey),
        ]
    );

    let report = sel.report();
    assert_eq!(report.file_count, 4);
    assert_eq!(report.directory_count, 2);
    assert_eq!(report.excluded.len(), 8);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["excluded"][1]["path"], ".env");
    assert_eq!(json["excluded"][1]["kind"], "env_file");
    let text = report.to_string();
    assert!(text.contains("4 files"), "{text}");
    assert!(text.contains(".env.local  (env_file)"), "{text}");
    // The report names files; it never holds their content.
    assert!(!text.contains("SECRET"));
    assert!(!json.to_string().contains("SECRET"));
}

#[test]
fn a_named_path_is_included() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".gitignore", ".env.production\n");
    write(root, ".env.production", "A=1\n");
    write(root, ".env.local", "B=2\n");
    write(root, ".aws/config", "[profile]\n");
    write(root, ".aws/credentials", "[default]\n");
    write(root, "app.js", "\n");

    let options = Options {
        include: vec![
            PathBuf::from(".env.production"),
            PathBuf::from("./.aws/config"),
        ],
        ..opts()
    };
    let sel = scan(root, &options).unwrap();
    assert_eq!(
        paths(&sel),
        [
            ".aws",
            ".env.production",
            ".gitignore",
            "app.js",
            ".aws/config"
        ]
    );
    // `.aws` is still reported: everything in it but the named file stays.
    assert_eq!(
        excluded(&sel),
        vec![
            (".aws".to_string(), Exclusion::CredentialStore),
            (".env.local".to_string(), Exclusion::EnvFile),
        ]
    );

    let missing = Options {
        include: vec![PathBuf::from("nope.txt")],
        ..opts()
    };
    assert!(matches!(
        scan(root, &missing),
        Err(ScanError::IncludeNotFound(p)) if p == "nope.txt"
    ));
    let outside = Options {
        include: vec![PathBuf::from("../elsewhere")],
        ..opts()
    };
    assert!(matches!(
        scan(root, &outside),
        Err(ScanError::InvalidInclude(_))
    ));
}

#[test]
fn a_named_directory_includes_its_descendants() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "*.log\n");
    write(root, "secrets/.env", "A=1\n");
    write(root, "secrets/app.log", "ignored\n");
    write(root, "secrets/inner/id_rsa", "key\n");
    write(root, "secrets/inner/run.log", "ignored\n");

    let options = Options {
        include: vec![PathBuf::from("secrets")],
        ..opts()
    };
    let sel = scan(root, &options).unwrap();
    assert_eq!(
        paths(&sel),
        [
            ".gitignore",
            "secrets",
            "secrets/.env",
            "secrets/app.log",
            "secrets/inner",
            "secrets/inner/id_rsa",
            "secrets/inner/run.log",
        ]
    );
    assert_eq!(excluded(&sel), vec![]);
}

#[test]
fn a_named_path_does_not_include_a_sibling_it_prefixes() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "*.log\n");
    write(root, "secrets/.env", "A=1\n");
    write(root, "secrets-old/.env", "B=2\n");
    write(root, "secrets-old/app.log", "ignored\n");
    write(root, "secrets-old/keep.txt", "kept\n");
    write(root, ".env", "C=3\n");
    write(root, ".env.local", "D=4\n");

    let options = Options {
        include: vec![PathBuf::from("secrets"), PathBuf::from(".env")],
        ..opts()
    };
    let sel = scan(root, &options).unwrap();
    assert_eq!(
        paths(&sel),
        [
            ".env",
            ".gitignore",
            "secrets",
            "secrets-old",
            "secrets/.env",
            "secrets-old/keep.txt",
        ]
    );
    assert_eq!(
        excluded(&sel),
        vec![
            (".env.local".to_string(), Exclusion::EnvFile),
            ("secrets-old/.env".to_string(), Exclusion::EnvFile),
        ]
    );
}

#[cfg(unix)]
#[test]
fn symlinks_are_skipped_and_never_followed() {
    use std::os::unix::fs::symlink;

    let outside = TempDir::new().unwrap();
    write(outside.path(), "secret.txt", "outside the tree\n");
    write(outside.path(), "dir/inner.txt", "outside the tree\n");

    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "real.txt", "real\n");
    symlink("real.txt", root.join("to-file")).unwrap();
    symlink(outside.path().join("secret.txt"), root.join("to-outside")).unwrap();
    symlink(outside.path().join("dir"), root.join("to-dir")).unwrap();
    symlink("missing", root.join("dangling")).unwrap();

    let sel = scan(root, &opts()).unwrap();
    assert_eq!(paths(&sel), ["real.txt"]);
    let skipped: Vec<(&str, SkipReason)> = sel
        .skipped()
        .iter()
        .map(|s| (s.path.as_str(), s.reason))
        .collect();
    assert_eq!(
        skipped,
        [
            ("dangling", SkipReason::Symlink),
            ("to-dir", SkipReason::Symlink),
            ("to-file", SkipReason::Symlink),
            ("to-outside", SkipReason::Symlink),
        ]
    );
    let (_, archive) = pack_and_read(&sel);
    assert_eq!(archive.len(), 1);
    assert!(
        sel.report()
            .to_string()
            .contains("to-outside  (symlink, not followed)")
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_swapped_in_after_the_scan_is_not_followed() {
    use std::os::unix::fs::symlink;

    let outside = TempDir::new().unwrap();
    write(outside.path(), "secret.txt", "12345\n");
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "a.txt", "abcde\n");

    let sel = scan(root, &opts()).unwrap();
    fs::remove_file(root.join("a.txt")).unwrap();
    symlink(outside.path().join("secret.txt"), root.join("a.txt")).unwrap();
    assert!(matches!(
        pack(&sel, std::io::sink()),
        Err(PackError::Changed(_))
    ));
}

#[cfg(unix)]
#[test]
fn a_parent_directory_swapped_for_a_symlink_after_the_scan_is_not_followed() {
    use std::os::unix::fs::symlink;

    // Same name and length as the scanned file, so only the parent differs.
    let outside = TempDir::new().unwrap();
    write(outside.path(), "data", "SECRET\n");
    write(outside.path(), "er/data", "SECRET\n");

    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "sub/data", "public\n");
    let sel = scan(root, &opts()).unwrap();
    fs::remove_dir_all(root.join("sub")).unwrap();
    symlink(outside.path(), root.join("sub")).unwrap();
    assert!(matches!(
        pack(&sel, std::io::sink()),
        Err(PackError::Changed(_))
    ));

    // A component above the file's parent, swapped the same way.
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "deep/er/data", "public\n");
    let sel = scan(root, &opts()).unwrap();
    fs::remove_dir_all(root.join("deep")).unwrap();
    symlink(outside.path(), root.join("deep")).unwrap();
    assert!(matches!(
        pack(&sel, std::io::sink()),
        Err(PackError::Changed(_))
    ));
}

#[cfg(unix)]
#[test]
fn a_fifo_is_skipped() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "a.txt", "a\n");
    let fifo = std::ffi::CString::new(root.join("pipe").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let sel = scan(root, &opts()).unwrap();
    assert_eq!(paths(&sel), ["a.txt"]);
    assert_eq!(sel.skipped()[0].reason, SkipReason::Special);
    pack(&sel, std::io::sink()).unwrap();
}

#[test]
fn the_same_tree_packs_to_the_same_hash() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "a.txt", "alpha\n");
    write(root, "sub/b.txt", "beta\n");
    write(root, "sub/deeper/c.txt", "gamma\n");
    let long = format!("{}/file.txt", "long-directory-name".repeat(8));
    write(root, &long, "long path\n");

    let first = pack_and_read(&scan(root, &opts()).unwrap());
    let second = pack_and_read(&scan(root, &opts()).unwrap());
    assert_eq!(first.0, second.0);
    assert!(first.1.contains_key(&long));

    // Timestamps are not part of the snapshot.
    let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    fs::File::options()
        .write(true)
        .open(root.join("a.txt"))
        .unwrap()
        .set_modified(t)
        .unwrap();
    let third = pack_and_read(&scan(root, &opts()).unwrap());
    assert_eq!(first.0, third.0);

    // Content is.
    write(root, "a.txt", "alpha!\n");
    let fourth = pack_and_read(&scan(root, &opts()).unwrap());
    assert_ne!(first.0, fourth.0);
}

#[test]
fn the_hash_is_the_sha256_of_the_bytes_written() {
    use sha2::{Digest, Sha256};

    let dir = TempDir::new().unwrap();
    write(dir.path(), "a.txt", "alpha\n");
    let sel = scan(dir.path(), &opts()).unwrap();
    let mut bytes = Vec::new();
    let packed = pack(&sel, &mut bytes).unwrap();
    let hex: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(packed.sha256, hex);
}

#[cfg(unix)]
#[test]
fn archive_holds_content_and_modes() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "run.sh", "#!/bin/sh\necho hi\n");
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o700)).unwrap();
    write(root, "data/plain.txt", "plain\n");
    fs::set_permissions(
        root.join("data/plain.txt"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    fs::create_dir(root.join("empty")).unwrap();

    let sel = scan(root, &opts()).unwrap();
    assert!(matches!(
        sel.entries()
            .iter()
            .find(|e| e.path == "run.sh")
            .unwrap()
            .kind,
        EntryKind::File {
            executable: true,
            ..
        }
    ));
    let (_, archive) = pack_and_read(&sel);
    assert_eq!(
        archive["run.sh"],
        (Some(b"#!/bin/sh\necho hi\n".to_vec()), 0o755)
    );
    assert_eq!(
        archive["data/plain.txt"],
        (Some(b"plain\n".to_vec()), 0o644)
    );
    assert_eq!(archive["data"], (None, 0o755));
    assert_eq!(archive["empty"], (None, 0o755));
    assert_eq!(archive.len(), 4);
}

#[test]
fn a_file_changed_since_the_scan_fails_the_pack() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "a.txt", "abc\n");
    let sel = scan(root, &opts()).unwrap();

    write(root, "a.txt", "longer now\n");
    assert!(matches!(
        pack(&sel, std::io::sink()),
        Err(PackError::Changed(_))
    ));

    fs::remove_file(root.join("a.txt")).unwrap();
    assert!(matches!(
        pack(&sel, std::io::sink()),
        Err(PackError::Changed(_))
    ));
}

#[test]
fn home_directory_and_its_ancestors_are_refused() {
    let home = TempDir::new().unwrap();
    write(home.path(), "project/app.js", "\n");
    let options = Options {
        home: Some(home.path().to_path_buf()),
        ..Options::default()
    };

    assert!(matches!(
        scan(home.path(), &options),
        Err(ScanError::HomeDirectory(_))
    ));
    assert!(matches!(
        scan(home.path().parent().unwrap(), &options),
        Err(ScanError::HomeDirectory(_))
    ));
    assert!(scan(&home.path().join("project"), &options).is_ok());
}

#[test]
fn a_filesystem_root_is_refused() {
    assert!(matches!(
        scan(Path::new("/"), &opts()),
        Err(ScanError::FilesystemRoot(_))
    ));
}

#[test]
fn a_file_is_not_a_tree() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), "a.txt", "a\n");
    assert!(matches!(
        scan(&dir.path().join("a.txt"), &opts()),
        Err(ScanError::NotADirectory(_))
    ));
}

#[test]
fn trees_over_the_limits_are_refused() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "a.txt", "12345");
    write(root, "b.txt", "12345");
    write(root, "c.txt", "12345");

    let few = Options {
        limits: Limits {
            max_files: 2,
            max_bytes: u64::MAX,
        },
        ..opts()
    };
    assert!(matches!(
        scan(root, &few),
        Err(ScanError::TooManyFiles { limit: 2 })
    ));

    let small = Options {
        limits: Limits {
            max_files: 100,
            max_bytes: 14,
        },
        ..opts()
    };
    assert!(matches!(
        scan(root, &small),
        Err(ScanError::TooLarge { limit: 14 })
    ));

    let exact = Options {
        limits: Limits {
            max_files: 3,
            max_bytes: 15,
        },
        ..opts()
    };
    let sel = scan(root, &exact).unwrap();
    assert_eq!((sel.file_count(), sel.total_bytes()), (3, 15));
}
