//! The unpacker against real files: round trips of packed trees, every
//! hostile entry of ADR-0005 rule 5, the limits, and broken streams. In
//! every case nothing outside the staging directory changes, and a failure
//! leaves no staging directory.

mod common;

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use common::{RawTar, Sandbox, Spec, tree, zstd};
use ship_source::{
    Limit, Limits, Options, UnpackError, UnpackLimits, UnsafeReason, pack, scan, unpack,
};

fn options() -> Options {
    Options {
        include: Vec::new(),
        limits: Limits::default(),
        home: Some(PathBuf::from("/nonexistent/ship-test-home")),
    }
}

type Outcome = Result<ship_source::Unpacked, UnpackError>;

fn is_malformed(result: &Outcome) -> bool {
    matches!(result, Err(UnpackError::Malformed(_)))
}

/// Unpacks `stream` into a fresh sandbox and checks the sandbox afterwards.
fn run(stream: &[u8], limits: &UnpackLimits) -> (Sandbox, Outcome) {
    let sandbox = Sandbox::new();
    let result = unpack(stream, &sandbox.staging(), limits);
    sandbox.assert_untouched();
    if result.is_err() {
        assert!(
            fs::symlink_metadata(sandbox.staging()).is_err(),
            "staging left behind: {result:?}"
        );
    }
    (sandbox, result)
}

fn refused(tar: &[u8]) -> UnsafeReason {
    match run(&zstd(tar), &UnpackLimits::default()).1 {
        Err(UnpackError::Unsafe { reason, .. }) => reason,
        other => panic!("expected an unsafe entry, got {other:?}"),
    }
}

#[test]
fn a_packed_tree_round_trips() {
    let src = tempfile::tempdir().unwrap();
    let root = src.path();
    fs::create_dir_all(root.join("src/deep/er")).unwrap();
    fs::write(root.join("README.md"), b"hello\n").unwrap();
    fs::write(root.join("empty"), b"").unwrap();
    fs::write(root.join("src/main.rs"), b"fn main() {}\n").unwrap();
    fs::write(root.join("src/deep/er/data.bin"), vec![7u8; 70_000]).unwrap();
    fs::write(root.join("caf\u{e9}.txt"), b"unicode\n").unwrap();
    fs::write(root.join("run.sh"), b"#!/bin/sh\n").unwrap();
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    let long_dir = "d".repeat(90);
    let long_file = format!("{long_dir}/{}", "f".repeat(120));
    fs::create_dir(root.join(&long_dir)).unwrap();
    fs::write(root.join(&long_file), b"long\n").unwrap();

    let selection = scan(root, &options()).unwrap();
    let mut stream = Vec::new();
    let packed = pack(&selection, &mut stream).unwrap();

    let (sandbox, result) = run(&stream, &UnpackLimits::default());
    let unpacked = result.unwrap();
    assert_eq!(unpacked.sha256, packed.sha256);
    assert_eq!(unpacked.compressed_bytes, packed.compressed_bytes);
    assert_eq!(unpacked.files, 7);
    assert_eq!(unpacked.entries, 7 + 4);
    assert_eq!(unpacked.bytes, 6 + 13 + 70_000 + 8 + 10 + 5);
    assert_eq!(tree(&sandbox.staging()), tree(root));
    assert!(
        tree(&sandbox.staging())
            .iter()
            .any(|(p, _, exec)| p == "run.sh" && *exec)
    );
}

