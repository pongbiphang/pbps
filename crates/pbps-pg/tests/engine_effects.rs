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

/// An index key naming an operator class of a package requires the package's
/// create in its final incarnation, as a column of its type does; a key naming
/// an engine class requires none (DEC-2447.14). Asked of the effects, because the
/// plan's order puts a package first whether or not the edge is there.
#[test]
fn an_index_key_naming_a_package_class_requires_the_package_and_an_engine_class_does_not() {
    use pbps_model::operation::{Incarnation, ObjectKey, Requirement};
    use pbps_model::{Index, IndexColumn, IndexKey, IndexMethod, OpClassRef};
    let name = TableName::new("app", "docs");
    let table = |index: Option<OpClassRef>| {
        let mut t = Table {
            columns: [("body".into(), Column::new("text".parse().unwrap()))].into(),
            ..Default::default()
        };
        if let Some(class) = index {
            t.indexes.insert(
                "docs_ix".into(),
                Index {
                    columns: vec![IndexColumn {
                        key: IndexKey::Column("body".into()),
                        descending: false,
                        opclass: Some(class),
                        collation: None,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: IndexMethod::Gin,
                    storage_parameters: Default::default(),
                },
            );
        }
        t
    };
    let ctx = pbps_diff::Context {
        operator: "test".into(),
        today: "2026-11-08".into(),
    };
    let requires_package = |class: OpClassRef| {
        let base = Schema {
            tables: [(name.clone(), table(None))].into(),
            ..Default::default()
        };
        let mut desired = Schema {
            tables: [(name.clone(), table(Some(class)))].into(),
            ..Default::default()
        };
        desired.extensions.insert(
            "pg_trgm".into(),
            pbps_model::Extension {
                schema: "ext".into(),
                version: "1.6".into(),
            },
        );
        let base_ids = pbps_diff::resolve(&base, &IdsFile::default(), &[], &ctx)
            .unwrap()
            .ids;
        let ids = pbps_diff::resolve(&desired, &base_ids, &[], &ctx)
            .unwrap()
            .ids;
        let changes = pbps_diff::diff(
            pbps_diff::Side {
                schema: &base,
                ids: &base_ids,
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &ids,
            },
            &Postgres::new(),
            &Default::default(),
        )
        .unwrap()
        .changes
        .clone();
        let ops = Operations::of_plan(&changes, ModuleEdits::InPlace);
        let owners = PlanOwners::new(PlanNames::of(&changes, &ids).unwrap(), &base_ids, &ids);
        let (before, after) = (
            Snapshot {
                schema: &base,
                ids: &base_ids,
            },
            Snapshot {
                schema: &desired,
                ids: &ids,
            },
        );
        let effects = analyse(&ops, &owners, before, after, &Postgres::new()).effects;
        let at = changes
            .iter()
            .position(|c| matches!(c.change, Change::AddIndex { .. }))
            .expect("the plan adds the index");
        effects[at].requires.iter().any(|r| {
            matches!(
                r,
                Requirement::Is(ObjectKey::Extension(_), Incarnation::Final)
            )
        })
    };
    assert!(requires_package(OpClassRef::in_schema(
        "ext",
        "gin_trgm_ops"
    )));
    // Negatives: an engine class, and a class of a schema no package stands in.
    assert!(!requires_package(OpClassRef::bare("jsonb_path_ops")));
    assert!(!requires_package(OpClassRef::in_schema(
        "other",
        "gin_trgm_ops"
    )));
}

/// A package's drop waits for an index the plan drops that would block it: the effect is the
/// index's absence, so the order puts `DROP INDEX` first and the check sees it (DEC-2447.14).
#[test]
fn a_package_drop_requires_the_absence_of_an_index_it_releases() {
    use pbps_model::operation::{ObjectKey, PartKey, Requirement};
    use pbps_model::{Extension, Index, IndexColumn, IndexKey, IndexMethod};
    let name = TableName::new("app", "docs");
    let table = |indexed: bool| {
        let mut t = Table {
            columns: [("body".into(), Column::new("text".parse().unwrap()))].into(),
            ..Default::default()
        };
        if indexed {
            t.indexes.insert(
                "docs_ix".into(),
                Index {
                    columns: vec![IndexColumn {
                        key: IndexKey::Column("body".into()),
                        descending: false,
                        opclass: None,
                        collation: None,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: IndexMethod::Btree,
                    storage_parameters: Default::default(),
                },
            );
        }
        t
    };
    let package = Extension {
        schema: "ext".into(),
        version: "1.6".into(),
    };
    let base = Schema {
        tables: [(name.clone(), table(true))].into(),
        extensions: [("pg_trgm".into(), package.clone())].into(),
        ..Default::default()
    };
    let desired = Schema {
        tables: [(name.clone(), table(false))].into(),
        ..Default::default()
    };
    let ctx = pbps_diff::Context {
        operator: "test".into(),
        today: "2026-11-08".into(),
    };
    let base_ids = pbps_diff::resolve(&base, &IdsFile::default(), &[], &ctx)
        .unwrap()
        .ids;
    let ids = pbps_diff::resolve(&desired, &base_ids, &[], &ctx)
        .unwrap()
        .ids;
    let uid = base_ids
        .extensions
        .keys()
        .next()
        .expect("the package is identified")
        .clone();
    let released =
        |releases: std::collections::BTreeSet<(TableName, pbps_model::ObjectLocalName)>| {
            let changes = vec![
                PlannedChange::new(Change::DropIndex {
                    table: name.clone(),
                    name: "docs_ix".into(),
                }),
                PlannedChange::new(Change::DropExtension {
                    uid: uid.clone(),
                    name: "pg_trgm".into(),
                    definition: package.clone(),
                    requirers: Default::default(),
                    releases: Default::default(),
                    index_releases: releases,
                    search_releases: Default::default(),
                }),
            ];
            let ops = Operations::of_plan(&changes, ModuleEdits::InPlace);
            let owners = PlanOwners::new(PlanNames::of(&changes, &ids).unwrap(), &base_ids, &ids);
            let (before, after) = (
                Snapshot {
                    schema: &base,
                    ids: &base_ids,
                },
                Snapshot {
                    schema: &desired,
                    ids: &ids,
                },
            );
            let effects = analyse(&ops, &owners, before, after, &Postgres::new()).effects;
            effects[1].requires.iter().any(|r| {
                matches!(
                    r,
                    Requirement::Absent(ObjectKey::Part {
                        part: PartKey::Index(_),
                        ..
                    })
                )
            })
        };
    assert!(released([(name.clone(), "docs_ix".into())].into()));
    // Negative: with nothing released the drop waits for no index.
    assert!(!released(Default::default()));
}

/// A configuration that maps a dictionary of a package waits for the plan's create of that
/// package, and a package's drop waits for the configuration the plan retires with it
/// (DEC-2728.2). Negatives: an engine dictionary draws no package edge, and a drop that
/// releases nothing waits for no configuration.
#[test]
fn search_objects_and_packages_wait_for_each_other_through_the_effects() {
    use pbps_model::operation::{Incarnation, ObjectKey, Requirement};
    use pbps_model::{Extension, SearchObject, SearchRef};
    let docs: SearchRef = "config app.docs".parse().unwrap();
    let package = Extension {
        schema: "ext".into(),
        version: "1.0".into(),
    };
    let config = |dictionary: &str| SearchObject::Config {
        parser: "pg_catalog.\"default\"".parse().unwrap(),
        mappings: [("word".to_owned(), vec![dictionary.parse().unwrap()])].into(),
    };
    let ctx = pbps_diff::Context {
        operator: "test".into(),
        today: "2026-11-13".into(),
    };
    let effects_of = |base: &Schema, desired: &Schema, changes: Vec<PlannedChange>| {
        let base_ids = pbps_diff::resolve(base, &IdsFile::default(), &[], &ctx)
            .unwrap()
            .ids;
        let ids = pbps_diff::resolve(desired, &base_ids, &[], &ctx)
            .unwrap()
            .ids;
        let ops = Operations::of_plan(&changes, ModuleEdits::InPlace);
        let owners = PlanOwners::new(PlanNames::of(&changes, &ids).unwrap(), &base_ids, &ids);
        let (before, after) = (
            Snapshot {
                schema: base,
                ids: &base_ids,
            },
            Snapshot {
                schema: desired,
                ids: &ids,
            },
        );
        (
            analyse(&ops, &owners, before, after, &Postgres::new()).effects,
            base_ids,
            ids,
        )
    };
    // A create: the configuration needs the package only for a dictionary of its schema.
    let waits_for_package = |dictionary: &str| {
        let desired = Schema {
            extensions: [("dict_int".into(), package.clone())].into(),
            search: [(docs.clone(), config(dictionary))].into(),
            ..Default::default()
        };
        let ids_for = pbps_diff::resolve(&desired, &IdsFile::default(), &[], &ctx)
            .unwrap()
            .ids;
        let create = PlannedChange::new(Change::CreateSearch {
            uid: ids_for.search_uid(&docs).unwrap().clone(),
            name: docs.clone(),
            definition: Box::new(config(dictionary)),
            unresolved: Vec::new(),
        });
        let (effects, ..) = effects_of(&Schema::default(), &desired, vec![create]);
        effects[0].requires.iter().any(|r| {
            matches!(
                r,
                Requirement::Is(ObjectKey::Extension(_), Incarnation::Final)
            )
        })
    };
    assert!(waits_for_package("ext.intdict"));
    assert!(!waits_for_package("pg_catalog.simple"));
    // A drop: the package waits for the configuration it releases.
    let base = Schema {
        extensions: [("dict_int".into(), package.clone())].into(),
        search: [(docs.clone(), config("ext.intdict"))].into(),
        ..Default::default()
    };
    let releases = |released: bool| {
        let base_ids = pbps_diff::resolve(&base, &IdsFile::default(), &[], &ctx)
            .unwrap()
            .ids;
        let changes = vec![
            PlannedChange::new(Change::DropSearch {
                uid: base_ids.search_uid(&docs).unwrap().clone(),
                name: docs.clone(),
                definition: Box::new(config("ext.intdict")),
            }),
            PlannedChange::new(Change::DropExtension {
                uid: base_ids.extension_uid(&"dict_int".into()).unwrap().clone(),
                name: "dict_int".into(),
                definition: package.clone(),
                requirers: Default::default(),
                releases: Default::default(),
                index_releases: Default::default(),
                search_releases: if released {
                    [docs.clone()].into()
                } else {
                    Default::default()
                },
            }),
        ];
        let desired = Schema::default();
        let ops = Operations::of_plan(&changes, ModuleEdits::InPlace);
        let ids = IdsFile::default();
        let owners = PlanOwners::new(PlanNames::of(&changes, &ids).unwrap(), &base_ids, &ids);
        let (before, after) = (
            Snapshot {
                schema: &base,
                ids: &base_ids,
            },
            Snapshot {
                schema: &desired,
                ids: &ids,
            },
        );
        let effects = analyse(&ops, &owners, before, after, &Postgres::new()).effects;
        effects[1]
            .requires
            .iter()
            .any(|r| matches!(r, Requirement::Absent(ObjectKey::Search(_))))
    };
    assert!(releases(true));
    assert!(!releases(false));
}
