//! PostgreSQL's name slots (#1964): which namespace holds each kind of name,
//! and that two spellings are two names.

use pbps_dialect::slots::{Namespace, Same, SlotKey, TableSpace, same, slots};
use pbps_model::operation::{Address, NameKind, ObjectKey, module_key};
use pbps_model::{ModuleId, ModuleKind};
use pbps_pg::Postgres;

/// Any key stands for the table: the dialect only scopes by it.
fn table() -> ObjectKey {
    module_key(&"app.t".parse::<ModuleId>().unwrap())
}

fn address(kind: NameKind, name: &str, signature: Option<&str>) -> Address {
    Address {
        kind,
        schema: "app".into(),
        table: Some(table()),
        name: name.into(),
        signature: signature.map(Into::into),
    }
}

fn namespaces(kind: NameKind) -> Vec<Namespace> {
    slots(&Postgres::new(), &address(kind, "x", None))
        .into_iter()
        .map(|s| s.namespace)
        .collect()
}

#[test]
fn tables_views_and_indexes_share_the_schemas_relation_namespace() {
    let relations = Namespace::Relations("app".into());
    for kind in [
        NameKind::Table,
        NameKind::Module(ModuleKind::View),
        NameKind::Index,
    ] {
        assert_eq!(
            namespaces(kind),
            std::slice::from_ref(&relations),
            "{kind:?}"
        );
    }
    // A key is a constraint backed by an index: its name is in the relation
    // namespace and among its table's constraints.
    assert_eq!(
        namespaces(NameKind::Key),
        [
            relations,
            Namespace::Table(table(), TableSpace::Constraints)
        ]
    );
}

/// A table keeps its columns, its constraints and its triggers apart: a
/// column and a check may share a name.
#[test]
fn a_tables_columns_constraints_and_triggers_are_separate_namespaces() {
    for (kind, space) in [
        (NameKind::Constraint, TableSpace::Constraints),
        (NameKind::Column, TableSpace::Columns),
        (NameKind::Module(ModuleKind::Trigger), TableSpace::Triggers),
    ] {
        assert_eq!(
            namespaces(kind),
            [Namespace::Table(table(), space)],
            "{kind:?}"
        );
    }
}

#[test]
fn overloaded_routines_are_told_apart_by_their_signatures() {
    let f = |signature: &str| {
        slots(
            &Postgres::new(),
            &address(NameKind::Module(ModuleKind::Function), "f", Some(signature)),
        )
    };
    assert_ne!(f("app.f(integer)"), f("app.f(text)"));
    // Not a relation: a function may share a table's name.
    assert_eq!(
        f("app.f(integer)")[0].namespace,
        Namespace::Routines("app".into())
    );
}

#[test]
fn two_spellings_are_two_names_without_any_catalog_fact() {
    let key = |name: &str| SlotKey {
        namespace: Namespace::Relations("app".into()),
        name: name.into(),
    };
    assert_eq!(
        same(&Postgres::new(), None, &key("ab"), &key("Ab")),
        Same::No
    );
    assert_eq!(
        same(&Postgres::new(), None, &key("ab"), &key("ab")),
        Same::Yes
    );
}

