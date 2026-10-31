use super::*;
#[path = "../../../pbps-diff/tests/fixtures/creation2469.rs"]
mod fixture;
use fixture::{FACETS, Fixture};
use pbps_dialect::lower::lower_all;
use pbps_model::{PlanMode, object_ref::ObjectDeps};
type Engine = Postgres;

async fn execute(db: &mut TestDb, plan: pbps_diff::order::CanonicalPlan<Engine>, engine: &Engine) {
    let ordered = plan.ordered(PlanMode::Transactional, engine);
    let lowered = lower_all(&ordered as &dyn pbps_diff::order::PlanView<Engine>, engine).unwrap();
    for statement in &lowered {
        db.conn.execute(&statement.sql).await.unwrap();
    }
}
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn demanded_facets_preserve_native_values_and_declared_column_order() {
    let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
    let mut conn = Conn::connect(Driver::Postgres, &server).await.unwrap();
    let rows = conn
        .query("SELECT current_setting('server_version_num')::int AS version")
        .await
        .unwrap();
    let version = rows[0].try_get::<i32>("version").unwrap().unwrap() as i64;
    let engine = Postgres::new().with_server_version(version);
    for facet in FACETS {
        let mut db = TestDb::create(&format!("creation2469_{facet}")).await;
        db.conn.execute("CREATE SCHEMA app").await.unwrap();
        db.conn.execute("BEGIN TRANSACTION").await.unwrap();
        let f = Fixture::new(facet, "postgres");
        let plan = f
            .compile(&engine)
            .unwrap_or_else(|e| panic!("{facet}: {e}"));
        assert!(
            plan.annotation_edges()
                .unwrap()
                .iter()
                .all(|e| e.before < e.after)
        );
        execute(&mut db, plan, &engine).await;
        db.conn
            .execute("INSERT INTO app.p(anchor) VALUES(7)")
            .await
            .unwrap();
        db.conn
            .execute(if *facet == "default" {
                "INSERT INTO app.t(anchor) VALUES(1)"
            } else {
                "INSERT INTO app.t(anchor,x) VALUES(1,7)"
            })
            .await
            .unwrap();
        let rows = db
            .conn
            .query(if *facet == "generation" {
                "SELECT g AS value FROM app.t"
            } else {
                "SELECT x AS value FROM app.t"
            })
            .await
            .unwrap();
        assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(7), "{facet}");
        let rows=db.conn.query("SELECT column_name FROM information_schema.columns WHERE table_schema='app' AND table_name='t' ORDER BY ordinal_position").await.unwrap();
        let actual: Vec<_> = rows
            .iter()
            .map(|r| r.try_get::<&str>("column_name").unwrap().unwrap())
            .collect();
        let expected = if *facet == "generation" {
            vec!["anchor", "x", "g"]
        } else {
            vec!["anchor", "x"]
        };
        assert_eq!(actual, expected, "{facet}");
        let bad = match *facet {
            "not_null" => Some("INSERT INTO app.t(anchor,x) VALUES(2,NULL)"),
            "primary_key" => Some("INSERT INTO app.t(anchor,x) VALUES(1,8)"),
            "unique" => Some("INSERT INTO app.t(anchor,x) VALUES(2,7)"),
            "foreign_key" => Some("INSERT INTO app.t(anchor,x) VALUES(2,999)"),
            "check" => Some("INSERT INTO app.t(anchor,x) VALUES(2,-1)"),
            _ => None,
        };
        if let Some(sql) = bad {
            db.conn.execute("SAVEPOINT bad2469").await.unwrap();
            assert!(db.conn.execute(sql).await.is_err(), "{facet}");
            db.conn
                .execute("ROLLBACK TO SAVEPOINT bad2469")
                .await
                .unwrap();
        }
        db.conn.execute("COMMIT").await.unwrap();
        db.drop().await;
    }
    let mut db = TestDb::create("creation2469_standing").await;
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    db.conn.execute("BEGIN TRANSACTION").await.unwrap();
    let mut f = Fixture::new("default", "postgres");
    f.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns
        .get_mut("x")
        .unwrap()
        .nullable = false;
    let mut base = f.clone();
    base.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns
        .shift_remove("x");
    base.ids.columns.remove(&"c_bbbbbb".parse().unwrap());
    execute(
        &mut db,
        base.compile_from(
            &Default::default(),
            &Default::default(),
            &ObjectDeps::default(),
            &engine,
        )
        .unwrap(),
        &engine,
    )
    .await;
    db.conn
        .execute("INSERT INTO app.t(anchor) VALUES(1)")
        .await
        .unwrap();
    execute(
        &mut db,
        f.compile_from(&base.schema, &base.ids, &f.deps, &engine)
            .unwrap(),
        &engine,
    )
    .await;
    let rows = db.conn.query("SELECT x AS value FROM app.t").await.unwrap();
    assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(7));
    db.conn.execute("COMMIT").await.unwrap();
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn scratch_facets_keep_zero_sql_paths_and_require_actual_role_setup() {
    use pbps_pg::resolver::reconstruct::Reconstruction;
    let mut db = TestDb::create("creation2469_scratch").await;
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    let rows = db
        .conn
        .query("SELECT current_setting('server_version_num')::int AS version")
        .await
        .unwrap();
    let engine = Postgres::new()
        .with_server_version(rows[0].try_get::<i32>("version").unwrap().unwrap() as i64);
    for facet in FACETS {
        let f = Fixture::new(facet, "postgres");
        let f = if *facet == "default" {
            f.with_skipped_policy()
        } else {
            f
        };
        let plan = f.bootstrap(&engine).unwrap();
        let mut scratch = Reconstruction::new(&engine, &plan, &f.ids).unwrap();
        scratch
            .compile(&engine, &pbps_model::SessionZone::utc(), &mut db.conn)
            .await
            .unwrap();
        assert!(scratch.observations().is_some(), "{facet}");
        db.conn.execute("DROP SCHEMA app CASCADE").await.unwrap();
        db.conn.execute("CREATE SCHEMA app").await.unwrap();
    }
    let role = format!("w2469c_setup_{}", std::process::id());
    let mut f = Fixture::new("default", "postgres");
    f.schema
        .roles
        .insert(role.clone().into(), pbps_model::Role::default());
    f.ids
        .roles
        .insert("r_aaaaaa".parse().unwrap(), role.clone().into());
    f.schema.modules.insert(
        "app.after_role".parse().unwrap(),
        pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            definition: "SELECT current_user::text AS role_name".into(),
            description: None,
            options: Default::default(),
        },
    );
    f.deps.0.extend([
        (
            format!("role::{role}").parse().unwrap(),
            ["default::app.t.x".parse().unwrap()].into(),
        ),
        (
            "app.after_role".parse().unwrap(),
            [format!("role::{role}").parse().unwrap()].into(),
        ),
    ]);
    let error = f.compile(&engine).unwrap_err();
    assert!(error.contains("fixed external setup boundary"), "{error}");
    f.deps
        .0
        .retain(|(owner, _)| owner != &format!("role::{role}").parse().unwrap());
    f.deps.0[0]
        .1
        .insert(format!("role::{role}").parse().unwrap());
    let base = pbps_model::Schema {
        roles: f.schema.roles.clone(),
        ..Default::default()
    };
    let base_ids = pbps_model::IdsFile {
        roles: f.ids.roles.clone(),
        ..Default::default()
    };
    let plan = f.compile_from(&base, &base_ids, &f.deps, &engine).unwrap();
    let missing = Reconstruction::new(&engine, &plan, &f.ids).unwrap();
    // Consume the actual external principal at the existing setup boundary.
    assert!(db.conn.execute(&format!("SET ROLE {role}")).await.is_err());
    assert!(missing.observations().is_none());
    db.conn
        .execute(&format!("CREATE ROLE {role}"))
        .await
        .unwrap();
    db.conn
        .execute(&format!("GRANT USAGE, CREATE ON SCHEMA app TO {role}"))
        .await
        .unwrap();
    db.conn.execute(&format!("SET ROLE {role}")).await.unwrap();
    let mut supplied = Reconstruction::new(&engine, &plan, &f.ids).unwrap();
    supplied
        .compile(&engine, &pbps_model::SessionZone::utc(), &mut db.conn)
        .await
        .unwrap();
    assert!(supplied.observations().is_some());
    let rows = db
        .conn
        .query("SELECT role_name::text AS role_name FROM app.after_role")
        .await
        .unwrap();
    assert_eq!(
        rows[0].try_get::<&str>("role_name").unwrap(),
        Some(role.as_str())
    );
    db.conn.execute("RESET ROLE").await.unwrap();
    db.drop().await;
    let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
    let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
    admin.execute(&format!("DROP ROLE {role}")).await.unwrap();
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn generated_complete_shape_is_version_gated_before_any_sql() {
    let mut db = TestDb::create("creation2469_placeholder").await;
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    let rows = db
        .conn
        .query("SELECT current_setting('server_version_num')::int AS version")
        .await
        .unwrap();
    let version = rows[0].try_get::<i32>("version").unwrap().unwrap() as i64;
    let engine = Postgres::new().with_server_version(version);
    let mut f = Fixture::new("generation", "postgres");
    f.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns
        .get_mut("g")
        .unwrap()
        .value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
        expression: "app.f()".into(),
        stored: true,
    });
    f.schema
        .modules
        .get_mut(&"app.f".parse().unwrap())
        .unwrap()
        .definition = "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN 7".into();
    f.deps.0[1].1 = ["column::app.t.g".parse().unwrap()].into();
    if version < 170000 {
        let error = f.compile(&engine).unwrap_err();
        assert!(error.contains("SET EXPRESSION"), "{error}");
        let rows = db
            .conn
            .query("SELECT to_regclass('app.t')::text AS table_name")
            .await
            .unwrap();
        assert!(rows[0].try_get::<&str>("table_name").unwrap().is_none());
    } else {
        db.conn.execute("BEGIN").await.unwrap();
        execute(&mut db, f.compile(&engine).unwrap(), &engine).await;
        db.conn
            .execute("INSERT INTO app.t(anchor,x) VALUES(1,7)")
            .await
            .unwrap();
        let rows = db.conn.query("SELECT g AS value FROM app.t").await.unwrap();
        assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(7));
        db.conn.execute("COMMIT").await.unwrap();
    }
    db.drop().await;
}

