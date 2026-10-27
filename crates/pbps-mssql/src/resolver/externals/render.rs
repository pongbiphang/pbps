//! Pure catalog-to-DDL templates. No module definition, expression, assembly
//! or synonym target is used as executable input. Complete staged comparison
//! remains the admission gate, including every unknown scalar property.

use crate::resolver::capture::{CapturedInputs, ExternalInputs, ObjectIdentity, Uncovered, class};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
pub struct Renderable {
    pub object: ObjectIdentity,
    pub statements: Vec<String>,
}

pub(crate) fn ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}
fn qualified(id: &ObjectIdentity) -> Result<String, Uncovered> {
    match id.name.as_slice() {
        [schema, name] => Ok(format!("{}.{}", ident(schema), ident(name))),
        _ => Err(Uncovered::object(
            id,
            "the renderer needs a schema-qualified identity",
        )),
    }
}
fn value<'a>(
    p: &'a BTreeMap<String, Value>,
    key: &str,
    id: &ObjectIdentity,
) -> Result<&'a Value, Uncovered> {
    p.get(key).ok_or_else(|| {
        Uncovered::object(id, "the renderer cannot read a required catalog property")
    })
}
fn text<'a>(p: &'a Value, key: &str, id: &ObjectIdentity) -> Result<&'a str, Uncovered> {
    p.get(key).and_then(Value::as_str).ok_or_else(|| {
        Uncovered::object(id, "the renderer cannot read a required catalog property")
    })
}
fn number(v: &Value, id: &ObjectIdentity) -> Result<String, Uncovered> {
    let s = v
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| v.to_string());
    s.parse::<i128>()
        .map_err(|_| Uncovered::object(id, "the renderer needs an integral catalog value"))?;
    Ok(s)
}
fn type_sql(
    p: &BTreeMap<String, Value>,
    key: &str,
    id: &ObjectIdentity,
) -> Result<String, Uncovered> {
    let ty: ObjectIdentity = serde_json::from_value(value(p, key, id)?.clone())
        .map_err(|_| Uncovered::object(id, "the renderer cannot identify a type"))?;
    if ty.class != class::TYPE {
        return Err(Uncovered::object(id, "the renderer needs a type identity"));
    }
    if ty.name.first().map(String::as_str) != Some("sys") {
        return qualified(&ty);
    }
    let name = ty
        .name
        .get(1)
        .ok_or_else(|| Uncovered::object(id, "the type has no name"))?;
    let numeric = |key| number(value(p, key, id)?, id);
    let suffix = match name.as_str() {
        "char" | "varchar" | "binary" | "varbinary" | "nchar" | "nvarchar" => {
            let size = numeric("max_length")?.parse::<i128>().unwrap();
            if size == -1 {
                "(max)".into()
            } else {
                format!(
                    "({})",
                    if name.starts_with('n') {
                        size / 2
                    } else {
                        size
                    }
                )
            }
        }
        "decimal" | "numeric" => format!("({},{})", numeric("precision")?, numeric("scale")?),
        "datetime2" | "datetimeoffset" | "time" => format!("({})", numeric("scale")?),
        "float" => format!("({})", numeric("precision")?),
        "bigint" | "int" | "smallint" | "tinyint" | "bit" | "real" | "money" | "smallmoney"
        | "date" | "datetime" | "smalldatetime" | "uniqueidentifier" => String::new(),
        _ => {
            return Err(Uncovered::object(
                id,
                "the renderer has no template for this system type",
            ));
        }
    };
    Ok(format!("{}{suffix}", ident(name)))
}

/// Plan only missing roots. A managed dependency refuses before any template
/// is attempted, matching the file path (DEC-2269.1).
pub fn missing(
    inputs: &ExternalInputs,
    present: &BTreeSet<ObjectIdentity>,
) -> Result<Vec<Renderable>, Vec<String>> {
    let waiting = inputs.waiting();
    if !waiting.is_empty() {
        return Err(waiting);
    }
    let mut items = Vec::new();
    let mut refused = Vec::new();
    for id in inputs.roots.difference(present) {
        match render(&inputs.captured, id) {
            Ok(item) => items.push(item),
            Err(error) => refused.push(format!("{error:?}; supply a matching externals file")),
        }
    }
    if !refused.is_empty() {
        return Err(refused);
    }
    // Alias types precede relations; constraints run after every relation.
    items.sort_by_key(|item| {
        let kind = inputs
            .captured
            .properties(&item.object)
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str);
        (
            if item.object.class == class::TYPE {
                0
            } else if kind == Some("F") {
                2
            } else {
                1
            },
            item.object.clone(),
        )
    });
    Ok(items)
}

