//! A SQL Server run seals evidence the artifact reader reads back, on supplied
//! and Docker scratch (#2143, DEC-2143.1-.4).

use super::reconstruct2374::{both, declared, exec, module, table};
use super::*;
use crate::resolver::server::ResolvedPlan;
use pbps_dialect::Dialect;
use pbps_diff::order::Canonical;
use pbps_model::resolver::{AuthorizationReproduction, ResolverRuntime, SameInstance, Surface};
use pbps_model::{
    Column, ComputedColumn, ModuleKind, PlanBaseline, PlanOrigin, SavedPlan, TableName,
};
use std::pin::Pin;

type Plan = ResolvedPlan<Canonical<pbps_mssql::Mssql>>;

/// The artifact a planner would write: the plan's changes with this evidence.
fn saved(plan: &Plan) -> SavedPlan {
    SavedPlan::new(
        PlanOrigin::Database,
        "mssql",
        "fixture",
        PlanBaseline {
            time_zone: None,
            description: "target database".into(),
            checksum: "00".repeat(32),
            database_collation: None,
        },
        plan.changes.changes().clone(),
        pbps_model::IdsFile::default(),
    )
    .with_resolution(plan.evidence.clone())
    .unwrap()
}

fn reread(saved: &SavedPlan) -> SavedPlan {
    serde_json::from_slice(&serde_json::to_vec(saved).unwrap()).unwrap()
}

const SEED: &[&str] = &[
    "CREATE TABLE dbo.t2143(id int NOT NULL);",
    "CREATE VIEW dbo.v2143 AS SELECT id FROM dbo.t2143;",
];

/// The target already holds a managed table and a view over it, and the plan
/// keeps them: the opening and compiled captures read the same roots.
async fn view_over_table(target: &str, seed: &[&str]) -> Declared {
    exec(target, seed).await;
    let mut declared = declared(&[module(
        ModuleKind::View,
        "dbo.v2143",
        "SELECT id FROM dbo.t2143",
    )]);
    let (name, mut table) = table("dbo.t2143");
    table.columns.get_mut("id").unwrap().nullable = false;
    declared.desired.tables.insert(name, table);
    declared.base = declared.desired.clone();
    declared
}

async fn plan(target: &str, scratch: &str, declared: &Declared) -> Plan {
    declared_plan(target, scratch, declared, None)
        .await
        .unwrap_or_else(|error| panic!("the run should seal a plan: {error}"))
        .0
}

fn route(plan: &Plan) -> pbps_model::resolver::ScratchLayout {
    let ResolverRuntime::Scratch { layout, .. } = &plan.evidence.qualification().runtime;
    *layout
}

/// Case 7: the evidence a run seals is read back through the SQL Server
/// reader, whole, and only by it.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_shared_evidence_reads_back_through_the_sql_server_reader() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let declared = view_over_table(target, SEED).await;
            let (plan, begun) = declared_plan(target, scratch, &declared, None)
                .await
                .unwrap_or_else(|error| panic!("the run should seal a plan: {error}"));
            plan.evidence.validate(&plan.changes).unwrap();
            pbps_mssql::resolver::validate_evidence(&plan.evidence).unwrap();
            let q = plan.evidence.qualification();
            assert_eq!(q.authorization, AuthorizationReproduction::Reproduced);
            let ResolverRuntime::Scratch {
                outside_writes,
                same_instance,
                dependencies,
                ..
            } = &q.runtime;
            assert_eq!(*same_instance, SameInstance::Refused);
            assert_eq!(
                *outside_writes,
                pbps_model::resolver::OutsideWrites::Refused
            );
            assert!(dependencies.is_empty());
            // The target identity is the keyed digest of the instance's own
            // identity, under a SQL Server label, never PostgreSQL's.
            let key =
                crate::resolver::sealing::environment_key(&begun.key.project, Some(ENVIRONMENT))
                    .unwrap();
            assert_eq!(
                q.target_cluster,
                crate::resolver::server::producer::fingerprint(
                    &key,
                    "pbps/mssql-instance-identity/v1",
                    "family-guid",
                    begun.identity.as_bytes(),
                )
            );
            assert_ne!(q.target_cluster, q.channels);
            // Through the file and the dialect's artifact admission.
            let artifact = saved(&plan);
            pbps_mssql::Mssql.validate_plan_analysis(&artifact).unwrap();
            pbps_mssql::Mssql
                .validate_plan_analysis(&reread(&artifact))
                .unwrap();
            // The other engine's reader does not read it, and it does not
            // read a tampered qualification.
            assert!(pbps_pg::resolver::validate_evidence(&plan.evidence).is_err());
            let mut json = serde_json::to_value(&plan.evidence).unwrap();
            *json.pointer_mut("/qualification/rule").unwrap() =
                serde_json::json!("pg-reported-scope-v2");
            let foreign: pbps_model::resolver::ResolverEvidence =
                serde_json::from_value(json).unwrap();
            assert!(pbps_mssql::resolver::validate_evidence(&foreign).is_err());
            let _ = route(&plan);
        })
    }
    both(case).await;
}

