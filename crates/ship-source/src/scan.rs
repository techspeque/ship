//! Selecting a working tree's files (ADR-0005 rules 1, 2, 3, 4 and 6).
//!
//! The walk reads no Git metadata. In each directory, the lines of
//! `.gitignore` and then `.shipignore` form one matcher, so a `.shipignore`
//! line overrides a `.gitignore` line; the deepest directory with a
//! matching line decides, and an ignored directory is not entered.
//! `.dockerignore` is an ordinary file: Docker applies it at build time.
//!
//! Symlinks are never followed. A directory's entries are listed together
//! in byte order of their names, then each subdirectory in that order, so
//! a parent always precedes its children and the same tree always gives
//! the same selection.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Serialize;

use crate::exclude::{self, Exclusion};

/// Ignore files read in every directory, in increasing precedence.
pub const IGNORE_FILES: &[&str] = &[".gitignore", ".shipignore"];

/// Size and count limits on a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most entries (files and directories) a snapshot may hold.
    pub max_files: u64,
    /// Most bytes of file content a snapshot may hold, before compression.
    pub max_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_files: 50_000,
            max_bytes: 512 * 1024 * 1024,
        }
    }
}

/// How to select a tree.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Paths, relative to the root, that the operator named explicitly. A
    /// named path is sent even when a default exclusion or an ignore rule
    /// would drop it; its blocked ancestors are entered only to reach it.
    pub include: Vec<PathBuf>,
    /// Limits on the selection.
    pub limits: Limits,
    /// The home directory to refuse. `None` reads `$HOME`.
    pub home: Option<PathBuf>,
}

/// One entry of a selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the root, `/`-separated, UTF-8.
    pub path: String,
    /// What the entry is.
    pub kind: EntryKind,
}

/// A selected entry's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A directory.
    Dir,
    /// A regular file, with its size at scan time and whether any execute
    /// bit is set.
    File { size: u64, executable: bool },
}

/// A default exclusion that applied, reported by `--dry-run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Excluded {
    pub path: String,
    pub kind: Exclusion,
}

/// Why an entry was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// A symbolic link; never followed (rule 6).
    Symlink,
    /// A FIFO, socket or device.
    Special,
    /// A name that is not valid UTF-8.
    NonUtf8Name,
}

/// An entry that was skipped, reported by `--dry-run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub path: String,
    pub reason: SkipReason,
}

/// The result of a scan: what a snapshot of the tree will hold.
#[derive(Debug, Clone)]
pub struct Selection {
    root: PathBuf,
    entries: Vec<Entry>,
    excluded: Vec<Excluded>,
    skipped: Vec<Skipped>,
    file_count: u64,
    total_bytes: u64,
}

impl Selection {
    /// The canonical root of the scanned tree.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Selected entries: a directory's entries in byte order, then each
    /// subdirectory's; a parent always precedes its children.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Default exclusions that applied.
    pub fn excluded(&self) -> &[Excluded] {
        &self.excluded
    }

    /// Entries skipped because they cannot be sent.
    pub fn skipped(&self) -> &[Skipped] {
        &self.skipped
    }

    /// Number of regular files selected.
    pub fn file_count(&self) -> u64 {
        self.file_count
    }

    /// Total bytes of selected file content.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// The `--dry-run` report (ADR-0005 rule 4).
    pub fn report(&self) -> DryRunReport {
        DryRunReport {
            root: self.root.to_string_lossy().into_owned(),
            file_count: self.file_count,
            directory_count: self.entries.len() as u64 - self.file_count,
            total_bytes: self.total_bytes,
            excluded: self.excluded.clone(),
            skipped: self.skipped.clone(),
        }
    }
}

/// What `ship push --dry-run` shows: the size of the snapshot and every
/// sensitive file left out of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DryRunReport {
    pub root: String,
    pub file_count: u64,
    pub directory_count: u64,
    pub total_bytes: u64,
    pub excluded: Vec<Excluded>,
    pub skipped: Vec<Skipped>,
}

impl fmt::Display for DryRunReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Snapshot of {}", self.root)?;
        writeln!(
            f,
            "  {} files, {} directories, {} bytes",
            self.file_count, self.directory_count, self.total_bytes
        )?;
        if self.excluded.is_empty() {
            writeln!(f, "  Excluded by default: none")?;
        } else {
            writeln!(f, "  Excluded by default:")?;
            for e in &self.excluded {
                writeln!(f, "    {}  ({})", e.path, e.kind.as_str())?;
            }
        }
        if !self.skipped.is_empty() {
            writeln!(f, "  Skipped:")?;
            for s in &self.skipped {
                let reason = match s.reason {
                    SkipReason::Symlink => "symlink, not followed",
                    SkipReason::Special => "not a regular file",
                    SkipReason::NonUtf8Name => "name is not UTF-8",
                };
                writeln!(f, "    {}  ({reason})", s.path)?;
            }
        }
        Ok(())
    }
}

