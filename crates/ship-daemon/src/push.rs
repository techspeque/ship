//! `ship push` on the daemon (phase-1-ws-1.9): one `push` operation that
//! receives a snapshot ([`crate::upload::receive`]) and deploys it
//! ([`crate::deploy::run`]), so the client follows one operation from the
//! first byte to the verified release; and the read of the deployment an
//! operation recorded, which the client's result line is built from.
//!
//! - `POST /api/v1/projects/{project}/pushes`: the snapshot is the body,
//!   with the upload headers; the query carries `file`, `port` and
//!   `confirm` ([`ship_core::api::parse_push_query`]). A project seen for
//!   the first time is created by its own `project` operation, so nothing
//!   is written outside an operation (ADR-0011 rule 1); the `push`
//!   operation then runs on the project's `development` environment,
//!   holding its lock. The answer, once the snapshot is stored, is `202`
//!   with a [`PushView`]; the deployment goes on in the operation.
//! - `GET /api/v1/operations/{id}/deployment`: [`deployment_of`].

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use ship_core::api::{
    EndpointView, IDEMPOTENCY_KEY_HEADER, OperationDeploymentView, PROJECT_KIND, PUSH_KIND,
    PushOptions, PushView, ServiceOutcomeView, parse_push_query,
};
use ship_core::{
    ApiError, DeploymentResult, ErrorCode, OperationId, OperationPhase, OperationStatus,
    ServiceStatus,
};
use ship_docker::{Runner, Selector, containers};
use ship_store::{DEVELOPMENT, Operation, Project, Store};
use tokio::sync::oneshot;

use crate::deploy::{DeployJob, Deployer};
use crate::http::{error_response, operation_view, submitted_response};
use crate::queue::{Context, JobFailure, Queue, QueueError, Submission};
use crate::upload::{UploadRequest, Uploads};

/// How long one endpoint gets to accept a TCP connection before it is
/// left out of the answer.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// How often a wait for an operation's end re-reads the store.
const POLL: Duration = Duration::from_secs(1);

/// What the pushes handler needs from the daemon.
pub struct PushContext<'a> {
    pub store: &'a Store,
    pub queue: &'a Queue,
    pub uploads: &'a Arc<Uploads>,
    pub deployer: &'a Arc<Deployer>,
}

fn invalid(message: impl Into<String>) -> Response {
    error_response(ApiError::new(ErrorCode::InvalidRequest, message))
}

fn internal(operation: Option<OperationId>) -> Response {
    let e = ApiError::new(
        ErrorCode::Internal,
        "the daemon could not read its database",
    );
    error_response(match operation {
        Some(op) => e.with_operation(op),
        None => e,
    })
}

/// The pushes handler. See the module documentation.
pub async fn handle(
    cx: PushContext<'_>,
    project: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let request = match crate::upload::parse_request(project, headers) {
        Ok(request) => request,
        Err(e) => return error_response(e),
    };
    let options = match parse_push_query(query) {
        Ok(options) => options,
        Err(why) => return invalid(why),
    };
    if let Some(file) = &options.file
        && let Err(e) = ship_core::deploy::check_file_path(file)
    {
        return invalid(e.to_string());
    }
    let Some(key) = headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|k| ship_core::api::valid_idempotency_key(k))
    else {
        return error_response(QueueError::InvalidKey.to_api());
    };
    let project = match ensure_project(&cx, &request, key).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let environment = match cx.store.list_environments(project.id).await {
        Ok(list) => match list.into_iter().find(|e| e.name.as_str() == DEVELOPMENT) {
            Some(e) => e,
            None => {
                return error_response(ApiError::new(
                    ErrorCode::NotFound,
                    "the project has no development environment",
                ));
            }
        },
        Err(e) => {
            tracing::error!(error = %e, "reading environments failed");
            return internal(None);
        }
    };

    let (reply, stored) = oneshot::channel::<Result<ship_store::Revision, JobFailure>>();
    let job = {
        let store = cx.store.clone();
        let uploads = Arc::clone(cx.uploads);
        let deployer = Arc::clone(cx.deployer);
        let request = request.clone();
        let project = project.clone();
        let environment = environment.clone();
        let PushOptions {
            file,
            port,
            confirm,
        } = options;
        move |ctx: Context| async move {
            let received = crate::upload::receive(&ctx, &store, &uploads, &request, body).await;
            let revision = match received {
                Ok(received) => {
                    let _ = reply.send(Ok(received.revision.clone()));
                    received.revision
                }
                Err(e) => {
                    let _ = reply.send(Err(e.clone()));
                    return Err(e);
                }
            };
            let job = DeployJob {
                project,
                environment,
                revision,
                file,
                port,
                confirm,
            };
            crate::deploy::run(&ctx, &deployer, &job).await.map(|_| ())
        }
    };
    let submission = Submission {
        idempotency_key: key,
        kind: PUSH_KIND,
        environment_id: Some(environment.id),
    };
    let submitted = match cx.queue.submit(submission, job).await {
        Ok(submitted) => submitted,
        Err(e) => {
            if let QueueError::Store(err) = &e {
                tracing::error!(error = %err, "operation could not be created");
            }
            return error_response(e.to_api());
        }
    };
    let op = submitted.operation;
    if !submitted.created {
        return replay(cx.store, &op, &project, environment.id, &request).await;
    }
    if op.phase.is_terminal() {
        // Refused at admission; the job never runs.
        return submitted_response(&op);
    }
    match stored.await {
        Ok(Ok(revision)) => {
            let view = match cx.store.get_operation(op.id).await {
                Ok(current) => operation_view(&current),
                Err(e) => {
                    tracing::error!(error = %e, "reading a push failed");
                    return internal(Some(op.id));
                }
            };
            accepted(PushView {
                operation: view,
                project_id: project.id,
                environment_id: environment.id,
                revision_id: revision.id,
            })
        }
        // The stored failure, never the job's own: the queue redacts the
        // message before storing it.
        Ok(Err(_)) | Err(_) => {
            wait_finished(cx.store, cx.queue, op.id).await;
            match cx.store.get_operation(op.id).await {
                Ok(stored) => failure_response(&stored),
                Err(e) => {
                    tracing::error!(error = %e, "reading a failed push failed");
                    internal(Some(op.id))
                }
            }
        }
    }
}

