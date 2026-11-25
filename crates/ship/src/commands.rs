//! Client subcommands: `target`, `projects` and `dial-stdio`; `open` is in
//! [`crate::open`].

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Subcommand, ValueEnum};
use serde::Serialize;
use ship_core::ProjectList;
use ship_core::api::PROJECTS_PATH;
use ship_net::{DEFAULT_SOCKET, TARGET_FORMS, Target};

use crate::client::{self, ClientError, Endpoint};
use crate::config::{self, Config, ConfigError};

/// Exit code for a failed command.
pub(crate) const FAILURE: u8 = 1;
/// Exit code for a usage error.
const USAGE: u8 = 2;

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

pub fn projects(args: ProjectsArgs) -> ExitCode {
    let endpoint = match endpoint() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let timeout = Duration::from_secs(args.timeout);
    let result: Result<ProjectList, ClientError> =
        rt.block_on(client::get_json(&endpoint, PROJECTS_PATH, timeout));
    // Drop the runtime now so an ssh child is reaped before printing.
    drop(rt);
    match result {
        Ok(list) => {
            print!("{}", render_projects(&list, args.output));
            ExitCode::SUCCESS
        }
        Err(e) => fail(e, FAILURE),
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
            let rows: Vec<[String; 3]> = list
                .projects
                .iter()
                .map(|p| {
                    [
                        p.slug.to_string(),
                        p.id.to_string(),
                        p.created_at.to_string(),
                    ]
                })
                .collect();
            let header = ["SLUG", "ID", "CREATED"].map(str::to_owned);
            let mut widths = [0usize; 3];
            for row in std::iter::once(&header).chain(&rows) {
                for (w, cell) in widths.iter_mut().zip(row) {
                    *w = (*w).max(cell.len());
                }
            }
            let mut s = String::new();
            for row in std::iter::once(&header).chain(&rows) {
                let line = format!(
                    "{:w0$}  {:w1$}  {}",
                    row[0],
                    row[1],
                    row[2],
                    w0 = widths[0],
                    w1 = widths[1]
                );
                s.push_str(line.trim_end());
                s.push('\n');
            }
            s
        }
    }
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
        let list = ProjectList {
            projects: vec![ProjectSummary {
                id: "prj_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
                slug: Slug::parse("my-app").unwrap(),
                created_at: Timestamp::from_unix_millis(1_469_918_176_385).unwrap(),
            }],
        };
        assert_eq!(
            render_projects(&list, Output::Text),
            "SLUG    ID                              CREATED\n\
             my-app  prj_01aryz6s41tsv4rrffq69g5fav  2016-07-30T22:36:16.385Z\n"
        );
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
