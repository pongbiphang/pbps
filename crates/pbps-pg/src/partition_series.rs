// Names in whole-escaped DialectError fields and shared probe text stay raw
// until #2084 step 5 (DEC-2084.1) makes those messages escape their individual values.

//! A series of RANGE partitions, generated from the declarations
//! (#1789, DEC-1789.1).
//!
//! `pbps partitions extend` asks this for the partitions that carry a parent's
//! series from where its last one ends to a stated end. Everything the answer
//! depends on is in the request: the declarations, the step, the end. Nothing
//! reads a clock or a database, so the same tree and the same words give the
//! same partitions on every machine, and what is written into git is what
//! was asked for.
//!
//! # What it writes, and why it writes exactly that
//!
//! A bound is the engine's own text for a value under the session this tool
//! pins, `TimeZone` UTC and ISO dates (DEC-1170.1), and a connected plan
//! refuses any other spelling. Measured on 16.15 and 18.6, the engine prints
//!
//! | key | bound |
//! |---|---|
//! | `date` | `2027-01-01` |
//! | `timestamp` | `2027-01-01 00:00:00` |
//! | `timestamptz` | `2027-01-01 00:00:00+00` |
//! | `smallint`, `integer`, `bigint` | `1000` |
//!
//! and the generator writes those and nothing else. They read the same under
//! every `DateStyle`, `TimeZone` and abbreviation set, so they pass
//! DEC-1756.1's single-reading check without a rule of its own about them. The
//! live test `a_generated_series_is_planned_applied_and_verified_through_the_cli`
//! is the proof of both, since it runs the real checks.
//!
//! # Why a series that does not tile is refused
//!
//! A step that does not land on the end it was given would leave a gap or an
//! overlap in the key space, and a gap is rows with no partition. So the
//! series starts where the last range ends, and both that start and `--until`
//! must lie on the grid the step makes. A month steps from the first of a
//! month at midnight, a day or a week from midnight. Dates are counted on the
//! calendar from the start, never stepped one after another.
//!
//! A refusal names the nearest values that would tile, because "your end is
//! wrong" without the right one is a second round trip. A nearest value above
//! what the key can hold is left out and said so: an integer key's above its
//! type's maximum (#1905), a temporal key's past year 9999, the last year a
//! bound is read or written in (#1996).
//!
//! # Limits, each a refusal rather than a guess
//!
//! - One key column of `date`, `timestamp`, `timestamptz`, `smallint`,
//!   `integer` or `bigint`. A `text` key has no step, and a key of several
//!   columns has no single frontier.
//! - Years 1 to 9999. The engine prints a year outside them with five digits
//!   or a ` BC` suffix, and a helper that knew those forms would be a larger
//!   one than a tool that only extends a present-day series needs.
//! - A `timestamptz` bound is read in UTC, `+00`. Bounds in another zone wait
//!   for a project time zone (#1757).
//! - A run adds at most [`MAX_PER_RUN`] partitions. `--every 1 --until` a
//!   `bigint`'s end would otherwise be a request for nine quintillion tables;
//!   a longer series is two runs.
//! - A negative integer bound has no name. `ev_-5` is not an identifier, and a
//!   convention for it would be one more thing for a reader to learn.

use pbps_dialect::{DialectError, RangeSeries, RangeSeriesRequest, SeriesPartition};
use pbps_model::{BoundDatum, ColumnType, PartitionBound, TableName};

use crate::MAX_IDENT_BYTES;
use crate::types::DIALECT;

/// The most partitions one run adds.
pub(crate) const MAX_PER_RUN: i128 = 2000;

/// The step `--every 1month` and the like may take.
const MAX_STEP: i64 = 100_000;

