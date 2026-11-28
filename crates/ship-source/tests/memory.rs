//! Packing streams in bounded memory (ADR-0005 rule 9).
//!
//! One test in its own file, so it runs alone in its own process and the
//! process's peak resident set is its own.

#![cfg(unix)]

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use ship_source::{Options, pack, scan};
use tempfile::TempDir;

const FILES: usize = 8;
const FILE_BYTES: usize = 8 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;
const BOUND: u64 = 32 * 1024 * 1024;

/// Peak resident set size of this process, in bytes.
fn peak_rss() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage writes a full rusage into the pointer it is given.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(rc, 0);
    // SAFETY: initialised by the successful call above.
    let max = unsafe { usage.assume_init() }.ru_maxrss as u64;
    // Linux reports KiB, macOS bytes.
    if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024
    }
}

#[test]
fn packing_many_large_files_stays_under_a_memory_bound() {
    let dir = TempDir::new().unwrap();
    // xorshift64: incompressible enough that zstd cannot shrink the work.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut chunk = vec![0u8; CHUNK];
    for i in 0..FILES {
        let mut f = File::create(dir.path().join(format!("blob-{i}.bin"))).unwrap();
        for _ in 0..FILE_BYTES / CHUNK {
            for word in chunk.chunks_exact_mut(8) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                word.copy_from_slice(&state.to_le_bytes());
            }
            f.write_all(&chunk).unwrap();
        }
    }
    drop(chunk);

    let options = Options {
        home: Some(PathBuf::from("/nonexistent-ship-test-home")),
        ..Options::default()
    };
    let sel = scan(dir.path(), &options).unwrap();
    assert_eq!(sel.total_bytes(), (FILES * FILE_BYTES) as u64);

    let before = peak_rss();
    let packed = pack(&sel, std::io::sink()).unwrap();
    let after = peak_rss();

    // Incompressible input: the stream is at least as big as the input,
    // so a pack that buffered it would grow by more than the bound.
    assert!(packed.compressed_bytes >= sel.total_bytes());
    let growth = after.saturating_sub(before);
    assert!(
        growth < BOUND,
        "peak RSS grew by {growth} bytes packing {} bytes",
        sel.total_bytes()
    );
}
