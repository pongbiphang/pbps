//! Order from declared effects (#1776 slices S1b-i and S3, DEC-1954.1,
//! DEC-1956.1).
//!
//! `pbps_model::operation::effects` says what each operation creates,
//! removes, rewrites and requires. This module reads those facts four ways:
//!
//! - [`edges`]: the constraints the facts imply, from a resource index and
//!   never a scan over all pairs;
//! - [`schedule`]: Kahn's algorithm over edges, the one primitive that orders
//!   a plan from them (`settle`), and refuses a cycle by name;
//! - [`verify`]: a replay of operations over symbolic objects, from the
//!   baseline, which refuses a plan the facts contradict, and the settle check
//!   that the order given is the one the effect graph settles to;
//! - [`shadow`]: the two run beside every order in tests and debug builds;
//! - [`commute`]: a pair the facts call independent, and a second order that
//!   swaps it, for the live suites that check the facts against an engine.
//!
//! In a release build the effects order the resolver's plan and the connected
//! PostgreSQL handovers of an identity sequence's or detached clone's name
//! (DEC-1986.1, DEC-1558.1), and baseline binding releases (DEC-2228.1). Other
//! callers settle only the edges they state, such as the generated- and
//! computed-column releases (DEC-1956.3). The resolver combines the effects,
//! name slots and its evidence (DEC-1968.2).

pub mod commute;
pub mod edges;
pub mod expand;
pub mod generated;
pub mod new_tables;
pub mod parent_defaults;
mod plan;
mod rewrites;
pub(crate) mod roles;
pub mod row_security;
pub mod schedule;
pub mod shadow;
pub mod slots;
pub mod verify;

pub use edges::{Baseline, Because, Checks, Edge, Graph, Source, Unmet, graph};
pub use plan::{OrderedPlan, PlanView};
pub use rewrites::before_rewrites;
pub use roles::role_drops;
pub use schedule::{Constraint, Refusal, Schedule, Settled};
pub use shadow::{Report, Shadowed, analyse};
pub use verify::{Violation, verify};

#[cfg(test)]
mod benchmark;
#[cfg(test)]
mod expand_tests;
#[cfg(test)]
mod schedule_tests;
#[cfg(test)]
mod slot_tests;
#[cfg(test)]
mod tests;
