use super::*;
use pbps_model::{
    Change, Column, ColumnType, Module, ModuleKind, ModuleOptions, PlannedChange, Severity, Table,
    TableName, Uid, ValueSource,
};

fn uid(s: &str) -> Uid {
    s.parse().unwrap()
}
fn name() -> TableName {
    "app.t".parse().unwrap()
}
fn schema() -> Schema {
    let mut table = Table::default();
    table
        .columns
        .insert("old".into(), Column::new(ColumnType::simple("integer")));
    Schema {
        tables: [(name(), table)].into(),
        ..Default::default()
    }
}
fn ids() -> IdsFile {
    IdsFile {
        tables: [(uid("t_aaaaaa"), name())].into(),
        columns: [(uid("c_aaaaaa"), name().column("old"))].into(),
        ..Default::default()
    }
}
fn consumers(text: &str, writes: bool) -> Consumers {
    [(
        "billing-v41".into(),
        Consumer {
            uses: if writes { vec![] } else { vec![text.into()] },
            writes: if writes { vec![text.into()] } else { vec![] },
            ..Default::default()
        },
    )]
    .into()
}
fn context() -> Context {
    Context {
        now: 1_788_429_600,
        max_rows: 1000,
        connected: false,
        only: None,
        deprecated_types: &[],
    }
}
fn findings(
    before: &Schema,
    after: &Schema,
    old: &IdsFile,
    new: &IdsFile,
    consumers: &Consumers,
    changes: Vec<Change>,
) -> Vec<(usize, Finding)> {
    let sides = [
        Side {
            schema: before,
            ids: old,
        },
        Side {
            schema: after,
            ids: new,
        },
    ];
    let bound = Bindings::resolve(consumers, &sides, &Clone::clone).unwrap();
    plan(
        &ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        },
        &bound,
        &sides,
        &Policies::default(),
        &context(),
    )
}

#[test]
fn a_renamed_entry_still_protects_the_same_column_uid() {
    let before = schema();
    let mut after = before.clone();
    let mut new = ids();
    let column = after
        .tables
        .get_mut(&name())
        .unwrap()
        .columns
        .shift_remove("old")
        .unwrap();
    after
        .tables
        .get_mut(&name())
        .unwrap()
        .columns
        .insert("new".into(), column);
    new.columns.insert(uid("c_aaaaaa"), name().column("new"));
    let change = Change::RenameColumn {
        uid: uid("c_aaaaaa"),
        table: name(),
        from: "old".into(),
        to: "new".into(),
        table_was: None,
    };
    for spelling in ["app.t.old", "app.t.new"] {
        let f = findings(
            &before,
            &after,
            &ids(),
            &new,
            &consumers(spelling, false),
            vec![change.clone()],
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].1.id, "consumer.breaking");
        assert!(
            f[0].1
                .message
                .contains(&format!("consumers.billing-v41.uses: {spelling}"))
        );
    }
    assert!(
        findings(
            &before,
            &after,
            &ids(),
            &new,
            &Consumers::default(),
            vec![change]
        )
        .is_empty()
    );
}

#[test]
fn missing_ambiguous_and_nonwritable_targets_are_errors_even_outside_since() {
    let mut s = schema();
    assert!(
        validate(&consumers("app.missing", false), &s, &Clone::clone)[0]
            .contains("no declared object")
    );
    let module = Module {
        kind: ModuleKind::View,
        definition: "SELECT 1".into(),
        description: None,
        options: ModuleOptions::default(),
    };
    s.modules.insert(ModuleId::Named(name()), module);
    let errors = validate(&consumers("app.t", false), &s, &Clone::clone);
    assert!(
        errors[0].contains("ambiguous")
            && errors[0].contains("table app.t")
            && errors[0].contains("module app.t"),
        "{errors:?}"
    );
    s.tables.clear();
    assert!(validate(&consumers("app.t", true), &s, &Clone::clone)[0].contains("writable column"));
    assert!(validate(&consumers("app.t", false), &s, &Clone::clone).is_empty());
    assert!(
        validate(&consumers("nonsense", false), &s, &Clone::clone)[0]
            .contains("invalid object reference")
    );
}

