//! The durable operation queue (ADR-0011).
//!
//! Every mutation is a row in the `operation` table before any side effect.
//! [`Queue::submit`] creates that row under the request's idempotency key,
//! so a replay returns the original operation and starts nothing; it then
//! admits the operation against [`Limits`] and hands its job to the
//! environment's worker.
//!
//! - One worker task per environment runs that environment's operations
//!   one at a time, in admission order. Operations on different
//!   environments run concurrently. Reads never touch the queue.
//! - [`Context::build`] waits for one of the host's build slots and runs
//!   the build under the build timeout.
//! - Events are persisted with gapless sequence numbers and announced to
//!   SSE readers; a job's terminal event is persisted before its terminal
//!   phase, so a reader that sees the operation ended finds every event.
//! - Output goes to a bounded per-operation log file. Every line, event
//!   message and failure message passes through the [`Redactor`] before it
//!   is written anywhere.
//! - [`recover`] runs at daemon start: whatever was left `queued` or
//!   `running` is marked `interrupted` and never resumed.

use std::collections::HashMap;
use std::fmt;
use std::fs::{DirBuilder, File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use ship_core::api::EventKind;
use ship_core::{ApiError, EnvironmentId, ErrorCode, OperationId, OperationPhase, Timestamp};
use ship_store::{NewEvent, NewOperation, Operation, OperationFailure, Store, StoreError};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinHandle;

use crate::log::Redactor;

/// Directory under the state directory holding one log file per operation.
pub const LOG_DIR: &str = "operations";

/// Message of the terminal event and failure of an operation found
/// unfinished at daemon start.
pub const INTERRUPTED_MESSAGE: &str =
    "the daemon stopped during the operation; it is not resumed, run it again";

/// Limits enforced before an operation's work starts (ADR-0011 rules 4
/// and 10, OVERVIEW §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most operations admitted and not yet finished, across the host.
    pub max_queue: usize,
    /// Builds that may run at once across the host.
    pub build_slots: usize,
    /// Longest a build may run before it fails `build_timeout`.
    pub build_timeout: Duration,
    /// An operation is refused when the state directory's filesystem has
    /// fewer free bytes than this.
    pub disk_watermark_bytes: u64,
    /// Largest an operation's log file grows, plus one truncation marker.
    pub max_log_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_queue: 32,
            build_slots: 1,
            build_timeout: Duration::from_secs(30 * 60),
            disk_watermark_bytes: 1024 * 1024 * 1024,
            max_log_bytes: 16 * 1024 * 1024,
        }
    }
}

/// What a mutation asks the queue for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Submission<'a> {
    /// The client's key; see [`ship_core::api::valid_idempotency_key`].
    pub idempotency_key: &'a str,
    /// What the operation does, e.g. `push`.
    pub kind: &'a str,
    /// The environment the operation mutates. Operations on one environment
    /// never run at the same time.
    pub environment_id: Option<EnvironmentId>,
}

/// Result of [`Queue::submit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submitted {
    /// The operation as it stood when `submit` returned.
    pub operation: Operation,
    /// `false` when the key was replayed: the original operation was
    /// returned and nothing was started.
    pub created: bool,
}

/// Why [`Queue::submit`] refused a request without creating an operation.
#[derive(Debug)]
pub enum QueueError {
    /// The idempotency key is empty, too long or not visible ASCII.
    InvalidKey,
    /// The key was used before for a different kind or environment.
    IdempotencyConflict,
    /// The environment does not exist.
    UnknownEnvironment,
    /// The daemon is shutting down.
    Closed,
    /// The database failed.
    Store(StoreError),
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueError::InvalidKey => f.write_str("invalid idempotency key"),
            QueueError::IdempotencyConflict => {
                f.write_str("idempotency key reused for a different request")
            }
            QueueError::UnknownEnvironment => f.write_str("no such environment"),
            QueueError::Closed => f.write_str("the daemon is shutting down"),
            QueueError::Store(e) => write!(f, "database: {e}"),
        }
    }
}

impl std::error::Error for QueueError {}

