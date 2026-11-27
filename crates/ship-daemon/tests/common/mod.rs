//! Shared helpers for the daemon integration tests: a config in a temporary
//! directory and a minimal HTTP/1.1 client over the unix socket or TCP.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ship_daemon::{Config, DeploySettings, Limits, Redactor, UnpackLimits};
use ship_docker::Runner;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A config whose state directory and socket live under `dir`, on a free
/// loopback port, with a `docker` pointed at a socket that does not exist
/// so that no test reaches a developer's real Docker by accident.
pub fn config(dir: &Path) -> Config {
    Config {
        state_dir: dir.join("state"),
        socket_path: dir.join("ship.sock"),
        port: 0,
        redactor: Redactor::new(),
        docker: Runner::docker().env("DOCKER_HOST", MISSING_DOCKER_HOST),
        // No watermark, so a nearly full CI disk does not refuse operations.
        limits: Limits {
            disk_watermark_bytes: 0,
            ..Limits::default()
        },
        upload_limits: UnpackLimits::default(),
        deploy: DeploySettings::default(),
        // No test asks the developer's own tailnet.
        tailnet: std::sync::Arc::new(ship_net::StaticTailnet(None)),
    }
}

/// A Docker endpoint that cannot exist.
pub const MISSING_DOCKER_HOST: &str = "unix:///nonexistent/ship-test/docker.sock";

/// A fake `docker` answering `version` with `$FAKE_ENGINE` and `compose
/// version` with `$FAKE_COMPOSE`.
const FAKE_DOCKER: &str = r#"#!/bin/sh
case "$1" in
  version) echo "$FAKE_ENGINE" ;;
  compose) echo "$FAKE_COMPOSE" ;;
  *) exit 64 ;;
esac
"#;

/// A runner for a fake `docker` reporting `engine` and `compose`. The
/// script is written once per test binary, so no test executes a file
/// another thread still has open for writing.
pub fn fake_docker(engine: &str, compose: &str) -> Runner {
    static PATH: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let (_, path) = PATH.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docker");
        std::fs::write(&path, FAKE_DOCKER).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, path)
    });
    Runner::new(path)
        .env("FAKE_ENGINE", engine)
        .env("FAKE_COMPOSE", compose)
}

/// Where to send a request.
#[derive(Debug, Clone)]
pub enum Via {
    Unix(PathBuf),
    Tcp(SocketAddr),
}

/// A parsed response.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub head: String,
    pub body: String,
}

impl Response {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

/// Sends `method path` with `headers` (each `Name: value`) and reads the
/// whole response; the connection is closed after one exchange.
pub async fn send(via: &Via, method: &str, path: &str, headers: &[&str]) -> Response {
    let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    for h in headers {
        request.push_str(h);
        request.push_str("\r\n");
    }
    if method != "GET" && method != "HEAD" {
        request.push_str("Content-Length: 0\r\n");
    }
    request.push_str("\r\n");

    let raw = match via {
        Via::Unix(path) => {
            let mut s = tokio::net::UnixStream::connect(path).await.unwrap();
            exchange(&mut s, &request).await
        }
        Via::Tcp(addr) => {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            exchange(&mut s, &request).await
        }
    };
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").expect("header terminator");
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");
    Response {
        status,
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

async fn exchange<S>(stream: &mut S, request: &str) -> Vec<u8>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    out
}

/// The body of a `Transfer-Encoding: chunked` response, decoded; any
/// other body as it is.
pub fn body_text(response: &Response) -> String {
    if !response
        .head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return response.body.clone();
    }
    let mut decoder = Dechunk::default();
    decoder.feed(response.body.as_bytes());
    String::from_utf8(decoder.out).unwrap()
}

/// Incremental decoder of an HTTP/1.1 chunked body.
#[derive(Default)]
struct Dechunk {
    raw: Vec<u8>,
    out: Vec<u8>,
    ended: bool,
}

impl Dechunk {
    fn feed(&mut self, bytes: &[u8]) {
        self.raw.extend_from_slice(bytes);
        while !self.ended {
            let Some(eol) = self.raw.windows(2).position(|w| w == b"\r\n") else {
                return;
            };
            let size = std::str::from_utf8(&self.raw[..eol]).unwrap();
            let size = usize::from_str_radix(size.split(';').next().unwrap().trim(), 16).unwrap();
            if size == 0 {
                self.ended = true;
                return;
            }
            let start = eol + 2;
            if self.raw.len() < start + size + 2 {
                return;
            }
            self.out.extend_from_slice(&self.raw[start..start + size]);
            self.raw.drain(..start + size + 2);
        }
    }
}

/// One SSE event: its `id:` and its `data:` as JSON.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub id: u64,
    pub data: serde_json::Value,
}

/// The events of a complete SSE body, keep-alive comments skipped.
pub fn sse_events(body: &str) -> Vec<SseEvent> {
    body.split("\n\n")
        .filter_map(parse_sse_frame)
        .collect::<Vec<_>>()
}

fn parse_sse_frame(frame: &str) -> Option<SseEvent> {
    let mut id = None;
    let mut data = None;
    for line in frame.lines() {
        if let Some(v) = line.strip_prefix("id: ") {
            id = Some(v.parse().unwrap());
        } else if let Some(v) = line.strip_prefix("data: ") {
            data = Some(serde_json::from_str(v).unwrap());
        }
    }
    Some(SseEvent {
        id: id?,
        data: data?,
    })
}

/// A live SSE connection over TCP, read frame by frame.
pub struct SseStream {
    stream: tokio::net::TcpStream,
    decoder: Dechunk,
    pub status: u16,
}

impl SseStream {
    /// Sends `GET path` with `headers` and reads the response head.
    pub async fn open(addr: SocketAddr, path: &str, headers: &[&str]) -> SseStream {
        let mut request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n");
        for h in headers {
            request.push_str(h);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let n = stream.read(&mut byte).await.unwrap();
            assert_eq!(n, 1, "connection closed in the response head");
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        assert!(
            head.to_ascii_lowercase()
                .contains("transfer-encoding: chunked"),
            "{head}"
        );
        SseStream {
            stream,
            decoder: Dechunk::default(),
            status,
        }
    }

    /// The next event, or `None` once the server ends the stream.
    pub async fn next(&mut self) -> Option<SseEvent> {
        loop {
            if let Some(end) = find(&self.decoder.out, b"\n\n") {
                let frame: Vec<u8> = self.decoder.out.drain(..end + 2).collect();
                let frame = String::from_utf8(frame).unwrap();
                match parse_sse_frame(&frame) {
                    Some(event) => return Some(event),
                    None => continue,
                }
            }
            if self.decoder.ended {
                return None;
            }
            let mut buf = [0u8; 4096];
            let n = self.stream.read(&mut buf).await.unwrap();
            if n == 0 {
                return None;
            }
            self.decoder.feed(&buf[..n]);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `GET path` with a loopback `Host`.
pub async fn get(via: &Via, path: &str) -> Response {
    send(via, "GET", path, &["Host: localhost"]).await
}