/// The recheck reads the cluster again; another one than the planning read
/// named is refused, not sealed.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_target_reaching_another_cluster_is_not_sealed() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let declared = view_over_table(target, SEED).await;
            let Err(error) =
                declared_plan(target, scratch, &declared, Some("another-instance")).await
            else {
                panic!("a plan sealed against another cluster");
            };
            assert!(error.to_string().contains("family_guid"), "{error}");
            // The same run against the cluster it planned on seals.
            plan(target, scratch, &declared).await;
        })
    }
    both(case).await;
}

/// A plan's own grant is in the sealed authorization: the engine's answer
/// after the grant differs from the answer before it, and a plan without one
/// seals equal endpoints.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_planned_grant_moves_the_sealed_authorization_condition() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            exec(target, &["CREATE ROLE readers2143;"]).await;
            let mut declared = view_over_table(target, SEED).await;
            let role: pbps_model::RoleName = "readers2143".into();
            declared
                .base
                .roles
                .insert(role.clone(), pbps_model::Role::default());
            declared
                .desired
                .roles
                .insert(role.clone(), pbps_model::Role::default());
            let without = plan(target, scratch, &declared).await;
            let ev = &without.evidence.authorization();
            assert_eq!(ev.before, ev.after, "no planned grant, no movement");
            declared.desired.roles.insert(
                role,
                pbps_model::Role {
                    description: None,
                    grants: [(
                        "schema::dbo".parse().unwrap(),
                        [pbps_model::Permission::Select].into(),
                    )]
                    .into(),
                },
            );
            let with = plan(target, scratch, &declared).await;
            let ev = with.evidence.authorization();
            assert_ne!(ev.before, ev.after, "the grant must change the condition");
            pbps_mssql::Mssql
                .validate_plan_analysis(&saved(&with))
                .unwrap();
        })
    }
    both(case).await;
}

fn with_computed(expression: &str) -> Schema {
    let mut schema = Schema::default();
    let (name, mut table) = table("dbo.t2143");
    let mut id = Column::new("int".parse().unwrap());
    id.nullable = false;
    table.columns.insert("id".into(), id);
    table.computed.insert(
        "c".into(),
        ComputedColumn {
            expression: expression.into(),
            persisted: false,
            not_null: false,
        },
    );
    schema.tables.insert(name, table);
    schema
}

/// A computed column is the Default-compatible surface of its column: changing
/// its expression is assessed for it, not skipped, and the sealed evidence is
/// read back whole.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_computed_column_binds_as_a_surface_and_a_changed_expression_is_assessed() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            exec(
                target,
                &["CREATE TABLE dbo.t2143(id int NOT NULL, c AS (id * 2));"],
            )
            .await;
            let declared = Declared {
                base: with_computed("id * 2"),
                desired: with_computed("id * 3"),
            };
            let plan = plan(target, scratch, &declared).await;
            let column = TableName::new("dbo", "t2143").column("c");
            let surface = Surface::Default(column);
            let resolution = plan
                .evidence
                .surfaces()
                .iter()
                .find(|s| s.surface == surface)
                .unwrap_or_else(|| {
                    panic!(
                        "the computed column must be an assessed surface: {:?}",
                        plan.evidence.surfaces()
                    )
                });
            let (current, desired) = (
                resolution.current.as_ref().expect("held by the target"),
                resolution.desired.as_ref().expect("declared"),
            );
            assert!(
                !desired.bindings.is_empty() && desired.bindings == current.bindings,
                "it binds the column it reads, unchanged by the expression: {desired:?}"
            );
            pbps_mssql::Mssql
                .validate_plan_analysis(&saved(&plan))
                .unwrap();
        })
    }
    both(case).await;
}

fn roots(manifest: &pbps_model::resolver::InputManifest) -> std::collections::BTreeSet<String> {
    manifest
        .scope()
        .retained
        .iter()
        .map(|o| o.name.join("."))
        .collect()
}

/// A plan that creates a managed view over a standing table: the scratch
/// retains a root the target does not hold yet, the closing scope gains
/// exactly that one, and the sealed evidence is read back whole (DEC-2143.5).
/// A created table with typed columns binds built-in types the opening
/// capture never reached; that is a separate boundary, not covered here.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_plan_that_creates_a_view_seals_its_root() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut declared = view_over_table(target, &SEED[..1]).await;
            declared.base.modules.clear();
            let plan = plan(target, scratch, &declared).await;
            plan.evidence.validate(&plan.changes).unwrap();
            let (opening, closing) = (roots(plan.evidence.before()), roots(plan.evidence.after()));
            assert!(
                opening.contains("dbo.t2143") && !opening.contains("dbo.v2143"),
                "{opening:?}"
            );
            assert!(
                closing.contains("dbo.t2143") && closing.contains("dbo.v2143"),
                "{closing:?}"
            );
            pbps_mssql::Mssql
                .validate_plan_analysis(&saved(&plan))
                .unwrap();
        })
    }
    both(case).await;
}

