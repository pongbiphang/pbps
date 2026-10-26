//! PostgreSQL: the dialect, as far as it is built.
//!
//! The counterpart of `pbps-mssql`, and the crate SPEC §11.1's map has been
//! reserving since Phase 0. Its design is [ADR-0009](../docs/ADR-0009-postgres-modules.md)
//! through [ADR-0014](../docs/ADR-0014-driver-seam-tested.md), each measured
//! against a real server before any of this existed.
//!
//! # What this crate does not hold
//!
//! No driver. `tokio-postgres` is named in `pbps-db` and nowhere else, the same
//! rule that keeps `tiberius` out of `pbps-mssql` (ADR-0007 decision 5). What
//! lives here is what to ask the database and what the answers mean.
//!
//! # What is built, and what refuses
//!
//! This crate arrives one step at a time (issue #76). Everything not yet built
//! **refuses by name**, through [`Unbuilt`], rather than returning an empty
//! answer: a dialect that reports "no changes" because its emitter is a stub
//! would be the silent wrong answer this tool exists to prevent, and *absent,
//! empty and unreadable are three different things*.
//!
//! # Where the connected answers live
//!
//! [`catalog`] and [`modules`] are the two places that run SQL, and they ask
//! different kinds of question. `catalog` reads the whole managed set in one
//! read-only snapshot and hands rows to [`introspect`]'s pure assembler;
//! `modules` answers what a plan has to know **before it rebuilds one object**,
//! inside the caller's own transaction and under that object's lock, because
//! its answer has to still be true when the `DROP` runs (ADR-0009 §3).

#[cfg(test)]
use pbps_dialect::PlanDialect;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::{Dialect, DialectError, Lexicon, Statement, TransactionFraming, TypeChangeRisk};
use pbps_model::{
    Change, ColumnType, Module, ModuleId, ModuleKind, Role, RoutineArg, Schema, SessionZone,
    Strategy, Table, TableName,
};

mod ambiguity;
pub mod assertions;
pub mod catalog;
pub mod data_triggers;
pub mod doctor;
mod drop_impact;
mod emit;
pub mod engine_effects;
pub mod resolver;

/// This engine's lexis, for the definition scanners (ADR-0011 Amendment 2):
/// `"` quotes an identifier and `[` does not, `E'…'` is an escape string and
/// `$tag$…$tag$` a literal closed only by its own tag.
pub(crate) const LEXICON: Lexicon = Lexicon {
    whitespace_is_ascii: true,
    quoted_identifiers: &[('"', '"')],
    escape_strings: true,
    dollar_quoted_strings: true,
    // Measured: `N'x'`, `B'101'`, `X'1F'`, `U&'d\0061ta'` and `E'y'` are
    // literals; `note'x'` is the type `note` applied to a string.
    string_prefixes: &["u&", "e", "n", "b", "x"],
    identifier_continues: pbps_dialect::continues_ident,
    reserved: types::is_reserved,
    unicode_identifiers: true,
};
pub mod estimate;
pub mod generated;
pub mod impact;
pub mod input_path;
pub mod introspect;
pub mod modules;
pub mod partition_index;
mod partition_runway;
mod partition_series;
pub mod partman;
pub mod pins;
mod preflight;
pub mod roles;
pub mod rows;
pub mod staged;
pub mod state;
pub mod type_rebuild;
pub mod type_refs;
mod types;
pub mod user_types;
pub mod validate;

/// A part of the dialect that Phase 5 has not built yet.
///
/// One place, so that "what is missing" is a list rather than a habit, and so
/// that every refusal names the step that supplies it. The message is written
/// for whoever runs the command, not for whoever writes the crate: it says what
/// pbps cannot do, and it does not pretend the answer is "nothing to do".
///
/// **No `emit` arm reads it any more.** Roles (step 6) and reference data
/// (step 7) were the last two, and each is now a statement or a refusal that
/// names its own rule. With the probes of step 9 in as well, **one** part is
/// left: the read-back the CLI has yet to call. The type stays a list rather
/// than collapsing into that one message, because the day another path has to
/// say "not built" is the day it must say it in these words rather than invent
/// its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unbuilt {
    Introspection,
}

impl Unbuilt {
    #[must_use]
    pub const fn step(self) -> &'static str {
        match self {
            Unbuilt::Introspection => "reading a database back (Phase 5 step 3)",
        }
    }

    #[must_use]
    pub fn refuse(self) -> DialectError {
        DialectError::NotBuilt {
            dialect: "postgres",
            part: self.step().to_owned(),
        }
    }
}

/// PostgreSQL's limit on one identifier: 63 bytes, which is `NAMEDATALEN - 1`.
///
/// **Bytes, not characters** — the SQL Server counterpart counts characters
/// (`ident::MAX_IDENT_CHARS`), and copying that shape here would have been
/// wrong in both directions. Measured on 18.6: 32 `ä` is 64 bytes and comes
/// back as 31 of them, truncated on a character boundary.
///
/// Enforced rather than left to the server, because the server does not refuse
/// a longer name — it truncates it and says so in a `NOTICE` nothing here
/// reads. Measured, both halves of that:
///
/// - a name that survives records itself at one length and reads back at
///   another, which is a drift report that never goes quiet;
/// - two names differing only after byte 63 **collide** — the second
///   `CREATE TABLE` fails with `relation "aaa…" already exists`, naming a
///   table the declarations do not contain.
pub const MAX_IDENT_BYTES: usize = 63;

/// Identity declaration checks: engine refusals and increments that exhaust
/// every valid starting point after one value (decision 462), measured on 18.6.
///
/// `validate` is where a declaration should fail, and until the catalogue
/// existed this could not be asked: `validate_table` refused every table
/// outright, so nothing reached the question.
/// What the engine refuses in a generated column, or this model does not
/// hold, refused before it is applied (DEC-1168.1). Measured on 16.15, 17.11
/// and 18.6: a default beside a generation expression is refused ("both
/// default and generation expression specified"), and a virtual kind is 18's
/// only and computes on read, which this model does not hold.
fn generation_problems(column: &str, generated: &pbps_model::Generated) -> Vec<DialectError> {
    let invalid = |message: String| DialectError::Invalid {
        dialect: crate::types::DIALECT,
        message,
    };
    let mut found = Vec::new();
    if !generated.stored {
        found.push(invalid(format!(
            "column `{column}` is a virtual generated column (`stored: false`), which this model \
             does not hold; declare `stored: true`",
            column = pbps_model::escape::Escaped(column),
        )));
    }
    if crate::LEXICON.expression_in(&generated.expression) == pbps_dialect::Expression::Absent {
        found.push(invalid(format!(
            "column `{column}` has an empty generation expression",
            column = pbps_model::escape::Escaped(column),
        )));
    }
    found
}

/// What a RANGE-partitioned parent and its partitions must be (#1170,
/// DEC-1170.1): a key over declared columns, which every key and UNIQUE
/// constraint includes (the engine refuses one that does not); a partition
/// that declares nothing of its own but checks, indexes (#1577), foreign keys
/// (#1632), persistence and storage parameters (#1580), since it inherits the
/// rest and the other partition-level extras are not held yet (#1532); and no `data:` on either, whose rows the engine routes by bound.
fn partition_problems(
    table: &pbps_model::Table,
    types: &BTreeMap<pbps_model::TypeRef, pbps_model::ManagedType>,
) -> Vec<DialectError> {
    let invalid = |message: String| DialectError::Invalid {
        dialect: crate::types::DIALECT,
        message,
    };
    let mut found = Vec::new();
    if let Some(by) = &table.partition_by {
        if by.columns.is_empty() {
            found.push(invalid("`partition_by` names no column".to_owned()));
        }
        // What the reader leaves a tree out over, so that a parent pbps
        // creates is one it reads back (DEC-1170.1).
        if !table.storage_parameters.is_empty()
            || table.unlogged
            || table.replica_identity.is_some()
        {
            found.push(invalid(
                "a partitioned table takes no `storage_parameters`, `unlogged` or \
                 `replica_identity` yet"
                    .to_owned(),
            ));
        }
        // A scalar declared enum keys a LIST or RANGE table (DEC-2346.1); every
        // other reference keeps DEC-1523.1's refusal. A HASH key is the engine's
        // hash of the enum's oid, which a rebuild or a restore changes.
        let admits_enum = by.strategy != pbps_model::PartitionStrategy::Hash;
        for column in &by.columns {
            if table.columns.get(column).is_some_and(|c| {
                c.ty.type_ref().is_some()
                    && !(admits_enum && declared_enum_labels(&c.ty, types).is_some())
            }) {
                found.push(DialectError::InvalidVerbatim {
                    dialect: crate::types::DIALECT,
                    message: format!(
                        "partition key `{column}` has a user-defined type reference, which is not supported as a partition key",
                        column = column.shown(),
                    ),
                });
            }
            if !table.columns.contains_key(column) {
                found.push(invalid(format!(
                    "`partition_by` names `{column}`, which is not a declared column",
                    column = column.shown(),
                )));
            }
        }
        let keys = table
            .primary_key
            .iter()
            .map(|k| ("the primary key".to_owned(), &k.columns))
            .chain(table.unique.iter().map(|(n, u)| {
                (
                    format!("unique constraint `{n}`", n = n.shown(),),
                    &u.columns,
                )
            }));
        for (what, columns) in keys {
            for key in &by.columns {
                if !columns.contains(key) {
                    found.push(invalid(format!(
                        "{what} does not include the partition key column `{key}`, which \
                         PostgreSQL requires of every key on a partitioned table",
                        key = key.shown(),
                    )));
                }
            }
        }
    }
    // A partition's own checks and indexes are its to declare (#1577), and
    // its persistence and storage parameters (#1580), and its foreign keys
    // (#1632); its columns and keys are its parent's, and the rest is not
    // held yet (#1532).
    if table.partition_of.is_some() {
        let own = !table.columns.is_empty()
            || !table.computed.is_empty()
            || table.primary_key.is_some()
            || !table.unique.is_empty()
            || table.clustered.is_some()
            || table.replica_identity.is_some()
            || table.system_time.is_some()
            || table.partition_by.is_some();
        if own {
            found.push(invalid(
                "a partition declares only its own checks, indexes, foreign keys, `unlogged` \
                 and `storage_parameters`: its columns and keys are its parent's, and the rest \
                 is not held yet"
                    .to_owned(),
            ));
        }
    }
    if table.data.is_some() && (table.partition_by.is_some() || table.partition_of.is_some()) {
        found.push(invalid(
            "`data:` cannot be declared on a partitioned table or a partition yet".to_owned(),
        ));
    }
    found
}

/// The labels of the declared enum a scalar column type names, or `None` for
/// any other type: an array, a domain, a composite, an undeclared name
/// (DEC-2346.1). A lookup by name in the schema's own declarations.
pub(crate) fn declared_enum_labels<'a>(
    ty: &pbps_model::ColumnType,
    types: &'a BTreeMap<pbps_model::TypeRef, pbps_model::ManagedType>,
) -> Option<&'a [String]> {
    if ty.is_array() {
        return None;
    }
    match types.get(ty.type_ref()?)? {
        pbps_model::ManagedType::Enum { values } => Some(values),
        pbps_model::ManagedType::Domain { .. } => None,
    }
}

