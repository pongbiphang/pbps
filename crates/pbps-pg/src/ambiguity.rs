//! Declared date/time literals whose value is decided by the session that
//! reads them rather than by their text (#1756, DEC-1756.1).
//!
//! Every write path pins the settings these types read under (DECISIONS 267),
//! so what an apply stores is the same everywhere. It is not necessarily what
//! the declaration's author meant: `'01/02/2026'` on a `date` is 2 January
//! under the pinned `DateStyle` and 1 February to whoever wrote it under DMY,
//! and `'2026-01-02 09:00'` on a `timestamptz` is 09:00 UTC to the pins and
//! 09:00 local to an author in Taipei. Nothing said so. A connected plan now
//! asks the engine to read each such literal under the pins and under
//! contrasting settings, and refuses one that comes back two ways.
//!
//! **Measured, not enumerated.** The engine is asked; no pattern in this file
//! says which spellings are ambiguous. `2026-01-02`, `20260102`, `Jan 2 2026`
//! and `2026-01-02 09:00+08` read the same under every set below and pass,
//! whatever a rule about slashes or offsets would have said.

use std::collections::BTreeMap;

use pbps_db::catalog::{Ambiguous, LiteralAt};
use pbps_db::{Conn, DbError};
use pbps_model::{BoundDatum, PartitionBound, Schema, Value};

use crate::emit::{SETTING_SENSITIVE, value_literal, verbatim};

/// One set of the settings a date/time literal's reading depends on.
struct Settings {
    /// How a refusal names the set.
    name: &'static str,
    datestyle: &'static str,
    timezone: &'static str,
    intervalstyle: &'static str,
    abbreviations: &'static str,
}

/// The values every write path pins (`session_pins!` in `lib.rs`), so the
/// reading under this set is what an apply stores. The test
/// `the_pinned_set_is_what_the_write_pins_say` holds the two together.
const PINNED: Settings = Settings {
    name: "this tool's settings",
    datestyle: "ISO, MDY",
    timezone: "UTC",
    intervalstyle: "postgres",
    abbreviations: "Default",
};

/// Settings that differ from [`PINNED`] in every way a reading can turn on.
///
/// **Two sets, because one cannot differ in every direction at once.**
/// Measured on 18.6: `'01/01/02'` is 2002-01-01 under both MDY and DMY and
/// 2001-01-02 under YMD; `'12:00 IST'` is the same instant under the
/// `Default` and `Australia` abbreviation dictionaries and a different one
/// under `India`. The zones are odd offsets, +12:45/+13:45 and -3:30/-2:30,
/// so that no zone-less time can land on the pinned UTC reading by
/// coincidence. `sql_standard` is the one `IntervalStyle` that parses
/// differently: `'-1 2:03:04'` is -1 day +02:03:04 under `postgres` and
/// -(1 day 02:03:04) under it.
const CONTRASTS: [Settings; 2] = [
    Settings {
        name: "DateStyle DMY, TimeZone Pacific/Chatham, IntervalStyle sql_standard, \
               timezone_abbreviations Australia",
        datestyle: "ISO, DMY",
        timezone: "Pacific/Chatham",
        intervalstyle: "sql_standard",
        abbreviations: "Australia",
    },
    Settings {
        name: "DateStyle YMD, TimeZone America/St_Johns, timezone_abbreviations India",
        datestyle: "ISO, YMD",
        timezone: "America/St_Johns",
        intervalstyle: "postgres",
        abbreviations: "India",
    },
];

/// Special inputs the date/time types read as the moment the statement runs.
/// Measured, `'now'::timestamptz` read in two transactions is two values and
/// `'today'` is a date only `TimeZone` and the clock decide, so a default
/// spelled with one is frozen at whichever moment the `CREATE` ran in.
const DECIDED_BY_THE_CLOCK: [&str; 4] = ["now", "today", "tomorrow", "yesterday"];

/// One literal to ask about.
struct Literal {
    at: LiteralAt,
    declared: String,
    /// The column's type, normalized, typmod and all: the reading is the
    /// stored value, rounding included.
    ty: String,
    base: String,
    /// An array of `base`: each element is read as one.
    array: bool,
    /// SQL that evaluates to the literal's text.
    text: String,
}

