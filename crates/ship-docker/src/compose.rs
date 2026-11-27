//! Compose operations on one environment's project.
//!
//! Every mutating operation first checks that each container and network
//! already in the Compose project carries Ship's labels for its
//! environment, and refuses otherwise (ADR-0001 rule 5). Every object it
//! creates is labelled through the override from [`labels_override`].
//!
//! `compose config` output is sensitive: interpolation can expand
//! credentials into it (ADR-0008 rule 7). It lives only in a
//! [`ComposeConfig`], which never prints its content, and this crate logs
//! nothing.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value, json};
use ship_core::{EnvironmentId, ProjectId, ReleaseId, SecretValue, ServiceId};

use crate::argv::{self, Argv, ComposeFiles, ComposeProject, LogOptions, UpOptions};
use crate::error::DockerError;
use crate::inspect::{inspect_json, label_map, parse_ids};
use crate::labels::ShipLabels;
use crate::runner::Runner;
use crate::stream::{Line, stream};

/// The canonical, interpolated Compose model of a project
/// (`compose config --format json`).
///
/// Sensitive: `Debug` prints nothing of it and there is no `Display`. The
/// content is reachable only through [`ComposeConfig::expose`].
#[derive(Clone)]
pub struct ComposeConfig {
    json: Value,
}

impl ComposeConfig {
    /// Parses `compose config --format json` output. A parse error names
    /// the position only, never the content.
    pub fn parse(bytes: &[u8]) -> Result<ComposeConfig, DockerError> {
        let json: Value = serde_json::from_slice(bytes).map_err(|e| {
            DockerError::Malformed(format!(
                "compose config: not JSON at line {} column {}",
                e.line(),
                e.column()
            ))
        })?;
        if !json.is_object() {
            return Err(DockerError::Malformed(
                "compose config: not a JSON object".to_owned(),
            ));
        }
        Ok(ComposeConfig { json })
    }

    /// The model itself. Every call site is a place interpolated values
    /// may leave this type; keep them few and never log what they read.
    pub fn expose(&self) -> &Value {
        &self.json
    }

    /// The service names, sorted.
    pub fn service_names(&self) -> Vec<String> {
        self.section("services").keys().cloned().collect()
    }

    fn section(&self, key: &str) -> Map<String, Value> {
        self.json
            .get(key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }
}

impl fmt::Debug for ComposeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ComposeConfig({})", ship_core::secret::REDACTED)
    }
}

/// The labels a deployment gives everything it creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployLabels {
    pub project: ProjectId,
    pub environment: EnvironmentId,
    /// The release being deployed: on containers and built images.
    pub release: ReleaseId,
    /// The Ship service id of every Compose service in the file.
    pub services: BTreeMap<String, ServiceId>,
    /// The release recorded on networks and volumes, which the
    /// environment's releases share. Keep it the release that first created
    /// them: Compose treats a volume whose labels changed as diverged and
    /// asks to recreate it, which would lose its data.
    pub shared_release: ReleaseId,
}

/// A Compose override that adds Ship's four labels to every service's
/// containers and built image, and to every network and volume the project
/// creates (external ones are not created, so not labelled). It changes
/// nothing else (ADR-0008 rules 4 and 5).
///
/// JSON, which Compose reads as YAML. Holds names from the file and ids,
/// no interpolated value, so it is not sensitive. Every service must have
/// an id in `labels.services`.
pub fn labels_override(
    config: &ComposeConfig,
    labels: &DeployLabels,
) -> Result<String, DockerError> {
    let mut services = Map::new();
    for (name, service) in config.section("services") {
        let id = labels.services.get(&name).ok_or_else(|| {
            DockerError::InvalidArgument(format!("Compose service `{name}` has no Ship service id"))
        })?;
        let ship = ShipLabels {
            project: labels.project,
            environment: labels.environment,
            service: Some(*id),
            release: labels.release,
        }
        .to_json();
        let mut entry = Map::new();
        entry.insert("labels".to_owned(), ship.clone());
        if service.get("build").is_some_and(|b| !b.is_null()) {
            entry.insert("build".to_owned(), json!({ "labels": ship }));
        }
        services.insert(name, Value::Object(entry));
    }
    let shared = ShipLabels {
        project: labels.project,
        environment: labels.environment,
        service: None,
        release: labels.shared_release,
    }
    .to_json();
    let mut out = Map::new();
    out.insert("services".to_owned(), Value::Object(services));
    for key in ["networks", "volumes"] {
        let mut section = Map::new();
        for (name, def) in config.section(key) {
            let external = def
                .get("external")
                .is_some_and(|x| x.as_bool() == Some(true) || x.is_object());
            if !external {
                section.insert(name, json!({ "labels": shared.clone() }));
            }
        }
        if !section.is_empty() {
            out.insert(key.to_owned(), Value::Object(section));
        }
    }
    serde_json::to_string_pretty(&Value::Object(out))
        .map_err(|e| DockerError::Malformed(e.to_string()))
}

