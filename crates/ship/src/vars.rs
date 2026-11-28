//! `ship vars set|unset|list` and `ship secrets set|unset|list`: config
//! values and write-only secrets (ADR-0009), for a project, an environment
//! (`--env`) or one of its services (`--service`).
//!
//! A secret's value is never taken as an argument, where it would land in
//! shell history and the process list (rule 2): `ship secrets set` reads it
//! from the terminal with echo off (`--prompt`) or from stdin (`--stdin`),
//! and refuses `NAME=VALUE` or any further argument before connecting,
//! without printing what it refused. The daemon answers names and
//! timestamps only. A change applies at the next deployment; until then the
//! services running with older values are named.

use std::io::{self, BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::time::Duration;

use clap::Subcommand;
use ship_core::api::{
    SecretList, SetSecretRequest, SetVarRequest, UnsetRequest, VarList, config_query, secrets_path,
    secrets_unset_path, vars_path, vars_unset_path,
};
use ship_core::config::{check_name, check_secret_value, check_value};
use ship_core::{OperationStatus, Slug};

use crate::client::{self, Endpoint};
use crate::commands::{FAILURE, Output, endpoint, fail, runtime};

/// Exit code for a usage error.
const USAGE: u8 = 2;

/// What a value is set for.
#[derive(Debug, Clone, clap::Args)]
pub struct ScopeArgs {
    /// The project.
    #[arg(long, value_name = "P")]
    project: String,
    /// An environment of the project instead of the whole project
    /// (Phase 1: development).
    #[arg(long = "env", value_name = "E")]
    env: Option<String>,
    /// One service of the environment (development unless --env names
    /// another); a service is known from its first deployment.
    #[arg(long, value_name = "S")]
    service: Option<String>,
    /// Give up after this many seconds, including the ssh connection.
    #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
}

impl ScopeArgs {
    fn slug(&self) -> Result<Slug, ExitCode> {
        Slug::parse(&self.project).map_err(|e| fail(format!("--project: {e}"), USAGE))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout)
    }

    /// "project my-app", "environment development of my-app", …
    fn describe(&self) -> String {
        let p = &self.project;
        match (&self.service, &self.env) {
            (Some(s), e) => format!(
                "service {s} in {} of {p}",
                e.as_deref().unwrap_or("development")
            ),
            (None, Some(e)) => format!("environment {e} of {p}"),
            (None, None) => format!("project {p}"),
        }
    }

    fn query(&self) -> String {
        config_query(self.env.as_deref(), self.service.as_deref())
    }
}

