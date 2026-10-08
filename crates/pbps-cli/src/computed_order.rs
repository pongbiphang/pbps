//! A connected SQL Server plan's computed columns, ordered and refused by the
//! catalog's own expression edges (#1431, DEC-1431.1).
//!
//! The differ can only read a computed column's expression as text, and a
//! text scan matches by spelling: it took `dbo.f` for `x.f`, missed `[cafe]`
//! binding `café`, and read `schemabinding` as a clause wherever it appeared
//! (#1423 review, rounds 5 to 14). An offline plan is never applied (SPEC
//! §7.3), so only a connected plan's order has to be right, and a connected
//! plan can ask the engine: `sys.sql_expression_dependencies` names, by
//! object id, every column a computed column reads, every function it calls,
//! and everything a schema-bound module is bound to.
//!
//! Three things follow from those edges, here and nowhere else:
//!
//! - **Release order.** A function a computed column calls cannot be dropped
//!   while the column stands (3729), and module drops are class 0, ahead of
//!   the column's. So a function's drop moves to right after the last change
//!   that removes a computed column calling it, the column's own drop or its
//!   table's. What the function is schema-bound to, among the plan's own
//!   drops, moves after it: another function, a table (#1432).
//! - **Refusals.** A standing computed column blocks a rename, drop, retype
//!   or nullability change of a column it reads (15336, 4922), and an alter
//!   or drop of a function it calls (3729). A schema-bound module over a
//!   computed column blocks the column's drop (4922). Each is refused by
//!   name. A column the plan drops and adds again is not standing: the edge
//!   is its old expression's, which is gone before the function changes, and
//!   what its new expression calls is the differ's offline screen to judge,
//!   as the catalog has no edge for text not yet stored.
//! - Nothing is rebuilt: a module over a computed column the plan changes is
//!   refused, not dropped and recreated around it. That is follow-up scope.
//!
//! The differ keeps its over-approximating refusals as an offline screen,
//! where a false yes costs a second plan; the moves are only here. A
//! connected plan leaves the computed columns that stand throughout to these
//! edges alone (`pbps_diff::Screen::Catalog`, DEC-1460.1): the screen folds
//! case where the database may not. A computed column the plan adds, new or
//! again, has no edge for its new text, and the screen still judges it; the
//! two-part names it spells are also compared with the functions the plan
//! changes under the collation (`refuse_added_calls`, DEC-1459.1).

use std::collections::BTreeSet;

use pbps_model::{Change, ChangeSet, PlannedChange, TableName};
use pbps_mssql::catalog::ExpressionEdge;

/// Which spellings name one object, as the database's catalog collation
/// says: a fold in Rust would join `dbo.f` and `dbo.F` in a case-sensitive
/// database, and part `café` from `cafe` in an accent-insensitive one (#1455
/// review). Asked of the engine once, over every name the edges and the plan
/// hold.
#[derive(Debug, Default)]
pub(crate) struct Alike(BTreeSet<(String, String)>);

impl Alike {
    /// From the pairs the engine reads as one name.
    pub(crate) fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self(pairs.into_iter().collect())
    }

    fn same(&self, a: &str, b: &str) -> bool {
        a == b
            || self.0.contains(&(a.to_owned(), b.to_owned()))
            || self.0.contains(&(b.to_owned(), a.to_owned()))
    }

    fn object(&self, a: &TableName, b: &TableName) -> bool {
        self.same(&a.schema, &b.schema) && self.same(&a.name, &b.name)
    }
}

/// Every name an edge or the plan's changes spell, for [`Alike`].
// The complement names nothing an expression edge can: no computed column,
// no column one reads, its table, or a module.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn spellings(cs: &ChangeSet, edges: &[ExpressionEdge]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let object = |t: &TableName, out: &mut BTreeSet<String>| {
        out.insert(t.schema.clone());
        out.insert(t.name.clone());
    };
    for e in edges {
        object(&e.from, &mut out);
        object(&e.to, &mut out);
        out.extend(e.from_column.iter().chain(&e.to_column).cloned());
    }
    for p in &cs.changes {
        match &p.change {
            Change::RenameTable { from, to, .. } => {
                object(from, &mut out);
                object(to, &mut out);
            }
            Change::RenameColumn {
                table, from, to, ..
            } => {
                object(table, &mut out);
                out.insert(from.clone());
                out.insert(to.clone());
            }
            Change::DropColumn { column, .. }
            | Change::AlterColumnType { column, .. }
            | Change::AlterColumnNullability { column, .. } => {
                object(&column.table, &mut out);
                out.insert(column.name.clone());
            }
            Change::AddComputedColumn { table, name, .. }
            | Change::DropComputedColumn { table, name, .. } => {
                object(table, &mut out);
                out.insert(name.clone());
            }
            Change::DropTable { name, .. } => object(name, &mut out),
            Change::AlterModule { id, .. } | Change::DropModule { id, .. } => {
                object(&id.object_name(), &mut out);
            }
            _ => {}
        }
    }
    out
}