impl QueueError {
    /// The API error for this refusal. Never carries a stored value.
    pub fn to_api(&self) -> ApiError {
        match self {
            QueueError::InvalidKey => ApiError::new(
                ErrorCode::InvalidRequest,
                "the Idempotency-Key header must be 1 to 128 visible ASCII characters",
            ),
            QueueError::IdempotencyConflict => ApiError::new(
                ErrorCode::IdempotencyConflict,
                "this idempotency key was already used for a different request",
            ),
            QueueError::UnknownEnvironment => {
                ApiError::new(ErrorCode::NotFound, "no such environment")
            }
            QueueError::Closed => {
                ApiError::new(ErrorCode::Interrupted, "the daemon is shutting down")
            }
            QueueError::Store(_) => ApiError::new(
                ErrorCode::Internal,
                "the daemon could not write its database",
            ),
        }
    }
}

/// A job's typed failure (ADR-0011 rule 7). The message is redacted before
/// it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFailure {
    pub code: ErrorCode,
    pub message: String,
}

impl JobFailure {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        JobFailure {
            code,
            message: message.into(),
        }
    }
}

impl From<StoreError> for JobFailure {
    fn from(e: StoreError) -> Self {
        tracing::error!(error = %e, "operation could not write the database");
        JobFailure::new(
            ErrorCode::Internal,
            "the daemon could not write its database",
        )
    }
}

impl fmt::Display for JobFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

type JobFuture = Pin<Box<dyn Future<Output = Result<(), JobFailure>> + Send>>;
type BoxJob = Box<dyn FnOnce(Context) -> JobFuture + Send>;

/// An admitted operation waiting for its environment's worker.
struct Pending {
    operation: OperationId,
    job: BoxJob,
}

/// The daemon's operation queue. Cheap to clone; clones share one queue.
#[derive(Clone)]
pub struct Queue {
    inner: Arc<Inner>,
}

impl fmt::Debug for Queue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Queue")
            .field("limits", &self.inner.limits)
            .finish_non_exhaustive()
    }
}

struct Inner {
    store: Store,
    redactor: Redactor,
    limits: Limits,
    state_dir: PathBuf,
    log_dir: PathBuf,
    builds: Arc<Semaphore>,
    /// Bumped after every persisted event; SSE readers wait on it.
    events: watch::Sender<u64>,
    /// Set once the queue is closed; SSE readers end their streams.
    closing: watch::Sender<bool>,
    /// Held from creating an operation's row until its admission is
    /// decided and any refusal stored, so a replay of the key never sees
    /// the row before that decision.
    admitting: tokio::sync::Mutex<()>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    closed: bool,
    /// Operations admitted and not yet finished.
    in_flight: usize,
    workers: HashMap<EnvironmentId, mpsc::UnboundedSender<Pending>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `text` with every registered value replaced, including values that
    /// span lines. The [`Redactor`] works line by line, so `text` goes to it
    /// whole as one JSON string (its newlines escaped), which it redacts
    /// inside the decoded value.
    fn redact(&self, text: &str) -> String {
        let json = serde_json::to_vec(text).expect("a str always serialises");
        let redacted = self.redactor.redact(&json);
        serde_json::from_slice(&redacted).unwrap_or_else(|_| {
            // Not reached: the Redactor keeps a JSON line valid JSON.
            String::from_utf8_lossy(&self.redactor.redact(text.as_bytes())).into_owned()
        })
    }

    fn announce(&self) {
        self.events.send_modify(|n| *n = n.wrapping_add(1));
    }

    async fn append(
        &self,
        operation: OperationId,
        kind: EventKind,
        phase: OperationPhase,
        message: &str,
    ) -> Result<(), StoreError> {
        let message = self.redact(message);
        self.store
            .append_event(
                operation,
                &NewEvent {
                    event_type: kind.as_str(),
                    phase,
                    message: &message,
                },
            )
            .await?;
        self.announce();
        Ok(())
    }
}