#[test]
fn every_hostile_entry_is_refused() {
    let nfc = "caf\u{e9}";
    let nfd = "cafe\u{301}";
    let long_escape = format!("../../{}", "x".repeat(120));
    let cases: Vec<(&str, Vec<u8>, UnsafeReason)> = vec![
        (
            "absolute",
            RawTar::new().file("/tmp/ship-escape", b"x").finish(),
            UnsafeReason::AbsolutePath,
        ),
        (
            "dotdot",
            RawTar::new().file("../ship-escape", b"x").finish(),
            UnsafeReason::ParentComponent,
        ),
        (
            "dotdot inside",
            RawTar::new()
                .dir("a")
                .file("a/../../ship-escape", b"x")
                .finish(),
            UnsafeReason::ParentComponent,
        ),
        (
            "dot",
            RawTar::new().file("./x", b"x").finish(),
            UnsafeReason::DotComponent,
        ),
        (
            "symlink",
            RawTar::new().special(b"link", b'2', b"/etc").finish(),
            UnsafeReason::Symlink,
        ),
        (
            "symlink then a file through it",
            RawTar::new()
                .special(b"link", b'2', b"..")
                .file("link/ship-escape", b"x")
                .finish(),
            UnsafeReason::Symlink,
        ),
        (
            "hardlink",
            RawTar::new().special(b"hl", b'1', b"/etc/passwd").finish(),
            UnsafeReason::Hardlink,
        ),
        (
            "char device",
            RawTar::new().special(b"null", b'3', b"").finish(),
            UnsafeReason::Device,
        ),
        (
            "block device",
            RawTar::new().special(b"sda", b'4', b"").finish(),
            UnsafeReason::Device,
        ),
        (
            "fifo",
            RawTar::new().special(b"pipe", b'6', b"").finish(),
            UnsafeReason::Fifo,
        ),
        (
            "pax local",
            RawTar::new()
                .raw(&Spec::new(b"pax", b'x', 22), b"22 path=../escape\n\0\0\0\0")
                .file("ok", b"x")
                .finish(),
            UnsafeReason::Pax,
        ),
        (
            "pax global",
            RawTar::new().special(b"pax", b'g', b"").finish(),
            UnsafeReason::Pax,
        ),
        (
            "gnu long link",
            RawTar::new().special(b"././@LongLink", b'K', b"").finish(),
            UnsafeReason::LinkTarget,
        ),
        (
            "sparse",
            RawTar::new().special(b"sparse", b'S', b"").finish(),
            UnsafeReason::UnsupportedType(b'S'),
        ),
        (
            "contiguous",
            RawTar::new().special(b"c", b'7', b"").finish(),
            UnsafeReason::UnsupportedType(b'7'),
        ),
        (
            "missing parent",
            RawTar::new().file("nodir/file", b"x").finish(),
            UnsafeReason::MissingParent,
        ),
        (
            "file as parent",
            RawTar::new().file("a", b"x").file("a/b", b"y").finish(),
            UnsafeReason::MissingParent,
        ),
        (
            "duplicate file",
            RawTar::new().file("a", b"x").file("a", b"y").finish(),
            UnsafeReason::Duplicate,
        ),
        (
            "file then directory",
            RawTar::new().file("a", b"x").dir("a").finish(),
            UnsafeReason::Duplicate,
        ),
        (
            "case collision",
            RawTar::new()
                .file("README", b"x")
                .file("Readme", b"y")
                .finish(),
            UnsafeReason::Collision,
        ),
        (
            "case collision of directories",
            RawTar::new().dir("Src").dir("src").finish(),
            UnsafeReason::Collision,
        ),
        (
            "unicode collision",
            RawTar::new().file(nfc, b"x").file(nfd, b"y").finish(),
            UnsafeReason::Collision,
        ),
        (
            "long name traversal",
            RawTar::new()
                .long_name(long_escape.as_bytes())
                .file("innocent", b"x")
                .finish(),
            UnsafeReason::ParentComponent,
        ),
        (
            "ustar prefix traversal",
            RawTar::new()
                .raw(
                    &Spec {
                        prefix: b"..",
                        ..Spec::new(b"ship-escape", b'0', 1)
                    },
                    b"x",
                )
                .finish(),
            UnsafeReason::ParentComponent,
        ),
        (
            "control character",
            RawTar::new().file("a\nb", b"x").finish(),
            UnsafeReason::ControlCharacter,
        ),
        (
            "not utf-8",
            RawTar::new()
                .raw(&Spec::new(b"a\xff", b'0', 1), b"x")
                .finish(),
            UnsafeReason::NotUtf8,
        ),
        (
            "good entries then a symlink",
            RawTar::new()
                .dir("a")
                .file("a/kept-until-the-failure", b"x")
                .special(b"a/link", b'2', b"/")
                .finish(),
            UnsafeReason::Symlink,
        ),
    ];
    for (name, tar, expected) in cases {
        assert_eq!(refused(&tar), expected, "{name}");
    }
}

#[test]
fn a_long_name_is_applied_to_the_next_entry() {
    let long = format!("{}/{}", "d".repeat(80), "f".repeat(80));
    let tar = RawTar::new()
        .dir(&"d".repeat(80))
        .long_name(long.as_bytes())
        .file("truncated-name-ignored", b"content")
        .finish();
    let (sandbox, result) = run(&zstd(&tar), &UnpackLimits::default());
    result.unwrap();
    assert_eq!(fs::read(sandbox.staging().join(&long)).unwrap(), b"content");
    assert!(!sandbox.staging().join("truncated-name-ignored").exists());
}

#[test]
fn long_names_are_bounded() {
    let huge = "x/".repeat(3000);
    let tar = RawTar::new()
        .long_name(huge.as_bytes())
        .file("a", b"x")
        .finish();
    assert_eq!(refused(&tar), UnsafeReason::PathTooLong);

    let tar = RawTar::new()
        .long_name(b"a")
        .long_name(b"b")
        .file("c", b"x")
        .finish();
    let (_, result) = run(&zstd(&tar), &UnpackLimits::default());
    assert!(is_malformed(&result), "{result:?}");
}

