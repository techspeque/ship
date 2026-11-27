//! Wire types shared by the client and the daemon.
//!
//! Shapes are provisional until Phase 5 (ADR-0004 rule 4).

use serde::{Deserialize, Serialize};

use crate::deploy::PortSpec;
use crate::error::ReceivedCode;
use crate::id::{DeploymentId, EnvironmentId, OperationId, ProjectId, ReleaseId, RevisionId};
use crate::slug::Slug;
use crate::state::{OperationPhase, OperationStatus};
use crate::time::Timestamp;

/// The API version this build speaks.
pub const API_VERSION: &str = "v1";
/// Path prefix of every versioned endpoint.
pub const API_PREFIX: &str = "/api/v1";
/// Request and response header carrying the API version (`v1`). A client
/// sends the version it speaks; the daemon answers with its own on every
/// `/api/` response and refuses a different one with `426
/// unsupported_api_version`.
pub const API_VERSION_HEADER: &str = "ship-api-version";
/// Response header carrying the daemon's Ship version on `/api/` responses,
/// so an incompatible client can name the release to upgrade to.
pub const SHIP_VERSION_HEADER: &str = "ship-version";

/// The number in an API version string (`v1` → 1); `None` if malformed.
pub fn api_version_number(version: &str) -> Option<u32> {
    let digits = version.strip_prefix('v')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `GET`: [`HostInfo`].
pub const HOST_PATH: &str = "/api/v1/host";
/// `GET`: [`ProjectList`].
pub const PROJECTS_PATH: &str = "/api/v1/projects";

/// Prefix of the operation endpoints: `GET {OPERATIONS_PATH}/{id}` is an
/// [`OperationView`]; `GET {OPERATIONS_PATH}/{id}/events` streams
/// [`OperationEventView`]s over SSE (ADR-0011 rule 5).
pub const OPERATIONS_PATH: &str = "/api/v1/operations";
/// Request header every mutating call carries (ADR-0011 rule 1). A replay
/// with the same key returns the original operation.
pub const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
/// Longest idempotency key the daemon accepts, in bytes.
pub const IDEMPOTENCY_KEY_MAX_LEN: usize = 128;
/// SSE resume header: the last sequence number the client received.
pub const LAST_EVENT_ID_HEADER: &str = "last-event-id";

/// `GET /api/v1/operations/{id}`.
pub fn operation_path(id: OperationId) -> String {
    format!("{OPERATIONS_PATH}/{id}")
}

/// `GET /api/v1/operations/{id}/events`; `?after=N` resumes after `N`.
pub fn operation_events_path(id: OperationId) -> String {
    format!("{OPERATIONS_PATH}/{id}/events")
}

/// `POST`: the body is a tar + zstd snapshot of `project` (ADR-0005),
/// answered with an [`UploadView`]. Needs an idempotency key and
/// [`SNAPSHOT_SHA256_HEADER`]; the project is created if it is new.
pub fn uploads_path(project: &Slug) -> String {
    format!("{PROJECTS_PATH}/{}/uploads", project.as_str())
}

/// Request header of an upload: the SHA-256 of the compressed stream, 64
/// lowercase hex digits. The daemon checks it once the stream has ended.
pub const SNAPSHOT_SHA256_HEADER: &str = "ship-snapshot-sha256";
/// Optional request header of an upload: the client's Git commit, 7 to 64
/// lowercase hex digits. Information only (ADR-0005 rule 8).
pub const GIT_COMMIT_HEADER: &str = "ship-git-commit";
/// Optional request header of an upload: `true` when the working tree had
/// uncommitted changes, else `false`. Information only.
pub const GIT_DIRTY_HEADER: &str = "ship-git-dirty";
/// The operation kind of an upload.
pub const UPLOAD_KIND: &str = "upload";

/// A snapshot received and stored as a revision: `201` when the revision
/// is new, `200` when the project already had one with this hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadView {
    pub operation_id: OperationId,
    pub project_id: ProjectId,
    pub revision_id: RevisionId,
    /// The revision's content hash: the stream's SHA-256.
    pub sha256: String,
    /// `false` when the project already had a revision with this hash.
    pub new_revision: bool,
}

