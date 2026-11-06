//! Resolver-backed transactional apply (#616 on PostgreSQL, #2144 on SQL
//! Server; SPEC 9.3.2).
//!
//! A resolved plan is applied as it was sealed: no resolver, no scratch, no
//! new binding decision. What this adds to an ordinary apply is the check
//! that the target is still the one the evidence describes, through the
//! apply's own connection: before the transaction, the key and the cluster;
//! inside it before the first statement, the target's facts, its deployer's
//! authorization and the opening manifest; after the last statement and
//! before recording, the closing manifest and the authorization the plan
//! leaves. Any difference refuses, and inside the transaction it rolls back.
//!
//! The checks are the same on both engines and the reads are not. PostgreSQL
//! reads inside one snapshot; SQL Server has none (DEC-619.1), so its checks
//! are plain reads compared at both ends, and a change made and restored in
//! between leaves nothing to find (DEC-2144.1).

use crate::resolver::{scope, sealing};
use pbps_db::Driver;
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_model::dependency::Dependencies;
use pbps_model::resolver::{ObjectIdentity, ObjectOwnership, ResolverEvidence, Surface};
use pbps_model::{IdsFile, ModuleId, SavedPlan, Schema};
use std::collections::BTreeMap;

/// A resolved plan whose sealing key is the environment's. Made only by
/// [`Keyed::check`], before apply connects.
pub struct Keyed<'a> {
    evidence: &'a ResolverEvidence,
    changes: &'a pbps_model::ChangeSet,
    declarations: Dependencies,
    key: EnvironmentFingerprintKey,
    label: String,
}

/// The checks one resolved plan's apply runs, under the key it was sealed
/// with. Made only by [`Keyed::open`], after the key and the cluster passed.
pub struct Guard<'a> {
    evidence: &'a ResolverEvidence,
    changes: &'a pbps_model::ChangeSet,
    declarations: &'a Dependencies,
    driver: Driver,
    key: &'a EnvironmentFingerprintKey,
    scope_schemas: Vec<String>,
    label: &'a str,
}

/// The remedy for a target that is no longer the sealed one: the evidence
/// describes another, and only a new plan describes this one.
const PLAN_AGAIN: &str = "Plan again against the target as it is now.";

impl<'a> Keyed<'a> {
    /// Before apply connects (#616 step 1): the environment's key is the one
    /// the plan was sealed under (DEC-952.1). `None` for a plan without
    /// resolver evidence. `key` is the environment's key as loaded; `label`
    /// names the target in a refusal.
    pub fn check(
        plan: &'a SavedPlan,
        key: Result<EnvironmentFingerprintKey, sealing::KeyRequired>,
        label: &str,
    ) -> anyhow::Result<Option<Self>> {
        let pbps_model::resolver::PlanAnalysis::Resolved(evidence) = &plan.analysis else {
            return Ok(None);
        };
        let sealed = evidence.before().key_id();
        let key = key.map_err(|why| {
            anyhow::anyhow!(
                "this plan's resolver evidence was sealed under fingerprint key {sealed}, and no \
                 key is configured for `{label}`: {why}\n\
                 Evidence sealed under one key is no evidence under another (DEC-952.1). {PLAN_AGAIN}"
            )
        })?;
        if key.id().as_str() != sealed {
            anyhow::bail!(
                "this plan's resolver evidence was sealed under fingerprint key {sealed}, and \
                 `{label}` has key {}.\n\
                 Evidence sealed under one key is no evidence under another (DEC-952.1). \
                 {PLAN_AGAIN}",
                key.id()
            );
        }
        // The plan retains the declarations the planning capture read under
        // (DEC-2322.1); a recapture without them would read a module that
        // names a declared database as an uncovered name.
        let declarations = plan
            .dependencies
            .iter()
            .map(|(name, planned)| (name.clone(), planned.declaration.clone()))
            .collect();
        Ok(Some(Self {
            evidence,
            changes: &plan.changes,
            declarations,
            key,
            label: label.to_owned(),
        }))
    }

