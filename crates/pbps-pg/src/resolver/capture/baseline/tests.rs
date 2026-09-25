use super::*;
use crate::resolver::capture::{CaptureError, CaptureScope, capture, scope};
use pbps_db::{Conn, Driver};
use pbps_model::{IdsFile, Schema, StateKind, StateSnapshot};

fn empty_scope() -> CaptureScope {
    CaptureScope {
        retained: Default::default(),
        candidates: Default::default(),
    }
}

fn snapshot() -> StateSnapshot {
    StateSnapshot::new(
        StateKind::Baseline,
        Schema::default(),
        IdsFile::default(),
        "capture ledger fixture",
    )
}

async fn baseline(conn: &mut Conn) -> Result<Baseline, Failure> {
    super::super::read::owned(conn, |catalog, major| {
        Ok(scope::prepare(catalog, major, &empty_scope())?.render)
    })
    .await
    .map(|read| read.baseline)
}

async fn genuine(conn: &mut Conn) {
    conn.execute("DROP TABLE IF EXISTS public.__pbps_state, public.__pbps_lock CASCADE")
        .await
        .unwrap();
    crate::state::ensure_tables(conn).await.unwrap();
    crate::state::record(conn, &snapshot()).await.unwrap();
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_captured_baseline_requires_the_complete_ordinary_ledger_recipe() {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        let token = crate::catalog::probe_token().replace('-', "_");
        let name = format!("pbps_capture883_{token}");
        let owner = format!("pbps_capture883_owner_{token}");
        let group = format!("pbps_capture883_group_{token}");
        let reader = format!("pbps_capture883_reader_{token}");
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        admin
            .execute(&format!(
                "CREATE ROLE {owner} LOGIN; CREATE ROLE {group} NOLOGIN; CREATE ROLE {reader} LOGIN; GRANT pg_read_all_settings TO {reader}"
            ))
            .await
            .unwrap();
        let connection = format!("{base} dbname={name}");
        let roles = (owner.clone(), group.clone(), reader.clone());
        let result = tokio::task::LocalSet::new()
            .run_until(async move { tokio::task::spawn_local(exercise(connection, roles)).await })
            .await;
        admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        admin
            .execute(&format!("DROP ROLE {owner}, {group}, {reader}"))
            .await
            .unwrap();
        result.expect("ledger capture fixture failed after owned database/role cleanup");
    }
}

