use super::*;

#[test]
fn key_surfaces_keep_observed_teardown_declared_restoration_and_physical_names() {
    let table: pbps_model::Table = serde_json::from_value(serde_json::json!({
        "columns": {"id": {"type": "integer", "nullable": false}},
        "primary_key": {"name": "desired_pk", "columns": ["id"]},
        "unique": {"uq": {"columns": ["id"]}},
        "foreign_keys": {"fk": {"columns": ["id"], "references_table": "app.target", "references_columns": ["id"]}}
    })).unwrap();
    let mut before = Schema::default();
    let mut ids = IdsFile::default();
    let mut after = ids.clone();
    let mut renames = Vec::new();
    for (uid, old, new) in [
        ("t_aaaaaa", "app.source", "app.renamed"),
        ("t_bbbbbb", "app.old_target", "app.target"),
    ] {
        let uid: pbps_model::Uid = uid.parse().unwrap();
        let old: TableName = old.parse().unwrap();
        let new: TableName = new.parse().unwrap();
        let mut source = table.clone();
        source.foreign_keys.get_mut("fk").unwrap().references_table =
            "app.old_target".parse().unwrap();
        before.tables.insert(old.clone(), source);
        let column: pbps_model::Uid = uid.to_string().replace("t_", "c_").parse().unwrap();
        ids.columns.insert(column.clone(), old.column("id"));
        after.columns.insert(column, new.column("id"));
        ids.tables.insert(uid.clone(), old.clone());
        after.tables.insert(uid.clone(), new.clone());
        renames.push(PlannedChange::new(Change::RenameTable {
            uid,
            from: old,
            to: new,
            defaults: Vec::new(),
        }));
    }
    let final_name: TableName = "app.renamed".parse().unwrap();
    let mut declared = Schema::default();
    declared.tables.insert(final_name.clone(), table.clone());
    let mut undeclared = declared.clone();
    undeclared
        .tables
        .insert(final_name.clone(), pbps_model::Table::default());
    let sources = Sources {
        before: crate::Side {
            schema: &before,
            ids: &ids,
        },
        after: &after,
    };
    let view = sources.project(&renames).unwrap();
    let mut observed = table.primary_key.clone().unwrap();
    observed.name = Some("observed_pk".into());
    for part in [
        Part::PrimaryKey(observed.clone()),
        Part::Unique("uq".into()),
        Part::ForeignKey("fk".into()),
    ] {
        let d = Dependent {
            described: format!("{part:?}"),
            holds: Holds::TablePart {
                table: "app.source".parse().unwrap(),
                part,
            },
        };
        let original = d;
        let d = as_declared(&view, &original).unwrap();
        assert!(d.managed(&declared));
        let removal = removal_of(&d, &declared, None).unwrap();
        if let Change::SetPrimaryKey { from, .. } = &removal {
            assert_eq!(from.as_ref(), Some(&observed));
        }
        assert!(removes(
            &removal.part_effects(),
            &d.holds,
            &Partitions::default()
        ));
        let restoration = restoration_of(&d, &declared, None).unwrap();
        if let Change::SetPrimaryKey { to, .. } = &restoration {
            assert_eq!(to.as_ref(), table.primary_key.as_ref());
        }
        assert!(restores(
            &restoration.part_effects(),
            &d.holds,
            &Partitions::default()
        ));
        let mut physical = planned(restoration, &pbps_dialect::MinimalDialect);
        respell_at(
            &mut physical,
            &original.holds,
            &view,
            At::Before(0),
            Intent::Restore,
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        assert_eq!(
            physical.change.table(),
            Some(&"app.source".parse().unwrap())
        );
        if let Change::AddForeignKey { constraint, .. } = physical.change {
            assert_eq!(
                constraint.references_table,
                "app.old_target".parse().unwrap()
            );
        }
        assert!(!d.managed(&undeclared));
        assert!(restoration_of(&d, &undeclared, None).is_err());
    }
}

const RAW_DESCRIPTION: &str = "dependent\n\u{1b}[31m\u{202e}\\n名前_ünï";
const SHOWN_DESCRIPTION: &str = r"dependent\n\u{1b}[31m\u{202e}\\n名前_ünï";

#[test]
fn unsupported_descriptions_are_shown_once_and_reasons_stay_rendered() {
    let module = ModuleId::Named(pbps_model::ObjectName::new("app", RAW_DESCRIPTION));
    let mut declared = Schema::default();
    for (holds, suffix) in [
        (Holds::Module(module.clone()), ": its kind is not declared"),
        (
            Holds::TablePart {
                table: TableName::new("app", "t"),
                part: Part::Generated(RAW_DESCRIPTION.into()),
            },
            ": a generated column's expression cannot be taken off around the module",
        ),
        (
            Holds::Unrepresentable(r"already shown `name\n`".into()),
            r" — already shown `name\n`",
        ),
    ] {
        let d = Dependent {
            described: RAW_DESCRIPTION.into(),
            holds,
        };
        assert_eq!(
            removal_of(&d, &declared, None).unwrap_err(),
            format!("{SHOWN_DESCRIPTION}{suffix}")
        );
        assert_eq!(
            restoration_of(&d, &declared, None).unwrap_err(),
            format!(
                "{SHOWN_DESCRIPTION}: the declarations hold it, but not in a form pbps can restore"
            )
        );
        assert_eq!(d.described, RAW_DESCRIPTION);
    }
    // A raw hostile identity still selects its declared module for both operations.
    declared.modules.insert(
        module.clone(),
        pbps_model::Module {
            kind: ModuleKind::View,
            description: None,
            definition: "SELECT 1".into(),
            options: Default::default(),
        },
    );
    let d = Dependent {
        described: RAW_DESCRIPTION.into(),
        holds: Holds::Module(module.clone()),
    };
    assert!(
        matches!(removal_of(&d, &declared, None).unwrap(), Change::DropModule { id, .. } if id == module)
    );
    assert!(
        matches!(restoration_of(&d, &declared, None).unwrap(), Change::CreateModule { id, .. } if id == module)
    );
}

#[test]
fn named_descriptions_are_shown_once_without_changing_obligations() {
    let root: ModuleId = "app.root".parse().unwrap();
    for (need, suffix) in [
        (
            Need::Split {
                root: root.clone(),
                k: 0,
            },
            "is edited in one step and has to be split around `app.root`",
        ),
        (
            Need::Removed {
                root: root.clone(),
                k: 0,
            },
            "has to be gone before `app.root` is dropped",
        ),
        (
            Need::Restored { root, k: 0 },
            "has to come back after `app.root` is created",
        ),
    ] {
        let named = Named {
            need,
            described: RAW_DESCRIPTION.into(),
        };
        assert_eq!(named.to_string(), format!("{SHOWN_DESCRIPTION} {suffix}"));
        assert_eq!(named.described, RAW_DESCRIPTION);
        assert_ne!(
            named,
            Named {
                described: SHOWN_DESCRIPTION.into(),
                ..named.clone()
            }
        );
    }
}

#[test]
fn missing_source_and_unaccounted_descriptions_are_shown_once() {
    let root: ModuleId = "app.root".parse().unwrap();
    let declared = Schema::default();
    let ids = IdsFile::default();
    let sources = Sources {
        before: crate::Side {
            schema: &declared,
            ids: &ids,
        },
        after: &ids,
    };
    let found = BTreeMap::from([(
        root.clone(),
        vec![Dependent {
            described: RAW_DESCRIPTION.into(),
            holds: Holds::TablePart {
                table: TableName::new("app", "untracked"),
                part: Part::Check("ck".into()),
            },
        }],
    )]);
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(Change::DropModule {
            id: root.clone(),
            kind: ModuleKind::View,
        })],
    };
    let weave = Weave {
        roots: vec![root.clone()],
        found: ModuleAdapter::new(&found),
        declared: &declared,
        sources,
        dialect: &pbps_dialect::MinimalDialect,
        partitions: Partitions::default(),
        parents: BTreeSet::new(),
    };
    let named = Named {
        need: Need::Removed {
            root: root.clone(),
            k: 0,
        },
        described: RAW_DESCRIPTION.into(),
    };
    assert_eq!(
        weave.expand(&changes.changes, &named).unwrap_err(),
        format!("{SHOWN_DESCRIPTION}: physical source is unavailable")
    );
    let view = sources.project(&changes.changes).unwrap();
    assert_eq!(
        unaccounted(&changes, &found, &Partitions::default(), &view),
        [format!("{SHOWN_DESCRIPTION} depends on `app.root`")]
    );
    let empty = ChangeSet::default();
    let empty_view = sources.project(&empty.changes).unwrap();
    assert!(unaccounted(&empty, &found, &Partitions::default(), &empty_view).is_empty());
    assert_eq!(
        weave.dependent(&root, 0).unwrap().described,
        RAW_DESCRIPTION
    );
    assert!(weave.dependent(&root, 1).is_err());
}

