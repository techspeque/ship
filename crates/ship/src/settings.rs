//! `ship settings get|set KEY [VALUE]`: daemon settings, held by the
//! daemon and never read from a pushed directory (phase-1 plan,
//! decisions).

use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Subcommand, ValueEnum};
use ship_core::OperationStatus;
use ship_core::api::{
    IDEMPOTENCY_KEY_HEADER, OperationView, PUBLISH_ADDRESS_KEY, SetSettingRequest, SettingView,
    operation_path, setting_path,
};
use ship_core::deploy::PublishAddress;

use crate::client::{self, Endpoint};
use crate::commands::{FAILURE, Output, endpoint, fail, runtime};

/// Exit code for a usage error.
const USAGE: u8 = 2;
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
}

impl SettingKey {
    fn as_str(self) -> &'static str {
        match self {
            SettingKey::PublishAddress => PUBLISH_ADDRESS_KEY,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum SettingsCommand {
    /// Show a setting of the current target's daemon.
    Get {
        key: SettingKey,
        #[arg(short = 'o', long = "output", value_enum, default_value_t)]
        output: Output,
        /// Give up after this many seconds, including the ssh connection.
        #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
    },
    /// Change a setting of the current target's daemon. Later deployments
    /// use it; nothing running moves.
    Set {
        key: SettingKey,
        value: String,
        /// Give up after this many seconds, including the ssh connection.
        #[arg(long, value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
    },
}

pub fn settings(command: SettingsCommand) -> ExitCode {
    match command {
        SettingsCommand::Get {
            key,
            output,
            timeout,
        } => get(key, output, Duration::from_secs(timeout)),
        SettingsCommand::Set {
            key,
            value,
            timeout,
        } => set(key, &value, Duration::from_secs(timeout)),
    }
}

fn get(key: SettingKey, output: Output, timeout: Duration) -> ExitCode {
    let endpoint = match endpoint() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let path = setting_path(key.as_str());
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
            let meaning = if view.key == PUBLISH_ADDRESS_KEY && view.value == "auto" {
                "; ports bind the host's tailnet address when it has one, else 127.0.0.1"
            } else {
                ""
            };
            format!("{}: {} ({when}{meaning})\n", view.key, view.value)
        }
    }
}

fn set(key: SettingKey, value: &str, timeout: Duration) -> ExitCode {
    if let Err(e) = value.parse::<PublishAddress>() {
        return fail(e, USAGE);
    }
    let endpoint = match endpoint() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    let result = rt.block_on(change(&endpoint, key, value, timeout));
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
    key: SettingKey,
    value: &str,
    timeout: Duration,
) -> Result<OperationView, String> {
    let deadline = Instant::now() + timeout;
    let body = serde_json::to_vec(&SetSettingRequest {
        value: value.to_owned(),
    })
    .expect("a request always serialises");
    let idempotency_key = new_key();
    let headers = [(IDEMPOTENCY_KEY_HEADER, idempotency_key.as_str())];
    let path = setting_path(key.as_str());
    let mut op: OperationView = client::post_json(endpoint, &path, &headers, body, timeout)
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
fn new_key() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("settings-{}-{nanos}", std::process::id())
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
        assert_ne!(new_key(), "");
    }
}