#[path = "../../../pbps-diff/tests/fixtures/forward2469.rs"]
mod forward_fixture;

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn demanded_forward_columns_preserve_generation_and_column_order() {
    for pairs in [1, 2] {
        for validators in [false, true] {
            let mut db =
                TestDb::create(&format!("creation2469_forward_{pairs}_{validators}")).await;
            db.conn.execute("CREATE SCHEMA app").await.unwrap();
            db.conn.execute("BEGIN TRANSACTION").await.unwrap();
            let rows = db
                .conn
                .query("SELECT current_setting('server_version_num')::int AS version")
                .await
                .unwrap();
            let version = rows[0].try_get::<i32>("version").unwrap().unwrap() as i64;
            let engine = Postgres::new().with_server_version(version);
            let mut f = forward_fixture::Scene::new(pairs, "postgres", validators);
            if pairs == 2 && validators {
                f.deps.0[3].1 = ["generation::app.t.a".parse().unwrap()].into();
            }
            // Native control: these are valid forward references even on
            // PG16 when the complete table is created at one boundary.
            db.conn.execute(if pairs == 1 {
                "CREATE TABLE app.original(a integer GENERATED ALWAYS AS (b + 1) STORED, b integer)"
            } else {
                "CREATE TABLE app.original(a integer GENERATED ALWAYS AS (b + 1) STORED, b integer, c integer GENERATED ALWAYS AS (d + 1) STORED, d integer)"
            }).await.unwrap();
            if version < pbps_pg::generated::SET_EXPRESSION_ARRIVED_IN {
                let error = f.compile(&engine).unwrap_err();
                assert!(error.contains("SET EXPRESSION"), "{error}");
                let rows = db
                    .conn
                    .query("SELECT (to_regclass('app.t') IS NULL) AS absent")
                    .await
                    .unwrap();
                assert_eq!(rows[0].try_get::<bool>("absent").unwrap(), Some(true));
            } else {
                let plan = f.compile(&engine).unwrap();
                execute(&mut db, plan, &engine).await;
                if validators {
                    db.conn
                        .execute("INSERT INTO app.p(k) VALUES(7)")
                        .await
                        .unwrap();
                }
                db.conn
                    .execute("INSERT INTO app.t DEFAULT VALUES")
                    .await
                    .unwrap();
                let rows = db.conn.query("SELECT a, b FROM app.t").await.unwrap();
                assert_eq!(rows[0].try_get::<i32>("a").unwrap(), Some(8));
                assert_eq!(rows[0].try_get::<i32>("b").unwrap(), Some(7));
                if pairs == 2 {
                    let rows = db.conn.query("SELECT c, d FROM app.t").await.unwrap();
                    assert_eq!(rows[0].try_get::<i32>("c").unwrap(), Some(12));
                    assert_eq!(rows[0].try_get::<i32>("d").unwrap(), Some(11));
                }
                let rows = db.conn.query("SELECT column_name FROM information_schema.columns WHERE table_schema='app' AND table_name='t' ORDER BY ordinal_position").await.unwrap();
                let actual: Vec<_> = rows
                    .iter()
                    .map(|r| r.try_get::<&str>("column_name").unwrap().unwrap())
                    .collect();
                assert_eq!(actual, f.columns);
                if validators {
                    let rows = db.conn.query("SELECT count(*)::integer AS n FROM pg_catalog.pg_indexes WHERE schemaname='app' AND tablename='t' AND indexname='ix_t'").await.unwrap();
                    assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(1));
                }
            }
            for invalid in [
                "CREATE TABLE app.bad_default(a integer DEFAULT (b + 1), b integer)",
                "CREATE TABLE app.bad_generation(a integer GENERATED ALWAYS AS (b + 1) STORED, b integer GENERATED ALWAYS AS (c + 1) STORED, c integer)",
            ] {
                db.conn.execute("SAVEPOINT invalid_forward").await.unwrap();
                let error = db.conn.execute(invalid).await.unwrap_err();
                assert!(error.server_error_code().is_some(), "{error}");
                db.conn
                    .execute("ROLLBACK TO SAVEPOINT invalid_forward")
                    .await
                    .unwrap();
            }
            db.conn.execute("COMMIT").await.unwrap();
            db.drop().await;
        }
    }
}

