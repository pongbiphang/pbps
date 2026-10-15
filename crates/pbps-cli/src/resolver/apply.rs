//! Resolver-backed transactional apply on PostgreSQL (#616, SPEC 9.3.2).
//!
//! A resolved plan is applied as it was sealed: no resolver, no scratch, no
//! new binding decision. What this adds to an ordinary apply is the check
//! that the target is still the one the evidence describes, through the
//! apply's own connection: before the transaction, the key and the cluster;
//! inside it before the first statement, the target's facts, its deployer's
//! authorization and the opening manifest; after the last statement and
//! before recording, the closing manifest and the authorization the plan
//! leaves. Any difference refuses, and inside the transaction it rolls back.

use crate::resolver::{scope, sealing};
use pbps_db::Driver;
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_model::resolver::{ObjectIdentity, ObjectOwnership, ResolverEvidence, Surface};
use pbps_model::{IdsFile, ModuleId, SavedPlan, Schema};
use std::collections::BTreeMap;

/// A resolved plan whose sealing key is the environment's. Made only by
/// [`Keyed::check`], before apply connects.
pub struct Keyed<'a> {
    evidence: &'a ResolverEvidence,
    key: EnvironmentFingerprintKey,
    label: String,
}

/// The checks one resolved plan's apply runs, under the key it was sealed
/// with. Made only by [`Keyed::open`], after the key and the cluster passed.
pub struct Guard<'a> {
    evidence: &'a ResolverEvidence,
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
        Ok(Some(Self {
            evidence,
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
        let identity = pbps_pg::resolver::scratch::cluster_identity(conn)
            .await
            .map_err(|error| {
                anyhow::anyhow!("cannot read `{label}`'s cluster identity: {error}")
            })?;
        let reached = sealing::hex(key.fingerprint(
            crate::resolver::server::CLUSTER_RULE,
            "system-identifier",
            identity.as_bytes(),
        ));
        if reached != qualification.target_cluster {
            anyhow::bail!(
                "`{label}` reaches another cluster than the one this plan was sealed against \
                 (DEC-1685.1). {PLAN_AGAIN}"
            );
        }
        let target = &qualification.target_scope;
        let scope_schemas =
            scope::scope_schemas(Driver::Postgres, &target.schemas, &target.write_path_extras)
                .map_err(|why| anyhow::anyhow!("the sealed analysis scope: {why}"))?;
        Ok(Guard {
            evidence,
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
        let fresh =
            pbps_pg::resolver::capture::recheck_opening(conn, self.evidence.before(), self.key)
                .await
                .map_err(|error| self.unreadable("the opening capture", error))?;
        changed.extend(self.evidence.before().opening_differences(&fresh));
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
        let routines = self.routines();
        let namespaces = self
            .evidence
            .qualification()
            .target_scope
            .schemas
            .iter()
            .chain(&self.evidence.qualification().target_scope.write_path_extras)
            .cloned()
            .collect();
        let recorded = pbps_pg::resolver::capture::RecordedOwnership {
            schema,
            ids,
            routines: &routines,
            dropped: &BTreeMap::new(),
            namespaces: &namespaces,
        };
        let fresh = pbps_pg::resolver::capture::capture_closing(
            conn,
            after,
            &after.vanishable(),
            self.key,
            &recorded,
        )
        .await
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
        let pbps_pg_scope = pbps_pg::resolver::environment::Scope {
            schemas: &target.schemas,
            write_path_extras: &target.write_path_extras,
        };
        let (catalog, authorization) = pbps_pg::resolver::scope_facts_in_transaction(
            conn,
            &pbps_pg_scope,
            &self.scope_schemas,
        )
        .await
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
        let authorization = scope::Authorization::Postgres(authorization);
        let expected = match phase {
            Phase::Before => &self.evidence.authorization().before,
            Phase::After => &self.evidence.authorization().after,
        };
        if authorization.digest(self.key).as_ref() != Some(expected) {
            changed.push("the target deployer's authorization".to_owned());
        }
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

    fn unreadable(
        &self,
        what: &str,
        error: pbps_db::resolver::capture::CaptureError,
    ) -> anyhow::Error {
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
