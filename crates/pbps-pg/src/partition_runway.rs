//! How far a RANGE series reaches past the rows it holds (#1789, DEC-1789.2).
//!
//! `doctor` warns when a partitioned table's partitions are about to run out.
//! This is the arithmetic of that question, with no connection in it: the
//! database's own answers (the partitions it has, and which of them holds a
//! row) come in as values, in `doctor/partitions.rs`, and what leaves is a
//! verdict.
//!
//! # Measured by occupancy, with no clock
//!
//! The runway is how far the series reaches past **the newest range
//! partition that holds a row**: the frontier, the highest finite upper end,
//! against the lower end of that partition. Not against today's date.
//!
//! - The same database gives the same answer whenever it is asked, which is
//!   what a check run from CI needs, and what the generator's own rule (no
//!   clock, DEC-1789.1) keeps.
//! - It works for an integer key, which has no date to compare to.
//! - It measures the thing that matters. A table loaded from an archive
//!   whose rows end last year has plenty of runway however long ago the
//!   series stopped, and one whose rows reach this month has none left when
//!   the series ends this month, whatever the date is.
//! - The partition the newest rows are in counts. A series that ends where
//!   that partition ends has no room left for the next period, and the
//!   horizon says how much room is wanted.
//!
//! A table with no row in a range partition has nothing to be ahead of, and
//! is not reported.
//!
//! # The horizon
//!
//! `partition_runway:` in `pbps.yml`, read here so that its grammar is the
//! generator's own ([`parse_step_as`]): a count of partitions, or a width
//! (`2month`). Absent, it is twice the newest partition's width. The width is
//! counted on the calendar from the occupied partition's lower end, as the
//! generator counts its steps, so a month is a month and not thirty days. A
//! count of partitions needs no calendar and serves every key type: a number
//! alone cannot say whether it counts key values or partitions, so a bare
//! number is always the second.
//!
//! # What a partition with no lower end means
//!
//! `MINVALUE` below the lowest partition is a lower end nobody can measure
//! from. If the newest rows are in that partition, the runway is measured
//! from its upper end, which is where its rows could be closest to the
//! frontier. That asks for more than a partition with a lower end does, and
//! it is the right side to err on for a series that has only just begun.
//!
//! A partition with no upper end (`MAXVALUE`) leaves nothing to run out of.

use pbps_db::doctor::Reach;
use pbps_model::BoundDatum;

use crate::partition_series::{
    Civil, Int, Key, Mode, Step, civil_from_days, days_from_civil, days_in_month, parse_civil,
    parse_integer, parse_step_as, why,
};

/// A bound's value, ordered. A key is of one kind, so two values compared
/// are of one variant.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Val {
    Time(Civil),
    Int(i128),
}

/// A bound as the engine printed it, and what it is worth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct End {
    pub(crate) text: String,
    pub(crate) val: Val,
}

/// `text` as `key` prints a bound, and no other way: the generator's reading.
pub(crate) fn end(key: Key, text: &str) -> Result<End, String> {
    let val = match key {
        Key::Integer(int) => Val::Int(parse_integer(int, text)?),
        Key::Date | Key::Timestamp | Key::TimestampTz => {
            Val::Time(parse_civil(key, text).map_err(|reject| why(key, &reject))?)
        }
    };
    Ok(End {
        text: text.to_owned(),
        val,
    })
}

/// One range partition: `lo` inclusive, `hi` exclusive, and `None` for
/// `MINVALUE` and `MAXVALUE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Range {
    pub(crate) lo: Option<End>,
    pub(crate) hi: Option<End>,
}

/// The range a one-column bound declares, or why it is not one.
pub(crate) fn range(key: Key, from: &BoundDatum, to: &BoundDatum) -> Result<Range, String> {
    let lo = match from {
        BoundDatum::MinValue => None,
        BoundDatum::MaxValue => return Err("it starts at MAXVALUE".to_owned()),
        BoundDatum::Value(text) => {
            Some(end(key, text).map_err(|why| format!("it starts at `{text}`, {why}"))?)
        }
    };
    let hi = match to {
        BoundDatum::MaxValue => None,
        BoundDatum::MinValue => return Err("it ends at MINVALUE".to_owned()),
        BoundDatum::Value(text) => {
            Some(end(key, text).map_err(|why| format!("it ends at `{text}`, {why}"))?)
        }
    };
    Ok(Range { lo, hi })
}

/// Whether some range has no upper end, so that the series cannot run out.
pub(crate) fn is_open_ended(ranges: &[Range]) -> bool {
    ranges.iter().any(|r| r.hi.is_none())
}

/// The ranges' indexes, the one ending last first. Only for ranges that all
/// have an upper end ([`is_open_ended`] is the question before this one).
pub(crate) fn newest_first(ranges: &[Range]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..ranges.len()).collect();
    order.sort_by(|a, b| {
        let hi = |i: &usize| ranges[*i].hi.as_ref().map(|e| &e.val);
        hi(b).cmp(&hi(a))
    });
    order
}

// ---------------------------------------------------------------------------
// The horizon
// ---------------------------------------------------------------------------

/// How far ahead of its rows a series must reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Horizon {
    /// Twice the newest partition's width.
    Default,
    /// This many partitions, the one holding the newest rows among them.
    Partitions(u32),
    /// This much of a date or time key's range.
    Width { step: Step, text: String },
}

impl Horizon {
    /// `partition_runway:` as written, or its absence.
    pub(crate) fn parse(text: Option<&str>) -> Result<Self, String> {
        let Some(text) = text else {
            return Ok(Self::Default);
        };
        let text = text.trim();
        if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
            return match text.parse::<u32>() {
                Ok(n) if n > 0 => Ok(Self::Partitions(n)),
                _ => Err(format!(
                    "partition_runway `{text}` is not a positive count of partitions, as `3`"
                )),
            };
        }
        let step = parse_step_as("partition_runway", text)
            .map_err(|message| format!("{message}; or write a count of partitions, as `3`"))?;
        Ok(Self::Width {
            step,
            text: text.to_owned(),
        })
    }
}

/// A distance on the key's axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Width {
    Months(i64),
    Seconds(i64),
}

fn width_of(step: &Step) -> Width {
    match step.mode {
        Mode::Days => Width::Seconds(step.len * 86_400),
        Mode::Months => Width::Months(step.len),
    }
}

/// The years a bound can be written in: years 1 to 9999, which is what
/// [`parse_civil`] reads and what `pbps partitions extend` steps through
/// (DEC-1789.1). The engine prints a year outside them with five digits, a
/// spelling no reader here takes, so a target past 9999 names a value no
/// series can hold.
const YEARS: std::ops::RangeInclusive<i64> = 1..=9999;

