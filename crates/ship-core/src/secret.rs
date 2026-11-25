//! A secret value that cannot be printed by accident.
//!
//! `Debug` and `Display` print `[redacted]`, so a secret inside any struct
//! with a derived `Debug`, a log line or an error message is never revealed.
//! There is no `Serialize`; the only way to the value is [`SecretValue::expose`],
//! which is easy to grep for. Memory is not zeroed on drop; that is not
//! claimed.

use std::fmt;

/// What every formatting of a [`SecretValue`] produces.
pub const REDACTED: &str = "[redacted]";

/// A secret value (ADR-0009). Holds the value; never shows it.
#[derive(Clone)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        SecretValue(value.into())
    }

    /// The value itself. Every call site is a place a secret leaves this
    /// type; keep them few and reviewed.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretValue {
    fn from(value: String) -> Self {
        SecretValue(value)
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALUE: &str = "hunter2-very-secret";

    #[derive(Debug)]
    #[allow(dead_code)]
    struct Holder {
        name: &'static str,
        value: SecretValue,
        list: Vec<SecretValue>,
        maybe: Option<SecretValue>,
    }

    #[test]
    fn no_formatting_reveals_the_value() {
        let s = SecretValue::new(VALUE);
        let holder = Holder {
            name: "DATABASE_URL",
            value: s.clone(),
            list: vec![s.clone()],
            maybe: Some(SecretValue::from(VALUE.to_owned())),
        };
        let outputs = [
            format!("{s}"),
            format!("{s:?}"),
            format!("{s:#?}"),
            format!("{s:>40}"),
            format!("{s:<40?}"),
            s.to_string(),
            format!("{holder:?}"),
            format!("{holder:#?}"),
            format!("{:?}", (&s, "x")),
            format!("{:?}", Some(&s)),
        ];
        for out in &outputs {
            assert!(!out.contains(VALUE), "{out}");
            assert!(!out.contains("hunter2"), "{out}");
            assert!(out.contains(REDACTED), "{out}");
        }
    }

    #[test]
    fn expose_returns_the_value() {
        let s = SecretValue::new(VALUE);
        assert_eq!(s.expose(), VALUE);
        assert_eq!(s.clone().expose(), VALUE);
    }
}
