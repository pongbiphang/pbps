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

use std::borrow::Cow;

use pbps_dialect::{Dialect, DialectError, Lexicon, Statement, TransactionFraming, TypeChangeRisk};
use pbps_model::{Change, ColumnType, Strategy, Table, TableName};

pub mod catalog;
mod emit;
pub mod introspect;
mod types;

/// A part of the dialect that Phase 5 has not built yet.
///
/// One place, so that "what is missing" is a list rather than a habit, and so
/// that every refusal names the step that supplies it. The message is written
/// for whoever runs the command, not for whoever writes the crate: it says what
/// pbps cannot do, and it does not pretend the answer is "nothing to do".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unbuilt {
    Introspection,
    Modules,
    Roles,
    ReferenceData,
    Ledger,
    Probes,
}

impl Unbuilt {
    const fn step(self) -> &'static str {
        match self {
            Unbuilt::Introspection => "reading a database back (Phase 5 step 3)",
            Unbuilt::Modules => "views, functions, procedures and triggers (Phase 5 step 5)",
            Unbuilt::Roles => "roles and grants (Phase 5 step 6)",
            Unbuilt::ReferenceData => "reference data (Phase 5 step 7)",
            Unbuilt::Ledger => "the state ledger (Phase 5 step 8)",
            Unbuilt::Probes => "preflight probes (Phase 5 step 9)",
        }
    }

    fn refuse(self) -> DialectError {
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
const MAX_IDENT_BYTES: usize = 63;

/// What this engine refuses about an `identity:`, each measured on 18.6.
///
/// `validate` is where a declaration should fail, and until the catalogue
/// existed this could not be asked: `validate_table` refused every table
/// outright, so nothing reached the question.
fn identity_problems(column: &str, declared: &pbps_model::Column) -> Vec<DialectError> {
    let Some(identity) = declared.identity else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let invalid = |message: String| DialectError::Invalid {
        dialect: "postgres",
        message,
    };
    match types::identity_seed_range(&declared.ty, identity.increment) {
        None => found.push(invalid(format!(
            "column `{column}` has an `identity:` on `{}`. This engine says it in so many words: \
             identity column type must be smallint, integer, or bigint. A `numeric` is refused \
             here even with a scale of zero, where SQL Server admits one.",
            declared.ty
        ))),
        // The seed is checked against the sequence's bounds, not the column's:
        // see [`types::identity_seed_range`] for why those are not the same
        // range, and for the two seeds that fit the type and are still refused.
        Some(accepted) if !accepted.contains(&identity.seed) => found.push(invalid(format!(
            "column `{column}` has an `identity:` with a seed of {}, and an `identity:` on `{}` \
             counting by {} starts somewhere in {}..={}: the engine refuses the table with START \
             value ({}) out of the sequence's own bounds.",
            identity.seed,
            declared.ty,
            identity.increment,
            accepted.start(),
            accepted.end(),
            identity.seed
        ))),
        Some(_) => {}
    }
    // `GENERATED ... AS IDENTITY` implies NOT NULL, and saying both is
    // `conflicting NULL/NOT NULL declarations`.
    if declared.nullable {
        found.push(invalid(format!(
            "column `{column}` has an `identity:`, so it cannot be nullable: the engine refuses \
             the pair as conflicting NULL/NOT NULL declarations."
        )));
    }
    // `INCREMENT must not be zero`, and it is worth saying why rather than
    // quoting: a step of zero hands every row the same value.
    if identity.increment == 0 {
        found.push(invalid(format!(
            "column `{column}` has an `identity:` with an increment of 0, which never advances: \
             the engine refuses it as INCREMENT must not be zero."
        )));
    }
    // `both default and identity specified for column`.
    if declared.default.is_some() {
        found.push(invalid(format!(
            "column `{column}` has an `identity:` and a `default:`. The engine refuses both on \
             one column, and the identity is the one that supplies the value."
        )));
    }
    found
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
/// honest somewhere is the project. They are held here rather than passed to
/// [`Dialect::emit`] because the trait takes a change and a strategy, and a
/// strategy says how to get there and never where (ADR-0003).
///
/// Empty is the value every caller has today: `pbps.yml` has no key for this
/// yet, because the CLI cannot select this dialect at all (`main.rs`'s
/// `dialect()` refuses it) and a configuration key nothing reads is worse than
/// none. The key arrives with the step that can read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Postgres {
    write_path_extras: Vec<String>,
}

impl Postgres {
    /// The dialect with nothing after an object's own schema on the write path.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The dialect with `extras` after an object's own schema, in that order.
    ///
    /// The order is part of what a declaration means, and ADR-0013 measured it:
    /// a view created under `(m_ea, m_eb)` keeps the binding it was created
    /// with, while a fresh one under `(m_eb, m_ea)` takes the other. So this
    /// takes a sequence and never a set.
    #[must_use]
    pub fn with_write_path_extras(extras: Vec<String>) -> Self {
        Self {
            write_path_extras: extras,
        }
    }

    /// The schemas after an object's own on the write path.
    #[must_use]
    pub fn write_path_extras(&self) -> &[String] {
        &self.write_path_extras
    }
}

/// Quotes one identifier, or refuses a string that cannot be one.
///
/// A free function because two callers need it and only one of them has a
/// `Postgres` to hand: [`Dialect::quote_ident`] is this, and the emitter is
/// this without having to build a dialect value to ask.
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

impl Dialect for Postgres {
    fn name(&self) -> &'static str {
        "postgres"
    }

    /// `"…"` alone quotes an identifier: `[` is an array subscript, which is
    /// code, and reading it as a quote made a reindent inside `a[1 + 2]` read
    /// as a changed module. Both extensions to the string literal are here —
    /// `E'…'`, where `\'` does not close the string, and `$tag$…$tag$`, which
    /// nothing inside it can close early (ADR-0011 Amendment 2).
    fn lexicon(&self) -> Lexicon {
        Lexicon {
            quoted_identifiers: &[('"', '"')],
            escape_strings: true,
            dollar_quoted_strings: true,
        }
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
    /// the earlier batch every connection runs — the same place SQL Server's
    /// `SET XACT_ABORT ON` lives, for the same structural reason.
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
    /// Neither is spelled `SET LOCAL`: a rendered script is run statement by
    /// statement outside any transaction, where `SET LOCAL` is a warning and a
    /// no-op — and a pin that quietly does nothing is the failure it exists to
    /// prevent. Inside the transaction this opens they are undone by the
    /// rollback, and outside one they are on the connection pbps opened for
    /// this deployment.
    fn transaction_framing(&self) -> TransactionFraming {
        TransactionFraming {
            begin: "SET standard_conforming_strings = on; SET check_function_bodies = on; BEGIN;",
            commit: "COMMIT;",
            // Tolerates a transaction the server has already killed, so that
            // this statement's own error cannot replace the real failure.
            rollback: "ROLLBACK;",
        }
    }

    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        types::normalize(ty)
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
        let mut found = Vec::new();
        // The names first, and this is not a formality: `quote_ident` refuses
        // an identifier over [`MAX_IDENT_BYTES`], so a table this method called
        // clean is one the emitter cannot spell. `validate` is the command that
        // exists to say so before anything connects.
        for part in [&name.schema, &name.name] {
            if let Err(e) = self.quote_ident(part) {
                found.push(e);
            }
        }
        // Every other name the table owns, which the engine truncates at the
        // same limit and which the emitter has to spell just as often: the
        // primary key's, and the keys of the four maps.
        let owned = table
            .primary_key
            .as_ref()
            .and_then(|pk| pk.name.as_deref())
            .into_iter()
            .chain(table.unique.keys().map(String::as_str))
            .chain(table.foreign_keys.keys().map(String::as_str))
            .chain(table.checks.keys().map(String::as_str))
            .chain(table.indexes.keys().map(String::as_str));
        for object in owned {
            if let Err(e) = self.quote_ident(object) {
                found.push(e);
            }
        }

        for (column_name, column) in &table.columns {
            if let Err(e) = self.quote_ident(column_name) {
                found.push(e);
            }
            if let Some(named) = types::refuse_serial(&column.ty, Some(column_name)) {
                found.push(named);
                // Everything below asks a question about the type, and asking
                // it of one the catalogue has already refused is noise on top
                // of the real error.
                continue;
            }
            if let Err(e) = types::normalize(&column.ty) {
                found.push(e);
                continue;
            }
            found.extend(identity_problems(column_name, column));
        }
        found
    }

    fn emit(&self, change: &Change, strategy: Strategy) -> Result<Vec<Statement>, DialectError> {
        emit::emit(self, change, strategy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refusal is output like any other, so what it says is tested: each
    /// names the step that supplies it, and none of them reads as "there is
    /// nothing to do".
    #[test]
    fn an_unbuilt_part_refuses_by_name_and_never_reads_as_nothing_to_do() {
        for part in [
            Unbuilt::Introspection,
            Unbuilt::Modules,
            Unbuilt::Roles,
            Unbuilt::ReferenceData,
            Unbuilt::Ledger,
            Unbuilt::Probes,
        ] {
            let message = part.refuse().to_string();
            assert!(message.contains("Phase 5 step"), "{message}");
            // "does not implement ... yet", never "does not support": the
            // second would say the engine lacks the feature.
            assert!(message.contains("does not implement"), "{message}");
            assert!(message.ends_with("yet"), "{message}");
        }
    }

    /// `emit` returning an empty statement list would be a plan that applies
    /// cleanly and changes nothing — the silent wrong answer. A part this
    /// crate has not built has to be an error, and the type is what makes that
    /// so. The structural half is built (step 4); the three that are not each
    /// name their own step.
    #[test]
    fn a_change_from_an_unbuilt_part_is_an_error_and_not_an_empty_plan() {
        let unbuilt = [
            (
                Change::DropModule {
                    id: "app.v".parse().expect("a module id parses"),
                    kind: pbps_model::ModuleKind::View,
                },
                "Phase 5 step 5",
            ),
            (
                Change::CreateRole {
                    uid: pbps_model::Uid::generate(pbps_model::UidKind::Role),
                    name: "analyst".to_owned(),
                },
                "Phase 5 step 6",
            ),
            (
                Change::DeleteRow {
                    table: "app.t".parse().expect("a table name parses"),
                    key_column: "code".to_owned(),
                    key: pbps_model::RowKey::from("a"),
                    cause: pbps_model::change::DeleteCause::Undeclared,
                    row: std::collections::BTreeMap::new(),
                    types: std::collections::BTreeMap::new(),
                    after_types: std::collections::BTreeMap::new(),
                },
                "Phase 5 step 7",
            ),
        ];
        for (change, step) in unbuilt {
            let refusal = Postgres::new()
                .emit(&change, Strategy::default())
                .expect_err("this part is not built");
            assert!(refusal.to_string().contains(step), "{refusal}");
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
            .insert("id".to_owned(), pbps_model::Column::new(ty("serial")));
        table
            .columns
            .insert("note".to_owned(), pbps_model::Column::new(ty("text")));
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
            .insert(long.clone(), pbps_model::Column::new(ty("integer")));
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
            .insert("id".to_owned(), pbps_model::Column::new(ty("integer")));
        assert!(
            Postgres::new()
                .validate_table(&"app.t".parse().unwrap(), &ok)
                .is_empty()
        );
    }

    /// The names a table owns are not only its own and its columns'. Each of
    /// these is emitted as an identifier and truncated by the server at the
    /// same limit, so `validate` has to ask about all of them or a plan fails
    /// on a name it printed itself.
    #[test]
    fn validating_a_table_refuses_every_owned_name_the_emitter_could_not_spell() {
        let long = "a".repeat(MAX_IDENT_BYTES + 1);
        let mut table = Table::default();
        table
            .columns
            .insert("id".to_owned(), pbps_model::Column::new(ty("integer")));
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: Some(long.clone()),
            columns: vec!["id".to_owned()],
        });
        table.unique.insert(
            long.clone(),
            pbps_model::UniqueConstraint {
                columns: vec!["id".to_owned()],
            },
        );
        table.checks.insert(
            long.clone(),
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
            column.identity = Some(pbps_model::Identity {
                seed: 1,
                increment: 1,
            });
            table.columns.insert("id".to_owned(), column);
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
    /// `conflicting NULL/NOT NULL declarations`, `both default and identity
    /// specified for column`, and `INCREMENT must not be zero`. Each is a rule
    /// the shipped SQL Server dialect carries too; the one that does *not*
    /// cross over is "only one IDENTITY per table", which this engine allows.
    #[test]
    fn an_identity_that_is_nullable_or_defaulted_or_never_advances_is_refused() {
        let mut table = Table::default();
        let mut column = pbps_model::Column::new(ty("integer"));
        column.nullable = true;
        column.default = Some("7".to_owned());
        column.identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 0,
        });
        table.columns.insert("id".to_owned(), column.clone());
        // A second identity column, which this engine takes: it must not add a
        // refusal of its own, only the three this one already earns.
        table.columns.insert("other".to_owned(), {
            let mut c = pbps_model::Column::new(ty("bigint"));
            c.nullable = false;
            c.identity = Some(pbps_model::Identity {
                seed: 1,
                increment: 1,
            });
            c
        });
        let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &table);
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found.iter().any(|e| e.to_string().contains("nullable")));
        assert!(found.iter().any(|e| e.to_string().contains("`default:`")));
        assert!(
            found
                .iter()
                .any(|e| e.to_string().contains("never advances"))
        );
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
            column.identity = Some(pbps_model::Identity { seed, increment });
            table.columns.insert("id".to_owned(), column);
            let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &table);
            assert_eq!(
                found.is_empty(),
                accepted,
                "`{declared}` starting at {seed} by {increment}: {found:?}"
            );
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
        for (name, ty_) in [("a", "widget"), ("b", "timestamp(3)"), ("c", "integer")] {
            table
                .columns
                .insert(name.to_owned(), pbps_model::Column::new(ty(ty_)));
        }
        let found = Postgres::new().validate_table(&"app.t".parse().unwrap(), &table);
        assert_eq!(found.len(), 2, "{found:?}");
    }

    fn ty(s: &str) -> ColumnType {
        s.parse().expect("a type parses")
    }
}
