//! Every argument vector the adapter runs (ADR-0001 rule 2).
//!
//! An [`Argv`] can be made only by the builders here, each from typed
//! inputs, and every one passes [`Argv::checked`]: no token that removes a
//! volume or starts a shell may appear among the flags and subcommands, and
//! every free-form value (a service name, an object id) sits after `--`,
//! where Docker reads it as a name, never as a flag. Nothing outside this
//! crate can run Docker with an argument vector of its own.
//!
//! There is deliberately no builder for `down -v`, `--volumes`, `volume rm`,
//! `volume prune`, `exec` or `run`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use ship_core::{EnvironmentId, compose_project_name};

use crate::error::DockerError;
use crate::labels::{COMPOSE_PROJECT_LABEL, Selector};

/// Tokens refused anywhere before `--`. Some never occur in a builder at
/// all; listing them makes a future builder that adds one fail its tests.
pub const DENIED: &[&str] = &[
    "-v",
    "--volumes",
    "--rmi",
    "volume",
    "volumes",
    "prune",
    "rm",
    "exec",
    "run",
    "sh",
    "bash",
    "-c",
    "--privileged",
    "system",
];

/// Longest service name passed to Compose.
pub const MAX_SERVICE_NAME: usize = 128;

/// An argument vector for the `docker` CLI that the adapter built and
/// checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argv {
    command: &'static str,
    args: Vec<OsString>,
}

impl Argv {
    /// The only constructor: refuses any [`DENIED`] token before `--`, and
    /// any value after `--` that starts with `-`.
    fn checked(command: &'static str, args: Vec<OsString>) -> Result<Argv, DockerError> {
        let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
        if let Some(bad) = args[..split]
            .iter()
            .find(|a| DENIED.iter().any(|d| OsStr::new(d) == a.as_os_str()))
        {
            return Err(DockerError::InvalidArgument(format!(
                "`{}` is never passed to docker",
                bad.to_string_lossy()
            )));
        }
        if let Some(bad) = args[split..]
            .iter()
            .skip(1)
            .find(|a| a.as_encoded_bytes().first() == Some(&b'-'))
        {
            return Err(DockerError::InvalidArgument(format!(
                "`{}` looks like a flag",
                bad.to_string_lossy()
            )));
        }
        Ok(Argv { command, args })
    }

    /// What this runs, for messages: `compose stop`, `container inspect`.
    pub fn command(&self) -> &'static str {
        self.command
    }

    /// The arguments, without the program.
    pub fn args(&self) -> &[OsString] {
        &self.args
    }
}

/// One environment's Compose project. The name derives from the
/// environment id alone (ADR-0001 rule 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeProject {
    environment: EnvironmentId,
    name: String,
}

impl ComposeProject {
    pub fn new(environment: EnvironmentId) -> Self {
        ComposeProject {
            environment,
            name: compose_project_name(&environment),
        }
    }

    pub fn environment(&self) -> EnvironmentId {
        self.environment
    }

    /// `ship_<environment id>`.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// The files a Compose project is defined by: the project directory and
/// one or more Compose files, all absolute paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeFiles {
    directory: PathBuf,
    files: Vec<PathBuf>,
}

impl ComposeFiles {
    /// `directory` is the project directory (relative paths in the files
    /// resolve against it); `files` are merged in order, later ones
    /// overriding earlier ones.
    pub fn new(
        directory: impl Into<PathBuf>,
        files: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, DockerError> {
        let directory = directory.into();
        let files: Vec<PathBuf> = files.into_iter().collect();
        if files.is_empty() {
            return Err(DockerError::InvalidArgument(
                "a Compose project needs at least one file".to_owned(),
            ));
        }
        for path in std::iter::once(&directory).chain(&files) {
            if !path.is_absolute() {
                return Err(DockerError::InvalidArgument(format!(
                    "`{}` is not an absolute path",
                    path.display()
                )));
            }
        }
        Ok(ComposeFiles { directory, files })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }
}

/// Options for `compose up`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpOptions {
    /// Also remove the project's containers whose service the files no
    /// longer define. Never removes a volume.
    pub remove_orphans: bool,
}

/// Options for `compose logs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogOptions {
    /// Keep streaming new lines until stopped.
    pub follow: bool,
    /// Only this many lines from the end of each container's log.
    pub tail: Option<u32>,
}

