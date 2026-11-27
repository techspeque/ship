//! Is Docker usable? Two read-only commands and a verdict.

use ship_core::{DockerInfo, DockerStatus, SupportedVersions};

use crate::runner::{RunError, Runner};
use crate::supported::{COMPOSE, ENGINE, Version, VersionRange};

/// Asks the Engine for its version; fails when the Engine does not answer.
pub const ENGINE_VERSION_ARGS: &[&str] = &["version", "--format", "{{.Server.Version}}"];
/// Asks the Compose v2 plugin for its version; needs no Engine.
pub const COMPOSE_VERSION_ARGS: &[&str] = &["compose", "version", "--short"];

/// Longest version string reported back as-is.
const MAX_VERSION_LEN: usize = 64;
/// Longest piece of Docker's error output quoted in a message.
const MAX_DETAIL_LEN: usize = 300;

/// What one version command produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// It succeeded and printed this (trimmed).
    Answered(String),
    /// It failed; why, in one line.
    Failed(String),
}

/// Runs [`ENGINE_VERSION_ARGS`] and [`COMPOSE_VERSION_ARGS`] through
/// `runner` and judges the result against [`ENGINE`] and [`COMPOSE`].
///
/// Read-only: it creates, changes and removes no Docker object.
pub async fn preflight(runner: &Runner) -> DockerInfo {
    let (engine, compose) = tokio::join!(
        probe(runner, ENGINE_VERSION_ARGS),
        probe(runner, COMPOSE_VERSION_ARGS)
    );
    evaluate(&engine, &compose)
}

async fn probe(runner: &Runner, args: &[&str]) -> Probe {
    match runner.run(args).await {
        Ok(out) if out.success() => Probe::Answered(out.stdout_text()),
        Ok(out) => {
            let err = out.stderr_text();
            let detail = if err.is_empty() {
                out.stdout_text()
            } else {
                err
            };
            Probe::Failed(if detail.is_empty() {
                format!("exited with {}", out.status)
            } else {
                one_line(&detail)
            })
        }
        Err(RunError::Spawn(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            Probe::Failed(format!(
                "the docker CLI ({}) was not found",
                runner.program().to_string_lossy()
            ))
        }
        Err(e) => Probe::Failed(e.to_string()),
    }
}

/// The verdict on two probes. `unreachable` outranks `unsupported_version`.
pub fn evaluate(engine: &Probe, compose: &Probe) -> DockerInfo {
    let supported = SupportedVersions {
        engine: ENGINE.to_string(),
        compose: COMPOSE.to_string(),
    };
    let engine_version = reported(engine);
    let compose_version = reported(compose);

    if let Probe::Failed(why) = engine {
        return DockerInfo {
            status: DockerStatus::Unreachable,
            engine_version: None,
            compose_version,
            supported,
            message: Some(format!(
                "Docker Engine is not reachable: {why}. Start Docker \
                 (systemctl start docker) and make sure the user running Ship \
                 can use the Docker socket (member of the docker group)."
            )),
        };
    }

    let mut problems = Vec::new();
    if let Some(p) = judge("Docker Engine", engine, ENGINE) {
        problems.push(p);
    }
    if let Some(p) = judge("Docker Compose v2 plugin", compose, COMPOSE) {
        problems.push(p);
    }
    let (status, message) = if problems.is_empty() {
        (DockerStatus::Ready, None)
    } else {
        (DockerStatus::UnsupportedVersion, Some(problems.join("; ")))
    };
    DockerInfo {
        status,
        engine_version,
        compose_version,
        supported,
        message,
    }
}

/// `None` when `probe` answered with a version inside `range`; otherwise
/// what is wrong, naming the range.
fn judge(what: &str, probe: &Probe, range: VersionRange) -> Option<String> {
    match probe {
        Probe::Answered(text) => match Version::parse(text) {
            Some(v) if range.contains(v) => None,
            Some(v) => Some(format!("{what} {v} is outside the supported range {range}")),
            None => Some(format!(
                "{what} reported an unreadable version ({}); supported range is {range}",
                reported(probe).unwrap_or_else(|| "unprintable".to_owned())
            )),
        },
        Probe::Failed(why) => Some(format!(
            "{what} not usable ({why}); install a version in the supported range {range}"
        )),
    }
}

/// The version text worth showing: short and made of version characters.
fn reported(probe: &Probe) -> Option<String> {
    match probe {
        Probe::Answered(text)
            if !text.is_empty()
                && text.len() <= MAX_VERSION_LEN
                && text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '~')) =>
        {
            Some(text.clone())
        }
        _ => None,
    }
}

