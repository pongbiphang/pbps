use super::*;
use pbps_model::{Column, Module, PartitionBound, PartitionColumn, PartitionOf, Table, Uid};

fn uid(s: &str) -> Uid {
    s.parse().unwrap()
}
fn table(s: &str) -> TableName {
    TableName::new("public", s)
}
fn function() -> ModuleId {
    "public.f(integer)".parse().unwrap()
}
fn ordinary() -> Table {
    let mut shape = Table::default();
    shape.columns.insert(
        "k".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    let mut n = Column::new("integer".parse().unwrap());
    n.value_source = ValueSource::Default("public.f(1)".into());
    shape.columns.insert("n".into(), n);
    shape
}
fn partition() -> Table {
    Table {
        partition_of: Some(PartitionOf {
            parent: table("parent"),
            bound: PartitionBound::Default,
            columns: [(
                "n".into(),
                PartitionColumn {
                    default: Some("public.f(1)".into()),
                    not_null: false,
                },
            )]
            .into(),
        }),
        ..Default::default()
    }
}
fn ids(attached: bool) -> IdsFile {
    let mut ids = IdsFile {
        tables: [
            (uid("t_aaaaaa"), table("parent")),
            (uid("t_bbbbbb"), table("child")),
        ]
        .into(),
        columns: [
            (uid("c_aaaaaa"), table("parent").column("k")),
            (uid("c_bbbbbb"), table("parent").column("n")),
        ]
        .into(),
        ..Default::default()
    };
    if !attached {
        ids.columns
            .insert(uid("c_cccccc"), table("child").column("k"));
        ids.columns
            .insert(uid("c_dddddd"), table("child").column("n"));
    }
    ids
}
fn changes(detach: bool, rebuild_first: bool) -> ChangeSet {
    let transition = if detach {
        Change::DetachPartition {
            uid: uid("t_bbbbbb"),
            table: table("child"),
            parent: table("parent"),
            names: vec![],
            shape: Box::new(ordinary()),
        }
    } else {
        Change::AttachPartition {
            uid: uid("t_bbbbbb"),
            table: table("child"),
            parent: table("parent"),
            bound: PartitionBound::Default,
            shape: Box::new(partition()),
        }
    };
    let rebuild = Change::AlterModule {
        id: function(),
        module: Box::new(Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT x + 1 $$".into(),
        }),
    };
    let changes = if rebuild_first {
        vec![rebuild, transition]
    } else {
        vec![transition, rebuild]
    };
    ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    }
}
fn found() -> BTreeMap<ModuleId, Vec<Dependent>> {
    [(
        function(),
        vec![Dependent {
            described: "default for public.child.n".into(),
            holds: Holds::TablePart {
                table: table("child"),
                part: Part::Default("n".into()),
            },
        }],
    )]
    .into()
}
fn assert_owner(cs: &ChangeSet, attached_at_rebuild: bool) {
    let defaults: Vec<_> = cs
        .changes
        .iter()
        .filter(|p| {
            matches!(
                p.change,
                Change::AlterColumnDefault { .. } | Change::SetPartitionDefault { .. }
            )
        })
        .collect();
    assert_eq!(defaults.len(), 2, "{cs:?}");
    for p in defaults {
        if attached_at_rebuild {
            assert!(
                matches!(&p.change, Change::SetPartitionDefault { uid: u, table: t, .. }
                if *u == uid("t_bbbbbb") && *t == table("child")),
                "{:?}",
                p.change
            );
        } else {
            assert!(
                matches!(&p.change, Change::AlterColumnDefault { uid: u, column, .. }
                if *u == uid("c_dddddd") && *column == table("child").column("n")),
                "{:?}",
                p.change
            );
        }
    }
}

