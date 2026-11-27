//! Operations over HTTP (ADR-0011): `http::accept` behind a test mutation
//! route, `GET /api/v1/operations/{id}` and its resumable SSE stream.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::post;
use common::{SseStream, Via, body_text, send, sse_events};
use ship_core::secret::REDACTED;
use ship_core::{EnvironmentId, OperationId, OperationPhase, SecretValue, Slug};
use ship_daemon::Redactor;
use ship_daemon::http::{self, HostProbe};
use ship_daemon::queue::{Limits, Queue};
use ship_docker::Runner;
use ship_store::Store;
use tokio::sync::Semaphore;

const SECRET: &str = "s3cret-api-token-value";

#[derive(Clone)]
struct TestState {
    queue: Queue,
    env: EnvironmentId,
    runs: Arc<AtomicUsize>,
    /// A held job waits for one permit.
    gate: Arc<Semaphore>,
}

struct Server {
    _dir: tempfile::TempDir,
    addr: SocketAddr,
    via: Via,
    store: Store,
    state: TestState,
}

/// The job behind the test routes: one phase, three log lines (one with
/// the secret), an optional hold, one more line.
async fn mutate(kind: &'static str, state: TestState, headers: HeaderMap) -> Response {
    let hold = headers.contains_key("x-test-hold");
    let runs = state.runs.clone();
    let gate = state.gate.clone();
    http::accept(
        &state.queue,
        &headers,
        kind,
        Some(state.env),
        move |ctx| async move {
            runs.fetch_add(1, Ordering::SeqCst);
            ctx.enter(OperationPhase::Building, "building").await?;
            ctx.log(&format!("one\ntoken {SECRET}\nthree")).await?;
            if hold {
                gate.acquire().await.unwrap().forget();
            }
            ctx.log("after").await?;
            Ok(())
        },
    )
    .await
}

async fn serve(limits: Limits) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let redactor = Redactor::new();
    redactor.register(&SecretValue::new(SECRET));
    let queue = Queue::new(store.clone(), redactor, limits, dir.path()).unwrap();
    let env = store
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap()
        .environment
        .id;
    let state = TestState {
        queue: queue.clone(),
        env,
        runs: Arc::new(AtomicUsize::new(0)),
        gate: Arc::new(Semaphore::new(0)),
    };
    let probe = HostProbe {
        docker: Runner::docker().env("DOCKER_HOST", common::MISSING_DOCKER_HOST),
        state_dir: dir.path().to_path_buf(),
    };
    let test_routes = Router::new()
        .route(
            "/test/mutate",
            post(|State(s): State<TestState>, h: HeaderMap| mutate("test", s, h)),
        )
        .route(
            "/test/other",
            post(|State(s): State<TestState>, h: HeaderMap| mutate("other", s, h)),
        )
        .with_state(state.clone());
    let app = http::router(store.clone(), probe, queue).merge(test_routes);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        _dir: dir,
        addr,
        via: Via::Tcp(addr),
        store,
        state,
    }
}

fn limits() -> Limits {
    Limits {
        disk_watermark_bytes: 0,
        ..Limits::default()
    }
}

async fn post_with(s: &Server, path: &str, headers: &[&str]) -> common::Response {
    let mut all = vec!["Host: localhost"];
    all.extend_from_slice(headers);
    send(&s.via, "POST", path, &all).await
}

