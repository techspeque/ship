//! `ship start|stop|restart PROJECT[/SERVICE]` and `ship redeploy|remove
//! PROJECT` (phase-1-ws-1.12): one operation each on the target's daemon,
//! followed to its end with its progress printed.
//!
//! `redeploy` and `remove` cover a whole environment; a `PROJECT/SERVICE`
//! is refused before anything is sent. `remove` asks first on a terminal;
//! with none it needs `--confirm`, else it exits 3 and sends nothing.

use std::process::ExitCode;
use std::time::Duration;

use ship_core::api::{
    IDEMPOTENCY_KEY_HEADER, OperationView, REDEPLOY_KIND, REMOVE_KIND, RESTART_KIND, START_KIND,
    STOP_KIND, config_query, lifecycle_path, operation_path,
};
use ship_core::{OperationStatus, Slug};

use crate::client::{self, ClientError, Endpoint};
use crate::commands::{FAILURE, Output, USAGE, endpoint, fail, runtime};
use crate::exit;
use crate::follow::{self, Halt};

/// How long a lost daemon is waited for while following.
const RECONNECT: Duration = Duration::from_secs(60);

#[derive(Debug, clap::Args)]
#[command(
    after_help = "Exit status: 0 done; 1 the action failed or was refused; 2 usage; \
                        90 no daemon answered; 92 Docker is unavailable on the host; \
                        94 the connection was lost while waiting (the operation continues)."
)]
pub struct TargetArgs {
    /// PROJECT, or PROJECT/SERVICE for one service.
    #[arg(value_name = "PROJECT[/SERVICE]")]
    target: String,
    #[command(flatten)]
    common: Common,
}

#[derive(Debug, clap::Args)]
#[command(
    after_help = "Exit status: 0 done; 1 the action failed or was refused; 2 usage; \
                        90 no daemon answered; 92 Docker is unavailable on the host; \
                        94 the connection was lost while waiting (the operation continues)."
)]
pub struct ProjectArgs {
    /// The project's slug. A release covers every service, so there is no
    /// PROJECT/SERVICE form.
    #[arg(value_name = "PROJECT")]
    project: String,
    #[command(flatten)]
    common: Common,
}

#[derive(Debug, clap::Args)]
#[command(
    after_help = "Exit status: 0 removed; 1 the removal failed, was refused or not \
                        confirmed; 2 usage; 3 no terminal to confirm on and no --confirm \
                        (nothing was sent); 90 no daemon answered; 92 Docker is unavailable \
                        on the host; 94 the connection was lost while waiting."
)]
pub struct RemoveArgs {
    /// The project's slug. Its containers and networks are removed; every
    /// named volume, the release files and the history are kept.
    #[arg(value_name = "PROJECT")]
    project: String,
    /// Remove without asking.
    #[arg(long)]
    confirm: bool,
    #[command(flatten)]
    common: Common,
}

#[derive(Debug, clap::Args)]
struct Common {
    /// The environment [default: development].
    #[arg(long, value_name = "E")]
    env: Option<String>,
    /// `json`: the operation as it ended, instead of its progress.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    output: Output,
    /// Give up sending the request after this many seconds, including the
    /// ssh connection.
    #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
}

/// What an action needs from the command line, parsed.
struct Request<'a> {
    action: &'static str,
    slug: Slug,
    service: Option<String>,
    common: &'a Common,
}

fn slug(text: &str) -> Result<Slug, ExitCode> {
    Slug::parse(text).map_err(|e| fail(format!("`{text}` is not a project slug: {e}"), USAGE))
}

/// `PROJECT[/SERVICE]`.
pub(crate) fn parse_target(text: &str) -> Result<(Slug, Option<String>), String> {
    match text.split_once('/') {
        None => Slug::parse(text)
            .map(|s| (s, None))
            .map_err(|e| format!("`{text}` is not a project slug: {e}")),
        Some((project, service)) if !service.is_empty() && !service.contains('/') => {
            Slug::parse(project)
                .map(|s| (s, Some(service.to_owned())))
                .map_err(|e| format!("`{project}` is not a project slug: {e}"))
        }
        Some(_) => Err(format!("`{text}` must be PROJECT or PROJECT/SERVICE")),
    }
}

/// The refusal of `PROJECT/SERVICE` for an action that covers the whole
/// environment, or `None`.
pub(crate) fn environment_only(action: &str, text: &str) -> Option<String> {
    text.contains('/').then(|| {
        format!(
            "{action} applies to a whole environment, not one service: a release and a \
             Compose project cover every service; run `ship {action} PROJECT`"
        )
    })
}

pub fn start(args: TargetArgs) -> ExitCode {
    with_target(START_KIND, &args)
}

pub fn stop(args: TargetArgs) -> ExitCode {
    with_target(STOP_KIND, &args)
}

pub fn restart(args: TargetArgs) -> ExitCode {
    with_target(RESTART_KIND, &args)
}

