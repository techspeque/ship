//! The Compose trust profile (ADR-0008): what a Compose file may do without
//! asking, what needs the operator's confirmation, and what Ship refuses.
//!
//! The profile reads the canonical model `docker compose config` prints,
//! after interpolation, so it sees what Compose will do rather than what
//! the file seems to say. That model is sensitive (rule 7): an [`Item`]
//! carries only keys, paths, ports, capability names and policy names from
//! it, never an environment value or a label.
//!
//! It is accident prevention, not isolation (rule 8): a build step or an
//! allowed container can still reach whatever it is given.

use std::fmt;

use serde_json::Value;

/// The restart policy Ship gives the services it generates; a Compose file
/// asking for another one needs confirmation (ADR-0008 rule 2).
pub const DEFAULT_RESTART: &str = "unless-stopped";

/// Directories that hold the host's Docker socket: binding one of them
/// hands the container the Engine, as binding the socket itself does.
pub const SOCKET_PARENTS: &[&str] = &["/", "/run", "/var", "/var/run"];

/// The namespace keys whose value `host` shares one of the host's
/// namespaces with the container.
pub const HOST_NAMESPACE_KEYS: &[&str] =
    &["pid", "ipc", "network_mode", "uts", "userns_mode", "cgroup"];

string_enum! {
    /// What the profile decides about one item. Everything that is neither
    /// is allowed and has no item.
    #[derive(PartialOrd, Ord)]
    pub enum Outcome {
        Confirm => "confirm",
        Reject => "reject",
    }
}

string_enum! {
    /// Which rule an item comes under.
    pub enum Rule {
        /// `privileged: true`.
        Privileged => "privileged",
        /// `pid`, `ipc`, `network_mode`, `uts`, `userns_mode` or `cgroup`
        /// set to `host`.
        HostNamespace => "host_namespace",
        /// `devices` or `device_cgroup_rules`.
        Devices => "devices",
        /// A bind of the Docker socket or a directory holding it.
        DockerSocket => "docker_socket",
        /// A bind whose source is a host path outside the snapshot.
        HostBind => "host_bind",
        /// A build context that is a host path outside the snapshot.
        BuildContext => "build_context",
        /// A `ports` entry.
        PublishedPort => "published_port",
        /// `cap_add`.
        AddedCapability => "cap_add",
        /// `gpus` or `deploy.resources.reservations.devices`.
        Gpu => "gpu",
        /// A `restart` other than [`DEFAULT_RESTART`], or a
        /// `deploy.restart_policy`.
        RestartPolicy => "restart_policy",
        /// A mount of a type the profile does not know.
        UnknownMount => "unknown_mount",
    }
}

impl Rule {
    /// The outcome of every item under this rule (ADR-0008 rules 2 and 3).
    pub fn outcome(self) -> Outcome {
        match self {
            Rule::Privileged | Rule::HostNamespace | Rule::Devices | Rule::DockerSocket => {
                Outcome::Reject
            }
            Rule::HostBind
            | Rule::BuildContext
            | Rule::PublishedPort
            | Rule::AddedCapability
            | Rule::Gpu
            | Rule::RestartPolicy
            | Rule::UnknownMount => Outcome::Confirm,
        }
    }
}

string_enum! {
    /// The `compose.confirm` setting: `ask` (the default) shows confirm
    /// items to the operator; `accept` accepts them by policy and records
    /// each one (ADR-0008 rule 2). Reject items are never affected.
    pub enum ConfirmPolicy {
        Ask => "ask",
        Accept => "accept",
    }
}

/// One thing in a Compose file the operator must confirm or Ship refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub rule: Rule,
    /// The Compose service it is in; `None` for a top-level volume, config
    /// or secret.
    pub service: Option<String>,
    /// Where it is in the model: `services.web.volumes[0]`.
    pub key: String,
    /// The 1-based line of the main Compose file it was found on, when the
    /// file's block YAML could be followed to it.
    pub line: Option<u32>,
    /// What it does, in words.
    pub text: String,
}

impl Item {
    pub fn outcome(&self) -> Outcome {
        self.rule.outcome()
    }
}

impl fmt::Display for Item {
    /// `line 7: services.web.privileged: privileged: true gives …`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(line) = self.line {
            write!(f, "line {line}: ")?;
        }
        write!(f, "{}: {}", self.key, self.text)
    }
}