fn invalid(message: impl Into<String>) -> DialectError {
    DialectError::Invalid {
        dialect: DIALECT,
        message: message.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Int {
    pub(crate) name: &'static str,
    pub(crate) min: i128,
    pub(crate) max: i128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Date,
    Timestamp,
    TimestampTz,
    Integer(Int),
}

impl Key {
    /// The key a column of this type makes, if it makes one. The type is
    /// normalized first, so `int4`, `timestamptz` and `timestamp with time
    /// zone` are what they are, and an array of any of them is none.
    pub(crate) fn of(ty: &ColumnType) -> Option<Self> {
        let ty = crate::types::normalize(ty).ok()?;
        if ty.is_array() {
            return None;
        }
        Some(match ty.base.as_str() {
            "date" => Key::Date,
            "timestamp without time zone" => Key::Timestamp,
            "timestamp with time zone" => Key::TimestampTz,
            "smallint" => Key::Integer(Int {
                name: "smallint",
                min: i128::from(i16::MIN),
                max: i128::from(i16::MAX),
            }),
            "integer" => Key::Integer(Int {
                name: "integer",
                min: i128::from(i32::MIN),
                max: i128::from(i32::MAX),
            }),
            "bigint" => Key::Integer(Int {
                name: "bigint",
                min: i128::from(i64::MIN),
                max: i128::from(i64::MAX),
            }),
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Key::Date => "date",
            Key::Timestamp => "timestamp",
            Key::TimestampTz => "timestamptz",
            Key::Integer(int) => int.name,
        }
    }

    /// A value as the engine prints it, to show in a message.
    fn example(self) -> &'static str {
        match self {
            Key::Date => "2027-01-01",
            Key::Timestamp => "2027-01-01 00:00:00",
            Key::TimestampTz => "2027-01-01 00:00:00+00",
            Key::Integer(_) => "1000",
        }
    }
}

// ---------------------------------------------------------------------------
// The calendar
// ---------------------------------------------------------------------------

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`). The engine's calendar is proleptic Gregorian too, and
/// a year in 1 to 9999 is all this is asked about.
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub(crate) fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

/// A date and a time of day, as far as the engine prints a bound.
///
/// `Ord` is the order of the instants, since the fields run from the most
/// significant to the least.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Civil {
    pub(crate) y: i64,
    pub(crate) m: i64,
    pub(crate) d: i64,
    /// Whole seconds into the day.
    pub(crate) secs: i64,
    /// The fraction of a second, in microseconds. It is a magnitude, not a
    /// flag: two bounds that differ only inside one second are two bounds,
    /// and ordered as such (#1912). The engine keeps microseconds and no
    /// finer, and `parse_civil` refuses a longer fraction.
    pub(crate) micros: i64,
}

/// Why a text is not a bound of the key's type.
pub(crate) enum Reject {
    /// Not the spelling the engine prints.
    Spelling,
    /// A `timestamptz` in another zone than UTC.
    Offset,
}

fn digits(text: &str, width: usize) -> Option<i64> {
    (text.len() == width && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// `text` as `key` prints a bound, and no other way: years 1 to 9999, a
/// two-digit month, day, hour, minute and second, and for `timestamptz` the
/// UTC offset `+00`.
pub(crate) fn parse_civil(key: Key, text: &str) -> Result<Civil, Reject> {
    // Ascii, so that every slice below is on a character boundary.
    let date = text
        .get(..10)
        .filter(|d| d.is_ascii())
        .ok_or(Reject::Spelling)?;
    let rest = &text[10..];
    let bytes = date.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(Reject::Spelling);
    }
    let y = digits(&date[..4], 4).ok_or(Reject::Spelling)?;
    let m = digits(&date[5..7], 2).ok_or(Reject::Spelling)?;
    let d = digits(&date[8..], 2).ok_or(Reject::Spelling)?;
    if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=days_in_month(y, m)).contains(&d)
    {
        return Err(Reject::Spelling);
    }
    let mut civil = Civil {
        y,
        m,
        d,
        secs: 0,
        micros: 0,
    };
    if key == Key::Date {
        return if rest.is_empty() {
            Ok(civil)
        } else {
            Err(Reject::Spelling)
        };
    }
    let time = rest.strip_prefix(' ').ok_or(Reject::Spelling)?;
    let clock = time
        .get(..8)
        .filter(|c| c.is_ascii())
        .ok_or(Reject::Spelling)?;
    if clock.as_bytes()[2] != b':' || clock.as_bytes()[5] != b':' {
        return Err(Reject::Spelling);
    }
    let h = digits(&clock[..2], 2).ok_or(Reject::Spelling)?;
    let mi = digits(&clock[3..5], 2).ok_or(Reject::Spelling)?;
    let s = digits(&clock[6..], 2).ok_or(Reject::Spelling)?;
    if h > 23 || mi > 59 || s > 59 {
        return Err(Reject::Spelling);
    }
    civil.secs = h * 3600 + mi * 60 + s;
    let mut after = &time[8..];
    if let Some(fraction) = after.strip_prefix('.') {
        let n = fraction
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count()
            .min(6);
        // The engine trims trailing zeros, so a fraction of zeros or one
        // ending in a zero is not how it prints a value.
        let shown = &fraction[..n];
        if n == 0 || shown.ends_with('0') {
            return Err(Reject::Spelling);
        }
        // At most six digits and the last one not a zero: scaled to
        // microseconds it is a whole number above zero, and its order is the
        // order of the fractions.
        let count = digits(shown, n).ok_or(Reject::Spelling)?;
        civil.micros = count * 10_i64.pow(u32::try_from(6 - n).unwrap_or(0));
        after = &fraction[n..];
    }
    match (key, after) {
        (Key::Timestamp, "") | (Key::TimestampTz, "+00") => Ok(civil),
        (Key::TimestampTz, other)
            if other.len() >= 3
                && matches!(other.as_bytes()[0], b'+' | b'-')
                && other[1..].bytes().all(|b| b.is_ascii_digit() || b == b':') =>
        {
            Err(Reject::Offset)
        }
        _ => Err(Reject::Spelling),
    }
}

pub(crate) fn why(key: Key, reject: &Reject) -> String {
    match reject {
        Reject::Spelling => format!(
            "which is not how the engine prints a {} bound (years 1 to 9999, as `{}`)",
            key.name(),
            key.example()
        ),
        Reject::Offset => "which is not in UTC: a timestamptz bound is written with the offset \
                           `+00`, as `2027-01-01 00:00:00+00`, since bounds in another zone \
                           are not generated"
            .to_owned(),
    }
}

// ---------------------------------------------------------------------------
// The step
// ---------------------------------------------------------------------------

/// What a step counts: days since 1970-01-01, or months since year 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Days,
    Months,
}

/// A step: whole `len` units of `mode`, and the unit that names its
/// partitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) mode: Mode,
    pub(crate) len: i64,
    pub(crate) unit: Unit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unit {
    Day,
    Week,
    Month,
    Year,
}

/// A step as `--every` writes it.
fn parse_step(every: &str) -> Result<Step, String> {
    parse_step_as("--every", every)
}

/// The same grammar for another setting that writes a step, named in the
/// refusal as `what` (`partition_runway:` reads it too, DEC-1789.2): one
/// parser, so the two cannot disagree on what a month is.
pub(crate) fn parse_step_as(what: &str, every: &str) -> Result<Step, String> {
    let text = every.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (count, unit) = text.split_at(split);
    let n: i64 = match count.parse() {
        Ok(n) if n > 0 => n,
        _ if count.is_empty() || count.bytes().all(|b| b == b'0') => {
            return Err(format!(
                "{what} `{every}` starts with no positive whole number: write a step such as \
                 `1month`, `7day`, `1week` or `1year`"
            ));
        }
        _ => {
            return Err(format!("{what} `{every}` is too large a step"));
        }
    };
    if n > MAX_STEP {
        return Err(format!(
            "{what} `{every}` is too large a step (at most {MAX_STEP})"
        ));
    }
    let unit = match unit.trim().to_ascii_lowercase().as_str() {
        "day" | "days" => Unit::Day,
        "week" | "weeks" => Unit::Week,
        "month" | "months" => Unit::Month,
        "year" | "years" => Unit::Year,
        "" => {
            return Err(format!(
                "{what} `{every}` has no unit: a date or time key steps in `day`, `week`, \
                 `month` or `year`, as `1month`"
            ));
        }
        other => {
            return Err(format!(
                "{what} `{every}`: `{other}` is not a unit; use `day`, `week`, `month` or \
                 `year`"
            ));
        }
    };
    Ok(match unit {
        Unit::Day => Step {
            mode: Mode::Days,
            len: n,
            unit,
        },
        Unit::Week => Step {
            mode: Mode::Days,
            len: 7 * n,
            unit,
        },
        Unit::Month => Step {
            mode: Mode::Months,
            len: n,
            unit,
        },
        Unit::Year => Step {
            mode: Mode::Months,
            len: 12 * n,
            unit,
        },
    })
}

/// Where a value falls on a step's axis: the unit it is in, and whether it is
/// past the start of that unit, so not on the grid at all.
struct Position {
    floor: i64,
    above: bool,
}

fn position(mode: Mode, c: &Civil) -> Position {
    let timed = c.secs > 0 || c.micros > 0;
    match mode {
        Mode::Days => Position {
            floor: days_from_civil(c.y, c.m, c.d),
            above: timed,
        },
        Mode::Months => Position {
            floor: c.y * 12 + (c.m - 1),
            above: c.d > 1 || timed,
        },
    }
}

/// The calendar date at the start of unit `floor`.
fn ymd(mode: Mode, floor: i64) -> (i64, i64, i64) {
    match mode {
        Mode::Days => civil_from_days(floor),
        Mode::Months => (floor.div_euclid(12), floor.rem_euclid(12) + 1, 1),
    }
}

/// The value at the start of unit `floor`, spelled as `key` prints it.
fn spell(key: Key, mode: Mode, floor: i64) -> String {
    let (y, m, d) = ymd(mode, floor);
    let date = format!("{y:04}-{m:02}-{d:02}");
    match key {
        Key::Date | Key::Integer(_) => date,
        Key::Timestamp => format!("{date} 00:00:00"),
        Key::TimestampTz => format!("{date} 00:00:00+00"),
    }
}

/// The last year a bound is read or written in (`parse_civil`).
const LAST_YEAR: i64 = 9999;

/// What a refusal says when the value above the nearest one lies past
/// [`LAST_YEAR`] (#1996). A proposal `--until` itself would refuse is no
/// proposal, as an integer key's past its type is none (#1905).
const NO_LATER: &str = "no later value fits the calendar, which ends in year 9999";

/// The two values nearest a request that would tile: the unit `lower` and the
/// unit above it, `lower + 1` for an off-grid frontier, `lower + len` for an
/// `--until`. `lead` is the sentence's opening for two values and for one, so
/// that its verb agrees with the count.
///
/// Only the value above can leave the calendar. `lower` is the floor of a
/// value that `parse_civil` read, or of one between two of them (the frontier
/// and `--until`, with `--until` above the frontier), so its year is in 1 to
/// 9999 and needs no guard. The value above is that plus a step, which can be
/// `MAX_STEP` years, and is left out when its year is past [`LAST_YEAR`].
fn nearest_values(key: Key, mode: Mode, lower: i64, upper: i64, lead: [&str; 2]) -> String {
    let [both, one] = lead;
    if ymd(mode, upper).0 <= LAST_YEAR {
        format!(
            "{both} `{}` and `{}`",
            spell(key, mode, lower),
            spell(key, mode, upper)
        )
    } else {
        format!("{one} `{}`; {NO_LATER}", spell(key, mode, lower))
    }
}

/// A partition's name from its lower bound: the year for a yearly step, the
/// month for a monthly one, the day for a daily or weekly one.
fn suffix(unit: Unit, mode: Mode, floor: i64) -> String {
    let (y, m, d) = ymd(mode, floor);
    match unit {
        Unit::Year => format!("{y:04}"),
        Unit::Month => format!("{y:04}_{m:02}"),
        Unit::Day | Unit::Week => format!("{y:04}_{m:02}_{d:02}"),
    }
}

// ---------------------------------------------------------------------------
// The series
// ---------------------------------------------------------------------------

/// What the declarations say about the parent: its key, and where each of its
/// declared ranges ends.
struct Declared {
    key: Key,
    /// Each range partition and its finite upper end, as written.
    ends: Vec<(TableName, String)>,
}

fn inspect(req: &RangeSeriesRequest<'_>) -> Result<Declared, DialectError> {
    let parent = req.parent;
    let table = req
        .schema
        .tables
        .get(parent)
        .ok_or_else(|| invalid(format!("`{parent}` is not declared", parent = parent.raw())))?;
    if let Some(of) = &table.partition_of {
        return Err(invalid(format!(
            "`{parent}` is a partition of `{}`, and a partition is not extended; name its parent",
            of.parent.raw(),
            parent = parent.raw()
        )));
    }
    let by = table.partition_by.as_ref().ok_or_else(|| {
        invalid(format!(
            "`{parent}` declares no `partition_by:`; a series extends a RANGE-partitioned table",
            parent = parent.raw()
        ))
    })?;
    if !by.strategy.is_range() {
        return Err(invalid(format!(
            "`{parent}` is partitioned by {}: a series extends a RANGE-partitioned table",
            by.strategy.keyword(),
            parent = parent.raw()
        )));
    }
    if by.external {
        return Err(invalid(format!(
            "`{parent}` declares `partitions: external`: its partitions are another tool's, and \
             pbps declares none of them. Extend them with that tool",
            parent = parent.raw()
        )));
    }
    let [column] = by.columns.as_slice() else {
        return Err(invalid(format!(
            "`{parent}` is partitioned by {} columns, and a series has one frontier: only a \
             single-column key is generated",
            by.columns.len(),
            parent = parent.raw()
        )));
    };
    let ty = table.columns.get(column).map(|c| &c.ty).ok_or_else(|| {
        invalid(format!(
            "`{parent}` is partitioned by `{column}`, which it does not declare",
            parent = parent.raw()
        ))
    })?;
    let key = Key::of(ty).ok_or_else(|| {
        invalid(format!(
            "`{parent}` is partitioned by `{column}`, a {ty}: a series is generated for a \
             `date`, `timestamp`, `timestamptz`, `smallint`, `integer` or `bigint` key",
            parent = parent.raw()
        ))
    })?;

    let mut ends = Vec::new();
    for (name, partition) in &req.schema.tables {
        let Some(of) = partition
            .partition_of
            .as_ref()
            .filter(|of| of.parent == *parent)
        else {
            continue;
        };
        match &of.bound {
            // A DEFAULT takes the rows no range does and has no end.
            PartitionBound::Default => {}
            // A series is of ranges (#1632).
            PartitionBound::List { .. } | PartitionBound::Hash { .. } => {
                return Err(invalid(format!(
                    "partition {name} is not a range: only a RANGE parent's partitions extend",
                    name = name.raw()
                )));
            }
            PartitionBound::Range { from, to } => {
                let ([_], [end]) = (from.as_slice(), to.as_slice()) else {
                    return Err(invalid(format!(
                        "partition {name} is bounded by {} value(s) from and {} to, and the key \
                         is one column",
                        from.len(),
                        to.len(),
                        name = name.raw()
                    )));
                };
                match end {
                    BoundDatum::MaxValue => {
                        return Err(invalid(format!(
                            "partition {name} reaches MAXVALUE, so no range can follow it; \
                             there is nothing to extend",
                            name = name.raw()
                        )));
                    }
                    BoundDatum::MinValue => {
                        return Err(invalid(format!(
                            "partition {name} ends at MINVALUE, which no range can",
                            name = name.raw()
                        )));
                    }
                    BoundDatum::Value(text) => ends.push((name.clone(), text.clone())),
                }
            }
        }
    }
    Ok(Declared { key, ends })
}

/// The partitions that extend `req.parent`'s declared ranges to `req.until`.
pub(crate) fn series(req: &RangeSeriesRequest<'_>) -> Result<RangeSeries, DialectError> {
    let declared = inspect(req)?;
    match declared.key {
        Key::Integer(int) => integers(req, &declared, int),
        Key::Date | Key::Timestamp | Key::TimestampTz => temporal(req, &declared),
    }
}

/// A name for a new partition, refused when the engine would truncate it or a
/// declaration already holds it.
fn named(req: &RangeSeriesRequest<'_>, suffix: &str) -> Result<TableName, DialectError> {
    let name = TableName::new(
        req.parent.schema.clone(),
        format!("{}_{suffix}", req.parent.name),
    );
    if name.name.len() > MAX_IDENT_BYTES {
        return Err(invalid(format!(
            "the partition name `{}` is {} bytes, and the engine keeps {MAX_IDENT_BYTES}: it \
             would truncate the name, and two such names can collide. Declare these partitions \
             by hand under shorter names",
            name.name,
            name.name.len()
        )));
    }
    if req.schema.tables.contains_key(&name) {
        return Err(invalid(format!(
            "`{name}` is already declared, and the series would name a partition so; declare \
             these partitions by hand, or rename that table",
            name = name.raw()
        )));
    }
    Ok(name)
}

fn too_many(count: i128) -> DialectError {
    invalid(format!(
        "that is {count} partitions, and one run adds at most {MAX_PER_RUN}: end --until \
         earlier, and run again from there"
    ))
}

fn temporal(
    req: &RangeSeriesRequest<'_>,
    declared: &Declared,
) -> Result<RangeSeries, DialectError> {
    let key = declared.key;
    let step = parse_step(req.every).map_err(invalid)?;
    let Step { mode, len, unit } = step;

    // Where the series starts: the highest declared end, or --from.
    let mut frontier: Option<(Civil, &str, String)> = None;
    for (name, text) in &declared.ends {
        let c = parse_civil(key, text).map_err(|r| {
            invalid(format!(
                "partition {name} ends at `{text}`, {}",
                why(key, &r),
                name = name.raw()
            ))
        })?;
        if frontier.as_ref().is_none_or(|(best, ..)| c > *best) {
            frontier = Some((
                c,
                text,
                format!("partition {name} ends at `{text}`", name = name.raw()),
            ));
        }
    }
    let until = parse_civil(key, req.until)
        .map_err(|r| invalid(format!("--until `{}`, {}", req.until, why(key, &r))))?;
    let (start, start_text, origin) = match (frontier, req.from) {
        (Some((_, end, _)), Some(from)) => {
            return Err(invalid(format!(
                "--from `{from}` is accepted only for a parent that declares no range yet; \
                 `{}` already ends at `{end}`, where the series continues",
                req.parent.raw()
            )));
        }
        (Some((c, text, origin)), None) => (c, text.to_owned(), origin),
        (None, Some(from)) => {
            let c = parse_civil(key, from)
                .map_err(|r| invalid(format!("--from `{from}`, {}", why(key, &r))))?;
            (c, from.to_owned(), format!("--from `{from}`"))
        }
        (None, None) => {
            return Err(invalid(format!(
                "`{}` declares no range to continue from: say where the series starts with \
                 --from, as `--from {}`",
                req.parent.raw(),
                key.example()
            )));
        }
    };

    if until <= start && declared.ends.is_empty() {
        return Err(invalid(format!(
            "--until `{}` is not after --from `{start_text}`",
            req.until
        )));
    }

    // The frontier is checked before the answer "nothing to add", not after
    // it: a frontier off the grid is not one a series can continue from, and
    // saying the series already reaches --until would pass it as if it were
    // (#1912). Equal to --until or past it, an on-grid frontier is still a
    // no-op, which is what makes a rerun one.
    let from = position(mode, &start);
    let unit_name = match mode {
        Mode::Days => "midnight",
        Mode::Months => "the first of a month at midnight",
    };
    if from.above {
        let step_unit = if mode == Mode::Days { "day" } else { "month" };
        let nearest = nearest_values(
            key,
            mode,
            from.floor,
            from.floor + 1,
            ["The nearest that do are", "The nearest that does is"],
        );
        return Err(invalid(format!(
            "{origin}, which is not at {unit_name}, so a series of {step_unit}s from it would \
             not tile. {nearest}"
        )));
    }
    if until <= start {
        return Ok(RangeSeries {
            start: start_text,
            partitions: Vec::new(),
        });
    }
    let to = position(mode, &until);
    let diff = to.floor - from.floor;
    let lower = from.floor + diff.div_euclid(len) * len;
    if to.above || lower != to.floor {
        let nearest = nearest_values(
            key,
            mode,
            lower,
            lower + len,
            ["The nearest that tile are", "The nearest that tiles is"],
        );
        return Err(invalid(format!(
            "--until `{}` is not a whole number of `{}` steps after `{start_text}`, so the last \
             partition would end short of it or past it. {nearest}",
            req.until,
            req.every.trim()
        )));
    }
    let count = i128::from(diff / len);
    if count > MAX_PER_RUN {
        return Err(too_many(count));
    }

    let mut partitions = Vec::new();
    for i in 0..diff / len {
        let lo = from.floor + i * len;
        let hi = lo + len;
        partitions.push(SeriesPartition {
            name: named(req, &suffix(unit, mode, lo))?,
            from: spell(key, mode, lo),
            to: spell(key, mode, hi),
        });
    }
    Ok(RangeSeries {
        start: start_text,
        partitions,
    })
}

/// An integer as the engine prints it: no sign but `-`, no leading zero.
pub(crate) fn parse_integer(int: Int, text: &str) -> Result<i128, String> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let canonical = !digits.is_empty()
        && digits.len() <= 20
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
        && text != "-0";
    let value: i128 = text.parse().ok().filter(|_| canonical).ok_or_else(|| {
        format!(
            "which is not how the engine prints a {} bound, as `1000`",
            int.name
        )
    })?;
    if !(int.min..=int.max).contains(&value) {
        return Err(format!(
            "which is outside the {} range {} to {}",
            int.name, int.min, int.max
        ));
    }
    Ok(value)
}

fn integers(
    req: &RangeSeriesRequest<'_>,
    declared: &Declared,
    int: Int,
) -> Result<RangeSeries, DialectError> {
    let every = req.every.trim();
    let step: i128 = every
        .parse::<i128>()
        .ok()
        .filter(|n| *n > 0 && every.len() <= 20 && every.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| {
            invalid(format!(
                "--every `{every}`: a {} key steps by a plain positive whole number, as \
                 `--every 1000`",
                int.name
            ))
        })?;

    let mut frontier: Option<(i128, String)> = None;
    for (name, text) in &declared.ends {
        let v = parse_integer(int, text).map_err(|why| {
            invalid(format!(
                "partition {name} ends at `{text}`, {why}",
                name = name.raw()
            ))
        })?;
        if frontier.as_ref().is_none_or(|(best, _)| v > *best) {
            frontier = Some((
                v,
                format!("partition {name} ends at `{text}`", name = name.raw()),
            ));
        }
    }
    let until = parse_integer(int, req.until)
        .map_err(|why| invalid(format!("--until `{}`, {why}", req.until)))?;
    let (start, _origin) = match (frontier, req.from) {
        (Some((v, _)), Some(from)) => {
            return Err(invalid(format!(
                "--from `{from}` is accepted only for a parent that declares no range yet; \
                 `{}` already ends at `{v}`, where the series continues",
                req.parent.raw()
            )));
        }
        (Some(found), None) => found,
        (None, Some(from)) => {
            let v = parse_integer(int, from)
                .map_err(|why| invalid(format!("--from `{from}`, {why}")))?;
            (v, format!("--from `{from}`"))
        }
        (None, None) => {
            return Err(invalid(format!(
                "`{}` declares no range to continue from: say where the series starts with \
                 --from, as `--from 0`",
                req.parent.raw()
            )));
        }
    };

    if until <= start {
        if declared.ends.is_empty() {
            return Err(invalid(format!(
                "--until `{}` is not after --from `{start}`",
                req.until
            )));
        }
        return Ok(RangeSeries {
            start: start.to_string(),
            partitions: Vec::new(),
        });
    }
    let diff = until - start;
    if diff % step != 0 {
        // `until > start`, so `diff` is positive and `lower` lies in
        // `[start, until)`, inside the key type whatever the step is. Only
        // the value above can leave it, and a value the column cannot hold
        // is no value to propose: it is left out and the refusal says so
        // (#1905).
        let lower = start + diff / step * step;
        let upper = lower + step;
        let nearest = if upper <= int.max {
            format!("The nearest that tile are `{lower}` and `{upper}`")
        } else {
            format!(
                "The nearest that tiles is `{lower}`; no larger value fits a {}",
                int.name
            )
        };
        return Err(invalid(format!(
            "--until `{until}` is not a whole number of steps of {step} after `{start}`, so the \
             last partition would end short of it or past it. {nearest}"
        )));
    }
    let count = diff / step;
    if count > MAX_PER_RUN {
        return Err(too_many(count));
    }

    let mut partitions = Vec::new();
    for i in 0..count {
        let lo = start + i * step;
        let hi = lo + step;
        if lo < 0 {
            return Err(invalid(format!(
                "the partition from `{lo}` would be named `{}_{lo}`, which is not an \
                 identifier; declare a series that starts at 0 or above, or declare these \
                 partitions by hand",
                req.parent.name
            )));
        }
        partitions.push(SeriesPartition {
            name: named(req, &lo.to_string())?,
            from: lo.to_string(),
            to: hi.to_string(),
        });
    }
    Ok(RangeSeries {
        start: start.to_string(),
        partitions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, PartitionBy, PartitionOf, Schema, Table};

    fn parent() -> TableName {
        "app.ev".parse().unwrap()
    }

    /// A parent keyed on `ts` of this type, with these ranges and, when
    /// asked, a DEFAULT.
    fn schema(ty: &str, ranges: &[(&str, &str, &str)]) -> Schema {
        let mut table = Table::default();
        table
            .columns
            .insert("ts".into(), Column::new(ty.parse().unwrap()));
        table.partition_by = Some(PartitionBy {
            columns: vec!["ts".into()],
            external: false,
            strategy: Default::default(),
        });
        let mut schema = Schema::default();
        schema.tables.insert(parent(), table);
        for (name, from, to) in ranges {
            add(&mut schema, name, bound(from, to));
        }
        schema
    }

    fn bound(from: &str, to: &str) -> PartitionBound {
        PartitionBound::Range {
            from: vec![BoundDatum::declared(from)],
            to: vec![BoundDatum::declared(to)],
        }
    }

    fn add(schema: &mut Schema, name: &str, bound: PartitionBound) {
        schema.tables.insert(
            TableName::new("app", name),
            Table {
                partition_of: Some(PartitionOf {
                    parent: parent(),
                    bound,
                    columns: Default::default(),
                }),
                ..Default::default()
            },
        );
    }

    fn ask(
        schema: &Schema,
        every: &str,
        until: &str,
        from: Option<&str>,
    ) -> Result<RangeSeries, DialectError> {
        let parent = parent();
        series(&RangeSeriesRequest {
            schema,
            parent: &parent,
            every,
            until,
            from,
        })
    }

    fn bounds(s: &RangeSeries) -> Vec<(String, String, String)> {
        s.partitions
            .iter()
            .map(|p| (p.name.name.clone(), p.from.clone(), p.to.clone()))
            .collect()
    }

    fn refusal(r: Result<RangeSeries, DialectError>) -> String {
        match r {
            Err(DialectError::Invalid { message, .. }) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A monthly series carries on across a year end and keeps the names the
    /// months have: `ev_2026_12` ends where `ev_2027_01` starts.
    #[test]
    fn monthly_series_runs_across_a_year_end() {
        let s = schema("date", &[("ev_2026_11", "2026-11-01", "2026-12-01")]);
        let got = ask(&s, "1month", "2027-03-01", None).unwrap();
        assert_eq!(got.start, "2026-12-01");
        assert_eq!(
            bounds(&got),
            vec![
                (
                    "ev_2026_12".into(),
                    "2026-12-01".into(),
                    "2027-01-01".into()
                ),
                (
                    "ev_2027_01".into(),
                    "2027-01-01".into(),
                    "2027-02-01".into()
                ),
                (
                    "ev_2027_02".into(),
                    "2027-02-01".into(),
                    "2027-03-01".into()
                ),
            ]
        );
    }

    /// February has 29 days in 2028 and the series does not care: a month is
    /// counted on the calendar from the start, not as a number of days, and a
    /// daily series counts the 29th.
    #[test]
    fn a_leap_february_is_a_month_and_29_days() {
        let s = schema("date", &[("ev_2028_01", "2028-01-01", "2028-02-01")]);
        let months = ask(&s, "1month", "2028-04-01", None).unwrap();
        assert_eq!(
            bounds(&months)
                .iter()
                .map(|(_, f, t)| format!("{f}..{t}"))
                .collect::<Vec<_>>(),
            vec!["2028-02-01..2028-03-01", "2028-03-01..2028-04-01"]
        );
        let s = schema("date", &[("ev_x", "2028-02-27", "2028-02-28")]);
        let days = ask(&s, "1day", "2028-03-02", None).unwrap();
        assert_eq!(
            bounds(&days)
                .iter()
                .map(|(n, f, t)| format!("{n} {f}..{t}"))
                .collect::<Vec<_>>(),
            vec![
                "ev_2028_02_28 2028-02-28..2028-02-29",
                "ev_2028_02_29 2028-02-29..2028-03-01",
                "ev_2028_03_01 2028-03-01..2028-03-02",
            ]
        );
        // And a non-leap year has no 29th.
        let s = schema("date", &[("ev_x", "2027-02-27", "2027-02-28")]);
        let days = ask(&s, "1day", "2027-03-02", None).unwrap();
        assert_eq!(days.partitions[1].from, "2027-03-01");
    }

    #[test]
    fn weekly_and_multi_unit_steps_count_from_the_start() {
        let s = schema("date", &[("ev_x", "2026-12-18", "2026-12-25")]);
        let weeks = ask(&s, "1week", "2027-01-08", None).unwrap();
        assert_eq!(
            bounds(&weeks)
                .iter()
                .map(|(n, f, t)| format!("{n} {f}..{t}"))
                .collect::<Vec<_>>(),
            vec![
                "ev_2026_12_25 2026-12-25..2027-01-01",
                "ev_2027_01_01 2027-01-01..2027-01-08",
            ]
        );
        // Three months at a time, named by the month each starts.
        let s = schema("date", &[("ev_x", "2026-01-01", "2026-04-01")]);
        let quarters = ask(&s, "3month", "2027-01-01", None).unwrap();
        assert_eq!(
            quarters
                .partitions
                .iter()
                .map(|p| p.name.name.as_str())
                .collect::<Vec<_>>(),
            ["ev_2026_04", "ev_2026_07", "ev_2026_10"]
        );
        // The plural is accepted, and so is a space before the unit.
        assert_eq!(ask(&s, "3 months", "2027-01-01", None).unwrap(), quarters);
    }

    /// A yearly series is named by the year it starts, and may start in any
    /// month: a fiscal year from July is `ev_2026` for July 2026 to July 2027.
    #[test]
    fn yearly_series_start_in_any_month() {
        let s = schema("date", &[("ev_2025", "2025-01-01", "2026-01-01")]);
        let years = ask(&s, "1year", "2028-01-01", None).unwrap();
        assert_eq!(
            bounds(&years),
            vec![
                ("ev_2026".into(), "2026-01-01".into(), "2027-01-01".into()),
                ("ev_2027".into(), "2027-01-01".into(), "2028-01-01".into()),
            ]
        );
        let fiscal = schema("date", &[("ev_2025", "2025-07-01", "2026-07-01")]);
        let years = ask(&fiscal, "1year", "2027-07-01", None).unwrap();
        assert_eq!(
            bounds(&years),
            vec![("ev_2026".into(), "2026-07-01".into(), "2027-07-01".into())]
        );
    }

    /// The bound is spelled as the engine prints the key's type, which is
    /// what a connected plan insists on.
    #[test]
    fn each_key_type_gets_the_spelling_the_engine_prints() {
        let s = schema(
            "timestamp",
            &[("ev_2026_12", "2026-12-01 00:00:00", "2027-01-01 00:00:00")],
        );
        let got = ask(&s, "1month", "2027-02-01 00:00:00", None).unwrap();
        assert_eq!(
            bounds(&got),
            vec![(
                "ev_2027_01".into(),
                "2027-01-01 00:00:00".into(),
                "2027-02-01 00:00:00".into()
            )]
        );
        let s = schema(
            "timestamptz",
            &[(
                "ev_2026_12",
                "2026-12-01 00:00:00+00",
                "2027-01-01 00:00:00+00",
            )],
        );
        let got = ask(&s, "1month", "2027-02-01 00:00:00+00", None).unwrap();
        assert_eq!(
            bounds(&got),
            vec![(
                "ev_2027_01".into(),
                "2027-01-01 00:00:00+00".into(),
                "2027-02-01 00:00:00+00".into()
            )]
        );
        // `timestamp with time zone` and `timestamptz` are one type.
        let spelled = schema("timestamp with time zone", &[]);
        assert!(
            ask(
                &spelled,
                "1day",
                "2027-01-02 00:00:00+00",
                Some("2027-01-01 00:00:00+00")
            )
            .is_ok()
        );
        // A precision does not change the spelling of a whole second.
        let spelled = schema("timestamp(3)", &[]);
        let got = ask(
            &spelled,
            "1day",
            "2027-01-02 00:00:00",
            Some("2027-01-01 00:00:00"),
        )
        .unwrap();
        assert_eq!(got.partitions[0].to, "2027-01-02 00:00:00");
    }

    #[test]
    fn an_integer_key_steps_by_a_count_and_is_named_by_its_lower_bound() {
        for ty in ["bigint", "int8", "integer", "int", "smallint"] {
            let s = schema(ty, &[("ev_0", "0", "1000")]);
            let got = ask(&s, "1000", "3000", None).unwrap();
            assert_eq!(
                bounds(&got),
                vec![
                    ("ev_1000".into(), "1000".into(), "2000".into()),
                    ("ev_2000".into(), "2000".into(), "3000".into()),
                ],
                "{ty}"
            );
        }
        // From nothing, with --from.
        let s = schema("bigint", &[]);
        let got = ask(&s, "5", "15", Some("0")).unwrap();
        assert_eq!(got.start, "0");
        assert_eq!(got.partitions.len(), 3);
    }

    /// The series ends where it was told to: a full key value, exclusive. A
    /// run that finds the declarations already there adds nothing and says
    /// so, which is what makes a rerun a no-op.
    #[test]
    fn a_frontier_at_or_past_until_adds_nothing() {
        let s = schema("date", &[("ev_2027_01", "2027-01-01", "2027-02-01")]);
        for until in ["2027-02-01", "2027-01-15", "2026-01-01"] {
            let got = ask(&s, "1month", until, None).unwrap();
            assert!(got.partitions.is_empty(), "{until}");
            assert_eq!(got.start, "2027-02-01");
        }
        // The highest end is the frontier whatever order the names sort in.
        let s = schema(
            "date",
            &[
                ("ev_b", "2027-01-01", "2027-02-01"),
                ("ev_a", "2026-01-01", "2027-01-01"),
            ],
        );
        assert_eq!(
            ask(&s, "1month", "2027-03-01", None).unwrap().start,
            "2027-02-01"
        );
        // A range from MINVALUE is a range like any other.
        let mut s = schema("date", &[]);
        add(
            &mut s,
            "ev_old",
            PartitionBound::Range {
                from: vec![BoundDatum::MinValue],
                to: vec![BoundDatum::Value("2026-01-01".into())],
            },
        );
        assert_eq!(
            ask(&s, "1year", "2027-01-01", None)
                .unwrap()
                .partitions
                .len(),
            1
        );
    }

    #[test]
    fn a_frontier_off_the_grid_is_refused_with_the_nearest_values() {
        let s = schema("date", &[("ev_x", "2026-01-01", "2026-01-15")]);
        let said = refusal(ask(&s, "1month", "2026-03-01", None));
        assert!(
            said.contains("partition app.ev_x ends at `2026-01-15`")
                && said.contains("`2026-01-01` and `2026-02-01`"),
            "{said}"
        );
        // A day grid is midnight, and a timestamp at noon is not on it.
        let s = schema(
            "timestamp",
            &[("ev_x", "2026-01-01 00:00:00", "2026-01-02 12:00:00")],
        );
        let said = refusal(ask(&s, "1day", "2026-01-05 00:00:00", None));
        assert!(
            said.contains("not at midnight")
                && said.contains("`2026-01-02 00:00:00` and `2026-01-03 00:00:00`"),
            "{said}"
        );
        // A fraction of a second is off the grid too.
        let s = schema(
            "timestamp",
            &[("ev_x", "2026-01-01 00:00:00", "2026-01-02 00:00:00.5")],
        );
        assert!(refusal(ask(&s, "1day", "2026-01-05 00:00:00", None)).contains("not at midnight"));
    }

    /// A fraction is a magnitude: bounds that differ only inside one second
    /// are two bounds, so the frontier below --until is not "already there"
    /// and the off-grid frontier is refused (#1912).
    #[test]
    fn a_frontier_differing_from_until_only_in_its_fraction_is_refused_as_off_the_grid() {
        for ty in ["timestamp", "timestamptz"] {
            let tz = if ty == "timestamptz" { "+00" } else { "" };
            let s = schema(
                ty,
                &[(
                    "ev_x",
                    &format!("2026-01-01 00:00:00{tz}"),
                    &format!("2026-01-02 00:00:00.1{tz}"),
                )],
            );
            let said = refusal(ask(&s, "1day", &format!("2026-01-02 00:00:00.9{tz}"), None));
            assert!(
                said.contains("partition app.ev_x ends at")
                    && said.contains("not at midnight")
                    && said.contains(&format!("`2026-01-02 00:00:00{tz}` and")),
                "{ty}: {said}"
            );
        }
    }

    /// The off-grid refusal comes before "nothing to add": a frontier that is
    /// off the grid is refused whether --until is above it, equal to it or
    /// below it, and only a frontier on the grid is a no-op rerun.
    #[test]
    fn an_off_grid_frontier_is_refused_before_nothing_to_add() {
        let off = schema(
            "timestamp",
            &[("ev_x", "2026-01-01 00:00:00", "2026-01-02 00:00:00.5")],
        );
        for until in [
            "2026-01-02 00:00:00.5", // equal fraction on both sides
            "2026-01-02 00:00:00.1", // below the frontier, inside the second
            "2026-01-01 00:00:00",   // below it by a day
            "2026-01-05 00:00:00",   // above it
        ] {
            let said = refusal(ask(&off, "1day", until, None));
            assert!(
                said.contains("not at midnight")
                    && said.contains("`2026-01-02 00:00:00` and `2026-01-03 00:00:00`"),
                "{until}: {said}"
            );
        }
        // A frontier past --until by a fraction of a second, on a month grid.
        let off = schema(
            "timestamp",
            &[("ev_x", "2026-01-01 00:00:00", "2026-02-01 00:00:00.000001")],
        );
        assert!(
            refusal(ask(&off, "1month", "2026-02-01 00:00:00", None))
                .contains("the first of a month at midnight")
        );
        // On the grid, the same asks are the no-op they always were, with
        // --until equal to the frontier or short of it by less than a second.
        let on = schema(
            "timestamp",
            &[("ev_x", "2026-01-01 00:00:00", "2026-01-02 00:00:00")],
        );
        for until in ["2026-01-02 00:00:00", "2026-01-01 23:59:59.9"] {
            let got = ask(&on, "1day", until, None).unwrap();
            assert!(got.partitions.is_empty(), "{until}");
            assert_eq!(got.start, "2026-01-02 00:00:00");
        }
        // And an --until a fraction past an on-grid frontier does not tile.
        let said = refusal(ask(&on, "1day", "2026-01-02 00:00:00.1", None));
        assert!(
            said.contains("is not a whole number of `1day` steps"),
            "{said}"
        );
    }

    /// A date frontier has no time of day to carry a fraction; its refusal
    /// still precedes "nothing to add".
    #[test]
    fn an_off_grid_date_frontier_below_until_is_refused_not_passed() {
        let s = schema("date", &[("ev_x", "2026-01-01", "2026-01-15")]);
        let said = refusal(ask(&s, "1month", "2026-01-10", None));
        assert!(said.contains("`2026-01-01` and `2026-02-01`"), "{said}");
    }

    #[test]
    fn a_fraction_orders_by_magnitude_not_by_presence() {
        let t = |text: &str| parse_civil(Key::Timestamp, text).ok().unwrap();
        let at = |frac: &str| t(&format!("2026-01-02 00:00:00{frac}"));
        // Digits, not a flag: .000001 < .1 < .12 < .5 < .9, and a whole
        // second is below every fraction of it.
        let order = [
            "", ".000001", ".00001", ".1", ".12", ".123456", ".5", ".9", ".999999",
        ];
        for pair in order.windows(2) {
            assert!(at(pair[0]) < at(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(at(".5").micros, 500_000);
        assert_eq!(at(".000001").micros, 1);
        assert_eq!(at(".123456").micros, 123_456);
        // A fraction does not outrank the second it is in.
        assert!(at(".999999") < t("2026-01-02 00:00:01"));
        // Negative: the spellings the engine does not print stay refused.
        for bad in [
            "2026-01-02 00:00:00.", // no digits
            "2026-01-02 00:00:00.0",
            "2026-01-02 00:00:00.10",      // trailing zero
            "2026-01-02 00:00:00.1234567", // beyond microseconds
        ] {
            assert!(parse_civil(Key::Timestamp, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_until_that_does_not_tile_is_refused_with_the_nearest_values() {
        let s = schema("date", &[("ev_x", "2026-12-01", "2027-01-01")]);
        // Not the first of a month.
        let said = refusal(ask(&s, "1month", "2027-03-15", None));
        assert!(said.contains("`2027-03-01` and `2027-04-01`"), "{said}");
        // Whole months, but not whole quarters.
        let said = refusal(ask(&s, "3month", "2027-06-01", None));
        assert!(said.contains("`2027-04-01` and `2027-07-01`"), "{said}");
        // Inside the first step.
        let said = refusal(ask(&s, "1month", "2027-01-20", None));
        assert!(said.contains("`2027-01-01` and `2027-02-01`"), "{said}");
        // A week that stops short.
        let s = schema("date", &[("ev_x", "2026-12-18", "2026-12-25")]);
        let said = refusal(ask(&s, "1week", "2027-01-02", None));
        assert!(said.contains("`2027-01-01` and `2027-01-08`"), "{said}");
        // Integers.
        let s = schema("bigint", &[("ev_0", "0", "1000")]);
        let said = refusal(ask(&s, "1000", "2500", None));
        assert!(said.contains("`2000` and `3000`"), "{said}");
    }

    /// Every number in `text`.
    fn numbers(text: &str) -> Vec<u128> {
        text.split(|c: char| !c.is_ascii_digit())
            .filter(|run| !run.is_empty())
            .map(|run| run.parse().unwrap())
            .collect()
    }

    /// The part of an integer refusal that proposes values: what follows
    /// "The nearest".
    fn proposed(said: &str) -> &str {
        said.split_once("The nearest")
            .unwrap_or_else(|| panic!("no proposal in {said}"))
            .1
    }

    /// The value above is left out when the key cannot hold it (#1905), for
    /// every integer type by its own maximum. A 20-digit step is the widest
    /// `--every` accepts, so it is the one that reaches past a `bigint`.
    #[test]
    fn a_nearest_value_beyond_the_key_type_is_left_out() {
        for (ty, max, step, until) in [
            (
                "bigint",
                i128::from(i64::MAX),
                "99999999999999999999",
                "9223372036854775807",
            ),
            (
                "bigint",
                i128::from(i64::MAX),
                "9223372036854774808",
                "2000",
            ),
            ("integer", i128::from(i32::MAX), "3000000000", "2000"),
            ("integer", i128::from(i32::MAX), "2147482648", "2000"),
            ("smallint", i128::from(i16::MAX), "40000", "2000"),
            ("smallint", i128::from(i16::MAX), "31768", "2000"),
        ] {
            let s = schema(ty, &[("ev_0", "0", "1000")]);
            let said = refusal(ask(&s, step, until, None));
            assert!(
                said.contains(&format!(
                    "The nearest that tiles is `1000`; no larger value fits a {ty}"
                )),
                "{ty} {step}: {said}"
            );
            assert!(
                numbers(proposed(&said))
                    .iter()
                    .all(|n| *n <= max.unsigned_abs()),
                "{ty} {step}: {said}"
            );
        }
    }

    /// A value exactly at the type's maximum fits, so it is still named, and
    /// an in-range pair reads as it always did.
    #[test]
    fn a_nearest_value_at_the_key_types_maximum_is_still_named() {
        for (ty, step, top) in [
            ("bigint", "9223372036854774807", "9223372036854775807"),
            ("integer", "2147482647", "2147483647"),
            ("smallint", "31767", "32767"),
        ] {
            let s = schema(ty, &[("ev_0", "0", "1000")]);
            let said = refusal(ask(&s, step, "2000", None));
            assert!(
                said.ends_with(&format!("The nearest that tile are `1000` and `{top}`")),
                "{ty}: {said}"
            );
        }
    }

    /// The value below cannot leave the type: it lies between the frontier
    /// and `--until`, both of which were read as the type's own values.
    #[test]
    fn a_nearest_value_below_is_always_inside_the_key_type() {
        let s = schema("smallint", &[("ev_x", "-40", "-32768")]);
        let said = refusal(ask(&s, "70000", "-1", None));
        assert!(
            said.ends_with("The nearest that tiles is `-32768`; no larger value fits a smallint"),
            "{said}"
        );
    }

    /// The value above is left out when it names a year past 9999 (#1996), at
    /// both refusal sites and for every temporal key: the `--until` that does
    /// not tile (`lower + len`) and the off-grid frontier (`from.floor + 1`).
    /// A value `--until` itself would refuse is no proposal.
    #[test]
    fn a_nearest_value_past_year_9999_is_left_out() {
        for (ty, tz) in [
            ("date", None),
            ("timestamp", Some("")),
            ("timestamptz", Some("+00")),
        ] {
            let at = |date: &str| match tz {
                None => date.to_owned(),
                Some(tz) => format!("{date} 00:00:00{tz}"),
            };
            // (frontier, every, until, the one value named, its verb)
            let mut cases = vec![
                // --until: lower + len is 10000-01-01, 10000-01-01, 10004-01-01
                // and 10000-01-06.
                (
                    "9999-01-01",
                    "1year",
                    "9999-06-15",
                    "9999-01-01",
                    "tiles is",
                ),
                (
                    "9999-12-01",
                    "1month",
                    "9999-12-15",
                    "9999-12-01",
                    "tiles is",
                ),
                (
                    "9999-01-01",
                    "5year",
                    "9999-12-01",
                    "9999-01-01",
                    "tiles is",
                ),
                ("9999-12-30", "7day", "9999-12-31", "9999-12-30", "tiles is"),
                // The off-grid frontier: floor + 1 is 10000-01-01.
                (
                    "9999-12-15",
                    "1month",
                    "9999-12-31",
                    "9999-12-01",
                    "does is",
                ),
            ];
            let noon = tz.map(|tz| format!("9999-12-31 12:00:00{tz}"));
            if let Some(noon) = &noon {
                cases.push((noon, "1day", noon, "9999-12-31", "does is"));
            }
            for (end, every, until, lower, verb) in cases {
                let end = if end.len() > 10 {
                    end.to_owned()
                } else {
                    at(end)
                };
                let until = if until.len() > 10 {
                    until.to_owned()
                } else {
                    at(until)
                };
                let lower = at(lower);
                let s = schema(ty, &[("ev_x", &at("0001-01-01"), &end)]);
                let said = refusal(ask(&s, every, &until, None));
                assert!(
                    said.ends_with(&format!(
                        "The nearest that {verb} `{lower}`; no later value fits the calendar, \
                         which ends in year 9999"
                    )),
                    "{ty} {end} {every}: {said}"
                );
                assert!(
                    numbers(proposed(&said)).iter().all(|n| *n <= 9999),
                    "{ty} {end} {every}: {said}"
                );
            }
        }
    }

    /// A value in year 9999 fits, so it is still named, up to the last day
    /// the calendar has; the first value past it is the one left out.
    #[test]
    fn a_nearest_value_in_year_9999_is_still_named() {
        for (ty, end, every, until, pair) in [
            // --until: lower + len is 9999-01-01, 9999-12-01 and 9999-12-31.
            (
                "date",
                "9998-01-01",
                "1year",
                "9998-06-15",
                "`9998-01-01` and `9999-01-01`",
            ),
            (
                "date",
                "9999-11-01",
                "1month",
                "9999-11-15",
                "`9999-11-01` and `9999-12-01`",
            ),
            (
                "timestamp",
                "9999-12-30 00:00:00",
                "1day",
                "9999-12-30 12:00:00",
                "`9999-12-30 00:00:00` and `9999-12-31 00:00:00`",
            ),
            (
                "timestamptz",
                "9999-12-30 00:00:00+00",
                "1day",
                "9999-12-30 12:00:00+00",
                "`9999-12-30 00:00:00+00` and `9999-12-31 00:00:00+00`",
            ),
            // The off-grid frontier: floor + 1 is 9999-12-01 and 9999-12-31.
            (
                "date",
                "9999-11-15",
                "1month",
                "9999-12-01",
                "`9999-11-01` and `9999-12-01`",
            ),
            (
                "timestamp",
                "9999-12-30 12:00:00",
                "1day",
                "9999-12-31 00:00:00",
                "`9999-12-30 00:00:00` and `9999-12-31 00:00:00`",
            ),
        ] {
            let s = schema(ty, &[("ev_x", "0001-01-01", end)]);
            let said = refusal(ask(&s, every, until, None));
            assert!(
                said.ends_with(pair) && !said.contains("no later value"),
                "{ty} {end}: {said}"
            );
        }
    }

    /// The value below cannot leave the calendar the other way: it is the
    /// floor of the frontier or of a value between the frontier and
    /// `--until`, so a start in year 1 and a step of `MAX_STEP` years still
    /// names year 1 (#1996).
    #[test]
    fn a_nearest_value_below_is_always_inside_the_calendar() {
        let s = schema("date", &[]);
        let said = refusal(ask(&s, "100000year", "0001-06-15", Some("0001-01-01")));
        assert!(
            said.ends_with(
                "The nearest that tiles is `0001-01-01`; no later value fits the calendar, which \
                 ends in year 9999"
            ),
            "{said}"
        );
        let said = refusal(ask(&s, "1month", "0001-03-01", Some("0001-01-15")));
        assert!(said.ends_with("`0001-01-01` and `0001-02-01`"), "{said}");
        let s = schema("timestamp", &[]);
        let said = refusal(ask(
            &s,
            "100000day",
            "0001-01-01 00:00:01",
            Some("0001-01-01 00:00:00"),
        ));
        assert!(
            said.ends_with("`0001-01-01 00:00:00` and `0274-10-17 00:00:00`"),
            "{said}"
        );
    }

    #[test]
    fn a_range_that_reaches_maxvalue_leaves_nothing_to_extend() {
        let mut s = schema("date", &[("ev_a", "2026-01-01", "2027-01-01")]);
        add(
            &mut s,
            "ev_rest",
            PartitionBound::Range {
                from: vec![BoundDatum::Value("2027-01-01".into())],
                to: vec![BoundDatum::MaxValue],
            },
        );
        let said = refusal(ask(&s, "1month", "2028-01-01", None));
        assert!(said.contains("ev_rest reaches MAXVALUE"), "{said}");
    }

    #[test]
    fn a_key_that_has_no_step_is_refused() {
        for ty in [
            "text",
            "numeric(10,2)",
            "uuid",
            "varchar(20)",
            "integer[]",
            "time",
        ] {
            let s = schema(ty, &[]);
            let said = refusal(ask(&s, "1", "2", Some("1")));
            assert!(said.contains("`date`, `timestamp`"), "{ty}: {said}");
        }
        // Several columns have no single frontier.
        let mut s = schema("date", &[]);
        let parent_table = s.tables.get_mut(&parent()).unwrap();
        parent_table
            .columns
            .insert("tenant".into(), Column::new("integer".parse().unwrap()));
        parent_table.partition_by = Some(PartitionBy {
            columns: vec!["tenant".into(), "ts".into()],
            external: false,
            strategy: Default::default(),
        });
        let said = refusal(ask(&s, "1month", "2027-01-01", Some("2026-01-01")));
        assert!(said.contains("2 columns"), "{said}");
        // A key column that is not declared.
        let mut s = schema("date", &[]);
        s.tables.get_mut(&parent()).unwrap().partition_by = Some(PartitionBy {
            columns: vec!["nope".into()],
            external: false,
            strategy: Default::default(),
        });
        assert!(refusal(ask(&s, "1month", "2027-01-01", Some("2026-01-01"))).contains("`nope`"));
    }

    #[test]
    fn a_parent_that_is_not_partitioned_or_is_external_or_is_a_partition_is_refused() {
        let mut s = schema("date", &[("ev_a", "2026-01-01", "2026-02-01")]);
        let table = s.tables.get_mut(&parent()).unwrap();
        table.partition_by.as_mut().unwrap().external = true;
        let said = refusal(ask(&s, "1month", "2026-03-01", None));
        assert!(said.contains("partitions: external"), "{said}");
        // A LIST or HASH parent has no range to extend (#1632).
        for strategy in [
            pbps_model::PartitionStrategy::List,
            pbps_model::PartitionStrategy::Hash,
        ] {
            let mut other = schema("date", &[]);
            other
                .tables
                .get_mut(&parent())
                .unwrap()
                .partition_by
                .as_mut()
                .unwrap()
                .strategy = strategy;
            let said = refusal(ask(&other, "1month", "2026-03-01", None));
            assert!(
                said.contains("a series extends a RANGE-partitioned"),
                "{said}"
            );
        }
        s.tables.get_mut(&parent()).unwrap().partition_by = None;
        let said = refusal(ask(&s, "1month", "2026-03-01", None));
        assert!(said.contains("declares no `partition_by:`"), "{said}");
        let said = refusal(ask(&schema("date", &[]), "1month", "2026-03-01", None));
        assert!(said.contains("no range to continue from"), "{said}");
        // A partition is not extended.
        let s = schema("date", &[("ev_a", "2026-01-01", "2026-02-01")]);
        let req_parent: TableName = "app.ev_a".parse().unwrap();
        let said = refusal(series(&RangeSeriesRequest {
            schema: &s,
            parent: &req_parent,
            every: "1month",
            until: "2026-03-01",
            from: None,
        }));
        assert!(said.contains("is a partition of `app.ev`"), "{said}");
        // And a table that is not declared at all.
        let missing: TableName = "app.nope".parse().unwrap();
        let said = refusal(series(&RangeSeriesRequest {
            schema: &s,
            parent: &missing,
            every: "1month",
            until: "2026-03-01",
            from: None,
        }));
        assert!(said.contains("is not declared"), "{said}");
    }

    #[test]
    fn from_is_for_a_parent_with_no_range_and_only_then() {
        let s = schema("date", &[("ev_a", "2026-01-01", "2026-02-01")]);
        let said = refusal(ask(&s, "1month", "2026-04-01", Some("2026-02-01")));
        assert!(
            said.contains(
                "--from `2026-02-01` is accepted only for a parent that declares no range"
            ) && said.contains("already ends at `2026-02-01`"),
            "{said}"
        );
        // With none declared, it is the start, and a DEFAULT alone declares
        // no range.
        let mut s = schema("date", &[]);
        add(&mut s, "ev_rest", PartitionBound::Default);
        let got = ask(&s, "1month", "2026-03-01", Some("2026-01-01")).unwrap();
        assert_eq!(got.start, "2026-01-01");
        assert_eq!(got.partitions.len(), 2);
        // A start off the grid, and an end before it.
        let said = refusal(ask(&s, "1month", "2026-03-01", Some("2026-01-10")));
        assert!(
            said.contains("--from `2026-01-10`") && said.contains("`2026-01-01` and `2026-02-01`"),
            "{said}"
        );
        let said = refusal(ask(&s, "1month", "2026-01-01", Some("2026-02-01")));
        assert!(said.contains("is not after --from"), "{said}");
        let said = refusal(ask(&s, "1month", "2026-01-01", Some("2026-01-01")));
        assert!(said.contains("is not after --from"), "{said}");
    }

    #[test]
    fn a_name_that_is_taken_or_too_long_is_refused() {
        let mut s = schema("date", &[("ev_2026_12", "2026-12-01", "2027-01-01")]);
        // Another table, not a partition of this parent, already has the name.
        s.tables
            .insert("app.ev_2027_01".parse().unwrap(), Table::default());
        let said = refusal(ask(&s, "1month", "2027-02-01", None));
        assert!(
            said.contains("`app.ev_2027_01` is already declared"),
            "{said}"
        );
        // The engine keeps 63 bytes of a name, and a name over them would
        // truncate and could collide.
        let long = "x".repeat(60);
        let mut s = Schema::default();
        let parent_name = TableName::new("app", long.clone());
        let mut table = Table::default();
        table
            .columns
            .insert("ts".into(), Column::new("date".parse().unwrap()));
        table.partition_by = Some(PartitionBy {
            columns: vec!["ts".into()],
            external: false,
            strategy: Default::default(),
        });
        s.tables.insert(parent_name.clone(), table);
        let said = refusal(series(&RangeSeriesRequest {
            schema: &s,
            parent: &parent_name,
            every: "1month",
            until: "2026-02-01",
            from: Some("2026-01-01"),
        }));
        assert!(
            said.contains("is 68 bytes") && said.contains("63"),
            "{said}"
        );
        // Sixty-three bytes exactly is a name.
        let fits = TableName::new("app", "x".repeat(55));
        let mut s2 = Schema::default();
        s2.tables
            .insert(fits.clone(), s.tables[&parent_name].clone());
        let got = series(&RangeSeriesRequest {
            schema: &s2,
            parent: &fits,
            every: "1month",
            until: "2026-02-01",
            from: Some("2026-01-01"),
        })
        .unwrap();
        assert_eq!(got.partitions[0].name.name.len(), 63);
    }

    #[test]
    fn a_step_that_is_not_a_positive_count_in_a_unit_is_refused() {
        let s = schema("date", &[("ev_a", "2026-01-01", "2026-02-01")]);
        for bad in [
            "0month",
            "-1month",
            "month",
            "1fortnight",
            "1",
            "",
            "1.5month",
            "1 2month",
        ] {
            let said = refusal(ask(&s, bad, "2026-03-01", None));
            assert!(said.contains("--every"), "{bad}: {said}");
        }
        // An integer key takes a plain count, and no unit.
        let s = schema("bigint", &[("ev_0", "0", "10")]);
        for bad in ["0", "-5", "1month", "", "1.5", "10x"] {
            let said = refusal(ask(&s, bad, "100", None));
            assert!(said.contains("--every"), "{bad}: {said}");
        }
    }

    #[test]
    fn a_bound_in_any_other_spelling_is_refused_not_guessed() {
        let s = schema("date", &[("ev_a", "2026-01-01", "2026-02-01")]);
        for bad in [
            "2027-1-1",
            "2027-01-01 00:00:00",
            "20270101",
            "2027-02-30",
            "2027-13-01",
            "0000-01-01",
            "10000-01-01",
            "2027-01-01 BC",
            "infinity",
            "",
        ] {
            let said = refusal(ask(&s, "1month", bad, None));
            assert!(said.contains("--until"), "{bad}: {said}");
        }
        let s = schema(
            "timestamptz",
            &[("ev_a", "2026-01-01 00:00:00+00", "2026-02-01 00:00:00+00")],
        );
        // A bound that is not UTC says so.
        for offset in ["+08", "-05", "+05:30"] {
            let said = refusal(ask(
                &s,
                "1month",
                &format!("2026-03-01 00:00:00{offset}"),
                None,
            ));
            assert!(said.contains("not in UTC"), "{offset}: {said}");
        }
        for bad in [
            "2026-03-01",
            "2026-03-01 00:00:00",
            "2026-03-01T00:00:00+00",
            "2026-03-01 24:00:00+00",
        ] {
            let said = refusal(ask(&s, "1month", bad, None));
            assert!(said.contains("not how the engine prints"), "{bad}: {said}");
        }
        // A declared bound the engine would not print is named by partition.
        let s = schema("date", &[("ev_a", "2026-01-01", "2026-2-1")]);
        let said = refusal(ask(&s, "1month", "2027-01-01", None));
        assert!(
            said.contains("partition app.ev_a ends at `2026-2-1`"),
            "{said}"
        );
        // Integers.
        let s = schema("integer", &[("ev_0", "0", "1000")]);
        for bad in [
            "01000",
            "1e3",
            "+5",
            "-0",
            "1_000",
            "1000.0",
            "",
            "99999999999",
        ] {
            let said = refusal(ask(&s, "1000", bad, None));
            assert!(said.contains("--until"), "{bad}: {said}");
        }
    }

    #[test]
    fn an_integer_end_outside_the_key_type_is_refused() {
        let s = schema("smallint", &[("ev_0", "0", "10000")]);
        let said = refusal(ask(&s, "10000", "40000", None));
        assert!(said.contains("outside the smallint range"), "{said}");
        // The type's own maximum is the last end that fits.
        let s = schema("smallint", &[("ev_0", "0", "32766")]);
        assert!(ask(&s, "1", "32767", None).is_ok());
        assert!(ask(&s, "1", "32768", None).is_err());
        // A negative lower bound has no name.
        let s = schema("integer", &[]);
        let said = refusal(ask(&s, "10", "10", Some("-10")));
        assert!(said.contains("not an identifier"), "{said}");
    }

    #[test]
    fn a_run_is_bounded() {
        let s = schema("bigint", &[("ev_0", "0", "1")]);
        let said = refusal(ask(&s, "1", "9223372036854775807", None));
        assert!(said.contains("at most 2000"), "{said}");
        // The bound itself is a run that works.
        let got = ask(&s, "1", &(1 + MAX_PER_RUN).to_string(), None).unwrap();
        assert_eq!(got.partitions.len() as i128, MAX_PER_RUN);
        let said = refusal(ask(&s, "1", &(2 + MAX_PER_RUN).to_string(), None));
        assert!(said.contains("2001 partitions"), "{said}");
        let d = schema("date", &[("ev_a", "2026-01-01", "2026-01-02")]);
        let said = refusal(ask(&d, "1day", "9999-12-31", None));
        assert!(said.contains("at most 2000"), "{said}");
    }

    /// The calendar helper agrees with itself over every day of 400 years,
    /// which is a whole Gregorian cycle, and with the known epoch.
    #[test]
    fn the_calendar_round_trips_over_a_gregorian_cycle() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1, 1, 1), -719_162);
        let mut day = days_from_civil(1600, 1, 1);
        for y in 1600..2000 {
            for m in 1..=12 {
                for d in 1..=days_in_month(y, m) {
                    assert_eq!(days_from_civil(y, m, d), day, "{y}-{m}-{d}");
                    assert_eq!(civil_from_days(day), (y, m, d), "{y}-{m}-{d}");
                    day += 1;
                }
            }
        }
        // The leap years' rule: divisible by 4, not by 100 unless by 400.
        assert!(is_leap(2000) && is_leap(2028) && !is_leap(1900) && !is_leap(2027));
        assert_eq!(days_in_month(2100, 2), 28);
    }
}
