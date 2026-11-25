//! `ship open`: the daemon's UI in a browser (OVERVIEW §6).
//!
//! The UI is served on the daemon's 127.0.0.1 listener, so a URL is only
//! ever `http://127.0.0.1:N/`, and it is printed only after `/healthz`
//! answers through it (OVERVIEW §3.3: never an address that does not
//! exist).
//!
//! - local target: the daemon's own listener on port N (default 7180).
//! - ssh target: `ssh -N -L 127.0.0.1:P:<remote socket>` forwards a free
//!   local port to the daemon's unix socket on the host, so no port on the
//!   host is assumed. `ship open` stays in the foreground until Ctrl-C
//!   (SIGINT or SIGTERM), when it kills and reaps `ssh`, closing the
//!   forward, or until `ssh` exits.
//!
//! `PROJECT` arrives with projects in Phase 1.

use std::net::{Ipv4Addr, SocketAddr};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use ship_net::{SshTarget, Target};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::commands::{self, FAILURE, fail};

/// How long one `/healthz` attempt may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Pause between `/healthz` attempts while the ssh forward comes up.
const RETRY: Duration = Duration::from_millis(100);
/// Largest `/healthz` answer read.
const MAX_PROBE: u64 = 64 * 1024;
/// How much of `ssh`'s stderr is kept for messages.
const MAX_STDERR: usize = 8 * 1024;

#[derive(Debug, clap::Args)]
pub struct OpenArgs {
    /// Port on 127.0.0.1: the local daemon's listener [default: 7180], or
    /// the local end of the ssh forward [default: a free port].
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..))]
    port: Option<u16>,
    /// Print the URL without starting a browser.
    #[arg(long)]
    no_browser: bool,
    /// Give up after this many seconds waiting for the daemon, including
    /// the ssh connection.
    #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
}

pub fn open(args: OpenArgs) -> ExitCode {
    let endpoint = match commands::endpoint() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let rt = match commands::runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let code = rt.block_on(async {
        match &endpoint.target {
            Target::Local { .. } => local(&args).await,
            Target::Ssh(ssh) => over_ssh(ssh, &args).await,
        }
    });
    drop(rt);
    code
}

fn url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

async fn local(args: &OpenArgs) -> ExitCode {
    let port = args.port.unwrap_or(ship_daemon::DEFAULT_PORT);
    let timeout = Duration::from_secs(args.timeout);
    let why = match tokio::time::timeout(timeout, healthz(port)).await {
        Ok(Ok(())) => {
            show(&url(port), args.no_browser);
            return ExitCode::SUCCESS;
        }
        Ok(Err(why)) => why,
        Err(_) => format!("no answer within {}s", args.timeout),
    };
    fail(
        format!(
            "the daemon's 127.0.0.1 listener is not answering on port {port} ({why}); \
             start the daemon, or name its port with --port"
        ),
        FAILURE,
    )
}

async fn over_ssh(ssh: &SshTarget, args: &OpenArgs) -> ExitCode {
    // Installed before ssh starts, so an early Ctrl-C still reaps it.
    let int = signal(SignalKind::interrupt());
    let term = signal(SignalKind::terminate());
    let (mut int, mut term) = match (int, term) {
        (Ok(i), Ok(t)) => (i, t),
        (Err(e), _) | (_, Err(e)) => return fail(format!("signal handler: {e}"), FAILURE),
    };
    let port = match args.port {
        Some(p) => p,
        None => match free_port() {
            Ok(p) => p,
            Err(e) => return fail(format!("cannot pick a free local port: {e}"), FAILURE),
        },
    };
    let timeout = Duration::from_secs(args.timeout);
    let host = ssh.display_host();
    let spawned = Command::new("ssh")
        .args(ssh.forward_args(port, timeout))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let why = format!("cannot run `ssh`: {e}; install an OpenSSH client");
            return fail(why, FAILURE);
        }
    };
    let mut stderr = tokio::spawn(read_bounded(child.stderr.take()));

    // Wait for the daemon to answer through the forward.
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let why = ssh_stderr(&mut stderr).await;
            return fail(
                format!("ssh to {host} exited ({status}) before the forward was up{why}"),
                FAILURE,
            );
        }
        if let Ok(Ok(())) = tokio::time::timeout(PROBE_TIMEOUT, healthz(port)).await {
            break;
        }
        if Instant::now() >= deadline {
            stop(&mut child, &stderr).await;
            return fail(
                format!(
                    "the daemon on {host} did not answer through the ssh forward within {}s",
                    args.timeout
                ),
                FAILURE,
            );
        }
        tokio::select! {
            _ = tokio::time::sleep(RETRY) => {}
            _ = int.recv() => { stop(&mut child, &stderr).await; return ExitCode::SUCCESS; }
            _ = term.recv() => { stop(&mut child, &stderr).await; return ExitCode::SUCCESS; }
        }
    }

    show(&url(port), args.no_browser);
    eprintln!("ship: forwarding to the daemon on {host} over ssh; press Ctrl-C to stop.");
    tokio::select! {
        status = child.wait() => {
            let why = ssh_stderr(&mut stderr).await;
            let status = status.map_or_else(|e| e.to_string(), |s| s.to_string());
            fail(format!("ssh to {host} exited ({status}); the forward is closed{why}"), FAILURE)
        }
        _ = int.recv() => { stop(&mut child, &stderr).await; ExitCode::SUCCESS }
        _ = term.recv() => { stop(&mut child, &stderr).await; ExitCode::SUCCESS }
    }
}

