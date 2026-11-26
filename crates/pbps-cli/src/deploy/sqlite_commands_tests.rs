//! Real-SQLite-file tests for #2950 item 7 step C PR 2: `plan --db`, `apply`,
//! `bootstrap --create`, early refusals, path display and `connected_checks`
//! casing, plus server-engine framing and output regression pins.

use super::*;
use crate::engine::CommandFeature;
use pbps_db::Driver;
use pbps_model::{Intent, PlanBaseline, PlanOrigin, Reason, SavedPlan, SessionZone};
use std::path::{Path, PathBuf};

struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pbps-sqlite-cmds-{tag}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(dir.join("schema")).unwrap();
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.path(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    fn project(&self, extra_yml: &str) -> Project {
        // `DialectName::Sqlite` is not wired yet; tests drive `Driver::Sqlite`
        // and `SelectedDialect::Sqlite` directly on real SQLite files.
        self.write(
            "pbps.yml",
            &format!("dialect: postgres\nunmanaged: ignore\n{extra_yml}"),
        );
        Project::load(&self.path("pbps.yml")).unwrap()
    }

    fn sqlite_declarations(&self, project: &Project) -> services::Declarations {
        let dialect = pbps_sqlite::dialect::Sqlite::default();
        let loaded = crate::load(project, &dialect).unwrap();
        let existing = crate::read_ids_opt(project).unwrap().unwrap_or_default();
        let resolved = pbps_diff::resolve_with_annotations(
            &loaded.schema,
            &existing,
            &loaded.intents,
            loaded.intents.len(),
            &pbps_diff::Context {
                operator: "tester".into(),
                today: "2026-11-26".into(),
            },
        )
        .unwrap();
        services::Declarations {
            loaded,
            ids: resolved.ids,
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn sqlite_dialect() -> crate::emission::SelectedDialect {
    crate::emission::SelectedDialect::Sqlite(pbps_sqlite::dialect::Sqlite::default())
}

fn sqlite_target(path: &Path) -> Target {
    db::target_from_connection(path.to_str().unwrap(), Driver::Sqlite)
}

fn run_async<F: std::future::Future>(f: F) -> F::Output {
    db::runtime().unwrap().block_on(f)
}

fn plan_sqlite(
    ws: &Workspace,
    project: &Project,
    target: &Target,
    resolver: Option<pbps_config::resolver::ResolverSelection>,
    no_rehearse: bool,
) -> anyhow::Result<services::PlanResult> {
    run_async(plan_db_selected(
        project,
        target,
        ws.sqlite_declarations(project),
        sqlite_dialect(),
        false,
        false,
        true,
        resolver,
        no_rehearse,
        None,
        &mut Vec::new(),
        None,
    ))
}

fn bootstrap_table(ws: &Workspace, db_path: &Path, table_yml: &str) -> Project {
    let project = ws.project("");
    ws.write("schema/items.yml", table_yml);
    let target = sqlite_target(db_path);
    let decls = ws.sqlite_declarations(&project);
    crate::ids_store::write(&project, &decls.ids).unwrap();
    let built = run_async(bootstrap_async(
        &project,
        Some(&target),
        None,
        true,
        decls,
        sqlite_dialect(),
        "tester",
        CatalogWarnings::Human,
    ))
    .unwrap();
    match built {
        services::Bootstrap::Built(b) => {
            assert_eq!(b.entry, 1);
            assert_eq!(b.target, db_path.to_str().unwrap());
            b.released.unwrap();
        }
        services::Bootstrap::Failed { error, .. } => panic!("bootstrap failed: {error:#}"),
        services::Bootstrap::Script => panic!("expected connected bootstrap"),
    }
    project
}

const ITEMS_V1: &str = "\
table: main.items
columns:
  id:
    type: integer
    nullable: false
  name:
    type: text
    nullable: false
primary_key:
  name: pk_items
  columns: [id]
";

const ITEMS_V2_ADD_COL: &str = "\
table: main.items
columns:
  id:
    type: integer
    nullable: false
  name:
    type: text
    nullable: false
  note:
    type: text
    nullable: true
primary_key:
  name: pk_items
  columns: [id]
";

// ---------------------------------------------------------------------------
// Item 1: `plan --db` uses read framing (`BEGIN`) and does not take write lock
// ---------------------------------------------------------------------------

#[test]
fn server_engines_keep_exact_read_transaction_framing_while_sqlite_uses_deferred_begin() {
    let zone = SessionZone::utc();
    let ms = pbps_mssql::Mssql;
    assert_eq!(
        ms.read_transaction_framing(&zone),
        ms.transaction_framing(&zone)
    );
    assert_eq!(
        ms.read_transaction_framing(&zone).begin,
        "SET XACT_ABORT ON; BEGIN TRANSACTION;"
    );

    let pg = pbps_pg::Postgres::new();
    assert_eq!(
        pg.read_transaction_framing(&zone),
        pg.transaction_framing(&zone)
    );
    assert!(
        pg.read_transaction_framing(&zone)
            .begin
            .contains("BEGIN ISOLATION LEVEL READ COMMITTED;")
    );

    let sq = pbps_sqlite::dialect::Sqlite::default();
    assert_eq!(sq.read_transaction_framing(&zone), pbps_sqlite::READ);
    assert_eq!(sq.read_transaction_framing(&zone).begin, "BEGIN");
    assert_eq!(sq.transaction_framing(&zone), pbps_sqlite::TRANSACTION);
    assert_eq!(sq.transaction_framing(&zone).begin, "BEGIN IMMEDIATE");
}

#[test]
fn plan_db_on_sqlite_succeeds_while_another_connection_holds_begin_immediate() {
    let ws = Workspace::new("read-framing");
    let db_path = ws.path("app.db");
    let project = bootstrap_table(&ws, &db_path, ITEMS_V1);

    let writer =
        pbps_db::sqlite::Connection::open(&db_path, pbps_db::sqlite::OpenMode::ExistingOnly)
            .unwrap();
    writer
        .execute_batch("INSERT INTO main.items (id, name) VALUES (1, 'committed')")
        .unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    writer
        .execute_batch("INSERT INTO main.items (id, name) VALUES (2, 'uncommitted')")
        .unwrap();

    ws.write("schema/items.yml", ITEMS_V2_ADD_COL);
    let target = sqlite_target(&db_path);
    let services::PlanResult::Planned(planned) =
        plan_sqlite(&ws, &project, &target, None, false).unwrap()
    else {
        panic!("expected plan to succeed under concurrent BEGIN IMMEDIATE");
    };
    assert_eq!(planned.data.changes, 1);
    assert_eq!(planned.artifact.wire().dialect, "sqlite");
    // Item 7: baseline description carries the SQLite path, never the placeholder.
    assert!(
        planned.data.baseline.starts_with(db_path.to_str().unwrap()),
        "baseline: {}",
        planned.data.baseline
    );
    // Item 8: every connected_checks row uses consistent "SQLite" casing.
    assert!(!planned.data.connected_checks.is_empty());
    for check in &planned.data.connected_checks {
        assert_eq!(check.engine, "SQLite", "row {}: {:?}", check.name, check);
    }
    assert!(
        planned
            .data
            .connected_checks
            .iter()
            .any(|c| c.name == "missing_roles" && c.engine == "SQLite")
    );
    assert!(
        planned
            .data
            .connected_checks
            .iter()
            .any(|c| c.name == "rename_evidence" && c.engine == "SQLite")
    );

    writer.execute_batch("ROLLBACK").unwrap();

    // Negative: an EXCLUSIVE lock blocks even reads and refuses the plan.
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let blocked = plan_sqlite(&ws, &project, &target, None, true);
    assert!(
        blocked.is_err(),
        "BEGIN EXCLUSIVE must block planning reads"
    );
    writer.execute_batch("ROLLBACK").unwrap();
}

// ---------------------------------------------------------------------------
// Item 2: SQLite in-process rehearsal on by default, opt-out, and refusals
// ---------------------------------------------------------------------------

#[test]
fn sqlite_plan_db_rehearses_by_default_and_honours_opt_out_switches() {
    let ws = Workspace::new("rehearsal-default");
    let db_path = ws.path("app.db");
    let _ = bootstrap_table(&ws, &db_path, ITEMS_V1);

    ws.write("schema/items.yml", ITEMS_V2_ADD_COL);
    let project = ws.project("environments:\n  test:\n    url_env: UNUSED\n    rehearse: false\n");
    let target = sqlite_target(&db_path);

    // Default: rehearsed in process, sealed as Rehearsal::InProcess.
    let services::PlanResult::Planned(default_plan) =
        plan_sqlite(&ws, &project, &target, None, false).unwrap()
    else {
        panic!("expected default SQLite plan to rehearse and succeed");
    };
    let record = default_plan
        .artifact
        .wire()
        .rehearsal
        .as_ref()
        .expect("default SQLite plan seals rehearsal record");
    match record {
        pbps_model::Rehearsal::InProcess { library, .. } => {
            assert!(library.raw().starts_with("SQLite "), "{library:?}");
        }
        pbps_model::Rehearsal::Converged { .. } | pbps_model::Rehearsal::Off => {
            panic!("expected Rehearsal::InProcess, got {record:?}")
        }
    }
    let detail = crate::explain::rehearsal_detail(record);
    assert_eq!(detail.outcome, "converged");
    assert_eq!(detail.scratch, Some("in-process"));
    assert_eq!(
        crate::explain::rehearsal_line(&detail),
        "converged (in-process scratch)"
    );

    // `--no-rehearse`: seals Rehearsal::Off.
    let services::PlanResult::Planned(no_rehearse_plan) =
        plan_sqlite(&ws, &project, &target, None, true).unwrap()
    else {
        panic!("expected --no-rehearse plan to succeed");
    };
    assert_eq!(
        no_rehearse_plan.artifact.wire().rehearsal,
        Some(pbps_model::Rehearsal::Off)
    );

    // Environment `rehearse: false`: seals Rehearsal::Off.
    let env_target = Target::test_owned_with_driver(
        Driver::Sqlite,
        db_path.to_str().unwrap().to_owned(),
        Vec::new(),
    );
    let services::PlanResult::Planned(env_off_plan) =
        plan_sqlite(&ws, &project, &env_target, None, false).unwrap()
    else {
        panic!("expected rehearse: false environment plan to succeed");
    };
    assert_eq!(
        env_off_plan.artifact.wire().rehearsal,
        Some(pbps_model::Rehearsal::Off)
    );
}

#[test]
fn sqlite_rehearsal_refuses_statement_and_resolution_failures_and_tolerates_preexisting_broken_view()
 {
    let ws = Workspace::new("rehearsal-refusals");
    let db_path = ws.path("app.db");
    let project = bootstrap_table(&ws, &db_path, ITEMS_V1);

    // Seed duplicate rows so adding a UNIQUE index fails during rehearsal.
    let conn = pbps_db::sqlite::Connection::open(&db_path, pbps_db::sqlite::OpenMode::ExistingOnly)
        .unwrap();
    conn.execute_batch("INSERT INTO main.items (id, name) VALUES (1, 'dup'), (2, 'dup')")
        .unwrap();
    drop(conn);

    // Negative 1: statement failure during rehearsal refuses the plan.
    ws.write(
        "schema/items.yml",
        "table: main.items\ncolumns:\n  id: {type: integer, nullable: false}\n  name: {type: text, nullable: false}\nprimary_key:\n  name: pk_items\n  columns: [id]\nindexes:\n  ux_items_name:\n    columns: [name]\n    unique: true\n",
    );
    let target = sqlite_target(&db_path);
    let services::PlanResult::Refused(report) =
        plan_sqlite(&ws, &project, &target, None, false).unwrap()
    else {
        panic!("expected rehearsal to refuse UNIQUE index violated by existing rows");
    };
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.id == "rehearsal.statement"),
        "{:?}",
        report.findings
    );

    // Negative 2: resolution failure — SQLite accepts `CREATE VIEW` over a
    // missing table at DDL time; only `pbps_sqlite::rehearsal::resolution` catches it.
    ws.write("schema/items.yml", ITEMS_V1);
    ws.write(
        "schema/v_bad.yml",
        "view: main.v_bad\ndefinition: |\n  SELECT id FROM main.no_such_table_2950\n",
    );
    let services::PlanResult::Refused(report) =
        plan_sqlite(&ws, &project, &target, None, false).unwrap()
    else {
        panic!("expected rehearsal resolution pass to refuse view over missing table");
    };
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.id == "rehearsal.resolution" && f.message.contains("v_bad")),
        "{:?}",
        report.findings
    );
    std::fs::remove_file(ws.path("schema/v_bad.yml")).unwrap();

    // Positive: a pre-existing broken view surfaces as a note without refusing.
    let conn = pbps_db::sqlite::Connection::open(&db_path, pbps_db::sqlite::OpenMode::ExistingOnly)
        .unwrap();
    conn.execute_batch("CREATE VIEW main.v_already_broken AS SELECT * FROM main.no_such_table")
        .unwrap();
    drop(conn);
    ws.write("schema/items.yml", ITEMS_V2_ADD_COL);
    let services::PlanResult::Planned(planned) =
        plan_sqlite(&ws, &project, &target, None, false).unwrap()
    else {
        panic!("pre-existing broken view must not refuse an unrelated plan");
    };
    assert!(
        planned
            .findings
            .iter()
            .any(|f| f.id == "rehearsal.note" && f.message.contains("v_already_broken")),
        "{:?}",
        planned.findings
    );
}

