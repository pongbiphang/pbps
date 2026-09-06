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

use pbps_dialect::{Dialect, DialectError, Statement, TransactionFraming, TypeChangeRisk};
use pbps_model::{Change, ColumnType, Strategy, Table, TableName};

/// A part of the dialect that Phase 5 has not built yet.
///
/// One place, so that "what is missing" is a list rather than a habit, and so
/// that every refusal names the step that supplies it. The message is written
/// for whoever runs the command, not for whoever writes the crate: it says what
/// pbps cannot do, and it does not pretend the answer is "nothing to do".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unbuilt {
    TypeCatalogue,
    Introspection,
    Emitter,
    Modules,
    Roles,
    ReferenceData,
    Ledger,
    Probes,
}

impl Unbuilt {
    const fn step(self) -> &'static str {
        match self {
            Unbuilt::TypeCatalogue => "the type catalogue (Phase 5 step 2)",
            Unbuilt::Introspection => "reading a database back (Phase 5 step 3)",
            Unbuilt::Emitter => "generating statements (Phase 5 step 4)",
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

/// The PostgreSQL dialect.
pub struct Postgres;

impl Dialect for Postgres {
    fn name(&self) -> &'static str {
        "postgres"
    }

    /// PostgreSQL runs DDL inside a transaction, and a failed statement aborts
    /// the whole one — which is what SQL Server needs `SET XACT_ABORT ON` to
    /// approximate. Nothing else has to be said, so nothing else is.
    fn transaction_framing(&self) -> TransactionFraming {
        TransactionFraming {
            begin: "BEGIN;",
            commit: "COMMIT;",
            // Tolerates a transaction the server has already killed, so that
            // this statement's own error cannot replace the real failure.
            rollback: "ROLLBACK;",
        }
    }

    fn normalize_type(&self, _ty: &ColumnType) -> Result<ColumnType, DialectError> {
        Err(Unbuilt::TypeCatalogue.refuse())
    }

    fn type_change_risk(&self, _from: &ColumnType, _to: &ColumnType) -> TypeChangeRisk {
        // Not a refusal, because the signature has nowhere to put one — so it
        // answers with the class that stops a plan rather than the one that
        // waves it through. An unbuilt catalogue must never read as "safe".
        TypeChangeRisk::Narrowing
    }

    /// Unquoted identifiers fold to **lower** case, where SQL Server folds to
    /// nothing at all. Measured, and the first thing in this crate that is a
    /// real answer rather than a refusal, because the loader needs it before
    /// anything connects.
    fn fold_ident<'a>(&self, ident: &'a str) -> Cow<'a, str> {
        if ident.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Owned(ident.to_lowercase())
        } else {
            Cow::Borrowed(ident)
        }
    }

    fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
        // A double quote inside an identifier is doubled; a NUL cannot be in
        // one at all, and the engine's own limit is bytes, not characters.
        if ident.is_empty() {
            return Err(DialectError::UnquotableIdent(ident.to_owned()));
        }
        if ident.contains('\0') {
            return Err(DialectError::UnquotableIdent(ident.to_owned()));
        }
        Ok(format!("\"{}\"", ident.replace('"', "\"\"")))
    }

    fn validate_table(&self, _name: &TableName, _table: &Table) -> Vec<DialectError> {
        vec![Unbuilt::TypeCatalogue.refuse()]
    }

    fn emit(&self, _change: &Change, _strategy: Strategy) -> Result<Vec<Statement>, DialectError> {
        Err(Unbuilt::Emitter.refuse())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusals are the point of this crate until step 4 lands, so what
    /// they say is tested like any other output: each names the step that
    /// supplies it, and none of them reads as "there is nothing to do".
    #[test]
    fn an_unbuilt_part_refuses_by_name_and_never_reads_as_nothing_to_do() {
        for part in [
            Unbuilt::TypeCatalogue,
            Unbuilt::Introspection,
            Unbuilt::Emitter,
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
    /// cleanly and changes nothing — the silent wrong answer. It has to be an
    /// error, and the type is what makes that so.
    #[test]
    fn an_unbuilt_emitter_is_an_error_and_not_an_empty_plan() {
        let risk = Postgres.type_change_risk(&ty("int"), &ty("bigint"));
        assert_eq!(
            risk,
            TypeChangeRisk::Narrowing,
            "an unbuilt catalogue must not answer `Safe`"
        );
        assert!(Postgres.normalize_type(&ty("int")).is_err());
    }

    /// Unquoted identifiers fold down, not away: this is the difference from
    /// SQL Server that the loader sees before anything connects.
    #[test]
    fn an_unquoted_identifier_folds_to_lower_case() {
        assert_eq!(Postgres.fold_ident("Customer"), "customer");
        assert_eq!(Postgres.fold_ident("CUSTOMER"), "customer");
        // Already lower: borrowed, not copied.
        assert!(matches!(Postgres.fold_ident("customer"), Cow::Borrowed(_)));
    }

    /// Quoting is what stops a name from being read as syntax, so the
    /// negative cases are the ones worth having.
    #[test]
    fn quoting_doubles_an_embedded_quote_and_refuses_what_cannot_be_a_name() {
        assert_eq!(Postgres.quote_ident("customer").unwrap(), "\"customer\"");
        assert_eq!(Postgres.quote_ident("Odd Name").unwrap(), "\"Odd Name\"");
        assert_eq!(Postgres.quote_ident("a\"b").unwrap(), "\"a\"\"b\"");
        assert!(Postgres.quote_ident("").is_err());
        assert!(Postgres.quote_ident("a\0b").is_err());
    }

    fn ty(s: &str) -> ColumnType {
        s.parse().expect("a type parses")
    }
}
