//! What a plan's schema grants do to the deployer's authorization, the part of
//! qualifying an analysis scope that is the same for every engine (ADR-0016
//! cases 5, 14, 16, 21, 23; SPEC §9.3.3).
//!
//! The resolver lifecycle — which session is opened when, what is sealed,
//! what a later check re-reads — is the CLI's and is the same for every
//! engine. What a scope *is* belongs to the engine crates: the facts they
//! read, the versioned rule that compares them, the deployment authorization
//! context and how it is reproduced on scratch. Each engine states those
//! through `ResolverEngine` (`resolver/contract.rs`), so this module names no
//! engine and adding one is a compile error in its own impl rather than a
//! branch someone forgot (docs/ARCHITECTURE.md; #714). What is left here is
//! the planned schema grants, the digest framing apply and the producer share,
//! and the shell of the persisted authorization condition.
//!
//! The way each engine reproduces a plan's preceding grants differs, and its
//! reason is kept with that engine: PostgreSQL's in `postgres/principals.rs`,
//! SQL Server's beside its `apply_planned` in `mssql.rs`.

pub(crate) mod object_grants;

/// A schema grant or revoke the plan performs before its DDL. A GRANT keeps
/// the emitter's whole comma-separated privilege mask in one entry; REVOKE
/// has one privilege per entry because PostgreSQL emits separate statements.
/// `principal` is the role or user the statement names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedGrant {
    pub principal: pbps_model::RoleName,
    pub schema: String,
    pub privilege: String,
    pub revoke: bool,
}

/// The label authorization digests are keyed under; distinct from the capture
/// rule's so that no authorization digest equals a catalog one.
const AUTHORIZATION_RULE: &str = "pbps/authorization-context/v1";

/// Seals a deployer context, given as its engine's canonical bytes, together
/// with the selected object-grant answers. Their keyed fingerprints use the
/// same endpoint as the context, and remain separate from catalog binding
/// inputs. The producer seals with it and apply compares with it, so the two
/// cannot frame a context differently.
pub(crate) fn authorization_digest(
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    canonical: &[u8],
    grants: &[String],
) -> String {
    if grants.is_empty() {
        return crate::resolver::sealing::hex(key.fingerprint(
            AUTHORIZATION_RULE,
            "authorization",
            canonical,
        ));
    }
    let combined =
        serde_json::to_vec(&(canonical, grants)).expect("authorization condition serialises");
    crate::resolver::sealing::hex(key.fingerprint(
        "pbps/authorization-with-object-grants/v1",
        "authorization",
        &combined,
    ))
}

/// The ordered schema-grant sequence whose projection the qualified scope
/// used. Compare it with the final pure planner changes before persisting
/// either authorization or target visibility.
pub(crate) fn planned_schema_grants(
    changes: &pbps_model::ChangeSet,
) -> Result<Vec<PlannedGrant>, String> {
    use pbps_model::{Change, GrantTarget, PermissionChange};
    let mut planned = Vec::new();
    for p in &changes.changes {
        if matches!(p.change, Change::Role(_)) {
            return Err(
                "persistent resolver authorization cannot project role identity changes".into(),
            );
        }
        if let Some((_, table)) = p.change.pending_rebuild() {
            return Err(format!(
                "blocking rewrite for `{}` is unavailable",
                table.shown()
            ));
        }
        // Only a role's grant on a schema moves the schema authorization: a
        // grant on anything else, and `PUBLIC`'s execution, project nothing.
        let Some((role, GrantTarget::Schema(schema), moved)) = p.change.grant() else {
            continue;
        };
        let (permissions, revoke) = match moved {
            PermissionChange::Granted(permissions) => (permissions, false),
            PermissionChange::Revoked(permissions) => (permissions, true),
        };
        let privileges = permissions
            .iter()
            .map(|permission| permission.as_str().to_ascii_uppercase().replace('-', " "))
            .collect::<Vec<_>>();
        if privileges.is_empty() {
            return Err("a planned schema grant names no permission".into());
        }
        if revoke {
            // PostgreSQL emits one REVOKE statement per permission.
            for privilege in privileges {
                planned.push(PlannedGrant {
                    principal: role.clone(),
                    schema: schema.clone(),
                    privilege,
                    revoke: true,
                });
            }
        } else {
            planned.push(PlannedGrant {
                principal: role.clone(),
                schema: schema.clone(),
                privilege: privileges.join(", "),
                revoke: false,
            });
        }
    }
    Ok(planned)
}

