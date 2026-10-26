//! What stands over a domain a plan tightens, read from the catalog
//! (DEC-1635.8, measured on PostgreSQL 16.15 and 18.6).
//!
//! A check added to a domain, and its NOT NULL, validate every column of the
//! domain and of the domains built on it. The engine refuses them, naming the
//! column, when an array or a composite holding the domain is a column
//! somewhere: `cannot alter type "d" because column "t.x" uses it`, over an
//! empty table too, and at `VALIDATE` as at `ADD`. Reading that here turns a
//! failure in the middle of an apply into a refusal by name before any
//! statement, and the same read names the tables the validation scans.

use pbps_db::{Conn, DbError, Param};
use pbps_model::{Change, ChangeSet, DomainDelta, DomainHalf, IdsFile, TypeRef};

/// The domains a plan tightens and what the validation reads.
#[derive(Debug, Default)]
pub struct Tightened {
    /// For each domain, the tables whose columns it or a domain built on it
    /// types: the tables `VALIDATE` scans under a lock held to commit.
    pub scans: Vec<(TypeRef, Vec<String>)>,
}

/// The domain, its descendants, and every array or composite that holds one;
/// then the columns of tables that use either. `direct` marks the domains.
const STANDING: &str = "WITH RECURSIVE reach(oid, direct) AS (
        SELECT pg_catalog.to_regtype($1)::oid, true
        UNION
        SELECT x.oid, x.direct
          FROM reach r
         CROSS JOIN LATERAL (
              SELECT t.oid, r.direct
                FROM pg_catalog.pg_type t
               WHERE r.direct AND t.typtype = 'd' AND t.typbasetype = r.oid
              UNION ALL
              SELECT t.typarray, false
                FROM pg_catalog.pg_type t
               WHERE t.oid = r.oid AND t.typarray <> 0
              UNION ALL
              SELECT k.reltype, false
                FROM pg_catalog.pg_attribute a
                JOIN pg_catalog.pg_class k ON k.oid = a.attrelid AND k.relkind = 'c'
               WHERE a.atttypid = r.oid AND a.attnum > 0 AND NOT a.attisdropped
         ) AS x(oid, direct)
        WHERE r.oid IS NOT NULL
     )
     SELECT DISTINCT r.direct AS direct,
            pg_catalog.format('%I.%I', n.nspname, c.relname) AS tbl,
            CASE WHEN r.direct THEN '' ELSE a.attname::text END AS col
       FROM reach r
       JOIN pg_catalog.pg_attribute a
         ON a.atttypid = r.oid AND a.attnum > 0 AND NOT a.attisdropped
       JOIN pg_catalog.pg_class c
         ON c.oid = a.attrelid AND c.relkind IN ('r', 'p', 'm') AND NOT c.relispartition
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      ORDER BY 1, 2, 3";

fn quoted(name: &TypeRef) -> Result<String, DbError> {
    let part = |ident: &str| crate::quote(ident).map_err(|e| DbError::Refused(e.to_string()));
    Ok(format!("{}.{}", part(name.schema())?, part(name.name())?))
}

/// Reads what stands over each domain whose `Tighten` half the plan holds, and
/// refuses one the engine would refuse. `stored` names a domain as the
/// database has it before the plan's renames.
pub async fn census(
    conn: &mut Conn,
    changes: &ChangeSet,
    stored: &IdsFile,
) -> Result<Tightened, DbError> {
    let mut out = Tightened::default();
    for planned in &changes.changes {
        let Change::AlterDomain {
            uid,
            name,
            from,
            to,
            half: DomainHalf::Tighten,
        } = &planned.change
        else {
            continue;
        };
        if !DomainDelta::of(from, to).is_some_and(|delta| delta.tightens()) {
            continue;
        }
        let live = stored.types.get(uid).unwrap_or(name);
        let rows = conn
            .query_with(STANDING, &[Param::Str(&quoted(live)?)])
            .await?;
        let mut scans = Vec::new();
        let mut blocked = Vec::new();
        for row in &rows {
            let direct = row.try_get::<bool>("direct")?.unwrap_or(false);
            let table = row.try_get::<&str>("tbl")?.unwrap_or_default().to_owned();
            if direct {
                scans.push(table);
            } else {
                let column = row.try_get::<&str>("col")?.unwrap_or_default().to_owned();
                blocked.push(format!("{table}.{column}"));
            }
        }
        if !blocked.is_empty() {
            return Err(DbError::Refused(format!(
                "domain {domain} cannot take a new check or NOT NULL while an array or a \
                 composite of it is a column: {columns}. PostgreSQL refuses it over any table, \
                 empty or not (`cannot alter type \"{name}\" because column \"{first}\" uses \
                 it`). Stop using the domain there first; the domain rebuild that lifts this \
                 is #1635 slice 4b",
                domain = live.shown(),
                columns = blocked.join(", "),
                name = live.name(),
                first = blocked[0],
            )));
        }
        out.scans.push((live.clone(), scans));
    }
    Ok(out)
}

/// A domain check the engine has not validated: added `NOT VALID` and never
/// validated, by hand or by another tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotValid {
    pub domain: TypeRef,
    pub check: String,
    /// The statement that validates it, for a person to run.
    pub remedy: String,
}

/// Every domain check in the database that is `NOT VALID` (`convalidated`,
/// which `constraints_query` selects with each check). The model and a
/// declaration hold no validation (DEC-1635.4, DECISIONS 251), so such a check
/// compares equal to its declaration; the plan says so (DEC-1635.8).
pub async fn not_valid(conn: &mut Conn) -> Result<Vec<NotValid>, DbError> {
    let rows = conn
        .query(
            "SELECT n.nspname::text AS schema, t.typname::text AS name, c.conname::text AS check
               FROM pg_catalog.pg_constraint c
               JOIN pg_catalog.pg_type t ON t.oid = c.contypid AND t.typtype = 'd'
               JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
              WHERE c.contype = 'c' AND NOT c.convalidated
              ORDER BY 1, 2, 3",
        )
        .await?;
    let mut out = Vec::new();
    for row in &rows {
        let schema = row.try_get::<&str>("schema")?.unwrap_or_default();
        let name = row.try_get::<&str>("name")?.unwrap_or_default();
        let check = row.try_get::<&str>("check")?.unwrap_or_default();
        let Ok(domain) = TypeRef::new(schema, name) else {
            continue;
        };
        let remedy = format!(
            "ALTER DOMAIN {} VALIDATE CONSTRAINT {};",
            quoted(&domain)?,
            crate::quote(check).map_err(|e| DbError::Refused(e.to_string()))?
        );
        out.push(NotValid {
            domain,
            check: check.to_owned(),
            remedy,
        });
    }
    Ok(out)
}