#[test]
fn each_default_uses_the_owner_at_its_insertion_boundary_on_both_sides_of_attach_and_detach() {
    for detach in [false, true] {
        for rebuild_first in [false, true] {
            let mut parent = ordinary();
            parent.columns.get_mut("n").unwrap().value_source = ValueSource::None;
            let before = Schema {
                tables: [
                    (table("parent"), parent),
                    (
                        table("child"),
                        if detach { partition() } else { ordinary() },
                    ),
                ]
                .into(),
                ..Default::default()
            };
            let mut after = before.clone();
            after.tables.insert(
                table("child"),
                if detach { ordinary() } else { partition() },
            );
            let mut cs = changes(detach, rebuild_first);
            if let Change::AlterModule { id, module } =
                &cs.changes[usize::from(!rebuild_first)].change
            {
                after.modules.insert(id.clone(), module.as_ref().clone());
            }
            let before_ids = ids(detach);
            let after_ids = ids(!detach);
            let sources = Sources {
                before: pbps_diff::Side {
                    schema: &before,
                    ids: &before_ids,
                },
                after: &after_ids,
            };
            let pg = pbps_pg::Postgres::new();
            // Negative: an unchanged catalog dependent is still a blocker.
            let view = sources.project(&cs.changes).unwrap();
            assert_eq!(
                unaccounted(&cs, &found(), &Partitions::default(), &view).len(),
                1
            );
            assert_eq!(weave(&mut cs, &found(), &after, sources, &pg).unwrap(), 2);
            assert_owner(&cs, if rebuild_first { detach } else { !detach });
            let view = sources.project(&cs.changes).unwrap();
            assert!(unaccounted(&cs, &found(), &Partitions::default(), &view).is_empty());
            assert_eq!(weave(&mut cs, &found(), &after, sources, &pg).unwrap(), 0);
        }
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn connected_weaving_executes_defaults_around_both_sides_of_each_ownership_transition() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let database = crate::test_pg::TestDb::create("physical_weave1875").await;
        let mut conn = database.connect().await;
        let pg = pbps_pg::Postgres::new();
        let framing = pg.transaction_framing(&pbps_model::SessionZone::utc());
        for transition in [Some(false), Some(true), None] {
            let detach = transition.unwrap_or(true);
            for rebuild_first in [false, true] {
                conn.execute("CREATE FUNCTION public.f(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT x $$;
                    CREATE TABLE public.parent(k integer NOT NULL, n integer) PARTITION BY RANGE(k);").await.unwrap();
                conn.execute(if detach {
                    "CREATE TABLE public.child PARTITION OF public.parent DEFAULT; ALTER TABLE public.child ALTER COLUMN n SET DEFAULT public.f(1);"
                } else {
                    "CREATE TABLE public.child(k integer NOT NULL, n integer DEFAULT public.f(1));"
                }).await.unwrap();
                let before = pbps_pg::catalog::introspect(&mut conn).await.unwrap().schema;
                assert!(before.tables.contains_key(&table("child")), "{before:?}");
                conn.begin(&framing).await.unwrap();
                let mut after = before.clone();
                after.tables.insert(table("child"), if detach { ordinary() } else { partition() });
                if transition.is_none() {
                    let parent = after.tables.get_mut(&table("parent")).unwrap();
                    let n = parent.columns.shift_remove("n").unwrap();
                    parent.columns.insert("n2".into(), n);
                    let mut child = partition();
                    let own = child.partition_of.as_mut().unwrap();
                    let n = own.columns.remove("n").unwrap();
                    own.columns.insert("n2".into(), n);
                    after.tables.insert(table("child"), child);
                }
                let mut cs = changes(detach, rebuild_first);
                if transition.is_none() {
                    cs.changes[usize::from(rebuild_first)] = PlannedChange::new(Change::RenameColumn {
                        uid: uid("c_bbbbbb"), table: table("parent"), from: "n".into(), to: "n2".into(), table_was: None,
                    });
                }
                if let Change::AlterModule { id, module } = &cs.changes[usize::from(!rebuild_first)].change {
                    after.modules.insert(id.clone(), module.as_ref().clone());
                }
                let before_ids = ids(detach); let mut after_ids = ids(!detach);
                if transition.is_none() {
                    after_ids = ids(true);
                    after_ids.columns.insert(uid("c_bbbbbb"), table("parent").column("n2"));
                }
                let sources = Sources { before: pbps_diff::Side { schema: &before, ids: &before_ids }, after: &after_ids };
                assert!(crate::engine::check_module_dependents(&mut conn, &cs, &before, &after_ids, &before_ids).await.is_err());
                crate::engine::account_for_module_dependents(&mut conn, &mut cs, &after, sources,
                    &ModuleDeps::default(), &pg, &|_| panic!("the plan already rebuilds its sole module"))
                    .await.unwrap();
                assert_owner(&cs, transition.is_none() || if rebuild_first { detach } else { !detach });
                crate::engine::check_module_dependents(&mut conn, &cs, &before, &after_ids, &before_ids).await.unwrap();
                for p in &cs.changes {
                    for statement in pg.emit(&p.change, p.strategy).unwrap() {
                        conn.execute(&statement.sql).await.unwrap();
                    }
                }
                let insert = if transition.is_none() { "INSERT INTO public.child(k) VALUES(7) RETURNING n2" }
                    else { "INSERT INTO public.child(k) VALUES(7) RETURNING n" };
                let rows = conn.query(insert).await.unwrap();
                assert_eq!(rows[0].try_get_at::<i32>(0).unwrap(), Some(2));
                conn.rollback(&framing).await.unwrap();
                conn.execute("DROP TABLE public.child; DROP TABLE public.parent; DROP FUNCTION public.f(integer);").await.unwrap();
            }
        }
        drop(conn);
        database.drop().await;
    });
}
