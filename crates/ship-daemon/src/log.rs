//! Structured JSON logs to stderr, redacted at the logger (OVERVIEW §8).
//!
//! Every log record is formatted in full, then every registered secret
//! value in it is replaced with [`REDACTED`] before any byte reaches the
//! output. A value is matched in its raw form and in the escaped forms the
//! JSON formatter and `Debug` produce, so a secret containing `"` or `\`
//! is caught as well.

use std::io::{self, Write};
use std::sync::{Arc, RwLock};

use ship_core::SecretValue;
use ship_core::secret::REDACTED;
use tracing_subscriber::fmt::MakeWriter;

/// The set of secret values the logger must never print.
///
/// Cheap to clone; clones share one set.
#[derive(Clone, Default)]
pub struct Redactor {
    /// Byte patterns to replace, longest first.
    patterns: Arc<RwLock<Vec<Vec<u8>>>>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.patterns.read().map(|p| p.len()).unwrap_or(0);
        f.debug_struct("Redactor").field("patterns", &n).finish()
    }
}

impl Redactor {
    /// An empty set.
    pub fn new() -> Self {
        Redactor::default()
    }

    /// Registers a secret value. An empty value is ignored.
    pub fn register(&self, secret: &SecretValue) {
        let raw = secret.expose();
        if raw.is_empty() {
            return;
        }
        let mut forms = vec![raw.to_owned()];
        let json = json_escape(raw);
        let debug = debug_escape(raw);
        forms.push(json_escape(&debug));
        forms.push(json);
        forms.push(debug);

        let mut patterns = self.patterns.write().unwrap_or_else(|e| e.into_inner());
        for form in forms {
            let form = form.into_bytes();
            if !patterns.contains(&form) {
                patterns.push(form);
            }
        }
        patterns.sort_by_key(|p| std::cmp::Reverse(p.len()));
    }

    /// `bytes` with every registered value replaced.
    pub fn redact(&self, bytes: &[u8]) -> Vec<u8> {
        let patterns = self.patterns.read().unwrap_or_else(|e| e.into_inner());
        let mut out = bytes.to_vec();
        for pattern in patterns.iter() {
            out = replace_all(&out, pattern, REDACTED.as_bytes());
        }
        out
    }
}

/// `s` as it appears inside a JSON string, without the quotes.
fn json_escape(s: &str) -> String {
    let quoted = serde_json::to_string(s).expect("a str always serialises");
    quoted[1..quoted.len() - 1].to_owned()
}

/// `s` as `{:?}` prints it, without the quotes.
fn debug_escape(s: &str) -> String {
    let quoted = format!("{s:?}");
    quoted[1..quoted.len() - 1].to_owned()
}

fn replace_all(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// A [`MakeWriter`] that redacts each record before writing it to `W`.
#[derive(Clone)]
pub struct RedactingMakeWriter<M> {
    inner: M,
    redactor: Redactor,
}

impl<M> RedactingMakeWriter<M> {
    pub fn new(inner: M, redactor: Redactor) -> Self {
        RedactingMakeWriter { inner, redactor }
    }
}

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for RedactingMakeWriter<M> {
    type Writer = RecordWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RecordWriter {
            inner: self.inner.make_writer(),
            redactor: self.redactor.clone(),
            buf: Vec::new(),
        }
    }
}

/// Buffers one record; redacts and writes it when dropped.
pub struct RecordWriter<W: Write> {
    inner: W,
    redactor: Redactor,
    buf: Vec<u8>,
}

impl<W: Write> Write for RecordWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    /// Buffered bytes are held until drop, so a secret split across
    /// writes is still whole when redacted.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write> Drop for RecordWriter<W> {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let out = self.redactor.redact(&self.buf);
        let _ = self.inner.write_all(&out);
        let _ = self.inner.flush();
    }
}

/// Builds the daemon's JSON subscriber writing through `make_writer`.
pub fn subscriber<M>(make_writer: M, redactor: Redactor) -> impl tracing::Subscriber + Send + Sync
where
    M: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .with_target(true)
        .with_max_level(tracing::Level::INFO)
        .with_writer(RedactingMakeWriter::new(make_writer, redactor))
        .finish()
}

/// Installs the daemon's logger globally: JSON to stderr, redacted.
pub fn init(redactor: Redactor) -> Result<(), tracing::subscriber::SetGlobalDefaultError> {
    tracing::subscriber::set_global_default(subscriber(io::stderr, redactor))
}

/// Logs a fatal error through the installed logger, for callers that do
/// not depend on `tracing` themselves.
pub fn error(message: &str) {
    tracing::error!(error = message, "daemon failed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const SECRET: &str = "s3cr3t-\"quoted\"-back\\slash-value";

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;
        fn make_writer(&'a self) -> Buffer {
            self.clone()
        }
    }

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn redactor() -> Redactor {
        let r = Redactor::new();
        r.register(&SecretValue::new(SECRET));
        r
    }

    #[test]
    fn redacts_raw_and_escaped_forms() {
        let r = redactor();
        for text in [
            SECRET.to_owned(),
            json_escape(SECRET),
            debug_escape(SECRET),
            json_escape(&debug_escape(SECRET)),
        ] {
            let input = format!("before {text} after");
            let out = String::from_utf8(r.redact(input.as_bytes())).unwrap();
            assert_eq!(out, format!("before {REDACTED} after"));
        }
    }

    #[test]
    fn empty_secret_is_ignored() {
        let r = Redactor::new();
        r.register(&SecretValue::new(""));
        assert_eq!(r.redact(b"unchanged"), b"unchanged");
    }

    #[test]
    fn record_split_across_writes_is_redacted() {
        let buf = Buffer::default();
        let make = RedactingMakeWriter::new(buf.clone(), redactor());
        {
            let mut w = make.make_writer();
            let (a, b) = SECRET.split_at(5);
            w.write_all(b"x=").unwrap();
            w.write_all(a.as_bytes()).unwrap();
            w.flush().unwrap();
            w.write_all(b.as_bytes()).unwrap();
            w.write_all(b"\n").unwrap();
        }
        assert_eq!(buf.text(), format!("x={REDACTED}\n"));
    }

    #[test]
    fn json_records_never_contain_the_secret() {
        let buf = Buffer::default();
        let sub = subscriber(buf.clone(), redactor());
        tracing::subscriber::with_default(sub, || {
            tracing::info!(token = SECRET, "display {}", SECRET);
            tracing::warn!(token = ?SECRET, "debug {:?}", SECRET);
        });
        let text = buf.text();
        assert_eq!(text.lines().count(), 2, "{text}");
        for line in text.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v["level"].is_string(), "{line}");
            assert!(v["fields"]["message"].is_string(), "{line}");
        }
        assert!(!text.contains("s3cr3t"), "{text}");
        assert!(text.contains(REDACTED), "{text}");
        assert!(text.contains("display "), "{text}");
    }
}