/// A bound datum over a declared enum key is one of the enum's labels, which
/// is the whole check: no engine is asked, so a type this plan creates is
/// judged as a standing one is (DEC-2346.1). `MINVALUE`, `MAXVALUE` and
/// `NULL` name no label.
fn bound_label_problems(
    name: &TableName,
    table: &Table,
    schema: &pbps_model::Schema,
) -> Vec<DialectError> {
    use pbps_model::{BoundDatum, ListDatum, PartitionBound};
    let Some(of) = &table.partition_of else {
        return Vec::new();
    };
    let Some(parent) = schema.tables.get(&of.parent) else {
        return Vec::new();
    };
    let Some(by) = &parent.partition_by else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (at, column) in by.columns.iter().enumerate() {
        let Some(ty) = parent.columns.get(column).map(|c| &c.ty) else {
            continue;
        };
        let Some(labels) = declared_enum_labels(ty, &schema.types) else {
            continue;
        };
        let named: Vec<&String> = match &of.bound {
            PartitionBound::Range { from, to } => [from.get(at), to.get(at)]
                .into_iter()
                .flatten()
                .filter_map(|datum| match datum {
                    BoundDatum::Value(label) => Some(label),
                    BoundDatum::MinValue | BoundDatum::MaxValue => None,
                })
                .collect(),
            PartitionBound::List { values } => values
                .iter()
                .filter_map(|datum| match datum {
                    ListDatum::Value(label) => Some(label),
                    ListDatum::Null => None,
                })
                .collect(),
            PartitionBound::Default | PartitionBound::Hash { .. } => Vec::new(),
        };
        for label in named.into_iter().filter(|l| !labels.contains(*l)) {
            found.push(DialectError::Invalid {
                dialect: crate::types::DIALECT,
                message: format!(
                    "partition `{name}` is bound to `{label}`, which is not a label of `{ty}`, the \
                     type of its key column `{column}`; the declared labels are {labels}",
                    name = name.shown(),
                    label = pbps_model::escape::Escaped(label),
                    ty = ty.shown(),
                    column = column.shown(),
                    labels = labels
                        .iter()
                        .map(|l| format!("`{}`", pbps_model::escape::Escaped(l)))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            });
        }
    }
    found
}

fn identity_problems(
    column: &str,
    declared: &pbps_model::Column,
    identity: &pbps_model::Identity,
) -> Vec<DialectError> {
    let mut found = Vec::new();
    let invalid = |message: String| DialectError::Invalid {
        dialect: "postgres",
        message,
    };
    found.extend(options_problems(
        column,
        "its `identity:`",
        &declared.ty,
        identity.increment,
        &identity.options,
    ));
    match types::sequence_range(&declared.ty, identity.increment, &identity.options) {
        None => found.push(invalid(format!(
            "column `{column}` has an `identity:` on `{}`. This engine says it in so many words: \
             identity column type must be smallint, integer, or bigint. A `numeric` is refused \
             here even with a scale of zero, where SQL Server admits one.",
            declared.ty.shown(),
            column = pbps_model::escape::Escaped(column),
        ))),
        // The seed is checked against the sequence's bounds, not the column's:
        // see [`types::identity_seed_range`] for why those are not the same
        // range, and for the two seeds that fit the type and are still refused.
        Some(accepted) if !accepted.contains(&identity.seed) => found.push(invalid(format!(
            "column `{column}` has an `identity:` with a seed of {}, and an `identity:` on `{}` \
             counting by {} starts somewhere in {}..={}: the engine refuses the table with START \
             value ({}) out of the sequence's own bounds.",
            identity.seed,
            declared.ty.shown(),
            identity.increment,
            accepted.start(),
            accepted.end(),
            identity.seed,
            column = pbps_model::escape::Escaped(column),
        ))),
        Some(accepted) => {
            // An exact span can reach both endpoints. PostgreSQL accepts a
            // larger step at CREATE, but no valid seed then yields a second
            // value. Widen before taking a magnitude: i64::MIN is spellable.
            let span = i128::from(*accepted.end()) - i128::from(*accepted.start());
            if i128::from(identity.increment).abs() > span {
                found.push(invalid(format!(
                    "column `{column}` has an `identity:` increment of {} whose magnitude exceeds \
                     the sequence span {span} for `{}` ({}..={}). PostgreSQL accepts the table, \
                     but no valid seed can produce a second value; use an increment magnitude \
                     at most {span}.",
                    identity.increment,
                    declared.ty.shown(),
                    accepted.start(),
                    accepted.end(),
                    column = pbps_model::escape::Escaped(column),
                )));
            }
        }
    }
    // `GENERATED ... AS IDENTITY` implies NOT NULL, and saying both is
    // `conflicting NULL/NOT NULL declarations`.
    if declared.nullable {
        found.push(invalid(format!(
            "column `{column}` has an `identity:`, so it cannot be nullable: the engine refuses \
             the pair as conflicting NULL/NOT NULL declarations.",
            column = pbps_model::escape::Escaped(column),
        )));
    }
    // `INCREMENT must not be zero`, and it is worth saying why rather than
    // quoting: a step of zero hands every row the same value.
    if identity.increment == 0 {
        found.push(invalid(format!(
            "column `{column}` has an `identity:` with an increment of 0, which never advances: \
             the engine refuses it as INCREMENT must not be zero.",
            column = pbps_model::escape::Escaped(column),
        )));
    }
    found
}

/// What the engine refuses in a column with an owned sequence (`serial`), or
/// this model does not hold, refused before it is applied (#1631,
/// DEC-1631.1). The sequence is created `AS` the column's type with the
/// engine's own bounds for it, which are an identity's, so its start and
/// increment are held to the same ranges (`types::identity_seed_range`).
fn sequence_problems(
    table_name: &TableName,
    table: &Table,
    column: &str,
    declared: &pbps_model::Column,
    sequence: &pbps_model::OwnedSequence,
) -> Vec<DialectError> {
    let invalid = |message: String| DialectError::Invalid {
        dialect: types::DIALECT,
        message,
    };
    let mut found = Vec::new();
    found.extend(options_problems(
        column,
        "its sequence",
        &declared.ty,
        sequence.increment,
        &sequence.options,
    ));
    match types::sequence_range(&declared.ty, sequence.increment, &sequence.options) {
        None => found.push(invalid(format!(
            "column `{column}` has an owned sequence on `{}`; a `serial` column is a smallint, \
             an integer or a bigint",
            declared.ty.shown(),
            column = pbps_model::escape::Escaped(column),
        ))),
        Some(accepted) if !accepted.contains(&sequence.start) => found.push(invalid(format!(
            "column `{column}`'s sequence starts at {}, and a sequence on `{}` counting by {} \
             starts somewhere in {}..={}: the engine refuses START value ({}) out of the \
             sequence's own bounds.",
            sequence.start,
            declared.ty.shown(),
            sequence.increment,
            accepted.start(),
            accepted.end(),
            sequence.start,
            column = pbps_model::escape::Escaped(column),
        ))),
        Some(accepted) => {
            let span = i128::from(*accepted.end()) - i128::from(*accepted.start());
            if i128::from(sequence.increment).abs() > span {
                found.push(invalid(format!(
                    "column `{column}`'s sequence has an increment of {} whose magnitude \
                     exceeds its span {span} for `{}`, so no start can produce a second value",
                    sequence.increment,
                    declared.ty.shown(),
                    column = pbps_model::escape::Escaped(column),
                )));
            }
        }
    }
    if sequence.increment == 0 {
        found.push(invalid(format!(
            "column `{column}`'s sequence has an increment of 0, which never advances: the \
             engine refuses it as INCREMENT must not be zero.",
            column = pbps_model::escape::Escaped(column),
        )));
    }
    // `serial` makes the column NOT NULL; a NULL in it is a row the sequence
    // was never asked for.
    if declared.nullable {
        found.push(invalid(format!(
            "column `{column}` is a `serial`, which is NOT NULL; leave out `nullable: true`",
            column = pbps_model::escape::Escaped(column),
        )));
    }
    // Only the parent owns the sequence; a partition declares no serial of
    // its own, even though the engine copies the parent's default (#2217).
    if table.partition_of.is_some() {
        found.push(invalid(format!(
            "column `{column}` is a `serial` on a partition; declare it on the partitioned parent",
            column = pbps_model::escape::Escaped(column),
        )));
    }
    if let Some(named) = &sequence.name {
        // Written into the plan's SQL as an identifier, so held to the rules
        // every other name is: an empty one, or one holding NUL, is refused
        // here, not by the emitter after the plan was made.
        if let Err(e) = quote(named) {
            found.push(e);
        }
        if *named == generated_name(&table_name.name, Some(column), "seq") {
            found.push(invalid(format!(
                "column `{column}`'s sequence is named `{named}`, the name the engine generates; \
                 leave out `name:`",
                column = pbps_model::escape::Escaped(column),
                named = pbps_model::escape::Escaped(named),
            )));
        }
        // The engine truncates a longer name to 63 bytes with a notice, and
        // then reads back a name the declaration does not hold.
        if named.len() > 63 {
            found.push(invalid(format!(
                "column `{column}`'s sequence name `{named}` is longer than 63 bytes, which the \
                 engine truncates",
                column = pbps_model::escape::Escaped(column),
                named = pbps_model::escape::Escaped(named),
            )));
        }
    }
    found
}

/// What the engine refuses, or this model would not read back the same, in
/// a sequence's bounds, cycle and cache (#1921, DEC-1921.1): a bound outside
/// the type, a bound spelled as its own default (read back as none, so the
/// next plan would differ), bounds that cross, and a cache below one.
fn options_problems(
    column: &str,
    what: &str,
    ty: &pbps_model::ColumnType,
    increment: i64,
    options: &pbps_model::SequenceOptions,
) -> Vec<DialectError> {
    let invalid = |message: String| DialectError::Invalid {
        dialect: types::DIALECT,
        message,
    };
    let mut found = Vec::new();
    let (Some(held), Some(default)) = (
        types::identity_range(ty),
        types::identity_seed_range(ty, increment),
    ) else {
        // The type itself is refused by the caller.
        return found;
    };
    for (name, value, own) in [
        ("min", options.min, *default.start()),
        ("max", options.max, *default.end()),
    ] {
        let Some(value) = value else { continue };
        if !held.contains(&value) {
            found.push(invalid(format!(
                "column `{column}`: {what} has `{name}: {value}`, outside `{ty}`'s \
                 {}..={}: the engine refuses it",
                held.start(),
                held.end(),
                ty = ty.shown(),
                column = pbps_model::escape::Escaped(column),
            )));
        } else if value == own {
            found.push(invalid(format!(
                "column `{column}`: {what} has `{name}: {value}`, which is already the \
                 engine's {name} for `{ty}` counting by {increment}; leave out `{name}:`",
                ty = ty.shown(),
                column = pbps_model::escape::Escaped(column),
            )));
        }
    }
    let low = options.min.unwrap_or(*default.start());
    let high = options.max.unwrap_or(*default.end());
    if low >= high {
        found.push(invalid(format!(
            "column `{column}`: {what} runs {low}..={high}, and its minimum must be below its \
             maximum: the engine refuses MINVALUE ({low}) must be less than MAXVALUE ({high})",
            column = pbps_model::escape::Escaped(column),
        )));
    }
    if let Some(cache) = options.cache {
        if cache < 1 {
            found.push(invalid(format!(
                "column `{column}`: {what} has `cache: {cache}`, and a cache holds at least \
                 one number: the engine refuses CACHE ({cache}) must be greater than zero",
                column = pbps_model::escape::Escaped(column),
            )));
        } else if cache == 1 {
            found.push(invalid(format!(
                "column `{column}`: {what} has `cache: 1`, which is already the engine's; \
                 leave out `cache:`",
                column = pbps_model::escape::Escaped(column),
            )));
        }
    }
    found
}

/// A numbering column whose sequence cycles, under a primary key, a unique
/// constraint or a unique index, refused by name (#1921, DEC-1921.1): once
/// it wraps it hands out a number a row already holds, and the insert fails
/// on that key.
fn cycle_problems(table: &Table, column: &str) -> Vec<DialectError> {
    let mut keys: Vec<String> = Vec::new();
    if let Some(pk) = &table.primary_key
        && pk.columns.iter().any(|c| c.raw() == column)
    {
        keys.push(match &pk.name {
            Some(name) => format!("primary key `{name}`", name = name.shown(),),
            None => "the primary key".to_owned(),
        });
    }
    for (name, unique) in &table.unique {
        if unique.columns.iter().any(|c| c.raw() == column) {
            keys.push(format!("unique constraint `{name}`", name = name.shown(),));
        }
    }
    for (name, index) in &table.indexes {
        if index.unique
            && index
                .columns
                .iter()
                .any(|c| c.key.column().is_some_and(|c| c.raw() == column))
        {
            keys.push(format!("unique index `{name}`", name = name.shown(),));
        }
    }
    keys.into_iter()
        .map(|key| DialectError::Invalid {
            dialect: types::DIALECT,
            message: format!(
                "column `{column}` has `cycle: true` and is under {key}: once its sequence \
                 wraps it hands out a number a row already holds, and the insert fails on that \
                 key. Leave out `cycle:`, or take the column out of the key",
                column = pbps_model::escape::Escaped(column),
            ),
        })
        .collect()
}

/// Each cycling numbering column under a unique index whose key is an
/// expression naming it, as a note rather than a refusal (#2014,
/// DEC-1921.1). A wrap collides there as on the column itself where the
/// expression gives back the number, as `(id + 0)` does (measured on 18), and
/// does not where it gives back NULL, as `NULLIF(id, id)` does under the
/// default `NULLS DISTINCT`. Which one an expression is takes reading it,
/// which the model does not do, so the plan is not refused and the question
/// is said to be open. The expression names the column by the rules
/// `impact::mentions` reads a stored body with.
fn cycle_expression_notes(schema: &Schema) -> Vec<String> {
    let mut notes = Vec::new();
    for (table_name, table) in &schema.tables {
        for (column, c) in &table.columns {
            let cycles = match &c.value_source {
                pbps_model::ValueSource::Identity(identity) => identity.options.cycle,
                pbps_model::ValueSource::Sequence(sequence) => sequence.options.cycle,
                pbps_model::ValueSource::None
                | pbps_model::ValueSource::Default(_)
                | pbps_model::ValueSource::Generated(_) => false,
            };
            if !cycles {
                continue;
            }
            for (name, index) in &table.indexes {
                let over_expression = index.columns.iter().any(|k| {
                    matches!(&k.key, pbps_model::IndexKey::Expression(text)
                        if crate::impact::mentions(text, column.raw()))
                });
                // A plain key on the column is refused by `cycle_problems`.
                let over_column = index.columns.iter().any(|k| k.key.column() == Some(column));
                if index.unique && over_expression && !over_column {
                    notes.push(format!(
                        "{}.{column} has `cycle: true` and unique index `{name}` has an \
                         expression over it: whether a number the sequence hands out again \
                         collides there depends on what the expression gives back, which is \
                         not checked",
                        table_name.shown(),
                        column = column.shown(),
                        name = name.shown(),
                    ));
                }
            }
        }
    }
    notes
}

/// The PostgreSQL dialect.
///
/// It carries one thing, and that is not decoration: the schemas that follow
/// an object's own on the **write** `search_path` (ADR-0013 §3). Measured on
/// 18.6, all three of the verbatim expressions this model holds — a column's
/// default, a check's expression and an index's filter — are *refused* at
/// creation when an unqualified name in them resolves nowhere:
///
/// ```text
/// search_path = ''         ->  refused: function floorish(integer) does not exist
/// search_path = wp         ->  refused: function floorish(integer) does not exist
/// search_path = wp, wpx    ->  accepted
/// ```
///
/// The middle line is the one that decides the shape. A path of the object's
/// own schema alone is not enough — an extension installed in `public` is the
/// ordinary case — so the extras have to come from somewhere, and the only
/// honest somewhere is the project. The selected dialect retains these extras
/// throughout certified emission; a strategy chooses how, and never the write
/// namespace (ADR-0003).
///
/// Ordinary CLI selection has no extras. Resolver reconstruction supplies the
/// qualified write-path extras from its captured inputs (ADR-0013 section 3).
/// Raw changes, immutable intermediates and caller-defined views are not
/// the real PostgreSQL emitter's associated input (DEC-1959.5).
///
/// ```compile_fail
/// fn erased(plan: &'static dyn pbps_diff::order::PlanView<pbps_pg::Postgres>,
///           emitter: &'static dyn pbps_dialect::PlanDialect<Plan = dyn pbps_diff::order::PlanView<pbps_pg::Postgres>>) {
///     pbps_dialect::lower::lower_all(plan, emitter);
/// }
/// ```
///
/// ```compile_fail
/// use pbps_dialect::PlanDialect;
/// fn raw(pg: &pbps_pg::Postgres, changes: &pbps_model::ChangeSet) {
///     pg.emit(changes, 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// use pbps_dialect::PlanDialect;
/// fn intermediate(pg: &pbps_pg::Postgres, plan: &pbps_diff::order::PlanningPlan) {
///     pg.emit(plan, 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn another_emitter(plan: &pbps_dialect::lower::LoweredPlan<pbps_dialect::MinimalDialect>,
///                    pg: &pbps_pg::Postgres) {
///     pbps_dialect::render_script(plan, pg, &pbps_model::SessionZone::utc());
/// }
/// ```
/// A prepared bootstrap script retains the lowered engine in both directions.
/// ```compile_fail
/// fn pg(script: &pbps_dialect::BootstrapScript<pbps_pg::Postgres>) {
///     script.render(&pbps_mssql::Mssql, &pbps_model::SessionZone::utc());
/// }
/// ```
/// ```compile_fail
/// fn ms(script: &pbps_dialect::BootstrapScript<pbps_mssql::Mssql>) {
///     script.render(&pbps_pg::Postgres::new(), &pbps_model::SessionZone::utc());
/// }
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Postgres {
    write_path_extras: Vec<String>,
    server_version_num: Option<i64>,
    attach_constraint_names: BTreeMap<pbps_model::Uid, BTreeSet<pbps_model::ObjectLocalName>>,
}

impl Postgres {
    /// The dialect with no configured extra project schemas on the write path.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The dialect with `extras` after an object's own schema, in that order.
    /// The emitted path places the `pg_temp` alias once at the end, including
    /// when it appears among these extras (DECISIONS 464).
    ///
    /// The order is part of what a declaration means, and ADR-0013 measured it:
    /// a view created under `(m_ea, m_eb)` keeps the binding it was created
    /// with, while a fresh one under `(m_eb, m_ea)` takes the other. So this
    /// takes a sequence and never a set.
    #[must_use]
    pub fn with_write_path_extras(extras: Vec<String>) -> Self {
        Self {
            write_path_extras: extras,
            ..Self::default()
        }
    }

    /// Table-local constraint names observed for this plan's attaching identities.
    #[must_use]
    pub fn with_attach_constraint_names(
        mut self,
        names: BTreeMap<pbps_model::Uid, BTreeSet<pbps_model::ObjectLocalName>>,
    ) -> Self {
        self.attach_constraint_names = names;
        self
    }

    /// A connected planner's observed capability. Unknown callers use the
    /// same split as PG16; only an unsplittable shape needs this observation.
    #[must_use]
    pub fn with_server_version(mut self, version: i64) -> Self {
        self.server_version_num = Some(version);
        self
    }

    /// The configured extras, before the emitter moves `pg_temp` to the end.
    #[must_use]
    pub fn write_path_extras(&self) -> &[String] {
        &self.write_path_extras
    }
}

/// Quotes one identifier, or refuses a string that cannot be one.
///
/// Shared by [`Dialect::quote_ident`], the emitter and the impact scanner:
/// matching a stored spelling must use the same escaping as writing it.
fn quote(ident: &str) -> Result<String, DialectError> {
    // A double quote inside an identifier is doubled; a NUL cannot be in
    // one at all, and the engine's own limit is bytes, not characters.
    if ident.is_empty() {
        return Err(DialectError::UnquotableIdent(ident.to_owned()));
    }
    if ident.contains('\0') {
        return Err(DialectError::UnquotableIdent(ident.to_owned()));
    }
    // The limit is enforced here because the server does not enforce it:
    // it **truncates** and says so in a `NOTICE` that nothing reads. See
    // [`MAX_IDENT_BYTES`].
    if ident.len() > MAX_IDENT_BYTES {
        return Err(DialectError::UnquotableIdent(ident.to_owned()));
    }
    Ok(format!("\"{}\"", ident.replace('"', "\"\"")))
}

/// The settings every write of this dialect runs under, as one statement.
///
/// Transactional execution, staged/resumed execution and rendered scripts
/// share this text, so their parser settings cannot drift apart.
///
/// The empty `search_path` is the one setting here that is not a parser's:
/// pbps's own SQL between the scopes must reach no operator a schema user
/// added (DEC-1564.1). `pbps-db` sets it on every connection already; here it
/// reaches a rendered script, and every scope ends on it.
///
/// `TimeZone` is the target's own, which the plan recorded, and not UTC: an
/// offset-less `timestamptz` is read in the target's zone (DEC-1916.1). It
/// follows `standard_conforming_strings`, which makes doubling the whole of
/// quoting its literal.
pub(crate) fn session_pins(zone: &SessionZone) -> String {
    format!(
        "SET search_path = ''; \
         SET standard_conforming_strings = on; SET check_function_bodies = on; \
         SET DateStyle = 'ISO, MDY'; SET TimeZone = {zone}; \
         SET IntervalStyle = 'postgres'; \
         SET timezone_abbreviations = 'Default'; \
         SET transform_null_equals = off; \
         SET bytea_output = 'hex'; SET extra_float_digits = 1; \
         SET xmloption = content;",
        zone = emit::literal(zone.as_str())
    )
}

/// What this engine makes of a column's declared default: one the applying
/// session would decide, and a NULL it erases. A table's column, and a
/// partition's own default on its parent's column (#1578 review).
fn default_problems(
    column_name: &str,
    column: &pbps_model::Column,
    normalized: &ColumnType,
    expr: &str,
) -> Vec<DialectError> {
    let mut found = Vec::new();
    // A default whose value the applying session would decide
    // (ADR-0013 §3). Asked here and not only in the emitter, because
    // `AlterColumnDefault` carries no type and the emitter therefore
    // cannot ask it on the one path that changes a default on a column
    // that is already there. This is where a user is looking at the
    // declaration, and every command that hands statements to a
    // database runs these checks (DECISIONS 141).
    if let Some(e) = emit::refuse_an_unresolved_default(column_name, normalized, expr) {
        found.push(e);
    }
    // A default that is a NULL of the column's own type. **Measured**
    // on 18.6: `DEFAULT NULL`, `DEFAULT (NULL)` and `DEFAULT NULL::text`
    // on a `text` column leave the column with no `pg_attrdef` row at
    // all — to this engine a column with no default *is* a column
    // whose default is NULL — so the pull reads the column back with
    // none, every connected plan sets the default again, and the
    // deploy's check of what came back, which compares whether a
    // default is there (DECISIONS 185, 186), refuses the column the
    // plan just wrote. Refused where the user is looking at the
    // declaration, naming what to declare instead, as `serial` is
    // (DECISIONS 227, 351). A NULL cast to any *other* type, or to
    // the column's type where the column carries a modifier —
    // `NULL::varchar` on `text`, `NULL::varchar(10)` on
    // `varchar(10)` — is a default the engine keeps and reads back,
    // and is left alone (DECISIONS 361). The type is the one the
    // grammar sees, so `NULL::pg_catalog.text` and `NULL::"text"`
    // are `NULL::text` (DECISIONS 364). SQL Server keeps `(NULL)` as
    // a default constraint of its own, which is why this rule is this
    // dialect's.
    let (core, cast) = rows::unwrapped_with_type(expr);
    let erased = core.eq_ignore_ascii_case("null")
        && match cast {
            None => true,
            Some(ty) => {
                column.ty.args.is_empty()
                    && types::as_the_grammar_spells(&ty)
                        .and_then(|t| t.parse::<ColumnType>().ok())
                        .and_then(|t| types::normalize(&t).ok())
                        .is_some_and(|t| t == *normalized)
            }
        };
    if erased && column.ty.type_ref().is_none() {
        found.push(DialectError::Invalid {
            dialect: types::DIALECT,
            message: format!(
                "column `{column_name}` declares `default: {expr}`, which this engine \
                 does not keep: a NULL of the column's own type, bare, cast or in \
                 parentheses, leaves no default in the catalog at all, and the \
                 column reads back with none. Every plan would set it again and the \
                 check of what the apply left behind would refuse the column. A \
                 column with no default already defaults to NULL here: declare no \
                 default.",
                column_name = pbps_model::escape::Escaped(column_name),
                expr = pbps_model::escape::Escaped(expr),
            ),
        });
    }
    found
}

/// Admission binds the checker, even when the wire artifact was loaded by a
/// different crate; neither direction may reach the other engine's lowering.
///
/// ```compile_fail
/// fn pg_admission_cannot_reach_mssql(wire: pbps_model::SavedPlan) {
///     let checksum = wire.wire_fingerprint();
///     let plan = pbps_diff::order::admit(wire, &pbps_pg::Postgres::new(),
///         std::path::Path::new("plan.json"), &checksum).unwrap();
///     let _ = pbps_dialect::lower::lower_all::<pbps_mssql::Mssql>(&plan, &pbps_mssql::Mssql);
/// }
/// ```
///
/// ```compile_fail
/// fn mssql_admission_cannot_reach_pg(wire: pbps_model::SavedPlan) {
///     let checksum = wire.wire_fingerprint();
///     let plan = pbps_diff::order::admit(wire, &pbps_mssql::Mssql,
///         std::path::Path::new("plan.json"), &checksum).unwrap();
///     let _ = pbps_dialect::lower::lower_all::<pbps_pg::Postgres>(&plan, &pbps_pg::Postgres::new());
/// }
/// ```
///
/// Ordered and newly sealed inputs retain that same brand:
///
/// ```compile_fail
/// fn pg_order_cannot_reach_mssql(plan: &pbps_diff::order::OrderedPlan<pbps_pg::Postgres>) {
///     let _ = pbps_dialect::lower::lower_all::<pbps_mssql::Mssql>(plan, &pbps_mssql::Mssql);
/// }
/// ```
///
/// ```compile_fail
/// fn mssql_order_cannot_reach_pg(plan: &pbps_diff::order::OrderedPlan<pbps_mssql::Mssql>) {
///     let _ = pbps_dialect::lower::lower_all::<pbps_pg::Postgres>(plan, &pbps_pg::Postgres::new());
/// }
/// ```
///
/// ```compile_fail
/// fn pg_seal_cannot_reach_mssql(plan: &pbps_diff::order::SealedPlan<pbps_pg::Postgres>) {
///     let _ = pbps_dialect::lower::lower_all::<pbps_mssql::Mssql>(plan, &pbps_mssql::Mssql);
/// }
/// ```
///
/// ```compile_fail
/// fn mssql_seal_cannot_reach_pg(plan: &pbps_diff::order::SealedPlan<pbps_mssql::Mssql>) {
///     let _ = pbps_dialect::lower::lower_all::<pbps_pg::Postgres>(plan, &pbps_pg::Postgres::new());
/// }
/// ```
/// Canonical completion also keeps its planner: the final constructor cannot
/// relabel a completed PostgreSQL route as SQL Server, or the reverse.
///
/// ```compile_fail
/// fn pg_canonical_cannot_reach_mssql(plan: pbps_diff::order::CanonicalPlan<pbps_pg::Postgres>) {
///     let _ = plan.ordered(pbps_model::PlanMode::Transactional, &pbps_mssql::Mssql);
/// }
/// ```
/// ```compile_fail
/// fn mssql_canonical_cannot_reach_pg(plan: pbps_diff::order::CanonicalPlan<pbps_mssql::Mssql>) {
///     let _ = plan.ordered(pbps_model::PlanMode::Transactional, &pbps_pg::Postgres::new());
/// }
/// ```
/// Re-scheduling cannot substitute another engine for an already completed
/// route. Both directions remain invalid even before the final emitter call.
///
/// ```compile_fail
/// use pbps_diff::order::deployment::schedule::{schedule, Declarations, Facts};
/// fn rescheduling_pg_to_mssql(plan: pbps_diff::order::CanonicalPlan<pbps_pg::Postgres>,
///     declared: &Declarations<'_>, facts: &Facts<'_>) {
///     let dialect = pbps_mssql::Mssql;
///     let scheduled = schedule(plan, declared, facts, &dialect).unwrap();
///     let ordered = scheduled.changes.ordered(pbps_model::PlanMode::Transactional, &dialect);
///     let _ = pbps_dialect::lower::lower_all::<pbps_mssql::Mssql>(&ordered, &dialect);
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::deployment::schedule::{schedule, Declarations, Facts};
/// fn scope_continuation_pg_to_mssql(plan: pbps_diff::order::CanonicalPlan<pbps_pg::Postgres>,
///     declared: &Declarations<'_>, facts: &Facts<'_>) {
///     let scope = pbps_diff::order::writebacks::ScopeInput::new(vec![]).unwrap();
///     let pending = scope.append_to(plan, &pbps_pg::Postgres::new());
///     let dialect = pbps_mssql::Mssql;
///     let scheduled = schedule(pending, declared, facts, &dialect).unwrap();
///     let _ = scheduled.changes.ordered(pbps_model::PlanMode::Transactional, &dialect);
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::deployment::schedule::{schedule, Declarations, Facts};
/// fn scope_input_pg_to_mssql(plan: pbps_diff::order::CanonicalPlan<pbps_pg::Postgres>,
///     declared: &Declarations<'_>, facts: &Facts<'_>) {
///     let scope = pbps_diff::order::writebacks::ScopeInput::new(vec![]).unwrap();
///     let _ = scope.append_to(plan, &pbps_mssql::Mssql);
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::deployment::schedule::{schedule, Declarations, Facts};
/// fn scratch_preparation_pg_to_mssql(plan: pbps_diff::order::CanonicalPlan<pbps_pg::Postgres>,
///     declared: &Declarations<'_>, facts: &Facts<'_>) {
///     let _ = pbps_diff::order::scratch::prepare(
///         &plan, &pbps_model::IdsFile::default(), &pbps_model::ModuleDeps::default(), &pbps_mssql::Mssql,
///     );
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::deployment::schedule::{schedule, Declarations, Facts};
/// fn semantic_erasure_pg_to_mssql(plan: pbps_diff::order::CanonicalPlan<pbps_pg::Postgres>,
///     declared: &Declarations<'_>, facts: &Facts<'_>) {
///     let _ = pbps_diff::order::deployment::schedule::schedule_semantic(
///         plan, declared, facts, &pbps_mssql::Mssql,
///     );
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::deployment::schedule::{schedule, Declarations, Facts};
/// fn rescheduling_mssql_to_pg(plan: pbps_diff::order::CanonicalPlan<pbps_mssql::Mssql>,
///     declared: &Declarations<'_>, facts: &Facts<'_>) {
///     let dialect = pbps_pg::Postgres::new();
///     let scheduled = schedule(plan, declared, facts, &dialect).unwrap();
///     let ordered = scheduled.changes.ordered(pbps_model::PlanMode::Transactional, &dialect);
///     let _ = pbps_dialect::lower::lower_all::<pbps_pg::Postgres>(&ordered, &dialect);
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::deployment::schedule::{schedule, Declarations, Facts};
/// fn scope_continuation_mssql_to_pg(plan: pbps_diff::order::CanonicalPlan<pbps_mssql::Mssql>,
///     declared: &Declarations<'_>, facts: &Facts<'_>) {
///     let scope = pbps_diff::order::writebacks::ScopeInput::new(vec![]).unwrap();
///     let pending = scope.append_to(plan, &pbps_mssql::Mssql);
///     let dialect = pbps_pg::Postgres::new();
///     let scheduled = schedule(pending, declared, facts, &dialect).unwrap();
///     let _ = scheduled.changes.ordered(pbps_model::PlanMode::Transactional, &dialect);
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::deployment::schedule::{schedule, Declarations, Facts};
/// fn scope_input_mssql_to_pg(plan: pbps_diff::order::CanonicalPlan<pbps_mssql::Mssql>,
///     declared: &Declarations<'_>, facts: &Facts<'_>) {
///     let scope = pbps_diff::order::writebacks::ScopeInput::new(vec![]).unwrap();
///     let _ = scope.append_to(plan, &pbps_pg::Postgres::new());
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::deployment::schedule::{schedule, Declarations, Facts};
/// fn scratch_preparation_mssql_to_pg(plan: pbps_diff::order::CanonicalPlan<pbps_mssql::Mssql>,
///     declared: &Declarations<'_>, facts: &Facts<'_>) {
///     let _ = pbps_diff::order::scratch::prepare(
///         &plan, &pbps_model::IdsFile::default(), &pbps_model::ModuleDeps::default(), &pbps_pg::Postgres::new(),
///     );
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::deployment::schedule::{schedule, Declarations, Facts};
/// fn semantic_erasure_mssql_to_pg(plan: pbps_diff::order::CanonicalPlan<pbps_mssql::Mssql>,
///     declared: &Declarations<'_>, facts: &Facts<'_>) {
///     let _ = pbps_diff::order::deployment::schedule::schedule_semantic(
///         plan, declared, facts, &pbps_pg::Postgres::new(),
///     );
/// }
/// ```
///
impl pbps_dialect::PlanDialect for Postgres {
    fn bootstrap_guard(
        &self,
        schema: &Schema,
        trusted: &std::collections::BTreeSet<pbps_model::RoleName>,
    ) -> Result<Option<Statement>, DialectError> {
        Ok(type_refs::bootstrap_guard(schema, trusted)?.map(Statement::new))
    }

    type Plan = dyn pbps_diff::order::PlanView<Self>;

    fn emit(&self, plan: &Self::Plan, position: usize) -> Result<Vec<Statement>, DialectError> {
        let planned = &pbps_dialect::PlanPayload::changes(plan).changes[position];
        emit::emit(self, &planned.change, planned.strategy)
    }

    fn emit_staged(
        &self,
        plan: &Self::Plan,
        position: usize,
    ) -> Result<Vec<Statement>, DialectError> {
        let planned = &pbps_dialect::PlanPayload::changes(plan).changes[position];
        emit::emit_staged(self, &planned.change, planned.strategy)
    }
}

impl Dialect for Postgres {
    /// Measured on 16 and 18: a CHECK, a foreign key and a partial or
    /// expression index hold constants typed as the old enum, which the
    /// retype refuses to compare with the replacement. Plain keys and indexes
    /// are rebuilt by the engine (DEC-1635.7).
    fn enum_rebuild_dependents(&self) -> pbps_dialect::RetypeDependents {
        pbps_dialect::RetypeDependents {
            keys_and_indexes: false,
            checks: true,
            filtered_indexes: true,
            foreign_keys: true,
        }
    }

    fn statement_count(
        &self,
        change: &Change,
        strategy: Strategy,
        mode: pbps_model::PlanMode,
    ) -> Result<usize, DialectError> {
        match mode {
            pbps_model::PlanMode::Transactional => emit::emit(self, change, strategy),
            pbps_model::PlanMode::Staged => emit::emit_staged(self, change, strategy),
        }
        .map(|statements| statements.len())
    }

    fn attach_scan_key(
        &self,
        uid: &pbps_model::Uid,
        key: &[(pbps_model::ColumnName, pbps_model::ColumnType)],
        types: &BTreeMap<pbps_model::TypeRef, pbps_model::ManagedType>,
    ) -> Vec<pbps_model::ColumnName> {
        // The staged CHECK spells its bound with bare operators and untyped
        // literals, which resolve to the type's own for a built-in type and for
        // an enum, and to none for a domain (measured, DEC-2346.1). A key over
        // anything else attaches by the single statement.
        let spells = key
            .iter()
            .all(|(_, ty)| ty.type_ref().is_none() || declared_enum_labels(ty, types).is_some());
        if !spells
            || self
                .attach_constraint_names
                .get(uid)
                .is_some_and(|names| names.contains(&emit::scan_check_name(uid)))
        {
            Vec::new()
        } else {
            key.iter().map(|(column, _)| column.clone()).collect()
        }
    }

    fn validate_plan_analysis(
        &self,
        plan: &pbps_model::SavedPlan,
    ) -> Result<(), pbps_dialect::PlanAnalysisError> {
        plan.validate_analysis()?;
        if let pbps_model::resolver::PlanAnalysis::Resolved(evidence) = &plan.analysis {
            if plan.dialect != "postgres" {
                return Err(pbps_dialect::PlanAnalysisError::Unsupported(
                    plan.dialect.clone(),
                ));
            }
            resolver::validate_evidence(evidence)?;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "postgres"
    }

    fn generated_table_parts(
        &self,
        change: &Change,
        ids: &pbps_model::IdsFile,
        functions: &[pbps_model::ModuleId],
        needs_complete_shape: bool,
    ) -> Result<Option<Vec<Change>>, DialectError> {
        generated::parts(
            change,
            ids,
            functions,
            needs_complete_shape,
            self.server_version_num,
            false,
        )
    }

    fn scratch_table_parts(
        &self,
        change: &Change,
        ids: &pbps_model::IdsFile,
        functions: &[pbps_model::ModuleId],
        needs_complete_shape: bool,
    ) -> Result<Option<Vec<Change>>, DialectError> {
        generated::parts(
            change,
            ids,
            functions,
            needs_complete_shape,
            self.server_version_num,
            true,
        )
    }

    fn generation_may_call(&self, text: &str, name: &str) -> bool {
        generated::may_call(text, name)
    }

    fn module_creation_text<'a>(&self, module: &'a pbps_model::Module) -> Cow<'a, str> {
        generated::creation_text(module)
    }

    fn generation_names_table(&self, text: &str, table: &TableName) -> bool {
        generated::names_table(text, table)
    }

    fn generation_table_name_only(&self, text: &str, table: &TableName) -> bool {
        generated::table_name_only(text, table)
    }

    /// `"…"` alone quotes an identifier: `[` is an array subscript, which is
    /// code, and reading it as a quote made a reindent inside `a[1 + 2]` read
    /// as a changed module. Both extensions to the string literal are here —
    /// `E'…'`, where `\'` does not close the string, and `$tag$…$tag$`, which
    /// nothing inside it can close early (ADR-0011 Amendment 2).
    fn lexicon(&self) -> Lexicon {
        LEXICON
    }

    /// PostgreSQL runs DDL inside a transaction, and a failed statement aborts
    /// the whole one — which is what SQL Server needs `SET XACT_ABORT ON` to
    /// approximate.
    ///
    /// `begin` carries two settings anyway, and they are here rather than in
    /// the emitter because **measured on 18.6, a setting cannot take effect in
    /// the batch it appears in**. A multi-statement simple query is *lexed as a
    /// whole* before any of it runs, so a `SET standard_conforming_strings`
    /// in front of the statement it is meant to protect protects nothing:
    ///
    /// ```text
    /// one batch:  SET LOCAL standard_conforming_strings = off; SELECT length('it\'s here');
    ///             -> syntax error: the batch was lexed under the old value
    /// two:        SET standard_conforming_strings = off;  then  the same SELECT
    ///             -> one literal of length 9
    /// ```
    ///
    /// So the pin has to be established on an *earlier* batch, and `begin` is
    /// the earlier batch a transactional apply runs — the same place SQL
    /// Server's `SET XACT_ABORT ON` lives, for the same structural reason.
    ///
    /// **A staged apply opens no transaction and is therefore not covered
    /// here.** The pin belongs on the connection in that mode, and it cannot be
    /// moved into the emitter's statements: a staged run checkpoints after each
    /// one, and a `--resume` on a fresh connection starts at the next
    /// unexecuted statement — which is the one whose pin was two statements
    /// back. Nothing can reach that path on this dialect yet, and it lands with
    /// the ledger and the staged path in Phase 5 step 8.
    ///
    /// - **`standard_conforming_strings = on`** is what makes ADR-0011's
    ///   scanner rule — a plain literal escapes by doubling — true.
    ///   Measured under `off`, `CHECK (label <> 'it\'s  here')` is *accepted*
    ///   as one literal, while the normalizer closes it at the escaped quote:
    ///   a whitespace edit inside that literal then compares equal and is
    ///   never planned. Under `on` the same text is a syntax error, which is
    ///   the loud failure (ADR-0013 §3).
    /// - **`check_function_bodies = on`** is what makes ADR-0009's
    ///   opaque-caller exemption true. Measured, a SQL body naming a relation
    ///   that does not exist is created *silently* under `off` and fails the
    ///   first time it is called; under `on` the `CREATE` is refused.
    ///
    /// **The rest are here for a different reason: they are constants, and each
    /// one decides what a declared expression *means*.** All three verbatim
    /// expressions this model holds — a default, a check, an index filter
    /// (ADR-0013 §3) — are text the engine reads through an input function or
    /// a parser rule, and the same declaration then creates a different object
    /// under two settings, silently. Measured, one row per setting:
    ///
    /// ```text
    /// CHECK (d >= '01/02/2026')       MDY -> '2026-01-02'   DMY -> '2026-02-01'
    /// CHECK (t >= '2026-01-02 00:00') UTC -> 00:00:00+00    America/New_York -> 05:00:00+00
    /// CHECK (i >= '-1 2:00:00')       postgres -> -1 days +02:00:00
    ///                                 sql_standard -> -1 days -02:00:00
    /// CHECK (t >= '2026-01-15 12:00:00 CST')
    ///                                 Default -> 18:00:00+00   Australia -> 02:30:00+00
    /// CHECK (x = NULL)                off -> (x = NULL::integer)   on -> (x IS NULL)
    /// ```
    ///
    /// One is the sign and one is the whole predicate, which are the kinds of
    /// difference nobody reads twice. `timezone_abbreviations` is a dictionary
    /// `TimeZone` does not cover — fifteen and a half hours apart above — and
    /// `transform_null_equals` is not an input function at all but a *parser*
    /// rewrite, which is why the list cannot be "the settings temporal input
    /// reads": it is **every setting that changes what the declared text
    /// means**, and that is the rule to extend it by.
    ///
    /// They could ride in the statement's own scope — unlike the two above,
    /// these are read at parse *analysis*, and measured, a `SET LOCAL
    /// DateStyle` does reach the rest of its own batch — but they do not vary
    /// per statement the way `search_path` does, and putting nine settings and
    /// nine `RESET`s around every line of a plan would bury the SQL a reviewer
    /// is there to read (SPEC §14.1). A constant belongs where the constants
    /// are.
    ///
    /// **`bytea_output` and `extra_float_digits` are here too, and were not.**
    /// They were excluded as *output-only*, and that was measured and true of
    /// the case it was measured on: a declared expression stores the same
    /// constraint under `hex`/`1` and under `escape`/`0`, because nothing in
    /// `CHECK (b >= '\x0102')` runs a value through an output function. A
    /// **type conversion** does. Measured:
    ///
    /// ```text
    /// bytea -> text            hex -> \x0102        escape -> \001\002
    /// double precision -> text 1 -> 0.12345678901234568   -3 -> 0.123456789012
    /// ```
    ///
    /// Same stored bytes, same approved `ALTER`, two different strings left in
    /// the table. Pinning them is the complete answer where refusing the
    /// conversion would be an enumeration — every cast to text goes through an
    /// output function, and the list of which ones read a setting is exactly
    /// the list this pin makes irrelevant. The values are the read scope's, so
    /// what a plan writes is what the next `pull` reads back.
    ///
    /// **`xmloption = content` joins the list (DEC-1634.1)**, and it is the
    /// parser class: it decides what an `xml` literal is read as. Measured on
    /// 16.15 and 18.6, `'abc'::xml` and `'<a/><b/>'::xml` are accepted under
    /// `content` and are `invalid XML document` under `document`, so the same
    /// `DEFAULT` or row is a valid plan from one session and a refusal from the
    /// other. `content` is the pin because it reads every document and every
    /// fragment: it never refuses a value the other would take. The server's
    /// own default is `content`, so a session nobody configured already agrees.
    ///
    /// `lc_monetary` belongs to this class and is deliberately absent, for the
    /// reason `catalog.rs` gives on the read side: `SET` fails outright on a
    /// locale the server does not have, so pinning it would turn a database
    /// that deploys into one that cannot. Its reach is a `money` literal, and
    /// this dialect's type catalogue refuses `money` outright.
    ///
    /// **Two settings the canonical *read* scope pins are deliberately not
    /// here**, and that is derived rather than trimmed: `bytea_output` and
    /// `extra_float_digits` decide how a value is *rendered*, not how one is
    /// read. Measured, `CHECK (b >= '\x0102' AND f >= 0.1)` stores the same
    /// constraint under `hex`/`1` and under `escape`/`0`.
    ///
    /// None is spelled `SET LOCAL`: a rendered script is run statement by
    /// statement outside any transaction, where `SET LOCAL` is a warning and a
    /// no-op — and a pin that quietly does nothing is the failure it exists to
    /// prevent. Inside the transaction this opens they are undone by the
    /// rollback, and outside one they are on the connection pbps opened for
    /// this deployment.
    ///
    /// The isolation level is named, not inherited. The routine-pin checks
    /// inside the transaction read `pg_proc` with a plain `SELECT`, and under a
    /// `default_transaction_isolation` of `repeatable read` that `SELECT` kept
    /// returning the row as the transaction first saw it, while a call in the
    /// same transaction ran the body another session had just replaced
    /// (measured on 18.6, DEC-319.1). `READ COMMITTED` gives each check a fresh
    /// snapshot, so it sees what the statements ran.
    fn transaction_framing(&self, zone: &SessionZone) -> TransactionFraming {
        TransactionFraming {
            begin: format!(
                "{} BEGIN ISOLATION LEVEL READ COMMITTED;",
                session_pins(zone)
            )
            .into(),
            commit: "COMMIT;".into(),
            // Tolerates a transaction the server has already killed, so that
            // this statement's own error cannot replace the real failure.
            rollback: "ROLLBACK;".into(),
        }
    }

    /// Read only, because a declared expression can write here: measured on
    /// 18.6, `CHECK (nextval('s') > 0)` is accepted, a probe evaluating it
    /// advances the sequence once per row, and the advance survives the
    /// probe's rollback. Under `READ ONLY` the engine refuses `nextval` itself
    /// and an ordinary probe is untouched (DECISIONS 537). No pins here: the
    /// runner establishes them on the connection first, as plain `SET`s that
    /// outlive this transaction.
    fn probe_framing(&self) -> Option<TransactionFraming> {
        Some(TransactionFraming {
            begin: "BEGIN READ ONLY;".into(),
            commit: "COMMIT;".into(),
            rollback: "ROLLBACK;".into(),
        })
    }

    /// The same pins, for the mode that opens no transaction to carry them.
    ///
    /// This is the hole [`Dialect::transaction_framing`] names above: a staged
    /// apply runs statement by statement outside a transaction, so `begin` is
    /// never sent, and a `--resume` starts on a fresh connection partway
    /// through the plan. Every settings-dependent meaning in that doc comment
    /// applies to those statements too — a declared `CHECK (d >= '01/02/2026')`
    /// is a different constraint under `DMY` — so the pins are established on
    /// the connection instead. Plain `SET`, not `SET LOCAL`, for the reason
    /// given there: outside a transaction `SET LOCAL` is a warning and a no-op,
    /// and a pin that quietly does nothing is the failure it exists to prevent.
    fn session_pins(&self, zone: &SessionZone) -> Option<String> {
        Some(session_pins(zone))
    }

    /// None: `text` is PostgreSQL's ordinary unbounded string, not SQL
    /// Server's deprecated LOB type, and nothing this dialect accepts is
    /// deprecated by the engine (DEC-1869.1).
    fn deprecated_types(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        types::normalize(ty)
    }

    /// The partitions that carry a RANGE parent's series on (#1789,
    /// DEC-1789.1). The tiling, the calendar and the spelling are all in
    /// [`partition_series`].
    fn range_series(
        &self,
        request: &pbps_dialect::RangeSeriesRequest<'_>,
    ) -> Result<pbps_dialect::RangeSeries, DialectError> {
        partition_series::series(request)
    }

    fn records_conversions(&self) -> bool {
        true
    }

    fn retype_dependents(
        &self,
        from: &ColumnType,
        to: &ColumnType,
    ) -> pbps_dialect::RetypeDependents {
        types::retype_dependents(from, to)
    }

    fn foreign_key_types_compatible(
        &self,
        referencing: &ColumnType,
        referenced: &ColumnType,
    ) -> Option<bool> {
        types::foreign_key_types_compatible(referencing, referenced)
    }

    fn type_change_needs_using(&self, from: &ColumnType, to: &ColumnType) -> bool {
        // The three refusals `emit` makes for a change with no recorded
        // conversion, in one place so the prompt asks for exactly the changes
        // the emitter would refuse. A reference to a user-defined type is not
        // among them: no expression retypes it (DEC-1523.1).
        let (Ok(from), Ok(to)) = (types::normalize(from), types::normalize(to)) else {
            return false;
        };
        if from.type_ref().is_some() || to.type_ref().is_some() {
            return false;
        }
        let risk = types::change_risk(&from, &to);
        risk == TypeChangeRisk::Incompatible
            || (from.is_array() && to.is_array() && risk != TypeChangeRisk::Safe)
            || types::depends_on_the_session_time_zone(&from, &to)
    }

    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
        // Normalized again here, as the SQL Server dialect does and for the
        // same reason: the trait says the caller normalizes first, but a
        // dialect that only works when it is called correctly is a trap, and
        // normalizing twice is free.
        //
        // The caller that does not is `validate_saved_plan`, which re-derives
        // the risks of a plan file **because it may have been edited**, and an
        // edited file spells its types however the editor liked. Unnormalized,
        // `int -> integer` reads as `Incompatible` and blocks a plan that
        // changes nothing, and `character(5) -> character` reads as `Safe`
        // because an argument-free `character` looks unbounded — it is
        // `character(1)`, and that is a narrowing walking past the gate.
        //
        // A type that does not normalize ends the question here rather than
        // travelling on in its declared form. Falling back to the declared
        // value is the answer that looks conservative and is not: an
        // unknown *base* is claimed by no family and comes out `Incompatible`,
        // but a rejected *modifier* on a known base keeps that base, and the
        // family reads the modifier as if the engine would accept it —
        // `numeric(1000) -> numeric(1001)` comes out `Safe` on a precision
        // this engine does not have, and `interval(6) -> interval(7)` comes
        // out `Safe` on the precision it silently stores as 6, which is the
        // round trip the catalogue refuses the declaration for.
        let (Ok(from), Ok(to)) = (types::normalize(from), types::normalize(to)) else {
            return TypeChangeRisk::Incompatible;
        };
        types::change_risk(&from, &to)
    }

    /// Unquoted identifiers fold to **lower** case, where SQL Server folds to
    /// nothing at all — and only the ASCII letters fold. The first thing in
    /// this crate that is a real answer rather than a refusal, because the
    /// loader needs it before anything connects.
    ///
    /// Measured on PostgreSQL 18.6 with `server_encoding` UTF8:
    /// `CREATE TABLE AÄ` makes the relation **`aÄ`**, not `aä`. The server
    /// downcases byte by byte and leaves anything with the high bit set alone,
    /// so a Unicode-aware `to_lowercase` folds one character too many — and a
    /// name that folds differently from the engine's is a declaration
    /// introspection can never match. That is drift no apply can settle, and a
    /// `CREATE` that makes an object under a name nobody asked for.
    ///
    /// ASCII-only is also the conservative answer for the encodings this cannot
    /// see: the server's own rule is ASCII-only for every multibyte encoding,
    /// and the folding happens in the loader, where there is no connection to
    /// ask.
    fn fold_ident<'a>(&self, ident: &'a str) -> Cow<'a, str> {
        if ident.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Owned(ident.to_ascii_lowercase())
        } else {
            Cow::Borrowed(ident)
        }
    }

    fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
        quote(ident)
    }

    /// A partition's own checks, indexes and foreign keys (#1632), held to
    /// its parent's columns as a table's are to its own (#1577): the same
    /// structure, method and class rules, over the columns the partition has
    /// without declaring them.
    ///
    /// And an own check or foreign key is not named as one of the parent's
    /// checks or foreign keys, whose clones on the partition keep the parent's names: measured on
    /// 16 and 18, `ADD CONSTRAINT` under one of them is `constraint … already
    /// exists`. The names the engine chooses for the other clones (an index's,
    /// a key's, and on 18 a NOT NULL's) are the engine's to choose and are not
    /// guessed at here; one an own name takes is refused the same way, and the
    /// transaction with it (DEC-1577.1).
    fn validate_partition(
        &self,
        name: &TableName,
        partition: &Table,
        parent: &Table,
    ) -> Vec<DialectError> {
        self.validate_partition_with_sources(name, partition, parent, Default::default())
    }

    fn validate_partition_with_sources(
        &self,
        _name: &TableName,
        partition: &Table,
        parent: &Table,
        parent_sources: pbps_model::diagnostic::ColumnSources<'_>,
    ) -> Vec<DialectError> {
        // The parent's columns as names and types only: what the parent
        // declares of them is its own validation's to report, once.
        let columns = parent
            .columns
            .iter()
            .map(|(name, column)| {
                let column = pbps_model::Column {
                    collation: None,
                    ..column.clone()
                };
                (name.clone(), column)
            })
            .collect();
        let mut found = validate::table_structure(&Table {
            columns,
            checks: partition.checks.clone(),
            indexes: partition.indexes.clone(),
            foreign_keys: partition.foreign_keys.clone(),
            storage_parameters: partition.storage_parameters.clone(),
            ..Table::default()
        });
        // This entry point validates a partition's own fields without going
        // through validate_table_sources. A programmatic child must not lose
        // the same RLS refusal that its YAML shape already enforces.
        if partition.has_row_security() {
            found.push(DialectError::Invalid {
                dialect: types::DIALECT,
                message: "row-security flags and policies on a partition tree are not supported"
                    .to_owned(),
            });
        }
        let own = partition.checks.keys().map(|name| ("check", name)).chain(
            partition
                .foreign_keys
                .keys()
                .map(|name| ("foreign key", name)),
        );
        for (own, name) in own {
            let kind = if parent.checks.contains_key(name) {
                "check"
            } else if parent.foreign_keys.contains_key(name) {
                "foreign key"
            } else {
                continue;
            };
            found.push(DialectError::Invalid {
                dialect: crate::types::DIALECT,
                message: format!(
                    "{own} `{name}` is named as its parent's {kind}, which the partition already \
                     has under that name; name it otherwise",
                    name = name.shown(),
                ),
            });
        }
        // An own foreign key equal to one of the parent's: measured on 16 and
        // 18, the engine keeps it apart when it is added to a standing
        // partition, but takes it for the parent's clone when the partition is
        // attached or the parent's key is added after it, and it then reads
        // back as nothing of the partition's own. Whether it survives would
        // depend on the path, so it is refused (#1632).
        for (name, fk) in &partition.foreign_keys {
            if let Some((parents, _)) = parent.foreign_keys.iter().find(|(_, p)| *p == fk) {
                found.push(DialectError::Invalid {
                    dialect: crate::types::DIALECT,
                    message: format!(
                        "foreign key `{name}` is its parent's `{parents}` again, which the \
                         partition already has as a clone, and which the engine may take it \
                         for; declare it on the parent only",
                        name = name.shown(),
                        parents = parents.shown(),
                    ),
                });
            }
        }
        // Its own default and NOT NULL, each an override of one of the
        // parent's columns (#1578). One equal to the parent's is not an
        // override, and would be planned against a read-back that has none.
        // Measured on 16 and 18: a generated column refuses a default of the
        // partition's own. On 18 an identity refuses the post-create SET
        // DEFAULT this emitter uses; an inline default is accepted but does
        // not supply a direct insert's value. A serial's nextval is an
        // ordinary default, which a partition can override (DEC-2217.1).
        let own = partition
            .partition_of
            .as_ref()
            .map(|of| &of.columns)
            .into_iter()
            .flatten();
        for (column, own) in own {
            let invalid = |message: String| DialectError::Invalid {
                dialect: crate::types::DIALECT,
                message,
            };
            let Some(theirs) = parent.columns.get(column) else {
                found.push(invalid(format!(
                    "column `{column}` is not a column of its parent, whose columns are the \
                     partition's",
                    column = column.shown(),
                )));
                continue;
            };
            if own.not_null && !theirs.nullable {
                found.push(invalid(format!(
                    "column `{column}` is NOT NULL on its parent already, which the partition \
                     takes; leave out its `nullable: false`",
                    column = column.shown(),
                )));
            }
            let Some(default) = &own.default else {
                continue;
            };
            let (mut generated, mut identity) = (false, false);
            for source in parent_sources.get(column.raw(), theirs) {
                match source {
                    pbps_model::ValueSource::Default(parent_default) => {
                        if parent_default == default {
                            found.push(invalid(format!(
                                "column `{column}`'s default is its parent's, which the partition takes; leave out its `default:`",
                                column = column.shown(),
                            )));
                        }
                    }
                    pbps_model::ValueSource::Generated(_) => generated = true,
                    pbps_model::ValueSource::Identity(_) => identity = true,
                    pbps_model::ValueSource::None | pbps_model::ValueSource::Sequence(_) => {}
                }
            }
            // Preserve the diagnostic precedence from before the exclusive
            // model: equality first, then generation rather than identity.
            if generated || identity {
                let kind = if generated {
                    "generated"
                } else {
                    "an identity column"
                };
                found.push(invalid(format!(
                    "column `{column}` is {kind} on its parent, and takes no default of the partition's own",
                    column = column.shown(),
                )));
            }
            if crate::LEXICON.expression_in(default) == pbps_dialect::Expression::Absent {
                found.push(invalid(format!(
                    "column `{column}` has an empty default expression",
                    column = column.shown(),
                )));
            }
            // As a column's default is held, against the parent's column,
            // whose type the partition's is.
            if let Ok(normalized) = types::normalize(&theirs.ty) {
                found.extend(default_problems(column.raw(), theirs, &normalized, default));
            }
        }
        found
    }

    /// Every column's type, through the catalogue.
    ///
    /// A `serial` is refused **by name** here rather than by the same message
    /// the catalogue gives elsewhere, because this is where a user is looking
    /// at the declaration: the refusal names the column to change, and the one
    /// from `normalize_type` cannot.
    ///
    /// Returns every problem rather than the first — a schema with three
    /// unspellable columns should need one pass, not three.
    fn validate_table(&self, name: &TableName, table: &Table) -> Vec<DialectError> {
        self.validate_table_sources(name, table, Default::default())
    }

    fn validate_table_sources(
        &self,
        name: &TableName,
        table: &Table,
        sources: pbps_model::diagnostic::ColumnSources<'_>,
    ) -> Vec<DialectError> {
        // No declared type is known to a table alone, so none is admitted.
        self.validate_table_in_schema(name, table, sources, &pbps_model::Schema::default())
    }

    fn validate_table_in_schema(
        &self,
        name: &TableName,
        table: &Table,
        sources: pbps_model::diagnostic::ColumnSources<'_>,
        schema: &pbps_model::Schema,
    ) -> Vec<DialectError> {
        let mut found = Vec::new();
        // A partition's columns are its parent's, which it does not declare:
        // its checks and indexes are held to them by `validate_partition`.
        if table.partition_of.is_none() {
            found.extend(validate::table_structure(table));
        }
        if table.has_row_security() {
            let invalid = |message| DialectError::Invalid {
                dialect: types::DIALECT,
                message,
            };
            if table.data.is_some() {
                found.push(invalid(
                    "`data:` cannot be declared with row-security flags or policies, including disabled policies"
                        .to_owned(),
                ));
            }
            if table.partition_by.is_some() || table.partition_of.is_some() {
                found.push(invalid(
                    "row-security flags and policies on a partition tree are not supported"
                        .to_owned(),
                ));
            }
            for (policy_name, policy) in &table.row_policies {
                found.extend(policy.problems().into_iter().map(|problem| {
                    invalid(format!(
                        "row policy `{policy_name}`: {problem}",
                        policy_name = pbps_model::escape::Escaped(policy_name),
                    ))
                }));
                if let Err(error) = self.quote_ident(policy_name) {
                    found.push(error);
                }
                if let pbps_model::PolicyRoles::Roles(roles) = &policy.to {
                    for role in roles {
                        if let Err(error) = self.quote_ident(role.raw()) {
                            found.push(error);
                        }
                    }
                }
            }
        }
        // SQL Server's computed columns (#1174). PostgreSQL's are generated
        // columns, which declare their type (DEC-1168.1).
        for computed in table.computed.keys() {
            found.push(DialectError::Invalid {
                dialect: crate::types::DIALECT,
                message: format!(
                    "`{computed}` is a computed column, which this model holds for SQL Server; \
                     declare it as a column with `generated:` on PostgreSQL",
                    computed = computed.shown(),
                ),
            });
        }
        // A row's values are scalars, and an array's equality includes its
        // bounds (`'[0:1]={1,2}'` is not `'{1,2}'`): rows are not held in a
        // table with an array column until a codec for them is (#1167).
        if table.data.is_some() {
            for (column, spec) in &table.columns {
                if spec.ty.type_ref().is_some() {
                    found.push(DialectError::InvalidVerbatim {
                        dialect: types::DIALECT,
                        message: format!(
                            "`data:` cannot be declared on a table with the user-defined type reference `{}` on column `{column}`",
                            spec.ty.shown(),
                            column = column.shown(),
                        ),
                    });
                }
            }
            for (column, spec) in &table.columns {
                if spec.ty.is_array() {
                    found.push(DialectError::Invalid {
                        dialect: crate::types::DIALECT,
                        message: format!(
                            "`data:` cannot be declared on a table with an array column, and \
                             `{column}` is `{}`; declare the rows elsewhere, or leave the block \
                             out",
                            spec.ty.shown(),
                            column = column.shown(),
                        ),
                    });
                }
            }
        }
        found.extend(partition_problems(table, &schema.types));
        found.extend(bound_label_problems(name, table, schema));
        // SQL Server's (#1176). PostgreSQL has no system-versioned table.
        if table.system_time.is_some() {
            found.push(DialectError::Invalid {
                dialect: crate::types::DIALECT,
                message: "`system_time` is SQL Server's `PERIOD FOR SYSTEM_TIME` and system \
                          versioning; PostgreSQL has neither, so remove the block"
                    .to_owned(),
            });
        }
        // The names first, and this is not a formality: `quote_ident` refuses
        // an identifier over [`MAX_IDENT_BYTES`], so a table this method called
        // clean is one the emitter cannot spell. `validate` is the command that
        // exists to say so before anything connects.
        for part in [&name.schema, &name.name] {
            if let Err(e) = self.quote_ident(part) {
                found.push(e);
            }
        }
        // A schema the *reader* excludes, refused here rather than left to an
        // engine that will not object. The pull skips `pg_catalog`,
        // `information_schema` and every name beginning with `pg_`
        // (`catalog.rs`), so a table declared in one is created and then
        // invisible: absent from the pulled schema, planned again as a
        // `CREATE` the engine refuses for already existing.
        //
        // `pg_temp` is the one that is not merely invisible. **Measured**, it
        // is the parser's alias for the session's temporary schema, so
        // `CREATE TABLE "pg_temp"."t"` succeeds and leaves `pg_temp_58.t` with
        // `relpersistence = 't'` — a session-local table under a name the
        // declaration never wrote, gone when the connection closes. That is
        // the silent rewrite DECISIONS 266 wrote this class of rule for: an
        // engine that refuses by name can be left to refuse, and one that
        // hands back something else cannot.
        let schema = name.schema.as_str();
        // The other name a schema cannot usefully have here, and it fails
        // somewhere else entirely: `$user` is what this engine substitutes for
        // the current role's own schema inside a `search_path`, quoted or not
        // (`emit::NOT_A_SCHEMA_A_PATH_CAN_NAME`). The table would be created —
        // its statements name it in full — and every unqualified name inside a
        // check, a filter or a default would bind through the deployment
        // role's schema instead of this one.
        if schema == emit::NOT_A_SCHEMA_A_PATH_CAN_NAME {
            found.push(DialectError::Invalid {
                dialect: types::DIALECT,
                message: format!(
                    "table `{name}` is declared in a schema named `{schema}`, which this engine \
                     reads as the current role's own schema wherever a `search_path` names it -- \
                     quoting does not make it literal. The table would be created and then \
                     every unqualified name in its checks, filters and defaults would resolve \
                     through whatever schema the deploying role owns. Declare it under a name \
                     the path can carry.",
                    name = name.shown(),
                    schema = pbps_model::escape::Escaped(schema),
                ),
            });
        }
        if !catalog::a_projects_schema(schema) {
            found.push(DialectError::Invalid {
                dialect: types::DIALECT,
                message: format!(
                    "table `{name}` is declared in `{schema}`, which this dialect's pull \
                     never reads: `pg_catalog`, `information_schema` and every schema whose \
                     name begins with `pg_` are excluded from the managed set. The engine \
                     would create the table and no plan could ever see it again -- and \
                     `pg_temp` is worse than invisible, because it is this engine's alias for \
                     the session's temporary schema: measured, `CREATE TABLE \"pg_temp\".\"t\"` \
                     leaves a `pg_temp_58.t` that disappears with the connection. Declare the \
                     table in a schema of the project's own.",
                    name = name.shown(),
                    schema = pbps_model::escape::Escaped(schema),
                ),
            });
        }
        // The pull and validation share the ledger's qualified identity.
        // Same-named tables elsewhere belong to the project (DECISIONS 284).
        if catalog::is_ours(name) {
            found.push(DialectError::Invalid {
                dialect: types::DIALECT,
                message: format!(
                    "table `{name}` is one of the ledger tables this tool owns (SPEC section 8.1). \
                     Only those names in schema `{}` are reserved; the same table name in \
                     another project schema is read back normally.",
                    crate::state::LEDGER_SCHEMA,
                    name = name.shown()
                ),
            });
        }
        // Every other name the table owns, which the engine truncates at the
        // same limit and which the emitter has to spell just as often: the
        // primary key's, and the keys of the four maps.
        let owned = table
            .primary_key
            .as_ref()
            .and_then(|pk| pk.name.as_ref().map(pbps_model::ObjectLocalName::raw))
            .into_iter()
            .chain(table.unique.keys().map(pbps_model::ObjectLocalName::raw))
            .chain(
                table
                    .foreign_keys
                    .keys()
                    .map(pbps_model::ObjectLocalName::raw),
            )
            .chain(table.checks.keys().map(pbps_model::ObjectLocalName::raw))
            .chain(table.indexes.keys().map(pbps_model::ObjectLocalName::raw));
        for object in owned {
            if let Err(e) = self.quote_ident(object) {
                found.push(e);
            }
        }

        // A primary key column that the declaration calls nullable. SQL Server
        // refuses this at `CREATE`; **measured, this engine does not** — it
        // accepts the table and sets `NOT NULL` itself, so the declaration and
        // the database disagree from the moment the table exists. The pull then
        // reads `nullable: false`, every plan proposes `DROP NOT NULL`, and the
        // engine refuses that with `column "id" is in a primary key`: a plan
        // that can never converge and can never succeed. Refused here, where a
        // user is looking at the declaration, and worded as the other dialect
        // words it because it is the same mistake.
        if let Some(pk) = &table.primary_key {
            for column in &pk.columns {
                if table.columns.get(column).is_some_and(|c| c.nullable) {
                    found.push(DialectError::Invalid {
                        dialect: types::DIALECT,
                        message: format!(
                            "primary key column `{column}` is nullable; a primary key column must \
                             be NOT NULL. This engine does not refuse the table -- it sets \
                             `NOT NULL` for you -- and then no plan can ever make the column match \
                             the declaration again.",
                            column = column.shown(),
                        ),
                    });
                }
            }
        }

        for (column_name, column) in &table.columns {
            if let Err(e) = self.quote_ident(column_name.raw()) {
                found.push(e);
            }
            if let Some(named) = types::refuse_serial(&column.ty, Some(column_name.raw())) {
                found.push(named);
                // Everything below asks a question about the type, and asking
                // it of one the catalogue has already refused is noise on top
                // of the real error.
                continue;
            }
            let normalized = match types::normalize(&column.ty) {
                Ok(ty) => ty,
                Err(e) => {
                    found.push(e);
                    continue;
                }
            };
            let supplied = sources.get(column_name.raw(), column);
            // Sources retain YAML's canonical default/identity/serial/generated
            // order. A valid model supplies one; diagnostic recovery may supply
            // several.
            let (mut has_default, mut has_identity, mut has_sequence) = (false, false, false);
            for source in supplied {
                match source {
                    pbps_model::ValueSource::None => {}
                    pbps_model::ValueSource::Default(_) => has_default = true,
                    pbps_model::ValueSource::Identity(_) => has_identity = true,
                    pbps_model::ValueSource::Sequence(_) => has_sequence = true,
                    pbps_model::ValueSource::Generated(_) => {}
                }
            }
            let invalid = |message| DialectError::Invalid {
                dialect: types::DIALECT,
                message,
            };
            for source in supplied {
                match source {
                    pbps_model::ValueSource::None => {}
                    pbps_model::ValueSource::Default(expr) => {
                        found.extend(default_problems(
                            column_name.raw(),
                            column,
                            &normalized,
                            expr,
                        ));
                    }
                    pbps_model::ValueSource::Identity(identity) => {
                        found.extend(identity_problems(column_name.raw(), column, identity));
                        if identity.options.cycle {
                            found.extend(cycle_problems(table, column_name.raw()));
                        }
                        if has_default {
                            found.push(invalid(format!(
                                "column `{column_name}` has an `identity:` and a `default:`. The engine refuses both on one column, and the identity is the one that supplies the value.",
                                column_name = column_name.shown(),
                            )));
                        }
                    }
                    // A `serial`'s default is its sequence's `nextval`, so it
                    // takes no other source (#1631).
                    pbps_model::ValueSource::Sequence(sequence) => {
                        found.extend(sequence_problems(
                            name,
                            table,
                            column_name.raw(),
                            column,
                            sequence,
                        ));
                        if sequence.options.cycle {
                            found.extend(cycle_problems(table, column_name.raw()));
                        }
                        if has_default {
                            found.push(invalid(format!(
                                "column `{column_name}` is a `serial` and has a `default:`; its default is its sequence's `nextval`",
                                column_name = column_name.shown(),
                            )));
                        }
                        if has_identity {
                            found.push(invalid(format!(
                                "column `{column_name}` is both a `serial` and an identity; the engine refuses an identity on a column that has a default",
                                column_name = column_name.shown(),
                            )));
                        }
                    }
                    pbps_model::ValueSource::Generated(generated) => {
                        if has_sequence {
                            found.push(invalid(format!(
                                "column `{column_name}` is both a `serial` and a generated column",
                                column_name = column_name.shown(),
                            )));
                        }
                        if has_default {
                            found.push(invalid(format!(
                                "column `{column_name}` has both a default and a generation expression; a generated column is computed on every write, and a default would never apply",
                                column_name = column_name.shown(),
                            )));
                        }
                        if has_identity {
                            found.push(invalid(format!(
                                "column `{column_name}` is both an identity and a generated column",
                                column_name = column_name.shown(),
                            )));
                        }
                        found.extend(generation_problems(column_name.raw(), generated));
                    }
                }
            }
        }
        // Reference data: the rules whose answer is this engine's, ADR-0013 §2
        // among them. The model's own rules — a row's key is its identity in
        // every dialect — are `pbps_model::data::check`'s.
        found.extend(rows::data_problems(name, table, sources));
        found
    }

    /// Views share `pg_class` with tables; nothing else does.
    ///
    /// **Measured on 18.6**, all four halves of it:
    ///
    /// ```text
    /// CREATE TABLE ns.x (...); CREATE VIEW ns.x AS ...
    ///     refused: relation "x" already exists
    /// CREATE TABLE ns.y (...); CREATE FUNCTION ns.y() ...       accepted
    /// CREATE TABLE ns.z (...); CREATE PROCEDURE ns.z() ...      accepted
    /// a trigger is named within its table, not within a schema  (ADR-0009 §1)
    /// ```
    ///
    /// A function and a procedure of one identity *do* collide with each other,
    /// which is not this question: they are two modules, not a module and a
    /// table, and [`pbps_dialect::check_module_names`] asks about them under
    /// [`Self::overloads`].
    fn shares_namespace_with_tables(&self, kind: ModuleKind) -> bool {
        match kind {
            ModuleKind::View => true,
            ModuleKind::Function | ModuleKind::Procedure | ModuleKind::Trigger => false,
        }
    }

    /// Routines overload; views and triggers do not.
    ///
    /// This is the first `true` any dialect returns here — SQL Server answers
    /// `false` for every kind — so it is the first time a declaration is
    /// *required* to carry a signature. **Measured**, both directions:
    ///
    /// ```text
    /// CREATE FUNCTION ov.f(int) ...; CREATE FUNCTION ov.f(text) ...   two objects
    /// CREATE VIEW ov.v AS ...;       CREATE VIEW ov.v AS ...
    ///     refused: relation "v" already exists
    /// two triggers named `audit`, one per table                       two objects
    /// ```
    ///
    /// A trigger is `false` because its overload-looking freedom is already in
    /// its identity: `ModuleId::Trigger` holds the table, so two `audit`
    /// triggers on two tables are two keys without anything overloading.
    fn overloads(&self, kind: ModuleKind) -> bool {
        match kind {
            ModuleKind::Function | ModuleKind::Procedure => true,
            ModuleKind::View | ModuleKind::Trigger => false,
        }
    }

    /// An index lives in `pg_class` beside tables and views, not in a
    /// per-table namespace.
    ///
    /// **Measured on 18.6** (issue #176): two tables in one schema each
    /// declaring `CREATE INDEX ix_n`, an index named after a table, a unique
    /// constraint sharing a name with an index, and a unique constraint named
    /// after a table are all refused with `relation "..." already exists` —
    /// the constraint cases because a named primary key or unique constraint
    /// is backed by an index of that name.
    fn relations_have_row_types(&self) -> bool {
        true
    }

    fn indexes_share_namespace_with_tables(&self) -> bool {
        true
    }

    /// A check or foreign-key name is per table here, and the index behind a
    /// primary key or unique constraint is the relation-namespace question
    /// above (issue #496; measured on 18.6, two tables in one schema each
    /// declaring `CONSTRAINT c CHECK (…)`, and a third declaring a foreign key
    /// `c`, are all created).
    fn constraints_share_namespace_with_tables(&self) -> bool {
        false
    }

    /// Identifiers are compared byte for byte, and pbps quotes every one it
    /// writes, so two spellings are two names (#1964).
    fn compares_names_exactly(&self) -> bool {
        true
    }

    /// The index behind an unnamed primary key, `<table>_pkey`, and the
    /// sequence behind each identity column, `<table>_<column>_seq`: the
    /// names the engine generates for what a `CREATE TABLE` implies (#465,
    /// DEC-465.1). A unique constraint is always named in a declaration, so
    /// its generated name cannot arise. A `serial` column's sequence is
    /// created by name (`emit::owned_sequence`), with no engine retry. Its
    /// declared name is a fixed claim in `check_index_names`; an unnamed
    /// one's actual name is the connected plan's to claim (#1899).
    fn owned_sequence_name(
        &self,
        table: &TableName,
        column: &str,
        sequence: &pbps_model::OwnedSequence,
    ) -> Option<String> {
        Some(emit::owned_sequence(table, column, sequence).name)
    }

    fn owned_sequence_default(
        &self,
        table: &TableName,
        column: &str,
        sequence: &pbps_model::OwnedSequence,
    ) -> Result<String, DialectError> {
        emit::owned_sequence_default(table, column, sequence)
    }

    fn implicit_relation_names(
        &self,
        name: &TableName,
        table: &Table,
    ) -> Vec<pbps_dialect::ImplicitRelation> {
        self.implicit_relation_names_with_sources(name, table, Default::default())
    }

    fn implicit_relation_names_with_sources(
        &self,
        name: &TableName,
        table: &Table,
        sources: pbps_model::diagnostic::ColumnSources<'_>,
    ) -> Vec<pbps_dialect::ImplicitRelation> {
        let mut out = Vec::new();
        if table
            .primary_key
            .as_ref()
            .is_some_and(|pk| pk.name.is_none())
        {
            out.push(pbps_dialect::ImplicitRelation {
                name: generated_name(&name.name, None, "pkey"),
                descriptor: format!(
                    "the index {} generates for `{name}`'s unnamed primary key",
                    types::DIALECT,
                    name = name.shown()
                ),
                remedy: "Name the primary key, or rename the other object.",
                // `ChooseRelationName(..., isconstraint = true)` (#1112).
                yields_to_constraints: true,
            });
        }
        for (column, spec) in &table.columns {
            if sources
                .get(column.raw(), spec)
                .iter()
                .any(|source| match source {
                    pbps_model::ValueSource::Identity(_) => true,
                    // Declared sequence names are fixed claims, not engine
                    // choices with a fallback (DEC-1898.1).
                    pbps_model::ValueSource::None
                    | pbps_model::ValueSource::Default(_)
                    | pbps_model::ValueSource::Sequence(_)
                    | pbps_model::ValueSource::Generated(_) => false,
                })
            {
                // Named after the table and the column: naming the key moves
                // nothing here (#990). Renaming the column does not either
                // once the sequence exists: `RENAME COLUMN` keeps the owned
                // sequence's name, so offline validation would predict a new
                // name that apply never gets.
                out.push(pbps_dialect::ImplicitRelation {
                    name: generated_name(&name.name, Some(column.raw()), "seq"),
                    descriptor: format!(
                        "the sequence {} generates for identity column `{name}.{column}`",
                        types::DIALECT,
                        name = name.shown(),
                        column = column.shown(),
                    ),
                    remedy: "Rename the other object.",
                    yields_to_constraints: false,
                });
            }
        }
        out
    }

    /// A constraint trigger has a `pg_constraint` row under its own name, in
    /// its table's schema, which a primary key's index retry finds taken
    /// (#1112). An ordinary trigger has none.
    fn declares_a_constraint(&self, module: &pbps_model::Module) -> bool {
        emit::is_constraint_trigger(module)
    }

    /// A row trigger on a partitioned table, measured on 16 and 18; a
    /// statement trigger is not cloned (#1632).
    fn clones_to_partitions(&self, trigger: &pbps_model::Module) -> bool {
        emit::is_row_trigger(trigger)
    }

    /// `ChooseRelationName` retries with the number on the *label*, and cuts
    /// the table name again to fit it. Measured on 18.6, three 60-byte tables
    /// that generate one `_pkey` get `…_pkey`, then a 57-byte cut with
    /// `_pkey1` and `_pkey2`; two identity columns, `…_id_seq` and
    /// `…_id_seq1` (#987).
    fn implicit_relation_fallbacks(
        &self,
        name: &TableName,
        table: &Table,
        suffix: u32,
    ) -> Vec<String> {
        self.implicit_relation_fallbacks_with_sources(name, table, suffix, Default::default())
    }

    fn implicit_relation_fallbacks_with_sources(
        &self,
        name: &TableName,
        table: &Table,
        suffix: u32,
        sources: pbps_model::diagnostic::ColumnSources<'_>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        if table
            .primary_key
            .as_ref()
            .is_some_and(|pk| pk.name.is_none())
        {
            out.push(generated_name(&name.name, None, &format!("pkey{suffix}")));
        }
        for (column, spec) in &table.columns {
            if sources
                .get(column.raw(), spec)
                .iter()
                .any(|source| match source {
                    pbps_model::ValueSource::Identity(_) => true,
                    // Created by name, so the engine never retries one.
                    pbps_model::ValueSource::None
                    | pbps_model::ValueSource::Default(_)
                    | pbps_model::ValueSource::Sequence(_)
                    | pbps_model::ValueSource::Generated(_) => false,
                })
            {
                out.push(generated_name(
                    &name.name,
                    Some(column.raw()),
                    &format!("seq{suffix}"),
                ));
            }
        }
        out
    }

    /// The project-schema portion of the write path: the object's own schema
    /// first, then the configured extras in order (DECISIONS 276 and 464).
    /// The implicit catalog and final temporary schema hold no declared
    /// candidates. A bare project name resolves to the *first* entry that
    /// holds one: measured,
    /// with `z.p` and `a.p` both present, a bare `p` under `SET search_path =
    /// "z", "a"` binds `z.p`, and under `"a", "z"` binds `a.p`. With no
    /// extras, `a.x AS SELECT * FROM b.z` over `b.z AS SELECT 1 AS x` is a
    /// valid plan, and the alias `x` read as a mention of `a.x` closed a
    /// cycle that put `a.x` first (317).
    fn bare_name_rank(&self, from: &str, to: &str) -> Option<usize> {
        if from == to {
            return Some(0);
        }
        self.write_path_extras
            .iter()
            .position(|extra| extra == to)
            .map(|at| at + 1)
    }

    fn rebound_modules(
        &self,
        declared: &Schema,
        arriving: &[ModuleId],
        already_changed: &std::collections::BTreeSet<ModuleId>,
    ) -> std::collections::BTreeSet<ModuleId> {
        modules::rebound_by_this_plan(
            declared,
            self.write_path_extras(),
            arriving,
            already_changed,
        )
        .into_iter()
        .map(|rebound| rebound.module)
        .collect()
    }

    /// The spelling this engine puts in a routine's identity (ADR-0009 §1).
    ///
    /// The canonical form is what `format_type` prints **under the empty
    /// search path**, which is the path every read here pins (DECISIONS 253):
    /// a built-in bare, a user type schema-qualified. **Measured on 18.6**, one
    /// function's twelve parameters:
    ///
    /// ```text
    /// declared   m2.money_amount  m2.mood  timestamp(3) with time zone  varchar(10)
    /// identity   m2.money_amount  m2.mood  timestamp with time zone     character varying
    ///
    /// declared   char       numeric(10,2)  int4     bit varying(4)  double precision[]
    /// identity   character  numeric        integer  bit varying     double precision[]
    ///
    /// declared   "char"  text[][]  interval hour to minute
    /// identity   "char"  text[]    interval
    /// ```
    ///
    /// Three rules come out of that, and they are applied in this order:
    ///
    /// 1. **A modifier is discarded.** `f(varchar(10))` and `f(varchar(20))`
    ///    are one function, so keying them as two modules would name a
    ///    signature the engine resolves to something else in every `DROP` and
    ///    every `GRANT`. This is the whole reason the hook is not
    ///    [`Dialect::normalize_type`], which keeps them on purpose.
    /// 2. **An array collapses to one `[]`.** `text[][]` is `text[]`;
    ///    PostgreSQL does not record a dimension count.
    /// 3. **Everything else is the catalogue's**, so `int4` becomes `integer`
    ///    and `char` becomes `character` by the same table a column uses.
    ///
    /// # What it passes through, and why that is not a gap
    ///
    /// A spelling the catalogue does not know — a domain, an enum, `"char"`,
    /// a pseudo-type — is returned **unchanged**. Refusing it would refuse
    /// ADR-0009 §1's own example, and the model has no way to tell a user type
    /// this dialect has never heard of from a mistake. The user writes what
    /// `pull` showed them, which is the engine's own text; a spelling that
    /// disagrees produces a `CREATE` the engine refuses or an object the next
    /// plan reports as one to drop and one to add, and ADR-0009 §1 accepts
    /// exactly that bargain: *"Both are loud, both are inside the plan's
    /// transaction, and neither is silent."*
    fn normalize_routine_arg(&self, arg: &RoutineArg) -> Result<RoutineArg, DialectError> {
        Ok(types::routine_arg(arg))
    }

    /// What a module declaration must satisfy before anything connects.
    ///
    /// Returns every problem rather than the first, for the reason
    /// [`Dialect::validate_table`] does: a schema with three unspellable
    /// modules should need one pass.
    fn module_matches_declaration(&self, wrote: &Module, now: &Module) -> bool {
        // pg_get_viewdef/pg_get_functiondef reconstruct text from the catalog;
        // added qualification and rendering are not an out-of-band rewrite.
        // Descriptions are not database state. Kind and presence still are
        // (SPEC §7.6); unchanged bodies compare catalog text against itself.
        wrote.kind == now.kind
    }

    fn validate_module(&self, id: &ModuleId, module: &Module) -> Vec<DialectError> {
        emit::validate_module(id, module)
    }

    /// **No.** A PostgreSQL role is a cluster object: `pg_authid` is shared,
    /// the role is visible from every database of the cluster and granted in
    /// each, and a `DROP ROLE` is refused by a dependency in a database this
    /// connection cannot see — measured, with every grant this database holds
    /// revoked, `DROP ROLE gr_reader` is still
    /// `role "gr_reader" cannot be dropped because some objects depend on it`
    /// / `DETAIL: 1 object in database otherdb`.
    ///
    /// A tool whose blast radius is one database must not own an object whose
    /// blast radius is the cluster (ADR-0010 §3, DECISIONS 211). So a declared
    /// role the cluster lacks is refused with the `CREATE ROLE` to run by
    /// hand, and a dropped role has its declared grants revoked and is left
    /// standing for a human. What the role *holds in this database* is managed
    /// either way: this answer is about the principal, not about its grants.
    fn manages_roles(&self) -> bool {
        false
    }

    fn implied_grants(&self, schema: &mut Schema) {
        pbps_model::role::implied_serial_grants(schema);
    }

    /// **Yes** (#1889): an identity's start and increment are `SET` in place,
    /// an owned sequence's are `ALTER SEQUENCE`, and either is put on or
    /// taken off a column with `ALTER COLUMN`, measured on 16 and 18.
    fn changes_numbering(&self) -> bool {
        true
    }

    /// **Yes.** Every module edit on this engine is a `DROP` and a `CREATE`
    /// (ADR-0009 §3), so an object about to be rebuilt has no ACL until the
    /// declared grants on it are re-emitted after the `CREATE` — the same
    /// state a `DropModule` leaves it in, and `diff_roles` compares against
    /// it the same way (#248). `crate::modules::before_a_rebuild` is the
    /// connected half: it still refuses whatever the declarations cannot
    /// reproduce (an ungranted role, `PUBLIC`, `WITH GRANT OPTION`, a column
    /// grant), and this only stops it refusing a grant this plan is about to
    /// restate anyway.
    fn rebuilds_modules(&self) -> bool {
        true
    }

    fn symbolic_facts(
        &self,
        before: pbps_model::operation::Snapshot<'_>,
        after: pbps_model::operation::Snapshot<'_>,
        stored: &pbps_model::operation::MembershipCatalog,
    ) -> pbps_model::operation::symbolic::Facts {
        engine_effects::facts::of(self, stored, before, after)
    }

    /// What PostgreSQL's catalog does beyond the portable effects
    /// (DEC-1954.3):
    ///
    /// - a check or filtered index that may name a column stands in the way
    ///   of dropping it;
    /// - `ALTER TABLE ... SET SCHEMA` carries the table's indexes and keys
    ///   with it, so they are rewritten by it (DEC-1767.1). Owned sequences
    ///   move too; they have no key, and reach the name slots as catalog
    ///   occupants that go with their table (DEC-1964.2).
    fn engine_effects(
        &self,
        op: &pbps_model::operation::Operation,
        owners: &dyn pbps_model::operation::Owners,
        before: pbps_model::operation::Snapshot<'_>,
        after: pbps_model::operation::Snapshot<'_>,
        _catalog: Option<&pbps_dialect::slots::CatalogFacts>,
    ) -> Result<pbps_model::operation::Effects, pbps_model::operation::OwnerError> {
        use pbps_model::Change;
        use pbps_model::operation::{Incarnation, PartKey, Resource, base_table};
        let mut fx =
            pbps_dialect::engine_effects::text_dependents(self, op, owners, before, after, false)?
                .merged(pbps_dialect::engine_effects::retype_blockers(
                    self, op, owners, before, after,
                )?);
        if let Change::RenameTable { from, to, .. } = op.change()
            && from.schema != to.schema
            && let Some(base) = base_table(op, owners, before, from)?
        {
            let t = base.table;
            let mut parts: Vec<PartKey> = Vec::new();
            if t.primary_key.is_some() {
                parts.push(PartKey::PrimaryKey);
            }
            parts.extend(t.unique.keys().cloned().map(PartKey::Unique));
            parts.extend(t.indexes.keys().cloned().map(PartKey::Index));
            for part in parts {
                fx.rewrites
                    .push(Resource::Object(base.part(part), Incarnation::Final));
            }
        }
        // A kept identity retuned goes before a retype that narrows its
        // column, and after one that does not: the engine moves an identity's
        // sequence with its column and refuses the retype while a bound set
        // for the wider type stands, but a bound set for the wider type is
        // refused on the narrower column (DEC-1921.1, measured on 16 and 18).
        // The portable effects leave the retune free of the column's
        // rewrites; this puts it after them unless the retype narrows.
        if let Change::AlterColumnNumbering {
            uid,
            from: pbps_model::ValueSource::Identity(_),
            to: pbps_model::ValueSource::Identity(_),
            ..
        } = op.change()
            && let Some((was, will)) = pbps_model::operation::column_types(uid, before, after)
            && self.type_change_risk(was, will) == TypeChangeRisk::Safe
        {
            fx.requires.push(pbps_model::operation::Requirement::Exists(
                pbps_model::operation::column_key(uid),
            ));
        }
        Ok(fx)
    }

    /// **Yes, and it is the reason this method exists.** Measured on 18.6: a
    /// function created with no grant at all has `proacl IS NULL`, which is
    /// not "granted to nobody" but "the built-in default applies", and that
    /// default is `EXECUTE` to `PUBLIC` (ADR-0010 §5). A `SET ROLE` to a
    /// principal holding nothing but `USAGE` on the schema can call it — and
    /// where the routine is `SECURITY DEFINER`, call it as its owner.
    fn creates_public_executable_routines(&self) -> bool {
        true
    }

    /// Every problem with a role, before anything connects (ADR-0010 §1, §2
    /// and §6). See [`validate::role`].
    fn validate_type(
        &self,
        name: &pbps_model::TypeRef,
        definition: &pbps_model::ManagedType,
    ) -> Vec<DialectError> {
        let mut errors = Vec::new();
        if let Err(error) = self.normalize_type(&ColumnType::reference(name.clone())) {
            errors.push(error);
        }
        if !catalog::a_projects_schema(name.schema()) {
            errors.push(DialectError::Invalid {
                dialect: self.name(),
                message: "a declared type must belong to a project schema".into(),
            });
        }
        if let pbps_model::ManagedType::Domain { base, .. } = definition
            && let Err(error) = self.normalize_type(base)
        {
            errors.push(error);
        }
        errors
    }

    fn validate_role(
        &self,
        name: &pbps_model::RoleName,
        role: &Role,
        schema: &Schema,
    ) -> Vec<DialectError> {
        validate::role(name, role, schema)
    }

    fn intermediate_tables(
        &self,
        change: &Change,
        strategy: Strategy,
    ) -> Result<Vec<TableName>, DialectError> {
        Ok(pbps_dialect::lower::intermediate_tables(&emit::emit(
            self, change, strategy,
        )?))
    }

    fn intermediate_types(
        &self,
        change: &Change,
        strategy: Strategy,
    ) -> Result<Vec<pbps_model::TypeRef>, DialectError> {
        let statements = emit::emit(self, change, strategy)?;
        Ok(statements
            .iter()
            .take(statements.len().saturating_sub(1))
            .filter_map(|s| s.declared_type.as_ref().map(|t| t.name.clone()))
            .collect())
    }

    /// What this plan implies about the data, asked before its first
    /// statement (SPEC §7.5).
    ///
    /// A probe list is never a promise that everything was checked: the cases
    /// with no probe are named in [`preflight`]'s own documentation, and the
    /// caller reports how many could not be checked rather than counting them
    /// as passes.
    fn preflight(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> pbps_dialect::Preflight {
        preflight::probes(names, &pbps_model::Partitions::default())
    }

    fn preflight_with(
        &self,
        names: &pbps_model::plan_names::PlanNames<'_>,
        partitions: &pbps_model::Partitions,
    ) -> pbps_dialect::Preflight {
        preflight::probes(names, partitions)
    }

    /// Whether an omitted cell in the read-back *means* at-default for this
    /// column — the same question the row reader asks when it builds the
    /// query, asked once so the two cannot drift (DECISIONS 191).
    fn reads_back_at_default(&self, column: &pbps_model::Column) -> bool {
        rows::confirms_default(column)
    }

    fn declaration_notes(&self, schema: &Schema) -> Vec<String> {
        let mut notes = rows::not_checked_offline(schema);
        notes.extend(cycle_expression_notes(schema));
        notes
    }
}

