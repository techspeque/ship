//! The daemon's unix socket and loopback TCP listener (ADR-0003 rules 1, 2).

use std::fmt;
use std::fs;
use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
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
/// dropped, but only while the path still names this very socket, so a
/// successor daemon's socket is never deleted.
///
/// On Linux a device and inode number alone are not enough. Once the
/// listener closes, the socket's inode is freed, and the next socket bound
/// at the path can get the same number. So the value also holds `pin`, an
/// `O_PATH` fd to the socket's inode. It accepts no connections and does not
/// keep the socket listening. While it is open the inode cannot be freed, so
/// no other file on the device can have its number.
#[derive(Debug)]
pub struct SocketFile {
    path: PathBuf,
    dev: u64,
    ino: u64,
    #[cfg(target_os = "linux")]
    pin: fs::File,
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
        let at_path = match fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if !self.is_ours(&at_path)? {
            return Ok(());
        }
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Whether `at_path` (the path's current metadata) is this socket.
    fn is_ours(&self, at_path: &fs::Metadata) -> io::Result<bool> {
        if (at_path.dev(), at_path.ino()) != (self.dev, self.ino) {
            return Ok(false);
        }
        #[cfg(target_os = "linux")]
        {
            let pinned = self.pin.metadata()?;
            if (at_path.dev(), at_path.ino()) != (pinned.dev(), pinned.ino()) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = self.remove_inner();
    }
}

/// Binds the daemon's unix socket at `path` with mode [`SOCKET_MODE`].
///
/// The socket is bound owner-only (on Linux, from the moment it exists),
/// widened to [`SOCKET_MODE`], and only then starts listening, so it never
/// accepts a connection under any other mode. The process umask is never
/// changed. If `path` exists:
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

    let bound = bind_owner_only(path).map_err(io_err("bind", path))?;
    // `bound` is alive here, so the inode at `path` is still this socket's.
    let file = match identify(path) {
        Ok(file) => file,
        Err(e) => {
            let _ = fs::remove_file(path);
            return Err(e);
        }
    };
    // From here on, dropping `file` on error removes the socket.
    fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE))
        .map_err(io_err("set mode of", path))?;
    let listener = bound.listen().map_err(io_err("listen on", path))?;
    Ok((listener, file))
}

/// Records the identity of the socket just bound at `path`. Call it only
/// while the bound socket is open.
///
/// On Linux the path is opened `O_PATH | O_NOFOLLOW` (std adds
/// `O_CLOEXEC`), and that pin is checked to be a socket and to be the inode
/// the path names. Elsewhere the identity is the path's device and inode.
fn identify(path: &Path) -> Result<SocketFile, BindError> {
    let meta = fs::symlink_metadata(path).map_err(io_err("stat", path))?;
    #[cfg(target_os = "linux")]
    let pin = {
        use std::os::unix::fs::OpenOptionsExt;
        let pin = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
            .open(path)
            .map_err(io_err("pin", path))?;
        let pinned = pin.metadata().map_err(io_err("stat pin of", path))?;
        if !pinned.file_type().is_socket()
            || (pinned.dev(), pinned.ino()) != (meta.dev(), meta.ino())
        {
            return Err(BindError::Io {
                step: "pin",
                path: path.to_path_buf(),
                source: io::Error::other("the path changed while binding"),
            });
        }
        pin
    };
    Ok(SocketFile {
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
        #[cfg(target_os = "linux")]
        pin,
        removed: false,
    })
}

/// A unix socket bound at a path but not yet listening: a connection
/// attempt is refused until [`Bound::listen`].
struct Bound(OwnedFd);

