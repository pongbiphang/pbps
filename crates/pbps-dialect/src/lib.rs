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
//! | One change, several statements | a view rebuild is `DROP`, `CREATE` and a `GRANT` per permission | a cross-schema table rename is `sp_rename` plus `ALTER SCHEMA TRANSFER` | [`Dialect::emit`] returns a `Vec` |
//! | Effect of a rename on views | updated automatically | definition text goes stale | left to Phase 3's `DialectDb` |
//! | Batch separation | not needed | some DDL needs its own batch | [`Statement::own_batch`] |
//!
//! The second row once read "type + nullability change: PostgreSQL needs two
//! statements, SQL Server merges them". Measured, PostgreSQL takes both in one
//! `ALTER TABLE`; the `Vec` stands for the reasons above (ADR-0011, Amendment
//! 1). A false reason under a true conclusion is the worse error, because
//! nothing downstream fails to expose it.
//!
//! If adding a dialect later requires changing `pbps-model`, this abstraction was
//! drawn in the wrong place.

use std::borrow::Cow;
use std::collections::BTreeMap;

use pbps_model::{
    Change, ChangeSet, ColumnType, Module, ModuleId, ModuleKind, ObjectName, RiskClass, Role,
    Schema, Strategy, Table, TableName,
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

    /// The objects this statement brings into being. A staged checkpoint
    /// scopes the environment by the identities the catalog had before the
    /// plan, and an object the plan just created is not among them — so a
    /// grant a new role gained, or a row a new table gained, while the
    /// deployment was paused went unseen by `--resume` and was recorded as
    /// clean by the closing entry. The executor adopts each of these into
    /// the live identities, under the uid the plan gave it, the moment the
    /// statement commits (DECISIONS 100).
    pub creates: Vec<Created>,
}

/// An object a statement creates, for a staged checkpoint to adopt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Created {
    Table(TableName),
    Column(TableName, String),
    Role(String),
}

impl Statement {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            own_batch: false,
            transactional: true,
            renames: Vec::new(),
            role_renames: Vec::new(),
            creates: Vec::new(),
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

