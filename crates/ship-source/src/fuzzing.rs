//! The unpacker's fuzz body, shared by the cargo-fuzz target in `fuzz/`
//! and the stable tests, so the target cannot drift from what CI checks.
//! Not part of the crate's API.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::unpack::{UnpackLimits, unpack};

/// Content of the canary file beside the staging directory.
const CANARY: &[u8] = b"canary: unpacking must never touch this file\n";

/// Small limits, so one input stays cheap.
pub const FUZZ_LIMITS: UnpackLimits = UnpackLimits {
    max_compressed_bytes: 1024 * 1024,
    max_bytes: 1024 * 1024,
    max_entries: 512,
    max_ratio: 100,
    ratio_grace_bytes: 1024 * 1024,
};

/// Unpacks `data` into a fresh staging directory inside `sandbox` and
/// panics if any invariant breaks:
///
/// - nothing in `sandbox` but the staging directory changes (a canary file
///   and the directory listing are checked);
/// - a failed unpack leaves no staging directory;
/// - a successful one leaves only directories and regular files.
///
/// An input whose first byte is even is the rest as a bare tar stream,
/// compressed here, so the fuzzer reaches the tar parser; otherwise the
/// rest is the compressed stream as sent.
pub fn check_unpack(data: &[u8], sandbox: &Path) {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let stream = if mode % 2 == 0 {
        compress(rest).expect("compressing in memory")
    } else {
        rest.to_vec()
    };
    let canary = sandbox.join("canary");
    let staging = sandbox.join("staging");
    fs::create_dir_all(sandbox).expect("sandbox");
    fs::write(&canary, CANARY).expect("canary");
    let before = listing(sandbox);

    let result = unpack(&stream[..], &staging, &FUZZ_LIMITS);

    assert_eq!(fs::read(&canary).expect("canary"), CANARY, "canary changed");
    match &result {
        Ok(_) => {
            assert!(staging.is_dir(), "no staging after success");
            assert_only_dirs_and_files(&staging);
            fs::remove_dir_all(&staging).expect("removing staging");
        }
        Err(e) => {
            assert!(
                fs::symlink_metadata(&staging).is_err(),
                "staging left after failure: {e}"
            );
        }
    }
    assert_eq!(listing(sandbox), before, "sandbox listing changed");
}

/// [`check_unpack`] in a per-process sandbox under the temporary
/// directory, for the fuzz target.
pub fn check_unpack_in_temp(data: &[u8]) {
    let sandbox = std::env::temp_dir().join(format!("ship-unpack-fuzz-{}", std::process::id()));
    check_unpack(data, &sandbox);
}

/// `tar` compressed as the packer does.
pub fn compress(tar: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), crate::ZSTD_LEVEL)?;
    encoder.write_all(tar)?;
    encoder.finish()
}

fn listing(dir: &Path) -> Vec<PathBuf> {
    let mut names: Vec<PathBuf> = fs::read_dir(dir)
        .expect("sandbox listing")
        .map(|e| e.expect("sandbox entry").path())
        .collect();
    names.sort();
    names
}

fn assert_only_dirs_and_files(root: &Path) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("staging listing") {
            let path = entry.expect("staging entry").path();
            let meta = fs::symlink_metadata(&path).expect("staging metadata");
            if meta.is_dir() {
                pending.push(path);
            } else {
                assert!(meta.is_file(), "not a regular file: {}", path.display());
            }
        }
    }
}