/// The name this engine gives the index behind an unnamed primary key of
/// `table`: what a plan that drops that key frees (#951).
#[must_use]
pub fn implicit_primary_key_name(table: &str) -> String {
    generated_name(table, None, "pkey")
}

/// The name the engine tries for `table`'s unnamed primary key's index once
/// `n` earlier candidates are held: `n == 0` is [`implicit_primary_key_name`],
/// then `<table>_pkey1`, `<table>_pkey2`…, the table's part cut again to fit
/// each (`ChooseRelationName`). Measured on 16 and 18 (#1645): twelve 61-byte
/// tables keep 58 bytes for `_pkey`, 57 for `_pkey1` to `_pkey9`, and 56 for
/// `_pkey10` and `_pkey11`.
pub fn implicit_primary_key_fallback(table: &str, n: u32) -> String {
    if n == 0 {
        implicit_primary_key_name(table)
    } else {
        generated_name(table, None, &format!("pkey{n}"))
    }
}

/// The name the engine generates for an implicit relation: `makeObjectName`
/// in PostgreSQL's `src/backend/commands/indexcmds.c`, ported and measured on
/// 18.6 (#465, DEC-465.1).
///
/// `<name1>_<name2>_<label>`, or `<name1>_<label>` without a `name2`, fitted
/// into `NAMEDATALEN - 1` = 63 bytes by shortening the longer of the two names
/// a byte at a time and then cutting each at a character boundary. Measured:
/// a 60-byte table gives `…ffffff_id_seq` with the table cut to 56 bytes, and a
/// 62-byte table of two-byte characters is cut to 58 and 56 bytes for `_pkey`
/// and `_id_seq`. A name the engine finds taken gets a numeric suffix instead
/// (`ChooseRelationName`); that is the order-dependent case the caller refuses
/// rather than predicts.
pub(crate) fn generated_name(name1: &str, name2: Option<&str>, label: &str) -> String {
    const MAX: usize = 63;
    let overhead = label.len() + 1 + usize::from(name2.is_some());
    let available = MAX - overhead;
    let mut n1 = name1.len();
    let mut n2 = name2.map_or(0, str::len);
    while n1 + n2 > available {
        if n1 > n2 {
            n1 -= 1;
        } else {
            n2 -= 1;
        }
    }
    let clip = |s: &str, mut n: usize| {
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s[..n].to_owned()
    };
    let mut out = clip(name1, n1);
    if let Some(name2) = name2 {
        out.push('_');
        out.push_str(&clip(name2, n2));
    }
    out.push('_');
    out.push_str(label);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn same_engine_continuations_keep_payload_sources_sql_and_checksum() {
        use pbps_dialect::{PlanDialect, PlanPayload};
        use pbps_diff::order::deployment::{
            Sources,
            schedule::{Declarations, Facts, schedule},
        };
        use pbps_diff::order::{CanonicalPlan, PlanView};
        use pbps_model::{IdsFile, PlanMode, SavedPlan, Schema};

        fn check<D: PlanDialect<Plan = dyn PlanView<D>> + 'static>(dialect: &D) {
            let before = Schema::default();
            let before_ids = IdsFile::default();
            let mut after = Schema::default();
            let name = pbps_model::TableName::new("app", "thing");
            let mut table = pbps_model::Table::default();
            table.columns.insert(
                "n".into(),
                pbps_model::Column::new("integer".parse().unwrap()),
            );
            after.tables.insert(name.clone(), table);
            let mut ids = IdsFile::default();
            ids.tables.insert("t_aaaaaa".parse().unwrap(), name.clone());
            ids.columns
                .insert("c_aaaaaa".parse().unwrap(), name.column("n"));
            let declarations = Declarations {
                schema: &after,
                sources: Sources {
                    before: pbps_diff::Side {
                        schema: &before,
                        ids: &before_ids,
                    },
                    after: &ids,
                },
                module_deps: &Default::default(),
            };
            let diff = pbps_diff::diff(
                declarations.sources.before,
                pbps_diff::Side {
                    schema: &after,
                    ids: &ids,
                },
                dialect,
                &Default::default(),
            )
            .unwrap();
            let first = schedule(diff, &declarations, &Facts::default(), dialect)
                .unwrap()
                .changes;
            assert!(!first.is_empty());
            let fingerprint = |plan: &CanonicalPlan<D>| {
                SavedPlan::new(
                    pbps_model::PlanOrigin::Database,
                    dialect.name(),
                    "2026-10-22",
                    pbps_model::PlanBaseline {
                        description: "continuation".into(),
                        checksum: String::new(),
                        database_collation: None,
                        time_zone: None,
                    },
                    plan.changes().clone(),
                    ids.clone(),
                )
                .wire_fingerprint()
            };
            let scope = pbps_diff::order::writebacks::ScopeInput::new(vec![]).unwrap();
            // Empty scope input used to erase a canonical engine just as a
            // nonempty scope tail did; neither may become a fresh Differ.
            let scoped = scope.append_to(first.clone(), dialect);
            let continued = [
                schedule(first.clone(), &declarations, &Facts::default(), dialect)
                    .unwrap()
                    .changes,
                schedule(scoped, &declarations, &Facts::default(), dialect)
                    .unwrap()
                    .changes,
            ];
            for actual in continued {
                assert_eq!(actual.changes(), first.changes());
                assert_eq!(actual.origins(), first.origins());
                assert_eq!(actual.logical(), first.logical());
                assert_eq!(actual.proof(), first.proof());
                assert_eq!(fingerprint(&actual), fingerprint(&first));
                for mode in [PlanMode::Transactional, PlanMode::Staged] {
                    let expected = first.clone().ordered(mode, dialect);
                    let ordered = actual.clone().ordered(mode, dialect);
                    assert_eq!(
                        ordered.operations().as_slice(),
                        expected.operations().as_slice()
                    );
                    assert_eq!(
                        pbps_dialect::lower::lower_all::<D>(&ordered, dialect)
                            .unwrap()
                            .statements(),
                        pbps_dialect::lower::lower_all::<D>(&expected, dialect)
                            .unwrap()
                            .statements()
                    );
                }
            }
        }
        check(&super::Postgres::new());
        check(&pbps_mssql::Mssql);
    }

    #[test]
    fn an_occupied_attach_check_name_keeps_the_single_statement() {
        use pbps_dialect::Dialect;
        use pbps_model::{BoundDatum, Change, ColumnName, PartitionBound, Uid};
        let uid: Uid = "t_aaaaaa".parse().unwrap();
        let other: Uid = "t_bbbbbb".parse().unwrap();
        let key: Vec<ColumnName> = vec!["id".into()];
        let typed: Vec<(ColumnName, pbps_model::ColumnType)> =
            vec![("id".into(), "integer".parse().unwrap())];
        let scratch = super::emit::scan_check_name(&uid);
        for (owner, held, occupied) in [
            (uid.clone(), scratch.clone(), true),
            (uid.clone(), "pbps_attach_t_aaaaaa_other".into(), false),
            (uid.clone(), "another_constraint".into(), false),
            (other, scratch, false),
        ] {
            let pg = super::Postgres::new()
                .with_attach_constraint_names([(owner, [held].into())].into());
            let change = Change::AttachPartition {
                uid: uid.clone(),
                table: "app.t".parse().unwrap(),
                parent: "app.ev".parse().unwrap(),
                bound: PartitionBound::Range {
                    from: vec![BoundDatum::Value("1".into())],
                    to: vec![BoundDatum::Value("10".into())],
                },
                key: pg.attach_scan_key(&uid, &typed, &Default::default()),
                shape: Box::default(),
            };
            // Only the optional proof key is sealed. A fresh emitter at apply
            // must reproduce the planning choice without another catalog read.
            let sql = super::Postgres::new()
                .fixture_emit_staged(&change, Default::default())
                .unwrap();
            assert_eq!(sql.len(), if occupied { 1 } else { 4 });
            if occupied {
                assert_eq!(sql, pg.fixture_emit(&change, Default::default()).unwrap());
                assert!(sql[0].scan_check.is_none());
            } else {
                assert!(sql[0].scan_check.is_some());
            }
        }
        assert_eq!(
            super::Postgres::new().attach_scan_key(&uid, &typed, &Default::default()),
            key
        );
    }

    fn partitioned(columns: &[&str], key: &[&str]) -> pbps_model::Table {
        let mut table = pbps_model::Table::default();
        for c in columns {
            let mut column = pbps_model::Column::new("integer".parse().unwrap());
            column.nullable = false;
            table.columns.insert((*c).into(), column);
        }
        table.partition_by = Some(pbps_model::PartitionBy {
            external: false,
            columns: key.iter().map(|c| (*c).into()).collect(),
            strategy: Default::default(),
        });
        table
    }

    fn problems(table: &pbps_model::Table) -> Vec<String> {
        use pbps_dialect::Dialect;
        super::Postgres::default()
            .validate_table(&"app.t".parse().unwrap(), table)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// A partitioned parent and its partitions hold what the reader reads
    /// back and nothing more (#1170): a key over declared columns that every
    /// key includes, a partition that declares only what is its own, and no
    /// `data:` or setting the reader would leave the tree out over.
    #[test]
    fn a_partition_tree_declares_only_what_is_read_back() {
        let mut ok = partitioned(&["id", "ts"], &["ts"]);
        ok.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".into(), "ts".into()],
            storage_parameters: Default::default(),
        });
        assert!(problems(&ok).is_empty(), "{:?}", problems(&ok));
        let partition = pbps_model::Table {
            partition_of: Some(pbps_model::PartitionOf {
                parent: "app.p".parse().unwrap(),
                bound: pbps_model::PartitionBound::Default,
                columns: Default::default(),
            }),
            ..Default::default()
        };
        assert!(problems(&partition).is_empty());

        let mut cases: Vec<(&str, pbps_model::Table)> = Vec::new();
        cases.push(("names no column", partitioned(&["id"], &[])));
        cases.push(("not a declared column", partitioned(&["id"], &["ts"])));
        let mut key = ok.clone();
        key.primary_key.as_mut().unwrap().columns = vec!["id".into()];
        cases.push(("does not include the partition key", key));
        let mut unlogged = ok.clone();
        unlogged.unlogged = true;
        cases.push(("takes no `storage_parameters`", unlogged));
        let mut identity = ok.clone();
        identity.replica_identity = Some(pbps_model::ReplicaIdentity::Full);
        cases.push(("takes no `storage_parameters`", identity));
        let mut rows = ok.clone();
        rows.data = Some(pbps_model::TableData {
            mode: pbps_model::DataMode::Exact,
            rows: Default::default(),
        });
        cases.push(("`data:` cannot be declared", rows));
        let mut own = partition.clone();
        own.columns.insert(
            "x".into(),
            pbps_model::Column::new("integer".parse().unwrap()),
        );
        cases.push(("declares only its own checks, indexes", own));
        let mut nested = partition.clone();
        nested.partition_by = Some(pbps_model::PartitionBy {
            external: false,
            columns: vec!["x".into()],
            strategy: Default::default(),
        });
        cases.push(("declares only its own checks, indexes", nested));
        // Negative: its own checks and indexes are a partition's to declare.
        let mut extras = partition.clone();
        extras.checks.insert(
            "p_ck".into(),
            pbps_model::CheckConstraint {
                expression: "id > 0".into(),
            },
        );
        extras.indexes.insert(
            "p_ix".into(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Column("id".into()),
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        // And its persistence and storage parameters (#1580).
        extras.unlogged = true;
        extras
            .storage_parameters
            .insert("fillfactor".into(), "70".into());
        assert!(problems(&extras).is_empty(), "{:?}", problems(&extras));
        for (expected, table) in cases {
            let found = problems(&table);
            assert!(
                found.iter().any(|p| p.contains(expected)),
                "{expected}: {found:?}"
            );
        }
    }

    /// A partition's own default and NOT NULL each override a column of its
    /// parent's (#1578): never one the parent lacks, a NOT NULL the parent's
    /// column already has, the parent's default again, or a default on a
    /// generated or identity column, which the engine refuses on a partition
    /// (measured on 16 and 18, identity on 18).
    #[test]
    fn a_partitions_own_column_overrides_answer_to_its_parents_columns() {
        use pbps_dialect::Dialect;
        let mut parent = partitioned(&["id", "ts"], &["ts"]);
        let mut v = pbps_model::Column::new("integer".parse().unwrap());
        v.value_source = pbps_model::ValueSource::Default("0".into());
        parent.columns.insert("v".into(), v);
        let mut g = pbps_model::Column::new("integer".parse().unwrap());
        g.value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
            expression: "v * 2".into(),
            stored: true,
        });
        parent.columns.insert("g".into(), g);
        let mut i = pbps_model::Column::new("integer".parse().unwrap());
        i.nullable = false;
        i.value_source = pbps_model::ValueSource::Identity(pbps_model::Identity {
            seed: 1,
            increment: 1,
            options: Default::default(),
        });
        parent.columns.insert("i".into(), i);
        parent
            .columns
            .insert("d".into(), pbps_model::Column::new("date".parse().unwrap()));
        let found = |overrides: &[(&str, Option<&str>, bool)]| -> Vec<String> {
            let partition = pbps_model::Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: "app.t".parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: overrides
                        .iter()
                        .map(|(c, d, n)| {
                            (
                                (*c).into(),
                                pbps_model::PartitionColumn {
                                    default: d.map(str::to_owned),
                                    not_null: *n,
                                },
                            )
                        })
                        .collect(),
                }),
                ..Default::default()
            };
            super::Postgres::default()
                .validate_partition(&"app.p1".parse().unwrap(), &partition, &parent)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        assert!(found(&[("v", Some("7"), true), ("ts", Some("'2025-01-01'"), false)]).is_empty());
        // A NULL of another type is one the engine keeps, as for a column.
        assert!(found(&[("v", Some("NULL::bigint"), false)]).is_empty());
        for (overrides, expected) in [
            (
                vec![("x", Some("1"), false)],
                "`x` is not a column of its parent",
            ),
            (
                vec![("id", None, true)],
                "`id` is NOT NULL on its parent already",
            ),
            (
                vec![("v", Some("0"), false)],
                "`v`'s default is its parent's",
            ),
            (
                vec![("g", Some("1"), false)],
                "`g` is generated on its parent",
            ),
            (
                vec![("i", Some("1"), false)],
                "`i` is an identity column on its parent",
            ),
            (
                vec![("v", Some("  "), false)],
                "`v` has an empty default expression",
            ),
            // Held as a column's default is, against the parent's column
            // type (#1578 review): measured on 16 and 18, a NULL of its type
            // leaves the partition no default at all.
            (
                vec![("v", Some("NULL::integer"), false)],
                "which this engine does not keep",
            ),
            (
                vec![("d", Some("'01/02/2026'"), false)],
                "is `date` and its default is the bare literal",
            ),
        ] {
            let got = found(&overrides);
            assert!(
                got.iter().any(|p| p.contains(expected)),
                "{expected}: {got:?}"
            );
        }
    }

    /// A serial is a replaceable default; an identity on 18 refuses the
    /// emitter's post-create SET DEFAULT even on a partition (DEC-2217.1).
    #[test]
    fn a_partitions_own_default_can_override_a_serial_but_not_an_identity() {
        use pbps_model::{
            Identity, OwnedSequence, PartitionBound, PartitionColumn, PartitionOf, ValueSource,
        };
        let mut parent = partitioned(&["id", "v"], &["v"]);
        let id = parent.columns.get_mut("id").unwrap();
        id.ty = ty("bigint");
        id.value_source = ValueSource::Sequence(OwnedSequence::serial());
        let partition = Table {
            partition_of: Some(PartitionOf {
                parent: "app.t".parse().unwrap(),
                bound: PartitionBound::Default,
                columns: [(
                    "id".into(),
                    PartitionColumn {
                        default: Some("0".into()),
                        not_null: false,
                    },
                )]
                .into_iter()
                .collect(),
            }),
            ..Default::default()
        };
        let name = "app.p1".parse().unwrap();
        let pg = Postgres::new();
        let found = pg.validate_partition(&name, &partition, &parent);
        assert!(found.is_empty(), "{found:?}");

        parent.columns.get_mut("id").unwrap().value_source = ValueSource::Identity(Identity {
            seed: 1,
            increment: 1,
            options: Default::default(),
        });
        let found = pg.validate_partition(&name, &partition, &parent);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0]
                .to_string()
                .contains("is an identity column on its parent"),
            "{found:?}"
        );
    }

    /// A partition's own trigger takes no name its parent's row trigger
    /// gives its clones, and an own check or foreign key none a parent's
    /// constraint trigger's clone holds, each refused by the engine as
    /// already existing, measured on 16 and 18 (#1632). A statement
    /// trigger's name is free.
    #[test]
    fn a_partitions_own_names_stay_clear_of_its_parents_cloned_triggers() {
        let trigger = |definition: &str| pbps_model::Module {
            kind: pbps_model::ModuleKind::Trigger,
            description: None,
            definition: definition.to_owned(),
            options: Default::default(),
        };
        let found = |parent_trigger: &str, own: &[&str], check: Option<&str>| {
            let mut schema = pbps_model::Schema::default();
            schema
                .tables
                .insert("app.p".parse().unwrap(), pbps_model::Table::default());
            let mut partition = pbps_model::Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: "app.p".parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Default::default()
            };
            if let Some(name) = check {
                partition.checks.insert(
                    name.into(),
                    pbps_model::CheckConstraint {
                        expression: "k > 0".into(),
                    },
                );
            }
            schema.tables.insert("app.p1".parse().unwrap(), partition);
            schema.modules.insert(
                "app.p.audit".parse().unwrap(),
                trigger(&format!("{parent_trigger} EXECUTE FUNCTION app.f()")),
            );
            for name in own {
                schema.modules.insert(
                    format!("app.p1.{name}").parse().unwrap(),
                    trigger("BEFORE UPDATE ON app.p1 FOR EACH ROW EXECUTE FUNCTION app.f()"),
                );
            }
            pbps_dialect::check_partition_triggers(&schema, &super::Postgres::default())
        };
        let row = "BEFORE INSERT ON app.p FOR EACH ROW";
        let got = found(row, &["audit"], None);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("trigger `audit` on `app.p1`"), "{got:?}");
        let got = found(
            "CONSTRAINT AFTER INSERT ON app.p FOR EACH ROW",
            &[],
            Some("audit"),
        );
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("check `audit` on `app.p1`"), "{got:?}");
        // Negative: another name, a statement trigger's name, and a check
        // under a plain row trigger's name, which holds no constraint.
        assert_eq!(found(row, &["mine"], None), Vec::<String>::new());
        assert_eq!(
            found("AFTER INSERT ON app.p FOR EACH STATEMENT", &["audit"], None),
            Vec::<String>::new()
        );
        assert_eq!(found(row, &[], Some("audit")), Vec::<String>::new());
    }

    /// A partition's own checks, indexes and foreign keys are held to the
    /// columns it has from its parent, by the rules a table's are (#1577,
    /// #1632); an own check or foreign key takes no name the parent's checks
    /// and foreign keys give their clones, which the engine refuses as already
    /// existing, measured on 16 and 18; and an own foreign key is never one
    /// equal to the parent's.
    #[test]
    fn a_partitions_own_checks_and_indexes_answer_to_its_parents_columns() {
        use pbps_dialect::Dialect;
        let mut parent = partitioned(&["id", "ts"], &["ts"]);
        parent.checks.insert(
            "ev_ck".into(),
            pbps_model::CheckConstraint {
                expression: "id > 0".into(),
            },
        );
        parent.foreign_keys.insert(
            "ev_fk".into(),
            pbps_model::ForeignKey {
                columns: vec!["id".into()],
                references_table: "app.r".parse().unwrap(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        let index = |column: &str| pbps_model::Index {
            columns: vec![pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Column(column.into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        let check = |expression: &str| pbps_model::CheckConstraint {
            expression: expression.into(),
        };
        let found = |partition: &pbps_model::Table| -> Vec<String> {
            super::Postgres::default()
                .validate_partition(&"app.p1".parse().unwrap(), partition, &parent)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        let mut ok = pbps_model::Table::default();
        ok.checks.insert("p1_ck".into(), check("ts > 0"));
        ok.indexes.insert("p1_id".into(), index("id"));
        assert!(found(&ok).is_empty(), "{:?}", found(&ok));
        // What the parent declares of its columns is the parent's to answer
        // for, once: not again under each partition.
        let mut collated = parent.clone();
        collated.columns.get_mut("id").unwrap().collation = Some(pbps_model::Collation::new("C"));
        assert!(
            super::Postgres::default()
                .validate_partition(&"app.p1".parse().unwrap(), &ok, &collated)
                .is_empty()
        );
        // Negative: a column the parent does not have, by name.
        let mut missing = ok.clone();
        missing.indexes.insert("p1_x".into(), index("x"));
        let got = found(&missing);
        assert!(
            got.iter()
                .any(|p| p.contains("index `p1_x` references `x`, which is not a column")),
            "{got:?}"
        );
        // Its storage parameters are held as a table's (#1580): one the
        // engine spells otherwise, or does not have, is refused.
        let mut stored = ok.clone();
        stored
            .storage_parameters
            .insert("fillfactor".into(), "70".into());
        assert!(found(&stored).is_empty(), "{:?}", found(&stored));
        for (value, name, expected) in [
            ("0070", "fillfactor", "spelled `56`"),
            ("1", "bogus", "`bogus`"),
        ] {
            let mut wrong = ok.clone();
            wrong.storage_parameters.insert(name.into(), value.into());
            let got = found(&wrong);
            assert!(got.iter().any(|p| p.contains(expected)), "{got:?}");
        }
        // Negative: an empty check is refused as a table's is.
        let mut empty = ok.clone();
        empty.checks.insert("p1_empty".into(), check("  "));
        assert!(
            found(&empty)
                .iter()
                .any(|p| p.contains("`p1_empty` has an empty expression")),
            "{:?}",
            found(&empty)
        );
        // Negative: the names the parent's clones keep.
        for (name, kind) in [("ev_ck", "check"), ("ev_fk", "foreign key")] {
            let mut taken = ok.clone();
            taken.checks.insert(name.into(), check("ts > 1"));
            let got = found(&taken);
            assert!(
                got.iter()
                    .any(|p| p.contains(&format!("check `{name}` is named as its parent's {kind}"))),
                "{got:?}"
            );
        }
        // Its own foreign keys (#1632): over the parent's columns, as a
        // table's are over its own, under no name the parent's clones keep,
        // and never one equal to a parent's, which the engine may take for
        // its clone.
        let fk = |column: &str, on_delete| pbps_model::ForeignKey {
            columns: vec![column.into()],
            references_table: "app.r".parse().unwrap(),
            references_columns: vec!["id".into()],
            on_delete,
            on_update: Default::default(),
        };
        let mut own_fk = ok.clone();
        own_fk
            .foreign_keys
            .insert("p1_fk".into(), fk("ts", Default::default()));
        // The parent's columns again, with another action: not the parent's.
        own_fk.foreign_keys.insert(
            "p1_cascade".into(),
            fk("id", pbps_model::ReferentialAction::Cascade),
        );
        assert!(found(&own_fk).is_empty(), "{:?}", found(&own_fk));
        for (name, definition, expected) in [
            (
                "p1_x",
                fk("x", Default::default()),
                "references `x`, which is not a column",
            ),
            (
                "ev_fk",
                fk("ts", Default::default()),
                "foreign key `ev_fk` is named as its parent's foreign key",
            ),
            (
                "ev_ck",
                fk("ts", Default::default()),
                "foreign key `ev_ck` is named as its parent's check",
            ),
            (
                "p1_again",
                fk("id", Default::default()),
                "foreign key `p1_again` is its parent's `ev_fk` again",
            ),
        ] {
            let mut wrong = ok.clone();
            wrong.foreign_keys.insert(name.into(), definition);
            let got = found(&wrong);
            assert!(
                got.iter().any(|p| p.contains(expected)),
                "{expected}: {got:?}"
            );
        }
    }

    /// A computed column is SQL Server's in this model, and refused on
    /// PostgreSQL by name, where a generated column is the form (#1174).
    #[test]
    fn a_computed_column_is_refused_on_postgres() {
        use pbps_dialect::Dialect;
        let mut table = pbps_model::Table::default();
        table.columns.insert(
            "a".into(),
            pbps_model::Column::new("integer".parse().unwrap()),
        );
        let name: pbps_model::TableName = "app.t".parse().unwrap();
        assert!(
            super::Postgres::default()
                .validate_table(&name, &table)
                .is_empty()
        );
        table.computed.insert(
            "c".into(),
            pbps_model::ComputedColumn {
                expression: "a * 2".into(),
                persisted: true,
                not_null: false,
            },
        );
        let found: Vec<String> = super::Postgres::default()
            .validate_table(&name, &table)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(
            found
                .iter()
                .any(|m| m.contains("computed column") && m.contains("generated:")),
            "{found:?}"
        );
    }

    /// An array is never mistaken for its element where a check reads the
    /// element's name (#1525 review): an identity on `integer[]`, and a GIN
    /// index over `jsonb[]`, are refused as on any other type, while the
    /// scalar `integer` and `jsonb` keep both.
    #[test]
    fn an_array_is_not_its_element_for_identity_or_gin() {
        use pbps_dialect::Dialect;
        let name: pbps_model::TableName = "app.t".parse().unwrap();
        let found = |t: &pbps_model::Table| -> Vec<String> {
            super::Postgres::default()
                .validate_table(&name, t)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        let identity = |ty: &str| {
            let mut t = pbps_model::Table::default();
            let mut c = pbps_model::Column::new(ty.parse().unwrap()).not_null();
            c.value_source = pbps_model::ValueSource::Identity(pbps_model::Identity {
                seed: 1,
                increment: 1,
                options: Default::default(),
            });
            t.columns.insert("id".into(), c);
            t
        };
        assert!(
            found(&identity("integer")).is_empty(),
            "{:?}",
            found(&identity("integer"))
        );
        let refused = found(&identity("integer[]"));
        assert!(
            refused.iter().any(|m| m.contains("identity")),
            "{refused:?}"
        );

        let gin = |ty: &str| {
            let mut t = pbps_model::Table::default();
            t.columns
                .insert("doc".into(), pbps_model::Column::new(ty.parse().unwrap()));
            t.indexes.insert(
                "ix_doc".into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column("doc".into()),
                        descending: false,
                        opclass: Some("jsonb_path_ops".into()),
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: pbps_model::IndexMethod::Gin,
                    storage_parameters: Default::default(),
                },
            );
            t
        };
        assert!(
            found(&gin("jsonb")).is_empty(),
            "{:?}",
            found(&gin("jsonb"))
        );
        let refused = found(&gin("jsonb[]"));
        assert!(
            refused.iter().any(|m| m.contains("GIN only over")),
            "{refused:?}"
        );
    }

    /// `data:` on a table with an array column is refused: a row's values
    /// are scalars, and an array's equality includes its bounds (#1167). A
    /// table with arrays and no rows, and rows beside no array, are clean.
    #[test]
    fn rows_beside_an_array_column_are_refused() {
        use pbps_dialect::Dialect;
        let name: pbps_model::TableName = "app.t".parse().unwrap();
        let table = |array: bool, rows: bool| {
            let mut t = pbps_model::Table::default();
            t.columns.insert(
                "id".into(),
                pbps_model::Column::new("integer".parse().unwrap()).not_null(),
            );
            let tags = if array { "text[]" } else { "text" };
            t.columns.insert(
                "tags".into(),
                pbps_model::Column::new(tags.parse().unwrap()),
            );
            t.primary_key = Some(pbps_model::PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            if rows {
                t.data = Some(pbps_model::TableData {
                    mode: pbps_model::DataMode::Exact,
                    rows: Default::default(),
                });
            }
            t
        };
        let found = |t: &pbps_model::Table| -> Vec<String> {
            super::Postgres::default()
                .validate_table(&name, t)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        let refused = found(&table(true, true));
        assert!(
            refused
                .iter()
                .any(|m| m.contains("array column") && m.contains("`tags`")),
            "{refused:?}"
        );
        assert!(
            found(&table(true, false)).is_empty(),
            "{:?}",
            found(&table(true, false))
        );
        assert!(
            found(&table(false, true)).is_empty(),
            "{:?}",
            found(&table(false, true))
        );
    }

    /// `system_time` is SQL Server's period and system versioning, which
    /// PostgreSQL has neither of: refused by name (#1176).
    #[test]
    fn system_time_is_refused_on_postgres() {
        use pbps_dialect::Dialect;
        let mut table = pbps_model::Table::default();
        for c in ["vf", "vt"] {
            table.columns.insert(
                c.into(),
                pbps_model::Column::new("timestamptz".parse().unwrap()).not_null(),
            );
        }
        let name: pbps_model::TableName = "app.t".parse().unwrap();
        let found = |t: &pbps_model::Table| -> Vec<String> {
            super::Postgres::default()
                .validate_table(&name, t)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        assert!(found(&table).is_empty(), "{:?}", found(&table));
        table.system_time = Some(pbps_model::SystemTime {
            start: "vf".into(),
            end: "vt".into(),
            hidden: false,
            versioning: None,
        });
        let refused = found(&table);
        assert!(
            refused.iter().any(|m| m.contains("`system_time`")),
            "{refused:?}"
        );
    }

    /// #1645: the numbered names the engine tries for an unnamed key's index,
    /// as measured on 16 and 18 with twelve 61-byte tables.
    #[test]
    fn a_numbered_key_name_cuts_the_table_again_to_fit() {
        use super::implicit_primary_key_fallback;
        let table = format!("{}a", "a".repeat(60));
        let names: Vec<String> = (0..12)
            .map(|n| implicit_primary_key_fallback(&table, n))
            .collect();
        assert_eq!(names[0], format!("{}_pkey", "a".repeat(58)));
        assert_eq!(names[1], format!("{}_pkey1", "a".repeat(57)));
        assert_eq!(names[9], format!("{}_pkey9", "a".repeat(57)));
        assert_eq!(names[10], format!("{}_pkey10", "a".repeat(56)));
        assert_eq!(names[11], format!("{}_pkey11", "a".repeat(56)));
        assert!(names.iter().all(|n| n.len() == 63), "{names:?}");
        // Negative: a short table is not cut.
        assert_eq!(implicit_primary_key_fallback("k", 1), "k_pkey1");
        assert_eq!(implicit_primary_key_fallback("k", 0), "k_pkey");
    }

    /// #465: the generated names are the engine's own, measured on 18.6 —
    /// the short forms, a long table cut to fit 63 bytes, a long table and a
    /// long column shortened in turn, and two-byte characters cut at a
    /// character boundary.
    #[test]
    fn generated_names_are_the_ones_the_engine_measured() {
        use super::generated_name;
        assert_eq!(generated_name("widget", None, "pkey"), "widget_pkey");
        assert_eq!(generated_name("gadget", Some("id"), "seq"), "gadget_id_seq");
        let long = "aaaaaaaaaabbbbbbbbbbccccccccccddddddddddeeeeeeeeeeffffffffff";
        assert_eq!(
            generated_name(long, None, "pkey"),
            "aaaaaaaaaabbbbbbbbbbccccccccccddddddddddeeeeeeeeeeffffffff_pkey"
        );
        assert_eq!(
            generated_name(long, Some("id"), "seq"),
            "aaaaaaaaaabbbbbbbbbbccccccccccddddddddddeeeeeeeeeeffffff_id_seq"
        );
        assert_eq!(
            generated_name(&"t".repeat(30), Some(&"c".repeat(46)), "seq"),
            format!("{}_{}_seq", "t".repeat(29), "c".repeat(29))
        );
        let wide = "é".repeat(31);
        assert_eq!(
            generated_name(&wide, None, "pkey"),
            format!("{}_pkey", "é".repeat(29))
        );
        assert_eq!(
            generated_name(&wide, Some("id"), "seq"),
            format!("{}_id_seq", "é".repeat(28))
        );
        for n in [
            generated_name(long, Some("id"), "seq"),
            generated_name(&wide, None, "pkey"),
        ] {
            assert!(n.len() <= 63, "{n}");
        }
    }

    /// #465: a declared index named for what an unnamed primary key or an
    /// identity column makes the engine generate is refused, because which
    /// of the two gets the name depends on the order they are created in. A
    /// named primary key generates nothing, and two generated names that meet
    /// are the engine's to resolve with a suffix. (An engine whose indexes do
    /// not share the table namespace is never asked: the hook's default is
    /// empty, and `check_index_names` returns before it for such a dialect.)
    /// #987: the fallbacks `ChooseRelationName` retries with, as measured on
    /// 18.6: the number goes on the label and the table name is cut again.
    #[test]
    fn generated_fallbacks_are_the_ones_the_engine_measured() {
        use pbps_model::{Column, Identity, PrimaryKey, Table};
        let long = format!("{}x", "a".repeat(59));
        let mut t = Table::default();
        let mut id = Column::new("integer".parse().unwrap()).not_null();
        id.value_source = pbps_model::ValueSource::Identity(Identity {
            seed: 1,
            increment: 1,
            options: Default::default(),
        });
        t.columns.insert("id".into(), id);
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        let pg = super::Postgres::new();
        let name = TableName::new("app", &long);
        assert_eq!(
            pg.implicit_relation_fallbacks(&name, &t, 1),
            [
                format!("{}_pkey1", "a".repeat(57)),
                format!("{}_id_seq1", "a".repeat(55)),
            ]
        );
        assert_eq!(
            pg.implicit_relation_fallbacks(&name, &t, 2)[0],
            format!("{}_pkey2", "a".repeat(57))
        );
    }

    /// #987: two tables whose unnamed keys generate one `_pkey` leave the
    /// engine a fallback, `_pkey1`, for whichever is created second, and a
    /// declared index there meets it in one order or the other. Its remedy
    /// is the key's own only at the last fallback in play (#990).
    /// Negatives: a single table has no fallback in play, and two claimants
    /// use only the first fallback, never `_pkey2`.
    #[test]
    fn a_declared_name_at_a_generated_fallback_is_refused() {
        use pbps_model::{Column, Index, IndexColumn, PrimaryKey, Schema, Table};
        let keyed = |index_name: Option<&str>| {
            let mut t = Table::default();
            t.columns.insert(
                "id".into(),
                Column::new("integer".parse().unwrap()).not_null(),
            );
            t.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            if let Some(n) = index_name {
                t.indexes.insert(
                    n.into(),
                    Index {
                        columns: vec![IndexColumn {
                            key: pbps_model::IndexKey::Column("id".into()),
                            descending: false,
                            opclass: None,
                        }],
                        include: Vec::new(),
                        unique: false,
                        filter: None,
                        method: Default::default(),
                        storage_parameters: Default::default(),
                    },
                );
            }
            t
        };
        let long = |c: char| format!("{}{c}", "a".repeat(59));
        let pkey = |n: &str| format!("{}_pkey{n}", "a".repeat(57));
        let pg = super::Postgres::new();
        let schema = |tables: Vec<(String, Table)>| {
            let mut s = Schema::default();
            for (name, t) in tables {
                s.tables.insert(TableName::new("app", &name), t);
            }
            s
        };

        let found = pbps_dialect::check_index_names(
            &schema(vec![
                (long('x'), keyed(None)),
                (long('y'), keyed(Some(&pkey("1")))),
            ]),
            &pg,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("may both be named"), "{found:?}");
        assert!(found[0].contains("Name the primary key"), "{found:?}");

        // Three claimants: naming one key leaves two that still meet and
        // still retry as `_pkey1`, so only the declared index can move...
        let three = |suffix: &str| {
            pbps_dialect::check_index_names(
                &schema(vec![
                    (long('x'), keyed(None)),
                    (long('z'), keyed(None)),
                    (long('y'), keyed(Some(&pkey(suffix)))),
                ]),
                &pg,
            )
        };
        let found = three("1");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].ends_with("Rename the other object."), "{found:?}");
        assert!(!found[0].contains("Name the primary key"), "{found:?}");
        // ...but the two that are left never reach `_pkey2`, so there
        // naming a key is a remedy again.
        let found = three("2");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("Name the primary key"), "{found:?}");

        assert!(
            pbps_dialect::check_index_names(
                &schema(vec![(long('y'), keyed(Some(&pkey("1"))))]),
                &pg
            )
            .is_empty(),
            "one table: its first choice is its own, and no fallback is in play"
        );
        assert!(
            pbps_dialect::check_index_names(
                &schema(vec![
                    (long('x'), keyed(None)),
                    (long('y'), keyed(Some(&pkey("2")))),
                ]),
                &pg,
            )
            .is_empty(),
            "two claimants retry once"
        );
    }

    /// #1111: groups with different first choices can share a fallback.
    /// Four 60-byte tables that agree on 57 bytes but not on the 58th form
    /// two `_pkey` groups of two, and both groups retry as the same 57-byte
    /// `_pkey1`, so the group that retries second can reach `_pkey2`. A
    /// declared index there is refused, though neither group alone has three
    /// members. Negative: groups that differ within the 57 bytes have
    /// different fallbacks, and `_pkey2` is out of either's reach.
    #[test]
    fn groups_that_share_a_fallback_reach_past_it_together() {
        use pbps_model::{Column, Index, IndexColumn, PrimaryKey, Schema, Table};
        let keyed = |index_name: Option<&str>| {
            let mut t = Table::default();
            t.columns.insert(
                "id".into(),
                Column::new("integer".parse().unwrap()).not_null(),
            );
            t.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            if let Some(n) = index_name {
                t.indexes.insert(
                    n.into(),
                    Index {
                        columns: vec![IndexColumn {
                            key: pbps_model::IndexKey::Column("id".into()),
                            descending: false,
                            opclass: None,
                        }],
                        include: Vec::new(),
                        unique: false,
                        filter: None,
                        method: Default::default(),
                        storage_parameters: Default::default(),
                    },
                );
            }
            t
        };
        let pg = super::Postgres::new();
        // `shared` bytes in common, then the group, then padding to 60.
        let four = [('x', 'p'), ('x', 'q'), ('y', 'p'), ('y', 'q')];
        let tables = |members: &[(char, char)], shared: usize, index_name: &str| {
            let mut s = Schema::default();
            for &(group, member) in members {
                let name = format!(
                    "{}{group}{}{member}",
                    "a".repeat(shared),
                    "b".repeat(58 - shared)
                );
                let index = (group, member) == ('x', 'p');
                s.tables.insert(
                    TableName::new("app", &name),
                    keyed(index.then_some(index_name)),
                );
            }
            s
        };
        let schema = |shared: usize, index_name: &str| tables(&four, shared, index_name);

        let pkey2 = format!("{}_pkey2", "a".repeat(57));
        let found = pbps_dialect::check_index_names(&schema(57, &pkey2), &pg);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("may both be named"), "{found:?}");
        // Taking one key out leaves one group of two and a lone table, and
        // they reach `_pkey1` at most.
        assert!(found[0].contains("Name the primary key"), "{found:?}");

        // Differing at byte 57, inside what the fallback keeps: two
        // fallbacks, one per group, and each group reaches its own `_pkey1`.
        let own = format!("{}x_pkey2", "a".repeat(56));
        assert!(
            pbps_dialect::check_index_names(&schema(56, &own), &pg).is_empty(),
            "a group of two never reaches its second fallback"
        );
        let own = format!("{}x_pkey1", "a".repeat(56));
        assert_eq!(
            pbps_dialect::check_index_names(&schema(56, &own), &pg).len(),
            1,
            "its first fallback it does reach"
        );

        // Groups of three and two sharing their fallbacks: `_pkey3` is in
        // reach. Taking a key out of the group of three leaves a reach that
        // is judged per claimant and still says `_pkey3`; taking one out of
        // the group of two does not. The refusal must find that remedy
        // though the group of three comes first.
        let five = [('x', 'p'), ('x', 'q'), ('x', 'r'), ('y', 'p'), ('y', 'q')];
        let pkey3 = format!("{}_pkey3", "a".repeat(57));
        let found = pbps_dialect::check_index_names(&tables(&five, 57, &pkey3), &pg);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("Name the primary key"), "{found:?}");
    }

    /// #1111: the reach fixpoint stays cheap when many generated names meet
    /// at one first choice, and exact at the edge. A hundred 63-byte tables
    /// sharing 58 bytes form one chain, whose last claimant reaches
    /// `_pkey99` and no further. Rebuilding every matching each round made
    /// this minutes long; it must stay well inside a test run.
    #[test]
    fn a_long_chain_of_generated_names_is_judged_exactly_and_quickly() {
        use pbps_model::{Column, Index, IndexColumn, PrimaryKey, Schema, Table};
        let schema = |index_name: &str| {
            let mut s = Schema::default();
            for k in 0..100 {
                let mut t = Table::default();
                t.columns.insert(
                    "id".into(),
                    Column::new("integer".parse().unwrap()).not_null(),
                );
                t.primary_key = Some(PrimaryKey {
                    name: None,
                    columns: vec!["id".into()],
                    storage_parameters: Default::default(),
                });
                if k == 0 {
                    t.indexes.insert(
                        index_name.into(),
                        Index {
                            columns: vec![IndexColumn {
                                key: pbps_model::IndexKey::Column("id".into()),
                                descending: false,
                                opclass: None,
                            }],
                            include: Vec::new(),
                            unique: false,
                            filter: None,
                            method: Default::default(),
                            storage_parameters: Default::default(),
                        },
                    );
                }
                s.tables.insert(
                    TableName::new("app", format!("{}{k:05}", "a".repeat(58))),
                    t,
                );
            }
            s
        };
        let pg = super::Postgres::new();
        // A longer label cuts the table name shorter: `_pkey99` keeps 56
        // bytes, not 57, so ask the dialect rather than spell it.
        let keyed =
            &schema("unused").tables[&TableName::new("app", format!("{}00001", "a".repeat(58)))];
        let at = |n: u32| {
            pg.implicit_relation_fallbacks(
                &TableName::new("app", format!("{}00001", "a".repeat(58))),
                keyed,
                n,
            )[0]
            .clone()
        };
        assert_eq!(at(99), format!("{}_pkey99", "a".repeat(56)));
        let found = pbps_dialect::check_index_names(&schema(&at(99)), &pg);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("Name the primary key"), "{found:?}");
        let found = pbps_dialect::check_index_names(&schema(&at(98)), &pg);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].ends_with("Rename the other object."), "{found:?}");
        assert!(
            pbps_dialect::check_index_names(&schema(&at(100)), &pg).is_empty(),
            "a hundred claimants retry at most 99 times"
        );
    }

    /// A `serial` that leaves its sequence's name out claims none: a renamed
    /// column keeps the sequence it had, so the name its table and column
    /// would generate may belong to a table the declarations hold. A name it
    /// does declare is claimed, and a table under it is refused (#1631).
    #[test]
    fn only_a_declared_serial_sequence_name_is_claimed() {
        use pbps_model::{Column, Schema, Table};
        let with = |sequence: Option<&str>, other_name: &str| {
            let mut serial = Column::new("integer".parse().unwrap()).not_null();
            serial.value_source = pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence {
                start: 1,
                increment: 1,
                name: sequence.map(str::to_owned),
                options: Default::default(),
            });
            let mut t = Table::default();
            t.columns.insert("n".into(), serial);
            let mut other = Table::default();
            other
                .columns
                .insert("x".into(), Column::new("integer".parse().unwrap()));
            let mut schema = Schema::default();
            schema.tables.insert(TableName::new("app", "t"), t);
            schema
                .tables
                .insert(TableName::new("app", other_name), other);
            pbps_dialect::check_index_names(&schema, &super::Postgres::new())
        };
        let unnamed = with(None, "t_n_seq");
        assert!(unnamed.is_empty(), "{unnamed:?}");
        // The negative case: the declared name is claimed.
        let found = with(Some("numbers"), "numbers");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("`serial` column `app.t.n`"), "{found:?}");
    }

    /// A `serial`'s sequence is created by name and never retried, so it
    /// takes no other claimant's fallback: a serial before an identity in its
    /// table does not reach the identity's `_seq1`, and a table under that
    /// name is no collision. The identity still reaches it (#1631).
    #[test]
    fn a_serial_takes_no_fallback_of_an_identity_beside_it() {
        use pbps_model::{Column, Identity, Schema, Table};
        let identity = || {
            let mut c = Column::new("integer".parse().unwrap()).not_null();
            c.value_source = pbps_model::ValueSource::Identity(Identity {
                seed: 1,
                increment: 1,
                options: Default::default(),
            });
            c
        };
        let mut serial = Column::new("integer".parse().unwrap()).not_null();
        serial.value_source = pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence {
            start: 1,
            increment: 1,
            name: Some("u_id_seq".into()),
            options: Default::default(),
        });
        let mut t = Table::default();
        t.columns.insert("a".into(), serial);
        t.columns.insert("id".into(), identity());
        let mut u = Table::default();
        u.columns.insert("id".into(), identity());
        let mut other = Table::default();
        other
            .columns
            .insert("x".into(), Column::new("integer".parse().unwrap()));
        let mut schema = Schema::default();
        schema.tables.insert(TableName::new("app", "t"), t.clone());
        schema.tables.insert(TableName::new("app", "u"), u);
        schema
            .tables
            .insert(TableName::new("app", "t_id_seq1"), other);
        let pg = super::Postgres::new();
        let found = pbps_dialect::check_index_names(&schema, &pg);
        assert!(found.is_empty(), "{found:?}");
        // The positions agree: the identity's fallback is at its own.
        let names = pg.implicit_relation_names(&TableName::new("app", "t"), &t);
        let fallbacks = pg.implicit_relation_fallbacks(&TableName::new("app", "t"), &t, 1);
        assert_eq!(names[0].name, "t_id_seq");
        assert_eq!(fallbacks, ["t_id_seq1"]);
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn a_declared_serial_sequence_can_take_an_identitys_first_choice_or_fallback() {
        use pbps_model::{Column, Identity, OwnedSequence, Schema, Table, ValueSource};
        let pg = super::Postgres::new();
        let mut numbered = Table::default();
        let mut id = Column::new("integer".parse().unwrap()).not_null();
        id.value_source = ValueSource::Identity(Identity {
            seed: 1,
            increment: 1,
            options: Default::default(),
        });
        numbered.columns.insert("id".into(), id);
        let long = |last| TableName::new("app", format!("{}{last}", "a".repeat(59)));
        let first = pg.implicit_relation_names(&long('x'), &numbered)[0]
            .name
            .clone();
        let fallback = pg.implicit_relation_fallbacks(&long('x'), &numbered, 1)[0].clone();
        for name in [&first, &fallback] {
            let mut serial = Column::new("integer".parse().unwrap()).not_null();
            serial.value_source = ValueSource::Sequence(OwnedSequence {
                start: 1,
                increment: 1,
                name: Some(name.clone()),
                options: Default::default(),
            });
            let mut table = Table::default();
            table.columns.insert("n".into(), serial);
            let mut schema = Schema::default();
            schema.tables.insert(long('x'), numbered.clone());
            schema.tables.insert(long('y'), numbered.clone());
            schema
                .tables
                .insert(TableName::new("app", "serials"), table);
            let found = pbps_dialect::check_index_names(&schema, &pg);
            assert!(found.is_empty(), "{found:?}");
            // A fixed first choice can still send an identity to a fallback
            // occupied by a declared table; that collision stays refused.
            if name == &first {
                schema
                    .tables
                    .insert(TableName::new("app", &fallback), Table::default());
                let found = pbps_dialect::check_index_names(&schema, &pg);
                assert_eq!(found.len(), 1, "{found:?}");
                assert!(found[0].contains("identity column"), "{found:?}");
                assert!(
                    found[0].contains(&format!("table `app.{fallback}`")),
                    "{found:?}"
                );
            }
        }
        // The same generated/truncated spelling is unknown for an unnamed serial.
        let mut unnamed = numbered.clone();
        unnamed.columns.get_mut("id").unwrap().value_source =
            ValueSource::Sequence(OwnedSequence {
                start: 1,
                increment: 1,
                name: None,
                options: Default::default(),
            });
        let mut named = unnamed.clone();
        if let ValueSource::Sequence(sequence) =
            &mut named.columns.get_mut("id").unwrap().value_source
        {
            sequence.name = Some(first);
        }
        let mut schema = Schema::default();
        schema.tables.insert(long('x'), unnamed);
        schema
            .tables
            .insert(TableName::new("app", "serials"), named);
        let found = pbps_dialect::check_index_names(&schema, &pg);
        assert!(found.is_empty(), "{found:?}");
    }

    /// #1112: PostgreSQL names a primary key's index with
    /// `ChooseRelationName(..., isconstraint = true)`, which also takes a
    /// constraint's name in the schema as used. A check named `t_pkey` on
    /// another table sends `t`'s unnamed key to `t_pkey1` with no other
    /// generated name in play, so a declared index there is order-dependent
    /// and refused, with the key's own remedy. Negatives: without the named
    /// check `t_pkey1` is free, and an identity sequence's retry does not
    /// look at constraints. A constraint trigger named `t_pkey` counts as a
    /// constraint; an ordinary trigger does not.
    #[test]
    fn a_named_constraint_at_a_keys_first_choice_puts_its_fallback_in_play() {
        use pbps_model::{
            CheckConstraint, Column, Identity, Index, IndexColumn, PrimaryKey, Schema, Table,
        };
        let index = |name: &str| {
            (
                name.into(),
                Index {
                    columns: vec![IndexColumn {
                        key: pbps_model::IndexKey::Column("id".into()),
                        descending: false,
                        opclass: None,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            )
        };
        let table = |pk: bool, identity: bool, index_name: Option<&str>| {
            let mut t = Table::default();
            let mut id = Column::new("integer".parse().unwrap()).not_null();
            if identity {
                id.value_source = pbps_model::ValueSource::Identity(Identity {
                    seed: 1,
                    increment: 1,
                    options: Default::default(),
                });
            }
            t.columns.insert("id".into(), id);
            if pk {
                t.primary_key = Some(PrimaryKey {
                    name: None,
                    columns: vec!["id".into()],
                    storage_parameters: Default::default(),
                });
            }
            t.indexes.extend(index_name.map(index));
            t
        };
        let other = |check: Option<&str>| {
            let mut t = table(false, false, None);
            if let Some(name) = check {
                t.checks.insert(
                    name.into(),
                    CheckConstraint {
                        expression: "id > 0".into(),
                    },
                );
            }
            t
        };
        let schema = |t: Table, o: Table| {
            let mut s = Schema::default();
            s.tables.insert(TableName::new("app", "t"), t);
            s.tables.insert(TableName::new("app", "other"), o);
            s
        };
        let pg = super::Postgres::new();

        let found = pbps_dialect::check_index_names(
            &schema(table(true, false, Some("t_pkey1")), other(Some("t_pkey"))),
            &pg,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("may both be named `app.t_pkey1`"),
            "{found:?}"
        );
        assert!(found[0].contains("Name the primary key"), "{found:?}");
        // The retry is the constraint's doing, and the refusal says a
        // constraint can be why, not only another generated name.
        assert!(found[0].contains("or by a constraint"), "{found:?}");

        assert!(
            pbps_dialect::check_index_names(
                &schema(table(true, false, Some("t_pkey1")), other(None)),
                &pg
            )
            .is_empty(),
            "a key alone keeps its first choice"
        );
        assert!(
            pbps_dialect::check_index_names(
                &schema(
                    table(false, true, Some("t_id_seq1")),
                    other(Some("t_id_seq"))
                ),
                &pg
            )
            .is_empty(),
            "a sequence's retry does not look at constraint names"
        );

        // A constraint trigger is a constraint under its own name; an
        // ordinary trigger is not.
        let with_trigger = |definition: &str| {
            let mut s = schema(table(true, false, Some("t_pkey1")), other(None));
            s.modules.insert(
                pbps_model::ModuleId::Trigger {
                    on: TableName::new("app", "other"),
                    name: "t_pkey".into(),
                },
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::Trigger,
                    description: None,
                    definition: definition.into(),
                    options: Default::default(),
                },
            );
            s
        };
        let found = pbps_dialect::check_index_names(
            &with_trigger(
                "CONSTRAINT AFTER INSERT ON app.other FOR EACH ROW EXECUTE FUNCTION app.f()",
            ),
            &pg,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("`app.t_pkey1`"), "{found:?}");
        assert!(
            pbps_dialect::check_index_names(
                &with_trigger("AFTER INSERT ON app.other FOR EACH ROW EXECUTE FUNCTION app.f()"),
                &pg
            )
            .is_empty(),
            "an ordinary trigger has no constraint row"
        );
    }

    #[test]
    fn a_declared_name_meeting_a_generated_one_is_refused() {
        use pbps_model::{Column, Identity, Index, IndexColumn, PrimaryKey, Schema, Table};
        let index = |col: &str| Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column(col.into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        let table = |pk_name: Option<&str>, identity: bool, index_name: Option<&str>| {
            let mut t = Table::default();
            let mut id = Column::new("integer".parse().unwrap()).not_null();
            if identity {
                id.value_source = pbps_model::ValueSource::Identity(Identity {
                    seed: 1,
                    increment: 1,
                    options: Default::default(),
                });
            }
            t.columns.insert("id".into(), id);
            t.primary_key = Some(PrimaryKey {
                name: pk_name.map(Into::into),
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            if let Some(n) = index_name {
                t.indexes.insert(n.into(), index("id"));
            }
            t
        };
        let one = |t: Table| {
            let mut s = Schema::default();
            s.tables.insert(TableName::new("app", "widget"), t);
            s
        };
        let pg = super::Postgres::new();

        let found =
            pbps_dialect::check_index_names(&one(table(None, false, Some("widget_pkey"))), &pg);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("unnamed primary key"), "{found:?}");

        assert!(found[0].contains("Name the primary key"), "{found:?}");

        let found = pbps_dialect::check_index_names(
            &one(table(Some("pk_w"), true, Some("widget_id_seq"))),
            &pg,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("identity column `app.widget.id`"),
            "{found:?}"
        );
        // #990: the key is already named, and naming it moves nothing here;
        // nor does renaming an existing column, whose sequence keeps its name.
        assert!(!found[0].contains("Name the primary key"), "{found:?}");
        assert!(!found[0].contains("or the identity column"), "{found:?}");
        assert!(found[0].ends_with("Rename the other object."), "{found:?}");

        // A named primary key generates no index name to meet.
        assert!(
            pbps_dialect::check_index_names(
                &one(table(Some("pk_w"), false, Some("widget_pkey"))),
                &pg
            )
            .is_empty()
        );

        // Two generated names that meet: a table named `widget_id_seq`'s own
        // `_pkey` cannot collide, but two long tables cut to the same prefix
        // can, and the engine suffixes the second.
        let long = "x".repeat(60);
        let mut s = Schema::default();
        s.tables.insert(
            TableName::new("app", format!("{long}a")),
            table(None, false, None),
        );
        s.tables.insert(
            TableName::new("app", format!("{long}b")),
            table(None, false, None),
        );
        assert!(pbps_dialect::check_index_names(&s, &pg).is_empty());
    }

    #[test]
    fn an_empty_postgres_script_does_not_change_session_settings() {
        assert!(
            script_oracle::render(
                &[],
                &super::Postgres::new(),
                &pbps_model::SessionZone::utc()
            )
            .is_empty()
        );
    }

    /// An `xml` literal is read as `content` wherever a write runs
    /// (DEC-1634.1): a fragment such as `<a/><b/>` is `invalid XML document`
    /// under the other option, and the server's own default is only the
    /// default. The pin rides in the transaction framing like the rest, and
    /// the read scope carries the same value, so a declared `xml` value is
    /// judged as the apply stores it.
    #[test]
    fn the_write_pins_and_the_read_scope_both_read_xml_as_content() {
        let pg = super::Postgres::new();
        let zone = pbps_model::SessionZone::new("UTC").unwrap();
        let pins = pg.session_pins(&zone).expect("this dialect pins");
        assert!(pins.contains("SET xmloption = content;"), "{pins}");
        assert!(!pins.contains("document"), "{pins}");
        assert!(
            pg.transaction_framing(&zone).begin.starts_with(&pins),
            "the framing carries every pin"
        );
        assert!(
            crate::catalog::canonical_path(&zone).contains("'xmloption', 'content', true"),
            "the read scope pins it too"
        );
    }

    /// The zone is replayed as the engine spelled it, quoted, into every pin
    /// a write runs under (DEC-1916.1).
    #[test]
    fn the_write_pins_carry_the_targets_zone_as_a_quoted_literal() {
        let pg = super::Postgres::new();
        let zone = pbps_model::SessionZone::new("<+08>-08").unwrap();
        let pins = pg.session_pins(&zone).expect("this dialect pins");
        assert!(pins.contains("SET TimeZone = '<+08>-08';"), "{pins}");
        assert!(!pins.contains("'UTC'"), "{pins}");
        let framing = pg.transaction_framing(&zone);
        assert!(framing.begin.starts_with(&pins), "{}", framing.begin);
        // A quote in the text cannot end the literal early.
        let odd = pbps_model::SessionZone::new("a'b").unwrap();
        assert!(
            pg.session_pins(&odd)
                .unwrap()
                .contains("SET TimeZone = 'a''b';")
        );
    }

    /// A table's rename keeps its `serial`'s sequence under the name the old
    /// table generated. Against a read, which knows that name, declaring it
    /// is no change and another renames the sequence. Against a git
    /// baseline, which is a declaration and does not know it, a later pull's
    /// name for it is no change; one the baseline does name is still
    /// compared (#1631), and renamed where it differs (#1889).
    #[test]
    fn a_renamed_tables_serial_may_name_the_sequence_it_kept() {
        use pbps_model::{Column, Hints, IdsFile, Intent, Schema, Table, TableName};

        let serial = |name: Option<&str>| {
            let mut c = Column::new("integer".parse().unwrap()).not_null();
            c.value_source = pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence {
                start: 1,
                increment: 1,
                name: name.map(str::to_owned),
                options: Default::default(),
            });
            c
        };
        let schema = |table: &str, column: Column| {
            let mut t = Table::default();
            t.columns.insert("id".into(), column);
            let mut s = Schema::default();
            s.tables.insert(table.parse::<TableName>().unwrap(), t);
            s
        };
        let context = pbps_diff::Context {
            operator: "test".into(),
            today: "2026-10-12".into(),
        };
        let diff = |base: &Schema, declared: &Schema, intents: &[Intent]| {
            let base_ids = pbps_diff::resolve(base, &IdsFile::default(), &[], &context)
                .unwrap()
                .ids;
            let ids = pbps_diff::resolve(declared, &base_ids, intents, &context)
                .unwrap()
                .ids;
            pbps_diff::diff(
                pbps_diff::Side {
                    schema: base,
                    ids: &base_ids,
                },
                pbps_diff::Side {
                    schema: declared,
                    ids: &ids,
                },
                &super::Postgres::new(),
                &Hints::default(),
            )
        };
        let renamed = |result: Result<pbps_model::ChangeSet, Vec<pbps_diff::DiffError>>,
                       to: &str| {
            result.expect("a rename").changes.iter().any(|p| {
                matches!(
                    &p.change,
                    pbps_model::Change::AlterColumnNumbering {
                        to: pbps_model::ValueSource::Sequence(s),
                        ..
                    } if s.name.as_deref() == Some(to)
                )
            })
        };

        // Against a read of `app.t` before the rename.
        let mut read = schema("app.t", serial(None));
        pbps_dialect::name_read_sequences(&mut read, &super::Postgres::new());
        let rename = [Intent::RenameTable {
            from: "app.t".parse().unwrap(),
            to: "app.u".parse().unwrap(),
        }];
        let kept = diff(&read, &schema("app.u", serial(Some("t_id_seq"))), &rename)
            .expect("the kept name");
        assert!(
            matches!(
                kept.changes.as_slice(),
                [p] if matches!(p.change, pbps_model::Change::RenameTable { .. })
            ),
            "{kept:#?}"
        );
        diff(&read, &schema("app.u", serial(None)), &rename).expect("no name declared");
        assert!(renamed(
            diff(&read, &schema("app.u", serial(Some("other_seq"))), &rename)
                .map(|plan| plan.changes().clone()),
            "other_seq"
        ));

        // Against a git baseline declaring `app.u` after that rename, without
        // the name, and the name a later pull wrote.
        let declared = schema("app.u", serial(None));
        diff(&declared, &schema("app.u", serial(Some("t_id_seq"))), &[])
            .expect("a pulled name against an unnamed declaration");
        assert!(renamed(
            diff(
                &schema("app.u", serial(Some("numbers"))),
                &schema("app.u", serial(Some("t_id_seq"))),
                &[]
            )
            .map(|plan| plan.changes().clone()),
            "t_id_seq"
        ));
    }

    /// An identity put on a nullable column of a table whose declared rows
    /// the plan writes: the column is made NOT NULL first, as the engine
    /// requires, and the rows after, which leave the identity column out and
    /// take its numbers. The tightening is not moved after the rows, nor
    /// split out of a retype, as it is for a column without one (#1367,
    /// #1930 review). A plan that only deletes rows moves both after the
    /// deletes, which may remove the rows holding NULL.
    #[test]
    fn an_identity_put_on_keeps_its_tightening_ahead_of_the_rows() {
        use pbps_model::{
            Change, Column, DataMode, Hints, Identity, IdsFile, PrimaryKey, Row, RowKey, Schema,
            Table, TableData, TableName, Value, ValueSource,
        };
        let shaped = |ty: &str, identity: bool, rows: &[&str]| {
            let mut table = Table::default();
            table.columns.insert(
                "code".into(),
                Column::new("text".parse().unwrap()).not_null(),
            );
            let mut id = Column::new(ty.parse().unwrap());
            if identity {
                id = id.not_null();
                id.value_source = ValueSource::Identity(Identity {
                    seed: 1,
                    increment: 1,
                    options: Default::default(),
                });
            }
            table.columns.insert("id".into(), id);
            table
                .columns
                .insert("label".into(), Column::new("text".parse().unwrap()));
            table.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec!["code".into()],
                storage_parameters: Default::default(),
            });
            table.data = Some(TableData {
                mode: DataMode::Ensure,
                rows: rows
                    .iter()
                    .map(|k| {
                        (
                            RowKey::from(*k),
                            [("label".into(), Value::Text((*k).to_owned()))]
                                .into_iter()
                                .collect::<Row>(),
                        )
                    })
                    .collect(),
            });
            let mut s = Schema::default();
            s.tables.insert(TableName::new("app", "t"), table);
            s
        };
        let context = pbps_diff::Context {
            operator: "test".into(),
            today: "2026-10-13".into(),
        };
        let planned = |base: &Schema, declared: &Schema| {
            let ids = pbps_diff::resolve(base, &IdsFile::default(), &[], &context)
                .unwrap()
                .ids;
            let side = |schema| pbps_diff::Side { schema, ids: &ids };
            pbps_diff::diff(
                side(base),
                side(declared),
                &super::Postgres::new(),
                &Hints::default(),
            )
        };
        let order = |base: &Schema, declared: &Schema| -> Vec<String> {
            planned(base, declared)
                .unwrap()
                .changes
                .iter()
                .filter_map(|p| {
                    if let Change::AlterColumnNullability { .. } = &p.change {
                        Some("tighten".to_owned())
                    } else if let Change::AlterColumnType { to_nullable, .. } = &p.change {
                        Some(format!("retype nullable={to_nullable}"))
                    } else if let Change::AlterColumnNumbering { .. } = &p.change {
                        Some("identity".to_owned())
                    } else if let Change::InsertRow { .. } = &p.change {
                        Some("insert".to_owned())
                    } else if let Change::DeleteRow { .. } = &p.change {
                        Some("delete".to_owned())
                    } else {
                        None
                    }
                })
                .collect()
        };
        let base = shaped("integer", false, &[]);
        assert_eq!(
            order(&base, &shaped("integer", true, &["a"])),
            ["tighten", "identity", "insert"]
        );
        assert_eq!(
            order(&base, &shaped("bigint", true, &["a"])),
            ["retype nullable=false", "identity", "insert"]
        );
        // A plan that only deletes rows may delete the ones holding NULL:
        // the tightening follows the deletes, and the identity follows it.
        let exact = |mut s: Schema| {
            for t in s.tables.values_mut() {
                t.data.as_mut().unwrap().mode = DataMode::Exact;
            }
            s
        };
        let held = exact(shaped("integer", false, &["a", "b"]));
        assert_eq!(
            order(&held, &exact(shaped("integer", true, &["a"]))),
            ["delete", "tighten", "identity"]
        );
        assert_eq!(
            order(&held, &exact(shaped("bigint", true, &["a"]))),
            ["retype nullable=true", "delete", "tighten", "identity"]
        );
        // Deleting and inserting both: the deletes, the tightening, the
        // identity, then the inserts that take its numbers.
        assert_eq!(
            order(&held, &exact(shaped("integer", true, &["a", "c"]))),
            ["delete", "tighten", "identity", "insert"]
        );
        // Then another table's rows referencing it would be written before
        // the rows they may reference, and are refused by name.
        let referencing = |mut s: Schema| {
            let mut r = Table::default();
            r.columns.insert(
                "code".into(),
                Column::new("text".parse().unwrap()).not_null(),
            );
            r.columns
                .insert("t_code".into(), Column::new("text".parse().unwrap()));
            r.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec!["code".into()],
                storage_parameters: Default::default(),
            });
            r.foreign_keys.insert(
                "r_t".into(),
                pbps_model::ForeignKey {
                    columns: vec!["t_code".into()],
                    references_table: TableName::new("app", "t"),
                    references_columns: vec!["code".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
            r.data = Some(TableData {
                mode: DataMode::Ensure,
                rows: [(
                    RowKey::from("x"),
                    [("t_code".into(), Value::Text("c".into()))]
                        .into_iter()
                        .collect::<Row>(),
                )]
                .into_iter()
                .collect(),
            });
            s.tables.insert(TableName::new("app", "r"), r);
            s
        };
        let mut held_r = referencing(held.clone());
        held_r
            .tables
            .get_mut(&TableName::new("app", "r"))
            .unwrap()
            .data
            .as_mut()
            .unwrap()
            .rows
            .clear();
        let errors = planned(
            &held_r,
            &referencing(exact(shaped("integer", true, &["a", "c"]))),
        )
        .unwrap_err();
        assert!(
            matches!(
                errors.as_slice(),
                [pbps_diff::DiffError::RowsReferenceLateIdentityRows { .. }]
            ),
            "{errors:?}"
        );
        // Negative: a referencing table this plan creates gets its key in
        // class 13, after every row, so nothing refuses its rows.
        planned(
            &held,
            &referencing(exact(shaped("integer", true, &["a", "c"]))),
        )
        .expect("the key is added after the rows");
        // Nor a key this plan adds to a table it has.
        let mut unkeyed = held_r.clone();
        unkeyed
            .tables
            .get_mut(&TableName::new("app", "r"))
            .unwrap()
            .foreign_keys
            .clear();
        planned(
            &unkeyed,
            &referencing(exact(shaped("integer", true, &["a", "c"]))),
        )
        .expect("the key is added after the rows");
        // Negative: inserting without deleting keeps both in class 9, where
        // the referencing rows follow, and plans.
        let mut kept = held_r.clone();
        for t in kept.tables.values_mut() {
            if let Some(d) = t.data.as_mut() {
                d.mode = DataMode::Ensure;
            }
        }
        let declared = referencing(shaped("integer", true, &["a", "b", "c"]));
        planned(&kept, &declared).expect("no delete, so the identity stays ahead");
        // Negative: without the identity the tightening follows the rows,
        // and comes out of the retype to do so.
        let mut plain = shaped("integer", true, &["a"]);
        let mut bigint = shaped("bigint", true, &["a"]);
        for s in [&mut plain, &mut bigint] {
            s.tables
                .get_mut(&TableName::new("app", "t"))
                .unwrap()
                .columns
                .get_mut("id")
                .unwrap()
                .value_source = ValueSource::None;
        }
        assert_eq!(order(&base, &plain), ["insert", "tighten"]);
        assert_eq!(
            order(&base, &bigint),
            ["retype nullable=true", "insert", "tighten"]
        );
    }

    /// Each numbering change is one change carrying both sources, gated only
    /// where a writer working today can fail (#1889, DEC-1889.1): an identity
    /// put on, a source given up. Put on, loosened or retuned, it is not; a
    /// known sequence name changed is a rename. Where the table cannot take
    /// one it is refused by name: partitioned, or a sequence created logged
    /// for an `UNLOGGED` table. To or from generated has no in-place form.
    #[test]
    fn a_numbering_change_is_one_change_gated_only_where_a_writer_can_fail() {
        use pbps_model::{
            Change, Column, Hints, Identity, IdsFile, OwnedSequence, RiskClass, Schema, Table,
            TableName, ValueSource,
        };
        let column = |ty: &str, source: ValueSource| {
            let mut c = Column::new(ty.parse().unwrap()).not_null();
            c.value_source = source;
            c
        };
        let schema = |c: Column, shape: &dyn Fn(&mut Table)| {
            let mut table = Table::default();
            table.columns.insert("id".into(), c);
            shape(&mut table);
            let mut s = Schema::default();
            s.tables.insert(TableName::new("app", "t"), table);
            s
        };
        let context = pbps_diff::Context {
            operator: "test".into(),
            today: "2026-10-13".into(),
        };
        let diff = |base: &Schema, declared: &Schema| {
            let ids = pbps_diff::resolve(base, &IdsFile::default(), &[], &context)
                .unwrap()
                .ids;
            pbps_diff::diff(
                pbps_diff::Side {
                    schema: base,
                    ids: &ids,
                },
                pbps_diff::Side {
                    schema: declared,
                    ids: &ids,
                },
                &super::Postgres::new(),
                &Hints::default(),
            )
        };
        let plain = |_: &mut Table| {};
        let serial = |start, increment, name: Option<&str>| {
            ValueSource::Sequence(OwnedSequence {
                start,
                increment,
                name: name.map(str::to_owned),
                options: Default::default(),
            })
        };
        let identity = ValueSource::Identity(Identity {
            seed: 1,
            increment: 1,
            options: Default::default(),
        });
        let none = ValueSource::None;
        for (from, to, risks) in [
            (none.clone(), identity.clone(), vec![RiskClass::Numbering]),
            (none.clone(), serial(1, 1, None), vec![]),
            (
                identity.clone(),
                serial(1, 1, None),
                vec![RiskClass::Numbering],
            ),
            (
                serial(1, 1, None),
                identity.clone(),
                vec![RiskClass::Numbering],
            ),
            (serial(1, 1, None), none.clone(), vec![RiskClass::Numbering]),
            (serial(1, 1, None), serial(9, 3, None), vec![]),
            // A name the declaration leaves out is the one the base has.
            (serial(1, 1, Some("old_seq")), serial(9, 3, None), vec![]),
            (
                ValueSource::Identity(Identity {
                    seed: 9,
                    increment: 3,
                    options: Default::default(),
                }),
                identity.clone(),
                vec![],
            ),
            (
                serial(1, 1, Some("old_seq")),
                serial(1, 1, Some("new_seq")),
                vec![RiskClass::Rename],
            ),
        ] {
            let planned = diff(
                &schema(column("integer", from.clone()), &plain),
                &schema(column("integer", to.clone()), &plain),
            )
            .unwrap_or_else(|e| panic!("{from:?} -> {to:?}: {e:?}"));
            let [p] = planned.changes.as_slice() else {
                panic!("{from:?} -> {to:?}: {planned:#?}");
            };
            assert!(
                matches!(&p.change, Change::AlterColumnNumbering { from: f, to: t, .. }
                    if *f == from && *t == to),
                "{p:#?}"
            );
            assert_eq!(p.risks, risks.into_iter().collect(), "{from:?} -> {to:?}");
        }

        // A plain default on either side is an `AlterColumnDefault` beside
        // the change, ordered as every default is: given up after a source
        // is taken on, taken after a source is given up (#1930 review).
        let default = ValueSource::Default("0".into());
        for (from, to) in [
            (serial(1, 1, None), default.clone()),
            (identity.clone(), default.clone()),
            (default.clone(), identity.clone()),
            (default.clone(), serial(1, 1, None)),
        ] {
            let planned = diff(
                &schema(column("integer", from.clone()), &plain),
                &schema(column("integer", to.clone()), &plain),
            )
            .unwrap();
            let kinds: Vec<_> = planned
                .changes
                .iter()
                .map(|p| {
                    if let Change::AlterColumnNumbering { from, to, .. } = &p.change {
                        assert!(
                            !matches!(from, ValueSource::Default(_))
                                && !matches!(to, ValueSource::Default(_)),
                            "{p:#?}"
                        );
                        "numbering"
                    } else if let Change::AlterColumnDefault { .. } = &p.change {
                        "default"
                    } else {
                        panic!("{p:?}")
                    }
                })
                .collect();
            let expected = if matches!(to, ValueSource::Default(_)) {
                ["numbering", "default"]
            } else {
                ["default", "numbering"]
            };
            assert_eq!(kinds, expected, "{from:?} -> {to:?}");
        }

        // A retype under a kept `serial` moves its sequence with it.
        let retyped = diff(
            &schema(column("integer", serial(1, 1, None)), &plain),
            &schema(column("bigint", serial(1, 1, None)), &plain),
        )
        .unwrap();
        assert!(
            retyped.changes.iter().any(|p| matches!(
                &p.change,
                Change::AlterColumnNumbering { ty, .. } if ty.raw().to_string() == "bigint"
            )),
            "{retyped:#?}"
        );
        // Negative: the same source unchanged is no change.
        assert!(
            diff(
                &schema(column("integer", serial(1, 1, None)), &plain),
                &schema(column("integer", serial(1, 1, None)), &plain),
            )
            .unwrap()
            .is_empty()
        );

        let partitioned = |t: &mut Table| {
            t.partition_by = Some(pbps_model::PartitionBy {
                columns: vec!["id".into()],
                external: false,
                strategy: Default::default(),
            });
        };
        let unlogged = |t: &mut Table| t.unlogged = true;
        for (base, declared, refused) in [
            (
                schema(column("integer", identity.clone()), &plain),
                schema(
                    column(
                        "integer",
                        ValueSource::Identity(Identity {
                            seed: -1,
                            increment: -1,
                            options: Default::default(),
                        }),
                    ),
                    &plain,
                ),
                "direction",
            ),
            (
                schema(column("integer", serial(1, 1, None)), &plain),
                schema(column("integer", serial(-1, -1, None)), &plain),
                "direction",
            ),
            (
                schema(column("integer", none.clone()), &partitioned),
                schema(column("integer", identity.clone()), &partitioned),
                "partitioned",
            ),
            (
                schema(column("integer", none.clone()), &unlogged),
                schema(column("integer", serial(1, 1, None)), &unlogged),
                "unlogged",
            ),
            (
                schema(column("integer", identity.clone()), &plain),
                schema(
                    column(
                        "integer",
                        ValueSource::Generated(pbps_model::Generated {
                            expression: "1".into(),
                            stored: true,
                        }),
                    ),
                    &plain,
                ),
                "generated",
            ),
        ] {
            let errs = diff(&base, &declared).expect_err(refused);
            assert!(
                matches!(
                    (refused, errs.as_slice()),
                    (
                        "partitioned",
                        [pbps_diff::DiffError::NumberingChangeOnPartitionedTable { .. }]
                    ) | (
                        "unlogged",
                        [pbps_diff::DiffError::SequenceChangeUnsupported { .. }]
                    ) | (
                        "generated",
                        [pbps_diff::DiffError::GenerationChangeUnsupported { .. }]
                    ) | (
                        "direction",
                        [pbps_diff::DiffError::NumberingDirectionChange { .. }]
                    )
                ),
                "{refused}: {errs:?}"
            );
        }
        // An identity on an unlogged table takes the table's persistence:
        // not refused.
        assert!(
            diff(
                &schema(column("integer", none.clone()), &unlogged),
                &schema(column("integer", identity.clone()), &unlogged),
            )
            .is_ok()
        );
    }

    /// Owned sequences renamed into each other's names are refused by name
    /// at diff time, as table and column rename cycles are (DEC-541.1,
    /// DEC-1966.1): no order of the renames is one the engine performs
    /// (#1930 review). A chain, where each name is freed before it is
    /// taken, is ordered and not refused.
    #[test]
    fn a_sequence_rename_cycle_is_refused_by_name() {
        use pbps_diff::DiffError;
        use pbps_model::{
            Column, Hints, IdsFile, OwnedSequence, Schema, Table, TableName, ValueSource,
        };
        let schema = |names: &[(&str, &str)]| {
            let mut s = Schema::default();
            for (table, sequence) in names {
                let mut c = Column::new("integer".parse().unwrap()).not_null();
                c.value_source = ValueSource::Sequence(OwnedSequence {
                    start: 1,
                    increment: 1,
                    name: Some((*sequence).to_owned()),
                    options: Default::default(),
                });
                let mut t = Table::default();
                t.columns.insert("id".into(), c);
                s.tables.insert(TableName::new("app", *table), t);
            }
            s
        };
        let context = pbps_diff::Context {
            operator: "test".into(),
            today: "2026-10-14".into(),
        };
        let diff = |base: &Schema, declared: &Schema| {
            let ids = pbps_diff::resolve(base, &IdsFile::default(), &[], &context)
                .unwrap()
                .ids;
            pbps_diff::diff(
                pbps_diff::Side {
                    schema: base,
                    ids: &ids,
                },
                pbps_diff::Side {
                    schema: declared,
                    ids: &ids,
                },
                &super::Postgres::new(),
                &Hints::default(),
            )
        };
        let cycles = |base: &Schema, declared: &Schema| -> Vec<Vec<String>> {
            diff(base, declared)
                .err()
                .unwrap_or_default()
                .iter()
                .filter_map(|e| {
                    if let DiffError::SequenceRenameCycle { renames } = e {
                        Some(
                            renames
                                .iter()
                                .map(|(from, to)| {
                                    format!("{from} -> {to}", from = from.shown(), to = to.shown())
                                })
                                .collect(),
                        )
                    } else {
                        None
                    }
                })
                .collect()
        };
        // Two sequences trading names.
        let base = schema(&[("a", "a_seq"), ("b", "b_seq")]);
        let swap = schema(&[("a", "b_seq"), ("b", "a_seq")]);
        assert_eq!(
            cycles(&base, &swap),
            [vec!["app.a_seq -> app.b_seq", "app.b_seq -> app.a_seq"]]
        );
        let message = diff(&base, &swap)
            .unwrap_err()
            .iter()
            .map(ToString::to_string)
            .collect::<String>();
        assert!(message.contains("separate deployments"), "{message}");
        assert!(message.contains("temporary name"), "{message}");
        // Three rotating.
        let base3 = schema(&[("a", "x"), ("b", "y"), ("c", "z")]);
        let rotated = schema(&[("a", "y"), ("b", "z"), ("c", "x")]);
        assert_eq!(cycles(&base3, &rotated).len(), 1, "one cycle, listed once");
        assert_eq!(cycles(&base3, &rotated)[0].len(), 3);
        // Negative: a chain is ordered, not refused.
        let chain = schema(&[("a", "b_seq"), ("b", "c_seq")]);
        let renames = diff(&base, &chain).expect("a chain is planned");
        assert_eq!(renames.changes.len(), 2, "{renames:?}");
    }

    /// What the block a numbering change runs says, for the parts a live run
    /// cannot see go wrong (#1889): pbps's own reads on the empty path under
    /// the catalog's operators (DEC-1564.1), the table locked before any of
    /// them, and no `RESTART` anywhere, which would reissue numbers.
    #[test]
    fn a_numbering_change_reads_under_the_lock_on_the_catalogs_operators() {
        use pbps_model::{Change, Identity, OwnedSequence, Strategy, TableName, ValueSource};
        let column = TableName::new("app", "t").column("id");
        let emit = |from: ValueSource, to: ValueSource| {
            super::Postgres::new()
                .fixture_emit(
                    &Change::AlterColumnNumbering {
                        uid: "c_a1b2c3".parse().unwrap(),
                        column: column.clone(),
                        ty: "integer".parse().unwrap(),
                        from,
                        to,
                    },
                    Strategy::default(),
                )
                .unwrap()
                .into_iter()
                .map(|s| s.sql)
                .collect::<Vec<_>>()
        };
        let serial = ValueSource::Sequence(OwnedSequence::serial());
        let identity = ValueSource::Identity(Identity {
            seed: 1,
            increment: 1,
            options: Default::default(),
        });
        // One statement, so no checkpoint falls inside it.
        let [converted] = emit(serial.clone(), identity.clone()).try_into().unwrap();
        let lock = converted
            .find("LOCK TABLE \"app\".\"t\" IN ACCESS EXCLUSIVE MODE;")
            .unwrap();
        let counter = converted.find("pbps.pbps_counter;").unwrap();
        let identity_on = converted.find("ADD GENERATED ALWAYS AS IDENTITY").unwrap();
        let read = converted.find("pg_catalog.max(r.\"id\")").unwrap();
        assert!(
            lock < counter && counter < identity_on && identity_on < read,
            "{converted}"
        );
        assert!(converted.contains("OPERATOR(pg_catalog.+)"), "{converted}");
        assert!(!converted.contains(" + "), "{converted}");
        assert!(converted.contains("PERFORM pg_catalog.set_config('search_path', '', true);"));
        let down = emit(
            ValueSource::None,
            ValueSource::Identity(Identity {
                seed: -1,
                increment: -1,
                options: Default::default(),
            }),
        )
        .concat();
        assert!(
            down.contains("pg_catalog.min(r.\"id\")") && down.contains("LEAST("),
            "{down}"
        );
        for retuned in [
            emit(
                serial.clone(),
                ValueSource::Sequence(OwnedSequence {
                    start: 5,
                    increment: 2,
                    name: None,
                    options: Default::default(),
                }),
            ),
            emit(
                identity.clone(),
                ValueSource::Identity(Identity {
                    seed: 5,
                    increment: 2,
                    options: Default::default(),
                }),
            ),
        ] {
            let sql = retuned.concat();
            assert!(!sql.contains("RESTART") && !sql.contains("setval"), "{sql}");
        }
        // Negative: generated is refused, never emitted.
        assert!(
            super::Postgres::new()
                .fixture_emit(
                    &Change::AlterColumnNumbering {
                        uid: "c_a1b2c3".parse().unwrap(),
                        column: column.clone(),
                        ty: "integer".parse().unwrap(),
                        from: ValueSource::Generated(pbps_model::Generated {
                            expression: "1".into(),
                            stored: true,
                        }),
                        to: identity,
                    },
                    Strategy::default(),
                )
                .is_err()
        );
    }

    #[test]
    fn planned_required_add_uses_postgres_default_identifier_boundaries() {
        use pbps_model::{Column, Hints, IdsFile, RiskClass, Schema, Table, TableName};

        let table = TableName::new("app", "t");
        let mut base = Schema::default();
        base.tables.insert(table.clone(), Table::default());
        let context = pbps_diff::Context {
            operator: "test".into(),
            today: "2026-09-12".into(),
        };
        let base_ids = pbps_diff::resolve(&base, &IdsFile::default(), &[], &context)
            .unwrap()
            .ids;
        for (default, risky) in [
            (Some("zz.null\u{301}x()"), false),
            (None, true),
            (Some("NULL"), true),
        ] {
            let mut declared = base.clone();
            let mut column = Column::new("integer".parse().unwrap()).not_null();
            column.value_source = default.map_or(pbps_model::ValueSource::None, |text| {
                pbps_model::ValueSource::Default(text.into())
            });
            declared
                .tables
                .get_mut(&table)
                .unwrap()
                .columns
                .insert("added".into(), column);
            let ids = pbps_diff::resolve(&declared, &base_ids, &[], &context)
                .unwrap()
                .ids;
            let changes = pbps_diff::diff(
                pbps_diff::Side {
                    schema: &base,
                    ids: &base_ids,
                },
                pbps_diff::Side {
                    schema: &declared,
                    ids: &ids,
                },
                &super::Postgres::new(),
                &Hints::default(),
            )
            .unwrap();
            assert_eq!(changes.changes.len(), 1);
            assert_eq!(
                changes.changes[0].risks.contains(&RiskClass::NotNull),
                risky,
                "{default:?}"
            );
        }
    }

    #[test]
    fn required_add_risks_follow_the_dialects_identifier_boundary() {
        use pbps_dialect::{Dialect, MinimalDialect};
        use pbps_model::{Change, Column, RiskClass, Uid, UidKind};

        for (default, pg_risky) in [
            (Some("zz.null\u{301}x()"), false),
            (None, true),
            (Some("NULL"), true),
        ] {
            let mut column = Column::new("integer".parse().unwrap()).not_null();
            column.value_source = default.map_or(pbps_model::ValueSource::None, |text| {
                pbps_model::ValueSource::Default(text.into())
            });
            let change = Change::AddColumn {
                uid: Uid::generate(UidKind::Column),
                table: "app.t".parse().unwrap(),
                name: "added".into(),
                column: Box::new(column),
            };
            assert_eq!(
                super::Postgres::new()
                    .change_risks(&change)
                    .contains(&RiskClass::NotNull),
                pg_risky,
                "{default:?}"
            );
            assert!(
                MinimalDialect
                    .change_risks(&change)
                    .contains(&RiskClass::NotNull),
                "{default:?}"
            );
            assert!(change.intrinsic_risks().contains(&RiskClass::NotNull));
        }
    }
    use super::*;

    #[test]
    fn a_plan_rebuilds_each_caller_once_and_leaves_other_paths_alone() {
        use std::collections::BTreeSet;
        let mut declared = Schema::default();
        let caller: ModuleId = "app.caller()".parse().unwrap();
        declared.modules.insert(
            caller.clone(),
            Module {
                kind: ModuleKind::Function,
                description: None,
                definition: "() RETURNS integer LANGUAGE sql BEGIN ATOMIC SELECT f(1); END".into(),
                options: Default::default(),
            },
        );
        let arrivals: Vec<ModuleId> = ["app.f(integer)", "app.f(smallint)"]
            .into_iter()
            .map(|id| id.parse().unwrap())
            .collect();
        let pg = Postgres::new();
        assert_eq!(
            pg.rebound_modules(&declared, &arrivals, &BTreeSet::new()),
            [caller.clone()].into_iter().collect()
        );
        assert!(
            pg.rebound_modules(&declared, &arrivals, &[caller].into_iter().collect())
                .is_empty()
        );
        for arrival in ["other.f(integer)", "app.unrelated()", "app.t.f"] {
            assert!(
                pg.rebound_modules(&declared, &[arrival.parse().unwrap()], &BTreeSet::new())
                    .is_empty(),
                "{arrival}"
            );
        }
        assert!(
            pg.rebound_modules(&declared, &[], &BTreeSet::new())
                .is_empty()
        );
    }

    #[test]
    fn a_deparsed_module_keeps_its_kind_without_predicting_its_text() {
        let pg = Postgres::new();
        let wrote = Module {
            kind: ModuleKind::View,
            description: Some("a repository annotation".into()),
            definition: "SELECT id FROM app.t".into(),
            options: Default::default(),
        };
        let mut read = Module {
            kind: ModuleKind::View,
            description: None,
            definition: " SELECT t.id\n   FROM app.t;".into(),
            options: Default::default(),
        };
        assert!(pg.module_matches_declaration(&wrote, &read));
        read.kind = ModuleKind::Function;
        assert!(!pg.module_matches_declaration(&wrote, &read));
    }

    /// A bare name resolves through the write path — the object's own schema
    /// first, then the extras in the order they were configured — and
    /// nowhere else. The rank is the order, because the first entry holding
    /// the name is the one it means.
    #[test]
    fn a_bare_name_ranks_the_own_schema_first_and_then_the_extras() {
        let pg = Postgres::with_write_path_extras(vec!["shared".into(), "public".into()]);
        assert_eq!(Dialect::bare_name_rank(&pg, "app", "app"), Some(0));
        assert_eq!(Dialect::bare_name_rank(&pg, "app", "shared"), Some(1));
        assert_eq!(Dialect::bare_name_rank(&pg, "app", "public"), Some(2));
        assert_eq!(Dialect::bare_name_rank(&pg, "app", "other"), None);
        assert_eq!(
            Dialect::bare_name_rank(&pg, "app", "App"),
            None,
            "names are exact"
        );
        // An extra that is also the object's own schema is still first.
        assert_eq!(Dialect::bare_name_rank(&pg, "shared", "shared"), Some(0));
        assert_eq!(
            Dialect::bare_name_rank(&Postgres::new(), "app", "shared"),
            None
        );
    }

    /// ADR-0010 §3, DECISIONS 211: the principal is the *cluster's*. This
    /// crate's answer is what every role path reads — the differ builds no
    /// `CreateRole`, `DropRole` or `RenameRole` on a dialect that says `false`
    /// — so the answer itself is worth a test of its own.
    #[test]
    fn a_role_is_not_this_tools_to_create_rename_or_drop_on_this_engine() {
        assert!(!Postgres::new().manages_roles());
    }

    /// A refusal is output like any other, so what it says is tested: each
    /// names the step that supplies it, and none of them reads as "there is
    /// nothing to do".
    #[test]
    fn an_unbuilt_part_refuses_by_name_and_never_reads_as_nothing_to_do() {
        // A list of one, deliberately: roles (step 6) and the probes (step 9)
        // each left it as they landed, and the day a part is added back it is
        // this array that grows rather than a second test being written.
        #[expect(
            clippy::single_element_loop,
            reason = "the shape is the point: every variant is walked"
        )]
        for part in [Unbuilt::Introspection] {
            let message = part.refuse().to_string();
            assert!(message.contains("Phase 5 step"), "{message}");
            // "does not implement ... yet", never "does not support": the
            // second would say the engine lacks the feature.
            assert!(message.contains("does not implement"), "{message}");
            assert!(message.ends_with("yet"), "{message}");
        }
    }

    /// `emit` returning an empty statement list would be a plan that applies
    /// cleanly and changes nothing — the silent wrong answer, and the reason
    /// every part this crate has not built is an *error*.
    ///
    /// With roles (step 6) and reference data (step 7) in, **no [`Change`]
    /// reaches [`Unbuilt`] at all**: the two parts that remain are read from
    /// the connected paths rather than from `emit`. What is left to pin is
    /// therefore the other direction — the one change that empties on purpose,
    /// and the fact that nothing else joins it.
    #[test]
    fn the_only_change_that_emits_nothing_is_the_one_with_nothing_to_run() {
        let pg = Postgres::new();
        // The mode is a property of the declaration: it decides what *future*
        // plans do about undeclared rows, and the row changes it implies are
        // separate entries in this same plan.
        assert!(
            pg.fixture_emit(
                &Change::SetDataMode {
                    table: "app.t".parse().expect("a table name parses"),
                    from: None,
                    to: Some(pbps_model::DataMode::Exact),
                },
                Strategy::default(),
            )
            .expect("the mode is built")
            .is_empty()
        );
        // And the three this dialect will not perform refuse rather than
        // emitting nothing, which is what keeps "pbps does not own the
        // principal" from reading as "there was nothing to do" (ADR-0010 §3).
        let uid = pbps_model::Uid::generate(pbps_model::UidKind::Role);
        for change in [
            Change::CreateRole {
                uid: uid.clone(),
                name: "analyst".to_owned().into(),
            },
            Change::DropRole {
                uid: uid.clone(),
                name: "analyst".to_owned().into(),
                members: Vec::new(),
            },
            Change::RenameRole {
                uid,
                from: "analyst".to_owned().into(),
                to: "reader".to_owned().into(),
            },
        ] {
            pg.fixture_emit(&change, Strategy::default())
                .expect_err("the principal is the cluster's");
        }
    }

    /// A default that is NULL is one this engine erases, so the declaration
    /// is refused before any plan restates it forever (DECISIONS 351).
    #[test]
    fn a_null_default_is_refused_in_every_spelling_the_engine_erases() {
        let mut table = Table::default();
        // (column, its type, its default): the first group the engine erases
        // — a NULL of the column's own unmodified type, however spelled —
        // and the second it keeps (DECISIONS 351, 361).
        let erased = [
            ("bare", "text", "NULL"),
            ("parens", "text", "(null)"),
            ("cast", "text", "NULL::text"),
            ("standard", "text", "CAST(NULL AS text)"),
            ("commented", "text", "NULL /* note */::text"),
            ("cast_commented", "text", "CAST(NULL /* note */ AS text)"),
            ("led", "text", "/* lead */ NULL"),
            ("typed_comment", "text", "CAST(NULL AS text /* note */)"),
            ("alias", "integer", "NULL::int"),
            (
                "worded",
                "timestamptz",
                "CAST(NULL AS timestamp with time zone)",
            ),
            ("twice", "text", "NULL::varchar::text"),
            (
                "worded_comment",
                "double precision",
                "CAST(NULL AS double /* note */ precision)",
            ),
            ("cr_comment", "text", "CAST(NULL -- note\r AS text)"),
            ("glued", "text", "CAST(NULL/**/AS/**/text)"),
            // The type as the grammar sees it: qualified, quoted, spaced,
            // folded (DECISIONS 364).
            ("qualified", "text", "NULL::pg_catalog.text"),
            ("qualified_call", "text", "CAST(NULL AS pg_catalog.text)"),
            ("quoted", "text", "NULL::\"text\""),
            (
                "quoted_both",
                "text",
                "CAST(NULL AS \"pg_catalog\" . \"text\")",
            ),
            ("spaced", "text", "CAST(NULL AS pg_catalog/**/./**/text)"),
            ("folded", "integer", "NULL::PG_CATALOG.INT4"),
            ("quoted_alias", "boolean", "NULL::\"bool\""),
            (
                "qualified_time",
                "timestamptz",
                "NULL::pg_catalog.timestamptz",
            ),
            ("glued_quote", "text", "CAST(NULL AS\"text\")"),
            // A Unicode-escaped type name is the name it spells
            // (DECISIONS 369).
            ("unicode", "text", "NULL::U&\"te\\0078t\""),
            ("unicode_call", "text", "CAST(NULL AS U&\"te\\0078t\")"),
            (
                "unicode_uescape",
                "text",
                "NULL::U&\"te!0078t\" UESCAPE '!'",
            ),
            (
                "unicode_qualified",
                "integer",
                "NULL::U&\"pg_catalog\".u&\"int\\0034\"",
            ),
        ];
        let kept = [
            ("other_type", "text", "NULL::varchar"),
            ("modified_column", "varchar(10)", "NULL::varchar(10)"),
            (
                "modified_cast",
                "timestamptz",
                "NULL::timestamp(3) with time zone",
            ),
            (
                "modified_numeric",
                "numeric(5,2)",
                "CAST(NULL AS numeric(5,2))",
            ),
            (
                "modified_comment",
                "numeric(5,2)",
                "CAST(NULL AS numeric(5, /* c */ 2))",
            ),
            ("wider", "integer", "NULL::bigint"),
            // Another schema's type is another type, a grammar word is no
            // catalog name, and `"char"` is not `character` (DECISIONS 364).
            ("other_schema", "text", "NULL::app.text"),
            ("no_such_type", "integer", "NULL::pg_catalog.integer"),
            ("quoted_grammar", "text", "CAST(NULL AS \"TEXT\")"),
            ("internal_char", "character", "NULL::\"char\""),
            ("catalog_char", "character", "NULL::pg_catalog.bpchar"),
            ("unicode_other", "text", "NULL::U&\"varch\\0061r\""),
            ("unicode_broken", "text", "NULL::U&\"te\\00zzt\""),
            ("value", "text", "'x'"),
            ("expression", "text", "NULLIF('a', 'a')"),
        ];
        for (name, ty, default) in erased.iter().chain(&kept) {
            let mut c = pbps_model::Column::new(ty.parse().expect("a type"));
            c.value_source = pbps_model::ValueSource::Default((*default).into());
            table.columns.insert((*name).into(), c);
        }
        table.columns.insert(
            "none".into(),
            pbps_model::Column::new("text".parse().expect("a type")),
        );

        let problems =
            Postgres::new().validate_table(&"app.t".parse().expect("a table name parses"), &table);
        let mut named: Vec<String> = problems
            .iter()
            .map(|e| {
                let m = e.to_string();
                assert!(m.contains("declare no default"), "{m}");
                m.split('`').nth(1).expect("a column name").to_owned()
            })
            .collect();
        named.sort();
        let mut expected: Vec<&str> = erased.iter().map(|(n, _, _)| *n).collect();
        expected.sort_unstable();
        assert_eq!(named, expected, "{problems:?}");
    }

    /// The one rule the emitter cannot enforce on every path, enforced where it
    /// can be.
    ///
    /// `AlterColumnDefault` carries a column reference and two expressions and
    /// no type, so `emit` cannot tell a bare `'01/02/2026'` on a `date` from
    /// one on a `text` — and it is only on a `date` that the applying session
    /// decides the value. `validate_table` sees the declaration, and every
    /// command that hands statements to a database runs it (DECISIONS 141), so
    /// the change never gets planned in the first place.
    #[test]
    fn validating_a_table_names_a_default_the_applying_session_would_decide() {
        let mut table = Table::default();
        let mut d = pbps_model::Column::new("date".parse().expect("a type"));
        d.value_source = pbps_model::ValueSource::Default("'01/02/2026'".into());
        table.columns.insert("d".into(), d);
        // The same text on a type whose input function reads no setting is not
        // a problem, and saying it were would refuse a valid declaration.
        let mut label = pbps_model::Column::new("text".parse().expect("a type"));
        label.value_source = pbps_model::ValueSource::Default("'01/02/2026'".into());
        table.columns.insert("label".into(), label);
        // Nor is the resolved spelling, which is what the pull reads back.
        let mut ok = pbps_model::Column::new("date".parse().expect("a type"));
        ok.value_source = pbps_model::ValueSource::Default("'2026-01-02'::date".into());
        table.columns.insert("resolved".into(), ok);

        let problems =
            Postgres::new().validate_table(&"app.t".parse().expect("a table name parses"), &table);
        assert_eq!(problems.len(), 1, "{problems:?}");
        let message = problems[0].to_string();
        assert!(message.contains("column `d`"), "{message}");
        assert!(message.contains("DateStyle"), "{message}");
    }

    /// A table declared where the pull will not look is refused before
    /// anything connects, and `pg_temp` is why the rule is not only about
    /// visibility.
    ///
    /// Measured, `pg_temp` is this engine's alias for the session's temporary
    /// schema: `CREATE TABLE "pg_temp"."t"` succeeds and leaves `pg_temp_58.t`
    /// with `relpersistence = 't'`, under a name the declaration never wrote
    /// and gone when the connection closes. An engine that refuses by name can
    /// be left to refuse; one that hands back something else cannot
    /// (DECISIONS 266).
    #[test]
    fn validating_a_table_refuses_a_schema_the_pull_would_never_read() {
        let one = |name: &str| {
            let mut table = Table::default();
            table.columns.insert(
                "id".into(),
                pbps_model::Column::new("integer".parse().expect("a type")),
            );
            Postgres::new().validate_table(&name.parse().expect("a table name parses"), &table)
        };
        for name in [
            "pg_temp.t",
            "pg_catalog.t",
            "information_schema.t",
            "pg_toast.t",
        ] {
            let problems = one(name);
            assert_eq!(problems.len(), 1, "`{name}`: {problems:?}");
            let message = problems[0].to_string();
            assert!(message.contains("never reads"), "`{name}`: {message}");
        }
        // The negative case, and it is the one that matters: a project schema
        // whose name merely begins with the same letters as the reader's
        // exclusion is not excluded by it — the reader compares the first
        // three characters against `pg_`, so `pga` is a project's schema and
        // has to stay one.
        for name in ["pga.t", "app.t", "public.t", "pg.t"] {
            assert!(one(name).is_empty(), "`{name}` is a project's own");
        }
    }

    /// A schema named `$user` is refused for a different reason from the
    /// hidden ones: the table would be created and read back fine, and what
    /// breaks is every unqualified name inside it.
    #[test]
    fn validating_a_table_refuses_the_schema_name_a_path_substitutes() {
        let mut table = Table::default();
        table.columns.insert(
            "id".into(),
            pbps_model::Column::new("integer".parse().expect("a type")),
        );
        let problems =
            Postgres::new().validate_table(&pbps_model::TableName::new("$user", "t"), &table);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].to_string().contains("role's own schema"),
            "{}",
            problems[0]
        );
        // Exactly that name, because the engine compares exactly that name.
        for ok in ["$users", "$USER", "app"] {
            assert!(
                Postgres::new()
                    .validate_table(&pbps_model::TableName::new(ok, "t"), &table)
                    .is_empty(),
                "`{ok}` is an ordinary schema"
            );
        }
    }

    /// Only the ledger's qualified table identities are reserved.
    ///
    /// The negative case is the whole reason the reader's filter names the two
    /// rather than matching a prefix: a project's own `__pbps_customers` is a
    /// project's table, and refusing it would refuse a declaration the pull
    /// reads perfectly well.
    #[test]
    fn validating_a_table_refuses_the_two_names_this_tool_owns() {
        let one = |name: &str| {
            let mut table = Table::default();
            table.columns.insert(
                "id".into(),
                pbps_model::Column::new("integer".parse().expect("a type")),
            );
            Postgres::new().validate_table(&name.parse().expect("a table name parses"), &table)
        };
        for name in [crate::state::STATE_TABLE, crate::state::LOCK_TABLE] {
            let problems = one(name);
            assert_eq!(problems.len(), 1, "`{name}`: {problems:?}");
            assert!(
                problems[0].to_string().contains("this tool owns"),
                "`{name}`: {}",
                problems[0]
            );
        }
        for name in [
            "app.__pbps_state",
            "app.__pbps_lock",
            "Public.__pbps_state",
            "public.__pbps_customers",
            "app.__pbps_customers",
            "app.__pbps_statement",
            "app.pbps_state",
            "app.customers",
        ] {
            assert!(one(name).is_empty(), "`{name}` is a project's own");
        }
    }

    /// Unquoted identifiers fold down, not away: this is the difference from
    /// SQL Server that the loader sees before anything connects.
    #[test]
    fn an_unquoted_identifier_folds_to_lower_case() {
        assert_eq!(Postgres::new().fold_ident("Customer"), "customer");
        assert_eq!(Postgres::new().fold_ident("CUSTOMER"), "customer");
        // Already lower: borrowed, not copied.
        assert!(matches!(
            Postgres::new().fold_ident("customer"),
            Cow::Borrowed(_)
        ));
    }

    /// And it folds the ASCII letters **only**. Measured on 18.6,
    /// `CREATE TABLE AÄ` makes the relation `aÄ`: the server leaves every byte
    /// with the high bit set alone. A Unicode-aware fold would key the
    /// declaration as `aä`, which introspection never returns.
    #[test]
    fn folding_leaves_every_letter_the_engine_leaves() {
        assert_eq!(Postgres::new().fold_ident("AÄ"), "aÄ");
        assert_eq!(Postgres::new().fold_ident("STRASSE"), "strasse");
        assert_eq!(Postgres::new().fold_ident("Straße"), "straße");
        // A name with no ASCII upper case at all is untouched, and borrowed.
        assert!(matches!(Postgres::new().fold_ident("Ä"), Cow::Borrowed(_)));
        assert_eq!(Postgres::new().fold_ident("Ä"), "Ä");
    }

    /// The server truncates a long identifier instead of refusing it, and says
    /// so only in a `NOTICE`. Refusing here is what keeps the model and the
    /// server naming the same object.
    #[test]
    fn a_name_the_server_would_truncate_is_refused_by_bytes_not_characters() {
        assert!(
            Postgres::new()
                .quote_ident(&"a".repeat(MAX_IDENT_BYTES))
                .is_ok()
        );
        assert!(
            Postgres::new()
                .quote_ident(&"a".repeat(MAX_IDENT_BYTES + 1))
                .is_err()
        );
        // 31 `ä` is 62 bytes and legal; 32 is 64 bytes and is not — measured,
        // the engine cuts it back to 31 on a character boundary. Counted as
        // characters this would have been the other way round.
        let long = "ä".repeat(32);
        assert_eq!(long.chars().count(), 32);
        assert_eq!(long.len(), 64);
        assert!(Postgres::new().quote_ident(&"ä".repeat(31)).is_ok());
        assert!(Postgres::new().quote_ident(&long).is_err());
    }

    /// Quoting is what stops a name from being read as syntax, so the
    /// negative cases are the ones worth having.
    #[test]
    fn quoting_doubles_an_embedded_quote_and_refuses_what_cannot_be_a_name() {
        assert_eq!(
            Postgres::new().quote_ident("customer").unwrap(),
            "\"customer\""
        );
        assert_eq!(
            Postgres::new().quote_ident("Odd Name").unwrap(),
            "\"Odd Name\""
        );
        assert_eq!(Postgres::new().quote_ident("a\"b").unwrap(), "\"a\"\"b\"");
        assert!(Postgres::new().quote_ident("").is_err());
        assert!(Postgres::new().quote_ident("a\0b").is_err());
    }

    /// The three rows of ADR-0011 Amendment 2's table, through this dialect.
    /// Each was measured against the engine, and each of the first two is the
    /// **silent** failure: two definitions returning different strings compared
    /// equal, so the change was never planned at all.
    #[test]
    fn a_definition_is_scanned_with_this_engines_literals_and_not_sql_servers() {
        // `$tag$…$tag$` holds data, and no escape can close it early.
        assert_ne!(
            Postgres::new().normalize_definition("SELECT $tag$a  b$tag$"),
            Postgres::new().normalize_definition("SELECT $tag$a b$tag$")
        );
        // `\'` does not close an `E'…'` string: measured, `E'it\'s  here'` is
        // one ten-character literal.
        assert_ne!(
            Postgres::new().normalize_definition(r"SELECT E'it\'s  here'"),
            Postgres::new().normalize_definition(r"SELECT E'it\'s here'")
        );
        // A `[` is a subscript here, so a reindent inside one is not a change.
        assert_eq!(
            Postgres::new().normalize_definition("SELECT a[1  +  2] FROM t"),
            Postgres::new().normalize_definition("SELECT a[1 + 2] FROM t")
        );
        // And a plain literal is still data, as on any engine.
        assert_ne!(
            Postgres::new().normalize_definition("SELECT 'a  b'"),
            Postgres::new().normalize_definition("SELECT 'a b'")
        );
    }

    /// `serial` is refused, not normalized, and the refusal says what to write
    /// instead. Normalizing it to anything would produce a schema that differs
    /// from itself on every run (ADR-0011 Amendment 3).
    #[test]
    fn a_serial_column_is_refused_by_name_and_never_normalized() {
        for (declared, reads_back) in [
            ("serial", "integer"),
            ("serial4", "integer"),
            ("SERIAL", "integer"),
            ("smallserial", "smallint"),
            ("serial2", "smallint"),
            ("bigserial", "bigint"),
            ("serial8", "bigint"),
        ] {
            let error = Postgres::new()
                .normalize_type(&ty(declared))
                .expect_err("a macro is not a type");
            let message = error.to_string();
            assert!(message.contains("macro, not a type"), "{message}");
            assert!(message.contains(reads_back), "{message}");
            assert!(message.contains("identity:"), "{message}");
            // Not the "come back after the next release" refusal: no build of
            // this dialect will ever normalize one.
            assert!(!message.contains("does not implement"), "{message}");
        }
    }

    /// The closed list is the point: a type whose name merely starts with the
    /// same letters is the catalogue's business, not this refusal's.
    #[test]
    fn a_type_that_is_not_in_the_serial_family_is_left_to_the_catalogue() {
        for spelling in ["integer", "text"] {
            assert!(
                Postgres::new().normalize_type(&ty(spelling)).is_ok(),
                "{spelling}"
            );
        }
        for spelling in ["serialized", "bigserialx"] {
            let message = Postgres::new()
                .normalize_type(&ty(spelling))
                .expect_err("the catalogue does not hold it")
                .to_string();
            assert!(message.contains("has no type"), "{message}");
            assert!(!message.contains("macro, not a type"), "{message}");
        }
    }

    /// A declaration is refused where a user is looking at it — beside the
    /// column, at validate time — and not only where the catalogue is asked.
    #[test]
    fn validating_a_table_names_the_serial_column() {
        let mut table = Table::default();
        table
            .columns
            .insert("id".into(), pbps_model::Column::new(ty("serial")));
        table
            .columns
            .insert("note".into(), pbps_model::Column::new(ty("text")));
        let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &table);
        let serial: Vec<_> = found
            .iter()
            .map(ToString::to_string)
            .filter(|m| m.contains("macro, not a type"))
            .collect();
        assert_eq!(serial.len(), 1, "{found:?}");
        assert!(serial[0].contains("column `id`"), "{}", serial[0]);
        // And the `text` column beside it is fine, so the refusal names the
        // one declaration to change rather than the whole table.
        assert_eq!(found.len(), 1, "{found:?}");
    }

    /// Normalization preserves reference identity rather than guessing a
    /// catalogue base type (#1523).
    #[test]
    fn a_user_type_reference_normalizes_idempotently_with_its_identity_and_array_marker() {
        for spelling in ["app.mood", "APP.Mood", "app.mood[]", "\"App\".\"Mood\"[]"] {
            let declared = ty(spelling);
            let normalized = Postgres::new().normalize_type(&declared).unwrap();
            assert_eq!(declared, normalized);
            assert_eq!(
                normalized,
                Postgres::new().normalize_type(&normalized).unwrap()
            );
        }
        assert!(Postgres::new().normalize_type(&ty("unknown")).is_err());
    }

    /// The engine truncates an identifier over [`MAX_IDENT_BYTES`] without an
    /// error, so a reference with such a part would name a different type from
    /// the one written (#2021). The model holds the length; this dialect refuses
    /// it, before the refusal every reference gets until they are admitted, and
    /// counts bytes and not characters (**measured** on 16.15 and 18.6: 32 `é`,
    /// which is 64 bytes, are stored as 31).
    #[test]
    fn a_type_reference_part_over_the_identifier_limit_is_refused_by_length() {
        let limit = "a".repeat(MAX_IDENT_BYTES);
        let over = "a".repeat(MAX_IDENT_BYTES + 1);
        // 31 `é` and an `a` are 63 bytes in 32 characters; 32 `é` are 64.
        let multibyte_fits = format!("{}a", "\u{e9}".repeat(31));
        let multibyte_over = "\u{e9}".repeat(32);
        for part in [&over, &multibyte_over] {
            for declared in [
                format!("app.{part}"),
                format!("{part}.mood"),
                format!("\"App\".\"{part}\""),
                format!("app.{part}[]"),
                format!("{}.{part}", "b".repeat(100)),
            ] {
                let message = Postgres::new()
                    .normalize_type(&ty(&declared))
                    .expect_err(&declared)
                    .to_string();
                assert!(
                    message.contains("PostgreSQL keeps 63 bytes of an identifier")
                        && message.contains("truncating the rest without an error"),
                    "{declared}: {message}"
                );
                assert!(
                    !message.contains("not supported yet"),
                    "the length refusal comes first: {message}"
                );
                assert_eq!(
                    message.contains("[]"),
                    declared.ends_with("[]"),
                    "{message}"
                );
            }
        }
        // The size is named, and the schema is the part named when both are over.
        let message = Postgres::new()
            .normalize_type(&ty(&format!("{}.{over}", "b".repeat(100))))
            .unwrap_err()
            .to_string();
        assert!(message.contains("whose schema is 100 bytes"), "{message}");
        let message = Postgres::new()
            .normalize_type(&ty(&format!("app.{over}")))
            .unwrap_err()
            .to_string();
        assert!(message.contains("whose name is 64 bytes"), "{message}");
        for part in [&limit, &multibyte_fits] {
            for declared in [
                format!("app.{part}"),
                format!("{part}.mood"),
                format!("\"{part}\".\"{part}\""),
                format!("app.{part}[]"),
            ] {
                assert!(
                    Postgres::new().normalize_type(&ty(&declared)).is_ok(),
                    "{declared}"
                );
            }
        }
        // And a built-in is untouched by the check.
        assert!(Postgres::new().normalize_type(&ty("text[]")).is_ok());
    }

    /// Where `validate` looks at the declaration, a long reference is named with
    /// the length refusal, and a column of the limit with the generic one (#2021).
    #[test]
    fn validating_a_table_refuses_a_long_type_reference_by_length() {
        let mut table = Table::default();
        for (column, spelling) in [
            ("long", format!("app.{}", "a".repeat(MAX_IDENT_BYTES + 1))),
            ("limit", format!("app.{}", "a".repeat(MAX_IDENT_BYTES))),
        ] {
            table
                .columns
                .insert(column.into(), pbps_model::Column::new(ty(&spelling)));
        }
        let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &table);
        let messages: Vec<String> = found.iter().map(ToString::to_string).collect();
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(
            messages.iter().filter(|m| m.contains("truncating")).count(),
            1,
            "{messages:?}"
        );
    }

    /// Where a user looks at the declaration, `validate` names the column's
    /// type once, beside the other columns, which it leaves alone (#1523).
    #[test]
    fn references_are_admitted_as_columns_but_refused_as_partition_keys() {
        let name = "app.t".parse().unwrap();
        for spelling in ["mood", "mood\n\\name"] {
            let column: pbps_model::ColumnName = spelling.into();
            let mut table = Table::default();
            for (column, spelling) in [
                (column.clone(), "app.mood"),
                ("moods".into(), "app.mood[]"),
                ("note".into(), "text"),
            ] {
                table
                    .columns
                    .insert(column, pbps_model::Column::new(ty(spelling)));
            }
            assert!(Postgres::new().validate_table(&name, &table).is_empty());
            table.partition_by = Some(pbps_model::PartitionBy {
                strategy: pbps_model::PartitionStrategy::Range,
                columns: vec![column.clone()],
                external: false,
            });
            let found = Postgres::new().validate_table(&name, &table);
            let message = found
                .iter()
                .map(ToString::to_string)
                .find(|m| m.contains("user-defined type reference"))
                .expect("a referenced partition key refuses");
            assert!(
                message.contains(&format!("partition key `{}`", column.shown())),
                "{message}"
            );
        }
    }

    #[test]
    fn a_scalar_reference_refuses_declared_data_by_type_before_any_row_read() {
        let name = "app.t".parse().unwrap();
        for spelling in ["mood", "mood\n\\name"] {
            let column: pbps_model::ColumnName = spelling.into();
            let mut table = Table::default();
            table
                .columns
                .insert(column.clone(), pbps_model::Column::new(ty("app.mood")));
            table.data = Some(pbps_model::TableData {
                mode: pbps_model::DataMode::Ensure,
                rows: Default::default(),
            });
            let message = Postgres::new()
                .validate_table(&name, &table)
                .iter()
                .map(ToString::to_string)
                .find(|m| m.contains("`data:`") && m.contains("user-defined type reference"))
                .expect("reference data refuses before reading rows");
            assert!(
                message.contains(&format!("column `{}`", column.shown())),
                "{message}"
            );
        }
    }

    /// A name the emitter cannot spell is a table `validate` must not call
    /// clean. The limit is `quote_ident`'s, and it is enforced nowhere else:
    /// the server truncates instead of refusing, so a declaration that got
    /// past here would make an object under a name nobody asked for.
    #[test]
    fn validating_a_table_refuses_a_name_the_emitter_could_not_spell() {
        let long = "a".repeat(MAX_IDENT_BYTES + 1);
        let mut table = Table::default();
        table
            .columns
            .insert(long.clone().into(), pbps_model::Column::new(ty("integer")));
        let found =
            Postgres::new().validate_table(&TableName::new(long.clone(), long.clone()), &table);
        // The schema, the table and the column: three names, three refusals.
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(
            found
                .iter()
                .all(|e| matches!(e, DialectError::UnquotableIdent(_))),
            "{found:?}"
        );

        // And a table whose names all fit is clean, so this is a limit and not
        // a blanket refusal.
        let mut ok = Table::default();
        ok.columns
            .insert("id".into(), pbps_model::Column::new(ty("integer")));
        assert!(
            Postgres::new()
                .validate_table(&"app.t".parse().unwrap(), &ok)
                .is_empty()
        );
    }

    fn structural_table() -> Table {
        let mut table = Table::default();
        for name in ["a", "b"] {
            table.columns.insert(
                name.into(),
                pbps_model::Column::new(ty("integer")).not_null(),
            );
        }
        table
            .columns
            .insert("j".into(), pbps_model::Column::new(ty("JSON")));
        table
    }

    fn structural_errors(table: &Table) -> Vec<String> {
        Postgres::new()
            .validate_table(&TableName::new("app", "t"), table)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn non_ascii_whitespace_is_an_expression_identifier_not_an_empty_expression() {
        for expression in ["\u{a0}", "\u{2003}", "\t\u{a0}\n"] {
            let mut table = structural_table();
            table.columns.insert(
                expression.trim_ascii().into(),
                pbps_model::Column::new(ty("boolean")),
            );
            table.checks.insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: expression.into(),
                },
            );
            table.indexes.insert(
                "ix".into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column("a".into()),
                        descending: false,
                        opclass: None,
                    }],
                    include: vec![],
                    unique: false,
                    filter: Some(expression.into()),
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
            let errors = structural_errors(&table);
            assert!(errors.is_empty(), "{expression:?}: {errors:?}");
            // ASCII whitespace still contains no expression on either path.
            table.checks.get_mut("ck").unwrap().expression = " \t\r\n\x0c".into();
            table.indexes.get_mut("ix").unwrap().filter = Some(" \t\r\n\x0c".into());
            assert_eq!(structural_errors(&table).len(), 2);
        }
    }

    #[test]
    fn every_independent_key_structure_mistake_is_reported_in_one_pass() {
        let mut table = structural_table();
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["missing".into()],
            storage_parameters: Default::default(),
        });
        table.unique.insert(
            "empty".into(),
            pbps_model::UniqueConstraint {
                columns: vec![],
                storage_parameters: Default::default(),
            },
        );
        table.unique.insert(
            "repeated".into(),
            pbps_model::UniqueConstraint {
                columns: vec!["a".into(), "a".into()],
                storage_parameters: Default::default(),
            },
        );
        table.indexes.insert(
            "json_key".into(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Column("j".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        let errors = structural_errors(&table);
        assert_eq!(errors.len(), 3, "{errors:?}");
        for expected in ["not a column", "names no columns", "twice"] {
            assert_eq!(
                errors.iter().filter(|e| e.contains(expected)).count(),
                1,
                "{errors:?}"
            );
        }
        assert!(structural_errors(&structural_table()).is_empty());
    }

    #[test]
    fn primary_and_unique_keys_require_nonempty_existing_distinct_columns() {
        for primary in [true, false] {
            for (columns, expected) in [
                (vec![], "names no columns"),
                (vec!["missing"], "not a column"),
                (vec!["a", "a"], "twice"),
                (vec!["j"], ""),
                (vec!["a", "b"], ""),
            ] {
                let mut table = structural_table();
                // The nullable-PK rule is independently pinned; isolate type here.
                table.columns.get_mut("j").unwrap().nullable = false;
                let columns = columns
                    .into_iter()
                    .map(pbps_model::ColumnName::from)
                    .collect();
                if primary {
                    table.primary_key = Some(pbps_model::PrimaryKey {
                        name: None,
                        columns,
                        storage_parameters: Default::default(),
                    });
                } else {
                    table.unique.insert(
                        "uq".into(),
                        pbps_model::UniqueConstraint {
                            columns,
                            storage_parameters: Default::default(),
                        },
                    );
                }
                let errors = structural_errors(&table);
                if expected.is_empty() {
                    assert!(errors.is_empty(), "{errors:?}");
                } else {
                    assert_eq!(errors.len(), 1, "{errors:?}");
                    assert!(errors[0].contains(expected), "{errors:?}");
                }
            }
        }
    }

    #[test]
    fn foreign_keys_require_local_columns_and_matching_nonempty_lists_but_allow_repetition() {
        for (local, remote, expected) in [
            (vec![], vec![], "names no columns"),
            (vec!["a"], vec![], "referenced table"),
            (vec!["missing"], vec!["a"], "not a column"),
            (vec!["j"], vec!["a"], ""),
            (vec!["a", "b"], vec!["a"], "must line up"),
            // Measured on 18.6 and left alone: a foreign key accepts repeated
            // *local* columns against a distinct composite unique key
            // (DECISIONS 452), so refusing this would reject a valid
            // declaration.
            (vec!["a", "a"], vec!["a", "b"], ""),
            // The other list, and the opposite answer: `42830: foreign key
            // referenced-columns list must not contain duplicates` — an
            // analysis rule of the engine rather than a missing-key error,
            // since `CREATE UNIQUE INDEX … (a, a)` is itself legal here
            // (issue #476).
            (
                vec!["a", "b"],
                vec!["a", "a"],
                "twice on the referenced table",
            ),
        ] {
            let mut table = structural_table();
            table.foreign_keys.insert(
                "fk".into(),
                pbps_model::ForeignKey {
                    columns: local
                        .into_iter()
                        .map(pbps_model::ColumnName::from)
                        .collect(),
                    references_table: TableName::new("app", "parent"),
                    references_columns: remote
                        .into_iter()
                        .map(pbps_model::ColumnName::from)
                        .collect(),
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
            let errors = structural_errors(&table);
            if expected.is_empty() {
                assert!(errors.is_empty(), "{errors:?}");
            } else {
                assert!(errors.iter().any(|e| e.contains(expected)), "{errors:?}");
            }
        }
    }

    /// One mistake, one sentence. A referenced list repeating a column three
    /// times is still one thing to fix, and the remedy is the same for both
    /// extra copies — while two *different* repeated names are two mistakes
    /// and are each named (issue #476).
    #[test]
    fn a_repeated_referenced_column_is_named_once_however_often_it_repeats() {
        for (remote, expected) in [
            (vec!["a", "a", "a"], vec!["`a`"]),
            (vec!["a", "a", "b", "b"], vec!["`a`", "`b`"]),
        ] {
            let mut table = structural_table();
            let local: Vec<pbps_model::ColumnName> =
                (0..remote.len()).map(|i| format!("c{i}").into()).collect();
            for column in &local {
                table.columns.insert(
                    column.clone(),
                    pbps_model::Column::new(ty("integer")).not_null(),
                );
            }
            table.foreign_keys.insert(
                "fk".into(),
                pbps_model::ForeignKey {
                    columns: local,
                    references_table: TableName::new("app", "parent"),
                    references_columns: remote.iter().map(|c| (*c).into()).collect(),
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
            let errors = structural_errors(&table);
            assert_eq!(errors.len(), expected.len(), "{remote:?}: {errors:?}");
            for (error, name) in errors.iter().zip(&expected) {
                assert!(error.contains(name), "{remote:?}: {errors:?}");
                assert!(
                    error.contains("twice on the referenced table"),
                    "{remote:?}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn indexes_require_local_keys_and_includes_but_accept_repetition_and_json_payloads() {
        for (keys, include, expected) in [
            (vec![], vec![], "names no columns"),
            (vec!["missing"], vec![], "not a column"),
            (vec!["j"], vec![], ""),
            (vec!["a"], vec!["missing"], "includes `missing`"),
            (vec!["a", "a"], vec!["a", "b", "b", "j"], ""),
        ] {
            let mut table = structural_table();
            table.indexes.insert(
                "ix".into(),
                pbps_model::Index {
                    columns: keys
                        .into_iter()
                        .map(|name| pbps_model::IndexColumn {
                            key: pbps_model::IndexKey::Column(name.into()),
                            descending: false,
                            opclass: None,
                        })
                        .collect(),
                    include: include
                        .into_iter()
                        .map(pbps_model::ColumnName::from)
                        .collect(),
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
            let errors = structural_errors(&table);
            if expected.is_empty() {
                assert!(errors.is_empty(), "{errors:?}");
            } else {
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert!(errors[0].contains(expected), "{errors:?}");
            }
        }
    }

    #[test]
    fn the_server_build_decides_the_index_limit_not_offline_validation() {
        for count in [32, 33] {
            // `foreign` beside the four index shapes: measured on 18.6, a
            // 33-column foreign key is `54011: cannot have more than 32 keys
            // in a foreign key`, the same SQLSTATE and the same number the
            // index limit answers with, and the same compile-time
            // `max_index_keys` behind both. So it is the server's answer to
            // give, not this validator's, for exactly the reason DECISIONS 452
            // gives for the index limit (issue #475).
            for kind in ["primary", "unique", "index", "include", "foreign"] {
                let mut table = structural_table();
                let columns: Vec<pbps_model::ColumnName> =
                    (0..count).map(|i| format!("c{i}").into()).collect();
                for column in &columns {
                    table.columns.insert(
                        column.clone(),
                        pbps_model::Column::new(ty("integer")).not_null(),
                    );
                }
                match kind {
                    "primary" => {
                        table.primary_key = Some(pbps_model::PrimaryKey {
                            name: None,
                            columns,
                            storage_parameters: Default::default(),
                        })
                    }
                    "unique" => {
                        table.unique.insert(
                            "uq".into(),
                            pbps_model::UniqueConstraint {
                                columns,
                                storage_parameters: Default::default(),
                            },
                        );
                    }
                    "foreign" => {
                        table.foreign_keys.insert(
                            "fk".into(),
                            pbps_model::ForeignKey {
                                columns: columns.clone(),
                                references_table: TableName::new("app", "parent"),
                                references_columns: columns,
                                on_delete: Default::default(),
                                on_update: Default::default(),
                            },
                        );
                    }
                    _ => {
                        let (keys, include) = if kind == "include" {
                            (columns[..1].to_vec(), columns[1..].to_vec())
                        } else {
                            (columns, vec![])
                        };
                        table.indexes.insert(
                            "ix".into(),
                            pbps_model::Index {
                                columns: keys
                                    .into_iter()
                                    .map(|name| pbps_model::IndexColumn {
                                        key: pbps_model::IndexKey::Column(name),
                                        descending: false,
                                        opclass: None,
                                    })
                                    .collect(),
                                include,
                                unique: false,
                                filter: None,
                                method: Default::default(),
                                storage_parameters: Default::default(),
                            },
                        );
                    }
                }
                let errors = structural_errors(&table);
                assert!(errors.is_empty(), "{kind} {count}: {errors:?}");
            }
        }
    }

    /// A column collation is SQL Server's in the model; this engine refuses
    /// the key rather than create the column under another one (#1175).
    #[test]
    fn a_column_collation_is_refused_on_this_engine() {
        let mut table = structural_table();
        assert!(structural_errors(&table).is_empty());
        table.columns.get_mut("a").unwrap().collation = Some(pbps_model::Collation::new("C"));
        let errors = structural_errors(&table);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("SQL Server only"), "{errors:?}");
    }

    /// `clustered` is SQL Server's table layout; this engine has no clustered
    /// index to create, so the line is refused whatever it names — even a
    /// well-formed selector (#1178).
    #[test]
    fn a_clustered_layout_is_refused_on_this_engine() {
        let mut table = structural_table();
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["a".into()],
            storage_parameters: Default::default(),
        });
        assert!(structural_errors(&table).is_empty());
        table.clustered = Some(pbps_model::Clustered::Heap);
        let errors = structural_errors(&table);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("SQL Server table layout"), "{errors:?}");
    }

    #[test]
    fn empty_checks_and_filters_are_reported_together_with_other_structural_errors() {
        let mut table = structural_table();
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["missing".into()],
            storage_parameters: Default::default(),
        });
        table.checks.insert(
            "ck".into(),
            pbps_model::CheckConstraint {
                expression: " \n ".into(),
            },
        );
        table.indexes.insert(
            "ix".into(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Column("a".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: Some(" \t".into()),
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        let errors = structural_errors(&table);
        assert_eq!(errors.len(), 3, "{errors:?}");
        for expected in ["not a column", "empty expression", "empty filter"] {
            assert!(errors.iter().any(|e| e.contains(expected)), "{errors:?}");
        }
        table.primary_key = None;
        table.checks.get_mut("ck").unwrap().expression = "a > 0".into();
        table.indexes.get_mut("ix").unwrap().filter = Some("b IS NOT NULL".into());
        assert!(structural_errors(&table).is_empty());
    }

    /// An expression is empty when the *engine* would find nothing in it, and
    /// this engine's layout is neither Rust's ASCII whitespace class nor its
    /// Unicode one: it separates tokens with a vertical tab, keeps every
    /// non-ASCII space inside an identifier, and treats both comment forms as
    /// layout (issues #480 and #482, DECISIONS 504).
    ///
    /// Both call sites, because they are the same question asked twice: a
    /// check constraint's expression and a partial index's filter.
    #[test]
    fn an_expression_is_empty_by_this_engines_layout_not_by_rusts_whitespace() {
        let empty = [
            "\u{b}",         // the vertical tab `trim_ascii` leaves out
            " \t\u{b}\r\n",  // and beside the ones it does not
            "-- nothing\n",  // a line comment is layout
            "/* nothing */", // so is a block comment
            "/* a /* b */ c */",
            "  /* a */ \t -- b\n",
        ];
        for expression in empty {
            let mut table = structural_table();
            table.checks.insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: expression.into(),
                },
            );
            table.indexes.insert(
                "ix".into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column("a".into()),
                        descending: false,
                        opclass: None,
                    }],
                    include: vec![],
                    unique: false,
                    filter: Some(expression.into()),
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
            let errors = structural_errors(&table);
            assert_eq!(errors.len(), 2, "{expression:?}: {errors:?}");
            for expected in ["empty expression", "empty filter"] {
                assert!(
                    errors.iter().any(|e| e.contains(expected)),
                    "{expression:?}: {errors:?}"
                );
            }
        }

        // The negatives, and they are the half that matters: each of these is
        // a declaration the engine accepts or refuses on its own grounds, so
        // refusing it here would refuse a valid plan. `\u{a0}` names a column,
        // `'true'` is a whole expression, and the text that reads like a
        // comment is inside a literal.
        let present = [
            "\u{a0}",
            "\t\u{a0}\n",
            "/* a */ a > 0",
            "a > 0 -- trailing\n",
            "'true'",
            "'-- not a comment'",
            "\"a\"",
            "/* a", // unreadable, not empty: the engine names that itself
        ];
        for expression in present {
            let mut table = structural_table();
            table
                .columns
                .insert("\u{a0}".into(), pbps_model::Column::new(ty("boolean")));
            table.checks.insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: expression.into(),
                },
            );
            table.indexes.insert(
                "ix".into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column("a".into()),
                        descending: false,
                        opclass: None,
                    }],
                    include: vec![],
                    unique: false,
                    filter: Some(expression.into()),
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
            assert!(
                structural_errors(&table).is_empty(),
                "{expression:?}: {:?}",
                structural_errors(&table)
            );
        }
    }

    /// The names a table owns are not only its own and its columns'. Each of
    /// these is emitted as an identifier and truncated by the server at the
    /// same limit, so `validate` has to ask about all of them or a plan fails
    /// on a name it printed itself.
    #[test]
    fn validating_a_table_refuses_every_owned_name_the_emitter_could_not_spell() {
        let long = "a".repeat(MAX_IDENT_BYTES + 1);
        let mut table = Table::default();
        table.columns.insert(
            "id".into(),
            // `not_null` because a nullable key column is its own finding, and
            // this test is about names.
            pbps_model::Column::new(ty("integer")).not_null(),
        );
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: Some(long.clone().into()),
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        table.unique.insert(
            format!("uq_{long}").into(),
            pbps_model::UniqueConstraint {
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            },
        );
        table.checks.insert(
            format!("ck_{long}").into(),
            pbps_model::CheckConstraint {
                expression: "id > 0".to_owned(),
            },
        );
        let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &table);
        // The primary key, the unique constraint and the check: three names.
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(
            found
                .iter()
                .all(|e| matches!(e, DialectError::UnquotableIdent(_))),
            "{found:?}"
        );
    }

    /// Measured on 18.6, and the engine says it in so many words: `identity
    /// column type must be smallint, integer, or bigint`. A `numeric` is
    /// refused even with a scale of zero, which is where SQL Server's rule and
    /// this one part company.
    #[test]
    fn an_identity_is_refused_on_a_type_this_engine_will_not_carry_one_on() {
        for (declared, accepted) in [
            ("smallint", true),
            ("integer", true),
            ("bigint", true),
            ("numeric(10,0)", false),
            ("numeric", false),
            ("text", false),
            ("uuid", false),
            ("real", false),
        ] {
            let mut table = Table::default();
            let mut column = pbps_model::Column::new(ty(declared));
            column.nullable = false;
            column.value_source = pbps_model::ValueSource::Identity(pbps_model::Identity {
                seed: 1,
                increment: 1,
                options: Default::default(),
            });
            table.columns.insert("id".into(), column);
            let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &table);
            assert_eq!(
                found.is_empty(),
                accepted,
                "`{declared}` as an identity: {found:?}"
            );
            if !accepted {
                assert!(
                    found[0]
                        .to_string()
                        .contains("smallint, integer, or bigint")
                );
            }
        }
    }

    /// The three the engine refuses for a reason that is not the type:
    /// A generated column is refused beside a default or an identity, as a
    /// virtual one, and with an empty expression; a stored one alone is
    /// accepted (DEC-1168.1).
    #[test]
    fn a_generated_column_is_held_to_what_the_engine_and_the_model_take() {
        let generated = |expression: &str, stored: bool| {
            let mut c = pbps_model::Column::new(ty("integer"));
            c.value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
                expression: expression.into(),
                stored,
            });
            c
        };
        let problems = |column: pbps_model::Column, additional: Option<pbps_model::ValueSource>| {
            let sources = additional
                .map(|source| {
                    std::collections::BTreeMap::from([(
                        "b".into(),
                        vec![source, column.value_source.clone()],
                    )])
                })
                .unwrap_or_default();
            let mut table = Table::default();
            table
                .columns
                .insert("a".into(), pbps_model::Column::new(ty("integer")));
            table.columns.insert("b".into(), column);
            Postgres::new()
                .validate_table_sources(
                    &"app.t".parse().unwrap(),
                    &table,
                    pbps_model::diagnostic::ColumnSources::new(Some(&sources)),
                )
                .into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        };
        assert!(problems(generated("a * 2", true), None).is_empty());
        let mut identity = generated("a * 2", true);
        identity.nullable = false;
        for (column, additional, expected) in [
            (
                generated("a * 2", true),
                Some(pbps_model::ValueSource::Default("0".into())),
                "both a default and a generation expression",
            ),
            (
                identity,
                Some(pbps_model::ValueSource::Identity(pbps_model::Identity {
                    seed: 1,
                    increment: 1,
                    options: Default::default(),
                })),
                "both an identity and a generated column",
            ),
            (generated("a * 2", false), None, "virtual generated column"),
            (
                generated(" -- nothing", true),
                None,
                "empty generation expression",
            ),
        ] {
            let found = problems(column, additional);
            assert!(
                found.iter().any(|m| m.contains(expected)),
                "{expected}: {found:?}"
            );
        }
    }

    /// `conflicting NULL/NOT NULL declarations`, `both default and identity
    /// specified for column`, and `INCREMENT must not be zero`. Each is a rule
    /// the shipped SQL Server dialect carries too; the one that does *not*
    /// cross over is "only one IDENTITY per table", which this engine allows.
    #[test]
    fn an_identity_that_is_nullable_or_defaulted_or_never_advances_is_refused() {
        let mut table = Table::default();
        let mut column = pbps_model::Column::new(ty("integer"));
        column.nullable = true;
        column.value_source = pbps_model::ValueSource::Identity(pbps_model::Identity {
            seed: 1,
            increment: 0,
            options: Default::default(),
        });
        table.columns.insert("id".into(), column.clone());
        // A second identity column, which this engine takes: it must not add a
        // refusal of its own, only the three this one already earns.
        table.columns.insert("other".into(), {
            let mut c = pbps_model::Column::new(ty("bigint"));
            c.nullable = false;
            c.value_source = pbps_model::ValueSource::Identity(pbps_model::Identity {
                seed: 1,
                increment: 1,
                options: Default::default(),
            });
            c
        });
        // Invalid declarations belong to the diagnostic boundary; a Column
        // itself can no longer contain both sources.
        let sources = std::collections::BTreeMap::from([(
            "id".into(),
            vec![
                pbps_model::ValueSource::Default("7".into()),
                column.value_source.clone(),
            ],
        )]);
        let found = Postgres::new().validate_table_sources(
            &"app.t".parse().unwrap(),
            &table,
            pbps_model::diagnostic::ColumnSources::new(Some(&sources)),
        );
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found.iter().any(|e| e.to_string().contains("nullable")));
        assert!(found.iter().any(|e| e.to_string().contains("`default:`")));
        assert!(
            found
                .iter()
                .any(|e| e.to_string().contains("never advances"))
        );
    }

    /// A `serial` is NOT NULL with its sequence's default and no other value
    /// source, on a plain table or partitioned parent, and names its sequence
    /// only where the name is not the engine's own (#1631, DEC-1631.1).
    #[test]
    fn a_serial_that_is_nullable_defaulted_on_a_partition_or_misnamed_is_refused() {
        let serial = |name: Option<&str>| {
            let mut c = pbps_model::Column::new(ty("integer")).not_null();
            c.value_source = pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence {
                start: 1,
                increment: 1,
                name: name.map(str::to_owned),
                options: Default::default(),
            });
            c
        };
        let t: TableName = "app.t".parse().unwrap();
        let found = |table: &Table| {
            Postgres::new()
                .validate_table(&t, table)
                .into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        };
        // The negative case: a plain one, and one under another name.
        let mut table = Table::default();
        table.columns.insert("id".into(), serial(None));
        table
            .columns
            .insert("other".into(), serial(Some("other_numbers")));
        assert_eq!(found(&table), Vec::<String>::new());

        let mut bad = serial(Some("t_id_seq"));
        bad.nullable = true;
        table.columns.insert("id".into(), bad.clone());
        let refused = found(&table);
        assert_eq!(refused.len(), 2, "{refused:?}");
        assert!(
            refused.iter().any(|m| m.contains("NOT NULL")),
            "{refused:?}"
        );
        assert!(
            refused.iter().any(|m| m.contains("leave out `name:`")),
            "{refused:?}"
        );
        // A `default:` beside it is a second source, which the loader keeps
        // beside the column for the dialect to name.
        let sources: std::collections::BTreeMap<
            pbps_model::ColumnName,
            Vec<pbps_model::ValueSource>,
        > = [(
            "id".into(),
            vec![
                pbps_model::ValueSource::Default("7".to_owned()),
                bad.value_source.clone(),
            ],
        )]
        .into_iter()
        .collect();
        let refused: Vec<String> = Postgres::new()
            .validate_table_sources(
                &t,
                &table,
                pbps_model::diagnostic::ColumnSources::new(Some(&sources)),
            )
            .into_iter()
            .map(|e| e.to_string())
            .collect();
        assert!(
            refused
                .iter()
                .any(|m| m.contains("is a `serial` and has a `default:`")),
            "{refused:?}"
        );

        let mut parent = Table::default();
        parent.columns.insert("id".into(), serial(None));
        parent.partition_by = Some(pbps_model::PartitionBy {
            external: false,
            columns: vec!["id".into()],
            strategy: Default::default(),
        });
        assert!(found(&parent).is_empty(), "{:?}", found(&parent));
        parent.partition_by = None;
        parent.partition_of = Some(pbps_model::PartitionOf {
            parent: "app.p".parse().unwrap(),
            bound: pbps_model::PartitionBound::Default,
            columns: Default::default(),
        });
        assert!(
            found(&parent)
                .iter()
                .any(|m| m.contains("`serial` on a partition;")),
            "{:?}",
            found(&parent)
        );

        // A name the emitter could not write is refused here, before a plan.
        for unwritable in ["", "a\0b"] {
            let mut table = Table::default();
            table.columns.insert("id".into(), serial(Some(unwritable)));
            let refused = found(&table);
            assert!(
                refused
                    .iter()
                    .any(|m| m.contains("cannot be written into SQL")),
                "{unwritable:?}: {refused:?}"
            );
        }
    }

    /// A standing parent's serial changes use the ordinary table's changes;
    /// identity conversions still name their partition limitation (DEC-2219.1).
    #[test]
    fn a_standing_parents_serial_changes_plan_but_identity_conversions_are_refused() {
        use pbps_model::{
            Change, Column, Hints, IdsFile, Intent, OwnedSequence, Schema, ValueSource,
        };
        let table_name = TableName::new("app", "t");
        let schema = |partitioned: bool, source: Option<ValueSource>| {
            let mut table = Table::default();
            table.columns.insert("v".into(), Column::new(ty("integer")));
            if let Some(source) = source {
                let mut c = Column::new(ty("integer")).not_null();
                c.value_source = source;
                table.columns.insert("n".into(), c);
            }
            if partitioned {
                table.partition_by = Some(pbps_model::PartitionBy {
                    columns: vec!["v".into()],
                    external: false,
                    strategy: Default::default(),
                });
            }
            let mut schema = Schema::default();
            schema.tables.insert(table_name.clone(), table);
            schema
        };
        let serial = ValueSource::Sequence(OwnedSequence::serial());
        let context = pbps_diff::Context {
            operator: "test".into(),
            today: "2026-10-19".into(),
        };
        for (from, to, what) in [
            (None, Some(serial.clone()), "adding a `serial` column"),
            (
                Some(ValueSource::None),
                Some(serial.clone()),
                "changing numbering to or from a `serial`",
            ),
            (
                Some(serial.clone()),
                Some(ValueSource::None),
                "changing numbering to or from a `serial`",
            ),
            (
                Some(serial.clone()),
                Some(ValueSource::Sequence(OwnedSequence {
                    start: 5,
                    ..OwnedSequence::serial()
                })),
                "changing numbering to or from a `serial`",
            ),
            (
                Some(serial.clone()),
                Some(ValueSource::Default("42".into())),
                "changing numbering to or from a `serial`",
            ),
            (
                Some(ValueSource::Default("42".into())),
                Some(serial.clone()),
                "changing numbering to or from a `serial`",
            ),
            (
                Some(serial.clone()),
                Some(ValueSource::Identity(pbps_model::Identity::new(1, 1))),
                "identity conversion",
            ),
            (
                Some(ValueSource::Identity(pbps_model::Identity::new(1, 1))),
                Some(serial.clone()),
                "identity conversion",
            ),
            (Some(serial.clone()), None, "dropping a `serial` column"),
        ] {
            for partitioned in [true, false] {
                let base = schema(partitioned, from.clone());
                let declared = schema(partitioned, to.clone());
                let base_ids = pbps_diff::resolve(&base, &IdsFile::default(), &[], &context)
                    .unwrap()
                    .ids;
                let intents = if to.is_none() {
                    vec![Intent::DropColumn {
                        column: table_name.column("n"),
                        reason: "retired".parse().unwrap(),
                    }]
                } else {
                    vec![]
                };
                let declared_ids = pbps_diff::resolve(&declared, &base_ids, &intents, &context)
                    .unwrap()
                    .ids;
                let outcome = pbps_diff::diff(
                    pbps_diff::Side {
                        schema: &base,
                        ids: &base_ids,
                    },
                    pbps_diff::Side {
                        schema: &declared,
                        ids: &declared_ids,
                    },
                    &Postgres::new(),
                    &Hints::default(),
                );
                if partitioned && what == "identity conversion" {
                    let errors = outcome.expect_err(what);
                    assert!(
                        matches!(
                            errors.as_slice(),
                            [pbps_diff::DiffError::NumberingChangeOnPartitionedTable { .. }]
                        ),
                        "{errors:?}"
                    );
                    let message = errors[0].to_string();
                    assert!(
                        message.contains("app.t.n") && message.contains("#1681"),
                        "{message}"
                    );
                } else {
                    let changes = outcome.expect("a plain table retains the same change");
                    assert!(
                        changes.changes.iter().any(|p| matches!(
                            (what, &p.change),
                            ("adding a `serial` column", Change::AddColumn { .. })
                                | (
                                    "changing numbering to or from a `serial`"
                                        | "identity conversion",
                                    Change::AlterColumnNumbering { .. }
                                )
                                | ("dropping a `serial` column", Change::DropColumn { .. })
                        )),
                        "{changes:?}"
                    );
                }
            }
        }
    }

    /// A sequence's bounds, cycle and cache are held to what the engine takes
    /// and reads back the same (#1921, DEC-1921.1): a bound inside the type,
    /// not spelled as its own default, bounds that do not cross, a start
    /// between them, and a cache of at least two. A bound the declaration
    /// moves widens where a start may be, as the engine's own does. SQL
    /// Server's refusal is pinned beside its own validation.
    #[test]
    fn sequence_options_the_engine_refuses_or_reads_back_otherwise_are_refused() {
        use pbps_model::{Identity, OwnedSequence, SequenceOptions, ValueSource};
        let t: TableName = "app.t".parse().unwrap();
        let found = |source: ValueSource| {
            let mut c = pbps_model::Column::new(ty("integer")).not_null();
            c.value_source = source;
            let mut table = Table::default();
            table.columns.insert("id".into(), c);
            Postgres::new()
                .validate_table(&t, &table)
                .into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        };
        let identity = |seed, increment, options| {
            ValueSource::Identity(Identity {
                seed,
                increment,
                options,
            })
        };
        let serial = |start, increment, options| {
            ValueSource::Sequence(OwnedSequence {
                start,
                increment,
                name: None,
                options,
            })
        };
        let o = |min, max, cycle, cache| SequenceOptions {
            min,
            max,
            cycle,
            cache,
        };
        // Negative: each option the engine takes, and a start a lowered
        // minimum admits, are accepted on both kinds.
        for options in [
            o(Some(-5), Some(100), true, Some(20)),
            o(None, Some(1000), false, None),
            o(Some(0), None, false, Some(2)),
        ] {
            assert_eq!(
                found(identity(0.max(options.min.unwrap_or(1)), 1, options)),
                Vec::<String>::new()
            );
            assert_eq!(
                found(serial(0.max(options.min.unwrap_or(1)), 1, options)),
                Vec::<String>::new()
            );
        }
        assert_eq!(
            found(identity(0, 1, o(Some(-5), None, false, None))),
            Vec::<String>::new()
        );
        // Counting down, the minimum is the bound it counts toward.
        assert_eq!(
            found(serial(-1, -1, o(Some(-100), None, false, None))),
            Vec::<String>::new()
        );
        for (source, says) in [
            (
                identity(1, 1, o(None, Some(i64::MAX), false, None)),
                "outside `integer`",
            ),
            (
                serial(1, 1, o(Some(i64::MIN), None, false, None)),
                "outside `integer`",
            ),
            (
                identity(1, 1, o(None, Some(2147483647), false, None)),
                "leave out `max:`",
            ),
            (
                serial(1, 1, o(Some(1), None, false, None)),
                "leave out `min:`",
            ),
            (
                serial(-1, -1, o(None, Some(-1), false, None)),
                "leave out `max:`",
            ),
            (
                identity(5, 1, o(Some(5), Some(5), false, None)),
                "must be below its maximum",
            ),
            (
                serial(10, 1, o(Some(20), Some(10), false, None)),
                "must be below its maximum",
            ),
            (identity(1, 1, o(None, None, false, Some(0))), "CACHE (0)"),
            (
                serial(1, 1, o(None, None, false, Some(1))),
                "leave out `cache:`",
            ),
            // A start outside the bounds the declaration sets.
            (identity(500, 1, o(None, Some(100), false, None)), "500"),
            (serial(1, 1, o(Some(5), None, false, None)), "starts at 1"),
        ] {
            let refused = found(source.clone());
            assert!(
                refused.iter().any(|m| m.contains(says)),
                "{source:?}: {refused:?}"
            );
        }
    }

    /// A sequence that cycles hands out, once it wraps, a number a row
    /// already holds, so on a column under a primary key, a unique
    /// constraint or a unique index it is refused, naming the key (#1921,
    /// DEC-1921.1). Under a unique index whose expression names the column it
    /// is accepted with a note, since only the expression decides whether a
    /// wrap collides (#2014). The same sequence on a column under no key is
    /// accepted, and beside a non-unique expression index, a unique one over
    /// another column, or a literal that spells the name, with no note.
    #[test]
    fn a_cycling_sequence_under_a_key_is_refused_naming_the_key() {
        let t: TableName = "app.t".parse().unwrap();
        let cycling = |kind: &str| {
            let mut c = pbps_model::Column::new(ty("integer")).not_null();
            let options = pbps_model::SequenceOptions {
                max: Some(100),
                cycle: true,
                ..Default::default()
            };
            c.value_source = if kind == "identity" {
                pbps_model::ValueSource::Identity(pbps_model::Identity {
                    seed: 1,
                    increment: 1,
                    options,
                })
            } else {
                pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence {
                    options,
                    ..pbps_model::OwnedSequence::serial()
                })
            };
            c
        };
        let found = |table: &Table| {
            Postgres::new()
                .validate_table(&t, table)
                .into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        };
        for kind in ["identity", "serial"] {
            let mut table = Table::default();
            table.columns.insert("id".into(), cycling(kind));
            table
                .columns
                .insert("v".into(), pbps_model::Column::new(ty("integer")));
            assert_eq!(found(&table), Vec::<String>::new(), "{kind} under no key");

            let mut keyed = table.clone();
            keyed.primary_key = Some(pbps_model::PrimaryKey {
                name: Some("t_pkey".into()),
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            let refused = found(&keyed);
            assert!(
                refused.iter().any(|m| m.contains("primary key `t_pkey`")),
                "{kind}: {refused:?}"
            );

            let mut unique = table.clone();
            unique.unique.insert(
                "t_id_v".into(),
                pbps_model::UniqueConstraint {
                    columns: vec!["v".into(), "id".into()],
                    storage_parameters: Default::default(),
                },
            );
            let refused = found(&unique);
            assert!(
                refused
                    .iter()
                    .any(|m| m.contains("unique constraint `t_id_v`")),
                "{kind}: {refused:?}"
            );

            // A unique expression index over the column is a note, not a
            // refusal: whether a wrap collides there depends on the
            // expression (#2014). A non-unique one, one over another column,
            // and a literal that spells the name give no note.
            let expression = |unique, text: &str| pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Expression(text.into()),
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            };
            let noted = |table: &Table| {
                let mut schema = pbps_model::Schema::default();
                schema.tables.insert(t.clone(), table.clone());
                Postgres::new().declaration_notes(&schema)
            };
            for text in ["(id + 0)", "NULLIF(id, id)"] {
                let mut over = table.clone();
                over.indexes
                    .insert("t_id_expr".into(), expression(true, text));
                assert_eq!(found(&over), Vec::<String>::new(), "{kind}: {text}");
                let notes = noted(&over);
                assert!(
                    notes.len() == 1
                        && notes[0].contains("app.t.id has `cycle: true`")
                        && notes[0].contains("unique index `t_id_expr`"),
                    "{kind}: {text}: {notes:?}"
                );
            }
            for (unique, text) in [(false, "(id + 0)"), (true, "(v + 0)"), (true, "('id')")] {
                let mut other = table.clone();
                other
                    .indexes
                    .insert("t_expr".into(), expression(unique, text));
                assert_eq!(found(&other), Vec::<String>::new(), "{kind}: {text}");
                assert_eq!(
                    noted(&other),
                    Vec::<String>::new(),
                    "{kind}: {unique} {text}"
                );
            }
        }
    }

    /// A kept source retuned is gated as `numbering` only where its numbers
    /// end sooner, making a writer fail where it went on before (#1921,
    /// DEC-1921.1): without cycling, the bound it counts toward set or lowered
    /// (raised, counting down), or its cycle turned off. A bound tightened
    /// while the destination cycles only wraps sooner. A bound raised or
    /// removed, a cycle turned on, and a cache changed are not gated.
    #[test]
    fn a_retune_is_gated_only_where_its_numbers_end_sooner() {
        use pbps_model::{
            Change, Hints, Identity, IdsFile, OwnedSequence, RiskClass, Schema, SequenceOptions,
            TableName, ValueSource,
        };
        let o = |min, max, cycle, cache| SequenceOptions {
            min,
            max,
            cycle,
            cache,
        };
        let schema = |source: ValueSource| {
            let mut c = pbps_model::Column::new(ty("integer")).not_null();
            c.value_source = source;
            let mut table = Table::default();
            table.columns.insert("id".into(), c);
            let mut s = Schema::default();
            s.tables.insert(TableName::new("app", "t"), table);
            s
        };
        let context = pbps_diff::Context {
            operator: "test".into(),
            today: "2026-10-15".into(),
        };
        let gated = |from: ValueSource, to: ValueSource| {
            let base = schema(from);
            let declared = schema(to);
            let ids = pbps_diff::resolve(&base, &IdsFile::default(), &[], &context)
                .unwrap()
                .ids;
            let cs = pbps_diff::diff(
                pbps_diff::Side {
                    schema: &base,
                    ids: &ids,
                },
                pbps_diff::Side {
                    schema: &declared,
                    ids: &ids,
                },
                &super::Postgres::new(),
                &Hints::default(),
            )
            .unwrap();
            assert_eq!(cs.changes.len(), 1, "{cs:?}");
            assert!(matches!(
                cs.changes[0].change,
                Change::AlterColumnNumbering { .. }
            ));
            cs.changes[0].risks.contains(&RiskClass::Numbering)
        };
        let up = |options| {
            ValueSource::Identity(Identity {
                seed: 1,
                increment: 1,
                options,
            })
        };
        let down = |options| {
            ValueSource::Sequence(OwnedSequence {
                start: -1,
                increment: -1,
                name: None,
                options,
            })
        };
        let none = o(None, None, false, None);
        for (from, to, expected) in [
            (up(none), up(o(None, Some(100), false, None)), true),
            (
                up(o(None, Some(100), false, None)),
                up(o(None, Some(50), false, None)),
                true,
            ),
            (
                up(o(None, Some(100), true, None)),
                up(o(None, Some(100), false, None)),
                true,
            ),
            (
                up(o(None, Some(100), true, None)),
                up(o(None, Some(50), false, None)),
                true,
            ),
            (down(none), down(o(Some(-100), None, false, None)), true),
            (
                down(o(Some(-100), None, false, None)),
                down(o(Some(-50), None, false, None)),
                true,
            ),
            (
                down(o(Some(-100), None, true, None)),
                down(o(Some(-50), None, false, None)),
                true,
            ),
            // Negative: each change that ends no sooner.
            (
                up(o(None, Some(100), true, None)),
                up(o(None, Some(50), true, None)),
                false,
            ),
            (
                down(o(Some(-100), None, true, None)),
                down(o(Some(-50), None, true, None)),
                false,
            ),
            (
                up(o(None, Some(100), false, None)),
                up(o(None, Some(50), true, None)),
                false,
            ),
            (
                down(o(Some(-100), None, false, None)),
                down(o(Some(-50), None, true, None)),
                false,
            ),
            (
                up(o(None, Some(100), false, None)),
                up(o(None, Some(500), false, None)),
                false,
            ),
            (up(o(None, Some(100), false, None)), up(none), false),
            (up(none), up(o(None, None, true, None)), false),
            (up(none), up(o(None, None, false, Some(50))), false),
            (up(none), up(o(Some(-10), None, false, None)), false),
            (down(none), down(o(None, Some(-5), false, None)), false),
        ] {
            assert_eq!(
                gated(from.clone(), to.clone()),
                expected,
                "{from:?} -> {to:?}"
            );
        }
    }

    /// A `serial`'s sequence is `AS` the column's type with an identity's
    /// bounds, so its start and increment are held to the same ranges, and a
    /// type no serial stands for is refused (#1631).
    #[test]
    fn a_serials_start_and_type_are_held_to_the_sequences_bounds() {
        let t: TableName = "app.t".parse().unwrap();
        let refused = |type_: &str, start: i64, increment: i64| {
            let mut c = pbps_model::Column::new(ty(type_)).not_null();
            c.value_source = pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence {
                start,
                increment,
                name: None,
                options: Default::default(),
            });
            let mut table = Table::default();
            table.columns.insert("id".into(), c);
            Postgres::new().validate_table(&t, &table)
        };
        assert!(refused("integer", 1, 1).is_empty());
        assert!(refused("bigint", -1, -1).is_empty());
        assert_eq!(refused("integer", 0, 1).len(), 1);
        assert_eq!(refused("smallint", 40_000, 1).len(), 1);
        assert_eq!(refused("integer", 1, 0).len(), 1);
        assert_eq!(refused("numeric(10, 0)", 1, 1).len(), 1);
    }

    /// The seed is bounded by the sequence, not by the column.
    ///
    /// Every pair here was measured on 18.6. Two of them are the point: `0` on
    /// an `integer` fits the type by any reading and is refused, and `5`
    /// counting down fits the type and is refused for being too *large*.
    #[test]
    fn an_identity_seed_outside_the_sequences_own_bounds_is_refused() {
        for (declared, seed, increment, accepted) in [
            ("smallint", 1, 1, true),
            ("smallint", 32767, 1, true),
            ("smallint", 32768, 1, false),
            ("integer", 2_147_483_647, 1, true),
            ("integer", 2_147_483_648, 1, false),
            // Fits every integer type this engine will carry an identity on,
            // and the sequence starts at 1.
            ("integer", 0, 1, false),
            ("integer", -5, 1, false),
            // Counting down, the sequence runs `type_min ..= -1`.
            ("integer", -5, -1, true),
            ("integer", 5, -1, false),
            ("smallint", -32768, -1, true),
            ("smallint", -32769, -1, false),
            ("bigint", i64::MAX, 1, true),
            ("bigint", i64::MIN, -1, true),
        ] {
            let mut table = Table::default();
            let mut column = pbps_model::Column::new(ty(declared));
            column.nullable = false;
            column.value_source = pbps_model::ValueSource::Identity(pbps_model::Identity {
                seed,
                increment,
                options: Default::default(),
            });
            table.columns.insert("id".into(), column);
            let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &table);
            assert_eq!(
                found.is_empty(),
                accepted,
                "`{declared}` starting at {seed} by {increment}: {found:?}"
            );
        }
    }

    #[test]
    fn identity_increments_must_fit_the_directional_sequence_span() {
        for (declared, seed, increment, accepted) in [
            ("smallint", 1, 40000, false),
            ("smallint", 1, 32767, false),
            ("smallint", 1, 32766, true),
            ("smallint", -1, -40000, false),
            ("smallint", -1, -32768, false),
            ("smallint", -1, -32767, true),
            ("integer", 1, 2_147_483_647, false),
            ("integer", 1, 2_147_483_646, true),
            ("integer", -1, -2_147_483_648, false),
            ("integer", -1, -2_147_483_647, true),
            ("bigint", 1, i64::MAX, false),
            ("bigint", 1, i64::MAX - 1, true),
            ("bigint", -1, i64::MIN, false),
            ("bigint", -1, -i64::MAX, true),
            ("int2", 1, 40000, false),
            ("int4", -1, -2_147_483_647, true),
            ("int8", -1, i64::MIN, false),
            // This rule concerns the increment, not remaining values from a
            // particular seed; the existing seed-boundary contract remains.
            ("smallint", 32767, 1, true),
            ("bigint", i64::MIN, -1, true),
        ] {
            let mut table = Table::default();
            let mut column = pbps_model::Column::new(ty(declared));
            column.nullable = false;
            column.value_source = pbps_model::ValueSource::Identity(pbps_model::Identity {
                seed,
                increment,
                options: Default::default(),
            });
            table.columns.insert("id".into(), column);
            let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &table);
            assert_eq!(
                found.is_empty(),
                accepted,
                "{declared} increment {increment}: {found:?}"
            );
            if !accepted {
                assert_eq!(found.len(), 1, "{found:?}");
                let message = found[0].to_string();
                for expected in [
                    "sequence span".to_owned(),
                    increment.to_string(),
                    "id".to_owned(),
                ] {
                    assert!(message.contains(&expected), "{message}");
                }
                if declared == "smallint" && increment == 40000 {
                    assert!(message.contains("32767"), "{message}");
                }
            }
        }
    }

    /// The classification is asked of the normalized types even when the
    /// caller forgot, because one caller cannot remember: `validate_saved_plan`
    /// re-derives a plan file's risks precisely because the file may have been
    /// edited, and an editor writes whatever spelling it likes.
    ///
    /// Both directions are here. The alias that reads as `Incompatible` blocks
    /// a plan that changes nothing; the omitted argument that reads as `Safe`
    /// walks a narrowing past the gate.
    #[test]
    fn a_risk_is_judged_on_the_normalized_types_even_if_the_caller_forgot() {
        for (from, to, expected) in [
            // `character` is `character(1)`, not an unbounded string.
            ("character(5)", "character", TypeChangeRisk::Narrowing),
            ("char(5)", "char(10)", TypeChangeRisk::Safe),
            // Aliases, which name the same type and change nothing.
            ("int", "integer", TypeChangeRisk::Safe),
            ("integer", "int", TypeChangeRisk::Safe),
            ("numeric(10,2)", "decimal(10,2)", TypeChangeRisk::Safe),
            // And a real narrowing that the alias spelling used to hide.
            ("varchar(50)", "varchar(10)", TypeChangeRisk::Narrowing),
            ("int8", "int4", TypeChangeRisk::Narrowing),
            // A type the catalogue will not spell stays refused rather than
            // falling through to a family. Both halves matter: an unknown base
            // no family claims, and — the one that reads as safe if the
            // declared value travels on — a rejected modifier on a base that
            // every family does claim.
            ("serial", "integer", TypeChangeRisk::Incompatible),
            ("nonesuch", "integer", TypeChangeRisk::Incompatible),
            // Past this engine's own precision, in the direction that looks
            // like widening.
            (
                "numeric(1000)",
                "numeric(1001)",
                TypeChangeRisk::Incompatible,
            ),
            (
                "numeric(1001)",
                "numeric(1000)",
                TypeChangeRisk::Incompatible,
            ),
            // The precision the engine takes and silently stores as 6, which
            // is why the catalogue refuses to spell it at all.
            ("interval(6)", "interval(7)", TypeChangeRisk::Incompatible),
            // And a length past the engine's maximum for a string.
            (
                "varchar(10485760)",
                "varchar(10485761)",
                TypeChangeRisk::Incompatible,
            ),
        ] {
            assert_eq!(
                Postgres::new().type_change_risk(&ty(from), &ty(to)),
                expected,
                "`{from}` -> `{to}`"
            );
        }
    }

    /// A column the catalogue cannot spell is reported here too, where a user
    /// is looking at the declaration — `validate` is the command that exists
    /// to say so before anything connects.
    #[test]
    fn validating_a_table_reports_every_type_it_cannot_spell() {
        let mut table = Table::default();
        for (name, ty_) in [
            ("a", "widget"),
            ("b", "timestamp(7)"),
            ("c", "timestamp(3)"),
        ] {
            table
                .columns
                .insert(name.into(), pbps_model::Column::new(ty(ty_)));
        }
        let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &table);
        assert_eq!(found.len(), 2, "{found:?}");
    }

    fn ty(s: &str) -> ColumnType {
        s.parse().expect("a type parses")
    }
    #[test]
    fn every_cross_kind_constraint_name_collision_is_reported() {
        use pbps_model::{
            CheckConstraint, Column, ForeignKey, Index, IndexColumn, PrimaryKey, ReferentialAction,
            UniqueConstraint,
        };
        let kinds = [
            "primary key",
            "unique constraint",
            "foreign key",
            "check constraint",
        ];
        for first in 0..kinds.len() {
            for second in first + 1..kinds.len() {
                let mut table = Table::default();
                table
                    .columns
                    .insert("id".into(), Column::new("int".parse().unwrap()).not_null());
                for (kind, name) in [(first, "same"), (second, "same")] {
                    match kind {
                        0 => {
                            table.primary_key = Some(PrimaryKey {
                                name: Some(name.into()),
                                columns: vec!["id".into()],
                                storage_parameters: Default::default(),
                            })
                        }
                        1 => {
                            table.unique.insert(
                                name.into(),
                                UniqueConstraint {
                                    columns: vec!["id".into()],
                                    storage_parameters: Default::default(),
                                },
                            );
                        }
                        2 => {
                            table.foreign_keys.insert(
                                name.into(),
                                ForeignKey {
                                    columns: vec!["id".into()],
                                    references_table: "app.parent".parse().unwrap(),
                                    references_columns: vec!["id".into()],
                                    on_delete: ReferentialAction::NoAction,
                                    on_update: ReferentialAction::NoAction,
                                },
                            );
                        }
                        3 => {
                            table.checks.insert(
                                name.into(),
                                CheckConstraint {
                                    expression: "id > 0".into(),
                                },
                            );
                        }
                        _ => unreachable!(),
                    }
                }
                let errors: Vec<String> = Postgres::new()
                    .validate_table(&"app.t".parse().unwrap(), &table)
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                assert_eq!(errors.len(), 1, "{errors:?}");
                for expected in [kinds[first], kinds[second], "`same`"] {
                    assert!(errors[0].contains(expected), "{errors:?}");
                }
            }
        }
        let mut table = Table::default();
        table
            .columns
            .insert("id".into(), Column::new("int".parse().unwrap()).not_null());
        table.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        table.checks.insert(
            "same".into(),
            CheckConstraint {
                expression: "id > 0".into(),
            },
        );
        table.indexes.insert(
            "same".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("id".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        assert!(
            Postgres::new()
                .validate_table(&"app.t".parse().unwrap(), &table)
                .is_empty(),
            "a check and an ordinary index may share a name"
        );
    }
}

#[cfg(test)]
mod certified_emission_tests {
    // Existing emitter fixtures compare their frozen raw result with the real
    // singleton route, including complete Statement metadata and wire bytes.
    pub(crate) fn compare_fixture(
        dialect: &Postgres,
        change: &Change,
        strategy: Strategy,
        expected: Result<Vec<Statement>, DialectError>,
    ) -> Result<Vec<Statement>, DialectError> {
        let mut planned = pbps_model::PlannedChange::new(change.clone());
        planned.strategy = strategy;
        let before = pbps_model::ChangeSet {
            changes: vec![planned.clone()],
        };
        let ordered = pbps_diff::order::singleton(
            planned.clone(),
            pbps_model::PlanMode::Transactional,
            dialect,
        );
        let actual = dialect.emit(&ordered, 0);
        for mode in [
            pbps_model::PlanMode::Transactional,
            pbps_model::PlanMode::Staged,
        ] {
            let ordered = pbps_diff::order::singleton(planned.clone(), mode, dialect);
            let lowered = pbps_dialect::lower::lower_all(
                &ordered as &dyn pbps_diff::order::PlanView<Postgres>,
                dialect,
            );
            let reference = match mode {
                pbps_model::PlanMode::Transactional => {
                    expected.as_ref().cloned().map_err(ToString::to_string)
                }
                pbps_model::PlanMode::Staged => emit::staged_attach(change)
                    .unwrap_or_else(|| emit::emit(dialect, change, strategy))
                    .map_err(|e| e.to_string()),
            };
            assert_eq!(
                lowered
                    .as_ref()
                    .map(|p| p.statements())
                    .map_err(ToString::to_string),
                reference
                    .as_ref()
                    .map(Vec::as_slice)
                    .map_err(ToString::to_string)
            );
            if let Ok(lowered) = lowered {
                assert_eq!(lowered.groups().len(), 1);
                assert_eq!(
                    (
                        lowered.groups()[0].operation().source(),
                        lowered.groups()[0].operation().part()
                    ),
                    (0, 0)
                );
                assert_eq!(
                    pbps_dialect::render_script(&lowered, dialect, &pbps_model::SessionZone::utc()),
                    script_oracle::render(
                        reference.as_ref().unwrap(),
                        dialect,
                        &pbps_model::SessionZone::utc()
                    )
                );
            }
        }
        assert_eq!(
            actual.as_ref().map_err(ToString::to_string),
            expected.as_ref().map_err(ToString::to_string)
        );
        let wire = |changes| {
            pbps_model::SavedPlan::new(
                pbps_model::PlanOrigin::Preview,
                dialect.name(),
                "2026-10-19T00:00:00Z",
                pbps_model::PlanBaseline {
                    description: "ordering byte oracle".into(),
                    checksum: "baseline".into(),
                    database_collation: None,
                    time_zone: None,
                },
                changes,
                pbps_model::IdsFile::default(),
            )
        };
        let before = wire(before);
        let after =
            pbps_diff::order::seal(ordered.clone(), wire(ordered.changes().clone())).unwrap();
        let bytes = |wire: &pbps_model::SavedPlan| {
            let mut bytes = serde_json::to_vec_pretty(wire).unwrap();
            bytes.push(b'\n');
            bytes
        };
        assert_eq!(bytes(&before), bytes(after.wire()));
        assert_eq!(
            before.wire_fingerprint(),
            pbps_diff::order::ArtifactView::checksum(&after)
        );
        if let (Ok(actual), Ok(expected)) = (&actual, &expected) {
            assert_eq!(
                script_oracle::render(actual, dialect, &pbps_model::SessionZone::utc()),
                script_oracle::render(expected, dialect, &pbps_model::SessionZone::utc())
            );
        }
        actual
    }

    use super::*;
    use pbps_model::{Change, ChangeSet, PlanMode, PlannedChange};

    #[test]
    fn certified_lowering_preserves_statement_bytes_and_mode() {
        let dialect = Postgres::new();
        let changes = ChangeSet {
            changes: vec![PlannedChange::new(Change::RenameTable {
                uid: "t_aaaaaa".parse().unwrap(),
                from: "dbo.old_name".parse().unwrap(),
                to: "dbo.new_name".parse().unwrap(),
                defaults: Vec::new(),
            })],
        };
        let expected = dialect
            .fixture_emit(&changes.changes[0].change, changes.changes[0].strategy)
            .unwrap();
        for mode in [PlanMode::Transactional, PlanMode::Staged] {
            let ordered = pbps_diff::order::singleton(changes.changes[0].clone(), mode, &dialect);
            let emitted = pbps_dialect::lower::lower_all(
                &ordered as &dyn pbps_diff::order::PlanView<Postgres>,
                &dialect,
            )
            .unwrap();
            assert_eq!(emitted.statements(), expected);
        }
    }

    #[test]
    fn partition_lowering_uses_each_certified_inputs_bound_mode() {
        use pbps_diff::order::{ArtifactView, PlanView, admit, seal, singleton};
        use pbps_model::{
            BoundDatum, IdsFile, PartitionBound, PlanBaseline, PlanOrigin, SavedPlan,
        };

        let dialect = Postgres::new();
        let change = PlannedChange::new(Change::AttachPartition {
            uid: "t_aaaaaa".parse().unwrap(),
            table: "app.child".parse().unwrap(),
            parent: "app.parent".parse().unwrap(),
            bound: PartitionBound::Range {
                from: vec![BoundDatum::Value("1".into())],
                to: vec![BoundDatum::Value("5".into())],
            },
            key: vec!["id".into()],
            shape: Box::default(),
        });
        let transactional = emit::emit(&dialect, &change.change, change.strategy).unwrap();
        let staged = emit::staged_attach(&change.change).unwrap().unwrap();
        assert_eq!(transactional.len(), 1);
        assert_eq!(staged.len(), 4);
        assert_ne!(transactional, staged);

        for mode in [PlanMode::Transactional, PlanMode::Staged] {
            let ordered = singleton(change.clone(), mode, &dialect);
            let mut wire = SavedPlan::new(
                PlanOrigin::Database,
                dialect.name(),
                "2026-10-22T00:00:00Z",
                PlanBaseline {
                    description: "target".into(),
                    checksum: "baseline".into(),
                    database_collation: None,
                    time_zone: None,
                },
                ordered.changes().clone(),
                IdsFile::default(),
            );
            wire.mode = mode;
            let bytes = serde_json::to_string(&wire).unwrap();
            let checksum = wire.wire_fingerprint();
            let admitted = admit(
                serde_json::from_str(&bytes).unwrap(),
                &dialect,
                std::path::Path::new("plan.json"),
                &checksum,
            )
            .unwrap();
            let sealed = seal(ordered.clone(), wire).unwrap();
            assert_eq!(serde_json::to_string(admitted.wire()).unwrap(), bytes);
            assert_eq!(serde_json::to_string(sealed.wire()).unwrap(), bytes);
            assert_eq!(admitted.checksum(), checksum);
            assert_eq!(sealed.checksum(), checksum);
            let expected = match mode {
                PlanMode::Transactional => &transactional,
                PlanMode::Staged => &staged,
            };
            assert_eq!(
                dialect
                    .statement_count(&change.change, change.strategy, mode)
                    .unwrap(),
                expected.len()
            );
            for input in [&ordered as &dyn PlanView<Postgres>, &sealed, &admitted] {
                assert_eq!(input.mode(), mode);
                let lowered = pbps_dialect::lower::lower_all(input, &dialect).unwrap();
                assert_eq!(lowered.statements(), expected);
            }
        }
    }
}

// Frozen raw observations exist only in fixture builds; real emission uses
// the private emitter and the engine-associated certified input.
#[cfg(test)]
impl Postgres {
    pub(crate) fn fixture_emit_staged(
        &self,
        change: &Change,
        strategy: Strategy,
    ) -> Result<Vec<Statement>, DialectError> {
        let mut part = pbps_model::PlannedChange::new(change.clone());
        part.strategy = strategy;
        let expected =
            emit::staged_attach(change).unwrap_or_else(|| emit::emit(self, change, strategy));
        let actual = self.emit_staged(
            &pbps_diff::order::singleton(part, pbps_model::PlanMode::Staged, self),
            0,
        );
        assert_eq!(
            actual.as_ref().map_err(ToString::to_string),
            expected.as_ref().map_err(ToString::to_string)
        );
        actual
    }
    pub(crate) fn fixture_emit(
        &self,
        change: &Change,
        strategy: Strategy,
    ) -> Result<Vec<Statement>, DialectError> {
        certified_emission_tests::compare_fixture(
            self,
            change,
            strategy,
            emit::emit(self, change, strategy),
        )
    }
}

#[cfg(test)]
mod script_oracle {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pbps-dialect/tests/support/script_oracle.rs"
    ));
}

