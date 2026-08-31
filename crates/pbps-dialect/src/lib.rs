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

use pbps_model::{
    Change, ChangeSet, ColumnType, Module, ObjectName, RiskClass, Strategy, Table, TableName,
};

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

    /// A declaration that parses but that this dialect will not accept — a
    /// primary key over a column that does not exist, an IDENTITY on a type that
    /// cannot carry one. Distinct from [`DialectError::Unsupported`], which says
    /// "this database has no such feature"; this one says "this database has the
    /// feature, and you used it wrongly".
    #[error("{dialect}: {message}")]
    Invalid {
        dialect: &'static str,
        message: String,
    },
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

    /// Whether this statement can run inside a transaction.
    ///
    /// "One plan, one transaction, all or nothing" (SPEC §7.5) is only a
    /// promise the executor can keep if it knows in advance which statements
    /// would break it. A plan containing one of these **fails at plan time**,
    /// asking to be split into its own deployment — rather than being
    /// discovered halfway through an apply, with half the plan committed and no
    /// way back.
    ///
    /// Almost all DDL is transactional on SQL Server; the exceptions are
    /// specific (certain ONLINE index operations, full-text). Defaulting to
    /// `true` is therefore right, and the emitter marks the exceptions.
    pub transactional: bool,
}

impl Statement {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            own_batch: false,
            transactional: true,
        }
    }

    pub fn own_batch(mut self) -> Self {
        self.own_batch = true;
        self
    }

    pub fn non_transactional(mut self) -> Self {
        self.transactional = false;
        self
    }
}

/// A question asked of the data before a plan runs (SPEC §7.5).
///
/// # Why these are derived rather than written
///
/// The differ's output is a typed `ChangeSet`, so the tool already knows how
/// each change can fail. Atlas asks users to hand-write pre-migration checks;
/// here the change *is* the specification of its own failure mode, and a probe
/// nobody remembered to write is a probe that does not exist.
///
/// This does not contradict "data is not read to decide the risk class"
/// (§7.2). Classification stays static and happens offline; probes are the last
/// line of defence at apply time, where a connection is guaranteed and reading
/// the data is precisely the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// What is being checked, in the operator's words. It becomes the error
    /// message when the count is non-zero, so it has to name the object.
    pub description: String,

    /// Returns exactly one row with one integer column: how many rows would
    /// break. Zero means the change is safe to run on today's data.
    pub sql: String,
}

impl Probe {
    pub fn new(description: impl Into<String>, sql: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            sql: sql.into(),
        }
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

    /// The comparison form of a module definition (ADR-0002).
    ///
    /// # Why this is normalization and not parsing
    ///
    /// SPEC §8.2's rule stands: the database is the normalizer, and after an
    /// apply the stored text is read back so that both sides of the drift check
    /// live in the engine's own space. This is the *other* comparison — the
    /// declaration against the baseline — where the two texts were written by
    /// different hands and only whitespace and line endings may separate them.
    /// Anything still different after this is re-emitted as `CREATE OR ALTER`,
    /// which is idempotent and lossless: the cost of a false positive is
    /// restating one definition.
    ///
    /// Case is deliberately **kept**. Two definitions differing only in the case
    /// of a keyword are still two different texts to the engine's stored form,
    /// and folding case here would also fold it inside string literals, where
    /// it means something.
    fn normalize_definition(&self, definition: &str) -> String {
        let mut out = String::with_capacity(definition.len());
        let mut in_space = false;
        for ch in definition.trim().chars() {
            if ch.is_whitespace() {
                in_space = true;
                continue;
            }
            if in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = false;
            out.push(ch);
        }
        out
    }

    /// Checks whether this dialect supports the features the module uses.
    ///
    /// The default is "no objection", which is the honest answer from a dialect
    /// that does not implement modules: `validate` says what it checked, and
    /// this one checked nothing.
    fn validate_module(&self, _name: &ObjectName, _module: &Module) -> Vec<DialectError> {
        Vec::new()
    }

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
    ///
    /// `strategy` says *how* to get there (ADR-0003) and never *where* to go: a
    /// dialect that cannot honour a hint on this statement emits the statement
    /// without it rather than failing, because the desired state is the same
    /// either way and refusing would turn a performance hint into an outage.
    fn emit(&self, change: &Change, strategy: Strategy) -> Result<Vec<Statement>, DialectError>;