/// A plan that drops managed objects: the opening scope retains roots the
/// scratch does not hold, and the closing scope loses exactly those.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_plan_that_drops_objects_releases_their_roots() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let kept = view_over_table(target, SEED).await;
            let declared = Declared {
                base: kept.base,
                desired: Schema::default(),
            };
            let plan = plan(target, scratch, &declared).await;
            plan.evidence.validate(&plan.changes).unwrap();
            let (opening, closing) = (roots(plan.evidence.before()), roots(plan.evidence.after()));
            assert!(
                opening.contains("dbo.t2143") && opening.contains("dbo.v2143"),
                "{opening:?}"
            );
            assert!(
                !closing.contains("dbo.t2143") && !closing.contains("dbo.v2143"),
                "{closing:?}"
            );
            pbps_mssql::Mssql
                .validate_plan_analysis(&saved(&plan))
                .unwrap();
        })
    }
    both(case).await;
}

/// A created table binds the engine's built-in types, which exist on every
/// instance: the opening capture reads them on the target too, so the plan
/// seals and reads back (DEC-2143.6).
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_plan_that_creates_a_typed_table_seals_and_reads_back() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut declared = declared(&[module(
                ModuleKind::View,
                "dbo.v2143",
                "SELECT id FROM dbo.t2143",
            )]);
            let (name, mut table) = table("dbo.t2143");
            table.columns.get_mut("id").unwrap().nullable = false;
            declared.desired.tables.insert(name, table);
            let plan = plan(target, scratch, &declared).await;
            plan.evidence.validate(&plan.changes).unwrap();
            let (opening, closing) = (roots(plan.evidence.before()), roots(plan.evidence.after()));
            assert!(
                opening.contains("sys.int"),
                "the built-in type is read on the target: {opening:?}"
            );
            assert!(!opening.contains("dbo.t2143"), "{opening:?}");
            assert!(
                closing.contains("dbo.t2143") && closing.contains("sys.int"),
                "{closing:?}"
            );
            pbps_mssql::Mssql
                .validate_plan_analysis(&saved(&plan))
                .unwrap();
        })
    }
    both(case).await;
}

/// Only what the engine ships is admitted that way: a user-defined type the
/// opening capture never read stays unresolved, however it is spelled.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_user_defined_type_absent_from_opening_is_not_admitted_as_a_built_in() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            exec(target, &["CREATE TYPE dbo.amount2143 FROM int NOT NULL;"]).await;
            let mut declared = Declared::default();
            let (name, mut table) = table("dbo.t2143");
            table
                .columns
                .insert("id".into(), Column::new("dbo.amount2143".parse().unwrap()));
            declared.desired.tables.insert(name, table);
            let outcome = declared_plan(target, scratch, &declared, None).await;
            assert!(
                outcome.is_err(),
                "a user type no capture read must not be sealed as a built-in"
            );
        })
    }
    both(case).await;
}

/// A filtered index's predicate binds only its own table's columns on SQL
/// Server, so an index with one is no resolver surface and the plan seals
/// (DEC-2143.2).
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_filtered_index_is_no_resolver_surface() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut declared = view_over_table(target, SEED).await;
            exec(
                target,
                &["CREATE INDEX ix2143 ON dbo.t2143(id) WHERE id > 0;"],
            )
            .await;
            for schema in [&mut declared.base, &mut declared.desired] {
                schema
                    .tables
                    .get_mut(&"dbo.t2143".parse().unwrap())
                    .unwrap()
                    .indexes
                    .insert(
                        "ix2143".into(),
                        pbps_model::Index {
                            columns: vec![pbps_model::IndexColumn {
                                key: pbps_model::IndexKey::Column("id".into()),
                                descending: false,
                                opclass: None,
                            }],
                            include: vec![],
                            unique: false,
                            filter: Some("id > 0".into()),
                            method: Default::default(),
                            storage_parameters: Default::default(),
                        },
                    );
            }
            let plan = plan(target, scratch, &declared).await;
            plan.evidence.validate(&plan.changes).unwrap();
            assert!(
                !plan
                    .evidence
                    .surfaces()
                    .iter()
                    .any(|s| matches!(s.surface, Surface::Index { .. })),
                "{:?}",
                plan.evidence.surfaces()
            );
            pbps_mssql::Mssql
                .validate_plan_analysis(&saved(&plan))
                .unwrap();
        })
    }
    both(case).await;
}
