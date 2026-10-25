//! PostgreSQL's engine effects, through the operation-level verifier (#1954).

use pbps_diff::order::{Baseline, Violation, analyse};
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
use pbps_model::plan_names::PlanNames;
use pbps_model::{
    Change, Column, ColumnType, Identity, IdsFile, PlannedChange, Schema, SequenceOptions, Table,
    TableName, Uid, UidKind, ValueSource,
};
use pbps_pg::Postgres;

fn type_order_violations(drop: bool, order: &[usize]) -> Vec<Violation> {
    let reference: pbps_model::TypeRef = "app.mood".parse().unwrap();
    let name = TableName::new("app", "t");
    let definition = pbps_model::ManagedType::Enum {
        values: vec!["ok".into()],
    };
    let table = Table {
        columns: [("v".into(), Column::new("app.mood".parse().unwrap()))].into(),
        ..Default::default()
    };
    let mut ids = IdsFile::default();
    let type_uid = Uid::derived(UidKind::Type, "app.mood", 0);
    let table_uid = Uid::derived(UidKind::Table, "app.t", 0);
    ids.types.insert(type_uid.clone(), reference.clone());
    ids.tables.insert(table_uid.clone(), name.clone());
    ids.columns.insert(
        Uid::derived(UidKind::Column, "app.t.v", 0),
        name.column("v"),
    );
    let populated = Schema {
        types: [(reference.clone(), definition.clone())].into(),
        tables: [(name.clone(), table.clone())].into(),
        ..Default::default()
    };
    let empty = Schema::default();
    let empty_ids = IdsFile::default();
    let (base, base_ids, desired, desired_ids, logical) = if drop {
        (
            &populated,
            &ids,
            &empty,
            &empty_ids,
            vec![
                Change::DropTable {
                    uid: table_uid,
                    name,
                    detach_from: None,
                },
                Change::DropType {
                    uid: type_uid,
                    name: reference,
                    definition: Box::new(definition),
                },
            ],
        )
    } else {
        (
            &empty,
            &empty_ids,
            &populated,
            &ids,
            vec![
                Change::CreateType {
                    uid: type_uid,
                    name: reference,
                    definition: Box::new(definition),
                },
                Change::CreateTable {
                    uid: table_uid,
                    name,
                    table: Box::new(table),
                    beside_default: None,
                    moves_at_most: None,
                },
            ],
        )
    };
    let proposed = pbps_diff::diff(
        pbps_diff::Side {
            schema: base,
            ids: base_ids,
        },
        pbps_diff::Side {
            schema: desired,
            ids: desired_ids,
        },
        &Postgres::new(),
        &Default::default(),
    )
    .unwrap();
    assert_eq!(proposed.changes.len(), 2);
    for (planned, expected) in proposed.changes.iter().zip(&logical) {
        assert_eq!(
            std::mem::discriminant(&planned.change),
            std::mem::discriminant(expected)
        );
    }
    let logical: Vec<_> = logical.into_iter().map(PlannedChange::new).collect();
    let owners = PlanOwners::new(
        PlanNames::of(&logical, desired_ids).unwrap(),
        base_ids,
        desired_ids,
    );
    let operations = Operations::of_plan(&logical, ModuleEdits::InPlace);
    let before = Snapshot {
        schema: base,
        ids: base_ids,
    };
    let after = Snapshot {
        schema: desired,
        ids: desired_ids,
    };
    let shadow = analyse(&operations, &owners, before, after, &Postgres::new());
    assert!(shadow.unowned.is_empty(), "{:?}", shadow.unowned);
    let reordered = pbps_diff::order::reordered_operations(&operations, order);
    let effects: Vec<_> = order.iter().map(|&i| shadow.effects[i].clone()).collect();
    pbps_diff::order::verify(
        reordered.as_slice(),
        &effects,
        &Baseline::of(before, &owners),
        &Default::default(),
    )
}

#[test]
fn a_created_type_precedes_its_table_in_the_schedule_and_shadow_verifier() {
    assert!(type_order_violations(false, &[0, 1]).is_empty());
    assert!(!type_order_violations(false, &[1, 0]).is_empty());
}

#[test]
fn a_dropped_type_follows_its_last_table_in_the_schedule_and_shadow_verifier() {
    assert!(type_order_violations(true, &[0, 1]).is_empty());
    assert!(!type_order_violations(true, &[1, 0]).is_empty());
}

fn t() -> TableName {
    TableName::new("public", "t")
}

fn ty(name: &str) -> ColumnType {
    name.parse().unwrap()
}

fn schema(column_ty: &str, identity: Identity) -> Schema {
    let mut column = Column::new(ty(column_ty));
    column.nullable = false;
    column.value_source = ValueSource::Identity(identity);
    let mut table = Table::default();
    table.columns.insert("id".into(), column);
    let mut schema = Schema::default();
    schema.tables.insert(t(), table);
    schema
}

