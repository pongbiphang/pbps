//! Pure regressions for the object transitions a resolved plan records:
//! UID inventories across renames, replacements and generated columns, and
//! the surfaces a binding verdict must cover. No engine is involved.

use super::scratch::fixtures::ids;
use super::*;
use pbps_model::resolver::{ObjectOwnership, Surface};
use pbps_model::{Change, ChangeSet, IdsFile, Schema};
use std::collections::{BTreeMap, BTreeSet};

/// A table replaced by a view of the same name, or the reverse, holds both
/// kinds across the two snapshots. A grant after the replacement names the
/// kind at its own position in the typed sequence, never an ambiguity.
#[test]
fn an_object_grant_after_a_same_name_replacement_names_the_replacement() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{
        Column, GrantTarget, Module, ModuleId, ModuleKind, Permission, PlannedChange, Table,
    };
    use pbps_pg::resolver::capture::BindingRecord;
    use std::collections::BTreeSet;

    let name: pbps_model::TableName = "app.t".parse().unwrap();
    let view_id = ModuleId::Named(name.clone());
    let mut table = Table::default();
    table
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    let view = Module {
        kind: ModuleKind::View,
        description: None,
        definition: "SELECT 1 AS id".into(),
    };
    let relation = ObjectIdentity {
        class: "pg_class".into(),
        name: vec!["app".into(), "t".into()],
        signature: vec![],
    };
    let owned = |surface| BindingRecord {
        object: relation.clone(),
        ownership: ObjectOwnership::Surface(surface),
        bindings: vec![],
    };
    let grant = || Change::Grant {
        role: "reader".into(),
        target: GrantTarget::Object(name.clone()),
        permissions: BTreeSet::from([Permission::Select]),
    };
    let mut with_table = Schema::default();
    with_table.tables.insert(name.clone(), table.clone());
    let mut with_view = Schema::default();
    with_view.modules.insert(view_id.clone(), view.clone());
    let table_ids = ids(&with_table, &IdsFile::default());
    let uid = table_ids.table_uid(&name).unwrap().clone();
    let view_ids = ids(&with_view, &IdsFile::default());
    for to_view in [true, false] {
        let (base, base_ids, desired, desired_ids) = if to_view {
            (&with_table, &table_ids, &with_view, &view_ids)
        } else {
            (&with_view, &view_ids, &with_table, &table_ids)
        };
        let (replace, expected) = if to_view {
            (
                vec![
                    Change::DropTable {
                        uid: uid.clone(),
                        name: name.clone(),
                        detach_from: None,
                    },
                    Change::CreateModule {
                        id: view_id.clone(),
                        module: Box::new(view.clone()),
                    },
                ],
                Surface::Module(view_id.clone()),
            )
        } else {
            (
                vec![
                    Change::DropModule {
                        id: view_id.clone(),
                        kind: ModuleKind::View,
                    },
                    Change::CreateTable {
                        uid: uid.clone(),
                        name: name.clone(),
                        table: Box::new(table.clone()),
                        beside_default: None,
                        moves_at_most: None,
                    },
                ],
                Surface::Table(name.clone()),
            )
        };
        let opening_surface = if to_view {
            Surface::Table(name.clone())
        } else {
            Surface::Module(view_id.clone())
        };
        let changes = ChangeSet {
            changes: replace
                .into_iter()
                .chain([grant()])
                .map(PlannedChange::new)
                .collect(),
        };
        let transitions = super::transitions::derive(
            &changes,
            pbps_diff::Side {
                schema: base,
                ids: base_ids,
            },
            pbps_diff::Side {
                schema: desired,
                ids: desired_ids,
            },
            &[owned(opening_surface)],
            &[owned(expected.clone())],
        )
        .unwrap_or_else(|error| panic!("to_view={to_view}: {error}"));
        assert!(
            transitions.iter().any(|t| t.surface == expected),
            "to_view={to_view}: the grant names the replacement"
        );
    }
}

/// A check or index dropped after its table's rename is named by the new
/// spelling, while the opening capture owns it under the old one. Its
/// opening inventory follows the recorded table UID, so the drop is not an
/// empty transition and the plan seals.
#[test]
fn a_child_dropped_after_its_tables_rename_keeps_its_opening_inventory() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{CheckConstraint, Column, Index, IndexColumn, IndexKey, PlannedChange, Table};
    use pbps_pg::resolver::capture::BindingRecord;

    let old: pbps_model::TableName = "app.t".parse().unwrap();
    let new: pbps_model::TableName = "app.u".parse().unwrap();
    let mut table = Table::default();
    table
        .columns
        .insert("n".into(), Column::new("integer".parse().unwrap()));
    let mut keyed = table.clone();
    keyed.checks.insert(
        "c".into(),
        CheckConstraint {
            expression: "n > 0".into(),
        },
    );
    keyed.indexes.insert(
        "ix".into(),
        Index {
            columns: vec![IndexColumn {
                key: IndexKey::Column("n".into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        },
    );
    let mut base = Schema::default();
    base.tables.insert(old.clone(), keyed);
    let base_ids = ids(&base, &IdsFile::default());
    let uid = base_ids.table_uid(&old).unwrap().clone();
    let mut desired = Schema::default();
    desired.tables.insert(new.clone(), table);
    let mut desired_ids = base_ids.clone();
    desired_ids.rename_table(&old, &new);
    let relation = |name: &pbps_model::TableName| ObjectIdentity {
        class: "pg_class".into(),
        name: vec![name.schema.clone(), name.name.clone()],
        signature: vec![],
    };
    let check = ObjectIdentity {
        class: "pg_constraint".into(),
        name: vec!["c".into()],
        signature: vec![relation(&old)],
    };
    let index = ObjectIdentity {
        class: "pg_class".into(),
        name: vec!["app".into(), "ix".into()],
        signature: vec![],
    };
    let owned = |object: ObjectIdentity, surface: Surface| BindingRecord {
        object,
        ownership: ObjectOwnership::Surface(surface),
        bindings: vec![],
    };
    let opening = vec![
        owned(relation(&old), Surface::Table(old.clone())),
        owned(
            check.clone(),
            Surface::Check {
                table: old.clone(),
                name: "c".into(),
            },
        ),
        owned(
            index.clone(),
            Surface::Index {
                table: old.clone(),
                name: "ix".into(),
            },
        ),
    ];
    let compiled = vec![owned(relation(&new), Surface::Table(new.clone()))];
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::RenameTable {
                uid,
                from: old.clone(),
                to: new.clone(),
                defaults: vec![],
            }),
            PlannedChange::new(Change::DropCheck {
                table: new.clone(),
                name: "c".into(),
            }),
            PlannedChange::new(Change::DropIndex {
                table: new.clone(),
                name: "ix".into(),
            }),
        ],
    };
    let transitions = super::transitions::derive(
        &changes,
        pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        },
        pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        },
        &opening,
        &compiled,
    )
    .unwrap();
    for (surface, object) in [
        (
            Surface::Check {
                table: new.clone(),
                name: "c".into(),
            },
            &check,
        ),
        (
            Surface::Index {
                table: new.clone(),
                name: "ix".into(),
            },
            &index,
        ),
    ] {
        let dropped = transitions
            .iter()
            .find(|t| t.surface == surface)
            .expect("the drop has its own transition");
        assert_eq!(exact(dropped).0, BTreeSet::from([object.clone()]));
        assert!(dropped.after.is_empty());
    }
    let parent = transitions
        .iter()
        .find(|t| t.surface == Surface::Table(new.clone()))
        .unwrap();
    assert!(!parent.before.contains(&check) && !parent.before.contains(&index));
}

