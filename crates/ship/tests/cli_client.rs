//! The client from the built binary: targets, `ship projects` over the
//! local and ssh transports, `ship dial-stdio`, and the errors a user sees
//! (ADR-0003, ADR-0004, OVERVIEW §8).
//!
//! The daemon runs in-process (`ship_daemon::start`) on a temporary socket,
//! so these tests run on macOS too, where `ship daemon` refuses to start.
//! The ssh transport is exercised through a fake `ssh` on PATH that records
//! its argv and execs the remote command (`ship dial-stdio …`) locally; the
//! real-sshd version of the same check is `ssh_real.rs`.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use ship_docker::Runner;

const SHIP: &str = env!("CARGO_BIN_EXE_ship");
const FIXTURE: &str = include_str!("fixtures/projects-empty.json");
const HOST_FIXTURE: &str = include_str!("fixtures/host.json");

/// A fake `docker` answering `version` with 28.3.2 and `compose version`
/// with 2.39.1, the fixture's versions.
const FAKE_DOCKER: &str = r#"#!/bin/sh
case "$1" in
  version) echo 28.3.2 ;;
  compose) echo 2.39.1 ;;
  *) exit 64 ;;
esac
"#;

/// A runner for [`FAKE_DOCKER`]. Written once per test binary, so no test
/// executes a file another thread still has open for writing.
fn fake_docker() -> Runner {
    static PATH: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let (_, path) = PATH.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docker");
        fs::write(&path, FAKE_DOCKER).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, path)
    });
    Runner::new(path)
}

/// The real `docker` CLI pointed at an Engine socket that cannot exist.
fn unreachable_docker() -> Runner {
    Runner::docker().env("DOCKER_HOST", "unix:///nonexistent/ship-test/docker.sock")
}

/// A fake `ssh`: records its arguments one per line, then either hangs or
/// runs the remote command's `ship …` words with the real binary.
const FAKE_SSH: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$FAKE_SSH_ARGV"
echo $$ > "$FAKE_SSH_PID"
if [ "$FAKE_SSH_MODE" = hang ]; then
  exec sleep 60
fi
if [ "$FAKE_SSH_MODE" = eof ]; then
  exec >&-
  exec sleep 60
fi
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift 2
[ "$1" = ship ] || exit 97
shift
exec "$SHIP_BIN" "$@"
"#;

/// A temporary home for one test: config file, fake ssh and socket.
struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let env = Env {
            dir: tempfile::tempdir().unwrap(),
        };
        let bin = env.dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let ssh = bin.join("ssh");
        fs::write(&ssh, FAKE_SSH).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        env
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn config(&self) -> PathBuf {
        self.path().join("cfg").join("ship").join("config.json")
    }

    fn argv_file(&self) -> PathBuf {
        self.path().join("ssh-argv")
    }

    fn ship(&self) -> Command {
        let mut c = Command::new(SHIP);
        let path = format!(
            "{}:{}",
            self.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        c.env("PATH", path)
            .env("SHIP_CONFIG", self.config())
            .env("SHIP_BIN", SHIP)
            .env("FAKE_SSH_ARGV", self.argv_file())
            .env("FAKE_SSH_PID", self.path().join("ssh-pid"))
            .env_remove("FAKE_SSH_MODE");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.ship().args(args).output().unwrap()
    }

    fn add(&self, name: &str, url: &str, socket: Option<&Path>) {
        let mut c = self.ship();
        c.args(["target", "add", name, url]);
        if let Some(s) = socket {
            c.arg("--socket").arg(s);
        }
        let out = c.output().unwrap();
        assert!(out.status.success(), "{}", show(&out));
    }

    fn recorded_argv(&self) -> Vec<String> {
        fs::read_to_string(self.argv_file())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn show(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// A daemon running in-process on its own runtime; shut down on drop.
struct Daemon {
    rt: tokio::runtime::Runtime,
    running: Option<ship_daemon::Running>,
    socket: PathBuf,
}

impl Daemon {
    fn start(dir: &Path) -> Daemon {
        Daemon::start_with(dir, None)
    }

    /// With `docker` as the daemon's Docker runner, or the default.
    fn start_with(dir: &Path, docker: Option<Runner>) -> Daemon {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut config = ship_daemon::Config {
            state_dir: dir.join("state"),
            socket_path: dir.join("ship.sock"),
            port: 0,
            ..ship_daemon::Config::new(ship_daemon::Redactor::new())
        };
        if let Some(docker) = docker {
            config.docker = docker;
        }
        let running = rt.block_on(ship_daemon::start(&config)).unwrap();
        let socket = running.socket_path().to_path_buf();
        Daemon {
            rt,
            running: Some(running),
            socket,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = fs::set_permissions(&self.socket, fs::Permissions::from_mode(0o660));
            let _ = self.rt.block_on(running.shutdown());
        }
    }
}

#[test]
fn projects_json_over_local_matches_fixture() {
    let env = Env::new();
    let daemon = Daemon::start(env.path());
    env.add("here", "local", Some(&daemon.socket));

    let out = env.run(&["projects", "-o", "json"]);
    assert!(out.status.success(), "{}", show(&out));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), FIXTURE);

    let out = env.run(&["projects"]);
    assert!(out.status.success(), "{}", show(&out));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "No projects.\n");
}

