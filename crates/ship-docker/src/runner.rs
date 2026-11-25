//! The one way Ship runs a Docker subprocess (ADR-0001 rule 2).
//!
//! A [`Runner`] executes its program with an argument vector. Every argument
//! reaches the child's `argv` unchanged; nothing is ever interpreted by a
//! shell, and no method accepts a command line as a string.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

/// Default time a command may run.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default cap on each of stdout and stderr, in bytes.
pub const DEFAULT_MAX_OUTPUT: usize = 1024 * 1024;

/// Runs one program with structured argv, a timeout and bounded output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    program: OsString,
    env: Vec<(OsString, OsString)>,
    timeout: Duration,
    max_output: usize,
}

impl Default for Runner {
    fn default() -> Self {
        Runner::docker()
    }
}

impl Runner {
    /// The `docker` CLI found on `PATH`, with the default limits.
    pub fn docker() -> Self {
        Runner::new("docker")
    }

    /// `program` (a path, or a name looked up on `PATH`) with the default
    /// limits.
    pub fn new(program: impl Into<OsString>) -> Self {
        Runner {
            program: program.into(),
            env: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }

    /// Sets `key` to `value` in the child's environment; the rest of the
    /// environment is inherited.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// How long a command may run before it is killed.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The most bytes kept from each of stdout and stderr; more kills the
    /// command.
    pub fn max_output(mut self, bytes: usize) -> Self {
        self.max_output = bytes;
        self
    }

    /// The program this runner executes.
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// Runs the program with `args` as its arguments, stdin closed, in its
    /// own process group. Returns its exit status and output whatever the
    /// status; a non-zero exit is not an error here.
    ///
    /// On timeout or oversize output the whole process group is killed.
    pub async fn run<I, S>(&self, args: I) -> Result<Output, RunError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(RunError::Spawn)?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            kill(&mut child).await;
            return Err(RunError::Io(io::Error::other("child pipes missing")));
        };

        let limit = self.max_output;
        let outcome = tokio::time::timeout(self.timeout, async {
            let (stdout, stderr) =
                tokio::try_join!(read_bounded(stdout, limit), read_bounded(stderr, limit))?;
            let status = child.wait().await.map_err(RunError::Io)?;
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        })
        .await;

        match outcome {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(e)) => {
                kill(&mut child).await;
                Err(e)
            }
            Err(_) => {
                kill(&mut child).await;
                Err(RunError::Timeout(self.timeout))
            }
        }
    }
}

/// Reads `reader` to its end, failing once more than `limit` bytes arrive.
async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> Result<Vec<u8>, RunError> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader.read(&mut chunk).await.map_err(RunError::Io)?;
        if n == 0 {
            return Ok(out);
        }
        if out.len() + n > limit {
            return Err(RunError::OutputTooLarge(limit));
        }
        out.extend_from_slice(&chunk[..n]);
    }
}

/// Kills the child's process group (the child and anything it started, such
/// as a Compose plugin), then reaps the child.
async fn kill(child: &mut Child) {
    if let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) {
        // SAFETY: kill(2) with a negative pid signals that process group; it
        // touches no memory. The group is the child's own (process_group(0)),
        // and the child is not yet reaped, so the id cannot have been reused.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
}

/// A finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    /// Whether the command exited with status 0.
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// stdout as text, lossily decoded and trimmed.
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }

    /// stderr as text, lossily decoded and trimmed.
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }
}

/// Why a command produced no [`Output`].
#[derive(Debug)]
pub enum RunError {
    /// The program could not be started; `NotFound` when it does not exist.
    Spawn(io::Error),
    /// It ran longer than this and was killed.
    Timeout(Duration),
    /// stdout or stderr exceeded this many bytes and it was killed.
    OutputTooLarge(usize),
    /// Reading its output or waiting for it failed.
    Io(io::Error),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Spawn(e) if e.kind() == io::ErrorKind::NotFound => {
                write!(f, "program not found")
            }
            RunError::Spawn(e) => write!(f, "could not start: {e}"),
            RunError::Timeout(d) => write!(f, "no answer within {} s", d.as_secs_f32()),
            RunError::OutputTooLarge(n) => write!(f, "output exceeded {n} bytes"),
            RunError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const NASTY: &str =
        "a b; rm -rf / && echo $(id) `id` | cat > /tmp/x < y 'q' \"d\" * ? \\ ~ # !";

    #[tokio::test]
    async fn metacharacters_reach_argv_unchanged() {
        let out = Runner::new("/usr/bin/printf")
            .run(["%s", NASTY])
            .await
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, NASTY.as_bytes());
    }

    #[tokio::test]
    async fn each_argument_stays_one_argument() {
        // Five arguments, one per line, even with spaces and an empty one.
        let out = Runner::new("/usr/bin/printf")
            .run(["%s\\n", "one two", "", "$HOME", "*"])
            .await
            .unwrap();
        assert_eq!(out.stdout_text(), "one two\n\n$HOME\n*");
    }

    #[tokio::test]
    async fn non_zero_exit_is_output() {
        let out = Runner::new("/bin/ls")
            .run(["/definitely/not/here"])
            .await
            .unwrap();
        assert!(!out.success());
        assert!(!out.stderr.is_empty());
    }

    #[tokio::test]
    async fn timeout_kills() {
        let start = Instant::now();
        let err = Runner::new("/bin/sleep")
            .timeout(Duration::from_millis(200))
            .run(["30"])
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::Timeout(_)), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn output_is_bounded() {
        let start = Instant::now();
        let err = Runner::new("/usr/bin/yes")
            .max_output(4096)
            .run(std::iter::empty::<&str>())
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::OutputTooLarge(4096)), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn output_at_the_limit_is_kept() {
        let out = Runner::new("/usr/bin/printf")
            .max_output(5)
            .run(["%s", "12345"])
            .await
            .unwrap();
        assert_eq!(out.stdout, b"12345");
    }

    #[tokio::test]
    async fn missing_program_is_not_found() {
        let err = Runner::new("/nonexistent/ship-test-docker")
            .run(["version"])
            .await
            .unwrap_err();
        match err {
            RunError::Spawn(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn env_override_reaches_child() {
        let out = Runner::new("/usr/bin/env")
            .env("SHIP_RUNNER_TEST", "x;y $z")
            .run(std::iter::empty::<&str>())
            .await
            .unwrap();
        assert!(
            out.stdout_text()
                .lines()
                .any(|l| l == "SHIP_RUNNER_TEST=x;y $z")
        );
    }

    #[tokio::test]
    async fn stdin_is_closed() {
        // `cat` with no arguments reads stdin; closed stdin ends it at once.
        let out = Runner::new("/bin/cat")
            .timeout(Duration::from_secs(5))
            .run(std::iter::empty::<&str>())
            .await
            .unwrap();
        assert!(out.success());
        assert!(out.stdout.is_empty());
    }
}