/// One DDL statement reaches past its own surface: a retype rebuilds the
/// index over the column, and a default change flips the column's own flag.
/// The table is the unit of the closing inventory (#1466): its whole tree
/// rides on the edit as references, while an untouched table's records, a
/// view tied to the table and an unqualified record never do.
#[test]
fn a_column_edit_carries_its_whole_table_and_nothing_beyond_it() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{Column, PlannedChange, Table};
    use pbps_pg::resolver::capture::BindingRecord;
    use std::collections::BTreeSet;

    let edited: pbps_model::TableName = "app.t".parse().unwrap();
    let untouched: pbps_model::TableName = "app.u".parse().unwrap();
    let mut table = Table::default();
    table
        .columns
        .insert("n".into(), Column::new("integer".parse().unwrap()));
    let mut schema = Schema::default();
    schema.tables.insert(edited.clone(), table.clone());
    schema.tables.insert(untouched.clone(), table);
    let ids = ids(&schema, &IdsFile::default());
    let relation = |name: &pbps_model::TableName| ObjectIdentity {
        class: "pg_class".into(),
        name: vec![name.schema.clone(), name.name.clone()],
        signature: vec![],
    };
    let column = |name: &pbps_model::TableName| ObjectIdentity {
        class: "column".into(),
        name: vec!["n".into()],
        signature: vec![relation(name)],
    };
    let owned = |object: ObjectIdentity, surface: Option<Surface>| BindingRecord {
        object,
        ownership: surface.map_or(ObjectOwnership::Unqualified, ObjectOwnership::Surface),
        bindings: vec![],
    };
    let index = |name: &pbps_model::TableName| ObjectIdentity {
        class: "pg_class".into(),
        name: vec![name.schema.clone(), format!("{}_ix", name.name)],
        signature: vec![],
    };
    let view = ObjectIdentity {
        class: "pg_rewrite".into(),
        name: vec!["_RETURN".into()],
        signature: vec![],
    };
    let view_edge = ObjectIdentity {
        class: "pg_depend".into(),
        name: vec!["n".into()],
        signature: vec![view.clone(), column(&edited)],
    };
    let stranger = ObjectIdentity {
        class: "pg_depend".into(),
        name: vec!["n".into()],
        signature: vec![
            ObjectIdentity {
                class: "pg_class".into(),
                name: vec!["app".into(), "unmanaged".into()],
                signature: vec![],
            },
            relation(&edited),
        ],
    };
    let mut records = Vec::new();
    for name in [&edited, &untouched] {
        records.push(owned(relation(name), Some(Surface::Table(name.clone()))));
        records.push(owned(column(name), Some(Surface::Column(name.column("n")))));
        records.push(owned(
            index(name),
            Some(Surface::Index {
                table: name.clone(),
                name: "ix".into(),
            }),
        ));
    }
    let routine = Surface::Module("app.v".parse().unwrap());
    records.push(owned(view.clone(), Some(routine.clone())));
    records.push(owned(view_edge.clone(), Some(routine)));
    records.push(owned(stranger.clone(), None));
    let uid = ids.column_uid(&edited.column("n")).unwrap().clone();
    let retype = Change::AlterColumnType {
        uid: uid.clone(),
        column: edited.column("n"),
        from: "integer".parse().unwrap(),
        to: "bigint".parse().unwrap(),
        from_nullable: true,
        to_nullable: true,
        through_external_partitions: false,
        from_collation: None,
        to_collation: None,
        using: None,
    };
    let default = Change::AlterColumnDefault {
        uid,
        column: edited.column("n"),
        from: None,
        to: Some("1".into()),
    };
    let tree = BTreeSet::from([relation(&edited), column(&edited), index(&edited)]);
    for change in [retype, default] {
        let side = pbps_diff::Side {
            schema: &schema,
            ids: &ids,
        };
        let transitions = super::transitions::derive(
            &ChangeSet {
                changes: vec![PlannedChange::new(change)],
            },
            side,
            side,
            &records,
            &records,
        )
        .unwrap();
        for side in [
            transitions
                .iter()
                .flat_map(|t| &t.before)
                .collect::<BTreeSet<_>>(),
            transitions
                .iter()
                .flat_map(|t| &t.after)
                .collect::<BTreeSet<_>>(),
        ] {
            for object in &tree {
                assert!(side.contains(object), "{object:?} is not inventoried");
            }
            for object in [
                relation(&untouched),
                column(&untouched),
                index(&untouched),
                view.clone(),
                view_edge.clone(),
                stranger.clone(),
            ] {
                assert!(!side.contains(&object), "{object:?} rode on the edit");
            }
        }
        assert!(
            transitions
                .iter()
                .flat_map(|t| &t.references)
                .all(|object| tree.contains(object))
        );
    }
}

/// A foreign key on another table names the renamed table through its
/// dependency row, its RI trigger on the renamed table and its own triggers.
/// The rename carries exactly those, on both sides; another record of the
/// referencing table, an unqualified dependent and a rename of an unrelated
/// table carry nothing.
#[test]
fn a_rename_carries_the_records_that_name_the_renamed_table() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{Column, PlannedChange, Table};
    use pbps_pg::resolver::capture::BindingRecord;
    use std::collections::BTreeSet;

    let parent: pbps_model::TableName = "app.p".parse().unwrap();
    let renamed: pbps_model::TableName = "app.q".parse().unwrap();
    let child: pbps_model::TableName = "app.c".parse().unwrap();
    let other: pbps_model::TableName = "app.o".parse().unwrap();
    let mut table = Table::default();
    table
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    let mut base = Schema::default();
    for name in [&parent, &child, &other] {
        base.tables.insert(name.clone(), table.clone());
    }
    let base_ids = ids(&base, &IdsFile::default());
    let relation = |name: &pbps_model::TableName| ObjectIdentity {
        class: "pg_class".into(),
        name: vec![name.schema.clone(), name.name.clone()],
        signature: vec![],
    };
    let identity = |class: &str, name: &str, signature: Vec<ObjectIdentity>| ObjectIdentity {
        class: class.into(),
        name: if name.is_empty() {
            vec![]
        } else {
            vec![name.into()]
        },
        signature,
    };
    let record = |object: ObjectIdentity, owner: Option<&pbps_model::TableName>| BindingRecord {
        object,
        ownership: owner.map_or(ObjectOwnership::Unqualified, |owner| {
            ObjectOwnership::Surface(Surface::Table(owner.clone()))
        }),
        bindings: vec![],
    };
    let function = identity("pg_proc", "RI_FKey_check_ins", vec![]);
    let key = identity("pg_constraint", "c_fk", vec![relation(&child)]);
    let check = identity("pg_constraint", "c_ck", vec![relation(&child)]);
    let child_trigger = identity(
        "pg_trigger",
        "",
        vec![relation(&child), key.clone(), function.clone()],
    );
    let side = |target: &pbps_model::TableName| {
        let parent_trigger = identity(
            "pg_trigger",
            "",
            vec![relation(target), key.clone(), function.clone()],
        );
        let edge = identity("pg_depend", "n", vec![key.clone(), relation(target)]);
        let stranger = identity(
            "pg_depend",
            "n",
            vec![identity("pg_rewrite", "_RETURN", vec![]), relation(target)],
        );
        let records = vec![
            record(relation(target), Some(target)),
            record(relation(&child), Some(&child)),
            record(relation(&other), Some(&other)),
            record(key.clone(), Some(&child)),
            record(check.clone(), Some(&child)),
            record(child_trigger.clone(), Some(&child)),
            record(parent_trigger.clone(), Some(&child)),
            record(edge.clone(), Some(&child)),
            record(stranger, None),
        ];
        let carried = BTreeSet::from([key.clone(), child_trigger.clone(), parent_trigger, edge]);
        (records, carried)
    };
    let (opening, opening_carried) = side(&parent);
    let (compiled, compiled_carried) = side(&renamed);
    let run = |from: &pbps_model::TableName, to: &pbps_model::TableName| {
        let uid = base_ids.table_uid(from).unwrap().clone();
        let mut desired = base.clone();
        let moved = desired.tables.remove(from).unwrap();
        desired.tables.insert(to.clone(), moved);
        let mut desired_ids = base_ids.clone();
        desired_ids.rename_table(from, to);
        let changes = ChangeSet {
            changes: vec![PlannedChange::new(Change::RenameTable {
                uid,
                from: from.clone(),
                to: to.clone(),
                defaults: vec![],
            })],
        };
        let (opening, compiled) = if from == &parent {
            (opening.clone(), compiled.clone())
        } else {
            let mut compiled = opening.clone();
            compiled[2] = record(relation(to), Some(to));
            (opening.clone(), compiled)
        };
        super::transitions::derive(
            &changes,
            pbps_diff::Side {
                schema: &base,
                ids: &base_ids,
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &desired_ids,
            },
            &opening,
            &compiled,
        )
        .unwrap()
    };
    let transitions = run(&parent, &renamed);
    let rename = transitions
        .iter()
        .find(|t| t.surface == Surface::Table(renamed.clone()))
        .unwrap();
    assert_eq!(
        rename.references,
        opening_carried.union(&compiled_carried).cloned().collect()
    );
    assert_eq!(
        rename.before,
        opening_carried
            .iter()
            .cloned()
            .chain([relation(&parent)])
            .collect()
    );
    assert_eq!(
        rename.after,
        compiled_carried
            .iter()
            .cloned()
            .chain([relation(&renamed)])
            .collect()
    );
    assert!(!rename.before.contains(&check) && !rename.after.contains(&check));
    let unrelated: pbps_model::TableName = "app.o2".parse().unwrap();
    let transitions = run(&other, &unrelated);
    assert!(transitions.iter().all(|t| t.references.is_empty()));
}

/// Dropping a table and renaming another into its name in one plan puts two
/// recorded UIDs behind one closing spelling. Both opening tables are the
/// transition's inventory: the dropped one and the renamed one.
#[test]
fn a_rename_into_a_dropped_tables_name_inventories_both_opening_tables() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{Column, PlannedChange, Table};
    use pbps_pg::resolver::capture::BindingRecord;
    use std::collections::BTreeSet;

    let target: pbps_model::TableName = "app.target".parse().unwrap();
    let other: pbps_model::TableName = "app.other".parse().unwrap();
    let mut table = Table::default();
    table
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    let mut base = Schema::default();
    base.tables.insert(target.clone(), table.clone());
    base.tables.insert(other.clone(), table.clone());
    let base_ids = ids(&base, &IdsFile::default());
    let dropped_uid = base_ids.table_uid(&target).unwrap().clone();
    let renamed_uid = base_ids.table_uid(&other).unwrap().clone();
    let mut desired = Schema::default();
    desired.tables.insert(target.clone(), table);
    let mut desired_ids = base_ids.clone();
    desired_ids.tables.remove(&dropped_uid);
    desired_ids
        .columns
        .retain(|_, column| column.table != target);
    desired_ids.rename_table(&other, &target);
    let relation = |name: &pbps_model::TableName| ObjectIdentity {
        class: "pg_class".into(),
        name: vec![name.schema.clone(), name.name.clone()],
        signature: vec![],
    };
    let owned = |name: &pbps_model::TableName| BindingRecord {
        object: relation(name),
        ownership: ObjectOwnership::Surface(Surface::Table(name.clone())),
        bindings: vec![],
    };
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::DropTable {
                uid: dropped_uid,
                name: target.clone(),
                detach_from: None,
            }),
            PlannedChange::new(Change::RenameTable {
                uid: renamed_uid,
                from: other.clone(),
                to: target.clone(),
                defaults: vec![],
            }),
        ],
    };
    let transitions = super::transitions::derive(
        &changes,
        pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        },
        pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        },
        &[owned(&target), owned(&other)],
        &[owned(&target)],
    )
    .unwrap();
    let merged = transitions
        .iter()
        .find(|t| t.surface == Surface::Table(target.clone()))
        .unwrap();
    assert_eq!(
        merged.before,
        BTreeSet::from([relation(&target), relation(&other)]),
        "the dropped table's opening record must not be replaced by the renamed one's"
    );
    assert_eq!(merged.after, BTreeSet::from([relation(&target)]));
}

