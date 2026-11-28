//! Lifecycle commands (phase-1-ws-1.12): `start`, `stop` and `restart` of
//! an environment or one of its services, and `redeploy` and `remove` of
//! an environment, each one operation on the environment's lock (ADR-0011)
//! and one audit event.
//!
//! `POST /api/v1/projects/{project}/{action}` with `env` and `service` in
//! the query ([`ship_core::api::lifecycle_path`]). A `service` on
//! `redeploy` or `remove` is refused `400` before any operation: a release
//! and a Compose project cover the whole environment (Human, 2026-11-28).
//! An unknown project, environment or service is `404`. Everything else is
//! decided inside the operation, on the environment as it is when the
//! operation runs, so an action queued behind another sees its result:
//!
//! - `start P`: the environment and every service desired `running`, after
//!   `compose start`. `start P/S`: that service, after `compose start S`;
//!   refused while the environment is stopped.
//! - `stop P`: the environment desired `stopped`, then `compose stop`.
//!   `stop P/S`: that service desired `stopped`, then `compose stop S`.
//!   The stop is recorded first, so even if Docker fails nothing in Ship
//!   starts what the operator stopped (ADR-0006 rule 6); status derives
//!   from it, so exit 137 or 143 is `stopped`, not `failed`.
//! - `restart P`: `compose restart` of the services desired `running`;
//!   `restart P/S`: of that one, refused when it or the environment is
//!   stopped. The release does not change (ADR-0006 rule 10).
//! - `redeploy P`: the serving deployment's revision deployed again with
//!   the options it was deployed with and the config stored now
//!   ([`crate::deploy::run`]).
//! - `remove P`: `compose down`, which never removes a volume; the
//!   environment's routes emptied; a `remove` deployment recorded in the
//!   history; desired `removed`. Volumes, history and the release files
//!   stay. An environment already removed is left as it is.
//!
//! On a removed environment `start`, `stop`, `restart` and `redeploy` fail
//! `conflict` and run nothing: it comes back only through
//! `ship push --recreate`.

use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::json;
use ship_core::api::{
    REDEPLOY_KIND, REMOVE_KIND, RESTART_KIND, START_KIND, STOP_KIND, is_environment_only,
};
use ship_core::{
    ApiError, DeploymentAction, DeploymentResult, DesiredState, EnvironmentId, ErrorCode,
    OperationPhase, ServiceId,
};
use ship_docker::{ComposeProject, DockerError, compose};
use ship_store::{Deployment, Environment, NewAuditEvent, NewDeployment, Project, Service, Store};

use crate::deploy::{DeployJob, Deployer, LogForward, docker_failure, recorded_options};
use crate::http::{accept, error_response};
use crate::queue::{Context, JobFailure, Queue};
use crate::settings::OPERATOR;

/// One lifecycle action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
    Redeploy,
    Remove,
}

impl Action {
    /// Its operation kind, audit action and path segment.
    pub fn kind(self) -> &'static str {
        match self {
            Action::Start => START_KIND,
            Action::Stop => STOP_KIND,
            Action::Restart => RESTART_KIND,
            Action::Redeploy => REDEPLOY_KIND,
            Action::Remove => REMOVE_KIND,
        }
    }
}

/// What the lifecycle handler needs from the daemon.
pub struct LifecycleContext<'a> {
    pub store: &'a Store,
    pub queue: &'a Queue,
    pub deployer: &'a std::sync::Arc<Deployer>,
}

/// The lifecycle handler. See the module documentation.
pub async fn handle(
    cx: LifecycleContext<'_>,
    action: Action,
    project: &str,
    query: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    let (env, service) = match crate::config::parse_query(query) {
        Ok(parsed) => parsed,
        Err(e) => return error_response(e),
    };
    if service.is_some() && is_environment_only(action.kind()) {
        return error_response(ApiError::new(
            ErrorCode::InvalidRequest,
            format!(
                "{} applies to a whole environment, not one service: a release and a Compose \
                 project cover every service; run it for the project",
                action.kind()
            ),
        ));
    }
    let env = env.unwrap_or_else(|| ship_store::DEVELOPMENT.to_owned());
    let scope =
        match crate::config::find_scope(cx.store, project, Some(&env), service.as_deref()).await {
            Ok(scope) => scope,
            Err(e) => return error_response(e),
        };
    let Some(environment) = scope.environment else {
        return error_response(ApiError::new(ErrorCode::Internal, "no environment"));
    };
    let target = Target {
        project: scope.project,
        environment: environment.id,
        service: scope.service.map(|s| s.id),
    };
    let deployer = std::sync::Arc::clone(cx.deployer);
    let job = move |ctx: Context| async move { run(&ctx, &deployer, action, &target).await };
    accept(cx.queue, headers, action.kind(), Some(environment.id), job).await
}

/// What an action applies to, by id; read again when it runs.
#[derive(Debug, Clone)]
struct Target {
    project: Project,
    environment: EnvironmentId,
    service: Option<ServiceId>,
}

