//! What an engine says about the resolver, once, at its dialect (DEC-2798.5).
//!
//! The static half is data: which resolver operations an engine supports and
//! the facts the sealed evidence names. Absence is an answer of its own, so an
//! engine without a resolver writes `type Resolver = NoResolver<Self>` and every
//! operation it lacks is refused by name, by the admission function that holds
//! the facts. No consumer holds a fact it did not admit, and none holds a
//! `bool` beside a placeholder.
//!
//! The resolved-apply recheck is the one engine half already typed to nothing
//! but strings and manifests, so it is an object-safe trait selected here; the
//! scratch lifecycle carries engine-branded state and will be typed through
//! `PlanningDialect::Resolver` instead.

use crate::resolver::scope;
use crate::resolver::server::scratch::rehearse::Roles;
use crate::resolver::server::scratch::{Provenance, Provisioning, Tokens};
use crate::resolver::server::{BindingRequest, Error, PlanningDialect, ProduceError, ScopeRequest};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::capture::{Assessment, CaptureDifference, CaptureError};
use pbps_db::resolver::environment::{CatalogFacts, DatabaseRecipe, ScopeReport};
use pbps_db::{Conn, DbError};
use pbps_model::dependency::Dependencies;
use pbps_model::dependency::DependencyName;
use pbps_model::operation::Captured;
use pbps_model::resolver::{
    AuthorizationGrant, AuthorizationReproduction, ManifestError, ObjectIdentity, ScratchLayout,
    Surface,
};
use pbps_model::resolver::{DependencyPlacement, OutsideWrites};
use pbps_model::resolver::{InputManifest, ResolverEvidence};
use pbps_model::{ChangeSet, Hints, IdsFile, ModuleDeps, RoleName, Schema, SessionZone};
use pbps_pg::resolver::capture::{BindingRecord, SealError};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;

/// A boxed engine step. Boxed so the trait stays object-safe and so the
/// resolver's future stays small (`plan_db` already boxes it for that).
pub(crate) type Step<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(crate) mod sealed {
    /// Only this crate's engines name a resolver.
    pub trait Sealed {}
}

/// The identity domain a cluster is fingerprinted under. Planning seals it and
/// apply recomputes it, so both read the one value an engine declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterDomain {
    pub rule: &'static str,
    pub component: &'static str,
}

/// Facts the sealed evidence names, each written once per engine and read by
/// the seal: the domain the cluster identity is keyed under, the rule the
/// qualified connections are keyed under, and what the evidence claims of the
/// reproduced authorization for the layout the run used.
pub struct Planning {
    pub cluster: ClusterDomain,
    pub channels_rule: &'static str,
    pub reproduction: fn(ScratchLayout) -> AuthorizationReproduction,
}

/// Connected rehearsal: the same scratch, nothing sealed. What an engine
/// decides about it is stated here once, where the rehearsal used to compare
/// the driver.
pub struct Rehearsal {
    /// How the scope the rehearsal qualifies is asked of the plan.
    pub request: RehearsalRequest,
    /// Whether the rehearsal's captures need a fingerprint key. They compare
    /// in memory and seal nothing, so the key is the run's own.
    pub run_key: bool,
}

/// The scope a rehearsal qualifies, by what the scratch stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RehearsalRequest {
    /// The plan's grants are projected onto the scope and replayed into the
    /// scratch before the rehearsal runs.
    Projected,
    /// The scratch stands for the target as it is now and the plan's own
    /// statements make every change, grants included (DEC-2379.3).
    AsItStands,
}

/// Target-less dev rehearsal. The operation itself moves in a later change.
pub struct Preview;

/// What a read of the target answers beside its catalog: the deployer's
/// authorization and the run-local history of the read.
pub type TargetRead<H> = (CatalogFacts, scope::Authorization, H);

/// What one engine supports. Written out in full per engine: there is no
/// default, so a new operation cannot silently read as unsupported or
/// supported.
pub struct Support {
    /// Resolver-backed planning (evidence production). `None` is a refusal.
    pub planning: Option<&'static Planning>,
    pub rehearsal: Option<&'static Rehearsal>,
    pub preview: Option<&'static Preview>,
    /// The recheck of a saved plan's resolver evidence at apply.
    pub apply: Option<&'static dyn ApplyRecheck>,
}

impl Support {
    pub const NONE: Self = Self {
        planning: None,
        rehearsal: None,
        preview: None,
        apply: None,
    };
}