#[test]
fn a_constraint_trigger_releases_its_own_and_its_clones_constraint_slots() {
    use pbps_diff::order::slots::{SlotViolation, analyse, edges, verify};
    use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
    use pbps_model::plan_names::PlanNames;
    use pbps_model::{
        Change, CheckConstraint, IdsFile, Module, PartitionBound, PartitionOf, PlannedChange,
        Schema, Table, TableName, Uid, UidKind,
    };
    let t = TableName::new("app", "t");
    let p = TableName::new("app", "p0");
    let u = TableName::new("app", "u");
    let trigger = ModuleId::Trigger {
        on: t.clone(),
        name: "audit".into(),
    };
    let module = |constraint| Module {
        kind: ModuleKind::Trigger,
        description: None,
        definition: format!(
            "{}AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.f()",
            if constraint { "CONSTRAINT " } else { "" }
        ),
    };
    let mut schema = Schema::default();
    let mut ids = IdsFile::default();
    for table in [&t, &p, &u] {
        schema.tables.insert(table.clone(), Table::default());
        ids.tables.insert(
            Uid::derived(UidKind::Table, &table.raw().to_string(), 0),
            table.clone(),
        );
    }
    schema.tables.get_mut(&p).unwrap().partition_of = Some(PartitionOf {
        parent: t.clone(),
        bound: PartitionBound::Default,
        columns: Default::default(),
    });
    for constraint in [false, true] {
        schema.modules.insert(trigger.clone(), module(constraint));
        let changes = vec![
            PlannedChange::new(Change::DropModule {
                id: trigger.clone(),
                kind: ModuleKind::Trigger,
            }),
            PlannedChange::new(Change::CreateModule {
                id: trigger.clone(),
                module: Box::new(module(constraint)),
            }),
        ];
        let names = PlanNames::of(&changes, &ids).unwrap();
        let owners = PlanOwners::new(names, &ids, &ids);
        let before = Snapshot {
            schema: &schema,
            ids: &ids,
        };
        let ops = Operations::of_plan(&changes, ModuleEdits::Rebuilt);
        let removed = pbps_dialect::slots::names(
            &Postgres::new(),
            &ops.as_slice()[0],
            &owners,
            before,
            ops.as_slice(),
        )
        .unwrap();
        let created = pbps_dialect::slots::names(
            &Postgres::new(),
            &ops.as_slice()[1],
            &owners,
            before,
            ops.as_slice(),
        )
        .unwrap();
        let count = |names: &[pbps_model::operation::Named]| {
            names
                .iter()
                .filter(|n| n.address.kind == NameKind::Constraint)
                .count()
        };
        assert_eq!(count(&removed.releases), if constraint { 2 } else { 0 });
        assert_eq!(count(&created.claims), if constraint { 2 } else { 0 });
        assert_eq!(
            count(&pbps_dialect::slots::baseline_names(
                &Postgres::new(),
                &owners,
                before
            )),
            if constraint { 2 } else { 0 }
        );
    }
    for (was, will, target, expected) in [
        (true, false, &t, true),
        (true, false, &p, true),
        (true, false, &u, false),
        (true, true, &p, false),
        (false, false, &t, false),
    ] {
        schema.modules.insert(trigger.clone(), module(was));
        let changes: Vec<_> = vec![
            Change::AddCheck {
                table: target.clone(),
                name: "audit".into(),
                constraint: CheckConstraint {
                    expression: "id > 0".into(),
                },
            },
            Change::AlterModule {
                id: trigger.clone(),
                module: Box::new(module(will)),
            },
        ]
        .into_iter()
        .map(PlannedChange::new)
        .collect();
        let names = PlanNames::of(&changes, &ids).unwrap();
        let owners = PlanOwners::new(names, &ids, &ids);
        let before = Snapshot {
            schema: &schema,
            ids: &ids,
        };
        let ops = Operations::of_plan(&changes, ModuleEdits::Rebuilt);
        let slots = analyse(ops.as_slice(), &owners, before, &Postgres::new());
        let found = edges(&slots, None, &[]);
        assert_eq!(
            found.iter().any(|e| (e.before, e.after) == (1, 0)),
            expected,
            "was={was}, will={will}, target={target:?}: {found:?}"
        );
        let (violations, _) = verify(
            ops.as_slice(),
            &slots,
            &Postgres::new(),
            None,
            &owners,
            before,
        );
        assert_eq!(
            violations
                .iter()
                .any(|v| matches!(v, SlotViolation::Held { .. })),
            expected,
            "{violations:?}"
        );
        if expected {
            let mut schedule = pbps_diff::order::Schedule::new(changes.len());
            pbps_diff::order::slots::schedule(&mut schedule, &found);
            assert_eq!(schedule.sequence().unwrap(), [1, 0]);
            let ordered = pbps_diff::order::reordered_operations(&ops, &[1, 0]);
            let slots = analyse(ordered.as_slice(), &owners, before, &Postgres::new());
            assert!(
                verify(
                    ordered.as_slice(),
                    &slots,
                    &Postgres::new(),
                    None,
                    &owners,
                    before
                )
                .0
                .is_empty()
            );
        }
    }
}