fn with_target(action: &'static str, args: &TargetArgs) -> ExitCode {
    let (slug, service) = match parse_target(&args.target) {
        Ok(parsed) => parsed,
        Err(why) => return fail(why, USAGE),
    };
    run(&Request {
        action,
        slug,
        service,
        common: &args.common,
    })
}

pub fn redeploy(args: ProjectArgs) -> ExitCode {
    if let Some(why) = environment_only(REDEPLOY_KIND, &args.project) {
        return fail(why, USAGE);
    }
    let slug = match slug(&args.project) {
        Ok(s) => s,
        Err(code) => return code,
    };
    run(&Request {
        action: REDEPLOY_KIND,
        slug,
        service: None,
        common: &args.common,
    })
}

pub fn remove(args: RemoveArgs) -> ExitCode {
    if let Some(why) = environment_only(REMOVE_KIND, &args.project) {
        return fail(why, USAGE);
    }
    let slug = match slug(&args.project) {
        Ok(s) => s,
        Err(code) => return code,
    };
    if !args.confirm {
        let env = args.common.env.as_deref().unwrap_or("development");
        let what = remove_question(&slug, env);
        if !crate::push::on_terminal() {
            eprintln!("ship remove: {what}");
            eprintln!(
                "ship remove: needs confirmation; nothing was sent. Confirm on a terminal, or \
                 run again with --confirm."
            );
            return ExitCode::from(exit::CONFIRMATION_REQUIRED);
        }
        if !crate::push::ask(&what) {
            return fail("nothing was removed", FAILURE);
        }
    }
    run(&Request {
        action: REMOVE_KIND,
        slug,
        service: None,
        common: &args.common,
    })
}

/// What `remove` asks, naming what goes and what stays.
pub(crate) fn remove_question(slug: &Slug, env: &str) -> String {
    format!(
        "Remove {slug} ({env})? Its containers and networks are removed; every named volume, \
         its release files and its history are kept, and `ship push --recreate` brings it back."
    )
}

/// Sends the action, follows its operation and prints how it ended.
fn run(request: &Request<'_>) -> ExitCode {
    let endpoint = match endpoint() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let code = rt.block_on(follow_action(&endpoint, request));
    drop(rt);
    ExitCode::from(code)
}

async fn follow_action(endpoint: &Endpoint, request: &Request<'_>) -> u8 {
    let name = request.action;
    let output = request.common.output;
    let path = format!(
        "{}{}",
        lifecycle_path(&request.slug, name),
        config_query(request.common.env.as_deref(), request.service.as_deref())
    );
    let key = new_key(name);
    let headers = [(IDEMPOTENCY_KEY_HEADER, key.as_str())];
    let timeout = Duration::from_secs(request.common.timeout);
    let submitted: OperationView =
        match client::post_json(endpoint, &path, &headers, Vec::new(), timeout).await {
            Ok(view) => view,
            Err(e) => return client_failure(name, &e),
        };
    let op = submitted.id;
    let followed = follow::follow(endpoint, op, 0, RECONNECT, |e| {
        if output == Output::Text {
            print!("{}", crate::push::render_event(e, Output::Text));
        }
    })
    .await;
    if let Err(halt) = followed {
        return halted(name, op, halt);
    }
    let view = match ended(endpoint, op).await {
        Ok(view) => view,
        Err(halt) => return halted(name, op, halt),
    };
    let code = exit_code(&view);
    match output {
        Output::Json => {
            let mut s = serde_json::to_string_pretty(&view).expect("a view serialises");
            s.push('\n');
            print!("{s}");
        }
        Output::Text => {
            let (out, err) = render_end(name, &request.slug, request.service.as_deref(), &view);
            print!("{out}");
            eprint!("{err}");
        }
    }
    code
}

