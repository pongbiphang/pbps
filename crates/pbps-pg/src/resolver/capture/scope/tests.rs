use super::*;
use serde_json::json;

fn catalog(extra: &[(&str, Vec<Value>)]) -> Catalog {
    let mut rows: BTreeMap<String, Vec<Row>> = properties::CLASSES
        .iter()
        .map(|class| ((*class).into(), Vec::new()))
        .collect();
    rows.insert(
        "pg_namespace".into(),
        vec![
            json!({"oid":1,"nspname":"app"})
                .as_object()
                .unwrap()
                .clone(),
        ],
    );
    rows.insert(
        "pg_type".into(),
        [
            json!({"oid":2,"typname":"a","typnamespace":1}),
            json!({"oid":3,"typname":"b","typnamespace":1}),
        ]
        .into_iter()
        .map(|row| row.as_object().unwrap().clone())
        .collect(),
    );
    for (class, members) in extra {
        rows.insert(
            (*class).into(),
            members
                .iter()
                .map(|row| row.as_object().unwrap().clone())
                .collect(),
        );
    }
    Catalog::new(rows).unwrap()
}

fn refusal(catalog: &Catalog) -> Uncovered {
    prepare(
        catalog,
        18,
        &CaptureScope {
            retained: BTreeSet::new(),
            candidates: BTreeSet::new(),
        },
    )
    .err()
    .expect("unrelated rows must still refuse")
}

#[test]
fn duplicate_logical_identity_outside_the_closure_is_refused() {
    let catalog = catalog(&[(
        "pg_proc",
        vec![
            json!({"oid":10,"proname":"unrelated","pronamespace":1,"proargtypes":[2]}),
            json!({"oid":11,"proname":"unrelated","pronamespace":1,"proargtypes":[2]}),
        ],
    )]);
    assert_eq!(
        refusal(&catalog),
        Uncovered::class("pg_proc", "duplicate logical identity")
    );
}

#[test]
fn unreadable_identity_outside_the_closure_is_refused() {
    for row in [
        json!({"oid":10,"proname":"unrelated","pronamespace":1}),
        json!({"oid":10,"proname":"unrelated","pronamespace":1,"proargtypes":[99]}),
        json!({"oid":10,"proname":"unrelated","pronamespace":1,"proargtypes":[0]}),
    ] {
        let catalog = catalog(&[("pg_proc", vec![row])]);
        assert_eq!(
            refusal(&catalog),
            Uncovered::class("pg_proc", "unreadable logical identity")
        );
    }
}

#[test]
fn duplicate_detection_uses_logical_parents_even_before_the_parent_class_is_validated() {
    let catalog = catalog(&[
        ("pg_am", vec![json!({"oid":20,"amname":"method"})]),
        (
            "pg_opfamily",
            vec![
                json!({"oid":21,"opfname":"family","opfnamespace":1,"opfmethod":20}),
                json!({"oid":22,"opfname":"family","opfnamespace":1,"opfmethod":20}),
            ],
        ),
        (
            "pg_amop",
            vec![
                json!({"oid":23,"amopfamily":21,"amoplefttype":2,"amoprighttype":2,"amopstrategy":1,"amoppurpose":"s"}),
                json!({"oid":24,"amopfamily":22,"amoplefttype":2,"amoprighttype":2,"amopstrategy":"1","amoppurpose":"s"}),
            ],
        ),
    ]);
    assert_eq!(
        refusal(&catalog),
        Uncovered::class("pg_amop", "duplicate logical identity")
    );
}

#[test]
fn validation_and_duplicates_keep_the_original_first_error() {
    let duplicate = json!({"oid":11,"proname":"same","pronamespace":1,"proargtypes":[]});
    let original = json!({"oid":10,"proname":"same","pronamespace":1,"proargtypes":[]});
    let unreadable = json!({"oid":12,"proname":"bad","pronamespace":1});
    let first = catalog(&[(
        "pg_proc",
        vec![original.clone(), duplicate.clone(), unreadable.clone()],
    )]);
    assert_eq!(
        refusal(&first),
        Uncovered::class("pg_proc", "duplicate logical identity")
    );
    let second = catalog(&[("pg_proc", vec![original, unreadable, duplicate])]);
    assert_eq!(
        refusal(&second),
        Uncovered::class("pg_proc", "unreadable logical identity")
    );
}

