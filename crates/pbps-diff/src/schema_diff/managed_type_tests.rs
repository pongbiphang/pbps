use super::*;
use pbps_dialect::MinimalDialect;
use pbps_model::ManagedType;
use pbps_model::TypeChange;

fn schema(definition: ManagedType) -> Schema {
    let mut schema = Schema::default();
    schema.types.insert("app.t".parse().unwrap(), definition);
    schema
}

#[test]
fn type_identity_drives_creation_removal_and_rename_while_swaps_refuse() {
    let definition = ManagedType::Enum {
        values: vec!["ok".into()],
    };
    let a: pbps_model::TypeRef = "app.a".parse().unwrap();
    let b: pbps_model::TypeRef = "app.b".parse().unwrap();
    let uid: Uid = "y_000001".parse().unwrap();
    let base = Schema {
        types: [(a.clone(), definition.clone())].into(),
        ..Default::default()
    };
    let target = Schema {
        types: [(b.clone(), definition.clone())].into(),
        ..Default::default()
    };
    let base_ids = IdsFile {
        types: [(uid.clone(), a.clone())].into(),
        ..Default::default()
    };
    let ids = IdsFile {
        types: [(uid.clone(), b.clone())].into(),
        ..Default::default()
    };
    let run = |base: &Schema, base_ids: &IdsFile, desired: &Schema, ids: &IdsFile| {
        diff(
            Side {
                schema: base,
                ids: base_ids,
            },
            Side {
                schema: desired,
                ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
    };
    let renamed = run(&base, &base_ids, &target, &ids).unwrap();
    assert!(
        matches!(&renamed.changes[0].change, Change::Type(TypeChange::RenameType { uid: got, from, to, .. }) if got == &uid && from == &a && to == &b)
    );
    let created = run(&Schema::default(), &IdsFile::default(), &base, &base_ids).unwrap();
    assert!(matches!(
        created.changes[0].change,
        Change::TypeValue(_) | Change::Type(TypeChange::CreateType { .. })
    ));
    let dropped = run(&base, &base_ids, &Schema::default(), &IdsFile::default()).unwrap();
    assert!(matches!(
        dropped.changes[0].change,
        Change::Type(TypeChange::DropType { .. })
    ));
    assert_eq!(
        dropped.risks(),
        [pbps_model::change::RiskClass::Destructive].into()
    );
    let both = Schema {
        types: [(a.clone(), definition.clone()), (b.clone(), definition)].into(),
        ..Default::default()
    };
    let other: Uid = "y_000002".parse().unwrap();
    let old = IdsFile {
        types: [(uid.clone(), a.clone()), (other.clone(), b.clone())].into(),
        ..Default::default()
    };
    let swapped = IdsFile {
        types: [(uid, b), (other, a)].into(),
        ..Default::default()
    };
    assert!(
        run(&both, &old, &both, &swapped)
            .unwrap_err()
            .iter()
            .any(|e| e.to_string().contains("swap"))
    );
}

#[test]
fn type_attribute_changes_remain_refused_by_name() {
    let enumeration = ManagedType::Enum {
        values: vec!["sad".into(), "ok".into()],
    };
    let domain = ManagedType::Domain {
        base: "varchar(20)".parse().unwrap(),
        not_null: false,
        default: Some("'0'".into()),
        checks: [("digits".into(), "VALUE <> ''".into())].into(),
        collation: None,
    };
    let mut cases = Vec::new();
    for values in [
        vec!["sad", "ok", "happy"],
        vec!["sad"],
        vec!["ok", "sad"],
        vec!["blue", "ok"],
    ] {
        cases.push((
            schema(enumeration.clone()),
            schema(ManagedType::Enum {
                values: values.into_iter().map(str::to_owned).collect(),
            }),
        ));
    }
    for field in 0..8 {
        let mut changed = domain.clone();
        let ManagedType::Domain {
            base,
            not_null,
            default,
            checks,
            collation,
        } = &mut changed
        else {
            unreachable!()
        };
        match field {
            0 => *base = "text".parse().unwrap(),
            1 => *not_null = true,
            2 => *default = None,
            3 => *default = Some("'1'".into()),
            4 => checks.clear(),
            5 => {
                checks.insert("digits".into(), "VALUE <> 'x'".into());
            }
            6 => {
                checks.insert("extra".into(), "VALUE <> 'x'".into());
            }
            _ => *collation = Some("pg_catalog.\"C\"".parse().unwrap()),
        }
        cases.push((schema(domain.clone()), schema(changed)));
    }
    cases.push((schema(domain.clone()), schema(enumeration.clone())));
    for (base, declared) in cases {
        let ids = IdsFile::default();
        let errors = diff(
            Side {
                schema: &base,
                ids: &ids,
            },
            Side {
                schema: &declared,
                ids: &ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap_err();
        assert!(
            errors.iter().any(|error| {
                let message = error.to_string();
                message.contains("app.t")
                    && (message.contains("slice 3") || message.contains("slice 4b"))
            }),
            "{errors:?}"
        );
    }
    for definition in [enumeration, domain] {
        let base = schema(definition);
        let ids = IdsFile::default();
        let plan = diff(
            Side {
                schema: &base,
                ids: &ids,
            },
            Side {
                schema: &base,
                ids: &ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();
        assert!(plan.changes().is_empty());
    }
}

#[test]
fn an_empty_plan_cannot_keep_an_undeclared_type_identity() {
    let schema = Schema::default();
    let mut ids = IdsFile::default();
    ids.types
        .insert("y_000001".parse().unwrap(), "app.gone".parse().unwrap());
    let errors = diff(
        Side {
            schema: &schema,
            ids: &ids,
        },
        Side {
            schema: &schema,
            ids: &ids,
        },
        &MinimalDialect,
        &Hints::default(),
    )
    .unwrap_err();
    assert!(
        errors.iter().any(|error| {
            let message = error.to_string();
            message.contains("app.gone") && message.contains("its identity remains recorded")
        }),
        "{errors:?}"
    );
    let empty = IdsFile::default();
    assert!(
        diff(
            Side {
                schema: &schema,
                ids: &empty
            },
            Side {
                schema: &schema,
                ids: &empty
            },
            &MinimalDialect,
            &Hints::default()
        )
        .unwrap()
        .changes()
        .is_empty()
    );
}

#[test]
fn type_scope_leaves_unknown_types_to_the_unmanaged_policy() {
    let schema = schema(ManagedType::Enum { values: vec![] });
    let ids = IdsFile::default();
    let scoped = crate::scope(&schema, &ids, &Default::default(), &Default::default());
    assert!(scoped.schema.types.is_empty());
    assert_eq!(
        scoped.unmanaged_types,
        schema.types.keys().cloned().collect::<Vec<_>>()
    );
    let ids = crate::resolve(
        &schema,
        &ids,
        &[],
        &crate::Context {
            operator: "test".into(),
            today: "2026-10-23".into(),
        },
    )
    .unwrap()
    .ids;
    let scoped = crate::scope(&schema, &ids, &Default::default(), &Default::default());
    assert_eq!(scoped.schema.types, schema.types);
    assert!(scoped.unmanaged_types.is_empty());
    assert_eq!(crate::observed_ids(&schema, &ids).types, ids.types);
}
