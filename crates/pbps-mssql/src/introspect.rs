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

use std::collections::BTreeMap;

use pbps_dialect::DialectError;
use pbps_model::{
    CheckConstraint, Column, ColumnType, ForeignKey, Identity, Index, IndexColumn, Module,
    ObjectName, PrimaryKey, ReferentialAction, Schema, Table, TableName, TypeArg, UniqueConstraint,
};

use crate::types;

/// One row of `sys.tables`.
#[derive(Debug, Clone)]
pub struct RawTable {
    pub object_id: i32,
    pub schema: String,
    pub name: String,
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

/// One column of an index that is not backing a PK or UNIQUE constraint.
#[derive(Debug, Clone)]
pub struct RawIndexColumn {
    pub object_id: i32,
    pub index_name: String,
    pub is_unique: bool,
    pub is_clustered: bool,
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
}

/// The result of a pull: the schema, plus everything that could not be said.
#[derive(Debug, Clone)]
pub struct Pulled {
    pub schema: Schema,
    /// Facts about the database the model cannot express. Never empty silence:
    /// the caller must show these, because each one is a difference that would
    /// otherwise surface as phantom drift or a destructive plan later.
    pub warnings: Vec<String>,
    /// Modules the database has that pbps cannot manage: a CLR object, one
    /// created `WITH ENCRYPTION`, or one whose stored text does not have the
    /// shape the emitter can reproduce.
    ///
    /// Separate from `warnings` because these are not defects in the pull —
    /// they are an inventory of what is left alone, and the user needs the
    /// count and the names.
    pub unmanaged_modules: Vec<UnmanagedModule>,
}

/// One module the database has and `pbps` does not manage.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnmanagedModule {
    pub kind: &'static str,
    pub name: String,
    /// Why it is not managed, in the operator's words.
    pub why: String,
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
            i += trimmed.find('\n').map_or(trimmed.len(), |n| n + 1);
            continue;
        }
        if trimmed.starts_with("/*") {
            match trimmed.find("*/") {
                Some(end) => i += end + 2,
                None => return s.len(),
            }
            continue;
        }
        return i;
    }
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
        if let Some(stripped) = rest.strip_prefix('[') {
            // `]]` is an escaped bracket inside a quoted identifier.
            let mut part = String::new();
            let mut chars = stripped.char_indices();
            let end = loop {
                let (at, ch) = chars.next()?;
                if ch == ']' {
                    match stripped[at + 1..].starts_with(']') {
                        true => {
                            part.push(']');
                            chars.next();
                        }
                        false => break at,
                    }
                } else {
                    part.push(ch);
                }
            };
            parts.push(part);
            i += end + 2;
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
    let mut names: BTreeMap<i32, TableName> = BTreeMap::new();
    let mut tables: BTreeMap<i32, Table> = BTreeMap::new();

    for t in &raw.tables {
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
            warnings.push(format!(
                "{table_name}.{}: computed columns are not supported yet; it was left out of the declarations",
                c.name
            ));
            continue;
        }
        if c.is_user_defined_type {
            warnings.push(format!(
                "{table_name}.{}: user-defined type `{}` is not supported yet; it was left out of the declarations",
                c.name, c.type_name
            ));
            continue;
        }
        let ty = match column_type(c) {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!(
                    "{table_name}.{}: {e}; it was left out of the declarations",
                    c.name
                ));
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

    for i in &raw.index_columns {
        let Some(table) = tables.get_mut(&i.object_id) else {
            continue;
        };
        if i.is_clustered {
            // The model has no clustered-ness; recording the index without it
            // would make bootstrap create a different physical layout.
            let table_name = name_of(i.object_id, &names);
            if !warnings
                .iter()
                .any(|w| w.contains(&format!("index `{}`", i.index_name)))
            {
                warnings.push(format!(
                    "{table_name}: index `{}` is clustered, which is not modelled yet; it was left out of the declarations",
                    i.index_name
                ));
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
            warnings.push(format!(
                "{table_name}: no supported columns remain; the whole table was left out of the declarations"
            ));
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
                name: name.to_string(),
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

        schema.modules.insert(
            name,
            Module {
                kind: m.kind,
                // A description lives in the declarations, not in the database;
                // pulling one back is not possible and pretending otherwise
                // would make every pulled module compare unequal.
                description: None,
                on,
                definition,
            },
        );
    }
    unmanaged_modules.sort();
    unmanaged_modules.dedup();

    Pulled {
        schema,
        warnings,
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
        }
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
            is_clustered: false,
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
        raw.index_columns.push(RawIndexColumn {
            object_id: 10,
            index_name: "cx_customer".into(),
            is_unique: false,
            is_clustered: true,
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
        assert!(p.warnings.iter().any(|w| w.contains("clustered")));
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
            p.schema.modules[&"dbo.v_active".parse::<ObjectName>().unwrap()].definition,
            "SELECT id FROM dbo.customer"
        );
        assert_eq!(
            p.schema.modules[&"dbo.sp_reprice".parse::<ObjectName>().unwrap()].definition,
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
        let m = &p.schema.modules[&"dbo.trg_audit".parse::<ObjectName>().unwrap()];
        assert_eq!(
            m.on.as_ref().map(ToString::to_string).as_deref(),
            Some("dbo.customer")
        );
        assert_eq!(m.definition, "AFTER INSERT AS SELECT 1;");
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
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["app.aaa", "dbo.zzz"]
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
                on: on.map(|t| t.parse().unwrap()),
                definition: body.to_owned(),
            };
            let stored = crate::emit::module_definition(&name, &module).expect("emit");
            let (back_on, back_body) = split_module(kind, &stored, false)
                .unwrap_or_else(|| panic!("could not split:\n{stored}"));
            assert_eq!(back_body, body, "{kind}");
            assert_eq!(back_on, module.on, "{kind}");
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
