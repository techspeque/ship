//! The typed API error (OVERVIEW §6): `{ "code", "message", "details",
//! "operation_id" }` with stable codes.
//!
//! Messages and details must never carry a secret value; constructors in
//! this crate build them from fixed text and typed values only.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::id::{IdError, OperationId};
use crate::slug::SlugError;
use crate::state::TransitionError;
use crate::time::TimestampError;

string_enum! {
    /// Stable machine-readable error codes. A code's string never changes
    /// once released; new codes may be added (JSON shapes are provisional
    /// until Phase 5, ADR-0004 rule 4).
    pub enum ErrorCode {
        InvalidRequest => "invalid_request",
        InvalidId => "invalid_id",
        InvalidSlug => "invalid_slug",
        InvalidTimestamp => "invalid_timestamp",
        NotFound => "not_found",
        Conflict => "conflict",
        IllegalTransition => "illegal_transition",
        Forbidden => "forbidden",
        UnsupportedApiVersion => "unsupported_api_version",
        DockerUnavailable => "docker_unavailable",
        Internal => "internal",
        // Phase 1. Appended; nothing above moves.
        /// An upload over the size or file-count limit.
        PayloadTooLarge => "payload_too_large",
        /// An uploaded snapshot has an entry unpacking refuses.
        UnsafeSnapshot => "unsafe_snapshot",
        /// An uploaded snapshot's digest differs from the declared one.
        DigestMismatch => "digest_mismatch",
        /// The source has neither a Dockerfile nor a Compose file.
        NoBuildDefinition => "no_build_definition",
        /// The trust profile rejects the Compose file (ADR-0008).
        ComposeRejected => "compose_rejected",
        /// The trust profile needs an operator's confirmation (ADR-0008).
        ConfirmationRequired => "confirmation_required",
        /// An idempotency key reused with a different request (ADR-0011).
        IdempotencyConflict => "idempotency_conflict",
        /// A mutation already runs on the environment (ADR-0011).
        EnvironmentBusy => "environment_busy",
        BuildFailed => "build_failed",
        BuildTimeout => "build_timeout",
        /// A host port the deployment publishes is already in use.
        PortConflict => "port_conflict",
        /// Starting the release's containers failed.
        DeployFailed => "deploy_failed",
        /// The release did not meet its verification criteria.
        HealthCheckFailed => "health_check_failed",
        /// The host's disk is past its watermark.
        InsufficientStorage => "insufficient_storage",
        /// The daemon stopped during the operation; it is never resumed.
        Interrupted => "interrupted",
    }
}

impl ErrorCode {
    /// The HTTP status the daemon answers with for this code.
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::InvalidRequest
            | ErrorCode::InvalidId
            | ErrorCode::InvalidSlug
            | ErrorCode::InvalidTimestamp => 400,
            ErrorCode::Forbidden => 403,
            ErrorCode::NotFound => 404,
            ErrorCode::Conflict
            | ErrorCode::IllegalTransition
            | ErrorCode::EnvironmentBusy
            | ErrorCode::PortConflict => 409,
            ErrorCode::PayloadTooLarge => 413,
            ErrorCode::UnsafeSnapshot
            | ErrorCode::DigestMismatch
            | ErrorCode::NoBuildDefinition
            | ErrorCode::ComposeRejected
            | ErrorCode::IdempotencyConflict
            | ErrorCode::BuildFailed
            | ErrorCode::DeployFailed
            | ErrorCode::HealthCheckFailed => 422,
            ErrorCode::UnsupportedApiVersion => 426,
            ErrorCode::ConfirmationRequired => 428,
            ErrorCode::Internal => 500,
            ErrorCode::DockerUnavailable | ErrorCode::Interrupted => 503,
            ErrorCode::BuildTimeout => 504,
            ErrorCode::InsufficientStorage => 507,
        }
    }
}

/// An error code as a client received it: one this build knows, or the raw
/// string of one added by a newer daemon. Codes may be added at any time
/// (ADR-0004 rule 4), so a client never fails to read an error over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceivedCode {
    Known(ErrorCode),
    Unknown(String),
}

impl ReceivedCode {
    /// The known code, if this build has it.
    pub fn known(&self) -> Option<ErrorCode> {
        match self {
            ReceivedCode::Known(c) => Some(*c),
            ReceivedCode::Unknown(_) => None,
        }
    }

    /// The code exactly as the daemon sent it.
    pub fn as_str(&self) -> &str {
        match self {
            ReceivedCode::Known(c) => c.as_str(),
            ReceivedCode::Unknown(s) => s,
        }
    }
}