// ---------------------------------------------------------------------------
// Item 3: Refusals by name, before anything nonsensical is asked
// ---------------------------------------------------------------------------

#[test]
fn sqlite_refuses_resolver_even_with_no_rehearse_and_before_touching_missing_file() {
    let ws = Workspace::new("refuse-resolver");
    let project = ws.project(
        "resolvers:\n  scratch:\n    kind: server\n    url_env: NEVER_NEEDED\n\
         environments:\n  local:\n    url_env: NEVER_NEEDED\n    resolve_with: scratch\n",
    );
    ws.write("schema/items.yml", ITEMS_V1);

    let flag_err =
        crate::refuse_plan_resolver(Driver::Sqlite, &project, Some("scratch"), None).unwrap_err();
    assert_eq!(flag_err.id, "resolver.unsupported");
    assert!(flag_err.message.contains("SQLite"), "{}", flag_err.message);

    let env_err =
        crate::refuse_plan_resolver(Driver::Sqlite, &project, None, Some("local")).unwrap_err();
    assert_eq!(env_err.id, "resolver.unsupported");
    assert!(env_err.message.contains("SQLite"), "{}", env_err.message);

    // Inside `plan_db_selected` with `--no-rehearse` and a missing file:
    // refuses by name before opening or creating the file.
    let missing = ws.path("never-opened.db");
    let selection = project
        .config
        .select_resolver(Some("scratch"), None)
        .unwrap();
    let services::PlanResult::Refused(report) =
        plan_sqlite(&ws, &project, &sqlite_target(&missing), selection, true).unwrap()
    else {
        panic!("expected resolver refusal on SQLite even with --no-rehearse");
    };
    assert_eq!(report.findings[0].id, "resolver.unsupported");
    assert!(report.findings[0].message.contains("SQLite"));
    assert!(!missing.exists(), "must not create any file");

    // Negative: server engines pass `refuse_plan_resolver`.
    for driver in [Driver::Mssql, Driver::Postgres] {
        assert!(
            crate::refuse_plan_resolver(driver, &project, Some("scratch"), Some("local")).is_ok()
        );
    }
}

