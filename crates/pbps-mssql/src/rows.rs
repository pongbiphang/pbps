//! Reading declared rows back from a table — the connected half of ADR-0004.
//!
//! # The engine spells the values
//!
//! Every cell comes back as text that the **server** rendered (`CONVERT` with a
//! fixed style), never as a driver-typed value this crate then formats. That is
//! §8.2's rule — the database is the normalizer — applied to rows: whatever
//! spelling the engine gives a `decimal(5,2)` or a `datetime2` is the spelling
//! the state records, both sides of a drift check see the same one, and a
//! declaration that wants to match it writes it that way (`pull --data` shows
//! it). The alternative, parsing values into typed Rust and formatting them
//! back, would put a second formatter between the engine and the comparison
//! and let the two disagree about `1.5` and `1.50`.
//!
//! Only three kinds survive the trip as anything but text: `bit` becomes a
//! boolean and the four integer types become integers, because those are the
//! two shapes a declaration can write unquoted and they have to compare equal
//! to what was written.
//!
//! # A cell that holds the default is read back as omitted
//!
//! A declared row says "the default" by leaving the column out ([`Row`]). The
//! catalog holds a value, not the fact that it came from the default — so the
//! read-back asks the engine, per cell, whether the value *equals* the column's
//! default expression, and omits the cell when it does. That makes the omitted
//! spelling round-trip, and it is what lets a declaration that omits `label`
//! compare equal to a table where every label is `'Unlabelled'`.
//!
//! Two consequences are deliberate. A default the engine cannot evaluate to the
//! stored value (`SYSUTCDATETIME()`, `NEWID()`) never matches, so such a cell
//! is read back explicit and a row that omits it is restated as `= DEFAULT` on
//! every connected plan — visibly, and the remedy is to write the value. And a
//! NULL in a column with no default is omitted too, because the model already
//! treats the two spellings as one there (see [`pbps_model::data::cell`]);
//! writing it explicitly would make the same table compare unequal to itself
//! across two reads.
//!
//! # What cannot be read
//!
//! A table whose live primary key is not a single column has rows with no
//! identity, and the read **fails** rather than returning nothing: "absent",
//! "empty" and "unreadable" are three different answers, and only one of them
//! is good news.

use std::collections::BTreeMap;

use pbps_db::DbError;
use pbps_dialect::DialectError;
use pbps_model::{Row, RowKey, RowScope, Table, TableName, Value};

use crate::emit::qualified;
use crate::ident::{literal, quote};

/// Why a table's rows could not be read back.
#[derive(Debug, thiserror::Error)]
pub enum RowsError {
    #[error("{table}: its rows cannot be read back — {why}")]
    Unreadable { table: TableName, why: String },

    #[error("{table}: reading its rows back failed: {source}")]
    Read {
        table: TableName,
        // Boxed so the error is not larger than every `Ok` it travels beside.
        #[source]
        source: Box<DbError>,
    },

    /// The engine sent a value the mapping cannot hold — an integer column
    /// whose text does not parse, say. A bug in this file, not bad data.
    #[error("{table}.{column}: the engine sent `{text}`, which is not a {kind}")]
    BadValue {
        table: TableName,
        column: String,
        text: String,
        kind: &'static str,
    },

    #[error(transparent)]
    Dialect(#[from] DialectError),
}

/// What a column's text becomes once it is back in the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Bool,
    Int,
    Text,
}

impl ValueKind {
    pub fn of(base: &str) -> ValueKind {
        match base {
            "bit" => ValueKind::Bool,
            "tinyint" | "smallint" | "int" | "bigint" => ValueKind::Int,
            _ => ValueKind::Text,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            ValueKind::Bool => "boolean",
            ValueKind::Int => "integer",
            ValueKind::Text => "text",
        }
    }
}

/// One selected column and where its pieces land in the result row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub column: String,
    pub kind: ValueKind,
    /// Position of the value in the result row.
    pub value_at: usize,
    /// Position of the "equals the default" flag, for a column that has a
    /// default the engine can compare.
    pub default_at: Option<usize>,
    /// Whether the column has a default at all — which decides what a NULL
    /// means (see the module docs).
    pub has_default: bool,
}

/// The query that reads one table's rows, and how to read its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowQuery {
    pub sql: String,
    pub key: Slot,
    pub columns: Vec<Slot>,
}

/// The single primary-key column, or why there is none.
fn key_column(name: &TableName, table: &Table) -> Result<String, RowsError> {
    match &table.primary_key {
        Some(pk) if pk.columns.len() == 1 => Ok(pk.columns[0].clone()),
        Some(pk) => Err(RowsError::Unreadable {
            table: name.clone(),
            why: format!(
                "its primary key has {} columns ({}), and rows are keyed by one",
                pk.columns.len(),
                pk.columns.join(", ")
            ),
        }),
        None => Err(RowsError::Unreadable {
            table: name.clone(),
            why: "it has no primary key, so its rows have no identity".to_owned(),
        }),
    }
}

