//! Dockerfile deployments (phase-1-ws-1.7): a revision with a Dockerfile
//! becomes a release running as a single-service Compose project. A
//! revision whose build definition is a Compose file takes the same
//! operation through [`crate::compose_import`] (phase-1-ws-1.8). Every
//! release's directory names the files that define it ([`FILES_FILE`]), so
//! a failure brings the previous release back the same way for both.
//!
//! One `deploy` operation, serialised on its environment (ADR-0011), moves
//! through:
//!
//! - `validating`: the build definition is chosen ([`deploy::detect`]);
//!   the deployment is recorded with the release serving now as its
//!   previous release (ADR-0007).
//! - `building`: a Compose file Ship generates, outside the snapshot, under
//!   `<state>/releases/<release id>/`, with Ship's labels override, is
//!   built in one of the host's build slots; every line goes to the
//!   operation's log.
//! - `preparing`: the built image is read back (id, `EXPOSE`, `VOLUME`);
//!   `--port` is mapped by the `EXPOSE` rule; the publish address is
//!   resolved (`publish.address`, `auto` asking the host's tailnet) and
//!   probed for a conflict; the final file is written and the release
//!   recorded.
//! - `deploying`: `compose up` replaces the environment's containers.
//! - `verifying`: the new release's container must pass its health check
//!   (`healthy`) or, with none, still run unrestarted after the settle
//!   period (`running_unverified`, ADR-0006 rule 2).
//!
//! Then the environment's routes are replaced and the deployment
//! succeeds. A failure before `deploying` touched nothing; one after it
//! brings the previous release's own generated files up again, so its
//! containers are recreated with their volumes (Human, 2026-11-27), or,
//! with no previous release, stops the new container and keeps it as
//! evidence. Either way the deployment is `failed` and the operation's
//! failure names the phase (ADR-0011 rule 7).

use std::fs;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use ship_core::api::PUBLISH_ADDRESS_KEY;
use ship_core::deploy::{self, BuildKind, DetectError, PortMapping, PortSpec, PublishAddress};
use ship_core::{
    DeploymentResult, DesiredState, EnvironmentId, ErrorCode, FailureDetail, OperationPhase,
    ReleaseId, RouteProvider, RouteStatus, ServiceKind, ServiceStatus, compose_project_name,
};
use ship_docker::{
    ComposeFiles, ComposeProject, DeployLabels, DockerError, Line, Runner, Selector, UpOptions,
    compose, containers, inspect_image, labels_override,
};
use ship_net::TailnetAddress;
use ship_store::{
    Deployment, Environment, NewDeployment, NewRelease, NewRoute, NewService, Project, Release,
    Revision, Route, Service, Store,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::compose_import;
use crate::queue::{Context, JobFailure};
use crate::upload::snapshot_dir;

/// Directory under the state directory holding one directory of generated
/// files per release.
pub const RELEASES_DIR: &str = "releases";
/// The generated Compose file of a release (JSON, which Compose reads as
/// YAML).
pub const COMPOSE_FILE: &str = "compose.yaml";
/// The labels override of a release.
pub const LABELS_FILE: &str = "labels.yaml";
/// The one service of a Dockerfile project.
pub const SERVICE: &str = "web";
/// Docker's restart policy for the service: restarted after a crash or a
/// reboot, but never after `compose stop` (ADR-0006 rule 6).
pub const RESTART_POLICY: &str = "unless-stopped";

/// The generated files of `release`.
pub fn release_dir(state_dir: &Path, release: ReleaseId) -> PathBuf {
    state_dir.join(RELEASES_DIR).join(release.to_string())
}

/// Which files define a release: its project directory and its Compose
/// files in order (JSON, `{ "directory": …, "files": [ … ] }`). Holds paths
/// only. A release from before it was written (phase-1-ws-1.7) is
/// [`COMPOSE_FILE`] and [`LABELS_FILE`] in its own directory.
pub const FILES_FILE: &str = "files.json";

/// The tag a release's image is built under: `ship_<env id>-web:<release id>`.
pub fn image_tag(environment: EnvironmentId, release: ReleaseId) -> String {
    service_image_tag(environment, SERVICE, release)
}

/// The tag Ship builds `service`'s image under for `release`, when the
/// Compose file names none: `ship_<env id>-<service>:<release id>`.
pub fn service_image_tag(environment: EnvironmentId, service: &str, release: ReleaseId) -> String {
    format!("{}-{service}:{release}", compose_project_name(&environment))
}

/// The files that define `release`, from its [`FILES_FILE`].
pub fn release_files(state_dir: &Path, release: ReleaseId) -> Result<ComposeFiles, DockerError> {
    let dir = release_dir(state_dir, release);
    let manifest = match fs::read(dir.join(FILES_FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return ComposeFiles::new(&dir, [dir.join(COMPOSE_FILE), dir.join(LABELS_FILE)]);
        }
        Err(e) => {
            return Err(DockerError::InvalidArgument(format!(
                "the files of release {release} cannot be read: {e}"
            )));
        }
    };
    let bad = || {
        DockerError::InvalidArgument(format!(
            "the {FILES_FILE} of release {release} is malformed"
        ))
    };
    let v: Value = serde_json::from_slice(&manifest).map_err(|_| bad())?;
    let directory = v["directory"].as_str().ok_or_else(bad)?;
    let files: Vec<PathBuf> = v["files"]
        .as_array()
        .ok_or_else(bad)?
        .iter()
        .map(|f| f.as_str().map(PathBuf::from).ok_or_else(bad))
        .collect::<Result<_, _>>()?;
    ComposeFiles::new(directory, files)
}

/// Writes `files` as the [`FILES_FILE`] of the release in `dir`.
pub(crate) fn write_manifest(dir: &Path, files: &ComposeFiles) -> Result<(), JobFailure> {
    let text = |p: &Path| path_text(p).map(Value::String);
    let list: Vec<Value> = files
        .files()
        .iter()
        .map(|f| text(f))
        .collect::<Result<_, _>>()?;
    let manifest = json!({ "directory": text(files.directory())?, "files": list });
    write_file(&dir.join(FILES_FILE), manifest.to_string().as_bytes())
}

/// How long the steps of a deployment may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeploySettings {
    /// A service with no health check must still be running, unrestarted,
    /// this long after `up` to be `running_unverified`.
    pub settle: Duration,
    /// Longest the `verifying` phase waits for a verdict.
    pub verify_timeout: Duration,
    /// Longest `compose config`, `up` or `stop` may run.
    pub step_timeout: Duration,
    /// How often `verifying` looks at the container.
    pub poll: Duration,
}

