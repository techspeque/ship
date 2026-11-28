//! Ship daemon: HTTP API, operation queue, reconciler and UI serving.
//!
//! Top-level crate: may depend on `ship-core` and the leaf crates
//! (OVERVIEW §2.3).
//!
//! [`start`] binds the unix socket and the 127.0.0.1 listener (ADR-0003),
//! opens the store, marks operations a previous daemon left unfinished as
//! `interrupted` ([`queue::recover`]), removes the staging directories of
//! uploads it left half-written ([`upload::clean_staging`]), registers
//! every stored secret with the redactor ([`secrets::register_all`]),
//! starts the operation [`queue`] and
//! serves the router in [`http`] (uploads, [`deploy`]ments, [`push`]es,
//! [`settings`] and [`config`] values and secrets among its routes), with
//! the embedded UI ([`ui`]) as its
//! fallback; it also starts the interval [`observe`]r. [`status`] and
//! [`logs`] serve `ship status`, `ship deployments`, `ship projects` and
//! `ship logs`. [`Running::shutdown`] stops it; [`run`] does both around
//! SIGTERM/SIGINT.
//! `ship daemon` is supported on Linux only (ADR-0002 rule 2); that check
//! belongs to the `ship` binary, so this library builds and tests on any
//! unix.

pub mod compose_import;
pub mod config;
pub mod deploy;
pub mod host;
pub mod http;
pub mod log;
pub mod logs;
pub mod observe;
pub mod push;
pub mod queue;
pub mod secrets;
pub mod settings;
pub mod status;
pub mod ui;
pub mod upload;

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use ship_docker::Runner;
use ship_net::{BindError, SocketFile, TailnetAddress, TailscaleCli};
use ship_store::{Store, StoreError};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use deploy::{DeploySettings, Deployer};
pub use log::Redactor;
pub use queue::{Limits, Queue};
pub use ship_source::UnpackLimits;

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
    /// How the daemon runs `docker`; the CLI on `PATH` by default.
    pub docker: Runner,
    /// Queue length, build slots, build timeout, disk watermark and log
    /// size (ADR-0011 rule 10).
    pub limits: Limits,
    /// Size, entry-count and expansion limits of an uploaded snapshot
    /// (ADR-0005 rule 5).
    pub upload_limits: UnpackLimits,
    /// Settle period and timeouts of a deployment's steps.
    pub deploy: DeploySettings,
    /// Where `publish.address auto` finds the host's tailnet address; the
    /// host's `tailscale` CLI by default.
    pub tailnet: Arc<dyn TailnetAddress>,
    /// Time between two observations of every environment by the interval
    /// observer ([`observe::spawn_observer`]).
    pub observe_interval: Duration,
}