/// `compose config --format json`. On failure the stderr is held as a
/// secret, since it may quote interpolated values.
pub async fn config(
    runner: &Runner,
    project: &ComposeProject,
    files: &ComposeFiles,
) -> Result<ComposeConfig, DockerError> {
    let argv = argv::compose_config(project, files)?;
    let out = runner
        .run(argv.args())
        .await
        .map_err(DockerError::from_run)?;
    if !out.success() {
        let stderr = out.stderr_text();
        if crate::error::is_unreachable_message(&stderr) {
            return Err(DockerError::Unreachable(
                "the Engine did not answer compose config".to_owned(),
            ));
        }
        return Err(DockerError::ConfigFailed {
            status: out.status,
            stderr: SecretValue::new(stderr),
        });
    }
    ComposeConfig::parse(&out.stdout)
}

/// `compose build`, streaming its output to `sink`.
pub async fn build<S: AsRef<str>>(
    runner: &Runner,
    project: &ComposeProject,
    files: &ComposeFiles,
    services: &[S],
    timeout: Duration,
    sink: &mut (dyn FnMut(Line) + Send),
) -> Result<(), DockerError> {
    let argv = argv::compose_build(project, files, services)?;
    run_streamed(runner, &argv, Some(timeout), sink).await
}

/// `compose up --detach --no-build`, after checking the project holds
/// nothing without Ship's labels.
pub async fn up<S: AsRef<str>>(
    runner: &Runner,
    project: &ComposeProject,
    files: &ComposeFiles,
    services: &[S],
    options: UpOptions,
    timeout: Duration,
    sink: &mut (dyn FnMut(Line) + Send),
) -> Result<(), DockerError> {
    let argv = argv::compose_up(project, files, services, options)?;
    ensure_only_ours(runner, project).await?;
    run_streamed(runner, &argv, Some(timeout), sink).await
}

/// `compose stop`, after the label check.
pub async fn stop<S: AsRef<str>>(
    runner: &Runner,
    project: &ComposeProject,
    services: &[S],
    timeout: Duration,
    sink: &mut (dyn FnMut(Line) + Send),
) -> Result<(), DockerError> {
    let argv = argv::compose_stop(project, services)?;
    ensure_only_ours(runner, project).await?;
    run_streamed(runner, &argv, Some(timeout), sink).await
}

/// `compose start`, after the label check.
pub async fn start<S: AsRef<str>>(
    runner: &Runner,
    project: &ComposeProject,
    services: &[S],
    timeout: Duration,
    sink: &mut (dyn FnMut(Line) + Send),
) -> Result<(), DockerError> {
    let argv = argv::compose_start(project, services)?;
    ensure_only_ours(runner, project).await?;
    run_streamed(runner, &argv, Some(timeout), sink).await
}

/// `compose restart`, after the label check.
pub async fn restart<S: AsRef<str>>(
    runner: &Runner,
    project: &ComposeProject,
    services: &[S],
    timeout: Duration,
    sink: &mut (dyn FnMut(Line) + Send),
) -> Result<(), DockerError> {
    let argv = argv::compose_restart(project, services)?;
    ensure_only_ours(runner, project).await?;
    run_streamed(runner, &argv, Some(timeout), sink).await
}

/// `compose down` without volumes, after the label check. Containers and
/// networks go; every volume stays.
pub async fn down(
    runner: &Runner,
    project: &ComposeProject,
    timeout: Duration,
    sink: &mut (dyn FnMut(Line) + Send),
) -> Result<(), DockerError> {
    let argv = argv::compose_down(project)?;
    ensure_only_ours(runner, project).await?;
    run_streamed(runner, &argv, Some(timeout), sink).await
}

