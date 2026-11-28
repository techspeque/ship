//! Typed, prefixed, immutable entity ids.
//!
//! String form: `<prefix>_<body>`, where the body is 26 lowercase Crockford
//! base32 characters encoding a 128-bit value laid out like a ULID: a 48-bit
//! millisecond timestamp followed by 80 bits of entropy. Ids sort by creation
//! time. `ship-core` never reads a clock or a random source: the caller
//! supplies both through [`ProjectId::from_parts`] and its siblings.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Crockford base32, lowercase: no `i`, `l`, `o` or `u`.
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const BODY_LEN: usize = 26;
const ENTROPY_BITS: u32 = 80;
const ENTROPY_MASK: u128 = (1u128 << ENTROPY_BITS) - 1;

/// Largest timestamp an id can hold, in milliseconds since the Unix epoch.
pub const MAX_TIMESTAMP_MS: u64 = (1u64 << 48) - 1;

/// Why a string is not a valid id of the requested type.
///
/// No variant echoes the input back, so an error never repeats whatever the
/// caller pasted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    /// The string does not start with `<expected>_`.
    WrongPrefix { expected: &'static str },
    /// The body is not exactly 26 characters.
    WrongLength,
    /// The body contains a character outside lowercase Crockford base32.
    InvalidCharacter,
    /// The body encodes a value wider than 128 bits.
    Overflow,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdError::WrongPrefix { expected } => {
                write!(f, "id must start with `{expected}_`")
            }
            IdError::WrongLength => write!(f, "id body must be {BODY_LEN} characters"),
            IdError::InvalidCharacter => f.write_str("id body must be lowercase Crockford base32"),
            IdError::Overflow => f.write_str("id body exceeds 128 bits"),
        }
    }
}

impl std::error::Error for IdError {}

fn pack(timestamp_ms: u64, entropy: u128) -> Option<u128> {
    if timestamp_ms > MAX_TIMESTAMP_MS {
        return None;
    }
    Some((u128::from(timestamp_ms) << ENTROPY_BITS) | (entropy & ENTROPY_MASK))
}

fn encode(value: u128) -> [u8; BODY_LEN] {
    let mut out = [0u8; BODY_LEN];
    let mut v = value;
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(v & 0x1f) as usize];
        v >>= 5;
    }
    out
}

fn digit(b: u8) -> Option<u8> {
    ALPHABET.iter().position(|&a| a == b).map(|p| p as u8)
}

fn decode(body: &str) -> Result<u128, IdError> {
    let bytes = body.as_bytes();
    if bytes.len() != BODY_LEN {
        return Err(IdError::WrongLength);
    }
    let mut v: u128 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let d = digit(b).ok_or(IdError::InvalidCharacter)?;
        // 26 * 5 = 130 bits; the first character may carry only 3 of them.
        if i == 0 && d > 7 {
            return Err(IdError::Overflow);
        }
        v = (v << 5) | u128::from(d);
    }
    Ok(v)
}

fn parse(s: &str, prefix: &'static str) -> Result<u128, IdError> {
    let body = s
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
        .ok_or(IdError::WrongPrefix { expected: prefix })?;
    decode(body)
}