pub fn render(captured: &CapturedInputs, id: &ObjectIdentity) -> Result<Renderable, Uncovered> {
    let p = captured
        .properties(id)
        .ok_or_else(|| Uncovered::object(id, "the external shape is unreadable"))?;
    let name = qualified(if id.class.starts_with("pbps-unnamed-") {
        id.signature
            .first()
            .ok_or_else(|| Uncovered::object(id, "an unnamed constraint has no parent"))?
    } else {
        id
    })?;
    let refused = |why| Uncovered::object(id, why);
    let mut statements = Vec::new();
    if id.class == class::TYPE {
        if p.get("table_type") != Some(&Value::Bool(false))
            || p.get("user_defined") != Some(&Value::Bool(true))
        {
            return Err(refused("the renderer supports alias types only"));
        }
        statements.push(format!(
            "CREATE TYPE {name} FROM {} {};",
            type_sql(p, "base", id)?,
            if p.get("nullable") == Some(&Value::Bool(true)) {
                "NULL"
            } else {
                "NOT NULL"
            }
        ));
    } else {
        match p.get("type").and_then(Value::as_str) {
            Some("U" | "V") => {
                let view = p.get("type").and_then(Value::as_str) == Some("V");
                let mut columns: Vec<_> = captured
                    .objects()
                    .filter(|c| c.class == class::COLUMN && c.signature.first() == Some(id))
                    .collect();
                columns.sort_by_key(|c| {
                    captured
                        .properties(c)
                        .and_then(|p| p.get("column_id"))
                        .and_then(Value::as_i64)
                });
                let mut definitions = Vec::new();
                for column in columns {
                    let cp = captured.properties(column).unwrap();
                    if cp.get("computed").is_some_and(|v| !v.is_null()) {
                        return Err(refused(
                            "computed-column expressions are never replayed by the renderer",
                        ));
                    }
                    let ty = type_sql(cp, "type", id)?;
                    let column_name = ident(&column.name[0]);
                    let nullable = cp.get("nullable") == Some(&Value::Bool(true));
                    let collation = cp
                        .get("collation")
                        .and_then(Value::as_str)
                        .map(|c| {
                            if c.is_empty()
                                || !c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                            {
                                return Err(refused(
                                    "the catalog collation is not a safe engine collation name",
                                ));
                            }
                            Ok(format!(" COLLATE {c}"))
                        })
                        .transpose()?
                        .unwrap_or_default();
                    if view {
                        if !nullable {
                            return Err(refused(
                                "a non-nullable view column has no qualified shape template",
                            ));
                        }
                        definitions.push(format!("CAST(NULL AS {ty}){collation} AS {column_name}"));
                    } else {
                        let identity = if let Some(parts) =
                            cp.get("identity_spec").and_then(Value::as_array)
                        {
                            if parts.len() != 3 {
                                return Err(refused("an identity specification is incomplete"));
                            }
                            format!(
                                " IDENTITY({},{}){}",
                                number(&parts[0], id)?,
                                number(&parts[1], id)?,
                                if parts[2] == Value::Bool(true) {
                                    " NOT FOR REPLICATION"
                                } else {
                                    ""
                                }
                            )
                        } else {
                            String::new()
                        };
                        definitions.push(format!(
                            "{column_name} {ty}{collation}{identity} {}",
                            if nullable { "NULL" } else { "NOT NULL" }
                        ));
                    }
                }
                if definitions.is_empty() {
                    return Err(refused("the renderer cannot read any columns"));
                }
                if view {
                    module_settings(p, &mut statements, id)?;
                    statements.push(format!(
                        "CREATE VIEW {name}{} AS SELECT {} WHERE 1=0;",
                        if p.get("is_schema_bound") == Some(&Value::Bool(true)) {
                            " WITH SCHEMABINDING"
                        } else {
                            ""
                        },
                        definitions.join(", ")
                    ));
                } else {
                    statements.push(format!("CREATE TABLE {name}({});", definitions.join(", ")));
                    indexes(captured, p, id, &name, &mut statements)?;
                }
            }
            Some("SO") => {
                let row = value(p, "sys.sequences", id)?;
                let mut facets = p.clone();
                for facet in ["precision", "scale", "max_length"] {
                    if let Some(v) = row.get(facet) {
                        facets.insert(facet.into(), v.clone());
                    }
                }
                facets.insert(
                    "sequence_type".into(),
                    row.get("user_type_id")
                        .cloned()
                        .ok_or_else(|| refused("the sequence type is unreadable"))?,
                );
                let ty = type_sql(&facets, "sequence_type", id)?;
                let cache = if text(row, "is_cached", id)? == "1" {
                    match row.get("cache_size").filter(|v| !v.is_null()) {
                        Some(n) => format!("CACHE {}", number(n, id)?),
                        None => "CACHE".into(),
                    }
                } else {
                    "NO CACHE".into()
                };
                statements.push(format!("CREATE SEQUENCE {name} AS {ty} START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} {} {cache};", number(&row["start_value"],id)?, number(&row["increment"],id)?, number(&row["minimum_value"],id)?, number(&row["maximum_value"],id)?, if text(row,"is_cycling",id)? == "1" { "CYCLE" } else { "NO CYCLE" }));
            }
            Some("FN") => {
                let props = value(p, "OBJECTPROPERTYEX", id)?;
                if props.get("UserDataAccess") != Some(&Value::from(0))
                    || props.get("SystemDataAccess") != Some(&Value::from(0))
                    || props.get("IsPrecise") != Some(&Value::from(1))
                {
                    return Err(refused(
                        "the routine's data-access or precision properties have no qualified template",
                    ));
                }
                let parameters = value(p, "parameters", id)?
                    .as_array()
                    .ok_or_else(|| refused("the routine parameters are unreadable"))?;
                let mut args = Vec::new();
                let mut returns = None;
                for parameter in parameters {
                    let map: BTreeMap<_, _> = parameter
                        .as_object()
                        .ok_or_else(|| refused("invalid routine parameter"))?
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    let ty = type_sql(&map, "type", id)?;
                    let n = text(parameter, "name", id)?;
                    if n == "return" {
                        returns = Some(ty);
                    } else {
                        if !n.starts_with('@')
                            || !n[1..]
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '_')
                            || parameter["output"] == true
                            || parameter["readonly"] == true
                            || parameter["has_default"] == true
                        {
                            return Err(refused("the routine parameter has no qualified template"));
                        }
                        args.push(format!("{n} {ty}"));
                    }
                }
                if p.get("execute_as").and_then(Value::as_str) != Some("caller") {
                    return Err(refused(
                        "the routine execution context has no qualified template",
                    ));
                }
                module_settings(p, &mut statements, id)?;
                let binding = if p.get("is_schema_bound") == Some(&Value::Bool(true)) {
                    " WITH SCHEMABINDING"
                } else {
                    ""
                };
                statements.push(format!(
                    "CREATE FUNCTION {name}({}) RETURNS {}{binding} AS BEGIN RETURN NULL; END;",
                    args.join(", "),
                    returns.ok_or_else(|| refused("the routine return type is unreadable"))?
                ));
            }
            Some("F") => {
                let fk = value(p, "foreign_key", id)?;
                let parent: ObjectIdentity =
                    serde_json::from_value(value(p, "parent", id)?.clone())
                        .map_err(|_| refused("the foreign key's parent is unreadable"))?;
                let referenced: ObjectIdentity =
                    serde_json::from_value(fk["references"].clone())
                        .map_err(|_| refused("the foreign key's reference is unreadable"))?;
                let pairs = fk["columns"]
                    .as_array()
                    .ok_or_else(|| refused("the foreign key's columns are unreadable"))?;
                let mut a = Vec::new();
                let mut b = Vec::new();
                for pair in pairs {
                    a.push(ident(
                        pair[0]
                            .as_str()
                            .ok_or_else(|| refused("invalid foreign key column"))?,
                    ));
                    b.push(ident(
                        pair[1]
                            .as_str()
                            .ok_or_else(|| refused("invalid foreign key target column"))?,
                    ));
                }
                let action = |key| match fk[key].as_str() {
                    Some("CASCADE") => Ok("CASCADE"),
                    Some("NO_ACTION") => Ok("NO ACTION"),
                    _ => Err(refused("the cascade action has no qualified template")),
                };
                statements.push(format!("ALTER TABLE {} WITH {} ADD {}FOREIGN KEY({}) REFERENCES {}({}) ON DELETE {} ON UPDATE {}{};", qualified(&parent)?, if fk["trusted"] == true { "CHECK" } else { "NOCHECK" }, constraint_prefix(id), a.join(","), qualified(&referenced)?, b.join(","), action("on_delete")?,action("on_update")?, if fk["not_for_replication"] == true { " NOT FOR REPLICATION" } else { "" }));
                if fk["disabled"] == true {
                    if id.class.starts_with("pbps-unnamed-") {
                        let create = statements.pop().expect("the foreign key was just emitted");
                        statements.push(disable_created(
                            &create,
                            &qualified(&parent)?,
                            "foreign_keys",
                            "object_id",
                            "parent_object_id",
                            true,
                        ));
                    } else {
                        statements.push(format!(
                            "ALTER TABLE {} NOCHECK CONSTRAINT {};",
                            qualified(&parent)?,
                            ident(&id.name[1])
                        ));
                    }
                }
            }
            _ => {
                return Err(refused(
                    "the renderer has no qualified template for this object class",
                ));
            }
        }
    }
    Ok(Renderable {
        object: id.clone(),
        statements,
    })
}

