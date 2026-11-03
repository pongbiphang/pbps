//! Modules outside the capture scope that an object the plan creates will
//! rebind (DEC-2143.7).
//!
//! A managed module's candidates are in the capture, so an arrival shows as a
//! difference between them. An unmanaged module is not captured, so the only
//! way to name it is a reverse read of the dependency rows, matched against the
//! names the plan creates by the engine itself (see [`rebind_sql`]).

use super::{CaptureError, ObjectIdentity, Uncovered, class};
use pbps_db::transport::QueryConnection;
use std::collections::BTreeSet;

/// One unmanaged module and the created object its name will resolve to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebound {
    pub module: Vec<String>,
    pub created: Vec<String>,
}

/// One unresolved reference the engine matched to a created name: the
/// referencing module's schema and name, then the created object's.
pub(crate) type Match = (String, String, String, String);

fn lookup(created: &ObjectIdentity) -> Option<(&str, &str)> {
    match (created.class.as_str(), created.name.as_slice()) {
        (class::OBJECT, [schema, name]) => Some((schema, name)),
        _ => None,
    }
}

fn literal(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

/// The matches that name an unmanaged module: not one the plan manages and
/// not one it creates.
pub(crate) fn unmanaged(
    matches: &[Match],
    created: &BTreeSet<ObjectIdentity>,
    own: &BTreeSet<ObjectIdentity>,
) -> Vec<Rebound> {
    let mut found = BTreeSet::new();
    for (module_schema, module, made_schema, made) in matches {
        let id = ObjectIdentity {
            class: class::OBJECT.into(),
            name: vec![module_schema.clone(), module.clone()],
            signature: Vec::new(),
        };
        if own.contains(&id) || created.contains(&id) {
            continue;
        }
        found.insert(Rebound {
            module: id.name,
            created: vec![made_schema.clone(), made.clone()],
        });
    }
    found.into_iter().collect()
}

impl Ord for Rebound {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.module, &self.created).cmp(&(&other.module, &other.created))
    }
}