/// The plan's names, back to the catalog's: the edges were stored under the
/// names the database has before this plan runs.
struct CatalogNames {
    tables: Vec<(TableName, TableName)>,
    columns: Vec<(TableName, String, String)>,
}

impl CatalogNames {
    fn of(cs: &ChangeSet) -> Self {
        let mut tables = Vec::new();
        let mut columns = Vec::new();
        for p in &cs.changes {
            if let Change::RenameTable { from, to, .. } = &p.change {
                tables.push((to.clone(), from.clone()));
            }
            if let Change::RenameColumn {
                table, from, to, ..
            } = &p.change
            {
                columns.push((table.clone(), to.clone(), from.clone()));
            }
        }
        Self { tables, columns }
    }

    fn table(&self, planned: &TableName) -> TableName {
        self.tables
            .iter()
            .find(|(to, _)| to == planned)
            .map_or_else(|| planned.clone(), |(_, from)| from.clone())
    }

    /// A column under the name the plan gives it, back to the catalog's.
    fn column(&self, table: &TableName, planned: &str) -> String {
        self.columns
            .iter()
            .find(|(t, to, _)| t == table && to == planned)
            .map_or_else(|| planned.to_owned(), |(_, _, from)| from.clone())
    }
}

/// The input a change renames, drops, retypes or tightens or relaxes, by its
/// catalog table and column, and what the change does to it.
fn input_change(
    change: &Change,
    names: &CatalogNames,
) -> Option<(TableName, String, &'static str)> {
    if let Change::RenameColumn { table, from, .. } = change {
        return Some((names.table(table), from.clone(), "renames"));
    }
    // A drop names the column as the catalog does (DEC-1316.1).
    if let Change::DropColumn { column, .. } = change {
        return Some((names.table(&column.table), column.name.clone(), "drops"));
    }
    if let Change::AlterColumnType { column, .. } = change {
        let table = names.table(&column.table);
        let name = names.column(&column.table, &column.name);
        return Some((table, name, "retypes or recollates"));
    }
    if let Change::AlterColumnNullability { column, .. } = change {
        let table = names.table(&column.table);
        let name = names.column(&column.table, &column.name);
        return Some((table, name, "changes the nullability of"));
    }
    None
}

/// Orders the plan's function drops after the computed columns that call
/// them, with what each is schema-bound to after it, and refuses what the
/// edges say the engine will not do (DEC-1431.1). Returns how many changes
/// moved.
pub(crate) fn order_by_edges(
    cs: &mut ChangeSet,
    edges: &[ExpressionEdge],
    alike: &Alike,
) -> Result<usize, String> {
    let names = CatalogNames::of(cs);
    // A computed column by its catalog table and name.
    let mut dropped: Vec<(TableName, String)> = Vec::new();
    let mut added: Vec<(TableName, String)> = Vec::new();
    let mut tables_dropped: Vec<TableName> = Vec::new();
    let mut modules_dropped: Vec<TableName> = Vec::new();
    for p in &cs.changes {
        if let Change::DropComputedColumn { table, name, .. } = &p.change {
            dropped.push((names.table(table), name.clone()));
        }
        if let Change::AddComputedColumn { table, name, .. } = &p.change {
            added.push((names.table(table), name.clone()));
        }
        if let Change::DropTable { name, .. } = &p.change {
            tables_dropped.push(name.clone());
        }
        if let Change::DropModule { id, .. } = &p.change {
            modules_dropped.push(id.object_name());
        }
    }
    let is = |list: &[(TableName, String)], table: &TableName, column: &str| {
        list.iter()
            .any(|(t, c)| alike.object(t, table) && alike.same(c, column))
    };
    let table_gone = |table: &TableName| tables_dropped.iter().any(|t| alike.object(t, table));
    // There before the plan and there throughout.
    let standing = |table: &TableName, column: &str| {
        !table_gone(table) && !is(&dropped, table, column) && !is(&added, table, column)
    };
    let computed_edges = || edges.iter().filter(|e| e.from_column.is_some());

    let mut refused = Vec::new();
    for p in &cs.changes {
        if let Some((table, column, what)) = input_change(&p.change, &names) {
            for e in computed_edges() {
                let computed = e.from_column.as_deref().unwrap_or_default();
                if alike.object(&e.from, &table)
                    && alike.object(&e.to, &table)
                    && e.to_column
                        .as_deref()
                        .is_some_and(|c| alike.same(c, &column))
                    && standing(&table, computed)
                {
                    refused.push(format!(
                        "computed column {}.{computed} reads `{column}`, which this plan {what}, \
                         and SQL Server refuses that while the computed column stands. Drop the \
                         computed column, or change its expression, in a plan of its own first, \
                         then this one.",
                        e.from
                    ));
                }
            }
        }
        let module = if let Change::AlterModule { id, .. } = &p.change {
            Some((id.object_name(), "alters"))
        } else if let Change::DropModule { id, .. } = &p.change {
            Some((id.object_name(), "drops"))
        } else {
            None
        };
        if let Some((function, what)) = module {
            for e in computed_edges() {
                let computed = e.from_column.as_deref().unwrap_or_default();
                // Dropped, for good or to add again, the column is out of the
                // way first: the function's drop is moved after it below, and
                // its alter runs in class 14, after the drop's class 2.
                if alike.object(&e.to, &function) && standing(&e.from, computed) {
                    refused.push(format!(
                        "computed column {}.{computed} calls `{function}`, which this plan {what}, \
                         and SQL Server refuses that while the column calls it. Apply the \
                         function change and the computed column change in separate plans.",
                        e.from
                    ));
                }
            }
        }
        if let Change::DropComputedColumn { table, name, .. } = &p.change {
            let table = names.table(table);
            for e in edges {
                if e.from_column.is_none()
                    && e.from_schema_bound
                    && alike.object(&e.to, &table)
                    && e.to_column.as_deref().is_some_and(|c| alike.same(c, name))
                    && !modules_dropped.iter().any(|m| alike.object(m, &e.from))
                {
                    refused.push(format!(
                        "computed column {table}.{name} is dropped by this plan, and the \
                         schema-bound {} reads it. Drop {}, or recreate it without \
                         SCHEMABINDING, in a plan of its own first, then this one.",
                        e.from, e.from
                    ));
                }
            }
        }
    }
    if !refused.is_empty() {
        refused.sort();
        refused.dedup();
        return Err(refused.join("\n"));
    }
    release(cs, edges, &names, alike)
}

