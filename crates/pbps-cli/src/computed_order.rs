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
//!   or nullability change of a column it reads (15336, 4922). Standing, or
//!   added again in the plan, it blocks an alter or drop of a function it
//!   calls (3729). A schema-bound module over a computed column blocks the
//!   column's drop (4922). Each is refused by name.
//! - Nothing is rebuilt: a module over a computed column the plan changes is
//!   refused, not dropped and recreated around it. That is follow-up scope.
//!
//! The differ keeps its over-approximating refusals as an offline screen,
//! where a false yes costs a second plan; the moves are only here.

use pbps_model::{Change, ChangeSet, PlannedChange, TableName};
use pbps_mssql::catalog::ExpressionEdge;

/// A fold under which two spellings may name one object: SQL Server compares
/// names under the database's collation, which a plan does not know, and the
/// common ones ignore case. Only ever widens a match.
fn same(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

fn same_object(a: &TableName, b: &TableName) -> bool {
    same(&a.schema, &b.schema) && same(&a.name, &b.name)
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
            .any(|(t, c)| same_object(t, table) && same(c, column))
    };
    let table_gone = |table: &TableName| tables_dropped.iter().any(|t| same_object(t, table));
    // There before the plan and there throughout.
    let standing = |table: &TableName, column: &str| {
        !table_gone(table) && !is(&dropped, table, column) && !is(&added, table, column)
    };
    // There when a module change reaches it: standing, or added again at
    // (9, 3), after the module drops of class 0 and before the alters of 14.
    let calls_then = |table: &TableName, column: &str| {
        !table_gone(table) && (!is(&dropped, table, column) || is(&added, table, column))
    };
    let computed_edges = || edges.iter().filter(|e| e.from_column.is_some());

    let mut refused = Vec::new();
    for p in &cs.changes {
        if let Some((table, column, what)) = input_change(&p.change, &names) {
            for e in computed_edges() {
                let computed = e.from_column.as_deref().unwrap_or_default();
                if same_object(&e.from, &table)
                    && same_object(&e.to, &table)
                    && e.to_column.as_deref().is_some_and(|c| same(c, &column))
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
                // Dropped for good, the column is out of the way first: the
                // function's drop is moved after it below.
                let blocks = if what == "drops" {
                    !table_gone(&e.from) && !is(&dropped, &e.from, computed)
                        || is(&added, &e.from, computed) && is(&dropped, &e.from, computed)
                } else {
                    calls_then(&e.from, computed)
                };
                if same_object(&e.to, &function) && blocks {
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
                    && same_object(&e.to, &table)
                    && e.to_column.as_deref().is_some_and(|c| same(c, name))
                    && !modules_dropped.iter().any(|m| same_object(m, &e.from))
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
    release(cs, edges, &names)
}

/// Moves each function drop to right after the last removal of a computed
/// column calling it, and what it is schema-bound to after it.
fn release(
    cs: &mut ChangeSet,
    edges: &[ExpressionEdge],
    names: &CatalogNames,
) -> Result<usize, String> {
    // What a change drops, by its catalog name: a module or a table.
    let drops = |change: &Change| -> Option<TableName> {
        if let Change::DropModule { id, .. } = change {
            Some(id.object_name())
        } else if let Change::DropTable { name, .. } = change {
            Some(name.clone())
        } else {
            None
        }
    };
    // Whether a change removes a computed column that calls `function`.
    let releases = |change: &Change, function: &TableName| {
        edges.iter().any(|e| {
            e.from_column.is_some()
                && same_object(&e.to, function)
                && (matches!(change, Change::DropComputedColumn { table, name, .. }
                    if same_object(&names.table(table), &e.from)
                        && e.from_column.as_deref().is_some_and(|c| same(c, name)))
                    || matches!(change, Change::DropTable { name, .. } if same_object(name, &e.from)))
        })
    };
    let mut moved = 0;
    // Bounded: each move puts one drop after another, and a plan whose drops
    // would have to keep chasing each other has no order to settle on.
    let limit = cs.changes.len() * cs.changes.len() + 1;
    let mut steps = 0;
    loop {
        let mut changed = false;
        for at in 0..cs.changes.len() {
            let Some(object) = drops(&cs.changes[at].change) else {
                continue;
            };
            let is_function = matches!(cs.changes[at].change, Change::DropModule { .. });
            // After the last removal of a computed column that calls it.
            let after_release = is_function
                .then(|| {
                    cs.changes
                        .iter()
                        .rposition(|p| releases(&p.change, &object))
                })
                .flatten();
            // After every drop of something schema-bound to it, which reaches
            // it through its own release.
            let after_dependents = cs.changes.iter().rposition(|p| {
                drops(&p.change).is_some_and(|dependent| {
                    edges.iter().any(|e| {
                        e.from_column.is_none()
                            && e.from_schema_bound
                            && same_object(&e.from, &dependent)
                            && same_object(&e.to, &object)
                    })
                })
            });
            let Some(last) = after_release.max(after_dependents) else {
                continue;
            };
            if last <= at {
                continue;
            }
            steps += 1;
            if steps > limit {
                return Err(format!(
                    "the drops of {object} and what it depends on cannot be ordered: each waits \
                     for another. Drop them in plans of their own."
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
        })
    }

    fn order(changes: Vec<PlannedChange>, edges: &[ExpressionEdge]) -> Result<Vec<String>, String> {
        let mut cs = ChangeSet { changes };
        order_by_edges(&mut cs, edges)?;
        Ok(cs
            .changes
            .iter()
            .map(|p| p.change.subject().to_string())
            .collect())
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
    }

    /// The refusals, each by the catalog's edge and so by the engine's
    /// spelling: an input change under a standing computed column (`café`
    /// read by `[cafe]` under an accent-insensitive collation is the edge,
    /// not the text, #1426), a function change under a standing or re-added
    /// one, and a computed column's drop under a schema-bound module (#1439:
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
        let e = order(
            vec![
                drop_module("dbo.f"),
                drop_computed("dbo.t", "c"),
                add_computed("dbo.t", "c"),
            ],
            &calls,
        )
        .unwrap_err();
        assert!(e.contains("calls `dbo.f`"), "{e}");

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