/// `POST`: deploy a revision of `project` to its `development`
/// environment. The body is a [`DeployRequest`]; the answer `202` with the
/// `deploy` operation. Needs an idempotency key.
pub fn deployments_path(project: &Slug) -> String {
    format!("{PROJECTS_PATH}/{}/deployments", project.as_str())
}

/// The operation kind of a deployment.
pub const DEPLOY_KIND: &str = "deploy";

/// What to deploy: a revision of the project, optionally the build file to
/// use and the port to publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployRequest {
    pub revision_id: RevisionId,
    /// A path relative to the revision's root (`--file`); `None` detects.
    #[serde(default)]
    pub file: Option<String>,
    /// `--port`: `N` or `HOST:CONTAINER`; `None` publishes nothing.
    #[serde(default)]
    pub port: Option<PortSpec>,
    /// The operator confirms the Compose file's confirm items (ADR-0008
    /// rule 2): `--confirm`, or a yes on a terminal. Recorded in the audit
    /// log with the file's fingerprint. Never waives a reject item.
    #[serde(default)]
    pub confirm: bool,
}

/// `POST`: `ship push`. The body is a snapshot exactly as for
/// [`uploads_path`], with the same headers; the query carries the
/// deployment's options ([`push_query`]). One `push` operation on the
/// project's `development` environment receives the snapshot and deploys
/// it. The answer, once the snapshot is stored, is `202` with a
/// [`PushView`]; a failed upload answers its error with `operation_id`. A
/// new project is created first by its own `project` operation.
pub fn pushes_path(project: &Slug) -> String {
    format!("{PROJECTS_PATH}/{}/pushes", project.as_str())
}

/// The operation kind of a push: an upload, then a deployment.
pub const PUSH_KIND: &str = "push";
/// The operation kind that creates a project and its `development`
/// environment before its first push.
pub const PROJECT_KIND: &str = "project";

/// The query of a push: `file` percent-encoded, `port` as `N` or
/// `HOST:CONTAINER`, `confirm=true`; empty when none is set.
pub fn push_query(file: Option<&str>, port: Option<PortSpec>, confirm: bool) -> String {
    let mut pairs = Vec::new();
    if let Some(file) = file {
        pairs.push(format!("file={}", percent_encode(file)));
    }
    if let Some(port) = port {
        pairs.push(format!("port={}", percent_encode(&port.to_string())));
    }
    if confirm {
        pairs.push("confirm=true".to_owned());
    }
    if pairs.is_empty() {
        String::new()
    } else {
        format!("?{}", pairs.join("&"))
    }
}

/// The options a push query carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PushOptions {
    pub file: Option<String>,
    pub port: Option<PortSpec>,
    pub confirm: bool,
}

/// Reads a push query ([`push_query`]); an unknown or repeated key, a bad
/// encoding or a bad value is an error naming it.
pub fn parse_push_query(query: Option<&str>) -> Result<PushOptions, String> {
    let mut options = PushOptions::default();
    let mut seen = Vec::new();
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if seen.contains(&key) {
            return Err(format!("`{key}` is given twice"));
        }
        seen.push(key);
        let value = percent_decode(value).ok_or_else(|| format!("`{key}` is badly encoded"))?;
        match key {
            "file" => options.file = Some(value),
            "port" => {
                options.port =
                    Some(value.parse().map_err(|_| {
                        format!("`port` must be N or HOST:CONTAINER, not `{value}`")
                    })?)
            }
            "confirm" => match value.as_str() {
                "true" => options.confirm = true,
                "false" => options.confirm = false,
                _ => return Err("`confirm` must be `true` or `false`".to_owned()),
            },
            _ => return Err(format!("unknown push option `{key}`")),
        }
    }
    Ok(options)
}

/// Percent-encodes every byte but ASCII letters, digits and `-._~`.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` escapes; `None` for a malformed escape or bytes that are
/// not UTF-8. `+` is kept as it is.
pub fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A push whose snapshot is stored: its operation, which goes on to
/// deploy, and what it deploys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushView {
    pub operation: OperationView,
    pub project_id: ProjectId,
    pub environment_id: EnvironmentId,
    pub revision_id: RevisionId,
}