/// A function this plan creates, alters or drops, by its catalog name, with
/// the word for what the plan does to it.
fn functions_changed(cs: &ChangeSet) -> Vec<(TableName, &'static str)> {
    let function = pbps_model::ModuleKind::Function;
    let mut out = Vec::new();
    for p in &cs.changes {
        if let Change::CreateModule { id, module } = &p.change
            && module.kind == function
        {
            out.push((id.object_name(), "creates"));
        }
        if let Change::AlterModule { id, module } = &p.change
            && module.kind == function
        {
            out.push((id.object_name(), "alters"));
        }
        if let Change::DropModule { id, kind } = &p.change
            && *kind == function
        {
            out.push((id.object_name(), "drops"));
        }
    }
    out
}

/// A computed column this plan adds, new or again, and the two-part names
/// its expression calls.
struct AddedCall {
    table: TableName,
    column: String,
    names: Vec<(String, String)>,
    /// The table's columns a call can be a method of, when the computed
    /// column is added: the declared ones, since a column the plan drops is
    /// gone first and one it adds comes first, of a type with methods.
    receivers: Vec<String>,
}

/// The types SQL Server gives methods, which a call `column.method()` binds
/// to (#1677). The type catalogue is closed, with no user-defined CLR type
/// (`pbps_mssql::types`), so these are all of them; an `int` column `x` has
/// no `x.f()`, and a call of that shape is a function's.
const METHOD_TYPES: [&str; 4] = ["geography", "geometry", "hierarchyid", "xml"];