impl Default for DeploySettings {
    fn default() -> Self {
        DeploySettings {
            settle: Duration::from_secs(5),
            verify_timeout: Duration::from_secs(120),
            step_timeout: Duration::from_secs(300),
            poll: Duration::from_secs(1),
        }
    }
}

/// What deployments need from the daemon.
#[derive(Debug, Clone)]
pub struct Deployer {
    store: Store,
    docker: Runner,
    state_dir: PathBuf,
    settings: DeploySettings,
    build_timeout: Duration,
    tailnet: Arc<dyn TailnetAddress>,
}

impl Deployer {
    pub fn new(
        store: Store,
        docker: Runner,
        state_dir: &Path,
        settings: DeploySettings,
        build_timeout: Duration,
        tailnet: Arc<dyn TailnetAddress>,
    ) -> Deployer {
        Deployer {
            store,
            docker,
            state_dir: state_dir.to_path_buf(),
            settings,
            build_timeout,
            tailnet,
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub(crate) fn docker(&self) -> &Runner {
        &self.docker
    }

    pub(crate) fn build_timeout(&self) -> Duration {
        self.build_timeout
    }

    /// Where a published port binds now: the `publish.address` setting,
    /// `auto` resolved through the host's tailnet.
    pub async fn publish_ip(&self) -> Result<(IpAddr, String), JobFailure> {
        let setting = self.store.host_setting(PUBLISH_ADDRESS_KEY).await?;
        let value = match &setting {
            Some(s) => s.value.parse::<PublishAddress>().map_err(|_| {
                JobFailure::new(ErrorCode::Internal, "the stored publish.address is invalid")
            })?,
            None => PublishAddress::Auto,
        };
        Ok(match value {
            PublishAddress::Fixed(ip) => (ip, format!("publish.address is {ip}")),
            PublishAddress::Auto => match self.tailnet.ipv4().await {
                Some(v4) => (
                    IpAddr::V4(v4),
                    format!("publish.address is auto: the host's tailnet address {v4}"),
                ),
                None => (
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    "publish.address is auto: the host has no tailnet, so 127.0.0.1".to_owned(),
                ),
            },
        })
    }
}

/// One deployment to run.
#[derive(Debug, Clone)]
pub struct DeployJob {
    pub project: Project,
    /// The project's environment being deployed to.
    pub environment: Environment,
    /// A revision of the project.
    pub revision: Revision,
    /// `--file`: relative to the revision's root.
    pub file: Option<String>,
    /// `--port`.
    pub port: Option<PortSpec>,
    /// The operator confirmed the Compose file's confirm items.
    pub confirm: bool,
}

/// A deployment that succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployed {
    pub deployment: Deployment,
    pub release: Release,
    pub status: ServiceStatus,
    /// The address recorded on the route, when a port is published.
    pub endpoint: Option<String>,
}

/// Runs `job` inside the operation `ctx`. Public so an operation that
/// carries an upload before it (`ship push`) runs the same steps.
pub async fn run(ctx: &Context, d: &Deployer, job: &DeployJob) -> Result<Deployed, JobFailure> {
    let env = &job.environment;
    ctx.enter(
        OperationPhase::Validating,
        &format!(
            "checking revision {} for a build definition",
            job.revision.id
        ),
    )
    .await?;
    let snapshot = snapshot_dir(&d.state_dir, &job.revision.source_ref);
    if !snapshot.is_dir() {
        tracing::error!(revision = %job.revision.id, "the snapshot of a revision is missing");
        return Err(JobFailure::new(
            ErrorCode::Internal,
            format!("the snapshot of revision {} is missing", job.revision.id),
        ));
    }
    let detected =
        deploy::detect(job.file.as_deref(), |p| is_file(&snapshot, p)).map_err(detect_failure)?;
    if let Some(note) = &detected.note {
        ctx.log(note).await?;
    }
    // A Compose file is read and put through the trust profile before
    // anything is recorded: a rejected or unconfirmed file leaves no
    // deployment behind (ADR-0008 rules 2 and 3).
    let compose = match detected.kind {
        BuildKind::Compose => {
            if job.port.is_some() {
                return Err(JobFailure::new(
                    ErrorCode::InvalidRequest,
                    format!(
                        "--port applies to a Dockerfile; {} publishes the ports it declares",
                        detected.path
                    ),
                ));
            }
            Some(compose_import::check(ctx, d, job, &snapshot, &detected.path).await?)
        }
        BuildKind::Dockerfile => {
            ctx.log(&format!("deploying {} as service {SERVICE}", detected.path))
                .await?;
            None
        }
    };

    let history = d.store.list_deployments(env.id).await?;
    // Newest first: the release serving now, and the oldest release, which
    // created the environment's networks and volumes.
    let previous = history
        .iter()
        .find(|x| x.result == Some(DeploymentResult::Succeeded))
        .and_then(|x| x.release_id);
    let first_release = history.iter().rev().find_map(|x| x.release_id);
    let deployment = d
        .store
        .create_deployment(&NewDeployment {
            environment_id: env.id,
            revision_id: job.revision.id,
            operation_id: ctx.operation_id(),
            previous_release_id: previous,
        })
        .await?;

    let mut attempt = Attempt {
        environment: env.id,
        project: ComposeProject::new(env.id),
        previous,
        started: false,
        routes_before: None,
    };
    let outcome = match compose {
        Some(checked) => {
            compose_import::steps(
                ctx,
                d,
                job,
                &deployment,
                first_release,
                checked,
                &mut attempt,
            )
            .await
        }
        None => {
            let steps = Steps {
                ctx,
                d,
                job,
                snapshot,
                dockerfile: detected.path,
                deployment: &deployment,
                first_release,
            };
            steps.run(&mut attempt).await
        }
    };
    match outcome {
        Ok(deployed) => Ok(deployed),
        Err(failure) => {
            attempt.undo(ctx, d).await;
            if let Err(e) = d
                .store
                .finish_deployment(deployment.id, DeploymentResult::Failed)
                .await
            {
                tracing::error!(error = %e, deployment = %deployment.id, "could not record a failed deployment");
            }
            Err(failure)
        }
    }
}

/// Whether `rel` names a regular file inside `root`. Snapshots hold no
/// symlinks (ADR-0005 rule 5); a link would not count anyway.
fn is_file(root: &Path, rel: &str) -> bool {
    fs::symlink_metadata(root.join(rel)).is_ok_and(|m| m.is_file())
}

fn detect_failure(e: DetectError) -> JobFailure {
    let code = match e {
        DetectError::NoDefinition => ErrorCode::NoBuildDefinition,
        DetectError::InvalidPath(_) | DetectError::Missing(_) => ErrorCode::InvalidRequest,
    };
    JobFailure::new(code, e.to_string())
}

/// What a failure must undo.
pub(crate) struct Attempt {
    pub(crate) environment: EnvironmentId,
    pub(crate) project: ComposeProject,
    pub(crate) previous: Option<ReleaseId>,
    /// `compose up` has run for the new release.
    pub(crate) started: bool,
    /// The environment's routes before this attempt replaced them; `None`
    /// until it did.
    pub(crate) routes_before: Option<Vec<Route>>,
}

impl Attempt {
    /// After a failure: the environment's routes put back if this attempt
    /// replaced them; nothing more if the new release never started; else
    /// the previous release brought up again from its own files, or, with
    /// none, the new container stopped and kept. Problems are logged; the
    /// original failure stands.
    async fn undo(&self, ctx: &Context, d: &Deployer) {
        if let Some(before) = &self.routes_before {
            let routes: Vec<NewRoute> = before
                .iter()
                .map(|r| NewRoute {
                    service_id: r.service_id,
                    provider: r.provider,
                    address: r.address.clone(),
                    status: r.status,
                })
                .collect();
            if let Err(e) = d.store.replace_routes(self.environment, &routes).await {
                tracing::error!(error = %e, environment = %self.environment, "could not put the previous routes back");
                let _ = ctx.log("putting the previous routes back failed").await;
            }
        }
        if !self.started {
            return;
        }
        let outcome = match self.previous {
            Some(previous) => {
                let _ = ctx
                    .log(&format!("bringing release {previous} up again"))
                    .await;
                match release_files(&d.state_dir, previous) {
                    Ok(files) => up(ctx, d, &self.project, &files)
                        .await
                        .map(|()| format!("release {previous} is running again")),
                    Err(e) => Err(e),
                }
            }
            None => {
                let _ = ctx
                    .log(
                        "there is no earlier release; stopping the new containers and keeping \
                         them for inspection",
                    )
                    .await;
                stop(ctx, d, &self.project)
                    .await
                    .map(|()| "the new containers are stopped".to_owned())
            }
        };
        let line = match outcome {
            Ok(done) => done,
            Err(e) => format!("putting the previous state back failed: {e}"),
        };
        let _ = ctx.log(&line).await;
    }
}

/// The steps after the deployment is recorded.
struct Steps<'a> {
    ctx: &'a Context,
    d: &'a Deployer,
    job: &'a DeployJob,
    snapshot: PathBuf,
    dockerfile: String,
    deployment: &'a Deployment,
    first_release: Option<ReleaseId>,
}