/// The engine half of the resolver. Sealed: nothing outside this crate names
/// it, so no other crate can add an engine's resolver by implementing it.
///
/// Acquisition is typed by the engine: its standard and its scratch cannot be
/// another engine's, and a cleanup obligation is held in the engine's own
/// type. The prepared scratch is typed the same way and read through
/// `PreparedView`, so the run's generic code keeps its accessors. The binding
/// and the staged set are typed the same way. Only the captures they are
/// compared with are still the two-engine unions of `server::engine`, named in
/// the signatures until their own group moves: a capture of the other engine is
/// a named error in the engine's step.
pub trait ResolverEngine: sealed::Sealed + Sized + Send + Sync + 'static {
    /// The dialect this resolver belongs to, one to one: brand and state are
    /// the same engine by construction, not by a runtime comparison.
    type Dialect: PlanningDialect<Resolver = Self>;
    const SUPPORT: Support;

    /// Whether an index filter binds at creation; see
    /// `Dialect::filter_binds_expression`.
    const FILTER_BINDS: bool;

    /// The state the scratch is put into, as the entry declares it.
    type Standard: Clone + Send + Sync + 'static;
    /// What a run acquired and must release; borrows the run's request.
    type Scratch<'a>: Send
    where
        Self: 'a;
    /// The scratch once provisioned and qualified: the sessions the
    /// declarations compile in and what the reproduction found.
    type Prepared: PreparedView + Send;
    /// The reconstruction of the declarations on the scratch, and the target
    /// links it is compared against.
    type Binding: BindingView + Send + Sync;
    /// The inventory of the shapes the externals file was compared with.
    type Staged: Send + Sync;
    /// What the scratch's compile left to seal: private to the engine, read
    /// for its planning records and consumed exactly once by `seal_for_plan`.
    /// Never erased: a capture of one engine cannot be sealed by another.
    type CompiledCapture: Sealable + Send + Sync;
    /// The opening capture the publication recheck captures again.
    type CapturedInputs: Send + Sync;
    /// Run-local history beside the reported scope; never saved or
    /// fingerprinted, but it decides the recheck's verdict: a missing
    /// difference is a weaker recheck.
    type History: Send + Sync;

    /// The catalog classes a surface's creation-time record may have in this
    /// engine, or `None` where the engine has no mapping for it.
    fn binding_classes(surface: &Surface) -> Option<&'static [&'static str]>;
    /// An object this engine's catalog ties to one already reached (a
    /// foreign key's triggers, for instance).
    fn dependent_object(
        record: &BindingRecord,
        reached: &BTreeSet<ObjectIdentity>,
    ) -> Option<ObjectIdentity>;
    /// The rule the reported facts of the target and the scratch are compared
    /// under, as the evidence names it. An engine without a resolver has none.
    fn reported_rule() -> Result<&'static str, Error>;
    /// The object-grant snapshot the publication recheck retains, read on the
    /// target after a clean recheck. An engine that seals none holds none.
    fn retain_grants(
        target: &mut Conn,
    ) -> Step<'_, Result<Option<Box<pbps_db::catalog::Pulled>>, Error>>;
    /// The object-grant questions the final plan's grant writes seal, from the
    /// snapshot `retain_grants` held.
    fn seal_grants(
        key: &EnvironmentFingerprintKey,
        changes: &ChangeSet,
        ids: &IdsFile,
        held: Option<&pbps_db::catalog::Pulled>,
    ) -> Result<Vec<AuthorizationGrant>, String>;
    /// A keyed fingerprint of what the engine reports about its build.
    fn reported_build(
        key: &EnvironmentFingerprintKey,
        catalog: &CatalogFacts,
    ) -> Result<String, Error>;

    /// The standard an entry declares, or the refusal of one this engine
    /// cannot hold.
    fn standard(
        declared: Option<&pbps_config::resolver::ScratchStandard>,
    ) -> Result<Self::Standard, Error>;
    /// The string the scratch server is reached by for this run. An ordinary
    /// string is itself; a SQL Server `(localdb)` selector is resolved to the
    /// pipe its instance listens on now, once, so that every connection of the
    /// run opens the same endpoint and a pipe that has gone is a failure rather
    /// than a reason to look again (DEC-2271.2).
    fn endpoint(connection: &str) -> Step<'_, Result<String, ProduceError>>;
    /// The scratch session that stays open for the whole run: named by the
    /// run's token from its first packet, acting as itself, and told to let the
    /// server notice if its client vanishes (#2410).
    fn connect_scratch<'a>(
        scratch: &'a str,
        tokens: &'a Tokens,
    ) -> Step<'a, Result<Conn, ProduceError>>;
    /// The engine lineage identity the target reports.
    fn target_identity(target: &mut Conn) -> Step<'_, Result<String, DbError>>;
    /// The checked acquisition. The call's own borrows (`'b`) end with the
    /// call; what the scratch keeps (`'a`) lives as long as it does.
    #[allow(clippy::too_many_arguments)]
    fn acquire<'a, 'b>(
        target: &'b mut Conn,
        admin: &'b mut Conn,
        tokens: &'b Tokens,
        planned_identity: &'b str,
        scratch: &'a str,
        request: &'a ScopeRequest,
        standard: &'a Self::Standard,
        externals: Option<&'a str>,
        provenance: &'a Provenance,
    ) -> Step<'b, Result<(Provisioning, Self::Scratch<'a>), ProduceError>>
    where
        'a: 'b;
    /// Drops what the run created. `Err` names exactly what may remain.
    fn release<'a>(
        scratch: &'a mut Self::Scratch<'_>,
        admin: &'a mut Conn,
    ) -> Step<'a, Result<(), Vec<String>>>;
    /// A code vet stages only the operator's externals file. Automatic fill
    /// could install an extension whose absence must be named by CREATE.
    fn prepare_code<'a>(
        scratch: &'a mut Self::Scratch<'_>,
        provisioning: Provisioning,
        admin: &'a mut Conn,
        target: &'a CatalogFacts,
        authorization: &'a scope::Authorization,
        schemas: &'a [String],
    ) -> Step<'a, Result<Self::Prepared, Error>>;
    #[allow(clippy::too_many_arguments)]
    fn prepare<'a>(
        scratch: &'a mut Self::Scratch<'_>,
        provisioning: Provisioning,
        admin: &'a mut Conn,
        target_connection: &'a mut Conn,
        target: &'a CatalogFacts,
        authorization: &'a scope::Authorization,
        schemas: &'a [String],
        binding: &'a BindingRequest<'a>,
        extras: &'a [String],
        key: Option<&'a EnvironmentFingerprintKey>,
    ) -> Step<'a, Result<Self::Prepared, Error>>;
    fn reconstruction<'a>(
        scratch: &'a Self::Scratch<'_>,
        target: &'a mut Conn,
        binding: &'a BindingRequest<'a>,
        ids: &'a IdsFile,
        dependencies: &'a ModuleDeps,
        prepared: &'a mut Self::Prepared,
    ) -> Step<'a, Result<Self::Binding, Error>>;
    fn compare_externals<'a>(
        scratch: &'a Self::Scratch<'_>,
        provisioning: Provisioning,
        prepared: &'a mut Self::Prepared,
        admin: &'a mut Conn,
        target: &'a mut Conn,
        binding: &'a Self::Binding,
    ) -> Step<'a, Result<Self::Staged, Error>>;
    /// The staged inventory a run starts from when no externals file was
    /// compared. Preparation always compares the externals, an empty set
    /// included, where the engine has them.
    fn staged(prepared: &Self::Prepared) -> Self::Staged;
    /// The staged inventory preparation completed, where it did.
    fn completed_staged(prepared: &Self::Prepared) -> Option<Self::Staged>;
    fn chains(binding: &mut Self::Binding, prepared: &Self::Prepared) -> Vec<String>;
    #[allow(clippy::too_many_arguments)]
    fn compile<'a>(
        binding: &'a mut Self::Binding,
        prepared: &'a mut Self::Prepared,
        admin: &'a mut Conn,
        target: &'a mut Conn,
        extras: &'a [String],
        zone: Option<&'a SessionZone>,
        outside_writes: OutsideWrites,
        staged: &'a mut Self::Staged,
    ) -> Step<'a, Result<(), Error>>;
    #[allow(clippy::too_many_arguments)]
    fn capture_desired<'a>(
        binding: &'a Self::Binding,
        prepared: &'a mut Self::Prepared,
        admin: &'a mut Conn,
        target: &'a mut Conn,
        extras: &'a [String],
        visibility: &'a BTreeMap<String, pbps_db::resolver::Observation>,
        schemas: &'a [String],
        key: &'a EnvironmentFingerprintKey,
        desired: pbps_diff::Side<'a>,
        staged: &'a Self::Staged,
    ) -> Step<'a, Result<Self::CompiledCapture, Error>>;
    /// The opening capture of the target, as the binding reads it.
    fn capture_opening<'a>(
        binding: &'a Self::Binding,
        target: &'a mut Conn,
        closing: &'a Self::CompiledCapture,
        key: &'a EnvironmentFingerprintKey,
        base: pbps_diff::Side<'a>,
    ) -> Step<'a, Result<(Self::CapturedInputs, InputManifest), Error>>;
    fn assess(
        binding: &mut Self::Binding,
        closing: &Self::CompiledCapture,
        current: &Self::CapturedInputs,
        staged: &Self::Staged,
    ) -> Result<Assessment, Error>;
    /// The shapes the externals file was compared with that the opening
    /// capture finds changed.
    fn changed_since(staged: &Self::Staged, current: &Self::CapturedInputs) -> Vec<String>;
    /// What the target now holds that the opening capture's scope held then:
    /// the publication recheck's capture, taken afresh on the target.
    fn recapture<'a>(
        target: &'a mut Conn,
        current: &'a Self::CapturedInputs,
    ) -> Step<'a, Result<Vec<CaptureDifference>, CaptureError>>;
    /// What the run bound at creation, for ordering: the schema-bound callees
    /// of each module the capture read whole, on the target now and on the
    /// scratch database the declarations compiled on (DEC-2143.8). PostgreSQL
    /// has no such lexical guess to replace, so it brings none.
    fn captured_bindings(
        compiled: &Self::CompiledCapture,
        current: &Self::CapturedInputs,
        base: &Schema,
        desired: &Schema,
    ) -> Captured;
    /// A failure of the scratch compile, as the staged shapes explain it.
    fn through_shape_view(staged: &Self::Staged, error: Error) -> Error;
    /// The planning dialect the binding was generated under, from the same
    /// measured capability reconstruction used.
    fn dialect(binding: &Self::Binding, extras: &[String]) -> Self::Dialect;
    /// Static authorization seeds for a request. PostgreSQL adds write-path
    /// extras without the nonpersistent `pg_temp` alias. SQL Server's connected
    /// reader adds and qualifies dbo/default fallback questions (DEC-717.1),
    /// including known absence; its static seeds are not the frozen domain.
    /// SQL Server write-path extras still describe no supported path and refuse.
    fn scope_schemas(schemas: &[String], extras: &[String]) -> Result<Vec<String>, String>;
    /// The catalog facts of one side, as the connection's current principal.
    fn read_catalog<'a>(
        connection: &'a mut Conn,
        schemas: &'a [String],
        extras: &'a [String],
    ) -> Step<'a, Result<CatalogFacts, DbError>>;
    /// The target's catalog facts and its deployer's authorization context,
    /// read together so the sealed scope never holds half of a change: one
    /// catalog snapshot on PostgreSQL, a bracketed double read on SQL Server,
    /// whose catalog views no transaction makes a snapshot.
    fn read_target<'a>(
        connection: &'a mut Conn,
        schemas: &'a [String],
        extras: &'a [String],
        authorization_schemas: &'a [String],
        planned: &'a [scope::PlannedGrant],
    ) -> Step<'a, Result<(CatalogFacts, scope::Authorization), DbError>>;
    /// The producer's opening/recheck route, with a complete PostgreSQL
    /// sidecar. Ordinary reads and apply retain `read_target`'s existing
    /// endpoint-value contract.
    fn read_target_with_history<'a>(
        connection: &'a mut Conn,
        schemas: &'a [String],
        extras: &'a [String],
        authorization_schemas: &'a [String],
        planned: &'a [scope::PlannedGrant],
    ) -> Step<'a, Result<TargetRead<Self::History>, DbError>>;
    /// How the scratch database must be created to stand in for the target's.
    /// `Err` is why it cannot be, and says so.
    fn recipe(catalog: &CatalogFacts) -> Result<DatabaseRecipe, String>;
    /// The resolver's rule over what both sides report, which observes no
    /// process (#1672).
    fn compare_reported(
        target: &CatalogFacts,
        resolver: &CatalogFacts,
    ) -> Result<ScopeReport, String>;
    /// What the run-local history says the compared rule cannot, by name.
    fn limitation(history: &Self::History) -> Option<(&'static str, &'static str)>;
    /// What the target's history shows changed between two reads of it.
    fn history_differences(
        before: &Self::History,
        after: &Self::History,
    ) -> Result<Vec<String>, DbError>;
    /// The externals the base and the plan read are the target's, as for
    /// the resolver: a file that stages something else would rehearse a
    /// plan on a state the target does not hold. The comparison needs the
    /// resolver's reconstruction, which says which target objects are
    /// managed, so a rehearsal with nothing staged never builds it. What the
    /// base itself needs is the engine's: SQL Server compiles it, PostgreSQL
    /// only compares what was staged.
    #[allow(clippy::too_many_arguments)]
    fn rehearsal_base<'a>(
        scratch: &'a Self::Scratch<'_>,
        provisioning: Provisioning,
        prepared: &'a mut Self::Prepared,
        admin: &'a mut Conn,
        target: &'a mut Conn,
        binding: &'a BindingRequest<'a>,
        base_ids: &'a IdsFile,
        desired_ids: &'a IdsFile,
        hints: &'a Hints,
        desired: &'a Schema,
        externals: Option<&'a str>,
    ) -> Step<'a, Result<(), Error>>;
    /// The roles the plan names, as the run can have them, made on the
    /// scratch where the engine makes any. `Err` inside is why a rehearsal
    /// there cannot run the plan's authorization, and says so.
    fn rehearsal_roles<'a>(
        scratch: &'a mut Self::Scratch<'_>,
        admin: &'a mut Conn,
        named: &'a BTreeSet<RoleName>,
        base: &'a Schema,
        desired: &'a Schema,
        tokens: &'a Tokens,
    ) -> Step<'a, Result<Roles, Error>>;
}