#[test]
fn writes_tightening_is_unproven_and_a_new_default_supplied_column_is_compatible() {
    let before = schema();
    let mut after = before.clone();
    let mut new = ids();
    let change = Change::AlterColumnNullability {
        uid: uid("c_aaaaaa"),
        column: name().column("old"),
        ty: ColumnType::simple("integer"),
        to_nullable: false,
        collation: None,
        through_external_partitions: false,
    };
    let f = findings(
        &before,
        &after,
        &ids(),
        &new,
        &consumers("app.t", true),
        vec![change.clone()],
    );
    assert_eq!(f[0].1.id, "consumer.unproven");
    assert_eq!(f[0].1.severity, Severity::Error);
    assert!(f[0].1.message.contains("cannot be proven compatible"));
    assert!(f[0].1.message.contains("app.t.old") && f[0].1.message.contains("NOT NULL"));
    assert!(
        findings(
            &before,
            &after,
            &ids(),
            &new,
            &consumers("app.t", false),
            vec![change]
        )
        .is_empty()
    );
    let mut added = Column::new(ColumnType::simple("integer")).not_null();
    added.value_source = ValueSource::Default("0".into());
    new.columns.insert(uid("c_bbbbbb"), name().column("added"));
    after
        .tables
        .get_mut(&name())
        .unwrap()
        .columns
        .insert("added".into(), added.clone());
    let add = Change::AddColumn {
        uid: uid("c_bbbbbb"),
        table: name(),
        name: "added".into(),
        column: Box::new(added),
    };
    assert!(
        findings(
            &before,
            &after,
            &ids(),
            &new,
            &consumers("app.t", true),
            vec![add.clone()]
        )
        .is_empty()
    );
    after
        .tables
        .get_mut(&name())
        .unwrap()
        .columns
        .get_mut("added")
        .unwrap()
        .value_source = ValueSource::None;
    let f = findings(
        &before,
        &after,
        &ids(),
        &new,
        &consumers("app.t", true),
        vec![add],
    );
    assert_eq!(f[0].1.id, "consumer.unproven");
}

#[test]
fn suppressions_expire_at_the_configured_severity_and_never_hide_validation() {
    let s = schema();
    let i = ids();
    let sides = [Side {
        schema: &s,
        ids: &i,
    }; 2];
    let bound = Bindings::resolve(&consumers("app.t.old", true), &sides, &Clone::clone).unwrap();
    let cs = ChangeSet {
        changes: vec![PlannedChange::new(Change::DropColumn {
            uid: uid("c_aaaaaa"),
            column: name().column("old"),
        })],
    };
    let mut policies = Policies::default();
    policies.rules.insert(
        "consumer.breaking".into(),
        crate::config::RuleSetting::Word("warning".into()),
    );
    policies.suppress.push(crate::Suppression {
        rule: "consumer.breaking".into(),
        on: Some("app.t.old".into()),
        reason: "retiring".into(),
        until: Some("2026-09-04".into()),
    });
    let mut ctx = context();
    assert!(plan(&cs, &bound, &sides, &policies, &ctx).is_empty());
    ctx.now += 86400;
    assert_eq!(
        plan(&cs, &bound, &sides, &policies, &ctx)[0].1.severity,
        Severity::Warning
    );
    policies.suppress[0].on = Some("app.other".into());
    assert_eq!(plan(&cs, &bound, &sides, &policies, &context()).len(), 1);
    assert!(!validate(&consumers("app.missing", false), &s, &Clone::clone).is_empty());
}

