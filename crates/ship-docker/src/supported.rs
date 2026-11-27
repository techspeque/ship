//! The Docker Engine and Compose versions Ship supports — held here and
//! nowhere else (ADR-0001 rule 6).
//!
//! The CI Docker job checks that the Engine and Compose it runs against fall
//! inside these ranges, so a range that no longer matches a current Docker
//! fails the build rather than a user.

use std::fmt;

/// Docker Engine versions Ship supports.
pub const ENGINE: VersionRange = VersionRange {
    min: Version::new(24, 0, 0),
    below: Version::new(30, 0, 0),
};

/// Docker Compose plugin versions Ship supports. Compose went from 2 to 5;
/// there is no 3 or 4.
pub const COMPOSE: VersionRange = VersionRange {
    min: Version::new(2, 20, 0),
    below: Version::new(6, 0, 0),
};

/// A `major.minor.patch` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Version {
            major,
            minor,
            patch,
        }
    }

    /// Parses the leading `major[.minor[.patch]]` of a version string as
    /// Docker prints it. A leading `v` is allowed; anything after the
    /// numbers that starts with `-`, `+` or `~` (`-ce`, `+dfsg1`,
    /// `-rc.1`) is ignored. A missing minor or patch is 0.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let end = s
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(s.len());
        let (numbers, rest) = s.split_at(end);
        if !(rest.is_empty() || rest.starts_with(['-', '+', '~'])) {
            return None;
        }
        let mut parts = numbers.split('.');
        let mut next = |required: bool| -> Option<u32> {
            match parts.next() {
                None if !required => Some(0),
                None => None,
                Some(p) if p.is_empty() || p.len() > 9 => None,
                Some(p) => p.parse().ok(),
            }
        };
        let version = Version::new(next(true)?, next(false)?, next(false)?);
        if parts.next().is_some() {
            return None;
        }
        Some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// `min <= v < below`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionRange {
    pub min: Version,
    pub below: Version,
}

impl VersionRange {
    pub fn contains(&self, v: Version) -> bool {
        self.min <= v && v < self.below
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, ">= {}, < {}", self.min, self.below)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_version_strings() {
        for (s, v) in [
            ("28.3.2", Version::new(28, 3, 2)),
            ("24.0.7\n", Version::new(24, 0, 7)),
            ("v2.29.1", Version::new(2, 29, 1)),
            ("2.29.1-desktop.1", Version::new(2, 29, 1)),
            ("20.10.24+dfsg1", Version::new(20, 10, 24)),
            ("27.5.1-ce", Version::new(27, 5, 1)),
            ("26.1.0-rc.1", Version::new(26, 1, 0)),
            ("5.0", Version::new(5, 0, 0)),
            ("29", Version::new(29, 0, 0)),
        ] {
            assert_eq!(Version::parse(s), Some(v), "{s}");
        }
        for bad in [
            "",
            "v",
            "abc",
            "1..2",
            ".1",
            "1.2.3.4",
            "1.2.x",
            "1.2.3beta",
            "1234567890.0.0",
            "Cannot connect",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn range_boundaries() {
        assert!(ENGINE.contains(ENGINE.min));
        assert!(!ENGINE.contains(ENGINE.below));
        assert!(!ENGINE.contains(Version::new(23, 99, 99)));
        assert!(ENGINE.contains(Version::new(29, 99, 99)));
        assert!(!ENGINE.contains(Version::new(19, 3, 0)));

        assert!(COMPOSE.contains(Version::new(2, 20, 0)));
        assert!(!COMPOSE.contains(Version::new(2, 19, 9)));
        assert!(!COMPOSE.contains(Version::new(1, 29, 2)));
        assert!(COMPOSE.contains(Version::new(5, 0, 0)));
        assert!(!COMPOSE.contains(Version::new(6, 0, 0)));
    }

    #[test]
    fn ranges_are_well_formed() {
        assert!(ENGINE.min < ENGINE.below);
        assert!(COMPOSE.min < COMPOSE.below);
        assert_eq!(ENGINE.to_string(), ">= 24.0.0, < 30.0.0");
        assert_eq!(COMPOSE.to_string(), ">= 2.20.0, < 6.0.0");
    }
}