/// The operation once its status is terminal: the daemon stores the
/// terminal event before the terminal phase, so it is read until it ended.
async fn ended(endpoint: &Endpoint, op: ship_core::OperationId) -> Result<OperationView, Halt> {
    let deadline = std::time::Instant::now() + follow::REQUEST_TIMEOUT;
    loop {
        let view: OperationView = follow::get_retrying(endpoint, &operation_path(op), RECONNECT)
            .await?
            .ok_or_else(|| {
                Halt::Client(ClientError::BadResponse(
                    "the operation vanished".to_owned(),
                ))
            })?;
        if is_terminal(view.status) || std::time::Instant::now() >= deadline {
            return Ok(view);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn is_terminal(status: OperationStatus) -> bool {
    matches!(
        status,
        OperationStatus::Succeeded | OperationStatus::Failed | OperationStatus::Interrupted
    )
}

/// 0 succeeded; Docker unavailable and an interruption keep their reserved
/// codes; every other failure, a refusal included, is 1.
pub(crate) fn exit_code(view: &OperationView) -> u8 {
    match (view.status, &view.failure) {
        (OperationStatus::Succeeded, _) => exit::SUCCEEDED,
        (OperationStatus::Interrupted, _) => exit::INTERRUPTED,
        (_, Some(f)) => match exit::for_failure(&f.code) {
            code @ (exit::DOCKER_UNAVAILABLE | exit::INTERRUPTED) => code,
            _ => exit::FAILED,
        },
        (_, None) => exit::FAILED,
    }
}

/// The last lines for people: what is done on stdout, a failure on
/// stderr.
pub(crate) fn render_end(
    action: &str,
    slug: &Slug,
    service: Option<&str>,
    view: &OperationView,
) -> (String, String) {
    let what = match service {
        Some(s) => format!("{slug}/{s}"),
        None => slug.to_string(),
    };
    if view.status == OperationStatus::Succeeded {
        let done = match action {
            START_KIND => "Started",
            STOP_KIND => "Stopped",
            RESTART_KIND => "Restarted",
            REDEPLOY_KIND => "Redeployed",
            _ => "Removed",
        };
        return (format!("{done} {what}.\n"), String::new());
    }
    let mut err = match &view.failure {
        Some(f) => format!(
            "ship {action}: {} ({} in {}): {}\n",
            view.status, f.code, f.phase, f.message
        ),
        None => format!("ship {action}: {}\n", view.status),
    };
    err.push_str(&format!("Operation: {}\n", view.id));
    (String::new(), err)
}

fn client_failure(action: &str, e: &ClientError) -> u8 {
    eprintln!("ship {action}: {e}");
    match exit::for_client_error(e) {
        exit::INTERNAL => match e {
            // A refusal of the request itself (`400`, `404`).
            ClientError::Api(_) => exit::FAILED,
            _ => exit::INTERNAL,
        },
        code => code,
    }
}

fn halted(action: &str, op: ship_core::OperationId, halt: Halt) -> u8 {
    match halt {
        Halt::Client(e) => client_failure(action, &e),
        Halt::Lost(why) => {
            eprintln!(
                "ship {action}: lost the daemon while waiting ({why}); operation {op} continues \
                 on the host"
            );
            exit::CONNECTION_LOST
        }
    }
}

/// A key no other request of this client uses: the action, process id and
/// time.
fn new_key(action: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{action}-{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ship_core::api::OperationFailureView;
    use ship_core::{OperationPhase, ReceivedCode, Timestamp};

    fn view(status: OperationStatus, failure: Option<(&str, &str)>) -> OperationView {
        let at = Timestamp::from_unix_millis(0).unwrap();
        OperationView {
            id: "op_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
            kind: "stop".to_owned(),
            environment_id: None,
            phase: OperationPhase::Deploying,
            status,
            failure: failure.map(|(code, message)| OperationFailureView {
                code: ReceivedCode::from(code.to_owned()),
                phase: OperationPhase::Deploying,
                message: message.to_owned(),
            }),
            log_bytes: 0,
            created_at: at,
            updated_at: at,
        }
    }

    #[test]
    fn targets_and_environment_only_actions() {
        let (p, s) = parse_target("my-app/web").unwrap();
        assert_eq!((p.as_str(), s.as_deref()), ("my-app", Some("web")));
        assert_eq!(parse_target("my-app").unwrap().1, None);
        for bad in ["my-app/", "my-app/a/b", "/web", "My App"] {
            assert!(parse_target(bad).is_err(), "{bad}");
        }
        let why = environment_only("redeploy", "my-app/web").unwrap();
        assert!(why.contains("whole environment"), "{why}");
        assert!(environment_only("remove", "my-app").is_none());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&view(OperationStatus::Succeeded, None)), 0);
        for code in ["conflict", "deploy_failed", "not_found", "something_new"] {
            assert_eq!(
                exit_code(&view(OperationStatus::Failed, Some((code, "x")))),
                exit::FAILED,
                "{code}"
            );
        }
        assert_eq!(
            exit_code(&view(
                OperationStatus::Failed,
                Some(("docker_unavailable", "x"))
            )),
            exit::DOCKER_UNAVAILABLE
        );
        assert_eq!(
            exit_code(&view(
                OperationStatus::Interrupted,
                Some(("interrupted", "x"))
            )),
            exit::INTERRUPTED
        );
    }

    #[test]
    fn the_end_is_printed_for_people() {
        let slug = Slug::parse("my-app").unwrap();
        let (out, err) = render_end(
            "stop",
            &slug,
            Some("web"),
            &view(OperationStatus::Succeeded, None),
        );
        assert_eq!((out.as_str(), err.as_str()), ("Stopped my-app/web.\n", ""));
        let (out, err) = render_end(
            "start",
            &slug,
            None,
            &view(
                OperationStatus::Failed,
                Some(("conflict", "the environment was removed")),
            ),
        );
        assert_eq!(out, "");
        assert_eq!(
            err,
            "ship start: failed (conflict in deploying): the environment was removed\n\
             Operation: op_01aryz6s41tsv4rrffq69g5fav\n"
        );
        let q = remove_question(&slug, "development");
        assert!(q.contains("named volume") && q.contains("kept"), "{q}");
    }
}