#[test]
fn sqlite_refuses_assert_before_demanding_assertion_connection_or_files() {
    let ws = Workspace::new("refuse-assert");
    let missing_sql = vec![PathBuf::from("does-not-exist.sql")];
    let findings = crate::plan_assert::Selection::prepare_for(
        Driver::Sqlite,
        &ws.dir,
        &missing_sql,
        Err(std::env::VarError::NotPresent),
    )
    .err()
    .expect("SQLite must refuse --assert");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].id, "plan.assertion.refused");
    assert!(findings[0].message.contains("SQLite"));
    assert!(
        !findings[0]
            .message
            .contains(crate::plan_assert::CONNECTION_VARIABLE),
        "{}",
        findings[0].message
    );

    // Negative: no assertions -> Ok(None).
    assert!(
        crate::plan_assert::Selection::prepare_for(
            Driver::Sqlite,
            &ws.dir,
            &[],
            Err(std::env::VarError::NotPresent),
        )
        .unwrap()
        .is_none()
    );

    // Negative: server engines still demand PBPS_ASSERTION_CONN first.
    for driver in [Driver::Mssql, Driver::Postgres] {
        let server_findings = crate::plan_assert::Selection::prepare_for(
            driver,
            &ws.dir,
            &missing_sql,
            Err(std::env::VarError::NotPresent),
        )
        .err()
        .unwrap();
        assert_eq!(server_findings[0].id, "plan.assertion.connection");
    }
}

