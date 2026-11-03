//! Modules outside the capture scope that an object the plan creates will
//! rebind (DEC-2143.7).
//!
//! The lookup rules live once, in `select::runtime_paths`. This read only finds
//! which unmanaged modules mention a created name (a name filter the engine
//! matches under the database collation), captures those modules with the
//! reader the managed modules use, and asks, for each of their runtime-bound
//! names, whether the first match on its lookup path moves once the objects the
//! plan creates exist.

use super::{CandidateSet, CaptureError, CaptureScope, ObjectIdentity, Uncovered, class};
use pbps_db::transport::QueryConnection;
use std::collections::BTreeSet;

/// One unmanaged module and the created object its name will resolve to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rebound {
    pub module: Vec<String>,
    pub created: Vec<String>,
}

/// The created objects whose names a module can mention: a table, view or
/// routine. A type is not one (an unresolved reference never names a type, and
/// a module naming a missing type cannot be created, measured on 17.0).
fn lookup(created: &ObjectIdentity) -> Option<(&str, &str)> {
    match (created.class.as_str(), created.name.as_slice()) {
        (class::OBJECT, [schema, name]) => Some((schema, name)),
        _ => None,
    }
}

fn literal(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

/// The arrival that becomes the first match on `path`, the ordered candidate
/// sets of one runtime-bound name, if one does. `target` and `scratch` hold
/// what each side found in them. Only an arrival counts: what scratch lacks of
/// the target's own (unmanaged objects it was never given) is no drop the plan
/// makes. The first set the target already fills keeps the binding, unless
/// scratch holds a second candidate there, which the path does not order.
pub(crate) fn arrival_on_path(
    path: &[CandidateSet],
    target: &super::CapturedInputs,
    scratch: &super::CapturedInputs,
) -> Result<Option<ObjectIdentity>, CaptureError> {
    for set in path {
        let held = target.members(set).ok_or(CaptureError::Incomplete)?;
        let there = scratch.members(set).ok_or(CaptureError::Incomplete)?;
        if !held.is_empty() {
            return Ok(there.difference(held).next().cloned());
        }
        if let Some(arrival) = there.iter().next() {
            return Ok(Some(arrival.clone()));
        }
    }
    Ok(None)
}

/// Reads the unmanaged modules the plan's created objects rebind. Without
/// database `VIEW DEFINITION` the catalog returns nothing for a module the
/// account cannot see, and nothing must never read as "no dependents": the
/// read refuses by condition instead.
pub async fn unmanaged_rebinds(
    target: &mut impl QueryConnection,
    scratch: &mut impl QueryConnection,
    created: &BTreeSet<ObjectIdentity>,
    own: &BTreeSet<ObjectIdentity>,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<Vec<Rebound>, CaptureError> {
    let pairs: Vec<String> = created
        .iter()
        .filter_map(lookup)
        .map(|(schema, name)| format!("({}, {})", literal(schema), literal(name)))
        .collect();
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let visible = target
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
    // Which modules mention a created name at all, the engine matching under
    // the database collation. How each one's names are looked up is not asked
    // here.
    let sql = format!(
        "SELECT DISTINCT CONVERT(nvarchar(128), SCHEMA_NAME(o.schema_id)) AS module_schema, \
                o.name AS module_name \
           FROM sys.sql_expression_dependencies d \
           JOIN sys.objects o ON o.object_id = d.referencing_id \
           JOIN (VALUES {}) c(made_schema, made_name) \
             ON d.referenced_entity_name = c.made_name \
          WHERE d.referenced_server_name IS NULL AND d.referenced_database_name IS NULL \
            AND d.referenced_class_desc = N'OBJECT_OR_COLUMN' \
            AND (d.referenced_schema_name IS NULL OR d.referenced_schema_name = c.made_schema) \
            AND o.type IN ('V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT', 'TR');",
        pairs.join(", ")
    );
    let mut referrers = BTreeSet::new();
    for row in target.query(&sql).await.map_err(|_| CaptureError::Read)? {
        let text = |field| -> Result<String, CaptureError> {
            row.try_get::<&str>(field)
                .ok()
                .flatten()
                .map(str::to_owned)
                .ok_or(CaptureError::Incomplete)
        };
        let module = super::object(&text("module_schema")?, &text("module_name")?);
        if !own.contains(&module) && !created.contains(&module) {
            referrers.insert(module);
        }
    }
    if referrers.is_empty() {
        return Ok(Vec::new());
    }
    // The same reader the managed modules use, then the candidate sets their
    // runtime-bound names need, read on both sides.
    let modules = super::capture(
        target,
        &CaptureScope {
            retained: referrers,
            candidates: BTreeSet::new(),
        },
        key,
    )
    .await?;
    let scope = modules.runtime_scope();
    let on_target = super::capture(target, &scope, key).await?;
    let on_scratch = super::capture(
        scratch,
        &CaptureScope {
            retained: BTreeSet::new(),
            candidates: scope.candidates.clone(),
        },
        key,
    )
    .await?;
    let mut found = BTreeSet::new();
    for (module, paths) in modules.lookup_paths() {
        for path in paths {
            if let Some(arrival) = arrival_on_path(&path, &on_target, &on_scratch)? {
                found.insert(Rebound {
                    module: module.name.clone(),
                    created: arrival.name,
                });
            }
        }
    }
    Ok(found.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_table_view_or_routine_name_is_looked_up() {
        let a_type = ObjectIdentity {
            class: class::TYPE.into(),
            name: vec!["dbo".into(), "ghost".into()],
            signature: Vec::new(),
        };
        let object = super::super::object("dbo", "ghost");
        assert_eq!(
            lookup(&a_type),
            None,
            "an unresolved reference is never a type"
        );
        assert_eq!(lookup(&object), Some(("dbo", "ghost")));
    }

    #[test]
    fn a_literal_doubles_its_quotes() {
        assert_eq!(literal("o'k"), "N'o''k'");
    }
}
