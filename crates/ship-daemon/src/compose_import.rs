//! Compose deployments (phase-1-ws-1.8): a revision whose build definition
//! is a Compose file runs as the environment's Compose project, with every
//! service the file defines, under the trust profile (ADR-0008).
//!
//! The same `deploy` operation as a Dockerfile ([`crate::deploy`]), with:
//!
//! - `validating` ([`check`]): `compose config` reads the file from the
//!   snapshot; its canonical model, which interpolation may have filled
//!   with credentials (rule 7), stays in a [`ComposeConfig`] and is never
//!   logged, stored or answered. [`ship_core::trust::evaluate`] judges it.
//!   Any reject item fails the operation `compose_rejected`; confirm items
//!   are listed with the file's fingerprint, and the operation goes on
//!   only when that fingerprint was confirmed before for the project, the
//!   request confirms it now, or the daemon's `compose.confirm` policy is
//!   `accept` (the project's setting over the host's). Each confirmation
//!   or acceptance is in the audit log with the items. Otherwise it fails
//!   `confirmation_required`. Either failure comes before any deployment
//!   is recorded, so nothing is built or started.
//! - [`steps`]: one service row per Compose service; a release directory
//!   with Ship's override (`labels.yaml`: the four labels, `read_only` for
//!   binds inside the snapshot, a per-release image tag for a built
//!   service the file names no image for) and `files.json` (the snapshot's
//!   Compose file, then the override); `building` the services that build;
//!   `preparing` (built images read, the declared host ports probed, the
//!   services' config values and secrets written to env files named by a
//!   last override, [`crate::config::prepare`], the release recorded); `deploying` (`up --remove-orphans`); `verifying`
//!   every service; then the routes of the declared published ports.
//!
//! Ship changes no port, volume, network or privilege the file declares
//! (rules 4 and 5); `publish.address` and `--port` do not apply. Nothing
//! here reads a policy from the snapshot or the file.

use std::collections::BTreeMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ship_core::api::COMPOSE_CONFIRM_KEY;
use ship_core::deploy::{PortMapping, Protocol};
use ship_core::trust::{self, ConfirmPolicy, Evaluation, SnapshotBind};
use ship_core::{
    DeploymentResult, ErrorCode, OperationPhase, ReleaseId, RouteProvider, RouteStatus,
};
use ship_core::{ServiceKind, ServiceStatus};
use ship_docker::argv::is_service_name;
use ship_docker::{
    ComposeConfig, ComposeFiles, ComposeProject, DeployLabels, DockerError, compose, inspect_image,
    labels_override,
};
use ship_store::{
    Deployment, NewAuditEvent, NewRelease, NewRoute, NewService, Service, SettingScope,
};

use crate::deploy::{
    Attempt, DeployJob, Deployed, Deployer, LABELS_FILE, LogForward, build_failure,
    create_release_dir, docker_failure, internal_docker, io_failure, literal, path_text, probe,
    release_dir, service_image_tag, up, up_failure_of, verify, write_file, write_manifest,
};
use crate::queue::{Context, JobFailure};
use crate::settings::OPERATOR;

/// The audit action of an operator's confirmation of a Compose file; the
/// target is the project id, the detail names the fingerprint and items.
pub const CONFIRM_ACTION: &str = "compose.confirm";
/// The audit action of confirm items accepted under `compose.confirm
/// accept`; the actor is [`POLICY_ACTOR`].
pub const ACCEPT_ACTION: &str = "compose.accept";
/// The audit actor of a decision taken by a setting.
pub const POLICY_ACTOR: &str = "policy";

