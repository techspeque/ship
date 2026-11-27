//! SIGTERM stops the daemon cleanly: `run` returns, the database is closed
//! and the socket is removed.
//!
//! This file is its own test binary, so the signal sent to this process
//! reaches only the daemon under test.

mod common;

use std::time::Duration;

use common::{Via, get};

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_closes_database_and_removes_socket() {
    let dir = tempfile::tempdir().unwrap();
    let config = common::config(dir.path());
    let socket = config.socket_path.clone();
    let wal = config.state_dir.join("ship.db-wal");

    // On its own thread and runtime, as `ship daemon` runs it.
    let daemon = {
        let config = config.clone();
        let thread = std::thread::spawn(move || ship_daemon::run_blocking(config));
        tokio::task::spawn_blocking(move || thread.join().unwrap())
    };

    // Wait until the daemon answers on its socket.
    let mut ready = false;
    for _ in 0..200 {
        if socket.exists() && std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(ready, "daemon did not start");
    assert_eq!(
        get(&Via::Unix(socket.clone()), "/healthz").await.status,
        200
    );
    assert!(wal.exists(), "WAL mode keeps a -wal file while open");

    let status = std::process::Command::new("kill")
        .args(["-TERM", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    let result = tokio::time::timeout(Duration::from_secs(15), daemon)
        .await
        .expect("run returned after SIGTERM")
        .unwrap();
    result.unwrap();

    assert!(!socket.exists(), "socket removed");
    // The last connection closing checkpoints and deletes the WAL file.
    assert!(!wal.exists(), "database closed");
    let store = ship_store::Store::open(&config.state_dir).await.unwrap();
    assert!(store.open_report().backup.is_none());
    store.close().await;
}
