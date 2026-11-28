//! `ship push [PATH]`: snapshot a working tree, send it to the target's
//! daemon as one `push` operation, follow it and print the result
//! (ADR-0004, ADR-0005, ADR-0011; `docs/contracts/cli-json.md`).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::Serialize;
use ship_core::api::{
    DeployRequest, IDEMPOTENCY_KEY_HEADER, OperationDeploymentView, OperationEventView,
    OperationView, PushOptions, PushView, RECREATE_REQUIRED, SNAPSHOT_SHA256_HEADER,
    ServiceOutcomeView, deployments_path, operation_deployment_path, operation_path, push_query,
    pushes_path,
};
use ship_core::deploy::PortSpec;
use ship_core::{
    DeploymentId, EnvironmentId, OperationId, OperationPhase, OperationStatus, ProjectId,
    ReleaseId, RevisionId, Slug,
};
use ship_net::client::RequestBody;
use ship_source::{Options, Packed, Selection, pack, scan};

use crate::client::{self, ClientError, Endpoint};
use crate::commands::{Output, runtime};
use crate::exit;
use crate::follow::{self, Halt, REQUEST_TIMEOUT};

/// The one environment of a Phase 1 project.
const DEVELOPMENT: &str = "development";

/// Bytes read from the packed snapshot per body chunk.
const CHUNK: usize = 64 * 1024;
/// Body chunks buffered between the file and the connection.
const BODY_CHANNEL: usize = 8;

#[derive(Debug, clap::Args)]
#[command(
    after_help = "Exit status (docs/contracts/cli-json.md): 0 deployed and verified; \
    1 the upload, build, deployment or verification failed; 2 usage; \
    3 the Compose file needs confirmation and none was given; 90 no daemon answered; \
    91 client and daemon API versions differ; 92 Docker is unavailable on the host; \
    93 the daemon stopped during the operation; 94 the connection was lost while waiting \
    (the operation continues); 99 any other Ship failure."
)]
pub struct PushArgs {
    /// The directory to push.
    #[arg(value_name = "PATH", default_value = ".")]
    path: PathBuf,
    /// The project to deploy to [default: the directory's name].
    #[arg(long, value_name = "P")]
    project: Option<String>,
    /// The build file, relative to PATH [default: a Compose file, else a
    /// Dockerfile].
    #[arg(long, value_name = "F")]
    file: Option<String>,
    /// Publish a Dockerfile's port: N (host N to the one EXPOSE port, or
    /// to N with none) or HOST:CONTAINER.
    #[arg(long, value_name = "N|HOST:CONTAINER")]
    port: Option<PortSpec>,
    /// Follow the deployment to its end (the default).
    #[arg(long, conflicts_with = "detach")]
    wait: bool,
    /// Return once the snapshot is stored, printing the operation id.
    #[arg(long)]
    detach: bool,
    /// Show what the snapshot would hold and send nothing.
    #[arg(long)]
    dry_run: bool,
    /// Confirm the Compose file's confirmation items (ADR-0008 rule 2).
    #[arg(long)]
    confirm: bool,
    /// Bring back a removed environment: the same environment, its named
    /// volumes reattached and its history continued.
    #[arg(long)]
    recreate: bool,
    /// Push to this target instead of the current one.
    #[arg(long, value_name = "TARGET")]
    to: Option<String>,
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    output: Output,
    /// After losing the daemon while waiting, keep trying to reach it for
    /// this many seconds.
    #[arg(long, value_name = "SECS", default_value_t = 60, value_parser = clap::value_parser!(u64).range(0..=3600))]
    reconnect: u64,
    /// Give up sending the snapshot after this many seconds.
    #[arg(long, value_name = "SECS", default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..=86400))]
    timeout: u64,
}

fn usage(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("ship push: {message}");
    ExitCode::from(exit::USAGE)
}

fn failed(message: impl std::fmt::Display, code: u8) -> ExitCode {
    eprintln!("ship push: {message}");
    ExitCode::from(code)
}

