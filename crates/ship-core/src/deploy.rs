//! Deployment decisions that need no I/O: which build definition a
//! revision has, how `--port` maps onto what the image exposes, and where a
//! published port binds (phase-1 plan, decisions; ADR-0010 rule 3).

use std::fmt;
// `core::net`: the same types as their `std` re-exports, and no I/O.
use core::net::IpAddr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Compose file names Ship looks for at the root of a revision, in order.
pub const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// The Dockerfile Ship looks for at the root of a revision.
pub const DOCKERFILE: &str = "Dockerfile";

/// Longest `--file` path accepted, in bytes.
pub const MAX_FILE_PATH: usize = 4096;

string_enum! {
    /// What a revision is built and run from.
    pub enum BuildKind {
        Dockerfile => "dockerfile",
        Compose => "compose",
    }
}

/// The build definition chosen for a revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub kind: BuildKind,
    /// Relative to the revision's root, `/`-separated.
    pub path: String,
    /// Set when the choice needs saying: both kinds were present.
    pub note: Option<String>,
}

/// Why no build definition was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectError {
    /// `--file` is not a plain relative path inside the revision.
    InvalidPath(String),
    /// `--file` names a file the revision does not have.
    Missing(String),
    /// Neither a Dockerfile nor a Compose file at the root.
    NoDefinition,
}

impl fmt::Display for DetectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DetectError::InvalidPath(why) => write!(f, "--file {why}"),
            DetectError::Missing(path) => write!(f, "the revision has no file `{path}`"),
            DetectError::NoDefinition => write!(
                f,
                "the revision has neither a {DOCKERFILE} nor a Compose file ({}) at its root",
                COMPOSE_FILES.join(", ")
            ),
        }
    }
}

impl std::error::Error for DetectError {}

/// Checks a `--file` value: a relative path of plain components (no
/// empty, `.` or `..` component, no backslash or control character), at
/// most [`MAX_FILE_PATH`] bytes.
pub fn check_file_path(path: &str) -> Result<(), DetectError> {
    let invalid = |why: &str| Err(DetectError::InvalidPath(why.to_owned()));
    if path.is_empty() {
        return invalid("must not be empty");
    }
    if path.len() > MAX_FILE_PATH {
        return invalid("is too long");
    }
    if path.starts_with('/') {
        return invalid("must be a path relative to the pushed directory");
    }
    if path.chars().any(|c| c.is_control() || c == '\\') {
        return invalid("must not contain control characters or backslashes");
    }
    if path
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return invalid("must not contain empty, `.` or `..` components");
    }
    Ok(())
}

/// Whether `path` names a Compose file rather than a Dockerfile.
fn is_compose_name(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.ends_with(".yaml") || name.ends_with(".yml")
}

/// Chooses the build definition of a revision. `exists` answers whether a
/// regular file exists at a relative path inside it.
///
/// With `file`, that file is used: a `.yaml` or `.yml` name is Compose,
/// anything else a Dockerfile. Without it, a Compose file at the root wins
/// over a `Dockerfile`, and the note says which was chosen.
pub fn detect(file: Option<&str>, exists: impl Fn(&str) -> bool) -> Result<Detected, DetectError> {
    if let Some(path) = file {
        check_file_path(path)?;
        if !exists(path) {
            return Err(DetectError::Missing(path.to_owned()));
        }
        let kind = if is_compose_name(path) {
            BuildKind::Compose
        } else {
            BuildKind::Dockerfile
        };
        return Ok(Detected {
            kind,
            path: path.to_owned(),
            note: None,
        });
    }
    let compose = COMPOSE_FILES.iter().find(|f| exists(f));
    let dockerfile = exists(DOCKERFILE);
    match (compose, dockerfile) {
        (Some(c), true) => Ok(Detected {
            kind: BuildKind::Compose,
            path: (*c).to_owned(),
            note: Some(format!(
                "both {c} and {DOCKERFILE} are present; deploying {c} \
                 (pass --file {DOCKERFILE} to deploy the Dockerfile)"
            )),
        }),
        (Some(c), false) => Ok(Detected {
            kind: BuildKind::Compose,
            path: (*c).to_owned(),
            note: None,
        }),
        (None, true) => Ok(Detected {
            kind: BuildKind::Dockerfile,
            path: DOCKERFILE.to_owned(),
            note: None,
        }),
        (None, false) => Err(DetectError::NoDefinition),
    }
}

