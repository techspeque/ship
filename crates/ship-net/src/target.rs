//! Where a client finds a daemon: a `local` unix socket or `ssh://` to a
//! host that runs `ship dial-stdio` (ADR-0003). `tailnet://` arrives in
//! Phase 2 (ADR-0003 rule 5).
//!
//! Every value that reaches `ssh` is validated here, before it is stored or
//! spawned: `ssh` passes its remote command through the remote login shell,
//! and a host or user that starts with `-` would be read as an option.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The daemon's socket when a target names none.
pub const DEFAULT_SOCKET: &str = "/run/ship/ship.sock";

/// The forms `ship target add` accepts, for error messages.
pub const TARGET_FORMS: &str = "ssh://USER@HOST[:PORT] or local";

/// A daemon a client can reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The daemon's unix socket on this machine.
    Local { socket: PathBuf },
    /// `ssh HOST ship dial-stdio`, bridging to the socket on the host.
    Ssh(SshTarget),
}

/// An `ssh://` target. Fields are validated; see [`Target::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    /// A host name, ssh config alias or IP literal (IPv6 without brackets).
    pub host: String,
    pub port: Option<u16>,
    /// The daemon's socket on the host; `None` lets `ship dial-stdio` use
    /// its default.
    pub socket: Option<PathBuf>,
}

/// Why a target was refused. No variant echoes a rejected value, so a
/// message never reflects attacker-chosen text back into a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetError {
    /// `tailnet://`: the transport arrives in Phase 2.
    TailnetNotYet,
    /// Not `local` or `ssh://…`.
    UnknownForm,
    /// `ssh://` with no host.
    MissingHost,
    InvalidHost,
    InvalidUser,
    InvalidPort,
    /// A path, query or fragment after the host.
    UnexpectedPath,
    /// The socket path is not absolute or contains characters outside
    /// letters, digits, `/`, `.`, `_` and `-`.
    InvalidSocket,
}

impl fmt::Display for TargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TargetError::TailnetNotYet => {
                "the tailnet transport arrives in Phase 2 (ADR-0003 rule 5); \
                 use ssh://USER@HOST for now"
            }
            TargetError::UnknownForm => "a target is ssh://USER@HOST[:PORT] or local",
            TargetError::MissingHost => "an ssh:// target needs a host",
            TargetError::InvalidHost => {
                "the host may contain only letters, digits, '.', '-' and '_' \
                 (or be a bracketed IPv6 address) and must not start with '-'"
            }
            TargetError::InvalidUser => {
                "the user may contain only letters, digits, '.', '-' and '_' \
                 and must not start with '-'"
            }
            TargetError::InvalidPort => "the port must be a number from 1 to 65535",
            TargetError::UnexpectedPath => {
                "an ssh:// target takes no path, query or fragment; \
                 name the remote socket with --socket"
            }
            TargetError::InvalidSocket => {
                "the socket must be an absolute path of letters, digits, \
                 '/', '.', '_' and '-'"
            }
        })
    }
}

impl std::error::Error for TargetError {}

impl Target {
    /// Parses `local` or `ssh://[USER@]HOST[:PORT]`, with an optional
    /// daemon socket path (local path, or the path on the ssh host).
    pub fn parse(url: &str, socket: Option<&str>) -> Result<Target, TargetError> {
        let socket = socket.map(validate_socket).transpose()?;
        if url == "local" {
            return Ok(Target::Local {
                socket: socket.unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET)),
            });
        }
        if let Some(rest) = strip_scheme(url, "ssh") {
            return parse_ssh(rest, socket).map(Target::Ssh);
        }
        if strip_scheme(url, "tailnet").is_some() {
            return Err(TargetError::TailnetNotYet);
        }
        Err(TargetError::UnknownForm)
    }

    /// The URL form, as `ship target add` accepted it (without the socket).
    pub fn url(&self) -> String {
        match self {
            Target::Local { .. } => "local".to_owned(),
            Target::Ssh(ssh) => {
                let mut url = String::from("ssh://");
                if let Some(user) = &ssh.user {
                    url.push_str(user);
                    url.push('@');
                }
                if ssh.host.contains(':') {
                    url.push('[');
                    url.push_str(&ssh.host);
                    url.push(']');
                } else {
                    url.push_str(&ssh.host);
                }
                if let Some(port) = ssh.port {
                    url.push_str(&format!(":{port}"));
                }
                url
            }
        }
    }

    /// The socket path the target names explicitly, if any.
    pub fn socket(&self) -> Option<&Path> {
        match self {
            Target::Local { socket } => Some(socket),
            Target::Ssh(ssh) => ssh.socket.as_deref(),
        }
    }
}