/// Every declared date/time literal the engine reads differently under
/// [`PINNED`] and under one of [`CONTRASTS`], or reads as the moment it runs.
///
/// Each set is asked in a `READ ONLY` transaction of its own, rolled back,
/// with `set_config(…, is_local)`, so the session leaves as it came — measured,
/// `SET LOCAL` is accepted in a read-only transaction. What cannot be read
/// under the pins is not an answer here: the spelling check reports it, by
/// name, as a value the engine cannot read at all.
pub(crate) async fn ask(conn: &mut Conn, schema: &Schema) -> Result<Vec<Ambiguous>, DbError> {
    let literals = declared_literals(schema)?;
    if literals.is_empty() {
        return Ok(Vec::new());
    }
    let pinned = read_under(conn, &PINNED, &literals).await?;
    let mut contrasts = Vec::with_capacity(CONTRASTS.len());
    for settings in &CONTRASTS {
        contrasts.push(read_under(conn, settings, &literals).await?);
    }
    let mut out = Vec::new();
    for (i, literal) in literals.into_iter().enumerate() {
        let Some((text, reading)) = &pinned[i] else {
            continue;
        };
        let temporal = !matches!(
            literal.base.as_str(),
            "interval" | "real" | "double precision"
        );
        if temporal && decided_by_the_clock(text) {
            out.push(Ambiguous {
                remedy: remedy(&literal.base, true).to_owned(),
                at: literal.at,
                declared: literal.declared,
                ty: literal.ty,
                pinned: None,
                otherwise: Vec::new(),
            });
            continue;
        }
        let mut otherwise: Vec<(String, String)> = Vec::new();
        for (settings, readings) in CONTRASTS.iter().zip(&contrasts) {
            let Some((_, other)) = &readings[i] else {
                continue;
            };
            if other == reading || otherwise.iter().any(|(o, _)| o == other) {
                continue;
            }
            otherwise.push((other.clone(), settings.name.to_owned()));
        }
        if otherwise.is_empty() {
            continue;
        }
        out.push(Ambiguous {
            remedy: remedy(&literal.base, false).to_owned(),
            at: literal.at,
            declared: literal.declared,
            ty: literal.ty,
            pinned: Some(reading.clone()),
            otherwise,
        });
    }
    Ok(out)
}

/// Whether a text is one of the inputs read as the moment the statement runs,
/// alone or beside a time: `now`, `today 10:00`, `Tomorrow`.
fn decided_by_the_clock(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphabetic()).any(|word| {
        DECIDED_BY_THE_CLOCK
            .iter()
            .any(|w| word.eq_ignore_ascii_case(w))
    })
}

/// What to write instead, by type.
///
/// Each is a spelling the engine also gives back as written, so the remedy
/// does not run straight into the spelling check's own (DECISIONS 101): a row
/// value or a bound must be written the way the engine reads it back, and a
/// `timestamptz` reads back in UTC. `+08` would be one reading, and refused
/// on the next plan as not the engine's spelling.
fn remedy(base: &str, clock: bool) -> &'static str {
    if clock {
        return "write the value itself, or for a default a function the engine runs on each \
                insert, such as `now()` or `CURRENT_DATE`";
    }
    match base {
        "date" => "write it as an ISO date, `YYYY-MM-DD`",
        "timestamp without time zone" => "write it as an ISO timestamp, `YYYY-MM-DD HH:MM:SS`",
        "timestamp with time zone" => {
            "write the instant in UTC with its offset, as the engine spells it, \
             `YYYY-MM-DD HH:MM:SS+00`; a zone abbreviation such as `CST` names different \
             offsets in different dictionaries"
        }
        "time with time zone" => "write it with its UTC offset, `HH:MM:SS+08`",
        "interval" => {
            "give every field its own sign, as the engine spells one: `-1 days -02:03:04`"
        }
        _ => "write it so that only one reading exists",
    }
}

