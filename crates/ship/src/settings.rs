//! `ship settings get|set KEY [VALUE]`: daemon settings, held by the
//! daemon and never read from a pushed directory (phase-1 plan,
//! decisions).

use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Subcommand, ValueEnum};
use ship_core::api::{
    COMPOSE_CONFIRM_KEY, IDEMPOTENCY_KEY_HEADER, OperationView, PUBLISH_ADDRESS_KEY,
    SetSettingRequest, SettingView, operation_path, project_setting_path, setting_path,
};
use ship_core::deploy::PublishAddress;
use ship_core::trust::ConfirmPolicy;
use ship_core::{OperationStatus, Slug};

use crate::client::{self, Endpoint};
use crate::commands::{FAILURE, Output, endpoint, fail, runtime};

/// Exit code for a usage error.
const USAGE: u8 = 2;
/// Exit code when a change needs a confirmation that was not given.
const CONFIRMATION_REQUIRED: u8 = 3;
/// How often `set` asks whether its operation has ended.
const POLL: Duration = Duration::from_millis(200);

/// A setting the daemon knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SettingKey {
    /// Where the ports Ship publishes bind: `auto` (the host's tailnet
    /// address when it has one, else 127.0.0.1) or an address of the host
    /// (0.0.0.0 for every address).
    #[value(name = "publish.address")]
    PublishAddress,
    /// Whether a Compose file's confirmation items are asked for (`ask`)
    /// or accepted by policy (`accept`), for the host or, with --project,
    /// one project.
    #[value(name = "compose.confirm")]
    ComposeConfirm,
}

impl SettingKey {
    fn as_str(self) -> &'static str {
        match self {
            SettingKey::PublishAddress => PUBLISH_ADDRESS_KEY,
            SettingKey::ComposeConfirm => COMPOSE_CONFIRM_KEY,
        }
    }

    /// The path of the setting, for the host or for `project`; a project
    /// scope for a host-only key is a usage error.
    fn path(self, project: Option<&Slug>) -> Result<String, String> {
        match (self, project) {
            (_, None) => Ok(setting_path(self.as_str())),
            (SettingKey::ComposeConfirm, Some(p)) => Ok(project_setting_path(p, self.as_str())),
            (SettingKey::PublishAddress, Some(_)) => Err(format!(
                "{} is a host setting; --project does not apply",
                self.as_str()
            )),
        }
    }

    /// Checks `value` before anything is sent.
    fn check(self, value: &str) -> Result<(), String> {
        match self {
            SettingKey::PublishAddress => value
                .parse::<PublishAddress>()
                .map(|_| ())
                .map_err(|e| e.to_string()),
            SettingKey::ComposeConfirm => value
                .parse::<ConfirmPolicy>()
                .map(|_| ())
                .map_err(|_| "compose.confirm must be `ask` or `accept`".to_owned()),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum SettingsCommand {
    /// Show a setting of the current target's daemon.
    Get {
        key: SettingKey,
        /// The value in effect for this project (compose.confirm only).
        #[arg(long, value_name = "P")]
        project: Option<String>,
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: Output,
        /// Give up after this many seconds, including the ssh connection.
        #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
    },
    /// Change a setting of the current target's daemon. Later deployments
    /// use it; nothing running moves.
    #[command(
        after_help = "`compose.confirm accept` waives the confirmation of Compose files: it \
                      asks on a terminal, and needs --confirm without one (exit 3 otherwise)."
    )]
    Set {
        key: SettingKey,
        value: String,
        /// Change this project's own value (compose.confirm only).
        #[arg(long, value_name = "P")]
        project: Option<String>,
        /// Confirm a change that waives a confirmation.
        #[arg(long)]
        confirm: bool,
        /// Give up after this many seconds, including the ssh connection.
        #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
    },
}

pub fn settings(command: SettingsCommand) -> ExitCode {
    match command {
        SettingsCommand::Get {
            key,
            project,
            output,
            timeout,
        } => get(
            key,
            project.as_deref(),
            output,
            Duration::from_secs(timeout),
        ),
        SettingsCommand::Set {
            key,
            value,
            project,
            confirm,
            timeout,
        } => set(
            key,
            &value,
            project.as_deref(),
            confirm,
            Duration::from_secs(timeout),
        ),
    }
}