#[test]
fn projects_over_ssh_returns_the_same_bytes() {
    let env = Env::new();
    let daemon = Daemon::start(env.path());
    env.add("lab", "ssh://tester@lab.example:2222", Some(&daemon.socket));

    let out = env.run(&["projects", "-o", "json"]);
    assert!(out.status.success(), "{}", show(&out));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), FIXTURE);

    let socket = daemon.socket.display().to_string();
    assert_eq!(
        env.recorded_argv(),
        [
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=30",
            "-p",
            "2222",
            "-l",
            "tester",
            "--",
            "lab.example",
            "ship",
            "dial-stdio",
            "--socket",
            socket.as_str(),
        ]
    );
}

#[test]
fn no_daemon_names_the_socket_and_how_to_start_one() {
    let env = Env::new();
    let socket = env.path().join("absent.sock");
    env.add("here", "local", Some(&socket));
    let out = env.run(&["projects"]);
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    let err = stderr(&out);
    assert!(err.contains(&socket.display().to_string()), "{err}");
    assert!(err.contains("systemctl start ship"), "{err}");
    assert!(err.contains("ship daemon"), "{err}");

    // Through ssh, dial-stdio's own message reaches the user.
    env.add("lab", "ssh://lab", Some(&socket));
    let out = env.run(&["target", "use", "lab"]);
    assert!(out.status.success(), "{}", show(&out));
    let out = env.run(&["projects"]);
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    let err = stderr(&out);
    assert!(err.contains("no Ship daemon is listening"), "{err}");
    assert!(err.contains("systemctl start ship"), "{err}");
}

#[test]
fn no_target_uses_the_default_local_socket() {
    if Path::new(ship_net::DEFAULT_SOCKET).exists() {
        eprintln!("skipped: a daemon socket exists at the default path");
        return;
    }
    let env = Env::new();
    let out = env.run(&["projects"]);
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    let err = stderr(&out);
    assert!(err.contains(ship_net::DEFAULT_SOCKET), "{err}");
    assert!(err.contains("ship target add"), "{err}");
}

