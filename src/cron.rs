//! Cron patterns and next fires, ported line for line from croner 10.0.1
//! (src/pattern.ts, src/date.ts, src/helpers/timezone.ts), which is what
//! 0.2.1 ran. A port rather than the Rust croner crate, because the crate
//! differs from the JavaScript one where schedules are most fragile: it
//! fires again in the hour a DST overlap repeats, it words its errors
//! differently, and it doesn't take the same odd spellings. Here the
//! quirks are kept on purpose, including ones that look like bugs (an
//! hour above 59 is never normalized; a fire found from inside the second
//! half of an overlap can land before the instant it was asked about),
//! because tests/fixtures/cron.json records exactly what croner answered.
//!
//! Wall-clock times convert through the zone's UTC offsets: chrono-tz for
//! IANA names, a fixed offset for `+10:00`-style zones, and the machine's
//! zone (honoring TZ) when a schedule has none, resolved the way JavaScript's
//! `new Date(y, m, d, ...)` resolves local times.

use chrono::{Local, NaiveDateTime, Offset, TimeZone};

/// A zone a cron expression is read in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zone {
    Named(chrono_tz::Tz),
    /// Seconds east of UTC.
    Fixed(i64),
    /// The machine's zone, for schedules with none.
    Local,
}

const LAST_OCCURRENCE: u32 = 0b10_0000;
const ANY_OCCURRENCE: u32 = 0b1_1111 | LAST_OCCURRENCE;
const OCCURRENCE_BITMASKS: [u32; 5] = [0b1, 0b10, 0b100, 0b1000, 0b1_0000];
const DAYS_OF_MONTH: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Second,
    Minute,
    Hour,
    Day,
    Month,
    DayOfWeek,
    NearestWeekdays,
    Year,
}

impl Part {
    fn name(self) -> &'static str {
        match self {
            Self::Second => "second",
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Month => "month",
            Self::DayOfWeek => "dayOfWeek",
            Self::NearestWeekdays => "nearestWeekdays",
            Self::Year => "year",
        }
    }
}

/// A value a part is set to: croner passes either the part's default or
/// the text after `#` (or `L`), and only weekdays read the text.
#[derive(Clone)]
enum Nth {
    Default(u32),
    Text(String),
}

/// A parsed pattern, field by field, as croner's CronPattern holds it.
#[derive(Debug, Clone)]
pub struct Pattern {
    second: Vec<u32>,
    minute: Vec<u32>,
    hour: Vec<u32>,
    day: Vec<u32>,
    month: Vec<u32>,
    day_of_week: Vec<u32>,
    year: Vec<u32>,
    nearest_weekdays: Vec<u32>,
    last_day_of_month: bool,
    last_weekday: bool,
    star_dom: bool,
    star_dow: bool,
    star_year: bool,
    use_and_logic: bool,
}

/// JavaScript's parseInt(text, 10): leading whitespace, a sign, then as
/// many digits as there are; none is NaN (`None`).
fn parse_int(text: &str) -> Option<i64> {
    let trimmed = text.trim_start();
    let (negative, rest) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let value = digits.parse::<i64>().unwrap_or(i64::MAX);
    Some(if negative { -value } else { value })
}

/// `text.replace(/needle/gi, replacement)` for an ASCII needle.
fn replace_ci(text: &str, needle: &str, replacement: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut result = String::new();
    let mut index = 0;
    while let Some(found) = lower[index..].find(needle) {
        result.push_str(&text[index..index + found]);
        result.push_str(replacement);
        index += found + needle.len();
    }
    result.push_str(&text[index..]);
    result
}

fn error(message: impl Into<String>) -> String {
    format!("CronPattern: {}", message.into())
}

