//! Structured JSON logs to stderr, redacted at the logger (OVERVIEW §8).
//!
//! Every log record is formatted in full, then every registered secret
//! value in it is replaced with [`REDACTED`] before any byte reaches the
//! output. A value is matched in its raw form and in the escaped forms the
//! JSON formatter and `Debug` produce, so a secret containing `"` or `\`
//! is caught as well. A JSON record is redacted inside its values, never
//! across its syntax, so it stays parseable.

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
    ///
    /// Each line that parses as JSON is redacted inside its values and
    /// keys, never across its syntax, and re-serialised only if something
    /// was replaced, so a record stays valid JSON whatever the secret is
    /// (a lone `"`, a number). Any other line is redacted as bytes.
    pub fn redact(&self, bytes: &[u8]) -> Vec<u8> {
        let patterns = self.patterns.read().unwrap_or_else(|e| e.into_inner());
        if patterns.is_empty() {
            return bytes.to_vec();
        }
        let mut out = Vec::with_capacity(bytes.len());
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            let (body, newline) = match line.strip_suffix(b"\n") {
                Some(body) => (body, &b"\n"[..]),
                None => (line, &b""[..]),
            };
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(mut value) => {
                    if redact_value(&mut value, &patterns) {
                        out.extend(serde_json::to_vec(&value).expect("a Value always serialises"));
                    } else {
                        out.extend_from_slice(body);
                    }
                }
                Err(_) => out.extend(redact_bytes(body, &patterns)),
            }
            out.extend_from_slice(newline);
        }
        out
    }
}

fn redact_bytes(bytes: &[u8], patterns: &[Vec<u8>]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for pattern in patterns {
        out = replace_all(&out, pattern, REDACTED.as_bytes());
    }
    out
}

/// `s` redacted, or `None` if no pattern occurs in it.
fn redact_str(s: &str, patterns: &[Vec<u8>]) -> Option<String> {
    let out = redact_bytes(s.as_bytes(), patterns);
    if out == s.as_bytes() {
        return None;
    }
    // Patterns and the replacement are whole UTF-8 strings, so matches
    // fall on character boundaries.
    Some(String::from_utf8(out).expect("UTF-8 in, UTF-8 out"))
}

/// Redacts every string, key and scalar in `value`; a number, boolean or
/// null whose text contains a secret becomes a redacted string. Returns
/// whether anything changed.
fn redact_value(value: &mut serde_json::Value, patterns: &[Vec<u8>]) -> bool {
    use serde_json::Value;
    match value {
        Value::String(s) => match redact_str(s, patterns) {
            Some(r) => {
                *s = r;
                true
            }
            None => false,
        },
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, v| redact_value(v, patterns) | changed),
        Value::Object(map) => {
            let mut changed = false;
            let old = std::mem::take(map);
            for (key, mut v) in old {
                changed |= redact_value(&mut v, patterns);
                let key = match redact_str(&key, patterns) {
                    Some(r) => {
                        changed = true;
                        r
                    }
                    None => key,
                };
                map.insert(key, v);
            }
            changed
        }
        Value::Number(_) | Value::Bool(_) | Value::Null => {
            match redact_str(&value.to_string(), patterns) {
                Some(r) => {
                    *value = Value::String(r);
                    true
                }
                None => false,
            }
        }
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

    /// Whether `secret` survives anywhere in `v`: a string, a key, or a
    /// scalar's text.
    fn leaks(v: &serde_json::Value, secret: &str) -> bool {
        use serde_json::Value;
        match v {
            Value::String(s) => s.contains(secret),
            Value::Array(items) => items.iter().any(|i| leaks(i, secret)),
            Value::Object(map) => map
                .iter()
                .any(|(k, v)| k.contains(secret) || leaks(v, secret)),
            other => other.to_string().contains(secret),
        }
    }

    /// Secrets made only of JSON syntax, or of digits, leave every record
    /// parseable and are removed from it (f-005).
    #[test]
    fn json_syntax_and_numeric_secrets_keep_records_parseable() {
        for secret in ["\"", "\\", "\"\\", "4242"] {
            let r = Redactor::new();
            r.register(&SecretValue::new(secret));
            let buf = Buffer::default();
            tracing::subscriber::with_default(subscriber(buf.clone(), r), || {
                tracing::info!(token = secret, "hello {} secret", secret);
                tracing::info!(token = ?secret, "debug {:?}", secret);
                tracing::info!(count = 4242_u64, ratio = 4242.5_f64, "n={}", 4242);
            });
            let text = buf.text();
            assert_eq!(text.lines().count(), 3, "{secret:?}: {text}");
            for line in text.lines() {
                let v: serde_json::Value = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("{secret:?}: {e}: {line}"));
                assert!(v["level"].is_string(), "{secret:?}: {line}");
                assert!(v["fields"]["message"].is_string(), "{secret:?}: {line}");
                assert!(!leaks(&v, secret), "{secret:?} in {line}");
            }
            assert!(text.contains(REDACTED), "{secret:?}: {text}");
        }
    }
}
