//! Observation of an environment's services (ADR-0006): what Docker
//! reports for Ship's labelled containers, recorded apart from desired
//! state and deployment results.
//!
//! Observing only reads: one labelled `ps` and a `container inspect`. It
//! never runs a command that changes anything, and it never touches
//! desired state, a deployment or a route. Docker unreachable, or any other
//! failure to read it, is recorded as unreachable for every service, so
//! each shows `unknown` (rule 4), and nothing else happens.

use std::collections::BTreeMap;
use std::time::Duration;

use ship_core::{ContainerState, DesiredState, HealthCheck, Observation, ServiceId, ServiceStatus};
use ship_docker::{ContainerObservation, Runner, Selector, containers};
use ship_store::{Environment, NewObservation, Service, Store, StoreError, StoredObservation};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Default time between two observations of every environment.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

/// One service of an environment as just observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub service: Service,
    pub stored: StoredObservation,
}

impl Observed {
    /// The status the observation gives under what is asked of the
    /// service: `environment`'s desired state when it is not `running`,
    /// else the service's own ([`DesiredState::effective`]).
    pub fn status(&self, environment: &Environment) -> ServiceStatus {
        ServiceStatus::from_observation(
            self.stored.observation,
            desired(environment, &self.service),
        )
    }
}

/// What is asked of `service` of `environment`.
pub fn desired(environment: &Environment, service: &Service) -> DesiredState {
    DesiredState::effective(environment.desired_state, service.desired_state)
}

/// What no container means: the service is not running.
const ABSENT: Observation = Observation::Reachable {
    container: ContainerState::Absent,
    health: HealthCheck::None,
    oom_killed: false,
};

/// Each service's observation from what Docker listed: the container of
/// least severity under `environment`'s desired state (so a service is
/// `healthy` only when every container is), restarts summed over them,
/// absent when there is none. `None` from Docker means it could not be
/// read: every service is unreachable.
pub fn per_service(
    environment: &Environment,
    services: &[Service],
    found: Option<&[ContainerObservation]>,
) -> Vec<NewObservation> {
    let Some(found) = found else {
        return services
            .iter()
            .map(|s| NewObservation {
                service_id: s.id,
                observation: Observation::DockerUnreachable,
                restarts: 0,
            })
            .collect();
    };
    let mut by_service: BTreeMap<ServiceId, Vec<&ContainerObservation>> = BTreeMap::new();
    for c in found {
        if let Some(id) = c.labels.service {
            by_service.entry(id).or_default().push(c);
        }
    }
    services
        .iter()
        .map(|s| {
            let ours = by_service.get(&s.id).map(Vec::as_slice).unwrap_or(&[]);
            let worst = ours.iter().min_by_key(|c| {
                ServiceStatus::from_observation(c.observation, desired(environment, s)).severity()
            });
            NewObservation {
                service_id: s.id,
                observation: worst.map_or(ABSENT, |c| c.observation),
                restarts: ours.iter().map(|c| c.restart_count).sum(),
            }
        })
        .collect()
}

/// Observes `environment`'s services now and records the observations.
pub async fn observe(
    store: &Store,
    docker: &Runner,
    environment: &Environment,
) -> Result<Vec<Observed>, StoreError> {
    let services = store.list_services(environment.id).await?;
    if services.is_empty() {
        return Ok(Vec::new());
    }
    let found = match containers(docker, &Selector::environment(environment.id)).await {
        Ok(found) => Some(found),
        Err(e) => {
            tracing::warn!(
                environment = %environment.id,
                error = %e,
                "could not observe the environment; its services are unknown"
            );
            None
        }
    };
    let new = per_service(environment, &services, found.as_deref());
    let stored = store.record_observations(&new).await?;
    Ok(services
        .into_iter()
        .zip(stored)
        .map(|(service, stored)| Observed { service, stored })
        .collect())
}

/// Observes every environment of every project once.
pub async fn observe_all(store: &Store, docker: &Runner) -> Result<(), StoreError> {
    for project in store.list_projects().await? {
        for environment in store.list_environments(project.id).await? {
            observe(store, docker, &environment).await?;
        }
    }
    Ok(())
}