/// What the run reads of an engine's prepared scratch. Held on the state's own
/// type, so a prepared scratch of one engine is never read as another's and the
/// run's generic code calls these as methods.
pub trait PreparedView: Sized + Send {
    /// What the authorization looked like after the run's work, where the
    /// engine measures it (SQL Server's `Condition`).
    fn authorization_condition<'a>(
        &'a mut self,
        target: &'a mut Conn,
    ) -> Step<'a, Result<Option<Box<pbps_mssql::resolver::authorization::Condition>>, Error>>;
    fn dependency_placements(&self) -> BTreeMap<DependencyName, DependencyPlacement>;
    fn catalog(&self) -> &CatalogFacts;
    #[cfg(test)]
    fn catalog_mut(&mut self) -> &mut CatalogFacts;
    fn into_catalog(self) -> CatalogFacts;
    /// The session the scratch's statements run on: the reproduced
    /// deployer's where the run made its own login, the checked connection
    /// where the scratch account compiles as itself.
    fn session<'a>(&'a mut self, admin: &'a mut Conn) -> &'a mut Conn;
    /// The session a rehearsal's statements run in: the reproduced deployer's,
    /// as an apply's are, where the scratch has one (SQL Server), and the
    /// checked connection otherwise.
    fn deployer<'a>(&'a mut self, admin: &'a mut Conn) -> &'a mut Conn;
    /// A connection that reads the scratch database as a whole, for the
    /// comparison after the plan ran: the deployer's cannot leave its context
    /// and may lack `VIEW DEFINITION`. `None` where the session reads it itself.
    fn reader(&self) -> Step<'_, Result<Option<Conn>, Error>>;
    fn differences(&self) -> &[String];
    fn supplied(&self) -> bool;
    fn has_filled(&self) -> bool;
    fn unfillable(&self) -> Vec<String>;
}

