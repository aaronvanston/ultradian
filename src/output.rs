//! The machine-readable surface: success and error envelopes, and the time
//! format every record uses. 0.2.1 built these with JSON.stringify, and
//! scripts that consume the output parse the text, so spacing, key order
//! and number formatting are part of the contract.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::errors::AppError;

/// Every envelope, success or error, and the command catalog carry this
/// version. It changes whenever a public record shape does.
pub const SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Human,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

/// The global flags, as one invocation resolved them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Globals {
    pub color: ColorMode,
    pub compact: bool,
    pub mode: Mode,
    pub non_interactive: bool,
    pub quiet: bool,
    pub verbose: bool,
}

/// Milliseconds since the epoch, the unit every stored time uses.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// `Date.prototype.toISOString`: UTC, always three decimals and a `Z`
/// (`2026-08-06T10:30:00.000Z`). Years outside 0000-9999 get the expanded
/// six-digit form with a sign, as JavaScript writes them.
pub fn iso_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let in_day = ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hours = in_day / 3_600_000;
    let minutes = in_day / 60_000 % 60;
    let seconds = in_day / 1000 % 60;
    let millis = in_day % 1000;
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", -year)
    } else {
        format!("+{year:06}")
    };
    format!("{year}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `JSON.stringify(value, null, 2)`, or one line with `compact`. serde_json's
/// formatter already matches JavaScript's: two-space indent, `": "`, `[]`
/// and `{}` for empty containers, lowercase `\u00xx` escapes.
pub fn to_json<T: Serialize + ?Sized>(value: &T, compact: bool) -> String {
    let rendered = if compact {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    };
    // Serializing plain data into a string cannot fail.
    rendered.unwrap_or_default()
}

/// What a command hands back on success, before it is rendered.
#[derive(Debug, Clone)]
pub struct Outcome<T> {
    pub data: T,
    pub exit_code: i32,
    pub hint: Option<String>,
    pub warnings: Vec<String>,
}

impl<T> Outcome<T> {
    pub fn new(data: T) -> Self {
        Self {
            data,
            exit_code: crate::errors::exit::OK,
            hint: None,
            warnings: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct Envelope<'a, T: Serialize> {
    command: &'a str,
    data: &'a T,
    ok: bool,
    #[serde(rename = "schemaVersion")]
    schema_version: i64,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    warnings: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
}

#[derive(Serialize)]
struct JsonlRecord<'a, T: Serialize> {
    timestamp: String,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    envelope: Envelope<'a, T>,
}

/// The success envelope for `--json` or `--jsonl`.
pub fn success_envelope<T: Serialize>(
    command: &str,
    outcome: &Outcome<T>,
    globals: &Globals,
) -> String {
    let envelope = Envelope {
        command,
        data: &outcome.data,
        ok: true,
        schema_version: SCHEMA_VERSION,
        warnings: &outcome.warnings,
        hint: outcome.hint.as_deref(),
    };
    if globals.mode == Mode::Jsonl {
        let record = JsonlRecord {
            timestamp: iso_ms(now_ms()),
            kind: "result",
            envelope,
        };
        return to_json(&record, true);
    }
    to_json(&envelope, globals.compact)
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
    #[serde(rename = "docsUrl", skip_serializing_if = "Option::is_none")]
    docs_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a Value>,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
    ok: bool,
    #[serde(rename = "schemaVersion")]
    schema_version: i64,
}

/// The error envelope written to stderr in every machine mode. `--jsonl`
/// gets the same document as `--json`, pretty unless `--compact`, as 0.2.1
/// wrote it.
pub fn error_envelope(error: &AppError, compact: bool) -> String {
    let envelope = ErrorEnvelope {
        error: ErrorBody {
            code: &error.code,
            message: &error.message,
            hint: error.hint.as_deref(),
            docs_url: error.docs_url.as_deref(),
            details: error.details.as_ref(),
        },
        ok: false,
        schema_version: SCHEMA_VERSION,
    };
    to_json(&envelope, compact)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_times_like_to_iso_string() {
        assert_eq!(iso_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_ms(1_786_012_200_000), "2026-08-06T10:30:00.000Z");
        assert_eq!(iso_ms(1_798_761_599_999), "2026-12-31T23:59:59.999Z");
        assert_eq!(iso_ms(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso_ms(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn envelopes_keep_their_key_order_and_spacing() {
        let globals = Globals {
            color: ColorMode::Auto,
            compact: false,
            mode: Mode::Json,
            non_interactive: false,
            quiet: false,
            verbose: false,
        };
        let mut outcome = Outcome::new(serde_json::json!({ "seconds": 900, "list": [] }));
        outcome.hint = Some("start it".into());
        assert_eq!(
            success_envelope("x", &outcome, &globals),
            "{\n  \"command\": \"x\",\n  \"data\": {\n    \"seconds\": 900,\n    \"list\": []\n  },\n  \"ok\": true,\n  \"schemaVersion\": 2,\n  \"hint\": \"start it\"\n}"
        );
        let error = AppError::usage("invalid_usage", "bad \"x\"\n\u{1}");
        assert_eq!(
            error_envelope(&error, true),
            "{\"error\":{\"code\":\"invalid_usage\",\"message\":\"bad \\\"x\\\"\\n\\u0001\"},\"ok\":false,\"schemaVersion\":2}"
        );
    }
}
