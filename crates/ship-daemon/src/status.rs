//! `ship status`, `ship deployments` and `ship projects` as the API serves
//! them (ADR-0006): desired state from the store, the last deployment from
//! the history, and what Docker reports now, kept apart.

use std::path::Path;

use ship_core::api::{
    DeploymentFailureView, DeploymentList, DeploymentView, EndpointView, EnvironmentView,
    LastDeploymentView, ProjectRef, ServiceStatusView, StatusView,
};
use ship_core::{
    ApiError, DeploymentResult, EnvironmentSummary, ErrorCode, ProjectList, ProjectSummary,
    ReceivedCode, RouteStatus, ServiceStatus,
};
use ship_docker::Runner;
use ship_store::{Deployment, Environment, Project, Store, StoreError};

use crate::observe::observe;

/// The deployment `ship status` reports: the latest that succeeded, which
/// is the one serving, so a later failure leaves it visible; else the
/// latest of any result; `None` when there is none. `deployments` is
/// newest first.
pub fn reported(deployments: &[Deployment]) -> Option<&Deployment> {
    deployments
        .iter()
        .find(|d| d.result == Some(DeploymentResult::Succeeded))
        .or_else(|| deployments.first())
}

/// The status of `environment` of `project`: its services observed now
/// (and recorded), each with the endpoints of its confirmed routes that
/// answer now and whether its config is stale.
pub async fn status_view(
    store: &Store,
    docker: &Runner,
    state_dir: &Path,
    project: &Project,
    environment: &Environment,
) -> Result<StatusView, StoreError> {
    let deployments = store.list_deployments(environment.id).await?;
    let last_deployment = reported(&deployments).map(|d| LastDeploymentView {
        id: d.id,
        release_id: d.release_id,
        operation_id: d.operation_id,
        result: d.result.map(|r| r.as_str().to_owned()),
        started_at: d.started_at,
        ended_at: d.ended_at,
    });
    let observed = observe(store, docker, environment).await?;
    let stale = crate::config::stale_services(store, state_dir, environment).await?;
    let routes = store.list_routes(environment.id).await?;
    let mut services = Vec::with_capacity(observed.len());
    for o in &observed {
        let (service, stored) = (&o.service, o.stored);
        let status = o.status(environment);
        let mut endpoints = Vec::new();
        // Only a confirmed route is ever shown (ADR-0010 rule 2), and only
        // while it answers (OVERVIEW §3.3); an unknown service shows none.
        if status != ServiceStatus::Unknown {
            for route in routes
                .iter()
                .filter(|r| r.service_id == service.id && r.status == RouteStatus::Confirmed)
            {
                if crate::push::answers(&route.address).await {
                    endpoints.push(EndpointView {
                        provider: route.provider.as_str().to_owned(),
                        address: route.address.clone(),
                    });
                }
            }
        }
        services.push(ServiceStatusView {
            id: service.id,
            name: service.name.clone(),
            kind: service.kind.as_str().to_owned(),
            status: status.as_str().to_owned(),
            restarts: stored.restarts,
            observed_at: stored.observed_at,
            stale_config: stale.contains(&service.name),
            endpoints,
        });
    }
    Ok(StatusView {
        project: ProjectRef {
            id: project.id,
            slug: project.slug.clone(),
        },
        environment: EnvironmentView {
            id: environment.id,
            name: environment.name.as_str().to_owned(),
            desired_state: environment.desired_state.as_str().to_owned(),
        },
        last_deployment,
        services,
    })
}

/// Every deployment of `environment`, newest first, each that did not
/// succeed with its operation's failure.
pub async fn deployment_list(
    store: &Store,
    environment: &Environment,
) -> Result<DeploymentList, StoreError> {
    let mut deployments = Vec::new();
    for d in store.list_deployments(environment.id).await? {
        let failure = match d.result {
            Some(DeploymentResult::Succeeded) => None,
            _ => {
                let op = store.get_operation(d.operation_id).await?;
                match (op.failure_code, op.failure_message, op.failure_phase) {
                    (Some(code), Some(message), Some(phase)) => Some(DeploymentFailureView {
                        code: ReceivedCode::from(code),
                        phase: phase.as_str().to_owned(),
                        message,
                    }),
                    _ => None,
                }
            }
        };
        deployments.push(DeploymentView {
            id: d.id,
            environment: environment.name.as_str().to_owned(),
            revision_id: d.revision_id,
            release_id: d.release_id,
            previous_release_id: d.previous_release_id,
            operation_id: d.operation_id,
            result: d.result.map(|r| r.as_str().to_owned()),
            failure,
            started_at: d.started_at,
            ended_at: d.ended_at,
        });
    }
    Ok(DeploymentList { deployments })
}

/// Every project with its environments, each with the overall status of
/// its services observed now (and recorded).
pub async fn project_list(store: &Store, docker: &Runner) -> Result<ProjectList, StoreError> {
    let mut projects = Vec::new();
    for p in store.list_projects().await? {
        let mut environments = Vec::new();
        for e in store.list_environments(p.id).await? {
            let observed = observe(store, docker, &e).await?;
            let status = ServiceStatus::overall(observed.iter().map(|o| o.status(&e)));
            environments.push(EnvironmentSummary {
                id: e.id,
                name: e.name.as_str().to_owned(),
                desired_state: e.desired_state.as_str().to_owned(),
                status: status.map(|s| s.as_str().to_owned()),
            });
        }
        projects.push(ProjectSummary {
            id: p.id,
            slug: p.slug,
            created_at: p.created_at,
            environments,
        });
    }
    Ok(ProjectList { projects })
}

/// `env` from a status or deployments query (`development` by default);
/// any other parameter is `400`.
pub fn env_of(query: Option<&str>) -> Result<String, ApiError> {
    let (env, service) = crate::config::parse_query(query)?;
    if service.is_some() {
        return Err(ApiError::new(
            ErrorCode::InvalidRequest,
            "the query takes env only",
        ));
    }
    Ok(env.unwrap_or_else(|| ship_store::DEVELOPMENT.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ship_core::{DeploymentId, EnvironmentId, OperationId, RevisionId, Timestamp};

    fn deployment(n: u128, result: Option<DeploymentResult>) -> Deployment {
        Deployment {
            id: DeploymentId::from_parts(1, n).unwrap(),
            environment_id: EnvironmentId::from_parts(1, 1).unwrap(),
            revision_id: RevisionId::from_parts(1, n).unwrap(),
            release_id: None,
            operation_id: OperationId::from_parts(1, n).unwrap(),
            result,
            previous_release_id: None,
            started_at: Timestamp::from_unix_millis(0).unwrap(),
            ended_at: None,
        }
    }

    #[test]
    fn the_serving_deployment_is_reported() {
        use DeploymentResult::*;
        assert!(reported(&[]).is_none());
        // Newest first: a failure after a success leaves the success.
        let list = [
            deployment(3, Some(Failed)),
            deployment(2, Some(Succeeded)),
            deployment(1, Some(Succeeded)),
        ];
        assert_eq!(reported(&list).unwrap().id, list[1].id);
        // Nothing succeeded yet: the latest, whatever its result.
        let list = [deployment(2, None), deployment(1, Some(Failed))];
        assert_eq!(reported(&list).unwrap().id, list[0].id);
    }

    #[test]
    fn status_queries_take_env_only() {
        assert_eq!(env_of(None).unwrap(), "development");
        assert_eq!(env_of(Some("env=staging")).unwrap(), "staging");
        assert!(env_of(Some("service=web")).is_err());
        assert!(env_of(Some("other=1")).is_err());
    }
}