/// A `--port` value: `N` (host port N, container port by the `EXPOSE`
/// rule) or `HOST:CONTAINER`. Serialised as that string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortSpec {
    pub host: u16,
    pub container: Option<u16>,
}

/// A `--port` value that is not `N` or `HOST:CONTAINER` with ports 1 to
/// 65535.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSpecError;

impl fmt::Display for PortSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("--port must be N or HOST:CONTAINER, with ports from 1 to 65535")
    }
}

impl std::error::Error for PortSpecError {}

fn parse_port(s: &str) -> Result<u16, PortSpecError> {
    if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(PortSpecError);
    }
    match s.parse::<u16>() {
        Ok(0) | Err(_) => Err(PortSpecError),
        Ok(p) => Ok(p),
    }
}

impl FromStr for PortSpec {
    type Err = PortSpecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once(':') {
            None => Ok(PortSpec {
                host: parse_port(s)?,
                container: None,
            }),
            Some((host, container)) => Ok(PortSpec {
                host: parse_port(host)?,
                container: Some(parse_port(container)?),
            }),
        }
    }
}

impl fmt::Display for PortSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.container {
            None => write!(f, "{}", self.host),
            Some(c) => write!(f, "{}:{c}", self.host),
        }
    }
}

impl Serialize for PortSpec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PortSpec {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

string_enum! {
    /// A transport protocol of a port.
    #[derive(PartialOrd, Ord)]
    pub enum Protocol {
        Tcp => "tcp",
        Udp => "udp",
    }
}

/// A port an image declares with `EXPOSE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExposedPort {
    pub port: u16,
    pub protocol: Protocol,
}

impl ExposedPort {
    /// Parses Docker's form, `3000/tcp` or `53/udp` (`3000` alone is tcp).
    /// Anything else, a range or `sctp` included, is `None`.
    pub fn parse(s: &str) -> Option<ExposedPort> {
        let (port, protocol) = match s.split_once('/') {
            None => (s, Protocol::Tcp),
            Some((port, proto)) => (port, proto.parse().ok()?),
        };
        Some(ExposedPort {
            port: parse_port(port).ok()?,
            protocol,
        })
    }
}

impl fmt::Display for ExposedPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.port, self.protocol)
    }
}

/// Where a published port goes: host port to container port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortMapping {
    pub host: u16,
    pub container: u16,
    pub protocol: Protocol,
}

/// `--port N` on an image that exposes more than one port: the candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousPort {
    pub host: u16,
    pub exposed: Vec<ExposedPort>,
}

impl fmt::Display for AmbiguousPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<String> = self.exposed.iter().map(ToString::to_string).collect();
        let first = self.exposed.first().map_or(0, |p| p.port);
        write!(
            f,
            "the image exposes {} ports ({}); choose one, e.g. --port {}:{first}",
            self.exposed.len(),
            names.join(", "),
            self.host
        )
    }
}

impl std::error::Error for AmbiguousPort {}

/// Maps `--port` onto the image's `EXPOSE` ports (Human, 2026-11-27):
/// `HOST:CONTAINER` is always taken as given; `N` maps to the one exposed
/// port, or to N when none is exposed; with several it is refused, naming
/// each. A port exposed as udp only is published as udp.
pub fn map_port(spec: PortSpec, exposed: &[ExposedPort]) -> Result<PortMapping, AmbiguousPort> {
    let mut ports: Vec<ExposedPort> = exposed.to_vec();
    ports.sort();
    ports.dedup();
    let protocol_of = |port: u16| {
        let has = |protocol| ports.contains(&ExposedPort { port, protocol });
        if has(Protocol::Udp) && !has(Protocol::Tcp) {
            Protocol::Udp
        } else {
            Protocol::Tcp
        }
    };
    let container = match spec.container {
        Some(c) => c,
        None => {
            let mut numbers: Vec<u16> = ports.iter().map(|p| p.port).collect();
            numbers.dedup();
            match numbers.as_slice() {
                [] => spec.host,
                [one] => *one,
                _ => {
                    return Err(AmbiguousPort {
                        host: spec.host,
                        exposed: ports,
                    });
                }
            }
        }
    };
    Ok(PortMapping {
        host: spec.host,
        container,
        protocol: protocol_of(container),
    })
}