impl PartialOrd for Rebound {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The read, with the matching done by the engine so that it follows the
/// database's own collation (accents, width and case sensitivity). A created
/// (schema, name) pair rebinds an unmanaged module's reference when it becomes
/// the first match on the module's lookup path (DEC-619.2):
///
/// - a name spelled with a schema reaches exactly that schema, and only an
///   unresolved one can still be taken;
/// - a one-part name reaches the module's own schema, then `dbo`: an
///   unresolved one is taken by either, a resolved one by the module's own
///   schema when it now resolves to the `dbo` fallback (a stale resolution
///   stays by id until the module is altered);
/// - a one-part call (`is_caller_dependent`) reaches the executing principal's
///   default schema, then `dbo`: the module's fixed principal, or every
///   principal's when it runs as its caller, as `select::runtime_candidates`
///   models it. Its row never says what it resolves to, so the pair is taken
///   whenever its schema is on the path.
///
/// A schema-bound reference is bound by id and cannot move.
fn rebind_sql(pairs: &str) -> String {
    format!(
        "SELECT DISTINCT CONVERT(nvarchar(128), SCHEMA_NAME(o.schema_id)) AS module_schema, \
                o.name AS module_name, c.made_schema AS made_schema, c.made_name AS made_name \
           FROM sys.sql_expression_dependencies d \
           JOIN sys.objects o ON o.object_id = d.referencing_id \
           JOIN sys.sql_modules m ON m.object_id = o.object_id \
           JOIN (VALUES {pairs}) c(made_schema, made_name) \
             ON d.referenced_entity_name = c.made_name \
           LEFT JOIN sys.objects r ON r.object_id = d.referenced_id \
          WHERE d.referenced_server_name IS NULL AND d.referenced_database_name IS NULL \
            AND d.referenced_class_desc = N'OBJECT_OR_COLUMN' \
            AND d.is_schema_bound_reference = 0 \
            AND o.type IN ('V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT', 'TR') \
            AND ( \
                 (d.referenced_schema_name IS NOT NULL AND d.referenced_id IS NULL \
                  AND d.referenced_schema_name = c.made_schema) \
              OR (d.referenced_schema_name IS NULL AND d.is_caller_dependent = 0 \
                  AND ((d.referenced_id IS NULL \
                        AND (SCHEMA_NAME(o.schema_id) = c.made_schema OR c.made_schema = N'dbo')) \
                    OR (d.referenced_id IS NOT NULL \
                        AND SCHEMA_NAME(r.schema_id) = N'dbo' \
                        AND SCHEMA_NAME(o.schema_id) <> N'dbo' \
                        AND SCHEMA_NAME(o.schema_id) = c.made_schema))) \
              OR (d.referenced_schema_name IS NULL AND d.is_caller_dependent = 1 \
                  AND (c.made_schema = N'dbo' \
                    OR c.made_schema IN (SELECT p.default_schema_name \
                         FROM sys.database_principals p \
                        WHERE m.execute_as_principal_id IS NULL \
                           OR p.principal_id = CASE WHEN m.execute_as_principal_id = -2 \
                                THEN COALESCE(o.principal_id, \
                                     (SELECT sc.principal_id FROM sys.schemas sc \
                                       WHERE sc.schema_id = o.schema_id)) \
                                ELSE m.execute_as_principal_id END))));"
    )
}

/// Reads the target's unresolved references to the names the plan creates.
/// Without database `VIEW DEFINITION` the view returns nothing for a module
/// the account cannot see, and nothing must never read as "no dependents": the
/// read refuses by condition instead.
pub async fn unmanaged_rebinds(
    conn: &mut impl QueryConnection,
    created: &BTreeSet<ObjectIdentity>,
    own: &BTreeSet<ObjectIdentity>,
) -> Result<Vec<Rebound>, CaptureError> {
    let names: BTreeSet<&str> = created.iter().filter_map(lookup).map(|(_, n)| n).collect();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let visible = conn
        .query(
            "SELECT CONVERT(nvarchar(1), HAS_PERMS_BY_NAME(DB_NAME(), N'DATABASE', \
             N'VIEW DEFINITION')) AS visible;",
        )
        .await
        .map_err(|_| CaptureError::Read)?;
    let allowed = visible
        .first()
        .and_then(|row| row.try_get::<&str>("visible").ok().flatten())
        == Some("1");
    if !allowed {
        return Err(Uncovered::class(
            "sys.sql_expression_dependencies",
            "the references to a created name are hidden without database VIEW \
             DEFINITION, so which unmanaged modules it rebinds cannot be named",
        )
        .into());
    }
    let pairs: Vec<String> = created
        .iter()
        .filter_map(lookup)
        .map(|(schema, name)| format!("({}, {})", literal(schema), literal(name)))
        .collect();
    let sql = rebind_sql(&pairs.join(", "));
    let mut matches = Vec::new();
    for row in conn.query(&sql).await.map_err(|_| CaptureError::Read)? {
        let text = |field| -> Result<String, CaptureError> {
            row.try_get::<&str>(field)
                .ok()
                .flatten()
                .map(str::to_owned)
                .ok_or(CaptureError::Incomplete)
        };
        matches.push((
            text("module_schema")?,
            text("module_name")?,
            text("made_schema")?,
            text("made_name")?,
        ));
    }
    Ok(unmanaged(&matches, created, own))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(schema: &str, name: &str) -> ObjectIdentity {
        ObjectIdentity {
            class: class::OBJECT.into(),
            name: vec![schema.into(), name.into()],
            signature: Vec::new(),
        }
    }

    fn matched(module: (&str, &str), made: (&str, &str)) -> Match {
        (
            module.0.into(),
            module.1.into(),
            made.0.into(),
            made.1.into(),
        )
    }

    #[test]
    fn a_managed_module_and_an_object_the_plan_creates_are_not_unmanaged() {
        let created = BTreeSet::from([object("dbo", "ghost"), object("dbo", "made")]);
        let own = BTreeSet::from([object("dbo", "managed")]);
        let rows = [
            matched(("dbo", "managed"), ("dbo", "ghost")),
            matched(("dbo", "made"), ("dbo", "ghost")),
            matched(("dbo", "outside"), ("dbo", "ghost")),
        ];
        let found = unmanaged(&rows, &created, &own);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].module[1], "outside");
        assert_eq!(found[0].created, vec!["dbo".to_owned(), "ghost".to_owned()]);
    }

    #[test]
    fn only_a_table_view_or_routine_name_is_looked_up() {
        let a_type = ObjectIdentity {
            class: class::TYPE.into(),
            name: vec!["dbo".into(), "ghost".into()],
            signature: Vec::new(),
        };
        assert_eq!(
            lookup(&a_type),
            None,
            "an unresolved reference is never a type"
        );
        assert_eq!(lookup(&object("dbo", "ghost")), Some(("dbo", "ghost")));
    }

    #[test]
    fn a_literal_doubles_its_quotes() {
        assert_eq!(literal("o'k"), "N'o''k'");
    }
}