/// Every literal on a setting-sensitive column: the rows' cells and keys, the
/// partitions' range bounds, and the defaults that are one string.
fn declared_literals(schema: &Schema) -> Result<Vec<Literal>, DbError> {
    let mut out = Vec::new();
    let sensitive =
        |ty: &pbps_model::ColumnType| -> Result<Option<(String, String, bool)>, DbError> {
            let ty = crate::types::normalize(ty).map_err(|e| DbError::BadRow(e.to_string()))?;
            Ok(SETTING_SENSITIVE
                .contains(&ty.base.as_str())
                .then(|| (ty.base.clone(), ty.to_string(), ty.is_array())))
        };
    for (name, table) in &schema.tables {
        // A partition's columns are its parent's; its own defaults are read
        // as the parent's column type.
        let columns = match &table.partition_of {
            Some(of) => match schema.tables.get(&of.parent) {
                Some(parent) => &parent.columns,
                None => continue,
            },
            None => &table.columns,
        };
        if let Some(data) = &table.data {
            let key =
                crate::rows::key_column(name, table).map_err(|e| DbError::BadRow(e.to_string()))?;
            for (column, spec) in &table.columns {
                if spec.engine_assigned() {
                    continue;
                }
                let Some((base, ty, array)) = sensitive(&spec.ty)? else {
                    continue;
                };
                for (row_key, row) in &data.rows {
                    let (at_column, text) = if *column == key {
                        (None, row_key.as_str().to_owned())
                    } else {
                        match row.get(column) {
                            Some(Value::Text(t)) => (Some(column.clone()), t.clone()),
                            _ => continue,
                        }
                    };
                    out.push(Literal {
                        at: LiteralAt::Cell {
                            table: name.clone(),
                            key: row_key.clone(),
                            column: at_column,
                        },
                        text: format!("CAST({} AS text)", value_literal(&text)),
                        declared: text,
                        ty: ty.clone(),
                        base: base.clone(),
                        array,
                    });
                }
            }
        }
        let own_defaults: Vec<(&String, &str)> = match &table.partition_of {
            Some(of) => of
                .columns
                .iter()
                .filter_map(|(c, own)| own.default.as_deref().map(|d| (c, d)))
                .collect(),
            None => table
                .columns
                .iter()
                .filter_map(|(c, spec)| spec.default.as_deref().map(|d| (c, d)))
                .collect(),
        };
        for (column, default) in own_defaults {
            let Some(spec) = columns.get(column) else {
                continue;
            };
            let Some((base, ty, array)) = sensitive(&spec.ty)? else {
                continue;
            };
            let Some(string) = crate::rows::the_string_of(default) else {
                continue;
            };
            out.push(Literal {
                at: LiteralAt::Default {
                    table: name.clone(),
                    column: column.clone(),
                },
                declared: default.to_owned(),
                // On a line of its own: a comment the reader let through as
                // trivia would otherwise swallow the rest of the query, as it
                // would the emitter's (DECISIONS 281).
                text: format!("CAST({} AS text)", verbatim(string)),
                ty,
                base,
                array,
            });
        }
        if let Some(of) = &table.partition_of
            && let PartitionBound::Range { from, to } = &of.bound
            && let Some(parent) = schema.tables.get(&of.parent)
            && let Some(by) = &parent.partition_by
        {
            for (i, column) in by.columns.iter().enumerate() {
                let Some(spec) = parent.columns.get(column) else {
                    continue;
                };
                let Some((base, ty, array)) = sensitive(&spec.ty)? else {
                    continue;
                };
                for datum in [from.get(i), to.get(i)].into_iter().flatten() {
                    let BoundDatum::Value(text) = datum else {
                        continue;
                    };
                    out.push(Literal {
                        at: LiteralAt::Bound {
                            partition: name.clone(),
                            column: column.clone(),
                        },
                        text: format!("CAST({} AS text)", value_literal(text)),
                        declared: text.clone(),
                        ty: ty.clone(),
                        base: base.clone(),
                        array,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// A reading of `x`, a value of type `base`, as text no setting changes: the
/// one [`ask`] compares and a refusal shows.
///
/// `DateStyle` is `ISO, …` in every set, so a date or timestamp prints in ISO
/// whichever order it was parsed in. A `timestamptz` or `timetz` prints in the
/// session's zone, so it is shifted to UTC first. An interval prints in the
/// session's `IntervalStyle`, so it is taken apart into months, days and
/// seconds, which together are exactly what it stores. A float has no
/// setting-dependent input, but prints by `extra_float_digits`; as `numeric`
/// it does not.
fn reading(base: &str, x: &str) -> String {
    match base {
        "timestamp with time zone" => format!("CAST(({x}) AT TIME ZONE 'UTC' AS text) || '+00'"),
        "time with time zone" => format!("CAST(({x}) AT TIME ZONE 'UTC' AS text)"),
        "interval" => format!(
            "pg_catalog.concat(EXTRACT(year FROM {x}) * 12 + EXTRACT(month FROM {x}), \
             ' months ', EXTRACT(day FROM {x}), ' days ', \
             EXTRACT(hour FROM {x}) * 3600 + EXTRACT(minute FROM {x}) * 60 \
             + EXTRACT(second FROM {x}), ' seconds')"
        ),
        "real" | "double precision" => format!("CAST(CAST({x} AS numeric) AS text)"),
        _ => format!("CAST({x} AS text)"),
    }
}

/// Each literal's text and reading under `settings`, by index; `None` where
/// the engine cannot read it as its type under them.
async fn read_under(
    conn: &mut Conn,
    settings: &Settings,
    literals: &[Literal],
) -> Result<Vec<Option<(String, String)>>, DbError> {
    let mut by_type: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, literal) in literals.iter().enumerate() {
        by_type.entry(literal.ty.as_str()).or_default().push(i);
    }
    conn.execute("BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY")
        .await?;
    let outcome = async {
        conn.query(&format!(
            "SELECT pg_catalog.set_config('search_path', '', true),
                    pg_catalog.set_config('standard_conforming_strings', 'on', true),
                    pg_catalog.set_config('datestyle', {}, true),
                    pg_catalog.set_config('timezone', {}, true),
                    pg_catalog.set_config('intervalstyle', {}, true),
                    pg_catalog.set_config('timezone_abbreviations', {}, true)",
            value_literal(settings.datestyle),
            value_literal(settings.timezone),
            value_literal(settings.intervalstyle),
            value_literal(settings.abbreviations),
        ))
        .await?;
        let mut out = vec![None; literals.len()];
        for (ty, indexes) in &by_type {
            let first = &literals[indexes[0]];
            let read = if first.array {
                // Element by element, in order: an array prints its elements
                // the way the session prints the element type, so the array's
                // own text is no better than theirs (measured, a
                // `timestamptz[]` prints in the session's zone).
                format!(
                    "CAST((SELECT pg_catalog.array_agg({} ORDER BY u.n) \
                     FROM pg_catalog.unnest(CAST(v.s AS {ty})) WITH ORDINALITY AS u(e, n)) \
                     AS text)",
                    reading(&first.base, "u.e")
                )
            } else {
                reading(&first.base, &format!("CAST(v.s AS {ty})"))
            };
            let values = indexes
                .iter()
                .map(|&i| format!("({i}, {})", literals[i].text))
                .collect::<Vec<_>>()
                .join(",\n");
            // The fence `spelling_queries` needs, for its reason: one row
            // would be folded and cast while planning (DECISIONS 324).
            let sql = format!(
                "SELECT v.i, v.s, CASE WHEN pg_catalog.pg_input_is_valid(v.s, {}) THEN {} END\n  \
                 FROM (SELECT pbps_v.i, pbps_v.s FROM (VALUES {values}) AS pbps_v(i, s) \
                 OFFSET 0) AS v(i, s);",
                value_literal(ty),
                read,
            );
            for row in conn.query(&sql).await? {
                let i: Option<i32> = row.try_get_at(0)?;
                let text: Option<&str> = row.try_get_at(1)?;
                let read: Option<&str> = row.try_get_at(2)?;
                let Some(i) = i.and_then(|i| usize::try_from(i).ok()) else {
                    return Err(DbError::BadRow(
                        "the ambiguity query returned a NULL index".to_owned(),
                    ));
                };
                if let (Some(slot), Some(text), Some(read)) = (out.get_mut(i), text, read) {
                    *slot = Some((text.to_owned(), read.to_owned()));
                }
            }
        }
        Ok::<_, DbError>(out)
    }
    .await;
    // Rolled back either way: nothing here is a write, and the settings are
    // the transaction's.
    let ended = conn.execute("ROLLBACK").await;
    let out = outcome?;
    ended?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned set is what an apply stores under only while it says what
    /// the write pins say. A change to one without the other would compare
    /// readings against settings no statement runs under.
    #[test]
    fn the_pinned_set_is_what_the_write_pins_say() {
        let pins = crate::SESSION_PINS;
        for (setting, value) in [
            ("DateStyle", PINNED.datestyle),
            ("TimeZone", PINNED.timezone),
            ("IntervalStyle", PINNED.intervalstyle),
            ("timezone_abbreviations", PINNED.abbreviations),
        ] {
            assert!(
                pins.contains(&format!("SET {setting} = '{value}';")),
                "{setting} = '{value}' is not in the write pins: {pins}"
            );
        }
    }

    #[test]
    fn every_contrast_differs_from_the_pins_in_every_setting_it_names() {
        for c in &CONTRASTS {
            assert_ne!(c.datestyle, PINNED.datestyle, "{}", c.name);
            assert_ne!(c.timezone, PINNED.timezone, "{}", c.name);
            assert_ne!(c.abbreviations, PINNED.abbreviations, "{}", c.name);
        }
        // `sql_standard` is the one style that parses differently, so one set
        // carrying it is enough, and one must.
        assert!(CONTRASTS.iter().any(|c| c.intervalstyle == "sql_standard"));
    }

    #[test]
    fn a_clock_word_is_found_alone_beside_a_time_and_in_any_case() {
        for t in ["now", " NOW ", "today", "Tomorrow 10:00", "yesterday"] {
            assert!(decided_by_the_clock(t), "{t}");
        }
    }

    #[test]
    fn a_date_or_zone_that_contains_no_clock_word_is_not_one() {
        // Words that merely contain one, and the ordinary spellings.
        for t in [
            "2026-01-02",
            "Jan 2 2026",
            "2026-01-02 09:00 America/Toronto",
            "nowhere",
            "epoch",
            "infinity",
            "allballs",
        ] {
            assert!(!decided_by_the_clock(t), "{t}");
        }
    }
}
