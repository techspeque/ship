//! The Rust half of the UI's type drift guard.
//!
//! Struct-literal samples of every type the UI reads, covering every field
//! and every [`DockerStatus`], and the list of [`ServiceStatus`] values, are
//! serialised and compared byte for byte
//! with the fixtures in `web/src/lib/contract/`. `web/src/lib/contract.test.ts`
//! runs the UI's runtime guards over the same files. So a changed Rust type
//! fails here until the fixtures are regenerated, and the regenerated
//! fixtures fail the TS test until `web/src/lib/api.ts` matches.
//!
//! Struct literals (no `..Default::default()`) make a new field a compile
//! error here, so it cannot be left out of the fixtures by accident.
//!
//! `SHIP_UPDATE_CONTRACT=1 cargo test -p ship-daemon --test web_contract`
//! rewrites the fixtures.

use std::path::{Path, PathBuf};

use ship_core::api::{
    DeploymentFailureView, DeploymentList, DeploymentView, EndpointView, EnvironmentView,
    LastDeploymentView, LogLineView, OperationEventView, OperationFailureView, OperationView,
    ProjectRef, SecretList, SecretView, ServiceStatusView, StatusView, VarList, VarView,
};
use ship_core::{
    ApiError, ConfigScope, DeploymentAction, DeploymentResult, DesiredState, DockerInfo,
    DockerStatus, EnvironmentSummary, ErrorCode, HostCapacity, HostInfo, OperationPhase,
    OperationStatus, ProjectList, ProjectSummary, ReceivedCode, ServiceStatus, Slug,
    SupportedVersions, Timestamp,
};

fn contract_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/lib/contract")
}

fn supported() -> SupportedVersions {
    SupportedVersions {
        engine: ">= 24.0.0, < 30.0.0".to_owned(),
        compose: ">= 2.20.0, < 6.0.0".to_owned(),
    }
}

fn host(docker: DockerInfo, capacity: HostCapacity) -> HostInfo {
    HostInfo {
        api_version: "v1".to_owned(),
        version: "0.0.1".to_owned(),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        docker,
        capacity,
    }
}

/// One sample per Docker status; the `match` makes a new status a compile
/// error until it has a sample.
fn host_sample(status: DockerStatus) -> (&'static str, HostInfo) {
    match status {
        DockerStatus::Ready => (
            "host-ready",
            host(
                DockerInfo {
                    status,
                    engine_version: Some("28.3.2".to_owned()),
                    compose_version: Some("2.39.1".to_owned()),
                    supported: supported(),
                    message: None,
                },
                HostCapacity {
                    cpus: Some(4),
                    memory_bytes: Some(8_589_934_592),
                    disk_free_bytes: Some(107_374_182_400),
                },
            ),
        ),
        DockerStatus::Unreachable => (
            "host-unreachable",
            host(
                DockerInfo {
                    status,
                    engine_version: None,
                    compose_version: None,
                    supported: supported(),
                    message: Some(
                        "the Docker Engine did not answer; start it with \
                         `sudo systemctl start docker`"
                            .to_owned(),
                    ),
                },
                HostCapacity {
                    cpus: None,
                    memory_bytes: None,
                    disk_free_bytes: None,
                },
            ),
        ),
        DockerStatus::UnsupportedVersion => (
            "host-unsupported-version",
            host(
                DockerInfo {
                    status,
                    engine_version: Some("23.0.1".to_owned()),
                    compose_version: None,
                    supported: supported(),
                    message: Some("Docker Engine 23.0.1 is older than 24.0.0".to_owned()),
                },
                HostCapacity {
                    cpus: Some(1),
                    memory_bytes: Some(1_073_741_824),
                    disk_free_bytes: None,
                },
            ),
        ),
    }
}

fn ts(millis: i64) -> Timestamp {
    Timestamp::from_unix_millis(millis).unwrap()
}

fn slug() -> Slug {
    Slug::parse("my-app").unwrap()
}