#[test]
fn sqlite_refuses_server_only_intents_pull_flags_and_dev_preview_by_name() {
    let ws = Workspace::new("server-only");
    let project = ws.project("dev:\n  url_env: NEVER_SET_DEV_URL\n");

    // `dev:` refused before reading unset `dev.url_env`.
    let dev_err =
        crate::engine::require_command_feature(Driver::Sqlite, CommandFeature::DevPreview)
            .unwrap_err();
    assert!(dev_err.to_string().contains("SQLite"), "{dev_err}");
    assert!(!dev_err.to_string().contains("NEVER_SET_DEV_URL"));
    // Negative: PostgreSQL accepts DevPreview and proceeds to read `dev.url_env`.
    let pg_dev_err = crate::dev::spec(&project, None).unwrap_err();
    assert!(
        pg_dev_err.to_string().contains("NEVER_SET_DEV_URL"),
        "{pg_dev_err}"
    );

    // Pull server-only flags refused by name.
    for (data, parts, exts) in [
        (vec!["main.t".to_owned()], vec![], vec![]),
        (vec![], vec!["main.p".to_owned()], vec![]),
        (vec![], vec![], vec!["citext".to_owned()]),
    ] {
        let err = crate::refuse_pull_flags(Driver::Sqlite, &data, &parts, &exts).unwrap_err();
        assert!(err.to_string().contains("SQLite"), "{err}");
        assert!(crate::refuse_pull_flags(Driver::Postgres, &data, &parts, &exts).is_ok());
    }

    // Server-only intents refused by name; PostgreSQL accepts them.
    let reason: Reason = "cleanup".parse().unwrap();
    let table: TableName = "main.t".parse().unwrap();
    let unsupported_intents = vec![
        Intent::RenameRole {
            from: "a".into(),
            to: "b".into(),
        },
        Intent::DropRole {
            role: "a".into(),
            reason: reason.clone(),
        },
        Intent::RenameType {
            from: "main.e1".parse().unwrap(),
            to: "main.e2".parse().unwrap(),
        },
        Intent::DropType {
            name: "main.e1".parse().unwrap(),
            reason: reason.clone(),
        },
        Intent::DropExtension {
            name: "citext".into(),
            reason: reason.clone(),
        },
        Intent::RenameSearch {
            from: "config main.a".parse().unwrap(),
            to: "config main.b".parse().unwrap(),
        },
        Intent::DropSearch {
            name: "config main.a".parse().unwrap(),
            reason: reason.clone(),
        },
        Intent::DropExclusion {
            table: table.clone(),
            name: "ex".into(),
            reason: reason.clone(),
        },
        Intent::MoveRows {
            table: table.clone(),
            max_rows: 10,
            reason: reason.clone(),
        },
    ];
    for intent in &unsupported_intents {
        let err = crate::refuse_unsupported_intents(Driver::Sqlite, std::slice::from_ref(intent))
            .unwrap_err();
        assert!(err.to_string().contains("SQLite"), "{intent:?}: {err}");
        assert!(
            crate::refuse_unsupported_intents(Driver::Postgres, std::slice::from_ref(intent))
                .is_ok()
        );
    }

    // Positive: SQLite accepts table/column rename and drop intents.
    let allowed_intents = vec![
        Intent::RenameTable {
            from: table.clone(),
            to: "main.t2".parse().unwrap(),
        },
        Intent::DropTable {
            table: table.clone(),
            reason: reason.clone(),
        },
        Intent::RenameColumn {
            table: table.clone(),
            from: "a".into(),
            to: "b".into(),
        },
        Intent::DropColumn {
            column: "main.t.a".parse().unwrap(),
            reason,
        },
    ];
    assert!(crate::refuse_unsupported_intents(Driver::Sqlite, &allowed_intents).is_ok());

    for feature in [CommandFeature::TableRebuild, CommandFeature::Partitions] {
        let err = crate::engine::require_command_feature(Driver::Sqlite, feature).unwrap_err();
        assert!(err.to_string().contains("SQLite"), "{feature:?}: {err}");
    }
}

