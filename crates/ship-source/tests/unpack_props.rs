//! Property tests of the unpacker over generated archives, from a seeded
//! generator (no extra dependency): archives of valid entries unpack to
//! exactly those entries; an archive with any hostile entry fails as
//! unsafe; random corruption of valid archives never breaks the invariants
//! of `fuzzing::check_unpack`. Nothing outside the staging directory ever
//! changes, and a failure leaves no staging directory.

mod common;

use std::collections::BTreeMap;
use std::fs;

use common::{RawTar, Sandbox, Spec, tree, zstd};
use ship_source::{UnpackError, UnpackLimits, unpack};

/// xorshift64*: small, seeded, good enough to pick shapes.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }

    fn word(&mut self) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789-_.";
        let len = 1 + self.below(8) as usize;
        let mut w: String = (0..len)
            .map(|_| ALPHABET[self.below(ALPHABET.len() as u64) as usize] as char)
            .collect();
        // `.` and `..` are hostile; a word is never either.
        if w.chars().all(|c| c == '.') {
            w.push('x');
        }
        w
    }
}

/// A generated archive and what unpacking it must give.
struct Case {
    tar: Vec<u8>,
    hostile: Option<&'static str>,
    /// Path to file content, or `None` for a directory.
    expected: BTreeMap<String, Option<Vec<u8>>>,
}

fn generate(rng: &mut Rng, allow_hostile: bool) -> Case {
    let mut t = RawTar::new();
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut expected = BTreeMap::new();
    let mut hostile = None;
    let n = 1 + rng.below(14);
    for i in 0..n {
        if allow_hostile && hostile.is_none() && rng.chance(6) {
            hostile = Some(hostile_entry(rng, &mut t, &dirs, &files));
            // Entries after the hostile one never matter.
            if rng.chance(2) {
                break;
            }
            continue;
        }
        let parent = if dirs.is_empty() || rng.chance(3) {
            String::new()
        } else {
            format!("{}/", dirs[rng.below(dirs.len() as u64) as usize])
        };
        // The index keeps every name distinct, also after case folding.
        let mut name = format!("{}{i}", rng.word());
        if rng.chance(10) {
            name = format!("{}{name}", "n".repeat(110));
        }
        let path = format!("{parent}{name}");
        if path.len() > 100 {
            t.long_name(path.as_bytes());
        }
        let header_name = if path.len() > 100 {
            "long-name-placeholder".to_owned()
        } else {
            path.clone()
        };
        if rng.chance(3) {
            t.dir(&header_name);
            dirs.push(path.clone());
            expected.insert(path, None);
        } else {
            let len = rng.below(1500) as usize;
            let data = rng.bytes(len);
            t.file(&header_name, &data);
            files.push(path.clone());
            expected.insert(path, Some(data));
        }
    }
    Case {
        tar: t.finish(),
        hostile,
        expected,
    }
}

/// Appends one entry ADR-0005 rule 5 refuses and names it.
fn hostile_entry(rng: &mut Rng, t: &mut RawTar, dirs: &[String], files: &[String]) -> &'static str {
    let existing: Vec<&String> = dirs.iter().chain(files).collect();
    match rng.below(14) {
        0 => {
            t.file("/tmp/ship-prop-escape", b"x");
            "absolute"
        }
        1 => {
            t.file("../ship-prop-escape", b"x");
            "dotdot"
        }
        2 => {
            t.special(b"link", b'2', b"/");
            "symlink"
        }
        3 => {
            t.special(b"hard", b'1', b"/etc/passwd");
            "hardlink"
        }
        4 => {
            t.special(b"fifo", b'6', b"");
            "fifo"
        }
        5 => {
            t.special(b"dev", b'3', b"");
            "device"
        }
        6 => {
            t.raw(&Spec::new(b"pax", b'x', 0), b"");
            "pax"
        }
        7 if !existing.is_empty() => {
            let p = existing[rng.below(existing.len() as u64) as usize];
            if p.len() > 100 {
                t.long_name(p.as_bytes());
                t.file("x", b"dup");
            } else {
                t.file(p, b"dup");
            }
            "duplicate"
        }
        8 if !existing.is_empty() => {
            let p = existing[rng.below(existing.len() as u64) as usize].to_uppercase();
            t.long_name(p.as_bytes());
            t.file("x", b"case");
            "case collision"
        }
        9 => {
            t.file("no-such-dir/file", b"x");
            "missing parent"
        }
        10 => {
            let name = format!("../../{}", "z".repeat(120));
            t.long_name(name.as_bytes());
            t.file("innocent", b"x");
            "long name traversal"
        }
        11 => {
            t.file("bell\u{7}", b"x");
            "control character"
        }
        12 if !files.is_empty() => {
            let f = &files[rng.below(files.len() as u64) as usize];
            t.long_name(format!("{f}/child").as_bytes());
            t.file("x", b"x");
            "file as parent"
        }
        _ => {
            t.file("a/./b", b"x");
            "dot component"
        }
    }
}

#[test]
fn generated_archives_unpack_exactly_or_fail_as_unsafe() {
    let mut hostile_seen = 0;
    for seed in 0..2000u64 {
        let mut rng = Rng::new(seed);
        let case = generate(&mut rng, true);
        let sandbox = Sandbox::new();
        let result = unpack(
            &zstd(&case.tar)[..],
            &sandbox.staging(),
            &UnpackLimits::default(),
        );
        sandbox.assert_untouched();
        match (case.hostile, result) {
            (None, Ok(unpacked)) => {
                let got: BTreeMap<String, Option<Vec<u8>>> = tree(&sandbox.staging())
                    .into_iter()
                    .map(|(p, content, _)| (p, content))
                    .collect();
                assert_eq!(got, case.expected, "seed {seed}");
                assert_eq!(unpacked.entries, case.expected.len() as u64, "seed {seed}");
            }
            (Some(_), Err(UnpackError::Unsafe { .. })) => {
                hostile_seen += 1;
                assert!(
                    fs::symlink_metadata(sandbox.staging()).is_err(),
                    "seed {seed}: staging left"
                );
            }
            (hostile, result) => panic!("seed {seed}: hostile {hostile:?}, got {result:?}"),
        }
    }
    assert!(hostile_seen > 300, "only {hostile_seen} hostile cases");
}

#[test]
fn corrupted_archives_never_break_the_invariants() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = dir.path().join("sandbox");
    for seed in 0..2000u64 {
        let mut rng = Rng::new(seed ^ 0x5eed);
        let case = generate(&mut rng, false);
        let compressed_mode = rng.chance(2);
        let mut body = if compressed_mode {
            zstd(&case.tar)
        } else {
            case.tar.clone()
        };
        if body.is_empty() {
            continue;
        }
        match rng.below(3) {
            0 => {
                let cut = rng.below(body.len() as u64) as usize;
                body.truncate(cut);
            }
            _ => {
                for _ in 0..1 + rng.below(4) {
                    let at = rng.below(body.len() as u64) as usize;
                    body[at] = rng.next() as u8;
                }
            }
        }
        let mut input = vec![u8::from(compressed_mode)];
        input.extend_from_slice(&body);
        ship_source::fuzzing::check_unpack(&input, &sandbox);
    }
}