/// A recorded rename frees a spelling another UID may take. A check dropped
/// from the old table belongs to its opening UID exactly once.
#[test]
fn reused_table_spelling_does_not_duplicate_an_opening_child_inventory() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{CheckConstraint, Column, PlannedChange, Table};
    use pbps_pg::resolver::capture::BindingRecord;
    use std::collections::BTreeSet;

    let old: pbps_model::TableName = "app.a".parse().unwrap();
    let renamed: pbps_model::TableName = "app.b".parse().unwrap();
    let other: pbps_model::TableName = "app.c".parse().unwrap();
    let mut first = Table::default();
    first
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    first.checks.insert(
        "ck".into(),
        CheckConstraint {
            expression: "id > 0".into(),
        },
    );
    let mut second = Table::default();
    second
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    let mut base = Schema::default();
    base.tables.insert(old.clone(), first.clone());
    base.tables.insert(other.clone(), second.clone());
    let mut desired = Schema::default();
    first.checks.clear();
    desired.tables.insert(renamed.clone(), first);
    desired.tables.insert(old.clone(), second);
    let base_ids = ids(&base, &IdsFile::default());
    let first_uid = base_ids.table_uid(&old).unwrap().clone();
    let second_uid = base_ids.table_uid(&other).unwrap().clone();
    let mut desired_ids = base_ids.clone();
    desired_ids.rename_table(&old, &renamed);
    desired_ids.rename_table(&other, &old);
    assert_eq!(desired_ids.table_uid(&renamed), Some(&first_uid));
    assert_eq!(desired_ids.table_uid(&old), Some(&second_uid));
    assert_ne!(first_uid, second_uid);

    let relation = |name: &pbps_model::TableName| ObjectIdentity {
        class: "pg_class".into(),
        name: vec![name.schema.clone(), name.name.clone()],
        signature: vec![],
    };
    let old_relation = relation(&old);
    let other_relation = relation(&other);
    let renamed_relation = relation(&renamed);
    let reused_relation = relation(&old);
    let check = ObjectIdentity {
        class: "pg_constraint".into(),
        name: vec![old.schema.clone(), "ck".into()],
        signature: vec![old_relation.clone()],
    };
    let check_surface = Surface::Check {
        table: old.clone(),
        name: "ck".into(),
    };
    let owned = |object, surface| BindingRecord {
        object,
        ownership: ObjectOwnership::Surface(surface),
        bindings: vec![],
    };
    let opening = vec![
        owned(old_relation.clone(), Surface::Table(old.clone())),
        owned(other_relation.clone(), Surface::Table(other.clone())),
        owned(check.clone(), check_surface.clone()),
    ];
    let compiled = vec![
        owned(renamed_relation.clone(), Surface::Table(renamed.clone())),
        owned(reused_relation.clone(), Surface::Table(old.clone())),
    ];
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::DropCheck {
                table: old.clone(),
                name: "ck".into(),
            }),
            PlannedChange::new(Change::RenameTable {
                uid: first_uid,
                from: old.clone(),
                to: renamed.clone(),
                defaults: vec![],
            }),
            PlannedChange::new(Change::RenameTable {
                uid: second_uid,
                from: other,
                to: old.clone(),
                defaults: vec![],
            }),
        ],
    };
    let transitions = super::transitions::derive(
        &changes,
        pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        },
        pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        },
        &opening,
        &compiled,
    )
    .unwrap();
    let first_table = transitions
        .iter()
        .find(|t| t.surface == Surface::Table(renamed.clone()))
        .unwrap();
    let reused_table = transitions
        .iter()
        .find(|t| t.surface == Surface::Table(old.clone()))
        .unwrap();
    let dropped_check = transitions
        .iter()
        .find(|t| t.surface == check_surface)
        .unwrap();
    assert_eq!(dropped_check.before, BTreeSet::from([check]));
    assert_eq!(first_table.before, BTreeSet::from([old_relation]));
    assert_eq!(reused_table.before, BTreeSet::from([other_relation]));
    assert_eq!(first_table.after, BTreeSet::from([renamed_relation]));
    assert_eq!(reused_table.after, BTreeSet::from([reused_relation]));
    let claimed: Vec<_> = transitions
        .iter()
        .flat_map(|t| t.before.iter().cloned())
        .collect();
    assert_eq!(claimed.len(), claimed.iter().collect::<BTreeSet<_>>().len());
}

/// An ordinary default removal after recorded table/column renames names the
/// final column. Its opening default belongs to the old column UID, and an
/// unrecorded UID must refuse instead of claiming an empty inventory.
#[test]
fn final_coordinate_default_removal_uses_the_recorded_opening_uid() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{Column, PlannedChange, Table};
    use pbps_pg::resolver::capture::BindingRecord;
    use std::collections::BTreeSet;

    let old: pbps_model::TableName = "app.a".parse().unwrap();
    let renamed: pbps_model::TableName = "app.b".parse().unwrap();
    let mut table = Table::default();
    let mut column = Column::new("integer".parse().unwrap());
    column.value_source = pbps_model::ValueSource::Default("1".into());
    table.columns.insert("id".into(), column);
    let mut base = Schema::default();
    base.tables.insert(old.clone(), table.clone());
    let mut desired = Schema::default();
    let mut final_column = table.columns.shift_remove("id").unwrap();
    final_column.value_source = pbps_model::ValueSource::None;
    table.columns.insert("n".into(), final_column);
    desired.tables.insert(renamed.clone(), table);
    let base_ids = ids(&base, &IdsFile::default());
    let table_uid = base_ids.table_uid(&old).unwrap().clone();
    let column_uid = base_ids.column_uid(&old.column("id")).unwrap().clone();
    let mut desired_ids = base_ids.clone();
    desired_ids.rename_table(&old, &renamed);
    desired_ids.columns.get_mut(&column_uid).unwrap().name = "n".into();
    assert_eq!(
        desired_ids.column_uid(&renamed.column("n")),
        Some(&column_uid)
    );

    let relation = ObjectIdentity {
        class: "pg_class".into(),
        name: vec![old.schema.clone(), old.name.clone()],
        signature: vec![],
    };
    let old_column = ObjectIdentity {
        class: "column".into(),
        name: vec!["id".into()],
        signature: vec![relation],
    };
    let default = ObjectIdentity {
        class: "pg_attrdef".into(),
        name: vec!["id_default".into()],
        signature: vec![old_column],
    };
    let opening = vec![BindingRecord {
        object: default.clone(),
        ownership: ObjectOwnership::Surface(Surface::Default(old.column("id"))),
        bindings: vec![],
    }];
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::RenameTable {
                uid: table_uid.clone(),
                from: old.clone(),
                to: renamed.clone(),
                defaults: vec!["id".into()],
            }),
            PlannedChange::new(Change::RenameColumn {
                uid: column_uid.clone(),
                table: renamed.clone(),
                from: "id".into(),
                to: "n".into(),
                table_was: Some(old.clone()),
            }),
            PlannedChange::new(Change::AlterColumnDefault {
                uid: column_uid,
                column: renamed.column("n"),
                from: Some("1".into()),
                to: None,
            }),
        ],
    };
    let derive = |plan: &ChangeSet| {
        super::transitions::derive(
            plan,
            pbps_diff::Side {
                schema: &base,
                ids: &base_ids,
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &desired_ids,
            },
            &opening,
            &[],
        )
    };
    let transitions = derive(&changes).unwrap();
    let removal = transitions
        .iter()
        .find(|t| t.surface == Surface::Default(renamed.column("n")))
        .unwrap();
    assert_eq!(removal.before, BTreeSet::from([default]));
    assert!(removal.after.is_empty());
    let mut wrong_uid = changes.clone();
    let Change::AlterColumnDefault { uid, .. } = &mut wrong_uid.changes[2].change else {
        panic!("the final typed step removes the default");
    };
    *uid = table_uid;
    assert!(derive(&wrong_uid).is_err());
}

