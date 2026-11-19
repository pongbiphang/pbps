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
use crate::resolver::server::engine::{CapturedInputs, CompiledCapture, Naming};
use crate::resolver::server::scratch::{Provenance, Provisioning, Tokens};
use crate::resolver::server::{BindingRequest, Error, PlanningDialect, ProduceError, ScopeRequest};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::capture::Assessment;
use pbps_db::resolver::environment::CatalogFacts;
use pbps_db::{Conn, DbError};
use pbps_model::dependency::Dependencies;
use pbps_model::dependency::DependencyName;
use pbps_model::resolver::{DependencyPlacement, OutsideWrites};
use pbps_model::resolver::{InputManifest, ResolverEvidence};
use pbps_model::{ChangeSet, IdsFile, ModuleDeps, RoleName, Schema, SessionZone};
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

/// Facts the sealed evidence names. The producer still holds its own copy of
/// the other facts until it moves; only the cluster is read from here.
pub struct Planning {
    pub cluster: ClusterDomain,
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
    ) -> Step<'a, Result<CompiledCapture, Error>>;
    /// The opening capture of the target, as the binding reads it. The
    /// capture states are still the two-engine unions, so a capture of the
    /// other engine is a named error until they are typed.
    fn capture_opening<'a>(
        binding: &'a Self::Binding,
        target: &'a mut Conn,
        closing: &'a CompiledCapture,
        key: &'a EnvironmentFingerprintKey,
        base: pbps_diff::Side<'a>,
    ) -> Step<'a, Result<(CapturedInputs, InputManifest), Error>>;
    fn assess(
        binding: &mut Self::Binding,
        closing: &CompiledCapture,
        current: &CapturedInputs,
        staged: &Self::Staged,
    ) -> Result<Assessment, Error>;
    /// The shapes the externals file was compared with that the opening
    /// capture finds changed.
    fn changed_since(staged: &Self::Staged, current: &CapturedInputs)
    -> Result<Vec<String>, Error>;
    /// A failure of the scratch compile, as the staged shapes explain it.
    fn through_shape_view(staged: &Self::Staged, error: Error) -> Error;
    /// The planning dialect the binding was generated under, from the same
    /// measured capability reconstruction used.
    fn dialect(binding: &Self::Binding, extras: &[String]) -> Self::Dialect;
    /// How a rehearsal names the roles a plan names on this scratch (see
    /// `Naming`). SQL Server has no cluster role to name: its roles are the
    /// scratch database's own and a rehearsal asks
    /// `scratch::rehearse::principals` instead (DEC-2379.3).
    fn naming(scratch: &Self::Scratch<'_>) -> Result<Naming, Error>;
    /// Makes the roles a rehearsal names, by `naming`. `Err` inside is why the
    /// scratch cannot hold them, and says so.
    fn rehearsal_roles<'a>(
        scratch: &'a mut Self::Scratch<'_>,
        admin: &'a mut Conn,
        roles: &'a [RoleName],
        naming: Naming,
    ) -> Step<'a, Result<Result<(), String>, Error>>;
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
    type Prepared = Never;
    type Binding = Never;
    type Staged = Never;

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
    ) -> Step<'a, Result<CompiledCapture, Error>> {
        match *prepared {}
    }
    fn capture_opening<'a>(
        binding: &'a Never,
        _: &'a mut Conn,
        _: &'a CompiledCapture,
        _: &'a EnvironmentFingerprintKey,
        _: pbps_diff::Side<'a>,
    ) -> Step<'a, Result<(CapturedInputs, InputManifest), Error>> {
        match *binding {}
    }
    fn assess(
        binding: &mut Never,
        _: &CompiledCapture,
        _: &CapturedInputs,
        _: &Never,
    ) -> Result<Assessment, Error> {
        match *binding {}
    }
    fn changed_since(staged: &Never, _: &CapturedInputs) -> Result<Vec<String>, Error> {
        match *staged {}
    }
    fn through_shape_view(staged: &Never, _: Error) -> Error {
        match *staged {}
    }
    fn dialect(binding: &Never, _: &[String]) -> D {
        match *binding {}
    }
    fn naming(scratch: &Never) -> Result<Naming, Error> {
        match *scratch {}
    }
    fn rehearsal_roles<'a>(
        scratch: &'a mut Never,
        _: &'a mut Conn,
        _: &'a [RoleName],
        _: Naming,
    ) -> Step<'a, Result<Result<(), String>, Error>> {
        match *scratch {}
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