// ---------------------------------------------------------------------------
// Item 4 & Item 5: `apply` forwards preflight warnings + empty-plan fast path
// ---------------------------------------------------------------------------

#[test]
fn apply_forwards_sqlite_preflight_warnings_and_plan_uses_version_floor_id() {
    let ws = Workspace::new("preflight-warnings");
    let db_path = ws.path("app.db");
    let project = bootstrap_table(&ws, &db_path, ITEMS_V1);

    ws.write("schema/items.yml", ITEMS_V2_ADD_COL);
    let target = sqlite_target(&db_path);
    let services::PlanResult::Planned(planned) =
        plan_sqlite(&ws, &project, &target, None, false).unwrap()
    else {
        panic!("expected plan to succeed");
    };

    // SQLite plan emits version-floor warning under `plan.version-floor`,
    // never the false server id `plan.edition`.
    assert!(
        planned
            .findings
            .iter()
            .any(|f| f.id == "plan.version-floor" && f.message.contains("floor")),
        "{:?}",
        planned.findings
    );
    assert!(
        planned.findings.iter().all(|f| f.id != "plan.edition"),
        "{:?}",
        planned.findings
    );

    // `apply_async` forwards the same preflight warning through `services::notice`.
    let wire = planned.artifact.wire().clone();
    let checksum = wire.wire_fingerprint();
    let request = ApplyRequest {
        plan_path: Path::new("plan.json"),
        approved_checksum: &checksum,
        allow: &wire.changes.gated_risks(),
        staged: false,
        resume: false,
    };
    let progress = services::Progress::default();
    let attempt = run_async(services::apply_async(
        &project,
        &target,
        &request,
        &wire,
        &checksum,
        &sqlite_dialect(),
        "tester",
        Some(&progress),
    ))
    .unwrap();
    match attempt {
        services::Attempt::Applied { entry, released } => {
            assert_eq!(entry, 2);
            released.unwrap();
        }
        services::Attempt::Failed { error, .. } => panic!("apply failed: {error:#}"),
        services::Attempt::Empty => panic!("expected non-empty apply"),
    }
    let notices = progress.findings.borrow();
    assert!(
        notices
            .iter()
            .any(|f| f.id == "apply.preflight" && f.message.contains("floor")),
        "expected preflight floor warning in apply notices: {notices:?}"
    );
}

