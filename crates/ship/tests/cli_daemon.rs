//! `ship daemon` from the built binary: Linux only (ADR-0002 rule 2).

use std::process::Command;

fn ship() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ship"))
}

#[test]
fn daemon_help_lists_options() {
    let out = ship().args(["daemon", "--help"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let help = String::from_utf8(out.stdout).unwrap();
    for option in ["--state", "--socket", "--port"] {
        assert!(help.contains(option), "{help}");
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn daemon_refuses_non_linux() {
    let dir = tempfile_dir();
    let out = ship()
        .args(["daemon", "--state"])
        .arg(dir.join("state"))
        .arg("--socket")
        .arg(dir.join("ship.sock"))
        .args(["--port", "0"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("runs on Linux only"), "{stderr}");
    assert!(stderr.contains(std::env::consts::OS), "{stderr}");
    // Nothing was created.
    assert!(!dir.join("state").exists());
    assert!(!dir.join("ship.sock").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(not(target_os = "linux"))]
fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ship-cli-daemon-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_rejects_an_address_for_port() {
    let out = ship()
        .args(["daemon", "--port", "0.0.0.0:7180"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}