#[test]
fn stored_columns_are_carried_only_when_someone_read_them() {
    use pbps_model::operation::{MembershipCatalog, StoredColumn};
    let stored = MembershipCatalog {
        columns: [(
            "app.p.d".parse().unwrap(),
            StoredColumn {
                default: Some("1".into()),
                ..Default::default()
            },
        )]
        .into(),
    };
    // Nobody asked is not the same as asked and found nothing.
    assert!(schedule::Facts::default().membership_catalog().is_none());
    let empty = MembershipCatalog::default();
    let read = schedule::Facts::default().with_membership_catalog(&empty);
    assert_eq!(read.membership_catalog(), Some(&empty));
    let read = read.with_membership_catalog(&stored);
    assert_eq!(read.membership_catalog(), Some(&stored));
    // The facts the carrier already held stay put.
    let dependents = schedule::ModuleDependents::default();
    let both = read.with_module_dependents(&dependents);
    assert_eq!(both.membership_catalog(), Some(&stored));
}

#[test]
fn indexed_creation_names_keep_foreign_keys_parents_and_row_types_hard() {
    let mut shape: pbps_model::Table = serde_json::from_value(serde_json::json!({
        "columns": {"bare": {"type": "bare_type", "nullable": true}, "qualified": {"type": "app.qualified_type", "nullable": true}},
        "foreign_keys": {"fk": {"columns": ["bare"], "references_table": "app.target", "references_columns": ["id"]}}
    })).unwrap();
    shape.partition_of = Some(pbps_model::PartitionOf {
        parent: "app.parent".parse().unwrap(),
        bound: pbps_model::PartitionBound::Default,
        columns: Default::default(),
    });
    let create = |name: &str, table| Change::CreateTable {
        uid: "t_aaaaaa".parse().unwrap(),
        name: name.parse().unwrap(),
        table: Box::new(table),
        beside_default: None,
        moves_at_most: None,
    };
    let owner = create("app.owner", shape);
    let indexed = TypedCreation::of(&owner);
    for (provider, inferred) in [
        ("app.target", false),
        ("other.target", true),
        ("app.parent", false),
        ("other.parent", true),
        ("app.bare_type", false),
        ("other.bare_type", false),
        ("app.qualified_type", false),
        ("other.qualified_type", true),
        ("app.absent", true),
    ] {
        let provider = create(provider, Default::default());
        assert_eq!(inferred_need(&owner, &provider, &indexed), inferred);
        let empty = create("app.empty", Default::default());
        assert!(inferred_need(&empty, &provider, &TypedCreation::of(&empty)));
    }
}
