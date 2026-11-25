//! `ship open`: the daemon's UI in a browser (OVERVIEW §6).
//!
//! The UI is served on the daemon's 127.0.0.1 listener, so a URL is only
//! ever `http://127.0.0.1:N/`, and it is printed only after `/healthz`
//! answers through it (OVERVIEW §3.3: never an address that does not
//! exist).
//!
//! - local target: the daemon's own listener on port N (default 7180).
//! - ssh target: `ship open` listens on a free 127.0.0.1 port itself and
//!   carries each connection over its own `ssh … HOST ship dial-stdio`
//!   child to the daemon's unix socket on the host ([`forward::serve`]).
//!   No ssh port forwarding is used, so servers that refuse it (Tailscale
//!   SSH) work, and no port on the host is assumed. The URL is printed once
//!   `/healthz` has answered over the same transport; a refusal prints
//!   ssh's own reason. `ship open` stays in the foreground until Ctrl-C
//!   (SIGINT or SIGTERM), when it stops accepting and kills and reaps every
//!   `ssh` child.
//!
//! `PROJECT` arrives with projects in Phase 1.

use std::net::{Ipv4Addr, SocketAddr};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use ship_net::forward::{self, Dialer, Forward};
use ship_net::{SshTarget, Target, client};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

use crate::commands::{self, FAILURE, fail};

/// Largest `/healthz` answer read.
const MAX_PROBE: u64 = 64 * 1024;
/// Most `ssh` children carrying browser connections at once; a browser
/// opens about six per origin.
const MAX_CONNECTIONS: usize = 16;

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
    // Installed before any ssh starts, so an early Ctrl-C still reaps it.
    let int = signal(SignalKind::interrupt());
    let term = signal(SignalKind::terminate());
    let (mut int, mut term) = match (int, term) {
        (Ok(i), Ok(t)) => (i, t),
        (Err(e), _) | (_, Err(e)) => return fail(format!("signal handler: {e}"), FAILURE),
    };
    // Bound here, before ssh runs, so an occupied port fails now and
    // whatever answers on it afterwards is this process.
    let listener = match listen(args.port) {
        Ok(l) => l,
        Err(e) => return fail(e, FAILURE),
    };
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => return fail(format!("cannot read the local port: {e}"), FAILURE),
    };
    let timeout = Duration::from_secs(args.timeout);
    let host = ssh.display_host();

    // The first connection, on its own: it fails with ssh's reason (or
    // dial-stdio's) rather than a timeout, and no URL is printed before
    // the daemon has answered over this transport.
    let target = Target::Ssh(ssh.clone());
    tokio::select! {
        probe = client::get(&target, "/healthz", timeout) => {
            if let Err(why) = probe.map_err(|e| e.to_string()).and_then(check_response) {
                return fail(
                    format!("the daemon on {host} did not answer through ssh: {why}"),
                    FAILURE,
                );
            }
        }
        _ = int.recv() => return ExitCode::SUCCESS,
        _ = term.recv() => return ExitCode::SUCCESS,
    }

    show(&url(port), args.no_browser);
    eprintln!("ship: forwarding to the daemon on {host} over ssh; press Ctrl-C to stop.");
    let forward = Forward {
        dialer: Dialer::ssh(ssh, timeout),
        limit: MAX_CONNECTIONS,
        drain: ship_net::DRAIN_AFTER_INPUT_EOF,
    };
    let shutdown = async move {
        tokio::select! {
            _ = int.recv() => {}
            _ = term.recv() => {}
        }
    };
    forward::serve(listener, forward, shutdown, |why| {
        eprintln!("ship: a connection to the daemon failed: {why}");
    })
    .await;
    ExitCode::SUCCESS
}

/// The 127.0.0.1 listener `ship open` serves over ssh: `port`, or a free
/// one.
fn listen(port: Option<u16>) -> Result<TcpListener, String> {
    let bound = ship_net::bind_loopback(port.unwrap_or(0)).and_then(|l| {
        l.set_nonblocking(true)?;
        TcpListener::from_std(l)
    });
    bound.map_err(|e| match port {
        Some(port) => format!(
            "port {port} on 127.0.0.1 is not free ({e}); \
             pick another with --port, or leave it out"
        ),
        None => format!("cannot listen on a free 127.0.0.1 port: {e}"),
    })
}

/// `Ok` only for the daemon's `200 {"status":"ok"}`.
fn check_response(r: client::Response) -> Result<(), String> {
    if r.status != 200 {
        return Err(format!("answered with status {}", r.status));
    }
    if r.body.trim_ascii() != br#"{"status":"ok"}"# {
        return Err("answered, but not as a Ship daemon".to_owned());
    }
    Ok(())
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

    #[test]
    fn probe_responses() {
        let response = |status, body: &'static [u8]| client::Response {
            status,
            api_version: None,
            daemon_version: None,
            body: body.into(),
        };
        assert_eq!(
            check_response(response(200, b"{\"status\":\"ok\"}\n")),
            Ok(())
        );
        assert!(check_response(response(503, b"{\"status\":\"ok\"}")).is_err());
        assert!(check_response(response(200, b"<html>")).is_err());
    }

    #[tokio::test]
    async fn an_occupied_port_is_refused_by_name() {
        let held = ship_net::bind_loopback(0).unwrap();
        let port = held.local_addr().unwrap().port();
        let e = listen(Some(port)).unwrap_err();
        assert!(e.contains(&format!("port {port}")), "{e}");
    }

    #[tokio::test]
    async fn nothing_listening_is_an_error() {
        let port = ship_net::bind_loopback(0)
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(healthz(port).await.is_err());
    }
}