/// What the run reads of a compiled capture, and the one door it is sealed
/// through. On the capture's own type, like `PreparedView`: `seal_for_plan`
/// consumes it, so a capture is sealed at most once.
pub trait Sealable: Sized {
    /// The logical ownership records of what the compile left, safe to use
    /// before the capture itself is sealed.
    fn planning_records(&self) -> Result<Vec<BindingRecord>, ManifestError>;
    fn seal_for_plan(self) -> Result<InputManifest, SealError>;
}

/// What the run reads of an engine's binding. On the state's own type, like
/// `PreparedView`, so a binding of one engine is never read as another's.
pub trait BindingView: Sized + Send {
    /// Whether the externals fill reads managed relations.
    fn has_fill(&self) -> bool;
}

/// The state of a resolver that does not exist: no value can be made, so
/// every method that would consume one is unreachable by type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Never {}

/// The resolver of an engine that has none.
pub struct NoResolver<D>(PhantomData<fn() -> D>);

impl<D> sealed::Sealed for NoResolver<D> {}

/// What every operation that would start a run says. The admission functions
/// refuse before any of these is asked; this keeps a caller that skipped them
/// from going further, because no state can be made.
fn refusal<D: PlanningDialect>() -> Unsupported {
    Unsupported {
        engine: D::NAME,
        operation: Operation::Planning,
    }
}