impl Queue {
    /// A queue over `store`, writing operation logs under
    /// `state_dir/operations` (created at 0700 if missing).
    pub fn new(
        store: Store,
        redactor: Redactor,
        limits: Limits,
        state_dir: &Path,
    ) -> io::Result<Queue> {
        let log_dir = state_dir.join(LOG_DIR);
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&log_dir)?;
        let (events, _) = watch::channel(0);
        let (closing, _) = watch::channel(false);
        Ok(Queue {
            inner: Arc::new(Inner {
                store,
                redactor,
                limits,
                state_dir: state_dir.to_path_buf(),
                log_dir,
                builds: Arc::new(Semaphore::new(limits.build_slots.max(1))),
                events,
                closing,
                admitting: tokio::sync::Mutex::new(()),
                state: Mutex::new(State::default()),
            }),
        })
    }

    /// The queue's limits.
    pub fn limits(&self) -> Limits {
        self.inner.limits
    }

    /// The store the queue writes to.
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The secret values the queue redacts from logs, events and failures.
    pub fn redactor(&self) -> &Redactor {
        &self.inner.redactor
    }

    /// Path of `operation`'s log file, which exists once a line is logged.
    pub fn log_path(&self, operation: OperationId) -> PathBuf {
        log_path(&self.inner.log_dir, operation)
    }

    /// Persists an operation for `submission` and, if it is new and
    /// admitted, queues `job` on its environment's worker.
    ///
    /// A replayed key returns the original operation with
    /// `created = false` and never runs `job`. A new operation over the
    /// queue length, or with the state directory's free disk under the
    /// watermark, is returned already `failed` (`queue_full`,
    /// `insufficient_storage`) in phase `queued`, and `job` never runs.
    pub async fn submit<F, Fut>(
        &self,
        submission: Submission<'_>,
        job: F,
    ) -> Result<Submitted, QueueError>
    where
        F: FnOnce(Context) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), JobFailure>> + Send + 'static,
    {
        let inner = &self.inner;
        if !ship_core::api::valid_idempotency_key(submission.idempotency_key) {
            return Err(QueueError::InvalidKey);
        }
        if inner.state().closed {
            return Err(QueueError::Closed);
        }
        if let Some(env) = submission.environment_id {
            match inner.store.get_environment(env).await {
                Ok(_) => {}
                Err(StoreError::NotFound { .. }) => return Err(QueueError::UnknownEnvironment),
                Err(e) => return Err(QueueError::Store(e)),
            }
        }
        // Held until admission is decided and a refusal stored (f-023).
        let _admitting = inner.admitting.lock().await;
        let created = inner
            .store
            .create_operation(&NewOperation {
                idempotency_key: submission.idempotency_key,
                kind: submission.kind,
                environment_id: submission.environment_id,
            })
            .await
            .map_err(|e| match e {
                StoreError::Conflict { .. } => QueueError::IdempotencyConflict,
                e => QueueError::Store(e),
            })?;
        let operation = created.operation;
        if !created.created {
            return Ok(Submitted {
                operation,
                created: false,
            });
        }

        // Admission. The disk is measured outside the lock; the count is
        // checked and taken under it, together with the dispatch, so
        // shutdown cannot slip between them.
        let disk_free = crate::host::disk_free_bytes(&inner.state_dir);
        let refusal = {
            let mut state = inner.state();
            if state.closed {
                // The row stays `queued`; the next start interrupts it.
                return Err(QueueError::Closed);
            }
            match admission(&inner.limits, state.in_flight, disk_free) {
                Some(refusal) => Some(refusal),
                None => {
                    state.in_flight += 1;
                    let pending = Pending {
                        operation: operation.id,
                        job: Box::new(move |ctx| -> JobFuture { Box::pin(job(ctx)) }),
                    };
                    dispatch(inner, &mut state, submission.environment_id, pending);
                    None
                }
            }
        };
        if let Some(refusal) = refusal {
            tracing::warn!(
                operation = %operation.id,
                code = %refusal.code,
                "operation refused at admission"
            );
            let operation = finish(inner, operation.id, Err(refusal))
                .await
                .map_err(QueueError::Store)?;
            return Ok(Submitted {
                operation,
                created: true,
            });
        }
        Ok(Submitted {
            operation,
            created: true,
        })
    }

    /// Refuses new submissions and tells SSE readers to end their streams.
    pub fn close(&self) {
        self.inner.state().closed = true;
        self.inner.closing.send_replace(true);
    }

    /// [`Queue::close`], then aborts every running job and waits for its
    /// task to end. Aborted operations stay `running` in the database; the
    /// next daemon start marks them `interrupted` ([`recover`]).
    pub async fn shutdown(&self) {
        self.close();
        let tasks = {
            let mut state = self.inner.state();
            state.workers.clear();
            std::mem::take(&mut state.tasks)
        };
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
    }

    pub(crate) fn subscribe_events(&self) -> watch::Receiver<u64> {
        self.inner.events.subscribe()
    }

    pub(crate) fn subscribe_closing(&self) -> watch::Receiver<bool> {
        self.inner.closing.subscribe()
    }
}

