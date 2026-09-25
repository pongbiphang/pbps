//! The routines a plan pins against replacement (DEC-319.1).
//!
//! The pull reads what the plan manages. This reads what it does not: every
//! routine a role short of a superuser could replace between approval and
//! apply, as the catalog row the engine will run. Which of them the plan
//! manages, and what the digest is, are the caller's; this module only reads.
//!
//! Every reference in a routine's input is an OID, never a name. An approved
//! rename changes names — a table's row type in the argument list, a grantee
//! in `proacl` — without replacing anything (measured, DEC-319.1), and the
//! engine binds all of these by OID anyway.

use pbps_db::{Conn, DbError, Row};
use pbps_dialect::TransactionFraming;

/// One routine in the pin set, as the catalog holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routine {
    /// `pg_namespace.oid`, which keys the pin: a schema the plan renames
    /// keeps it.
    pub namespace: i64,
    /// The schema's name when this was read, for messages only.
    pub schema: String,
    pub name: String,
    /// The argument types in the spelling the pull gives a routine's identity
    /// (`format_type` under the empty `search_path`), so that the caller can
    /// tell a managed routine from an unmanaged one.
    pub args: Vec<String>,
    /// The canonical JSON this routine contributes to its schema's pin.
    pub input: String,
}

/// What one read returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    /// `server_version_num`. A plan and an apply on different releases would
    /// render the same routine's row differently, so the caller compares this
    /// before any digest.
    pub server_version: String,
    pub routines: Vec<Routine>,
}

/// The owners whose routines only superusers can replace: a superuser every
/// one of whose members, by any path and with any grant options, is a
/// superuser too.
///
/// `MEMBER` rather than `USAGE` or `SET` on purpose. It over-counts a grant
/// with `INHERIT FALSE, SET FALSE`, which can replace nothing, and that errs
/// towards pinning. Narrowing it re-derives the engine's authorization, and
/// every narrowing tried dropped a routine that could be replaced (DEC-319.1,
/// #989).
const HELD_ONLY_BY_SUPERUSERS: &str = "
    SELECT r.oid
      FROM pg_catalog.pg_roles r
     WHERE r.rolsuper
       AND NOT EXISTS (
             SELECT 1
               FROM pg_catalog.pg_roles m
              WHERE NOT m.rolsuper
                AND m.oid <> r.oid
                AND pg_catalog.pg_has_role(m.oid, r.oid, 'MEMBER'))";

/// Every `reg*` column is cast to `oid`, because `to_jsonb` renders one as a
/// bare name: `aggtransfn` came back as `acc` (measured on 18.6).
/// `aggsortop` is an `oid` column already.
const AGGREGATE_ROW: &str = "
    SELECT pg_catalog.to_jsonb(a)
           || pg_catalog.jsonb_build_object(
                'aggfnoid', a.aggfnoid::oid,
                'aggtransfn', a.aggtransfn::oid,
                'aggfinalfn', a.aggfinalfn::oid,
                'aggcombinefn', a.aggcombinefn::oid,
                'aggserialfn', a.aggserialfn::oid,
                'aggdeserialfn', a.aggdeserialfn::oid,
                'aggmtransfn', a.aggmtransfn::oid,
                'aggminvtransfn', a.aggminvtransfn::oid,
                'aggmfinalfn', a.aggmfinalfn::oid)
      FROM pg_catalog.pg_aggregate a
     WHERE a.aggfnoid = p.oid";

fn pinned_routines_query() -> String {
    format!(
        "WITH super_only AS ({HELD_ONLY_BY_SUPERUSERS}),
              member_of AS (
                SELECT d.objid, e.oid AS extension, e.extowner, e.extversion
                  FROM pg_catalog.pg_depend d
                  JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid
                 WHERE d.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass
                   AND d.refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass
                   AND d.deptype = 'e')
         SELECT p.pronamespace::int8 AS namespace,
                n.nspname AS schema_name,
                p.proname AS name,
                pg_catalog.to_jsonb(ARRAY(
                    SELECT pg_catalog.format_type(u.ty, NULL)
                      FROM pg_catalog.unnest(p.proargtypes) WITH ORDINALITY AS u(ty, pos)
                     ORDER BY u.pos))::text AS args,
                pg_catalog.jsonb_build_object(
                    'oid', p.oid::int8,
                    'proc', (pg_catalog.to_jsonb(p) - 'oid' - 'proacl')
                            || pg_catalog.jsonb_build_object('prosupport', p.prosupport::oid),
                    'acl', (SELECT pg_catalog.jsonb_agg(
                                       pg_catalog.jsonb_build_array(
                                           x.grantor, x.grantee, x.privilege_type, x.is_grantable)
                                       ORDER BY x.grantor, x.grantee, x.privilege_type, x.is_grantable)
                              FROM pg_catalog.aclexplode(p.proacl) AS x),
                    'aggregate', ({AGGREGATE_ROW}),
                    'extension', (SELECT pg_catalog.jsonb_build_object(
                                             'oid', m.extension::int8, 'version', m.extversion)
                                    FROM member_of m WHERE m.objid = p.oid))::text AS input
           FROM pg_catalog.pg_proc p
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
          WHERE NOT pg_catalog.pg_is_other_temp_schema(p.pronamespace)
            AND p.pronamespace IS DISTINCT FROM pg_catalog.pg_my_temp_schema()
            AND (p.proowner NOT IN (SELECT oid FROM super_only)
                 OR EXISTS (SELECT 1 FROM member_of m
                             WHERE m.objid = p.oid
                               AND m.extowner NOT IN (SELECT oid FROM super_only)))
          ORDER BY p.pronamespace, p.oid"
    )
}

