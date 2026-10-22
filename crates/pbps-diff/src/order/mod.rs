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

#![cfg_attr(
    not(feature = "test-fixtures"),
    doc = r#"
The emitter fixture constructor is absent from an ordinary external build:

```compile_fail
let _ = pbps_diff::order::singleton(
    pbps_model::PlannedChange::new(pbps_model::Change::CreateRole {
        uid: "r_aaaaaa".parse().unwrap(),
        name: "reader".into(),
    }),
    pbps_model::PlanMode::Transactional,
    &pbps_dialect::MinimalDialect,
);
```
"#
)]

pub mod commute;
mod companions;
pub mod edges;
pub mod expand;
pub mod generated;
pub mod new_tables;
pub(crate) mod normalization;
pub mod parent_defaults;
mod plan;
pub(crate) mod rename_order;
pub(crate) mod resolver_finish;
pub(crate) mod resolver_prepare;
pub mod resolver_remap;
mod rewrites;
pub(crate) mod roles;
pub mod row_security;
mod saved;
pub mod schedule;
pub mod scratch;
pub mod shadow;
pub mod slots;
pub mod verify;
pub mod writebacks;

pub use edges::{Baseline, Because, Checks, Edge, Graph, Source, Unmet, graph};
#[cfg(any(test, feature = "test-fixtures"))]
pub use plan::singleton;
pub use plan::{OrderedPlan, PlanView, PlanningPlan, propose, take};
pub use rewrites::before_rewrites;
pub use roles::role_drops;
pub use saved::{AdmissionError, AdmittedPlan, SealedPlan, admit, seal, validate_saved_plan};
pub use schedule::{Constraint, Refusal, Schedule, Settled};
pub use shadow::{Report, Shadowed, analyse};
pub use verify::{Violation, verify};

#[cfg(test)]
mod benchmark;
#[cfg(test)]
mod expand_tests;
#[cfg(test)]
mod payload_tests;
#[cfg(test)]
mod saved_tests;
#[cfg(test)]
mod schedule_tests;
#[cfg(test)]
mod slot_tests;
#[cfg(test)]
mod tests;

pub use companions::{
    PublicDecisions, settle_public_execution, split_moving_defaults, take_public_execution,
};
pub use normalization::{drop_parameter_changes_of_rebuilt_indexes, normalize};

pub use resolver_finish::reordered_operations;