#[test]
fn the_byte_limit_is_checked_before_writing() {
    // A header claiming 4 GiB with no data behind it.
    let tar = RawTar::new()
        .raw(&Spec::new(b"big", b'0', 4 * 1024 * 1024 * 1024), b"")
        .finish();
    let (_, result) = run(&zstd(&tar), &UnpackLimits::default());
    assert!(
        matches!(
            result,
            Err(UnpackError::TooLarge {
                limit: Limit::Bytes,
                ..
            })
        ),
        "{result:?}"
    );

    let limits = UnpackLimits {
        max_bytes: 10,
        ..UnpackLimits::default()
    };
    let tar = RawTar::new()
        .file("a", b"123456")
        .file("b", b"123456")
        .finish();
    let (_, result) = run(&zstd(&tar), &limits);
    assert!(
        matches!(
            result,
            Err(UnpackError::TooLarge {
                limit: Limit::Bytes,
                max: 10
            })
        ),
        "{result:?}"
    );
}

#[test]
fn the_entry_and_compressed_limits_hold() {
    let mut t = RawTar::new();
    for i in 0..6 {
        t.file(&format!("f{i}"), b"x");
    }
    let tar = t.finish();
    let limits = UnpackLimits {
        max_entries: 5,
        ..UnpackLimits::default()
    };
    let (_, result) = run(&zstd(&tar), &limits);
    assert!(
        matches!(
            result,
            Err(UnpackError::TooLarge {
                limit: Limit::Entries,
                max: 5
            })
        ),
        "{result:?}"
    );

    // Incompressible content, so the stream is larger than the limit.
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let noise: Vec<u8> = (0..200_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect();
    let tar = RawTar::new().file("noise", &noise).finish();
    let limits = UnpackLimits {
        max_compressed_bytes: 50_000,
        ..UnpackLimits::default()
    };
    let (_, result) = run(&zstd(&tar), &limits);
    assert!(
        matches!(
            result,
            Err(UnpackError::TooLarge {
                limit: Limit::CompressedBytes,
                ..
            })
        ),
        "{result:?}"
    );
}

/// A file of `size` zeros, compressed while it is generated, so the test
/// never holds it.
fn zero_bomb(size: u64) -> Vec<u8> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    encoder
        .write_all(&common::header(&Spec::new(b"bomb", b'0', size)))
        .unwrap();
    let chunk = vec![0u8; 1024 * 1024];
    let mut left = size;
    while left > 0 {
        let n = left.min(chunk.len() as u64) as usize;
        encoder.write_all(&chunk[..n]).unwrap();
        left -= n as u64;
    }
    encoder.write_all(&[0u8; 1024]).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn a_decompression_bomb_is_stopped_by_the_ratio_with_bounded_disk_use() {
    let stream = zero_bomb(256 * 1024 * 1024);
    let limits = UnpackLimits {
        max_bytes: 1024 * 1024 * 1024,
        max_ratio: 100,
        ratio_grace_bytes: 1024 * 1024,
        ..UnpackLimits::default()
    };
    assert!(stream.len() < 1024 * 1024, "{} bytes", stream.len());

    let sandbox = Sandbox::new();
    let staging = sandbox.staging();
    // Watches the bomb's size on disk from beside the unpack.
    let watcher = {
        let file = staging.join("bomb");
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = done.clone();
        let handle = std::thread::spawn(move || {
            let mut peak = 0u64;
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(m) = fs::metadata(&file) {
                    peak = peak.max(m.len());
                }
                std::thread::yield_now();
            }
            peak
        });
        (done, handle)
    };
    let result = unpack(&stream[..], &staging, &limits);
    watcher.0.store(true, std::sync::atomic::Ordering::Relaxed);
    let peak = watcher.1.join().unwrap();

    let (expanded, compressed) = match result {
        Err(UnpackError::Expansion {
            expanded,
            compressed,
        }) => (expanded, compressed),
        other => panic!("expected the expansion limit, got {other:?}"),
    };
    // At most one read past the allowance.
    let bound = limits.expansion_allowance(compressed) + 128 * 1024;
    assert!(expanded <= bound, "{expanded} > {bound}");
    assert!(peak <= expanded, "{peak} bytes on disk");
    assert!(fs::symlink_metadata(&staging).is_err());
    sandbox.assert_untouched();
}

