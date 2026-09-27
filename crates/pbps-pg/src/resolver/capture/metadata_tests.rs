//! Declaration-only metadata must not turn intact engine bindings into DDL.
use super::*;
use pbps_db::{Conn, Driver};
use pbps_dialect::Dialect;
use pbps_model::resolver::{Binding, BoundSurface, Surface, SurfaceResolution};
use pbps_model::{Change, Column, Hints, IdsFile, Module, ModuleKind, Schema, Table};

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn deprecation_keeps_column_bound_modules_and_their_unmanaged_dependents() {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).unwrap();
        let db = format!(
            "pbps_metadata614_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {db}"))
            .await
            .unwrap();
        let connection = format!("{base} dbname={db}");
        let result = tokio::task::LocalSet::new()
            .run_until(async move { tokio::task::spawn_local(exercise(connection)).await })
            .await;
        admin
            .execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
            .await
            .unwrap();
        result.expect("metadata fixture failed after cleanup");
    }
}

async fn exercise(connection: String) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    conn.execute("CREATE SCHEMA app; CREATE TABLE app.t(id integer NOT NULL); INSERT INTO app.t VALUES(7); CREATE VIEW app.v AS SELECT id FROM app.t; CREATE FUNCTION app.f() RETURNS integer LANGUAGE SQL RETURN (SELECT id FROM app.t LIMIT 1); CREATE VIEW app.kept AS SELECT v.id, app.f() AS f FROM app.v AS v").await.unwrap();
    // Neither managed object can be rebuilt while this unmanaged caller exists.
    for statement in ["DROP VIEW app.v", "DROP FUNCTION app.f()"] {
        conn.execute("BEGIN").await.unwrap();
        let error = conn.execute(statement).await.unwrap_err().to_string();
        conn.execute("ROLLBACK").await.unwrap();
        assert!(error.contains("depend on it"), "{error}");
    }
    let scope = CaptureScope {
        retained: BTreeSet::new(),
        candidates: [
            CandidateClass::Relation,
            CandidateClass::Routine,
            CandidateClass::Type,
        ]
        .into_iter()
        .map(|class| CandidateSet {
            class,
            namespace: Some("app".into()),
            name: None,
        })
        .collect(),
    };
    let before = capture(&mut conn, &scope).await.unwrap();
    let column: pbps_model::ColumnRef = "app.t.id".parse().unwrap();
    let observations: Vec<_> = before
        .inputs
        .iter()
        .filter_map(|(object, input)| {
            let module = if object.class == "pg_proc" && object.name == ["app", "f"] {
                "app.f()"
            } else if object.class == "pg_rewrite"
                && object
                    .signature
                    .first()
                    .is_some_and(|owner| owner.name == ["app", "v"])
            {
                "app.v"
            } else {
                return None;
            };
            let managed_inputs = input
                .bindings
                .iter()
                .filter(|b| {
                    b.target.class == "column"
                        && b.target.name == ["id"]
                        && b.target
                            .signature
                            .first()
                            .is_some_and(|owner| owner.name == ["app", "t"])
                })
                .map(|_| Surface::Column(column.clone()))
                .collect();
            let mut bindings: Vec<_> = input
                .bindings
                .iter()
                .map(|b| Binding {
                    node: b.node.clone(),
                    path: b.path.clone(),
                    target: b.target.clone(),
                })
                .collect();
            bindings.sort();
            let observed = BoundSurface {
                object: object.clone(),
                bindings,
                managed_inputs,
            };
            assert!(
                observed
                    .managed_inputs
                    .contains(&Surface::Column(column.clone())),
                "{module}: {:?}",
                observed.bindings
            );
            Some(SurfaceResolution {
                surface: Surface::Module(module.parse().unwrap()),
                current: Some(observed.clone()),
                desired: Some(observed),
            })
        })
        .collect();
    assert_eq!(
        observations.len(),
        2,
        "both stored view and function column bindings must be observed"
    );
    let mut schema = Schema::default();
    let mut table = Table::default();
    table.columns.insert(
        "id".into(),
        Column::new("integer".parse().unwrap()).not_null(),
    );
    schema.tables.insert(column.table.clone(), table);
    for (name, kind, definition) in [
        ("app.v", ModuleKind::View, "SELECT id FROM app.t"),
        (
            "app.f()",
            ModuleKind::Function,
            "() RETURNS integer LANGUAGE SQL RETURN (SELECT id FROM app.t LIMIT 1)",
        ),
    ] {
        schema.modules.insert(
            name.parse().unwrap(),
            Module {
                kind,
                description: None,
                definition: definition.into(),
            },
        );
    }
    let ids = pbps_diff::resolve(
        &schema,
        &IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "614-test".into(),
            today: "2026-09-28".into(),
        },
    )
    .unwrap()
    .ids;
    let dialect = crate::Postgres::default();
    for reason in [Some("old API"), Some("use the new API"), None] {
        let mut desired = schema.clone();
        desired
            .tables
            .get_mut(&column.table)
            .unwrap()
            .columns
            .get_mut(&column.name)
            .unwrap()
            .deprecated = reason.map(str::to_owned);
        let ordered = pbps_diff::resolver::plan(
            pbps_diff::Side {
                schema: &schema,
                ids: &ids,
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &ids,
            },
            &Hints::default(),
            &observations,
            &dialect,
        )
        .unwrap();
        ordered.proof.validate(&ordered.changes).unwrap();
        assert!(
            matches!(&ordered.changes.changes[..], [p] if matches!(p.change, Change::SetColumnDeprecated { .. })),
            "metadata must not add module DDL: {:?}",
            ordered.changes
        );
        for planned in &ordered.changes.changes {
            assert!(
                dialect
                    .emit(&planned.change, planned.strategy)
                    .unwrap()
                    .is_empty()
            );
        }
        let (_, differences) = recapture(&mut conn, &before).await.unwrap();
        assert!(
            differences.is_empty(),
            "metadata must retain actual catalog identities, properties and bindings: {differences:?}"
        );
        let rows = conn.query("SELECT id, f FROM app.kept").await.unwrap();
        assert_eq!(rows[0].try_get::<i32>("id").unwrap(), Some(7));
        assert_eq!(rows[0].try_get::<i32>("f").unwrap(), Some(7));
        schema = desired;
    }
}
