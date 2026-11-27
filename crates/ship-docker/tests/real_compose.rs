//! The Compose adapter against a real Docker Engine. Ignored by
//! `make verify`; the CI `docker` job runs these with `--ignored`.
//!
//! The test talks to Docker through `std::process::Command` only to look
//! at objects and to clean up after itself; everything under test goes
//! through the adapter.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ship_core::{
    ContainerState, DesiredState, EnvironmentId, HealthCheck, Observation, ProjectId, ReleaseId,
    ServiceId, ServiceStatus,
};
use ship_docker::compose::{self, ComposeConfig, DeployLabels, labels_override};
use ship_docker::labels::{ENVIRONMENT_LABEL, ShipLabels};
use ship_docker::{
    ComposeFiles, ComposeProject, ContainerObservation, DockerError, Line, LogOptions, Runner,
    Selector, Stream, UpOptions, containers,
};

const LONG: Duration = Duration::from_secs(600);
const NO: &[&str] = &[];

fn entropy() -> (u64, u128) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let ms = u64::try_from(now.as_millis()).unwrap();
    let e = (u128::from(std::process::id()) << 32) | u128::from(now.subsec_nanos());
    (ms, e)
}

struct Ids {
    project: ProjectId,
    environment: EnvironmentId,
    web: ServiceId,
    side: ServiceId,
    release: ReleaseId,
}

fn ids() -> Ids {
    let (ms, e) = entropy();
    Ids {
        project: ProjectId::from_parts(ms, e).unwrap(),
        environment: EnvironmentId::from_parts(ms, e + 1).unwrap(),
        web: ServiceId::from_parts(ms, e + 2).unwrap(),
        side: ServiceId::from_parts(ms, e + 3).unwrap(),
        release: ReleaseId::from_parts(ms, e + 4).unwrap(),
    }
}

fn docker(args: &[&str]) -> std::process::Output {
    Command::new("docker").args(args).output().unwrap()
}

fn docker_ok(args: &[&str]) -> String {
    let out = docker(args);
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Removes everything carrying the Compose project's label, whatever the
/// test left, including volumes; test cleanup only.
struct Cleanup(String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let filter = format!("label=com.docker.compose.project={}", self.0);
        for (ls, rm) in [
            (&["ps", "-aq", "--filter"][..], &["rm", "-f"][..]),
            (
                &["network", "ls", "-q", "--filter"][..],
                &["network", "rm"][..],
            ),
            (
                &["volume", "ls", "-q", "--filter"][..],
                &["volume", "rm", "-f"][..],
            ),
        ] {
            let mut args = ls.to_vec();
            args.push(&filter);
            let out = docker(&args);
            let ids = String::from_utf8_lossy(&out.stdout).into_owned();
            let ids: Vec<&str> = ids.split_whitespace().collect();
            if !ids.is_empty() {
                let mut args = rm.to_vec();
                args.extend(ids);
                let _ = docker(&args);
            }
        }
        let _ = docker(&["image", "rm", "-f", &format!("{}-web", self.0)]);
    }
}

