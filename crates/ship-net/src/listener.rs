//! The daemon's unix socket and loopback TCP listener (ADR-0003 rules 1, 2).

use std::fmt;
use std::fs;
use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Mode of the daemon's unix socket: owner and group read/write, nothing
/// for others. Socket access is the whole authorisation for the local and
/// ssh transports.
pub const SOCKET_MODE: u32 = 0o660;

/// Why the unix socket could not be bound.
#[derive(Debug)]
pub enum BindError {
    /// A socket at the path accepted a connection: another daemon is live.
    InUse(PathBuf),
    /// Something other than a socket is at the path. It is left untouched.
    NotASocket(PathBuf),
    /// An I/O step failed; `step` says which.
    Io {
        step: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BindError::InUse(path) => write!(
                f,
                "{} is in use by a running daemon; stop it first",
                path.display()
            ),
            BindError::NotASocket(path) => write!(
                f,
                "{} exists and is not a socket; refusing to replace it",
                path.display()
            ),
            BindError::Io { step, path, source } => {
                write!(f, "{step} {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for BindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BindError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn io_err<'a>(step: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> BindError + 'a {
    move |source| BindError::Io {
        step,
        path: path.to_path_buf(),
        source,
    }
}

/// The socket file a [`bind_unix`] call created.
///
/// Removes the file when [`SocketFile::remove`] is called or the value is
/// dropped, but only while the path still names this very socket (same
/// device and inode), so a successor daemon's socket is never deleted.
#[derive(Debug)]
pub struct SocketFile {
    path: PathBuf,
    dev: u64,
    ino: u64,
    removed: bool,
}

impl SocketFile {
    /// The socket's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the socket file if it is still this socket.
    pub fn remove(mut self) -> io::Result<()> {
        self.remove_inner()
    }

    fn remove_inner(&mut self) -> io::Result<()> {
        if self.removed {
            return Ok(());
        }
        self.removed = true;
        match fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.dev() == self.dev && meta.ino() == self.ino => {
                fs::remove_file(&self.path)
            }
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = self.remove_inner();
    }
}

/// Binds the daemon's unix socket at `path` with mode [`SOCKET_MODE`].
///
/// The socket is created under a umask of `0177`, so it is owner-only from
/// the moment it exists, then widened to [`SOCKET_MODE`]; it never exists
/// with a mode wider than that. If `path` exists:
///
/// - a socket that accepts a connection → [`BindError::InUse`];
/// - a socket that refuses connections (a crashed daemon's) → replaced;
/// - anything else → [`BindError::NotASocket`], left untouched.
///
/// The parent directory of `path` must exist. The listener is blocking;
/// the caller sets non-blocking mode for its runtime.
pub fn bind_unix(path: &Path) -> Result<(UnixListener, SocketFile), BindError> {
    if check_existing(path)? == Existing::Stale {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("remove stale socket", path)(e)),
        }
    }

    let listener = bind_owner_only(path).map_err(io_err("bind", path))?;
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) => {
            let _ = fs::remove_file(path);
            return Err(io_err("stat", path)(e));
        }
    };
    let file = SocketFile {
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
        removed: false,
    };
    // From here on, dropping `file` on error removes the socket.
    fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE))
        .map_err(io_err("set mode of", path))?;
    Ok((listener, file))
}

/// Binds with the process umask set to `0177` for the duration of the call.
///
/// The umask is process-wide; it is restored immediately, and only ever
/// narrowed, so a file another thread creates meanwhile is at worst
/// created owner-only.
fn bind_owner_only(path: &Path) -> io::Result<UnixListener> {
    // SAFETY: umask has no memory-safety preconditions.
    let previous = unsafe { libc::umask(0o177) };
    let result = UnixListener::bind(path);
    // SAFETY: as above.
    unsafe { libc::umask(previous) };
    result
}

#[derive(Debug, PartialEq, Eq)]
enum Existing {
    Absent,
    Stale,
}