/// Why a tree cannot be selected.
#[derive(Debug)]
pub enum ScanError {
    /// The root is a filesystem root.
    FilesystemRoot(PathBuf),
    /// The root is the home directory or contains it.
    HomeDirectory(PathBuf),
    /// The root is not a directory.
    NotADirectory(PathBuf),
    /// More entries than `Limits::max_files`.
    TooManyFiles { limit: u64 },
    /// More content than `Limits::max_bytes`.
    TooLarge { limit: u64 },
    /// An include path is absolute, contains `..`, is empty or not UTF-8.
    InvalidInclude(PathBuf),
    /// An include path names nothing that can be sent.
    IncludeNotFound(String),
    /// Reading the tree failed.
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::FilesystemRoot(p) => {
                write!(f, "refusing to snapshot a filesystem root: {}", p.display())
            }
            ScanError::HomeDirectory(p) => write!(
                f,
                "refusing to snapshot the home directory or a directory containing it: {}",
                p.display()
            ),
            ScanError::NotADirectory(p) => write!(f, "not a directory: {}", p.display()),
            ScanError::TooManyFiles { limit } => {
                write!(f, "the tree has more than {limit} files and directories")
            }
            ScanError::TooLarge { limit } => write!(f, "the tree holds more than {limit} bytes"),
            ScanError::InvalidInclude(p) => write!(
                f,
                "an included path must be relative, inside the tree and UTF-8: {}",
                p.display()
            ),
            ScanError::IncludeNotFound(p) => write!(
                f,
                "included path is not a file or directory in the tree: {p}"
            ),
            ScanError::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ScanError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> ScanError + '_ {
    move |source| ScanError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Normalise an include path to its `/`-joined relative form.
fn normalise_include(path: &Path) -> Result<String, ScanError> {
    let invalid = || ScanError::InvalidInclude(path.to_path_buf());
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str().ok_or_else(invalid)?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(invalid());
            }
        }
    }
    if parts.is_empty() {
        return Err(invalid());
    }
    Ok(parts.join("/"))
}

/// Refuse a filesystem root, the home directory and any directory that
/// contains the home directory (ADR-0005 rule 4).
fn refuse_root(root: &Path, home: Option<&Path>) -> Result<(), ScanError> {
    if root.parent().is_none() {
        return Err(ScanError::FilesystemRoot(root.to_path_buf()));
    }
    if let Some(home) = home {
        let home = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        if home.starts_with(root) {
            return Err(ScanError::HomeDirectory(root.to_path_buf()));
        }
    }
    Ok(())
}

/// The ignore matcher of one directory, from its ignore files; `None` when
/// it has none.
fn dir_matcher(dir: &Path) -> Result<Option<Gitignore>, ScanError> {
    let mut builder = GitignoreBuilder::new(dir);
    let mut any = false;
    for name in IGNORE_FILES {
        let path = dir.join(name);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(&path)(e)),
        };
        // An ignore file that is a symlink is not followed either.
        if !meta.is_file() {
            continue;
        }
        let bytes = fs::read(&path).map_err(io_err(&path))?;
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            // Git skips a pattern it cannot parse; so does Ship.
            let _ = builder.add_line(Some(path.clone()), line);
        }
        any = true;
    }
    if !any {
        return Ok(None);
    }
    builder.build().map(Some).map_err(|e| ScanError::Io {
        path: dir.to_path_buf(),
        source: io::Error::other(e),
    })
}

/// Whether the matchers, deepest first, ignore `path`.
fn is_ignored(matchers: &[Arc<Gitignore>], path: &Path, is_dir: bool) -> bool {
    for m in matchers.iter().rev() {
        let r = m.matched(path, is_dir);
        if r.is_ignore() {
            return true;
        }
        if r.is_whitelist() {
            return false;
        }
    }
    false
}

/// A directory waiting to be read.
struct Pending {
    abs: PathBuf,
    rel: String,
    matchers: Vec<Arc<Gitignore>>,
    /// Inside a blocked directory, entered only to reach a named path.
    restricted: bool,
}