impl Steps<'_> {
    async fn run(&self, attempt: &mut Attempt) -> Result<Deployed, JobFailure> {
        let (ctx, d, env) = (self.ctx, self.d, &self.job.environment);
        let service = d
            .store
            .ensure_service(
                env.id,
                &NewService {
                    name: SERVICE.to_owned(),
                    kind: ServiceKind::Web,
                    port: None,
                },
            )
            .await?;
        let release_id = d.store.allocate_release_id()?;
        let dir = release_dir(&d.state_dir, release_id);
        create_release_dir(&d.state_dir, &dir)?;
        let labels = DeployLabels {
            project: self.job.project.id,
            environment: env.id,
            release: release_id,
            services: [(SERVICE.to_owned(), service.id)].into(),
            shared_release: self.first_release.unwrap_or(release_id),
        };
        let mut shape = Shape {
            context: path_text(&self.snapshot)?,
            dockerfile: self.dockerfile.clone(),
            image: image_tag(env.id, release_id),
            publish: None,
            volumes: Vec::new(),
        };

        // building
        ctx.enter(OperationPhase::Building, &format!("building {SERVICE}"))
            .await?;
        let files = write_files(d, &attempt.project, &dir, &shape, &labels).await?;
        let forward = LogForward::start(ctx);
        let built = ctx
            .build(async {
                let mut sink = forward.sink();
                compose::build(
                    &d.docker,
                    &attempt.project,
                    &files,
                    &[SERVICE],
                    d.build_timeout,
                    &mut sink,
                )
                .await
                .map_err(build_failure)
            })
            .await;
        forward.finish().await?;
        built?;

        // preparing
        ctx.enter(OperationPhase::Preparing, "reading the built image")
            .await?;
        let image = inspect_image(&d.docker, &shape.image)
            .await
            .map_err(|e| docker_failure(e, ErrorCode::DeployFailed, "reading the built image"))?;
        if let Some(spec) = self.job.port {
            let mapping = deploy::map_port(spec, &image.exposed)
                .map_err(|e| JobFailure::new(ErrorCode::InvalidRequest, e.to_string()))?;
            let (ip, why) = d.publish_ip().await?;
            ctx.log(&why).await?;
            let endpoint = SocketAddr::new(ip, mapping.host);
            let routes = d.store.list_routes(env.id).await?;
            let ours = routes.iter().any(|r| r.address == endpoint.to_string());
            if !ours {
                probe(endpoint, mapping)?;
            }
            ctx.log(&format!(
                "publishing {SERVICE} port {}/{} on {endpoint}",
                mapping.container, mapping.protocol
            ))
            .await?;
            shape.publish = Some((ip, mapping));
        }
        shape.volumes = volume_mounts(&image.volumes);
        for m in &shape.volumes {
            ctx.log(&format!(
                "VOLUME {} is kept in the named volume {}_{}",
                m.path,
                attempt.project.name(),
                m.name
            ))
            .await?;
        }
        let files = write_files(d, &attempt.project, &dir, &shape, &labels).await?;
        write_manifest(&dir, &files)?;
        let release = d
            .store
            .create_release_as(
                release_id,
                self.job.revision.id,
                &NewRelease {
                    images: std::slice::from_ref(&image.id),
                    config_fingerprint: &shape.fingerprint(),
                },
            )
            .await?;
        d.store
            .set_deployment_release(self.deployment.id, release.id)
            .await?;
        ctx.log(&format!("release {} (image {})", release.id, image.id))
            .await?;

        // deploying
        ctx.enter(OperationPhase::Deploying, "starting 1 service")
            .await?;
        attempt.started = true;
        up(ctx, d, &attempt.project, &files)
            .await
            .map_err(|e| up_failure(e, shape.endpoint()))?;

        // verifying
        ctx.enter(OperationPhase::Verifying, &format!("verifying {SERVICE}"))
            .await?;
        let status = verify(d, env.id, std::slice::from_ref(&service), release_id)
            .await?
            .first()
            .copied()
            .unwrap_or(ServiceStatus::Unknown);
        let verdict = match status {
            ServiceStatus::Healthy => format!("{SERVICE} is healthy"),
            _ => format!(
                "{SERVICE} is running with no health check, so it is {status}: running, not verified"
            ),
        };
        ctx.log(&verdict).await?;

        let endpoint = shape.endpoint().map(|e| e.to_string());
        let routes: Vec<NewRoute> = endpoint
            .iter()
            .map(|address| NewRoute {
                service_id: service.id,
                provider: RouteProvider::Port,
                address: address.clone(),
                status: RouteStatus::Confirmed,
            })
            .collect();
        // Replaced before the deployment succeeds; a failure from here on
        // (the log, the result) puts the previous routes back in `undo`.
        let before = d.store.list_routes(env.id).await?;
        d.store.replace_routes(env.id, &routes).await?;
        attempt.routes_before = Some(before);
        let line = match &endpoint {
            Some(address) => format!("Endpoint: {address}"),
            None => "Endpoint: not configured (no --port)".to_owned(),
        };
        ctx.log(&line).await?;
        let deployment = d
            .store
            .finish_deployment(self.deployment.id, DeploymentResult::Succeeded)
            .await?;
        Ok(Deployed {
            deployment,
            release,
            status,
            endpoint,
        })
    }
}