/// A bind mount whose source is inside the snapshot and which Compose
/// would mount read-write. Ship mounts it read-only (ADR-0008 rule 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotBind {
    pub service: String,
    /// The mount's position in the service's `volumes`.
    pub index: usize,
    /// The normalised source path.
    pub source: String,
    /// The path inside the container.
    pub target: String,
    /// The canonical entry's `bind` options (propagation, SELinux label,
    /// host path creation), kept as they are.
    pub bind: Option<Value>,
}

/// The profile's verdict on a whole file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evaluation {
    /// In the model's order: services first, then top-level volumes,
    /// configs and secrets.
    pub items: Vec<Item>,
    pub snapshot_binds: Vec<SnapshotBind>,
}

impl Evaluation {
    /// The file's outcome: the strongest of its items, `None` for allow.
    pub fn outcome(&self) -> Option<Outcome> {
        self.items.iter().map(Item::outcome).max()
    }

    pub fn rejected(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| i.outcome() == Outcome::Reject)
    }

    pub fn confirmations(&self) -> impl Iterator<Item = &Item> {
        self.items
            .iter()
            .filter(|i| i.outcome() == Outcome::Confirm)
    }
}

/// Evaluates the canonical model `config` (`compose config --format json`)
/// of a Compose file in the snapshot at `snapshot_root` (an absolute path).
/// `source`, the main file's text, is used only to find line numbers.
pub fn evaluate(config: &Value, snapshot_root: &str, source: Option<&str>) -> Evaluation {
    let mut e = Evaluator {
        root: normalize(snapshot_root).unwrap_or_else(|| snapshot_root.to_owned()),
        lines: source.map(content_lines).unwrap_or_default(),
        out: Evaluation::default(),
    };
    for (name, service) in section(config, "services") {
        e.service(name, service);
    }
    for (name, volume) in section(config, "volumes") {
        e.top_volume(name, volume);
    }
    for kind in ["configs", "secrets"] {
        for (name, def) in section(config, kind) {
            e.file_source(kind, name, def);
        }
    }
    e.out
}

fn section<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = (&'a String, &'a Value)> {
    v.get(key).and_then(Value::as_object).into_iter().flatten()
}

fn non_empty_array(v: Option<&Value>) -> Option<&Vec<Value>> {
    v.and_then(Value::as_array).filter(|a| !a.is_empty())
}

/// Where a host path is, as far as the profile cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Snapshot,
    Socket,
    Host,
}

/// One step of a key: a map key, or the `n`th of `len` list items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seg<'a> {
    Key(&'a str),
    Index(usize, usize),
}

fn key_text(segs: &[Seg<'_>]) -> String {
    let mut out = String::new();
    for seg in segs {
        match seg {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Index(n, _) => out.push_str(&format!("[{n}]")),
        }
    }
    out
}

struct Evaluator<'t> {
    root: String,
    lines: Vec<SourceLine<'t>>,
    out: Evaluation,
}

