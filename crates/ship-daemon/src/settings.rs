//! Daemon settings over the API: `GET /api/v1/settings/{key}` reads one,
//! `POST` changes it as a `settings` operation (ADR-0011: a mutation is an
//! operation before any side effect). A project-scoped setting has the
//! same pair under `/api/v1/projects/{project}/settings/{key}`.
//!
//! Phase 1 knows `publish.address` (host only: `auto`, an address of the
//! host, or an unspecified address) and `compose.confirm` (host or
//! project: `ask` or `accept`; phase-1 plan, decisions). A setting lives
//! in the daemon's store; nothing in a pushed directory can change it.

use std::net::{IpAddr, SocketAddr, UdpSocket};

use serde_json::json;
use ship_core::api::{COMPOSE_CONFIRM_KEY, PUBLISH_ADDRESS_KEY, SettingView};
use ship_core::deploy::PublishAddress;
use ship_core::trust::ConfirmPolicy;
use ship_core::{ApiError, ErrorCode, OperationPhase};
use ship_store::{NewAuditEvent, Setting, SettingScope, Store};

use crate::queue::{Context, JobFailure};

/// Who an audit event names for a change made through the API. Phase 1 has
/// no identity beyond socket access (ADR-0003 rule 2).
pub const OPERATOR: &str = "operator";

/// The settings the daemon knows.
pub const KNOWN: &[&str] = &[PUBLISH_ADDRESS_KEY, COMPOSE_CONFIRM_KEY];

/// The settings a project may set for itself.
pub const PROJECT_SCOPED: &[&str] = &[COMPOSE_CONFIRM_KEY];

/// The value a setting has when it was never set.
pub fn default_value(key: &str) -> Option<&'static str> {
    match key {
        PUBLISH_ADDRESS_KEY => Some("auto"),
        COMPOSE_CONFIRM_KEY => Some(ConfirmPolicy::Ask.as_str()),
        _ => None,
    }
}

/// A checked new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    PublishAddress(PublishAddress),
    ComposeConfirm(ConfirmPolicy),
}

impl Value {
    fn text(self) -> String {
        match self {
            Value::PublishAddress(a) => a.to_string(),
            Value::ComposeConfirm(p) => p.as_str().to_owned(),
        }
    }
}

/// `404 not_found` unless `key` is a known setting.
pub fn check_key(key: &str) -> Result<(), ApiError> {
    if KNOWN.contains(&key) {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorCode::NotFound,
            format!("no such setting; the settings are {}", KNOWN.join(", ")),
        ))
    }
}

/// `404 not_found` unless `key` is a setting a project may have.
pub fn check_project_key(key: &str) -> Result<(), ApiError> {
    if PROJECT_SCOPED.contains(&key) {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorCode::NotFound,
            format!(
                "no such project setting; the project settings are {}",
                PROJECT_SCOPED.join(", ")
            ),
        ))
    }
}

/// Parses a new value for `key`; `400 invalid_request` when it is not one.
pub fn parse_value(key: &str, value: &str) -> Result<Value, ApiError> {
    check_key(key)?;
    let invalid = |why: String| ApiError::new(ErrorCode::InvalidRequest, why);
    match key {
        COMPOSE_CONFIRM_KEY => value
            .parse::<ConfirmPolicy>()
            .map(Value::ComposeConfirm)
            .map_err(|_| invalid(format!("{COMPOSE_CONFIRM_KEY} is `ask` or `accept`"))),
        _ => value
            .parse::<PublishAddress>()
            .map(Value::PublishAddress)
            .map_err(|e| invalid(e.to_string())),
    }
}

/// The view of `key` given what is stored.
pub fn view(key: &str, stored: Option<Setting>) -> SettingView {
    match stored {
        Some(s) => SettingView {
            key: s.key,
            value: s.value,
            updated_at: Some(s.updated_at),
        },
        None => SettingView {
            key: key.to_owned(),
            value: default_value(key).unwrap_or_default().to_owned(),
            updated_at: None,
        },
    }
}

/// Whether `ip` is an address of this host: binding a UDP socket to it
/// (on a port the kernel picks) succeeds only for a local address. An
/// unspecified address means every address and is always accepted.
pub fn is_host_address(ip: IpAddr) -> bool {
    ip.is_unspecified() || UdpSocket::bind(SocketAddr::new(ip, 0)).is_ok()
}

/// The `settings` job: checks a fixed address is the host's, stores the
/// value and records who changed it.
pub async fn apply(
    ctx: &Context,
    store: &Store,
    scope: SettingScope,
    key: &str,
    value: Value,
) -> Result<(), JobFailure> {
    let text = value.text();
    let (scope_name, target, project) = match scope {
        SettingScope::Host => ("host", text.clone(), None),
        SettingScope::Project(p) => ("project", p.to_string(), Some(p.to_string())),
    };
    ctx.enter(
        OperationPhase::Validating,
        &format!("checking {key} {text} for the {scope_name}"),
    )
    .await?;
    if let Value::PublishAddress(PublishAddress::Fixed(ip)) = value
        && !is_host_address(ip)
    {
        return Err(JobFailure::new(
            ErrorCode::InvalidRequest,
            format!("{ip} is not an address of this host"),
        ));
    }
    store.set_setting(scope, key, &text, OPERATOR).await?;
    let detail = json!({
        "key": key,
        "value": text,
        "scope": scope_name,
        "project_id": project,
    })
    .to_string();
    store
        .append_audit_event(&NewAuditEvent {
            actor: OPERATOR,
            action: &format!("set {key}"),
            target: &target,
            detail: Some(&detail),
        })
        .await?;
    let line = match value {
        Value::ComposeConfirm(ConfirmPolicy::Accept) => format!(
            "{key} is now accept for the {scope_name}: Compose files deploy without asking \
             for the items that need confirmation (host paths, published ports, added \
             capabilities, GPU reservations, restart policies); each is recorded in the \
             audit log, and rejected files are still rejected"
        ),
        _ => format!("{key} is now {text} for the {scope_name}"),
    };
    ctx.log(&line).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn keys_and_values() {
        assert!(check_key("publish.address").is_ok());
        assert!(check_key("compose.confirm").is_ok());
        assert_eq!(check_key("nope").unwrap_err().code, ErrorCode::NotFound);
        assert!(check_project_key("compose.confirm").is_ok());
        assert_eq!(
            check_project_key("publish.address").unwrap_err().code,
            ErrorCode::NotFound
        );
        assert_eq!(
            parse_value("publish.address", "auto"),
            Ok(Value::PublishAddress(PublishAddress::Auto))
        );
        assert_eq!(
            parse_value("publish.address", "nope").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            parse_value("compose.confirm", "accept"),
            Ok(Value::ComposeConfirm(ConfirmPolicy::Accept))
        );
        assert_eq!(
            parse_value("compose.confirm", "yes").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let unset = view("publish.address", None);
        assert_eq!((unset.value.as_str(), unset.updated_at), ("auto", None));
        assert_eq!(view("compose.confirm", None).value, "ask");
    }

    #[test]
    fn host_addresses() {
        assert!(is_host_address(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_host_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!is_host_address(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))));
    }
}
