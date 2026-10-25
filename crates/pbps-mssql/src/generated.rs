//! A new table's creation parts on SQL Server (DEC-1503.1, DEC-2374.1).
//!
//! A table's `CREATE` carries its computed columns, defaults and checks
//! inline, and a computed column, default or check that names a declared
//! function fails there (4121, 342; measured on 17.0): the function does not
//! exist yet. So a new table whose generation calls a planned function is
//! split here, and `pbps_diff::order::generated::constraints` orders the parts
//! as it does PostgreSQL's: the bare table, then each late column in declared
//! order, then what names them.
//!
//! The model keeps a table's plain columns before its computed ones (the
//! `CREATE` writes them in that order), so the cut is among the computed
//! columns: those before the first one that calls a planned function stay in
//! the `CREATE`, and every later one is an `AddComputedColumn`, in the
//! declared order its position depends on.

use crate::Mssql;
use pbps_dialect::{Dialect, DialectError};
use pbps_model::{Change, IdsFile, ModuleId, ValueSource};
use std::collections::BTreeSet;

/// Whether `text` calls a declared function: by the two-part name SQL Server
/// requires of a scalar function in an expression (a one-part call is 195,
/// measured), exact by schema and leaf name.
pub(crate) fn calls(dialect: &Mssql, text: &str, function: &ModuleId) -> bool {
    dialect.qualified_calls(text).iter().any(|(schema, name)| {
        schema.eq_ignore_ascii_case(function.schema()) && name.eq_ignore_ascii_case(function.name())
    })
}