/// The project slug for `root`: its directory name, which must be a valid
/// slug.
pub fn default_slug(root: &Path) -> Result<Slug, String> {
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "the directory has no usable name; pass --project".to_owned())?;
    Slug::parse(name).map_err(|e| {
        format!("the directory name `{name}` is not a valid project name ({e}); pass --project")
    })
}

pub fn push(args: PushArgs) -> ExitCode {
    let root = match fs::canonicalize(&args.path) {
        Ok(p) if p.is_dir() => p,
        Ok(p) => return usage(format!("{} is not a directory", p.display())),
        Err(e) => return usage(format!("{}: {e}", args.path.display())),
    };
    let slug = match &args.project {
        Some(p) => match Slug::parse(p) {
            Ok(s) => s,
            Err(e) => return usage(format!("--project: {e}")),
        },
        None => match default_slug(&root) {
            Ok(s) => s,
            Err(why) => return usage(why),
        },
    };
    if let Some(file) = &args.file
        && let Err(e) = ship_core::deploy::check_file_path(file)
    {
        return usage(format!("--file: {e}"));
    }
    let selection = match scan(&root, &Options::default()) {
        Ok(s) => s,
        Err(e) if args.dry_run => return failed(e, exit::FAILED),
        Err(e) => return local_failure(args.output, &slug, e),
    };
    if args.dry_run {
        print!("{}", render_dry_run(&selection, args.output));
        return ExitCode::SUCCESS;
    }
    let endpoint = match resolve_endpoint(args.to.as_deref()) {
        Ok(e) => e,
        Err(code) => return code,
    };
    let (file, packed) = match pack_to_temp(&selection) {
        Ok(p) => p,
        Err(e) => {
            return local_failure(
                args.output,
                &slug,
                format!("packing {}: {e}", root.display()),
            );
        }
    };
    if args.output == Output::Text {
        println!(
            "Pushing {} to {slug}: {} files, {} bytes ({} compressed)",
            root.display(),
            selection.file_count(),
            selection.total_bytes(),
            packed.compressed_bytes
        );
    }
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(_) => return ExitCode::from(exit::INTERNAL),
    };
    let run = Run {
        endpoint: &endpoint,
        args: &args,
        printer: Printer {
            output: args.output,
            slug: slug.clone(),
        },
        slug: &slug,
    };
    let code = rt.block_on(run.push(file, &packed));
    // Drop the runtime now so an ssh child is reaped before exiting.
    drop(rt);
    ExitCode::from(code)
}

/// The tree could not be scanned or packed: exit 1, with `-o json` as the
/// one result line, every id unknown.
fn local_failure(output: Output, slug: &Slug, message: impl std::fmt::Display) -> ExitCode {
    match output {
        Output::Text => failed(message, exit::FAILED),
        Output::Json => {
            let line = failure_line(
                slug,
                None,
                Known::default(),
                exit::name(exit::FAILED),
                &message.to_string(),
            );
            println!("{}", render_result_json(&line));
            ExitCode::from(exit::FAILED)
        }
    }
}

/// `--dry-run`: the report as text, or as one JSON object.
fn render_dry_run(selection: &Selection, output: Output) -> String {
    let report = selection.report();
    match output {
        Output::Text => report.to_string(),
        Output::Json => {
            let mut s = serde_json::to_string_pretty(&report).expect("a report serialises");
            s.push('\n');
            s
        }
    }
}

