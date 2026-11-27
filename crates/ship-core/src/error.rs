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
            ErrorCode::Conflict | ErrorCode::IllegalTransition => 409,
            ErrorCode::UnsupportedApiVersion => 426,
            ErrorCode::Internal => 500,
            ErrorCode::DockerUnavailable => 503,
        }
    }
}

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
