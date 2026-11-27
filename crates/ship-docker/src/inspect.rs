//! What Docker reports about Ship's containers, read from `container
//! inspect` into [`Observation`]s (ADR-0006 rule 2).

use std::collections::BTreeMap;

use serde_json::Value;
use ship_core::{ContainerState, HealthCheck, Observation};

use crate::argv::{container_ids, container_inspect};
use crate::error::DockerError;
use crate::labels::{COMPOSE_SERVICE_LABEL, Selector, ShipLabels};
use crate::runner::Runner;

/// One of Ship's containers as Docker reports it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerObservation {
    /// The full container id.
    pub id: String,
    /// The container name, without Docker's leading `/`.
    pub name: String,
    /// The Compose service it belongs to, from Compose's label.
    pub compose_service: Option<String>,
    /// Ship's labels; `service` is always present for a container.
    pub labels: ShipLabels,
    /// Always [`Observation::Reachable`]: an unreachable Docker is an
    /// error, not an observation of a container.
    pub observation: Observation,
    /// How many times Docker has restarted it under its restart policy.
    pub restart_count: u64,
}

/// The containers carrying Ship's labels for `selector`: listed by label
/// with `docker ps`, read with `container inspect`, and checked again on
/// what comes back, so an object without the labels is never returned.
pub async fn containers(
    runner: &Runner,
    selector: &Selector,
) -> Result<Vec<ContainerObservation>, DockerError> {
    let argv = container_ids(selector)?;
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
    let ids = parse_ids(&out.stdout_text())?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let json = inspect_json(runner, &container_inspect(&ids)?).await?;
    parse_inspect(&json, selector)
}

/// Runs an `inspect` argv. A container removed between listing and
/// inspecting makes Docker exit non-zero while still printing the others;
/// that output is used.
pub(crate) async fn inspect_json(
    runner: &Runner,
    argv: &crate::argv::Argv,
) -> Result<Vec<u8>, DockerError> {
    let out = runner
        .run(argv.args())
        .await
        .map_err(DockerError::from_run)?;
    if out.success() {
        return Ok(out.stdout);
    }
    let stderr = out.stderr_text();
    let partial = !crate::error::is_unreachable_message(&stderr)
        && stderr.contains("No such")
        && serde_json::from_slice::<Vec<Value>>(&out.stdout).is_ok();
    if partial {
        Ok(out.stdout)
    } else {
        Err(DockerError::from_exit(argv.command(), out.status, &stderr))
    }
}

/// Container or network ids, one per line; each must be a Docker object id.
pub(crate) fn parse_ids(text: &str) -> Result<Vec<String>, DockerError> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            if crate::argv::is_object_id(l) {
                Ok(l.to_owned())
            } else {
                Err(DockerError::Malformed(format!(
                    "`{}` is not an object id",
                    l.chars().take(80).collect::<String>()
                )))
            }
        })
        .collect()
}

