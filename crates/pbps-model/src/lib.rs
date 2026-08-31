//! The domain model of `pbps`.
//!
//! # The boundaries of this layer
//!
//! - **Dialect-agnostic**. There is no T-SQL or PostgreSQL knowledge here. Type
//!   strings are parsed *syntactically* only (name + arguments); "is `nvarchar` a
//!   valid type" and "does `int → bigint` count as narrowing" belong to
//!   `pbps-dialect`.
//! - **No spans**. Source locations from load time stay in `pbps-load` and never
//!   enter the model. The reason is that `Schema` must be comparable with `==`
//!   directly (both diff and drift detection rely on it), and carrying spans
//!   would make two semantically identical schemas unequal.
//! - **Serializes to JSON** (the identity file and the `__pbps_state` snapshot),
//!   not YAML. The YAML shape differs from the model — map keys act as names,
//!   fields have defaults — and `pbps-load` owns the conversion between them.
//!
//! # Determinism
//!
//! Every collection is a `BTreeMap` / `BTreeSet` so that serialized output is
//! stable: the identity file is committed to git, and shifting order would
//! manufacture phantom diffs. The one exception is `Table::columns`, which uses
//! `IndexMap` to preserve declaration order because that affects the column
//! layout of `CREATE TABLE`.

pub mod change;
pub mod drift;
pub mod ids;
pub mod intent;
pub mod name;
pub mod plan;
pub mod schema;
pub mod state;
pub mod strategy;
pub mod types;
pub mod uid;

pub use change::{Change, ChangeSet, PlannedChange, RiskClass};
pub use drift::{DriftBaseline, DriftReport};
pub use ids::{IdsFile, Tombstone};
pub use intent::Intent;
pub use name::{ColumnRef, NameError, TableName};
pub use plan::{PlanBaseline, PlanOrigin, SavedPlan, plan_checksum, state_checksum};
pub use schema::{
    CheckConstraint, Column, ForeignKey, Identity, Index, IndexColumn, PrimaryKey,
    ReferentialAction, Schema, Table, UniqueConstraint,
};
pub use state::{StateKind, StateSnapshot};
pub use strategy::{Strategies, Strategy};
pub use types::{ColumnType, TypeArg, TypeParseError};
pub use uid::{Uid, UidError, UidKind};
