//! A raw tar writer for building archives the packer never would: any
//! name, any entry type, any size field.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

pub const BLOCK: usize = 512;

/// One header's fields.
#[derive(Debug, Clone)]
pub struct Spec<'a> {
    pub name: &'a [u8],
    pub typeflag: u8,
    pub mode: u32,
    /// The size field; the data written is `data`, whatever this says.
    pub size: u64,
    pub link: &'a [u8],
    /// A POSIX ustar prefix.
    pub prefix: &'a [u8],
}

impl<'a> Spec<'a> {
    pub fn new(name: &'a [u8], typeflag: u8, size: u64) -> Self {
        Spec {
            name,
            typeflag,
            mode: 0o644,
            size,
            link: b"",
            prefix: b"",
        }
    }
}

fn octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let text = format!("{value:0width$o}");
    assert!(text.len() <= width, "{value} does not fit");
    field[..width].copy_from_slice(text.as_bytes());
    field[width] = 0;
}

/// One 512-byte header block with a valid checksum.
pub fn header(spec: &Spec) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    assert!(spec.name.len() <= 100, "name too long for a header");
    h[..spec.name.len()].copy_from_slice(spec.name);
    octal(&mut h[100..108], u64::from(spec.mode));
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], spec.size);
    octal(&mut h[136..148], 0);
    h[156] = spec.typeflag;
    h[157..157 + spec.link.len()].copy_from_slice(spec.link);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[345..345 + spec.prefix.len()].copy_from_slice(spec.prefix);
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

/// A tar stream under construction.
#[derive(Debug, Default, Clone)]
pub struct RawTar {
    pub bytes: Vec<u8>,
}

impl RawTar {
    pub fn new() -> Self {
        RawTar::default()
    }

    /// A header and `data`, padded to the next block.
    pub fn raw(&mut self, spec: &Spec, data: &[u8]) -> &mut Self {
        self.bytes.extend_from_slice(&header(spec));
        self.bytes.extend_from_slice(data);
        let pad = (BLOCK - data.len() % BLOCK) % BLOCK;
        self.bytes.extend(std::iter::repeat_n(0u8, pad));
        self
    }

    pub fn file(&mut self, name: &str, data: &[u8]) -> &mut Self {
        self.raw(&Spec::new(name.as_bytes(), b'0', data.len() as u64), data)
    }

    pub fn exec(&mut self, name: &str, data: &[u8]) -> &mut Self {
        let mut spec = Spec::new(name.as_bytes(), b'0', data.len() as u64);
        spec.mode = 0o755;
        self.raw(&spec, data)
    }

    pub fn dir(&mut self, name: &str) -> &mut Self {
        let mut spec = Spec::new(name.as_bytes(), b'5', 0);
        spec.mode = 0o755;
        self.raw(&spec, b"")
    }

    /// An entry of `typeflag` with no data.
    pub fn special(&mut self, name: &[u8], typeflag: u8, link: &[u8]) -> &mut Self {
        let mut spec = Spec::new(name, typeflag, 0);
        spec.link = link;
        self.raw(&spec, b"")
    }

    /// A GNU long name for the next entry.
    pub fn long_name(&mut self, name: &[u8]) -> &mut Self {
        let mut data = name.to_vec();
        data.push(0);
        self.raw(&Spec::new(b"././@LongLink", b'L', data.len() as u64), &data)
    }

    /// The two end-of-archive blocks.
    pub fn end(&mut self) -> &mut Self {
        self.bytes.extend(std::iter::repeat_n(0u8, 2 * BLOCK));
        self
    }

    pub fn finish(&mut self) -> Vec<u8> {
        self.end();
        std::mem::take(&mut self.bytes)
    }
}

/// `tar` compressed as the packer does.
pub fn zstd(tar: &[u8]) -> Vec<u8> {
    ship_source::fuzzing::compress(tar).unwrap()
}

/// A sandbox holding a canary file beside where the staging directory goes.
pub struct Sandbox {
    pub dir: tempfile::TempDir,
}

pub const CANARY: &[u8] = b"do not touch\n";

impl Sandbox {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("canary"), CANARY).unwrap();
        Sandbox { dir }
    }

    pub fn staging(&self) -> PathBuf {
        self.dir.path().join("staging")
    }

    /// Every path under the sandbox, staging excluded, with file content.
    pub fn outside(&self) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        let staging = self.staging();
        let mut out = Vec::new();
        walk(self.dir.path(), &mut |p| {
            if p.starts_with(&staging) {
                return;
            }
            let meta = fs::symlink_metadata(p).unwrap();
            let content = meta.is_file().then(|| fs::read(p).unwrap());
            out.push((p.to_path_buf(), content));
        });
        out.sort();
        out
    }

    /// Panics unless only the canary is outside staging, unchanged.
    pub fn assert_untouched(&self) {
        assert_eq!(
            self.outside(),
            vec![(self.dir.path().join("canary"), Some(CANARY.to_vec()))]
        );
    }
}

pub fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        f(&path);
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            walk(&path, f);
        }
    }
}

/// Every entry under `root`, relative, with file content (`None` for a
/// directory) and whether a file is executable.
pub fn tree(root: &Path) -> Vec<(String, Option<Vec<u8>>, bool)> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    walk(root, &mut |p| {
        let meta = fs::symlink_metadata(p).unwrap();
        assert!(meta.is_dir() || meta.is_file(), "{}", p.display());
        let rel = p.strip_prefix(root).unwrap().to_str().unwrap().to_owned();
        let exec = meta.permissions().mode() & 0o111 != 0;
        let content = meta.is_file().then(|| fs::read(p).unwrap());
        out.push((rel, content, meta.is_file() && exec));
    });
    out.sort();
    out
}