/// Observes every environment every `interval` until `closing` is set or
/// dropped. A failed round is logged and the next one runs as usual.
pub fn spawn_observer(
    store: Store,
    docker: Runner,
    interval: Duration,
    mut closing: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                changed = closing.wait_for(|c| *c) => {
                    let _ = changed;
                    return;
                }
            }
            if let Err(e) = observe_all(&store, &docker).await {
                tracing::error!(error = %e, "observing the environments failed");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ship_core::{
        DesiredState, EnvironmentId, ProjectId, RecoveryPolicy, ReleaseId, ServiceKind, Slug,
        Timestamp,
    };
    use ship_docker::ShipLabels;

    fn environment(desired: DesiredState) -> Environment {
        Environment {
            id: EnvironmentId::from_parts(1, 1).unwrap(),
            project_id: ProjectId::from_parts(1, 1).unwrap(),
            name: Slug::parse("development").unwrap(),
            desired_state: desired,
            recovery_policy: RecoveryPolicy::ObserveOnly,
            created_at: Timestamp::from_unix_millis(0).unwrap(),
        }
    }

    fn service(n: u128) -> Service {
        Service {
            id: ServiceId::from_parts(1, n).unwrap(),
            environment_id: EnvironmentId::from_parts(1, 1).unwrap(),
            name: format!("s{n}"),
            kind: ServiceKind::Web,
            port: None,
            desired_state: DesiredState::Running,
            created_at: Timestamp::from_unix_millis(0).unwrap(),
        }
    }

    fn container(
        service: ServiceId,
        observation: Observation,
        restarts: u64,
    ) -> ContainerObservation {
        ContainerObservation {
            id: "a".repeat(64),
            name: "c".to_owned(),
            compose_service: None,
            labels: ShipLabels {
                project: ProjectId::from_parts(1, 1).unwrap(),
                environment: EnvironmentId::from_parts(1, 1).unwrap(),
                service: Some(service),
                release: ReleaseId::from_parts(1, 1).unwrap(),
            },
            observation,
            restart_count: restarts,
        }
    }

    fn running(health: HealthCheck) -> Observation {
        Observation::Reachable {
            container: ContainerState::Running,
            health,
            oom_killed: false,
        }
    }

    #[test]
    fn unreachable_docker_makes_every_service_unreachable() {
        let env = environment(DesiredState::Running);
        let services = [service(1), service(2)];
        let got = per_service(&env, &services, None);
        assert_eq!(got.len(), 2);
        assert!(
            got.iter()
                .all(|o| o.observation == Observation::DockerUnreachable)
        );
    }

    #[test]
    fn worst_container_wins_and_restarts_add_up() {
        let env = environment(DesiredState::Running);
        let services = [service(1), service(2), service(3)];
        let found = [
            container(services[0].id, running(HealthCheck::Passing), 1),
            container(services[0].id, running(HealthCheck::None), 2),
            container(services[1].id, running(HealthCheck::Passing), 0),
        ];
        let got = per_service(&env, &services, Some(&found));
        assert_eq!(got[0].observation, running(HealthCheck::None));
        assert_eq!(got[0].restarts, 3);
        assert_eq!(got[1].observation, running(HealthCheck::Passing));
        // No container: absent, so `stopped`, never `unknown`.
        assert_eq!(got[2].observation, ABSENT);
        assert_eq!(
            ServiceStatus::from_observation(got[2].observation, env.desired_state),
            ServiceStatus::Stopped
        );
    }

    #[test]
    fn a_service_stopped_on_purpose_is_stopped_not_failed() {
        let exited = Observation::Reachable {
            container: ContainerState::Exited { exit_code: 143 },
            health: HealthCheck::None,
            oom_killed: false,
        };
        let stored = |service: Service| Observed {
            service,
            stored: StoredObservation {
                service_id: ServiceId::from_parts(1, 1).unwrap(),
                observation: exited,
                restarts: 0,
                observed_at: Timestamp::from_unix_millis(0).unwrap(),
            },
        };
        let mut stopped = service(1);
        stopped.desired_state = DesiredState::Stopped;
        let running_env = environment(DesiredState::Running);
        assert_eq!(
            stored(stopped.clone()).status(&running_env),
            ServiceStatus::Stopped
        );
        assert_eq!(
            stored(service(2)).status(&running_env),
            ServiceStatus::Failed
        );
        // The environment's stop wins over a service desired running.
        let stopped_env = environment(DesiredState::Stopped);
        assert_eq!(
            stored(service(2)).status(&stopped_env),
            ServiceStatus::Stopped
        );
    }
}