const OP: &str = "op_01aryz6s41tsv4rrffq69g5fav";
const OP_FAILED: &str = "op_01aryz6s41tsv4rrffq69g5faw";
const ENV: &str = "env_01aryz6s41tsv4rrffq69g5fav";
const REL: &str = "rel_01aryz6s41tsv4rrffq69g5fav";
const REL_OLD: &str = "rel_01aryz6s41tsv4rrffq69g5faw";
const REV: &str = "rev_01aryz6s41tsv4rrffq69g5fav";

/// `ship status`: a deployed environment with a healthy service and one
/// stopped on purpose with stale config, and one never deployed.
fn status_samples() -> Vec<(&'static str, StatusView)> {
    let project = ProjectRef {
        id: "prj_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
        slug: slug(),
    };
    let environment = EnvironmentView {
        id: ENV.parse().unwrap(),
        name: "development".to_owned(),
        desired_state: "running".to_owned(),
    };
    vec![
        (
            "status-deployed",
            StatusView {
                project: project.clone(),
                environment: environment.clone(),
                last_deployment: Some(LastDeploymentView {
                    id: "dep_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
                    action: "deploy".to_owned(),
                    release_id: Some(REL.parse().unwrap()),
                    operation_id: OP.parse().unwrap(),
                    result: Some("succeeded".to_owned()),
                    started_at: ts(1_767_323_045_000),
                    ended_at: Some(ts(1_767_323_085_000)),
                }),
                services: vec![
                    ServiceStatusView {
                        id: "svc_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
                        name: "web".to_owned(),
                        kind: "web".to_owned(),
                        desired_state: "running".to_owned(),
                        status: "healthy".to_owned(),
                        restarts: 0,
                        observed_at: ts(1_767_323_100_000),
                        stale_config: false,
                        endpoints: vec![EndpointView {
                            provider: "port".to_owned(),
                            address: "192.0.2.10:8080".to_owned(),
                        }],
                    },
                    ServiceStatusView {
                        id: "svc_01aryz6s41tsv4rrffq69g5faw".parse().unwrap(),
                        name: "worker".to_owned(),
                        kind: "worker".to_owned(),
                        desired_state: "stopped".to_owned(),
                        status: "stopped".to_owned(),
                        restarts: 2,
                        observed_at: ts(1_767_323_100_000),
                        stale_config: true,
                        endpoints: vec![],
                    },
                ],
            },
        ),
        (
            "status-never-deployed",
            StatusView {
                project,
                environment,
                last_deployment: None,
                services: vec![],
            },
        ),
    ]
}

/// One deployment of the history; its id and its operation's end in the
/// letter `suffix`.
fn deployment(
    suffix: char,
    action: &str,
    release: Option<&str>,
    result: Option<&str>,
    failure: Option<DeploymentFailureView>,
    started: i64,
    ended: Option<i64>,
) -> DeploymentView {
    let id = |prefix: &str| format!("{prefix}_01aryz6s41tsv4rrffq69g5fa{suffix}");
    DeploymentView {
        id: id("dep").parse().unwrap(),
        environment: "development".to_owned(),
        action: action.to_owned(),
        revision_id: REV.parse().unwrap(),
        release_id: release.map(|r| r.parse().unwrap()),
        previous_release_id: Some(REL_OLD.parse().unwrap()),
        operation_id: id("op").parse().unwrap(),
        result: result.map(str::to_owned),
        failure,
        started_at: ts(started),
        ended_at: ended.map(ts),
    }
}

/// `ship deployments`, newest first: one in progress, a removal, a failed
/// deployment with its phase, and the one that served.
fn deployments_sample() -> DeploymentList {
    DeploymentList {
        deployments: vec![
            deployment('y', "deploy", None, None, None, 1_767_323_400_000, None),
            deployment(
                'x',
                "remove",
                None,
                Some("succeeded"),
                None,
                1_767_323_300_000,
                Some(1_767_323_302_000),
            ),
            deployment(
                'w',
                "deploy",
                None,
                Some("failed"),
                Some(DeploymentFailureView {
                    code: ReceivedCode::Known(ErrorCode::BuildFailed),
                    phase: "building".to_owned(),
                    message: "the build exited 1".to_owned(),
                }),
                1_767_323_200_000,
                Some(1_767_323_230_000),
            ),
            deployment(
                'v',
                "deploy",
                Some(REL),
                Some("succeeded"),
                None,
                1_767_323_045_000,
                Some(1_767_323_085_000),
            ),
        ],
    }
}

