//! One owned, read-only snapshot and a fresh rendering-stability observation.
//! This reads inputs; coverage and runtime qualification belong to the caller
//! in this engine module, not to an arbitrary "verified" flag.

use super::logical::{Catalog, Row};
use super::{properties, queries, render::Selection};
use pbps_db::transport::QueryConnection;
use std::collections::{BTreeMap, BTreeSet};

mod cursor;

// Neither source-bearing rows nor physical witnesses implement Debug or
// Serialize. The witness is discarded when this capture has closed.
pub(super) struct Read {
    pub catalog: Catalog,
    pub major: u32,
    pub baseline: super::baseline::Baseline,
    pub session: super::session::Facts,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    #[error(transparent)]
    Coverage(#[from] super::Uncovered),
    #[error("PostgreSQL target capture requires its own transaction")]
    CallerTransaction,
    #[error("PostgreSQL target capture cannot qualify this engine version")]
    Version,
    #[error("PostgreSQL target capture could not read a required catalog")]
    Read,
    #[error("PostgreSQL target capture received incomplete catalog input")]
    Incomplete,
    #[error("PostgreSQL catalog changed while canonical definitions were rendered")]
    Changed,
    #[error("PostgreSQL session inputs changed during target capture")]
    EnvironmentChanged,
    #[error("PostgreSQL target capture could not close its read transaction")]
    Close,
}

/// The lifecycle owner must expire/drop the connection if this future is
/// cancelled. Ordinary failures roll back the transaction here; an already
/// open caller transaction is refused before BEGIN and is left untouched.
/// `qualify` is the adapter's private pre-render coverage check: in particular,
/// custom datum output must not run before its type has qualified.
pub(super) async fn owned(
    conn: &mut impl QueryConnection,
    qualify: impl FnOnce(&Catalog, u32) -> Result<Selection, Failure>,
) -> Result<Read, Failure> {
    let token = crate::catalog::probe_token();
    conn.query(&crate::catalog::probe_set(&token))
        .await
        .map_err(|_| Failure::Read)?;
    let rows = conn
        .query(crate::catalog::PROBE_READ)
        .await
        .map_err(|_| Failure::Read)?;
    let [row] = rows.as_slice() else {
        return Err(Failure::Incomplete);
    };
    let probe = row
        .try_get::<&str>("probe")
        .map_err(|_| Failure::Incomplete)?
        .ok_or(Failure::Incomplete)?;
    if probe == token {
        return Err(Failure::CallerTransaction);
    }
    conn.query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(|_| Failure::Read)?;
    let outcome = within(conn, qualify).await;
    let (read, witnesses, environment) = match outcome {
        Ok(value) => {
            if conn.query("COMMIT").await.is_err() {
                let _ = conn.query("ROLLBACK").await;
                return Err(Failure::Close);
            }
            value
        }
        Err(error) => {
            conn.query("ROLLBACK").await.map_err(|_| Failure::Close)?;
            return Err(error);
        }
    };
    // The witness cursor gets its own fresh snapshot after rendering has
    // closed. Its pages must never reuse the earlier snapshot or observe a
    // different snapshot per fetch.
    let current = fresh_witnesses(conn).await?;
    if !environment.unchanged(&super::session::observe(conn).await?) {
        return Err(Failure::EnvironmentChanged);
    }
    if current != witnesses {
        return Err(Failure::Changed);
    }
    Ok(read)
}

type Witnesses = BTreeMap<String, BTreeSet<String>>;

async fn within(
    conn: &mut impl QueryConnection,
    qualify: impl FnOnce(&Catalog, u32) -> Result<Selection, Failure>,
) -> Result<(Read, Witnesses, super::session::Observation), Failure> {
    conn.query(crate::catalog::CANONICAL_PATH)
        .await
        .map_err(|_| Failure::Read)?;
    let environment = super::session::pin(conn).await?;
    let settings = conn.query("SELECT pg_catalog.current_setting('server_version_num') AS version, pg_catalog.current_setting('transaction_isolation') AS isolation, pg_catalog.current_setting('transaction_read_only') AS read_only").await.map_err(|_| Failure::Read)?;
    let [row] = settings.as_slice() else {
        return Err(Failure::Incomplete);
    };
    if row
        .try_get::<&str>("isolation")
        .map_err(|_| Failure::Incomplete)?
        != Some("repeatable read")
        || row
            .try_get::<&str>("read_only")
            .map_err(|_| Failure::Incomplete)?
            != Some("on")
    {
        return Err(Failure::Incomplete);
    }
    let major = row
        .try_get::<&str>("version")
        .map_err(|_| Failure::Incomplete)?
        .and_then(|s| s.parse::<u32>().ok())
        .map(|n| n / 10000)
        .ok_or(Failure::Version)?;
    if !matches!(major, 16 | 18) {
        return Err(Failure::Version);
    }
    let (raw, witnesses) = batch(conn, major, None).await?;
    properties::qualify_layout(&raw, major).map_err(|_| Failure::Incomplete)?;
    let baseline = super::baseline::read(conn, &raw).await?;
    let session = environment.logical(&raw)?;
    let selected = qualify(&raw, major)?;
    let (rendered, held) = batch(conn, major, Some(&selected)).await?;
    if held != witnesses {
        return Err(Failure::Changed);
    }
    Ok((
        Read {
            catalog: rendered,
            major,
            baseline,
            session,
        },
        witnesses,
        environment,
    ))
}

async fn batch(
    conn: &mut impl QueryConnection,
    major: u32,
    selection: Option<&Selection>,
) -> Result<(Catalog, Witnesses), Failure> {
    let sql = queries::batch(major, selection).map_err(|_| Failure::Incomplete)?;
    let mut catalogs: BTreeMap<String, Vec<Row>> = properties::CLASSES
        .iter()
        .map(|class| ((*class).into(), Vec::new()))
        .collect();
    let mut witnesses = empty_witnesses();
    let mut markers = BTreeSet::new();
    cursor::read(conn, &sql, |row| {
        let class = text(&row, "part")?.ok_or(Failure::Incomplete)?;
        let members = catalogs.get_mut(class).ok_or(Failure::Incomplete)?;
        let body = text(&row, "body")?;
        let witness = text(&row, "witness")?;
        let Some(body) = body else {
            if witness.is_some() || !markers.insert(class.to_owned()) {
                return Err(Failure::Incomplete);
            }
            return Ok(());
        };
        members.push(serde_json::from_str(body).map_err(|_| Failure::Incomplete)?);
        if class == "pg_roles" {
            if witness.is_some() {
                return Err(Failure::Incomplete);
            }
        } else {
            add_witness(&mut witnesses, class, witness)?;
        }
        Ok(())
    })
    .await?;
    if markers.len() != properties::CLASSES.len() {
        return Err(Failure::Incomplete);
    }
    let catalog = Catalog::new(catalogs).map_err(|_| Failure::Incomplete)?;
    Ok((catalog, witnesses))
}

fn text<'a>(row: &'a pbps_db::Row, name: &str) -> Result<Option<&'a str>, Failure> {
    row.try_get::<&str>(name).map_err(|_| Failure::Incomplete)
}

