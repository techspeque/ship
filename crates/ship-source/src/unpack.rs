//! Unpacking an uploaded snapshot, daemon side (ADR-0005 rules 5, 7, 9).
//!
//! The compressed stream is read once, through a reader that hashes and
//! counts it, a zstd decoder with a bounded window, a reader that counts
//! the expanded bytes, and a tar header parser written here, so every
//! header field is checked before it is used. Every entry is validated as
//! it arrives and written into a staging directory that [`unpack`] creates
//! itself; nothing is written anywhere else. Only regular files and
//! directories are accepted. On any failure the staging directory is
//! removed before `unpack` returns.
//!
//! Memory stays bounded: one 512-byte header block, one copy buffer, one
//! GNU long name of at most [`MAX_PATH_BYTES`] and a 32-byte digest per
//! entry for the duplicate and collision checks.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// Size of a tar block.
const BLOCK: usize = 512;
/// Longest path accepted, in bytes (Linux `PATH_MAX`).
pub const MAX_PATH_BYTES: usize = 4096;
/// Longest path component accepted, in bytes (Linux `NAME_MAX`).
pub const MAX_COMPONENT_BYTES: usize = 255;
/// Largest zstd window the decoder accepts, as a power of two (16 MiB).
/// The packer's level never needs more; a hostile frame asking for more is
/// refused instead of being given the memory.
pub const ZSTD_WINDOW_LOG_MAX: u32 = 24;
/// Most zero bytes accepted after the end-of-archive block.
pub const MAX_TRAILER_BYTES: u64 = 1024 * 1024;
/// Read buffer of the compressed stream and copy buffer of file content.
const BUFFER: usize = 64 * 1024;

/// Limits enforced while a snapshot streams in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnpackLimits {
    /// Most bytes of the compressed stream.
    pub max_compressed_bytes: u64,
    /// Most bytes of file content, summed over the snapshot.
    pub max_bytes: u64,
    /// Most entries (files and directories).
    pub max_entries: u64,
    /// Expanded bytes may exceed the compressed bytes read so far by at
    /// most this factor ...
    pub max_ratio: u64,
    /// ... once past this many expanded bytes; see
    /// [`UnpackLimits::expansion_allowance`].
    pub ratio_grace_bytes: u64,
}

impl Default for UnpackLimits {
    /// The pack side's defaults (`Limits`), a ratio of 100 and 32 MiB of
    /// grace.
    fn default() -> Self {
        UnpackLimits {
            max_compressed_bytes: 512 * 1024 * 1024,
            max_bytes: 512 * 1024 * 1024,
            max_entries: 50_000,
            max_ratio: 100,
            ratio_grace_bytes: 32 * 1024 * 1024,
        }
    }
}

impl UnpackLimits {
    /// Most expanded bytes allowed once `compressed` compressed bytes have
    /// been read: `max(ratio_grace_bytes, max_ratio × compressed)`.
    pub fn expansion_allowance(&self, compressed: u64) -> u64 {
        compressed
            .saturating_mul(self.max_ratio)
            .max(self.ratio_grace_bytes)
    }
}

/// A snapshot unpacked into its staging directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpacked {
    /// SHA-256 of the compressed stream, 64 lowercase hex digits.
    pub sha256: String,
    pub compressed_bytes: u64,
    /// Bytes the decoder produced, headers and padding included.
    pub expanded_bytes: u64,
    /// Files and directories.
    pub entries: u64,
    pub files: u64,
    /// Bytes of file content.
    pub bytes: u64,
}

/// Why an entry is refused (ADR-0005 rule 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsafeReason {
    AbsolutePath,
    ParentComponent,
    DotComponent,
    EmptyComponent,
    NotUtf8,
    ControlCharacter,
    ComponentTooLong,
    PathTooLong,
    Symlink,
    Hardlink,
    /// A GNU long link name, which only a link entry uses.
    LinkTarget,
    Device,
    Fifo,
    Pax,
    /// Any other tar entry type, by its type byte.
    UnsupportedType(u8),
    /// The entry's parent directory is not an earlier directory entry.
    MissingParent,
    /// The same path as an earlier entry.
    Duplicate,
    /// The same path as an earlier entry after Unicode normalisation and
    /// case folding.
    Collision,
}