/// An operation running, and one failed.
fn operation_samples() -> Vec<(&'static str, OperationView)> {
    vec![
        (
            "operation-running",
            OperationView {
                id: OP.parse().unwrap(),
                kind: "restart".to_owned(),
                environment_id: Some(ENV.parse().unwrap()),
                phase: OperationPhase::Deploying,
                status: OperationStatus::Running,
                failure: None,
                log_bytes: 120,
                created_at: ts(1_767_323_045_000),
                updated_at: ts(1_767_323_046_000),
            },
        ),
        (
            "operation-failed",
            OperationView {
                id: OP_FAILED.parse().unwrap(),
                kind: "deploy".to_owned(),
                environment_id: None,
                phase: OperationPhase::Failed,
                status: OperationStatus::Failed,
                failure: Some(OperationFailureView {
                    code: ReceivedCode::Known(ErrorCode::BuildFailed),
                    phase: OperationPhase::Building,
                    message: "the build exited 1".to_owned(),
                }),
                log_bytes: 0,
                created_at: ts(1_767_323_200_000),
                updated_at: ts(1_767_323_230_000),
            },
        ),
    ]
}

fn event_sample() -> OperationEventView {
    OperationEventView {
        operation_id: OP.parse().unwrap(),
        sequence: 3,
        phase: OperationPhase::Building,
        event: "log".to_owned(),
        message: "Step 1/4 : FROM alpine".to_owned(),
        ts: ts(1_767_323_046_000),
    }
}

fn log_line_sample() -> LogLineView {
    LogLineView {
        service: "web".to_owned(),
        stream: "stderr".to_owned(),
        ts: ts(1_767_323_087_003),
        line: "warning: no cache directory".to_owned(),
    }
}

/// Config values of the project's scope and of a service's, and secrets
/// of the environment's.
fn config_samples() -> Vec<(String, String)> {
    vec![
        (
            "vars-project".to_owned(),
            pretty(&VarList {
                scope: ConfigScope::Project,
                project: slug(),
                environment: None,
                service: None,
                vars: vec![VarView {
                    name: "LOG_LEVEL".to_owned(),
                    value: "debug".to_owned(),
                    updated_at: ts(1_767_323_000_000),
                }],
                stale_services: vec!["web".to_owned()],
            }),
        ),
        (
            "vars-service".to_owned(),
            pretty(&VarList {
                scope: ConfigScope::Service,
                project: slug(),
                environment: Some("development".to_owned()),
                service: Some("web".to_owned()),
                vars: vec![],
                stale_services: vec![],
            }),
        ),
        (
            "secrets-environment".to_owned(),
            pretty(&SecretList {
                scope: ConfigScope::Environment,
                project: slug(),
                environment: Some("development".to_owned()),
                service: None,
                secrets: vec![SecretView {
                    name: "DATABASE_URL".to_owned(),
                    updated_at: ts(1_767_323_010_000),
                }],
                stale_services: vec![],
            }),
        ),
    ]
}

/// The strings of an enum's values, in declaration order.
fn names<T>(all: &[T], as_str: fn(&T) -> &'static str) -> String {
    pretty(&all.iter().map(as_str).collect::<Vec<_>>())
}

/// Pretty JSON with a trailing newline, keys in declaration order.
fn pretty(v: &impl serde::Serialize) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("sample serialises");
    s.push('\n');
    s
}

