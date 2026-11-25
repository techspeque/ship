//! `ship service install` and `ship service uninstall`: run the daemon
//! under systemd as a dedicated `ship` user (OVERVIEW §3.1, §8).
//!
//! Install creates the `ship` group and user, puts the user in the `docker`
//! group, creates the state directory (0700) and the runtime socket
//! directory (0750), and writes and enables the unit. Every step is skipped
//! when it is already satisfied, so a second run changes nothing. Uninstall
//! removes the unit and nothing else. Neither touches a unit that does not
//! carry [`MARKER`].
//!
//! Host facts and changes go through [`Host`], with structured argv and no
//! shell, so tests drive both commands against a fake host and a temporary
//! [`Layout`].

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ship_daemon::{DEFAULT_SOCKET_PATH, DEFAULT_STATE_DIR};

/// Name of the user and group the daemon runs as.
pub const SHIP: &str = "ship";
/// Name of the group that grants access to the Docker socket.
pub const DOCKER_GROUP: &str = "docker";
/// systemd unit name.
pub const UNIT_NAME: &str = "ship.service";
/// Where install writes the unit.
pub const UNIT_PATH: &str = "/etc/systemd/system/ship.service";
/// The runtime socket directory: the parent of the default socket path.
pub const RUNTIME_DIR: &str = "/run/ship";
/// First line of every unit Ship writes; a unit without it is never
/// overwritten or removed.
pub const MARKER: &str = "# Written by ship service install. ship service uninstall removes it.";

const STATE_DIR_MODE: u32 = 0o700;
const RUNTIME_DIR_MODE: u32 = 0o750;
const UNIT_MODE: u32 = 0o644;
const NOLOGIN_CANDIDATES: [&str; 3] = ["/usr/sbin/nologin", "/sbin/nologin", "/bin/false"];

/// The root-equivalence warning install always prints.
pub const DOCKER_WARNING: &str = "The ship user is in the docker group, which is root-equivalent \
     on this host. Membership of the ship group grants the same: whoever can use the daemon's \
     socket can run any container.";

