//! DTO to domain model, extracting the one-shot intent annotations along the way.
//!
//! Errors are always **collected and returned together**, never aborted on the
//! first one — the user should see everything that needs fixing in one pass
//! instead of fixing one and running again.

use serde_saphyr::Spanned;
use std::str::FromStr;

use pbps_model::{
    CheckConstraint, Column, ColumnType, DataMode, ForeignKey, GrantTarget, Identity, Index,
    IndexColumn, Intent, Module, ModuleKind, ObjectName, Permission, PrimaryKey, Role, Row, RowKey,
    Strategy, Table, TableData, TableName, UniqueConstraint, Value,
};

use crate::dto::{DataDto, ModuleDto, PrimaryKeyDto, RoleDto, TableDto, ValueDto};
use crate::error::{LoadError, SourceFile, to_span};

/// The result of loading one declaration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedTable {
    pub name: TableName,
    pub table: Table,
    pub intents: Vec<Intent>,
    /// `None` when the file declares no `strategy:` block. Kept out of `table`
    /// so that `Schema` equality stays a question about the database alone.
    pub strategy: Option<Strategy>,
}

/// The result of loading one module declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModule {
    pub name: ObjectName,
    pub module: Module,
    /// Kept out of `module` so that `Schema` equality stays a question about
    /// the database alone: creation order is invisible there (ADR-0002).
    pub depends_on: std::collections::BTreeSet<ObjectName>,
}

/// The result of loading one role declaration (ADR-0005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedRole {
    pub name: String,
    pub role: Role,
    /// A `renamed_from:`, extracted rather than stored — the same one-shot
    /// rule tables follow.
    pub intents: Vec<Intent>,
}

/// DTO to domain model for one role.
///
/// The name is a bare identifier: a role is a database principal, not an
/// object in a schema, so a dot in it is almost certainly a table name typed
/// into the wrong file.
pub fn convert_role(src: &SourceFile, dto: RoleDto) -> Result<LoadedRole, Vec<LoadError>> {
    let mut errs = Vec::new();
    let mut intents = Vec::new();

    let name = dto.role.value.trim().to_owned();
    if name.is_empty() {
        errs.push(LoadError::semantic(
            src,
            to_span(&dto.role.defined),
            "a role must have a name",
            "empty",
        ));
    } else if name.contains('.') {
        errs.push(
            LoadError::semantic(
                src,
                to_span(&dto.role.defined),
                format!("`{name}` is not a role name: a role is not in a schema"),
                "contains a dot",
            )
            .with_help("write the bare role name, e.g. `role: app_reader`"),
        );
    }

    if let Some(from) = &dto.renamed_from {
        let from = from.value.trim().to_owned();
        if !name.is_empty() {
            intents.push(Intent::RenameRole {
                from,
                to: name.clone(),
            });
        }
    }

    let mut role = Role {
        description: dto.description,
        grants: Default::default(),
    };
    for (target, permissions) in &dto.grants {
        let target = match GrantTarget::from_str(target) {
            Ok(t) => t,
            Err(e) => {
                errs.push(
                    LoadError::semantic(
                        src,
                        to_span(&dto.role.defined),
                        format!("invalid grant target `{target}`: {e}"),
                        e.to_string(),
                    )
                    .with_help("a target is `schema.object` or `schema::name`"),
                );
                continue;
            }
        };
        if permissions.is_empty() {
            errs.push(LoadError::semantic(
                src,
                to_span(&dto.role.defined),
                format!("`{target}` is listed with no permission"),
                "empty list",
            ));
            continue;
        }
        let mut set = std::collections::BTreeSet::new();
        for p in permissions {
            match parse_at::<Permission>(src, p, "invalid permission") {
                Ok(p) => {
                    set.insert(p);
                }
                Err(e) => errs.push(e),
            }
        }
        role.grants.insert(target, set);
    }

    if errs.is_empty() {
        Ok(LoadedRole {
            name,
            role,
            intents,
        })
    } else {
        Err(errs)
    }
}