/// The labels of every object of `kind` in the Compose project.
fn project_labels(kind: &str, project: &str) -> Vec<BTreeMap<String, String>> {
    let filter = format!("label=com.docker.compose.project={project}");
    let filter = filter.as_str();
    let (ls, inspect, format): (Vec<&str>, Vec<&str>, &str) = match kind {
        "container" => (
            vec!["ps", "-aq", "--no-trunc", "--filter", filter],
            vec!["container", "inspect", "--format"],
            "{{json .Config.Labels}}",
        ),
        "network" => (
            vec!["network", "ls", "-q", "--no-trunc", "--filter", filter],
            vec!["network", "inspect", "--format"],
            "{{json .Labels}}",
        ),
        "volume" => (
            vec!["volume", "ls", "-q", "--filter", filter],
            vec!["volume", "inspect", "--format"],
            "{{json .Labels}}",
        ),
        other => panic!("{other}"),
    };
    let list = docker_ok(&ls);
    let ids: Vec<&str> = list.split_whitespace().collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let mut args = inspect;
    args.push(format);
    args.extend(ids);
    docker_ok(&args)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn write_project(dir: &Path, token: &str) {
    std::fs::write(
        dir.join("Dockerfile"),
        format!("FROM busybox:1.36\nRUN echo build-line-{token}\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        r#"services:
  web:
    build: .
    command: ["sh", "-c", "echo out-line; echo err-line >&2; exec sleep 3600"]
    stop_grace_period: 1s
    healthcheck:
      test: ["CMD", "true"]
      interval: 1s
      timeout: 2s
      retries: 3
    volumes:
      - data:/data
    networks: [back]
  side:
    image: busybox:1.36
    command: ["sleep", "3600"]
    stop_grace_period: 1s
    networks: [back]
volumes:
  data: {}
networks:
  back: {}
"#,
    )
    .unwrap();
}

async fn wait_for(
    runner: &Runner,
    selector: &Selector,
    what: &str,
    ok: impl Fn(&[ContainerObservation]) -> bool,
) -> Vec<ContainerObservation> {
    let start = Instant::now();
    loop {
        let seen = containers(runner, selector).await.unwrap();
        if ok(&seen) {
            return seen;
        }
        assert!(
            start.elapsed() < Duration::from_secs(90),
            "timed out waiting for {what}: {seen:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn state(c: &ContainerObservation) -> (ContainerState, HealthCheck) {
    match c.observation {
        Observation::Reachable {
            container, health, ..
        } => (container, health),
        Observation::DockerUnreachable => panic!("not an observation of a container"),
    }
}

#[tokio::test]
#[ignore = "needs a real Docker Engine; run by the CI docker job"]
async fn compose_project_lifecycle_is_labelled_and_keeps_volumes() {
    let ids = ids();
    let runner = Runner::docker().timeout(Duration::from_secs(60));
    let project = ComposeProject::new(ids.environment);
    let _cleanup = Cleanup(project.name().to_owned());
    let src = tempfile::tempdir().unwrap();
    let generated = tempfile::tempdir().unwrap();
    write_project(src.path(), &ids.environment.to_string());

    // config → override
    let base = ComposeFiles::new(src.path(), [src.path().join("compose.yaml")]).unwrap();
    let config: ComposeConfig = compose::config(&runner, &project, &base).await.unwrap();
    assert_eq!(config.service_names(), ["side", "web"]);
    let labels = DeployLabels {
        project: ids.project,
        environment: ids.environment,
        release: ids.release,
        services: [("web".to_owned(), ids.web), ("side".to_owned(), ids.side)].into(),
        shared_release: ids.release,
    };
    let override_path = generated.path().join("ship.override.json");
    std::fs::write(&override_path, labels_override(&config, &labels).unwrap()).unwrap();
    let files = ComposeFiles::new(
        src.path(),
        [src.path().join("compose.yaml"), override_path.clone()],
    )
    .unwrap();

    // build: output arrives line by line.
    let mut build_lines: Vec<Line> = Vec::new();
    compose::build(&runner, &project, &files, NO, LONG, &mut |l| {
        build_lines.push(l)
    })
    .await
    .unwrap();
    let token = format!("build-line-{}", ids.environment);
    assert!(
        build_lines.iter().any(|l| l.text.contains(&token)),
        "{build_lines:?}"
    );

    // up: every container, network and volume carries the four labels.
    compose::up(
        &runner,
        &project,
        &files,
        NO,
        UpOptions::default(),
        LONG,
        &mut |_| {},
    )
    .await
    .unwrap();
    for kind in ["container", "network", "volume"] {
        let all = project_labels(kind, project.name());
        assert!(!all.is_empty(), "no {kind}");
        for l in &all {
            let ship =
                ShipLabels::parse(l).unwrap_or_else(|| panic!("{kind} without labels: {l:?}"));
            assert_eq!(ship.project, ids.project, "{kind}");
            assert_eq!(ship.environment, ids.environment, "{kind}");
            assert_eq!(ship.release, ids.release, "{kind}");
            if kind == "container" {
                assert!(
                    ship.service == Some(ids.web) || ship.service == Some(ids.side),
                    "{l:?}"
                );
            } else {
                assert_eq!(ship.service, None, "{kind}");
            }
        }
    }
    let image = docker_ok(&[
        "image",
        "inspect",
        "--format",
        "{{json .Config.Labels}}",
        &format!("{}-web", project.name()),
    ]);
    let image: BTreeMap<String, String> = serde_json::from_str(image.trim()).unwrap();
    assert_eq!(
        ShipLabels::parse(&image).and_then(|l| l.service),
        Some(ids.web)
    );

    // containers: Ship's, read into observations.
    let env = Selector::environment(ids.environment);
    let seen = wait_for(&runner, &env, "web healthy", |cs| {
        cs.len() == 2
            && cs.iter().any(|c| {
                c.labels.service == Some(ids.web)
                    && state(c) == (ContainerState::Running, HealthCheck::Passing)
            })
    })
    .await;
    let side = seen
        .iter()
        .find(|c| c.labels.service == Some(ids.side))
        .unwrap();
    assert_eq!(side.compose_service.as_deref(), Some("side"));
    assert_eq!(state(side), (ContainerState::Running, HealthCheck::None));
    assert_eq!(
        ServiceStatus::from_observation(side.observation, DesiredState::Running),
        ServiceStatus::RunningUnverified
    );
    assert_eq!(side.restart_count, 0);
    let web_only = env.service(ids.web);
    assert_eq!(containers(&runner, &web_only).await.unwrap().len(), 1);
    assert_eq!(compose::ps(&runner, &project).await.unwrap().len(), 2);

    // logs: stdout and stderr apart.
    let mut lines: Vec<Line> = Vec::new();
    compose::logs(
        &runner,
        &project,
        &["web"],
        LogOptions::default(),
        Some(Duration::from_secs(30)),
        &mut |l| lines.push(l),
    )
    .await
    .unwrap();
    assert!(
        lines
            .iter()
            .any(|l| l.stream == Stream::Stdout && l.text.contains("out-line")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.stream == Stream::Stderr && l.text.contains("err-line")),
        "{lines:?}"
    );

    // stop: exited under desired `stopped` is `stopped`, whatever the code.
    compose::stop(&runner, &project, NO, LONG, &mut |_| {})
        .await
        .unwrap();
    let stopped = wait_for(&runner, &env, "both exited", |cs| {
        cs.len() == 2
            && cs
                .iter()
                .all(|c| matches!(state(c).0, ContainerState::Exited { .. }))
    })
    .await;
    for c in &stopped {
        assert_eq!(
            ServiceStatus::from_observation(c.observation, DesiredState::Stopped),
            ServiceStatus::Stopped,
            "{c:?}"
        );
    }

    // start, restart.
    compose::start(&runner, &project, NO, LONG, &mut |_| {})
        .await
        .unwrap();
    compose::restart(&runner, &project, &["side"], LONG, &mut |_| {})
        .await
        .unwrap();
    wait_for(&runner, &env, "both running", |cs| {
        cs.len() == 2 && cs.iter().all(|c| state(c).0 == ContainerState::Running)
    })
    .await;

    // down: containers and networks go, the named volume stays.
    compose::down(&runner, &project, LONG, &mut |_| {})
        .await
        .unwrap();
    assert!(containers(&runner, &env).await.unwrap().is_empty());
    assert!(project_labels("container", project.name()).is_empty());
    assert!(project_labels("network", project.name()).is_empty());
    let volumes = project_labels("volume", project.name());
    assert_eq!(volumes.len(), 1, "{volumes:?}");
    assert_eq!(
        volumes[0].get(ENVIRONMENT_LABEL),
        Some(&ids.environment.to_string())
    );
}

#[tokio::test]
#[ignore = "needs a real Docker Engine; run by the CI docker job"]
async fn an_unlabelled_container_in_the_project_is_never_touched() {
    let ids = ids();
    let runner = Runner::docker().timeout(Duration::from_secs(60));
    let project = ComposeProject::new(ids.environment);
    let _cleanup = Cleanup(project.name().to_owned());
    let _ = docker(&["pull", "--quiet", "busybox:1.36"]);

    // Looks like a Compose container of this project, without Ship's labels.
    let foreign = docker_ok(&[
        "create",
        "--label",
        &format!("com.docker.compose.project={}", project.name()),
        "--label",
        "com.docker.compose.service=web",
        "--label",
        "com.docker.compose.oneoff=False",
        "busybox:1.36",
        "sleep",
        "3600",
    ]);
    let foreign = foreign.trim().to_owned();
    docker_ok(&["start", &foreign]);

    let refused = |e: DockerError| match e {
        DockerError::Foreign { kind, id } => {
            assert_eq!(kind, "container");
            assert_eq!(id, foreign);
        }
        other => panic!("{other:?}"),
    };
    refused(
        compose::stop(&runner, &project, NO, LONG, &mut |_| {})
            .await
            .unwrap_err(),
    );
    refused(
        compose::restart(&runner, &project, NO, LONG, &mut |_| {})
            .await
            .unwrap_err(),
    );
    refused(
        compose::down(&runner, &project, LONG, &mut |_| {})
            .await
            .unwrap_err(),
    );

    // Never returned as one of Ship's.
    let env = Selector::environment(ids.environment);
    assert!(containers(&runner, &env).await.unwrap().is_empty());

    // Still there and still running.
    let status = docker_ok(&["inspect", "--format", "{{.State.Status}}", &foreign]);
    assert_eq!(status.trim(), "running");
}