#[test]
fn module_changes_are_unproven_but_an_identical_restoration_is_compatible() {
    let mut s = schema();
    let m: ModuleId = "app.v".parse().unwrap();
    let module = Module {
        kind: ModuleKind::View,
        definition: "SELECT old FROM app.t".into(),
        description: None,
        options: ModuleOptions::default(),
    };
    s.modules.insert(m.clone(), module.clone());
    let drop = Change::DropModule {
        id: m.clone(),
        kind: ModuleKind::View,
    };
    assert!(
        findings(
            &s,
            &s,
            &ids(),
            &ids(),
            &consumers("app.v", false),
            vec![
                drop.clone(),
                Change::CreateModule {
                    id: m.clone(),
                    module: Box::new(module.clone())
                }
            ]
        )
        .is_empty()
    );
    let mut after = s.clone();
    after.modules.get_mut(&m).unwrap().definition = "SELECT 1".into();
    assert_eq!(
        findings(
            &s,
            &after,
            &ids(),
            &ids(),
            &consumers("app.v", false),
            vec![Change::AlterModule {
                id: m.clone(),
                module: Box::new(after.modules[&m].clone())
            }]
        )[0]
        .1
        .id,
        "consumer.unproven"
    );
    after.modules.clear();
    assert_eq!(
        findings(
            &s,
            &after,
            &ids(),
            &ids(),
            &consumers("app.v", false),
            vec![drop]
        )[0]
        .1
        .id,
        "consumer.breaking"
    );
}

