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

use crate::resolver::server::PlanningDialect;
use pbps_db::Conn;
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::environment::CatalogFacts;
use pbps_model::dependency::Dependencies;
use pbps_model::resolver::{InputManifest, ResolverEvidence};
use pbps_model::{ChangeSet, IdsFile, Schema};
use std::collections::BTreeSet;
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

/// Connected rehearsal: the same scratch, nothing sealed. The policy it holds
/// today as engine comparisons moves here with the rehearsal.
pub struct Rehearsal;

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
pub trait ResolverEngine: sealed::Sealed + Sized + Send + Sync + 'static {
    /// The dialect this resolver belongs to, one to one: brand and state are
    /// the same engine by construction, not by a runtime comparison.
    type Dialect: PlanningDialect<Resolver = Self>;
    const SUPPORT: Support;
}

/// The resolver of an engine that has none.
pub struct NoResolver<D>(PhantomData<fn() -> D>);

impl<D> sealed::Sealed for NoResolver<D> {}

impl<D: PlanningDialect<Resolver = Self>> ResolverEngine for NoResolver<D> {
    type Dialect = D;
    const SUPPORT: Support = Support::NONE;
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
// a `with_plan_dialect!` arm, where the engine has no name to write.

fn admit<D: PlanningDialect, T>(
    _: &D,
    fact: impl FnOnce(&Support) -> Option<&'static T>,
    operation: Operation,
) -> Result<&'static T, Unsupported> {
    fact(&<D::Resolver as ResolverEngine>::SUPPORT).ok_or(Unsupported {
        engine: D::NAME,
        operation,
    })
}

pub fn planning<D: PlanningDialect>(dialect: &D) -> Result<&'static Planning, Unsupported> {
    admit(dialect, |support| support.planning, Operation::Planning)
}

pub fn rehearsal<D: PlanningDialect>(dialect: &D) -> Result<&'static Rehearsal, Unsupported> {
    admit(dialect, |support| support.rehearsal, Operation::Rehearsal)
}

pub fn preview<D: PlanningDialect>(dialect: &D) -> Result<&'static Preview, Unsupported> {
    admit(dialect, |support| support.preview, Operation::Preview)
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