#[test]
fn an_arriving_partition_claims_a_constraint_clone_only_until_its_parent_releases_it() {
    use pbps_diff::order::slots::{SlotViolation, analyse, edges, verify};
    use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
    use pbps_model::plan_names::PlanNames;
    use pbps_model::{
        Change, CheckConstraint, IdsFile, Module, PartitionBound, PartitionOf, PlannedChange,
        Schema, Table, TableName, Uid, UidKind,
    };
    let parent = TableName::new("app", "t");
    let child = TableName::new("app", "p0");
    let parent_uid = Uid::derived(UidKind::Table, "parent", 0);
    let child_uid = Uid::derived(UidKind::Table, "child", 0);
    let id = ModuleId::Trigger {
        on: parent.clone(),
        name: "audit".into(),
    };
    let trigger = |constraint| Module {
        kind: ModuleKind::Trigger,
        description: None,
        definition: format!(
            "{}AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.f()",
            if constraint { "CONSTRAINT " } else { "" }
        ),
    };
    for attach in [false, true] {
        for (was, will, check, handover) in [
            (true, false, "audit", true),
            (true, true, "audit", false),
            (false, false, "audit", false),
            (true, false, "other", false),
        ] {
            let mut before = Schema::default();
            before.tables.insert(parent.clone(), Table::default());
            before.modules.insert(id.clone(), trigger(was));
            let mut ids = IdsFile::default();
            ids.tables.insert(parent_uid.clone(), parent.clone());
            let mut shape = Table::default();
            shape.checks.insert(
                check.into(),
                CheckConstraint {
                    expression: "id >= 0".into(),
                },
            );
            if attach {
                before.tables.insert(child.clone(), shape.clone());
                ids.tables.insert(child_uid.clone(), child.clone());
            }
            shape.partition_of = Some(PartitionOf {
                parent: parent.clone(),
                bound: PartitionBound::Default,
                columns: Default::default(),
            });
            let arrival = if attach {
                Change::AttachPartition {
                    uid: child_uid.clone(),
                    table: child.clone(),
                    parent: parent.clone(),
                    bound: PartitionBound::Default,
                    key: Vec::new(),
                    shape: Box::new(shape),
                }
            } else {
                Change::CreateTable {
                    uid: child_uid.clone(),
                    name: child.clone(),
                    table: Box::new(shape),
                    beside_default: None,
                    moves_at_most: None,
                }
            };
            let mut after_ids = ids.clone();
            after_ids.tables.insert(child_uid.clone(), child.clone());
            let changes: Vec<_> = [
                arrival,
                Change::AlterModule {
                    id: id.clone(),
                    module: Box::new(trigger(will)),
                },
            ]
            .into_iter()
            .map(PlannedChange::new)
            .collect();
            let names = PlanNames::of(&changes, &after_ids).unwrap();
            let owners = PlanOwners::new(names, &ids, &after_ids);
            let base = Snapshot {
                schema: &before,
                ids: &ids,
            };
            let ops = Operations::of_plan(&changes, ModuleEdits::Rebuilt);
            let slots = analyse(ops.as_slice(), &owners, base, &Postgres::new());
            let clones = |fx: &pbps_diff::order::slots::SlotEffects| {
                fx.claims
                    .iter()
                    .filter(|(_, h)| matches!(h, ObjectKey::Module(_)))
                    .count()
            };
            assert_eq!(
                clones(&slots.effects[0]),
                usize::from(was),
                "attach={attach}, was={was}, will={will}, check={check}"
            );
            let found = edges(&slots, None, &[]);
            assert_eq!(
                found.iter().any(|e| (e.before, e.after) == (1, 0)),
                handover,
                "{found:?}"
            );
            let mut order = pbps_diff::order::Schedule::new(changes.len());
            pbps_diff::order::slots::schedule(&mut order, &found);
            let sequence = order.sequence().unwrap();
            assert_eq!(sequence, if handover { vec![1, 0] } else { vec![0, 1] });
            let ordered = pbps_diff::order::reordered_operations(&ops, &sequence);
            let slots = analyse(ordered.as_slice(), &owners, base, &Postgres::new());
            let (violations, _) = verify(
                ordered.as_slice(),
                &slots,
                &Postgres::new(),
                None,
                &owners,
                base,
            );
            if was && will && check == "audit" {
                assert!(
                    violations
                        .iter()
                        .any(|v| matches!(v, SlotViolation::Contested { .. })),
                    "{violations:?}"
                );
            } else {
                assert!(violations.is_empty(), "{violations:?}");
                if handover {
                    assert_eq!(
                        clones(&slots.effects[1]),
                        0,
                        "the ordinary parent creates no constraint clone"
                    );
                }
            }
        }
    }
}