#[test]
fn empty_sqlite_plan_takes_connection_free_fast_path_without_opening_file() {
    let ws = Workspace::new("empty-apply");
    let project = ws.project("");
    let missing = ws.path("never-opened.db");
    let target = sqlite_target(&missing);

    let plan = SavedPlan::new(
        PlanOrigin::Database,
        "sqlite",
        "2026-11-26T00:00:00Z",
        PlanBaseline {
            description: missing.to_str().unwrap().into(),
            checksum: "0".repeat(64),
            database_collation: None,
            time_zone: None,
        },
        pbps_model::ChangeSet::default(),
        IdsFile::default(),
    );
    let checksum = plan.wire_fingerprint();
    let allow = std::collections::BTreeSet::new();
    let request = ApplyRequest {
        plan_path: Path::new("empty.plan.json"),
        approved_checksum: &checksum,
        allow: &allow,
        staged: false,
        resume: false,
    };
    let attempt = run_async(services::apply_async(
        &project,
        &target,
        &request,
        &plan,
        &checksum,
        &sqlite_dialect(),
        "tester",
        None,
    ))
    .unwrap();
    assert!(matches!(attempt, services::Attempt::Empty));
    assert!(
        !missing.exists(),
        "empty fast path must neither open nor create the file"
    );

    assert!(crate::engine::checks_empty_plan_under_lock(
        Driver::Postgres
    ));
    assert!(!crate::engine::checks_empty_plan_under_lock(Driver::Mssql));
    assert!(!crate::engine::checks_empty_plan_under_lock(Driver::Sqlite));
}

// ---------------------------------------------------------------------------
// Item 6: `bootstrap --create`
// ---------------------------------------------------------------------------

