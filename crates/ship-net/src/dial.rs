//! `ship dial-stdio`: the remote end of the ssh transport (ADR-0003).
//!
//! The client runs `ssh HOST ship dial-stdio`; this bridges that process's
//! stdin and stdout to the daemon's unix socket, so the client speaks HTTP
//! to the daemon through ssh's pipes. Socket access is the whole
//! authorisation: dial-stdio adds none and bypasses none.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::connect::{ConnectError, connect_unix};

/// After the client closes its side, how long the daemon has to finish.
pub const DRAIN_AFTER_INPUT_EOF: Duration = Duration::from_secs(10);

/// Connects to `socket` and bridges this process's stdin and stdout to it
/// until either side closes.
pub fn dial_stdio(socket: &Path) -> Result<(), ConnectError> {
    let stream = connect_unix(socket)?;
    bridge(stream, io::stdin(), io::stdout(), DRAIN_AFTER_INPUT_EOF).map_err(|source| {
        ConnectError::Io {
            socket: socket.to_path_buf(),
            source,
        }
    })
}

enum Event {
    /// `input` reached EOF (or failed) and the socket's write half is shut.
    InputClosed,
    /// The socket side ended: the daemon closed, or `output` failed.
    SocketClosed(io::Result<()>),
}

/// Copies `input` → `stream` and `stream` → `output` on two threads.
///
/// Returns when the socket side ends. When `input` ends first, the socket's
/// write half is shut down so the daemon sees the close, and the socket side
/// gets up to `drain` to finish before this returns anyway. The input thread
/// may still be blocked in a read when this returns; the caller exits.
pub fn bridge<R, W>(stream: UnixStream, input: R, output: W, drain: Duration) -> io::Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let to_socket = stream.try_clone()?;
    let from_socket = stream;
    let (tx, rx) = mpsc::channel();

    let tx_in = tx.clone();
    thread::spawn(move || {
        let mut input = input;
        let mut to_socket = to_socket;
        let _ = io::copy(&mut input, &mut to_socket);
        let _ = to_socket.shutdown(Shutdown::Write);
        let _ = tx_in.send(Event::InputClosed);
    });
    thread::spawn(move || {
        let mut from_socket = from_socket;
        let mut output = output;
        let result = copy_flushing(&mut from_socket, &mut output);
        let _ = from_socket.shutdown(Shutdown::Both);
        let _ = tx.send(Event::SocketClosed(result));
    });

    match rx.recv() {
        Ok(Event::SocketClosed(result)) => result,
        Ok(Event::InputClosed) => match rx.recv_timeout(drain) {
            Ok(Event::SocketClosed(result)) => result,
            Ok(Event::InputClosed) | Err(_) => Ok(()),
        },
        Err(_) => Ok(()),
    }
}

/// Like `io::copy`, but flushes after every chunk so a response is never
/// held in a buffer while the client waits for it.
fn copy_flushing(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return to.flush(),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&buf[..n])?;
        to.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    /// A writer whose bytes the test can read afterwards.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A reader that never returns.
    struct Blocked;

    impl Read for Blocked {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            thread::sleep(Duration::from_secs(3600));
            Ok(0)
        }
    }

    #[test]
    fn input_eof_half_closes_and_returns_after_peer_answers() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let out = Shared::default();
        let daemon = thread::spawn(move || {
            let mut got = Vec::new();
            peer.read_to_end(&mut got).unwrap(); // ends when input hits EOF
            peer.write_all(b"pong").unwrap();
            got
        });
        bridge(
            ours,
            Cursor::new(b"ping".to_vec()),
            out.clone(),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(daemon.join().unwrap(), b"ping");
        assert_eq!(&*out.0.lock().unwrap(), b"pong");
    }

    #[test]
    fn returns_when_peer_closes_even_if_input_is_open() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let out = Shared::default();
        thread::spawn(move || {
            peer.write_all(b"bye").unwrap();
            drop(peer);
        });
        let start = Instant::now();
        bridge(ours, Blocked, out.clone(), Duration::from_secs(30)).unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(&*out.0.lock().unwrap(), b"bye");
    }

    #[test]
    fn drain_is_bounded_when_peer_never_closes() {
        let (ours, peer) = UnixStream::pair().unwrap();
        let start = Instant::now();
        bridge(
            ours,
            Cursor::new(Vec::new()),
            Shared::default(),
            Duration::from_millis(200),
        )
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        drop(peer);
    }

    #[test]
    fn missing_socket_reports_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let e = dial_stdio(&dir.path().join("none.sock")).unwrap_err();
        assert!(matches!(e, ConnectError::NoDaemon { .. }), "{e:?}");
    }
}
