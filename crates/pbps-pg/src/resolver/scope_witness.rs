//! Private, bounded catalog history for a producer's publication recheck.
//!
//! These tokens are not saved evidence: apply retains its value contract.
//! An ordinary deployer keeps public role checks when protected role history
//! cannot be read (DEC-695.1).

use super::authorization::{self, AuthorizationContext};
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::transport::QueryConnection;
use pbps_db::{DbError, Row};
use std::collections::{BTreeMap, BTreeSet};

const HALF: u64 = 1 << 31;
const EPOCH: u64 = 1 << 32;
const SECTIONS: &[&str] = &[
    "pg_database",
    "pg_namespace",
    "pg_extension",
    "pg_depend",
    "pg_proc",
    "pg_language",
    "pg_collation",
    "pg_db_role_setting",
    "pg_auth_members",
];

/// A complete observation from one owned scope snapshot. No physical token
/// implements Debug or Serialize, including through its public carrier.
///
/// ```compile_fail,E0277
/// fn disclose(value: pbps_pg::resolver::scope_witness::Witness) {
///     println!("{value:?}");
/// }
/// ```
/// ```compile_fail,E0277
/// fn persist(value: pbps_pg::resolver::scope_witness::Witness) {
///     let _ = serde_json::to_value(value);
/// }
/// ```
pub struct Witness {
    bounds: Bounds,
    rows: BTreeMap<(String, String), Entry>,
    role_versions: bool,
    scope: Vec<String>,
    principal: (String, String),
}

struct Bounds {
    lower: u64,
    upper: u64,
}

#[derive(PartialEq, Eq)]
struct Token {
    oid: u64,
    xmin: u32,
}

struct Entry {
    token: Token,
    label: String,
}

fn unknown(what: &str) -> DbError {
    DbError::BadRow(format!("target catalog history is Unknown: {what}"))
}

fn required(row: &Row, field: &str) -> Result<String, DbError> {
    row.try_get::<&str>(field)?
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| unknown("an incomplete catalog witness row"))
}

fn number(text: &str) -> Result<u64, DbError> {
    let value = text
        .parse::<u64>()
        .map_err(|_| unknown("an invalid catalog witness number"))?;
    if value.to_string() != text {
        return Err(unknown("a noncanonical catalog witness number"));
    }
    Ok(value)
}

impl Bounds {
    // SQL xmin exposes the raw 32-bit value, even on an ancient frozen tuple.
    // Do not infer visibility of subxids from pg_visible_in_snapshot. Instead
    // qualify a possible origin strictly below the oldest active top-level
    // xid; every active writer and its children are at or above that horizon.
    fn origin(&self, raw: u32) -> Result<Option<u64>, DbError> {
        if raw == 0 {
            return Err(unknown("an invalid row version"));
        }
        if raw < 3 {
            return Ok(None); // Bootstrap/Frozen IDs are never allocated to writers.
        }
        let mut full = (self.lower & !(EPOCH - 1)) | u64::from(raw);
        if full >= self.lower {
            full = full.checked_sub(EPOCH).ok_or_else(|| {
                unknown("a row version cannot be qualified before the active horizon")
            })?;
        }
        if self.lower - full >= HALF {
            return Err(unknown(
                "a row version is outside the qualified transaction horizon",
            ));
        }
        Ok(Some(full))
    }

    fn check(&self) -> Result<(), DbError> {
        if self.lower < 3 || self.upper < self.lower || self.upper - self.lower >= HALF {
            return Err(unknown(
                "the transaction horizon cannot qualify row history",
            ));
        }
        Ok(())
    }
}

impl Witness {
    /// A public description only; no row tokens enter qualification metadata.
    pub fn role_history_limitation(&self) -> Option<(&'static str, &'static str)> {
        (!self.role_versions).then_some((
            "target.role-history",
            "PostgreSQL role history is checked by value: this deployer cannot read protected role-row versions, so a role change restored between reads can be missed (DEC-695.1).",
        ))
    }