/// Parses a `Spanned` string, labelling any failure on that value.
fn parse_at<T>(src: &SourceFile, v: &Spanned<String>, what: &str) -> Result<T, LoadError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    T::from_str(&v.value).map_err(|e| {
        LoadError::semantic(
            src,
            to_span(&v.defined),
            format!("{what}: {e}"),
            e.to_string(),
        )
    })
}

pub fn convert(src: &SourceFile, dto: TableDto) -> Result<LoadedTable, Vec<LoadError>> {
    let mut errs = Vec::new();
    let mut intents = Vec::new();

    let name: Option<TableName> = match parse_at(src, &dto.table, "invalid table name") {
        Ok(n) => Some(n),
        Err(e) => {
            errs.push(e.with_help(
                "a table name must have the two parts `schema.table`, e.g. `dbo.customer`",
            ));
            None
        }
    };

    if let (Some(name), Some(from)) = (&name, &dto.renamed_from) {
        match parse_at::<TableName>(src, from, "invalid table name in renamed_from") {
            Ok(from) => intents.push(Intent::RenameTable {
                from,
                to: name.clone(),
            }),
            Err(e) => errs.push(e),
        }
    }

    let mut columns = indexmap::IndexMap::with_capacity(dto.columns.len());
    for (col_name, c) in dto.columns {
        let ty = match parse_at::<ColumnType>(src, &c.ty, "invalid type") {
            Ok(t) => t,
            Err(e) => {
                errs.push(e);
                continue;
            }
        };

        if let (Some(table), Some(from)) = (&name, &c.renamed_from) {
            intents.push(Intent::RenameColumn {
                table: table.clone(),
                from: from.value.clone(),
                to: col_name.clone(),
            });
        }

        columns.insert(
            col_name,
            Column {
                ty,
                nullable: c.nullable,
                default: c.default,
                identity: c
                    .identity
                    .map(|[seed, increment]| Identity { seed, increment }),
                description: c.description,
                deprecated: c.deprecated,
            },
        );
    }

    let primary_key = dto.primary_key.map(|pk| match pk {
        PrimaryKeyDto::Columns(columns) => PrimaryKey {
            name: None,
            columns,
        },
        PrimaryKeyDto::Named { name, columns } => PrimaryKey {
            name: Some(name),
            columns,
        },
    });

    let unique = dto
        .unique
        .into_iter()
        .map(|(k, columns)| (k, UniqueConstraint { columns }))
        .collect();

    let mut foreign_keys = std::collections::BTreeMap::new();
    for (k, fk) in dto.foreign_keys {
        match parse_reference(src, &fk.references) {
            Ok((references_table, references_columns)) => {
                foreign_keys.insert(
                    k,
                    ForeignKey {
                        columns: fk.columns,
                        references_table,
                        references_columns,
                        on_delete: fk.on_delete,
                        on_update: fk.on_update,
                    },
                );
            }
            Err(e) => errs.push(e),
        }
    }

    let checks = dto
        .checks
        .into_iter()
        .map(|(k, expression)| (k, CheckConstraint { expression }))
        .collect();

    let mut indexes = std::collections::BTreeMap::new();
    for (k, ix) in dto.indexes {
        let mut cols = Vec::with_capacity(ix.columns.len());
        let mut ok = true;
        for c in &ix.columns {
            match parse_index_column(src, c) {
                Ok(v) => cols.push(v),
                Err(e) => {
                    errs.push(e);
                    ok = false;
                }
            }
        }
        if ok {
            indexes.insert(
                k,
                Index {
                    columns: cols,
                    include: ix.include,
                    unique: ix.unique,
                    filter: ix.filter,
                },
            );
        }
    }

    let data = match dto.data {
        Some(d) => match convert_data(src, d) {
            Ok(d) => Some(d),
            Err(e) => {
                errs.extend(e);
                None
            }
        },
        None => None,
    };

    match (name, errs.is_empty()) {
        (Some(name), true) => Ok(LoadedTable {
            name,
            table: Table {
                description: dto.description,
                columns,
                primary_key,
                unique,
                foreign_keys,
                checks,
                indexes,
                data,
            },
            intents,
            strategy: dto.strategy.map(|s| Strategy { online: s.online }),
        }),
        _ => Err(errs),
    }
}