/// `compose logs`, streaming each line to `sink` with stdout and stderr
/// apart. With `follow` and no `timeout` it runs until the future is
/// dropped.
pub async fn logs<S: AsRef<str>>(
    runner: &Runner,
    project: &ComposeProject,
    services: &[S],
    options: LogOptions,
    timeout: Option<Duration>,
    sink: &mut (dyn FnMut(Line) + Send),
) -> Result<(), DockerError> {
    let argv = argv::compose_logs(project, services, options)?;
    run_streamed(runner, &argv, timeout, sink).await
}

/// `compose ps --all --quiet`: the id of every container in the project,
/// with or without Ship's labels. Use [`crate::containers`] to read Ship's.
pub async fn ps(runner: &Runner, project: &ComposeProject) -> Result<Vec<String>, DockerError> {
    ids(runner, &argv::compose_ps(project)?).await
}

/// Refuses with [`DockerError::Foreign`] when the Compose project holds a
/// container or network without Ship's labels for its environment.
pub async fn ensure_only_ours(
    runner: &Runner,
    project: &ComposeProject,
) -> Result<(), DockerError> {
    let environment = project.environment();
    let containers = ids(runner, &argv::project_container_ids(project)?).await?;
    if !containers.is_empty() {
        let json = inspect_json(runner, &argv::container_inspect(&containers)?).await?;
        check_ours(&json, environment, "container", "/Config/Labels", true)?;
    }
    let networks = ids(runner, &argv::project_network_ids(project)?).await?;
    if !networks.is_empty() {
        let json = inspect_json(runner, &argv::network_inspect(&networks)?).await?;
        check_ours(&json, environment, "network", "/Labels", false)?;
    }
    Ok(())
}

/// Runs an argv that prints object ids, one per line.
async fn ids(runner: &Runner, argv: &Argv) -> Result<Vec<String>, DockerError> {
    let out = runner
        .run(argv.args())
        .await
        .map_err(DockerError::from_run)?;
    if !out.success() {
        return Err(DockerError::from_exit(
            argv.command(),
            out.status,
            &out.stderr_text(),
        ));
    }
    parse_ids(&out.stdout_text())
}