/// Kills `ssh` and waits for it, so the forward is closed and no process
/// is left behind.
async fn stop(child: &mut Child, stderr: &JoinHandle<String>) {
    let _ = child.kill().await;
    stderr.abort();
}

/// `: <ssh's stderr>`, or nothing, for a message.
async fn ssh_stderr(stderr: &mut JoinHandle<String>) -> String {
    match tokio::time::timeout(Duration::from_secs(1), stderr).await {
        Ok(Ok(text)) if !text.is_empty() => format!(": {text}"),
        _ => String::new(),
    }
}

/// A port on 127.0.0.1 that was free a moment ago.
fn free_port() -> std::io::Result<u16> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let listener = std::net::TcpListener::bind(addr)?;
    Ok(listener.local_addr()?.port())
}

/// `GET /healthz` on `127.0.0.1:port`; `Ok` only for the daemon's `200
/// {"status":"ok"}`.
async fn healthz(port: u16) -> Result<(), String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| e.to_string())?;
    let request =
        format!("GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream
        .take(MAX_PROBE)
        .read_to_end(&mut raw)
        .await
        .map_err(|e| e.to_string())?;
    check_healthz(&raw)
}

/// Whether `raw` is the daemon's healthy `/healthz` answer.
fn check_healthz(raw: &[u8]) -> Result<(), String> {
    let text = String::from_utf8_lossy(raw);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return Err("not an HTTP answer".to_owned());
    };
    let status = head.split(' ').nth(1).unwrap_or("");
    if !head.starts_with("HTTP/1.") || status != "200" {
        return Err(format!("answered with status {status:?}"));
    }
    if body.trim() != r#"{"status":"ok"}"# {
        return Err("answered, but not as a Ship daemon".to_owned());
    }
    Ok(())
}

/// Prints `url`, then starts the platform's browser on it unless
/// `no_browser`. The URL is one argument; no shell is involved.
fn show(url: &str, no_browser: bool) {
    println!("{url}");
    if no_browser {
        return;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let started = std::process::Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match started {
        // Reaped in the background; the opener returns once it has
        // handed the URL over.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("ship: cannot start `{opener}` ({e}); open {url} yourself."),
    }
}

/// Reads up to [`MAX_STDERR`] bytes, then drains the rest, so `ssh` never
/// blocks on a full pipe.
async fn read_bounded(r: Option<impl AsyncRead + Unpin>) -> String {
    let Some(mut r) = r else {
        return String::new();
    };
    let mut kept = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_STDERR.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthz_answers() {
        let ok = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"status\":\"ok\"}";
        assert_eq!(check_healthz(ok), Ok(()));
        for bad in [
            &b""[..],
            b"HTTP/1.1 200 OK\r\n",
            b"HTTP/1.1 503 Service Unavailable\r\n\r\n{\"status\":\"ok\"}",
            b"HTTP/1.1 200 OK\r\n\r\n<html>",
            b"SSH-2.0-OpenSSH_9.6\r\n\r\n",
        ] {
            assert!(check_healthz(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn urls_are_loopback() {
        assert_eq!(url(7180), "http://127.0.0.1:7180/");
    }

    #[tokio::test]
    async fn nothing_listening_is_an_error() {
        let port = free_port().unwrap();
        assert!(healthz(port).await.is_err());
    }
}
