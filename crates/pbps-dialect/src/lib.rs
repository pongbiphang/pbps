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
use std::collections::{BTreeMap, BTreeSet};

pub mod estimate;

use pbps_model::{
    Change, ChangeSet, ColumnType, Module, ModuleId, ModuleKind, ObjectName, RiskClass, Role,
    RoutineArg, Schema, Strategy, Table, TableName,
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

    /// A part of a dialect this build has not implemented **yet**.
    ///
    /// Distinct from [`DialectError::Unsupported`], and the distinction is the
    /// whole reason it exists: "this database has no such feature" and "pbps
    /// cannot do this here yet" send a reader to two different places, and a
    /// dialect arriving one step at a time (Phase 5) would otherwise have to
    /// tell the first lie to report the second. Returning an empty answer
    /// instead is the one thing neither may do — a plan that applies cleanly
    /// and changes nothing is the silent wrong answer.
    #[error("the {dialect} dialect does not implement {part} yet")]
    NotBuilt { dialect: &'static str, part: String },

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
/// The criterion is **whether this kind of change can fail or change stored
/// values**, not whether today's data happens to be safe — inspecting data is a
/// runtime concern and has no place in the declarative layer (SPEC §7.2). A
/// conversion that always succeeds is still not `Safe` when it rewrites what
/// is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeChangeRisk {
    /// No change, or a widening that keeps every stored value as it was
    /// (`int` → `bigint`, `varchar(50)` → `varchar(100)`).
    Safe,
    /// Narrowing or value-changing: may truncate or round (`varchar(50)` →
    /// `varchar(10)`, `time(7)` → `time(3)`), or changes values even though it
    /// cannot fail — the blank padding of `varchar(10)` → `char(100)`, SQL
    /// Server's zero bytes in `binary(8)` → `binary(16)`, a number rendered as
    /// text.
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

/// A relation an engine creates with a table under a name it generates itself
/// (#465), with how to move a declared name out of its way (#990).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplicitRelation {
    /// The name the engine generates.
    pub name: String,
    /// What it is, as a refusal names it.
    pub descriptor: String,
    /// What the author can change so a declared name stops meeting it. Naming
    /// the primary key moves only its index; an identity sequence is named
    /// after its table and column, so it moves with those.
    pub remedy: &'static str,
}

/// One executable statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub sql: String,

    /// Reference-data writes that need an engine-specific execution guard.
    /// Derived by the emitter, never inferred by parsing the SQL.
    pub row_write: Option<RowWrite>,

    /// An index this statement builds outside a transaction. The executor
    /// needs its identity to recover a failed build without parsing SQL.
    pub index_build: Option<IndexBuild>,

    /// Must be sent as a batch of its own.
    ///
    /// Some SQL Server DDL cannot share a batch with statements that reference it
    /// (adding a column and referencing it in the same batch fails to compile).
    ///
    /// PostgreSQL has one restriction of its own, and an earlier version of this
    /// comment said it had none: `CREATE INDEX CONCURRENTLY` cannot run inside a
    /// transaction block, and a multi-statement simple query *is* one — measured,
    /// `SET LOCAL search_path = s; CREATE INDEX CONCURRENTLY …` is refused where
    /// the same `CREATE` alone is accepted. So the concurrent build is alone in
    /// its batch, and it is [`Statement::non_transactional`] besides.
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

/// The reference-data operation performed by a guarded statement.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowOperation {
    Insert,
    /// Exactly the columns named in the emitted SET list.
    Update {
        columns: std::collections::BTreeSet<String>,
    },
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowWrite {
    pub table: TableName,
    pub operation: RowOperation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexBuild {
    pub table: TableName,
    pub name: String,
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
            row_write: None,
            index_build: None,
            own_batch: false,
            transactional: true,
            renames: Vec::new(),
            role_renames: Vec::new(),
            creates: Vec::new(),
        }
    }

    pub fn writing_rows(mut self, table: TableName, operation: RowOperation) -> Self {
        self.row_write = Some(RowWrite { table, operation });
        self
    }

    pub fn building_index(mut self, table: TableName, name: impl Into<String>) -> Self {
        self.index_build = Some(IndexBuild {
            table,
            name: name.into(),
        });
        self
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

/// Executable questions and deliberately unasked questions stay distinct.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preflight {
    pub probes: Vec<Probe>,
    pub unchecked: Vec<Unchecked>,
}

/// A check whose answer cannot be obtained before the plan runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unchecked {
    pub description: String,
    pub reason: String,
}

impl Unchecked {
    pub fn for_change(change: &Change, reason: impl Into<String>) -> Self {
        let description = match change {
            Change::AddCheck { table, name, .. } => format!("new check {name} on {table}"),
            Change::AddUnique { table, name, .. } => {
                format!("new unique constraint {name} on {table}")
            }
            Change::AddForeignKey { table, name, .. } => {
                format!("new foreign key {name} on {table}")
            }
            Change::AddIndex { table, name, .. } => format!("new unique index {name} on {table}"),
            Change::SetPrimaryKey { table, to, .. } => format!(
                "new primary key {} on {table}",
                to.as_ref()
                    .and_then(|key| key.name.as_deref())
                    .unwrap_or("(unnamed)")
            ),
            Change::AddColumn { table, name, .. } => {
                format!("new NOT NULL column {}", table.column(name))
            }
            Change::AlterColumnNullability { column, .. } => format!("NOT NULL column {column}"),
            Change::AlterColumnType { column, .. } => format!("type conversion of {column}"),
            Change::DeleteRow { table, key, .. } => format!("references to row {key} in {table}"),
            Change::CreateTable { .. }
            | Change::DropTable { .. }
            | Change::RenameTable { .. }
            | Change::DropColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnDefault { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::DropUnique { .. }
            | Change::DropForeignKey { .. }
            | Change::DropCheck { .. }
            | Change::DropIndex { .. }
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::SetDataMode { .. }
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            | Change::DropModule { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => change.subject(),
        };
        Self::new(description, reason)
    }

    pub fn new(description: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            reason: reason.into(),
        }
    }
}

/// What the shared definition scanner has to know about one engine's lexis.
///
/// Not a table of delimiters. Two of the three failures ADR-0011 Amendment 2
/// measured were **termination rules** — where a region ends, not where it
/// begins — and a table of delimiters alone would have fixed only the third
/// (DECISIONS 226).
///
/// What is *not* a field here is as deliberate as what is. A `'…'` string
/// closed by a doubled quote, `--` running to the end of its line, and
/// `/*…*/` that **nests** are shared because both engines were measured and
/// both answered the same — `SELECT /* a /* b */ c */ 1` returns `1` on either.
/// A field no implementation varies is a field nobody maintains, and the
/// abstraction worth having is the one two implementations draw (ADR-0014).
#[derive(Clone, Copy, Debug)]
pub struct Lexicon {
    /// Whether definition layout is limited to ASCII whitespace. PostgreSQL
    /// keeps every non-ASCII byte in an identifier; SQL Server accepts Unicode
    /// White_Space as token separators (measured in DECISIONS 475).
    pub whitespace_is_ascii: bool,

    /// Every opener of a quoted identifier, with the character that closes it.
    ///
    /// This is where the two engines part company over a character they both
    /// use: `[` is SQL Server's identifier quote and PostgreSQL's array
    /// subscript. Read as a quote where it is a subscript, a reindent inside
    /// `a[1 + 2]` reads as a changed module; read as code where it is a quote,
    /// the spacing in `[a  b]` — which is part of a name — is folded away.
    pub quoted_identifiers: &'static [(char, char)],

    /// Whether `E'…'` is an escape string, in which a backslash escapes the
    /// character after it, so `\'` does **not** close the literal.
    ///
    /// Specific to `E'…'` because `standard_conforming_strings` is `on` by
    /// default: in a plain literal the backslash is literal and the quote that
    /// follows it does close the string.
    pub escape_strings: bool,

    /// Whether `$tag$…$tag$` is a string literal — closed only by its own tag,
    /// with no escape sequences inside it at all.
    pub dollar_quoted_strings: bool,

    /// The prefixes a string literal may carry, lower-cased and longest
    /// first: part of the literal's token, not a name before it.
    ///
    /// **Measured** on PostgreSQL 18.6, `N'x'`, `B'101'`, `X'1F'`,
    /// `U&'d\0061ta'` and `E'y'` are all literals, while `note'x'` is the
    /// type `note` applied to a string — so only these spellings are blanked
    /// with the literal, and only where nothing continues an identifier
    /// before them. Left as code, the `E` of an escape string matched a
    /// module named `e` and drew an edge that was not there (DECISIONS 315).
    pub string_prefixes: &'static [&'static str],

    /// Whether a character continues an unquoted identifier once one has
    /// started — where a word ends, for every scan that looks for one.
    ///
    /// PostgreSQL's rule admits every non-ASCII byte, so a non-breaking space
    /// is a name byte and `x\u{a0}y` one alias; SQL Server's is Unicode's
    /// alphanumerics plus four symbols (DECISIONS 313, 315).
    pub identifier_continues: fn(char) -> bool,

    /// Whether a lower-cased word can **never** stand unquoted as a name on
    /// this engine — a reserved keyword, in the engine's own sense of the
    /// word, which is narrower than "keyword".
    ///
    /// The name scans read a bare word as a possible reference to a module of
    /// that name; a word the engine refuses as a bare name is not one, and
    /// reading it as one drew an edge from every view to a view named
    /// `select`. **Measured** (DECISIONS 316): on PostgreSQL `FROM select`
    /// is a syntax error while `FROM "select"`, `FROM app.select` and `FROM
    /// user` are accepted, so the word is reserved only where no bare
    /// position takes it; on SQL Server none of the documented reserved words
    /// stands unbracketed.
    pub reserved: fn(&str) -> bool,

    /// Whether `U&"…"`, with an optional `UESCAPE 'x'` after it, is a
    /// Unicode-escaped identifier — a spelling of the name it decodes to,
    /// which is what the name scans have to see. **Measured** on PostgreSQL
    /// 18.6, `SELECT * FROM dq.U&"\007a"` and `FROM U&"dq".U&"!007a" UESCAPE
    /// '!'` both select from `dq.z`; read as the spelling on the page, the
    /// scan found no `z` in either and created the view first (DECISIONS
    /// 315).
    pub unicode_identifiers: bool,
}

/// Standard SQL's identifier rule: letters, digits and `_`.
fn ansi_identifier_continues(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The answer of a lexis that reserves nothing: every bare word may be a
/// name. The shared scanner's rule, and the honest one for a dialect that has
/// not measured its engine's.
pub fn never_reserved(_: &str) -> bool {
    false
}

/// What an engine's lexis finds in a declared check or filter expression.
///
/// Three answers rather than a bool, because the third is the one a bool hides:
/// text that ends inside a block comment holds *unknown*, not *nothing*, and a
/// validator that refused it as empty would name a cause the engine disagrees
/// with (DECISIONS 504).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expression {
    /// Nothing the engine would parse: this engine's whitespace, and comments
    /// that close. Measured, the engine refuses this as a syntax error.
    Absent,
    /// At least one token — including a literal or a quoted identifier, either
    /// of which can be a whole valid expression on its own.
    Present,
    /// The text ends inside a block comment that never closes, so what it
    /// holds cannot be read. The engine has its own name for this one.
    Unreadable,
}

impl Lexicon {
    /// Standard SQL and nothing past it: `"` quotes an identifier and `'`
    /// quotes a string. The honest description for a dialect that is not any
    /// real database.
    pub const ANSI: Self = Self {
        whitespace_is_ascii: false,
        quoted_identifiers: &[('"', '"')],
        escape_strings: false,
        dollar_quoted_strings: false,
        string_prefixes: &["n"],
        identifier_continues: ansi_identifier_continues,
        reserved: never_reserved,
        unicode_identifiers: true,
    };