fn accepted(view: PushView) -> Response {
    (StatusCode::ACCEPTED, Json(view)).into_response()
}

/// The project named by the request, created first by a `project`
/// operation when it is new. Its key is derived from the push's, so a
/// replayed push replays it too.
async fn ensure_project(
    cx: &PushContext<'_>,
    request: &UploadRequest,
    key: &str,
) -> Result<Project, Response> {
    let find = || async {
        match cx.store.list_projects().await {
            Ok(list) => Ok(list.into_iter().find(|p| p.slug == request.project)),
            Err(e) => {
                tracing::error!(error = %e, "listing projects failed");
                Err(internal(None))
            }
        }
    };
    if let Some(project) = find().await? {
        return Ok(project);
    }
    let derived = project_key(key);
    let slug = request.project.clone();
    let store = cx.store.clone();
    let job = move |ctx: Context| async move {
        let ensured = store.get_or_create_project(&slug).await?;
        if ensured.created {
            ctx.log(&format!(
                "created project {} with its {DEVELOPMENT} environment",
                ensured.project.slug
            ))
            .await?;
        }
        Ok(())
    };
    let submission = Submission {
        idempotency_key: &derived,
        kind: PROJECT_KIND,
        environment_id: None,
    };
    let op = match cx.queue.submit(submission, job).await {
        Ok(submitted) => submitted.operation,
        Err(e) => {
            if let QueueError::Store(err) = &e {
                tracing::error!(error = %err, "operation could not be created");
            }
            return Err(error_response(e.to_api()));
        }
    };
    wait_finished(cx.store, cx.queue, op.id).await;
    match cx.store.get_operation(op.id).await {
        Ok(done) if done.status == OperationStatus::Succeeded => {}
        Ok(done) => return Err(failure_response(&done)),
        Err(e) => {
            tracing::error!(error = %e, "reading a project operation failed");
            return Err(internal(Some(op.id)));
        }
    }
    match find().await? {
        Some(project) => Ok(project),
        None => Err(internal(Some(op.id))),
    }
}

/// The idempotency key of the `project` operation a push with `key` runs
/// first: `project-` and the SHA-256 of `key`, within the length limit
/// whatever `key` is.
pub fn project_key(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("project-{hex}")
}

/// The answer for a replayed key, without reading the body: the push's
/// view once its snapshot is stored, its failure if it failed before
/// that, else `409` while it is still receiving.
async fn replay(
    store: &Store,
    op: &Operation,
    project: &Project,
    environment: ship_core::EnvironmentId,
    request: &UploadRequest,
) -> Response {
    if op.kind != PUSH_KIND || op.environment_id != Some(environment) {
        return error_response(
            ApiError::new(
                ErrorCode::IdempotencyConflict,
                "this idempotency key was already used for a different request",
            )
            .with_operation(op.id),
        );
    }
    let revision = match crate::upload::find_revision(store, project.id, &request.sha256).await {
        Ok(revision) => revision,
        Err(_) => return internal(Some(op.id)),
    };
    let received = !matches!(op.phase, OperationPhase::Queued | OperationPhase::Uploading)
        && !matches!(
            op.failure_phase,
            Some(OperationPhase::Queued | OperationPhase::Uploading)
        );
    match revision {
        Some(revision) if received => accepted(PushView {
            operation: operation_view(op),
            project_id: project.id,
            environment_id: environment,
            revision_id: revision.id,
        }),
        _ if op.phase.is_terminal() => failure_response(op),
        _ => error_response(
            ApiError::new(
                ErrorCode::Conflict,
                "a push with this idempotency key is still receiving its snapshot",
            )
            .with_operation(op.id),
        ),
    }
}