/// Files and directories install and uninstall touch. Production uses
/// [`Layout::system`]; tests relocate them. The unit text and the argv
/// always name the production paths.
#[derive(Debug, Clone)]
pub struct Layout {
    pub unit_path: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Layout {
    pub fn system() -> Self {
        Self {
            unit_path: PathBuf::from(UNIT_PATH),
            state_dir: PathBuf::from(DEFAULT_STATE_DIR),
            runtime_dir: PathBuf::from(RUNTIME_DIR),
        }
    }
}

/// Result of one command run by a [`Host`].
#[derive(Debug, Clone, Default)]
pub struct CmdOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs host commands with structured argv; never through a shell.
pub trait Host {
    fn run(&mut self, argv: &[&str]) -> io::Result<CmdOutput>;
    /// Whether `path` exists (for picking the nologin shell).
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

/// The real host: `std::process::Command`, stdin closed.
pub struct SystemHost;

impl Host for SystemHost {
    fn run(&mut self, argv: &[&str]) -> io::Result<CmdOutput> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
        let out = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        Ok(CmdOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

#[derive(Debug)]
pub enum ServiceError {
    /// A precondition failed; nothing was changed.
    Refused(String),
    /// A command could not run or failed.
    Command { argv: String, detail: String },
    /// A file-system operation failed.
    Io {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(m) => f.write_str(m),
            Self::Command { argv, detail } => write!(f, "`{argv}` failed: {detail}"),
            Self::Io { what, path, source } => {
                write!(f, "cannot {what} {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ServiceError {}

fn io_err<'a>(what: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> ServiceError + 'a {
    move |source| ServiceError::Io {
        what,
        path: path.to_path_buf(),
        source,
    }
}

/// What a step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Created,
    Updated,
    Unchanged,
}

impl Change {
    fn word(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Unchanged => "ok",
        }
    }
}

/// Summary of an install, in step order.
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub steps: Vec<(String, Change)>,
    pub unit_changed: bool,
    /// Set when the binary looks unreachable for the `ship` user.
    pub warnings: Vec<String>,
}

impl InstallReport {
    pub fn changed_anything(&self) -> bool {
        self.steps.iter().any(|(_, c)| *c != Change::Unchanged)
    }
}

/// Summary of an uninstall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallReport {
    Removed,
    NothingToRemove,
}

/// Runs a query: `None` when the command exits non-zero (e.g. `getent` on a
/// missing name).
fn query(host: &mut dyn Host, argv: &[&str]) -> Result<Option<String>, ServiceError> {
    let out = host.run(argv).map_err(|e| ServiceError::Command {
        argv: argv.join(" "),
        detail: e.to_string(),
    })?;
    Ok(out.success.then_some(out.stdout))
}

/// Runs a change; a non-zero exit is an error carrying the command's stderr.
fn change(host: &mut dyn Host, argv: &[&str]) -> Result<(), ServiceError> {
    let out = host.run(argv).map_err(|e| ServiceError::Command {
        argv: argv.join(" "),
        detail: e.to_string(),
    })?;
    if out.success {
        Ok(())
    } else {
        let stderr = out.stderr.trim();
        Err(ServiceError::Command {
            argv: argv.join(" "),
            detail: if stderr.is_empty() {
                "exited non-zero".to_owned()
            } else {
                stderr.to_owned()
            },
        })
    }
}

fn group_exists(host: &mut dyn Host, name: &str) -> Result<bool, ServiceError> {
    Ok(query(host, &["getent", "group", name])?.is_some())
}

/// `(uid, gid)` of the `ship` user, if it exists.
fn ship_user(host: &mut dyn Host) -> Result<Option<(u32, u32)>, ServiceError> {
    let Some(line) = query(host, &["getent", "passwd", SHIP])? else {
        return Ok(None);
    };
    let fields: Vec<&str> = line.trim().split(':').collect();
    let parse = |i: usize| fields.get(i).and_then(|f| f.parse::<u32>().ok());
    match (parse(2), parse(3)) {
        (Some(uid), Some(gid)) => Ok(Some((uid, gid))),
        _ => Err(ServiceError::Command {
            argv: format!("getent passwd {SHIP}"),
            detail: format!("unexpected output {:?}", line.trim()),
        }),
    }
}

fn in_docker_group(host: &mut dyn Host) -> Result<bool, ServiceError> {
    Ok(query(host, &["id", "-nG", SHIP])?
        .is_some_and(|groups| groups.split_whitespace().any(|g| g == DOCKER_GROUP)))
}

fn nologin_shell(host: &dyn Host) -> &'static str {
    NOLOGIN_CANDIDATES
        .iter()
        .copied()
        .find(|p| host.exists(Path::new(p)))
        .unwrap_or("/bin/false")
}

/// The existing unit file's contents: `None` if absent. Anything at the
/// path that is not a regular file is refused.
fn read_unit(path: &Path) -> Result<Option<String>, ServiceError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => fs::read_to_string(path)
            .map(Some)
            .map_err(io_err("read", path)),
        Ok(_) => Err(ServiceError::Refused(format!(
            "{} exists and is not a regular file; not touching it",
            path.display()
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err("stat", path)(e)),
    }
}

fn is_ours(contents: &str) -> bool {
    contents.lines().next() == Some(MARKER)
}

fn foreign_unit(path: &Path) -> ServiceError {
    ServiceError::Refused(format!(
        "{} was not written by ship service install; not touching it. \
         Move it aside first if Ship should manage it.",
        path.display()
    ))
}

/// `exe` is written unquoted into `ExecStart=`; only characters systemd
/// takes literally are accepted.
pub fn check_exe_path(exe: &Path) -> Result<&str, ServiceError> {
    let s = exe.to_str().unwrap_or("");
    let ok = s.starts_with('/')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._+-".contains(&b));
    if ok {
        Ok(s)
    } else {
        Err(ServiceError::Refused(format!(
            "the ship binary is at {}, which cannot be written into a systemd unit as is; \
             install it at a plain path such as /usr/local/bin/ship and run that",
            exe.display()
        )))
    }
}

/// A warning when `exe` or one of its directories is not executable by
/// others, so the `ship` user could not start it.
fn exe_reachability_warning(exe: &Path) -> Option<String> {
    let unreachable = exe.ancestors().any(|p| {
        fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o001 == 0)
            .unwrap_or(true)
    });
    unreachable.then(|| {
        format!(
            "{} (or a directory above it) is not executable by other users, so the ship \
             user may not be able to run it; install it at /usr/local/bin/ship",
            exe.display()
        )
    })
}

/// The unit text for the binary at `exe`.
pub fn render_unit(exe: &str) -> String {
    format!(
        "{MARKER}
[Unit]
Description=Ship daemon
After=network-online.target docker.service
Wants=docker.service

[Service]
Type=simple
User={SHIP}
Group={SHIP}
ExecStart={exe} daemon --state {DEFAULT_STATE_DIR} --socket {DEFAULT_SOCKET_PATH}
RuntimeDirectory=ship
RuntimeDirectoryMode=0750
UMask=0077
Restart=always
RestartSec=2
TimeoutStopSec=30
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=full

[Install]
WantedBy=multi-user.target
"
    )
}

/// Creates `path` with `mode` owned by `uid:gid`, or corrects an existing
/// directory's mode and owner. A non-directory at `path` is refused.
fn ensure_dir(path: &Path, mode: u32, uid: u32, gid: u32) -> Result<Change, ServiceError> {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => {
            if meta.mode() & 0o7777 == mode && meta.uid() == uid && meta.gid() == gid {
                return Ok(Change::Unchanged);
            }
            Change::Updated
        }
        Ok(_) => {
            return Err(ServiceError::Refused(format!(
                "{} exists and is not a directory; not touching it",
                path.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(mode)
                .create(path)
                .map_err(io_err("create", path))?;
            Change::Created
        }
        Err(e) => return Err(io_err("stat", path)(e)),
    };
    std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(io_err("chown", path))?;
    // After chown, which may clear set-id bits; and the umask narrowed create.
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(io_err("set mode of", path))?;
    Ok(result)
}

/// Writes `contents` to `path` atomically: a new 0644 file beside it,
/// fsynced, then renamed over.
fn write_atomic(path: &Path, contents: &str) -> Result<(), ServiceError> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    let tmp = dir.join(format!(".{UNIT_NAME}.ship-tmp"));
    let _ = fs::remove_file(&tmp);
    let write = || -> io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(UNIT_MODE)
            .open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(UNIT_MODE))?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_err("write", path)(e)
    })
}

