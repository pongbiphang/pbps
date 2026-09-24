//! Actual engine-resolved bindings. Dependency rows are supplementary:
//! pg_depend intentionally omits pinned builtins, while a tree carries each
//! occurrence, including columns and projection positions.

use super::logical::{self, Catalog};
use super::nodes::{self, Node, ReferenceClass, Value};
use pbps_db::resolver::capture::ObjectIdentity;

// These are private capture inputs, not an ordinary diagnostic or wire type.
#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub(super) struct Binding {
    pub path: Vec<String>,
    pub target: ObjectIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Uncovered {
    Node,
    Reference,
    Context,
    Subobject,
}

impl From<nodes::Uncovered> for Uncovered {
    fn from(_: nodes::Uncovered) -> Self {
        Self::Node
    }
}
impl From<logical::Uncovered> for Uncovered {
    fn from(_: logical::Uncovered) -> Self {
        Self::Reference
    }
}

type Result<T> = std::result::Result<T, Uncovered>;

pub(super) fn extract(
    catalog: &Catalog,
    text: &str,
    major: u32,
    relation: Option<u32>,
) -> Result<Vec<Binding>> {
    let tree = nodes::decode(text, major)?;
    fn has_node(value: &Value) -> bool {
        match value {
            Value::Node(_) => true,
            Value::List(values) => !values.is_empty() && values.iter().all(has_node),
            Value::Null | Value::Atom(_) | Value::Datum => false,
        }
    }
    if !has_node(&tree) {
        return Err(Uncovered::Node);
    }
    let mut bindings = Vec::new();
    for reference in nodes::references(&tree, major)? {
        let class = match reference.class {
            ReferenceClass::Relation => "pg_class",
            ReferenceClass::Routine => "pg_proc",
            ReferenceClass::Type => "pg_type",
            ReferenceClass::Operator => "pg_operator",
            ReferenceClass::Collation => "pg_collation",
            ReferenceClass::Role => "pg_authid",
            ReferenceClass::Constraint => "pg_constraint",
            ReferenceClass::OperatorClass => "pg_opclass",
            ReferenceClass::OperatorFamily => "pg_opfamily",
        };
        bindings.push(Binding {
            path: reference.path,
            target: catalog.object(class, reference.oid)?,
        });
    }
    subobjects(
        catalog,
        &tree,
        relation,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut bindings,
    )?;
    bindings.sort_by(|a, b| (&a.path, &a.target).cmp(&(&b.path, &b.target)));
    bindings.dedup();
    Ok(bindings)
}

struct Frame<'a> {
    query: &'a Node,
    path: Vec<String>,
}

fn subobjects<'a>(
    catalog: &Catalog,
    value: &'a Value,
    relation: Option<u32>,
    frames: &mut Vec<Frame<'a>>,
    path: &mut Vec<String>,
    bindings: &mut Vec<Binding>,
) -> Result<()> {
    match value {
        Value::Node(node) => {
            if node.tag == "QUERY" {
                frames.push(Frame {
                    query: node,
                    path: path.clone(),
                });
            }
            match node.tag.as_str() {
                "VAR" => {
                    let target = variable(catalog, node, relation, frames)?;
                    let mut path = path.clone();
                    path.push("column".into());
                    bindings.push(Binding { path, target });
                }
                "FIELDSTORE" => {
                    for (index, target) in assigned_fields(catalog, node)?.into_iter().enumerate() {
                        let mut path = path.clone();
                        path.extend(["assigned_field".into(), index.to_string()]);
                        bindings.push(Binding { path, target });
                    }
                }
                "FIELDSELECT" => {
                    let argument = node.fields.get("arg").ok_or(Uncovered::Node)?;
                    let type_id = expression_type(argument)?;
                    let type_row = catalog.row("pg_type", type_id)?;
                    let relation = logical::number(type_row, "typrelid")?;
                    if relation == 0 {
                        return Err(Uncovered::Subobject);
                    }
                    let target = catalog.column(relation, signed(node, "fieldnum")?)?;
                    let mut path = path.clone();
                    path.push("field".into());
                    bindings.push(Binding { path, target });
                }
                "TARGETENTRY" => {
                    let relation = node.number("resorigtbl")?;
                    let column = signed(node, "resorigcol")?;
                    if relation != 0 && column != 0 {
                        let mut path = path.clone();
                        path.push("origin_column".into());
                        bindings.push(Binding {
                            path,
                            target: catalog.column(relation, column)?,
                        });
                    }
                }
                _ => {}
            }
            for (field, value) in &node.fields {
                path.push(field.clone());
                subobjects(catalog, value, relation, frames, path, bindings)?;
                path.pop();
            }
            if node.tag == "QUERY" {
                frames.pop();
            }
        }
        Value::List(values) => {
            for (index, value) in values.iter().enumerate() {
                path.push(index.to_string());
                subobjects(catalog, value, relation, frames, path, bindings)?;
                path.pop();
            }
        }
        Value::Null | Value::Atom(_) | Value::Datum => {}
    }
    Ok(())
}

