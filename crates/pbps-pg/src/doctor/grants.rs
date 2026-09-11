//! Grant authority is per securable and privilege, including exact overloads.

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError, Param, doctor::GrantTargets};
use pbps_model::{GrantTarget, ObjectName, Permission};

use super::{Gap, Securable, flag, text};

const WHY: &str =
    "changing a managed role's grant needs ownership or this privilege with grant option";

fn permission(word: &str) -> Option<&'static str> {
    Some(match word {
        "SELECT" => "SELECT WITH GRANT OPTION",
        "INSERT" => "INSERT WITH GRANT OPTION",
        "UPDATE" => "UPDATE WITH GRANT OPTION",
        "DELETE" => "DELETE WITH GRANT OPTION",
        "REFERENCES" => "REFERENCES WITH GRANT OPTION",
        "TRUNCATE" => "TRUNCATE WITH GRANT OPTION",
        "TRIGGER" => "TRIGGER WITH GRANT OPTION",
        "MAINTAIN" => "MAINTAIN WITH GRANT OPTION",
        "EXECUTE" => "EXECUTE WITH GRANT OPTION",
        "USAGE" => "USAGE WITH GRANT OPTION",
        "CREATE" => "CREATE WITH GRANT OPTION",
        _ => return None,
    })
}

pub(super) async fn missing(conn: &mut Conn, granted: &GrantTargets) -> Result<Vec<Gap>, DbError> {
    let mut demands = granted.permissions.clone();
    let mut roles: BTreeSet<String> = granted.roles.iter().cloned().collect();
    // Removed declarations still produce REVOKE statements. As in the other
    // engine, an unreadable ledger is reported by the ledger diagnosis itself.
    if let Ok(Some(recorded)) = crate::state::latest(conn).await {
        for (name, role) in recorded.snapshot.schema.roles {
            roles.insert(name);
            for (target, permissions) in role.grants {
                demands.entry(target).or_default().extend(permissions);
            }
        }
    }
    let mut gaps = Vec::new();
    for (target, permissions) in &demands {
        for right in permissions {
            let Ok(word) = crate::emit::permission_sql(*right) else {
                // Offline validation names privileges belonging to the other
                // dialect; never send an unknown privilege to PostgreSQL.
                continue;
            };
            let Some(permission) = permission(word) else {
                continue;
            };
            let (securable, query, mut params) = question(target, *right);
            params.push(Param::Str(permission));
            if *right == Permission::Maintain
                && crate::roles::server_version_num(conn).await? < crate::roles::MAINTAIN_ARRIVED_IN
            {
                gaps.push(Gap {
                    permission,
                    why: "MAINTAIN requires PostgreSQL 17 or later",
                    securable,
                });
                continue;
            }
            for row in conn.query_with(&query, &params).await? {
                add_gap(&mut gaps, &row, permission, securable.clone())?;
            }
        }
    }
    // The catalog also holds adopted grants absent from both the declarations
    // and the ledger. Ask only about managed grantees, never every cluster role.
    if !roles.is_empty() {
        let values = super::values_list(roles.len(), 1);
        let query = catalog_question(&values);
        let params: Vec<_> = roles.iter().map(|r| Param::Str(r)).collect();
        for row in conn.query_with(&query, &params).await? {
            let Some(permission) = permission(&text(&row, "privilege")?) else {
                continue;
            };
            let schema = text(&row, "schema_name")?;
            let securable = match text(&row, "kind")?.as_str() {
                "schema" => Securable::Schema(schema),
                "routine" => Securable::Routine(
                    ObjectName::new(schema, text(&row, "object_name")?),
                    Some(text(&row, "signature")?),
                ),
                _ => Securable::Object(ObjectName::new(schema, text(&row, "object_name")?)),
            };
            add_gap(&mut gaps, &row, permission, securable)?;
        }
    }
    Ok(gaps)
}

fn add_gap(
    gaps: &mut Vec<Gap>,
    row: &pbps_db::Row,
    permission: &'static str,
    securable: Securable,
) -> Result<(), DbError> {
    if !flag(row, "held")? {
        let gap = Gap {
            permission,
            why: WHY,
            securable: securable.clone(),
        };
        if !gaps.contains(&gap) {
            gaps.push(gap);
        }
    }
    if !flag(row, "usage_ok")? {
        let schema = match securable {
            Securable::Object(o) | Securable::Routine(o, _) => o.schema,
            Securable::Schema(_) => return Ok(()),
        };
        let gap = Gap {
            permission: "USAGE",
            why: "a role grant must be able to resolve its securable's schema",
            securable: Securable::Schema(schema),
        };
        if !gaps.contains(&gap) {
            gaps.push(gap);
        }
    }
    Ok(())
}