impl SshTarget {
    /// The arguments to `ssh` (not including `ssh` itself) that run
    /// `ship dial-stdio` on the host. `--` ends ssh's options, so the host
    /// is never read as one; the remote command is fixed words plus a
    /// socket path restricted to shell-inert characters.
    pub fn ssh_args(&self, connect_timeout: Duration) -> Vec<OsString> {
        let secs = connect_timeout.as_secs().max(1);
        let mut args: Vec<OsString> = vec![
            "-T".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            format!("ConnectTimeout={secs}").into(),
        ];
        if let Some(port) = self.port {
            args.push("-p".into());
            args.push(port.to_string().into());
        }
        if let Some(user) = &self.user {
            args.push("-l".into());
            args.push(user.into());
        }
        args.push("--".into());
        args.push((&self.host).into());
        args.push("ship".into());
        args.push("dial-stdio".into());
        if let Some(socket) = &self.socket {
            args.push("--socket".into());
            args.push(socket.into());
        }
        args
    }

    /// The arguments to `ssh` (not including `ssh` itself) that forward
    /// `127.0.0.1:local_port` on this machine to the daemon's unix socket on
    /// the host, running no remote command (`ship open`). The forward is
    /// to the socket, so no port on the host is assumed; `--` ends ssh's
    /// options before the host.
    pub fn forward_args(&self, local_port: u16, connect_timeout: Duration) -> Vec<OsString> {
        let secs = connect_timeout.as_secs().max(1);
        let default = Path::new(DEFAULT_SOCKET);
        let socket = self.socket.as_deref().unwrap_or(default);
        let mut args: Vec<OsString> = vec![
            "-N".into(),
            "-T".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ExitOnForwardFailure=yes".into(),
            "-o".into(),
            format!("ConnectTimeout={secs}").into(),
        ];
        if let Some(port) = self.port {
            args.push("-p".into());
            args.push(port.to_string().into());
        }
        if let Some(user) = &self.user {
            args.push("-l".into());
            args.push(user.into());
        }
        let mut forward = OsString::from(format!("127.0.0.1:{local_port}:"));
        forward.push(socket);
        args.push("-L".into());
        args.push(forward);
        args.push("--".into());
        args.push((&self.host).into());
        args
    }

    /// `USER@HOST` or `HOST`, for messages.
    pub fn display_host(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// `rest` after `scheme://`, matching the scheme case-insensitively.
fn strip_scheme<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    let (s, rest) = url.split_once("://")?;
    s.eq_ignore_ascii_case(scheme).then_some(rest)
}

fn parse_ssh(rest: &str, socket: Option<PathBuf>) -> Result<SshTarget, TargetError> {
    if rest.contains(['/', '?', '#']) {
        return Err(TargetError::UnexpectedPath);
    }
    let (user, hostport) = match rest.rsplit_once('@') {
        Some((user, hostport)) => (Some(user), hostport),
        None => (None, rest),
    };
    let user = user.map(validate_user).transpose()?;

    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        let (host, after) = inner.split_once(']').ok_or(TargetError::InvalidHost)?;
        if host.is_empty() || !host.bytes().all(|b| b.is_ascii_hexdigit() || b == b':') {
            return Err(TargetError::InvalidHost);
        }
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or(TargetError::InvalidHost)?),
        };
        (host, port)
    } else {
        match hostport.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (hostport, None),
        }
    };
    if host.is_empty() {
        return Err(TargetError::MissingHost);
    }
    if !hostport.starts_with('[') {
        validate_name(host).map_err(|()| TargetError::InvalidHost)?;
    }
    let port = port.map(parse_port).transpose()?;

    Ok(SshTarget {
        user,
        host: host.to_owned(),
        port,
        socket,
    })
}