fn samples() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = DockerStatus::ALL
        .iter()
        .map(|s| {
            let (name, h) = host_sample(*s);
            (name.to_owned(), pretty(&h))
        })
        .collect();
    out.push((
        "docker-statuses".to_owned(),
        pretty(
            &DockerStatus::ALL
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
        ),
    ));
    out.push((
        "service-statuses".to_owned(),
        pretty(
            &ServiceStatus::ALL
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
        ),
    ));
    out.push(("projects-empty".to_owned(), pretty(&ProjectList::default())));
    out.push((
        "projects-one".to_owned(),
        pretty(&ProjectList {
            projects: vec![ProjectSummary {
                id: "prj_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
                slug: Slug::parse("my-app").unwrap(),
                created_at: Timestamp::from_unix_millis(1_469_918_176_385).unwrap(),
                environments: vec![EnvironmentSummary {
                    id: "env_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
                    name: "development".to_owned(),
                    desired_state: "running".to_owned(),
                    status: Some("running_unverified".to_owned()),
                }],
            }],
        }),
    ));
    for (name, status) in status_samples() {
        out.push((name.to_owned(), pretty(&status)));
    }
    out.push(("deployments".to_owned(), pretty(&deployments_sample())));
    for (name, op) in operation_samples() {
        out.push((name.to_owned(), pretty(&op)));
    }
    out.push(("operation-event".to_owned(), pretty(&event_sample())));
    out.push(("log-line".to_owned(), pretty(&log_line_sample())));
    out.extend(config_samples());
    out.push((
        "operation-phases".to_owned(),
        names(OperationPhase::ALL, |p| p.as_str()),
    ));
    out.push((
        "operation-statuses".to_owned(),
        names(OperationStatus::ALL, |s| s.as_str()),
    ));
    out.push((
        "desired-states".to_owned(),
        names(DesiredState::ALL, |s| s.as_str()),
    ));
    out.push((
        "deployment-results".to_owned(),
        names(DeploymentResult::ALL, |r| r.as_str()),
    ));
    out.push((
        "deployment-actions".to_owned(),
        names(DeploymentAction::ALL, |a| a.as_str()),
    ));
    out.push((
        "event-kinds".to_owned(),
        names(ship_core::api::EventKind::ALL, |k| k.as_str()),
    ));
    out.push((
        "error-not-found".to_owned(),
        pretty(&ApiError::new(ErrorCode::NotFound, "no such endpoint")),
    ));
    out
}

#[test]
fn fixtures_match_the_rust_types() {
    let dir = contract_dir();
    let update = std::env::var_os("SHIP_UPDATE_CONTRACT").is_some_and(|v| v == "1");
    let samples = samples();

    if update {
        std::fs::create_dir_all(&dir).unwrap();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "json") {
                std::fs::remove_file(path).unwrap();
            }
        }
        for (name, json) in &samples {
            std::fs::write(dir.join(format!("{name}.json")), json).unwrap();
        }
        return;
    }

    let mut failures = Vec::new();
    for (name, json) in &samples {
        let path = dir.join(format!("{name}.json"));
        match std::fs::read_to_string(&path) {
            Ok(on_disk) if on_disk == *json => {}
            Ok(on_disk) => failures.push(format!(
                "{name}.json differs from the Rust type\n--- committed\n{on_disk}--- serialised\n{json}"
            )),
            Err(e) => failures.push(format!("{}: {e}", path.display())),
        }
    }
    // A fixture with no sample would keep testing a shape Rust no longer has.
    let expected: Vec<String> = samples.iter().map(|(n, _)| format!("{n}.json")).collect();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name.ends_with(".json") && !expected.contains(&name) {
            failures.push(format!("{name} has no Rust sample"));
        }
    }
    assert!(
        failures.is_empty(),
        "{}\n\nIf the change is intended, run \
         `SHIP_UPDATE_CONTRACT=1 cargo test -p ship-daemon --test web_contract`, \
         then update web/src/lib/api.ts until `npm test` in web/ passes.",
        failures.join("\n\n")
    );
}

#[test]
fn every_docker_status_has_a_host_fixture() {
    let names: Vec<&str> = DockerStatus::ALL
        .iter()
        .map(|s| host_sample(*s).0)
        .collect();
    assert_eq!(names.len(), DockerStatus::ALL.len());
    for (s, name) in DockerStatus::ALL.iter().zip(&names) {
        assert_eq!(host_sample(*s).1.docker.status, *s, "{name}");
    }
}