    /// Compare with the original opening observation, never an advanced baseline.
    pub fn differences(&self, fresh: &Self) -> Result<Vec<String>, DbError> {
        if self.scope != fresh.scope || self.principal != fresh.principal {
            return Ok(vec!["the target catalog history scope or principal".into()]);
        }
        if fresh.bounds.lower < self.bounds.lower || fresh.bounds.upper < self.bounds.upper {
            return Err(unknown("the transaction horizon moved backwards"));
        }
        if fresh.bounds.upper - self.bounds.lower >= HALF {
            return Err(unknown("the catalog witness transaction horizon expired"));
        }
        let mut changed = BTreeSet::new();
        if self.role_versions != fresh.role_versions {
            changed.insert("the target role-history coverage".to_owned());
        }
        for (key, old) in &self.rows {
            if let Some(origin) = self.bounds.origin(old.token.xmin)?
                && fresh
                    .bounds
                    .upper
                    .checked_sub(origin)
                    .is_none_or(|age| age >= HALF)
            {
                return Err(unknown(&format!(
                    "{} exceeded the transaction horizon",
                    old.label
                )));
            }
            if fresh.rows.get(key).is_none_or(|new| old.token != new.token) {
                changed.insert(old.label.clone());
            }
        }
        for (key, new) in &fresh.rows {
            if !self.rows.contains_key(key) {
                changed.insert(new.label.clone());
            }
        }
        Ok(changed.into_iter().collect())
    }
}

fn insert(
    rows: &mut BTreeMap<(String, String), Entry>,
    bounds: &Bounds,
    row: &Row,
) -> Result<(), DbError> {
    let kind = required(row, "kind")?;
    if !SECTIONS.contains(&kind.as_str()) && kind != "pg_authid" {
        return Err(unknown("an unclassified witness section"));
    }
    let key = required(row, "key")?;
    // The composite key must be structural, never a delimiter-joined name.
    let parts: Vec<String> =
        serde_json::from_str(&key).map_err(|_| unknown("a malformed witness key"))?;
    if parts.is_empty() {
        return Err(unknown("an empty witness key"));
    }
    let label = format!(
        "target catalog row {kind} {}",
        pbps_model::RoleName::from(required(row, "label")?).shown()
    );
    let xmin = u32::try_from(number(&required(row, "version")?)?)
        .map_err(|_| unknown("an oversized row version"))?;
    bounds
        .origin(xmin)
        .map_err(|_| unknown(&format!("{label} has an unqualified row version")))?;
    let oid = number(&required(row, "oid")?)?;
    if oid == 0 && !matches!(kind.as_str(), "pg_depend" | "pg_db_role_setting") {
        return Err(unknown("a catalog row identity is missing"));
    }
    let token = Token { oid, xmin };
    if rows.insert((kind, key), Entry { token, label }).is_some() {
        return Err(unknown("a duplicate catalog witness key"));
    }
    Ok(())
}

