//! DTO to domain model, extracting the one-shot intent annotations along the way.
//!
//! Errors are always **collected and returned together**, never aborted on the
//! first one — the user should see everything that needs fixing in one pass
//! instead of fixing one and running again.

use serde_saphyr::Spanned;
use std::str::FromStr;

use pbps_model::{
    CheckConstraint, Column, ColumnType, ForeignKey, Identity, Index, IndexColumn, Intent,
    PrimaryKey, Table, TableName, UniqueConstraint,
};

use crate::dto::{PrimaryKeyDto, TableDto};
use crate::error::{LoadError, SourceFile, to_span};

/// The result of loading one declaration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedTable {
    pub name: TableName,
    pub table: Table,
    pub intents: Vec<Intent>,
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
            },
            intents,
        }),
        _ => Err(errs),
    }
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