/// A Compose file that passed the trust profile.
pub(crate) struct Checked {
    /// Relative to the snapshot.
    file: String,
    /// The file's directory in the snapshot: the project directory.
    directory: PathBuf,
    /// The file in the snapshot.
    path: PathBuf,
    /// Sensitive: never logged or stored.
    config: ComposeConfig,
    evaluation: Evaluation,
    fingerprint: String,
    services: Vec<String>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The fingerprint a confirmation is recorded under: the file's path, its
/// bytes and the confirm items as the profile states them, so the same
/// file is not asked about twice and any change asks again.
pub fn fingerprint(file: &str, bytes: &[u8], items: &[String]) -> String {
    let canonical = json!({ "file": file, "sha256": sha256_hex(bytes), "items": items });
    format!("sha256:{}", sha256_hex(canonical.to_string().as_bytes()))
}

fn config_failure(e: DockerError, file: &str) -> JobFailure {
    match e {
        DockerError::ConfigFailed { status, .. } => JobFailure::new(
            ErrorCode::InvalidRequest,
            format!(
                "docker compose config could not read {file} ({}); its message is not shown, \
                 since it may quote interpolated values: run `docker compose -f {file} config` \
                 where the file was written to see it",
                status
                    .code()
                    .map_or_else(|| "killed".to_owned(), |c| format!("exit {c}"))
            ),
        ),
        e => docker_failure(e, ErrorCode::DeployFailed, "reading the Compose file"),
    }
}

/// `validating` for a Compose file: reads it through `compose config`,
/// judges it by the trust profile and settles its confirm items. Nothing
/// is recorded or started when this fails.
pub(crate) async fn check(
    ctx: &Context,
    d: &Deployer,
    job: &DeployJob,
    snapshot: &Path,
    file: &str,
) -> Result<Checked, JobFailure> {
    let path = snapshot.join(file);
    let directory = path
        .parent()
        .map_or_else(|| snapshot.to_path_buf(), Path::to_path_buf);
    let project = ComposeProject::new(job.environment.id);
    ctx.log(&format!("reading {file} with docker compose config"))
        .await?;
    let files = ComposeFiles::new(&directory, [path.clone()]).map_err(internal_docker)?;
    let config = compose::config(d.docker(), &project, &files)
        .await
        .map_err(|e| config_failure(e, file))?;
    let services = config.service_names();
    if services.is_empty() {
        return Err(JobFailure::new(
            ErrorCode::InvalidRequest,
            format!("{file} defines no services"),
        ));
    }
    if let Some(bad) = services.iter().find(|s| !is_service_name(s)) {
        return Err(JobFailure::new(
            ErrorCode::InvalidRequest,
            format!("`{bad}` in {file} is not a service name Ship can use"),
        ));
    }
    let bytes = fs::read(&path)
        .map_err(|e| io_failure(&e, "the daemon could not read the Compose file"))?;
    let source = String::from_utf8_lossy(&bytes);
    let root = path_text(snapshot)?;
    let evaluation = trust::evaluate(config.expose(), &root, Some(&source));
    ctx.log(&format!(
        "{file} defines {} service{}: {}",
        services.len(),
        if services.len() == 1 { "" } else { "s" },
        services.join(", ")
    ))
    .await?;

    let rejected: Vec<String> = evaluation.rejected().map(ToString::to_string).collect();
    if !rejected.is_empty() {
        for item in &rejected {
            ctx.log(&format!("rejected: {item}")).await?;
        }
        return Err(JobFailure::new(
            ErrorCode::ComposeRejected,
            format!(
                "{file} is rejected by Ship's trust profile, which stops accidents and is no \
                 isolation: {}",
                rejected.join("; ")
            ),
        ));
    }
    for line in diff(&project, &config, &evaluation.snapshot_binds) {
        ctx.log(&format!("Ship changes: {line}")).await?;
    }

    let items: Vec<String> = evaluation
        .confirmations()
        .map(ToString::to_string)
        .collect();
    let fingerprint = fingerprint(file, &bytes, &items);
    if !items.is_empty() {
        settle_confirmation(ctx, d, job, file, &fingerprint, &items).await?;
    }
    Ok(Checked {
        file: file.to_owned(),
        directory,
        path,
        config,
        evaluation,
        fingerprint,
        services,
    })
}

/// Lets a file with confirm items through when the same fingerprint was
/// confirmed before for the project, `job.confirm` confirms it now, or the
/// policy in effect is `accept`; each new decision is audited.
async fn settle_confirmation(
    ctx: &Context,
    d: &Deployer,
    job: &DeployJob,
    file: &str,
    fingerprint: &str,
    items: &[String],
) -> Result<(), JobFailure> {
    for item in items {
        ctx.log(&format!("needs confirmation: {item}")).await?;
    }
    ctx.log(&format!("{file} fingerprint {fingerprint}"))
        .await?;
    let store = d.store();
    let project = job.project.id.to_string();
    let earlier = store.audit_events_for(CONFIRM_ACTION, &project).await?;
    if let Some(e) = earlier.iter().find(|e| {
        e.detail
            .as_deref()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .is_some_and(|v| v["fingerprint"] == fingerprint)
    }) {
        ctx.log(&format!(
            "the operator confirmed this file for the project at {}",
            e.ts
        ))
        .await?;
        return Ok(());
    }
    let mut detail = json!({
        "file": file,
        "fingerprint": fingerprint,
        "revision_id": job.revision.id.to_string(),
        "items": items,
    });
    if job.confirm {
        store
            .append_audit_event(&NewAuditEvent {
                actor: OPERATOR,
                action: CONFIRM_ACTION,
                target: &project,
                detail: Some(&detail.to_string()),
            })
            .await?;
        ctx.log(&format!(
            "the operator confirmed {} item{}; recorded in the audit log",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        ))
        .await?;
        return Ok(());
    }
    let policy = store
        .effective_setting(job.project.id, COMPOSE_CONFIRM_KEY)
        .await?;
    if let Some(setting) = policy.filter(|s| s.value.parse() == Ok(ConfirmPolicy::Accept)) {
        let scope = match setting.scope {
            SettingScope::Host => "host",
            SettingScope::Project(_) => "project",
        };
        detail["policy"] = json!({
            "key": COMPOSE_CONFIRM_KEY,
            "value": setting.value,
            "scope": scope,
            "set_by": setting.updated_by,
            "set_at": setting.updated_at.to_string(),
        });
        store
            .append_audit_event(&NewAuditEvent {
                actor: POLICY_ACTOR,
                action: ACCEPT_ACTION,
                target: &project,
                detail: Some(&detail.to_string()),
            })
            .await?;
        ctx.log(&format!(
            "accepted by policy: {COMPOSE_CONFIRM_KEY} is accept for the {scope}, set by {} at \
             {}; each item is recorded in the audit log",
            setting.updated_by, setting.updated_at
        ))
        .await?;
        return Ok(());
    }
    Err(JobFailure::new(
        ErrorCode::ConfirmationRequired,
        format!(
            "{file} needs the operator's confirmation of {} item{} (listed in this operation's \
             events): {}",
            items.len(),
            if items.len() == 1 { "" } else { "s" },
            items.join("; ")
        ),
    ))
}

/// What Ship adds to the file, for the operator (ADR-0008 rule 4). Names
/// and paths only.
fn diff(project: &ComposeProject, config: &ComposeConfig, binds: &[SnapshotBind]) -> Vec<String> {
    let model = config.expose();
    let mut out = vec![format!(
        "name: {} (the environment's project name)",
        project.name()
    )];
    for name in config.service_names() {
        let s = &model["services"][&name];
        out.push(format!(
            "services.{name}.labels: + dev.ship.project, dev.ship.environment, dev.ship.service, \
             dev.ship.release"
        ));
        if builds(s) && s.get("image").and_then(Value::as_str).is_none() {
            out.push(format!(
                "services.{name}.image: + {}-{name}:<release id> (the file names no image to \
                 build into)",
                project.name()
            ));
        }
    }
    for b in binds {
        out.push(format!(
            "services.{}.volumes[{}]: read_only: true (binds {} from the snapshot at {})",
            b.service, b.index, b.source, b.target
        ));
    }
    for section in ["networks", "volumes"] {
        if let Some(defs) = model.get(section).and_then(Value::as_object) {
            for (name, def) in defs {
                if !is_external(def) {
                    out.push(format!("{section}.{name}.labels: + Ship's labels"));
                }
            }
        }
    }
    out
}

fn builds(service: &Value) -> bool {
    service.get("build").is_some_and(|b| !b.is_null())
}

fn is_external(def: &Value) -> bool {
    def.get("external")
        .is_some_and(|x| x.as_bool() == Some(true) || x.is_object())
}

/// A host port the file publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Published {
    service: String,
    endpoint: SocketAddr,
    mapping: PortMapping,
}

