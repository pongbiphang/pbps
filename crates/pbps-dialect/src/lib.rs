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
    Change, ChangeSet, ColumnType, Module, ObjectName, RiskClass, Role, Schema, Strategy, Table,
    TableName,
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

    /// The object renames this statement performs, `(from, to)`, in order.
    ///
    /// # Why the emitter has to say this
    ///
    /// One [`pbps_model::Change::RenameTable`] can need two statements — SQL
    /// Server's `sp_rename` cannot move a table between schemas and `ALTER
    /// SCHEMA TRANSFER` cannot rename it — and between them the table carries a
    /// name that appears in neither the baseline nor the plan. A staged apply
    /// checkpoints after every statement, so that intermediate name is the only
    /// one under which the table can be found, and a checkpoint that cannot
    /// find it records the environment without it.
    ///
    /// Working the name out anywhere else would mean a second copy of the
    /// emitter's statement order, and the two would drift. The emitter knows
    /// what its own SQL does to a name, so it says so — the same reason SQL
    /// itself is written in exactly one place.
    ///
    /// Empty for every statement that renames nothing, which is nearly all of
    /// them.
    pub renames: Vec<(TableName, TableName)>,

    /// The role renames this statement performs, `(from, to)`, for the same
    /// reason: a staged checkpoint after `ALTER ROLE ... WITH NAME` has to
    /// find the role under its new name, or it records the environment
    /// without it and a resume cannot see what changed on it while paused.
    pub role_renames: Vec<(String, String)>,
}

impl Statement {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            own_batch: false,
            transactional: true,
            renames: Vec::new(),
            role_renames: Vec::new(),
        }
    }

    /// Records that this statement moves `from` to `to`.
    pub fn renaming(mut self, from: TableName, to: TableName) -> Self {
        self.renames.push((from, to));
        self
    }

    /// Records that this statement renames the role `from` to `to`.
    pub fn renaming_role(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.role_renames.push((from.into(), to.into()));
        self
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
        // What the scanner is in the middle of. Layout is only layout in
        // `Code`: inside a literal or a quoted identifier the spacing is data,
        // and after `--` the *line ending* is what stops the comment, so
        // collapsing it would splice the next line into the comment and make
        // two bodies that run differently compare equal.
        enum At {
            Code,
            Quoted(char),
            Line,
            /// Carrying how many non-space characters have been consumed, so
            /// that the `*` of the opener cannot also close it (`/*/`).
            Block(usize),
        }

        let mut out = String::with_capacity(definition.len());
        let mut in_space = false;
        let mut at = At::Code;
        let text = definition.trim();
        let bytes = text.as_bytes();

        for (i, ch) in text.char_indices() {
            match at {
                At::Quoted(q) => {
                    // A doubled `''` needs no special case: the first closes the
                    // literal and the second opens it again, and everything
                    // between them is copied either way.
                    if if q == '[' { ch == ']' } else { ch == q } {
                        at = At::Code;
                    }
                    out.push(ch);
                }
                At::Line => {
                    if ch == '\n' {
                        // The newline is the comment's terminator, so it is
                        // structure. Its own trailing spaces are not.
                        while out.ends_with(' ') {
                            out.pop();
                        }
                        out.push('\n');
                        at = At::Code;
                        in_space = false;
                    } else if ch.is_whitespace() {
                        in_space = true;
                    } else {
                        if in_space {
                            out.push(' ');
                        }
                        in_space = false;
                        out.push(ch);
                    }
                }
                At::Block(seen) => {
                    // A block comment ends at `*/` wherever it falls, so nothing
                    // inside it is structure and its layout collapses like code.
                    if ch.is_whitespace() {
                        in_space = true;
                    } else {
                        if in_space {
                            out.push(' ');
                        }
                        in_space = false;
                        out.push(ch);
                        at = if ch == '/' && seen >= 2 && out.ends_with("*/") {
                            At::Code
                        } else {
                            At::Block(seen + 1)
                        };
                    }
                }
                At::Code => {
                    if ch.is_whitespace() {
                        in_space = true;
                        continue;
                    }
                    if in_space && !out.is_empty() && !out.ends_with('\n') {
                        out.push(' ');
                    }
                    in_space = false;
                    let next = bytes.get(i + ch.len_utf8()).copied();
                    at = match (ch, next) {
                        ('-', Some(b'-')) => At::Line,
                        ('/', Some(b'*')) => At::Block(0),
                        ('\'' | '"' | '[', _) => At::Quoted(ch),
                        _ => At::Code,
                    };
                    out.push(ch);
                }
            }
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

    /// Checks whether this dialect can express the role and its grants
    /// (ADR-0005). The same default, for the same reason. The schema is there
    /// so a grant can be checked against what its target *is*: which
    /// permissions apply to a table, a procedure or a function is the engine's
    /// rule, and a `GRANT` the engine refuses would fail an apply after the
    /// changes before it had run.
    fn validate_role(&self, _name: &str, _role: &Role, _schema: &Schema) -> Vec<DialectError> {
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

    /// Whitespace stops being layout the moment it is inside a literal or a
    /// quoted identifier. Collapsing it there would make two module bodies that
    /// return different rows compare equal, and the change would never be
    /// planned at all.
    #[test]
    fn whitespace_inside_a_literal_is_data() {
        let d = MinimalDialect;
        assert_ne!(
            d.normalize_definition("SELECT 'a  b'"),
            d.normalize_definition("SELECT 'a b'")
        );
        assert_ne!(
            d.normalize_definition("SELECT [a  b] FROM t"),
            d.normalize_definition("SELECT [a b] FROM t")
        );
        // Layout around the literal is still layout.
        assert_eq!(
            d.normalize_definition("SELECT   'a  b'\n  FROM t"),
            d.normalize_definition("SELECT 'a  b' FROM t")
        );
        // A doubled quote closes and reopens; the text after it is still inside
        // the literal, so its spacing survives too.
        assert_ne!(
            d.normalize_definition("SELECT 'it''s  here'"),
            d.normalize_definition("SELECT 'it''s here'")
        );
    }

    /// A `--` comment runs to the end of its line, so that line ending is
    /// structure, not layout. Collapsing it splices the next statement into the
    /// comment — and two bodies that execute differently would compare equal,
    /// which means the change is never planned.
    #[test]
    fn a_line_comment_keeps_the_newline_that_ends_it() {
        let d = MinimalDialect;
        assert_ne!(
            d.normalize_definition("SELECT 1 -- note\nUNION ALL SELECT 2"),
            d.normalize_definition("SELECT 1 -- note UNION ALL SELECT 2")
        );
        // Indentation around the comment is still only indentation.
        assert_eq!(
            d.normalize_definition("SELECT 1   --  note\n   UNION ALL SELECT 2"),
            d.normalize_definition("SELECT 1 -- note\nUNION ALL SELECT 2")
        );
    }

    /// A block comment ends at `*/` wherever that falls, so nothing inside it
    /// is structure — but a quote inside one must not be read as opening a
    /// literal, and `/*/` must not close what it opened.
    #[test]
    fn a_block_comment_collapses_but_does_not_confuse_the_scanner() {
        let d = MinimalDialect;
        assert_eq!(
            d.normalize_definition("SELECT /* it's\n   fine */ 1"),
            d.normalize_definition("SELECT /* it's fine */ 1")
        );
        // If the opener's own `*` closed the comment, the `'` would be read as
        // starting a literal and everything after it would keep its spacing.
        assert_eq!(
            d.normalize_definition("SELECT /*/ ' */ a  b"),
            "SELECT /*/ ' */ a b"
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