impl Pattern {
    /// Parses a pattern, or returns croner's error message.
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut pattern = Self {
            second: vec![0; 60],
            minute: vec![0; 60],
            hour: vec![0; 24],
            day: vec![0; 31],
            month: vec![0; 12],
            day_of_week: vec![0; 7],
            year: vec![0; 10_000],
            nearest_weekdays: vec![0; 31],
            last_day_of_month: false,
            last_weekday: false,
            star_dom: false,
            star_dow: false,
            star_year: false,
            use_and_logic: false,
        };
        let mut text = source.to_owned();
        if text.contains('@') {
            text = handle_nicknames(&text)?.trim().to_owned();
        }
        let mut parts: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
        if parts.is_empty() {
            parts.push(String::new());
        }
        if parts.len() < 5 || parts.len() > 7 {
            return Err(error(format!(
                "invalid configuration format ('{text}'), exactly five, six, or seven space separated parts are required."
            )));
        }
        if parts.len() == 5 {
            parts.insert(0, "0".into());
        }
        if parts.len() == 6 {
            parts.push("*".into());
        }
        if parts[3].to_uppercase() == "LW" {
            pattern.last_weekday = true;
            parts[3].clear();
        } else if parts[3].to_uppercase().contains('L') {
            parts[3] = parts[3].replace(['L', 'l'], "");
            pattern.last_day_of_month = true;
        }
        pattern.star_dom = parts[3] == "*";
        pattern.star_year = parts[6] == "*";
        if parts[4].len() >= 3 {
            parts[4] = replace_alpha_months(&parts[4]);
        }
        if parts[5].len() >= 3 {
            parts[5] = replace_alpha_days(&parts[5]);
        }
        if let Some(rest) = parts[5].strip_prefix('+') {
            pattern.use_and_logic = true;
            parts[5] = rest.to_owned();
            if parts[5].is_empty() {
                return Err(error(
                    "Day-of-week field cannot be empty after '+' modifier.",
                ));
            }
        }
        pattern.star_dow = parts[5] == "*";
        if text.contains('?') {
            for part in &mut parts {
                *part = part.replace('?', "*");
            }
        }
        for (index, part) in parts.iter().enumerate() {
            let allowed = |c: char| {
                matches!(c, '/' | '*' | ',' | '-' | '0'..='9')
                    || (index == 3 && matches!(c, 'W' | 'w' | 'L' | 'l'))
                    || (index == 5 && matches!(c, '#' | 'L' | 'l'))
            };
            if !part.chars().all(allowed) {
                return Err(error(format!(
                    "configuration entry {index} ({part}) contains illegal characters."
                )));
            }
        }
        pattern.part_to_array(Part::Second, &parts[0], 0, 1)?;
        pattern.part_to_array(Part::Minute, &parts[1], 0, 1)?;
        pattern.part_to_array(Part::Hour, &parts[2], 0, 1)?;
        pattern.part_to_array(Part::Day, &parts[3], -1, 1)?;
        pattern.part_to_array(Part::Month, &parts[4], -1, 1)?;
        pattern.part_to_array(Part::DayOfWeek, &parts[5], 0, ANY_OCCURRENCE)?;
        pattern.part_to_array(Part::Year, &parts[6], 0, 1)?;
        Ok(pattern)
    }

    fn array(&mut self, part: Part) -> &mut Vec<u32> {
        match part {
            Part::Second => &mut self.second,
            Part::Minute => &mut self.minute,
            Part::Hour => &mut self.hour,
            Part::Day => &mut self.day,
            Part::Month => &mut self.month,
            Part::DayOfWeek => &mut self.day_of_week,
            Part::NearestWeekdays => &mut self.nearest_weekdays,
            Part::Year => &mut self.year,
        }
    }

    fn part_to_array(
        &mut self,
        part: Part,
        conf: &str,
        offset: i64,
        default: u32,
    ) -> Result<(), String> {
        let last_day_of_month = part == Part::Day && self.last_day_of_month;
        let last_weekday = part == Part::Day && self.last_weekday;
        if conf.is_empty() && !last_day_of_month && !last_weekday {
            return Err(error(format!(
                "configuration entry {} ({conf}) is empty, check for trailing spaces.",
                part.name()
            )));
        }
        if conf == "*" {
            self.array(part).fill(default);
            return Ok(());
        }
        let split: Vec<&str> = conf.split(',').collect();
        if split.len() > 1 {
            for piece in split {
                self.part_to_array(part, piece, offset, default)?;
            }
        } else if conf.contains('-') && conf.contains('/') {
            self.handle_range_with_stepping(conf, part, offset, default)?;
        } else if conf.contains('-') {
            self.handle_range(conf, part, offset, default)?;
        } else if conf.contains('/') {
            self.handle_stepping(conf, part, default)?;
        } else if !conf.is_empty() {
            self.handle_number(conf, part, offset, default)?;
        }
        Ok(())
    }

    fn handle_number(
        &mut self,
        conf: &str,
        part: Part,
        offset: i64,
        default: u32,
    ) -> Result<(), String> {
        let (rest, nth) = extract_nth(conf, part)?;
        let nearest = conf.to_uppercase().contains('W');
        if part != Part::Day && nearest {
            return Err(error(
                "Nearest weekday modifier (W) only allowed in day-of-month.",
            ));
        }
        let part = if nearest { Part::NearestWeekdays } else { part };
        let Some(value) = parse_int(&rest) else {
            return Err(error(format!("{} is not a number: '{conf}'", part.name())));
        };
        self.set_part(part, value + offset, value_of(nth, default))
    }

    fn set_part(&mut self, part: Part, index: i64, value: Nth) -> Result<(), String> {
        if part == Part::DayOfWeek {
            let index = if index == 7 { 0 } else { index };
            if !(0..=6).contains(&index) {
                return Err(error(format!("Invalid value for dayOfWeek: {index}")));
            }
            return self.set_nth_weekday(index as usize, value);
        }
        let in_range = match part {
            Part::Second | Part::Minute => (0..60).contains(&index),
            Part::Hour => (0..24).contains(&index),
            Part::Day | Part::NearestWeekdays => (0..31).contains(&index),
            Part::Month => (0..12).contains(&index),
            Part::Year => (1..10_000).contains(&index),
            Part::DayOfWeek => true,
        };
        if !in_range {
            let suffix = if part == Part::Year {
                " (supported range: 1-9999)"
            } else {
                ""
            };
            return Err(error(format!(
                "Invalid value for {}: {index}{suffix}",
                part.name()
            )));
        }
        // Only weekdays ever get text; anything else gets its default.
        let value = match value {
            Nth::Default(value) => value,
            Nth::Text(_) => 1,
        };
        self.array(part)[index as usize] = value;
        Ok(())
    }

    fn set_nth_weekday(&mut self, index: usize, nth: Nth) -> Result<(), String> {
        match nth {
            Nth::Text(text) if text.to_uppercase() == "L" => {
                self.day_of_week[index] |= LAST_OCCURRENCE;
            }
            Nth::Default(ANY_OCCURRENCE) => self.day_of_week[index] = ANY_OCCURRENCE,
            Nth::Text(text) => {
                // JavaScript compares the text as a number: "" is 0, and
                // anything that isn't digits is NaN.
                let number = if text.is_empty() {
                    Some(0)
                } else if text.bytes().all(|byte| byte.is_ascii_digit()) {
                    text.parse::<i64>().ok()
                } else {
                    None
                };
                match number {
                    Some(n @ 1..=5) => {
                        self.day_of_week[index] |= OCCURRENCE_BITMASKS[(n - 1) as usize]
                    }
                    _ => {
                        return Err(error(format!(
                            "nth weekday out of range, should be 1-5 or L. Value: {text}, Type: string"
                        )));
                    }
                }
            }
            Nth::Default(other) => {
                return Err(error(format!(
                    "nth weekday out of range, should be 1-5 or L. Value: {other}, Type: number"
                )));
            }
        }
        Ok(())
    }

    fn validate_range(
        &mut self,
        lower: i64,
        upper: i64,
        steps: Option<i64>,
        part: Part,
        conf: &str,
    ) -> Result<(), String> {
        if lower > upper {
            return Err(error(format!(
                "From value is larger than to value: '{conf}'"
            )));
        }
        if let Some(steps) = steps {
            if steps == 0 {
                return Err(error("Syntax error, illegal stepping: 0"));
            }
            let length = self.array(part).len() as i64;
            if steps > length {
                return Err(error(format!(
                    "Syntax error, steps cannot be greater than maximum value of part ({length})"
                )));
            }
        }
        Ok(())
    }

    fn handle_range_with_stepping(
        &mut self,
        conf: &str,
        part: Part,
        offset: i64,
        default: u32,
    ) -> Result<(), String> {
        if conf.to_uppercase().contains('W') {
            return Err(error(
                "Syntax error, W is not allowed in ranges with stepping.",
            ));
        }
        let (rest, nth) = extract_nth(conf, part)?;
        let groups = rest.split_once('-').and_then(|(lower, tail)| {
            let (upper, step) = tail.split_once('/')?;
            let digits =
                |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
            (digits(lower) && digits(upper) && digits(step)).then_some((lower, upper, step))
        });
        let Some((lower, upper, step)) = groups else {
            return Err(error(format!(
                "Syntax error, illegal range with stepping: '{conf}'"
            )));
        };
        let lower = parse_int(lower).unwrap_or(0) + offset;
        let upper = parse_int(upper).unwrap_or(0) + offset;
        let steps = parse_int(step).unwrap_or(0);
        self.validate_range(lower, upper, Some(steps), part, conf)?;
        let mut index = lower;
        while index <= upper {
            self.set_part(part, index, value_of(nth.clone(), default))?;
            index += steps;
        }
        Ok(())
    }

    fn handle_range(
        &mut self,
        conf: &str,
        part: Part,
        offset: i64,
        default: u32,
    ) -> Result<(), String> {
        if conf.to_uppercase().contains('W') {
            return Err(error("Syntax error, W is not allowed in a range."));
        }
        let (rest, nth) = extract_nth(conf, part)?;
        let split: Vec<&str> = rest.split('-').collect();
        if split.len() != 2 {
            return Err(error(format!("Syntax error, illegal range: '{conf}'")));
        }
        let Some(lower) = parse_int(split[0]) else {
            return Err(error("Syntax error, illegal lower range (NaN)"));
        };
        let Some(upper) = parse_int(split[1]) else {
            return Err(error("Syntax error, illegal upper range (NaN)"));
        };
        let (lower, upper) = (lower + offset, upper + offset);
        self.validate_range(lower, upper, None, part, conf)?;
        for index in lower..=upper {
            self.set_part(part, index, value_of(nth.clone(), default))?;
        }
        Ok(())
    }

    fn handle_stepping(&mut self, conf: &str, part: Part, default: u32) -> Result<(), String> {
        if conf.to_uppercase().contains('W') {
            return Err(error(
                "Syntax error, W is not allowed in parts with stepping.",
            ));
        }
        let (rest, nth) = extract_nth(conf, part)?;
        let split: Vec<&str> = rest.split('/').collect();
        if split.len() != 2 {
            return Err(error(format!("Syntax error, illegal stepping: '{conf}'")));
        }
        if split[0].is_empty() {
            return Err(error(format!(
                "Syntax error, stepping with missing prefix ('{conf}') is not allowed. Use wildcard (*/step) or range (min-max/step) instead."
            )));
        }
        if split[0] != "*" {
            return Err(error(format!(
                "Syntax error, stepping with numeric prefix ('{conf}') is not allowed. Use wildcard (*/step) or range (min-max/step) instead."
            )));
        }
        let Some(steps) = parse_int(split[1]) else {
            return Err(error("Syntax error, illegal stepping: (NaN)"));
        };
        let length = self.array(part).len() as i64;
        self.validate_range(0, length - 1, Some(steps), part, conf)?;
        let mut index = 0;
        while index < length {
            self.set_part(part, index, value_of(nth.clone(), default))?;
            // A negative step can't reach here: '-' takes the range path.
            index += steps.max(1);
        }
        Ok(())
    }
}