impl Evaluator<'_> {
    fn push(&mut self, rule: Rule, service: Option<&str>, segs: &[Seg<'_>], text: String) {
        self.out.items.push(Item {
            rule,
            service: service.map(str::to_owned),
            key: key_text(segs),
            line: locate(&self.lines, segs),
            text,
        });
    }

    fn place(&self, path: &str) -> Place {
        let Some(p) = normalize(path) else {
            return Place::Host;
        };
        if p == self.root || p.starts_with(&format!("{}/", self.root)) {
            Place::Snapshot
        } else if p.ends_with("/docker.sock") || SOCKET_PARENTS.contains(&p.as_str()) {
            Place::Socket
        } else {
            Place::Host
        }
    }

    fn service(&mut self, name: &str, s: &Value) {
        let base = [Seg::Key("services"), Seg::Key(name)];
        let at = |key: &'static str| [base[0], base[1], Seg::Key(key)];
        let svc = Some(name);

        if s.get("privileged").and_then(Value::as_bool) == Some(true) {
            self.push(
                Rule::Privileged,
                svc,
                &at("privileged"),
                "privileged: true gives the container the host".to_owned(),
            );
        }
        for &key in HOST_NAMESPACE_KEYS {
            if s.get(key).and_then(Value::as_str) == Some("host") {
                self.push(
                    Rule::HostNamespace,
                    svc,
                    &at(key),
                    format!("{key}: host shares the host's namespace with the container"),
                );
            }
        }
        if let Some(devices) = non_empty_array(s.get("devices")) {
            let len = devices.len();
            for (i, d) in devices.iter().enumerate() {
                let source = match d {
                    Value::String(text) => text.split(':').next().unwrap_or("").to_owned(),
                    other => other
                        .get("source")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                };
                self.push(
                    Rule::Devices,
                    svc,
                    &[base[0], base[1], Seg::Key("devices"), Seg::Index(i, len)],
                    format!("passes the host device {source} through to the container"),
                );
            }
        }
        if non_empty_array(s.get("device_cgroup_rules")).is_some() {
            self.push(
                Rule::Devices,
                svc,
                &at("device_cgroup_rules"),
                "device_cgroup_rules gives the container raw host devices".to_owned(),
            );
        }
        if let Some(mounts) = non_empty_array(s.get("volumes")) {
            let len = mounts.len();
            for (i, m) in mounts.iter().enumerate() {
                let segs = [base[0], base[1], Seg::Key("volumes"), Seg::Index(i, len)];
                self.mount(name, i, m, &segs);
            }
        }
        if let Some(ports) = non_empty_array(s.get("ports")) {
            let len = ports.len();
            for (i, p) in ports.iter().enumerate() {
                self.push(
                    Rule::PublishedPort,
                    svc,
                    &[base[0], base[1], Seg::Key("ports"), Seg::Index(i, len)],
                    port_text(p),
                );
            }
        }
        if let Some(caps) = non_empty_array(s.get("cap_add")) {
            let names: Vec<&str> = caps.iter().filter_map(Value::as_str).collect();
            self.push(
                Rule::AddedCapability,
                svc,
                &at("cap_add"),
                format!("adds the capabilities {}", names.join(", ")),
            );
        }
        let gpus = s.get("gpus");
        if gpus.is_some_and(|g| !g.is_null() && g != &Value::Array(Vec::new())) {
            self.push(
                Rule::Gpu,
                svc,
                &at("gpus"),
                "reserves the host's GPUs (gpus)".to_owned(),
            );
        }
        let reserved = s.pointer("/deploy/resources/reservations/devices");
        if let Some(devices) = non_empty_array(reserved) {
            let len = devices.len();
            for (i, d) in devices.iter().enumerate() {
                self.push(
                    Rule::Gpu,
                    svc,
                    &[
                        base[0],
                        base[1],
                        Seg::Key("deploy"),
                        Seg::Key("resources"),
                        Seg::Key("reservations"),
                        Seg::Key("devices"),
                        Seg::Index(i, len),
                    ],
                    reservation_text(d),
                );
            }
        }
        if let Some(restart) = s.get("restart").and_then(Value::as_str)
            && restart != DEFAULT_RESTART
        {
            self.push(
                Rule::RestartPolicy,
                svc,
                &at("restart"),
                format!("restart: {restart} differs from Ship's default {DEFAULT_RESTART}"),
            );
        }
        if s.pointer("/deploy/restart_policy")
            .is_some_and(|p| !p.is_null())
        {
            let condition = s
                .pointer("/deploy/restart_policy/condition")
                .and_then(Value::as_str)
                .unwrap_or("any");
            self.push(
                Rule::RestartPolicy,
                svc,
                &[
                    base[0],
                    base[1],
                    Seg::Key("deploy"),
                    Seg::Key("restart_policy"),
                ],
                format!(
                    "deploy.restart_policy (condition {condition}) differs from Ship's default \
                     {DEFAULT_RESTART}"
                ),
            );
        }
        if let Some(build) = s.get("build").filter(|b| b.is_object()) {
            self.build(name, build);
        }
    }

    fn mount(&mut self, service: &str, index: usize, m: &Value, segs: &[Seg<'_>]) {
        let kind = m.get("type").and_then(Value::as_str).unwrap_or("");
        let target = m.get("target").and_then(Value::as_str).unwrap_or("");
        let svc = Some(service);
        match kind {
            "volume" | "tmpfs" | "image" => {}
            "bind" | "npipe" => {
                let source = m.get("source").and_then(Value::as_str).unwrap_or("");
                let read_only = m.get("read_only").and_then(Value::as_bool) == Some(true);
                match self.place(source) {
                    Place::Snapshot => {
                        if !read_only && kind == "bind" {
                            self.out.snapshot_binds.push(SnapshotBind {
                                service: service.to_owned(),
                                index,
                                source: normalize(source).unwrap_or_else(|| source.to_owned()),
                                target: target.to_owned(),
                                bind: m.get("bind").cloned(),
                            });
                        }
                    }
                    Place::Socket => self.push(
                        Rule::DockerSocket,
                        svc,
                        segs,
                        format!("mounts {source}, which gives the container the Docker Engine"),
                    ),
                    Place::Host => {
                        let mode = if read_only { "read-only" } else { "read-write" };
                        self.push(
                            Rule::HostBind,
                            svc,
                            segs,
                            format!("mounts the host path {source} at {target} ({mode})"),
                        );
                    }
                }
            }
            other => self.push(
                Rule::UnknownMount,
                svc,
                segs,
                format!("a mount of type `{other}` at {target}, which Ship does not know"),
            ),
        }
    }

    fn build(&mut self, service: &str, build: &Value) {
        let svc = Some(service);
        if let Some(context) = build.get("context").and_then(Value::as_str)
            && context.starts_with('/')
            && self.place(context) != Place::Snapshot
        {
            self.push(
                Rule::BuildContext,
                svc,
                &[
                    Seg::Key("services"),
                    Seg::Key(service),
                    Seg::Key("build"),
                    Seg::Key("context"),
                ],
                format!("the build reads the host path {context}, outside the snapshot"),
            );
        }
        for (name, ctx) in section(build, "additional_contexts") {
            if let Some(path) = ctx.as_str()
                && path.starts_with('/')
                && self.place(path) != Place::Snapshot
            {
                self.push(
                    Rule::BuildContext,
                    svc,
                    &[
                        Seg::Key("services"),
                        Seg::Key(service),
                        Seg::Key("build"),
                        Seg::Key("additional_contexts"),
                        Seg::Key(name),
                    ],
                    format!("the build reads the host path {path}, outside the snapshot"),
                );
            }
        }
    }

    /// A top-level volume of the `local` driver bound to a host path.
    fn top_volume(&mut self, name: &str, v: &Value) {
        let Some(opts) = v.get("driver_opts").and_then(Value::as_object) else {
            return;
        };
        let bind = opts.get("o").and_then(Value::as_str).is_some_and(|o| {
            o.split(',')
                .any(|x| x.trim() == "bind" || x.trim() == "rbind")
        });
        let Some(device) = opts.get("device").and_then(Value::as_str) else {
            return;
        };
        if !bind {
            return;
        }
        let segs = [Seg::Key("volumes"), Seg::Key(name), Seg::Key("driver_opts")];
        if self.place(device) == Place::Socket {
            self.push(
                Rule::DockerSocket,
                None,
                &segs,
                format!(
                    "volume {name} binds {device}, which gives its containers the Docker Engine"
                ),
            );
        } else {
            self.push(
                Rule::HostBind,
                None,
                &segs,
                format!("volume {name} binds the host path {device}"),
            );
        }
    }

    /// A top-level config or secret read from a file.
    fn file_source(&mut self, kind: &'static str, name: &str, def: &Value) {
        let Some(file) = def.get("file").and_then(Value::as_str) else {
            return;
        };
        let segs = [Seg::Key(kind), Seg::Key(name), Seg::Key("file")];
        match self.place(file) {
            Place::Snapshot => {}
            Place::Socket => self.push(
                Rule::DockerSocket,
                None,
                &segs,
                format!(
                    "{kind} {name} mounts {file}, which gives its containers the Docker Engine"
                ),
            ),
            Place::Host => self.push(
                Rule::HostBind,
                None,
                &segs,
                format!("{kind} {name} mounts the host file {file}"),
            ),
        }
    }
}

fn port_text(p: &Value) -> String {
    let target = match p.get("target") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => "?".to_owned(),
    };
    let protocol = p.get("protocol").and_then(Value::as_str).unwrap_or("tcp");
    let host_ip = p
        .get("host_ip")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("0.0.0.0");
    match p
        .get("published")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(published) => format!("publishes {host_ip}:{published} to {target}/{protocol}"),
        None => format!("publishes {target}/{protocol} on an ephemeral port of {host_ip}"),
    }
}