fn variable(
    catalog: &Catalog,
    node: &Node,
    relation: Option<u32>,
    frames: &[Frame<'_>],
) -> Result<ObjectIdentity> {
    let varno = node.number("varno")?;
    let column = signed(node, "varattno")?;
    let level = usize::try_from(node.number("varlevelsup")?).map_err(|_| Uncovered::Context)?;
    let relation = if frames.is_empty() {
        if level != 0 || varno != 1 {
            return Err(Uncovered::Context);
        }
        relation.ok_or(Uncovered::Context)?
    } else {
        let frame = frames
            .len()
            .checked_sub(level + 1)
            .and_then(|i| frames.get(i))
            .ok_or(Uncovered::Context)?;
        let Value::List(rtable) = frame.query.fields.get("rtable").ok_or(Uncovered::Context)?
        else {
            return Err(Uncovered::Context);
        };
        let index = usize::try_from(varno)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .ok_or(Uncovered::Context)?;
        let Some(Value::Node(range)) = rtable.get(index) else {
            return Err(Uncovered::Context);
        };
        if range.tag != "RANGETBLENTRY" {
            return Err(Uncovered::Context);
        }
        let kind = range.number("rtekind")?;
        if kind != 0 {
            // A derived query/function/CTE output has no persistent column
            // OID. Its ordinal is a semantic projection position. Retain the
            // exact local source and output position, while the tree's own
            // underlying references are independently resolved above.
            if !matches!(kind, 1..=6 | 8 | 9) || column < 0 {
                return Err(Uncovered::Subobject);
            }
            let mut name = frame.path.clone();
            name.extend([
                "rtable".into(),
                index.to_string(),
                "output".into(),
                column.to_string(),
            ]);
            let mut signature = vec![catalog.object("pg_type", node.number("vartype")?)?];
            if column == 0 {
                signature.extend(derived_row_fields(range)?);
            }
            return Ok(ObjectIdentity {
                class: "query-output".into(),
                name,
                signature,
            });
        }
        range.number("relid")?
    };
    if column == 0 {
        catalog.object("pg_class", relation).map_err(Into::into)
    } else {
        catalog.column(relation, column).map_err(Into::into)
    }
}

// A whole derived row consumes every output position, including repeated
// labels. Pin that ordered shape alongside its row type. The full-tree walk
// and catalog closure independently retain each expression/type/collation
// binding and the routine or named-composite result descriptor.
fn derived_row_fields(range: &Node) -> Result<Vec<ObjectIdentity>> {
    fn list<'a>(node: &'a Node, field: &str) -> Result<&'a [Value]> {
        match node.fields.get(field) {
            Some(Value::List(values)) => Ok(values),
            Some(Value::Null) => Ok(&[]),
            _ => Err(Uncovered::Subobject),
        }
    }
    fn child<'a>(node: &'a Node, field: &str, tag: &str) -> Result<&'a Node> {
        match node.fields.get(field) {
            Some(Value::Node(child)) if child.tag == tag => Ok(child),
            _ => Err(Uncovered::Subobject),
        }
    }
    fn flag(node: &Node, field: &str) -> Result<bool> {
        match node.fields.get(field) {
            Some(Value::Atom(value)) if value == "true" => Ok(true),
            Some(Value::Atom(value)) if value == "false" => Ok(false),
            _ => Err(Uncovered::Subobject),
        }
    }
    fn typed_columns(node: &Node) -> Result<usize> {
        match list(node, "coltypes")? {
            [] => Ok(0),
            [Value::Atom(marker), columns @ ..] if marker == "o" => Ok(columns.len()),
            _ => Err(Uncovered::Subobject),
        }
    }
    let labels = list(child(range, "eref", "ALIAS")?, "colnames")?;
    let width = match range.number("rtekind")? {
        1 => {
            let mut count = 0;
            for target in list(child(range, "subquery", "QUERY")?, "targetList")? {
                let Value::Node(target) = target else {
                    return Err(Uncovered::Subobject);
                };
                if target.tag != "TARGETENTRY" {
                    return Err(Uncovered::Subobject);
                }
                if !flag(target, "resjunk")? {
                    if !matches!(target.fields.get("expr"), Some(Value::Node(_))) {
                        return Err(Uncovered::Subobject);
                    }
                    count += 1;
                }
            }
            count
        }
        2 => list(range, "joinaliasvars")?.len(),
        3 => {
            let mut count = usize::from(flag(range, "funcordinality")?);
            for function in list(range, "functions")? {
                let Value::Node(function) = function else {
                    return Err(Uncovered::Subobject);
                };
                if function.tag != "RANGETBLFUNCTION" {
                    return Err(Uncovered::Subobject);
                }
                count = count
                    .checked_add(
                        usize::try_from(function.number("funccolcount")?)
                            .map_err(|_| Uncovered::Subobject)?,
                    )
                    .ok_or(Uncovered::Subobject)?;
            }
            count
        }
        4 => typed_columns(child(range, "tablefunc", "TABLEFUNC")?)?,
        5 | 6 => typed_columns(range)?,
        8 => 0,
        9 => list(range, "groupexprs")?.len(),
        _ => return Err(Uncovered::Subobject),
    };
    if labels.len() != width {
        return Err(Uncovered::Subobject);
    }
    labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let Value::Atom(label) = label else {
                return Err(Uncovered::Subobject);
            };
            Ok(ObjectIdentity {
                class: "query-field".into(),
                name: vec![(index + 1).to_string(), label.clone()],
                signature: vec![],
            })
        })
        .collect()
}

