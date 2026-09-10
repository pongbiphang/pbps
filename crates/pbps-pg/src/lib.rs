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

use std::borrow::Cow;

use pbps_dialect::{Dialect, DialectError, Lexicon, Statement, TransactionFraming, TypeChangeRisk};
use pbps_model::{
    Change, ChangeSet, ColumnType, Module, ModuleId, ModuleKind, RoutineArg, Schema, Strategy,
    Table, TableName,
};

pub mod catalog;
pub mod doctor;
mod emit;

/// This engine's lexis, for the definition scanners (ADR-0011 Amendment 2):
/// `"` quotes an identifier and `[` does not, `E'…'` is an escape string and
/// `$tag$…$tag$` a literal closed only by its own tag.
pub(crate) const LEXICON: Lexicon = Lexicon {
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
pub mod introspect;
pub mod modules;
mod preflight;
pub mod rows;
pub mod state;
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
    Roles,
    Probes,
}

impl Unbuilt {
    const fn step(self) -> &'static str {
        match self {
            Unbuilt::Introspection => "reading a database back (Phase 5 step 3)",
            Unbuilt::Roles => "roles and grants (Phase 5 step 6)",
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
pub(crate) const MAX_IDENT_BYTES: usize = 63;

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

/// The settings every write of this dialect runs under, as one statement.
///
/// A macro rather than a constant because both users need it as a **literal**:
/// [`Dialect::transaction_framing`] builds `begin` by `concat!`ing it with
/// `BEGIN;`, and `concat!` takes literals and not constants. One text, two
/// call sites, and no way for the transactional and staged paths to pin
/// different things — which is the failure this replaces, since the staged path
/// pinned nothing at all.
macro_rules! session_pins {
    () => {
        "SET standard_conforming_strings = on; SET check_function_bodies = on; \
         SET DateStyle = 'ISO, MDY'; SET TimeZone = 'UTC'; \
         SET IntervalStyle = 'postgres'; \
         SET timezone_abbreviations = 'Default'; \
         SET transform_null_equals = off; \
         SET bytea_output = 'hex'; SET extra_float_digits = 1;"
    };
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
    fn transaction_framing(&self) -> TransactionFraming {
        TransactionFraming {
            begin: concat!(session_pins!(), " BEGIN;"),
            commit: "COMMIT;",
            // Tolerates a transaction the server has already killed, so that
            // this statement's own error cannot replace the real failure.
            rollback: "ROLLBACK;",
        }
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
    fn session_pins(&self) -> Option<&'static str> {
        Some(session_pins!())
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
                     reads as the current role's own schema wherever a `search_path` names it — \
                     quoting does not make it literal. The table would be created and then \
                     every unqualified name in its checks, filters and defaults would resolve \
                     through whatever schema the deploying role owns. Declare it under a name \
                     the path can carry."
                ),
            });
        }
        if schema == "information_schema" || schema.starts_with("pg_") {
            found.push(DialectError::Invalid {
                dialect: types::DIALECT,
                message: format!(
                    "table `{name}` is declared in `{schema}`, which this dialect's pull \
                     never reads: `pg_catalog`, `information_schema` and every schema whose \
                     name begins with `pg_` are excluded from the managed set. The engine \
                     would create the table and no plan could ever see it again — and \
                     `pg_temp` is worse than invisible, because it is this engine's alias for \
                     the session's temporary schema: measured, `CREATE TABLE \"pg_temp\".\"t\"` \
                     leaves a `pg_temp_58.t` that disappears with the connection. Declare the \
                     table in a schema of the project's own."
                ),
            });
        }
        // The two tables this tool owns, refused for the same reason and from
        // the same list the reader hides them by (`catalog::OURS`). A
        // declaration naming one is created and then invisible: the pull
        // reports it absent and the next plan creates it again, which the
        // engine refuses for already existing.
        //
        // By name and in every schema, because that is how the filter reads —
        // `catalog.rs` narrowed it from a prefix on purpose, so that a
        // project's own `app.__pbps_customers` stays a project's table
        // (DECISIONS 274).
        if catalog::OURS.contains(&name.name.as_str()) {
            found.push(DialectError::Invalid {
                dialect: types::DIALECT,
                message: format!(
                    "table `{name}` uses the name `{}`, which is one of the two this tool owns \
                     (SPEC §8.1) and which this dialect's pull hides in every schema. The engine \
                     would create the table and no plan could ever see it again. Only these two \
                     names are taken: a table of your own called `{}_customers`, or anything \
                     else beginning with the same letters, is read back normally.",
                    name.name, name.name
                ),
            });
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
                             be NOT NULL. This engine does not refuse the table — it sets \
                             `NOT NULL` for you — and then no plan can ever make the column match \
                             the declaration again."
                        ),
                    });
                }
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
            let normalized = match types::normalize(&column.ty) {
                Ok(ty) => ty,
                Err(e) => {
                    found.push(e);
                    continue;
                }
            };
            // A default whose value the applying session would decide
            // (ADR-0013 §3). Asked here and not only in the emitter, because
            // `AlterColumnDefault` carries no type and the emitter therefore
            // cannot ask it on the one path that changes a default on a column
            // that is already there. This is where a user is looking at the
            // declaration, and every command that hands statements to a
            // database runs these checks (DECISIONS 141).
            if let Some(expr) = &column.default
                && let Some(e) = emit::refuse_an_unresolved_default(column_name, &normalized, expr)
            {
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
            if let Some(expr) = &column.default {
                let (core, cast) = rows::unwrapped_with_type(expr);
                let erased = core.eq_ignore_ascii_case("null")
                    && match cast {
                        None => true,
                        Some(ty) => {
                            column.ty.args.is_empty()
                                && types::as_the_grammar_spells(&ty)
                                    .and_then(|t| t.parse::<ColumnType>().ok())
                                    .and_then(|t| types::normalize(&t).ok())
                                    .is_some_and(|t| t == normalized)
                        }
                    };
                if erased {
                    found.push(DialectError::Invalid {
                        dialect: types::DIALECT,
                        message: format!(
                            "column `{column_name}` declares `default: {expr}`, which this engine \
                             does not keep: a NULL of the column's own type, bare, cast or in \
                             parentheses, leaves no default in the catalog at all, and the \
                             column reads back with none. Every plan would set it again and the \
                             check of what the apply left behind would refuse the column. A \
                             column with no default already defaults to NULL here: declare no \
                             default."
                        ),
                    });
                }
            }
            found.extend(identity_problems(column_name, column));
        }
        // Reference data: the rules whose answer is this engine's, ADR-0013 §2
        // among them. The model's own rules — a row's key is its identity in
        // every dialect — are `pbps_model::data::check`'s.
        found.extend(rows::data_problems(name, table));
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

    /// The write path: the object's own schema first, then the configured
    /// extras in order, which is the `search_path` every statement of this
    /// dialect runs under (DECISIONS 276). A bare name resolves through it
    /// and nowhere else, and to the *first* entry that holds one: measured,
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
    fn validate_module(&self, id: &ModuleId, module: &Module) -> Vec<DialectError> {
        emit::validate_module(id, module)
    }

    fn emit(&self, change: &Change, strategy: Strategy) -> Result<Vec<Statement>, DialectError> {
        emit::emit(self, change, strategy)
    }

    /// The reference-data probes, and no others yet: the rest of this
    /// dialect's preflight arrives with Phase 5 step 9. A probe list is not a
    /// promise that everything was checked — [`Unbuilt::Probes`] is what says
    /// the step is missing, and it is what `emit` still refuses for the
    /// changes that need those probes.
    fn preflight(&self, changes: &ChangeSet) -> Vec<pbps_dialect::Probe> {
        preflight::probes(changes)
    }

    /// Whether an omitted cell in the read-back *means* at-default for this
    /// column — the same question the row reader asks when it builds the
    /// query, asked once so the two cannot drift (DECISIONS 191).
    fn reads_back_at_default(&self, column: &pbps_model::Column) -> bool {
        rows::confirms_default(column)
    }

    fn declaration_notes(&self, schema: &Schema) -> Vec<String> {
        rows::not_checked_offline(schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A refusal is output like any other, so what it says is tested: each
    /// names the step that supplies it, and none of them reads as "there is
    /// nothing to do".
    #[test]
    fn an_unbuilt_part_refuses_by_name_and_never_reads_as_nothing_to_do() {
        for part in [Unbuilt::Introspection, Unbuilt::Roles, Unbuilt::Probes] {
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
        let unbuilt = [(
            Change::CreateRole {
                uid: pbps_model::Uid::generate(pbps_model::UidKind::Role),
                name: "analyst".to_owned(),
            },
            "Phase 5 step 6",
        )];
        for (change, step) in unbuilt {
            let refusal = Postgres::new()
                .emit(&change, Strategy::default())
                .expect_err("this part is not built");
            assert!(refusal.to_string().contains(step), "{refusal}");
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
            ("value", "text", "'x'"),
            ("expression", "text", "NULLIF('a', 'a')"),
        ];
        for (name, ty, default) in erased.iter().chain(&kept) {
            let mut c = pbps_model::Column::new(ty.parse().expect("a type"));
            c.default = Some((*default).into());
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
        d.default = Some("'01/02/2026'".into());
        table.columns.insert("d".into(), d);
        // The same text on a type whose input function reads no setting is not
        // a problem, and saying it were would refuse a valid declaration.
        let mut label = pbps_model::Column::new("text".parse().expect("a type"));
        label.default = Some("'01/02/2026'".into());
        table.columns.insert("label".into(), label);
        // Nor is the resolved spelling, which is what the pull reads back.
        let mut ok = pbps_model::Column::new("date".parse().expect("a type"));
        ok.default = Some("'2026-01-02'::date".into());
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

    /// A table named as one of the two this tool owns is refused, in any
    /// schema, because the reader hides it in any schema.
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
        for name in ["app.__pbps_state", "app.__pbps_lock", "public.__pbps_state"] {
            let problems = one(name);
            assert_eq!(problems.len(), 1, "`{name}`: {problems:?}");
            assert!(
                problems[0].to_string().contains("this tool owns"),
                "`{name}`: {}",
                problems[0]
            );
        }
        for name in [
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
        table.columns.insert(
            "id".to_owned(),
            // `not_null` because a nullable key column is its own finding, and
            // this test is about names.
            pbps_model::Column::new(ty("integer")).not_null(),
        );
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
