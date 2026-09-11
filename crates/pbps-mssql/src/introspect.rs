//! Reading a live database back into the model — the heart of `pbps pull`.
//!
//! # Shape
//!
//! The catalog queries and the model assembly are deliberately separated: the
//! `Raw*` structs are plain data mirroring the catalog views, [`assemble`] is a
//! pure function from them to a [`Schema`], and only [`introspect`] touches a
//! connection. Everything that can be wrong here — a type read back differently
//! than declared, an index losing its INCLUDE columns — is in `assemble`, and a
//! pure `assemble` can be pinned by tests without a server in the room.
//!
//! # What cannot be expressed
//!
//! The model does not cover everything a database can hold (computed columns,
//! clustered-ness, collations). Those are **reported, never silently dropped**:
//! a `pull` that quietly loses a computed column would produce declarations that
//! plan the column's destruction on the next run. The caller decides whether the
//! warnings are acceptable.

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::DialectError;
use pbps_model::{
    CheckConstraint, Column, ColumnType, ForeignKey, Identity, Index, IndexColumn, Module,
    ObjectName, PrimaryKey, ReferentialAction, Schema, Table, TableName, TypeArg, UniqueConstraint,
};

use crate::types;

// The shapes a pull returns are `pbps-db`'s, filled here from `sys.*` and
// re-exported under the paths this crate's callers and tests have always used
// (DECISIONS 417).
pub use pbps_db::catalog::{Limitation, LimitationTarget, Pulled, Unexpressible, UnmanagedModule};

/// One row of `sys.tables`.
#[derive(Debug, Clone)]
pub struct RawTable {
    pub object_id: i32,
    pub schema: String,
    pub name: String,
    /// `sys.tables.temporal_type`: zero is ordinary, one history, two versioned.
    pub temporal_type: u8,
    /// Whether `sys.periods` still defines `PERIOD FOR SYSTEM_TIME` on the table.
    pub has_period: bool,
}

/// One row of `sys.columns`, joined with its type, identity and default.
#[derive(Debug, Clone)]
pub struct RawColumn {
    pub object_id: i32,
    pub name: String,
    /// The type name as `sys.types` reports it.
    pub type_name: String,
    /// Bytes, not characters; `-1` for the `max` forms.
    pub max_length: i16,
    pub precision: u8,
    pub scale: u8,
    pub is_nullable: bool,
    pub is_computed: bool,
    /// Whether the type is a user-defined alias type.
    pub is_user_defined_type: bool,
    /// `(seed, increment)` when the column is IDENTITY.
    pub identity: Option<(i64, i64)>,
    /// The default definition as stored, wrapped in parentheses.
    pub default: Option<String>,
}

/// One column of a PRIMARY KEY or UNIQUE constraint, in key order.
#[derive(Debug, Clone)]
pub struct RawKeyColumn {
    pub object_id: i32,
    pub constraint_name: String,
    pub is_primary: bool,
    pub column: String,
}

/// One column pair of a foreign key, in constraint-column order.
#[derive(Debug, Clone)]
pub struct RawForeignKeyColumn {
    pub object_id: i32,
    pub constraint_name: String,
    pub ref_schema: String,
    pub ref_table: String,
    pub column: String,
    pub ref_column: String,
    /// `sys.foreign_keys.delete_referential_action`: 0..=3.
    pub on_delete: u8,
    pub on_update: u8,
}

/// One row of `sys.check_constraints`.
#[derive(Debug, Clone)]
pub struct RawCheck {
    pub object_id: i32,
    pub name: String,
    pub definition: String,
}

/// The physical kind of an index, as `sys.indexes.type` reports it.
///
/// The declarations hold one kind of index: a nonclustered rowstore one. Every
/// other kind is a different physical object with the same catalog shape, so
/// the code travels rather than a `is_clustered` yes/no. Read as "clustered or
/// not", a columnstore, XML, spatial or hash index answered "not clustered"
/// and was adopted as a plain index it is not — bootstrapping a B-tree where
/// the database had a columnstore, or emitting a `CREATE INDEX` the engine
/// rejects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    /// `type = 2`: nonclustered rowstore, the only kind [`pbps_model::Index`]
    /// expresses.
    Nonclustered,
    /// Any other type, carried whole so the limitation can name what it was.
    /// A type this build does not know about lands here too: an unknown
    /// physical kind is not evidence of an ordinary one.
    Unmodelled(u8),
}

impl IndexKind {
    pub fn from_type_code(code: u8) -> Self {
        match code {
            2 => Self::Nonclustered,
            other => Self::Unmodelled(other),
        }
    }
}

/// How an unmodelled index type is named to the operator.
///
/// The codes are `sys.indexes.type`. An unrecognised one is reported by number
/// rather than guessed at: the operator can look it up, and a wrong name would
/// send them after the wrong object.
fn index_type_name(code: u8) -> String {
    match code {
        1 => "clustered".to_owned(),
        3 => "an XML index".to_owned(),
        4 => "a spatial index".to_owned(),
        5 => "a clustered columnstore index".to_owned(),
        6 => "a nonclustered columnstore index".to_owned(),
        7 => "a hash index".to_owned(),
        other => format!("of `sys.indexes.type` {other}"),
    }
}

/// One column of an index that is not backing a PK or UNIQUE constraint.
#[derive(Debug, Clone)]
pub struct RawIndexColumn {
    pub object_id: i32,
    pub index_name: String,
    pub is_unique: bool,
    pub kind: IndexKind,
    pub filter: Option<String>,
    pub column: String,
    pub is_included: bool,
    pub is_descending: bool,
}

/// One view, procedure, function or trigger, as `sys.objects` and
/// `sys.sql_modules` report it (ADR-0002).
#[derive(Debug, Clone)]
pub struct RawModule {
    pub schema: String,
    pub name: String,
    pub kind: ModuleKind,
    /// The definition as stored — the whole `CREATE ...` text, verbatim.
    ///
    /// `None` for a CLR object and for one created `WITH ENCRYPTION`: neither
    /// has a readable definition, and neither can be managed.
    pub definition: Option<String>,
    /// A trigger's table, as `(schema, table)`.
    pub parent: Option<(String, String)>,
    /// Whether the module was created with `QUOTED_IDENTIFIER` and `ANSI_NULLS`
    /// both ON, which is what a `CREATE OR ALTER` sent by pbps will run under.
    ///
    /// SQL Server persists these two with the module and re-applies them on
    /// every execution, so a module created with either OFF behaves differently
    /// from the same text recreated by pbps — double-quoted tokens become string
    /// literals, or `= NULL` starts matching rows. The model has nowhere to keep
    /// them (they are options, not definition), so such a module is inventoried
    /// rather than claimed to round-trip (ADR-0002).
    pub default_set_options: bool,
}

/// One user-defined database role, as `sys.database_principals` reports it
/// (ADR-0005).
#[derive(Debug, Clone)]
pub struct RawRole {
    pub name: String,
}

/// What a permission is on, as far as the catalog could name it.
///
/// A class 1 or class 3 row whose joined name came back NULL used to be
/// dropped, on the reading that it was a dropped object's orphaned row. There
/// is a second reading, and it is the common one: `sys.objects` and
/// `sys.schemas` are subject to metadata visibility, so an object the
/// connected principal holds nothing on — or has `DENY VIEW DEFINITION` on —
/// yields a NULL name while its `sys.database_permissions` row still arrives.
/// The grant exists and is being read; only its securable cannot be named. Read
/// as an absence, `pull` wrote a role narrower than the database holds and the
/// next `plan` proposed a `REVOKE` of a permission nobody removed — absent,
/// empty and unreadable being three different things.
///
/// Neither reading can be told from the other in the joined row, and neither
/// changes what the tool may do: a securable it cannot name is one it cannot
/// compare, so it is reported rather than dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Securable {
    /// Class 1: an object, in the schema that owns it.
    Object { schema: String, name: String },
    /// Class 3: a schema.
    Schema(String),
    /// Class 1 or 3 whose name the connection could not read.
    Unreadable,
    /// A class with no name to read: the database itself, and the classes the
    /// model does not hold (a type, an assembly, another principal).
    Unnamed,
}

/// One permission row of `sys.database_permissions` granted to a role, on an
/// object or a schema.
#[derive(Debug, Clone)]
pub struct RawPermission {
    pub role: String,
    /// `sys.database_permissions.class`: 1 for an object, 3 for a schema, 0
    /// for the database itself, and the rest for classes the model does not
    /// hold (a type, an assembly, another principal, ...).
    pub class: u8,
    /// The catalog's own name for the class (`DATABASE`, `OBJECT_OR_COLUMN`,
    /// `TYPE`), for the report of one the model does not hold.
    pub class_desc: String,
    /// The permission name as the catalog spells it (`SELECT`, `VIEW
    /// DEFINITION`).
    pub permission: String,
    /// `G` granted, `W` granted with grant option, `D` denied, `R` revoked.
    pub state: String,
    /// What the permission is on, as far as the catalog could name it.
    pub securable: Securable,
    /// `sys.database_permissions.minor_id`: non-zero for a column-level
    /// permission, which the model does not hold.
    pub minor_id: i32,
}

/// Maps `sys.objects.type` codes onto the model's kinds.
///
/// Returns `None` for anything that is not a module, so an unknown code is
/// skipped rather than mislabelled.
pub fn kind_from_type_code(code: &str) -> Option<ModuleKind> {
    match code.trim() {
        "V" => Some(ModuleKind::View),
        "P" | "PC" => Some(ModuleKind::Procedure),
        "FN" | "IF" | "TF" | "FS" | "FT" => Some(ModuleKind::Function),
        "TR" => Some(ModuleKind::Trigger),
        _ => None,
    }
}

pub use pbps_model::ModuleKind;

/// Everything read from one database.
#[derive(Debug, Clone, Default)]
pub struct RawCatalog {
    pub tables: Vec<RawTable>,
    pub columns: Vec<RawColumn>,
    pub key_columns: Vec<RawKeyColumn>,
    pub foreign_key_columns: Vec<RawForeignKeyColumn>,
    pub checks: Vec<RawCheck>,
    pub index_columns: Vec<RawIndexColumn>,
    pub modules: Vec<RawModule>,
    pub roles: Vec<RawRole>,
    pub permissions: Vec<RawPermission>,
}

