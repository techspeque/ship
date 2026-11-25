//! Shared helpers for the daemon integration tests: a config in a temporary
//! directory and a minimal HTTP/1.1 client over the unix socket or TCP.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use ship_daemon::{Config, Redactor};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A config whose state directory and socket live under `dir`, on a free
/// loopback port.
pub fn config(dir: &Path) -> Config {
    Config {
        state_dir: dir.join("state"),
        socket_path: dir.join("ship.sock"),
        port: 0,
        redactor: Redactor::new(),
    }
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

/// `GET path` with a loopback `Host`.
pub async fn get(via: &Via, path: &str) -> Response {
    send(via, "GET", path, &["Host: localhost"]).await
}