/// `GET`: the deployment an operation recorded, as an
/// [`OperationDeploymentView`]; `404 not_found` when it recorded none.
pub fn operation_deployment_path(id: OperationId) -> String {
    format!("{OPERATIONS_PATH}/{id}/deployment")
}

/// The deployment an operation recorded, and, when it succeeded, each
/// service with the status Docker reports now and the endpoints that
/// answer now (OVERVIEW §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationDeploymentView {
    pub deployment_id: DeploymentId,
    /// `None` when the deployment failed before a release was recorded.
    pub release_id: Option<ReleaseId>,
    /// A `DeploymentResult` string; `None` while in progress.
    pub result: Option<String>,
    /// Empty unless the deployment succeeded.
    pub services: Vec<ServiceOutcomeView>,
}

/// One service of a deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceOutcomeView {
    pub name: String,
    /// A `ServiceStatus` string.
    pub status: String,
    /// Only addresses that answered; empty when none.
    pub endpoints: Vec<EndpointView>,
}

/// An address a service is reached on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointView {
    /// A `RouteProvider` string.
    pub provider: String,
    pub address: String,
}

/// Prefix of the daemon settings: `GET {SETTINGS_PATH}/{key}` is a
/// [`SettingView`]; `POST` with a [`SetSettingRequest`] and an idempotency
/// key changes it as a `settings` operation.
pub const SETTINGS_PATH: &str = "/api/v1/settings";
/// The operation kind of a settings change.
pub const SETTINGS_KIND: &str = "settings";
/// Where the ports Ship publishes bind: `auto` or an address
/// ([`crate::deploy::PublishAddress`]).
pub const PUBLISH_ADDRESS_KEY: &str = "publish.address";
/// Whether the confirm items of a Compose file are asked for (`ask`) or
/// accepted by policy (`accept`), for the host or one project
/// ([`crate::trust::ConfirmPolicy`]).
pub const COMPOSE_CONFIRM_KEY: &str = "compose.confirm";

/// `GET` and `POST` path of the setting `key`.
pub fn setting_path(key: &str) -> String {
    format!("{SETTINGS_PATH}/{key}")
}

/// `GET` and `POST` path of the project-scoped setting `key`
/// ([`COMPOSE_CONFIRM_KEY`] only). `GET` gives the value in effect for the
/// project: its own, else the host's, else the default.
pub fn project_setting_path(project: &Slug, key: &str) -> String {
    format!("{PROJECTS_PATH}/{}/settings/{key}", project.as_str())
}

/// A daemon setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingView {
    pub key: String,
    /// The stored value, or the default when it was never set.
    pub value: String,
    /// `None` when it was never set.
    pub updated_at: Option<Timestamp>,
}

/// The body of a settings change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSettingRequest {
    pub value: String,
}

// ---- config values and secrets (ADR-0009) ----

/// The operation kind of a change to a config value or a secret.
pub const CONFIG_KIND: &str = "config";

/// `GET` (with [`config_query`]): a [`VarList`]; `POST` with a
/// [`SetVarRequest`] and an idempotency key: set one, as a `config`
/// operation answered `202`.
pub fn vars_path(project: &Slug) -> String {
    format!("{PROJECTS_PATH}/{}/vars", project.as_str())
}

/// `POST` with an [`UnsetRequest`]: remove a config value.
pub fn vars_unset_path(project: &Slug) -> String {
    format!("{PROJECTS_PATH}/{}/vars/unset", project.as_str())
}

/// `GET` (with [`config_query`]): a [`SecretList`], names and timestamps
/// only; `POST` with a [`SetSecretRequest`]: set one.
pub fn secrets_path(project: &Slug) -> String {
    format!("{PROJECTS_PATH}/{}/secrets", project.as_str())
}

/// `POST` with an [`UnsetRequest`]: remove a secret.
pub fn secrets_unset_path(project: &Slug) -> String {
    format!("{PROJECTS_PATH}/{}/secrets/unset", project.as_str())
}