#[cfg(test)]
mod enum_surface_refusal_tests {
    use super::*;
    #[test]
    fn enum_data_and_list_keys_remain_named_refusals_while_defaults_stay_admitted() {
        for ty in ["app.e", "app.e[]", "app.d"] {
            let yaml = format!(
                "table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n  m: {{type: '{ty}'}}\nprimary_key: [id]\ndata: {{mode: ensure, rows: {{}}}}\n"
            );
            let loaded = pbps_load::load_table_str(std::path::Path::new("t.yml"), &yaml).unwrap();
            let mut table = loaded.table;
            let pg = Postgres::new();
            let refused = pg
                .validate_table(&loaded.name, &table)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                refused.contains(ty)
                    && refused.contains("column `m`")
                    && refused.contains("user-defined type reference"),
                "{refused}"
            );
            table.data = None;
            table.primary_key = None;
            table.partition_by = Some(pbps_model::PartitionBy {
                strategy: pbps_model::PartitionStrategy::List,
                columns: vec!["m".into()],
                external: false,
            });
            let refused = pg
                .validate_table(&loaded.name, &table)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                refused.contains("partition key")
                    && refused.contains("partition key `m`")
                    && refused.contains("user-defined type reference"),
                "{refused}"
            );
            table.partition_by = None;
            table.columns["m"].value_source = pbps_model::ValueSource::Default("'old'".into());
            assert!(pg.validate_table(&loaded.name, &table).is_empty());
        }
    }
}

