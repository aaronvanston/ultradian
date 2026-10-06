//! Reading parsed option values, and the few checks 0.2.1 left to zod after
//! Commander: their `invalid_options` errors carry zod's message text and
//! issue objects, which `--json` prints as `details`.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::commander::OptValue;
use crate::errors::AppError;

pub type Options = HashMap<String, OptValue>;

/// A value option given on the command line (a string).
pub fn string(options: &Options, name: &str) -> Option<String> {
    match options.get(name) {
        Some(OptValue::Str(value)) => Some(value.clone()),
        Some(OptValue::Default(Value::String(value))) => Some(value.clone()),
        _ => None,
    }
}

/// A boolean flag such as `--yes`.
pub fn flag(options: &Options, name: &str) -> bool {
    matches!(options.get(name), Some(OptValue::Bool(true)))
}

/// An option with a `--no-*` twin: unset, removed (`--no-x`), or a value.
pub enum Toggle {
    Unset,
    Removed,
    Value(String),
}

pub fn toggle(options: &Options, name: &str) -> Toggle {
    match options.get(name) {
        Some(OptValue::Bool(false)) => Toggle::Removed,
        Some(OptValue::Str(value)) => Toggle::Value(value.clone()),
        _ => Toggle::Unset,
    }
}

/// zod's invalid_options error from its issues: each issue's message,
/// prefixed with its path, joined with "; ".
pub fn invalid_options(issues: Vec<Value>) -> AppError {
    let message = issues
        .iter()
        .map(|issue| {
            let path: Vec<String> = issue["path"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .map(|part| part.as_str().unwrap_or_default().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            let prefix = if path.is_empty() {
                String::new()
            } else {
                format!("{}: ", path.join("."))
            };
            format!("{prefix}{}", issue["message"].as_str().unwrap_or_default())
        })
        .collect::<Vec<_>>()
        .join("; ");
    AppError::usage("invalid_options", message).details(Value::Array(issues))
}

/// A required string option zod found missing.
pub fn missing_string(name: &str) -> AppError {
    invalid_options(vec![json!({
        "expected": "string",
        "code": "invalid_type",
        "path": [name],
        "message": "Invalid input: expected string, received undefined",
    })])
}

/// JavaScript's `Number(text)`.
fn js_number(text: &str) -> f64 {
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if trimmed.is_empty() {
        return 0.0;
    }
    match trimmed {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return digits.chars().fold(0.0, |total, c| {
                total * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            });
        }
    }
    let body = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(index) => (&body[..index], Some(&body[index + 1..])),
        None => (body, None),
    };
    let digits = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((whole, fraction)) => {
            digits(whole) && digits(fraction) && !(whole.is_empty() && fraction.is_empty())
        }
        None => !mantissa.is_empty() && digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|part| {
        let unsigned = part.strip_prefix(['+', '-']).unwrap_or(part);
        !unsigned.is_empty() && digits(unsigned)
    });
    if !mantissa_ok || !exponent_ok {
        return f64::NAN;
    }
    trimmed.parse().unwrap_or(f64::NAN)
}

const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

/// A lower or upper bound on a number, as zod words it.
#[derive(Clone, Copy)]
pub struct Bound {
    pub value: i64,
    pub inclusive: bool,
}

/// `z.coerce.number().int()` with optional bounds, on a `--limit`-style
/// option. Returns the number, or zod's error.
pub fn coerce_int(
    name: &str,
    value: &OptValue,
    minimum: Option<Bound>,
    maximum: Option<Bound>,
) -> Result<i64, AppError> {
    let number = match value {
        OptValue::Str(text) => js_number(text),
        OptValue::Default(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN),
        OptValue::Default(Value::String(text)) => js_number(text),
        OptValue::Bool(true) => 1.0,
        _ => 0.0,
    };
    if number.is_nan() {
        return Err(invalid_options(vec![json!({
            "expected": "number",
            "code": "invalid_type",
            "received": "NaN",
            "path": [name],
            "message": "Invalid input: expected number, received NaN",
        })]));
    }
    if number.is_infinite() {
        return Err(invalid_options(vec![json!({
            "expected": "number",
            "code": "invalid_type",
            "received": "Infinity",
            "path": [name],
            "message": "Invalid input: expected number, received number",
        })]));
    }
    if number.fract() != 0.0 {
        return Err(invalid_options(vec![json!({
            "expected": "int",
            "format": "safeint",
            "code": "invalid_type",
            "path": [name],
            "message": "Invalid input: expected int, received number",
        })]));
    }
    let mut issues = Vec::new();
    if number > MAX_SAFE {
        issues.push(json!({
            "code": "too_big",
            "maximum": 9_007_199_254_740_991_i64,
            "note": "Integers must be within the safe integer range.",
            "origin": "int",
            "inclusive": true,
            "path": [name],
            "message": "Too big: expected int to be <=9007199254740991",
        }));
    } else if number < -MAX_SAFE {
        issues.push(json!({
            "code": "too_small",
            "minimum": -9_007_199_254_740_991_i64,
            "note": "Integers must be within the safe integer range.",
            "origin": "int",
            "inclusive": true,
            "path": [name],
            "message": "Too small: expected int to be >=-9007199254740991",
        }));
    }
    if let Some(bound) = minimum {
        let below = if bound.inclusive {
            number < bound.value as f64
        } else {
            number <= bound.value as f64
        };
        if below {
            let sign = if bound.inclusive { ">=" } else { ">" };
            issues.push(json!({
                "origin": "number",
                "code": "too_small",
                "minimum": bound.value,
                "inclusive": bound.inclusive,
                "path": [name],
                "message": format!("Too small: expected number to be {sign}{}", bound.value),
            }));
        }
    }
    if let Some(bound) = maximum {
        let above = if bound.inclusive {
            number > bound.value as f64
        } else {
            number >= bound.value as f64
        };
        if above {
            let sign = if bound.inclusive { "<=" } else { "<" };
            issues.push(json!({
                "origin": "number",
                "code": "too_big",
                "maximum": bound.value,
                "inclusive": bound.inclusive,
                "path": [name],
                "message": format!("Too big: expected number to be {sign}{}", bound.value),
            }));
        }
    }
    if issues.is_empty() {
        // Within the safe range, so the conversion is exact.
        Ok(number as i64)
    } else {
        Err(invalid_options(issues))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Limits outside their bounds, or not whole numbers, are usage errors
    /// that name the option.
    #[test]
    fn coerces_limits_and_refuses_out_of_range_values() {
        let positive = Some(Bound {
            value: 0,
            inclusive: false,
        });
        let at_most_200 = Some(Bound {
            value: 200,
            inclusive: true,
        });
        let value = |text: &str| OptValue::Str(text.into());
        assert_eq!(
            coerce_int("limit", &value("500"), positive, None).ok(),
            Some(500)
        );
        assert_eq!(
            coerce_int(
                "limit",
                &OptValue::Default(json!(10)),
                positive,
                at_most_200
            )
            .ok(),
            Some(10)
        );
        for (text, message) in [
            ("0", "limit: Too small: expected number to be >0"),
            ("1.5", "limit: Invalid input: expected int, received number"),
            ("201", "limit: Too big: expected number to be <=200"),
        ] {
            let error = coerce_int("limit", &value(text), positive, at_most_200).unwrap_err();
            assert_eq!(
                (error.code.as_str(), error.exit_code, error.message.as_str()),
                ("invalid_options", 2, message),
                "{text}"
            );
        }
    }
}