/// Why a new operation is refused before its work starts, if it is.
fn admission(limits: &Limits, in_flight: usize, disk_free: Option<u64>) -> Option<JobFailure> {
    if in_flight >= limits.max_queue {
        return Some(JobFailure::new(
            ErrorCode::QueueFull,
            format!(
                "the daemon already has {} unfinished operations, its limit",
                limits.max_queue
            ),
        ));
    }
    match disk_free {
        None => Some(JobFailure::new(
            ErrorCode::InsufficientStorage,
            "the free space of the state directory could not be read",
        )),
        Some(free) if free < limits.disk_watermark_bytes => Some(JobFailure::new(
            ErrorCode::InsufficientStorage,
            format!(
                "the state directory has {free} bytes free, under the watermark of {} bytes",
                limits.disk_watermark_bytes
            ),
        )),
        Some(_) => None,
    }
}

/// Sends `pending` to its environment's worker, starting one if needed;
/// an operation with no environment gets a task of its own.
fn dispatch(
    inner: &Arc<Inner>,
    state: &mut State,
    environment: Option<EnvironmentId>,
    pending: Pending,
) {
    state.tasks.retain(|t| !t.is_finished());
    let Some(env) = environment else {
        let inner = Arc::clone(inner);
        state
            .tasks
            .push(tokio::spawn(async move { run(&inner, pending).await }));
        return;
    };
    let pending = match state.workers.get(&env) {
        Some(tx) => match tx.send(pending) {
            Ok(()) => return,
            // The worker is gone (it never stops on its own, but be safe).
            Err(mpsc::error::SendError(pending)) => pending,
        },
        None => pending,
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<Pending>();
    tx.send(pending).expect("the receiver is alive");
    state.workers.insert(env, tx);
    let inner = Arc::clone(inner);
    state.tasks.push(tokio::spawn(async move {
        while let Some(pending) = rx.recv().await {
            run(&inner, pending).await;
        }
    }));
}

/// Runs one admitted operation to its end.
async fn run(inner: &Arc<Inner>, pending: Pending) {
    let Pending { operation, job } = pending;
    let ctx = Context {
        inner: Arc::clone(inner),
        operation,
        shared: Arc::new(tokio::sync::Mutex::new(Shared {
            phase: OperationPhase::Queued,
            log: None,
            written: 0,
            truncated: false,
        })),
    };
    let result = AssertUnwindSafe(async move { job(ctx).await })
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            tracing::error!(operation = %operation, "operation job panicked");
            Err(JobFailure::new(
                ErrorCode::Internal,
                "the operation failed inside the daemon",
            ))
        });
    if let Err(e) = finish(inner, operation, result).await {
        tracing::error!(operation = %operation, error = %e, "could not record the end of an operation");
    }
    // Wakes readers waiting for the terminal phase, which is stored after
    // the terminal event's announcement.
    inner.announce();
    let mut state = inner.state();
    state.in_flight = state.in_flight.saturating_sub(1);
}

/// Records the end of an operation: its terminal event first, then its
/// terminal phase, so a reader that sees it ended finds every event.
async fn finish(
    inner: &Inner,
    operation: OperationId,
    result: Result<(), JobFailure>,
) -> Result<Operation, StoreError> {
    match result {
        Ok(()) => {
            inner
                .append(
                    operation,
                    EventKind::Finished,
                    OperationPhase::Succeeded,
                    "succeeded",
                )
                .await?;
            inner
                .store
                .advance_operation(operation, OperationPhase::Succeeded)
                .await
        }
        Err(failure) => {
            let message = inner.redact(&failure.message);
            inner
                .append(
                    operation,
                    EventKind::Finished,
                    OperationPhase::Failed,
                    &message,
                )
                .await?;
            inner
                .store
                .fail_operation(
                    operation,
                    OperationPhase::Failed,
                    &OperationFailure {
                        code: failure.code,
                        message: &message,
                    },
                )
                .await
        }
    }
}

/// What a job's [`Context`] clones share.
struct Shared {
    phase: OperationPhase,
    log: Option<File>,
    /// Bytes in the log file, which is also the stored log cursor.
    written: u64,
    truncated: bool,
}

