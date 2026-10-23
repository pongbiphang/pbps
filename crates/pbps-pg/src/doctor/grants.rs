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
    // A new identity reusing a departing one's name. Kept apart from
    // `demands`: that name is the departing identity's too, and its recorded
    // grants must still be asked about under it.
    let mut future = std::collections::BTreeMap::<_, BTreeSet<_>>::new();
    for (target, permissions) in &granted.permissions {
        let sequence = if let GrantTarget::Sequence(c) = target {
            Some(c)
        } else {
            None
        };
        let table = match target {
            GrantTarget::Object(name) => Some(name),
            GrantTarget::Sequence(c) => Some(&c.table),
            GrantTarget::Schema(_) | GrantTarget::Routine(_) => None,
        };
        let target = if let Some(name) = table {
            match identities.table(name) {
                Table::Recorded(current) => {
                    let target = if let Some(c) = sequence {
                        let Some(column) = identities.column(name, &current, &c.name) else {
                            future
                                .entry(target.clone())
                                .or_default()
                                .extend(permissions.iter().copied());
                            continue;
                        };
                        GrantTarget::Sequence(current.column(column))
                    } else {
                        GrantTarget::Object(current)
                    };
                    recorded_targets.insert(target.clone());
                    target
                }
                // The old occupant belongs to another identity. The new
                // table's creator retains grant options even after revoking
                // ordinary DML from themselves; never inspect that occupant.
                Table::Future => {
                    future
                        .entry(target.clone())
                        .or_default()
                        .extend(permissions.iter().copied());
                    continue;
                }
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
    let mut roles: BTreeSet<pbps_model::RoleName> = granted.roles.iter().cloned().collect();
    // Removed declarations still produce REVOKE statements. As in the other
    // engine, an unreadable ledger is reported by the ledger diagnosis itself.
    if let Ok(Some(recorded)) = crate::state::latest(conn).await {
        managed_tables.extend(recorded.snapshot.ids.tables.into_values());
        managed_tables.extend(recorded.snapshot.schema.tables.into_keys());
        managed_modules.extend(recorded.snapshot.schema.modules.into_keys());
        for (name, role) in recorded.snapshot.schema.roles {
            roles.insert(name);
            for (target, permissions) in role.grants {
                // Recorded by its physical name, which a removed grant's
                // `REVOKE` names too: absent, its authority cannot be
                // established, exactly as for a declared recorded target
                // (#568). An exact routine overload the same: `REVOKE ... ON
                // FUNCTION` fails on a missing one just as on a table.
                if matches!(
                    target,
                    GrantTarget::Object(_) | GrantTarget::Routine(_) | GrantTarget::Sequence(_)
                ) {
                    recorded_targets.insert(target.clone());
                }
                demands.entry(target).or_default().extend(permissions);
            }
        }
    }
    let mut gaps = Vec::new();
    let mut absent_schemas = BTreeSet::new();
    // What a future target needs that does not depend on its occupant: the
    // server has to know the privilege at all (#569). Its catalog is not asked,
    // since that would answer about the departing identity's object.
    for (target, permissions) in &future {
        if permissions.contains(&Permission::Maintain)
            && crate::roles::server_version_num(conn).await? < crate::roles::MAINTAIN_ARRIVED_IN
        {
            let (securable, _, _) = question(target, Permission::Maintain);
            let Some(permission) = crate::emit::permission_sql(Permission::Maintain)
                .ok()
                .and_then(permission)
            else {
                continue;
            };
            gaps.push(Gap {
                permission,
                why: "MAINTAIN requires PostgreSQL 17 or later".to_owned(),
                securable,
            });
        }
    }
    for (target, permissions) in &demands {
        for right in permissions {
            let word = if *right == Permission::SerialSequences {
                Ok("USAGE")
            } else {
                crate::emit::permission_sql(*right)
            };
            let Ok(word) = word else {
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
        let params: Vec<_> = roles.iter().map(|r| Param::Str(r.raw())).collect();
        for row in crate::catalog::canonical_query(conn, &query, &params).await? {
            let Some(permission) = permission(&text(&row, "privilege")?) else {
                continue;
            };
            let Some(securable) = catalog_securable(
                &text(&row, "kind")?,
                ObjectName::new(text(&row, "schema_name")?, text(&row, "object_name")?),
                text(&row, "signature")?,
                text(&row, "column_name")?,
            ) else {
                continue;
            };
            if !manages(&securable, &managed_tables, &managed_modules) {
                continue;
            }
            let grantor = pbps_model::RoleName::from(text(&row, "grantor")?);
            let quoted_grantor = super::spelled(grantor.raw());
            add_gap(
                &mut gaps,
                &row,
                permission,
                securable,
                format!(
                    "revoking this ACL needs its original grantor {grantor}; select that role explicitly when inherited grant paths compete",
                    grantor = pbps_model::escape::Escaped(&quoted_grantor)
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
            Securable::Sequence(c) => c.table.schema,
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

fn catalog_securable(
    kind: &str,
    object: ObjectName,
    signature: String,
    column: String,
) -> Option<Securable> {
    Some(match kind {
        "schema" => Securable::Schema(object.schema),
        "routine" => Securable::Routine(object, Some(signature)),
        // Like the planner, address an owned sequence by its table column,
        // never by a physical name that can be changed independently.
        "sequence" if !column.is_empty() => Securable::Sequence(object.column(column)),
        "sequence" => return None,
        _ => Securable::Object(object),
    })
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
        Securable::Sequence(c) => tables.contains(&c.table),
        Securable::Object(o) => {
            tables.contains(o)
                || modules.iter().any(|id| {
                    !matches!(id, ModuleId::Routine(_)) && id.referenced_name().as_ref() == Some(o)
                })
        }
        Securable::Routine(o, Some(signature)) => modules.iter().any(|id| {
            matches!(id, ModuleId::Routine(r) if &r.name == o &&
                r.args.iter().map(|a| a.raw()).collect::<Vec<_>>().join(", ") == *signature)
        }),
        Securable::Routine(_, None) => false,
    }
}

fn question(target: &GrantTarget, right: Permission) -> (Securable, String, Vec<Param<'_>>) {
    match target {
        GrantTarget::Sequence(c) => (
            Securable::Sequence(c.clone()),
            "SELECT COALESCE(pg_catalog.has_sequence_privilege(
                        pg_catalog.pg_get_serial_sequence(pg_catalog.format('%I.%I', n.nspname, t.relname), $3), $4), true) AS held,
                    pg_catalog.has_schema_privilege(n.oid, 'USAGE') AS usage_ok
               FROM pg_catalog.pg_class t
               JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace
              WHERE n.nspname = $1 AND t.relname = $2 AND t.relkind IN ('r', 'p')".to_owned(),
            vec![Param::Str(&c.table.schema), Param::Str(&c.table.name), Param::Str(c.name.raw())],
        ),
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
                .map(|a| a.raw())
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
            params.extend(r.args.iter().map(|a| Param::Str(a.raw())));
            (
                Securable::Routine(r.name.clone(), Some(signature)),
                query,
                params,
            )
        }
        GrantTarget::Object(o) if right == Permission::SerialSequences => (
            Securable::Object(o.clone()),
            // A declared serial may not exist yet. Keep the table row even
            // when it owns no sequences, so absence is not a missing target.
            "SELECT COALESCE(pg_catalog.bool_and(
                        pg_catalog.has_sequence_privilege(s.oid, $3)), true) AS held,
                    pg_catalog.has_schema_privilege(n.oid, 'USAGE') AS usage_ok
               FROM pg_catalog.pg_class t
               JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace
               LEFT JOIN pg_catalog.pg_depend d
                 ON d.refobjid = COALESCE(pg_catalog.pg_partition_root(t.oid)::pg_catalog.oid, t.oid) AND d.refobjsubid > 0 AND d.deptype = 'a'
                AND d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass
                AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass
               LEFT JOIN pg_catalog.pg_class s ON s.oid = d.objid AND s.relkind = 'S'
              WHERE n.nspname = $1 AND t.relname = $2 AND t.relkind IN ('r', 'p')
              GROUP BY t.oid, n.oid"
                .to_owned(),
            vec![Param::Str(&o.schema), Param::Str(&o.name)],
        ),
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
              WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r', 'v', 'p')"
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
    let depths = crate::catalog::MEMBERSHIP_DEPTHS;
    format!(
        "WITH RECURSIVE {depths}, managed AS (
            SELECT r.oid FROM pg_catalog.pg_roles r
            JOIN (VALUES {values}) AS wanted(name) ON r.rolname = wanted.name
        ), targets AS (
            SELECT n.nspname AS schema_name, c.relname AS object_name,
                   'table' AS kind, '' AS signature, c.relowner AS owner, c.relacl AS acl,
                   pg_catalog.has_schema_privilege(n.oid, 'USAGE') AS usage_ok, '' AS column_name
              FROM pg_catalog.pg_class c
              JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r', 'v', 'p')
            UNION ALL
            SELECT n.nspname, t.relname, 'sequence', '', s.relowner, s.relacl,
                   pg_catalog.has_schema_privilege(n.oid, 'USAGE'), a.attname
              FROM pg_catalog.pg_class s
              JOIN pg_catalog.pg_depend d
                ON d.objid = s.oid AND d.objsubid = 0 AND d.deptype = 'a'
               AND d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass
               AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass
              JOIN pg_catalog.pg_class t ON t.oid = d.refobjid AND t.relkind IN ('r', 'p')
              JOIN pg_catalog.pg_attribute a
                ON a.attrelid = t.oid AND a.attnum = d.refobjsubid
               AND a.attnum > 0 AND NOT a.attisdropped
              JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace
             WHERE s.relkind = 'S'
            UNION ALL
            SELECT n.nspname, p.proname, 'routine',
                   COALESCE((SELECT pg_catalog.string_agg(pg_catalog.format_type(u.ty, NULL), ', ' ORDER BY u.pos)
                       FROM pg_catalog.unnest(p.proargtypes) WITH ORDINALITY u(ty, pos)), ''),
                   p.proowner, p.proacl, pg_catalog.has_schema_privilege(n.oid, 'USAGE'), ''
              FROM pg_catalog.pg_proc p
              JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
             WHERE p.prokind IN ('f', 'p')
            UNION ALL
            SELECT n.nspname, '', 'schema', '', n.nspowner, n.nspacl, true, ''
              FROM pg_catalog.pg_namespace n
        )
        SELECT t.schema_name, t.object_name, t.kind, t.signature,
               a.privilege_type AS privilege, pg_catalog.pg_get_userbyid(a.grantor) AS grantor,
               {held} AS held,
               t.usage_ok, t.column_name
          FROM targets t
          CROSS JOIN LATERAL pg_catalog.aclexplode(t.acl) a
          JOIN managed m ON m.oid = a.grantee
          CROSS JOIN pg_catalog.pg_roles me
         WHERE me.rolname = current_user AND a.grantee <> t.owner
         ORDER BY 1, 2, 3, 4, 5, 6"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopted_sequence_authority_uses_its_owning_column_in_a_managed_table() {
        let table = ObjectName::new("app", "t");
        let decode = |kind: &str, column: &str| {
            catalog_securable(kind, table.clone(), String::new(), column.to_owned())
        };
        let sequence = decode("sequence", "id").unwrap();
        assert_eq!(sequence, Securable::Sequence(table.column("id")));
        assert!(manages(
            &sequence,
            &[table.clone()].into(),
            &BTreeSet::new()
        ));
        assert!(!manages(&sequence, &BTreeSet::new(), &BTreeSet::new()));
        assert_eq!(decode("sequence", ""), None, "no owning column");
        assert_eq!(decode("table", "id"), Some(Securable::Object(table)));
    }

    #[test]
    fn explicit_sequence_authority_uses_the_column_and_each_requested_privilege() {
        let target = "sequence::app.t.id".parse().unwrap();
        for right in [Permission::Usage, Permission::Select, Permission::Update] {
            let (securable, sql, params) = question(&target, right);
            assert_eq!(securable, Securable::Sequence("app.t.id".parse().unwrap()));
            assert_eq!(params.len(), 3);
            assert!(
                sql.contains("pg_catalog.pg_get_serial_sequence")
                    && sql.contains("has_sequence_privilege")
            );
            assert!(!sql.contains("has_table_privilege"));
            assert!(manages(
                &securable,
                &[ObjectName::new("app", "t")].into(),
                &BTreeSet::new()
            ));
            assert!(!manages(&securable, &BTreeSet::new(), &BTreeSet::new()));
        }
    }

    #[test]
    fn serial_grant_authority_is_asked_of_owned_sequences_and_not_the_table() {
        let target: GrantTarget = "app.t".parse().unwrap();
        let (securable, query, params) = question(&target, Permission::SerialSequences);
        assert_eq!(securable, Securable::Object(ObjectName::new("app", "t")));
        assert_eq!(params.len(), 2);
        assert!(
            query.contains("has_sequence_privilege(s.oid, $3)"),
            "{query}"
        );
        assert!(query.contains("d.deptype = 'a'"), "{query}");
        assert!(
            query.contains("COALESCE(pg_catalog.pg_partition_root(t.oid)::pg_catalog.oid, t.oid)"),
            "{query}"
        );
        assert!(query.contains("s.relkind = 'S'"), "{query}");
        assert!(
            query.contains("d.refclassid = 'pg_catalog.pg_class'"),
            "{query}"
        );
        assert!(query.contains("COALESCE(pg_catalog.bool_and("), "{query}");
        assert!(!query.contains("has_table_privilege"), "{query}");
        let (_, ordinary, _) = question(&target, Permission::Insert);
        assert!(ordinary.contains("has_table_privilege"), "{ordinary}");
        assert!(!ordinary.contains("has_sequence_privilege"), "{ordinary}");
    }

    #[test]
    fn routine_grant_lookup_and_bindings_use_the_catalogs_raw_signature() {
        let part = "x\n\\\u{200b}";
        let arg = format!("s.\"{part}\"");
        let routine: pbps_model::RoutineId = format!("app.f({arg},integer)").parse().unwrap();
        let target = GrantTarget::Routine(routine.clone());
        let modules = [ModuleId::Routine(routine)].into();
        let (securable, _, params) = question(&target, Permission::Execute);
        assert!(manages(&securable, &BTreeSet::new(), &modules));
        assert_eq!(
            securable,
            Securable::Routine(ObjectName::new("app", "f"), Some(format!("{arg}, integer")))
        );
        assert!(matches!(params[2], Param::Str(actual) if actual == arg));
        let escaped = Securable::Routine(
            ObjectName::new("app", "f"),
            Some("s.\"x\\n\\\\\\u{200b}\", integer".into()),
        );
        assert!(!manages(&escaped, &BTreeSet::new(), &modules));
        assert!(!manages(
            &Securable::Routine(ObjectName::new("app", "f"), None),
            &BTreeSet::new(),
            &modules
        ));
    }
}
