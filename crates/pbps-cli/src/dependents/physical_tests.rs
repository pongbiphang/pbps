use super::test_adapters::weave;
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
            key: Vec::new(),
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
                let mut cs = pbps_diff::order::propose(cs);
    crate::engine::account_for_module_dependents(&mut conn, &mut cs, &after, sources,
                    &ModuleDeps::default(), None, &pg, &|_| panic!("the plan already rebuilds its sole module"))
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
        // Moving a parent release across its rename must preserve recursion
        // and use the parent's name at the insertion boundary.
        conn.execute("CREATE FUNCTION public.f(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT x $$;
            CREATE TABLE public.parent(k integer NOT NULL, n integer DEFAULT 0) PARTITION BY RANGE(k);
            CREATE TABLE public.child PARTITION OF public.parent DEFAULT;
            ALTER TABLE public.child ALTER COLUMN n SET DEFAULT public.f(1);").await.unwrap();
        let before = pbps_pg::catalog::introspect(&mut conn).await.unwrap().schema;
        let (after, after_ids, cs) = parent_release(&before);
        let before_ids = ids(true);
        let sources = Sources { before: pbps_diff::Side { schema: &before, ids: &before_ids }, after: &after_ids };
        conn.begin(&framing).await.unwrap();
        assert!(crate::engine::check_module_dependents(&mut conn, &cs, &before, &after_ids, &before_ids).await.is_err());
        let mut cs = pbps_diff::order::propose(cs);
    crate::engine::account_for_module_dependents(&mut conn, &mut cs, &after, sources,
            &ModuleDeps::default(), None, &pg, &|_| panic!("the plan already drops its sole module"))
            .await.unwrap();
        assert_parent_release(&cs);
        crate::engine::check_module_dependents(&mut conn, &cs, &before, &after_ids, &before_ids).await.unwrap();
        for p in &cs.changes {
            for statement in pg.emit(&p.change, p.strategy).unwrap() {
                conn.execute(&statement.sql).await.unwrap();
            }
        }
        for target in ["child", "renamed"] {
            let rows = conn.query(&format!("INSERT INTO public.{target}(k) VALUES(7) RETURNING n")).await.unwrap();
            assert_eq!(rows[0].try_get_at::<i32>(0).unwrap(), None);
        }
        conn.rollback(&framing).await.unwrap();
        drop(conn);
        database.drop().await;
    });
}

#[test]
fn an_inherited_default_release_moved_before_a_parent_rename_keeps_the_parent_uid_and_old_name() {
    let mut parent = ordinary();
    parent.columns.get_mut("n").unwrap().value_source = ValueSource::Default("0".into());
    let before = Schema {
        tables: [
            (table("parent"), parent.clone()),
            (table("child"), partition()),
        ]
        .into(),
        ..Default::default()
    };
    let (after, after_ids, mut cs) = parent_release(&before);
    let before_ids = ids(true);
    let sources = Sources {
        before: pbps_diff::Side {
            schema: &before,
            ids: &before_ids,
        },
        after: &after_ids,
    };
    weave(
        &mut cs,
        &found(),
        &after,
        sources,
        &pbps_pg::Postgres::new(),
    )
    .unwrap();
    assert_parent_release(&cs);
}

fn parent_release(before: &Schema) -> (Schema, IdsFile, ChangeSet) {
    let mut parent = before.tables[&table("parent")].clone();
    parent.columns.get_mut("n").unwrap().value_source = ValueSource::None;
    let mut child = partition();
    let of = child.partition_of.as_mut().unwrap();
    of.parent = table("renamed");
    of.columns.clear();
    let after = Schema {
        tables: [(table("renamed"), parent), (table("child"), child)].into(),
        ..Default::default()
    };
    let before_ids = ids(true);
    let mut after_ids = before_ids.clone();
    after_ids.tables.insert(uid("t_aaaaaa"), table("renamed"));
    for column in after_ids.columns.values_mut() {
        column.table = table("renamed");
    }
    let cs = ChangeSet {
        changes: vec![
            Change::DropModule {
                id: function(),
                kind: ModuleKind::Function,
            },
            Change::RenameTable {
                uid: uid("t_aaaaaa"),
                from: table("parent"),
                to: table("renamed"),
                defaults: vec![],
            },
            Change::AlterColumnDefault {
                uid: uid("c_bbbbbb"),
                column: table("renamed").column("n"),
                from: Some("0".into()),
                to: None,
            },
        ]
        .into_iter()
        .map(PlannedChange::new)
        .collect(),
    };
    (after, after_ids, cs)
}

fn assert_parent_release(cs: &ChangeSet) {
    assert!(
        matches!(&cs.changes[0].change, Change::AlterColumnDefault { uid: u, column, to: None, .. }
        if *u == uid("c_bbbbbb") && *column == table("parent").column("n")),
        "{cs:?}"
    );
    assert_eq!(
        cs.changes.len(),
        3,
        "no partition-only replacement for the parent's release"
    );
}

