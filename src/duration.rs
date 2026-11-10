//! Durations as people and agents write them: `<n>s|m|h|d` (`--for`, and
//! the MCP tools' `for`).

use std::time::Duration;

use thiserror::Error;

/// The longest `--for`.
pub const MAX_FOR: Duration = Duration::from_hours(30 * 24);

/// A `--for` value that is not `<n>s|m|h|d` with n ≥ 1, up to 30 days.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DurationError {
    #[error(
        "{0:?} is not a duration: use <n>s|m|h|d with a whole n of at least 1, e.g. 90s, 30m, 2h or 1d"
    )]
    Malformed(String),
    #[error("{0:?} is longer than 30 days: use <n>s|m|h|d up to 30d")]
    TooLong(String),
}

/// `<n>s|m|h|d` with a whole n ≥ 1, at most 30 days.
///
/// # Errors
/// [`DurationError`] for anything else.
pub fn parse_duration(s: &str) -> Result<Duration, DurationError> {
    let malformed = || DurationError::Malformed(s.to_owned());
    let too_long = || DurationError::TooLong(s.to_owned());
    let (at, unit) = s.char_indices().last().ok_or_else(malformed)?;
    let digits = &s[..at];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(malformed());
    }
    let per: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(malformed()),
    };
    // All digits, so the only parse failure left is overflow.
    let n: u64 = digits.parse().map_err(|_| too_long())?;
    if n == 0 {
        return Err(malformed());
    }
    let secs = n.checked_mul(per).ok_or_else(too_long)?;
    let duration = Duration::from_secs(secs);
    if duration > MAX_FOR {
        return Err(too_long());
    }
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_take_every_unit() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_mins(30));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_hours(2));
        assert_eq!(parse_duration("30d").unwrap(), MAX_FOR);
    }

    #[test]
    fn durations_refuse_zero_signs_bare_numbers_and_junk() {
        for bad in [
            "0", "0s", "-5m", "+5m", "5", "abc", "", "5w", "1.5h", "m", "5 m",
        ] {
            assert!(
                matches!(parse_duration(bad), Err(DurationError::Malformed(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn durations_refuse_more_than_30_days() {
        for bad in ["31d", "100000d", "99999999999999999999999s"] {
            assert!(
                matches!(parse_duration(bad), Err(DurationError::TooLong(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn duration_errors_name_the_format() {
        for bad in ["abc", "100000d"] {
            let message = parse_duration(bad).unwrap_err().to_string();
            assert!(message.contains("<n>s|m|h|d"), "{message}");
        }
    }
}