#[test]
fn a_broken_or_padded_stream_fails() {
    let good = RawTar::new().file("a", b"hello").finish();
    let stream = zstd(&good);

    // Truncated anywhere.
    for cut in [0, 1, stream.len() / 2, stream.len() - 1] {
        let (_, result) = run(&stream[..cut], &UnpackLimits::default());
        assert!(is_malformed(&result), "cut {cut}: {result:?}");
    }
    // Garbage after the zstd frame.
    let mut garbage = stream.clone();
    garbage.extend_from_slice(b"trailing garbage");
    let (_, result) = run(&garbage, &UnpackLimits::default());
    assert!(is_malformed(&result), "{result:?}");
    // Non-zero data after the end-of-archive blocks.
    let mut tail = good.clone();
    tail.extend_from_slice(b"hidden");
    let (_, result) = run(&zstd(&tail), &UnpackLimits::default());
    assert!(is_malformed(&result), "{result:?}");
    // No end-of-archive block.
    let open = RawTar::new().file("a", b"hello").bytes.clone();
    let (_, result) = run(&zstd(&open), &UnpackLimits::default());
    assert!(is_malformed(&result), "{result:?}");
    // A bad header checksum.
    let mut bad = good.clone();
    bad[0] = b'b';
    let (_, result) = run(&zstd(&bad), &UnpackLimits::default());
    assert!(is_malformed(&result), "{result:?}");
    // Not zstd at all.
    let (_, result) = run(&good, &UnpackLimits::default());
    assert!(is_malformed(&result), "{result:?}");
    // Zero padding after the end is fine.
    let mut padded = good.clone();
    padded.extend(std::iter::repeat_n(0u8, 10 * 512));
    let (_, result) = run(&zstd(&padded), &UnpackLimits::default());
    result.unwrap();
}

/// Yields its bytes, then fails like a dropped connection.
struct Broken<'a> {
    data: &'a [u8],
}

impl Read for Broken<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.data.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "connection reset",
            ));
        }
        let n = buf.len().min(self.data.len()).min(100);
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

#[test]
fn a_failing_source_is_a_transfer_error() {
    let stream = zstd(&RawTar::new().file("a", &[1u8; 5000]).finish());
    let sandbox = Sandbox::new();
    let result = unpack(
        Broken {
            data: &stream[..stream.len() / 2],
        },
        &sandbox.staging(),
        &UnpackLimits::default(),
    );
    assert!(
        matches!(result, Err(UnpackError::Transfer(_))),
        "{result:?}"
    );
    assert!(fs::symlink_metadata(sandbox.staging()).is_err());
    sandbox.assert_untouched();
}

#[test]
fn an_existing_staging_directory_is_refused_and_kept() {
    let sandbox = Sandbox::new();
    let staging = sandbox.staging();
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("keep"), b"mine").unwrap();
    let stream = zstd(&RawTar::new().file("a", b"x").finish());
    let result = unpack(&stream[..], &staging, &UnpackLimits::default());
    assert!(
        matches!(result, Err(UnpackError::Staging { .. })),
        "{result:?}"
    );
    assert_eq!(fs::read(staging.join("keep")).unwrap(), b"mine");
    assert!(!staging.join("a").exists());
}

#[test]
fn modes_are_normalised() {
    let mut spec = Spec::new(b"setuid", b'0', 1);
    spec.mode = 0o4777;
    let tar = RawTar::new()
        .raw(&spec, b"x")
        .exec("tool", b"y")
        .file("plain", b"z")
        .finish();
    let (sandbox, result) = run(&zstd(&tar), &UnpackLimits::default());
    result.unwrap();
    let mode = |name: &str| {
        fs::metadata(sandbox.staging().join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("setuid"), 0o755);
    assert_eq!(mode("tool"), 0o755);
    assert_eq!(mode("plain"), 0o644);
}

#[test]
fn the_fuzz_body_holds_on_a_hostile_corpus() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = dir.path().join("sandbox");
    let corpus: Vec<Vec<u8>> = vec![
        RawTar::new().file("a", b"x").finish(),
        RawTar::new().file("../a", b"x").finish(),
        RawTar::new().special(b"l", b'2', b"/").finish(),
        RawTar::new().dir("d").file("d/f", b"x").finish(),
        RawTar::new().long_name(b"../../x").file("y", b"x").finish(),
        Vec::new(),
        vec![0u8; 512],
        vec![0xffu8; 2048],
    ];
    for tar in &corpus {
        // Even first byte: the harness compresses the rest.
        let mut input = vec![0u8];
        input.extend_from_slice(tar);
        ship_source::fuzzing::check_unpack(&input, &sandbox);
        // Odd first byte: the rest is the stream as sent.
        let mut input = vec![1u8];
        input.extend_from_slice(&zstd(tar));
        ship_source::fuzzing::check_unpack(&input, &sandbox);
    }
}
