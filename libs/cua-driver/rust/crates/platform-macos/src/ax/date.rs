//! The dates a caller writes into a date or time control.
//!
//! A date control publishes its value only as a `CFDate`, and the tree renders
//! it as local ISO-8601 with its UTC offset (`2026-09-25T17:00:00+02:00`). The
//! same text, or any shorter ISO-8601 form of it, is what a write accepts:
//!
//! - `YYYY-MM-DD` — the calendar date in the Mac's current time zone. The
//!   control keeps its current local time of day (midnight when it has none
//!   readable), so moving an event to another day does not also move it to
//!   midnight.
//! - `YYYY-MM-DDTHH:MM`, `…:SS`, `…:SS.fraction` — a wall-clock date-time. With
//!   no offset it is local time: the Mac's current time zone, which is the zone
//!   the control displays and the tree renders.
//! - either date-time form followed by `Z`, `±HH:MM`, `±HHMM` or `±HH` — that
//!   exact instant, whatever the local zone is.
//!
//! A space may stand in for the `T`.

use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

/// CFDate's epoch — 2001-01-01 00:00:00 UTC — as Unix seconds.
const CF_ABSOLUTE_TIME_UNIX_EPOCH: f64 = 978_307_200.0;

/// The forms a refusal names, so a caller can correct its value in one step.
pub const ACCEPTED_FORMS: &str = "YYYY-MM-DD, YYYY-MM-DDTHH:MM or YYYY-MM-DDTHH:MM:SS \
     (local time), or a date-time followed by Z or ±HH:MM";

/// A parsed ISO-8601 request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestedDate {
    date: Date,
    /// `None` for a date-only request.
    time: Option<Time>,
    /// `None` for local wall-clock time.
    offset: Option<UtcOffset>,
}

/// Parse one of the accepted ISO-8601 forms. `None` for anything else,
/// including a well-formed string naming a day or time that does not exist.
pub fn parse_iso8601(text: &str) -> Option<RequestedDate> {
    let text = text.trim();
    let (date_part, rest) = match text.find(['T', 't', ' ']) {
        Some(split) => (&text[..split], Some(&text[split + 1..])),
        None => (text, None),
    };
    let date = parse_date(date_part)?;
    let Some(rest) = rest else {
        return Some(RequestedDate {
            date,
            time: None,
            offset: None,
        });
    };
    let (clock, offset) = split_offset(rest)?;
    Some(RequestedDate {
        date,
        time: Some(parse_clock(clock)?),
        offset,
    })
}

