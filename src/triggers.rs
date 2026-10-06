//! What makes a schedule fire: cron expressions in a zone, fixed intervals,
//! or nothing (manual). Durations are parsed here too.

use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};

use crate::cron::{self, Pattern, Zone};
use crate::errors::AppError;
use crate::zones::NamedZone;

/// When a schedule fires. A cron trigger with no zone reads its expression
/// in the machine's local time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Cron {
        expression: String,
        timezone: Option<String>,
    },
    Every {
        seconds: i64,
    },
    Manual,
}

// Records print a trigger as 0.2.1 did: its fields alphabetically, `kind`
// among them.
impl Serialize for Trigger {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Cron {
                expression,
                timezone,
            } => {
                let mut record = serializer.serialize_struct("Trigger", 3)?;
                record.serialize_field("expression", expression)?;
                record.serialize_field("kind", "cron")?;
                record.serialize_field("timezone", timezone)?;
                record.end()
            }
            Self::Every { seconds } => {
                let mut record = serializer.serialize_struct("Trigger", 2)?;
                record.serialize_field("kind", "every")?;
                record.serialize_field("seconds", seconds)?;
                record.end()
            }
            Self::Manual => {
                let mut record = serializer.serialize_struct("Trigger", 1)?;
                record.serialize_field("kind", "manual")?;
                record.end()
            }
        }
    }
}

impl Trigger {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Cron { .. } => "cron",
            Self::Every { .. } => "every",
            Self::Manual => "manual",
        }
    }
}

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
/// instead". Zero may be written bare (`0`) or with any unit (`0m`), since
/// callers often write zero with the unit they use elsewhere.
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

/// Names ICU still accepts that tz data has since dropped, with the zone
/// they meant.
const RETIRED_ZONES: [(&str, &str); 2] = [
    ("Canada/East-Saskatchewan", "America/Regina"),
    ("US/Pacific-New", "America/Los_Angeles"),
];

/// An offset zone as Intl reads one: `+HH`, `+HHMM` or `+HH:MM` (either
/// sign), at most 23:59. Returns its seconds east of UTC.
fn offset_seconds(zone: &str) -> Option<i64> {
    let (sign, rest) = match zone.as_bytes().first()? {
        b'+' => (1, &zone[1..]),
        b'-' => (-1, &zone[1..]),
        _ => return None,
    };
    let digits = |text: &str| text.len() == 2 && text.bytes().all(|byte| byte.is_ascii_digit());
    let (hours, minutes) = match rest.len() {
        2 => (rest, "00"),
        4 => (&rest[..2], &rest[2..]),
        5 if rest.as_bytes()[2] == b':' => (&rest[..2], &rest[3..]),
        _ => return None,
    };
    if !digits(hours) || !digits(minutes) {
        return None;
    }
    let (hours, minutes): (i64, i64) = (hours.parse().ok()?, minutes.parse().ok()?);
    (hours <= 23 && minutes <= 59).then_some(sign * (hours * 3600 + minutes * 60))
}

/// A zone name as Intl canonicalizes it and the zone to compute in: IANA
/// names matched without regard to case and kept as named (Intl doesn't
/// resolve links such as US/Eastern), offsets as `+HH:MM`.
fn resolve_zone(name: &str) -> Option<(String, Zone)> {
    if let Some(seconds) = offset_seconds(name) {
        let minutes = seconds.abs() / 60;
        let sign = if seconds < 0 { '-' } else { '+' };
        return Some((
            format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60),
            Zone::Fixed(seconds),
        ));
    }
    if let Some((retired, target)) = RETIRED_ZONES
        .iter()
        .find(|(retired, _)| retired.eq_ignore_ascii_case(name))
    {
        let zone = NamedZone::find(target)?;
        return Some(((*retired).to_owned(), Zone::Named(zone)));
    }
    NamedZone::find(name).map(|zone| (zone.name().to_owned(), Zone::Named(zone)))
}

/// Checks a --tz value and returns the zone's own spelling, so
/// `australia/sydney` is stored as `Australia/Sydney`.
pub fn require_timezone(zone: &str) -> Result<String, AppError> {
    resolve_zone(zone).map(|(name, _)| name).ok_or_else(|| {
        AppError::usage("invalid_timezone", format!("Unknown time zone \"{zone}\"."))
            .hint("Use an IANA zone name such as Australia/Sydney or America/New_York.")
    })
}