// These qualified-record fixtures exercise the producer's actual resolution
// and transition gates. Raw OIDs are not logical addresses: PG18 replaced the
// measured generated attrdef while preserving its table and column.
fn generation_schema(table: &pbps_model::TableName) -> Schema {
    use pbps_model::{Column, Generated, Table};

    let mut definition = Table::default();
    definition
        .columns
        .insert("a".into(), Column::new("integer".parse().unwrap()));
    let mut generated = Column::new("integer".parse().unwrap());
    generated.value_source = pbps_model::ValueSource::Generated(Generated {
        expression: "a * 2 + 1".into(),
        stored: true,
    });
    definition.columns.insert("g".into(), generated);
    let mut ordinary = Column::new("integer".parse().unwrap());
    ordinary.value_source = pbps_model::ValueSource::Default("7".into());
    definition.columns.insert("d".into(), ordinary);
    let mut schema = Schema::default();
    schema.tables.insert(table.clone(), definition);
    schema
}

/// A transition's exact, authority-bearing inventory: what remains once the
/// records that only ride on the touched table (#1466) are set aside.
fn exact(
    transition: &pbps_model::resolver::ObjectTransition,
) -> (
    BTreeSet<pbps_db::resolver::capture::ObjectIdentity>,
    BTreeSet<pbps_db::resolver::capture::ObjectIdentity>,
) {
    let keep = |side: &BTreeSet<_>| {
        side.iter()
            .filter(|object| !transition.references.contains(*object))
            .cloned()
            .collect()
    };
    (keep(&transition.before), keep(&transition.after))
}

/// `records` with `object` binding a call: a surface a candidate could
/// change, which needs its verdict (the fixture's records bind nothing).
fn with_binding(
    records: &[pbps_pg::resolver::capture::BindingRecord],
    object: &pbps_db::resolver::capture::ObjectIdentity,
) -> Vec<pbps_pg::resolver::capture::BindingRecord> {
    records
        .iter()
        .cloned()
        .map(|mut record| {
            if &record.object == object {
                record.bindings.push(pbps_model::resolver::Binding {
                    node: "FuncExpr".into(),
                    path: vec!["expr".into()],
                    target: pbps_db::resolver::capture::ObjectIdentity {
                        class: "pg_proc".into(),
                        name: vec!["app".into(), "f".into()],
                        signature: vec![],
                    },
                });
            }
            record
        })
        .collect()
}

fn generation_objects(
    table: &pbps_model::TableName,
    column: &str,
) -> [pbps_model::resolver::ObjectIdentity; 4] {
    use pbps_model::resolver::ObjectIdentity;

    let relation = ObjectIdentity {
        class: "pg_class".into(),
        name: vec![table.schema.clone(), table.name.clone()],
        signature: vec![],
    };
    let column = ObjectIdentity {
        class: "column".into(),
        name: vec![column.into()],
        signature: vec![relation.clone()],
    };
    let attrdef = ObjectIdentity {
        class: "pg_attrdef".into(),
        name: vec![],
        signature: vec![column.clone()],
    };
    let owner = ObjectIdentity {
        class: "pg_depend".into(),
        name: vec!["i".into()],
        signature: vec![attrdef.clone(), column.clone()],
    };
    let reference = ObjectIdentity {
        class: "pg_depend".into(),
        name: vec!["n".into()],
        signature: vec![
            attrdef.clone(),
            ObjectIdentity {
                class: "column".into(),
                name: vec!["a".into()],
                signature: vec![relation],
            },
        ],
    };
    [column, attrdef, owner, reference]
}

fn generation_records(
    table: &pbps_model::TableName,
    generated_name: &str,
) -> Vec<pbps_pg::resolver::capture::BindingRecord> {
    use pbps_pg::resolver::capture::BindingRecord;

    let owned = |object, surface| BindingRecord {
        object,
        ownership: ObjectOwnership::Surface(surface),
        bindings: vec![],
    };
    let [generated, attrdef, owner, reference] = generation_objects(table, generated_name);
    let relation = generated.signature[0].clone();
    let [source, ..] = generation_objects(table, "a");
    let [ordinary, default, mut automatic, ..] = generation_objects(table, "d");
    automatic.name = vec!["a".into()];
    let surface = Surface::Default(table.column(generated_name));
    vec![
        owned(relation, Surface::Table(table.clone())),
        owned(source, Surface::Column(table.column("a"))),
        owned(generated, Surface::Column(table.column(generated_name))),
        owned(ordinary, Surface::Column(table.column("d"))),
        owned(attrdef, surface.clone()),
        owned(owner, surface.clone()),
        owned(reference, surface),
        owned(default, Surface::Default(table.column("d"))),
        owned(automatic, Surface::Default(table.column("d"))),
    ]
}

#[test]
fn generated_expression_transitions_keep_exact_inventory_and_recorded_rename_identity() {
    use pbps_model::PlannedChange;

    let old = pbps_model::TableName::new("app", "t");
    for rename in [false, true] {
        let final_table = if rename {
            pbps_model::TableName::new("app", "renamed")
        } else {
            old.clone()
        };
        let final_name = if rename { "n" } else { "g" };
        let base = generation_schema(&old);
        let base_ids = ids(&base, &IdsFile::default());
        let table_uid = base_ids.table_uid(&old).unwrap().clone();
        let column_uid = base_ids.column_uid(&old.column("g")).unwrap().clone();
        let source_uid = base_ids.column_uid(&old.column("a")).unwrap().clone();
        let mut desired = base.clone();
        let mut definition = desired.tables.remove(&old).unwrap();
        let mut column = definition.columns.shift_remove("g").unwrap();
        let pbps_model::ValueSource::Generated(generated) = &mut column.value_source else {
            panic!("generated fixture")
        };
        generated.expression = "a * 3".into();
        definition.columns.insert(final_name.into(), column);
        desired.tables.insert(final_table.clone(), definition);
        let mut desired_ids = base_ids.clone();
        desired_ids.rename_table(&old, &final_table);
        desired_ids.columns.get_mut(&column_uid).unwrap().name = final_name.into();
        let opening = generation_records(&old, "g");
        let compiled = generation_records(&final_table, final_name);
        let mut changes = ChangeSet::default();
        if rename {
            changes.changes.extend([
                PlannedChange::new(Change::RenameTable {
                    uid: table_uid.clone(),
                    from: old.clone(),
                    to: final_table.clone(),
                    defaults: vec!["d".into()],
                }),
                PlannedChange::new(Change::RenameColumn {
                    uid: column_uid.clone(),
                    table: final_table.clone(),
                    from: "g".into(),
                    to: final_name.into(),
                    table_was: Some(old.clone()),
                }),
            ]);
        }
        changes
            .changes
            .push(PlannedChange::new(Change::AlterColumnExpression {
                uid: column_uid.clone(),
                column: final_table.column(final_name),
                from: "a * 2 + 1".into(),
                to: "a * 3".into(),
            }));
        let derive = |changes: &ChangeSet, opening_ids: &IdsFile| {
            super::transitions::derive(
                changes,
                pbps_diff::Side {
                    schema: &base,
                    ids: opening_ids,
                },
                pbps_diff::Side {
                    schema: &desired,
                    ids: &desired_ids,
                },
                &opening,
                &compiled,
            )
        };
        let transitions = derive(&changes, &base_ids).unwrap();
        let expression = transitions
            .iter()
            .find(|transition| {
                transition.surface == Surface::Default(final_table.column(final_name))
            })
            .expect("SET EXPRESSION has its own exact attrdef transition");
        let [old_column, before, old_owner, old_reference] = generation_objects(&old, "g");
        let [new_column, after, new_owner, new_reference] =
            generation_objects(&final_table, final_name);
        assert_eq!(
            exact(expression).0,
            BTreeSet::from([before, old_owner, old_reference])
        );
        assert_eq!(
            exact(expression).1,
            BTreeSet::from([after, new_owner, new_reference])
        );
        assert!(!exact(expression).0.contains(&old_column));
        assert!(!exact(expression).1.contains(&new_column));
        for opening in [true, false] {
            let claimed: Vec<_> = transitions
                .iter()
                .flat_map(|transition| {
                    if opening {
                        &transition.before
                    } else {
                        &transition.after
                    }
                })
                .collect();
            assert_eq!(claimed.len(), claimed.iter().collect::<BTreeSet<_>>().len());
        }

        let mut missing = base_ids.clone();
        missing.columns.remove(&column_uid);
        assert!(matches!(derive(&changes, &missing), Err(Error::Binding(_))));
        // A valid UID for a different recorded column is also wrong authority.
        // It must not select that input column's empty attrdef inventory.
        for wrong in [table_uid, source_uid] {
            let mut invalid = changes.clone();
            let Change::AlterColumnExpression { uid, .. } =
                &mut invalid.changes.last_mut().unwrap().change
            else {
                unreachable!()
            };
            *uid = wrong;
            assert!(matches!(
                derive(&invalid, &base_ids),
                Err(Error::Binding(_))
            ));
        }
    }
}