#[cfg(test)]
mod enum_partition_key_tests {
    use super::*;
    use pbps_model::{
        BoundDatum, ColumnName, ColumnType, ListDatum, ManagedType, PartitionBound, PartitionOf,
        PartitionStrategy, Schema, Uid,
    };

    fn declared() -> BTreeMap<pbps_model::TypeRef, ManagedType> {
        let domain = |base: &str| ManagedType::Domain {
            base: base.parse().unwrap(),
            not_null: false,
            default: None,
            checks: Default::default(),
            collation: None,
        };
        [
            (
                "app.mood".parse().unwrap(),
                ManagedType::Enum {
                    values: vec!["sad".into(), "ok".into(), "happy".into()],
                },
            ),
            ("app.dmood".parse().unwrap(), domain("app.mood")),
            ("app.dtxt".parse().unwrap(), domain("text")),
            ("app.dint".parse().unwrap(), domain("integer")),
        ]
        .into()
    }

    fn keyed(ty: &str, strategy: PartitionStrategy) -> Table {
        let mut table = Table::default();
        for (name, ty) in [("id", "integer"), ("m", ty)] {
            let mut column = pbps_model::Column::new(ty.parse().unwrap());
            column.nullable = false;
            table.columns.insert(name.into(), column);
        }
        table.partition_by = Some(pbps_model::PartitionBy {
            strategy,
            columns: vec!["m".into()],
            external: false,
        });
        table
    }

