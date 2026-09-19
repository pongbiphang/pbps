//! Grant authority is per securable and privilege, including exact overloads.

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError, Param, doctor::Ask};
use pbps_model::{GrantTarget, ModuleId, ObjectName, Permission};

use super::{
    Gap, Securable, flag,
    identity::{Identity, Table},
    text,
};

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

pub(super) struct Read {
    pub gaps: Vec<Gap>,
    pub absent_schemas: BTreeSet<String>,
}

pub(super) async fn missing(
    conn: &mut Conn,
    ask: &Ask<'_>,
    identities: &Identity<'_>,
) -> Result<Read, DbError> {
    let granted = ask.granted;
    let mut demands = std::collections::BTreeMap::<_, BTreeSet<_>>::new();
    let mut recorded_targets = BTreeSet::new();
    for (target, permissions) in &granted.permissions {
        let target = if let GrantTarget::Object(name) = target {
            match identities.table(name) {
                Table::Recorded(current) => {
                    recorded_targets.insert(GrantTarget::Object(current.clone()));
                    GrantTarget::Object(current)
                }
                // The old occupant belongs to another identity. The new
                // table's creator retains grant options even after revoking
                // ordinary DML from themselves; never inspect that occupant.
                Table::Future => continue,
                Table::Unrecorded(_) => target.clone(),
            }
        } else {
            target.clone()
        };
        demands
            .entry(target)
            .or_default()
            .extend(permissions.iter().copied());
    }
    let mut managed_tables: BTreeSet<_> = granted
        .managed_tables
        .iter()
        .chain(ask.managed_tables)
        .filter_map(|name| identities.table(name).name().cloned())
        .collect();
    let mut managed_modules = granted.managed_modules.clone();
    let mut roles: BTreeSet<String> = granted.roles.iter().cloned().collect();
    // Removed declarations still produce REVOKE statements. As in the other
    // engine, an unreadable ledger is reported by the ledger diagnosis itself.
    if let Ok(Some(recorded)) = crate::state::latest(conn).await {
        managed_tables.extend(recorded.snapshot.ids.tables.into_values());
        managed_tables.extend(recorded.snapshot.schema.tables.into_keys());
        managed_modules.extend(recorded.snapshot.schema.modules.into_keys());
        for (name, role) in recorded.snapshot.schema.roles {
            roles.insert(name);
            for (target, permissions) in role.grants {
                demands.entry(target).or_default().extend(permissions);
            }
        }
    }
    let mut gaps = Vec::new();
    let mut absent_schemas = BTreeSet::new();
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
                    why: "MAINTAIN requires PostgreSQL 17 or later".to_owned(),
                    securable,
                });
                continue;
            }
            let rows = crate::catalog::canonical_query(conn, &query, &params).await?;
            if rows.is_empty() && recorded_targets.contains(target) {
                gaps.push(Gap {
                    permission,
                    why: "the recorded grant target is absent; its grant authority cannot be established".to_owned(),
                    securable: securable.clone(),
                });
            }
            for row in rows {
                if let GrantTarget::Schema(schema) = target
                    && !flag(&row, "present")?
                {
                    absent_schemas.insert(schema.clone());
                    continue;
                }
                add_gap(
                    &mut gaps,
                    &row,
                    permission,
                    securable.clone(),
                    WHY.to_owned(),
                )?;
            }
        }
    }
    // The catalog also holds adopted grants absent from both the declarations
    // and the ledger. Ask only about managed grantees, never every cluster role.
    if !roles.is_empty() {
        let values = super::values_list(roles.len(), 1);
        let query = catalog_question(&values);
        let params: Vec<_> = roles.iter().map(|r| Param::Str(r)).collect();
        for row in crate::catalog::canonical_query(conn, &query, &params).await? {
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
            if !manages(&securable, &managed_tables, &managed_modules) {
                continue;
            }
            let grantor = super::spelled(&text(&row, "grantor")?);
            add_gap(
                &mut gaps,
                &row,
                permission,
                securable,
                format!(
                    "revoking this ACL needs its original grantor {grantor}; select that role explicitly when inherited grant paths compete"
                ),
            )?;
        }
    }
    Ok(Read {
        gaps,
        absent_schemas,
    })
}

