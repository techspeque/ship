//! Running a command whose output matters as it happens: a build, `up`,
//! `logs --follow`. Each line goes to a sink the moment it is complete,
//! marked stdout or stderr.

use std::process::ExitStatus;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;

use crate::runner::{RunError, Runner};

/// Longest line passed to a sink, in bytes; a longer one is cut into
/// pieces of this size.
pub const MAX_LINE: usize = 64 * 1024;
/// How many of the last stderr lines are kept for the error message.
const STDERR_TAIL_LINES: usize = 20;

/// Which of a command's outputs a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// One line of output, without its line ending, lossily decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub stream: Stream,
    pub text: String,
}

/// A finished streamed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Streamed {
    pub status: ExitStatus,
    /// The last [`STDERR_TAIL_LINES`] stderr lines, joined with newlines.
    pub stderr_tail: String,
}

/// Kills the process group of a child that has not been reaped, when the
/// streaming future is dropped or times out.
struct GroupGuard(Option<i32>);

impl GroupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: kill(2) on the child's own process group
            // (process_group(0)); the child is not yet reaped while armed,
            // so the id cannot have been reused. Touches no memory.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

/// Runs `args` through `runner`, passing every line of stdout and stderr to
/// `sink` as it arrives. `timeout` of `None` means no limit (a followed log);
/// the caller stops it by dropping the future, which kills the process
/// group.
pub(crate) async fn stream<I, S>(
    runner: &Runner,
    args: I,
    timeout: Option<Duration>,
    sink: &mut (dyn FnMut(Line) + Send),
) -> Result<Streamed, RunError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut child = runner.command(args).spawn().map_err(RunError::Spawn)?;
    let mut guard = GroupGuard(child.id().and_then(|p| i32::try_from(p).ok()));
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        crate::runner::kill(&mut child).await;
        guard.disarm();
        return Err(RunError::Io(std::io::Error::other("child pipes missing")));
    };

    let (tx, mut rx) = mpsc::channel::<Line>(64);
    let out = tokio::spawn(forward(stdout, Stream::Stdout, tx.clone()));
    let err = tokio::spawn(forward(stderr, Stream::Stderr, tx));

    let body = async {
        let mut tail: std::collections::VecDeque<String> = Default::default();
        while let Some(line) = rx.recv().await {
            if line.stream == Stream::Stderr {
                if tail.len() == STDERR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line.text.clone());
            }
            sink(line);
        }
        for task in [out, err] {
            task.await
                .map_err(|e| RunError::Io(std::io::Error::other(e)))?
                .map_err(RunError::Io)?;
        }
        let status = child.wait().await.map_err(RunError::Io)?;
        Ok::<_, RunError>(Streamed {
            status,
            stderr_tail: Vec::from(tail).join("\n"),
        })
    };

    let result = match timeout {
        Some(limit) => match tokio::time::timeout(limit, body).await {
            Ok(r) => r,
            Err(_) => Err(RunError::Timeout(limit)),
        },
        None => body.await,
    };
    match result {
        Ok(done) => {
            guard.disarm();
            Ok(done)
        }
        // The guard kills the group on drop; the child is reaped by
        // kill_on_drop.
        Err(e) => Err(e),
    }
}

/// Reads `reader` line by line into `tx`, cutting lines at [`MAX_LINE`].
async fn forward<R: AsyncRead + Unpin>(
    reader: R,
    stream: Stream,
    tx: mpsc::Sender<Line>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut line: Vec<u8> = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            if !line.is_empty() {
                send(&tx, stream, &line).await;
            }
            return Ok(());
        }
        let room = MAX_LINE - line.len();
        let window = &buf[..buf.len().min(room)];
        match window.iter().position(|&b| b == b'\n') {
            Some(i) => {
                line.extend_from_slice(&window[..i]);
                reader.consume(i + 1);
                send(&tx, stream, &line).await;
                line.clear();
            }
            None => {
                let n = window.len();
                line.extend_from_slice(window);
                reader.consume(n);
                if line.len() == MAX_LINE {
                    send(&tx, stream, &line).await;
                    line.clear();
                }
            }
        }
    }
}

async fn send(tx: &mpsc::Sender<Line>, stream: Stream, bytes: &[u8]) {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    let text = String::from_utf8_lossy(bytes).into_owned();
    // A closed receiver means the caller stopped listening; keep draining
    // so the child is never blocked on a full pipe.
    let _ = tx.send(Line { stream, text }).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    async fn collect(
        args: &[&str],
        timeout: Option<Duration>,
    ) -> (Result<Streamed, RunError>, Vec<Line>) {
        let mut lines = Vec::new();
        let r = stream(&Runner::new("/bin/sh"), args, timeout, &mut |l| {
            lines.push(l)
        })
        .await;
        (r, lines)
    }

    #[tokio::test]
    async fn stdout_and_stderr_are_told_apart() {
        let (r, lines) = collect(
            &["-c", "echo out1; echo err1 >&2; printf 'out2'; exit 3"],
            Some(Duration::from_secs(5)),
        )
        .await;
        let r = r.unwrap();
        assert_eq!(r.status.code(), Some(3));
        assert_eq!(r.stderr_tail, "err1");
        let of = |s| {
            lines
                .iter()
                .filter(|l| l.stream == s)
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(of(Stream::Stdout), ["out1", "out2"]);
        assert_eq!(of(Stream::Stderr), ["err1"]);
    }

    #[tokio::test]
    async fn long_lines_are_cut() {
        let script = format!(
            "head -c {} /dev/zero | tr '\\0' x; echo; echo end",
            MAX_LINE * 2 + 5
        );
        let (r, lines) = collect(&["-c", &script], Some(Duration::from_secs(10))).await;
        assert!(r.unwrap().status.success());
        let lens: Vec<usize> = lines.iter().map(|l| l.text.len()).collect();
        assert_eq!(lens, [MAX_LINE, MAX_LINE, 5, 3]);
    }

    #[tokio::test]
    async fn timeout_kills_the_group() {
        let start = Instant::now();
        let (r, lines) = collect(
            &["-c", "echo started; sleep 30 & wait"],
            Some(Duration::from_millis(300)),
        )
        .await;
        assert!(matches!(r, Err(RunError::Timeout(_))), "{r:?}");
        assert_eq!(lines.first().map(|l| l.text.as_str()), Some("started"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn lines_arrive_before_the_command_ends() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut sink = move |l: Line| {
                let _ = tx.send(l);
            };
            stream(
                &Runner::new("/bin/sh"),
                ["-c", "echo first; sleep 30"],
                Some(Duration::from_secs(20)),
                &mut sink,
            )
            .await
        });
        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a line before the command ends")
            .unwrap();
        assert_eq!(first.text, "first");
        task.abort();
    }
}