async fn finished(store: &Store, id: OperationId) {
    for _ in 0..1000 {
        if store.get_operation(id).await.unwrap().phase.is_terminal() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("operation {id} did not finish");
}

fn op_id(response: &common::Response) -> OperationId {
    response.json()["id"].as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn accept_answers_202_and_replays_the_original() {
    let s = serve(limits()).await;

    let missing = post_with(&s, "/test/mutate", &[]).await;
    assert_eq!(missing.status, 400, "{}", missing.body);
    assert_eq!(missing.json()["code"], "invalid_request");
    let bad = post_with(&s, "/test/mutate", &["Idempotency-Key: a b"]).await;
    assert_eq!(bad.status, 400);

    let first = post_with(&s, "/test/mutate", &["Idempotency-Key: key-1"]).await;
    assert_eq!(first.status, 202, "{}", first.body);
    let body = first.json();
    assert_eq!(body["kind"], "test");
    assert_eq!(body["environment_id"], s.state.env.to_string());
    assert_eq!(body["failure"], serde_json::Value::Null);
    let id = op_id(&first);

    let replay = post_with(&s, "/test/mutate", &["Idempotency-Key: key-1"]).await;
    assert_eq!(replay.status, 202);
    assert_eq!(op_id(&replay), id);
    finished(&s.store, id).await;
    let late = post_with(&s, "/test/mutate", &["Idempotency-Key: key-1"]).await;
    assert_eq!(late.status, 202);
    assert_eq!(late.json()["status"], "succeeded");
    assert_eq!(s.state.runs.load(Ordering::SeqCst), 1);

    let conflict = post_with(&s, "/test/other", &["Idempotency-Key: key-1"]).await;
    assert_eq!(conflict.status, 422, "{}", conflict.body);
    assert_eq!(conflict.json()["code"], "idempotency_conflict");
    assert_eq!(s.state.runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_refusal_at_admission_answers_its_code_with_the_operation() {
    let s = serve(Limits {
        disk_watermark_bytes: u64::MAX,
        ..Limits::default()
    })
    .await;
    let refused = post_with(&s, "/test/mutate", &["Idempotency-Key: full"]).await;
    assert_eq!(refused.status, 507, "{}", refused.body);
    let body = refused.json();
    assert_eq!(body["code"], "insufficient_storage");
    let id = body["operation_id"].as_str().unwrap().to_owned();
    let again = post_with(&s, "/test/mutate", &["Idempotency-Key: full"]).await;
    assert_eq!(again.status, 507);
    assert_eq!(again.json()["operation_id"], id.as_str());

    let view = common::get(&s.via, &format!("/api/v1/operations/{id}")).await;
    assert_eq!(view.status, 200);
    assert_eq!(view.json()["status"], "failed");
    assert_eq!(view.json()["failure"]["code"], "insufficient_storage");
    assert_eq!(view.json()["failure"]["phase"], "queued");
    assert_eq!(s.state.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn get_operation_found_missing_and_malformed() {
    let s = serve(limits()).await;
    let id = op_id(&post_with(&s, "/test/mutate", &["Idempotency-Key: k"]).await);
    finished(&s.store, id).await;

    let found = common::get(&s.via, &format!("/api/v1/operations/{id}")).await;
    assert_eq!(found.status, 200);
    let v = found.json();
    assert_eq!(v["id"], id.to_string());
    assert_eq!(v["phase"], "succeeded");
    assert_eq!(v["status"], "succeeded");
    assert!(v["log_bytes"].as_u64().unwrap() > 0);
    assert!(!found.body.contains(SECRET));

    let missing = common::get(&s.via, "/api/v1/operations/op_01aryz6s41tsv4rrffq69g5fav").await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json()["code"], "not_found");
    let malformed = common::get(&s.via, "/api/v1/operations/nope").await;
    assert_eq!(malformed.status, 400);
    assert_eq!(malformed.json()["code"], "invalid_id");
    let events = common::get(&s.via, "/api/v1/operations/nope/events").await;
    assert_eq!(events.status, 400);
}

/// The events of `id` after `headers` / `query`, read to the stream's end.
async fn read_events(s: &Server, id: OperationId, query: &str, headers: &[&str]) -> Vec<u64> {
    let mut all = vec!["Host: localhost"];
    all.extend_from_slice(headers);
    let r = send(
        &s.via,
        "GET",
        &format!("/api/v1/operations/{id}/events{query}"),
        &all,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.head.to_ascii_lowercase().contains("text/event-stream"));
    let body = body_text(&r);
    assert!(!body.contains(SECRET), "{body}");
    sse_events(&body).iter().map(|e| e.id).collect()
}

#[tokio::test]
async fn the_event_stream_resumes_after_any_sequence_once_ended() {
    let s = serve(limits()).await;
    let id = op_id(&post_with(&s, "/test/mutate", &["Idempotency-Key: k"]).await);
    finished(&s.store, id).await;

    // phase_started, three log lines, "after", finished.
    let all = read_events(&s, id, "", &[]).await;
    assert_eq!(all, [1, 2, 3, 4, 5, 6]);
    assert_eq!(
        read_events(&s, id, "", &["Last-Event-ID: 2"]).await,
        [3, 4, 5, 6]
    );
    assert_eq!(read_events(&s, id, "?after=4", &[]).await, [5, 6]);
    assert_eq!(
        read_events(&s, id, "?after=6", &[]).await,
        Vec::<u64>::new()
    );
    let bad = send(
        &s.via,
        "GET",
        &format!("/api/v1/operations/{id}/events?after=x"),
        &["Host: localhost"],
    )
    .await;
    assert_eq!(bad.status, 400);

    // The data lines carry the event view; the secret is redacted.
    let r = send(
        &s.via,
        "GET",
        &format!("/api/v1/operations/{id}/events"),
        &["Host: localhost"],
    )
    .await;
    let events = sse_events(&body_text(&r));
    assert_eq!(events[0].data["event"], "phase_started");
    assert_eq!(events[0].data["phase"], "building");
    assert_eq!(events[0].data["operation_id"], id.to_string());
    assert_eq!(events[2].data["event"], "log");
    assert!(
        events[2].data["message"]
            .as_str()
            .unwrap()
            .contains(REDACTED)
    );
    assert_eq!(events[5].data["event"], "finished");
    assert_eq!(events[5].data["phase"], "succeeded");
    for e in &events {
        assert_eq!(e.data["sequence"], e.id);
    }
}

#[tokio::test]
async fn a_live_stream_reconnecting_misses_and_repeats_nothing() {
    let s = serve(limits()).await;
    let id = op_id(
        &post_with(
            &s,
            "/test/mutate",
            &["Idempotency-Key: live", "X-Test-Hold: 1"],
        )
        .await,
    );
    let path = format!("/api/v1/operations/{id}/events");

    // Read two events, then drop the connection mid-operation.
    let mut first = SseStream::open(s.addr, &path, &[]).await;
    assert_eq!(first.status, 200);
    let a = first.next().await.unwrap();
    let b = first.next().await.unwrap();
    assert_eq!((a.id, b.id), (1, 2));
    drop(first);

    // Reconnect from the last one seen; the job is still held after
    // event 4.
    let mut second = SseStream::open(s.addr, &path, &["Last-Event-ID: 2"]).await;
    let c = second.next().await.unwrap();
    let d = second.next().await.unwrap();
    assert_eq!((c.id, d.id), (3, 4));
    s.state.gate.add_permits(1);
    let mut rest = Vec::new();
    while let Some(e) = tokio::time::timeout(Duration::from_secs(10), second.next())
        .await
        .expect("the stream ends after the terminal event")
    {
        rest.push(e);
    }
    assert_eq!(rest.iter().map(|e| e.id).collect::<Vec<_>>(), [5, 6]);
    assert_eq!(rest[1].data["event"], "finished");
}

#[tokio::test]
async fn reads_answer_while_a_mutation_holds_the_environment() {
    let s = serve(limits()).await;
    let held = op_id(
        &post_with(
            &s,
            "/test/mutate",
            &["Idempotency-Key: held", "X-Test-Hold: 1"],
        )
        .await,
    );
    let waiting = op_id(
        &post_with(
            &s,
            "/test/mutate",
            &["Idempotency-Key: waiting", "X-Test-Hold: 1"],
        )
        .await,
    );
    let quick = Duration::from_secs(2);
    let r = tokio::time::timeout(
        quick,
        common::get(&s.via, &format!("/api/v1/operations/{waiting}")),
    )
    .await
    .unwrap();
    assert_eq!(r.json()["status"], "queued");
    let r = tokio::time::timeout(quick, common::get(&s.via, "/api/v1/projects"))
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    // The held operation's stream answers with what exists so far.
    let mut stream = tokio::time::timeout(
        quick,
        SseStream::open(s.addr, &format!("/api/v1/operations/{held}/events"), &[]),
    )
    .await
    .unwrap();
    let first = tokio::time::timeout(quick, stream.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.id, 1);

    s.state.gate.add_permits(2);
    finished(&s.store, held).await;
    finished(&s.store, waiting).await;
    assert_eq!(s.state.runs.load(Ordering::SeqCst), 2);
}