/// The endpoint of `--to`, or the current target's (the local daemon at
/// the default socket when none is current). A config that cannot be
/// located, read or parsed is Ship's own failure, 99, on both paths; a bad
/// target name or URL is usage, 2.
fn resolve_endpoint(to: Option<&str>) -> Result<Endpoint, ExitCode> {
    let fail = |e: crate::config::ConfigError| {
        let code = if e.is_usage() {
            exit::USAGE
        } else {
            exit::INTERNAL
        };
        failed(e, code)
    };
    let path = crate::config::default_location().map_err(fail)?;
    let cfg = crate::config::Config::load(&path).map_err(fail)?;
    let Some(name) = to else {
        return Ok(match cfg.current().map_err(fail)? {
            Some((name, target)) => Endpoint {
                name: Some(name.to_owned()),
                target,
            },
            None => Endpoint {
                name: None,
                target: ship_net::Target::Local {
                    socket: PathBuf::from(ship_net::DEFAULT_SOCKET),
                },
            },
        });
    };
    let target = cfg.named(name).map_err(fail)?;
    Ok(Endpoint {
        name: Some(name.to_owned()),
        target,
    })
}

/// Packs `selection` into an unlinked temporary file at mode 0600, so its
/// SHA-256 is known before the upload starts and memory stays bounded.
fn pack_to_temp(selection: &Selection) -> io::Result<(File, Packed)> {
    let path = std::env::temp_dir().join(format!("ship-push-{}.tar.zst", unique()));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    // The handle keeps the data; nothing is left behind on any exit.
    let _ = fs::remove_file(&path);
    let packed = {
        let mut out = BufWriter::new(&file);
        let packed = pack(selection, &mut out).map_err(io::Error::other)?;
        out.flush()?;
        packed
    };
    file.seek(SeekFrom::Start(0))?;
    Ok((file, packed))
}

/// A string no other call of this process returns: pid and time.
fn unique() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

/// Whether a person can be asked: stdin and stderr are terminals.
pub fn on_terminal() -> bool {
    // SAFETY: isatty has no preconditions.
    unsafe { libc::isatty(0) == 1 && libc::isatty(2) == 1 }
}

/// Asks `question` on stderr and reads a yes from stdin.
pub fn ask(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

/// One push.
struct Run<'a> {
    endpoint: &'a Endpoint,
    args: &'a PushArgs,
    printer: Printer,
    slug: &'a Slug,
}

/// What the operator confirmed for a push.
#[derive(Debug, Clone, Copy, Default)]
struct Confirmed {
    /// The Compose file's confirmation items (ADR-0008 rule 2).
    compose: bool,
    /// Bringing back a removed environment (`--recreate`).
    recreate: bool,
}

/// Whether a `confirmation_required` failure asks to recreate a removed
/// environment rather than to confirm a Compose file's items.
fn is_recreate_required(message: &str) -> bool {
    message.starts_with(RECREATE_REQUIRED)
}

/// What the result line knows of the push.
#[derive(Debug, Clone, Copy, Default)]
struct Known {
    project_id: Option<ProjectId>,
    environment_id: Option<EnvironmentId>,
    revision_id: Option<RevisionId>,
}