/// A kept identity whose bounds change beside a retype of its column, checked
/// in `order` (indexes into `[retune, retype]`).
fn violations(from: &str, to: &str, order: &[usize]) -> Vec<Violation> {
    let retuned = Identity {
        options: SequenceOptions {
            max: Some(100),
            ..SequenceOptions::default()
        },
        ..Identity::new(1, 1)
    };
    let (before, after) = (schema(from, Identity::new(1, 1)), schema(to, retuned));
    let mut ids = IdsFile::default();
    let uid = Uid::derived(UidKind::Column, "t.id", 0);
    ids.tables.insert(Uid::derived(UidKind::Table, "t", 0), t());
    ids.columns.insert(uid.clone(), t().column("id"));
    let logical: Vec<PlannedChange> = vec![
        Change::AlterColumnNumbering {
            uid: uid.clone(),
            column: t().column("id"),
            ty: ty(to),
            from: ValueSource::Identity(Identity::new(1, 1)),
            to: ValueSource::Identity(retuned),
        },
        Change::AlterColumnType {
            uid,
            column: t().column("id"),
            from: ty(from),
            to: ty(to),
            from_nullable: false,
            to_nullable: false,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
            using: None,
        },
    ]
    .into_iter()
    .map(PlannedChange::new)
    .collect();
    let ops = Operations::of_plan(&logical, ModuleEdits::InPlace);
    let names = PlanNames::of(&logical, &ids).unwrap();
    let owners = PlanOwners::new(names, &ids, &ids);
    let (was, will) = (
        Snapshot {
            schema: &before,
            ids: &ids,
        },
        Snapshot {
            schema: &after,
            ids: &ids,
        },
    );
    let baseline = Baseline::of(was, &owners);
    let analysed = analyse(&ops, &owners, was, will, &Postgres::new());
    assert!(analysed.unowned.is_empty(), "{:?}", analysed.unowned);
    let ordered = pbps_diff::order::reordered_operations(&ops, order);
    let effects: Vec<_> = order.iter().map(|&i| analysed.effects[i].clone()).collect();
    pbps_diff::order::verify(ordered.as_slice(), &effects, &baseline, &Default::default())
}

#[test]
fn a_kept_identity_retuned_goes_before_a_retype_that_narrows_its_column() {
    // Measured on 16 and 18: the engine moves an identity's sequence with its
    // column and refuses the retype while a bound for the wider type stands.
    assert_eq!(violations("integer", "smallint", &[0, 1]), []);
}

#[test]
fn a_kept_identity_retuned_goes_after_a_retype_that_widens_its_column() {
    assert_eq!(violations("smallint", "integer", &[1, 0]), []);
    // Negative: its new bounds are set for the wider type, which the narrower
    // column refuses.
    assert!(
        !violations("smallint", "integer", &[0, 1]).is_empty(),
        "the retune before a widening is refused"
    );
}

#[test]
fn domain_bases_are_created_first_and_dropped_last_even_when_names_sort_the_other_way() {
    let enumeration: pbps_model::TypeRef = "app.a_base".parse().unwrap();
    let domain: pbps_model::TypeRef = "app.z_domain".parse().unwrap();
    let schema = Schema {
        types: [
            (
                enumeration.clone(),
                pbps_model::ManagedType::Enum {
                    values: vec!["ok".into()],
                },
            ),
            (
                domain.clone(),
                pbps_model::ManagedType::Domain {
                    base: "app.a_base".parse().unwrap(),
                    not_null: false,
                    default: None,
                    checks: Default::default(),
                    collation: None,
                },
            ),
        ]
        .into(),
        ..Default::default()
    };
    let ids = IdsFile {
        types: [
            ("y_000001".parse().unwrap(), enumeration.clone()),
            ("y_000002".parse().unwrap(), domain.clone()),
        ]
        .into(),
        ..Default::default()
    };
    let empty = Schema::default();
    let empty_ids = IdsFile::default();
    let full = pbps_diff::Side {
        schema: &schema,
        ids: &ids,
    };
    let none = pbps_diff::Side {
        schema: &empty,
        ids: &empty_ids,
    };
    let created = pbps_diff::diff(none, full, &Postgres::new(), &Default::default()).unwrap();
    assert!(
        matches!(&created.changes[0].change, Change::CreateType { name, .. } if name == &enumeration)
    );
    let dropped = pbps_diff::diff(full, none, &Postgres::new(), &Default::default()).unwrap();
    assert!(matches!(&dropped.changes[0].change, Change::DropType { name, .. } if name == &domain));
}