fn module_settings(
    p: &BTreeMap<String, Value>,
    sql: &mut Vec<String>,
    id: &ObjectIdentity,
) -> Result<(), Uncovered> {
    for (key, setting) in [
        ("uses_ansi_nulls", "ANSI_NULLS"),
        ("uses_quoted_identifier", "QUOTED_IDENTIFIER"),
    ] {
        let on = value(p, key, id)?
            .as_bool()
            .ok_or_else(|| Uncovered::object(id, "unreadable module settings"))?;
        sql.push(format!("SET {setting} {};", if on { "ON" } else { "OFF" }));
    }
    Ok(())
}

fn indexes(
    captured: &CapturedInputs,
    p: &BTreeMap<String, Value>,
    id: &ObjectIdentity,
    name: &str,
    sql: &mut Vec<String>,
) -> Result<(), Uncovered> {
    let mut indexes: Vec<_> = p
        .get("sys.indexes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    indexes.sort_by_key(|index| if index["index"]["type"] == "1" { 0 } else { 1 });
    for index in indexes {
        let row = &index["index"];
        if row["is_unique"] != "1" {
            continue;
        }
        let kind = match row["type"].as_str() {
            Some("1") => "CLUSTERED",
            Some("2") => "NONCLUSTERED",
            _ => {
                return Err(Uncovered::object(
                    id,
                    "only ordinary unique B-tree indexes have a qualified template",
                ));
            }
        };
        if row["has_filter"] == "1" {
            return Err(Uncovered::object(
                id,
                "filtered-index expressions are never replayed",
            ));
        }
        let index_name = index
            .get("name")
            .ok_or_else(|| Uncovered::object(id, "unreadable index identity"))?;
        let mut keys = Vec::new();
        let mut included = Vec::new();
        for c in index["columns"]
            .as_array()
            .ok_or_else(|| Uncovered::object(id, "unreadable index columns"))?
        {
            let column = ident(text(c, "column_id", id)?);
            if c["is_included_column"] == "1" {
                included.push(column);
            } else if c["key_ordinal"] != "0" {
                keys.push((
                    number(&c["key_ordinal"], id)?.parse::<i128>().unwrap(),
                    format!(
                        "{column} {}",
                        if c["is_descending_key"] == "1" {
                            "DESC"
                        } else {
                            "ASC"
                        }
                    ),
                ));
            }
        }
        keys.sort();
        let key_columns = keys
            .into_iter()
            .map(|(_, s)| s)
            .collect::<Vec<_>>()
            .join(",");
        let constraint = captured.objects().find(|child| {
            captured.properties(child).is_some_and(|cp| {
                cp.get("parent") == Some(&serde_json::json!(id))
                    && cp
                        .get("sys.key_constraints")
                        .and_then(|k| k.get("unique_index_id"))
                        == Some(index_name)
            })
        });
        let fill = number(&row["fill_factor"], id)?;
        let fill = if fill == "0" {
            String::new()
        } else {
            format!("FILLFACTOR={fill}, ")
        };
        let options = format!(
            "WITH (PAD_INDEX={}, {fill}IGNORE_DUP_KEY={}, ALLOW_ROW_LOCKS={}, ALLOW_PAGE_LOCKS={})",
            if row["is_padded"] == "1" { "ON" } else { "OFF" },
            if row["ignore_dup_key"] == "1" {
                "ON"
            } else {
                "OFF"
            },
            if row["allow_row_locks"] == "1" {
                "ON"
            } else {
                "OFF"
            },
            if row["allow_page_locks"] == "1" {
                "ON"
            } else {
                "OFF"
            }
        );
        if let Some(child) = constraint {
            let pk = captured
                .properties(child)
                .and_then(|cp| cp.get("type"))
                .and_then(Value::as_str)
                == Some("PK");
            sql.push(format!(
                "ALTER TABLE {name} ADD {}{} {kind} ({key_columns}) {options};",
                if child.class.starts_with("pbps-unnamed-") {
                    String::new()
                } else {
                    format!("CONSTRAINT {} ", ident(&child.name[1]))
                },
                if pk { "PRIMARY KEY" } else { "UNIQUE" }
            ));
        } else {
            sql.push(format!(
                "CREATE UNIQUE {kind} INDEX {} ON {name} ({key_columns}){} {options};",
                ident(
                    index_name
                        .as_str()
                        .ok_or_else(|| Uncovered::object(id, "unreadable named index"))?
                ),
                if included.is_empty() {
                    String::new()
                } else {
                    format!(" INCLUDE ({})", included.join(","))
                }
            ));
        }
        if row["is_disabled"] == "1" {
            if index_name.is_object() {
                let create = sql.pop().expect("the index was just emitted");
                sql.push(disable_created(
                    &create,
                    name,
                    "indexes",
                    "index_id",
                    "object_id",
                    false,
                ));
            } else {
                sql.push(format!(
                    "ALTER INDEX {} ON {name} DISABLE;",
                    ident(
                        index_name
                            .as_str()
                            .ok_or_else(|| Uncovered::object(id, "unreadable named index"))?
                    )
                ));
            }
        }
    }
    Ok(())
}

fn constraint_prefix(id: &ObjectIdentity) -> String {
    if id.class.starts_with("pbps-unnamed-") {
        String::new()
    } else {
        format!("CONSTRAINT {} ", ident(&id.name[1]))
    }
}

// The engine chose the new name. Read precisely the one new catalog child
// around its CREATE, rather than guessing its name or disabling its siblings.
fn disable_created(
    create: &str,
    table: &str,
    view: &str,
    key: &str,
    parent: &str,
    foreign: bool,
) -> String {
    let table_literal = crate::ident::literal(table);
    let alter = if foreign {
        format!("ALTER TABLE {table} NOCHECK CONSTRAINT ")
    } else {
        "ALTER INDEX ".into()
    };
    let suffix = if foreign {
        String::new()
    } else {
        format!(" ON {table} DISABLE;")
    };
    format!(
        "DECLARE @before TABLE(id int); INSERT @before SELECT {key} FROM sys.{view} WHERE {parent}=OBJECT_ID({table_literal}); {create} DECLARE @child sysname; SELECT @child=name FROM sys.{view} WHERE {parent}=OBJECT_ID({table_literal}) AND {key} NOT IN (SELECT id FROM @before); IF @@ROWCOUNT<>1 THROW 50000, 'the rendered child cannot be identified', 1; DECLARE @sql nvarchar(max)={} + QUOTENAME(@child) + {}; EXEC(@sql);",
        crate::ident::literal(&alter),
        crate::ident::literal(&suffix)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opaque_names_are_quoted_and_nonintegral_values_refuse() {
        assert_eq!(ident("x]; DROP TABLE y;--"), "[x]]; DROP TABLE y;--]");
        let id = crate::resolver::capture::object("dbo", "x");
        assert!(number(&Value::String("1; SELECT 2".into()), &id).is_err());
        assert_eq!(number(&Value::String("-4".into()), &id).unwrap(), "-4");
    }
}