impl Config {
    /// The default paths, port and limits.
    pub fn new(redactor: Redactor) -> Self {
        Config {
            state_dir: PathBuf::from(DEFAULT_STATE_DIR),
            socket_path: PathBuf::from(DEFAULT_SOCKET_PATH),
            port: DEFAULT_PORT,
            redactor,
            docker: Runner::docker(),
            limits: Limits::default(),
            upload_limits: UnpackLimits::default(),
            deploy: DeploySettings::default(),
            tailnet: Arc::new(TailscaleCli::default()),
            observe_interval: observe::DEFAULT_INTERVAL,
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
    queue: Queue,
    stop: watch::Sender<bool>,
    /// Fails every open connection's I/O once the drain deadline passes.
    kill: watch::Sender<bool>,
    servers: Vec<JoinHandle<io::Result<()>>>,
    observer: JoinHandle<()>,
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

    // Before anything is served: what a previous daemon left in flight is
    // ended, never resumed (ADR-0011 rule 8).
    let interrupted = match queue::recover(&store).await {
        Ok(n) => n,
        Err(e) => {
            store.close().await;
            return Err(DaemonError::Store(e));
        }
    };
    if interrupted > 0 {
        tracing::warn!(interrupted, "operations left unfinished were interrupted");
    }
    // Before anything runs or is served: every stored secret's value is
    // redacted from the first log line on (ADR-0009 rule 7).
    if let Err(e) = secrets::register_all(
        &store,
        &config.state_dir,
        &secrets::SecretStore::new(&config.state_dir),
        &config.redactor,
    )
    .await
    {
        store.close().await;
        return Err(DaemonError::Store(e));
    }
    // Uploads a dead daemon left half-written: their operations were just
    // interrupted, and nothing of them may remain (ADR-0005 rule 7).
    let uploads = match upload::clean_staging(&config.state_dir)
        .and_then(|_| upload::Uploads::new(&config.state_dir, config.upload_limits))
    {
        Ok(uploads) => uploads,
        Err(e) => {
            store.close().await;
            return Err(DaemonError::StateDir(config.state_dir.clone(), e));
        }
    };
    let queue = match Queue::new(
        store.clone(),
        config.redactor.clone(),
        config.limits,
        &config.state_dir,
    ) {
        Ok(queue) => queue,
        Err(e) => {
            store.close().await;
            return Err(DaemonError::StateDir(config.state_dir.clone(), e));
        }
    };

    let deployer = Deployer::new(
        store.clone(),
        config.docker.clone(),
        &config.state_dir,
        config.deploy,
        config.limits.build_timeout,
        Arc::clone(&config.tailnet),
    );
    let router = http::router_with(
        store.clone(),
        http::HostProbe {
            docker: config.docker.clone(),
            state_dir: config.state_dir.clone(),
        },
        queue.clone(),
        uploads,
        Arc::new(deployer),
    );
    let observer = observe::spawn_observer(
        store.clone(),
        config.docker.clone(),
        config.observe_interval,
        queue.subscribe_closing(),
    );
    let (stop, stopped) = watch::channel(false);
    let (kill, killed) = watch::channel(false);
    let servers = vec![
        tokio::spawn(serve(
            Killable::new(unix, killed.clone()),
            router.clone(),
            stopped.clone(),
        )),
        tokio::spawn(serve(Killable::new(tcp, killed), router, stopped)),
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
        queue,
        stop,
        kill,
        servers,
        observer,
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

    /// The daemon's operation queue.
    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    /// Stops the operation queue (running jobs are aborted and left for
    /// the next start to mark `interrupted`; event streams end) and the
    /// interval observer, stops
    /// accepting, drains open connections on both listeners for up to
    /// 10 s in total, closes whatever is still open and waits for those
    /// connections to end, closes the database and removes the socket file.
    pub async fn shutdown(self) -> Result<(), DaemonError> {
        let Running {
            socket,
            store,
            queue,
            stop,
            kill,
            servers,
            observer,
            ..
        } = self;
        queue.shutdown().await;
        // Before the store closes: a round in progress is abandoned.
        observer.abort();
        let _ = observer.await;
        let _ = stop.send(true);
        // One deadline for every listener, not one per listener.
        let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
        let mut first_error = None;
        for mut server in servers {
            let joined = match tokio::time::timeout_at(deadline, &mut server).await {
                Ok(joined) => joined,
                Err(_) => {
                    if !*kill.borrow() {
                        tracing::warn!("connections still open after drain timeout; closing them");
                        let _ = kill.send(true);
                    }
                    // The server returns once every connection task has
                    // ended; killed connections end at their next I/O.
                    server.await
                }
            };
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    first_error.get_or_insert(DaemonError::Serve(e));
                }
                Err(join) => {
                    first_error.get_or_insert(DaemonError::Serve(io::Error::other(join)));
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

/// A listener whose accepted connections all fail their I/O once `kill`
/// is set, so their tasks end and graceful shutdown can join them.
struct Killable<L> {
    inner: L,
    kill: watch::Receiver<bool>,
}

impl<L> Killable<L> {
    fn new(inner: L, kill: watch::Receiver<bool>) -> Self {
        Killable { inner, kill }
    }
}

impl<L: axum::serve::Listener> axum::serve::Listener for Killable<L> {
    type Io = KillableIo<L::Io>;
    type Addr = L::Addr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (io, addr) = self.inner.accept().await;
        let mut kill = self.kill.clone();
        let killed = Box::pin(async move {
            // A dropped sender also ends the connection.
            let _ = kill.wait_for(|k| *k).await;
        });
        (
            KillableIo {
                io,
                killed,
                dead: false,
            },
            addr,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// One accepted connection; see [`Killable`].
struct KillableIo<T> {
    io: T,
    killed: Pin<Box<dyn Future<Output = ()> + Send>>,
    dead: bool,
}

impl<T> KillableIo<T> {
    /// Registers the task for wake-up on kill; errors once killed.
    fn check(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if !self.dead && self.killed.as_mut().poll(cx).is_ready() {
            self.dead = true;
        }
        if self.dead {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "daemon shutting down",
            ))
        } else {
            Ok(())
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for KillableIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.check(cx)?;
        Pin::new(&mut this.io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for KillableIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.check(cx)?;
        Pin::new(&mut this.io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.check(cx)?;
        Pin::new(&mut this.io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
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
