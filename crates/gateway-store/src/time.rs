//! The single conversion between `DateTime<Utc>` and the integer columns.

use chrono::{DateTime, Utc};

/// Encode a timestamp as UTC milliseconds since the Unix epoch.
#[must_use]
pub fn to_millis(value: DateTime<Utc>) -> i64 {
    value.timestamp_millis()
}

/// Encode an optional timestamp.
#[must_use]
pub fn to_millis_opt(value: Option<DateTime<Utc>>) -> Option<i64> {
    value.map(to_millis)
}

/// Decode a timestamp column.
///
/// Out-of-range values (only reachable through hand-edited databases) fall
/// back to the Unix epoch rather than panicking: a corrupt row should not take
/// the daemon down.
#[must_use]
pub fn from_millis(value: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(value).unwrap_or(DateTime::UNIX_EPOCH)
}

/// Decode an optional timestamp column.
#[must_use]
pub fn from_millis_opt(value: Option<i64>) -> Option<DateTime<Utc>> {
    value.map(from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip_at_millisecond_resolution() {
        let now = Utc::now();
        let round_tripped = from_millis(to_millis(now));
        assert_eq!(round_tripped.timestamp_millis(), now.timestamp_millis());
    }

    #[test]
    fn a_corrupt_timestamp_degrades_instead_of_panicking() {
        assert_eq!(from_millis(i64::MAX), DateTime::UNIX_EPOCH);
    }
}