/// `c` moved forward by `times` widths, or `None` when that leaves
/// [`YEARS`]. The year is checked before the value is built, so that no
/// `Civil` past the calendar ever exists to be compared or printed (#1924).
///
/// A month is a calendar month, and a day that the target month lacks is its
/// last day, which is how the engine adds an interval.
fn add(c: &Civil, width: Width, times: i64) -> Option<Civil> {
    match width {
        Width::Months(n) => {
            let total = c.y * 12 + (c.m - 1) + n * times;
            let (y, m) = (total.div_euclid(12), total.rem_euclid(12) + 1);
            YEARS.contains(&y).then(|| Civil {
                y,
                m,
                d: c.d.min(days_in_month(y, m)),
                ..*c
            })
        }
        Width::Seconds(s) => {
            let total = days_from_civil(c.y, c.m, c.d) * 86_400 + c.secs + s * times;
            let (y, m, d) = civil_from_days(total.div_euclid(86_400));
            YEARS.contains(&y).then(|| Civil {
                y,
                m,
                d,
                secs: total.rem_euclid(86_400),
                micros: c.micros,
            })
        }
    }
}

/// The distance from `a` to `b`: whole months when both are the first of a
/// month at midnight, which is how a monthly or yearly series is laid out,
/// and seconds otherwise. `None` when it has no whole answer: a fraction of a
/// second, or `b` not after `a`.
fn width_between(a: &Civil, b: &Civil) -> Option<Width> {
    if b <= a {
        return None;
    }
    let aligned = |c: &Civil| c.d == 1 && c.secs == 0 && c.micros == 0;
    if aligned(a) && aligned(b) {
        return Some(Width::Months((b.y * 12 + b.m) - (a.y * 12 + a.m)));
    }
    if a.micros != 0 || b.micros != 0 {
        return None;
    }
    let seconds = |c: &Civil| days_from_civil(c.y, c.m, c.d) * 86_400 + c.secs;
    Some(Width::Seconds(seconds(b) - seconds(a)))
}

/// A width as `--every` would write it, so that the remedy can be pasted.
fn every_of(width: Width) -> Option<String> {
    match width {
        Width::Months(n) if n % 12 == 0 => Some(format!("{}year", n / 12)),
        Width::Months(n) => Some(format!("{n}month")),
        Width::Seconds(s) if s % 86_400 == 0 => {
            let days = s / 86_400;
            Some(if days % 7 == 0 {
                format!("{}week", days / 7)
            } else {
                format!("{days}day")
            })
        }
        Width::Seconds(_) => None,
    }
}

/// What the horizon asks of the series, worked out against one occupied
/// partition.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Need {
    /// This many partitions end after the reference.
    Partitions(u32),
    /// Some partition ends at or after this value.
    Until(Val),
}

/// `c` as `key` prints a bound, which is the spelling [`parse_civil`] reads
/// back: `parse_civil(key, &fmt_civil(key, c)) == c` for every value a
/// `timestamp` or `timestamptz` bound holds.
///
/// The fraction of a second is the engine's own: up to six digits with the
/// trailing zeros trimmed, and no point at all when there is none. A reach
/// that dropped it would name a point earlier than the one the horizon asks
/// for, and a remedy that reached only that far would still be short (#1923).
fn fmt_civil(key: Key, c: &Civil) -> String {
    let date = format!("{:04}-{:02}-{:02}", c.y, c.m, c.d);
    let fraction = if c.micros == 0 {
        String::new()
    } else {
        format!(".{:06}", c.micros).trim_end_matches('0').to_owned()
    };
    let clock = format!(
        "{date} {:02}:{:02}:{:02}{fraction}",
        c.secs / 3600,
        c.secs % 3600 / 60,
        c.secs % 60
    );
    match key {
        Key::Date | Key::Integer(_) => date,
        Key::Timestamp => clock,
        Key::TimestampTz => format!("{clock}+00"),
    }
}

fn plural(n: u32) -> String {
    format!("{n} partition{}", if n == 1 { "" } else { "s" })
}

/// Why a horizon cannot be asked of a series whose newest rows start at
/// `held_from`: its target is a value past the calendar, so there is no point
/// to say the series falls short of (#1924). The reason reads inside the
/// `…-unknown` finding's parentheses, like the others.
fn past_the_calendar(horizon: &str, held_from: &str, remedy: &str) -> String {
    format!(
        "{horizon}, counted from `{held_from}`, reaches past the supported calendar, which ends \
         in year 9999: {remedy}"
    )
}

/// Why a horizon cannot be asked of a series whose newest rows start at
/// `held_from`: its target is an integer above the largest the key's type
/// holds, so no partition can end there and there is no point to say the
/// series falls short of (#2020). The same shape as [`past_the_calendar`], and
/// the wording of the generator's own refusal (#1905: "no larger value fits a
/// bigint"). The reason reads inside the `…-unknown` finding's parentheses.
fn past_the_largest(horizon: &str, held_from: &str, int: Int, remedy: &str) -> String {
    format!(
        "{horizon}, counted from `{held_from}`, reaches past the largest {} value, {}, and no \
         larger value fits: {remedy}",
        int.name, int.max
    )
}

/// The need, and the horizon in words for the message.
///
/// A count of partitions is a number of partitions and never a point on the
/// key's axis, so it has no calendar or type to leave and serves every key
/// type, however large (it is at most `u32::MAX`). A target on the axis of a
/// date or time key can leave the calendar, and one on an integer key can
/// leave the key's type; either is an `Err`: undetermined, not a series that
/// is short of a point nothing can reach.
fn need(
    key: Key,
    horizon: &Horizon,
    reference: &End,
    newest: &Range,
) -> Result<(Need, String), String> {
    match horizon {
        Horizon::Partitions(k) => Ok((Need::Partitions(*k), plural(*k))),
        Horizon::Width { step, text } => match &reference.val {
            Val::Time(c) => {
                let until = add(c, width_of(step), 1).ok_or_else(|| {
                    past_the_calendar(
                        &format!("partition_runway `{text}`"),
                        &reference.text,
                        "write a shorter width, or a count of partitions, as `3`",
                    )
                })?;
                Ok((Need::Until(Val::Time(until)), text.clone()))
            }
            Val::Int(_) => Err(format!(
                "partition_runway `{text}` is a width of time, and this key is a {}: write a \
                 count of partitions, as `3`",
                key.name()
            )),
        },
        Horizon::Default => {
            let words = "twice the newest partition's width".to_owned();
            let twice = match (newest.lo.as_ref(), newest.hi.as_ref(), &reference.val) {
                (Some(lo), Some(hi), Val::Time(c)) => match (&lo.val, &hi.val) {
                    (Val::Time(a), Val::Time(b)) => match width_between(a, b) {
                        Some(w) => Some(Val::Time(add(c, w, 2).ok_or_else(|| {
                            past_the_calendar(
                                &format!("the default horizon, {words}"),
                                &reference.text,
                                "set partition_runway to a count of partitions, as `3`",
                            )
                        })?)),
                        None => None,
                    },
                    _ => None,
                },
                (Some(lo), Some(hi), Val::Int(r)) => match (key, &lo.val, &hi.val) {
                    (Key::Integer(int), Val::Int(a), Val::Int(b)) => {
                        // The engine refuses a partition whose lower end is
                        // not below its upper end, so the width is positive
                        // and the target is above `r`, itself no lower than
                        // `int.min` ([`parse_integer`]): it cannot leave the
                        // type downwards. Upwards it can, and the largest
                        // value is a target like any other, since a series
                        // can end there.
                        let target = r + 2 * (b - a);
                        if target > int.max {
                            return Err(past_the_largest(
                                &format!("the default horizon, {words}"),
                                &reference.text,
                                int,
                                "set partition_runway to a count of partitions, as `3`",
                            ));
                        }
                        Some(Val::Int(target))
                    }
                    _ => None,
                },
                // A newest partition with no lower end has no width: two
                // partitions is the nearest thing the series can be asked
                // for.
                _ => None,
            };
            Ok(match twice {
                Some(value) => (Need::Until(value), words),
                None => (Need::Partitions(2), words),
            })
        }
    }
}