impl<D: PlanningDialect<Resolver = Self>> ResolverEngine for NoResolver<D> {
    type Dialect = D;
    const SUPPORT: Support = Support::NONE;
    /// Configuration, not a state: it holds nothing to release and carries no
    /// brand, so a caller can name one. What it cannot do is go further:
    /// every operation that starts a run refuses by name, and the scratch
    /// that would follow is uninhabited.
    type Standard = ();
    type Scratch<'a> = Never;
    const FILTER_BINDS: bool = false;
    type Prepared = Never;
    type Binding = Never;
    type Staged = Never;
    type CompiledCapture = Never;
    type CapturedInputs = Never;
    type History = Never;

    fn binding_classes(_: &Surface) -> Option<&'static [&'static str]> {
        None
    }
    fn dependent_object(_: &BindingRecord, _: &BTreeSet<ObjectIdentity>) -> Option<ObjectIdentity> {
        None
    }
    fn retain_grants(
        _: &mut Conn,
    ) -> Step<'_, Result<Option<Box<pbps_db::catalog::Pulled>>, Error>> {
        Box::pin(std::future::ready(Err(Error::Unusable(
            refusal::<D>().to_string(),
        ))))
    }
    fn seal_grants(
        _: &EnvironmentFingerprintKey,
        _: &ChangeSet,
        _: &IdsFile,
        _: Option<&pbps_db::catalog::Pulled>,
    ) -> Result<Vec<AuthorizationGrant>, String> {
        Err(refusal::<D>().to_string())
    }
    fn reported_rule() -> Result<&'static str, Error> {
        Err(Error::Unusable(refusal::<D>().to_string()))
    }
    fn reported_build(_: &EnvironmentFingerprintKey, _: &CatalogFacts) -> Result<String, Error> {
        Err(Error::Unusable(refusal::<D>().to_string()))
    }
    fn standard(_: Option<&pbps_config::resolver::ScratchStandard>) -> Result<(), Error> {
        Err(Error::Unusable(refusal::<D>().to_string()))
    }
    fn endpoint(_: &str) -> Step<'_, Result<String, ProduceError>> {
        Box::pin(std::future::ready(Err(ProduceError::Acquire(
            refusal::<D>().to_string(),
        ))))
    }
    fn connect_scratch<'a>(_: &'a str, _: &'a Tokens) -> Step<'a, Result<Conn, ProduceError>> {
        Box::pin(std::future::ready(Err(ProduceError::Acquire(
            refusal::<D>().to_string(),
        ))))
    }
    fn target_identity(_: &mut Conn) -> Step<'_, Result<String, DbError>> {
        Box::pin(std::future::ready(Err(DbError::Refused(
            refusal::<D>().to_string(),
        ))))
    }
    fn acquire<'a, 'b>(
        _: &'b mut Conn,
        _: &'b mut Conn,
        _: &'b Tokens,
        _: &'b str,
        _: &'a str,
        _: &'a ScopeRequest,
        _: &'a (),
        _: Option<&'a str>,
        _: &'a Provenance,
    ) -> Step<'b, Result<(Provisioning, Never), ProduceError>>
    where
        'a: 'b,
    {
        Box::pin(std::future::ready(Err(ProduceError::Acquire(
            refusal::<D>().to_string(),
        ))))
    }
    fn release<'a>(scratch: &'a mut Never, _: &'a mut Conn) -> Step<'a, Result<(), Vec<String>>> {
        match *scratch {}
    }
    fn prepare_code<'a>(
        scratch: &'a mut Never,
        _: Provisioning,
        _: &'a mut Conn,
        _: &'a CatalogFacts,
        _: &'a scope::Authorization,
        _: &'a [String],
    ) -> Step<'a, Result<Never, Error>> {
        match *scratch {}
    }
    fn prepare<'a>(
        scratch: &'a mut Never,
        _: Provisioning,
        _: &'a mut Conn,
        _: &'a mut Conn,
        _: &'a CatalogFacts,
        _: &'a scope::Authorization,
        _: &'a [String],
        _: &'a BindingRequest<'a>,
        _: &'a [String],
        _: Option<&'a EnvironmentFingerprintKey>,
    ) -> Step<'a, Result<Never, Error>> {
        match *scratch {}
    }
    fn reconstruction<'a>(
        scratch: &'a Never,
        _: &'a mut Conn,
        _: &'a BindingRequest<'a>,
        _: &'a IdsFile,
        _: &'a ModuleDeps,
        _: &'a mut Never,
    ) -> Step<'a, Result<Never, Error>> {
        match *scratch {}
    }
    fn compare_externals<'a>(
        scratch: &'a Never,
        _: Provisioning,
        _: &'a mut Never,
        _: &'a mut Conn,
        _: &'a mut Conn,
        _: &'a Never,
    ) -> Step<'a, Result<Never, Error>> {
        match *scratch {}
    }
    fn staged(prepared: &Never) -> Never {
        match *prepared {}
    }
    fn completed_staged(prepared: &Never) -> Option<Never> {
        match *prepared {}
    }
    fn chains(_: &mut Never, prepared: &Never) -> Vec<String> {
        match *prepared {}
    }
    fn compile<'a>(
        _: &'a mut Never,
        prepared: &'a mut Never,
        _: &'a mut Conn,
        _: &'a mut Conn,
        _: &'a [String],
        _: Option<&'a SessionZone>,
        _: OutsideWrites,
        _: &'a mut Never,
    ) -> Step<'a, Result<(), Error>> {
        match *prepared {}
    }
    fn capture_desired<'a>(
        _: &'a Never,
        prepared: &'a mut Never,
        _: &'a mut Conn,
        _: &'a mut Conn,
        _: &'a [String],
        _: &'a BTreeMap<String, pbps_db::resolver::Observation>,
        _: &'a [String],
        _: &'a EnvironmentFingerprintKey,
        _: pbps_diff::Side<'a>,
        _: &'a Never,
    ) -> Step<'a, Result<Never, Error>> {
        match *prepared {}
    }
    fn capture_opening<'a>(
        binding: &'a Never,
        _: &'a mut Conn,
        _: &'a Never,
        _: &'a EnvironmentFingerprintKey,
        _: pbps_diff::Side<'a>,
    ) -> Step<'a, Result<(Never, InputManifest), Error>> {
        match *binding {}
    }
    fn assess(binding: &mut Never, _: &Never, _: &Never, _: &Never) -> Result<Assessment, Error> {
        match *binding {}
    }
    fn changed_since(staged: &Never, _: &Never) -> Vec<String> {
        match *staged {}
    }
    fn recapture<'a>(
        _: &'a mut Conn,
        current: &'a Never,
    ) -> Step<'a, Result<Vec<CaptureDifference>, CaptureError>> {
        match *current {}
    }
    fn captured_bindings(compiled: &Never, _: &Never, _: &Schema, _: &Schema) -> Captured {
        match *compiled {}
    }
    fn through_shape_view(staged: &Never, _: Error) -> Error {
        match *staged {}
    }
    fn dialect(binding: &Never, _: &[String]) -> D {
        match *binding {}
    }
    fn scope_schemas(_: &[String], _: &[String]) -> Result<Vec<String>, String> {
        Err(refusal::<D>().to_string())
    }
    fn read_catalog<'a>(
        _: &'a mut Conn,
        _: &'a [String],
        _: &'a [String],
    ) -> Step<'a, Result<CatalogFacts, DbError>> {
        Box::pin(std::future::ready(Err(DbError::Refused(
            refusal::<D>().to_string(),
        ))))
    }
    fn read_target<'a>(
        _: &'a mut Conn,
        _: &'a [String],
        _: &'a [String],
        _: &'a [String],
        _: &'a [scope::PlannedGrant],
    ) -> Step<'a, Result<(CatalogFacts, scope::Authorization), DbError>> {
        Box::pin(std::future::ready(Err(DbError::Refused(
            refusal::<D>().to_string(),
        ))))
    }
    fn read_target_with_history<'a>(
        _: &'a mut Conn,
        _: &'a [String],
        _: &'a [String],
        _: &'a [String],
        _: &'a [scope::PlannedGrant],
    ) -> Step<'a, Result<TargetRead<Never>, DbError>> {
        Box::pin(std::future::ready(Err(DbError::Refused(
            refusal::<D>().to_string(),
        ))))
    }
    fn recipe(_: &CatalogFacts) -> Result<DatabaseRecipe, String> {
        Err(refusal::<D>().to_string())
    }
    fn compare_reported(_: &CatalogFacts, _: &CatalogFacts) -> Result<ScopeReport, String> {
        Err(refusal::<D>().to_string())
    }
    fn limitation(history: &Never) -> Option<(&'static str, &'static str)> {
        match *history {}
    }
    fn history_differences(before: &Never, _: &Never) -> Result<Vec<String>, DbError> {
        match *before {}
    }
    fn rehearsal_base<'a>(
        scratch: &'a Never,
        _: Provisioning,
        _: &'a mut Never,
        _: &'a mut Conn,
        _: &'a mut Conn,
        _: &'a BindingRequest<'a>,
        _: &'a IdsFile,
        _: &'a IdsFile,
        _: &'a Hints,
        _: &'a Schema,
        _: Option<&'a str>,
    ) -> Step<'a, Result<(), Error>> {
        match *scratch {}
    }
    fn rehearsal_roles<'a>(
        scratch: &'a mut Never,
        _: &'a mut Conn,
        _: &'a BTreeSet<RoleName>,
        _: &'a Schema,
        _: &'a Schema,
        _: &'a Tokens,
    ) -> Step<'a, Result<Roles, Error>> {
        match *scratch {}
    }
}