#[test]
fn unreachable_host_times_out_and_ssh_is_killed() {
    let env = Env::new();
    env.add("lab", "ssh://lab.invalid", None);
    let start = Instant::now();
    let out = env
        .ship()
        .env("FAKE_SSH_MODE", "hang")
        .args(["projects", "--timeout", "1"])
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    let err = stderr(&out);
    assert!(err.contains("timed out after 1s"), "{err}");

    let pid: i32 = fs::read_to_string(env.path().join("ssh-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks whether the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "ssh (pid {pid}) is still running");
}

#[test]
fn early_eof_with_ssh_still_running_times_out_within_the_deadline() {
    let env = Env::new();
    env.add("lab", "ssh://lab.invalid", None);
    let start = Instant::now();
    let out = env
        .ship()
        .env("FAKE_SSH_MODE", "eof")
        .args(["projects", "--timeout", "1"])
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    assert!(elapsed < Duration::from_millis(2500), "{elapsed:?}");
    let err = stderr(&out);
    assert!(err.contains("timed out after 1s"), "{err}");

    let pid: i32 = fs::read_to_string(env.path().join("ssh-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks whether the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "ssh (pid {pid}) is still running");
}

#[test]
fn permission_denied_names_the_group() {
    if is_root() {
        eprintln!("skipped: root bypasses socket permissions");
        return;
    }
    let env = Env::new();
    let daemon = Daemon::start(env.path());
    fs::set_permissions(&daemon.socket, fs::Permissions::from_mode(0o000)).unwrap();
    let gid = fs::metadata(&daemon.socket).unwrap().gid();
    let group = ship_net::group_name(gid).expect("socket group has a name");

    env.add("here", "local", Some(&daemon.socket));
    env.add("lab", "ssh://lab", Some(&daemon.socket));
    for target in ["here", "lab"] {
        let out = env.run(&["target", "use", target]);
        assert!(out.status.success(), "{}", show(&out));
        let out = env.run(&["projects"]);
        assert_eq!(out.status.code(), Some(1), "{target}: {}", show(&out));
        let err = stderr(&out);
        assert!(err.contains("permission denied"), "{target}: {err}");
        assert!(err.contains(&format!("`{group}`")), "{target}: {err}");
        assert!(err.contains("usermod -aG"), "{target}: {err}");
    }
}

#[test]
fn hostile_targets_are_rejected_and_nothing_changes() {
    let env = Env::new();
    env.add("lab", "ssh://deploy@lab", None);
    let before = fs::read(env.config()).unwrap();
    for url in [
        "ssh://lab;touch${IFS}pwned",
        "ssh://$(touch pwned)",
        "ssh://`touch pwned`",
        "ssh://lab|sh",
        "ssh://lab&&id",
        "ssh://lab id",
        "ssh://-oProxyCommand=touch pwned",
        "ssh://-oProxyCommand=sh@lab",
        "ssh://a;b@lab",
        "ssh://$(id)@lab",
        "ssh://a b@lab",
        "ssh://lab/../etc",
        "ssh://lab:22;id",
        "ssh://[::1;id]",
    ] {
        let out = env.run(&["target", "add", "evil", url]);
        assert_eq!(out.status.code(), Some(2), "{url}: {}", show(&out));
        assert!(!stderr(&out).contains("pwned"), "{url}: echoed input");
    }
    for socket in ["/tmp/$(touch pwned)", "relative.sock", "/tmp/a b"] {
        let out = env.run(&["target", "add", "evil", "ssh://lab", "--socket", socket]);
        assert_eq!(out.status.code(), Some(2), "{socket}: {}", show(&out));
    }
    assert_eq!(fs::read(env.config()).unwrap(), before);
    assert!(!env.argv_file().exists(), "ssh was run");
    assert!(!env.path().join("pwned").exists());
}

#[test]
fn tailnet_targets_arrive_in_phase_2() {
    let env = Env::new();
    let out = env.run(&["target", "add", "tn", "tailnet://lab"]);
    assert_eq!(out.status.code(), Some(2), "{}", show(&out));
    assert!(stderr(&out).contains("Phase 2"), "{}", show(&out));
    assert!(!env.config().exists());
}

#[test]
fn target_add_use_list() {
    let env = Env::new();
    let out = env.run(&["target", "list"]);
    assert!(out.status.success(), "{}", show(&out));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("No targets."));

    env.add("lab", "ssh://deploy@lab", None);
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&env.config()), 0o600);
    assert_eq!(mode(env.config().parent().unwrap()), 0o700);

    env.add("gpu", "ssh://gpu:2200", None);
    let out = env.run(&["target", "list", "-o", "json"]);
    assert!(out.status.success(), "{}", show(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "targets": [
            { "name": "gpu", "url": "ssh://gpu:2200", "socket": null, "current": false },
            { "name": "lab", "url": "ssh://deploy@lab", "socket": null, "current": true },
        ]})
    );

    let out = env.run(&["target", "add", "lab", "local"]);
    assert_eq!(out.status.code(), Some(2), "{}", show(&out));
    let out = env.run(&["target", "use", "nope"]);
    assert_eq!(out.status.code(), Some(2), "{}", show(&out));

    let out = env.run(&["target", "use", "gpu"]);
    assert!(out.status.success(), "{}", show(&out));
    let out = env.run(&["target", "list"]);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "* gpu  ssh://gpu:2200\n  lab  ssh://deploy@lab\n"
    );
    assert_eq!(mode(&env.config()), 0o600);
}

