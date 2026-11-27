//! Client requests to the current target, with the API version handshake
//! (OVERVIEW §8) applied to every answer.

use std::fmt;
use std::time::Duration;

use ship_core::{API_VERSION, ErrorCode, ReceivedError, api_version_number};
use ship_net::client::{self, Response, TransportError};
use ship_net::{ConnectError, Target};

/// This client's Ship version.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The daemon a command talks to.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// The target's name; `None` for the implicit local default.
    pub name: Option<String>,
    pub target: Target,
}

impl Endpoint {
    fn describe(&self) -> String {
        match &self.name {
            Some(name) => format!("target `{name}` ({})", self.target.url()),
            None => "the local daemon".to_owned(),
        }
    }
}

/// Why a client request failed.
#[derive(Debug)]
pub enum ClientError {
    Transport {
        endpoint: Box<Endpoint>,
        error: Box<TransportError>,
    },
    /// Client and daemon speak different API versions.
    Incompatible(String),
    /// The daemon answered with an error, possibly with a code this build
    /// does not know; its message is kept either way.
    Api(Box<ReceivedError>),
    /// The daemon's answer could not be understood.
    BadResponse(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Transport { endpoint, error } => {
                write!(f, "{}: {error}", endpoint.describe())?;
                let no_daemon = matches!(
                    **error,
                    TransportError::Connect(ConnectError::NoDaemon { .. })
                );
                if endpoint.name.is_none() && no_daemon {
                    write!(
                        f,
                        "\nTo use a daemon on another machine: \
                         `ship target add NAME ssh://USER@HOST`"
                    )?;
                }
                Ok(())
            }
            ClientError::Incompatible(message) => f.write_str(message),
            ClientError::Api(e) => write!(f, "the daemon refused: {} ({})", e.message, e.code),
            ClientError::BadResponse(why) => write!(f, "unexpected answer from the daemon: {why}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// `GET path` from `endpoint`, checking the API version, and decoding a
/// successful JSON body as `T`.
pub async fn get_json<T: serde::de::DeserializeOwned>(
    endpoint: &Endpoint,
    path: &str,
    timeout: Duration,
) -> Result<T, ClientError> {
    let response = client::get(&endpoint.target, path, timeout)
        .await
        .map_err(|error| ClientError::Transport {
            endpoint: Box::new(endpoint.clone()),
            error: Box::new(error),
        })?;
    check_version(&response)?;
    if !(200..300).contains(&response.status) {
        return Err(decode_error(&response));
    }
    serde_json::from_slice(&response.body).map_err(|e| ClientError::BadResponse(e.to_string()))
}

/// `POST path` with a JSON `body` and `headers` to `endpoint`, checking
/// the API version, and decoding a successful JSON answer as `T`.
pub async fn post_json<T: serde::de::DeserializeOwned>(
    endpoint: &Endpoint,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
    timeout: Duration,
) -> Result<T, ClientError> {
    let mut all = vec![("content-type", "application/json")];
    all.extend_from_slice(headers);
    let response = client::post(
        &endpoint.target,
        path,
        &all,
        client::RequestBody::bytes(body),
        timeout,
    )
    .await
    .map_err(|error| ClientError::Transport {
        endpoint: Box::new(endpoint.clone()),
        error: Box::new(error),
    })?;
    check_version(&response)?;
    if !(200..300).contains(&response.status) {
        return Err(decode_error(&response));
    }
    serde_json::from_slice(&response.body).map_err(|e| ClientError::BadResponse(e.to_string()))
}

/// `POST path` with `headers` and a streamed `body`, checking the API
/// version; the answer whatever its status, for a caller that reads an
/// error body itself.
pub async fn post_stream(
    endpoint: &Endpoint,
    path: &str,
    headers: &[(&str, &str)],
    body: client::RequestBody,
    timeout: Duration,
) -> Result<Response, ClientError> {
    let response = client::post(&endpoint.target, path, headers, body, timeout)
        .await
        .map_err(|error| ClientError::Transport {
            endpoint: Box::new(endpoint.clone()),
            error: Box::new(error),
        })?;
    check_version(&response)?;
    Ok(response)
}

/// Largest error body read from a streamed answer.
const MAX_STREAM_ERROR: usize = 64 * 1024;

/// `GET path` with `headers` as a stream (SSE), checking the API version
/// once the head arrived within `timeout`. A non-2xx answer is read whole
/// and decoded as an error.
pub async fn get_stream(
    endpoint: &Endpoint,
    path: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<client::StreamResponse, ClientError> {
    let transport = |error| ClientError::Transport {
        endpoint: Box::new(endpoint.clone()),
        error: Box::new(error),
    };
    let mut stream = client::get_stream(&endpoint.target, path, headers, timeout)
        .await
        .map_err(transport)?;
    let head = stream.head.clone();
    if (200..300).contains(&head.status) {
        check_parts(
            head.status,
            head.api_version.as_deref(),
            head.daemon_version.as_deref(),
            b"",
        )?;
        return Ok(stream);
    }
    let mut body = Vec::new();
    let read = tokio::time::timeout(timeout, async {
        while let Some(chunk) = stream.next_chunk().await {
            let chunk = chunk?;
            body.extend_from_slice(&chunk);
            if body.len() > MAX_STREAM_ERROR {
                break;
            }
        }
        Ok::<(), TransportError>(())
    })
    .await;
    match read {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(transport(e)),
        Err(_) => return Err(transport(TransportError::TimedOut { after: timeout })),
    }
    let response = Response {
        status: head.status,
        api_version: head.api_version,
        daemon_version: head.daemon_version,
        body: bytes::Bytes::from(body),
    };
    check_version(&response)?;
    Err(decode_error(&response))
}

/// The error for a non-2xx answer: the daemon's error body, whatever its
/// code, or the bare status when the body is not one.
pub fn decode_error(response: &Response) -> ClientError {
    match serde_json::from_slice::<ReceivedError>(&response.body) {
        Ok(e) => ClientError::Api(Box::new(e)),
        Err(_) => ClientError::BadResponse(format!("HTTP {}", response.status)),
    }
}

/// Fails unless the daemon speaks this client's API version.
///
/// A 426 is a refusal whatever its body says, and so is an error body whose
/// code is `unsupported_api_version` on any status: a body this build
/// cannot fully read is never taken as agreement.
fn check_version(response: &Response) -> Result<(), ClientError> {
    check_parts(
        response.status,
        response.api_version.as_deref(),
        response.daemon_version.as_deref(),
        &response.body,
    )
}

fn check_parts(
    status: u16,
    daemon: Option<&str>,
    daemon_version: Option<&str>,
    body: &[u8],
) -> Result<(), ClientError> {
    let refused = status == 426
        || serde_json::from_slice::<ReceivedError>(body)
            .is_ok_and(|e| e.code.known() == Some(ErrorCode::UnsupportedApiVersion));
    if !refused && daemon == Some(API_VERSION) {
        return Ok(());
    }
    Err(ClientError::Incompatible(incompatibility(
        API_VERSION,
        CLIENT_VERSION,
        daemon,
        daemon_version,
    )))
}

/// The message for an incompatible client and daemon, naming the side to
/// upgrade and how.
pub fn incompatibility(
    client_api: &str,
    client_version: &str,
    daemon_api: Option<&str>,
    daemon_version: Option<&str>,
) -> String {
    let daemon_api_text = daemon_api.unwrap_or("an unknown version");
    let head = format!(
        "this client (ship {client_version}) speaks API {client_api}, \
         but the daemon speaks {daemon_api_text}."
    );
    let client_n = api_version_number(client_api);
    let daemon_n = daemon_api.and_then(api_version_number);
    let client_older = matches!((client_n, daemon_n), (Some(c), Some(d)) if c < d);
    if client_older {
        let release = daemon_version
            .map(|v| format!("ship {v} or later"))
            .unwrap_or_else(|| "the daemon's release".to_owned());
        format!(
            "{head} Upgrade this client to {release}; `ship --version` on the \
             daemon's host shows its release."
        )
    } else {
        format!(
            "{head} Upgrade ship on the daemon's host to {client_version} or later \
             and restart the daemon (`sudo systemctl restart ship`)."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn response(status: u16, api: Option<&str>, body: &str) -> Response {
        Response {
            status,
            api_version: api.map(str::to_owned),
            daemon_version: Some("0.9.0".to_owned()),
            body: Bytes::from(body.to_owned()),
        }
    }

    #[test]
    fn matching_version_passes() {
        assert!(check_version(&response(200, Some(API_VERSION), "{}")).is_ok());
        assert!(check_version(&response(404, Some(API_VERSION), "{}")).is_ok());
    }

    #[test]
    fn older_client_is_told_to_upgrade_itself() {
        let m = incompatibility("v1", "0.1.0", Some("v2"), Some("0.9.0"));
        assert!(m.contains("API v1"), "{m}");
        assert!(m.contains("speaks v2"), "{m}");
        assert!(
            m.contains("Upgrade this client to ship 0.9.0 or later"),
            "{m}"
        );
    }

    #[test]
    fn older_daemon_is_told_to_upgrade() {
        let m = incompatibility("v2", "0.9.0", Some("v1"), Some("0.1.0"));
        assert!(
            m.contains("Upgrade ship on the daemon's host to 0.9.0"),
            "{m}"
        );
        assert!(m.contains("systemctl restart ship"), "{m}");
        let m = incompatibility("v1", "0.1.0", None, None);
        assert!(m.contains("an unknown version"), "{m}");
        assert!(m.contains("Upgrade ship on the daemon's host"), "{m}");
    }

    #[test]
    fn mismatched_or_missing_header_fails() {
        let e = check_version(&response(200, Some("v2"), "{}")).unwrap_err();
        assert!(matches!(e, ClientError::Incompatible(_)), "{e:?}");
        assert!(e.to_string().contains("Upgrade this client"), "{e}");
        let e = check_version(&response(200, None, "{}")).unwrap_err();
        assert!(matches!(e, ClientError::Incompatible(_)), "{e:?}");
    }

    #[test]
    fn refusal_426_fails_even_with_our_header() {
        let body =
            r#"{"code":"unsupported_api_version","message":"x","details":{},"operation_id":null}"#;
        let e = check_version(&response(426, Some(API_VERSION), body)).unwrap_err();
        assert!(matches!(e, ClientError::Incompatible(_)), "{e:?}");
    }

    #[test]
    fn refusal_426_with_an_unknown_or_unreadable_body_fails() {
        let unknown =
            r#"{"code":"api_version_retired","message":"x","details":{},"operation_id":null}"#;
        for body in [unknown, "{}", "not json", ""] {
            let e = check_version(&response(426, Some(API_VERSION), body)).unwrap_err();
            assert!(matches!(e, ClientError::Incompatible(_)), "{body}: {e:?}");
        }
    }

    #[test]
    fn unsupported_api_version_code_fails_on_any_status() {
        let body =
            r#"{"code":"unsupported_api_version","message":"x","details":{},"operation_id":null}"#;
        for status in [200, 400, 409, 500] {
            let e = check_version(&response(status, Some(API_VERSION), body)).unwrap_err();
            assert!(matches!(e, ClientError::Incompatible(_)), "{status}: {e:?}");
        }
    }

    #[test]
    fn unknown_code_is_decoded_with_the_daemons_message() {
        let body = r#"{"code":"quota_exceeded","message":"the project is over its quota","details":{"limit":3},"operation_id":null}"#;
        let r = response(409, Some(API_VERSION), body);
        assert!(check_version(&r).is_ok());
        let e = decode_error(&r);
        let ClientError::Api(api) = &e else {
            panic!("{e:?}");
        };
        assert_eq!(api.code.as_str(), "quota_exceeded");
        assert_eq!(api.code.known(), None);
        assert_eq!(api.details, serde_json::json!({ "limit": 3 }));
        assert_eq!(
            e.to_string(),
            "the daemon refused: the project is over its quota (quota_exceeded)"
        );
    }

    #[test]
    fn non_error_body_is_a_bad_response() {
        let e = decode_error(&response(500, Some(API_VERSION), "oops"));
        assert!(matches!(e, ClientError::BadResponse(_)), "{e:?}");
        assert_eq!(e.to_string(), "unexpected answer from the daemon: HTTP 500");
    }
}