impl Sealable for Never {
    fn planning_records(&self) -> Result<Vec<BindingRecord>, ManifestError> {
        match *self {}
    }
    fn seal_for_plan(self) -> Result<InputManifest, SealError> {
        match self {}
    }
}

impl BindingView for Never {
    fn has_fill(&self) -> bool {
        match *self {}
    }
}

impl PreparedView for Never {
    fn authorization_condition<'a>(
        &'a mut self,
        _: &'a mut Conn,
    ) -> Step<'a, Result<Option<Box<pbps_mssql::resolver::authorization::Condition>>, Error>> {
        match *self {}
    }
    fn dependency_placements(&self) -> BTreeMap<DependencyName, DependencyPlacement> {
        match *self {}
    }
    fn catalog(&self) -> &CatalogFacts {
        match *self {}
    }
    #[cfg(test)]
    fn catalog_mut(&mut self) -> &mut CatalogFacts {
        match *self {}
    }
    fn into_catalog(self) -> CatalogFacts {
        match self {}
    }
    fn session<'a>(&'a mut self, _: &'a mut Conn) -> &'a mut Conn {
        match *self {}
    }
    fn deployer<'a>(&'a mut self, _: &'a mut Conn) -> &'a mut Conn {
        match *self {}
    }
    fn reader(&self) -> Step<'_, Result<Option<Conn>, Error>> {
        match *self {}
    }
    fn differences(&self) -> &[String] {
        match *self {}
    }
    fn supplied(&self) -> bool {
        match *self {}
    }
    fn has_filled(&self) -> bool {
        match *self {}
    }
    fn unfillable(&self) -> Vec<String> {
        match *self {}
    }
}

