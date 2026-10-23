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