#[test]
fn create_and_add_stored_generation_keep_the_attrdef_child_inventory() {
    use pbps_model::PlannedChange;

    let table = pbps_model::TableName::new("app", "t");
    let desired = generation_schema(&table);
    let compiled = generation_records(&table, "g");
    for create in [true, false] {
        let mut base = Schema::default();
        if !create {
            base = desired.clone();
            base.tables
                .get_mut(&table)
                .unwrap()
                .columns
                .shift_remove("g");
        }
        let base_ids = ids(&base, &IdsFile::default());
        let desired_ids = ids(&desired, &base_ids);
        // ADD preserves a live parent on both sides. CREATE has no opening
        // parent; neither premise changes the generated child's inventory.
        let opening = if create {
            vec![]
        } else {
            generation_records(&table, "g")
                .into_iter()
                .filter(|record| {
                    !matches!(
                        &record.ownership,
                        ObjectOwnership::Surface(Surface::Column(c) | Surface::Default(c))
                            if c.name == "g".into()
                    )
                })
                .collect()
        };
        let (surface, change, expected) = if create {
            (
                Surface::Table(table.clone()),
                Change::CreateTable {
                    uid: desired_ids.table_uid(&table).unwrap().clone(),
                    name: table.clone(),
                    table: Box::new(desired.tables[&table].clone()),
                    beside_default: None,
                    moves_at_most: None,
                },
                compiled
                    .iter()
                    .map(|record| record.object.clone())
                    .collect(),
            )
        } else {
            let [column, attrdef, owner, reference] = generation_objects(&table, "g");
            (
                Surface::Column(table.column("g")),
                Change::AddColumn {
                    uid: desired_ids.column_uid(&table.column("g")).unwrap().clone(),
                    table: table.clone(),
                    name: "g".into(),
                    column: Box::new(desired.tables[&table].columns["g"].clone()),
                },
                BTreeSet::from([column, attrdef, owner, reference]),
            )
        };
        let transitions = super::transitions::derive(
            &ChangeSet {
                changes: vec![PlannedChange::new(change)],
            },
            pbps_diff::Side {
                schema: &base,
                ids: &base_ids,
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &desired_ids,
            },
            &opening,
            &compiled,
        )
        .unwrap();
        assert_eq!(transitions.len(), if create { 1 } else { 2 });
        let child = transitions.iter().find(|t| t.surface == surface).unwrap();
        assert!(exact(child).0.is_empty());
        assert_eq!(exact(child).1, expected);
        if !create {
            let parent = transitions
                .iter()
                .find(|t| t.surface == Surface::Table(table.clone()))
                .unwrap();
            let relation = generation_objects(&table, "g")[0].signature[0].clone();
            assert_eq!(exact(parent).0, BTreeSet::from([relation.clone()]));
            assert_eq!(exact(parent).1, BTreeSet::from([relation]));
        }
    }
}

#[test]
fn generated_creation_surfaces_require_unique_qualified_attrdef_records() {
    use pbps_pg::resolver::capture::Assessment;

    let table = pbps_model::TableName::new("app", "t");
    let desired = generation_schema(&table);
    let compiled = generation_records(&table, "g");
    let [_, generated, ..] = generation_objects(&table, "g");
    let [_, ordinary, ..] = generation_objects(&table, "d");
    for create in [true, false] {
        let mut base = Schema::default();
        let mut opening = Vec::new();
        let mut assessment = Assessment::default();
        if !create {
            base = desired.clone();
            base.tables
                .get_mut(&table)
                .unwrap()
                .columns
                .shift_remove("g");
            let new_objects = BTreeSet::from(generation_objects(&table, "g"));
            opening = compiled
                .iter()
                .filter(|record| !new_objects.contains(&record.object))
                .cloned()
                .collect();
            assessment.surfaces.insert(
                ordinary.clone(),
                pbps_pg::resolver::capture::Verdict::Unaffected,
            );
        } else {
            // A surface the target does not hold yet carries the candidate
            // check's verdict, as the assessment gives it.
            assessment.surfaces.insert(
                ordinary.clone(),
                pbps_pg::resolver::capture::Verdict::Created,
            );
        }
        assessment.surfaces.insert(
            generated.clone(),
            pbps_pg::resolver::capture::Verdict::Created,
        );
        let resolutions =
            super::resolution::from_records(&base, &desired, &opening, &compiled, &assessment)
                .unwrap();
        assert_eq!(resolutions.len(), 2);
        for (name, object) in [("g", &generated), ("d", &ordinary)] {
            let resolved = resolutions
                .iter()
                .find(|resolved| resolved.surface == Surface::Default(table.column(name)))
                .expect("ordinary defaults and generated expressions both require an attrdef");
            assert_eq!(resolved.current.is_some(), !create && name == "d");
            assert_eq!(&resolved.desired.as_ref().unwrap().object, object);
        }
    }

    for name in ["g", "d"] {
        let [_, attrdef, ..] = generation_objects(&table, name);
        for defect in ["missing", "duplicate", "unqualified", "reference-only"] {
            let mut records = compiled.clone();
            let index = records
                .iter()
                .position(|record| record.object == attrdef)
                .unwrap();
            match defect {
                "missing" => {
                    records.remove(index);
                }
                "duplicate" => {
                    records.push(records[index].clone());
                }
                "unqualified" => records[index].ownership = ObjectOwnership::Unqualified,
                "reference-only" => {
                    records[index].ownership =
                        ObjectOwnership::Surface(Surface::Column(table.column("a")));
                }
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    super::resolution::from_records(
                        &Schema::default(),
                        &desired,
                        &[],
                        &records,
                        &Assessment::default(),
                    ),
                    Err(Error::Binding(_))
                ),
                "{name}: {defect} must refuse instead of omitting the required surface"
            );
        }
    }
}

#[test]
fn existing_generated_surfaces_require_both_records_and_a_resolved_binding_verdict() {
    use pbps_pg::resolver::capture::{Assessment, Verdict};

    let table = pbps_model::TableName::new("app", "t");
    let schema = generation_schema(&table);
    let records = generation_records(&table, "g");
    let [_, generated, ..] = generation_objects(&table, "g");
    let [_, ordinary, ..] = generation_objects(&table, "d");
    let assessment = |verdict| Assessment {
        surfaces: BTreeMap::from([
            (generated.clone(), verdict),
            (ordinary.clone(), Verdict::Unaffected),
        ]),
        ..Assessment::default()
    };
    for verdict in [Verdict::Unaffected, Verdict::Rebuild] {
        let resolutions = super::resolution::from_records(
            &schema,
            &schema,
            &records,
            &records,
            &assessment(verdict),
        )
        .unwrap();
        assert_eq!(resolutions.len(), 2);
        assert!(
            resolutions
                .iter()
                .all(|resolved| { resolved.current.is_some() && resolved.desired.is_some() })
        );
    }
    let mut no_verdict = assessment(Verdict::Unaffected);
    no_verdict.surfaces.remove(&generated);
    for verdict in [
        no_verdict,
        assessment(Verdict::Unresolved {
            condition: "test binding is unqualified".into(),
        }),
    ] {
        let bound = with_binding(&records, &generated);
        assert!(matches!(
            super::resolution::from_records(&schema, &schema, &bound, &bound, &verdict),
            Err(Error::Binding(_))
        ));
    }
    let missing: Vec<_> = records
        .iter()
        .filter(|record| record.object != generated)
        .cloned()
        .collect();
    for opening in [true, false] {
        let (before, after) = if opening {
            (&missing, &records)
        } else {
            (&records, &missing)
        };
        assert!(matches!(
            super::resolution::from_records(
                &schema,
                &schema,
                before,
                after,
                &assessment(Verdict::Unaffected),
            ),
            Err(Error::Binding(_))
        ));
    }
}

/// A surface the plan creates has no target record to compare, but its
/// creation binds against the target's candidates, so it needs the
/// candidate check's verdict too. `Created` is accepted only where the target
/// does not hold the surface; an unresolved or absent verdict refuses, and
/// `Created` for a surface both sides hold is no comparison at all.
#[test]
fn a_created_surface_requires_its_candidate_verdict() {
    use pbps_pg::resolver::capture::{Assessment, Verdict};

    let table = pbps_model::TableName::new("app", "t");
    let schema = generation_schema(&table);
    let records = generation_records(&table, "g");
    let [_, generated, ..] = generation_objects(&table, "g");
    let [_, ordinary, ..] = generation_objects(&table, "d");
    let assessment = |verdict: Option<Verdict>| Assessment {
        surfaces: [(ordinary.clone(), Verdict::Created)]
            .into_iter()
            .chain(verdict.map(|verdict| (generated.clone(), verdict)))
            .collect(),
        ..Assessment::default()
    };
    let empty = Schema::default();
    let created = super::resolution::from_records(
        &empty,
        &schema,
        &[],
        &records,
        &assessment(Some(Verdict::Created)),
    )
    .unwrap();
    assert!(
        created
            .iter()
            .all(|resolved| resolved.current.is_none() && resolved.desired.is_some())
    );
    for verdict in [
        None,
        Some(Verdict::Unresolved {
            condition: "a same-named candidate on the target was not reconstructed on scratch"
                .into(),
        }),
    ] {
        let bound = with_binding(&records, &generated);
        assert!(matches!(
            super::resolution::from_records(&empty, &schema, &[], &bound, &assessment(verdict)),
            Err(Error::Binding(_))
        ));
    }
    assert!(matches!(
        super::resolution::from_records(
            &schema,
            &schema,
            &records,
            &records,
            &assessment(Some(Verdict::Created)),
        ),
        Err(Error::Binding(_))
    ));
}