/// A running operation's handle on the queue: phase changes, output and
/// the build slot. Cheap to clone.
#[derive(Clone)]
pub struct Context {
    inner: Arc<Inner>,
    operation: OperationId,
    shared: Arc<tokio::sync::Mutex<Shared>>,
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

impl Context {
    /// The running operation.
    pub fn operation_id(&self) -> OperationId {
        self.operation
    }

    /// Moves the operation to `phase` (a later non-terminal phase) and
    /// records a `phase_started` event with `message`, redacted.
    pub async fn enter(&self, phase: OperationPhase, message: &str) -> Result<(), JobFailure> {
        if phase.is_terminal() {
            return Err(JobFailure::new(
                ErrorCode::Internal,
                "an operation ends by returning from its job",
            ));
        }
        let mut shared = self.shared.lock().await;
        self.inner
            .store
            .advance_operation(self.operation, phase)
            .await?;
        shared.phase = phase;
        self.inner
            .append(self.operation, EventKind::PhaseStarted, phase, message)
            .await?;
        Ok(())
    }

    /// [`Context::enter`] `phase`, or, when the operation is already in
    /// it (a push validates its snapshot before its deployment validates
    /// the build definition), [`Context::log`] `message` instead, so
    /// phases only ever increase.
    pub async fn continue_in(
        &self,
        phase: OperationPhase,
        message: &str,
    ) -> Result<(), JobFailure> {
        let current = self.shared.lock().await.phase;
        if current == phase {
            self.log(message).await
        } else {
            self.enter(phase, message).await
        }
    }

    /// Writes `text` to the operation's log, one record per line, each
    /// redacted, and records each kept line as a `log` event. Once the log
    /// reaches its limit one truncation marker is written and later lines
    /// are dropped.
    pub async fn log(&self, text: &str) -> Result<(), JobFailure> {
        let mut shared = self.shared.lock().await;
        // Redacted whole first, so a value spanning lines is caught.
        let text = self.inner.redact(text);
        for line in text.lines() {
            if shared.truncated {
                break;
            }
            let line = self.inner.redact(line.strip_suffix('\r').unwrap_or(line));
            let phase = shared.phase;
            let record = self.record(phase, &line);
            let max = self.inner.limits.max_log_bytes;
            let (bytes, message) = if fits(shared.written, record.len(), max) {
                (record, line)
            } else {
                shared.truncated = true;
                let marker = format!("[log truncated: limit of {max} bytes reached]");
                (self.record(phase, &marker), marker)
            };
            self.write_log(&mut shared, &bytes)?;
            self.inner
                .store
                .set_log_cursor(self.operation, shared.written)
                .await?;
            self.inner
                .append(self.operation, EventKind::Log, phase, &message)
                .await?;
        }
        Ok(())
    }

    /// Runs `work` in one of the host's build slots, waiting for one if all
    /// are taken, and fails it `build_timeout` if it runs longer than the
    /// build timeout. `work` is dropped at the timeout.
    pub async fn build<T, W>(&self, work: W) -> Result<T, JobFailure>
    where
        W: Future<Output = Result<T, JobFailure>>,
    {
        let builds = Arc::clone(&self.inner.builds);
        let _permit = match Arc::clone(&builds).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let phase = self.shared.lock().await.phase;
                self.inner
                    .append(
                        self.operation,
                        EventKind::Waiting,
                        phase,
                        "waiting for a build slot",
                    )
                    .await?;
                builds.acquire_owned().await.map_err(|_| {
                    JobFailure::new(ErrorCode::Internal, "the build slots were closed")
                })?
            }
        };
        let timeout = self.inner.limits.build_timeout;
        match tokio::time::timeout(timeout, work).await {
            Ok(result) => result,
            Err(_) => Err(JobFailure::new(
                ErrorCode::BuildTimeout,
                format!(
                    "the build ran longer than the build timeout of {} s",
                    timeout.as_secs()
                ),
            )),
        }
    }

    /// One log record: a JSON line, redacted again as a whole.
    fn record(&self, phase: OperationPhase, line: &str) -> Vec<u8> {
        let ts = now();
        let mut json = serde_json::to_vec(&serde_json::json!({
            "ts": ts,
            "phase": phase,
            "line": line,
        }))
        .expect("a JSON value always serialises");
        json.push(b'\n');
        self.inner.redactor.redact(&json)
    }

    fn write_log(&self, shared: &mut Shared, bytes: &[u8]) -> Result<(), JobFailure> {
        let io_failure = |e: io::Error| {
            tracing::error!(operation = %self.operation, error = %e, "operation log write failed");
            JobFailure::new(
                ErrorCode::Internal,
                "the daemon could not write the operation log",
            )
        };
        if shared.log.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(log_path(&self.inner.log_dir, self.operation))
                .map_err(io_failure)?;
            shared.log = Some(file);
        }
        let file = shared.log.as_mut().expect("opened above");
        file.write_all(bytes).map_err(io_failure)?;
        shared.written += bytes.len() as u64;
        Ok(())
    }
}