// Assignments bind the written composite fields even when the RHS reads no
// field. Physical attribute slots become snapshot-local logical column names.
fn assigned_fields(catalog: &Catalog, node: &Node) -> Result<Vec<ObjectIdentity>> {
    let row = catalog.row("pg_type", node.number("resulttype")?)?;
    let relation = logical::number(row, "typrelid")?;
    let Some(Value::List(fields)) = node.fields.get("fieldnums") else {
        return Err(Uncovered::Subobject);
    };
    let Some((Value::Atom(marker), fields)) = fields.split_first() else {
        return Err(Uncovered::Subobject);
    };
    let Some(Value::List(values)) = node.fields.get("newvals") else {
        return Err(Uncovered::Subobject);
    };
    if relation == 0 || marker != "i" || fields.is_empty() || fields.len() != values.len() {
        return Err(Uncovered::Subobject);
    }
    fields
        .iter()
        .map(|field| {
            let Value::Atom(number) = field else {
                return Err(Uncovered::Subobject);
            };
            let number: i32 = number.parse().map_err(|_| Uncovered::Subobject)?;
            if number <= 0 {
                return Err(Uncovered::Subobject);
            }
            catalog.column(relation, number).map_err(Into::into)
        })
        .collect()
}

fn signed(node: &Node, field: &str) -> Result<i32> {
    let Some(Value::Atom(value)) = node.fields.get(field) else {
        return Err(Uncovered::Node);
    };
    value.parse().map_err(|_| Uncovered::Node)
}

