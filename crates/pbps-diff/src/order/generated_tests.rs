//! Every Change kind has an independently stated structural-use pin (DEC-1503.6).
use super::{TableUse, table_use};
use pbps_model::change::DeleteCause;
use pbps_model::*;

#[test]
fn every_change_kind_has_its_complete_shape_and_row_write_classification_pinned() {
    let table = TableName::new("app", "a");
    let parent = TableName::new("app", "parent");
    let source = TableName::new("app", "source");
    let uid: Uid = "t_015030".parse().unwrap();
    let column_uid: Uid = "c_015031".parse().unwrap();
    let column = table.column("id");
    let ty: ColumnType = "integer".parse().unwrap();
    let shape = Box::new(Table::default());
    let mut seen = std::collections::HashSet::new();
    let mut pin = |change: Change, complete: Vec<&TableName>, writes: Vec<&TableName>, seed| {
        seen.insert(std::mem::discriminant(&change));
        assert_eq!(
            table_use(&change),
            TableUse {
                complete,
                writes,
                seed
            },
            "{change:?}"
        );
    };
    let type_uid: Uid = "y_000001".parse().unwrap();
    let type_name: pbps_model::TypeRef = "app.mood".parse().unwrap();
    let definition = Box::new(pbps_model::ManagedType::Enum {
        values: vec!["ok".into()],
    });
    pin(
        Change::AddEnumValue {
            uid: type_uid.clone(),
            name: type_name.clone(),
            value: "new".into(),
            placement: None,
            ordinal: 0,
            definition: definition.clone(),
        },
        vec![],
        vec![],
        None,
    );
    // A rebuild's retype is a table's alteration; its type-wide steps use no table.
    for (step, complete) in [
        (
            EnumRebuildStep::Retype {
                column_uid: column_uid.clone(),
                column: column.clone(),
                to: pbps_model::ColumnType::reference("app.e".parse().unwrap()),
                array: false,
                values: Box::new(pbps_model::ValueChange::Relabel { removed: vec![] }),
            },
            vec![&table],
        ),
        (
            EnumRebuildStep::Swap {
                definition: definition.clone(),
                comment: None,
                depth: 0,
                over: None,
            },
            vec![],
        ),
    ] {
        pin(
            Change::EnumRebuild {
                uid: type_uid.clone(),
                name: type_name.clone(),
                tmp: "app.pbps_rebuild_y_000001".parse().unwrap(),
                step,
            },
            complete,
            vec![],
            None,
        );
    }
    // Both halves of a domain alteration name a type and no table.
    for half in [
        pbps_model::DomainHalf::Loosen,
        pbps_model::DomainHalf::Tighten,
    ] {
        pin(
            Change::AlterDomain {
                uid: type_uid.clone(),
                name: type_name.clone(),
                from: definition.clone(),
                to: definition.clone(),
                half,
            },
            vec![],
            vec![],
            None,
        );
    }
    pin(
        Change::RenameEnumValue {
            uid: type_uid.clone(),
            name: type_name.clone(),
            from: "old".into(),
            to: "new".into(),
            ordinal: 0,
            definition: definition.clone(),
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::CreateType {
            uid: type_uid.clone(),
            name: type_name.clone(),
            definition: definition.clone(),
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::DropType {
            uid: type_uid.clone(),
            name: type_name.clone(),
            definition: definition.clone(),
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::RenameType {
            uid: type_uid,
            from: type_name,
            to: "app.feeling".parse().unwrap(),
            definition,
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::CreateTable {
            uid: uid.clone(),
            name: table.clone(),
            table: shape.clone(),
            beside_default: None,
            moves_at_most: None,
        },
        vec![],
        vec![],
        None,
    );
    let mut partition = shape.clone();
    partition.partition_of = Some(PartitionOf {
        parent: parent.clone(),
        bound: PartitionBound::Default,
        columns: Default::default(),
    });
    pin(
        Change::CreateTable {
            uid: uid.clone(),
            name: table.clone(),
            table: partition.clone(),
            beside_default: None,
            moves_at_most: None,
        },
        vec![&parent],
        vec![],
        None,
    );
    pin(
        Change::CreateTable {
            uid: uid.clone(),
            name: table.clone(),
            table: partition,
            beside_default: Some(source.clone()),
            moves_at_most: Some(1),
        },
        vec![&parent],
        vec![&table, &source, &parent],
        None,
    );
    pin(
        Change::DropTable {
            uid: uid.clone(),
            name: table.clone(),
            detach_from: None,
        },
        vec![],
        vec![&table],
        None,
    );
    pin(
        Change::DropTable {
            uid: uid.clone(),
            name: table.clone(),
            detach_from: Some(parent.clone()),
        },
        vec![],
        vec![&table, &parent],
        None,
    );
    pin(
        Change::AttachPartition {
            uid: uid.clone(),
            table: table.clone(),
            parent: parent.clone(),
            bound: PartitionBound::Default,
            key: vec![],
            shape: shape.clone(),
        },
        vec![&table, &parent],
        vec![&table, &parent],
        None,
    );
    pin(
        Change::DetachPartition {
            uid: uid.clone(),
            table: table.clone(),
            parent: parent.clone(),
            names: vec![],
            shape,
        },
        vec![&table, &parent],
        vec![&table, &parent],
        None,
    );
    pin(
        Change::RenameTable {
            uid: uid.clone(),
            from: table.clone(),
            to: source.clone(),
            defaults: vec![],
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::AddColumn {
            uid: column_uid.clone(),
            table: table.clone(),
            name: "id".into(),
            column: Box::new(Column::new(ty.clone())),
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::DropColumn {
            uid: column_uid.clone(),
            column: column.clone(),
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::RenameColumn {
            uid: column_uid.clone(),
            table: table.clone(),
            from: "id".into(),
            to: "other".into(),
            table_was: None,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::AlterColumnType {
            uid: column_uid.clone(),
            column: column.clone(),
            from: ty.clone(),
            to: ty.clone(),
            from_nullable: true,
            to_nullable: true,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
            using: None,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::AlterColumnNullability {
            uid: column_uid.clone(),
            column: column.clone(),
            ty: ty.clone(),
            to_nullable: false,
            collation: None,
            through_external_partitions: false,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::AlterColumnDefault {
            uid: column_uid.clone(),
            column: column.clone(),
            from: None,
            to: Some("1".into()),
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::AlterColumnExpression {
            uid: column_uid.clone(),
            column: column.clone(),
            from: "NULL::integer".into(),
            to: "1".into(),
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::AlterColumnNumbering {
            uid: column_uid.clone(),
            column: column.clone(),
            ty,
            from: ValueSource::None,
            to: ValueSource::None,
        },
        vec![&table],
        vec![],
        None,
    );
    let computed = ComputedColumn {
        expression: "1".into(),
        persisted: true,
        not_null: false,
    };
    pin(
        Change::AddComputedColumn {
            table: table.clone(),
            name: "c".into(),
            computed: computed.clone(),
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::DropComputedColumn {
            table: table.clone(),
            name: "c".into(),
            computed,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::SetColumnDeprecated {
            uid: column_uid,
            column,
            reason: None,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::SetReplicaIdentity {
            uid: uid.clone(),
            table: table.clone(),
            to: None,
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::SetStorageParameters {
            uid: uid.clone(),
            table: table.clone(),
            set: Default::default(),
            reset: Default::default(),
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::SetTablePersistence {
            uid: uid.clone(),
            table: table.clone(),
            unlogged: true,
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::SetPartitionDefault {
            uid: uid.clone(),
            table: table.clone(),
            parent: parent.clone(),
            column: "id".into(),
            from: None,
            to: None,
            fallback: None,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::SetPartitionNotNull {
            uid: uid.clone(),
            table: table.clone(),
            column: "id".into(),
            not_null: true,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::SetIndexStorageParameters {
            table: table.clone(),
            target: IndexPart::PrimaryKey,
            method: Default::default(),
            set: Default::default(),
            reset: Default::default(),
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::SetPrimaryKey {
            table: table.clone(),
            from: None,
            to: None,
            nonclustered: false,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::AddUnique {
            table: table.clone(),
            name: "uq".into(),
            constraint: UniqueConstraint {
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            },
            clustered: false,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::DropUnique {
            table: table.clone(),
            name: "uq".into(),
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::AddForeignKey {
            table: table.clone(),
            name: "fk".into(),
            constraint: Box::new(ForeignKey {
                columns: vec!["id".into()],
                references_table: parent.clone(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            }),
        },
        vec![&table, &parent],
        vec![],
        None,
    );
    pin(
        Change::DropForeignKey {
            table: table.clone(),
            name: "fk".into(),
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::AddCheck {
            table: table.clone(),
            name: "ck".into(),
            constraint: CheckConstraint {
                expression: "id > 0".into(),
            },
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::DropCheck {
            table: table.clone(),
            name: "ck".into(),
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::AddIndex {
            table: table.clone(),
            name: "ix".into(),
            index: Box::new(Index {
                columns: vec![],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            }),
            clustered: false,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::DropIndex {
            table: table.clone(),
            name: "ix".into(),
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::InsertRow {
            table: table.clone(),
            key_column: "id".into(),
            identity_key: false,
            key: "1".into(),
            row: Row::default(),
            defaults: Default::default(),
            types: Default::default(),
        },
        vec![&table],
        vec![&table],
        Some(&table),
    );
    pin(
        Change::UpdateRow {
            table: table.clone(),
            key_column: "id".into(),
            key: "1".into(),
            columns: Default::default(),
            unchanged: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
            key_type: None,
        },
        vec![&table],
        vec![&table],
        Some(&table),
    );
    pin(
        Change::DeleteRow {
            table: table.clone(),
            key_column: "id".into(),
            key: "1".into(),
            cause: DeleteCause::Undeclared,
            row: Default::default(),
            dropped: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
            key_type: None,
        },
        vec![&table],
        vec![&table],
        Some(&table),
    );
    pin(
        Change::SetDataMode {
            table: table.clone(),
            from: None,
            to: None,
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::SetEnsureKeys {
            table: table.clone(),
            from: Default::default(),
            to: Default::default(),
        },
        vec![],
        vec![],
        None,
    );
    let function: ModuleId = "app.f(integer)".parse().unwrap();
    let module = Box::new(Module {
        kind: ModuleKind::Function,
        description: None,
        definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
        options: Default::default(),
    });
    pin(
        Change::CreateModule {
            id: function.clone(),
            module: module.clone(),
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::AlterModule {
            id: function.clone(),
            module: module.clone(),
        },
        vec![],
        vec![],
        None,
    );
    let trigger = ModuleId::Trigger {
        on: table.clone(),
        name: "writes_rows".into(),
    };
    let mut trigger_module = module.clone();
    trigger_module.kind = ModuleKind::Trigger;
    trigger_module.definition =
        "AFTER INSERT ON app.a FOR EACH ROW EXECUTE FUNCTION app.write_rows()".into();
    pin(
        Change::CreateModule {
            id: trigger.clone(),
            module: trigger_module.clone(),
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::AlterModule {
            id: trigger,
            module: trigger_module,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::DropModule {
            id: function,
            kind: ModuleKind::Function,
        },
        vec![],
        vec![],
        None,
    );
    let role_uid: Uid = "r_015032".parse().unwrap();
    pin(
        Change::CreateRole {
            uid: role_uid.clone(),
            name: "reader".into(),
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::DropRole {
            uid: role_uid.clone(),
            name: "reader".into(),
            members: vec![],
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::RenameRole {
            uid: role_uid,
            from: "reader".into(),
            to: "writer".into(),
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::Grant {
            role: "reader".into(),
            target: GrantTarget::Schema("app".into()),
            permissions: Default::default(),
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::Revoke {
            role: "reader".into(),
            target: GrantTarget::Schema("app".into()),
            permissions: Default::default(),
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::PublicExecution {
            routine: "app.f(integer)".parse().unwrap(),
            access: PublicAccess::Kept,
            origin: RoutineOrigin::Created,
        },
        vec![],
        vec![],
        None,
    );

    pin(
        Change::DropPolicy {
            table: table.clone(),
            name: "visible".into(),
            policy: RowPolicy {
                using: Some("id > 0".into()),
                ..Default::default()
            },
        },
        vec![],
        vec![],
        None,
    );
    pin(
        Change::CreatePolicy {
            table: table.clone(),
            name: "visible".into(),
            policy: RowPolicy {
                using: Some("id > 0".into()),
                ..Default::default()
            },
            origin: RowSecurityOrigin::Created,
        },
        vec![&table],
        vec![],
        None,
    );
    pin(
        Change::SetRowSecurity {
            uid,
            table: table.clone(),
            from: RowSecurity::default(),
            to: RowSecurity {
                enabled: true,
                forced: true,
            },
            origin: RowSecurityOrigin::Created,
        },
        vec![&table],
        vec![],
        None,
    );

    // A missing pin must fail even if a new variant was classified to compile.
    let source = include_str!("../../../pbps-model/src/change.rs");
    let variants = source
        .split("pub enum Change {")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap()
        .lines()
        .filter(|line| {
            line.starts_with("    ")
                && !line.starts_with("        ")
                && line.trim_end().ends_with(" {")
                && !line.trim_start().starts_with('/')
        })
        .count();
    assert_eq!(
        seen.len(),
        variants,
        "each Change kind needs a classification pin"
    );
}

#[test]
fn policy_setup_waits_for_complete_generated_columns_without_invalidating_emptiness() {
    let table = TableName::new("app", "a");
    let uid: Uid = "t_011720".parse().unwrap();
    let column = Column::new("integer".parse().unwrap());
    let mut full = Table::default();
    full.columns.insert("id".into(), column.clone());
    let mut generated = column;
    generated.value_source = ValueSource::Generated(Generated {
        expression: "id * 2".into(),
        stored: true,
    });
    full.columns.insert("g".into(), generated.clone());
    let declared = Schema {
        tables: [(table.clone(), full.clone())].into(),
        ..Default::default()
    };
    full.columns.shift_remove(&ColumnName::from("g"));
    let part = |change| {
        let mut p = PlannedChange::new(change);
        p.provenance = Some(Provenance {
            rule: super::RULE,
            role: AddedRole::SplitPart,
            source: "app.a".into(),
            reason: "generated suffix".into(),
        });
        p
    };
    let policy = |owner| {
        PlannedChange::new(Change::CreatePolicy {
            table: owner,
            name: "visible".into(),
            policy: RowPolicy {
                using: Some("g > 0".into()),
                with_check: Some("g < 10".into()),
                ..Default::default()
            },
            origin: RowSecurityOrigin::Created,
        })
    };
    let cs = ChangeSet {
        changes: vec![
            part(Change::CreateTable {
                uid: uid.clone(),
                name: table.clone(),
                table: Box::new(full),
                beside_default: None,
                moves_at_most: None,
            }),
            policy(table.clone()),
            PlannedChange::new(Change::SetRowSecurity {
                uid,
                table: table.clone(),
                from: RowSecurity::default(),
                to: RowSecurity {
                    enabled: true,
                    forced: true,
                },
                origin: RowSecurityOrigin::Created,
            }),
            policy(TableName::new("app", "other")),
            part(Change::AddColumn {
                uid: "c_011722".parse().unwrap(),
                table,
                name: "g".into(),
                column: Box::new(generated),
            }),
        ],
    };
    let pg = pbps_dialect::MinimalDialect;
    let constraints = super::constraints(&cs, &declared, &ModuleDeps::new(), &pg);
    assert!(constraints.iter().any(|e| e.before == 4 && e.after == 1));
    assert!(constraints.iter().any(|e| e.before == 4 && e.after == 2));
    assert!(!constraints.iter().any(|e| e.before == 4 && e.after == 3));
    assert_eq!(super::texts(&cs.changes[1].change), vec!["g > 0", "g < 10"]);
    assert!(super::texts(&cs.changes[2].change).is_empty());
}

#[test]
fn every_policy_replacement_carries_both_risks_including_mode_flips() {
    let table = TableName::new("app", "docs");
    let uid: Uid = "t_117300".parse().unwrap();
    let mut ids = IdsFile::default();
    ids.tables.insert(uid, table.clone());
    for restrictive in [false, true] {
        let old = RowPolicy {
            restrictive,
            using: Some("id > 0".into()),
            with_check: Some("id < 100".into()),
            ..Default::default()
        };
        for field in 0..6 {
            let mut new = old.clone();
            match field {
                0 => new.command = PolicyCommand::Update,
                1 => new.restrictive = !restrictive,
                2 => new.to = PolicyRoles::Roles(["app_user".into()].into()),
                3 => new.using = Some("id > 5".into()),
                4 => new.with_check = Some("id < 50".into()),
                _ => new.using = None,
            }
            let cs = ChangeSet {
                changes: vec![
                    PlannedChange::new(Change::DropPolicy {
                        table: table.clone(),
                        name: "p".into(),
                        policy: old.clone(),
                    }),
                    PlannedChange::new(Change::CreatePolicy {
                        table: table.clone(),
                        name: "p".into(),
                        policy: new,
                        origin: RowSecurityOrigin::Standing,
                    }),
                ],
            };
            let both = [RiskClass::Revoke, RiskClass::RowSecurity].into();
            assert_eq!(
                super::risks(&cs, &ids, &pbps_dialect::MinimalDialect),
                vec![both; 2],
                "{restrictive}, field {field}"
            );
        }
    }
}

#[test]
fn policy_replacement_follows_table_uid_across_rename_but_not_name_reuse() {
    let a = TableName::new("app", "a");
    let b = TableName::new("app", "b");
    let old_name = TableName::new("app", "old");
    let first: Uid = "t_117300".parse().unwrap();
    let second: Uid = "t_117301".parse().unwrap();
    let policy = RowPolicy {
        using: Some("id > 0".into()),
        ..Default::default()
    };
    let drop = Change::DropPolicy {
        table: a.clone(),
        name: "p".into(),
        policy: policy.clone(),
    };
    let create = |table, name: &str| Change::CreatePolicy {
        table,
        name: name.into(),
        policy: RowPolicy {
            restrictive: true,
            ..policy.clone()
        },
        origin: RowSecurityOrigin::Standing,
    };
    let pg = pbps_dialect::MinimalDialect;
    let cs = |changes: Vec<Change>| ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    };
    let mut ids = IdsFile::default();
    ids.tables.insert(first.clone(), b.clone());
    let renamed = cs(vec![
        drop.clone(),
        Change::RenameTable {
            uid: first.clone(),
            from: a.clone(),
            to: b.clone(),
            defaults: vec![],
        },
        create(b.clone(), "p"),
    ]);
    let risks = super::risks(&renamed, &ids, &pg);
    let both = [RiskClass::Revoke, RiskClass::RowSecurity].into();
    assert_eq!(risks[0], both);
    assert_eq!(risks[2], both);
    ids.tables.insert(first.clone(), old_name.clone());
    ids.tables.insert(second.clone(), a.clone());
    let handed = cs(vec![
        drop.clone(),
        Change::RenameTable {
            uid: first.clone(),
            from: a.clone(),
            to: old_name,
            defaults: vec![],
        },
        Change::RenameTable {
            uid: second.clone(),
            from: b.clone(),
            to: a.clone(),
            defaults: vec![],
        },
        create(a.clone(), "p"),
    ]);
    let risks = super::risks(&handed, &ids, &pg);
    assert_eq!(risks[0], [RiskClass::Revoke].into());
    assert_eq!(risks[3], [RiskClass::Revoke].into());
    ids.tables.insert(first, a.clone());
    ids.tables.insert(second, b.clone());
    for other in [create(b, "p"), create(a, "other")] {
        let risks = super::risks(&cs(vec![drop.clone(), other]), &ids, &pg);
        assert_eq!(risks, vec![[RiskClass::Revoke].into(); 2]);
    }
}