fn reservation_text(d: &Value) -> String {
    let words = |key: &str| -> String {
        d.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let capabilities = words("capabilities");
    let driver = d.get("driver").and_then(Value::as_str).unwrap_or("any");
    let count = match d.get("count") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => match d.get("device_ids") {
            Some(Value::Array(ids)) if !ids.is_empty() => format!("{} named", ids.len()),
            _ => "all".to_owned(),
        },
    };
    format!(
        "reserves host devices through a container runtime (capabilities {capabilities}, \
         driver {driver}, count {count})"
    )
}

/// `path` with `.` and `..` components resolved and no trailing `/`;
/// `None` unless it is absolute.
pub fn normalize(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    Some(format!("/{}", parts.join("/")))
}

// ---- line numbers ----

/// A line of YAML with content: not blank, not only a comment.
#[derive(Debug, Clone, Copy)]
struct SourceLine<'t> {
    number: u32,
    indent: usize,
    body: &'t str,
}

fn content_lines(text: &str) -> Vec<SourceLine<'_>> {
    text.lines()
        .enumerate()
        .filter_map(|(i, raw)| {
            let body = raw.trim_start_matches(' ');
            let blank = body.trim().is_empty() || body.starts_with('#');
            let marker = body.starts_with("---") || body.starts_with("...");
            (!blank && !marker).then(|| SourceLine {
                number: u32::try_from(i + 1).unwrap_or(u32::MAX),
                indent: raw.len() - body.len(),
                body,
            })
        })
        .collect()
}

