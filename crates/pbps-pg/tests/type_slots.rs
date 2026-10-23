//! pg_class and pg_type are distinct, overlapping name namespaces.

use pbps_dialect::slots::{CatalogFacts, Namespace, Occupant, SlotKey};
use pbps_diff::order::slots::{self, SlotViolation};
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
use pbps_model::plan_names::PlanNames;
use pbps_model::{
    Change, IdsFile, ManagedType, PlannedChange, Schema, Table, TableName, Uid, UidKind,
};

fn uid(kind: UidKind, seed: &str) -> Uid {
    Uid::derived(kind, seed, 0)
}
fn name() -> TableName {
    TableName::new("app", "x")
}
fn create_type(domain: bool) -> Change {
    Change::CreateType {
        uid: uid(UidKind::Type, "x"),
        name: "app.x".parse().unwrap(),
        definition: Box::new(if domain {
            ManagedType::Domain {
                base: "integer".parse().unwrap(),
                not_null: false,
                default: None,
                checks: Default::default(),
                collation: None,
            }
        } else {
            ManagedType::Enum {
                values: vec!["ok".into()],
            }
        }),
    }
}
fn check(
    changes: Vec<Change>,
    before: &Schema,
    base_ids: &IdsFile,
    ids: &IdsFile,
    facts: &CatalogFacts,
) -> Vec<SlotViolation> {
    let changes: Vec<_> = changes.into_iter().map(PlannedChange::new).collect();
    let owners = PlanOwners::new(PlanNames::of(&changes, ids).unwrap(), base_ids, ids);
    let ops = Operations::of_plan(&changes, ModuleEdits::InPlace);
    let before = Snapshot {
        schema: before,
        ids: base_ids,
    };
    let slots = slots::analyse(ops.as_slice(), &owners, before, &pbps_pg::Postgres::new());
    assert!(slots.unowned.is_empty(), "{:?}", slots.unowned);
    slots::verify(
        ops.as_slice(),
        &slots,
        &pbps_pg::Postgres::new(),
        Some(facts),
        &owners,
        before,
    )
    .0
}

#[test]
fn a_type_meets_table_and_view_row_types_but_not_indexes_or_sequences() {
    let before = Schema::default();
    let empty = IdsFile::default();
    let mut ids = IdsFile::default();
    ids.types
        .insert(uid(UidKind::Type, "x"), "app.x".parse().unwrap());
    ids.tables.insert(uid(UidKind::Table, "x"), name());
    for domain in [false, true] {
        let table = Change::CreateTable {
            uid: uid(UidKind::Table, "x"),
            name: name(),
            table: Box::new(Table::default()),
            beside_default: None,
            moves_at_most: None,
        };
        assert!(
            !check(
                vec![create_type(domain), table],
                &before,
                &empty,
                &ids,
                &CatalogFacts::default()
            )
            .is_empty()
        );
        let view = Change::CreateModule {
            id: "app.x".parse().unwrap(),
            module: Box::new(pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                definition: "CREATE VIEW app.x AS SELECT 1 AS v".into(),
                description: None,
            }),
        };
        assert!(
            !check(
                vec![create_type(domain), view],
                &before,
                &empty,
                &ids,
                &CatalogFacts::default()
            )
            .is_empty()
        );
        for (namespace, kind, conflict) in [
            (Namespace::Relations("app".into()), "index", false),
            (Namespace::Relations("app".into()), "sequence", false),
            (Namespace::Types("app".into()), "composite type", true),
            (Namespace::Types("app".into()), "enum", true),
        ] {
            let facts = CatalogFacts {
                occupants: vec![Occupant {
                    slot: SlotKey {
                        namespace,
                        name: "x".into(),
                    },
                    kind: kind.into(),
                    owner: None,
                    column: None,
                }],
                ..Default::default()
            };
            assert_eq!(
                !check(vec![create_type(domain)], &before, &empty, &ids, &facts).is_empty(),
                conflict,
                "{kind}"
            );
        }
    }
}

#[test]
fn a_table_to_type_handoff_is_an_existing_slot_edge_and_the_reverse_is_rejected() {
    let table_uid = uid(UidKind::Table, "x");
    let before = Schema {
        tables: [(name(), Table::default())].into(),
        ..Default::default()
    };
    let base_ids = IdsFile {
        tables: [(table_uid.clone(), name())].into(),
        ..Default::default()
    };
    let ids = IdsFile {
        types: [(uid(UidKind::Type, "x"), "app.x".parse().unwrap())].into(),
        ..Default::default()
    };
    let drop = Change::DropTable {
        uid: table_uid,
        name: name(),
        detach_from: None,
    };
    assert!(
        check(
            vec![drop.clone(), create_type(false)],
            &before,
            &base_ids,
            &ids,
            &CatalogFacts::default()
        )
        .is_empty()
    );
    assert!(
        !check(
            vec![create_type(false), drop.clone()],
            &before,
            &base_ids,
            &ids,
            &CatalogFacts::default()
        )
        .is_empty()
    );
    let logical: Vec<_> = vec![drop, create_type(false)]
        .into_iter()
        .map(PlannedChange::new)
        .collect();
    let owners = PlanOwners::new(PlanNames::of(&logical, &ids).unwrap(), &base_ids, &ids);
    let ops = Operations::of_plan(&logical, ModuleEdits::InPlace);
    let before = Snapshot {
        schema: &before,
        ids: &base_ids,
    };
    let facts = CatalogFacts::default();
    let slots = slots::analyse(ops.as_slice(), &owners, before, &pbps_pg::Postgres::new());
    let carried = slots::carried(
        ops.as_slice(),
        &facts,
        &owners,
        before,
        &pbps_pg::Postgres::new(),
    );
    assert!(
        slots::edges(&slots, Some(&facts), &carried)
            .iter()
            .any(|edge| edge.before == 0 && edge.after == 1)
    );
}
