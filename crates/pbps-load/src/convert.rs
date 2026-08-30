//! DTO → 領域模型，並抽出一次性的意圖註記。
//!
//! 錯誤一律**收集完再回傳**，不在第一個錯誤就中止 —— 使用者應該一次看完所有
//! 要修的地方，而不是修一個跑一次。

use serde_saphyr::Spanned;
use std::str::FromStr;

use pbps_model::{
    CheckConstraint, Column, ColumnType, ForeignKey, Identity, Index, IndexColumn, Intent,
    PrimaryKey, Table, TableName, UniqueConstraint,
};

use crate::dto::{PrimaryKeyDto, TableDto};
use crate::error::{LoadError, SourceFile, to_span};

/// 一份宣告檔的載入結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedTable {
    pub name: TableName,
    pub table: Table,
    pub intents: Vec<Intent>,
}

/// 解析一個 `Spanned` 字串，失敗時把錯誤標在該值上。
fn parse_at<T>(src: &SourceFile, v: &Spanned<String>, what: &str) -> Result<T, LoadError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    T::from_str(&v.value).map_err(|e| {
        LoadError::semantic(
            src,
            to_span(&v.defined),
            format!("{what}：{e}"),
            e.to_string(),
        )
    })
}

pub fn convert(src: &SourceFile, dto: TableDto) -> Result<LoadedTable, Vec<LoadError>> {
    let mut errs = Vec::new();
    let mut intents = Vec::new();

    let name: Option<TableName> = match parse_at(src, &dto.table, "表名無效") {
        Ok(n) => Some(n),
        Err(e) => {
            errs.push(e.with_help("表名必須是 `schema.table` 兩段式，例如 `dbo.customer`"));
            None
        }
    };

    if let (Some(name), Some(from)) = (&name, &dto.renamed_from) {
        match parse_at::<TableName>(src, from, "renamed_from 的表名無效") {
            Ok(from) => intents.push(Intent::RenameTable {
                from,
                to: name.clone(),
            }),
            Err(e) => errs.push(e),
        }
    }

    let mut columns = indexmap::IndexMap::with_capacity(dto.columns.len());
    for (col_name, c) in dto.columns {
        let ty = match parse_at::<ColumnType>(src, &c.ty, "型別無效") {
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

/// `dbo.region(region_id)` / `dbo.region(a, b)` → 表名與欄位清單。
fn parse_reference(
    src: &SourceFile,
    v: &Spanned<String>,
) -> Result<(TableName, Vec<String>), LoadError> {
    let bad = |msg: &str| {
        LoadError::semantic(
            src,
            to_span(&v.defined),
            format!("外鍵目標無效：{msg}"),
            msg,
        )
        .with_help("格式為 `schema.table(column)`，多欄位用逗號分隔")
    };

    let s = v.value.trim();
    let open = s.find('(').ok_or_else(|| bad("缺少 `(`"))?;
    let close = s.rfind(')').ok_or_else(|| bad("缺少 `)`"))?;
    if close < open || !s[close + 1..].trim().is_empty() {
        return Err(bad("括號位置不正確"));
    }

    let table = TableName::from_str(s[..open].trim()).map_err(|e| bad(&e.to_string()))?;

    let mut columns = Vec::new();
    for raw in s[open + 1..close].split(',') {
        let c = raw.trim();
        if c.is_empty() {
            return Err(bad("欄位清單中有空項目"));
        }
        columns.push(c.to_owned());
    }
    Ok((table, columns))
}

/// `created_at` 或 `created_at desc`
fn parse_index_column(src: &SourceFile, v: &Spanned<String>) -> Result<IndexColumn, LoadError> {
    let parts: Vec<&str> = v.value.split_whitespace().collect();
    let bad = |msg: &str| {
        LoadError::semantic(
            src,
            to_span(&v.defined),
            format!("索引欄位無效：{msg}"),
            msg,
        )
        .with_help("格式為 `欄位名` 或 `欄位名 desc`")
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
        [_, dir] => Err(bad(&format!("`{dir}` 不是 asc 或 desc"))),
        _ => Err(bad("項目數不正確")),
    }
}
