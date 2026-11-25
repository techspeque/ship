//! The `ship` binary: client commands and `ship daemon` dispatch.
//!
//! Phase 0: `--version` / `-V`, `ship daemon`, `ship target`, `ship host`,
//! `ship projects`, `ship open`, `ship dial-stdio` and `ship service`. Exit
//! codes: 0 success, 1 failure, 2 usage error.

mod client;
mod commands;
mod config;
mod open;
mod service;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: &str = env!("SHIP_COMMIT");

/// `ship <version> (commit <commit>, <os>-<arch>)`, platform as compiled.
fn version_line() -> String {
    format!(
        "ship {VERSION} (commit {COMMIT}, {}-{})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Turn a folder into a running, observable application on a Linux machine
/// you own.
#[derive(Debug, Parser)]
#[command(name = "ship", disable_version_flag = true)]
struct Cli {
    /// Print version, commit and platform.
    #[arg(short = 'V', long = "version")]
    version: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the Ship daemon (Linux only).
    Daemon(DaemonArgs),
    /// Manage the daemons this client talks to.
    #[command(subcommand)]
    Target(commands::TargetCommand),
    /// Show the current target's host and Docker status.
    Host(commands::HostArgs),
    /// List the projects on the current target.
    Projects(commands::ProjectsArgs),
    /// Open the current target's UI in a browser; over ssh, forward it
    /// until Ctrl-C.
    Open(open::OpenArgs),
    /// Bridge stdin and stdout to the daemon's socket (the remote end of
    /// the ssh transport; not for interactive use).
    #[command(name = "dial-stdio")]
    DialStdio(commands::DialStdioArgs),
    /// Install or remove the systemd unit that runs the daemon (Linux,
    /// root).
    #[command(subcommand)]
    Service(ServiceCommand),
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Create the ship user and group, the state and socket directories,
    /// give the ship user Docker access, and write and enable the unit.
    /// Safe to run again.
    Install,
    /// Stop, disable and remove the unit. Leaves the user, the state
    /// directory, containers and volumes in place.
    Uninstall,
}

fn service(command: ServiceCommand) -> ExitCode {
    let name = match command {
        ServiceCommand::Install => "install",
        ServiceCommand::Uninstall => "uninstall",
    };
    if let Some(message) = service::unsupported(std::env::consts::OS) {
        eprintln!("ship: {message}");
        return ExitCode::from(2);
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    if let Some(message) = service::not_root(unsafe { libc::geteuid() }, name) {
        eprintln!("ship: {message}");
        return ExitCode::FAILURE;
    }
    let mut host = service::SystemHost;
    let layout = service::Layout::system();
    let result = match command {
        ServiceCommand::Install => std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .map_err(|e| service::ServiceError::Refused(format!("cannot locate this binary: {e}")))
            .and_then(|exe| service::install(&mut host, &layout, &exe))
            .map(|r| service::print_install(&r)),
        ServiceCommand::Uninstall => {
            service::uninstall(&mut host, &layout).map(service::print_uninstall)
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ship service {name}: {e}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, clap::Args)]
struct DaemonArgs {
    /// State directory holding the database; created at 0700 if missing.
    #[arg(long = "state", value_name = "DIR", default_value = ship_daemon::DEFAULT_STATE_DIR)]
    state: PathBuf,

    /// Path of the unix socket (mode 0660); its directory must exist.
    #[arg(long, value_name = "PATH", default_value = ship_daemon::DEFAULT_SOCKET_PATH)]
    socket: PathBuf,

    /// Port of the listener on 127.0.0.1. Ship never listens on any other
    /// interface.
    #[arg(long, value_name = "N", default_value_t = ship_daemon::DEFAULT_PORT)]
    port: u16,
}

/// The refusal `ship daemon` prints on `os`, or `None` on Linux.
fn daemon_unsupported(os: &str) -> Option<String> {
    (os != "linux").then(|| {
        format!(
            "ship daemon runs on Linux only (ADR-0002); this client runs on {os}. \
             Run the daemon on a Linux host and reach it with a target."
        )
    })
}

fn daemon(args: DaemonArgs) -> ExitCode {
    if let Some(message) = daemon_unsupported(std::env::consts::OS) {
        eprintln!("ship: {message}");
        return ExitCode::from(2);
    }
    let redactor = ship_daemon::Redactor::new();
    if let Err(e) = ship_daemon::log::init(redactor.clone()) {
        eprintln!("ship daemon: logger: {e}");
        return ExitCode::FAILURE;
    }
    let config = ship_daemon::Config {
        state_dir: args.state,
        socket_path: args.socket,
        port: args.port,
        ..ship_daemon::Config::new(redactor)
    };
    match ship_daemon::run_blocking(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing_error(&e);
            eprintln!("ship daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Records a fatal error in the structured log as well as plain stderr.
fn tracing_error(e: &ship_daemon::DaemonError) {
    ship_daemon::log::error(&e.to_string());
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    if cli.version {
        println!("{}", version_line());
        return ExitCode::SUCCESS;
    }
    match cli.command {
        Some(Command::Daemon(args)) => daemon(args),
        Some(Command::Target(command)) => commands::target(command),
        Some(Command::Host(args)) => commands::host(args),
        Some(Command::Projects(args)) => commands::projects(args),
        Some(Command::Open(args)) => open::open(args),
        Some(Command::DialStdio(args)) => commands::dial_stdio(args),
        Some(Command::Service(command)) => service(command),
        None => {
            eprintln!("ship: no command given; see `ship --help`");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn version_line_names_version_commit_and_platform() {
        let line = version_line();
        assert!(line.starts_with(&format!("ship {VERSION} ")), "{line}");
        assert!(line.contains(&format!("commit {COMMIT}")), "{line}");
        let platform = format!("{}-{})", std::env::consts::OS, std::env::consts::ARCH);
        assert!(line.contains(&platform), "{line}");
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn daemon_defaults() {
        let cli = Cli::try_parse_from(["ship", "daemon"]).unwrap();
        let Some(Command::Daemon(args)) = cli.command else {
            panic!("expected daemon");
        };
        assert_eq!(args.state, PathBuf::from("/var/lib/ship"));
        assert_eq!(args.socket, PathBuf::from("/run/ship/ship.sock"));
        assert_eq!(args.port, 7180);
    }

    #[test]
    fn daemon_takes_no_address() {
        assert!(Cli::try_parse_from(["ship", "daemon", "--listen", "0.0.0.0"]).is_err());
        assert!(Cli::try_parse_from(["ship", "daemon", "--port", "0.0.0.0:80"]).is_err());
    }

    #[test]
    fn daemon_is_linux_only() {
        assert_eq!(daemon_unsupported("linux"), None);
        for os in ["macos", "windows", "freebsd"] {
            let message = daemon_unsupported(os).unwrap();
            assert!(message.contains("Linux only"), "{message}");
            assert!(message.contains(os), "{message}");
        }
    }
}
