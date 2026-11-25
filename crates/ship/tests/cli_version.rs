//! Runs the built `ship` binary and checks `--version` output and exit codes.

use std::process::Command;

fn ship() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ship"))
}

#[test]
fn version_flag_prints_version_commit_and_platform() {
    for flag in ["--version", "-V"] {
        let output = ship().arg(flag).output().expect("run ship");
        assert!(output.status.success(), "{flag}: {:?}", output.status);
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        let line = stdout.trim_end();
        assert!(
            line.starts_with(&format!("ship {} (commit ", env!("CARGO_PKG_VERSION"))),
            "{line}"
        );
        let platform = format!("{}-{})", std::env::consts::OS, std::env::consts::ARCH);
        assert!(line.ends_with(&platform), "{line}");
        let commit = line
            .split("(commit ")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .expect("commit field");
        assert!(!commit.is_empty(), "{line}");
    }
}

#[test]
fn unknown_arguments_exit_non_zero() {
    let output = ship().arg("no-such-command").output().expect("run ship");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}
