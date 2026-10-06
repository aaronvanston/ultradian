//! What makes a schedule fire: cron expressions in a zone, fixed intervals,
//! or nothing (manual). Durations are parsed here too.

use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};

use crate::cron::{self, Pattern, Zone};
use crate::errors::AppError;

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
        let tz = chrono_tz::Tz::from_str_insensitive(target).ok()?;
        return Some(((*retired).to_owned(), Zone::Named(tz)));
    }
    chrono_tz::Tz::from_str_insensitive(name)
        .ok()
        .map(|tz| (tz.name().to_owned(), Zone::Named(tz)))
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
}

#[cfg(test)]
mod fixtures {
    //! The answers croner and Intl gave in 0.2.1, from
    //! legacy/scripts/cron-fixtures.ts.
    use super::*;
    use serde_json::Value;

    fn load(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture exists"))
            .expect("fixture is JSON")
    }

    #[test]
    fn next_fires_match_croner() {
        // SAFETY: tests that read local time run in this one test.
        unsafe { std::env::set_var("TZ", "Australia/Sydney") };
        let fixture = load("cron.json");
        let cases = fixture["cases"].as_array().expect("cases");
        let mut wrong = Vec::new();
        for case in cases {
            let trigger = Trigger::Cron {
                expression: case["expr"].as_str().unwrap_or_default().to_owned(),
                timezone: case["tz"].as_str().map(str::to_owned),
            };
            let mut cursor = case["from_ms"].as_i64();
            let mut got = Vec::new();
            for _ in 0..3 {
                cursor = cursor.and_then(|from| next_fire_at(&trigger, from));
                got.push(cursor);
                if cursor.is_none() {
                    break;
                }
            }
            let want: Vec<Option<i64>> = case["next_ms"]
                .as_array()
                .expect("next")
                .iter()
                .map(Value::as_i64)
                .collect();
            if got != want {
                wrong.push(format!(
                    "{} {:?} {} {:?} != {:?}",
                    case["expr"], case["tz"], case["label"], got, want
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} differ, e.g.\n{}",
            wrong.len(),
            cases.len(),
            wrong[..wrong.len().min(15)].join("\n")
        );
    }

    #[test]
    fn zone_names_canonicalize_like_intl() {
        let fixture = load("tz-names.json");
        let cases = fixture["cases"].as_array().expect("cases");
        let mut wrong = Vec::new();
        for case in cases {
            let input = case["input"].as_str().unwrap_or_default();
            let got = require_timezone(input).ok();
            let want = case["canonical"].as_str().map(str::to_owned);
            if got != want {
                wrong.push(format!("{input:?}: {got:?} != {want:?}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} differ:\n{}",
            wrong.len(),
            cases.len(),
            wrong[..wrong.len().min(30)].join("\n")
        );
    }
}

#[cfg(test)]
mod error_fixtures {
    use serde_json::Value;

    use super::*;

    /// Every expression croner refused, with the message invalid_cron
    /// carries, and the odd ones it took.
    #[test]
    fn invalid_cron_matches_croner() {
        let path = format!(
            "{}/tests/fixtures/cron-errors.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let fixture: Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("fixture exists"))
                .expect("fixture is JSON");
        let cases = fixture["cases"].as_array().expect("cases");
        let mut wrong = Vec::new();
        for case in cases {
            let expression = case["expr"].as_str().unwrap_or_default();
            let got = parse_trigger(Some(expression), None, Some("UTC"));
            let ok = match (&got, case["accepted"].as_bool()) {
                (Ok(_), Some(true)) => true,
                (Err(error), None) => {
                    error.code == case["code"].as_str().unwrap_or_default()
                        && error.message == case["message"].as_str().unwrap_or_default()
                        && error.exit_code == 2
                }
                _ => false,
            };
            if !ok {
                wrong.push(format!("{expression:?}: {got:?}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} differ:\n{}",
            wrong.len(),
            cases.len(),
            wrong.join("\n")
        );
    }
}
