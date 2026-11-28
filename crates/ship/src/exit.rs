//! The exit codes of `ship push` (`docs/contracts/cli-json.md`). Codes 90
//! to 99 are Ship's own failures; 1 is a failure of what was pushed.

use ship_core::{ErrorCode, ReceivedCode};
use ship_net::client::TransportError;

use crate::client::ClientError;

pub const SUCCEEDED: u8 = 0;
pub const FAILED: u8 = 1;
pub const USAGE: u8 = 2;
pub const CONFIRMATION_REQUIRED: u8 = 3;
pub const DAEMON_UNREACHABLE: u8 = 90;
pub const VERSION_MISMATCH: u8 = 91;
pub const DOCKER_UNAVAILABLE: u8 = 92;
pub const INTERRUPTED: u8 = 93;
pub const CONNECTION_LOST: u8 = 94;
pub const INTERNAL: u8 = 99;

/// The contract's name for an exit code.
pub fn name(code: u8) -> &'static str {
    match code {
        SUCCEEDED => "succeeded",
        FAILED => "failed",
        USAGE => "usage",
        CONFIRMATION_REQUIRED => "confirmation_required",
        DAEMON_UNREACHABLE => "daemon_unreachable",
        VERSION_MISMATCH => "version_mismatch",
        DOCKER_UNAVAILABLE => "docker_unavailable",
        INTERRUPTED => "interrupted",
        CONNECTION_LOST => "connection_lost",
        _ => "internal",
    }
}

/// The exit code for an operation that failed with `code`: 1 for a code
/// that describes what was pushed, the reserved code for Ship's own
/// failures, 99 for a code this build does not know.
pub fn for_failure(code: &ReceivedCode) -> u8 {
    use ErrorCode::*;
    match code.known() {
        Some(
            BuildFailed | BuildTimeout | DeployFailed | PortConflict | HealthCheckFailed
            | NoBuildDefinition | ComposeRejected | InvalidRequest | DigestMismatch
            | PayloadTooLarge | UnsafeSnapshot,
        ) => FAILED,
        Some(ConfirmationRequired) => CONFIRMATION_REQUIRED,
        Some(DockerUnavailable) => DOCKER_UNAVAILABLE,
        Some(Interrupted) => INTERRUPTED,
        Some(UnsupportedApiVersion) => VERSION_MISMATCH,
        _ => INTERNAL,
    }
}

/// The exit code for a request that failed in the client.
pub fn for_client_error(e: &ClientError) -> u8 {
    match e {
        ClientError::Transport { error, .. } => match **error {
            TransportError::BodyTooLarge => INTERNAL,
            _ => DAEMON_UNREACHABLE,
        },
        ClientError::Incompatible(_) => VERSION_MISMATCH,
        ClientError::Api(e) => for_failure(&e.code),
        ClientError::BadResponse(_) => INTERNAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ship_core::ReceivedError;
    use ship_net::ConnectError;
    use std::path::PathBuf;

    fn code(s: &str) -> ReceivedCode {
        serde_json::from_value(serde_json::json!(s)).unwrap()
    }

    #[test]
    fn failure_codes_map_to_the_contract() {
        for pushed in [
            "build_failed",
            "build_timeout",
            "deploy_failed",
            "port_conflict",
            "health_check_failed",
            "no_build_definition",
            "compose_rejected",
            "invalid_request",
            "digest_mismatch",
            "payload_too_large",
            "unsafe_snapshot",
        ] {
            assert_eq!(for_failure(&code(pushed)), FAILED, "{pushed}");
        }
        assert_eq!(
            for_failure(&code("confirmation_required")),
            CONFIRMATION_REQUIRED
        );
        assert_eq!(for_failure(&code("docker_unavailable")), DOCKER_UNAVAILABLE);
        assert_eq!(for_failure(&code("interrupted")), INTERRUPTED);
        for ours in [
            "internal",
            "queue_full",
            "insufficient_storage",
            "brand_new",
        ] {
            assert_eq!(for_failure(&code(ours)), INTERNAL, "{ours}");
        }
    }

    #[test]
    fn client_errors_map_to_the_contract() {
        let endpoint = crate::client::Endpoint {
            name: None,
            target: ship_net::Target::Local {
                socket: PathBuf::from("/nonexistent"),
            },
        };
        let transport = |error| ClientError::Transport {
            endpoint: Box::new(endpoint.clone()),
            error: Box::new(error),
        };
        let no_daemon = transport(TransportError::Connect(ConnectError::NoDaemon {
            socket: PathBuf::from("/nonexistent"),
        }));
        assert_eq!(for_client_error(&no_daemon), DAEMON_UNREACHABLE);
        let timed_out = transport(TransportError::TimedOut {
            after: std::time::Duration::from_secs(1),
        });
        assert_eq!(for_client_error(&timed_out), DAEMON_UNREACHABLE);
        assert_eq!(
            for_client_error(&transport(TransportError::BodyTooLarge)),
            INTERNAL
        );
        assert_eq!(
            for_client_error(&ClientError::Incompatible(String::new())),
            VERSION_MISMATCH
        );
        assert_eq!(
            for_client_error(&ClientError::BadResponse(String::new())),
            INTERNAL
        );
        let api: ReceivedError = serde_json::from_value(serde_json::json!({
            "code": "docker_unavailable", "message": "x", "details": {}, "operation_id": null
        }))
        .unwrap();
        assert_eq!(
            for_client_error(&ClientError::Api(Box::new(api))),
            DOCKER_UNAVAILABLE
        );
    }

    #[test]
    fn names() {
        for (c, n) in [
            (0, "succeeded"),
            (1, "failed"),
            (2, "usage"),
            (3, "confirmation_required"),
            (90, "daemon_unreachable"),
            (91, "version_mismatch"),
            (92, "docker_unavailable"),
            (93, "interrupted"),
            (94, "connection_lost"),
            (99, "internal"),
        ] {
            assert_eq!(name(c), n);
        }
    }
}