/// Installs the service for the binary at `exe` (already canonical).
/// Checks every precondition before the first change.
pub fn install(
    host: &mut dyn Host,
    layout: &Layout,
    exe: &Path,
) -> Result<InstallReport, ServiceError> {
    // Preflight: nothing below changes the host until all of these pass.
    let existing = read_unit(&layout.unit_path)?;
    if let Some(contents) = &existing
        && !is_ours(contents)
    {
        return Err(foreign_unit(&layout.unit_path));
    }
    let exe_str = check_exe_path(exe)?;
    if !group_exists(host, DOCKER_GROUP)? {
        return Err(ServiceError::Refused(
            "there is no docker group on this host; install Docker Engine first".to_owned(),
        ));
    }
    let mut warnings = Vec::new();
    warnings.extend(exe_reachability_warning(exe));

    let mut steps = Vec::new();

    // 1. group
    if group_exists(host, SHIP)? {
        steps.push((format!("group {SHIP}"), Change::Unchanged));
    } else {
        change(host, &["groupadd", "--system", SHIP])?;
        steps.push((format!("group {SHIP}"), Change::Created));
    }

    // 2. user: no password is set, so the account is locked.
    let (uid, gid) = match ship_user(host)? {
        Some(ids) => {
            steps.push((format!("user {SHIP}"), Change::Unchanged));
            ids
        }
        None => {
            let shell = nologin_shell(host);
            change(
                host,
                &[
                    "useradd",
                    "--system",
                    "--gid",
                    SHIP,
                    "--home-dir",
                    DEFAULT_STATE_DIR,
                    "--no-create-home",
                    "--shell",
                    shell,
                    SHIP,
                ],
            )?;
            steps.push((format!("user {SHIP}"), Change::Created));
            ship_user(host)?.ok_or_else(|| ServiceError::Command {
                argv: format!("getent passwd {SHIP}"),
                detail: "user missing right after useradd".to_owned(),
            })?
        }
    };

    // 3. Docker access.
    if in_docker_group(host)? {
        steps.push((format!("{SHIP} in group {DOCKER_GROUP}"), Change::Unchanged));
    } else {
        change(host, &["usermod", "-aG", DOCKER_GROUP, SHIP])?;
        steps.push((format!("{SHIP} in group {DOCKER_GROUP}"), Change::Created));
    }

    // 4–5. Directories.
    let c = ensure_dir(&layout.state_dir, STATE_DIR_MODE, uid, gid)?;
    steps.push((format!("state directory {DEFAULT_STATE_DIR} (0700)"), c));
    let c = ensure_dir(&layout.runtime_dir, RUNTIME_DIR_MODE, uid, gid)?;
    steps.push((format!("runtime directory {RUNTIME_DIR} (0750)"), c));

    // 6. Unit.
    let unit = render_unit(exe_str);
    let unit_change = match &existing {
        Some(contents) if *contents == unit => Change::Unchanged,
        Some(_) => Change::Updated,
        None => Change::Created,
    };
    if unit_change != Change::Unchanged {
        write_atomic(&layout.unit_path, &unit)?;
        change(host, &["systemctl", "daemon-reload"])?;
    }
    steps.push((format!("unit {UNIT_PATH}"), unit_change));

    // 7. Enabled at boot.
    let enabled = query(host, &["systemctl", "is-enabled", UNIT_NAME])?
        .is_some_and(|s| s.trim() == "enabled");
    if enabled {
        steps.push((format!("{UNIT_NAME} enabled at boot"), Change::Unchanged));
    } else {
        change(host, &["systemctl", "enable", UNIT_NAME])?;
        steps.push((format!("{UNIT_NAME} enabled at boot"), Change::Created));
    }

    Ok(InstallReport {
        steps,
        unit_changed: unit_change == Change::Updated,
        warnings,
    })
}

