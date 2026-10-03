//! PostgreSQL heap storage parameters (#1441, DEC-1441.1): the closed list a
//! table may declare, and each value in one canonical spelling.
//!
//! `pg_class.reloptions` keeps the spelling it was given, and the engine
//! parses that spelling with C's rules (measured on 16.15 and 18.6):
//! `autovacuum_enabled=of` is `false`, `fillfactor=070` is octal 56 and
//! `0x14` is 20, `vacuum_index_cleanup=TRUE` is `on`. Text comparison would
//! call each of those a change, so both sides are parsed here, by the
//! engine's rules, into one spelling: `true`/`false`, a decimal integer, the
//! shortest decimal of a real, and `auto`/`on`/`off`.
//!
//! `toast.*` parameters are not here: a table without a TOAST relation
//! silently discards them, so a declared one could never read back.

/// How the engine parses a parameter's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `parse_bool`: `true`, `yes`, `on`, `1` and their unambiguous
    /// prefixes, and the same for false.
    Bool,
    /// `strtol(…, 0)`: decimal, `0x` hexadecimal, leading-zero octal.
    Int,
    /// `strtod`.
    Real,
    /// `vacuum_index_cleanup`: `auto`, or a whole boolean word.
    IndexCleanup,
}

/// Every heap parameter a table may declare, by name. Two are 18's
/// (`autovacuum_vacuum_max_threshold`, `vacuum_max_eager_freeze_failure_rate`):
/// a 16 server refuses them when the plan runs, inside its transaction.
pub const TABLE_PARAMETERS: &[(&str, Kind)] = &[
    ("autovacuum_analyze_scale_factor", Kind::Real),
    ("autovacuum_analyze_threshold", Kind::Int),
    ("autovacuum_enabled", Kind::Bool),
    ("autovacuum_freeze_max_age", Kind::Int),
    ("autovacuum_freeze_min_age", Kind::Int),
    ("autovacuum_freeze_table_age", Kind::Int),
    ("autovacuum_multixact_freeze_max_age", Kind::Int),
    ("autovacuum_multixact_freeze_min_age", Kind::Int),
    ("autovacuum_multixact_freeze_table_age", Kind::Int),
    ("autovacuum_vacuum_cost_delay", Kind::Real),
    ("autovacuum_vacuum_cost_limit", Kind::Int),
    ("autovacuum_vacuum_insert_scale_factor", Kind::Real),
    ("autovacuum_vacuum_insert_threshold", Kind::Int),
    ("autovacuum_vacuum_max_threshold", Kind::Int),
    ("autovacuum_vacuum_scale_factor", Kind::Real),
    ("autovacuum_vacuum_threshold", Kind::Int),
    ("fillfactor", Kind::Int),
    ("log_autovacuum_min_duration", Kind::Int),
    ("parallel_workers", Kind::Int),
    ("toast_tuple_target", Kind::Int),
    ("user_catalog_table", Kind::Bool),
    ("vacuum_index_cleanup", Kind::IndexCleanup),
    ("vacuum_max_eager_freeze_failure_rate", Kind::Real),
    ("vacuum_truncate", Kind::Bool),
];

/// How `name` is parsed, where it is a table parameter at all.
pub fn table_kind(name: &str) -> Option<Kind> {
    TABLE_PARAMETERS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, k)| *k)
}

/// `value` of table parameter `name` in its one canonical spelling, or why
/// it is not one.
pub fn canonical(name: &str, value: &str) -> Result<String, String> {
    let kind = table_kind(name)
        .ok_or_else(|| format!("`{name}` is not a table storage parameter this model declares"))?;
    let read = match kind {
        Kind::Bool => parse_bool(value).map(|b| b.to_string()),
        Kind::Int => parse_int(value).map(|i| i.to_string()),
        // `+ 0.0` turns a negative zero into zero, which `to_string` would
        // otherwise spell `-0` (#1477 review).
        Kind::Real => parse_real(value).map(|r| (r + 0.0).to_string()),
        Kind::IndexCleanup => {
            let word = value.trim().to_lowercase();
            if word == "auto" {
                Some(word)
            } else {
                whole_bool(&word).map(|b| if b { "on" } else { "off" }.to_owned())
            }
        }
    };
    read.ok_or_else(|| format!("`{value}` is not a value of storage parameter `{name}`"))
}

/// PostgreSQL's `parse_bool`: case-insensitive, surrounding whitespace
/// allowed, and any prefix of a word that names one value (`tr`, `of`, but
/// not `o`).
fn parse_bool(value: &str) -> Option<bool> {
    let v = value.trim().to_lowercase();
    if v.is_empty() {
        return None;
    }
    let prefix = |word: &str, min: usize| v.len() >= min && word.starts_with(v.as_str());
    if prefix("true", 1) || prefix("yes", 1) || prefix("on", 2) || v == "1" {
        Some(true)
    } else if prefix("false", 1) || prefix("no", 1) || prefix("off", 2) || v == "0" {
        Some(false)
    } else {
        None
    }
}

