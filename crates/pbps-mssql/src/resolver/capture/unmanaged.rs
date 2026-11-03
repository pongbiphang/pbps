//! Modules outside the capture scope that an object the plan creates will
//! rebind (DEC-2143.7).
//!
//! A managed module's candidates are in the capture, so an arrival shows as a
//! difference between them. An unmanaged module is not captured, so the only
//! way to name it is a reverse read: the dependency rows whose referenced name
//! is still unresolved (`referenced_id` is `NULL` while no object holds the
//! name) and mention the name the plan creates.

use super::{CaptureError, ObjectIdentity, Uncovered, class};
use pbps_db::transport::QueryConnection;
use std::collections::BTreeSet;

/// One unmanaged module and the created object its name will resolve to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebound {
    pub module: Vec<String>,
    pub created: Vec<String>,
}

/// One unresolved reference as the catalog lists it: the referencing module's
/// schema and name, then the schema the reference spelled (empty when it
/// spelled none) and the name.
pub(crate) type Reference = (String, String, String, String);

/// The created objects an unresolved reference can bind. A type is not one:
/// the catalog lists the unresolved references of `OBJECT_OR_COLUMN` class
/// only, and a module naming a type that does not exist cannot be created at
/// all (measured on 17.0), so creating a type can never satisfy one.
fn lookup(created: &ObjectIdentity) -> Option<(&str, &str)> {
    match (created.class.as_str(), created.name.as_slice()) {
        (class::OBJECT, [schema, name]) => Some((schema, name)),
        _ => None,
    }
}

fn literal(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

/// The modules among `references` that a created object takes a name from:
/// a one-part reference reaches the module's own schema first and `dbo`
/// second; a two-part reference reaches exactly its schema. Names compare
/// as the default collation does, without case.
pub(crate) fn rebound(
    references: &[Reference],
    created: &BTreeSet<ObjectIdentity>,
    own: &BTreeSet<ObjectIdentity>,
) -> Vec<Rebound> {
    let mut found = BTreeSet::new();
    for (module_schema, module, spelled, name) in references {
        let id = ObjectIdentity {
            class: class::OBJECT.into(),
            name: vec![module_schema.clone(), module.clone()],
            signature: Vec::new(),
        };
        if own.contains(&id) || created.contains(&id) {
            continue;
        }
        for object in created {
            let Some((schema, made)) = lookup(object) else {
                continue;
            };
            let reaches = if spelled.is_empty() {
                module_schema.eq_ignore_ascii_case(schema) || schema.eq_ignore_ascii_case("dbo")
            } else {
                spelled.eq_ignore_ascii_case(schema)
            };
            if reaches && name.eq_ignore_ascii_case(made) {
                found.insert(Rebound {
                    module: id.name.clone(),
                    created: object.name.clone(),
                });
            }
        }
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
            "the unresolved references to a created name are hidden without database VIEW \
             DEFINITION, so which unmanaged modules it rebinds cannot be named",
        )
        .into());
    }
    let list: Vec<String> = names.iter().map(|n| literal(n)).collect();
    let sql = format!(
        "SELECT DISTINCT CONVERT(nvarchar(128), SCHEMA_NAME(o.schema_id)) AS module_schema, \
                o.name AS module_name, \
                ISNULL(d.referenced_schema_name, N'') AS spelled, \
                d.referenced_entity_name AS name \
           FROM sys.sql_expression_dependencies d \
           JOIN sys.objects o ON o.object_id = d.referencing_id \
          WHERE d.referenced_id IS NULL \
            AND d.referenced_server_name IS NULL AND d.referenced_database_name IS NULL \
            AND o.type IN ('V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT', 'TR') \
            AND d.referenced_entity_name IN ({});",
        list.join(", ")
    );
    let mut references = Vec::new();
    for row in conn.query(&sql).await.map_err(|_| CaptureError::Read)? {
        let text = |field| -> Result<String, CaptureError> {
            row.try_get::<&str>(field)
                .ok()
                .flatten()
                .map(str::to_owned)
                .ok_or(CaptureError::Incomplete)
        };
        references.push((
            text("module_schema")?,
            text("module_name")?,
            text("spelled")?,
            text("name")?,
        ));
    }
    Ok(rebound(&references, created, own))
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

    fn reference(module: (&str, &str), spelled: &str, name: &str) -> Reference {
        (
            module.0.into(),
            module.1.into(),
            spelled.into(),
            name.into(),
        )
    }

    #[test]
    fn a_one_part_reference_reaches_its_own_schema_then_dbo() {
        let created = BTreeSet::from([object("dbo", "ghost")]);
        let none = BTreeSet::new();
        let from_app = [reference(("app", "reader"), "", "ghost")];
        let found = rebound(&from_app, &created, &none);
        assert_eq!(found.len(), 1, "dbo is the fallback of every schema");
        assert_eq!(found[0].module, vec!["app".to_owned(), "reader".to_owned()]);
        let created = BTreeSet::from([object("other", "ghost")]);
        assert!(
            rebound(&from_app, &created, &none).is_empty(),
            "a schema off the module's path is not reached"
        );
    }

    #[test]
    fn a_two_part_reference_reaches_exactly_its_schema() {
        let none = BTreeSet::new();
        let spelled = [reference(("app", "reader"), "other", "ghost")];
        assert_eq!(
            rebound(&spelled, &BTreeSet::from([object("other", "ghost")]), &none).len(),
            1
        );
        assert!(rebound(&spelled, &BTreeSet::from([object("dbo", "ghost")]), &none).is_empty());
    }

    #[test]
    fn a_managed_module_and_an_object_the_plan_creates_are_not_unmanaged() {
        let created = BTreeSet::from([object("dbo", "ghost"), object("dbo", "made")]);
        let own = BTreeSet::from([object("dbo", "managed")]);
        let rows = [
            reference(("dbo", "managed"), "", "ghost"),
            reference(("dbo", "made"), "", "ghost"),
            reference(("dbo", "outside"), "", "ghost"),
        ];
        let found = rebound(&rows, &created, &own);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].module[1], "outside");
    }

    #[test]
    fn a_name_is_compared_without_case() {
        let created = BTreeSet::from([object("dbo", "Ghost")]);
        let rows = [reference(("dbo", "outside"), "", "GHOST")];
        assert_eq!(rebound(&rows, &created, &BTreeSet::new()).len(), 1);
    }

    #[test]
    fn a_created_type_is_not_what_an_unresolved_object_reference_binds() {
        let created = BTreeSet::from([ObjectIdentity {
            class: class::TYPE.into(),
            name: vec!["dbo".into(), "ghost".into()],
            signature: Vec::new(),
        }]);
        let rows = [reference(("dbo", "outside"), "", "ghost")];
        assert!(rebound(&rows, &created, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn a_literal_doubles_its_quotes() {
        assert_eq!(literal("o'k"), "N'o''k'");
    }
}