/// The first non-empty line of `text`, cut to [`MAX_DETAIL_LEN`] characters.
fn one_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut out: String = line.chars().take(MAX_DETAIL_LEN).collect();
    if line.chars().count() > MAX_DETAIL_LEN {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Probe {
        Probe::Answered(s.to_owned())
    }

    fn failed(s: &str) -> Probe {
        Probe::Failed(s.to_owned())
    }

    #[test]
    fn ready_when_both_in_range() {
        let info = evaluate(&ok("28.3.2"), &ok("2.39.1"));
        assert_eq!(info.status, DockerStatus::Ready);
        assert_eq!(info.engine_version.as_deref(), Some("28.3.2"));
        assert_eq!(info.compose_version.as_deref(), Some("2.39.1"));
        assert_eq!(info.message, None);
        assert_eq!(info.supported.engine, ENGINE.to_string());
        assert_eq!(info.supported.compose, COMPOSE.to_string());
    }

    #[test]
    fn unreachable_engine() {
        let info = evaluate(
            &failed("Cannot connect to the Docker daemon at unix:///nope.sock"),
            &ok("2.39.1"),
        );
        assert_eq!(info.status, DockerStatus::Unreachable);
        assert_eq!(info.engine_version, None);
        assert_eq!(info.compose_version.as_deref(), Some("2.39.1"));
        let m = info.message.unwrap();
        assert!(m.contains("Cannot connect"), "{m}");
        assert!(m.contains("systemctl start docker"), "{m}");
        assert!(m.contains("docker group"), "{m}");
    }

    #[test]
    fn unreachable_outranks_unsupported() {
        let info = evaluate(&failed("timeout"), &failed("not a docker command"));
        assert_eq!(info.status, DockerStatus::Unreachable);
    }

    #[test]
    fn old_engine_names_the_range() {
        let info = evaluate(&ok("19.03.0"), &ok("2.39.1"));
        assert_eq!(info.status, DockerStatus::UnsupportedVersion);
        assert_eq!(info.engine_version.as_deref(), Some("19.03.0"));
        let m = info.message.unwrap();
        assert!(m.contains("19.3.0"), "{m}");
        assert!(m.contains(&ENGINE.to_string()), "{m}");
    }

    #[test]
    fn new_engine_is_unsupported() {
        let info = evaluate(&ok("30.0.0"), &ok("2.39.1"));
        assert_eq!(info.status, DockerStatus::UnsupportedVersion);
    }

    #[test]
    fn old_compose_names_the_range() {
        let info = evaluate(&ok("28.3.2"), &ok("2.12.2"));
        assert_eq!(info.status, DockerStatus::UnsupportedVersion);
        let m = info.message.unwrap();
        assert!(m.contains("Compose"), "{m}");
        assert!(m.contains(&COMPOSE.to_string()), "{m}");
    }

    #[test]
    fn missing_compose_is_unsupported() {
        let info = evaluate(
            &ok("28.3.2"),
            &failed("docker: 'compose' is not a docker command."),
        );
        assert_eq!(info.status, DockerStatus::UnsupportedVersion);
        assert_eq!(info.compose_version, None);
        let m = info.message.unwrap();
        assert!(m.contains("not a docker command"), "{m}");
        assert!(m.contains(&COMPOSE.to_string()), "{m}");
    }

    #[test]
    fn both_wrong_are_both_named() {
        let m = evaluate(&ok("20.10.0"), &ok("1.29.2")).message.unwrap();
        assert!(m.contains("Engine"), "{m}");
        assert!(m.contains("Compose"), "{m}");
    }

    #[test]
    fn garbage_version_is_unsupported_and_not_echoed() {
        let junk = "<script>alert(1)</script>";
        let info = evaluate(&ok(junk), &ok("2.39.1"));
        assert_eq!(info.status, DockerStatus::UnsupportedVersion);
        assert_eq!(info.engine_version, None);
        assert!(!info.message.unwrap().contains(junk));
        let long = "1".repeat(200);
        assert_eq!(evaluate(&ok(&long), &ok("2.39.1")).engine_version, None);
    }

    #[test]
    fn detail_is_one_short_line() {
        assert_eq!(one_line("\n  first \nsecond"), "first");
        let long = "x".repeat(1000);
        assert_eq!(one_line(&long).chars().count(), MAX_DETAIL_LEN + 1);
    }

    #[test]
    fn commands_are_read_only() {
        assert_eq!(
            ENGINE_VERSION_ARGS,
            ["version", "--format", "{{.Server.Version}}"]
        );
        assert_eq!(COMPOSE_VERSION_ARGS, ["compose", "version", "--short"]);
    }
}
