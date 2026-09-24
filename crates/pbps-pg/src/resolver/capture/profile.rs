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

// Exact output identities measured on both qualified majors. Some builtins
// share implementations; the SQL function name and implementation symbol are
// independently qualified. OID aliases and internal-only types stay refused.
fn builtin_output(name: &str) -> Option<(&'static str, &'static str)> {
    let function = match name {
        "bit" => "bit_out",
        "bool" => "boolout",
        "box" => "box_out",
        "bpchar" => "bpcharout",
        "bytea" => "byteaout",
        "char" => "charout",
        "cid" => "cidout",
        "cidr" => "cidr_out",
        "circle" => "circle_out",
        "date" => "date_out",
        "float4" => "float4out",
        "float8" => "float8out",
        "inet" => "inet_out",
        "int2" => "int2out",
        "int2vector" => "int2vectorout",
        "int4" => "int4out",
        "int8" => "int8out",
        "interval" => "interval_out",
        "json" => "json_out",
        "jsonb" => "jsonb_out",
        "jsonpath" => "jsonpath_out",
        "line" => "line_out",
        "lseg" => "lseg_out",
        "macaddr" => "macaddr_out",
        "macaddr8" => "macaddr8_out",
        "money" => "cash_out",
        "name" => "nameout",
        "numeric" => "numeric_out",
        "oid" => "oidout",
        "oidvector" => "oidvectorout",
        "path" => "path_out",
        "pg_lsn" => "pg_lsn_out",
        "pg_snapshot" => "pg_snapshot_out",
        "point" => "point_out",
        "polygon" => "poly_out",
        "refcursor" => "textout",
        "text" => "textout",
        "tid" => "tidout",
        "time" => "time_out",
        "timestamp" => "timestamp_out",
        "timestamptz" => "timestamptz_out",
        "timetz" => "timetz_out",
        "tsquery" => "tsqueryout",
        "tsvector" => "tsvectorout",
        "txid_snapshot" => "txid_snapshot_out",
        "unknown" => "unknownout",
        "uuid" => "uuid_out",
        "varbit" => "varbit_out",
        "varchar" => "varcharout",
        "xid" => "xidout",
        "xid8" => "xid8out",
        "xml" => "xml_out",
        _ => return None,
    };
    let symbol = if function == "txid_snapshot_out" {
        "pg_snapshot_out"
    } else {
        function
    };
    Some((function, symbol))
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
            let builtin = if id.name.first().map(String::as_str) == Some("pg_catalog") {
                builtin_output(name)
            } else {
                None
            };
            let (function_name, symbol) = if kind == "e" {
                ("enum_out", "enum_out")
            } else if kind == "c" {
                ("record_out", "record_out")
            } else if let Some(builtin) = builtin {
                // int2vector/oidvector are category A but use their own
                // exact builtin outputs, not generic array_out.
                builtin
            } else if element != 0 && logical::string(row, "typcategory") == Ok("A") {
                ("array_out", "array_out")
            } else {
                return Err(fail());
            };
            // A user alias to a builtin symbol is still an unqualified type.
            // Build/content qualification remains the native lifecycle's job.
            if function_id.name != ["pg_catalog", function_name]
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn measured_scalar_outputs_require_the_exact_function_namespace_language_and_symbol() {
        let handlers: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("fixtures/scalar-outputs.json")).unwrap();
        for handler in handlers {
            let name = handler["name"].as_str().unwrap();
            for variant in [
                "builtin",
                "function_name",
                "function_namespace",
                "symbol",
                "language",
                "type_namespace",
            ] {
                let row = |v: serde_json::Value| v.as_object().unwrap().clone();
                let catalog = Catalog::new(BTreeMap::from([
                    ("pg_namespace".into(),vec![row(json!({"oid":11,"nspname":"pg_catalog"})),row(json!({"oid":12,"nspname":"app"}))]),
                    ("pg_language".into(),vec![row(json!({"oid":13,"lanname":if variant=="language" {"sql"} else {"internal"}}))]),
                    ("pg_proc".into(),vec![row(json!({"oid":14,"proname":if variant=="function_name" {"wrong"} else {handler["proname"].as_str().unwrap()},"pronamespace":if variant=="function_namespace" {12} else {11},"proargtypes":[100],"prolang":13,"prosrc":if variant=="symbol" {"user_output"} else {handler["prosrc"].as_str().unwrap()}}))]),
                    ("pg_type".into(),vec![row(json!({"oid":100,"typname":name,"typnamespace":if variant=="type_namespace" {12} else {11},"typtype":"b","typoutput":14,"typelem":0,"typcategory":"U"}))]),
                ])).unwrap();
                assert_eq!(
                    datum(&catalog, 100).is_ok(),
                    variant == "builtin",
                    "{name}/{variant}"
                );
            }
        }
        // Rendering OID-bearing aliases needs an additional logical-binding
        // proof. An output function allowlist cannot supply that proof.
        for name in [
            "regclass",
            "regproc",
            "regprocedure",
            "regtype",
            "regrole",
            "aclitem",
            "pg_node_tree",
        ] {
            assert!(builtin_output(name).is_none(), "{name}");
        }
    }

    #[test]
    fn an_unknown_literal_requires_the_exact_builtin_output_handler() {
        let catalog = |variant: &str| {
            let row = |v: serde_json::Value| v.as_object().unwrap().clone();
            Catalog::new(BTreeMap::from([
                ("pg_namespace".into(),vec![row(json!({"oid":11,"nspname":"pg_catalog"})),row(json!({"oid":12,"nspname":"app"}))]),
                ("pg_language".into(),vec![row(json!({"oid":13,"lanname":if variant=="language" {"sql"} else {"internal"}}))]),
                ("pg_proc".into(),vec![row(json!({"oid":14,"proname":"unknownout","pronamespace":if variant=="function_namespace" {12} else {11},"proargtypes":[705],"prolang":13,"prosrc":if variant=="symbol" {"user_output"} else {"unknownout"}}))]),
                ("pg_type".into(),vec![row(json!({"oid":705,"typname":"unknown","typnamespace":if variant=="type_namespace" {12} else {11},"typtype":"p","typoutput":14,"typelem":0,"typcategory":"X"}))]),
            ])).unwrap_or_else(|e| panic!("{e:?}"))
        };
        assert!(datum(&catalog("builtin"), 705).is_ok());
        for variant in ["language", "symbol", "function_namespace", "type_namespace"] {
            assert!(datum(&catalog(variant), 705).is_err(), "{variant}");
        }
    }
}
