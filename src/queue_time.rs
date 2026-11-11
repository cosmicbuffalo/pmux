//! Explicit local-time parsing without shell evaluation or platform-specific date commands.
use anyhow::{Result, bail};

fn local(epoch: i64) -> Result<libc::tm> {
    let time: libc::time_t = epoch as libc::time_t;
    // SAFETY: both pointers refer to valid, suitably aligned storage.
    let mut value = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&time, &mut value) }.is_null() {
        bail!("time is out of range");
    }
    Ok(value)
}
fn parts(t: &libc::tm) -> (i32, i32, i32, i32, i32) {
    (t.tm_year, t.tm_mon, t.tm_mday, t.tm_hour, t.tm_min)
}
fn timestamp(value: libc::tm) -> Result<i64> {
    let expected = parts(&value);
    let mut candidates = vec![];
    for dst in [0, 1] {
        let mut candidate = value;
        candidate.tm_isdst = dst;
        // SAFETY: mktime receives a valid initialized tm.
        let epoch = unsafe { libc::mktime(&mut candidate) };
        if epoch != -1 && parts(&candidate) == expected && candidate.tm_isdst == dst {
            candidates.push(epoch as i64);
        }
    }
    candidates.sort();
    candidates.dedup();
    match candidates.as_slice() {
        [epoch] => Ok(*epoch),
        [] => bail!("invalid date or local time skipped by daylight saving; use 'in 2h' instead"),
        _ => {
            bail!("local time is ambiguous during daylight saving; use a relative duration instead")
        }
    }
}
/// Picker fields are `[year, month, day, hour, minute]` in host-local time.
pub const YEAR: usize = 0;
pub const MONTH: usize = 1;
pub const DAY: usize = 2;
pub const HOUR: usize = 3;
pub const MINUTE: usize = 4;
pub const SLOTS: [&str; 5] = ["year", "month", "day", "hour", "minute"];