/// A named volume holding one of the image's `VOLUME` paths.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mount {
    name: String,
    path: String,
}

/// What the generated Compose file says.
#[derive(Debug, Clone)]
struct Shape {
    /// The snapshot: the build context.
    context: String,
    /// Relative to the context.
    dockerfile: String,
    image: String,
    publish: Option<(IpAddr, PortMapping)>,
    volumes: Vec<Mount>,
}

impl Shape {
    fn endpoint(&self) -> Option<SocketAddr> {
        self.publish
            .map(|(ip, mapping)| SocketAddr::new(ip, mapping.host))
    }

    /// The generated Compose file. Holds paths, names and ports only,
    /// never a configured value. Paths are literal: each `$` is written
    /// `$$`, so Compose's interpolation reads them back unchanged.
    fn document(&self) -> Value {
        let mut web = Map::new();
        web.insert(
            "build".to_owned(),
            json!({
                "context": literal(&self.context),
                "dockerfile": literal(&self.dockerfile),
            }),
        );
        web.insert("image".to_owned(), json!(self.image));
        web.insert("restart".to_owned(), json!(RESTART_POLICY));
        if let Some((ip, m)) = self.publish {
            web.insert(
                "ports".to_owned(),
                json!([{
                    "target": m.container,
                    "published": m.host.to_string(),
                    "host_ip": ip.to_string(),
                    "protocol": m.protocol.as_str(),
                }]),
            );
        }
        let mut doc = Map::new();
        if !self.volumes.is_empty() {
            let mounts: Vec<Value> = self
                .volumes
                .iter()
                .map(|m| json!({ "type": "volume", "source": m.name, "target": literal(&m.path) }))
                .collect();
            web.insert("volumes".to_owned(), Value::Array(mounts));
            let named: Map<String, Value> = self
                .volumes
                .iter()
                .map(|m| (m.name.clone(), json!({})))
                .collect();
            doc.insert("volumes".to_owned(), Value::Object(named));
        }
        let mut services = Map::new();
        services.insert(SERVICE.to_owned(), Value::Object(web));
        doc.insert("services".to_owned(), Value::Object(services));
        Value::Object(doc)
    }

