//! Config values and secrets (ADR-0009): the rules for their names and
//! values, the precedence of their scopes, and the env-file line a value is
//! handed to Compose in.
//!
//! A config value is non-secret and readable through the API; a secret is
//! write-only. Both become environment variables of the services they
//! apply to, so both share one namespace and one set of rules.

use std::collections::BTreeMap;
use std::fmt;

string_enum! {
    /// Where a config value or a secret is set. A service sees the values
    /// of its project, its environment and itself.
    pub enum ConfigScope {
        Project => "project",
        Environment => "environment",
        Service => "service",
    }
}

/// Longest name of a config value or secret, in bytes (the schema's
/// limit).
pub const MAX_NAME_LEN: usize = 256;
/// Longest value, in bytes.
pub const MAX_VALUE_LEN: usize = 64 * 1024;

/// Shortest secret value, in bytes. Every registered value is replaced
/// wherever it occurs in a log line, so a shorter one would mangle every
/// line that happens to contain it; such a value is a config value.
pub const MIN_SECRET_LEN: usize = 4;

/// Why a name or value is refused. Never quotes the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// Not an environment variable name.
    Name,
    /// A secret shorter than [`MIN_SECRET_LEN`].
    TooShort,
    /// Longer than [`MAX_VALUE_LEN`].
    TooLong,
    /// Holds a newline, a carriage return or a NUL.
    Multiline,
    /// Ends with a backslash, which Compose's env-file parser reads as
    /// escaping the closing quote.
    TrailingBackslash,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ConfigError::Name => {
                "a name is an environment variable name: a letter or `_`, then letters, \
                 digits or `_`, at most 256 bytes"
            }
            ConfigError::TooShort => {
                "a secret is at least 4 bytes: the logs redact every occurrence of a secret, \
                 so a shorter one would mangle them; set a value that short with `ship vars`"
            }
            ConfigError::TooLong => "a value is at most 64 KiB",
            ConfigError::Multiline => {
                "a value is one line: no newline, carriage return or NUL (Phase 1)"
            }
            ConfigError::TrailingBackslash => {
                "a value cannot end with a backslash (Compose's env-file format cannot hold it)"
            }
        })
    }
}

impl std::error::Error for ConfigError {}

/// Checks a config value's or secret's name: an environment variable name.
pub fn check_name(name: &str) -> Result<(), ConfigError> {
    let mut bytes = name.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
    if first_ok
        && name.len() <= MAX_NAME_LEN
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        Ok(())
    } else {
        Err(ConfigError::Name)
    }
}

/// Checks a value: one line, at most [`MAX_VALUE_LEN`], not ending in `\`.
/// An empty value is allowed.
pub fn check_value(value: &str) -> Result<(), ConfigError> {
    if value.len() > MAX_VALUE_LEN {
        return Err(ConfigError::TooLong);
    }
    if value.bytes().any(|b| matches!(b, b'\n' | b'\r' | b'\0')) {
        return Err(ConfigError::Multiline);
    }
    if value.ends_with('\\') {
        return Err(ConfigError::TrailingBackslash);
    }
    Ok(())
}

/// Checks a secret's value: [`check_value`], and at least
/// [`MIN_SECRET_LEN`] bytes.
pub fn check_secret_value(value: &str) -> Result<(), ConfigError> {
    check_value(value)?;
    if value.len() < MIN_SECRET_LEN {
        return Err(ConfigError::TooShort);
    }
    Ok(())
}

/// The entries a service sees: `layers` from lowest to highest precedence
/// (manifest defaults, project, environment, service; ADR-0009 rule 1),
/// each name taking its value from the highest layer that has it, whatever
/// kind of entry it is there.
pub fn resolve<T: Clone>(layers: &[&BTreeMap<String, T>]) -> BTreeMap<String, T> {
    let mut out = BTreeMap::new();
    for layer in layers {
        for (name, entry) in *layer {
            out.insert(name.clone(), entry.clone());
        }
    }
    out
}

/// One line of an env file Compose reads (`env_file:`), newline included:
/// `NAME="value"`, with `\`, `"` and `$` escaped so the value is read back
/// literally and nothing in it is interpolated. `name` and `value` must
/// pass [`check_name`] and [`check_value`].
pub fn env_file_line(name: &str, value: &str) -> String {
    let mut line = String::with_capacity(name.len() + value.len() + 4);
    line.push_str(name);
    line.push_str("=\"");
    for c in value.chars() {
        if matches!(c, '\\' | '"' | '$') {
            line.push('\\');
        }
        line.push(c);
    }
    line.push_str("\"\n");
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["A", "_", "DATABASE_URL", "a1", "_x_2", &"N".repeat(256)] {
            assert_eq!(check_name(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "1A",
            "A-B",
            "A B",
            "A=B",
            "Ä",
            "A.B",
            "$A",
            &"N".repeat(257),
        ] {
            assert_eq!(check_name(bad), Err(ConfigError::Name), "{bad}");
        }
    }

    #[test]
    fn values() {
        for ok in ["", "x", "a b", "$HOME", "\"'#", "a\\b", "ünïcode", "=="] {
            assert_eq!(check_value(ok), Ok(()), "{ok}");
        }
        assert_eq!(check_value("a\nb"), Err(ConfigError::Multiline));
        assert_eq!(check_value("a\rb"), Err(ConfigError::Multiline));
        assert_eq!(check_value("a\0b"), Err(ConfigError::Multiline));
        assert_eq!(check_value("a\\"), Err(ConfigError::TrailingBackslash));
        assert_eq!(
            check_value(&"x".repeat(MAX_VALUE_LEN + 1)),
            Err(ConfigError::TooLong)
        );
        assert_eq!(check_value(&"x".repeat(MAX_VALUE_LEN)), Ok(()));
        assert_eq!(check_secret_value("abc"), Err(ConfigError::TooShort));
        assert_eq!(check_secret_value(""), Err(ConfigError::TooShort));
        assert_eq!(check_secret_value("abcd"), Ok(()));
        assert_eq!(check_secret_value("ab\ncd"), Err(ConfigError::Multiline));
        // No message quotes anything given to it.
        for e in [
            ConfigError::Name,
            ConfigError::TooShort,
            ConfigError::TooLong,
            ConfigError::Multiline,
            ConfigError::TrailingBackslash,
        ] {
            assert!(!e.to_string().is_empty());
        }
    }

    fn layer(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn later_layers_win() {
        let defaults = layer(&[("A", "default"), ("B", "default"), ("D", "default")]);
        let project = layer(&[("A", "project"), ("B", "project"), ("C", "project")]);
        let environment = layer(&[("B", "environment"), ("C", "environment")]);
        let service = layer(&[("C", "service")]);
        let out = resolve(&[&defaults, &project, &environment, &service]);
        assert_eq!(
            out,
            layer(&[
                ("A", "project"),
                ("B", "environment"),
                ("C", "service"),
                ("D", "default"),
            ])
        );
        assert!(resolve::<String>(&[]).is_empty());
    }

    #[test]
    fn env_file_lines_escape_what_compose_would_read() {
        assert_eq!(env_file_line("A", "plain"), "A=\"plain\"\n");
        assert_eq!(env_file_line("A", ""), "A=\"\"\n");
        assert_eq!(
            env_file_line("A", r#"a"b\c$d ${E} # f 'g'"#),
            "A=\"a\\\"b\\\\c\\$d \\${E} # f 'g'\"\n"
        );
    }

    #[test]
    fn scopes() {
        assert_eq!(ConfigScope::Service.as_str(), "service");
        assert_eq!("environment".parse(), Ok(ConfigScope::Environment));
    }
}
