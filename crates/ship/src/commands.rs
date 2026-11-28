//! Client subcommands: `target`, `projects` and `dial-stdio`; `open` is in
//! [`crate::open`].

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Subcommand, ValueEnum};
use serde::Serialize;
use ship_core::api::{HOST_PATH, PROJECTS_PATH};
use ship_core::{DockerStatus, HostInfo, ProjectList};
use ship_net::{DEFAULT_SOCKET, TARGET_FORMS, Target};

use crate::client::{self, ClientError, Endpoint};
use crate::config::{self, Config, ConfigError};

/// Exit code for a failed command.
pub(crate) const FAILURE: u8 = 1;
/// Exit code for a usage error.
pub(crate) const USAGE: u8 = 2;

/// Output format of list and status commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Output {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Subcommand)]
pub enum TargetCommand {
    /// Add a target: ssh://USER@HOST[:PORT] or local. The first target
    /// added becomes current.
    Add {
        /// Name for the target: lowercase letters, digits and hyphens.
        name: String,
        /// ssh://USER@HOST[:PORT] or local.
        url: String,
        /// The daemon's socket: a local path, or the path on the ssh host
        /// [default: /run/ship/ship.sock].
        #[arg(long, value_name = "PATH")]
        socket: Option<String>,
    },
    /// Make a target current.
    Use { name: String },
    /// List targets; the current one is marked.
    List {
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: Output,
    },
}

#[derive(Debug, clap::Args)]
pub struct ProjectsArgs {
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    output: Output,
    /// Give up after this many seconds, including the ssh connection.
    #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
}

#[derive(Debug, clap::Args)]
#[command(
    after_help = "Exit status: 0 when the daemon answered, whatever Docker's status \
                        (this is a report, not a check); 1 when the daemon could not be \
                        reached or refused; 2 on a usage error."
)]
pub struct HostArgs {
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    output: Output,
    /// Give up after this many seconds, including the ssh connection.
    #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
}

#[derive(Debug, clap::Args)]
pub struct DialStdioArgs {
    /// The daemon's socket.
    #[arg(long, value_name = "PATH", default_value = DEFAULT_SOCKET)]
    socket: PathBuf,
}

pub(crate) fn fail(message: impl std::fmt::Display, code: u8) -> ExitCode {
    eprintln!("ship: {message}");
    ExitCode::from(code)
}

fn config_failure(e: ConfigError) -> ExitCode {
    let code = if e.is_usage() { USAGE } else { FAILURE };
    fail(e, code)
}

pub fn target(command: TargetCommand) -> ExitCode {
    let path = match config::default_location() {
        Ok(p) => p,
        Err(e) => return config_failure(e),
    };
    let mut cfg = match Config::load(&path) {
        Ok(c) => c,
        Err(e) => return config_failure(e),
    };
    match command {
        TargetCommand::Add { name, url, socket } => {
            let current = match cfg.add(&name, &url, socket.as_deref()) {
                Ok(current) => current,
                Err(e) => return config_failure(e),
            };
            if let Err(e) = cfg.save(&path) {
                return config_failure(e);
            }
            let url = &cfg.targets[&name].url;
            if current {
                println!("Added target `{name}` ({url}); it is now the current target.");
            } else {
                println!("Added target `{name}` ({url}). Use it with `ship target use {name}`.");
            }
            ExitCode::SUCCESS
        }
        TargetCommand::Use { name } => {
            if let Err(e) = cfg.use_target(&name) {
                return config_failure(e);
            }
            if let Err(e) = cfg.save(&path) {
                return config_failure(e);
            }
            println!("Current target: `{name}`.");
            ExitCode::SUCCESS
        }
        TargetCommand::List { output } => {
            print!("{}", render_targets(&cfg, output));
            ExitCode::SUCCESS
        }
    }
}

#[derive(Serialize)]
struct TargetRow<'a> {
    name: &'a str,
    url: &'a str,
    socket: Option<&'a str>,
    current: bool,
}

#[derive(Serialize)]
struct TargetListJson<'a> {
    targets: Vec<TargetRow<'a>>,
}