/// The query selecting a scope for a list: none for the project,
/// `?env=E` for an environment, `?env=E&service=S` for a service (`env`
/// defaults to `development` when only `service` is given).
pub fn config_query(env: Option<&str>, service: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(e) = env {
        parts.push(format!("env={}", percent_encode(e)));
    }
    if let Some(s) = service {
        parts.push(format!("service={}", percent_encode(s)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// Sets a config value. With no `env` and no `service` it is the
/// project's; with `env`, that environment's; with `service`, that service's
/// (in `env`, by default `development`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetVarRequest {
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
    pub name: String,
    pub value: String,
}

/// Sets a secret, scoped as [`SetVarRequest`]. Its `Debug` never shows the
/// value. The value is never answered back.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSecretRequest {
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
    pub name: String,
    pub value: String,
}

impl std::fmt::Debug for SetSecretRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetSecretRequest")
            .field("env", &self.env)
            .field("service", &self.service)
            .field("name", &self.name)
            .field("value", &crate::secret::REDACTED)
            .finish()
    }
}

/// Removes a config value or a secret, scoped as [`SetVarRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsetRequest {
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
    pub name: String,
}

/// A config value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VarView {
    pub name: String,
    pub value: String,
    pub updated_at: Timestamp,
}

/// A secret: its name and when it was last set. Never its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretView {
    pub name: String,
    pub updated_at: Timestamp,
}

/// The config values set in one scope, and the services of the project's
/// environment running with values older than the stored ones (ADR-0009
/// rule 6), by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VarList {
    pub scope: crate::config::ConfigScope,
    pub project: Slug,
    pub environment: Option<String>,
    pub service: Option<String>,
    pub vars: Vec<VarView>,
    pub stale_services: Vec<String>,
}

/// The secrets set in one scope, as [`VarList`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretList {
    pub scope: crate::config::ConfigScope,
    pub project: Slug,
    pub environment: Option<String>,
    pub service: Option<String>,
    pub secrets: Vec<SecretView>,
    pub stale_services: Vec<String>,
}

/// Whether `key` is an acceptable idempotency key: 1 to
/// [`IDEMPOTENCY_KEY_MAX_LEN`] visible ASCII characters.
pub fn valid_idempotency_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= IDEMPOTENCY_KEY_MAX_LEN
        && key.bytes().all(|b| b.is_ascii_graphic())
}

/// A durable operation as the API shows it (ADR-0011).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationView {
    pub id: OperationId,
    pub kind: String,
    pub environment_id: Option<EnvironmentId>,
    pub phase: OperationPhase,
    pub status: OperationStatus,
    /// Set exactly when the status is `failed` or `interrupted`.
    pub failure: Option<OperationFailureView>,
    /// Bytes written to the operation's on-disk log so far.
    pub log_bytes: u64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Why an operation failed: a stable code and the phase it failed in
/// (ADR-0011 rule 7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationFailureView {
    pub code: ReceivedCode,
    pub phase: OperationPhase,
    pub message: String,
}

string_enum! {
    /// What an operation event records. Readers must tolerate values they
    /// do not know, so [`OperationEventView::event`] is a plain string.
    pub enum EventKind {
        /// The operation entered `phase`.
        PhaseStarted => "phase_started",
        /// One line of build or deploy output.
        Log => "log",
        /// The operation waits for something it needs (a build slot).
        Waiting => "waiting",
        /// The operation ended; `phase` is its terminal phase. Always the
        /// last event of an operation.
        Finished => "finished",
    }
}

/// One event of an operation, as each SSE `data:` line carries it. The
/// field names are those of the `ship push --wait` event lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationEventView {
    pub operation_id: OperationId,
    /// Starts at 1 and increases by one, with no gaps. Also the SSE `id:`.
    pub sequence: u64,
    pub phase: OperationPhase,
    /// An [`EventKind`] string.
    pub event: String,
    pub message: String,
    pub ts: Timestamp,
}