impl From<String> for ReceivedCode {
    fn from(s: String) -> Self {
        match s.parse::<ErrorCode>() {
            Ok(c) => ReceivedCode::Known(c),
            Err(_) => ReceivedCode::Unknown(s),
        }
    }
}

impl fmt::Display for ReceivedCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReceivedCode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(ReceivedCode::from)
    }
}

impl Serialize for ReceivedCode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// An [`ApiError`] as a client decodes it: same shape, but an unknown code
/// is kept as its raw string rather than failing the whole body, so the
/// daemon's message, details and operation id are never lost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReceivedError {
    pub code: ReceivedCode,
    pub message: String,
    #[serde(default = "empty_details")]
    pub details: Value,
    #[serde(default)]
    pub operation_id: Option<OperationId>,
}

impl From<ApiError> for ReceivedError {
    fn from(e: ApiError) -> Self {
        ReceivedError {
            code: ReceivedCode::Known(e.code),
            message: e.message,
            details: e.details,
            operation_id: e.operation_id,
        }
    }
}

impl fmt::Display for ReceivedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ReceivedError {}

/// An error as the API returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    /// Structured context; an empty object when there is none.
    #[serde(default = "empty_details")]
    pub details: Value,
    /// The operation this error belongs to, if any. Serialised as `null`
    /// when absent so the shape is always the same four keys.
    #[serde(default)]
    pub operation_id: Option<OperationId>,
}

fn empty_details() -> Value {
    Value::Object(Map::new())
}

