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

use pbps_dialect::{Dialect, DialectError, Lexicon, Statement, TransactionFraming, TypeChangeRisk};
use pbps_model::{
    Change, ChangeSet, ColumnType, Module, ModuleId, Role, Schema, Strategy, Table, TableName,
};

pub mod catalog;
pub mod doctor;
pub mod edition;
pub mod emit;
pub mod estimate;
pub mod ident;
pub mod impact;
pub mod introspect;
pub mod preflight;
pub mod resolver;
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

    /// The default constraint `emit` creates for every column that declares a
    /// default (#969). An adopted column's existing default keeps whatever name
    /// it has and is not recreated, but its generated name is listed all the
    /// same: the declarations cannot tell the two apart, and the only thing
    /// the listing can refuse is a declared constraint spelled `DF_pbps_…`.
    fn generated_constraint_names(
        &self,
        name: &pbps_model::TableName,
        table: &pbps_model::schema::Table,
    ) -> Vec<(String, String)> {
        table
            .columns
            .iter()
            .filter(|(_, column)| column.default.is_some())
            .map(|(column, _)| {
                (
                    emit::default_constraint_name(name, column),
                    format!("the default of `{name}.{column}`"),
                )
            })
            .collect()
    }

    /// `[…]` and `"…"` both quote an identifier here, and neither of
    /// PostgreSQL's two extensions to the string literal exists: there is no
    /// `E'…'`, and `$` is an ordinary identifier character (`total$`) and the
    /// opener of a money literal (`$1.00`), never a dollar-quote tag.
    fn lexicon(&self) -> Lexicon {
        Lexicon {
            whitespace_is_ascii: false,
            quoted_identifiers: &[('[', ']'), ('"', '"')],
            escape_strings: false,
            dollar_quoted_strings: false,
            string_prefixes: &["n"],
            identifier_continues: pbps_model::module::is_regular_identifier_continue,
            reserved: ident::is_reserved,
            unicode_identifiers: false,
        }
    }

    /// The module's own schema, then `dbo`. **Measured** on SQL Server 2022:
    /// with `b.z` and `dbo.z` both present, `CREATE VIEW a.x AS SELECT *
    /// FROM z` reads `dbo.z`; with `a.p` and `dbo.p` both present it reads
    /// `a.p`; with only `b.z` it is refused (`Invalid object name 'z'`). So
    /// the own schema outranks `dbo`, and nothing else is on the path. The
    /// second step is the caller's default schema, which is `dbo` for every
    /// login that has not been given another; a deployer with another default
    /// schema says the edge with `depends_on:` (DECISIONS 317).
    fn bare_name_rank(&self, from: &str, to: &str) -> Option<usize> {
        if from.eq_ignore_ascii_case(to) {
            Some(0)
        } else if to.eq_ignore_ascii_case("dbo") {
            Some(1)
        } else {
            None
        }
    }

    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        types::normalize(ty)
    }

    fn retype_dependents(
        &self,
        from: &ColumnType,
        to: &ColumnType,
    ) -> pbps_dialect::RetypeDependents {
        types::retype_dependents(from, to)
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
        let mut found = validate::table(name, table);
        // The pull filters the ledger tables out by their qualified names, so a
        // declaration of one would read back as absent and be planned for
        // creation over the table that is there: refused here instead, as the
        // PostgreSQL dialect does. Same-named tables in another schema are the
        // project's.
        if catalog::is_ours(name) {
            found.push(DialectError::Invalid {
                dialect: types::DIALECT,
                message: format!(
                    "table `{name}` is one of the ledger tables this tool owns (SPEC §8.1). \
                     Only those names in schema `dbo` are reserved; the same table name in \
                     another schema is read back normally."
                ),
            });
        }
        found
    }

    fn declaration_notes(&self, schema: &Schema) -> Vec<String> {
        rows::not_checked_offline(schema)
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

    fn preflight(&self, changes: &ChangeSet) -> pbps_dialect::Preflight {
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

    /// None: a T-SQL function cannot modify data, so a declared `CHECK` has no
    /// side effect for a probe to set off (DECISIONS 537).
    fn probe_framing(&self) -> Option<TransactionFraming> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #878: the pull filters the three ledger tables out of `dbo`, so a
    /// declaration of one must be refused rather than read back as absent and
    /// planned for creation. Only the exact spelling pbps creates: another
    /// case is a different table on a case-sensitive database, and the same
    /// names in another schema stay the project's.
    #[test]
    fn validating_a_table_refuses_the_ledger_names_in_dbo_only() {
        let mut table = Table::default();
        table.columns.insert(
            "id".into(),
            pbps_model::Column::new("int".parse().expect("a type")),
        );
        let refused = |name: &str| {
            !Mssql
                .validate_table(&name.parse().unwrap(), &table)
                .is_empty()
        };
        for ours in [
            "dbo.__pbps_state",
            "dbo.__pbps_lock",
            "dbo.__pbps_state_confidential",
        ] {
            assert!(refused(ours), "{ours}");
        }
        for theirs in [
            "DBO.__PBPS_STATE_CONFIDENTIAL",
            "dbo.__PBPS_State",
            "app.__pbps_state_confidential",
            "dbo.__pbps_statements",
            "dbo.__pbps_customers",
        ] {
            assert!(!refused(theirs), "{theirs}");
        }
    }

    /// A bare name resolves in the module's own schema first and then in
    /// `dbo`, compared the way this engine compares names, and nowhere else.
    #[test]
    fn a_bare_name_ranks_the_own_schema_ahead_of_dbo() {
        assert_eq!(Dialect::bare_name_rank(&Mssql, "app", "app"), Some(0));
        assert_eq!(Dialect::bare_name_rank(&Mssql, "app", "APP"), Some(0));
        assert_eq!(Dialect::bare_name_rank(&Mssql, "app", "dbo"), Some(1));
        assert_eq!(Dialect::bare_name_rank(&Mssql, "dbo", "dbo"), Some(0));
        assert_eq!(Dialect::bare_name_rank(&Mssql, "app", "other"), None);
    }

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
        // Measured on 17.0.4075.5: non-ASCII White_Space separates tokens,
        // unlike PostgreSQL's identifier-byte rule (DECISIONS 475).
        assert_eq!(
            Mssql.normalize_definition("\u{85}SELECT\u{a0}1 AS x\u{3000}"),
            "SELECT 1 AS x"
        );
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
