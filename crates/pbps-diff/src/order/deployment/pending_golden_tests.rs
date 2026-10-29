//! The reference's pending DEFAULT inventory, before positional settlement or
//! spelling qualification. Full certified order/spelling remains DEC-2296.5.
use super::*;
use pbps_model::{Column, Module, ModuleKind, Table};

// Share the existing class input and explicit operation builder below diff;
// no schema-diff guard is bypassed, and these proposals stay non-executable.
#[allow(dead_code)]
#[path = "../../../../pbps-pg/src/engine_effects/scenes_test.rs"]
mod scenes;
use scenes::{KINDS, Motion, build, name, steps, uid};

#[test]
fn dependent_meetings_keep_the_reference_pending_default_templates() {
    let expected: BTreeMap<_, Vec<String>> = include_str!("pending_defaults.golden")
        .lines()
        .map(|line| {
            let (label, value) = line.split_once('\t').unwrap();
            (label, serde_json::from_str(value).unwrap())
        })
        .collect();
    let mut seen = BTreeSet::new();
    let id: ModuleId = "app.f(integer)".parse().unwrap();
    let module = |v| Module {
        kind: ModuleKind::Function,
        definition: format!(
            "CREATE FUNCTION app.f(a integer) RETURNS integer AS 'SELECT a + {v}' LANGUAGE sql;"
        ),
        description: None,
        options: Default::default(),
    };
    for kind in KINDS {
        for motion in [Motion::Attach, Motion::Detach, Motion::Drop] {
            for cut_final in [false, true] {
                for version in [160000, 180000] {
                    let mut s = build(kind, motion, cut_final);
                    let mut found = Vec::new();
                    for (schema, is_before) in [(&mut s.before, true), (&mut s.after, false)] {
                        schema
                            .modules
                            .insert(id.clone(), module(if is_before { 0 } else { 1 }));
                        for (table, shape) in &mut schema.tables {
                            if let Some(k) = shape.columns.get_mut("k") {
                                k.value_source = ValueSource::Default("app.f(1)".into());
                                if is_before {
                                    found.push(Dependent {
                                        described: format!("default of {}.k", table.shown()),
                                        holds: Holds::TablePart {
                                            table: table.clone(),
                                            part: Part::Default("k".into()),
                                        },
                                    });
                                }
                            }
                        }
                    }
                    // An unrelated renamed holder must still use its exact
                    // boundary, even while the moving family has two templates.
                    let old = name("app.u");
                    let new = name("app.u2");
                    let mut t = Table::default();
                    let mut n = Column::new("integer".parse().unwrap());
                    n.value_source = ValueSource::Default("app.f(1)".into());
                    t.columns.insert("n".into(), n);
                    s.before.tables.insert(old.clone(), t.clone());
                    s.after.tables.insert(new.clone(), t);
                    s.base_ids.tables.insert(uid("t_cccccc"), old.clone());
                    s.ids.tables.insert(uid("t_cccccc"), new.clone());
                    s.base_ids.columns.insert(uid("c_gggggg"), old.column("n"));
                    s.ids.columns.insert(uid("c_gggggg"), new.column("n"));
                    found.push(Dependent {
                        described: "default of app.u.n".into(),
                        holds: Holds::TablePart {
                            table: old.clone(),
                            part: Part::Default("n".into()),
                        },
                    });
                    let mut changes = vec![Change::RenameTable {
                        uid: uid("t_cccccc"),
                        from: old,
                        to: new,
                        defaults: vec![],
                    }];
                    changes.extend(steps(&s, kind, motion));
                    changes.push(Change::AlterModule {
                        id: id.clone(),
                        module: Box::new(s.after.modules[&id].clone()),
                    });
                    let mut plan = crate::order::propose(ChangeSet {
                        changes: changes.into_iter().map(PlannedChange::new).collect(),
                    });
                    let before = crate::Side {
                        schema: &s.before,
                        ids: &s.base_ids,
                    };
                    let after = crate::Side {
                        schema: &s.after,
                        ids: &s.ids,
                    };
                    plan.pending_boundaries(
                        crate::order::boundaries::meetings(before, after, &plan),
                        pbps_model::operation::ModuleEdits::Rebuilt,
                    );
                    let label = format!("{kind:?}/{motion:?}/final={cut_final}/PG{version}");
                    weave(
                        &mut plan,
                        &[(id.clone(), found)].into(),
                        &s.after,
                        Sources {
                            before,
                            after: &s.ids,
                        },
                        &pbps_dialect::MinimalDialect,
                    )
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                    assert!(
                        plan.boundaries.ensure(&plan).is_err(),
                        "{label}: pending is not certified"
                    );
                    let mut defaults: Vec<_> = plan
                        .changes
                        .iter()
                        .filter(|p| {
                            matches!(
                                p.change,
                                Change::AlterColumnDefault { .. }
                                    | Change::SetPartitionDefault { .. }
                            )
                        })
                        .map(|p| format!("{:?}", p.change))
                        .collect();
                    defaults.sort();
                    assert_eq!(&defaults, &expected[label.as_str()], "{label}");
                    assert!(seen.insert(label));
                }
            }
        }
    }
    assert_eq!(seen.len(), expected.len());
}
