//! The shape of a stored view rule (`pg_rewrite.ev_action`, #2563).
//!
//! A table's expressions are bound as a temporary view over the real table, one
//! target entry each, so the engine resolves every spelling of the table in them
//! (`t`, `t.*`, `t.col`, `(t).col`, `app.t.col`) as it does on the table itself
//! (DEC-2563.8). The view's rule is one `Query` in the node format of `conbin`,
//! and each target entry holds the expression, which is the node `conbin` or
//! `polqual` would hold.
//!
//! What the input-path proof can say about it is only as wide as what the tree
//! can name. An expression names its calls. A second relation, a subquery, a CTE
//! or a clause does not: they bring row security, rules and view bodies that run
//! code no expression node mentions. So the rule is admitted as the one plain
//! relation the table is, with nothing but target entries over it, and every
//! other part of the `Query` must be empty ("empty unless listed", as for a
//! function body, so a field a later release adds can only refuse).

use super::nodes::{Node, Value};

/// The fields of the `Query` that may hold something. The range table and its
/// permission list are checked separately; everything else must be empty.
const FREE: &[&str] = &[
    "commandType",
    "querySource",
    "canSetTag",
    "isReturn",
    "rtable",
    "rteperminfos",
    "jointree",
    "targetList",
    "stmt_location",
    "stmt_len",
];

/// The fields of the one range table entry that may hold something. Its kind
/// must be a plain relation, and it may carry no row-security quals.
const FREE_ENTRY: &[&str] = &[
    "alias",
    "eref",
    "rtekind",
    "relid",
    "inh",
    "relkind",
    "rellockmode",
    "perminfoindex",
    "inFromCl",
];

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::List(values) => values.is_empty(),
        Value::Atom(atom) => atom == "false" || atom == "0",
        Value::Node(_) | Value::Datum | Value::DatumBytes { .. } => false,
    }
}

fn node<'a>(value: &'a Value, tag: &str, what: &str) -> Result<&'a Node, String> {
    match value {
        Value::Node(node) if node.tag == tag => Ok(node),
        Value::Node(_)
        | Value::Null
        | Value::Atom(_)
        | Value::List(_)
        | Value::Datum
        | Value::DatumBytes { .. } => Err(format!("the view's {what} is not a {tag}")),
    }
}

fn one<'a>(value: Option<&'a Value>, what: &str) -> Result<&'a Value, String> {
    match value {
        Some(Value::List(items)) => match items.as_slice() {
            [only] => Ok(only),
            _ => Err(format!("the view has more or less than one {what}")),
        },
        _ => Err(format!("the view has no readable {what}")),
    }
}

/// The expression of every target entry of a view rule that is one plain
/// `SELECT` of expressions over one relation, or why it is not.
pub(super) fn expressions(tree: &Value) -> Result<Vec<&Value>, String> {
    let query = node(one(Some(tree), "rule action")?, "QUERY", "rule action")?;
    if query.fields.get("commandType") != Some(&Value::Atom("1".to_owned())) {
        return Err("the view is not a SELECT".to_owned());
    }
    for (field, value) in &query.fields {
        if !FREE.contains(&field.as_str()) && !is_empty(value) {
            return Err(format!(
                "the view is more than a list of plain expressions over one relation \
                 (`{field}` is set: it uses a CTE, a subquery, an aggregate, a clause or a \
                 set operation)"
            ));
        }
    }
    let entry = node(
        one(query.fields.get("rtable"), "range table entry")?,
        "RANGETBLENTRY",
        "range table entry",
    )?;
    if entry.fields.get("rtekind") != Some(&Value::Atom("0".to_owned())) {
        return Err("the view reads something other than a relation".to_owned());
    }
    for (field, value) in &entry.fields {
        if !FREE_ENTRY.contains(&field.as_str()) && !is_empty(value) {
            return Err(format!(
                "the view's relation is more than a plain table (`{field}` is set)"
            ));
        }
    }
    let Some(Value::Node(from)) = query.fields.get("jointree") else {
        return Err("the view has no readable FROM clause".to_owned());
    };
    let reference = node(
        one(from.fields.get("fromlist"), "FROM item")?,
        "RANGETBLREF",
        "FROM item",
    )?;
    if from.tag != "FROMEXPR"
        || reference.fields.get("rtindex") != Some(&Value::Atom("1".to_owned()))
        || !from.fields.get("quals").is_some_and(is_empty)
    {
        return Err("the view has a WHERE clause or a FROM item beyond its table".to_owned());
    }
    let Some(Value::List(entries)) = query.fields.get("targetList") else {
        return Err("the view has no readable target list".to_owned());
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = node(entry, "TARGETENTRY", "target entry")?;
        if !entry.fields.get("resjunk").is_some_and(is_empty) {
            return Err("the view has a target entry it did not select".to_owned());
        }
        out.push(
            entry
                .fields
                .get("expr")
                .ok_or_else(|| "a target entry of the view has no expression".to_owned())?,
        );
    }
    Ok(out)
}