#[derive(Debug, Subcommand)]
pub enum VarsCommand {
    /// Set a config value: NAME=VALUE. Config values are not secret: they
    /// are shown by `ship vars list`. Running services keep their values
    /// until the next deployment.
    Set {
        /// NAME=VALUE; NAME is an environment variable name.
        assignment: String,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Remove a config value from the scope.
    Unset {
        name: String,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// List the config values set in the scope.
    List {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: Output,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecretsCommand {
    /// Set a secret, read with --prompt (from the terminal, not echoed) or
    /// --stdin (all of it, less one final newline); never from an argument.
    #[command(
        after_help = "Phase 1 keeps secret values in files under the daemon's state directory, \
                      readable only by the daemon's user: file permissions, not encryption. A \
                      service can read its own secrets, and anyone with Docker access on the \
                      host can read every injected one."
    )]
    Set {
        /// The secret's name: an environment variable name.
        name: String,
        /// Refused: a value is never an argument.
        #[arg(hide = true, num_args = 0.., value_name = "VALUE")]
        rest: Vec<String>,
        /// Read the value from the terminal with echo off.
        #[arg(long, conflicts_with = "stdin")]
        prompt: bool,
        /// Read the value from stdin.
        #[arg(long)]
        stdin: bool,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Remove a secret from the scope, and its value from the daemon's
    /// store.
    Unset {
        name: String,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// List the secrets set in the scope: names and when they were set,
    /// never values.
    List {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: Output,
    },
}

pub fn vars(command: VarsCommand) -> ExitCode {
    match command {
        VarsCommand::Set { assignment, scope } => {
            let Some((name, value)) = assignment.split_once('=') else {
                return fail("give the value as NAME=VALUE", USAGE);
            };
            if let Err(e) = check_name(name) {
                return fail(e, USAGE);
            }
            if let Err(e) = check_value(value) {
                return fail(e, USAGE);
            }
            let request = SetVarRequest {
                env: scope.env.clone(),
                service: scope.service.clone(),
                name: name.to_owned(),
                value: value.to_owned(),
            };
            change(
                &scope,
                Change::Vars,
                true,
                serde_json::to_vec(&request).expect("a request always serialises"),
                &format!("{name} is set"),
            )
        }
        VarsCommand::Unset { name, scope } => unset(&scope, Change::Vars, &name),
        VarsCommand::List { scope, output } => list(&scope, Change::Vars, output),
    }
}

pub fn secrets(command: SecretsCommand) -> ExitCode {
    match command {
        SecretsCommand::Set {
            name,
            rest,
            prompt,
            stdin,
            scope,
        } => {
            if let Some(why) = refuse_argument(&name, &rest) {
                return fail(why, USAGE);
            }
            if let Err(e) = check_name(&name) {
                return fail(e, USAGE);
            }
            let value = match (prompt, stdin) {
                (true, _) => read_prompt(&name),
                (_, true) => read_stdin(),
                (false, false) => {
                    return fail(
                        "give the value with --prompt or --stdin; it is never an argument",
                        USAGE,
                    );
                }
            };
            let value = match value {
                Ok(v) => v,
                Err(why) => return fail(why, USAGE),
            };
            if let Err(e) = check_secret_value(&value) {
                return fail(e, USAGE);
            }
            let request = SetSecretRequest {
                env: scope.env.clone(),
                service: scope.service.clone(),
                name: name.clone(),
                value,
            };
            change(
                &scope,
                Change::Secrets,
                true,
                serde_json::to_vec(&request).expect("a request always serialises"),
                &format!("secret {name} is set"),
            )
        }
        SecretsCommand::Unset { name, scope } => unset(&scope, Change::Secrets, &name),
        SecretsCommand::List { scope, output } => list(&scope, Change::Secrets, output),
    }
}

/// Why `ship secrets set` refuses its arguments, if it does: a `NAME=VALUE`
/// or anything after the name. The message never repeats them.
pub fn refuse_argument(name: &str, rest: &[String]) -> Option<&'static str> {
    (name.contains('=') || !rest.is_empty()).then_some(
        "a secret value is never taken as an argument, where it would stay in shell history \
         and the process list; give only the name, with --prompt or --stdin",
    )
}

/// Whether Clap's `error` for `args` (without the program name) is
/// replaced by [`parse_refusal`]: any for a `ship secrets` command (the
/// first argument that is not a flag is `secrets`), whose arguments may
/// hold a value, except help asked for by `-h`, `--help` or `help`, or
/// shown for a missing subcommand. A value such as `-hx` asks Clap for
/// help too, and is refused.
pub fn refuses_parse(error: &clap::Error, args: &[std::ffi::OsString]) -> bool {
    use clap::error::ErrorKind;
    let secrets = args
        .iter()
        .find(|a| !a.as_encoded_bytes().starts_with(b"-"))
        .is_some_and(|a| a == "secrets");
    if !secrets {
        return false;
    }
    match error.kind() {
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => false,
        ErrorKind::DisplayHelp => !args
            .iter()
            .any(|a| a == "-h" || a == "--help" || a == "help"),
        _ => true,
    }
}

/// What `ship secrets …` prints when its arguments do not parse, in place
/// of Clap's diagnostic, which would quote them: the kind of the problem
/// and never an argument.
pub fn parse_refusal(kind: clap::error::ErrorKind) -> String {
    let what = kind.as_str().unwrap_or("the arguments are not valid");
    format!(
        "{what}. The arguments are not repeated: a secret value is never taken as an argument, \
         where it would stay in shell history and the process list; give only the name, with \
         --prompt or --stdin. See `ship secrets set --help`."
    )
}

/// The value on stdin: all of it, less one final `\n` or `\r\n`.
fn read_stdin() -> Result<String, String> {
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("reading stdin: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| "the value is not UTF-8".to_owned())?;
    Ok(strip_newline(text))
}

fn strip_newline(mut text: String) -> String {
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    text
}

/// The value typed on the controlling terminal, with echo off.
fn read_prompt(name: &str) -> Result<String, String> {
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|_| "--prompt needs a terminal; use --stdin".to_owned())?;
    let fd = tty.as_raw_fd();
    // SAFETY: a zeroed termios is a valid value to pass to tcgetattr, which
    // fills it; fd is an open terminal for the life of `tty`.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
        return Err("--prompt needs a terminal; use --stdin".to_owned());
    }
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    quiet.c_lflag |= libc::ICANON;
    // SAFETY: as above; `quiet` is a copy of the terminal's own settings.
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &quiet) } != 0 {
        return Err("could not turn the terminal's echo off".to_owned());
    }
    let mut out = &tty;
    let _ = write!(out, "Value of {name} (not shown): ");
    let _ = out.flush();
    let mut line = String::new();
    let read = io::BufReader::new(&tty).read_line(&mut line);
    // SAFETY: restores the settings read above on the same terminal.
    unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &saved) };
    let _ = writeln!(out);
    read.map_err(|e| format!("reading the terminal: {e}"))?;
    Ok(strip_newline(line))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Vars,
    Secrets,
}