fn expression_type(value: &Value) -> Result<u32> {
    let Value::Node(node) = value else {
        return Err(Uncovered::Subobject);
    };
    let field = match node.tag.as_str() {
        "VAR" => "vartype",
        "CONST" => "consttype",
        "PARAM" => "paramtype",
        "FUNCEXPR" => "funcresulttype",
        "AGGREF" => "aggtype",
        "WINDOWFUNC" => "wintype",
        "OPEXPR" | "DISTINCTEXPR" | "NULLIFEXPR" => "opresulttype",
        "MINMAXEXPR" => "minmaxtype",
        "FIELDSTORE" => "resulttype",
        "JSONCONSTRUCTOREXPR" | "JSONEXPR" => {
            let Some(Value::Node(returning)) = node.fields.get("returning") else {
                return Err(Uncovered::Subobject);
            };
            if returning.tag != "JSONRETURNING" {
                return Err(Uncovered::Subobject);
            }
            return returning.number("typid").map_err(Into::into);
        }
        "SUBLINK" => {
            // A scalar subquery returns its sole non-junk projection, not
            // the input row type or the first type referenced by its tree.
            // EXPR_SUBLINK is 4 in both qualified engine versions.
            if node.number("subLinkType")? != 4 {
                return Err(Uncovered::Subobject);
            }
            let Some(Value::Node(query)) = node.fields.get("subselect") else {
                return Err(Uncovered::Subobject);
            };
            if query.tag != "QUERY" {
                return Err(Uncovered::Subobject);
            }
            let Some(Value::List(targets)) = query.fields.get("targetList") else {
                return Err(Uncovered::Subobject);
            };
            let mut result = None;
            for target in targets {
                let Value::Node(target) = target else {
                    return Err(Uncovered::Subobject);
                };
                if target.tag != "TARGETENTRY" {
                    return Err(Uncovered::Subobject);
                }
                match target.fields.get("resjunk") {
                    Some(Value::Atom(value)) if value == "true" => continue,
                    Some(Value::Atom(value)) if value == "false" => {}
                    _ => return Err(Uncovered::Subobject),
                }
                if result.is_some() {
                    return Err(Uncovered::Subobject);
                }
                result = Some(expression_type(
                    target.fields.get("expr").ok_or(Uncovered::Node)?,
                )?);
            }
            return result.ok_or(Uncovered::Subobject);
        }
        "SUBSCRIPTINGREF" => "refrestype",
        "ROWEXPR" => "row_typeid",
        "FIELDSELECT" | "RELABELTYPE" | "COERCEVIAIO" | "ARRAYCOERCEEXPR" | "COERCETODOMAIN"
        | "CONVERTROWTYPEEXPR" => "resulttype",
        "CASEEXPR" => "casetype",
        "CASETESTEXPR" | "COERCETODOMAINVALUE" => "typeId",
        "COALESCEEXPR" => "coalescetype",
        _ => return Err(Uncovered::Subobject),
    };
    node.number(field).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn find_node<'a>(value: &'a Value, tag: &str) -> Option<&'a Node> {
        match value {
            Value::Node(node) if node.tag == tag => Some(node),
            Value::Node(node) => node.fields.values().find_map(|v| find_node(v, tag)),
            Value::List(values) => values.iter().find_map(|v| find_node(v, tag)),
            Value::Null | Value::Atom(_) | Value::Datum => None,
        }
    }

    #[test]
    fn a_scalar_subquery_needs_exactly_one_readable_output() {
        for (major, fixtures) in [
            (16, include_str!("fixtures/expression-results-16.nodes")),
            (18, include_str!("fixtures/expression-results-18.nodes")),
        ] {
            let tree = nodes::decode(fixtures.lines().last().unwrap(), major)
                .unwrap_or_else(|e| panic!("{e:?}"));
            let original = find_node(&tree, "SUBLINK").unwrap();
            let expected = expression_type(&Value::Node(original.clone())).unwrap();
            assert_ne!(expected, 0);
            for case in [
                "not_scalar",
                "absent",
                "unreadable",
                "empty",
                "multiple",
                "junk_only",
                "unknown_junk",
                "extra_junk",
            ] {
                let mut changed = original.clone();
                if case == "not_scalar" {
                    changed
                        .fields
                        .insert("subLinkType".into(), Value::Atom("0".into()));
                } else {
                    let Some(Value::Node(query)) = changed.fields.get_mut("subselect") else {
                        panic!("query fixture")
                    };
                    let Some(Value::List(targets)) = query.fields.get_mut("targetList") else {
                        panic!("target fixture")
                    };
                    assert_eq!(targets.len(), 1);
                    match case {
                        "absent" => {
                            query.fields.remove("targetList");
                        }
                        "unreadable" => {
                            query
                                .fields
                                .insert("targetList".into(), Value::Atom("unreadable".into()));
                        }
                        "empty" => targets.clear(),
                        "multiple" => targets.push(targets[0].clone()),
                        "junk_only" | "unknown_junk" | "extra_junk" => {
                            let Value::Node(target) = &targets[0] else {
                                panic!("target fixture")
                            };
                            let mut target = target.clone();
                            target.fields.insert(
                                "resjunk".into(),
                                Value::Atom(
                                    if case == "unknown_junk" {
                                        "unreadable"
                                    } else {
                                        "true"
                                    }
                                    .into(),
                                ),
                            );
                            if case == "extra_junk" {
                                targets.insert(0, Value::Node(target));
                            } else {
                                targets[0] = Value::Node(target);
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                let result = expression_type(&Value::Node(changed));
                if case == "extra_junk" {
                    assert_eq!(result, Ok(expected));
                } else {
                    assert!(result.is_err(), "{case}");
                }
            }
        }
    }

    #[test]
    fn whole_row_shape_preserves_order_and_refuses_incomplete_labels() {
        let mut rejected = Vec::new();
        for (major, fixtures) in [
            (16, include_str!("fixtures/query-ranges-16.nodes")),
            (18, include_str!("fixtures/query-ranges-18.nodes")),
        ] {
            let tree = nodes::decode(fixtures.lines().nth(3).unwrap(), major)
                .unwrap_or_else(|e| panic!("{e:?}"));
            let query = find_node(&tree, "QUERY").unwrap();
            let Value::List(ranges) = &query.fields["rtable"] else {
                panic!("range fixture")
            };
            let range = ranges
                .iter()
                .find_map(|v| match v {
                    Value::Node(n) if n.number("rtekind") == Ok(1) => Some(n),
                    Value::Null
                    | Value::Atom(_)
                    | Value::List(_)
                    | Value::Node(_)
                    | Value::Datum => None,
                })
                .unwrap();
            let original = derived_row_fields(range).unwrap();
            assert_eq!(
                original
                    .iter()
                    .map(|f| f.name[1].as_str())
                    .collect::<Vec<_>>(),
                ["id", "label"]
            );
            for case in [
                "missing",
                "unreadable",
                "truncated",
                "extra",
                "empty",
                "bad_label",
                "reordered",
                "renamed",
            ] {
                let mut changed = range.clone();
                let Some(Value::Node(alias)) = changed.fields.get_mut("eref") else {
                    panic!("alias fixture")
                };
                let Some(Value::List(labels)) = alias.fields.get_mut("colnames") else {
                    panic!("label fixture")
                };
                match case {
                    "missing" => {
                        alias.fields.remove("colnames");
                    }
                    "unreadable" => {
                        alias
                            .fields
                            .insert("colnames".into(), Value::Atom("unreadable".into()));
                    }
                    "truncated" => {
                        labels.pop();
                    }
                    "extra" => labels.push(Value::Atom("extra".into())),
                    "empty" => {
                        alias.fields.insert("colnames".into(), Value::Null);
                    }
                    "bad_label" => labels[0] = Value::Null,
                    "reordered" => labels.swap(0, 1),
                    "renamed" => labels[0] = Value::Atom("renamed".into()),
                    _ => unreachable!(),
                }
                let result = derived_row_fields(&changed);
                if matches!(case, "reordered" | "renamed") {
                    assert_ne!(result.unwrap(), original);
                } else {
                    rejected.push((major, case, result.is_err()));
                }
            }
        }
        assert!(
            rejected.iter().all(|(_, _, refused)| *refused),
            "incomplete shapes: {rejected:?}"
        );
    }

    #[test]
    fn a_column_reference_needs_its_actual_relation_and_live_name() {
        let row = |v: serde_json::Value| v.as_object().unwrap().clone();
        let catalog = Catalog::new(BTreeMap::from([
            (
                "pg_namespace".into(),
                vec![row(json!({"oid":1,"nspname":"app"}))],
            ),
            (
                "pg_class".into(),
                vec![row(json!({"oid":2,"relname":"t","relnamespace":1}))],
            ),
            (
                "pg_attribute".into(),
                vec![row(
                    json!({"attrelid":2,"attnum":4,"attname":"actual","attisdropped":false}),
                )],
            ),
        ]))
        .unwrap_or_else(|e| panic!("{e:?}"));
        let node = Node {
            tag: "VAR".into(),
            fields: BTreeMap::from([
                ("varno".into(), Value::Atom("1".into())),
                ("varattno".into(), Value::Atom("4".into())),
                ("varlevelsup".into(), Value::Atom("0".into())),
            ]),
        };
        assert_eq!(
            variable(&catalog, &node, Some(2), &[]).unwrap().name,
            ["actual"]
        );
        assert_eq!(
            variable(&catalog, &node, None, &[]),
            Err(Uncovered::Context)
        );
        assert_eq!(
            variable(&catalog, &node, Some(99), &[]),
            Err(Uncovered::Reference)
        );
    }
    #[test]
    fn written_composite_fields_need_live_slots_and_matching_values() {
        for (major, fixture) in [
            (16, include_str!("fixtures/stored-surfaces-16.nodes")),
            (18, include_str!("fixtures/stored-surfaces-18.nodes")),
        ] {
            let tree = nodes::decode(fixture.lines().next().unwrap(), major).unwrap();
            let original = find_node(&tree, "FIELDSTORE").unwrap();
            let oid = original.number("resulttype").unwrap();
            let row = |v: serde_json::Value| v.as_object().unwrap().clone();
            let catalog=Catalog::new(BTreeMap::from([
                ("pg_namespace".into(),vec![row(json!({"oid":1,"nspname":"app"}))]),
                ("pg_type".into(),vec![row(json!({"oid":oid,"typrelid":2}))]),
                ("pg_class".into(),vec![row(json!({"oid":2,"relname":"pair","relnamespace":1}))]),
                ("pg_attribute".into(),vec![row(json!({"attrelid":2,"attnum":1,"attname":"id","attisdropped":false})),row(json!({"attrelid":2,"attnum":2,"attname":"label","attisdropped":false})),row(json!({"attrelid":2,"attnum":3,"attname":"dropped","attisdropped":true}))]),
            ])).unwrap();
            assert_eq!(expression_type(&Value::Node(original.clone())), Ok(oid));
            assert!(!assigned_fields(&catalog, original).unwrap().is_empty());
            for case in [
                "absent",
                "null",
                "bad_marker",
                "empty",
                "zero",
                "negative",
                "dropped",
                "unreadable",
                "absent_value",
                "extra_value",
            ] {
                let mut node = original.clone();
                match case {
                    "absent" => {
                        node.fields.remove("fieldnums");
                    }
                    "null" => {
                        node.fields.insert("fieldnums".into(), Value::Null);
                    }
                    "absent_value" | "extra_value" => {
                        let Some(Value::List(values)) = node.fields.get_mut("newvals") else {
                            panic!()
                        };
                        if case == "absent_value" {
                            values.pop();
                        } else {
                            values.push(Value::Null);
                        }
                    }
                    _ => {
                        let Some(Value::List(fields)) = node.fields.get_mut("fieldnums") else {
                            panic!()
                        };
                        match case {
                            "bad_marker" => fields[0] = Value::Atom("o".into()),
                            "empty" => fields.truncate(1),
                            "zero" => fields[1] = Value::Atom("0".into()),
                            "negative" => fields[1] = Value::Atom("-1".into()),
                            "dropped" => fields[1] = Value::Atom("3".into()),
                            "unreadable" => fields[1] = Value::Null,
                            _ => unreachable!(),
                        }
                    }
                }
                assert!(assigned_fields(&catalog, &node).is_err(), "{major}/{case}");
            }
        }
    }

    #[test]
    fn json_result_types_require_their_returning_descriptor() {
        for tag in ["JSONCONSTRUCTOREXPR", "JSONEXPR"] {
            let returning = Node {
                tag: "JSONRETURNING".into(),
                fields: BTreeMap::from([("typid".into(), Value::Atom("23".into()))]),
            };
            let original = Node {
                tag: tag.into(),
                fields: BTreeMap::from([("returning".into(), Value::Node(returning.clone()))]),
            };
            assert_eq!(expression_type(&Value::Node(original.clone())), Ok(23));
            for case in ["missing", "unreadable", "wrong_tag", "missing_type"] {
                let mut node = original.clone();
                let mut child = returning.clone();
                match case {
                    "missing" => {
                        node.fields.remove("returning");
                    }
                    "unreadable" => {
                        node.fields.insert("returning".into(), Value::Null);
                    }
                    "wrong_tag" => {
                        child.tag = "PARAM".into();
                        node.fields.insert("returning".into(), Value::Node(child));
                    }
                    "missing_type" => {
                        child.fields.remove("typid");
                        node.fields.insert("returning".into(), Value::Node(child));
                    }
                    _ => unreachable!(),
                }
                assert!(expression_type(&Value::Node(node)).is_err(), "{tag}/{case}");
            }
        }
    }
}
