//! Standalone sequences: what the pull holds, and how a declaration is checked (#2721,
//! DEC-2721.1).
//!
//! Held is a sequence no column owns, that is permanent (not `UNLOGGED`, not temporary),
//! that no package owns, in a project's schema, and whose name the model can spell. A
//! sequence a column owns (`OWNED BY`, an identity, a `serial`) is part of that column and
//! stays as it is; one that is not held is named by the census, never read as less than it
//! is. One SQL text decides "held" for the pull and for the census (the macro below), so a
//! sequence is named exactly when it is not read.
//!
//! **Only `pg_sequence` is read, and never what the counter has reached.** `last_value` and
//! `is_called` (and the `pg_sequences` view that shows them) are state the database advances
//! on every `nextval`. Reading them into the model would make each insert look like a change,
//! so no query here can name them, and a test pins that.

use std::collections::BTreeMap;

use pbps_dialect::DialectError;
use pbps_model::{ColumnType, ObjectName, Schema, Sequence};

/// Whether the namespace behind `$n` (a `pg_namespace` alias) is a schema the pull reads: a
/// project's, not a system or a `pg_*` one, and one a declaration can spell. [`schema_is_held`]
/// is the same question asked of one name in Rust, for `validate`, and a live test puts the
/// two to the engine side by side.
macro_rules! held_namespace {
    ($n:literal) => {
        concat!(
            $n,
            ".nspname NOT IN ('pg_catalog', 'information_schema') \
             AND pg_catalog.left(",
            $n,
            ".nspname, 3) <> 'pg_' AND pg_catalog.strpos(",
            $n,
            ".nspname, '.') = 0"
        )
    };
}

/// [`held_namespace`], asked of one name in Rust.
pub fn schema_is_held(name: &str) -> bool {
    crate::catalog::a_projects_schema(name) && !name.contains('.')
}

/// The namespace predicate over a `pg_namespace` alias `hn`, public so a live test can ask the
/// engine which schemas it keeps.
pub fn held_namespace_predicate() -> &'static str {
    held_namespace!("hn")
}

/// Whether the relation behind `$c` (a `pg_class` alias) is a sequence the pull holds.
///
/// The schema is part of the predicate, not of the pull's query: a system schema, a `pg_*`
/// schema and one with a `.` in its name are not a project's, and the census and the
/// default-sequence check must call a sequence model-owned exactly when the pull declares it.
///
/// A `.` in a schema or a sequence name is not held: a declaration names a sequence as
/// `schema.name`, and a name with a dot in it cannot be read back from that spelling.
macro_rules! held {
    ($c:literal) => {
        concat!(
            "(",
            $c,
            ".relkind = 'S' AND ",
            $c,
            ".oid >= 16384 AND ",
            $c,
            ".relpersistence = 'p' \
             AND pg_catalog.strpos(",
            $c,
            ".relname, '.') = 0 \
             AND EXISTS (SELECT 1 FROM pg_catalog.pg_namespace hn \
               WHERE hn.oid = ",
            $c,
            ".relnamespace AND ",
            $crate::sequence::held_namespace!("hn"),
            ") \
             AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend o \
               WHERE o.classid = 'pg_catalog.pg_class'::pg_catalog.regclass AND o.objid = ",
            $c,
            ".oid AND o.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass \
                AND o.refobjsubid > 0 AND o.deptype IN ('a', 'i')) \
             AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend x \
               WHERE x.classid = 'pg_catalog.pg_class'::pg_catalog.regclass AND x.objid = ",
            $c,
            ".oid AND x.deptype = 'e'))"
        )
    };
}
pub(crate) use {held, held_namespace};

/// The predicate [`query`], the census and the default-sequence check share, over a `pg_class`
/// alias `c`. Public so a live test can ask the engine whether it and the pull agree.
pub fn held_predicate() -> &'static str {
    held!("c")
}

/// Every held sequence, one row each. `pg_sequence`'s own columns only.
pub fn query() -> String {
    format!(
        "SELECT n.nspname::pg_catalog.text AS schema, c.relname::pg_catalog.text AS name,
                pg_catalog.format_type(s.seqtypid, NULL) AS ty,
                s.seqstart::int8 AS start, s.seqincrement::int8 AS increment,
                s.seqmin::int8 AS min, s.seqmax::int8 AS max,
                s.seqcache::int8 AS cache, s.seqcycle AS cycle
           FROM pg_catalog.pg_sequence s
           JOIN pg_catalog.pg_class c ON c.oid = s.seqrelid
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE {held}",
        held = held!("c"),
    )
}