/// An operation an engine can lack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Planning,
    Rehearsal,
    Preview,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Planning => "resolver-backed planning",
            Self::Rehearsal => "connected rehearsal",
            Self::Preview => "target-less dev rehearsal",
        })
    }
}

/// The engine has no resolver, or none for this operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsupported {
    pub engine: &'static str,
    pub operation: Operation,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} does not support {}", self.engine, self.operation)
    }
}

impl std::error::Error for Unsupported {}

/// A plan carries resolver evidence the engine cannot recheck. Applying it as
/// an ordinary plan would drop the check the evidence exists for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CannotRecheck {
    pub engine: &'static str,
}

impl fmt::Display for CannotRecheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} cannot recheck this plan's resolver evidence",
            self.engine
        )
    }
}

impl std::error::Error for CannotRecheck {}

// The admission functions take the dialect as a witness: every caller sits in
// a `with_plan_dialect!` arm, where the engine has no name to write. The
// library's own entry points are generic in the dialect and ask by type.

fn admit<D: PlanningDialect, T>(
    fact: impl FnOnce(&Support) -> Option<&'static T>,
    operation: Operation,
) -> Result<&'static T, Unsupported> {
    fact(&<D::Resolver as ResolverEngine>::SUPPORT).ok_or(Unsupported {
        engine: D::NAME,
        operation,
    })
}