    /// Questions to ask the data before this **plan** runs (SPEC §7.5).
    ///
    /// The unit is the plan and not one change, and that is not a convenience.
    /// Probes run before the first statement, so every name in them must be the
    /// name the database still has — and a plan that renames a column and then
    /// tightens it to NOT NULL describes that column by its *new* name.
    /// Building each probe in isolation would query a column that does not
    /// exist yet, and the check the pre-flight most needed to make is the one
    /// it would skip. For the same reason a table this plan creates is not
    /// probed at all: it is empty, and nothing in it can violate anything.
    ///
    /// The default is "none", which is the honest answer for a dialect that has
    /// not implemented them: an empty list means "nothing was checked", and the
    /// caller reports it that way rather than as "nothing is wrong".
    fn preflight(&self, _changes: &ChangeSet) -> Vec<Probe> {
        Vec::new()
    }

    /// The line that separates batches in a script for this dialect, if the
    /// dialect has batches at all.
    ///
    /// `GO` for SQL Server; `None` for PostgreSQL, where a script is just a
    /// sequence of statements. Used when rendering a plan into a script a human
    /// can read or paste into their own tooling.
    fn batch_separator(&self) -> Option<&'static str> {
        None
    }
}

/// Renders emitted statements as one script, honouring [`Statement::own_batch`].
///
/// This lives here rather than in the CLI because "what a runnable script looks
/// like" is dialect knowledge — but only the separator differs per dialect, so
/// the walk itself is shared.
pub fn render_script(statements: &[Statement], separator: Option<&str>) -> String {
    let mut out = String::new();
    let mut previous_own_batch = false;
    for (i, s) in statements.iter().enumerate() {
        if i > 0 {
            if let Some(sep) = separator
                && (s.own_batch || previous_own_batch)
            {
                out.push_str(sep);
                out.push('\n');
            }
            out.push('\n');
        }
        out.push_str(&s.sql);
        out.push('\n');
        previous_own_batch = s.own_batch;
    }
    out
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

    /// An own-batch statement must be separated on both sides, and a dialect
    /// with no separator must render plain statements.
    #[test]
    fn scripts_put_separators_around_own_batch_statements() {
        let stmts = vec![
            Statement::new("A;"),
            Statement::new("B;").own_batch(),
            Statement::new("C;"),
            Statement::new("D;"),
        ];
        assert_eq!(
            render_script(&stmts, Some("GO")),
            "A;\nGO\n\nB;\nGO\n\nC;\n\nD;\n"
        );
        assert_eq!(render_script(&stmts, None), "A;\n\nB;\n\nC;\n\nD;\n");
        assert_eq!(render_script(&[], Some("GO")), "");
    }

    /// Almost all DDL is transactional, and the default has to reflect that —
    /// but the exceptions must be sayable, or §7.5's "fails at plan time"
    /// cannot be enforced.
    #[test]
    fn statements_default_to_shared_batch_and_to_transactional() {
        let s = Statement::new("ALTER TABLE t ADD c INT");
        assert!(!s.own_batch);
        assert!(s.transactional);
        assert!(s.clone().own_batch().own_batch);
        assert!(!s.non_transactional().transactional);
    }

    /// Line endings and indentation are what separate a definition someone
    /// pasted from the same definition someone reindented. Neither is a change
    /// worth re-stating a view for.
    #[test]
    fn definition_comparison_ignores_layout_but_not_case() {
        let d = MinimalDialect;
        assert_eq!(
            d.normalize_definition("  SELECT a,\r\n       b\n  FROM t\n"),
            d.normalize_definition("SELECT a, b FROM t")
        );
        assert_ne!(
            d.normalize_definition("select a from t"),
            d.normalize_definition("SELECT a FROM t"),
            "case is meaningful inside string literals, so it is kept"
        );
    }

    /// A dialect that has not implemented probes must say "I checked nothing",
    /// never "nothing is wrong".
    #[test]
    fn a_dialect_without_probes_returns_none() {
        let changes = ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(Change::DropTable {
                uid: "t_a1b2c3".parse().unwrap(),
                name: "dbo.customer".parse().unwrap(),
            })],
        };
        assert!(MinimalDialect.preflight(&changes).is_empty());
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

    fn emit(&self, _change: &Change, _strategy: Strategy) -> Result<Vec<Statement>, DialectError> {
        Err(DialectError::Unsupported {
            dialect: "minimal",
            feature: "SQL generation (the real emitter arrives in Phase 2)".to_owned(),
        })
    }
}
