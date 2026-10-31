//! The shape of a stored function body (`pg_proc.prosqlbody`, #2563).
//!
//! A `RETURN expr` function stores one `Query`, and a `BEGIN ATOMIC` function a
//! list holding one list of them (an empty body is that inner list, printed
//! `<>`). Every `Query` is analysed at `CREATE FUNCTION`, so each call it makes
//! is already bound by OID, as in `conbin`.
//!
//! What the input-path proof can say about such a tree is only as wide as what
//! the tree can name. An expression names its calls, and the proof reads them.
//! A relation does not: a table, view or CTE brings row security, rules and
//! view bodies that run code no node in the `Query` mentions. So a body is
//! admitted as a list of plain `SELECT` target lists, and every other part of a
//! `Query` must be empty. The rule is "empty unless listed", so a field a
//! future release adds can only refuse, never slip through.

use super::nodes::{Node, Value};

/// The fields of a `Query` that may hold something. Everything else (the
/// range table, CTEs, clauses, the flags that announce them) must be empty.
const FREE: &[&str] = &[
    "commandType",
    "querySource",
    "canSetTag",
    "isReturn",
    "jointree",
    "targetList",
    "stmt_location",
    "stmt_len",
];

/// The statements of a body, or why it is not the stored form of one.
fn statements(tree: &Value) -> Result<Vec<&Node>, String> {
    fn query(value: &Value) -> Result<&Node, String> {
        match value {
            Value::Node(node) if node.tag == "QUERY" => Ok(node),
            Value::Node(_)
            | Value::Null
            | Value::Atom(_)
            | Value::List(_)
            | Value::Datum
            | Value::DatumBytes { .. } => {
                Err("a statement of the body is not a parsed query".to_owned())
            }
        }
    }
    match tree {
        Value::Node(_) => Ok(vec![query(tree)?]),
        Value::List(outer) => match outer.as_slice() {
            [Value::Null] => Ok(Vec::new()),
            [Value::List(inner)] => inner.iter().map(query).collect(),
            _ => Err("the body is not one list of statements".to_owned()),
        },
        Value::Null | Value::Atom(_) | Value::Datum | Value::DatumBytes { .. } => {
            Err("the body is not one list of statements".to_owned())
        }
    }
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::List(values) => values.is_empty(),
        Value::Atom(atom) => atom == "false" || atom == "0",
        Value::Node(_) | Value::Datum | Value::DatumBytes { .. } => false,
    }
}

/// Whether the body is a list of plain `SELECT`s over no relation.
pub(super) fn check(tree: &Value) -> Result<(), String> {
    for query in statements(tree)? {
        if query.fields.get("commandType") != Some(&Value::Atom("1".to_owned())) {
            return Err("a statement of the body is not a SELECT".to_owned());
        }
        for (field, value) in &query.fields {
            if !FREE.contains(&field.as_str()) && !is_empty(value) {
                return Err(format!(
                    "a statement of the body is more than a list of plain expressions \
                     (`{field}` is set: it reads a relation or uses FROM, WITH, a subquery, \
                     an aggregate, a clause or a set operation)"
                ));
            }
        }
        match query.fields.get("jointree") {
            Some(Value::Node(from))
                if from.tag == "FROMEXPR"
                    && from.fields.get("fromlist").is_some_and(is_empty)
                    && from.fields.get("quals").is_some_and(is_empty) => {}
            _ => return Err("a statement of the body has a FROM or WHERE clause".to_owned()),
        }
        match query.fields.get("targetList") {
            Some(Value::List(entries))
                if entries
                    .iter()
                    .all(|e| matches!(e, Value::Node(n) if n.tag == "TARGETENTRY")) => {}
            _ => return Err("a statement of the body has no readable target list".to_owned()),
        }
    }
    Ok(())
}

/// Every expression the body evaluates: the expression of each target entry of
/// each statement. [`check`] has already shown there is nothing else.
pub(super) fn expressions(tree: &Value) -> Result<Vec<&Value>, String> {
    let mut out = Vec::new();
    for query in statements(tree)? {
        let Some(Value::List(entries)) = query.fields.get("targetList") else {
            return Err("a statement of the body has no readable target list".to_owned());
        };
        for entry in entries {
            let Value::Node(entry) = entry else {
                return Err("a target entry of the body is not a node".to_owned());
            };
            out.push(
                entry
                    .fields
                    .get("expr")
                    .ok_or_else(|| "a target entry of the body has no expression".to_owned())?,
            );
        }
    }
    Ok(out)
}