/// Whether `ranges` reach what `need` asks, measured from `reference`.
fn covers(ranges: &[Range], reference: &Val, need: &Need) -> bool {
    if is_open_ended(ranges) {
        return true;
    }
    let mut ends = ranges.iter().filter_map(|r| r.hi.as_ref());
    match need {
        Need::Partitions(k) => {
            ends.filter(|hi| hi.val > *reference).count()
                >= usize::try_from(*k).unwrap_or(usize::MAX)
        }
        Need::Until(value) => ends.any(|hi| hi.val >= *value),
    }
}

// ---------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------

/// A series that does not reach the horizon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Short {
    /// Where the series ends.
    pub(crate) frontier: String,
    /// Where the partition holding the newest rows starts (or ends, for one
    /// with no lower end).
    pub(crate) held_from: String,
    /// What the horizon asks the series to reach.
    pub(crate) reach: Reach,
    /// The horizon, in words.
    pub(crate) horizon: String,
    /// The newest partition's width as `--every` spells it, if it can be.
    pub(crate) every: Option<String>,
    /// Whether the declarations already reach it.
    pub(crate) declared_covers: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Covered,
    Short(Short),
}

/// Whether `catalog`, the database's range partitions, reach the horizon past
/// `occupied`, one of them: the newest that holds a row. `declared` is what
/// the declarations give, to say whether what is missing is a plan applied.
///
/// `Err` is a horizon this key cannot be asked for, and never a verdict.
pub(crate) fn assess(
    key: Key,
    catalog: &[Range],
    declared: &[Range],
    occupied: &Range,
    horizon: &Horizon,
) -> Result<Verdict, String> {
    if is_open_ended(catalog) {
        return Ok(Verdict::Covered);
    }
    let Some(newest) = newest_first(catalog)
        .first()
        .map(|i| &catalog[*i])
        .filter(|r| r.hi.is_some())
    else {
        return Ok(Verdict::Covered);
    };
    let reference = occupied
        .lo
        .as_ref()
        .or(occupied.hi.as_ref())
        .ok_or_else(|| "the partition holding the newest rows has no bound".to_owned())?;
    let (needed, words) = need(key, horizon, reference, newest)?;
    if covers(catalog, &reference.val, &needed) {
        return Ok(Verdict::Covered);
    }
    let every = match (&newest.lo, &newest.hi) {
        (Some(lo), Some(hi)) => match (&lo.val, &hi.val) {
            (Val::Time(a), Val::Time(b)) => width_between(a, b).and_then(every_of),
            (Val::Int(a), Val::Int(b)) => Some((b - a).to_string()),
            _ => None,
        },
        _ => None,
    };
    let reach = match &needed {
        Need::Partitions(k) => Reach::Partitions(*k),
        Need::Until(Val::Time(c)) => Reach::Until(fmt_civil(key, c)),
        Need::Until(Val::Int(i)) => Reach::Until(i.to_string()),
    };
    Ok(Verdict::Short(Short {
        frontier: newest
            .hi
            .as_ref()
            .map(|e| e.text.clone())
            .unwrap_or_default(),
        held_from: reference.text.clone(),
        reach,
        horizon: words,
        every,
        declared_covers: covers(declared, &reference.val, &needed),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition_series::Int;

    fn d(text: &str) -> End {
        end(Key::Date, text).unwrap()
    }

    fn months(spec: &[(&str, &str)]) -> Vec<Range> {
        spec.iter()
            .map(|(lo, hi)| Range {
                lo: Some(d(lo)),
                hi: Some(d(hi)),
            })
            .collect()
    }

    /// The monthly series of 2026, January to the end of `to`.
    fn series(to: u32) -> Vec<Range> {
        let edge = |m: u32| {
            if m > 12 {
                format!("2027-{:02}-01", m - 12)
            } else {
                format!("2026-{m:02}-01")
            }
        };
        (1..=to)
            .map(|m| Range {
                lo: Some(d(&edge(m))),
                hi: Some(d(&edge(m + 1))),
            })
            .collect()
    }

    fn short(v: Result<Verdict, String>) -> Short {
        match v.unwrap() {
            Verdict::Short(s) => s,
            other => panic!("expected a short runway, got {other:?}"),
        }
    }

    #[test]
    fn a_series_twice_the_newest_width_past_the_rows_is_covered_by_default() {
        // Rows in October; the series ends at 2027-01-01, three months on.
        let ranges = series(12);
        let october = ranges[9].clone();
        assert_eq!(
            assess(Key::Date, &ranges, &ranges, &october, &Horizon::Default),
            Ok(Verdict::Covered)
        );
        // Rows in November: it ends exactly two months past November's start,
        // which the default accepts.
        let november = ranges[10].clone();
        assert_eq!(
            assess(Key::Date, &ranges, &ranges, &november, &Horizon::Default),
            Ok(Verdict::Covered)
        );
    }

    #[test]
    fn a_series_that_ends_in_the_partition_holding_the_rows_is_short() {
        // Rows in December, the last partition: the series ends at 2027-01-01
        // and two months from 2026-12-01 is 2027-02-01.
        let ranges = series(12);
        let december = ranges[11].clone();
        let s = short(assess(
            Key::Date,
            &ranges,
            &[],
            &december,
            &Horizon::Default,
        ));
        assert_eq!(s.frontier, "2027-01-01");
        assert_eq!(s.held_from, "2026-12-01");
        assert_eq!(s.reach, Reach::Until("2027-02-01".into()));
        assert_eq!(s.horizon, "twice the newest partition's width");
        assert_eq!(s.every.as_deref(), Some("1month"));
        assert!(!s.declared_covers);
    }

    #[test]
    fn declarations_that_already_reach_the_horizon_are_a_plan_to_apply() {
        let ranges = series(12);
        let december = ranges[11].clone();
        let mut declared = ranges.clone();
        declared.extend(months(&[
            ("2027-01-01", "2027-02-01"),
            ("2027-02-01", "2027-03-01"),
        ]));
        let s = short(assess(
            Key::Date,
            &ranges,
            &declared,
            &december,
            &Horizon::Default,
        ));
        assert!(s.declared_covers);
        // One month short of it is still a declaration to write.
        declared.pop();
        declared.pop();
        let s = short(assess(
            Key::Date,
            &ranges,
            &declared,
            &december,
            &Horizon::Default,
        ));
        assert!(!s.declared_covers);
    }

    #[test]
    fn a_configured_width_is_counted_on_the_calendar_from_the_occupied_partition() {
        let ranges = series(12);
        let october = ranges[9].clone();
        let width = |text: &str| Horizon::parse(Some(text)).unwrap();
        // Three months from 2026-10-01 is 2027-01-01, which the series reaches.
        assert_eq!(
            assess(Key::Date, &ranges, &[], &october, &width("3month")),
            Ok(Verdict::Covered)
        );
        // Four months is 2027-02-01, which it does not.
        let s = short(assess(
            Key::Date,
            &ranges,
            &[],
            &october,
            &width("4 months"),
        ));
        assert_eq!(s.reach, Reach::Until("2027-02-01".into()));
        assert_eq!(s.horizon, "4 months");
        // A year is a year, leap or not.
        let s = short(assess(Key::Date, &ranges, &[], &october, &width("1year")));
        assert_eq!(s.reach, Reach::Until("2027-10-01".into()));
        // And weeks and days are exact.
        let s = short(assess(Key::Date, &ranges, &[], &october, &width("20week")));
        assert_eq!(s.reach, Reach::Until("2027-02-18".into()));
    }

    #[test]
    fn a_month_added_to_the_end_of_a_month_stays_in_the_month() {
        // The engine's interval arithmetic: January 31 and a month is the last
        // day of February, in a leap year or not.
        let jan31 = parse_civil(Key::Date, "2027-01-31").ok().unwrap();
        assert_eq!(
            fmt_civil(Key::Date, &add(&jan31, Width::Months(1), 1).unwrap()),
            "2027-02-28"
        );
        let jan31 = parse_civil(Key::Date, "2028-01-31").ok().unwrap();
        assert_eq!(
            fmt_civil(Key::Date, &add(&jan31, Width::Months(1), 1).unwrap()),
            "2028-02-29"
        );
        assert_eq!(
            fmt_civil(Key::Date, &add(&jan31, Width::Months(13), 1).unwrap()),
            "2029-02-28"
        );
        // Across a year end, and a negative-free count of days across a leap day.
        let dec = parse_civil(Key::Date, "2026-12-15").ok().unwrap();
        assert_eq!(
            fmt_civil(Key::Date, &add(&dec, Width::Months(2), 1).unwrap()),
            "2027-02-15"
        );
        let feb = parse_civil(Key::Date, "2028-02-28").ok().unwrap();
        assert_eq!(
            fmt_civil(Key::Date, &add(&feb, Width::Seconds(86_400), 2).unwrap()),
            "2028-03-01"
        );
    }

    #[test]
    fn a_count_of_partitions_counts_the_one_holding_the_rows() {
        let ranges = series(12);
        let november = ranges[10].clone();
        let count = |text: &str| Horizon::parse(Some(text)).unwrap();
        // November and December: two partitions from November's start.
        assert_eq!(
            assess(Key::Date, &ranges, &[], &november, &count("2")),
            Ok(Verdict::Covered)
        );
        let s = short(assess(Key::Date, &ranges, &[], &november, &count("3")));
        assert_eq!(s.reach, Reach::Partitions(3));
        assert_eq!(s.horizon, "3 partitions");
        // One partition is the occupied one, which every series has.
        assert_eq!(
            assess(Key::Date, &ranges, &[], &ranges[11], &count("1 ")),
            Ok(Verdict::Covered)
        );
        assert_eq!(plural(1), "1 partition");
    }

    /// A series with a hole in it is counted by partitions, not by the span.
    #[test]
    fn a_count_of_partitions_skips_over_a_gap() {
        let ranges = months(&[
            ("2026-01-01", "2026-02-01"),
            ("2026-03-01", "2026-04-01"),
            ("2026-05-01", "2026-06-01"),
        ]);
        let january = ranges[0].clone();
        assert_eq!(
            assess(Key::Date, &ranges, &[], &january, &Horizon::Partitions(3)),
            Ok(Verdict::Covered)
        );
        assert!(matches!(
            assess(Key::Date, &ranges, &[], &january, &Horizon::Partitions(4)),
            Ok(Verdict::Short(_))
        ));
    }

    #[test]
    fn integer_keys_use_a_count_or_the_default_width() {
        let int = |lo: &str, hi: &str| Range {
            lo: Some(end(Key::Integer(INT), lo).unwrap()),
            hi: Some(end(Key::Integer(INT), hi).unwrap()),
        };
        const INT: Int = Int {
            name: "bigint",
            min: i64::MIN as i128,
            max: i64::MAX as i128,
        };
        let key = Key::Integer(INT);
        let ranges = vec![int("0", "1000"), int("1000", "2000"), int("2000", "3000")];
        // Rows in the middle partition: two widths from 1000 is 3000.
        assert_eq!(
            assess(key, &ranges, &[], &ranges[1], &Horizon::Default),
            Ok(Verdict::Covered)
        );
        let s = short(assess(key, &ranges, &[], &ranges[2], &Horizon::Default));
        assert_eq!(s.frontier, "3000");
        assert_eq!(s.reach, Reach::Until("4000".into()));
        assert_eq!(s.every.as_deref(), Some("1000"));
        assert_eq!(
            assess(key, &ranges, &[], &ranges[0], &Horizon::Partitions(3)),
            Ok(Verdict::Covered)
        );
        // A width of time on a number is refused, not guessed at.
        let refused = assess(
            key,
            &ranges,
            &[],
            &ranges[0],
            &Horizon::parse(Some("2month")).unwrap(),
        )
        .unwrap_err();
        assert!(
            refused.contains("a width of time") && refused.contains("bigint"),
            "{refused}"
        );
    }

    #[test]
    fn timestamps_keep_their_clock_and_their_zone() {
        let at = |k: Key, text: &str| end(k, text).unwrap();
        let tz = Key::TimestampTz;
        let weeks = vec![
            Range {
                lo: Some(at(tz, "2026-12-28 00:00:00+00")),
                hi: Some(at(tz, "2027-01-04 00:00:00+00")),
            },
            Range {
                lo: Some(at(tz, "2027-01-04 00:00:00+00")),
                hi: Some(at(tz, "2027-01-11 00:00:00+00")),
            },
        ];
        let s = short(assess(tz, &weeks, &[], &weeks[1], &Horizon::Default));
        assert_eq!(s.reach, Reach::Until("2027-01-18 00:00:00+00".into()));
        assert_eq!(s.every.as_deref(), Some("1week"));
        let plain = vec![Range {
            lo: Some(at(Key::Timestamp, "2026-01-01 00:00:00")),
            hi: Some(at(Key::Timestamp, "2026-01-01 06:00:00")),
        }];
        // Six hours is not a step the generator writes, so the remedy keeps its
        // placeholder, but the horizon still works in seconds.
        let s = short(assess(
            Key::Timestamp,
            &plain,
            &[],
            &plain[0],
            &Horizon::Default,
        ));
        assert_eq!(s.reach, Reach::Until("2026-01-01 12:00:00".into()));
        assert_eq!(s.every, None);
    }

    #[test]
    fn a_partition_without_a_lower_end_is_measured_from_its_upper_end() {
        let ranges = vec![Range {
            lo: None,
            hi: Some(d("2026-01-01")),
        }];
        // Nothing past the frontier: the only partition is the one with the
        // rows, and the default needs two partitions.
        let s = short(assess(
            Key::Date,
            &ranges,
            &[],
            &ranges[0],
            &Horizon::Default,
        ));
        assert_eq!(s.held_from, "2026-01-01");
        assert_eq!(s.every, None);
        assert_eq!(s.reach, Reach::Partitions(2));
    }

    #[test]
    fn a_series_with_no_upper_end_cannot_run_out() {
        let ranges = vec![
            Range {
                lo: Some(d("2026-01-01")),
                hi: Some(d("2026-02-01")),
            },
            Range {
                lo: Some(d("2026-02-01")),
                hi: None,
            },
        ];
        assert!(is_open_ended(&ranges));
        assert_eq!(
            assess(Key::Date, &ranges, &[], &ranges[1], &Horizon::Partitions(5)),
            Ok(Verdict::Covered)
        );
    }

    #[test]
    fn partitions_are_searched_newest_first() {
        let ranges = months(&[
            ("2026-02-01", "2026-03-01"),
            ("2026-04-01", "2026-05-01"),
            ("2026-01-01", "2026-02-01"),
        ]);
        assert_eq!(newest_first(&ranges), vec![1, 0, 2]);
    }

    fn stamp(key: Key, lo: &str, hi: &str) -> Range {
        Range {
            lo: Some(end(key, lo).unwrap()),
            hi: Some(end(key, hi).unwrap()),
        }
    }

    /// The values a `timestamp` or `timestamptz` bound holds, in the
    /// engine's spelling: no fraction, and fractions of every length that
    /// does not end in a zero, at both ends of the supported years.
    const STAMPS: [&str; 10] = [
        "2026-01-02 00:00:00",
        "2026-01-02 00:00:00.1",
        "2026-01-02 00:00:00.000001",
        "2026-01-02 00:00:00.123456",
        "2026-01-02 00:00:00.5",
        "2026-01-02 00:00:00.999999",
        "2026-01-02 23:59:59.01",
        "2026-01-02 12:30:45.00001",
        "0001-01-01 00:00:00.000001",
        "9999-12-31 23:59:59.999999",
    ];

    #[test]
    fn a_printed_bound_reads_back_as_the_value_it_was_printed_from() {
        for key in [Key::Timestamp, Key::TimestampTz] {
            for text in STAMPS {
                let spelled = if key == Key::TimestampTz {
                    format!("{text}+00")
                } else {
                    text.to_owned()
                };
                let c = parse_civil(key, &spelled).ok().unwrap();
                // The engine's spelling is the one fmt_civil writes.
                assert_eq!(fmt_civil(key, &c), spelled, "{key:?}");
                assert_eq!(
                    parse_civil(key, &fmt_civil(key, &c)).ok(),
                    Some(c),
                    "{spelled}"
                );
            }
        }
    }

    #[test]
    fn a_printed_fraction_is_trimmed_and_never_a_bare_point() {
        let micros = |micros: i64| {
            let c = Civil {
                y: 2026,
                m: 1,
                d: 2,
                secs: 0,
                micros,
            };
            fmt_civil(Key::Timestamp, &c)
        };
        assert_eq!(micros(0), "2026-01-02 00:00:00");
        assert_eq!(micros(100_000), "2026-01-02 00:00:00.1");
        assert_eq!(micros(120_000), "2026-01-02 00:00:00.12");
        assert_eq!(micros(1), "2026-01-02 00:00:00.000001");
        assert_eq!(micros(10), "2026-01-02 00:00:00.00001");
        assert_eq!(micros(999_999), "2026-01-02 00:00:00.999999");
        // Whatever it prints, the reader takes: no spelling here is one the
        // generator would refuse as not the engine's.
        for m in [0, 1, 10, 100, 1_000, 10_000, 100_000, 500_000, 999_999] {
            assert!(
                parse_civil(Key::Timestamp, &micros(m)).is_ok(),
                "{}",
                micros(m)
            );
        }
        // The zone follows the fraction, as the engine writes it.
        let c = parse_civil(Key::TimestampTz, "2026-01-02 00:00:00.5+00")
            .ok()
            .unwrap();
        assert_eq!(fmt_civil(Key::TimestampTz, &c), "2026-01-02 00:00:00.5+00");
    }

    #[test]
    fn a_date_or_an_integer_key_prints_no_clock_and_no_fraction() {
        // A date holds no clock, so a stray fraction on the value cannot
        // reach the text.
        let c = Civil {
            y: 2026,
            m: 1,
            d: 2,
            secs: 3_661,
            micros: 500_000,
        };
        assert_eq!(fmt_civil(Key::Date, &c), "2026-01-02");
        assert_eq!(
            fmt_civil(
                Key::Integer(crate::partition_series::Int {
                    name: "bigint",
                    min: i64::MIN as i128,
                    max: i64::MAX as i128,
                }),
                &c
            ),
            "2026-01-02"
        );
        let whole = parse_civil(Key::Date, "2026-01-02").ok().unwrap();
        assert_eq!(fmt_civil(Key::Date, &whole), "2026-01-02");
        // And a date bound with a clock is not a date bound at all.
        assert!(parse_civil(Key::Date, "2026-01-02 00:00:00.5").is_err());
        assert!(parse_civil(Key::Date, "2026-01-02 00:00:00").is_err());
    }

    /// Two bounds inside one second are two bounds (#1923): the probe that
    /// looks for the newest rows must try the later one first.
    #[test]
    fn partitions_ending_inside_one_second_are_searched_newest_first() {
        let early = stamp(
            Key::Timestamp,
            "2026-01-01 00:00:00",
            "2026-01-02 00:00:00.1",
        );
        let late = stamp(
            Key::Timestamp,
            "2026-01-02 00:00:00.1",
            "2026-01-02 00:00:00.9",
        );
        assert_eq!(
            newest_first(&[early.clone(), late.clone()]),
            vec![1, 0],
            "the .9 end is after the .1 end"
        );
        assert_eq!(newest_first(&[late, early]), vec![0, 1]);
        // The same on a timestamptz, whose zone suffix sits after the fraction.
        let early = stamp(
            Key::TimestampTz,
            "2026-01-01 00:00:00+00",
            "2026-01-02 00:00:00.1+00",
        );
        let late = stamp(
            Key::TimestampTz,
            "2026-01-02 00:00:00.1+00",
            "2026-01-02 00:00:00.9+00",
        );
        assert_eq!(newest_first(&[early, late]), vec![1, 0]);
    }

    /// A frontier a fraction of a second early does not reach a width that
    /// lands later in that second, and the reach says where it lands.
    #[test]
    fn a_width_landing_inside_a_second_is_judged_by_its_fraction() {
        let one_day = Horizon::parse(Some("1day")).unwrap();
        // Rows from 2026-01-01 00:00:00.5, so a day on is 2026-01-02 00:00:00.5.
        let held = stamp(
            Key::Timestamp,
            "2026-01-01 00:00:00.5",
            "2026-01-02 00:00:00.1",
        );
        // The series ends at .1, before the .5 the horizon asks for.
        let ranges = [held.clone()];
        let s = short(assess(Key::Timestamp, &ranges, &[], &held, &one_day));
        assert_eq!(s.frontier, "2026-01-02 00:00:00.1");
        assert_eq!(s.held_from, "2026-01-01 00:00:00.5");
        assert_eq!(s.reach, Reach::Until("2026-01-02 00:00:00.5".into()));
        // The same on a timestamptz: the zone follows the fraction.
        let held_tz = stamp(
            Key::TimestampTz,
            "2026-01-01 00:00:00.5+00",
            "2026-01-02 00:00:00.1+00",
        );
        let s = short(assess(
            Key::TimestampTz,
            std::slice::from_ref(&held_tz),
            &[],
            &held_tz,
            &one_day,
        ));
        assert_eq!(s.reach, Reach::Until("2026-01-02 00:00:00.5+00".into()));

        // A partition that ends at .9 does reach it, whichever order the
        // catalog lists the two in: .9 is after .5.
        let next = stamp(
            Key::Timestamp,
            "2026-01-02 00:00:00.1",
            "2026-01-02 00:00:00.9",
        );
        for ranges in [[held.clone(), next.clone()], [next.clone(), held.clone()]] {
            assert_eq!(
                assess(Key::Timestamp, &ranges, &[], &held, &one_day),
                Ok(Verdict::Covered)
            );
        }

        // And a width that lands past .9, at .95, is not reached by it. The
        // frontier named is the later of the two, in either order.
        let held_late = stamp(
            Key::Timestamp,
            "2026-01-01 00:00:00.95",
            "2026-01-02 00:00:00.1",
        );
        for ranges in [
            [held_late.clone(), next.clone()],
            [next.clone(), held_late.clone()],
        ] {
            let s = short(assess(Key::Timestamp, &ranges, &[], &held_late, &one_day));
            assert_eq!(s.frontier, "2026-01-02 00:00:00.9");
            assert_eq!(s.reach, Reach::Until("2026-01-02 00:00:00.95".into()));
        }

        // Negative: a whole-second horizon from a whole-second row prints no
        // fraction, and a fraction elsewhere in the series does not add one.
        let whole = stamp(Key::Timestamp, "2026-01-01 00:00:00", "2026-01-02 00:00:00");
        let two_days = Horizon::parse(Some("2day")).unwrap();
        let s = short(assess(
            Key::Timestamp,
            std::slice::from_ref(&whole),
            &[],
            &whole,
            &two_days,
        ));
        assert_eq!(s.reach, Reach::Until("2026-01-03 00:00:00".into()));
    }

    #[test]
    fn a_horizon_is_a_count_or_a_width_and_nothing_else() {
        assert_eq!(Horizon::parse(None), Ok(Horizon::Default));
        assert_eq!(Horizon::parse(Some(" 4 ")), Ok(Horizon::Partitions(4)));
        assert!(matches!(
            Horizon::parse(Some("2 Months")),
            Ok(Horizon::Width { .. })
        ));
        // Negative cases: each is named, and none becomes the default.
        for refused in [
            "0",
            "000",
            "",
            "  ",
            "month",
            "2fortnight",
            "-1",
            "1.5month",
            "99999999999",
        ] {
            let message = Horizon::parse(Some(refused)).unwrap_err();
            assert!(
                message.contains("partition_runway") || message.contains("count"),
                "{refused}: {message}"
            );
        }
        let message = Horizon::parse(Some("2fortnight")).unwrap_err();
        assert!(message.contains("`fortnight` is not a unit"), "{message}");
        assert!(message.contains("count of partitions"), "{message}");
        assert!(
            !message.contains("--every"),
            "a setting is not a flag: {message}"
        );
    }

    #[test]
    fn bounds_the_engine_would_not_print_are_refused_with_the_generators_words() {
        let refused = end(Key::Date, "2027-1-1").unwrap_err();
        assert!(
            refused.contains("not how the engine prints a date bound"),
            "{refused}"
        );
        let refused = end(Key::TimestampTz, "2027-01-01 00:00:00+08").unwrap_err();
        assert!(refused.contains("not in UTC"), "{refused}");
        assert!(range(Key::Date, &BoundDatum::MaxValue, &BoundDatum::MaxValue).is_err());
        assert!(range(Key::Date, &BoundDatum::MinValue, &BoundDatum::MinValue).is_err());
        let all = range(Key::Date, &BoundDatum::MinValue, &BoundDatum::MaxValue).unwrap();
        assert_eq!((all.lo, all.hi), (None, None));
    }

    // -----------------------------------------------------------------------
    // A horizon that leaves the calendar (#1924)
    // -----------------------------------------------------------------------

    fn width(text: &str) -> Horizon {
        Horizon::parse(Some(text)).unwrap()
    }

    /// The reason a horizon could not be asked, which must not have built the
    /// year it names: the point is that no value past the calendar exists.
    fn undetermined(v: Result<Verdict, String>) -> String {
        match v {
            Err(reason) => reason,
            other => panic!("expected an undetermined horizon, got {other:?}"),
        }
    }

    #[test]
    fn the_calendar_here_is_the_one_a_bound_is_read_in() {
        // The edge of `YEARS` is the edge of `parse_civil`, which the
        // generator steps through: a value either can name, the other can.
        assert!(parse_civil(Key::Date, "9999-12-31").is_ok());
        assert!(parse_civil(Key::Date, "0001-01-01").is_ok());
        assert!(parse_civil(Key::Date, "10000-01-01").is_err());
        assert!(parse_civil(Key::Date, "0000-12-31").is_err());
        assert_eq!((*YEARS.start(), *YEARS.end()), (1, 9999));
    }

    #[test]
    fn a_width_that_leaves_the_calendar_is_undetermined_not_short() {
        let ranges = series(12);
        let october = ranges[9].clone();
        for text in ["100000year", "9000year", "99999month"] {
            let reason = undetermined(assess(Key::Date, &ranges, &[], &october, &width(text)));
            assert!(
                reason.contains(&format!("partition_runway `{text}`"))
                    && reason.contains("counted from `2026-10-01`")
                    && reason.contains("reaches past the supported calendar")
                    && reason.contains("which ends in year 9999")
                    && reason.contains("a count of partitions, as `3`"),
                "{text}: {reason}"
            );
        }
        // No year past the calendar is spelled out: 2026 and 9000 years is
        // 11026, and that value is never built.
        let reason = undetermined(assess(
            Key::Date,
            &ranges,
            &[],
            &october,
            &width("9000year"),
        ));
        assert!(!reason.contains("11026"), "{reason}");
        // Days and weeks leave it too, near the end of it.
        let late = months(&[("9999-06-01", "9999-07-01")]);
        for text in ["365day", "31week"] {
            let reason = undetermined(assess(Key::Date, &late, &[], &late[0], &width(text)));
            assert!(
                reason.contains("counted from `9999-06-01`")
                    && reason.contains("reaches past the supported calendar"),
                "{text}: {reason}"
            );
        }
    }

    #[test]
    fn a_time_key_that_leaves_the_calendar_is_undetermined_whatever_its_fraction() {
        for (key, suffix) in [(Key::Timestamp, ""), (Key::TimestampTz, "+00")] {
            let at = |text: &str| end(key, &format!("{text}{suffix}")).unwrap();
            let held = Range {
                lo: Some(at("2026-01-01 00:00:00.5")),
                hi: Some(at("2026-01-02 00:00:00.5")),
            };
            for text in ["100000year", "9000year", "99999month"] {
                let reason = undetermined(assess(
                    key,
                    std::slice::from_ref(&held),
                    &[],
                    &held,
                    &width(text),
                ));
                assert!(
                    reason.contains(&format!("counted from `2026-01-01 00:00:00.5{suffix}`"))
                        && reason.contains("reaches past the supported calendar"),
                    "{key:?} {text}: {reason}"
                );
            }
        }
    }

    #[test]
    fn a_default_horizon_that_leaves_the_calendar_is_undetermined_not_short() {
        // Yearly partitions of 9997 and 9998: twice a year from 9998 is 10000.
        let ranges = months(&[("9997-01-01", "9998-01-01"), ("9998-01-01", "9999-01-01")]);
        let reason = undetermined(assess(
            Key::Date,
            &ranges,
            &[],
            &ranges[1],
            &Horizon::Default,
        ));
        assert!(
            reason.contains(
                "the default horizon, twice the newest partition's width, counted from \
                 `9998-01-01`, reaches past the supported calendar"
            ) && reason.contains("set partition_runway to a count of partitions"),
            "{reason}"
        );
        // Negative: from 9997 it is 9999-01-01, which the series reaches.
        assert_eq!(
            assess(Key::Date, &ranges, &[], &ranges[0], &Horizon::Default),
            Ok(Verdict::Covered)
        );
        // And a count of partitions, which has no calendar, is still asked.
        let s = short(assess(
            Key::Date,
            &ranges,
            &[],
            &ranges[1],
            &Horizon::Partitions(3),
        ));
        assert_eq!(s.reach, Reach::Partitions(3));
    }

    /// Negative: a target that lands in the last year the calendar has is a
    /// target like any other, short or covered on its merits.
    #[test]
    fn a_horizon_ending_in_year_9999_is_assessed_as_usual() {
        // A date: the series ends 9999-12-30; a day from 9999-12-29 is
        // 9999-12-30 and covered, two days is 9999-12-31 and short.
        let days = months(&[("9999-12-29", "9999-12-30")]);
        assert_eq!(
            assess(Key::Date, &days, &[], &days[0], &width("1day")),
            Ok(Verdict::Covered)
        );
        let s = short(assess(Key::Date, &days, &[], &days[0], &width("2day")));
        assert_eq!(s.reach, Reach::Until("9999-12-31".into()));
        assert_eq!(s.frontier, "9999-12-30");
        // The day after it is not a date, and is not a point to fall short of.
        assert!(
            undetermined(assess(Key::Date, &days, &[], &days[0], &width("3day")))
                .contains("reaches past the supported calendar")
        );
        // Thirty weeks from the 1st of June is the 28th of December: short of
        // it, and still a finding about the series.
        let late = months(&[("9999-06-01", "9999-07-01")]);
        let s = short(assess(Key::Date, &late, &[], &late[0], &width("30week")));
        assert_eq!(s.reach, Reach::Until("9999-12-28".into()));
        // Months and years end in 9999 too: a year from 9998-01-01 is
        // 9999-01-01, a short runway for a series that ends on it, and 2 years
        // is 10000-01-01.
        let year = months(&[("9997-01-01", "9998-01-01")]);
        let s = short(assess(Key::Date, &year, &[], &year[0], &width("2year")));
        assert_eq!(s.reach, Reach::Until("9999-01-01".into()));
        assert!(
            undetermined(assess(Key::Date, &year, &[], &year[0], &width("3year")))
                .contains("reaches past the supported calendar")
        );
        // A timestamp, to the last microsecond the calendar has.
        for (key, suffix) in [(Key::Timestamp, ""), (Key::TimestampTz, "+00")] {
            let at = |text: &str| end(key, &format!("{text}{suffix}")).unwrap();
            let held = Range {
                lo: Some(at("9999-12-30 23:59:59.999999")),
                hi: Some(at("9999-12-31 00:00:00")),
            };
            let s = short(assess(
                key,
                std::slice::from_ref(&held),
                &[],
                &held,
                &width("1day"),
            ));
            assert_eq!(
                s.reach,
                Reach::Until(format!("9999-12-31 23:59:59.999999{suffix}")),
                "{key:?}"
            );
            // One day on from the start of that day is out of the calendar.
            assert!(
                undetermined(assess(
                    key,
                    std::slice::from_ref(&held),
                    &[],
                    &held,
                    &width("2day"),
                ))
                .contains("counted from `9999-12-30 23:59:59.999999"),
                "{key:?}"
            );
        }
    }

    /// A count of partitions is not a point on any axis: the largest one the
    /// setting reads is a short runway, for a key with a calendar and a key
    /// without one alike, and never an undetermined horizon.
    #[test]
    fn a_huge_count_of_partitions_is_short_not_undetermined() {
        let huge = u32::MAX.to_string();
        let ranges = series(12);
        let s = short(assess(Key::Date, &ranges, &[], &ranges[11], &width(&huge)));
        assert_eq!(s.reach, Reach::Partitions(u32::MAX));
        assert_eq!(s.horizon, "4294967295 partitions");
        let int = |lo: &str, hi: &str| Range {
            lo: Some(end(Key::Integer(BIGINT), lo).unwrap()),
            hi: Some(end(Key::Integer(BIGINT), hi).unwrap()),
        };
        const BIGINT: Int = Int {
            name: "bigint",
            min: i64::MIN as i128,
            max: i64::MAX as i128,
        };
        let ints = vec![int("0", "1000"), int("1000", "2000")];
        let s = short(assess(
            Key::Integer(BIGINT),
            &ints,
            &[],
            &ints[1],
            &width(&huge),
        ));
        assert_eq!(s.reach, Reach::Partitions(u32::MAX));
    }

    const SMALLINT: Int = Int {
        name: "smallint",
        min: i16::MIN as i128,
        max: i16::MAX as i128,
    };
    const INTEGER: Int = Int {
        name: "integer",
        min: i32::MIN as i128,
        max: i32::MAX as i128,
    };
    const BIGINT_KEY: Int = Int {
        name: "bigint",
        min: i64::MIN as i128,
        max: i64::MAX as i128,
    };

    /// One partition `[lo, hi)` of an integer key, as the engine prints it.
    fn whole(int: Int, lo: i128, hi: i128) -> Range {
        Range {
            lo: Some(end(Key::Integer(int), &lo.to_string()).unwrap()),
            hi: Some(end(Key::Integer(int), &hi.to_string()).unwrap()),
        }
    }

    /// A default horizon whose target lies above the largest value the key's
    /// type holds is undetermined, for each integer type by its own maximum:
    /// no partition can end at it, so the series is not short of anything
    /// (#2020).
    #[test]
    fn a_default_horizon_above_the_largest_integer_is_undetermined_not_short() {
        for (int, width) in [(SMALLINT, 100), (INTEGER, 1000), (BIGINT_KEY, 1000)] {
            // One past what the type holds: twice the width on from here.
            let from = int.max - 2 * width + 1;
            let held = whole(int, from, from + width);
            let reason = undetermined(assess(
                Key::Integer(int),
                std::slice::from_ref(&held),
                &[],
                &held,
                &Horizon::Default,
            ));
            assert!(
                reason.contains(&format!(
                    "the default horizon, twice the newest partition's width, counted from \
                     `{from}`, reaches past the largest {} value, {}",
                    int.name, int.max
                )) && reason.contains("set partition_runway to a count of partitions, as `3`"),
                "{}: {reason}",
                int.name
            );
            // The target is not built, so it is not spelled out.
            assert!(
                !reason.contains(&(from + 2 * width).to_string()),
                "{}: {reason}",
                int.name
            );
        }
        // The case the issue names: an `integer` series ending at 2147483000
        // with width 1000 and rows in its newest partition.
        let held = whole(INTEGER, 2_147_482_000, 2_147_483_000);
        let reason = undetermined(assess(
            Key::Integer(INTEGER),
            std::slice::from_ref(&held),
            &[],
            &held,
            &Horizon::Default,
        ));
        assert!(
            reason.contains("counted from `2147482000`")
                && reason.contains("reaches past the largest integer value, 2147483647"),
            "{reason}"
        );
    }

    /// Negative: a target that is exactly the largest value is a value a
    /// series can end at, so it is short or covered on its merits.
    #[test]
    fn a_default_horizon_at_the_largest_integer_is_assessed_as_usual() {
        for (int, width) in [(SMALLINT, 100), (INTEGER, 1000), (BIGINT_KEY, 1000)] {
            let key = Key::Integer(int);
            let from = int.max - 2 * width;
            let held = whole(int, from, from + width);
            let s = short(assess(
                key,
                std::slice::from_ref(&held),
                &[],
                &held,
                &Horizon::Default,
            ));
            assert_eq!(s.reach, Reach::Until(int.max.to_string()), "{}", int.name);
            assert_eq!(s.frontier, (from + width).to_string());
            assert_eq!(s.every, Some(width.to_string()));
            // A series that does end at the largest value reaches it.
            let ranges = vec![held.clone(), whole(int, from + width, int.max)];
            assert_eq!(
                assess(key, &ranges, &[], &held, &Horizon::Default),
                Ok(Verdict::Covered),
                "{}",
                int.name
            );
            // One value on, the target is out of the type again.
            let past = whole(int, from + 1, from + width + 1);
            assert!(
                undetermined(assess(
                    key,
                    std::slice::from_ref(&past),
                    &[],
                    &past,
                    &Horizon::Default,
                ))
                .contains(&format!("reaches past the largest {} value", int.name)),
                "{}",
                int.name
            );
        }
    }

    /// Negative: only the target of the default horizon is a point on the
    /// key's axis. A count of partitions, the only horizon an integer key
    /// is given, is a number of partitions at any position of the series,
    /// the last value of the type included.
    #[test]
    fn a_count_of_partitions_at_the_largest_integer_is_short_not_undetermined() {
        let held = whole(INTEGER, 2_147_482_000, 2_147_483_000);
        for k in [2_u32, 3, u32::MAX] {
            let s = short(assess(
                Key::Integer(INTEGER),
                std::slice::from_ref(&held),
                &[],
                &held,
                &Horizon::Partitions(k),
            ));
            assert_eq!(s.reach, Reach::Partitions(k));
        }
    }

    /// The lower end of the type is never a target: the width is positive, so
    /// the default horizon is above the rows, which are no lower than the
    /// type's minimum.
    #[test]
    fn a_default_horizon_from_the_smallest_integer_is_assessed_as_usual() {
        for (int, width) in [(SMALLINT, 100), (INTEGER, 1000), (BIGINT_KEY, 1000)] {
            let held = whole(int, int.min, int.min + width);
            let s = short(assess(
                Key::Integer(int),
                std::slice::from_ref(&held),
                &[],
                &held,
                &Horizon::Default,
            ));
            assert_eq!(
                s.reach,
                Reach::Until((int.min + 2 * width).to_string()),
                "{}",
                int.name
            );
            // A newest partition with no lower end has no width: two
            // partitions, not a point near the minimum.
            let open = Range {
                lo: None,
                hi: held.hi.clone(),
            };
            let s = short(assess(
                Key::Integer(int),
                std::slice::from_ref(&open),
                &[],
                &open,
                &Horizon::Default,
            ));
            assert_eq!(s.reach, Reach::Partitions(2), "{}", int.name);
        }
    }
}