/// Builds the read for one table, or `None` when the scope names no row at
/// all — there is nothing to ask, and `IN ()` is not T-SQL.
pub fn query(
    name: &TableName,
    table: &Table,
    scope: &RowScope,
) -> Result<Option<RowQuery>, RowsError> {
    let key = key_column(name, table)?;
    let Some(key_spec) = table.columns.get(&key) else {
        return Err(RowsError::Unreadable {
            table: name.clone(),
            why: format!("its primary key column `{key}` is not among the columns read"),
        });
    };
    if let RowScope::Keys(keys) = scope
        && keys.is_empty()
    {
        return Ok(None);
    }

    let mut select = vec![read_expr(&quote(&key)?, &key_spec.ty.base)];
    let key_slot = Slot {
        column: key.clone(),
        kind: ValueKind::of(&key_spec.ty.base),
        value_at: 0,
        default_at: None,
        has_default: key_spec.default.is_some(),
    };

    let mut columns = Vec::new();
    for (column, spec) in &table.columns {
        if *column == key {
            continue;
        }
        let quoted = quote(column)?;
        let value_at = select.len();
        select.push(read_expr(&quoted, &spec.ty.base));
        let default_at = match &spec.default {
            Some(default) if comparable(&spec.ty.base) => {
                select.push(format!(
                    // Both halves, because `=` is UNKNOWN for a NULL on either
                    // side and `DEFAULT NULL` is a real declaration.
                    "CASE WHEN {quoted} = ({default}) OR ({quoted} IS NULL AND ({default}) IS NULL) \
                     THEN 1 ELSE 0 END"
                ));
                Some(select.len() - 1)
            }
            _ => None,
        };
        columns.push(Slot {
            column: column.clone(),
            kind: ValueKind::of(&spec.ty.base),
            value_at,
            default_at,
            has_default: spec.default.is_some(),
        });
    }

    let mut sql = format!(
        "SELECT {}\n  FROM {}",
        select
            .iter()
            .enumerate()
            .map(|(i, e)| format!("{e} AS c{i}"))
            .collect::<Vec<_>>()
            .join(",\n       "),
        qualified(name)?
    );
    if let RowScope::Keys(keys) = scope {
        // The same rendering the emitter's `WHERE key = N'...'` uses: always a
        // string literal, converted by the engine to the key column's type.
        let list: Vec<String> = keys.iter().map(|k| literal(k.as_str())).collect();
        sql.push_str(&format!(
            "\n WHERE {} IN ({})",
            quote(&key)?,
            list.join(", ")
        ));
    }
    sql.push(';');

    Ok(Some(RowQuery {
        sql,
        key: key_slot,
        columns,
    }))
}

/// The expression that renders one column as the text the state will hold.
///
/// Styles are fixed so the spelling never depends on a session setting:
/// `126` is ISO 8601 for every date and time type, `1` keeps the `0x` prefix
/// on binary, and `3` is the round-trippable 17-digit form for floats. A
/// fixed-width `char` is trimmed because the engine itself ignores the padding
/// when it compares — `'ab' = 'ab   '` — and a declaration should not have to
/// count spaces to agree with it.
fn read_expr(quoted: &str, base: &str) -> String {
    match base {
        "date" | "time" | "datetime" | "datetime2" | "datetimeoffset" | "smalldatetime" => {
            format!("CONVERT(nvarchar(max), {quoted}, 126)")
        }
        "float" | "real" => format!("CONVERT(nvarchar(max), {quoted}, 3)"),
        "binary" | "varbinary" | "timestamp" => format!("CONVERT(nvarchar(max), {quoted}, 1)"),
        "image" => format!("CONVERT(nvarchar(max), CONVERT(varbinary(max), {quoted}), 1)"),
        // CLR types have no conversion to a string type; they render themselves.
        "geometry" | "geography" | "hierarchyid" => format!("{quoted}.ToString()"),
        "char" | "nchar" => format!("RTRIM({quoted})"),
        _ => format!("CONVERT(nvarchar(max), {quoted})"),
    }
}

/// Whether `=` is defined on the type. Where it is not, the default flag is
/// left out and the cell is always read explicit.
fn comparable(base: &str) -> bool {
    !matches!(
        base,
        "xml" | "geometry" | "geography" | "text" | "ntext" | "image"
    )
}

