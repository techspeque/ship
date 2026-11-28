//! `GET /api/v1/projects/{project}/logs`: what a project's services wrote,
//! as NDJSON, one [`LogLineView`] per line, stdout and stderr kept apart.
//!
//! The project, environment and service are checked, and Docker asked once,
//! before anything streams, so those failures are ordinary error answers.
//! Each service's containers are read by `ship-docker` (one `container
//! logs` per container, selected by Ship's labels) in a task that is
//! aborted, killing its readers, when the answer is dropped: the client
//! went away, or the daemon is closing. Every line passes through the
//! [`Redactor`] before it is sent.

use std::convert::Infallible;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures_util::future::join_all;
use ship_core::api::{LogLineView, parse_logs_query};
use ship_core::{ApiError, ErrorCode, Timestamp};
use ship_docker::{ComposeProject, Line, LogOptions, Runner, Selector, Stream};
use ship_store::{DEVELOPMENT, Store};
use tokio::sync::{mpsc, watch};

use crate::http::error_response;
use crate::log::Redactor;

/// How long a log read without `follow` may take.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Silence after which a blank line is sent.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// The NDJSON media type.
pub const NDJSON: &str = "application/x-ndjson";

/// What the logs route needs from the daemon.
pub struct LogsContext<'a> {
    pub store: &'a Store,
    pub docker: &'a Runner,
    pub redactor: &'a Redactor,
    pub closing: watch::Receiver<bool>,
}

/// Answers a logs request for `project` with `query` (`env`, `service`,
/// `follow`, `tail`).
pub async fn handle(cx: LogsContext<'_>, project: &str, query: Option<&str>) -> Response {
    let options = match parse_logs_query(query) {
        Ok(options) => options,
        Err(why) => return error_response(ApiError::new(ErrorCode::InvalidRequest, why)),
    };
    let env = options.env.as_deref().unwrap_or(DEVELOPMENT);
    let scope =
        match crate::config::find_scope(cx.store, project, Some(env), options.service.as_deref())
            .await
        {
            Ok(scope) => scope,
            Err(e) => return error_response(e),
        };
    let Some(environment) = scope.environment else {
        return error_response(ApiError::new(ErrorCode::Internal, "no environment"));
    };
    let services: Vec<String> = match scope.service {
        Some(s) => vec![s.name],
        None => match cx.store.list_services(environment.id).await {
            Ok(list) => list.into_iter().map(|s| s.name).collect(),
            Err(e) => {
                tracing::error!(error = %e, "reading the services failed");
                return error_response(ApiError::new(
                    ErrorCode::Internal,
                    "the daemon could not read its database",
                ));
            }
        },
    };
    if let Err(e) = ship_docker::containers(cx.docker, &Selector::environment(environment.id)).await
    {
        tracing::warn!(error = %e, "logs: Docker could not be read");
        let error = if e.is_unreachable() {
            ApiError::new(
                ErrorCode::DockerUnavailable,
                format!("Docker is not reachable: {e}"),
            )
        } else {
            ApiError::new(ErrorCode::Internal, "Docker's answer could not be read")
        };
        return error_response(error);
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(read(
        cx.docker.clone(),
        ComposeProject::new(environment.id),
        services,
        LogOptions {
            follow: options.follow,
            tail: options.tail,
        },
        tx,
    ));
    let state = Lines {
        rx,
        _task: AbortOnDrop(task),
        closing: cx.closing,
        redactor: cx.redactor.clone(),
    };
    let stream = futures_util::stream::unfold(state, next_line);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, NDJSON)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| error_response(ApiError::new(ErrorCode::Internal, "could not stream")))
}

/// Reads every service's lines into `out`: as they come with `follow`;
/// otherwise all of them, then sorted by time (stably, so each container's
/// order is kept), then sent.
async fn read(
    docker: Runner,
    project: ComposeProject,
    services: Vec<String>,
    options: LogOptions,
    out: mpsc::UnboundedSender<LogLineView>,
) {
    let timeout = (!options.follow).then_some(READ_TIMEOUT);
    let (raw, mut lines) = mpsc::unbounded_channel::<LogLineView>();
    let readers = join_all(services.into_iter().map(|service| {
        let raw = raw.clone();
        let docker = &docker;
        let project = &project;
        async move {
            let mut sink = |line: Line| {
                let _ = raw.send(parse_line(&service, &line));
            };
            let read = ship_docker::compose::logs(
                docker,
                project,
                std::slice::from_ref(&service),
                options,
                timeout,
                &mut sink,
            )
            .await;
            if let Err(e) = read {
                tracing::warn!(service = %service, error = %e, "reading a service's logs failed");
            }
        }
    }));
    drop(raw);
    if options.follow {
        let forward = async {
            while let Some(line) = lines.recv().await {
                if out.send(line).is_err() {
                    return;
                }
            }
        };
        tokio::join!(readers, forward);
    } else {
        readers.await;
        let mut all = Vec::new();
        while let Ok(line) = lines.try_recv() {
            all.push(line);
        }
        all.sort_by_key(|l| l.ts);
        for line in all {
            if out.send(line).is_err() {
                return;
            }
        }
    }
}

