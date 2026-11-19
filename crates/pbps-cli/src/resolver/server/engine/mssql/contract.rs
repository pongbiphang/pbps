//! SQL Server's half of the resolver contract: what it supports and how it
//! rechecks a resolved plan at apply (DEC-2798.5). The recheck's reads are the
//! ones `resolver::apply` made before it named no engine, moved with their
//! comments; they run on the apply's own connection, inside its transaction.
//! SQL Server has no snapshot (DEC-619.1), so its checks are plain reads
//! compared at both ends, and a change made and restored in between leaves
//! nothing to find (DEC-2144.1).

use crate::resolver::contract::{
    ApplyRecheck, ClosingInputs, ClusterDomain, FactsError, GrantRecheck, Phase, Planning, Preview,
    Rehearsal, Replay, ResolverEngine, Step, Support, sealed::Sealed,
};
use crate::resolver::scope;
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, DbError, Driver};
use pbps_model::resolver::InputManifest;

/// SQL Server's resolver: planning, both rehearsals and the apply recheck.
pub struct MssqlResolver;

impl Sealed for MssqlResolver {}

/// The domain the planning seals the cluster under; apply recomputes it.
const CLUSTER: ClusterDomain = ClusterDomain {
    rule: crate::resolver::server::MSSQL_CLUSTER_RULE,
    component: "family-guid",
};

impl ResolverEngine for MssqlResolver {
    type Dialect = pbps_mssql::Mssql;
    const SUPPORT: Support = Support {
        planning: Some(&Planning { cluster: CLUSTER }),
        rehearsal: Some(&Rehearsal),
        preview: Some(&Preview),
        apply: Some(&MssqlResolver),
    };
}

impl ApplyRecheck for MssqlResolver {
    fn cluster(&self) -> ClusterDomain {
        CLUSTER
    }

    fn cluster_identity<'a>(&'a self, conn: &'a mut Conn) -> Step<'a, Result<String, String>> {
        Box::pin(async move {
            pbps_mssql::resolver::scratch::cluster_identity(conn)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn scope_schemas(&self, schemas: &[String], extras: &[String]) -> Result<Vec<String>, String> {
        scope::scope_schemas(Driver::Mssql, schemas, extras)
    }

    fn recheck_opening<'a>(
        &'a self,
        conn: &'a mut Conn,
        r: Replay<'a>,
    ) -> Step<'a, Result<InputManifest, String>> {
        Box::pin(async move {
            let sealed = r.evidence.before();
            pbps_mssql::resolver::capture::recheck_opening(conn, sealed, r.declarations, r.key)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn capture_closing<'a>(
        &'a self,
        conn: &'a mut Conn,
        r: Replay<'a>,
        closing: ClosingInputs<'a>,
    ) -> Step<'a, Result<InputManifest, String>> {
        Box::pin(async move {
            let after = r.evidence.after();
            // No routine identity is proved apart from the declarations:
            // SQL Server routines are objects of their schema like any
            // other (DEC-2144.1).
            let recorded = pbps_mssql::resolver::capture::RecordedOwnership {
                schema: closing.schema,
                ids: closing.ids,
                namespaces: closing.namespaces,
            };
            pbps_mssql::resolver::capture::capture_closing(
                conn,
                after,
                &after.vanishable(),
                r.declarations,
                r.key,
                &recorded,
            )
            .await
            .map_err(|error| error.to_string())
        })
    }

    fn scope_facts<'a>(
        &'a self,
        conn: &'a mut Conn,
        r: Replay<'a>,
    ) -> Step<'a, Result<(CatalogFacts, scope::Authorization), FactsError>> {
        Box::pin(async move {
            let target = &r.evidence.qualification().target_scope;
            // The bracketed double read the planning used, inside the apply's
            // transaction. The principals the plan's own grants name are
            // resolved to the catalog's spelling within each read.
            let planned = scope::planned_schema_grants(r.changes)
                .map_err(|why| FactsError::Refused(format!("the plan's own grants: {why}")))?;
            async {
                let (catalog, read) = scope::read_target(
                    conn,
                    Driver::Mssql,
                    &target.schemas,
                    &target.write_path_extras,
                    r.scope_schemas,
                    &planned,
                )
                .await?;
                // What the plan sealed is the deployer's answers, not the
                // catalog read alone: the engine-read effective
                // permissions of the same questions (DEC-2143.1).
                let scope::Authorization::Mssql(read) = read else {
                    unreachable!("a SQL Server read answers with SQL Server's context")
                };
                let planned_principals: Vec<_> = planned
                    .iter()
                    .map(|grant| grant.principal.clone())
                    .collect();
                let answered = pbps_mssql::resolver::capture::answered_authorization(
                    conn,
                    &read,
                    &planned_principals,
                    [r.evidence.before(), r.evidence.after()],
                    r.declarations,
                    r.key,
                )
                .await?;
                Ok::<_, DbError>((catalog, scope::Authorization::Mssql(Box::new(answered))))
            }
            .await
            .map_err(|error| FactsError::Unread(error.to_string()))
        })
    }

    fn object_grants<'a>(
        &'a self,
        _conn: &'a mut Conn,
        _r: Replay<'a>,
        _phase: Phase,
    ) -> Step<'a, Result<GrantRecheck, String>> {
        // SQL Server's object permissions are part of the authorization
        // context the engine read (DEC-2143.1); evidence that claims
        // separate grants was not sealed by this build.
        Box::pin(async { Err("SQL Server evidence carries none".to_owned()) })
    }
}