fn digits(text: &str, width: usize) -> Option<u32> {
    (text.len() == width && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn parse_date(text: &str) -> Option<Date> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = digits(&text[..4], 4)? as i32;
    let month = Month::try_from(digits(&text[5..7], 2)? as u8).ok()?;
    Date::from_calendar_date(year, month, digits(&text[8..], 2)? as u8).ok()
}

fn split_offset(text: &str) -> Option<(&str, Option<UtcOffset>)> {
    if let Some(clock) = text.strip_suffix(['Z', 'z']) {
        return Some((clock, Some(UtcOffset::UTC)));
    }
    let Some(sign_at) = text.rfind(['+', '-']) else {
        return Some((text, None));
    };
    let (clock, zone) = text.split_at(sign_at);
    let sign: i8 = if zone.starts_with('-') { -1 } else { 1 };
    let zone = &zone[1..];
    let (hours, minutes) = match zone.len() {
        2 => (digits(zone, 2)?, 0),
        4 => (digits(&zone[..2], 2)?, digits(&zone[2..], 2)?),
        5 if zone.as_bytes()[2] == b':' => (digits(&zone[..2], 2)?, digits(&zone[3..], 2)?),
        _ => return None,
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    let offset = UtcOffset::from_hms(sign * hours as i8, sign * minutes as i8, 0).ok()?;
    Some((clock, Some(offset)))
}

fn parse_clock(text: &str) -> Option<Time> {
    let (hms, fraction) = match text.split_once('.') {
        Some((hms, fraction)) => (hms, Some(fraction)),
        None => (text, None),
    };
    let mut parts = hms.split(':');
    let hour = digits(parts.next()?, 2)? as u8;
    let minute = digits(parts.next()?, 2)? as u8;
    let second = match parts.next() {
        Some(second) => digits(second, 2)? as u8,
        None if fraction.is_none() => 0,
        None => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    let nanos = match fraction {
        None => 0,
        Some(fraction) => {
            if fraction.is_empty()
                || fraction.len() > 9
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            format!("{fraction:0<9}").parse().ok()?
        }
    };
    Time::from_hms_nano(hour, minute, second, nanos).ok()
}

impl RequestedDate {
    /// The instant to write, as a `CFAbsoluteTime`. `current` is the value the
    /// control holds now: a date-only request keeps its local time of day.
    /// `None` when the local calendar cannot place the wall-clock time.
    pub fn absolute_time(&self, current: Option<f64>) -> Option<f64> {
        let time = match self.time {
            Some(time) => time,
            None => current
                .and_then(local_time_of_day)
                .unwrap_or(Time::MIDNIGHT),
        };
        let unix_nanos = match self.offset {
            Some(offset) => PrimitiveDateTime::new(self.date, time)
                .assume_offset(offset)
                .unix_timestamp_nanos(),
            None => {
                i128::from(local_unix_seconds(self.date, time)?) * 1_000_000_000
                    + i128::from(time.nanosecond())
            }
        };
        Some(unix_nanos as f64 / 1e9 - CF_ABSOLUTE_TIME_UNIX_EPOCH)
    }

    /// Whether a control that reads back `readback` holds what was asked.
    ///
    /// A date-only request asked for a calendar day, so a control that also
    /// normalised the time of day (a date-only picker keeps midnight) still
    /// holds it. A date-time request asked for an instant, compared to the
    /// second the tree renders.
    pub fn is_held_by(&self, target: f64, readback: f64) -> bool {
        match self.time {
            None => local_date(readback) == Some(self.date),
            Some(_) => (readback - target).abs() < 1.0,
        }
    }
}

fn local_components(absolute: f64) -> Option<libc::tm> {
    let seconds = absolute.floor();
    if !seconds.is_finite() || seconds.abs() > 9.0e15 {
        return None;
    }
    let unix = (seconds + CF_ABSOLUTE_TIME_UNIX_EPOCH) as libc::time_t;
    let mut components: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&unix, &mut components) }.is_null() {
        return None;
    }
    Some(components)
}

fn local_time_of_day(absolute: f64) -> Option<Time> {
    let components = local_components(absolute)?;
    let nanos = ((absolute - absolute.floor()) * 1e9) as u32;
    Time::from_hms_nano(
        components.tm_hour as u8,
        components.tm_min as u8,
        // A leap second has no wall-clock slot of its own here.
        components.tm_sec.min(59) as u8,
        nanos.min(999_999_999),
    )
    .ok()
}

fn local_date(absolute: f64) -> Option<Date> {
    let components = local_components(absolute)?;
    Date::from_calendar_date(
        components.tm_year + 1900,
        Month::try_from((components.tm_mon + 1) as u8).ok()?,
        components.tm_mday as u8,
    )
    .ok()
}

/// Unix seconds for a local wall-clock date-time, through the C library's own
/// time zone rules (`mktime`, daylight saving decided by the zone).
fn local_unix_seconds(date: Date, time: Time) -> Option<i64> {
    let mut components: libc::tm = unsafe { std::mem::zeroed() };
    components.tm_year = date.year() - 1900;
    components.tm_mon = i32::from(u8::from(date.month())) - 1;
    components.tm_mday = i32::from(date.day());
    components.tm_hour = i32::from(time.hour());
    components.tm_min = i32::from(time.minute());
    components.tm_sec = i32::from(time.second());
    components.tm_isdst = -1;
    let unix = unsafe { libc::mktime(&mut components) };
    (unix != -1).then_some(unix as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute(text: &str) -> f64 {
        parse_iso8601(text)
            .unwrap_or_else(|| panic!("{text} did not parse"))
            .absolute_time(None)
            .unwrap()
    }

    /// What the tree prints for a control is a value the control takes back:
    /// the instant it names, not the wall clock read in this Mac's zone.
    #[test]
    fn the_form_the_tree_prints_names_its_exact_instant() {
        // 2026-09-25 15:00:00 UTC.
        let expected = 1_790_348_400.0 - CF_ABSOLUTE_TIME_UNIX_EPOCH;
        for text in [
            "2026-09-25T17:00:00+02:00",
            "2026-09-25T15:00:00Z",
            "2026-09-25 10:00-0500",
            "2026-09-25T20:00+05",
        ] {
            assert_eq!(absolute(text), expected, "{text}");
        }
    }

    /// No offset is local time: the wall clock the control shows.
    #[test]
    fn a_date_time_without_offset_is_the_local_wall_clock() {
        let components = local_components(absolute("2026-01-15T10:30")).unwrap();
        assert_eq!(
            (
                components.tm_year + 1900,
                components.tm_mon + 1,
                components.tm_mday,
                components.tm_hour,
                components.tm_min,
                components.tm_sec
            ),
            (2026, 1, 15, 10, 30, 0)
        );
    }

    /// Moving an event to another day must not also move it to midnight.
    #[test]
    fn a_date_only_request_keeps_the_controls_time_of_day() {
        let current = absolute("2026-01-10T08:45:30");
        let written = parse_iso8601("2026-03-02")
            .unwrap()
            .absolute_time(Some(current))
            .unwrap();
        let components = local_components(written).unwrap();
        assert_eq!(
            (
                components.tm_year + 1900,
                components.tm_mon + 1,
                components.tm_mday,
                components.tm_hour,
                components.tm_min,
                components.tm_sec
            ),
            (2026, 3, 2, 8, 45, 30)
        );
        let unreadable = parse_iso8601("2026-03-02").unwrap().absolute_time(None);
        assert_eq!(unreadable, Some(absolute("2026-03-02T00:00")));
    }

    #[test]
    fn a_fraction_is_kept_to_the_nanosecond_it_names() {
        let whole = absolute("2026-09-25T15:00:00Z");
        assert!((absolute("2026-09-25T15:00:00.25Z") - whole - 0.25).abs() < 1e-6);
    }

    /// Anything else is refused rather than guessed: a locale-order date, a
    /// day or hour that does not exist, a zone on a bare date, a truncated
    /// clock, trailing text.
    #[test]
    fn a_value_outside_the_accepted_forms_does_not_parse() {
        for text in [
            "12/21/26",
            "2026-02-30",
            "2026-12-21T24:00",
            "2026-12-21Z",
            "2026-12-21T10",
            "2026-12-21T10:30.5",
            "2026-12-21T10:30+2:00",
            "2026-12-21T10:30+24:00",
            "2026-12-21T10:30:00 PM",
            "",
        ] {
            assert_eq!(parse_iso8601(text), None, "{text}");
        }
    }

    #[test]
    fn a_date_only_request_is_held_by_any_time_on_that_local_day() {
        let request = parse_iso8601("2026-03-02").unwrap();
        let target = request.absolute_time(None).unwrap();
        assert!(request.is_held_by(target, absolute("2026-03-02T23:59:59")));
        assert!(!request.is_held_by(target, absolute("2026-03-03T00:00:00")));
    }

    #[test]
    fn a_date_time_request_is_held_only_to_the_second() {
        let request = parse_iso8601("2026-03-02T09:30").unwrap();
        let target = request.absolute_time(None).unwrap();
        assert!(request.is_held_by(target, target + 0.5));
        assert!(!request.is_held_by(target, target + 60.0));
    }
}