/// `docker compose --ansi never --project-name <name>`.
fn compose(project: &ComposeProject) -> Vec<OsString> {
    vec![
        "compose".into(),
        "--ansi".into(),
        "never".into(),
        "--project-name".into(),
        project.name.as_str().into(),
    ]
}

/// [`compose`] plus the project directory and files.
fn compose_with_files(project: &ComposeProject, files: &ComposeFiles) -> Vec<OsString> {
    let mut args = compose(project);
    args.push("--project-directory".into());
    args.push(files.directory.clone().into());
    for file in &files.files {
        args.push("--file".into());
        args.push(file.clone().into());
    }
    args
}

/// Whether `name` is a Compose service name: `[a-zA-Z0-9][a-zA-Z0-9_.-]*`.
pub fn is_service_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_SERVICE_NAME
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

/// Appends `--` and `services`, each checked. Nothing for no services
/// (which means every service).
fn push_services<S: AsRef<str>>(
    args: &mut Vec<OsString>,
    services: &[S],
) -> Result<(), DockerError> {
    if services.is_empty() {
        return Ok(());
    }
    args.push("--".into());
    for s in services {
        let s = s.as_ref();
        if !is_service_name(s) {
            return Err(DockerError::InvalidArgument(format!(
                "`{s}` is not a Compose service name"
            )));
        }
        args.push(s.into());
    }
    Ok(())
}

