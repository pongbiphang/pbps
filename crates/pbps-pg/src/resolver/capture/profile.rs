//! Qualify datum rendering before calling a deparser or anyarray output.
//! A readable pg_node_tree is not permission to invoke arbitrary type I/O.

use super::{
    Uncovered,
    logical::{self, Catalog},
    nodes::{self, Value},
};
use std::collections::BTreeSet;

pub(super) fn expression(catalog: &Catalog, text: &str, major: u32) -> Result<(), Uncovered> {
    let tree = nodes::decode(text, major)
        .map_err(|_| Uncovered::class("stored-expression", "unqualified node layout"))?;
    for reference in nodes::references(&tree, major)
        .map_err(|_| Uncovered::class("stored-expression", "unqualified binding reference"))?
    {
        if reference.class == nodes::ReferenceClass::Type {
            typmod(catalog, reference.oid, &mut BTreeSet::new())?;
        }
    }
    fn walk(catalog: &Catalog, value: &Value) -> Result<(), Uncovered> {
        match value {
            Value::Node(node) => {
                if node.tag == "CONST"
                    && node.fields.get("constisnull") != Some(&Value::Atom("true".into()))
                {
                    let oid = node
                        .number("consttype")
                        .map_err(|_| Uncovered::class("pg_type", "missing constant type"))?;
                    datum(catalog, oid)?;
                }
                for child in node.fields.values() {
                    walk(catalog, child)?;
                }
            }
            Value::List(children) => {
                for child in children {
                    walk(catalog, child)?;
                }
            }
            Value::Null | Value::Atom(_) | Value::Datum => {}
        }
        Ok(())
    }
    walk(catalog, &tree)
}

pub(super) fn datum(catalog: &Catalog, oid: u32) -> Result<(), Uncovered> {
    fn output(catalog: &Catalog, oid: u32, visiting: &mut BTreeSet<u32>) -> Result<(), Uncovered> {
        let id = catalog
            .object("pg_type", oid)
            .map_err(|_| Uncovered::class("pg_type", "unreadable datum type"))?;
        let fail = || Uncovered::object(&id, "unqualified datum output");
        if visiting.len() >= 64 || !visiting.insert(oid) {
            return Err(fail());
        }
        let row = catalog.row("pg_type", oid).map_err(|_| fail())?;
        let kind = logical::string(row, "typtype").map_err(|_| fail())?;
        if kind == "d" {
            output(
                catalog,
                logical::number(row, "typbasetype").map_err(|_| fail())?,
                visiting,
            )?;
        } else {
            let function = catalog
                .row(
                    "pg_proc",
                    logical::number(row, "typoutput").map_err(|_| fail())?,
                )
                .map_err(|_| fail())?;
            let function_id = catalog.identity("pg_proc", function).map_err(|_| fail())?;
            let language = catalog
                .object(
                    "pg_language",
                    logical::number(function, "prolang").map_err(|_| fail())?,
                )
                .map_err(|_| fail())?;
            let name = id.name.last().ok_or_else(fail)?;
            let element = logical::number(row, "typelem").map_err(|_| fail())?;
            let symbol = if kind == "e" {
                "enum_out"
            } else if kind == "c" {
                "record_out"
            } else if element != 0 && logical::string(row, "typcategory") == Ok("A") {
                "array_out"
            } else if id.name.first().map(String::as_str) == Some("pg_catalog") {
                match name.as_str() {
                    "bool" => "boolout",
                    "bytea" => "byteaout",
                    "char" => "charout",
                    "name" => "nameout",
                    "int2" => "int2out",
                    "int4" => "int4out",
                    "int8" => "int8out",
                    "text" => "textout",
                    "varchar" => "varcharout",
                    "bpchar" => "bpcharout",
                    "numeric" => "numeric_out",
                    "float4" => "float4out",
                    "float8" => "float8out",
                    "date" => "date_out",
                    "time" => "time_out",
                    "timetz" => "timetz_out",
                    "timestamp" => "timestamp_out",
                    "timestamptz" => "timestamptz_out",
                    "interval" => "interval_out",
                    "uuid" => "uuid_out",
                    "json" => "json_out",
                    "jsonb" => "jsonb_out",
                    "bit" => "bit_out",
                    "varbit" => "varbit_out",
                    "oid" => "oidout",
                    _ => return Err(fail()),
                }
            } else {
                return Err(fail());
            };
            // A user alias to a builtin symbol is still an unqualified type.
            // Build/content qualification remains the native lifecycle's job.
            if function_id.name != ["pg_catalog", symbol]
                || language.name != ["internal"]
                || logical::string(function, "prosrc") != Ok(symbol)
            {
                return Err(fail());
            }
            if symbol == "array_out" {
                output(catalog, element, visiting)?;
            }
            if kind == "c" {
                let relation = logical::number(row, "typrelid").map_err(|_| fail())?;
                for attribute in &catalog.rows["pg_attribute"] {
                    if logical::number(attribute, "attrelid") == Ok(relation)
                        && logical::signed(attribute, "attnum").is_ok_and(|n| n > 0)
                        && attribute.get("attisdropped") == Some(&serde_json::Value::Bool(false))
                    {
                        output(
                            catalog,
                            logical::number(attribute, "atttypid").map_err(|_| fail())?,
                            visiting,
                        )?;
                    }
                }
            }
        }
        visiting.remove(&oid);
        Ok(())
    }
    output(catalog, oid, &mut BTreeSet::new())
}

// Deparsers can invoke typmod output even when an expression has no Const,
// for example a length-coercion FuncExpr or a record function's column list.
fn typmod(catalog: &Catalog, oid: u32, visiting: &mut BTreeSet<u32>) -> Result<(), Uncovered> {
    let id = catalog
        .object("pg_type", oid)
        .map_err(|_| Uncovered::class("pg_type", "unreadable type modifier"))?;
    let fail = || Uncovered::object(&id, "unqualified type modifier output");
    if visiting.len() >= 64 || !visiting.insert(oid) {
        return Err(fail());
    }
    let row = catalog.row("pg_type", oid).map_err(|_| fail())?;
    let output = logical::number(row, "typmodout").map_err(|_| fail())?;
    if output != 0 {
        let function = catalog.row("pg_proc", output).map_err(|_| fail())?;
        let function_id = catalog.identity("pg_proc", function).map_err(|_| fail())?;
        let language = catalog
            .object(
                "pg_language",
                logical::number(function, "prolang").map_err(|_| fail())?,
            )
            .map_err(|_| fail())?;
        let symbol = logical::string(function, "prosrc").map_err(|_| fail())?;
        if !matches!(
            symbol,
            "bpchartypmodout"
                | "varchartypmodout"
                | "numerictypmodout"
                | "bittypmodout"
                | "varbittypmodout"
                | "timetypmodout"
                | "timetztypmodout"
                | "timestamptypmodout"
                | "timestamptztypmodout"
                | "intervaltypmodout"
        ) || function_id.name != ["pg_catalog", symbol]
            || language.name != ["internal"]
        {
            return Err(fail());
        }
    }
    for field in ["typbasetype", "typelem"] {
        let child = logical::number(row, field).map_err(|_| fail())?;
        if child != 0 {
            typmod(catalog, child, visiting)?;
        }
    }
    visiting.remove(&oid);
    Ok(())
}