/// The single host ports the file publishes, each on its declared
/// `host_ip` (none: every address). A range or an ephemeral port has no
/// address to record.
fn published(config: &ComposeConfig) -> (Vec<Published>, Vec<String>) {
    let mut out = Vec::new();
    let mut unrecorded = Vec::new();
    let model = config.expose();
    for name in config.service_names() {
        let ports = model["services"][&name]["ports"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for p in ports {
            let protocol = p["protocol"]
                .as_str()
                .unwrap_or("tcp")
                .parse::<Protocol>()
                .ok();
            let host = p["published"].as_str().and_then(|s| s.parse::<u16>().ok());
            let container = p["target"].as_u64().and_then(|t| u16::try_from(t).ok());
            let ip = match p["host_ip"].as_str().filter(|s| !s.is_empty()) {
                None => Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                Some(text) => text.parse::<IpAddr>().ok(),
            };
            match (host, container, ip, protocol) {
                (Some(host), Some(container), Some(ip), Some(protocol)) if host > 0 => {
                    out.push(Published {
                        service: name.clone(),
                        endpoint: SocketAddr::new(ip, host),
                        mapping: PortMapping {
                            host,
                            container,
                            protocol,
                        },
                    });
                }
                _ => unrecorded.push(name.clone()),
            }
        }
    }
    (out, unrecorded)
}

/// Ship's override: the labels from [`labels_override`], `read_only` for
/// the snapshot's binds (every other field of the mount as the file has
/// it) and the image tags of `tags`. Names, paths and ids only.
fn ship_override(
    config: &ComposeConfig,
    labels: &DeployLabels,
    binds: &[SnapshotBind],
    tags: &BTreeMap<String, String>,
) -> Result<String, JobFailure> {
    let text = labels_override(config, labels).map_err(internal_docker)?;
    let mut doc: Value = serde_json::from_str(&text)
        .map_err(|_| JobFailure::new(ErrorCode::Internal, "the labels override is not JSON"))?;
    for (service, tag) in tags {
        doc["services"][service]["image"] = json!(tag);
    }
    let mut mounts: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for b in binds {
        let mut m = json!({
            "type": "bind",
            "source": literal(&b.source),
            "target": literal(&b.target),
            "read_only": true,
        });
        if let Some(options) = &b.bind {
            m["bind"] = options.clone();
        }
        mounts.entry(b.service.as_str()).or_default().push(m);
    }
    for (service, list) in mounts {
        doc["services"][service]["volumes"] = Value::Array(list);
    }
    serde_json::to_string_pretty(&doc)
        .map_err(|_| JobFailure::new(ErrorCode::Internal, "could not write the override"))
}

/// `sha256:<hex>` of the release's effective non-secret configuration
/// (ADR-0007 rule 3): the file's fingerprint, its services and what Ship
/// added, with the services' non-secret config.
fn release_fingerprint(
    checked: &Checked,
    tagged: &BTreeMap<String, String>,
    config: &Value,
) -> String {
    let read_only: Vec<Value> = checked
        .evaluation
        .snapshot_binds
        .iter()
        .map(|b| json!([b.service, b.target]))
        .collect();
    let canonical = json!({
        "kind": "compose",
        "file": checked.file,
        "fingerprint": checked.fingerprint,
        "services": checked.services,
        "read_only": read_only,
        "tagged": tagged.keys().collect::<Vec<_>>(),
        "config": config,
    });
    format!("sha256:{}", sha256_hex(canonical.to_string().as_bytes()))
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// The steps of a Compose deployment after it is recorded.
pub(crate) async fn steps(
    ctx: &Context,
    d: &Deployer,
    job: &DeployJob,
    deployment: &Deployment,
    first_release: Option<ReleaseId>,
    checked: Checked,
    attempt: &mut Attempt,
) -> Result<Deployed, JobFailure> {
    let env = &job.environment;
    let store = d.store();
    let mut rows: Vec<Service> = Vec::with_capacity(checked.services.len());
    for name in &checked.services {
        rows.push(
            store
                .ensure_service(
                    env.id,
                    &NewService {
                        name: name.clone(),
                        kind: ServiceKind::Compose,
                        port: None,
                    },
                )
                .await?,
        );
    }
    let release_id = store.allocate_release_id()?;
    attempt.release = Some(release_id);
    let dir = release_dir(d.state_dir(), release_id);
    create_release_dir(d.state_dir(), &dir, ctx.operation_id())?;
    let labels = DeployLabels {
        project: job.project.id,
        environment: env.id,
        release: release_id,
        services: rows.iter().map(|s| (s.name.clone(), s.id)).collect(),
        shared_release: first_release.unwrap_or(release_id),
    };
    let model = checked.config.expose();
    let building: Vec<String> = checked
        .services
        .iter()
        .filter(|s| builds(&model["services"][s.as_str()]))
        .cloned()
        .collect();
    let tags: BTreeMap<String, String> = building
        .iter()
        .filter(|s| model["services"][s.as_str()]["image"].as_str().is_none())
        .map(|s| (s.clone(), service_image_tag(env.id, s, release_id)))
        .collect();
    let override_text = ship_override(
        &checked.config,
        &labels,
        &checked.evaluation.snapshot_binds,
        &tags,
    )?;
    let labels_path = dir.join(LABELS_FILE);
    write_file(&labels_path, override_text.as_bytes())?;
    let files = ComposeFiles::new(&checked.directory, [checked.path.clone(), labels_path])
        .map_err(internal_docker)?;
    write_manifest(&dir, &files)?;

    // building
    if building.is_empty() {
        ctx.enter(
            OperationPhase::Building,
            "nothing to build: every service runs an image",
        )
        .await?;
    } else {
        ctx.enter(
            OperationPhase::Building,
            &format!("building {}", building.join(", ")),
        )
        .await?;
        let forward = LogForward::start(ctx);
        let built = ctx
            .build(async {
                let mut sink = forward.sink();
                compose::build(
                    d.docker(),
                    &attempt.project,
                    &files,
                    &building,
                    d.build_timeout(),
                    &mut sink,
                )
                .await
                .map_err(build_failure)
            })
            .await;
        forward.finish().await?;
        built?;
    }

    // preparing
    ctx.enter(OperationPhase::Preparing, "reading the images")
        .await?;
    let mut images = Vec::with_capacity(checked.services.len());
    for name in &checked.services {
        let declared = model["services"][name.as_str()]["image"].as_str();
        if building.contains(name) {
            let reference = tags
                .get(name)
                .map_or_else(|| declared.unwrap_or_default().to_owned(), Clone::clone);
            let info = inspect_image(d.docker(), &reference).await.map_err(|e| {
                docker_failure(
                    e,
                    ErrorCode::DeployFailed,
                    &format!("reading the image of {name}"),
                )
            })?;
            ctx.log(&format!("{name}: built image {}", info.id)).await?;
            images.push(info.id);
        } else if let Some(reference) = declared {
            images.push(reference.to_owned());
        }
    }
    let (ports, unrecorded) = published(&checked.config);
    let current = store.list_routes(env.id).await?;
    for p in &ports {
        let address = p.endpoint.to_string();
        if !current.iter().any(|r| r.address == address) {
            probe(p.endpoint, p.mapping)?;
        }
        ctx.log(&format!(
            "{} publishes port {}/{} on {address}, as the file declares",
            p.service, p.mapping.container, p.mapping.protocol
        ))
        .await?;
    }
    for name in unrecorded {
        ctx.log(&format!(
            "{name} publishes a port range or an ephemeral port; no endpoint is recorded for it"
        ))
        .await?;
    }
    // Config values and secrets: their own env files and override, added
    // last, after the build and the trust profile read the file.
    let config = crate::config::prepare(ctx, d, env, &rows, &dir).await?;
    for (name, names) in &config.names {
        let Some(declared) = model["services"][name.as_str()]["environment"].as_object() else {
            continue;
        };
        let shadowed: Vec<&str> = names
            .iter()
            .filter(|n| declared.contains_key(n.as_str()))
            .map(String::as_str)
            .collect();
        if !shadowed.is_empty() {
            ctx.log(&format!(
                "{name}: the Compose file also sets {}; an entry under the service's own \
                 environment: takes precedence over Ship's value (Compose's rule)",
                shadowed.join(", ")
            ))
            .await?;
        }
    }
    let files = config.files(files)?;
    write_manifest(&dir, &files)?;
    let release = store
        .create_release_as(
            release_id,
            job.revision.id,
            &NewRelease {
                images: &images,
                config_fingerprint: &release_fingerprint(&checked, &tags, &config.fingerprint),
            },
        )
        .await?;
    store
        .set_deployment_release(deployment.id, release.id)
        .await?;
    ctx.log(&format!("release {}", release.id)).await?;

    // deploying
    ctx.enter(
        OperationPhase::Deploying,
        &format!("starting {}", plural(rows.len(), "service")),
    )
    .await?;
    attempt.started = true;
    let first_endpoint = ports.first().map(|p| p.endpoint);
    up(ctx, d, &attempt.project, &files)
        .await
        .map_err(|e| up_failure_of(e, first_endpoint, "starting the services failed"))?;

    // verifying
    ctx.enter(
        OperationPhase::Verifying,
        &format!("verifying {}", plural(rows.len(), "service")),
    )
    .await?;
    let statuses = verify(d, env.id, &rows, release_id).await?;
    for (row, status) in rows.iter().zip(&statuses) {
        let line = match status {
            ServiceStatus::Healthy => format!("{} is healthy", row.name),
            _ => format!(
                "{} is running with no health check, so it is {status}: running, not verified",
                row.name
            ),
        };
        ctx.log(&line).await?;
    }

    let routes: Vec<NewRoute> = ports
        .iter()
        .filter_map(|p| {
            let row = rows.iter().find(|r| r.name == p.service)?;
            Some(NewRoute {
                service_id: row.id,
                provider: RouteProvider::Port,
                address: p.endpoint.to_string(),
                status: RouteStatus::Confirmed,
            })
        })
        .collect();
    let before = store.list_routes(env.id).await?;
    store.replace_routes(env.id, &routes).await?;
    attempt.routes_before = Some(before);
    if ports.is_empty() {
        ctx.log("Endpoint: not configured (the Compose file publishes no port)")
            .await?;
    }
    for p in &ports {
        ctx.log(&format!("Endpoint: {} ({})", p.endpoint, p.service))
            .await?;
    }
    let deployment = store
        .finish_deployment(deployment.id, DeploymentResult::Succeeded)
        .await?;
    let status = statuses
        .iter()
        .copied()
        .find(|s| *s != ServiceStatus::Healthy)
        .unwrap_or(ServiceStatus::Healthy);
    Ok(Deployed {
        deployment,
        release,
        status,
        endpoint: first_endpoint.map(|e| e.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ship_core::{EnvironmentId, ProjectId, ServiceId};

    fn config(v: Value) -> ComposeConfig {
        ComposeConfig::parse(v.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn fingerprints_follow_the_file_and_its_items() {
        let items = vec!["services.web.ports[0]: publishes 0.0.0.0:80 to 80/tcp".to_owned()];
        let a = fingerprint("compose.yaml", b"a", &items);
        assert!(a.starts_with("sha256:") && a.len() == 7 + 64, "{a}");
        assert_eq!(a, fingerprint("compose.yaml", b"a", &items));
        assert_ne!(a, fingerprint("compose.yaml", b"b", &items));
        assert_ne!(a, fingerprint("other.yaml", b"a", &items));
        assert_ne!(a, fingerprint("compose.yaml", b"a", &[]));
    }

    #[test]
    fn published_ports_keep_their_declared_addresses() {
        let c = config(json!({ "services": {
            "web": { "ports": [
                { "target": 80, "published": "8080", "protocol": "tcp" },
                { "target": 53, "published": "5353", "protocol": "udp", "host_ip": "127.0.0.1" },
                { "target": 90, "published": "9000-9001", "protocol": "tcp" },
                { "target": 91, "protocol": "tcp" },
            ]},
            "db": { "image": "postgres" },
        }}));
        let (ports, unrecorded) = published(&c);
        let got: Vec<(String, String)> = ports
            .iter()
            .map(|p| (p.service.clone(), p.endpoint.to_string()))
            .collect();
        assert_eq!(
            got,
            [
                ("web".to_owned(), "0.0.0.0:8080".to_owned()),
                ("web".to_owned(), "127.0.0.1:5353".to_owned()),
            ]
        );
        assert_eq!(ports[1].mapping.protocol, Protocol::Udp);
        assert_eq!(unrecorded, ["web", "web"]);
    }

    #[test]
    fn the_override_adds_labels_read_only_and_tags_only() {
        let c = config(json!({
            "services": {
                "web": {
                    "build": { "context": "/snap" },
                    "ports": [{ "target": 80, "published": "8080" }],
                    "privileged": false,
                    "volumes": [
                        { "type": "bind", "source": "/snap/conf", "target": "/conf" },
                        { "type": "volume", "source": "data", "target": "/data" },
                    ],
                    "environment": { "PASSWORD": "canary-in-the-model" },
                },
                "db": { "image": "postgres:16" },
            },
            "volumes": { "data": {} },
        }));
        let e = EnvironmentId::from_parts(1, 2).unwrap();
        let r = ReleaseId::from_parts(1, 3).unwrap();
        let labels = DeployLabels {
            project: ProjectId::from_parts(1, 1).unwrap(),
            environment: e,
            release: r,
            services: [
                ("web".to_owned(), ServiceId::from_parts(1, 4).unwrap()),
                ("db".to_owned(), ServiceId::from_parts(1, 5).unwrap()),
            ]
            .into(),
            shared_release: r,
        };
        let binds = [SnapshotBind {
            service: "web".to_owned(),
            index: 0,
            source: "/snap/conf$x".to_owned(),
            target: "/conf".to_owned(),
            bind: Some(json!({ "create_host_path": true })),
        }];
        let tags: BTreeMap<String, String> =
            [("web".to_owned(), service_image_tag(e, "web", r))].into();
        let text = ship_override(&c, &labels, &binds, &tags).unwrap();
        assert!(!text.contains("canary-in-the-model"), "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        let web = v["services"]["web"].as_object().unwrap();
        let keys: Vec<&str> = web.keys().map(String::as_str).collect();
        assert_eq!(keys, ["build", "image", "labels", "volumes"]);
        assert_eq!(web["image"], format!("ship_{e}-web:{r}"));
        assert_eq!(
            web["volumes"],
            json!([{ "type": "bind", "source": "/snap/conf$$x", "target": "/conf",
                     "read_only": true, "bind": { "create_host_path": true } }])
        );
        let db = v["services"]["db"].as_object().unwrap();
        assert_eq!(db.keys().collect::<Vec<_>>(), ["labels"]);
        assert_eq!(
            v["volumes"]["data"]["labels"]["dev.ship.release"],
            r.to_string()
        );
        assert!(v.get("networks").is_none());
        assert!(v.get("ports").is_none());
    }

    #[test]
    fn the_diff_names_what_ship_adds() {
        let c = config(json!({
            "services": {
                "web": { "build": { "context": "/snap" } },
                "api": { "build": { "context": "/snap" }, "image": "me/api:1" },
            },
            "networks": { "default": {}, "outside": { "external": true } },
        }));
        let project = ComposeProject::new(EnvironmentId::from_parts(1, 2).unwrap());
        let lines = diff(&project, &c, &[]);
        assert!(lines[0].starts_with(&format!("name: {}", project.name())));
        assert!(
            lines.iter().any(|l| l.starts_with("services.web.image: +")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("services.api.image")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("networks.default.labels"))
        );
        assert!(!lines.iter().any(|l| l.contains("outside")));
    }
}