/// Letters, digits, `.`, `-`, `_`; not empty; not starting with `-`.
fn validate_name(s: &str) -> Result<(), ()> {
    let ok = !s.is_empty()
        && !s.starts_with('-')
        && s.len() <= 255
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if ok { Ok(()) } else { Err(()) }
}

fn validate_user(s: &str) -> Result<String, TargetError> {
    validate_name(s)
        .map(|()| s.to_owned())
        .map_err(|()| TargetError::InvalidUser)
}

fn parse_port(s: &str) -> Result<u16, TargetError> {
    if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TargetError::InvalidPort);
    }
    match s.parse::<u16>() {
        Ok(0) | Err(_) => Err(TargetError::InvalidPort),
        Ok(port) => Ok(port),
    }
}

fn validate_socket(s: &str) -> Result<PathBuf, TargetError> {
    let ok = s.starts_with('/')
        && s.len() > 1
        && s.len() <= 1024
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'));
    if ok {
        Ok(PathBuf::from(s))
    } else {
        Err(TargetError::InvalidSocket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(url: &str) -> SshTarget {
        match Target::parse(url, None).unwrap() {
            Target::Ssh(s) => s,
            other => panic!("{other:?}"),
        }
    }

    fn args(t: &SshTarget) -> Vec<String> {
        t.ssh_args(Duration::from_secs(10))
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn local() {
        assert_eq!(
            Target::parse("local", None).unwrap(),
            Target::Local {
                socket: PathBuf::from(DEFAULT_SOCKET)
            }
        );
        assert_eq!(
            Target::parse("local", Some("/tmp/x/ship.sock")).unwrap(),
            Target::Local {
                socket: PathBuf::from("/tmp/x/ship.sock")
            }
        );
        assert_eq!(Target::parse("local", None).unwrap().url(), "local");
    }

    #[test]
    fn ssh_forms() {
        let t = ssh("ssh://deploy@lab.example");
        assert_eq!(t.user.as_deref(), Some("deploy"));
        assert_eq!(t.host, "lab.example");
        assert_eq!(t.port, None);

        let t = ssh("ssh://lab:2222");
        assert_eq!(t.user, None);
        assert_eq!(t.host, "lab");
        assert_eq!(t.port, Some(2222));

        let t = ssh("SSH://u_1@[fe80::1]:22");
        assert_eq!(t.host, "fe80::1");
        assert_eq!(t.port, Some(22));
        assert_eq!(t.display_host(), "u_1@fe80::1");

        for url in [
            "ssh://deploy@lab.example",
            "ssh://lab:2222",
            "ssh://u_1@[fe80::1]:22",
            "ssh://10.0.0.5",
        ] {
            assert_eq!(Target::parse(url, None).unwrap().url(), url);
        }
    }

    #[test]
    fn tailnet_is_phase_2() {
        assert_eq!(
            Target::parse("tailnet://lab", None),
            Err(TargetError::TailnetNotYet)
        );
        assert!(TargetError::TailnetNotYet.to_string().contains("Phase 2"));
    }

    #[test]
    fn unknown_forms() {
        for url in [
            "",
            "lab",
            "Local",
            "http://lab",
            "ssh:lab",
            "unix:///x",
            "ssh//lab",
        ] {
            assert_eq!(
                Target::parse(url, None),
                Err(TargetError::UnknownForm),
                "{url}"
            );
        }
    }

    #[test]
    fn hostile_values_are_rejected() {
        let cases = [
            ("ssh://", TargetError::MissingHost),
            ("ssh://deploy@", TargetError::MissingHost),
            ("ssh://-oProxyCommand=sh", TargetError::InvalidHost),
            ("ssh://lab;id", TargetError::InvalidHost),
            ("ssh://lab$(id)", TargetError::InvalidHost),
            ("ssh://lab`id`", TargetError::InvalidHost),
            ("ssh://lab|id", TargetError::InvalidHost),
            ("ssh://lab&id", TargetError::InvalidHost),
            ("ssh://lab id", TargetError::InvalidHost),
            ("ssh://lab\nid", TargetError::InvalidHost),
            ("ssh://lab'x", TargetError::InvalidHost),
            ("ssh://lab\"x", TargetError::InvalidHost),
            ("ssh://lab%h", TargetError::InvalidHost),
            ("ssh://lab*", TargetError::InvalidHost),
            ("ssh://[::1;id]", TargetError::InvalidHost),
            ("ssh://[::1]x", TargetError::InvalidHost),
            ("ssh://[::1", TargetError::InvalidHost),
            ("ssh://-l@lab", TargetError::InvalidUser),
            ("ssh://a;b@lab", TargetError::InvalidUser),
            ("ssh://$(id)@lab", TargetError::InvalidUser),
            ("ssh://a b@lab", TargetError::InvalidUser),
            ("ssh://a@b@lab", TargetError::InvalidUser),
            ("ssh://@lab", TargetError::InvalidUser),
            ("ssh://lab:", TargetError::InvalidPort),
            ("ssh://lab:0", TargetError::InvalidPort),
            ("ssh://lab:65536", TargetError::InvalidPort),
            ("ssh://lab:22;id", TargetError::InvalidPort),
            ("ssh://lab:-1", TargetError::InvalidPort),
            ("ssh://lab/path", TargetError::UnexpectedPath),
            ("ssh://lab?x=1", TargetError::UnexpectedPath),
            ("ssh://lab#x", TargetError::UnexpectedPath),
        ];
        for (url, expected) in cases {
            assert_eq!(Target::parse(url, None), Err(expected), "{url:?}");
        }
        for socket in [
            "relative.sock",
            "/",
            "/tmp/a b.sock",
            "/tmp/$(id).sock",
            "/tmp/a;b",
            "/tmp/a'b",
            "/tmp/a\nb",
        ] {
            assert_eq!(
                Target::parse("ssh://lab", Some(socket)),
                Err(TargetError::InvalidSocket),
                "{socket:?}"
            );
            assert_eq!(
                Target::parse("local", Some(socket)),
                Err(TargetError::InvalidSocket),
                "{socket:?}"
            );
        }
    }

    #[test]
    fn messages_do_not_echo_input() {
        let e = Target::parse("ssh://lab$(evil)", None).unwrap_err();
        assert!(!e.to_string().contains("evil"));
    }

    #[test]
    fn ssh_argv_is_structured() {
        assert_eq!(
            args(&ssh("ssh://lab")),
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "--",
                "lab",
                "ship",
                "dial-stdio"
            ]
        );
        let t = match Target::parse("ssh://deploy@lab.example:2222", Some("/srv/ship/s.sock")) {
            Ok(Target::Ssh(t)) => t,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            args(&t),
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-p",
                "2222",
                "-l",
                "deploy",
                "--",
                "lab.example",
                "ship",
                "dial-stdio",
                "--socket",
                "/srv/ship/s.sock"
            ]
        );
        // A sub-second timeout still gives ssh a valid ConnectTimeout.
        let a: Vec<String> = ssh("ssh://lab")
            .ssh_args(Duration::from_millis(200))
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert!(a.contains(&"ConnectTimeout=1".to_owned()));
    }

    fn forward(t: &SshTarget, port: u16) -> Vec<String> {
        t.forward_args(port, Duration::from_secs(10))
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn forward_argv_is_structured() {
        assert_eq!(
            forward(&ssh("ssh://lab"), 40123),
            [
                "-N",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ConnectTimeout=10",
                "-L",
                "127.0.0.1:40123:/run/ship/ship.sock",
                "--",
                "lab"
            ]
        );
        let t = match Target::parse("ssh://deploy@lab.example:2222", Some("/srv/ship/s.sock")) {
            Ok(Target::Ssh(t)) => t,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            forward(&t, 7181),
            [
                "-N",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ConnectTimeout=10",
                "-p",
                "2222",
                "-l",
                "deploy",
                "-L",
                "127.0.0.1:7181:/srv/ship/s.sock",
                "--",
                "lab.example"
            ]
        );
        // The host is the last argument, alone, after `--`.
        let a = forward(&ssh("ssh://[fe80::1]"), 1);
        assert_eq!(&a[a.len() - 2..], ["--", "fe80::1"]);
    }
}