/// The project of `--project`, or a usage failure.
fn project_slug(project: Option<&str>) -> Result<Option<Slug>, ExitCode> {
    project
        .map(|p| Slug::parse(p).map_err(|e| fail(format!("--project: {e}"), USAGE)))
        .transpose()
}

fn get(key: SettingKey, project: Option<&str>, output: Output, timeout: Duration) -> ExitCode {
    let project = match project_slug(project) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let path = match key.path(project.as_ref()) {
        Ok(p) => p,
        Err(why) => return fail(why, USAGE),
    };
    let endpoint = match endpoint() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let result = rt.block_on(client::get_json::<SettingView>(&endpoint, &path, timeout));
    drop(rt);
    match result {
        Ok(view) => {
            print!("{}", render(&view, output));
            ExitCode::SUCCESS
        }
        Err(e) => fail(e, FAILURE),
    }
}

/// `-o json` is the daemon's view; text is `KEY: VALUE` and what it means.
fn render(view: &SettingView, output: Output) -> String {
    match output {
        Output::Json => {
            let mut s = serde_json::to_string_pretty(view).expect("a view always serialises");
            s.push('\n');
            s
        }
        Output::Text => {
            let when = match view.updated_at {
                Some(at) => format!("set {at}"),
                None => "the default".to_owned(),
            };
            let meaning = match (view.key.as_str(), view.value.as_str()) {
                (PUBLISH_ADDRESS_KEY, "auto") => {
                    "; ports bind the host's tailnet address when it has one, else 127.0.0.1"
                }
                (COMPOSE_CONFIRM_KEY, "ask") => {
                    "; a Compose file's confirmation items are asked for"
                }
                (COMPOSE_CONFIRM_KEY, "accept") => {
                    "; a Compose file's confirmation items are accepted by policy"
                }
                _ => "",
            };
            format!("{}: {} ({when}{meaning})\n", view.key, view.value)
        }
    }
}

/// What setting `key` to `value` waives, when it waives a confirmation.
pub fn waiver(key: SettingKey, value: &str, project: Option<&Slug>) -> Option<String> {
    if key != SettingKey::ComposeConfirm || value != ConfirmPolicy::Accept.as_str() {
        return None;
    }
    let scope = match project {
        Some(p) => format!("project {p}"),
        None => "every project on this host without a value of its own".to_owned(),
    };
    Some(format!(
        "compose.confirm accept waives the confirmation of Compose files for {scope}: \
         bind mounts to host paths, published ports, added capabilities, GPU reservations \
         and restart policies then deploy without asking, and each item is recorded in the \
         audit log as accepted by policy. Rejected files are still rejected."
    ))
}

fn set(
    key: SettingKey,
    value: &str,
    project: Option<&str>,
    confirm: bool,
    timeout: Duration,
) -> ExitCode {
    if let Err(why) = key.check(value) {
        return fail(why, USAGE);
    }
    let project = match project_slug(project) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let path = match key.path(project.as_ref()) {
        Ok(p) => p,
        Err(why) => return fail(why, USAGE),
    };
    if let Some(waived) = waiver(key, value, project.as_ref()) {
        eprintln!("{waived}");
        let confirmed =
            confirm || (crate::push::on_terminal() && crate::push::ask("Waive the confirmation?"));
        if !confirmed {
            return fail(
                "not changed: confirm on a terminal, or pass --confirm",
                CONFIRMATION_REQUIRED,
            );
        }
    }
    let endpoint = match endpoint() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let result = rt.block_on(change(&endpoint, &path, value, timeout));
    drop(rt);
    match result {
        Ok(op) if op.status == OperationStatus::Succeeded => {
            println!("{}: {value}", key.as_str());
            ExitCode::SUCCESS
        }
        Ok(op) => {
            let why = op
                .failure
                .map(|f| format!("{} ({})", f.message, f.code))
                .unwrap_or_else(|| format!("the operation ended {}", op.status));
            fail(format!("{} was not changed: {why}", key.as_str()), FAILURE)
        }
        Err(e) => fail(e, FAILURE),
    }
}

