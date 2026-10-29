use super::*;
use crate::diagnostic::Declaration;
use crate::operation::{Origin, Owners, PlanOwners, Spelling};
use crate::{
    Change, Column, IdsFile, Module, ModuleDeps, ModuleKind, PlannedChange, Schema, Table, Uid,
};
use std::collections::{BTreeMap, BTreeSet};

fn reference(s: &str) -> ObjectRef {
    s.parse().unwrap()
}
fn dependencies(rows: &[(&str, &[&str])]) -> ObjectDeps {
    ObjectDeps(
        rows.iter()
            .map(|(owner, targets)| {
                (
                    reference(owner),
                    targets.iter().map(|s| reference(s)).collect(),
                )
            })
            .collect(),
    )
}
fn module(kind: ModuleKind) -> Module {
    Module {
        kind,
        definition: "SELECT 1".into(),
        description: None,
        options: Default::default(),
    }
}
fn table() -> Table {
    let mut column = Column::new("integer".parse().unwrap()).not_null();
    column.value_source = crate::ValueSource::Default("1".into());
    Table {
        columns: [("x".into(), column)].into(),
        ..Table::default()
    }
}
fn fixture() -> Schema {
    let mut t = table();
    t.primary_key = Some(serde_json::from_str(r#"{"columns":["x"]}"#).unwrap());
    t.unique.insert(
        "u".into(),
        serde_json::from_str(r#"{"columns":["x"]}"#).unwrap(),
    );
    t.foreign_keys.insert(
        "fk".into(),
        serde_json::from_str(
            r#"{"columns":["x"],"references_table":"app.t","references_columns":["x"]}"#,
        )
        .unwrap(),
    );
    t.checks.insert(
        "same".into(),
        crate::CheckConstraint {
            expression: "x > 0".into(),
        },
    );
    t.indexes.insert(
        "same".into(),
        serde_json::from_str(r#"{"columns":[{"name":"x"}]}"#).unwrap(),
    );
    t.row_policies
        .insert("p".into(), serde_json::from_str("{}").unwrap());
    t.computed.insert(
        "c".into(),
        serde_json::from_str(r#"{"expression":"x+1"}"#).unwrap(),
    );
    let mut serial = Column::new("integer".parse().unwrap());
    serial.value_source = crate::ValueSource::Sequence(crate::OwnedSequence::serial());
    t.columns.insert("id".into(), serial);
    let mut generation = Column::new("integer".parse().unwrap());
    generation.value_source = crate::ValueSource::Generated(crate::Generated {
        expression: "x+1".into(),
        stored: true,
    });
    t.columns.insert("g".into(), generation);
    let mut view = module(ModuleKind::View);
    view.options.indexes = t.indexes.clone();
    Schema {
        tables: [("app.t".parse().unwrap(), t)].into(),
        types: [(
            "app.d".parse().unwrap(),
            crate::ManagedType::Domain {
                base: "integer".parse().unwrap(),
                not_null: true,
                default: Some("1".into()),
                checks: [("ck".into(), "VALUE > 0".into())].into(),
                collation: None,
            },
        )]
        .into(),
        modules: [
            ("app.v".parse().unwrap(), view),
            (
                "app.f(integer)".parse().unwrap(),
                module(ModuleKind::Function),
            ),
        ]
        .into(),
        roles: [("reader".into(), Default::default())].into(),
    }
}

#[test]
fn every_prefix_has_one_typed_identity_and_round_trips() {
    for s in [
        "app.v",
        "app.f(integer)",
        "app.t.tr",
        "table::app.t",
        "column::app.t.x",
        "module::app.v",
        "module::app.f(integer)",
        "module::table::app.v",
        "type::app.d",
        "role::reader",
        "default::app.t.x",
        "generation::app.t.g",
        "sequence::app.t.id",
        "not_null::app.t.x",
        "default::type::app.d",
        "not_null::type::app.d",
        "check::type::app.d.ck",
        "index::app.t.same",
        "index::module::app.v.same",
        "check::app.t.same",
        "unique::app.t.u",
        "foreign_key::app.t.fk",
        "primary_key::app.t",
        "policy::app.t.p",
        "row_security::app.t",
    ] {
        let r = reference(s);
        assert_eq!(reference(&r.raw()), r, "{s}");
    }
    assert_ne!(
        reference("index::app.t.same"),
        reference("check::app.t.same")
    );
    assert_eq!(reference("app.t.tr"), reference("module::app.t.tr"));
    for bad in [
        "table::",
        "column::app.t",
        "type::",
        "unknown::app.v",
        "index::app.t.",
        "index::app.t",
        "role::",
        "::app.t",
        "a..b",
        "module::app",
        "generation::app.t.",
    ] {
        assert!(bad.parse::<ObjectRef>().is_err(), "{bad}");
    }
    assert!(
        "unknown::app.v"
            .parse::<ObjectRef>()
            .unwrap_err()
            .to_string()
            .contains("unknown reference prefix")
    );
    assert!(
        "index::app.t."
            .parse::<ObjectRef>()
            .unwrap_err()
            .to_string()
            .contains("empty member")
    );
}

#[test]
fn structured_escape_preserves_every_local_name_without_a_second_grammar() {
    let atoms = [
        "a", ".", "::", "\"", "'", "\n", "\r", "\u{1b}", "\\", "é", "表", "null", "[a,b]",
    ];
    for a in atoms {
        for b in atoms {
            let name = format!("{a}{b}");
            for part in [
                PartKey::Index(name.clone().into()),
                PartKey::Check(name.clone().into()),
                PartKey::Unique(name.clone().into()),
                PartKey::ForeignKey(name.clone().into()),
                PartKey::Policy(name.clone()),
                PartKey::Computed(name.clone().into()),
            ] {
                let r = ObjectRef {
                    root: Root::Table("app.t".parse().unwrap()),
                    part: Selector::Part(part),
                };
                assert_eq!(reference(&r.raw()), r, "{r:?}");
            }
        }
    }
    let r = ObjectRef {
        root: Root::Table("app.t".parse().unwrap()),
        part: Selector::Part(PartKey::Index("a.b".into())),
    };
    assert!(r.raw().starts_with("object::"));
    for bad in [
        r#"object::{"root":{"table":"app.t"},"part":{"index":""}}"#,
        r#"object::{"root":{"table":"app.t"},"part":{"index":"x","check":"y"}}"#,
        r#"object::{"root":{"role":""},"part":"whole"}"#,
        r#"object::{"root":{"table":"app.t"},"part":{"index":"x","index":"y"}}"#,
        r#"object::{"root":{"table":"app.t"},"part":"imaginary"}"#,
        r#"object::{"root":{"table":"app.t"},"part":"whole","other":1}"#,
    ] {
        assert!(bad.parse::<ObjectRef>().is_err(), "{bad}");
    }
}

#[test]
fn the_registry_enumerates_every_declared_surface_in_one_index() {
    let schema = fixture();
    let registry = Registry::new(&schema);
    for s in [
        "table::app.t",
        "column::app.t.x",
        "column::app.t.c",
        "default::app.t.x",
        "generation::app.t.g",
        "generation::app.t.c",
        "sequence::app.t.id",
        "not_null::app.t.x",
        "primary_key::app.t",
        "unique::app.t.u",
        "foreign_key::app.t.fk",
        "check::app.t.same",
        "index::app.t.same",
        "policy::app.t.p",
        "row_security::app.t",
        "type::app.d",
        "default::type::app.d",
        "not_null::type::app.d",
        "check::type::app.d.ck",
        "module::app.v",
        "module::app.f(integer)",
        "index::module::app.v.same",
        "role::reader",
    ] {
        assert!(registry.declaration(&reference(s)).is_ok(), "{s}");
    }
    assert_eq!(
        registry
            .declaration(&reference("default::app.t.x"))
            .unwrap(),
        &Declaration::Table("app.t".parse().unwrap())
    );
    assert!(matches!(
        registry.declaration(&reference("default::app.t.id")),
        Err(Problem::MissingPart(_))
    ));
    assert!(matches!(
        registry.declaration(&reference("sequence::app.t.x")),
        Err(Problem::MissingPart(_))
    ));
    assert!(matches!(
        registry.declaration(&reference("check::app.missing.ck")),
        Err(Problem::UnknownTarget(_))
    ));
    let wrong = ObjectRef {
        root: Root::Table("app.t".parse().unwrap()),
        part: Selector::Default,
    };
    assert!(matches!(
        registry.declaration(&wrong),
        Err(Problem::WrongSelector(_))
    ));
}

#[test]
fn owner_target_duplicate_self_and_cycle_failures_are_distinct() {
    let schema = fixture();
    let registry = Registry::new(&schema);
    for (deps, check) in [
        (dependencies(&[("app.absent", &["app.v"])]), "owner"),
        (dependencies(&[("app.v", &["app.absent"])]), "target"),
        (dependencies(&[("app.v", &["default::app.t.id"])]), "part"),
        (dependencies(&[("app.v", &["app.v"])]), "itself"),
        (
            dependencies(&[("app.v", &[]), ("module::app.v", &[])]),
            "duplicate",
        ),
    ] {
        let problems = registry.validate(&deps, None);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].to_string().contains(check), "{problems:?}");
    }
    let deps = dependencies(&[
        ("app.v", &["table::app.t"]),
        ("table::app.t", &["type::app.d"]),
        ("type::app.d", &["app.v"]),
        ("role::reader", &["app.v"]),
    ]);
    let problems = registry.validate(&deps, None);
    let [Problem::Cycle(legs)] = problems.as_slice() else {
        panic!("{problems:?}")
    };
    assert_eq!(legs.len(), 3);
    let text = problems[0].to_string();
    for name in ["module::app.v", "table::app.t", "type::app.d"] {
        assert!(text.contains(name), "{text}");
    }
    assert!(!text.contains("reader"));
    assert!(
        registry
            .validate(
                &dependencies(&[
                    ("app.v", &["table::app.t"]),
                    ("table::app.t", &["type::app.d"])
                ]),
                None
            )
            .is_empty()
    );
    let computed_alias = ObjectRef {
        root: Root::Table("app.t".parse().unwrap()),
        part: Selector::Part(PartKey::Computed("c".into())),
    };
    let same_generation = ObjectDeps(vec![(
        computed_alias,
        [reference("generation::app.t.c")].into(),
    )]);
    assert!(matches!(
        registry.validate(&same_generation, None).as_slice(),
        [Problem::SelfDependency(_)]
    ));
    // Different facets of the same root are legal; no whole-table self shortcut.
    assert!(
        registry
            .validate(
                &dependencies(&[("default::app.t.x", &["column::app.t.x"])]),
                None
            )
            .is_empty()
    );
}

fn normalize(r: &ObjectRef) -> ObjectRef {
    let mut r = r.clone();
    if let Root::Module(id) = &r.root {
        let spelling = id.raw().to_string().replace("(int)", "(integer)");
        r.root = Root::Module(spelling.parse().unwrap());
    }
    r
}

#[test]
fn declaration_ambiguity_and_duplicate_owners_are_checked_after_normalization() {
    let mut schema = fixture();
    let deps = dependencies(&[
        ("app.f(int)", &["app.v"]),
        ("app.f(integer)", &["table::app.t"]),
    ]);
    assert!(
        Registry::new(&schema)
            .declaration(&reference("app.f(int)"))
            .is_err()
    );
    let registry = Registry::normalized(&schema, normalize);
    assert!(matches!(
        registry.validate(&deps, None).as_slice(),
        [Problem::DuplicateOwner(_)]
    ));
    schema
        .modules
        .insert("app.f(int)".parse().unwrap(), module(ModuleKind::Function));
    assert!(
        Registry::new(&schema)
            .validate(&dependencies(&[("app.v", &["app.f(int)"])]), None)
            .is_empty()
    );
    let registry = Registry::normalized(&schema, normalize);
    assert!(matches!(
        registry
            .validate(&dependencies(&[("app.v", &["app.f(int)"])]), None)
            .as_slice(),
        [Problem::Ambiguous(_)]
    ));
}

#[test]
fn typed_collisions_and_external_assumptions_never_become_guessed_local_targets() {
    let mut schema = fixture();
    schema
        .modules
        .insert("table::app.t".parse().unwrap(), module(ModuleKind::View));
    let registry = Registry::new(&schema);
    let external = [(
        "otherdb.dbo.t".parse().unwrap(),
        crate::dependency::Dependency {
            kind: crate::dependency::DependencyKind::Table,
            columns: BTreeMap::new(),
        },
    )]
    .into();
    assert!(
        registry
            .parse("table::app.t", &external)
            .unwrap_err()
            .contains("ambiguous")
    );
    assert_eq!(
        registry.parse("module::table::app.t", &external).unwrap(),
        reference("module::table::app.t")
    );
    let object = r#"object::{"root":{"table":"app.t"},"part":"whole"}"#;
    assert_eq!(
        registry.parse(object, &external).unwrap(),
        reference("table::app.t")
    );
    let rendered = registry.render(&reference("table::app.t")).unwrap();
    assert!(rendered.starts_with("object::"));
    assert_eq!(
        registry.parse(&rendered, &external).unwrap(),
        reference("table::app.t")
    );
    let error = registry
        .parse("table::otherdb.dbo.t", &external)
        .unwrap_err();
    assert!(error.contains("in another database"), "{error}");
    assert!(
        error.contains("deploy its prerequisite there first"),
        "{error}"
    );
    assert!(
        registry
            .parse("module::otherdb.dbo.t", &external)
            .unwrap_err()
            .contains("in another database")
    );
    // The three components of a local column or trigger are not database qualification.
    assert_eq!(
        registry.parse("column::app.t.x", &external).unwrap(),
        reference("column::app.t.x")
    );
    assert_eq!(
        registry.parse("module::app.t.tr", &external).unwrap(),
        reference("app.t.tr")
    );
    assert!(
        registry
            .parse("unknown::app.v", &external)
            .unwrap_err()
            .contains("unknown reference prefix")
    );
}

#[test]
fn every_legacy_module_identity_family_round_trips_without_wire_changes() {
    let mut names = vec![
        "app.v".to_owned(),
        "app.t.tr".into(),
        "table::app.v".into(),
        "unknown::app.v".into(),
        "app.f()".into(),
        "app.f( INTEGER )".into(),
        "app.f(numeric(12, 2),app.mood[],\"App\".\"a,b\")".into(),
        "app.f(timestamp with time zone, double precision)".into(),
    ];
    for schema in ["app", "quoted schema", "表", "a\nb", "x::y"] {
        for local in ["v", "a b", "é", "null", "a\r\u{1b}"] {
            names.push(format!("{schema}.{local}"));
            names.push(format!("{schema}.{local}.tr"));
        }
    }
    let ids: BTreeSet<ModuleId> = names.iter().map(|s| s.parse().unwrap()).collect();
    let deps: ModuleDeps = ids.iter().map(|id| (id.clone(), ids.clone())).collect();
    let before = serde_json::to_vec(&deps).unwrap();
    let refs = ObjectDeps::from_legacy(&deps);
    assert_eq!(
        serde_json::to_vec(&refs.to_legacy().unwrap()).unwrap(),
        before
    );
    for id in &ids {
        let r = ObjectRef::legacy(&id.raw().to_string()).unwrap();
        assert_eq!(r.legacy_spelling().unwrap(), id.raw().to_string());
        assert_eq!(reference(&r.raw()), r);
    }
    let empty: ModuleDeps = [("app.empty".parse().unwrap(), BTreeSet::new())].into();
    assert_eq!(ObjectDeps::from_legacy(&empty).to_legacy().unwrap(), empty);
    assert!(
        dependencies(&[("app.v", &["table::app.t"])])
            .to_legacy()
            .is_err()
    );
    assert!(
        dependencies(&[("table::app.t", &["app.v"])])
            .to_legacy()
            .is_err()
    );
    assert!(
        dependencies(&[("app.v", &[]), ("app.v", &[])])
            .to_legacy()
            .is_err()
    );
}

#[test]
fn renames_keep_recorded_edges_and_reused_addresses_have_new_anchors() {
    let old: TableName = "app.old".parse().unwrap();
    let new: TableName = "app.new".parse().unwrap();
    let tu: Uid = "t_aaaaaa".parse().unwrap();
    let cu: Uid = "c_aaaaaa".parse().unwrap();
    let replacement: Uid = "t_bbbbbb".parse().unwrap();
    let replacement_column: Uid = "c_bbbbbb".parse().unwrap();
    let base_ids = IdsFile {
        tables: [(tu.clone(), old.clone())].into(),
        columns: [(cu.clone(), old.column("x"))].into(),
        ..Default::default()
    };
    let goal_ids = IdsFile {
        tables: [
            (tu.clone(), new.clone()),
            (replacement.clone(), old.clone()),
        ]
        .into(),
        columns: [
            (cu.clone(), new.column("y")),
            (replacement_column, old.column("x")),
        ]
        .into(),
        ..Default::default()
    };
    let changes = vec![
        PlannedChange::new(Change::RenameTable {
            uid: tu,
            from: old.clone(),
            to: new.clone(),
            defaults: vec![],
        }),
        PlannedChange::new(Change::RenameColumn {
            uid: cu,
            table: new.clone(),
            from: "x".into(),
            to: "y".into(),
            table_was: Some(old.clone()),
        }),
        PlannedChange::new(Change::CreateTable {
            uid: replacement,
            name: old.clone(),
            table: Box::new(table()),
            beside_default: None,
            moves_at_most: None,
        }),
    ];
    let names = crate::plan_names::PlanNames::of(&changes, &goal_ids).unwrap();
    let owners = PlanOwners::new(names, &base_ids, &goal_ids);
    let base = Schema {
        tables: [(old.clone(), table())].into(),
        ..Default::default()
    };
    let mut renamed = table();
    let column = renamed.columns.shift_remove("x").unwrap();
    renamed.columns.insert("y".into(), column);
    let goal = Schema {
        tables: [(new.clone(), renamed), (old.clone(), table())].into(),
        ..Default::default()
    };
    let before = Registry::new(&base)
        .bind(&reference("default::app.old.x"), &owners, Spelling::Base)
        .unwrap();
    let after = Registry::new(&goal)
        .bind(&reference("default::app.new.y"), &owners, Spelling::Final)
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        before.root,
        BoundRoot::Recorded(base_ids.columns.keys().next().unwrap().clone())
    );
    assert_ne!(
        before,
        Registry::new(&goal)
            .bind(&reference("default::app.old.x"), &owners, Spelling::Final)
            .unwrap()
    );
    let intermediate = Schema {
        tables: [(new, table())].into(),
        ..Default::default()
    };
    let at = Origin::Whole(1).spelling();
    assert_eq!(
        before,
        Registry::new(&intermediate)
            .bind(&reference("default::app.new.x"), &owners, at)
            .unwrap()
    );
    assert!(
        Registry::new(&goal)
            .bind(&reference("default::app.new.y"), &owners, at)
            .is_err()
    );
}

#[test]
fn missing_recorded_identity_is_an_error_even_for_a_declared_surface() {
    let schema = fixture();
    let ids = IdsFile::default();
    let names = crate::plan_names::PlanNames::of(&[], &ids).unwrap();
    let owners = PlanOwners::new(names, &ids, &ids);
    for s in [
        "table::app.t",
        "column::app.t.x",
        "type::app.d",
        "role::reader",
    ] {
        assert!(
            matches!(
                Registry::new(&schema).bind(&reference(s), &owners, Spelling::Final),
                Err(Problem::Unrecorded { .. })
            ),
            "{s}"
        );
    }
    assert!(
        Registry::new(&schema)
            .bind(&reference("app.v"), &owners, Spelling::Final)
            .is_ok()
    );
}

#[test]
fn self_dependency_compares_bound_identity_rather_than_spelled_names() {
    struct Alias(Uid);
    impl Owners for Alias {
        fn table(&self, _: &TableName, _: Spelling) -> Result<Uid, crate::operation::OwnerError> {
            Ok(self.0.clone())
        }
        fn column(&self, _: &ColumnRef, _: Spelling) -> Result<Uid, crate::operation::OwnerError> {
            unreachable!()
        }
        fn role(&self, _: &RoleName, _: Spelling) -> Result<Uid, crate::operation::OwnerError> {
            unreachable!()
        }
    }
    let schema = Schema {
        tables: [
            ("app.a".parse().unwrap(), table()),
            ("app.b".parse().unwrap(), table()),
        ]
        .into(),
        ..Default::default()
    };
    let registry = Registry::new(&schema);
    let deps = dependencies(&[("table::app.a", &["table::app.b"])]);
    assert!(registry.validate(&deps, None).is_empty());
    let aliases = Alias("t_aaaaaa".parse().unwrap());
    assert!(matches!(
        registry
            .validate(&deps, Some((&aliases, Spelling::Final)))
            .as_slice(),
        [Problem::SelfDependency(_)]
    ));
}

#[test]
fn inherited_columns_follow_the_parent_column_uid_through_renames() {
    let parent: TableName = "app.parent".parse().unwrap();
    let renamed: TableName = "app.renamed".parse().unwrap();
    let child: TableName = "app.child".parse().unwrap();
    let parent_uid: Uid = "t_aaaaaa".parse().unwrap();
    let child_uid: Uid = "t_bbbbbb".parse().unwrap();
    let column_uid: Uid = "c_aaaaaa".parse().unwrap();
    let base_ids = IdsFile {
        tables: [
            (parent_uid.clone(), parent.clone()),
            (child_uid.clone(), child.clone()),
        ]
        .into(),
        columns: [(column_uid.clone(), parent.column("x"))].into(),
        ..Default::default()
    };
    let goal_ids = IdsFile {
        tables: [
            (parent_uid.clone(), renamed.clone()),
            (child_uid, child.clone()),
        ]
        .into(),
        columns: [(column_uid.clone(), renamed.column("y"))].into(),
        ..Default::default()
    };
    let changes = [
        PlannedChange::new(Change::RenameTable {
            uid: parent_uid,
            from: parent.clone(),
            to: renamed.clone(),
            defaults: vec![],
        }),
        PlannedChange::new(Change::RenameColumn {
            uid: column_uid,
            table: renamed.clone(),
            from: "x".into(),
            to: "y".into(),
            table_was: Some(parent.clone()),
        }),
    ];
    let owners = PlanOwners::new(
        crate::plan_names::PlanNames::of(&changes, &goal_ids).unwrap(),
        &base_ids,
        &goal_ids,
    );
    let partition = |parent| Table {
        partition_of: Some(crate::PartitionOf {
            parent,
            bound: crate::PartitionBound::Default,
            columns: Default::default(),
        }),
        ..Default::default()
    };
    let base = Schema {
        tables: [
            (parent.clone(), table()),
            (child.clone(), partition(parent)),
        ]
        .into(),
        ..Default::default()
    };
    let mut goal_parent = table();
    let c = goal_parent.columns.shift_remove("x").unwrap();
    goal_parent.columns.insert("y".into(), c);
    let goal = Schema {
        tables: [(renamed.clone(), goal_parent), (child, partition(renamed))].into(),
        ..Default::default()
    };
    let before = Registry::new(&base)
        .bind(&reference("default::app.child.x"), &owners, Spelling::Base)
        .unwrap();
    let after = Registry::new(&goal)
        .bind(&reference("default::app.child.y"), &owners, Spelling::Final)
        .unwrap();
    assert_eq!(before, after);
    assert!(
        matches!(&after.root, BoundRoot::InheritedColumn { table, column }
        if table == base_ids.table_uid(&"app.child".parse().unwrap()).unwrap()
        && column == base_ids.columns.keys().next().unwrap())
    );
    assert!(
        Registry::new(&goal)
            .declaration(&reference("column::app.child.x"))
            .is_err()
    );
}

#[test]
fn type_role_and_table_scoped_module_anchors_keep_their_recorded_identity() {
    let mut base = fixture();
    base.modules
        .insert("app.t.tr".parse().unwrap(), module(ModuleKind::Trigger));
    let mut goal = base.clone();
    let ty = goal.types.remove(&"app.d".parse().unwrap()).unwrap();
    goal.types.insert("app.renamed_d".parse().unwrap(), ty);
    let role = goal.roles.remove("reader").unwrap();
    goal.roles.insert("renamed_reader".into(), role);
    let table = goal.tables.remove(&"app.t".parse().unwrap()).unwrap();
    goal.tables.insert("app.renamed_t".parse().unwrap(), table);
    let trigger = goal.modules.remove(&"app.t.tr".parse().unwrap()).unwrap();
    goal.modules
        .insert("app.renamed_t.tr".parse().unwrap(), trigger);
    let base_ids = IdsFile {
        tables: [("t_aaaaaa".parse().unwrap(), "app.t".parse().unwrap())].into(),
        types: [("y_aaaaaa".parse().unwrap(), "app.d".parse().unwrap())].into(),
        roles: [("r_aaaaaa".parse().unwrap(), "reader".into())].into(),
        ..Default::default()
    };
    let goal_ids = IdsFile {
        tables: [(
            "t_aaaaaa".parse().unwrap(),
            "app.renamed_t".parse().unwrap(),
        )]
        .into(),
        types: [(
            "y_aaaaaa".parse().unwrap(),
            "app.renamed_d".parse().unwrap(),
        )]
        .into(),
        roles: [("r_aaaaaa".parse().unwrap(), "renamed_reader".into())].into(),
        ..Default::default()
    };
    let changes = [
        PlannedChange::new(Change::RenameTable {
            uid: "t_aaaaaa".parse().unwrap(),
            from: "app.t".parse().unwrap(),
            to: "app.renamed_t".parse().unwrap(),
            defaults: vec![],
        }),
        PlannedChange::new(Change::RenameRole {
            uid: "r_aaaaaa".parse().unwrap(),
            from: "reader".into(),
            to: "renamed_reader".into(),
        }),
    ];
    let owners = PlanOwners::new(
        crate::plan_names::PlanNames::of(&changes, &goal_ids).unwrap(),
        &base_ids,
        &goal_ids,
    );
    for (before, after) in [
        ("type::app.d", "type::app.renamed_d"),
        ("role::reader", "role::renamed_reader"),
        ("app.t.tr", "app.renamed_t.tr"),
        ("index::app.t.same", "index::app.renamed_t.same"),
    ] {
        assert_eq!(
            Registry::new(&base)
                .bind(&reference(before), &owners, Spelling::Base)
                .unwrap(),
            Registry::new(&goal)
                .bind(&reference(after), &owners, Spelling::Final)
                .unwrap()
        );
    }
    assert!(
        Registry::new(&goal)
            .bind(&reference("type::app.d"), &owners, Spelling::Final)
            .is_err()
    );
}

#[test]
fn diagnostic_references_escape_controls_once_without_changing_identity() {
    let r = ObjectRef::legacy("a\nb.v").unwrap();
    assert_eq!(r.shown().to_string(), "module::a\\nb.v");
    assert_eq!(r.legacy_spelling().unwrap(), "a\nb.v");
    for problem in [
        Problem::UnknownOwner(r.clone()),
        Problem::MissingPart(r.clone()),
        Problem::Cycle(vec![(r.clone(), reference("app.v"))]),
    ] {
        let text = problem.to_string();
        assert!(text.contains("module::a\\nb.v"), "{text}");
        assert!(!text.contains('\n'));
    }
}

mod partitions;