/// The persisted authorization condition: what the plan's schema grants and
/// revokes do to the deployer's context, digested before and after. The shell
/// is the same for every engine; what the context becomes is the engine's
/// (`transition`): PostgreSQL projects the planned grants onto it, and SQL
/// Server takes what the verified reproduction measured, and refuses without it.
pub(crate) fn persisted_after<E: crate::resolver::contract::ResolverEngine>(
    authorization: &E::Authorization,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    changes: &pbps_model::ChangeSet,
    object_grants: Vec<pbps_model::resolver::AuthorizationGrant>,
    measured: Option<&E::Condition>,
) -> Result<pbps_model::resolver::AuthorizationCondition, String> {
    use pbps_model::Change;
    let planned = planned_schema_grants(changes)?;
    if !E::unpredictable(authorization, &planned).is_empty() {
        return Err("the adapter cannot derive the approved authorization transition".into());
    }
    let (before, after) = E::transition(authorization, &planned, measured)?;
    let before_grants: Vec<_> = object_grants
        .iter()
        .map(|g| g.before.fingerprint.clone())
        .collect();
    let after_grants: Vec<_> = object_grants
        .iter()
        .map(|g| g.after.fingerprint.clone())
        .collect();
    Ok(pbps_model::resolver::AuthorizationCondition {
        rule: E::authorization_rule()?.into(),
        before: authorization_digest(key, &before, &before_grants),
        after: authorization_digest(key, &after, &after_grants),
        object_grants,
        changes: changes
            .changes
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                matches!(
                    p.change,
                    Change::Permission(_)
                        | Change::TypeValue(_)
                        | Change::Type(_)
                        | Change::Role(_)
                )
            })
            .map(|(i, _)| i)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::server::PlanningDialect;
    use pbps_mssql::resolver::authorization as mssql_auth;
    use pbps_pg::resolver::authorization as pg_auth;
    use std::collections::BTreeMap;

    type Pg = <pbps_pg::Postgres as PlanningDialect>::Resolver;

    #[cfg(unix)]
    fn fixed_key() -> pbps_db::fingerprint::EnvironmentFingerprintKey {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path =
            std::env::temp_dir().join(format!("pbps2798-digest-key-{}", rand::random::<u64>()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=")
            .unwrap();
        let key = pbps_db::fingerprint::EnvironmentFingerprintKey::from_file(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        key
    }

    pub(super) fn pg_fixture() -> pg_auth::AuthorizationContext {
        pg_auth::AuthorizationContext {
            principal: pbps_db::resolver::environment::DeploymentPrincipal {
                login: "d".into(),
                effective: "d".into(),
                superuser: true,
            },
            schemas: [(
                "app".to_owned(),
                pg_auth::SchemaAuthorization {
                    owner: "d".into(),
                    privileges: BTreeMap::from([("USAGE".into(), true), ("CREATE".into(), true)]),
                    acl: pbps_pg::resolver::authorization::SchemaAcl::Default,
                },
            )]
            .into_iter()
            .collect(),
            roles: BTreeMap::new(),
            settings: BTreeMap::new(),
        }
    }

    /// What apply compares the sealed digest with: the shared framing over the
    /// engine's canonical bytes, with and without object-grant fingerprints.
    /// The expected values were recorded from the union's `digest` before it
    /// was removed (its equality with this framing was asserted then), for a
    /// fixed context of each engine and the fixed key.
    #[cfg(unix)]
    #[test]
    fn the_digest_of_each_engines_context_is_the_shared_framing_over_its_canonical_bytes() {
        let key = fixed_key();
        let pg = pg_fixture().canonical();
        let ms = mssql_auth::fixture_context().canonical();
        let two = ["aa".to_owned(), "bb".to_owned()];
        for (name, canonical, grants, expected) in [
            (
                "postgres",
                &pg,
                &[][..],
                "f0e9dc9ac0184ca6faac24ff7a4601d4c5caaf34332aaeead452f284a5d193e4",
            ),
            (
                "postgres",
                &pg,
                &two[..],
                "c82ec2630df9af38dbe54a2b05b2d1ae474579f3a68a397340daa2b4998451dd",
            ),
            (
                "mssql",
                &ms,
                &[][..],
                "531c26ef913988967c07f2f11ceca3def7e717ce4928570740dae14af64e1a30",
            ),
            (
                "mssql",
                &ms,
                &two[..],
                "cf58e5d043593850885fa7a3ef4f5dd7706003174935a7adeedfd0eefcd81cb7",
            ),
        ] {
            assert_eq!(
                authorization_digest(&key, canonical, grants),
                expected,
                "{name} with {} object grants",
                grants.len()
            );
        }
        // Negative: the two engines' contexts never digest alike.
        assert_ne!(
            authorization_digest(&key, &pg, &[]),
            authorization_digest(&key, &ms, &[])
        );
    }

    /// The persisted after-digest moves with a planned grant, and the
    /// before-digest does not: the two are different facts.
    #[cfg(unix)]
    #[test]
    fn a_planned_grant_moves_the_after_digest_and_not_the_before() {
        let postgres = pg_fixture();
        assert_eq!(
            <Pg as crate::resolver::contract::ResolverEngine>::authorization_rule().unwrap(),
            "pg-auth-v2"
        );
        let key = fixed_key();
        let persisted = |changes: &pbps_model::ChangeSet| {
            persisted_after::<Pg>(&postgres, &key, changes, Vec::new(), None)
        };
        let unchanged = persisted(&pbps_model::ChangeSet::default()).unwrap();
        assert_eq!(unchanged.before, unchanged.after);
        let changes = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::Permission(pbps_model::GrantChange::Grant {
                    role: "PUBLIC".into(),
                    target: pbps_model::GrantTarget::Schema("app".into()),
                    permissions: std::collections::BTreeSet::from([pbps_model::Permission::Usage]),
                }),
            )],
        };
        let projected = persisted(&changes).unwrap();
        assert_eq!(projected.before, unchanged.before);
        assert_ne!(projected.before, projected.after);
        assert_eq!(projected.changes, std::collections::BTreeSet::from([0]));
        let unsupported = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Role(
                pbps_model::RoleChange::CreateRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: "new_role".into(),
                },
            ))],
        };
        assert!(persisted(&unsupported).is_err());
    }

    /// SQL Server's after-state is what the verified reproduction measured; a
    /// seal without that measurement is refused, never read as "unchanged".
    #[cfg(unix)]
    #[test]
    fn sql_server_refuses_to_persist_an_authorization_it_did_not_measure() {
        type Ms = <pbps_mssql::Mssql as PlanningDialect>::Resolver;
        let context = Box::new(mssql_auth::fixture_context());
        let refused = persisted_after::<Ms>(
            &context,
            &fixed_key(),
            &pbps_model::ChangeSet::default(),
            Vec::new(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            refused,
            "the SQL Server authorization after-state was not read from the verified reproduction"
        );
    }

    fn facts() -> pbps_db::resolver::environment::CatalogFacts {
        pbps_db::resolver::environment::CatalogFacts {
            observations: BTreeMap::new(),
            extensions: Vec::new(),
            available_extensions: BTreeMap::new(),
            collations: Vec::new(),
            settings: BTreeMap::new(),
            visibility: BTreeMap::from([(
                "app".to_owned(),
                pbps_db::resolver::Observation::reported(Some("as read")),
            )]),
        }
    }

    /// The recheck compares each engine's complete context by its canonical
    /// bytes, the predicate it had: an unchanged read is the same context, and
    /// a changed field of either engine's is not.
    #[test]
    fn each_engine_compares_its_own_contexts_by_their_canonical_bytes() {
        use crate::resolver::contract::ResolverEngine;
        type Ms = <pbps_mssql::Mssql as PlanningDialect>::Resolver;
        let pg = pg_fixture();
        assert!(Pg::same_context(&pg, &pg.clone()));
        let mut revoked = pg.clone();
        revoked
            .schemas
            .get_mut("app")
            .unwrap()
            .privileges
            .insert("USAGE".into(), false);
        assert!(!Pg::same_context(&pg, &revoked));
        let ms = Box::new(mssql_auth::fixture_context());
        assert!(Ms::same_context(&ms, &ms.clone()));
        let mut moved = ms.clone();
        moved.language = "german".into();
        assert!(!Ms::same_context(&ms, &moved));
    }

    /// PostgreSQL derives the deployer's visibility from the projected
    /// context; SQL Server's grants change no binding order, so its facts
    /// stand as read. Nothing planned on SQL Server is "unpredictable".
    #[test]
    fn only_postgres_rewrites_visibility_and_only_it_can_find_a_grant_unpredictable() {
        use crate::resolver::contract::ResolverEngine;
        type Ms = <pbps_mssql::Mssql as PlanningDialect>::Resolver;
        let planned = [PlannedGrant {
            principal: "d".into(),
            schema: "app".into(),
            privilege: "USAGE".into(),
            revoke: true,
        }];
        let schemas = ["app".to_owned()];
        let mut postgres = facts();
        // A superuser keeps USAGE whatever is revoked; the deployer here does not.
        let mut deployer = pg_fixture();
        deployer.principal.superuser = false;
        Pg::project_visibility(&deployer, &planned, &schemas, &[], &mut postgres);
        assert_eq!(
            postgres.visibility["app"],
            pbps_db::resolver::Observation::reported(Some(r#"["pg_catalog"]"#)),
            "the revoked USAGE leaves only pg_catalog on the path"
        );
        let mut sql_server = facts();
        Ms::project_visibility(
            &Box::new(mssql_auth::fixture_context()),
            &planned,
            &schemas,
            &[],
            &mut sql_server,
        );
        assert_eq!(sql_server, facts());
        assert!(Ms::unpredictable(&Box::new(mssql_auth::fixture_context()), &planned).is_empty());
        assert!(
            Ms::unusable_schemas(&Box::new(mssql_auth::fixture_context()), &planned).is_empty()
        );
    }
}

// One change of every kind, the corpus `pbps-model` keeps complete
// (`the_equivalence_corpus_covers_every_change_kind`).
#[cfg(test)]
#[path = "../../../pbps-model/tests/support/part_effect_cases.rs"]
mod change_kinds;

#[cfg(test)]
mod planned_schema_grants_tests {
    use super::*;
    use pbps_model::{Change, ChangeSet, GrantChange, GrantTarget, Permission, PlannedChange};
    use std::collections::BTreeSet;

    fn one(change: Change) -> ChangeSet {
        ChangeSet {
            changes: vec![PlannedChange::new(change)],
        }
    }

    fn on(target: GrantTarget, permissions: &[Permission], revoke: bool) -> ChangeSet {
        let permissions: BTreeSet<_> = permissions.iter().copied().collect();
        one(Change::Permission(if revoke {
            GrantChange::Revoke {
                role: "reader".into(),
                target,
                permissions,
            }
        } else {
            GrantChange::Grant {
                role: "reader".into(),
                target,
                permissions,
            }
        }))
    }

    /// The corpus grants and revokes on a table, so every kind in it but the
    /// two refused classes projects nothing.
    #[test]
    fn every_kind_but_a_schema_grant_projects_nothing_or_is_refused() {
        for (name, change) in change_kinds::cases() {
            let answer = planned_schema_grants(&one(change.clone()));
            match name.split('/').next().unwrap() {
                "CreateRole" | "DropRole" | "RenameRole" => assert_eq!(
                    answer.unwrap_err(),
                    "persistent resolver authorization cannot project role identity changes",
                    "{name}"
                ),
                "RebuildPartitionedTable"
                | "CopyTableRows"
                | "AnalyzeTableTree"
                | "BuildPartitionedIndex" => assert_eq!(
                    answer.unwrap_err(),
                    format!(
                        "blocking rewrite for `{}` is unavailable",
                        change.table().unwrap().shown()
                    ),
                    "{name}"
                ),
                _ => assert_eq!(answer, Ok(Vec::new()), "{name}"),
            }
        }
    }

    #[test]
    fn a_schema_grant_is_one_entry_and_a_schema_revoke_is_one_per_permission() {
        let both = [Permission::Usage, Permission::Create];
        let entry = |privilege: &str, revoke| PlannedGrant {
            principal: "reader".into(),
            schema: "app".into(),
            privilege: privilege.into(),
            revoke,
        };
        assert_eq!(
            planned_schema_grants(&on(GrantTarget::Schema("app".into()), &both, false)),
            Ok(vec![entry("USAGE, CREATE", false)])
        );
        assert_eq!(
            planned_schema_grants(&on(GrantTarget::Schema("app".into()), &both, true)),
            Ok(vec![entry("USAGE", true), entry("CREATE", true)])
        );
        // Negative: the same permissions on a table are not a schema's.
        let table = GrantTarget::Object("app.t".parse().unwrap());
        assert_eq!(
            planned_schema_grants(&on(table, &[Permission::Select], false)),
            Ok(Vec::new())
        );
    }

    #[test]
    fn a_schema_grant_that_names_no_permission_is_refused() {
        for revoke in [false, true] {
            assert_eq!(
                planned_schema_grants(&on(GrantTarget::Schema("app".into()), &[], revoke)),
                Err("a planned schema grant names no permission".into())
            );
        }
    }
}

// ORACLE-2997-BEGIN
#[cfg(test)]
mod planned_schema_grants_listing_oracle {
    use super::*;
    use pbps_model::{Change, ChangeSet, GrantChange, GrantTarget, Permission, PlannedChange};
    use std::collections::BTreeSet;

    fn listed(changes: &pbps_model::ChangeSet) -> Result<Vec<PlannedGrant>, String> {
        use pbps_model::GrantChange;
        use pbps_model::IndexChange;
        use pbps_model::PartitionChange;
        use pbps_model::{Change, GrantTarget};
        let mut planned = Vec::new();
        for p in &changes.changes {
            if matches!(p.change, Change::Role(_)) {
                return Err(
                    "persistent resolver authorization cannot project role identity changes".into(),
                );
            }
            let statement = match &p.change {
                Change::Partition(PartitionChange::RebuildPartitionedTable(_))
                | Change::Index(IndexChange::BuildPartitionedIndex(_))
                | Change::Partition(PartitionChange::CopyTableRows(_))
                | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => {
                    return Err(format!(
                        "blocking rewrite for `{}` is unavailable",
                        p.change.table().unwrap().shown()
                    ));
                }
                Change::Permission(GrantChange::Grant {
                    role,
                    target: GrantTarget::Schema(schema),
                    permissions,
                })
                | Change::Permission(GrantChange::Revoke {
                    role,
                    target: GrantTarget::Schema(schema),
                    permissions,
                }) => Some((role, schema, permissions)),
                Change::Table(_)
                | Change::Partition(PartitionChange::DetachPartition { .. })
                | Change::Partition(PartitionChange::AttachPartition { .. })
                | Change::Column(_)
                | Change::Policy(_)
                | Change::TableProperty(_)
                | Change::Index(IndexChange::SetIndexStorageParameters { .. })
                | Change::Partition(PartitionChange::SetPartitionDefault { .. })
                | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
                | Change::Constraint(_)
                | Change::Index(IndexChange::AddIndex { .. })
                | Change::Index(IndexChange::DropIndex { .. })
                | Change::Row(_)
                | Change::Module(_)
                | Change::TypeValue(_)
                | Change::Role(_)
                | Change::Type(_)
                | Change::Search(_)
                | Change::Extension(_)
                | Change::Permission(_) => None,
            };
            if let Some((role, schema, permissions)) = statement {
                let privileges = permissions
                    .iter()
                    .map(|permission| permission.as_str().to_ascii_uppercase().replace('-', " "))
                    .collect::<Vec<_>>();
                if privileges.is_empty() {
                    return Err("a planned schema grant names no permission".into());
                }
                if matches!(&p.change, Change::Permission(GrantChange::Grant { .. })) {
                    planned.push(PlannedGrant {
                        principal: role.clone(),
                        schema: schema.clone(),
                        privilege: privileges.join(", "),
                        revoke: false,
                    });
                } else {
                    // PostgreSQL emits one REVOKE statement per permission.
                    for privilege in privileges {
                        planned.push(PlannedGrant {
                            principal: role.clone(),
                            schema: schema.clone(),
                            privilege,
                            revoke: true,
                        });
                    }
                }
            }
        }
        Ok(planned)
    }

    #[test]
    fn the_projections_answer_as_the_listing_did_for_every_kind() {
        let permission = |role: &str, target: GrantTarget, permissions: &[Permission], revoke| {
            let permissions: BTreeSet<_> = permissions.iter().copied().collect();
            let role = role.into();
            Change::Permission(if revoke {
                GrantChange::Revoke {
                    role,
                    target,
                    permissions,
                }
            } else {
                GrantChange::Grant {
                    role,
                    target,
                    permissions,
                }
            })
        };
        let schema = || GrantTarget::Schema("app".into());
        let both = [Permission::Usage, Permission::Create];
        let extras = [
            ("Grant/schema", permission("reader", schema(), &both, false)),
            ("Revoke/schema", permission("reader", schema(), &both, true)),
            (
                "Grant/schema-empty",
                permission("reader", schema(), &[], false),
            ),
            (
                "Revoke/schema-empty",
                permission("reader", schema(), &[], true),
            ),
            (
                "Grant/table",
                permission(
                    "reader",
                    GrantTarget::Object("app.t".parse().unwrap()),
                    &[Permission::Select],
                    false,
                ),
            ),
        ]
        .map(|(name, change)| (name.to_owned(), change));
        let cases: Vec<_> = change_kinds::cases().into_iter().chain(extras).collect();
        assert!(cases.len() > 100, "{}", cases.len());
        for (name, change) in &cases {
            let set = ChangeSet {
                changes: vec![PlannedChange::new(change.clone())],
            };
            assert_eq!(planned_schema_grants(&set), listed(&set), "{name}");
        }
        // A sequence too: the first refusal wins, and what precedes it is lost.
        let sequence = |names: &[&str]| ChangeSet {
            changes: names
                .iter()
                .map(|wanted| {
                    let (_, change) = cases.iter().find(|(name, _)| name == wanted).unwrap();
                    PlannedChange::new(change.clone())
                })
                .collect(),
        };
        for names in [
            &["Grant/schema", "Revoke/schema", "Grant/table"][..],
            &["Grant/schema", "CreateRole", "CopyTableRows"],
            &["Revoke/schema", "BuildPartitionedIndex", "DropRole"],
            &["Grant/schema", "Grant/schema-empty"],
        ] {
            let set = sequence(names);
            assert_eq!(planned_schema_grants(&set), listed(&set), "{names:?}");
        }
    }
}
// ORACLE-2997-END