fn question(target: &GrantTarget, right: Permission) -> (Securable, String, Vec<Param<'_>>) {
    match target {
        GrantTarget::Schema(schema) => (
            Securable::Schema(schema.clone()),
            "SELECT pg_catalog.has_schema_privilege(n.oid, $2) AS held, true AS usage_ok
               FROM pg_catalog.pg_namespace n WHERE n.nspname = $1"
                .to_owned(),
            vec![Param::Str(schema)],
        ),
        GrantTarget::Routine(r) => {
            let signature = r
                .args
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let mut query = routine_question();
            // Parameter-owned strings cannot borrow a local rendering, so the
            // signature's components are individually bound as text values.
            let predicates = r
                .args
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    format!(
                        "pg_catalog.format_type(p.proargtypes[{i}], NULL) = ${}::text",
                        i + 3
                    )
                })
                .collect::<Vec<_>>();
            query.push_str(&format!(" AND p.pronargs = {}", r.args.len()));
            if !predicates.is_empty() {
                query.push_str(&format!(" AND {}", predicates.join(" AND ")));
            }
            query = query.replace("$PRIVILEGE", &format!("${}", r.args.len() + 3));
            let mut params = vec![Param::Str(&r.name.schema), Param::Str(&r.name.name)];
            params.extend(r.args.iter().map(|a| Param::Str(a.as_str())));
            (
                Securable::Routine(r.name.clone(), Some(signature)),
                query,
                params,
            )
        }
        GrantTarget::Object(o) if right == Permission::Execute => (
            Securable::Routine(o.clone(), None),
            routine_question().replace("$PRIVILEGE", "$3"),
            vec![Param::Str(&o.schema), Param::Str(&o.name)],
        ),
        GrantTarget::Object(o) => (
            Securable::Object(o.clone()),
            "SELECT pg_catalog.has_table_privilege(c.oid, $3) AS held,
                    pg_catalog.has_schema_privilege(n.oid, 'USAGE') AS usage_ok
               FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r', 'v')"
                .to_owned(),
            vec![Param::Str(&o.schema), Param::Str(&o.name)],
        ),
    }
}

fn routine_question() -> String {
    "SELECT pg_catalog.has_function_privilege(p.oid, $PRIVILEGE) AS held,
            pg_catalog.has_schema_privilege(n.oid, 'USAGE') AS usage_ok
       FROM pg_catalog.pg_proc p
       JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
      WHERE n.nspname = $1 AND p.proname = $2 AND p.prokind IN ('f', 'p')"
        .to_owned()
}

fn catalog_question(values: &str) -> String {
    // Match introspection's zero point: built-in defaults and an owner's
    // own ACL entries are not grants the declarative role set manages.
    format!(
        "WITH managed AS (
            SELECT r.oid FROM pg_catalog.pg_roles r
            JOIN (VALUES {values}) AS wanted(name) ON r.rolname = wanted.name
        )
        SELECT n.nspname AS schema_name, c.relname AS object_name,
               'table' AS kind, '' AS signature, a.privilege_type AS privilege,
               pg_catalog.has_table_privilege(c.oid, a.privilege_type || ' WITH GRANT OPTION') AS held,
               pg_catalog.has_schema_privilege(n.oid, 'USAGE') AS usage_ok
          FROM pg_catalog.pg_class c
          JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) a
          JOIN managed m ON m.oid = a.grantee
         WHERE c.relkind IN ('r', 'v') AND a.grantee <> c.relowner
        UNION ALL
        SELECT n.nspname, p.proname, 'routine',
               COALESCE((SELECT pg_catalog.string_agg(pg_catalog.format_type(u.ty, NULL), ', ' ORDER BY u.pos)
                   FROM pg_catalog.unnest(p.proargtypes) WITH ORDINALITY u(ty, pos)), ''),
               a.privilege_type,
               pg_catalog.has_function_privilege(p.oid, a.privilege_type || ' WITH GRANT OPTION'),
               pg_catalog.has_schema_privilege(n.oid, 'USAGE')
          FROM pg_catalog.pg_proc p
          JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
          CROSS JOIN LATERAL pg_catalog.aclexplode(p.proacl) a
          JOIN managed m ON m.oid = a.grantee
         WHERE p.prokind IN ('f', 'p') AND a.grantee <> p.proowner
        UNION ALL
        SELECT n.nspname, '', 'schema', '', a.privilege_type,
               pg_catalog.has_schema_privilege(n.oid, a.privilege_type || ' WITH GRANT OPTION'), true
          FROM pg_catalog.pg_namespace n
          CROSS JOIN LATERAL pg_catalog.aclexplode(n.nspacl) a
          JOIN managed m ON m.oid = a.grantee
         WHERE a.grantee <> n.nspowner
        ORDER BY 1, 2, 3, 4, 5"
    )
}
