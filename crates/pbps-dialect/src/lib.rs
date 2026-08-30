//! The dialect abstraction.
//!
//! # Boundaries
//!
//! `pbps-model`, `pbps-load` and `pbps-diff` know nothing about any database.
//! All knowledge of "is this type valid", "how is this change written as SQL" and
//! "do these two identifiers name the same thing" is concentrated in the
//! implementations here (SPEC §11.2).
//!
//! # Why this is split into two traits
//!
//! [`Dialect`] is pure and touches no network, and it is all that Phase 1's diff
//! and Phase 2's planner need. Connection-bound capabilities (introspection,
//! rename impact analysis) are left to Phase 3's `DialectDb`, which is when async
//! and a database driver enter the picture. Binding them together would force
//! Phase 1's tests to drag a runtime along.
//!
//! # The PostgreSQL check done in Phase 0
//!
//! This interface was deliberately validated against PG as a second hypothetical
//! implementation. The four most easily missed differences all fit:
//!
//! | Difference | PostgreSQL | SQL Server | How the interface accommodates it |
//! |---|---|---|---|
//! | Unquoted identifiers | folded to lowercase | kept as written | [`Dialect::fold_ident`] |
//! | Type + nullability change | needs two statements | can be merged into one | [`Dialect::emit`] returns a `Vec` |
//! | Effect of a rename on views | updated automatically | definition text goes stale | left to Phase 3's `DialectDb` |
//! | Batch separation | not needed | some DDL needs its own batch | [`Statement::own_batch`] |
//!
//! If adding a dialect later requires changing `pbps-model`, this abstraction was
//! drawn in the wrong place.

use std::borrow::Cow;

use pbps_model::{Change, ColumnType, RiskClass, Table, TableName};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DialectError {
    #[error("{dialect} has no type `{ty}`")]
    UnknownType { dialect: &'static str, ty: String },

    #[error("wrong number of arguments for `{ty}` in {dialect}: {detail}")]
    BadTypeArity {
        dialect: &'static str,
        ty: String,
        detail: String,
    },

    #[error("{dialect} does not support {feature}")]
    Unsupported {
        dialect: &'static str,
        feature: String,
    },

    #[error("the identifier `{0}` cannot be written into SQL safely")]
    UnquotableIdent(String),
}

/// How safe a type change is.
///
/// The criterion is **whether this kind of change can fail at all**, not whether
/// today's data happens to be safe — inspecting data is a runtime concern and has
/// no place in the declarative layer (SPEC §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeChangeRisk {
    /// No change, or a widening (`int` → `bigint`, `varchar(50)` → `varchar(100)`).
    Safe,
    /// Narrowing: may truncate.
    Narrowing,
    /// Incompatible: the conversion itself may fail (`nvarchar` → `int`).
    Incompatible,
}

impl TypeChangeRisk {
    /// Maps to the risk class used by the gate. `Safe` needs no approval.
    pub const fn risk_class(self) -> Option<RiskClass> {
        match self {
            TypeChangeRisk::Safe => None,
            TypeChangeRisk::Narrowing | TypeChangeRisk::Incompatible => Some(RiskClass::Narrowing),
        }
    }
}

/// One executable statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub sql: String,

    /// Must be sent as a batch of its own.
    ///
    /// Some SQL Server DDL cannot share a batch with statements that reference it
    /// (adding a column and referencing it in the same batch fails to compile).
    /// PostgreSQL has no such restriction and can always leave this `false` — but
    /// the field has to exist in the interface, or the executor has no way to know
    /// whether to split.
    pub own_batch: bool,
}

impl Statement {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            own_batch: false,
        }
    }

    pub fn own_batch(mut self) -> Self {
        self.own_batch = true;
        self
    }
}

/// Dialect knowledge that needs no database connection.
pub trait Dialect {
    fn name(&self) -> &'static str;

    /// Expands aliases and fills in omitted default arguments, so that two
    /// semantically identical spellings become the same value.
    ///
    /// This step is a precondition for diff being correct: unless `INTEGER` and
    /// `int` converge on one value first, every run reports a type change.
    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError>;

    /// Judges how safe a type change is. The caller is responsible for calling
    /// [`normalize_type`](Dialect::normalize_type) first.
    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk;