fn push_limitation(
    warnings: &mut Vec<String>,
    limitations: &mut Vec<Limitation>,
    table: Option<&TableName>,
    detail: String,
) {
    warnings.push(detail.clone());
    if let Some(table) = table {
        limitations.push(Limitation {
            target: LimitationTarget::Relation(table.clone()),
            detail,
        });
    }
}

/// Splits a stored definition back into the body a declaration carries.
///
/// # Why this is not parsing SQL
///
/// It reads exactly as far as the emitter writes and no further: the
/// `CREATE [OR ALTER] <kind> <name>` prefix, plus a view's `AS` and a trigger's
/// `ON <table>`. Everything after that is returned untouched. For anything pbps
/// wrote the round trip is exact, because [`crate::emit::module_definition`]
/// produced that prefix; for a hand-written module it is a best-effort read,
/// and when the scan meets something it cannot account for it returns `None`
/// rather than guessing — the module is then reported as unmanaged, which is
/// the rule `pull` already follows for everything it cannot express.
///
/// A view with options between its name and `AS` (`WITH SCHEMABINDING`) is one
/// of those cases. The model has nowhere to keep them, and dropping them
/// silently would have the next apply recreate the view *without*
/// SCHEMABINDING — a change nobody asked for, in the one direction that
/// removes a guarantee.
/// `has_parent` says whether the catalog can supply the trigger's target. When
/// it can, an unqualified `ON customer` is no obstacle: the schema the text
/// omits is exactly what `sys.objects` records, and `assemble` substitutes it.
/// Refusing there would inventory as unmanageable the commonest spelling of a
/// perfectly reproducible trigger.
pub fn split_module(
    kind: ModuleKind,
    stored: &str,
    has_parent: bool,
) -> Option<(Option<TableName>, String)> {
    let s = stored;
    let mut i = keyword(s, 0, "create")?;
    // `OR ALTER` is optional: pbps emits it, a hand-written module has not.
    if let Some(next) = keyword(s, i, "or") {
        i = keyword(s, next, "alter")?;
    }
    i = keyword_prefix(
        s,
        i,
        match kind {
            ModuleKind::View => "view",
            ModuleKind::Procedure => "proc",
            ModuleKind::Function => "function",
            ModuleKind::Trigger => "trigger",
        },
    )?;

    let (_, after_name) = qualified_name(s, skip_ws(s, i))?;
    let mut i = skip_ws(s, after_name);

    match kind {
        ModuleKind::View => {
            // Anything but `AS` here is an option the model cannot hold.
            let at = keyword(s, i, "as")?;
            Some((None, s[at..].trim().to_owned()))
        }
        ModuleKind::Trigger => {
            i = keyword(s, i, "on")?;
            let (table, after) = qualified_name(s, skip_ws(s, i))?;
            let on = match table.as_slice() {
                [schema, name] => Some(TableName::new(schema.clone(), name.clone())),
                // An unqualified table means the default schema of whoever
                // created it, which the definition text does not record — so it
                // is readable only when the catalog can answer instead.
                [_] if has_parent => None,
                _ => return None,
            };
            Some((on, s[after..].trim().to_owned()))
        }
        ModuleKind::Procedure | ModuleKind::Function => {
            Some((None, s[after_name..].trim().to_owned()))
        }
    }
}

/// Whitespace plus the two comment forms, which may sit anywhere the emitter's
/// prefix has a gap.
fn skip_ws(s: &str, mut i: usize) -> usize {
    loop {
        let rest = &s[i..];
        let trimmed = rest.trim_start();
        i = s.len() - trimmed.len();
        if trimmed.starts_with("--") {
            // A bare carriage return ends a line comment on this engine, not
            // just a line feed (PITFALLS, "A comment ends at a carriage
            // return"). Waiting for `\n` alone swallowed the rest of a
            // CR-terminated header, so the `AS` was never found and a module
            // that reads perfectly well was inventoried as unmanageable.
            i += trimmed.find(['\n', '\r']).map_or(trimmed.len(), |n| n + 1);
            continue;
        }
        if trimmed.starts_with("/*") {
            match block_comment(trimmed) {
                Some(end) => i += end,
                None => return s.len(),
            }
            continue;
        }
        return i;
    }
}