    /// `sha256:<hex>` of the effective non-secret configuration: what Ship
    /// decided, without the ids and paths that differ per release (ADR-0007
    /// rule 3).
    fn fingerprint(&self) -> String {
        let publish = self.publish.map(|(ip, m)| {
            json!({
                "address": ip.to_string(),
                "host": m.host,
                "container": m.container,
                "protocol": m.protocol.as_str(),
            })
        });
        let volumes: Vec<Value> = self
            .volumes
            .iter()
            .map(|m| json!([m.name, m.path]))
            .collect();
        let canonical = json!({
            "kind": BuildKind::Dockerfile.as_str(),
            "dockerfile": self.dockerfile,
            "service": SERVICE,
            "restart": RESTART_POLICY,
            "publish": publish,
            "volumes": volumes,
        });
        let digest = Sha256::digest(canonical.to_string().as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256:{hex}")
    }
}

/// `text` escaped for Compose interpolation: `$` is written `$$`.
pub(crate) fn literal(text: &str) -> String {
    text.replace('$', "$$")
}

/// One named volume per `VOLUME` path. The name depends on that exact path
/// alone, never on the image's other paths, so a path keeps its volume
/// across releases however paths are added or removed: a readable part
/// from the path (`/var/lib/data` gives `var-lib-data`) and the first 12
/// hex digits of the SHA-256 of the exact path, which tells apart paths
/// the readable part folds together (`/data`, `/DATA`, `/data/`).
fn volume_mounts(paths: &[String]) -> Vec<Mount> {
    paths
        .iter()
        .map(|path| Mount {
            name: volume_name(path),
            path: path.clone(),
        })
        .collect()
}

fn volume_name(path: &str) -> String {
    let words: Vec<String> = path
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let base = match words.join("-") {
        w if w.is_empty() => "volume".to_owned(),
        w => w,
    };
    let digest = Sha256::digest(path.as_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("{base}-{hex}")
}

pub(crate) fn path_text(path: &Path) -> Result<String, JobFailure> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        JobFailure::new(
            ErrorCode::Internal,
            "the state directory's path is not UTF-8",
        )
    })
}

pub(crate) fn create_release_dir(state_dir: &Path, dir: &Path) -> Result<(), JobFailure> {
    let made = fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir.join(RELEASES_DIR))
        .and_then(|()| fs::DirBuilder::new().mode(0o700).create(dir));
    made.map_err(|e| io_failure(&e, "the daemon could not create the release directory"))
}

pub(crate) fn io_failure(e: &io::Error, message: &str) -> JobFailure {
    tracing::error!(error = %e, "{message}");
    if e.kind() == io::ErrorKind::StorageFull {
        JobFailure::new(
            ErrorCode::InsufficientStorage,
            "the state directory is full",
        )
    } else {
        JobFailure::new(ErrorCode::Internal, message)
    }
}

/// Writes `bytes` to `path` (mode 0600), replacing it.
pub(crate) fn write_file(path: &Path, bytes: &[u8]) -> Result<(), JobFailure> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|e| io_failure(&e, "the daemon could not write the generated Compose file"))
}

/// Writes the release's Compose file for `shape`, reads it back through
/// `compose config` and writes the labels override for it. The files
/// together define the release.
async fn write_files(
    d: &Deployer,
    project: &ComposeProject,
    dir: &Path,
    shape: &Shape,
    labels: &DeployLabels,
) -> Result<ComposeFiles, JobFailure> {
    let compose_path = dir.join(COMPOSE_FILE);
    let labels_path = dir.join(LABELS_FILE);
    let text = serde_json::to_string_pretty(&shape.document())
        .map_err(|_| JobFailure::new(ErrorCode::Internal, "could not write the Compose file"))?;
    write_file(&compose_path, text.as_bytes())?;
    let bare = ComposeFiles::new(dir, [compose_path.clone()]).map_err(internal_docker)?;
    let config = compose::config(&d.docker, project, &bare)
        .await
        .map_err(|e| docker_failure(e, ErrorCode::DeployFailed, "checking the Compose file"))?;
    let override_text = labels_override(&config, labels).map_err(internal_docker)?;
    write_file(&labels_path, override_text.as_bytes())?;
    ComposeFiles::new(dir, [compose_path, labels_path]).map_err(internal_docker)
}

pub(crate) fn internal_docker(e: DockerError) -> JobFailure {
    tracing::error!(error = %e, "the Docker adapter refused generated input");
    JobFailure::new(ErrorCode::Internal, e.to_string())
}

/// The failure for a Docker error: `docker_unavailable` when Docker did
/// not answer, `conflict` for an object without Ship's labels, else `code`.
pub(crate) fn docker_failure(e: DockerError, code: ErrorCode, what: &str) -> JobFailure {
    match e {
        DockerError::Unreachable(_) => JobFailure::new(ErrorCode::DockerUnavailable, e.to_string()),
        DockerError::Foreign { .. } => JobFailure::new(ErrorCode::Conflict, e.to_string()),
        e => JobFailure::new(code, format!("{what}: {e}")),
    }
}

pub(crate) fn build_failure(e: DockerError) -> JobFailure {
    match e {
        DockerError::Failed { status, .. } => JobFailure::new(
            ErrorCode::BuildFailed,
            match status.code() {
                Some(code) => format!("the image build exited {code}"),
                None => "the image build was killed".to_owned(),
            },
        ),
        DockerError::Timeout(after) => JobFailure::new(
            ErrorCode::BuildTimeout,
            format!("the build ran longer than {} s", after.as_secs()),
        ),
        e => docker_failure(e, ErrorCode::BuildFailed, "the image build failed"),
    }
}

