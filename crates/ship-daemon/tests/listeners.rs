//! The daemon listens only on its unix socket and 127.0.0.1 (ADR-0003
//! rule 1), as the operating system reports it.
//!
//! This file is its own test binary, so the sockets enumerated are the
//! daemon's own and those of nothing else.

mod common;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// One listening socket of this process.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Listener {
    Tcp(SocketAddr),
    Unix(PathBuf),
    /// Anything else: a UDP, raw or IPv6 socket, described.
    Other(String),
}

#[cfg(target_os = "linux")]
fn listeners() -> BTreeSet<Listener> {
    use std::collections::HashSet;
    use std::fs;
    use std::net::Ipv4Addr;

    // Socket inodes this process holds.
    let mut ours = HashSet::new();
    for entry in fs::read_dir("/proc/self/fd").unwrap() {
        let Ok(target) = fs::read_link(entry.unwrap().path()) else {
            continue;
        };
        let target = target.to_string_lossy().into_owned();
        if let Some(inode) = target
            .strip_prefix("socket:[")
            .and_then(|s| s.strip_suffix(']'))
        {
            ours.insert(inode.to_owned());
        }
    }

    let mut found = BTreeSet::new();
    let table =
        |name: &str| fs::read_to_string(format!("/proc/self/net/{name}")).unwrap_or_default();

    // tcp: sl local rem st ... inode(9). st 0A is LISTEN.
    for line in table("tcp").lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || !ours.contains(f[9]) || f[3] != "0A" {
            continue;
        }
        let (ip, port) = f[1].split_once(':').unwrap();
        let ip = u32::from_str_radix(ip, 16).unwrap();
        let ip = Ipv4Addr::from(ip.to_ne_bytes());
        let port = u16::from_str_radix(port, 16).unwrap();
        found.insert(Listener::Tcp(SocketAddr::from((ip, port))));
    }
    // Any IPv6, UDP or raw socket at all is unexpected.
    for name in ["tcp6", "udp", "udp6", "raw", "raw6"] {
        for line in table(name).lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 10 && ours.contains(f[9]) {
                found.insert(Listener::Other(format!("{name} {}", f[1])));
            }
        }
    }
    // unix: Num RefCount Protocol Flags Type St Inode [Path]. Flag
    // 00010000 (__SO_ACCEPTCON) marks a listening socket.
    for line in table("unix").lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 7 || !ours.contains(f[6]) {
            continue;
        }
        let flags = u32::from_str_radix(f[3], 16).unwrap();
        if flags & 0x0001_0000 == 0 {
            continue;
        }
        match f.get(7) {
            Some(path) => found.insert(Listener::Unix(PathBuf::from(path))),
            None => found.insert(Listener::Other(format!("unnamed unix {}", f[6]))),
        };
    }
    found
}

#[cfg(target_os = "macos")]
fn listeners() -> BTreeSet<Listener> {
    use std::process::Command;

    let pid = std::process::id().to_string();
    let lsof = |args: &[&str]| -> Vec<String> {
        let program = if std::path::Path::new("/usr/sbin/lsof").exists() {
            "/usr/sbin/lsof"
        } else {
            "lsof"
        };
        // Exit status 1 means "nothing matched"; only stdout matters.
        let out = Command::new(program)
            .args(["-nP", "-a", "-p", &pid])
            .args(args)
            .arg("-Fn")
            .output()
            .expect("run lsof");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix('n').map(str::to_owned))
            .collect()
    };

    let mut found = BTreeSet::new();
    for name in lsof(&["-iTCP", "-sTCP:LISTEN"]) {
        match name.parse::<SocketAddr>() {
            Ok(addr) => found.insert(Listener::Tcp(addr)),
            Err(_) => found.insert(Listener::Other(format!("tcp {name}"))),
        };
    }
    for name in lsof(&["-iUDP"]) {
        found.insert(Listener::Other(format!("udp {name}")));
    }
    // Unnamed socket pairs (the runtime's own wake-up channels) show as
    // `->0x…` or a bare address; only a filesystem name can be listening.
    for name in lsof(&["-U"]) {
        if name.starts_with('/') {
            found.insert(Listener::Unix(PathBuf::from(name)));
        }
    }
    found
}

/// macOS may report a socket under its resolved /private path.
fn normalise(set: &BTreeSet<Listener>) -> BTreeSet<Listener> {
    set.iter()
        .map(|l| match l {
            Listener::Unix(p) => Listener::Unix(std::fs::canonicalize(p).unwrap_or(p.clone())),
            other => other.clone(),
        })
        .collect()
}

/// The listeners in `found` that break ADR-0003 rule 1: anything but the
/// daemon's own unix `socket` and a TCP listener on IPv4 loopback.
fn violations(found: &BTreeSet<Listener>, socket: &std::path::Path) -> Vec<Listener> {
    let socket = std::fs::canonicalize(socket).unwrap_or(socket.to_path_buf());
    normalise(found)
        .into_iter()
        .filter(|l| match l {
            Listener::Unix(p) => *p != socket,
            Listener::Tcp(addr) => !(addr.is_ipv4() && addr.ip().is_loopback()),
            Listener::Other(_) => true,
        })
        .collect()
}

/// The two tests enumerate the same process's sockets; one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test(flavor = "multi_thread")]
async fn only_unix_socket_and_loopback_listen() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let config = common::config(dir.path());
    let running = ship_daemon::start(&config).await.unwrap();

    let socket = running.socket_path().to_path_buf();
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o660);

    let found = listeners();
    let expected: BTreeSet<Listener> = [
        Listener::Unix(socket.clone()),
        Listener::Tcp(running.tcp_addr()),
    ]
    .into();
    assert_eq!(normalise(&found), normalise(&expected));
    assert_eq!(violations(&found, &socket), []);

    running.shutdown().await.unwrap();
    assert!(listeners().is_empty(), "{:?}", listeners());
}

/// The check has teeth: a listener deliberately bound on every address,
/// and one on a unix socket that is not the daemon's, are both found.
#[tokio::test(flavor = "multi_thread")]
async fn a_listener_bound_elsewhere_is_found() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let config = common::config(dir.path());
    let running = ship_daemon::start(&config).await.unwrap();
    let socket = running.socket_path().to_path_buf();

    let wide = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let wide_addr = wide.local_addr().unwrap();
    let stray_path = dir.path().join("stray.sock");
    let stray = std::os::unix::net::UnixListener::bind(&stray_path).unwrap();

    let found = violations(&listeners(), &socket);
    // lsof reports the unspecified address as `*:PORT`.
    let port = format!(":{}", wide_addr.port());
    assert!(
        found.iter().any(|l| match l {
            Listener::Tcp(a) => *a == wide_addr,
            Listener::Other(s) => s.starts_with("tcp") && s.ends_with(&port),
            Listener::Unix(_) => false,
        }),
        "{found:?}"
    );
    let stray_path = std::fs::canonicalize(&stray_path).unwrap();
    assert!(found.contains(&Listener::Unix(stray_path)), "{found:?}");
    assert_eq!(found.len(), 2, "{found:?}");

    drop((wide, stray));
    assert_eq!(violations(&listeners(), &socket), []);
    running.shutdown().await.unwrap();
}
