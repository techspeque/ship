//! Ship daemon: HTTP API, operation queue, reconciler and UI serving.
//!
//! Top-level crate: may depend on `ship-core` and the leaf crates
//! (OVERVIEW §2.3).
//!
//! Phase 0: [`start`] binds the unix socket and the 127.0.0.1 listener
//! (ADR-0003), opens the store and serves the router in [`http`];
//! [`Running::shutdown`] stops it; [`run`] does both around SIGTERM/SIGINT.
//! `ship daemon` is supported on Linux only (ADR-0002 rule 2); that check
//! belongs to the `ship` binary, so this library builds and tests on any
//! unix.

pub mod http;
pub mod log;

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ship_net::{BindError, SocketFile};
use ship_store::{Store, StoreError};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use log::Redactor;

/// Default state directory.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/ship";
/// Default unix socket path.
pub const DEFAULT_SOCKET_PATH: &str = "/run/ship/ship.sock";
/// Default port of the 127.0.0.1 listener.
pub const DEFAULT_PORT: u16 = 7180;

/// How long shutdown waits for open connections before dropping them.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// What `ship daemon` was asked to do.
#[derive(Debug, Clone)]
pub struct Config {
    /// Holds the database; created at 0700 if missing.
    pub state_dir: PathBuf,
    /// The unix socket; its parent directory must exist.
    pub socket_path: PathBuf,
    /// Port on 127.0.0.1; `0` picks a free one.
    pub port: u16,
    /// Secret values the logger must never print.
    pub redactor: Redactor,
}

impl Config {
    /// The default paths and port.
    pub fn new(redactor: Redactor) -> Self {
        Config {
            state_dir: PathBuf::from(DEFAULT_STATE_DIR),
            socket_path: PathBuf::from(DEFAULT_SOCKET_PATH),
            port: DEFAULT_PORT,
            redactor,
        }
    }
}

/// Why the daemon could not start or stop cleanly.
#[derive(Debug)]
pub enum DaemonError {
    StateDir(PathBuf, io::Error),
    Socket(BindError),
    Loopback(u16, io::Error),
    Store(StoreError),
    Signal(io::Error),
    Serve(io::Error),
    Runtime(io::Error),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonError::StateDir(path, e) => {
                write!(f, "state directory {}: {e}", path.display())
            }
            DaemonError::Socket(e) => write!(f, "unix socket: {e}"),
            DaemonError::Loopback(port, e) => write!(f, "listen on 127.0.0.1:{port}: {e}"),
            DaemonError::Store(e) => write!(f, "database: {e}"),
            DaemonError::Signal(e) => write!(f, "signal handler: {e}"),
            DaemonError::Serve(e) => write!(f, "serving: {e}"),
            DaemonError::Runtime(e) => write!(f, "runtime: {e}"),
        }
    }
}

impl std::error::Error for DaemonError {}

/// A started daemon.
pub struct Running {
    socket: SocketFile,
    tcp_addr: SocketAddr,
    store: Store,
    stop: watch::Sender<bool>,
    servers: Vec<JoinHandle<io::Result<()>>>,
}

impl fmt::Debug for Running {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Running")
            .field("socket", &self.socket.path())
            .field("tcp_addr", &self.tcp_addr)
            .finish_non_exhaustive()
    }
}

