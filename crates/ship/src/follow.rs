//! Following an operation over its SSE event stream (ADR-0011 rule 5),
//! resuming from the last sequence received after a dropped connection,
//! so no event is lost and none is shown twice.

use std::time::{Duration, Instant};

use ship_core::OperationId;
use ship_core::api::{EventKind, LAST_EVENT_ID_HEADER, OperationEventView, operation_events_path};

use crate::client::{self, ClientError, Endpoint};

/// How long opening the stream, or one request, may take.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest silence on an open stream before it counts as lost. The
/// daemon sends a keep-alive comment every 15 s.
const IDLE: Duration = Duration::from_secs(45);
/// Pause between attempts to reach the daemon again.
const RETRY: Duration = Duration::from_millis(500);

/// Why following stopped short.
#[derive(Debug)]
pub enum Halt {
    /// The daemon could not be reached again within the reconnect window.
    Lost(String),
    /// The daemon answered with something other than the stream: an
    /// error, another API version, or an answer this build cannot read.
    Client(ClientError),
}

/// Splits an SSE byte stream into the `data` of each complete frame.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    frames: u64,
}

impl SseParser {
    /// Adds `chunk` and returns the data of every frame it completed.
    /// Comments (keep-alives) and `id:` lines are dropped; several `data:`
    /// lines of one frame are joined with newlines.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = find(&self.buf, b"\n\n") {
            let frame: Vec<u8> = self.buf.drain(..end + 2).collect();
            self.frames += 1;
            let text = String::from_utf8_lossy(&frame[..end]);
            let data: Vec<&str> = text
                .lines()
                .filter_map(|l| {
                    l.strip_prefix("data:")
                        .map(|d| d.strip_prefix(' ').unwrap_or(d))
                })
                .collect();
            if !data.is_empty() {
                out.push(data.join("\n"));
            }
        }
        out
    }

    /// Whether some complete frame, keep-alive included, has arrived: the
    /// stream is live, not only its response head.
    pub fn saw_frame(&self) -> bool {
        self.frames > 0
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Whether `e` is a failure to reach the daemon, worth retrying.
pub fn is_transport(e: &ClientError) -> bool {
    matches!(e, ClientError::Transport { .. })
}

/// The events after sequence `last` that were not seen yet, in order;
/// advances `last`. Returns whether the terminal event was among them.
pub fn take_new(
    last: &mut u64,
    events: Vec<OperationEventView>,
    on_event: &mut impl FnMut(&OperationEventView),
) -> bool {
    let mut finished = false;
    for event in events {
        if event.sequence <= *last {
            continue;
        }
        *last = event.sequence;
        on_event(&event);
        if event.event == EventKind::Finished.as_str() {
            finished = true;
        }
    }
    finished
}

/// Follows `operation`'s events after sequence `after` until its terminal
/// event, calling `on_event` once for each, in order. A dropped stream is
/// opened again from the last sequence received, for up to `reconnect`
/// after it was lost. The loss ends only when a reopened stream delivers a
/// complete frame (an answer with no frame, or a truncated one, does not
/// count), and every attempt meanwhile is bounded by the time left.
/// Returns the last sequence.
pub async fn follow(
    endpoint: &Endpoint,
    operation: OperationId,
    after: u64,
    reconnect: Duration,
    mut on_event: impl FnMut(&OperationEventView),
) -> Result<u64, Halt> {
    let path = operation_events_path(operation);
    let mut last = after;
    let mut lost_at: Option<Instant> = None;
    loop {
        let resume = last.to_string();
        let headers = [(LAST_EVENT_ID_HEADER, resume.as_str())];
        let opening = bounded(REQUEST_TIMEOUT, lost_at, reconnect);
        let why = match client::get_stream(endpoint, &path, &headers, opening).await {
            Ok(mut stream) => {
                let mut parser = SseParser::default();
                loop {
                    let idle = bounded(IDLE, lost_at, reconnect);
                    match tokio::time::timeout(idle, stream.next_chunk()).await {
                        Err(_) if lost_at.is_some() => {
                            break "the reopened event stream sent nothing in time".to_owned();
                        }
                        Err(_) => {
                            break format!("the daemon sent nothing for {} s", IDLE.as_secs());
                        }
                        Ok(None) => break "the event stream ended early".to_owned(),
                        Ok(Some(Err(e))) => break e.to_string(),
                        Ok(Some(Ok(chunk))) => {
                            let mut events = Vec::new();
                            for data in parser.feed(&chunk) {
                                let event: OperationEventView = serde_json::from_str(&data)
                                    .map_err(|e| {
                                        Halt::Client(ClientError::BadResponse(format!(
                                            "an operation event: {e}"
                                        )))
                                    })?;
                                events.push(event);
                            }
                            if parser.saw_frame() {
                                lost_at = None;
                            }
                            if take_new(&mut last, events, &mut on_event) {
                                return Ok(last);
                            }
                        }
                    }
                }
            }
            Err(e) if is_transport(&e) => e.to_string(),
            Err(e) => return Err(Halt::Client(e)),
        };
        wait_or_give_up(&mut lost_at, reconnect, why).await?;
    }
}

/// `limit`, or less while the daemon is lost: the time left of the
/// reconnect window that began at `lost_at`.
fn bounded(limit: Duration, lost_at: Option<Instant>, reconnect: Duration) -> Duration {
    match lost_at {
        Some(since) => limit.min(reconnect.saturating_sub(since.elapsed())),
        None => limit,
    }
}

/// After a failed attempt: waits before the next, or gives up once the
/// daemon has been out of reach for `reconnect`.
async fn wait_or_give_up(
    lost_at: &mut Option<Instant>,
    reconnect: Duration,
    why: String,
) -> Result<(), Halt> {
    let since = *lost_at.get_or_insert_with(Instant::now);
    if since.elapsed() >= reconnect {
        return Err(Halt::Lost(why));
    }
    tokio::time::sleep(bounded(RETRY, Some(since), reconnect)).await;
    Ok(())
}

/// `GET path` as `T`, retrying a failure to reach the daemon for up to
/// `reconnect`. `Ok(None)` for `404 not_found`.
pub async fn get_retrying<T: serde::de::DeserializeOwned>(
    endpoint: &Endpoint,
    path: &str,
    reconnect: Duration,
) -> Result<Option<T>, Halt> {
    let mut lost_at = None;
    loop {
        let timeout = bounded(REQUEST_TIMEOUT, lost_at, reconnect);
        match client::get_json::<T>(endpoint, path, timeout).await {
            Ok(value) => return Ok(Some(value)),
            Err(ClientError::Api(e)) if e.code.known() == Some(ship_core::ErrorCode::NotFound) => {
                return Ok(None);
            }
            Err(e) if is_transport(&e) => {
                wait_or_give_up(&mut lost_at, reconnect, e.to_string()).await?;
            }
            Err(e) => return Err(Halt::Client(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ship_core::{OperationPhase, Timestamp};

    #[test]
    fn frames_are_split_across_chunks() {
        let mut p = SseParser::default();
        assert!(p.feed(b"id: 1\ndata: {\"a\"").is_empty());
        assert_eq!(p.feed(b":1}\n\n: keep-alive\n\nid: 2\n"), ["{\"a\":1}"]);
        assert_eq!(p.feed(b"data: x\ndata: y\n\n"), ["x\ny"]);
        assert!(p.feed(b"\n\n").is_empty());
        assert_eq!(p.feed(b"data:z\n\n"), ["z"]);
    }

    fn event(sequence: u64, kind: EventKind) -> OperationEventView {
        OperationEventView {
            operation_id: "op_01aryz6s41tsv4rrffq69g5fav".parse().unwrap(),
            sequence,
            phase: OperationPhase::Building,
            event: kind.as_str().to_owned(),
            message: format!("line {sequence}"),
            ts: Timestamp::from_unix_millis(0).unwrap(),
        }
    }

    #[test]
    fn a_resumed_stream_shows_nothing_twice() {
        let mut last = 0;
        let mut seen = Vec::new();
        let mut record = |e: &OperationEventView| seen.push(e.sequence);
        assert!(!take_new(
            &mut last,
            vec![event(1, EventKind::PhaseStarted), event(2, EventKind::Log)],
            &mut record
        ));
        // A reconnect that replays 2 (it should not) shows it once.
        assert!(take_new(
            &mut last,
            vec![
                event(2, EventKind::Log),
                event(3, EventKind::Log),
                event(4, EventKind::Finished)
            ],
            &mut record
        ));
        assert_eq!(seen, [1, 2, 3, 4]);
        assert_eq!(last, 4);
    }
}