pub fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}
fn local_fields(epoch: i64) -> Result<[i32; 5]> {
    let t = local(epoch)?;
    Ok([
        t.tm_year + 1900,
        t.tm_mon + 1,
        t.tm_mday,
        t.tm_hour,
        t.tm_min,
    ])
}
/// The picker's opening value: now, rounded up to the next whole minute so that
/// confirming straight away is not already in the past.
pub fn picker_start(now: i64) -> Result<[i32; 5]> {
    local_fields(now - now.rem_euclid(60) + 60)
}
/// Fields for an already-armed batch, so re-picking starts from its current time.
pub fn picker_fields(epoch: i64) -> Result<[i32; 5]> {
    local_fields(epoch)
}
/// Move one field by `delta`, carrying into the coarser fields. Pure calendar
/// arithmetic: no timezone is consulted until the fields are resolved, so
/// arrowing through a DST boundary never silently rewrites what is displayed.
pub fn shift_fields(mut fields: [i32; 5], slot: usize, delta: i32) -> [i32; 5] {
    match slot {
        YEAR => fields[YEAR] = (fields[YEAR] + delta).clamp(1970, 9999),
        MONTH => {
            let months = fields[MONTH] - 1 + delta;
            fields[MONTH] = months.rem_euclid(12) + 1;
            fields[YEAR] = (fields[YEAR] + months.div_euclid(12)).clamp(1970, 9999);
        }
        DAY => {
            let mut day = fields[DAY] + delta;
            while day < 1 {
                fields = shift_fields(fields, MONTH, -1);
                day += days_in_month(fields[YEAR], fields[MONTH]);
            }
            loop {
                let month_days = days_in_month(fields[YEAR], fields[MONTH]);
                if day <= month_days {
                    break;
                }
                day -= month_days;
                fields = shift_fields(fields, MONTH, 1);
            }
            fields[DAY] = day;
        }
        HOUR => {
            let hour = fields[HOUR] + delta;
            fields = shift_fields(fields, DAY, hour.div_euclid(24));
            fields[HOUR] = hour.rem_euclid(24);
        }
        MINUTE => {
            let minute = fields[MINUTE] + delta;
            fields = shift_fields(fields, HOUR, minute.div_euclid(60));
            fields[MINUTE] = minute.rem_euclid(60);
        }
        _ => {}
    }
    // A month or year change can strand the day, as with Jan 31 -> Feb.
    fields[DAY] = fields[DAY].clamp(1, days_in_month(fields[YEAR], fields[MONTH]));
    fields
}
/// Resolve picker fields to an epoch, rejecting DST gaps and folds exactly as
/// the textual parser does.
pub fn fields_epoch(fields: [i32; 5]) -> Result<i64> {
    let [year, month, day, hour, minute] = fields;
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
    {
        bail!("invalid date or time");
    }
    // SAFETY: a zeroed tm is a valid initialized value for mktime.
    let mut value = unsafe { std::mem::zeroed::<libc::tm>() };
    value.tm_year = year - 1900;
    value.tm_mon = month - 1;
    value.tm_mday = day;
    value.tm_hour = hour;
    value.tm_min = minute;
    value.tm_sec = 0;
    timestamp(value)
}
/// Weekday plus wall clock and zone, for the picker's live preview.
pub fn format_weekday(epoch: i64) -> String {
    let Ok(value) = local(epoch) else {
        return epoch.to_string();
    };
    let mut buf = [0u8; 96];
    // SAFETY: buffer and tm are valid; format is a nul-terminated literal.
    let len = unsafe {
        libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            c"%a %Y-%m-%d %H:%M %Z (%z)".as_ptr(),
            &value,
        )
    };
    String::from_utf8_lossy(&buf[..len]).into_owned()
}
pub fn format_time(epoch: i64) -> String {
    let Ok(value) = local(epoch) else {
        return epoch.to_string();
    };
    let mut buf = [0u8; 96];
    // SAFETY: buffer and tm are valid; format is a nul-terminated literal.
    let len = unsafe {
        libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            c"%Y-%m-%d %H:%M:%S %Z (%z)".as_ptr(),
            &value,
        )
    };
    String::from_utf8_lossy(&buf[..len]).into_owned()
}
/// Accept `in 45m`, `in 2h 15m`, `7pm`, `19:30`, `tomorrow 9am`,
/// `today 19:00`, and `YYYY-MM-DD HH:MM` in the host's local timezone.
pub fn parse_time(input: &str, now: i64) -> Result<i64> {
    let text = input.trim().to_ascii_lowercase();
    if let Some(duration) = text.strip_prefix("in ") {
        let compact: String = duration.chars().filter(|c| !c.is_whitespace()).collect();
        let mut number = String::new();
        let mut seconds = 0i64;
        for c in compact.chars() {
            if c.is_ascii_digit() {
                number.push(c);
                continue;
            }
            let multiplier = match c {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                'd' => 86400,
                _ => bail!("use duration units s, m, h, or d"),
            };
            let count = number
                .parse::<i64>()
                .map_err(|_| anyhow::anyhow!("invalid duration"))?;
            seconds = seconds
                .checked_add(
                    count
                        .checked_mul(multiplier)
                        .ok_or_else(|| anyhow::anyhow!("duration too large"))?,
                )
                .ok_or_else(|| anyhow::anyhow!("duration too large"))?;
            number.clear();
        }
        if !number.is_empty() || seconds == 0 {
            bail!("use a positive duration, such as 'in 45m' or 'in 2h 15m'");
        }
        return now
            .checked_add(seconds)
            .ok_or_else(|| anyhow::anyhow!("duration too large"));
    }
    let mut value = local(now)?;
    value.tm_sec = 0;
    let (clock, explicit) = if let Some(clock) = text.strip_prefix("tomorrow ") {
        value.tm_mday += 1;
        value.tm_hour = 12;
        value.tm_isdst = -1;
        // Normalize only the calendar increment, before validating user input.
        unsafe {
            libc::mktime(&mut value);
        }
        (clock, true)
    } else if let Some(clock) = text.strip_prefix("today ") {
        (clock, true)
    } else if let Some((date, clock)) = text.split_once(' ') {
        let fields: Vec<_> = date.split('-').collect();
        if fields.len() != 3 {
            bail!("use '7pm', 'tomorrow 9am', 'in 45m', or 'YYYY-MM-DD HH:MM'");
        }
        let year = fields[0].parse::<i32>()?;
        let month = fields[1].parse::<i32>()?;
        let day = fields[2].parse::<i32>()?;
        if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day)
        {
            bail!("invalid date");
        }
        value.tm_year = year - 1900;
        value.tm_mon = month - 1;
        value.tm_mday = day;
        (clock, true)
    } else {
        (text.as_str(), false)
    };
    let clock = clock.trim();
    let (digits, suffix) = if let Some(x) = clock.strip_suffix("am") {
        (x, Some(false))
    } else if let Some(x) = clock.strip_suffix("pm") {
        (x, Some(true))
    } else {
        (clock, None)
    };
    let fields: Vec<_> = digits.trim().split(':').collect();
    if fields.is_empty() || fields.len() > 2 {
        bail!("invalid clock time");
    }
    let mut hour = fields[0].parse::<i32>()?;
    let minute = if fields.len() == 2 {
        fields[1].parse::<i32>()?
    } else {
        0
    };
    if !(0..=59).contains(&minute) {
        bail!("minute must be 0–59");
    }
    if let Some(pm) = suffix {
        if !(1..=12).contains(&hour) {
            bail!("hour must be 1–12 with am/pm");
        }
        hour = hour % 12 + if pm { 12 } else { 0 };
    } else if !(0..=23).contains(&hour) {
        bail!("hour must be 0–23");
    }
    value.tm_hour = hour;
    value.tm_min = minute;
    let mut at = timestamp(value)?;
    if at <= now && !explicit {
        value.tm_mday += 1;
        value.tm_hour = 12;
        value.tm_isdst = -1;
        unsafe {
            libc::mktime(&mut value);
        }
        value.tm_hour = hour;
        value.tm_min = minute;
        at = timestamp(value)?;
    }
    if at <= now {
        bail!("scheduled time is in the past");
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_and_invalid_times() {
        assert_eq!(parse_time("in 2h 15m", 100).unwrap(), 8200);
        for input in [
            "in 0m",
            "in -2h",
            "in 30",
            "in 999999999999999999h",
            "25:00",
            "12:99",
            "0pm",
            "2027-02-30 12:00",
            "2027-13-01 12:00",
        ] {
            assert!(parse_time(input, 100).is_err(), "{input}");
        }
    }
    #[test]
    fn picker_fields_carry_and_clamp() {
        // Month change strands the day: Jan 31 -> Feb 28 in a common year.
        assert_eq!(
            shift_fields([2027, 1, 31, 9, 0], MONTH, 1),
            [2027, 2, 28, 9, 0]
        );
        // ...and Feb 29 in a leap year.
        assert_eq!(
            shift_fields([2028, 1, 31, 9, 0], MONTH, 1),
            [2028, 2, 29, 9, 0]
        );
        // Leap day survives only while the year stays leap.
        assert_eq!(
            shift_fields([2028, 2, 29, 9, 0], YEAR, 1),
            [2029, 2, 28, 9, 0]
        );
        // Minute carries through hour and day into the next year.
        assert_eq!(
            shift_fields([2027, 12, 31, 23, 59], MINUTE, 1),
            [2028, 1, 1, 0, 0]
        );
        // ...and back again.
        assert_eq!(
            shift_fields([2028, 1, 1, 0, 0], MINUTE, -1),
            [2027, 12, 31, 23, 59]
        );
        // Month wraps into the year in both directions.
        assert_eq!(
            shift_fields([2027, 12, 1, 9, 0], MONTH, 1),
            [2028, 1, 1, 9, 0]
        );
        assert_eq!(
            shift_fields([2027, 1, 1, 9, 0], MONTH, -1),
            [2026, 12, 1, 9, 0]
        );
        // Day wraps across a month boundary.
        assert_eq!(
            shift_fields([2027, 3, 1, 9, 0], DAY, -1),
            [2027, 2, 28, 9, 0]
        );
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
    }
    #[test]
    fn picker_fields_round_trip_through_the_local_zone() {
        let now = crate::util::now_epoch();
        let start = picker_start(now).unwrap();
        let at = fields_epoch(start).unwrap();
        // Opening value is the next whole minute, so confirming at once is not past.
        assert!(at > now && at - now <= 60);
        assert_eq!(picker_fields(at).unwrap(), start);
        assert!(fields_epoch([2027, 2, 30, 9, 0]).is_err());
        assert!(fields_epoch([2027, 13, 1, 9, 0]).is_err());
        assert!(fields_epoch([2027, 1, 1, 24, 0]).is_err());
    }
    #[test]
    fn local_clock_rolls_forward_and_explicit_past_fails() {
        let now = crate::util::now_epoch();
        let at = parse_time("7pm", now).unwrap();
        assert!(at > now && at - now <= 90000);
        assert_eq!(local(at).unwrap().tm_hour, 19);
        assert!(parse_time("2000-01-01 09:00", now).is_err());
        assert!(parse_time("tomorrow 9am", now).unwrap() > now);
    }
}
