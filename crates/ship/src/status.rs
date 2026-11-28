//! `ship status`, `ship deployments` and `ship logs` (OVERVIEW §6). Each
//! `-o json` is a contract shape (ADR-0004 rule 2): `status.json`,
//! `deployments.json` and `logs.jsonl`.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use ship_core::Slug;
use ship_core::api::{
    DeploymentList, LogLineView, StatusView, deployments_path, env_query, logs_path, logs_query,
    status_path,
};

use crate::client::{self, ClientError};
use crate::commands::{FAILURE, Output, USAGE, endpoint, fail, fetch, runtime, table};

#[derive(Debug, clap::Args)]
#[command(
    after_help = "Exit status: 0 when the daemon answered, whatever the services' \
                        status (this is a report, not a check); 1 when the daemon could \
                        not be reached or refused; 2 on a usage error."
)]
pub struct StatusArgs {
    /// The project's slug.
    project: String,
    /// The environment [default: development].
    #[arg(long, value_name = "E")]
    env: Option<String>,
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    output: Output,
    /// Give up after this many seconds, including the ssh connection.
    #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
}

#[derive(Debug, clap::Args)]
pub struct DeploymentsArgs {
    /// The project's slug.
    project: String,
    /// The environment [default: development].
    #[arg(long, value_name = "E")]
    env: Option<String>,
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    output: Output,
    /// Give up after this many seconds, including the ssh connection.
    #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
}

#[derive(Debug, clap::Args)]
pub struct LogsArgs {
    /// PROJECT, or PROJECT/SERVICE for one service.
    #[arg(value_name = "PROJECT[/SERVICE]")]
    target: String,
    /// The environment [default: development].
    #[arg(long, value_name = "E")]
    env: Option<String>,
    /// Keep printing lines as they are written, until interrupted.
    #[arg(short = 'f', long)]
    follow: bool,
    /// Only this many lines from the end of each container's log.
    #[arg(long, value_name = "N")]
    tail: Option<u32>,
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    output: Output,
    /// Give up after this many seconds waiting for the daemon to answer,
    /// including the ssh connection; lines then stream with no limit.
    #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
}

/// A project slug, or the usage error.
fn project_slug(text: &str) -> Result<Slug, ExitCode> {
    Slug::parse(text).map_err(|e| fail(format!("`{text}` is not a project slug: {e}"), USAGE))
}

/// `PROJECT[/SERVICE]`.
fn parse_target(text: &str) -> Result<(Slug, Option<String>), ExitCode> {
    match text.split_once('/') {
        None => Ok((project_slug(text)?, None)),
        Some((project, service)) => {
            if service.is_empty() || service.contains('/') {
                return Err(fail(
                    format!("`{text}` must be PROJECT or PROJECT/SERVICE"),
                    USAGE,
                ));
            }
            Ok((project_slug(project)?, Some(service.to_owned())))
        }
    }
}

fn pretty(v: &impl serde::Serialize) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("a view serialises");
    s.push('\n');
    s
}