/// Reads from `r` until `needle` has been seen, failing after 10 s.
fn read_until(r: &mut impl Read, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !got.windows(needle.len()).any(|w| w == needle) {
        assert!(Instant::now() < deadline, "timed out; got {got:?}");
        let n = r.read(&mut buf).unwrap();
        assert!(
            n > 0,
            "EOF before {needle:?}; got {:?}",
            String::from_utf8_lossy(&got)
        );
        got.extend_from_slice(&buf[..n]);
    }
    got
}

/// Waits up to 10 s for `child` to exit.
fn wait_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("dial-stdio did not exit");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn dial_stdio_bridges_and_exits_when_either_side_closes() {
    let env = Env::new();
    let daemon = Daemon::start(env.path());
    let spawn = || {
        Command::new(SHIP)
            .arg("dial-stdio")
            .arg("--socket")
            .arg(&daemon.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };

    // Client closes first: after a keep-alive exchange, closing stdin ends it.
    let mut child = spawn();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    stdin
        .write_all(b"GET /api/v1/projects HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let got = read_until(&mut stdout, b"{\"projects\":[]}");
    assert!(got.starts_with(b"HTTP/1.1 200"), "{got:?}");
    drop(stdin);
    assert!(wait_exit(&mut child).success());

    // Daemon closes first: `Connection: close` ends it with stdin still open.
    let mut child = spawn();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    stdin
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut all = Vec::new();
    stdout.read_to_end(&mut all).unwrap();
    assert!(all.starts_with(b"HTTP/1.1 200"), "{all:?}");
    assert!(wait_exit(&mut child).success());
    drop(stdin);
}

#[test]
fn incompatible_daemon_gets_the_upgrade_instruction() {
    let env = Env::new();
    let socket = env.path().join("v2.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        read_until(&mut conn, b"\r\n\r\n");
        let body = r#"{"projects":[]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nship-api-version: v2\r\nship-version: 9.0.0\r\n\
             content-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        );
        conn.write_all(response.as_bytes()).unwrap();
    });
    env.add("next", "local", Some(&socket));
    let out = env.run(&["projects", "-o", "json"]);
    server.join().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    assert!(out.stdout.is_empty(), "{}", show(&out));
    let err = stderr(&out);
    assert!(err.contains("speaks API v1"), "{err}");
    assert!(err.contains("the daemon speaks v2"), "{err}");
    assert!(err.contains("Upgrade this client to ship 9.0.0"), "{err}");
}

/// `ship host -o json` with the fields that vary by host and build set to
/// the fixture's values, after checking each has the right JSON type.
fn normalised_host(stdout: &[u8]) -> serde_json::Value {
    let fixture: serde_json::Value = serde_json::from_str(HOST_FIXTURE).unwrap();
    let mut v: serde_json::Value = serde_json::from_slice(stdout).unwrap();
    for key in ["version", "os", "arch"] {
        assert!(v[key].is_string(), "{key}: {v}");
        v[key] = fixture[key].clone();
    }
    for key in ["cpus", "memory_bytes", "disk_free_bytes"] {
        let n = &v["capacity"][key];
        assert!(n.is_u64() || n.is_null(), "capacity.{key}: {v}");
        v["capacity"][key] = fixture["capacity"][key].clone();
    }
    v
}

/// `ship host -o json` output with the `disk_free_bytes` line's value
/// blanked, so two answers can be compared byte for byte.
fn without_disk_free(stdout: &[u8]) -> String {
    let text = String::from_utf8(stdout.to_vec()).unwrap();
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("\"disk_free_bytes\":") {
            out.push_str("    \"disk_free_bytes\": _\n");
        } else {
            out.push_str(line);
        }
    }
    assert_eq!(out.matches("disk_free_bytes").count(), 1, "{text}");
    out
}