#[test]
fn candidate_index_keeps_overloads_and_source_order_and_records_absence() {
    let catalog = catalog(&[(
        "pg_proc",
        vec![
            json!({"oid":10,"proname":"z","pronamespace":1,"proargtypes":[2]}),
            json!({"oid":11,"proname":"a","pronamespace":1,"proargtypes":[2]}),
            json!({"oid":12,"proname":"z","pronamespace":1,"proargtypes":[3]}),
        ],
    )]);
    let index = Index::new(&catalog).unwrap();
    assert_eq!(index.named("pg_proc", Some("app"), None), [0, 1, 2]);
    assert_eq!(index.named("pg_proc", Some("app"), Some("z")), [0, 2]);
    assert_eq!(index.named("pg_proc", None, Some("z")), [0, 2]);
    assert!(index.named("pg_proc", Some("absent"), None).is_empty());
    assert!(
        index
            .named("pg_proc", Some("app"), Some("absent"))
            .is_empty()
    );
    for row in &catalog.rows["pg_proc"] {
        let id = catalog.identity("pg_proc", row).unwrap();
        let found = index.find(&catalog, &id).unwrap();
        assert_eq!(
            logical::number(&catalog.rows[found.class][found.index], "oid"),
            logical::number(row, "oid")
        );
    }
    let absent = ObjectIdentity {
        class: "pg_proc".into(),
        name: vec!["app".into(), "absent".into()],
        signature: vec![],
    };
    assert!(index.find(&catalog, &absent).is_none());
    assert!(
        prepare(
            &catalog,
            18,
            &CaptureScope {
                retained: BTreeSet::new(),
                candidates: BTreeSet::new()
            }
        )
        .unwrap()
        .members
        .is_empty()
    );
}

fn builder(catalog: &Catalog) -> Builder<'_> {
    Builder {
        catalog,
        major: 18,
        index: Index::new(catalog).unwrap(),
        selected: BTreeSet::new(),
        pending: VecDeque::new(),
        result: Prepared {
            members: BTreeMap::new(),
            bindings: BTreeMap::new(),
            candidates: BTreeMap::new(),
            limitations: BTreeSet::new(),
            addresses: BTreeMap::new(),
            render: render::Selection::default(),
        },
    }
}

#[test]
fn child_groups_keep_snapshot_order_including_system_columns_and_skip_only_dropped_columns() {
    let catalog = catalog(&[
        (
            "pg_class",
            vec![
                json!({"oid":10,"relname":"t","relnamespace":1}),
                json!({"oid":11,"relname":"u","relnamespace":1}),
            ],
        ),
        (
            "pg_attribute",
            vec![
                json!({"attrelid":10,"attnum":2,"attname":"second","attisdropped":false}),
                json!({"attrelid":11,"attnum":1,"attname":"other","attisdropped":false}),
                json!({"attrelid":10,"attnum":-1,"attname":"system","attisdropped":false}),
                json!({"attrelid":10,"attnum":3,"attname":"gone","attisdropped":true}),
                json!({"attrelid":10,"attnum":1,"attname":"first","attisdropped":false}),
            ],
        ),
    ]);
    let mut builder = builder(&catalog);
    builder.children("pg_class", 10).unwrap();
    assert_eq!(
        builder
            .pending
            .iter()
            .map(|id| id.name[0].as_str())
            .collect::<Vec<_>>(),
        ["second", "system", "first"]
    );
    builder.children("pg_class", 11).unwrap();
    assert_eq!(builder.pending.back().unwrap().name, ["other"]);
    builder.children("pg_class", 10).unwrap();
    assert_eq!(builder.pending.len(), 4);
}

#[test]
fn unreadable_child_membership_is_checked_only_at_its_original_traversal() {
    let catalog = catalog(&[("pg_range", vec![json!({"rngtypid":2})])]);
    // rngmultitypid is not part of identity: unrelated bad membership must
    // not become an earlier validation error.
    assert!(
        prepare(
            &catalog,
            18,
            &CaptureScope {
                retained: BTreeSet::new(),
                candidates: BTreeSet::new()
            }
        )
        .is_ok()
    );
    let mut builder = builder(&catalog);
    assert_eq!(
        builder.children("pg_type", 3),
        Err(Uncovered::class("pg_range", "unreadable child membership"))
    );
}

#[test]
fn a_shortlist_hash_collision_never_counts_as_identity_equality() {
    let catalog = catalog(&[(
        "pg_proc",
        vec![json!({"oid":10,"proname":"actual","pronamespace":1,"proargtypes":[]})],
    )]);
    let mut index = Index::new(&catalog).unwrap();
    let requested = ObjectIdentity {
        class: "pg_proc".into(),
        name: vec!["app".into(), "different".into()],
        signature: vec![],
    };
    index
        .identities
        .get_mut("pg_proc")
        .unwrap()
        .insert(logical::identity_key(&requested), vec![0]);
    assert!(index.find(&catalog, &requested).is_none());
}