fn add_gap(
    gaps: &mut Vec<Gap>,
    row: &pbps_db::Row,
    permission: &'static str,
    securable: Securable,
    why: String,
) -> Result<(), DbError> {
    if !flag(row, "held")? {
        let gap = Gap {
            permission,
            why,
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
            why: "a role grant must be able to resolve its securable's schema".to_owned(),
            securable: Securable::Schema(schema),
        };
        if !gaps.contains(&gap) {
            gaps.push(gap);
        }
    }
    Ok(())
}

// The same namespace boundary as pbps_diff::scope: a managed overload never
// adopts a same-named relation, and schema ACLs remain declarable everywhere.
fn manages(
    securable: &Securable,
    tables: &BTreeSet<ObjectName>,
    modules: &BTreeSet<ModuleId>,
) -> bool {
    match securable {
        Securable::Schema(_) => true,
        Securable::Object(o) => {
            tables.contains(o)
                || modules.iter().any(|id| {
                    !matches!(id, ModuleId::Routine(_)) && id.referenced_name().as_ref() == Some(o)
                })
        }
        Securable::Routine(o, Some(signature)) => modules.iter().any(|id| {
            matches!(id, ModuleId::Routine(r) if &r.name == o &&
                r.args.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(", ") == *signature)
        }),
        Securable::Routine(_, None) => false,
    }
}

fn question(target: &GrantTarget, right: Permission) -> (Securable, String, Vec<Param<'_>>) {
    match target {
        GrantTarget::Schema(schema) => (
            Securable::Schema(schema.clone()),
            "SELECT n.oid IS NOT NULL AS present,
                    pg_catalog.has_schema_privilege(n.oid, $2) AS held, true AS usage_ok
               FROM (SELECT $1::text AS name) wanted
               LEFT JOIN pg_catalog.pg_namespace n ON n.nspname = wanted.name"
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
    // The question itself is `crate::catalog::revocable_by_current_role`,
    // shared with the pull so that the diagnosis and the read that refuses a
    // plan cannot answer it differently (#251).
    let held = crate::catalog::revocable_by_current_role("t.owner", "t.acl");
    format!(
        "WITH managed AS (
            SELECT r.oid FROM pg_catalog.pg_roles r
            JOIN (VALUES {values}) AS wanted(name) ON r.rolname = wanted.name
        ), targets AS (
            SELECT n.nspname AS schema_name, c.relname AS object_name,
                   'table' AS kind, '' AS signature, c.relowner AS owner, c.relacl AS acl,
                   pg_catalog.has_schema_privilege(n.oid, 'USAGE') AS usage_ok
              FROM pg_catalog.pg_class c
              JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r', 'v')
            UNION ALL
            SELECT n.nspname, p.proname, 'routine',
                   COALESCE((SELECT pg_catalog.string_agg(pg_catalog.format_type(u.ty, NULL), ', ' ORDER BY u.pos)
                       FROM pg_catalog.unnest(p.proargtypes) WITH ORDINALITY u(ty, pos)), ''),
                   p.proowner, p.proacl, pg_catalog.has_schema_privilege(n.oid, 'USAGE')
              FROM pg_catalog.pg_proc p
              JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
             WHERE p.prokind IN ('f', 'p')
            UNION ALL
            SELECT n.nspname, '', 'schema', '', n.nspowner, n.nspacl, true
              FROM pg_catalog.pg_namespace n
        )
        SELECT t.schema_name, t.object_name, t.kind, t.signature,
               a.privilege_type AS privilege, pg_catalog.pg_get_userbyid(a.grantor) AS grantor,
               {held} AS held,
               t.usage_ok
          FROM targets t
          CROSS JOIN LATERAL pg_catalog.aclexplode(t.acl) a
          JOIN managed m ON m.oid = a.grantee
          CROSS JOIN pg_catalog.pg_roles me
         WHERE me.rolname = current_user AND a.grantee <> t.owner
         ORDER BY 1, 2, 3, 4, 5, 6"
    )
}