    /// Under the deployment lock, before the transaction (#616 step 3):
    /// `conn` reaches the cluster the planning read did (DEC-1685.1).
    pub async fn open(&self, conn: &mut pbps_db::Conn) -> anyhow::Result<Guard<'_>> {
        let (evidence, key, label) = (self.evidence, &self.key, self.label.as_str());
        let qualification = evidence.qualification();
        if qualification.target_cluster.is_empty() {
            anyhow::bail!(
                "this plan's resolver evidence names no cluster it was sealed against. {PLAN_AGAIN}"
            );
        }
        let driver = conn.driver();
        let (identity, rule, component) = match driver {
            Driver::Postgres => (
                pbps_pg::resolver::scratch::cluster_identity(conn).await,
                crate::resolver::server::CLUSTER_RULE,
                "system-identifier",
            ),
            Driver::Mssql => (
                pbps_mssql::resolver::scratch::cluster_identity(conn).await,
                crate::resolver::server::MSSQL_CLUSTER_RULE,
                "family-guid",
            ),
        };
        let identity = identity.map_err(|error| {
            anyhow::anyhow!("cannot read `{label}`'s cluster identity: {error}")
        })?;
        let reached = sealing::hex(key.fingerprint(rule, component, identity.as_bytes()));
        if reached != qualification.target_cluster {
            anyhow::bail!(
                "`{label}` reaches another cluster than the one this plan was sealed against \
                 (DEC-1685.1). {PLAN_AGAIN}"
            );
        }
        let target = &qualification.target_scope;
        let scope_schemas =
            scope::scope_schemas(driver, &target.schemas, &target.write_path_extras)
                .map_err(|why| anyhow::anyhow!("the sealed analysis scope: {why}"))?;
        Ok(Guard {
            evidence,
            changes: self.changes,
            declarations: &self.declarations,
            driver,
            key,
            scope_schemas,
            label,
        })
    }
}