    /// Records that this statement creates `what`.
    pub fn creating(mut self, what: Created) -> Self {
        self.creates.push(what);
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
            /// Block comments nest, in T-SQL and in PostgreSQL alike —
            /// measured, `SELECT /* a /* b */ c */ 1` returns 1 on both —
            /// so `depth` counts the unmatched openers. `seen` counts the
            /// non-space characters since the last opener, so that an opener's
            /// own `*` cannot also close it (`/*/`).
            Block {
                depth: usize,
                seen: usize,
            },
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
                At::Block { depth, seen } => {
                    // Nothing inside a block comment is structure, so its layout
                    // collapses like code — but the comment ends only at the
                    // `*/` that matches its opener. Leaving at the first `*/`
                    // read the rest of an outer comment as code: an apostrophe
                    // in it opened a literal that was not there, and the
                    // spacing inside the real literal after it was folded as
                    // layout, so two bodies returning different strings
                    // compared equal (ADR-0011, Amendment 2).
                    if ch.is_whitespace() {
                        in_space = true;
                    } else {
                        if in_space {
                            out.push(' ');
                        }
                        in_space = false;
                        out.push(ch);
                        let next = bytes.get(i + ch.len_utf8()).copied();
                        at = if ch == '/' && seen >= 2 && out.ends_with("*/") {
                            if depth == 1 {
                                At::Code
                            } else {
                                At::Block {
                                    depth: depth - 1,
                                    seen: 2,
                                }
                            }
                        } else if ch == '/' && next == Some(b'*') {
                            At::Block {
                                depth: depth + 1,
                                seen: 0,
                            }
                        } else {
                            At::Block {
                                depth,
                                seen: seen + 1,
                            }
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
                        ('/', Some(b'*')) => At::Block { depth: 1, seen: 0 },
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
    fn validate_module(&self, _id: &ModuleId, _module: &Module) -> Vec<DialectError> {
        Vec::new()
    }

    /// Whether this engine keeps modules of `kind` in the same namespace as
    /// tables, so that a table and a module may not share a name.
    ///
    /// Measured on PostgreSQL 18 (ADR-0009 §1): `CREATE VIEW app.customer`
    /// over a table `app.customer` fails, and `CREATE FUNCTION app.customer(int)`
    /// succeeds — views live in `pg_class` with tables, routines live in
    /// `pg_proc`. On SQL Server every kind shares `sys.objects`, which is what
    /// the default says.
    fn shares_namespace_with_tables(&self, _kind: ModuleKind) -> bool {
        true
    }

    /// Whether this engine lets modules of `kind` overload — several objects
    /// of one name, told apart by their argument types.
    ///
    /// The default is "no", which is SQL Server's answer for every kind: a
    /// declared signature there names an object the engine cannot have, so
    /// `check_module_names` refuses it rather than emitting a `DROP` the
    /// engine would not parse.
    fn overloads(&self, _kind: ModuleKind) -> bool {
        false
    }

    /// This type as the engine spells it *for routine identity*.
    ///
    /// Deliberately not [`Dialect::normalize_type`]. Measured on PostgreSQL 18
    /// (ADR-0009 §1), `f(varchar(10))` and `f(varchar(20))` are one function,
    /// identified as `f(character varying)`: the engine discards type
    /// modifiers when it identifies a routine, and keeps them when it types a
    /// column. Reusing the column normalizer would key one engine object as
    /// two modules, and every `DROP` and `GRANT` the plan emitted would name a
    /// signature the engine resolves to something else.
    ///
    /// The default defers to the column spelling, which is right for any
    /// dialect where nothing overloads: nothing calls it there.
    fn normalize_routine_arg(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        self.normalize_type(ty)
    }

    /// Whether this engine's roles are the tool's to create, rename and drop
    /// (ADR-0010 §3).
    ///
    /// SQL Server's database role lives inside the one database the tool is
    /// connected to, so ADR-0005 manages its existence, and that is what the
    /// default says. A PostgreSQL role is a cluster object — visible from, and
    /// granted in, every database of the cluster — and a tool whose blast
    /// radius is one database must not own an object whose blast radius is
    /// the cluster; that dialect answers `false`, and on it a declared role
    /// the cluster lacks is refused by `plan --db` with the `CREATE ROLE` to
    /// run by hand, while `drop-role` revokes the declared grants and leaves
    /// the `DROP ROLE` to a human (DECISIONS 211). Grants are managed either
    /// way: this is about the principal, not what it holds.
    fn manages_roles(&self) -> bool {
        true
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
    /// Returning a `Vec` is necessary because one change is not always one
    /// statement: SQL Server's `RenameTable` across schemas is `sp_rename` plus
    /// `ALTER SCHEMA TRANSFER` (see [`Statement::renames`]), and PostgreSQL's
    /// `AlterModule` on a view is `DROP VIEW`, `CREATE VIEW` and a `GRANT` per
    /// declared permission (ADR-0009 §3). An earlier version of this comment
    /// gave a different reason — that PostgreSQL splits a type change and a
    /// nullability change into two `ALTER COLUMN`s — and it is false: measured,
    /// one `ALTER TABLE` takes both subcommands (ADR-0011, Amendment 1).
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

    /// The statements that open, commit and roll back one transaction in this
    /// dialect.
    ///
    /// Required, not defaulted: a default here would be one engine's answer
    /// under a neutral name, the shape ADR-0011 found three times. SQL
    /// Server's `begin` has to carry `SET XACT_ABORT ON` or a failed statement
    /// leaves the earlier ones committable, and its `rollback` has to tolerate
    /// a transaction the server already killed; PostgreSQL needs neither.
    /// `pbps-db` runs these and owns the framing around them, and holds no SQL
    /// of its own (ADR-0014 §2).
    fn transaction_framing(&self) -> TransactionFraming;

    /// Whether a read-back can tell a cell of this column that is at its
    /// default from one that is not.
    ///
    /// The row reader asks the engine to confirm a cell at its default only
    /// where the default is a literal and the type has `=`; anything else is
    /// read as a value nobody can tell from the default. A caller holding a
    /// row to "at its default" has to ask the same question, or it refuses a
    /// `NEWID()` cell for being there (DECISIONS 191). The default is `false`:
    /// a dialect that has not said is one that cannot confirm anything.
    fn reads_back_at_default(&self, _column: &pbps_model::Column) -> bool {
        false
    }
}

/// Renders emitted statements as one script, honouring [`Statement::own_batch`].
///
/// This lives here rather than in the CLI because "what a runnable script looks
/// like" is dialect knowledge — but only the separator differs per dialect, so
/// the walk itself is shared.
/// The whole-schema module rules that only the engine can answer.
///
/// The dialect-free half lives in `pbps_model::module::check_names`. These two
/// are here because their answer differs by engine (ADR-0009 §1,
/// DECISIONS 201):
///
/// - a module competing for its name with a table, or with another module,
///   which is a rule about one namespace and holds for views everywhere and
///   for routines only on SQL Server;
/// - a declared signature on an engine where that kind does not overload,
///   which names an object that engine cannot have.
///
/// Both would otherwise surface as an engine error at apply time, on a
/// database that is already half-changed.
///
/// The module-against-module half exists because this crate's callers stopped
/// getting it for free (DECISIONS 204). `Schema::modules` was keyed by
/// `ObjectName`, so two
/// modules with one name could not both be in the map; keyed by `ModuleId`
/// they can — a trigger is distinguished by its table and a routine by its
/// signature — and on an engine that keeps them in one namespace per schema
/// that is a pair of objects it cannot have. Removing the reason for a
/// guarantee is not the same as replacing it.
pub fn check_module_names(schema: &Schema, dialect: &dyn Dialect) -> Vec<String> {
    let mut problems = Vec::new();
    // Only the kinds the engine keeps beside tables: the rest have namespaces
    // of their own, where `ModuleId` is already the whole identity.
    let mut shared: BTreeMap<ObjectName, &ModuleId> = BTreeMap::new();
    for (id, module) in &schema.modules {
        if dialect.shares_namespace_with_tables(module.kind) {
            let name = id.object_name();
            if let Some(first) = shared.insert(name.clone(), id)
                && first != id
            {
                problems.push(format!(
                    "`{first}` and `{id}` are both declared as `{name}`; {} keeps them in one \
                     namespace per schema, so it can hold only one of them",
                    dialect.name()
                ));
            }
            if schema.tables.contains_key(&name) {
                problems.push(format!(
                    "`{name}` is declared both as a table and as a {}; {} keeps tables and {}s \
                     in one namespace per schema",
                    module.kind,
                    dialect.name(),
                    module.kind
                ));
            }
        }
        match id {
            ModuleId::Routine(_) if !dialect.overloads(module.kind) => {
                problems.push(format!(
                    "`{id}` is declared with an argument list, but {} does not overload a {}: \
                     name it `{}` instead",
                    dialect.name(),
                    module.kind,
                    id.object_name()
                ));
            }
            // The mirror image. Where the kind overloads, the engine identifies
            // every routine by its argument types — the one taking none as
            // `app.f()` — so a bare name is a key the engine never reads back
            // under, and connected planning would see the same routine as a
            // drop and a create.
            ModuleId::Named(_) if dialect.overloads(module.kind) => {
                problems.push(format!(
                    "`{id}` is declared without an argument list, but {} identifies a {} by its \
                     argument types: name it `{id}()` if it takes none, or list the types",
                    dialect.name(),
                    module.kind
                ));
            }
            ModuleId::Routine(_) | ModuleId::Named(_) | ModuleId::Trigger { .. } => {}
        }
    }
    problems
}

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

    /// Block comments nest (measured: `SELECT /* a /* b */ c */ 1` returns 1),
    /// so the scanner may leave one only at the `*/` that matches its opener.
    /// Leaving at the first `*/` reads `it's here */` as code: the apostrophe
    /// opens a literal that is not there, the `'` that really opens one closes
    /// it, and the literal's spacing is folded as layout — two bodies returning
    /// different strings compare equal, and the change is never planned.
    #[test]
    fn a_nested_block_comment_ends_at_the_closer_that_matches_its_opener() {
        let d = MinimalDialect;
        assert_ne!(
            d.normalize_definition("SELECT /* outer /* inner */ it's here */ 'a  b'"),
            d.normalize_definition("SELECT /* outer /* inner */ it's here */ 'a b'")
        );
        // Inside the outer comment, layout is still only layout.
        assert_eq!(
            d.normalize_definition("SELECT /* outer\n  /* inner */\n  done */ 1"),
            "SELECT /* outer /* inner */ done */ 1"
        );
        // The overlapping spelling opens and does not close, at either depth.
        assert_eq!(
            d.normalize_definition("SELECT /*/ /*/ ' */ ' */ a  b"),
            "SELECT /*/ /*/ ' */ ' */ a b"
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

    // ---- module identity (ADR-0009 §1) ----

    /// An engine that overloads its routines and keeps them out of the table
    /// namespace: PostgreSQL's answers, as measured in ADR-0009 §1, with the
    /// modifier-discarding normalization that goes with them.
    #[derive(Debug, Clone, Copy, Default)]
    struct OverloadingDialect;

    impl Dialect for OverloadingDialect {
        fn name(&self) -> &'static str {
            "overloading"
        }
        fn transaction_framing(&self) -> TransactionFraming {
            MinimalDialect.transaction_framing()
        }
        fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
            Ok(ty.clone())
        }
        fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> Cow<'a, str> {
            Cow::Borrowed(ident)
        }
        fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
            Ok(ident.to_owned())
        }
        fn validate_table(&self, _n: &TableName, _t: &Table) -> Vec<DialectError> {
            Vec::new()
        }
        fn emit(&self, _c: &Change, _s: Strategy) -> Result<Vec<Statement>, DialectError> {
            Ok(Vec::new())
        }
        fn shares_namespace_with_tables(&self, kind: ModuleKind) -> bool {
            matches!(kind, ModuleKind::View)
        }
        fn overloads(&self, kind: ModuleKind) -> bool {
            matches!(kind, ModuleKind::Function | ModuleKind::Procedure)
        }
        /// The modifiers a column keeps: `varchar(10)` and `varchar(20)` are
        /// one function to this engine, and `f(character varying)` is what it
        /// calls both.
        fn normalize_routine_arg(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
            Ok(ColumnType::simple(&ty.base))
        }
    }

    fn module(kind: ModuleKind) -> Module {
        Module {
            kind,
            description: None,
            definition: "AS SELECT 1".to_owned(),
        }
    }

    fn schema_with(entries: &[(&str, ModuleKind)], tables: &[&str]) -> Schema {
        let mut schema = Schema::default();
        for t in tables {
            schema.tables.insert(t.parse().unwrap(), Table::default());
        }
        for (id, kind) in entries {
            schema.modules.insert(id.parse().unwrap(), module(*kind));
        }
        schema
    }

    /// Whether a module competes with a table for its name is the engine's
    /// answer: SQL Server keeps every kind in `sys.objects`, PostgreSQL keeps
    /// views in `pg_class` with tables and routines in `pg_proc`.
    #[test]
    fn a_name_a_table_holds_is_refused_only_where_the_kind_shares_its_namespace() {
        let clash = schema_with(&[("app.customer", ModuleKind::View)], &["app.customer"]);
        assert!(
            check_module_names(&clash, &MinimalDialect)[0].contains("one namespace"),
            "{:?}",
            check_module_names(&clash, &MinimalDialect)
        );
        assert!(
            check_module_names(&clash, &OverloadingDialect)[0].contains("one namespace"),
            "a view shares the table namespace on both"
        );

        // A routine of that name does not, where routines have their own.
        let routine = schema_with(
            &[("app.customer(integer)", ModuleKind::Function)],
            &["app.customer"],
        );
        assert!(
            check_module_names(&routine, &OverloadingDialect).is_empty(),
            "{:?}",
            check_module_names(&routine, &OverloadingDialect)
        );
    }

    /// Two modules can now hold one name — `ModuleId` distinguishes a trigger
    /// by its table and a routine by its signature — and on an engine that
    /// keeps them all in one namespace per schema, that is a pair of objects
    /// the engine cannot both have. The `ObjectName` key used to make this
    /// unrepresentable; nothing does now, so the check has to.
    #[test]
    fn two_modules_with_one_name_are_refused_where_they_share_a_namespace() {
        let two_triggers = schema_with(
            &[
                ("app.orders.audit", ModuleKind::Trigger),
                ("app.customers.audit", ModuleKind::Trigger),
            ],
            &[],
        );
        let problems = check_module_names(&two_triggers, &MinimalDialect);
        assert!(
            problems[0].contains("one namespace"),
            "an engine with one namespace accepted two `app.audit`: {problems:?}"
        );
        // Both identities are named, because "one of these is wrong" is not a
        // finding anyone can act on.
        assert!(problems[0].contains("app.orders.audit"), "{problems:?}");
        assert!(problems[0].contains("app.customers.audit"), "{problems:?}");

        // A trigger and a view competing for the same name is the same fault.
        let mixed = schema_with(
            &[
                ("app.orders.audit", ModuleKind::Trigger),
                ("app.audit", ModuleKind::View),
            ],
            &[],
        );
        assert!(
            check_module_names(&mixed, &MinimalDialect)[0].contains("one namespace"),
            "{:?}",
            check_module_names(&mixed, &MinimalDialect)
        );

        // Where the kinds have namespaces of their own, both pairs are two
        // real objects and neither is a finding: a trigger belongs to its
        // table, and a view is not in the trigger namespace at all.
        assert!(
            check_module_names(&two_triggers, &OverloadingDialect).is_empty(),
            "{:?}",
            check_module_names(&two_triggers, &OverloadingDialect)
        );
        assert!(
            check_module_names(&mixed, &OverloadingDialect).is_empty(),
            "{:?}",
            check_module_names(&mixed, &OverloadingDialect)
        );
    }

    /// A signature on an engine where the kind does not overload names an
    /// object that engine cannot have, and the message says what to write
    /// instead.
    /// Where the kind overloads, the engine identifies every routine by its
    /// argument types — the one taking none as `app.f()` — so a bare name is
    /// a key the read-back never carries, and the same routine would plan as
    /// a drop and a create. A view is named, on either engine.
    #[test]
    fn a_bare_name_is_refused_where_the_kind_overloads() {
        let bare = schema_with(&[("app.f", ModuleKind::Function)], &[]);
        let problems = check_module_names(&bare, &OverloadingDialect);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("`app.f()`"), "{problems:?}");
        assert!(check_module_names(&bare, &MinimalDialect).is_empty());

        let spelled = schema_with(&[("app.f()", ModuleKind::Function)], &[]);
        assert!(check_module_names(&spelled, &OverloadingDialect).is_empty());
        let view = schema_with(&[("app.v", ModuleKind::View)], &[]);
        assert!(check_module_names(&view, &OverloadingDialect).is_empty());
    }

    #[test]
    fn a_signature_is_refused_where_the_kind_does_not_overload() {
        let signed = schema_with(&[("app.f(integer)", ModuleKind::Function)], &[]);
        let problems = check_module_names(&signed, &MinimalDialect);
        assert!(problems[0].contains("does not overload"), "{problems:?}");
        assert!(problems[0].contains("`app.f`"), "{problems:?}");
        assert!(check_module_names(&signed, &OverloadingDialect).is_empty());

        // And a view is not a thing that overloads anywhere.
        let signed_view = schema_with(&[("app.v(integer)", ModuleKind::View)], &[]);
        assert!(
            check_module_names(&signed_view, &OverloadingDialect)[0].contains("does not overload"),
            "{:?}",
            check_module_names(&signed_view, &OverloadingDialect)
        );
    }

    /// Routine identity is not column identity: the modifiers `normalize_type`
    /// keeps are the ones the engine discards when it identifies a routine
    /// (ADR-0009 §1, measured). The default defers to the column spelling,
    /// which is only ever right where nothing overloads.
    #[test]
    fn routine_argument_normalization_is_not_column_normalization() {
        let ty: ColumnType = "varchar(10)".parse().unwrap();
        assert_eq!(
            OverloadingDialect.normalize_routine_arg(&ty).unwrap(),
            ColumnType::simple("varchar")
        );
        assert_eq!(OverloadingDialect.normalize_type(&ty).unwrap(), ty);
        assert_eq!(MinimalDialect.normalize_routine_arg(&ty).unwrap(), ty);
    }
}

/// The three statements that frame a transaction in a dialect
/// ([`Dialect::transaction_framing`]).
///
/// `pbps-db` owns *when* a transaction opens and closes and what a failure on
/// the way out must not hide; how each step is spelled is the dialect's. Three
/// plain strings rather than a trait, because nothing about the framing varies
/// between engines except the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionFraming {
    /// Opens the transaction — and, where the engine needs telling, makes any
    /// statement error doom it.
    pub begin: &'static str,
    /// Commits it.
    pub commit: &'static str,
    /// Rolls it back. Must succeed on a transaction the server has already
    /// aborted, or the rollback's own error replaces the statement that broke.
    pub rollback: &'static str,
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

    /// Standard SQL, which is all a dialect that is not any real database can
    /// honestly say. Nothing opens a connection with this dialect.
    fn transaction_framing(&self) -> TransactionFraming {
        TransactionFraming {
            begin: "START TRANSACTION;",
            commit: "COMMIT;",
            rollback: "ROLLBACK;",
        }
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