/// Stops, disables and removes the unit Ship wrote. Leaves the user, the
/// group, the state directory and every Docker object in place.
pub fn uninstall(host: &mut dyn Host, layout: &Layout) -> Result<UninstallReport, ServiceError> {
    match read_unit(&layout.unit_path)? {
        None => return Ok(UninstallReport::NothingToRemove),
        Some(contents) if !is_ours(&contents) => return Err(foreign_unit(&layout.unit_path)),
        Some(_) => {}
    }
    change(host, &["systemctl", "disable", "--now", UNIT_NAME])?;
    fs::remove_file(&layout.unit_path).map_err(io_err("remove", &layout.unit_path))?;
    change(host, &["systemctl", "daemon-reload"])?;
    Ok(UninstallReport::Removed)
}

/// The refusal printed on a non-Linux OS, or `None` on Linux.
pub fn unsupported(os: &str) -> Option<String> {
    (os != "linux").then(|| {
        format!(
            "ship service manages a systemd unit and runs on Linux only (ADR-0002); \
             this client runs on {os}"
        )
    })
}

/// The refusal printed when not root, or `None` as root.
pub fn not_root(euid: u32, subcommand: &str) -> Option<String> {
    (euid != 0).then(|| {
        format!("ship service {subcommand} must run as root: sudo ship service {subcommand}")
    })
}

pub fn print_install(report: &InstallReport) {
    for (what, c) in &report.steps {
        println!("{what}: {}", c.word());
    }
    println!();
    println!("{DOCKER_WARNING}");
    for w in &report.warnings {
        println!("warning: {w}");
    }
    println!();
    if !report.changed_anything() {
        println!("Nothing changed.");
    }
    println!("Start the daemon:  sudo systemctl start ship");
    if report.unit_changed {
        println!("The unit changed; if the daemon is running:  sudo systemctl restart ship");
    }
    println!(
        "Let a login user use it (local and ssh targets):  sudo usermod -aG ship USER, \
         then log in again. See docs/service.md."
    );
}

