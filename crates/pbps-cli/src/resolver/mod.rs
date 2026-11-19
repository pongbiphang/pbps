//! The resolver: scratch compilation of the managed declarations, separate
//! from environment and binding qualification (ADR-0016, DEC-1528.1).
//!
//! It reaches its scratch with SQL alone, so nothing here is gated to one
//! platform: a server scratch is a connection string, and a Docker scratch
//! is the `docker` command the runner has, on any platform it runs on.

pub mod apply;

mod contract;
pub use contract::{
    CannotRecheck, NoResolver, Operation, Unsupported, planning, preview, rehearsal,
};

pub mod scope;

pub mod server;

pub mod sealing;

#[cfg(test)]
pub(crate) mod generated_fixtures;

#[cfg(test)]
mod contract_tests;