/// Checks every object in inspect output carries Ship's labels for
/// `environment` (and a service, when `needs_service`).
pub(crate) fn check_ours(
    json: &[u8],
    environment: EnvironmentId,
    kind: &'static str,
    labels_at: &str,
    needs_service: bool,
) -> Result<(), DockerError> {
    let items: Vec<Value> = serde_json::from_slice(json)
        .map_err(|e| DockerError::Malformed(format!("{kind} inspect: {e}")))?;
    for item in &items {
        let ours = ShipLabels::parse(&label_map(item.pointer(labels_at))).is_some_and(|l| {
            l.environment == environment && (!needs_service || l.service.is_some())
        });
        if !ours {
            let id = item
                .get("Id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .chars()
                .take(64)
                .collect();
            return Err(DockerError::Foreign { kind, id });
        }
    }
    Ok(())
}

async fn run_streamed(
    runner: &Runner,
    argv: &Argv,
    timeout: Option<Duration>,
    sink: &mut (dyn FnMut(Line) + Send),
) -> Result<(), DockerError> {
    let done = stream(runner, argv.args(), timeout, sink)
        .await
        .map_err(DockerError::from_run)?;
    if done.status.success() {
        Ok(())
    } else {
        Err(DockerError::from_exit(
            argv.command(),
            done.status,
            &done.stderr_tail,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::tests::ids;

    fn deploy_labels() -> DeployLabels {
        let (p, e, s, r) = ids();
        DeployLabels {
            project: p,
            environment: e,
            release: r,
            services: [
                ("web".to_owned(), s),
                ("db".to_owned(), ServiceId::from_parts(7, 7).unwrap()),
            ]
            .into(),
            shared_release: ReleaseId::from_parts(1, 1).unwrap(),
        }
    }

    fn canonical() -> ComposeConfig {
        ComposeConfig::parse(
            json!({
                "name": "ship_x",
                "services": {
                    "web": {
                        "build": { "context": "/snap", "dockerfile": "Dockerfile" },
                        "environment": { "PASSWORD": "hunter2" },
                        "networks": { "default": null, "back": null },
                    },
                    "db": { "image": "postgres:16", "volumes": [] },
                },
                "networks": {
                    "default": { "name": "ship_x_default" },
                    "back": { "name": "ship_x_back" },
                    "outside": { "name": "outside", "external": true },
                },
                "volumes": {
                    "data": { "name": "ship_x_data" },
                    "shared": { "name": "shared", "external": true },
                },
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn override_labels_everything_created() {
        let labels = deploy_labels();
        let text = labels_override(&canonical(), &labels).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        let svc = |name: &str| {
            let id = labels.services[name];
            json!({
                "dev.ship.project": labels.project.to_string(),
                "dev.ship.environment": labels.environment.to_string(),
                "dev.ship.service": id.to_string(),
                "dev.ship.release": labels.release.to_string(),
            })
        };
        let shared = json!({
            "dev.ship.project": labels.project.to_string(),
            "dev.ship.environment": labels.environment.to_string(),
            "dev.ship.service": "",
            "dev.ship.release": labels.shared_release.to_string(),
        });
        assert_eq!(
            v,
            json!({
                "services": {
                    "web": { "labels": svc("web"), "build": { "labels": svc("web") } },
                    "db": { "labels": svc("db") },
                },
                "networks": {
                    "default": { "labels": shared },
                    "back": { "labels": shared },
                },
                "volumes": {
                    "data": { "labels": shared },
                },
            })
        );
    }

    #[test]
    fn override_needs_every_service_id() {
        let mut labels = deploy_labels();
        labels.services.remove("db");
        assert!(matches!(
            labels_override(&canonical(), &labels),
            Err(DockerError::InvalidArgument(_))
        ));
    }

    #[test]
    fn config_never_prints_its_content() {
        let c = canonical();
        assert!(!format!("{c:?}").contains("hunter2"));
        assert_eq!(c.service_names(), ["db", "web"]);
        let err = ComposeConfig::parse(b"{\"password\": hunter2}").unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
        assert!(ComposeConfig::parse(b"[]").is_err());
    }

    #[test]
    fn foreign_objects_are_refused() {
        let (p, e, s, r) = ids();
        let ours = json!({
            "Id": "aaaaaaaaaaaa",
            "Config": { "Labels": {
                "dev.ship.project": p.to_string(),
                "dev.ship.environment": e.to_string(),
                "dev.ship.service": s.to_string(),
                "dev.ship.release": r.to_string(),
            }},
        });
        let foreign = json!({
            "Id": "bbbbbbbbbbbb",
            "Config": { "Labels": { "com.docker.compose.project": "ship_x" } },
        });
        let doc = |items: Vec<Value>| Value::Array(items).to_string().into_bytes();
        assert!(
            check_ours(
                &doc(vec![ours.clone()]),
                e,
                "container",
                "/Config/Labels",
                true
            )
            .is_ok()
        );
        match check_ours(
            &doc(vec![ours.clone(), foreign]),
            e,
            "container",
            "/Config/Labels",
            true,
        ) {
            Err(DockerError::Foreign { kind, id }) => {
                assert_eq!(kind, "container");
                assert_eq!(id, "bbbbbbbbbbbb");
            }
            other => panic!("{other:?}"),
        }
        let other_env = EnvironmentId::from_parts(3, 3).unwrap();
        assert!(
            check_ours(
                &doc(vec![ours.clone()]),
                other_env,
                "container",
                "/Config/Labels",
                true
            )
            .is_err()
        );

        let network = json!({
            "Id": "cccccccccccc",
            "Labels": {
                "dev.ship.project": p.to_string(),
                "dev.ship.environment": e.to_string(),
                "dev.ship.service": "",
                "dev.ship.release": r.to_string(),
            },
        });
        assert!(check_ours(&doc(vec![network.clone()]), e, "network", "/Labels", false).is_ok());
        // A container must name its service.
        let mut shared_container = ours;
        shared_container["Config"]["Labels"]["dev.ship.service"] = json!("");
        assert!(
            check_ours(
                &doc(vec![shared_container]),
                e,
                "container",
                "/Config/Labels",
                true
            )
            .is_err()
        );
    }
}
