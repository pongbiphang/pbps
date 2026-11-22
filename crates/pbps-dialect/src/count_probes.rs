//! The count statements of the basic probes, written once (#2988, DEC-2988.2).
//!
//! A `NOT NULL`, a unique key, a check and a foreign key ask the same question
//! of the data on every engine. The statement that asks it differs in spelling
//! and in one rule, whether a unique key takes two NULLs for equal. An engine
//! gives both as one [`CountSql`], and the statements are here.
//!
//! Only the statements. Which rows a statement will meet is each engine's own
//! rule at every step (what a plan adds, retypes, recollates and writes before
//! it), so a relation arrives as text the engine built, and so does every name
//! and literal. Nothing here quotes, so nothing here can quote for the wrong
//! engine.

use std::collections::BTreeSet;

use pbps_model::{ColumnRef, ObjectLocalName};

use crate::Probe;

/// How one engine spells the count statements. No field has a default: an
/// engine that has not answered one has no probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountSql {
    /// The count of a statement's rows, as the one integer a probe answers.
    pub rows: &'static str,
    /// The sum of the per-group counts `c` as that integer, zero when no group
    /// is left.
    pub grouped_rows: &'static str,
    /// A count that is not the probe's answer: a group's rows, or a term the
    /// engine wraps itself.
    pub count: &'static str,
    /// What follows an expression to ask whether it is NULL.
    pub is_null: &'static str,
    /// What the engine's unique key makes of two NULLs.
    pub unique_nulls: UniqueNulls,
}

/// The rule a duplicate count has to share with the key it stands for. Counted
/// under the other rule, it refuses a plan the engine accepts, or passes one
/// the engine refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniqueNulls {
    /// Two NULLs collide, which is also how `GROUP BY` groups them.
    Equal,
    /// A row with a NULL in any column of the key collides with nothing, so it
    /// is left out before the grouping, which would put it with its like.
    Distinct,
}

/// The rows a plan deletes before the statement that is being probed: they
/// will not be there to violate anything.
#[derive(Debug, Clone, Copy)]
pub struct Gone<'a> {
    /// The key column, quoted, as the probed relation spells it.
    pub key: &'a str,
    /// The deleted keys as literals. A set, so the statement is the same
    /// whatever order the plan deletes in.
    pub literals: &'a BTreeSet<String>,
}

/// `r.k0 .. r.kn`: the columns of the rows in [`relation`].
pub fn keys(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("r.k{i}")).collect()
}

/// The rows an engine built for a key, as the relation `r` whose columns are
/// [`keys`].
pub fn relation(rows: &str) -> String {
    format!("(\n{rows}\n) AS r")
}

/// The NULLs a `NOT NULL` would reject.
///
/// `column` names it as the plan does, for the message a human reads. `from` is
/// the relation the statement will meet and `reads` is how a row in it reads in
/// this column: the quoted column name, or an expression where the plan is the
/// one deciding what is there.
pub fn nulls(sql: &CountSql, column: &ColumnRef, from: &str, reads: &str) -> Probe {
    Probe::new(
        format!(
            "existing NULLs in {column}, which NOT NULL would reject",
            column = column.shown()
        ),
        format!(
            "SELECT {} AS n FROM {from} WHERE {reads} {};",
            sql.rows, sql.is_null
        ),
    )
}

/// The rows that would collide under a new unique key.
///
/// `from` is the relation the key's statement will meet and `keys` are its key
/// expressions within it: for a key a plan adds, the rows the engine builds
/// from the changes before it (DECISIONS 175).
pub fn duplicates(sql: &CountSql, from: &str, keys: &[String], description: &str) -> Probe {
    let list = keys.join(", ");
    let present = match sql.unique_nulls {
        UniqueNulls::Equal => String::new(),
        UniqueNulls::Distinct => {
            let present: Vec<String> = keys.iter().map(|k| format!("{k} IS NOT NULL")).collect();
            format!("\n     WHERE {}", present.join(" AND "))
        }
    };
    Probe::new(
        description,
        // The rows, not the groups: "3 duplicate groups" makes an operator do
        // arithmetic before they know how much data is involved.
        format!(
            "SELECT {} AS n FROM (\n    SELECT {count} AS c FROM {from}{present}\n     GROUP BY {list} HAVING {count} > 1) AS dup;",
            sql.grouped_rows,
            count = sql.count,
        ),
    )
}

/// The rows of `from` that a new check would refuse.
///
/// A check rejects a row only when its predicate is FALSE, and UNKNOWN passes.
/// `WHERE NOT (predicate)` behaves exactly so, and the count is what the engine
/// will refuse.
///
/// `predicate` is the declared text as the engine wants it written, and is not
/// rewritten for a rename: rewriting SQL text by substitution is how a tool
/// that promised never to parse SQL starts parsing it badly. Such a probe fails
/// to run and is reported as unchecked, which is honest.
pub fn check_violations(
    sql: &CountSql,
    name: &ObjectLocalName,
    from: &str,
    predicate: &str,
    gone: Option<Gone<'_>>,
) -> Probe {
    let mut text = format!(
        "SELECT {} AS n FROM {from} WHERE NOT ({predicate})",
        sql.rows
    );
    // Counting a deleted row refused a plan that cleans up after itself before
    // it tightens. No deleted key is no clause: `NOT IN ()` is not a statement.
    if let Some(gone) = gone.filter(|gone| !gone.literals.is_empty()) {
        let literals: Vec<&str> = gone.literals.iter().map(String::as_str).collect();
        text.push_str(&format!(
            " AND {} NOT IN ({})",
            gone.key,
            literals.join(", ")
        ));
    }
    text.push(';');
    Probe::new(
        format!(
            "rows that violate the new check {name}",
            name = name.shown()
        ),
        text,
    )
}

/// The rows of `child` with no row in `parent`, as the `FROM` and `WHERE` of a
/// count the engine completes. Both are rows for [`relation`] with `columns`
/// columns, and `join` compares `q.k{i}` with `r.k{i}` under the engine's own
/// collation rule.
///
/// A row with a NULL in any column of the key is exempt (`MATCH SIMPLE`, which
/// is what every engine here implements), so leaving those out is the rule and
/// not leniency.
pub fn orphaned(child: &str, parent: &str, columns: usize, join: &[String]) -> String {
    let present: Vec<String> = keys(columns)
        .iter()
        .map(|key| format!("{key} IS NOT NULL"))
        .collect();
    format!(
        "FROM {}\n WHERE {}\n   AND NOT EXISTS (SELECT 1 FROM (\n{parent}\n) AS q WHERE {})",
        relation(child),
        present.join("\n   AND "),
        join.join(" AND ")
    )
}

#[cfg(test)]
mod tests;