/// The new table's parts, in the order its tail is declared; `None` where the
/// `CREATE` already compiles because nothing in it calls a planned function.
pub(crate) fn parts(
    dialect: &Mssql,
    change: &Change,
    ids: &IdsFile,
    functions: &[ModuleId],
) -> Result<Option<Vec<Change>>, DialectError> {
    let Change::CreateTable {
        uid,
        name,
        table,
        beside_default,
        moves_at_most,
    } = change
    else {
        return Ok(None);
    };
    let calling = |text: &str| functions.iter().any(|id| calls(dialect, text, id));
    let cut = table.computed.values().position(|c| calling(&c.expression));
    let default_calls = table
        .columns
        .values()
        .any(|c| matches!(&c.value_source, ValueSource::Default(text) if calling(text)));
    let check_calls = table.checks.values().any(|c| calling(&c.expression));
    if cut.is_none() && !default_calls && !check_calls {
        return Ok(None);
    }
    let cut = cut.unwrap_or(table.computed.len());
    let pending: BTreeSet<String> = table
        .computed
        .keys()
        .skip(cut)
        .map(|c| c.raw().to_lowercase())
        .collect();
    let reads_pending = |text: &str| {
        column_names(dialect, text).is_none_or(|read| !read.is_disjoint(&pending))
            && !pending.is_empty()
    };
    let late_text = |text: &str| calling(text) || reads_pending(text);
    let late_column = |columns: &[pbps_model::ColumnName]| {
        columns
            .iter()
            .any(|c| pending.contains(&c.raw().to_lowercase()))
    };
    let mut bare = (**table).clone();
    bare.computed = table
        .computed
        .iter()
        .take(cut)
        .map(|(column, expression)| (column.clone(), expression.clone()))
        .collect();
    let mut tail = Vec::new();
    for (column, expression) in table.computed.iter().skip(cut) {
        tail.push(Change::AddComputedColumn {
            table: name.clone(),
            name: column.clone(),
            computed: expression.clone(),
        });
    }
    if bare
        .primary_key
        .as_ref()
        .is_some_and(|key| late_column(&key.columns))
    {
        tail.push(Change::SetPrimaryKey {
            table: name.clone(),
            from: None,
            to: bare.primary_key.take(),
            nonclustered: !table.primary_key_is_clustered(),
        });
    }
    for (key, constraint) in std::mem::take(&mut bare.unique) {
        if late_column(&constraint.columns) {
            tail.push(Change::AddUnique {
                table: name.clone(),
                clustered: table.unique_is_clustered(key.raw()),
                name: key,
                constraint,
            });
        } else {
            bare.unique.insert(key, constraint);
        }
    }
    for (key, index) in std::mem::take(&mut bare.indexes) {
        // An expression key or a filter can name any column, and is read as
        // naming every pending one when the dialect cannot tell which.
        let deferred = late_column(&index.include)
            || index.columns.iter().any(|c| {
                c.key
                    .column()
                    .is_some_and(|c| late_column(std::slice::from_ref(c)))
            })
            || (index.holds_expression() && !pending.is_empty())
            || index.filter.as_deref().is_some_and(late_text);
        if deferred {
            tail.push(Change::AddIndex {
                table: name.clone(),
                clustered: table.index_is_clustered(key.raw()),
                name: key,
                index: Box::new(index),
            });
        } else {
            bare.indexes.insert(key, index);
        }
    }
    // The differ already separates foreign keys from every new table; one
    // the table still holds waits as one of its parts.
    for (key, constraint) in std::mem::take(&mut bare.foreign_keys) {
        tail.push(Change::AddForeignKey {
            table: name.clone(),
            name: key,
            constraint: Box::new(constraint),
        });
    }
    for (key, constraint) in std::mem::take(&mut bare.checks) {
        if late_text(&constraint.expression) {
            tail.push(Change::AddCheck {
                table: name.clone(),
                name: key,
                constraint,
            });
        } else {
            bare.checks.insert(key, constraint);
        }
    }
    let column_uid = |column: &pbps_model::ColumnName| {
        ids.column_uid(&name.column(column.clone()))
            .cloned()
            .ok_or_else(|| DialectError::Invalid {
                dialect: "mssql",
                message: format!(
                    "{} has no recorded column identity for its creation parts",
                    name.column(column.clone()).shown()
                ),
            })
    };
    for (column, spec) in &mut bare.columns {
        if let ValueSource::Default(text) = &mut spec.value_source
            && calling(text)
        {
            let to = std::mem::take(text);
            spec.value_source = ValueSource::None;
            tail.push(Change::AlterColumnDefault {
                uid: column_uid(column)?,
                column: name.column(column.clone()),
                from: None,
                to: Some(to),
            });
        }
    }
    let mut parts = vec![Change::CreateTable {
        uid: uid.clone(),
        name: name.clone(),
        table: Box::new(bare),
        beside_default: beside_default.clone(),
        moves_at_most: *moves_at_most,
    }];
    parts.extend(tail);
    Ok(Some(parts))
}

/// The names `text` may read as columns of a table, lowercased, or `None`
/// where it projects every column. Every identifier in the code counts,
/// variables and parameters excepted (`@c` is no column): a name that is no
/// column of the table matches no part, and one that is an alias or another
/// table's column over-holds, which costs a cycle that names its remedy.
pub(crate) fn column_names(dialect: &Mssql, text: &str) -> Option<BTreeSet<String>> {
    let code = without_variables(&dialect.code_only(text));
    if selects_all(&code) {
        return None;
    }
    let mut names = BTreeSet::new();
    let mut chars = code.chars().peekable();
    while let Some(c) = chars.next() {
        let name = match c {
            '[' | '"' => {
                let close = if c == '[' { ']' } else { '"' };
                let mut name = String::new();
                while let Some(n) = chars.next() {
                    if n == close && chars.next_if_eq(&close).is_none() {
                        break;
                    }
                    name.push(n);
                }
                name
            }
            c if c.is_alphanumeric() || matches!(c, '_' | '$') => {
                let mut name = String::from(c);
                while let Some(n) = chars.next_if(|n| is_identifier_part(*n)) {
                    name.push(n);
                }
                name
            }
            _ => continue,
        };
        names.insert(name.to_lowercase());
    }
    Some(names)
}