impl ApiError {
    /// An error with no details and no operation.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ApiError {
            code,
            message: message.into(),
            details: empty_details(),
            operation_id: None,
        }
    }

    /// Replaces the details.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    /// Attaches the operation the error belongs to.
    pub fn with_operation(mut self, operation_id: OperationId) -> Self {
        self.operation_id = Some(operation_id);
        self
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<IdError> for ApiError {
    fn from(e: IdError) -> Self {
        ApiError::new(ErrorCode::InvalidId, e.to_string())
    }
}

impl From<SlugError> for ApiError {
    fn from(e: SlugError) -> Self {
        ApiError::new(ErrorCode::InvalidSlug, e.to_string())
    }
}

impl From<TimestampError> for ApiError {
    fn from(e: TimestampError) -> Self {
        ApiError::new(ErrorCode::InvalidTimestamp, e.to_string())
    }
}

impl<S: fmt::Display> From<TransitionError<S>> for ApiError {
    fn from(e: TransitionError<S>) -> Self {
        ApiError::new(ErrorCode::IllegalTransition, e.to_string())
            .with_details(serde_json::json!({ "from": e.from.to_string(), "to": e.to.to_string() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DesiredState;

    fn keys(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    #[test]
    fn shape_is_exactly_four_keys() {
        let e = ApiError::new(ErrorCode::NotFound, "project not found");
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(keys(&v), ["code", "details", "message", "operation_id"]);
        assert_eq!(v["code"], "not_found");
        assert_eq!(v["message"], "project not found");
        assert_eq!(v["details"], serde_json::json!({}));
        assert_eq!(v["operation_id"], Value::Null);
    }

    #[test]
    fn operation_id_and_details_round_trip() {
        let op = OperationId::from_parts(1_469_918_176_385, 7).unwrap();
        let e = ApiError::new(ErrorCode::DockerUnavailable, "docker is not reachable")
            .with_details(serde_json::json!({ "phase": "deploying" }))
            .with_operation(op);
        let json = serde_json::to_string(&e).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["operation_id"], op.to_string());
        assert_eq!(serde_json::from_str::<ApiError>(&json).unwrap(), e);
    }

    #[test]
    fn missing_optional_fields_default() {
        let e: ApiError = serde_json::from_str(r#"{"code":"internal","message":"x"}"#).unwrap();
        assert_eq!(e, ApiError::new(ErrorCode::Internal, "x"));
    }

    #[test]
    fn codes_are_stable_unique_snake_case() {
        let expected = [
            "invalid_request",
            "invalid_id",
            "invalid_slug",
            "invalid_timestamp",
            "not_found",
            "conflict",
            "illegal_transition",
            "forbidden",
            "unsupported_api_version",
            "docker_unavailable",
            "internal",
            // Phase 1, appended.
            "payload_too_large",
            "unsafe_snapshot",
            "digest_mismatch",
            "no_build_definition",
            "compose_rejected",
            "confirmation_required",
            "idempotency_conflict",
            "environment_busy",
            "build_failed",
            "build_timeout",
            "port_conflict",
            "deploy_failed",
            "health_check_failed",
            "insufficient_storage",
            "interrupted",
        ];
        let got: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(got, expected);
        let mut dedup = got.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(dedup.len(), got.len());
        for c in ErrorCode::ALL {
            let s = c.as_str();
            assert!(
                s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{s}"
            );
            assert_eq!(serde_json::to_value(c).unwrap(), s);
            assert_eq!(s.parse::<ErrorCode>().unwrap(), *c);
            assert!((400..600).contains(&c.http_status()));
        }
    }

    #[test]
    fn http_statuses() {
        use ErrorCode::*;
        let table = [
            (InvalidRequest, 400),
            (InvalidId, 400),
            (InvalidSlug, 400),
            (InvalidTimestamp, 400),
            (NotFound, 404),
            (Conflict, 409),
            (IllegalTransition, 409),
            (Forbidden, 403),
            (UnsupportedApiVersion, 426),
            (DockerUnavailable, 503),
            (Internal, 500),
            (PayloadTooLarge, 413),
            (UnsafeSnapshot, 422),
            (DigestMismatch, 422),
            (NoBuildDefinition, 422),
            (ComposeRejected, 422),
            (ConfirmationRequired, 428),
            (IdempotencyConflict, 422),
            (EnvironmentBusy, 409),
            (BuildFailed, 422),
            (BuildTimeout, 504),
            (PortConflict, 409),
            (DeployFailed, 422),
            (HealthCheckFailed, 422),
            (InsufficientStorage, 507),
            (Interrupted, 503),
        ];
        assert_eq!(table.len(), ErrorCode::ALL.len());
        for (code, status) in table {
            assert_eq!(code.http_status(), status, "{code}");
        }
        // 426 is the version refusal and nothing else.
        for c in ErrorCode::ALL {
            assert_eq!(c.http_status() == 426, *c == UnsupportedApiVersion, "{c}");
        }
    }

    #[test]
    fn received_error_keeps_an_unknown_code_and_everything_else() {
        let op = OperationId::from_parts(1_469_918_176_385, 7).unwrap();
        let body = format!(
            r#"{{"code":"quota_exceeded","message":"the project is over its quota","details":{{"limit":3}},"operation_id":"{op}"}}"#
        );
        assert!(serde_json::from_str::<ApiError>(&body).is_err());
        let e: ReceivedError = serde_json::from_str(&body).unwrap();
        assert_eq!(e.code, ReceivedCode::Unknown("quota_exceeded".to_owned()));
        assert_eq!(e.code.known(), None);
        assert_eq!(e.code.as_str(), "quota_exceeded");
        assert_eq!(e.message, "the project is over its quota");
        assert_eq!(e.details, serde_json::json!({ "limit": 3 }));
        assert_eq!(e.operation_id, Some(op));
        assert_eq!(
            e.to_string(),
            "quota_exceeded: the project is over its quota"
        );
        // Re-serialises to the same wire shape.
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v, serde_json::from_str::<Value>(&body).unwrap());
    }

    #[test]
    fn received_error_decodes_known_codes_and_defaults() {
        for c in ErrorCode::ALL {
            let e = ApiError::new(*c, "m").with_details(serde_json::json!({ "k": 1 }));
            let json = serde_json::to_string(&e).unwrap();
            let r: ReceivedError = serde_json::from_str(&json).unwrap();
            assert_eq!(r.code.known(), Some(*c));
            assert_eq!(r, ReceivedError::from(e));
        }
        let r: ReceivedError =
            serde_json::from_str(r#"{"code":"internal","message":"x"}"#).unwrap();
        assert_eq!(
            r,
            ReceivedError::from(ApiError::new(ErrorCode::Internal, "x"))
        );
        // Not an error body at all.
        assert!(serde_json::from_str::<ReceivedError>(r#"{"message":"x"}"#).is_err());
        assert!(serde_json::from_str::<ReceivedError>(r#"{"code":7,"message":"x"}"#).is_err());
    }

    #[test]
    fn conversions() {
        let e: ApiError = IdError::WrongLength.into();
        assert_eq!(e.code, ErrorCode::InvalidId);
        let e: ApiError = SlugError::Empty.into();
        assert_eq!(e.code, ErrorCode::InvalidSlug);
        let e: ApiError = TimestampError::Malformed.into();
        assert_eq!(e.code, ErrorCode::InvalidTimestamp);
        let e: ApiError = DesiredState::Removed
            .transition(DesiredState::Running)
            .unwrap_err()
            .into();
        assert_eq!(e.code, ErrorCode::IllegalTransition);
        assert_eq!(
            e.details,
            serde_json::json!({ "from": "removed", "to": "running" })
        );
        assert_eq!(
            e.to_string(),
            "illegal_transition: illegal transition from `removed` to `running`"
        );
    }
}