/// A surface that binds nothing at creation, such as a routine whose string
/// body binds only at run time, has no lookup a candidate could change, so
/// the assessment gives it no verdict and resolution needs none: created or
/// kept. One that binds something still needs its verdict.
#[test]
fn a_surface_binding_nothing_at_creation_needs_no_verdict() {
    use pbps_pg::resolver::capture::{Assessment, Verdict};

    let table = pbps_model::TableName::new("app", "t");
    let schema = generation_schema(&table);
    let records = generation_records(&table, "g");
    let [_, generated, ..] = generation_objects(&table, "g");
    let [_, ordinary, ..] = generation_objects(&table, "d");
    let bindingless = records.clone();
    let binding = with_binding(&records, &generated);
    let empty = Schema::default();
    for (base, opening, verdict) in [
        (&empty, &[][..], Verdict::Created),
        (&schema, &bindingless[..], Verdict::Unaffected),
    ] {
        let assessment = Assessment {
            surfaces: BTreeMap::from([(ordinary.clone(), verdict)]),
            ..Assessment::default()
        };
        super::resolution::from_records(base, &schema, opening, &bindingless, &assessment).unwrap();
        assert!(matches!(
            super::resolution::from_records(base, &schema, opening, &binding, &assessment),
            Err(Error::Binding(_))
        ));
    }
}

/// A plan that renames a table and its column, then removes the column's
/// default and the table's check, names the removals by the new spelling,
/// while the opening capture holds them under the old one. Each removed
/// surface is resolved by its opening record and named as its removal names
/// it, through the recorded UIDs; a surface the plan keeps is not moved.
#[test]
fn a_surface_removed_after_its_rename_is_named_as_its_removal_names_it() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{CheckConstraint, Column, Table};
    use pbps_pg::resolver::capture::{Assessment, BindingRecord};

    let old: pbps_model::TableName = "app.t".parse().unwrap();
    let new: pbps_model::TableName = "app.u".parse().unwrap();
    let mut table = Table::default();
    let mut id = Column::new("integer".parse().unwrap());
    id.value_source = pbps_model::ValueSource::Default("0".into());
    table.columns.insert("id".into(), id);
    table.checks.insert(
        "c".into(),
        CheckConstraint {
            expression: "id > 0".into(),
        },
    );
    let mut base = Schema::default();
    base.tables.insert(old.clone(), table);
    let base_ids = ids(&base, &IdsFile::default());
    let mut bare = Table::default();
    bare.columns
        .insert("n".into(), Column::new("integer".parse().unwrap()));
    let mut desired = Schema::default();
    desired.tables.insert(new.clone(), bare);
    let mut desired_ids = base_ids.clone();
    desired_ids.rename_table(&old, &new);
    let uid = base_ids.column_uid(&old.column("id")).unwrap();
    desired_ids.columns.get_mut(uid).unwrap().name = "n".into();
    let relation = ObjectIdentity {
        class: "pg_class".into(),
        name: vec!["app".into(), "t".into()],
        signature: vec![],
    };
    let record = |class: &str, name: &str, surface: Surface| BindingRecord {
        object: ObjectIdentity {
            class: class.into(),
            name: vec![name.into()],
            signature: vec![relation.clone()],
        },
        ownership: ObjectOwnership::Surface(surface),
        bindings: vec![],
    };
    let check = Surface::Check {
        table: old.clone(),
        name: "c".into(),
    };
    let default = Surface::Default(old.column("id"));
    let opening = vec![
        record("pg_constraint", "c", check.clone()),
        record("pg_attrdef", "id", default.clone()),
    ];
    let resolutions = super::resolution::from_sides(
        pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        },
        pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        },
        &opening,
        &[],
        &Assessment::default(),
    )
    .unwrap();
    let named: Vec<_> = resolutions.iter().map(|r| r.surface.clone()).collect();
    assert_eq!(
        named,
        [
            Surface::Default(new.column("n")),
            Surface::Check {
                table: new.clone(),
                name: "c".into(),
            },
        ]
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
    );
    assert!(
        resolutions
            .iter()
            .all(|r| r.current.is_some() && r.desired.is_none())
    );
    // Without the recorded identities the removals keep the old spelling,
    // which the removal changes do not name.
    let unmapped =
        super::resolution::from_records(&base, &desired, &opening, &[], &Assessment::default())
            .unwrap();
    assert!(unmapped.iter().any(|r| r.surface == check));
}

/// A plan that drops `app.a` and renames `app.b` to `app.a`, removing each
/// table's check `c`, would name both removals `app.a.c` after the renames.
/// The dropped table's check holds that spelling, so the renamed table's
/// keeps its opening one, and neither removal is lost or refused.
#[test]
fn a_reused_name_keeps_two_removed_surfaces_distinct() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{CheckConstraint, Column, Table};
    use pbps_pg::resolver::capture::{Assessment, BindingRecord};

    let a: pbps_model::TableName = "app.a".parse().unwrap();
    let b: pbps_model::TableName = "app.b".parse().unwrap();
    let checked = || {
        let mut table = Table::default();
        table
            .columns
            .insert("n".into(), Column::new("integer".parse().unwrap()));
        table.checks.insert(
            "c".into(),
            CheckConstraint {
                expression: "n > 0".into(),
            },
        );
        table
    };
    let mut base = Schema::default();
    base.tables.insert(a.clone(), checked());
    base.tables.insert(b.clone(), checked());
    let base_ids = ids(&base, &IdsFile::default());
    let dropped = base_ids.table_uid(&a).unwrap().clone();
    let mut bare = Table::default();
    bare.columns
        .insert("n".into(), Column::new("integer".parse().unwrap()));
    let mut desired = Schema::default();
    desired.tables.insert(a.clone(), bare);
    let mut desired_ids = base_ids.clone();
    desired_ids.tables.remove(&dropped);
    desired_ids.columns.retain(|_, column| column.table != a);
    desired_ids.rename_table(&b, &a);
    let check = |table: &pbps_model::TableName| BindingRecord {
        object: ObjectIdentity {
            class: "pg_constraint".into(),
            name: vec!["c".into()],
            signature: vec![ObjectIdentity {
                class: "pg_class".into(),
                name: vec!["app".into(), table.name.clone()],
                signature: vec![],
            }],
        },
        ownership: ObjectOwnership::Surface(Surface::Check {
            table: table.clone(),
            name: "c".into(),
        }),
        bindings: vec![],
    };
    let resolutions = super::resolution::from_sides(
        pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        },
        pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        },
        &[check(&a), check(&b)],
        &[],
        &Assessment::default(),
    )
    .expect("two removals keep two spellings");
    let named: BTreeSet<_> = resolutions.iter().map(|r| r.surface.clone()).collect();
    assert_eq!(
        named,
        BTreeSet::from([
            Surface::Check {
                table: a.clone(),
                name: "c".into(),
            },
            Surface::Check {
                table: b.clone(),
                name: "c".into(),
            },
        ])
    );
    assert!(
        resolutions
            .iter()
            .all(|r| r.current.is_some() && r.desired.is_none())
    );
}

mod column_vector_parent {
    use super::*;
    use pbps_model::{Column, Generated, PlannedChange, TableName};
    use pbps_pg::resolver::capture::BindingRecord;

    fn derive(
        changes: &ChangeSet,
        base: &Schema,
        base_ids: &IdsFile,
        desired: &Schema,
        desired_ids: &IdsFile,
        opening: &[BindingRecord],
        compiled: &[BindingRecord],
    ) -> Vec<pbps_model::resolver::ObjectTransition> {
        super::super::transitions::derive(
            changes,
            pbps_diff::Side {
                schema: base,
                ids: base_ids,
            },
            pbps_diff::Side {
                schema: desired,
                ids: desired_ids,
            },
            opening,
            compiled,
        )
        .unwrap()
    }

    fn without_generated(table: &TableName, name: &str) -> Vec<BindingRecord> {
        generation_records(table, "g")
            .into_iter()
            .filter(|record| {
                !matches!(
                    &record.ownership,
                    ObjectOwnership::Surface(Surface::Column(c) | Surface::Default(c))
                        if c.name == name.into()
                )
            })
            .collect()
    }