#[test]
fn bootstrap_create_creates_missing_file_accepts_existing_and_refuses_without_flag_or_on_servers() {
    let ws = Workspace::new("bootstrap-create");
    let project = ws.project("");
    ws.write("schema/items.yml", ITEMS_V1);

    // Missing file without `--create`: refuses, names `--create`, creates nothing.
    let missing = ws.path("missing.db");
    let err = run_async(bootstrap_async(
        &project,
        Some(&sqlite_target(&missing)),
        None,
        false,
        ws.sqlite_declarations(&project),
        sqlite_dialect(),
        "tester",
        CatalogWarnings::Human,
    ))
    .err()
    .expect("missing file without --create must refuse");
    let text = format!("{err:#}");
    assert!(text.contains("--create"), "{text}");
    assert!(!missing.exists(), "must not create file without --create");

    // Invalid declarations with `--create`: refuses before creating any file.
    let invalid_db = ws.path("not-created-invalid.db");
    ws.write(
        "schema/bad.yml",
        "table: other.bad\ncolumns:\n  id: {type: integer, nullable: false}\nprimary_key:\n  name: pk_bad\n  columns: [id]\n",
    );
    let err = run_async(bootstrap_async(
        &project,
        Some(&sqlite_target(&invalid_db)),
        None,
        true,
        ws.sqlite_declarations(&project),
        sqlite_dialect(),
        "tester",
        CatalogWarnings::Human,
    ))
    .err()
    .expect("invalid declarations must refuse before file creation");
    assert!(err.to_string().contains("problem"), "{err}");
    assert!(
        !invalid_db.exists(),
        "invalid declarations must not create a file"
    );
    std::fs::remove_file(ws.path("schema/bad.yml")).unwrap();
    let _ = std::fs::remove_dir_all(project.ids_dir());

    // `--create` on an existing empty file is accepted and creates nothing.
    ws.write("schema/items.yml", ITEMS_V1);
    let existing = ws.path("existing.db");
    let pre = pbps_db::Conn::create(Driver::Sqlite, existing.to_str().unwrap()).unwrap();
    drop(pre);
    #[cfg(unix)]
    let inode_before = {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(&existing).unwrap().ino()
    };
    let built = run_async(bootstrap_async(
        &project,
        Some(&sqlite_target(&existing)),
        None,
        true,
        ws.sqlite_declarations(&project),
        sqlite_dialect(),
        "tester",
        CatalogWarnings::Human,
    ))
    .unwrap();
    assert!(matches!(built, services::Bootstrap::Built(_)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(std::fs::metadata(&existing).unwrap().ino(), inode_before);
    }

    // Running `bootstrap --create` again on an already-bootstrapped file
    // refuses as occupied without clobbering it.
    let occupied = run_async(bootstrap_async(
        &project,
        Some(&sqlite_target(&existing)),
        None,
        true,
        ws.sqlite_declarations(&project),
        sqlite_dialect(),
        "tester",
        CatalogWarnings::Human,
    ))
    .unwrap();
    match occupied {
        services::Bootstrap::Failed { error, released } => {
            released.unwrap();
            assert!(error.to_string().contains("already has"), "{error}");
        }
        services::Bootstrap::Built(_) | services::Bootstrap::Script => {
            panic!("expected occupied refusal")
        }
    }

    // `--create` with no target refuses.
    let err = run_async(bootstrap_async(
        &project,
        None,
        Some(&ws.path("out.sql")),
        true,
        ws.sqlite_declarations(&project),
        sqlite_dialect(),
        "tester",
        CatalogWarnings::Human,
    ))
    .err()
    .expect("--create without target must refuse");
    assert!(err.to_string().contains("--create"), "{err}");

    // Negative: server engines refuse `--create` by name without connecting.
    for (driver, dialect, expected_name) in [
        (
            Driver::Mssql,
            crate::emission::SelectedDialect::Mssql(pbps_mssql::Mssql),
            "SQL Server",
        ),
        (
            Driver::Postgres,
            crate::emission::SelectedDialect::Postgres(pbps_pg::Postgres::new()),
            "PostgreSQL",
        ),
    ] {
        let server_target = db::target_from_connection("Server=127.0.0.1,1;Database=x", driver);
        let err = run_async(bootstrap_async(
            &project,
            Some(&server_target),
            None,
            true,
            ws.sqlite_declarations(&project),
            dialect,
            "tester",
            CatalogWarnings::Human,
        ))
        .err()
        .expect("server engine must refuse --create");
        assert!(
            err.to_string().contains(expected_name) && err.to_string().contains("--create"),
            "{expected_name}: {err}"
        );
    }
}