impl Guard<'_> {
    /// Inside the transaction, under the deployment lock, before the first
    /// statement (#616 step 5): the target's facts and its deployer's
    /// authorization are the sealed ones, and the opening manifest's scope
    /// and lookups, including what the externals file staged, read the same.
    pub async fn before_statements(&self, conn: &mut pbps_db::Conn) -> anyhow::Result<()> {
        let mut changed = self
            .facts(
                conn,
                &self.evidence.qualification().target_environment,
                Phase::Before,
            )
            .await?;
        let sealed = self.evidence.before();
        let fresh = match self.driver {
            Driver::Postgres => {
                pbps_pg::resolver::capture::recheck_opening(conn, sealed, self.key).await
            }
            Driver::Mssql => {
                pbps_mssql::resolver::capture::recheck_opening(
                    conn,
                    sealed,
                    self.declarations,
                    self.key,
                )
                .await
            }
        }
        .map_err(|error| self.unreadable("the opening capture", error))?;
        changed.extend(sealed.opening_differences(&fresh));
        self.refuse(changed, "before its first statement")
    }

    /// Inside the transaction, after the last statement and before anything
    /// is recorded (#616 step 8, DEC-1274.1): the closing manifest's records,
    /// candidates and lookups as the plan left them, a record the seal does
    /// not hold proved to be one a transition installs, and the deployer's
    /// authorization the one the plan's own grants make. `schema` and `ids`
    /// are the declarations after the plan, which prove that ownership.
    pub async fn before_recording(
        &self,
        conn: &mut pbps_db::Conn,
        schema: &Schema,
        ids: &IdsFile,
    ) -> anyhow::Result<()> {
        let mut changed = self
            .facts(
                conn,
                &self.evidence.qualification().target_environment_after,
                Phase::After,
            )
            .await?;
        let after = self.evidence.after();
        let namespaces = self
            .evidence
            .qualification()
            .target_scope
            .schemas
            .iter()
            .chain(&self.evidence.qualification().target_scope.write_path_extras)
            .cloned()
            .collect();
        let fresh = match self.driver {
            Driver::Postgres => {
                let routines = self.routines();
                let recorded = pbps_pg::resolver::capture::RecordedOwnership {
                    schema,
                    ids,
                    routines: &routines,
                    dropped: &BTreeMap::new(),
                    namespaces: &namespaces,
                };
                pbps_pg::resolver::capture::capture_closing(
                    conn,
                    after,
                    &after.vanishable(),
                    self.key,
                    &recorded,
                )
                .await
            }
            Driver::Mssql => {
                // No routine identity is proved apart from the declarations:
                // SQL Server routines are objects of their schema like any
                // other (DEC-2144.1).
                let recorded = pbps_mssql::resolver::capture::RecordedOwnership {
                    schema,
                    ids,
                    namespaces: &namespaces,
                };
                pbps_mssql::resolver::capture::capture_closing(
                    conn,
                    after,
                    &after.vanishable(),
                    self.declarations,
                    self.key,
                    &recorded,
                )
                .await
            }
        }
        .map_err(|error| self.unreadable("the closing capture", error))?;
        changed.extend(after.closing_differences(&fresh, self.evidence.transitions()));
        self.refuse(changed, "after its last statement")
    }

    /// The target's reported facts and its deployer's authorization, each
    /// against the sealed phase's digest.
    async fn facts(
        &self,
        conn: &mut pbps_db::Conn,
        sealed: &str,
        phase: Phase,
    ) -> anyhow::Result<Vec<String>> {
        let target = &self.evidence.qualification().target_scope;
        let (catalog, authorization) = match self.driver {
            Driver::Postgres => {
                let scope = pbps_pg::resolver::environment::Scope {
                    schemas: &target.schemas,
                    write_path_extras: &target.write_path_extras,
                };
                pbps_pg::resolver::scope_facts_in_transaction(conn, &scope, &self.scope_schemas)
                    .await
                    .map(|(catalog, authorization)| {
                        (catalog, scope::Authorization::Postgres(authorization))
                    })
            }
            // The bracketed double read the planning used, inside the apply's
            // transaction. The principals the plan's own grants name are
            // resolved to the catalog's spelling within each read.
            Driver::Mssql => {
                let planned = scope::planned_schema_grants(self.changes)
                    .map_err(|why| anyhow::anyhow!("the plan's own grants: {why}"))?;
                async {
                    let (catalog, read) = scope::read_target(
                        conn,
                        Driver::Mssql,
                        &target.schemas,
                        &target.write_path_extras,
                        &self.scope_schemas,
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
                        [self.evidence.before(), self.evidence.after()],
                        self.declarations,
                        self.key,
                    )
                    .await?;
                    Ok((catalog, scope::Authorization::Mssql(Box::new(answered))))
                }
                .await
            }
        }
        .map_err(|error| {
            anyhow::anyhow!(
                "cannot read `{}`'s analysis scope: {error}\n\nNothing has been applied — the \
                 transaction was rolled back.",
                self.label
            )
        })?;
        let mut changed = Vec::new();
        if sealing::target_catalog_fingerprint(self.key, &catalog)
            .ok()
            .as_deref()
            != Some(sealed)
        {
            changed.push("the target's reported facts".to_owned());
        }
        let expected = match phase {
            Phase::Before => &self.evidence.authorization().before,
            Phase::After => &self.evidence.authorization().after,
        };
        let grants = &self.evidence.authorization().object_grants;
        let (fingerprints, grant_changes) = if grants.is_empty() {
            (Vec::new(), Vec::new())
        } else if self.driver == Driver::Mssql {
            // SQL Server's object permissions are part of the authorization
            // context the engine read (DEC-2143.1); evidence that claims
            // separate grants was not sealed by this build.
            return Err(self.unreadable(
                "the object grant postconditions",
                "SQL Server evidence carries none",
            ));
        } else {
            let read = pbps_pg::catalog::introspect_within_transaction(conn)
                .await
                .map_err(|error| self.unreadable("the object grant postconditions", error))?;
            let end = match phase {
                Phase::Before => scope::object_grants::End::Before,
                Phase::After => scope::object_grants::End::After,
            };
            scope::object_grants::recheck(self.key, self.changes, grants, &read, end)
                .map_err(|why| self.unreadable("the object grant postconditions", why))?
        };
        if authorization.digest(self.key, &fingerprints).as_ref() != Some(expected)
            && grant_changes.is_empty()
        {
            changed.push("the target deployer's authorization".to_owned());
        }
        changed.extend(grant_changes);
        Ok(changed)
    }

    /// Every declared routine's identity the evidence proved, by its module:
    /// the closing read classifies a record it does not hold against them.
    fn routines(&self) -> BTreeMap<ModuleId, ObjectIdentity> {
        let mut routines = BTreeMap::new();
        let mut proved = |object: &ObjectIdentity, ownership: &ObjectOwnership| {
            if let ObjectOwnership::Surface(Surface::Module(id @ ModuleId::Routine(_))) = ownership
                && object.class == "pg_proc"
            {
                routines.insert(id.clone(), object.clone());
            }
        };
        for record in self
            .evidence
            .before()
            .prerequisites()
            .iter()
            .chain(self.evidence.after().prerequisites())
        {
            proved(&record.object, &record.ownership);
        }
        for transition in self.evidence.transitions() {
            if let Surface::Module(id @ ModuleId::Routine(_)) = &transition.surface {
                for object in transition.after.iter().filter(|o| o.class == "pg_proc") {
                    routines.insert(id.clone(), object.clone());
                }
            }
        }
        routines
    }

    fn unreadable(&self, what: &str, error: impl std::fmt::Display) -> anyhow::Error {
        anyhow::anyhow!(
            "{what} of `{}` could not be read: {error}\n\
             An unreadable input is not an unchanged one. Nothing has been applied — the \
             transaction was rolled back.",
            self.label
        )
    }

    fn refuse(&self, changed: Vec<String>, when: &str) -> anyhow::Result<()> {
        if changed.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
            "`{}` is not the target this plan's resolver evidence was sealed against, {when}: \
             {}.\n\
             Nothing has been applied — the transaction was rolled back. {PLAN_AGAIN}",
            self.label,
            changed.join("; ")
        )
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Before,
    After,
}
