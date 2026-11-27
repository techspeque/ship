//! One HTTP exchange with a daemon over a [`Target`]'s transport.
//!
//! `local` opens the unix socket; `ssh://` spawns `ssh` with structured
//! argv (no shell on this side) running `ship dial-stdio`, and speaks HTTP
//! over its pipes. The whole exchange, including ssh's connection, is
//! bounded by a timeout; on timeout the `ssh` child is killed.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::{Frame, SizeHint};
use hyper::header::{CONNECTION, HOST};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use ship_core::{API_VERSION, API_VERSION_HEADER, SHIP_VERSION_HEADER};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::connect::{ConnectError, connect_unix_async};
use crate::target::{DEFAULT_SOCKET, SshTarget, Target};

/// Largest response body a client accepts.
pub const MAX_BODY: usize = 16 * 1024 * 1024;
/// How much of `ssh`'s stderr is kept for messages.
const MAX_STDERR: usize = 8 * 1024;

/// A daemon's answer.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    /// The daemon's `Ship-Api-Version` header, if it sent one.
    pub api_version: Option<String>,
    /// The daemon's `Ship-Version` header, if it sent one.
    pub daemon_version: Option<String>,
    pub body: Bytes,
}

/// Why an exchange failed.
#[derive(Debug)]
pub enum TransportError {
    /// The local socket could not be opened.
    Connect(ConnectError),
    /// No complete answer within the timeout.
    TimedOut { after: Duration },
    /// `ssh` itself failed (exit 255): host unreachable, auth refused, …
    SshUnreachable { host: String, stderr: String },
    /// `ssh` could not be started.
    SshSpawn(io::Error),
    /// `ship` is not on the host's PATH for the login user (exit 127).
    ShipNotInstalled { host: String },
    /// The remote `ship dial-stdio` failed; `stderr` carries its message.
    Remote {
        host: String,
        status: Option<i32>,
        stderr: String,
    },
    /// The HTTP exchange failed on an open connection.
    Http(String),
    /// The response body exceeded [`MAX_BODY`].
    BodyTooLarge,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Connect(e) => e.fmt(f),
            TransportError::TimedOut { after } => write!(
                f,
                "timed out after {}s waiting for the daemon; check that the host \
                 is up and reachable",
                after.as_secs_f64()
            ),
            TransportError::SshUnreachable { host, stderr } => {
                write!(f, "cannot reach {host} over ssh")?;
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            TransportError::SshSpawn(e) => write!(
                f,
                "cannot run `ssh`: {e}; install an OpenSSH client and make sure \
                 it is on PATH"
            ),
            TransportError::ShipNotInstalled { host } => write!(
                f,
                "`ship` was not found on {host}: install Ship there and make sure \
                 it is on the PATH of the ssh login user"
            ),
            TransportError::Remote {
                host,
                status,
                stderr,
            } => {
                write!(f, "on {host}: ")?;
                match (stderr.is_empty(), status) {
                    (false, _) => f.write_str(stderr),
                    (true, Some(code)) => write!(f, "`ship dial-stdio` exited with {code}"),
                    (true, None) => f.write_str("the connection closed before the daemon answered"),
                }
            }
            TransportError::Http(e) => write!(f, "talking to the daemon: {e}"),
            TransportError::BodyTooLarge => write!(
                f,
                "the daemon's answer is larger than {} MiB",
                MAX_BODY / (1024 * 1024)
            ),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<ConnectError> for TransportError {
    fn from(e: ConnectError) -> Self {
        TransportError::Connect(e)
    }
}

/// A request body for [`post`]: bytes held whole, or chunks fed through a
/// bounded channel, so a large body streams in bounded memory. A chunk
/// that is an error aborts the request.
#[derive(Debug)]
pub struct RequestBody {
    inner: BodyInner,
}

#[derive(Debug)]
enum BodyInner {
    Bytes(Option<Bytes>),
    Channel(mpsc::Receiver<io::Result<Bytes>>),
}

impl RequestBody {
    /// A body of `bytes`.
    pub fn bytes(bytes: impl Into<Bytes>) -> Self {
        RequestBody {
            inner: BodyInner::Bytes(Some(bytes.into())),
        }
    }

    /// A body fed through the returned sender, holding at most `capacity`
    /// chunks; it ends when the sender is dropped.
    pub fn channel(capacity: usize) -> (mpsc::Sender<io::Result<Bytes>>, RequestBody) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (
            tx,
            RequestBody {
                inner: BodyInner::Channel(rx),
            },
        )
    }
}

impl hyper::body::Body for RequestBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        match &mut self.get_mut().inner {
            BodyInner::Bytes(bytes) => Poll::Ready(bytes.take().map(|b| Ok(Frame::data(b)))),
            BodyInner::Channel(rx) => rx
                .poll_recv(cx)
                .map(|chunk| chunk.map(|r| r.map(Frame::data))),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.inner {
            BodyInner::Bytes(Some(b)) => SizeHint::with_exact(b.len() as u64),
            BodyInner::Bytes(None) => SizeHint::with_exact(0),
            BodyInner::Channel(_) => SizeHint::default(),
        }
    }
}