/// Runs `action` on `target` inside its operation.
async fn run(
    ctx: &Context,
    d: &Deployer,
    action: Action,
    target: &Target,
) -> Result<(), JobFailure> {
    let store = d.store();
    let env = store.get_environment(target.environment).await?;
    let service = match target.service {
        Some(id) => Some(store.get_service(id).await?),
        None => None,
    };
    if env.desired_state == DesiredState::Removed {
        if action == Action::Remove {
            ctx.log("the environment is already removed; nothing changes")
                .await?;
            return Ok(());
        }
        return Err(JobFailure::new(
            ErrorCode::Conflict,
            format!(
                "the environment was removed, so there is nothing to {}; bring it back with \
                 `ship push --recreate`",
                action.kind()
            ),
        ));
    }
    let project = ComposeProject::new(env.id);
    match (action, &service) {
        (Action::Start, None) => start_environment(ctx, d, &env, &project).await?,
        (Action::Start, Some(s)) => start_service(ctx, d, &env, s, &project).await?,
        (Action::Stop, None) => stop_environment(ctx, d, &env, &project).await?,
        (Action::Stop, Some(s)) => stop_service(ctx, d, s, &project).await?,
        (Action::Restart, s) => restart(ctx, d, &env, s.as_ref(), &project).await?,
        (Action::Redeploy, _) => redeploy(ctx, d, &target.project, &env).await?,
        (Action::Remove, _) => remove(ctx, d, &env, &project).await?,
    }
    audit(store, action, &env, service.as_ref()).await
}

/// One audit event for `action` (OVERVIEW §2.2): the service's id when it
/// names one, else the environment's.
async fn audit(
    store: &Store,
    action: Action,
    env: &Environment,
    service: Option<&Service>,
) -> Result<(), JobFailure> {
    let target = service.map_or_else(|| env.id.to_string(), |s| s.id.to_string());
    let detail = json!({
        "environment": env.name.as_str(),
        "service": service.map(|s| s.name.as_str()),
    })
    .to_string();
    store
        .append_audit_event(&NewAuditEvent {
            actor: OPERATOR,
            action: action.kind(),
            target: &target,
            detail: Some(&detail),
        })
        .await?;
    Ok(())
}

/// The deployment serving now: the latest that succeeded, if it is a
/// deploy (a removal serves nothing).
fn serving(history: &[Deployment]) -> Option<&Deployment> {
    history
        .iter()
        .find(|x| x.result == Some(DeploymentResult::Succeeded))
        .filter(|x| x.action == DeploymentAction::Deploy)
}

fn nothing_deployed() -> JobFailure {
    JobFailure::new(
        ErrorCode::Conflict,
        "nothing is deployed in this environment yet; push it first",
    )
}

/// A Compose step on the environment's project.
#[derive(Debug, Clone, Copy)]
enum Step {
    Start,
    Stop,
    Restart,
    Down,
}

/// Runs `step` for `services` (every one when empty) with its output in
/// the operation's log; a failure is the operation's, naming `what`.
async fn step(
    ctx: &Context,
    d: &Deployer,
    project: &ComposeProject,
    step: Step,
    services: &[String],
    what: &str,
) -> Result<(), JobFailure> {
    let forward = LogForward::start(ctx);
    let timeout = crate::deploy::step_timeout(d);
    let result: Result<(), DockerError> = {
        let mut sink = forward.sink();
        let docker = d.docker();
        match step {
            Step::Start => compose::start(docker, project, services, timeout, &mut sink).await,
            Step::Stop => compose::stop(docker, project, services, timeout, &mut sink).await,
            Step::Restart => compose::restart(docker, project, services, timeout, &mut sink).await,
            Step::Down => compose::down(docker, project, timeout, &mut sink).await,
        }
    };
    forward.finish().await?;
    result.map_err(|e| docker_failure(e, ErrorCode::DeployFailed, what))
}

async fn start_environment(
    ctx: &Context,
    d: &Deployer,
    env: &Environment,
    project: &ComposeProject,
) -> Result<(), JobFailure> {
    let store = d.store();
    if serving(&store.list_deployments(env.id).await?).is_none() {
        return Err(nothing_deployed());
    }
    ctx.enter(OperationPhase::Deploying, "starting every service")
        .await?;
    step(ctx, d, project, Step::Start, &[], "starting the services").await?;
    store
        .set_desired_state(env.id, DesiredState::Running)
        .await?;
    store.set_services_running(env.id).await?;
    ctx.log("the environment and every service are desired running")
        .await
}

async fn start_service(
    ctx: &Context,
    d: &Deployer,
    env: &Environment,
    service: &Service,
    project: &ComposeProject,
) -> Result<(), JobFailure> {
    if env.desired_state == DesiredState::Stopped {
        return Err(JobFailure::new(
            ErrorCode::Conflict,
            format!(
                "the environment is stopped, which wins over {}; start the project instead",
                service.name
            ),
        ));
    }
    ctx.enter(
        OperationPhase::Deploying,
        &format!("starting {}", service.name),
    )
    .await?;
    let names = [service.name.clone()];
    step(
        ctx,
        d,
        project,
        Step::Start,
        &names,
        &format!("starting {}", service.name),
    )
    .await?;
    d.store()
        .set_service_desired_state(service.id, DesiredState::Running)
        .await?;
    ctx.log(&format!("{} is desired running", service.name))
        .await
}

