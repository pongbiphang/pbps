#[cfg(test)]
mod default_rebuild_matrix {
    use super::*;
    use crate::order::{Constraint, Schedule};
    use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};

    #[derive(Clone, Copy, Debug)]
    enum Step {
        Release(&'static str, &'static str),
        Restore(&'static str, &'static str),
        DropColumn(&'static str),
        AddColumn(&'static str),
        Rename(&'static str),
        DropTable(&'static str),
        CreateTable(&'static str),
        Retype,
        DropRoutine,
        CreateRoutine,
        Attach,
        Detach,
    }

    impl Step {
        fn matches(self, c: &Change) -> bool {
            match (self, c) {
                (
                    Self::Release(t, n),
                    Change::AlterColumnDefault {
                        column,
                        from: Some(_),
                        to: None,
                        ..
                    },
                ) => {
                    column.table == t.parse::<pbps_model::TableName>().unwrap()
                        && column.name.raw() == n
                }
                (
                    Self::Release(t, n),
                    Change::SetPartitionDefault {
                        table,
                        column,
                        from: Some(_),
                        to: None,
                        ..
                    },
                ) => *table == t.parse::<pbps_model::TableName>().unwrap() && column.raw() == n,
                (
                    Self::Restore(t, n),
                    Change::AlterColumnDefault {
                        column,
                        to: Some(_),
                        ..
                    },
                ) => {
                    column.table == t.parse::<pbps_model::TableName>().unwrap()
                        && column.name.raw() == n
                }
                (
                    Self::Restore(t, n),
                    Change::SetPartitionDefault {
                        table,
                        column,
                        to: Some(_),
                        ..
                    },
                ) => *table == t.parse::<pbps_model::TableName>().unwrap() && column.raw() == n,
                (Self::DropColumn(n), Change::DropColumn { column, .. }) => column.name.raw() == n,
                (Self::AddColumn(n), Change::AddColumn { name, .. }) => name.raw() == n,
                (Self::Rename(n), Change::RenameColumn { from, .. }) => from.raw() == n,
                (Self::DropTable(t), Change::DropTable { name, .. }) => {
                    *name == t.parse::<pbps_model::TableName>().unwrap()
                }
                (Self::CreateTable(t), Change::CreateTable { name, .. }) => {
                    *name == t.parse::<pbps_model::TableName>().unwrap()
                }
                (Self::Retype, Change::AlterColumnType { .. })
                | (Self::DropRoutine, Change::DropModule { .. })
                | (Self::CreateRoutine, Change::CreateModule { .. })
                | (Self::Attach, Change::AttachPartition { .. })
                | (Self::Detach, Change::DetachPartition { .. }) => true,
                _ => false,
            }
        }
    }

    struct Case {
        name: &'static str,
        fixture: fn() -> Fixture,
        edges: Vec<(Step, Step)>,
        absent: Vec<Step>,
    }

    fn fixture(base: Schema, desired: Schema, observations: Vec<SurfaceResolution>) -> Fixture {
        let before_ids = ids(&base, &IdsFile::default());
        // Rename fixtures supply identity intent explicitly below.
        let after_ids = before_ids.clone();
        Fixture {
            base,
            desired,
            before_ids,
            after_ids,
            resolution: observations,
        }
    }

    fn module(body: &str) -> pbps_model::Module {
        pbps_model::Module {
            kind: pbps_model::ModuleKind::Function,
            description: None,
            options: Default::default(),
            definition: format!("(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN {body}"),
        }
    }

    fn routine() -> SurfaceResolution {
        let object = Some(BoundSurface {
            object: ObjectIdentity {
                class: "pg_proc".into(),
                name: vec!["app".into(), "f".into()],
                signature: vec![],
            },
            bindings: vec![],
            managed_inputs: BTreeSet::new(),
        });
        SurfaceResolution {
            surface: Surface::Module("app.f(integer)".parse().unwrap()),
            current: object.clone(),
            desired: object,
        }
    }

    fn shared_routine() -> Fixture {
        let mut base = partitioned(Some("app.f(0)"), Some("app.f(1)"));
        let f = "app.f(integer)".parse().unwrap();
        base.modules.insert(f, module("$1"));
        let mut desired = base.clone();
        desired
            .modules
            .insert("app.f(integer)".parse().unwrap(), module("$1+1"));
        let mut observations = vec![routine()];
        for t in ["app.t", "app.p"] {
            let table: pbps_model::TableName = t.parse().unwrap();
            observations.push(SurfaceResolution {
                surface: Surface::Default(table.column("d")),
                current: Some(bound(
                    "pg_attrdef",
                    &format!("{t}.d"),
                    &[Surface::Module("app.f(integer)".parse().unwrap())],
                    "f",
                )),
                desired: Some(bound(
                    "pg_attrdef",
                    &format!("{t}.d"),
                    &[Surface::Module("app.f(integer)".parse().unwrap())],
                    "f_v2",
                )),
            });
        }
        fixture(base, desired, observations)
    }

    fn retyped_default() -> Fixture {
        let mut f = shared_routine();
        f.base.tables.remove(&"app.p".parse().unwrap());
        f.desired.tables.remove(&"app.p".parse().unwrap());
        for schema in [&mut f.base, &mut f.desired] {
            schema
                .tables
                .get_mut(&"app.t".parse().unwrap())
                .unwrap()
                .partition_by = None;
        }
        f.desired
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .get_mut("d")
            .unwrap()
            .ty = "bigint".parse().unwrap();
        f.resolution
            .retain(|r| r.surface != Surface::Default("app.p.d".parse().unwrap()));
        f.before_ids = ids(&f.base, &IdsFile::default());
        f.after_ids = f.before_ids.clone();
        f
    }

    fn retyped_tree() -> Fixture {
        let mut f = shared_routine();
        for schema in [&mut f.base, &mut f.desired] {
            let parent = schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
            parent
                .columns
                .insert("k".into(), Column::new("int".parse().unwrap()));
            parent.partition_by.as_mut().unwrap().columns = vec!["k".into()];
        }
        f.desired
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .get_mut("d")
            .unwrap()
            .ty = "bigint".parse().unwrap();
        f.before_ids = ids(&f.base, &IdsFile::default());
        f.after_ids = f.before_ids.clone();
        f
    }

    fn retyped_own_default() -> Fixture {
        let mut f = retyped_tree();
        for schema in [&mut f.base, &mut f.desired] {
            schema
                .tables
                .get_mut(&"app.t".parse().unwrap())
                .unwrap()
                .columns
                .get_mut("d")
                .unwrap()
                .value_source = pbps_model::ValueSource::None;
        }
        f.resolution
            .retain(|r| r.surface != Surface::Default("app.t.d".parse().unwrap()));
        f
    }

    #[test]
    fn a_parent_retype_runs_in_each_rebuilt_partitions_default_gap() {
        use Step::*;
        for (name, fixture, parent) in [
            ("retype/tree", retyped_tree as fn() -> Fixture, true),
            (
                "retype/own-default-only",
                retyped_own_default as fn() -> Fixture,
                false,
            ),
        ] {
            let mut edges = vec![
                (Release("app.p", "d"), Retype),
                (Retype, Restore("app.p", "d")),
            ];
            if parent {
                edges.extend([
                    (Release("app.t", "d"), Retype),
                    (Retype, Restore("app.t", "d")),
                ]);
            }
            check(&Case {
                name,
                fixture,
                edges,
                absent: if parent {
                    vec![]
                } else {
                    vec![Release("app.t", "d"), Restore("app.t", "d")]
                },
            });
        }
    }

    #[test]
    fn a_rebuilt_held_default_is_released_before_its_column_is_retyped() {
        check(&Case {
            name: "retype/held-default",
            fixture: retyped_default,
            edges: vec![
                (Step::Release("app.t", "d"), Step::Retype),
                (Step::Retype, Step::Restore("app.t", "d")),
            ],
            absent: vec![],
        });
    }

    #[test]
    fn a_retype_without_a_default_rebuild_keeps_the_ordinary_plan() {
        let mut f = retyped_default();
        f.base.modules.clear();
        f.desired.modules.clear();
        for schema in [&mut f.base, &mut f.desired] {
            schema
                .tables
                .get_mut(&"app.t".parse().unwrap())
                .unwrap()
                .columns
                .get_mut("d")
                .unwrap()
                .value_source = pbps_model::ValueSource::Default("7".into());
        }
        let before = crate::Side {
            schema: &f.base,
            ids: &f.before_ids,
        };
        let after = crate::Side {
            schema: &f.desired,
            ids: &f.after_ids,
        };
        let ordinary = crate::diff(before, after, &Candidates, &Hints::default()).unwrap();
        let held = Some(bound("pg_attrdef", "app.t.d", &[], "constant"));
        let planned = crate::resolver::plan_operations(
            before,
            after,
            &Hints::default(),
            &[SurfaceResolution {
                surface: Surface::Default("app.t.d".parse().unwrap()),
                current: held.clone(),
                desired: held,
            }],
            &Candidates,
        )
        .unwrap();
        assert_eq!(ordinary.changes.len(), 1, "{:?}", ordinary.changes);
        assert!(matches!(
            ordinary.changes[0].change,
            Change::AlterColumnType { .. }
        ));
        assert_eq!(
            serde_json::to_value(&planned.ordered.changes.changes).unwrap(),
            serde_json::to_value(&ordinary.changes).unwrap(),
        );
        planned
            .ordered
            .proof
            .validate(&planned.ordered.changes)
            .unwrap();
        let names = pbps_model::plan_names::PlanNames::of(&ordinary.changes, &f.after_ids).unwrap();
        let owners = PlanOwners::new(names, &f.before_ids, &f.after_ids);
        let before = Snapshot {
            schema: &f.base,
            ids: &f.before_ids,
        };
        let after = Snapshot {
            schema: &f.desired,
            ids: &f.after_ids,
        };
        let shadow =
            crate::order::shadow::analyse(&planned.operations, &owners, before, after, &Candidates);
        assert!(shadow.unowned.is_empty(), "{:?}", shadow.unowned);
        let baseline = crate::order::edges::Baseline::of(before, &owners);
        let violations = crate::order::verify::verify(
            planned.operations.as_slice(),
            &shadow.effects,
            &baseline,
            &Hints::default().module_deps,
        );
        assert!(
            violations.is_empty(),
            "retained-default replay: {violations:?}"
        );
    }

    fn replaced_column() -> Fixture {
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let mut base = Schema::default();
        base.tables.insert(
            t.clone(),
            Table {
                columns: [
                    ("a".into(), Column::new("int".parse().unwrap())),
                    ("c".into(), generated("a+1")),
                ]
                .into(),
                ..Default::default()
            },
        );
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&t)
            .unwrap()
            .columns
            .get_mut("c")
            .unwrap()
            .value_source = pbps_model::ValueSource::Default("1".into());
        let mut f = fixture(
            base,
            desired,
            vec![SurfaceResolution {
                surface: Surface::Default(t.column("c")),
                ..rebinds("app.t")
            }],
        );
        let uid = f.before_ids.column_uid(&t.column("c")).unwrap().clone();
        f.after_ids.columns.remove(&uid);
        f.after_ids
            .columns
            .insert("c_zzzzzz".parse().unwrap(), t.column("c"));
        f
    }

    fn parent_replaced_by_rename() -> Fixture {
        let mut f = fixture(
            tree("app.t", &["k", "d", "e"], "app.p", &[("d", "app.f(1)")]),
            tree("app.t", &["k", "d"], "app.p", &[("d", "app.f(1)")]),
            vec![rebinds("app.p")],
        );
        rename_parent(&mut f, false);
        f
    }

    fn rename_parent(f: &mut Fixture, table: bool) {
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let d = f.before_ids.column_uid(&t.column("d")).unwrap().clone();
        let e = f.before_ids.column_uid(&t.column("e")).unwrap().clone();
        f.after_ids = f.before_ids.clone();
        f.after_ids.columns.remove(&d);
        f.after_ids.columns.get_mut(&e).unwrap().name = "d".into();
        if table {
            let uid = f.before_ids.table_uid(&t).unwrap();
            *f.after_ids.tables.get_mut(uid).unwrap() = "app.u".parse().unwrap();
            for c in f.after_ids.columns.values_mut().filter(|c| c.table == t) {
                c.table = "app.u".parse().unwrap();
            }
        }
    }

    fn renamed_override(table: bool) -> Fixture {
        let parent = if table { "app.u" } else { "app.t" };
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let mut f = fixture(
            tree(
                "app.t",
                &["k", "d", "e"],
                "app.p",
                &[("d", "app.f(1)"), ("e", "app.f(2)")],
            ),
            tree(parent, &["k", "d"], "app.p", &[("d", "app.f(2)")]),
            vec![
                rebinds("app.p"),
                SurfaceResolution {
                    surface: Surface::Default(p.column("e")),
                    current: Some(bound("pg_attrdef", "app.p.e", &[], "f")),
                    desired: None,
                },
            ],
        );
        rename_parent(&mut f, table);
        f
    }

    fn parent_drop_add() -> Fixture {
        let mut f = shared_routine();
        // d is not the partition key: replacing it is an executable shape.
        f.base
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .insert("k".into(), Column::new("int".parse().unwrap()));
        f.base
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .partition_by
            .as_mut()
            .unwrap()
            .columns = vec!["k".into()];
        f.desired
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .insert("k".into(), Column::new("int".parse().unwrap()));
        f.desired
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .partition_by
            .as_mut()
            .unwrap()
            .columns = vec!["k".into()];
        f.before_ids = ids(&f.base, &IdsFile::default());
        f.after_ids = ids(&f.desired, &f.before_ids);
        let col = "app.t"
            .parse::<pbps_model::TableName>()
            .unwrap()
            .column("d");
        let uid = f.before_ids.column_uid(&col).unwrap().clone();
        f.after_ids.columns.remove(&uid);
        f.after_ids.columns.insert("c_zzzzzz".parse().unwrap(), col);
        f
    }

    fn replaced_owner(partition: bool) -> Fixture {
        let t: pbps_model::TableName = if partition { "app.p" } else { "app.t" }.parse().unwrap();
        let base = if partition {
            partitioned(None, Some("app.f(1)"))
        } else {
            let mut b = Schema::default();
            let mut c = Column::new("int".parse().unwrap());
            c.value_source = pbps_model::ValueSource::Default("app.f(1)".into());
            b.tables.insert(
                t.clone(),
                Table {
                    columns: [("d".into(), c)].into(),
                    ..Default::default()
                },
            );
            b
        };
        let mut f = fixture(
            base.clone(),
            base,
            vec![rebinds(if partition { "app.p" } else { "app.t" })],
        );
        let uid = f.before_ids.table_uid(&t).unwrap().clone();
        f.after_ids.tables.remove(&uid);
        f.after_ids
            .tables
            .insert("t_zzzzzz".parse().unwrap(), t.clone());
        let columns: Vec<_> = f
            .after_ids
            .columns
            .iter()
            .filter(|(_, c)| c.table == t)
            .map(|(u, c)| (u.clone(), c.clone()))
            .collect();
        for (u, c) in columns {
            f.after_ids.columns.remove(&u);
            f.after_ids.columns.insert("c_zzzzzz".parse().unwrap(), c);
        }
        f
    }

    fn transition(attach: bool) -> Fixture {
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let mut ordinary = partitioned(None, None);
        let mut c = Column::new("int".parse().unwrap());
        c.value_source = pbps_model::ValueSource::Default("app.f(1)".into());
        ordinary.tables.insert(
            p.clone(),
            Table {
                columns: [("d".into(), c)].into(),
                ..Default::default()
            },
        );
        let mut partition = partitioned(None, Some("app.f(1)"));
        partition
            .tables
            .get_mut(&p)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .bound = pbps_model::PartitionBound::Range {
            from: vec![pbps_model::BoundDatum::Value("0".into())],
            to: vec![pbps_model::BoundDatum::Value("10".into())],
        };
        let mut f = if attach {
            fixture(ordinary, partition, vec![rebinds("app.p")])
        } else {
            fixture(partition, ordinary, vec![rebinds("app.p")])
        };
        f.after_ids = ids(&f.desired, &f.before_ids);
        f
    }

    fn chain() -> Fixture {
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let mut f = fixture(
            tree("app.t", &["k", "d", "e"], "app.p", &[("d", "app.f(1)")]),
            tree("app.t", &["k", "e", "f"], "app.p", &[]),
            vec![SurfaceResolution {
                surface: Surface::Default(p.column("d")),
                current: Some(bound("pg_attrdef", "app.p.d", &[], "f")),
                desired: None,
            }],
        );
        f.after_ids = f.before_ids.clone();
        for c in f.after_ids.columns.values_mut().filter(|c| c.table == t) {
            c.name = match c.name.raw() {
                "d" => "e",
                "e" => "f",
                _ => continue,
            }
            .into();
        }
        f
    }

    fn fallback() -> Fixture {
        let (f, dropped) = function("f", true);
        let mut base = partitioned(Some("0"), Some("app.f(1)"));
        base.modules.insert(f, module("$1"));
        let desired = partitioned(Some("0"), None);
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let observation = SurfaceResolution {
            surface: Surface::Default(p.column("d")),
            current: Some(bound(
                "pg_attrdef",
                "app.p.d",
                std::slice::from_ref(&dropped.surface),
                "f",
            )),
            desired: None,
        };
        fixture(base, desired, vec![keeps("app.t"), observation, dropped])
    }

    fn standing(inherited: bool) -> Fixture {
        let b = partitioned(Some("app.f(0)"), if inherited { None } else { Some("7") });
        fixture(
            b.clone(),
            b,
            vec![rebinds("app.t"), keeps("app.p")]
                .into_iter()
                .filter(|r| {
                    !inherited
                        || r.surface
                            != Surface::Default(
                                "app.p"
                                    .parse::<pbps_model::TableName>()
                                    .unwrap()
                                    .column("d"),
                            )
                })
                .collect(),
        )
    }

    fn path(edges: impl IntoIterator<Item = (usize, usize)>, from: usize, to: usize) -> bool {
        let edges: Vec<_> = edges.into_iter().collect();
        let mut reached = BTreeSet::from([from]);
        let mut stack = vec![from];
        while let Some(a) = stack.pop() {
            for &(_, b) in edges.iter().filter(|(p, _)| *p == a) {
                if b == to {
                    return true;
                }
                if reached.insert(b) {
                    stack.push(b);
                }
            }
        }
        false
    }

    fn check(case: &Case) {
        let Fixture {
            base,
            desired,
            before_ids,
            after_ids,
            resolution,
        } = (case.fixture)();
        let before = crate::Side {
            schema: &base,
            ids: &before_ids,
        };
        let after = crate::Side {
            schema: &desired,
            ids: &after_ids,
        };
        let hints = Hints::default();
        let dialect = Candidates;
        println!("matrix case: {}", case.name);
        let (logical, prepared) =
            crate::resolver::prepare(before, after, &hints, &resolution, &dialect)
                .unwrap_or_else(|e| panic!("{} prepare: {e:?}", case.name));
        let ops = Operations::expanded(
            logical.changes.len(),
            prepared
                .origins
                .iter()
                .cloned()
                .zip(prepared.changes.changes.iter().cloned()),
            ModuleEdits::Rebuilt,
        );
        let names = pbps_model::plan_names::PlanNames::of(&logical.changes, &after_ids).unwrap();
        let owners = PlanOwners::new(names, &before_ids, &after_ids);
        let schedule: Schedule = crate::resolver::scheduled(
            &prepared.changes,
            &prepared.origins,
            &ops,
            &owners,
            &resolution,
            &hints.module_deps,
            before,
            after,
            &dialect,
        );
        let planned =
            crate::resolver::plan_operations(before, after, &hints, &resolution, &dialect)
                .unwrap_or_else(|e| {
                    panic!(
                        "{} refused: {e:?}; prepared={:#?}",
                        case.name, prepared.changes
                    )
                });
        pbps_model::plan_names::PlanNames::of(&planned.ordered.changes.changes, &after_ids)
            .unwrap();
        planned
            .ordered
            .proof
            .validate(&planned.ordered.changes)
            .unwrap();
        for step in &case.absent {
            assert!(
                !planned
                    .operations
                    .iter()
                    .any(|op| step.matches(op.change())),
                "{} unexpected {step:?}",
                case.name
            );
        }
        for &(a, b) in &case.edges {
            let find = |operations: &Operations, s: Step| {
                let found: Vec<_> = operations
                    .iter()
                    .enumerate()
                    .filter(|(_, op)| s.matches(op.change()))
                    .map(|(i, _)| i)
                    .collect();
                assert_eq!(
                    found.len(),
                    1,
                    "{} {s:?}: {found:?}; {operations:#?}",
                    case.name
                );
                found[0]
            };
            let (a0, b0) = (find(&ops, a), find(&ops, b));
            assert!(
                path(
                    schedule.constraints().iter().map(|e| (e.before, e.after)),
                    a0,
                    b0
                ),
                "{} missing schedule edge/path {a:?} -> {b:?}; {:#?}",
                case.name,
                schedule.constraints()
            );
            let (a1, b1) = (find(&planned.operations, a), find(&planned.operations, b));
            assert!(
                path(
                    planned
                        .ordered
                        .proof
                        .edges()
                        .iter()
                        .map(|e| (e.before, e.after)),
                    a1,
                    b1
                ),
                "{} missing proof edge/path {a:?} -> {b:?}; {:#?}",
                case.name,
                planned.ordered.proof.edges()
            );
        }
        // Seal the exact settle edges/reasons, remapped by stable operation IDs.
        for Constraint {
            before: a,
            after: b,
            reason,
            ..
        } in schedule.constraints()
        {
            let pos = |i: usize| {
                planned
                    .operations
                    .iter()
                    .position(|op| op.id() == ops.as_slice()[i].id())
                    .unwrap()
            };
            assert!(
                planned.ordered.proof.edges().contains(&OrderEdge {
                    before: pos(*a),
                    after: pos(*b),
                    reason: *reason
                }),
                "{} lost constraint {a}->{b}: {reason:?}",
                case.name
            );
        }
        let shadow = crate::order::shadow::analyse(
            &planned.operations,
            &owners,
            Snapshot {
                schema: &base,
                ids: &before_ids,
            },
            Snapshot {
                schema: &desired,
                ids: &after_ids,
            },
            &dialect,
        );
        assert!(
            shadow.unowned.is_empty(),
            "{}: {:?}",
            case.name,
            shadow.unowned
        );
        let baseline = crate::order::edges::Baseline::of(
            Snapshot {
                schema: &base,
                ids: &before_ids,
            },
            &owners,
        );
        let violations = crate::order::verify::verify(
            planned.operations.as_slice(),
            &shadow.effects,
            &baseline,
            &hints.module_deps,
        );
        assert!(
            violations.is_empty(),
            "{} replay: {violations:?}",
            case.name
        );
    }

    #[test]
    fn seven_default_rebuild_interactions_have_settle_edges_and_valid_proofs() {
        use Step::*;
        let cases = [
            Case {
                name: "draft1/replaced-column",
                fixture: replaced_column,
                edges: vec![(DropColumn("c"), AddColumn("c"))],
                absent: vec![Release("app.t", "c")],
            },
            Case {
                name: "draft1/shared-parent-partition-routine",
                fixture: shared_routine,
                edges: vec![
                    (Release("app.t", "d"), DropRoutine),
                    (Release("app.p", "d"), DropRoutine),
                    (DropRoutine, CreateRoutine),
                    (CreateRoutine, Restore("app.t", "d")),
                    (Restore("app.t", "d"), Restore("app.p", "d")),
                ],
                absent: vec![],
            },
            Case {
                name: "draft2/parent-replaced-by-rename",
                fixture: parent_replaced_by_rename,
                edges: vec![
                    (DropColumn("d"), Rename("e")),
                    (Rename("e"), Restore("app.p", "d")),
                ],
                absent: vec![Release("app.p", "d")],
            },
            Case {
                name: "draft2/parent-drop-add",
                fixture: parent_drop_add,
                edges: vec![
                    (Release("app.p", "d"), DropColumn("d")),
                    (DropColumn("d"), AddColumn("d")),
                    (AddColumn("d"), Restore("app.p", "d")),
                ],
                absent: vec![],
            },
            Case {
                name: "draft2/replaced-table",
                fixture: || replaced_owner(false),
                edges: vec![(DropTable("app.t"), CreateTable("app.t"))],
                absent: vec![Release("app.t", "d")],
            },
            Case {
                name: "draft2/detach",
                fixture: || transition(false),
                edges: vec![
                    (Release("app.p", "d"), Detach),
                    (Detach, Restore("app.p", "d")),
                ],
                absent: vec![],
            },
            Case {
                name: "draft3/parent-column-rename",
                fixture: || renamed_override(false),
                edges: vec![
                    (Release("app.p", "e"), Rename("e")),
                    (Rename("e"), Restore("app.p", "d")),
                ],
                absent: vec![],
            },
            Case {
                name: "draft4/parent-rename-chain",
                fixture: chain,
                edges: vec![
                    (Rename("e"), Release("app.p", "e")),
                    (Rename("d"), Release("app.p", "e")),
                ],
                absent: vec![],
            },
            Case {
                name: "draft4/parent-table-and-column-rename",
                fixture: || renamed_override(true),
                edges: vec![
                    (Release("app.p", "e"), Rename("e")),
                    (Rename("e"), Restore("app.p", "d")),
                ],
                absent: vec![],
            },
            Case {
                name: "ready1/replaced-partition",
                fixture: || replaced_owner(true),
                edges: vec![(DropTable("app.p"), CreateTable("app.p"))],
                absent: vec![Release("app.p", "d")],
            },
            Case {
                name: "ready2/attach",
                fixture: || transition(true),
                edges: vec![
                    (Release("app.p", "d"), Attach),
                    (Attach, Restore("app.p", "d")),
                ],
                absent: vec![],
            },
            Case {
                name: "ready2/fallback-releases-routine",
                fixture: fallback,
                edges: vec![(Release("app.p", "d"), DropRoutine)],
                absent: vec![],
            },
            Case {
                name: "ready3/standing-override",
                fixture: || standing(false),
                edges: vec![(Restore("app.t", "d"), Restore("app.p", "d"))],
                absent: vec![],
            },
            Case {
                name: "negative/inherited-default",
                fixture: || standing(true),
                edges: vec![(Release("app.t", "d"), Restore("app.t", "d"))],
                absent: vec![Release("app.p", "d"), Restore("app.p", "d")],
            },
        ];
        for case in &cases {
            check(case);
        }
    }
}