/// `GET path` from the daemon behind `target`, sending this build's API
/// version, within `timeout`.
pub async fn get(
    target: &Target,
    path: &str,
    timeout: Duration,
) -> Result<Response, TransportError> {
    let request = request(Method::GET, path, &[], Empty::<Bytes>::new())?;
    send(target, request, timeout).await
}

/// `POST path` with `headers` and `body` to the daemon behind `target`,
/// sending this build's API version; the whole exchange, body included,
/// within `timeout`.
pub async fn post(
    target: &Target,
    path: &str,
    headers: &[(&str, &str)],
    body: RequestBody,
    timeout: Duration,
) -> Result<Response, TransportError> {
    let request = request(Method::POST, path, headers, body)?;
    send(target, request, timeout).await
}

/// A request to the daemon: `Host: localhost`, one exchange per
/// connection, this build's API version, then `headers`.
fn request<B>(
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: B,
) -> Result<Request<B>, TransportError> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(HOST, "localhost")
        .header(CONNECTION, "close")
        .header(API_VERSION_HEADER, API_VERSION);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(body)
        .map_err(|e| TransportError::Http(e.to_string()))
}

async fn send<B>(
    target: &Target,
    request: Request<B>,
    timeout: Duration,
) -> Result<Response, TransportError>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    match target {
        Target::Local { socket } => {
            let exchange = async {
                let stream = connect_unix_async(socket).await?;
                exchange(stream, request).await
            };
            tokio::time::timeout(timeout, exchange)
                .await
                .unwrap_or(Err(TransportError::TimedOut { after: timeout }))
        }
        Target::Ssh(ssh) => over_ssh(ssh, request, timeout).await,
    }
}

async fn over_ssh<B>(
    ssh: &SshTarget,
    request: Request<B>,
    timeout: Duration,
) -> Result<Response, TransportError>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut child = Command::new("ssh")
        .args(ssh.ssh_args(timeout))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(TransportError::SshSpawn)?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(TransportError::SshSpawn(io::Error::other(
            "ssh started without its pipes",
        )));
    };
    let mut stderr = tokio::spawn(read_bounded(stderr));

    // One deadline bounds the exchange and, when it fails, collecting ssh's
    // exit status and stderr to explain why.
    let deadline = Instant::now() + timeout;
    let timed_out = || TransportError::TimedOut { after: timeout };
    let result =
        tokio::time::timeout_at(deadline, exchange(tokio::io::join(stdout, stdin), request)).await;
    match result {
        Ok(Ok(response)) => Ok(response),
        Err(_) => {
            kill_and_reap(&mut child, &stderr).await;
            Err(timed_out())
        }
        Ok(Err(e)) => {
            let Ok(status) = tokio::time::timeout_at(deadline, child.wait()).await else {
                kill_and_reap(&mut child, &stderr).await;
                return Err(timed_out());
            };
            let stderr = match tokio::time::timeout_at(deadline, &mut stderr).await {
                Ok(read) => read.unwrap_or_default(),
                Err(_) => {
                    stderr.abort();
                    String::new()
                }
            };
            Err(ssh_failure(ssh, status.ok(), stderr, e))
        }
    }
}