    /// The comparison form of a module definition, for the dialect this
    /// describes. See [`Dialect::normalize_definition`], which is this with the
    /// engine's own lexis already supplied.
    pub fn normalize_definition(&self, definition: &str) -> String {
        // What the scanner is in the middle of. Layout is only layout in
        // `Code`: inside a literal or a quoted identifier the spacing is data,
        // and after `--` the *line ending* is what stops the comment, so
        // collapsing it would splice the next line into the comment and make
        // two bodies that run differently compare equal.
        enum At {
            Code,
            /// A quoted identifier, or a plain `'…'` string. A doubled closer
            /// needs no special case: the first one closes the region and the
            /// second opens it again, and everything between them is copied
            /// either way.
            Quoted {
                close: char,
            },
            /// An `E'…'` string, in which a backslash escapes the character
            /// after it. The doubled quote *does* need a special case here:
            /// leaving and re-entering through `Code` would come back as a
            /// plain `Quoted`, and a backslash in the second half would then
            /// close the literal early.
            Escape {
                after_backslash: bool,
            },
            /// A `$tag$…$tag$` string. `open` and `len` locate the opening tag
            /// in the input, and `pushed` counts the bytes written since it, so
            /// that the opener cannot close the region it opened.
            Dollar {
                open: usize,
                len: usize,
                pushed: usize,
            },
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
        // The one thing a `char_indices` loop cannot do is consume more than
        // one character: an opening `$tag$` and a doubled quote both do.
        let mut consumed_to = 0usize;
        let text = definition.trim_matches(|ch| self.is_definition_whitespace(ch));
        let bytes = text.as_bytes();

        for (i, ch) in text.char_indices() {
            if i < consumed_to {
                continue;
            }
            match at {
                At::Quoted { close } => {
                    if ch == close {
                        at = At::Code;
                    }
                    out.push(ch);
                }
                At::Escape { after_backslash } => {
                    out.push(ch);
                    at = if after_backslash {
                        // Whatever it was, the backslash has already spoken for
                        // it — including a `\\`, whose second half must not
                        // escape the quote that may follow it.
                        At::Escape {
                            after_backslash: false,
                        }
                    } else if ch == '\\' {
                        At::Escape {
                            after_backslash: true,
                        }
                    } else if ch == '\'' {
                        if bytes.get(i + 1) == Some(&b'\'') {
                            out.push('\'');
                            consumed_to = i + 2;
                            At::Escape {
                                after_backslash: false,
                            }
                        } else {
                            At::Code
                        }
                    } else {
                        At::Escape {
                            after_backslash: false,
                        }
                    };
                }
                At::Dollar { open, len, pushed } => {
                    out.push(ch);
                    let pushed = pushed + ch.len_utf8();
                    at = if pushed >= len && out.ends_with(&text[open..open + len]) {
                        At::Code
                    } else {
                        At::Dollar { open, len, pushed }
                    };
                }
                At::Line => {
                    if matches!(ch, '\r' | '\n') {
                        // The newline is the comment's terminator, so it is
                        // structure. Canonicalize CR as LF; a following LF
                        // is ordinary Code whitespace, not another terminator.
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
                    if self.is_definition_whitespace(ch) {
                        in_space = true;
                        continue;
                    }
                    if in_space && !out.is_empty() && !out.ends_with('\n') {
                        out.push(' ');
                    }
                    in_space = false;
                    // A `$` opens a literal only when it opens a *tag*. On an
                    // engine without dollar quoting, and on one where the run
                    // of characters after the `$` is not a tag, it is ordinary
                    // code — which is what keeps SQL Server's `$` identifiers
                    // and money literals reading as code here.
                    if self.dollar_quoted_strings
                        && ch == '$'
                        && !continues_identifier(text, i)
                        && let Some(len) = dollar_tag(&text[i..])
                    {
                        out.push_str(&text[i..i + len]);
                        consumed_to = i + len;
                        at = At::Dollar {
                            open: i,
                            len,
                            pushed: 0,
                        };
                        continue;
                    }
                    let next = bytes.get(i + ch.len_utf8()).copied();
                    at = if ch == '-' && next == Some(b'-') {
                        At::Line
                    } else if ch == '/' && next == Some(b'*') {
                        At::Block { depth: 1, seen: 0 }
                    } else if ch == '\'' {
                        if self.escape_strings && opens_escape_string(text, i) {
                            At::Escape {
                                after_backslash: false,
                            }
                        } else {
                            At::Quoted { close: '\'' }
                        }
                    } else if let Some(&(_, close)) = self
                        .quoted_identifiers
                        .iter()
                        .find(|&&(open, _)| open == ch)
                    {
                        At::Quoted { close }
                    } else {
                        At::Code
                    };
                    out.push(ch);
                }
            }
        }
        out
    }

    fn is_definition_whitespace(&self, ch: char) -> bool {
        // Both measured engines accept vertical tab. is_ascii_whitespace
        // omits it, so intersect Unicode's class with ASCII instead (475).
        ch.is_whitespace() && (!self.whitespace_is_ascii || ch.is_ascii())
    }
}

impl Lexicon {
    /// Whether a declared expression holds anything this engine would parse.
    ///
    /// The question a declaration validator asks about a check constraint or
    /// an index filter. It is not `trim`: **measured** on PostgreSQL 18.6, of
    /// the Unicode White_Space characters only the six ASCII separators
    /// separate tokens — `CHECK ( )` with space, tab, LF, **vertical tab**, FF
    /// or CR between the parentheses is `syntax error at or near ")"`, while
    /// every non-ASCII member names a column (`column " " does not exist`),
    /// so a non-breaking space is a legal unquoted expression (DECISIONS 452).
    /// Rust's `trim_ascii` is the wrong set by exactly one character: its
    /// ASCII-whitespace class omits the vertical tab (issue #480).
    ///
    /// **Comments are layout too**, and they are not whitespace bytes. Measured
    /// on the same server, `CHECK (/* x */)`, `CHECK (-- x\n)`, the nesting
    /// form `/* a /* b */ c */` and any mixture of them with whitespace are
    /// each the same `syntax error at or near ")"`, for a check constraint and
    /// a partial index's `WHERE` alike (issue #482).
    ///
    /// # Why this does not lex the literals
    ///
    /// It never needs to. A literal, a quoted identifier and a dollar-quoted
    /// body are all *content*: the moment one opens, the answer is
    /// [`Expression::Present`] and the scan is over — so there is no literal
    /// to skip, and no second lexis to drift from [`code_only`]'s. That is
    /// what keeps this short enough to be obviously right, and it is why the
    /// `--` inside `'-- not a comment'` cannot be read as a comment: the
    /// scan stopped at the quote. Measured, that literal is a valid
    /// expression the engine refuses on *type* grounds
    /// (`invalid input syntax for type boolean`), not on syntax, and
    /// `CHECK ('true')` is accepted outright — so blanking literals the way
    /// [`code_only`] does would refuse a valid declaration.
    ///
    /// [`code_only`]: Lexicon::code_only
    pub fn expression_in(&self, text: &str) -> Expression {
        let bytes = text.as_bytes();
        let mut i = 0;
        while let Some(ch) = text[i..].chars().next() {
            if self.is_definition_whitespace(ch) {
                i += ch.len_utf8();
                continue;
            }
            if ch == '-' && bytes.get(i + 1) == Some(&b'-') {
                // To the end of its line, or to the end of the text where
                // there is no line left to end.
                i = text[i..].find(['\n', '\r']).map_or(text.len(), |at| i + at);
                continue;
            }
            if ch == '/' && bytes.get(i + 1) == Some(&b'*') {
                match closing_block_comment(text, i) {
                    Some(end) => i = end,
                    // Absent, empty and unreadable are three different things.
                    // Measured, the engine calls this `unterminated /* comment`
                    // rather than a missing expression, and saying "empty" here
                    // would name the wrong cause for text nobody can read.
                    None => return Expression::Unreadable,
                }
                continue;
            }
            return Expression::Present;
        }
        Expression::Absent
    }

    /// The definition with everything that is not code blanked out, by this
    /// engine's lexis.
    ///
    /// String literals and both comment forms are replaced by spaces,
    /// character for character, so line structure and offsets survive; a
    /// quoted identifier is passed through, because it is a name and a name
    /// is what the callers look for. The reader is the dependency scan
    /// (`creation_order`), and it used to lex every engine with one engine's
    /// rules: **measured**, `CREATE VIEW es.b AS SELECT E'x\' , es.a' AS s`
    /// is one literal to PostgreSQL, and the shared scanner closed it at the
    /// `\'`, read `, es.a` as code, and invented an edge from `b` to `a`.
    /// With `a` selecting from `b`, that edge closed a cycle, the two were
    /// emitted in name order, and `CREATE VIEW es.a` failed inside the plan's
    /// own transaction (DECISIONS 315).
    ///
    /// On the engine with dollar quoting, a routine's body can also be a
    /// plain single-quoted string. The body after `AS` at depth zero is lexed
    /// as code, with doubled quotes decoded first for the single-quoted form;
    /// its own literals still contain data. This is the one place this scan and
    /// [`normalize_definition`] part company on purpose.
    ///
    /// [`normalize_definition`]: Lexicon::normalize_definition
    pub fn code_only(&self, definition: &str) -> String {
        // LANGUAGE may follow AS. Inspect the header with every string blanked
        // first, so body text cannot masquerade as a language clause. Native
        // routines store library/symbol names in AS, not SQL source to scan.
        let native = self.dollar_quoted_strings
            && self.has_native_language(definition, &self.code_only_inner(definition, false));
        self.code_only_inner(definition, !native)
    }

    fn has_native_language(&self, definition: &str, header: &str) -> bool {
        let mut cursor = 0;
        let mut depth = 0usize;
        while cursor < header.len() {
            let rest = &header[cursor..];
            let ch = rest.chars().next().unwrap();
            if ch == '"' {
                cursor += quoted_identifier_len(rest).unwrap_or(rest.len());
            } else if (self.identifier_continues)(ch) {
                let len = rest
                    .find(|c| !(self.identifier_continues)(c))
                    .unwrap_or(rest.len());
                cursor += len;
                if depth == 0 && rest[..len].eq_ignore_ascii_case("language") {
                    let name = after_string_gap(&definition[cursor..]).0;
                    let name = if let Some(literal) = sql_string(name, 0) {
                        literal.contents
                    } else {
                        // The header already decodes Unicode identifiers and
                        // pads their span, so this offset also covers U&"…".
                        let name = &header[definition.len() - name.len()..];
                        if name.starts_with('"') {
                            quoted_identifier_len(name)
                                .map(|end| name[1..end - 1].replace("\"\"", "\""))
                                .unwrap_or_default()
                        } else {
                            let end = name
                                .find(|c| !(self.identifier_continues)(c))
                                .unwrap_or(name.len());
                            name[..end].to_ascii_lowercase()
                        }
                    };
                    if matches!(name.as_str(), "c" | "internal") {
                        return true;
                    }
                }
            } else {
                match ch {
                    '(' => depth += 1,
                    ')' => depth = depth.saturating_sub(1),
                    _ => {}
                }
                cursor += ch.len_utf8();
            }
        }
        false
    }

    fn code_only_inner(&self, definition: &str, scan_bodies: bool) -> String {
        enum At {
            Code,
            /// Inside `'…'`: a doubled quote is a quote *inside* the
            /// literal, and the literal it resumes is the same one.
            /// `unicode` marks a `U&'…'`, which may carry a `UESCAPE 'x'`
            /// after its closing quote — and which a doubled quote must not
            /// cost it: measured, `U&'a''b' uescape '!'` is the string
            /// `a'b`, clause and all.
            Literal {
                unicode: bool,
            },
            /// Inside `E'…'`: a backslash speaks for the character after it.
            Escape {
                after_backslash: bool,
            },
            /// Inside a quoted identifier: kept. The flag marks the second
            /// delimiter of an escaped pair so it cannot also close the name.
            Ident {
                close: char,
                escaped: bool,
            },
            Line,
            Block {
                depth: usize,
                seen: usize,
            },
        }
        let mut out = String::with_capacity(definition.len());
        let bytes = definition.as_bytes();
        let mut at = At::Code;
        let mut consumed_to = 0usize;
        let mut paren_depth = 0usize;
        for (i, ch) in definition.char_indices() {
            if i < consumed_to {
                continue;
            }
            let next = bytes.get(i + ch.len_utf8()).copied();
            match at {
                At::Literal { unicode } => {
                    blank(&mut out, ch);
                    if ch == '\'' && next == Some(b'\'') {
                        // A quote inside the literal, not the end of it: the
                        // prefix it opened with is still the prefix, and its
                        // clause still follows the real closing quote.
                        blank(&mut out, '\'');
                        consumed_to = i + 2;
                    } else if ch == '\'' {
                        at = At::Code;
                        // The clause belongs to the literal it follows —
                        // measured, `U&'d!0061ta' UESCAPE '!'` is the string
                        // `data` — and left as code the word matched a module
                        // named `uescape` and drew an edge that was not there.
                        if unicode && let Some(n) = uescape_clause_len(&definition[i + 1..]) {
                            for c in definition[i + 1..i + 1 + n].chars() {
                                blank(&mut out, c);
                            }
                            consumed_to = i + 1 + n;
                        }
                    }
                }
                At::Escape { after_backslash } => {
                    at = if after_backslash {
                        At::Escape {
                            after_backslash: false,
                        }
                    } else if ch == '\\' {
                        At::Escape {
                            after_backslash: true,
                        }
                    } else if ch == '\'' {
                        if next == Some(b'\'') {
                            blank(&mut out, '\'');
                            consumed_to = i + 2;
                            At::Escape {
                                after_backslash: false,
                            }
                        } else {
                            At::Code
                        }
                    } else {
                        At::Escape {
                            after_backslash: false,
                        }
                    };
                    blank(&mut out, ch);
                }
                At::Ident { close, escaped } => {
                    at = if escaped {
                        At::Ident {
                            close,
                            escaped: false,
                        }
                    } else if ch == close {
                        if next == Some(close as u8) {
                            At::Ident {
                                close,
                                escaped: true,
                            }
                        } else {
                            At::Code
                        }
                    } else {
                        At::Ident {
                            close,
                            escaped: false,
                        }
                    };
                    out.push(ch);
                }
                At::Line => {
                    if matches!(ch, '\n' | '\r') {
                        at = At::Code;
                    }
                    blank(&mut out, ch);
                }
                At::Block { depth, seen } => {
                    at = if ch == '/' && seen >= 2 && bytes[i - 1] == b'*' {
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
                    blank(&mut out, ch);
                }
                At::Code => {
                    // A `$` opens a string only where it opens a tag: `a$b$c`
                    // is one name, and `$1.00` is code (see `dollar_tag`).
                    if self.dollar_quoted_strings
                        && ch == '$'
                        && !continues_identifier(definition, i)
                        && let Some(len) = dollar_tag(&definition[i..])
                    {
                        let body = scan_bodies
                            && paren_depth == 0
                            && follows_the_word_as(&out, self.identifier_continues);
                        consumed_to = self.dollar_quoted_string(definition, i, len, body, &mut out);
                        continue;
                    }
                    match (ch, next) {
                        ('-', Some(b'-')) => {
                            at = At::Line;
                            blank(&mut out, ch);
                        }
                        ('/', Some(b'*')) => {
                            at = At::Block { depth: 1, seen: 0 };
                            blank(&mut out, ch);
                        }
                        ('\'', _) => {
                            let prefix = self.blank_string_prefix(&mut out);
                            if scan_bodies
                                && self.dollar_quoted_strings
                                && prefix.is_none_or(|p| {
                                    p.eq_ignore_ascii_case("e") || p.eq_ignore_ascii_case("u&")
                                })
                                && paren_depth == 0
                                && follows_the_word_as(&out, self.identifier_continues)
                                && let Some(end) = self.quoted_body(
                                    definition,
                                    i,
                                    prefix.map_or(0, str::len),
                                    &mut out,
                                )
                            {
                                consumed_to = end;
                                continue;
                            }
                            if self.dollar_quoted_strings
                                && let Some(literal) =
                                    sql_string(definition, i - prefix.map_or(0, str::len))
                            {
                                // A datum carries its continuations and
                                // UESCAPE clause too; none of it is source.
                                for ch in definition[i..literal.end].chars() {
                                    blank(&mut out, ch);
                                }
                                consumed_to = literal.end;
                                continue;
                            }
                            at = if self.escape_strings && opens_escape_string(definition, i) {
                                At::Escape {
                                    after_backslash: false,
                                }
                            } else {
                                At::Literal {
                                    unicode: prefix.is_some_and(|p| p.eq_ignore_ascii_case("u&")),
                                }
                            };
                            blank(&mut out, ch);
                        }
                        ('(', _) => {
                            paren_depth += 1;
                            out.push(ch);
                        }
                        (')', _) => {
                            paren_depth = paren_depth.saturating_sub(1);
                            out.push(ch);
                        }
                        _ => {
                            if ch == '"'
                                && self.unicode_identifiers
                                && let Some(end) =
                                    self.decode_unicode_identifier(definition, i, &mut out)
                            {
                                consumed_to = end;
                                continue;
                            }
                            if let Some(&(_, close)) = self
                                .quoted_identifiers
                                .iter()
                                .find(|&&(open, _)| open == ch)
                            {
                                at = At::Ident {
                                    close,
                                    escaped: false,
                                };
                            }
                            out.push(ch);
                        }
                    }
                }
            }
        }
        out
    }

    /// Where the quote at `at` closes a `U&"…"` — the `U&` already in `out`
    /// as code, as a string prefix would be — replaces the spelling with the
    /// quoted name it decodes to and returns the offset the code after it,
    /// and after its `UESCAPE 'x'` if it has one, resumes at. `None` where
    /// the quote is not that, or the text does not decode: the quote then
    /// opens an ordinary identifier, spelled as it is.
    ///
    /// The decoded name is never longer than its spelling, so the difference
    /// is padded and offsets survive; a line break inside the span is kept.
    fn decode_unicode_identifier(
        &self,
        definition: &str,
        at: usize,
        out: &mut String,
    ) -> Option<usize> {
        let n = out.len();
        if n < 2
            || !out.is_char_boundary(n - 2)
            || !out[n - 2..].eq_ignore_ascii_case("u&")
            || out[..n - 2]
                .chars()
                .next_back()
                .is_some_and(self.identifier_continues)
        {
            return None;
        }
        let close = quoted_identifier_len(&definition[at..])?;
        let inner = definition[at + 1..at + close - 1].replace("\"\"", "\"");
        let (escape, end) = unicode_escape_clause(definition, at + close)?;
        let decoded = pbps_model::module::decode_unicode_escapes(&inner, escape)?;
        out.truncate(n - 2);
        let spelled = format!("\"{}\"", decoded.replace('"', "\"\""));
        let span = end - at + 2;
        out.push_str(&spelled);
        let breaks = definition[at..end].matches('\n').count();
        for _ in spelled.len() + breaks..span {
            out.push(' ');
        }
        for _ in 0..breaks {
            out.push('\n');
        }
        Some(end)
    }

    /// Decode the SQL string before scanning the source it contains. Padding
    /// retains subsequent clause offsets without splitting names in the body.
    fn quoted_body(
        &self,
        definition: &str,
        at: usize,
        prefix_len: usize,
        out: &mut String,
    ) -> Option<usize> {
        let literal = sql_string(definition, at - prefix_len)?;
        blank(out, '\'');
        out.push_str(&self.code_only(&literal.contents));
        for _ in out.len() + literal.breaks.len()..literal.end {
            out.push(' ');
        }
        out.push_str(&literal.breaks);
        Some(literal.end)
    }

    /// Reads the dollar-quoted string that opens at `at` with a tag of `len`
    /// bytes, and returns the offset the code after it resumes at.
    ///
    /// A routine's body is a dollar-quoted string on this engine, and the
    /// name scans exist to read what the body says — blanked, every routine
    /// would call nothing and depend on nothing. A dollar-quoted *datum* in a
    /// view is a literal like any other, and a name inside one drew an edge
    /// to a view that the datum never depends on; with the other direction
    /// real, that edge closed a cycle and the dependent was created first
    /// (DECISIONS 315). The two are told apart by what precedes the string:
    /// a body follows the word `AS`, and a datum never does — measured, `AS
    /// $x$` where a view's alias would go is a syntax error, so a
    /// dollar-quoted string after `AS` is a body unless LANGUAGE identifies
    /// a native library/symbol (checked before this scan). The body is lexed
    /// as code by the same rules: its own
    /// literals and comments are blanked, an `E'…'` by the escape rule, and a
    /// dollar-quoted datum inside it by this one. Its tags are blanked too:
    /// a delimiter is not a name, and one that read as code matched a module
    /// named `$a$`.
    fn dollar_quoted_string(
        &self,
        definition: &str,
        at: usize,
        len: usize,
        body: bool,
        out: &mut String,
    ) -> usize {
        let tag = &definition[at..at + len];
        let inner_start = at + len;
        let (inner_end, end) = match definition[inner_start..].find(tag) {
            Some(j) => (inner_start + j, inner_start + j + len),
            // Nothing closes it. The engine would refuse the definition; the
            // scan reads it the way the engine's lexer would have, to its end.
            None => (definition.len(), definition.len()),
        };
        if body {
            // The tags are delimiters, not code: `$a$` is never a name, and
            // kept, it matched a module named `$a$` and drew an edge from
            // every routine delimited by it.
            for ch in tag.chars() {
                blank(out, ch);
            }
            out.push_str(&self.code_only(&definition[inner_start..inner_end]));
            for ch in definition[inner_end..end].chars() {
                blank(out, ch);
            }
        } else {
            for ch in definition[at..end].chars() {
                blank(out, ch);
            }
        }
        end
    }
}

struct SqlString {
    contents: String,
    end: usize,
    breaks: String,
}

/// PostgreSQL's Sconst forms, shared by routine bodies and language names.
/// B/X/N prefixes are not accepted in those grammar positions.
fn sql_string(definition: &str, at: usize) -> Option<SqlString> {
    let text = &definition[at..];
    if text.starts_with('$')
        && let Some(len) = dollar_tag(text)
    {
        let tag = &text[..len];
        let close = len + text[len..].find(tag)?;
        return Some(SqlString {
            contents: text[len..close].to_owned(),
            end: at + close + len,
            breaks: String::new(),
        });
    }
    let escape = text.starts_with("E'") || text.starts_with("e'");
    let unicode = text.starts_with("U&'") || text.starts_with("u&'");
    let prefix = if escape {
        1
    } else if unicode {
        2
    } else {
        0
    };
    if !text[prefix..].starts_with('\'') {
        return None;
    }
    let mut contents = Vec::new();
    let mut high_surrogate = None;
    let mut breaks = String::new();
    let mut cursor = at + prefix + 1;
    loop {
        let mut piece = String::new();
        loop {
            let ch = definition[cursor..].chars().next()?;
            cursor += ch.len_utf8();
            if ch == '\'' {
                if definition[cursor..].starts_with('\'') {
                    piece.push('\'');
                    cursor += 1;
                } else {
                    break;
                }
            } else {
                piece.push(ch);
                if escape && ch == '\\' {
                    // An escaped quote is data, not this piece's terminator.
                    let escaped = definition[cursor..].chars().next()?;
                    piece.push(escaped);
                    cursor += escaped.len_utf8();
                }
            }
        }
        if escape {
            // E escapes are read within each piece: E'\x' ⏎ '63' is x63,
            // not c. The prefix still applies to every continued piece.
            escape_piece(&piece, &mut contents, &mut high_surrogate)?;
        } else {
            contents.extend_from_slice(piece.as_bytes());
        }
        let (after, continues) = after_string_gap(&definition[cursor..]);
        if !continues || !after.starts_with('\'') {
            break;
        }
        let next = definition.len() - after.len();
        breaks.extend(
            definition[cursor..next]
                .chars()
                .filter(|c| matches!(c, '\r' | '\n')),
        );
        cursor = next + 1;
    }
    if high_surrogate.is_some() {
        return None;
    }
    let mut contents = String::from_utf8(contents).ok()?;
    if unicode {
        let (escape, end) = unicode_escape_clause(definition, cursor)?;
        breaks.extend(
            definition[cursor..end]
                .chars()
                .filter(|c| matches!(c, '\r' | '\n')),
        );
        cursor = end;
        contents = pbps_model::module::decode_unicode_escapes(&contents, escape)?;
    }
    if contents.contains('\0') {
        return None;
    }
    Some(SqlString {
        contents,
        end: cursor,
        breaks,
    })
}

/// Byte escapes may form one UTF-8 character across continued pieces; decode
/// to bytes first and validate UTF-8 only after the complete constant is read.
fn escape_piece(piece: &str, out: &mut Vec<u8>, high: &mut Option<u32>) -> Option<()> {
    let bytes = piece.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        cursor += 1;
        if byte != b'\\' {
            if high.is_some() {
                return None;
            }
            out.push(byte);
            continue;
        }
        let escaped = *bytes.get(cursor)?;
        cursor += 1;
        if matches!(escaped, b'u' | b'U') {
            let digits = if escaped == b'u' { 4 } else { 8 };
            let mut code = 0u32;
            for _ in 0..digits {
                code = code
                    .checked_mul(16)?
                    .checked_add((*bytes.get(cursor)? as char).to_digit(16)?)?;
                cursor += 1;
            }
            let code = match (high.take(), code) {
                (None, 0xD800..=0xDBFF) => {
                    *high = Some(code);
                    continue;
                }
                (Some(h), 0xDC00..=0xDFFF) => 0x10000 + ((h - 0xD800) << 10) + (code - 0xDC00),
                (None, code) => code,
                (Some(_), _) => return None,
            };
            let mut encoded = [0; 4];
            out.extend_from_slice(char::from_u32(code)?.encode_utf8(&mut encoded).as_bytes());
            continue;
        }
        if high.is_some() {
            return None;
        }
        let value = if escaped == b'x' || matches!(escaped, b'0'..=b'7') {
            let (radix, limit, mut value, mut digits) = if escaped == b'x' {
                (16, 2, 0u32, 0)
            } else {
                (8, 3, u32::from(escaped - b'0'), 1)
            };
            while digits < limit {
                let Some(digit) = bytes.get(cursor).and_then(|b| (*b as char).to_digit(radix))
                else {
                    break;
                };
                value = value * radix + digit;
                cursor += 1;
                digits += 1;
            }
            if digits == 0 { escaped } else { value as u8 }
        } else {
            match escaped {
                b'b' => 8,
                b'f' => 12,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                other => other,
            }
        };
        out.push(value);
    }
    Some(())
}

/// Past PostgreSQL's whitespace and comments following one piece of a string
/// constant, and whether what they separate can still be a *continuation* of
/// it.
///
/// **Measured**, and neither half is the obvious one:
///
/// ```text
/// '01/02/' -- c ⏎ '2026'     -> 01/02/2026     a line comment is part of the
/// '01/02/' -- /* x ⏎ '2026'  -> 01/02/2026     gap, and the newline that ends
///                                              it is the newline a
///                                              continuation needs
/// '01/02/' /* c */ ⏎ '2026'  -> syntax error   a block comment ends the
/// '01/02/' ⏎ /* c */ '2026'  -> syntax error   possibility of a continuation,
/// '01/02/' /* -- x ⏎ */ '2026' -> syntax error wherever the newline stands
/// '01/02/2026' -- c          -> 01/02/2026     after the last piece either
/// '01/02/2026' /* c */       -> 01/02/2026     comment is only trailing text
/// ```
///
/// So the two comment forms are not interchangeable here, which is why they
/// are scanned rather than skipped together: the engine's `{whitespace}` rule
/// counts a `--` comment among the things a continuation may be written
/// across, and does not count a `/* … */` one (DECISIONS 278).
pub fn after_string_gap(mut text: &str) -> (&str, bool) {
    let mut newline = false;
    let mut blocked = false;
    loop {
        let trimmed = text.trim_start_matches(|c: char| c.is_ascii_whitespace());
        newline |= text[..text.len() - trimmed.len()].contains(['\r', '\n']);
        text = trimmed;
        if let Some(comment) = text.strip_prefix("--") {
            let Some(end) = comment.find(['\r', '\n']) else {
                return ("", newline);
            };
            newline = true;
            text = &comment[end + 1..];
        } else if let Some(comment) = text.strip_prefix("/*") {
            let mut depth = 1usize;
            let mut cursor = 0;
            while cursor < comment.len() && depth > 0 {
                if comment[cursor..].starts_with("/*") {
                    depth += 1;
                    cursor += 2;
                } else if comment[cursor..].starts_with("*/") {
                    depth -= 1;
                    cursor += 2;
                } else {
                    cursor += comment[cursor..].chars().next().unwrap().len_utf8();
                }
            }
            if depth != 0 {
                return (text, false);
            }
            blocked = true;
            text = &comment[cursor..];
        } else {
            return (text, newline && !blocked);
        }
    }
}

/// The length of the `"…"` at the front of `text`, a doubled quote being a
/// quote inside the name; `None` where it never closes.
fn quoted_identifier_len(text: &str) -> Option<usize> {
    let mut at = 1;
    loop {
        let close = at + text[at..].find('"')?;
        at = close + 1;
        if text[at..].starts_with('"') {
            at += 1;
        } else {
            return Some(at);
        }
    }
}

/// Where the block comment opening at `at` closes, or `None` where it never
/// does.
///
/// Byte-wise, and safely so: `/` and `*` are ASCII, and a UTF-8 continuation
/// byte is never an ASCII byte, so no multi-byte character can contain a false
/// opener or closer. They **nest** on both measured engines, which is why this
/// counts depth rather than looking for the first `*/`
/// (see [`Lexicon`]'s own note).
fn closing_block_comment(text: &str, at: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = at;
    while i + 1 < bytes.len() {
        match (bytes[i], bytes[i + 1]) {
            (b'/', b'*') => {
                depth += 1;
                i += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Blanks `ch` in `out`, keeping a line break so that line structure and
/// positions survive.
fn blank(out: &mut String, ch: char) {
    if matches!(ch, '\n' | '\r') {
        out.push(ch);
    } else {
        for _ in 0..ch.len_utf8() {
            out.push(' ');
        }
    }
}

/// Whether the code lexed so far ends with the keyword `AS` as a word of its
/// own — `has` does not end with it, and neither does `x$as`, nor
/// `as\u{a0}`: the gap before the string is whitespace the dialect does
/// not count as a name byte, and a Unicode trim took the non-breaking space
/// off a type named `as\u{a0}` and read a datum it was applied to as a body.
/// Measured, `SELECT as\u{a0} $$app.a$$` is that type applied to a string,
/// and a valid view.
///
/// It is asked of the *lexed* text, so a comment between the keyword and the
/// string it introduces is already a run of blanks: measured, `AS /* c */ $$
/// SELECT 1 $$` is accepted as a body.
fn follows_the_word_as(code: &str, continues: fn(char) -> bool) -> bool {
    let code = code.trim_end_matches(|c: char| c.is_whitespace() && !continues(c));
    let Some(start) = code.len().checked_sub(2) else {
        return false;
    };
    code.is_char_boundary(start)
        && code[start..].eq_ignore_ascii_case("as")
        && !code[..start].chars().next_back().is_some_and(continues)
}

impl Lexicon {
    /// Blanks a string prefix that `out` ends with, where it is one: a
    /// prefix is part of the literal's token, and a name before a literal is
    /// something else — measured, `note'x'` is the type `note` applied to a
    /// string, and `áE'a\'` that name applied to `a\` (see
    /// [`continues_ident`]).
    fn blank_string_prefix(&self, out: &mut String) -> Option<&'static str> {
        for prefix in self.string_prefixes {
            let n = prefix.len();
            if out.len() < n || !out.is_char_boundary(out.len() - n) {
                continue;
            }
            let start = out.len() - n;
            if out[start..].eq_ignore_ascii_case(prefix)
                && !out[..start]
                    .chars()
                    .next_back()
                    .is_some_and(self.identifier_continues)
            {
                out.truncate(start);
                for _ in 0..n {
                    out.push(' ');
                }
                return Some(prefix);
            }
        }
        None
    }
}

/// The length in bytes of the `UESCAPE 'x'` clause at the front of `text`,
/// leading whitespace included, or `None` where there is not one.
///
/// The clause is part of the token it follows — a `U&'…'` literal or a
/// `U&"…"` name — and the escape may be almost any punctuation: measured, the
/// engine takes `,`, `(`, `)`, `[`, `]` and `.`, and refuses a quote, a `+`, a
/// hex digit and whitespace (DECISIONS 301). What the escape *is* does not
/// matter to a literal whose contents are blanked either way; that the clause
/// is not code does.
fn uescape_clause_len(text: &str) -> Option<usize> {
    let (_, end) = unicode_escape_clause(text, 0)?;
    (end > 0).then_some(end)
}

/// Both Unicode identifiers and strings carry this clause. Its character is
/// itself an SQL string, so E'!' and dollar quoting must use the same decoder.
fn unicode_escape_clause(definition: &str, at: usize) -> Option<(char, usize)> {
    let after = after_string_gap(&definition[at..]).0;
    if !after
        .get(..7)
        .is_some_and(|word| word.eq_ignore_ascii_case("uescape"))
        || after[7..].chars().next().is_some_and(continues_ident)
    {
        return Some(('\\', at));
    }
    let operand = after_string_gap(&after[7..]).0;
    let clause = sql_string(definition, definition.len() - operand.len())?;
    let mut chars = clause.contents.chars();
    let escape = chars.next()?;
    if chars.next().is_some()
        || escape.is_ascii_hexdigit()
        || escape.is_whitespace()
        || matches!(escape, '+' | '\'' | '"')
    {
        return None;
    }
    Some((escape, clause.end))
}

/// The length in bytes of the `$tag$` that `s` opens with, if it opens with one.
///
/// The tag follows the rules of an unquoted identifier and may not contain a
/// `$`, so `$1.00` and a SQL Server identifier like `total$` are not tags —
/// which is why this is a question and not an assumption.
fn dollar_tag(s: &str) -> Option<usize> {
    for (j, c) in s[1..].char_indices() {
        if c == '$' {
            return Some(j + 2);
        }
        // The tag follows the identifier grammar, minus the `$` that ends it
        // and minus a digit in first place: `dolq_start [A-Za-z\200-\377_]`,
        // `dolq_cont` the same plus the digits.
        if !continues_ident(c) || (j == 0 && c.is_ascii_digit()) {
            return None;
        }
    }
    None
}

/// Whether `c` is a character PostgreSQL counts as part of an unquoted
/// identifier once one has started.
///
/// The engine's grammar is over **bytes**, not Unicode classes (DECISIONS 233):
/// `ident_cont [A-Za-z\200-\377_0-9\$]`. Every byte of a non-ASCII character is
/// ≥ 0x80, so "not ASCII" is the whole of that half. `char::is_alphanumeric` is
/// a different set and a smaller one, and the difference is not exotic: `á`
/// spelled `a` then U+0301 continues an identifier for the engine and ends one
/// for Rust. Measured on 18.6 — `á$tag$` is one name, and `áE'a\'` is that name
/// applied to the two-character string `a\`, not an escape string.
///
/// The rule is PostgreSQL's, and every caller is behind a PostgreSQL-only flag:
/// SQL Server has neither dollar quoting nor escape strings, so its scan never
/// asks the question.
///
/// Public because `pbps-pg` asks it too — its emitter has to know where a
/// dollar-quoted literal ends before it can say whether a default is one — and
/// a second spelling of this rule is how the difference above gets rediscovered
/// (PITFALLS, "one rule, spelled in three places").
pub fn continues_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// Whether the character at `at` continues an identifier instead of starting a
/// token of its own.
///
/// `$` is a legal identifier character in both engines, so `a$b$c` is one name
/// and not a name followed by a dollar-quoted string. Without this the `$b$` in
/// the middle would open a literal that the rest of the definition never
/// closes.
fn continues_identifier(text: &str, at: usize) -> bool {
    text[..at].chars().next_back().is_some_and(continues_ident)
}

/// Whether the quote at `at` is the one in an `E'…'`.
///
/// The `E` has to be a token of its own: `note'x'` is an identifier followed by
/// a plain string, and reading its final `e` as the escape prefix would arm the
/// backslash rule over a literal that has no such rule.
fn opens_escape_string(text: &str, at: usize) -> bool {
    let before = &text[..at];
    let mut chars = before.chars().rev();
    match chars.next() {
        Some('e' | 'E') => !chars.next().is_some_and(continues_ident),
        Some(_) | None => false,
    }
}

/// Managed dependents an engine requires rebuilt around a column type change.
/// The differ turns these facts into ordinary changes before ordering and risk
/// classification. Expressions are opaque, so CHECKs and filtered indexes are
/// conservatively selected at table scope; named column lists are exact.
#[derive(Debug, Clone, Copy, Default)]
pub struct RetypeDependents {
    pub keys_and_indexes: bool,
    pub checks: bool,
    pub filtered_indexes: bool,
    pub foreign_keys: bool,
}

/// Dialect knowledge that needs no database connection.
pub trait Dialect {
    fn name(&self) -> &'static str;

    /// Dependencies that cannot remain standing while this type changes.
    /// Engines that perform their own dependency maintenance need no extra
    /// changes. Defaults remain part of the column and are the emitter's work.
    fn retype_dependents(&self, _from: &ColumnType, _to: &ColumnType) -> RetypeDependents {
        RetypeDependents::default()
    }

    /// Expands aliases and fills in omitted default arguments, so that two
    /// semantically identical spellings become the same value.
    ///
    /// This step is a precondition for diff being correct: unless `INTEGER` and
    /// `int` converge on one value first, every run reports a type change.
    ///
    /// # The contract (ADR-0011 Amendment 3)
    ///
    /// Normalization is **idempotent**, and its output is **what introspection
    /// reads back for a column declared that way**. A spelling for which that is
    /// impossible is an error, not something to normalize.
    ///
    /// The second half is the one that had to be written down. PostgreSQL's
    /// `serial` is the proof: it is a macro, not a type — the column is created
    /// as `integer` with an owned sequence, and `integer` is what comes back.
    /// Normalized to anything at all it produces a schema that differs from
    /// itself on every single run, which is the permanent phantom change. The
    /// contract makes that a refusal instead, and the refusal names what to
    /// declare in its place (DECISIONS 227).
    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError>;

    /// Judges how safe a type change is. The caller is responsible for calling
    /// [`normalize_type`](Dialect::normalize_type) first — **and the
    /// implementation must not depend on it having happened.**
    ///
    /// The second half is not belt and braces. One caller cannot honour the
    /// first: `validate_saved_plan` re-derives a plan file's risks *because the
    /// file may have been edited*, so the types it passes are spelled however
    /// the editor liked. Unnormalized, an alias reads as a change between two
    /// unrelated types and blocks a plan that changes nothing, and an omitted
    /// argument reads as the wrong type and can let a narrowing past the gate.
    /// Both dialects therefore normalize again here, which costs nothing.
    ///
    /// A type that does not normalize ends the question: the answer is
    /// [`TypeChangeRisk::Incompatible`], not the classification of the declared
    /// value. Carrying it on looks conservative and is not — a rejected
    /// *modifier* keeps a base every family claims, so the family reads the
    /// modifier as one the engine would accept and can call it `Safe`.
    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk;

    /// Derives the complete risk set for planning and saved-plan verification.
    /// Defaults use this engine's identifier boundary; persisted risks are
    /// checked against this same answer so artifact validation cannot disagree.
    fn change_risks(&self, change: &Change) -> std::collections::BTreeSet<RiskClass> {
        let mut risks = change.intrinsic_risks_with(self.lexicon().identifier_continues);
        if let Change::AlterColumnType { from, to, .. } = change
            && let Some(risk) = self.type_change_risk(from, to).risk_class()
        {
            risks.insert(risk);
        }
        risks
    }

    /// The canonical form of an unquoted identifier in this dialect.
    ///
    /// PostgreSQL folds to lowercase; SQL Server keeps it as written. Name
    /// comparison must go through this, or names read back by introspection will
    /// not line up with the declarations and drift detection will cry wolf daily.
    fn fold_ident<'a>(&self, ident: &'a str) -> Cow<'a, str>;

    /// Quotes an identifier for embedding in SQL.
    fn quote_ident(&self, ident: &str) -> Result<String, DialectError>;

    /// This engine's lexis, for the definition scanner (ADR-0011 Amendment 2).
    ///
    /// Required, and deliberately so. [`normalize_definition`] used to carry a
    /// default that was really one engine's scanner wearing a neutral name: it
    /// opened a quoted region on `[`, which is SQL Server's identifier quote
    /// and PostgreSQL's array subscript, and it knew neither `E'…'` nor
    /// `$tag$…$tag$`. Two of the three failures that caused were **silent** —
    /// two definitions returning different strings compared equal, so the
    /// change was never planned at all. A dialect can no longer get
    /// normalization without saying what its literals are.
    ///
    /// [`normalize_definition`]: Dialect::normalize_definition
    fn lexicon(&self) -> Lexicon;

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
        self.lexicon().normalize_definition(definition)
    }

    /// Whether a module read back after a write holds what that write promised.
    /// Engines preserving the body can compare it; deparsed text is outside
    /// SPEC §7.6's promise and cannot be predicted by a lexical normalizer.
    /// Unchanged modules still compare two catalog reads exactly.
    fn module_matches_declaration(&self, wrote: &Module, now: &Module) -> bool {
        wrote == now
    }

    /// The definition with everything that is not code blanked out, by this
    /// engine's lexis: the text the dependency scan reads (DECISIONS 315).
    fn code_only(&self, definition: &str) -> String {
        self.lexicon().code_only(definition)
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

    /// Whether this engine keeps an index — and the index that backs an
    /// explicitly named primary key or unique constraint — in the schema's
    /// **relation** namespace, alongside tables, views and sequences, so that
    /// two tables in one schema may not declare an index of one name, and an
    /// index may not be named after a table.
    ///
    /// Measured on PostgreSQL 18.6 (issue #176):
    ///
    /// ```text
    /// CREATE TABLE r2.t1 (n int); CREATE TABLE r2.t2 (n int);
    /// CREATE INDEX ix_n ON r2.t1 (n);
    /// CREATE INDEX ix_n ON r2.t2 (n);        -> ERROR: relation "ix_n" already exists
    ///
    /// CREATE TABLE coll.t1 (n int, CONSTRAINT ix_n PRIMARY KEY (n));
    /// CREATE INDEX ix_n ON coll.t2 (n);      -> ERROR: relation "ix_n" already exists
    ///
    /// ALTER TABLE coll.t1 ADD CONSTRAINT t2 UNIQUE (n);
    ///                                         -> ERROR: relation "t2" already exists
    /// ```
    ///
    /// A primary key or unique constraint is in this question because a named
    /// one is backed by an index of that name; a check or foreign-key
    /// constraint is not backed by an index and stays out of it — those are
    /// per-table, which is issue #179's subject.
    ///
    /// This is the second place the two engines' namespaces differ (the first
    /// is routines against tables, ADR-0009 §1): on SQL Server an index name
    /// only has to be unique per table, so the same declaration is valid
    /// there — which is what the default, `false`, says.
    fn indexes_share_namespace_with_tables(&self) -> bool {
        false
    }

    /// Whether this engine keeps constraint names in one namespace per schema
    /// with tables, views, routines and triggers, so that two tables may not
    /// declare constraints of one name (issue #496).
    ///
    /// SQL Server does: every primary key, unique, check, foreign-key and
    /// default constraint is a row in `sys.objects`, whose names are unique
    /// per schema. Measured on 17.0.4075.5, each of these is refused (Msg 1750
    /// after Msg 2714, or 2714 alone for a table):
    ///
    /// ```text
    /// CREATE TABLE app.a (id int CONSTRAINT c CHECK (id > 0));
    /// CREATE TABLE app.b (id int CONSTRAINT c CHECK (id > 0));        -- check vs check
    /// CREATE TABLE app.b (pid int CONSTRAINT c REFERENCES app.p(id)); -- foreign key vs check
    /// CREATE TABLE app.b (id int CONSTRAINT t PRIMARY KEY);           -- beside a table `app.t`
    /// CREATE TABLE app.b (id int CONSTRAINT v UNIQUE);                -- beside a view `app.v`
    /// CREATE TABLE app.b (id int CONSTRAINT p CHECK (id > 0));        -- beside a procedure, a
    ///                                                                  -- function or a trigger
    /// ```
    ///
    /// while `CREATE INDEX c ON app.b (id)` beside that check, and the same
    /// check name in another schema, are both accepted. That is the default,
    /// `true`. PostgreSQL keeps a check or foreign-key name per table and puts
    /// only the index behind a key in its relation namespace, which
    /// [`Dialect::indexes_share_namespace_with_tables`] already covers.
    fn constraints_share_namespace_with_tables(&self) -> bool {
        true
    }

    /// The relations this engine creates with `table` under names it
    /// generates itself, as `(name, what it is)`: no declaration names them,
    /// yet each takes a name in the schema's relation namespace (#465,
    /// DEC-465.1).
    ///
    /// Asked by [`check_index_names`] only where
    /// [`Dialect::indexes_share_namespace_with_tables`] says the namespace is
    /// shared; the default, none, is right for an engine where it is not.
    fn implicit_relation_names(&self, _name: &TableName, _table: &Table) -> Vec<ImplicitRelation> {
        Vec::new()
    }

    /// The name the engine falls back to for each of
    /// [`Dialect::implicit_relation_names`], in the same order, when its first
    /// choice is taken and this is its `suffix`-th retry (#987). Two tables
    /// whose generated names meet do not fail: the engine suffixes the later
    /// one, and which one is later is the plan's order. A declared name equal
    /// to a fallback is the same order dependence as one equal to the first
    /// choice. The default is none, like the first choices.
    fn implicit_relation_fallbacks(
        &self,
        _name: &TableName,
        _table: &Table,
        _suffix: u32,
    ) -> Vec<String> {
        Vec::new()
    }

    /// Where `to` sits on the path a bare name in a definition in `from` is
    /// looked up along, or `None` where it is not on that path at all
    /// (DECISIONS 317).
    ///
    /// A rank rather than a yes, because the engine resolves a bare name in
    /// the **first** schema of the path that holds one: two declared modules
    /// of the same bare name are not two candidates, and an edge to the
    /// worse-placed one is invented.
    ///
    /// The default is `Some(0)` everywhere, which is the honest answer from a
    /// dialect that has not measured its engine's rule: every schema on an
    /// equal footing, an edge too many in the direction the scan has always
    /// erred, and never an edge too few.
    fn bare_name_rank(&self, _from: &str, _to: &str) -> Option<usize> {
        Some(0)
    }

    /// Unchanged modules whose binding can move when this plan introduces
    /// names on their write path. The differ adds these as ordinary typed
    /// alterations before risk classification and dependency ordering, so
    /// approval and connected rebuild guards cover them too (DECISIONS 422).
    /// Dialects without this binding rule return no additional alterations.
    fn rebound_modules(
        &self,
        _declared: &Schema,
        _arriving: &[ModuleId],
        _already_changed: &BTreeSet<ModuleId>,
    ) -> BTreeSet<ModuleId> {
        BTreeSet::new()
    }

    /// This argument type as the engine spells it *in a routine's identity*.
    ///
    /// Deliberately not [`Dialect::normalize_type`], and deliberately not over
    /// a [`ColumnType`] at all. Measured on PostgreSQL 18.6 (ADR-0009 §1):
    ///
    /// - `f(varchar(10))` and `f(varchar(20))` are one function, identified as
    ///   `f(character varying)` — the engine discards type modifiers when it
    ///   identifies a routine and keeps them when it types a column, so reusing
    ///   the column normalizer would key one engine object as two modules and
    ///   every `DROP` and `GRANT` the plan emitted would name a signature the
    ///   engine resolves to something else; and
    /// - an identity holds spellings a column type cannot — `"char"`,
    ///   `integer[]`, a schema-qualified domain — which is why
    ///   [`pbps_model::RoutineArg`] carries text and this returns text.
    ///
    /// The default reads the argument as a column type and spells it that way
    /// where it can, and leaves it alone where it cannot. That is right for a
    /// dialect whose routine arguments *are* column types — SQL Server's are,
    /// and `dbo.f(int)` declared against `dbo.f(integer)` read back is one
    /// routine there, which is the case this default exists to keep — and it
    /// cannot damage the spellings a column type has no room for, because
    /// those do not parse as one and come back unchanged.
    ///
    /// A dialect whose identities are not column types overrides it.
    fn normalize_routine_arg(&self, arg: &RoutineArg) -> Result<RoutineArg, DialectError> {
        let Ok(ty) = arg.as_str().parse::<ColumnType>() else {
            return Ok(arg.clone());
        };
        let spelled = self.normalize_type(&ty)?;
        spelled
            .to_string()
            .parse()
            .map_err(|_| DialectError::Unsupported {
                dialect: self.name(),
                feature: format!("`{spelled}` as a routine argument type"),
            })
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

    /// Whether an `AlterModule` on this engine takes the object's existing
    /// grants with it, the way a `DropModule` does (ADR-0009 §3, #248).
    ///
    /// SQL Server's `AlterModule` is `CREATE OR ALTER` — the grant-preserving
    /// path ADR-0002 chose it for — so a grant a declared role already holds
    /// and this plan does not touch needs no restatement: the default answers
    /// `false`. On PostgreSQL every module edit is a `DROP` and a `CREATE`
    /// (ADR-0009 §3): the object arrives with no ACL at all, so a grant that
    /// would otherwise read as unchanged has to be written into the plan
    /// again or it does not come back. `diff_roles` treats an `AlterModule`
    /// exactly as it treats a `DropModule` when this answers `true`.
    fn rebuilds_modules(&self) -> bool {
        false
    }

    /// Whether a routine this engine creates arrives executable by every
    /// principal in it (ADR-0010 §5, DECISIONS 371).
    ///
    /// PostgreSQL's `acldefault` for a function includes `EXECUTE` to
    /// `PUBLIC`, so a `SECURITY DEFINER` routine is callable — with its
    /// owner's privileges — by anyone who can reach its schema the moment the
    /// `CREATE` commits. SQL Server grants no principal `EXECUTE` on a
    /// procedure it creates, and its `public` is an ordinary database role
    /// rather than the engine's default, so it answers `false` and
    /// [`Change::PublicExecution`] is never built for it.
    ///
    /// What the differ does with a `true` is take the default away as part of
    /// creating the routine, unless the declaration opts back in
    /// (`pbps_model::PublicExecute`). It is a property of the *engine* and not
    /// a policy knob: a dialect answering `false` while its engine grants the
    /// default would be the silent wrong answer this whole issue is about.
    fn creates_public_executable_routines(&self) -> bool {
        false
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
    /// it would skip. A new table's key probes can instead ask about its
    /// declared rows, including a typed empty relation where it has none.
    ///
    /// The default is "none", which is the honest answer for a dialect that has
    /// not implemented them. A deliberately unbuilt check belongs in
    /// `unchecked`, with its object and reason; only executable queries belong
    /// in `probes`.
    fn preflight(&self, _changes: &ChangeSet) -> Preflight {
        Preflight::default()
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

    /// The settings a deployment must run under, for a mode that opens no
    /// transaction to carry them.
    ///
    /// A dialect whose [`Dialect::transaction_framing`] `begin` carries session
    /// settings — because what a declared expression *means* depends on them —
    /// has a hole where a **staged** apply runs: that mode commits each
    /// statement on its own and never opens a transaction, so `begin` is never
    /// sent and the pins never happen. `--resume` makes it worse, because it
    /// starts on a fresh connection partway through the plan.
    ///
    /// So this is the same pins without the `BEGIN`, established on the
    /// connection before the first statement of a staged run. Rendered scripts
    /// carry the same list before their DDL; the client must send it before
    /// parsing later statements (DECISIONS 458). `None` is the
    /// right answer for a dialect whose framing carries nothing but the
    /// transaction — SQL Server's `SET XACT_ABORT ON` governs a transaction and
    /// means nothing outside one — and it is the default, so a dialect says
    /// this only when it has something to say.
    fn session_pins(&self) -> Option<&'static str> {
        None
    }

    /// The transaction each pre-flight probe runs in, or `None` where a probe
    /// can run without one.
    ///
    /// A probe evaluates the operator's declared expression against the live
    /// rows, between "this plan was approved" and "these statements
    /// executed". On an engine where an expression can write — PostgreSQL
    /// accepts `CHECK (nextval('s') > 0)` — that evaluation is a side effect
    /// nobody approved, and one a rollback does not undo. A read-only
    /// transaction makes the engine refuse the write itself, so the probe fails
    /// and is reported unchecked, instead of pbps judging which expressions
    /// are safe to run (DECISIONS 537).
    ///
    /// The runner opens it with `begin` and always closes it with `rollback`,
    /// one transaction per probe, so a probe the engine refuses cannot abort
    /// the next one. It runs outside any transaction of the deployment's own.
    ///
    /// Required, not defaulted, for the reason [`Dialect::transaction_framing`]
    /// gives: whether an engine's expressions can write is the engine's
    /// answer, and a default would be one engine's answer under a neutral name.
    fn probe_framing(&self) -> Option<TransactionFraming>;

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

    /// What this dialect could **not** answer about the declarations offline.
    ///
    /// Not errors and not warnings about the declarations: notes about the
    /// check itself. `validate` runs with no connection (SPEC §9.1 makes it a
    /// preview), and some questions have no offline answer at all — whether
    /// two declared row keys are one row is decided by the live key column's
    /// collation, which ADR-0013 §5 keeps out of `pbps-model` on purpose. A
    /// command that reports clean about a question it never asked is the
    /// silence this project's own rule is about: absent, empty and unreadable
    /// are three different answers.
    ///
    /// Empty by default, which is the right answer for a dialect whose offline
    /// checks are complete (DECISIONS 327).
    fn declaration_notes(&self, _schema: &Schema) -> Vec<String> {
        Vec::new()
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

/// The whole-schema question of whether a declared index, or the index behind
/// a named primary key or unique constraint, collides with another relation
/// in its schema — a third case of the namespace-sharing rule 201 moved to
/// the dialect, not a new one (issue #176, DECISIONS 453).
///
/// `false` from [`Dialect::indexes_share_namespace_with_tables`] means this
/// engine has no such namespace to collide in, so there is nothing to check —
/// the same short-circuit `check_module_names` takes per module kind.
///
/// The namespace is seeded with every table and every module kind that shares
/// it too (a view, on PostgreSQL) — the same boundary
/// [`Dialect::shares_namespace_with_tables`] already draws — and then every
/// table's declared index, primary-key and unique-constraint names are folded
/// in, each checked against everything already claimed. A check or
/// foreign-key constraint has no backing index and is not in this union
/// (ADR-0009 §1; issue #179 owns per-table constraint-kind reuse).
///
/// # The one pair this does not report, and who owns it instead
///
/// A named primary key and a unique constraint **on the same table** sharing a
/// name is already refused by [`Table::constraint_name_conflicts`], which every
/// dialect's `validate_table` runs. Reporting it here too earned one
/// declaration two findings for one defect — on `validate`, and again on the
/// connected planning and bootstrap gates that run the same list
/// (DECISIONS 141, 507; issue #498).
///
/// The table-local rule owns it, because it is the narrower and the
/// engine-independent one: two constraints of one table may not share a name on
/// either engine, while this function does not run at all where indexes have no
/// shared namespace. Nothing else moves. An index against a key constraint, a
/// key constraint against a table or a view, and two key constraints on
/// *different* tables are each still reported here — the last because no
/// table-local rule can see across tables — and a check or foreign key sharing
/// a name is still the table-local rule's alone.
pub fn check_index_names(schema: &Schema, dialect: &dyn Dialect) -> Vec<String> {
    if !dialect.indexes_share_namespace_with_tables() {
        return Vec::new();
    }
    /// A name already taken, and by what. `key_of` is the table whose own
    /// constraint-name rule already covers this claim — `Some` for a named
    /// primary key or unique constraint, `None` for an index, a table or a
    /// module, which that rule does not compare.
    struct Claim<'a> {
        descriptor: String,
        key_of: Option<&'a TableName>,
    }
    let unclaimed = |descriptor| Claim {
        descriptor,
        key_of: None,
    };
    let mut problems = Vec::new();
    let mut claimed: BTreeMap<ObjectName, Claim<'_>> = BTreeMap::new();
    for name in schema.tables.keys() {
        claimed.insert(name.clone(), unclaimed(format!("table `{name}`")));
    }
    for (id, module) in &schema.modules {
        if dialect.shares_namespace_with_tables(module.kind) {
            claimed.insert(
                id.object_name(),
                unclaimed(format!("{} `{id}`", module.kind)),
            );
        }
    }
    for (table_name, table) in &schema.tables {
        let mut declared: Vec<(&str, Claim<'_>)> = Vec::new();
        for index_name in table.indexes.keys() {
            declared.push((
                index_name.as_str(),
                unclaimed(format!("index `{table_name}.{index_name}`")),
            ));
        }
        if let Some(pk_name) = table.primary_key.as_ref().and_then(|pk| pk.name.as_deref()) {
            declared.push((
                pk_name,
                Claim {
                    descriptor: format!("primary key `{table_name}.{pk_name}`"),
                    key_of: Some(table_name),
                },
            ));
        }
        for unique_name in table.unique.keys() {
            declared.push((
                unique_name.as_str(),
                Claim {
                    descriptor: format!("unique constraint `{table_name}.{unique_name}`"),
                    key_of: Some(table_name),
                },
            ));
        }
        for (name, claim) in declared {
            let claim_name = ObjectName::new(table_name.schema.clone(), name);
            let descriptor = claim.descriptor.clone();
            let key_of = claim.key_of;
            if let Some(existing) = claimed.insert(claim_name.clone(), claim) {
                // Both sides a named key constraint of the same table: the
                // table-local rule has already refused this declaration, and
                // saying so twice does not make it more refused.
                if existing.key_of.is_some() && existing.key_of == key_of {
                    continue;
                }
                problems.push(format!(
                    "{} and {descriptor} are both named `{claim_name}`; {} keeps tables, \
                     views and indexes in one namespace per schema, so it can hold only one of \
                     them",
                    existing.descriptor,
                    dialect.name()
                ));
            }
        }
    }
    // The names the engine generates for what a table implies: the index
    // behind an unnamed primary key, the sequence behind an identity column.
    // Asked after every declared name is in, because which of two claimants
    // the engine lets have a name depends on which is created first — a
    // generated name the engine finds taken gets a numeric suffix instead,
    // measured — and a plan's order is not one to depend on. So a generated
    // name that meets a declared one is refused whichever comes first; two
    // generated names that meet are not, because the engine resolves that
    // itself (#465, DEC-465.1). It resolves it by suffixing the later one,
    // so the names it may fall back to are in play as well (#987).
    let declared_names: BTreeSet<ObjectName> = claimed.keys().cloned().collect();
    let mut generated: BTreeMap<ObjectName, Vec<(&TableName, usize, ImplicitRelation)>> =
        BTreeMap::new();
    for (table_name, table) in &schema.tables {
        for (i, relation) in dialect
            .implicit_relation_names(table_name, table)
            .into_iter()
            .enumerate()
        {
            let claim_name = ObjectName::new(table_name.schema.clone(), relation.name.clone());
            if declared_names.contains(&claim_name) {
                let existing = &claimed[&claim_name];
                problems.push(format!(
                    "{} and {} are both named `{claim_name}`; {} keeps tables, views, indexes \
                     and sequences in one namespace per schema, so it can hold only one of \
                     them, and which one depends on the order they are created in. {}",
                    existing.descriptor,
                    relation.descriptor,
                    dialect.name(),
                    relation.remedy
                ));
            }
            generated
                .entry(claim_name)
                .or_default()
                .push((table_name, i, relation));
        }
    }
    // `c` generated names meeting at one name: the engine keeps it for the
    // first created and gives the others its 1st to `c - 1`th fallback, in an
    // order the plan decides. Any of them may be where any claimant lands.
    for claimants in generated.values().filter(|c| c.len() > 1) {
        let retries = u32::try_from(claimants.len() - 1).unwrap_or(u32::MAX);
        // A claimant's own remedy takes that one claimant out. With two, that
        // leaves the other its first choice and no fallback in play; with
        // three or more, the rest still meet and the fallback stays taken,
        // so only moving the declared object is a remedy.
        let lone_remedy = (claimants.len() > 2).then_some("Rename the other object.");
        let mut reported = BTreeSet::new();
        for (table_name, i, relation) in claimants {
            let descriptor = &relation.descriptor;
            let table = &schema.tables[*table_name];
            for suffix in 1..=retries {
                let Some(fallback) = dialect
                    .implicit_relation_fallbacks(table_name, table, suffix)
                    .into_iter()
                    .nth(*i)
                else {
                    continue;
                };
                let claim_name = ObjectName::new(table_name.schema.clone(), fallback);
                if declared_names.contains(&claim_name) && reported.insert(claim_name.clone()) {
                    let existing = &claimed[&claim_name];
                    problems.push(format!(
                        "{} and {descriptor} may both be named `{claim_name}`: {descriptor} \
                         meets another generated name, and {} gives one of them this name \
                         instead, which one depending on the order they are created in. {}",
                        existing.descriptor,
                        dialect.name(),
                        lone_remedy.unwrap_or(relation.remedy)
                    ));
                }
            }
        }
    }
    problems
}

/// The whole-schema question of whether a declared constraint's name is taken
/// in its schema, on an engine that keeps constraints in one namespace with
/// tables, views, routines and triggers (issue #496).
///
/// `false` from [`Dialect::constraints_share_namespace_with_tables`] means
/// there is nothing to check, the same short-circuit [`check_index_names`]
/// takes. The namespace is seeded with every table and every module that
/// shares it, and then each table's named primary key, unique constraints,
/// foreign keys and checks are folded in, each checked against everything
/// already claimed.
///
/// Two constraints of **one** table sharing a name are left to
/// [`Table::constraint_name_conflicts`], which every dialect's
/// `validate_table` runs, so one defect is not reported twice (issue #498's
/// reasoning). A constraint named after its own table is reported here: the
/// table-local rule does not compare against the table's name.
///
/// Compared exactly. Validation runs offline and cannot know the database's
/// collation. A pair that differs only in a way a case- or accent-insensitive
/// collation folds is still refused by the engine, loudly and before anything
/// else of the statement runs; this rule exists so that the exact pair, which
/// no collation separates, is refused before a plan is written.
pub fn check_constraint_names(schema: &Schema, dialect: &dyn Dialect) -> Vec<String> {
    if !dialect.constraints_share_namespace_with_tables() {
        return Vec::new();
    }
    /// A name already taken, by what, and — for a constraint — which table's
    /// own rule already compares it.
    struct Claim<'a> {
        descriptor: String,
        constraint_of: Option<&'a TableName>,
    }
    let mut claimed: BTreeMap<ObjectName, Claim<'_>> = BTreeMap::new();
    for name in schema.tables.keys() {
        claimed.insert(
            name.clone(),
            Claim {
                descriptor: format!("table `{name}`"),
                constraint_of: None,
            },
        );
    }
    for (id, module) in &schema.modules {
        if dialect.shares_namespace_with_tables(module.kind) {
            claimed.insert(
                id.object_name(),
                Claim {
                    descriptor: format!("{} `{id}`", module.kind),
                    constraint_of: None,
                },
            );
        }
    }
    let mut problems = Vec::new();
    for (table_name, table) in &schema.tables {
        let named = table
            .primary_key
            .as_ref()
            .and_then(|pk| pk.name.as_deref())
            .map(|name| (name, "primary key"))
            .into_iter()
            .chain(
                table
                    .unique
                    .keys()
                    .map(|n| (n.as_str(), "unique constraint")),
            )
            .chain(
                table
                    .foreign_keys
                    .keys()
                    .map(|n| (n.as_str(), "foreign key")),
            )
            .chain(
                table
                    .checks
                    .keys()
                    .map(|n| (n.as_str(), "check constraint")),
            );
        for (name, kind) in named {
            let claim_name = ObjectName::new(table_name.schema.clone(), name);
            let descriptor = format!("{kind} `{table_name}.{name}`");
            let claim = Claim {
                descriptor: descriptor.clone(),
                constraint_of: Some(table_name),
            };
            if let Some(existing) = claimed.insert(claim_name.clone(), claim) {
                if existing.constraint_of == Some(table_name) {
                    continue;
                }
                problems.push(format!(
                    "{} and {descriptor} are both named `{claim_name}`; {} keeps tables, views, \
                     routines, triggers and constraints in one namespace per schema, so it can \
                     hold only one of them",
                    existing.descriptor,
                    dialect.name()
                ));
            }
        }
    }
    problems
}

/// Render the dialect's session setup and statements in execution order.
///
/// Taking the dialect, rather than just its separator, keeps a new script
/// caller from omitting settings the deployment connection already establishes.
pub fn render_script(statements: &[Statement], dialect: &dyn Dialect) -> String {
    let Some(pins) = dialect.session_pins().filter(|_| !statements.is_empty()) else {
        return render_batches(statements, dialect.batch_separator());
    };
    let prepared: Vec<_> = std::iter::once(Statement::new(pins).own_batch())
        .chain(statements.iter().cloned())
        .collect();
    format!(
        "-- Execute statements/batches in order, not as one query.\n\
         -- Session settings must take effect before the following SQL is parsed.\n\n{}",
        render_batches(&prepared, dialect.batch_separator())
    )
}

fn render_batches(statements: &[Statement], separator: Option<&str>) -> String {
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
    use pbps_model::{CheckConstraint, Index, IndexColumn, PrimaryKey, UniqueConstraint};

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
            render_batches(&stmts, Some("GO")),
            "A;\nGO\n\nB;\nGO\n\nC;\n\nD;\n"
        );
        assert_eq!(render_batches(&stmts, None), "A;\n\nB;\n\nC;\n\nD;\n");
        assert_eq!(render_batches(&[], Some("GO")), "");
        assert_eq!(
            render_script(&stmts, &MinimalDialect),
            render_batches(&stmts, None)
        );
        assert_eq!(render_script(&[], &MinimalDialect), "");
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

    /// SQL Server's lexis, as `pbps-mssql` states it. Kept here so the scanner
    /// can be exercised against both shapes; each crate pins its own answers.
    const T_SQL: Lexicon = Lexicon {
        whitespace_is_ascii: false,
        quoted_identifiers: &[('[', ']'), ('"', '"')],
        escape_strings: false,
        dollar_quoted_strings: false,
        string_prefixes: &["n"],
        identifier_continues: pbps_model::module::is_regular_identifier_continue,
        reserved: never_reserved,
        unicode_identifiers: false,
    };

    /// PostgreSQL's, as `pbps-pg` states it.
    const PG: Lexicon = Lexicon {
        whitespace_is_ascii: true,
        quoted_identifiers: &[('"', '"')],
        escape_strings: true,
        dollar_quoted_strings: true,
        string_prefixes: &["u&", "e", "n", "b", "x"],
        identifier_continues: continues_ident,
        reserved: never_reserved,
        unicode_identifiers: true,
    };

    #[test]
    fn definition_layout_uses_the_engines_whitespace_class() {
        // Measured on PostgreSQL 18.6 and SQL Server 17.0.4075.5: all of
        // Unicode White_Space outside ASCII are identifier bytes on the
        // former and token separators on the latter (475).
        for ch in [
            '\u{85}', '\u{a0}', '\u{1680}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}',
            '\u{3000}',
        ]
        .into_iter()
        .chain('\u{2000}'..='\u{200a}')
        {
            for (wide, narrow) in [
                (format!("SELECT 1 AS x{ch}"), "SELECT 1 AS x"),
                (format!("SELECT 1 AS {ch}x"), "SELECT 1 AS x"),
                (format!("SELECT a{ch}b"), "SELECT a b"),
            ] {
                assert_ne!(
                    PG.normalize_definition(&wide),
                    PG.normalize_definition(narrow),
                    "{wide:?}"
                );
                assert_eq!(
                    T_SQL.normalize_definition(&wide),
                    T_SQL.normalize_definition(narrow),
                    "{wide:?}"
                );
            }
            // Quoting makes the same character data on both engines.
            for lexicon in [PG, T_SQL] {
                for wide in [format!("SELECT 'a{ch}b'"), format!("SELECT \"a{ch}b\"")] {
                    assert_eq!(lexicon.normalize_definition(&wide), wide);
                    assert_ne!(
                        lexicon.normalize_definition(&wide),
                        lexicon.normalize_definition(&wide.replace(ch, " "))
                    );
                }
            }
        }
        // Both engines accept all six ASCII separators, including vertical
        // tab, which Rust's is_ascii_whitespace omits.
        for ch in [' ', '\t', '\n', '\r', '\x0b', '\x0c'] {
            for lexicon in [PG, T_SQL] {
                assert_eq!(
                    lexicon.normalize_definition(&format!("{ch}SELECT{ch}{ch}1{ch}")),
                    "SELECT 1"
                );
            }
        }
    }

    /// The row of ADR-0011 Amendment 2's table that points the other way from
    /// the rest: one character, opposite meanings, and a shared default could
    /// only have been wrong for one of the two engines.
    #[test]
    fn a_bracket_is_a_quote_on_one_engine_and_a_subscript_on_the_other() {
        // A name is not layout.
        assert_ne!(
            T_SQL.normalize_definition("SELECT [a  b] FROM t"),
            T_SQL.normalize_definition("SELECT [a b] FROM t")
        );
        // A subscript is code, and a reindent inside one is not a change.
        assert_eq!(
            PG.normalize_definition("SELECT a[1  +  2] FROM t"),
            PG.normalize_definition("SELECT a[1 + 2] FROM t")
        );
    }

    /// A dollar-quoted string has no escapes at all, so nothing inside it can
    /// end it early and nothing inside it is layout. Read as code — which is
    /// what a scanner that does not know the syntax does — two bodies that
    /// return different strings compare equal and the change is never planned.
    #[test]
    fn spacing_inside_a_dollar_quoted_string_is_data() {
        assert_ne!(
            PG.normalize_definition("SELECT $tag$a  b$tag$"),
            PG.normalize_definition("SELECT $tag$a b$tag$")
        );
        // The tagless spelling, and a nested `$` that is not the closing tag.
        assert_ne!(
            PG.normalize_definition("SELECT $$a  b$$"),
            PG.normalize_definition("SELECT $$a b$$")
        );
        assert_eq!(
            PG.normalize_definition("SELECT $a$x  $b$  y$a$"),
            "SELECT $a$x  $b$  y$a$"
        );
        // The tag grammar is the engine's, which is over bytes: any character
        // outside ASCII is a tag character, including one `char::is_alphanumeric`
        // refuses. Measured on 18.6, `$á$` with a combining acute — `a` then
        // U+0301 — is a literal, and reading it as code folded the spacing
        // inside it away, which is the silent failure again.
        assert_ne!(
            PG.normalize_definition("SELECT $a\u{301}$x  y$a\u{301}$"),
            PG.normalize_definition("SELECT $a\u{301}$x y$a\u{301}$")
        );
        // The same grammar decides where the *preceding* identifier ends, and
        // a name it continues swallows the `$` that would have opened a tag.
        // Measured on 18.6, `á$tag$` is one identifier — so here the first
        // `$tag$` is part of a name and the second opens the literal, which is
        // where the two spellings differ.
        assert_ne!(
            PG.normalize_definition("SELECT a\u{301}$tag$ + $tag$x  y$tag$"),
            PG.normalize_definition("SELECT a\u{301}$tag$ + $tag$x y$tag$")
        );
        // Layout *around* one is still layout.
        assert_eq!(
            PG.normalize_definition("SELECT   $tag$a  b$tag$\n  FROM t"),
            "SELECT $tag$a  b$tag$ FROM t"
        );
    }

    /// A `$` is only a literal when it opens a tag. The tag follows the rules
    /// of an unquoted identifier, so a money literal and an identifier with a
    /// `$` in it are code — on the engine that has dollar quoting as much as on
    /// the one that has not.
    #[test]
    fn a_dollar_that_opens_no_tag_is_code() {
        for l in [PG, T_SQL] {
            assert_eq!(
                l.normalize_definition("SELECT   $1.00  + x"),
                "SELECT $1.00 + x"
            );
            assert_eq!(
                l.normalize_definition("SELECT total$   FROM t"),
                "SELECT total$ FROM t"
            );
            // Unterminated: it never becomes a tag, so the rest stays code.
            assert_eq!(l.normalize_definition("SELECT $tag  x"), "SELECT $tag x");
            // Two of them, with something between that is no tag.
            assert_eq!(
                l.normalize_definition("SELECT $1.00  +  $2.00"),
                "SELECT $1.00 + $2.00"
            );
            assert_eq!(
                l.normalize_definition("SELECT price$  ,  qty$  FROM t"),
                "SELECT price$ , qty$ FROM t"
            );
            // `$` inside a name is part of the name, not an opener: measured,
            // PostgreSQL lexes `a$b$c` as one identifier.
            assert_eq!(
                l.normalize_definition("SELECT a$b$c  ,  d  FROM t"),
                "SELECT a$b$c , d FROM t"
            );
            // A tag may not start with a digit, on either engine.
            assert_eq!(
                l.normalize_definition("SELECT $1x$a  b$1x$"),
                "SELECT $1x$a b$1x$"
            );
        }
    }

    /// `\'` does not close an `E'…'` string, so the spacing after it is still
    /// inside the literal. Measured against the engine (ADR-0011 Amendment 2):
    /// `E'it\'s  here'` is one ten-character string.
    #[test]
    fn a_backslash_escaped_quote_does_not_close_an_escape_string() {
        assert_ne!(
            PG.normalize_definition(r"SELECT E'it\'s  here'"),
            PG.normalize_definition(r"SELECT E'it\'s here'")
        );
        // `\\` is a literal backslash, so the quote after it *does* close.
        assert_eq!(
            PG.normalize_definition(r"SELECT E'a\\'   ,   'b  c'"),
            r"SELECT E'a\\' , 'b  c'"
        );
        // A doubled quote inside one stays inside it: leaving and re-entering
        // as a plain literal would disarm the backslash for the second half.
        assert_ne!(
            PG.normalize_definition(r"SELECT E'a''b\'c  d'"),
            PG.normalize_definition(r"SELECT E'a''b\'c d'")
        );
    }

    /// The `E` has to be a token of its own. `note'x'` is an identifier and a
    /// plain string, where a backslash is an ordinary character — arming the
    /// escape rule there would run the literal past the quote that ends it.
    #[test]
    fn an_identifier_ending_in_e_does_not_arm_the_escape_rule() {
        assert_eq!(
            PG.normalize_definition(r"SELECT note'a\'   ,   x  y"),
            r"SELECT note'a\' , x y"
        );
        // Nor does an `E` that is not touching the quote.
        assert_eq!(
            PG.normalize_definition(r"SELECT E   'a\'   ,   x  y"),
            r"SELECT E 'a\' , x y"
        );
        // "Is this `E` a token of its own" is the same question about where
        // the identifier before it ends, so it is the same grammar. Measured
        // on 18.6: with `á` spelled `a` then U+0301, `áE'a\'` is the name `áe`
        // applied to the plain two-character string `a\` — the `E` stays in
        // the name, and arming the escape rule here ran the literal past its
        // closer and folded the spacing of the next one away.
        assert_eq!(
            PG.normalize_definition("SELECT a\u{301}E'a\\'   ,   'p  q'"),
            "SELECT a\u{301}E'a\\' , 'p  q'"
        );
    }

    /// The engine that has neither extension must scan exactly as it did
    /// before they existed: `E` is an alias, `$` is a character in a name, and
    /// a backslash escapes nothing.
    #[test]
    fn an_engine_without_the_extensions_reads_them_as_ordinary_code() {
        assert_eq!(
            T_SQL.normalize_definition(r"SELECT E'it\'s  here'"),
            r"SELECT E'it\'s here'"
        );
        assert_eq!(
            T_SQL.normalize_definition("SELECT $tag$a  b$tag$"),
            "SELECT $tag$a b$tag$"
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

    #[test]
    fn line_comment_endings_preserve_literal_data_and_ignore_layout() {
        for lexicon in [Lexicon::ANSI, T_SQL, PG] {
            let expected = "SELECT 1 -- c\nWHERE x = 'a  b'";
            for ending in ["\r", "\n", "\r\n"] {
                let wide = format!("SELECT 1 -- c{ending}WHERE x = 'a  b'");
                let narrow = format!("SELECT 1 -- c{ending}WHERE x = 'a b'");
                assert_eq!(lexicon.normalize_definition(&wide), expected);
                assert_ne!(
                    lexicon.normalize_definition(&wide),
                    lexicon.normalize_definition(&narrow)
                );
            }
            for lookalike in ['\u{85}', '\u{2028}'] {
                assert_eq!(
                    lexicon
                        .normalize_definition(&format!("SELECT 1 -- c{lookalike}WHERE x = 'a  b'")),
                    "SELECT 1 -- c WHERE x = 'a b'"
                );
            }
        }
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
        assert!(MinimalDialect.preflight(&changes).probes.is_empty());
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
        fn lexicon(&self) -> Lexicon {
            Lexicon::ANSI
        }
        fn transaction_framing(&self) -> TransactionFraming {
            MinimalDialect.transaction_framing()
        }
        fn probe_framing(&self) -> Option<TransactionFraming> {
            MinimalDialect.probe_framing()
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
        fn indexes_share_namespace_with_tables(&self) -> bool {
            true
        }
        fn constraints_share_namespace_with_tables(&self) -> bool {
            false
        }
        /// The modifiers a column keeps: `varchar(10)` and `varchar(20)` are
        /// one function to this engine, and `f(varchar)` is what it calls
        /// both. Text in, text out — the argument of a routine identity is not
        /// a column type (ADR-0009 §1).
        fn normalize_routine_arg(&self, arg: &RoutineArg) -> Result<RoutineArg, DialectError> {
            let s = arg.as_str();
            let base = s.split('(').next().unwrap_or(s).trim();
            base.parse().map_err(|_| DialectError::Unsupported {
                dialect: "overloading",
                feature: format!("`{s}` as a routine argument"),
            })
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
    /// (ADR-0009 §1, measured). The default leaves the argument alone, which is
    /// only ever asked where nothing overloads.
    #[test]
    fn routine_argument_normalization_is_not_column_normalization() {
        let arg: RoutineArg = "varchar(10)".parse().unwrap();
        let ty: ColumnType = "varchar(10)".parse().unwrap();
        assert_eq!(
            OverloadingDialect.normalize_routine_arg(&arg).unwrap(),
            "varchar".parse::<RoutineArg>().unwrap()
        );
        assert_eq!(OverloadingDialect.normalize_type(&ty).unwrap(), ty);
        assert_eq!(MinimalDialect.normalize_routine_arg(&arg).unwrap(), arg);
    }

    fn table() -> Table {
        Table::default()
    }

    fn with_index(mut table: Table, name: &str) -> Table {
        table.indexes.insert(
            name.to_owned(),
            Index {
                columns: vec![IndexColumn {
                    name: "n".to_owned(),
                    descending: false,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
            },
        );
        table
    }

    fn with_primary_key(mut table: Table, name: &str) -> Table {
        table.primary_key = Some(PrimaryKey {
            name: Some(name.to_owned()),
            columns: vec!["n".to_owned()],
        });
        table
    }

    fn with_unique(mut table: Table, name: &str) -> Table {
        table.unique.insert(
            name.to_owned(),
            UniqueConstraint {
                columns: vec!["n".to_owned()],
            },
        );
        table
    }

    fn with_check(mut table: Table, name: &str) -> Table {
        table.checks.insert(
            name.to_owned(),
            CheckConstraint {
                expression: "n > 0".to_owned(),
            },
        );
        table
    }

    fn schema_of(tables: &[(&str, Table)]) -> Schema {
        let mut schema = Schema::default();
        for (name, table) in tables {
            schema.tables.insert(name.parse().unwrap(), table.clone());
        }
        schema
    }

    fn with_foreign_key(mut table: Table, name: &str) -> Table {
        table.foreign_keys.insert(
            name.to_owned(),
            pbps_model::ForeignKey {
                columns: vec!["n".to_owned()],
                references_table: "coll.parent".parse().unwrap(),
                references_columns: vec!["n".to_owned()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        table
    }

    /// Issue #496, measured on SQL Server 17.0.4075.5: two tables in one
    /// schema declaring checks of one name cannot both be created, and a
    /// foreign key of that name collides with the check the same way.
    /// PostgreSQL 18.6 creates all three.
    #[test]
    fn two_tables_constraints_of_one_name_collide_only_where_constraints_share_the_namespace() {
        let schema = schema_of(&[
            ("coll.t1", with_check(table(), "c")),
            ("coll.t2", with_check(table(), "c")),
            ("coll.t3", with_foreign_key(table(), "c")),
        ]);
        let problems = check_constraint_names(&schema, &MinimalDialect);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems[0].contains("check constraint `coll.t1.c`")
                && problems[0].contains("check constraint `coll.t2.c`"),
            "{problems:?}"
        );
        assert!(
            problems[1].contains("foreign key `coll.t3.c`"),
            "{problems:?}"
        );
        assert!(
            check_constraint_names(&schema, &OverloadingDialect).is_empty(),
            "a check or foreign-key name is per table on PostgreSQL"
        );
    }

    /// The same namespace holds the tables, so a constraint named after
    /// another table, or after its own, collides with it (#496).
    #[test]
    fn a_constraint_named_after_a_table_collides_where_constraints_share_the_namespace() {
        let schema = schema_of(&[
            ("coll.t1", table()),
            ("coll.t2", with_primary_key(table(), "t1")),
            ("coll.t3", with_unique(table(), "t3")),
        ]);
        let problems = check_constraint_names(&schema, &MinimalDialect);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains("table `coll.t1`") && p.contains("primary key `coll.t2.t1`")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("table `coll.t3`")
                    && p.contains("unique constraint `coll.t3.t3`")),
            "{problems:?}"
        );
    }

    /// The negative cases: another schema is another namespace, an index is
    /// not in it (measured: `CREATE INDEX c` beside a check `c` is accepted),
    /// and two constraints of one table are the table-local rule's to report,
    /// once (#179, #498).
    #[test]
    fn constraint_names_in_other_schemas_indexes_and_one_tables_own_are_not_this_rules() {
        let schema = schema_of(&[
            ("coll.t1", with_check(table(), "c")),
            ("other.t2", with_check(table(), "c")),
            ("coll.t3", with_index(table(), "c")),
            ("coll.t4", with_unique(with_check(table(), "k"), "k")),
        ]);
        assert!(
            check_constraint_names(&schema, &MinimalDialect).is_empty(),
            "{:?}",
            check_constraint_names(&schema, &MinimalDialect)
        );
    }

    /// Two tables in one schema declaring an index of the same name is the
    /// finding measured on PostgreSQL 18.6 in issue #176: the second
    /// `CREATE INDEX ix_n` fails with `42P07`. SQL Server keeps an index name
    /// unique per table, so the same declaration is valid there.
    #[test]
    fn two_tables_with_one_index_name_collide_only_where_indexes_share_the_namespace() {
        let schema = schema_of(&[
            ("coll.t1", with_index(table(), "ix_n")),
            ("coll.t2", with_index(table(), "ix_n")),
        ]);
        let problems = check_index_names(&schema, &OverloadingDialect);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("coll.t1.ix_n"), "{problems:?}");
        assert!(problems[0].contains("coll.t2.ix_n"), "{problems:?}");
        assert!(problems[0].contains("coll.ix_n"), "{problems:?}");

        assert!(
            check_index_names(&schema, &MinimalDialect).is_empty(),
            "an index name is only unique per table on SQL Server"
        );
    }

    /// An index named after a table in the same schema is the same namespace
    /// collision from the other direction (issue #176).
    #[test]
    fn an_index_named_after_a_table_collides_where_indexes_share_the_namespace() {
        let schema = schema_of(&[("coll.t1", table()), ("coll.t2", with_index(table(), "t1"))]);
        let problems = check_index_names(&schema, &OverloadingDialect);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("table `coll.t1`"), "{problems:?}");
        assert!(problems[0].contains("index `coll.t2.t1`"), "{problems:?}");

        assert!(check_index_names(&schema, &MinimalDialect).is_empty());
    }

    /// A named unique constraint is backed by an index of that name (the
    /// review-widened half of issue #176), so it collides with a declared
    /// index sharing its name.
    #[test]
    fn a_unique_constraint_and_an_index_sharing_a_name_collide() {
        let schema = schema_of(&[
            ("coll.t1", with_unique(table(), "uq_n")),
            ("coll.t2", with_index(table(), "uq_n")),
        ]);
        let problems = check_index_names(&schema, &OverloadingDialect);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("unique constraint `coll.t1.uq_n`"),
            "{problems:?}"
        );
        assert!(problems[0].contains("index `coll.t2.uq_n`"), "{problems:?}");

        assert!(check_index_names(&schema, &MinimalDialect).is_empty());
    }

    /// A named primary key is backed by an index too, and a unique constraint
    /// named after a table collides with it from the other direction — both
    /// measured on 18.6 in the widened issue.
    #[test]
    fn a_primary_key_and_a_unique_constraint_named_after_a_table_collide() {
        let pk_schema = schema_of(&[
            ("coll.t1", with_primary_key(table(), "ix_n")),
            ("coll.t2", with_index(table(), "ix_n")),
        ]);
        let problems = check_index_names(&pk_schema, &OverloadingDialect);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("primary key `coll.t1.ix_n`"),
            "{problems:?}"
        );
        assert!(problems[0].contains("index `coll.t2.ix_n`"), "{problems:?}");
        assert!(check_index_names(&pk_schema, &MinimalDialect).is_empty());

        let uq_schema = schema_of(&[
            ("coll.t1", with_unique(table(), "t2")),
            ("coll.t2", table()),
        ]);
        let problems = check_index_names(&uq_schema, &OverloadingDialect);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("table `coll.t2`"), "{problems:?}");
        assert!(
            problems[0].contains("unique constraint `coll.t1.t2`"),
            "{problems:?}"
        );
        assert!(check_index_names(&uq_schema, &MinimalDialect).is_empty());
    }

    /// The boundary, measured beside the collisions above: a check constraint
    /// has no backing index, so it is not in this namespace at all, even when
    /// it is named after a table. This is the negative case issue #176 names
    /// explicitly.
    #[test]
    fn a_check_constraint_named_after_a_table_is_not_a_collision() {
        let schema = schema_of(&[("coll.t1", with_check(table(), "t2")), ("coll.t2", table())]);
        assert!(
            check_index_names(&schema, &OverloadingDialect).is_empty(),
            "{:?}",
            check_index_names(&schema, &OverloadingDialect)
        );
        assert!(check_index_names(&schema, &MinimalDialect).is_empty());
    }

    /// A view is in `pg_class` beside tables, so an index named after one
    /// collides too — the same boundary `shares_namespace_with_tables`
    /// already draws for a module against a table.
    #[test]
    fn an_index_named_after_a_view_collides_where_views_share_the_namespace() {
        let mut schema = schema_of(&[("coll.t1", with_index(table(), "v"))]);
        schema
            .modules
            .insert("coll.v".parse().unwrap(), module(ModuleKind::View));
        let problems = check_index_names(&schema, &OverloadingDialect);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("view `coll.v`"), "{problems:?}");
        assert!(problems[0].contains("index `coll.t1.v`"), "{problems:?}");
    }

    /// One defect, one finding. A named primary key and a unique constraint on
    /// the same table sharing a name is refused by
    /// `Table::constraint_name_conflicts`, which every dialect's
    /// `validate_table` runs, so reporting it here as well gave the same
    /// declaration two findings — on `validate`, and again on the connected
    /// planning and bootstrap gates that run the same list (issue #498,
    /// DECISIONS 141).
    ///
    /// The table-local rule keeps it because it is the narrower and the
    /// engine-independent one: it holds on SQL Server, where this function does
    /// not run at all.
    #[test]
    fn a_primary_key_and_a_unique_constraint_of_one_table_are_left_to_the_table_local_rule() {
        let shared = with_unique(with_primary_key(table(), "shared"), "shared");
        assert_eq!(
            shared.constraint_name_conflicts().len(),
            1,
            "the premise: the table-local rule refuses this declaration"
        );
        let schema = schema_of(&[("coll.t1", shared)]);
        assert!(
            check_index_names(&schema, &OverloadingDialect).is_empty(),
            "{:?}",
            check_index_names(&schema, &OverloadingDialect)
        );
        assert!(check_index_names(&schema, &MinimalDialect).is_empty());
    }

    /// And nothing else moves. Each of these is a collision no table-local
    /// rule can see — across two tables, or against an index, which
    /// `constraint_name_conflicts` deliberately excludes — so each is still
    /// this function's to report.
    #[test]
    fn the_collisions_no_table_local_rule_can_see_are_still_reported() {
        for (label, tables) in [
            (
                "two tables' key constraints",
                vec![
                    ("coll.t1", with_primary_key(table(), "shared")),
                    ("coll.t2", with_unique(table(), "shared")),
                ],
            ),
            (
                "a primary key and an index on one table",
                vec![(
                    "coll.t1",
                    with_index(with_primary_key(table(), "shared"), "shared"),
                )],
            ),
            (
                "a unique constraint and an index on one table",
                vec![(
                    "coll.t1",
                    with_index(with_unique(table(), "shared"), "shared"),
                )],
            ),
            (
                "a primary key named after a table",
                vec![
                    ("coll.t1", with_primary_key(table(), "t2")),
                    ("coll.t2", table()),
                ],
            ),
        ] {
            let schema = schema_of(&tables);
            let problems = check_index_names(&schema, &OverloadingDialect);
            assert_eq!(problems.len(), 1, "{label}: {problems:?}");
            assert!(
                problems[0].contains("shared") || problems[0].contains("t2"),
                "{label}"
            );
        }
    }

    /// The three constraint kinds the table-local rule owns outright: none of
    /// them is backed by an index, so a name they share was never this
    /// function's to report and is unaffected by the deduplication above.
    #[test]
    fn a_check_or_foreign_key_sharing_a_name_stays_with_the_table_local_rule() {
        let shared = with_check(with_primary_key(table(), "shared"), "shared");
        assert_eq!(shared.constraint_name_conflicts().len(), 1);
        let schema = schema_of(&[("coll.t1", shared)]);
        assert!(
            check_index_names(&schema, &OverloadingDialect).is_empty(),
            "{:?}",
            check_index_names(&schema, &OverloadingDialect)
        );
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

    /// Standard SQL, for the same reason as the transaction framing below: a
    /// dialect that is not any real database may not claim one's brackets.
    fn lexicon(&self) -> Lexicon {
        Lexicon::ANSI
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

    /// None: nothing opens a connection with this dialect, so nothing probes
    /// through it.
    fn probe_framing(&self) -> Option<TransactionFraming> {
        None
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

#[cfg(test)]
mod code_only_tests {
    use super::*;

    const PG: Lexicon = Lexicon {
        whitespace_is_ascii: true,
        quoted_identifiers: &[('"', '"')],
        escape_strings: true,
        dollar_quoted_strings: true,
        string_prefixes: &["u&", "e", "n", "b", "x"],
        identifier_continues: continues_ident,
        reserved: never_reserved,
        unicode_identifiers: true,
    };
    const MSSQL: Lexicon = Lexicon {
        whitespace_is_ascii: false,
        quoted_identifiers: &[('[', ']'), ('"', '"')],
        escape_strings: false,
        dollar_quoted_strings: false,
        string_prefixes: &["n"],
        identifier_continues: pbps_model::module::is_regular_identifier_continue,
        reserved: never_reserved,
        unicode_identifiers: false,
    };

    /// `text` with each of `regions` replaced by spaces, character for
    /// character — what the blanking of exactly those regions looks like.
    fn blanked(text: &str, regions: &[&str]) -> String {
        regions.iter().fold(text.to_owned(), |out, region| {
            out.replace(region, &" ".repeat(region.chars().count()))
        })
    }

    /// Every character PostgreSQL separates tokens with, and no other.
    ///
    /// **Measured** on 18.6, one `ALTER TABLE … CHECK (<char>)` per character:
    /// the six ASCII separators are each `syntax error at or near ")"`, and
    /// every non-ASCII member of Unicode White_Space names a column instead
    /// (`column " " does not exist`). The vertical tab is the one Rust's
    /// `trim_ascii` leaves out, and the one that reached the engine as an
    /// empty `CHECK` (issue #480).
    #[test]
    fn only_the_six_ascii_separators_are_absent_expressions_on_postgres() {
        for ch in ['\u{20}', '\u{9}', '\u{a}', '\u{b}', '\u{c}', '\u{d}'] {
            assert_eq!(
                PG.expression_in(&ch.to_string()),
                Expression::Absent,
                "{ch:?} separates tokens on this engine"
            );
        }
        // The measured identifier half, which is why this is not a `trim`.
        for ch in [
            '\u{85}', '\u{a0}', '\u{1680}', '\u{2000}', '\u{2003}', '\u{2028}', '\u{2029}',
            '\u{202f}', '\u{205f}', '\u{3000}',
        ] {
            assert_eq!(
                PG.expression_in(&ch.to_string()),
                Expression::Present,
                "{ch:?} names a column on this engine"
            );
        }
        // Rust's own classes, for the record: `trim` would refuse the whole
        // second list, and `trim_ascii` accepts a vertical tab the engine
        // does not.
        assert!(!'\u{b}'.is_ascii_whitespace());
        assert!('\u{a0}'.is_whitespace());
        // Mixtures are still absent, and a mixture with an identifier is not.
        assert_eq!(PG.expression_in(" \t\u{b}\r\n\u{c}"), Expression::Absent);
        assert_eq!(PG.expression_in("\t\u{a0}\n"), Expression::Present);
        assert_eq!(PG.expression_in(""), Expression::Absent);
    }

    /// Comments are layout too, and they are not whitespace bytes.
    ///
    /// **Measured** on 18.6: a check constraint and a partial index's `WHERE`
    /// each answer `syntax error at or near ")"` for a line comment, a block
    /// comment, the nesting form, and any mixture of them with whitespace
    /// (issue #482).
    #[test]
    fn comments_are_absent_expressions_and_what_surrounds_them_is_not() {
        for text in [
            "-- nothing\n",
            "-- nothing",
            "/* nothing */",
            "/* a /* b */ c */",
            "  /* a */ \t\n -- b\n",
            "-- a\n-- b\n",
        ] {
            assert_eq!(PG.expression_in(text), Expression::Absent, "{text:?}");
        }
        for text in ["/* a */ true", "true /* a */", "true -- a\n", "-x", "a/b"] {
            assert_eq!(PG.expression_in(text), Expression::Present, "{text:?}");
        }
    }

    /// A literal and a quoted identifier are content, and the scan stops at
    /// the one that opens first — which is what keeps a `--` inside a literal
    /// from being read as a comment.
    ///
    /// **Measured** on 18.6, and this is the half that would refuse a valid
    /// declaration if it were wrong: `CHECK ('true')` and `CHECK ("flag")` are
    /// accepted outright, while `CHECK ('-- not a comment')` is refused on
    /// *type* grounds (`invalid input syntax for type boolean`) rather than as
    /// a missing expression.
    #[test]
    fn a_literal_or_a_quoted_name_is_an_expression_even_when_it_reads_like_a_comment() {
        for text in [
            "'true'",
            "\"flag\"",
            "'-- not a comment'",
            "'/* still data */'",
            "$q$ -- data $q$::boolean",
            "E'x'",
            "  '-- a'  ",
        ] {
            assert_eq!(PG.expression_in(text), Expression::Present, "{text:?}");
        }
    }

    /// Absent, empty and unreadable are three different things.
    ///
    /// **Measured** on 18.6, `CHECK (/* a)` is `unterminated /* comment`, not
    /// a syntax error about a missing expression — so this must not be called
    /// empty, and the engine keeps the sentence that names the real cause.
    #[test]
    fn text_that_ends_inside_a_block_comment_is_unreadable_rather_than_absent() {
        for text in ["/* a", "/* a /* b */", "  /* a */ /* b"] {
            assert_eq!(PG.expression_in(text), Expression::Unreadable, "{text:?}");
        }
        // The closed forms of the same shapes, for contrast.
        assert_eq!(PG.expression_in("/* a */"), Expression::Absent);
        assert_eq!(PG.expression_in("/* a /* b */ */"), Expression::Absent);
    }

    /// The other engine takes Unicode White_Space as a separator, so the same
    /// text is a different answer there (DECISIONS 475). One lexicon field,
    /// two answers — which is the whole reason this is asked of the lexicon.
    #[test]
    fn the_engines_disagree_about_non_ascii_layout_and_agree_about_comments() {
        assert_eq!(MSSQL.expression_in("\u{a0}"), Expression::Absent);
        assert_eq!(PG.expression_in("\u{a0}"), Expression::Present);
        for lexicon in [PG, MSSQL] {
            assert_eq!(lexicon.expression_in("/* a */"), Expression::Absent);
            assert_eq!(lexicon.expression_in("-- a\n"), Expression::Absent);
            assert_eq!(lexicon.expression_in("x > 0"), Expression::Present);
        }
    }

    /// Measured: `SELECT E'x\' , es.a'` is one literal to PostgreSQL. Read
    /// with the other engine's rules it closed at the `\'`, the name after
    /// it was code, and the quote before `AS` opened a literal that ran to
    /// the end.
    #[test]
    fn an_escape_string_is_one_literal_where_the_engine_has_them() {
        let text = "SELECT E'x\\' , es.a' AS s";
        assert_eq!(PG.code_only(text), blanked(text, &["E'x\\' , es.a'"]));
        assert_eq!(MSSQL.code_only(text), blanked(text, &["'x\\'", "' AS s"]));
        // A doubled quote inside one does not close it either.
        assert_eq!(
            PG.code_only("E'a''b' x"),
            blanked("E'a''b' x", &["E'a''b'"])
        );
        // And the prefix has to be a token of its own: `note'x'` is a name
        // followed by a plain literal, whose backslash is a character.
        assert_eq!(
            PG.code_only("note'x\\' y"),
            blanked("note'x\\' y", &["'x\\'"])
        );
    }

    /// A routine's body is a dollar-quoted string on this engine, and the
    /// scans are about what it says; so the one after `AS` is code, and what
    /// is inside it is lexed as code — its own literals blanked, an escape
    /// string by the escape rule. Every other dollar-quoted string is a
    /// datum, and a datum is blanked: measured, a view may not write `AS $x$`
    /// where an alias goes, so the word tells the two apart.
    #[test]
    fn a_dollar_quoted_string_is_a_body_after_the_word_as_and_a_datum_elsewhere() {
        let body = "() RETURNS int LANGUAGE sql AS $$ SELECT app.f('x', E'y\\'z') $$";
        // The tags go with the literals: a delimiter is not a name.
        assert_eq!(
            PG.code_only(body),
            blanked(body, &["$$", "'x'", "E'y\\'z'"])
        );
        let tagged = "AS $a$ SELECT app.f(1) $a$";
        assert_eq!(PG.code_only(tagged), blanked(tagged, &["$a$"]));
        let datum = "SELECT $$ es.a $$ AS t, $x$es.a$x$ AS u";
        assert_eq!(
            PG.code_only(datum),
            blanked(datum, &["$$ es.a $$", "$x$es.a$x$"])
        );
        // A default in the argument list is a datum, and the body after it is
        // still the body; a comment between `AS` and the body is no
        // objection, and `has` is not the word.
        let routine = "(a text DEFAULT $x$es.a$x$) RETURNS int AS /* c */ $$ SELECT es.b('q') $$";
        assert_eq!(
            PG.code_only(routine),
            blanked(routine, &["$x$es.a$x$", "/* c */", "'q'", "$$"])
        );
        let has = "SELECT * FROM es.has $$ es.a $$";
        assert_eq!(PG.code_only(has), blanked(has, &["$$ es.a $$"]));
        // Nor is a type named `as\u{a0}` applied to a string: the gap before
        // the string is ASCII whitespace, and the non-breaking space is a
        // name byte (measured, a valid view).
        let nbsp = "SELECT as\u{a0} $$ es.a $$ AS t";
        assert_eq!(PG.code_only(nbsp), blanked(nbsp, &["$$ es.a $$"]));
        // And a datum inside the body is a datum.
        let nested = "AS $b$ SELECT $$ es.a $$ $b$";
        assert_eq!(
            PG.code_only(nested),
            blanked(nested, &["$$ es.a $$", "$b$"])
        );
        // `$` inside an identifier is a name byte on either engine, and a
        // `$` that opens no tag is code.
        assert_eq!(PG.code_only("SELECT a$b$c FROM t"), "SELECT a$b$c FROM t");
        assert_eq!(PG.code_only("SELECT $1.00 FROM t"), "SELECT $1.00 FROM t");
        // SQL Server has no dollar quoting: the text is code.
        assert_eq!(MSSQL.code_only(datum), datum);
    }

    #[test]
    fn a_plain_routine_body_decodes_quotes_without_exposing_its_data() {
        let definition = "(arg text DEFAULT 'app.default_data()') RETURNS text
            AS /* body */ 'SELECT app.\"a''b\"(), ''app.inner_data()'';'
            LANGUAGE sql";
        let code = PG.code_only(definition);
        assert!(code.contains("app.\"a'b\"()"), "{code}");
        assert!(!code.contains("default_data"), "{code}");
        assert!(!code.contains("inner_data"), "{code}");
        assert_eq!(code.len(), definition.len());
        assert_eq!(code.find("LANGUAGE"), definition.find("LANGUAGE"));
        assert_eq!(code.matches('\n').count(), definition.matches('\n').count());
        assert!(!MSSQL.code_only(definition).contains("a'b"));

        for datum in [
            "SELECT 'app.datum()' AS value",
            "SELECT as\u{a0} 'app.datum()' AS value",
            "AS $$ SELECT 'app.datum()' $$",
            "AS 'SELECT $$app.datum()$$'",
        ] {
            assert!(!PG.code_only(datum).contains("app.datum"), "{datum}");
        }
    }

    #[test]
    fn native_language_operands_are_data_on_either_side_of_the_body() {
        for language in [
            "c",
            "internal",
            "C",
            "INTERNAL",
            "\"c\"",
            "'internal'",
            "U&\"c\"",
            "U&\"\\0063\"",
            "U&\"intern!0061l\" UESCAPE '!'",
            "E'\\x63'",
            "U&'intern!0061l' UESCAPE /* gap */ E'!'",
            "$lang$internal$lang$",
            "U&\"intern!0061l\" UESCAPE /* gap */ E'!'",
            "/* outer /* nested */ end */ c",
            "-- language name\r\ninternal",
        ] {
            for body in ["'app.native_symbol'", "$$app.native_symbol$$"] {
                for definition in [
                    format!("() RETURNS int LANGUAGE {language} AS {body}"),
                    format!("() RETURNS int AS {body} LANGUAGE {language}"),
                ] {
                    let code = PG.code_only(&definition);
                    assert!(!code.contains("native_symbol"), "{definition}: {code}");
                    assert_eq!(code.len(), definition.len());
                }
            }
        }
        for definition in [
            "() RETURNS int AS 'SELECT app.f(), ''LANGUAGE internal''' LANGUAGE sql",
            "() RETURNS int LANGUAGE sql AS $$ SELECT app.f(), 'LANGUAGE c' $$",
            "(language internal) RETURNS int LANGUAGE sql AS 'SELECT app.f()'",
            "() RETURNS int LANGUAGE sql AS 'SELECT app.f()' /* LANGUAGE c */",
            "() RETURNS int LANGUAGE sql AS 'SELECT app.f()' SET \"language\" = 'c'",
            "() RETURNS int LANGUAGE \"INTERNAL\" AS 'SELECT app.f()'",
            "() RETURNS int LANGUAGE 'C' AS 'SELECT app.f()'",
            "() RETURNS int LANGUAGE U&\"s!0071l\" UESCAPE '!' AS 'SELECT app.f()'",
        ] {
            assert!(PG.code_only(definition).contains("app.f()"), "{definition}");
        }
        // An unfinished declaration should still be safe to scan.
        assert_eq!(PG.code_only("LANGUAGE \""), "LANGUAGE \"");
    }

    #[test]
    fn continued_body_pieces_form_one_source_before_its_literals_are_blanked() {
        for gap in ["\n", "\r", "\r\n", " -- comment\n", " -- /* comment\r "] {
            let definition = format!(
                "() RETURNS text AS 'SELECT app.'{gap}'f(), ''app.'{gap}'datum()''' LANGUAGE sql"
            );
            let code = PG.code_only(&definition);
            assert!(code.contains("app.f()"), "{code}");
            assert!(!code.contains("datum()"), "{code}");
            assert_eq!(code.len(), definition.len());
            assert_eq!(code.find("LANGUAGE"), definition.find("LANGUAGE"));
            for newline in ['\n', '\r'] {
                assert_eq!(
                    code.matches(newline).count(),
                    definition.matches(newline).count()
                );
            }
        }
        for gap in [" ", "\t", " /* x */\n", "\n/* x */", "\u{a0}\n"] {
            let definition = format!("AS 'SELECT app.'{gap}'f()'");
            assert!(
                !PG.code_only(&definition).contains("app.f()"),
                "{definition}"
            );
        }
    }

    #[test]
    fn prefixed_bodies_decode_source_before_blanking_its_string_data() {
        for body in [
            r"E'SELECT app.\146(), \'app.datum()\''",
            r"E'SELECT app.\x66(), ''app.datum()'''",
            r"E'SELECT app.\u0066(), ''app.datum()'''",
            r"E'SELECT app.\U00000066(), ''app.datum()'''",
            "E'SELECT app.'\n'\\x66(), ''app.datum()'''",
            r"U&'SELECT app.\0066(), ''app.datum()'''",
            r"U&'SELECT app.!0066(), ''app.datum()''' UESCAPE /* gap */ E'!'",
        ] {
            let definition = format!("() RETURNS text AS {body} LANGUAGE sql");
            let code = PG.code_only(&definition);
            assert!(code.contains("app.f()"), "{definition}: {code}");
            assert!(!code.contains("datum()"), "{code}");
            assert_eq!(code.len(), definition.len());
            assert_eq!(code.find("LANGUAGE"), definition.find("LANGUAGE"));
        }
        // A decoded newline ends the body's comment even though its outer
        // spelling contains no physical newline.
        assert!(
            PG.code_only(r"AS E'-- comment\nSELECT app.f()'")
                .contains("app.f()")
        );
        let data = "AS $$ SELECT U&'app.'\n'f()' UESCAPE /* data clause */ E'!' $$";
        let code = PG.code_only(data);
        assert!(!code.contains("app."), "{code}");
        assert!(!code.contains("UESCAPE"), "{code}");
        assert_eq!(code.len(), data.len());
    }

    #[test]
    fn escape_decoding_preserves_bytes_piece_boundaries_and_unicode_pairs() {
        for (spelling, value) in [
            (r"E'\b\f\n\r\t\\\'\q'", "\u{8}\u{c}\n\r\t\\'q"),
            ("E'\\x'\n'63'", "x63"),
            ("E'\\xc3'\n'\\xa9'", "é"),
            (r"E'\uD83D\uDE00'", "😀"),
            (r"E'\U0001F600'", "😀"),
            (r"U&'!D83D!DE00' UESCAPE '!'", "😀"),
        ] {
            let literal = super::sql_string(spelling, 0).unwrap();
            assert_eq!(literal.contents, value, "{spelling}");
            assert_eq!(literal.end, spelling.len());
        }
        for invalid in [
            r"E'\x00'",
            r"E'\xff'",
            r"E'\uD800'",
            r"E'\uDC00'",
            r"E'\UFFFFFFFF'",
            r"U&'\0000'",
            r"U&'a' UESCAPE 'ab'",
            "E'unclosed",
            "B'0101'",
            "N'source'",
        ] {
            assert!(super::sql_string(invalid, 0).is_none(), "{invalid}");
        }
    }

    #[test]
    fn a_quoted_identifier_is_kept_and_a_bracket_is_only_a_quote_where_it_quotes() {
        let text = "SELECT (ARRAY['a]b'])[1], \"es\".\"a\"";
        assert_eq!(PG.code_only(text), blanked(text, &["'a]b'"]));
        let bracketed = "SELECT [dbo].[a]]b], 'x'";
        assert_eq!(MSSQL.code_only(bracketed), blanked(bracketed, &["'x'"]));
    }

    /// Measured: `FROM dq.U&"\007a"` and `FROM U&"dq".U&"!007a" UESCAPE '!'`
    /// select from `dq.z`, so the scan sees the name and not its spelling —
    /// at the same offsets, the difference padded. A spelling that does not
    /// decode, and a `u&` that continues a name, are left as they are; and
    /// SQL Server has no such form.
    #[test]
    fn a_unicode_escaped_identifier_is_read_as_the_name_it_spells() {
        assert_eq!(
            PG.code_only("SELECT * FROM app.U&\"\\007a\" WHERE 1"),
            "SELECT * FROM app.\"z\"       WHERE 1"
        );
        assert_eq!(
            PG.code_only("FROM u&\"app\".U&\"!007a\" UESCAPE '!' x"),
            "FROM \"app\"  .\"z\"                   x"
        );
        // A doubled quote inside, and an escape that spells a quote: both
        // are one quote in the name, spelled doubled again.
        assert_eq!(PG.code_only("U&\"a\"\"\\0022b\""), "\"a\"\"\"\"b\"     ");
        // A line break in the span survives, at the end of it.
        assert_eq!(
            PG.code_only("U&\"!007a\"\nUESCAPE '!' x"),
            "\"z\"                 \n x"
        );
        // Not a Unicode-escaped identifier: a name that ends in `u&` is an
        // operator's operand, and a `\` that spells nothing does not decode.
        assert_eq!(PG.code_only("xu&\"a\""), "xu&\"a\"");
        assert_eq!(PG.code_only("U&\"\\00G1\""), "U&\"\\00G1\"");
        assert_eq!(
            MSSQL.code_only("SELECT * FROM U&\"\\007a\""),
            "SELECT * FROM U&\"\\007a\""
        );
    }

    /// Measured: `N'x'`, `B'101'`, `X'1F'`, `U&'d\0061ta'` and `E'y'` are
    /// literals, and `note'x'` is the type `note` applied to a string. The
    /// prefix goes with its literal; a name before a literal stays.
    #[test]
    fn a_literals_prefix_is_blanked_with_it_and_a_name_before_one_is_not() {
        let text = "SELECT N'x', b'101', X'1F', u&'d\\0061ta', E'y', note'x', áE'a', e";
        assert_eq!(
            PG.code_only(text),
            blanked(
                text,
                &[
                    "N'x'",
                    "b'101'",
                    "X'1F'",
                    "u&'d\\0061ta'",
                    "E'y'",
                    "'x'",
                    "'a'"
                ]
            )
        );
        // SQL Server has the national prefix and nothing else.
        let tsql = "SELECT N'x', E'y'";
        assert_eq!(MSSQL.code_only(tsql), blanked(tsql, &["N'x'", "'y'"]));
    }

    /// A `UESCAPE 'x'` belongs to the `U&'…'` it follows: measured,
    /// `U&'d!0061ta' UESCAPE '!'` is the string `data`. Left as code the word
    /// matched a module named `uescape`, drew an edge that was not there, and
    /// with the real edge the other way a cycle put the dependent first.
    #[test]
    fn a_uescape_clause_is_part_of_the_literal_it_follows() {
        let text = "SELECT U&'d!0061ta' UESCAPE '!' AS s FROM es.z";
        assert_eq!(
            PG.code_only(text),
            blanked(text, &["U&'d!0061ta' UESCAPE '!'"])
        );
        // The keyword is the engine's, in any case: measured, a lower-cased
        // `uescape` is the same clause — and that is the spelling a module
        // named `uescape` collides with at every case pass.
        let lower = "SELECT U&'d!0061ta' uescape '!' AS s";
        assert_eq!(
            PG.code_only(lower),
            blanked(lower, &["U&'d!0061ta' uescape '!'"])
        );
        // The clause may be on the next line, and the break survives.
        let wrapped = "SELECT U&'x'
UESCAPE '!' AS s";
        assert_eq!(
            PG.code_only(wrapped),
            blanked(wrapped, &["U&'x'", "UESCAPE '!'"])
        );
        // Only after a Unicode string: a plain literal is followed by code,
        // and a name that merely starts with the word is a name.
        let plain = "SELECT 'x' UESCAPE FROM es.uescape";
        assert_eq!(PG.code_only(plain), blanked(plain, &["'x'"]));
        let longer = "SELECT U&'x' uescapes FROM t";
        assert_eq!(PG.code_only(longer), blanked(longer, &["U&'x'"]));
        // An escape the engine refuses is no clause: measured, a quote, a
        // `+`, a hex digit and whitespace are all refused. The word then
        // stays code and what follows it is read as the ordinary literal it
        // lexically is — the statement is one the engine refuses either way.
        for quoted in ["'+'", "'a'", "''''"] {
            let text = format!("SELECT U&'x' UESCAPE {quoted}");
            assert_eq!(
                PG.code_only(&text),
                blanked(&text, &["U&'x'", quoted]),
                "{text}"
            );
        }
        // A doubled quote inside the literal is a quote of its own, and the
        // clause still follows the real closing quote: measured,
        // `U&'a''b' uescape '!'` is the string `a'b`.
        let doubled = "SELECT U&'a''b' AS s";
        assert_eq!(PG.code_only(doubled), blanked(doubled, &["U&'a''b'"]));
        let both = "SELECT U&'a''b' uescape '!' AS s FROM es.z";
        assert_eq!(PG.code_only(both), blanked(both, &["U&'a''b' uescape '!'"]));
        // And a plain literal's doubled quote opens no lookahead at all.
        let plain_doubled = "SELECT 'a''b' uescape '!' AS s";
        assert_eq!(
            PG.code_only(plain_doubled),
            blanked(plain_doubled, &["'a''b'", "'!'"])
        );
        // SQL Server has no such literal, so the word stays code.
        let tsql = "SELECT N'x' UESCAPE";
        assert_eq!(MSSQL.code_only(tsql), blanked(tsql, &["N'x'"]));
    }

    #[test]
    fn comments_are_blanked_and_line_structure_survives() {
        let text = "SELECT 1 -- es.a\nFROM /* a /* es.a */ c */ es.b";
        assert_eq!(
            PG.code_only(text),
            blanked(text, &["-- es.a", "/* a /* es.a */ c */"])
        );
        for open in ["'unterminated", "E'x\\'", "/* open", "\"open"] {
            assert_eq!(PG.code_only(open).len(), open.len(), "{open}");
        }
    }
}
