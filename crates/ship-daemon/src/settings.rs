//! Daemon settings over the API: `GET /api/v1/settings/{key}` reads one,
//! `POST` changes it as a `settings` operation (ADR-0011: a mutation is an
//! operation before any side effect).
//!
//! Phase 1 knows `publish.address` (phase-1 plan, decisions): `auto`, or
//! an address of the host, or an unspecified address. A setting lives in
//! the daemon's store; nothing in a pushed directory can change it.

use std::net::{IpAddr, SocketAddr, UdpSocket};

use ship_core::api::{PUBLISH_ADDRESS_KEY, SettingView};
use ship_core::deploy::PublishAddress;
use ship_core::{ApiError, ErrorCode, OperationPhase};
use ship_store::{NewAuditEvent, Setting, Store};

use crate::queue::{Context, JobFailure};

/// Who an audit event names for a change made through the API. Phase 1 has
/// no identity beyond socket access (ADR-0003 rule 2).
pub const OPERATOR: &str = "operator";

/// The settings the daemon knows.
pub const KNOWN: &[&str] = &[PUBLISH_ADDRESS_KEY];

/// The value a setting has when it was never set.
pub fn default_value(key: &str) -> Option<&'static str> {
    (key == PUBLISH_ADDRESS_KEY).then_some("auto")
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

/// Parses a new value for `key`; `400 invalid_request` when it is not one.
pub fn parse_value(key: &str, value: &str) -> Result<PublishAddress, ApiError> {
    check_key(key)?;
    value
        .parse::<PublishAddress>()
        .map_err(|e| ApiError::new(ErrorCode::InvalidRequest, e.to_string()))
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
    key: &str,
    value: PublishAddress,
) -> Result<(), JobFailure> {
    ctx.enter(
        OperationPhase::Validating,
        &format!("checking {key} {value}"),
    )
    .await?;
    if let PublishAddress::Fixed(ip) = value
        && !is_host_address(ip)
    {
        return Err(JobFailure::new(
            ErrorCode::InvalidRequest,
            format!("{ip} is not an address of this host"),
        ));
    }
    let text = value.to_string();
    store.set_host_setting(key, &text).await?;
    store
        .append_audit_event(&NewAuditEvent {
            actor: OPERATOR,
            action: &format!("set {key}"),
            target: &text,
        })
        .await?;
    ctx.log(&format!("{key} is now {text}")).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn keys_and_values() {
        assert!(check_key("publish.address").is_ok());
        assert_eq!(check_key("nope").unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(
            parse_value("publish.address", "auto"),
            Ok(PublishAddress::Auto)
        );
        assert_eq!(
            parse_value("publish.address", "nope").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let unset = view("publish.address", None);
        assert_eq!((unset.value.as_str(), unset.updated_at), ("auto", None));
    }

    #[test]
    fn host_addresses() {
        assert!(is_host_address(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_host_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!is_host_address(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))));
    }
}