impl Change {
    fn paths(self, slug: &Slug) -> (String, String) {
        match self {
            Change::Vars => (vars_path(slug), vars_unset_path(slug)),
            Change::Secrets => (secrets_path(slug), secrets_unset_path(slug)),
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Change::Vars => "vars",
            Change::Secrets => "secrets",
        }
    }
}

fn unset(scope: &ScopeArgs, what: Change, name: &str) -> ExitCode {
    if let Err(e) = check_name(name) {
        return fail(e, USAGE);
    }
    let request = UnsetRequest {
        env: scope.env.clone(),
        service: scope.service.clone(),
        name: name.to_owned(),
    };
    let done = match what {
        Change::Vars => format!("{name} is unset"),
        Change::Secrets => format!("secret {name} is unset"),
    };
    change(
        scope,
        what,
        false,
        serde_json::to_vec(&request).expect("a request always serialises"),
        &done,
    )
}

/// Submits a change (to the set path when `set`, else the unset path),
/// follows its operation and prints `done` and the services still running
/// with older values.
fn change(scope: &ScopeArgs, what: Change, set: bool, body: Vec<u8>, done: &str) -> ExitCode {
    let slug = match scope.slug() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let (set_path, unset_path) = what.paths(&slug);
    let path = if set { set_path.clone() } else { unset_path };
    let endpoint = match endpoint() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let timeout = scope.timeout();
    let result = rt.block_on(async {
        let op = crate::settings::submit(&endpoint, &path, body, what.kind(), timeout).await?;
        let stale = stale(&endpoint, what, &set_path, scope, timeout).await;
        Ok::<_, String>((op, stale))
    });
    drop(rt);
    match result {
        Ok((op, stale)) if op.status == OperationStatus::Succeeded => {
            println!("{done} for the {}.", scope.describe());
            println!("It applies at the next deployment; running services are not changed.");
            if let Some(names) = stale.filter(|n| !n.is_empty()) {
                println!(
                    "Running with older values until redeployed: {}",
                    names.join(", ")
                );
            }
            ExitCode::SUCCESS
        }
        Ok((op, _)) => {
            let why = op
                .failure
                .map(|f| format!("{} ({})", f.message, f.code))
                .unwrap_or_else(|| format!("the operation ended {}", op.status));
            fail(format!("nothing changed: {why}"), FAILURE)
        }
        Err(e) => fail(e, FAILURE),
    }
}

