#[test]
fn cancelled_cleanup_findings_keep_the_reported_recovery_names() {
    use pbps_cli::resolver::server::scratch::recovery::Collected;
    let findings = super::recovery_report_findings(Collected {
        cancelled: vec![vec![
            "database pbps_scratch_1519".into(),
            "role pbps_run_1519".into(),
        ]],
        ..Collected::default()
    });
    assert_eq!(findings.len(), 1);
    let finding = &findings[0];
    assert_eq!(finding.id, "scratch.cleanup-unconfirmed");
    assert_eq!(finding.severity, crate::output::Severity::Warning);
    assert_eq!(
        finding.message,
        "a cancelled resolver run could not confirm cleanup; remove exactly: \
             database pbps_scratch_1519, role pbps_run_1519"
    );
    let json = serde_json::to_value(finding).unwrap();
    assert_eq!(json["id"], "scratch.cleanup-unconfirmed");
    assert_eq!(json["severity"], "warning");
    assert_eq!(json["message"], finding.message);
    // An observed cleanup error is returned to its caller instead of
    // entering this collector, so an empty report must stay silent.
    assert!(super::recovery_report_findings(Collected::default()).is_empty());
}

#[test]
fn recovery_findings_name_only_confirmed_removal_and_keep_volume_failures() {
    use pbps_cli::resolver::server::scratch::recovery::{Collected, Group, Kept, Resource};
    let resource = Resource::one("docker container", "owned");
    let findings = super::recovery_report_findings(Collected {
        groups: vec![
            Group {
                token: "dead".into(),
                removed: vec![resource.clone()],
                kept: Vec::new(),
            },
            Group {
                token: "pending".into(),
                removed: Vec::new(),
                kept: vec![Kept {
                    resource,
                    why: "the docker volume data: permission denied".into(),
                    statement: "docker rm --force --volumes owned".into(),
                }],
            },
        ],
        unread: vec!["daemon denied".into()],
        ..Collected::default()
    });
    assert_eq!(
        findings
            .iter()
            .filter(|f| f.id == "scratch.recovered")
            .count(),
        1
    );
    let confirmed = findings
        .iter()
        .find(|f| f.id == "scratch.recovered")
        .unwrap();
    assert!(
        confirmed
            .message
            .starts_with("confirmed removal of docker container owned")
    );
    let pending = findings
        .iter()
        .find(|f| f.id == "scratch.leftover")
        .unwrap();
    assert!(
        pending
            .message
            .contains("docker volume data: permission denied")
    );
    assert_eq!(
        pending.remedy.as_deref(),
        Some("docker rm --force --volumes owned")
    );
    assert!(findings.iter().any(|f| f.id == "scratch.leftovers-unread"));
    assert!(super::recovery_report_findings(Collected::default()).is_empty());
}

/// The plan-time removed-label count and the apply-time probes read the
/// plan against one relation: the stored partitions under the plan's final
/// parent names. Here the plan renames the parent, rebuilds an enum column
/// of it and checks a partition: the partition's check is judged under the
/// replacement's order, so it gets no probe over the old one. Negatives: an
/// empty map (the plan-time call before #2406) probes it, and so does a plan
/// without the rebuild (#2406).
#[test]
fn the_probes_read_a_plan_against_the_stored_partitions_under_their_new_parent() {
    use pbps_dialect::Dialect as _;
    let (parent, renamed, partition) = (
        "app.p".parse::<pbps_model::TableName>().unwrap(),
        "app.p2".parse::<pbps_model::TableName>().unwrap(),
        "app.p_1".parse::<pbps_model::TableName>().unwrap(),
    );
    let mut stored = Schema::default();
    stored
        .tables
        .insert(parent.clone(), pbps_model::Table::default());
    stored.tables.insert(
        partition.clone(),
        pbps_model::Table {
            partition_of: Some(pbps_model::PartitionOf {
                parent: parent.clone(),
                bound: pbps_model::PartitionBound::Default,
                columns: Default::default(),
            }),
            ..pbps_model::Table::default()
        },
    );
    let (table_uid, column_uid): (pbps_model::Uid, pbps_model::Uid) =
        ("t_aaaaaa".parse().unwrap(), "c_aaaaaa".parse().unwrap());
    let ids = IdsFile {
        tables: [(table_uid.clone(), renamed.clone())].into(),
        columns: [(column_uid.clone(), renamed.column("m"))].into(),
        ..IdsFile::default()
    };
    let rename = pbps_model::Change::RenameTable {
        uid: table_uid,
        from: parent,
        to: renamed.clone(),
        defaults: vec![],
    };
    let rebuild = pbps_model::Change::EnumRebuild {
        uid: "y_000001".parse().unwrap(),
        name: "app.mood".parse().unwrap(),
        tmp: "app.tmp".parse().unwrap(),
        step: pbps_model::EnumRebuildStep::Retype {
            column_uid,
            column: renamed.column("m"),
            to: pbps_model::ColumnType::reference("app.mood".parse().unwrap()),
            array: false,
            values: Box::new(pbps_model::ValueChange::Relabel {
                removed: vec!["sad".to_owned()],
            }),
        },
    };
    let check = pbps_model::Change::AddCheck {
        table: partition,
        name: "p_1_mood".into(),
        constraint: pbps_model::CheckConstraint {
            expression: "m <= 'ok'::app.mood".to_owned(),
        },
    };
    let probed = |changes: Vec<pbps_model::Change>, with_relation: bool| {
        let changes: Vec<_> = changes
            .into_iter()
            .map(pbps_model::PlannedChange::new)
            .collect();
        let names = pbps_model::plan_names::PlanNames::of(&changes, &ids).unwrap();
        let partitions = if with_relation {
            probe_partitions(&stored, &names)
        } else {
            pbps_model::Partitions::default()
        };
        pbps_pg::Postgres::new()
            .preflight_with(&names, &partitions)
            .probes
            .iter()
            .any(|p| p.sql.contains("m <= ''ok''"))
    };
    let beside = vec![rename.clone(), rebuild, check.clone()];
    assert!(!probed(beside.clone(), true), "judged by the new order");
    assert!(probed(beside, false), "the empty map reaches no partition");
    assert!(
        probed(vec![rename, check], true),
        "no rebuild, still probed"
    );
}

#[test]
fn bootstrap_requires_each_declared_type_identity() {
    for yaml in [
        "enum: app.mood\nvalues: [sad, happy]\n",
        "domain: app.score\ntype: integer\n",
    ] {
        let ty = pbps_load::load_type_str(std::path::Path::new("type.yml"), yaml).unwrap();
        let mut schema = Schema::default();
        schema.types.insert(ty.name.clone(), ty.definition);
        let mut ids = IdsFile::default();
        // An unrelated identity cannot stand in for this declaration.
        ids.types.insert(
            "y_aaaaaa".parse().unwrap(),
            pbps_model::TypeRef::new("app", "other").unwrap(),
        );
        let error = refuse_unidentified_bootstrap_objects(&schema, &ids)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("type {}", ty.name.shown())),
            "{error}"
        );
        assert!(error.contains("pbps plan"), "{error}");
        ids.types.insert("y_bbbbbb".parse().unwrap(), ty.name);
        refuse_unidentified_bootstrap_objects(&schema, &ids).unwrap();
    }
    refuse_unidentified_bootstrap_objects(&Schema::default(), &IdsFile::default()).unwrap();
}

#[test]
fn bootstrap_refuses_to_record_an_annotation_the_state_cannot_hold() {
    let mut loaded = pbps_load::Loaded::default();
    loaded.hints.annotations.declared = pbps_model::object_ref::ObjectDeps(vec![(
        "module::app.v".parse().unwrap(),
        ["table::app.t".parse().unwrap()].into(),
    )]);
    let built = pbps_diff::scope(
        &Schema::default(),
        &IdsFile::default(),
        &Default::default(),
        &Default::default(),
    );
    let why = bootstrap_record(built, IdsFile::default(), &loaded, "operator").unwrap_err();
    assert!(why.to_string().contains("table::app.t"), "{why}");
    // Negative: the same call without the extended reference records.
    loaded.hints.annotations = Default::default();
    let built = pbps_diff::scope(
        &Schema::default(),
        &IdsFile::default(),
        &Default::default(),
        &Default::default(),
    );
    assert!(bootstrap_record(built, IdsFile::default(), &loaded, "operator").is_ok());
}

#[test]
fn bootstrap_records_identified_types_created_on_an_empty_target() {
    for yaml in [
        "enum: app.mood\nvalues: [ok]\n",
        "domain: app.score\ntype: integer\nchecks: {positive: 'VALUE > 0'}\n",
    ] {
        let ty = pbps_load::load_type_str(std::path::Path::new("type.yml"), yaml).unwrap();
        let mut loaded = pbps_load::Loaded::default();
        loaded.schema.types.insert(ty.name.clone(), ty.definition);
        let mut ids = IdsFile::default();
        ids.types
            .insert("y_aaaaaa".parse().unwrap(), ty.name.clone());
        let dialect = pbps_pg::Postgres::new();
        refuse_unidentified_bootstrap_objects(&loaded.schema, &ids).unwrap();
        refuse_nonempty_bootstrap(&Schema::default(), &[], &dialect, "target").unwrap();
        let changes = pbps_diff::diff(
            pbps_diff::Side {
                schema: &Schema::default(),
                ids: &IdsFile::default(),
            },
            pbps_diff::Side {
                schema: &loaded.schema,
                ids: &ids,
            },
            &dialect,
            &loaded.hints,
        )
        .unwrap();
        assert_eq!(changes.changes.len(), 1);
        assert!(
            matches!(&changes.changes[0].change, pbps_model::Change::CreateType { name, .. } if name == &ty.name)
        );
        // The catalog may respell domain expressions. The recording must
        // retain both that read-back and the authored definition.
        let mut read_back = loaded.schema.clone();
        if let pbps_model::ManagedType::Domain { checks, .. } =
            read_back.types.get_mut(&ty.name).unwrap()
        {
            checks.insert("positive".into(), "(VALUE > 0)".into());
        }
        let built = pbps_diff::scope(&read_back, &ids, &Default::default(), &Default::default());
        let snapshot = bootstrap_record(built, ids.clone(), &loaded, "operator").unwrap();
        assert_eq!(snapshot.kind, StateKind::Bootstrap);
        assert_eq!(snapshot.ids, ids);
        assert_eq!(snapshot.schema, read_back);
        assert_eq!(
            snapshot.declared,
            pbps_model::Declared::from_schema(&loaded.schema)
        );
        assert_eq!(snapshot.declared.overlay(&snapshot.schema), loaded.schema);
    }
}

#[test]
fn bootstrap_extensions_are_exact_prerequisites_but_ordinary_objects_must_be_absent() {
    let name = "vendor_default".into();
    let package = pbps_model::Extension {
        schema: "pg_catalog".into(),
        version: "1.0".into(),
    };
    let mut schema = Schema {
        extensions: [(name, package)].into(),
        ..Default::default()
    };
    let ids = IdsFile {
        extensions: [("e_000001".parse().unwrap(), "vendor_default".into())].into(),
        ..Default::default()
    };
    let pg = pbps_pg::Postgres::new();
    refuse_nonempty_bootstrap(&schema, &[], &pg, "target").unwrap();
    refuse_extension_prerequisite_drift(&schema, &schema, &ids, &pg, "bootstrap").unwrap();
    assert!(refuse_nonempty_bootstrap(&schema, &[], &pbps_mssql::Mssql, "target").is_err());
    for changed in [
        Schema::default(),
        {
            let mut changed = schema.clone();
            changed
                .extensions
                .get_mut("vendor_default")
                .unwrap()
                .version = "2.0".into();
            changed
        },
        {
            let mut changed = schema.clone();
            changed.extensions.get_mut("vendor_default").unwrap().schema = "other".into();
            changed
        },
    ] {
        let error = refuse_extension_prerequisite_drift(&changed, &schema, &ids, &pg, "bootstrap")
            .unwrap_err();
        assert!(error.to_string().contains("vendor_default"));
    }
    schema
        .tables
        .insert(TableName::new("app", "ordinary"), Default::default());
    assert!(refuse_nonempty_bootstrap(&schema, &[], &pg, "target").is_err());
}

/// A sequence the declarations do not name does not make a bootstrap target non-empty, and one
/// they name does (DEC-2721.1): the target is asked through the declarations' projection.
#[test]
fn an_undeclared_sequence_does_not_make_a_bootstrap_target_non_empty_and_a_declared_one_does() {
    let sequence = pbps_model::Sequence {
        ty: pbps_model::ColumnType::simple("bigint"),
        start: 1,
        increment: 1,
        options: Default::default(),
    };
    let pg = pbps_pg::Postgres::new();
    let mut existing = Schema::default();
    existing
        .sequences
        .insert("app.theirs".parse().unwrap(), sequence.clone());
    let declared = Schema::default();
    refuse_nonempty_bootstrap(
        &standing_of_the_declared(&existing, &declared),
        &[],
        &pg,
        "target",
    )
    .unwrap();
    // Negative: the one the declarations name stands, and is a reason to refuse.
    let mut named = Schema::default();
    named
        .sequences
        .insert("app.theirs".parse().unwrap(), sequence);
    let error = refuse_nonempty_bootstrap(
        &standing_of_the_declared(&existing, &named),
        &[],
        &pg,
        "target",
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("sequence `app.theirs`"),
        "{error}"
    );
}

#[test]
fn bootstrap_schema_spelling_checks_include_unreferenced_types() {
    for yaml in [
        "enum: typed.mood\nvalues: [ok]\n",
        "domain: typed.score\ntype: integer\n",
    ] {
        let ty = pbps_load::load_type_str(std::path::Path::new("type.yml"), yaml).unwrap();
        let mut schema = Schema::default();
        schema.types.insert(ty.name.clone(), ty.definition);
        let declared = schemas_declared(&schema);
        assert_eq!(declared.len(), 1);
        assert!(declared["typed"].contains(&ty.name.shown().to_string()));
        let wrong = wrongly_spelt(&[("typed".into(), Some("TYPED".into()))].into(), &declared);
        assert_eq!(wrong.len(), 1);
        assert!(wrong[0].contains(&ty.name.shown().to_string()));
        assert!(
            wrongly_spelt(&[("typed".into(), Some("typed".into()))].into(), &declared).is_empty()
        );
        assert!(wrongly_spelt(&[("typed".into(), None)].into(), &declared).is_empty());
    }
    assert!(schemas_declared(&Schema::default()).is_empty());
}

#[test]
fn bootstrap_refuses_existing_types_and_omitted_definitions_before_building() {
    let dialect = pbps_pg::Postgres::new();
    for yaml in [
        "enum: app.mood\nvalues: [sad, happy]\n",
        "domain: app.score\ntype: integer\n",
    ] {
        let ty = pbps_load::load_type_str(std::path::Path::new("type.yml"), yaml).unwrap();
        let mut schema = Schema::default();
        schema.types.insert(ty.name.clone(), ty.definition);
        let error = refuse_nonempty_bootstrap(&schema, &[], &dialect, "target")
            .unwrap_err()
            .to_string();
        assert!(error.contains("already has 1"), "{error}");
        assert!(
            error.contains(&format!("type {}", ty.name.shown())),
            "{error}"
        );
    }
    let empty = Schema::default();
    let error = refuse_nonempty_bootstrap(
        &empty,
        &["type app.omitted cannot be read".into()],
        &dialect,
        "target",
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("type app.omitted"), "{error}");
    refuse_nonempty_bootstrap(&empty, &[], &dialect, "target").unwrap();
}

#[test]
fn nonempty_bootstrap_shows_the_extracted_environment_label_once() {
    let schema = Schema::default();
    for (label, shown) in [
        ("target\n\u{1b}[31m\u{202e}", r"target\n\u{1b}[31m\u{202e}"),
        (
            r"target\n\u{1b}[31m\u{202e}",
            r"target\\n\\u{1b}[31m\\u{202e}",
        ),
        ("名前_ünï", "名前_ünï"),
        ("ordinary", "ordinary"),
    ] {
        let limitations = vec![r"type app.literal\n cannot be read".to_owned()];
        let error =
            refuse_nonempty_bootstrap(&schema, &limitations, &pbps_pg::Postgres::new(), label)
                .unwrap_err()
                .to_string();
        assert_eq!(
            error,
            format!(
                "`{shown}` already has 1 of the declared object(s): type app.literal\\n cannot be read.\nBootstrap builds into an empty database; use `pbps plan --db` and `pbps apply` to migrate it instead."
            )
        );
        refuse_nonempty_bootstrap(&schema, &[], &pbps_pg::Postgres::new(), label).unwrap();
    }
}

#[test]
fn bootstrap_admits_only_pg_cluster_roles_without_managed_grants() {
    let mut schema = Schema::default();
    let role: pbps_model::RoleName = "reader".into();
    schema.roles.insert(role.clone(), Default::default());
    refuse_nonempty_bootstrap(&schema, &[], &pbps_pg::Postgres::new(), "target").unwrap();
    assert!(refuse_nonempty_bootstrap(&schema, &[], &pbps_mssql::Mssql, "target").is_err());
    schema.roles.get_mut(&role).unwrap().grants.insert(
        pbps_model::GrantTarget::Schema("app".into()),
        [pbps_model::Permission::Select].into(),
    );
    let error = refuse_nonempty_bootstrap(&schema, &[], &pbps_pg::Postgres::new(), "target")
        .unwrap_err()
        .to_string();
    assert!(error.contains("role reader"), "{error}");
}

fn order_carried_moves(
    cs: &mut pbps_model::ChangeSet,
    occupants: &[pbps_pg::catalog::NameOccupant],
    relations: &[(TableName, &'static str)],
    sides: (pbps_diff::Side<'_>, pbps_diff::Side<'_>),
    dialect: &dyn pbps_dialect::Dialect,
    catalog: &pbps_dialect::slots::CatalogFacts,
) {
    crate::test_planning(cs, |plan| {
        super::order_carried_moves(plan, occupants, relations, sides, dialect, catalog)
    })
}

#[test]
fn created_column_guard_keys_stay_raw_and_missing_names_are_shown_once() {
    use pbps_model::{Change, ChangeSet, Column, PlannedChange, Table};
    const RAW: &str = "column\n\r\u{1b}[31m\\\u{200b}名前";
    const SHOWN: &str = r"column\n\r\u{1b}[31m\\\u{200b}名前";
    let name = TableName::new("app", "t");
    let ordinary = Table {
        columns: [(RAW.into(), Column::new("int".parse().unwrap()))].into(),
        ..Default::default()
    };
    let schema = |table: Table| Schema {
        tables: [(name.clone(), table)].into(),
        ..Default::default()
    };
    let created = ChangeSet {
        changes: vec![PlannedChange::new(Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: name.clone(),
            table: Box::new(ordinary.clone()),
            beside_default: None,
            moves_at_most: None,
        })],
    };
    let empty = Schema::default();
    let correct = schema(ordinary.clone());
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &created,
        &empty,
        &correct,
        "prod",
        Settled::Whole,
    )
    .unwrap();
    let mut literal = ordinary.clone();
    let column = literal.columns.shift_remove(RAW).unwrap();
    literal.columns.insert(SHOWN.into(), column);
    let error = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &created,
        &empty,
        &schema(literal),
        "prod",
        Settled::Whole,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains(&format!("column `{SHOWN}` is not there")),
        "{message:?}"
    );
    assert!(!message.contains(RAW), "{message:?}");
}

#[test]
fn computed_column_guard_keys_stay_raw_and_changed_names_are_shown_once() {
    use pbps_model::{Change, ChangeSet, Column, ComputedColumn, PlannedChange, Table};
    const RAW: &str = "column\n\r\u{1b}[31m\\\u{200b}名前";
    const SHOWN: &str = r"column\n\r\u{1b}[31m\\\u{200b}名前";
    let name = TableName::new("app", "t");
    let schema = |table: Table| Schema {
        tables: [(name.clone(), table)].into(),
        ..Default::default()
    };
    let mut computed = Table {
        columns: [("id".into(), Column::new("int".parse().unwrap()))].into(),
        ..Default::default()
    };
    computed.computed.insert(
        RAW.into(),
        ComputedColumn {
            expression: "1".into(),
            persisted: false,
            not_null: false,
        },
    );
    let before = schema(computed.clone());
    let changing = ChangeSet {
        changes: vec![PlannedChange::new(Change::AlterColumnNullability {
            uid: "c_aaaaaa".parse().unwrap(),
            column: name.column("id"),
            ty: "int".parse().unwrap(),
            to_nullable: false,
            collation: None,
            through_external_partitions: false,
        })],
    };
    computed.columns.get_mut("id").unwrap().nullable = false;
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &changing,
        &before,
        &schema(computed.clone()),
        "prod",
        Settled::Whole,
    )
    .unwrap();
    computed.computed.get_mut(RAW).unwrap().persisted = true;
    let error = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &changing,
        &before,
        &schema(computed),
        "prod",
        Settled::Whole,
    )
    .unwrap_err();
    let message = format!("{error:#}");
    let expected = format!("computed column `{SHOWN}` is not as the plan left it");
    assert_eq!(message.matches(&expected).count(), 1, "{message:?}");
    assert!(!message.contains(RAW), "{message:?}");
}

#[test]
fn inventory_prose_shows_names_once_beside_rendered_module_errors() {
    use pbps_db::catalog::{LimitationTarget, Pulled, UnmanagedModule};
    for (part, shown) in [
        (
            "a\nb\r\u{1b}[31m\\c\u{200b}",
            r"a\nb\r\u{1b}[31m\\c\u{200b}",
        ),
        ("名前_ünï", "名前_ünï"),
    ] {
        let name = TableName::new("app", part);
        let id = ModuleId::Named(name.clone());
        let why = pbps_dialect::DialectError::Invalid {
            dialect: "test",
            message: format!("module `app.{part}` is invalid"),
        }
        .to_string();
        let pulled = Pulled {
            unmanaged_modules: vec![UnmanagedModule {
                kind: "view",
                target: LimitationTarget::module(id.clone()),
                why: why.clone(),
            }],
            ..Default::default()
        };
        let unreadable = unreadable_objects(&pulled);
        let expected = format!("view app.{shown} ({why})");
        assert_eq!(unreadable[0].1, expected);
        let empty = IdsFile::default();
        let scoped = pbps_diff::scope(
            &pulled.schema,
            &empty,
            &BTreeSet::new(),
            &Default::default(),
        );
        let labels = unmanaged_objects(&scoped, &unreadable, &empty, &BTreeSet::new());
        assert_eq!(labels, std::slice::from_ref(&expected));
        let refusal = apply_unmanaged_policy(&labels, pbps_config::Unmanaged::Error).unwrap_err();
        let expected_refusal = format!(
            "`unmanaged: error` in pbps.yml, and 1 object(s) here are not declared: {expected}.\nDeclare them (`pbps pull` reverse-generates them) or relax the setting."
        );
        assert_eq!(refusal.to_string(), expected_refusal);
        let report = crate::output::Report::new(
            "verify",
            vec![crate::output::Finding::error(
                "state.unmanaged-refused",
                refusal.to_string(),
            )],
            Some(serde_json::json!({"name": name})),
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["findings"][0]["message"], expected_refusal);
        assert_eq!(json["data"]["name"], format!("app.{part}"));
        assert_eq!(
            crate::output::human(&report.findings),
            format!("  error: {expected_refusal}\n")
        );

        let modules = [id].into_iter().collect();
        let managed = managed_limitations(&pulled, &empty, &modules, &BTreeSet::new());
        let expected_managed = format!("view app.{shown} is in the managed set, but {why}");
        assert_eq!(managed, std::slice::from_ref(&expected_managed));
        assert_eq!(
            refuse_managed_limitations(&managed)
                .unwrap_err()
                .to_string(),
            format!(
                "1 fact(s) inside the managed set cannot be represented:\n  {expected_managed}\nRefusing to compare, plan from, or record a partial schema."
            )
        );
    }
}

#[test]
fn catalog_prose_copies_keep_one_fact_without_merging_other_facts() {
    use pbps_db::catalog::{Limitation, LimitationTarget, Pulled, UnmanagedModule};
    let part = "a\nb\\c\u{200b}";
    let id = ModuleId::Named(TableName::new("app", part));
    let target = LimitationTarget::module(id.clone());
    let detail = r"app.a\nb\\c\u{200b} cannot be represented".to_owned();
    let mut pulled = Pulled {
        limitations: vec![Limitation {
            target: target.clone(),
            detail: detail.clone(),
        }],
        unmanaged_modules: vec![UnmanagedModule {
            kind: "view",
            target,
            why: detail.clone(),
        }],
        ..Default::default()
    };
    let modules = [id].into_iter().collect();
    let facts = managed_limitations(&pulled, &IdsFile::default(), &modules, &BTreeSet::new());
    assert_eq!(facts, [r"app.a\nb\\c\u{200b} cannot be represented"]);
    assert_eq!(pulled.limitations[0].detail, detail);
    pulled.unmanaged_modules[0].why = "another unsupported property".into();
    assert_eq!(
        managed_limitations(&pulled, &IdsFile::default(), &modules, &BTreeSet::new()).len(),
        2
    );
}

#[test]
fn dropped_referrer_name_keys_keep_raw_qualified_names() {
    let part = "a\nb\\c\u{200b}";
    let table = pbps_model::TableName::new("app", part);
    let changes = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::DropModule {
                id: pbps_model::ModuleId::Named(table),
                kind: pbps_model::ModuleKind::View,
            },
        )],
    };
    assert_eq!(
        super::dropped_referrer_names(&changes),
        [format!("app.{part}")].into()
    );
    assert!(!super::dropped_referrer_names(&changes).contains(r"app.a\nb\c\u{200b}"));
}

/// The refusal prints the SQL of the statements that cannot run in a
/// transaction, and the SQL carries the plan's names: a quoted identifier
/// may hold a line break or a terminal sequence, which the refusal shows
/// escaped and not as a line of its own (DEC-2032.1, #2035).
#[test]
fn the_non_transactional_refusal_shows_the_sql_escaped() {
    use pbps_dialect::Statement;
    let hostile = "CREATE INDEX CONCURRENTLY \"x\n\u{1b}[31mred\r\u{202e}\" ON t (c);";
    let e = super::reject_non_transactional(&[
        Statement::new("SELECT 1;"),
        Statement::new(hostile).non_transactional(),
    ])
    .expect_err("a statement that cannot run in a transaction")
    .to_string();
    // The refusal's own four lines, and nothing the name added.
    assert!(!e.chars().any(|c| c.is_control() && c != '\n'), "{e:?}");
    assert_eq!(e.lines().count(), 4, "{e}");
    assert!(e.contains("\"x\\n\\u{1b}[31mred\\r\\u{202e}\""), "{e}");
    // Negative: the statements that can run are not listed, and a plain one
    // is listed as written.
    assert!(!e.contains("SELECT 1"), "{e}");
    let e = super::reject_non_transactional(&[Statement::new(
        "CREATE INDEX CONCURRENTLY ix ON t (名前);",
    )
    .non_transactional()])
    .expect_err("a statement that cannot run in a transaction")
    .to_string();
    assert!(
        e.contains("CREATE INDEX CONCURRENTLY ix ON t (名前);"),
        "{e}"
    );
}

#[test]
fn verify_keeps_the_recorded_entry_and_distinguishes_its_absence_from_emptiness() {
    let entry = pbps_db::LedgerEntry {
        id: 7,
        applied_at: "2026-10-13T00:00:00Z".into(),
        snapshot: pbps_model::StateSnapshot::new(
            pbps_model::StateKind::Baseline,
            pbps_model::Schema::default(),
            pbps_model::IdsFile::default(),
            "operator",
        ),
    };
    assert_eq!(
        super::recorded_verify_state(Ok(Some(entry.clone())), "prod").unwrap(),
        entry
    );
    let empty = super::recorded_verify_state(Ok(None), "prod").unwrap_err();
    assert_eq!(empty.id, "state.no-entries");
    assert!(
        empty
            .error
            .to_string()
            .contains("`prod` has a ledger but no entries")
    );
    assert!(empty.error.to_string().contains("pbps snapshot"));
    let absent = super::recorded_verify_state(Err(pbps_db::LedgerError::NotInitialized), "prod")
        .unwrap_err();
    assert_eq!(absent.id, "state.uninitialized");
    assert!(absent.error.to_string().contains("pbps baseline"));
    assert!(absent.error.to_string().contains("pbps bootstrap"));
}

#[test]
fn verify_keeps_ledger_read_causes_out_of_connection_failures() {
    for error in [
        pbps_db::LedgerError::BadEntry {
            id: 8,
            message: "malformed JSON".into(),
        },
        pbps_db::LedgerError::BadEntry {
            id: 9,
            message: "unsupported version 999".into(),
        },
        pbps_db::LedgerError::Db(pbps_db::DbError::Refused("ledger query denied".into())),
        pbps_db::LedgerError::Locked(pbps_db::LockInfo {
            locked_by: "another operator".into(),
            locked_at: "2026-10-13T00:00:00Z".into(),
            session_lookup: None,
        }),
    ] {
        let cause = error.to_string();
        let failure = super::recorded_verify_state(Err(error), "prod").unwrap_err();
        assert_eq!(failure.id, "state.unreadable");
        assert_eq!(failure.error.to_string(), cause);
        assert!(
            failure
                .error
                .downcast_ref::<pbps_db::LedgerError>()
                .is_some()
        );
    }
}

fn zone(text: &str) -> pbps_model::SessionZone {
    pbps_model::SessionZone::new(text).unwrap()
}

#[test]
fn an_apply_in_the_zone_the_plan_recorded_goes_ahead() {
    let taipei = zone("Asia/Taipei");
    assert_eq!(
        super::moved_time_zone(Some(&taipei), Some(&taipei), "prod"),
        None
    );
    // SQL Server: no zone on either side, nothing to hold.
    assert_eq!(super::moved_time_zone(None, None, "prod"), None);
}

#[test]
fn an_apply_in_another_zone_is_refused_naming_both() {
    let said = super::moved_time_zone(
        Some(&zone("Asia/Taipei")),
        Some(&zone("America/New_York")),
        "prod",
    )
    .expect("refused");
    assert!(said.contains("`America/New_York` now"), "{said}");
    assert!(said.contains("computed in `Asia/Taipei`"), "{said}");
    assert!(said.contains("pbps plan --db"), "{said}");
    assert!(said.contains("ALTER DATABASE"), "{said}");
}

#[test]
fn two_spellings_of_one_zone_are_refused_like_two_zones() {
    let message = super::moved_time_zone(Some(&zone("UTC")), Some(&zone("Etc/UTC")), "prod")
        .expect("spellings still refuse");
    for text in [
        "`Etc/UTC` now",
        "computed in `UTC`",
        "Different spellings are treated as different zones",
        "texts must match exactly",
        "pbps plan --db",
    ] {
        assert!(message.contains(text), "{message}");
    }
    assert_eq!(
        super::moved_time_zone(Some(&zone("UTC")), Some(&zone("UTC")), "prod"),
        None,
        "matching spellings carry no refusal or guidance"
    );
}

#[test]
fn a_zone_on_one_side_only_is_refused() {
    let taipei = zone("Asia/Taipei");
    assert!(super::moved_time_zone(Some(&taipei), None, "prod").is_some());
    let said = super::moved_time_zone(None, Some(&taipei), "prod").expect("refused");
    assert!(said.contains("records no time zone"), "{said}");
}

fn object_reads(cs: &pbps_model::ChangeSet) -> (Vec<TableName>, Vec<TableName>) {
    let ids = crate::plan_fixture::ids(cs);
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, &ids).unwrap();
    super::object_reads(cs, &names)
}
fn refuse_permanent_referencers(
    cs: &pbps_model::ChangeSet,
    referencers: &[pbps_pg::catalog::PermanentReferencer],
    staged: bool,
) -> anyhow::Result<()> {
    let ids = crate::plan_fixture::ids(cs);
    let names = pbps_model::plan_names::PlanNames::of(&cs.changes, &ids)?;
    super::refuse_permanent_referencers(cs, &names, referencers, staged)
}

/// #1077: on SQL Server, any other object at a name the plan creates
/// refuses it, with its kind and parent named. One the plan frees first
/// does not: an object it drops or renames away, a constraint it drops,
/// and a constraint of a table it drops or moves to another schema. A
/// constraint of a table renamed within its schema stays behind, and a
/// module the plan rebuilds already owns its name.
/// DEC-981.2: a table rename's own target is claimed when it runs. Two
/// renames into names the database reads as one (`Ck_Name`, `ck_name` on a
/// case-insensitive catalog) are refused at `plan --db`; on a database that
/// reads them as two, and for a rename into a case variant of the table's
/// own name, nothing is refused. An object the project does not record
/// that holds the target is refused too.
#[test]
fn a_table_renames_target_is_claimed_when_the_rename_runs() {
    use pbps_model::{Change, ChangeSet, PlannedChange};
    use pbps_mssql::catalog::NameOccupant;
    let rename = |from: &str, to: &str| {
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, from, 0),
            from: from.parse().unwrap(),
            to: to.parse().unwrap(),
            defaults: vec![],
        })
    };
    let both = ChangeSet {
        changes: vec![
            rename("dbo.a", "dbo.Ck_Name"),
            rename("dbo.b", "dbo.ck_name"),
        ],
    };
    let alike = [(
        TableName::new("dbo", "Ck_Name"),
        TableName::new("dbo", "ck_name"),
    )];
    let e = refuse_occupied_objects_under(&both, &[], &alike, "prod")
        .unwrap_err()
        .to_string();
    assert!(e.contains("`dbo.ck_name`"), "{e}");
    assert!(
        e.contains("this plan puts user table `dbo.Ck_Name` there first"),
        "{e}"
    );
    refuse_occupied_objects_under(&both, &[], &[], "prod")
        .expect("two names where the database reads them as two");

    // A case variant of the table's own name is the table, not a holder.
    let own = ChangeSet {
        changes: vec![rename("dbo.orders", "dbo.Orders")],
    };
    let self_alike = [(
        TableName::new("dbo", "orders"),
        TableName::new("dbo", "Orders"),
    )];
    refuse_occupied_objects_under(&own, &[], &self_alike, "prod")
        .expect("a rename into a case variant of its own name");

    // An object the project does not record, holding the target.
    let sequence = NameOccupant {
        wanted: TableName::new("dbo", "fresh"),
        name: TableName::new("dbo", "fresh"),
        kind: "sequence object".into(),
        parent: None,
        parent_column: None,
    };
    let into_sequence = ChangeSet {
        changes: vec![rename("dbo.old", "dbo.fresh")],
    };
    // Read from the catalog, so a holder no other change names is seen.
    assert!(
        object_reads(&into_sequence)
            .0
            .contains(&TableName::new("dbo", "fresh")),
        "the rename target is read"
    );
    let e = refuse_occupied_objects_under(&into_sequence, &[sequence], &[], "prod")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("the database already has sequence object `dbo.fresh`"),
        "{e}"
    );

    // A check the table carries into the target schema, whose name there
    // is the target's under the collation: the transfer runs first, so it
    // holds the name when `sp_rename` runs (review of #1346).
    let check = NameOccupant {
        wanted: TableName::new("s1", "New"),
        name: TableName::new("s1", "New"),
        kind: "check constraint".into(),
        parent: Some(TableName::new("s1", "old")),
        parent_column: None,
    };
    let moved = ChangeSet {
        changes: vec![rename("s1.old", "s2.new")],
    };
    let carried_alike = [(TableName::new("s2", "New"), TableName::new("s2", "new"))];
    let e =
        refuse_occupied_objects_under(&moved, std::slice::from_ref(&check), &carried_alike, "prod")
            .unwrap_err()
            .to_string();
    assert!(e.contains("check constraint `s2.New` on `s2.new`"), "{e}");
    refuse_occupied_objects_under(&moved, &[check], &[], "prod")
        .expect("two names where the database reads them as two");
}

/// A history name the database holds blocks the `CREATE` only if it is
/// still held when that runs: a table the plan drops or renames away
/// first frees it, as the plan-time walk agrees (#1501 review). A name no
/// history takes is not this check's.
#[test]
fn a_history_occupant_blocks_unless_the_plan_frees_it_first() {
    use pbps_model::{Change, ChangeSet, PlannedChange};
    use pbps_mssql::catalog::NameOccupant;
    let history = TableName::new("dbo", "old_history");
    let uid = |n: &str| pbps_model::Uid::derived(pbps_model::UidKind::Table, n, 0);
    let create = PlannedChange::new(Change::CreateTable {
        uid: uid("dbo.t"),
        name: TableName::new("dbo", "t"),
        table: Box::new(pbps_model::Table {
            system_time: Some(pbps_model::SystemTime {
                start: "vf".into(),
                end: "vt".into(),
                hidden: false,
                versioning: Some(pbps_model::SystemVersioning {
                    history: history.clone(),
                    retention: None,
                }),
            }),
            ..Default::default()
        }),
        beside_default: None,
        moves_at_most: None,
    });
    let at = |name: &TableName| NameOccupant {
        wanted: name.clone(),
        name: name.clone(),
        kind: "user table".into(),
        parent: None,
        parent_column: None,
    };
    let held = [at(&history)];
    let blocking = |changes: Vec<PlannedChange>| {
        blocking_history_occupants(&ChangeSet { changes }, &held)
            .into_iter()
            .map(|(t, h, _)| (t.raw().to_string(), h.raw().to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        blocking(vec![create.clone()]),
        [("dbo.t".to_owned(), "dbo.old_history".to_owned())]
    );
    let dropped = PlannedChange::new(Change::DropTable {
        uid: uid("dbo.old_history"),
        name: history.clone(),
        detach_from: None,
    });
    assert!(blocking(vec![dropped, create.clone()]).is_empty());
    let renamed = PlannedChange::new(Change::RenameTable {
        uid: uid("dbo.old_history"),
        from: history.clone(),
        to: TableName::new("dbo", "archived"),
        defaults: Vec::new(),
    });
    assert!(blocking(vec![renamed, create.clone()]).is_empty());
    // Negative: an occupant of a name no history takes.
    let other = [at(&TableName::new("dbo", "elsewhere"))];
    assert!(
        blocking_history_occupants(
            &ChangeSet {
                changes: vec![create]
            },
            &other
        )
        .is_empty()
    );
}

#[test]
fn a_sys_objects_occupant_refuses_unless_the_plan_frees_it() {
    use pbps_model::{Change, ChangeSet, Module, ModuleKind, PlannedChange};
    use pbps_mssql::catalog::NameOccupant;
    let x = TableName::new("dbo", "x");
    let old = TableName::new("dbo", "old");
    let create_table = || {
        PlannedChange::new(Change::CreateTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.x", 0),
            name: x.clone(),
            table: Box::default(),
            beside_default: None,
            moves_at_most: None,
        })
    };
    let occupant = |kind: &str, parent: Option<&TableName>| NameOccupant {
        wanted: x.clone(),
        name: x.clone(),
        kind: kind.into(),
        parent: parent.cloned(),
        parent_column: None,
    };
    let plan = |changes: Vec<PlannedChange>| ChangeSet { changes };
    let refused = |changes: Vec<PlannedChange>, o: NameOccupant| {
        refuse_occupied_objects(&plan(changes), &[o], "prod")
            .err()
            .map(|e| e.to_string())
    };

    let e = refused(vec![create_table()], occupant("sequence object", None)).unwrap();
    assert!(e.contains("already has sequence object `dbo.x`"), "{e}");
    let e = refused(
        vec![create_table()],
        occupant("check constraint", Some(&old)),
    )
    .unwrap();
    assert!(
        e.contains("already has check constraint `dbo.x` on `dbo.old`"),
        "{e}"
    );
    // Spelled as the catalog spells it, under the plan's own name.
    let e = refuse_occupied_objects(
        &plan(vec![create_table()]),
        &[NameOccupant {
            name: TableName::new("dbo", "X"),
            ..occupant("synonym", None)
        }],
        "prod",
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("`dbo.x`: the database already has synonym `dbo.X`"),
        "{e}"
    );

    // Freed by the plan itself.
    let drop_table = |name: &TableName| {
        PlannedChange::new(Change::DropTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.gone", 0),
            name: name.clone(),
            detach_from: None,
        })
    };
    let rename = |to: TableName| {
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.old", 0),
            from: old.clone(),
            to,
            defaults: Vec::new(),
        })
    };
    assert_eq!(
        refused(
            vec![drop_table(&x), create_table()],
            occupant("user table", None)
        ),
        None
    );
    assert_eq!(
        refused(
            vec![
                PlannedChange::new(Change::DropCheck {
                    table: old.clone(),
                    name: "x".into(),
                }),
                create_table()
            ],
            occupant("check constraint", Some(&old))
        ),
        None
    );
    // A check dropped from a table the plan also renames names the
    // table by its new name; the constraint is still where it was.
    let new = TableName::new("dbo", "new");
    assert_eq!(
        refused(
            vec![
                rename(new.clone()),
                PlannedChange::new(Change::DropCheck {
                    table: new.clone(),
                    name: "x".into(),
                }),
                create_table()
            ],
            occupant("check constraint", Some(&old))
        ),
        None
    );
    assert_eq!(
        refused(
            vec![drop_table(&old), create_table()],
            occupant("check constraint", Some(&old))
        ),
        None
    );
    assert_eq!(
        refused(
            vec![rename(TableName::new("archive", "old")), create_table()],
            occupant("check constraint", Some(&old))
        ),
        None,
        "a transfer takes the table's constraints to the other schema"
    );
    assert!(
        refused(
            vec![rename(new.clone()), create_table()],
            occupant("check constraint", Some(&old))
        )
        .is_some(),
        "a rename within the schema leaves the constraint's name where it is"
    );
    let view = || Module {
        kind: ModuleKind::View,
        description: None,
        definition: "SELECT 1".into(),
        options: Default::default(),
    };
    let create_view = || {
        PlannedChange::new(Change::CreateModule {
            id: ModuleId::Named(x.clone()),
            module: Box::new(view()),
        })
    };
    // A default goes with its column, whose drop runs before the tables
    // are created; a changed default is dropped only after them, in time
    // for a view but not for a table.
    let default_of = |column: &str| NameOccupant {
        parent_column: Some(column.into()),
        ..occupant("default constraint", Some(&old))
    };
    let column = pbps_model::ColumnRef {
        table: old.clone(),
        name: "c".into(),
    };
    let drop_column = || {
        PlannedChange::new(Change::DropColumn {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "dbo.old.c", 0),
            column: column.clone(),
        })
    };
    let redefault = || {
        PlannedChange::new(Change::AlterColumnDefault {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "dbo.old.c", 0),
            column: column.clone(),
            from: Some("0".into()),
            to: Some("1".into()),
        })
    };
    assert_eq!(
        refused(vec![drop_column(), create_table()], default_of("c")),
        None
    );
    assert!(
        refused(vec![drop_column(), create_table()], default_of("other")).is_some(),
        "another column's default stays"
    );
    assert!(refused(vec![create_table(), redefault()], default_of("c")).is_some());
    assert_eq!(
        refused(vec![redefault(), create_view()], default_of("c")),
        None
    );
    // An unnamed key's server-given name goes with it too.
    assert_eq!(
        refused(
            vec![
                PlannedChange::new(Change::SetPrimaryKey {
                    table: old.clone(),
                    from: Some(pbps_model::PrimaryKey {
                        name: None,
                        columns: vec!["id".into()],
                        storage_parameters: Default::default(),
                    }),
                    to: None,
                    nonclustered: false,
                }),
                create_table()
            ],
            occupant("primary key constraint", Some(&old))
        ),
        None
    );
    // A named key dropped outright goes before the tables; one replaced
    // in place goes with its add, after them.
    let key = |to: Option<pbps_model::PrimaryKey>| {
        PlannedChange::new(Change::SetPrimaryKey {
            table: old.clone(),
            from: Some(pbps_model::PrimaryKey {
                name: Some("x".into()),
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            }),
            to,
            nonclustered: false,
        })
    };
    let replacement = || {
        Some(pbps_model::PrimaryKey {
            name: Some("pk_new".into()),
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        })
    };
    let pk = || occupant("primary key constraint", Some(&old));
    assert_eq!(refused(vec![key(None), create_table()], pk()), None);
    assert!(refused(vec![create_table(), key(replacement())], pk()).is_some());
    assert_eq!(refused(vec![key(replacement()), create_view()], pk()), None);

    // A generated default a column rename moves out of the way, when
    // nothing holds the name it moves to (#1147 review).
    let generated = TableName::new(
        "dbo",
        pbps_mssql::emit::default_constraint_name(&old, "c").into_raw(),
    );
    let target = TableName::new(
        "dbo",
        pbps_mssql::emit::default_constraint_name(&old, "d").into_raw(),
    );
    let fallback = TableName::new(
        "dbo",
        pbps_mssql::emit::fallback_default_constraint_name(&old, "d").into_raw(),
    );
    let synonym_at = |name: &TableName| NameOccupant {
        wanted: name.clone(),
        name: name.clone(),
        kind: "synonym".into(),
        parent: None,
        parent_column: None,
    };
    let create_at = |name: &TableName| {
        PlannedChange::new(Change::CreateTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.n", 0),
            name: name.clone(),
            table: Box::default(),
            beside_default: None,
            moves_at_most: None,
        })
    };
    let rename_column = || {
        PlannedChange::new(Change::RenameColumn {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "dbo.old.c", 0),
            table: old.clone(),
            from: "c".into(),
            to: "d".into(),
            table_was: None,
        })
    };
    let the_default = NameOccupant {
        wanted: generated.clone(),
        name: generated.clone(),
        kind: "default constraint".into(),
        parent: Some(old.clone()),
        parent_column: Some("c".into()),
    };
    let moves = plan(vec![rename_column(), create_at(&generated)]);
    refuse_occupied_objects(&moves, std::slice::from_ref(&the_default), "prod")
        .expect("the rename moves the default out of the way");
    // A default another column owns under that name is not this
    // column's: the emitter matches the column (#991), so the rename
    // leaves it and the name stays held.
    let on_another_column = NameOccupant {
        parent_column: Some("b".into()),
        ..the_default.clone()
    };
    assert!(
        refuse_occupied_objects(&moves, &[on_another_column], "prod").is_err(),
        "another column's default under this column's name is not moved"
    );
    // #1201: a constraint the plan adds is checked like a created table.
    // With the target held, the rename parks the default at its fallback,
    // so a check added under that name after it is refused, naming the
    // default; under a free name it passes, and a check dropped and
    // added again under one name passes too.
    let check_named = |name: &str| {
        PlannedChange::new(Change::AddCheck {
            table: old.clone(),
            name: name.into(),
            constraint: pbps_model::CheckConstraint {
                expression: "1 = 1".into(),
            },
        })
    };
    let e = refuse_occupied_objects(
        &plan(vec![rename_column(), check_named(&fallback.name)]),
        &[the_default.clone(), synonym_at(&target)],
        "prod",
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains(&format!(
            "this plan puts default constraint `{fallback}`",
            fallback = fallback.shown()
        )),
        "{e}"
    );
    refuse_occupied_objects(
        &plan(vec![rename_column(), check_named("ck_free")]),
        &[the_default.clone(), synonym_at(&target)],
        "prod",
    )
    .expect("a check under a free name");
    let existing_check = NameOccupant {
        wanted: TableName::new("dbo", "ck_same"),
        name: TableName::new("dbo", "ck_same"),
        kind: "check constraint".into(),
        parent: Some(old.clone()),
        parent_column: None,
    };
    refuse_occupied_objects(
        &plan(vec![
            PlannedChange::new(Change::DropCheck {
                table: old.clone(),
                name: "ck_same".into(),
            }),
            check_named("ck_same"),
        ]),
        &[existing_check],
        "prod",
    )
    .expect("a check dropped and added again under its name");
    // And a new column's generated default, where the old default is
    // still held because target and fallback are both taken.
    let mut defaulted = pbps_model::Column::new("int".parse().unwrap());
    defaulted.value_source = pbps_model::ValueSource::Default("0".into());
    let add_c = PlannedChange::new(Change::AddColumn {
        uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.old", 0),
        table: old.clone(),
        name: "c".into(),
        column: Box::new(defaulted),
    });
    assert!(
        refuse_occupied_objects(
            &plan(vec![rename_column(), add_c]),
            &[
                the_default.clone(),
                synonym_at(&target),
                synonym_at(&fallback)
            ],
            "prod"
        )
        .is_err(),
        "the old default still holds the name the new column generates"
    );
    // #981: a taken target sends it to its digested fallback, which
    // frees the old name for the table created after it, as the emitted
    // SQL does. Only a taken fallback too keeps it where it is.
    refuse_occupied_objects(&moves, &[the_default.clone(), synonym_at(&target)], "prod")
        .expect("a taken target moves the default to its fallback");
    let e = refuse_occupied_objects(
        &moves,
        &[
            the_default.clone(),
            synonym_at(&target),
            synonym_at(&fallback),
        ],
        "prod",
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("default constraint"),
        "a taken target and fallback keep it: {e}"
    );
    // A check at the target that the plan drops first is gone by the
    // time the column rename runs (#1147 review).
    let check_at_target = NameOccupant {
        wanted: target.clone(),
        name: target.clone(),
        kind: "check constraint".into(),
        parent: Some(old.clone()),
        parent_column: None,
    };
    let drop_check = PlannedChange::new(Change::DropCheck {
        table: old.clone(),
        name: target.name.clone().into(),
    });
    refuse_occupied_objects(
        &plan(vec![drop_check, rename_column(), create_at(&generated)]),
        &[the_default.clone(), check_at_target.clone()],
        "prod",
    )
    .expect("the check is dropped before the rename looks");
    // The fallback is held here, so the order alone decides.
    assert!(
        refuse_occupied_objects(
            &moves,
            &[the_default.clone(), check_at_target, synonym_at(&fallback)],
            "prod"
        )
        .is_err(),
        "a check that stays still holds the target"
    );
    assert!(
        !e.contains("synonym"),
        "a name asked only to judge a move is not reported: {e}"
    );
    // A constraint the plan did not generate is not moved.
    assert!(
        refuse_occupied_objects(
            &moves,
            &[NameOccupant {
                kind: "check constraint".into(),
                ..the_default.clone()
            }],
            "prod"
        )
        .is_err()
    );
    // A check at the target on a table the plan transfers to another
    // schema leaves with its table before the column rename looks.
    let other = TableName::new("dbo", "other");
    let on_other = NameOccupant {
        wanted: target.clone(),
        name: target.clone(),
        kind: "check constraint".into(),
        parent: Some(other.clone()),
        parent_column: None,
    };
    let transfer = PlannedChange::new(Change::RenameTable {
        uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.other", 0),
        from: other.clone(),
        to: TableName::new("archive", "other"),
        defaults: Vec::new(),
    });
    refuse_occupied_objects(
        &plan(vec![transfer, rename_column(), create_at(&generated)]),
        &[the_default.clone(), on_other.clone()],
        "prod",
    )
    .expect("the transfer takes the check out of the schema first");
    assert!(
        refuse_occupied_objects(
            &moves,
            &[the_default.clone(), on_other, synonym_at(&fallback)],
            "prod"
        )
        .is_err(),
        "untransferred, the check still holds the target"
    );

    // A chain: `b -> c` moves `b`'s default away before `a -> b` needs
    // its name, so `a`'s old default name is free for a new table. In
    // the other order `a`'s target is still held, so it goes to its
    // digested fallback and the old name is free all the same (#981);
    // only a held fallback keeps it there.
    let named = |column: &str| {
        TableName::new(
            "dbo",
            pbps_mssql::emit::default_constraint_name(&old, column).into_raw(),
        )
    };
    let default_at = |column: &str| NameOccupant {
        wanted: named(column),
        name: named(column),
        kind: "default constraint".into(),
        parent: Some(old.clone()),
        parent_column: Some(column.into()),
    };
    let rename = |from: &str, to: &str| {
        PlannedChange::new(Change::RenameColumn {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "dbo.old.x", 0),
            table: old.clone(),
            from: from.into(),
            to: to.into(),
            table_was: None,
        })
    };
    let chain = [default_at("a"), default_at("b")];
    refuse_occupied_objects(
        &plan(vec![
            rename("b", "c"),
            rename("a", "b"),
            create_at(&named("a")),
        ]),
        &chain,
        "prod",
    )
    .expect("the chain frees `a`'s default name");
    let reversed = || {
        plan(vec![
            rename("a", "b"),
            rename("b", "c"),
            create_at(&named("a")),
        ])
    };
    refuse_occupied_objects(&reversed(), &chain, "prod")
        .expect("`a`'s default goes to its fallback while `b`'s name is held");
    let b_fallback = TableName::new(
        "dbo",
        pbps_mssql::emit::fallback_default_constraint_name(&old, "b").into_raw(),
    );
    assert!(
        refuse_occupied_objects(
            &reversed(),
            &[default_at("a"), default_at("b"), synonym_at(&b_fallback)],
            "prod",
        )
        .is_err(),
        "`a`'s target and fallback are both held when its rename runs"
    );

    // A table rename in its schema moves each default it lists.
    let renamed = TableName::new("dbo", "fresh");
    let table_moves = plan(vec![
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.old", 0),
            from: old.clone(),
            to: renamed.clone(),
            defaults: vec!["c".into()],
        }),
        create_at(&generated),
    ]);
    // The read covers the move's target and the renamed table's children.
    let (names, parents) = object_reads(&table_moves);
    assert!(
        names.contains(&TableName::new(
            "dbo",
            pbps_mssql::emit::default_constraint_name(&renamed, "c").into_raw()
        )),
        "{names:?}"
    );
    // #1215: two checks the plan adds meet under the database's answer on
    // which names are one; with no such answer they are two names.
    let other = TableName::new("dbo", "other_t");
    let check_on = |table: &TableName, name: &str| {
        PlannedChange::new(Change::AddCheck {
            table: table.clone(),
            name: name.into(),
            constraint: pbps_model::CheckConstraint {
                expression: "1 = 1".into(),
            },
        })
    };
    let both = plan(vec![check_on(&old, "Ck_Name"), check_on(&other, "ck_name")]);
    let alike = [(
        TableName::new("dbo", "Ck_Name"),
        TableName::new("dbo", "ck_name"),
    )];
    let e = refuse_occupied_objects_under(&both, &[], &alike, "prod")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("this plan puts check constraint `dbo.Ck_Name`"),
        "{e}"
    );
    refuse_occupied_objects_under(&both, &[], &[], "prod")
        .expect("two names where the database reads them as two");

    // A transfer's carried constraint is asked about under the name it
    // arrives with in the new schema (review of #1240).
    let carried = NameOccupant {
        wanted: TableName::new("dbo", "Ck_Name"),
        name: TableName::new("dbo", "Ck_Name"),
        kind: "check constraint".into(),
        parent: Some(old.clone()),
        parent_column: None,
    };
    let transfer_old = plan(vec![PlannedChange::new(Change::RenameTable {
        uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.old", 0),
        from: old.clone(),
        to: TableName::new("archive", "old"),
        defaults: Vec::new(),
    })]);
    let candidates = alike_candidates(&transfer_old, &[], std::slice::from_ref(&carried));
    assert!(
        candidates.contains(&TableName::new("archive", "Ck_Name")),
        "{candidates:?}"
    );
    assert!(
        candidates.contains(&TableName::new("dbo", "Ck_Name")),
        "{candidates:?}"
    );
    // A renamed table's new name is held and asked about too, so a later
    // name the database reads as the same one meets it.
    let into_ck = plan(vec![
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.old", 0),
            from: old.clone(),
            to: TableName::new("dbo", "Ck_Name"),
            defaults: Vec::new(),
        }),
        check_on(&other, "ck_name"),
    ]);
    assert!(alike_candidates(&into_ck, &[], &[]).contains(&TableName::new("dbo", "Ck_Name")));
    let e = refuse_occupied_objects_under(
        &into_ck,
        &[],
        &[(
            TableName::new("dbo", "Ck_Name"),
            TableName::new("dbo", "ck_name"),
        )],
        "prod",
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("this plan puts user table `dbo.Ck_Name`"), "{e}");
    refuse_occupied_objects_under(&into_ck, &[], &[], "prod")
        .expect("two names where the database reads them as two");
    // A rebuilt module is a name the plan creates too, though the read
    // leaves it out: it is asked about all the same.
    let rebuilt_view = ModuleId::Named(TableName::new("dbo", "Ck_Name"));
    let rebuild_view = plan(vec![
        PlannedChange::new(Change::DropModule {
            id: rebuilt_view.clone(),
            kind: pbps_model::ModuleKind::View,
        }),
        PlannedChange::new(Change::CreateModule {
            id: rebuilt_view,
            module: Box::new(pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: "CREATE VIEW dbo.Ck_Name AS SELECT 1 AS x".into(),
                options: Default::default(),
            }),
        }),
    ]);
    assert!(
        alike_candidates(&rebuild_view, &[], &[]).contains(&TableName::new("dbo", "Ck_Name")),
        "a rebuilt module is a candidate"
    );
    // And the walk meets it there under the database's answer.
    let archive_other = TableName::new("archive", "other");
    let mut carried_then_added = transfer_old.clone();
    carried_then_added
        .changes
        .push(check_on(&archive_other, "ck_name"));
    let e = refuse_occupied_objects_under(
        &carried_then_added,
        std::slice::from_ref(&carried),
        &[(
            TableName::new("archive", "Ck_Name"),
            TableName::new("archive", "ck_name"),
        )],
        "prod",
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("`archive.ck_name`"), "{e}");

    // #1214: every name the walk claims for an addition is read too, so
    // one already held in the database is found.
    let cs = plan(vec![
        PlannedChange::new(Change::AddCheck {
            table: old.clone(),
            name: "ck_added".into(),
            constraint: pbps_model::CheckConstraint {
                expression: "1 = 1".into(),
            },
        }),
        PlannedChange::new(Change::AddUnique {
            table: old.clone(),
            name: "uq_added".into(),
            constraint: pbps_model::UniqueConstraint {
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            },
            clustered: false,
        }),
    ]);
    let (added, _) = object_reads(&cs);
    for name in ["ck_added", "uq_added"] {
        assert!(
            added.contains(&TableName::new("dbo", name)),
            "{name}: {added:?}"
        );
    }
    // And the fallback the move takes when that is held (#981).
    assert!(
        names.contains(&TableName::new(
            "dbo",
            pbps_mssql::emit::fallback_default_constraint_name(&renamed, "c").into_raw()
        )),
        "{names:?}"
    );
    assert_eq!(parents, std::slice::from_ref(&old));
    refuse_occupied_objects(&table_moves, std::slice::from_ref(&the_default), "prod")
        .expect("the table rename moves the default out of the way");

    // An earlier table rename vacates a later one's target: `dbo.a`'s
    // default moves only once `dbo.b` has become `dbo.gone_b`, and the
    // walk sees that (#1147 review).
    let a = TableName::new("dbo", "a");
    let b = TableName::new("dbo", "b");
    let gone_b = TableName::new("dbo", "goneb");
    let default_on = |table: &TableName, column: &str| NameOccupant {
        wanted: TableName::new(
            "dbo",
            pbps_mssql::emit::default_constraint_name(table, column).into_raw(),
        ),
        name: TableName::new(
            "dbo",
            pbps_mssql::emit::default_constraint_name(table, column).into_raw(),
        ),
        kind: "default constraint".into(),
        parent: Some(table.clone()),
        parent_column: Some(column.into()),
    };
    let rename_table = |from: &TableName, to: &TableName| {
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.t", 0),
            from: from.clone(),
            to: to.clone(),
            defaults: vec!["c".into()],
        })
    };
    let a_default = default_on(&a, "c");
    refuse_occupied_objects(
        &plan(vec![
            rename_table(&b, &gone_b),
            rename_table(&a, &b),
            create_at(&a_default.name),
        ]),
        &[a_default.clone(), default_on(&b, "c")],
        "prod",
    )
    .expect("`b`'s default leaves before `a`'s arrives");
    // The other way round, `a`'s target is still held, so its default
    // takes its digested fallback and the old name is free all the same
    // (#981); only a held fallback keeps it.
    let a_first = || {
        plan(vec![
            rename_table(&a, &b),
            rename_table(&b, &gone_b),
            create_at(&a_default.name),
        ])
    };
    refuse_occupied_objects(
        &a_first(),
        &[a_default.clone(), default_on(&b, "c")],
        "prod",
    )
    .expect("`a`'s default takes its fallback while `b`'s is held");
    let a_fallback = TableName::new(
        "dbo",
        pbps_mssql::emit::fallback_default_constraint_name(&b, "c").into_raw(),
    );
    assert!(
        refuse_occupied_objects(
            &a_first(),
            &[
                a_default.clone(),
                default_on(&b, "c"),
                synonym_at(&a_fallback)
            ],
            "prod",
        )
        .is_err(),
        "the other way round, `a`'s target and fallback are both held"
    );

    // Names the plan moves into are taken (#1153): a default a rename
    // moves onto a created name, and a check a transfer carries into
    // the schema of one.
    let e = refuse_occupied_objects(
        &plan(vec![rename_column(), create_at(&target)]),
        std::slice::from_ref(&the_default),
        "prod",
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("this plan puts default constraint"), "{e}");
    let carried = NameOccupant {
        wanted: TableName::new("dbo", "carried"),
        name: TableName::new("dbo", "carried"),
        kind: "check constraint".into(),
        parent: Some(other.clone()),
        parent_column: None,
    };
    let e = refuse_occupied_objects(
        &plan(vec![
            PlannedChange::new(Change::RenameTable {
                uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.other", 0),
                from: other.clone(),
                to: TableName::new("archive", "other"),
                defaults: Vec::new(),
            }),
            create_at(&TableName::new("archive", "carried")),
        ]),
        &[carried],
        "prod",
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("check constraint `archive.carried` on `archive.other`"),
        "{e}"
    );
    // And a check the plan adds before a view at its name is created.
    let e = refuse_occupied_objects(
        &plan(vec![
            PlannedChange::new(Change::AddCheck {
                table: old.clone(),
                name: "x".into(),
                constraint: pbps_model::CheckConstraint {
                    expression: "1 = 1".into(),
                },
            }),
            create_view(),
        ]),
        &[],
        "prod",
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("this plan puts check constraint `dbo.x`"), "{e}");

    let rebuild = plan(vec![
        PlannedChange::new(Change::DropModule {
            id: ModuleId::Named(x.clone()),
            kind: ModuleKind::View,
        }),
        PlannedChange::new(Change::CreateModule {
            id: ModuleId::Named(x.clone()),
            module: Box::new(view()),
        }),
    ]);
    assert!(created_object_names(&rebuild).is_empty());
    // A trigger is asked of its table's schema.
    let trigger = plan(vec![PlannedChange::new(Change::CreateModule {
        id: ModuleId::Trigger {
            on: TableName::new("sales", "t"),
            name: "tr".into(),
        },
        module: Box::new(Module {
            kind: ModuleKind::Trigger,
            ..view()
        }),
    })]);
    assert_eq!(
        created_object_names(&trigger),
        [TableName::new("sales", "tr")]
    );

    // A system-versioned table's history takes a `sys.objects` name with
    // it, read and refused like the table's own (#1176).
    let history = TableName::new("hist", "x_history");
    let temporal = |versioned: bool| {
        let table = pbps_model::Table {
            system_time: Some(pbps_model::SystemTime {
                start: "vf".into(),
                end: "vt".into(),
                hidden: false,
                versioning: versioned.then(|| pbps_model::SystemVersioning {
                    history: history.clone(),
                    retention: None,
                }),
            }),
            ..Default::default()
        };
        plan(vec![PlannedChange::new(Change::CreateTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.x", 0),
            name: x.clone(),
            table: Box::new(table),
            beside_default: None,
            moves_at_most: None,
        })])
    };
    assert_eq!(
        created_object_names(&temporal(true)),
        [x.clone(), history.clone()]
    );
    let at_history = NameOccupant {
        wanted: history.clone(),
        name: history.clone(),
        kind: "user table".into(),
        parent: None,
        parent_column: None,
    };
    let e = refuse_occupied_objects(&temporal(true), std::slice::from_ref(&at_history), "prod")
        .unwrap_err()
        .to_string();
    assert!(e.contains("already has user table `hist.x_history`"), "{e}");
    // Negative: a period alone creates no history, and takes no name.
    assert_eq!(
        created_object_names(&temporal(false)),
        std::slice::from_ref(&x)
    );
    refuse_occupied_objects(&temporal(false), &[at_history], "prod")
        .expect("no history to collide");
}

/// DEC-1883.1: each date or time the engine read takes the place of the
/// text that declared it, a cell under its row's declared key before the
/// key itself is replaced, and a bound at both ends of its own column
/// only. Nothing it was not handed moves: another row, another column,
/// another key column's bound, the same text elsewhere.
#[test]
fn the_engines_reading_replaces_only_the_texts_it_was_read_for() {
    use pbps_db::catalog::{Misspelt, MisspeltBound};
    use pbps_model::{
        BoundDatum, Column, DataMode, PartitionBound, PartitionBy, PartitionOf, Row, RowKey, Table,
        TableData, Value,
    };
    let text = |t: &str| Value::Text(t.to_owned());
    let row =
        |cells: &[(&str, &str)]| Row(cells.iter().map(|(c, v)| ((*c).into(), text(v))).collect());
    let ev = TableName::new("app", "ev");
    let p = TableName::new("app", "p");
    let p_a = TableName::new("app", "p_a");
    let column = || Column::new("timestamp".parse().unwrap());
    let mut schema = Schema::default();
    schema.tables.insert(
        ev.clone(),
        Table {
            columns: [("k", column()), ("at", column()), ("note", column())]
                .into_iter()
                .map(|(n, c)| (n.into(), c))
                .collect(),
            data: Some(TableData {
                mode: DataMode::Exact,
                rows: [
                    (
                        RowKey::from("2026-02-01"),
                        row(&[("at", "2026-02-01"), ("note", "2026-02-01")]),
                    ),
                    (
                        RowKey::from("2026-03-01 00:00:00"),
                        row(&[("at", "2026-02-01")]),
                    ),
                ]
                .into_iter()
                .collect(),
            }),
            ..Table::default()
        },
    );
    schema.tables.insert(
        p.clone(),
        Table {
            columns: [("a", column()), ("b", column())]
                .into_iter()
                .map(|(n, c)| (n.into(), c))
                .collect(),
            partition_by: Some(PartitionBy {
                external: false,
                columns: vec!["a".into(), "b".into()],
                strategy: Default::default(),
            }),
            ..Table::default()
        },
    );
    let value = |v: &str| BoundDatum::Value(v.to_owned());
    schema.tables.insert(
        p_a.clone(),
        Table {
            partition_of: Some(PartitionOf {
                parent: p.clone(),
                bound: PartitionBound::Range {
                    from: vec![value("2026-02-01"), value("2026-02-01")],
                    to: vec![value("2026-02-01"), BoundDatum::MaxValue],
                },
                columns: Default::default(),
            }),
            ..Table::default()
        },
    );
    let cell = |key: &str, column: Option<&str>| Misspelt {
        table: ev.clone(),
        key: RowKey::from(key),
        column: column.map(Into::into),
        declared: "2026-02-01".to_owned(),
        ty: "timestamp without time zone".to_owned(),
        canonical: Some("2026-02-01 00:00:00".to_owned()),
        temporal: true,
    };
    let read = super::as_read(
        &schema,
        &[cell("2026-02-01", None), cell("2026-02-01", Some("at"))],
        &[MisspeltBound {
            partition: p_a.clone(),
            column: "a".into(),
            declared: "2026-02-01".to_owned(),
            ty: "timestamp without time zone".to_owned(),
            canonical: Some("2026-02-01 00:00:00".to_owned()),
            temporal: true,
        }],
    );
    let rows = &read.tables[&ev].data.as_ref().unwrap().rows;
    assert_eq!(
        rows.keys().map(RowKey::as_str).collect::<Vec<_>>(),
        ["2026-02-01 00:00:00", "2026-03-01 00:00:00"]
    );
    let moved = &rows[&RowKey::from("2026-02-01 00:00:00")];
    assert_eq!(moved.get("at"), Some(&text("2026-02-01 00:00:00")));
    // Not handed over: the same text in another column, and in another row.
    assert_eq!(moved.get("note"), Some(&text("2026-02-01")));
    assert_eq!(
        rows[&RowKey::from("2026-03-01 00:00:00")].get("at"),
        Some(&text("2026-02-01"))
    );
    let Some(PartitionOf {
        bound: PartitionBound::Range { from, to },
        ..
    }) = &read.tables[&p_a].partition_of
    else {
        panic!("the bound is still a range");
    };
    assert_eq!(from, &[value("2026-02-01 00:00:00"), value("2026-02-01")]);
    assert_eq!(to, &[value("2026-02-01 00:00:00"), BoundDatum::MaxValue]);
    // A list's value on its one key column is replaced wherever it stands
    // in the list; another value, and NULL, are not (#1632).
    let mut listed = schema.clone();
    listed
        .tables
        .get_mut(&p)
        .unwrap()
        .partition_by
        .as_mut()
        .unwrap()
        .strategy = pbps_model::PartitionStrategy::List;
    listed.tables.get_mut(&p_a).unwrap().partition_of = Some(PartitionOf {
        parent: p.clone(),
        bound: PartitionBound::List {
            values: vec![
                pbps_model::ListDatum::Null,
                pbps_model::ListDatum::Value("2026-02-01".into()),
                pbps_model::ListDatum::Value("2026-03-01".into()),
            ]
            .into_iter()
            .collect(),
        },
        columns: Default::default(),
    });
    let bound = |column: &str| MisspeltBound {
        partition: p_a.clone(),
        column: column.into(),
        declared: "2026-02-01".to_owned(),
        ty: "timestamp without time zone".to_owned(),
        canonical: Some("2026-02-01 00:00:00".to_owned()),
        temporal: true,
    };
    let read = super::as_read(&listed, &[], &[bound("a")]);
    assert_eq!(
        read.tables[&p_a].partition_of.as_ref().map(|of| &of.bound),
        Some(&PartitionBound::List {
            values: vec![
                pbps_model::ListDatum::Null,
                pbps_model::ListDatum::Value("2026-02-01 00:00:00".into()),
                pbps_model::ListDatum::Value("2026-03-01".into()),
            ]
            .into_iter()
            .collect(),
        })
    );
    // Negative: a list has no second key column to replace a value on.
    assert_eq!(super::as_read(&listed, &[], &[bound("b")]), listed);
    // Nothing to replace leaves the declarations as they were.
    assert_eq!(super::as_read(&schema, &[], &[]), schema);
}

/// A table this plan attaches is held to its declared shape once it reads
/// as a partition, as a created one is, and compared as the ordinary
/// table it was before then (#1545): neither read is movement, and
/// something another session adds to it is, before the attach or after.
#[test]
fn an_attached_table_is_held_to_its_shape_once_it_is_a_partition() {
    use pbps_model::{
        Change, ChangeSet, Column, PartitionBound, PartitionBy, PartitionOf, PlannedChange, Table,
    };
    let column = || Column::new("integer".parse().unwrap());
    let parent = Table {
        columns: [("id".into(), column())].into_iter().collect(),
        partition_by: Some(PartitionBy {
            external: false,
            columns: vec!["id".into()],
            strategy: Default::default(),
        }),
        ..Table::default()
    };
    let ordinary = Table {
        columns: [("id".into(), column())].into_iter().collect(),
        ..Table::default()
    };
    let shape = Table {
        partition_of: Some(PartitionOf {
            parent: TableName::new("app", "ev"),
            bound: PartitionBound::Default,
            columns: Default::default(),
        }),
        ..Table::default()
    };
    let schema = |t: &Table| {
        let mut s = Schema::default();
        s.tables.insert(TableName::new("app", "ev"), parent.clone());
        s.tables.insert(TableName::new("app", "t"), t.clone());
        s
    };
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(Change::AttachPartition {
            uid: "t_aaaaaa".parse().unwrap(),
            table: TableName::new("app", "t"),
            parent: TableName::new("app", "ev"),
            bound: PartitionBound::Default,
            key: Vec::new(),
            shape: Box::new(shape.clone()),
        })],
    };
    let before = schema(&ordinary);
    let held = |after: &Schema, settled: Settled| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &changes,
            &before,
            after,
            "prod",
            settled,
        )
        .map_err(|e| format!("{e:#}"))
    };
    held(&before, Settled::SoFar).expect("not attached yet");
    // Negative: once every statement has run, it must be attached.
    let e = held(&before, Settled::Whole).expect_err("detached again");
    assert!(
        e.contains("app.t is not attached to the parent and range this plan declares"),
        "{e}"
    );
    held(&schema(&shape), Settled::Whole).expect("attached as declared");
    // Negative: attached somewhere else than the plan says.
    let mut elsewhere = shape.clone();
    elsewhere.partition_of.as_mut().unwrap().bound = PartitionBound::Range {
        from: vec![pbps_model::BoundDatum::MinValue],
        to: vec![pbps_model::BoundDatum::MaxValue],
    };
    let e = held(&schema(&elsewhere), Settled::SoFar).expect_err("another range");
    assert!(
        e.contains("app.t is not attached to the parent and range this plan declares"),
        "{e}"
    );
    // Negative: an index another session adds, after the attach and
    // before it.
    let index = pbps_model::Index {
        columns: vec![pbps_model::IndexColumn {
            key: pbps_model::IndexKey::Column("id".into()),
            descending: false,
            opclass: None,
            collation: None,
        }],
        include: Vec::new(),
        unique: false,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
    };
    let mut moved = shape.clone();
    moved.indexes.insert("x".into(), index.clone());
    let e = held(&schema(&moved), Settled::Whole).expect_err("an index of its own");
    assert!(
        e.contains("app.t index `x` is there, and this plan declares no such index"),
        "{e}"
    );
    let mut moved = ordinary.clone();
    moved.indexes.insert("x".into(), index);
    held(&schema(&moved), Settled::SoFar).expect_err("an index before the attach");
}

/// An unlogged partition this plan creates under a parent a permanent
/// table's key references is refused, naming the table and the key
/// (#1595, DEC-1595.1). Not refused: a permanent partition, a key under
/// another parent, and a key this plan takes away before the partition's
/// `CREATE`, by dropping it, dropping its table or making that table
/// unlogged, under the name the catalog has now when the plan also
/// renames the table, and through the key a partitioned table declares
/// for its leaf's copy. A parent the plan renames is read back to the
/// catalog's name too (#1613).
/// #1489: a table this plan makes unlogged is refused by name when a
/// publication lists it, under the name the catalog has it by when the
/// plan starts. A table made logged is never asked about.
#[test]
fn a_table_made_unlogged_is_refused_by_the_publication_that_lists_it() {
    use pbps_model::{Change, ChangeSet, PlannedChange};
    use pbps_pg::catalog::NamingPublication;
    let uid = pbps_model::Uid::derived(pbps_model::UidKind::Table, "app.t", 0);
    let switch = |table: &str, unlogged: bool| {
        PlannedChange::new(Change::SetTablePersistence {
            uid: uid.clone(),
            table: table.parse().unwrap(),
            unlogged,
        })
    };
    let rename = PlannedChange::new(Change::RenameTable {
        uid: uid.clone(),
        from: "app.t".parse().unwrap(),
        to: "app.t2".parse().unwrap(),
        defaults: vec![],
    });
    let listing = |table: &str| NamingPublication {
        table: table.parse().unwrap(),
        publication: "pub_x".into(),
    };
    let refused = |changes: Vec<PlannedChange>, naming: &[NamingPublication]| {
        let cs = ChangeSet { changes };
        let ids = crate::plan_fixture::ids(&cs);
        let names = pbps_model::plan_names::PlanNames::of(&cs.changes, &ids).unwrap();
        super::refuse_published_unlogged(&super::tables_made_unlogged(&names), naming)
            .map_err(|e| e.to_string())
    };

    let e = refused(vec![switch("app.t", true)], &[listing("app.t")]).unwrap_err();
    assert!(
        e.contains("app.t is listed by the publication `pub_x`")
            && e.contains(r#"ALTER PUBLICATION "pub_x" DROP TABLE "app"."t";"#)
            && e.contains("DEC-1489.1"),
        "{e}"
    );
    // Renamed first: named as declared, and asked about, and fixed, under
    // the catalog's name for it.
    let e = refused(
        vec![rename.clone(), switch("app.t2", true)],
        &[listing("app.t")],
    )
    .unwrap_err();
    assert!(
        e.contains("app.t2 is listed by the publication `pub_x`")
            && e.contains(r#"ALTER PUBLICATION "pub_x" DROP TABLE "app"."t";`"#),
        "{e}"
    );
    // A name the display would escape gets no statement: shown escaped,
    // it would not be the statement that runs.
    let odd = NamingPublication {
        table: "app.t".parse().unwrap(),
        publication: "pub\\x".into(),
    };
    let e = refused(vec![switch("app.t", true)], &[odd]).unwrap_err();
    assert!(
        e.contains("remove it from that publication") && !e.contains("ALTER PUBLICATION"),
        "{e}"
    );
    // Negative: made logged, another table listed, or nothing listed.
    refused(vec![switch("app.t", false)], &[listing("app.t")]).expect("made logged");
    refused(vec![switch("app.t", true)], &[listing("app.other")]).expect("another table");
    refused(vec![switch("app.t", true)], &[]).expect("unpublished");
}

/// #1852: a partition a drop intent names is matched to the recorded state
/// by uid, so a rename before the drop still finds it, and an old
/// tombstone whose name was reused does not.
#[test]
fn a_baseline_refuses_handing_over_only_a_held_partition_a_drop_intent_names() {
    use pbps_model::{BoundDatum, PartitionBound, PartitionOf, Table, Uid, UidKind};
    let t = |n: &str| TableName::new("app", n);
    let year = |y: u32| PartitionBound::Range {
        from: vec![BoundDatum::Value(format!("{y}-01-01"))],
        to: vec![BoundDatum::Value(format!("{}-01-01", y + 1))],
    };
    let partition = |bound| Table {
        partition_of: Some(PartitionOf {
            parent: t("ev"),
            bound,
            columns: Default::default(),
        }),
        ..Default::default()
    };
    let uid = Uid::derived(UidKind::Table, "app.ev_a", 0);
    let parent = Uid::derived(UidKind::Table, "app.ev", 0);
    let tombstone = |was: &str| pbps_model::Tombstone {
        require: None,
        was: was.into(),
        dropped_at: "2026-10-18".into(),
        reason: "retired".into(),
        operator: "o".into(),
    };
    // Renamed by the intent twice before it was dropped: the tombstone
    // carries the last name, which neither the database nor the recorded
    // state needs to have.
    let mut working = IdsFile::default();
    working.tables.insert(parent.clone(), t("ev"));
    working
        .tombstones
        .insert(uid.clone(), tombstone("app.ev_c"));
    let column = Uid::derived(UidKind::Column, "app.ev_a.x", 0);
    working
        .tombstones
        .insert(column.clone(), tombstone("app.ev_a.x"));
    let dropped = super::tables_a_drop_intent_names(&working);
    assert_eq!(dropped, [&uid], "a column's tombstone is not a table's");

    // The scope's own reading: two pure partitions of a managed parent
    // whose partitions are external, which the working file does not hold.
    // The database has advanced the rename only as far as `ev_b`.
    let mut live = pbps_model::Schema::default();
    live.tables.insert(
        t("ev"),
        Table {
            partition_by: Some(pbps_model::PartitionBy {
                external: false,
                columns: vec!["ts".into()],
                strategy: Default::default(),
            }),
            ..Default::default()
        },
    );
    live.tables.insert(t("ev_b"), partition(year(2025)));
    live.tables.insert(t("ev_2026"), partition(year(2026)));
    let scoped = pbps_diff::scope(&live, &working, &Default::default(), &[t("ev")].into());
    assert_eq!(
        scoped.external.keys().collect::<Vec<_>>(),
        [&t("ev_2026"), &t("ev_b")]
    );

    // The recorded state: `holds` under `name` with `bound`, its parent
    // under `parent_uid`.
    let entry = |holds: &Uid, name: &str, bound, parent_uid: &Uid| {
        let mut ids = IdsFile::default();
        ids.tables.insert(parent_uid.clone(), t("ev"));
        ids.tables.insert(holds.clone(), t(name));
        let mut schema = pbps_model::Schema::default();
        schema.tables.insert(t(name), partition(bound));
        pbps_db::LedgerEntry {
            id: 3,
            applied_at: "2026-10-18T06:00:00.000".into(),
            snapshot: pbps_model::StateSnapshot::new(
                pbps_model::StateKind::Baseline,
                schema,
                ids,
                "o",
            ),
        }
    };
    let refuse = |previous| {
        super::refuse_handing_over_held_partitions(
            &dropped, &working, &scoped, previous, "prod", None,
        )
    };
    let e = refuse(Ok(Some(entry(&uid, "ev_a", year(2025), &parent))))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(
            "app.ev_b is named by a drop intent and held by the recorded state (entry #3, \
                 recorded as app.ev_a)"
        ) && e.contains("DEC-1852.1")
            && !e.contains("app.ev_2026"),
        "{e}"
    );
    // Not renamed at all: the recorded name is the live one.
    let e = refuse(Ok(Some(entry(&uid, "ev_b", year(2025), &parent))))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("app.ev_b is named by a drop intent and held by the recorded state (entry #3)"),
        "{e}"
    );
    // Rebound, or dropped by hand, while another partition of the parent
    // is left external: none has the recorded bound, so which it is cannot
    // be told, and the parent refuses it (#2177).
    let e = refuse(Ok(Some(entry(&uid, "ev_a", year(2024), &parent))))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(
            "app.ev_a is named by a drop intent and held by the recorded state (entry #3) as \
                 a partition of app.ev, whose partitions this baseline would leave external \
                 (app.ev_2026, app.ev_b); none has its bound"
        ) && e.contains("revert the drop intent"),
        "{e}"
    );
    // No partition of its parent is left external: nothing to hand over,
    // whatever another parent's are.
    let mut elsewhere = scoped.clone();
    elsewhere.external = [(
        t("other_1"),
        PartitionOf {
            parent: t("other"),
            bound: year(2025),
            columns: Default::default(),
        },
    )]
    .into();
    super::refuse_handing_over_held_partitions(
        &dropped,
        &working,
        &elsewhere,
        Ok(Some(entry(&uid, "ev_a", year(2025), &parent))),
        "prod",
        None,
    )
    .expect("another parent's partitions");
    // The parent the recorded state names is another table that had the
    // same name.
    let other_parent = Uid::derived(UidKind::Table, "app.ev", 1);
    refuse(Ok(Some(entry(&uid, "ev_a", year(2025), &other_parent)))).expect("another parent");
    // An old tombstone: the recorded state holds another table's uid.
    let other = Uid::derived(UidKind::Table, "app.ev_a", 1);
    refuse(Ok(Some(entry(&other, "ev_b", year(2025), &parent)))).expect("an old tombstone");
    // The first baseline: nothing recorded holds it.
    refuse(Ok(None)).expect("empty ledger");
    // Unreadable is not "not held": the partitions left external are named.
    let e = refuse(Err(pbps_db::LedgerError::Db(pbps_db::DbError::BadRow(
        "unreadable".into(),
    ))))
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("app.ev_b is left to its external parent, and whether a drop intent")
            && e.contains("app.ev_2026 is left")
            && e.contains("cannot be read"),
        "{e}"
    );
}

#[test]
fn an_unlogged_partition_under_a_permanent_key_is_refused_by_name() {
    use pbps_model::{Change, ChangeSet, PartitionBound, PartitionOf, PlannedChange, Table};
    use pbps_pg::catalog::PermanentReferencer;
    let parent = TableName::new("app", "ev");
    let partition = |unlogged: bool| {
        PlannedChange::new(Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: TableName::new("app", "ev_u"),
            table: Box::new(Table {
                partition_of: Some(PartitionOf {
                    parent: parent.clone(),
                    bound: PartitionBound::Default,
                    columns: Default::default(),
                }),
                unlogged,
                ..Table::default()
            }),
            beside_default: None,
            moves_at_most: None,
        })
    };
    let referencer = |of: &TableName| PermanentReferencer {
        parent: of.clone(),
        table: TableName::new("ext", "r"),
        key: "r_ev_fkey".into(),
        root_table: TableName::new("ext", "r"),
        root_key: "r_ev_fkey".into(),
    };
    // Staged unless a case says otherwise: the stricter of the two.
    let refused_as = |changes: Vec<PlannedChange>, of: &TableName, staged: bool| {
        refuse_permanent_referencers(&ChangeSet { changes }, &[referencer(of)], staged)
            .map_err(|e| e.to_string())
    };
    let refused = |changes: Vec<PlannedChange>, of: &TableName| refused_as(changes, of, true);
    let e = refused(vec![partition(true)], &parent).unwrap_err();
    assert!(
            e.contains("app.ev_u would be created unlogged under app.ev, which ext.r's foreign key `r_ev_fkey` references"),
            "{e}"
        );
    refused(vec![partition(false)], &parent).expect("a permanent partition");
    refused(vec![partition(true)], &TableName::new("app", "other"))
        .expect("a key to another parent");
    // A table attached unlogged is a new partition under the key all
    // the same (#1545), and one attached permanent is not.
    let attach = |unlogged: bool| {
        PlannedChange::new(Change::AttachPartition {
            uid: "t_bbbbbb".parse().unwrap(),
            table: TableName::new("app", "t"),
            parent: parent.clone(),
            bound: PartitionBound::Default,
            key: Vec::new(),
            shape: Box::new(Table {
                unlogged,
                ..Table::default()
            }),
        })
    };
    let e = refused(vec![attach(true)], &parent).unwrap_err();
    assert!(
            e.contains("app.t would be attached unlogged under app.ev, which ext.r's foreign key `r_ev_fkey` references"),
            "{e}"
        );
    refused(vec![attach(false)], &parent).expect("a permanent attach");
    assert_eq!(
        unlogged_partitions_created(&[attach(true), attach(false), partition(true)]),
        [
            (0, &TableName::new("app", "t"), &parent),
            (2, &TableName::new("app", "ev_u"), &parent)
        ]
    );
    let renamed = TableName::new("ext", "r2");
    for (what, first) in [
        (
            "its key dropped",
            vec![PlannedChange::new(Change::DropForeignKey {
                table: TableName::new("ext", "r"),
                name: "r_ev_fkey".into(),
            })],
        ),
        (
            "its table dropped",
            vec![PlannedChange::new(Change::DropTable {
                uid: "t_bbbbbb".parse().unwrap(),
                name: TableName::new("ext", "r"),
                detach_from: None,
            })],
        ),
        (
            "its table renamed and made unlogged",
            vec![
                PlannedChange::new(Change::RenameTable {
                    uid: "t_bbbbbb".parse().unwrap(),
                    from: TableName::new("ext", "r"),
                    to: renamed.clone(),
                    defaults: Vec::new(),
                }),
                PlannedChange::new(Change::SetTablePersistence {
                    uid: "t_bbbbbb".parse().unwrap(),
                    table: renamed.clone(),
                    unlogged: true,
                }),
            ],
        ),
    ] {
        let mut changes = first;
        changes.push(partition(true));
        refused(changes, &parent).expect(what);
    }
    // Negative: a key dropped on another table, or a table made logged,
    // still leaves this one's key in place.
    let e = refused(
        vec![
            PlannedChange::new(Change::DropForeignKey {
                table: TableName::new("ext", "other"),
                name: "r_ev_fkey".into(),
            }),
            PlannedChange::new(Change::SetTablePersistence {
                uid: "t_bbbbbb".parse().unwrap(),
                table: TableName::new("ext", "r"),
                unlogged: false,
            }),
            partition(true),
        ],
        &parent,
    )
    .unwrap_err();
    assert!(e.contains("ext.r's foreign key"), "{e}");
    // A removal after the partition's `CREATE`, as the persistence
    // switch sorts: a staged apply leaves the key in place across its
    // commits until it runs, and a transactional one commits both or
    // neither (#1612 review).
    let switched_after = || {
        vec![
            partition(true),
            PlannedChange::new(Change::SetTablePersistence {
                uid: "t_bbbbbb".parse().unwrap(),
                table: TableName::new("ext", "r"),
                unlogged: true,
            }),
        ]
    };
    let e = refused(switched_after(), &parent).unwrap_err();
    assert!(e.contains("ext.r's foreign key"), "{e}");
    refused_as(switched_after(), &parent, false).expect("one transaction");

    // A table dropped from the name another is renamed into: the drop
    // is of the table that leaves, which the catalog names (#1612
    // review).
    refused(
        vec![
            PlannedChange::new(Change::DropTable {
                uid: "t_bbbbbb".parse().unwrap(),
                name: TableName::new("ext", "r"),
                detach_from: None,
            }),
            PlannedChange::new(Change::RenameTable {
                uid: "t_cccccc".parse().unwrap(),
                from: TableName::new("ext", "other"),
                to: TableName::new("ext", "r"),
                defaults: Vec::new(),
            }),
            partition(true),
        ],
        &parent,
    )
    .expect("the referencing table dropped before its name is taken");
    // A freeing FK drop may precede its owner's rename (DECISIONS 496).
    // The old name's next holder must not receive that removal (#1770).
    for (key_table, before_renames, allowed) in
        [("r", true, true), ("r2", false, true), ("r", false, false)]
    {
        let drop_key = PlannedChange::new(Change::DropForeignKey {
            table: TableName::new("ext", key_table),
            name: "r_ev_fkey".into(),
        });
        let mut changes = vec![
            PlannedChange::new(Change::RenameTable {
                uid: "t_bbbbbb".parse().unwrap(),
                from: TableName::new("ext", "r"),
                to: renamed.clone(),
                defaults: Vec::new(),
            }),
            PlannedChange::new(Change::RenameTable {
                uid: "t_cccccc".parse().unwrap(),
                from: TableName::new("ext", "other"),
                to: TableName::new("ext", "r"),
                defaults: Vec::new(),
            }),
        ];
        changes.insert(if before_renames { 0 } else { 2 }, drop_key);
        changes.push(partition(true));
        for staged in [false, true] {
            let result = refused_as(changes.clone(), &parent, staged);
            assert_eq!(
                result.is_ok(),
                allowed,
                "drop on {key_table}, before renames: {before_renames}, staged: {staged}: {result:?}"
            );
        }
    }
    // Negative: the newcomer made unlogged is not the referencer.
    let e = refused(
        vec![
            PlannedChange::new(Change::RenameTable {
                uid: "t_cccccc".parse().unwrap(),
                from: TableName::new("ext", "other"),
                to: TableName::new("ext", "r"),
                defaults: Vec::new(),
            }),
            PlannedChange::new(Change::SetTablePersistence {
                uid: "t_cccccc".parse().unwrap(),
                table: TableName::new("ext", "r"),
                unlogged: true,
            }),
            partition(true),
        ],
        &parent,
    )
    .unwrap_err();
    assert!(e.contains("ext.r's foreign key"), "{e}");

    // A partitioned referencing table: its leaf's copy, under a name of
    // its own, goes with the key the table declares, and is named
    // through it (#1612 review).
    let leaf = PermanentReferencer {
        parent: parent.clone(),
        table: TableName::new("ext", "rp_1"),
        key: "own_name".into(),
        root_table: TableName::new("ext", "rp"),
        root_key: "rp_k".into(),
    };
    let drop_root = PlannedChange::new(Change::DropForeignKey {
        table: TableName::new("ext", "rp"),
        name: "rp_k".into(),
    });
    let leaf_refused = |changes: Vec<PlannedChange>| {
        refuse_permanent_referencers(&ChangeSet { changes }, std::slice::from_ref(&leaf), true)
            .map_err(|e| e.to_string())
    };
    leaf_refused(vec![drop_root.clone(), partition(true)]).expect("the declared key dropped");
    let e = leaf_refused(vec![partition(true)]).unwrap_err();
    assert!(
        e.contains("ext.rp's foreign key `rp_k` (on its partition ext.rp_1 as `own_name`)"),
        "{e}"
    );
    let e = leaf_refused(vec![partition(true), drop_root]).unwrap_err();
    assert!(e.contains("ext.rp's foreign key `rp_k`"), "{e}");

    // The parent renamed in the plan that adds the partition under its
    // new name: the catalog's referencer names it as it is now, and is
    // the same key (#1613).
    let ev2 = TableName::new("app", "ev2");
    let under_ev2 = PlannedChange::new(Change::CreateTable {
        uid: "t_aaaaaa".parse().unwrap(),
        name: TableName::new("app", "ev_u"),
        table: Box::new(Table {
            partition_of: Some(PartitionOf {
                parent: ev2.clone(),
                bound: PartitionBound::Default,
                columns: Default::default(),
            }),
            unlogged: true,
            ..Table::default()
        }),
        beside_default: None,
        moves_at_most: None,
    });
    let rename_parent = PlannedChange::new(Change::RenameTable {
        uid: "t_dddddd".parse().unwrap(),
        from: parent.clone(),
        to: ev2.clone(),
        defaults: Vec::new(),
    });
    let e = refused(vec![rename_parent.clone(), under_ev2.clone()], &parent).unwrap_err();
    assert!(e.contains("ext.r's foreign key `r_ev_fkey`"), "{e}");
    // Negative: a key under the new name, which no table the catalog
    // reads holds, is another parent's.
    refused(vec![rename_parent, under_ev2], &ev2).expect("the catalog's name is the old one");
}

/// An index the plan creates takes a relation name too (#1355): one at
/// the name of a sequence the database holds, or of one a cross-schema
/// rename carries in first, is refused like a table there. A unique
/// constraint's and a named key's index are named after them. Not
/// occupants: an index the plan drops first, and an unnamed key, whose
/// `<table>_pkey` PostgreSQL moves aside when taken (DEC-465.1).
#[test]
fn an_index_the_plan_creates_meets_held_and_carried_names() {
    use pbps_model::{Change, ChangeSet, PlannedChange, PrimaryKey, UniqueConstraint};
    use pbps_pg::catalog::NameOccupant;
    let t = TableName::new("archive", "t");
    let index: Box<pbps_model::Index> =
        Box::new(serde_json::from_str(r#"{"columns": [{"name": "n"}]}"#).unwrap());
    let add_index = |name: &str| {
        PlannedChange::new(Change::AddIndex {
            table: t.clone(),
            name: name.into(),
            index: index.clone(),
            clustered: false,
        })
    };
    let add_unique = |name: &str| {
        PlannedChange::new(Change::AddUnique {
            table: t.clone(),
            name: name.into(),
            constraint: UniqueConstraint {
                columns: vec!["n".into()],
                storage_parameters: Default::default(),
            },
            clustered: false,
        })
    };
    let set_key = |name: Option<&str>| {
        PlannedChange::new(Change::SetPrimaryKey {
            table: t.clone(),
            from: None,
            to: Some(PrimaryKey {
                name: name.map(Into::into),
                columns: vec!["n".into()],
                storage_parameters: Default::default(),
            }),
            nonclustered: false,
        })
    };
    let held = NameOccupant {
        name: TableName::new("archive", "s"),
        kind: "sequence",
        owner: None,
        owner_column: None,
    };
    let plan = |changes: Vec<PlannedChange>| ChangeSet { changes };
    let refused = |changes: Vec<PlannedChange>, occupant: &NameOccupant| {
        refuse_uninventoried_occupants(
            &plan(changes),
            std::slice::from_ref(occupant),
            &[],
            "prod",
            &Default::default(),
        )
        .map_err(|e| e.to_string())
    };

    for change in [add_index("s"), add_unique("s"), set_key(Some("s"))] {
        let e = refused(vec![change], &held).unwrap_err();
        assert!(
            e.contains("`archive.s`: the database already has sequence `archive.s`"),
            "{e}"
        );
    }
    let mut declared = pbps_model::Table::default();
    declared.indexes.insert("s".into(), (*index).clone());
    let created = PlannedChange::new(Change::CreateTable {
        uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "archive.u", 0),
        name: TableName::new("archive", "u"),
        table: Box::new(declared),
        beside_default: None,
        moves_at_most: None,
    });
    assert!(
        refused(vec![created], &held).is_err(),
        "a created table's own index is created too"
    );

    // Carried in first: `app.old` moves to `archive` with its sequence.
    let old = TableName::new("app", "old");
    let carried = NameOccupant {
        name: TableName::new("app", "old_n_seq"),
        kind: "sequence",
        owner: Some(old.clone()),
        owner_column: Some("n".into()),
    };
    let to_archive = PlannedChange::new(Change::RenameTable {
        uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "app.old", 0),
        from: old,
        to: TableName::new("archive", "new"),
        defaults: Vec::new(),
    });
    let e = refused(vec![to_archive.clone(), add_index("old_n_seq")], &carried).unwrap_err();
    assert!(
        e.contains("`archive.old_n_seq`: this plan moves sequence `app.old_n_seq`"),
        "{e}"
    );

    // A numbering change that drops the column's sequence, or renames
    // it, frees its name; a retune keeps it (#1930 review).
    let numbered = NameOccupant {
        name: TableName::new("archive", "t_n_seq"),
        kind: "sequence",
        owner: Some(t.clone()),
        owner_column: Some("n".into()),
    };
    let serial = |name: Option<&str>, start| {
        pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence {
            start,
            increment: 1,
            name: name.map(Into::into),
            options: Default::default(),
        })
    };
    let numbering = |from, to| {
        PlannedChange::new(Change::AlterColumnNumbering {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "archive.t.n", 0),
            column: pbps_model::ColumnRef::new(t.clone(), "n"),
            ty: "integer".parse().unwrap(),
            from,
            to,
        })
    };
    for to in [
        pbps_model::ValueSource::None,
        serial(Some("other_seq"), 1),
        pbps_model::ValueSource::Identity(pbps_model::Identity {
            seed: 1,
            increment: 1,
            options: Default::default(),
        }),
    ] {
        refused(
            vec![
                numbering(serial(Some("t_n_seq"), 1), to),
                add_index("t_n_seq"),
            ],
            &numbered,
        )
        .unwrap();
    }
    let e = refused(
        vec![
            numbering(serial(Some("t_n_seq"), 1), serial(None, 5)),
            add_index("t_n_seq"),
        ],
        &numbered,
    )
    .unwrap_err();
    assert!(
        e.contains("`archive.t_n_seq`"),
        "a retune keeps the name: {e}"
    );
    // Under a column this plan renames first, the sequence follows it.
    let renamed = PlannedChange::new(Change::RenameColumn {
        uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "archive.t.n", 0),
        table: t.clone(),
        from: "n".into(),
        to: "m".into(),
        table_was: None,
    });
    let numbering_m = PlannedChange::new(Change::AlterColumnNumbering {
        uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "archive.t.n", 0),
        column: pbps_model::ColumnRef::new(t.clone(), "m"),
        ty: "integer".parse().unwrap(),
        from: serial(Some("t_n_seq"), 1),
        to: pbps_model::ValueSource::None,
    });
    refused(
        vec![renamed.clone(), numbering_m, add_index("t_n_seq")],
        &numbered,
    )
    .unwrap();
    // Negative: the old column name, after the rename, frees nothing.
    let e = refused(
        vec![
            renamed,
            numbering(serial(Some("t_n_seq"), 1), pbps_model::ValueSource::None),
            add_index("t_n_seq"),
        ],
        &numbered,
    )
    .unwrap_err();
    assert!(e.contains("`archive.t_n_seq`"), "{e}");

    // Negatives: a free name, an index dropped first, an unnamed key.
    refused(vec![add_index("free")], &held).unwrap();
    refused(vec![to_archive, add_index("free")], &carried).unwrap();
    let own = NameOccupant {
        name: TableName::new("archive", "ix"),
        kind: "index",
        owner: Some(t.clone()),
        owner_column: None,
    };
    let drop = PlannedChange::new(Change::DropIndex {
        table: t.clone(),
        name: "ix".into(),
    });
    refused(vec![drop, add_index("ix")], &own).unwrap();
    let pkey = NameOccupant {
        name: TableName::new("archive", "t_pkey"),
        kind: "sequence",
        owner: None,
        owner_column: None,
    };
    refused(vec![set_key(None)], &pkey).unwrap();
}

/// #1723: a change names a table as it is at that point of the plan,
/// and `rename_order` puts a drop before a rename or after it, so each
/// occupant is followed through the renames in the plan's order. With
/// `s2.b -> s3.c` then `s1.a -> s2.b`, an index dropped from `s2.b`
/// before both is the original `s2.b`'s, which then never reaches `s3`,
/// while `s1.a`'s holds its name in `s1` until it moves to `s2`.
#[test]
fn an_occupant_is_matched_by_the_name_its_owner_has_at_each_change() {
    use pbps_model::{Change, ChangeSet, PlannedChange};
    use pbps_pg::catalog::NameOccupant;
    let (a, b, c) = (
        TableName::new("s1", "a"),
        TableName::new("s2", "b"),
        TableName::new("s3", "c"),
    );
    let index: Box<pbps_model::Index> =
        Box::new(serde_json::from_str(r#"{"columns": [{"name": "n"}]}"#).unwrap());
    let add_index = |table: &TableName| {
        PlannedChange::new(Change::AddIndex {
            table: table.clone(),
            name: "n_pkey".into(),
            index: index.clone(),
            clustered: false,
        })
    };
    let rename = |from: &TableName, to: &TableName| {
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, &from.raw().to_string(), 0),
            from: from.clone(),
            to: to.clone(),
            defaults: Vec::new(),
        })
    };
    let moves = || {
        vec![
            PlannedChange::new(Change::DropIndex {
                table: b.clone(),
                name: "n_pkey".into(),
            }),
            rename(&b, &c),
            rename(&a, &b),
        ]
    };
    let held = |owner: &TableName| NameOccupant {
        name: TableName::new(owner.schema.clone(), "n_pkey"),
        kind: "index",
        owner: Some(owner.clone()),
        owner_column: None,
    };
    let occupants = [held(&a), held(&b)];
    let refused = |changes: Vec<PlannedChange>| {
        refuse_uninventoried_occupants(
            &ChangeSet { changes },
            &occupants,
            &[],
            "prod",
            &Default::default(),
        )
        .map_err(|e| e.to_string())
    };

    let mut in_s3 = moves();
    in_s3.push(add_index(&c));
    refused(in_s3).expect("the dropped index never reaches s3");
    let x = TableName::new("s1", "x");
    let mut before = vec![add_index(&x)];
    before.extend(moves());
    let e = refused(before).unwrap_err();
    assert!(
        e.contains("`s1.n_pkey`: the database already has index `s1.n_pkey` on `s1.a`"),
        "{e}"
    );
    let mut in_s2 = moves();
    in_s2.push(add_index(&b));
    let e = refused(in_s2).unwrap_err();
    assert!(
        e.contains(
            "`s2.n_pkey`: this plan moves index `s1.n_pkey` on `s1.a` there first, with \
                 the rename of `s1.a` to `s2.b`"
        ),
        "{e}"
    );

    // Negative: once `s1.a` has moved, its name in `s1` is free.
    let mut after = moves();
    after.push(add_index(&x));
    refused(after).expect("s1.a's index left s1 with it");
}

/// #1749: `SET SCHEMA` refuses to carry an index or a sequence onto a
/// name another relation holds in the destination (measured on 16 and
/// 18), one an earlier rename carried there included. Negatives: distinct
/// names pass, and one the plan drops first frees the name.
#[test]
fn a_carried_occupant_meets_what_already_holds_its_name() {
    use pbps_model::{Change, ChangeSet, PlannedChange};
    use pbps_pg::catalog::NameOccupant;
    let rename = |from: &TableName, to: &TableName| {
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, &from.raw().to_string(), 0),
            from: from.clone(),
            to: to.clone(),
            defaults: Vec::new(),
        })
    };
    let occupant = |at: &TableName, kind, owner: Option<&TableName>| NameOccupant {
        name: at.clone(),
        kind,
        owner: owner.cloned(),
        owner_column: None,
    };
    let refused = |changes: Vec<PlannedChange>, occupants: &[NameOccupant]| {
        refuse_uninventoried_occupants(
            &ChangeSet { changes },
            occupants,
            &[],
            "prod",
            &Default::default(),
        )
        .map_err(|e| e.to_string())
    };
    let (a, x) = (TableName::new("s1", "a"), TableName::new("s4", "x"));
    let (a_to, x_to) = (TableName::new("s2", "b"), TableName::new("s2", "y"));
    let index = occupant(&TableName::new("s1", "x_id_seq"), "index", Some(&a));
    let sequence = NameOccupant {
        owner_column: Some("id".into()),
        ..occupant(&TableName::new("s4", "x_id_seq"), "sequence", Some(&x))
    };

    // Two moves land on `s2.x_id_seq`: the second is refused, naming both.
    let e = refused(
        vec![rename(&a, &a_to), rename(&x, &x_to)],
        &[index.clone(), sequence.clone()],
    )
    .unwrap_err();
    assert!(
        e.contains(
            "`s2.x_id_seq`: this plan moves sequence `s4.x_id_seq` on `s4.x` there, with \
                 the rename of `s4.x` to `s2.y`, where index `s1.x_id_seq`, which the rename \
                 of `s1.a` to `s2.b` moves there first"
        ),
        "{e}"
    );
    assert!(
        e.contains("apply that rename in a plan of its own first"),
        "the remedy for a holder a later rename moves away: {e}"
    );
    // One already there, owned by nothing the plan moves.
    let held = occupant(&TableName::new("s2", "x_id_seq"), "sequence", None);
    let e = refused(vec![rename(&a, &a_to)], &[index.clone(), held.clone()]).unwrap_err();
    assert!(e.contains("where sequence `s2.x_id_seq` already is"), "{e}");

    // Negatives: another name lands free, and one dropped first frees it.
    let other = occupant(&TableName::new("s1", "a_ix"), "index", Some(&a));
    refused(
        vec![rename(&a, &a_to), rename(&x, &x_to)],
        &[other, sequence],
    )
    .expect("distinct names land apart");
    let drop_index = PlannedChange::new(Change::DropIndex {
        table: a.clone(),
        name: "x_id_seq".into(),
    });
    refused(vec![drop_index, rename(&a, &a_to)], &[index, held])
        .expect("the index is dropped before its table moves");
}

/// #1765: a table or view at a carried object's landing name, managed or
/// not, refuses the move, `SET SCHEMA` refusing it (measured on 18), and
/// so does a table an earlier rename moves there. Negatives: a holder
/// elsewhere, a rename within the schema, a holder the plan drops or
/// renames away first, and an index dropped before its table moves.
#[test]
fn a_carried_index_meets_the_table_or_view_at_its_landing_name() {
    use pbps_model::{Change, ChangeSet, ModuleKind, PlannedChange};
    use pbps_pg::catalog::NameOccupant;
    let rename = |from: &TableName, to: TableName| {
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, &from.raw().to_string(), 0),
            from: from.clone(),
            to,
            defaults: Vec::new(),
        })
    };
    let a = TableName::new("s1", "a");
    let landing = TableName::new("s2", "n");
    let moved = || rename(&a, TableName::new("s2", "a"));
    let index = NameOccupant {
        name: TableName::new("s1", "n"),
        kind: "index",
        owner: Some(a.clone()),
        owner_column: None,
    };
    let refused = |changes: Vec<PlannedChange>, relations: &[(TableName, &'static str)]| {
        refuse_uninventoried_occupants(
            &ChangeSet { changes },
            std::slice::from_ref(&index),
            relations,
            "prod",
            &Default::default(),
        )
        .map_err(|e| e.to_string())
    };
    let move_onto = "`s2.n`: this plan moves index `s1.n` on `s1.a` there, with the rename of \
                         `s1.a` to `s2.a`, where";
    for kind in ["table", "view"] {
        let e = refused(vec![moved()], &[(landing.clone(), kind)]).unwrap_err();
        assert!(
            e.contains(&format!("{move_onto} {kind} `s2.n` already is")),
            "{e}"
        );
    }
    let q = TableName::new("s3", "q");
    let e = refused(vec![rename(&q, landing.clone()), moved()], &[]).unwrap_err();
    assert!(
        e.contains(&format!(
            "{move_onto} table `s2.n`, which the rename of `s3.q` to `s2.n` moves there first"
        )),
        "{e}"
    );

    // Negatives.
    let held = [(landing.clone(), "table")];
    refused(vec![moved()], &[(TableName::new("s3", "n"), "table")]).expect("nothing holds `s2.n`");
    refused(vec![rename(&a, TableName::new("s1", "b"))], &held)
        .expect("a rename within `s1` lands nothing in `s2`");
    let drop_table = PlannedChange::new(Change::DropTable {
        uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "s2.n", 0),
        name: landing.clone(),
        detach_from: None,
    });
    refused(vec![drop_table, moved()], &held).expect("the holder is dropped first");
    refused(
        vec![rename(&landing, TableName::new("s2", "m")), moved()],
        &held,
    )
    .expect("the holder is renamed away first");
    let drop_view = PlannedChange::new(Change::DropModule {
        id: ModuleId::Named(landing.clone()),
        kind: ModuleKind::View,
    });
    refused(vec![drop_view, moved()], &[(landing.clone(), "view")])
        .expect("the view is dropped first");
    let drop_index = PlannedChange::new(Change::DropIndex {
        table: a.clone(),
        name: "n".into(),
    });
    refused(vec![drop_index, moved()], &held).expect("the index is dropped before its table moves");
    // The holder moved away after the move is still there when it runs.
    let e = refused(
        vec![moved(), rename(&landing, TableName::new("s2", "m"))],
        &held,
    )
    .unwrap_err();
    assert!(e.contains("table `s2.n` already is"), "{e}");
}

/// The plan's two sides for [`order_carried_moves`]: every table the
/// renames move, at its source before and its target after, beside
/// `others`, which stay.
fn carried_sides(
    changes: &[pbps_model::PlannedChange],
    others: &[TableName],
) -> (
    pbps_model::Schema,
    pbps_model::IdsFile,
    pbps_model::Schema,
    pbps_model::IdsFile,
) {
    let mut sides = (
        pbps_model::Schema::default(),
        pbps_model::IdsFile::default(),
        pbps_model::Schema::default(),
        pbps_model::IdsFile::default(),
    );
    let mut put = |uid: pbps_model::Uid, before: &TableName, after: &TableName| {
        sides
            .0
            .tables
            .insert(before.clone(), pbps_model::Table::default());
        sides.1.tables.insert(uid.clone(), before.clone());
        sides
            .2
            .tables
            .insert(after.clone(), pbps_model::Table::default());
        sides.3.tables.insert(uid, after.clone());
    };
    for p in changes {
        if let pbps_model::Change::RenameTable { uid, from, to, .. } = &p.change {
            put(uid.clone(), from, to);
        }
    }
    for other in others {
        let uid = pbps_model::Uid::derived(pbps_model::UidKind::Table, &other.raw().to_string(), 0);
        put(uid, other, other);
    }
    sides
}

/// [`order_carried_moves`] over `changes`, with the tables the renames
/// move and `others` as the plan's sides.
fn carried_moves(
    changes: Vec<pbps_model::PlannedChange>,
    occupants: &[pbps_pg::catalog::NameOccupant],
    others: &[TableName],
) -> pbps_model::ChangeSet {
    carried_moves_beside(changes, occupants, &[], others)
}

/// [`carried_moves`], with the tables and views the catalog read at the
/// names the renames take.
fn carried_moves_beside(
    changes: Vec<pbps_model::PlannedChange>,
    occupants: &[pbps_pg::catalog::NameOccupant],
    relations: &[(TableName, &'static str)],
    others: &[TableName],
) -> pbps_model::ChangeSet {
    let (before, before_ids, after, after_ids) = carried_sides(&changes, others);
    let mut cs = pbps_model::ChangeSet { changes };
    order_carried_moves(
        &mut cs,
        occupants,
        relations,
        (
            pbps_diff::Side {
                schema: &before,
                ids: &before_ids,
            },
            pbps_diff::Side {
                schema: &after,
                ids: &after_ids,
            },
        ),
        &pbps_pg::Postgres::new(),
        &Default::default(),
    );
    cs
}

#[test]
fn only_uniquely_identified_renamed_clones_release_their_names() {
    use pbps_model::{DetachedKind, DetachedName, Uid, UidKind};
    let uid = Uid::derived(UidKind::Table, "partition", 0);
    for kind in [
        DetachedKind::Index,
        DetachedKind::PrimaryKey,
        DetachedKind::Unique,
    ] {
        let clone = pbps_pg::catalog::DetachClone {
            kind,
            parent: "parent_object".into(),
            name: TableName::new("app", "engine_name"),
        };
        let wanted = DetachedName {
            kind,
            parent: "parent_object".into(),
            name: Some("declared".into()),
        };
        let read = detach_clone_slots(
            &uid,
            std::slice::from_ref(&wanted),
            std::slice::from_ref(&clone),
        );
        assert_eq!(read.detach_releases.len(), 1);
        assert_eq!(read.detach_releases.values().next(), Some(&uid));
        assert!(read.detach_retained.is_empty());
        assert_eq!(
            detach_clone_slots(&uid, std::slice::from_ref(&wanted), &[]),
            Default::default()
        );
        assert_eq!(
            detach_clone_slots(
                &uid,
                std::slice::from_ref(&wanted),
                &[clone.clone(), clone.clone()]
            ),
            Default::default()
        );
        for kept in [None, Some("engine_name".into())] {
            let retained = kept.is_some();
            let wanted = DetachedName {
                name: kept,
                ..wanted.clone()
            };
            let read = detach_clone_slots(&uid, &[wanted], std::slice::from_ref(&clone));
            assert!(read.detach_releases.is_empty());
            assert_eq!(read.detach_retained.len(), usize::from(retained));
            if retained {
                assert_eq!(read.detach_retained.values().next(), Some(&uid));
            }
        }
        let another = pbps_pg::catalog::DetachClone {
            kind: DetachedKind::Check,
            ..clone
        };
        assert_eq!(
            detach_clone_slots(&uid, &[wanted], &[another]),
            Default::default()
        );
    }
}

#[test]
fn a_detach_refuses_owned_non_clones_at_its_requested_clone_name() {
    use pbps_model::{Change, ChangeSet, DetachedKind, DetachedName, PlannedChange, Uid, UidKind};
    use pbps_pg::catalog::{DetachClone, NameOccupant};
    let table = TableName::new("app", "a");
    let uid = Uid::derived(UidKind::Table, "a", 0);
    let names = vec![DetachedName {
        kind: DetachedKind::Index,
        parent: "p_index".into(),
        name: Some("claimed".into()),
    }];
    let clone = DetachClone {
        kind: DetachedKind::Index,
        parent: "p_index".into(),
        name: TableName::new("app", "a_index"),
    };
    let facts = detach_clone_slots(&uid, &names, std::slice::from_ref(&clone));
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(Change::DetachPartition {
            uid,
            table: table.clone(),
            parent: TableName::new("app", "p"),
            names,
            shape: Box::default(),
        })],
    };
    for kind in ["sequence", "index"] {
        let occupants = [
            NameOccupant {
                name: clone.name.clone(),
                kind: "index",
                owner: Some(table.clone()),
                owner_column: None,
            },
            NameOccupant {
                name: TableName::new("app", "claimed"),
                kind,
                owner: Some(table.clone()),
                owner_column: (kind == "sequence").then(|| "id".into()),
            },
        ];
        let error = refuse_uninventoried_occupants(&changes, &occupants, &[], "prod", &facts)
            .expect_err("the detach does not rename this same-owner occupant")
            .to_string();
        assert!(
            error.contains("detach of `app.a` claims `app.claimed`"),
            "{error}"
        );
        assert!(error.contains(&format!("{kind} `app.claimed`")), "{error}");
    }
}

#[test]
fn a_detach_admits_its_exact_retained_clones_and_internal_exchanges() {
    use pbps_model::{
        Change, ChangeSet, DetachedKind as K, DetachedName, PlannedChange, Uid, UidKind,
    };
    use pbps_pg::catalog::{DetachClone, NameOccupant};
    let table = TableName::new("app", "a");
    let uid = Uid::derived(UidKind::Table, "a", 0);
    for exchange in [false, true] {
        let bindings = [
            (
                K::PrimaryKey,
                "p_pk",
                "a_key",
                if exchange { "a_unique" } else { "a_key" },
            ),
            (
                K::Unique,
                "p_uq",
                "a_unique",
                if exchange { "a_key" } else { "a_unique" },
            ),
            (K::Index, "p_v", "a_index", "released_index"),
            (K::Index, "p_kept", "a_kept_index", "a_kept_index"),
        ];
        let names: Vec<_> = bindings
            .iter()
            .map(|&(kind, parent, _, to)| DetachedName {
                kind,
                parent: parent.into(),
                name: Some(to.into()),
            })
            .collect();
        let clones: Vec<_> = bindings
            .iter()
            .map(|&(kind, parent, from, _)| DetachClone {
                kind,
                parent: parent.into(),
                name: TableName::new("app", from),
            })
            .collect();
        let mut facts = detach_clone_slots(&uid, &names, &clones);
        assert_eq!(facts.detach_releases.len(), if exchange { 3 } else { 1 });
        assert_eq!(facts.detach_retained.len(), if exchange { 1 } else { 3 });
        let occupants: Vec<_> = clones
            .iter()
            .map(|clone| NameOccupant {
                name: clone.name.clone(),
                kind: "index",
                owner: Some(table.clone()),
                owner_column: None,
            })
            .collect();
        let changes = ChangeSet {
            changes: vec![PlannedChange::new(Change::DetachPartition {
                uid: uid.clone(),
                table: table.clone(),
                parent: TableName::new("app", "p"),
                names,
                shape: Box::default(),
            })],
        };
        refuse_uninventoried_occupants(&changes, &occupants, &[], "prod", &facts)
            .expect("retaining a_key/a_unique or exchanging them is a valid detach");
        for owner in facts.detach_retained.values_mut() {
            *owner = Uid::derived(UidKind::Table, "other", 0);
        }
        assert!(
            refuse_uninventoried_occupants(&changes, &occupants, &[], "prod", &facts).is_err(),
            "another detach's batch is no exemption"
        );
        facts.detach_retained.clear();
        assert!(
            refuse_uninventoried_occupants(&changes, &occupants, &[], "prod", &facts).is_err(),
            "ownership without an exact retained-clone fact is no exemption"
        );
    }
}

enum CloneRelease {
    Detach,
    Drop,
    Retain,
}

fn check_rename_clone_handover(carried: bool, release: CloneRelease) {
    use pbps_dialect::slots::{CatalogFacts, Namespace, SlotKey};
    use pbps_model::{Change, ChangeSet, DetachedKind, DetachedName, PlannedChange, Uid, UidKind};
    use pbps_pg::catalog::NameOccupant;

    let child = TableName::new("app", "a");
    let parent = TableName::new("app", "p");
    let held = TableName::new("app", "held");
    let from = TableName::new(if carried { "src" } else { "app" }, "source");
    let to = if carried {
        TableName::new("app", "source")
    } else {
        held.clone()
    };
    let child_uid = Uid::derived(UidKind::Table, &child.raw().to_string(), 0);
    let releases = !matches!(release, CloneRelease::Retain);
    let drops = matches!(release, CloneRelease::Drop);
    let rename = PlannedChange::new(Change::RenameTable {
        uid: Uid::derived(UidKind::Table, &from.raw().to_string(), 0),
        from: from.clone(),
        to,
        defaults: Vec::new(),
    });
    let freeing = PlannedChange::new(if drops {
        Change::DropTable {
            uid: child_uid.clone(),
            name: child.clone(),
            detach_from: Some(parent.clone()),
        }
    } else {
        Change::DetachPartition {
            uid: child_uid.clone(),
            table: child.clone(),
            parent: parent.clone(),
            names: vec![DetachedName {
                kind: DetachedKind::Index,
                parent: "p_n".into(),
                name: Some(if releases { "released" } else { "held" }.into()),
            }],
            shape: Box::default(),
        }
    });
    let mut changes = ChangeSet {
        changes: vec![rename, freeing],
    };
    let (before, before_ids, mut after, mut after_ids) =
        carried_sides(&changes.changes, &[parent, child.clone()]);
    if drops {
        after.tables.remove(&child);
        after_ids.tables.remove(&child_uid);
    }
    let mut occupants = vec![NameOccupant {
        name: held.clone(),
        kind: "index",
        owner: Some(child),
        owner_column: None,
    }];
    if carried {
        occupants.push(NameOccupant {
            name: TableName::new("src", "held"),
            kind: "index",
            owner: Some(from),
            owner_column: None,
        });
    }
    let mut facts = CatalogFacts::default();
    if releases && !drops {
        facts.detach_releases.insert(
            SlotKey {
                namespace: Namespace::Relations(held.schema.clone()),
                name: held.name.clone(),
            },
            child_uid,
        );
    }
    let refused = |changes: &ChangeSet| {
        refuse_uninventoried_occupants(changes, &occupants, &[], "prod", &facts).is_err()
    };
    assert!(refused(&changes), "the original order meets the clone");
    let original = changes.clone();
    let order = |changes: &mut pbps_model::ChangeSet| {
        order_carried_moves(
            changes,
            &occupants,
            &[],
            (
                pbps_diff::Side {
                    schema: &before,
                    ids: &before_ids,
                },
                pbps_diff::Side {
                    schema: &after,
                    ids: &after_ids,
                },
            ),
            &pbps_pg::Postgres::new(),
            &facts,
        );
    };
    order(&mut changes);
    if releases {
        assert_eq!(
            changes.changes,
            [original.changes[1].clone(), original.changes[0].clone()],
            "the partition releases the rename's claim first"
        );
        assert!(!refused(&changes));
    } else {
        assert_eq!(changes, original, "a retained clone supplies no release");
        assert!(refused(&changes));
    }
    let settled = changes.clone();
    order(&mut changes);
    assert_eq!(changes, settled, "settling again changes nothing");
}

#[test]
fn a_table_rename_follows_the_partition_releasing_its_target_name() {
    for release in [CloneRelease::Detach, CloneRelease::Drop] {
        check_rename_clone_handover(false, release);
    }
}

#[test]
fn a_schema_move_follows_the_partition_releasing_its_carried_index_name() {
    for release in [CloneRelease::Detach, CloneRelease::Drop] {
        check_rename_clone_handover(true, release);
    }
}

#[test]
fn rename_claims_cannot_take_a_retained_partition_clone_name() {
    for carried in [false, true] {
        check_rename_clone_handover(carried, CloneRelease::Retain);
    }
}

struct IdentityHandover {
    before: Schema,
    before_ids: IdsFile,
    after: Schema,
    after_ids: IdsFile,
    changes: pbps_model::ChangeSet,
    occupants: Vec<pbps_pg::catalog::NameOccupant>,
}

impl IdentityHandover {
    fn new(to: pbps_model::ValueSource) -> Self {
        use pbps_model::{Change, ColumnRef, PlannedChange, Table, Uid, UidKind};
        let owner = TableName::new("app", "t");
        // The catalog name differs from the engine's original spelling.
        let claimed = TableName::new("app", "counter");
        let table_id = Uid::derived(UidKind::Table, "owner", 0);
        let column_id = Uid::derived(UidKind::Column, "owner.id", 0);
        let claimed_id = Uid::derived(UidKind::Table, "claim", 0);
        let mut before = Schema::default();
        let table: Table = serde_json::from_str(
            r#"{"columns":{"id":{"type":"integer","nullable":false,"identity":[1,1]}}}"#,
        )
        .unwrap();
        let from = table.columns["id"].value_source.clone();
        let ty = table.columns["id"].ty.clone();
        before.tables.insert(owner.clone(), table);
        let column = ColumnRef::new(owner.clone(), "id");
        let mut before_ids = IdsFile::default();
        before_ids.tables.insert(table_id, owner.clone());
        before_ids.columns.insert(column_id.clone(), column.clone());
        let mut after = before.clone();
        after
            .tables
            .get_mut(&owner)
            .unwrap()
            .columns
            .get_mut("id")
            .unwrap()
            .value_source = to.clone();
        let created: Table =
            serde_json::from_str(r#"{"columns":{"n":{"type":"integer","nullable":true}}}"#)
                .unwrap();
        after.tables.insert(claimed.clone(), created.clone());
        let mut after_ids = before_ids.clone();
        after_ids.tables.insert(claimed_id.clone(), claimed.clone());
        after_ids.columns.insert(
            Uid::derived(UidKind::Column, "claim.n", 0),
            ColumnRef::new(claimed.clone(), "n"),
        );
        Self {
            before,
            before_ids,
            after,
            after_ids,
            changes: pbps_model::ChangeSet {
                changes: vec![
                    PlannedChange::new(Change::CreateTable {
                        uid: claimed_id,
                        name: claimed.clone(),
                        table: Box::new(created),
                        beside_default: None,
                        moves_at_most: None,
                    }),
                    PlannedChange::new(Change::AlterColumnNumbering {
                        uid: column_id,
                        column,
                        ty,
                        from,
                        to,
                    }),
                ],
            },
            occupants: vec![pbps_pg::catalog::NameOccupant {
                name: claimed,
                kind: "sequence",
                owner: Some(owner),
                owner_column: Some("id".into()),
            }],
        }
    }

    fn order(&mut self) {
        order_carried_moves(
            &mut self.changes,
            &self.occupants,
            &[],
            (
                pbps_diff::Side {
                    schema: &self.before,
                    ids: &self.before_ids,
                },
                pbps_diff::Side {
                    schema: &self.after,
                    ids: &self.after_ids,
                },
            ),
            &pbps_pg::Postgres::new(),
            &Default::default(),
        );
    }

    fn refused(&self) -> bool {
        refuse_uninventoried_occupants(
            &self.changes,
            &self.occupants,
            &[],
            "prod",
            &Default::default(),
        )
        .is_err()
    }
}

#[test]
fn an_identity_release_precedes_the_relation_claiming_its_catalog_name() {
    use pbps_model::{Identity, OwnedSequence, ValueSource};
    for to in [
        ValueSource::None,
        ValueSource::Sequence(OwnedSequence {
            name: Some("new_counter".into()),
            ..OwnedSequence::serial()
        }),
    ] {
        let mut plan = IdentityHandover::new(to);
        assert!(plan.refused());
        let original = plan.changes.changes.clone();
        plan.order();
        assert_eq!(
            plan.changes.changes,
            vec![original[1].clone(), original[0].clone()]
        );
        assert!(!plan.refused());
        let ordered = plan.changes.clone();
        plan.order();
        assert_eq!(plan.changes, ordered, "settling again changes nothing");
    }

    // Keeping or retuning the identity gives up no sequence, even though
    // the generic slot analysis considers a numbering change a release.
    for identity in [Identity::new(1, 1), Identity::new(5, 2)] {
        let mut plan = IdentityHandover::new(ValueSource::Identity(identity));
        let original = plan.changes.clone();
        plan.order();
        assert_eq!(plan.changes, original);
        assert!(plan.refused());
    }
    for wrong_owner in [None, Some(TableName::new("app", "another"))] {
        let mut plan = IdentityHandover::new(ValueSource::None);
        plan.occupants[0].owner = wrong_owner;
        let original = plan.changes.clone();
        plan.order();
        assert_eq!(plan.changes, original);
        assert!(plan.refused());
    }
    let mut plan = IdentityHandover::new(ValueSource::None);
    plan.occupants.clear();
    let original = plan.changes.clone();
    plan.order();
    assert_eq!(
        plan.changes, original,
        "no catalog fact, no guessed handover"
    );
}

#[test]
fn identity_handover_matches_raw_column_owners_without_escaped_aliases() {
    const RAW: &str = "id\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";
    const SHOWN: &str = r"id\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";
    for owner_column in [RAW, SHOWN] {
        let mut plan = IdentityHandover::new(pbps_model::ValueSource::None);
        let owner = TableName::new("app", "t");
        for schema in [&mut plan.before, &mut plan.after] {
            let columns = &mut schema.tables.get_mut(&owner).unwrap().columns;
            let column = columns.shift_remove("id").unwrap();
            columns.insert(RAW.into(), column);
        }
        for ids in [&mut plan.before_ids, &mut plan.after_ids] {
            for column in ids.columns.values_mut().filter(|c| c.table == owner) {
                column.name = RAW.into();
            }
        }
        let pbps_model::Change::AlterColumnNumbering { column, .. } =
            &mut plan.changes.changes[1].change
        else {
            panic!("the fixture gives up an identity");
        };
        column.name = RAW.into();
        plan.occupants[0].owner_column = Some(owner_column.into());
        let original = plan.changes.changes.clone();
        plan.order();
        if owner_column == RAW {
            assert_eq!(
                plan.changes.changes,
                vec![original[1].clone(), original[0].clone()]
            );
            assert!(!plan.refused());
        } else {
            assert_eq!(plan.changes.changes, original);
            assert!(plan.refused(), "an escaped spelling proves no owner match");
        }
    }
}

#[test]
fn a_retyped_identity_releases_its_catalog_name_before_the_new_holder() {
    use pbps_model::{Change, Identity, OwnedSequence, ValueSource};

    for to in [
        ValueSource::None,
        ValueSource::Sequence(OwnedSequence {
            name: Some("new_counter".into()),
            ..OwnedSequence::serial()
        }),
        ValueSource::Identity(Identity::new(1, 1)),
    ] {
        let released = !matches!(to, ValueSource::Identity(_));
        let mut plan = IdentityHandover::new(to);
        plan.after
            .tables
            .get_mut(&TableName::new("app", "t"))
            .unwrap()
            .columns
            .get_mut("id")
            .unwrap()
            .ty = "bigint".parse().unwrap();
        let sides = (
            pbps_diff::Side {
                schema: &plan.before,
                ids: &plan.before_ids,
            },
            pbps_diff::Side {
                schema: &plan.after,
                ids: &plan.after_ids,
            },
        );
        plan.changes = pbps_diff::diff(
            sides.0,
            sides.1,
            &pbps_pg::Postgres::new(),
            &pbps_model::Hints::default(),
        )
        .unwrap()
        .changes()
        .clone();
        assert!(
            plan.changes
                .changes
                .iter()
                .any(|p| matches!(p.change, Change::AlterColumnType { .. }))
        );
        let generic = pbps_diff::order::slots::handovers(
            &plan.changes.changes,
            sides.0,
            sides.1,
            &pbps_pg::Postgres::new(),
            &occupant_slots(&plan.occupants),
        )
        .unwrap();
        if released {
            assert!(
                generic
                    .constraints()
                    .iter()
                    .all(|edge| edge.source != pbps_diff::order::Source::Catalog),
                "the retype makes the generic catalog release ambiguous"
            );
        } else {
            assert!(
                generic
                    .constraints()
                    .iter()
                    .any(|edge| edge.source == pbps_diff::order::Source::Catalog),
                "a retype alone may supply a generic edge without freeing the identity"
            );
        }
        assert!(plan.refused());
        let original = plan.changes.changes.clone();
        plan.order();
        if released {
            let release = plan
                .changes
                .changes
                .iter()
                .position(|p| matches!(p.change, Change::AlterColumnNumbering { .. }))
                .unwrap();
            let claim = plan
                .changes
                .changes
                .iter()
                .position(|p| matches!(p.change, Change::CreateTable { .. }))
                .unwrap();
            assert!(
                release < claim,
                "the exact identity release still orders its claimant"
            );
            assert!(!plan.refused());
            let kept = |steps: &[pbps_model::PlannedChange]| {
                steps
                    .iter()
                    .filter(|p| !matches!(p.change, Change::CreateTable { .. }))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                kept(&plan.changes.changes),
                kept(&original),
                "the retype and numbering keep their order"
            );
            let ordered = plan.changes.clone();
            plan.order();
            assert_eq!(plan.changes, ordered);
        } else {
            assert!(
                original
                    .iter()
                    .all(|p| !matches!(p.change, Change::AlterColumnNumbering { .. })),
                "this negative only retypes the kept identity"
            );
            assert_eq!(plan.changes.changes, original);
            assert!(
                plan.refused(),
                "a retype alone keeps the identity sequence occupied"
            );
        }
    }
}

#[test]
fn settling_an_identity_handover_preserves_the_claims_dependents() {
    use pbps_model::{Change, ColumnRef, PlannedChange, Uid, UidKind, ValueSource};
    let mut plan = IdentityHandover::new(ValueSource::None);
    let claimed = TableName::new("app", "counter");
    let uid = Uid::derived(UidKind::Column, "claim.extra", 0);
    let column: pbps_model::Column =
        serde_json::from_str(r#"{"type":"integer","nullable":true}"#).unwrap();
    plan.after
        .tables
        .get_mut(&claimed)
        .unwrap()
        .columns
        .insert("extra".into(), column.clone());
    plan.after_ids
        .columns
        .insert(uid.clone(), ColumnRef::new(claimed.clone(), "extra"));
    plan.changes.changes.insert(
        1,
        PlannedChange::new(Change::AddColumn {
            uid,
            table: claimed,
            name: "extra".into(),
            column: Box::new(column),
        }),
    );
    let original = plan.changes.changes.clone();
    plan.order();
    assert_eq!(
        plan.changes.changes,
        vec![
            original[2].clone(),
            original[0].clone(),
            original[1].clone()
        ]
    );
    assert!(!plan.refused());

    // A column the operations cannot own is unknown, not independent.
    let mut unowned = IdentityHandover::new(ValueSource::None);
    unowned.after_ids.tables.retain(|_, name| name.name != "t");
    unowned.before_ids.tables.clear();
    let original = unowned.changes.clone();
    unowned.order();
    assert_eq!(unowned.changes, original);
    assert!(unowned.refused());
}

/// #1767, #1957: a move whose carried sequence lands on a name a rename of
/// this plan moves away runs after that rename, by the handover the
/// catalog's occupant gives (DEC-1957.2). A chain of such moves is ordered
/// by the same evidence. Negatives keep the order and the refusal: a plan
/// that clears as it is, two moves that each wait for the other, and a
/// change other than a table rename that the move would pass.
#[test]
fn a_move_runs_after_the_rename_that_moves_its_carried_names_holder_away() {
    use pbps_model::{Change, PlannedChange};
    use pbps_pg::catalog::NameOccupant;
    let t = TableName::new;
    let rename = |from: TableName, to: TableName| {
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, &from.raw().to_string(), 0),
            from,
            to,
            defaults: Vec::new(),
        })
    };
    let sequence = |name: TableName, owner: TableName| NameOccupant {
        name,
        kind: "sequence",
        owner: Some(owner),
        owner_column: Some("id".into()),
    };
    // `s1.a` owns `s1.a_id_seq`; `s2.b` owns `s2.a_id_seq`.
    let occupants = [
        sequence(t("s1", "a_id_seq"), t("s1", "a")),
        sequence(t("s2", "a_id_seq"), t("s2", "b")),
    ];
    let a_in = || rename(t("s1", "a"), t("s2", "a"));
    let b_out = || rename(t("s2", "b"), t("s3", "b"));
    let ordered = |changes: Vec<PlannedChange>, others: &[TableName]| {
        let cs = carried_moves(changes, &occupants, others);
        let refused =
            refuse_uninventoried_occupants(&cs, &occupants, &[], "prod", &Default::default())
                .map_err(|e| e.to_string());
        (cs.changes, refused)
    };

    let (changes, refused) = ordered(vec![a_in(), b_out()], &[]);
    assert_eq!(changes, vec![b_out(), a_in()]);
    refused.expect("after the holder moves away, the move lands on a free name");

    // A chain: `s2.b` moves to `s3.c`, which only the rename after
    // `s1.a`'s move frees. The handovers order all three, which the one
    // hoist of DEC-1767.1 refused.
    let b_waits = || rename(t("s2", "b"), t("s3", "c"));
    let frees = || rename(t("s3", "c"), t("s3", "d"));
    let (changes, refused) = ordered(vec![a_in(), frees(), b_waits()], &[]);
    assert_eq!(changes, vec![frees(), b_waits(), a_in()]);
    refused.expect("each name is free when it is taken");

    // `s5.c -> s3.e` passes through `s3.c`, which `s2.b` takes: run
    // first, as given, it passes through a free name (review of #1785).
    let passes_through = || rename(t("s5", "c"), t("s3", "e"));
    let (changes, refused) = ordered(vec![a_in(), passes_through(), b_waits()], &[]);
    assert_eq!(changes, vec![passes_through(), b_waits(), a_in()]);
    refused.expect("the name it passes through is free when it runs");

    // `s2.b -> s3.z` takes the name of `s3.x`'s index `s3.z`, which only
    // `s3.x`'s move to `s4` carries away (review of #1785): a second
    // occupant's handover, ordered by its own evidence.
    let mut with_index = occupants.to_vec();
    with_index.push(NameOccupant {
        name: t("s3", "z"),
        kind: "index",
        owner: Some(t("s3", "x")),
        owner_column: None,
    });
    let x_out = || rename(t("s3", "x"), t("s4", "x"));
    let b_to_z = || rename(t("s2", "b"), t("s3", "z"));
    let cs = carried_moves(vec![a_in(), b_to_z(), x_out()], &with_index, &[]);
    assert_eq!(cs.changes, vec![x_out(), b_to_z(), a_in()]);
    refuse_uninventoried_occupants(&cs, &with_index, &[], "prod", &Default::default())
        .expect("each occupant has moved away before its name is taken");

    // `s5.a -> s1.q` passes through `s1.a` (`SET SCHEMA` first), which
    // only `s1.a`'s move frees: that move stays ahead of it, after `s2.b`'s.
    let through_a = || rename(t("s5", "a"), t("s1", "q"));
    let at_a = [(t("s1", "a"), "table")];
    let cs = carried_moves_beside(vec![a_in(), through_a(), b_out()], &occupants, &at_a, &[]);
    assert_eq!(cs.changes, vec![b_out(), a_in(), through_a()]);
    refuse_uninventoried_occupants(&cs, &occupants, &at_a, "prod", &Default::default())
        .expect("`s1.a` has moved away before the rename passes through it");

    // Negatives.
    let (changes, refused) = ordered(vec![b_out(), a_in()], &[]);
    assert_eq!(
        changes,
        vec![b_out(), a_in()],
        "already clear: left as it is"
    );
    refused.expect("already clear");

    // An occupant in another schema hands over no name the move takes.
    let elsewhere = [
        sequence(t("s1", "a_id_seq"), t("s1", "a")),
        sequence(t("s9", "a_id_seq"), t("s2", "b")),
    ];
    let cs = carried_moves(vec![a_in(), b_out()], &elsewhere, &[]);
    assert_eq!(cs.changes, vec![a_in(), b_out()], "nothing to order");

    let swap = || rename(t("s2", "b"), t("s1", "b"));
    let cyclic = [
        sequence(t("s1", "q"), t("s1", "a")),
        sequence(t("s2", "q"), t("s2", "b")),
    ];
    let cs = carried_moves(vec![a_in(), swap()], &cyclic, &[]);
    assert_eq!(cs.changes, vec![a_in(), swap()], "a cycle keeps its order");
    refuse_uninventoried_occupants(&cs, &cyclic, &[], "prod", &Default::default())
        .expect_err("each move waits for the other");

    // The move would pass a change other than a table rename, which a
    // release build has no edge to order it against.
    let between = PlannedChange::new(Change::DropIndex {
        table: t("s9", "z"),
        name: "z_ix".into(),
    });
    let (changes, refused) = ordered(vec![a_in(), between.clone(), b_out()], &[t("s9", "z")]);
    assert_eq!(changes, vec![a_in(), between, b_out()]);
    refused.expect_err("only table renames pass one another");

    // The rename's own target is checked as it runs: an index there is
    // refused by name, and dropping it first frees the name.
    let e = refuse_uninventoried_occupants(
        &pbps_model::ChangeSet {
            changes: vec![b_to_z()],
        },
        &with_index,
        &[],
        "prod",
        &Default::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains(
            "`s3.z`: this plan renames `s2.b` to `s3.z`, which takes `s3.z`, where index \
                 `s3.z` already is"
        ),
        "{e}"
    );
    let drop_z = PlannedChange::new(Change::DropIndex {
        table: t("s3", "x"),
        name: "z".into(),
    });
    refuse_uninventoried_occupants(
        &pbps_model::ChangeSet {
            changes: vec![drop_z, b_to_z()],
        },
        &[with_index[2].clone()],
        &[],
        "prod",
        &Default::default(),
    )
    .expect("the index is dropped before the rename takes its name");
}

/// Which steps a settled order lets pass one another: only those marked,
/// on both ends of every inverted pair.
#[test]
fn only_marked_steps_pass_one_another() {
    assert!(only_crossed_by(&[0, 1, 2], &[false, false, false]));
    assert!(only_crossed_by(&[1, 0, 2], &[true, true, false]));
    assert!(!only_crossed_by(&[1, 0, 2], &[true, false, false]));
    // `2` passes `1` and `0`: all three must be marked.
    assert!(!only_crossed_by(&[2, 0, 1], &[true, false, true]));
    assert!(only_crossed_by(&[2, 0, 1], &[true, true, true]));
    // `0` sinks past `1`; `2` stays.
    assert!(only_crossed_by(&[1, 0, 2], &[true, true, false]));
    assert!(!only_crossed_by(&[0, 2, 1], &[false, false, true]));
}

/// #1749: what the cross-schema renames carry, by the name each lands
/// on, for the second catalog read. A same-schema rename carries nothing.
#[test]
fn carried_names_are_where_each_move_lands() {
    use pbps_model::{Change, ChangeSet, PlannedChange};
    use pbps_pg::catalog::NameOccupant;
    let a = TableName::new("s1", "a");
    let rename = |to: TableName| {
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "s1.a", 0),
            from: a.clone(),
            to,
            defaults: Vec::new(),
        })
    };
    let index = NameOccupant {
        name: TableName::new("s1", "ix"),
        kind: "index",
        owner: Some(a.clone()),
        owner_column: None,
    };
    let plan = |changes| ChangeSet { changes };
    assert_eq!(
        carried_names(
            &plan(vec![rename(TableName::new("s2", "a"))]),
            std::slice::from_ref(&index)
        ),
        [TableName::new("s2", "ix")]
    );
    assert!(
        carried_names(
            &plan(vec![rename(TableName::new("s1", "b"))]),
            std::slice::from_ref(&index)
        )
        .is_empty()
    );
}

/// #1084: a cross-schema rename carries its table's indexes and owned
/// sequences into the destination schema under their own names, before
/// any table is created. A created name there that meets one refuses the
/// plan, naming the object and the rename. Negatives: a rename within
/// the schema carries nothing out; a sequence whose column the plan
/// drops, and an index it drops, stay behind; and a name nothing lands
/// on is free.
#[test]
fn a_cross_schema_rename_carries_what_its_table_owns() {
    use pbps_model::{Change, ChangeSet, ColumnRef, PlannedChange};
    use pbps_pg::catalog::NameOccupant;
    let old = TableName::new("app", "old");
    let carried = TableName::new("archive", "old_n_seq");
    let rename = |to: TableName| {
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "app.old", 0),
            from: old.clone(),
            to,
            defaults: Vec::new(),
        })
    };
    let create = |name: &TableName| {
        PlannedChange::new(Change::CreateTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "archive.x", 0),
            name: name.clone(),
            table: Box::default(),
            beside_default: None,
            moves_at_most: None,
        })
    };
    let sequence = NameOccupant {
        name: TableName::new("app", "old_n_seq"),
        kind: "sequence",
        owner: Some(old.clone()),
        owner_column: Some("n".into()),
    };
    let index = NameOccupant {
        name: TableName::new("app", "old_n_seq"),
        kind: "index",
        owner: Some(old.clone()),
        owner_column: None,
    };
    let plan = |changes: Vec<PlannedChange>| ChangeSet { changes };
    let to_archive = || rename(TableName::new("archive", "new"));

    let e = refuse_uninventoried_occupants(
        &plan(vec![to_archive(), create(&carried)]),
        std::slice::from_ref(&sequence),
        &[],
        "prod",
        &Default::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains(
            "`archive.old_n_seq`: this plan moves sequence `app.old_n_seq` on `app.old` \
                 there first, with the rename of `app.old` to `archive.new`"
        ),
        "{e}"
    );
    assert!(
        refuse_uninventoried_occupants(
            &plan(vec![to_archive(), create(&carried)]),
            std::slice::from_ref(&index),
            &[],
            "prod",
            &Default::default()
        )
        .is_err(),
        "an index is carried too"
    );

    // Within the schema: nothing leaves `app`.
    refuse_uninventoried_occupants(
        &plan(vec![rename(TableName::new("app", "new")), create(&carried)]),
        std::slice::from_ref(&sequence),
        &[],
        "prod",
        &Default::default(),
    )
    .expect("a same-schema rename carries nothing into archive");
    // Dropped first: the column takes its sequence, the drop its index.
    refuse_uninventoried_occupants(
        &plan(vec![
            to_archive(),
            PlannedChange::new(Change::DropColumn {
                uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "app.old.n", 0),
                column: ColumnRef {
                    table: TableName::new("archive", "new"),
                    name: "n".into(),
                },
            }),
            create(&carried),
        ]),
        std::slice::from_ref(&sequence),
        &[],
        "prod",
        &Default::default(),
    )
    .expect("the column's drop takes its sequence");
    refuse_uninventoried_occupants(
        &plan(vec![
            to_archive(),
            PlannedChange::new(Change::DropIndex {
                table: TableName::new("archive", "new"),
                name: "old_n_seq".into(),
            }),
            create(&carried),
        ]),
        std::slice::from_ref(&index),
        &[],
        "prod",
        &Default::default(),
    )
    .expect("the index is dropped before the table is created");
    // A view the plan rebuilds in the destination: its drop frees the
    // name before the rename, and the carried sequence takes it.
    let view = TableName::new("archive", "old_n_seq");
    let rebuild = vec![
        PlannedChange::new(Change::DropModule {
            id: ModuleId::Named(view.clone()),
            kind: pbps_model::ModuleKind::View,
        }),
        to_archive(),
        PlannedChange::new(Change::CreateModule {
            id: ModuleId::Named(view.clone()),
            module: Box::new(pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: "SELECT 1".into(),
                options: Default::default(),
            }),
        }),
    ];
    let e = refuse_uninventoried_occupants(
        &plan(rebuild),
        std::slice::from_ref(&sequence),
        &[],
        "prod",
        &Default::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("`archive.old_n_seq`: this plan moves sequence"),
        "{e}"
    );
    refuse_uninventoried_occupants(
        &plan(vec![
            to_archive(),
            create(&TableName::new("archive", "free")),
        ]),
        std::slice::from_ref(&sequence),
        &[],
        "prod",
        &Default::default(),
    )
    .expect("a name nothing lands on is free");
    assert_eq!(
        transferred_tables(&plan(vec![
            to_archive(),
            rename(TableName::new("app", "x"))
        ])),
        std::slice::from_ref(&old)
    );
}

/// #951: a sequence, index or composite type at a name the plan creates
/// refuses it, with the kind and owning table named. One the plan frees
/// first does not: an index it drops, or an index or owned sequence of a
/// table it drops. A view the plan rebuilds already owns its name.
#[test]
fn an_uninventoried_occupant_refuses_unless_the_plan_frees_it() {
    use pbps_model::{Change, ChangeSet, Module, ModuleKind, PlannedChange};
    use pbps_pg::catalog::NameOccupant;
    let x = TableName::new("app", "x");
    let owner = TableName::new("app", "old");
    let create_table = || {
        PlannedChange::new(Change::CreateTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "app.x", 0),
            name: x.clone(),
            table: Box::default(),
            beside_default: None,
            moves_at_most: None,
        })
    };
    let view = || Module {
        kind: ModuleKind::View,
        description: None,
        definition: "SELECT 1".into(),
        options: Default::default(),
    };
    let occupant = |kind, owner: Option<&TableName>| NameOccupant {
        name: x.clone(),
        kind,
        owner: owner.cloned(),
        owner_column: None,
    };
    let plan = |changes: Vec<PlannedChange>| ChangeSet { changes };

    let e = refuse_uninventoried_occupants(
        &plan(vec![create_table()]),
        &[occupant("sequence", None)],
        &[],
        "prod",
        &Default::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("already has sequence `app.x`"), "{e}");
    let e = refuse_uninventoried_occupants(
        &plan(vec![create_table()]),
        &[occupant("index", Some(&owner))],
        &[],
        "prod",
        &Default::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("already has index `app.x` on `app.old`"), "{e}");
    // A new view's name is asked too.
    let new_view = PlannedChange::new(Change::CreateModule {
        id: ModuleId::Named(x.clone()),
        module: Box::new(view()),
    });
    assert!(
        refuse_uninventoried_occupants(
            &plan(vec![new_view.clone()]),
            &[occupant("composite type", None)],
            &[],
            "prod",
            &Default::default()
        )
        .is_err()
    );

    // Freed by the plan itself.
    let drop_index = PlannedChange::new(Change::DropIndex {
        table: owner.clone(),
        name: "x".into(),
    });
    refuse_uninventoried_occupants(
        &plan(vec![drop_index, create_table()]),
        &[occupant("index", Some(&owner))],
        &[],
        "prod",
        &Default::default(),
    )
    .expect("an index the plan drops first");
    let drop_table = PlannedChange::new(Change::DropTable {
        uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "app.old", 0),
        name: owner.clone(),
        detach_from: None,
    });
    for kind in ["index", "sequence"] {
        refuse_uninventoried_occupants(
            &plan(vec![drop_table.clone(), create_table()]),
            &[occupant(kind, Some(&owner))],
            &[],
            "prod",
            &Default::default(),
        )
        .expect("it goes with the table the plan drops");
    }
    // The index behind a unique constraint or a primary key the plan
    // drops, named after the constraint (#1082 review).
    let drop_unique = PlannedChange::new(Change::DropUnique {
        table: owner.clone(),
        name: "x".into(),
    });
    let drop_key = |name: Option<&str>| {
        PlannedChange::new(Change::SetPrimaryKey {
            table: TableName::new("app", if name.is_some() { "old" } else { "x" }),
            from: Some(pbps_model::PrimaryKey {
                name: name.map(Into::into),
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            }),
            to: None,
            nonclustered: false,
        })
    };
    for dropped in [drop_unique, drop_key(Some("x"))] {
        refuse_uninventoried_occupants(
            &plan(vec![dropped, create_table()]),
            &[occupant("index", Some(&owner))],
            &[],
            "prod",
            &Default::default(),
        )
        .expect("a constraint's index the plan drops first");
    }
    // An unnamed key's index is `<table>_pkey`: freeing `app.x_pkey`
    // frees nothing at `app.x`.
    assert!(
        refuse_uninventoried_occupants(
            &plan(vec![drop_key(None), create_table()]),
            &[occupant("index", Some(&owner))],
            &[],
            "prod",
            &Default::default()
        )
        .is_err()
    );
    // An owner moved to another schema takes its index along; one renamed
    // within the schema does not.
    let rename = |to: TableName| {
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "app.old", 0),
            from: owner.clone(),
            to,
            defaults: Vec::new(),
        })
    };
    refuse_uninventoried_occupants(
        &plan(vec![
            rename(TableName::new("archive", "old")),
            create_table(),
        ]),
        &[occupant("index", Some(&owner))],
        &[],
        "prod",
        &Default::default(),
    )
    .expect("the index moves to `archive` with its table");
    assert!(
        refuse_uninventoried_occupants(
            &plan(vec![rename(TableName::new("app", "older")), create_table()]),
            &[occupant("index", Some(&owner))],
            &[],
            "prod",
            &Default::default()
        )
        .is_err()
    );
    // A sequence whose owning column the plan drops goes with it; one
    // owned by another column stays.
    let owned_by = |column: &str| NameOccupant {
        owner_column: Some(column.into()),
        ..occupant("sequence", Some(&owner))
    };
    let drop_column = PlannedChange::new(Change::DropColumn {
        uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "app.old.id", 0),
        column: pbps_model::ColumnRef::new(owner.clone(), "id"),
    });
    refuse_uninventoried_occupants(
        &plan(vec![drop_column.clone(), create_table()]),
        &[owned_by("id")],
        &[],
        "prod",
        &Default::default(),
    )
    .expect("the sequence goes with its column");
    assert!(
        refuse_uninventoried_occupants(
            &plan(vec![drop_column, create_table()]),
            &[owned_by("other")],
            &[],
            "prod",
            &Default::default()
        )
        .is_err()
    );

    // A table renamed within its schema by the same plan: the catalog
    // still names its key's index and its owned sequence after the old
    // table, and the plan's changes carry the new name (#1082 review).
    let renamed = TableName::new("app", "renamed");
    let at = |name: &str, kind, column: Option<&str>| NameOccupant {
        name: TableName::new("app", name),
        kind,
        owner: Some(owner.clone()),
        owner_column: column.map(Into::into),
    };
    let create_at = |name: &str| {
        PlannedChange::new(Change::CreateTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, name, 0),
            name: TableName::new("app", name),
            table: Box::default(),
            beside_default: None,
            moves_at_most: None,
        })
    };
    let unnamed_key_on = |table: &TableName| {
        PlannedChange::new(Change::SetPrimaryKey {
            table: table.clone(),
            from: Some(pbps_model::PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            }),
            to: None,
            nonclustered: false,
        })
    };
    refuse_uninventoried_occupants(
        &plan(vec![
            rename(renamed.clone()),
            unnamed_key_on(&renamed),
            create_at("old_pkey"),
        ]),
        &[at("old_pkey", "index", None)],
        &[],
        "prod",
        &Default::default(),
    )
    .expect("the key's index is still `old_pkey`, and the plan drops it");
    assert!(
        refuse_uninventoried_occupants(
            &plan(vec![unnamed_key_on(&renamed), create_at("old_pkey")]),
            &[at("old_pkey", "index", None)],
            &[],
            "prod",
            &Default::default()
        )
        .is_err(),
        "without the rename, dropping `renamed`'s key frees `renamed_pkey`"
    );
    let drop_renamed_column = PlannedChange::new(Change::DropColumn {
        uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "app.renamed.n", 0),
        column: pbps_model::ColumnRef::new(renamed.clone(), "n"),
    });
    refuse_uninventoried_occupants(
        &plan(vec![
            rename(renamed.clone()),
            drop_renamed_column,
            create_at("old_n_seq"),
        ]),
        &[at("old_n_seq", "sequence", Some("n"))],
        &[],
        "prod",
        &Default::default(),
    )
    .expect("the sequence's owning column is dropped under its new table name");

    // A view dropped and created is a rebuild: its name is its own.
    let drop_view = PlannedChange::new(Change::DropModule {
        id: ModuleId::Named(x.clone()),
        kind: ModuleKind::View,
    });
    assert!(created_relation_names(&plan(vec![drop_view, new_view])).is_empty());
}

#[test]
fn serial_claims_use_the_emitted_name_and_leave_ordering_names_unchanged() {
    use pbps_dialect::PlanDialect as _;
    use pbps_model::{Change, Column, OwnedSequence, PlanMode, PlannedChange, Table, ValueSource};
    let pg = pbps_pg::Postgres::new();
    for (table, column) in [
        ("renamed", "new_id"),
        (
            "a_very_long_table_name_that_must_be_truncated_for_its_sequence",
            "a_long_column_name",
        ),
        (
            "資料表資料表資料表資料表資料表資料表",
            "編號編號編號編號編號",
        ),
    ] {
        for declared in [None, Some("chosen")] {
            let name = TableName::new("moved", table);
            let mut c = Column::new("integer".parse().unwrap()).not_null();
            c.value_source = ValueSource::Sequence(OwnedSequence {
                name: declared.map(Into::into),
                ..OwnedSequence::serial()
            });
            let mut t = Table::default();
            t.columns.insert(column.into(), c.clone());
            let create = Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: name.clone(),
                table: Box::new(t),
                beside_default: None,
                moves_at_most: None,
            };
            let add = Change::AddColumn {
                uid: "c_aaaaaa".parse().unwrap(),
                table: name.clone(),
                name: column.into(),
                column: Box::new(c),
            };
            assert_eq!(relation_names_created(&create), vec![name]);
            assert!(relation_names_created(&add).is_empty());
            for change in [create, add] {
                let claims = sequence_names_claimed(&change);
                assert_eq!(claims.len(), 1);
                let (sequence, owner) = &claims[0];
                assert_eq!(owner.name.raw(), column);
                assert!(sequence.name.len() <= 63);
                let ordered = pbps_diff::order::singleton(
                    PlannedChange::new(change.clone()),
                    PlanMode::Transactional,
                    &pg,
                );
                let sql = pg.emit(&ordered, 0).unwrap();
                assert!(sql.iter().any(|s| {
                    s.sql
                        .contains(&format!("CREATE SEQUENCE \"moved\".\"{}\"", sequence.name))
                }));
                assert!(
                    created_relation_names(&pbps_model::ChangeSet {
                        changes: vec![pbps_model::PlannedChange::new(change)]
                    })
                    .contains(sequence)
                );
            }
        }
    }
    let add_identity = Change::AddColumn {
        uid: "c_aaaaaa".parse().unwrap(),
        table: TableName::new("app", "t"),
        name: "id".into(),
        column: Box::new(
            serde_json::from_str(
                r#"{"type":"integer","nullable":false,"identity":{"seed":1,"increment":1}}"#,
            )
            .unwrap(),
        ),
    };
    assert!(sequence_names_claimed(&add_identity).is_empty());
}

#[test]
fn fixed_sequence_claims_meet_catalog_and_planned_relations_after_releases() {
    use pbps_model::{Change, Column, ColumnRef, OwnedSequence, Table, ValueSource};
    use pbps_pg::catalog::NameOccupant;
    let table = TableName::new("app", "t");
    let target = TableName::new("app", "t_id_seq");
    let serial = |name: Option<&str>| {
        ValueSource::Sequence(OwnedSequence {
            name: name.map(Into::into),
            ..OwnedSequence::serial()
        })
    };
    let mut column = Column::new("integer".parse().unwrap()).not_null();
    column.value_source = serial(None);
    let mut t = Table::default();
    t.columns.insert("id".into(), column.clone());
    let create = Change::CreateTable {
        uid: "t_aaaaaa".parse().unwrap(),
        name: table.clone(),
        table: Box::new(t),
        beside_default: None,
        moves_at_most: None,
    };
    let add = Change::AddColumn {
        uid: "c_aaaaaa".parse().unwrap(),
        table: table.clone(),
        name: "id".into(),
        column: Box::new(column),
    };
    let numbering = |from, to| Change::AlterColumnNumbering {
        uid: "c_aaaaaa".parse().unwrap(),
        column: ColumnRef::new(table.clone(), "id"),
        ty: "integer".parse().unwrap(),
        from,
        to,
    };
    let identity = ValueSource::Identity(pbps_model::Identity {
        seed: 1,
        increment: 1,
        options: Default::default(),
    });
    let claims = [
        create,
        add,
        numbering(serial(Some("old_seq")), serial(Some("t_id_seq"))),
        numbering(ValueSource::None, serial(None)),
        numbering(identity.clone(), serial(None)),
    ];
    let replay = |changes: Vec<Change>,
                  occupants: &[NameOccupant],
                  relations: &[(TableName, &'static str)]| {
        name_clashes(
            &changes
                .into_iter()
                .map(pbps_model::PlannedChange::new)
                .collect::<Vec<_>>(),
            occupants,
            relations,
            &Default::default(),
        )
    };
    let other_table = Change::CreateTable {
        uid: "t_bbbbbb".parse().unwrap(),
        name: target.clone(),
        table: Box::default(),
        beside_default: None,
        moves_at_most: None,
    };
    let index = Change::AddIndex {
        table: TableName::new("app", "other"),
        name: target.name.clone().into(),
        index: Box::new(serde_json::from_str(r#"{"columns":[{"name":"n"}]}"#).unwrap()),
        clustered: false,
    };
    for claim in claims {
        assert!(
            created_relation_names(&pbps_model::ChangeSet {
                changes: vec![pbps_model::PlannedChange::new(claim.clone())]
            })
            .contains(&target)
        );
        for kind in ["sequence", "index", "composite type"] {
            let occupant = NameOccupant {
                name: target.clone(),
                kind,
                owner: None,
                owner_column: None,
            };
            let clashes = replay(vec![claim.clone()], &[occupant], &[]);
            assert!(
                clashes.iter().any(|(_, s)| s.contains("app.t_id_seq")
                    && s.contains("app.t.id")
                    && s.contains(kind)),
                "{clashes:?}"
            );
        }
        for kind in ["table", "view"] {
            assert!(!replay(vec![claim.clone()], &[], &[(target.clone(), kind)]).is_empty());
        }
        for other in [other_table.clone(), index.clone()] {
            for changes in [
                vec![claim.clone(), other.clone()],
                vec![other, claim.clone()],
            ] {
                assert!(!replay(changes, &[], &[]).is_empty());
            }
        }
        let drop = Change::DropTable {
            uid: "t_bbbbbb".parse().unwrap(),
            name: target.clone(),
            detach_from: None,
        };
        assert!(replay(vec![drop, claim.clone()], &[], &[(target.clone(), "table")]).is_empty());
        let owner = TableName::new("app", "old");
        let occupant = NameOccupant {
            name: target.clone(),
            kind: "sequence",
            owner: Some(owner.clone()),
            owner_column: Some("id".into()),
        };
        let drop = Change::DropTable {
            uid: "t_bbbbbb".parse().unwrap(),
            name: owner,
            detach_from: None,
        };
        assert!(replay(vec![drop, claim.clone()], &[occupant], &[]).is_empty());
        let elsewhere = NameOccupant {
            name: TableName::new("other", "t_id_seq"),
            kind: "sequence",
            owner: None,
            owner_column: None,
        };
        assert!(replay(vec![claim], &[elsewhere], &[]).is_empty());
    }
    // Unknown or unchanged names keep the existing sequence even after
    // its table/column is renamed; removing/converting it claims none.
    for (from, to) in [
        (serial(None), serial(Some("chosen"))),
        (serial(Some("old_seq")), serial(None)),
        (serial(Some("old_seq")), serial(Some("old_seq"))),
        (serial(Some("old_seq")), identity),
        (serial(None), ValueSource::None),
    ] {
        assert!(sequence_names_claimed(&numbering(from, to)).is_empty());
    }
    let other = numbering(ValueSource::None, serial(Some("other")));
    assert!(replay(vec![other], &[], &[(target, "table")]).is_empty());
}

/// The name checks behind DEC-316.1, one occupant at a time. A relation
/// occupies a new table's or view's name; an unreadable routine of the
/// same bare name does not, because routines are not in the relation
/// namespace — refusing it would refuse a plan the engine takes.
/// An index the plan adds is refused at the name of a table, view or
/// unreadable relation outside the recorded scope, like a table there
/// (#1355); a free name is not.
#[test]
fn an_added_index_meets_unrecorded_relations() {
    use pbps_db::catalog::LimitationTarget;
    use pbps_model::{Change, ChangeSet, PlannedChange};
    let x = TableName::new("app", "x");
    let add = |name: &str| ChangeSet {
        changes: vec![PlannedChange::new(Change::AddIndex {
            table: TableName::new("app", "t"),
            name: name.into(),
            index: Box::new(serde_json::from_str(r#"{"columns": [{"name": "n"}]}"#).unwrap()),
            clustered: false,
        })],
    };
    let empty = pbps_diff::Scoped {
        baseline_evidence: None,
        schema: Schema::default(),
        unmanaged: Vec::new(),
        external: Default::default(),
        missing: Vec::new(),
        unmanaged_modules: Vec::new(),
        unmanaged_roles: Vec::new(),
        unmanaged_types: Vec::new(),
        unexpressible: Vec::new(),
        missing_roles: Vec::new(),
        missing_types: Vec::new(),
        public_execute: Default::default(),
        owners: Default::default(),
        session_role: String::new().into(),
    };
    let mut table = empty.clone();
    table.unmanaged.push(x.clone());
    let mut view = empty.clone();
    view.unmanaged_modules.push(ModuleId::Named(x.clone()));
    let unreadable = [(
        LimitationTarget::Relation(x.clone()),
        "partitioned".to_owned(),
    )];
    for (scoped, unreadable, expected) in [
        (
            &table,
            &[][..],
            "index `app.x`: the database already has table `app.x`",
        ),
        (
            &view,
            &[][..],
            "index `app.x`: the database already has view `app.x`",
        ),
        (&empty, &unreadable[..], "`app.x`, which pbps cannot read"),
    ] {
        let e = refuse_occupied_names(&add("x"), scoped, unreadable, &[], "prod")
            .unwrap_err()
            .to_string();
        assert!(e.contains(expected), "{e}");
    }
    refuse_occupied_names(&add("free"), &table, &[], &[], "prod").expect("a free name");
}

#[test]
fn a_new_name_is_occupied_only_by_what_shares_its_namespace() {
    use pbps_db::catalog::LimitationTarget;
    use pbps_model::{Change, ChangeSet, PlannedChange};
    let x = TableName::new("app", "x");
    let create = ChangeSet {
        changes: vec![PlannedChange::new(Change::CreateTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "app.x", 0),
            name: x.clone(),
            table: Box::default(),
            beside_default: None,
            moves_at_most: None,
        })],
    };
    let empty = pbps_diff::Scoped {
        baseline_evidence: None,
        schema: Schema::default(),
        unmanaged: Vec::new(),
        external: Default::default(),
        missing: Vec::new(),
        unmanaged_modules: Vec::new(),
        unmanaged_roles: Vec::new(),
        unmanaged_types: Vec::new(),
        unexpressible: Vec::new(),
        missing_roles: Vec::new(),
        missing_types: Vec::new(),
        public_execute: Default::default(),
        owners: Default::default(),
        session_role: String::new().into(),
    };
    let check = |scoped: &pbps_diff::Scoped,
                 unreadable: &[(LimitationTarget, String)],
                 relations: &[TableName]| {
        refuse_occupied_names(&create, scoped, unreadable, relations, "prod")
    };

    // Nothing there: the name is free.
    check(&empty, &[], &[]).expect("a free name");

    // A readable table, a view, and a relation pbps cannot read.
    let mut table = empty.clone();
    table.unmanaged.push(x.clone());
    let e = check(&table, &[], &[]).unwrap_err().to_string();
    assert!(e.contains("already has table `app.x`"), "{e}");
    let mut view = empty.clone();
    view.unmanaged_modules.push(ModuleId::Named(x.clone()));
    let e = check(&view, &[], &[]).unwrap_err().to_string();
    assert!(e.contains("already has view `app.x`"), "{e}");
    let e = check(&empty, &[], std::slice::from_ref(&x))
        .unwrap_err()
        .to_string();
    assert!(e.contains("a relation pbps cannot read"), "{e}");
    let matview = (
        LimitationTarget::Relation(x.clone()),
        "a materialized view".to_owned(),
    );
    let e = check(&empty, &[matview], &[]).unwrap_err().to_string();
    assert!(e.contains("which pbps cannot read"), "{e}");

    // An unreadable aggregate of the same bare name is not a relation.
    let aggregate = (
        LimitationTarget::Module("app.x(integer)".parse().unwrap()),
        "an aggregate".to_owned(),
    );
    check(&empty, &[aggregate], &[]).expect("a routine does not occupy a table's name");

    let ty = LimitationTarget::Type(x);
    assert!(!ty.matches_module(&"app.x".parse().unwrap()));
    check(&empty, &[(ty, "an omitted type".into())], &[])
        .expect("a type limitation is not an unreadable relation");
}

/// SPEC §7.6 promises the closing read holds the field the plan is itself
/// changing to the value the plan promised. What `PUBLIC` holds is in no
/// `Schema` (DECISIONS 371), so the movement check cannot be the thing
/// that keeps that promise here, and this is.
///
/// Both directions fail, and both hold: the routine the plan closed
/// found open, and the one it declared open found closed.
#[test]
fn the_closing_read_holds_a_routine_to_what_the_plan_settled_about_public() {
    use pbps_model::{Change, ChangeSet, PlannedChange, PublicAccess, RoutineOrigin};
    let id: pbps_model::ModuleId = "app.f(integer)".parse().unwrap();
    let decided = |access| {
        let mut cs = ChangeSet::default();
        cs.changes.push(PlannedChange::new(Change::PublicExecution {
            routine: "app.f(integer)".parse().unwrap(),
            access,
            origin: RoutineOrigin::Created,
        }));
        cs
    };
    let read = |open: bool| {
        let mut scoped = pbps_diff::scope(
            &pbps_model::Schema::default(),
            &pbps_model::IdsFile::default(),
            &Default::default(),
            &Default::default(),
        );
        if open {
            scoped.public_execute.insert(id.clone());
        }
        scoped
    };

    // What each decision promised, found.
    assert!(
        refuse_unmet_public_execution(&decided(PublicAccess::Revoked), &read(false), "target")
            .is_ok()
    );
    assert!(
        refuse_unmet_public_execution(&decided(PublicAccess::Kept), &read(true), "target").is_ok()
    );

    // And each reversed by somebody else between the statement and this
    // read, which is the case SPEC §7.6 says lands here.
    let e = refuse_unmet_public_execution(&decided(PublicAccess::Kept), &read(false), "target")
        .expect_err("an opened routine found closed")
        .to_string();
    assert!(e.contains("app.f(integer)"), "{e}");
    assert!(e.contains("declared it would stay"), "{e}");
    let e = refuse_unmet_public_execution(&decided(PublicAccess::Revoked), &read(true), "target")
        .expect_err("a closed routine found open")
        .to_string();
    assert!(e.contains("this plan closed"), "{e}");

    // A plan that settles nothing about PUBLIC has nothing to fail, on
    // either reading of the database.
    for open in [true, false] {
        assert!(
            refuse_unmet_public_execution(&ChangeSet::default(), &read(open), "target").is_ok()
        );
    }
}

/// `--staged` applies one logical change (ADR-0003), and the differ's own
/// companion to a routine is not a second one. A routine that declares
/// `public_execute: true` arrives with a `PublicExecution` beside its
/// `CreateModule`, and counting entries would refuse a staged creation
/// that was legal before that decision existed.
///
/// The negative cases are the point: two routines are two changes, and a
/// decision whose routine this plan does not build is counted on its own
/// because it has nothing to be a companion of.
#[test]
fn a_routines_public_decision_is_counted_with_the_create_it_belongs_to() {
    use pbps_model::{
        Change, ChangeSet, Module, ModuleKind, PlannedChange, PublicAccess, RoutineOrigin,
    };
    let create = |name: &str| Change::CreateModule {
        id: name.parse().unwrap(),
        module: Box::new(Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "() RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$".into(),
            options: Default::default(),
        }),
    };
    let decided = |name: &str, access| Change::PublicExecution {
        routine: name.parse().unwrap(),
        access,
        origin: RoutineOrigin::Created,
    };
    let count = |changes: Vec<Change>| {
        let mut cs = ChangeSet::default();
        cs.changes
            .extend(changes.into_iter().map(PlannedChange::new));
        crate::logical_changes(&cs)
    };

    for access in [PublicAccess::Kept, PublicAccess::Revoked] {
        assert_eq!(
            count(vec![
                create("app.f(integer)"),
                decided("app.f(integer)", access)
            ]),
            1,
            "{access:?}"
        );
    }
    assert_eq!(
        count(vec![
            create("app.f(integer)"),
            decided("app.f(integer)", PublicAccess::Kept),
            create("app.g(integer)"),
            decided("app.g(integer)", PublicAccess::Kept),
        ]),
        2
    );
    // Nothing in this plan builds `app.h`, so its decision stands alone.
    assert_eq!(
        count(vec![
            create("app.f(integer)"),
            decided("app.h(integer)", PublicAccess::Revoked),
        ]),
        2
    );
}

/// The steps of one enum's rebuild are one logical change to `--staged`,
/// which runs and resumes them statement by statement; the negatives are
/// two enums, and anything the rebuild drops and restores, each a change
/// of its own (DEC-1635.7).
#[test]
fn a_rebuilds_steps_are_one_logical_change_and_its_companions_are_not() {
    use pbps_model::{Change, ChangeSet, EnumRebuildStep, ManagedType, PlannedChange};
    let step = |uid: &str, step| Change::EnumRebuild {
        uid: uid.parse().unwrap(),
        name: "app.e".parse().unwrap(),
        tmp: "app.tmp".parse().unwrap(),
        step,
    };
    let definition = Box::new(ManagedType::Enum {
        values: vec!["ok".into()],
    });
    let rebuild = |uid: &str| {
        vec![
            step(
                uid,
                EnumRebuildStep::Create {
                    definition: definition.clone(),
                    removed: vec![],
                    ordinal: 0,
                    over: None,
                    why: None,
                },
            ),
            step(
                uid,
                EnumRebuildStep::Retype {
                    column_uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, uid, 0),
                    column: "app.t"
                        .parse::<pbps_model::TableName>()
                        .unwrap()
                        .column("m"),
                    to: pbps_model::ColumnType::reference("app.e".parse().unwrap()),
                    array: false,
                    values: Box::new(pbps_model::ValueChange::Relabel { removed: vec![] }),
                },
            ),
            step(
                uid,
                EnumRebuildStep::Swap {
                    definition: definition.clone(),
                    comment: None,
                    depth: 0,
                    over: None,
                },
            ),
        ]
    };
    let count = |changes: Vec<Change>| {
        crate::logical_changes(&ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        })
    };
    assert_eq!(count(rebuild("y_000001")), 1);
    let mut two = rebuild("y_000001");
    two.extend(rebuild("y_000002"));
    assert_eq!(count(two), 2);
    let mut with_default = rebuild("y_000001");
    with_default.push(Change::DropCheck {
        table: "app.t".parse().unwrap(),
        name: "c".into(),
    });
    assert_eq!(count(with_default), 2);
}

/// DECISIONS 415: a probe is answered under the settings the statement it
/// clears will run under, because [`run_probes`] pins them itself.
///
/// The operator's own session is the hostile one here, and it is hostile in
/// the way a real one is: `DateStyle` set on the connection, as a
/// `ALTER ROLE … SET` or a `PGOPTIONS` would. One stored row `2026-01-15`
/// and the declared `CHECK (d < '02/01/2026')` — 1 February under `DMY`,
/// 2 January under the pinned `MDY`. The engine takes the constraint; a
/// probe parsed under `DMY` counts the row and refuses it.
///
/// Asserted against the engine's own verdict on the very statement, not
/// against a remembered count: the two have to agree, and either one alone
/// can be wrong for its own reasons.
/// Issue #274: a probe evaluates the declared expression, and PostgreSQL
/// accepts a volatile one in a `CHECK`. Before the probe transaction,
/// `nextval` advanced the sequence once per row between approval and the
/// plan's first statement, and a rollback did not take it back. Under
/// `READ ONLY` the engine refuses it: the sequence stays where it was, the
/// probe is reported unchecked rather than refusing the plan, and the
/// connection is left outside any transaction for the apply that follows.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn a_probe_cannot_advance_a_sequence_named_in_a_declared_check() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    rt.block_on(async {
        let database = crate::test_pg::TestDb::create("probe_ro").await;
        let mut conn = database.connect().await;
        let schema = format!("pbps_probe_ro_{}", std::process::id());
        conn.execute(&format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE;
                 CREATE SCHEMA {schema};
                 CREATE SEQUENCE {schema}.s;
                 CREATE TABLE {schema}.t (v integer);
                 INSERT INTO {schema}.t VALUES (1), (2), (3);"
        ))
        .await
        .expect("the fixture");
        let sequence = format!(
            "SELECT last_value::int, CASE WHEN is_called THEN 1 ELSE 0 END FROM {schema}.s"
        );
        let read = |rows: Vec<pbps_db::Row>| -> (i32, i32) {
            let row = rows.first().expect("one row");
            (
                row.try_get_at::<i32>(0).unwrap().unwrap(),
                row.try_get_at::<i32>(1).unwrap().unwrap(),
            )
        };
        let before = read(conn.query(&sequence).await.expect("the sequence"));

        let changes = check_on(&schema, &format!("nextval('{schema}.s') > 0"));
        let probed = run_probes(
            &mut conn,
            &pbps_pg::Postgres::new(),
            &pbps_model::plan_names::PlanNames::of(&changes.changes, &IdsFile::default()).unwrap(),
            &pbps_model::Partitions::default(),
            None,
            None,
        )
        .await;
        let after = read(conn.query(&sequence).await.expect("the sequence"));
        // Outside any transaction, and not a read-only one: the apply's
        // own `BEGIN` must open a real transaction after this.
        let write = conn
            .query(&format!("SELECT nextval('{schema}.s')::int"))
            .await
            .map(|_| ());

        let cleanup = conn
            .execute(&format!("DROP SCHEMA {schema} CASCADE;"))
            .await;
        assert_eq!(before, (1, 0), "the fixture's untouched sequence");
        assert_eq!(after, before, "the probe advanced the sequence");
        assert!(
            probed.is_ok(),
            "a probe the engine refuses is unchecked, not a refusal: {probed:?}"
        );
        assert!(
            write.is_ok(),
            "the probe left its transaction open: {write:?}"
        );
        cleanup.expect("drop");
        drop(conn);
        database.drop().await;
    });
}

/// The negative half of #274: the read-only transaction changes what a
/// probe may do, not what it finds. A row that violates an ordinary
/// declared `CHECK` still refuses the plan.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn a_read_only_probe_still_refuses_a_violating_row() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    rt.block_on(async {
        let database = crate::test_pg::TestDb::create("probe_ro_neg").await;
        let mut conn = database.connect().await;
        let schema = format!("pbps_probe_ro_neg_{}", std::process::id());
        conn.execute(&format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE;
                 CREATE SCHEMA {schema};
                 CREATE TABLE {schema}.t (v integer);
                 INSERT INTO {schema}.t VALUES (1), (2), (3);"
        ))
        .await
        .expect("the fixture");

        let changes = check_on(&schema, "v > 1");
        let probed = run_probes(
            &mut conn,
            &pbps_pg::Postgres::new(),
            &pbps_model::plan_names::PlanNames::of(&changes.changes, &IdsFile::default()).unwrap(),
            &pbps_model::Partitions::default(),
            None,
            None,
        )
        .await;

        let cleanup = conn
            .execute(&format!("DROP SCHEMA {schema} CASCADE;"))
            .await;
        let err = probed.expect_err("one row violates `v > 1`").to_string();
        assert!(
            err.contains("the data will not accept this plan") && err.contains("1 "),
            "{err}"
        );
        cleanup.expect("drop");
        drop(conn);
        database.drop().await;
    });
}

fn check_on(schema: &str, expression: &str) -> pbps_model::ChangeSet {
    use pbps_model::{Change, ChangeSet, CheckConstraint, PlannedChange};
    ChangeSet {
        changes: vec![PlannedChange::new(Change::AddCheck {
            table: format!("{schema}.t").parse().expect("a table name"),
            name: "t_ck".into(),
            constraint: CheckConstraint {
                expression: expression.to_owned(),
            },
        })],
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn a_probe_is_answered_under_the_settings_the_statement_will_run_under() {
    // A runtime built by hand, as the flow suite does: this workspace's
    // `tokio` carries `net`, `rt` and `time` and not `macros`.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    rt.block_on(a_probe_is_answered_under_the_pins());
}

async fn a_probe_is_answered_under_the_pins() {
    use pbps_model::{Change, ChangeSet, CheckConstraint, PlannedChange};

    let database = crate::test_pg::TestDb::create("pins").await;
    let mut conn = database.connect().await;

    let schema = format!("pbps_pins_{}", std::process::id());
    conn.execute(&format!(
        "DROP SCHEMA IF EXISTS {schema} CASCADE;
             CREATE SCHEMA {schema};
             CREATE TABLE {schema}.t (d date);
             INSERT INTO {schema}.t VALUES ('2026-01-15');"
    ))
    .await
    .expect("the fixture");

    // The operator's session, before pbps says anything.
    conn.execute("SET DateStyle = 'ISO, DMY';")
        .await
        .expect("the operator's own setting");

    let table: pbps_model::TableName = format!("{schema}.t").parse().expect("a table name");
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(Change::AddCheck {
            table: table.clone(),
            name: "t_ck".into(),
            constraint: CheckConstraint {
                expression: "d < '02/01/2026'".to_owned(),
            },
        })],
    };

    let dialect = pbps_pg::Postgres::new();
    let probed = run_probes(
        &mut conn,
        &dialect,
        &pbps_model::plan_names::PlanNames::of(&changes.changes, &IdsFile::default()).unwrap(),
        &pbps_model::Partitions::default(),
        None,
        None,
    )
    .await;

    // And what the engine does with the statement the probe just judged,
    // under the pins, which is where it will run. Established here rather
    // than relied on from `run_probes`: the engine's verdict is this
    // test's ground truth and must not move with the thing under test, or
    // a regression fails on the wrong assertion.
    conn.execute(
        &pbps_dialect::Dialect::session_pins(&dialect, &pin_zone(&conn))
            .expect("this dialect pins its session"),
    )
    .await
    .expect("the pins the framing would establish");
    let applied = conn
        .execute(&format!(
            "ALTER TABLE {schema}.t ADD CONSTRAINT t_ck CHECK (d < '02/01/2026');"
        ))
        .await;

    let cleanup = conn
        .execute(&format!("DROP SCHEMA {schema} CASCADE;"))
        .await;
    assert!(
        applied.is_ok(),
        "the engine takes this constraint under the pins: {applied:?}"
    );
    assert!(
        probed.is_ok(),
        "the probe refused a plan this engine takes: {probed:?}"
    );
    cleanup.expect("drop");
    drop(conn);
    database.drop().await;
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn transactional_read_back_refuses_concurrent_managed_ddl_but_ignores_unmanaged_ddl() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        read_back_under_concurrent_ddl(false).await;
        read_back_under_concurrent_ddl(true).await;
    });
}

async fn read_back_under_concurrent_ddl(interrupted: bool) {
    let database = crate::test_pg::TestDb::create("capture").await;
    let mut reader = database.connect().await;
    let mut writer = database.connect().await;
    let mut sibling = crate::test_pg::shared().await;
    let own = reader
        .query("SELECT current_database() AS name")
        .await
        .unwrap();
    let shared = sibling
        .query("SELECT current_database() AS name")
        .await
        .unwrap();
    // Timing alone cannot prove isolation: without this assertion a quiet
    // scheduler can make the old shared fixture pass the churn test.
    assert_ne!(
        own[0].try_get::<&str>("name").unwrap(),
        shared[0].try_get::<&str>("name").unwrap()
    );
    let churn_schema = format!("pbps_sibling_{}", std::process::id());
    if interrupted {
        sibling
            .execute(&format!(
                "CREATE SCHEMA {churn_schema}; CREATE TABLE {churn_schema}.t (id integer)"
            ))
            .await
            .unwrap();
    }
    // The shared server survives interrupted runs, including their last
    // churn table; PID reuse must not prevent the read-back assertions.
    sibling
        .execute(&format!("DROP SCHEMA IF EXISTS {churn_schema} CASCADE"))
        .await
        .unwrap();
    sibling
        .execute(&format!("CREATE SCHEMA {churn_schema}"))
        .await
        .unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = stop.clone();
    let churn = tokio::spawn(async move {
        let mut rounds = 0;
        loop {
            sibling
                .execute(&format!(
                    "CREATE TABLE {churn_schema}.t (id integer); DROP TABLE {churn_schema}.t"
                ))
                .await
                .unwrap();
            rounds += 1;
            if stopped.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
        }
        sibling
            .execute(&format!("DROP SCHEMA {churn_schema}"))
            .await
            .unwrap();
        rounds
    });
    let schema = format!("pbps_capture_{}", std::process::id());
    writer.execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; CREATE TABLE {schema}.untouched (id integer)")).await.unwrap();
    reader.execute("BEGIN").await.unwrap();
    reader
        .execute(&format!("CREATE TABLE {schema}.own_write (id integer)"))
        .await
        .unwrap();
    let read = crate::engine::Read::InsideOwnTransaction;
    let mut watched = crate::engine::introspect(&mut reader, read)
        .await
        .unwrap()
        .schema;
    watched.tables.retain(|name, _| name.schema == schema);
    watched.modules.clear();
    watched.roles.clear();
    let ids = pbps_diff::observed_ids(&watched, &IdsFile::default());
    let modules = BTreeSet::new();
    let scopes = DataScopes::default();
    let reference = Schema::default();
    let request = ManagedRead {
        ids: &ids,
        modules: &modules,
        external: &BTreeSet::new(),
        unmanaged: pbps_config::Unmanaged::Ignore.into(),
        scopes: &scopes,
        reference: &reference,
        read,
        warnings: CatalogWarnings::Human,
    };
    let result = managed_state_then(&mut reader, &request, || async {
        writer
            .execute(&format!(
                "ALTER TABLE {schema}.untouched ADD CONSTRAINT concurrent_check CHECK (id > 0)"
            ))
            .await?;
        Ok(())
    })
    .await;
    // The second capture now includes the independently committed check;
    // unrelated objects remain outside the comparison's managed scope.
    let stable = managed_state_then(&mut reader, &request, || async {
        writer
            .execute(&format!("CREATE TABLE {schema}.unmanaged (id integer)"))
            .await?;
        Ok(())
    })
    .await;
    let still_open = pbps_pg::catalog::in_transaction(&mut reader).await.unwrap();
    reader.execute("ROLLBACK").await.unwrap();
    let after = pbps_pg::catalog::introspect(&mut writer).await.unwrap();
    writer
        .execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        churn.await.unwrap() > 0,
        "the sibling must actually issue DDL"
    );
    assert!(
        result
            .as_ref()
            .is_err_and(|e| e.to_string().contains("managed state changed during")),
        "{result:?}"
    );
    let stable = stable.expect("unmanaged DDL does not invalidate this recording");
    assert!(
        stable
            .schema
            .tables
            .contains_key(&TableName::new(&schema, "own_write"))
    );
    assert!(
        stable.schema.tables[&TableName::new(&schema, "untouched")]
            .checks
            .contains_key("concurrent_check")
    );
    assert!(still_open, "read-back must preserve the caller transaction");
    assert!(
        after.schema.tables[&TableName::new(&schema, "untouched")]
            .checks
            .contains_key("concurrent_check")
    );
    assert!(
        !after
            .schema
            .tables
            .contains_key(&TableName::new(&schema, "own_write"))
    );
    drop(reader);
    drop(writer);
    database.drop().await;
}

/// `scope` drops a managed role's *plain* grant on an object nobody
/// manages, with its reason recorded: that is the object's business. The
/// unsupported permission beside it — a DENY, a column-level grant, a
/// WITH GRANT OPTION — was filtered by role alone, so it stopped every
/// command over a securable outside the managed set (DECISIONS 176).
#[test]
fn an_unsupported_permission_on_somebody_elses_object_is_not_this_projects_drift() {
    use pbps_db::catalog::{Pulled, Unexpressible};

    let mine: TableName = "dbo.mine".parse().unwrap();
    let theirs: TableName = "dbo.theirs".parse().unwrap();
    let mut ids = IdsFile::default();
    ids.tables.insert("t_aaaaaa".parse().unwrap(), mine.clone());
    ids.roles
        .insert("r_aaaaaa".parse().unwrap(), "app".to_owned().into());
    let module: ModuleId = "dbo.v".parse().unwrap();
    let modules: BTreeSet<ModuleId> = [module.clone()].into_iter().collect();

    let entry = |role: &pbps_model::RoleName,
                 target: Option<pbps_model::GrantTarget>,
                 what: &str| Unexpressible {
        role: role.to_owned(),
        target,
        what: what.to_owned(),
    };
    let object = |t: &TableName| Some(pbps_model::GrantTarget::Object(t.clone()));
    let pulled = Pulled {
        baseline_evidence: None,
        schema: Schema::default(),
        onboarding_notices: Vec::new(),
        catalog_notices: Vec::new(),
        extension_notices: BTreeMap::new(),
        provided_types: BTreeMap::new(),
        provided_classes: BTreeMap::new(),
        warnings: Vec::new(),
        limitations: Vec::new(),
        unmanaged_modules: Vec::new(),
        public_execute: Default::default(),
        owners: Default::default(),
        session_role: String::new().into(),
        unrevocable: Vec::new(),
        unexpressible: vec![
            entry(
                &pbps_model::RoleName::from("app"),
                object(&mine),
                "on a table this project manages",
            ),
            entry(
                &pbps_model::RoleName::from("app"),
                object(&module.object_name()),
                "on a module this project manages",
            ),
            entry(
                &pbps_model::RoleName::from("app"),
                object(&theirs),
                "on somebody else's table",
            ),
            entry(
                &pbps_model::RoleName::from("other"),
                object(&mine),
                "an unmanaged role's business",
            ),
            // No object to belong to: a permission on the database itself,
            // and a class the model cannot even name (105).
            entry(&pbps_model::RoleName::from("app"), None, "on the database"),
            // Declarable, so a DENY on one is a difference we must hold.
            entry(
                &pbps_model::RoleName::from("app"),
                Some(pbps_model::GrantTarget::Schema("dbo".to_owned())),
                "on a schema",
            ),
        ],
        database_collation: None,
    };

    let kept = unexpressible_permissions(&pulled, &ids, &modules);
    assert_eq!(
        kept,
        [
            "on a table this project manages",
            "on a module this project manages",
            "on the database",
            "on a schema",
        ],
        "{kept:?}"
    );
}

#[test]
fn permission_advisories_share_the_fatal_target_cut_but_require_an_unmanaged_grantee() {
    use pbps_db::catalog::Unexpressible;
    use pbps_model::{ColumnRef, GrantTarget};

    let table: TableName = "app.t".parse().unwrap();
    let mut ids = IdsFile::default();
    ids.tables
        .insert("t_aaaaaa".parse().unwrap(), table.clone());
    ids.roles
        .insert("r_aaaaaa".parse().unwrap(), "managed".into());
    let modules = ["app.v", "app.f(integer)", "app.only_routine(integer)"]
        .map(|name| name.parse().unwrap())
        .into_iter()
        .collect();
    let object = |name: &str| Some(GrantTarget::Object(name.parse().unwrap()));
    let cases = [
        (object("app.t"), true),
        (object("app.v"), true),
        (object("other.t"), false),
        (object("app.only_routine"), false),
        (Some("app.f(integer)".parse().unwrap()), true),
        (Some("app.f(text)".parse().unwrap()), false),
        (Some("app.t(integer)".parse().unwrap()), false),
        (
            Some(GrantTarget::Sequence(ColumnRef::new(table, "id"))),
            true,
        ),
        (
            Some(GrantTarget::Sequence(ColumnRef::new(
                "other.t".parse().unwrap(),
                "id",
            ))),
            false,
        ),
        (Some(GrantTarget::Schema("outside".into())), true),
        (None, false),
    ];
    for (target, advisory_target) in cases {
        for role in ["managed", "unmanaged"] {
            let found = [Unexpressible {
                role: role.into(),
                target: target.clone(),
                what: "named catalog finding".into(),
            }];
            let fatal = permissions_the_managed_set_keeps(&found, &ids, &modules);
            let advisory = permission_advisories(&found, &ids, &modules);
            assert_eq!(
                !fatal.is_empty(),
                role == "managed" && (advisory_target || target.is_none()),
                "fatal: {role} {target:?}"
            );
            assert_eq!(
                !advisory.is_empty(),
                role == "unmanaged" && advisory_target,
                "advisory: {role} {target:?}"
            );
            assert!(fatal.is_empty() || advisory.is_empty());
        }
    }
    assert!(permission_advisories(&[], &ids, &modules).is_empty());
}

use super::*;

fn referrer(kind: &str, name: &str, detail: Option<&str>) -> pbps_db::impact::Referrer {
    pbps_db::impact::Referrer {
        kind: kind.to_owned(),
        name: name.to_owned(),
        detail: detail.map(ToOwned::to_owned),
        removed_with: None,
    }
}

fn carried_view(described: &str, schema: &str, name: &str) -> pbps_db::impact::Referrer {
    pbps_db::impact::Referrer {
        removed_with: Some((
            pbps_model::ModuleKind::View,
            pbps_model::ModuleId::Named(pbps_model::ObjectName::new(schema, name)),
        )),
        ..referrer("view", described, None)
    }
}

fn dropping(kind: pbps_model::ModuleKind, id: &str) -> pbps_model::PlannedChange {
    pbps_model::PlannedChange::new(pbps_model::Change::DropModule {
        id: id.parse().unwrap(),
        kind,
    })
}

/// #823: a view this plan drops is not reported as carried by the rename
/// that follows it. Excused before anything is counted, so a report left
/// with nothing prints no heading and no carried note, and one that still
/// holds a surviving view keeps both.
#[test]
fn a_dropped_carried_view_is_excused_before_impact_rendering() {
    let changes = pbps_model::ChangeSet {
        changes: vec![dropping(pbps_model::ModuleKind::View, "app.v")],
    };
    let names = dropped_referrer_names(&changes);
    let modules = dropped_modules(&changes);

    let mut only_dropped = pbps_db::impact::ImpactReport {
        target: "column app.t.label".to_owned(),
        carried: vec![carried_view("view app.v", "app", "v")],
        ..Default::default()
    };
    excuse_dropped_referrers(&mut only_dropped, &names, &modules);
    assert!(only_dropped.is_empty(), "{only_dropped:#?}");

    let mut with_survivor = pbps_db::impact::ImpactReport {
        target: "column app.t.label".to_owned(),
        carried: vec![
            carried_view("view app.v", "app", "v"),
            carried_view("view outside.keep", "outside", "keep"),
        ],
        ..Default::default()
    };
    excuse_dropped_referrers(&mut with_survivor, &names, &modules);
    let lines = impact_lines(
        &with_survivor,
        &crate::engine::impact_notes(pbps_db::Driver::Postgres, &with_survivor),
    );
    assert!(!lines.iter().any(|l| l.contains("app.v")), "{lines:#?}");
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("view outside.keep") && l.contains("keeps working"))
            .count(),
        1,
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("keeps its old output column name")),
        "{lines:#?}"
    );
}

/// The negative half of #823: only the exact typed module excuses a
/// carried referrer. A view of the same name in another schema, a
/// function dropped under the view's name, and a carried object the engine
/// named no module for — even one whose description is the dropped name —
/// all stay reported.
#[test]
fn a_carried_referrer_is_excused_only_by_its_exact_dropped_module() {
    let changes = pbps_model::ChangeSet {
        changes: vec![
            dropping(pbps_model::ModuleKind::View, "app.v"),
            dropping(pbps_model::ModuleKind::Function, "app.w()"),
        ],
    };
    let names = dropped_referrer_names(&changes);
    let modules = dropped_modules(&changes);
    let mut report = pbps_db::impact::ImpactReport {
        target: "column app.t.label".to_owned(),
        carried: vec![
            carried_view("view other.v", "other", "v"),
            carried_view("view app.w", "app", "w"),
            referrer("carried", "app.v", None),
            referrer("carried", "rule r on view app.v", None),
        ],
        ..Default::default()
    };
    let before = report.carried.clone();
    excuse_dropped_referrers(&mut report, &names, &modules);
    assert_eq!(report.carried, before);
}

fn carried_module(kind: pbps_model::ModuleKind, id: &str) -> pbps_db::impact::Referrer {
    pbps_db::impact::Referrer {
        removed_with: Some((kind, id.parse().unwrap())),
        ..referrer(kind.as_str(), id, None)
    }
}

/// #1647: a routine, a trigger and a user rule on a view are excused by
/// the plan's exact drop, as a view is — a routine by its full signature,
/// a trigger by its table, a rule by the view it is on. Another overload,
/// the same trigger name on another table, the same routine as a
/// procedure, and a routine of the same name in another schema all stay.
#[test]
fn a_carried_routine_or_trigger_is_excused_only_by_its_full_identity() {
    use pbps_model::ModuleKind::{Function, Procedure, Trigger, View};
    let changes = pbps_model::ChangeSet {
        changes: vec![
            dropping(Function, "app.f(integer, text)"),
            dropping(Trigger, "app.t.t_of"),
            dropping(View, "app.v"),
        ],
    };
    let names = dropped_referrer_names(&changes);
    let modules = dropped_modules(&changes);
    let survivors = vec![
        carried_module(Function, "app.f(integer)"),
        carried_module(Function, "other.f(integer, text)"),
        carried_module(Procedure, "app.f(integer, text)"),
        carried_module(Trigger, "app.u.t_of"),
    ];
    let mut report = pbps_db::impact::ImpactReport {
        target: "column app.t.label".to_owned(),
        carried: [
            vec![
                carried_module(Function, "app.f(integer, text)"),
                carried_module(Trigger, "app.t.t_of"),
                pbps_db::impact::Referrer {
                    kind: "rule".to_owned(),
                    ..carried_module(View, "app.v")
                },
            ],
            survivors.clone(),
        ]
        .concat(),
        ..Default::default()
    };
    excuse_dropped_referrers(&mut report, &names, &modules);
    assert_eq!(report.carried, survivors);
}

/// #307: a PostgreSQL report holding only carried objects printed an
/// "affects:" heading with nothing under it. Each carried object is now a
/// line of its own, with the engine's note about what carrying means.
#[test]
fn a_carried_only_report_prints_what_it_carries() {
    let report = pbps_db::impact::ImpactReport {
        target: "app.t.label".to_owned(),
        carried: vec![referrer("carried", "rule _RETURN on view outside.v", None)],
        ..Default::default()
    };
    let lines = impact_lines(
        &report,
        &crate::engine::impact_notes(pbps_db::Driver::Postgres, &report),
    );
    assert!(
        lines.iter().any(|l| l.contains("view outside.v")
            && l.contains("carried into the new name, keeps working")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("keeps its old output column name")),
        "{lines:#?}"
    );
}

/// The negative half of #307: in a mixed report a carried object is never
/// described as breaking or blocking, and an advisory one is never
/// described as carried.
#[test]
fn a_carried_object_is_not_described_as_broken_or_blocking() {
    let report = pbps_db::impact::ImpactReport {
        target: "app.t.label".to_owned(),
        advisory: vec![referrer("function", "outside.text_body()", None)],
        carried: vec![referrer("carried", "rule _RETURN on view outside.v", None)],
        ..Default::default()
    };
    let lines = impact_lines(
        &report,
        &crate::engine::impact_notes(pbps_db::Driver::Postgres, &report),
    );
    let carried: Vec<&String> = lines.iter().filter(|l| l.contains("outside.v")).collect();
    assert_eq!(carried.len(), 1, "{lines:#?}");
    assert!(!carried[0].contains("SCHEMABINDING"), "{lines:#?}");
    let advisory: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("outside.text_body"))
        .collect();
    assert_eq!(advisory.len(), 1, "{lines:#?}");
    assert!(!advisory[0].contains("carried"), "{lines:#?}");
}

/// SQL Server's output is unchanged: it carries nothing, and says what no
/// catalog can see only under an advisory list.
#[test]
fn sql_server_impact_output_is_unchanged() {
    let report = pbps_db::impact::ImpactReport {
        target: "dbo.t.email".to_owned(),
        advisory: vec![referrer("procedure", "dbo.p", Some("reads it by name"))],
        blocking: vec![referrer("view", "dbo.v", None)],
        ..Default::default()
    };
    assert_eq!(
        impact_lines(
            &report,
            &crate::engine::impact_notes(pbps_db::Driver::Mssql, &report)
        ),
        [
            "  procedure dbo.p — reads it by name",
            "  view dbo.v — SCHEMABINDING, which blocks the rename",
            "  Nothing outside the database is visible here: applications and downstream \
                 consumers need a human's checklist.",
        ]
    );
    let blocking_only = pbps_db::impact::ImpactReport {
        advisory: Vec::new(),
        ..report
    };
    assert_eq!(
        impact_lines(
            &blocking_only,
            &crate::engine::impact_notes(pbps_db::Driver::Mssql, &blocking_only)
        ),
        ["  view dbo.v — SCHEMABINDING, which blocks the rename"]
    );
}

#[test]
fn intermediate_names_are_absent_unless_the_plan_deliberately_reuses_them() {
    use pbps_dialect::Statement;
    use pbps_model::{Change, ChangeSet, PlannedChange};
    let rename = |uid: &str, from: &str, to: &str| {
        PlannedChange::new(Change::RenameTable {
            uid: uid.parse().unwrap(),
            from: from.parse().unwrap(),
            to: to.parse().unwrap(),
            defaults: Vec::new(),
        })
    };
    let mut changes = ChangeSet {
        changes: vec![rename("t_aaaaaa", "app.t", "moved.u")],
    };
    let mut statements = vec![
        Statement::new("").renaming("app.t".parse().unwrap(), "moved.t".parse().unwrap()),
        Statement::new("").renaming("moved.t".parse().unwrap(), "moved.u".parse().unwrap()),
    ];
    assert_eq!(
        removed_table_names(&changes, &statements),
        vec![
            "app.t".parse::<TableName>().unwrap(),
            "moved.t".parse().unwrap()
        ]
    );
    changes
        .changes
        .push(rename("t_bbbbbb", "app.other", "moved.t"));
    statements.push(
        Statement::new("").renaming("app.other".parse().unwrap(), "moved.t".parse().unwrap()),
    );
    assert_eq!(
        removed_table_names(&changes, &statements),
        vec![
            "app.other".parse::<TableName>().unwrap(),
            "app.t".parse().unwrap()
        ]
    );
}

use pbps_db::catalog::{Limitation, Pulled, UnmanagedModule};
use pbps_model::{DataMode, DataScope};

/// A permission the declarations cannot hold stops every command that
/// would write the state down, not just the one that plans over it: a
/// state recorded without it is one `verify` calls drift on sight.
#[test]
fn a_state_that_cannot_be_expressed_is_not_recorded_by_any_command() {
    // The real constructor, so the empty case is the one a clean
    // database actually produces.
    let empty = || {
        pbps_diff::scope(
            &Schema::default(),
            &IdsFile::default(),
            &BTreeSet::new(),
            &Default::default(),
        )
    };
    for then in ["plan again", "snapshot again", "baseline again"] {
        refuse_unexpressible(&empty(), "prod", then).expect("nothing to refuse");
    }

    let mut held = empty();
    held.unexpressible
        .push("role app_reader holds SELECT ON dbo.t(secret)".to_owned());
    for then in ["plan again", "snapshot again", "baseline again"] {
        let e = refuse_unexpressible(&held, "prod", then).expect_err(then);
        let msg = format!("{e:#}");
        assert!(msg.contains("app_reader"), "{msg}");
        // The remedy names the command the operator actually ran.
        assert!(msg.contains(then), "{msg}");
    }
}

/// The guard that keeps `apply` from recording somebody else's change as
/// its own: everything the plan does not touch has to come back the way
/// the baseline had it, and everything it does touch is exempt because
/// changing it is the whole point (DECISIONS 150).
#[test]
fn a_change_no_statement_of_this_plan_makes_refuses_the_apply() {
    fn table(ty: &str) -> pbps_model::Table {
        let mut t = pbps_model::Table::default();
        t.columns
            .insert("c".into(), pbps_model::Column::new(ty.parse().unwrap()));
        t
    }
    let dbo = || pbps_model::GrantTarget::Schema("dbo".to_owned());
    fn role(held: &[pbps_model::Permission]) -> pbps_model::Role {
        let mut grants = BTreeMap::new();
        if !held.is_empty() {
            grants.insert(
                pbps_model::GrantTarget::Schema("dbo".to_owned()),
                held.iter().copied().collect(),
            );
        }
        pbps_model::Role {
            description: None,
            grants,
        }
    }
    let schema = |cols: &str, held: &[pbps_model::Permission]| {
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), table(cols));
        s.roles.insert("app".to_owned().into(), role(held));
        s
    };
    let changes = |c: Vec<pbps_model::Change>| pbps_model::ChangeSet {
        changes: c.into_iter().map(pbps_model::PlannedChange::new).collect(),
    };
    let refuse = |cs: &pbps_model::ChangeSet, before: &Schema, after: &Schema| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            cs,
            before,
            after,
            "prod",
            Settled::Whole,
        )
        .map_err(|e| format!("{e:#}"))
    };

    let before = schema("int", &[pbps_model::Permission::Select]);
    // Nothing moved: the plan may be empty and the apply still records.
    refuse(&changes(vec![]), &before, &before).expect("nothing moved");

    // A grant revoked by another session while the plan ran. The plan
    // never mentions the role, so nothing else would ever notice: the
    // read-back records the revocation and the next `verify` is clean.
    let robbed = schema("int", &[]);
    let e = refuse(&changes(vec![]), &before, &robbed).expect_err("the role moved");
    assert!(e.contains("role app"), "{e}");
    // The reason, and no remedy: that is the caller's to name (190).
    assert!(e.contains("written down as this plan's own result"), "{e}");
    assert!(!e.contains("rolled back"), "{e}");

    // The same change, when it *is* this plan's: a plan that revokes the
    // grant must not be refused for having revoked it.
    let planned = changes(vec![pbps_model::Change::Revoke {
        role: "app".to_owned().into(),
        target: dbo(),
        permissions: [pbps_model::Permission::Select].into_iter().collect(),
    }]);
    refuse(&planned, &before, &robbed).expect("the plan's own change");

    // A table retyped behind the plan's back, and the same retype planned.
    let retyped = schema("bigint", &[pbps_model::Permission::Select]);
    let e = refuse(&changes(vec![]), &before, &retyped).expect_err("the table moved");
    assert!(e.contains("dbo.t"), "{e}");
    let planned = changes(vec![pbps_model::Change::AlterColumnType {
        uid: "c_aaaaaa".parse().unwrap(),
        column: pbps_model::ColumnRef::new("dbo.t".parse().unwrap(), "c"),
        from: "int".parse().unwrap(),
        to: "bigint".parse().unwrap(),
        from_nullable: true,
        to_nullable: true,
        through_external_partitions: false,
        from_collation: None,
        to_collation: None,
        using: None,
    }]);
    refuse(&planned, &before, &retyped).expect("the plan's own change");

    // Appearing and vanishing are movements too, and neither reads as
    // "nothing there": a table dropped by somebody else is not the same
    // answer as a table this plan drops.
    let mut gone = before.clone();
    gone.tables.remove(&"dbo.t".parse::<TableName>().unwrap());
    let e = refuse(&changes(vec![]), &before, &gone).expect_err("the table went");
    assert!(e.contains("dbo.t is gone"), "{e}");
    let e = refuse(&changes(vec![]), &gone, &before).expect_err("the table arrived");
    assert!(e.contains("dbo.t is there"), "{e}");
}

/// A numbering change is held to the source it gives the column, start
/// and increment included, not to its kind alone: a retune another
/// session undoes before the read-back leaves the kind as the plan made
/// it, and is still found (SPEC §7.6, #1930 review). And a source the
/// plan does not touch is compared as before.
#[test]
fn a_numbering_change_is_held_to_the_source_it_gives() {
    use pbps_model::{Identity, ValueSource};
    let identity = |seed, increment| {
        ValueSource::Identity(Identity {
            seed,
            increment,
            options: Default::default(),
        })
    };
    let schema = |source: ValueSource| {
        let mut c = pbps_model::Column::new("integer".parse().unwrap()).not_null();
        c.value_source = source;
        let mut t = pbps_model::Table::default();
        t.columns.insert(
            "a".into(),
            pbps_model::Column::new("integer".parse().unwrap()),
        );
        t.columns.insert("id".into(), c);
        let mut s = Schema::default();
        s.tables.insert("app.t".parse().unwrap(), t);
        s
    };
    let changes = |c: Vec<pbps_model::Change>| pbps_model::ChangeSet {
        changes: c.into_iter().map(pbps_model::PlannedChange::new).collect(),
    };
    let refuse = |cs: &pbps_model::ChangeSet, before: &Schema, after: &Schema| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            cs,
            before,
            after,
            "prod",
            Settled::Whole,
        )
        .map_err(|e| format!("{e:#}"))
    };
    let retune = changes(vec![pbps_model::Change::AlterColumnNumbering {
        uid: "c_aaaaaa".parse().unwrap(),
        column: pbps_model::ColumnRef::new("app.t".parse().unwrap(), "id"),
        ty: "integer".parse().unwrap(),
        from: identity(1, 1),
        to: identity(5, 2),
    }]);
    let before = schema(identity(1, 1));
    refuse(&retune, &before, &schema(identity(5, 2))).expect("the plan's own change");
    let e = refuse(&retune, &before, &schema(identity(5, 3))).expect_err("undone");
    assert!(
        e.contains("column `id` does not have the numbering source this plan gives it"),
        "{e}"
    );
    let e = refuse(&retune, &before, &schema(ValueSource::None)).expect_err("removed");
    assert!(e.contains("numbering source"), "{e}");
    // A kept sequence retuned under a declaration that names none keeps
    // the name it had, which another session renaming it moves.
    let serial = |start, name: &str| {
        ValueSource::Sequence(pbps_model::OwnedSequence {
            start,
            increment: 1,
            name: (!name.is_empty()).then(|| name.to_owned()),
            options: Default::default(),
        })
    };
    let unnamed = changes(vec![pbps_model::Change::AlterColumnNumbering {
        uid: "c_aaaaaa".parse().unwrap(),
        column: pbps_model::ColumnRef::new("app.t".parse().unwrap(), "id"),
        ty: "integer".parse().unwrap(),
        from: serial(1, "t_id_seq"),
        to: serial(5, ""),
    }]);
    let before = schema(serial(1, "t_id_seq"));
    refuse(&unnamed, &before, &schema(serial(5, "t_id_seq"))).expect("kept");
    let e = refuse(&unnamed, &before, &schema(serial(5, "other_seq"))).expect_err("renamed");
    assert!(e.contains("numbering source"), "{e}");
    let before = schema(identity(1, 1));
    // Negative: a plan that touches only another column excuses nothing.
    let elsewhere = changes(vec![pbps_model::Change::AlterColumnDefault {
        uid: "c_bbbbbb".parse().unwrap(),
        column: pbps_model::ColumnRef::new("app.t".parse().unwrap(), "a"),
        from: None,
        to: Some("1".to_owned()),
    }]);
    let e = refuse(&elsewhere, &before, &schema(identity(5, 2))).expect_err("unplanned");
    assert!(e.contains("column `id` has a different identity"), "{e}");
}

/// A generated column's generation is compared across an apply like its
/// type: a planned expression change excuses a new expression and
/// nothing else, and a column that stops being generated, or changes
/// kind, is somebody else's work whatever the plan says (SPEC §7.6,
/// DEC-1168.1).
#[test]
fn a_generation_this_plan_does_not_move_is_still_compared() {
    let schema = |generated: Option<(&str, bool)>| {
        let mut c = pbps_model::Column::new("integer".parse().unwrap());
        c.value_source = generated.map_or(pbps_model::ValueSource::None, |(expression, stored)| {
            pbps_model::ValueSource::Generated(pbps_model::Generated {
                expression: expression.to_owned(),
                stored,
            })
        });
        let mut t = pbps_model::Table::default();
        t.columns.insert(
            "a".into(),
            pbps_model::Column::new("integer".parse().unwrap()),
        );
        t.columns.insert("g".into(), c);
        let mut s = Schema::default();
        s.tables.insert("app.t".parse().unwrap(), t);
        s
    };
    let changes = |c: Vec<pbps_model::Change>| pbps_model::ChangeSet {
        changes: c.into_iter().map(pbps_model::PlannedChange::new).collect(),
    };
    let refuse = |cs: &pbps_model::ChangeSet, before: &Schema, after: &Schema| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            cs,
            before,
            after,
            "prod",
            Settled::Whole,
        )
        .map_err(|e| format!("{e:#}"))
    };
    let recompute = changes(vec![pbps_model::Change::AlterColumnExpression {
        uid: "c_aaaaaa".parse().unwrap(),
        column: pbps_model::ColumnRef::new("app.t".parse().unwrap(), "g"),
        from: "(a * 2)".to_owned(),
        to: "a * 3".to_owned(),
    }]);
    // A plan that touches the table through another column, so the
    // table is compared column by column rather than whole.
    let elsewhere = changes(vec![pbps_model::Change::AlterColumnDefault {
        uid: "c_bbbbbb".parse().unwrap(),
        column: pbps_model::ColumnRef::new("app.t".parse().unwrap(), "a"),
        from: None,
        to: Some("1".to_owned()),
    }]);
    let before = schema(Some(("(a * 2)", true)));
    let changed = schema(Some(("(a * 3)", true)));
    refuse(&recompute, &before, &changed).expect("the plan's own change");
    let e = refuse(&elsewhere, &before, &changed).expect_err("an unplanned expression");
    assert!(
        e.contains("column `g` has a different generation expression"),
        "{e}"
    );
    // `DROP EXPRESSION` by another session leaves an ordinary column,
    // which no change of the plan makes, not even its expression change.
    let ordinary = schema(None);
    for plan in [elsewhere, recompute.clone()] {
        let e = refuse(&plan, &before, &ordinary).expect_err("the generation went");
        assert!(e.contains("column `g` has a different generation "), "{e}");
    }
    let virtual_ = schema(Some(("(a * 2)", false)));
    let e = refuse(&recompute, &before, &virtual_).expect_err("the kind moved");
    assert!(e.contains("different generation "), "{e}");
    refuse(&changes(vec![]), &before, &before).expect("nothing moved");
}

/// A role the plan touches is exempt down to the permissions it moves, and
/// no further. A plan that adds one grant says nothing about the rest of
/// the role's set, and neither does any statement it runs — so exempting
/// the whole role let a concurrent revoke of an unchanged grant be
/// recorded as this plan's own result (DECISIONS 156).
#[test]
fn a_grant_this_plan_does_not_move_is_still_compared() {
    use pbps_model::{GrantTarget, Permission};
    let dbo = || GrantTarget::Schema("dbo".to_owned());
    let role = |held: &[Permission]| {
        let mut grants = BTreeMap::new();
        if !held.is_empty() {
            grants.insert(dbo(), held.iter().copied().collect());
        }
        pbps_model::Role {
            description: None,
            grants,
        }
    };
    let schema = |held: &[Permission]| {
        let mut s = Schema::default();
        s.roles.insert("app".to_owned().into(), role(held));
        s
    };
    let plan_granting = |permission: Permission| pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Grant {
            role: "app".to_owned().into(),
            target: dbo(),
            permissions: [permission].into_iter().collect(),
        })],
    };
    let refuse = |cs: &pbps_model::ChangeSet, before: &Schema, after: &Schema| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            cs,
            before,
            after,
            "prod",
            Settled::Whole,
        )
        .map_err(|e| format!("{e:#}"))
    };

    let before = schema(&[Permission::Select]);
    let after = schema(&[Permission::Select, Permission::Insert]);
    // The plan's own grant, and nothing else moved.
    refuse(&plan_granting(Permission::Insert), &before, &after).expect("the plan's own change");

    // The same plan, with `SELECT` revoked underneath it by somebody else.
    // The role is named by the plan, so a whole-role exemption saw nothing.
    let robbed = schema(&[Permission::Insert]);
    let e = refuse(&plan_granting(Permission::Insert), &before, &robbed)
        .expect_err("the untouched grant moved");
    assert!(e.contains("role app"), "{e}");
    assert!(e.contains("schema::dbo"), "{e}");

    // And a revoke the plan *did* ask for is not reported as movement.
    let revoking = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Revoke {
            role: "app".to_owned().into(),
            target: dbo(),
            permissions: [Permission::Select].into_iter().collect(),
        })],
    };
    refuse(&revoking, &before, &schema(&[])).expect("the plan's own revoke");

    // The other half, and the reason the two directions are kept apart:
    // the plan's own change is *verified*, not excused. Nothing else in a
    // run has a postcondition on a permission, so a grant reversed before
    // the read-back — by another session, or by a DDL trigger inside the
    // statement itself — was recorded as the plan's result (DECISIONS 160).
    let e = refuse(&plan_granting(Permission::Insert), &before, &before)
        .expect_err("the grant this plan asked for did not take");
    assert!(e.contains("role app"), "{e}");
    // #700: and which permission, in either direction.
    assert!(e.contains("it lacks insert"), "{e}");
    assert!(!e.contains("still holds"), "{e}");
    let e = refuse(&revoking, &before, &before)
        .expect_err("the revoke this plan asked for did not take");
    assert!(e.contains("role app"), "{e}");
    assert!(e.contains("it still holds select"), "{e}");
    assert!(!e.contains("lacks"), "{e}");
    // #1052: and a permission nobody planned is not called a revoke that
    // did not take. `INSERT` arrived from another session beside the
    // `SELECT` this plan failed to revoke.
    let e = refuse(
        &revoking,
        &before,
        &schema(&[Permission::Select, Permission::Insert]),
    )
    .expect_err("a revoke that did not take, and a grant nobody planned");
    assert!(e.contains("it still holds select"), "{e}");
    assert!(e.contains("it unexpectedly holds insert"), "{e}");
    assert!(!e.contains("still holds insert"), "{e}");
}

/// A table's rename forwards the grants on the table and nothing else.
/// Where routines have their own namespace a routine may share the
/// table's name, and its grant stays where it is when the table moves;
/// forwarding it through the rename refused an apply that had done
/// exactly what the plan said.
#[test]
fn a_table_rename_does_not_forward_the_grant_on_a_routine_of_that_name() {
    use pbps_model::{GrantTarget, Permission};
    let routine: GrantTarget = "dbo.f(integer)".parse().unwrap();
    let with = |table: &str, on_routine: &GrantTarget| {
        let mut grants = BTreeMap::new();
        grants.insert(
            GrantTarget::Object(table.parse().unwrap()),
            [Permission::Select].into_iter().collect::<BTreeSet<_>>(),
        );
        grants.insert(
            on_routine.clone(),
            [Permission::Execute].into_iter().collect::<BTreeSet<_>>(),
        );
        let mut s = Schema::default();
        s.tables
            .insert(table.parse().unwrap(), pbps_model::Table::default());
        s.modules.insert(
            "dbo.f(integer)".parse().unwrap(),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::Function,
                description: None,
                definition: "RETURN 1".to_owned(),
                options: Default::default(),
            },
        );
        s.roles.insert(
            "app".to_owned().into(),
            pbps_model::Role {
                description: None,
                grants,
            },
        );
        s
    };
    let rename = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::RenameTable {
                uid: "t_aaaaaa".parse().unwrap(),
                from: "dbo.f".parse().unwrap(),
                to: "dbo.g".parse().unwrap(),
                defaults: Vec::new(),
            },
        )],
    };
    let refuse = |after: &Schema| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &rename,
            &with("dbo.f", &routine),
            after,
            "prod",
            Settled::Whole,
        )
        .map_err(|e| format!("{e:#}"))
    };
    // The table's grant moved with the table; the routine's stayed.
    refuse(&with("dbo.g", &routine)).expect("the routine's grant did not move");
    // A routine grant that *did* move to the new name is movement.
    let moved: GrantTarget = "dbo.g(integer)".parse().unwrap();
    let e = refuse(&with("dbo.g", &moved)).expect_err("the routine's grant moved");
    assert!(e.contains("role app"), "{e}");
}

/// The rename impact report names a referrer as the catalog does,
/// `schema.name`; a trigger this plan drops has to be looked up under that
/// spelling, not under its id's, or the drop excuses nothing.
#[test]
fn a_dropped_trigger_is_excused_from_the_rename_impact_under_its_catalog_name() {
    let changes = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::DropModule {
                id: "dbo.t.audit".parse().unwrap(),
                kind: pbps_model::ModuleKind::Trigger,
            }),
            pbps_model::PlannedChange::new(pbps_model::Change::DropModule {
                id: "dbo.v".parse().unwrap(),
                kind: pbps_model::ModuleKind::View,
            }),
            pbps_model::PlannedChange::new(pbps_model::Change::AlterModule {
                id: "dbo.kept".parse().unwrap(),
                module: Box::new(pbps_model::Module {
                    kind: pbps_model::ModuleKind::View,
                    description: None,
                    definition: "SELECT 1".to_owned(),
                    options: Default::default(),
                }),
            }),
        ],
    };
    let names: Vec<String> = dropped_referrer_names(&changes).into_iter().collect();
    assert_eq!(names, ["dbo.audit", "dbo.v"]);
}

/// A dropped routine takes its own grants and no other's: where routines
/// overload, a plan dropping `dbo.f(integer)` says nothing about the
/// grants on `dbo.f(text)`, and one of those revoked underneath the apply
/// is movement to refuse, not a permission gone with the drop
/// (ADR-0009 §1, DECISIONS 158).
#[test]
fn a_dropped_overload_does_not_excuse_the_siblings_grants() {
    use pbps_model::{GrantTarget, Permission};
    let f = |body: &str| pbps_model::Module {
        kind: pbps_model::ModuleKind::Function,
        description: None,
        definition: body.to_owned(),
        options: Default::default(),
    };
    let int: ModuleId = "dbo.f(integer)".parse().unwrap();
    let text: ModuleId = "dbo.f(text)".parse().unwrap();
    let on = |id: &ModuleId| match id {
        ModuleId::Routine(r) => GrantTarget::Routine(r.clone()),
        ModuleId::Named(_) | ModuleId::Trigger { .. } => unreachable!(),
    };
    let schema = |with_int: bool, text_granted: bool| {
        let mut s = Schema::default();
        if with_int {
            s.modules.insert(int.clone(), f("RETURN 1"));
        }
        s.modules.insert(text.clone(), f("RETURN 'a'"));
        let mut grants = BTreeMap::new();
        if with_int {
            grants.insert(on(&int), [Permission::Execute].into_iter().collect());
        }
        if text_granted {
            grants.insert(on(&text), [Permission::Execute].into_iter().collect());
        }
        s.roles.insert(
            "app".to_owned().into(),
            pbps_model::Role {
                description: None,
                grants,
            },
        );
        s
    };
    let dropping_int = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::DropModule {
                id: int.clone(),
                kind: pbps_model::ModuleKind::Function,
            },
        )],
    };
    let refuse = |after: &Schema| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &dropping_int,
            &schema(true, true),
            after,
            "prod",
            Settled::Whole,
        )
        .map_err(|e| format!("{e:#}"))
    };
    // The drop took its own grant with it, and the sibling's stands.
    refuse(&schema(false, true)).expect("only the dropped overload's grant went");
    // The sibling's grant went too: not this plan's doing.
    // Named at role granularity: the plan does not name the role, so the
    // whole-role compare answers, as it does for any untouched role.
    let e = refuse(&schema(false, false)).expect_err("the sibling's grant moved");
    assert!(
        e.contains("role app is not what the plan was approved over"),
        "{e}"
    );
}

/// A single staged parent rename carries only its own incoming references.
/// The child's other foreign key and a vacated name's new occupant remain
/// independently held. Multiple parents are a transactional plan (ADR-0003).
#[test]
fn a_checkpoint_carries_only_the_named_parent_in_a_child() {
    use pbps_model::{Change, ChangeSet, ForeignKey, PlannedChange, Table};

    let child = |first: &str, second: &str| {
        let mut t = Table::default();
        for (n, parent) in [("fk_one", first), ("fk_two", second)] {
            t.foreign_keys.insert(
                n.into(),
                ForeignKey {
                    columns: vec!["pid".into()],
                    references_table: parent.parse().unwrap(),
                    references_columns: vec!["id".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
        }
        let mut s = Schema::default();
        s.tables.insert("dbo.child".parse().unwrap(), t);
        s
    };

    let renames = ChangeSet {
        changes: vec![PlannedChange::new(Change::RenameTable {
            uid: "t_aaaaaa".parse().unwrap(),
            from: "dbo.one".parse().unwrap(),
            to: "dbo.one_x".parse().unwrap(),
            defaults: Vec::new(),
        })],
    };

    // The parents themselves, so the reads are the shape a real one has:
    // whether a name has been reused is decided by what each read holds.
    let with = |first: &str, second: &str, parents: &[&str]| {
        let mut s = child(first, second);
        for t in parents {
            s.tables.insert(t.parse().unwrap(), Table::default());
        }
        s
    };

    // The first parent has been renamed and the second has not: the child
    // is spelled one way for one key and the other way for the other.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renames,
        &with("dbo.one", "dbo.two", &["dbo.one", "dbo.two"]),
        &with("dbo.one_x", "dbo.two", &["dbo.one_x", "dbo.two"]),
        "prod",
        Settled::SoFar,
    )
    .expect("a child carried only halfway");

    // The alias: another session creates a *different* table under the
    // name the rename vacated, and repoints the untouched child at it.
    // Undoing collapses the two identities — the checkpoint's `dbo.one_x`
    // and the closing read's `dbo.one` both rewind to `dbo.one` — and a
    // real retarget of a table the plan does not touch would be recorded
    // as the plan's own result, which SPEC 7.6 promises to catch. Holding
    // both spellings in one read is what says the name has been reused, so
    // that rename is not undone at all and the difference stands.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renames,
        // The checkpoint the last statement left: the single rename done.
        &with("dbo.one_x", "dbo.two", &["dbo.one_x", "dbo.two"]),
        // The read after it: a new `dbo.one` exists and the child points
        // at it.
        &with("dbo.one", "dbo.two", &["dbo.one", "dbo.one_x", "dbo.two"]),
        "prod",
        Settled::SoFar,
    )
    .expect_err("a child repointed at a new table under the vacated name");
    assert!(format!("{e:#}").contains("dbo.child"), "{e:#}");

    // The negative case: a parent this plan never names is movement.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renames,
        &child("dbo.one", "dbo.two"),
        &child("dbo.one_x", "dbo.elsewhere"),
        "prod",
        Settled::SoFar,
    )
    .expect_err("a parent no rename of this plan produces");
    assert!(format!("{e:#}").contains("dbo.child"), "{e:#}");
}

/// A single column rename carries its constraints at a checkpoint;
/// multiple renames compose only at transactional settlement (DEC-1950.1).
#[test]
fn renames_keep_constraints_at_single_change_checkpoints_and_transactional_close() {
    use pbps_model::{Change, ChangeSet, Column, PlannedChange, PrimaryKey, Table};

    // The columns and the constraints that name them move together: a
    // read finds both spelled as the statements so far have left them.
    let table = |pk: &str, ix: &str| {
        let mut t = Table {
            columns: [pk, ix]
                .into_iter()
                .map(|c| (c.into(), Column::new("int".parse().unwrap())))
                .collect(),
            primary_key: Some(PrimaryKey {
                name: Some("pk_t".into()),
                columns: vec![pk.into()],
                storage_parameters: Default::default(),
            }),
            ..Default::default()
        };
        t.unique.insert(
            "uq_t".into(),
            pbps_model::UniqueConstraint {
                columns: vec![ix.into()],
                storage_parameters: Default::default(),
            },
        );
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), t);
        s
    };

    let renames = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::RenameColumn {
                uid: "c_aaaaaa".parse().unwrap(),
                table: "dbo.t".parse().unwrap(),
                from: "a".into(),
                to: "a2".into(),
                table_was: None,
            }),
            PlannedChange::new(Change::RenameColumn {
                uid: "c_bbbbbb".parse().unwrap(),
                table: "dbo.t".parse().unwrap(),
                from: "b".into(),
                to: "b2".into(),
                table_was: None,
            }),
        ],
    };

    let first = ChangeSet {
        changes: vec![renames.changes[0].clone()],
    };
    // Staged reads admit the first rename alone. Both renames together
    // are still checked transactionally below.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &first,
        &table("a", "b"),
        &table("a2", "b"),
        "prod",
        Settled::SoFar,
    )
    .expect("a checkpoint of the single rename");

    // And at the end, both.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renames,
        &table("a", "b"),
        &table("a2", "b2"),
        "prod",
        Settled::Whole,
    )
    .expect("both renames done");

    // The negative case: a spelling this plan never produces is movement,
    // at a checkpoint as much as at the end.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &first,
        &table("a", "b"),
        &table("a2", "b"),
        "prod",
        Settled::SoFar,
    );
    e.expect("the control: the same read with nothing else moved");

    // Two reads already at the final spelling still compare directly;
    // neither side can borrow the retired source spelling's definition.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renames,
        &table("a2", "b2"),
        &table("a2", "b2"),
        "prod",
        Settled::Closing,
    )
    .expect("two reads that both carry the renames");

    // The negative case: a constraint spelled a way no rename of this plan
    // produces is movement, at a checkpoint as much as at the end.
    let mut drifted = table("a2", "b");
    drifted
        .tables
        .get_mut(&"dbo.t".parse::<TableName>().unwrap())
        .unwrap()
        .unique
        .insert(
            "uq_t".into(),
            pbps_model::UniqueConstraint {
                columns: vec!["elsewhere".into()],
                storage_parameters: Default::default(),
            },
        );
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &first,
        &table("a", "b"),
        &drifted,
        "prod",
        Settled::SoFar,
    )
    .expect_err("a spelling no rename of this plan leaves");
    assert!(format!("{e:#}").contains("uq_t"), "{e:#}");
}

/// A module the plan writes is held to the definition it wrote. Nothing
/// else in a run is: `CREATE OR ALTER` reports success and says nothing
/// about what is now stored (DECISIONS 160).
#[test]
fn a_module_this_plan_writes_is_held_to_what_it_wrote() {
    let view = |definition: &str| pbps_model::Module {
        kind: pbps_model::ModuleKind::View,
        description: None,
        definition: definition.to_owned(),
        options: Default::default(),
    };
    let schema_with = |module: Option<pbps_model::Module>| {
        let mut s = Schema::default();
        if let Some(m) = module {
            s.modules.insert("dbo.v".parse().unwrap(), m);
        }
        s
    };
    let writing = |definition: &str| pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::AlterModule {
                id: "dbo.v".parse().unwrap(),
                module: Box::new(view(definition)),
            },
        )],
    };
    let before = schema_with(Some(view("SELECT 1")));

    // What the plan wrote is what is there.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &writing("SELECT 2"),
        &before,
        &schema_with(Some(view("SELECT 2"))),
        "prod",
        Settled::Whole,
    )
    .expect("the plan's own definition");

    // Something else rewrote it straight afterwards.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &writing("SELECT 2"),
        &before,
        &schema_with(Some(view("SELECT 3"))),
        "prod",
        Settled::Whole,
    )
    .expect_err("not the definition this plan wrote");
    assert!(format!("{e:#}").contains("dbo.v"), "{e:#}");

    // Or dropped it.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &writing("SELECT 2"),
        &before,
        &schema_with(None),
        "prod",
        Settled::Whole,
    )
    .expect_err("the module is gone");
    assert!(format!("{e:#}").contains("is not there"), "{e:#}");

    // And the other direction: a module this plan drops must be gone.
    let dropping = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::DropModule {
                id: "dbo.v".parse().unwrap(),
                kind: pbps_model::ModuleKind::View,
            },
        )],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &dropping,
        &before,
        &schema_with(None),
        "prod",
        Settled::Whole,
    )
    .expect("the plan's own drop");
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &dropping,
        &before,
        &before,
        "prod",
        Settled::Whole,
    )
    .expect_err("the module is still there");
    assert!(format!("{e:#}").contains("is still there"), "{e:#}");
}

/// A `serial`'s sequence keeps its name across a rename of its column or
/// table, where the name either would generate changes: the two reads
/// compare the sequence, not the generated name, so the plan's own
/// rename is not somebody else's change. A sequence another session
/// renamed, or restarted elsewhere, still is (#1631).
#[test]
fn a_renamed_serial_keeps_its_sequence_across_the_apply() {
    let serial = |name: Option<&str>, start: i64| {
        let mut c = pbps_model::Column::new("integer".parse().unwrap()).not_null();
        c.value_source = pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence {
            start,
            increment: 1,
            name: name.map(Into::into),
            options: Default::default(),
        });
        c
    };
    let schema = |table: &str, column: &str, spec: pbps_model::Column| {
        let mut t = pbps_model::Table::default();
        t.columns.insert(column.into(), spec);
        let mut s = Schema::default();
        s.tables.insert(table.parse().unwrap(), t);
        s
    };
    let check = |change: pbps_model::Change, after: &Schema| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &pbps_model::ChangeSet {
                changes: vec![pbps_model::PlannedChange::new(change)],
            },
            &schema("app.t", "id", serial(None, 1)),
            after,
            "prod",
            Settled::Whole,
        )
    };
    let column = || pbps_model::Change::RenameColumn {
        uid: "c_aaaaaa".parse().unwrap(),
        table: "app.t".parse().unwrap(),
        from: "id".into(),
        to: "n".into(),
        table_was: None,
    };
    let table = || pbps_model::Change::RenameTable {
        uid: "t_aaaaaa".parse().unwrap(),
        from: "app.t".parse().unwrap(),
        to: "app.u".parse().unwrap(),
        defaults: Vec::new(),
    };
    check(column(), &schema("app.t", "n", serial(Some("t_id_seq"), 1)))
        .expect("the column's rename leaves its sequence's name");
    check(table(), &schema("app.u", "id", serial(Some("t_id_seq"), 1)))
        .expect("the table's rename leaves its sequence's name");

    // The negative cases: a sequence renamed, or started elsewhere, by
    // somebody else.
    for after in [
        schema("app.t", "n", serial(Some("other_seq"), 1)),
        schema("app.t", "n", serial(Some("t_id_seq"), 5)),
    ] {
        let e = check(column(), &after).expect_err("not this plan's");
        assert!(format!("{e:#}").contains("sequence"), "{e:#}");
    }
}

/// #541: a name that changes hands within the plan. Both reads hold it,
/// for two different columns, so the comparison follows the plan's own
/// column changes from each baseline column to the name it ends under.
/// Each case gives the columns different types, so comparing two of them
/// by name is a type difference no change excuses.
#[test]
fn a_column_name_that_changes_hands_is_compared_by_the_column_holding_it() {
    let table = |columns: &[(&str, &str)]| {
        let mut t = pbps_model::Table::default();
        for (name, spec) in columns {
            t.columns.insert(
                (*name).into(),
                pbps_model::Column::new(spec.parse().unwrap()),
            );
        }
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), t);
        s
    };
    let named: TableName = "dbo.t".parse().unwrap();
    let rename = |uid: &str, from: &str, to: &str| {
        pbps_model::PlannedChange::new(pbps_model::Change::RenameColumn {
            uid: uid.parse().unwrap(),
            table: named.clone(),
            from: from.into(),
            to: to.into(),
            table_was: None,
        })
    };
    let add = |uid: &str, name: &str, spec: &str| {
        pbps_model::PlannedChange::new(pbps_model::Change::AddColumn {
            uid: uid.parse().unwrap(),
            table: named.clone(),
            name: name.into(),
            column: Box::new(pbps_model::Column::new(spec.parse().unwrap())),
        })
    };
    let drop = |uid: &str, name: &str| {
        pbps_model::PlannedChange::new(pbps_model::Change::DropColumn {
            uid: uid.parse().unwrap(),
            column: named.column(name),
        })
    };
    let check = |changes: Vec<pbps_model::PlannedChange>,
                 before: &[(&str, &str)],
                 after: &[(&str, &str)]| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &pbps_model::ChangeSet { changes },
            &table(before),
            &table(after),
            "prod",
            Settled::Whole,
        )
    };

    // A chain, in the order the differ now emits it.
    check(
        vec![rename("c_aaaaaa", "b", "c"), rename("c_bbbbbb", "a", "b")],
        &[("a", "int"), ("b", "bigint")],
        &[("b", "int"), ("c", "bigint")],
    )
    .expect("`b` is the renamed `a` and `c` the renamed `b`");

    // A source name reclaimed by a new column.
    check(
        vec![
            rename("c_aaaaaa", "a", "b"),
            add("c_bbbbbb", "a", "smallint"),
        ],
        &[("a", "int")],
        &[("b", "int"), ("a", "smallint")],
    )
    .expect("`b` is the renamed `a`, and `a` is the new column");

    // And with a drop in front: `drop b`, `a -> b`, `add a`.
    check(
        vec![
            drop("c_cccccc", "b"),
            rename("c_aaaaaa", "a", "b"),
            add("c_bbbbbb", "a", "smallint"),
        ],
        &[("a", "int"), ("b", "bigint")],
        &[("b", "int"), ("a", "smallint")],
    )
    .expect("the dropped `b`, the renamed `a`, and the new `a` are three columns");

    // Negative: the identity is followed, not excused. The column the
    // chain carries from `a` to `b` came back retyped.
    let e = check(
        vec![rename("c_aaaaaa", "b", "c"), rename("c_bbbbbb", "a", "b")],
        &[("a", "int"), ("b", "bigint")],
        &[("b", "text"), ("c", "bigint")],
    )
    .expect_err("the renamed `a` is not what the plan was approved over");
    assert!(format!("{e:#}").contains("`b`"), "{e:#}");
}

/// A name that belongs to two columns across one plan: the one this plan
/// drops, and the one it renames into the name that leaves.
///
/// The shape comparison is keyed by name, and the baseline holds both
/// spellings, so the rename rewind is skipped (the ambiguity rule above)
/// and `note` on the two sides was compared as one column — the dropped
/// occupant against the renamed survivor. Identical definitions hid it;
/// `note int` against `label nvarchar(50)` is a type difference no change
/// of this plan excuses, and a valid plan was refused at its own
/// checkpoint after the engine had already performed it (issue #398).
#[test]
fn a_column_renamed_into_a_dropped_columns_name_is_not_movement() {
    let table = |columns: &[(&str, &str)]| {
        let mut t = pbps_model::Table::default();
        for (name, spec) in columns {
            t.columns.insert(
                (*name).into(),
                pbps_model::Column::new(spec.parse().unwrap()),
            );
        }
        let mut s = Schema::default();
        s.tables.insert("dbo.s".parse().unwrap(), t);
        s
    };
    let named: TableName = "dbo.s".parse().unwrap();
    // `order_key` order: the drop frees the name, then the rename claims
    // it (DECISIONS 474).
    let changes = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::DropColumn {
                uid: "c_aaaaaa".parse().unwrap(),
                column: named.column("note"),
            }),
            pbps_model::PlannedChange::new(pbps_model::Change::RenameColumn {
                uid: "c_bbbbbb".parse().unwrap(),
                table: named.clone(),
                from: "label".into(),
                to: "note".into(),
                table_was: None,
            }),
        ],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &changes,
        &table(&[
            ("code", "varchar(20)"),
            ("label", "nvarchar(50)"),
            ("note", "int"),
        ]),
        &table(&[("code", "varchar(20)"), ("note", "nvarchar(50)")]),
        "prod",
        Settled::Whole,
    )
    .expect("the dropped occupant and the renamed survivor are two columns");

    // An index on the renamed column, which the engine carries to the
    // name it takes: the later read is undone where it holds the rename
    // alone, though the earlier one holds both names (#1699). It was
    // compared under two spellings, and the plan refused at apply.
    let indexed = |s: Schema, column: &str| {
        let mut s = s;
        s.tables.get_mut(&named).unwrap().indexes.insert(
            "s_ix".into(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn::column(column)],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: pbps_model::IndexMethod::default(),
                storage_parameters: BTreeMap::new(),
            },
        );
        s
    };
    let baseline = || {
        indexed(
            table(&[
                ("code", "varchar(20)"),
                ("label", "nvarchar(50)"),
                ("note", "int"),
            ]),
            "label",
        )
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &changes,
        &baseline(),
        &indexed(
            table(&[("code", "varchar(20)"), ("note", "nvarchar(50)")]),
            "note",
        ),
        "prod",
        Settled::Whole,
    )
    .expect("the index went with the renamed column");
    // Negative: the index moved to another column is still movement.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &changes,
        &baseline(),
        &indexed(
            table(&[("code", "varchar(20)"), ("note", "nvarchar(50)")]),
            "code",
        ),
        "prod",
        Settled::Whole,
    )
    .expect_err("an index another session moved");
    assert!(format!("{e:#}").contains("s_ix"), "{e:#}");

    // And the negative case: a column nothing in the plan touches, whose
    // type moved under it, is still movement.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &changes,
        &table(&[
            ("code", "varchar(20)"),
            ("label", "nvarchar(50)"),
            ("note", "int"),
        ]),
        &table(&[("code", "int"), ("note", "nvarchar(50)")]),
        "prod",
        Settled::Whole,
    )
    .expect_err("a column the plan never names is answerable for itself");
    assert!(format!("{e:#}").contains("`code`"), "{e:#}");

    // When both reads already hold the survivor under its final name,
    // falling back to the absent source would lose its held definition.
    // A final comparison must still reject a concurrent retype (SPEC 7.6).
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &changes,
        &table(&[("code", "varchar(20)"), ("note", "nvarchar(50)")]),
        &table(&[("code", "varchar(20)"), ("note", "int")]),
        "prod",
        Settled::Whole,
    )
    .expect_err("once both reads hold the survivor, they are compared directly");
    assert!(format!("{e:#}").contains("`note`"), "{e:#}");

    // A transactional result that still holds the source and an impostor
    // at the destination is not the promised surviving column. This is
    // not a supported multi-change staged checkpoint (ADR-0003).
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &changes,
        &table(&[
            ("code", "varchar(20)"),
            ("label", "nvarchar(50)"),
            ("note", "int"),
        ]),
        &table(&[
            ("code", "varchar(20)"),
            ("label", "nvarchar(50)"),
            ("note", "bigint"),
        ]),
        "prod",
        Settled::Whole,
    )
    .expect_err("a name put back before the rename claims it is not the survivor");
    assert!(format!("{e:#}").contains("`note`"), "{e:#}");
}

/// A touched table is exempt down to the columns and constraints the plan
/// moves, and no further — the same narrowing its rows got, one level up
/// (DECISIONS 166).
#[test]
fn a_touched_table_answers_for_the_shape_the_plan_leaves_alone() {
    let table = |note: &str, index: Option<&str>| {
        let mut t = pbps_model::Table::default();
        t.columns.insert(
            "note".into(),
            pbps_model::Column::new(note.parse().unwrap()),
        );
        t.columns.insert(
            "other".into(),
            pbps_model::Column::new("int".parse().unwrap()),
        );
        if let Some(name) = index {
            t.indexes.insert(
                name.into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column("note".into()),
                        descending: false,
                        opclass: None,
                        collation: None,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
        }
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), t);
        s
    };
    // The plan widens `note`, and nothing else.
    let widening = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::AlterColumnType {
                uid: "c_aaaaaa".parse().unwrap(),
                column: "dbo.t.note".parse().unwrap(),
                from: "nvarchar(50)".parse().unwrap(),
                to: "nvarchar(100)".parse().unwrap(),
                from_nullable: true,
                to_nullable: true,
                through_external_partitions: false,
                from_collation: None,
                to_collation: None,
                using: None,
            },
        )],
    };
    let before = table("nvarchar(50)", None);
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &widening,
        &before,
        &table("nvarchar(100)", None),
        "prod",
        Settled::Whole,
    )
    .expect("the column this plan retypes is its own business");

    // An index that arrived on the same table is not.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &widening,
        &before,
        &table("nvarchar(100)", Some("ix_rogue")),
        "prod",
        Settled::Whole,
    )
    .expect_err("an index nobody planned");
    assert!(format!("{e:#}").contains("ix_rogue"), "{e:#}");

    // And so is another column changing underneath it.
    let mut retyped_other = table("nvarchar(100)", None);
    retyped_other
        .tables
        .get_mut(&"dbo.t".parse::<TableName>().unwrap())
        .unwrap()
        .columns
        .insert(
            "other".into(),
            pbps_model::Column::new("bigint".parse().unwrap()),
        );
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &widening,
        &before,
        &retyped_other,
        "prod",
        Settled::Whole,
    )
    .expect_err("a column nobody planned");
    assert!(format!("{e:#}").contains("`other`"), "{e:#}");

    // A constraint dropped and recreated under the same name with a
    // different body is present on both sides; only its definition says
    // so (DECISIONS 167).
    let mut rebuilt = table("nvarchar(50)", Some("ix_note"));
    rebuilt
        .tables
        .get_mut(&"dbo.t".parse::<TableName>().unwrap())
        .unwrap()
        .indexes
        .get_mut("ix_note")
        .unwrap()
        .unique = true;
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &widening,
        &table("nvarchar(50)", Some("ix_note")),
        &rebuilt,
        "prod",
        Settled::Whole,
    )
    .expect_err("the index came back different");
    assert!(format!("{e:#}").contains("ix_note"), "{e:#}");

    // And the shape is compared at every read, not only the settled one:
    // a checkpoint that blessed a change became `previous`, after which
    // the final comparison measured the contaminated shape against itself.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &widening,
        &before,
        &table("nvarchar(50)", Some("ix_rogue")),
        "prod",
        Settled::SoFar,
    )
    .expect_err("mid-run is still a read");
    assert!(format!("{e:#}").contains("ix_rogue"), "{e:#}");

    // The plan's own index change is exempt, both ways round.
    let adding = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::AddIndex {
                table: "dbo.t".parse().unwrap(),
                name: "ix_note".into(),
                index: Box::new(pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column("note".into()),
                        descending: false,
                        opclass: None,
                        collation: None,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                }),
                clustered: false,
            },
        )],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &adding,
        &table("nvarchar(50)", None),
        &table("nvarchar(50)", Some("ix_note")),
        "prod",
        Settled::Whole,
    )
    .expect("the index this plan adds");

    // What the plan does to a table's parts is checked too. Nothing else
    // says so: the shape comparison excludes exactly these, and the table
    // check answers only for the table (DECISIONS 168).
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &adding,
        &table("nvarchar(50)", None),
        &table("nvarchar(50)", None),
        "prod",
        Settled::Whole,
    )
    .expect_err("the index this plan adds is not there");
    assert!(format!("{e:#}").contains("ix_note"), "{e:#}");

    // And an index and a check may share a name — separate namespaces to
    // the engine — so a planned change to one must not exempt the other.
    let mut with_check = table("nvarchar(50)", Some("ix_note"));
    let put_check = |s: &mut Schema, expression: &str| {
        s.tables
            .get_mut(&"dbo.t".parse::<TableName>().unwrap())
            .unwrap()
            .checks
            .insert(
                "ix_note".into(),
                pbps_model::CheckConstraint {
                    expression: expression.to_owned(),
                },
            );
    };
    put_check(&mut with_check, "note IS NOT NULL");
    let mut check_moved = table("nvarchar(50)", Some("ix_note"));
    put_check(&mut check_moved, "note <> N''");
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &adding,
        &with_check,
        &check_moved,
        "prod",
        Settled::Whole,
    )
    .expect_err("the check shares a name with the planned index, and is not it");
    assert!(format!("{e:#}").contains("check `ix_note`"), "{e:#}");
}
/// A row the plan writes is held to the cells it spelled, once every
/// statement has run. Its own statement stops speaking at its commit, and
/// a staged run leaves the row loose from then until the checkpoint read
/// (DECISIONS 165).
#[test]
fn a_row_this_plan_writes_holds_the_cells_it_spelled() {
    use pbps_model::{DataMode, RowKey, TableData, Value};
    let holding = |cells: &[(&str, &str)]| {
        let t = pbps_model::Table {
            data: Some(TableData {
                mode: DataMode::Exact,
                rows: [(
                    RowKey::from("k"),
                    cells
                        .iter()
                        .map(|(c, v)| ((*c).into(), Value::Text((*v).to_owned())))
                        .collect::<pbps_model::Row>(),
                )]
                .into_iter()
                .collect(),
            }),
            ..Default::default()
        };
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), t);
        s
    };
    let inserting = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::InsertRow {
                table: "dbo.t".parse().unwrap(),
                key_column: "code".into(),
                identity_key: false,
                key: RowKey::from("k"),
                row: [("note".into(), Value::Text("wrote".to_owned()))]
                    .into_iter()
                    .collect(),
                defaults: Default::default(),
                types: Default::default(),
            },
        )],
    };
    let empty = Schema::default();
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &inserting,
        &empty,
        &holding(&[("note", "wrote")]),
        "prod",
        Settled::Whole,
    )
    .expect("the row holds what the plan wrote");
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &inserting,
        &empty,
        &holding(&[("note", "rewritten")]),
        "prod",
        Settled::Whole,
    )
    .expect_err("something rewrote the cell after the statement committed");
    assert!(format!("{e:#}").contains("`note`"), "{e:#}");

    // A cell the read-back does not carry proves nothing: it is at its
    // column's default, which is where a spelled value equal to that
    // default also lands. Saying anything here would refuse valid applies.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &inserting,
        &empty,
        &holding(&[]),
        "prod",
        Settled::Whole,
    )
    .expect("an omitted cell is a cell at its default");
}

/// A module the plan drops leaves the managed set only once the plan has
/// run. Out of it from the first checkpoint, it was absent from the
/// checkpoint's schema and invisible to the `--resume` that scopes the
/// live side the same way — so the remaining `DROP` ran against an object
/// nobody had looked at since the plan was approved (DECISIONS 164).
#[test]
fn a_module_still_to_be_dropped_stays_in_the_staged_scope() {
    let module = |name: &str| {
        (
            name.parse::<pbps_model::ModuleId>().unwrap(),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: "SELECT 1".to_owned(),
                options: Default::default(),
            },
        )
    };
    let mut schema = Schema::default();
    schema.modules.extend([module("dbo.a"), module("dbo.b")]);
    let recorded = StateSnapshot::new(StateKind::Apply, schema, IdsFile::default(), "leon");
    let dropping = pbps_model::ChangeSet {
        changes: ["dbo.a", "dbo.b"]
            .into_iter()
            .map(|name| {
                pbps_model::PlannedChange::new(pbps_model::Change::DropModule {
                    id: name.parse().unwrap(),
                    kind: pbps_model::ModuleKind::View,
                })
            })
            .collect(),
    };
    // Once the plan has run they are gone, which is what the closing entry
    // records.
    assert!(
        modules_after(&recorded, &dropping, Settled::Whole).is_empty(),
        "the finished plan drops both"
    );
    // Until then both are still watched — including the one whose own
    // `DROP` has already run, which the read simply finds absent.
    let during = modules_after(&recorded, &dropping, Settled::SoFar);
    assert_eq!(during.len(), 2, "{during:?}");

    // And a module the plan creates is watched from the start either way.
    let creating = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateModule {
                id: "dbo.c".parse().unwrap(),
                module: Box::new(module("dbo.c").1),
            },
        )],
    };
    for settled in [Settled::Whole, Settled::SoFar] {
        assert!(
            modules_after(&recorded, &creating, settled)
                .contains(&"dbo.c".parse::<pbps_model::ModuleId>().unwrap()),
            "a module the plan adds is in the set from the start"
        );
    }
}

/// An object this plan creates has no baseline entry, so a loop over the
/// baseline never visits it — and everything that arrived on it while it
/// was new went unchecked (DECISIONS 163).
#[test]
fn what_this_plan_creates_is_compared_too() {
    use pbps_model::{DataMode, GrantTarget, Permission, RowKey, TableData};

    // A rogue row in a table this plan creates `exact`.
    let created = |keys: &[&str]| {
        let t = pbps_model::Table {
            data: Some(TableData {
                mode: DataMode::Exact,
                rows: keys
                    .iter()
                    .map(|k| (RowKey::from(*k), pbps_model::Row::default()))
                    .collect(),
            }),
            ..Default::default()
        };
        let mut s = Schema::default();
        s.tables.insert("dbo.new".parse().unwrap(), t);
        s
    };
    let creating = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.new".parse().unwrap(),
                table: Box::new(pbps_model::Table::default()),
                beside_default: None,
                moves_at_most: None,
            }),
            pbps_model::PlannedChange::new(pbps_model::Change::InsertRow {
                table: "dbo.new".parse().unwrap(),
                key_column: "code".into(),
                identity_key: false,
                key: RowKey::from("declared"),
                row: pbps_model::Row::default(),
                defaults: Default::default(),
                types: Default::default(),
            }),
        ],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating,
        &Schema::default(),
        &created(&["declared"]),
        "prod",
        Settled::Whole,
    )
    .expect("the row this plan inserts into the table it creates");
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating,
        &Schema::default(),
        &created(&["declared", "rogue"]),
        "prod",
        Settled::Whole,
    )
    .expect_err("a row nobody declared, in a table one statement old");
    assert!(format!("{e:#}").contains("row `rogue`"), "{e:#}");

    // And a grant on a role this plan creates.
    let with_grants = |held: &[Permission]| {
        let mut grants = BTreeMap::new();
        if !held.is_empty() {
            grants.insert(
                GrantTarget::Schema("dbo".to_owned()),
                held.iter().copied().collect::<BTreeSet<_>>(),
            );
        }
        let mut s = Schema::default();
        s.roles.insert(
            "app".to_owned().into(),
            pbps_model::Role {
                description: None,
                grants,
            },
        );
        s
    };
    let creating_role = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::CreateRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: "app".to_owned().into(),
            }),
            pbps_model::PlannedChange::new(pbps_model::Change::Grant {
                role: "app".to_owned().into(),
                target: GrantTarget::Schema("dbo".to_owned()),
                permissions: [Permission::Select].into_iter().collect(),
            }),
        ],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating_role,
        &Schema::default(),
        &with_grants(&[Permission::Select]),
        "prod",
        Settled::Whole,
    )
    .expect("the grant this plan gives the role it creates");
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating_role,
        &Schema::default(),
        &with_grants(&[Permission::Select, Permission::Delete]),
        "prod",
        Settled::Whole,
    )
    .expect_err("a permission nobody planned, on a role one statement old");
    assert!(format!("{e:#}").contains("role app"), "{e:#}");
}

/// Three things the guard could not see, each because of where it looked
/// rather than what it compared (DECISIONS 162).
#[test]
fn the_guard_looks_where_the_plan_reaches() {
    use pbps_model::{DataMode, GrantTarget, Permission, RowKey, TableData, Value};

    // 1. A target only the plan names. Reversed before the read, it is in
    //    neither side's grants and the loop never visited it.
    let role = |grants: BTreeMap<GrantTarget, BTreeSet<Permission>>| {
        let mut s = Schema::default();
        s.roles.insert(
            "app".to_owned().into(),
            pbps_model::Role {
                description: None,
                grants,
            },
        );
        s
    };
    let granting = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Grant {
            role: "app".to_owned().into(),
            target: GrantTarget::Schema("dbo".to_owned()),
            permissions: [Permission::Select].into_iter().collect(),
        })],
    };
    let none = role(BTreeMap::new());
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &granting,
        &none,
        &none,
        "prod",
        Settled::Whole,
    )
    .expect_err("the first permission on a target is still a permission");
    assert!(format!("{e:#}").contains("role app"), "{e:#}");

    // 2. A row the plan writes, gone before the read. Its statement's own
    //    postcondition stopped speaking at the commit.
    let table = |keys: &[&str]| {
        let t = pbps_model::Table {
            data: Some(TableData {
                mode: DataMode::Exact,
                rows: keys
                    .iter()
                    .map(|k| (RowKey::from(*k), pbps_model::Row::default()))
                    .collect(),
            }),
            ..Default::default()
        };
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), t);
        s
    };
    let inserting = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::InsertRow {
                table: "dbo.t".parse().unwrap(),
                key_column: "code".into(),
                identity_key: false,
                key: RowKey::from("new"),
                row: pbps_model::Row::default(),
                defaults: Default::default(),
                types: Default::default(),
            },
        )],
    };
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &inserting,
        &table(&[]),
        &table(&[]),
        "prod",
        Settled::Whole,
    )
    .expect_err("the row this plan inserts is not there");
    assert!(format!("{e:#}").contains("row `new`"), "{e:#}");
    // Mid-run it simply has not happened yet.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &inserting,
        &table(&[]),
        &table(&[]),
        "prod",
        Settled::SoFar,
    )
    .expect("the insert has not run yet");

    // 3. A column change that moves no reading leaves its cells compared.
    let with_note = |note: &str| {
        let t = pbps_model::Table {
            data: Some(TableData {
                mode: DataMode::Exact,
                rows: [(
                    RowKey::from("k"),
                    [("note".into(), Value::Text(note.to_owned()))]
                        .into_iter()
                        .collect::<pbps_model::Row>(),
                )]
                .into_iter()
                .collect(),
            }),
            ..Default::default()
        };
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), t);
        s
    };
    let tightening = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::AlterColumnNullability {
                uid: "c_aaaaaa".parse().unwrap(),
                column: "dbo.t.note".parse().unwrap(),
                ty: "nvarchar(50)".parse().unwrap(),
                to_nullable: false,
                through_external_partitions: false,
                collation: None,
            },
        )],
    };
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &tightening,
        &with_note("kept"),
        &with_note("rewritten"),
        "prod",
        Settled::Whole,
    )
    .expect_err("nullability rewrites no value, so the cell is still compared");
    assert!(format!("{e:#}").contains("row `k`"), "{e:#}");
}

/// A module that changes kind is a drop *and* a create for one name, and
/// the net of the two is what has to hold: checked separately, the drop
/// always failed because the create had put the module back, and every
/// replacement was refused (DECISIONS 161).
#[test]
fn a_module_replacement_is_judged_by_its_net_result() {
    let module = |kind: pbps_model::ModuleKind| pbps_model::Module {
        kind,
        description: None,
        definition: "SELECT 1".to_owned(),
        options: Default::default(),
    };
    let schema_with = |kind: pbps_model::ModuleKind| {
        let mut s = Schema::default();
        s.modules.insert("dbo.v".parse().unwrap(), module(kind));
        s
    };
    // `order_key` puts the drop first, which is the order a plan holds.
    let replacing = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::DropModule {
                id: "dbo.v".parse().unwrap(),
                kind: pbps_model::ModuleKind::View,
            }),
            pbps_model::PlannedChange::new(pbps_model::Change::CreateModule {
                id: "dbo.v".parse().unwrap(),
                module: Box::new(module(pbps_model::ModuleKind::Function)),
            }),
        ],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &replacing,
        &schema_with(pbps_model::ModuleKind::View),
        &schema_with(pbps_model::ModuleKind::Function),
        "prod",
        Settled::Whole,
    )
    .expect("a replacement is a drop and a create, and the create is the net");
}

/// A created table's computed columns answer for themselves: one another
/// session adds or removes after the `CREATE TABLE`, or one stored
/// differently from the declaration, is movement (#1174 review, SPEC
/// 7.6).
#[test]
fn a_created_tables_computed_columns_answer_for_themselves() {
    use pbps_model::{Change, Column, ComputedColumn, PlannedChange, Table};
    let name = TableName::new("dbo", "t");
    let computed = |persisted: bool| ComputedColumn {
        expression: "a * 2".into(),
        persisted,
        not_null: false,
    };
    let mut created = Table::default();
    created
        .columns
        .insert("a".into(), Column::new("int".parse().unwrap()));
    created.computed.insert("c".into(), computed(true));
    let changes = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: name.clone(),
            table: Box::new(created.clone()),
            beside_default: None,
            moves_at_most: None,
        })],
    };
    let read = |edit: &dyn Fn(&mut Table)| {
        let mut t = created.clone();
        edit(&mut t);
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let check = |after: &Schema| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &changes,
            &Schema::default(),
            after,
            "test",
            Settled::Whole,
        )
    };
    check(&read(&|_| {})).expect("the computed column this plan creates, as it creates it");
    for (what, edit) in [
        (
            "removed",
            &(|t: &mut Table| {
                t.computed.remove("c");
            }) as &dyn Fn(&mut Table),
        ),
        ("added", &|t: &mut Table| {
            t.computed.insert("d".into(), computed(false));
        }),
        ("not persisted", &|t: &mut Table| {
            t.computed.insert("c".into(), computed(false));
        }),
    ] {
        let e = check(&read(edit)).expect_err(what).to_string();
        assert!(e.contains("computed column"), "{what}: {e}");
    }
}

/// A created partition's own defaults and NOT NULLs answer for
/// themselves, as a column's do (#1578, SPEC 7.6): one another session
/// adds, drops or loosens after the `CREATE` is movement, and a default
/// the engine respelled is not.
#[test]
fn a_created_partitions_own_defaults_and_not_nulls_answer_for_themselves() {
    use pbps_model::{Change, PartitionColumn, PartitionOf, PlannedChange, Table};
    let name = TableName::new("app", "p");
    let own = |default: Option<&str>, not_null: bool| PartitionColumn {
        default: default.map(str::to_owned),
        not_null,
    };
    let created = Table {
        partition_of: Some(PartitionOf {
            parent: TableName::new("app", "ev"),
            bound: pbps_model::PartitionBound::Default,
            columns: [("v".into(), own(Some("'y'"), true))].into(),
        }),
        ..Table::default()
    };
    let changes = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: name.clone(),
            table: Box::new(created.clone()),
            beside_default: None,
            moves_at_most: None,
        })],
    };
    let read = |edit: &dyn Fn(
        &mut std::collections::BTreeMap<pbps_model::ColumnName, PartitionColumn>,
    )| {
        let mut t = created.clone();
        edit(&mut t.partition_of.as_mut().unwrap().columns);
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let check = |after: &Schema| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &changes,
            &Schema::default(),
            after,
            "test",
            Settled::Whole,
        )
    };
    check(&read(&|_| {})).expect("as this plan creates it");
    check(&read(&|c| {
        c.insert("v".into(), own(Some("'y'::text"), true));
    }))
    .expect("the engine's spelling of the same default");
    for (what, edit) in [
        (
            "dropped",
            &(|c: &mut std::collections::BTreeMap<pbps_model::ColumnName, PartitionColumn>| {
                c.clear();
            })
                as &dyn Fn(
                    &mut std::collections::BTreeMap<pbps_model::ColumnName, PartitionColumn>,
                ),
        ),
        ("added", &|c| {
            c.insert("w".into(), own(None, true));
        }),
        ("loosened", &|c| {
            c.insert("v".into(), own(Some("'y'"), false));
        }),
        ("default dropped", &|c| {
            c.insert("v".into(), own(None, true));
        }),
    ] {
        let e = check(&read(edit)).expect_err(what).to_string();
        assert!(
            e.contains("own column defaults or NOT NULLs"),
            "{what}: {e}"
        );
    }
}

/// A created partition's persistence and storage parameters answer for
/// themselves, as a table's do, from the first read that finds it
/// (#1580, SPEC 7.6): set in the `CREATE`, either changed by another
/// session is movement.
#[test]
fn a_created_partitions_persistence_and_storage_answer_for_themselves() {
    use pbps_model::{Change, PartitionOf, PlannedChange, Table};
    let name = TableName::new("app", "p");
    let created = Table {
        partition_of: Some(PartitionOf {
            parent: TableName::new("app", "ev"),
            bound: pbps_model::PartitionBound::Default,
            columns: Default::default(),
        }),
        unlogged: true,
        storage_parameters: [("fillfactor".to_owned(), "70".to_owned())].into(),
        ..Table::default()
    };
    let changes = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: name.clone(),
            table: Box::new(created.clone()),
            beside_default: None,
            moves_at_most: None,
        })],
    };
    let read = |edit: &dyn Fn(&mut Table)| {
        let mut t = created.clone();
        edit(&mut t);
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    for settled in [Settled::Whole, Settled::SoFar] {
        let check = |after: &Schema| {
            refuse_unplanned_movement(
                &pbps_pg::Postgres::new(),
                &changes,
                &Schema::default(),
                after,
                "test",
                settled,
            )
        };
        check(&read(&|_| {})).expect("as this plan creates it");
        for (what, edit, expected) in [
            (
                "logged",
                &(|t: &mut Table| t.unlogged = false) as &dyn Fn(&mut Table),
                "is not unlogged",
            ),
            (
                "reset",
                &|t: &mut Table| t.storage_parameters.clear(),
                "storage parameters are not",
            ),
            (
                "changed",
                &|t: &mut Table| {
                    t.storage_parameters
                        .insert("fillfactor".into(), "80".into());
                },
                "storage parameters are not",
            ),
        ] {
            let e = check(&read(edit)).expect_err(what).to_string();
            assert!(e.contains(expected), "{what}: {e}");
        }
    }
}

/// An index split out of a created table's payload (#1027) answers for
/// its whole structure, not only its name: another session's same-named
/// index of another shape is movement, at every read (#1056 review).
#[test]
fn a_created_tables_split_index_answers_for_its_structure() {
    use pbps_model::{Change, Column, Index, IndexColumn, PlannedChange, Table};
    let name = TableName::new("app", "t");
    let mut created = Table::default();
    for column in ["id", "other"] {
        created
            .columns
            .insert(column.into(), Column::new("int".parse().unwrap()));
    }
    let index = |column: &str, unique: bool, filter: Option<&str>| Index {
        columns: vec![IndexColumn {
            key: pbps_model::IndexKey::Column(column.into()),
            descending: false,
            opclass: None,
            collation: None,
        }],
        include: vec![],
        unique,
        filter: filter.map(Into::into),
        method: Default::default(),
        storage_parameters: Default::default(),
    };
    let planned_index = index("id", false, Some("id > 0"));
    let changes = pbps_model::ChangeSet {
        changes: vec![
            PlannedChange::new(Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: name.clone(),
                table: Box::new(created.clone()),
                beside_default: None,
                moves_at_most: None,
            }),
            PlannedChange::new(Change::AddIndex {
                table: name.clone(),
                name: "ix".into(),
                index: Box::new(planned_index.clone()),
                clustered: false,
            }),
        ],
    };
    let with_index = |ix: Index| {
        let mut t = created.clone();
        t.indexes.insert("ix".into(), ix);
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let mut carried = changes.clone();
    carried.changes.truncate(1);
    let Change::CreateTable { table, .. } = &mut carried.changes[0].change else {
        unreachable!()
    };
    table.indexes.insert("ix".into(), planned_index.clone());
    // A separate AddIndex is transactional; a CREATE payload can emit
    // its index in a later statement of the same staged logical change.
    for (changes, settled) in [(&carried, Settled::SoFar), (&changes, Settled::Whole)] {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            changes,
            &Schema::default(),
            &with_index(planned_index.clone()),
            "test",
            settled,
        )
        .expect("the index this plan adds, as it adds it");
        for other in [
            index("other", false, Some("id > 0")),
            index("id", true, Some("id > 0")),
            index("id", false, None),
        ] {
            let e = refuse_unplanned_movement(
                &pbps_mssql::Mssql,
                changes,
                &Schema::default(),
                &with_index(other.clone()),
                "test",
                settled,
            )
            .expect_err("a same-named index of another shape is not this plan's");
            assert!(format!("{e:#}").contains("index `ix`"), "{other:?}: {e:#}");
        }
    }
}

/// An index the plan adds, and a created table's key, are held to their
/// declared storage parameters too: another session's `ALTER INDEX …
/// SET` after the creation is movement, not the plan's result (#1483
/// review).
#[test]
fn a_created_index_is_held_to_its_declared_parameters() {
    use pbps_model::{Change, Column, Index, IndexColumn, PlannedChange, PrimaryKey, Table};
    let name = TableName::new("app", "t");
    let fill =
        |v: &str| -> BTreeMap<String, String> { [("fillfactor".to_owned(), v.to_owned())].into() };
    let index = |v: &str| Index {
        columns: vec![IndexColumn {
            key: pbps_model::IndexKey::Column("id".into()),
            descending: false,
            opclass: None,
            collation: None,
        }],
        include: Vec::new(),
        unique: false,
        filter: None,
        method: Default::default(),
        storage_parameters: fill(v),
    };
    let table = |ix: Option<&str>, pk: &str| {
        let mut t = Table::default();
        t.columns.insert(
            "id".into(),
            Column::new("integer".parse().unwrap()).not_null(),
        );
        t.primary_key = Some(PrimaryKey {
            name: Some("t_pkey".into()),
            columns: vec!["id".into()],
            storage_parameters: fill(pk),
        });
        if let Some(v) = ix {
            t.indexes.insert("ix".into(), index(v));
        }
        t
    };
    let schema = |t: Table| Schema {
        tables: [(name.clone(), t)].into(),
        ..Default::default()
    };
    let adding = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::AddIndex {
            table: name.clone(),
            name: "ix".into(),
            index: Box::new(index("70")),
            clustered: false,
        })],
    };
    let check = |plan: &pbps_model::ChangeSet, before: &Schema, after: &Schema| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            plan,
            before,
            after,
            "test",
            Settled::Whole,
        )
    };
    let before = schema(table(None, "80"));
    check(&adding, &before, &schema(table(Some("70"), "80"))).expect("as added");
    let e = check(&adding, &before, &schema(table(Some("90"), "80"))).expect_err("set after");
    assert!(format!("{e:#}").contains("index `ix`"), "{e:#}");

    let creating = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::CreateTable {
            uid: "t_000000".parse().unwrap(),
            name: name.clone(),
            table: Box::new(table(None, "80")),
            beside_default: None,
            moves_at_most: None,
        })],
    };
    let empty = Schema::default();
    check(&creating, &empty, &schema(table(None, "80"))).expect("as created");
    let e = check(&creating, &empty, &schema(table(None, "90"))).expect_err("set after");
    assert!(format!("{e:#}").contains("primary key"), "{e:#}");
}

/// An index's parameters the plan sets in place are held once the run is
/// whole to the before-read with its change applied: its part is
/// excused, as an index change's is, and another session's value after
/// the plan's is movement (#1442).
#[test]
fn index_storage_parameters_the_plan_sets_are_held_at_the_close() {
    use pbps_model::{Change, Column, Index, IndexColumn, PlannedChange, Table};
    let name = TableName::new("app", "t");
    let schema = |fillfactor: &str| {
        let mut t = Table::default();
        t.columns.insert(
            "id".into(),
            Column::new("integer".parse().unwrap()).not_null(),
        );
        t.indexes.insert(
            "ix".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("id".into()),
                    descending: false,
                    opclass: None,
                    collation: None,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: [("fillfactor".to_owned(), fillfactor.to_owned())].into(),
            },
        );
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let setting = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::SetIndexStorageParameters {
            table: name.clone(),
            target: pbps_model::IndexPart::Index("ix".into()),
            method: Default::default(),
            set: [("fillfactor".to_owned(), "80".to_owned())].into(),
            reset: Default::default(),
        })],
    };
    let check = |after: &Schema, settled| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            &setting,
            &schema("70"),
            after,
            "test",
            settled,
        )
    };
    check(&schema("80"), Settled::Whole).expect("the plan's own");
    check(&schema("70"), Settled::SoFar).expect("not run yet");
    let e = check(&schema("90"), Settled::Whole).expect_err("another session's");
    assert!(
        format!("{e:#}").contains("index storage parameters"),
        "{e:#}"
    );
    // The whole index is held, not only its parameters: dropped, or
    // recreated under its name with another definition, after the plan's
    // `ALTER INDEX`, it is movement (#1483 review).
    let mut dropped = schema("80");
    dropped.tables.get_mut(&name).unwrap().indexes.clear();
    let e = check(&dropped, Settled::Whole).expect_err("dropped");
    assert!(format!("{e:#}").contains("`ix`"), "{e:#}");
    let mut redefined = schema("80");
    redefined
        .tables
        .get_mut(&name)
        .unwrap()
        .indexes
        .get_mut("ix")
        .unwrap()
        .columns[0]
        .descending = true;
    let e = check(&redefined, Settled::Whole).expect_err("redefined");
    assert!(format!("{e:#}").contains("`ix`"), "{e:#}");
    // At a staged checkpoint too: before the plan's statement or after
    // it, never another definition, so no checkpoint records one for
    // the next read to compare against (#1483 review).
    check(&schema("80"), Settled::SoFar).expect("after the statement");
    let e = check(&redefined, Settled::SoFar).expect_err("redefined mid-run");
    assert!(format!("{e:#}").contains("`ix`"), "{e:#}");
    let e = check(&dropped, Settled::SoFar).expect_err("dropped mid-run");
    assert!(format!("{e:#}").contains("`ix`"), "{e:#}");
    let e = check(&schema("90"), Settled::SoFar).expect_err("neither side mid-run");
    assert!(format!("{e:#}").contains("`ix`"), "{e:#}");
}

/// A standing partition's own defaults and NOT NULLs are held across an
/// apply (#1581, #1607 review): another session's change to them is
/// movement; the plan's own is held once the run is whole; mid-run a
/// column the plan sets may hold either, and another may not.
/// A table a change works on but does not drop or rename is held to being
/// there at the closing read: the shape comparison passes over every
/// table a change names, so a table whose index the plan drops, gone by
/// a DDL trigger with all its rows, was recorded as success (#1692
/// review).
#[test]
fn a_table_the_plan_alters_but_does_not_drop_has_to_stay() {
    use pbps_model::{Change, Column, Index, IndexColumn, IndexKey, PlannedChange, Table};
    let name = TableName::new("app", "t");
    let with_table = |indexed: bool| {
        let mut t = Table::default();
        t.columns
            .insert("v".into(), Column::new("integer".parse().unwrap()));
        if indexed {
            t.indexes.insert(
                "t_v".into(),
                Index {
                    columns: vec![IndexColumn {
                        key: IndexKey::Column("v".into()),
                        descending: false,
                        opclass: None,
                        collation: None,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
        }
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let plan = |changes: Vec<Change>| pbps_model::ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    };
    let check = |plan: &pbps_model::ChangeSet, after: &Schema, settled| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            plan,
            &with_table(true),
            after,
            "test",
            settled,
        )
    };
    let drop_index = plan(vec![Change::DropIndex {
        table: name.clone(),
        name: "t_v".into(),
    }]);
    let e = check(&drop_index, &Schema::default(), Settled::Whole).expect_err("the table is gone");
    assert!(
        format!("{e:#}").contains("app.t is gone, and no change of this plan drops it"),
        "{e:#}"
    );
    // At a staged checkpoint too: the next read's `before` is this one,
    // which would no longer hold the table (#1692 review).
    let e =
        check(&drop_index, &Schema::default(), Settled::SoFar).expect_err("gone at a checkpoint");
    assert!(
        format!("{e:#}").contains("app.t is gone, and no change of this plan drops it"),
        "{e:#}"
    );
    // Negative: the table is there without the index, at either read.
    check(&drop_index, &with_table(false), Settled::Whole).expect("as planned");
    check(&drop_index, &with_table(false), Settled::SoFar).expect("as planned so far");
    // Negative: a plan that drops the table answers for it as before.
    let drop_table = plan(vec![Change::DropTable {
        uid: "t_a1b2c3".parse().unwrap(),
        name: name.clone(),
        detach_from: None,
    }]);
    check(&drop_table, &Schema::default(), Settled::Whole).expect("dropped as planned");
    check(&drop_table, &Schema::default(), Settled::SoFar).expect("dropped so far");
}

#[test]
fn a_partitions_own_columns_set_by_someone_else_are_movement() {
    use pbps_model::{
        Change, Column, PartitionBound, PartitionBy, PartitionColumn, PartitionOf, PlannedChange,
        Table,
    };
    let parent = TableName::new("app", "ev");
    let name = TableName::new("app", "ev_1");
    // Each column's own default (by presence) and NOT NULL.
    let schema = |own: &[(&str, bool, bool)]| {
        let mut p = Table::default();
        for c in ["v", "w"] {
            p.columns
                .insert(c.into(), Column::new("integer".parse().unwrap()));
        }
        p.partition_by = Some(PartitionBy {
            external: false,
            columns: vec!["v".into()],
            strategy: Default::default(),
        });
        let t = Table {
            partition_of: Some(PartitionOf {
                parent: parent.clone(),
                bound: PartitionBound::Default,
                columns: own
                    .iter()
                    .map(|(c, default, not_null)| {
                        (
                            (*c).into(),
                            PartitionColumn {
                                default: default.then(|| "1".to_owned()),
                                not_null: *not_null,
                            },
                        )
                    })
                    .collect(),
            }),
            ..Table::default()
        };
        Schema {
            tables: [(parent.clone(), p), (name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let check = |plan: &pbps_model::ChangeSet, before: &Schema, after: &Schema, settled| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            plan,
            before,
            after,
            "test",
            settled,
        )
    };
    let plan = |changes: Vec<Change>| pbps_model::ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    };
    let none = schema(&[]);
    let nothing = plan(Vec::new());
    check(&nothing, &none, &none, Settled::Whole).expect("unchanged");
    // Untouched, the whole table answers for it.
    let e = check(
        &nothing,
        &none,
        &schema(&[("v", false, true)]),
        Settled::Whole,
    )
    .expect_err("tightened by another");
    assert!(format!("{e:#}").contains("app.ev_1"), "{e:#}");

    let uid: pbps_model::Uid = "t_000000".parse().unwrap();
    let tightening = plan(vec![Change::SetPartitionNotNull {
        uid: uid.clone(),
        table: name.clone(),
        column: "v".into(),
        not_null: true,
    }]);
    let tight = schema(&[("v", false, true)]);
    check(&tightening, &none, &tight, Settled::Whole).expect("the plan's own");
    check(&tightening, &none, &none, Settled::SoFar).expect("not run yet");
    let e = check(&tightening, &none, &none, Settled::Whole).expect_err("relaxed again");
    assert!(format!("{e:#}").contains("app.ev_1's own column"), "{e:#}");
    // Another column moved while the plan sets `v`.
    let e = check(
        &tightening,
        &none,
        &schema(&[("v", false, true), ("w", true, false)]),
        Settled::SoFar,
    )
    .expect_err("another column");
    assert!(format!("{e:#}").contains("app.ev_1's own column"), "{e:#}");

    let defaulting = plan(vec![Change::SetPartitionDefault {
        uid,
        table: name.clone(),
        parent: parent.clone(),
        column: "v".into(),
        from: None,
        to: Some("1".into()),
        fallback: None,
    }]);
    let own = schema(&[("v", true, false)]);
    check(&defaulting, &none, &own, Settled::Whole).expect("the plan's own");
    check(&defaulting, &none, &own, Settled::SoFar).expect("run so far");
    // Its NOT NULL set by another while the plan sets its default.
    let e = check(
        &defaulting,
        &none,
        &schema(&[("v", true, true)]),
        Settled::SoFar,
    )
    .expect_err("the other field of the same column");
    assert!(format!("{e:#}").contains("app.ev_1's own column"), "{e:#}");
    let e = check(&defaulting, &none, &none, Settled::Whole).expect_err("never set");
    assert!(format!("{e:#}").contains("app.ev_1's own column"), "{e:#}");
    // Attached again under another bound, or to another parent, while
    // the plan sets its default: its own columns are as planned, its
    // place in the tree is not (#1607 review).
    let moved_to = |edit: &dyn Fn(&mut PartitionOf)| {
        let mut s = own.clone();
        edit(
            s.tables
                .get_mut(&name)
                .unwrap()
                .partition_of
                .as_mut()
                .unwrap(),
        );
        s
    };
    for (what, after) in [
        (
            "wider bound",
            moved_to(&|of| {
                of.bound = PartitionBound::Range {
                    from: vec![pbps_model::BoundDatum::MinValue],
                    to: vec![pbps_model::BoundDatum::MaxValue],
                }
            }),
        ),
        (
            "another parent",
            moved_to(&|of| of.parent = TableName::new("app", "other")),
        ),
    ] {
        for settled in [Settled::SoFar, Settled::Whole] {
            let e = check(&defaulting, &none, &after, settled).expect_err(what);
            assert!(
                format!("{e:#}").contains("app.ev_1's partition parent, bound or key"),
                "{what}: {e:#}"
            );
        }
    }

    // The parent's own column changes reach the partition (#1687). A
    // rename of `w` to `w2` with the partition's own default set on it
    // in the same plan: both reads are brought back to `w`, the change's
    // column with them.
    let renamed = |own_default: bool| {
        let mut s = schema(&[("w", own_default, false)]);
        for t in s.tables.values_mut() {
            t.columns = std::mem::take(&mut t.columns)
                .into_iter()
                .map(|(c, col)| (if c == "w".into() { "w2".into() } else { c }, col))
                .collect();
            if let Some(of) = t.partition_of.as_mut() {
                of.columns = std::mem::take(&mut of.columns)
                    .into_iter()
                    .map(|(c, own)| (if c == "w".into() { "w2".into() } else { c }, own))
                    .collect();
            }
        }
        s
    };
    let renaming = plan(vec![
        Change::RenameColumn {
            uid: "c_000001".parse().unwrap(),
            table: parent.clone(),
            from: "w".into(),
            to: "w2".into(),
            table_was: None,
        },
        Change::SetPartitionDefault {
            uid: "t_000000".parse().unwrap(),
            table: name.clone(),
            parent: parent.clone(),
            column: "w2".into(),
            from: None,
            to: Some("1".into()),
            fallback: None,
        },
    ]);
    check(&renaming, &none, &renamed(true), Settled::Whole).expect("renamed and set");
    let e =
        check(&renaming, &none, &renamed(false), Settled::Whole).expect_err("renamed, never set");
    assert!(format!("{e:#}").contains("app.ev_1's own column"), "{e:#}");
    // The parent's `w` made NOT NULL leaves none of the partition's own
    // on it, though no change names the partition.
    let mut tightened_parent = none.clone();
    tightened_parent
        .tables
        .get_mut(&parent)
        .unwrap()
        .columns
        .get_mut("w")
        .unwrap()
        .nullable = false;
    let tightening_parent = plan(vec![Change::AlterColumnNullability {
        uid: "c_000001".parse().unwrap(),
        column: pbps_model::ColumnRef::new(parent.clone(), "w"),
        ty: "integer".parse().unwrap(),
        to_nullable: false,
        through_external_partitions: false,
        collation: None,
    }]);
    check(
        &tightening_parent,
        &schema(&[("w", false, true)]),
        &tightened_parent,
        Settled::Whole,
    )
    .expect("the parent's NOT NULL holds it");
    // `w` renamed to `w2` and a new `w` added: the name changes hands
    // (DEC-541.1), and the partition's own NOT NULL on the old `w` and
    // its own default on the new one are told apart under the names the
    // plan leaves (#1692 review).
    let handed = |old_not_null: bool| {
        let mut s = schema(&[("w2", false, old_not_null), ("w", true, false)]);
        s.tables
            .get_mut(&parent)
            .unwrap()
            .columns
            .insert("w2".into(), Column::new("integer".parse().unwrap()));
        s
    };
    let handing = plan(vec![
        Change::RenameColumn {
            uid: "c_000001".parse().unwrap(),
            table: parent.clone(),
            from: "w".into(),
            to: "w2".into(),
            table_was: None,
        },
        Change::AddColumn {
            uid: "c_000002".parse().unwrap(),
            table: parent.clone(),
            name: "w".into(),
            column: Box::new(Column::new("integer".parse().unwrap())),
        },
        Change::SetPartitionDefault {
            uid: "t_000000".parse().unwrap(),
            table: name.clone(),
            parent: parent.clone(),
            column: "w".into(),
            from: None,
            to: Some("1".into()),
            fallback: None,
        },
    ]);
    let was = schema(&[("w", false, true)]);
    check(&handing, &was, &handed(true), Settled::Whole).expect("each column its own");
    let e = check(&handing, &was, &handed(false), Settled::Whole)
        .expect_err("the old column's NOT NULL lost");
    assert!(format!("{e:#}").contains("app.ev_1's own column"), "{e:#}");
    // The same names changing hands with no change of the plan naming
    // the partition: an own default another session set on the new `w`
    // is movement, not folded into the old column's entry.
    let unnamed = plan(
        handing.changes[..2]
            .iter()
            .map(|p| p.change.clone())
            .collect(),
    );
    let mut kept = schema(&[("w2", false, true)]);
    kept.tables
        .insert(parent.clone(), handed(true).tables[&parent].clone());
    check(&unnamed, &was, &kept, Settled::Whole).expect("the old column's own kept");
    let e = check(&unnamed, &was, &handed(true), Settled::Whole)
        .expect_err("an own default set by another on the new column");
    assert!(format!("{e:#}").contains("app.ev_1's own column"), "{e:#}");
    // Held field by field for its parent's change alone, a partition is
    // still held to being there: dropped or arrived by another session.
    let mut gone = kept.clone();
    gone.tables.remove(&name);
    let e = check(&unnamed, &was, &gone, Settled::Whole).expect_err("dropped by another");
    assert!(format!("{e:#}").contains("app.ev_1 is gone"), "{e:#}");
    let mut arrived = was.clone();
    arrived.tables.remove(&name);
    let e = check(&unnamed, &arrived, &kept, Settled::Whole).expect_err("created by another");
    assert!(format!("{e:#}").contains("app.ev_1 is there"), "{e:#}");
    // `w` dropped and `x` renamed into its name, which no undo rename
    // can say (#1692 review): the partition's own entry on `x` is
    // compared with the one now on `w`.
    let with_x = |s: &mut Schema, column: &str| {
        let p = s.tables.get_mut(&parent).unwrap();
        p.columns.shift_remove("w");
        p.columns
            .insert(column.into(), Column::new("integer".parse().unwrap()));
    };
    let mut was_x = schema(&[("w", true, false), ("x", false, true)]);
    with_x(&mut was_x, "x");
    was_x
        .tables
        .get_mut(&parent)
        .unwrap()
        .columns
        .insert("w".into(), Column::new("integer".parse().unwrap()));
    let reused = |not_null: bool| {
        let mut s = schema(&[("w", false, not_null)]);
        with_x(&mut s, "w");
        s
    };
    let reusing = plan(vec![
        Change::DropColumn {
            uid: "c_000002".parse().unwrap(),
            column: pbps_model::ColumnRef::new(parent.clone(), "w"),
        },
        Change::RenameColumn {
            uid: "c_000003".parse().unwrap(),
            table: parent.clone(),
            from: "x".into(),
            to: "w".into(),
            table_was: None,
        },
    ]);
    check(&reusing, &was_x, &reused(true), Settled::Whole).expect("the renamed column's own");
    let e = check(&reusing, &was_x, &reused(false), Settled::Whole)
        .expect_err("the renamed column's own NOT NULL lost");
    assert!(format!("{e:#}").contains("app.ev_1's own column"), "{e:#}");
    // `w` dropped and the key column `v` renamed into its name: the key
    // is compared under the name the plan leaves (#1692 review).
    let keyed = |key: &str| {
        let mut s = schema(&[]);
        let p = s.tables.get_mut(&parent).unwrap();
        p.columns = [
            ("w".into(), Column::new("integer".parse().unwrap())),
            ("u".into(), Column::new("integer".parse().unwrap())),
        ]
        .into_iter()
        .collect();
        p.partition_by = Some(PartitionBy {
            external: false,
            columns: vec![key.into()],
            strategy: Default::default(),
        });
        s
    };
    let mut was_u = schema(&[]);
    was_u
        .tables
        .get_mut(&parent)
        .unwrap()
        .columns
        .insert("u".into(), Column::new("integer".parse().unwrap()));
    let rekeying = plan(vec![
        Change::DropColumn {
            uid: "c_000002".parse().unwrap(),
            column: pbps_model::ColumnRef::new(parent.clone(), "w"),
        },
        Change::RenameColumn {
            uid: "c_000003".parse().unwrap(),
            table: parent.clone(),
            from: "v".into(),
            to: "w".into(),
            table_was: None,
        },
    ]);
    check(&rekeying, &was_u, &keyed("w"), Settled::Whole).expect("the key renamed");
    let e = check(&rekeying, &was_u, &keyed("u"), Settled::Whole)
        .expect_err("the key moved to another column");
    assert!(
        format!("{e:#}").contains("app.ev's partition parent, bound or key"),
        "{e:#}"
    );
}

/// A table's persistence is held across an apply (#1443): another
/// session's switch on a touched table is movement; the plan's own is
/// held once the run is whole; a created table is held to its `CREATE`.
#[test]
fn a_persistence_switch_by_someone_else_is_movement() {
    use pbps_model::{Change, Column, PlannedChange, Table};
    let name = TableName::new("app", "t");
    let schema = |unlogged: bool| {
        let mut t = Table::default();
        t.columns.insert(
            "id".into(),
            Column::new("integer".parse().unwrap()).not_null(),
        );
        t.unlogged = unlogged;
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let check = |plan: &pbps_model::ChangeSet, before: &Schema, after: &Schema, settled| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            plan,
            before,
            after,
            "test",
            settled,
        )
    };
    let nothing = pbps_model::ChangeSet {
        changes: Vec::new(),
    };
    check(&nothing, &schema(false), &schema(false), Settled::Whole).expect("unchanged");
    let e = check(&nothing, &schema(false), &schema(true), Settled::Whole)
        .expect_err("switched by another");
    assert!(format!("{e:#}").contains("app.t"), "{e:#}");
    let switching = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::SetTablePersistence {
            uid: "t_000000".parse().unwrap(),
            table: name.clone(),
            unlogged: true,
        })],
    };
    check(&switching, &schema(false), &schema(true), Settled::Whole).expect("the plan's own");
    check(&switching, &schema(false), &schema(false), Settled::SoFar).expect("not run yet");
    let e =
        check(&switching, &schema(false), &schema(false), Settled::Whole).expect_err("never run");
    assert!(format!("{e:#}").contains("logged"), "{e:#}");
    let mut declared = schema(true).tables.remove(&name).unwrap();
    declared.unlogged = true;
    let creating = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::CreateTable {
            uid: "t_000000".parse().unwrap(),
            name: name.clone(),
            table: Box::new(declared),
            beside_default: None,
            moves_at_most: None,
        })],
    };
    let empty = Schema::default();
    check(&creating, &empty, &schema(true), Settled::SoFar).expect("as created");
    let e = check(&creating, &empty, &schema(false), Settled::SoFar).expect_err("not as created");
    assert!(format!("{e:#}").contains("unlogged"), "{e:#}");
}

/// A history's schema is a declared schema, whose spelling bootstrap
/// holds to the database's like any other (#1501 review); a period alone
/// has no history and adds none.
#[test]
fn a_history_schema_is_a_declared_schema() {
    let mut schema = Schema::default();
    let mut table = pbps_model::Table {
        system_time: Some(pbps_model::SystemTime {
            start: "vf".into(),
            end: "vt".into(),
            hidden: false,
            versioning: Some(pbps_model::SystemVersioning {
                history: TableName::new("hist", "t_history"),
                retention: None,
            }),
        }),
        ..Default::default()
    };
    schema
        .tables
        .insert(TableName::new("dbo", "t"), table.clone());
    let declared = schemas_declared(&schema);
    assert_eq!(declared.keys().collect::<Vec<_>>(), ["dbo", "hist"]);
    assert!(declared["hist"].contains("history table"), "{declared:?}");
    table.system_time.as_mut().unwrap().versioning = None;
    schema.tables.insert(TableName::new("dbo", "t"), table);
    assert_eq!(
        schemas_declared(&schema).keys().collect::<Vec<_>>(),
        ["dbo"]
    );
}

/// A table's period and versioning are held across an apply (#1176): a
/// change by another session to an untouched or touched table is
/// movement, and a created table is held to its `CREATE` from the first
/// read, so a retention changed after it commits is not recorded as the
/// plan's (#1501 review).
#[test]
fn a_system_time_change_by_someone_else_is_movement() {
    use pbps_model::{Change, Column, PlannedChange, Table};
    let name = TableName::new("dbo", "t");
    let schema = |retention: Option<&str>| {
        let mut t = Table::default();
        t.columns
            .insert("id".into(), Column::new("int".parse().unwrap()).not_null());
        t.system_time = Some(pbps_model::SystemTime {
            start: "vf".into(),
            end: "vt".into(),
            hidden: false,
            versioning: Some(pbps_model::SystemVersioning {
                history: TableName::new("dbo", "t_history"),
                retention: retention.map(|r| r.parse().unwrap()),
            }),
        });
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let check = |plan: &pbps_model::ChangeSet, before: &Schema, after: &Schema, settled| {
        refuse_unplanned_movement(&pbps_mssql::Mssql, plan, before, after, "test", settled)
    };
    let plan = |changes: Vec<Change>| pbps_model::ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    };
    let nothing = plan(Vec::new());
    check(&nothing, &schema(None), &schema(None), Settled::Whole).expect("unchanged");
    let e = check(
        &nothing,
        &schema(None),
        &schema(Some("1 day")),
        Settled::Whole,
    )
    .expect_err("changed by another, untouched");
    assert!(format!("{e:#}").contains("dbo.t"), "{e:#}");
    // Touched by the plan, which never sets it on a table that exists.
    let mut grown = schema(Some("1 day"));
    grown
        .tables
        .get_mut(&name)
        .unwrap()
        .columns
        .insert("note".into(), Column::new("int".parse().unwrap()));
    let adding = plan(vec![Change::AddColumn {
        table: name.clone(),
        name: "note".into(),
        column: Box::new(Column::new("int".parse().unwrap())),
        uid: "c_000000".parse().unwrap(),
    }]);
    let e = check(&adding, &schema(None), &grown, Settled::Whole)
        .expect_err("changed by another, touched");
    assert!(format!("{e:#}").contains("system versioning"), "{e:#}");
    // Created: held to the `CREATE` from the first read.
    let declared = schema(Some("6 months")).tables.remove(&name).unwrap();
    let creating = plan(vec![Change::CreateTable {
        uid: "t_000000".parse().unwrap(),
        name: name.clone(),
        table: Box::new(declared),
        beside_default: None,
        moves_at_most: None,
    }]);
    let empty = Schema::default();
    check(&creating, &empty, &schema(Some("6 months")), Settled::SoFar).expect("as created");
    let e = check(&creating, &empty, &schema(Some("1 day")), Settled::SoFar)
        .expect_err("not as created");
    assert!(format!("{e:#}").contains("system versioning"), "{e:#}");
}

/// Storage parameters are held across an apply (#1441): another
/// session's change on a touched table is movement; the plan's own is
/// held once the run is whole and either side of it is fine mid-run; a
/// created table is held to its `CREATE` from the first read.
#[test]
fn storage_parameters_moved_by_someone_else_are_movement() {
    use pbps_model::{Change, Column, PlannedChange, Table};
    let name = TableName::new("app", "t");
    let mut t = Table::default();
    t.columns.insert(
        "id".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    let schema = |pairs: &[(&str, &str)]| {
        let mut t = t.clone();
        t.storage_parameters = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let check = |plan: &pbps_model::ChangeSet, before: &Schema, after: &Schema, settled| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            plan,
            before,
            after,
            "test",
            settled,
        )
    };
    let unrelated = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::AddCheck {
            table: name.clone(),
            name: "ck".into(),
            constraint: pbps_model::CheckConstraint {
                expression: "id > 0".into(),
            },
        })],
    };
    let was = schema(&[("fillfactor", "70"), ("autovacuum_enabled", "false")]);
    check(&unrelated, &was, &was, Settled::SoFar).expect("unchanged");
    let e = check(
        &unrelated,
        &was,
        &schema(&[("fillfactor", "70")]),
        Settled::SoFar,
    )
    .expect_err("another session reset one");
    assert!(format!("{e:#}").contains("storage parameters"), "{e:#}");

    let setting = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::SetStorageParameters {
            uid: "t_000000".parse().unwrap(),
            table: name.clone(),
            set: [("fillfactor".to_owned(), "80".to_owned())].into(),
            reset: ["autovacuum_enabled".to_owned()].into(),
        })],
    };
    let after = schema(&[("fillfactor", "80")]);
    check(&setting, &was, &after, Settled::Whole).expect("the plan's own");
    check(&setting, &was, &was, Settled::SoFar).expect("not run yet");
    let e = check(&setting, &was, &was, Settled::Whole).expect_err("never run");
    assert!(format!("{e:#}").contains("storage parameters"), "{e:#}");
    let e = check(
        &setting,
        &was,
        &schema(&[("fillfactor", "90")]),
        Settled::SoFar,
    )
    .expect_err("neither side of the plan");
    assert!(format!("{e:#}").contains("storage parameters"), "{e:#}");

    let mut declared = t.clone();
    declared
        .storage_parameters
        .insert("fillfactor".into(), "80".into());
    let creating = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::CreateTable {
            uid: "t_000000".parse().unwrap(),
            name: name.clone(),
            table: Box::new(declared),
            beside_default: None,
            moves_at_most: None,
        })],
    };
    let empty = Schema::default();
    check(&creating, &empty, &after, Settled::SoFar).expect("as created");
    let e = check(&creating, &empty, &schema(&[]), Settled::SoFar).expect_err("not as created");
    assert!(format!("{e:#}").contains("storage parameters"), "{e:#}");
}

/// A touched table is held to its replica identity, which every part can
/// read back unchanged around (#1444); the plan's own setting of it, under
/// the name it runs by, is not movement. A created table is held to its
/// declared identity once it shows one, or once the run is whole.
#[test]
fn a_replica_identity_moved_by_someone_else_is_movement() {
    use pbps_model::{Change, Column, PlannedChange, ReplicaIdentity, Table};
    let name = TableName::new("app", "t");
    let mut t = Table::default();
    t.columns.insert(
        "id".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    let schema = |identity: Option<ReplicaIdentity>| {
        let mut t = t.clone();
        t.replica_identity = identity;
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let unrelated = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::AddCheck {
            table: name.clone(),
            name: "ck".into(),
            constraint: pbps_model::CheckConstraint {
                expression: "id > 0".into(),
            },
        })],
    };
    let check = |plan: &pbps_model::ChangeSet, before: &Schema, after: &Schema, settled| {
        refuse_unplanned_movement(
            &pbps_pg::Postgres::new(),
            plan,
            before,
            after,
            "test",
            settled,
        )
    };
    check(&unrelated, &schema(None), &schema(None), Settled::SoFar).expect("unchanged");
    let e = check(
        &unrelated,
        &schema(None),
        &schema(Some(ReplicaIdentity::Full)),
        Settled::SoFar,
    )
    .expect_err("another session set FULL");
    assert!(
        format!("{e:#}").contains("replica identity changed"),
        "{e:#}"
    );
    let setting = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::SetReplicaIdentity {
            uid: "t_000000".parse().unwrap(),
            table: name.clone(),
            to: Some(ReplicaIdentity::Full),
        })],
    };
    check(
        &setting,
        &schema(None),
        &schema(Some(ReplicaIdentity::Full)),
        Settled::SoFar,
    )
    .expect("the plan's own setting");
    // Once the run is whole it must be the plan's value: another session's
    // after the plan's is movement, though any value mid-run is the plan in
    // progress (#1467 review).
    check(
        &setting,
        &schema(None),
        &schema(Some(ReplicaIdentity::Full)),
        Settled::Whole,
    )
    .expect("the plan's value at the close");
    check(
        &setting,
        &schema(None),
        &schema(Some(ReplicaIdentity::Nothing)),
        Settled::SoFar,
    )
    .expect("mid-run");
    for closing in [Settled::Whole, Settled::Closing] {
        let e = check(
            &setting,
            &schema(None),
            &schema(Some(ReplicaIdentity::Nothing)),
            closing,
        )
        .expect_err("another session's value after the plan's");
        assert!(
            format!("{e:#}").contains("not the one this plan sets"),
            "{e:#}"
        );
    }

    // Created: the default mid-run is the plan in progress; another
    // identity, or the default once the run is whole, is not.
    let mut declared = t.clone();
    declared.replica_identity = Some(ReplicaIdentity::Full);
    let creating = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::CreateTable {
            uid: "t_000000".parse().unwrap(),
            name: name.clone(),
            table: Box::new(declared),
            beside_default: None,
            moves_at_most: None,
        })],
    };
    let empty = Schema::default();
    check(&creating, &empty, &schema(None), Settled::SoFar).expect("not set yet");
    check(
        &creating,
        &empty,
        &schema(Some(ReplicaIdentity::Full)),
        Settled::Whole,
    )
    .expect("as declared");
    let e = check(&creating, &empty, &schema(None), Settled::Whole).expect_err("never set");
    assert!(
        format!("{e:#}").contains("replica identity is not the one"),
        "{e:#}"
    );
    let e = check(
        &creating,
        &empty,
        &schema(Some(ReplicaIdentity::Nothing)),
        Settled::SoFar,
    )
    .expect_err("set otherwise");
    assert!(
        format!("{e:#}").contains("replica identity is not the one"),
        "{e:#}"
    );
    // The resolver's split: a `CREATE` without the identity, set by a
    // later change of the same uid, which is the one it ends with (#1467
    // review).
    let mut bare = t.clone();
    bare.replica_identity = None;
    let split = pbps_model::ChangeSet {
        changes: vec![
            PlannedChange::new(Change::CreateTable {
                uid: "t_000000".parse().unwrap(),
                name: name.clone(),
                table: Box::new(bare),
                beside_default: None,
                moves_at_most: None,
            }),
            PlannedChange::new(Change::SetReplicaIdentity {
                uid: "t_000000".parse().unwrap(),
                table: name.clone(),
                to: Some(ReplicaIdentity::Full),
            }),
        ],
    };
    // Splitting the setting creates a multi-change transactional plan;
    // the single-CREATE midpoint policy is covered above.
    check(
        &split,
        &empty,
        &schema(Some(ReplicaIdentity::Full)),
        Settled::Whole,
    )
    .expect("the split setting");
    let e = check(&split, &empty, &schema(None), Settled::Whole).expect_err("never set");
    assert!(
        format!("{e:#}").contains("replica identity is not the one"),
        "{e:#}"
    );

    // A name handoff: `a` becomes `b` while `b` becomes `c`. `b`'s early
    // FULL runs under the name `b`, and `a`'s late NOTHING under its new
    // name `b` too; each table is held to its own (#1467 review).
    let (a, b, c) = (
        TableName::new("app", "a"),
        TableName::new("app", "b"),
        TableName::new("app", "c"),
    );
    let named = |tables: &[(&TableName, Option<ReplicaIdentity>)]| Schema {
        tables: tables
            .iter()
            .map(|(n, identity)| {
                let mut t = t.clone();
                t.replica_identity = identity.clone();
                ((*n).clone(), t)
            })
            .collect(),
        ..Default::default()
    };
    let (one, two): (pbps_model::Uid, pbps_model::Uid) =
        ("t_000001".parse().unwrap(), "t_000002".parse().unwrap());
    let handoff = pbps_model::ChangeSet {
        changes: vec![
            PlannedChange::new(Change::SetReplicaIdentity {
                uid: two.clone(),
                table: b.clone(),
                to: Some(ReplicaIdentity::Full),
            }),
            PlannedChange::new(Change::RenameTable {
                uid: two,
                from: b.clone(),
                to: c.clone(),
                defaults: Vec::new(),
            }),
            PlannedChange::new(Change::RenameTable {
                uid: one.clone(),
                from: a.clone(),
                to: b.clone(),
                defaults: Vec::new(),
            }),
            PlannedChange::new(Change::SetReplicaIdentity {
                uid: one,
                table: b.clone(),
                to: Some(ReplicaIdentity::Nothing),
            }),
        ],
    };
    check(
        &handoff,
        &named(&[(&a, None), (&b, None)]),
        &named(&[
            (&b, Some(ReplicaIdentity::Nothing)),
            (&c, Some(ReplicaIdentity::Full)),
        ]),
        Settled::Whole,
    )
    .expect("each table holds its own identity");
    let e = check(
        &handoff,
        &named(&[(&a, None), (&b, None)]),
        &named(&[
            (&b, Some(ReplicaIdentity::Nothing)),
            (&c, Some(ReplicaIdentity::Nothing)),
        ]),
        Settled::Whole,
    )
    .expect_err("c holds b's planned FULL, not a's NOTHING");
    assert!(format!("{e:#}").contains("app.c replica identity"), "{e:#}");
}

/// An existing table the plan touches for something else is held to its
/// layout (#1178): its parts can all read back unchanged while another
/// session moves the clustered index between them. The plan's own move
/// of the clustered index is not movement.
#[test]
fn a_touched_tables_layout_moved_by_someone_else_is_movement() {
    use pbps_model::{Change, Clustered, Column, Index, IndexColumn, PlannedChange, Table};
    let name = TableName::new("app", "t");
    let mut t = Table::default();
    t.columns
        .insert("id".into(), Column::new("int".parse().unwrap()).not_null());
    t.primary_key = Some(pbps_model::PrimaryKey {
        name: Some("pk_t".into()),
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
    });
    let ix = Index {
        columns: vec![IndexColumn {
            key: pbps_model::IndexKey::Column("id".into()),
            descending: false,
            opclass: None,
            collation: None,
        }],
        include: vec![],
        unique: false,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
    };
    t.indexes.insert("ix".into(), ix.clone());
    let schema = |layout: Option<Clustered>| {
        let mut t = t.clone();
        t.clustered = layout;
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let unrelated = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::AddCheck {
            table: name.clone(),
            name: "ck".into(),
            constraint: pbps_model::CheckConstraint {
                expression: "id > 0".into(),
            },
        })],
    };
    let check = |plan: &pbps_model::ChangeSet, after: &Schema, settled| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            plan,
            &schema(None),
            after,
            "test",
            settled,
        )
    };
    // The control: nothing moved the layout.
    check(&unrelated, &schema(None), Settled::SoFar).expect("the layout as approved");
    let e = check(
        &unrelated,
        &schema(Some(Clustered::Index("ix".into()))),
        Settled::SoFar,
    )
    .expect_err("another session made ix the clustered index");
    assert!(format!("{e:#}").contains("clustered differently"), "{e:#}");
    let e = check(&unrelated, &schema(Some(Clustered::Heap)), Settled::SoFar)
        .expect_err("another session rebuilt the key nonclustered");
    assert!(format!("{e:#}").contains("clustered differently"), "{e:#}");
    // The plan's own move is not movement.
    let moving = pbps_model::ChangeSet {
        changes: vec![
            PlannedChange::new(Change::SetPrimaryKey {
                table: name.clone(),
                from: t.primary_key.clone(),
                to: None,
                nonclustered: false,
            }),
            PlannedChange::new(Change::DropIndex {
                table: name.clone(),
                name: "ix".into(),
            }),
            PlannedChange::new(Change::AddIndex {
                table: name.clone(),
                name: "ix".into(),
                index: Box::new(ix),
                clustered: true,
            }),
            PlannedChange::new(Change::SetPrimaryKey {
                table: name.clone(),
                from: None,
                to: t.primary_key.clone(),
                nonclustered: true,
            }),
        ],
    };
    check(
        &moving,
        &schema(Some(Clustered::Index("ix".into()))),
        Settled::Whole,
    )
    .expect("the plan moved it itself");

    // A change to a key that holds the rows on neither side moves
    // nothing: with the key nonclustered beside a clustered `ix`, a
    // second session moving the rows to `ix2` is still movement.
    let mut t2 = t.clone();
    t2.indexes.insert("ix2".into(), t.indexes["ix"].clone());
    let beside = |layout: &str| {
        let mut t = t2.clone();
        t.clustered = Some(Clustered::Index(layout.into()));
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    // One SetPrimaryKey emits its own DROP/ADD statements. Two separate
    // changes would not be an admitted staged plan.
    let rekeying = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::SetPrimaryKey {
            table: name.clone(),
            from: t.primary_key.clone(),
            to: t.primary_key.clone(),
            nonclustered: true,
        })],
    };
    let recheck = |after: &Schema| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &rekeying,
            &beside("ix"),
            after,
            "test",
            Settled::SoFar,
        )
    };
    recheck(&beside("ix")).expect("the nonclustered key replaced, as planned");
    let e = recheck(&beside("ix2")).expect_err("someone else moved the rows to ix2");
    assert!(format!("{e:#}").contains("clustered differently"), "{e:#}");

    // A plan that leaves the rows on no index, caught between its key's
    // drop and its add by a session that makes `ix` the clustered one:
    // touching the old holder does not make `ix` the plan's. The heap it
    // does leave, and the moment between, are the plan's own.
    let to_heap = |after: &Schema| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &rekeying,
            &schema(None),
            after,
            "test",
            Settled::SoFar,
        )
    };
    to_heap(&schema(Some(Clustered::Heap))).expect("the heap this plan leaves");
    let mut keyless = schema(None);
    keyless.tables.get_mut(&name).unwrap().primary_key = None;
    to_heap(&keyless).expect("between the key's drop and its add");
    let e = to_heap(&schema(Some(Clustered::Index("ix".into()))))
        .expect_err("another session made ix clustered mid-plan");
    assert!(format!("{e:#}").contains("clustered differently"), "{e:#}");
}

/// A created table's layout is held with its parts (#1178): read back
/// clustered otherwise than its `CREATE TABLE` says, it is movement. A
/// staged checkpoint between the `CREATE TABLE` and the `ALTER` that adds
/// the clustered constraint finds a heap, which is the plan in progress.
#[test]
fn a_created_tables_layout_is_held_once_its_clustered_object_is_there() {
    use pbps_model::{Change, Clustered, Column, PlannedChange, PrimaryKey, Table};
    let name = TableName::new("app", "t");
    let mut created = Table::default();
    for column in ["id", "code"] {
        created.columns.insert(
            column.into(),
            Column::new("int".parse().unwrap()).not_null(),
        );
    }
    created.primary_key = Some(PrimaryKey {
        name: Some("pk_t".into()),
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
    });
    created.unique.insert(
        "uq_code".into(),
        pbps_model::UniqueConstraint {
            columns: vec!["code".into()],
            storage_parameters: Default::default(),
        },
    );
    created.clustered = Some(Clustered::Unique("uq_code".into()));
    let changes = pbps_model::ChangeSet {
        changes: vec![PlannedChange::new(Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: name.clone(),
            table: Box::new(created.clone()),
            beside_default: None,
            moves_at_most: None,
        })],
    };
    let read = |f: &dyn Fn(&mut Table)| {
        let mut t = created.clone();
        f(&mut t);
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let check = |after: &Schema, settled| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &changes,
            &Schema::default(),
            after,
            "test",
            settled,
        )
    };
    for settled in [Settled::SoFar, Settled::Whole] {
        check(&read(&|_| {}), settled).expect("clustered as declared");
        let e = check(&read(&|t| t.clustered = None), settled)
            .expect_err("the key came back clustered instead");
        assert!(
            format!("{e:#}").contains("is not clustered the way"),
            "{e:#}"
        );
    }
    // Midway, before the constraint exists: a heap is the plan in
    // progress, and only the whole plan is held to the layout.
    let midway = read(&|t| {
        t.unique.clear();
        t.clustered = Some(Clustered::Heap);
    });
    check(&midway, Settled::SoFar).expect("a checkpoint before the ALTER");
}

/// A created table's default set by a later change of the same plan
/// (#1027: split out to follow a rebuilt function) is what the column has
/// to hold once every statement has run. Before then, either state is
/// one the plan passes through. A default nobody planned still refuses.
#[test]
fn a_created_tables_default_set_later_by_the_plan_is_the_one_it_must_hold() {
    use pbps_model::{Change, Column, ColumnRef, PlannedChange, Table};
    let name = TableName::new("app", "t");
    let mut created = Table::default();
    created
        .columns
        .insert("v".into(), Column::new("int".parse().unwrap()));
    let with_default = |default: Option<&str>| {
        let mut t = created.clone();
        t.columns.get_mut("v").unwrap().value_source = default
            .map_or(pbps_model::ValueSource::None, |text| {
                pbps_model::ValueSource::Default(text.into())
            });
        Schema {
            tables: [(name.clone(), t)].into(),
            ..Default::default()
        }
    };
    let create = PlannedChange::new(Change::CreateTable {
        uid: "t_aaaaaa".parse().unwrap(),
        name: name.clone(),
        table: Box::new(created.clone()),
        beside_default: None,
        moves_at_most: None,
    });
    let set_default = PlannedChange::new(Change::AlterColumnDefault {
        uid: "c_bbbbbb".parse().unwrap(),
        column: ColumnRef::new(name.clone(), "v"),
        from: None,
        to: Some("1".into()),
    });
    let planned = pbps_model::ChangeSet {
        changes: vec![create.clone(), set_default],
    };
    let check = |changes: &pbps_model::ChangeSet, after: &Schema, settled| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            changes,
            &Schema::default(),
            after,
            "test",
            settled,
        )
    };
    check(&planned, &with_default(Some("1")), Settled::Whole)
        .expect("the default the plan sets is there");
    let e = check(&planned, &with_default(None), Settled::Whole)
        .expect_err("every statement ran, and the planned default is missing");
    assert!(format!("{e:#}").contains("column `v`"), "{e:#}");
    // The split CREATE/default plan above is transactional. The default
    // change alone admits a checkpoint over an already-existing table.
    let setting = pbps_model::ChangeSet {
        changes: vec![planned.changes[1].clone()],
    };
    for default in [None, Some("1")] {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &setting,
            &with_default(None),
            &with_default(default),
            "test",
            Settled::SoFar,
        )
        .expect("a single default change can be before or after its statement");
    }
    let unplanned = pbps_model::ChangeSet {
        changes: vec![create],
    };
    for settled in [Settled::SoFar, Settled::Whole] {
        check(&unplanned, &with_default(Some("1")), settled)
            .expect_err("a default no change of this plan sets is movement");
    }
}

#[test]
fn a_created_tables_pending_parts_are_added_but_recorded_parts_still_compare() {
    use pbps_model::{CheckConstraint, ForeignKey, Index, IndexColumn, Table, UniqueConstraint};
    let name = TableName::new("app", "t");
    let mut bare = Table::default();
    for column in ["id", "other"] {
        bare.columns.insert(
            column.into(),
            pbps_model::Column::new("int".parse().unwrap()),
        );
    }
    let mut declared = bare.clone();
    declared.unique.insert(
        "uq".into(),
        UniqueConstraint {
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        },
    );
    declared.foreign_keys.insert(
        "fk".into(),
        ForeignKey {
            columns: vec!["id".into()],
            references_table: name.clone(),
            references_columns: vec!["id".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    declared.checks.insert(
        "ck".into(),
        CheckConstraint {
            expression: "id > 0".into(),
        },
    );
    declared.indexes.insert(
        "ix".into(),
        Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column("id".into()),
                descending: false,
                opclass: None,
                collation: None,
            }],
            include: vec![],
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        },
    );
    let changes = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: name.clone(),
                table: Box::new(declared.clone()),
                beside_default: None,
                moves_at_most: None,
            },
        )],
    };
    let schema = |table| Schema {
        tables: [(name.clone(), table)].into(),
        ..Default::default()
    };
    let before = schema(bare);
    // The stored CHECK text is deliberately not the declaration's text.
    let mut recorded = declared;
    recorded.checks.get_mut("ck").unwrap().expression = "([id]>(0))".into();
    let after = schema(recorded.clone());
    for settled in [Settled::SoFar, Settled::Whole] {
        staged_comparison(
            &pbps_mssql::Mssql,
            &changes,
            &before,
            &after,
            "test",
            settled,
        )
        .expect("the CREATE payload's parts are emitted after the table");
    }
    staged_comparison(
        &pbps_mssql::Mssql,
        &changes,
        &after,
        &after,
        "test",
        Settled::Closing,
    )
    .expect("unchanged recorded parts still close");
    staged_comparison(
        &pbps_mssql::Mssql,
        &changes,
        &before,
        &after,
        "test",
        Settled::Closing,
    )
    .expect_err("no statement runs between the final checkpoint and the closing read");
    for part in [
        "unique",
        "foreign key",
        "check",
        "index",
        "removed",
        "extra",
    ] {
        let mut changed = recorded.clone();
        match part {
            "unique" => changed.unique.get_mut("uq").unwrap().columns = vec!["other".into()],
            "foreign key" => {
                changed
                    .foreign_keys
                    .get_mut("fk")
                    .unwrap()
                    .references_columns = vec!["other".into()]
            }
            "check" => changed.checks.get_mut("ck").unwrap().expression = "([id]>(1))".into(),
            "index" => {
                changed.indexes.get_mut("ix").unwrap().columns[0].key =
                    pbps_model::IndexKey::Column("other".into())
            }
            "removed" => {
                changed.checks.remove("ck");
            }
            "extra" => {
                changed.checks.insert(
                    "rogue".into(),
                    CheckConstraint {
                        expression: "id > 0".into(),
                    },
                );
            }
            _ => unreachable!(),
        }
        let changed = schema(changed);
        if matches!(part, "unique" | "foreign key" | "index") {
            for settled in [Settled::SoFar, Settled::Whole, Settled::Closing] {
                staged_comparison(
                    &pbps_mssql::Mssql,
                    &changes,
                    &before,
                    &changed,
                    "test",
                    settled,
                )
                .expect_err("the first observed part must match the CREATE payload");
                staged_comparison(
                    &pbps_mssql::Mssql,
                    &changes,
                    &changed,
                    &changed,
                    "test",
                    settled,
                )
                .expect_err("a bad checkpoint cannot certify its own structure");
            }
        }
        for settled in [Settled::SoFar, Settled::Whole, Settled::Closing] {
            staged_comparison(
                &pbps_mssql::Mssql,
                &changes,
                &after,
                &changed,
                "test",
                settled,
            )
            .expect_err("a recorded part or a name outside the CREATE payload changed");
        }
    }
}

/// A table this plan creates has no `before` entry, so the shape
/// comparison never ran for one — and `CreateTable` names no column and
/// no part of its own, so the final checks answered only for the table's
/// existence. A DDL trigger, or another session between a staged
/// `CREATE TABLE` and its checkpoint, could add a column or an index to
/// it and have that recorded as this plan's own result (DECISIONS 181).
///
/// By name here, and by value in the fields the catalog reads back
/// unchanged (185, 186); a created column's type is compared normalized
/// because the engine fills in its defaulted arguments.
#[test]
fn a_table_this_plan_creates_answers_for_the_shape_it_was_given() {
    let declared = || {
        let mut t = pbps_model::Table::default();
        t.columns
            .insert("id".into(), pbps_model::Column::new("int".parse().unwrap()));
        t
    };
    let creating = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.new".parse().unwrap(),
                table: Box::new(declared()),
                beside_default: None,
                moves_at_most: None,
            },
        )],
    };
    let after_with = |t: pbps_model::Table| {
        let mut s = Schema::default();
        s.tables.insert("dbo.new".parse().unwrap(), t);
        s
    };
    let before = Schema::default();

    // The engine's own spelling of a type is not movement: it fills in
    // the arguments a declaration omits, and `normalize_type` expands the
    // same ones, so the two meet (DECISIONS 186).
    let mut spelled = pbps_model::Table::default();
    spelled.columns.insert(
        "id".into(),
        pbps_model::Column::new("decimal(18,0)".parse().unwrap()),
    );
    let bare = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.new".parse().unwrap(),
                table: Box::new({
                    let mut t = pbps_model::Table::default();
                    t.columns.insert(
                        "id".into(),
                        pbps_model::Column::new("decimal".parse().unwrap()),
                    );
                    t
                }),
                beside_default: None,
                moves_at_most: None,
            },
        )],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &bare,
        &before,
        &after_with(spelled),
        "prod",
        Settled::Whole,
    )
    .expect("`decimal` and `decimal(18,0)` are one type");

    // A type that really changed is not that.
    let mut widened = declared();
    widened.columns.insert(
        "id".into(),
        pbps_model::Column::new("bigint".parse().unwrap()),
    );
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating,
        &before,
        &after_with(widened),
        "prod",
        Settled::Whole,
    )
    .expect_err("a type nobody planned");
    assert!(format!("{e:#}").contains("`id`"), "{e:#}");

    // A column nobody declared.
    let mut extra = declared();
    extra.columns.insert(
        "sneaky".into(),
        pbps_model::Column::new("int".parse().unwrap()),
    );
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating,
        &before,
        &after_with(extra),
        "prod",
        Settled::Whole,
    )
    .expect_err("a column nobody planned");
    assert!(format!("{e:#}").contains("sneaky"), "{e:#}");

    // An index nobody declared.
    let mut indexed = declared();
    indexed.indexes.insert(
        "ix_rogue".into(),
        pbps_model::Index {
            columns: vec![pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Column("id".into()),
                descending: false,
                opclass: None,
                collation: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        },
    );
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating,
        &before,
        &after_with(indexed),
        "prod",
        Settled::Whole,
    )
    .expect_err("an index nobody planned");
    assert!(format!("{e:#}").contains("ix_rogue"), "{e:#}");

    // A foreign key is *not* in the `CreateTable` payload: `diff_partial`
    // takes it out with `std::mem::take` and emits an `AddForeignKey` of
    // its own, because it sorts after every create. Read off the payload
    // alone the expectation is empty, and the key the plan itself adds
    // reads as movement — every created table with a foreign key refused
    // (DECISIONS 182).
    let fk = pbps_model::ForeignKey {
        columns: vec!["id".into()],
        references_table: "dbo.other".parse().unwrap(),
        references_columns: vec!["id".into()],
        on_delete: Default::default(),
        on_update: Default::default(),
    };
    let with_fk = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.new".parse().unwrap(),
                table: Box::new(declared()),
                beside_default: None,
                moves_at_most: None,
            }),
            pbps_model::PlannedChange::new(pbps_model::Change::AddForeignKey {
                table: "dbo.new".parse().unwrap(),
                name: "fk_new".into(),
                constraint: Box::new(fk.clone()),
            }),
        ],
    };
    let mut keyed = declared();
    keyed.foreign_keys.insert("fk_new".into(), fk);
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &with_fk,
        &before,
        &after_with(keyed),
        "prod",
        Settled::Whole,
    )
    .expect("the foreign key this plan adds to the table it creates");

    // Nor is a column just a name. Its catalog-stable fields are its own
    // promise: nullability, identity, and whether it has a default at all
    // (the default's *text* is rewritten by the engine, so only its
    // presence is comparable) (DECISIONS 185).
    let column = |f: &dyn Fn(&mut pbps_model::Column)| {
        let mut c = pbps_model::Column::new("int".parse().unwrap());
        f(&mut c);
        let mut t = pbps_model::Table::default();
        t.columns.insert("id".into(), c);
        t
    };
    let creates = |t: pbps_model::Table| pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.new".parse().unwrap(),
                table: Box::new(t),
                beside_default: None,
                moves_at_most: None,
            },
        )],
    };
    let plain = creates(column(&|_| {}));
    for tamper in [
        (&|c: &mut pbps_model::Column| c.nullable = false) as &dyn Fn(&mut pbps_model::Column),
        &|c: &mut pbps_model::Column| {
            c.value_source = pbps_model::ValueSource::Identity(pbps_model::Identity {
                seed: 1,
                increment: 1,
                options: Default::default(),
            })
        },
        &|c: &mut pbps_model::Column| {
            c.value_source = pbps_model::ValueSource::Default("((0))".to_owned())
        },
    ] {
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &plain,
            &before,
            &after_with(column(tamper)),
            "prod",
            Settled::Whole,
        )
        .expect_err("a column altered underneath the CREATE");
        assert!(format!("{e:#}").contains("`id`"), "{e:#}");
    }

    // A filtered index replaced by an unfiltered one covers different
    // rows. The predicate's text is the engine's to rewrite; whether
    // there is one at all is not.
    let filtered = |filter: Option<&str>| {
        let mut t = declared();
        t.indexes.insert(
            "ix_new".into(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Column("id".into()),
                    descending: false,
                    opclass: None,
                    collation: None,
                }],
                include: Vec::new(),
                unique: false,
                filter: filter.map(str::to_owned),
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        t
    };
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creates(filtered(Some("[id] > 0"))),
        &before,
        &after_with(filtered(None)),
        "prod",
        Settled::Whole,
    )
    .expect_err("a filter this plan declared, gone");
    assert!(format!("{e:#}").contains("ix_new"), "{e:#}");
    // But the engine's rewriting of the predicate is not movement.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creates(filtered(Some("[id] > 0"))),
        &before,
        &after_with(filtered(Some("([id]>(0))"))),
        "prod",
        Settled::Whole,
    )
    .expect("the engine rewrites a filter's text");

    // A part is not just a name. A primary key put back on different
    // columns, or under a different declared name, is `Some` on both
    // sides and the presence check accepted it (DECISIONS 183).
    let keyed = |name: Option<&str>, columns: &[&str]| {
        let mut t = declared();
        t.primary_key = Some(pbps_model::PrimaryKey {
            name: name.map(Into::into),
            columns: columns.iter().map(|c| (*c).into()).collect(),
            storage_parameters: Default::default(),
        });
        t
    };
    let plans = |t: pbps_model::Table| pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.new".parse().unwrap(),
                table: Box::new(t),
                beside_default: None,
                moves_at_most: None,
            },
        )],
    };
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &plans(keyed(Some("pk_new"), &["id"])),
        &before,
        &after_with(keyed(Some("pk_new"), &["other"])),
        "prod",
        Settled::Whole,
    )
    .expect_err("a primary key on columns nobody planned");
    assert!(format!("{e:#}").contains("primary key"), "{e:#}");
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &plans(keyed(Some("pk_new"), &["id"])),
        &before,
        &after_with(keyed(Some("pk_other"), &["id"])),
        "prod",
        Settled::Whole,
    )
    .expect_err("a primary key under a name nobody planned");
    assert!(format!("{e:#}").contains("primary key"), "{e:#}");

    // A declaration that leaves the naming to the database gets whatever
    // the engine generates, and that is not movement.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &plans(keyed(None, &["id"])),
        &before,
        &after_with(keyed(Some("PK__new__3213E83F"), &["id"])),
        "prod",
        Settled::Whole,
    )
    .expect("an unnamed key is named by the database");

    // A unique constraint is nothing but its columns, so the same holds.
    let uniqued = |columns: &[&str]| {
        let mut t = declared();
        t.unique.insert(
            "uq_new".into(),
            pbps_model::UniqueConstraint {
                columns: columns.iter().map(|c| (*c).into()).collect(),
                storage_parameters: Default::default(),
            },
        );
        t
    };
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &plans(uniqued(&["id"])),
        &before,
        &after_with(uniqued(&["other"])),
        "prod",
        Settled::Whole,
    )
    .expect_err("a unique constraint nobody planned");
    assert!(format!("{e:#}").contains("uq_new"), "{e:#}");

    // The foreign key's own definition, which lives on the
    // `AddForeignKey` change rather than in the payload (182): restoring
    // only its *name* left what it points at unchecked (DECISIONS 184).
    let fk_to = |columns: &[&str], on_delete| pbps_model::ForeignKey {
        columns: columns.iter().map(|c| (*c).into()).collect(),
        references_table: "dbo.other".parse().unwrap(),
        references_columns: vec!["id".into()],
        on_delete,
        on_update: Default::default(),
    };
    let adding = |fk: pbps_model::ForeignKey| pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.new".parse().unwrap(),
                table: Box::new(declared()),
                beside_default: None,
                moves_at_most: None,
            }),
            pbps_model::PlannedChange::new(pbps_model::Change::AddForeignKey {
                table: "dbo.new".parse().unwrap(),
                name: "fk_new".into(),
                constraint: Box::new(fk),
            }),
        ],
    };
    let read_back = |fk: pbps_model::ForeignKey| {
        let mut t = declared();
        t.foreign_keys.insert("fk_new".into(), fk);
        t
    };
    let planned = fk_to(&["id"], pbps_model::ReferentialAction::NoAction);

    // Different columns under the planned name.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &adding(planned.clone()),
        &before,
        &after_with(read_back(fk_to(
            &["other"],
            pbps_model::ReferentialAction::NoAction,
        ))),
        "prod",
        Settled::Whole,
    )
    .expect_err("a foreign key on columns nobody planned");
    assert!(format!("{e:#}").contains("fk_new"), "{e:#}");

    // Same columns, different referential action: the engine will delete
    // rows this plan never said it could.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &adding(planned.clone()),
        &before,
        &after_with(read_back(fk_to(
            &["id"],
            pbps_model::ReferentialAction::Cascade,
        ))),
        "prod",
        Settled::Whole,
    )
    .expect_err("a cascade nobody planned");
    assert!(format!("{e:#}").contains("fk_new"), "{e:#}");

    // And the one the plan actually asked for.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &adding(planned.clone()),
        &before,
        &after_with(read_back(planned)),
        "prod",
        Settled::Whole,
    )
    .expect("the foreign key this plan adds");

    // And one the CREATE asked for that is not there once it has run.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating,
        &before,
        &after_with(pbps_model::Table::default()),
        "prod",
        Settled::Whole,
    )
    .expect_err("the column the CREATE declared");
    assert!(format!("{e:#}").contains("`id`"), "{e:#}");
}

/// A cell the plan writes as an explicit NULL was dropped from the
/// expectation entirely, on the argument that the read-back omits a NULL
/// in a column with no default. It does — but the argument only excuses
/// *absence*. A value that arrived in its place is present, and nothing
/// looked (DECISIONS 179).
#[test]
fn a_cell_this_plan_writes_as_null_is_still_answered_for() {
    let schema_with = |cell: Option<pbps_model::Value>| {
        let mut row = pbps_model::Row::default();
        row.0
            .insert("code".into(), pbps_model::Value::Text("k".into()));
        if let Some(v) = cell {
            row.0.insert("note".into(), v);
        }
        let mut t = pbps_model::Table::default();
        t.columns.insert(
            "code".into(),
            pbps_model::Column::new("varchar(20)".parse().unwrap()),
        );
        t.columns.insert(
            "note".into(),
            pbps_model::Column::new("nvarchar(50)".parse().unwrap()),
        );
        t.data = Some(pbps_model::TableData {
            mode: pbps_model::DataMode::Exact,
            rows: [(pbps_model::RowKey::from("k"), row)].into_iter().collect(),
        });
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), t);
        s
    };
    let mut written = pbps_model::Row::default();
    written
        .0
        .insert("code".into(), pbps_model::Value::Text("k".into()));
    written.0.insert("note".into(), pbps_model::Value::Null);
    let inserting = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::InsertRow {
                table: "dbo.t".parse().unwrap(),
                key_column: "code".into(),
                identity_key: false,
                key: pbps_model::RowKey::from("k"),
                row: written,
                defaults: Default::default(),
                types: Default::default(),
            },
        )],
    };
    let mut before = schema_with(None);
    before
        .tables
        .get_mut(&"dbo.t".parse::<TableName>().unwrap())
        .unwrap()
        .data
        .as_mut()
        .unwrap()
        .rows
        .clear();

    // Absent: a NULL in a column with no default is omitted from the
    // read-back, so absence still proves nothing.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &inserting,
        &before,
        &schema_with(None),
        "prod",
        Settled::Whole,
    )
    .expect("an omitted cell is the NULL this plan wrote");

    // Present and NULL: a column *with* a default reads one back
    // explicitly, and that is the plan's own result too.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &inserting,
        &before,
        &schema_with(Some(pbps_model::Value::Null)),
        "prod",
        Settled::Whole,
    )
    .expect("an explicit NULL is the NULL this plan wrote");

    // Present and something else: somebody wrote it.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &inserting,
        &before,
        &schema_with(Some(pbps_model::Value::Text("rogue".into()))),
        "prod",
        Settled::Whole,
    )
    .expect_err("a value nobody planned");
    assert!(format!("{e:#}").contains("note"), "{e:#}");
}

/// The exclusion was per *column*, and its reason covers one *field*. A
/// plan that retypes a column could not be held to what its type became —
/// only the engine's stored form says that — but everything else about
/// that column still came back from two reads, and nothing compared them.
/// So a default, an identity or a nullability another session changed
/// while the plan ran was recorded as this plan's own result
/// (DECISIONS 173).
#[test]
fn a_column_this_plan_retypes_still_answers_for_its_other_fields() {
    let schema_with = |ty: &str, default: Option<&str>, nullable: bool| {
        let mut c = pbps_model::Column::new(ty.parse().unwrap());
        c.value_source = default.map_or(pbps_model::ValueSource::None, |text| {
            pbps_model::ValueSource::Default(text.into())
        });
        c.nullable = nullable;
        let mut t = pbps_model::Table::default();
        t.columns.insert("note".into(), c);
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), t);
        s
    };
    let widening = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::AlterColumnType {
                uid: "c_aaaaaa".parse().unwrap(),
                column: "dbo.t.note".parse().unwrap(),
                from: "nvarchar(50)".parse().unwrap(),
                to: "nvarchar(100)".parse().unwrap(),
                from_nullable: true,
                to_nullable: true,
                through_external_partitions: false,
                from_collation: None,
                to_collation: None,
                using: None,
            },
        )],
    };
    let before = schema_with("nvarchar(50)", None, true);

    // The type is the plan's own business: only the stored form says what
    // it became, which is why the field is excluded at all.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &widening,
        &before,
        &schema_with("nvarchar(100)", None, true),
        "prod",
        Settled::Whole,
    )
    .expect("the column this plan retypes");

    // A default that arrived beside it is not.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &widening,
        &before,
        &schema_with("nvarchar(100)", Some("N'x'"), true),
        "prod",
        Settled::Whole,
    )
    .expect_err("a default nobody planned");
    assert!(format!("{e:#}").contains("default"), "{e:#}");

    // Nor is a nullability this plan's own statement does not move: the
    // change carries `from_nullable == to_nullable`, so the restatement
    // leaves a value two reads still agree on.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &widening,
        &before,
        &schema_with("nvarchar(100)", None, false),
        "prod",
        Settled::Whole,
    )
    .expect_err("a nullability nobody planned");
    assert!(format!("{e:#}").contains("nullability"), "{e:#}");

    // Where the plan does move it, it is the plan's own business again.
    let tightening = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::AlterColumnType {
                uid: "c_aaaaaa".parse().unwrap(),
                column: "dbo.t.note".parse().unwrap(),
                from: "nvarchar(50)".parse().unwrap(),
                to: "nvarchar(100)".parse().unwrap(),
                from_nullable: true,
                to_nullable: false,
                through_external_partitions: false,
                from_collation: None,
                to_collation: None,
                using: None,
            },
        )],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &tightening,
        &before,
        &schema_with("nvarchar(100)", None, false),
        "prod",
        Settled::Whole,
    )
    .expect("a type change folds the nullability into itself");

    // And an identity nothing in this model can move is never excused.
    let mut with_identity = schema_with("nvarchar(100)", None, true);
    with_identity
        .tables
        .get_mut(&"dbo.t".parse::<TableName>().unwrap())
        .unwrap()
        .columns
        .get_mut("note")
        .unwrap()
        .value_source = pbps_model::ValueSource::Identity(pbps_model::Identity {
        seed: 1,
        increment: 1,
        options: Default::default(),
    });
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &widening,
        &before,
        &with_identity,
        "prod",
        Settled::Whole,
    )
    .expect_err("an identity nobody planned");
    assert!(format!("{e:#}").contains("identity"), "{e:#}");
}

/// `Change::columns` answers "whose *reading* did this move", which 162
/// narrowed for the row comparison; 166 then reused the same set to
/// exclude the plan's own shape edits. Nullability rewrites no cell and so
/// is rightly absent there, but the catalog reads `is_nullable` back — so
/// the shape comparison saw the plan's own `ALTER COLUMN ... NOT NULL` as
/// somebody else's work and refused every nullability-only plan
/// (DECISIONS 170).
#[test]
fn a_column_this_plan_makes_not_null_is_its_own_business() {
    let schema_with = |nullable: bool| {
        let mut t = pbps_model::Table::default();
        let mut c = pbps_model::Column::new("int".parse().unwrap());
        c.nullable = nullable;
        t.columns.insert("note".into(), c);
        t.columns.insert(
            "other".into(),
            pbps_model::Column::new("int".parse().unwrap()),
        );
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), t);
        s
    };
    let tightening = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::AlterColumnNullability {
                uid: "c_aaaaaa".parse().unwrap(),
                column: "dbo.t.note".parse().unwrap(),
                ty: "int".parse().unwrap(),
                to_nullable: false,
                through_external_partitions: false,
                collation: None,
            },
        )],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &tightening,
        &schema_with(true),
        &schema_with(false),
        "prod",
        Settled::Whole,
    )
    .expect("the column this plan makes NOT NULL is its own business");

    // Its own, and no wider: another column changing underneath it is
    // still movement.
    let mut other_moved = schema_with(false);
    let mut c = pbps_model::Column::new("int".parse().unwrap());
    c.nullable = false;
    other_moved
        .tables
        .get_mut(&"dbo.t".parse::<TableName>().unwrap())
        .unwrap()
        .columns
        .insert("other".into(), c);
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &tightening,
        &schema_with(true),
        &other_moved,
        "prod",
        Settled::Whole,
    )
    .expect_err("a column nobody planned");
    assert!(format!("{e:#}").contains("`other`"), "{e:#}");
}

/// The same for a table's parts, one field over. A constraint or an index
/// whose *definition* changes is a drop and an add under one name — the
/// differ's `by_name!` emits both — so holding both outcomes made the add
/// satisfy `Present` and the drop then fail `Absent`, refusing every
/// redefinition (DECISIONS 169).
#[test]
fn a_redefined_index_is_judged_by_its_net_result() {
    let index = |unique: bool| pbps_model::Index {
        columns: vec![pbps_model::IndexColumn {
            key: pbps_model::IndexKey::Column("note".into()),
            descending: false,
            opclass: None,
            collation: None,
        }],
        include: Vec::new(),
        unique,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
    };
    let schema_with = |unique: bool| {
        let mut t = pbps_model::Table::default();
        t.indexes.insert("ix_note".into(), index(unique));
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), t);
        s
    };
    // `order_key` puts the drop first, which is the order a plan holds.
    let redefining = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::DropIndex {
                table: "dbo.t".parse().unwrap(),
                name: "ix_note".into(),
            }),
            pbps_model::PlannedChange::new(pbps_model::Change::AddIndex {
                table: "dbo.t".parse().unwrap(),
                name: "ix_note".into(),
                index: Box::new(index(true)),
                clustered: false,
            }),
        ],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &redefining,
        &schema_with(false),
        &schema_with(true),
        "prod",
        Settled::Whole,
    )
    .expect("a redefinition is a drop and an add, and the add is the net");

    // Net, not blanket: a plan that only drops still has to see it gone.
    let dropping = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::DropIndex {
                table: "dbo.t".parse().unwrap(),
                name: "ix_note".into(),
            },
        )],
    };
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &dropping,
        &schema_with(false),
        &schema_with(false),
        "prod",
        Settled::Whole,
    )
    .expect_err("the drop did not take");
    assert!(format!("{e:#}").contains("ix_note"), "{e:#}");
}

#[test]
fn early_part_drops_follow_their_table_and_still_enforce_absence() {
    use pbps_model::{
        Change, ChangeSet, ForeignKey, Index, IndexColumn, PlannedChange, PrimaryKey, Table,
        UniqueConstraint,
    };
    for target in ["dbo.new", "moved.new"] {
        for kind in ["index", "unique", "foreign key", "primary key"] {
            let source: TableName = "dbo.old".parse().unwrap();
            let destination: TableName = target.parse().unwrap();
            let mut original = Table::default();
            let change = match kind {
                "index" => {
                    original.indexes.insert(
                        "gone".into(),
                        Index {
                            columns: vec![IndexColumn {
                                key: pbps_model::IndexKey::Column("id".into()),
                                descending: false,
                                opclass: None,
                                collation: None,
                            }],
                            include: vec![],
                            unique: false,
                            filter: None,
                            method: Default::default(),
                            storage_parameters: Default::default(),
                        },
                    );
                    Change::DropIndex {
                        table: source.clone(),
                        name: "gone".into(),
                    }
                }
                "unique" => {
                    original.unique.insert(
                        "gone".into(),
                        UniqueConstraint {
                            columns: vec!["id".into()],
                            storage_parameters: Default::default(),
                        },
                    );
                    Change::DropUnique {
                        table: source.clone(),
                        name: "gone".into(),
                    }
                }
                "foreign key" => {
                    original.foreign_keys.insert(
                        "gone".into(),
                        ForeignKey {
                            columns: vec!["id".into()],
                            references_table: "dbo.parent".parse().unwrap(),
                            references_columns: vec!["id".into()],
                            on_delete: Default::default(),
                            on_update: Default::default(),
                        },
                    );
                    Change::DropForeignKey {
                        table: source.clone(),
                        name: "gone".into(),
                    }
                }
                "primary key" => {
                    let key = PrimaryKey {
                        name: Some("gone".into()),
                        columns: vec!["id".into()],
                        storage_parameters: Default::default(),
                    };
                    original.primary_key = Some(key.clone());
                    Change::SetPrimaryKey {
                        table: source.clone(),
                        from: Some(key),
                        to: None,
                        nonclustered: false,
                    }
                }
                _ => unreachable!(),
            };
            let before = Schema {
                tables: [(source.clone(), original.clone())].into(),
                ..Schema::default()
            };
            let after = Schema {
                tables: [(destination.clone(), Table::default())].into(),
                ..Schema::default()
            };
            let changes = ChangeSet {
                changes: vec![
                    PlannedChange::new(change),
                    PlannedChange::new(Change::RenameTable {
                        uid: "t_aaaaaa".parse().unwrap(),
                        from: source,
                        to: destination.clone(),
                        defaults: Vec::new(),
                    }),
                ],
            };
            for settled in [Settled::Whole, Settled::Closing] {
                refuse_unplanned_movement(
                    &pbps_pg::Postgres::new(),
                    &changes,
                    &before,
                    &after,
                    "prod",
                    settled,
                )
                .unwrap();
                let recreated = Schema {
                    tables: [(destination.clone(), original.clone())].into(),
                    ..Schema::default()
                };
                let error = refuse_unplanned_movement(
                    &pbps_pg::Postgres::new(),
                    &changes,
                    &before,
                    &recreated,
                    "prod",
                    settled,
                )
                .unwrap_err();
                assert!(error.to_string().contains("still there"), "{kind}: {error}");
            }
            if kind == "index" {
                let mut rebuilding = changes.clone();
                rebuilding
                    .changes
                    .push(PlannedChange::new(Change::AddIndex {
                        table: destination.clone(),
                        name: "gone".into(),
                        index: Box::new(original.indexes["gone"].clone()),
                        clustered: false,
                    }));
                let mut restored = Schema {
                    tables: [(destination.clone(), original.clone())].into(),
                    ..Schema::default()
                };
                refuse_unplanned_movement(
                    &pbps_pg::Postgres::new(),
                    &rebuilding,
                    &before,
                    &restored,
                    "prod",
                    Settled::Whole,
                )
                .unwrap();
                restored
                    .tables
                    .get_mut(&destination)
                    .unwrap()
                    .indexes
                    .get_mut("gone")
                    .unwrap()
                    .unique = true;
                assert!(
                    refuse_unplanned_movement(
                        &pbps_pg::Postgres::new(),
                        &rebuilding,
                        &before,
                        &restored,
                        "prod",
                        Settled::Whole
                    )
                    .unwrap_err()
                    .to_string()
                    .contains("not the one")
                );
            }
            let mut changed = after.clone();
            changed.tables.get_mut(&destination).unwrap().unique.insert(
                "unplanned".into(),
                UniqueConstraint {
                    columns: vec!["id".into()],
                    storage_parameters: Default::default(),
                },
            );
            assert!(
                refuse_unplanned_movement(
                    &pbps_pg::Postgres::new(),
                    &changes,
                    &before,
                    &changed,
                    "prod",
                    Settled::Whole
                )
                .unwrap_err()
                .to_string()
                .contains("unplanned")
            );
        }
    }
}

/// A staged checkpoint cannot be asked what the plan achieved: most of it
/// has not run. Only movement is comparable there (DECISIONS 161).
#[test]
fn a_half_run_plan_is_compared_on_movement_alone() {
    let mut before = Schema::default();
    before
        .tables
        .insert("dbo.kept".parse().unwrap(), pbps_model::Table::default());
    let creating = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.new".parse().unwrap(),
                table: Box::new(pbps_model::Table::default()),
                beside_default: None,
                moves_at_most: None,
            },
        )],
    };
    // Mid-run the table is not there yet, and that is not a failure.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating,
        &before,
        &before,
        "prod",
        Settled::SoFar,
    )
    .expect("the statement has not run yet");
    // Once every statement has run it is one: `CREATE TABLE` reporting
    // success is not the table being there.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating,
        &before,
        &before,
        "prod",
        Settled::Whole,
    )
    .expect_err("the table this plan creates is not there");
    assert!(format!("{e:#}").contains("dbo.new"), "{e:#}");
    // And movement is compared either way.
    let mut moved = before.clone();
    moved
        .tables
        .remove(&"dbo.kept".parse::<TableName>().unwrap());
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating,
        &before,
        &moved,
        "prod",
        Settled::SoFar,
    )
    .expect_err("an untouched table went");
    assert!(format!("{e:#}").contains("dbo.kept"), "{e:#}");
}

/// And the wiring, not just the function: a staged run asks for movement
/// at every checkpoint and for the whole answer only at the last read.
///
/// Its own test because the one above calls `refuse_unplanned_movement`
/// directly — reverting `staged_movement`'s choice of mode left that one
/// green, which is a revert check that proves nothing (DECISIONS 161).
#[test]
fn a_staged_checkpoint_is_not_asked_what_the_plan_achieved() {
    let before = Schema::default();
    let creating = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.new".parse().unwrap(),
                table: Box::new(pbps_model::Table::default()),
                beside_default: None,
                moves_at_most: None,
            },
        )],
    };
    staged_movement(
        &pbps_mssql::Mssql,
        &creating,
        &before,
        &before,
        "prod",
        Some("prod"),
        StagedRead::Checkpoint {
            completed: 1,
            total: 2,
        },
    )
    .expect("statement 1 of 2: the create has not run yet");
    let e = staged_movement(
        &pbps_mssql::Mssql,
        &creating,
        &before,
        &before,
        "prod",
        Some("prod"),
        StagedRead::Checkpoint {
            completed: 2,
            total: 2,
        },
    )
    .expect_err("the last read is asked what the plan achieved");
    let e = format!("{e:#}");
    assert!(e.contains("dbo.new"), "{e}");
    // And the message says what a staged run cannot do about it.
    assert!(e.contains("nothing was rolled back"), "{e}");
}

/// A role with no grants has nothing but its name, so nothing else here
/// would notice it being dropped straight after it was created.
#[test]
fn a_role_this_plan_creates_has_to_be_there() {
    let empty = Schema::default();
    let creating = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: "app".to_owned().into(),
            },
        )],
    };
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating,
        &empty,
        &empty,
        "prod",
        Settled::Whole,
    )
    .expect_err("the role this plan creates is not there");
    assert!(format!("{e:#}").contains("role app"), "{e:#}");

    let mut made = Schema::default();
    made.roles.insert(
        "app".to_owned().into(),
        pbps_model::Role {
            description: None,
            grants: BTreeMap::new(),
        },
    );
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &creating,
        &empty,
        &made,
        "prod",
        Settled::Whole,
    )
    .expect("the plan's own role");

    // And a dropped role that came back.
    let dropping = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::DropRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: "app".to_owned().into(),
                members: Vec::new(),
            },
        )],
    };
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &dropping,
        &made,
        &made,
        "prod",
        Settled::Whole,
    )
    .expect_err("the role this plan drops is still there");
    assert!(format!("{e:#}").contains("role app"), "{e:#}");
}

/// A grant on a table this plan renames is the same grant afterwards, under
/// the name the table now has. Measured on SQL Server 2025: `sp_rename`
/// carries the permission to the new name, so nothing about the role
/// changed and the differ emits no grant change — leaving the role
/// untouched, and the comparison reading one grant as two (DECISIONS 157).
#[test]
fn a_grant_follows_the_table_this_plan_renames() {
    use pbps_model::{GrantTarget, Permission};
    let granted_on = |table: &str| {
        let mut grants = BTreeMap::new();
        grants.insert(
            GrantTarget::Object(table.parse().unwrap()),
            [Permission::Select].into_iter().collect::<BTreeSet<_>>(),
        );
        let mut s = Schema::default();
        s.tables
            .insert(table.parse().unwrap(), pbps_model::Table::default());
        s.roles.insert(
            "app".to_owned().into(),
            pbps_model::Role {
                description: None,
                grants,
            },
        );
        s
    };
    let rename = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::RenameTable {
                uid: "t_aaaaaa".parse().unwrap(),
                from: "dbo.old".parse().unwrap(),
                to: "dbo.new".parse().unwrap(),
                defaults: Vec::new(),
            },
        )],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &rename,
        &granted_on("dbo.old"),
        &granted_on("dbo.new"),
        "prod",
        Settled::Whole,
    )
    .expect("the grant moved with the table, which is not movement");

    // And the guard still bites underneath the forwarding: the same rename
    // with the grant actually gone is movement, not bookkeeping.
    let mut robbed = granted_on("dbo.new");
    robbed.roles.get_mut("app").unwrap().grants.clear();
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &rename,
        &granted_on("dbo.old"),
        &robbed,
        "prod",
        Settled::Whole,
    )
    .expect_err("the grant is gone");
    assert!(format!("{e:#}").contains("role app"), "{e:#}");
}

/// A column change re-shapes every row of its table without any row change
/// saying so, and none of that is somebody else's work (DECISIONS 158).
#[test]
fn a_column_this_plan_changes_is_not_compared_row_by_row() {
    use pbps_model::{Cell, DataMode, RowKey, TableData, Value};
    let table = |cells: &[(&str, &str)]| {
        let mut t = pbps_model::Table {
            data: Some(TableData {
                mode: DataMode::Exact,
                rows: [(
                    RowKey::from("k"),
                    cells
                        .iter()
                        .map(|(c, v)| ((*c).into(), Value::Text((*v).to_owned())))
                        .collect::<pbps_model::Row>(),
                )]
                .into_iter()
                .collect(),
            }),
            ..Default::default()
        };
        // The columns too: a rename is checked for having happened as
        // well as for re-keying the row (DECISIONS 168), and a table
        // whose rows name columns it does not have is not a state any
        // read produces.
        for (column, _) in cells {
            t.columns.insert(
                (*column).into(),
                pbps_model::Column::new("nvarchar(50)".parse().unwrap()),
            );
        }
        let mut s = Schema::default();
        s.tables.insert("dbo.t".parse().unwrap(), t);
        s
    };
    let renaming = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::RenameColumn {
                uid: "c_aaaaaa".parse().unwrap(),
                table: "dbo.t".parse().unwrap(),
                from: "old".into(),
                to: "new".into(),
                table_was: None,
            },
        )],
    };
    // The one column is renamed, so the row is keyed differently on each
    // side. That is the plan's own doing.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renaming,
        &table(&[("old", "kept"), ("note", "same")]),
        &table(&[("new", "kept"), ("note", "same")]),
        "prod",
        Settled::Whole,
    )
    .expect("a renamed column re-keys the row, which is not movement");

    // And the columns it leaves alone are still compared underneath.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renaming,
        &table(&[("old", "kept"), ("note", "same")]),
        &table(&[("new", "kept"), ("note", "rewritten")]),
        "prod",
        Settled::Whole,
    )
    .expect_err("the untouched column moved");
    assert!(format!("{e:#}").contains("row `k`"), "{e:#}");
    let _ = Cell::Value(Value::Null);
}

/// A securable takes its permissions with it, so a plan that drops a
/// granted table emits no `REVOKE` and the grant is simply gone
/// (DECISIONS 158).
#[test]
fn a_grant_on_a_table_this_plan_drops_goes_with_it() {
    use pbps_model::{GrantTarget, Permission};
    let mut before = Schema::default();
    before
        .tables
        .insert("dbo.t".parse().unwrap(), pbps_model::Table::default());
    before.roles.insert(
        "app".to_owned().into(),
        pbps_model::Role {
            description: None,
            grants: [(
                GrantTarget::Object("dbo.t".parse().unwrap()),
                [Permission::Select].into_iter().collect::<BTreeSet<_>>(),
            )]
            .into_iter()
            .collect(),
        },
    );
    let mut after = Schema::default();
    after.roles.insert(
        "app".to_owned().into(),
        pbps_model::Role {
            description: None,
            grants: BTreeMap::new(),
        },
    );
    let dropping = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::DropTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.t".parse().unwrap(),
                detach_from: None,
            },
        )],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &dropping,
        &before,
        &after,
        "prod",
        Settled::Whole,
    )
    .expect("the grant went with the table it was on");

    // A grant on a table the plan does *not* drop still has to be there.
    let mut before_two = before.clone();
    before_two.roles.get_mut("app").unwrap().grants.insert(
        GrantTarget::Schema("dbo".to_owned()),
        [Permission::Select].into_iter().collect(),
    );
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &dropping,
        &before_two,
        &after,
        "prod",
        Settled::Whole,
    )
    .expect_err("the schema grant did not go anywhere");
    assert!(format!("{e:#}").contains("role app"), "{e:#}");
}

/// Both ends of a rename are the plan's business. The recorded state knows
/// the object by its old name and the read-back by its new one, so a
/// comparison that took only one end would see the rename itself as a
/// table vanishing and another appearing — and refuse every rename.
#[test]
fn a_rename_exempts_the_name_at_each_end() {
    let mut before = Schema::default();
    before
        .tables
        .insert("dbo.old".parse().unwrap(), pbps_model::Table::default());
    before.roles.insert(
        "was".to_owned().into(),
        pbps_model::Role {
            description: None,
            grants: Default::default(),
        },
    );
    let mut after = Schema::default();
    after
        .tables
        .insert("dbo.new".parse().unwrap(), pbps_model::Table::default());
    after.roles.insert(
        "now".to_owned().into(),
        pbps_model::Role {
            description: None,
            grants: Default::default(),
        },
    );
    let uid: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
    let cs = pbps_model::ChangeSet {
        changes: [
            pbps_model::Change::RenameTable {
                uid: uid.clone(),
                from: "dbo.old".parse().unwrap(),
                to: "dbo.new".parse().unwrap(),
                defaults: Vec::new(),
            },
            pbps_model::Change::RenameRole {
                uid: "r_aaaaaa".parse().unwrap(),
                from: "was".to_owned().into(),
                to: "now".to_owned().into(),
            },
        ]
        .into_iter()
        .map(pbps_model::PlannedChange::new)
        .collect(),
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &cs,
        &before,
        &after,
        "prod",
        Settled::Whole,
    )
    .expect("a rename is not movement");
}

/// A renamed table's declaration is found under the name the database
/// still has; an unrenamed one, and one the plan creates, stay put.
#[test]
fn the_declarations_are_keyed_by_the_names_the_database_has_now() {
    let uid: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
    let mut final_ids = IdsFile::default();
    final_ids
        .tables
        .insert(uid.clone(), "dbo.t2".parse().unwrap());
    let mut live_ids = IdsFile::default();
    live_ids.tables.insert(uid, "dbo.t".parse().unwrap());
    let mut declared = Schema::default();
    let t = pbps_model::Table {
        data: Some(pbps_model::TableData {
            mode: DataMode::Exact,
            rows: BTreeMap::new(),
        }),
        ..Default::default()
    };
    declared.tables.insert("dbo.t2".parse().unwrap(), t);
    declared
        .tables
        .insert("dbo.new".parse().unwrap(), pbps_model::Table::default());

    let live = tables_under(&declared, &final_ids, &live_ids);
    let names: Vec<String> = live
        .tables
        .keys()
        .map(|name| name.raw().to_string())
        .collect();
    assert_eq!(names, ["dbo.new", "dbo.t"]);
    assert!(
        live.tables[&"dbo.t".parse::<TableName>().unwrap()]
            .data
            .is_some()
    );
    let scopes = live.data_scopes();
    assert!(scopes.contains_key(&"dbo.t".parse::<TableName>().unwrap()));
}

/// The recorded scope wins where there is one; a table only the plan
/// covers is pinned under the plan's.
#[test]
fn a_baseline_is_pinned_under_the_recorded_scopes_plus_the_newly_covered() {
    let t: TableName = "dbo.t".parse().unwrap();
    let u: TableName = "dbo.u".parse().unwrap();
    let recorded: DataScopes = [(
        t.clone(),
        DataScope {
            mode: DataMode::Ensure,
            keys: ["a"].into_iter().map(pbps_model::RowKey::from).collect(),
        },
    )]
    .into_iter()
    .collect();
    let exact = DataScope {
        mode: DataMode::Exact,
        keys: BTreeSet::new(),
    };
    let planned: DataScopes = [(t.clone(), exact.clone()), (u.clone(), exact.clone())]
        .into_iter()
        .collect();
    let pinned = pinned_scopes(&recorded, &planned);
    // Both cover `t`: the plan's `exact` widens the read, and the recorded
    // key is still spelled so the engine can say which row it names.
    assert_eq!(pinned[&t].mode, DataMode::Exact, "{:?}", pinned[&t]);
    assert_eq!(
        pinned[&t].keys,
        ["a"].into_iter().map(pbps_model::RowKey::from).collect()
    );
    assert_eq!(pinned[&u], exact, "newly covered: the plan's");
    assert_eq!(pinned.len(), 2);

    // A key added to an `ensure` block is pinned too: read at plan time,
    // it has to be checked again before apply, or a change to it in
    // between is overwritten by an approved update nobody measured.
    let wider: DataScopes = [(
        t.clone(),
        DataScope {
            mode: DataMode::Ensure,
            keys: ["a", "b"]
                .into_iter()
                .map(pbps_model::RowKey::from)
                .collect(),
        },
    )]
    .into_iter()
    .collect();
    let pinned = pinned_scopes(&recorded, &wider);
    assert_eq!(pinned[&t].mode, DataMode::Ensure);
    assert_eq!(
        pinned[&t].keys,
        ["a", "b"]
            .into_iter()
            .map(pbps_model::RowKey::from)
            .collect()
    );
}

/// The mapping from `01` to the stored `1` holds under `int`; a plan
/// that makes the column `varchar` is refused while it stands, and a
/// plan on a table whose keys are spelled the engine's way is not.
#[test]
fn a_key_type_change_over_an_aliased_key_is_named_and_one_without_is_not() {
    use pbps_model::{Change, ChangeSet, ColumnRef, ColumnType, PlannedChange};
    use std::str::FromStr;
    let table: TableName = "dbo.t".parse().unwrap();
    let mut declared = Schema::default();
    let mut t = pbps_model::Table::default();
    t.columns.insert(
        "id".into(),
        pbps_model::Column::new(ColumnType::from_str("varchar(10)").unwrap()).not_null(),
    );
    t.primary_key = Some(pbps_model::PrimaryKey {
        name: None,
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
    });
    declared.tables.insert(table.clone(), t);
    let change = |name: &str| {
        PlannedChange::new(Change::AlterColumnType {
            uid: "c_aaaaaa".parse().unwrap(),
            column: ColumnRef::new(table.clone(), name),
            from: ColumnType::from_str("int").unwrap(),
            to: ColumnType::from_str("varchar(10)").unwrap(),
            from_nullable: false,
            to_nullable: false,
            through_external_partitions: false,
            from_collation: None,
            to_collation: None,
            using: None,
        })
    };
    let cs = ChangeSet {
        changes: vec![change("id")],
    };
    let mut rows = pbps_model::ObservedRows::new();
    let mut observed = pbps_model::ObservedTable::default();
    observed.aliases.insert(
        pbps_model::RowKey::from("01"),
        pbps_model::RowKey::from("1"),
    );
    observed
        .aliases
        .insert(pbps_model::RowKey::from("2"), pbps_model::RowKey::from("2"));
    rows.insert(table.clone(), observed);

    // No rename in play: the plan's name is the database's name.
    let same = IdsFile::default();
    let found = key_type_changes_over_aliases(&cs, &declared, &rows, &same, &same);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("`01` (stored as `1`)"), "{}", found[0]);
    assert!(
        !found[0].contains("`2`"),
        "a key spelled the engine's way: {}",
        found[0]
    );

    // Not the key column: nothing to say. Keys all spelled the engine's
    // way: nothing to say either.
    let other = ChangeSet {
        changes: vec![change("label")],
    };
    assert!(key_type_changes_over_aliases(&other, &declared, &rows, &same, &same).is_empty());
    rows.get_mut(&table)
        .unwrap()
        .aliases
        .remove(&pbps_model::RowKey::from("01"));
    assert!(key_type_changes_over_aliases(&cs, &declared, &rows, &same, &same).is_empty());
}

/// The change names the table as the plan leaves it; the declarations and
/// the rows are keyed by the name the database has now. A rename in the
/// same revision made both lookups miss, and the guard let the key-type
/// change through.
#[test]
fn a_table_renamed_by_the_same_plan_is_still_checked_for_aliased_keys() {
    use pbps_model::{Change, ChangeSet, ColumnRef, ColumnType, PlannedChange};
    use std::str::FromStr;
    let uid: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
    let after: TableName = "dbo.customer".parse().unwrap();
    let before: TableName = "dbo.client".parse().unwrap();
    let mut final_ids = IdsFile::default();
    final_ids.tables.insert(uid.clone(), after.clone());
    let mut live_ids = IdsFile::default();
    live_ids.tables.insert(uid, before.clone());

    let mut t = pbps_model::Table::default();
    t.columns.insert(
        "id".into(),
        pbps_model::Column::new(ColumnType::from_str("varchar(10)").unwrap()).not_null(),
    );
    t.primary_key = Some(pbps_model::PrimaryKey {
        name: None,
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
    });
    // Keyed by the live name, as `tables_under` leaves it.
    let mut declared = Schema::default();
    declared.tables.insert(before.clone(), t);

    let mut rows = pbps_model::ObservedRows::new();
    let mut observed = pbps_model::ObservedTable::default();
    observed.aliases.insert(
        pbps_model::RowKey::from("01"),
        pbps_model::RowKey::from("1"),
    );
    rows.insert(before, observed);

    let cs = ChangeSet {
        changes: vec![PlannedChange::new(Change::AlterColumnType {
            uid: "c_aaaaaa".parse().unwrap(),
            // The plan's own name for it: the one after the rename.
            column: ColumnRef::new(after, "id"),
            from: ColumnType::from_str("int").unwrap(),
            to: ColumnType::from_str("varchar(10)").unwrap(),
            from_nullable: false,
            to_nullable: false,
            through_external_partitions: false,
            from_collation: None,
            to_collation: None,
            using: None,
        })],
    };
    let found = key_type_changes_over_aliases(&cs, &declared, &rows, &final_ids, &live_ids);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("`01` (stored as `1`)"), "{}", found[0]);
}

/// Counted off the emitter's statements: every member before statement
/// one, fewer as the `DROP MEMBER`s commit, none once the role is gone.
#[test]
fn a_resumed_role_drop_expects_only_the_members_whose_statements_remain() {
    use pbps_model::{Change, ChangeSet, PlannedChange};
    let uid: pbps_model::Uid = "r_aaaaaa".parse().unwrap();
    let cs = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::CreateRole {
                uid: uid.clone(),
                name: "auditors".into(),
            }),
            PlannedChange::new(Change::DropRole {
                uid,
                name: "reporting".into(),
                members: vec!["a".into(), "b".into()],
            }),
        ],
    };
    let expect = |completed: usize| {
        role_drop_expectations(
            &crate::fixture_ordered(cs.clone(), &pbps_mssql::Mssql),
            &pbps_mssql::Mssql,
            pbps_model::PlanMode::Staged,
            completed,
        )
        .unwrap()
    };
    let both = vec![(
        pbps_model::RoleName::from("reporting"),
        vec![
            pbps_model::RoleName::from("a"),
            pbps_model::RoleName::from("b"),
        ],
    )];
    assert_eq!(expect(0), both, "before statement one");
    assert_eq!(
        expect(1),
        both,
        "the CREATE ROLE is done; the drop has not started"
    );
    assert_eq!(
        expect(2),
        vec![(
            pbps_model::RoleName::from("reporting"),
            vec![pbps_model::RoleName::from("b")]
        )],
        "one DROP MEMBER committed"
    );
    assert_eq!(
        expect(3),
        vec![(pbps_model::RoleName::from("reporting"), vec![])],
        "every member gone; the DROP ROLE still to run"
    );
    assert!(
        expect(4).is_empty(),
        "the role is dropped; nothing to expect"
    );
}

/// The names the remaining statements need free and the ones they free
/// first, counted off the emitter's statements the same way: a rename
/// wants its new name and vacates its old one until its statement runs,
/// and a half-done role drop still holds the name it is vacating.
#[test]
fn a_resume_asks_only_about_the_role_names_its_remaining_statements_touch() {
    use pbps_model::{Change, ChangeSet, PlannedChange};
    let r1: pbps_model::Uid = "r_aaaaaa".parse().unwrap();
    let r2: pbps_model::Uid = "r_bbbbbb".parse().unwrap();
    let r3: pbps_model::Uid = "r_cccccc".parse().unwrap();
    let cs = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::RenameRole {
                uid: r1,
                from: "reader".into(),
                to: "app_reader".into(),
            }),
            PlannedChange::new(Change::CreateRole {
                uid: r2,
                name: "auditors".into(),
            }),
            PlannedChange::new(Change::DropRole {
                uid: r3,
                name: "reporting".into(),
                members: vec!["a".into()],
            }),
        ],
    };
    let expect = |completed: usize| {
        role_name_expectations(
            &crate::fixture_ordered(cs.clone(), &pbps_mssql::Mssql),
            &pbps_mssql::Mssql,
            pbps_model::PlanMode::Staged,
            completed,
        )
        .unwrap()
    };
    let s = |v: &[&str]| {
        v.iter()
            .map(|s| pbps_model::RoleName::from(*s))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        expect(0),
        (s(&["app_reader", "auditors"]), s(&["reader", "reporting"])),
        "before statement one"
    );
    assert_eq!(
        expect(1),
        (s(&["auditors"]), s(&["reporting"])),
        "the rename is done: its new name is held, its old one is nobody's business"
    );
    assert_eq!(
        expect(2),
        (s(&[]), s(&["reporting"])),
        "the CREATE ROLE is done; the drop has not started"
    );
    assert_eq!(
        expect(3),
        (s(&[]), s(&["reporting"])),
        "one DROP MEMBER committed; the role still holds its name"
    );
    assert_eq!(expect(4), (s(&[]), s(&[])), "everything ran");
}

/// A created object enters the checkpoint's identities the moment its
/// statement commits, under the plan's uid, columns and all; a name the
/// plan does not know is left alone.
#[test]
fn a_created_object_is_adopted_into_the_live_identities_under_the_plans_uid() {
    use pbps_dialect::Created;
    let t_uid: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
    let c_uid: pbps_model::Uid = "c_aaaaaa".parse().unwrap();
    let d_uid: pbps_model::Uid = "c_bbbbbb".parse().unwrap();
    let r_uid: pbps_model::Uid = "r_aaaaaa".parse().unwrap();
    let table: TableName = "app.audit".parse().unwrap();
    let mut plan_ids = IdsFile::default();
    plan_ids.tables.insert(t_uid.clone(), table.clone());
    plan_ids.columns.insert(
        c_uid.clone(),
        pbps_model::ColumnRef::new(table.clone(), "id"),
    );
    plan_ids.columns.insert(
        d_uid.clone(),
        pbps_model::ColumnRef::new("app.other".parse().unwrap(), "later"),
    );
    plan_ids
        .roles
        .insert(r_uid.clone(), "auditors".to_owned().into());

    let mut live = IdsFile::default();
    adopt_created(&mut live, &plan_ids, &[Created::Table(table.clone())]);
    assert_eq!(live.tables.get(&t_uid), Some(&table));
    assert!(live.columns.contains_key(&c_uid), "{live:?}");
    assert!(!live.columns.contains_key(&d_uid), "another table's column");
    assert!(live.roles.is_empty());

    adopt_created(
        &mut live,
        &plan_ids,
        &[
            Created::Column("app.other".parse().unwrap(), "later".into()),
            Created::Role("auditors".into()),
            Created::Role("nobody".into()),
            Created::Table("app.unknown".parse().unwrap()),
        ],
    );
    assert!(live.columns.contains_key(&d_uid));
    assert_eq!(
        live.roles.get(&r_uid).map(pbps_model::RoleName::raw),
        Some("auditors")
    );
    assert_eq!(live.roles.len(), 1);
    assert_eq!(live.tables.len(), 1);
}

/// Halfway through a rename that moves both schema and name, the table
/// stands at neither end; the checkpoint's scope has to stand there too.
#[test]
fn a_checkpoint_reads_rows_under_the_name_the_catalog_has_now() {
    let uid: pbps_model::Uid = "t_aaaaaa".parse().unwrap();
    let mut final_ids = IdsFile::default();
    final_ids
        .tables
        .insert(uid.clone(), "app.customer".parse().unwrap());
    let mut live_ids = IdsFile::default();
    live_ids
        .tables
        .insert(uid, "app.old_customer".parse().unwrap());
    let scope = DataScope {
        mode: DataMode::Exact,
        keys: BTreeSet::new(),
    };
    let mut data = DataScopes::new();
    data.insert("app.customer".parse().unwrap(), scope.clone());
    // A table the plan creates later is not in the live mapping at all,
    // and keeps its final name: there is nothing to read yet either way.
    data.insert("app.later".parse().unwrap(), scope.clone());

    let now = scopes_under(&data, &final_ids, &live_ids);
    let names: Vec<String> = now.keys().map(|name| name.raw().to_string()).collect();
    assert_eq!(names, ["app.later", "app.old_customer"]);
    assert_eq!(
        now[&"app.old_customer".parse::<TableName>().unwrap()],
        scope
    );
}

/// A schema target the database spells differently is refused; one it
/// does not have at all is the probe's business, and one that agrees is
/// nobody's.
#[test]
fn only_a_schema_the_database_spells_differently_is_reported() {
    let mut spelled = BTreeMap::new();
    spelled.insert("DBO".to_owned(), Some("dbo".to_owned()));
    spelled.insert("sales".to_owned(), Some("sales".to_owned()));
    spelled.insert("legacy".to_owned(), None);
    let declared_by = [("DBO".to_owned(), "granted to role `reporting`".to_owned())]
        .into_iter()
        .collect();

    let wrong = wrongly_spelt(&spelled, &declared_by);
    assert_eq!(wrong.len(), 1, "{wrong:?}");
    assert!(wrong[0].contains("`DBO`"), "{wrong:?}");
    assert!(
        wrong[0].contains("granted to role `reporting`"),
        "{wrong:?}"
    );
    assert!(wrong[0].contains("`dbo`"), "{wrong:?}");
}

fn pulled() -> Pulled {
    Pulled {
        baseline_evidence: None,
        schema: Schema::default(),
        onboarding_notices: Vec::new(),
        catalog_notices: Vec::new(),
        extension_notices: BTreeMap::new(),
        provided_types: BTreeMap::new(),
        provided_classes: BTreeMap::new(),
        warnings: Vec::new(),
        unexpressible: Vec::new(),
        limitations: vec![
            Limitation {
                target: pbps_db::catalog::LimitationTarget::Relation(
                    "dbo.managed".parse().unwrap(),
                ),
                detail: "dbo.managed has a computed column".into(),
            },
            Limitation {
                target: pbps_db::catalog::LimitationTarget::Relation("dbo.theirs".parse().unwrap()),
                detail: "dbo.theirs has a computed column".into(),
            },
        ],
        unmanaged_modules: vec![
            UnmanagedModule {
                kind: "procedure",
                target: pbps_db::catalog::LimitationTarget::SharedModule(
                    "dbo.declared_secret".parse().unwrap(),
                ),
                why: "its definition cannot be read back".into(),
            },
            UnmanagedModule {
                kind: "procedure",
                target: pbps_db::catalog::LimitationTarget::SharedModule(
                    "dbo.stray_secret".parse().unwrap(),
                ),
                why: "its definition cannot be read back".into(),
            },
        ],
        public_execute: Default::default(),
        owners: Default::default(),
        session_role: String::new().into(),
        unrevocable: Vec::new(),
        database_collation: None,
    }
}

fn ids() -> IdsFile {
    let mut ids = IdsFile::default();
    ids.tables.insert(
        pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.managed", 0),
        "dbo.managed".parse().unwrap(),
    );
    ids
}

/// A module the managed set names and the catalog cannot read is a
/// limitation of the projection, exactly as an unsupported feature on a
/// managed table is. Reported as a warning instead, `baseline` recorded a
/// schema without it, and the scope rebuilt from that schema then treated
/// the same module as undeclared.
#[test]
fn a_module_in_the_managed_set_that_cannot_be_read_back_is_a_limitation() {
    let modules = std::iter::once("dbo.declared_secret".parse().unwrap()).collect();
    let found = managed_limitations(&pulled(), &ids(), &modules, &BTreeSet::new());
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0], "dbo.managed has a computed column");
    assert!(
        found[1].contains("procedure dbo.declared_secret is in the managed set")
            && found[1].contains("cannot be read back"),
        "{found:?}"
    );
}

/// The other direction: an unreadable module outside the set is the
/// unmanaged policy's business (see [`unmanaged_objects`]), not a
/// limitation — and a limitation on somebody else's table is neither.
#[test]
fn objects_outside_the_managed_set_are_not_limitations() {
    let found = managed_limitations(&pulled(), &ids(), &Default::default(), &BTreeSet::new());
    assert_eq!(found, ["dbo.managed has a computed column"]);

    let found = managed_limitations(
        &pulled(),
        &IdsFile::default(),
        &Default::default(),
        &BTreeSet::new(),
    );
    assert!(found.is_empty(), "{found:?}");
}

/// The two inventories partition the unreadable modules: whichever set a
/// module falls outside, one of them names it, so an encrypted module can
/// never be silent in a command that consults both.
#[test]
fn every_unreadable_module_is_either_a_limitation_or_unmanaged() {
    let pulled = pulled();
    let modules = std::iter::once("dbo.declared_secret".parse().unwrap()).collect();
    let scoped = pbps_diff::scope(
        &pulled.schema,
        &IdsFile::default(),
        &modules,
        &Default::default(),
    );
    let unreadable = unreadable_objects(&pulled);

    let limited = managed_limitations(&pulled, &IdsFile::default(), &modules, &BTreeSet::new());
    let unmanaged = unmanaged_objects(&scoped, &unreadable, &IdsFile::default(), &modules);
    assert!(limited.iter().any(|l| l.contains("dbo.declared_secret")));
    assert!(!limited.iter().any(|l| l.contains("dbo.stray_secret")));
    assert!(unmanaged.iter().any(|u| u.contains("dbo.stray_secret")));
    assert!(!unmanaged.iter().any(|u| u.contains("dbo.declared_secret")));
}

#[test]
fn managed_omission_fallbacks_only_yield_to_the_same_target_and_detail() {
    use pbps_db::catalog::LimitationTarget;
    let mut pulled = pulled();
    pulled.limitations.clear();
    pulled.unmanaged_modules.clear();
    let target = LimitationTarget::module("app.f(integer)".parse().unwrap());
    let other = LimitationTarget::module("app.f(bigint)".parse().unwrap());
    let modules = ["app.f(integer)", "app.f(bigint)"]
        .into_iter()
        .map(|name| name.parse().unwrap())
        .collect();
    for detail in ["omitted aggregate", "another unsupported property"] {
        pulled.limitations.push(Limitation {
            target: target.clone(),
            detail: detail.into(),
        });
    }
    for (target, why) in [
        (target.clone(), "omitted aggregate"),
        (target, "inventory-only fact"),
        (other, "omitted aggregate"),
    ] {
        pulled.unmanaged_modules.push(UnmanagedModule {
            kind: "aggregate",
            target,
            why: why.into(),
        });
    }
    let before = pulled.unmanaged_modules.clone();
    let facts = managed_limitations(&pulled, &IdsFile::default(), &modules, &BTreeSet::new());
    assert_eq!(facts.len(), 4, "{facts:?}");
    assert_eq!(facts[0], "omitted aggregate");
    assert_eq!(facts[1], "another unsupported property");
    assert!(facts[2].contains("app.f(integer)") && facts[2].contains("inventory-only fact"));
    assert!(facts[3].contains("app.f(bigint)") && facts[3].contains("omitted aggregate"));
    assert_eq!(pulled.unmanaged_modules, before);
    assert_eq!(pulled.limitations.len(), 2);
    assert!(
        managed_limitations(
            &pulled,
            &IdsFile::default(),
            &BTreeSet::new(),
            &BTreeSet::new()
        )
        .is_empty()
    );
}

#[test]
fn managed_omission_table_identities_do_not_capture_other_namespaces() {
    use pbps_db::catalog::LimitationTarget;
    let ids = ids();
    let mut pulled = pulled();
    pulled.limitations.clear();
    pulled.unmanaged_modules.clear();
    let managed = LimitationTarget::Relation("dbo.managed".parse().unwrap());
    let targets = [
        managed.clone(),
        LimitationTarget::Relation("dbo.theirs".parse().unwrap()),
        LimitationTarget::module("dbo.managed(integer)".parse().unwrap()),
        LimitationTarget::module("dbo.t.managed".parse().unwrap()),
        LimitationTarget::UnnameableModule("dbo.managed".parse().unwrap()),
    ];
    for target in targets {
        let why = format!("omitted {target:?}");
        pulled.limitations.push(Limitation {
            target: target.clone(),
            detail: why.clone(),
        });
        pulled.unmanaged_modules.push(UnmanagedModule {
            kind: "module",
            target,
            why,
        });
    }
    let modules = BTreeSet::new();
    // The identity remains authoritative even though no table was pulled.
    let scoped = pbps_diff::scope(&pulled.schema, &ids, &modules, &Default::default());
    assert!(scoped.schema.tables.is_empty());
    let unreadable = unreadable_objects(&pulled);
    let outside = unmanaged_objects(&scoped, &unreadable, &ids, &modules);
    assert_eq!(outside.len(), 4, "{outside:?}");
    for item in pulled.unmanaged_modules.iter().skip(1) {
        assert!(
            outside.iter().any(|fact| fact.contains(&item.why)),
            "{outside:?}"
        );
    }
    assert!(
        !outside
            .iter()
            .any(|fact| fact.contains(&pulled.limitations[0].detail))
    );
    assert_eq!(
        managed_limitations(&pulled, &ids, &modules, &BTreeSet::new()),
        [pulled.limitations[0].detail.clone()]
    );

    // Inventory-only fallback has the same membership even without a
    // typed limitation; SQL Server's shared-module fallback is tested above.
    pulled.limitations.clear();
    let facts = managed_limitations(&pulled, &ids, &modules, &BTreeSet::new());
    assert_eq!(facts.len(), 1, "{facts:?}");
    assert!(facts[0].contains("dbo.managed is in the managed set"));
    let empty = IdsFile::default();
    let unscoped = pbps_diff::scope(&pulled.schema, &empty, &modules, &Default::default());
    assert_eq!(
        unmanaged_objects(&unscoped, &unreadable, &empty, &modules).len(),
        5
    );
    assert!(managed_limitations(&pulled, &empty, &modules, &BTreeSet::new()).is_empty());
}

#[test]
fn omitted_types_are_managed_only_by_their_own_declared_identities() {
    use pbps_db::catalog::{Limitation, LimitationTarget, Pulled};
    let name = TableName::new("app", "mood");
    let pulled = Pulled {
        limitations: vec![Limitation {
            target: LimitationTarget::Type(name.clone()),
            detail: "type app.mood cannot be represented".into(),
        }],
        ..Default::default()
    };
    let mut ids = IdsFile::default();
    ids.tables.insert(
        pbps_model::Uid::derived(pbps_model::UidKind::Table, "app.mood", 0),
        name.clone(),
    );
    let modules = [ModuleId::Named(name.clone())].into_iter().collect();
    assert!(managed_limitations(&pulled, &ids, &modules, &BTreeSet::new()).is_empty());
    let unreadable = unreadable_objects(&pulled);
    let scoped = pbps_diff::scope(&pulled.schema, &ids, &modules, &BTreeSet::new());
    assert_eq!(
        unmanaged_objects(&scoped, &unreadable, &ids, &modules),
        ["type app.mood (type app.mood cannot be represented)"]
    );
    let refusal = cut_report(
        &pulled,
        &ids,
        &modules,
        &BTreeSet::new(),
        &unreadable,
        pbps_config::Unmanaged::Error.into(),
        CatalogWarnings::Human,
    )
    .unwrap_err();
    assert!(refusal.is::<UnmanagedPolicy>(), "{refusal:#}");
    assert!(refusal.to_string().contains("type app.mood"));
    ids.types.insert(
        pbps_model::Uid::derived(pbps_model::UidKind::Type, "app.mood", 0),
        "app.mood".parse().unwrap(),
    );
    assert_eq!(
        managed_limitations(&pulled, &ids, &modules, &BTreeSet::new()),
        ["type app.mood cannot be represented"]
    );
    assert!(unmanaged_objects(&scoped, &unreadable, &ids, &modules).is_empty());
    cut_report(
        &pulled,
        &ids,
        &modules,
        &BTreeSet::new(),
        &unreadable,
        pbps_config::Unmanaged::Error.into(),
        CatalogWarnings::Human,
    )
    .expect("a declared type is refused by managed limitations, not unmanaged policy");
    let relation = Pulled {
        limitations: vec![Limitation {
            target: LimitationTarget::Relation(name),
            detail: "table app.mood cannot be represented".into(),
        }],
        ..Default::default()
    };
    ids.tables.clear();
    assert!(managed_limitations(&relation, &ids, &BTreeSet::new(), &BTreeSet::new()).is_empty());
}

#[test]
fn shared_module_names_match_triggers_but_relation_names_do_not() {
    use pbps_db::catalog::LimitationTarget;
    let trigger: ModuleId = "dbo.t.audit".parse().unwrap();
    let name = trigger.object_name();
    assert!(LimitationTarget::SharedModule(name.clone()).matches_module(&trigger));
    assert!(!LimitationTarget::Relation(name.clone()).matches_module(&trigger));
    assert!(LimitationTarget::Relation(name.clone()).matches_module(&ModuleId::Named(name)));
}

#[test]
fn unreadable_inventory_keeps_overloads_trigger_parents_and_unnameable_identities_distinct() {
    use pbps_db::catalog::LimitationTarget;
    let mut pulled = pulled();
    pulled.limitations.clear();
    let held: BTreeSet<ModuleId> = ["app.f(integer)", "app.t.audit"]
        .into_iter()
        .map(|id| id.parse().unwrap())
        .collect();
    let targets = [
        LimitationTarget::module("app.f(integer)".parse().unwrap()),
        LimitationTarget::module("app.f(bigint)".parse().unwrap()),
        LimitationTarget::module("app.t.audit".parse().unwrap()),
        LimitationTarget::module("app.other.audit".parse().unwrap()),
        LimitationTarget::UnnameableModule("app.f".parse().unwrap()),
    ];
    pulled.unmanaged_modules = targets
        .into_iter()
        .map(|target| UnmanagedModule {
            kind: "module",
            target,
            why: "cannot be represented".into(),
        })
        .collect();
    let scoped = pbps_diff::scope(
        &pulled.schema,
        &IdsFile::default(),
        &held,
        &Default::default(),
    );
    let unreadable = unreadable_objects(&pulled);
    let unmanaged = unmanaged_objects(&scoped, &unreadable, &IdsFile::default(), &held);
    assert_eq!(unmanaged.len(), 3, "{unmanaged:?}");
    assert!(unmanaged.iter().any(|s| s.contains("app.f(bigint)")));
    assert!(unmanaged.iter().any(|s| s.contains("app.other.audit")));
    assert!(unmanaged.iter().any(|s| s.contains("module app.f (")));
    let limited = managed_limitations(&pulled, &IdsFile::default(), &held, &BTreeSet::new());
    assert_eq!(limited.len(), 2, "{limited:?}");
    assert!(limited.iter().any(|s| s.contains("app.f(integer)")));
    assert!(limited.iter().any(|s| s.contains("app.t.audit")));
}

/// A column this plan adds or alters is held to what the plan gives it,
/// field by field, once every statement has run. The shape comparison
/// excludes exactly those fields, so nothing else says what became of them
/// — and presence alone let a column another session retyped after the
/// plan's own statement be recorded as the plan's result, while a created
/// table's columns were already held to their declaration (DECISIONS 189).
#[test]
fn a_column_this_plan_writes_is_held_to_the_definition_it_gives() {
    let dbo_t: TableName = "dbo.t".parse().unwrap();
    let column = |ty: &str, nullable: bool, default: Option<&str>| {
        let mut c = pbps_model::Column::new(ty.parse().unwrap());
        c.nullable = nullable;
        c.value_source = default.map_or(pbps_model::ValueSource::None, |text| {
            pbps_model::ValueSource::Default(text.into())
        });
        c
    };
    let schema = |columns: &[(&str, pbps_model::Column)]| {
        let mut t = pbps_model::Table::default();
        for (name, c) in columns {
            t.columns.insert((*name).into(), c.clone());
        }
        let mut s = Schema::default();
        s.tables.insert(dbo_t.clone(), t);
        s
    };
    let plan = |change: pbps_model::Change| pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(change)],
    };
    let kept = column("int", false, None);
    let before = schema(&[("kept", kept.clone())]);

    // Adding: the read-back is the declaration, in the engine's spelling.
    let adding = plan(pbps_model::Change::AddColumn {
        uid: "c_aaaaaa".parse().unwrap(),
        table: dbo_t.clone(),
        name: "amount".into(),
        column: Box::new(column("decimal", false, Some("0"))),
    });
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &adding,
        &before,
        &schema(&[
            ("kept", kept.clone()),
            ("amount", column("decimal(18,0)", false, Some("((0))"))),
        ]),
        "prod",
        Settled::Whole,
    )
    .expect("the engine's spelling of the declared column");
    for (what, got) in [
        ("type", column("decimal(18,2)", false, Some("((0))"))),
        ("nullability", column("decimal(18,0)", true, Some("((0))"))),
        ("default", column("decimal(18,0)", false, None)),
    ] {
        let e = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &adding,
            &before,
            &schema(&[("kept", kept.clone()), ("amount", got)]),
            "prod",
            Settled::Whole,
        )
        .expect_err(what);
        assert!(format!("{e:#}").contains("`amount`"), "{what}: {e:#}");
    }

    // Altering: each change promises the field it moves, and only that.
    let retyping = plan(pbps_model::Change::AlterColumnType {
        uid: "c_aaaaaa".parse().unwrap(),
        column: dbo_t.column("kept"),
        from: "int".parse().unwrap(),
        to: "bigint".parse().unwrap(),
        from_nullable: false,
        to_nullable: false,
        through_external_partitions: false,
        from_collation: None,
        to_collation: None,
        using: None,
    });
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &retyping,
        &before,
        &schema(&[("kept", column("bigint", false, None))]),
        "prod",
        Settled::Whole,
    )
    .expect("the type the plan gives it");
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &retyping,
        &before,
        &schema(&[("kept", column("int", false, None))]),
        "prod",
        Settled::Whole,
    )
    .expect_err("the ALTER was undone before the read");
    assert!(format!("{e:#}").contains("type"), "{e:#}");
    // `ALTER COLUMN` restates the nullability with the type, so a retype
    // promises it too.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &retyping,
        &before,
        &schema(&[("kept", column("bigint", true, None))]),
        "prod",
        Settled::Whole,
    )
    .expect_err("the nullability the ALTER restated");
    assert!(format!("{e:#}").contains("nullability"), "{e:#}");

    let loosening = plan(pbps_model::Change::AlterColumnNullability {
        uid: "c_aaaaaa".parse().unwrap(),
        column: dbo_t.column("kept"),
        ty: "int".parse().unwrap(),
        to_nullable: true,
        through_external_partitions: false,
        collation: None,
    });
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &loosening,
        &before,
        &before,
        "prod",
        Settled::Whole,
    )
    .expect_err("still NOT NULL");
    assert!(format!("{e:#}").contains("nullability"), "{e:#}");

    let defaulting = plan(pbps_model::Change::AlterColumnDefault {
        uid: "c_aaaaaa".parse().unwrap(),
        column: dbo_t.column("kept"),
        from: None,
        to: Some("0".to_owned()),
    });
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &defaulting,
        &before,
        &schema(&[("kept", column("int", false, Some("((0))")))]),
        "prod",
        Settled::Whole,
    )
    .expect("the text is the engine's; that there is one is the promise");
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &defaulting,
        &before,
        &before,
        "prod",
        Settled::Whole,
    )
    .expect_err("no default arrived");
    assert!(format!("{e:#}").contains("default"), "{e:#}");

    // Not at a checkpoint: the statement may not have run yet.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &retyping,
        &before,
        &before,
        "prod",
        Settled::SoFar,
    )
    .expect("mid-run, the old type is not a failure");
}

/// A column is excused from the shape comparison only on the read where
/// it is on one side alone. Across the read that spans its rename it is
/// followed from the old name to the new; on every other read of a
/// staged run it is two read-backs like any other column. Excusing the
/// name outright left a renamed or added column exempt for the rest of
/// the run (DECISIONS 189).
#[test]
fn a_column_is_followed_across_its_rename_and_compared_after_it() {
    let dbo_t: TableName = "dbo.t".parse().unwrap();
    let schema = |name: &str, ty: &str, nullable: bool| {
        let mut c = pbps_model::Column::new(ty.parse().unwrap());
        c.nullable = nullable;
        let mut t = pbps_model::Table::default();
        t.columns.insert(name.into(), c);
        let mut s = Schema::default();
        s.tables.insert(dbo_t.clone(), t);
        s
    };
    let renaming = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::RenameColumn {
                uid: "c_aaaaaa".parse().unwrap(),
                table: dbo_t.clone(),
                from: "a".into(),
                to: "b".into(),
                table_was: None,
            },
        )],
    };
    // The read spanning the rename: one column, two names.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renaming,
        &schema("a", "int", false),
        &schema("b", "int", false),
        "prod",
        Settled::SoFar,
    )
    .expect("renamed, otherwise as it was");
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renaming,
        &schema("a", "int", false),
        &schema("b", "bigint", false),
        "prod",
        Settled::SoFar,
    )
    .expect_err("renamed and retyped, and the plan only renames");
    assert!(format!("{e:#}").contains("`b`"), "{e:#}");
    // A later read of the same staged run: both sides know it as `b`.
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renaming,
        &schema("b", "int", false),
        &schema("b", "int", true),
        "prod",
        Settled::SoFar,
    )
    .expect_err("loosened after the rename, by nobody in this plan");
    assert!(format!("{e:#}").contains("nullability"), "{e:#}");
    // And before it ran: both sides know it as `a`, and it has not moved.
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renaming,
        &schema("a", "int", false),
        &schema("a", "int", false),
        "prod",
        Settled::SoFar,
    )
    .expect("not renamed yet");

    // The same for a column the plan adds: on both sides of a later read,
    // it is compared like any other.
    let adding = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::AddColumn {
                uid: "c_aaaaaa".parse().unwrap(),
                table: dbo_t.clone(),
                name: "b".into(),
                column: Box::new(pbps_model::Column::new("int".parse().unwrap())),
            },
        )],
    };
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &adding,
        &schema("b", "int", true),
        &schema("b", "bigint", true),
        "prod",
        Settled::SoFar,
    )
    .expect_err("added at an earlier checkpoint, retyped since");
    assert!(format!("{e:#}").contains("type"), "{e:#}");
    // A rename the plan pairs with a retype excuses the type across the
    // rename, and nothing else.
    let renaming_and_retyping = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::RenameColumn {
                uid: "c_aaaaaa".parse().unwrap(),
                table: dbo_t.clone(),
                from: "a".into(),
                to: "b".into(),
                table_was: None,
            }),
            pbps_model::PlannedChange::new(pbps_model::Change::AlterColumnType {
                uid: "c_aaaaaa".parse().unwrap(),
                column: dbo_t.column("b"),
                from: "int".parse().unwrap(),
                to: "bigint".parse().unwrap(),
                from_nullable: false,
                to_nullable: false,
                through_external_partitions: false,
                from_collation: None,
                to_collation: None,
                using: None,
            }),
        ],
    };
    refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renaming_and_retyping,
        &schema("a", "int", false),
        &schema("b", "bigint", false),
        "prod",
        Settled::Whole,
    )
    .expect("renamed and retyped, as planned");
    let e = refuse_unplanned_movement(
        &pbps_mssql::Mssql,
        &renaming_and_retyping,
        &schema("a", "int", false),
        &schema("b", "bigint", true),
        "prod",
        Settled::Whole,
    )
    .expect_err("the nullability is nobody's plan");
    assert!(format!("{e:#}").contains("nullability"), "{e:#}");
}

/// A column's collation is held to what the plan leaves it (#1175): an
/// `ALTER COLUMN` promises the collation it restates, and one read back
/// under another — the silent reset an omitted `COLLATE` makes — is not
/// the plan's result; nor is a collation another session changed on a
/// column the plan does not touch.
#[test]
fn a_columns_collation_is_held_to_what_the_plan_leaves_it() {
    use pbps_model::Collation;
    let dbo_t: TableName = "dbo.t".parse().unwrap();
    let schema = |nullable: bool, collation: Option<&str>| {
        let mut c = pbps_model::Column::new("varchar(10)".parse().unwrap());
        c.nullable = nullable;
        c.collation = collation.map(Collation::new);
        let mut t = pbps_model::Table::default();
        t.columns.insert("c".into(), c);
        t.columns
            .insert("n".into(), pbps_model::Column::new("int".parse().unwrap()));
        Schema {
            tables: [(dbo_t.clone(), t)].into(),
            ..Default::default()
        }
    };
    let tightening = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::AlterColumnNullability {
                uid: "c_aaaaaa".parse().unwrap(),
                column: dbo_t.column("c"),
                ty: "varchar(10)".parse().unwrap(),
                to_nullable: false,
                through_external_partitions: false,
                collation: Some(Collation::new("Latin1_General_CS_AS")),
            },
        )],
    };
    let check = |plan: &pbps_model::ChangeSet, after: &Schema| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            plan,
            &schema(true, Some("Latin1_General_CS_AS")),
            after,
            "prod",
            Settled::Whole,
        )
    };
    check(&tightening, &schema(false, Some("Latin1_General_CS_AS")))
        .expect("tightened, collation kept");
    let e = check(&tightening, &schema(false, None))
        .expect_err("the collation was reset to the default");
    assert!(format!("{e:#}").contains("collation"), "{e:#}");
    // Negative: a plan that touches another column is not excused for
    // this one's collation.
    let elsewhere = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::AlterColumnNullability {
                uid: "c_bbbbbb".parse().unwrap(),
                column: dbo_t.column("n"),
                ty: "int".parse().unwrap(),
                to_nullable: true,
                through_external_partitions: false,
                collation: None,
            },
        )],
    };
    let e = check(&elsewhere, &schema(true, Some("Latin1_General_CI_AS")))
        .expect_err("another session changed the collation");
    assert!(format!("{e:#}").contains("collation"), "{e:#}");
}

/// A part this plan adds is held to the definition it adds it with, not
/// to being there: anyone who redefines a constraint drops and recreates
/// it under the same name, so a read that finds the name says nothing
/// about which definition is behind it (DECISIONS 189).
#[test]
fn a_part_this_plan_adds_is_held_to_its_definition() {
    let dbo_t: TableName = "dbo.t".parse().unwrap();
    let dbo_p: TableName = "dbo.p".parse().unwrap();
    let index = |on: &str, unique: bool| pbps_model::Index {
        columns: vec![pbps_model::IndexColumn {
            key: pbps_model::IndexKey::Column(on.into()),
            descending: false,
            opclass: None,
            collation: None,
        }],
        include: Vec::new(),
        unique,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
    };
    let unique = |on: &str| pbps_model::UniqueConstraint {
        columns: vec![on.into()],
        storage_parameters: Default::default(),
    };
    let key = |on: &str| pbps_model::PrimaryKey {
        name: None,
        columns: vec![on.into()],
        storage_parameters: Default::default(),
    };
    let fk = |to: &TableName, on_delete: pbps_model::ReferentialAction| pbps_model::ForeignKey {
        columns: vec!["p_id".into()],
        references_table: to.clone(),
        references_columns: vec!["id".into()],
        on_delete,
        on_update: pbps_model::ReferentialAction::NoAction,
    };
    let with = |f: &dyn Fn(&mut pbps_model::Table)| {
        let mut t = pbps_model::Table::default();
        for c in ["id", "other", "p_id"] {
            t.columns
                .insert(c.into(), pbps_model::Column::new("int".parse().unwrap()));
        }
        f(&mut t);
        let mut s = Schema::default();
        s.tables.insert(dbo_t.clone(), t);
        s.tables.insert(dbo_p.clone(), pbps_model::Table::default());
        s
    };
    let bare = with(&|_| {});
    let plan = |change: pbps_model::Change| pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(change)],
    };
    let check = |plan: &pbps_model::ChangeSet, after: &Schema| {
        refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            plan,
            &bare,
            after,
            "prod",
            Settled::Whole,
        )
    };
    let refused = |plan: &pbps_model::ChangeSet, after: &Schema, why: &str| {
        let e = check(plan, after).expect_err(why);
        let e = format!("{e:#}");
        assert!(e.contains("is not the one this plan adds"), "{why}: {e}");
    };

    let adding_index = plan(pbps_model::Change::AddIndex {
        table: dbo_t.clone(),
        name: "ix".into(),
        index: Box::new(index("id", false)),
        clustered: false,
    });
    check(
        &adding_index,
        &with(&|t| {
            t.indexes.insert("ix".into(), index("id", false));
        }),
    )
    .expect("the index as planned");
    // The layout is part of the definition (#1178): the same index read
    // back as the clustered one is not the index this plan adds, and a
    // clustered one read back nonclustered is not either.
    refused(
        &adding_index,
        &with(&|t| {
            t.indexes.insert("ix".into(), index("id", false));
            t.clustered = Some(pbps_model::Clustered::Index("ix".into()));
        }),
        "the index came back clustered",
    );
    // So is the method (DEC-1169.1): the planned B-tree read back as a
    // GIN index over the same key is another index.
    refused(
        &adding_index,
        &with(&|t| {
            let mut gin = index("id", false);
            gin.method = pbps_model::IndexMethod::Gin;
            t.indexes.insert("ix".into(), gin);
        }),
        "the index came back under another method",
    );
    // An expression key is held to being one, not to its text, which
    // the engine respells (DEC-1169.2); the same position read back as a
    // column is another index.
    let over = |key: pbps_model::IndexKey| pbps_model::Index {
        columns: vec![pbps_model::IndexColumn {
            key,
            descending: false,
            opclass: None,
            collation: None,
        }],
        ..index("id", false)
    };
    let adding_expression = plan(pbps_model::Change::AddIndex {
        table: dbo_t.clone(),
        name: "ix".into(),
        index: Box::new(over(pbps_model::IndexKey::Expression("id+1".into()))),
        clustered: false,
    });
    check(
        &adding_expression,
        &with(&|t| {
            t.indexes.insert(
                "ix".into(),
                over(pbps_model::IndexKey::Expression("(id + 1)".into())),
            );
        }),
    )
    .expect("the expression as the engine respells it");
    refused(
        &adding_expression,
        &with(&|t| {
            t.indexes
                .insert("ix".into(), over(pbps_model::IndexKey::Column("id".into())));
        }),
        "the expression key came back as a column",
    );
    let adding_clustered = plan(pbps_model::Change::AddIndex {
        table: dbo_t.clone(),
        name: "ix".into(),
        index: Box::new(index("id", false)),
        clustered: true,
    });
    check(
        &adding_clustered,
        &with(&|t| {
            t.indexes.insert("ix".into(), index("id", false));
            t.clustered = Some(pbps_model::Clustered::Index("ix".into()));
        }),
    )
    .expect("the clustered index as planned");
    refused(
        &adding_clustered,
        &with(&|t| {
            t.indexes.insert("ix".into(), index("id", false));
        }),
        "the index came back nonclustered",
    );
    let adding_key = |nonclustered: bool| {
        plan(pbps_model::Change::SetPrimaryKey {
            table: dbo_t.clone(),
            from: None,
            to: Some(key("id")),
            nonclustered,
        })
    };
    let keyed = |layout: Option<pbps_model::Clustered>| {
        with(&|t| {
            t.primary_key = Some(key("id"));
            t.clustered = layout.clone();
        })
    };
    check(&adding_key(false), &keyed(None)).expect("the clustered key as planned");
    check(&adding_key(true), &keyed(Some(pbps_model::Clustered::Heap)))
        .expect("the nonclustered key as planned");
    refused(
        &adding_key(false),
        &keyed(Some(pbps_model::Clustered::Heap)),
        "the key came back nonclustered",
    );
    refused(
        &adding_key(true),
        &keyed(None),
        "the key came back clustered",
    );
    refused(
        &adding_index,
        &with(&|t| {
            t.indexes.insert("ix".into(), index("other", false));
        }),
        "an index of that name on another column",
    );
    refused(
        &adding_index,
        &with(&|t| {
            t.indexes.insert("ix".into(), index("id", true));
        }),
        "an index of that name, made unique by somebody",
    );
    // Absent is a different finding from wrong, and keeps its own words.
    let e = check(&adding_index, &bare).expect_err("not there at all");
    assert!(format!("{e:#}").contains("is not there"), "{e:#}");

    let adding_unique = plan(pbps_model::Change::AddUnique {
        table: dbo_t.clone(),
        name: "uq".into(),
        constraint: unique("id"),
        clustered: false,
    });
    refused(
        &adding_unique,
        &with(&|t| {
            t.unique.insert("uq".into(), unique("other"));
        }),
        "a unique of that name over another column",
    );

    let keying = plan(pbps_model::Change::SetPrimaryKey {
        table: dbo_t.clone(),
        from: None,
        to: Some(key("id")),
        nonclustered: false,
    });
    check(
        &keying,
        &with(&|t| {
            t.primary_key = Some(pbps_model::PrimaryKey {
                name: Some("PK__t__3213E83F".into()),
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
        }),
    )
    .expect("the engine names an unnamed key; that is not movement");
    refused(
        &keying,
        &with(&|t| {
            t.primary_key = Some(key("other"));
        }),
        "a key over another column",
    );

    let adding_fk = plan(pbps_model::Change::AddForeignKey {
        table: dbo_t.clone(),
        name: "fk".into(),
        constraint: Box::new(fk(&dbo_p, pbps_model::ReferentialAction::Cascade)),
    });
    check(
        &adding_fk,
        &with(&|t| {
            t.foreign_keys.insert(
                "fk".into(),
                fk(&dbo_p, pbps_model::ReferentialAction::Cascade),
            );
        }),
    )
    .expect("the foreign key as planned");
    refused(
        &adding_fk,
        &with(&|t| {
            t.foreign_keys.insert(
                "fk".into(),
                fk(&dbo_p, pbps_model::ReferentialAction::NoAction),
            );
        }),
        "the same key without its cascade",
    );

    // A check is nothing but an expression, and the engine rewrites it:
    // its name is all a read-back can be held to (183).
    let adding_check = plan(pbps_model::Change::AddCheck {
        table: dbo_t.clone(),
        name: "ck".into(),
        constraint: pbps_model::CheckConstraint {
            expression: "id > 0".to_owned(),
        },
    });
    check(
        &adding_check,
        &with(&|t| {
            t.checks.insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: "([id]>(0))".to_owned(),
                },
            );
        }),
    )
    .expect("the engine's rendering of the check");
}

/// The remedy a staged refusal names depends on which read found the
/// change. At a checkpoint the change is in the record and a resume goes
/// on; at the closing read nothing records it and a resume refuses the
/// database as moved — so telling the operator to resume there sent them
/// down a path that cannot work (DECISIONS 190).
#[test]
fn a_closing_staged_refusal_does_not_promise_a_resume() {
    let declared = pbps_model::Table {
        columns: [("id".into(), pbps_model::Column::new("int".parse().unwrap()))].into(),
        ..Default::default()
    };
    let schema = |rows: &[&str]| {
        let t = pbps_model::Table {
            data: Some(pbps_model::TableData {
                mode: pbps_model::DataMode::Exact,
                rows: rows
                    .iter()
                    .map(|k| (pbps_model::RowKey::from(*k), pbps_model::Row::default()))
                    .collect(),
            }),
            ..Default::default()
        };
        let mut s = Schema::default();
        s.tables.insert("dbo.other".parse().unwrap(), t);
        s.tables
            .insert("dbo.planned".parse().unwrap(), declared.clone());
        s
    };
    // The data table is untouched while one other table is created.
    let untouched = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.planned".parse().unwrap(),
                table: Box::new(declared.clone()),
                beside_default: None,
                moves_at_most: None,
            },
        )],
    };
    let refusal_for = |environment: Option<&str>, read: StagedRead| {
        staged_movement(
            &pbps_mssql::Mssql,
            &untouched,
            &schema(&["kept"]),
            &schema(&["kept"]),
            "prod",
            environment,
            read,
        )
        .expect("the unrelated create leaves the data table unchanged");
        let e = staged_movement(
            &pbps_mssql::Mssql,
            &untouched,
            &schema(&["kept"]),
            &schema(&["kept", "rogue"]),
            "prod",
            environment,
            read,
        )
        .expect_err("a row nobody planned");
        format!("{e:#}")
    };
    let refusal = |read: StagedRead| refusal_for(Some("prod"), read);
    let at_checkpoint = refusal(StagedRead::Checkpoint {
        completed: 1,
        total: 2,
    });
    assert!(
        at_checkpoint.contains("resuming accepts it"),
        "{at_checkpoint}"
    );
    assert!(at_checkpoint.contains("1 of 2"), "{at_checkpoint}");
    // The checkpoint already recorded the movement, so `verify` compares
    // against a baseline that holds it and reads clean: the refusal must
    // not send the operator there to see it. Its own list is the record,
    // and `status` shows it again as the failed entry's reason.
    assert!(
        !at_checkpoint.contains("`pbps verify` shows"),
        "{at_checkpoint}"
    );
    assert!(at_checkpoint.contains("`pbps status`"), "{at_checkpoint}");
    let at_close = refusal(StagedRead::Closing { total: 2 });
    assert!(!at_close.contains("resuming accepts"), "{at_close}");
    assert!(at_close.contains("no checkpoint holds it"), "{at_close}");
    assert!(at_close.contains("`--resume` will refuse"), "{at_close}");
    // No checkpoint holds a change that landed after the last one, so at
    // the closing read `verify` does show it, and the pointer stays.
    assert!(at_close.contains("`pbps verify` shows"), "{at_close}");
    // Both say what moved and that nothing was undone — and neither
    // carries the transactional remedy, which used to sit one line above
    // "nothing was rolled back" in every staged refusal.
    for e in [&at_checkpoint, &at_close] {
        assert!(e.contains("dbo.other"), "{e}");
        assert!(e.contains("nothing was rolled back"), "{e}");
        assert!(!e.contains("transaction was rolled back"), "{e}");
        assert!(!e.contains("then apply again"), "{e}");
    }
}

/// A `--db` run is told where the record is, and sent to no command that
/// cannot take one.
///
/// `status` reports on the environments `pbps.yml` configures and accepts
/// no target at all, so for a target given as a connection string the
/// pointer added with DECISIONS 192's sibling led nowhere: to other
/// databases, or to "no environments are configured". The sentence has to
/// stand without it, because for half the callers there is nothing to name
/// (DECISIONS 196).
#[test]
fn a_checkpoint_refusal_names_status_only_to_a_caller_who_can_run_it() {
    let declared = pbps_model::Table {
        columns: [("id".into(), pbps_model::Column::new("int".parse().unwrap()))].into(),
        ..Default::default()
    };
    let schema = |rows: &[&str]| {
        let t = pbps_model::Table {
            data: Some(pbps_model::TableData {
                mode: pbps_model::DataMode::Exact,
                rows: rows
                    .iter()
                    .map(|k| (pbps_model::RowKey::from(*k), pbps_model::Row::default()))
                    .collect(),
            }),
            ..Default::default()
        };
        let mut s = Schema::default();
        s.tables.insert("dbo.other".parse().unwrap(), t);
        s.tables
            .insert("dbo.planned".parse().unwrap(), declared.clone());
        s
    };
    // The data table is untouched while one other table is created.
    let untouched = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: "dbo.planned".parse().unwrap(),
                table: Box::new(declared.clone()),
                beside_default: None,
                moves_at_most: None,
            },
        )],
    };
    let refusal = |environment: Option<&str>| {
        staged_movement(
            &pbps_mssql::Mssql,
            &untouched,
            &schema(&["kept"]),
            &schema(&["kept"]),
            "localhost/app",
            environment,
            StagedRead::Checkpoint {
                completed: 1,
                total: 2,
            },
        )
        .expect("the unchanged rows do not produce a recovery message");
        let e = staged_movement(
            &pbps_mssql::Mssql,
            &untouched,
            &schema(&["kept"]),
            &schema(&["kept", "rogue"]),
            "localhost/app",
            environment,
            StagedRead::Checkpoint {
                completed: 1,
                total: 2,
            },
        )
        .expect_err("a row nobody planned");
        format!("{e:#}")
    };

    let from_db = refusal(None);
    assert!(!from_db.contains("pbps status"), "{from_db}");
    // What is left has to be an answer on its own: where the record is, and
    // that the command which *can* be run against this target reads clean.
    assert!(from_db.contains("the record of what moved"), "{from_db}");
    assert!(
        from_db.contains("recorded as the failed entry's reason"),
        "{from_db}"
    );
    assert!(from_db.contains("`pbps verify` reads clean"), "{from_db}");
    assert!(from_db.contains("dbo.other"), "{from_db}");

    // `--env`: the environment is one `status` walks, so it is named.
    let from_env = refusal(Some("prod"));
    assert!(from_env.contains("`pbps status` shows"), "{from_env}");
}

/// A cell the plan leaves to its default is held there at the closing
/// read. The read-back omits a cell the engine confirmed at its default,
/// so a present cell on such a column is one somebody else wrote — and
/// dropping the expectation because the value could not be named meant a
/// rewrite between a staged `UPDATE` and its checkpoint was recorded as
/// the plan's result. Only at the closing read, and only on a column
/// whose default the engine confirms: a `NEWID()` cell is present on
/// every read, and a checkpoint read spells cells after the checkpoint
/// before it (DECISIONS 191).
#[test]
fn a_cell_left_to_its_default_is_held_there_at_the_close() {
    use pbps_model::{Cell, DataMode, RowKey, TableData, Value};
    let dbo_t: TableName = "dbo.t".parse().unwrap();
    let schema = |cells: &[(&str, &str)]| {
        let column = |ty: &str, default: Option<&str>| {
            let mut c = pbps_model::Column::new(ty.parse().unwrap());
            c.value_source = default.map_or(pbps_model::ValueSource::None, |text| {
                pbps_model::ValueSource::Default(text.into())
            });
            c
        };
        let mut t = pbps_model::Table::default();
        t.columns.insert("code".into(), column("varchar(20)", None));
        t.columns
            .insert("note".into(), column("nvarchar(50)", Some("('n/a')")));
        t.columns
            .insert("tag".into(), column("uniqueidentifier", Some("(newid())")));
        t.data = Some(TableData {
            mode: DataMode::Exact,
            rows: [(
                RowKey::from("k"),
                cells
                    .iter()
                    .map(|(c, v)| ((*c).into(), Value::Text((*v).to_owned())))
                    .collect::<pbps_model::Row>(),
            )]
            .into_iter()
            .collect(),
        });
        let mut s = Schema::default();
        s.tables.insert(dbo_t.clone(), t);
        s
    };
    let plan = |change: pbps_model::Change| pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(change)],
    };
    // The declaration stops spelling `note`, so the plan sets it to
    // DEFAULT; `tag` was always left to its `NEWID()`.
    let defaulting = plan(pbps_model::Change::UpdateRow {
        table: dbo_t.clone(),
        key_column: "code".into(),
        key: RowKey::from("k"),
        columns: [(
            "note".into(),
            (
                Cell::Value(Value::Text("custom".to_owned())),
                Cell::Default("'n/a'".to_owned()),
            ),
        )]
        .into_iter()
        .collect(),
        unchanged: [("tag".into(), Cell::Default("NEWID()".to_owned()))]
            .into_iter()
            .collect(),
        types: Default::default(),
        after_types: Default::default(),
        key_type: None,
    });
    let before = schema(&[("note", "custom")]);
    let check = |after: &Schema, settled: Settled| {
        staged_comparison(
            &pbps_mssql::Mssql,
            &defaulting,
            &before,
            after,
            "prod",
            settled,
        )
    };
    // At the close, the cell is omitted: at its default, as planned. The
    // `NEWID()` cell is there with its value, and that proves nothing.
    check(
        &schema(&[("tag", "6F9619FF-8B86-D011-B42D-00C04FC964FF")]),
        Settled::Closing,
    )
    .expect("at its default, and an unconfirmable default beside it");
    // Present on a column the engine confirms: somebody wrote it.
    let e = check(&schema(&[("note", "rogue")]), Settled::Closing)
        .expect_err("a value where the plan left the default");
    assert!(format!("{e:#}").contains("`note`"), "{e:#}");
    assert!(
        format!("{e:#}").contains("leaves it at its default"),
        "{e:#}"
    );
    // The same read-back at the final checkpoint says nothing: that read
    // spells a cell the way the checkpoint before it did, so a cell at
    // its default may well be present there.
    check(&schema(&[("note", "rogue")]), Settled::Whole)
        .expect("a checkpoint read cannot tell at-default from a value");

    // The transactional read is plan-scoped too; its API must not lose
    // closing-read default evidence while separating it from checkpoints.
    for (note, accepted) in [(None, true), (Some("rogue"), false)] {
        let cells = note.map(|value| vec![("note", value)]).unwrap_or_default();
        let result = refuse_unplanned_movement(
            &pbps_mssql::Mssql,
            &defaulting,
            &before,
            &schema(&cells),
            "prod",
            Settled::Closing,
        );
        if accepted {
            result.expect("transactional omission proves the planned default");
        } else {
            let error = result.expect_err("transactional closing still holds default cells");
            assert!(
                format!("{error:#}").contains("leaves it at its default"),
                "{error:#}"
            );
        }
    }

    // An insert leaves columns to their defaults too.
    let inserting = plan(pbps_model::Change::InsertRow {
        table: dbo_t.clone(),
        key_column: "code".into(),
        identity_key: false,
        key: RowKey::from("k"),
        row: Default::default(),
        defaults: [("note".into(), "'n/a'".to_owned())].into_iter().collect(),
        types: Default::default(),
    });
    let empty = Schema::default();
    let e = staged_comparison(
        &pbps_mssql::Mssql,
        &inserting,
        &empty,
        &schema(&[("note", "rogue")]),
        "prod",
        Settled::Closing,
    )
    .expect_err("inserted at its default, read back with a value");
    assert!(format!("{e:#}").contains("`note`"), "{e:#}");
    staged_comparison(
        &pbps_mssql::Mssql,
        &inserting,
        &empty,
        &schema(&[]),
        "prod",
        Settled::Closing,
    )
    .expect("inserted at its default, read back omitted");
}
