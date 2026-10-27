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
// Policy unit cases supply the differ's answer; CLI tests exercise its real
// canonicalization. Do not reimplement restoration equivalence in this crate.
fn unchanged(_: &Change, _: Option<&Uid>) -> bool {
    false
}

fn findings(
    before: &Schema,
    after: &Schema,
    old: &IdsFile,
    new: &IdsFile,
    consumers: &Consumers,
    changes: Vec<Change>,
) -> Vec<(usize, Finding)> {
    findings_with_restoration(before, after, old, new, consumers, changes, false)
}
fn findings_with_restoration(
    before: &Schema,
    after: &Schema,
    old: &IdsFile,
    new: &IdsFile,
    consumers: &Consumers,
    changes: Vec<Change>,
    restored: bool,
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
        &|_, _| restored,
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
    assert!(plan(&cs, &bound, &sides, &policies, &ctx, &unchanged).is_empty());
    ctx.now += 86400;
    assert_eq!(
        plan(&cs, &bound, &sides, &policies, &ctx, &unchanged)[0]
            .1
            .severity,
        Severity::Warning
    );
    policies.suppress[0].on = Some("app.other".into());
    assert_eq!(
        plan(&cs, &bound, &sides, &policies, &context(), &unchanged).len(),
        1
    );
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
    let mut described = s.clone();
    described.modules.get_mut(&m).unwrap().description = Some("consumer report".into());
    assert!(
        findings_with_restoration(
            &s,
            &described,
            &ids(),
            &ids(),
            &consumers("app.v", false),
            vec![
                drop.clone(),
                Change::CreateModule {
                    id: m.clone(),
                    module: Box::new(module.clone())
                }
            ],
            true,
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
    assert!(
        findings_with_restoration(
            &s,
            &s,
            &i,
            &i,
            &consumers("app.t", true),
            vec![restore],
            true
        )
        .is_empty()
    );
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

#[test]
fn written_tables_include_sibling_columns_but_not_unrelated_tables() {
    let mut before = schema();
    let mut i = ids();
    before
        .tables
        .get_mut(&name())
        .unwrap()
        .columns
        .insert("other".into(), Column::new(ColumnType::simple("integer")));
    let unrelated: TableName = "app.unrelated".parse().unwrap();
    before
        .tables
        .insert(unrelated.clone(), before.tables[&name()].clone());
    i.tables.insert(uid("t_bbbbbb"), unrelated.clone());
    i.columns.insert(uid("c_bbbbbb"), name().column("other"));
    i.columns.insert(uid("c_cccccc"), unrelated.column("other"));
    for (table, column_uid, expected) in [
        (name(), uid("c_bbbbbb"), 1),
        (unrelated, uid("c_cccccc"), 0),
    ] {
        for folded in [false, true] {
            let mut after = before.clone();
            let column = after
                .tables
                .get_mut(&table)
                .unwrap()
                .columns
                .get_mut("other")
                .unwrap();
            column.nullable = false;
            let change = if folded {
                column.ty = ColumnType::simple("bigint");
                Change::AlterColumnType {
                    uid: column_uid.clone(),
                    column: table.column("other"),
                    from: ColumnType::simple("integer"),
                    to: column.ty.clone(),
                    from_nullable: true,
                    to_nullable: false,
                    from_collation: None,
                    to_collation: None,
                    through_external_partitions: false,
                    using: None,
                }
            } else {
                Change::AlterColumnNullability {
                    uid: column_uid.clone(),
                    column: table.column("other"),
                    ty: column.ty.clone(),
                    to_nullable: false,
                    collation: None,
                    through_external_partitions: false,
                }
            };
            let f = findings(
                &before,
                &after,
                &i,
                &i,
                &consumers("app.t.old", true),
                vec![change],
            );
            assert_eq!(f.len(), expected, "folded={folded}, table={table:?}: {f:?}");
            assert!(f.iter().all(|(_, f)| f.id == "consumer.unproven"));
        }
    }
}

#[test]
fn written_tables_include_the_whole_partition_tree_by_uid() {
    let mut before = schema();
    let mut i = ids();
    let branch: TableName = "app.branch".parse().unwrap();
    let leaf: TableName = "app.leaf".parse().unwrap();
    let sibling: TableName = "app.sibling".parse().unwrap();
    let unrelated: TableName = "app.unrelated".parse().unwrap();
    before.tables.get_mut(&name()).unwrap().partition_by = Some(pbps_model::PartitionBy {
        strategy: Default::default(),
        columns: vec!["old".into()],
        external: false,
    });
    for (child, parent, id) in [
        (branch.clone(), name(), "t_bbbbbb"),
        (leaf.clone(), branch.clone(), "t_cccccc"),
        (sibling.clone(), name(), "t_dddddd"),
    ] {
        let mut shape = Table {
            partition_of: Some(pbps_model::PartitionOf {
                parent,
                bound: pbps_model::PartitionBound::Default,
                columns: Default::default(),
            }),
            ..Default::default()
        };
        if child == branch {
            shape.partition_by = before.tables[&name()].partition_by.clone();
        }
        before.tables.insert(child.clone(), shape);
        i.tables.insert(uid(id), child);
    }
    before
        .tables
        .insert(unrelated.clone(), before.tables[&name()].clone());
    i.tables.insert(uid("t_eeeeee"), unrelated.clone());
    for table in [name(), leaf, sibling, unrelated.clone()] {
        let mut after = before.clone();
        let constraint = pbps_model::CheckConstraint {
            expression: "old > 0".into(),
        };
        after
            .tables
            .get_mut(&table)
            .unwrap()
            .checks
            .insert("ck".into(), constraint.clone());
        let change = Change::AddCheck {
            table: table.clone(),
            name: "ck".into(),
            constraint,
        };
        for entry in ["app.leaf", "app.branch.old", "app.t"] {
            let f = findings(
                &before,
                &after,
                &i,
                &i,
                &consumers(entry, true),
                vec![change.clone()],
            );
            assert_eq!(
                f.len(),
                usize::from(table != unrelated),
                "entry={entry}, table={table:?}: {f:?}"
            );
            assert!(f.iter().all(|(_, f)| f.id == "consumer.unproven"));
            assert!(
                findings_with_restoration(
                    &after,
                    &after,
                    &i,
                    &i,
                    &consumers(entry, true),
                    vec![change.clone()],
                    true,
                )
                .is_empty(),
                "an unchanged inherited constraint restoration is compatible"
            );
        }
    }
}

#[test]
fn unrelated_write_entries_keep_their_own_findings_and_suppressions() {
    let mut before = schema();
    let mut i = ids();
    let second: TableName = "app.second".parse().unwrap();
    before
        .tables
        .insert(second.clone(), before.tables[&name()].clone());
    i.tables.insert(uid("t_bbbbbb"), second.clone());
    i.columns.insert(uid("c_bbbbbb"), second.column("old"));
    let mut after = before.clone();
    let constraint = pbps_model::CheckConstraint {
        expression: "old > 0".into(),
    };
    after
        .tables
        .get_mut(&second)
        .unwrap()
        .checks
        .insert("ck".into(), constraint.clone());
    let mut consumers = consumers("app.t.old", true);
    consumers
        .get_mut("billing-v41")
        .unwrap()
        .writes
        .push("app.second.old".into());
    let sides = [
        Side {
            schema: &before,
            ids: &i,
        },
        Side {
            schema: &after,
            ids: &i,
        },
    ];
    let bound = Bindings::resolve(&consumers, &sides, &Clone::clone).unwrap();
    let cs = ChangeSet {
        changes: vec![PlannedChange::new(Change::AddCheck {
            table: second,
            name: "ck".into(),
            constraint,
        })],
    };
    let mut policies = Policies::default();
    let f = plan(&cs, &bound, &sides, &policies, &context(), &unchanged);
    assert_eq!(f.len(), 1, "only the second entry is affected: {f:?}");
    assert!(
        f[0].1
            .message
            .contains("consumers.billing-v41.writes: app.second.old")
    );
    policies.suppress.push(crate::Suppression {
        rule: "consumer.unproven".into(),
        on: Some("app.second.old".into()),
        reason: "reviewed compatibility exception".into(),
        until: Some("2099-01-01".into()),
    });
    assert!(plan(&cs, &bound, &sides, &policies, &context(), &unchanged).is_empty());
}

fn partitioned() -> (Schema, IdsFile, TableName) {
    let mut s = schema();
    let mut i = ids();
    let leaf: TableName = "app.leaf".parse().unwrap();
    s.tables.get_mut(&name()).unwrap().partition_by = Some(pbps_model::PartitionBy {
        strategy: Default::default(),
        columns: vec!["old".into()],
        external: false,
    });
    s.tables.insert(
        leaf.clone(),
        Table {
            partition_of: Some(pbps_model::PartitionOf {
                parent: name(),
                bound: pbps_model::PartitionBound::Default,
                columns: Default::default(),
            }),
            ..Default::default()
        },
    );
    i.tables.insert(uid("t_bbbbbb"), leaf.clone());
    (s, i, leaf)
}

#[test]
fn partition_column_bindings_survive_attachment_and_detachment() {
    let (attached, inherited, leaf) = partitioned();
    let mut detached = attached.clone();
    let shape = schema().tables[&name()].clone();
    detached.tables.insert(leaf.clone(), shape.clone());
    let mut owned = inherited.clone();
    owned.columns.insert(uid("c_bbbbbb"), leaf.column("old"));
    for attach in [false, true] {
        let (before, after, old, new) = if attach {
            (&detached, &attached, &owned, &inherited)
        } else {
            (&attached, &detached, &inherited, &owned)
        };
        let change = if attach {
            Change::AttachPartition {
                uid: uid("t_bbbbbb"),
                table: leaf.clone(),
                parent: name(),
                bound: pbps_model::PartitionBound::Default,
                key: vec![],
                shape: Box::new(attached.tables[&leaf].clone()),
            }
        } else {
            Change::DetachPartition {
                uid: uid("t_bbbbbb"),
                table: leaf.clone(),
                parent: name(),
                names: vec![],
                shape: Box::new(shape.clone()),
            }
        };
        let cs = consumers("app.leaf.old", false);
        assert!(findings(before, after, old, new, &cs, vec![change]).is_empty());
        // Joining identities must not stop a parent-column break reaching the leaf.
        let f = findings(
            before,
            after,
            old,
            new,
            &cs,
            vec![Change::DropColumn {
                uid: uid("c_aaaaaa"),
                column: name().column("old"),
            }],
        );
        assert_eq!(f[0].1.id, "consumer.breaking");
    }
}

#[test]
fn partition_overrides_share_the_supplied_new_column_exception() {
    let (before, old, leaf) = partitioned();
    for parent_default in [false, true] {
        for local_default in [false, true] {
            let mut after = before.clone();
            let mut new = old.clone();
            let mut column = Column::new(ColumnType::simple("integer"));
            if parent_default {
                column.value_source = ValueSource::Default("0".into());
            }
            after
                .tables
                .get_mut(&name())
                .unwrap()
                .columns
                .insert("added".into(), column.clone());
            new.columns.insert(uid("c_bbbbbb"), name().column("added"));
            after
                .tables
                .get_mut(&leaf)
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap()
                .columns
                .insert(
                    "added".into(),
                    pbps_model::PartitionColumn {
                        default: local_default.then(|| "1".into()),
                        not_null: true,
                    },
                );
            let changes = vec![
                Change::AddColumn {
                    uid: uid("c_bbbbbb"),
                    table: name(),
                    name: "added".into(),
                    column: Box::new(column),
                },
                Change::SetPartitionDefault {
                    uid: uid("t_bbbbbb"),
                    table: leaf.clone(),
                    parent: name(),
                    column: "added".into(),
                    from: None,
                    to: local_default.then(|| "1".into()),
                    fallback: parent_default.then(|| "0".into()),
                },
                Change::SetPartitionNotNull {
                    uid: uid("t_bbbbbb"),
                    table: leaf.clone(),
                    column: "added".into(),
                    not_null: true,
                },
            ];
            let f = findings(
                &before,
                &after,
                &old,
                &new,
                &consumers("app.leaf.old", true),
                changes,
            );
            assert_eq!(
                f.is_empty(),
                parent_default || local_default,
                "{parent_default}/{local_default}: {f:?}"
            );
            // A default on an existing column still changes the writer interface.
            let f = findings(
                &before,
                &after,
                &old,
                &new,
                &consumers("app.leaf.old", true),
                vec![Change::SetPartitionDefault {
                    uid: uid("t_bbbbbb"),
                    table: leaf.clone(),
                    parent: name(),
                    column: "old".into(),
                    from: None,
                    to: Some("1".into()),
                    fallback: None,
                }],
            );
            assert_eq!(f[0].1.id, "consumer.unproven");
        }
    }
}

#[test]
fn new_written_tables_have_no_old_writers_but_existing_referenced_tables_do() {
    let (mut after, new, leaf) = partitioned();
    let constraint = pbps_model::ForeignKey {
        columns: vec!["old".into()],
        references_table: name(),
        references_columns: vec!["old".into()],
        on_delete: Default::default(),
        on_update: Default::default(),
    };
    after
        .tables
        .get_mut(&leaf)
        .unwrap()
        .foreign_keys
        .insert("fk".into(), constraint.clone());
    let create = |t: &TableName, id: &str| Change::CreateTable {
        uid: uid(id),
        name: t.clone(),
        table: Box::new(after.tables[t].clone()),
        beside_default: None,
        moves_at_most: None,
    };
    for entry in ["app.t", "app.t.old", "app.leaf", "app.leaf.old"] {
        assert!(
            findings(
                &Schema::default(),
                &after,
                &IdsFile::default(),
                &new,
                &consumers(entry, true),
                vec![create(&name(), "t_aaaaaa"), create(&leaf, "t_bbbbbb")]
            )
            .is_empty()
        );
    }
    let fk = Change::AddForeignKey {
        table: leaf.clone(),
        name: "fk".into(),
        constraint: Box::new(constraint),
    };
    assert!(
        findings(
            &schema(),
            &after,
            &ids(),
            &new,
            &consumers("app.leaf", true),
            vec![create(&leaf, "t_bbbbbb"), fk.clone()]
        )
        .is_empty()
    );
    let f = findings(
        &schema(),
        &after,
        &ids(),
        &new,
        &consumers("app.t", true),
        vec![create(&leaf, "t_bbbbbb"), fk],
    );
    assert!(
        f.iter()
            .any(|(at, f)| *at == 1 && f.id == "consumer.unproven")
    );
    // Missing baseline identity alone is no exemption: creation must be in this plan.
    assert!(
        !findings(
            &Schema::default(),
            &after,
            &IdsFile::default(),
            &new,
            &consumers("app.t", true),
            vec![create(&leaf, "t_bbbbbb")]
        )
        .is_empty()
    );
}

#[test]
fn ordinary_columns_require_uids_even_when_partitions_can_inherit_them() {
    let s = schema();
    let mut i = ids();
    i.columns.clear();
    let sides = [Side {
        schema: &s,
        ids: &i,
    }; 2];
    let errors =
        Bindings::resolve(&consumers("app.t.old", false), &sides, &Clone::clone).unwrap_err();
    assert!(errors[0].contains("has no resolved identity"), "{errors:?}");
}

fn domain(not_null: bool, default: Option<&str>, check: Option<&str>) -> pbps_model::ManagedType {
    pbps_model::ManagedType::Domain {
        base: ColumnType::simple("integer"),
        not_null,
        default: default.map(str::to_owned),
        checks: check
            .into_iter()
            .map(|text| ("ck".into(), text.into()))
            .collect(),
        collation: None,
    }
}

#[test]
fn domain_deltas_follow_the_same_column_and_constraint_paths() {
    use pbps_model::DomainHalf::{Loosen, Tighten};
    let empty = domain(false, None, None);
    let checked = domain(false, None, Some("VALUE > 0"));
    // Literal expected cells are (reader, writer); they are not a second
    // interpretation of DomainDelta or of the ordinary classifier.
    for (label, from, to, half, expected) in [
        (
            "check added",
            empty.clone(),
            checked.clone(),
            Tighten,
            [0, 1],
        ),
        (
            "check replaced",
            checked.clone(),
            domain(false, None, Some("VALUE > 1")),
            Tighten,
            [0, 1],
        ),
        (
            "replacement drop half",
            checked.clone(),
            domain(false, None, Some("VALUE > 1")),
            Loosen,
            [0, 0],
        ),
        ("check removed", checked, empty.clone(), Loosen, [0, 0]),
        (
            "NOT NULL tightened",
            empty.clone(),
            domain(true, None, None),
            Tighten,
            [0, 1],
        ),
        (
            "nullability loosened",
            domain(true, None, None),
            empty.clone(),
            Loosen,
            [1, 0],
        ),
        (
            "DEFAULT added",
            empty.clone(),
            domain(false, Some("1"), None),
            Loosen,
            [0, 1],
        ),
        (
            "DEFAULT replaced",
            domain(false, Some("1"), None),
            domain(false, Some("2"), None),
            Loosen,
            [0, 1],
        ),
        (
            "DEFAULT removed",
            domain(false, Some("1"), None),
            empty,
            Loosen,
            [0, 1],
        ),
    ] {
        let changed: pbps_model::TypeRef = "app.domain".parse().unwrap();
        for ty in [
            ColumnType::reference(changed.clone()),
            ColumnType::reference("app.other".parse().unwrap()),
            ColumnType::simple("integer"),
        ] {
            let matches = ty.type_ref() == Some(&changed);
            let mut before = schema();
            before
                .tables
                .get_mut(&name())
                .unwrap()
                .columns
                .get_mut("old")
                .unwrap()
                .ty = ty;
            before.types.insert(changed.clone(), from.clone());
            let mut after = before.clone();
            after.types.insert(changed.clone(), to.clone());
            let change = Change::AlterDomain {
                uid: uid("y_aaaaaa"),
                name: changed.clone(),
                from: Box::new(from.clone()),
                to: Box::new(to.clone()),
                half,
            };
            for (writes, count) in [false, true].into_iter().zip(expected) {
                let f = findings(
                    &before,
                    &after,
                    &ids(),
                    &ids(),
                    &consumers("app.t.old", writes),
                    vec![change.clone()],
                );
                assert_eq!(
                    f.len(),
                    if matches { count } else { 0 },
                    "{label}, writes={writes}, matches={matches}: {f:?}"
                );
                for (_, f) in f {
                    assert_eq!(f.id, "consumer.unproven");
                    assert!(
                        f.message.contains("alter domain") && f.message.contains("app.domain"),
                        "{}",
                        f.message
                    );
                }
            }
        }
    }
}

#[test]
fn domain_translation_keeps_overrides_supplied_columns_and_the_shared_restoration_answer() {
    use pbps_model::DomainHalf::{Loosen, Tighten};
    let changed: pbps_model::TypeRef = "app.domain".parse().unwrap();
    for (from, to, half) in [
        (domain(false, None, None), domain(true, None, None), Tighten),
        (domain(true, None, None), domain(false, None, None), Loosen),
        (
            domain(false, Some("1"), None),
            domain(false, Some("2"), None),
            Loosen,
        ),
    ] {
        let mut before = schema();
        let col = before
            .tables
            .get_mut(&name())
            .unwrap()
            .columns
            .get_mut("old")
            .unwrap();
        col.ty = ColumnType::reference(changed.clone());
        // A column's own NOT NULL and DEFAULT keep their effective meaning.
        col.nullable = false;
        col.value_source = ValueSource::Default("3".into());
        before.types.insert(changed.clone(), from.clone());
        let mut after = before.clone();
        after.types.insert(changed.clone(), to.clone());
        let change = Change::AlterDomain {
            uid: uid("y_aaaaaa"),
            name: changed.clone(),
            from: Box::new(from),
            to: Box::new(to),
            half,
        };
        for writes in [false, true] {
            assert!(
                findings(
                    &before,
                    &after,
                    &ids(),
                    &ids(),
                    &consumers("app.t.old", writes),
                    vec![change.clone()]
                )
                .is_empty()
            );
        }
    }
    for (from, to, half) in [
        (
            domain(false, Some("1"), None),
            domain(false, Some("2"), None),
            Loosen,
        ),
        (
            domain(false, None, None),
            domain(false, None, Some("VALUE > 0")),
            Tighten,
        ),
    ] {
        let mut before = schema();
        before
            .tables
            .get_mut(&name())
            .unwrap()
            .columns
            .get_mut("old")
            .unwrap()
            .ty = ColumnType::reference(changed.clone());
        let change = Change::AlterDomain {
            uid: uid("y_aaaaaa"),
            name: changed.clone(),
            from: Box::new(from),
            to: Box::new(to),
            half,
        };
        assert!(
            findings_with_restoration(
                &before,
                &before,
                &ids(),
                &ids(),
                &consumers("app.t.old", true),
                vec![change],
                true
            )
            .is_empty()
        );
    }
    let before = schema();
    let mut after = before.clone();
    let mut new = ids();
    let mut spec = Column::new(ColumnType::reference(changed.clone()));
    spec.value_source = ValueSource::Default("1".into());
    after
        .tables
        .get_mut(&name())
        .unwrap()
        .columns
        .insert("added".into(), spec.clone());
    new.columns.insert(uid("c_bbbbbb"), name().column("added"));
    let changes = vec![
        Change::AddColumn {
            uid: uid("c_bbbbbb"),
            table: name(),
            name: "added".into(),
            column: Box::new(spec),
        },
        Change::AlterDomain {
            uid: uid("y_aaaaaa"),
            name: changed,
            from: Box::new(domain(false, None, None)),
            to: Box::new(domain(true, None, None)),
            half: Tighten,
        },
    ];
    assert!(
        findings(
            &before,
            &after,
            &ids(),
            &new,
            &consumers("app.t", true),
            changes
        )
        .is_empty()
    );
}

#[test]
fn domain_checks_reach_sibling_writers_and_usage_on_either_side() {
    let changed: pbps_model::TypeRef = "app.domain".parse().unwrap();
    let from = domain(false, None, None);
    let to = domain(false, None, Some("VALUE > 0"));
    for matching_side in [0, 1] {
        let mut sides = [schema(), schema()];
        let mut i = ids();
        i.columns.insert(uid("c_bbbbbb"), name().column("other"));
        for (index, schema) in sides.iter_mut().enumerate() {
            schema.tables.get_mut(&name()).unwrap().columns.insert(
                "other".into(),
                Column::new(if index == matching_side {
                    ColumnType::reference(changed.clone())
                } else {
                    ColumnType::simple("integer")
                }),
            );
        }
        let change = Change::AlterDomain {
            uid: uid("y_aaaaaa"),
            name: changed.clone(),
            from: Box::new(from.clone()),
            to: Box::new(to.clone()),
            half: pbps_model::DomainHalf::Tighten,
        };
        assert_eq!(
            findings(
                &sides[0],
                &sides[1],
                &i,
                &i,
                &consumers("app.t.old", true),
                vec![change.clone()]
            )
            .len(),
            1
        );
        assert!(
            findings(
                &sides[0],
                &sides[1],
                &i,
                &i,
                &consumers("app.t.old", false),
                vec![change]
            )
            .is_empty()
        );
    }
}

#[test]
fn derived_domain_overrides_keep_effective_column_attributes() {
    let root: pbps_model::TypeRef = "app.domain".parse().unwrap();
    let derived: pbps_model::TypeRef = "app.derived".parse().unwrap();
    for (from, to, half) in [
        (
            domain(true, None, None),
            domain(false, None, None),
            pbps_model::DomainHalf::Loosen,
        ),
        (
            domain(false, Some("1"), None),
            domain(false, Some("2"), None),
            pbps_model::DomainHalf::Loosen,
        ),
    ] {
        let mut before = schema();
        before
            .tables
            .get_mut(&name())
            .unwrap()
            .columns
            .get_mut("old")
            .unwrap()
            .ty = ColumnType::reference(derived.clone());
        let mut local = domain(true, Some("3"), None);
        let pbps_model::ManagedType::Domain { base, .. } = &mut local else {
            unreachable!()
        };
        *base = ColumnType::reference(root.clone());
        before.types.insert(root.clone(), from.clone());
        before.types.insert(derived.clone(), local);
        let mut after = before.clone();
        after.types.insert(root.clone(), to.clone());
        let change = Change::AlterDomain {
            uid: uid("y_aaaaaa"),
            name: root.clone(),
            from: Box::new(from),
            to: Box::new(to),
            half,
        };
        for writes in [false, true] {
            assert!(
                findings(
                    &before,
                    &after,
                    &ids(),
                    &ids(),
                    &consumers("app.t.old", writes),
                    vec![change.clone()]
                )
                .is_empty()
            );
        }
    }
}