/// One cell's text as a model value.
pub fn value_of(kind: ValueKind, text: &str) -> Option<Value> {
    Some(match kind {
        ValueKind::Bool => match text {
            "1" => Value::Bool(true),
            "0" => Value::Bool(false),
            _ => return None,
        },
        ValueKind::Int => Value::Int(text.parse().ok()?),
        ValueKind::Text => Value::Text(text.to_owned()),
    })
}

/// The canonical cell: `None` means the column is omitted from the row.
///
/// The rules are in the module docs; they are what make two reads of the same
/// table produce the same [`Row`], which `StateSnapshot::matches` relies on.
pub fn canonical(
    slot: &Slot,
    text: Option<&str>,
    is_default: bool,
) -> Result<Option<Value>, &'static str> {
    if is_default {
        return Ok(None);
    }
    match text {
        None if !slot.has_default => Ok(None),
        None => Ok(Some(Value::Null)),
        Some(t) => value_of(slot.kind, t).map(Some).ok_or(slot.kind.name()),
    }
}

/// Reads one result row into the model.
pub fn decode(
    name: &TableName,
    query: &RowQuery,
    row: &pbps_db::Row,
) -> Result<(RowKey, Row), RowsError> {
    let read = |table: &TableName, source: DbError| RowsError::Read {
        table: table.clone(),
        source: Box::new(source),
    };
    let key_text: Option<&str> = row
        .try_get_at(query.key.value_at)
        .map_err(|e| read(name, e))?;
    let Some(key_text) = key_text else {
        // A NULL primary key cannot exist; a NULL here means the query and the
        // slots disagree.
        return Err(read(
            name,
            DbError::BadRow("the key column came back NULL".to_owned()),
        ));
    };

    let mut cells = BTreeMap::new();
    for slot in &query.columns {
        let text: Option<&str> = row.try_get_at(slot.value_at).map_err(|e| read(name, e))?;
        let is_default = match slot.default_at {
            Some(at) => {
                row.try_get_at::<i32>(at)
                    .map_err(|e| read(name, e))?
                    .unwrap_or(0)
                    == 1
            }
            None => false,
        };
        match canonical(slot, text, is_default) {
            Ok(Some(v)) => {
                cells.insert(slot.column.clone(), v);
            }
            Ok(None) => {}
            Err(kind) => {
                return Err(RowsError::BadValue {
                    table: name.clone(),
                    column: slot.column.clone(),
                    text: text.unwrap_or_default().to_owned(),
                    kind,
                });
            }
        }
    }
    Ok((RowKey::from(key_text), Row(cells)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, ColumnType, PrimaryKey};
    use std::str::FromStr;

    fn table(pk: Option<Vec<&str>>, columns: &[(&str, &str, Option<&str>)]) -> Table {
        let mut t = Table::default();
        for (name, ty, default) in columns {
            let mut c = Column::new(ColumnType::from_str(ty).unwrap());
            c.default = default.map(str::to_owned);
            t.columns.insert((*name).to_owned(), c);
        }
        t.primary_key = pk.map(|c| PrimaryKey {
            name: None,
            columns: c.into_iter().map(str::to_owned).collect(),
        });
        t
    }

    fn name() -> TableName {
        TableName::new("dbo", "status")
    }

    #[test]
    fn the_query_reads_the_key_first_then_every_other_column() {
        let t = table(
            Some(vec!["code"]),
            &[
                ("code", "varchar(20)", None),
                ("label", "nvarchar(50)", Some("'Unlabelled'")),
                ("rank", "int", None),
            ],
        );
        let q = query(&name(), &t, &RowScope::Every).unwrap().unwrap();
        assert_eq!(q.key.value_at, 0);
        assert_eq!(q.key.column, "code");
        assert_eq!(q.columns.len(), 2);
        assert_eq!(q.columns[0].column, "label");
        assert_eq!(q.columns[0].value_at, 1);
        assert_eq!(q.columns[0].default_at, Some(2));
        assert_eq!(q.columns[1].column, "rank");
        assert_eq!(q.columns[1].value_at, 3);
        assert_eq!(q.columns[1].default_at, None);
        assert_eq!(q.columns[1].kind, ValueKind::Int);
        assert!(q.sql.contains("FROM [dbo].[status]"), "{}", q.sql);
        assert!(!q.sql.contains("WHERE"), "{}", q.sql);
        assert!(
            q.sql.contains(
                "CASE WHEN [label] = ('Unlabelled') OR ([label] IS NULL AND ('Unlabelled') IS NULL)"
            ),
            "{}",
            q.sql
        );
        assert!(q.sql.trim_end().ends_with(';'), "{}", q.sql);
    }

    #[test]
    fn an_ensure_scope_asks_only_for_its_keys_as_string_literals() {
        let t = table(Some(vec!["id"]), &[("id", "int", None), ("n", "int", None)]);
        let keys = ["7", "o'k"].into_iter().map(RowKey::from).collect();
        let q = query(&name(), &t, &RowScope::Keys(keys)).unwrap().unwrap();
        assert!(q.sql.contains("WHERE [id] IN (N'7', N'o''k')"), "{}", q.sql);
    }

    /// `IN ()` is not T-SQL, and nothing is being asked for anyway.
    #[test]
    fn an_empty_key_scope_reads_nothing() {
        let t = table(Some(vec!["id"]), &[("id", "int", None)]);
        assert!(
            query(&name(), &t, &RowScope::Keys(Default::default()))
                .unwrap()
                .is_none()
        );
    }

    /// Unreadable is an error, never an empty answer.
    #[test]
    fn a_table_whose_rows_have_no_identity_is_refused_by_name() {
        let none = table(None, &[("id", "int", None)]);
        let e = query(&name(), &none, &RowScope::Every).unwrap_err();
        assert!(e.to_string().contains("no primary key"), "{e}");
        let two = table(
            Some(vec!["a", "b"]),
            &[("a", "int", None), ("b", "int", None)],
        );
        let e = query(&name(), &two, &RowScope::Every).unwrap_err();
        assert!(e.to_string().contains("2 columns"), "{e}");
    }

    #[test]
    fn types_without_equality_get_no_default_flag() {
        let t = table(
            Some(vec!["id"]),
            &[
                ("id", "int", None),
                ("doc", "xml", Some("''")),
                ("blob", "image", Some("0x")),
            ],
        );
        let q = query(&name(), &t, &RowScope::Every).unwrap().unwrap();
        assert!(q.columns.iter().all(|s| s.default_at.is_none()), "{q:?}");
        assert!(q.columns.iter().all(|s| s.has_default), "{q:?}");
    }

    #[test]
    fn the_engine_spells_dates_binary_and_floats_with_fixed_styles() {
        assert_eq!(
            read_expr("[d]", "datetime2"),
            "CONVERT(nvarchar(max), [d], 126)"
        );
        assert_eq!(
            read_expr("[b]", "varbinary"),
            "CONVERT(nvarchar(max), [b], 1)"
        );
        assert_eq!(read_expr("[f]", "float"), "CONVERT(nvarchar(max), [f], 3)");
        assert_eq!(read_expr("[c]", "char"), "RTRIM([c])");
        assert_eq!(read_expr("[g]", "geography"), "[g].ToString()");
        assert_eq!(read_expr("[n]", "decimal"), "CONVERT(nvarchar(max), [n])");
    }

    #[test]
    fn bits_and_integers_come_back_typed_and_everything_else_as_text() {
        assert_eq!(value_of(ValueKind::Bool, "1"), Some(Value::Bool(true)));
        assert_eq!(value_of(ValueKind::Bool, "0"), Some(Value::Bool(false)));
        assert_eq!(value_of(ValueKind::Bool, "true"), None);
        assert_eq!(value_of(ValueKind::Int, "-7"), Some(Value::Int(-7)));
        assert_eq!(value_of(ValueKind::Int, "1.5"), None);
        assert_eq!(
            value_of(ValueKind::Text, "1.50"),
            Some(Value::Text("1.50".to_owned()))
        );
        assert_eq!(ValueKind::of("bigint"), ValueKind::Int);
        assert_eq!(ValueKind::of("bit"), ValueKind::Bool);
        assert_eq!(ValueKind::of("decimal"), ValueKind::Text);
    }

    /// The canonical form is what lets two reads of one table be `==`: a cell
    /// holding its default is omitted, a NULL is omitted only where the model
    /// already reads omission as NULL, and everything else is explicit.
    #[test]
    fn a_cell_equal_to_its_default_is_read_back_as_omitted() {
        let with_default = Slot {
            column: "label".into(),
            kind: ValueKind::Text,
            value_at: 1,
            default_at: Some(2),
            has_default: true,
        };
        assert_eq!(canonical(&with_default, Some("Unlabelled"), true), Ok(None));
        assert_eq!(
            canonical(&with_default, Some("New"), false),
            Ok(Some(Value::Text("New".into())))
        );
        // A NULL where a default exists is *not* the default: it was sent, and
        // an omitted cell would say the opposite.
        assert_eq!(canonical(&with_default, None, false), Ok(Some(Value::Null)));

        let without = Slot {
            has_default: false,
            default_at: None,
            ..with_default
        };
        assert_eq!(canonical(&without, None, false), Ok(None));
        // And a value the mapping cannot hold names the kind it expected.
        let int = Slot {
            kind: ValueKind::Int,
            ..without
        };
        assert_eq!(canonical(&int, Some("x"), false), Err("integer"));
    }
}