/// `code` without the names that are never a column: a variable or parameter
/// (`@c`, `@@c`) and a temporary object (`#c`). The dialect's name test reads
/// `c` out of all three (measured), so a function with a parameter `@c` would
/// be taken for a reader of a column `c`. Only a token that starts at a
/// boundary is one: `@` and `#` inside a delimited identifier (`[c#d]`,
/// `"c#d"`) or after an identifier character (`c#d`, a legal column name) are
/// part of the name.
pub(crate) fn without_variables(code: &str) -> String {
    let mut clean = String::with_capacity(code.len());
    let mut chars = code.chars().peekable();
    let mut previous = ' ';
    while let Some(c) = chars.next() {
        match c {
            '[' | '"' => {
                // Kept whole, a doubled closing delimiter being part of the name.
                let close = if c == '[' { ']' } else { '"' };
                clean.push(c);
                while let Some(n) = chars.next() {
                    clean.push(n);
                    if n == close {
                        if chars.next_if_eq(&close).is_none() {
                            break;
                        }
                        clean.push(close);
                    }
                }
                previous = close;
            }
            '@' | '#' if !is_identifier_part(previous) => {
                while chars.next_if(|n| is_identifier_part(*n)).is_some() {}
                clean.push(' ');
                previous = ' ';
            }
            _ => {
                clean.push(c);
                previous = c;
            }
        }
    }
    clean
}

/// A character a regular identifier continues with.
fn is_identifier_part(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$')
}

/// Whether `code`, a definition with strings and comments blanked
/// (`Dialect::code_only`), projects every column: a `*` that ends a select
/// list or stands before a comma (`SELECT *`, `TOP 5 * FROM`, `*, a`), or
/// follows `alias.` or `SELECT`/`DISTINCT`/`ALL`. `COUNT(*)`, a multiplication
/// and `EXISTS (SELECT * ..)` are none of these: the first two have an operand
/// beside the `*`, and the last projects no column list at all.
pub(crate) fn selects_all(code: &str) -> bool {
    // The identifier next to `*`, reading outward from it.
    let word = |chars: &mut dyn Iterator<Item = char>| -> String {
        chars
            .skip_while(|c| c.is_whitespace())
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$'))
            .collect()
    };
    let chars: Vec<char> = code.chars().collect();
    (0..chars.len()).filter(|&at| chars[at] == '*').any(|at| {
        let before = chars[..at].iter().rev().find(|c| !c.is_whitespace());
        let next = chars[at + 1..].iter().find(|c| !c.is_whitespace());
        let after = word(&mut chars[at + 1..].iter().copied());
        let preceding: String = word(&mut chars[..at].iter().rev().copied())
            .chars()
            .rev()
            .collect();
        let select = preceding.eq_ignore_ascii_case("SELECT");
        let exists = select && inside_exists(&chars, at);
        !exists
            && (before == Some(&'.')
                || next == Some(&',')
                || after.eq_ignore_ascii_case("FROM")
                || select
                || ["DISTINCT", "ALL"]
                    .iter()
                    .any(|keyword| preceding.eq_ignore_ascii_case(keyword)))
    })
}

/// Whether the `SELECT` before the `*` at `at` opens an `EXISTS (` subquery.
fn inside_exists(chars: &[char], at: usize) -> bool {
    let skip_space = |mut i: usize| {
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    };
    // Past the `SELECT` word, then the `(` that opens the subquery.
    let i = skip_space(skip_space(at).saturating_sub("SELECT".len()));
    if i == 0 || chars[i - 1] != '(' {
        return false;
    }
    let end = skip_space(i - 1);
    let mut start = end;
    while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
        start -= 1;
    }
    chars[start..end]
        .iter()
        .collect::<String>()
        .eq_ignore_ascii_case("EXISTS")
}