/// A boolean written as a whole word, the only form `vacuum_index_cleanup`
/// takes besides `auto`.
fn whole_bool(word: &str) -> Option<bool> {
    match word {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// PostgreSQL's `parse_int`: C's `strtol` with base 0 (surrounding
/// whitespace, a sign, then `0x` hexadecimal, a leading `0` octal, or
/// decimal), and where that stops at `.`, `e` or `E`, the whole value read
/// again as a real and rounded half to even, as `rint` does: `70.5` is 70
/// and `7e1` is 70 (measured on 16 and 18, #1477 review).
fn parse_int(value: &str) -> Option<i64> {
    let v = value.trim();
    let (integer, rest) = strtol(v)?;
    if rest.is_empty() {
        return Some(integer);
    }
    if !rest.starts_with(['.', 'e', 'E']) {
        return None;
    }
    // A hexadecimal real (`0x46.0`) is `strtod`'s too; `parse_real` reads
    // no hexadecimal, so such a value is refused rather than misread.
    let real = parse_real(v)?.round_ties_even();
    (real >= i64::MIN as f64 && real <= i64::MAX as f64).then_some(real as i64)
}

/// C's `strtol(…, 0)` over `v`: the value read and what follows it, or
/// `None` where no digit was read or the value overflows.
fn strtol(v: &str) -> Option<(i64, &str)> {
    let (negative, unsigned) = match v.as_bytes().first()? {
        b'-' => (true, &v[1..]),
        b'+' => (false, &v[1..]),
        _ => (false, v),
    };
    let hex = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
        .filter(|h| h.starts_with(|c: char| c.is_ascii_hexdigit()));
    let (radix, digits) = match hex {
        Some(h) => (16, h),
        None if unsigned.starts_with('0') => (8, unsigned),
        None => (10, unsigned),
    };
    let end = digits
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let mut magnitude: i64 = 0;
    for c in digits[..end].chars() {
        let d = i64::from(c.to_digit(radix)?);
        magnitude = magnitude.checked_mul(i64::from(radix))?.checked_add(d)?;
    }
    Some((
        if negative { -magnitude } else { magnitude },
        &digits[end..],
    ))
}

/// A decimal real, as `strtod` reads it, refused where `strtod` sets
/// `ERANGE`: an overflow, and an underflow to zero or below the smallest
/// normal value, which the engine refuses rather than reading as 0 (measured
/// on 16 and 18, #1477 review). The hexadecimal and infinite forms `strtod`
/// also takes are not read: no bound of these parameters admits infinity,
/// and a hexadecimal one is left to fail where it is read, as a value this
/// reader cannot spell.
fn parse_real(value: &str) -> Option<f64> {
    let v = value.trim();
    if v.is_empty() || v.contains(['x', 'X', 'n', 'N', 'i', 'I']) {
        return None;
    }
    let real = v.parse::<f64>().ok().filter(|r| r.is_finite())?;
    let mantissa = v.split(['e', 'E']).next().unwrap_or_default();
    let written_nonzero = mantissa.contains(|c: char| ('1'..='9').contains(&c));
    let underflow = (real == 0.0 && written_nonzero) || (real != 0.0 && !real.is_normal());
    (!underflow).then_some(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each spelling the engine reads as one value is one value here, by the
    /// engine's own rules (measured on 16 and 18).
    #[test]
    fn every_spelling_the_engine_reads_alike_is_one_value() {
        for (name, spellings, value) in [
            (
                "autovacuum_enabled",
                &["off", "OF", "false", "f", "no", "0", " False "][..],
                "false",
            ),
            (
                "autovacuum_enabled",
                &["on", "tr", "TRUE", "y", "yes", "1"][..],
                "true",
            ),
            (
                "fillfactor",
                &["56", "070", "0x38", "+56", " 56 "][..],
                "56",
            ),
            (
                "autovacuum_vacuum_scale_factor",
                &["0.01", "1e-2", ".01", "0.010"][..],
                "0.01",
            ),
            // Negative zero is zero: kept as `-0`, a bare `-0` would load back
            // as the integer 0 and never round-trip (#1477 review).
            (
                "autovacuum_vacuum_scale_factor",
                &["0", "-0", "-0.0", "0e5"][..],
                "0",
            ),
            // An integer `strtol` stops short of at `.` or `e` is read again
            // as a real and rounded half to even, as `parse_int` does:
            // `70.5` is 70 (measured on 16 and 18, #1477 review).
            (
                "fillfactor",
                &["70.0", "7e1", "70.5", "070.0", "69.5"][..],
                "70",
            ),
            ("fillfactor", &["71.5"][..], "72"),
            (
                "vacuum_index_cleanup",
                &["on", "TRUE", "yes", "1"][..],
                "on",
            ),
            (
                "vacuum_index_cleanup",
                &["off", "false", "no", "0"][..],
                "off",
            ),
            ("vacuum_index_cleanup", &["auto", "AUTO"][..], "auto"),
        ] {
            for spelling in spellings {
                assert_eq!(
                    canonical(name, spelling).as_deref(),
                    Ok(value),
                    "{name}={spelling}"
                );
            }
        }
    }

    /// What the engine refuses, or this reader cannot spell, is refused by
    /// name: never read as some value it is not.
    #[test]
    fn a_name_or_value_the_engine_would_not_take_is_refused() {
        for (name, value) in [
            ("bogus", "1"),
            ("toast.autovacuum_enabled", "false"),
            ("autovacuum_enabled", "o"),
            ("autovacuum_enabled", "maybe"),
            ("fillfactor", "08"),
            ("fillfactor", "1s"),
            ("fillfactor", ""),
            ("autovacuum_vacuum_scale_factor", "inf"),
            ("autovacuum_vacuum_scale_factor", "0x1p-3"),
            ("vacuum_index_cleanup", "tr"),
            // `strtod` underflow, which the engine refuses rather than reading
            // as 0 (#1477 review).
            ("autovacuum_vacuum_scale_factor", "1e-400"),
            ("autovacuum_vacuum_scale_factor", "1e-320"),
            // Neither a fraction nor an exponent where `strtol` stopped.
            ("fillfactor", "70x"),
            ("fillfactor", "08.5"),
        ] {
            assert!(canonical(name, value).is_err(), "{name}={value}");
        }
    }
}