/// Whether a record of `len` bytes fits in a log of `written` bytes capped
/// at `max`.
fn fits(written: u64, len: usize, max: u64) -> bool {
    written
        .checked_add(len as u64)
        .is_some_and(|total| total <= max)
}

fn log_path(log_dir: &Path, operation: OperationId) -> PathBuf {
    log_dir.join(format!("{operation}.log"))
}

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    Timestamp::from_unix_millis(ms)
        .unwrap_or_else(|_| Timestamp::from_unix_millis(0).expect("the epoch is valid"))
}

/// Marks every operation left `queued` or `running` as `interrupted`
/// (ADR-0011 rule 8), with a terminal event, and returns how many there
/// were. Nothing is resumed; logs are left as they are.
pub async fn recover(store: &Store) -> Result<usize, StoreError> {
    let unfinished = store.unfinished_operations().await?;
    for op in &unfinished {
        // A crash after the terminal event but before the phase change
        // would otherwise leave two `interrupted` events.
        let events = store.events_after(op.id, 0).await?;
        let already = events.last().is_some_and(|e| {
            e.event_type == EventKind::Finished.as_str() && e.phase == OperationPhase::Interrupted
        });
        if !already {
            store
                .append_event(
                    op.id,
                    &NewEvent {
                        event_type: EventKind::Finished.as_str(),
                        phase: OperationPhase::Interrupted,
                        message: INTERRUPTED_MESSAGE,
                    },
                )
                .await?;
        }
        store
            .fail_operation(
                op.id,
                OperationPhase::Interrupted,
                &OperationFailure {
                    code: ErrorCode::Interrupted,
                    message: INTERRUPTED_MESSAGE,
                },
            )
            .await?;
        tracing::warn!(
            operation = %op.id,
            kind = %op.kind,
            phase = %op.phase,
            "operation interrupted by a daemon restart; not resumed"
        );
    }
    Ok(unfinished.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_cap_arithmetic() {
        assert!(fits(0, 10, 10));
        assert!(!fits(0, 11, 10));
        assert!(fits(5, 5, 10));
        assert!(!fits(6, 5, 10));
        assert!(!fits(u64::MAX, 1, u64::MAX));
        assert!(fits(0, 0, 0));
    }

    #[test]
    fn admission_order_and_messages() {
        let limits = Limits {
            max_queue: 2,
            disk_watermark_bytes: 100,
            ..Limits::default()
        };
        assert_eq!(admission(&limits, 1, Some(100)), None);
        assert_eq!(
            admission(&limits, 2, Some(100)).unwrap().code,
            ErrorCode::QueueFull
        );
        // The queue length is checked first.
        assert_eq!(
            admission(&limits, 2, Some(0)).unwrap().code,
            ErrorCode::QueueFull
        );
        assert_eq!(
            admission(&limits, 0, Some(99)).unwrap().code,
            ErrorCode::InsufficientStorage
        );
        assert_eq!(
            admission(&limits, 0, None).unwrap().code,
            ErrorCode::InsufficientStorage
        );
    }

    #[test]
    fn default_limits_are_conservative() {
        let l = Limits::default();
        assert_eq!(l.build_slots, 1);
        assert!(l.max_queue > 0);
        assert!(l.build_timeout > Duration::ZERO);
        assert!(l.disk_watermark_bytes > 0);
        assert!(l.max_log_bytes > 0);
    }

    #[test]
    fn queue_errors_map_to_stable_codes() {
        assert_eq!(
            QueueError::InvalidKey.to_api().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            QueueError::IdempotencyConflict.to_api().code,
            ErrorCode::IdempotencyConflict
        );
        assert_eq!(
            QueueError::UnknownEnvironment.to_api().code,
            ErrorCode::NotFound
        );
        assert_eq!(QueueError::Closed.to_api().code, ErrorCode::Interrupted);
    }
}