impl fmt::Display for UnsafeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnsafeReason::AbsolutePath => f.write_str("absolute path"),
            UnsafeReason::ParentComponent => f.write_str("`..` component"),
            UnsafeReason::DotComponent => f.write_str("`.` component"),
            UnsafeReason::EmptyComponent => f.write_str("empty path component"),
            UnsafeReason::NotUtf8 => f.write_str("path is not UTF-8"),
            UnsafeReason::ControlCharacter => f.write_str("control character in the path"),
            UnsafeReason::ComponentTooLong => {
                write!(f, "path component longer than {MAX_COMPONENT_BYTES} bytes")
            }
            UnsafeReason::PathTooLong => write!(f, "path longer than {MAX_PATH_BYTES} bytes"),
            UnsafeReason::Symlink => f.write_str("symbolic link"),
            UnsafeReason::Hardlink => f.write_str("hard link"),
            UnsafeReason::LinkTarget => f.write_str("link target"),
            UnsafeReason::Device => f.write_str("device file"),
            UnsafeReason::Fifo => f.write_str("FIFO"),
            UnsafeReason::Pax => f.write_str("PAX extended header"),
            UnsafeReason::UnsupportedType(t) => write!(f, "unsupported entry type {t:#04x}"),
            UnsafeReason::MissingParent => f.write_str("parent directory is not an earlier entry"),
            UnsafeReason::Duplicate => f.write_str("duplicate path"),
            UnsafeReason::Collision => f.write_str(
                "path collides with an earlier entry after Unicode or case normalisation",
            ),
        }
    }
}

/// Which limit a snapshot exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    CompressedBytes,
    Bytes,
    Entries,
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Limit::CompressedBytes => "compressed bytes",
            Limit::Bytes => "bytes of file content",
            Limit::Entries => "entries",
        })
    }
}

/// Why an unpack failed. The staging directory is gone in every case but
/// [`UnpackError::Staging`] for the staging directory itself.
#[derive(Debug)]
pub enum UnpackError {
    /// An entry ADR-0005 rule 5 refuses. `path` is the entry's path as
    /// sent, lossily decoded.
    Unsafe { path: String, reason: UnsafeReason },
    /// Not a well-formed zstd + tar stream.
    Malformed(String),
    /// Over one of the [`UnpackLimits`].
    TooLarge { limit: Limit, max: u64 },
    /// The expanded stream outgrew the expansion allowance.
    Expansion { expanded: u64, compressed: u64 },
    /// Reading the source failed: a broken transfer.
    Transfer(io::Error),
    /// Writing under the staging directory failed (or creating it: it
    /// must not exist).
    Staging { path: PathBuf, source: io::Error },
}

impl fmt::Display for UnpackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnpackError::Unsafe { path, reason } => write!(f, "refused entry {path:?}: {reason}"),
            UnpackError::Malformed(why) => write!(f, "not a valid snapshot: {why}"),
            UnpackError::TooLarge { limit, max } => {
                write!(f, "the snapshot is over its limit of {max} {limit}")
            }
            UnpackError::Expansion {
                expanded,
                compressed,
            } => write!(
                f,
                "the snapshot expanded to {expanded} bytes from {compressed} compressed bytes, \
                 over the expansion limit"
            ),
            UnpackError::Transfer(e) => write!(f, "the upload broke off: {e}"),
            UnpackError::Staging { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for UnpackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UnpackError::Transfer(e) | UnpackError::Staging { source: e, .. } => Some(e),
            _ => None,
        }
    }
}

