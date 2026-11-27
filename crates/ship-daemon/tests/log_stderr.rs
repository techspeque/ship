//! A registered secret logged through the daemon's real logger never
//! reaches the process's stderr.
//!
//! The parent test re-runs this test binary as a child that installs the
//! global stderr logger and logs the secret; the parent reads the child's
//! actual stderr.

use std::process::Command;

use ship_core::SecretValue;

const CHILD_ENV: &str = "SHIP_LOG_STDERR_CHILD";
/// Contains `"` and `\`, which the JSON formatter and `Debug` escape.
const SECRET: &str = "TOPSECRET-\"q\\b-7731";
const MARKER: &str = "log-stderr-marker";

#[test]
fn child_logs_secret() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let redactor = ship_daemon::Redactor::new();
    redactor.register(&SecretValue::new(SECRET));
    ship_daemon::log::init(redactor).unwrap();
    tracing::info!(token = SECRET, "{MARKER} display {}", SECRET);
    tracing::warn!(token = ?SECRET, "{MARKER} debug {:?}", SECRET);
    tracing::error!(secret = %SecretValue::new(SECRET), "{MARKER} wrapped");
}

#[test]
fn secret_never_reaches_stderr() {
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args([
            "--exact",
            "child_logs_secret",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8(out.stderr).unwrap();

    let records: Vec<serde_json::Value> = stderr
        .lines()
        .filter(|l| l.contains(MARKER))
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    assert_eq!(records.len(), 3, "{stderr}");
    for r in &records {
        assert!(r["level"].is_string(), "{r}");
        assert!(r["timestamp"].is_string(), "{r}");
    }
    assert!(stderr.contains("[redacted]"), "{stderr}");
    assert!(!stderr.contains("TOPSECRET"), "{stderr}");
    assert!(!stderr.contains("7731"), "{stderr}");
}