pub fn status(args: StatusArgs) -> ExitCode {
    let slug = match project_slug(&args.project) {
        Ok(slug) => slug,
        Err(code) => return code,
    };
    let path = format!("{}{}", status_path(&slug), env_query(args.env.as_deref()));
    match fetch::<StatusView>(&path, args.timeout) {
        Ok(view) => {
            print!("{}", render_status(&view, args.output));
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

pub fn deployments(args: DeploymentsArgs) -> ExitCode {
    let slug = match project_slug(&args.project) {
        Ok(slug) => slug,
        Err(code) => return code,
    };
    let path = format!(
        "{}{}",
        deployments_path(&slug),
        env_query(args.env.as_deref())
    );
    match fetch::<DeploymentList>(&path, args.timeout) {
        Ok(list) => {
            print!("{}", render_deployments(&list, args.output));
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

/// `-o json` is `status.json`. Text keeps the three facts apart: what was
/// asked, what the last deployment did, what Docker reports now.
pub(crate) fn render_status(view: &StatusView, output: Output) -> String {
    if output == Output::Json {
        return pretty(view);
    }
    let mut s = format!(
        "Project:     {} ({})\nEnvironment: {}\nDesired:     {}\n",
        view.project.slug, view.project.id, view.environment.name, view.environment.desired_state
    );
    match &view.last_deployment {
        None => s.push_str("Deployment:  none yet\n"),
        Some(d) if d.action == "remove" => {
            let at = d.ended_at.unwrap_or(d.started_at);
            s.push_str(&format!(
                "Deployment:  removed at {at} ({}); `ship push --recreate` brings it back\n",
                d.id
            ));
        }
        Some(d) => {
            let result = d.result.as_deref().unwrap_or("in progress");
            let at = d.ended_at.unwrap_or(d.started_at);
            let release = d
                .release_id
                .map_or_else(|| "no release".to_owned(), |r| format!("release {r}"));
            s.push_str(&format!(
                "Deployment:  {result} at {at} ({}, {release})\n",
                d.id
            ));
        }
    }
    if view.services.is_empty() {
        s.push_str("Services:    none\n");
        return s;
    }
    s.push('\n');
    let rows: Vec<Vec<String>> = view
        .services
        .iter()
        .map(|svc| {
            let status = if svc.stale_config {
                format!("{} (stale config)", svc.status)
            } else {
                svc.status.clone()
            };
            let endpoints = if svc.endpoints.is_empty() {
                "not configured".to_owned()
            } else {
                svc.endpoints
                    .iter()
                    .map(|e| e.address.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            vec![
                svc.name.clone(),
                svc.kind.clone(),
                svc.desired_state.clone(),
                status,
                svc.restarts.to_string(),
                endpoints,
            ]
        })
        .collect();
    s.push_str(&table(
        &[
            "SERVICE", "KIND", "DESIRED", "STATUS", "RESTARTS", "ENDPOINT",
        ],
        &rows,
    ));
    s
}

/// `-o json` is `deployments.json`. Text: one row per deployment, newest
/// first, each failure on the line below it.
pub(crate) fn render_deployments(list: &DeploymentList, output: Output) -> String {
    if output == Output::Json {
        return pretty(list);
    }
    if list.deployments.is_empty() {
        return "No deployments.\n".to_owned();
    }
    let rows: Vec<Vec<String>> = list
        .deployments
        .iter()
        .map(|d| {
            vec![
                d.started_at.to_string(),
                d.action.clone(),
                d.result.clone().unwrap_or_else(|| "in progress".to_owned()),
                d.release_id
                    .map_or_else(|| "-".to_owned(), |r| r.to_string()),
                d.id.to_string(),
            ]
        })
        .collect();
    let text = table(
        &["STARTED", "ACTION", "RESULT", "RELEASE", "DEPLOYMENT"],
        &rows,
    );
    let mut s = String::new();
    let mut lines = text.lines();
    if let Some(header) = lines.next() {
        s.push_str(header);
        s.push('\n');
    }
    for (line, d) in lines.zip(&list.deployments) {
        s.push_str(line);
        s.push('\n');
        if let Some(f) = &d.failure {
            s.push_str(&format!("  {} in {}: {}\n", f.code, f.phase, f.message));
        }
    }
    s
}

/// One log line for people: `SERVICE | line`.
pub(crate) fn render_log_text(line: &LogLineView) -> String {
    format!("{} | {}\n", line.service, line.line)
}

/// One log line for machines: a `logs.jsonl` line.
pub(crate) fn render_log_json(line: &LogLineView) -> String {
    let mut s = serde_json::to_string(line).expect("a log line serialises");
    s.push('\n');
    s
}

/// Why printing logs stopped.
enum Stop {
    Client(ClientError),
    /// stdout or stderr closed (`| head`): not a failure.
    Closed,
}

pub fn logs(args: LogsArgs) -> ExitCode {
    let (slug, service) = match parse_target(&args.target) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let path = format!(
        "{}{}",
        logs_path(&slug),
        logs_query(
            args.env.as_deref(),
            service.as_deref(),
            args.follow,
            args.tail
        )
    );
    let endpoint = match endpoint() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let timeout = Duration::from_secs(args.timeout);
    let output = args.output;
    let result = rt.block_on(async {
        let mut stream = client::get_stream(&endpoint, &path, &[], timeout)
            .await
            .map_err(Stop::Client)?;
        let mut pending: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next_chunk().await {
            let chunk = chunk.map_err(|error| {
                Stop::Client(ClientError::Transport {
                    endpoint: Box::new(endpoint.clone()),
                    error: Box::new(error),
                })
            })?;
            pending.extend_from_slice(&chunk);
            while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = pending.drain(..=end).collect();
                print_line(&raw[..raw.len() - 1], output)?;
            }
        }
        if !pending.is_empty() {
            print_line(&pending, output)?;
        }
        Ok(())
    });
    drop(rt);
    match result {
        Ok(()) | Err(Stop::Closed) => ExitCode::SUCCESS,
        Err(Stop::Client(e)) => fail(e, FAILURE),
    }
}

/// Prints one NDJSON line from the daemon: with `-o json` on stdout, as
/// text on stdout or stderr after the stream it came from.
fn print_line(raw: &[u8], output: Output) -> Result<(), Stop> {
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let line: LogLineView = serde_json::from_slice(raw).map_err(|e| {
        Stop::Client(ClientError::BadResponse(format!(
            "a log line is not valid: {e}"
        )))
    })?;
    let written = match output {
        Output::Json => {
            let mut out = std::io::stdout().lock();
            out.write_all(render_log_json(&line).as_bytes())
                .and_then(|()| out.flush())
        }
        Output::Text if line.stream == "stderr" => {
            let mut err = std::io::stderr().lock();
            err.write_all(render_log_text(&line).as_bytes())
                .and_then(|()| err.flush())
        }
        Output::Text => {
            let mut out = std::io::stdout().lock();
            out.write_all(render_log_text(&line).as_bytes())
                .and_then(|()| out.flush())
        }
    };
    written.map_err(|_| Stop::Closed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = include_str!("../tests/fixtures/status.json");
    const NEVER: &str = include_str!("../tests/fixtures/status-never-deployed.json");
    const DEPLOYMENTS: &str = include_str!("../tests/fixtures/deployments.json");
    const LOGS: &str = include_str!("../tests/fixtures/logs.jsonl");

    #[test]
    fn json_is_the_fixture() {
        for text in [STATUS, NEVER] {
            let view: StatusView = serde_json::from_str(text).unwrap();
            assert_eq!(render_status(&view, Output::Json), text);
        }
        let list: DeploymentList = serde_json::from_str(DEPLOYMENTS).unwrap();
        assert_eq!(render_deployments(&list, Output::Json), DEPLOYMENTS);
        let mut out = String::new();
        for raw in LOGS.lines() {
            let line: LogLineView = serde_json::from_str(raw).unwrap();
            out.push_str(&render_log_json(&line));
        }
        assert_eq!(out, LOGS);
    }

    #[test]
    fn status_text_keeps_the_three_facts_apart() {
        let view: StatusView = serde_json::from_str(STATUS).unwrap();
        assert_eq!(
            render_status(&view, Output::Text),
            "Project:     my-app (prj_01aryz6s41tsv4rrffq69g5fav)\n\
             Environment: development\n\
             Desired:     running\n\
             Deployment:  succeeded at 2026-01-02T03:04:45.000Z \
             (dep_01aryz6s41tsv4rrffq69g5fav, release rel_01aryz6s41tsv4rrffq69g5fav)\n\
             \n\
             SERVICE  KIND    DESIRED  STATUS              RESTARTS  ENDPOINT\n\
             web      web     running  healthy             0         192.0.2.10:8080\n\
             worker   worker  running  running_unverified  2         not configured\n"
        );
        let mut removed = view.clone();
        if let Some(d) = removed.last_deployment.as_mut() {
            d.action = "remove".to_owned();
            d.release_id = None;
        }
        let text = render_status(&removed, Output::Text);
        assert!(
            text.contains(
                "Deployment:  removed at 2026-01-02T03:04:45.000Z \
                 (dep_01aryz6s41tsv4rrffq69g5fav); `ship push --recreate` brings it back\n"
            ),
            "{text}"
        );
        let mut stale = view.clone();
        stale.services[0].stale_config = true;
        assert!(render_status(&stale, Output::Text).contains("healthy (stale config)"));

        let never: StatusView = serde_json::from_str(NEVER).unwrap();
        let text = render_status(&never, Output::Text);
        assert!(text.contains("Deployment:  none yet\n"), "{text}");
        assert!(text.ends_with("Services:    none\n"), "{text}");
    }

    #[test]
    fn deployments_text_shows_each_failure() {
        let list: DeploymentList = serde_json::from_str(DEPLOYMENTS).unwrap();
        assert_eq!(
            render_deployments(&list, Output::Text),
            "STARTED                   ACTION  RESULT     RELEASE                         DEPLOYMENT\n\
             2026-01-03T03:04:05.000Z  deploy  failed     -                               dep_01aryz6s41tsv4rrffq69g5faw\n\
             \x20 build_failed in building: the image build exited 1\n\
             2026-01-02T03:04:05.000Z  deploy  succeeded  rel_01aryz6s41tsv4rrffq69g5fav  dep_01aryz6s41tsv4rrffq69g5fav\n"
        );
        assert_eq!(
            render_deployments(&DeploymentList::default(), Output::Text),
            "No deployments.\n"
        );
    }

    #[test]
    fn log_text_names_the_service() {
        let line: LogLineView = serde_json::from_str(LOGS.lines().next().unwrap()).unwrap();
        assert_eq!(render_log_text(&line), "web | listening on :8080\n");
    }

    #[test]
    fn targets() {
        let (p, s) = parse_target("my-app").unwrap();
        assert_eq!((p.as_str(), s), ("my-app", None));
        let (p, s) = parse_target("my-app/web").unwrap();
        assert_eq!((p.as_str(), s.as_deref()), ("my-app", Some("web")));
        for bad in ["My App", "my-app/", "my-app/a/b", "/web"] {
            assert!(parse_target(bad).is_err(), "{bad}");
        }
    }
}
