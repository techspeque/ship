//! Human-chosen names for projects (and later environments and services).
//!
//! A slug is user input. It is shown and used for lookup, and never used to
//! name a Docker object (ADR-0001 rule 4); see [`crate::compose`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum slug length in bytes (all slug characters are ASCII).
pub const MAX_SLUG_LEN: usize = 63;

/// A validated slug: 1–63 characters of `a-z`, `0-9` and `-`, starting with
/// a letter, not ending with `-`, with no `--`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slug(String);

/// Why a string is not a valid slug. No variant echoes the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlugError {
    Empty,
    TooLong,
    InvalidStart,
    InvalidCharacter,
    TrailingHyphen,
    ConsecutiveHyphens,
}

impl fmt::Display for SlugError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SlugError::Empty => "slug must not be empty",
            SlugError::TooLong => "slug must be at most 63 characters",
            SlugError::InvalidStart => "slug must start with a lowercase letter",
            SlugError::InvalidCharacter => {
                "slug may contain only lowercase letters, digits and hyphens"
            }
            SlugError::TrailingHyphen => "slug must not end with a hyphen",
            SlugError::ConsecutiveHyphens => "slug must not contain consecutive hyphens",
        })
    }
}

impl std::error::Error for SlugError {}

impl Slug {
    /// Validates `s` as a slug.
    pub fn parse(s: &str) -> Result<Self, SlugError> {
        let bytes = s.as_bytes();
        let first = *bytes.first().ok_or(SlugError::Empty)?;
        if bytes.len() > MAX_SLUG_LEN {
            return Err(SlugError::TooLong);
        }
        if !first.is_ascii_lowercase() {
            return Err(SlugError::InvalidStart);
        }
        if !bytes
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(SlugError::InvalidCharacter);
        }
        if bytes.ends_with(b"-") {
            return Err(SlugError::TrailingHyphen);
        }
        if s.contains("--") {
            return Err(SlugError::ConsecutiveHyphens);
        }
        Ok(Slug(s.to_owned()))
    }

    /// The slug text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Slug {
    type Err = SlugError;

    fn from_str(s: &str) -> Result<Self, SlugError> {
        Slug::parse(s)
    }
}

impl AsRef<str> for Slug {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for Slug {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Slug {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Slug::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid() {
        let max = format!("a{}", "b".repeat(MAX_SLUG_LEN - 1));
        for s in ["a", "web", "my-app", "app2", "a-b-c", "x9-y8", max.as_str()] {
            assert_eq!(Slug::parse(s).unwrap().as_str(), s);
        }
    }

    #[test]
    fn rejects_invalid() {
        let too_long = "a".repeat(MAX_SLUG_LEN + 1);
        let cases: &[(&str, SlugError)] = &[
            ("", SlugError::Empty),
            (&too_long, SlugError::TooLong),
            ("1app", SlugError::InvalidStart),
            ("-app", SlugError::InvalidStart),
            ("App", SlugError::InvalidStart),
            ("_app", SlugError::InvalidStart),
            ("my_app", SlugError::InvalidCharacter),
            ("my.app", SlugError::InvalidCharacter),
            ("my app", SlugError::InvalidCharacter),
            ("myApp", SlugError::InvalidCharacter),
            ("app/..", SlugError::InvalidCharacter),
            ("café", SlugError::InvalidCharacter),
            ("app-", SlugError::TrailingHyphen),
            ("my--app", SlugError::ConsecutiveHyphens),
        ];
        for (s, err) in cases {
            assert_eq!(Slug::parse(s), Err(*err), "{s:?}");
        }
    }

    #[test]
    fn serde_round_trip_and_rejection() {
        let slug = Slug::parse("my-app").unwrap();
        let json = serde_json::to_string(&slug).unwrap();
        assert_eq!(json, "\"my-app\"");
        assert_eq!(serde_json::from_str::<Slug>(&json).unwrap(), slug);
        assert!(serde_json::from_str::<Slug>("\"My App\"").is_err());
    }
}