/// Whether `id` is a Docker object id (12 to 64 lowercase hex digits).
pub fn is_object_id(id: &str) -> bool {
    (12..=64).contains(&id.len()) && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// Appends `--` and `ids`, each checked.
fn push_ids<S: AsRef<str>>(args: &mut Vec<OsString>, ids: &[S]) -> Result<(), DockerError> {
    if ids.is_empty() {
        return Err(DockerError::InvalidArgument("no object ids".to_owned()));
    }
    args.push("--".into());
    for id in ids {
        let id = id.as_ref();
        if !is_object_id(id) {
            return Err(DockerError::InvalidArgument(format!(
                "`{id}` is not a Docker object id"
            )));
        }
        args.push(id.into());
    }
    Ok(())
}

/// `compose … config --format json`: the canonical, interpolated model.
pub fn compose_config(project: &ComposeProject, files: &ComposeFiles) -> Result<Argv, DockerError> {
    let mut args = compose_with_files(project, files);
    args.extend(["config".into(), "--format".into(), "json".into()]);
    Argv::checked("compose config", args)
}

/// `compose … build [-- services]`.
pub fn compose_build<S: AsRef<str>>(
    project: &ComposeProject,
    files: &ComposeFiles,
    services: &[S],
) -> Result<Argv, DockerError> {
    let mut args = compose_with_files(project, files);
    args.push("build".into());
    push_services(&mut args, services)?;
    Argv::checked("compose build", args)
}

/// `compose … up --detach --no-build [--remove-orphans] [-- services]`.
/// Building is its own step ([`compose_build`]).
pub fn compose_up<S: AsRef<str>>(
    project: &ComposeProject,
    files: &ComposeFiles,
    services: &[S],
    options: UpOptions,
) -> Result<Argv, DockerError> {
    let mut args = compose_with_files(project, files);
    args.extend(["up".into(), "--detach".into(), "--no-build".into()]);
    if options.remove_orphans {
        args.push("--remove-orphans".into());
    }
    push_services(&mut args, services)?;
    Argv::checked("compose up", args)
}

/// `compose -p <name> stop [-- services]`.
pub fn compose_stop<S: AsRef<str>>(
    project: &ComposeProject,
    services: &[S],
) -> Result<Argv, DockerError> {
    let mut args = compose(project);
    args.push("stop".into());
    push_services(&mut args, services)?;
    Argv::checked("compose stop", args)
}

/// `compose -p <name> start [-- services]`.
pub fn compose_start<S: AsRef<str>>(
    project: &ComposeProject,
    services: &[S],
) -> Result<Argv, DockerError> {
    let mut args = compose(project);
    args.push("start".into());
    push_services(&mut args, services)?;
    Argv::checked("compose start", args)
}

/// `compose -p <name> restart [-- services]`.
pub fn compose_restart<S: AsRef<str>>(
    project: &ComposeProject,
    services: &[S],
) -> Result<Argv, DockerError> {
    let mut args = compose(project);
    args.push("restart".into());
    push_services(&mut args, services)?;
    Argv::checked("compose restart", args)
}

/// `compose -p <name> down`: removes the project's containers and
/// networks. Volumes, named or anonymous, and images stay; there is no
/// option to change that.
pub fn compose_down(project: &ComposeProject) -> Result<Argv, DockerError> {
    let mut args = compose(project);
    args.push("down".into());
    Argv::checked("compose down", args)
}

/// `compose -p <name> ps --all --quiet`: the id of every container in the
/// project, labelled by Ship or not.
pub fn compose_ps(project: &ComposeProject) -> Result<Argv, DockerError> {
    let mut args = compose(project);
    args.extend(["ps".into(), "--all".into(), "--quiet".into()]);
    Argv::checked("compose ps", args)
}

/// `compose -p <name> logs --no-color --timestamps [--follow] [--tail N]
/// [-- services]`.
pub fn compose_logs<S: AsRef<str>>(
    project: &ComposeProject,
    services: &[S],
    options: LogOptions,
) -> Result<Argv, DockerError> {
    let mut args = compose(project);
    args.extend(["logs".into(), "--no-color".into(), "--timestamps".into()]);
    if options.follow {
        args.push("--follow".into());
    }
    if let Some(n) = options.tail {
        args.push("--tail".into());
        args.push(n.to_string().into());
    }
    push_services(&mut args, services)?;
    Argv::checked("compose logs", args)
}

/// `ps --all --quiet --no-trunc --filter label=…`: containers carrying
/// Ship's labels for the selector.
pub fn container_ids(selector: &Selector) -> Result<Argv, DockerError> {
    let mut args: Vec<OsString> = vec![
        "ps".into(),
        "--all".into(),
        "--quiet".into(),
        "--no-trunc".into(),
    ];
    args.extend(selector.filters());
    Argv::checked("ps", args)
}

/// `ps --all --quiet --no-trunc --filter label=com.docker.compose.project=<name>`:
/// every container Compose counts as the project's, one-off or not,
/// labelled by Ship or not. This is the set `compose down` removes.
pub fn project_container_ids(project: &ComposeProject) -> Result<Argv, DockerError> {
    let args: Vec<OsString> = vec![
        "ps".into(),
        "--all".into(),
        "--quiet".into(),
        "--no-trunc".into(),
        "--filter".into(),
        format!("label={COMPOSE_PROJECT_LABEL}={}", project.name).into(),
    ];
    Argv::checked("ps", args)
}

/// `container inspect -- ids`.
pub fn container_inspect<S: AsRef<str>>(ids: &[S]) -> Result<Argv, DockerError> {
    let mut args: Vec<OsString> = vec!["container".into(), "inspect".into()];
    push_ids(&mut args, ids)?;
    Argv::checked("container inspect", args)
}

/// `network ls --quiet --no-trunc --filter label=com.docker.compose.project=<name>`:
/// every network in the Compose project, labelled by Ship or not.
pub fn project_network_ids(project: &ComposeProject) -> Result<Argv, DockerError> {
    let args: Vec<OsString> = vec![
        "network".into(),
        "ls".into(),
        "--quiet".into(),
        "--no-trunc".into(),
        "--filter".into(),
        format!("label={COMPOSE_PROJECT_LABEL}={}", project.name).into(),
    ];
    Argv::checked("network ls", args)
}

/// `network inspect -- ids`.
pub fn network_inspect<S: AsRef<str>>(ids: &[S]) -> Result<Argv, DockerError> {
    let mut args: Vec<OsString> = vec!["network".into(), "inspect".into()];
    push_ids(&mut args, ids)?;
    Argv::checked("network inspect", args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::tests::ids;

    fn strs(argv: &Argv) -> Vec<String> {
        argv.args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn project() -> ComposeProject {
        ComposeProject::new(ids().1)
    }

    fn files() -> ComposeFiles {
        ComposeFiles::new(
            "/srv/ship/snap",
            [
                PathBuf::from("/srv/ship/snap/compose.yaml"),
                PathBuf::from("/srv/ship/gen/override.json"),
            ],
        )
        .unwrap()
    }

    const NO: &[&str] = &[];

    /// Every builder, called with `services` (and the equivalent ids).
    fn every_builder(services: &[&str]) -> Vec<Result<Argv, DockerError>> {
        let p = project();
        let f = files();
        let up = UpOptions {
            remove_orphans: true,
        };
        let logs = LogOptions {
            follow: true,
            tail: Some(10),
        };
        vec![
            compose_config(&p, &f),
            compose_build(&p, &f, services),
            compose_up(&p, &f, services, up),
            compose_up(&p, &f, services, UpOptions::default()),
            compose_stop(&p, services),
            compose_start(&p, services),
            compose_restart(&p, services),
            compose_down(&p),
            compose_ps(&p),
            compose_logs(&p, services, logs),
            compose_logs(&p, services, LogOptions::default()),
            container_ids(&Selector::environment(ids().1)),
            container_ids(&Selector::environment(ids().1).service(ids().2)),
            container_inspect(services),
            project_network_ids(&p),
            network_inspect(services),
            project_container_ids(&p),
        ]
    }

    fn assert_safe(argv: &Argv) {
        let args = strs(argv);
        let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
        for a in &args[..split] {
            assert!(!DENIED.contains(&a.as_str()), "{a} in {args:?}");
        }
        for a in &args[split..].iter().skip(1).collect::<Vec<_>>() {
            assert!(!a.starts_with('-'), "{a} in {args:?}");
        }
    }

    #[test]
    fn exact_argv() {
        let p = project();
        let name = p.name().to_owned();
        let f = files();
        let head = [
            "compose",
            "--ansi",
            "never",
            "--project-name",
            name.as_str(),
        ];
        let mut with_files: Vec<&str> = head.to_vec();
        with_files.extend([
            "--project-directory",
            "/srv/ship/snap",
            "--file",
            "/srv/ship/snap/compose.yaml",
            "--file",
            "/srv/ship/gen/override.json",
        ]);
        fn cat<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<&'a str> {
            [a, b].concat()
        }

        assert_eq!(
            strs(&compose_config(&p, &f).unwrap()),
            cat(&with_files, &["config", "--format", "json"])
        );
        assert_eq!(
            strs(&compose_build(&p, &f, &["web"]).unwrap()),
            cat(&with_files, &["build", "--", "web"])
        );
        assert_eq!(
            strs(&compose_build(&p, &f, NO).unwrap()),
            cat(&with_files, &["build"])
        );
        assert_eq!(
            strs(&compose_up(&p, &f, NO, UpOptions::default()).unwrap()),
            cat(&with_files, &["up", "--detach", "--no-build"])
        );
        assert_eq!(
            strs(
                &compose_up(
                    &p,
                    &f,
                    &["a", "b"],
                    UpOptions {
                        remove_orphans: true
                    }
                )
                .unwrap()
            ),
            cat(
                &with_files,
                &[
                    "up",
                    "--detach",
                    "--no-build",
                    "--remove-orphans",
                    "--",
                    "a",
                    "b"
                ]
            )
        );
        assert_eq!(
            strs(&compose_stop(&p, &["web"]).unwrap()),
            cat(&head, &["stop", "--", "web"])
        );
        assert_eq!(
            strs(&compose_start(&p, NO).unwrap()),
            cat(&head, &["start"])
        );
        assert_eq!(
            strs(&compose_restart(&p, NO).unwrap()),
            cat(&head, &["restart"])
        );
        assert_eq!(strs(&compose_down(&p).unwrap()), cat(&head, &["down"]));
        assert_eq!(
            strs(&compose_ps(&p).unwrap()),
            cat(&head, &["ps", "--all", "--quiet"])
        );
        assert_eq!(
            strs(
                &compose_logs(
                    &p,
                    &["web"],
                    LogOptions {
                        follow: true,
                        tail: Some(5)
                    }
                )
                .unwrap()
            ),
            cat(
                &head,
                &[
                    "logs",
                    "--no-color",
                    "--timestamps",
                    "--follow",
                    "--tail",
                    "5",
                    "--",
                    "web"
                ]
            )
        );
        let env = ids().1;
        assert_eq!(
            strs(&container_ids(&Selector::environment(env)).unwrap()),
            [
                "ps",
                "--all",
                "--quiet",
                "--no-trunc",
                "--filter",
                format!("label=dev.ship.environment={env}").as_str()
            ]
        );
        let id = "0123456789abcdef";
        assert_eq!(
            strs(&container_inspect(&[id]).unwrap()),
            ["container", "inspect", "--", id]
        );
        assert_eq!(
            strs(&project_network_ids(&p).unwrap()),
            [
                "network",
                "ls",
                "--quiet",
                "--no-trunc",
                "--filter",
                format!("label=com.docker.compose.project={name}").as_str()
            ]
        );
        assert_eq!(
            strs(&network_inspect(&[id]).unwrap()),
            ["network", "inspect", "--", id]
        );
        assert_eq!(
            strs(&project_container_ids(&p).unwrap()),
            [
                "ps",
                "--all",
                "--quiet",
                "--no-trunc",
                "--filter",
                format!("label=com.docker.compose.project={name}").as_str()
            ]
        );
    }

    #[test]
    fn down_never_removes_volumes() {
        let args = strs(&compose_down(&project()).unwrap());
        for bad in ["-v", "--volumes", "--rmi"] {
            assert!(!args.iter().any(|a| a == bad), "{args:?}");
        }
    }

    #[test]
    fn hostile_inputs_are_refused_or_harmless() {
        let hostile: &[&str] = &[
            "-v",
            "--volumes",
            "--rmi",
            "volume",
            "rm",
            "prune",
            ";sh",
            "sh -c id",
            "$(id)",
            "a b",
            "",
            "-",
            "--",
            "../x",
            "web\n-v",
            "é",
            &"a".repeat(MAX_SERVICE_NAME + 1),
        ];
        for bad in hostile {
            for (i, result) in every_builder(&[*bad]).into_iter().enumerate() {
                match result {
                    Ok(argv) => {
                        // `volume`, `rm` and `prune` are legal service
                        // names; they may appear, but only after `--`.
                        assert_safe(&argv);
                        if !is_service_name(bad) {
                            assert!(
                                !strs(&argv).iter().any(|a| a == *bad),
                                "builder {i} passed {bad:?}: {argv:?}"
                            );
                        }
                    }
                    Err(DockerError::InvalidArgument(_)) => {}
                    Err(other) => panic!("builder {i}: {other:?}"),
                }
            }
        }
        // Names Compose allows pass, after `--`.
        for good in ["web", "db-1", "a.b_c", "0"] {
            let argv = compose_stop(&project(), &[good]).unwrap();
            assert_eq!(strs(&argv).last().unwrap(), good);
            assert_safe(&argv);
        }
    }

    #[test]
    fn every_builder_is_safe_with_ordinary_input() {
        for (i, result) in every_builder(&["0123456789abcdef"]).into_iter().enumerate() {
            let argv = result.unwrap_or_else(|e| panic!("builder {i}: {e}"));
            assert_safe(&argv);
        }
        for (i, result) in every_builder(NO).into_iter().enumerate() {
            match result {
                Ok(argv) => assert_safe(&argv),
                // Inspect needs at least one id.
                Err(DockerError::InvalidArgument(_)) if i == 13 || i == 15 => {}
                Err(e) => panic!("builder {i}: {e}"),
            }
        }
    }

    #[test]
    fn the_check_refuses_denied_tokens() {
        for bad in DENIED {
            let err = Argv::checked("x", vec!["compose".into(), (*bad).into()]).unwrap_err();
            assert!(matches!(err, DockerError::InvalidArgument(_)), "{bad}");
        }
        assert!(Argv::checked("x", vec!["stop".into(), "--".into(), "-v".into()]).is_err());
        // After `--` a denied word is a name, not a flag.
        assert!(Argv::checked("x", vec!["stop".into(), "--".into(), "volume".into()]).is_ok());
    }

    #[test]
    fn files_must_be_absolute_and_present() {
        assert!(ComposeFiles::new("/a", Vec::<PathBuf>::new()).is_err());
        assert!(ComposeFiles::new("a", [PathBuf::from("/a/c.yaml")]).is_err());
        assert!(ComposeFiles::new("/a", [PathBuf::from("c.yaml")]).is_err());
        assert!(ComposeFiles::new("/a", [PathBuf::from("-v")]).is_err());
        assert!(ComposeFiles::new("/a", [PathBuf::from("/a/c.yaml")]).is_ok());
    }

    #[test]
    fn project_name_derives_from_the_environment() {
        let env = ids().1;
        assert_eq!(ComposeProject::new(env).name(), format!("ship_{env}"));
    }

    #[test]
    fn object_ids() {
        assert!(is_object_id("0123456789ab"));
        assert!(is_object_id(&"f".repeat(64)));
        assert!(!is_object_id("0123456789a"));
        assert!(!is_object_id(&"f".repeat(65)));
        assert!(!is_object_id("0123456789AB"));
        assert!(!is_object_id("-123456789ab"));
    }
}