/// The error for an operation that ended without success, from its stored
/// (redacted) failure.
fn failure_response(op: &Operation) -> Response {
    let code = op
        .failure_code
        .as_deref()
        .and_then(|c| c.parse::<ErrorCode>().ok())
        .unwrap_or(ErrorCode::Internal);
    let message = op
        .failure_message
        .clone()
        .unwrap_or_else(|| format!("the operation ended {}", op.status));
    error_response(ApiError::new(code, message).with_operation(op.id))
}

/// Returns once `id` is in a terminal phase (or cannot be read).
async fn wait_finished(store: &Store, queue: &Queue, id: OperationId) {
    let mut events = queue.subscribe_events();
    loop {
        events.borrow_and_update();
        match store.get_operation(id).await {
            Ok(op) if !op.phase.is_terminal() => {}
            _ => return,
        }
        let _ = tokio::time::timeout(POLL, events.changed()).await;
    }
}

/// The deployment `operation` recorded, `None` when it recorded none.
///
/// For a succeeded deployment, every service of the environment with the
/// status Docker reports now for that release's containers (`unknown`
/// when Docker cannot be asked, `stopped` when the release has no
/// container of it), and, while the deployment is the environment's
/// latest success, the routes recorded for it that accept a TCP
/// connection now: no address is shown that does not answer (OVERVIEW
/// §3.3). A deployment that did not succeed lists no services.
pub async fn deployment_of(
    store: &Store,
    docker: &Runner,
    operation: OperationId,
) -> Result<Option<OperationDeploymentView>, ship_store::StoreError> {
    let Some(deployment) = store.deployment_for_operation(operation).await? else {
        return Ok(None);
    };
    let mut view = OperationDeploymentView {
        deployment_id: deployment.id,
        release_id: deployment.release_id,
        result: deployment.result.map(|r| r.as_str().to_owned()),
        services: Vec::new(),
    };
    let (Some(DeploymentResult::Succeeded), Some(release)) =
        (deployment.result, deployment.release_id)
    else {
        return Ok(Some(view));
    };
    let env = store.get_environment(deployment.environment_id).await?;
    let services = store.list_services(env.id).await?;
    let latest = store
        .list_deployments(env.id)
        .await?
        .into_iter()
        .find(|d| d.result == Some(DeploymentResult::Succeeded))
        .map(|d| d.id);
    let routes = if latest == Some(deployment.id) {
        store.list_routes(env.id).await?
    } else {
        Vec::new()
    };
    let found = containers(docker, &Selector::environment(env.id)).await;
    for service in services {
        let status = match &found {
            Err(_) => ServiceStatus::Unknown,
            Ok(found) => {
                let statuses: Vec<ServiceStatus> = found
                    .iter()
                    .filter(|c| c.labels.release == release && c.labels.service == Some(service.id))
                    .map(|c| ServiceStatus::from_observation(c.observation, env.desired_state))
                    .collect();
                statuses
                    .iter()
                    .copied()
                    .find(|s| *s != ServiceStatus::Healthy)
                    .or(statuses.first().copied())
                    .unwrap_or(ServiceStatus::Stopped)
            }
        };
        let mut endpoints = Vec::new();
        for route in routes.iter().filter(|r| r.service_id == service.id) {
            if answers(&route.address).await {
                endpoints.push(EndpointView {
                    provider: route.provider.as_str().to_owned(),
                    address: route.address.clone(),
                });
            }
        }
        view.services.push(ServiceOutcomeView {
            name: service.name,
            status: status.as_str().to_owned(),
            endpoints,
        });
    }
    Ok(Some(view))
}

/// Whether `address` (`IP:PORT`) accepts a TCP connection within
/// [`PROBE_TIMEOUT`]. An unspecified address (`0.0.0.0`, `::`) is tried
/// on loopback, where a port bound to every address also answers.
pub async fn answers(address: &str) -> bool {
    let Ok(mut addr) = address.parse::<SocketAddr>() else {
        return false;
    };
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_keys_are_valid_and_stable() {
        let long = "k".repeat(128);
        for key in ["a", "push-1", long.as_str()] {
            let derived = project_key(key);
            assert!(ship_core::api::valid_idempotency_key(&derived), "{derived}");
            assert_eq!(derived, project_key(key));
        }
        assert_ne!(project_key("a"), project_key("b"));
    }

    #[tokio::test]
    async fn only_an_answering_address_answers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(answers(&format!("127.0.0.1:{port}")).await);
        assert!(answers(&format!("0.0.0.0:{port}")).await);
        drop(listener);
        assert!(!answers(&format!("127.0.0.1:{port}")).await);
        assert!(!answers("not an address").await);
    }
}
