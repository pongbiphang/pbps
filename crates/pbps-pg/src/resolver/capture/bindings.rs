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
            if !matches!(kind, 1..=6 | 8 | 9) || column <= 0 {
                return Err(Uncovered::Subobject);
            }
            let mut name = frame.path.clone();
            name.extend([
                "rtable".into(),
                index.to_string(),
                "output".into(),
                column.to_string(),
            ]);
            return Ok(ObjectIdentity {
                class: "query-output".into(),
                name,
                signature: vec![catalog.object("pg_type", node.number("vartype")?)?],
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
}
