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

#[test]
fn a_type_move_waits_for_its_intermediate_holder_and_refuses_a_kept_holder() {
    use pbps_diff::order::deployment::{
        Sources,
        schedule::{Declarations, Facts, schedule},
    };
    let a: pbps_model::TypeRef = "app.a".parse().unwrap();
    let held: pbps_model::TypeRef = "dst.a".parse().unwrap();
    let b: pbps_model::TypeRef = "dst.b".parse().unwrap();
    let moving = uid(UidKind::Type, "moving");
    let holder = uid(UidKind::Type, "holder");
    let definition = ManagedType::Enum {
        values: vec!["ok".into()],
    };
    let before = Schema {
        types: [
            (a.clone(), definition.clone()),
            (held.clone(), definition.clone()),
        ]
        .into(),
        ..Default::default()
    };
    let base_ids = IdsFile {
        types: [(moving.clone(), a), (holder.clone(), held.clone())].into(),
        ..Default::default()
    };
    for keep in [false, true] {
        let mut after = Schema {
            types: [(b.clone(), definition.clone())].into(),
            ..Default::default()
        };
        let mut ids = IdsFile {
            types: [(moving.clone(), b.clone())].into(),
            ..Default::default()
        };
        if keep {
            after.types.insert(held.clone(), definition.clone());
            ids.types.insert(holder.clone(), held.clone());
        }
        let base = pbps_diff::Side {
            schema: &before,
            ids: &base_ids,
        };
        let desired = pbps_diff::Side {
            schema: &after,
            ids: &ids,
        };
        let dialect = pbps_pg::Postgres::new();
        let hints = pbps_model::Hints::default();
        let logical = pbps_diff::diff(base, desired, &dialect, &hints).unwrap();
        assert!(matches!(
            logical.changes[0].change,
            Change::RenameType { .. }
        ));
        let result = schedule(
            logical,
            &Declarations {
                schema: &after,
                sources: Sources {
                    before: base,
                    after: &ids,
                },
                module_deps: &hints.module_deps,
            },
            &Facts::default(),
            &dialect,
        );
        if keep {
            assert_eq!(
                result.unwrap_err().to_string(),
                "type `dst.a`: pg_type name is already occupied by another declaration"
            );
        } else {
            let settled = result.unwrap().changes;
            assert!(matches!(settled.changes[0].change, Change::DropType { .. }));
            assert!(matches!(
                settled.changes[1].change,
                Change::RenameType { .. }
            ));
        }
    }
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
                options: Default::default(),
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

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18"]
async fn replacement_type_slots_apply_new_dependents_and_refuse_retained_bindings() {
    use pbps_diff::order::deployment::{
        Sources,
        schedule::{Declarations, Facts, schedule},
    };
    for variable in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let mut conn =
            pbps_db::Conn::connect(pbps_db::Driver::Postgres, &std::env::var(variable).unwrap())
                .await
                .unwrap();
        for array in [false, true] {
            for domain in [false, true] {
                conn.execute("BEGIN; CREATE SCHEMA resolver2321; CREATE TYPE resolver2321.mood AS ENUM ('old')").await.unwrap();
                let name: pbps_model::TypeRef = "resolver2321.mood".parse().unwrap();
                let old = uid(UidKind::Type, "old2321");
                let new = uid(UidKind::Type, "new2321");
                let base = Schema {
                    types: [(
                        name.clone(),
                        ManagedType::Enum {
                            values: vec!["old".into()],
                        },
                    )]
                    .into(),
                    ..Default::default()
                };
                let mut desired = Schema {
                    types: [(
                        name.clone(),
                        ManagedType::Enum {
                            values: vec!["new".into()],
                        },
                    )]
                    .into(),
                    ..Default::default()
                };
                let base_ids = IdsFile {
                    types: [(old, name.clone())].into(),
                    ..Default::default()
                };
                let mut ids = IdsFile {
                    types: [(new, name)].into(),
                    ..Default::default()
                };
                let ty = if array {
                    "resolver2321.mood[]"
                } else {
                    "resolver2321.mood"
                };
                if domain {
                    desired.types.insert(
                        "resolver2321.choice".parse().unwrap(),
                        ManagedType::Domain {
                            base: ty.parse().unwrap(),
                            not_null: false,
                            default: None,
                            checks: Default::default(),
                            collation: None,
                        },
                    );
                    ids.types.insert(
                        uid(UidKind::Type, "domain2321"),
                        "resolver2321.choice".parse().unwrap(),
                    );
                } else {
                    let table = Table {
                        columns: [("v".into(), pbps_model::Column::new(ty.parse().unwrap()))]
                            .into(),
                        ..Default::default()
                    };
                    desired
                        .tables
                        .insert("resolver2321.t".parse().unwrap(), table);
                    ids.tables.insert(
                        uid(UidKind::Table, "table2321"),
                        "resolver2321.t".parse().unwrap(),
                    );
                    ids.columns.insert(
                        uid(UidKind::Column, "column2321"),
                        pbps_model::ColumnRef {
                            table: "resolver2321.t".parse().unwrap(),
                            name: "v".into(),
                        },
                    );
                }
                let dialect = pbps_pg::Postgres::new();
                let hints = pbps_model::Hints::default();
                let before = pbps_diff::Side {
                    schema: &base,
                    ids: &base_ids,
                };
                let after = pbps_diff::Side {
                    schema: &desired,
                    ids: &ids,
                };
                let logical = pbps_diff::diff(before, after, &dialect, &hints).unwrap();
                let ordered = schedule(
                    logical,
                    &Declarations {
                        schema: &desired,
                        sources: Sources {
                            before,
                            after: &ids,
                        },
                        module_deps: &hints.module_deps,
                    },
                    &Facts::default(),
                    &dialect,
                )
                .unwrap()
                .changes;
                let ordered = ordered.ordered(pbps_model::PlanMode::Transactional, &dialect);
                let lowered = pbps_dialect::lower::lower_all(
                    &ordered as &dyn pbps_diff::order::PlanView<pbps_pg::Postgres>,
                    &dialect,
                )
                .unwrap();
                for statement in lowered.statements() {
                    conn.execute(&statement.sql).await.unwrap();
                }
                let live = pbps_pg::catalog::introspect_within_transaction(&mut conn)
                    .await
                    .unwrap()
                    .schema;
                assert_eq!(
                    live.types[&"resolver2321.mood".parse().unwrap()],
                    desired.types[&"resolver2321.mood".parse().unwrap()]
                );
                assert!(
                    pbps_diff::diff(
                        pbps_diff::Side {
                            schema: &live,
                            ids: &ids
                        },
                        after,
                        &dialect,
                        &hints
                    )
                    .unwrap()
                    .changes()
                    .is_empty()
                );
                // The identical desired spelling still binds the retained base uid.
                let mut retained = base.clone();
                let mut retained_ids = base_ids.clone();
                if domain {
                    retained.types.insert(
                        "resolver2321.choice".parse().unwrap(),
                        desired.types[&"resolver2321.choice".parse().unwrap()].clone(),
                    );
                    retained_ids.types.insert(
                        uid(UidKind::Type, "domain2321"),
                        "resolver2321.choice".parse().unwrap(),
                    );
                } else {
                    retained.tables = desired.tables.clone();
                    retained_ids.tables = ids.tables.clone();
                    retained_ids.columns = ids.columns.clone();
                }
                assert!(
                    pbps_diff::diff(
                        pbps_diff::Side {
                            schema: &retained,
                            ids: &retained_ids
                        },
                        after,
                        &dialect,
                        &hints
                    )
                    .is_err()
                );
                conn.execute("ROLLBACK").await.unwrap();
            }
        }
    }
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18"]
#[allow(clippy::wildcard_enum_match_arm)] // This fixture contains only label operations.
async fn enum_label_definitions_are_vetted_at_each_add_and_rename_and_resumed_prefix() {
    use pbps_model::{DeclaredTypeBefore, DeclaredTypePin};
    use pbps_pg::{pins::Within, type_refs::declared};
    let enumeration = |values: &[&str]| ManagedType::Enum {
        values: values.iter().map(|v| (*v).into()).collect(),
    };
    for variable in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let mut conn =
            pbps_db::Conn::connect(pbps_db::Driver::Postgres, &std::env::var(variable).unwrap())
                .await
                .unwrap();
        conn.execute(
            "BEGIN; CREATE SCHEMA label1635; CREATE TYPE label1635.e AS ENUM ('sad','ok')",
        )
        .await
        .unwrap();
        let name: pbps_model::TypeRef = "label1635.e".parse().unwrap();
        let pin = DeclaredTypePin {
            uid: uid(UidKind::Type, "label1635"),
            name: name.clone(),
            definition: enumeration(&["blue", "middle", "ok", "end"]),
            before: Some(DeclaredTypeBefore {
                name: name.clone(),
                definition: enumeration(&["sad", "ok"]),
            }),
        };
        let mut executed = Vec::new();
        let changes = pbps_model::ChangeSet {
            changes: vec![
                PlannedChange::new(Change::RenameEnumValue {
                    uid: pin.uid.clone(),
                    name: name.clone(),
                    from: "sad".into(),
                    to: "blue".into(),
                    ordinal: 0,
                    retained: Vec::new(),
                    definition: Box::new(enumeration(&["blue", "ok"])),
                }),
                PlannedChange::new(Change::AddEnumValue {
                    uid: pin.uid.clone(),
                    name: name.clone(),
                    value: "middle".into(),
                    placement: Some(pbps_model::EnumPlacement::After("blue".into())),
                    ordinal: 1,
                    definition: Box::new(enumeration(&["blue", "middle", "ok"])),
                }),
                PlannedChange::new(Change::AddEnumValue {
                    uid: pin.uid.clone(),
                    name: name.clone(),
                    value: "end".into(),
                    placement: Some(pbps_model::EnumPlacement::After("ok".into())),
                    ordinal: 2,
                    definition: Box::new(pin.definition.clone()),
                }),
            ],
        };
        let sql = [
            "ALTER TYPE label1635.e RENAME VALUE 'sad' TO 'blue'",
            "ALTER TYPE label1635.e ADD VALUE 'middle' AFTER 'blue'",
            "ALTER TYPE label1635.e ADD VALUE 'end' AFTER 'ok'",
        ];
        for (i, step) in changes.changes.iter().enumerate() {
            declared::check(
                &mut conn,
                std::slice::from_ref(&pin),
                &Default::default(),
                Within::CallersTransaction,
                false,
                &changes,
                &executed,
            )
            .await
            .unwrap();
            conn.execute(sql[i]).await.unwrap();
            let definition = match &step.change {
                Change::AddEnumValue { definition, .. }
                | Change::RenameEnumValue { definition, .. } => definition.as_ref().clone(),
                _ => unreachable!(),
            };
            let metadata = pbps_dialect::DeclaredTypeStatement {
                uid: pin.uid.clone(),
                name: name.clone(),
                definition: Some(definition),
                altered: false,
                after_vet: None,
            };
            declared::vet(&mut conn, &metadata, &Default::default())
                .await
                .unwrap();
            let mut statement = pbps_dialect::Statement::new(sql[i]);
            statement.declared_type = Some(metadata);
            executed.push(statement);
            declared::check(
                &mut conn,
                std::slice::from_ref(&pin),
                &Default::default(),
                Within::CallersTransaction,
                i == 2,
                &changes,
                &executed,
            )
            .await
            .unwrap();
            let mut wrong = pin.clone();
            wrong.uid = uid(UidKind::Type, "wrong label1635");
            assert!(
                declared::check(
                    &mut conn,
                    &[wrong],
                    &Default::default(),
                    Within::CallersTransaction,
                    true,
                    &changes,
                    &executed
                )
                .await
                .is_err()
            );
        }
        conn.execute("ROLLBACK").await.unwrap();
    }
}

