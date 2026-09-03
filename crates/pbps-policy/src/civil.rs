//! Enough of a calendar for a change window and an expiry date, and no more.
//!
//! No date library is pulled in: this needs a civil date from a Unix time, a
//! weekday, and a fixed UTC offset. That is forty lines, and a dependency the
//! audit has to read is worth more than forty lines saved (the same trade the
//! CLI's own `civil_from_days` makes). Time zones are **offsets** here, never
//! names: a zone database is exactly the dependency being declined, and a
//! change window written as `+08:00` says what it means without one.

/// A civil date and time, already shifted into the caller's offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    /// Monday is 0, Sunday is 6.
    pub weekday: u32,
}

impl Civil {
    /// `YYYY-MM-DD`, the spelling an expiry date is written in.
    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Minutes since midnight, for comparing against a window.
    pub fn minute_of_day(&self) -> u32 {
        self.hour * 60 + self.minute
    }
}

/// The civil time `offset_minutes` east of UTC at `unix_seconds`.
pub fn at(unix_seconds: i64, offset_minutes: i32) -> Civil {
    let local = unix_seconds + i64::from(offset_minutes) * 60;
    let days = local.div_euclid(86_400);
    let rem = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday: Monday-based, that is 3.
    let weekday = ((days + 3).rem_euclid(7)) as u32;
    Civil {
        year,
        month,
        day,
        hour: (rem / 3600) as u32,
        minute: ((rem % 3600) / 60) as u32,
        weekday,
    }
}

/// Howard Hinnant's days-to-civil, proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `+08:00`, `-05:30`, `Z` — the offsets a window is written in.
pub fn parse_offset(s: &str) -> Result<i32, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("z") || s == "+00:00" {
        return Ok(0);
    }
    let bad = || format!("`{s}` is not a UTC offset like `+08:00` or `-05:30`");
    let (sign, rest) = match s.chars().next() {
        Some('+') => (1, &s[1..]),
        Some('-') => (-1, &s[1..]),
        _ => return Err(bad()),
    };
    let (h, m) = rest.split_once(':').ok_or_else(bad)?;
    let h: i32 = h.parse().map_err(|_| bad())?;
    let m: i32 = m.parse().map_err(|_| bad())?;
    if h > 14 || m > 59 {
        return Err(bad());
    }
    Ok(sign * (h * 60 + m))
}

/// `HH:MM` as minutes since midnight; `24:00` is allowed as an end.
pub fn parse_hhmm(s: &str) -> Result<u32, String> {
    let bad = || format!("`{s}` is not a time like `09:00`");
    let (h, m) = s.trim().split_once(':').ok_or_else(bad)?;
    let h: u32 = h.parse().map_err(|_| bad())?;
    let m: u32 = m.parse().map_err(|_| bad())?;
    if h > 24 || m > 59 || (h == 24 && m != 0) {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

/// `mon`, `tue`, ... as Monday-based indexes.
pub fn parse_weekday(s: &str) -> Result<u32, String> {
    const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let key = s.trim().to_ascii_lowercase();
    DAYS.iter()
        .position(|d| key.starts_with(d))
        .map(|p| p as u32)
        .ok_or_else(|| format!("`{s}` is not a weekday like `mon`"))
}

/// `YYYY-MM-DD`, checked for shape and range only.
///
/// The shape is exact — four, two and two digits — because an expiry is
/// compared to today's date **as text** (`Civil::date`), and `2026-9-4`
/// sorts after `2026-10-01`, which would keep a suppression alive for a
/// month past its date without anyone being told.
pub fn parse_date(s: &str) -> Result<(i64, u32, u32), String> {
    let bad = || format!("`{s}` is not a date like `2026-12-31`");
    let s = s.trim();
    let parts: Vec<&str> = s.split('-').collect();
    let [y, m, d] = parts.as_slice() else {
        return Err(bad());
    };
    let digits = |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    if !digits(y, 4) || !digits(m, 2) || !digits(d, 2) {
        return Err(bad());
    }
    let y: i64 = y.parse().map_err(|_| bad())?;
    let m: u32 = m.parse().map_err(|_| bad())?;
    let d: u32 = d.parse().map_err(|_| bad())?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(bad());
    }
    Ok((y, m, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_a_thursday_and_offsets_move_the_day() {
        let c = at(0, 0);
        assert_eq!(
            (c.year, c.month, c.day, c.hour, c.minute),
            (1970, 1, 1, 0, 0)
        );
        assert_eq!(c.weekday, 3, "Thursday");
        // 23:30 in Taipei on 1969-12-31 is 15:30 UTC that day; 1970-01-01
        // 00:30 UTC is 08:30 Taipei, the same day.
        let c = at(-30 * 60, 8 * 60);
        assert_eq!((c.year, c.month, c.day), (1970, 1, 1));
        assert_eq!(c.hour, 7);
        let c = at(30 * 60, -8 * 60);
        assert_eq!((c.year, c.month, c.day), (1969, 12, 31));
        assert_eq!(c.weekday, 2, "Wednesday");
    }

    #[test]
    fn known_dates_come_out_right() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_513), (2026, 3, 1));
        assert_eq!(civil_from_days(20_512), (2026, 2, 28));
        assert_eq!(at(20_513 * 86_400, 0).date(), "2026-03-01");
    }

    #[test]
    fn offsets_times_days_and_dates_parse_or_are_refused_by_name() {
        assert_eq!(parse_offset("+08:00").unwrap(), 480);
        assert_eq!(parse_offset("-05:30").unwrap(), -330);
        assert_eq!(parse_offset("Z").unwrap(), 0);
        assert!(parse_offset("8").is_err());
        assert!(parse_offset("+15:00").is_err());
        assert_eq!(parse_hhmm("09:30").unwrap(), 570);
        assert_eq!(parse_hhmm("24:00").unwrap(), 1440);
        assert!(parse_hhmm("24:01").is_err());
        assert!(parse_hhmm("9").is_err());
        assert_eq!(parse_weekday("Mon").unwrap(), 0);
        assert_eq!(parse_weekday("sunday").unwrap(), 6);
        assert!(parse_weekday("someday").is_err());
        assert_eq!(parse_date("2026-12-31").unwrap(), (2026, 12, 31));
        assert!(parse_date("31/12/2026").is_err());
        assert!(parse_date("2026-13-01").is_err());
        // Unpadded is refused: as text, `2026-9-4` outlives `2026-10-01`.
        assert!(parse_date("2026-9-4").is_err());
        assert!(parse_date("2026-09-4").is_err());
        assert!(parse_date("+2026-09-04").is_err());
    }
}
