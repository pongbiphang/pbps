//! The ordering audit names every kind of change (#1351). A new `Change`
//! variant without a row there is a kind whose interactions nobody has
//! classified, which is how ordering corners were found one review round at a
//! time.

use std::path::Path;

use syn::Item;

/// The variants of `pbps_model::Change`, read from its source rather than
/// listed here, so the list cannot fall behind the enum.
fn change_kinds() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/change.rs");
    let file = syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap();
    file.items
        .iter()
        .find_map(|item| {
            if let Item::Enum(e) = item
                && e.ident == "Change"
            {
                Some(
                    e.variants
                        .iter()
                        .map(|v| v.ident.to_string())
                        .collect::<Vec<_>>(),
                )
            } else {
                None
            }
        })
        .expect("`Change` is an enum in change.rs")
}

/// The requires table of `docs/ORDERING.md`: the lines between its heading
/// and the next one.
fn requires_table() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/ORDERING.md");
    let text = std::fs::read_to_string(path).unwrap();
    let start = text
        .find("## What each change requires")
        .expect("the audit has its requires table");
    let rest = &text[start + 3..];
    rest[..rest.find("\n## ").unwrap_or(rest.len())].to_owned()
}

/// The kinds named in the first cell of each row, alone or with a field
/// pattern (`SetPrimaryKey { to: None }`). A kind mentioned only in another
/// cell, or in prose, has no row.
fn row_kinds(table: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in table.lines().filter(|l| l.starts_with("| `")) {
        let first = line.split('|').nth(1).unwrap_or("");
        for named in first.split('`').skip(1).step_by(2) {
            let kind = named.split([' ', '{']).next().unwrap_or("");
            out.push(kind.to_owned());
        }
    }
    out
}

#[test]
fn every_change_kind_has_a_row_in_the_ordering_audit() {
    let kinds = change_kinds();
    assert!(
        kinds.len() > 30,
        "the sweep must reach the whole enum: {kinds:?}"
    );
    let named = row_kinds(&requires_table());
    let missing: Vec<&String> = kinds.iter().filter(|k| !named.contains(*k)).collect();
    assert!(
        missing.is_empty(),
        "docs/ORDERING.md has no row for {missing:?}: classify the new kind's \
         interactions before it ships"
    );
}

/// Negative: a kind named only in another row's cells, or in prose, is not a
/// row of its own.
#[test]
fn a_kind_named_outside_the_first_cell_has_no_row() {
    let table = "| Change | Class | Requires |\n\
                 |---|---|---|\n\
                 | `AddColumn` | 8 | `CreateTable` first |\n\
                 Prose naming `DropTable`.\n";
    let named = row_kinds(table);
    assert_eq!(named, ["AddColumn"]);
    assert!(row_kinds(&requires_table()).contains(&"AlterColumnExpression".to_owned()));
}

// ---------------------------------------------------------------------------
// The effects, kind by kind (#1954).

mod effects_of_each_kind {
    use std::collections::{BTreeMap, BTreeSet};

    use indexmap::IndexMap;
    use pbps_model::operation::{
        Effects, ModuleEdits, Operations, Origin, PlanOwners, Requirement, Snapshot, Source,
        effects,
    };
    use pbps_model::plan_names::PlanNames;
    use pbps_model::*;

    use super::requires_table;