/// Submits the change and follows its operation to its end.
async fn change(
    endpoint: &Endpoint,
    path: &str,
    value: &str,
    timeout: Duration,
) -> Result<OperationView, String> {
    let body = serde_json::to_vec(&SetSettingRequest {
        value: value.to_owned(),
    })
    .expect("a request always serialises");
    submit(endpoint, path, body, "settings", timeout).await
}

/// `POST`s `body` to `path` under a new idempotency key starting with
/// `kind`, and follows the operation it answers to its end.
pub(crate) async fn submit(
    endpoint: &Endpoint,
    path: &str,
    body: Vec<u8>,
    kind: &str,
    timeout: Duration,
) -> Result<OperationView, String> {
    let deadline = Instant::now() + timeout;
    let idempotency_key = new_key(kind);
    let headers = [(IDEMPOTENCY_KEY_HEADER, idempotency_key.as_str())];
    let mut op: OperationView = client::post_json(endpoint, path, &headers, body, timeout)
        .await
        .map_err(|e| e.to_string())?;
    while !matches!(
        op.status,
        OperationStatus::Succeeded | OperationStatus::Failed | OperationStatus::Interrupted
    ) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!(
                "the change was submitted as operation {} but did not end in time",
                op.id
            ));
        }
        tokio::time::sleep(POLL.min(left)).await;
        op = client::get_json(endpoint, &operation_path(op.id), left)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(op)
}

/// A key no other request of this client uses: process id and the time.
fn new_key(kind: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{kind}-{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ship_core::Timestamp;

    #[test]
    fn rendering() {
        let unset = SettingView {
            key: PUBLISH_ADDRESS_KEY.to_owned(),
            value: "auto".to_owned(),
            updated_at: None,
        };
        assert_eq!(
            render(&unset, Output::Text),
            "publish.address: auto (the default; ports bind the host's tailnet address \
             when it has one, else 127.0.0.1)\n"
        );
        let set = SettingView {
            value: "127.0.0.1".to_owned(),
            updated_at: Some(Timestamp::from_unix_millis(1_469_918_176_385).unwrap()),
            ..unset.clone()
        };
        assert_eq!(
            render(&set, Output::Text),
            "publish.address: 127.0.0.1 (set 2016-07-30T22:36:16.385Z)\n"
        );
        let json: serde_json::Value = serde_json::from_str(&render(&set, Output::Json)).unwrap();
        assert_eq!(json["value"], "127.0.0.1");
        assert!(new_key("settings").starts_with("settings-"));
        let confirm = SettingView {
            key: COMPOSE_CONFIRM_KEY.to_owned(),
            value: "accept".to_owned(),
            updated_at: None,
        };
        assert_eq!(
            render(&confirm, Output::Text),
            "compose.confirm: accept (the default; a Compose file's confirmation items are \
             accepted by policy)\n"
        );
    }

    #[test]
    fn keys_values_and_scopes() {
        let p = Slug::parse("my-app").unwrap();
        assert_eq!(
            SettingKey::ComposeConfirm.path(Some(&p)).unwrap(),
            "/api/v1/projects/my-app/settings/compose.confirm"
        );
        assert_eq!(
            SettingKey::ComposeConfirm.path(None).unwrap(),
            "/api/v1/settings/compose.confirm"
        );
        assert!(SettingKey::PublishAddress.path(Some(&p)).is_err());
        assert!(SettingKey::ComposeConfirm.check("ask").is_ok());
        assert!(SettingKey::ComposeConfirm.check("accept").is_ok());
        assert!(SettingKey::ComposeConfirm.check("yes").is_err());
        assert!(SettingKey::PublishAddress.check("accept").is_err());
        assert!(SettingKey::PublishAddress.check("127.0.0.1").is_ok());
    }

    #[test]
    fn only_accept_waives() {
        let p = Slug::parse("my-app").unwrap();
        let w = waiver(SettingKey::ComposeConfirm, "accept", Some(&p)).unwrap();
        assert!(w.contains("project my-app"), "{w}");
        assert!(w.contains("bind mounts"), "{w}");
        assert!(waiver(SettingKey::ComposeConfirm, "accept", None).is_some());
        assert!(waiver(SettingKey::ComposeConfirm, "ask", Some(&p)).is_none());
        assert!(waiver(SettingKey::PublishAddress, "0.0.0.0", None).is_none());
    }
}