/// One line from `ship-docker` (`<container>  | <timestamp> <text>`) as a
/// [`LogLineView`] of `service`. A line without Docker's timestamp keeps
/// all its text and gets the time it was read.
pub fn parse_line(service: &str, line: &Line) -> LogLineView {
    let rest = line
        .text
        .split_once("  | ")
        .map_or(line.text.as_str(), |(_, rest)| rest);
    let (ts, text) = match rest.split_once(' ').map(|(ts, text)| (docker_ts(ts), text)) {
        Some((Some(ts), text)) => (ts, text),
        _ => match docker_ts(rest) {
            // A timestamp and nothing after it: an empty line.
            Some(ts) => (ts, ""),
            None => (now(), rest),
        },
    };
    LogLineView {
        service: service.to_owned(),
        stream: match line.stream {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
        .to_owned(),
        ts,
        line: text.to_owned(),
    }
}

/// Docker's RFC 3339 timestamp in UTC (`2026-01-02T03:04:46.120123456Z`,
/// trailing zeros of the fraction dropped), cut to milliseconds.
pub fn docker_ts(text: &str) -> Option<Timestamp> {
    let body = text.strip_suffix('Z')?;
    let (seconds, fraction) = body.split_once('.').unwrap_or((body, ""));
    if seconds.len() != 19 || fraction.len() > 9 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut ms: String = fraction.chars().take(3).collect();
    while ms.len() < 3 {
        ms.push('0');
    }
    format!("{seconds}.{ms}Z").parse().ok()
}

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    Timestamp::from_unix_millis(ms)
        .unwrap_or_else(|_| Timestamp::from_unix_millis(0).expect("the epoch is a valid timestamp"))
}

/// Aborts the reading task when the answer is dropped; its readers'
/// process groups are killed by `ship-docker`'s own guards.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Lines {
    rx: mpsc::UnboundedReceiver<LogLineView>,
    _task: AbortOnDrop,
    closing: watch::Receiver<bool>,
    redactor: Redactor,
}

async fn next_line(mut s: Lines) -> Option<(Result<Bytes, Infallible>, Lines)> {
    if *s.closing.borrow() {
        return None;
    }
    let next = tokio::select! {
        line = s.rx.recv() => line.map(Some),
        _ = s.closing.wait_for(|c| *c) => None,
        // A blank line, which readers skip: writing it is how a client
        // that went away is noticed while its services are quiet.
        () = tokio::time::sleep(KEEPALIVE) => Some(None),
    };
    let Some(line) = next? else {
        return Some((Ok(Bytes::from_static(b"\n")), s));
    };
    let line = LogLineView {
        line: String::from_utf8_lossy(&s.redactor.redact(line.line.as_bytes())).into_owned(),
        ..line
    };
    let mut json = serde_json::to_string(&line).expect("a log line always serialises");
    json.push('\n');
    Some((Ok(Bytes::from(json)), s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(stream: Stream, text: &str) -> Line {
        Line {
            stream,
            text: text.to_owned(),
        }
    }

    #[test]
    fn docker_timestamps_are_cut_to_milliseconds() {
        let cases = [
            ("2026-01-02T03:04:46.120123456Z", "2026-01-02T03:04:46.120Z"),
            ("2026-01-02T03:04:46.12Z", "2026-01-02T03:04:46.120Z"),
            ("2026-01-02T03:04:46Z", "2026-01-02T03:04:46.000Z"),
            ("2026-01-02T03:04:46.999999999Z", "2026-01-02T03:04:46.999Z"),
        ];
        for (docker, wire) in cases {
            assert_eq!(docker_ts(docker).unwrap().to_string(), wire, "{docker}");
        }
        for bad in [
            "",
            "Z",
            "2026-01-02T03:04:46",
            "2026-01-02T03:04:46.1x2Z",
            "2026-01-02T03:04:46+01:00",
            "2026-01-02T03:04:46.1234567890Z",
            "listening",
        ] {
            assert_eq!(docker_ts(bad), None, "{bad}");
        }
    }

    #[test]
    fn lines_are_split_into_time_and_text() {
        let got = parse_line(
            "web",
            &line(
                Stream::Stderr,
                "web-1  | 2026-01-02T03:04:47.003Z warning: no cache directory",
            ),
        );
        assert_eq!(
            got,
            LogLineView {
                service: "web".to_owned(),
                stream: "stderr".to_owned(),
                ts: "2026-01-02T03:04:47.003Z".parse().unwrap(),
                line: "warning: no cache directory".to_owned(),
            }
        );
        // Text after the time keeps its own separators and spaces.
        let got = parse_line(
            "web",
            &line(Stream::Stdout, "web-1  | 2026-01-02T03:04:47Z  a  | b "),
        );
        assert_eq!(got.line, " a  | b ");
        assert_eq!(got.stream, "stdout");
        // A timestamp alone is an empty line.
        let got = parse_line(
            "web",
            &line(Stream::Stdout, "web-1  | 2026-01-02T03:04:47Z"),
        );
        assert_eq!(got.line, "");
        // No timestamp: the whole text, stamped now.
        let got = parse_line("web", &line(Stream::Stdout, "web-1  | no time here"));
        assert_eq!(got.line, "no time here");
    }
}
