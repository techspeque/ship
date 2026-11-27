//! Streaming a selection as tar + zstd with a SHA-256 of the stream
//! (ADR-0005 rules 8 and 9).
//!
//! The archive is deterministic: entries in selection order, GNU headers
//! with mtime 0, uid and gid 0, no owner names, and mode 0755 for
//! directories and executables, 0644 otherwise. The same tree therefore
//! packs to the same bytes and the same hash. File content is copied
//! through fixed buffers, so memory does not grow with the tree.

use std::cell::Cell;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tar::{EntryType, Header};

use crate::scan::{EntryKind, Selection};

/// zstd compression level of a snapshot.
pub const ZSTD_LEVEL: i32 = 3;

/// The outcome of a pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    /// SHA-256 of the compressed stream, 64 lowercase hex digits.
    pub sha256: String,
    /// Bytes of the compressed stream.
    pub compressed_bytes: u64,
}

/// Why a pack failed.
#[derive(Debug)]
pub enum PackError {
    /// A file is no longer the regular file of the given size it was at
    /// scan time.
    Changed(PathBuf),
    /// Reading a file failed.
    Read { path: PathBuf, source: io::Error },
    /// Writing the stream failed.
    Write(io::Error),
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackError::Changed(p) => write!(f, "file changed while packing: {}", p.display()),
            PackError::Read { path, source } => write!(f, "{}: {source}", path.display()),
            PackError::Write(e) => write!(f, "writing the snapshot: {e}"),
        }
    }
}

impl std::error::Error for PackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PackError::Changed(_) => None,
            PackError::Read { source, .. } => Some(source),
            PackError::Write(e) => Some(e),
        }
    }
}

/// A writer that hashes and counts what passes through it.
struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    count: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Reads exactly `remaining` bytes, failing (and setting `changed`) if the
/// source ends early.
struct ExactReader<'a, R> {
    inner: R,
    remaining: u64,
    changed: &'a Cell<bool>,
}

impl<R: Read> Read for ExactReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let max = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..max])?;
        if n == 0 {
            self.changed.set(true);
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file shrank while packing",
            ));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Open a file for reading without following a symlink at its last
/// component and without blocking on a FIFO swapped in since the scan.
#[cfg(unix)]
fn open_nofollow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_nofollow(path: &Path) -> io::Result<File> {
    File::open(path)
}

fn header(kind: EntryType, size: u64, mode: u32) -> Header {
    let mut h = Header::new_gnu();
    h.set_entry_type(kind);
    h.set_size(size);
    h.set_mode(mode);
    h.set_mtime(0);
    h.set_uid(0);
    h.set_gid(0);
    h
}

/// Write `selection` to `out` as a tar + zstd stream and return its hash.
///
/// A file that changed size or type since the scan fails the pack; the
/// caller discards what was written.
pub fn pack<W: Write>(selection: &Selection, out: W) -> Result<Packed, PackError> {
    let hashing = HashingWriter {
        inner: out,
        hasher: Sha256::new(),
        count: 0,
    };
    let encoder =
        zstd::stream::write::Encoder::new(hashing, ZSTD_LEVEL).map_err(PackError::Write)?;
    let mut builder = tar::Builder::new(encoder);

    for entry in selection.entries() {
        match entry.kind {
            EntryKind::Dir => {
                let mut h = header(EntryType::Directory, 0, 0o755);
                builder
                    .append_data(&mut h, &entry.path, io::empty())
                    .map_err(PackError::Write)?;
            }
            EntryKind::File { size, executable } => {
                let abs = selection.root().join(&entry.path);
                let changed = || PackError::Changed(abs.clone());
                let file = match open_nofollow(&abs) {
                    Ok(f) => f,
                    Err(e) if is_symlink_refusal(&e) => return Err(changed()),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(changed()),
                    Err(source) => return Err(PackError::Read { path: abs, source }),
                };
                let meta = file.metadata().map_err(|source| PackError::Read {
                    path: abs.clone(),
                    source,
                })?;
                if !meta.is_file() || meta.len() != size {
                    return Err(changed());
                }
                let flag = Cell::new(false);
                let mut reader = ExactReader {
                    inner: &file,
                    remaining: size,
                    changed: &flag,
                };
                let mode = if executable { 0o755 } else { 0o644 };
                let mut h = header(EntryType::Regular, size, mode);
                if let Err(e) = builder.append_data(&mut h, &entry.path, &mut reader) {
                    return Err(if flag.get() || e.kind() == io::ErrorKind::UnexpectedEof {
                        changed()
                    } else {
                        PackError::Write(e)
                    });
                }
                // A file that grew since the scan was packed short.
                let mut probe = [0u8; 1];
                match (&file).read(&mut probe) {
                    Ok(0) => {}
                    Ok(_) => return Err(changed()),
                    Err(source) => return Err(PackError::Read { path: abs, source }),
                }
            }
        }
    }

    let encoder = builder.into_inner().map_err(PackError::Write)?;
    let mut hashing = encoder.finish().map_err(PackError::Write)?;
    hashing.flush().map_err(PackError::Write)?;
    let digest = hashing.hasher.finalize();
    let mut sha256 = String::with_capacity(64);
    for b in digest.iter() {
        sha256.push_str(&format!("{b:02x}"));
    }
    Ok(Packed {
        sha256,
        compressed_bytes: hashing.count,
    })
}

/// `O_NOFOLLOW` on a symlink fails with `ELOOP` (Linux, macOS) or `EMLINK`
/// (some BSDs).
#[cfg(unix)]
fn is_symlink_refusal(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::EMLINK))
}

#[cfg(not(unix))]
fn is_symlink_refusal(_e: &io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_reader_reads_exactly() {
        let flag = Cell::new(false);
        let mut r = ExactReader {
            inner: &b"hello world"[..],
            remaining: 5,
            changed: &flag,
        };
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello");
        assert!(!flag.get());
    }

    #[test]
    fn exact_reader_flags_a_short_source() {
        let flag = Cell::new(false);
        let mut r = ExactReader {
            inner: &b"abc"[..],
            remaining: 5,
            changed: &flag,
        };
        let mut out = Vec::new();
        let err = r.read_to_end(&mut out).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(flag.get());
    }

    #[test]
    fn hashing_writer_hashes_what_it_passes() {
        let mut sink = Vec::new();
        let mut w = HashingWriter {
            inner: &mut sink,
            hasher: Sha256::new(),
            count: 0,
        };
        w.write_all(b"abc").unwrap();
        let count = w.count;
        let digest = w.hasher.finalize();
        assert_eq!(count, 3);
        assert_eq!(sink, b"abc");
        // SHA-256("abc"), FIPS 180-2 appendix B.1.
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
