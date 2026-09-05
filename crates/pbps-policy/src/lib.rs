//! The `policies:` block and the built-in analyzer catalogue
//! ([ADR-0008](../../../docs/ADR-0008-policies.md)).
//!
//! # What this is
//!
//! Built-in validation says what the engine will refuse and what the model
//! cannot express. This says what *the project* refuses: a naming convention,
//! a reference table grown past what a lookup table should be, a revision
//! that adds and drops in one table. Each rule is a closed, named thing with
//! a default severity; the project raises or lowers it, switches it off, or
//! suppresses it on one object with a reason — and never writes code.
//!
//! # Two evaluation points, one rule set
//!
//! Some rules read the declarations and some read the plan, so `validate`
//! runs the first kind and `plan` the second, through the same block and the
//! same ids. A rule says which point it belongs to; the block does not have to
//! know. Every plan rule works on the typed `ChangeSet` — nothing here ever
//! looks at emitted SQL (constraint 3).
//!
//! # What this is not
//!
//! Not a gate. An `error` finding refuses to *produce* a plan; `apply` runs a
//! checksum-pinned plan that already passed the gate and is never touched by a
//! policy. And not a plugin engine: parameters are data (a pattern, a number,
//! a window), because a parameter that could run something would be the
//! second execution engine SPEC §14.3 refuses.

pub mod civil;
pub mod config;
pub mod evaluate;
pub mod rules;

pub use config::{Policies, RuleConfig, Suppression};
pub use evaluate::{Context, declarations, plan};
pub use pbps_model::{Finding, Severity};
pub use rules::{Point, RULES, Rule, rule};