/// `format_type` qualifies a name only when it is not visible on the path, so
/// the argument spelling needs the same empty `search_path` the pull pins
/// (DECISIONS 253), and `set_config(…, true)` needs a transaction to be local
/// to.
const CANONICAL_PATH: &str = "SELECT pg_catalog.set_config('search_path', '', true) AS was_set";

/// The read's own transaction, when the caller has none open.
const OWN_TRANSACTION: TransactionFraming = TransactionFraming {
    begin: "BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY;",
    commit: "COMMIT;",
    rollback: "ROLLBACK;",
};

/// Where the read runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Within {
    /// No transaction is open; the read opens and closes its own.
    OwnTransaction,
    /// Inside the caller's open transaction, under a savepoint that is rolled
    /// back afterwards so the path it sets goes with it (DECISIONS 418).
    CallersTransaction,
}

/// Reads every routine in the pin set, managed ones included.
pub async fn read(conn: &mut Conn, within: Within) -> Result<Read, DbError> {
    match within {
        Within::OwnTransaction => {
            conn.begin(OWN_TRANSACTION).await?;
            let result = read_under_canonical_path(conn).await;
            let closed = conn.rollback(OWN_TRANSACTION).await;
            let read = result?;
            closed?;
            Ok(read)
        }
        Within::CallersTransaction => {
            conn.execute("SAVEPOINT pbps_routine_pins").await?;
            let read = read_under_canonical_path(conn).await?;
            conn.execute("ROLLBACK TO SAVEPOINT pbps_routine_pins")
                .await?;
            conn.execute("RELEASE SAVEPOINT pbps_routine_pins").await?;
            Ok(read)
        }
    }
}

async fn read_under_canonical_path(conn: &mut Conn) -> Result<Read, DbError> {
    conn.query(CANONICAL_PATH).await?;
    let version = conn
        .query("SELECT pg_catalog.current_setting('server_version_num') AS version")
        .await?;
    let server_version = version
        .first()
        .map(|row| text(row, "version"))
        .transpose()?
        .ok_or_else(|| DbError::BadRow("`server_version_num` returned no row".to_owned()))?;
    let routines = conn
        .query(&pinned_routines_query())
        .await?
        .iter()
        .map(routine)
        .collect::<Result<_, _>>()?;
    Ok(Read {
        server_version,
        routines,
    })
}

fn routine(row: &Row) -> Result<Routine, DbError> {
    let args = text(row, "args")?;
    let args: Vec<String> = serde_json::from_str(&args)
        .map_err(|e| DbError::BadRow(format!("a routine's argument list is not JSON: {e}")))?;
    Ok(Routine {
        namespace: row
            .try_get::<i64>("namespace")?
            .ok_or_else(|| null_column("namespace"))?,
        schema: text(row, "schema_name")?,
        name: text(row, "name")?,
        args,
        input: text(row, "input")?,
    })
}

fn text(row: &Row, column: &str) -> Result<String, DbError> {
    row.try_get::<&str>(column)?
        .map(str::to_owned)
        .ok_or_else(|| null_column(column))
}

fn null_column(column: &str) -> DbError {
    DbError::BadRow(format!("column `{column}` is unexpectedly NULL"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_regproc_aggregate_column_is_read_as_an_oid() {
        for column in [
            "aggfnoid",
            "aggtransfn",
            "aggfinalfn",
            "aggcombinefn",
            "aggserialfn",
            "aggdeserialfn",
            "aggmtransfn",
            "aggminvtransfn",
            "aggmfinalfn",
        ] {
            assert!(
                AGGREGATE_ROW.contains(&format!("'{column}', a.{column}::oid")),
                "{column} is not cast to oid"
            );
        }
    }

    #[test]
    fn the_routine_row_leaves_out_nothing_but_its_own_oid_and_acl() {
        let query = pinned_routines_query();
        assert!(query.contains("(pg_catalog.to_jsonb(p) - 'oid' - 'proacl')"));
        assert!(query.contains("aclexplode(p.proacl)"));
        assert!(!query.contains("pg_get_functiondef"));
    }

    #[test]
    fn temporary_schemas_are_left_out_by_the_engines_own_predicate() {
        let query = pinned_routines_query();
        assert!(query.contains("NOT pg_catalog.pg_is_other_temp_schema(p.pronamespace)"));
        assert!(query.contains("pg_catalog.pg_my_temp_schema()"));
        assert!(!query.contains("pg_temp_"));
    }
}