fn is_item(body: &str) -> bool {
    body == "-" || body.starts_with("- ")
}

/// The key a block-mapping line starts, unquoted: `web:` and `"web": x`
/// give `web`. `None` for a list item or a line that is no `key:`.
fn key_of(body: &str) -> Option<&str> {
    if is_item(body) {
        return None;
    }
    let (key, rest) = match body.chars().next()? {
        q @ ('"' | '\'') => {
            let end = body[1..].find(q)? + 1;
            (&body[1..end], body[end + 1..].trim_start())
        }
        _ => {
            let colon = body
                .char_indices()
                .find(|&(i, c)| c == ':' && body[i + 1..].chars().next().is_none_or(|n| n == ' '))?
                .0;
            (body[..colon].trim_end(), &body[colon..])
        }
    };
    rest.starts_with(':').then_some(key)
}

/// The line of `segs` in a block-YAML file. Every map key must be found,
/// or there is no line. A list item that cannot be found, because the list
/// is in flow style or its length differs from the model's (Compose
/// expanded or merged it), gives the line of the list's key instead.
fn locate(lines: &[SourceLine<'_>], segs: &[Seg<'_>]) -> Option<u32> {
    let (mut lo, mut hi) = (0, lines.len());
    let mut best = None;
    for seg in segs {
        if lo >= hi {
            return match seg {
                Seg::Index(..) => best,
                Seg::Key(_) => None,
            };
        }
        let level = lines[lo].indent;
        match *seg {
            Seg::Key(k) => {
                let i = (lo..hi)
                    .find(|&i| lines[i].indent == level && key_of(lines[i].body) == Some(k))?;
                best = Some(lines[i].number);
                let mut j = i + 1;
                while j < hi
                    && (lines[j].indent > level
                        || (lines[j].indent == level && is_item(lines[j].body)))
                {
                    j += 1;
                }
                (lo, hi) = (i + 1, j);
            }
            Seg::Index(n, len) => {
                let at_level: Vec<usize> = (lo..hi).filter(|&i| lines[i].indent == level).collect();
                if at_level.len() != len || !at_level.iter().all(|&i| is_item(lines[i].body)) {
                    return best;
                }
                let i = at_level[n];
                best = Some(lines[i].number);
                let mut j = i + 1;
                while j < hi && lines[j].indent > level {
                    j += 1;
                }
                (lo, hi) = (i + 1, j);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ROOT: &str = "/var/lib/ship/snapshots/abc";

    fn eval(config: Value) -> Evaluation {
        evaluate(&config, ROOT, None)
    }

    fn rules(e: &Evaluation) -> Vec<Rule> {
        e.items.iter().map(|i| i.rule).collect()
    }

    fn one(service: Value) -> Evaluation {
        eval(json!({ "services": { "web": service } }))
    }

    #[test]
    fn an_ordinary_file_is_allowed() {
        let e = eval(json!({
            "name": "x",
            "services": {
                "web": {
                    "image": "nginx:1",
                    "build": { "context": format!("{ROOT}/web"), "dockerfile": "Dockerfile" },
                    "depends_on": { "db": { "condition": "service_healthy" } },
                    "healthcheck": { "test": ["CMD", "true"] },
                    "networks": { "default": null },
                    "expose": ["80"],
                    "restart": "unless-stopped",
                    "environment": { "PASSWORD": "interpolated" },
                    "volumes": [
                        { "type": "volume", "source": "data", "target": "/data" },
                        { "type": "tmpfs", "target": "/tmp" },
                        { "type": "bind", "source": format!("{ROOT}/conf/nginx.conf"),
                          "target": "/etc/nginx/nginx.conf", "read_only": true },
                    ],
                },
                "db": { "image": "postgres:16", "network_mode": "service:web", "pid": "service:web" },
            },
            "networks": { "default": { "name": "x_default" } },
            "volumes": { "data": { "name": "x_data" } },
            "secrets": { "token": { "file": format!("{ROOT}/token.txt") } },
            "configs": { "c": { "environment": "C" } },
        }));
        assert_eq!(e.items, Vec::new());
        assert_eq!(e.outcome(), None);
        assert_eq!(e.snapshot_binds, Vec::new());
    }

    #[test]
    fn rejects() {
        let e = one(json!({
            "privileged": true,
            "pid": "host", "ipc": "host", "network_mode": "host",
            "uts": "host", "userns_mode": "host", "cgroup": "host",
            "devices": ["/dev/sda:/dev/sda:rwm", { "source": "/dev/kvm", "target": "/dev/kvm" }],
            "device_cgroup_rules": ["c 1:3 mr"],
        }));
        assert_eq!(
            rules(&e),
            [
                Rule::Privileged,
                Rule::HostNamespace,
                Rule::HostNamespace,
                Rule::HostNamespace,
                Rule::HostNamespace,
                Rule::HostNamespace,
                Rule::HostNamespace,
                Rule::Devices,
                Rule::Devices,
                Rule::Devices,
            ]
        );
        assert_eq!(e.outcome(), Some(Outcome::Reject));
        assert_eq!(e.items[7].key, "services.web.devices[0]");
        assert!(e.items[7].text.contains("/dev/sda"), "{}", e.items[7].text);
        assert!(e.items[8].text.contains("/dev/kvm"), "{}", e.items[8].text);
        assert_eq!(e.confirmations().count(), 0);
        assert_eq!(e.rejected().count(), 10);
        // `privileged: false` and a container's namespace are allowed.
        assert_eq!(
            one(json!({ "privileged": false, "ipc": "shareable" })).items,
            Vec::new()
        );
    }

    #[test]
    fn the_docker_socket_is_rejected_however_it_is_mounted() {
        for source in [
            "/var/run/docker.sock",
            "/run/docker.sock",
            "/run/user/1000/docker.sock",
            "/var/run",
            "/run/",
            "/var",
            "/",
            "/srv/../var/run/./docker.sock",
        ] {
            let e =
                one(json!({ "volumes": [{ "type": "bind", "source": source, "target": "/s" }] }));
            assert_eq!(rules(&e), [Rule::DockerSocket], "{source}");
        }
        let e = eval(json!({
            "services": {},
            "volumes": { "sock": { "driver_opts": { "type": "none", "o": "bind", "device": "/var/run" } } },
            "secrets": { "s": { "file": "/var/run/docker.sock" } },
        }));
        assert_eq!(rules(&e), [Rule::DockerSocket, Rule::DockerSocket]);
        assert_eq!(e.items[0].service, None);
        assert_eq!(e.items[0].key, "volumes.sock.driver_opts");
    }

    #[test]
    fn host_paths_need_confirmation() {
        let e = eval(json!({
            "services": { "web": {
                "volumes": [
                    { "type": "bind", "source": "/srv/data", "target": "/data" },
                    { "type": "bind", "source": format!("{ROOT}/../other"), "target": "/o", "read_only": true },
                    { "type": "cluster", "source": "x", "target": "/c" },
                ],
                "build": {
                    "context": "/home/me/elsewhere",
                    "additional_contexts": { "a": "/opt/x", "b": "docker-image://alpine", "c": format!("{ROOT}/c") },
                },
            }},
            "volumes": { "db": { "driver": "local", "driver_opts": { "type": "none", "o": "rbind,ro", "device": "/mnt/db" } } },
            "configs": { "c": { "file": "/etc/app.conf" } },
        }));
        assert_eq!(
            rules(&e),
            [
                Rule::HostBind,
                Rule::HostBind,
                Rule::UnknownMount,
                Rule::BuildContext,
                Rule::BuildContext,
                Rule::HostBind,
                Rule::HostBind,
            ]
        );
        assert_eq!(e.outcome(), Some(Outcome::Confirm));
        assert_eq!(
            e.items[0].text,
            "mounts the host path /srv/data at /data (read-write)"
        );
        assert!(
            e.items[1].text.ends_with("(read-only)"),
            "{}",
            e.items[1].text
        );
        assert_eq!(e.items[4].key, "services.web.build.additional_contexts.a");
        // A local volume without a bind is a named volume.
        let named = eval(
            json!({ "services": {}, "volumes": { "v": { "driver_opts": { "type": "tmpfs", "device": "tmpfs" } } } }),
        );
        assert_eq!(named.items, Vec::new());
    }

    #[test]
    fn snapshot_binds_are_listed_unless_read_only() {
        let e = one(json!({ "volumes": [
            { "type": "bind", "source": format!("{ROOT}/conf/"), "target": "/conf", "bind": { "create_host_path": true } },
            { "type": "bind", "source": format!("{ROOT}/ro"), "target": "/ro", "read_only": true },
            { "type": "bind", "source": ROOT, "target": "/app" },
            { "type": "bind", "source": format!("{ROOT}x/sibling"), "target": "/s" },
        ]}));
        assert_eq!(
            rules(&e),
            [Rule::HostBind],
            "a sibling directory is not inside"
        );
        let binds: Vec<(usize, &str, &str)> = e
            .snapshot_binds
            .iter()
            .map(|b| (b.index, b.source.as_str(), b.target.as_str()))
            .collect();
        let conf = format!("{ROOT}/conf");
        assert_eq!(binds, [(0, conf.as_str(), "/conf"), (2, ROOT, "/app")]);
        assert_eq!(
            e.snapshot_binds[0].bind,
            Some(json!({ "create_host_path": true }))
        );
    }

    #[test]
    fn ports_capabilities_gpus_and_restart_need_confirmation() {
        let e = one(json!({
            "ports": [
                { "mode": "ingress", "target": 80, "published": "8080", "protocol": "tcp" },
                { "target": 53, "published": "5353", "protocol": "udp", "host_ip": "127.0.0.1" },
                { "target": 9000, "protocol": "tcp" },
            ],
            "cap_add": ["NET_ADMIN", "SYS_TIME"],
            "gpus": [{ "driver": "nvidia", "count": -1 }],
            "deploy": {
                "resources": { "reservations": { "devices": [
                    { "capabilities": ["gpu"], "driver": "nvidia", "count": 1 },
                ]}},
                "restart_policy": { "condition": "on-failure" },
            },
            "restart": "always",
        }));
        assert_eq!(
            rules(&e),
            [
                Rule::PublishedPort,
                Rule::PublishedPort,
                Rule::PublishedPort,
                Rule::AddedCapability,
                Rule::Gpu,
                Rule::Gpu,
                Rule::RestartPolicy,
                Rule::RestartPolicy,
            ]
        );
        let texts: Vec<&str> = e.items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts[0], "publishes 0.0.0.0:8080 to 80/tcp");
        assert_eq!(texts[1], "publishes 127.0.0.1:5353 to 53/udp");
        assert_eq!(
            texts[2],
            "publishes 9000/tcp on an ephemeral port of 0.0.0.0"
        );
        assert_eq!(texts[3], "adds the capabilities NET_ADMIN, SYS_TIME");
        assert!(
            texts[5].contains("capabilities gpu, driver nvidia, count 1"),
            "{}",
            texts[5]
        );
        assert_eq!(
            e.items[5].key,
            "services.web.deploy.resources.reservations.devices[0]"
        );
        assert!(texts[6].starts_with("restart: always"), "{}", texts[6]);
        assert!(texts[7].contains("on-failure"), "{}", texts[7]);
        // Ship's own policy, an empty list and a null are allowed.
        let e =
            one(json!({ "restart": "unless-stopped", "ports": [], "cap_add": [], "gpus": null }));
        assert_eq!(e.items, Vec::new());
        assert_eq!(
            rules(&one(json!({ "restart": "no" }))),
            [Rule::RestartPolicy]
        );
        assert_eq!(rules(&one(json!({ "gpus": "all" }))), [Rule::Gpu]);
    }

    #[test]
    fn paths_are_normalised() {
        assert_eq!(normalize("/a/./b/../c/"), Some("/a/c".to_owned()));
        assert_eq!(normalize("/../.."), Some("/".to_owned()));
        assert_eq!(normalize("//x//y"), Some("/x/y".to_owned()));
        assert_eq!(normalize("rel/path"), None);
        // A relative source is not taken to be inside the snapshot.
        let e = one(json!({ "volumes": [{ "type": "bind", "source": "conf", "target": "/c" }] }));
        assert_eq!(rules(&e), [Rule::HostBind]);
    }

    #[test]
    fn policies() {
        assert_eq!("ask".parse::<ConfirmPolicy>(), Ok(ConfirmPolicy::Ask));
        assert_eq!("accept".parse::<ConfirmPolicy>(), Ok(ConfirmPolicy::Accept));
        assert!("yes".parse::<ConfirmPolicy>().is_err());
        assert_eq!(Outcome::Reject.max(Outcome::Confirm), Outcome::Reject);
    }

    const SOURCE: &str = "\
# a comment
name: demo
services:
  web:
    image: nginx
    privileged: true
    ports:
      - \"8080:80\"
      # between items
      - target: 443
        published: \"8443\"
    volumes: [\"/srv:/srv\"]
  \"db\":
    image: postgres
    cap_add:
    - NET_ADMIN
    devices:
    - /dev/a
    - /dev/b
    deploy:
      resources:
        reservations:
          devices:
            - capabilities: [gpu]
";

    fn line(segs: &[Seg<'_>]) -> Option<u32> {
        locate(&content_lines(SOURCE), segs)
    }

    #[test]
    fn lines_are_found_in_block_yaml() {
        use Seg::{Index, Key};
        let svc = |s, k| [Key("services"), Key(s), Key(k)];
        assert_eq!(line(&svc("web", "privileged")), Some(6));
        assert_eq!(
            line(&[Key("services"), Key("web"), Key("ports"), Index(0, 2)]),
            Some(8)
        );
        assert_eq!(
            line(&[Key("services"), Key("web"), Key("ports"), Index(1, 2)]),
            Some(10)
        );
        // A quoted key, a sequence at its key's indentation.
        assert_eq!(line(&svc("db", "cap_add")), Some(15));
        assert_eq!(
            line(&[Key("services"), Key("db"), Key("devices"), Index(1, 2)]),
            Some(19)
        );
        assert_eq!(
            line(&[
                Key("services"),
                Key("db"),
                Key("deploy"),
                Key("resources"),
                Key("reservations"),
                Key("devices"),
                Index(0, 1)
            ]),
            Some(24)
        );
        // Flow style, or a list Compose changed the length of: the list's line.
        assert_eq!(
            line(&[Key("services"), Key("web"), Key("volumes"), Index(0, 1)]),
            Some(12)
        );
        assert_eq!(
            line(&[Key("services"), Key("web"), Key("ports"), Index(0, 3)]),
            Some(7)
        );
        // A key that is not there (from another file, say): no line.
        assert_eq!(line(&svc("web", "cap_add")), None);
        assert_eq!(line(&svc("cache", "privileged")), None);
        // `image` under `db` is not `image` under `web`.
        assert_eq!(line(&svc("db", "image")), Some(14));
        assert_eq!(
            locate(
                &content_lines("{services: {web: {privileged: true}}}"),
                &svc("web", "privileged")
            ),
            None
        );
    }

    #[test]
    fn items_carry_their_lines() {
        let config = json!({ "services": {
            "web": { "privileged": true, "ports": [
                { "target": 80, "published": "8080" },
                { "target": 443, "published": "8443" },
            ]},
        }});
        let e = evaluate(&config, ROOT, Some(SOURCE));
        let lines: Vec<Option<u32>> = e.items.iter().map(|i| i.line).collect();
        assert_eq!(lines, [Some(6), Some(8), Some(10)]);
        assert_eq!(
            e.items[0].to_string(),
            "line 6: services.web.privileged: privileged: true gives the container the host"
        );
        let unlocated = evaluate(&config, ROOT, None);
        assert_eq!(
            unlocated.items[0].to_string(),
            "services.web.privileged: privileged: true gives the container the host"
        );
    }

    #[test]
    fn keys_are_read_plainly() {
        assert_eq!(key_of("web:"), Some("web"));
        assert_eq!(key_of("image: a:b"), Some("image"));
        assert_eq!(key_of("\"we:b\": x"), Some("we:b"));
        assert_eq!(key_of("'db':"), Some("db"));
        assert_eq!(key_of("- a: b"), None);
        assert_eq!(key_of("plain"), None);
        assert_eq!(key_of("url: http://x"), Some("url"));
        assert_eq!(key_of("http://x"), None);
    }
}