#[test]
#[ignore = "needs a live PostgreSQL"]
fn inherited_default_keeps_its_expression_schema_across_attach() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for rebuild_first in [true, false] {
                for conflicting_function in [false, true] {
                    inherited_scope_case(rebuild_first, conflicting_function).await;
                }
            }
        });
}

async fn inherited_scope_case(rebuild_first: bool, conflicting_function: bool) {
    let database = crate::test_pg::TestDb::create("physical_scope1875").await;
    let mut conn = database.connect().await;
    let pg = pbps_pg::Postgres::new();
    conn.execute(
        "CREATE SCHEMA parent_scope; CREATE SCHEMA child_scope;
         CREATE FUNCTION parent_scope.f(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT x $$;
         CREATE TABLE parent_scope.parent(k integer NOT NULL, n integer DEFAULT parent_scope.f(1)) PARTITION BY RANGE(k);
         CREATE TABLE child_scope.child(k integer NOT NULL, n integer DEFAULT parent_scope.f(1));",
    ).await.unwrap();
    if conflicting_function {
        // Successful execution alone cannot detect rebinding to the child's
        // schema. Its same-signature function must never supply this default.
        conn.execute("CREATE FUNCTION child_scope.f(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT 99 $$;")
            .await.unwrap();
    }
    let before = pbps_pg::catalog::introspect(&mut conn)
        .await
        .unwrap()
        .schema;
    let parent = TableName::new("parent_scope", "parent");
    let child = TableName::new("child_scope", "child");
    let root: ModuleId = "parent_scope.f(integer)".parse().unwrap();
    let mut after = before.clone();
    after
        .tables
        .get_mut(&parent)
        .unwrap()
        .columns
        .get_mut("n")
        .unwrap()
        .value_source = ValueSource::Default("f(1)".into());
    let attached = Table {
        partition_of: Some(PartitionOf {
            parent: parent.clone(),
            bound: PartitionBound::Default,
            columns: Default::default(),
        }),
        ..Default::default()
    };
    after.tables.insert(child.clone(), attached.clone());
    let module = Module {
        kind: ModuleKind::Function,
        description: None,
        definition: "(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT x + 1 $$".into(),
    };
    after.modules.insert(root.clone(), module.clone());
    let before_ids = IdsFile {
        tables: [
            (uid("t_aaaaaa"), parent.clone()),
            (uid("t_bbbbbb"), child.clone()),
        ]
        .into(),
        columns: [
            (uid("c_aaaaaa"), parent.column("k")),
            (uid("c_bbbbbb"), parent.column("n")),
            (uid("c_cccccc"), child.column("k")),
            (uid("c_dddddd"), child.column("n")),
        ]
        .into(),
        ..Default::default()
    };
    let mut after_ids = before_ids.clone();
    after_ids.columns.retain(|_, c| c.table != child);
    let rebuild = Change::AlterModule {
        id: root,
        module: Box::new(module),
    };
    let attach = Change::AttachPartition {
        uid: uid("t_bbbbbb"),
        table: child.clone(),
        parent,
        bound: PartitionBound::Default,
        key: Vec::new(),
        shape: Box::new(attached),
    };
    let changes = if rebuild_first {
        vec![rebuild, attach]
    } else {
        vec![attach, rebuild]
    };
    let cs = ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    };
    let sources = Sources {
        before: pbps_diff::Side {
            schema: &before,
            ids: &before_ids,
        },
        after: &after_ids,
    };
    let framing = pg.transaction_framing(&pbps_model::SessionZone::utc());
    conn.begin(&framing).await.unwrap();
    let mut cs = pbps_diff::order::propose(cs);
    crate::engine::account_for_module_dependents(
        &mut conn,
        &mut cs,
        &after,
        sources,
        &ModuleDeps::default(),
        None,
        &pg,
        &|_| panic!("the plan already rebuilds the function"),
    )
    .await
    .unwrap();
    crate::engine::check_module_dependents(&mut conn, &cs, &before, &after_ids, &before_ids)
        .await
        .unwrap();
    for p in &cs.changes {
        for statement in pg.emit(&p.change, p.strategy).unwrap() {
            conn.execute(&statement.sql)
                .await
                .unwrap_or_else(|e| panic!("{e}\n{:?}\n{}", p.change, statement.sql));
        }
    }
    let rows = conn
        .query("INSERT INTO child_scope.child(k) VALUES(7) RETURNING n")
        .await
        .unwrap();
    assert_eq!(
        rows[0].try_get_at::<i32>(0).unwrap(),
        Some(2),
        "rebuild_first={rebuild_first}, conflicting_function={conflicting_function}"
    );
    conn.rollback(&framing).await.unwrap();
    drop(conn);
    database.drop().await;
}
