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

use pbps_dialect::{Dialect, DialectError, Probe, Statement, TypeChangeRisk};
use pbps_model::{
    Change, ChangeSet, ColumnType, Module, ObjectName, Role, Schema, Strategy, Table, TableName,
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

    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        types::normalize(ty)
    }

    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
        // Comparing unnormalized types would call `int` -> `integer` a change.
        // The trait says the caller normalizes first, but a dialect that only
        // works when it is called correctly is a trap, and normalizing twice is
        // free.
        let norm = |t: &ColumnType| types::normalize(t).unwrap_or_else(|_| t.clone());
        types::change_risk(&norm(from), &norm(to))
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

    fn validate_module(&self, name: &ObjectName, module: &Module) -> Vec<DialectError> {
        validate::module(name, module)
    }

    fn validate_role(&self, name: &str, role: &Role, schema: &Schema) -> Vec<DialectError> {
        validate::role(name, role, schema)
    }

    fn emit(&self, change: &Change, strategy: Strategy) -> Result<Vec<Statement>, DialectError> {
        emit::emit(change, strategy)
    }

    fn preflight(&self, changes: &ChangeSet) -> Vec<Probe> {
        preflight::probes(changes)
    }

    fn batch_separator(&self) -> Option<&'static str> {
        Some("GO")
    }
}
