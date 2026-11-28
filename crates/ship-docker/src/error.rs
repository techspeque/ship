//! What can go wrong talking to Docker, with "Docker did not answer" kept
//! apart from "Docker answered no" (ADR-0006 rule 4).

use std::fmt;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use ship_core::SecretValue;

use crate::runner::RunError;

/// Longest piece of Docker's error output quoted in an error.
pub const MAX_DETAIL_LEN: usize = 500;

/// Why an adapter call did not do what was asked.
#[derive(Debug)]
pub enum DockerError {
    /// Docker could not be asked: the CLI is missing, or the Engine's socket
    /// is missing, refused or not permitted. Nothing is known about the
    /// objects; this is `unknown`, never `stopped`.
    Unreachable(String),
    /// The command ran and exited non-zero.
    Failed {
        /// What was run, e.g. `compose stop`.
        command: &'static str,
        status: ExitStatus,
        /// The last lines of its stderr, cut to [`MAX_DETAIL_LEN`].
        detail: String,
    },
    /// `compose config` ran and failed. Its stderr may quote interpolated
    /// values, so it is held as a secret (ADR-0008 rule 7).
    ConfigFailed {
        status: ExitStatus,
        stderr: SecretValue,
    },
    /// It ran longer than this and was killed.
    Timeout(Duration),
    /// Its output exceeded this many bytes and it was killed.
    OutputTooLarge(usize),
    /// Reading its output or waiting for it failed.
    Io(io::Error),
    /// Docker answered with something the adapter cannot read.
    Malformed(String),
    /// The Compose project holds an object without Ship's labels for its
    /// environment; nothing was done.
    Foreign {
        /// `container` or `network`.
        kind: &'static str,
        id: String,
    },
    /// An argument the adapter refuses to pass to Docker.
    InvalidArgument(String),
}

impl DockerError {
    /// Whether Docker could not be asked at all.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, DockerError::Unreachable(_))
    }

    /// The error for a run that produced no output.
    pub(crate) fn from_run(error: RunError) -> DockerError {
        match error {
            RunError::Spawn(e) if e.kind() == io::ErrorKind::NotFound => {
                DockerError::Unreachable("the docker CLI was not found".to_owned())
            }
            RunError::Spawn(e) => DockerError::Unreachable(format!("could not start docker: {e}")),
            RunError::Timeout(d) => DockerError::Timeout(d),
            RunError::OutputTooLarge(n) => DockerError::OutputTooLarge(n),
            RunError::Io(e) => DockerError::Io(e),
        }
    }

    /// The error for a command that exited non-zero with `stderr`:
    /// [`DockerError::Unreachable`] when the stderr says the Engine did not
    /// answer, else [`DockerError::Failed`].
    pub(crate) fn from_exit(command: &'static str, status: ExitStatus, stderr: &str) -> Self {
        let detail = tail(stderr);
        if is_unreachable_message(stderr) {
            DockerError::Unreachable(detail)
        } else {
            DockerError::Failed {
                command,
                status,
                detail,
            }
        }
    }
}

impl fmt::Display for DockerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DockerError::Unreachable(why) => write!(f, "Docker is not reachable: {why}"),
            DockerError::Failed {
                command,
                status,
                detail,
            } => {
                write!(f, "docker {command} failed ({status})")?;
                if !detail.is_empty() {
                    write!(f, ": {detail}")?;
                }
                Ok(())
            }
            DockerError::ConfigFailed { status, .. } => write!(
                f,
                "docker compose config failed ({status}); its output is withheld because it may contain interpolated values"
            ),
            DockerError::Timeout(d) => {
                write!(f, "docker gave no answer within {} s", d.as_secs_f32())
            }
            DockerError::OutputTooLarge(n) => write!(f, "docker output exceeded {n} bytes"),
            DockerError::Io(e) => write!(f, "talking to docker failed: {e}"),
            DockerError::Malformed(why) => write!(f, "unreadable docker output: {why}"),
            DockerError::Foreign { kind, id } => write!(
                f,
                "the Compose project holds a {kind} ({id}) without Ship's labels; refusing to touch it"
            ),
            DockerError::InvalidArgument(why) => write!(f, "refused argument: {why}"),
        }
    }
}

impl std::error::Error for DockerError {}

/// Messages the docker CLI and Compose print when the Engine does not
/// answer, as opposed to answering with an error.
const UNREACHABLE_MARKERS: &[&str] = &[
    "cannot connect to the docker daemon",
    "is the docker daemon running",
    "permission denied while trying to connect to the docker daemon",
    "error during connect",
];

/// Whether `stderr` says the Engine could not be reached.
pub(crate) fn is_unreachable_message(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    UNREACHABLE_MARKERS.iter().any(|m| lower.contains(m))
}

/// The last non-empty lines of `text`, at most [`MAX_DETAIL_LEN`]
/// characters, joined with `; `.
pub(crate) fn tail(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut len = 0;
    for line in text.lines().rev().map(str::trim).filter(|l| !l.is_empty()) {
        if len + line.chars().count() > MAX_DETAIL_LEN {
            if out.is_empty() {
                let cut: String = line.chars().take(MAX_DETAIL_LEN).collect();
                return format!("{cut}…");
            }
            break;
        }
        len += line.chars().count() + 2;
        out.push(line);
    }
    out.reverse();
    out.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn exit(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn unreachable_messages_are_classified() {
        for msg in [
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
            "permission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock",
            "error during connect: Get \"http://%2Fvar%2Frun%2Fdocker.sock/v1.47/containers/json\": dial unix",
        ] {
            assert!(
                DockerError::from_exit("ps", exit(1), msg).is_unreachable(),
                "{msg}"
            );
        }
    }

    #[test]
    fn a_daemon_error_is_failed() {
        let err = DockerError::from_exit(
            "compose stop",
            exit(1),
            "Error response from daemon: No such container: abc",
        );
        match &err {
            DockerError::Failed {
                command, detail, ..
            } => {
                assert_eq!(*command, "compose stop");
                assert!(detail.contains("No such container"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
        assert!(err.to_string().contains("No such container"));
    }

    #[test]
    fn missing_cli_is_unreachable() {
        let err = DockerError::from_run(RunError::Spawn(io::Error::from(io::ErrorKind::NotFound)));
        assert!(err.is_unreachable());
    }

    #[test]
    fn config_failure_withholds_its_output() {
        let err = DockerError::ConfigFailed {
            status: exit(15),
            stderr: SecretValue::new("password=hunter2"),
        };
        assert!(!err.to_string().contains("hunter2"));
        assert!(!format!("{err:?}").contains("hunter2"));
    }

    #[test]
    fn tail_keeps_the_last_lines_within_the_limit() {
        assert_eq!(tail("a\n\n b \nc\n"), "a; b; c");
        let long = "x".repeat(2000);
        assert_eq!(tail(&long).chars().count(), MAX_DETAIL_LEN + 1);
        let many = (0..500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = tail(&many);
        assert!(t.ends_with("line 499"), "{t}");
        assert!(t.chars().count() <= MAX_DETAIL_LEN);
    }
}
