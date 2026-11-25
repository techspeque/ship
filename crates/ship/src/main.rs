//! The `ship` binary: client commands and `ship daemon` dispatch.
//!
//! Phase 0 scaffold: only `--version` / `-V` is implemented. Argument parsing
//! with Clap arrives with the first real subcommand.

use std::process::ExitCode;

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

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--version" || args[0] == "-V") {
        println!("{}", version_line());
        return ExitCode::SUCCESS;
    }
    eprintln!("ship: pre-alpha scaffold; only `ship --version` is implemented");
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_version_commit_and_platform() {
        let line = version_line();
        assert!(line.starts_with(&format!("ship {VERSION} ")), "{line}");
        assert!(line.contains(&format!("commit {COMMIT}")), "{line}");
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        assert!(line.contains(&platform), "{line}");
    }
}