/// `GET /api/v1/host`. The daemon's identity, whether Docker is usable and
/// the host's capacity. Served only on the daemon's own listeners;
/// `/healthz` reveals none of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// The API version the daemon speaks, e.g. `v1`.
    pub api_version: String,
    /// The daemon's Ship version.
    pub version: String,
    /// Operating system, e.g. `linux`.
    pub os: String,
    /// CPU architecture, e.g. `x86_64`.
    pub arch: String,
    /// Docker Engine and Compose preflight (ADR-0001 rule 6).
    pub docker: DockerInfo,
    /// What the host has to run workloads on.
    pub capacity: HostCapacity,
}

string_enum! {
    /// Whether the daemon can use Docker.
    pub enum DockerStatus {
        /// Engine reachable; Engine and Compose inside the supported range.
        Ready => "ready",
        /// The Engine did not answer. Never read as "stopped".
        Unreachable => "unreachable",
        /// Engine or Compose missing from, or outside, the supported range.
        UnsupportedVersion => "unsupported_version",
    }
}

/// The supported version ranges as text, e.g. `>= 24.0.0, < 30.0.0`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupportedVersions {
    pub engine: String,
    pub compose: String,
}

/// Result of the Docker preflight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerInfo {
    pub status: DockerStatus,
    /// The Engine's version as Docker reported it; `None` when unknown.
    pub engine_version: Option<String>,
    /// The Compose plugin's version as it reported it; `None` when unknown.
    pub compose_version: Option<String>,
    /// The ranges this build supports.
    pub supported: SupportedVersions,
    /// What is wrong and what to do about it; `None` when `ready`.
    pub message: Option<String>,
}

/// Host capacity; each field `None` when it could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HostCapacity {
    /// CPUs available to the daemon.
    pub cpus: Option<u64>,
    /// Total memory in bytes.
    pub memory_bytes: Option<u64>,
    /// Bytes free to unprivileged users on the state directory's filesystem.
    pub disk_free_bytes: Option<u64>,
}

/// One project in [`ProjectList`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: ProjectId,
    pub slug: Slug,
    pub created_at: Timestamp,
}