/// `result[1] || defaultValue`: empty text counts as no text.
fn value_of(nth: Option<String>, default: u32) -> Nth {
    match nth {
        Some(text) if !text.is_empty() => Nth::Text(text),
        _ => Nth::Default(default),
    }
}

/// Splits off `#n` or a trailing `L`, which only weekdays may carry.
fn extract_nth(conf: &str, part: Part) -> Result<(String, Option<String>), String> {
    if conf.contains('#') {
        if part != Part::DayOfWeek {
            return Err(error("nth (#) only allowed in day-of-week field"));
        }
        let mut pieces = conf.split('#');
        let rest = pieces.next().unwrap_or_default().to_owned();
        let nth = pieces.next().unwrap_or_default().to_owned();
        return Ok((rest, Some(nth)));
    }
    if conf.to_uppercase().ends_with('L') {
        if part != Part::DayOfWeek {
            return Err(error(
                "L modifier only allowed in day-of-week field (use L alone for day-of-month)",
            ));
        }
        return Ok((conf[..conf.len() - 1].to_owned(), Some("L".into())));
    }
    Ok((conf.to_owned(), None))
}

fn replace_alpha_days(conf: &str) -> String {
    let mut text = replace_ci(conf, "-sun", "-7");
    for (name, number) in [
        ("sun", "0"),
        ("mon", "1"),
        ("tue", "2"),
        ("wed", "3"),
        ("thu", "4"),
        ("fri", "5"),
        ("sat", "6"),
    ] {
        text = replace_ci(&text, name, number);
    }
    text
}