    fn uid(kind: UidKind, name: &str) -> Uid {
        Uid::derived(kind, name, 0)
    }
    fn tn(name: &str) -> TableName {
        TableName::new("app", name)
    }
    fn int() -> ColumnType {
        "int".parse().unwrap()
    }
    fn table(columns: &[(&str, Column)]) -> Table {
        Table {
            columns: columns
                .iter()
                .map(|(n, c)| ((*n).into(), c.clone()))
                .collect::<IndexMap<_, _>>(),
            ..Table::default()
        }
    }
    fn index() -> Index {
        Index {
            columns: vec![IndexColumn {
                key: IndexKey::Column("x".into()),
                descending: false,
                opclass: None,
            }],
            include: vec![],
            unique: false,
            filter: None,
            method: IndexMethod::Btree,
            storage_parameters: BTreeMap::new(),
        }
    }
    fn pk() -> PrimaryKey {
        PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: BTreeMap::new(),
        }
    }
    fn fk() -> ForeignKey {
        ForeignKey {
            columns: vec!["x".into()],
            references_table: tn("r"),
            references_columns: vec!["id".into()],
            on_delete: ReferentialAction::default(),
            on_update: ReferentialAction::default(),
        }
    }
    fn check() -> CheckConstraint {
        CheckConstraint {
            expression: "x > 0".into(),
        }
    }
    fn computed() -> ComputedColumn {
        ComputedColumn {
            expression: "x + 1".into(),
            persisted: false,
            not_null: false,
        }
    }
    fn bound() -> PartitionBound {
        PartitionBound::Range {
            from: vec![BoundDatum::Value("0".into())],
            to: vec![BoundDatum::Value("10".into())],
        }
    }

    /// The world every kind is asked about: a referenced table, a table that
    /// holds every kind of part, a partition of it, a routine and a view, and
    /// a role. The baseline and the desired state are the same, so what a
    /// kind reports is what its payload says plus what stands around it.
    struct World {
        schema: Schema,
        after: Schema,
        ids: IdsFile,
        t: Table,
        p: Table,
    }

    fn world() -> World {
        let id = Column::new(int()).not_null();
        let mut x = Column::new(int());
        x.value_source = ValueSource::Default("1".into());
        let y = Column::new(int()).not_null();
        let r = {
            let mut r = table(&[("id", id.clone())]);
            r.primary_key = Some(pk());
            r
        };
        let mut t = table(&[("id", id.clone()), ("x", x), ("y", y)]);
        t.primary_key = Some(pk());
        t.unique.insert(
            "u".into(),
            UniqueConstraint {
                columns: vec!["x".into()],
                storage_parameters: BTreeMap::new(),
            },
        );
        t.foreign_keys.insert("f".into(), fk());
        t.checks.insert("k".into(), check());
        t.indexes.insert("ix".into(), index());
        t.computed.insert("c".into(), computed());
        t.replica_identity = Some(ReplicaIdentity::Index("ix".into()));
        let p = Table {
            partition_of: Some(PartitionOf {
                parent: tn("t"),
                bound: bound(),
                columns: [(
                    "x".into(),
                    PartitionColumn {
                        default: Some("2".into()),
                        not_null: false,
                    },
                )]
                .into(),
            }),
            ..Table::default()
        };
        let mut schema = Schema::default();
        schema.tables.insert(tn("r"), r);
        schema.tables.insert(tn("t"), t.clone());
        schema.tables.insert(tn("p"), p.clone());
        schema
            .tables
            .insert(tn("n"), table(&[("id", Column::new(int()).not_null())]));
        let f: ModuleId = "app.f(integer)".parse().unwrap();
        let v = ModuleId::Named(tn("v"));
        for (m, kind) in [(f, ModuleKind::Function), (v, ModuleKind::View)] {
            schema.modules.insert(
                m,
                Module {
                    kind,
                    definition: "SELECT 1".into(),
                    description: None,
                    options: Default::default(),
                },
            );
        }
        let mut ids = IdsFile::default();
        for name in ["r", "t", "p", "n"] {
            ids.tables.insert(uid(UidKind::Table, name), tn(name));
        }
        for (table, column) in [("r", "id"), ("t", "id"), ("t", "x"), ("t", "y")] {
            ids.columns.insert(
                uid(UidKind::Column, &format!("{table}.{column}")),
                tn(table).column(column),
            );
        }
        ids.columns
            .insert(uid(UidKind::Column, "n.id"), tn("n").column("id"));
        ids.roles
            .insert(uid(UidKind::Role, "reader"), "reader".to_owned().into());
        // The desired state: `x` takes another default, `y` is relaxed and
        // the check is gone, so the kinds that read what the plan takes away
        // have something to read.
        schema.types.insert(
            "app.base".parse().unwrap(),
            ManagedType::Enum {
                values: vec!["ok".into()],
            },
        );
        schema.types.insert(
            "app.old".parse().unwrap(),
            ManagedType::Domain {
                base: "app.base".parse().unwrap(),
                not_null: false,
                default: None,
                checks: BTreeMap::new(),
                collation: None,
            },
        );
        ids.types
            .insert(uid(UidKind::Type, "base"), "app.base".parse().unwrap());
        ids.types
            .insert(uid(UidKind::Type, "old"), "app.old".parse().unwrap());
        let mut after = schema.clone();
        let at = after.tables.get_mut(&tn("t")).unwrap();
        at.columns.get_mut("x").unwrap().value_source = ValueSource::Default("9".into());
        at.columns.get_mut("y").unwrap().nullable = true;
        at.checks.clear();
        World {
            schema,
            after,
            ids,
            t,
            p,
        }
    }

    fn cols(t: &str, c: &str) -> (Uid, ColumnRef) {
        (uid(UidKind::Column, &format!("{t}.{c}")), tn(t).column(c))
    }

    /// One fixture or more for every kind of change.
    fn fixtures(w: &World) -> Vec<(&'static str, Change)> {
        let (x, xref) = cols("t", "x");
        let (y, yref) = cols("t", "y");
        let routine: ModuleId = "app.f(integer)".parse().unwrap();
        let ModuleId::Routine(routine_id) = routine.clone() else {
            unreachable!()
        };
        let module = Module {
            kind: ModuleKind::Function,
            definition: "SELECT 1".into(),
            description: None,
            options: Default::default(),
        };
        let t = uid(UidKind::Table, "t");
        let p = uid(UidKind::Table, "p");
        let key = RowKey::from("k");
        let cell = |v: i64| Cell::Value(Value::Int(v));
        let mut new_table = table(&[("id", Column::new(int()).not_null())]);
        new_table.foreign_keys.insert("f".into(), fk());
        let mut out = vec![
            (
                "AddEnumValue",
                Change::AddEnumValue {
                    uid: uid(UidKind::Type, "base"),
                    name: "app.base".parse().unwrap(),
                    value: "new".into(),
                    placement: Some(EnumPlacement::After("ok".into())),
                    ordinal: 0,
                    definition: Box::new(ManagedType::Enum {
                        values: vec!["ok".into(), "new".into()],
                    }),
                },
            ),
            // Both halves state the same effects; the row says which class
            // each one takes.
            (
                "AlterDomain",
                Change::AlterDomain {
                    uid: uid(UidKind::Type, "base"),
                    name: "app.base".parse().unwrap(),
                    from: Box::new(ManagedType::Domain {
                        base: "text".parse().unwrap(),
                        not_null: false,
                        default: None,
                        checks: Default::default(),
                        collation: None,
                    }),
                    to: Box::new(ManagedType::Domain {
                        base: "text".parse().unwrap(),
                        not_null: true,
                        default: None,
                        checks: Default::default(),
                        collation: None,
                    }),
                    half: DomainHalf::Tighten,
                },
            ),
            (
                "RenameEnumValue",
                Change::RenameEnumValue {
                    uid: uid(UidKind::Type, "base"),
                    name: "app.base".parse().unwrap(),
                    from: "ok".into(),
                    to: "fine".into(),
                    ordinal: 0,
                    definition: Box::new(ManagedType::Enum {
                        values: vec!["fine".into()],
                    }),
                },
            ),
            // The three steps of one rebuild share a row, as its kinds do.
            (
                "EnumRebuild",
                Change::EnumRebuild {
                    uid: uid(UidKind::Type, "base"),
                    name: "app.base".parse().unwrap(),
                    tmp: "app.pbps_rebuild_base".parse().unwrap(),
                    step: pbps_model::EnumRebuildStep::Create {
                        definition: Box::new(ManagedType::Enum {
                            values: vec!["ok".into()],
                        }),
                        removed: vec!["old".into()],
                        ordinal: 0,
                        over: None,
                        why: None,
                    },
                },
            ),
            // A domain's replacement is created from its final declaration:
            // the type it is built on stands in its final incarnation first.
            (
                "EnumRebuild",
                Change::EnumRebuild {
                    uid: uid(UidKind::Type, "new_domain"),
                    name: "app.new_domain".parse().unwrap(),
                    tmp: "app.pbps_rebuild_new_domain".parse().unwrap(),
                    step: pbps_model::EnumRebuildStep::Create {
                        definition: Box::new(ManagedType::Domain {
                            base: "app.base".parse().unwrap(),
                            not_null: false,
                            default: None,
                            checks: BTreeMap::new(),
                            collation: None,
                        }),
                        removed: vec![],
                        ordinal: 0,
                        over: None,
                        why: None,
                    },
                },
            ),
            (
                "EnumRebuild",
                Change::EnumRebuild {
                    uid: uid(UidKind::Type, "base"),
                    name: "app.base".parse().unwrap(),
                    tmp: "app.pbps_rebuild_base".parse().unwrap(),
                    step: pbps_model::EnumRebuildStep::Retype {
                        column_uid: x.clone(),
                        column: xref.clone(),
                        to: pbps_model::ColumnType::reference("app.e".parse().unwrap()),
                        array: false,
                        removed: vec![],
                        base_change: None,
                        conversion: Default::default(),
                    },
                },
            ),
            (
                "EnumRebuild",
                Change::EnumRebuild {
                    uid: uid(UidKind::Type, "base"),
                    name: "app.base".parse().unwrap(),
                    tmp: "app.pbps_rebuild_base".parse().unwrap(),
                    step: pbps_model::EnumRebuildStep::Swap {
                        definition: Box::new(ManagedType::Enum {
                            values: vec!["ok".into()],
                        }),
                        comment: None,
                        depth: 0,
                        over: None,
                    },
                },
            ),
            (
                "CreateType",
                Change::CreateType {
                    uid: uid(UidKind::Type, "new_domain"),
                    name: "app.new_domain".parse().unwrap(),
                    definition: Box::new(ManagedType::Domain {
                        base: "app.base".parse().unwrap(),
                        not_null: false,
                        default: None,
                        checks: BTreeMap::new(),
                        collation: None,
                    }),
                },
            ),
            (
                "DropType",
                Change::DropType {
                    uid: uid(UidKind::Type, "base"),
                    name: "app.base".parse().unwrap(),
                    definition: Box::new(ManagedType::Enum { values: vec![] }),
                },
            ),
            (
                "CreateType",
                Change::CreateType {
                    uid: uid(UidKind::Type, "new"),
                    name: "app.new".parse().unwrap(),
                    definition: Box::new(ManagedType::Enum { values: vec![] }),
                },
            ),
            (
                "DropType",
                Change::DropType {
                    uid: uid(UidKind::Type, "old"),
                    name: "app.old".parse().unwrap(),
                    definition: Box::new(ManagedType::Enum { values: vec![] }),
                },
            ),
            (
                "RenameType",
                Change::RenameType {
                    uid: uid(UidKind::Type, "old"),
                    from: "app.old".parse().unwrap(),
                    to: "app.renamed".parse().unwrap(),
                    definition: Box::new(ManagedType::Enum { values: vec![] }),
                },
            ),
            (
                "CreateTable",
                Change::CreateTable {
                    uid: uid(UidKind::Table, "n"),
                    name: tn("n"),
                    table: Box::new(new_table),
                    beside_default: None,
                    moves_at_most: None,
                },
            ),
            (
                "CreateTable",
                Change::CreateTable {
                    uid: p.clone(),
                    name: tn("p"),
                    table: Box::new(w.p.clone()),
                    beside_default: Some(tn("r")),
                    moves_at_most: None,
                },
            ),
            (
                "DropTable",
                Change::DropTable {
                    uid: uid(UidKind::Table, "r"),
                    name: tn("r"),
                    detach_from: None,
                },
            ),
            (
                "DropTable",
                Change::DropTable {
                    uid: p.clone(),
                    name: tn("p"),
                    detach_from: Some(tn("t")),
                },
            ),
            (
                "DetachPartition",
                Change::DetachPartition {
                    uid: p.clone(),
                    table: tn("p"),
                    parent: tn("t"),
                    names: vec![],
                    shape: Box::new(table(&[("id", Column::new(int()))])),
                },
            ),
            (
                "AttachPartition",
                Change::AttachPartition {
                    uid: p.clone(),
                    table: tn("p"),
                    parent: tn("t"),
                    bound: bound(),
                    key: Vec::new(),
                    shape: Box::new(w.p.clone()),
                },
            ),
            (
                "AttachPartition",
                Change::AttachPartition {
                    uid: uid(UidKind::Table, "n"),
                    table: tn("n"),
                    parent: tn("t"),
                    bound: bound(),
                    key: Vec::new(),
                    shape: Box::new(w.p.clone()),
                },
            ),
            (
                "RenameTable",
                Change::RenameTable {
                    uid: t.clone(),
                    from: tn("t"),
                    to: tn("t2"),
                    defaults: vec![],
                },
            ),
            (
                "AddColumn",
                Change::AddColumn {
                    uid: uid(UidKind::Column, "t.z"),
                    table: tn("t"),
                    name: "z".into(),
                    column: Box::new(Column::new(int())),
                },
            ),
            (
                "DropColumn",
                Change::DropColumn {
                    uid: x.clone(),
                    column: xref.clone(),
                },
            ),
            (
                "RenameColumn",
                Change::RenameColumn {
                    uid: x.clone(),
                    table: tn("t"),
                    from: "x".into(),
                    to: "x2".into(),
                    table_was: None,
                },
            ),
            (
                "AlterColumnType",
                Change::AlterColumnType {
                    uid: x.clone(),
                    column: xref.clone(),
                    from: int(),
                    to: "bigint".parse().unwrap(),
                    from_nullable: true,
                    to_nullable: false,
                    from_collation: None,
                    to_collation: None,
                    through_external_partitions: false,
                    using: None,
                },
            ),
            (
                "AlterColumnNullability",
                Change::AlterColumnNullability {
                    uid: y.clone(),
                    column: yref.clone(),
                    ty: int(),
                    to_nullable: false,
                    collation: None,
                    through_external_partitions: false,
                },
            ),
            (
                "AlterColumnNullability",
                Change::AlterColumnNullability {
                    uid: uid(UidKind::Column, "t.id"),
                    column: tn("t").column("id"),
                    ty: int(),
                    to_nullable: true,
                    collation: None,
                    through_external_partitions: false,
                },
            ),
            (
                "AlterColumnDefault",
                Change::AlterColumnDefault {
                    uid: x.clone(),
                    column: xref.clone(),
                    from: Some("1".into()),
                    to: Some("2".into()),
                },
            ),
            (
                "AlterColumnDefault",
                Change::AlterColumnDefault {
                    uid: x.clone(),
                    column: xref.clone(),
                    from: Some("1".into()),
                    to: None,
                },
            ),
            (
                "AlterColumnExpression",
                Change::AlterColumnExpression {
                    uid: y.clone(),
                    column: yref.clone(),
                    from: "1".into(),
                    to: "2".into(),
                },
            ),
            (
                "AlterColumnNumbering",
                Change::AlterColumnNumbering {
                    uid: x.clone(),
                    column: xref.clone(),
                    ty: int(),
                    from: ValueSource::Default("1".into()),
                    to: ValueSource::Identity(Identity::new(1, 1)),
                },
            ),
            (
                "AlterColumnNumbering",
                Change::AlterColumnNumbering {
                    uid: y.clone(),
                    column: yref.clone(),
                    ty: int(),
                    from: ValueSource::Identity(Identity::new(1, 1)),
                    to: ValueSource::None,
                },
            ),
            (
                "AddComputedColumn",
                Change::AddComputedColumn {
                    table: tn("t"),
                    name: "c2".into(),
                    computed: computed(),
                },
            ),
            (
                "DropComputedColumn",
                Change::DropComputedColumn {
                    table: tn("t"),
                    name: "c".into(),
                    computed: computed(),
                },
            ),
            (
                "SetColumnDeprecated",
                Change::SetColumnDeprecated {
                    uid: y.clone(),
                    column: yref.clone(),
                    reason: None,
                },
            ),
            (
                "DropPolicy",
                Change::DropPolicy {
                    table: tn("t"),
                    name: "policy".into(),
                    policy: RowPolicy {
                        using: Some("x > 0".into()),
                        ..Default::default()
                    },
                },
            ),
            (
                "CreatePolicy",
                Change::CreatePolicy {
                    table: tn("t"),
                    name: "policy".into(),
                    policy: RowPolicy {
                        using: Some("x > 0".into()),
                        with_check: Some("x < 100".into()),
                        ..Default::default()
                    },
                    origin: RowSecurityOrigin::Created,
                },
            ),
            (
                "SetRowSecurity",
                Change::SetRowSecurity {
                    uid: t.clone(),
                    table: tn("t"),
                    from: RowSecurity::default(),
                    to: RowSecurity {
                        enabled: true,
                        forced: false,
                    },
                    origin: RowSecurityOrigin::Created,
                },
            ),
            (
                "CreatePolicy",
                Change::CreatePolicy {
                    table: tn("t"),
                    name: "standing_policy".into(),
                    policy: RowPolicy {
                        restrictive: true,
                        using: Some("x > 10".into()),
                        with_check: Some("x < 50".into()),
                        ..Default::default()
                    },
                    origin: RowSecurityOrigin::Standing,
                },
            ),
            (
                "SetRowSecurity",
                Change::SetRowSecurity {
                    uid: t.clone(),
                    table: tn("t"),
                    from: RowSecurity {
                        enabled: true,
                        forced: true,
                    },
                    to: RowSecurity {
                        enabled: false,
                        forced: true,
                    },
                    origin: RowSecurityOrigin::Standing,
                },
            ),
            (
                "SetReplicaIdentity",
                Change::SetReplicaIdentity {
                    uid: t.clone(),
                    table: tn("t"),
                    to: Some(ReplicaIdentity::Index("ix".into())),
                },
            ),
            (
                "SetStorageParameters",
                Change::SetStorageParameters {
                    uid: t.clone(),
                    table: tn("t"),
                    set: BTreeMap::new(),
                    reset: BTreeSet::new(),
                },
            ),
            (
                "SetTablePersistence",
                Change::SetTablePersistence {
                    uid: t.clone(),
                    table: tn("t"),
                    unlogged: true,
                },
            ),
            (
                "SetPartitionDefault",
                Change::SetPartitionDefault {
                    uid: p.clone(),
                    table: tn("p"),
                    parent: tn("t"),
                    column: "x".into(),
                    from: None,
                    to: Some("3".into()),
                    fallback: None,
                },
            ),
            (
                "SetPartitionNotNull",
                Change::SetPartitionNotNull {
                    uid: p.clone(),
                    table: tn("p"),
                    column: "x".into(),
                    not_null: true,
                },
            ),
            (
                "SetIndexStorageParameters",
                Change::SetIndexStorageParameters {
                    table: tn("t"),
                    target: IndexPart::Index("ix".into()),
                    method: IndexMethod::Btree,
                    set: BTreeMap::new(),
                    reset: BTreeSet::new(),
                },
            ),
            (
                "SetPrimaryKey { to: Some }",
                Change::SetPrimaryKey {
                    table: tn("t"),
                    from: Some(pk()),
                    to: Some(pk()),
                    nonclustered: false,
                },
            ),
            (
                "SetPrimaryKey { to: None }",
                Change::SetPrimaryKey {
                    table: tn("t"),
                    from: Some(pk()),
                    to: None,
                    nonclustered: false,
                },
            ),
            (
                "AddUnique",
                Change::AddUnique {
                    table: tn("t"),
                    name: "u2".into(),
                    constraint: UniqueConstraint {
                        columns: vec!["x".into()],
                        storage_parameters: BTreeMap::new(),
                    },
                    clustered: false,
                },
            ),
            (
                "DropUnique",
                Change::DropUnique {
                    table: tn("t"),
                    name: "u".into(),
                },
            ),
            (
                "AddForeignKey",
                Change::AddForeignKey {
                    table: tn("t"),
                    name: "f2".into(),
                    constraint: Box::new(fk()),
                },
            ),
            (
                "DropForeignKey",
                Change::DropForeignKey {
                    table: tn("t"),
                    name: "f".into(),
                },
            ),
            (
                "AddCheck",
                Change::AddCheck {
                    table: tn("t"),
                    name: "k2".into(),
                    constraint: check(),
                },
            ),
            (
                "DropCheck",
                Change::DropCheck {
                    table: tn("t"),
                    name: "k".into(),
                },
            ),
            (
                "AddIndex",
                Change::AddIndex {
                    table: tn("t"),
                    name: "ix2".into(),
                    index: Box::new(index()),
                    clustered: false,
                },
            ),
            (
                "DropIndex",
                Change::DropIndex {
                    table: tn("t"),
                    name: "ix".into(),
                },
            ),
            (
                "InsertRow",
                Change::InsertRow {
                    table: tn("t"),
                    key_column: "id".into(),
                    identity_key: false,
                    key: key.clone(),
                    row: [("id".into(), Value::Int(1)), ("x".into(), Value::Int(2))]
                        .into_iter()
                        .collect(),
                    defaults: [("y".into(), "1".to_owned())].into(),
                    types: BTreeMap::new(),
                },
            ),
            (
                "UpdateRow",
                Change::UpdateRow {
                    table: tn("t"),
                    key_column: "id".into(),
                    key: key.clone(),
                    columns: [("x".into(), (cell(1), cell(2)))].into(),
                    unchanged: BTreeMap::new(),
                    types: BTreeMap::new(),
                    after_types: BTreeMap::new(),
                    key_type: None,
                },
            ),
            (
                "DeleteRow",
                Change::DeleteRow {
                    table: tn("t"),
                    key_column: "id".into(),
                    key,
                    cause: pbps_model::change::DeleteCause::Undeclared,
                    row: [("x".into(), cell(1))].into(),
                    dropped: BTreeMap::new(),
                    types: BTreeMap::new(),
                    after_types: BTreeMap::new(),
                    key_type: None,
                },
            ),
            (
                "SetDataMode",
                Change::SetDataMode {
                    table: tn("t"),
                    from: None,
                    to: Some(DataMode::Exact),
                },
            ),
            (
                "SetEnsureKeys",
                Change::SetEnsureKeys {
                    table: tn("t"),
                    from: BTreeSet::new(),
                    to: BTreeSet::new(),
                },
            ),
            (
                "CreateModule",
                Change::CreateModule {
                    id: routine.clone(),
                    module: Box::new(module.clone()),
                },
            ),
            (
                "AlterModule",
                Change::AlterModule {
                    id: routine.clone(),
                    module: Box::new(module),
                },
            ),
            (
                "DropModule",
                Change::DropModule {
                    id: routine,
                    kind: ModuleKind::Function,
                },
            ),
            (
                "CreateRole",
                Change::CreateRole {
                    uid: uid(UidKind::Role, "new"),
                    name: "new".into(),
                },
            ),
            (
                "DropRole",
                Change::DropRole {
                    uid: uid(UidKind::Role, "reader"),
                    name: "reader".into(),
                    members: vec![],
                },
            ),
            (
                "RenameRole",
                Change::RenameRole {
                    uid: uid(UidKind::Role, "reader"),
                    from: "reader".into(),
                    to: "viewer".into(),
                },
            ),
            (
                "Grant",
                Change::Grant {
                    role: "reader".into(),
                    target: GrantTarget::Object(tn("t")),
                    permissions: BTreeSet::from([Permission::Select]),
                },
            ),
            (
                "Revoke",
                Change::Revoke {
                    role: "reader".into(),
                    target: GrantTarget::Object(tn("t")),
                    permissions: BTreeSet::from([Permission::Select]),
                },
            ),
            (
                "PublicExecution",
                Change::PublicExecution {
                    routine: routine_id,
                    access: PublicAccess::Revoked,
                    origin: RoutineOrigin::Created,
                },
            ),
        ];
        let mut rebuild: Vec<Change> =
            serde_json::from_str(include_str!("fixtures/rebuild_operations.json")).unwrap();
        if let Change::CopyTableRows(c) = &mut rebuild[1] {
            c.source = pbps_model::RebuildRelation {
                uid: uid(UidKind::Table, "r"),
                name: tn("r"),
            };
            c.target = pbps_model::RebuildRelation {
                uid: uid(UidKind::Table, "t"),
                name: tn("t"),
            };
            c.source_partitions.clear();
            c.target_partitions.clear();
            c.columns.clear();
        }
        if let Change::AnalyzeTableTree(c) = &mut rebuild[2] {
            c.parent = pbps_model::RebuildRelation {
                uid: uid(UidKind::Table, "t"),
                name: tn("t"),
            };
            c.partitions.clear();
        }
        out.push((
            "BuildPartitionedIndex",
            Change::BuildPartitionedIndex(Box::new(pbps_model::BuildPartitionedIndex {
                parent: pbps_model::RebuildRelation {
                    uid: uid(UidKind::Table, "t"),
                    name: tn("t"),
                },
                name: "recipe_index".into(),
                definition: pbps_model::PartitionIndexDefinition::Index(
                    serde_json::from_str(r#"{"columns":[{"name":"k"}]}"#).unwrap(),
                ),
                clones: Vec::new(),
            })),
        ));
        out.extend(
            [
                "RebuildPartitionedTable",
                "CopyTableRows",
                "AnalyzeTableTree",
            ]
            .into_iter()
            .zip(rebuild),
        );
        out
    }

    /// What an `Effects` says, as the words its row in the table uses.
    fn words(fx: &Effects) -> BTreeSet<&'static str> {
        let mut out = BTreeSet::new();
        if !fx.creates.is_empty() {
            out.insert("creates");
        }
        if !fx.removes.is_empty() {
            out.insert("removes");
        }
        if !fx.rewrites.is_empty() {
            out.insert("rewrites");
        }
        for r in &fx.requires {
            out.insert(match r {
                Requirement::Holds(_) => "Holds",
                Requirement::Exists(_) => "Exists",
                Requirement::Is(..) => "Is",
                Requirement::Bound(..) => "Bound",
                Requirement::Absent(_) => "Absent",
                Requirement::NotBoundTo { .. } => "NotBoundTo",
                Requirement::Reads(_) => "Reads",
                Requirement::BeforeRewrite(_) => "BeforeRewrite",
            });
        }
        out
    }

    /// The kinds named in the first cell of each row, as the row spells
    /// them (`SetPrimaryKey { to: None }` is not `SetPrimaryKey`), and the
    /// words of its last cell.
    fn documented(table: &str) -> Vec<(Vec<String>, BTreeSet<String>)> {
        table
            .lines()
            .filter(|l| l.starts_with("| `"))
            .map(|line| {
                let cells: Vec<&str> = line.split('|').collect();
                let kinds = cells[1]
                    .split('`')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_owned)
                    .collect();
                let last = cells[cells.len() - 2];
                (
                    kinds,
                    last.split([',', ' ', '`'])
                        .filter(|w| !w.is_empty())
                        .map(str::to_owned)
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn the_requires_table_states_the_effects_of_each_kind_it_names() {
        let w = world();
        let names = PlanNames::of(&[], &w.ids).unwrap();
        let owners = PlanOwners::new(names, &w.ids, &w.ids);
        let snapshot = Snapshot {
            schema: &w.schema,
            ids: &w.ids,
        };
        let desired = Snapshot {
            schema: &w.after,
            ids: &w.ids,
        };
        let mut seen: BTreeMap<&str, BTreeSet<&'static str>> = BTreeMap::new();
        for (kind, change) in fixtures(&w) {
            let ops = Operations::expanded(
                1,
                [(
                    Origin::Restoration(Source::Change(0)),
                    PlannedChange::new(change),
                )],
                ModuleEdits::InPlace,
            );
            let result = effects(&ops.as_slice()[0], &owners, snapshot, desired);
            if kind == "RebuildPartitionedTable" {
                assert!(result.unwrap_err().to_string().contains("must be expanded"));
                seen.insert(kind, BTreeSet::from(["refuses"]));
                continue;
            }
            let fx = result.unwrap_or_else(|e| panic!("{kind}: {e}"));
            if kind == "DropPolicy" {
                assert!(fx.creates.is_empty());
                assert_eq!(fx.removes.len(), 1);
                let names =
                    pbps_model::operation::names(&ops.as_slice()[0], &owners, snapshot).unwrap();
                assert!(names.claims.is_empty());
                assert_eq!(names.releases.len(), 1);
                assert_eq!(
                    names.releases[0].address.kind,
                    pbps_model::operation::NameKind::Policy
                );
                assert_eq!(names.releases[0].address.name, "policy");
            }
            if let Change::EnumRebuild { step, .. } = ops.as_slice()[0].change() {
                let names =
                    pbps_model::operation::names(&ops.as_slice()[0], &owners, snapshot).unwrap();
                match step {
                    // The replacement's temporary name is held from its creation
                    // to the swap, and the retype of a defaulted column needs
                    // the default off first (DEC-1635.7).
                    pbps_model::EnumRebuildStep::Create { .. } => {
                        assert_eq!((names.claims.len(), names.releases.len()), (1, 0));
                    }
                    pbps_model::EnumRebuildStep::Swap { .. } => {
                        assert_eq!((names.claims.len(), names.releases.len()), (0, 1));
                    }
                    pbps_model::EnumRebuildStep::Retype { .. } => {
                        assert!(names.claims.is_empty() && names.releases.is_empty());
                        assert!(fx.requires.iter().any(|r| matches!(
                            r,
                            Requirement::Absent(pbps_model::operation::ObjectKey::Default(_))
                        )));
                    }
                }
            }
            seen.entry(kind).or_default().extend(words(&fx));
        }
        let table = requires_table();
        let mut problems = Vec::new();
        let mut print = String::new();
        for (kinds, doc) in documented(&table) {
            let mut union: BTreeSet<String> = BTreeSet::new();
            for kind in &kinds {
                // The plain name of a kind a row spells with a pattern
                // elsewhere is the one with `to: Some`.
                let label = if kind == "SetPrimaryKey" {
                    "SetPrimaryKey { to: Some }"
                } else {
                    kind.as_str()
                };
                let Some(words) = seen.get(label) else {
                    problems.push(format!("no fixture for {kind}"));
                    continue;
                };
                union.extend(words.iter().map(|w| (*w).to_owned()));
            }
            print.push_str(&format!(
                "{kinds:?}: {}\n",
                union.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
            if union != doc {
                problems.push(format!(
                    "{kinds:?}: the table says {doc:?}, effects say {union:?}"
                ));
            }
        }
        if std::env::var_os("PBPS_AUDIT_PRINT").is_some() {
            eprintln!("{print}");
        }
        assert!(problems.is_empty(), "{problems:#?}");
        let named: BTreeSet<&str> = seen.keys().copied().collect();
        assert!(
            named.len() > 40,
            "the sweep must reach the whole enum: {named:?}"
        );
    }

    /// Negative: a table whose last cell disagrees with the effects is
    /// refused, so the check is not satisfied by an empty comparison.
    #[test]
    fn a_row_that_states_other_effects_than_the_code_makes_is_refused() {
        let doc = documented("| `AddColumn` | 8 | x | y | removes |\n");
        assert_eq!(doc[0].1, BTreeSet::from(["removes".to_owned()]));
        let w = world();
        let names = PlanNames::of(&[], &w.ids).unwrap();
        let owners = PlanOwners::new(names, &w.ids, &w.ids);
        let snapshot = Snapshot {
            schema: &w.schema,
            ids: &w.ids,
        };
        let (kind, change) = fixtures(&w)
            .into_iter()
            .find(|(k, _)| *k == "AddColumn")
            .unwrap();
        let ops = Operations::expanded(
            1,
            [(
                Origin::Restoration(Source::Change(0)),
                PlannedChange::new(change),
            )],
            ModuleEdits::InPlace,
        );
        let fx = effects(&ops.as_slice()[0], &owners, snapshot, snapshot).unwrap();
        assert_eq!(kind, "AddColumn");
        assert_ne!(
            words(&fx)
                .iter()
                .map(|w| w.to_string())
                .collect::<BTreeSet<_>>(),
            doc[0].1
        );
        let _ = w.t.columns.len();
    }
}