/// Unpacks the tar + zstd stream `source` into `staging`, which this
/// function creates (mode 0700; its parent must exist and it must not).
///
/// Returns the stream's hash; the caller compares it with the declared
/// one. On any error `staging` is removed, unless the error is that it
/// could not be created.
pub fn unpack<R: Read>(
    source: R,
    staging: &Path,
    limits: &UnpackLimits,
) -> Result<Unpacked, UnpackError> {
    DirBuilder::new()
        .mode(0o700)
        .create(staging)
        .map_err(|source| UnpackError::Staging {
            path: staging.to_path_buf(),
            source,
        })?;
    let result = unpack_into(source, staging, limits);
    if result.is_err() {
        let _ = fs::remove_dir_all(staging);
    }
    result
}

/// What the readers below have counted, shared between them.
struct Meter {
    compressed: u64,
    expanded: u64,
    hasher: Sha256,
    /// The source's own error, kept so it is reported as a transfer
    /// failure and not as a malformed stream.
    source_error: Option<io::Error>,
    compressed_tripped: bool,
    expansion_tripped: bool,
}

type SharedMeter = Rc<RefCell<Meter>>;

/// The compressed stream: hashed, counted and held to its limit.
struct SourceReader<R> {
    inner: R,
    meter: SharedMeter,
    max: u64,
}

impl<R: Read> Read for SourceReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.inner.read(buf) {
            Ok(n) => {
                let mut m = self.meter.borrow_mut();
                m.compressed += n as u64;
                m.hasher.update(&buf[..n]);
                if m.compressed > self.max {
                    m.compressed_tripped = true;
                    return Err(io::Error::other("compressed limit reached"));
                }
                Ok(n)
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Err(e),
            Err(e) => {
                self.meter.borrow_mut().source_error =
                    Some(io::Error::new(e.kind(), e.to_string()));
                Err(e)
            }
        }
    }
}

/// The expanded stream, held to the expansion allowance.
struct ExpandedReader<R> {
    inner: R,
    meter: SharedMeter,
    limits: UnpackLimits,
}

impl<R: Read> Read for ExpandedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        let mut m = self.meter.borrow_mut();
        m.expanded += n as u64;
        if m.expanded > self.limits.expansion_allowance(m.compressed) {
            m.expansion_tripped = true;
            return Err(io::Error::other("expansion limit reached"));
        }
        Ok(n)
    }
}

/// The error for a failed read of the expanded stream: whatever tripped
/// first, else a malformed stream.
fn read_error(meter: &SharedMeter, limits: &UnpackLimits, e: io::Error, what: &str) -> UnpackError {
    let mut m = meter.borrow_mut();
    if let Some(source) = m.source_error.take() {
        return UnpackError::Transfer(source);
    }
    if m.compressed_tripped {
        return UnpackError::TooLarge {
            limit: Limit::CompressedBytes,
            max: limits.max_compressed_bytes,
        };
    }
    if m.expansion_tripped {
        return UnpackError::Expansion {
            expanded: m.expanded,
            compressed: m.compressed,
        };
    }
    UnpackError::Malformed(format!("{what}: {e}"))
}