fn write(f: &mut fmt::Formatter<'_>, prefix: &str, value: u128) -> fmt::Result {
    let body = encode(value);
    f.write_str(prefix)?;
    f.write_str("_")?;
    f.write_str(std::str::from_utf8(&body).map_err(|_| fmt::Error)?)
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        ///
        /// Immutable: there is no way to change an id once built.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u128);

        impl $name {
            /// The string prefix, without the `_` separator.
            pub const PREFIX: &'static str = $prefix;

            /// Builds an id from a millisecond Unix timestamp and caller-supplied
            /// entropy (only the low 80 bits are used). `None` if the timestamp
            /// exceeds [`MAX_TIMESTAMP_MS`].
            pub fn from_parts(timestamp_ms: u64, entropy: u128) -> Option<Self> {
                pack(timestamp_ms, entropy).map(Self)
            }

            /// The creation timestamp encoded in the id, in Unix milliseconds.
            pub fn timestamp_ms(&self) -> u64 {
                (self.0 >> ENTROPY_BITS) as u64
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write(f, Self::PREFIX, self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, IdError> {
                parse(s, Self::PREFIX).map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

define_id!(
    /// Identifies a project.
    ProjectId,
    "prj"
);
define_id!(
    /// Identifies an environment. The Compose project name derives from it.
    EnvironmentId,
    "env"
);
define_id!(
    /// Identifies a service within an environment.
    ServiceId,
    "svc"
);
define_id!(
    /// Identifies an immutable revision (snapshot, commit or image).
    RevisionId,
    "rev"
);
define_id!(
    /// Identifies an immutable release.
    ReleaseId,
    "rel"
);
define_id!(
    /// Identifies a deployment.
    DeploymentId,
    "dep"
);
define_id!(
    /// Identifies a durable operation.
    OperationId,
    "op"
);
define_id!(
    /// Identifies a route: a service's address through a provider (ADR-0010).
    RouteId,
    "rte"
);

#[cfg(test)]
mod tests {
    use super::*;

    const TS: u64 = 1_469_918_176_385;
    const ENTROPY: u128 = 0x0123_4567_89ab_cdef_fedc;

    fn round_trip<T>(id: T)
    where
        T: fmt::Display + FromStr<Err = IdError> + PartialEq + fmt::Debug + Serialize,
        T: for<'de> Deserialize<'de>,
    {
        let s = id.to_string();
        assert_eq!(s.parse::<T>().unwrap(), id);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{s}\""));
        assert_eq!(serde_json::from_str::<T>(&json).unwrap(), id);
    }

    #[test]
    fn all_types_round_trip() {
        round_trip(ProjectId::from_parts(TS, ENTROPY).unwrap());
        round_trip(EnvironmentId::from_parts(TS, ENTROPY).unwrap());
        round_trip(ServiceId::from_parts(TS, ENTROPY).unwrap());
        round_trip(RevisionId::from_parts(TS, ENTROPY).unwrap());
        round_trip(ReleaseId::from_parts(TS, ENTROPY).unwrap());
        round_trip(DeploymentId::from_parts(TS, ENTROPY).unwrap());
        round_trip(OperationId::from_parts(TS, ENTROPY).unwrap());
        round_trip(RouteId::from_parts(TS, ENTROPY).unwrap());
        round_trip(ProjectId::from_parts(0, 0).unwrap());
        round_trip(ProjectId::from_parts(MAX_TIMESTAMP_MS, u128::MAX).unwrap());
    }

    #[test]
    fn prefixes_are_distinct() {
        let mut prefixes = [
            ProjectId::PREFIX,
            EnvironmentId::PREFIX,
            ServiceId::PREFIX,
            RevisionId::PREFIX,
            ReleaseId::PREFIX,
            DeploymentId::PREFIX,
            OperationId::PREFIX,
            RouteId::PREFIX,
        ];
        prefixes.sort_unstable();
        let before = prefixes.len();
        let mut v = prefixes.to_vec();
        v.dedup();
        assert_eq!(v.len(), before);
    }

    #[test]
    fn known_vector() {
        // ULID spec example: time part 01ARYZ6S41 is 1469918176385 ms.
        let id: ProjectId = "prj_01aryz6s41tsv4rrffq69g5fav".parse().unwrap();
        assert_eq!(id.timestamp_ms(), TS);
        assert_eq!(id.to_string(), "prj_01aryz6s41tsv4rrffq69g5fav");
        assert_eq!(
            ProjectId::from_parts(0, 0).unwrap().to_string(),
            "prj_00000000000000000000000000"
        );
        assert_eq!(
            ProjectId::from_parts(MAX_TIMESTAMP_MS, u128::MAX)
                .unwrap()
                .to_string(),
            "prj_7zzzzzzzzzzzzzzzzzzzzzzzzz"
        );
    }

    #[test]
    fn from_parts_keeps_timestamp_and_masks_entropy() {
        let id = OperationId::from_parts(TS, u128::MAX).unwrap();
        assert_eq!(id.timestamp_ms(), TS);
        assert!(OperationId::from_parts(MAX_TIMESTAMP_MS + 1, 0).is_none());
    }

    #[test]
    fn ids_sort_by_timestamp() {
        let a = ServiceId::from_parts(1, u128::MAX).unwrap();
        let b = ServiceId::from_parts(2, 0).unwrap();
        assert!(a < b);
        assert!(a.to_string() < b.to_string());
    }

    #[test]
    fn rejects_wrong_prefix() {
        let body = "01aryz6s41tsv4rrffq69g5fav";
        let expected = Err(IdError::WrongPrefix { expected: "prj" });
        assert_eq!(format!("env_{body}").parse::<ProjectId>(), expected);
        assert_eq!(format!("PRJ_{body}").parse::<ProjectId>(), expected);
        assert_eq!(format!("prj-{body}").parse::<ProjectId>(), expected);
        assert_eq!(format!("prj{body}").parse::<ProjectId>(), expected);
        assert_eq!(body.parse::<ProjectId>(), expected);
        assert_eq!("".parse::<ProjectId>(), expected);
        // Every other entity's id is rejected by every type but its own.
        let env = EnvironmentId::from_parts(TS, ENTROPY).unwrap().to_string();
        assert!(env.parse::<ProjectId>().is_err());
        assert!(env.parse::<ServiceId>().is_err());
        assert!(env.parse::<OperationId>().is_err());
        let op = OperationId::from_parts(TS, ENTROPY).unwrap().to_string();
        assert!(op.parse::<EnvironmentId>().is_err());
        assert!(op.parse::<DeploymentId>().is_err());
    }

    #[test]
    fn rejects_wrong_length() {
        for s in [
            "prj_",
            "prj_01aryz6s41tsv4rrffq69g5fa",
            "prj_01aryz6s41tsv4rrffq69g5fav0",
            "prj_01aryz6s41tsv4rrffq69g5fav ",
        ] {
            assert_eq!(s.parse::<ProjectId>(), Err(IdError::WrongLength), "{s}");
        }
    }

    #[test]
    fn rejects_invalid_characters() {
        for s in [
            "prj_01ARYZ6S41TSV4RRFFQ69G5FAV",
            "prj_01aryz6s41tsv4rrffq69g5faV",
            "prj_01aryz6s41tsv4rrffq69g5fai",
            "prj_01aryz6s41tsv4rrffq69g5fal",
            "prj_01aryz6s41tsv4rrffq69g5fao",
            "prj_01aryz6s41tsv4rrffq69g5fau",
            "prj_01aryz6s41tsv4rrffq69g5fa-",
            "prj_01aryz6s41tsv4rrffq69g5fa_",
            "prj_01aryz6s41tsv4rrffq69g5fa/",
        ] {
            assert_eq!(
                s.parse::<ProjectId>(),
                Err(IdError::InvalidCharacter),
                "{s}"
            );
        }
        // A multibyte character changes the byte length.
        assert!(
            "prj_01aryz6s41tsv4rrffq69g5faé"
                .parse::<ProjectId>()
                .is_err()
        );
    }

    #[test]
    fn rejects_overflow() {
        assert_eq!(
            "prj_80000000000000000000000000".parse::<ProjectId>(),
            Err(IdError::Overflow)
        );
        assert_eq!(
            "prj_zzzzzzzzzzzzzzzzzzzzzzzzzz".parse::<ProjectId>(),
            Err(IdError::Overflow)
        );
    }

    #[test]
    fn serde_rejects_invalid() {
        assert!(serde_json::from_str::<ProjectId>("\"env_01aryz6s41tsv4rrffq69g5fav\"").is_err());
        assert!(serde_json::from_str::<ProjectId>("42").is_err());
    }

    #[test]
    fn errors_do_not_echo_input() {
        let input = "prj_s3cr3t-pasted-by-mistake";
        let err = input.parse::<ProjectId>().unwrap_err().to_string();
        assert!(!err.contains("s3cr3t"));
    }
}
