use super::*;
use pbps_db::resolver::capture::InputChange;
use pbps_db::{Conn, Driver};

fn fixture_scope() -> CaptureScope {
    CaptureScope {
        retained: BTreeSet::from([ObjectIdentity {
            class: "pg_class".into(),
            name: vec!["app".into(), "historical".into()],
            signature: vec![],
        }]),
        candidates: [
            CandidateClass::Relation,
            CandidateClass::Routine,
            CandidateClass::Type,
            CandidateClass::Operator,
        ]
        .into_iter()
        .map(|class| CandidateSet {
            class,
            namespace: Some("app".into()),
            name: None,
        })
        .chain([
            CandidateSet {
                class: CandidateClass::Cast,
                namespace: None,
                name: None,
            },
            CandidateSet {
                class: CandidateClass::Extension,
                namespace: None,
                name: None,
            },
        ])
        .collect(),
    }
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn coherent_inputs_pin_properties_membership_and_actual_bindings() {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        assert!(!base.contains("://"));
        let name = format!(
            "pbps_manifest612_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let connection = format!("{base} dbname={name}");
        let result = tokio::task::LocalSet::new()
            .run_until(async move { tokio::task::spawn_local(exercise_capture(connection)).await })
            .await;
        admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        result.expect("capture API fixture failed after cleanup");
    }
}

async fn exercise_capture(connection: String) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    conn.execute("CREATE SCHEMA app; CREATE SCHEMA earlier; CREATE TABLE app.t(id integer); SET search_path=earlier,app; CREATE VIEW app.historical AS SELECT id + 1 AS id FROM t; CREATE TABLE earlier.t(id integer); SET search_path=app").await.unwrap();
    let scope = fixture_scope();
    let original = capture(&mut conn, &scope).await.unwrap();
    let (_, unchanged) = recapture(&mut conn, &original).await.unwrap();
    assert!(
        unchanged.is_empty(),
        "complete unchanged inputs must pass: {unchanged:?}"
    );
    let historical_rule = original
        .objects()
        .find(|id| {
            id.class == "pg_rewrite"
                && id
                    .signature
                    .first()
                    .is_some_and(|owner| owner.name == ["app", "historical"])
        })
        .unwrap();
    let actual: Vec<_> = original.bound_objects(historical_rule).unwrap().collect();
    assert!(
        actual
            .iter()
            .any(|id| id.class == "pg_class" && id.name == ["app", "t"]),
        "historical relation must come from the stored target binding"
    );
    assert!(
        !actual
            .iter()
            .any(|id| id.class == "pg_class" && id.name == ["earlier", "t"])
    );
    assert!(
        actual
            .iter()
            .any(|id| id.class == "pg_operator" && id.name == ["pg_catalog", "+"]),
        "pinned builtin edges must not disappear behind empty pg_depend"
    );
    // Recreating identical objects allocates different OIDs. Neither
    // private properties nor actual binding targets may pin those IDs.
    conn.execute("DROP VIEW app.historical; DROP TABLE app.t; CREATE TABLE app.t(id integer); CREATE VIEW app.historical AS SELECT id + 1 AS id FROM app.t").await.unwrap();
    let (_, changes) = recapture(&mut conn, &original).await.unwrap();
    assert!(
        changes.is_empty(),
        "logical capture must ignore fresh physical OIDs: {changes:?}"
    );
    conn.execute(
        "CREATE FUNCTION app.arriving(integer) RETURNS integer LANGUAGE SQL RETURN $1 + 1",
    )
    .await
    .unwrap();
    let (arrived, changes) = recapture(&mut conn, &original).await.unwrap();
    assert!(changes.iter().any(|change| {
        change.change == InputChange::Added
            && change
                .object
                .as_ref()
                .is_some_and(|id| id.class == "pg_proc" && id.name == ["app", "arriving"])
    }));
    conn.execute("ALTER FUNCTION app.arriving(integer) IMMUTABLE")
        .await
        .unwrap();
    let (changed, changes) = recapture(&mut conn, &arrived).await.unwrap();
    assert!(changes.iter().any(|change| {
        change.change == InputChange::Properties
            && change
                .object
                .as_ref()
                .is_some_and(|id| id.class == "pg_proc" && id.name == ["app", "arriving"])
    }));
    conn.execute("DROP FUNCTION app.arriving(integer)")
        .await
        .unwrap();
    let (_, changes) = recapture(&mut conn, &changed).await.unwrap();
    assert!(changes.iter().any(|change| {
        change.change == InputChange::Removed
            && change
                .object
                .as_ref()
                .is_some_and(|id| id.class == "pg_proc" && id.name == ["app", "arriving"])
    }));
    crate::state::ensure_tables(&mut conn).await.unwrap();
    let (empty, changes) = recapture(&mut conn, &original).await.unwrap();
    assert!(
        changes
            .iter()
            .any(|change| change.change == InputChange::Baseline)
    );
    let snapshot = pbps_model::StateSnapshot::new(
        pbps_model::StateKind::Bootstrap,
        pbps_model::Schema::default(),
        pbps_model::IdsFile::default(),
        "capture-fixture",
    );
    crate::state::record(&mut conn, &snapshot).await.unwrap();
    let (recorded, changes) = recapture(&mut conn, &empty).await.unwrap();
    assert!(
        changes
            .iter()
            .any(|change| change.change == InputChange::Baseline)
    );
    let (_, changes) = recapture(&mut conn, &recorded).await.unwrap();
    assert!(changes.is_empty());
    conn.execute("CREATE TYPE app.a; CREATE FUNCTION app.a_in(cstring) RETURNS app.a LANGUAGE internal IMMUTABLE STRICT AS 'textin'; CREATE FUNCTION app.a_out(app.a) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'textout'; CREATE TYPE app.a (INPUT=app.a_in,OUTPUT=app.a_out,CATEGORY='S'); CREATE TYPE app.b AS ENUM ('x'); CREATE FUNCTION app.a_b(app.a) RETURNS app.b LANGUAGE sql IMMUTABLE STRICT AS 'SELECT $1::text::app.b'; CREATE FUNCTION app.a_text(app.a) RETURNS text LANGUAGE sql IMMUTABLE STRICT AS 'SELECT $1::text'; CREATE CAST (app.a AS app.b) WITH FUNCTION app.a_b(app.a) AS IMPLICIT; CREATE CAST (app.a AS text) WITH FUNCTION app.a_text(app.a) AS ASSIGNMENT; CREATE FUNCTION app.pick(app.b) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 1; CREATE FUNCTION app.pick(text) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 2; CREATE TABLE app.cast_input(x app.a); CREATE VIEW app.cast_before AS SELECT app.pick(x) FROM app.cast_input").await.unwrap();
    let before_cast = capture(&mut conn, &scope).await.unwrap();
    conn.execute("DROP CAST (app.a AS text); CREATE CAST (app.a AS text) WITH FUNCTION app.a_text(app.a) AS IMPLICIT").await.unwrap();
    let (after_cast, changes) = recapture(&mut conn, &before_cast).await.unwrap();
    assert!(
        changes
            .iter()
            .any(|change| change.change == InputChange::Properties
                && change
                    .object
                    .as_ref()
                    .is_some_and(|id| id.class == "pg_cast"
                        && id
                            .signature
                            .first()
                            .is_some_and(|ty| ty.name == ["app", "a"])
                        && id
                            .signature
                            .get(1)
                            .is_some_and(|ty| ty.name == ["pg_catalog", "text"]))),
        "identity-only manifests missed a changed cast"
    );
    assert!(
        !changes
            .iter()
            .any(|change| change.change == InputChange::Bindings),
        "the target's historical binding must stay unchanged"
    );
    conn.execute("CREATE VIEW app.cast_after AS SELECT app.pick(x) FROM app.cast_input")
        .await
        .unwrap();
    let binding_sql = "SELECT p.proargtypes::text AS signature FROM pg_rewrite r JOIN pg_depend d ON d.classid='pg_rewrite'::regclass AND d.objid=r.oid JOIN pg_proc p ON p.oid=d.refobjid AND d.refclassid='pg_proc'::regclass WHERE r.ev_class=$1::text::regclass AND p.proname='pick'";
    let old = conn
        .query_with(binding_sql, &["app.cast_before".into()])
        .await
        .unwrap();
    let new = conn
        .query_with(binding_sql, &["app.cast_after".into()])
        .await
        .unwrap();
    assert_eq!(old.len(), 1);
    assert_eq!(new.len(), 1);
    assert_ne!(
        old[0].try_get::<&str>("signature").unwrap(),
        new[0].try_get::<&str>("signature").unwrap(),
        "fresh compilation must really change its overload"
    );
    conn.execute("ALTER TYPE app.a SET (STORAGE=extended)")
        .await
        .unwrap();
    let (_, changes) = recapture(&mut conn, &after_cast).await.unwrap();
    assert!(changes.iter().any(|change| {
        change.change == InputChange::Properties
            && change
                .object
                .as_ref()
                .is_some_and(|id| id.class == "pg_type" && id.name == ["app", "a"])
    }));
    conn.execute("CREATE FUNCTION app.equal(integer,integer) RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN $1=$2; CREATE OPERATOR app.=== (FUNCTION=app.equal,LEFTARG=integer,RIGHTARG=integer,RESTRICT=pg_catalog.eqsel)").await.unwrap();
    let before_operator = capture(&mut conn, &scope).await.unwrap();
    conn.execute("ALTER OPERATOR app.=== (integer,integer) SET (RESTRICT=pg_catalog.neqsel)")
        .await
        .unwrap();
    let (_, changes) = recapture(&mut conn, &before_operator).await.unwrap();
    assert!(changes.iter().any(|change| {
        change.change == InputChange::Properties
            && change
                .object
                .as_ref()
                .is_some_and(|id| id.class == "pg_operator" && id.name == ["app", "==="])
    }));
    conn.execute(
        "CREATE SCHEMA ext_a; CREATE SCHEMA ext_b; CREATE EXTENSION hstore WITH SCHEMA ext_a",
    )
    .await
    .unwrap();
    let (before_extension, extension_runtime) = capture_with_runtime_inputs(&mut conn, &scope)
        .await
        .unwrap();
    assert!(
        extension_runtime
            .libraries
            .iter()
            .any(|library| library == "$libdir/hstore"),
        "extension native code is a required runtime input"
    );
    conn.execute("CREATE FUNCTION app.extra_handler() RETURNS language_handler AS '$libdir/plpgsql','plpgsql_call_handler' LANGUAGE c").await.unwrap();
    let standalone_scope = CaptureScope {
        retained: BTreeSet::new(),
        candidates: BTreeSet::from([CandidateSet {
            class: CandidateClass::Routine,
            namespace: Some("app".into()),
            name: Some("extra_handler".into()),
        }]),
    };
    let (standalone, standalone_runtime) =
        capture_with_runtime_inputs(&mut conn, &standalone_scope)
            .await
            .unwrap();
    let ordinary = capture(&mut conn, &standalone_scope).await.unwrap();
    assert!(standalone.compare(&ordinary).is_empty());
    assert!(
        standalone_runtime
            .libraries
            .iter()
            .any(|library| library == "$libdir/plpgsql"),
        "native code outside extension membership cannot be omitted"
    );

    conn.execute("ALTER EXTENSION hstore SET SCHEMA ext_b")
        .await
        .unwrap();
    let (_, changes) = recapture(&mut conn, &before_extension).await.unwrap();
    assert!(changes.iter().any(|change| {
        change.change == InputChange::Properties
            && change
                .object
                .as_ref()
                .is_some_and(|id| id.class == "pg_extension" && id.name == ["hstore"])
    }));
    conn.execute("UPDATE public.__pbps_state SET state_json=jsonb_set(state_json::jsonb,'{version}','2147483647')::text").await.unwrap();
    assert!(
        matches!(
            capture(&mut conn, &scope).await,
            Err(CaptureError::Coverage(Uncovered {
                condition: "state format is not supported",
                ..
            }))
        ),
        "an unreadable baseline version cannot become empty state"
    );
    crate::state::record(&mut conn, &snapshot).await.unwrap();
    let before_setting = capture(&mut conn, &scope).await.unwrap();
    conn.execute("SET array_nulls=off").await.unwrap();
    let (_, changes) = recapture(&mut conn, &before_setting).await.unwrap();
    assert!(
        changes
            .iter()
            .any(|change| change.change == InputChange::Environment)
    );
    conn.execute("SET array_nulls=on").await.unwrap();
    conn.execute("CREATE SCHEMA outside; CREATE TYPE outside.unqualified; CREATE FUNCTION outside.type_in(cstring) RETURNS outside.unqualified LANGUAGE internal IMMUTABLE AS 'textin'; CREATE FUNCTION outside.type_out(outside.unqualified) RETURNS cstring LANGUAGE internal AS 'shell_out'; CREATE TYPE outside.unqualified (INPUT=outside.type_in,OUTPUT=outside.type_out,INTERNALLENGTH=variable,STORAGE=extended); CREATE TABLE outside.t(value outside.unqualified DEFAULT 'private-fixture')").await.unwrap();
    let (_, changes) = recapture(&mut conn, &before_setting).await.unwrap();
    assert!(
        changes.is_empty(),
        "unrelated unqualified output must stay outside the render scope"
    );
    let mut unsupported_scope = scope.clone();
    unsupported_scope.candidates.insert(CandidateSet {
        class: CandidateClass::Relation,
        namespace: Some("outside".into()),
        name: None,
    });
    let unsupported = capture(&mut conn, &unsupported_scope).await;
    assert!(
        matches!(
            unsupported,
            Err(CaptureError::Coverage(Uncovered {
                condition: "unqualified datum output",
                ..
            }))
        ),
        "unqualified output must refuse before invoking it"
    );
    conn.execute("CREATE TYPE outside.modifier; CREATE FUNCTION outside.modifier_in(cstring) RETURNS outside.modifier LANGUAGE internal IMMUTABLE AS 'textin'; CREATE FUNCTION outside.modifier_out(outside.modifier) RETURNS cstring LANGUAGE internal IMMUTABLE AS 'textout'; CREATE FUNCTION outside.modifier_modout(integer) RETURNS cstring LANGUAGE internal AS 'int4out'; CREATE TYPE outside.modifier (INPUT=outside.modifier_in,OUTPUT=outside.modifier_out,TYPMOD_IN=pg_catalog.varchartypmodin,TYPMOD_OUT=outside.modifier_modout,INTERNALLENGTH=variable); CREATE TABLE outside.modifier_table(value outside.modifier(4)); CREATE VIEW outside.modifier_view AS SELECT value FROM outside.modifier_table").await.unwrap();
    let modifier_scope = CaptureScope {
        retained: BTreeSet::from([ObjectIdentity {
            class: "pg_class".into(),
            name: vec!["outside".into(), "modifier_view".into()],
            signature: vec![],
        }]),
        candidates: BTreeSet::new(),
    };
    assert!(
        matches!(
            capture(&mut conn, &modifier_scope).await,
            Err(CaptureError::Coverage(Uncovered {
                condition: "unqualified type modifier output",
                ..
            }))
        ),
        "type modifier output must qualify even without a constant"
    );
    conn.execute("REVOKE ALL ON SCHEMA app FROM postgres")
        .await
        .unwrap();
    let (_, changes) = recapture(&mut conn, &original).await.unwrap();
    assert!(changes.iter().any(|change| {
        change.change == InputChange::Properties
            && change
                .object
                .as_ref()
                .is_some_and(|id| id.class == "pg_namespace" && id.name == ["app"])
    }));
}
