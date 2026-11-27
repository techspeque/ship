//! The host's tailnet address, asked of the host's own Tailscale (or
//! Headscale-joined) client. Ship embeds no tailnet client and manages no
//! tailnet key (ADR-0003 rule 4); without a usable tailnet the answer is
//! simply "none".
//!
//! [`TailnetAddress`] is the seam: the daemon resolves `publish.address
//! auto` through it, so tests fake a host with and without a tailnet
//! ([`StaticTailnet`]). [`TailscaleCli`] is the real one.

use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

/// How long [`TailscaleCli`] waits for an answer by default.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(2);

/// Largest answer read from `tailscale ip -4`.
const MAX_ANSWER: usize = 4096;

/// A boxed future of a tailnet address.
pub type AddressFuture<'a> = Pin<Box<dyn Future<Output = Option<Ipv4Addr>> + Send + 'a>>;

/// Where to ask for the host's tailnet IPv4 address.
pub trait TailnetAddress: fmt::Debug + Send + Sync {
    /// The host's tailnet IPv4 address now, or `None` when the host has no
    /// usable tailnet. Never fails and always returns within a bounded
    /// time.
    fn ipv4(&self) -> AddressFuture<'_>;
}

/// A fixed answer: `None` for a host with no tailnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StaticTailnet(pub Option<Ipv4Addr>);

impl TailnetAddress for StaticTailnet {
    fn ipv4(&self) -> AddressFuture<'_> {
        let answer = self.0;
        Box::pin(async move { answer })
    }
}

/// Asks the local `tailscale` CLI: `tailscale ip -4`.
///
/// A CLI that is missing, exits non-zero (logged out, `tailscaled`
/// stopped), prints something that is not one IPv4 address, or does not
/// answer within the wait counts as no tailnet; a late answer's process is
/// killed.
#[derive(Debug, Clone)]
pub struct TailscaleCli {
    program: OsString,
    wait: Duration,
}

impl Default for TailscaleCli {
    fn default() -> Self {
        TailscaleCli::new("tailscale", DEFAULT_WAIT)
    }
}

impl TailscaleCli {
    /// Runs `program` (found on `PATH` when it has no `/`), waiting at most
    /// `wait`.
    pub fn new(program: impl Into<OsString>, wait: Duration) -> Self {
        TailscaleCli {
            program: program.into(),
            wait,
        }
    }

    async fn ask(&self) -> Option<Ipv4Addr> {
        let child = tokio::process::Command::new(&self.program)
            .args(["ip", "-4"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(self.wait, child).await.ok()?.ok()?;
        if !output.status.success() || output.stdout.len() > MAX_ANSWER {
            return None;
        }
        parse_answer(&output.stdout)
    }
}

impl TailnetAddress for TailscaleCli {
    fn ipv4(&self) -> AddressFuture<'_> {
        Box::pin(self.ask())
    }
}

/// The address in `tailscale ip -4` output: its first non-empty line, which
/// must be an IPv4 address and nothing else.
pub fn parse_answer(stdout: &[u8]) -> Option<Ipv4Addr> {
    let text = std::str::from_utf8(stdout).ok()?;
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let ip: Ipv4Addr = line.parse().ok()?;
    (!ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast() && !ip.is_broadcast())
        .then_some(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_parsed() {
        let ip = Ipv4Addr::new(100, 101, 102, 103);
        assert_eq!(parse_answer(b"100.101.102.103\n"), Some(ip));
        assert_eq!(parse_answer(b"\n  100.101.102.103  \n"), Some(ip));
        for bad in [
            &b""[..],
            b"\n",
            b"fd7a:115c:a1e0::1\n",
            b"100.101.102\n",
            b"0.0.0.0\n",
            b"127.0.0.1\n",
            b"224.0.0.1\n",
            b"Logged out.\n",
            b"\xff\xfe",
        ] {
            assert_eq!(parse_answer(bad), None, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_static_tailnet_answers_what_it_holds() {
        let ip = Ipv4Addr::new(100, 64, 0, 1);
        assert_eq!(StaticTailnet(Some(ip)).ipv4().await, Some(ip));
        assert_eq!(StaticTailnet(None).ipv4().await, None);
    }
}