    #[test]
    fn vector_changes_account_for_the_exact_parent_and_only_the_changed_column() {
        let table = TableName::new("app", "t");
        let base = generation_schema(&table);
        let base_ids = ids(&base, &IdsFile::default());
        let opening = generation_records(&table, "g");
        let relation = generation_objects(&table, "g")[0].signature[0].clone();
        for kind in ["plain", "default", "generated", "drop", "rename"] {
            let mut desired = base.clone();
            let (change, surface, before, after, compiled, desired_ids) = match kind {
                "drop" => {
                    desired
                        .tables
                        .get_mut(&table)
                        .unwrap()
                        .columns
                        .shift_remove("g");
                    let uid = base_ids.column_uid(&table.column("g")).unwrap().clone();
                    let mut desired_ids = base_ids.clone();
                    desired_ids.columns.remove(&uid);
                    let objects = generation_objects(&table, "g");
                    (
                        Change::DropColumn {
                            uid,
                            column: table.column("g"),
                        },
                        Surface::Column(table.column("g")),
                        BTreeSet::from(objects),
                        BTreeSet::new(),
                        without_generated(&table, "g"),
                        desired_ids,
                    )
                }
                "rename" => {
                    let definition = desired.tables.get_mut(&table).unwrap();
                    let column = definition.columns.shift_remove("g").unwrap();
                    definition.columns.insert("h".into(), column);
                    let uid = base_ids.column_uid(&table.column("g")).unwrap().clone();
                    let mut desired_ids = base_ids.clone();
                    desired_ids.columns.get_mut(&uid).unwrap().name = "h".into();
                    (
                        Change::RenameColumn {
                            uid,
                            table: table.clone(),
                            from: "g".into(),
                            to: "h".into(),
                            table_was: None,
                        },
                        Surface::Column(table.column("h")),
                        BTreeSet::from(generation_objects(&table, "g")),
                        BTreeSet::from(generation_objects(&table, "h")),
                        generation_records(&table, "h"),
                        desired_ids,
                    )
                }
                _ => {
                    let mut column = Column::new("integer".parse().unwrap());
                    if kind == "default" {
                        column.value_source = pbps_model::ValueSource::Default("7".into());
                    } else if kind == "generated" {
                        column.value_source = pbps_model::ValueSource::Generated(Generated {
                            expression: "a + 4".into(),
                            stored: true,
                        });
                    }
                    desired
                        .tables
                        .get_mut(&table)
                        .unwrap()
                        .columns
                        .insert("h".into(), column.clone());
                    let desired_ids = ids(&desired, &base_ids);
                    let [child, attrdef, mut owner, reference] = generation_objects(&table, "h");
                    let mut additions = vec![BindingRecord {
                        object: child.clone(),
                        ownership: ObjectOwnership::Surface(Surface::Column(table.column("h"))),
                        bindings: vec![],
                    }];
                    if kind != "plain" {
                        if kind == "default" {
                            owner.name = vec!["a".into()];
                        }
                        for object in [attrdef, owner] {
                            additions.push(BindingRecord {
                                object,
                                ownership: ObjectOwnership::Surface(Surface::Default(
                                    table.column("h"),
                                )),
                                bindings: vec![],
                            });
                        }
                        if kind == "generated" {
                            additions.push(BindingRecord {
                                object: reference,
                                ownership: ObjectOwnership::Surface(Surface::Default(
                                    table.column("h"),
                                )),
                                bindings: vec![],
                            });
                        }
                    }
                    let after = additions.iter().map(|r| r.object.clone()).collect();
                    let mut compiled = opening.clone();
                    compiled.extend(additions);
                    (
                        Change::AddColumn {
                            uid: desired_ids.column_uid(&table.column("h")).unwrap().clone(),
                            table: table.clone(),
                            name: "h".into(),
                            column: Box::new(column),
                        },
                        Surface::Column(table.column("h")),
                        BTreeSet::new(),
                        after,
                        compiled,
                        desired_ids,
                    )
                }
            };
            let transitions = derive(
                &ChangeSet {
                    changes: vec![PlannedChange::new(change)],
                },
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                &opening,
                &compiled,
            );
            assert_eq!(transitions.len(), 2, "{kind}");
            let parent = transitions
                .iter()
                .find(|t| t.surface == Surface::Table(table.clone()))
                .unwrap();
            assert_eq!(
                exact(parent).0,
                BTreeSet::from([relation.clone()]),
                "{kind}"
            );
            assert_eq!(
                exact(parent).1,
                BTreeSet::from([relation.clone()]),
                "{kind}"
            );
            let column = transitions.iter().find(|t| t.surface == surface).unwrap();
            assert_eq!(exact(column).0, before, "{kind}");
            assert_eq!(exact(column).1, after, "{kind}");
            for inventory in [
                transitions
                    .iter()
                    .flat_map(|t| &t.before)
                    .collect::<Vec<_>>(),
                transitions
                    .iter()
                    .flat_map(|t| &t.after)
                    .collect::<Vec<_>>(),
            ] {
                assert_eq!(
                    inventory.len(),
                    inventory.iter().collect::<BTreeSet<_>>().len(),
                    "{kind}"
                );
            }
        }
    }

    #[test]
    fn vector_parent_endpoints_follow_table_uids_when_an_opening_name_is_reused() {
        let old = TableName::new("app", "a");
        let renamed = TableName::new("app", "b");
        let other = TableName::new("app", "c");
        let mut base = generation_schema(&old);
        base.tables.extend(generation_schema(&other).tables);
        let base_ids = ids(&base, &IdsFile::default());
        let mut desired = Schema::default();
        let mut first = base.tables[&old].clone();
        first.columns.shift_remove("g");
        desired.tables.insert(renamed.clone(), first);
        desired
            .tables
            .insert(old.clone(), base.tables[&other].clone());
        let mut desired_ids = base_ids.clone();
        desired_ids.rename_table(&old, &renamed);
        desired_ids.rename_table(&other, &old);
        desired_ids
            .columns
            .remove(base_ids.column_uid(&old.column("g")).unwrap());
        let changes = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::DropColumn {
                    uid: base_ids.column_uid(&old.column("g")).unwrap().clone(),
                    column: old.column("g"),
                }),
                PlannedChange::new(Change::RenameTable {
                    uid: base_ids.table_uid(&old).unwrap().clone(),
                    from: old.clone(),
                    to: renamed.clone(),
                    defaults: vec!["d".into()],
                }),
                PlannedChange::new(Change::RenameTable {
                    uid: base_ids.table_uid(&other).unwrap().clone(),
                    from: other.clone(),
                    to: old.clone(),
                    defaults: vec!["d".into(), "g".into()],
                }),
            ],
        };
        let mut opening = generation_records(&old, "g");
        opening.extend(generation_records(&other, "g"));
        let mut compiled = without_generated(&renamed, "g");
        compiled.extend(generation_records(&old, "g"));
        let transitions = derive(
            &changes,
            &base,
            &base_ids,
            &desired,
            &desired_ids,
            &opening,
            &compiled,
        );
        let dropped = transitions
            .iter()
            .find(|t| t.surface == Surface::Column(old.column("g")))
            .unwrap();
        assert_eq!(
            dropped.before,
            BTreeSet::from(generation_objects(&old, "g"))
        );
        assert!(dropped.after.is_empty());
        for (final_table, opening_table) in [(&renamed, &old), (&old, &other)] {
            let parent = transitions
                .iter()
                .find(|t| t.surface == Surface::Table(final_table.clone()))
                .unwrap();
            let relation = generation_objects(opening_table, "g")[0].signature[0].clone();
            assert!(parent.before.contains(&relation));
            let wrong = generation_objects(if opening_table == &old { &other } else { &old }, "g")
                [0]
            .signature[0]
                .clone();
            assert!(!parent.before.contains(&wrong));
        }
        for (actual, expected) in [
            (
                transitions
                    .iter()
                    .flat_map(|t| &t.before)
                    .collect::<Vec<_>>(),
                &opening,
            ),
            (
                transitions
                    .iter()
                    .flat_map(|t| &t.after)
                    .collect::<Vec<_>>(),
                &compiled,
            ),
        ] {
            let unique = actual.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(actual.len(), unique.len());
            assert_eq!(unique, expected.iter().map(|r| &r.object).collect());
        }
    }

    #[test]
    fn nonvector_column_edits_do_not_acquire_parent_inventory() {
        let table = TableName::new("app", "t");
        let base = generation_schema(&table);
        let ids = ids(&base, &IdsFile::default());
        let uid = ids.column_uid(&table.column("g")).unwrap().clone();
        let records = generation_records(&table, "g");
        for change in [
            Change::AlterColumnType {
                uid: uid.clone(),
                column: table.column("g"),
                from: "integer".parse().unwrap(),
                to: "bigint".parse().unwrap(),
                from_nullable: true,
                to_nullable: true,
                through_external_partitions: false,
                from_collation: None,
                to_collation: None,
                using: None,
            },
            Change::AlterColumnNullability {
                uid: uid.clone(),
                column: table.column("g"),
                ty: "integer".parse().unwrap(),
                to_nullable: false,
                through_external_partitions: false,
                collation: None,
            },
            Change::AlterColumnExpression {
                uid,
                column: table.column("g"),
                from: "a * 2 + 1".into(),
                to: "a * 3".into(),
            },
            Change::AlterColumnDefault {
                uid: ids.column_uid(&table.column("d")).unwrap().clone(),
                column: table.column("d"),
                from: Some("7".into()),
                to: Some("8".into()),
            },
        ] {
            let changes = ChangeSet {
                changes: vec![PlannedChange::new(change)],
            };
            let transitions = derive(&changes, &base, &ids, &base, &ids, &records, &records);
            assert_eq!(transitions.len(), 1);
            assert!(!matches!(transitions[0].surface, Surface::Table(_)));
            let relation = generation_objects(&table, "g")[0].signature[0].clone();
            assert!(!exact(&transitions[0]).0.contains(&relation));
            assert!(!exact(&transitions[0]).1.contains(&relation));
            // The table still rides as a reference: the edit touches it.
            assert!(transitions[0].references.contains(&relation));
        }
    }
}