/// Where Ship binds the ports it publishes: `auto` (the host's tailnet
/// address when it has one, else `127.0.0.1`) or a fixed address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PublishAddress {
    #[default]
    Auto,
    Fixed(IpAddr),
}

/// A `publish.address` value that is neither `auto` nor a unicast or
/// unspecified IP address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishAddressError;

impl fmt::Display for PublishAddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "publish.address must be `auto` or an IP address of the host \
             (for example 127.0.0.1, or 0.0.0.0 for every address)",
        )
    }
}

impl std::error::Error for PublishAddressError {}

impl FromStr for PublishAddress {
    type Err = PublishAddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "auto" {
            return Ok(PublishAddress::Auto);
        }
        let ip: IpAddr = s.parse().map_err(|_| PublishAddressError)?;
        let broadcast = matches!(ip, IpAddr::V4(v4) if v4.is_broadcast());
        if ip.is_multicast() || broadcast {
            return Err(PublishAddressError);
        }
        Ok(PublishAddress::Fixed(ip))
    }
}

impl fmt::Display for PublishAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishAddress::Auto => f.write_str("auto"),
            PublishAddress::Fixed(ip) => write!(f, "{ip}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{Ipv4Addr, Ipv6Addr};

    fn tree<'a>(files: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |p| files.contains(&p)
    }

    #[test]
    fn compose_wins_over_a_dockerfile_and_says_so() {
        for compose in COMPOSE_FILES {
            let d = detect(None, tree(&[compose, "Dockerfile"])).unwrap();
            assert_eq!(d.kind, BuildKind::Compose);
            assert_eq!(d.path, compose);
            let note = d.note.unwrap();
            assert!(
                note.contains(compose) && note.contains("--file Dockerfile"),
                "{note}"
            );

            let d = detect(None, tree(&[compose])).unwrap();
            assert_eq!((d.kind, d.note), (BuildKind::Compose, None));
        }
        let d = detect(None, tree(&["Dockerfile", "src/main.rs"])).unwrap();
        assert_eq!(d.kind, BuildKind::Dockerfile);
        assert_eq!(d.path, "Dockerfile");
        assert_eq!(d.note, None);
        assert_eq!(
            detect(None, tree(&["README.md"])),
            Err(DetectError::NoDefinition)
        );
        // Not at the root: not found.
        assert_eq!(
            detect(None, tree(&["app/Dockerfile"])),
            Err(DetectError::NoDefinition)
        );
    }

    #[test]
    fn file_overrides_detection() {
        let files = [
            "compose.yaml",
            "Dockerfile",
            "docker/Dockerfile.prod",
            "ops/stack.yml",
        ];
        let d = detect(Some("Dockerfile"), tree(&files)).unwrap();
        assert_eq!(
            (d.kind, d.path.as_str()),
            (BuildKind::Dockerfile, "Dockerfile")
        );
        let d = detect(Some("docker/Dockerfile.prod"), tree(&files)).unwrap();
        assert_eq!(d.kind, BuildKind::Dockerfile);
        let d = detect(Some("ops/stack.yml"), tree(&files)).unwrap();
        assert_eq!(d.kind, BuildKind::Compose);
        assert_eq!(
            detect(Some("nope/Dockerfile"), tree(&files)),
            Err(DetectError::Missing("nope/Dockerfile".to_owned()))
        );
        for bad in [
            "",
            "/etc/passwd",
            "../Dockerfile",
            "a/../Dockerfile",
            "./Dockerfile",
            "a//b",
            "a/",
            "a\\b",
            "a\nb",
        ] {
            assert!(
                matches!(
                    detect(Some(bad), |_| true),
                    Err(DetectError::InvalidPath(_))
                ),
                "{bad:?}"
            );
        }
        let long = "a".repeat(MAX_FILE_PATH + 1);
        assert!(check_file_path(&long).is_err());
    }

    #[test]
    fn port_specs() {
        assert_eq!(
            "8080".parse(),
            Ok(PortSpec {
                host: 8080,
                container: None
            })
        );
        assert_eq!(
            "8080:3000".parse(),
            Ok(PortSpec {
                host: 8080,
                container: Some(3000)
            })
        );
        assert_eq!(
            "65535:1".parse::<PortSpec>().unwrap().to_string(),
            "65535:1"
        );
        for bad in [
            "", "0", "65536", "-1", "+80", "80:", ":80", "a", "80:0", "1:2:3", " 80", "080000",
        ] {
            assert_eq!(bad.parse::<PortSpec>(), Err(PortSpecError), "{bad:?}");
        }
        let spec: PortSpec = serde_json::from_str("\"8080:3000\"").unwrap();
        assert_eq!(serde_json::to_string(&spec).unwrap(), "\"8080:3000\"");
        assert!(serde_json::from_str::<PortSpec>("8080").is_err());
        assert!(serde_json::from_str::<PortSpec>("\"x\"").is_err());
    }

    fn tcp(port: u16) -> ExposedPort {
        ExposedPort {
            port,
            protocol: Protocol::Tcp,
        }
    }

    #[test]
    fn exposed_ports_parse() {
        assert_eq!(ExposedPort::parse("3000/tcp"), Some(tcp(3000)));
        assert_eq!(ExposedPort::parse("3000"), Some(tcp(3000)));
        assert_eq!(
            ExposedPort::parse("53/udp"),
            Some(ExposedPort {
                port: 53,
                protocol: Protocol::Udp
            })
        );
        for bad in ["", "x/tcp", "80/sctp", "8000-8010/tcp", "0/tcp", "/tcp"] {
            assert_eq!(ExposedPort::parse(bad), None, "{bad}");
        }
        assert_eq!(tcp(3000).to_string(), "3000/tcp");
    }

    #[test]
    fn the_expose_rule() {
        let n = |host| PortSpec {
            host,
            container: None,
        };
        let mapping = |host, container, protocol| PortMapping {
            host,
            container,
            protocol,
        };
        // None exposed: N to N.
        assert_eq!(
            map_port(n(8080), &[]),
            Ok(mapping(8080, 8080, Protocol::Tcp))
        );
        // One: N to it.
        assert_eq!(
            map_port(n(8080), &[tcp(3000)]),
            Ok(mapping(8080, 3000, Protocol::Tcp))
        );
        // The same port twice, or over two protocols, is one port.
        let udp = |port| ExposedPort {
            port,
            protocol: Protocol::Udp,
        };
        let both = [tcp(3000), tcp(3000), udp(3000)];
        assert_eq!(
            map_port(n(8080), &both),
            Ok(mapping(8080, 3000, Protocol::Tcp))
        );
        assert_eq!(
            map_port(n(5353), &[udp(53)]),
            Ok(mapping(5353, 53, Protocol::Udp))
        );
        // Several: refused, naming each.
        let err = map_port(n(8080), &[tcp(8080), tcp(3000)]).unwrap_err();
        assert_eq!(err.exposed, [tcp(3000), tcp(8080)]);
        let text = err.to_string();
        assert!(
            text.contains("3000/tcp") && text.contains("8080/tcp"),
            "{text}"
        );
        assert!(text.contains("--port 8080:3000"), "{text}");
        // Explicit: always taken.
        let explicit = PortSpec {
            host: 8080,
            container: Some(3000),
        };
        assert_eq!(
            map_port(explicit, &[tcp(8080), tcp(3000)]),
            Ok(mapping(8080, 3000, Protocol::Tcp))
        );
        assert_eq!(
            map_port(explicit, &[]),
            Ok(mapping(8080, 3000, Protocol::Tcp))
        );
    }

    #[test]
    fn publish_addresses() {
        assert_eq!("auto".parse(), Ok(PublishAddress::Auto));
        assert_eq!(PublishAddress::default(), PublishAddress::Auto);
        for (s, ip) in [
            ("127.0.0.1", IpAddr::V4(Ipv4Addr::LOCALHOST)),
            ("0.0.0.0", IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            (
                "100.101.102.103",
                IpAddr::V4(Ipv4Addr::new(100, 101, 102, 103)),
            ),
            ("::1", IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ] {
            let parsed: PublishAddress = s.parse().unwrap();
            assert_eq!(parsed, PublishAddress::Fixed(ip));
            assert_eq!(parsed.to_string(), s);
        }
        for bad in [
            "",
            "Auto",
            "localhost",
            "224.0.0.1",
            "255.255.255.255",
            "ff02::1",
            "127.0.0.1:80",
            " 127.0.0.1",
        ] {
            assert_eq!(
                bad.parse::<PublishAddress>(),
                Err(PublishAddressError),
                "{bad:?}"
            );
        }
    }
}