fn parse_cron(expression: &str) -> Result<Pattern, AppError> {
    Pattern::parse(expression).map_err(|message| {
        AppError::usage(
            "invalid_cron",
            format!("Invalid cron expression \"{expression}\": {message}"),
        )
    })
}

/// The trigger the --cron, --every and --tz flags describe.
pub fn parse_trigger(
    cron: Option<&str>,
    every: Option<&str>,
    tz: Option<&str>,
) -> Result<Trigger, AppError> {
    if tz.is_some() && cron.is_none() {
        return Err(AppError::usage(
            "timezone_requires_cron",
            "--tz applies to --cron triggers only.",
        ));
    }
    if cron.is_some() && every.is_some() {
        return Err(AppError::usage(
            "conflicting_triggers",
            "Use either --cron or --every, not both.",
        ));
    }
    if let Some(expression) = cron {
        let timezone = tz.map(require_timezone).transpose()?;
        parse_cron(expression)?;
        return Ok(Trigger::Cron {
            expression: expression.to_owned(),
            timezone,
        });
    }
    if let Some(every) = every {
        return Ok(Trigger::Every {
            seconds: parse_duration(every)? / 1000,
        });
    }
    Ok(Trigger::Manual)
}

pub fn describe_trigger(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Cron {
            expression,
            timezone: None,
        } => format!("cron {expression}"),
        Trigger::Cron {
            expression,
            timezone: Some(zone),
        } => format!("cron {expression} ({zone})"),
        Trigger::Every { seconds } => format!("every {}", format_seconds(*seconds)),
        Trigger::Manual => "manual".into(),
    }
}