fn unpack_into<R: Read>(
    source: R,
    staging: &Path,
    limits: &UnpackLimits,
) -> Result<Unpacked, UnpackError> {
    let meter: SharedMeter = Rc::new(RefCell::new(Meter {
        compressed: 0,
        expanded: 0,
        hasher: Sha256::new(),
        source_error: None,
        compressed_tripped: false,
        expansion_tripped: false,
    }));
    let source = SourceReader {
        inner: source,
        meter: Rc::clone(&meter),
        max: limits.max_compressed_bytes,
    };
    let mut decoder =
        zstd::stream::read::Decoder::with_buffer(BufReader::with_capacity(BUFFER, source))
            .map_err(|e| UnpackError::Malformed(format!("zstd: {e}")))?;
    decoder
        .window_log_max(ZSTD_WINDOW_LOG_MAX)
        .map_err(|e| UnpackError::Malformed(format!("zstd: {e}")))?;
    let mut r = ExpandedReader {
        inner: decoder,
        meter: Rc::clone(&meter),
        limits: *limits,
    };
    let fail = |e: io::Error, what: &str| read_error(&meter, limits, e, what);

    let mut tree = Tree::default();
    let mut block = [0u8; BLOCK];
    let mut buffer = vec![0u8; BUFFER];
    let mut long_name: Option<Vec<u8>> = None;
    let (mut entries, mut files, mut bytes) = (0u64, 0u64, 0u64);

    loop {
        if !read_block(&mut r, &mut block).map_err(|e| fail(e, "reading a header"))? {
            return Err(UnpackError::Malformed(
                "the archive ends without an end-of-archive block".into(),
            ));
        }
        if block.iter().all(|&b| b == 0) {
            break;
        }
        let header = parse_header(&block)?;

        if header.typeflag == b'L' {
            if long_name.is_some() {
                return Err(UnpackError::Malformed("two GNU long names in a row".into()));
            }
            if header.size > (MAX_PATH_BYTES + 1) as u64 {
                return Err(UnpackError::Unsafe {
                    path: lossy(&header.name),
                    reason: UnsafeReason::PathTooLong,
                });
            }
            let size = header.size as usize;
            let mut name = vec![0u8; size];
            read_exact(&mut r, &mut name).map_err(|e| fail(e, "reading a long name"))?;
            skip_padding(&mut r, header.size).map_err(|e| fail(e, "reading a long name"))?;
            while name.last() == Some(&0) {
                name.pop();
            }
            long_name = Some(name);
            continue;
        }

        let raw = long_name.take().unwrap_or(header.name);
        let unsafe_entry = |reason| UnpackError::Unsafe {
            path: lossy(&raw),
            reason,
        };
        let is_dir = match header.typeflag {
            b'0' | 0 => false,
            b'5' => true,
            b'1' => return Err(unsafe_entry(UnsafeReason::Hardlink)),
            b'2' => return Err(unsafe_entry(UnsafeReason::Symlink)),
            b'3' | b'4' => return Err(unsafe_entry(UnsafeReason::Device)),
            b'6' => return Err(unsafe_entry(UnsafeReason::Fifo)),
            b'K' => return Err(unsafe_entry(UnsafeReason::LinkTarget)),
            b'x' | b'g' => return Err(unsafe_entry(UnsafeReason::Pax)),
            t => return Err(unsafe_entry(UnsafeReason::UnsupportedType(t))),
        };
        let path = check_path(&raw, is_dir).map_err(unsafe_entry)?;
        tree.admit(&path, is_dir).map_err(unsafe_entry)?;

        entries += 1;
        if entries > limits.max_entries {
            return Err(UnpackError::TooLarge {
                limit: Limit::Entries,
                max: limits.max_entries,
            });
        }
        let target = staging.join(&path);
        let staging_err = |source: io::Error| UnpackError::Staging {
            path: target.clone(),
            source,
        };

        if is_dir {
            if header.size != 0 {
                return Err(UnpackError::Malformed(format!(
                    "directory {path:?} has a size"
                )));
            }
            DirBuilder::new()
                .mode(0o755)
                .create(&target)
                .map_err(staging_err)?;
            fs::set_permissions(&target, Permissions::from_mode(0o755)).map_err(staging_err)?;
            continue;
        }

        // Checked against the header before a byte is written.
        if header.size > limits.max_bytes.saturating_sub(bytes) {
            return Err(UnpackError::TooLarge {
                limit: Limit::Bytes,
                max: limits.max_bytes,
            });
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&target)
            .map_err(staging_err)?;
        let mut remaining = header.size;
        while remaining > 0 {
            let want = buffer
                .len()
                .min(usize::try_from(remaining).unwrap_or(BUFFER));
            let n = r
                .read(&mut buffer[..want])
                .map_err(|e| fail(e, "reading a file"))?;
            if n == 0 {
                return Err(UnpackError::Malformed(format!(
                    "the archive ends inside {path:?}"
                )));
            }
            file.write_all(&buffer[..n]).map_err(staging_err)?;
            remaining -= n as u64;
        }
        skip_padding(&mut r, header.size).map_err(|e| fail(e, "reading a file"))?;
        let mode = if header.mode & 0o111 != 0 {
            0o755
        } else {
            0o644
        };
        file.set_permissions(Permissions::from_mode(mode))
            .map_err(staging_err)?;
        files += 1;
        bytes += header.size;
    }

    if long_name.is_some() {
        return Err(UnpackError::Malformed(
            "a GNU long name ends the archive".into(),
        ));
    }

    // The rest must be zeros (the second end block and any record
    // padding), and the stream must end cleanly, which also completes
    // the hash.
    let mut trailer = 0u64;
    loop {
        let n = r
            .read(&mut buffer)
            .map_err(|e| fail(e, "reading the end of the archive"))?;
        if n == 0 {
            break;
        }
        if buffer[..n].iter().any(|&b| b != 0) {
            return Err(UnpackError::Malformed(
                "data after the end of the archive".into(),
            ));
        }
        trailer += n as u64;
        if trailer > MAX_TRAILER_BYTES {
            return Err(UnpackError::Malformed(
                "too much padding after the end of the archive".into(),
            ));
        }
    }
    drop(r);

    let mut m = meter.borrow_mut();
    let digest = std::mem::replace(&mut m.hasher, Sha256::new()).finalize();
    let sha256: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(Unpacked {
        sha256,
        compressed_bytes: m.compressed,
        expanded_bytes: m.expanded,
        entries,
        files,
        bytes,
    })
}