/// The `data:` block (ADR-0004).
///
/// Every problem is collected, like everywhere else here: a lookup table's
/// block is the one place a user writes dozens of similar lines, and reporting
/// the first bad one at a time would be the worst possible place to do it.
fn convert_data(src: &SourceFile, dto: DataDto) -> Result<TableData, Vec<LoadError>> {
    let mut errs = Vec::new();

    // The `mode:` scalar is the only spanned thing in the block, so it is also
    // where the cell diagnostics below point. They name the row and column in
    // the message; the span gets the reader to the right block.
    let at = to_span(&dto.mode.defined);

    let mode = match dto.mode.value.as_str() {
        "exact" => Some(DataMode::Exact),
        "ensure" => Some(DataMode::Ensure),
        other => {
            errs.push(
                LoadError::semantic(
                    src,
                    at,
                    format!("unknown data mode `{other}`"),
                    "not a mode",
                )
                .with_help(
                    "`exact` means the declared rows are the whole table and an undeclared row is deleted; \
                     `ensure` means they must exist and anything else is left alone",
                ),
            );
            None
        }
    };

    let mut rows = std::collections::BTreeMap::new();
    for (key, cells) in dto.rows {
        let mut row = std::collections::BTreeMap::new();
        for (column, cell) in cells {
            match convert_value(cell) {
                Ok(v) => {
                    row.insert(column, v);
                }
                // The message deliberately does not echo the number back.
                // It has already been through `f64` by the time it gets here,
                // so echoing it would print `1.5` at a user who wrote `1.50` —
                // demonstrating the bug in the middle of explaining it.
                Err(FloatCell) => errs.push(
                    LoadError::semantic(
                        src,
                        at,
                        format!("row `{key}`, column `{column}`: an unquoted decimal"),
                        "quote it",
                    )
                    .with_help(
                        "the exact form written is the literal that reaches the column, and reading it \
                         as a floating-point number would not give it back — `'1.50'` stays 1.50",
                    ),
                ),
            }
        }
        rows.insert(RowKey::from(key), Row(row));
    }

    match (mode, errs.is_empty()) {
        (Some(mode), true) => Ok(TableData { mode, rows }),
        _ => Err(errs),
    }
}

/// A YAML float, which the loader refuses.
struct FloatCell;

fn convert_value(dto: ValueDto) -> Result<Value, FloatCell> {
    Ok(match dto {
        ValueDto::Null => Value::Null,
        ValueDto::Bool(b) => Value::Bool(b),
        ValueDto::Int(i) => Value::Int(i),
        ValueDto::Float(_) => return Err(FloatCell),
        ValueDto::Text(t) => Value::Text(t),
    })
}

/// `dbo.region(region_id)` / `dbo.region(a, b)` into a table name and a column
/// list.
fn parse_reference(
    src: &SourceFile,
    v: &Spanned<String>,
) -> Result<(TableName, Vec<String>), LoadError> {
    let bad = |msg: &str| {
        LoadError::semantic(
            src,
            to_span(&v.defined),
            format!("invalid foreign key target: {msg}"),
            msg,
        )
        .with_help("the format is `schema.table(column)`; separate multiple columns with commas")
    };

    let s = v.value.trim();
    let open = s.find('(').ok_or_else(|| bad("missing `(`"))?;
    let close = s.rfind(')').ok_or_else(|| bad("missing `)`"))?;
    if close < open || !s[close + 1..].trim().is_empty() {
        return Err(bad("the parentheses are misplaced"));
    }

    let table = TableName::from_str(s[..open].trim()).map_err(|e| bad(&e.to_string()))?;

    let mut columns = Vec::new();
    for raw in s[open + 1..close].split(',') {
        let c = raw.trim();
        if c.is_empty() {
            return Err(bad("the column list has an empty entry"));
        }
        columns.push(c.to_owned());
    }
    Ok((table, columns))
}

