//! Preflight against a real Docker Engine. Ignored by `make verify`; the CI
//! `docker` job runs these with `--ignored`.

use std::process::Command;

use ship_core::DockerStatus;
use ship_docker::supported::{COMPOSE, ENGINE};
use ship_docker::{Runner, Version, preflight};

/// Every container, image, network and volume id, by kind.
fn docker_objects() -> Vec<(&'static str, Vec<String>)> {
    let lists: [(&str, &[&str]); 4] = [
        ("containers", &["ps", "--all", "--quiet", "--no-trunc"]),
        ("images", &["images", "--all", "--quiet", "--no-trunc"]),
        ("networks", &["network", "ls", "--quiet", "--no-trunc"]),
        ("volumes", &["volume", "ls", "--quiet"]),
    ];
    lists
        .iter()
        .map(|(kind, args)| {
            let out = Command::new("docker").args(*args).output().unwrap();
            assert!(
                out.status.success(),
                "docker {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let mut ids: Vec<String> = String::from_utf8(out.stdout)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect();
            ids.sort();
            (*kind, ids)
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs a real Docker Engine; run by the CI docker job"]
async fn real_engine_is_ready_and_in_range() {
    let info = preflight(&Runner::docker()).await;
    eprintln!("preflight: {info:?}");
    assert_eq!(info.status, DockerStatus::Ready, "{info:?}");
    let engine = Version::parse(info.engine_version.as_deref().unwrap()).unwrap();
    let compose = Version::parse(info.compose_version.as_deref().unwrap()).unwrap();
    assert!(ENGINE.contains(engine), "{engine} not in {ENGINE}");
    assert!(COMPOSE.contains(compose), "{compose} not in {COMPOSE}");
}

#[tokio::test]
#[ignore = "needs a real Docker Engine; run by the CI docker job"]
async fn preflight_changes_no_docker_object() {
    let before = docker_objects();
    for _ in 0..3 {
        preflight(&Runner::docker()).await;
    }
    let after = docker_objects();
    assert_eq!(before, after);
}
