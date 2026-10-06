//! What makes a schedule fire: cron expressions in a zone, fixed intervals,
//! or nothing (manual). Durations are parsed here too.

use crate::errors::AppError;

const UNITS: [(char, i64); 4] = [
    ('d', 86_400_000),
    ('h', 3_600_000),
    ('m', 60_000),
    ('s', 1000),
];

fn duration_error(value: &str) -> AppError {
    AppError::usage(
        "invalid_duration",
        format!("Expected a duration like 30s, 15m, 2h, or 1d, received \"{value}\"."),
    )
}

/// Splits `15m` into 15 and the unit's milliseconds. Whitespace around the
/// value is ignored, as JavaScript's trim() did.
fn split_duration(value: &str) -> Option<(i64, i64)> {
    let trimmed = value.trim();
    let unit = trimmed.chars().last()?;
    let (_, size) = UNITS.iter().find(|(name, _)| *name == unit)?;
    let digits = &trimmed[..trimmed.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let amount = digits.parse::<i64>().ok()?;
    Some((amount, *size))
}

/// A positive duration such as `30s`, `15m`, `2h` or `1d`, in milliseconds.
pub fn parse_duration(value: &str) -> Result<i64, AppError> {
    match split_duration(value) {
        Some((amount, size)) if amount > 0 => amount
            .checked_mul(size)
            .ok_or_else(|| duration_error(value)),
        _ => Err(duration_error(value)),
    }
}

/// A catch-up window: a duration, or zero for "record a missed run
/// instead". Zero may be written bare (`0`) or with any unit (`0m`), because
/// Arbor sends `--catch-up 0m` for automations with no grace period.
pub fn parse_catch_up(value: &str) -> Result<i64, AppError> {
    if value.trim() == "0" || matches!(split_duration(value), Some((0, _))) {
        return Ok(0);
    }
    parse_duration(value)
}

/// The largest unit that divides the span evenly, so 900 reads as 15m.
pub fn format_seconds(seconds: i64) -> String {
    for (unit, size) in [('d', 86_400), ('h', 3600), ('m', 60)] {
        if seconds >= size && seconds % size == 0 {
            return format!("{}{unit}", seconds / size);
        }
    }
    format!("{seconds}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations_and_rejects_junk() {
        assert_eq!(parse_duration("30s"), Ok(30_000));
        assert_eq!(parse_duration("15m"), Ok(900_000));
        assert_eq!(parse_duration(" 2h "), Ok(7_200_000));
        assert_eq!(parse_duration("1d"), Ok(86_400_000));
        for junk in [
            "10", "5x", "0m", "-5m", "1.5h", "m", "", "5 m", "+5m", "５m",
        ] {
            let error = parse_duration(junk).unwrap_err();
            assert_eq!(error.code, "invalid_duration", "{junk}");
            assert_eq!(error.exit_code, 2);
        }
        assert_eq!(
            parse_duration("soon").unwrap_err().message,
            "Expected a duration like 30s, 15m, 2h, or 1d, received \"soon\"."
        );
    }

    #[test]
    fn catch_up_takes_zero_in_any_unit() {
        for zero in ["0", " 0 ", "0s", "0m", "0h", "0d", "00m"] {
            assert_eq!(parse_catch_up(zero), Ok(0), "{zero}");
        }
        assert_eq!(parse_catch_up("30m"), Ok(1_800_000));
        assert!(parse_catch_up("soon").is_err());
        assert!(parse_catch_up("0x").is_err());
    }

    #[test]
    fn formats_seconds_in_the_largest_even_unit() {
        assert_eq!(format_seconds(900), "15m");
        assert_eq!(format_seconds(7200), "2h");
        assert_eq!(format_seconds(86_400), "1d");
        assert_eq!(format_seconds(90), "90s");
        assert_eq!(format_seconds(0), "0s");
    }
}