/// `created_at` or `created_at desc`.
fn parse_index_column(src: &SourceFile, v: &Spanned<String>) -> Result<IndexColumn, LoadError> {
    let parts: Vec<&str> = v.value.split_whitespace().collect();
    let bad = |msg: &str| {
        LoadError::semantic(
            src,
            to_span(&v.defined),
            format!("invalid index column: {msg}"),
            msg,
        )
        .with_help("the format is `column` or `column desc`")
    };

    match parts.as_slice() {
        [name] => Ok(IndexColumn {
            name: (*name).to_owned(),
            descending: false,
        }),
        [name, dir] if dir.eq_ignore_ascii_case("asc") || dir.eq_ignore_ascii_case("desc") => {
            Ok(IndexColumn {
                name: (*name).to_owned(),
                descending: dir.eq_ignore_ascii_case("desc"),
            })
        }
        [_, dir] => Err(bad(&format!("`{dir}` is not asc or desc"))),
        _ => Err(bad("wrong number of parts")),
    }
}

/// DTO to domain model for one module.
pub fn convert_module(src: &SourceFile, dto: ModuleDto) -> Result<LoadedModule, Vec<LoadError>> {
    let mut errs = Vec::new();

    // Exactly one leading key names the object, and it is also the kind. Two of
    // them is not a file the tool can guess its way through: which one is the
    // object and which one a stray line is precisely what it cannot know.
    let named: Vec<(ModuleKind, &Spanned<String>)> = [
        (ModuleKind::View, &dto.view),
        (ModuleKind::Procedure, &dto.procedure),
        (ModuleKind::Function, &dto.function),
        (ModuleKind::Trigger, &dto.trigger),
    ]
    .into_iter()
    .filter_map(|(kind, v)| v.as_ref().map(|v| (kind, v)))
    .collect();

    let (kind, name_value) = match named.as_slice() {
        [one] => *one,
        [] => {
            return Err(vec![LoadError::Yaml {
                path: std::path::PathBuf::from(&src.name),
                message: "a declaration file starts with `table:`, `view:`, `procedure:`, \
                          `function:` or `trigger:`"
                    .to_owned(),
            }]);
        }
        many => {
            let kinds: Vec<&str> = many.iter().map(|(k, _)| k.as_str()).collect();
            return Err(vec![LoadError::semantic(
                src,
                to_span(&many[0].1.defined),
                format!(
                    "this file declares {} objects at once: {}",
                    many.len(),
                    kinds.join(", ")
                ),
                "one object per file",
            )]);
        }
    };

    let name: Option<ObjectName> = match parse_at(src, name_value, "invalid object name") {
        Ok(n) => Some(n),
        Err(e) => {
            errs.push(e.with_help(
                "an object name must have the two parts `schema.object`, e.g. `dbo.active_customer`",
            ));
            None
        }
    };

    let mut on = None;
    if let Some(v) = &dto.on {
        match parse_at::<TableName>(src, v, "invalid table name in `on`") {
            Ok(t) => on = Some(t),
            Err(e) => errs.push(e),
        }
    }

    let mut depends_on = std::collections::BTreeSet::new();
    for v in &dto.depends_on {
        match parse_at::<ObjectName>(src, v, "invalid object name in `depends_on`") {
            Ok(n) => {
                depends_on.insert(n);
            }
            Err(e) => errs.push(e),
        }
    }

    match (name, errs.is_empty()) {
        (Some(name), true) => Ok(LoadedModule {
            name,
            module: Module {
                kind,
                description: dto.description,
                on,
                definition: dto.definition,
            },
            depends_on,
        }),
        _ => Err(errs),
    }
}