/// A partition's own default is a surface the qualified catalog must
/// account for, as a column's is (#1578 review): with no record of it the
/// resolution is refused, never reported as nothing to resolve. Capturing
/// it is #1587. A partition with no default of its own needs no record.
#[test]
fn a_partitions_own_default_needs_its_catalog_record() {
    use pbps_pg::resolver::capture::Assessment;
    let tree = |own: bool| {
        let mut schema = pbps_model::Schema::default();
        let mut parent = pbps_model::Table::default();
        parent.columns.insert(
            "id".into(),
            pbps_model::Column::new("integer".parse().unwrap()),
        );
        parent.partition_by = Some(pbps_model::PartitionBy {
            external: false,
            columns: vec!["id".into()],
            strategy: Default::default(),
        });
        schema.tables.insert("app.ev".parse().unwrap(), parent);
        let partition = pbps_model::Table {
            partition_of: Some(pbps_model::PartitionOf {
                parent: "app.ev".parse().unwrap(),
                bound: pbps_model::PartitionBound::Default,
                columns: own
                    .then(|| {
                        (
                            "id".into(),
                            pbps_model::PartitionColumn {
                                default: Some("app.f(1)".into()),
                                not_null: false,
                            },
                        )
                    })
                    .into_iter()
                    .collect(),
            }),
            ..pbps_model::Table::default()
        };
        schema.tables.insert("app.ev_1".parse().unwrap(), partition);
        schema
    };
    let empty = pbps_model::Schema::default();
    let refused =
        super::resolution::from_records(&empty, &tree(true), &[], &[], &Assessment::default())
            .expect_err("an own default with no record");
    let said = refused.to_string();
    assert!(
        said.contains("no creation-time record") && said.contains("\"ev_1\""),
        "{said}"
    );
    let resolved =
        super::resolution::from_records(&empty, &tree(false), &[], &[], &Assessment::default())
            .unwrap();
    assert!(resolved.is_empty(), "{resolved:?}");
}

#[test]
fn a_generated_placeholder_created_in_the_plan_has_no_opening_inventory() {
    use pbps_model::PlannedChange;
    let name = pbps_model::TableName::new("app", "t");
    let desired = generation_schema(&name);
    let recorded = ids(&desired, &IdsFile::default());
    let compiled = generation_records(&name, "g");
    let mut placeholder = desired.tables[&name].clone();
    let pbps_model::ValueSource::Generated(g) =
        &mut placeholder.columns.get_mut("g").unwrap().value_source
    else {
        unreachable!()
    };
    let to = std::mem::replace(&mut g.expression, "NULL::integer".into());
    let create = PlannedChange::new(Change::CreateTable {
        uid: recorded.table_uid(&name).unwrap().clone(),
        name: name.clone(),
        table: Box::new(placeholder),
        beside_default: None,
        moves_at_most: None,
    });
    let install = PlannedChange::new(Change::AlterColumnExpression {
        uid: recorded.column_uid(&name.column("g")).unwrap().clone(),
        column: name.column("g"),
        from: "NULL::integer".into(),
        to,
    });
    let empty = Schema::default();
    let no_ids = IdsFile::default();
    let derive = |changes: &ChangeSet| {
        super::transitions::derive(
            changes,
            pbps_diff::Side {
                schema: &empty,
                ids: &no_ids,
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &recorded,
            },
            &[],
            &compiled,
        )
    };
    let cs = ChangeSet {
        changes: vec![create.clone(), install.clone()],
    };
    let transitions = derive(&cs).unwrap();
    let generated = transitions
        .iter()
        .find(|t| t.surface == Surface::Default(name.column("g")))
        .unwrap();
    assert!(generated.before.is_empty());
    assert!(!generated.after.is_empty());
    // An absent opening UID is not authority unless the plan itself creates
    // that exact generated column first, with the stated placeholder.
    for changes in [vec![install.clone()], vec![install.clone(), create.clone()]] {
        assert!(derive(&ChangeSet { changes }).is_err());
    }
    let mut wrong = cs;
    let Change::AlterColumnExpression { from, .. } = &mut wrong.changes[1].change else {
        unreachable!()
    };
    *from = "0".into();
    assert!(derive(&wrong).is_err());
}

/// A declared type is one surface of its own (#2407): its creation installs
/// the records it owns, its drop removes the opening ones, a label operation
/// replaces them in place, and a rename carries the opening spelling's
/// inventory to the final one. Another type's records and a type the
/// desired schema no longer declares are not claimed.
#[test]
fn a_declared_types_transition_inventories_the_records_it_owns() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{ManagedType, PlannedChange, TypeRef};
    use pbps_pg::resolver::capture::BindingRecord;

    let mood: TypeRef = "app.mood".parse().unwrap();
    let old: TypeRef = "app.old".parse().unwrap();
    let other: TypeRef = "app.other".parse().unwrap();
    let definition = || ManagedType::Enum {
        values: vec!["a".into()],
    };
    let record = |name: &str, owner: &TypeRef| {
        let object = ObjectIdentity {
            class: "pg_type".into(),
            name: vec!["app".into(), name.into()],
            signature: vec![],
        };
        (
            object.clone(),
            BindingRecord {
                object,
                ownership: ObjectOwnership::Surface(Surface::Type(owner.clone())),
                bindings: vec![],
            },
        )
    };
    let schema_with = |names: &[&TypeRef]| {
        let mut schema = Schema::default();
        for name in names {
            schema.types.insert((*name).clone(), definition());
        }
        schema
    };
    let derive = |changes: Vec<Change>,
                  base: &Schema,
                  desired: &Schema,
                  opening: &[BindingRecord],
                  compiled: &[BindingRecord]| {
        let base_ids = ids(base, &IdsFile::default());
        // A drop needs a recorded reason to be resolved; the desired side
        // here only needs to lack the type.
        let mut kept = base_ids.clone();
        kept.types
            .retain(|_, name| desired.types.contains_key(name));
        let desired_ids = ids(desired, &kept);
        super::transitions::derive(
            &ChangeSet {
                changes: changes.into_iter().map(PlannedChange::new).collect(),
            },
            pbps_diff::Side {
                schema: base,
                ids: &base_ids,
            },
            pbps_diff::Side {
                schema: desired,
                ids: &desired_ids,
            },
            opening,
            compiled,
        )
        .unwrap()
    };
    let uid = pbps_model::Uid::generate(pbps_model::UidKind::Type);
    let (mood_row, mood_record) = record("mood", &mood);
    let (_, other_record) = record("other", &other);

    // Created: the compiled records of the type, and not another's.
    let created = derive(
        vec![Change::CreateType {
            uid: uid.clone(),
            name: mood.clone(),
            definition: Box::new(definition()),
        }],
        &Schema::default(),
        &schema_with(&[&mood]),
        &[],
        &[mood_record.clone(), other_record.clone()],
    );
    let [transition] = created.as_slice() else {
        panic!("{created:?}");
    };
    assert_eq!(transition.surface, Surface::Type(mood.clone()));
    assert!(transition.before.is_empty());
    assert_eq!(transition.after, BTreeSet::from([mood_row.clone()]));

    // Dropped: the opening records, and nothing is installed.
    let dropped = derive(
        vec![Change::DropType {
            uid: uid.clone(),
            name: mood.clone(),
            definition: Box::new(definition()),
        }],
        &schema_with(&[&mood]),
        &Schema::default(),
        &[mood_record.clone(), other_record.clone()],
        &[],
    );
    let [transition] = dropped.as_slice() else {
        panic!("{dropped:?}");
    };
    assert_eq!(transition.before, BTreeSet::from([mood_row.clone()]));
    assert!(transition.after.is_empty());

    // A label operation replaces the records in place.
    let altered = derive(
        vec![Change::AddEnumValue {
            uid: uid.clone(),
            name: mood.clone(),
            value: "b".into(),
            placement: None,
            ordinal: 0,
            definition: Box::new(definition()),
        }],
        &schema_with(&[&mood]),
        &schema_with(&[&mood]),
        std::slice::from_ref(&mood_record),
        std::slice::from_ref(&mood_record),
    );
    let [transition] = altered.as_slice() else {
        panic!("{altered:?}");
    };
    assert_eq!(transition.before, BTreeSet::from([mood_row.clone()]));
    assert_eq!(transition.after, BTreeSet::from([mood_row.clone()]));
    // Negative: a type the desired schema does not declare is not installed.
    let undeclared = derive(
        vec![Change::AddEnumValue {
            uid,
            name: mood.clone(),
            value: "b".into(),
            placement: None,
            ordinal: 0,
            definition: Box::new(definition()),
        }],
        &schema_with(&[&mood]),
        &Schema::default(),
        std::slice::from_ref(&mood_record),
        std::slice::from_ref(&mood_record),
    );
    assert!(undeclared[0].after.is_empty(), "{undeclared:?}");

    // A rename: the opening spelling's records go, the final one's arrive.
    let (old_row, old_record) = record("old", &old);
    let base = schema_with(&[&old]);
    let base_ids = ids(&base, &IdsFile::default());
    let renamed_uid = base_ids.type_uid(&old).unwrap().clone();
    let mut desired_ids = base_ids.clone();
    desired_ids.types.insert(renamed_uid.clone(), mood.clone());
    let desired = schema_with(&[&mood]);
    let renamed = super::transitions::derive(
        &ChangeSet {
            changes: vec![PlannedChange::new(Change::RenameType {
                uid: renamed_uid,
                from: old.clone(),
                to: mood.clone(),
                definition: Box::new(definition()),
            })],
        },
        pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        },
        pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        },
        &[old_record, other_record.clone()],
        &[mood_record],
    )
    .unwrap();
    let [transition] = renamed.as_slice() else {
        panic!("{renamed:?}");
    };
    assert_eq!(transition.surface, Surface::Type(mood.clone()));
    assert_eq!(transition.before, BTreeSet::from([old_row]));
    assert_eq!(transition.after, BTreeSet::from([mood_row]));
}