#[path = "../../../pbps-diff/tests/fixtures/prefix2469.rs"]
mod prefix_fixture;

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn prefix_facets_precede_the_function_and_later_column_in_native_order() {
    let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
    let mut conn = Conn::connect(Driver::Postgres, &server).await.unwrap();
    let rows = conn
        .query("SELECT current_setting('server_version_num')::int AS version")
        .await
        .unwrap();
    let engine = Postgres::new()
        .with_server_version(rows[0].try_get::<i32>("version").unwrap().unwrap() as i64);
    for facet in prefix_fixture::FACETS {
        let mut db = TestDb::create(&format!("prefix2469_{facet}")).await;
        db.conn.execute("CREATE SCHEMA app").await.unwrap();
        db.conn.execute("BEGIN TRANSACTION").await.unwrap();
        let f = prefix_fixture::scene(facet, engine.name(), false);
        let plan = f.compile(&engine).unwrap();
        let (part, function, column) = prefix_fixture::positions(plan.changes(), facet);
        assert!(part < function && function < column, "{facet}");
        let ordered = plan.ordered(PlanMode::Transactional, &engine);
        let lowered =
            lower_all(&ordered as &dyn pbps_diff::order::PlanView<Engine>, &engine).unwrap();
        for (at, group) in lowered.groups().iter().enumerate() {
            for statement in group.statements() {
                db.conn.execute(&statement.sql).await.unwrap();
            }
            if at == part || at == function {
                let rows = db.conn.query("SELECT column_name FROM information_schema.columns WHERE table_schema='app' AND table_name='t' ORDER BY ordinal_position").await.unwrap();
                let names: Vec<_> = rows
                    .iter()
                    .map(|row| row.try_get::<&str>("column_name").unwrap().unwrap())
                    .collect();
                assert_eq!(
                    names,
                    ["anchor"],
                    "the facet and function precede the physical append: {facet}"
                );
            }
        }
        db.conn
            .execute("INSERT INTO app.p(anchor) VALUES(7)")
            .await
            .unwrap();
        db.conn
            .execute(if *facet == "default" {
                "INSERT INTO app.t(x) VALUES(11)"
            } else {
                "INSERT INTO app.t(anchor,x) VALUES(7,11)"
            })
            .await
            .unwrap();
        let rows = db.conn.query("SELECT anchor,x FROM app.t").await.unwrap();
        assert_eq!(rows[0].try_get::<i32>("anchor").unwrap(), Some(7));
        assert_eq!(rows[0].try_get::<i32>("x").unwrap(), Some(11));
        db.conn.execute("COMMIT").await.unwrap();
        db.conn.execute("DROP SCHEMA app CASCADE").await.unwrap();
        db.drop().await;
        let mut scratch_db = TestDb::create(&format!("prefix2469_scratch_{facet}")).await;
        scratch_db.conn.execute("CREATE SCHEMA app").await.unwrap();
        let input = f.bootstrap(&engine).unwrap();
        let mut scratch =
            pbps_pg::resolver::reconstruct::Reconstruction::new(&engine, &input, &f.ids).unwrap();
        scratch
            .compile(
                &engine,
                &pbps_model::SessionZone::utc(),
                &mut scratch_db.conn,
            )
            .await
            .unwrap();
        assert!(scratch.observations().is_some());
        scratch_db.drop().await;
    }
    for (f, read, insert) in [
        (
            prefix_fixture::generations(),
            "SELECT a AS value,c AS other FROM app.t",
            "INSERT INTO app.t DEFAULT VALUES",
        ),
        (
            prefix_fixture::generated_not_null(),
            "SELECT g AS value,11 AS other FROM app.t",
            "INSERT INTO app.t(anchor) VALUES(1)",
        ),
    ] {
        if rows[0].try_get::<i32>("version").unwrap().unwrap() >= 170000 {
            let mut db = TestDb::create("prefix2469_generation").await;
            db.conn.execute("CREATE SCHEMA app").await.unwrap();
            db.conn.execute("BEGIN TRANSACTION").await.unwrap();
            execute(&mut db, f.compile(&engine).unwrap(), &engine).await;
            db.conn.execute(insert).await.unwrap();
            let rows = db.conn.query(read).await.unwrap();
            assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(7));
            assert_eq!(rows[0].try_get::<i32>("other").unwrap(), Some(11));
            db.conn.execute("COMMIT").await.unwrap();
            db.drop().await;
        } else {
            assert!(f.compile(&engine).unwrap_err().contains("SET EXPRESSION"));
        }
    }
}