fn lossy(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
}

/// Fills `block`; `Ok(false)` at a clean end of stream before its first
/// byte.
fn read_block(r: &mut impl Read, block: &mut [u8; BLOCK]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < BLOCK {
        match r.read(&mut block[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the archive ends inside a header",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn read_exact(r: &mut impl Read, buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the archive ends early",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Reads the padding after `size` bytes of entry data, up to the next block.
fn skip_padding(r: &mut impl Read, size: u64) -> io::Result<()> {
    let pad = (BLOCK - (size % BLOCK as u64) as usize) % BLOCK;
    let mut buf = [0u8; BLOCK];
    read_exact(r, &mut buf[..pad])
}

/// The header fields unpacking uses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Header {
    /// `name`, or `prefix/name` in a POSIX ustar header.
    name: Vec<u8>,
    mode: u32,
    size: u64,
    typeflag: u8,
}

/// Parses and checks one header block (POSIX ustar or GNU).
fn parse_header(b: &[u8; BLOCK]) -> Result<Header, UnpackError> {
    let bad = |what: &str| UnpackError::Malformed(format!("bad {what} field in a tar header"));
    let stored = parse_octal(&b[148..156]).ok_or_else(|| bad("checksum"))?;
    if stored != checksum(b) {
        return Err(UnpackError::Malformed(
            "tar header checksum mismatch".into(),
        ));
    }
    let mode = parse_octal(&b[100..108]).ok_or_else(|| bad("mode"))?;
    let size = parse_octal(&b[124..136]).ok_or_else(|| bad("size"))?;
    let mut name = field(&b[0..100]).to_vec();
    if &b[257..263] == b"ustar\0" {
        let prefix = field(&b[345..500]);
        if !prefix.is_empty() {
            let mut full = prefix.to_vec();
            full.push(b'/');
            full.extend_from_slice(&name);
            name = full;
        }
    }
    Ok(Header {
        name,
        mode: (mode & 0o7777) as u32,
        size,
        typeflag: b[156],
    })
}

/// The sum of a header's bytes with its checksum field read as spaces.
fn checksum(b: &[u8; BLOCK]) -> u64 {
    b.iter()
        .enumerate()
        .map(|(i, &x)| {
            if (148..156).contains(&i) {
                u64::from(b' ')
            } else {
                u64::from(x)
            }
        })
        .sum()
}

/// A NUL-terminated header field's bytes.
fn field(f: &[u8]) -> &[u8] {
    match f.iter().position(|&b| b == 0) {
        Some(end) => &f[..end],
        None => f,
    }
}

/// An octal header number: optional leading spaces, at least one octal
/// digit, then only spaces or NULs. The base-256 form is refused; no
/// accepted size needs it.
fn parse_octal(f: &[u8]) -> Option<u64> {
    let start = f.iter().position(|&b| b != b' ')?;
    let digits = f[start..]
        .iter()
        .take_while(|&&b| (b'0'..=b'7').contains(&b))
        .count();
    if digits == 0 {
        return None;
    }
    if !f[start + digits..].iter().all(|&b| b == b' ' || b == 0) {
        return None;
    }
    f[start..start + digits].iter().try_fold(0u64, |n, &d| {
        n.checked_mul(8)?.checked_add(u64::from(d - b'0'))
    })
}

/// Checks an entry path and returns it without a directory's trailing
/// slash.
fn check_path(raw: &[u8], is_dir: bool) -> Result<String, UnsafeReason> {
    let s = std::str::from_utf8(raw).map_err(|_| UnsafeReason::NotUtf8)?;
    if s.starts_with('/') {
        return Err(UnsafeReason::AbsolutePath);
    }
    let s = if is_dir {
        s.strip_suffix('/').unwrap_or(s)
    } else {
        s
    };
    if s.len() > MAX_PATH_BYTES {
        return Err(UnsafeReason::PathTooLong);
    }
    if s.chars().any(char::is_control) {
        return Err(UnsafeReason::ControlCharacter);
    }
    for component in s.split('/') {
        match component {
            "" => return Err(UnsafeReason::EmptyComponent),
            "." => return Err(UnsafeReason::DotComponent),
            ".." => return Err(UnsafeReason::ParentComponent),
            c if c.len() > MAX_COMPONENT_BYTES => return Err(UnsafeReason::ComponentTooLong),
            _ => {}
        }
    }
    Ok(s.to_owned())
}

/// The form two paths share when they name the same file on a filesystem
/// that normalises Unicode or ignores case: decomposed, lowercased, then
/// composed.
pub fn collision_key(path: &str) -> String {
    let decomposed: String = path.nfd().collect();
    decomposed.to_lowercase().nfc().collect()
}

fn digest(s: &str) -> [u8; 32] {
    let d = Sha256::digest(s.as_bytes());
    let mut out = [0u8; 32];
    for (o, b) in out.iter_mut().zip(d.iter()) {
        *o = *b;
    }
    out
}

/// The paths seen so far, as digests.
#[derive(Default)]
struct Tree {
    exact: HashSet<[u8; 32]>,
    folded: HashSet<[u8; 32]>,
    dirs: HashSet<[u8; 32]>,
}

impl Tree {
    fn admit(&mut self, path: &str, is_dir: bool) -> Result<(), UnsafeReason> {
        if let Some((parent, _)) = path.rsplit_once('/')
            && !self.dirs.contains(&digest(parent))
        {
            return Err(UnsafeReason::MissingParent);
        }
        let exact = digest(path);
        if self.exact.contains(&exact) {
            return Err(UnsafeReason::Duplicate);
        }
        if !self.folded.insert(digest(&collision_key(path))) {
            return Err(UnsafeReason::Collision);
        }
        self.exact.insert(exact);
        if is_dir {
            self.dirs.insert(exact);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octal_fields() {
        assert_eq!(parse_octal(b"0000644\0"), Some(0o644));
        assert_eq!(parse_octal(b"00000000012\0"), Some(10));
        assert_eq!(parse_octal(b"  755 \0\0"), Some(0o755));
        assert_eq!(parse_octal(b"012345\0 "), Some(0o12345));
        for bad in [
            &b"\0\0\0\0\0\0\0\0"[..],
            b"        ",
            b"0008\0",
            b"12a\0",
            b"12 3\0",
            b"\x80\0\0\0\0\0\0\x10",
            b"77777777777777777777777777",
        ] {
            assert_eq!(parse_octal(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn paths() {
        assert_eq!(check_path(b"a/b.txt", false).as_deref(), Ok("a/b.txt"));
        assert_eq!(check_path(b"a/", true).as_deref(), Ok("a"));
        assert_eq!(check_path("é/ü".as_bytes(), false).as_deref(), Ok("é/ü"));
        let cases: &[(&[u8], bool, UnsafeReason)] = &[
            (b"/etc/passwd", false, UnsafeReason::AbsolutePath),
            (b"/", true, UnsafeReason::AbsolutePath),
            (b"../x", false, UnsafeReason::ParentComponent),
            (b"a/../../x", false, UnsafeReason::ParentComponent),
            (b"a/..", true, UnsafeReason::ParentComponent),
            (b"./x", false, UnsafeReason::DotComponent),
            (b"a//b", false, UnsafeReason::EmptyComponent),
            (b"a/", false, UnsafeReason::EmptyComponent),
            (b"", false, UnsafeReason::EmptyComponent),
            (b"", true, UnsafeReason::EmptyComponent),
            (b"a\xff", false, UnsafeReason::NotUtf8),
            (b"a\nb", false, UnsafeReason::ControlCharacter),
            (b"a\x7f", false, UnsafeReason::ControlCharacter),
        ];
        for (raw, dir, reason) in cases {
            assert_eq!(check_path(raw, *dir), Err(*reason), "{raw:?}");
        }
        let long = "x".repeat(MAX_COMPONENT_BYTES + 1);
        assert_eq!(
            check_path(long.as_bytes(), false),
            Err(UnsafeReason::ComponentTooLong)
        );
        let deep = vec!["abc"; MAX_PATH_BYTES / 4 + 1].join("/");
        assert_eq!(
            check_path(deep.as_bytes(), false),
            Err(UnsafeReason::PathTooLong)
        );
    }

    #[test]
    fn collision_keys() {
        assert_eq!(collision_key("README.md"), collision_key("readme.MD"));
        // Precomposed and decomposed é.
        assert_eq!(collision_key("caf\u{e9}"), collision_key("cafe\u{301}"));
        assert_eq!(collision_key("CAF\u{c9}"), collision_key("cafe\u{301}"));
        assert_ne!(collision_key("a"), collision_key("b"));
    }

    #[test]
    fn tree_rules() {
        let mut t = Tree::default();
        assert_eq!(t.admit("a/b", false), Err(UnsafeReason::MissingParent));
        t.admit("a", true).unwrap();
        t.admit("a/b", false).unwrap();
        assert_eq!(t.admit("a/b", false), Err(UnsafeReason::Duplicate));
        assert_eq!(t.admit("a/B", false), Err(UnsafeReason::Collision));
        assert_eq!(t.admit("A", true), Err(UnsafeReason::Collision));
        // A file is not a parent.
        assert_eq!(t.admit("a/b/c", false), Err(UnsafeReason::MissingParent));
        // Only the exact parent counts.
        t.admit("c", true).unwrap();
        assert_eq!(t.admit("C/x", false), Err(UnsafeReason::MissingParent));
    }

    #[test]
    fn expansion_allowance() {
        let l = UnpackLimits {
            max_ratio: 10,
            ratio_grace_bytes: 100,
            ..UnpackLimits::default()
        };
        assert_eq!(l.expansion_allowance(0), 100);
        assert_eq!(l.expansion_allowance(5), 100);
        assert_eq!(l.expansion_allowance(50), 500);
        assert_eq!(l.expansion_allowance(u64::MAX), u64::MAX);
    }

    #[test]
    fn header_checksum_is_checked() {
        let mut b = [0u8; BLOCK];
        b[..1].copy_from_slice(b"a");
        b[100..108].copy_from_slice(b"0000644\0");
        b[124..136].copy_from_slice(b"00000000000\0");
        b[156] = b'0';
        let sum = checksum(&b);
        b[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let h = parse_header(&b).unwrap();
        assert_eq!(
            (h.name.as_slice(), h.mode, h.size, h.typeflag),
            (&b"a"[..], 0o644, 0, b'0')
        );
        b[0] = b'b';
        assert!(matches!(parse_header(&b), Err(UnpackError::Malformed(_))));
    }
}
