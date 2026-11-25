//! Shutdown drains both listeners against one 10 s deadline, then closes
//! every connection still open before returning (f-004).

mod common;

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A request with its header block left unfinished.
const PARTIAL: &[u8] = b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n";

/// Finishes the request and reads whatever comes back; a closed or reset
/// connection yields what was read before it ended.
async fn finish<S: AsyncReadExt + AsyncWriteExt + Unpin>(s: &mut S) -> Vec<u8> {
    let _ = s.write_all(b"\r\n").await;
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    out
}

#[tokio::test]
async fn incomplete_requests_are_closed_within_one_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let running = ship_daemon::start(&common::config(dir.path()))
        .await
        .unwrap();

    let mut unix = tokio::net::UnixStream::connect(running.socket_path())
        .await
        .unwrap();
    unix.write_all(PARTIAL).await.unwrap();
    let mut tcp = tokio::net::TcpStream::connect(running.tcp_addr())
        .await
        .unwrap();
    tcp.write_all(PARTIAL).await.unwrap();
    // Let both connections be accepted and their headers start parsing.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let began = Instant::now();
    running.shutdown().await.unwrap();
    let took = began.elapsed();
    assert!(
        took < Duration::from_secs(12),
        "shutdown took {took:?}: one deadline per listener"
    );

    for (name, reply) in [
        ("unix", finish(&mut unix).await),
        ("tcp", finish(&mut tcp).await),
    ] {
        let text = String::from_utf8_lossy(&reply);
        assert!(
            !text.contains("HTTP/1.1"),
            "{name} connection answered after shutdown: {text}"
        );
    }
}