impl Bound {
    fn listen(self) -> io::Result<UnixListener> {
        // SAFETY: the fd is a valid, owned, bound socket.
        if unsafe { libc::listen(self.0.as_raw_fd(), libc::SOMAXCONN) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(UnixListener::from(self.0))
    }
}

/// Binds a socket at `path` without listening and without touching the
/// process umask, which is process-wide and would race with every other
/// thread creating files.
///
/// On Linux the socket's mode is set to `0600` before `bind`; the kernel
/// creates the file with that mode (further narrowed by the umask), so it
/// is owner-only from the moment it exists. On every OS the caller widens
/// the mode to [`SOCKET_MODE`] before [`Bound::listen`], so no connection
/// can be accepted while the mode is anything else.
fn bind_owner_only(path: &Path) -> io::Result<Bound> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.contains(&0) || bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path too long or contains NUL",
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        addr.sun_len = len as u8;
    }

    // SAFETY: plain syscall; the result is checked before use.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh fd this function owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: valid fd; FD_CLOEXEC has no memory-safety preconditions.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    // SAFETY: valid fd. Linux applies an unbound socket's inode mode to
    // the file `bind` creates.
    if unsafe { libc::fchmod(fd.as_raw_fd(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `addr` is a valid sockaddr_un and `len` does not exceed it.
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
            len as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Bound(fd))
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

    /// Forces the collision the old `(dev, ino)` check fell for instead of
    /// waiting for the filesystem to reuse a number: the predecessor's
    /// recorded identity is set to the successor's. Only the pin tells the
    /// two sockets apart.
    #[cfg(target_os = "linux")]
    #[test]
    fn remove_leaves_a_successor_with_the_same_inode_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock_path(&dir);
        let (first, mut by_remove) = bind_unix(&path).unwrap();
        drop(first);
        let (_second, second_file) = bind_unix(&path).unwrap();
        let successor = fs::symlink_metadata(&path).unwrap();
        by_remove.dev = successor.dev();
        by_remove.ino = successor.ino();

        by_remove.remove().unwrap();
        assert_eq!(fs::symlink_metadata(&path).unwrap().ino(), successor.ino());
        UnixStream::connect(&path).unwrap();

        // The same through Drop, with a predecessor of the live successor.
        let (third, mut by_drop) = bind_unix(&dir.path().join("other.sock")).unwrap();
        drop(third);
        by_drop.path = path.clone();
        by_drop.dev = successor.dev();
        by_drop.ino = successor.ino();
        drop(by_drop);
        assert_eq!(fs::symlink_metadata(&path).unwrap().ino(), successor.ino());
        UnixStream::connect(&path).unwrap();

        drop(second_file);
        assert!(!path.exists());
    }

    /// The pin holds the inode, not the socket: once the listener closes,
    /// the socket refuses connections and a successor sees it as stale.
    #[cfg(target_os = "linux")]
    #[test]
    fn pin_does_not_keep_the_socket_listening() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock_path(&dir);
        let (listener, file) = bind_unix(&path).unwrap();
        drop(listener);
        let err = UnixStream::connect(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(check_existing(&path).unwrap(), Existing::Stale);
        drop(file);
        assert!(!path.exists());
    }

    /// Binding never changes the process umask, so directories and files
    /// other threads create meanwhile keep their normal modes (f-003).
    #[test]
    fn concurrent_file_creation_is_unaffected() {
        let base = tempfile::tempdir().unwrap();
        let reference = base.path().join("reference");
        fs::create_dir(&reference).unwrap();
        let dir_mode = mode(&reference);
        fs::write(reference.join("f"), b"").unwrap();
        let file_mode = mode(&reference.join("f"));
        assert_ne!(dir_mode & 0o100, 0, "test needs owner search on dirs");

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let binders: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let dir = tempfile::tempdir().unwrap();
                        let path = sock_path(&dir);
                        let (_l, _f) = bind_unix(&path).unwrap();
                        assert_eq!(mode(&path), SOCKET_MODE);
                    }
                })
            })
            .collect();

        for i in 0..2000 {
            let dir = base.path().join(format!("d{i}"));
            fs::create_dir(&dir).unwrap();
            assert_eq!(mode(&dir), dir_mode, "directory {i}");
            let file = dir.join("f");
            fs::write(&file, b"").unwrap();
            assert_eq!(mode(&file), file_mode, "file {i}");
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for b in binders {
            b.join().unwrap();
        }
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