/// The length of the block comment `s` opens, both delimiters included, or
/// `None` when it is never closed.
///
/// Block comments nest here — measured, and recorded in `pbps-dialect`'s own
/// scanner: `SELECT /* a /* b */ c */ 1` returns 1 — so the outer comment ends
/// at the terminator that matches its opener and not at the first `*/`. Ending
/// early read the rest of the outer comment as code, where an `ON` inside a
/// comment could be taken for a trigger's target.
///
/// Stepping two bytes past each delimiter is what keeps `/*/` from reading as
/// an opener that also closes itself. Bytes rather than characters is safe
/// because neither delimiter's bytes can occur inside a multi-byte character,
/// so every offset it returns is a character boundary.
fn block_comment(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        match (bytes[i], bytes[i + 1]) {
            (b'/', b'*') => {
                depth += 1;
                i += 2;
            }
            // `depth > 0` guards the subtraction rather than trusting the
            // caller to have found an opener first: an unmatched `*/` makes
            // this read no comment at all, where a wrapping subtraction would
            // make it read to the end of the definition.
            (b'*', b'/') if depth > 0 => {
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

/// Matches one keyword case-insensitively, skipping whatever whitespace and
/// comments precede it, and returns the offset just after it.
///
/// A keyword has to end at a word boundary, or `VIEWS` would match `VIEW` and
/// the split would take the rest of a word for the object's name.
fn keyword(s: &str, i: usize, word: &str) -> Option<usize> {
    let i = skip_ws(s, i);
    let rest = &s[i..];
    if rest.len() < word.len() || !rest[..word.len()].eq_ignore_ascii_case(word) {
        return None;
    }
    let after = i + word.len();
    match s[after..].chars().next() {
        Some(c) if c.is_alphanumeric() || c == '_' => None,
        _ => Some(after),
    }
}

/// As [`keyword`], but the word may continue: `PROC` is a legal abbreviation of
/// `PROCEDURE`, and both have to be accepted from a hand-written module.
fn keyword_prefix(s: &str, i: usize, prefix: &str) -> Option<usize> {
    let i = skip_ws(s, i);
    let rest = &s[i..];
    if rest.len() < prefix.len() || !rest[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return None;
    }
    Some(skip_word_tail(s, i + prefix.len()))
}

/// Steps over whatever is left of an identifier-shaped word.
fn skip_word_tail(s: &str, i: usize) -> usize {
    s[i..]
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map_or(s.len(), |n| i + n)
}

/// Reads `a`, `a.b` or `[a].[b]`, returning the parts and the offset after.
fn qualified_name(s: &str, mut i: usize) -> Option<(Vec<String>, usize)> {
    let mut parts = Vec::new();
    loop {
        let rest = &s[i..];
        // `"name"` is the ANSI spelling of `[name]` and means the same thing
        // under QUOTED_IDENTIFIER ON, which is the only setting pbps manages
        // (see `RawModule::default_set_options`). Refusing it would inventory a
        // perfectly reproducible module as unmanageable over a choice of quote.
        let quoted = rest
            .strip_prefix('[')
            .map(|r| (r, ']'))
            .or_else(|| rest.strip_prefix('"').map(|r| (r, '"')));
        if let Some((stripped, close)) = quoted {
            // A doubled closing character is an escaped one inside the name.
            let mut part = String::new();
            let mut chars = stripped.char_indices();
            let end = loop {
                let (at, ch) = chars.next()?;
                if ch == close {
                    match stripped[at + ch.len_utf8()..].starts_with(close) {
                        true => {
                            part.push(close);
                            chars.next();
                        }
                        false => break at,
                    }
                } else {
                    part.push(ch);
                }
            };
            parts.push(part);
            i += end + 1 + close.len_utf8();
        } else {
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '@' || c == '#'))
                .unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            parts.push(rest[..end].to_owned());
            i += end;
        }
        let after_part = i;
        let j = skip_ws(s, i);
        if s[j..].starts_with('.') {
            i = skip_ws(s, j + 1);
            continue;
        }
        return Some((parts, after_part));
    }
}

/// Rebuilds the declared type from what the catalog stores.
///
/// The catalog does not keep the spelling the user wrote; it keeps the type id
/// plus lengths. This mapping plus [`types::normalize`] is what makes a pulled
/// schema comparable with a declared one.
fn column_type(c: &RawColumn) -> Result<ColumnType, DialectError> {
    let name = c.type_name.to_ascii_lowercase();
    let ty = match name.as_str() {
        // max_length is bytes; the n-types store UTF-16, two bytes a character.
        "nchar" | "nvarchar" => {
            if c.max_length == -1 {
                ColumnType::new(name, vec![TypeArg::Max])
            } else {
                ColumnType::new(name, vec![TypeArg::Int(i64::from(c.max_length) / 2)])
            }
        }
        "char" | "varchar" | "binary" | "varbinary" => {
            if c.max_length == -1 {
                ColumnType::new(name, vec![TypeArg::Max])
            } else {
                ColumnType::new(name, vec![TypeArg::Int(i64::from(c.max_length))])
            }
        }
        "decimal" | "numeric" => ColumnType::new(
            name,
            vec![
                TypeArg::Int(i64::from(c.precision)),
                TypeArg::Int(i64::from(c.scale)),
            ],
        ),
        "datetime2" | "datetimeoffset" | "time" => {
            ColumnType::new(name, vec![TypeArg::Int(i64::from(c.scale))])
        }
        _ => ColumnType::simple(name),
    };
    types::normalize(&ty)
}

/// Strips the parentheses the engine wraps around stored expressions.
///
/// A default declared as `0` is stored as `((0))`; a filter declared as
/// `a IS NOT NULL` comes back `([a] IS NOT NULL)`. Only *whole-string balanced*
/// pairs are removed, so `(a) AND (b)` is untouched — peeling that would change
/// its meaning.
pub fn strip_stored_parens(s: &str) -> &str {
    let mut s = s.trim();
    while s.starts_with('(') && s.ends_with(')') {
        let inner = &s[1..s.len() - 1];
        let mut depth = 0i32;
        // The outer pair is removable only if it closes at the very end.
        if inner.chars().all(|ch| {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            depth >= 0
        }) && depth == 0
        {
            s = inner.trim();
        } else {
            break;
        }
    }
    s
}

fn action(code: u8) -> ReferentialAction {
    match code {
        1 => ReferentialAction::Cascade,
        2 => ReferentialAction::SetNull,
        3 => ReferentialAction::SetDefault,
        _ => ReferentialAction::NoAction,
    }
}

/// Assembles the raw catalog rows into a [`Schema`].
///
/// Rows referring to an object id that is not in `tables` are ignored: the
/// queries fetch the whole database, and the table list is what defines the
/// managed set.
pub fn assemble(raw: &RawCatalog) -> Pulled {
    let mut warnings = Vec::new();
    let mut unexpressible: Vec<Unexpressible> = Vec::new();
    let mut limitations = Vec::new();
    let mut names: BTreeMap<i32, TableName> = BTreeMap::new();
    let mut tables: BTreeMap<i32, Table> = BTreeMap::new();
    let mut unsupported_temporal_tables = BTreeSet::new();

    for t in &raw.tables {
        // Both halves of active versioning, and a current table whose period
        // remains after versioning is disabled, must stay unmanaged. Declaring
        // any of them as ordinary loses temporal semantics on bootstrap.
        if t.temporal_type != 0 || t.has_period {
            let name = TableName::new(t.schema.clone(), t.name.clone());
            unsupported_temporal_tables.insert(name.clone());
            push_limitation(
                &mut warnings,
                &mut limitations,
                Some(&name),
                format!(
                    "{name}: system versioning or PERIOD FOR SYSTEM_TIME (temporal_type = {}, has_period = {}) is not supported yet; the table was left out of the declarations",
                    t.temporal_type, t.has_period
                ),
            );
            continue;
        }
        names.insert(
            t.object_id,
            TableName::new(t.schema.clone(), t.name.clone()),
        );
        tables.insert(t.object_id, Table::default());
    }
    let name_of = |id: i32, names: &BTreeMap<i32, TableName>| {
        names
            .get(&id)
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("object {id}"))
    };

    for c in &raw.columns {
        let Some(table) = tables.get_mut(&c.object_id) else {
            continue;
        };
        let table_name = name_of(c.object_id, &names);

        if c.is_computed {
            push_limitation(
                &mut warnings,
                &mut limitations,
                names.get(&c.object_id),
                format!(
                    "{table_name}.{}: computed columns are not supported yet; it was left out of the declarations",
                    c.name
                ),
            );
            continue;
        }
        if c.is_user_defined_type {
            push_limitation(
                &mut warnings,
                &mut limitations,
                names.get(&c.object_id),
                format!(
                    "{table_name}.{}: user-defined type `{}` is not supported yet; it was left out of the declarations",
                    c.name, c.type_name
                ),
            );
            continue;
        }
        let ty = match column_type(c) {
            Ok(t) => t,
            Err(e) => {
                push_limitation(
                    &mut warnings,
                    &mut limitations,
                    names.get(&c.object_id),
                    format!(
                        "{table_name}.{}: {e}; it was left out of the declarations",
                        c.name
                    ),
                );
                continue;
            }
        };

        let mut column = Column::new(ty);
        column.nullable = c.is_nullable;
        column.identity = c
            .identity
            .map(|(seed, increment)| Identity { seed, increment });
        column.default = c
            .default
            .as_deref()
            .map(|d| strip_stored_parens(d).to_owned());
        table.columns.insert(c.name.clone(), column);
    }

    for k in &raw.key_columns {
        let Some(table) = tables.get_mut(&k.object_id) else {
            continue;
        };
        if k.is_primary {
            let pk = table.primary_key.get_or_insert_with(|| PrimaryKey {
                name: Some(k.constraint_name.clone()),
                columns: Vec::new(),
            });
            pk.columns.push(k.column.clone());
        } else {
            table
                .unique
                .entry(k.constraint_name.clone())
                .or_insert_with(|| UniqueConstraint {
                    columns: Vec::new(),
                })
                .columns
                .push(k.column.clone());
        }
    }

    for f in &raw.foreign_key_columns {
        let Some(table) = tables.get_mut(&f.object_id) else {
            continue;
        };
        let fk = table
            .foreign_keys
            .entry(f.constraint_name.clone())
            .or_insert_with(|| ForeignKey {
                columns: Vec::new(),
                references_table: TableName::new(f.ref_schema.clone(), f.ref_table.clone()),
                references_columns: Vec::new(),
                on_delete: action(f.on_delete),
                on_update: action(f.on_update),
            });
        fk.columns.push(f.column.clone());
        fk.references_columns.push(f.ref_column.clone());
    }

    for c in &raw.checks {
        let Some(table) = tables.get_mut(&c.object_id) else {
            continue;
        };
        table.checks.insert(
            c.name.clone(),
            CheckConstraint {
                expression: strip_stored_parens(&c.definition).to_owned(),
            },
        );
    }

    let mut unmodelled_indexes = BTreeSet::new();
    for i in &raw.index_columns {
        let Some(table) = tables.get_mut(&i.object_id) else {
            continue;
        };
        if let IndexKind::Unmodelled(code) = i.kind {
            // The model holds a nonclustered rowstore index and nothing else;
            // recording any other kind without what makes it that kind would
            // make bootstrap create a different physical object — or, for the
            // kinds `CREATE INDEX` cannot spell at all, a statement the engine
            // rejects.
            let table_name = name_of(i.object_id, &names);
            // One catalog row is returned per index column. Deduplicate those
            // rows by the owning object as well as the index name: SQL Server
            // permits two tables to use the same index name.
            if unmodelled_indexes.insert((i.object_id, i.index_name.clone())) {
                push_limitation(
                    &mut warnings,
                    &mut limitations,
                    names.get(&i.object_id),
                    format!(
                        "{table_name}: index `{}` is {}, which is not modelled yet; it was left out of the declarations",
                        i.index_name,
                        index_type_name(code)
                    ),
                );
            }
            continue;
        }
        let index = table
            .indexes
            .entry(i.index_name.clone())
            .or_insert_with(|| Index {
                columns: Vec::new(),
                include: Vec::new(),
                unique: i.is_unique,
                filter: i
                    .filter
                    .as_deref()
                    .map(|f| strip_stored_parens(f).to_owned()),
            });
        if i.is_included {
            index.include.push(i.column.clone());
        } else {
            index.columns.push(IndexColumn {
                name: i.column.clone(),
                descending: i.is_descending,
            });
        }
    }

    let mut schema = Schema::default();
    for (id, table) in tables {
        // A table whose every column was unsupported must not be declared as an
        // empty table — that would plan the drop of the columns it really has.
        if table.columns.is_empty() {
            let table_name = name_of(id, &names);
            push_limitation(
                &mut warnings,
                &mut limitations,
                names.get(&id),
                format!(
                    "{table_name}: no supported columns remain; the whole table was left out of the declarations"
                ),
            );
            continue;
        }
        schema.tables.insert(names.remove(&id).unwrap(), table);
    }

    // Modules (ADR-0002). Each one either becomes part of the desired state or
    // is inventoried as unmanaged with the reason — never quietly dropped, for
    // the same reason a computed column is not: a module missing from the
    // declarations is a module the next plan would propose destroying.
    let mut unmanaged_modules: Vec<UnmanagedModule> = Vec::new();
    for m in &raw.modules {
        let name = ObjectName::new(m.schema.clone(), m.name.clone());
        let mut unmanageable = |why: &str| {
            unmanaged_modules.push(UnmanagedModule {
                kind: m.kind.as_str(),
                target: LimitationTarget::SharedModule(name.clone()),
                why: why.to_owned(),
            });
        };

        let Some(stored) = &m.definition else {
            unmanageable(
                "its definition cannot be read back (a CLR object, or created WITH ENCRYPTION)",
            );
            continue;
        };
        if !m.default_set_options {
            unmanageable(
                "it was created with QUOTED_IDENTIFIER or ANSI_NULLS OFF, which pbps cannot \
                 restate — recreating it would change how it behaves",
            );
            continue;
        }
        let Some((on, definition)) = split_module(m.kind, stored, m.parent.is_some()) else {
            unmanageable(
                "its definition is not of a shape pbps can reproduce (a view with options such \
                 as SCHEMABINDING, or a trigger on an unqualified table)",
            );
            continue;
        };
        // The catalog knows a trigger's table even when the definition text
        // does not qualify it, so the more reliable answer wins.
        let on = m
            .parent
            .as_ref()
            .map(|(s, t)| ObjectName::new(s.clone(), t.clone()))
            .or(on);

        // A trigger declaration requires its parent in the managed schema.
        // Keep the temporal table's dependent trigger in the same inventory
        // instead of writing a declaration that validation cannot load.
        if m.kind == ModuleKind::Trigger
            && on
                .as_ref()
                .is_some_and(|parent| unsupported_temporal_tables.contains(parent))
        {
            unmanageable(
                "its parent table uses system versioning or PERIOD FOR SYSTEM_TIME, which pbps cannot express",
            );
            continue;
        }

        // The identity, which for a trigger is its table and its own name
        // (ADR-0009 §1). Nothing on this engine overloads, so no read-back
        // ever carries a signature.
        let id = match (m.kind, &on) {
            (pbps_model::ModuleKind::Trigger, Some(table)) => pbps_model::ModuleId::Trigger {
                on: table.clone(),
                name: name.name.clone(),
            },
            _ => pbps_model::ModuleId::Named(name.clone()),
        };
        // The id crosses a snapshot and a plan as its string form, whose
        // punctuation is structural: `.` separates the parts and `(` opens a
        // signature. A legal quoted identifier may contain either —
        // `[audit.v1]`, `[sales(archive)]` — and such an id would be written
        // faithfully and read back as a *different* module: a trigger on
        // `dbo.audit`, a routine with an argument. Before the typed id that
        // read failed loudly; now it would succeed wrongly, so the round trip
        // is checked here, at the one place engine names enter the model, and
        // a name that does not survive it is inventoried like any other shape
        // the format cannot carry.
        if id.to_string().parse::<pbps_model::ModuleId>().as_ref() != Ok(&id) {
            unmanageable(
                "its name contains a period or a parenthesis, which a declaration cannot spell \
                 (the tool would read it back as a different module)",
            );
            continue;
        }
        schema.modules.insert(
            id,
            Module {
                kind: m.kind,
                // A description lives in the declarations, not in the database;
                // pulling one back is not possible and pretending otherwise
                // would make every pulled module compare unequal.
                description: None,
                definition,
            },
        );
    }
    unmanaged_modules.sort();
    unmanaged_modules.dedup();

    // Roles (ADR-0005). Every user-defined role is read; the managed-set cut
    // happens later, by the ids file. What the model cannot hold — a DENY, a
    // column-level grant, a permission outside the closed set, a grant with
    // GRANT OPTION — is reported, never dropped: each is a difference the
    // next plan would otherwise revoke or fail to see.
    for r in &raw.roles {
        schema
            .roles
            .insert(r.name.clone(), pbps_model::Role::default());
    }
    for p in &raw.permissions {
        let Some(role) = schema.roles.get_mut(&p.role) else {
            continue;
        };
        let target = match (&p.securable, p.class) {
            (Securable::Object { schema, name }, _) => {
                pbps_model::GrantTarget::Object(ObjectName::new(schema.clone(), name.clone()))
            }
            (Securable::Schema(schema), _) => pbps_model::GrantTarget::Schema(schema.clone()),
            // The grant is there and readable; only its securable is not. It
            // cannot be named, so it cannot be compared, so it is reported —
            // dropped, it made `pull` write a role narrower than the database
            // holds and the next `plan` revoke what nobody removed.
            (Securable::Unreadable, _) => {
                unexpressible.push(Unexpressible {
                    role: p.role.clone(),
                    target: None,
                    what: format!(
                        "role {}: {} is on {} this connection cannot name (the catalog \
                         returned no name for it, which is what a securable the account \
                         holds nothing on looks like); the declarations cannot express it",
                        p.role,
                        p.permission,
                        if p.class == 3 {
                            "a schema"
                        } else {
                            "an object"
                        }
                    ),
                });
                continue;
            }
            // The database itself (`CONTROL`, `CREATE TABLE`), a type, an
            // assembly, another principal: nothing a declaration can name,
            // and a role that gained one out of band has changed even when
            // every grant the model does hold still matches (DECISIONS 105).
            (Securable::Unnamed, 0) => {
                unexpressible.push(Unexpressible {
                    role: p.role.clone(),
                    target: None,
                    what: format!(
                        "role {}: {} on the database is not modelled; the declarations cannot \
                         express it",
                        p.role, p.permission
                    ),
                });
                continue;
            }
            _ => {
                unexpressible.push(Unexpressible {
                    role: p.role.clone(),
                    target: None,
                    what: format!(
                        "role {}: {} on a {} (class {}) is not modelled; the declarations \
                         cannot express it",
                        p.role,
                        p.permission,
                        p.class_desc.to_ascii_lowercase().replace('_', " "),
                        p.class
                    ),
                });
                continue;
            }
        };
        // The target crosses a snapshot as its string form, in which `(`
        // opens a routine signature; a legal quoted object name may contain
        // one, and `dbo.sales(archive)` would be read back as a grant on a
        // routine (DECISIONS 205). The structured target is kept: only its
        // *string* form is ambiguous, and the target is what scopes the report
        // to the managed set — without it, a grant on an unmanaged object of
        // such a name would be reported, and refused, where an ordinary grant
        // on the same object is ignored (`unexpressible_permissions`).
        if target
            .to_string()
            .parse::<pbps_model::GrantTarget>()
            .as_ref()
            != Ok(&target)
        {
            unexpressible.push(Unexpressible {
                role: p.role.clone(),
                target: Some(target.clone()),
                what: format!(
                    "role {}: {} on [{}].[{}] is on an object whose name contains a period or a \
                     parenthesis, which a declaration cannot spell; the declarations cannot \
                     express it",
                    p.role,
                    p.permission,
                    match &p.securable {
                        Securable::Object { schema, .. } | Securable::Schema(schema) => schema,
                        // Neither reaches here: both are reported above,
                        // before a target exists to be spelled.
                        Securable::Unreadable | Securable::Unnamed => "",
                    },
                    match &p.securable {
                        Securable::Object { name, .. } => name,
                        // A schema target has no object half, and the other
                        // two are reported above.
                        Securable::Schema(_) | Securable::Unreadable | Securable::Unnamed => "",
                    }
                ),
            });
            continue;
        }
        // Every permission the model cannot hold is left out of the role's
        // set *and* reported as unexpressible, never as a warning alone: a
        // managed role that gained a column-level grant, a DENY, a CONTROL
        // or a grant on a sequence out of band is wider or narrower than
        // the recorded one, and a comparison of the sets that remain would
        // call it clean (DECISIONS 95, 97). Written into the role, a grant
        // on an object the model does not hold would also make `validate`
        // refuse the project `pull` just wrote.
        if let pbps_model::GrantTarget::Object(object) = &target
            && !schema.tables.contains_key(object)
            && !schema
                .modules
                .keys()
                .any(|id| id.referenced_name().as_ref() == Some(object))
        {
            unexpressible.push(Unexpressible {
                role: p.role.clone(),
                target: Some(target.clone()),
                what: format!(
                    "role {}: {} on {target} is on an object pbps does not model, or could not \
                     read; the declarations cannot express it",
                    p.role, p.permission
                ),
            });
            continue;
        }
        if p.minor_id != 0 {
            unexpressible.push(Unexpressible {
                role: p.role.clone(),
                target: Some(target.clone()),
                what: format!(
                    "role {}: a column-level {} on {target} is not modelled; the declarations \
                     cannot express it",
                    p.role, p.permission
                ),
            });
            continue;
        }
        match p.state.trim() {
            "G" | "W" => {}
            "D" => {
                unexpressible.push(Unexpressible {
                    role: p.role.clone(),
                    target: Some(target.clone()),
                    what: format!(
                        "role {}: DENY {} on {target} is not modelled (ADR-0005); the \
                         declarations cannot express it",
                        p.role, p.permission
                    ),
                });
                continue;
            }
            other => {
                unexpressible.push(Unexpressible {
                    role: p.role.clone(),
                    target: Some(target.clone()),
                    what: format!(
                        "role {}: permission state `{other}` on {target} is not modelled; the \
                         declarations cannot express it",
                        p.role
                    ),
                });
                continue;
            }
        }
        // The model's set is the union of the engines' (ADR-0010 §6), so a
        // word that parses is not yet a word this engine has: the second
        // filter keeps a spelling SQL Server never returns (measured, none of
        // the five is a built-in permission in any class) from being written
        // into a role on the day the catalog grows one, where `validate`
        // would refuse the project `pull` just wrote.
        let permission = match p.permission.parse::<pbps_model::Permission>() {
            Ok(permission) if crate::validate::has_permission(permission) => permission,
            _ => {
                unexpressible.push(Unexpressible {
                    role: p.role.clone(),
                    target: Some(target.clone()),
                    what: format!(
                        "role {}: {} on {target} is outside the permissions pbps manages on SQL \
                         Server; the declarations cannot express it",
                        p.role, p.permission
                    ),
                });
                continue;
            }
        };
        if p.state.trim() == "W" {
            // Not folded into the plain grant: it is wider, and a comparison
            // that read it as equal would call a widened role clean.
            unexpressible.push(Unexpressible {
                role: p.role.clone(),
                target: Some(target.clone()),
                what: format!(
                    "role {}: {} on {target} is granted WITH GRANT OPTION, which the \
                     declarations cannot express; revoke the grant option by hand \
                     (`REVOKE GRANT OPTION FOR {} ON {} FROM {}`) to bring it under pbps",
                    p.role,
                    permission.as_str(),
                    p.permission,
                    target,
                    p.role
                ),
            });
            continue;
        }
        role.grants.entry(target).or_default().insert(permission);
    }

    Pulled {
        schema,
        warnings,
        unexpressible,
        limitations,
        unmanaged_modules,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_table(id: i32, schema: &str, name: &str) -> RawTable {
        RawTable {
            object_id: id,
            schema: schema.into(),
            name: name.into(),
            temporal_type: 0,
            has_period: false,
        }
    }

    #[test]
    fn system_versioning_and_history_are_reported_instead_of_managed() {
        let mut raw = RawCatalog::default();
        for (id, name, temporal_type) in [(1, "current", 2), (2, "history", 1), (3, "plain", 0)] {
            let mut table = raw_table(id, "dbo", name);
            table.temporal_type = temporal_type;
            raw.tables.push(table);
            raw.columns.push(raw_column(id, "id", "int"));
        }
        let pulled = assemble(&raw);
        assert_eq!(pulled.schema.tables.len(), 1);
        assert!(
            pulled.schema.tables[&TableName::new("dbo", "plain")]
                .columns
                .contains_key("id")
        );
        assert_eq!(pulled.limitations.len(), 2);
        for name in ["current", "history"] {
            assert!(pulled.limitations.iter().any(|l| {
                l.target.object_name() == TableName::new("dbo", name)
                    && l.detail.contains("system versioning")
            }));
            assert!(
                pulled
                    .warnings
                    .iter()
                    .any(|w| w.contains(name) && w.contains("system versioning"))
            );
        }
    }

    #[test]
    fn a_period_definition_without_active_versioning_is_reported_instead_of_managed() {
        let mut raw = RawCatalog::default();
        let mut table = raw_table(1, "dbo", "disabled");
        table.has_period = true;
        raw.tables.push(table);
        raw.columns.push(raw_column(1, "id", "int"));

        let pulled = assemble(&raw);

        assert!(pulled.schema.tables.is_empty());
        assert_eq!(pulled.limitations.len(), 1);
        assert!(
            pulled.limitations[0]
                .detail
                .contains("PERIOD FOR SYSTEM_TIME")
        );
    }

    fn raw_column(id: i32, name: &str, type_name: &str) -> RawColumn {
        RawColumn {
            object_id: id,
            name: name.into(),
            type_name: type_name.into(),
            max_length: 8,
            precision: 0,
            scale: 0,
            is_nullable: true,
            is_computed: false,
            is_user_defined_type: false,
            identity: None,
            default: None,
        }
    }

    #[test]
    fn nvarchar_lengths_are_bytes_and_come_back_as_characters() {
        let mut c = raw_column(1, "x", "nvarchar");
        c.max_length = 200;
        assert_eq!(column_type(&c).unwrap().to_string(), "nvarchar(100)");
        c.max_length = -1;
        assert_eq!(column_type(&c).unwrap().to_string(), "nvarchar(max)");
        let mut v = raw_column(1, "x", "varchar");
        v.max_length = 200;
        assert_eq!(column_type(&v).unwrap().to_string(), "varchar(200)");
    }

    #[test]
    fn decimal_and_time_types_carry_their_stored_arguments() {
        let mut c = raw_column(1, "x", "numeric");
        (c.precision, c.scale) = (12, 4);
        // numeric normalizes to decimal, same as on the declared side.
        assert_eq!(column_type(&c).unwrap().to_string(), "decimal(12, 4)");
        let mut t = raw_column(1, "x", "datetime2");
        t.scale = 3;
        assert_eq!(column_type(&t).unwrap().to_string(), "datetime2(3)");
    }

    #[test]
    fn stored_parens_are_peeled_only_when_they_wrap_the_whole_string() {
        assert_eq!(strip_stored_parens("((0))"), "0");
        assert_eq!(strip_stored_parens("(getdate())"), "getdate()");
        assert_eq!(strip_stored_parens("([amount]>(0))"), "[amount]>(0)");
        // Peeling this one would change its meaning.
        assert_eq!(strip_stored_parens("(a) AND (b)"), "(a) AND (b)");
        assert_eq!(strip_stored_parens("plain"), "plain");
    }

    fn one_table_catalog() -> RawCatalog {
        let mut id_col = raw_column(10, "id", "bigint");
        id_col.is_nullable = false;
        id_col.identity = Some((1, 1));
        let mut email = raw_column(10, "email", "nvarchar");
        email.max_length = 510;
        let mut status = raw_column(10, "status", "tinyint");
        status.is_nullable = false;
        status.default = Some("((0))".into());
        RawCatalog {
            tables: vec![raw_table(10, "dbo", "customer")],
            columns: vec![id_col, email, status],
            key_columns: vec![RawKeyColumn {
                object_id: 10,
                constraint_name: "pk_customer".into(),
                is_primary: true,
                column: "id".into(),
            }],
            ..Default::default()
        }
    }

    /// A grant on something the model does not hold — a sequence, a synonym,
    /// a module that could not be read — is reported and left out. Written
    /// into the role, `validate` would refuse the project `pull` just wrote.
    #[test]
    fn a_grant_on_an_unmodelled_object_is_reported_and_left_out() {
        let mut raw = one_table_catalog();
        raw.roles.push(RawRole {
            name: "app_reader".into(),
        });
        let grant = |object: Option<&str>, class: u8| RawPermission {
            role: "app_reader".into(),
            class,
            class_desc: if class == 1 {
                "OBJECT_OR_COLUMN"
            } else {
                "SCHEMA"
            }
            .into(),
            permission: "SELECT".into(),
            state: "G".into(),
            securable: match object {
                Some(name) => Securable::Object {
                    schema: "dbo".into(),
                    name: name.to_owned(),
                },
                None => Securable::Schema("dbo".into()),
            },
            minor_id: 0,
        };
        raw.permissions.push(grant(Some("customer"), 1));
        raw.permissions.push(grant(Some("order_seq"), 1));
        raw.permissions.push(grant(None, 3));
        let p = assemble(&raw);
        let role = &p.schema.roles["app_reader"];
        let targets: Vec<String> = role.grants.keys().map(ToString::to_string).collect();
        assert_eq!(targets, ["dbo.customer", "schema::dbo"]);
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert_eq!(p.unexpressible.len(), 1, "{:?}", p.unexpressible);
        assert_eq!(p.unexpressible[0].role, "app_reader");
        assert!(
            p.unexpressible[0].what.contains("dbo.order_seq")
                && p.unexpressible[0].what.contains("does not model"),
            "{:?}",
            p.unexpressible
        );
    }

    /// A word the model spells for the other engine (ADR-0010 §6) is not a
    /// word this engine's catalog returns — measured, none of the five is a
    /// built-in permission in any class — and if it ever did, it is reported
    /// and left out like `CONTROL`, never written into a role `validate`
    /// would then refuse.
    #[test]
    fn a_catalog_word_the_model_holds_for_the_other_engine_is_reported_and_left_out() {
        let mut raw = one_table_catalog();
        raw.roles.push(RawRole {
            name: "app_reader".into(),
        });
        let grant = |permission: &str| RawPermission {
            role: "app_reader".into(),
            class: 1,
            class_desc: "OBJECT_OR_COLUMN".into(),
            permission: permission.into(),
            state: "G".into(),
            securable: Securable::Object {
                schema: "dbo".into(),
                name: "customer".into(),
            },
            minor_id: 0,
        };
        raw.permissions.push(grant("SELECT"));
        raw.permissions.push(grant("TRUNCATE"));
        let p = assemble(&raw);
        let held = &p.schema.roles["app_reader"].grants[&"dbo.customer".parse().unwrap()];
        assert_eq!(
            held.iter().copied().collect::<Vec<_>>(),
            [pbps_model::Permission::Select]
        );
        assert_eq!(p.unexpressible.len(), 1, "{:?}", p.unexpressible);
        assert!(
            p.unexpressible[0].what.contains("TRUNCATE")
                && p.unexpressible[0].what.contains("on SQL Server"),
            "{:?}",
            p.unexpressible
        );
    }

    /// A grant target's string form gives `(` a meaning, and a legal quoted
    /// object name may contain one: read back, `dbo.sales(archive)` would be
    /// a grant on a routine, not on the table it was granted on (DECISIONS
    /// 205). Reported, and left out of the set — with the structured target
    /// kept, so the report is still scoped to the managed set.
    #[test]
    fn a_grant_on_an_object_whose_name_cannot_be_spelled_is_unexpressible() {
        let mut raw = one_table_catalog();
        raw.roles.push(RawRole {
            name: "app_reader".into(),
        });
        let grant = |object: &str| RawPermission {
            role: "app_reader".into(),
            class: 1,
            class_desc: "OBJECT_OR_COLUMN".into(),
            permission: "SELECT".into(),
            state: "G".into(),
            securable: Securable::Object {
                schema: "dbo".into(),
                name: object.to_owned(),
            },
            minor_id: 0,
        };
        raw.permissions.push(grant("customer"));
        raw.permissions.push(grant("sales(archive)"));
        raw.permissions.push(grant("audit.v1"));
        let p = assemble(&raw);
        let targets: Vec<String> = p.schema.roles["app_reader"]
            .grants
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(targets, ["dbo.customer"], "{:?}", p.unexpressible);
        assert_eq!(p.unexpressible.len(), 2, "{:?}", p.unexpressible);
        for u in &p.unexpressible {
            assert_eq!(u.role, "app_reader");
            assert!(u.what.contains("period or a parenthesis"), "{u:?}");
        }
        let targets: Vec<Option<pbps_model::GrantTarget>> =
            p.unexpressible.iter().map(|u| u.target.clone()).collect();
        assert_eq!(
            targets,
            [
                Some(pbps_model::GrantTarget::Object(ObjectName::new(
                    "dbo",
                    "sales(archive)"
                ))),
                Some(pbps_model::GrantTarget::Object(ObjectName::new(
                    "dbo", "audit.v1"
                ))),
            ],
            "the securable stays structured, so the managed-set scope still applies"
        );
        assert!(
            p.unexpressible[0].what.contains("[dbo].[sales(archive)]")
                || p.unexpressible[1].what.contains("[dbo].[sales(archive)]"),
            "{:?}",
            p.unexpressible
        );
    }

    /// Wider than the plain grant, so never the plain grant: left out of the
    /// set and reported where a drift check will see it, not in a warning.
    #[test]
    fn a_grant_with_grant_option_is_unexpressible_not_a_plain_grant() {
        let mut raw = one_table_catalog();
        raw.roles.push(RawRole {
            name: "app_reader".into(),
        });
        let grant = |permission: &str, state: &str| RawPermission {
            role: "app_reader".into(),
            class: 1,
            class_desc: "OBJECT_OR_COLUMN".into(),
            permission: permission.into(),
            state: state.into(),
            securable: Securable::Object {
                schema: "dbo".into(),
                name: "customer".into(),
            },
            minor_id: 0,
        };
        raw.permissions.push(grant("SELECT", "G"));
        raw.permissions.push(grant("UPDATE", "W"));
        raw.permissions.push(grant("DELETE", "D"));
        raw.permissions.push(grant("CONTROL", "G"));
        let mut column = grant("INSERT", "G");
        column.minor_id = 2;
        raw.permissions.push(column);
        let p = assemble(&raw);
        let target: pbps_model::GrantTarget = "dbo.customer".parse().unwrap();
        let grants = &p.schema.roles["app_reader"].grants[&target];
        assert_eq!(
            grants.iter().copied().collect::<Vec<_>>(),
            [pbps_model::Permission::Select],
            "only the plain grant on the whole object is the declaration's"
        );
        let what: Vec<&str> = p.unexpressible.iter().map(|u| u.what.as_str()).collect();
        assert_eq!(what.len(), 4, "{what:?}");
        assert!(p.unexpressible.iter().all(|u| u.role == "app_reader"));
        assert!(
            what.iter()
                .any(|w| w.contains("WITH GRANT OPTION")
                    && w.contains("REVOKE GRANT OPTION FOR UPDATE")),
            "{what:?}"
        );
        assert!(what.iter().any(|w| w.contains("DENY DELETE")), "{what:?}");
        assert!(what.iter().any(|w| w.contains("CONTROL")), "{what:?}");
        assert!(
            what.iter().any(|w| w.contains("column-level INSERT")),
            "{what:?}"
        );
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    /// The row arrives, the grant is real, and only its securable has no
    /// name: dropped as if it were not there, `pull` wrote a role narrower
    /// than the database holds and the next `plan` proposed a `REVOKE` of a
    /// permission nobody removed (issue #93).
    #[test]
    fn a_grant_whose_securable_cannot_be_named_is_reported_not_dropped() {
        let mut raw = one_table_catalog();
        raw.roles.push(RawRole {
            name: "app_reader".into(),
        });
        let grant = |permission: &str, class: u8, securable: Securable| RawPermission {
            role: "app_reader".into(),
            class,
            class_desc: if class == 1 {
                "OBJECT_OR_COLUMN"
            } else {
                "SCHEMA"
            }
            .into(),
            permission: permission.into(),
            state: "G".into(),
            securable,
            minor_id: 0,
        };
        raw.permissions
            .push(grant("SELECT", 1, Securable::Unreadable));
        raw.permissions
            .push(grant("EXECUTE", 3, Securable::Unreadable));
        // The negative case: a grant whose securable the catalog did name is
        // still the role's, and costs no report.
        raw.permissions.push(grant(
            "SELECT",
            1,
            Securable::Object {
                schema: "dbo".into(),
                name: "customer".into(),
            },
        ));

        let p = assemble(&raw);
        let targets: Vec<String> = p.schema.roles["app_reader"]
            .grants
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(targets, ["dbo.customer"]);
        assert_eq!(p.unexpressible.len(), 2, "{:?}", p.unexpressible);
        assert!(p.unexpressible.iter().all(|u| u.role == "app_reader"));
        assert!(
            p.unexpressible.iter().all(|u| u.target.is_none()),
            "there is no name to scope the report by: {:?}",
            p.unexpressible
        );
        let what: Vec<&str> = p.unexpressible.iter().map(|u| u.what.as_str()).collect();
        assert!(
            what[0].contains("SELECT is on an object this connection cannot name"),
            "{what:?}"
        );
        assert!(
            what[1].contains("EXECUTE is on a schema this connection cannot name"),
            "{what:?}"
        );
    }

    /// A permission at the database, or on a class the model does not hold,
    /// has no target a declaration can name — and a role that gained one has
    /// changed even when its object grants still match (DECISIONS 105).
    #[test]
    fn a_permission_of_a_class_the_model_does_not_hold_is_unexpressible() {
        let mut raw = one_table_catalog();
        raw.roles.push(RawRole {
            name: "app_reader".into(),
        });
        let grant = |class: u8, class_desc: &str, permission: &str| RawPermission {
            role: "app_reader".into(),
            class,
            class_desc: class_desc.into(),
            permission: permission.into(),
            state: "G".into(),
            securable: Securable::Unnamed,
            minor_id: 0,
        };
        raw.permissions.push(grant(0, "DATABASE", "CREATE TABLE"));
        raw.permissions.push(grant(0, "DATABASE", "CONTROL"));
        raw.permissions
            .push(grant(4, "DATABASE_PRINCIPAL", "IMPERSONATE"));
        let p = assemble(&raw);
        assert!(p.schema.roles["app_reader"].grants.is_empty());
        let what: Vec<&str> = p.unexpressible.iter().map(|u| u.what.as_str()).collect();
        assert_eq!(what.len(), 3, "{what:?}");
        assert!(
            what.iter()
                .any(|w| w.contains("CREATE TABLE on the database")),
            "{what:?}"
        );
        assert!(
            what.iter().any(|w| w.contains("CONTROL on the database")),
            "{what:?}"
        );
        assert!(
            what.iter()
                .any(|w| w.contains("IMPERSONATE on a database principal (class 4)")),
            "{what:?}"
        );
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[test]
    fn a_full_table_assembles_with_no_warnings() {
        let p = assemble(&one_table_catalog());
        assert_eq!(p.warnings, Vec::<String>::new());
        let t = p
            .schema
            .tables
            .get(&TableName::new("dbo", "customer"))
            .unwrap();
        assert_eq!(
            t.columns["id"].identity,
            Some(Identity {
                seed: 1,
                increment: 1
            })
        );
        assert_eq!(t.columns["email"].ty.to_string(), "nvarchar(255)");
        assert_eq!(t.columns["status"].default.as_deref(), Some("0"));
        assert_eq!(
            t.primary_key.as_ref().unwrap().columns,
            vec!["id".to_owned()]
        );
    }

    /// Losing a column silently is the one unforgivable failure of pull: the
    /// generated declarations would plan its destruction.
    #[test]
    fn unsupported_columns_are_warned_about_never_dropped_silently() {
        let mut raw = one_table_catalog();
        let mut computed = raw_column(10, "total", "money");
        computed.is_computed = true;
        raw.columns.push(computed);
        let mut udt = raw_column(10, "region_code", "my_udt");
        udt.is_user_defined_type = true;
        raw.columns.push(udt);

        let p = assemble(&raw);
        assert_eq!(p.warnings.len(), 2, "{:?}", p.warnings);
        assert_eq!(p.limitations.len(), 2, "{:?}", p.limitations);
        assert!(
            p.limitations
                .iter()
                .all(|limitation| limitation.target.object_name()
                    == TableName::new("dbo", "customer"))
        );
        assert!(p.warnings[0].contains("computed"), "{:?}", p.warnings);
        assert!(p.warnings[1].contains("my_udt"), "{:?}", p.warnings);
        // The table itself survives with the supported columns.
        let t = p
            .schema
            .tables
            .get(&TableName::new("dbo", "customer"))
            .unwrap();
        assert_eq!(t.columns.len(), 3);
    }

    #[test]
    fn a_table_with_no_expressible_columns_is_left_out_entirely() {
        let mut c = raw_column(11, "only", "geometry_udt");
        c.is_user_defined_type = true;
        let raw = RawCatalog {
            tables: vec![raw_table(11, "dbo", "shapes")],
            columns: vec![c],
            ..Default::default()
        };
        let p = assemble(&raw);
        assert!(p.schema.tables.is_empty());
        assert_eq!(
            p.limitations[0].target.object_name(),
            TableName::new("dbo", "shapes")
        );
        assert!(
            p.warnings.iter().any(|w| w.contains("whole table")),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn multi_column_keys_keep_their_order() {
        let mut raw = one_table_catalog();
        raw.key_columns = vec![
            RawKeyColumn {
                object_id: 10,
                constraint_name: "pk_customer".into(),
                is_primary: true,
                column: "email".into(),
            },
            RawKeyColumn {
                object_id: 10,
                constraint_name: "pk_customer".into(),
                is_primary: true,
                column: "id".into(),
            },
        ];
        let p = assemble(&raw);
        let t = &p.schema.tables[&TableName::new("dbo", "customer")];
        assert_eq!(t.primary_key.as_ref().unwrap().columns, ["email", "id"]);
    }

    #[test]
    fn foreign_keys_line_both_sides_up_in_order() {
        let mut raw = one_table_catalog();
        for (a, b) in [("id", "cid"), ("email", "cemail")] {
            raw.foreign_key_columns.push(RawForeignKeyColumn {
                object_id: 10,
                constraint_name: "fk_x".into(),
                ref_schema: "dbo".into(),
                ref_table: "other".into(),
                column: a.into(),
                ref_column: b.into(),
                on_delete: 1,
                on_update: 0,
            });
        }
        let p = assemble(&raw);
        let fk = &p.schema.tables[&TableName::new("dbo", "customer")].foreign_keys["fk_x"];
        assert_eq!(fk.columns, ["id", "email"]);
        assert_eq!(fk.references_columns, ["cid", "cemail"]);
        assert_eq!(fk.on_delete, ReferentialAction::Cascade);
        assert_eq!(fk.on_update, ReferentialAction::NoAction);
    }

    #[test]
    fn index_key_and_include_columns_are_kept_apart() {
        let mut raw = one_table_catalog();
        let base = RawIndexColumn {
            object_id: 10,
            index_name: "ix_email".into(),
            is_unique: false,
            kind: IndexKind::Nonclustered,
            filter: Some("([email] IS NOT NULL)".into()),
            column: "email".into(),
            is_included: false,
            is_descending: true,
        };
        raw.index_columns.push(base.clone());
        raw.index_columns.push(RawIndexColumn {
            column: "status".into(),
            is_included: true,
            ..base
        });
        let p = assemble(&raw);
        let ix = &p.schema.tables[&TableName::new("dbo", "customer")].indexes["ix_email"];
        assert_eq!(ix.columns.len(), 1);
        assert!(ix.columns[0].descending);
        assert_eq!(ix.include, ["status"]);
        assert_eq!(ix.filter.as_deref(), Some("[email] IS NOT NULL"));
    }

    #[test]
    fn clustered_indexes_are_warned_about_and_left_out() {
        let mut raw = one_table_catalog();
        raw.tables.push(raw_table(20, "dbo", "archive"));
        raw.columns.push(raw_column(20, "id", "bigint"));
        let customer_index = RawIndexColumn {
            object_id: 10,
            index_name: "cx_shared".into(),
            is_unique: false,
            kind: IndexKind::Unmodelled(1),
            filter: None,
            column: "id".into(),
            is_included: false,
            is_descending: false,
        };
        // Multiple columns of one clustered index produce one limitation.
        raw.index_columns.push(customer_index.clone());
        raw.index_columns.push(RawIndexColumn {
            column: "email".into(),
            ..customer_index
        });
        // The same index name on a different table is a distinct limitation.
        raw.index_columns.push(RawIndexColumn {
            object_id: 20,
            index_name: "cx_shared".into(),
            is_unique: false,
            kind: IndexKind::Unmodelled(1),
            filter: None,
            column: "id".into(),
            is_included: false,
            is_descending: false,
        });
        let p = assemble(&raw);
        assert!(
            p.schema.tables[&TableName::new("dbo", "customer")]
                .indexes
                .is_empty()
        );
        assert_eq!(p.warnings.len(), 2, "{:?}", p.warnings);
        assert_eq!(p.limitations.len(), 2, "{:?}", p.limitations);
        assert_eq!(
            p.limitations
                .iter()
                .map(|limitation| limitation.target.object_name())
                .collect::<Vec<_>>(),
            [
                TableName::new("dbo", "customer"),
                TableName::new("dbo", "archive")
            ]
        );
    }

    /// A clustered columnstore is the case that used to pass silently: it is
    /// not `type = 1`, so the old clustered flag called it "not clustered" and
    /// the index was written into the declarations as an ordinary rowstore one.
    #[test]
    fn a_physical_index_kind_the_model_cannot_express_is_named_and_left_out() {
        // Through `from_type_code`, so the test pins the catalog's reading of
        // `sys.indexes.type` and not only the assembler's use of it.
        let column = |name: &str, type_code: u8| RawIndexColumn {
            object_id: 10,
            index_name: name.to_owned(),
            is_unique: false,
            kind: IndexKind::from_type_code(type_code),
            filter: None,
            column: "email".into(),
            is_included: false,
            is_descending: false,
        };
        let mut raw = one_table_catalog();
        raw.index_columns.push(column("cci", 5));
        raw.index_columns.push(column("xi", 3));
        // A type this build has never heard of is still not an ordinary index.
        raw.index_columns.push(column("zi", 9));
        // Negative case: the one kind the model does express is kept, and
        // costs no limitation.
        raw.index_columns.push(column("ix_email", 2));

        let p = assemble(&raw);
        let indexes = &p.schema.tables[&TableName::new("dbo", "customer")].indexes;
        assert_eq!(indexes.keys().collect::<Vec<_>>(), ["ix_email"]);
        assert_eq!(p.limitations.len(), 3, "{:?}", p.limitations);
        let said = p
            .limitations
            .iter()
            .map(|l| l.detail.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            said.contains("`cci` is a clustered columnstore index"),
            "{said}"
        );
        assert!(said.contains("`xi` is an XML index"), "{said}");
        assert!(said.contains("`zi` is of `sys.indexes.type` 9"), "{said}");
        assert!(!said.contains("ix_email"), "{said}");
    }

    #[test]
    fn only_the_nonclustered_rowstore_type_reads_as_an_expressible_index() {
        assert_eq!(IndexKind::from_type_code(2), IndexKind::Nonclustered);
        for code in [1, 3, 4, 5, 6, 7, 9] {
            assert_eq!(
                IndexKind::from_type_code(code),
                IndexKind::Unmodelled(code),
                "type {code}"
            );
        }
    }

    /// Rows for tables outside the managed set (dropped between queries, or
    /// pbps's own state tables) must not invent entries.
    #[test]
    fn rows_for_unknown_tables_are_ignored() {
        let mut raw = one_table_catalog();
        raw.columns.push(raw_column(99, "ghost", "int"));
        raw.checks.push(RawCheck {
            object_id: 99,
            name: "ck_ghost".into(),
            definition: "(1=1)".into(),
        });
        let p = assemble(&raw);
        assert_eq!(p.schema.tables.len(), 1);
    }
}

#[cfg(test)]
mod module_tests {
    use super::*;

    fn catalog_with(modules: Vec<RawModule>) -> RawCatalog {
        let mut c = raw_one_table();
        c.modules = modules;
        c
    }

    fn raw_one_table() -> RawCatalog {
        RawCatalog {
            tables: vec![RawTable {
                object_id: 1,
                schema: "dbo".into(),
                name: "t".into(),
                temporal_type: 0,
                has_period: false,
            }],
            columns: vec![RawColumn {
                object_id: 1,
                name: "id".into(),
                type_name: "int".into(),
                max_length: 4,
                precision: 10,
                scale: 0,
                is_nullable: false,
                is_computed: false,
                is_user_defined_type: false,
                identity: None,
                default: None,
            }],
            ..Default::default()
        }
    }

    fn module(schema: &str, name: &str, kind: ModuleKind, definition: Option<&str>) -> RawModule {
        RawModule {
            schema: schema.into(),
            name: name.into(),
            kind,
            definition: definition.map(str::to_owned),
            parent: None,
            default_set_options: true,
        }
    }

    /// ADR-0002: a module pbps can read becomes part of the desired state, and
    /// what it reads back is the *body* — the prefix is the emitter's, and
    /// keeping it would make every declaration compare unequal to the database
    /// it came from.
    #[test]
    fn readable_modules_become_part_of_the_schema() {
        let p = assemble(&catalog_with(vec![
            module(
                "dbo",
                "v_active",
                ModuleKind::View,
                Some("CREATE OR ALTER VIEW [dbo].[v_active]\nAS\nSELECT id FROM dbo.customer"),
            ),
            module(
                "dbo",
                "sp_reprice",
                ModuleKind::Procedure,
                Some(
                    "CREATE PROCEDURE dbo.sp_reprice @pct int AS UPDATE dbo.customer SET id = id;",
                ),
            ),
        ]));
        assert!(p.unmanaged_modules.is_empty(), "{:?}", p.unmanaged_modules);
        assert_eq!(p.schema.modules.len(), 2);
        assert_eq!(
            p.schema.modules[&"dbo.v_active".parse::<pbps_model::ModuleId>().unwrap()].definition,
            "SELECT id FROM dbo.customer"
        );
        assert_eq!(
            p.schema.modules[&"dbo.sp_reprice".parse::<pbps_model::ModuleId>().unwrap()].definition,
            "@pct int AS UPDATE dbo.customer SET id = id;"
        );
    }

    /// A trigger's table comes from the catalog, which knows it even when the
    /// definition text leaves it unqualified.
    #[test]
    fn a_trigger_keeps_the_table_it_is_on() {
        let mut trg = module(
            "dbo",
            "trg_audit",
            ModuleKind::Trigger,
            Some("CREATE TRIGGER dbo.trg_audit ON dbo.customer AFTER INSERT AS SELECT 1;"),
        );
        trg.parent = Some(("dbo".into(), "customer".into()));
        let p = assemble(&catalog_with(vec![trg]));
        // The table is in the key now, not in a field beside it
        // (ADR-0009 §1).
        let id: pbps_model::ModuleId = "dbo.customer.trg_audit".parse().unwrap();
        let m = &p.schema.modules[&id];
        assert_eq!(
            id.attached_to().map(ToString::to_string).as_deref(),
            Some("dbo.customer")
        );
        assert_eq!(m.definition, "AFTER INSERT AS SELECT 1;");
    }

    #[test]
    fn a_trigger_on_a_temporal_table_is_inventoried_with_its_parent() {
        let mut raw = raw_one_table();
        let mut plain = raw.tables[0].clone();
        plain.object_id = 2;
        plain.name = "plain".into();
        raw.tables.push(plain);
        let mut column = raw.columns[0].clone();
        column.object_id = 2;
        raw.columns.push(column);
        raw.tables[0].temporal_type = 2;
        for table in ["t", "plain"] {
            let mut trigger = module(
                "dbo",
                &format!("tr_{table}"),
                ModuleKind::Trigger,
                Some(&format!(
                    "CREATE TRIGGER dbo.tr_{table} ON dbo.{table} AFTER INSERT AS SELECT 1;"
                )),
            );
            trigger.parent = Some(("dbo".into(), table.into()));
            raw.modules.push(trigger);
        }
        let pulled = assemble(&raw);
        assert_eq!(pulled.schema.modules.len(), 1);
        assert!(
            pulled
                .schema
                .modules
                .contains_key(&"dbo.plain.tr_plain".parse().unwrap())
        );
        assert_eq!(pulled.unmanaged_modules.len(), 1);
        assert_eq!(
            pulled.unmanaged_modules[0].target.object_name(),
            TableName::new("dbo", "tr_t")
        );
        assert!(
            pulled.unmanaged_modules[0]
                .why
                .contains("system versioning")
        );
    }

    /// A module whose definition cannot be read, or whose shape the emitter
    /// could not reproduce, is inventoried with the reason — never dropped in
    /// silence, which would leave the next plan proposing its destruction.
    #[test]
    fn unmanageable_modules_are_inventoried_with_the_reason() {
        let p = assemble(&catalog_with(vec![
            // Encrypted or CLR: no definition at all.
            module("dbo", "sp_secret", ModuleKind::Procedure, None),
            // A view whose options the model cannot hold. Recreating it without
            // SCHEMABINDING would silently remove a guarantee.
            module(
                "dbo",
                "v_bound",
                ModuleKind::View,
                Some("CREATE VIEW dbo.v_bound WITH SCHEMABINDING AS SELECT 1"),
            ),
        ]));
        assert!(p.schema.modules.is_empty());
        assert_eq!(p.unmanaged_modules.len(), 2);
        assert!(
            p.unmanaged_modules[0].why.contains("ENCRYPTION"),
            "{:?}",
            p.unmanaged_modules
        );
        assert!(
            p.unmanaged_modules[1].why.contains("SCHEMABINDING"),
            "{:?}",
            p.unmanaged_modules
        );
    }

    /// A quoted identifier may hold the punctuation the id's string form uses
    /// for structure. Written to a snapshot such a module would read back as
    /// another one — `dbo.audit.v1` as a trigger on `dbo.audit`, `dbo.sales(archive)`
    /// as a routine — so it is inventoried instead, with the reason.
    #[test]
    fn a_module_whose_name_the_id_cannot_spell_is_inventoried() {
        let p = assemble(&catalog_with(vec![
            module(
                "dbo",
                "audit.v1",
                ModuleKind::View,
                Some("CREATE VIEW dbo.[audit.v1] AS SELECT 1"),
            ),
            module(
                "dbo",
                "sales(archive)",
                ModuleKind::View,
                Some("CREATE VIEW dbo.[sales(archive)] AS SELECT 1"),
            ),
            module(
                "dbo",
                "sales_archive",
                ModuleKind::View,
                Some("CREATE VIEW dbo.[sales_archive] AS SELECT 1"),
            ),
        ]));
        assert_eq!(
            p.schema
                .modules
                .keys()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["dbo.sales_archive"],
            "{:?}",
            p.unmanaged_modules
        );
        assert_eq!(p.unmanaged_modules.len(), 2, "{:?}", p.unmanaged_modules);
        for u in &p.unmanaged_modules {
            assert!(u.why.contains("period or a parenthesis"), "{u:?}");
        }
    }

    /// SQL Server persists QUOTED_IDENTIFIER and ANSI_NULLS with the module and
    /// re-applies them on every execution, so a module created with either OFF
    /// does not mean the same thing as the identical text recreated by pbps.
    /// The model has nowhere to keep them, so the honest answer is an inventory
    /// entry rather than a claim that it round-trips.
    #[test]
    fn a_module_with_nondefault_set_options_is_inventoried() {
        let mut m = module(
            "dbo",
            "v_quirk",
            ModuleKind::View,
            Some("CREATE VIEW dbo.v_quirk AS SELECT 1"),
        );
        m.default_set_options = false;
        let p = assemble(&catalog_with(vec![m]));
        assert!(p.schema.modules.is_empty());
        assert_eq!(p.unmanaged_modules.len(), 1);
        assert!(
            p.unmanaged_modules[0].why.contains("QUOTED_IDENTIFIER"),
            "{:?}",
            p.unmanaged_modules
        );
    }

    #[test]
    fn a_database_with_no_modules_reports_an_empty_inventory() {
        let p = assemble(&raw_one_table());
        assert!(p.unmanaged_modules.is_empty());
        assert!(p.schema.modules.is_empty());
    }

    /// The order must not depend on what the server happened to return.
    #[test]
    fn the_inventory_is_sorted_and_deduplicated() {
        let p = assemble(&catalog_with(vec![
            module("dbo", "zzz", ModuleKind::View, None),
            module("app", "aaa", ModuleKind::Trigger, None),
            module("dbo", "zzz", ModuleKind::View, None),
        ]));
        assert_eq!(
            p.unmanaged_modules
                .iter()
                .map(|m| m.target.to_string())
                .collect::<Vec<_>>(),
            ["app.aaa".to_owned(), "dbo.zzz".to_owned()]
        );
    }

    #[test]
    fn an_unreadable_module_keeps_a_dotted_identifier_structured() {
        let p = assemble(&catalog_with(vec![module(
            "dbo",
            "audit.v1",
            ModuleKind::Procedure,
            None,
        )]));
        assert_eq!(p.unmanaged_modules.len(), 1);
        assert_eq!(
            p.unmanaged_modules[0].target.object_name(),
            ObjectName::new("dbo", "audit.v1")
        );
    }

    #[test]
    fn every_module_type_code_maps_to_the_right_word() {
        for (code, want) in [
            ("V", Some("view")),
            ("P", Some("procedure")),
            ("PC", Some("procedure")),
            ("FN", Some("function")),
            ("IF", Some("function")),
            ("TF", Some("function")),
            ("TR", Some("trigger")),
        ] {
            assert_eq!(
                kind_from_type_code(code).map(ModuleKind::as_str),
                want,
                "type code {code}"
            );
        }
        // A table is not a module, and neither is a constraint; mislabelling one
        // would put it in the inventory as something the user must act on.
        assert_eq!(kind_from_type_code("U"), None);
        assert_eq!(kind_from_type_code("PK"), None);
        assert_eq!(kind_from_type_code("D"), None);
    }

    // ---- splitting a stored definition (ADR-0002) ----

    /// The round trip that matters: what the emitter writes, the splitter has
    /// to give back. Anything else and every apply would be followed by drift.
    #[test]
    fn what_the_emitter_writes_splits_back_into_the_body() {
        let name: ObjectName = "dbo.active_customer".parse().unwrap();
        for (kind, on, body) in [
            (
                ModuleKind::View,
                None,
                "SELECT customer_id\nFROM dbo.customer",
            ),
            (ModuleKind::Procedure, None, "@since date\nAS\nSELECT 1;"),
            (
                ModuleKind::Function,
                None,
                "(@a int)\nRETURNS int\nAS\nBEGIN RETURN @a; END",
            ),
            (
                ModuleKind::Trigger,
                Some("dbo.customer"),
                "AFTER INSERT\nAS SELECT 1;",
            ),
        ] {
            let module = Module {
                kind,
                description: None,
                definition: body.to_owned(),
            };
            let on: Option<ObjectName> = on.map(|t| t.parse().unwrap());
            let id = match &on {
                Some(table) => pbps_model::ModuleId::Trigger {
                    on: table.clone(),
                    name: name.name.clone(),
                },
                None => pbps_model::ModuleId::Named(name.clone()),
            };
            let stored = crate::emit::module_definition(&id, &module).expect("emit");
            let (back_on, back_body) = split_module(kind, &stored, false)
                .unwrap_or_else(|| panic!("could not split:\n{stored}"));
            assert_eq!(back_body, body, "{kind}");
            assert_eq!(back_on, on, "{kind}");
        }
    }

    /// A hand-written module was not written by the emitter, and the shapes it
    /// takes are the whole reason the splitter is tolerant.
    #[test]
    fn hand_written_spellings_still_split() {
        for stored in [
            "create view dbo.v as select 1",
            "CREATE   VIEW   [dbo] . [v]\n  AS  select 1",
            "-- who and why\nCREATE VIEW dbo.v\nAS select 1",
            "/* header */ CREATE OR ALTER VIEW dbo.v AS select 1",
        ] {
            let split = split_module(ModuleKind::View, stored, false);
            assert!(split.is_some(), "{stored}");
            assert!(
                split.unwrap().1.to_lowercase().contains("select 1"),
                "{stored}"
            );
        }
        // `PROC` is a legal abbreviation, and a definition using it is not a
        // module pbps should refuse to manage.
        assert!(
            split_module(
                ModuleKind::Procedure,
                "CREATE PROC dbo.p AS SELECT 1",
                false
            )
            .is_some()
        );

        // The ANSI spelling of a quoted name means the same thing as brackets
        // under QUOTED_IDENTIFIER ON, which is the only setting pbps manages.
        let split = split_module(
            ModuleKind::View,
            "CREATE VIEW \"dbo\".\"active customer\" AS SELECT 1",
            false,
        );
        assert_eq!(split, Some((None, "SELECT 1".to_owned())), "ANSI quoting");
        // Including its doubled-quote escape.
        assert!(
            split_module(
                ModuleKind::View,
                "CREATE VIEW \"dbo\".\"say \"\"hi\"\"\" AS SELECT 1",
                false
            )
            .is_some()
        );

        // An unqualified trigger target is the commonest spelling of all, and
        // the catalog knows the schema even though the text does not. The split
        // leaves `on` empty for `assemble` to fill from `sys.objects`.
        let split = split_module(
            ModuleKind::Trigger,
            "CREATE TRIGGER dbo.trg ON customer AFTER INSERT AS SELECT 1",
            true,
        );
        assert_eq!(split, Some((None, "AFTER INSERT AS SELECT 1".to_owned())));
    }

    /// A comment in the header is skipped as one however it is terminated and
    /// however deeply it nests: what follows it is the definition, and reading
    /// any of it as comment — or any of the comment as definition — reports a
    /// readable module as one this tool cannot read.
    #[test]
    fn a_header_comment_ends_where_the_engine_ends_it() {
        let plain = split_module(ModuleKind::View, "CREATE VIEW dbo.v AS SELECT 1", false);
        assert_eq!(plain, Some((None, "SELECT 1".to_owned())));
        for stored in [
            // A carriage return ends a line comment here, so the `AS` after it
            // is code. Ending the comment at `\n` alone took the whole rest of
            // the definition with it.
            "CREATE VIEW dbo.v -- note\rAS SELECT 1",
            "CREATE VIEW dbo.v -- note\r\nAS SELECT 1",
            "CREATE VIEW dbo.v -- note\nAS SELECT 1",
            // Block comments nest: the outer one ends at its own terminator.
            // Ending at the first `*/` left ` c */ AS SELECT 1` to be read as
            // code, where the keyword scan meets `c` and refuses.
            "CREATE VIEW dbo.v /* a /* b */ c */ AS SELECT 1",
            "CREATE VIEW dbo.v /* a /* b /* c */ d */ e */ AS SELECT 1",
            // The opener's own `*` must not also close it.
            "CREATE VIEW dbo.v /*/ still inside */ AS SELECT 1",
            // A line comment inside a block comment is comment, not a line
            // ending that could end the block.
            "CREATE VIEW dbo.v /* -- a */ AS SELECT 1",
        ] {
            assert_eq!(
                split_module(ModuleKind::View, stored, false),
                plain,
                "{stored:?}"
            );
        }
        // And the trigger target is read from the definition, not from an `ON`
        // that a nested comment holds.
        assert_eq!(
            split_module(
                ModuleKind::Trigger,
                "CREATE TRIGGER dbo.trg /* /* ON wrong */ */ ON dbo.customer AFTER INSERT AS SELECT 1",
                false,
            ),
            Some((
                Some(TableName::new("dbo".to_owned(), "customer".to_owned())),
                "AFTER INSERT AS SELECT 1".to_owned()
            ))
        );
    }

    /// When the scan meets something it cannot account for it must return
    /// nothing rather than guess: a wrong split would silently drop part of the
    /// definition, and the next apply would recreate the object without it.
    #[test]
    fn an_unaccountable_definition_does_not_split() {
        for stored in [
            "CREATE VIEW dbo.v WITH SCHEMABINDING AS SELECT 1",
            // Unqualified, and this time the catalog has no parent to fall back
            // on: guessing a schema would attach the trigger to the wrong table.
            "CREATE TRIGGER trg ON customer AFTER INSERT AS SELECT 1",
            "ALTER VIEW dbo.v AS SELECT 1",
            // An unterminated block comment swallows the definition on the
            // server too. Reading past it would be guessing which half of the
            // text the engine took as comment.
            "CREATE VIEW dbo.v /* unterminated AS SELECT 1",
            // And nesting does not rescue it: the inner `*/` closes the inner
            // comment, leaving the outer one open to the end.
            "CREATE VIEW dbo.v /* a /* b */ AS SELECT 1",
            "",
        ] {
            let kind = if stored.contains("TRIGGER") {
                ModuleKind::Trigger
            } else {
                ModuleKind::View
            };
            assert!(split_module(kind, stored, false).is_none(), "{stored}");
        }
    }
}