/// `table`'s columns of a type with methods.
fn method_receivers(table: &pbps_model::Table) -> Vec<String> {
    table
        .columns
        .iter()
        .filter(|(_, c)| METHOD_TYPES.contains(&c.ty.base.as_str()))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Each computed column this plan adds, new or again, with its names.
fn added_calls(
    cs: &ChangeSet,
    declared: &pbps_model::Schema,
    dialect: &dyn pbps_dialect::Dialect,
) -> Vec<AddedCall> {
    let receivers_of = |table: &TableName| {
        declared
            .tables
            .get(table)
            .map(method_receivers)
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for p in &cs.changes {
        if let Change::AddComputedColumn {
            table,
            name,
            computed,
        } = &p.change
        {
            out.push(AddedCall {
                table: table.clone(),
                column: name.clone(),
                names: dialect.qualified_calls(&computed.expression),
                receivers: receivers_of(table),
            });
        }
        if let Change::CreateTable { name, table, .. } = &p.change {
            for (column, computed) in &table.computed {
                out.push(AddedCall {
                    table: name.clone(),
                    column: column.clone(),
                    names: dialect.qualified_calls(&computed.expression),
                    receivers: method_receivers(table),
                });
            }
        }
    }
    out
}

/// Every spelling [`refuse_added_calls`] compares, for the engine to say
/// which name one object under its collation. Empty where there is nothing
/// to compare: no function changed, or no added computed column names one.
pub(crate) fn added_call_spellings(
    cs: &ChangeSet,
    declared: &pbps_model::Schema,
    dialect: &dyn pbps_dialect::Dialect,
) -> BTreeSet<String> {
    let functions = functions_changed(cs);
    let calls = added_calls(cs, declared, dialect);
    if functions.is_empty() || calls.iter().all(|c| c.names.is_empty()) {
        return BTreeSet::new();
    }
    let mut out = BTreeSet::new();
    for (f, _) in functions {
        out.insert(f.schema);
        out.insert(f.name);
    }
    for call in calls {
        if call.names.is_empty() {
            continue;
        }
        for (schema, name) in call.names {
            out.insert(schema);
            out.insert(name);
        }
        out.extend(call.receivers);
    }
    out
}

/// Refuses a computed column this plan adds, new or again, whose expression
/// calls, under the database's collation, a function the plan creates,
/// alters or drops (#1459). The catalog has no edge for text it has not yet
/// stored, so the differ's screen is the only other judge, and it folds by
/// text: under an accent-insensitive collation `[dbo].[cafe]` calls
/// `dbo.café`, which the screen does not see. The column is added before the
/// function changes (class 9 before 14), and SQL Server then refuses the
/// alter (3729) or the add, inside the apply.
///
/// One exception, measured on 17.0 under `CI_AI` (#1668 review): a call
/// whose first part is a column of the table, of a type with methods
/// (#1677), may be the column's method, `geo.STAsText()`. A function the plan drops is gone before the add, which
/// then binds the method, so that drop refuses nothing. While a function of
/// that name exists the engine rejects the call as ambiguous (327), and one
/// created after the add leaves the table's computed columns unloadable
/// (474), so an alter or a create still refuses.
pub(crate) fn refuse_added_calls(
    cs: &ChangeSet,
    declared: &pbps_model::Schema,
    dialect: &dyn pbps_dialect::Dialect,
    alike: &Alike,
) -> Result<(), String> {
    let functions = functions_changed(cs);
    let mut refused = Vec::new();
    for AddedCall {
        table,
        column,
        names,
        receivers,
    } in added_calls(cs, declared, dialect)
    {
        for (schema, name) in &names {
            let a_method = receivers.iter().any(|c| alike.same(schema, c));
            for (function, what) in &functions {
                if *what == "drops" && a_method {
                    continue;
                }
                if alike.same(schema, &function.schema) && alike.same(name, &function.name) {
                    refused.push(format!(
                        "computed column {table}.{column} calls `{function}` as `{schema}.{name}` \
                         under the database's collation, and this plan {what} it. Apply the \
                         function change and the computed column change in separate plans."
                    ));
                }
            }
        }
    }
    if refused.is_empty() {
        return Ok(());
    }
    refused.sort();
    refused.dedup();
    Err(refused.join("\n"))
}

/// What a change drops, by its catalog name: a module or a table.
fn dropped_object(change: &Change) -> Option<TableName> {
    if let Change::DropModule { id, .. } = change {
        Some(id.object_name())
    } else if let Change::DropTable { name, .. } = change {
        Some(name.clone())
    } else {
        None
    }
}

/// The positions the drop at `at` waits for, by the edges: for a function,
/// each removal of a computed column that calls it; for any module or table,
/// each drop of something schema-bound to it, which reaches it through its
/// own release. A computed column's drop waits for the drop of each module
/// schema-bound to it, and a column's drop for the drop of each computed
/// column of its table that reads it: once a module drop can move later
/// (#1680), the computed drop it binds has to follow, and so does the drop of
/// what that computed column reads. Empty for any other change.
fn waits_for(
    changes: &[PlannedChange],
    at: usize,
    edges: &[ExpressionEdge],
    names: &CatalogNames,
    alike: &Alike,
) -> Vec<usize> {
    let change = &changes[at].change;
    let among = |waits: &dyn Fn(&Change) -> bool| -> Vec<usize> {
        (0..changes.len())
            .filter(|&p| p != at && waits(&changes[p].change))
            .collect()
    };
    if let Change::DropComputedColumn { table, name, .. } = change {
        let table = names.table(table);
        return among(&|c: &Change| {
            matches!(c, Change::DropModule { id, .. } if edges.iter().any(|e| {
                e.from_column.is_none()
                    && e.from_schema_bound
                    && alike.object(&e.from, &id.object_name())
                    && alike.object(&e.to, &table)
                    && e.to_column.as_deref().is_some_and(|x| alike.same(x, name))
            }))
        });
    }
    if let Change::DropColumn { column, .. } = change {
        let table = names.table(&column.table);
        let read = names.column(&column.table, &column.name);
        return among(&|c: &Change| {
            matches!(c, Change::DropComputedColumn { table: t, name: computed, .. }
            if alike.object(&names.table(t), &table)
                && edges.iter().any(|e| {
                    alike.object(&e.from, &table)
                        && e.from_column.as_deref().is_some_and(|f| alike.same(f, computed))
                        && alike.object(&e.to, &table)
                        && e.to_column.as_deref().is_some_and(|x| alike.same(x, &read))
                }))
        });
    }
    let Some(object) = dropped_object(change) else {
        return Vec::new();
    };
    let is_function = matches!(change, Change::DropModule { .. });
    // Whether a change removes a computed column that calls `object`.
    let releases = |change: &Change| {
        edges.iter().any(|e| {
            e.from_column.is_some()
                && alike.object(&e.to, &object)
                && (matches!(change, Change::DropComputedColumn { table, name, .. }
                    if alike.object(&names.table(table), &e.from)
                        && e.from_column.as_deref().is_some_and(|c| alike.same(c, name)))
                    || matches!(change, Change::DropTable { name, .. } if alike.object(name, &e.from)))
        })
    };
    let bound = |change: &Change| {
        dropped_object(change).is_some_and(|dependent| {
            edges.iter().any(|e| {
                e.from_column.is_none()
                    && e.from_schema_bound
                    && alike.object(&e.from, &dependent)
                    && alike.object(&e.to, &object)
            })
        })
    };
    among(&|c: &Change| (is_function && releases(c)) || bound(c))
}

/// Which drop runs before which, as `release` ordered them: each pair is a
/// change and a drop that waits for it. The rename search reads these to
/// move a module drop only where its edges allow (#1680), instead of
/// keeping the place `release` gave it among every drop.
pub(crate) fn drop_precedence(
    cs: &ChangeSet,
    edges: &[ExpressionEdge],
    alike: &Alike,
) -> Vec<(Change, Change)> {
    let names = CatalogNames::of(cs);
    let mut out = Vec::new();
    for at in 0..cs.changes.len() {
        for p in waits_for(&cs.changes, at, edges, &names, alike) {
            out.push((cs.changes[p].change.clone(), cs.changes[at].change.clone()));
        }
    }
    out
}

/// Moves each drop to right after the last change it waits for
/// ([`waits_for`]): a function's drop after the last removal of a computed
/// column calling it, what it is schema-bound to after it, and a computed
/// column's drop, and what that column reads, after a module bound to it.
fn release(
    cs: &mut ChangeSet,
    edges: &[ExpressionEdge],
    names: &CatalogNames,
    alike: &Alike,
) -> Result<usize, String> {
    let mut moved = 0;
    // Bounded: each move puts one drop after another, and a plan whose drops
    // would have to keep chasing each other has no order to settle on.
    let limit = cs.changes.len() * cs.changes.len() + 1;
    let mut steps = 0;
    loop {
        let mut changed = false;
        for at in 0..cs.changes.len() {
            let Some(last) = waits_for(&cs.changes, at, edges, names, alike)
                .into_iter()
                .max()
            else {
                continue;
            };
            if last <= at {
                continue;
            }
            steps += 1;
            if steps > limit {
                return Err(format!(
                    "the drops of {} and what it depends on cannot be ordered: each waits for \
                     another. Drop them in plans of their own.",
                    cs.changes[at].change.subject()
                ));
            }
            let change: PlannedChange = cs.changes.remove(at);
            cs.changes.insert(last, change);
            moved += 1;
            changed = true;
            break;
        }
        if !changed {
            return Ok(moved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{ComputedColumn, ModuleId, ModuleKind};

    fn t(s: &str) -> TableName {
        s.parse().unwrap()
    }

    fn computed_edge(
        table: &str,
        computed: &str,
        to: &str,
        column: Option<&str>,
    ) -> ExpressionEdge {
        ExpressionEdge {
            from: t(table),
            from_column: Some(computed.into()),
            from_schema_bound: false,
            to: t(to),
            to_column: column.map(Into::into),
            to_kind: if column.is_some() { "U" } else { "FN" }.into(),
        }
    }

    fn bound_edge(module: &str, to: &str, column: Option<&str>) -> ExpressionEdge {
        ExpressionEdge {
            from: t(module),
            from_column: None,
            from_schema_bound: true,
            to: t(to),
            to_column: column.map(Into::into),
            to_kind: if column.is_some() { "U" } else { "FN" }.into(),
        }
    }

    fn computed() -> ComputedColumn {
        ComputedColumn {
            expression: "x".into(),
            persisted: false,
            not_null: false,
        }
    }

    fn drop_computed(table: &str, name: &str) -> PlannedChange {
        PlannedChange::new(Change::DropComputedColumn {
            table: t(table),
            name: name.into(),
            computed: computed(),
        })
    }

    fn add_computed(table: &str, name: &str) -> PlannedChange {
        PlannedChange::new(Change::AddComputedColumn {
            table: t(table),
            name: name.into(),
            computed: computed(),
        })
    }

    fn drop_module(name: &str) -> PlannedChange {
        PlannedChange::new(Change::DropModule {
            id: ModuleId::Named(t(name)),
            kind: ModuleKind::Function,
        })
    }

    fn drop_table(name: &str) -> PlannedChange {
        PlannedChange::new(Change::DropTable {
            uid: "t_000000".parse().unwrap(),
            name: t(name),
            detach_from: None,
        })
    }

    fn order(changes: Vec<PlannedChange>, edges: &[ExpressionEdge]) -> Result<Vec<String>, String> {
        let mut cs = ChangeSet { changes };
        order_by_edges(&mut cs, edges, &Alike::default())?;
        Ok(cs
            .changes
            .iter()
            .map(|p| p.change.subject().to_string())
            .collect())
    }

    /// An added computed column calling a function the plan changes, by a
    /// name the collation reads as the function's, is refused; a spelling
    /// the collation keeps apart, another schema, or a view's drop is not
    /// (#1459).
    #[test]
    fn an_added_computed_column_is_refused_by_the_collations_reading_of_its_calls() {
        let add = |expression: &str| {
            PlannedChange::new(Change::AddComputedColumn {
                table: t("dbo.t"),
                name: "c".into(),
                computed: ComputedColumn {
                    expression: expression.into(),
                    persisted: false,
                    not_null: false,
                },
            })
        };
        let plan = |expression: &str, change: PlannedChange| ChangeSet {
            changes: vec![add(expression), change],
        };
        let dialect = pbps_mssql::Mssql;
        // dbo.t as declared, with these columns, `int` unless typed.
        let declared = |columns: &[&str]| {
            let mut table = pbps_model::Table::default();
            for column in columns {
                let (name, ty) = column.split_once(' ').unwrap_or((column, "int"));
                table.columns.insert(
                    name.to_owned(),
                    pbps_model::Column::new(ty.parse().unwrap()),
                );
            }
            let mut schema = pbps_model::Schema::default();
            schema.tables.insert(t("dbo.t"), table);
            schema
        };
        let plain = declared(&["id", "a"]);
        let ai = Alike::from_pairs([("cafe".to_owned(), "café".to_owned())]);
        let ai_geo = || Alike::from_pairs([("geo".to_owned(), "géo".to_owned())]);
        let refused = refuse_added_calls(
            &plan("[dbo].[cafe]([a])", drop_module("dbo.café")),
            &plain,
            &dialect,
            &ai,
        )
        .unwrap_err();
        assert!(
            refused.contains("computed column dbo.t.c calls `dbo.café` as `dbo.cafe`"),
            "{refused}"
        );
        assert!(refused.contains("this plan drops it"), "{refused}");
        // Kept apart by the collation.
        assert!(
            refuse_added_calls(
                &plan("[dbo].[cafe]([a])", drop_module("dbo.café")),
                &plain,
                &dialect,
                &Alike::default()
            )
            .is_ok()
        );
        // A longer name SQL Server continues with `#`, `@` or `$` (#1668
        // review).
        for longer in ["dbo.cafe#helper([a])", "dbo.cafe@x([a])", "dbo.cafe$1([a])"] {
            assert!(
                refuse_added_calls(
                    &plan(longer, drop_module("dbo.café")),
                    &plain,
                    &dialect,
                    &ai
                )
                .is_ok(),
                "{longer}"
            );
        }
        // A spatial column's property, which no parenthesis follows.
        assert!(
            refuse_added_calls(
                &plan("[geo].Lat", drop_module("géo.Lat")),
                &plain,
                &dialect,
                &ai_geo()
            )
            .is_ok()
        );
        // A column's method, `geo.STAsText()`, beside a function `géo.STAsText`
        // the plan drops: gone before the add, which binds the method
        // (#1668 review). With no such column it is a call to the dropped
        // function, and refused.
        let geo = Alike::from_pairs([("geo".to_owned(), "géo".to_owned())]);
        let method = plan("geo.STAsText()", drop_module("géo.STAsText"));
        for ty in ["geography", "geometry", "hierarchyid", "xml"] {
            let typed = declared(&["id", &format!("geo {ty}")]);
            assert!(
                refuse_added_calls(&method, &typed, &dialect, &geo).is_ok(),
                "{ty}"
            );
        }
        assert!(refuse_added_calls(&method, &plain, &dialect, &geo).is_err());
        // #1677: a column of a type with no methods is no receiver, so
        // `x.f(a)` on an `int` column `x` calls the dropped function `x.f`,
        // and is refused; so is one of `nvarchar`.
        let x = Alike::from_pairs([("x".to_owned(), "X".to_owned())]);
        let call = plan("x.f([a])", drop_module("X.f"));
        for ty in ["int", "nvarchar(50)"] {
            let refused =
                refuse_added_calls(&call, &declared(&["a", &format!("x {ty}")]), &dialect, &x)
                    .unwrap_err();
            assert!(refused.contains("calls `X.f` as `x.f`"), "{ty}: {refused}");
        }
        // Another schema.
        assert!(
            refuse_added_calls(
                &plan("[x].[cafe]([a])", drop_module("dbo.café")),
                &plain,
                &dialect,
                &ai
            )
            .is_ok()
        );
        // A view is not what a computed column calls.
        let view = PlannedChange::new(Change::DropModule {
            id: ModuleId::Named(t("dbo.café")),
            kind: ModuleKind::View,
        });
        assert!(
            refuse_added_calls(&plan("[dbo].[cafe]([a])", view), &plain, &dialect, &ai).is_ok()
        );
        // Nothing to ask the engine without a function change.
        assert!(
            added_call_spellings(
                &ChangeSet {
                    changes: vec![add("[dbo].[cafe]([a])")]
                },
                &plain,
                &dialect
            )
            .is_empty()
        );
    }

    /// A function drop follows the last removal of a computed column that
    /// calls it, by the catalog's edge: its column's drop, or its table's.
    /// A function of the same leaf name in another schema, which the edge
    /// does not name, keeps its place (#1423 round 8).
    #[test]
    fn a_function_drop_follows_the_removal_of_what_calls_it() {
        let edges = [computed_edge("dbo.t", "c", "x.f", None)];
        let ran = order(
            vec![
                drop_module("x.f"),
                drop_module("dbo.f"),
                drop_computed("dbo.t", "c"),
            ],
            &edges,
        )
        .unwrap();
        assert_eq!(ran, ["dbo.f", "dbo.t", "x.f"]);
        let ran = order(vec![drop_module("x.f"), drop_table("dbo.t")], &edges).unwrap();
        assert_eq!(ran, ["dbo.t", "x.f"]);
    }

    /// What a released function is schema-bound to, among the plan's drops,
    /// follows it: another function, and a table (#1432).
    #[test]
    fn what_a_released_function_is_bound_to_follows_it() {
        let edges = [
            computed_edge("dbo.u", "c", "dbo.f", None),
            bound_edge("dbo.f", "dbo.g", None),
            bound_edge("dbo.f", "dbo.lookup", None),
        ];
        let ran = order(
            vec![
                drop_module("dbo.f"),
                drop_module("dbo.g"),
                drop_table("dbo.lookup"),
                drop_table("dbo.u"),
            ],
            &edges,
        )
        .unwrap();
        let at = |s: &str| ran.iter().position(|x| x == s).unwrap();
        assert!(at("dbo.u") < at("dbo.f"), "{ran:?}");
        assert!(at("dbo.f") < at("dbo.g"), "{ran:?}");
        assert!(at("dbo.f") < at("dbo.lookup"), "{ran:?}");

        // The same order, as the pairs the rename search keeps (#1680): `f`
        // after `u`, and `g` and `lookup` after `f`; nothing about `g` and
        // `lookup`, which the edges do not relate.
        let cs = ChangeSet {
            changes: vec![
                drop_table("dbo.u"),
                drop_module("dbo.f"),
                drop_module("dbo.g"),
                drop_table("dbo.lookup"),
            ],
        };
        let subject = |c: &Change| c.subject().to_string();
        let mut pairs: Vec<(String, String)> = drop_precedence(&cs, &edges, &Alike::default())
            .iter()
            .map(|(a, b)| (subject(a), subject(b)))
            .collect();
        pairs.sort();
        let ab = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(
            pairs,
            [
                ab("dbo.f", "dbo.g"),
                ab("dbo.f", "dbo.lookup"),
                ab("dbo.u", "dbo.f"),
            ]
        );
    }

    /// #1680 review: a module the rename search can now move later is also
    /// schema-bound to a computed column this plan drops, so that column's
    /// drop follows the module's, and the drop of what the column reads
    /// follows it in turn. `dbo.a`'s drop releases `f`, `f` reads the computed
    /// `dbo.b.c`, and `c` reads `dbo.b.x`.
    #[test]
    fn a_computed_drop_follows_the_module_bound_to_it_and_its_inputs_follow_it() {
        let edges = [
            computed_edge("dbo.a", "called", "dbo.f", None),
            bound_edge("dbo.f", "dbo.b", Some("c")),
            computed_edge("dbo.b", "c", "dbo.b", Some("x")),
        ];
        let drop_x = || {
            PlannedChange::new(Change::DropColumn {
                uid: "c_000000".parse().unwrap(),
                column: t("dbo.b").column("x"),
            })
        };
        let plan = || {
            vec![
                drop_computed("dbo.b", "c"),
                drop_x(),
                drop_table("dbo.a"),
                drop_module("dbo.f"),
            ]
        };
        let mut cs = ChangeSet { changes: plan() };
        order_by_edges(&mut cs, &edges, &Alike::default()).unwrap();
        let at = |c: &Change| cs.changes.iter().position(|p| p.change == *c).unwrap();
        let (a, f) = (
            at(&drop_table("dbo.a").change),
            at(&drop_module("dbo.f").change),
        );
        let (c, x) = (
            at(&drop_computed("dbo.b", "c").change),
            at(&drop_x().change),
        );
        assert!(a < f && f < c && c < x, "{:?}", cs.changes);
        let pairs = drop_precedence(&cs, &edges, &Alike::default());
        assert!(
            pairs.contains(&(
                drop_module("dbo.f").change,
                drop_computed("dbo.b", "c").change
            )),
            "{pairs:?}"
        );
        assert!(
            pairs.contains(&(drop_computed("dbo.b", "c").change, drop_x().change)),
            "{pairs:?}"
        );

        // Negative: not bound to `f`, the computed column's drop keeps its
        // place, and so does the column's.
        let edges = [
            computed_edge("dbo.a", "called", "dbo.f", None),
            computed_edge("dbo.b", "c", "dbo.b", Some("x")),
        ];
        let mut cs = ChangeSet { changes: plan() };
        order_by_edges(&mut cs, &edges, &Alike::default()).unwrap();
        assert_eq!(cs.changes[0].change, drop_computed("dbo.b", "c").change);
        assert_eq!(cs.changes[1].change, drop_x().change);
    }

    /// Names are one where the catalog's collation says so, and only there:
    /// in a case-sensitive database column `A` is not the `a` a computed
    /// column reads, and its retype is not refused (#1455 review).
    #[test]
    fn names_match_by_the_catalogs_collation_not_a_fold() {
        let reads = [computed_edge("dbo.t", "c", "dbo.t", Some("a"))];
        let drop_upper = || {
            PlannedChange::new(Change::DropColumn {
                uid: "c_000000".parse().unwrap(),
                column: t("dbo.t").column("A"),
            })
        };
        let sensitive = Alike::default();
        let mut cs = ChangeSet {
            changes: vec![drop_upper()],
        };
        assert_eq!(order_by_edges(&mut cs, &reads, &sensitive), Ok(0));
        let insensitive = Alike::from_pairs([("a".to_owned(), "A".to_owned())]);
        let e = order_by_edges(&mut cs, &reads, &insensitive).unwrap_err();
        assert!(e.contains("reads `A`"), "{e}");
    }

    /// The refusals, each by the catalog's edge and so by the engine's
    /// spelling: an input change under a standing computed column (`café`
    /// read by `[cafe]` under an accent-insensitive collation is the edge,
    /// not the text, #1426), a function change under a standing one, and a computed column's drop under a schema-bound module (#1439:
    /// `is_schema_bound`, not the word).
    #[test]
    fn the_edges_refuse_what_the_engine_would() {
        let input = [computed_edge("dbo.t", "c", "dbo.t", Some("café"))];
        let retype = PlannedChange::new(Change::DropColumn {
            uid: "c_000000".parse().unwrap(),
            column: t("dbo.t").column("café"),
        });
        let e = order(vec![retype.clone()], &input).unwrap_err();
        assert!(e.contains("reads `café`"), "{e}");
        // Dropped with it, the computed column is out of the way first.
        assert!(order(vec![drop_computed("dbo.t", "c"), retype], &input).is_ok());

        let calls = [computed_edge("dbo.t", "c", "dbo.f", None)];
        let e = order(vec![drop_module("dbo.f")], &calls).unwrap_err();
        assert!(e.contains("calls `dbo.f`"), "{e}");
        // Dropped to be added again, the column's edge is its old
        // expression's, gone before the function changes: no refusal, and
        // the drop moves after the column's (#1455 review). What the new
        // expression calls is the differ's screen.
        let ran = order(
            vec![
                drop_module("dbo.f"),
                drop_computed("dbo.t", "c"),
                add_computed("dbo.t", "c"),
            ],
            &calls,
        )
        .unwrap();
        assert!(
            ran.iter().position(|x| x == "dbo.f") > ran.iter().position(|x| x == "dbo.t"),
            "{ran:?}"
        );

        let viewed = [bound_edge("dbo.v", "dbo.t", Some("c"))];
        let e = order(vec![drop_computed("dbo.t", "c")], &viewed).unwrap_err();
        assert!(e.contains("schema-bound dbo.v"), "{e}");
        // Not schema-bound: no refusal, whatever its text says.
        let plain = [ExpressionEdge {
            from_schema_bound: false,
            ..bound_edge("dbo.v", "dbo.t", Some("c"))
        }];
        assert!(order(vec![drop_computed("dbo.t", "c")], &plain).is_ok());
        // Dropped in the plan, the module goes first.
        assert!(
            order(
                vec![drop_module("dbo.v"), drop_computed("dbo.t", "c")],
                &viewed
            )
            .is_ok()
        );
    }
}
