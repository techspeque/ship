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

use ship_core::{
    ApiError, DockerInfo, DockerStatus, EnvironmentSummary, ErrorCode, HostCapacity, HostInfo,
    ProjectList, ProjectSummary, ServiceStatus, Slug, SupportedVersions, Timestamp,
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
