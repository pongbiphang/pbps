//! The resolver graph's constraints, classified against the edges effects
//! imply (#1954 section 4, DEC-1954.7). Tests and debug builds only.
//!
//! `graph::constraints` derives ordering facts by hand and some of its edges
//! only keep the original order. Containment is not the criterion, so every
//! edge it emits is classified and none is left unexplained.

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::operation::Operations;
use pbps_model::resolver::{OrderEdge, OrderReason};

use super::edges::{Because, Graph, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// A real dependency `graph::constraints` emits, implied by effects
    /// directly or transitively.
    Covered,
    /// A `Binding` edge, which the resolver's evidence supplies and which
    /// stays in `graph.rs` (S7a).
    Binding,
    /// Order kept between operations whose effects do not interact.
    PositionOnly,
    /// A name released and claimed again by another object: a name slot, S1b-ii
    /// (#1964).
    Slot,
    /// The executing principal must hold a permission when the step runs: an
    /// execution requirement from the lowering (#1954 section 1), not an
    /// effect.
    Execution,
    /// A real dependency effects do not imply: an `effects` correction.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    pub edge: OrderEdge,
    pub class: Class,
    pub why: &'static str,
}

fn reaches(adjacent: &BTreeMap<usize, BTreeSet<usize>>, from: usize, to: usize) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = vec![from];
    while let Some(i) = stack.pop() {
        if i == to {
            return true;
        }
        if seen.insert(i) {
            stack.extend(adjacent.get(&i).into_iter().flatten().copied());
        }
    }
    false
}

fn row(op: &pbps_model::operation::Operation) -> bool {
    matches!(
        op.change(),
        pbps_model::Change::InsertRow { .. }
            | pbps_model::Change::UpdateRow { .. }
            | pbps_model::Change::DeleteRow { .. }
    )
}

/// Classifies `constraints`, over the operations in the order they were
/// given to it, against `effect`'s edges.
pub fn classify(
    operations: &Operations,
    constraints: &BTreeSet<OrderEdge>,
    effect: &Graph,
    _effects: &[pbps_model::operation::Effects],
) -> (Vec<Classified>, Vec<(usize, usize, Because)>) {
    let mut adjacent: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for e in &effect.edges {
        adjacent.entry(e.before).or_default().insert(e.after);
    }
    let mut graph_adjacent: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for e in constraints {
        graph_adjacent.entry(e.before).or_default().insert(e.after);
    }
    let ops = operations.as_slice();
    let classified = constraints
        .iter()
        .map(|edge| {
            let (class, why) = if edge.reason == OrderReason::Binding {
                (Class::Binding, "resolver evidence (S7a)")
            } else if reaches(&adjacent, edge.before, edge.after) {
                (Class::Covered, "implied by effects")
            } else {
                let (a, b) = (&ops[edge.before], &ops[edge.after]);
                let (released, claimed) = (addresses(a.change()), addresses(b.change()));
                let shares_a_table = released.iter().any(|x| {
                    claimed.iter().any(|y| {
                        x == y
                            || y.strip_prefix(x.as_str())
                                .is_some_and(|r| r.starts_with('.'))
                            || x.strip_prefix(y.as_str())
                                .is_some_and(|r| r.starts_with('.'))
                    })
                });
                match edge.reason {
                    OrderReason::Authorization => (
                        Class::Execution,
                        "the executing principal's permission, not an effect",
                    ),
                    OrderReason::Restoration | OrderReason::Structural | OrderReason::Identity
                        if shares_a_table && claims(b.change()) =>
                    {
                        (Class::Slot, "an address released and claimed again")
                    }
                    OrderReason::Restoration => (
                        Class::PositionOnly,
                        "a drop and an install of different objects",
                    ),
                    OrderReason::Data if row(a) && row(b) => (
                        Class::PositionOnly,
                        "two row operations on different rows keep the data order",
                    ),
                    OrderReason::Identity => {
                        (Class::PositionOnly, "shared objects(), no effect meets")
                    }
                    OrderReason::Structural | OrderReason::Data | OrderReason::Binding => {
                        (Class::Missing, "no effect implies it")
                    }
                }
            };
            Classified {
                edge: edge.clone(),
                class,
                why,
            }
        })
        .collect();
    // What effects add: an effect edge the graph's closure does not imply.
    let added = effect
        .edges
        .iter()
        .filter(|e| e.source == Source::Effect)
        .filter(|e| !reaches(&graph_adjacent, e.before, e.after))
        .map(|e| (e.before, e.after, e.because.clone()))
        .collect();
    (classified, added)
}