/// The held sequences of one batch's rows. A row that cannot be read is an error that names
/// the column: a sequence is never left out silently, and never held with a field made up.
pub fn decode(
    rows: &[serde_json::Value],
) -> Result<BTreeMap<ObjectName, Sequence>, pbps_db::DbError> {
    let bad = |what: &str| pbps_db::DbError::BadRow(format!("sequence row: {what}"));
    let text = |row: &serde_json::Value, column: &str| {
        row.get(column)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| bad(column))
    };
    let number = |row: &serde_json::Value, column: &str| {
        row.get(column)
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| bad(column))
    };
    let mut held = BTreeMap::new();
    for row in rows {
        let name = ObjectName::new(text(row, "schema")?, text(row, "name")?);
        let ty = ColumnType::simple(text(row, "ty")?);
        let increment = number(row, "increment")?;
        let options = crate::introspect::sequence_options(
            &ty,
            increment,
            number(row, "min")?,
            number(row, "max")?,
            row.get("cycle")
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(|| bad("cycle"))?,
            number(row, "cache")?,
        )
        .ok_or_else(|| bad("ty"))?;
        let sequence = Sequence {
            ty,
            start: number(row, "start")?,
            increment,
            options,
        };
        if held.insert(name, sequence).is_some() {
            return Err(bad("a name twice"));
        }
    }
    Ok(held)
}

fn invalid(message: String) -> DialectError {
    DialectError::Invalid {
        dialect: crate::types::DIALECT,
        message,
    }
}