async fn stop_environment(
    ctx: &Context,
    d: &Deployer,
    env: &Environment,
    project: &ComposeProject,
) -> Result<(), JobFailure> {
    ctx.enter(OperationPhase::Deploying, "stopping every service")
        .await?;
    // Recorded first: whatever Docker does next, nothing in Ship starts it
    // again until `ship start` (OVERVIEW §3.3).
    d.store()
        .set_desired_state(env.id, DesiredState::Stopped)
        .await?;
    ctx.log("the environment is desired stopped; nothing in Ship starts it until `ship start`")
        .await?;
    step(ctx, d, project, Step::Stop, &[], "stopping the services").await
}

async fn stop_service(
    ctx: &Context,
    d: &Deployer,
    service: &Service,
    project: &ComposeProject,
) -> Result<(), JobFailure> {
    ctx.enter(
        OperationPhase::Deploying,
        &format!("stopping {}", service.name),
    )
    .await?;
    d.store()
        .set_service_desired_state(service.id, DesiredState::Stopped)
        .await?;
    ctx.log(&format!(
        "{} is desired stopped; nothing in Ship starts it until `ship start`",
        service.name
    ))
    .await?;
    let names = [service.name.clone()];
    step(
        ctx,
        d,
        project,
        Step::Stop,
        &names,
        &format!("stopping {}", service.name),
    )
    .await
}

async fn restart(
    ctx: &Context,
    d: &Deployer,
    env: &Environment,
    service: Option<&Service>,
    project: &ComposeProject,
) -> Result<(), JobFailure> {
    if env.desired_state == DesiredState::Stopped {
        return Err(JobFailure::new(
            ErrorCode::Conflict,
            "the environment is stopped; start it with `ship start`",
        ));
    }
    let store = d.store();
    if serving(&store.list_deployments(env.id).await?).is_none() {
        return Err(nothing_deployed());
    }
    let names: Vec<String> = match service {
        Some(s) if s.desired_state == DesiredState::Stopped => {
            return Err(JobFailure::new(
                ErrorCode::Conflict,
                format!("{} is stopped; start it with `ship start`", s.name),
            ));
        }
        Some(s) => vec![s.name.clone()],
        None => store
            .list_services(env.id)
            .await?
            .into_iter()
            .filter(|s| s.desired_state == DesiredState::Running)
            .map(|s| s.name)
            .collect(),
    };
    if names.is_empty() {
        ctx.log("every service is stopped; nothing to restart")
            .await?;
        return Ok(());
    }
    ctx.enter(
        OperationPhase::Deploying,
        &format!("restarting {} on the release serving now", names.join(", ")),
    )
    .await?;
    step(ctx, d, project, Step::Restart, &names, "restarting").await
}

async fn redeploy(
    ctx: &Context,
    d: &Deployer,
    project: &Project,
    env: &Environment,
) -> Result<(), JobFailure> {
    let store = d.store();
    let history = store.list_deployments(env.id).await?;
    let serving = serving(&history).ok_or_else(nothing_deployed)?;
    let (file, port) = recorded_options(serving.options.as_deref())?;
    let revision = store.get_revision(serving.revision_id).await?;
    ctx.log(&format!(
        "deploying revision {} again with the config stored now",
        revision.id
    ))
    .await?;
    let job = DeployJob {
        project: project.clone(),
        environment: env.clone(),
        revision,
        file,
        port,
        confirm: false,
        recreate: false,
    };
    crate::deploy::run(ctx, d, &job).await.map(|_| ())
}

async fn remove(
    ctx: &Context,
    d: &Deployer,
    env: &Environment,
    project: &ComposeProject,
) -> Result<(), JobFailure> {
    let store = d.store();
    ctx.enter(
        OperationPhase::Deploying,
        "removing the containers and networks; every volume and the history are kept",
    )
    .await?;
    step(ctx, d, project, Step::Down, &[], "removing the containers").await?;
    store.replace_routes(env.id, &[]).await?;
    let history = store.list_deployments(env.id).await?;
    match history.first() {
        Some(latest) => {
            let removal = store
                .record_removal(&NewDeployment {
                    environment_id: env.id,
                    revision_id: latest.revision_id,
                    operation_id: ctx.operation_id(),
                    previous_release_id: serving(&history).and_then(|x| x.release_id),
                })
                .await?;
            ctx.log(&format!("the removal is deployment {}", removal.id))
                .await?;
        }
        None => {
            ctx.log("the environment had no deployment").await?;
        }
    }
    store
        .set_desired_state(env.id, DesiredState::Removed)
        .await?;
    ctx.log("the environment is removed; `ship push --recreate` brings it back with its volumes")
        .await
}
