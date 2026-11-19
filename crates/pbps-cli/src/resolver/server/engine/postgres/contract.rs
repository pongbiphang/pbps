//! PostgreSQL's half of the resolver contract: what it supports and how it
//! rechecks a resolved plan at apply (DEC-2798.5). The recheck's reads are the
//! ones `resolver::apply` made before it named no engine, moved with their
//! comments; they run on the apply's own connection, inside its transaction.

use crate::resolver::contract::{
    ApplyRecheck, ClosingInputs, ClusterDomain, FactsError, GrantRecheck, Phase, Planning,
    Rehearsal, Replay, ResolverEngine, Step, Support, sealed::Sealed,
};
use crate::resolver::scope;
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, Driver};
use pbps_model::ModuleId;
use pbps_model::resolver::{
    InputManifest, ObjectIdentity, ObjectOwnership, ResolverEvidence, Surface,
};
use std::collections::BTreeMap;

/// PostgreSQL's resolver: planning, connected rehearsal and the apply recheck.
/// It has no target-less dev rehearsal, whose scratch lifecycle is SQL
/// Server's alone (SPEC §9.3).
pub struct PgResolver;

impl Sealed for PgResolver {}

/// The domain the planning seals the cluster under; apply recomputes it.
const CLUSTER: ClusterDomain = ClusterDomain {
    rule: crate::resolver::server::CLUSTER_RULE,
    component: "system-identifier",
};

impl ResolverEngine for PgResolver {
    type Dialect = pbps_pg::Postgres;
    const SUPPORT: Support = Support {
        planning: Some(&Planning { cluster: CLUSTER }),
        rehearsal: Some(&Rehearsal),
        preview: None,
        apply: Some(&PgResolver),
    };
}

impl ApplyRecheck for PgResolver {
    fn cluster(&self) -> ClusterDomain {
        CLUSTER
    }

    fn cluster_identity<'a>(&'a self, conn: &'a mut Conn) -> Step<'a, Result<String, String>> {
        Box::pin(async move {
            pbps_pg::resolver::scratch::cluster_identity(conn)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn scope_schemas(&self, schemas: &[String], extras: &[String]) -> Result<Vec<String>, String> {
        scope::scope_schemas(Driver::Postgres, schemas, extras)
    }

    fn recheck_opening<'a>(
        &'a self,
        conn: &'a mut Conn,
        r: Replay<'a>,
    ) -> Step<'a, Result<InputManifest, String>> {
        Box::pin(async move {
            let sealed = r.evidence.before();
            pbps_pg::resolver::capture::recheck_opening(conn, sealed, r.key)
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
            let routines = routines(r.evidence);
            let recorded = pbps_pg::resolver::capture::RecordedOwnership {
                schema: closing.schema,
                ids: closing.ids,
                routines: &routines,
                dropped: &BTreeMap::new(),
                namespaces: closing.namespaces,
            };
            pbps_pg::resolver::capture::capture_closing(
                conn,
                after,
                &after.vanishable(),
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
            let scope = pbps_pg::resolver::environment::Scope {
                schemas: &target.schemas,
                write_path_extras: &target.write_path_extras,
            };
            pbps_pg::resolver::scope_facts_in_transaction(conn, &scope, r.scope_schemas)
                .await
                .map(|(catalog, authorization)| {
                    (catalog, scope::Authorization::Postgres(authorization))
                })
                .map_err(|error| FactsError::Unread(error.to_string()))
        })
    }

    fn object_grants<'a>(
        &'a self,
        conn: &'a mut Conn,
        r: Replay<'a>,
        phase: Phase,
    ) -> Step<'a, Result<GrantRecheck, String>> {
        Box::pin(async move {
            let grants = &r.evidence.authorization().object_grants;
            let read = pbps_pg::catalog::introspect_within_transaction(conn)
                .await
                .map_err(|error| error.to_string())?;
            let end = match phase {
                Phase::Before => scope::object_grants::End::Before,
                Phase::After => scope::object_grants::End::After,
            };
            scope::object_grants::recheck(r.key, r.changes, grants, &read, end)
        })
    }
}

/// Every declared routine's identity the evidence proved, by its module:
/// the closing read classifies a record it does not hold against them.
fn routines(evidence: &ResolverEvidence) -> BTreeMap<ModuleId, ObjectIdentity> {
    let mut routines = BTreeMap::new();
    let mut proved = |object: &ObjectIdentity, ownership: &ObjectOwnership| {
        if let ObjectOwnership::Surface(Surface::Module(id @ ModuleId::Routine(_))) = ownership
            && object.class == "pg_proc"
        {
            routines.insert(id.clone(), object.clone());
        }
    };
    for record in evidence
        .before()
        .prerequisites()
        .iter()
        .chain(evidence.after().prerequisites())
    {
        proved(&record.object, &record.ownership);
    }
    for transition in evidence.transitions() {
        if let Surface::Module(id @ ModuleId::Routine(_)) = &transition.surface {
            for object in transition.after.iter().filter(|o| o.class == "pg_proc") {
                routines.insert(id.clone(), object.clone());
            }
        }
    }
    routines
}
