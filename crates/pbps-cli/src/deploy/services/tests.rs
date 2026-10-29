use super::*;
use std::path::{Path, PathBuf};

struct Fixture(Project);
impl Fixture {
    fn new(tag: &str, pg: bool) -> Self {
        let root =
            std::env::temp_dir().join(format!("pbps-services1165-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(root.join("schema")).unwrap();
        let dialect = if pg { "postgres" } else { "mssql" };
        std::fs::write(
            root.join("pbps.yml"),
            format!("dialect: {dialect}\nunmanaged: ignore\n"),
        )
        .unwrap();
        let schema = if pg { "app" } else { "dbo" };
        std::fs::write(root.join("schema/t.yml"), format!("table: {schema}.t\ncolumns:\n  id: {{type: int, nullable: false}}\nprimary_key: {{name: pk_t, columns: [id]}}\n")).unwrap();
        let project = Project::load(&root.join("pbps.yml")).unwrap();
        let dialect = crate::dialect(&project).unwrap();
        let loaded = crate::load(&project, dialect.as_ref()).unwrap();
        let resolved = pbps_diff::resolve_with_annotations(
            &loaded.schema,
            &IdsFile::default(),
            &loaded.intents,
            loaded.intents.len(),
            &pbps_diff::Context {
                operator: "fixture".into(),
                today: "2026-10-29".into(),
            },
        )
        .unwrap();
        std::fs::write(
            project.ids_file(),
            serde_json::to_vec(&resolved.ids).unwrap(),
        )
        .unwrap();
        Self(project)
    }
    fn declarations(&self) -> Declarations {
        Declarations::read(&self.0, crate::dialect(&self.0).unwrap().as_ref()).unwrap()
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.root.join(name)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0.root);
    }
}

#[test]
fn bootstrap_consumes_the_loaded_declarations_and_ids_without_rereading_them() {
    for pg in [false, true] {
        let fixture = Fixture::new(
            if pg {
                "bootstrap-pg"
            } else {
                "bootstrap-mssql"
            },
            pg,
        );
        let declarations = fixture.declarations();
        std::fs::write(fixture.path("schema/t.yml"), "not a declaration").unwrap();
        std::fs::write(fixture.0.ids_file(), "not an identity file").unwrap();
        let path = fixture.path("bootstrap.sql");
        let built = crate::deploy::bootstrap(
            &fixture.0,
            None,
            Some(&path),
            declarations,
            crate::dialect(&fixture.0).unwrap(),
            "fixture",
        )
        .unwrap();
        assert!(
            built.is_none(),
            "an offline script is not a connected build"
        );
        let script = std::fs::read_to_string(path).unwrap();
        assert!(script.contains("CREATE TABLE"));
        assert!(!script.contains("not a declaration"));
    }
}

fn planned() -> Planned {
    let artifact =
        crate::fixture_ordered(pbps_model::ChangeSet::default(), &pbps_pg::Postgres::new());
    Planned {
        artifact: Box::new(artifact),
        script: Some("-- exact script\nSELECT 1;\n".into()),
        statements: 1,
        data: crate::PlanData {
            dependencies: Default::default(),
            baseline: "fixture".into(),
            time_zone: None,
            changes: 0,
            tables: 0,
            modules: 0,
            roles: 0,
            risks: Vec::new(),
            connected_checks: Vec::new(),
            type_routines: Vec::new(),
            type_routine_approval: None,
            origins: Vec::new(),
            cost: None,
            resolver_selection: None,
            resolver_assessment: None,
            rehearsal: None,
        },
        findings: Vec::new(),
    }
}

#[test]
fn publication_retains_the_exact_artifact_script_and_existing_file_permissions() {
    let fixture = Fixture::new("publication", true);
    let plan = planned();
    let expected = crate::rendered_plan(plan.artifact.as_ref()).unwrap();
    let path = fixture.path("plan.json");
    let sql = fixture.path("plan.sql");
    std::fs::write(&path, "previous bytes").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    plan.publish(Some(&path), Some(&sql), true).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
    assert_eq!(
        std::fs::read(&sql).unwrap(),
        b"-- exact script\nSELECT 1;\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}

#[test]
fn a_failed_sql_write_keeps_the_old_plan_and_a_failed_plan_write_keeps_the_written_sql() {
    let fixture = Fixture::new("publication-failure", true);
    let path = fixture.path("plan.json");
    let sql = fixture.path("plan.sql");
    std::fs::write(&path, "previous plan\n").unwrap();
    std::fs::create_dir(&sql).unwrap();
    let error = planned()
        .publish(Some(&path), Some(&sql), true)
        .unwrap_err();
    assert!(error.to_string().contains("cannot write"));
    assert_eq!(std::fs::read(&path).unwrap(), b"previous plan\n");
    std::fs::remove_dir(&sql).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let error = planned()
        .publish(Some(&path), Some(&sql), true)
        .unwrap_err();
    assert!(error.to_string().contains("cannot write"));
    assert_eq!(std::fs::read(sql).unwrap(), b"-- exact script\nSELECT 1;\n");
}

#[test]
#[ignore = "needs both pinned PostgreSQL fixtures"]
fn connected_planning_and_apply_use_in_memory_inputs_without_a_plan_file() {
    let mut failures = Vec::new();
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let server = std::env::var(variable).expect(variable);
        let name = format!("pbps_services1165_{}", std::process::id());
        let runtime = crate::db::runtime().unwrap();
        runtime.block_on(async {
            let mut admin = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &server)
                .await
                .unwrap();
            admin
                .execute(&format!("CREATE DATABASE {name}"))
                .await
                .unwrap();
        });
        let result = std::panic::catch_unwind(|| {
            let fixture = Fixture::new("connected", true);
            let connection = format!("{server} dbname={name}");
            let target = crate::db::target_from_connection(&connection, pbps_db::Driver::Postgres);
            runtime.block_on(async {
                let mut conn = crate::db::connect(&target).await.unwrap();
                conn.execute("CREATE SCHEMA app").await.unwrap();
            });
            let built = crate::deploy::bootstrap(
                &fixture.0,
                Some(&target),
                None,
                fixture.declarations(),
                crate::dialect(&fixture.0).unwrap(),
                "fixture",
            )
            .unwrap()
            .unwrap();
            assert_eq!((built.entry, built.tables), (1, 1));
            built.released.unwrap();
            let path = fixture.path("schema/t.yml");
            let declaration = std::fs::read_to_string(&path)
                .unwrap()
                .replace("primary_key:", "  label: {type: text}\nprimary_key:");
            std::fs::write(&path, declaration).unwrap();
            let loaded = crate::load(&fixture.0, &pbps_pg::Postgres::new()).unwrap();
            let ids = crate::read_ids(&fixture.0).unwrap();
            let resolved = pbps_diff::resolve_with_annotations(
                &loaded.schema,
                &ids,
                &loaded.intents,
                loaded.intents.len(),
                &pbps_diff::Context {
                    operator: "fixture".into(),
                    today: "2026-10-29".into(),
                },
            )
            .unwrap();
            std::fs::write(
                fixture.0.ids_file(),
                serde_json::to_vec(&resolved.ids).unwrap(),
            )
            .unwrap();
            let declarations = fixture.declarations();
            std::fs::write(path, "not a declaration").unwrap();
            std::fs::write(fixture.0.ids_file(), "not an identity file").unwrap();
            let result = crate::deploy::plan_db(
                &fixture.0,
                &target,
                declarations,
                crate::dialect(&fixture.0).unwrap(),
                true,
                false,
                true,
                None,
                false,
                &mut Vec::new(),
            )
            .unwrap();
            let PlanResult::Planned(plan) = result else {
                panic!("the ordinary connected plan was refused")
            };
            assert_eq!(plan.artifact.wire().changes.changes.len(), 1);
            assert!(plan.script.as_ref().unwrap().contains("label"));
            let approved = plan.artifact.checksum();
            let mut request = crate::deploy::ApplyRequest {
                plan_path: Path::new("an in-memory artifact"),
                approved_checksum: "wrong",
                allow: &Default::default(),
                staged: false,
                resume: false,
            };
            let selected = crate::dialect(&fixture.0).unwrap();
            let refused = apply_identified(
                &fixture.0,
                &target,
                &request,
                plan.artifact.wire(),
                &approved,
                &selected,
                "fixture",
            );
            assert!(
                refused
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("no longer matches")
            );
            request.approved_checksum = &approved;
            let applied = apply_identified(
                &fixture.0,
                &target,
                &request,
                plan.artifact.wire(),
                &approved,
                &selected,
                "fixture",
            )
            .unwrap();
            let Attempt::Applied { entry, released } = applied else {
                panic!("the plan did not apply")
            };
            assert_eq!(entry, 2);
            released.unwrap();
            assert!(!fixture.path("plan.json").exists());
            runtime.block_on(async {
                let mut conn = crate::db::connect(&target).await.unwrap();
                let latest = crate::engine::latest(&mut conn).await.unwrap().unwrap();
                assert_eq!(
                    latest.snapshot.plan_checksum.as_deref(),
                    Some(approved.as_str())
                );
                let rows = conn.query("SELECT label FROM app.t").await.unwrap();
                assert!(rows.is_empty());
            });
        });
        runtime.block_on(async {
            let mut admin = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &server)
                .await
                .unwrap();
            admin
                .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
                .await
                .unwrap();
        });
        if result.is_err() {
            failures.push(variable);
        }
    }
    assert!(failures.is_empty(), "failed on {failures:?}");
}
