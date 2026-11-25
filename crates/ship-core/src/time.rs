//! UTC timestamps with millisecond precision.
//!
//! Wire form is RFC 3339, always `YYYY-MM-DDTHH:MM:SS.mmmZ`. This module
//! formats and parses only; it never reads the clock. The range is
//! 1970-01-01 through 9999-12-31.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

const MS_PER_DAY: i64 = 86_400_000;
/// 9999-12-31T23:59:59.999Z.
const MAX_MS: i64 = 253_402_300_799_999;
const WIRE_LEN: usize = 24;

/// A UTC instant in Unix milliseconds, 1970 through 9999.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

/// Why a value is not a valid timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampError {
    OutOfRange,
    Malformed,
}

impl fmt::Display for TimestampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TimestampError::OutOfRange => "timestamp outside 1970-01-01..9999-12-31",
            TimestampError::Malformed => "timestamp must be YYYY-MM-DDTHH:MM:SS.mmmZ",
        })
    }
}

impl std::error::Error for TimestampError {}

impl Timestamp {
    /// From Unix milliseconds.
    pub fn from_unix_millis(ms: i64) -> Result<Self, TimestampError> {
        if (0..=MAX_MS).contains(&ms) {
            Ok(Timestamp(ms))
        } else {
            Err(TimestampError::OutOfRange)
        }
    }

    /// Unix milliseconds.
    pub fn unix_millis(self) -> i64 {
        self.0
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        _ => 28,
    }
}

/// Days since 1970-01-01 to (year, month, day). H. Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) to days since 1970-01-01. Inverse of the above.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.0.div_euclid(MS_PER_DAY);
        let rem = self.0.rem_euclid(MS_PER_DAY);
        let (y, mo, d) = civil_from_days(days);
        let (h, mi, s, ms) = (
            rem / 3_600_000,
            rem / 60_000 % 60,
            rem / 1000 % 60,
            rem % 1000,
        );
        write!(f, "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
    }
}

fn number(b: &[u8]) -> Result<i64, TimestampError> {
    b.iter().try_fold(0i64, |acc, &c| {
        if c.is_ascii_digit() {
            Ok(acc * 10 + i64::from(c - b'0'))
        } else {
            Err(TimestampError::Malformed)
        }
    })
}

impl FromStr for Timestamp {
    type Err = TimestampError;

    fn from_str(s: &str) -> Result<Self, TimestampError> {
        let b = s.as_bytes();
        if b.len() != WIRE_LEN
            || b[4] != b'-'
            || b[7] != b'-'
            || b[10] != b'T'
            || b[13] != b':'
            || b[16] != b':'
            || b[19] != b'.'
            || b[23] != b'Z'
        {
            return Err(TimestampError::Malformed);
        }
        let y = number(&b[0..4])?;
        let mo = number(&b[5..7])?;
        let d = number(&b[8..10])?;
        let h = number(&b[11..13])?;
        let mi = number(&b[14..16])?;
        let sec = number(&b[17..19])?;
        let ms = number(&b[20..23])?;
        if !(1..=12).contains(&mo)
            || d < 1
            || d > days_in_month(y, mo)
            || h > 23
            || mi > 59
            || sec > 59
        {
            return Err(TimestampError::Malformed);
        }
        let total =
            days_from_civil(y, mo, d) * MS_PER_DAY + h * 3_600_000 + mi * 60_000 + sec * 1000 + ms;
        Timestamp::from_unix_millis(total)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        let cases: &[(i64, &str)] = &[
            (0, "1970-01-01T00:00:00.000Z"),
            (951_782_400_000, "2000-02-29T00:00:00.000Z"),
            (1_469_918_176_385, "2016-07-30T22:36:16.385Z"),
            (4_107_542_399_999, "2100-02-28T23:59:59.999Z"),
            (MAX_MS, "9999-12-31T23:59:59.999Z"),
        ];
        for &(ms, s) in cases {
            let t = Timestamp::from_unix_millis(ms).unwrap();
            assert_eq!(t.to_string(), s);
            assert_eq!(s.parse::<Timestamp>().unwrap(), t, "{s}");
        }
    }

    #[test]
    fn round_trips_across_range() {
        let mut ms = 0i64;
        while ms <= MAX_MS {
            let t = Timestamp::from_unix_millis(ms).unwrap();
            assert_eq!(t.to_string().parse::<Timestamp>().unwrap(), t);
            ms += 7_777_777_777;
        }
    }

    #[test]
    fn rejects_out_of_range() {
        assert_eq!(
            Timestamp::from_unix_millis(-1),
            Err(TimestampError::OutOfRange)
        );
        assert_eq!(
            Timestamp::from_unix_millis(MAX_MS + 1),
            Err(TimestampError::OutOfRange)
        );
        assert_eq!(
            "1969-12-31T23:59:59.999Z".parse::<Timestamp>(),
            Err(TimestampError::OutOfRange)
        );
    }

    #[test]
    fn rejects_malformed() {
        for s in [
            "",
            "2016-07-30T22:36:16Z",
            "2016-07-30T22:36:16.385+00:00",
            "2016-07-30 22:36:16.385Z",
            "2016-07-30T22:36:16.385z",
            "2016-13-01T00:00:00.000Z",
            "2016-00-01T00:00:00.000Z",
            "2016-02-30T00:00:00.000Z",
            "2100-02-29T00:00:00.000Z",
            "2016-07-00T00:00:00.000Z",
            "2016-07-30T24:00:00.000Z",
            "2016-07-30T23:60:00.000Z",
            "2016-07-30T23:59:60.000Z",
            "2016-07-30T23:59:5a.000Z",
            "+016-07-30T23:59:59.000Z",
        ] {
            assert_eq!(
                s.parse::<Timestamp>(),
                Err(TimestampError::Malformed),
                "{s}"
            );
        }
    }

    #[test]
    fn serde_round_trip() {
        let t = Timestamp::from_unix_millis(1_469_918_176_385).unwrap();
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "\"2016-07-30T22:36:16.385Z\"");
        assert_eq!(serde_json::from_str::<Timestamp>(&json).unwrap(), t);
        assert!(serde_json::from_str::<Timestamp>("1469918176385").is_err());
    }
}