/// Select the files of the tree at `root` that a snapshot sends.
pub fn scan(root: &Path, options: &Options) -> Result<Selection, ScanError> {
    let root = fs::canonicalize(root).map_err(io_err(root))?;
    let home = match &options.home {
        Some(h) => Some(h.clone()),
        None => std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from),
    };
    refuse_root(&root, home.as_deref())?;
    let meta = fs::metadata(&root).map_err(io_err(&root))?;
    if !meta.is_dir() {
        return Err(ScanError::NotADirectory(root));
    }

    let includes: BTreeSet<String> = options
        .include
        .iter()
        .map(|p| normalise_include(p))
        .collect::<Result<_, _>>()?;
    let mut found: BTreeSet<String> = BTreeSet::new();
    let limits = options.limits;

    let mut entries = Vec::new();
    let mut excluded = Vec::new();
    let mut skipped = Vec::new();
    let mut file_count = 0u64;
    let mut total_bytes = 0u64;

    // Directories depth-first in byte order: a stack popped from the end,
    // so each directory's subdirectories are pushed in reverse.
    let mut stack = vec![Pending {
        abs: root.clone(),
        rel: String::new(),
        matchers: Vec::new(),
        restricted: false,
    }];

    while let Some(dir) = stack.pop() {
        let mut matchers = dir.matchers;
        if let Some(m) = dir_matcher(&dir.abs)? {
            matchers.push(Arc::new(m));
        }

        let mut names: Vec<OsString> = fs::read_dir(&dir.abs)
            .map_err(io_err(&dir.abs))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .map_err(io_err(&dir.abs))?;
        names.sort();

        let mut subdirs = Vec::new();
        for name in names {
            let abs = dir.abs.join(&name);
            let Some(name) = name.to_str() else {
                let shown = name.to_string_lossy();
                skipped.push(Skipped {
                    path: join(&dir.rel, &shown),
                    reason: SkipReason::NonUtf8Name,
                });
                continue;
            };
            let rel = join(&dir.rel, name);
            let meta = fs::symlink_metadata(&abs).map_err(io_err(&abs))?;
            let ft = meta.file_type();
            let is_dir = ft.is_dir();

            let named = includes.contains(&rel);
            let leads = is_dir && {
                let prefix = format!("{rel}/");
                includes.iter().any(|i| i.starts_with(&prefix))
            };
            if dir.restricted && !named && !leads {
                continue;
            }

            let mut blocked = false;
            if !named {
                if let Some(kind) = exclude::classify(name, is_dir) {
                    excluded.push(Excluded {
                        path: rel.clone(),
                        kind,
                    });
                    blocked = true;
                } else if is_ignored(&matchers, &abs, is_dir) {
                    blocked = true;
                }
                if blocked && !leads {
                    continue;
                }
            }

            if ft.is_symlink() {
                skipped.push(Skipped {
                    path: rel,
                    reason: SkipReason::Symlink,
                });
                continue;
            }
            let kind = if is_dir {
                EntryKind::Dir
            } else if ft.is_file() {
                EntryKind::File {
                    size: meta.len(),
                    executable: is_executable(&meta),
                }
            } else {
                skipped.push(Skipped {
                    path: rel,
                    reason: SkipReason::Special,
                });
                continue;
            };

            if entries.len() as u64 >= limits.max_files {
                return Err(ScanError::TooManyFiles {
                    limit: limits.max_files,
                });
            }
            if let EntryKind::File { size, .. } = kind {
                total_bytes = total_bytes
                    .checked_add(size)
                    .filter(|t| *t <= limits.max_bytes)
                    .ok_or(ScanError::TooLarge {
                        limit: limits.max_bytes,
                    })?;
                file_count += 1;
            }
            if named {
                found.insert(rel.clone());
            }
            entries.push(Entry {
                path: rel.clone(),
                kind,
            });
            if is_dir {
                subdirs.push(Pending {
                    abs,
                    rel,
                    matchers: matchers.clone(),
                    restricted: (dir.restricted || blocked) && !named,
                });
            }
        }
        stack.extend(subdirs.into_iter().rev());
    }

    if let Some(missing) = includes.difference(&found).next() {
        return Err(ScanError::IncludeNotFound(missing.clone()));
    }

    Ok(Selection {
        root,
        entries,
        excluded,
        skipped,
        file_count,
        total_bytes,
    })
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_paths_normalise() {
        assert_eq!(normalise_include(Path::new(".env")).unwrap(), ".env");
        assert_eq!(normalise_include(Path::new("./a/b/")).unwrap(), "a/b");
        assert_eq!(normalise_include(Path::new("a/./b")).unwrap(), "a/b");
    }

    #[test]
    fn include_paths_outside_the_tree_are_invalid() {
        for p in ["/etc/passwd", "../x", "a/../../x", "a/..", "", "."] {
            assert!(
                matches!(
                    normalise_include(Path::new(p)),
                    Err(ScanError::InvalidInclude(_))
                ),
                "{p:?}"
            );
        }
    }

    #[test]
    fn roots_refused() {
        assert!(matches!(
            refuse_root(Path::new("/"), None),
            Err(ScanError::FilesystemRoot(_))
        ));
        let home = Path::new("/nonexistent-ship-home/user");
        assert!(matches!(
            refuse_root(home, Some(home)),
            Err(ScanError::HomeDirectory(_))
        ));
        assert!(matches!(
            refuse_root(Path::new("/nonexistent-ship-home"), Some(home)),
            Err(ScanError::HomeDirectory(_))
        ));
        assert!(refuse_root(Path::new("/nonexistent-ship-home/user/project"), Some(home)).is_ok());
        assert!(refuse_root(Path::new("/nonexistent-ship-home/username"), Some(home)).is_ok());
    }

    #[test]
    fn join_paths() {
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("a", "b"), "a/b");
    }
}