impl Run<'_> {
    fn reconnect(&self) -> Duration {
        Duration::from_secs(self.args.reconnect)
    }

    /// Sends the snapshot and follows the operation; returns the exit code.
    async fn push(&self, file: File, packed: &Packed) -> u8 {
        let key = format!("push-{}", unique());
        let query = push_query(&PushOptions {
            file: self.args.file.clone(),
            port: self.args.port,
            confirm: self.args.confirm,
            recreate: self.args.recreate,
        });
        let path = format!("{}{query}", pushes_path(self.slug));
        let headers = [
            ("content-type", "application/zstd"),
            (IDEMPOTENCY_KEY_HEADER, key.as_str()),
            (SNAPSHOT_SHA256_HEADER, packed.sha256.as_str()),
        ];
        let body = stream_file(file);
        let timeout = Duration::from_secs(self.args.timeout);
        let response =
            match client::post_stream(self.endpoint, &path, &headers, body, timeout).await {
                Ok(r) => r,
                Err(e) => return self.client_failure(None, Known::default(), &e),
            };
        if response.status != 202 {
            let e = client::decode_error(&response);
            // A snapshot refused inside its operation: show that
            // operation's events and failure.
            if let ClientError::Api(api) = &e
                && let Some(op) = api.operation_id
            {
                return self.finish(op, Known::default()).await;
            }
            return self.client_failure(None, Known::default(), &e);
        }
        let view: PushView = match serde_json::from_slice(&response.body) {
            Ok(v) => v,
            Err(e) => {
                let e = ClientError::BadResponse(e.to_string());
                return self.client_failure(None, Known::default(), &e);
            }
        };
        let known = Known {
            project_id: Some(view.project_id),
            environment_id: Some(view.environment_id),
            revision_id: Some(view.revision_id),
        };
        // `--wait` is the default; clap refuses it beside `--detach`.
        if self.args.detach && !self.args.wait {
            self.printer.detached(&view);
            return exit::SUCCEEDED;
        }
        self.finish(view.operation.id, known).await
    }

    /// Follows `op` to its end, prints the result, and returns the exit
    /// code. A Compose file that needs confirmation is confirmed here when
    /// a person is at a terminal, and the same revision deployed again.
    async fn finish(&self, mut op: OperationId, known: Known) -> u8 {
        // What was confirmed so far: `--confirm` / `--recreate`, then each
        // yes on a terminal.
        let mut confirmed = Confirmed {
            compose: self.args.confirm,
            recreate: self.args.recreate,
        };
        loop {
            let printer = &self.printer;
            if let Err(halt) =
                follow::follow(self.endpoint, op, 0, self.reconnect(), |e| printer.event(e)).await
            {
                return self.halted(op, known, halt);
            }
            let view = match self.ended(op).await {
                Ok(v) => v,
                Err(halt) => return self.halted(op, known, halt),
            };
            let deployment: Option<OperationDeploymentView> = match follow::get_retrying(
                self.endpoint,
                &operation_deployment_path(op),
                self.reconnect(),
            )
            .await
            {
                Ok(d) => d,
                Err(halt) => return self.halted(op, known, halt),
            };
            let code = match (&view.status, &view.failure) {
                (OperationStatus::Succeeded, _) => exit::SUCCEEDED,
                (OperationStatus::Interrupted, _) => exit::INTERRUPTED,
                (_, Some(f)) => exit::for_failure(&f.code),
                (_, None) => exit::INTERNAL,
            };
            let asks_recreate = view
                .failure
                .as_ref()
                .is_some_and(|f| is_recreate_required(&f.message));
            let unconfirmed = if asks_recreate {
                !confirmed.recreate
            } else {
                !confirmed.compose
            };
            if code == exit::CONFIRMATION_REQUIRED
                && unconfirmed
                && let Some(revision) = known.revision_id
                && on_terminal()
            {
                if let Some(f) = &view.failure {
                    eprintln!("{}", f.message);
                }
                let question = if asks_recreate {
                    "Recreate the environment, reattaching these volumes?"
                } else {
                    "Deploy with these items confirmed?"
                };
                if ask(question) {
                    if asks_recreate {
                        confirmed.recreate = true;
                    } else {
                        confirmed.compose = true;
                    }
                    match self.redeploy_confirmed(revision, confirmed).await {
                        Ok(next) => {
                            op = next;
                            continue;
                        }
                        Err(e) => return self.client_failure(Some(op), known, &e),
                    }
                }
            }
            self.printer
                .result(&self.result_line(&view, known, deployment), code);
            return code;
        }
    }

    /// The operation once its status is terminal. The daemon stores the
    /// terminal event before the terminal phase, so a read right after the
    /// event can still find it running; it is read again until it ended.
    async fn ended(&self, op: OperationId) -> Result<OperationView, Halt> {
        let deadline = std::time::Instant::now() + REQUEST_TIMEOUT;
        loop {
            let view: OperationView =
                follow::get_retrying(self.endpoint, &operation_path(op), self.reconnect())
                    .await?
                    .ok_or_else(|| {
                        Halt::Client(ClientError::BadResponse(
                            "the operation vanished".to_owned(),
                        ))
                    })?;
            let terminal = matches!(
                view.status,
                OperationStatus::Succeeded | OperationStatus::Failed | OperationStatus::Interrupted
            );
            if terminal {
                return Ok(view);
            }
            if std::time::Instant::now() >= deadline {
                return Err(Halt::Client(ClientError::BadResponse(format!(
                    "operation {op} sent its last event but is still {}",
                    view.status
                ))));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Deploys `revision` again with what is `confirmed`.
    async fn redeploy_confirmed(
        &self,
        revision: RevisionId,
        confirmed: Confirmed,
    ) -> Result<OperationId, ClientError> {
        let request = DeployRequest {
            revision_id: revision,
            file: self.args.file.clone(),
            port: self.args.port,
            confirm: confirmed.compose,
            recreate: confirmed.recreate,
        };
        let body = serde_json::to_vec(&request).expect("a request serialises");
        let key = format!("confirm-{}", unique());
        let headers = [(IDEMPOTENCY_KEY_HEADER, key.as_str())];
        let view: OperationView = client::post_json(
            self.endpoint,
            &deployments_path(self.slug),
            &headers,
            body,
            REQUEST_TIMEOUT,
        )
        .await?;
        Ok(view.id)
    }

    fn result_line(
        &self,
        view: &OperationView,
        known: Known,
        deployment: Option<OperationDeploymentView>,
    ) -> ResultLine {
        let (deployment_id, release_id, services) = match deployment {
            Some(d) => (Some(d.deployment_id), d.release_id, d.services),
            None => (None, None, Vec::new()),
        };
        ResultLine {
            kind: "result",
            operation_id: Some(view.id),
            status: view.status.as_str().to_owned(),
            failure: view.failure.as_ref().map(|f| FailureLine {
                code: f.code.as_str().to_owned(),
                phase: Some(f.phase),
                message: f.message.clone(),
            }),
            project: ProjectRef {
                id: known.project_id,
                slug: self.slug.clone(),
            },
            environment: EnvironmentRef {
                id: known.environment_id.or(view.environment_id),
                name: DEVELOPMENT,
            },
            revision_id: known.revision_id,
            release_id,
            deployment_id,
            services,
        }
    }

    /// Prints a failure of the client itself and returns its exit code.
    fn client_failure(&self, op: Option<OperationId>, known: Known, e: &ClientError) -> u8 {
        let code = exit::for_client_error(e);
        let (failure_code, message) = match e {
            ClientError::Api(api) => (api.code.as_str().to_owned(), api.message.clone()),
            _ => (exit::name(code).to_owned(), e.to_string()),
        };
        self.printer
            .result(&self.client_line(op, known, &failure_code, &message), code);
        code
    }

    fn halted(&self, op: OperationId, known: Known, halt: Halt) -> u8 {
        match halt {
            Halt::Client(e) => self.client_failure(Some(op), known, &e),
            Halt::Lost(why) => {
                let message = format!(
                    "lost the daemon while waiting ({why}); operation {op} continues on the host"
                );
                let line = ResultLine {
                    status: "unknown".to_owned(),
                    ..self.client_line(Some(op), known, exit::name(exit::CONNECTION_LOST), &message)
                };
                self.printer.result(&line, exit::CONNECTION_LOST);
                exit::CONNECTION_LOST
            }
        }
    }

    fn client_line(
        &self,
        op: Option<OperationId>,
        known: Known,
        code: &str,
        message: &str,
    ) -> ResultLine {
        failure_line(self.slug, op, known, code, message)
    }
}

/// The result line of a failure of the client itself, with what it knows.
fn failure_line(
    slug: &Slug,
    op: Option<OperationId>,
    known: Known,
    code: &str,
    message: &str,
) -> ResultLine {
    ResultLine {
        kind: "result",
        operation_id: op,
        status: "failed".to_owned(),
        failure: Some(FailureLine {
            code: code.to_owned(),
            phase: None,
            message: message.to_owned(),
        }),
        project: ProjectRef {
            id: known.project_id,
            slug: slug.clone(),
        },
        environment: EnvironmentRef {
            id: known.environment_id,
            name: DEVELOPMENT,
        },
        revision_id: known.revision_id,
        release_id: None,
        deployment_id: None,
        services: Vec::new(),
    }
}

/// A request body fed from `file` by a thread, in bounded memory.
fn stream_file(mut file: File) -> RequestBody {
    let (tx, body) = RequestBody::channel(BODY_CHANNEL);
    std::thread::spawn(move || {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .blocking_send(Ok(Bytes::copy_from_slice(&buf[..n])))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });
    body
}

/// The last line of `ship push --wait -o json` (`push-wait-*.jsonl`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultLine {
    #[serde(rename = "type")]
    kind: &'static str,
    operation_id: Option<OperationId>,
    /// An operation status, or `unknown` after the connection was lost.
    status: String,
    failure: Option<FailureLine>,
    project: ProjectRef,
    environment: EnvironmentRef,
    revision_id: Option<RevisionId>,
    release_id: Option<ReleaseId>,
    deployment_id: Option<DeploymentId>,
    services: Vec<ServiceOutcomeView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FailureLine {
    /// The operation's failure code, or the exit code's name for a
    /// failure of the client.
    code: String,
    phase: Option<OperationPhase>,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ProjectRef {
    id: Option<ProjectId>,
    slug: Slug,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EnvironmentRef {
    id: Option<EnvironmentId>,
    name: &'static str,
}

/// An event line of `ship push --wait -o json`.
#[derive(Serialize)]
struct EventLine<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    event: &'a OperationEventView,
}

/// Prints progress and the result as text or JSON lines.
struct Printer {
    output: Output,
    slug: Slug,
}

impl Printer {
    fn event(&self, e: &OperationEventView) {
        print!("{}", render_event(e, self.output));
        let _ = io::stdout().flush();
    }

    fn result(&self, line: &ResultLine, code: u8) {
        match self.output {
            Output::Json => println!("{}", render_result_json(line)),
            Output::Text => {
                let (out, err) = render_result_text(line, &self.slug, code);
                print!("{out}");
                eprint!("{err}");
            }
        }
    }

    fn detached(&self, view: &PushView) {
        match self.output {
            Output::Json => println!(
                "{}",
                serde_json::to_string(view).expect("a view serialises")
            ),
            Output::Text => println!(
                "Pushed {}: operation {} deploys revision {} on the host.",
                self.slug, view.operation.id, view.revision_id
            ),
        }
    }
}

/// One event as printed: a JSON line, or text for people (nothing for
/// the terminal event, which the result follows).
pub fn render_event(e: &OperationEventView, output: Output) -> String {
    match output {
        Output::Json => {
            let line = EventLine {
                kind: "event",
                event: e,
            };
            format!(
                "{}\n",
                serde_json::to_string(&line).expect("an event serialises")
            )
        }
        Output::Text => match e.event.as_str() {
            "phase_started" => format!("{}: {}\n", e.phase, e.message),
            "finished" => String::new(),
            "waiting" => format!("  ({})\n", e.message),
            _ => format!("  {}\n", e.message),
        },
    }
}

fn render_result_json(line: &ResultLine) -> String {
    serde_json::to_string(line).expect("a result serialises")
}

/// The text result: what goes to stdout and what to stderr. A success
/// names the release, each service and every endpoint, or says there is
/// none; a failure goes to stderr.
fn render_result_text(line: &ResultLine, slug: &Slug, code: u8) -> (String, String) {
    if code == exit::SUCCEEDED {
        let mut out = format!(
            "Deployed {slug}: release {}, deployment {}\n",
            show(line.release_id),
            show(line.deployment_id)
        );
        let mut endpoints = Vec::new();
        for s in &line.services {
            out.push_str(&format!("  {}: {}\n", s.name, s.status));
            endpoints.extend(s.endpoints.iter().map(|e| (s.name.as_str(), e)));
        }
        if endpoints.is_empty() {
            out.push_str("Endpoint: not configured\n");
        } else {
            let many = line.services.len() > 1;
            for (service, e) in endpoints {
                if many {
                    out.push_str(&format!("Endpoint: {} ({service})\n", e.address));
                } else {
                    out.push_str(&format!("Endpoint: {}\n", e.address));
                }
            }
        }
        return (out, String::new());
    }
    let mut err = String::new();
    if let Some(f) = &line.failure {
        let phase = f.phase.map(|p| format!(" in {p}")).unwrap_or_default();
        err.push_str(&format!(
            "ship push: {} ({}{phase}): {}\n",
            line.status, f.code, f.message
        ));
    } else {
        err.push_str(&format!("ship push: {}\n", line.status));
    }
    if let Some(op) = line.operation_id {
        err.push_str(&format!("Operation: {op}\n"));
    }
    let recreate = line
        .failure
        .as_ref()
        .is_some_and(|f| is_recreate_required(&f.message));
    if code == exit::CONFIRMATION_REQUIRED && recreate {
        err.push_str(
            "Nothing was deployed. Confirm on a terminal, or run again with --recreate.\n",
        );
    } else if code == exit::CONFIRMATION_REQUIRED {
        err.push_str(
            "Nothing was started. Confirm on a terminal, or run again with --confirm \
             (`ship settings set compose.confirm accept` waives it on the host).\n",
        );
    }
    (String::new(), err)
}

fn show<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string())
        .unwrap_or_else(|| "none".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use ship_core::Timestamp;
    use ship_core::api::EndpointView;

    const SUCCEEDED_FIXTURE: &str = include_str!("../tests/fixtures/push-wait-succeeded.jsonl");
    const FAILED_FIXTURE: &str = include_str!("../tests/fixtures/push-wait-failed.jsonl");

    fn lines(text: &str) -> Vec<Value> {
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn slug() -> Slug {
        Slug::parse("my-app").unwrap()
    }

    #[test]
    fn default_slug_is_the_directory_name() {
        assert_eq!(default_slug(Path::new("/w/my-app")).unwrap(), slug());
        let e = default_slug(Path::new("/w/My App")).unwrap_err();
        assert!(e.contains("--project"), "{e}");
        assert!(default_slug(Path::new("/")).is_err());
    }

    /// Every event line of both fixtures prints back byte for byte.
    #[test]
    fn event_lines_are_the_fixtures() {
        for fixture in [SUCCEEDED_FIXTURE, FAILED_FIXTURE] {
            for line in fixture.lines() {
                let v: Value = serde_json::from_str(line).unwrap();
                if v["type"] != "event" {
                    continue;
                }
                let e: OperationEventView = serde_json::from_value(v).unwrap();
                assert_eq!(render_event(&e, Output::Json), format!("{line}\n"));
            }
        }
    }

    fn succeeded_line() -> ResultLine {
        ResultLine {
            kind: "result",
            operation_id: Some("op_01aryz6s41tsv4rrffq69g5fav".parse().unwrap()),
            status: "succeeded".to_owned(),
            failure: None,
            project: ProjectRef {
                id: Some("prj_01aryz6s41tsv4rrffq69g5fav".parse().unwrap()),
                slug: slug(),
            },
            environment: EnvironmentRef {
                id: Some("env_01aryz6s41tsv4rrffq69g5fav".parse().unwrap()),
                name: DEVELOPMENT,
            },
            revision_id: Some("rev_01aryz6s41tsv4rrffq69g5fav".parse().unwrap()),
            release_id: Some("rel_01aryz6s41tsv4rrffq69g5fav".parse().unwrap()),
            deployment_id: Some("dep_01aryz6s41tsv4rrffq69g5fav".parse().unwrap()),
            services: vec![ServiceOutcomeView {
                name: "web".to_owned(),
                status: "healthy".to_owned(),
                endpoints: vec![EndpointView {
                    provider: "port".to_owned(),
                    address: "192.0.2.10:8080".to_owned(),
                }],
            }],
        }
    }

    /// The result lines print back byte for byte.
    #[test]
    fn result_lines_are_the_fixtures() {
        let ok = succeeded_line();
        assert_eq!(
            render_result_json(&ok),
            SUCCEEDED_FIXTURE.lines().last().unwrap()
        );
        let failed = ResultLine {
            operation_id: Some("op_01aryz6s41tsv4rrffq69g5faw".parse().unwrap()),
            status: "failed".to_owned(),
            failure: Some(FailureLine {
                code: "build_failed".to_owned(),
                phase: Some(OperationPhase::Building),
                message: "the image build exited 1".to_owned(),
            }),
            environment: EnvironmentRef {
                id: Some("env_01aryz6s41tsv4rrffq69g5fav".parse().unwrap()),
                name: DEVELOPMENT,
            },
            revision_id: Some("rev_01aryz6s41tsv4rrffq69g5faw".parse().unwrap()),
            release_id: None,
            deployment_id: Some("dep_01aryz6s41tsv4rrffq69g5faw".parse().unwrap()),
            services: Vec::new(),
            ..ok
        };
        assert_eq!(
            render_result_json(&failed),
            FAILED_FIXTURE.lines().last().unwrap()
        );
        let last = lines(FAILED_FIXTURE).pop().unwrap();
        assert_eq!(last["type"], "result");
    }

    #[test]
    fn text_results() {
        let ok = succeeded_line();
        let (out, err) = render_result_text(&ok, &slug(), exit::SUCCEEDED);
        assert_eq!(
            out,
            "Deployed my-app: release rel_01aryz6s41tsv4rrffq69g5fav, \
             deployment dep_01aryz6s41tsv4rrffq69g5fav\n  web: healthy\n\
             Endpoint: 192.0.2.10:8080\n"
        );
        assert_eq!(err, "");
        let mut none = ok.clone();
        none.services[0].endpoints.clear();
        let (out, _) = render_result_text(&none, &slug(), exit::SUCCEEDED);
        assert!(
            out.ends_with("  web: healthy\nEndpoint: not configured\n"),
            "{out}"
        );

        let confirm = ResultLine {
            status: "failed".to_owned(),
            failure: Some(FailureLine {
                code: "confirmation_required".to_owned(),
                phase: Some(OperationPhase::Validating),
                message: "needs confirmation: services.web.ports".to_owned(),
            }),
            services: Vec::new(),
            ..ok
        };
        let (out, err) = render_result_text(&confirm, &slug(), exit::CONFIRMATION_REQUIRED);
        assert_eq!(out, "");
        assert!(
            err.starts_with(
                "ship push: failed (confirmation_required in validating): \
                 needs confirmation: services.web.ports\n"
            ),
            "{err}"
        );
        assert!(err.contains("--confirm"), "{err}");
    }

    #[test]
    fn text_events() {
        let e = OperationEventView {
            operation_id: "op_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
            sequence: 1,
            phase: OperationPhase::Building,
            event: "phase_started".to_owned(),
            message: "building web".to_owned(),
            ts: Timestamp::from_unix_millis(0).unwrap(),
        };
        assert_eq!(render_event(&e, Output::Text), "building: building web\n");
        let log = OperationEventView {
            event: "log".to_owned(),
            message: "#1 DONE".to_owned(),
            ..e.clone()
        };
        assert_eq!(render_event(&log, Output::Text), "  #1 DONE\n");
        let end = OperationEventView {
            event: "finished".to_owned(),
            ..e
        };
        assert_eq!(render_event(&end, Output::Text), "");
    }
}