/// Kills `ssh`, waits for it so no process is left behind, and stops
/// reading its stderr.
async fn kill_and_reap(child: &mut Child, stderr: &JoinHandle<String>) {
    let _ = child.kill().await;
    stderr.abort();
}

/// Explains a failed exchange over ssh from ssh's exit status and stderr.
fn ssh_failure(
    ssh: &SshTarget,
    status: Option<ExitStatus>,
    stderr: String,
    cause: TransportError,
) -> TransportError {
    let host = ssh.display_host();
    let code = status.and_then(|s| s.code());
    match code {
        Some(255) => TransportError::SshUnreachable { host, stderr },
        Some(127) => TransportError::ShipNotInstalled { host },
        Some(0) | None if stderr.is_empty() => cause,
        _ => TransportError::Remote {
            host,
            status: code,
            stderr,
        },
    }
}

/// Reads up to [`MAX_STDERR`] bytes, then drains the rest, so `ssh` never
/// blocks on a full pipe. Returns the text trimmed.
pub(crate) async fn read_bounded(mut r: impl AsyncRead + Unpin) -> String {
    let mut kept = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_STDERR.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).trim().to_owned()
}

/// One request over an established byte stream.
async fn exchange<S, B>(stream: S, request: Request<B>) -> Result<Response, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let http = |e: hyper::Error| TransportError::Http(e.to_string());
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(http)?;
    let driver = tokio::spawn(conn);

    let response = sender.send_request(request).await.map_err(http)?;
    let (parts, body) = response.into_parts();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let api_version = header(API_VERSION_HEADER);
    let daemon_version = header(SHIP_VERSION_HEADER);
    let body = Limited::new(body, MAX_BODY)
        .collect()
        .await
        .map_err(|e| {
            if e.is::<http_body_util::LengthLimitError>() {
                TransportError::BodyTooLarge
            } else {
                TransportError::Http(e.to_string())
            }
        })?
        .to_bytes();
    drop(sender);
    driver.abort();
    Ok(Response {
        status: parts.status.as_u16(),
        api_version,
        daemon_version,
        body,
    })
}

