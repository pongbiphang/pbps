use super::*;
#[path = "../fixtures/domain2469.rs"]
mod fixture;
use fixture::Fixture;
use pbps_dialect::lower::lower_all;
use pbps_model::{Declared, PlanMode};

async fn engine(db: &mut TestDb) -> Postgres {
    let rows = db
        .conn
        .query("SELECT current_setting('server_version_num')::int AS version")
        .await
        .unwrap();
    Postgres::new().with_server_version(rows[0].try_get::<i32>("version").unwrap().unwrap() as i64)
}
async fn apply(db: &mut TestDb, f: &Fixture, pg: &Postgres, mode: PlanMode, user_code: bool) {
    let plan = f.compile(pg).unwrap();
    let mut record = Declared::default();
    record.advance(plan.changes());
    let ordered = plan.ordered(mode, pg);
    let mut wire = pbps_model::SavedPlan::new(
        pbps_model::PlanOrigin::Database,
        "postgres",
        "2026-10-31T00:00:00Z",
        pbps_model::PlanBaseline {
            description: "domain facet fixture".into(),
            checksum: "baseline".into(),
            database_collation: None,
            time_zone: None,
        },
        ordered.changes().clone(),
        f.ids.clone(),
    );
    wire.mode = mode;
    wire.declared_type_pins = pbps_pg::type_refs::declared::seal(
        &Schema::default(),
        &IdsFile::default(),
        &f.schema,
        &f.ids,
    );
    let sealed = pbps_diff::order::seal(ordered.clone(), wire).unwrap();
    let wire: pbps_model::SavedPlan =
        serde_json::from_slice(&serde_json::to_vec(sealed.wire()).unwrap()).unwrap();
    assert!(wire.changes.changes.iter().all(|p| p.provenance.is_none()));
    let checksum = wire.wire_fingerprint();
    let admitted = pbps_diff::order::admit(
        wire,
        pg,
        std::path::Path::new("domain2469.plan.json"),
        &checksum,
    );
    let lowered = if mode == PlanMode::Staged {
        // These multi-change fixtures exercise the engine's staged recipe,
        // not permission to bypass the product's single-change staged gate.
        let why = admitted.unwrap_err();
        assert!(
            why.to_string().contains("exactly one logical change"),
            "{why}"
        );
        lower_all(&ordered as &dyn pbps_diff::order::PlanView<Postgres>, pg).unwrap()
    } else {
        lower_all(
            &admitted.unwrap() as &dyn pbps_diff::order::PlanView<Postgres>,
            pg,
        )
        .unwrap()
    };
    let trusted = ["postgres".into()].into();
    for group in lowered.groups() {
        if mode == PlanMode::Staged {
            db.conn.execute("BEGIN").await.unwrap();
        }
        for statement in group.statements() {
            db.conn.execute(&statement.sql).await.unwrap();
            if let Some(metadata) = &statement.declared_type {
                if user_code
                    && matches!(&metadata.definition,Some(pbps_model::ManagedType::Domain{checks,..}) if !checks.is_empty())
                {
                    let error = pbps_pg::type_refs::declared::vet(&mut db.conn, metadata, &trusted)
                        .await
                        .unwrap_err();
                    assert!(error.to_string().contains("c2469"), "{error}");
                    // Native acceptance measures the design's SQL order. The
                    // product's built-in-only code gate remains a refusal.
                    if let Some(tail) = &metadata.after_vet {
                        db.conn.execute(tail).await.unwrap();
                    }
                } else {
                    pbps_pg::type_refs::declared::vet(&mut db.conn, metadata, &trusted)
                        .await
                        .unwrap();
                }
            }
        }
        if mode == PlanMode::Staged {
            db.conn.execute("COMMIT").await.unwrap();
        }
    }
    let read = if mode == PlanMode::Transactional {
        pbps_pg::catalog::introspect_within_transaction(&mut db.conn)
            .await
            .unwrap()
    } else {
        pbps_pg::catalog::introspect(&mut db.conn).await.unwrap()
    };
    let authored = record.overlay(&read.schema);
    for (name, definition) in &f.schema.types {
        assert_eq!(&authored.types[name], definition);
    }
}
async fn bad(db: &mut TestDb, sql: &str, code: &str) {
    db.conn.execute("SAVEPOINT domain_bad").await.unwrap();
    let error = db.conn.execute(sql).await.unwrap_err();
    assert_eq!(error.server_error_code().as_deref(), Some(code), "{error}");
    db.conn
        .execute("ROLLBACK TO SAVEPOINT domain_bad")
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn domain_check_uses_the_bare_type_before_its_function_and_enforces_values() {
    for f in [Fixture::new("check", true), Fixture::indirect_check()] {
        let mut db = TestDb::create("domain2469_accept").await;
        db.conn.execute("CREATE SCHEMA c2469").await.unwrap();
        let pg = engine(&mut db).await;
        db.conn.execute("BEGIN").await.unwrap();
        apply(&mut db, &f, &pg, PlanMode::Transactional, true).await;
        let rows = db.conn.query("SELECT 7::c2469.d AS value").await.unwrap();
        assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(7));
        bad(&mut db, "SELECT (-1)::c2469.d", "23514").await;
        db.conn.execute("COMMIT").await.unwrap();
        db.conn
            .execute("DROP SCHEMA c2469 CASCADE; CREATE SCHEMA c2469")
            .await
            .unwrap();
        let input = f.bootstrap(&pg).unwrap();
        let mut scratch =
            pbps_pg::resolver::reconstruct::Reconstruction::new(&pg, &input, &f.ids).unwrap();
        scratch
            .compile(&pg, &pbps_model::SessionZone::utc(), &mut db.conn)
            .await
            .unwrap();
        assert!(scratch.observations().is_some());
        db.conn.execute("BEGIN").await.unwrap();
        db.conn.query("SELECT 7::c2469.d").await.unwrap();
        bad(&mut db, "SELECT (-1)::c2469.d", "23514").await;
        db.conn.execute("COMMIT").await.unwrap();
        db.drop().await;
    }
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn domain_facets_keep_vetted_intermediates_authored_text_and_execution_modes() {
    for mode in [PlanMode::Transactional, PlanMode::Staged] {
        for f in [
            Fixture::new("default", false),
            Fixture::new("not_null", false),
            Fixture::new("check", false),
            Fixture::multiple(),
        ] {
            let mut db = TestDb::create("domain2469_facets").await;
            db.conn.execute("CREATE SCHEMA c2469").await.unwrap();
            let pg = engine(&mut db).await;
            if mode == PlanMode::Transactional {
                db.conn.execute("BEGIN").await.unwrap();
            }
            apply(&mut db, &f, &pg, mode, false).await;
            if mode == PlanMode::Staged {
                db.conn.execute("BEGIN").await.unwrap();
            }
            db.conn
                .execute("CREATE TABLE c2469.values_seen(v c2469.d)")
                .await
                .unwrap();
            db.conn
                .execute("INSERT INTO c2469.values_seen(v) VALUES(7)")
                .await
                .unwrap();
            let definition = &f.schema.types[&"c2469.d".parse().unwrap()];
            if let pbps_model::ManagedType::Domain {
                default,
                not_null,
                checks,
                ..
            } = definition
            {
                if default.is_some() {
                    db.conn
                        .execute("INSERT INTO c2469.values_seen DEFAULT VALUES")
                        .await
                        .unwrap();
                    let rows = db
                        .conn
                        .query("SELECT count(*)::int AS n FROM c2469.values_seen WHERE v=7")
                        .await
                        .unwrap();
                    assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(2));
                }
                if *not_null {
                    bad(&mut db, "SELECT NULL::c2469.d", "23502").await;
                }
                if !checks.is_empty() {
                    bad(&mut db, "SELECT (-1)::c2469.d", "23514").await;
                }
            }
            db.conn.execute("COMMIT").await.unwrap();
            db.drop().await;
        }
    }
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn scalar_domain_columns_unlock_facets_without_changing_defaults_or_checks() {
    for facet in ["default", "not_null", "check"] {
        for suffix in [false, true] {
            let f = Fixture::column_consumer(facet, suffix, false);
            let mut db = TestDb::create("domain2469_columns").await;
            db.conn.execute("CREATE SCHEMA c2469").await.unwrap();
            let pg = engine(&mut db).await;
            db.conn.execute("BEGIN").await.unwrap();
            apply(&mut db, &f, &pg, PlanMode::Transactional, false).await;
            if facet == "default" {
                db.conn
                    .execute("INSERT INTO c2469.t(anchor) VALUES(1)")
                    .await
                    .unwrap();
            } else {
                db.conn
                    .execute("INSERT INTO c2469.t(anchor,x) VALUES(1,7)")
                    .await
                    .unwrap();
            }
            let rows = db
                .conn
                .query("SELECT x::integer AS value FROM c2469.t")
                .await
                .unwrap();
            assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(7));
            if facet == "check" {
                bad(
                    &mut db,
                    "INSERT INTO c2469.t(anchor,x) VALUES(2,-1)",
                    "23514",
                )
                .await;
            }
            if facet == "not_null" {
                bad(
                    &mut db,
                    "INSERT INTO c2469.t(anchor,x) VALUES(2,NULL)",
                    "23502",
                )
                .await;
            }
            db.conn.execute("COMMIT").await.unwrap();
            db.drop().await;
        }
    }
    // Domain defaults copied by another domain are a creation-time snapshot;
    // both legal recipes must therefore install the default before that copy.
    let mut db = TestDb::create("domain2469_copy").await;
    for schema in ["ordinary", "split"] {
        db.conn
            .execute(&format!("CREATE SCHEMA {schema}"))
            .await
            .unwrap();
    }
    db.conn.execute("CREATE DOMAIN ordinary.d AS integer DEFAULT 7; CREATE DOMAIN ordinary.child AS ordinary.d; CREATE TABLE ordinary.t(v ordinary.child)").await.unwrap();
    db.conn.execute("CREATE DOMAIN split.d AS integer; ALTER DOMAIN split.d SET DEFAULT 7; CREATE DOMAIN split.child AS split.d; CREATE TABLE split.t(v split.child)").await.unwrap();
    db.conn.execute("ALTER DOMAIN ordinary.d SET DEFAULT 9; ALTER DOMAIN split.d SET DEFAULT 9; INSERT INTO ordinary.t DEFAULT VALUES; INSERT INTO split.t DEFAULT VALUES").await.unwrap();
    let rows = db
        .conn
        .query(
            "SELECT (SELECT v::integer FROM ordinary.t) AS a,(SELECT v::integer FROM split.t) AS b",
        )
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("a").unwrap(), Some(7));
    assert_eq!(rows[0].try_get::<i32>("b").unwrap(), Some(7));
    db.conn.execute("CREATE DOMAIN split.middle AS split.d DEFAULT 11; CREATE DOMAIN split.leaf AS split.middle; CREATE TABLE split.leaf_values(v split.leaf); ALTER DOMAIN split.d SET DEFAULT 13; INSERT INTO split.leaf_values DEFAULT VALUES").await.unwrap();
    let rows = db
        .conn
        .query("SELECT v::integer AS value FROM split.leaf_values")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(11));
    db.drop().await;
}