fn every_class() -> Catalog {
    let examples = vec![
        ("pg_class", json!({"oid":10,"relname":"t","relnamespace":1})),
        (
            "pg_attribute",
            json!({"attrelid":10,"attnum":1,"attname":"c","attisdropped":false}),
        ),
        (
            "pg_proc",
            json!({"oid":20,"proname":"f","pronamespace":1,"proargtypes":[2]}),
        ),
        (
            "pg_operator",
            json!({"oid":21,"oprname":"!","oprnamespace":1,"oprleft":0,"oprright":2}),
        ),
        ("pg_cast", json!({"castsource":2,"casttarget":3})),
        (
            "pg_collation",
            json!({"collname":"coll","collnamespace":1,"collencoding":-1}),
        ),
        ("pg_language", json!({"lanname":"sql"})),
        ("pg_extension", json!({"extname":"ext"})),
        ("pg_roles", json!({"oid":30,"rolname":"r"})),
        (
            "pg_auth_members",
            json!({"roleid":30,"member":30,"grantor":30}),
        ),
        ("pg_db_role_setting", json!({"setdatabase":0,"setrole":0})),
        ("pg_rewrite", json!({"ev_class":10,"rulename":"rule"})),
        ("pg_attrdef", json!({"adrelid":10,"adnum":1})),
        (
            "pg_constraint",
            json!({"oid":31,"connamespace":1,"conrelid":10,"contypid":0,"conname":"key"}),
        ),
        ("pg_index", json!({"indexrelid":10})),
        ("pg_sequence", json!({"seqrelid":10})),
        ("pg_range", json!({"rngtypid":2})),
        ("pg_enum", json!({"enumtypid":2,"enumlabel":"label"})),
        ("pg_aggregate", json!({"aggfnoid":20})),
        ("pg_am", json!({"oid":32,"amname":"method"})),
        (
            "pg_opclass",
            json!({"opcname":"opc","opcnamespace":1,"opcmethod":32}),
        ),
        (
            "pg_opfamily",
            json!({"oid":33,"opfname":"opf","opfnamespace":1,"opfmethod":32}),
        ),
        (
            "pg_amop",
            json!({"amopfamily":33,"amoplefttype":2,"amoprighttype":3,"amopstrategy":1,"amoppurpose":"s"}),
        ),
        (
            "pg_amproc",
            json!({"amprocfamily":33,"amproclefttype":2,"amprocrighttype":3,"amprocnum":1}),
        ),
        ("pg_partitioned_table", json!({"partrelid":10})),
        ("pg_policy", json!({"polrelid":10,"polname":"policy"})),
        (
            "pg_trigger",
            json!({"tgrelid":10,"tgconstraint":31,"tgfoid":20,"tgisinternal":true}),
        ),
        ("pg_database", json!({"datname":"db"})),
        ("pg_parameter_acl", json!({"parname":"parameter"})),
        ("pg_tablespace", json!({"spcname":"space"})),
        ("pg_inherits", json!({"inhrelid":10,"inhparent":10})),
        ("pg_ts_parser", json!({"prsname":"parser","prsnamespace":1})),
        (
            "pg_ts_template",
            json!({"tmplname":"template","tmplnamespace":1}),
        ),
        (
            "pg_ts_dict",
            json!({"dictname":"dictionary","dictnamespace":1}),
        ),
        (
            "pg_ts_config",
            json!({"oid":34,"cfgname":"config","cfgnamespace":1}),
        ),
        (
            "pg_ts_config_map",
            json!({"mapcfg":34,"maptokentype":1,"mapseqno":1}),
        ),
    ];
    let extra: Vec<_> = examples
        .iter()
        .map(|(class, row)| (*class, vec![row.clone()]))
        .collect();
    catalog(&extra)
}

#[test]
fn the_identity_index_matches_logical_identity_for_every_class() {
    let catalog = every_class();
    let index = Index::new(&catalog).unwrap();
    let mut source_classes = BTreeMap::new();
    for &class in properties::CLASSES
        .iter()
        .filter(|&&class| class != "pg_depend")
    {
        assert!(
            !catalog.rows[class].is_empty(),
            "missing example for {class}"
        );
        for (position, row) in catalog.rows[class].iter().enumerate() {
            let mut identity = catalog.identity(class, row).unwrap();
            if let Some(previous) = source_classes.insert(identity.class.clone(), class) {
                assert_eq!(
                    previous, class,
                    "logical classes must have one catalog source"
                );
            }
            let found = index.find(&catalog, &identity).unwrap();
            assert!(found.class == class && found.index == position, "{class}");
            identity.name.push("absent extra name component".into());
            assert!(index.find(&catalog, &identity).is_none(), "{class}");
        }
    }
}

#[test]
fn candidate_index_matches_identity_predicates_for_every_candidate_class() {
    let catalog = every_class();
    let index = Index::new(&catalog).unwrap();
    for candidate in CandidateClass::ALL {
        let class = candidate.catalog();
        let global = matches!(candidate, CandidateClass::Cast | CandidateClass::Extension);
        let identities: Vec<_> = catalog.rows[class]
            .iter()
            .map(|row| catalog.identity(class, row).unwrap())
            .collect();
        let name_index = usize::from(!global);
        let first_name = identities[0].name.get(name_index).map(String::as_str);
        for namespace in [None, Some("app"), Some("absent")] {
            for name in [None, first_name, Some("absent")] {
                if global && namespace.is_some()
                    || candidate == CandidateClass::Cast && name.is_some()
                {
                    continue;
                }
                let expected: Vec<_> = identities
                    .iter()
                    .enumerate()
                    .filter_map(|(position, id)| {
                        (namespace.is_none_or(|ns| id.name.first().map(String::as_str) == Some(ns))
                            && name.is_none_or(|name| {
                                id.name.get(name_index).map(String::as_str) == Some(name)
                            }))
                        .then_some(position)
                    })
                    .collect();
                assert_eq!(
                    index.named(class, namespace, name),
                    expected,
                    "{class} {namespace:?} {name:?}"
                );
            }
        }
    }
}