/// Refuses a live socket or a non-socket at `path`; reports absent or stale.
fn check_existing(path: &Path) -> Result<Existing, BindError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Existing::Absent),
        Err(e) => return Err(io_err("stat", path)(e)),
    };
    if !meta.file_type().is_socket() {
        return Err(BindError::NotASocket(path.to_path_buf()));
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(BindError::InUse(path.to_path_buf())),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => Ok(Existing::Stale),
        Err(e) => Err(io_err("probe existing socket", path)(e)),
    }
}

/// Binds a TCP listener on `127.0.0.1:port` (`0` picks a free port).
///
/// The address is fixed; there is no way to request another interface.
/// The listener is blocking; the caller sets non-blocking mode.
pub fn bind_loopback(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn sock_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("ship.sock")
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn socket_has_mode_0660_and_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock_path(&dir);
        let (listener, file) = bind_unix(&path).unwrap();
        assert_eq!(file.path(), path);
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
        assert_eq!(mode(&path), SOCKET_MODE);

        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"x").unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1];
        server.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"x");

        // The kernel's name for the socket is the path itself.
        assert_eq!(
            listener.local_addr().unwrap().as_pathname(),
            Some(path.as_path())
        );
    }

    #[test]
    fn stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock_path(&dir);
        // A crashed daemon: the file stays, nothing listens.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let old_ino = fs::symlink_metadata(&path).unwrap().ino();

        let (_listener, _file) = bind_unix(&path).unwrap();
        let meta = fs::symlink_metadata(&path).unwrap();
        assert_ne!(meta.ino(), old_ino);
        assert_eq!(mode(&path), SOCKET_MODE);
        UnixStream::connect(&path).unwrap();
    }

    #[test]
    fn live_socket_is_refused_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock_path(&dir);
        let _live = UnixListener::bind(&path).unwrap();
        let ino = fs::symlink_metadata(&path).unwrap().ino();

        let err = bind_unix(&path).unwrap_err();
        assert!(
            matches!(err, BindError::InUse(ref p) if p == &path),
            "{err}"
        );
        assert_eq!(fs::symlink_metadata(&path).unwrap().ino(), ino);
        UnixStream::connect(&path).unwrap();
    }

    #[test]
    fn regular_file_is_refused_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock_path(&dir);
        fs::write(&path, b"not a socket").unwrap();

        let err = bind_unix(&path).unwrap_err();
        assert!(matches!(err, BindError::NotASocket(_)), "{err}");
        assert_eq!(fs::read(&path).unwrap(), b"not a socket");
    }

    #[test]
    fn missing_parent_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent").join("ship.sock");
        assert!(matches!(bind_unix(&path), Err(BindError::Io { .. })));
        assert!(!dir.path().join("absent").exists());
    }

    #[test]
    fn remove_deletes_own_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock_path(&dir);
        let (listener, file) = bind_unix(&path).unwrap();
        drop(listener);
        file.remove().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn drop_deletes_own_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock_path(&dir);
        let (_listener, file) = bind_unix(&path).unwrap();
        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn remove_leaves_a_successor_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock_path(&dir);
        let (first, file) = bind_unix(&path).unwrap();
        drop(first);
        // A successor replaced the (now stale) socket.
        let (_second, second_file) = bind_unix(&path).unwrap();
        let successor = fs::symlink_metadata(&path).unwrap().ino();

        file.remove().unwrap();
        assert_eq!(fs::symlink_metadata(&path).unwrap().ino(), successor);
        drop(second_file);
        assert!(!path.exists());
    }

    #[test]
    fn loopback_binds_127_0_0_1_only() {
        let listener = bind_loopback(0).unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(addr.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(addr.port(), 0);
    }

    #[test]
    fn loopback_port_in_use_fails() {
        let first = bind_loopback(0).unwrap();
        let port = first.local_addr().unwrap().port();
        assert!(bind_loopback(port).is_err());
    }
}