/// The addresses a change names, as text: the table, the column or the part
/// within its table. Names, because an address is what a slot is about.
fn addresses(change: &pbps_model::Change) -> Vec<String> {
    use pbps_model::Change;
    #[allow(clippy::wildcard_enum_match_arm)]
    match change {
        Change::DropTable { name, .. } | Change::CreateTable { name, .. } => {
            vec![name.raw().to_string()]
        }
        Change::RenameTable { from, to, .. } => vec![from.raw().to_string(), to.raw().to_string()],
        Change::AddColumn { table, name, .. } => {
            vec![format!("{table}.{name}", table = table.raw())]
        }
        Change::DropColumn { column, .. } | Change::AlterColumnDefault { column, .. } => {
            vec![column.raw().to_string()]
        }
        Change::RenameColumn {
            table, from, to, ..
        } => {
            vec![
                format!("{table}.{from}", table = table.raw()),
                format!("{table}.{to}", table = table.raw()),
            ]
        }
        Change::SetPartitionDefault { table, column, .. } => {
            vec![format!("{table}.{column}", table = table.raw())]
        }
        Change::DropIndex { table, name }
        | Change::AddIndex { table, name, .. }
        | Change::DropCheck { table, name }
        | Change::AddCheck { table, name, .. } => {
            vec![format!("{table}.{name}", table = table.raw())]
        }
        _ => Vec::new(),
    }
}

/// Whether a change takes an address: a creation or a rename into it.
fn claims(change: &pbps_model::Change) -> bool {
    use pbps_model::Change;
    matches!(
        change,
        Change::CreateTable { .. }
            | Change::AddColumn { .. }
            | Change::AddIndex { .. }
            | Change::AddCheck { .. }
            | Change::RenameTable { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnDefault { .. }
            | Change::SetPartitionDefault { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::addresses;
    use pbps_model::{Change, Column, ColumnRef, Table, TableName, Uid};

    const PART: &str = "a\nb\\c\u{200b}";

    fn table() -> TableName {
        TableName::new("app", PART)
    }

    fn uid(kind: &str) -> Uid {
        format!("{kind}_aaaaaa").parse().unwrap()
    }

    #[test]
    fn table_creation_and_removal_address_the_same_raw_slot() {
        let name = table();
        let create = Change::CreateTable {
            uid: uid("t"),
            name: name.clone(),
            table: Box::new(Table::default()),
            beside_default: None,
            moves_at_most: None,
        };
        let drop = Change::DropTable {
            uid: uid("t"),
            name,
            detach_from: None,
        };
        assert_eq!(addresses(&create), [format!("app.{PART}")]);
        assert_eq!(addresses(&drop), addresses(&create));
    }

    #[test]
    fn table_rename_records_both_raw_slots() {
        let to = TableName::new(PART, "renamed");
        let rename = Change::RenameTable {
            uid: uid("t"),
            from: table(),
            to,
            defaults: vec![],
        };
        assert_eq!(
            addresses(&rename),
            [format!("app.{PART}"), format!("{PART}.renamed")]
        );
    }

    #[test]
    fn an_added_column_and_a_column_reference_have_the_same_raw_slot() {
        let add = Change::AddColumn {
            uid: uid("c"),
            table: table(),
            name: PART.into(),
            column: Box::new(Column::new("int".parse().unwrap())),
        };
        let drop = Change::DropColumn {
            uid: uid("c"),
            column: table().column(PART),
        };
        assert_eq!(addresses(&add), [format!("app.{PART}.{PART}")]);
        assert_eq!(addresses(&drop), addresses(&add));
        // Negative: a literal backslash-n is not the newline it resembles.
        let other = Change::DropColumn {
            uid: uid("c"),
            column: ColumnRef::new(table(), r"a\nb\c\u{200b}"),
        };
        assert_ne!(addresses(&other), addresses(&drop));
    }

    #[test]
    fn column_rename_records_both_raw_slots() {
        let rename = Change::RenameColumn {
            uid: uid("c"),
            table: table(),
            from: PART.into(),
            to: format!("{PART}2"),
            table_was: None,
        };
        assert_eq!(
            addresses(&rename),
            [format!("app.{PART}.{PART}"), format!("app.{PART}.{PART}2")]
        );
    }

    #[test]
    fn a_partition_default_uses_the_same_raw_slot_as_its_column() {
        let default = Change::SetPartitionDefault {
            uid: uid("t"),
            parent: TableName::new("app", "parent"),
            from: None,
            table: table(),
            column: PART.into(),
            to: Some("1".into()),
            fallback: None,
        };
        let drop = Change::DropColumn {
            uid: uid("c"),
            column: table().column(PART),
        };
        assert_eq!(addresses(&default), [format!("app.{PART}.{PART}")]);
        assert_eq!(addresses(&default), addresses(&drop));
    }

    #[test]
    fn index_and_check_slots_keep_raw_table_and_local_names() {
        for change in [
            Change::DropIndex {
                table: table(),
                name: PART.into(),
            },
            Change::DropCheck {
                table: table(),
                name: PART.into(),
            },
        ] {
            assert_eq!(addresses(&change), [format!("app.{PART}.{PART}")]);
        }
    }
}