/// Read only inside the collector's owned, repeatable-read snapshot.
pub(super) async fn read(
    conn: &mut impl QueryConnection,
    schemas: &[String],
    catalog: &CatalogFacts,
    authorization: &AuthorizationContext,
) -> Result<Witness, DbError> {
    let anchor = conn.query("SELECT pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())::text AS lower, pg_catalog.pg_snapshot_xmax(pg_catalog.pg_current_snapshot())::text AS upper").await?;
    let [anchor] = anchor.as_slice() else {
        return Err(unknown("the snapshot anchor is missing or ambiguous"));
    };
    let bounds = Bounds {
        lower: number(&required(anchor, "lower")?)?,
        upper: number(&required(anchor, "upper")?)?,
    };
    bounds.check()?;
    let names = |names: Vec<String>| {
        if names.is_empty() {
            "NULL".to_owned()
        } else {
            names
                .iter()
                .map(|s| authorization::setting_literal(s))
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let mut scope = schemas.to_vec();
    scope.sort();
    scope.dedup();
    let schema_names = names(scope.clone());
    let mut roles: BTreeSet<String> = authorization
        .roles
        .keys()
        .map(|n| n.raw().to_owned())
        .collect();
    roles.insert(authorization.principal.login.clone());
    roles.insert(authorization.principal.effective.raw().to_owned());
    let role_names = names(roles.iter().cloned().collect());
    let sql = catalog_sql(&schema_names, &role_names);
    let mut rows = BTreeMap::new();
    let mut complete = BTreeSet::new();
    for row in conn.query(&sql).await? {
        if row.try_get::<&str>("kind")? == Some("complete") {
            let section = required(&row, "key")?;
            if !SECTIONS.contains(&section.as_str()) || !complete.insert(section) {
                return Err(unknown("an invalid section completion marker"));
            }
        } else {
            insert(&mut rows, &bounds, &row)?;
        }
    }
    if complete.len() != SECTIONS.len() {
        return Err(unknown("the catalog witness inventory is incomplete"));
    }
    let labels = |kind: &str| {
        rows.iter()
            .filter(|((k, _), _)| k == kind)
            .map(|(_, v)| v.label.clone())
            .collect::<Vec<_>>()
    };
    if labels("pg_database").len() != 1
        || labels("pg_extension").len() != catalog.extensions.len()
        || labels("pg_collation").len() + 1 != catalog.collations.len()
    {
        return Err(unknown(
            "the witness inventory disagrees with the scope facts",
        ));
    }
    for schema in &scope {
        let label = format!(
            "target catalog row pg_namespace {}",
            pbps_model::RoleName::from(schema.as_str()).shown()
        );
        if !rows.values().any(|v| v.label == label) {
            return Err(unknown("an authorization namespace witness is missing"));
        }
    }

    // Probe the actual restricted query, not a Rust privilege approximation.
    // Only protected role history's measured 42501 has the approved limitation;
    // every other failed read stays an error. Separate statements are required
    // to recover a failed PostgreSQL command before the collector can commit.
    conn.query("SAVEPOINT pbps_role_history").await?;
    let protected = conn.query(&format!("SELECT 'pg_authid'::text AS kind, pg_catalog.jsonb_build_array(r.rolname::text)::text AS key, r.oid::text AS oid, r.xmin::text AS version, r.rolname::text AS label FROM pg_catalog.pg_authid r WHERE r.rolname IN ({role_names}) ORDER BY r.rolname")).await;
    let recovered = if protected.is_err() {
        conn.query("ROLLBACK TO SAVEPOINT pbps_role_history")
            .await
            .map(|_| ())
    } else {
        Ok(())
    };
    let released = if recovered.is_ok() {
        conn.query("RELEASE SAVEPOINT pbps_role_history")
            .await
            .map(|_| ())
    } else {
        Ok(())
    };
    let role_versions = match protected {
        Ok(found) => {
            recovered?;
            released?;
            if found.len() != roles.len() {
                return Err(unknown("a required role witness is missing"));
            }
            for row in found {
                insert(&mut rows, &bounds, &row)?;
            }
            true
        }
        Err(error) if error.server_error_code().as_deref() == Some("42501") => {
            recovered?;
            released?;
            false
        }
        Err(error) => return Err(error),
    };
    Ok(Witness {
        bounds,
        rows,
        role_versions,
        scope,
        principal: (
            authorization.principal.login.clone(),
            authorization.principal.effective.raw().to_owned(),
        ),
    })
}

// Expand only the original template. A quoted identifier can itself contain
// either marker and must never be rescanned as SQL syntax (DEC-695.1).
fn catalog_sql(schemas: &str, roles: &str) -> String {
    CATALOG
        .split("@SCHEMAS@")
        .map(|part| part.replace("@ROLES@", roles))
        .collect::<Vec<_>>()
        .join(schemas)
}

// Only rows that the compared facts use, including their joined inputs.
// The fixed section markers distinguish a readable empty inventory from an
// incomplete result. Existing semantic readers remain independent oracles.
const CATALOG: &str = r#"
WITH roles AS (
    SELECT oid FROM pg_catalog.pg_roles WHERE rolname IN (@ROLES@)
), library_members AS (
    SELECT d.*, p.prolang FROM pg_catalog.pg_depend d
    JOIN pg_catalog.pg_proc p ON p.oid=d.objid
    JOIN pg_catalog.pg_language l ON l.oid=p.prolang
    WHERE d.refclassid='pg_catalog.pg_extension'::regclass
      AND d.classid='pg_catalog.pg_proc'::regclass
      AND l.lanname='c' AND p.probin IS NOT NULL
), dependencies AS (
    SELECT d.classid,d.objid,d.objsubid,d.refclassid,d.refobjid,d.refobjsubid,d.deptype
    FROM pg_catalog.pg_depend d
    WHERE d.classid='pg_catalog.pg_extension'::regclass
      AND d.refclassid='pg_catalog.pg_extension'::regclass AND d.deptype='n'
    UNION
    SELECT classid,objid,objsubid,refclassid,refobjid,refobjsubid,deptype FROM library_members
), namespaces AS (
    SELECT oid FROM pg_catalog.pg_namespace WHERE nspname IN (@SCHEMAS@)
    UNION SELECT extnamespace FROM pg_catalog.pg_extension
    UNION SELECT c.collnamespace FROM pg_catalog.pg_collation c
          JOIN pg_catalog.pg_namespace n ON n.oid=c.collnamespace
          WHERE n.nspname NOT IN ('pg_catalog','pg_toast')
)
SELECT 'pg_database'::text AS kind, pg_catalog.jsonb_build_array(d.datname::text)::text AS key,
       d.oid::text AS oid,d.xmin::text AS version,d.datname::text AS label
FROM pg_catalog.pg_database d WHERE d.datname=pg_catalog.current_database()
UNION ALL
SELECT 'pg_namespace',pg_catalog.jsonb_build_array(n.nspname::text)::text,n.oid::text,n.xmin::text,n.nspname::text
FROM pg_catalog.pg_namespace n WHERE n.oid IN (SELECT oid FROM namespaces)
UNION ALL
SELECT 'pg_extension',pg_catalog.jsonb_build_array(e.extname::text)::text,e.oid::text,e.xmin::text,e.extname::text
FROM pg_catalog.pg_extension e
UNION ALL
SELECT 'pg_depend',pg_catalog.jsonb_build_array(d.classid::text,d.objid::text,d.objsubid::text,d.refclassid::text,d.refobjid::text,d.refobjsubid::text,d.deptype::text)::text,
       '0',d.xmin::text,e.extname::text||' dependency'
FROM pg_catalog.pg_depend d JOIN dependencies x USING(classid,objid,objsubid,refclassid,refobjid,refobjsubid,deptype)
JOIN pg_catalog.pg_extension e ON e.oid=d.refobjid
UNION ALL
SELECT 'pg_proc',pg_catalog.jsonb_build_array(n.nspname::text,p.proname::text,p.proargtypes::text)::text,p.oid::text,p.xmin::text,n.nspname::text||'.'||p.proname::text
FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace WHERE p.oid IN (SELECT objid FROM library_members)
UNION ALL
SELECT 'pg_language',pg_catalog.jsonb_build_array(l.lanname::text)::text,l.oid::text,l.xmin::text,l.lanname::text
FROM pg_catalog.pg_language l WHERE l.oid IN (SELECT prolang FROM library_members)
UNION ALL
SELECT 'pg_collation',pg_catalog.jsonb_build_array(n.nspname::text,c.collname::text,c.collencoding::text)::text,c.oid::text,c.xmin::text,n.nspname::text||'.'||c.collname::text
FROM pg_catalog.pg_collation c JOIN pg_catalog.pg_namespace n ON n.oid=c.collnamespace
WHERE n.nspname NOT IN ('pg_catalog','pg_toast')
UNION ALL
SELECT 'pg_db_role_setting',pg_catalog.jsonb_build_array(s.setdatabase::text,s.setrole::text)::text,'0',s.xmin::text,
       (CASE WHEN s.setdatabase=0 THEN 'all databases' ELSE pg_catalog.current_database()::text END)||' / '||
       (CASE WHEN s.setrole=0 THEN 'all roles' ELSE session_user::text END)
FROM pg_catalog.pg_db_role_setting s
WHERE (s.setrole=0 OR s.setrole=session_user::regrole)
  AND (s.setdatabase=0 OR s.setdatabase=(SELECT oid FROM pg_catalog.pg_database WHERE datname=pg_catalog.current_database()))
UNION ALL
SELECT 'pg_auth_members',pg_catalog.jsonb_build_array(m.roleid::text,m.member::text,m.grantor::text)::text,m.oid::text,m.xmin::text,r.rolname::text||' to '||u.rolname::text||' by '||g.rolname::text
FROM pg_catalog.pg_auth_members m
JOIN pg_catalog.pg_roles r ON r.oid=m.roleid
JOIN pg_catalog.pg_roles u ON u.oid=m.member
JOIN pg_catalog.pg_roles g ON g.oid=m.grantor WHERE m.member IN (SELECT oid FROM roles)
UNION ALL
SELECT 'complete',pg_catalog.unnest(ARRAY['pg_database','pg_namespace','pg_extension','pg_depend','pg_proc','pg_language','pg_collation','pg_db_role_setting','pg_auth_members']),NULL,NULL,NULL
"#;

#[cfg(test)]
mod tests;