#[test]
fn constraints_restrict_writers_but_not_readers_or_unrelated_tables() {
    let s = schema();
    let i = ids();
    let key = pbps_model::UniqueConstraint {
        columns: vec!["old".into()],
        storage_parameters: Default::default(),
    };
    let cases = vec![
        Change::AddUnique {
            table: name(),
            name: "uq".into(),
            constraint: key,
            clustered: false,
        },
        Change::AddCheck {
            table: name(),
            name: "ck".into(),
            constraint: pbps_model::CheckConstraint {
                expression: "old > 0".into(),
            },
        },
        Change::AddForeignKey {
            table: name(),
            name: "fk".into(),
            constraint: Box::new(pbps_model::ForeignKey {
                columns: vec!["old".into()],
                references_table: name(),
                references_columns: vec!["old".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            }),
        },
    ];
    for change in cases {
        let mut after = s.clone();
        let target = after.tables.get_mut(&name()).unwrap();
        if let Change::AddUnique {
            name, constraint, ..
        } = &change
        {
            target.unique.insert(name.clone(), constraint.clone());
        }
        if let Change::AddCheck {
            name, constraint, ..
        } = &change
        {
            target.checks.insert(name.clone(), constraint.clone());
        }
        if let Change::AddForeignKey {
            name, constraint, ..
        } = &change
        {
            target
                .foreign_keys
                .insert(name.clone(), *constraint.clone());
        }
        assert_eq!(
            findings(
                &s,
                &after,
                &i,
                &i,
                &consumers("app.t.old", true),
                vec![change.clone()]
            )[0]
            .1
            .id,
            "consumer.unproven"
        );
        assert!(
            findings(
                &s,
                &after,
                &i,
                &i,
                &consumers("app.t.old", false),
                vec![change]
            )
            .is_empty()
        );
    }
    let unrelated = Change::AddCheck {
        table: "app.other".parse().unwrap(),
        name: "ck".into(),
        constraint: pbps_model::CheckConstraint {
            expression: "x > 0".into(),
        },
    };
    assert!(findings(&s, &s, &i, &i, &consumers("app.t", true), vec![unrelated]).is_empty());
}

#[test]
fn table_renames_preserve_bindings_and_a_reused_name_is_not_that_table() {
    let before = schema();
    let mut after = before.clone();
    let mut new = ids();
    let renamed: TableName = "app.renamed".parse().unwrap();
    let shape = after.tables.remove(&name()).unwrap();
    after.tables.insert(renamed.clone(), shape.clone());
    new.tables.insert(uid("t_aaaaaa"), renamed.clone());
    new.columns.insert(uid("c_aaaaaa"), renamed.column("old"));
    let rename = Change::RenameTable {
        uid: uid("t_aaaaaa"),
        from: name(),
        to: renamed.clone(),
        defaults: vec![],
    };
    let f = findings(
        &before,
        &after,
        &ids(),
        &new,
        &consumers("app.renamed.old", false),
        vec![rename.clone()],
    );
    assert_eq!(f[0].1.id, "consumer.breaking");
    after.tables.insert(name(), shape.clone());
    new.tables.insert(uid("t_bbbbbb"), name());
    new.columns.insert(uid("c_bbbbbb"), name().column("old"));
    let sides = [
        Side {
            schema: &before,
            ids: &ids(),
        },
        Side {
            schema: &after,
            ids: &new,
        },
    ];
    assert!(
        Bindings::resolve(&consumers("app.t", false), &sides, &Clone::clone).unwrap_err()[0]
            .contains("ambiguous")
    );
    let cases = vec![
        rename,
        Change::CreateTable {
            uid: uid("t_bbbbbb"),
            name: name(),
            table: Box::new(shape),
            beside_default: None,
            moves_at_most: None,
        },
        Change::AddCheck {
            table: name(),
            name: "ck".into(),
            constraint: pbps_model::CheckConstraint {
                expression: "old > 0".into(),
            },
        },
    ];
    let f = findings(
        &before,
        &after,
        &ids(),
        &new,
        &consumers("app.renamed", true),
        cases,
    );
    assert_eq!(
        f.len(),
        1,
        "the new table's CHECK must not attach to the renamed table: {f:?}"
    );
    assert_eq!(f[0].1.id, "consumer.breaking");
}

#[test]
fn unchanged_constraint_restorations_do_not_accuse_existing_writers() {
    let mut s = schema();
    let i = ids();
    let constraint = pbps_model::CheckConstraint {
        expression: "old > 0".into(),
    };
    s.tables
        .get_mut(&name())
        .unwrap()
        .checks
        .insert("ck".into(), constraint.clone());
    let restore = Change::AddCheck {
        table: name(),
        name: "ck".into(),
        constraint: pbps_model::CheckConstraint {
            expression: "((old)>(0))".into(),
        },
    };
    assert!(findings(&s, &s, &i, &i, &consumers("app.t", true), vec![restore]).is_empty());
}

#[test]
fn inherited_column_entries_see_parent_column_changes() {
    let mut s = schema();
    let mut i = ids();
    let leaf: TableName = "app.leaf".parse().unwrap();
    let parent = s.tables.get_mut(&name()).unwrap();
    parent.partition_by = Some(pbps_model::PartitionBy {
        strategy: Default::default(),
        columns: vec!["old".into()],
        external: false,
    });
    let table = Table {
        partition_of: Some(pbps_model::PartitionOf {
            parent: name(),
            bound: pbps_model::PartitionBound::Default,
            columns: Default::default(),
        }),
        ..Default::default()
    };
    s.tables.insert(leaf.clone(), table);
    i.tables.insert(uid("t_bbbbbb"), leaf);
    let change = Change::DropColumn {
        uid: uid("c_aaaaaa"),
        column: name().column("old"),
    };
    assert_eq!(
        findings(
            &s,
            &s,
            &i,
            &i,
            &consumers("app.leaf.old", false),
            vec![change]
        )[0]
        .1
        .id,
        "consumer.breaking"
    );
    s.tables
        .get_mut(&name())
        .unwrap()
        .columns
        .get_mut("old")
        .unwrap()
        .value_source = ValueSource::Generated(pbps_model::Generated {
        expression: "1".into(),
        stored: true,
    });
    assert!(
        validate(&consumers("app.leaf.old", true), &s, &Clone::clone)[0]
            .contains("writable column")
    );
}

#[test]
fn punctuation_does_not_hide_a_declared_relation_or_an_ambiguous_routine() {
    let mut s = Schema::default();
    let relation: TableName = "app.f(integer)".parse().unwrap();
    s.tables.insert(relation, Table::default());
    assert!(validate(&consumers("app.f(integer)", false), &s, &Clone::clone).is_empty());
    s.modules.insert(
        "app.f(integer)".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            options: Default::default(),
            definition: "opaque".into(),
        },
    );
    let errors = validate(&consumers("app.f(integer)", false), &s, &Clone::clone);
    assert!(
        errors[0].contains("ambiguous")
            && errors[0].contains("table app.f(integer)")
            && errors[0].contains("module app.f(integer)"),
        "{errors:?}"
    );
}