/// `GET /api/v1/projects`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectList {
    pub projects: Vec<ProjectSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_are_under_prefix() {
        assert_eq!(API_PREFIX, format!("/api/{API_VERSION}"));
        assert!(HOST_PATH.starts_with(API_PREFIX));
        assert!(PROJECTS_PATH.starts_with(API_PREFIX));
        let slug = Slug::parse("my-app").unwrap();
        assert_eq!(uploads_path(&slug), "/api/v1/projects/my-app/uploads");
        let token = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        for h in [SNAPSHOT_SHA256_HEADER, GIT_COMMIT_HEADER, GIT_DIRTY_HEADER] {
            assert!(h.bytes().all(token), "{h}");
        }
    }

    #[test]
    fn deploy_and_settings_shapes() {
        let slug = Slug::parse("my-app").unwrap();
        assert_eq!(
            deployments_path(&slug),
            "/api/v1/projects/my-app/deployments"
        );
        assert_eq!(
            setting_path(PUBLISH_ADDRESS_KEY),
            "/api/v1/settings/publish.address"
        );
        assert!(SETTINGS_PATH.starts_with(API_PREFIX));

        let request: DeployRequest = serde_json::from_value(json!({
            "revision_id": "rev_01aryz6s41tsv4rrffq69g5fav",
            "port": "8080:3000",
        }))
        .unwrap();
        assert_eq!(request.file, None);
        assert_eq!(request.port.unwrap().to_string(), "8080:3000");
        let v = serde_json::to_value(&request).unwrap();
        assert_eq!(
            v,
            json!({
                "revision_id": "rev_01aryz6s41tsv4rrffq69g5fav",
                "file": null,
                "port": "8080:3000",
                "confirm": false,
            })
        );
        let bad = json!({ "revision_id": "rev_01aryz6s41tsv4rrffq69g5fav", "port": "0" });
        assert!(serde_json::from_value::<DeployRequest>(bad).is_err());

        let unset = SettingView {
            key: PUBLISH_ADDRESS_KEY.to_owned(),
            value: "auto".to_owned(),
            updated_at: None,
        };
        assert_eq!(
            serde_json::to_value(&unset).unwrap(),
            json!({ "key": "publish.address", "value": "auto", "updated_at": null })
        );
    }

    #[test]
    fn push_paths_and_query() {
        let slug = Slug::parse("my-app").unwrap();
        assert_eq!(pushes_path(&slug), "/api/v1/projects/my-app/pushes");
        let op: OperationId = "op_01aryz6s41tsv4rrffq69g5fav".parse().unwrap();
        assert_eq!(
            operation_deployment_path(op),
            "/api/v1/operations/op_01aryz6s41tsv4rrffq69g5fav/deployment"
        );
        assert_eq!(push_query(None, None, false), "");
        let port: PortSpec = "8080:3000".parse().unwrap();
        let q = push_query(Some("deploy/My File é.yml"), Some(port), true);
        assert_eq!(
            q,
            "?file=deploy%2FMy%20File%20%C3%A9.yml&port=8080%3A3000&confirm=true"
        );
        let parsed = parse_push_query(q.strip_prefix('?')).unwrap();
        assert_eq!(
            parsed,
            PushOptions {
                file: Some("deploy/My File é.yml".to_owned()),
                port: Some(port),
                confirm: true,
            }
        );
        assert_eq!(parse_push_query(None).unwrap(), PushOptions::default());
        for bad in [
            "file=a&file=b",
            "port=0",
            "port=x",
            "confirm=yes",
            "other=1",
            "file=%zz",
            "file=%c3",
            "file=%4",
        ] {
            assert!(parse_push_query(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn percent_round_trip() {
        for text in ["", "a", "a b/c?d&e=f%g", "é漢字", "-._~"] {
            assert_eq!(percent_decode(&percent_encode(text)).unwrap(), text);
        }
        assert_eq!(percent_encode("-._~aZ9"), "-._~aZ9");
        assert_eq!(percent_decode("a+b").unwrap(), "a+b");
    }

    #[test]
    fn push_and_deployment_view_shapes() {
        let view = OperationDeploymentView {
            deployment_id: "dep_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
            release_id: None,
            result: Some("failed".to_owned()),
            services: vec![ServiceOutcomeView {
                name: "web".to_owned(),
                status: "healthy".to_owned(),
                endpoints: vec![EndpointView {
                    provider: "port".to_owned(),
                    address: "192.0.2.10:8080".to_owned(),
                }],
            }],
        };
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(
            v,
            json!({
                "deployment_id": "dep_01aryz6s41tsv4rrffq69g5fav",
                "release_id": null,
                "result": "failed",
                "services": [{
                    "name": "web",
                    "status": "healthy",
                    "endpoints": [{ "provider": "port", "address": "192.0.2.10:8080" }],
                }],
            })
        );
        assert_eq!(
            serde_json::from_value::<OperationDeploymentView>(v).unwrap(),
            view
        );
    }

    #[test]
    fn upload_view_shape() {
        let v = UploadView {
            operation_id: "op_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
            project_id: "prj_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
            revision_id: "rev_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
            sha256: "ab".repeat(32),
            new_revision: true,
        };
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(
            json,
            json!({
                "operation_id": "op_01aryz6s41tsv4rrffq69g5fav",
                "project_id": "prj_01aryz6s41tsv4rrffq69g5fav",
                "revision_id": "rev_01aryz6s41tsv4rrffq69g5fav",
                "sha256": "ab".repeat(32),
                "new_revision": true,
            })
        );
        assert_eq!(serde_json::from_value::<UploadView>(json).unwrap(), v);
    }

    #[test]
    fn api_version_numbers() {
        assert_eq!(api_version_number(API_VERSION), Some(1));
        assert_eq!(api_version_number("v12"), Some(12));
        for bad in ["", "v", "1", "V1", "v1.0", "v-1", "v1 ", "vx"] {
            assert_eq!(api_version_number(bad), None, "{bad}");
        }
        assert!(
            API_VERSION_HEADER
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'-')
        );
        assert!(
            SHIP_VERSION_HEADER
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'-')
        );
    }

    #[test]
    fn host_info_round_trip() {
        let h = HostInfo {
            api_version: API_VERSION.to_owned(),
            version: "0.0.1".to_owned(),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            docker: DockerInfo {
                status: DockerStatus::Ready,
                engine_version: Some("28.3.2".to_owned()),
                compose_version: Some("2.39.1".to_owned()),
                supported: SupportedVersions {
                    engine: ">= 24.0.0, < 30.0.0".to_owned(),
                    compose: ">= 2.20.0, < 6.0.0".to_owned(),
                },
                message: None,
            },
            capacity: HostCapacity {
                cpus: Some(4),
                memory_bytes: Some(8_589_934_592),
                disk_free_bytes: None,
            },
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(
            v,
            json!({
                "api_version": "v1",
                "version": "0.0.1",
                "os": "linux",
                "arch": "x86_64",
                "docker": {
                    "status": "ready",
                    "engine_version": "28.3.2",
                    "compose_version": "2.39.1",
                    "supported": {
                        "engine": ">= 24.0.0, < 30.0.0",
                        "compose": ">= 2.20.0, < 6.0.0",
                    },
                    "message": null,
                },
                "capacity": {
                    "cpus": 4,
                    "memory_bytes": 8_589_934_592_u64,
                    "disk_free_bytes": null,
                },
            })
        );
        assert_eq!(serde_json::from_value::<HostInfo>(v).unwrap(), h);
    }

    #[test]
    fn docker_status_strings() {
        let all: Vec<&str> = DockerStatus::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(all, ["ready", "unreachable", "unsupported_version"]);
        for s in DockerStatus::ALL {
            assert_eq!(s.as_str().parse::<DockerStatus>().unwrap(), *s);
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.as_str()));
        }
        assert!("stopped".parse::<DockerStatus>().is_err());
    }

    #[test]
    fn project_list_round_trip() {
        let empty = serde_json::to_value(ProjectList::default()).unwrap();
        assert_eq!(empty, json!({ "projects": [] }));

        let list = ProjectList {
            projects: vec![ProjectSummary {
                id: "prj_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
                slug: Slug::parse("my-app").unwrap(),
                created_at: Timestamp::from_unix_millis(1_469_918_176_385).unwrap(),
            }],
        };
        let v = serde_json::to_value(&list).unwrap();
        assert_eq!(
            v,
            json!({ "projects": [{
                "id": "prj_01aryz6s41tsv4rrffq69g5fav",
                "slug": "my-app",
                "created_at": "2016-07-30T22:36:16.385Z",
            }]})
        );
        assert_eq!(serde_json::from_value::<ProjectList>(v).unwrap(), list);
    }

    #[test]
    fn operation_paths_and_headers() {
        let op: OperationId = "op_01aryz6s41tsv4rrffq69g5fav".parse().unwrap();
        assert!(OPERATIONS_PATH.starts_with(API_PREFIX));
        assert_eq!(
            operation_path(op),
            "/api/v1/operations/op_01aryz6s41tsv4rrffq69g5fav"
        );
        assert_eq!(
            operation_events_path(op),
            "/api/v1/operations/op_01aryz6s41tsv4rrffq69g5fav/events"
        );
        for h in [IDEMPOTENCY_KEY_HEADER, LAST_EVENT_ID_HEADER] {
            assert!(h.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'));
        }
    }

    #[test]
    fn idempotency_keys() {
        for ok in ["k", "push-2026-11-27T10:00:00Z", &"x".repeat(128)] {
            assert!(valid_idempotency_key(ok), "{ok}");
        }
        for bad in ["", " ", "a b", "tab\t", "é", &"x".repeat(129)] {
            assert!(!valid_idempotency_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn event_kind_strings() {
        let all: Vec<&str> = EventKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(all, ["phase_started", "log", "waiting", "finished"]);
    }

    #[test]
    fn operation_view_shape() {
        let view = OperationView {
            id: "op_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
            kind: "push".to_owned(),
            environment_id: Some("env_01aryz6s41tsv4rrffq69g5fav".parse().unwrap()),
            phase: OperationPhase::Failed,
            status: OperationStatus::Failed,
            failure: Some(OperationFailureView {
                code: ReceivedCode::Known(crate::ErrorCode::BuildFailed),
                phase: OperationPhase::Building,
                message: "the image build exited 1".to_owned(),
            }),
            log_bytes: 42,
            created_at: Timestamp::from_unix_millis(1_469_918_176_385).unwrap(),
            updated_at: Timestamp::from_unix_millis(1_469_918_176_386).unwrap(),
        };
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(
            v,
            json!({
                "id": "op_01aryz6s41tsv4rrffq69g5fav",
                "kind": "push",
                "environment_id": "env_01aryz6s41tsv4rrffq69g5fav",
                "phase": "failed",
                "status": "failed",
                "failure": {
                    "code": "build_failed",
                    "phase": "building",
                    "message": "the image build exited 1",
                },
                "log_bytes": 42,
                "created_at": "2016-07-30T22:36:16.385Z",
                "updated_at": "2016-07-30T22:36:16.386Z",
            })
        );
        assert_eq!(serde_json::from_value::<OperationView>(v).unwrap(), view);

        let open = OperationView {
            phase: OperationPhase::Queued,
            status: OperationStatus::Queued,
            failure: None,
            environment_id: None,
            ..view
        };
        let v = serde_json::to_value(&open).unwrap();
        assert_eq!(v["failure"], serde_json::Value::Null);
        assert_eq!(v["environment_id"], serde_json::Value::Null);
    }

    /// The event lines of the `ship push --wait` fixtures, less their
    /// `type` wrapper field, decode as [`OperationEventView`].
    #[test]
    fn operation_event_view_matches_the_push_fixture_fields() {
        let line = json!({
            "operation_id": "op_01aryz6s41tsv4rrffq69g5fav",
            "sequence": 4,
            "phase": "building",
            "event": "log",
            "message": "#5 DONE 12.1s",
            "ts": "2026-01-02T03:04:20.000Z",
        });
        let e: OperationEventView = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(e.sequence, 4);
        assert_eq!(e.event, EventKind::Log.as_str());
        assert_eq!(serde_json::to_value(&e).unwrap(), line);
    }

    #[test]
    fn project_list_rejects_invalid_members() {
        let bad_id = json!({ "projects": [{
            "id": "env_01aryz6s41tsv4rrffq69g5fav",
            "slug": "my-app",
            "created_at": "2016-07-30T22:36:16.385Z",
        }]});
        assert!(serde_json::from_value::<ProjectList>(bad_id).is_err());
        let bad_slug = json!({ "projects": [{
            "id": "prj_01aryz6s41tsv4rrffq69g5fav",
            "slug": "My App",
            "created_at": "2016-07-30T22:36:16.385Z",
        }]});
        assert!(serde_json::from_value::<ProjectList>(bad_slug).is_err());
    }

    #[test]
    fn config_paths_and_requests() {
        let p = Slug::parse("my-app").unwrap();
        assert_eq!(vars_path(&p), "/api/v1/projects/my-app/vars");
        assert_eq!(vars_unset_path(&p), "/api/v1/projects/my-app/vars/unset");
        assert_eq!(secrets_path(&p), "/api/v1/projects/my-app/secrets");
        assert_eq!(
            secrets_unset_path(&p),
            "/api/v1/projects/my-app/secrets/unset"
        );
        assert_eq!(config_query(None, None), "");
        assert_eq!(config_query(Some("development"), None), "?env=development");
        assert_eq!(
            config_query(Some("dev elopment"), Some("web")),
            "?env=dev%20elopment&service=web"
        );
        let request = SetSecretRequest {
            env: None,
            service: Some("web".to_owned()),
            name: "TOKEN".to_owned(),
            value: "hunter2-api-secret".to_owned(),
        };
        let debug = format!("{request:?} {request:#?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("TOKEN"), "{debug}");
        let back: SetSecretRequest =
            serde_json::from_value(json!({ "name": "TOKEN", "value": "v" })).unwrap();
        assert_eq!((back.env, back.service), (None, None));
    }
}