fn replace_alpha_months(conf: &str) -> String {
    let mut text = conf.to_owned();
    for (name, number) in [
        ("jan", "1"),
        ("feb", "2"),
        ("mar", "3"),
        ("apr", "4"),
        ("may", "5"),
        ("jun", "6"),
        ("jul", "7"),
        ("aug", "8"),
        ("sep", "9"),
        ("oct", "10"),
        ("nov", "11"),
        ("dec", "12"),
    ] {
        text = replace_ci(&text, name, number);
    }
    text
}

fn handle_nicknames(pattern: &str) -> Result<String, String> {
    Ok(match pattern.trim().to_lowercase().as_str() {
        "@yearly" | "@annually" => "0 0 1 1 *".into(),
        "@monthly" => "0 0 1 * *".into(),
        "@weekly" => "0 0 * * 0".into(),
        "@daily" | "@midnight" => "0 0 * * *".into(),
        "@hourly" => "0 * * * *".into(),
        "@reboot" => {
            return Err(error(
                "@reboot is not supported in this environment. This is an event-based trigger that requires system startup detection.",
            ));
        }
        _ => pattern.to_owned(),
    })
}

// ---- Date arithmetic, as JavaScript's Date does it ------------------------

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `Date.UTC(y, month0, d, h, mi, s, ms)`, overflowing fields carried.
fn date_utc(year: i64, month0: i64, day: i64, hour: i64, minute: i64, second: i64, ms: i64) -> i64 {
    let year = year + month0.div_euclid(12);
    let month = month0.rem_euclid(12) + 1;
    let days = days_from_civil(year, month, 1) + day - 1;
    days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000 + ms
}