fn render_targets(cfg: &Config, output: Output) -> String {
    let rows: Vec<TargetRow> = cfg
        .targets
        .iter()
        .map(|(name, t)| TargetRow {
            name,
            url: &t.url,
            socket: t.socket.as_deref(),
            current: cfg.current.as_deref() == Some(name.as_str()),
        })
        .collect();
    match output {
        Output::Json => {
            let mut s = serde_json::to_string_pretty(&TargetListJson { targets: rows })
                .expect("target list serialises");
            s.push('\n');
            s
        }
        Output::Text if rows.is_empty() => format!(
            "No targets. Commands use the local daemon at {DEFAULT_SOCKET}.\n\
             Add one with `ship target add NAME {TARGET_FORMS}`.\n"
        ),
        Output::Text => {
            let width = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
            let mut s = String::new();
            for r in rows {
                let mark = if r.current { '*' } else { ' ' };
                s.push_str(&format!("{mark} {:width$}  {}", r.name, r.url));
                if let Some(socket) = r.socket {
                    s.push_str(&format!("  (socket {socket})"));
                }
                s.push('\n');
            }
            s
        }
    }
}

/// The endpoint commands talk to: the current target, or the local daemon
/// at the default socket.
pub(crate) fn endpoint() -> Result<Endpoint, ExitCode> {
    let path = config::default_location().map_err(config_failure)?;
    let cfg = Config::load(&path).map_err(config_failure)?;
    match cfg.current().map_err(config_failure)? {
        Some((name, target)) => Ok(Endpoint {
            name: Some(name.to_owned()),
            target,
        }),
        None => Ok(Endpoint {
            name: None,
            target: Target::Local {
                socket: PathBuf::from(DEFAULT_SOCKET),
            },
        }),
    }
}

pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, ExitCode> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| fail(format!("cannot start the runtime: {e}"), FAILURE))
}

/// `GET path` from the current target as `T`; on failure the message is
/// already printed and the exit code returned.
pub(crate) fn fetch<T: serde::de::DeserializeOwned>(
    path: &str,
    timeout_secs: u64,
) -> Result<T, ExitCode> {
    let endpoint = endpoint()?;
    let rt = runtime()?;
    let timeout = Duration::from_secs(timeout_secs);
    let result: Result<T, ClientError> = rt.block_on(client::get_json(&endpoint, path, timeout));
    // Drop the runtime now so an ssh child is reaped before printing.
    drop(rt);
    result.map_err(|e| fail(e, FAILURE))
}