/// The first fire after `from_ms` (croner's nextRun), or none for a
/// manual trigger or a cron expression with no time left.
pub fn next_fire_at(trigger: &Trigger, from_ms: i64) -> Option<i64> {
    match trigger {
        Trigger::Cron {
            expression,
            timezone,
        } => {
            let pattern = parse_cron(expression).ok()?;
            let zone = match timezone {
                None => Zone::Local,
                // A zone 0.2.1 could store but no longer names a zone would
                // have failed there too; there is no next fire to give.
                Some(name) => resolve_zone(name)?.1,
            };
            cron::next_run(&pattern, zone, from_ms)
        }
        Trigger::Every { seconds } => Some(from_ms + seconds * 1000),
        Trigger::Manual => None,
    }
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

    fn ms(instant: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(instant)
            .expect("an RFC 3339 instant")
            .timestamp_millis()
    }

    /// Three fires in a row from each start, as croner 10.0.1 gave them in
    /// 0.2.1: expression | zone | from | the next three fires. DST gaps fire
    /// late, repeated hours fire once, and the rest are the field forms most
    /// likely to drift.
    const NEXT_FIRES: &str = "
# DST in Sydney: 02:00-03:00 is skipped on 4 Oct 2026 and repeated on 5 Apr 2026
30 2 * * *        | Australia/Sydney | 2026-10-03T15:59:59.999Z | 2026-10-03T16:30:00.000Z 2026-10-04T15:30:00.000Z 2026-10-05T15:30:00.000Z
30 2 * * *        | Australia/Sydney | 2026-04-04T15:59:59.999Z | 2026-04-05T16:30:00.000Z 2026-04-06T16:30:00.000Z 2026-04-07T16:30:00.000Z
0 2 * * *         | Australia/Sydney | 2026-10-03T15:59:59.999Z | 2026-10-03T16:00:00.000Z 2026-10-04T15:00:00.000Z 2026-10-05T15:00:00.000Z
0 2 * * *         | Australia/Sydney | 2026-04-04T15:59:59.999Z | 2026-04-05T16:00:00.000Z 2026-04-06T16:00:00.000Z 2026-04-07T16:00:00.000Z
* 2 * * *         | Australia/Sydney | 2026-10-03T15:59:59.999Z | 2026-10-03T16:00:00.000Z 2026-10-04T15:00:00.000Z 2026-10-04T15:01:00.000Z
* 2 * * *         | Australia/Sydney | 2026-04-04T15:59:59.999Z | 2026-04-05T16:00:00.000Z 2026-04-05T16:01:00.000Z 2026-04-05T16:02:00.000Z
0 3 * * *         | Australia/Sydney | 2026-10-03T15:59:59.999Z | 2026-10-03T16:00:00.000Z 2026-10-04T16:00:00.000Z 2026-10-05T16:00:00.000Z
30 1 * * *        | Australia/Sydney | 2026-10-03T15:59:59.999Z | 2026-10-04T14:30:00.000Z 2026-10-05T14:30:00.000Z 2026-10-06T14:30:00.000Z
*/15 * * * *      | Australia/Sydney | 2026-04-04T15:59:59.999Z | 2026-04-04T17:00:00.000Z 2026-04-04T17:15:00.000Z 2026-04-04T17:30:00.000Z
0 */2 * * *       | Australia/Sydney | 2026-04-04T15:59:59.999Z | 2026-04-04T18:00:00.000Z 2026-04-04T20:00:00.000Z 2026-04-04T22:00:00.000Z
0 30 2 * * * *    | Australia/Sydney | 2026-10-03T15:59:59.999Z | 2026-10-03T16:30:00.000Z 2026-10-04T15:30:00.000Z 2026-10-05T15:30:00.000Z
15 30 2 * * *     | Australia/Sydney | 2026-04-04T15:59:59.999Z | 2026-04-05T16:30:15.000Z 2026-04-06T16:30:15.000Z 2026-04-07T16:30:15.000Z
30 2 * * *        | Australia/Sydney | 2026-04-04T16:30:00.000Z | 2026-04-05T16:30:00.000Z 2026-04-06T16:30:00.000Z 2026-04-07T16:30:00.000Z
0 2 * * *         | Australia/Sydney | 2026-10-03T14:30:00.000Z | 2026-10-03T16:00:00.000Z 2026-10-04T15:00:00.000Z 2026-10-05T15:00:00.000Z
0 9 * * MON-FRI   | Australia/Sydney | 2026-07-01T12:34:56.789Z | 2026-07-01T23:00:00.000Z 2026-07-02T23:00:00.000Z 2026-07-05T23:00:00.000Z
@daily            | Australia/Sydney | 2026-12-31T23:59:59.999Z | 2027-01-01T13:00:00.000Z 2027-01-02T13:00:00.000Z 2027-01-03T13:00:00.000Z
# DST in New York: 02:00-03:00 is skipped on 8 Mar 2026, 01:00-02:00 repeated on 1 Nov 2026
30 2 * * *        | America/New_York | 2026-03-08T06:59:59.999Z | 2026-03-08T07:30:00.000Z 2026-03-09T06:30:00.000Z 2026-03-10T06:30:00.000Z
30 2 * * *        | America/New_York | 2026-11-01T05:59:59.999Z | 2026-11-01T07:30:00.000Z 2026-11-02T07:30:00.000Z 2026-11-03T07:30:00.000Z
30 1 * * *        | America/New_York | 2026-03-08T06:59:59.999Z | 2026-03-09T05:30:00.000Z 2026-03-10T05:30:00.000Z 2026-03-11T05:30:00.000Z
30 1 * * *        | America/New_York | 2026-11-01T05:59:59.999Z | 2026-11-02T06:30:00.000Z 2026-11-03T06:30:00.000Z 2026-11-04T06:30:00.000Z
0 2 * * *         | America/New_York | 2026-03-08T06:59:59.999Z | 2026-03-08T07:00:00.000Z 2026-03-09T06:00:00.000Z 2026-03-10T06:00:00.000Z
*/30 * * * * *    | America/New_York | 2026-11-01T05:59:59.999Z | 2026-11-01T07:00:00.000Z 2026-11-01T07:00:30.000Z 2026-11-01T07:01:00.000Z
30 1 * * *        | America/New_York | 2026-11-01T06:30:00.000Z | 2026-11-02T06:30:00.000Z 2026-11-03T06:30:00.000Z 2026-11-04T06:30:00.000Z
0 9 * * 1-5       | America/New_York | 2026-01-15T00:00:00.000Z | 2026-01-15T14:00:00.000Z 2026-01-16T14:00:00.000Z 2026-01-19T14:00:00.000Z
# DST in Berlin: 02:00-03:00 is skipped on 29 Mar 2026, repeated on 25 Oct 2026
30 2 * * *        | Europe/Berlin    | 2026-03-29T00:59:59.999Z | 2026-03-29T01:30:00.000Z 2026-03-30T00:30:00.000Z 2026-03-31T00:30:00.000Z
30 2 * * *        | Europe/Berlin    | 2026-10-25T00:59:59.999Z | 2026-10-26T01:30:00.000Z 2026-10-27T01:30:00.000Z 2026-10-28T01:30:00.000Z
0 2 * * *         | Europe/Berlin    | 2026-03-29T00:59:59.999Z | 2026-03-29T01:00:00.000Z 2026-03-30T00:00:00.000Z 2026-03-31T00:00:00.000Z
0 2 * * *         | Europe/Berlin    | 2026-10-25T00:59:59.999Z | 2026-10-26T01:00:00.000Z 2026-10-27T01:00:00.000Z 2026-10-28T01:00:00.000Z
0 3 * * *         | Europe/Berlin    | 2026-10-25T00:59:59.999Z | 2026-10-25T02:00:00.000Z 2026-10-26T02:00:00.000Z 2026-10-27T02:00:00.000Z
15 */6 * * *      | Europe/Berlin    | 2026-03-29T00:59:59.999Z | 2026-03-29T04:15:00.000Z 2026-03-29T10:15:00.000Z 2026-03-29T16:15:00.000Z
0 9 * * 7         | Europe/Berlin    | 2026-07-01T12:34:56.789Z | 2026-07-05T07:00:00.000Z 2026-07-12T07:00:00.000Z 2026-07-19T07:00:00.000Z
# L, W and #
0 9 L * *         | UTC              | 2027-02-28T10:00:00.000Z | 2027-03-31T09:00:00.000Z 2027-04-30T09:00:00.000Z 2027-05-31T09:00:00.000Z
0 9 LW * *        | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-30T09:00:00.000Z 2026-02-27T09:00:00.000Z 2026-03-31T09:00:00.000Z
0 9 15W * *       | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-15T09:00:00.000Z 2026-02-16T09:00:00.000Z 2026-03-16T09:00:00.000Z
0 9 1W * *        | UTC              | 2027-02-28T10:00:00.000Z | 2027-03-01T09:00:00.000Z 2027-04-01T09:00:00.000Z 2027-05-03T09:00:00.000Z
0 9 31W * *       | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-30T09:00:00.000Z 2026-03-31T09:00:00.000Z 2026-05-29T09:00:00.000Z
0 9 * * 5L        | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-30T09:00:00.000Z 2026-02-27T09:00:00.000Z 2026-03-27T09:00:00.000Z
0 9 * * 5#2       | UTC              | 2026-01-15T00:00:00.000Z | 2026-02-13T09:00:00.000Z 2026-03-13T09:00:00.000Z 2026-04-10T09:00:00.000Z
0 9 * * 5#L       | UTC              | 2026-12-31T23:59:59.999Z | 2027-01-29T09:00:00.000Z 2027-02-26T09:00:00.000Z 2027-03-26T09:00:00.000Z
0 9 * * 1#1       | UTC              | 2026-07-01T12:34:56.789Z | 2026-07-06T09:00:00.000Z 2026-08-03T09:00:00.000Z 2026-09-07T09:00:00.000Z
0 9 * * 1L-3      | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-19T09:00:00.000Z 2026-01-20T09:00:00.000Z 2026-01-21T09:00:00.000Z
# Day of month OR day of week, unless + makes it AND
0 9 1 * 1         | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-19T09:00:00.000Z 2026-01-26T09:00:00.000Z 2026-02-01T09:00:00.000Z
0 0 13 * 5        | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-16T00:00:00.000Z 2026-01-23T00:00:00.000Z 2026-01-30T00:00:00.000Z
0 9 L * 1         | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-19T09:00:00.000Z 2026-01-26T09:00:00.000Z 2026-01-31T09:00:00.000Z
0 9 1 * +1        | UTC              | 2026-01-15T00:00:00.000Z | 2026-06-01T09:00:00.000Z 2027-02-01T09:00:00.000Z 2027-03-01T09:00:00.000Z
0 9 * * +1        | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-19T09:00:00.000Z 2026-01-26T09:00:00.000Z 2026-02-02T09:00:00.000Z
0 9 15 * ?        | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-15T09:00:00.000Z 2026-01-16T09:00:00.000Z 2026-01-17T09:00:00.000Z
0 9 ? * *         | UTC              | 2026-07-01T12:34:56.789Z | 2026-07-02T09:00:00.000Z 2026-07-03T09:00:00.000Z 2026-07-04T09:00:00.000Z
# Sunday as 0 and 7, names, steps, lists and rare dates
0 9 * * 0         | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-18T09:00:00.000Z 2026-01-25T09:00:00.000Z 2026-02-01T09:00:00.000Z
0 9 * * fri-sun   | UTC              | 2026-07-01T12:34:56.789Z | 2026-07-03T09:00:00.000Z 2026-07-04T09:00:00.000Z 2026-07-05T09:00:00.000Z
0,30 8 * * 1,3,5  | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-16T08:00:00.000Z 2026-01-16T08:30:00.000Z 2026-01-19T08:00:00.000Z
*/5 9-17 * * *    | UTC              | 2026-07-01T12:34:56.789Z | 2026-07-01T12:35:00.000Z 2026-07-01T12:40:00.000Z 2026-07-01T12:45:00.000Z
*/60 * * * *      | UTC              | 2026-07-01T12:34:56.789Z | 2026-07-01T13:00:00.000Z 2026-07-01T14:00:00.000Z 2026-07-01T15:00:00.000Z
0 0 */3 * *       | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-16T00:00:00.000Z 2026-01-19T00:00:00.000Z 2026-01-22T00:00:00.000Z
0 0 29 2 *        | UTC              | 2026-01-15T00:00:00.000Z | 2028-02-29T00:00:00.000Z 2032-02-29T00:00:00.000Z 2036-02-29T00:00:00.000Z
0 0 31 * *        | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-31T00:00:00.000Z 2026-03-31T00:00:00.000Z 2026-05-31T00:00:00.000Z
0 9,17 * * *      | UTC              | 2026-12-31T23:59:59.999Z | 2027-01-01T09:00:00.000Z 2027-01-01T17:00:00.000Z 2027-01-02T09:00:00.000Z
# Six and seven fields, and nicknames
*/30 * * * * *    | UTC              | 2026-07-01T12:34:56.789Z | 2026-07-01T12:35:00.000Z 2026-07-01T12:35:30.000Z 2026-07-01T12:36:00.000Z
0 */20 * * * *    | UTC              | 2026-07-01T12:34:56.789Z | 2026-07-01T12:40:00.000Z 2026-07-01T13:00:00.000Z 2026-07-01T13:20:00.000Z
0 0 9 * * *       | UTC              | 2026-12-31T23:59:59.999Z | 2027-01-01T09:00:00.000Z 2027-01-02T09:00:00.000Z 2027-01-03T09:00:00.000Z
0 0 9 * * * 2027  | UTC              | 2026-01-15T00:00:00.000Z | 2027-01-01T09:00:00.000Z 2027-01-02T09:00:00.000Z 2027-01-03T09:00:00.000Z
@hourly           | UTC              | 2026-07-01T12:34:56.789Z | 2026-07-01T13:00:00.000Z 2026-07-01T14:00:00.000Z 2026-07-01T15:00:00.000Z
@weekly           | UTC              | 2026-01-15T00:00:00.000Z | 2026-01-18T00:00:00.000Z 2026-01-25T00:00:00.000Z 2026-02-01T00:00:00.000Z
@monthly          | UTC              | 2026-12-31T23:59:59.999Z | 2027-01-01T00:00:00.000Z 2027-02-01T00:00:00.000Z 2027-03-01T00:00:00.000Z
@yearly           | UTC              | 2026-01-15T00:00:00.000Z | 2027-01-01T00:00:00.000Z 2028-01-01T00:00:00.000Z 2029-01-01T00:00:00.000Z
@YEARLY           | UTC              | 2027-02-28T10:00:00.000Z | 2028-01-01T00:00:00.000Z 2029-01-01T00:00:00.000Z 2030-01-01T00:00:00.000Z
# A fixed offset
0 9 * * *         | +05:30           | 2026-01-15T00:00:00.000Z | 2026-01-15T03:30:00.000Z 2026-01-16T03:30:00.000Z 2026-01-17T03:30:00.000Z
30 * * * *        | +05:30           | 2026-07-01T12:34:56.789Z | 2026-07-01T13:00:00.000Z 2026-07-01T14:00:00.000Z 2026-07-01T15:00:00.000Z
0 0 * * *         | +05:30           | 2026-12-31T23:59:59.999Z | 2027-01-01T18:30:00.000Z 2027-01-02T18:30:00.000Z 2027-01-03T18:30:00.000Z
";

    #[test]
    fn next_fires_match_croner() {
        let rows = NEXT_FIRES
            .lines()
            .filter(|row| !row.is_empty() && !row.starts_with('#'));
        for row in rows {
            let fields: Vec<&str> = row.split('|').map(str::trim).collect();
            let [expression, zone, from, fires] = fields[..] else {
                panic!("{row}");
            };
            let trigger = Trigger::Cron {
                expression: expression.to_owned(),
                timezone: Some(zone.to_owned()),
            };
            let mut cursor = ms(from);
            for next in fires.split(' ') {
                cursor = next_fire_at(&trigger, cursor)
                    .unwrap_or_else(|| panic!("{expression} in {zone} from {from} stopped"));
                assert_eq!(cursor, ms(next), "{expression} in {zone} from {from}");
            }
        }
        // A year that has passed has no next fire.
        let spent = Trigger::Cron {
            expression: "0 0 9 * * * 2027".into(),
            timezone: Some("UTC".into()),
        };
        assert_eq!(next_fire_at(&spent, ms("2028-01-01T00:00:00Z")), None);
    }

    /// Fires for a cron with no zone, read in the machine's local time, as
    /// croner 10.0.1 gave them in 0.2.1 with the machine in
    /// Australia/Sydney: expression | from | the next three fires. Covers
    /// the 4 Oct 2026 gap and the 5 Apr 2026 repeated hour.
    const LOCAL_FIRES: &str = "
30 2 * * *        | 2026-10-03T15:59:59.999Z | 2026-10-03T16:30:00.000Z 2026-10-04T15:30:00.000Z 2026-10-05T15:30:00.000Z
* 2 * * *         | 2026-10-03T15:59:59.999Z | 2026-10-03T16:00:00.000Z 2026-10-04T15:00:00.000Z 2026-10-04T15:01:00.000Z
30 2 * * *        | 2026-04-04T15:59:59.999Z | 2026-04-05T16:30:00.000Z 2026-04-06T16:30:00.000Z 2026-04-07T16:30:00.000Z
* 2 * * *         | 2026-04-04T15:59:59.999Z | 2026-04-05T16:00:00.000Z 2026-04-05T16:01:00.000Z 2026-04-05T16:02:00.000Z
*/15 * * * *      | 2026-04-04T15:59:59.999Z | 2026-04-04T17:00:00.000Z 2026-04-04T17:15:00.000Z 2026-04-04T17:30:00.000Z
15 30 2 * * *     | 2026-04-04T16:30:00.000Z | 2026-04-04T15:30:15.000Z 2026-04-05T16:30:15.000Z 2026-04-06T16:30:15.000Z
0 9 * * MON-FRI   | 2026-07-01T12:34:56.789Z | 2026-07-01T23:00:00.000Z 2026-07-02T23:00:00.000Z 2026-07-05T23:00:00.000Z
";

    /// Local time comes from TZ, which is process-wide, so the rows run in
    /// a child of this test binary with TZ set there alone.
    #[test]
    fn next_fires_in_the_local_zone_match_croner() {
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--ignored",
                "--exact",
                "triggers::tests::local_fires_in_sydney",
            ])
            .env("TZ", "Australia/Sydney")
            .status()
            .expect("runs");
        assert!(status.success());
    }

    #[test]
    #[ignore = "run by next_fires_in_the_local_zone_match_croner with TZ set"]
    fn local_fires_in_sydney() {
        assert_eq!(std::env::var("TZ").as_deref(), Ok("Australia/Sydney"));
        let rows = LOCAL_FIRES.lines().filter(|row| !row.is_empty());
        for row in rows {
            let fields: Vec<&str> = row.split('|').map(str::trim).collect();
            let [expression, from, fires] = fields[..] else {
                panic!("{row}");
            };
            let trigger = Trigger::Cron {
                expression: expression.to_owned(),
                timezone: None,
            };
            let mut cursor = ms(from);
            for next in fires.split(' ') {
                cursor = next_fire_at(&trigger, cursor)
                    .unwrap_or_else(|| panic!("{expression} from {from} stopped"));
                assert_eq!(cursor, ms(next), "{expression} from {from}");
            }
        }
    }

    /// --tz input as Intl canonicalized or refused it in 0.2.1.
    #[test]
    fn zone_names_canonicalize_like_intl() {
        let rows: &[(&str, Option<&str>)] = &[
            ("Australia/Sydney", Some("Australia/Sydney")),
            ("australia/sydney", Some("Australia/Sydney")),
            ("AUSTRALIA/SYDNEY", Some("Australia/Sydney")),
            ("aUsTrAlIa/MeLbOuRnE", Some("Australia/Melbourne")),
            ("america/new_york", Some("America/New_York")),
            ("europe/berlin", Some("Europe/Berlin")),
            ("UTC", Some("UTC")),
            ("utc", Some("UTC")),
            ("Etc/UTC", Some("Etc/UTC")),
            ("etc/utc", Some("Etc/UTC")),
            ("Etc/GMT", Some("Etc/GMT")),
            ("GMT", Some("GMT")),
            ("gmt", Some("GMT")),
            ("Etc/GMT+5", Some("Etc/GMT+5")),
            ("etc/gmt-10", Some("Etc/GMT-10")),
            ("Z", None),
            ("Zulu", Some("Zulu")),
            ("Universal", Some("Universal")),
            ("UCT", Some("UCT")),
            ("US/Eastern", Some("US/Eastern")),
            ("us/pacific", Some("US/Pacific")),
            ("EST", Some("EST")),
            ("EST5EDT", Some("EST5EDT")),
            ("CET", Some("CET")),
            ("Asia/Calcutta", Some("Asia/Calcutta")),
            ("Asia/Kolkata", Some("Asia/Kolkata")),
            ("Asia/Saigon", Some("Asia/Saigon")),
            ("Asia/Ho_Chi_Minh", Some("Asia/Ho_Chi_Minh")),
            ("Europe/Kiev", Some("Europe/Kiev")),
            ("Europe/Kyiv", Some("Europe/Kyiv")),
            ("America/Buenos_Aires", Some("America/Buenos_Aires")),
            (
                "America/Argentina/Buenos_Aires",
                Some("America/Argentina/Buenos_Aires"),
            ),
            ("America/Indianapolis", Some("America/Indianapolis")),
            (
                "America/Indiana/Indianapolis",
                Some("America/Indiana/Indianapolis"),
            ),
            ("Australia/ACT", Some("Australia/ACT")),
            ("Australia/Canberra", Some("Australia/Canberra")),
            ("Antarctica/South_Pole", Some("Antarctica/South_Pole")),
            ("Africa/Asmera", Some("Africa/Asmera")),
            ("Pacific/Auckland", Some("Pacific/Auckland")),
            ("NZ", Some("NZ")),
            ("+10:00", Some("+10:00")),
            ("+1000", Some("+10:00")),
            ("-05:00", Some("-05:00")),
            ("Mars/Olympus", None),
            ("Australia/Sydney ", None),
            (" Australia/Sydney", None),
            ("Australia//Sydney", None),
            ("local", None),
            ("", None),
            ("+10", Some("+10:00")),
            ("-00:00", Some("+00:00")),
            ("+00:00", Some("+00:00")),
            ("-00", Some("+00:00")),
            ("+24:00", None),
            ("+23:59", Some("+23:59")),
            ("+10:60", None),
            ("+1:00", None),
            ("+10:0", None),
            ("+10:00:00", None),
            ("+100000", None),
            ("\u{2212}10:00", None),
            ("+10:30", Some("+10:30")),
            ("-0530", Some("-05:30")),
            ("utc+10", None),
            ("UTC+10", None),
            ("GMT+10", None),
            ("Etc/GMT-14", Some("Etc/GMT-14")),
            ("Etc/GMT+12", Some("Etc/GMT+12")),
            ("Etc/GMT+13", None),
            ("est5edt", Some("EST5EDT")),
            ("SystemV/EST5", None),
            ("America/Godthab", Some("America/Godthab")),
            ("Pacific/Enderbury", Some("Pacific/Enderbury")),
            ("Factory", None),
            ("ROC", Some("ROC")),
            ("PRC", Some("PRC")),
            ("Etc/Unknown", None),
            ("Canada/East-Saskatchewan", Some("Canada/East-Saskatchewan")),
            ("US/Pacific-New", Some("US/Pacific-New")),
            ("MST", Some("MST")),
            ("HST", Some("HST")),
            ("WET", Some("WET")),
        ];
        for (input, want) in rows {
            match (require_timezone(input), want) {
                (Ok(got), Some(want)) => assert_eq!(&got, want, "{input:?}"),
                (Err(error), None) => {
                    assert_eq!(error.code, "invalid_timezone", "{input:?}");
                    assert_eq!(error.exit_code, 2, "{input:?}");
                }
                (got, want) => panic!("{input:?}: {got:?}, wanted {want:?}"),
            }
        }
    }

    /// Expressions croner refused in 0.2.1 with the message it gave, and
    /// odd ones it took.
    #[test]
    fn invalid_cron_matches_croner() {
        for (expression, reason) in [
            (
                "",
                "CronPattern: invalid configuration format (''), exactly five, six, or seven space separated parts are required.",
            ),
            (
                "not a cron",
                "CronPattern: invalid configuration format ('not a cron'), exactly five, six, or seven space separated parts are required.",
            ),
            (
                "* * * *",
                "CronPattern: invalid configuration format ('* * * *'), exactly five, six, or seven space separated parts are required.",
            ),
            (
                "@sometimes",
                "CronPattern: invalid configuration format ('@sometimes'), exactly five, six, or seven space separated parts are required.",
            ),
            ("61 * * * *", "CronPattern: Invalid value for minute: 61"),
            ("0 25 * * *", "CronPattern: Invalid value for hour: 25"),
            ("0 9 32 * *", "CronPattern: Invalid value for day: 31"),
            ("0 9 0 * *", "CronPattern: Invalid value for day: -1"),
            ("0 9 * 13 *", "CronPattern: Invalid value for month: 12"),
            ("0 9 * * 8", "CronPattern: Invalid value for dayOfWeek: 8"),
            (
                "0 9 * * MON#6",
                "CronPattern: nth weekday out of range, should be 1-5 or L. Value: 6, Type: string",
            ),
            (
                "0 9 32W * *",
                "CronPattern: Invalid value for nearestWeekdays: 31",
            ),
            (
                "*/0 * * * *",
                "CronPattern: Syntax error, illegal stepping: 0",
            ),
            (
                "5-1 * * * *",
                "CronPattern: From value is larger than to value: '5-1'",
            ),
            (
                "0 9 * * FOO",
                "CronPattern: configuration entry 5 (FOO) contains illegal characters.",
            ),
            (
                "0/10 * * * *",
                "CronPattern: Syntax error, stepping with numeric prefix ('0/10') is not allowed. Use wildcard (*/step) or range (min-max/step) instead.",
            ),
            (
                "0 9 15W-20 * *",
                "CronPattern: Syntax error, W is not allowed in a range.",
            ),
            (
                "@reboot",
                "CronPattern: @reboot is not supported in this environment. This is an event-based trigger that requires system startup detection.",
            ),
            (
                "0 0 0 1 1 * 10000",
                "CronPattern: Invalid value for year: 10000 (supported range: 1-9999)",
            ),
            (
                "1W * * * *",
                "CronPattern: configuration entry 1 (1W) contains illegal characters.",
            ),
        ] {
            let error = parse_trigger(Some(expression), None, Some("UTC")).unwrap_err();
            assert_eq!(error.code, "invalid_cron", "{expression:?}");
            assert_eq!(error.exit_code, 2, "{expression:?}");
            assert_eq!(
                error.message,
                format!("Invalid cron expression \"{expression}\": {reason}")
            );
        }
        for expression in [
            "1 2 3 4 5 6 7",
            "0 9 ? * *",
            "*/60 * * * *",
            "0 9 * * 5#",
            "0 9 * * 5#l",
            "0 9 * * +1",
            "@YEARLY",
            "0 9 * jan-dec *",
            "0 9 * * sun-sat",
            "0\t9 * *\t*",
            " 0 9 * * * ",
        ] {
            assert!(
                parse_trigger(Some(expression), None, Some("UTC")).is_ok(),
                "{expression:?}"
            );
        }
    }
}
