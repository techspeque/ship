//! Wire types shared by the client and the daemon.
//!
//! Shapes are provisional until Phase 5 (ADR-0004 rule 4).

use serde::{Deserialize, Serialize};

use crate::id::ProjectId;
use crate::slug::Slug;
use crate::time::Timestamp;

/// The API version this build speaks.
pub const API_VERSION: &str = "v1";
/// Path prefix of every versioned endpoint.
pub const API_PREFIX: &str = "/api/v1";
/// `GET`: [`HostInfo`].
pub const HOST_PATH: &str = "/api/v1/host";
/// `GET`: [`ProjectList`].
pub const PROJECTS_PATH: &str = "/api/v1/projects";

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