pub fn projects(args: ProjectsArgs) -> ExitCode {
    match fetch::<ProjectList>(PROJECTS_PATH, args.timeout) {
        Ok(list) => {
            print!("{}", render_projects(&list, args.output));
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

/// Exit 0 whenever the daemon answered: Docker's status is reported, not
/// judged.
pub fn host(args: HostArgs) -> ExitCode {
    match fetch::<HostInfo>(HOST_PATH, args.timeout) {
        Ok(info) => {
            print!("{}", render_host(&info, args.output, CLIENT_VERSION));
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

/// `-o json` is the contract shape (ADR-0004 rule 2): the daemon's
/// [`HostInfo`] pretty-printed and a newline. Text is one block with
/// Docker's status first and, unless `ready`, the daemon's message verbatim.
/// When `client` (this binary's version) is not the daemon's, a `Client:`
/// line says so: after an in-place upgrade the daemon keeps running the old
/// binary until restarted (f-018).
fn render_host(info: &HostInfo, output: Output, client: &str) -> String {
    if output == Output::Json {
        let mut s = serde_json::to_string_pretty(info).expect("host info serialises");
        s.push('\n');
        return s;
    }
    let docker = &info.docker;
    let mut lines = vec![("Docker", docker.status.as_str().to_owned())];
    if docker.status != DockerStatus::Ready
        && let Some(message) = &docker.message
    {
        lines.push(("Message", message.clone()));
    }
    let version = |v: &Option<String>, range: &str| {
        format!("{} (supported {range})", v.as_deref().unwrap_or("unknown"))
    };
    lines.push((
        "Engine",
        version(&docker.engine_version, &docker.supported.engine),
    ));
    lines.push((
        "Compose",
        version(&docker.compose_version, &docker.supported.compose),
    ));
    let c = &info.capacity;
    let cpus = match c.cpus {
        Some(1) => "1 CPU".to_owned(),
        Some(n) => format!("{n} CPUs"),
        None => "unknown CPUs".to_owned(),
    };
    lines.push((
        "Host",
        format!(
            "{} {}, {cpus}, {} memory, {} disk free",
            info.os,
            info.arch,
            gib(c.memory_bytes),
            gib(c.disk_free_bytes)
        ),
    ));
    lines.push((
        "Daemon",
        format!("ship {}, API {}", info.version, info.api_version),
    ));
    if info.version != client {
        lines.push((
            "Client",
            format!(
                "ship {client}, not the daemon's version; if the host was upgraded, \
                 restart the daemon there: sudo systemctl restart ship"
            ),
        ));
    }
    let mut s = String::new();
    for (label, value) in lines {
        s.push_str(&format!("{:10}{value}\n", format!("{label}:")));
    }
    s
}

/// This client's Ship version.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Bytes as GiB with one decimal; `unknown` when absent.
fn gib(bytes: Option<u64>) -> String {
    match bytes {
        #[allow(clippy::cast_precision_loss)] // one decimal is shown
        Some(b) => format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64),
        None => "unknown".to_owned(),
    }
}

/// `-o json` is the contract shape (ADR-0004 rule 2): pretty-printed
/// [`ProjectList`] and a newline, identical over every transport.
fn render_projects(list: &ProjectList, output: Output) -> String {
    match output {
        Output::Json => {
            let mut s = serde_json::to_string_pretty(list).expect("project list serialises");
            s.push('\n');
            s
        }
        Output::Text if list.projects.is_empty() => "No projects.\n".to_owned(),
        Output::Text => {
            let mut rows: Vec<Vec<String>> = Vec::new();
            for p in &list.projects {
                if p.environments.is_empty() {
                    rows.push(vec![
                        p.slug.to_string(),
                        "-".to_owned(),
                        "-".to_owned(),
                        "-".to_owned(),
                        p.id.to_string(),
                    ]);
                }
                for e in &p.environments {
                    rows.push(vec![
                        p.slug.to_string(),
                        e.name.clone(),
                        e.desired_state.clone(),
                        e.status.clone().unwrap_or_else(|| "no services".to_owned()),
                        p.id.to_string(),
                    ]);
                }
            }
            table(&["SLUG", "ENVIRONMENT", "DESIRED", "STATUS", "ID"], &rows)
        }
    }
}

/// `header` and `rows` as left-aligned columns two spaces apart, each line
/// trimmed at the end.
pub(crate) fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let header: Vec<String> = header.iter().map(|h| (*h).to_owned()).collect();
    let mut s = String::new();
    for row in std::iter::once(&header).chain(rows) {
        let mut line = String::new();
        for (cell, w) in row.iter().zip(&widths) {
            line.push_str(cell);
            for _ in cell.chars().count()..*w + 2 {
                line.push(' ');
            }
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    s
}

pub fn dial_stdio(args: DialStdioArgs) -> ExitCode {
    match ship_net::dial_stdio(&args.socket) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ship dial-stdio: {e}");
            ExitCode::from(FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ship_core::{ProjectSummary, Slug, Timestamp};

    #[test]
    fn projects_json_is_the_fixture() {
        let got = render_projects(&ProjectList::default(), Output::Json);
        assert_eq!(got, include_str!("../tests/fixtures/projects-empty.json"));
    }

    #[test]
    fn projects_text() {
        assert_eq!(
            render_projects(&ProjectList::default(), Output::Text),
            "No projects.\n"
        );
        let list: ProjectList =
            serde_json::from_str(include_str!("../tests/fixtures/projects.json")).unwrap();
        assert_eq!(
            render_projects(&list, Output::Text),
            "SLUG     ENVIRONMENT  DESIRED  STATUS              ID\n\
             my-app   development  running  running_unverified  prj_01aryz6s41tsv4rrffq69g5fav\n\
             new-app  development  running  no services         prj_01aryz6s41tsv4rrffq69g5faw\n"
        );
        let bare = ProjectList {
            projects: vec![ProjectSummary {
                id: "prj_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
                slug: Slug::parse("my-app").unwrap(),
                created_at: Timestamp::from_unix_millis(1_469_918_176_385).unwrap(),
                environments: Vec::new(),
            }],
        };
        assert_eq!(
            render_projects(&bare, Output::Text),
            "SLUG    ENVIRONMENT  DESIRED  STATUS  ID\n\
             my-app  -            -        -       prj_01aryz6s41tsv4rrffq69g5fav\n"
        );
    }

    #[test]
    fn projects_json_is_the_fixture_with_entries() {
        let text = include_str!("../tests/fixtures/projects.json");
        let list: ProjectList = serde_json::from_str(text).unwrap();
        assert_eq!(render_projects(&list, Output::Json), text);
    }

    const HOST_FIXTURE: &str = include_str!("../tests/fixtures/host.json");

    fn fixture_host() -> HostInfo {
        serde_json::from_str(HOST_FIXTURE).unwrap()
    }

    #[test]
    fn host_json_is_the_fixture() {
        assert_eq!(render_host(&fixture_host(), Output::Json, "0.0.0"), HOST_FIXTURE);
        // The contract shape does not change when the versions differ.
        assert_eq!(render_host(&fixture_host(), Output::Json, "9.9.9"), HOST_FIXTURE);
    }

    #[test]
    fn host_text_shows_a_client_daemon_version_difference() {
        let text = render_host(&fixture_host(), Output::Text, "0.0.1");
        let last = text.lines().last().unwrap();
        assert!(last.starts_with("Client:   ship 0.0.1, not the daemon's version"), "{text}");
        assert!(last.ends_with("sudo systemctl restart ship"), "{text}");
        assert!(text.contains("Daemon:   ship 0.0.0, API v1\n"), "{text}");
    }

    #[test]
    fn host_text_ready() {
        assert_eq!(
            render_host(&fixture_host(), Output::Text, "0.0.0"),
            "Docker:   ready\n\
             Engine:   28.3.2 (supported >= 24.0.0, < 30.0.0)\n\
             Compose:  2.39.1 (supported >= 2.20.0, < 6.0.0)\n\
             Host:     linux x86_64, 4 CPUs, 8.0 GiB memory, 100.0 GiB disk free\n\
             Daemon:   ship 0.0.0, API v1\n"
        );
    }

    #[test]
    fn host_text_unreachable_puts_status_then_message_verbatim() {
        let mut info = fixture_host();
        let message = "Docker Engine is not reachable: x. Start Docker (systemctl start docker).";
        info.docker.status = DockerStatus::Unreachable;
        info.docker.engine_version = None;
        info.docker.compose_version = None;
        info.docker.message = Some(message.to_owned());
        info.capacity = ship_core::HostCapacity {
            cpus: Some(1),
            memory_bytes: None,
            disk_free_bytes: None,
        };
        let text = render_host(&info, Output::Text, "0.0.0");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Docker:   unreachable");
        assert_eq!(lines[1], format!("Message:  {message}"));
        assert_eq!(
            lines[2],
            "Engine:   unknown (supported >= 24.0.0, < 30.0.0)"
        );
        assert_eq!(lines[3], "Compose:  unknown (supported >= 2.20.0, < 6.0.0)");
        assert_eq!(
            lines[4],
            "Host:     linux x86_64, 1 CPU, unknown memory, unknown disk free"
        );
    }

    #[test]
    fn host_text_unsupported_version_shows_message() {
        let mut info = fixture_host();
        info.docker.status = DockerStatus::UnsupportedVersion;
        info.docker.engine_version = Some("20.10.24".to_owned());
        info.docker.message = Some("Docker Engine 20.10.24 is outside >= 24.0.0".to_owned());
        let text = render_host(&info, Output::Text, "0.0.0");
        assert!(text.starts_with(
            "Docker:   unsupported_version\n\
             Message:  Docker Engine 20.10.24 is outside >= 24.0.0\n\
             Engine:   20.10.24 (supported"
        ));
    }

    #[test]
    fn gib_formatting() {
        assert_eq!(gib(None), "unknown");
        assert_eq!(gib(Some(0)), "0.0 GiB");
        assert_eq!(gib(Some(16_314_660 * 1024)), "15.6 GiB");
    }

    #[test]
    fn targets_render() {
        let mut cfg = Config::default();
        assert!(render_targets(&cfg, Output::Text).starts_with("No targets."));
        cfg.add("lab", "ssh://deploy@lab", None).unwrap();
        cfg.add("here", "local", Some("/tmp/s.sock")).unwrap();
        assert_eq!(
            render_targets(&cfg, Output::Text),
            "  here  local  (socket /tmp/s.sock)\n* lab   ssh://deploy@lab\n"
        );
        let v: serde_json::Value =
            serde_json::from_str(&render_targets(&cfg, Output::Json)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "targets": [
                { "name": "here", "url": "local", "socket": "/tmp/s.sock", "current": false },
                { "name": "lab", "url": "ssh://deploy@lab", "socket": null, "current": true },
            ]})
        );
    }
}