/// Binds both listeners, opens the store and starts serving.
///
/// Fails if either listener cannot be bound exactly as specified; anything
/// already bound is released and the socket file removed.
pub async fn start(config: &Config) -> Result<Running, DaemonError> {
    ensure_state_dir(&config.state_dir)?;

    // The socket first: it refuses to start beside a live daemon before
    // anything touches the database.
    let (unix, socket) = ship_net::bind_unix(&config.socket_path).map_err(DaemonError::Socket)?;
    let tcp =
        ship_net::bind_loopback(config.port).map_err(|e| DaemonError::Loopback(config.port, e))?;
    let tcp_addr = tcp
        .local_addr()
        .map_err(|e| DaemonError::Loopback(config.port, e))?;

    let unix = into_tokio_unix(unix).map_err(DaemonError::Serve)?;
    let tcp = into_tokio_tcp(tcp).map_err(DaemonError::Serve)?;

    let store = Store::open(&config.state_dir)
        .await
        .map_err(DaemonError::Store)?;

    let router = http::router(store.clone());
    let (stop, stopped) = watch::channel(false);
    let servers = vec![
        tokio::spawn(serve(unix, router.clone(), stopped.clone())),
        tokio::spawn(serve(tcp, router, stopped)),
    ];

    tracing::info!(
        socket = %socket.path().display(),
        tcp = %tcp_addr,
        schema_version = store.open_report().to_version,
        "daemon listening"
    );
    Ok(Running {
        socket,
        tcp_addr,
        store,
        stop,
        servers,
    })
}

impl Running {
    /// The unix socket's path.
    pub fn socket_path(&self) -> &Path {
        self.socket.path()
    }

    /// The bound 127.0.0.1 address.
    pub fn tcp_addr(&self) -> SocketAddr {
        self.tcp_addr
    }

    /// The daemon's store.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Stops accepting, drains open connections for up to 10 s, closes the
    /// database and removes the socket file.
    pub async fn shutdown(self) -> Result<(), DaemonError> {
        let Running {
            socket,
            store,
            stop,
            servers,
            ..
        } = self;
        let _ = stop.send(true);
        let mut first_error = None;
        for mut server in servers {
            match tokio::time::timeout(DRAIN_TIMEOUT, &mut server).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => {
                    first_error.get_or_insert(DaemonError::Serve(e));
                }
                Ok(Err(join)) => {
                    first_error.get_or_insert(DaemonError::Serve(io::Error::other(join)));
                }
                Err(_) => {
                    tracing::warn!("connections still open after drain timeout; closing them");
                    server.abort();
                }
            }
        }
        store.close().await;
        if let Err(e) = socket.remove() {
            tracing::warn!(error = %e, "could not remove the socket file");
        }
        tracing::info!("daemon stopped");
        first_error.map_or(Ok(()), Err)
    }
}

/// Starts the daemon and runs it until SIGTERM or SIGINT, then shuts down.
///
/// Signal handlers are installed before anything is bound, so a signal
/// that arrives during start-up is not lost.
pub async fn run(config: Config) -> Result<(), DaemonError> {
    let mut term = signal(SignalKind::terminate()).map_err(DaemonError::Signal)?;
    let mut int = signal(SignalKind::interrupt()).map_err(DaemonError::Signal)?;
    let running = start(&config).await?;
    let name = tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
    };
    tracing::info!(signal = name, "shutting down");
    running.shutdown().await
}

/// [`run`] on a fresh multi-threaded Tokio runtime.
pub fn run_blocking(config: Config) -> Result<(), DaemonError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(DaemonError::Runtime)?
        .block_on(run(config))
}

fn ensure_state_dir(path: &Path) -> Result<(), DaemonError> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(DaemonError::StateDir(
            path.to_path_buf(),
            io::Error::new(io::ErrorKind::NotADirectory, "not a directory"),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|e| DaemonError::StateDir(path.to_path_buf(), e)),
        Err(e) => Err(DaemonError::StateDir(path.to_path_buf(), e)),
    }
}

fn into_tokio_unix(
    listener: std::os::unix::net::UnixListener,
) -> io::Result<tokio::net::UnixListener> {
    listener.set_nonblocking(true)?;
    tokio::net::UnixListener::from_std(listener)
}

fn into_tokio_tcp(listener: std::net::TcpListener) -> io::Result<tokio::net::TcpListener> {
    listener.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(listener)
}

async fn serve<L>(
    listener: L,
    router: axum::Router,
    mut stopped: watch::Receiver<bool>,
) -> io::Result<()>
where
    L: axum::serve::Listener,
    L::Addr: fmt::Debug,
{
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = stopped.wait_for(|stop| *stop).await;
        })
        .await
}