/// Calendar fields of a millisecond count read as UTC: (year, month0,
/// day, hour, minute, second, ms).
fn utc_fields(ms: i64) -> (i64, i64, i64, i64, i64, i64, i64) {
    let days = ms.div_euclid(86_400_000);
    let in_day = ms.rem_euclid(86_400_000);
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
    (
        year,
        month - 1,
        day,
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000,
    )
}

/// `new Date(Date.UTC(y, m, d)).getUTCDay()`.
fn weekday(year: i64, month0: i64, day: i64) -> i64 {
    (date_utc(year, month0, day, 0, 0, 0, 0).div_euclid(86_400_000) + 4).rem_euclid(7)
}

fn naive(ms: i64) -> Option<NaiveDateTime> {
    chrono::DateTime::from_timestamp_millis(ms).map(|time| time.naive_utc())
}

/// The zone's offset from UTC at an instant, in milliseconds.
fn offset_ms(zone: Zone, instant: i64) -> i64 {
    let Some(utc) = naive(instant) else { return 0 };
    let seconds = match zone {
        Zone::Named(tz) => i64::from(tz.offset_from_utc_datetime(&utc).fix().local_minus_utc()),
        Zone::Fixed(seconds) => seconds,
        Zone::Local => i64::from(Local.offset_from_utc_datetime(&utc).local_minus_utc()),
    };
    seconds * 1000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimePoint {
    y: i64,
    m: i64,
    d: i64,
    h: i64,
    i: i64,
    s: i64,
}

impl TimePoint {
    fn ms(self) -> i64 {
        date_utc(self.y, self.m - 1, self.d, self.h, self.i, self.s, 0)
    }
}

/// croner's toTZ: the wall clock in the zone, to the second (Intl drops the
/// milliseconds).
fn to_tz(instant: i64, zone: Zone) -> TimePoint {
    let local = instant + offset_ms(zone, instant);
    let (y, m0, d, h, i, s, _) = utc_fields(local.div_euclid(1000) * 1000);
    TimePoint {
        y,
        m: m0 + 1,
        d,
        h,
        i,
        s,
    }
}

/// croner's fromTZ: the instant a wall clock names in the zone. Of two it
/// prefers the earlier; in a gap, the later of its two guesses.
fn from_tz(point: TimePoint, zone: Zone) -> i64 {
    let in_date = point.ms();
    let check0 = to_tz(in_date, zone);
    let guess = in_date + (point.ms() - check0.ms());
    let check1 = to_tz(guess, zone);
    if check1 == point {
        let earlier = guess - 3_600_000;
        return if to_tz(earlier, zone) == point {
            earlier
        } else {
            guess
        };
    }
    let guess2 = guess + point.ms() - check1.ms();
    if to_tz(guess2, zone) == point {
        return guess2;
    }
    guess.max(guess2)
}

/// `new Date(y, m, d, h, mi, s, ms)` in the machine's zone: the earlier of
/// two readings in an overlap, and the offset from before the change in a
/// gap, as ECMAScript specifies.
fn from_local(local_ms: i64) -> i64 {
    // Each offset in force within a day either side is a candidate; one is
    // right when reading the instant it gives back yields the same wall
    // clock. (chrono's own local-to-UTC lookup misplaces the hour just
    // after a fall-back, so it isn't used.)
    let before = offset_ms(Zone::Local, local_ms - 86_400_000);
    let candidates = [
        before,
        offset_ms(Zone::Local, local_ms),
        offset_ms(Zone::Local, local_ms + 86_400_000),
    ];
    candidates
        .iter()
        .map(|offset| local_ms - offset)
        .filter(|instant| offset_ms(Zone::Local, *instant) == local_ms - instant)
        .min()
        .unwrap_or(local_ms - before)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

/// (target, the field it carries into, offset into the pattern's array).
const STEPS: [(Target, Target, i64); 5] = [
    (Target::Month, Target::Year, 0),
    (Target::Day, Target::Month, -1),
    (Target::Hour, Target::Day, 0),
    (Target::Minute, Target::Hour, 0),
    (Target::Second, Target::Minute, 0),
];

/// croner's CronDate: a wall clock that walks forward to the next match.
struct CronDate {
    zone: Zone,
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    ms: i64,
}

fn last_day_of_month(year: i64, month: i64) -> Option<i64> {
    if month == 1 {
        Some(utc_fields(date_utc(year, 2, 0, 0, 0, 0, 0)).2)
    } else {
        usize::try_from(month)
            .ok()
            .and_then(|index| DAYS_OF_MONTH.get(index).copied())
    }
}

fn last_weekday(year: i64, month: i64) -> i64 {
    let last = last_day_of_month(year, month).unwrap_or(0);
    match weekday(year, month, last) {
        0 => last - 2,
        6 => last - 1,
        _ => last,
    }
}

fn nearest_weekday(year: i64, month: i64, day: i64) -> i64 {
    let days_in_month = last_day_of_month(year, month).unwrap_or(0);
    if day > days_in_month {
        return -1;
    }
    match weekday(year, month, day) {
        0 if day == days_in_month => day - 2,
        0 => day + 1,
        6 if day == 1 => day + 2,
        6 => day - 1,
        _ => day,
    }
}

fn is_nth_weekday_of_month(year: i64, month: i64, day: i64, nth: u32) -> bool {
    let target = weekday(year, month, day);
    let count = (1..=day)
        .filter(|d| weekday(year, month, *d) == target)
        .count();
    if nth & ANY_OCCURRENCE != 0
        && count >= 1
        && OCCURRENCE_BITMASKS
            .get(count - 1)
            .is_some_and(|mask| mask & nth != 0)
    {
        return true;
    }
    if nth & LAST_OCCURRENCE != 0 {
        let days_in_month = last_day_of_month(year, month).unwrap_or(0);
        return !((day + 1)..=days_in_month).any(|d| weekday(year, month, d) == target);
    }
    false
}

impl CronDate {
    fn from_instant(instant: i64, zone: Zone) -> Self {
        let point = to_tz(instant, zone);
        Self {
            zone,
            year: point.y,
            month: point.m - 1,
            day: point.d,
            hour: point.h,
            minute: point.i,
            second: point.s,
            ms: instant.rem_euclid(1000),
        }
    }

    fn get(&self, target: Target) -> i64 {
        match target {
            Target::Year => self.year,
            Target::Month => self.month,
            Target::Day => self.day,
            Target::Hour => self.hour,
            Target::Minute => self.minute,
            Target::Second => self.second,
        }
    }

    fn set(&mut self, target: Target, value: i64) {
        match target {
            Target::Year => self.year = value,
            Target::Month => self.month = value,
            Target::Day => self.day = value,
            Target::Hour => self.hour = value,
            Target::Minute => self.minute = value,
            Target::Second => self.second = value,
        }
    }

    /// Carries out-of-range fields, as croner's apply() does, with its
    /// checks as written (`hour > 59`, February always 28 days); true
    /// when it normalized anything.
    fn apply(&mut self) -> bool {
        let month_days = usize::try_from(self.month)
            .ok()
            .and_then(|index| DAYS_OF_MONTH.get(index).copied());
        let out = self.month > 11
            || self.month < 0
            || month_days.is_some_and(|days| self.day > days)
            || self.day < 1
            || self.hour > 59
            || self.minute > 59
            || self.second > 59
            || self.hour < 0
            || self.minute < 0
            || self.second < 0;
        if !out {
            return false;
        }
        let (year, month, day, hour, minute, second, ms) = utc_fields(date_utc(
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.ms,
        ));
        (
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.ms,
        ) = (year, month, day, hour, minute, second, ms);
        true
    }

    fn pattern_array(pattern: &Pattern, target: Target) -> &[u32] {
        match target {
            Target::Year => &pattern.year,
            Target::Month => &pattern.month,
            Target::Day => &pattern.day,
            Target::Hour => &pattern.hour,
            Target::Minute => &pattern.minute,
            Target::Second => &pattern.second,
        }
    }

    /// 1: still matches, 2: moved to a match, 3: no match left in range.
    fn find_next(&mut self, target: Target, pattern: &Pattern, offset: i64) -> u8 {
        let original = self.get(target);
        let last_dom = if pattern.last_day_of_month {
            last_day_of_month(self.year, self.month)
        } else {
            None
        };
        let first_weekday =
            (!pattern.star_dow && target == Target::Day).then(|| weekday(self.year, self.month, 1));
        let values = Self::pattern_array(pattern, target);
        let mut index = original + offset;
        while index < values.len() as i64 {
            let day_value = index - offset;
            let mut matched = usize::try_from(index)
                .ok()
                .and_then(|i| values.get(i).copied())
                .unwrap_or(0);
            if target == Target::Day {
                if matched == 0 {
                    for (with_w, flag) in pattern.nearest_weekdays.iter().enumerate() {
                        if *flag == 0 {
                            continue;
                        }
                        let execution =
                            nearest_weekday(self.year, self.month, with_w as i64 - offset);
                        if execution == -1 {
                            continue;
                        }
                        if execution == day_value {
                            matched = 1;
                            break;
                        }
                    }
                }
                if pattern.last_weekday && day_value == last_weekday(self.year, self.month) {
                    matched = 1;
                }
                if pattern.last_day_of_month && Some(day_value) == last_dom {
                    matched = 1;
                }
                if let Some(first) = first_weekday {
                    let slot = (first + day_value - 1).rem_euclid(7) as usize;
                    let mut dow = pattern.day_of_week[slot];
                    if dow != 0 && dow & ANY_OCCURRENCE != 0 {
                        dow = u32::from(is_nth_weekday_of_month(
                            self.year, self.month, day_value, dow,
                        ));
                    }
                    matched = if pattern.use_and_logic {
                        if matched != 0 { dow } else { matched }
                    } else if !pattern.star_dom {
                        if matched != 0 { matched } else { dow }
                    } else if matched != 0 {
                        dow
                    } else {
                        matched
                    };
                }
            }
            if matched != 0 {
                self.set(target, day_value);
                return if original == day_value { 1 } else { 2 };
            }
            index += 1;
        }
        3
    }

    /// croner's recurse(), with its tail calls as a loop.
    fn walk(&mut self, pattern: &Pattern) -> bool {
        let mut doing = 0_usize;
        loop {
            if doing == 0 && !pattern.star_year {
                if (0..pattern.year.len() as i64).contains(&self.year)
                    && pattern.year[self.year as usize] == 0
                {
                    let Some(found) =
                        (self.year + 1..10_000).find(|year| pattern.year[*year as usize] == 1)
                    else {
                        return false;
                    };
                    self.year = found;
                    (
                        self.month,
                        self.day,
                        self.hour,
                        self.minute,
                        self.second,
                        self.ms,
                    ) = (0, 1, 0, 0, 0, 0);
                }
                if self.year >= 10_000 {
                    return false;
                }
            }
            let (target, carry, offset) = STEPS[doing];
            let result = self.find_next(target, pattern, offset);
            if result > 1 {
                for &(lower, _, lower_offset) in &STEPS[doing + 1..] {
                    self.set(lower, -lower_offset);
                }
                if result == 3 {
                    self.set(carry, self.get(carry) + 1);
                    self.set(target, -offset);
                    self.apply();
                    if doing == 0 && !pattern.star_year {
                        while (0..pattern.year.len() as i64).contains(&self.year)
                            && pattern.year[self.year as usize] == 0
                        {
                            self.year += 1;
                        }
                        if self.year >= 10_000 {
                            return false;
                        }
                    }
                    doing = 0;
                    continue;
                } else if self.apply() {
                    // croner steps back a level after normalizing; from the
                    // month there is no level above, and it never needs one.
                    doing = doing.saturating_sub(1);
                    continue;
                }
            }
            doing += 1;
            if doing >= STEPS.len() {
                return true;
            }
            if if pattern.star_year {
                self.year >= 3000
            } else {
                self.year >= 10_000
            } {
                return false;
            }
        }
    }

    fn instant(&self) -> i64 {
        match self.zone {
            Zone::Local => from_local(date_utc(
                self.year,
                self.month,
                self.day,
                self.hour,
                self.minute,
                self.second,
                self.ms,
            )),
            zone => from_tz(
                TimePoint {
                    y: self.year,
                    m: self.month + 1,
                    d: self.day,
                    h: self.hour,
                    i: self.minute,
                    s: self.second,
                },
                zone,
            ),
        }
    }
}

/// croner's `nextRun(new Date(from))`: the next matching wall clock after
/// the second `from` falls in, as an instant. None when nothing is left.
pub fn next_run(pattern: &Pattern, zone: Zone, from_ms: i64) -> Option<i64> {
    let mut date = CronDate::from_instant(from_ms, zone);
    date.second += 1;
    date.ms = 0;
    date.apply();
    date.walk(pattern).then(|| date.instant())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_arithmetic_matches_javascript() {
        assert_eq!(date_utc(2026, 0, 1, 0, 0, 0, 0), 1_767_225_600_000);
        assert_eq!(
            date_utc(2026, 12, 1, 0, 0, 0, 0),
            date_utc(2027, 0, 1, 0, 0, 0, 0)
        );
        assert_eq!(
            date_utc(2024, 2, 0, 0, 0, 0, 0),
            date_utc(2024, 1, 29, 0, 0, 0, 0)
        );
        assert_eq!(
            utc_fields(date_utc(2026, 3, 31, 25, 61, 0, 0)),
            (2026, 4, 2, 2, 1, 0, 0)
        );
        assert_eq!(weekday(2026, 9, 6), 2);
        assert_eq!(parse_int(" 15W"), Some(15));
        assert_eq!(parse_int("1L"), Some(1));
        assert_eq!(parse_int("L"), None);
        assert_eq!(parse_int(""), None);
    }

    #[test]
    fn replaces_names_case_insensitively_in_order() {
        assert_eq!(replace_alpha_days("MON-FRI"), "1-5");
        assert_eq!(replace_alpha_days("fri-sun"), "5-7");
        assert_eq!(replace_alpha_months("jan-DEC"), "1-12");
    }
}