async fn exercise(connection: String, (owner, group, reader): (String, String, String)) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    assert!(matches!(baseline(&mut conn).await, Ok(Baseline::Absent)));
    assert!(conn.query("SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relname IN ('__pbps_state','__pbps_lock')").await.unwrap().is_empty());
    crate::state::ensure_tables(&mut conn).await.unwrap();
    assert!(matches!(baseline(&mut conn).await, Ok(Baseline::Empty)));
    crate::state::record(&mut conn, &snapshot()).await.unwrap();
    assert!(matches!(
        baseline(&mut conn).await,
        Ok(Baseline::Recorded { entry: 1, .. })
    ));
    capture(&mut conn, &empty_scope()).await.unwrap();
    conn.execute("SET quote_all_identifiers=on; SET search_path=pg_temp; SET DateStyle='SQL, DMY'")
        .await
        .unwrap();
    assert!(matches!(
        baseline(&mut conn).await,
        Ok(Baseline::Recorded { .. })
    ));
    assert_eq!(
        conn.query("SHOW quote_all_identifiers").await.unwrap()[0]
            .try_get::<&str>("quote_all_identifiers")
            .unwrap(),
        Some("on")
    );
    conn.execute("RESET quote_all_identifiers; RESET search_path; RESET DateStyle")
        .await
        .unwrap();

    // Every subset of the historical optional timeline columns is supported;
    // capture must not call the migration just to make its validator happy.
    for columns in [
        "state_version, tables_count",
        "modules_count, staged_completed, staged_total",
    ] {
        let clauses = columns
            .split(", ")
            .map(|c| format!("DROP COLUMN {c}"))
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute(&format!("ALTER TABLE public.__pbps_state {clauses}"))
            .await
            .unwrap();
        let count = conn.query("SELECT count(*)::text AS count FROM pg_attribute WHERE attrelid='public.__pbps_state'::regclass AND attnum>0 AND NOT attisdropped").await.unwrap();
        assert!(matches!(
            baseline(&mut conn).await,
            Ok(Baseline::Recorded { .. })
        ));
        let after = conn.query("SELECT count(*)::text AS count FROM pg_attribute WHERE attrelid='public.__pbps_state'::regclass AND attnum>0 AND NOT attisdropped").await.unwrap();
        assert_eq!(
            count[0].try_get::<&str>("count").unwrap(),
            after[0].try_get::<&str>("count").unwrap()
        );
    }

    genuine(&mut conn).await;
    conn.execute("DROP TABLE public.__pbps_state; CREATE TABLE public.__pbps_state(id bigint PRIMARY KEY, state_json text NOT NULL)").await.unwrap();
    let state = serde_json::to_string(&snapshot()).unwrap();
    conn.execute_with(
        "INSERT INTO public.__pbps_state VALUES(1,$1)",
        &[state.as_str().into()],
    )
    .await
    .unwrap();
    assert!(
        !crate::state::ledger_problems(&mut conn)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        capture(&mut conn, &empty_scope()).await.is_err(),
        "forged two-column ledger cannot supply a baseline"
    );

    genuine(&mut conn).await;
    conn.execute(&format!("ALTER TABLE public.__pbps_state OWNER TO {group}"))
        .await
        .unwrap();
    assert!(
        crate::state::ledger_problems(&mut conn)
            .await
            .unwrap()
            .is_empty()
    );
    capture(&mut conn, &empty_scope()).await.unwrap();
    conn.execute(&format!("ALTER TABLE public.__pbps_state OWNER TO {owner}"))
        .await
        .unwrap();
    assert!(
        !crate::state::ledger_problems(&mut conn)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        capture(&mut conn, &empty_scope()).await.is_err(),
        "unqualified owner must refuse"
    );

    for mutation in [
        "ALTER TABLE public.__pbps_state ALTER COLUMN kind TYPE varchar(17)",
        "ALTER TABLE public.__pbps_state ALTER COLUMN operator DROP NOT NULL",
        "ALTER TABLE public.__pbps_state ALTER COLUMN reason SET DEFAULT 'private-883-default'",
        "ALTER TABLE public.__pbps_state ALTER COLUMN id SET GENERATED BY DEFAULT",
        "ALTER TABLE public.__pbps_state ADD COLUMN extra integer",
        "ALTER TABLE public.__pbps_state DROP COLUMN operator",
        "ALTER TABLE public.__pbps_state DROP CONSTRAINT pk___pbps_state",
        "ALTER TABLE public.__pbps_state ADD CONSTRAINT extra_check CHECK(id>0)",
        "CREATE INDEX extra_index ON public.__pbps_state(kind)",
        "CREATE TRIGGER extra_trigger BEFORE UPDATE ON public.__pbps_state FOR EACH STATEMENT EXECUTE FUNCTION pg_catalog.suppress_redundant_updates_trigger()",
        "CREATE RULE extra_rule AS ON UPDATE TO public.__pbps_state DO ALSO NOTIFY pbps_capture883",
        "CREATE POLICY extra_policy ON public.__pbps_state USING (true)",
        "ALTER TABLE public.__pbps_state ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE public.__pbps_state FORCE ROW LEVEL SECURITY",
        "ALTER TABLE public.__pbps_state SET UNLOGGED",
        "ALTER TABLE public.__pbps_state REPLICA IDENTITY FULL",
        "ALTER SEQUENCE public.__pbps_state_id_seq INCREMENT 2",
        "CREATE TABLE public.extra_child() INHERITS(public.__pbps_state)",
        "CREATE TABLE public.extra_reference(id bigint REFERENCES public.__pbps_state(id))",
        "ALTER TABLE public.__pbps_lock ALTER COLUMN locked_by TYPE varchar(257)",
    ] {
        genuine(&mut conn).await;
        conn.execute(mutation).await.unwrap();
        assert!(
            !crate::state::ledger_problems(&mut conn)
                .await
                .unwrap()
                .is_empty(),
            "established recipe must refuse {mutation}"
        );
        let result = capture(&mut conn, &empty_scope()).await;
        let Err(error) = result else {
            panic!("capture accepted recipe mutation: {mutation}");
        };
        assert!(!error.to_string().contains("private-883-default"));
        conn.execute("DROP TABLE IF EXISTS public.extra_child, public.extra_reference CASCADE")
            .await
            .unwrap();
    }

    genuine(&mut conn).await;
    conn.execute(&format!("GRANT TRIGGER ON public.__pbps_state TO {owner}"))
        .await
        .unwrap();
    assert!(
        capture(&mut conn, &empty_scope()).await.is_err(),
        "untrusted effective trigger editor must refuse"
    );
    conn.execute(&format!("REVOKE TRIGGER ON public.__pbps_state FROM {owner}; GRANT UPDATE ON SEQUENCE public.__pbps_state_id_seq TO {owner}")).await.unwrap();
    assert!(
        capture(&mut conn, &empty_scope()).await.is_err(),
        "untrusted sequence editor must refuse"
    );
    conn.execute(&format!(
        "REVOKE UPDATE ON SEQUENCE public.__pbps_state_id_seq FROM {owner}"
    ))
    .await
    .unwrap();
    capture(&mut conn, &empty_scope()).await.unwrap();

    conn.execute("BEGIN; SET LOCAL application_name='pbps883caller'")
        .await
        .unwrap();
    assert!(matches!(
        capture(&mut conn, &empty_scope()).await,
        Err(CaptureError::CallerTransaction)
    ));
    assert_eq!(
        conn.query("SHOW application_name").await.unwrap()[0]
            .try_get::<&str>("application_name")
            .unwrap(),
        Some("pbps883caller")
    );
    conn.execute("ROLLBACK").await.unwrap();

    conn.execute(&format!("SET ROLE {reader}")).await.unwrap();
    let unreadable = capture(&mut conn, &empty_scope()).await.err();
    assert!(
        matches!(unreadable, Some(CaptureError::Read)),
        "unreadable recorded data cannot be absent or empty: {unreadable:?}"
    );
    conn.execute("RESET ROLE").await.unwrap();
    conn.execute(&format!(
        "GRANT SELECT ON public.__pbps_state TO {reader}; SET ROLE {reader}"
    ))
    .await
    .unwrap();
    capture(&mut conn, &empty_scope()).await.unwrap();
    conn.execute("RESET ROLE").await.unwrap();

    // Recipe deparsing must obey the capture's existing output qualification.
    // This output routine errors if called, so a read error would reveal that
    // the recipe ran before the required source/type closure was qualified.
    conn.execute("CREATE SCHEMA outside; CREATE TYPE outside.datum; CREATE FUNCTION outside.datum_in(cstring) RETURNS outside.datum LANGUAGE internal IMMUTABLE AS 'textin'; CREATE FUNCTION outside.datum_out(outside.datum) RETURNS cstring LANGUAGE internal IMMUTABLE AS 'shell_out'; CREATE TYPE outside.datum(INPUT=outside.datum_in,OUTPUT=outside.datum_out,INTERNALLENGTH=variable)").await.unwrap();
    for table in ["__pbps_state", "__pbps_lock"] {
        conn.execute(&format!("ALTER TABLE public.{table} ADD COLUMN private_value outside.datum DEFAULT 'private-883-output'")).await.unwrap();
        assert!(
            matches!(
                capture(&mut conn, &empty_scope()).await,
                Err(CaptureError::Coverage(
                    crate::resolver::capture::Uncovered {
                        condition: "unqualified datum output",
                        ..
                    }
                ))
            ),
            "unqualified datum output must refuse before recipe rendering: {table}"
        );
        conn.execute(&format!(
            "ALTER TABLE public.{table} DROP COLUMN private_value"
        ))
        .await
        .unwrap();
        capture(&mut conn, &empty_scope()).await.unwrap();
    }
    conn.execute("DROP SCHEMA outside CASCADE").await.unwrap();

    // A committed change between the raw snapshot and rendering cannot use
    // live catalog caches to qualify a different recipe than we captured.
    let writer = connection.clone();
    let result = super::super::read::owned(&mut conn, |catalog, major| {
        let selected = scope::prepare(catalog, major, &empty_scope())?.render;
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let mut conn = Conn::connect(Driver::Postgres, &writer).await.unwrap();
                    conn.execute(
                        "ALTER TABLE public.__pbps_state ALTER COLUMN kind TYPE varchar(17)",
                    )
                    .await
                    .unwrap();
                });
        })
        .join()
        .unwrap();
        Ok(selected)
    })
    .await;
    assert!(
        result.is_err(),
        "a committed ledger change during capture must refuse"
    );
    conn.execute("ALTER TABLE public.__pbps_state ALTER COLUMN kind TYPE varchar(16)")
        .await
        .unwrap();
    capture(&mut conn, &empty_scope()).await.unwrap();
}