/// A label map from an inspect object's `Labels` value (`null` is empty).
pub(crate) fn label_map(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// Parses `docker container inspect` output, keeping only containers whose
/// Ship labels are complete, name a service, and match `selector`.
pub fn parse_inspect(
    json: &[u8],
    selector: &Selector,
) -> Result<Vec<ContainerObservation>, DockerError> {
    let items: Vec<Value> = serde_json::from_slice(json)
        .map_err(|e| DockerError::Malformed(format!("container inspect: {e}")))?;
    let mut out = Vec::new();
    for item in &items {
        let raw = label_map(item.pointer("/Config/Labels"));
        let Some(labels) = ShipLabels::parse(&raw) else {
            continue;
        };
        if labels.service.is_none() || !selector.matches(&labels) {
            continue;
        }
        out.push(parse_container(item, labels, &raw)?);
    }
    Ok(out)
}

fn parse_container(
    item: &Value,
    labels: ShipLabels,
    raw: &BTreeMap<String, String>,
) -> Result<ContainerObservation, DockerError> {
    let malformed = |what: &str| DockerError::Malformed(format!("container inspect: {what}"));
    let id = item
        .get("Id")
        .and_then(Value::as_str)
        .filter(|s| crate::argv::is_object_id(s))
        .ok_or_else(|| malformed("missing Id"))?
        .to_owned();
    let name = item
        .get("Name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_owned();
    let state = item
        .get("State")
        .ok_or_else(|| malformed("missing State"))?;
    let status = state
        .get("Status")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed("missing State.Status"))?;
    let exit_code = state.get("ExitCode").and_then(Value::as_i64).unwrap_or(0);
    let container = match status {
        "created" => ContainerState::Created,
        "running" => ContainerState::Running,
        "paused" => ContainerState::Paused,
        "restarting" => ContainerState::Restarting,
        // Being removed: its process has already ended.
        "exited" | "removing" => ContainerState::Exited { exit_code },
        "dead" => ContainerState::Dead,
        other => {
            return Err(malformed(&format!(
                "unknown State.Status `{}`",
                other.chars().take(40).collect::<String>()
            )));
        }
    };
    let health = match state.pointer("/Health/Status").and_then(Value::as_str) {
        None | Some("none") => HealthCheck::None,
        Some("starting") => HealthCheck::Starting,
        Some("healthy") => HealthCheck::Passing,
        Some("unhealthy") => HealthCheck::Failing,
        Some(other) => {
            return Err(malformed(&format!(
                "unknown health status `{}`",
                other.chars().take(40).collect::<String>()
            )));
        }
    };
    let oom_killed = state
        .get("OOMKilled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let restart_count = item
        .get("RestartCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(ContainerObservation {
        id,
        name,
        compose_service: raw.get(COMPOSE_SERVICE_LABEL).cloned(),
        labels,
        observation: Observation::Reachable {
            container,
            health,
            oom_killed,
        },
        restart_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::tests::ids;
    use serde_json::json;
    use ship_core::{EnvironmentId, ServiceId};

    const ID: &str = "4f9a1c0d2e3b4a5968778695a4b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5";

    fn labels_json(env: EnvironmentId, service: Option<ServiceId>) -> Value {
        let (p, _, _, r) = ids();
        json!({
            "dev.ship.project": p.to_string(),
            "dev.ship.environment": env.to_string(),
            "dev.ship.service": service.map(|s| s.to_string()).unwrap_or_default(),
            "dev.ship.release": r.to_string(),
            "com.docker.compose.service": "web",
        })
    }

    fn container(state: Value, labels: Value) -> Value {
        json!({
            "Id": ID,
            "Name": "/ship_env-web-1",
            "RestartCount": 3,
            "State": state,
            "Config": { "Labels": labels },
        })
    }

    fn observe_one(state: Value) -> ContainerObservation {
        let (_, e, s, _) = ids();
        let doc = json!([container(state, labels_json(e, Some(s)))]);
        let mut v = parse_inspect(doc.to_string().as_bytes(), &Selector::environment(e)).unwrap();
        assert_eq!(v.len(), 1);
        v.remove(0)
    }

    fn reachable(c: ContainerState, h: HealthCheck, oom: bool) -> Observation {
        Observation::Reachable {
            container: c,
            health: h,
            oom_killed: oom,
        }
    }

    #[test]
    fn states() {
        let cases = [
            (
                json!({"Status": "created", "ExitCode": 0}),
                ContainerState::Created,
            ),
            (
                json!({"Status": "running", "ExitCode": 0}),
                ContainerState::Running,
            ),
            (
                json!({"Status": "paused", "ExitCode": 0}),
                ContainerState::Paused,
            ),
            (
                json!({"Status": "restarting", "ExitCode": 1}),
                ContainerState::Restarting,
            ),
            (
                json!({"Status": "exited", "ExitCode": 137}),
                ContainerState::Exited { exit_code: 137 },
            ),
            (
                json!({"Status": "removing", "ExitCode": 143}),
                ContainerState::Exited { exit_code: 143 },
            ),
            (
                json!({"Status": "dead", "ExitCode": 0}),
                ContainerState::Dead,
            ),
        ];
        for (state, expected) in cases {
            let got = observe_one(state.clone());
            assert_eq!(
                got.observation,
                reachable(expected, HealthCheck::None, false),
                "{state}"
            );
        }
    }

    #[test]
    fn health_and_oom_and_restarts() {
        let health = [
            ("starting", HealthCheck::Starting),
            ("healthy", HealthCheck::Passing),
            ("unhealthy", HealthCheck::Failing),
            ("none", HealthCheck::None),
        ];
        for (s, expected) in health {
            let got = observe_one(json!({"Status": "running", "Health": {"Status": s}}));
            assert_eq!(
                got.observation,
                reachable(ContainerState::Running, expected, false),
                "{s}"
            );
        }
        let got = observe_one(json!({"Status": "exited", "ExitCode": 137, "OOMKilled": true}));
        assert_eq!(
            got.observation,
            reachable(
                ContainerState::Exited { exit_code: 137 },
                HealthCheck::None,
                true
            )
        );
        assert_eq!(got.restart_count, 3);
        assert_eq!(got.id, ID);
        assert_eq!(got.name, "ship_env-web-1");
        assert_eq!(got.compose_service.as_deref(), Some("web"));
        assert_eq!(got.labels.service, Some(ids().2));
    }

    #[test]
    fn unlabelled_and_foreign_containers_are_dropped() {
        let (_, e, s, _) = ids();
        let other_env = EnvironmentId::from_parts(5, 5).unwrap();
        let running = json!({"Status": "running"});
        let doc = json!([
            container(
                running.clone(),
                json!({"com.docker.compose.project": "ship_x"})
            ),
            container(running.clone(), Value::Null),
            container(running.clone(), labels_json(other_env, Some(s))),
            container(running.clone(), labels_json(e, None)),
            container(running.clone(), labels_json(e, Some(s))),
        ]);
        let got = parse_inspect(doc.to_string().as_bytes(), &Selector::environment(e)).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].labels.environment, e);

        let other_service = ServiceId::from_parts(9, 9).unwrap();
        let narrow = Selector::environment(e).service(other_service);
        assert!(
            parse_inspect(doc.to_string().as_bytes(), &narrow)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn malformed_output_is_an_error() {
        let e = ids().1;
        let sel = Selector::environment(e);
        assert!(matches!(
            parse_inspect(b"not json", &sel),
            Err(DockerError::Malformed(_))
        ));
        let doc = json!([container(
            json!({"Status": "weird"}),
            labels_json(e, Some(ids().2))
        )]);
        assert!(matches!(
            parse_inspect(doc.to_string().as_bytes(), &sel),
            Err(DockerError::Malformed(_))
        ));
        assert!(parse_inspect(b"[]", &sel).unwrap().is_empty());
    }

    #[test]
    fn ids_are_checked() {
        assert_eq!(parse_ids(&format!("{ID}\n\n")).unwrap(), [ID]);
        assert!(parse_ids("").unwrap().is_empty());
        assert!(parse_ids("--volumes").is_err());
    }
}
