//! Wire types shared by the client and the daemon.
//!
//! Shapes are provisional until Phase 5 (ADR-0004 rule 4).

use serde::{Deserialize, Serialize};

use crate::deploy::PortSpec;
use crate::error::ReceivedCode;
use crate::id::{EnvironmentId, OperationId, ProjectId, RevisionId};
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

/// `GET` and `POST` path of the setting `key`.
pub fn setting_path(key: &str) -> String {
    format!("{SETTINGS_PATH}/{key}")
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
}