/// Docker's words for a host port another process holds.
const PORT_TAKEN: &[&str] = &["port is already allocated", "address already in use"];

fn up_failure(e: DockerError, endpoint: Option<SocketAddr>) -> JobFailure {
    up_failure_of(e, endpoint, &format!("starting {SERVICE} failed"))
}

/// The failure of `compose up`: `port_conflict` when Docker says a host
/// port is taken, else `deploy_failed` saying `what`.
pub(crate) fn up_failure_of(
    e: DockerError,
    endpoint: Option<SocketAddr>,
    what: &str,
) -> JobFailure {
    if let DockerError::Failed { detail, .. } = &e {
        let lower = detail.to_ascii_lowercase();
        if PORT_TAKEN.iter().any(|m| lower.contains(m)) {
            let at = endpoint.map_or_else(|| "a published port".to_owned(), |e| e.to_string());
            return JobFailure::new(
                ErrorCode::PortConflict,
                format!("{at} is already in use on the host: {detail}"),
            );
        }
    }
    docker_failure(e, ErrorCode::DeployFailed, what)
}

/// Refuses an endpoint another process holds, before anything is started
/// (ADR-0010 rule 3). An unspecified address is not probed (the probe would
/// listen on every interface); a port the daemon may not bind (below 1024)
/// is left to Docker. Either way `up` still reports a conflict.
pub(crate) fn probe(endpoint: SocketAddr, mapping: PortMapping) -> Result<(), JobFailure> {
    if endpoint.ip().is_unspecified() {
        return Ok(());
    }
    let bound = match mapping.protocol {
        ship_core::deploy::Protocol::Tcp => TcpListener::bind(endpoint).map(drop),
        ship_core::deploy::Protocol::Udp => UdpSocket::bind(endpoint).map(drop),
    };
    match bound {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Err(JobFailure::new(
            ErrorCode::PortConflict,
            format!(
                "{endpoint}/{} is already in use on the host; choose another --port",
                mapping.protocol
            ),
        )),
        Err(e) if e.kind() == io::ErrorKind::AddrNotAvailable => Err(JobFailure::new(
            ErrorCode::DeployFailed,
            format!(
                "{} is not an address of this host; check publish.address",
                endpoint.ip()
            ),
        )),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        Err(e) => Err(JobFailure::new(
            ErrorCode::DeployFailed,
            format!("cannot publish on {endpoint}: {e}"),
        )),
    }
}

/// `compose up --remove-orphans` with its output in the log.
pub(crate) async fn up(
    ctx: &Context,
    d: &Deployer,
    project: &ComposeProject,
    files: &ComposeFiles,
) -> Result<(), DockerError> {
    let forward = LogForward::start(ctx);
    let result = {
        let mut sink = forward.sink();
        let options = UpOptions {
            remove_orphans: true,
        };
        let no_services: &[&str] = &[];
        compose::up(
            &d.docker,
            project,
            files,
            no_services,
            options,
            d.settings.step_timeout,
            &mut sink,
        )
        .await
    };
    let _ = forward.finish().await;
    result
}

/// `compose stop` with its output in the log.
async fn stop(ctx: &Context, d: &Deployer, project: &ComposeProject) -> Result<(), DockerError> {
    let forward = LogForward::start(ctx);
    let result = {
        let mut sink = forward.sink();
        let no_services: &[&str] = &[];
        compose::stop(
            &d.docker,
            project,
            no_services,
            d.settings.step_timeout,
            &mut sink,
        )
        .await
    };
    let _ = forward.finish().await;
    result
}

/// Waits for the verdict on each service's container of the release
/// (ADR-0006 rule 2): a passing health check is `healthy`; with no check,
/// still running and never restarted after the settle period is
/// `running_unverified`; a failing check, an exit, a restart or no verdict
/// in time fails, naming the service. Every service must pass; the
/// statuses come back in the order of `services`.
pub(crate) async fn verify(
    d: &Deployer,
    environment: EnvironmentId,
    services: &[Service],
    release: ReleaseId,
) -> Result<Vec<ServiceStatus>, JobFailure> {
    let started = tokio::time::Instant::now();
    let deadline = started + d.settings.verify_timeout;
    let selector = Selector::environment(environment);
    let failed = |message: String| JobFailure::new(ErrorCode::HealthCheckFailed, message);
    let mut verdicts: Vec<Option<ServiceStatus>> = vec![None; services.len()];
    let mut last: Vec<Option<ServiceStatus>> = vec![None; services.len()];
    loop {
        let found = containers(&d.docker, &selector).await.map_err(|e| {
            docker_failure(e, ErrorCode::HealthCheckFailed, "reading the containers")
        })?;
        for (i, service) in services.iter().enumerate() {
            if verdicts[i].is_some() {
                continue;
            }
            let name = &service.name;
            let Some(c) = found
                .iter()
                .find(|c| c.labels.release == release && c.labels.service == Some(service.id))
            else {
                continue;
            };
            let status = ServiceStatus::from_observation(c.observation, DesiredState::Running);
            last[i] = Some(status);
            match status {
                ServiceStatus::Healthy => verdicts[i] = Some(status),
                ServiceStatus::RunningUnverified if c.restart_count > 0 => {
                    return Err(failed(format!(
                        "{name} restarted {} times after starting",
                        c.restart_count
                    )));
                }
                ServiceStatus::RunningUnverified if started.elapsed() >= d.settings.settle => {
                    verdicts[i] = Some(status);
                }
                ServiceStatus::RunningUnverified | ServiceStatus::Starting => {}
                ServiceStatus::Unhealthy => {
                    return Err(failed(format!("the health check of {name} failed")));
                }
                ServiceStatus::Failed
                | ServiceStatus::Stopped
                | ServiceStatus::Paused
                | ServiceStatus::Unknown => {
                    let detail =
                        FailureDetail::from_observation(c.observation, DesiredState::Running)
                            .map(|f| format!(": {f}"))
                            .unwrap_or_default();
                    return Err(failed(format!("{name} is {status}{detail}")));
                }
            }
        }
        if verdicts.iter().all(Option::is_some) {
            return Ok(verdicts.into_iter().flatten().collect());
        }
        if tokio::time::Instant::now() >= deadline {
            let secs = d.settings.verify_timeout.as_secs();
            let (i, service) = services
                .iter()
                .enumerate()
                .find(|(i, _)| verdicts[*i].is_none())
                .expect("a service has no verdict yet");
            let name = &service.name;
            return Err(failed(match last[i] {
                None => format!("no container of {name} appeared within {secs} s"),
                Some(ServiceStatus::Starting) => {
                    format!("the health check of {name} did not pass within {secs} s")
                }
                Some(status) => format!("{name} was still {status} after {secs} s"),
            }));
        }
        tokio::time::sleep(d.settings.poll).await;
    }
}

