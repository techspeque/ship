//! `ship_net::forward::serve` with `cat` (an echo) or `sh` standing in for
//! `ssh … ship dial-stdio`.
//!
//! These spawn child processes, so they live in their own test binary: a
//! fork in the lib tests' process would briefly hold the fds of unrelated
//! tests (for example a unix listener a listener test has just dropped).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ship_net::forward::{Dialer, Forward, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

fn echo(limit: usize) -> Forward {
    Forward {
        dialer: Dialer {
            program: "cat".into(),
            args: Vec::new(),
        },
        limit,
        drain: Duration::from_secs(5),
    }
}

struct Served {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Served {
    async fn start(forward: Forward) -> Served {
        let std = ship_net::bind_loopback(0).unwrap();
        std.set_nonblocking(true).unwrap();
        let listener = TcpListener::from_std(std).unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, mut rx) = watch::channel(false);
        let errors = Arc::new(Mutex::new(Vec::new()));
        let seen = errors.clone();
        let task = tokio::spawn(serve(
            listener,
            forward,
            async move {
                let _ = rx.wait_for(|stop| *stop).await;
            },
            move |e| seen.lock().unwrap().push(e),
        ));
        Served {
            addr,
            stop,
            task,
            errors,
        }
    }

    /// Stops serving; `serve` must return (every child reaped) promptly.
    async fn shutdown(self) -> Vec<String> {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("serve did not return")
            .unwrap();
        std::mem::take(&mut *self.errors.lock().unwrap())
    }
}

async fn roundtrip(stream: &mut TcpStream, data: &[u8]) -> Vec<u8> {
    stream.write_all(data).await.unwrap();
    let mut got = vec![0u8; data.len()];
    stream.read_exact(&mut got).await.unwrap();
    got
}

async fn read_to_eof(stream: &mut TcpStream) -> Vec<u8> {
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut rest))
        .await
        .expect("no EOF")
        .ok();
    rest
}

#[tokio::test]
async fn bytes_roundtrip_and_eof_closes() {
    let served = Served::start(echo(4)).await;
    let mut s = TcpStream::connect(served.addr).await.unwrap();
    assert_eq!(roundtrip(&mut s, b"ping").await, b"ping");
    // Closing our write side ends cat, which closes the connection.
    s.shutdown().await.unwrap();
    assert!(read_to_eof(&mut s).await.is_empty());
    assert!(served.shutdown().await.is_empty());
}

#[tokio::test]
async fn connections_over_the_limit_wait_for_a_slot() {
    let served = Served::start(echo(1)).await;
    let mut first = TcpStream::connect(served.addr).await.unwrap();
    assert_eq!(roundtrip(&mut first, b"one").await, b"one");

    // Connected (backlog), but not carried while `first` holds the slot.
    let mut second = TcpStream::connect(served.addr).await.unwrap();
    second.write_all(b"two").await.unwrap();
    let mut buf = [0u8; 3];
    let early = tokio::time::timeout(Duration::from_millis(300), second.read_exact(&mut buf));
    assert!(early.await.is_err(), "served over the limit");

    drop(first);
    tokio::time::timeout(Duration::from_secs(5), second.read_exact(&mut buf))
        .await
        .expect("not served after the slot freed")
        .unwrap();
    assert_eq!(&buf, b"two");
    served.shutdown().await;
}

#[tokio::test]
async fn shutdown_kills_live_children_and_closes_the_port() {
    let served = Served::start(echo(4)).await;
    let addr = served.addr;
    let mut held = Vec::new();
    for _ in 0..3 {
        let mut s = TcpStream::connect(addr).await.unwrap();
        assert_eq!(roundtrip(&mut s, b"x").await, b"x");
        held.push(s);
    }
    // Returns although every connection is still open: each child was
    // killed and reaped, so each connection then reads EOF.
    served.shutdown().await;
    for mut s in held {
        read_to_eof(&mut s).await;
    }
    assert!(TcpStream::connect(addr).await.is_err(), "port still open");
}

#[tokio::test]
async fn a_failing_child_reports_its_stderr() {
    let message = "channel 1: open failed: administratively prohibited";
    let forward = Forward {
        dialer: Dialer {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                format!("echo '{message}' >&2; exit 255").into(),
            ],
        },
        limit: 2,
        drain: Duration::from_secs(5),
    };
    let served = Served::start(forward).await;
    let mut s = TcpStream::connect(served.addr).await.unwrap();
    read_to_eof(&mut s).await;
    assert_eq!(served.shutdown().await, [message]);
}

#[tokio::test]
async fn a_missing_program_is_reported() {
    let forward = Forward {
        dialer: Dialer {
            program: "/nonexistent/ssh".into(),
            args: Vec::new(),
        },
        limit: 2,
        drain: Duration::from_secs(5),
    };
    let served = Served::start(forward).await;
    let mut s = TcpStream::connect(served.addr).await.unwrap();
    read_to_eof(&mut s).await;
    let errors = served.shutdown().await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].starts_with("cannot run `/nonexistent/ssh`"),
        "{errors:?}"
    );
}