pub fn print_uninstall(report: UninstallReport) {
    match report {
        UninstallReport::NothingToRemove => {
            println!("No unit at {UNIT_PATH}; nothing to remove.");
        }
        UninstallReport::Removed => {
            println!("Stopped and removed {UNIT_PATH}.");
            println!(
                "Left in place: the {SHIP} user and group, {DEFAULT_STATE_DIR}, and every \
                 container and volume. Apps keep running under Docker."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// A fake host that tracks group/user/enable state and records argv.
    struct FakeHost {
        groups: BTreeSet<String>,
        user: Option<(u32, u32)>,
        in_docker: bool,
        enabled: bool,
        calls: Vec<Vec<String>>,
        fail: Option<&'static str>,
    }

    impl FakeHost {
        fn clean() -> Self {
            Self {
                groups: [DOCKER_GROUP.to_owned()].into(),
                user: None,
                in_docker: false,
                enabled: false,
                calls: Vec::new(),
                fail: None,
            }
        }

        fn mutating_calls(&self) -> Vec<String> {
            self.calls
                .iter()
                .filter(|c| !matches!(c[0].as_str(), "getent" | "id"))
                .filter(|c| !(c[0] == "systemctl" && c[1] == "is-enabled"))
                .map(|c| c.join(" "))
                .collect()
        }
    }

    fn me() -> (u32, u32) {
        // SAFETY: plain getters with no preconditions.
        unsafe { (libc::geteuid(), libc::getegid()) }
    }

    impl Host for FakeHost {
        fn run(&mut self, argv: &[&str]) -> io::Result<CmdOutput> {
            self.calls
                .push(argv.iter().map(|s| (*s).to_owned()).collect());
            let ok = |stdout: String| {
                Ok(CmdOutput {
                    success: true,
                    stdout,
                    stderr: String::new(),
                })
            };
            let no = || Ok(CmdOutput::default());
            if self.fail == Some(argv[0]) {
                return Ok(CmdOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "boom".to_owned(),
                });
            }
            match argv {
                ["getent", "group", g] => {
                    if self.groups.contains(*g) {
                        ok(format!("{g}:x:999:\n"))
                    } else {
                        no()
                    }
                }
                ["getent", "passwd", "ship"] => match self.user {
                    Some((u, g)) => ok(format!("ship:x:{u}:{g}::/var/lib/ship:/bin/false\n")),
                    None => no(),
                },
                ["id", "-nG", "ship"] => match self.user {
                    Some(_) if self.in_docker => ok("ship docker\n".to_owned()),
                    Some(_) => ok("ship\n".to_owned()),
                    None => no(),
                },
                ["groupadd", "--system", g] => {
                    self.groups.insert((*g).to_owned());
                    ok(String::new())
                }
                ["useradd", ..] => {
                    self.user = Some(me());
                    ok(String::new())
                }
                ["usermod", "-aG", "docker", "ship"] => {
                    self.in_docker = true;
                    ok(String::new())
                }
                ["systemctl", "daemon-reload"] => ok(String::new()),
                ["systemctl", "is-enabled", _] => {
                    if self.enabled {
                        ok("enabled\n".to_owned())
                    } else {
                        Ok(CmdOutput {
                            success: false,
                            stdout: "disabled\n".to_owned(),
                            stderr: String::new(),
                        })
                    }
                }
                ["systemctl", "enable", _] => {
                    self.enabled = true;
                    ok(String::new())
                }
                ["systemctl", "disable", "--now", _] => {
                    self.enabled = false;
                    ok(String::new())
                }
                other => panic!("unexpected command {other:?}"),
            }
        }

        fn exists(&self, path: &Path) -> bool {
            path == Path::new("/usr/sbin/nologin")
        }
    }

    struct Env {
        _dir: tempfile::TempDir,
        layout: Layout,
        exe: PathBuf,
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let systemd = root.join("etc-systemd");
        fs::create_dir(&systemd).unwrap();
        let exe = root.join("ship-bin");
        fs::write(&exe, b"#!/bin/false\n").unwrap();
        Env {
            layout: Layout {
                unit_path: systemd.join(UNIT_NAME),
                state_dir: root.join("var-lib-ship"),
                runtime_dir: root.join("run-ship"),
            },
            exe,
            _dir: dir,
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    #[test]
    fn clean_install_issues_exactly_the_expected_changes() {
        let e = env();
        let mut host = FakeHost::clean();
        let report = install(&mut host, &e.layout, &e.exe).unwrap();
        assert_eq!(
            host.mutating_calls(),
            vec![
                "groupadd --system ship".to_owned(),
                "useradd --system --gid ship --home-dir /var/lib/ship --no-create-home \
                 --shell /usr/sbin/nologin ship"
                    .to_owned(),
                "usermod -aG docker ship".to_owned(),
                "systemctl daemon-reload".to_owned(),
                "systemctl enable ship.service".to_owned(),
            ]
        );
        assert!(
            host.calls.iter().all(
                |c| !c.iter().any(|a| a == "-p" || a.starts_with("--password"))
                    && c[0] != "passwd"
                    && c[0] != "chpasswd"
            ),
            "no password option: {:?}",
            host.calls
        );
        assert_eq!(mode(&e.layout.state_dir), 0o700);
        assert_eq!(mode(&e.layout.runtime_dir), 0o750);
        assert_eq!(mode(&e.layout.unit_path), 0o644);
        let unit = fs::read_to_string(&e.layout.unit_path).unwrap();
        assert_eq!(unit.lines().next(), Some(MARKER));
        assert_eq!(unit, render_unit(e.exe.to_str().unwrap()));
        assert!(report.changed_anything());
        assert!(!report.unit_changed);
    }

    #[test]
    fn second_install_changes_nothing() {
        let e = env();
        let mut host = FakeHost::clean();
        install(&mut host, &e.layout, &e.exe).unwrap();
        let before = fs::metadata(&e.layout.unit_path).unwrap();
        let bytes = fs::read(&e.layout.unit_path).unwrap();
        host.calls.clear();

        let report = install(&mut host, &e.layout, &e.exe).unwrap();
        assert_eq!(host.mutating_calls(), Vec::<String>::new());
        assert!(!report.changed_anything(), "{:?}", report.steps);
        let after = fs::metadata(&e.layout.unit_path).unwrap();
        assert_eq!(fs::read(&e.layout.unit_path).unwrap(), bytes);
        assert_eq!(before.ino(), after.ino());
        assert_eq!(before.mtime(), after.mtime());
        assert_eq!(before.mtime_nsec(), after.mtime_nsec());
    }

    #[test]
    fn install_rewrites_its_own_unit_when_the_binary_moved() {
        let e = env();
        let mut host = FakeHost::clean();
        install(&mut host, &e.layout, &e.exe).unwrap();
        let moved = e.exe.with_file_name("ship-new");
        fs::write(&moved, b"").unwrap();
        host.calls.clear();
        let report = install(&mut host, &e.layout, &moved).unwrap();
        assert!(report.unit_changed);
        assert_eq!(host.mutating_calls(), vec!["systemctl daemon-reload"]);
        let unit = fs::read_to_string(&e.layout.unit_path).unwrap();
        assert!(unit.contains(&format!("ExecStart={} daemon", moved.display())));
    }

    #[test]
    fn install_corrects_a_wrong_state_dir_mode() {
        let e = env();
        fs::create_dir(&e.layout.state_dir).unwrap();
        fs::set_permissions(&e.layout.state_dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(e.layout.state_dir.join("ship.db"), b"keep").unwrap();
        let mut host = FakeHost::clean();
        let report = install(&mut host, &e.layout, &e.exe).unwrap();
        assert_eq!(mode(&e.layout.state_dir), 0o700);
        assert!(
            report
                .steps
                .iter()
                .any(|(w, c)| w.starts_with("state directory") && *c == Change::Updated)
        );
        assert_eq!(
            fs::read(e.layout.state_dir.join("ship.db")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn foreign_unit_is_refused_by_install_and_uninstall() {
        let e = env();
        let foreign = "[Unit]\nDescription=someone else's\n";
        fs::write(&e.layout.unit_path, foreign).unwrap();
        let before = fs::metadata(&e.layout.unit_path).unwrap();

        let mut host = FakeHost::clean();
        let err = install(&mut host, &e.layout, &e.exe).unwrap_err();
        assert!(matches!(err, ServiceError::Refused(_)), "{err}");
        assert!(err.to_string().contains("not written by ship"), "{err}");
        assert!(host.calls.is_empty(), "{:?}", host.calls);
        assert!(!e.layout.state_dir.exists());

        let err = uninstall(&mut host, &e.layout).unwrap_err();
        assert!(err.to_string().contains("not written by ship"), "{err}");
        assert!(host.calls.is_empty(), "{:?}", host.calls);

        let after = fs::metadata(&e.layout.unit_path).unwrap();
        assert_eq!(fs::read_to_string(&e.layout.unit_path).unwrap(), foreign);
        assert_eq!(before.mtime_nsec(), after.mtime_nsec());
    }

    #[test]
    fn missing_docker_group_is_refused_before_any_change() {
        let e = env();
        let mut host = FakeHost::clean();
        host.groups.clear();
        let err = install(&mut host, &e.layout, &e.exe).unwrap_err();
        assert!(
            err.to_string().contains("install Docker Engine first"),
            "{err}"
        );
        assert_eq!(host.mutating_calls(), Vec::<String>::new());
        assert!(!e.layout.state_dir.exists());
        assert!(!e.layout.runtime_dir.exists());
        assert!(!e.layout.unit_path.exists());
    }

    #[test]
    fn a_failing_command_is_reported_with_its_stderr() {
        let e = env();
        let mut host = FakeHost::clean();
        host.fail = Some("groupadd");
        let err = install(&mut host, &e.layout, &e.exe).unwrap_err();
        assert_eq!(err.to_string(), "`groupadd --system ship` failed: boom");
    }

    #[test]
    fn uninstall_removes_only_the_unit_and_is_idempotent() {
        let e = env();
        let mut host = FakeHost::clean();
        install(&mut host, &e.layout, &e.exe).unwrap();
        fs::write(e.layout.state_dir.join("ship.db"), b"keep").unwrap();
        host.calls.clear();

        assert_eq!(
            uninstall(&mut host, &e.layout).unwrap(),
            UninstallReport::Removed
        );
        assert_eq!(
            host.mutating_calls(),
            vec![
                "systemctl disable --now ship.service",
                "systemctl daemon-reload"
            ]
        );
        assert!(!e.layout.unit_path.exists());
        assert_eq!(
            fs::read(e.layout.state_dir.join("ship.db")).unwrap(),
            b"keep"
        );
        assert_eq!(mode(&e.layout.state_dir), 0o700);
        assert!(host.user.is_some() && host.groups.contains(SHIP));

        host.calls.clear();
        assert_eq!(
            uninstall(&mut host, &e.layout).unwrap(),
            UninstallReport::NothingToRemove
        );
        assert!(host.calls.is_empty());
    }

    #[test]
    fn unit_runs_the_daemon_as_ship_and_restarts_it() {
        let unit = render_unit("/usr/local/bin/ship");
        for line in [
            MARKER,
            "User=ship",
            "Group=ship",
            "ExecStart=/usr/local/bin/ship daemon --state /var/lib/ship --socket /run/ship/ship.sock",
            "RuntimeDirectory=ship",
            "RuntimeDirectoryMode=0750",
            "Restart=always",
            "WantedBy=multi-user.target",
        ] {
            assert!(
                unit.lines().any(|l| l == line),
                "missing {line:?} in\n{unit}"
            );
        }
        assert!(
            !unit.contains("Requires="),
            "Docker down must not stop the daemon"
        );
    }

    #[test]
    fn exe_path_must_be_plain() {
        assert!(check_exe_path(Path::new("/usr/local/bin/ship")).is_ok());
        assert!(check_exe_path(Path::new("/opt/ship-1.0_x86+64/ship")).is_ok());
        for bad in [
            "/opt/my ship/ship",
            "/opt/50%/ship",
            "relative/ship",
            "/a;b",
            "/a\nb",
        ] {
            assert!(check_exe_path(Path::new(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn exe_path_with_a_space_is_refused_before_any_change() {
        let e = env();
        let spaced = e.exe.with_file_name("my ship");
        let mut host = FakeHost::clean();
        assert!(install(&mut host, &e.layout, &spaced).is_err());
        assert!(host.calls.is_empty());
    }

    #[test]
    fn nologin_falls_back() {
        struct NoShells;
        impl Host for NoShells {
            fn run(&mut self, _: &[&str]) -> io::Result<CmdOutput> {
                unreachable!()
            }
            fn exists(&self, _: &Path) -> bool {
                false
            }
        }
        assert_eq!(nologin_shell(&NoShells), "/bin/false");
        assert_eq!(nologin_shell(&FakeHost::clean()), "/usr/sbin/nologin");
    }

    #[test]
    fn refusals() {
        assert_eq!(unsupported("linux"), None);
        assert!(unsupported("macos").unwrap().contains("Linux only"));
        assert_eq!(not_root(0, "install"), None);
        assert!(
            not_root(1000, "install")
                .unwrap()
                .contains("sudo ship service install")
        );
    }
}