fn empty_witnesses() -> Witnesses {
    properties::CLASSES
        .iter()
        .filter(|&&class| class != "pg_roles")
        .map(|class| ((*class).into(), BTreeSet::new()))
        .collect()
}

fn add_witness(witnesses: &mut Witnesses, class: &str, value: Option<&str>) -> Result<(), Failure> {
    let values = witnesses.get_mut(class).ok_or(Failure::Incomplete)?;
    if !values.insert(value.ok_or(Failure::Incomplete)?.to_owned()) {
        return Err(Failure::Incomplete);
    }
    Ok(())
}

async fn fresh_witnesses(conn: &mut impl QueryConnection) -> Result<Witnesses, Failure> {
    conn.query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(|_| Failure::Read)?;
    let result = async {
        conn.query(crate::catalog::CANONICAL_PATH)
            .await
            .map_err(|_| Failure::Read)?;
        let mut witnesses = empty_witnesses();
        let mut markers = BTreeSet::new();
        cursor::read(conn, &queries::witness(), |row| {
            let class = text(&row, "part")?.ok_or(Failure::Incomplete)?;
            if !witnesses.contains_key(class) {
                return Err(Failure::Incomplete);
            }
            match text(&row, "witness")? {
                Some(value) => add_witness(&mut witnesses, class, Some(value))?,
                None if markers.insert(class.to_owned()) => (),
                None => return Err(Failure::Incomplete),
            }
            Ok(())
        })
        .await?;
        if markers.len() != witnesses.len() {
            return Err(Failure::Incomplete);
        }
        Ok(witnesses)
    }
    .await;
    let end = if result.is_ok() { "COMMIT" } else { "ROLLBACK" };
    if conn.query(end).await.is_err() {
        let _ = conn.query("ROLLBACK").await;
        return Err(Failure::Close);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::logical;
    use super::*;
    use pbps_db::{Conn, Driver};

    // The writer runs on its own connection/runtime so the adapter's private,
    // synchronous pre-render qualifier can place a real committed DDL change
    // precisely between the two reads without a timing-dependent sleep.
    fn writer(connection: String, sql: &'static str) -> Result<(), Failure> {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| Failure::Read)?
                .block_on(async {
                    let mut conn = Conn::connect(Driver::Postgres, &connection)
                        .await
                        .map_err(|_| Failure::Read)?;
                    conn.execute("SET lock_timeout='2s'; SET statement_timeout='10s'")
                        .await
                        .map_err(|_| Failure::Read)?;
                    conn.execute(sql).await.map_err(|_| Failure::Read)
                })
        })
        .join()
        .map_err(|_| Failure::Read)?
    }

    fn fixture_selection(catalog: &Catalog, _: u32) -> Result<Selection, Failure> {
        // This test creates only known builtin/synthetic output types. The
        // production coordinator must qualify its own requested closure.
        let mut selection = Selection::default();
        for &class in properties::CLASSES {
            for row in &catalog.rows[class] {
                selection
                    .include(class, row)
                    .map_err(|_| Failure::Incomplete)?;
            }
        }
        Ok(selection)
    }

    async fn restored(conn: &mut Conn) {
        let rows = conn.query("SELECT current_setting('search_path') AS path, current_setting('timezone') AS zone, current_setting('transaction_isolation') AS isolation").await.unwrap();
        assert_eq!(rows[0].try_get::<&str>("path").unwrap(), Some("app"));
        assert_eq!(
            rows[0].try_get::<&str>("zone").unwrap(),
            Some("Pacific/Honolulu")
        );
        assert_eq!(
            rows[0].try_get::<&str>("isolation").unwrap(),
            Some("read committed")
        );
        let token = crate::catalog::probe_token();
        conn.query(&crate::catalog::probe_set(&token))
            .await
            .unwrap();
        let rows = conn.query(crate::catalog::PROBE_READ).await.unwrap();
        assert_ne!(
            rows[0].try_get::<&str>("probe").unwrap(),
            Some(token.as_str())
        );
    }

    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn owned_capture_rejects_mixed_rendering_and_restores_its_session() {
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
            assert!(
                !base.contains("://"),
                "capture fixtures require libpq keyword settings"
            );
            let name = format!(
                "pbps_capture612_{}",
                crate::catalog::probe_token().replace('-', "_")
            );
            let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
            admin
                .execute(&format!("CREATE DATABASE {name}"))
                .await
                .unwrap();
            let role = format!("{name}_reader");
            admin
                .execute(&format!(
                    "CREATE ROLE {role} NOLOGIN IN ROLE pg_read_all_settings"
                ))
                .await
                .unwrap();
            let fixture_role = role.clone();
            let connection = format!("{base} dbname={name}");
            // A panic in the isolated test task still reaches database cleanup.
            let result = tokio::spawn(exercise_owned_capture(connection, fixture_role)).await;
            admin
                .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
                .await
                .unwrap();
            admin.execute(&format!("DROP ROLE {role}")).await.unwrap();
            result.expect("owned capture fixture failed after cleanup");
        }
    }

    async fn exercise_owned_capture(connection: String, fixture_role: String) {
        let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
        conn.execute("CREATE SCHEMA app; CREATE TABLE app.a(id integer); CREATE VIEW app.v AS SELECT id FROM app.a; SET search_path=app; SET timezone='Pacific/Honolulu'; SET default_transaction_isolation='read committed'").await.unwrap();
        let read = owned(&mut conn, |catalog, major| {
            // A new catalog property must be noticed even though the
            // SQL projection still names only previously known fields.
            let relation = catalog.rows["pg_class"]
                .iter()
                .find(|row| {
                    logical::string(row, "relname") == Ok("pg_proc")
                        && catalog
                            .object(
                                "pg_namespace",
                                logical::number(row, "relnamespace").unwrap(),
                            )
                            .unwrap()
                            .name
                            == ["pg_catalog"]
                })
                .unwrap();
            let relation = logical::number(relation, "oid").unwrap();
            let mut rows = catalog.rows.clone();
            let mut extra = rows["pg_attribute"]
                .iter()
                .find(|row| {
                    logical::number(row, "attrelid") == Ok(relation)
                        && logical::string(row, "attname") == Ok("prosrc")
                })
                .unwrap()
                .clone();
            extra.insert("attnum".into(), serde_json::json!(30000));
            extra.insert(
                "attname".into(),
                serde_json::json!("future_resolution_input"),
            );
            rows.get_mut("pg_attribute").unwrap().push(extra);
            let newer = Catalog::new(rows).unwrap_or_else(|e| panic!("{e:?}"));
            assert!(
                properties::qualify_layout(&newer, major).is_err(),
                "unknown catalog fields must refuse complete property coverage"
            );
            fixture_selection(catalog, major)
        })
        .await
        .unwrap();
        assert!(matches!(read.major, 16 | 18));
        assert!(read.catalog.rows["pg_rewrite"].iter().any(|row| {
            row["__definitions"]["complete"]
                .as_str()
                .is_some_and(|s| s.contains("app.a"))
        }));
        restored(&mut conn).await;

        let changed = owned(&mut conn, |catalog, major| {
            writer(connection.clone(), "ALTER TABLE app.a RENAME TO changed")?;
            fixture_selection(catalog, major)
        })
        .await;
        assert!(
            matches!(changed, Err(Failure::Changed)),
            "a mixed snapshot/rendered definition was accepted"
        );
        restored(&mut conn).await;
        conn.execute("ALTER TABLE app.changed RENAME TO a")
            .await
            .unwrap();
        let aba = owned(&mut conn,|catalog,major| { writer(connection.clone(),"ALTER TABLE app.a RENAME TO temporary_name; ALTER TABLE app.temporary_name RENAME TO a")?; fixture_selection(catalog,major) }).await;
        assert!(
            matches!(aba, Err(Failure::Changed)),
            "change-and-restore escaped the rendering witness"
        );
        restored(&mut conn).await;

        let candidate = owned(&mut conn, |catalog, major| {
            writer(
                connection.clone(),
                "CREATE FUNCTION app.arriving(integer) RETURNS integer LANGUAGE SQL RETURN $1+1",
            )?;
            fixture_selection(catalog, major)
        })
        .await;
        assert!(
            matches!(candidate, Err(Failure::Changed)),
            "a candidate arriving between catalog reads must invalidate capture"
        );
        restored(&mut conn).await;
        conn.execute("DROP FUNCTION app.arriving(integer)")
            .await
            .unwrap();

        let refusal = owned(&mut conn, |_, _| Err(Failure::Incomplete)).await;
        assert!(matches!(refusal, Err(Failure::Incomplete)));
        restored(&mut conn).await;
        let scope = super::super::CaptureScope {
            retained: Default::default(),
            candidates: Default::default(),
        };
        let before_role = super::super::capture(&mut conn, &scope).await.unwrap();
        conn.execute(&format!("SET ROLE {fixture_role}"))
            .await
            .unwrap();
        let (inherited, changes) = super::super::recapture(&mut conn, &before_role)
            .await
            .unwrap();
        assert!(changes.iter().any(|change| change.change==pbps_db::resolver::capture::InputChange::Environment),"effective role must be a captured input");
        let (_, changes) = super::super::recapture(&mut conn, &inherited)
            .await
            .unwrap();
        assert!(
            changes.is_empty(),
            "unchanged inherited privileges must pass"
        );
        conn.execute("RESET ROLE").await.unwrap();
        conn.execute(&format!("ALTER ROLE {fixture_role} NOINHERIT"))
            .await
            .unwrap();
        let (_, changes) = super::super::recapture(&mut conn, &before_role)
            .await
            .unwrap();
        assert!(
            changes.iter().any(|change| {
                change.change == pbps_db::resolver::capture::InputChange::Properties
                    && change.object.as_ref().is_some_and(|id| {
                        id.class == "pg_authid" && id.name == [fixture_role.clone()]
                    })
            }),
            "role inheritance properties must be captured"
        );
        conn.execute(&format!("ALTER ROLE {fixture_role} INHERIT"))
            .await
            .unwrap();
        conn.execute(&format!(
            "REVOKE SELECT ON pg_catalog.pg_cast FROM PUBLIC; SET ROLE {fixture_role}"
        ))
        .await
        .unwrap();
        let unreadable = owned(&mut conn, |_, _| {
            panic!("unreadable catalog must fail before qualification")
        })
        .await;
        assert!(
            matches!(unreadable, Err(Failure::Read)),
            "unreadable catalog was treated as absent"
        );
        restored(&mut conn).await;
        conn.execute("RESET ROLE; GRANT SELECT ON pg_catalog.pg_cast TO PUBLIC")
            .await
            .unwrap();
        conn.execute("BEGIN; CREATE TEMP TABLE caller_owned(value integer); INSERT INTO caller_owned VALUES (1)").await.unwrap();
        let caller = owned(&mut conn, |_, _| panic!("must refuse before reading")).await;
        assert!(matches!(caller, Err(Failure::CallerTransaction)));
        let rows = conn
            .query("SELECT count(*)::text AS count FROM caller_owned")
            .await
            .unwrap();
        assert_eq!(rows[0].try_get::<&str>("count").unwrap(), Some("1"));
        conn.execute("ROLLBACK").await.unwrap();
        restored(&mut conn).await;

        // shell_out always raises without reading its datum; this
        // needs no native fixture library or invalid memory layout.
        // A raw catalog is readable even when its type cannot render.
        conn.execute("CREATE TYPE app.unqualified; CREATE FUNCTION app.type_in(cstring) RETURNS app.unqualified LANGUAGE internal IMMUTABLE AS 'textin'; CREATE FUNCTION app.type_out(app.unqualified) RETURNS cstring LANGUAGE internal AS 'shell_out'; CREATE TYPE app.unqualified (INPUT=app.type_in,OUTPUT=app.type_out,INTERNALLENGTH=variable,STORAGE=extended); CREATE TABLE app.unselected(value app.unqualified DEFAULT 'private-fixture'); CREATE TABLE app.fast_default(id integer); INSERT INTO app.fast_default VALUES (1); ALTER TABLE app.fast_default ADD value app.unqualified DEFAULT 'private-fixture'; CREATE FUNCTION app.unselected_function(value app.unqualified DEFAULT 'private-fixture') RETURNS integer LANGUAGE sql AS 'SELECT 1' ").await.unwrap();
        let unselected = owned(&mut conn, |_, _| Ok(Selection::default())).await;
        assert!(
            unselected.is_ok(),
            "an unselected output handler was invoked"
        );
        restored(&mut conn).await;
        let selected = owned(&mut conn, fixture_selection).await;
        assert!(
            matches!(selected, Err(Failure::Read)),
            "required unreadable output was accepted"
        );
        restored(&mut conn).await;
    }
}

#[cfg(test)]
mod large_tests;

#[cfg(test)]
mod subscripting_tests;

#[cfg(test)]
mod coverage_tests;

#[cfg(test)]
mod expression_tests;

#[cfg(test)]
mod range_tests;

#[cfg(test)]
mod merge_scalar_tests;

#[cfg(test)]
mod stored_tests;
