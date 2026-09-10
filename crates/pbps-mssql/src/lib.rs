//! The Microsoft SQL Server dialect.
//!
//! Everything the rest of `pbps` must not know about T-SQL lives here: which
//! types exist and what they are really called, whether one type can become
//! another without losing anything, what SQL Server will refuse, and how a
//! [`Change`] is written as executable statements.
//!
//! The module split follows the three questions the [`Dialect`] trait asks:
//!
//! - [`types`] — what a type *is* ([`Dialect::normalize_type`]) and what changing
//!   one *costs* ([`Dialect::type_change_risk`]).
//! - [`validate`] — what the engine will refuse ([`Dialect::validate_table`]).
//! - [`emit`] — the SQL ([`Dialect::emit`]). The only place in the codebase that
//!   writes the SQL of a *change*.
//! - [`preflight`] — the questions a change implies about the data
//!   ([`Dialect::preflight`]), asked before the first statement runs.
//!
//! [`Dialect`] itself opens no connection; everything above is pure and
//! testable without a server. The two capabilities that genuinely need one —
//! [`catalog`] introspection and [`impact`] dependency analysis — are free
//! async functions taking a `pbps_db::Conn`, so the trait stays synchronous and
//! nothing that only compares schemas has to drag a runtime along.

use std::borrow::Cow;

use pbps_dialect::{
    Dialect, DialectError, Lexicon, Probe, Statement, TransactionFraming, TypeChangeRisk,
};
use pbps_model::{
    Change, ChangeSet, ColumnType, Module, ModuleId, Role, Schema, Strategy, Table, TableName,
};

pub mod catalog;
pub mod doctor;
pub mod edition;
pub mod emit;
pub mod ident;
pub mod impact;
pub mod introspect;
pub mod preflight;
pub mod rows;
pub mod state;
pub mod types;
pub mod validate;

/// The SQL Server dialect. Stateless: it holds knowledge, not a connection.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mssql;