/// The stale services the list of the scope names, if it can be read.
async fn stale(
    endpoint: &Endpoint,
    what: Change,
    path: &str,
    scope: &ScopeArgs,
    timeout: Duration,
) -> Option<Vec<String>> {
    let path = format!("{path}{}", scope.query());
    match what {
        Change::Vars => client::get_json::<VarList>(endpoint, &path, timeout)
            .await
            .ok()
            .map(|l| l.stale_services),
        Change::Secrets => client::get_json::<SecretList>(endpoint, &path, timeout)
            .await
            .ok()
            .map(|l| l.stale_services),
    }
}

fn list(scope: &ScopeArgs, what: Change, output: Output) -> ExitCode {
    let slug = match scope.slug() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let path = format!("{}{}", what.paths(&slug).0, scope.query());
    let endpoint = match endpoint() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let timeout = scope.timeout();
    let text = rt.block_on(async {
        match what {
            Change::Vars => client::get_json::<VarList>(&endpoint, &path, timeout)
                .await
                .map(|l| render_vars(&l, scope, output)),
            Change::Secrets => client::get_json::<SecretList>(&endpoint, &path, timeout)
                .await
                .map(|l| render_secrets(&l, scope, output)),
        }
    });
    drop(rt);
    match text {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(e, FAILURE),
    }
}

fn json_text<T: serde::Serialize>(value: &T) -> String {
    let mut s = serde_json::to_string_pretty(value).expect("a list always serialises");
    s.push('\n');
    s
}

fn stale_line(names: &[String]) -> String {
    if names.is_empty() {
        String::new()
    } else {
        format!(
            "Running with older values until redeployed: {}\n",
            names.join(", ")
        )
    }
}

/// `-o json` is the daemon's list; text is `NAME=VALUE (set …)` per line.
pub fn render_vars(list: &VarList, scope: &ScopeArgs, output: Output) -> String {
    if output == Output::Json {
        return json_text(list);
    }
    let mut out = String::new();
    if list.vars.is_empty() {
        out.push_str(&format!(
            "No config values set for the {}.\n",
            scope.describe()
        ));
    }
    for v in &list.vars {
        out.push_str(&format!("{}={}  (set {})\n", v.name, v.value, v.updated_at));
    }
    out.push_str(&stale_line(&list.stale_services));
    out
}

/// `-o json` is the daemon's list; text is `NAME (set …)` per line.
pub fn render_secrets(list: &SecretList, scope: &ScopeArgs, output: Output) -> String {
    if output == Output::Json {
        return json_text(list);
    }
    let mut out = String::new();
    if list.secrets.is_empty() {
        out.push_str(&format!("No secrets set for the {}.\n", scope.describe()));
    }
    for s in &list.secrets {
        out.push_str(&format!("{}  (set {})\n", s.name, s.updated_at));
    }
    out.push_str(&stale_line(&list.stale_services));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_holding_a_value_are_refused_without_quoting_them() {
        let why = refuse_argument("TOKEN=hunter2", &[]).unwrap();
        assert!(!why.contains("hunter2"));
        assert!(refuse_argument("TOKEN", &["hunter2".to_owned()]).is_some());
        assert_eq!(refuse_argument("TOKEN", &[]), None);
    }

    #[test]
    fn one_final_newline_is_dropped() {
        assert_eq!(strip_newline("v\n".to_owned()), "v");
        assert_eq!(strip_newline("v\r\n".to_owned()), "v");
        assert_eq!(strip_newline("v\n\n".to_owned()), "v\n");
        assert_eq!(strip_newline("v".to_owned()), "v");
    }

    #[test]
    fn scopes_are_described() {
        let scope = |env: Option<&str>, service: Option<&str>| ScopeArgs {
            project: "my-app".to_owned(),
            env: env.map(str::to_owned),
            service: service.map(str::to_owned),
            timeout: 30,
        };
        assert_eq!(scope(None, None).describe(), "project my-app");
        assert_eq!(
            scope(Some("development"), None).describe(),
            "environment development of my-app"
        );
        assert_eq!(
            scope(None, Some("web")).describe(),
            "service web in development of my-app"
        );
        assert_eq!(scope(None, Some("web")).query(), "?service=web");
    }
}