    fn problems(table: &Table, schema: &Schema) -> Vec<String> {
        Postgres::new()
            .validate_table_in_schema(&"app.t".parse().unwrap(), table, Default::default(), schema)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// DEC-2346.1: a scalar reference to a declared enum keys a LIST or RANGE
    /// table, and every other reference keeps DEC-1523.1's refusal and text.
    #[test]
    fn only_a_scalar_declared_enum_keys_a_list_or_range_table() {
        let schema = Schema {
            types: declared(),
            ..Default::default()
        };
        for strategy in [PartitionStrategy::List, PartitionStrategy::Range] {
            for (ty, admitted) in [
                ("app.mood", true),
                ("app.mood[]", false),
                ("app.dmood", false),
                ("app.dmood[]", false),
                ("app.dtxt", false),
                // Not declared here: an external enum, a composite.
                ("ext.mood", false),
                ("app.pair", false),
            ] {
                let table = keyed(ty, strategy);
                let found = problems(&table, &schema);
                if admitted {
                    assert!(found.is_empty(), "{ty}: {found:?}");
                } else {
                    assert_eq!(found.len(), 1, "{ty}: {found:?}");
                    assert!(
                        found[0].contains(
                            "partition key `m` has a user-defined type reference, which is not \
                             supported as a partition key"
                        ),
                        "{ty}: {found:?}"
                    );
                }
                // A table seen alone, or beside no declaration, admits nothing.
                let lone = Postgres::new().validate_table(&"app.t".parse().unwrap(), &table);
                assert!(!lone.is_empty(), "{ty}: no schema, no admission");
                assert_eq!(
                    problems(&table, &Schema::default()).len(),
                    1,
                    "{ty}: the declaration is on the other side"
                );
            }
        }
        // The engine hashes an enum by its oid, which a rebuild changes.
        let hashed = keyed("app.mood", PartitionStrategy::Hash);
        assert_eq!(problems(&hashed, &schema).len(), 1);
    }

    fn child(bound: PartitionBound) -> Table {
        Table {
            partition_of: Some(PartitionOf {
                parent: "app.p".parse().unwrap(),
                bound,
                columns: Default::default(),
            }),
            ..Default::default()
        }
    }

    /// DEC-2346.1: a bound datum over a declared enum key is a declared label
    /// (or an unbounded end, or NULL); the refusal names the partition and the
    /// label, and the other key columns are not read as labels.
    #[test]
    fn a_bound_over_an_enum_key_names_declared_labels_only() {
        let value = |l: &str| ListDatum::Value(l.into());
        let at = |l: &str| BoundDatum::Value(l.into());
        let mut schema = Schema {
            types: declared(),
            ..Default::default()
        };
        schema.tables.insert(
            "app.p".parse().unwrap(),
            keyed("app.mood", PartitionStrategy::List),
        );
        schema.tables.insert(
            "app.r".parse().unwrap(),
            keyed("app.mood", PartitionStrategy::Range),
        );
        let check = |schema: &Schema, parent: &str, bound: PartitionBound| {
            let mut table = child(bound);
            table.partition_of.as_mut().unwrap().parent = parent.parse().unwrap();
            Postgres::new()
                .validate_table_in_schema(
                    &"app.child".parse().unwrap(),
                    &table,
                    Default::default(),
                    schema,
                )
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        let list = |values: &[ListDatum]| PartitionBound::List {
            values: values.iter().cloned().collect(),
        };
        assert!(
            check(
                &schema,
                "app.p",
                list(&[value("ok"), value("sad"), ListDatum::Null])
            )
            .is_empty()
        );
        assert!(check(&schema, "app.p", PartitionBound::Default).is_empty());
        let found = check(&schema, "app.p", list(&[value("ok"), value("nonesuch")]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("partition `app.child`")
                && found[0].contains("`nonesuch`")
                && found[0].contains("`app.mood`")
                && found[0].contains("`sad`, `ok`, `happy`"),
            "{found:?}"
        );
        // A label is exact: another case is another label.
        assert_eq!(check(&schema, "app.p", list(&[value("OK")])).len(), 1);
        let range = |from: BoundDatum, to: BoundDatum| PartitionBound::Range {
            from: vec![from],
            to: vec![to],
        };
        assert!(check(&schema, "app.r", range(BoundDatum::MinValue, at("ok"))).is_empty());
        assert!(check(&schema, "app.r", range(at("sad"), BoundDatum::MaxValue)).is_empty());
        assert_eq!(
            check(&schema, "app.r", range(at("sad"), at("zzz"))).len(),
            1
        );
        assert_eq!(
            check(&schema, "app.r", range(at("yyy"), at("zzz"))).len(),
            2
        );
        // Only the enum key column's datum is read as a label.
        schema
            .tables
            .get_mut(&"app.r".parse().unwrap())
            .unwrap()
            .partition_by = Some(pbps_model::PartitionBy {
            strategy: PartitionStrategy::Range,
            columns: vec!["id".into(), "m".into()],
            external: false,
        });
        let two = |id: &str, label: &str| PartitionBound::Range {
            from: vec![at(id), at("sad")],
            to: vec![at(id), at(label)],
        };
        assert!(check(&schema, "app.r", two("not a label", "ok")).is_empty());
        assert_eq!(check(&schema, "app.r", two("1", "zzz")).len(), 1);
    }

    /// DEC-2346.1 (#2112): the staged attach's CHECK spells its bound with bare
    /// operators and untyped literals, which resolve to the key type's own for
    /// a built-in type and for an enum, and to none for a domain.
    #[test]
    fn an_attach_carries_its_key_only_over_built_in_types_and_declared_enums() {
        let uid: Uid = "t_aaaaaa".parse().unwrap();
        let pg = Postgres::new();
        let types = declared();
        let key =
            |ty: &str| -> Vec<(ColumnName, ColumnType)> { vec![("k".into(), ty.parse().unwrap())] };
        let carried = |ty: &str, types: &BTreeMap<_, _>| {
            pg.attach_scan_key(&uid, &key(ty), types) == vec![ColumnName::from("k")]
        };
        for (ty, carries) in [
            ("date", true),
            ("text[]", true),
            ("integer", true),
            ("app.mood", true),
            // A domain, over an enum or not: no operator for the first, and
            // the others are not measured (DEC-1523.1 refuses them anyway).
            ("app.dmood", false),
            ("app.dint", false),
            ("app.mood[]", false),
            ("public.citext", false),
        ] {
            assert_eq!(carried(ty, &types), carries, "{ty}");
        }
        // The same name, undeclared on this side, is some other type.
        assert!(!carried("app.mood", &BTreeMap::new()));
        // Every column of the key must be spelled, not the first.
        let mixed = vec![
            ("a".into(), "date".parse().unwrap()),
            ("b".into(), "app.dmood".parse().unwrap()),
        ];
        assert!(pg.attach_scan_key(&uid, &mixed, &types).is_empty());
        assert!(pg.attach_scan_key(&uid, &[], &types).is_empty());
        // DEC-2126.1's held name still empties the key for an enum.
        let held = Postgres::new().with_attach_constraint_names(
            [(uid.clone(), [emit::scan_check_name(&uid)].into())].into(),
        );
        assert!(
            held.attach_scan_key(&uid, &key("app.mood"), &types)
                .is_empty()
        );
    }
}

#[cfg(test)]
mod enum_rebuild_dependents_tests {
    use super::*;

    /// Measured on 16 and 18 (DEC-1635.7): constants typed as the old enum sit
    /// in checks, foreign keys and filtered or expression indexes, and the
    /// retype refuses them; plain keys and indexes are rebuilt by the engine.
    #[test]
    fn postgres_rebuilds_what_holds_constants_of_the_old_enum_and_leaves_keys_to_the_engine() {
        let pg = Postgres::new().enum_rebuild_dependents();
        assert!(pg.checks && pg.filtered_indexes && pg.foreign_keys);
        assert!(!pg.keys_and_indexes);
        // An engine without declared enums rebuilds nothing around them.
        let other = pbps_dialect::MinimalDialect.enum_rebuild_dependents();
        assert!(
            !(other.checks
                || other.filtered_indexes
                || other.foreign_keys
                || other.keys_and_indexes)
        );
    }
}