/// What the engine would refuse, or this model would not read back the same, before any
/// plan: the type, the start and the increment against the bounds, the options, the name,
/// and (elsewhere) its claim on the relation namespace.
pub(crate) fn validate(
    name: &ObjectName,
    sequence: &Sequence,
    _schema: &Schema,
) -> Vec<DialectError> {
    let mut found = Vec::new();
    let shown = name.shown();
    // The type first: every check below reads its range.
    let ty = match crate::types::normalize(&sequence.ty) {
        Ok(ty) => ty,
        Err(refusal) => {
            found.push(refusal);
            return found;
        }
    };
    let Some(accepted) = crate::types::sequence_range(&ty, sequence.increment, &sequence.options)
    else {
        found.push(invalid(format!(
            "sequence `{shown}` is a `{}`, and a sequence is a smallint, an integer or a bigint",
            ty.shown()
        )));
        return found;
    };
    found.extend(crate::options_problems(
        &format!("sequence `{shown}`"),
        "it",
        &ty,
        sequence.increment,
        &sequence.options,
    ));
    if !accepted.contains(&sequence.start) {
        found.push(invalid(format!(
            "sequence `{shown}` starts at {}, and a sequence on `{}` counting by {} starts \
             somewhere in {}..={}: the engine refuses START value ({}) out of the sequence's \
             own bounds.",
            sequence.start,
            ty.shown(),
            sequence.increment,
            accepted.start(),
            accepted.end(),
            sequence.start,
        )));
    }
    // No rule about an increment larger than the span: the engine accepts it (the sequence hands
    // out its start and then refuses), so a database can hold one, the pull reads it, and
    // refusing it here would make a pulled declaration fail `validate` (DEC-2721.1).
    if sequence.increment == 0 {
        found.push(invalid(format!(
            "sequence `{shown}` has an increment of 0, which never advances: the engine refuses \
             it as INCREMENT must not be zero."
        )));
    }
    // Written into the plan's SQL as identifiers, so held to the rules every other name is:
    // an empty one, one holding NUL, or one the engine would truncate.
    for part in [name.schema.as_str(), name.name.as_str()] {
        if let Err(error) = crate::quote(part) {
            found.push(error);
        }
    }
    if !crate::catalog::a_projects_schema(&name.schema) {
        found.push(invalid(format!(
            "sequence `{shown}` is declared in `{schema}`, which this dialect's pull never \
             reads: `pg_catalog`, `information_schema` and every schema whose name begins with \
             `pg_` are excluded from the managed set, so the engine would create the sequence \
             and no plan could ever see it again. Declare it in a project schema",
            schema = pbps_model::escape::Escaped(&name.schema),
        )));
    }
    if name.schema.contains('.') || name.name.contains('.') {
        found.push(invalid(format!(
            "sequence `{shown}` has a `.` inside its schema or name, which a declaration cannot \
             spell: it names a sequence as `schema.name`"
        )));
    }
    // Its name in the relation namespace is claimed with the tables', views', indexes' and keys'
    // in `pbps_dialect::check_index_names`, the one rule that compares them with each other.
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequence(ty: &str, start: i64, increment: i64) -> Sequence {
        Sequence {
            ty: ColumnType::simple(ty),
            start,
            increment,
            options: Default::default(),
        }
    }

    fn said(name: &str, sequence: &Sequence, schema: &Schema) -> String {
        validate(&name.parse().unwrap(), sequence, schema)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn row(name: &str) -> serde_json::Value {
        serde_json::json!({"schema": "app", "name": name, "ty": "bigint", "start": 1,
            "increment": 1, "min": 1, "max": i64::MAX, "cache": 1, "cycle": false})
    }

    /// The query reads `pg_sequence` and never the counter, in the SQL text itself.
    #[test]
    fn the_query_cannot_read_where_the_counter_stands() {
        let sql = query();
        assert!(sql.contains("pg_catalog.pg_sequence s"), "{sql}");
        for forbidden in [
            "last_value",
            "is_called",
            "pg_sequences",
            "pg_sequence_last_value",
        ] {
            assert!(
                !sql.contains(forbidden),
                "the pull query mentions {forbidden}"
            );
        }
    }

    /// A row of the engine's defaults is held with no options set; a row that differs is
    /// held with exactly the difference; a descending one is read against its own defaults.
    #[test]
    fn a_row_is_held_with_only_what_differs_from_the_engines_defaults() {
        let mut custom = row("custom");
        custom["ty"] = "integer".into();
        custom["start"] = 10.into();
        custom["increment"] = 5.into();
        custom["min"] = 10.into();
        custom["max"] = 9999.into();
        custom["cache"] = 20.into();
        custom["cycle"] = true.into();
        let mut down = row("down");
        down["ty"] = "smallint".into();
        down["start"] = (-1).into();
        down["increment"] = (-1).into();
        down["min"] = i64::from(i16::MIN).into();
        down["max"] = (-1).into();
        let held = decode(&[row("plain"), custom, down]).unwrap();
        assert!(held[&"app.plain".parse().unwrap()].options.is_default());
        let custom = &held[&"app.custom".parse().unwrap()];
        assert_eq!((custom.start, custom.increment), (10, 5));
        assert_eq!(custom.options.min, Some(10));
        assert_eq!(custom.options.max, Some(9999));
        assert_eq!(custom.options.cache, Some(20));
        assert!(custom.options.cycle);
        let down = &held[&"app.down".parse().unwrap()];
        assert!(
            down.options.is_default(),
            "a descending default is not a bound"
        );
    }

    /// Negatives: a row missing a column, one with a column of the wrong shape, one naming a
    /// type that is not an integer, and one twice are errors, never a quietly smaller set.
    #[test]
    fn a_row_that_cannot_be_read_is_an_error_and_not_a_missing_sequence() {
        for column in [
            "schema",
            "name",
            "ty",
            "start",
            "increment",
            "min",
            "max",
            "cache",
            "cycle",
        ] {
            let mut missing = row("s");
            missing.as_object_mut().unwrap().remove(column);
            assert!(decode(&[missing]).is_err(), "{column}");
            let mut null = row("s");
            null[column] = serde_json::Value::Null;
            assert!(decode(&[null]).is_err(), "{column}");
        }
        let mut text = row("s");
        text["ty"] = "numeric".into();
        assert!(decode(&[text]).is_err());
        assert!(decode(&[row("s"), row("s")]).is_err());
    }

    #[test]
    fn a_valid_sequence_is_accepted_and_each_refusal_names_the_sequence() {
        let schema = Schema::default();
        assert_eq!(said("app.ok", &sequence("bigint", 1, 1), &schema), "");
        assert_eq!(
            said("app.ok", &sequence("int", 1, 1), &schema),
            "",
            "an alias is a type"
        );
        for (case, seq, expected) in [
            ("type", sequence("numeric", 1, 1), "numeric"),
            (
                "zero",
                sequence("bigint", 1, 0),
                "INCREMENT must not be zero",
            ),
            (
                "range",
                sequence("smallint", 40_000, 1),
                "START value (40000)",
            ),
            ("down", sequence("integer", 1, -1), "START value (1)"),
        ] {
            let said = said("app.s", &seq, &schema);
            assert!(
                said.contains("sequence `app.s`") || case == "type",
                "{case}: {said}"
            );
            assert!(said.contains(expected), "{case}: {said}");
        }
        let mut cap = sequence("bigint", 1, 1);
        cap.options.max = Some(i64::MAX);
        assert!(said("app.s", &cap, &schema).contains("already the engine's max"));
        let mut cache = sequence("bigint", 1, 1);
        cache.options.cache = Some(1);
        assert!(said("app.s", &cache, &schema).contains("leave out `cache:`"));
    }

    /// The one predicate decides "held" for the pull, the census and the default-sequence
    /// check, so it carries the namespace exclusions and the pull's query carries none of its
    /// own: they cannot disagree about a sequence in a system schema (#2721).
    #[test]
    fn the_namespace_exclusions_are_in_the_shared_predicate_and_not_in_the_query() {
        let predicate = held_predicate();
        for needed in [
            "'pg_catalog', 'information_schema'",
            "pg_catalog.left(hn.nspname, 3) <> 'pg_'",
            "pg_catalog.strpos(hn.nspname, '.') = 0",
            "relpersistence = 'p'",
            "refobjsubid > 0",
            "deptype = 'e'",
        ] {
            assert!(predicate.contains(needed), "the predicate lacks {needed}");
        }
        let sql = query();
        assert!(
            sql.contains(predicate),
            "the query uses the shared predicate"
        );
        let own = sql.replace(predicate, "");
        for filter in ["NOT IN", "information_schema", "'pg_'", "left(", "strpos"] {
            assert!(
                !own.contains(filter),
                "the query carries a namespace filter of its own ({filter}): {own}"
            );
        }
    }

    /// What the engine accepts and the pull therefore reads must validate, or a pulled
    /// declaration could not be used (#2721): a step larger than the type's span is one such
    /// shape, and the only one the rules here once refused.
    #[test]
    fn a_step_the_engine_accepts_beyond_the_types_span_validates() {
        let schema = Schema::default();
        assert_eq!(
            said("app.stride", &sequence("smallint", 1, 40_000), &schema),
            ""
        );
        assert_eq!(
            said("app.back", &sequence("smallint", -1, -40_000), &schema),
            ""
        );
        // Negative: what the engine refuses still is.
        assert!(said("app.s", &sequence("smallint", 1, 0), &schema).contains("INCREMENT"));
    }

    /// A sequence in a schema the pull never reads is refused with the table's reasoning, and
    /// the Rust question agrees with the SQL predicate's text on every schema it names.
    #[test]
    fn a_sequence_in_a_schema_the_pull_never_reads_is_refused() {
        let schema = Schema::default();
        for refused in [
            "pg_catalog.s",
            "information_schema.s",
            "pg_toast.s",
            "pg_temp.s",
            "pg_custom.s",
        ] {
            let said = said(refused, &sequence("bigint", 1, 1), &schema);
            assert!(
                said.contains("which this dialect's pull never reads"),
                "{refused}: {said}"
            );
        }
        // Negatives: a project's schema, one that merely contains `pg`, and a dotted one,
        // which is refused for its own reason.
        assert_eq!(said("app.s", &sequence("bigint", 1, 1), &schema), "");
        assert_eq!(said("mypg_app.s", &sequence("bigint", 1, 1), &schema), "");
        assert!(schema_is_held("app") && schema_is_held("pgx") && schema_is_held("Public"));
        assert!(!schema_is_held("pg_x") && !schema_is_held("information_schema"));
        assert!(!schema_is_held("a.b"));
        let found = validate(
            &ObjectName::new("a.b", "s"),
            &sequence("bigint", 1, 1),
            &schema,
        );
        assert!(
            found.iter().any(|e| e.to_string().contains("`.`")),
            "{found:?}"
        );
    }

    #[test]
    fn a_name_the_engine_would_truncate_or_cannot_spell_is_refused() {
        let schema = Schema::default();
        let long = format!("app.{}", "n".repeat(64));
        assert!(!said(&long, &sequence("bigint", 1, 1), &schema).is_empty());
        let name = ObjectName::new("app", "a.b");
        let found = validate(&name, &sequence("bigint", 1, 1), &schema);
        assert!(
            found.iter().any(|e| e.to_string().contains("`.`")),
            "{found:?}"
        );
    }
}