pub(crate) fn planning_of<D: PlanningDialect>() -> Result<&'static Planning, Unsupported> {
    admit::<D, _>(|support| support.planning, Operation::Planning)
}

pub(crate) fn rehearsal_of<D: PlanningDialect>() -> Result<&'static Rehearsal, Unsupported> {
    admit::<D, _>(|support| support.rehearsal, Operation::Rehearsal)
}

pub fn planning<D: PlanningDialect>(_: &D) -> Result<&'static Planning, Unsupported> {
    planning_of::<D>()
}

pub fn rehearsal<D: PlanningDialect>(_: &D) -> Result<&'static Rehearsal, Unsupported> {
    rehearsal_of::<D>()
}

pub fn preview<D: PlanningDialect>(_: &D) -> Result<&'static Preview, Unsupported> {
    admit::<D, _>(|support| support.preview, Operation::Preview)
}

/// The recheck of this engine's resolver evidence. A resolved plan on an
/// engine without one is refused here, never applied as an ordinary plan.
pub(crate) fn apply<D: PlanningDialect>(_: &D) -> Result<&'static dyn ApplyRecheck, CannotRecheck> {
    <D::Resolver as ResolverEngine>::SUPPORT
        .apply
        .ok_or(CannotRecheck { engine: D::NAME })
}

/// What one resolved plan's recheck reads, borrowed from the saved plan and
/// the environment. Built only by `apply`, whose guard owns the plan's
/// evidence; no engine can make one.
#[derive(Clone, Copy)]
pub struct Replay<'a> {
    pub(in crate::resolver) evidence: &'a ResolverEvidence,
    pub(in crate::resolver) changes: &'a ChangeSet,
    pub(in crate::resolver) declarations: &'a Dependencies,
    pub(in crate::resolver) key: &'a EnvironmentFingerprintKey,
    pub(in crate::resolver) scope_schemas: &'a [String],
}

/// The declarations after the plan, which prove the ownership of what the
/// closing capture finds, and the schemas the measured scope names.
#[derive(Clone, Copy)]
pub struct ClosingInputs<'a> {
    pub(in crate::resolver) schema: &'a Schema,
    pub(in crate::resolver) ids: &'a IdsFile,
    pub(in crate::resolver) namespaces: &'a BTreeSet<String>,
}

/// Which end of the plan a check reads for: the facts and authorization the
/// plan opens with, or the ones it leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Before,
    After,
}

/// What the object-grant postconditions read: the fingerprint of each
/// answer, and the postconditions found changed.
pub type GrantRecheck = (Vec<String>, Vec<String>);

/// How a read of the target's facts failed.
#[derive(Debug)]
pub enum FactsError {
    /// The read itself failed; nothing is known of the target.
    Unread(String),
    /// Refused before any read, in words that need no wrapper.
    Refused(String),
}

/// The engine half of `resolver::apply`: every read it makes of the target,
/// on the apply's own connection. Object-safe and free of associated state:
/// it takes the serialized evidence and returns strings and manifests the
/// neutral guard compares with the sealed ones. An engine's error is the
/// `Display` of what it raised, so the guard's text for an unreadable input is
/// the text it had before the reads moved here.
pub trait ApplyRecheck: Send + Sync {
    /// The domain the cluster identity is fingerprinted under.
    fn cluster(&self) -> ClusterDomain;
    fn cluster_identity<'a>(&'a self, conn: &'a mut Conn) -> Step<'a, Result<String, String>>;
    /// The schemas whose authorization the sealed scope covers.
    fn scope_schemas(&self, schemas: &[String], extras: &[String]) -> Result<Vec<String>, String>;
    /// The opening manifest as the target reads now.
    fn recheck_opening<'a>(
        &'a self,
        conn: &'a mut Conn,
        replay: Replay<'a>,
    ) -> Step<'a, Result<InputManifest, String>>;
    /// The closing manifest as the plan left the target.
    fn capture_closing<'a>(
        &'a self,
        conn: &'a mut Conn,
        replay: Replay<'a>,
        closing: ClosingInputs<'a>,
    ) -> Step<'a, Result<InputManifest, String>>;
    /// The target's reported facts and its deployer's authorization, read
    /// together as the engine's own promise of coherence allows.
    fn scope_facts<'a>(
        &'a self,
        conn: &'a mut Conn,
        replay: Replay<'a>,
    ) -> Step<'a, Result<(CatalogFacts, crate::resolver::scope::Authorization), FactsError>>;
    /// The separate object-grant postconditions: their fingerprints and the
    /// changes found. An engine that holds none refuses a claim of any.
    fn object_grants<'a>(
        &'a self,
        conn: &'a mut Conn,
        replay: Replay<'a>,
        phase: Phase,
    ) -> Step<'a, Result<GrantRecheck, String>>;
}