/// DEC-1635.8: a domain tightening is vetted between its `NOT VALID` add and
/// its validation. The check that calls a function the project wrote is
/// refused with the function never run, and what is allowed is validated by
/// the tail the vet runs, over the rows that were there.
#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18"]
async fn a_domain_tightening_is_vetted_after_its_add_and_before_its_validation() {
    use pbps_dialect::DeclaredTypeStatement;
    use pbps_pg::type_refs::declared;
    let domain = |checks: &[(&str, &str)]| ManagedType::Domain {
        base: "text".parse().unwrap(),
        not_null: false,
        default: None,
        checks: checks
            .iter()
            .map(|(name, text)| ((*name).into(), (*text).to_owned()))
            .collect(),
        collation: None,
    };
    let statement = |checks: &[(&str, &str)], tail: &str| DeclaredTypeStatement {
        uid: uid(UidKind::Type, "tighten1635"),
        name: "tighten4a.code".parse().unwrap(),
        definition: Some(domain(checks)),
        altered: true,
        after_vet: Some(tail.to_owned()),
    };
    for variable in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let mut conn =
            pbps_db::Conn::connect(pbps_db::Driver::Postgres, &std::env::var(variable).unwrap())
                .await
                .unwrap();
        let ran =
            |rows: Vec<pbps_db::Row>| -> i64 { rows[0].try_get::<i64>("ran").unwrap().unwrap() };
        conn.execute(
            "BEGIN; CREATE SCHEMA tighten4a;
             CREATE SEQUENCE tighten4a.seen;
             CREATE FUNCTION tighten4a.probe(text) RETURNS boolean LANGUAGE sql
               AS $$ SELECT nextval('tighten4a.seen') > 0 $$;
             CREATE DOMAIN tighten4a.code AS text;
             CREATE TABLE tighten4a.t (v tighten4a.code);
             INSERT INTO tighten4a.t VALUES ('a'), ('bb')",
        )
        .await
        .unwrap();
        let validated = |name: &'static str| {
            format!(
                "SELECT convalidated::int::int8 AS ran FROM pg_constraint \
                 WHERE contypid = 'tighten4a.code'::regtype AND conname = '{name}'"
            )
        };
        // Added NOT VALID, a project function: refused by the vet, never run.
        conn.execute(
            "ALTER DOMAIN tighten4a.code ADD CONSTRAINT probed \
             CHECK (tighten4a.probe(VALUE)) NOT VALID",
        )
        .await
        .unwrap();
        let refused = declared::vet(
            &mut conn,
            &statement(
                &[("probed", "tighten4a.probe(VALUE)")],
                "ALTER DOMAIN tighten4a.code VALIDATE CONSTRAINT probed",
            ),
            &Default::default(),
        )
        .await;
        assert!(refused.is_err());
        assert_eq!(
            ran(conn
                .query("SELECT CASE WHEN is_called THEN last_value ELSE 0 END AS ran FROM tighten4a.seen")
                .await
                .unwrap()),
            0,
            "the function ran before the vet"
        );
        conn.execute("ALTER DOMAIN tighten4a.code DROP CONSTRAINT probed")
            .await
            .unwrap();
        // A built-in check passes the vet and the tail validates it.
        conn.execute(
            "ALTER DOMAIN tighten4a.code ADD CONSTRAINT short CHECK (length(VALUE) < 3) NOT VALID",
        )
        .await
        .unwrap();
        assert_eq!(ran(conn.query(&validated("short")).await.unwrap()), 0);
        declared::vet(
            &mut conn,
            &statement(
                &[("short", "length(VALUE) < 3")],
                "ALTER DOMAIN tighten4a.code VALIDATE CONSTRAINT short",
            ),
            &Default::default(),
        )
        .await
        .unwrap();
        assert_eq!(ran(conn.query(&validated("short")).await.unwrap()), 1);
        // A tail that finds a row the check refuses fails the vet.
        conn.execute(
            "ALTER DOMAIN tighten4a.code ADD CONSTRAINT tiny CHECK (length(VALUE) < 2) NOT VALID",
        )
        .await
        .unwrap();
        let failed = declared::vet(
            &mut conn,
            &statement(
                &[
                    ("short", "length(VALUE) < 3"),
                    ("tiny", "length(VALUE) < 2"),
                ],
                "ALTER DOMAIN tighten4a.code VALIDATE CONSTRAINT tiny",
            ),
            &Default::default(),
        )
        .await;
        assert!(failed.unwrap_err().to_string().contains("violate"));
        conn.execute("ROLLBACK").await.unwrap();
    }
}
