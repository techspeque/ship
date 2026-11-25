//! Runs the built `ship` binary: `ship service` refuses before touching the
//! host when it is not on Linux or not root. The real install runs in the
//! CI `service` job (`scripts/service-smoke.sh`).

use std::process::Command;

fn ship() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ship"))
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn service_refuses_off_linux_or_without_root() {
    if is_root() && cfg!(target_os = "linux") {
        eprintln!("skipped: running as root on Linux would really install");
        return;
    }
    for sub in ["install", "uninstall"] {
        let output = ship().args(["service", sub]).output().expect("run ship");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.stdout.is_empty(), "{sub}: {stderr}");
        if cfg!(target_os = "linux") {
            assert_eq!(output.status.code(), Some(1), "{sub}: {stderr}");
            assert!(
                stderr.contains(&format!("sudo ship service {sub}")),
                "{sub}: {stderr}"
            );
        } else {
            assert_eq!(output.status.code(), Some(2), "{sub}: {stderr}");
            assert!(stderr.contains("Linux only"), "{sub}: {stderr}");
        }
    }
}

#[test]
fn service_help_lists_install_and_uninstall() {
    let output = ship()
        .args(["service", "--help"])
        .output()
        .expect("run ship");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("install"), "{stdout}");
    assert!(stdout.contains("uninstall"), "{stdout}");
}
