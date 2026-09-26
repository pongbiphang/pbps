//! Desired bindings resolved through a qualified run (#613).
//!
//! The engine semantics are pinned by the adapter's own live suite; this
//! pins the lifecycle: only a verified scope resolves, the declarations are
//! compiled as the reproduced deployer in the run's own database, the target
//! is captured under the scope scratch derived, and a run resolves once.

use super::*;
use pbps_model::{IdsFile, Module, ModuleKind, Schema};

fn declared(modules: &[(&str, ModuleKind, &str)]) -> Schema {
    let mut schema = Schema::default();
    for &(id, kind, definition) in modules {
        schema.modules.insert(
            id.parse().unwrap(),
            Module {
                kind,
                description: None,
                definition: definition.into(),
            },
        );
    }
    schema
}

fn bootstrap(desired: &Schema) -> Vec<pbps_model::Change> {
    let context = pbps_diff::Context {
        operator: "fixture".into(),
        today: "2026-09-25".into(),
    };
    let ids = pbps_diff::resolve(desired, &IdsFile::default(), &[], &context)
        .unwrap()
        .ids;
    let empty = Schema::default();
    pbps_diff::diff(
        pbps_diff::Side {
            schema: &empty,
            ids: &IdsFile::default(),
        },
        pbps_diff::Side {
            schema: desired,
            ids: &ids,
        },
        &pbps_pg::Postgres::new(),
        &pbps_model::Hints::default(),
    )
    .unwrap()
    .changes
    .into_iter()
    .map(|planned| planned.change)
    .collect()
}

#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn a_qualified_run_resolves_desired_bindings_against_the_target() {
    fixture();
    const SCHEMA: &str = "pbps_bind613";
    let numeric = (
        "pbps_bind613.f(numeric)",
        ModuleKind::Function,
        "(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    );
    let integer = (
        "pbps_bind613.f(integer)",
        ModuleKind::Function,
        "(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    );
    let view = ("pbps_bind613.v", ModuleKind::View, "SELECT f(1) AS x");
    let base = declared(&[numeric, view]);
    let desired = declared(&[numeric, integer, view]);
    let bootstrap = bootstrap(&desired);
    let request = BindingRequest {
        bootstrap: &bootstrap,
        desired: &desired,
        base: &base,
    };
    let mut setup =
        PeerVerifiedConn::connect(driver(), &std::env::var("PBPS_NATIVE_CONNECTION").unwrap())
            .await
            .unwrap();
    if driver() == Driver::Postgres {
        for statement in [
            "DROP SCHEMA IF EXISTS pbps_bind613 CASCADE",
            "CREATE SCHEMA pbps_bind613",
            "CREATE FUNCTION pbps_bind613.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
            "CREATE VIEW pbps_bind613.v AS SELECT pbps_bind613.f(1) AS x",
        ] {
            setup.query(statement).await.unwrap();
        }
    }
    let mut target = native_target().await;
    let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
    let mut run = server
        .open_scratch(&scratch_recipe(&mut target).await)
        .await
        .expect("scratch resources");
    // Nothing is compiled before the scope is qualified.
    let unqualified = run
        .resolve(&mut target, &request)
        .await
        .expect_err("an unqualified scope cannot resolve");
    assert!(
        unqualified.to_string().contains("not been qualified"),
        "{unqualified}"
    );
    let schemas = match driver() {
        Driver::Postgres => vec![SCHEMA.to_owned()],
        Driver::Mssql => vec!["dbo".to_owned()],
    };
    let verdict = run
        .qualify(
            &mut target,
            &ScopeRequest {
                schemas,
                write_path_extras: Vec::new(),
                planned: Vec::new(),
            },
        )
        .await
        .expect("qualify runs");
    assert_eq!(verdict, Verdict::Verified, "{verdict:?}");
    let resolved = run.resolve(&mut target, &request).await;
    match driver() {
        Driver::Postgres => {
            let assessment = resolved.expect("a verified scope resolves");
            let view = assessment
                .surfaces
                .iter()
                .find(|(object, _)| {
                    object.class == "pg_rewrite"
                        && object
                            .signature
                            .first()
                            .is_some_and(|relation| relation.name == [SCHEMA, "v"])
                })
                .map(|(_, verdict)| verdict.clone());
            assert_eq!(
                view,
                Some(pbps_db::resolver::capture::Verdict::Rebuild),
                "an arriving exact overload captures the view: {assessment:?}"
            );
            let again = run
                .resolve(&mut target, &request)
                .await
                .expect_err("a run resolves once");
            assert!(again.to_string().contains("fresh run"), "{again}");
        }
        Driver::Mssql => {
            let refused = resolved.expect_err("SQL Server has no binding adapter");
            assert!(refused.to_string().contains("#619"), "{refused}");
        }
    }
    run.close()
        .await
        .expect("cleanup removes the run's resources");
    target.check().await.unwrap();
    if driver() == Driver::Postgres {
        setup
            .query("DROP SCHEMA pbps_bind613 CASCADE")
            .await
            .unwrap();
    }
}