#[path = "../../../pbps-diff/tests/fixtures/creation2469_inline.rs"]
mod required_groups;

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn required_inline_groups_preserve_create_options_and_late_columns() {
    let mut db = TestDb::create("creation2469_inline_partition").await;
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    let rows = db
        .conn
        .query("SELECT current_setting('server_version_num')::int AS version")
        .await
        .unwrap();
    let engine = Postgres::new()
        .with_server_version(rows[0].try_get::<i32>("version").unwrap().unwrap() as i64);
    db.conn.execute("BEGIN").await.unwrap();
    let f = required_groups::partitioned();
    let plan = f.compile(&engine).unwrap();
    let function = plan
        .changes()
        .changes
        .iter()
        .position(|p| matches!(p.change, pbps_model::Change::CreateModule { .. }))
        .unwrap();
    let ordered = plan.ordered(PlanMode::Transactional, &engine);
    let lowered = lower_all(&ordered as &dyn pbps_diff::order::PlanView<Engine>, &engine).unwrap();
    for (at, group) in lowered.groups().iter().enumerate() {
        for statement in group.statements() {
            db.conn.execute(&statement.sql).await.unwrap();
        }
        if at == function {
            let rows=db.conn.query("SELECT column_name FROM information_schema.columns WHERE table_schema='app' AND table_name='t' ORDER BY ordinal_position").await.unwrap();
            let columns: Vec<_> = rows
                .iter()
                .map(|r| r.try_get::<&str>("column_name").unwrap().unwrap())
                .collect();
            assert_eq!(columns, ["anchor"]);
            let rows=db.conn.query("SELECT count(*)::int AS n FROM pg_partitioned_table WHERE partrelid='app.t'::regclass").await.unwrap();
            assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(1));
        }
    }
    db.conn.execute("CREATE TABLE app.t0 PARTITION OF app.t FOR VALUES FROM(0) TO(100); INSERT INTO app.t(anchor,x) VALUES(1,7)").await.unwrap();
    let rows = db.conn.query("SELECT x FROM app.t").await.unwrap();
    assert_eq!(rows[0].try_get::<i32>("x").unwrap(), Some(7));
    let error = required_groups::require(f, "column::app.t.anchor")
        .compile(&engine)
        .unwrap_err();
    assert!(
        error.contains("inline") && error.contains("PARTITION BY"),
        "{error}"
    );
    db.conn.execute("COMMIT").await.unwrap();
    db.drop().await;
    let mut db = TestDb::create("creation2469_inline_separated").await;
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    db.conn.execute("BEGIN").await.unwrap();
    execute(
        &mut db,
        required_groups::separated_providers("postgres")
            .compile(&engine)
            .unwrap(),
        &engine,
    )
    .await;
    db.conn.execute("CREATE TABLE app.t0 PARTITION OF app.t FOR VALUES FROM(0) TO(100); INSERT INTO app.t(anchor,x,y) VALUES(1,7,11)").await.unwrap();
    let rows = db.conn.query("SELECT y FROM app.t").await.unwrap();
    assert_eq!(rows[0].try_get::<i32>("y").unwrap(), Some(11));
    let rows=db.conn.query("SELECT column_name FROM information_schema.columns WHERE table_schema='app' AND table_name='t' ORDER BY ordinal_position").await.unwrap();
    let columns: Vec<_> = rows
        .iter()
        .map(|r| r.try_get::<&str>("column_name").unwrap().unwrap())
        .collect();
    assert_eq!(columns, ["anchor", "x", "y"]);
    db.conn.execute("COMMIT").await.unwrap();
    db.drop().await;
    let mut db = TestDb::create("creation2469_inline_generation").await;
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    let rows = db
        .conn
        .query("SELECT current_setting('server_version_num')::int AS version")
        .await
        .unwrap();
    let f = required_groups::separated_generation();
    if rows[0].try_get::<i32>("version").unwrap().unwrap() >= 170000 {
        db.conn.execute("BEGIN").await.unwrap();
        execute(&mut db, f.compile(&engine).unwrap(), &engine).await;
        db.conn.execute("CREATE TABLE app.t0 PARTITION OF app.t FOR VALUES FROM(0) TO(100); INSERT INTO app.t(x,y) VALUES(7,11)").await.unwrap();
        let rows = db.conn.query("SELECT anchor,y FROM app.t").await.unwrap();
        assert_eq!(rows[0].try_get::<i32>("anchor").unwrap(), Some(7));
        assert_eq!(rows[0].try_get::<i32>("y").unwrap(), Some(11));
        let rows=db.conn.query("SELECT column_name FROM information_schema.columns WHERE table_schema='app' AND table_name='t' ORDER BY ordinal_position").await.unwrap();
        let columns: Vec<_> = rows
            .iter()
            .map(|r| r.try_get::<&str>("column_name").unwrap().unwrap())
            .collect();
        assert_eq!(columns, ["anchor", "x", "y"]);
        db.conn.execute("COMMIT").await.unwrap();
    } else {
        assert!(f.compile(&engine).unwrap_err().contains("SET EXPRESSION"));
    }
    db.drop().await;
    let mut db = TestDb::create("creation2469_inline_identity").await;
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    db.conn.execute("BEGIN").await.unwrap();
    execute(
        &mut db,
        required_groups::identity("postgres")
            .compile(&engine)
            .unwrap(),
        &engine,
    )
    .await;
    db.conn
        .execute("INSERT INTO app.t(anchor) VALUES(1)")
        .await
        .unwrap();
    let rows = db.conn.query("SELECT x FROM app.t").await.unwrap();
    assert_eq!(rows[0].try_get::<i32>("x").unwrap(), Some(1));
    let rows=db.conn.query("SELECT attnotnull AS nn FROM pg_attribute WHERE attrelid='app.t'::regclass AND attname='x'").await.unwrap();
    assert_eq!(rows[0].try_get::<bool>("nn").unwrap(), Some(true));
    db.conn.execute("COMMIT").await.unwrap();
    db.drop().await;
}
