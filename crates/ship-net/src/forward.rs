//! `ship open` over ssh: a 127.0.0.1 port whose every connection is carried
//! by its own `ssh … HOST ship dial-stdio` child (ADR-0003).
//!
//! No ssh port forwarding is used. Servers that refuse streamlocal
//! forwarding (Tailscale SSH refuses `-L port:/socket`) still run a remote
//! command, and `ship dial-stdio` is the transport every other client
//! command already uses. The bytes reach the daemon's unix socket unchanged,
//! so the daemon's Host and origin guard sees what the browser sent.
//!
//! Each accepted connection spawns one child with structured argv and pumps
//! TCP → child stdin and child stdout → TCP. At most `limit` children run at
//! once; further connections wait in the listener's backlog. On shutdown the
//! listener closes first, then every child is killed and reaped before
//! [`serve`] returns, so no process is left behind.

use std::ffi::OsString;
use std::future::Future;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

use crate::client::read_bounded;
use crate::target::SshTarget;

/// How long a child may take to exit on its own once the daemon's side of
/// its connection has closed, before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(1);
/// Pause after a failed `accept` (for example, out of file descriptors), so
/// the loop does not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// The command that carries one connection: its stdin and stdout are the
/// byte stream to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialer {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl Dialer {
    /// `ssh <SshTarget::ssh_args>`: `ssh … -- HOST ship dial-stdio
    /// [--socket S]`, the same argv every other client command runs.
    pub fn ssh(target: &SshTarget, connect_timeout: Duration) -> Dialer {
        Dialer {
            program: "ssh".into(),
            args: target.ssh_args(connect_timeout),
        }
    }
}

/// How [`serve`] carries connections.
#[derive(Debug, Clone)]
pub struct Forward {
    pub dialer: Dialer,
    /// Most children running at once; at least 1.
    pub limit: usize,
    /// After the browser closes its side, how long the daemon's side may
    /// keep sending before the child is killed.
    pub drain: Duration,
}

/// Serves `listener` until `shutdown` completes, carrying each connection
/// over its own child. `on_error` receives one line per connection whose
/// child could not start or failed with a message (ssh's or dial-stdio's
/// stderr). Returns after the listener is closed and every child is reaped.
pub async fn serve<S, E>(listener: TcpListener, forward: Forward, shutdown: S, on_error: E)
where
    S: Future<Output = ()>,
    E: Fn(String) + Send + Sync + 'static,
{
    let forward = Arc::new(forward);
    let on_error = Arc::new(on_error);
    let slots = Arc::new(Semaphore::new(forward.limit.max(1)));
    let (stop_tx, stop_rx) = watch::channel(());
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        while connections.try_join_next().is_some() {}
        // Take a slot before accepting, so a connection over the limit
        // waits in the backlog rather than holding a socket here.
        let slot = tokio::select! {
            _ = &mut shutdown => break,
            slot = slots.clone().acquire_owned() => match slot {
                Ok(slot) => slot,
                Err(_) => break,
            },
        };
        let stream = tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => {
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            },
        };
        let forward = forward.clone();
        let on_error = on_error.clone();
        let stop = stop_rx.clone();
        connections.spawn(async move {
            let _slot = slot;
            if let Err(message) = carry(stream, &forward, stop).await {
                on_error(message);
            }
        });
    }

    // Stop accepting, then end every child.
    drop(listener);
    let _ = stop_tx.send(());
    while connections.join_next().await.is_some() {}
}

/// Carries one connection over one child until either side ends or `stop`
/// fires; the child is always reaped before this returns. `Err` carries a
/// message when the child could not start or exited non-zero with one.
async fn carry(
    stream: TcpStream,
    forward: &Forward,
    mut stop: watch::Receiver<()>,
) -> Result<(), String> {
    let program = forward.dialer.program.to_string_lossy().into_owned();
    let mut child = Command::new(&forward.dialer.program)
        .args(&forward.dialer.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run `{program}`: {e}"))?;
    let (Some(mut stdin), Some(mut stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill().await;
        return Err(format!("`{program}` started without its pipes"));
    };
    let stderr = tokio::spawn(read_bounded(stderr));
    let (mut from_browser, mut to_browser) = stream.into_split();

    // Browser → child; closing stdin tells dial-stdio the browser is done.
    let up = async move {
        let _ = tokio::io::copy(&mut from_browser, &mut stdin).await;
        drop(stdin);
    };
    // Child → browser; the browser sees the daemon's close.
    let down = async move {
        let _ = tokio::io::copy(&mut stdout, &mut to_browser).await;
        let _ = to_browser.shutdown().await;
    };
    tokio::pin!(up, down);
    let drain = forward.drain;
    let ended = async {
        tokio::select! {
            _ = &mut down => return true,
            _ = &mut up => {}
        }
        tokio::time::timeout(drain, &mut down).await.is_ok()
    };
    let daemon_closed = tokio::select! {
        closed = ended => closed,
        _ = stop.changed() => false,
    };

    let status = if daemon_closed {
        match tokio::time::timeout(EXIT_GRACE, child.wait()).await {
            Ok(status) => status.ok(),
            Err(_) => reap(&mut child).await,
        }
    } else {
        reap(&mut child).await
    };
    let stderr = match tokio::time::timeout(EXIT_GRACE, stderr).await {
        Ok(Ok(text)) => text,
        _ => String::new(),
    };
    match status.and_then(|s| s.code()) {
        Some(code) if code != 0 && !stderr.is_empty() => Err(stderr),
        _ => Ok(()),
    }
}

/// Kills `child` and waits for it, so no process is left behind.
async fn reap(child: &mut Child) -> Option<ExitStatus> {
    let _ = child.start_kill();
    child.wait().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The serving tests spawn processes, so they live in tests/forward.rs,
    // a test binary of their own.

    #[test]
    fn ssh_dialer_runs_dial_stdio() {
        let target = SshTarget {
            user: Some("deploy".into()),
            host: "lab".into(),
            port: None,
            socket: None,
        };
        let d = Dialer::ssh(&target, Duration::from_secs(10));
        assert_eq!(d.program, "ssh");
        assert_eq!(d.args, target.ssh_args(Duration::from_secs(10)));
        let tail: Vec<_> = d.args[d.args.len() - 4..].to_vec();
        assert_eq!(tail, ["--", "lab", "ship", "dial-stdio"]);
    }
}