    /// The canonical form of an unquoted identifier in this dialect.
    ///
    /// PostgreSQL folds to lowercase; SQL Server keeps it as written. Name
    /// comparison must go through this, or names read back by introspection will
    /// not line up with the declarations and drift detection will cry wolf daily.
    fn fold_ident<'a>(&self, ident: &'a str) -> Cow<'a, str>;

    /// Quotes an identifier for embedding in SQL.
    fn quote_ident(&self, ident: &str) -> Result<String, DialectError>;

    /// Checks whether this dialect supports the features the table uses.
    ///
    /// Returns every problem rather than the first one — the user should see
    /// everything that needs fixing in one pass.
    fn validate_table(&self, name: &TableName, table: &Table) -> Vec<DialectError>;

    /// Renders one change as statements.
    ///
    /// Returning a `Vec` is necessary: PostgreSQL has to split a type change and
    /// a nullability change into two `ALTER COLUMN` statements, whereas SQL Server
    /// can merge them into one.
    fn emit(&self, change: &Change) -> Result<Vec<Statement>, DialectError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_type_changes_need_no_approval() {
        assert_eq!(TypeChangeRisk::Safe.risk_class(), None);
    }

    #[test]
    fn unsafe_type_changes_map_to_narrowing() {
        assert_eq!(
            TypeChangeRisk::Narrowing.risk_class(),
            Some(RiskClass::Narrowing)
        );
        assert_eq!(
            TypeChangeRisk::Incompatible.risk_class(),
            Some(RiskClass::Narrowing)
        );
    }

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    #[test]
    fn minimal_dialect_treats_shrinking_as_narrowing() {
        let d = MinimalDialect;
        assert_eq!(
            d.type_change_risk(&ty("nvarchar(100)"), &ty("nvarchar(50)")),
            TypeChangeRisk::Narrowing
        );
        assert_eq!(
            d.type_change_risk(&ty("nvarchar(50)"), &ty("nvarchar(100)")),
            TypeChangeRisk::Safe
        );
        assert_eq!(
            d.type_change_risk(&ty("bigint"), &ty("bigint")),
            TypeChangeRisk::Safe
        );
        assert_eq!(
            d.type_change_risk(&ty("bigint"), &ty("nvarchar(10)")),
            TypeChangeRisk::Incompatible
        );
    }

    /// `max` means "no upper bound", not a length of zero, so the direction must
    /// not come out backwards.
    #[test]
    fn minimal_dialect_handles_max_length() {
        let d = MinimalDialect;
        assert_eq!(
            d.type_change_risk(&ty("nvarchar(max)"), &ty("nvarchar(100)")),
            TypeChangeRisk::Narrowing
        );
        assert_eq!(
            d.type_change_risk(&ty("nvarchar(100)"), &ty("nvarchar(max)")),
            TypeChangeRisk::Safe
        );
    }

    #[test]
    fn statements_default_to_shared_batch() {
        let s = Statement::new("ALTER TABLE t ADD c INT");
        assert!(!s.own_batch);
        assert!(s.own_batch().own_batch);
    }
}

/// The minimal dialect used by tests and by Phase 1.
///
/// **It is not any real database.** It implements only the conservative rules
/// needed for type comparison, so that database-independent logic (diff, risk
/// classification) can be tested. The real MSSQL implementation lands in Phase 2.
///
/// Conservative means: when in doubt, call it dangerous. Better to make the user
/// approve one more time than to let a truncating change through unflagged.
#[derive(Debug, Clone, Copy, Default)]
pub struct MinimalDialect;

impl Dialect for MinimalDialect {
    fn name(&self) -> &'static str {
        "minimal"
    }

    /// No alias expansion — which names alias which is real-dialect knowledge.
    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        Ok(ty.clone())
    }

    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
        if from == to {
            return TypeChangeRisk::Safe;
        }
        if from.base != to.base {
            return TypeChangeRisk::Incompatible;
        }
        match (from.is_max(), to.is_max()) {
            // Shrinking from unbounded to bounded can always truncate.
            (true, false) => TypeChangeRisk::Narrowing,
            // Widening to unbounded is safe.
            (false, true) => TypeChangeRisk::Safe,
            _ => match (from.first_int_arg(), to.first_int_arg()) {
                (Some(a), Some(b)) if b < a => TypeChangeRisk::Narrowing,
                (Some(_), Some(_)) => TypeChangeRisk::Safe,
                // A mismatch in whether arguments are present at all (say
                // decimal → decimal(18,2)) is ambiguous; conservatively treat it
                // as narrowing.
                _ => TypeChangeRisk::Narrowing,
            },
        }
    }

    fn fold_ident<'a>(&self, ident: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(ident)
    }

    fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
        if ident.contains('"') {
            return Err(DialectError::UnquotableIdent(ident.to_owned()));
        }
        Ok(format!("\"{ident}\""))
    }

    fn validate_table(&self, _name: &TableName, _table: &Table) -> Vec<DialectError> {
        Vec::new()
    }

    fn emit(&self, _change: &Change) -> Result<Vec<Statement>, DialectError> {
        Err(DialectError::Unsupported {
            dialect: "minimal",
            feature: "SQL generation (the real emitter arrives in Phase 2)".to_owned(),
        })
    }
}