/// The socket a target reaches, for messages: the local path, or the path
/// on the ssh host.
pub fn socket_of(target: &Target) -> PathBuf {
    target
        .socket()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn target() -> SshTarget {
        SshTarget {
            user: Some("deploy".into()),
            host: "lab".into(),
            port: None,
            socket: None,
        }
    }

    fn exited(code: i32) -> Option<ExitStatus> {
        Some(ExitStatus::from_raw(code << 8))
    }

    fn cause() -> TransportError {
        TransportError::Http("connection closed".into())
    }

    #[test]
    fn ssh_exit_codes_are_explained() {
        let e = ssh_failure(&target(), exited(255), "No route to host".into(), cause());
        assert!(matches!(e, TransportError::SshUnreachable { .. }), "{e:?}");
        assert_eq!(
            e.to_string(),
            "cannot reach deploy@lab over ssh: No route to host"
        );

        let e = ssh_failure(&target(), exited(127), String::new(), cause());
        assert!(
            matches!(e, TransportError::ShipNotInstalled { .. }),
            "{e:?}"
        );

        let remote = "ship dial-stdio: permission denied on /run/ship/ship.sock: …";
        let e = ssh_failure(&target(), exited(1), remote.into(), cause());
        assert_eq!(e.to_string(), format!("on deploy@lab: {remote}"));

        let e = ssh_failure(&target(), exited(0), String::new(), cause());
        assert!(matches!(e, TransportError::Http(_)), "{e:?}");
    }

    #[tokio::test]
    async fn stderr_is_bounded() {
        let big = vec![b'x'; MAX_STDERR * 3];
        let got = read_bounded(&big[..]).await;
        assert_eq!(got.len(), MAX_STDERR);
    }

    #[tokio::test]
    async fn local_missing_socket_is_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let t = Target::Local {
            socket: dir.path().join("none.sock"),
        };
        let e = get(&t, "/api/v1/projects", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            matches!(e, TransportError::Connect(ConnectError::NoDaemon { .. })),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn exchange_sends_version_and_reads_headers() {
        use tokio::io::AsyncWriteExt;
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let daemon = tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let mut got = Vec::new();
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = server.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            server
                .write_all(
                    b"HTTP/1.1 200 OK\r\nship-api-version: v1\r\nship-version: 9.9.9\r\n\
                      content-length: 2\r\n\r\n{}",
                )
                .await
                .unwrap();
            String::from_utf8(got).unwrap()
        });
        let get = request(Method::GET, "/api/v1/projects", &[], Empty::<Bytes>::new()).unwrap();
        let r = exchange(client, get).await.unwrap();
        let request = daemon.await.unwrap().to_ascii_lowercase();
        assert!(
            request.starts_with("get /api/v1/projects http/1.1\r\n"),
            "{request}"
        );
        assert!(request.contains("host: localhost\r\n"), "{request}");
        assert!(request.contains("ship-api-version: v1\r\n"), "{request}");
        assert_eq!(r.status, 200);
        assert_eq!(r.api_version.as_deref(), Some("v1"));
        assert_eq!(r.daemon_version.as_deref(), Some("9.9.9"));
        assert_eq!(&r.body[..], b"{}");
    }

    #[tokio::test]
    async fn post_streams_a_channel_body_with_headers() {
        use tokio::io::AsyncWriteExt;
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let daemon = tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let mut got = Vec::new();
            // A chunked body ends with an empty chunk.
            while !got.windows(5).any(|w| w == b"0\r\n\r\n") {
                let n = server.read(&mut buf).await.unwrap();
                assert!(n > 0, "request ended early");
                got.extend_from_slice(&buf[..n]);
            }
            server
                .write_all(b"HTTP/1.1 201 Created\r\ncontent-length: 2\r\n\r\n{}")
                .await
                .unwrap();
            got
        });
        let (tx, body) = RequestBody::channel(2);
        let feeder = tokio::spawn(async move {
            for chunk in [&b"first-"[..], b"second"] {
                tx.send(Ok(Bytes::from_static(chunk))).await.unwrap();
            }
        });
        let post = request(
            Method::POST,
            "/api/v1/projects/a/uploads",
            &[("idempotency-key", "k1"), ("ship-snapshot-sha256", "ab")],
            body,
        )
        .unwrap();
        let r = exchange(client, post).await.unwrap();
        feeder.await.unwrap();
        let raw = daemon.await.unwrap();
        let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
        assert!(
            text.starts_with("post /api/v1/projects/a/uploads http/1.1\r\n"),
            "{text}"
        );
        assert!(text.contains("idempotency-key: k1\r\n"), "{text}");
        assert!(text.contains("ship-snapshot-sha256: ab\r\n"), "{text}");
        assert!(text.contains("transfer-encoding: chunked\r\n"), "{text}");
        assert!(text.contains("first-"), "{text}");
        assert!(text.contains("second"), "{text}");
        assert_eq!(r.status, 201);
    }

    #[test]
    fn a_bytes_body_has_an_exact_size() {
        use hyper::body::Body;
        let body = RequestBody::bytes(&b"hello"[..]);
        assert_eq!(body.size_hint().exact(), Some(5));
        let (_tx, body) = RequestBody::channel(1);
        assert_eq!(body.size_hint().exact(), None);
    }
}
