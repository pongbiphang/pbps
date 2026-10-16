//! Order from declared effects (#1776 slice S1b-i, DEC-1954.1).
//!
//! `pbps_model::operation::effects` says what each operation creates,
//! removes, rewrites and requires. This module reads those facts three ways,
//! none of which decides an order:
//!
//! - [`edges`]: the constraints the facts imply, from a resource index and
//!   never a scan over all pairs;
//! - [`verify`]: a replay of operations over symbolic objects, from the
//!   baseline, which refuses a plan the facts contradict;
//! - [`shadow`]: the two run beside today's passes in tests and debug builds,
//!   so the report that decides S3 is measured on the orders that exist.
//!
//! Nothing here changes a plan, a statement or a checksum.

#[cfg(any(test, debug_assertions))]
pub(crate) mod compare;
pub mod edges;
pub mod shadow;
pub mod verify;

pub use edges::{Baseline, Because, Checks, Edge, Graph, Source, Unmet, graph};
pub use shadow::{Report, Shadowed, analyse};
pub use verify::{Violation, verify};

#[cfg(test)]
mod tests;