#[test]
fn an_attach_claims_its_standing_checks_instead_of_its_final_checks() {
    use pbps_diff::order::slots::{analyse, edges};
    use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
    use pbps_model::plan_names::PlanNames;
    use pbps_model::{
        Change, CheckConstraint, IdsFile, Module, PartitionBound, PartitionOf, PlannedChange,
        Schema, Table, TableName, Uid, UidKind,
    };
    let parent = TableName::new("app", "t");
    let child = TableName::new("app", "p0");
    let mut ids = IdsFile::default();
    let mut before = Schema::default();
    for name in [&parent, &child] {
        ids.tables.insert(
            Uid::derived(UidKind::Table, &name.raw().to_string(), 0),
            name.clone(),
        );
        before.tables.insert(name.clone(), Table::default());
    }
    let id = ModuleId::Trigger {
        on: parent.clone(),
        name: "audit".into(),
    };
    let module = |constraint| Module {
        kind: ModuleKind::Trigger,
        description: None,
        definition: format!(
            "{}AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.f()",
            if constraint { "CONSTRAINT " } else { "" }
        ),
    };
    before.modules.insert(id.clone(), module(true));
    for removed_first in [false, true] {
        let mut before = before.clone();
        let check = CheckConstraint {
            expression: "id >= 0".into(),
        };
        if removed_first {
            before
                .tables
                .get_mut(&child)
                .unwrap()
                .checks
                .insert("audit".into(), check.clone());
        }
        let shape = Table {
            partition_of: Some(PartitionOf {
                parent: parent.clone(),
                bound: PartitionBound::Default,
                columns: Default::default(),
            }),
            checks: [("audit".into(), check.clone())].into(),
            ..Table::default()
        };
        let attach = Change::AttachPartition {
            uid: ids.table_uid(&child).unwrap().clone(),
            table: child.clone(),
            parent: parent.clone(),
            bound: PartitionBound::Default,
            key: Vec::new(),
            shape: Box::new(shape),
        };
        let mut changes = Vec::new();
        if removed_first {
            changes.push(Change::DropCheck {
                table: child.clone(),
                name: "audit".into(),
            });
        }
        let at = changes.len();
        changes.push(attach);
        changes.push(Change::AddCheck {
            table: child.clone(),
            name: "audit".into(),
            constraint: check,
        });
        changes.push(Change::AlterModule {
            id: id.clone(),
            module: Box::new(module(false)),
        });
        let changes: Vec<_> = changes.into_iter().map(PlannedChange::new).collect();
        let names = PlanNames::of(&changes, &ids).unwrap();
        let owners = PlanOwners::new(names, &ids, &ids);
        let ops = Operations::of_plan(&changes, ModuleEdits::Rebuilt);
        let slots = analyse(
            ops.as_slice(),
            &owners,
            Snapshot {
                schema: &before,
                ids: &ids,
            },
            &Postgres::new(),
        );
        let found = edges(&slots, None, &[]);
        assert!(!found.iter().any(|e| e.after == at), "{found:?}");
        assert!(
            found
                .iter()
                .any(|e| (e.before, e.after) == (at + 2, at + 1)),
            "{found:?}"
        );
    }
}