/// Carries output lines from a synchronous sink to the operation's log.
pub(crate) struct LogForward {
    tx: mpsc::UnboundedSender<String>,
    task: JoinHandle<Result<(), JobFailure>>,
}

impl LogForward {
    pub(crate) fn start(ctx: &Context) -> LogForward {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let ctx = ctx.clone();
        let task = tokio::spawn(async move {
            let mut first_error = None;
            while let Some(line) = rx.recv().await {
                if first_error.is_none()
                    && let Err(e) = ctx.log(&line).await
                {
                    first_error = Some(e);
                }
            }
            first_error.map_or(Ok(()), Err)
        });
        LogForward { tx, task }
    }

    /// A sink for one adapter call; drop it before [`LogForward::finish`].
    pub(crate) fn sink(&self) -> impl FnMut(Line) + Send + 'static {
        let tx = self.tx.clone();
        move |line: Line| {
            let _ = tx.send(line.text);
        }
    }

    /// Waits until every line sent is logged.
    pub(crate) async fn finish(self) -> Result<(), JobFailure> {
        drop(self.tx);
        self.task.await.unwrap_or_else(|_| {
            Err(JobFailure::new(
                ErrorCode::Internal,
                "the operation log writer failed",
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ship_core::deploy::Protocol;
    use std::collections::BTreeSet;

    fn shape() -> Shape {
        Shape {
            context: "/var/lib/ship/snapshots/abc".to_owned(),
            dockerfile: "Dockerfile".to_owned(),
            image: "ship_env_x-web:rel_y".to_owned(),
            publish: None,
            volumes: Vec::new(),
        }
    }

    #[test]
    fn the_generated_file_publishes_on_the_address() {
        let mut s = shape();
        assert_eq!(
            s.document(),
            json!({ "services": { "web": {
                "build": { "context": "/var/lib/ship/snapshots/abc", "dockerfile": "Dockerfile" },
                "image": "ship_env_x-web:rel_y",
                "restart": "unless-stopped",
            }}})
        );
        let mapping = PortMapping {
            host: 8080,
            container: 3000,
            protocol: Protocol::Tcp,
        };
        s.publish = Some((IpAddr::V4(Ipv4Addr::new(100, 64, 0, 9)), mapping));
        s.volumes = volume_mounts(&["/data".to_owned()]);
        let doc = s.document();
        assert_eq!(
            doc["services"]["web"]["ports"],
            json!([{ "target": 3000, "published": "8080", "host_ip": "100.64.0.9", "protocol": "tcp" }])
        );
        assert_eq!(
            doc["services"]["web"]["volumes"],
            json!([{ "type": "volume", "source": volume_name("/data"), "target": "/data" }])
        );
        assert_eq!(doc["volumes"], json!({ volume_name("/data"): {} }));
        assert_eq!(s.endpoint().unwrap().to_string(), "100.64.0.9:8080");
        let v6 = SocketAddr::new("fd7a::1".parse().unwrap(), 8080);
        assert_eq!(v6.to_string(), "[fd7a::1]:8080");
    }

    /// A Dockerfile, a context and a `VOLUME` path that Compose would
    /// interpolate if they were not escaped.
    fn dollar_shape(context: &str) -> Shape {
        Shape {
            context: context.to_owned(),
            dockerfile: "Dockerfile.${SHIP_REVIEW_UNSET_VAR:-wrong}".to_owned(),
            image: "ship_env_x-web:rel_y".to_owned(),
            publish: None,
            volumes: volume_mounts(&["/data/$SHIP_REVIEW_UNSET_VAR".to_owned()]),
        }
    }

    #[test]
    fn dollar_signs_in_paths_are_escaped_for_compose() {
        let s = dollar_shape("/state/$HOME/snapshots/abc");
        let doc = s.document();
        let web = &doc["services"]["web"];
        assert_eq!(web["build"]["context"], "/state/$$HOME/snapshots/abc");
        assert_eq!(
            web["build"]["dockerfile"],
            "Dockerfile.$${SHIP_REVIEW_UNSET_VAR:-wrong}"
        );
        assert_eq!(web["volumes"][0]["target"], "/data/$$SHIP_REVIEW_UNSET_VAR");
        let name = web["volumes"][0]["source"].as_str().unwrap();
        assert!(!name.contains('$'), "{name}");
        assert_eq!(literal("a$$b"), "a$$$$b");
    }

    /// `compose config` reads the generated file's paths back exactly.
    #[tokio::test]
    #[ignore = "needs a real Docker Engine; run by the CI docker job"]
    async fn real_compose_config_keeps_literal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let context = root.join("ctx-$SHIP_REVIEW_UNSET_VAR");
        fs::create_dir(&context).unwrap();
        let context = context.to_str().unwrap().to_owned();
        let s = dollar_shape(&context);
        let file = root.join(COMPOSE_FILE);
        fs::write(&file, serde_json::to_string_pretty(&s.document()).unwrap()).unwrap();
        let files = ComposeFiles::new(&root, [file]).unwrap();
        let project = ComposeProject::new(EnvironmentId::from_parts(1_767_225_600_000, 7).unwrap());
        let config = compose::config(&Runner::docker(), &project, &files)
            .await
            .unwrap();
        // Compose may print a literal `$` as `$$` again; either way no
        // part of a path was interpolated.
        let read = |v: &Value| v.as_str().unwrap().replace("$$", "$");
        let web = &config.expose()["services"]["web"];
        assert_eq!(read(&web["build"]["context"]), context);
        assert_eq!(read(&web["build"]["dockerfile"]), s.dockerfile);
        assert_eq!(read(&web["volumes"][0]["target"]), s.volumes[0].path);
    }

    #[test]
    fn the_fingerprint_follows_the_configuration_not_the_ids() {
        let a = shape();
        let mut b = shape();
        b.image = "ship_env_x-web:rel_z".to_owned();
        b.context = "/elsewhere".to_owned();
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert!(a.fingerprint().starts_with("sha256:"));
        assert_eq!(a.fingerprint().len(), "sha256:".len() + 64);
        b.publish = Some((
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            PortMapping {
                host: 1,
                container: 1,
                protocol: Protocol::Tcp,
            },
        ));
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    fn names(paths: &[&str]) -> Vec<(String, String)> {
        let paths: Vec<String> = paths.iter().map(|s| (*s).to_owned()).collect();
        volume_mounts(&paths)
            .into_iter()
            .map(|m| (m.path, m.name))
            .collect()
    }

    fn name_of(mounts: &[(String, String)], path: &str) -> String {
        mounts.iter().find(|(p, _)| p == path).unwrap().1.clone()
    }

    #[test]
    fn volumes_are_named_after_their_paths() {
        let all = names(&[
            "/var/lib/data",
            "/data",
            "/DATA",
            "/",
            "/a",
            "/var/lib/data/",
        ]);
        let readable: Vec<&str> = all
            .iter()
            .map(|(_, n)| n.rsplit_once('-').unwrap().0)
            .collect();
        assert_eq!(
            readable,
            [
                "var-lib-data",
                "data",
                "data",
                "volume",
                "a",
                "var-lib-data"
            ]
        );
        for (_, n) in &all {
            let hex = n.rsplit_once('-').unwrap().1;
            assert_eq!(hex.len(), 12, "{n}");
            assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "{n}");
        }
        // Paths that fold to the same words still get their own volume.
        let distinct: BTreeSet<&String> = all.iter().map(|(_, n)| n).collect();
        assert_eq!(distinct.len(), all.len());
    }

    #[test]
    fn a_volume_keeps_its_name_as_colliding_paths_come_and_go() {
        // Release 1 has /data; release 2 adds /DATA (before it, and after
        // it); release 3 drops /data. /data and /DATA each keep one name.
        let r1 = names(&["/data"]);
        let r2 = names(&["/DATA", "/data"]);
        let r2b = names(&["/data", "/DATA"]);
        let r3 = names(&["/DATA"]);
        let data = name_of(&r1, "/data");
        assert_eq!(name_of(&r2, "/data"), data);
        assert_eq!(name_of(&r2b, "/data"), data);
        let upper = name_of(&r2, "/DATA");
        assert_ne!(upper, data);
        assert_eq!(name_of(&r2b, "/DATA"), upper);
        assert_eq!(name_of(&r3, "/DATA"), upper);
    }

    #[test]
    fn port_conflicts_are_told_apart() {
        let status = std::process::Command::new("false").status().unwrap();
        let failed = |detail: &str| DockerError::Failed {
            command: "compose up",
            status,
            detail: detail.to_owned(),
        };
        let at = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080));
        let e = up_failure(
            failed("Bind for 127.0.0.1:8080 failed: port is already allocated"),
            at,
        );
        assert_eq!(e.code, ErrorCode::PortConflict);
        assert!(
            e.message.starts_with("127.0.0.1:8080 is already in use"),
            "{}",
            e.message
        );
        assert_eq!(
            up_failure(failed("no such image"), at).code,
            ErrorCode::DeployFailed
        );
        assert_eq!(
            up_failure(DockerError::Unreachable("x".to_owned()), at).code,
            ErrorCode::DockerUnavailable
        );
    }

    #[test]
    fn a_held_port_is_a_conflict() {
        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = held.local_addr().unwrap();
        let mapping = PortMapping {
            host: endpoint.port(),
            container: 80,
            protocol: Protocol::Tcp,
        };
        assert_eq!(
            probe(endpoint, mapping).unwrap_err().code,
            ErrorCode::PortConflict
        );
        drop(held);
        // The port is free again, though a kernel may take a moment to
        // release it.
        let mut again = probe(endpoint, mapping);
        for _ in 0..50 {
            if again.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            again = probe(endpoint, mapping);
        }
        assert!(again.is_ok(), "{again:?}");
        let unspecified = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), endpoint.port());
        assert!(probe(unspecified, mapping).is_ok());
        // TEST-NET-1 is never an address of the host.
        let foreign = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 8080);
        assert_eq!(
            probe(foreign, mapping).unwrap_err().code,
            ErrorCode::DeployFailed
        );
    }
}