impl Dialect for Mssql {
    fn name(&self) -> &'static str {
        types::DIALECT
    }

    /// `[…]` and `"…"` both quote an identifier here, and neither of
    /// PostgreSQL's two extensions to the string literal exists: there is no
    /// `E'…'`, and `$` is an ordinary identifier character (`total$`) and the
    /// opener of a money literal (`$1.00`), never a dollar-quote tag.
    fn lexicon(&self) -> Lexicon {
        Lexicon {
            quoted_identifiers: &[('[', ']'), ('"', '"')],
            escape_strings: false,
            dollar_quoted_strings: false,
            string_prefixes: &["n"],
            identifier_continues: pbps_model::module::is_regular_identifier_continue,
            reserved: ident::is_reserved,
            unicode_identifiers: false,
        }
    }

    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        types::normalize(ty)
    }

    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
        // Comparing unnormalized types would call `int` -> `integer` a change.
        // The trait says the caller normalizes first, but a dialect that only
        // works when it is called correctly is a trap, and normalizing twice is
        // free.
        //
        // The caller that cannot honour it is `validate_saved_plan`, which
        // re-derives a plan file's risks **because the file may have been
        // edited**, and an edited file spells its types however the editor
        // liked.
        //
        // A type that does not normalize ends the question here rather than
        // travelling on in its declared form. Falling back to the declared
        // value is the answer that looks conservative and is not: an unknown
        // *base* is claimed by no family and comes out `Incompatible` anyway,
        // but a rejected *modifier* keeps a base every family does claim, and
        // the family then reads the modifier as one this engine would accept —
        // `decimal(38,0) -> decimal(39,0)` comes out `Safe` past a maximum
        // precision of 38, and `varchar(8000) -> varchar(9000)` comes out
        // `Safe` past the 8000 a non-`max` string caps at. `Safe` contributes
        // no risk class, so an edited `risks: []` is accepted and the change
        // goes through the gate unreviewed.
        //
        // The same rule as `Postgres::type_change_risk`, which the trait states
        // for every implementation.
        let (Ok(from), Ok(to)) = (types::normalize(from), types::normalize(to)) else {
            return TypeChangeRisk::Incompatible;
        };
        types::change_risk(&from, &to)
    }

    /// SQL Server keeps identifiers exactly as written.
    ///
    /// It compares them under the database collation, which is usually
    /// case-insensitive — but that is a property of the server, not something
    /// that can be read off a YAML file. Folding case here would let two columns
    /// that a case-sensitive database keeps apart silently become one, so the
    /// identity is the only safe answer offline.
    fn fold_ident<'a>(&self, ident: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(ident)
    }

    fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
        ident::quote(ident)
    }

    fn validate_table(&self, name: &TableName, table: &Table) -> Vec<DialectError> {
        validate::table(name, table)
    }

    fn validate_module(&self, id: &ModuleId, module: &Module) -> Vec<DialectError> {
        validate::module(id, module)
    }

    fn validate_role(&self, name: &str, role: &Role, schema: &Schema) -> Vec<DialectError> {
        validate::role(name, role, schema)
    }

    /// A SQL Server database role is scoped to the database this connection
    /// names, so it is the tool's to create, rename and drop (ADR-0005) —
    /// today's answer, kept (ADR-0010 §3, DECISIONS 211).
    fn manages_roles(&self) -> bool {
        true
    }

    fn emit(&self, change: &Change, strategy: Strategy) -> Result<Vec<Statement>, DialectError> {
        emit::emit(change, strategy)
    }

    fn preflight(&self, changes: &ChangeSet) -> Vec<Probe> {
        preflight::probes(changes)
    }

    fn reads_back_at_default(&self, column: &pbps_model::Column) -> bool {
        rows::confirms_default(column)
    }

    fn batch_separator(&self) -> Option<&'static str> {
        Some("GO")
    }

    /// `XACT_ABORT ON` is what makes SPEC §7.5's "all or nothing" true rather
    /// than intended: without it SQL Server keeps the transaction alive past
    /// most statement-level errors, so a failed statement halfway through a
    /// plan would leave the earlier ones committable. Once it has doomed the
    /// transaction, `ROLLBACK` may find nothing to roll back and error with
    /// "no corresponding BEGIN TRANSACTION"; reporting that would replace the
    /// real failure — the statement that broke — with a confusing second one,
    /// so the rollback checks `@@TRANCOUNT` first.
    fn transaction_framing(&self) -> TransactionFraming {
        TransactionFraming {
            begin: "SET XACT_ABORT ON; BEGIN TRANSACTION;",
            commit: "COMMIT TRANSACTION;",
            rollback: "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The framing moved here from `pbps-db` unchanged (ADR-0014 §2). The two
    /// properties the live rollback test depends on are pinned where the text
    /// now lives: a statement error dooms the transaction, and the rollback
    /// tolerates one already doomed.
    #[test]
    fn the_transaction_framing_dooms_on_error_and_tolerates_a_dead_transaction() {
        let tx = Mssql.transaction_framing();
        assert!(tx.begin.contains("SET XACT_ABORT ON"));
        assert!(tx.rollback.starts_with("IF @@TRANCOUNT > 0"));
    }

    /// Role existence is a dialect capability since ADR-0010 §3, and this
    /// engine's answer did not move: a database role is inside the database.
    #[test]
    fn sql_server_roles_are_the_tools_to_create_and_drop() {
        assert!(Mssql.manages_roles());
    }

    /// The scanner became a parameter in ADR-0011 Amendment 2, and the answers
    /// this engine gets must be the ones it got before. A bracket is a name
    /// here, and neither of PostgreSQL's two extensions to the string literal
    /// exists — an `E` before a quote is an alias, and a `$` is a character in
    /// a name or the front of a money literal.
    #[test]
    fn a_bracket_holds_a_name_and_the_postgres_string_syntaxes_are_ordinary_code() {
        assert_ne!(
            Mssql.normalize_definition("SELECT [a  b] FROM t"),
            Mssql.normalize_definition("SELECT [a b] FROM t")
        );
        assert_eq!(
            Mssql.normalize_definition(r"SELECT E'it\'s  here'"),
            r"SELECT E'it\'s here'"
        );
        assert_eq!(
            Mssql.normalize_definition("SELECT $tag$a  b$tag$"),
            "SELECT $tag$a b$tag$"
        );
        assert_eq!(
            Mssql.normalize_definition("SELECT   $1.00  +  total$"),
            "SELECT $1.00 + total$"
        );
    }

    /// The classification is asked of the normalized types even when the
    /// caller forgot, because one caller cannot remember: `validate_saved_plan`
    /// re-derives a plan file's risks precisely because the file may have been
    /// edited, and an editor writes whatever spelling it likes.
    ///
    /// Both halves of that are here. The alias that reads as `Incompatible`
    /// blocks a plan that changes nothing; the rejected modifier that reads as
    /// `Safe` walks a change this engine will refuse past the gate.
    #[test]
    fn a_risk_is_judged_on_the_normalized_types_even_if_the_caller_forgot() {
        let mut wrong = Vec::new();
        for (from, to, expected) in [
            // Aliases, which name the same type and change nothing.
            ("integer", "int", TypeChangeRisk::Safe),
            ("numeric(10,2)", "decimal(10,2)", TypeChangeRisk::Safe),
            ("character varying(50)", "varchar(50)", TypeChangeRisk::Safe),
            // And a real narrowing, which the alias spelling must not hide.
            ("varchar(50)", "varchar(10)", TypeChangeRisk::Narrowing),
            // A type the catalogue will not spell stays refused rather than
            // falling through to a family. Both halves matter: an unknown base
            // that no family claims, and — the one that reads as safe when the
            // declared value travels on — a rejected modifier on a base that
            // every family does claim.
            ("nonesuch", "int", TypeChangeRisk::Incompatible),
            // Past this engine's maximum precision of 38, in the direction
            // that looks like widening.
            (
                "decimal(38,0)",
                "decimal(39,0)",
                TypeChangeRisk::Incompatible,
            ),
            (
                "decimal(39,0)",
                "decimal(38,0)",
                TypeChangeRisk::Incompatible,
            ),
            // Past the 8000 a non-`max` string caps at.
            (
                "varchar(8000)",
                "varchar(9000)",
                TypeChangeRisk::Incompatible,
            ),
            // A scale larger than its own precision, which the family reads as
            // an ordinary narrowing of a type that does not exist.
            (
                "decimal(10,2)",
                "decimal(10,99)",
                TypeChangeRisk::Incompatible,
            ),
        ] {
            let got = Mssql.type_change_risk(&ty(from), &ty(to));
            if got != expected {
                wrong.push(format!("`{from}` -> `{to}`: {got:?}, want {expected:?}"));
            }
        }
        // Collected rather than asserted one at a time: the first mismatch
        // would hide the rest, and the rejected-modifier cases are three
        // separate ways for a modifier to be refused.
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }
}
