//! Connecting to the daemon's unix socket, with errors that say what to do.

use std::ffi::CStr;
use std::fmt;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// Group named when the socket's own group cannot be read.
pub const FALLBACK_GROUP: &str = "ship";

/// Why the daemon's socket could not be reached.
#[derive(Debug)]
pub enum ConnectError {
    /// Nothing listens at the path (missing, or a stale socket).
    NoDaemon { socket: PathBuf },
    /// The socket exists but this user may not open it.
    PermissionDenied { socket: PathBuf, group: String },
    /// Any other failure.
    Io { socket: PathBuf, source: io::Error },
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::NoDaemon { socket } => write!(
                f,
                "no Ship daemon is listening on {}; start it with \
                 `sudo systemctl start ship`, or run `ship daemon` on that host",
                socket.display()
            ),
            ConnectError::PermissionDenied { socket, group } => write!(
                f,
                "permission denied on {}: the daemon's socket belongs to group \
                 `{group}` and this user is not in it. Add the user with \
                 `sudo usermod -aG {group} $USER`, then log in again",
                socket.display()
            ),
            ConnectError::Io { socket, source } => {
                write!(f, "cannot connect to {}: {source}", socket.display())
            }
        }
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConnectError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Connects to the daemon's socket at `socket`.
pub fn connect_unix(socket: &Path) -> Result<UnixStream, ConnectError> {
    UnixStream::connect(socket).map_err(|e| classify(socket, e))
}

/// Connects to the daemon's socket at `socket` for use on a Tokio runtime.
pub async fn connect_unix_async(socket: &Path) -> Result<tokio::net::UnixStream, ConnectError> {
    tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| classify(socket, e))
}

/// Maps a connect error on `socket` to a [`ConnectError`].
pub fn classify(socket: &Path, e: io::Error) -> ConnectError {
    let socket = socket.to_path_buf();
    match e.raw_os_error() {
        Some(libc::ENOENT) | Some(libc::ECONNREFUSED) => ConnectError::NoDaemon { socket },
        Some(libc::EACCES) | Some(libc::EPERM) => {
            let group = owning_group(&socket).unwrap_or_else(|| FALLBACK_GROUP.to_owned());
            ConnectError::PermissionDenied { socket, group }
        }
        _ => ConnectError::Io { socket, source: e },
    }
}

/// The name of the group owning `socket`, or its directory when the socket
/// itself cannot be inspected.
fn owning_group(socket: &Path) -> Option<String> {
    let meta = match std::fs::metadata(socket) {
        Ok(meta) => meta,
        Err(_) => std::fs::metadata(socket.parent()?).ok()?,
    };
    let gid = meta.gid();
    group_name(gid)
}

/// The name of group `gid` from the system group database.
pub fn group_name(gid: u32) -> Option<String> {
    let mut buf = vec![0u8; 1024];
    loop {
        let mut group: libc::group = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::group = std::ptr::null_mut();
        // SAFETY: every pointer refers to live, correctly sized storage owned
        // by this frame; `result` is only read after a successful call.
        let rc = unsafe {
            libc::getgrgid_r(
                gid as libc::gid_t,
                &mut group,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() || group.gr_name.is_null() {
            return None;
        }
        // SAFETY: on success `gr_name` points to a NUL-terminated string
        // inside `buf`, which outlives this read.
        let name = unsafe { CStr::from_ptr(group.gr_name) };
        return name.to_str().ok().map(str::to_owned);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn missing_socket_is_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ship.sock");
        let e = connect_unix(&path).unwrap_err();
        assert!(matches!(e, ConnectError::NoDaemon { .. }), "{e:?}");
        let msg = e.to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        assert!(msg.contains("systemctl start ship"), "{msg}");
        assert!(msg.contains("ship daemon"), "{msg}");
    }

    #[test]
    fn stale_socket_is_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ship.sock");
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let e = connect_unix(&path).unwrap_err();
        assert!(matches!(e, ConnectError::NoDaemon { .. }), "{e:?}");
    }

    #[test]
    fn inaccessible_socket_names_its_group() {
        if is_root() {
            eprintln!("skipped: root bypasses socket permissions");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ship.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let expected = group_name(std::fs::metadata(&path).unwrap().gid()).unwrap();

        let e = connect_unix(&path).unwrap_err();
        match &e {
            ConnectError::PermissionDenied { group, .. } => assert_eq!(group, &expected),
            other => panic!("{other:?}"),
        }
        let msg = e.to_string();
        assert!(msg.contains(&format!("`{expected}`")), "{msg}");
        assert!(msg.contains("usermod -aG"), "{msg}");
    }

    #[test]
    fn unknown_group_falls_back() {
        assert_eq!(group_name(u32::MAX - 7), None);
        let e = classify(
            Path::new("/nonexistent/ship-test/ship.sock"),
            io::Error::from_raw_os_error(libc::EACCES),
        );
        match e {
            ConnectError::PermissionDenied { group, .. } => assert_eq!(group, FALLBACK_GROUP),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn other_errors_are_io() {
        let e = classify(
            Path::new("/x.sock"),
            io::Error::from_raw_os_error(libc::ENOTSOCK),
        );
        assert!(matches!(e, ConnectError::Io { .. }), "{e:?}");
    }
}
