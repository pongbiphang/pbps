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
use pbps_model::{ColumnType, ModuleId, ObjectName, Schema, Sequence};

/// Whether the relation behind `$c` (a `pg_class` alias) is a sequence the pull holds.
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
             AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace hn \
               WHERE hn.oid = ",
            $c,
            ".relnamespace AND pg_catalog.strpos(hn.nspname, '.') > 0) \
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
pub(crate) use held;

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
          WHERE {held}
            AND n.nspname NOT IN ('pg_catalog', 'information_schema')
            AND pg_catalog.left(n.nspname, 3) <> 'pg_'",
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
/// and a table or view of the same name (a sequence is in their namespace).
pub(crate) fn validate(
    name: &ObjectName,
    sequence: &Sequence,
    schema: &Schema,
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
    } else {
        let span = i128::from(*accepted.end()) - i128::from(*accepted.start());
        if i128::from(sequence.increment).abs() > span {
            found.push(invalid(format!(
                "sequence `{shown}` has an increment of {} whose magnitude exceeds its span \
                 {span} for `{}`, so no start can produce a second value",
                sequence.increment,
                ty.shown(),
            )));
        }
    }
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
    if name.schema.contains('.') || name.name.contains('.') {
        found.push(invalid(format!(
            "sequence `{shown}` has a `.` inside its schema or name, which a declaration cannot \
             spell: it names a sequence as `schema.name`"
        )));
    }
    if schema.tables.contains_key(name) {
        found.push(invalid(format!(
            "sequence `{shown}` shares its name with a table, and the engine keeps tables, views \
             and sequences in one namespace"
        )));
    }
    if schema.modules.contains_key(&ModuleId::Named(name.clone())) {
        found.push(invalid(format!(
            "sequence `{shown}` shares its name with a view, and the engine keeps tables, views \
             and sequences in one namespace"
        )));
    }
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
            ("span", sequence("smallint", 1, 70_000), "magnitude"),
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

    #[test]
    fn a_sequence_sharing_a_name_with_a_table_or_a_view_is_refused() {
        let mut schema = Schema::default();
        schema
            .tables
            .insert("app.t".parse().unwrap(), Default::default());
        assert!(
            said("app.t", &sequence("bigint", 1, 1), &schema)
                .contains("shares its name with a table")
        );
        schema.modules.insert(
            ModuleId::Named("app.v".parse().unwrap()),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: "SELECT 1".into(),
                options: Default::default(),
            },
        );
        assert!(
            said("app.v", &sequence("bigint", 1, 1), &schema)
                .contains("shares its name with a view")
        );
        // Negative: another name is fine.
        assert_eq!(said("app.other", &sequence("bigint", 1, 1), &schema), "");
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