#[test]
fn host_json_matches_fixture_over_local_and_ssh() {
    let env = Env::new();
    let daemon = Daemon::start_with(env.path(), Some(fake_docker()));
    env.add("here", "local", Some(&daemon.socket));
    env.add("lab", "ssh://tester@lab.example", Some(&daemon.socket));

    let local = env.run(&["host", "-o", "json"]);
    assert!(local.status.success(), "{}", show(&local));
    let fixture: serde_json::Value = serde_json::from_str(HOST_FIXTURE).unwrap();
    assert_eq!(normalised_host(&local.stdout), fixture);
    let text = String::from_utf8(local.stdout.clone()).unwrap();
    assert!(
        text.starts_with("{\n  \"api_version\": \"v1\",\n"),
        "{text}"
    );
    assert!(text.ends_with("}\n"), "{text}");

    let out = env.run(&["target", "use", "lab"]);
    assert!(out.status.success(), "{}", show(&out));
    let ssh = env.run(&["host", "-o", "json"]);
    assert!(ssh.status.success(), "{}", show(&ssh));
    // Free disk is read live on every request, so only that line may differ
    // between the two answers; every other byte is the same.
    assert_eq!(normalised_host(&ssh.stdout), fixture);
    assert_eq!(
        without_disk_free(&ssh.stdout),
        without_disk_free(&local.stdout)
    );
    assert!(env.recorded_argv().contains(&"dial-stdio".to_owned()));
}

#[test]
fn host_text_with_docker_ready() {
    let env = Env::new();
    let daemon = Daemon::start_with(env.path(), Some(fake_docker()));
    env.add("here", "local", Some(&daemon.socket));
    let out = env.run(&["host"]);
    assert!(out.status.success(), "{}", show(&out));
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "Docker:   ready", "{text}");
    assert!(
        lines[1].starts_with("Engine:   28.3.2 (supported "),
        "{text}"
    );
    assert!(
        lines[2].starts_with("Compose:  2.39.1 (supported "),
        "{text}"
    );
    assert!(!text.contains("Message:"), "{text}");
    let daemon_line = format!("Daemon:   ship {}, API v1", env!("CARGO_PKG_VERSION"));
    assert!(text.contains(&daemon_line), "{text}");
}

#[test]
fn host_with_docker_unreachable_reports_it_and_exits_zero() {
    let env = Env::new();
    let daemon = Daemon::start_with(env.path(), Some(unreachable_docker()));
    env.add("here", "local", Some(&daemon.socket));

    let out = env.run(&["host", "-o", "json"]);
    assert_eq!(out.status.code(), Some(0), "{}", show(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["docker"]["status"], "unreachable", "{v}");
    let message = v["docker"]["message"].as_str().unwrap().to_owned();

    let out = env.run(&["host"]);
    assert_eq!(out.status.code(), Some(0), "{}", show(&out));
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "Docker:   unreachable", "{text}");
    assert_eq!(lines[1], format!("Message:  {message}"), "{text}");
    assert!(
        lines[2].starts_with("Engine:   unknown (supported "),
        "{text}"
    );

    // The daemon is still serving afterwards.
    let out = env.run(&["projects", "-o", "json"]);
    assert!(out.status.success(), "{}", show(&out));
}

#[test]
fn host_without_a_daemon_fails_and_says_how_to_start_one() {
    let env = Env::new();
    let socket = env.path().join("absent.sock");
    env.add("here", "local", Some(&socket));
    let out = env.run(&["host"]);
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    assert!(out.stdout.is_empty(), "{}", show(&out));
    let err = stderr(&out);
    assert!(err.contains("systemctl start ship"), "{err}");
}

#[test]
fn host_help_states_the_exit_status() {
    let out = Env::new().run(&["host", "--help"]);
    assert!(out.status.success(), "{}", show(&out));
    let help = String::from_utf8(out.stdout).unwrap();
    assert!(
        help.contains("0 when the daemon answered, whatever Docker's status"),
        "{help}"
    );
}
