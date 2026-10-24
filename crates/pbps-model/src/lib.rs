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
//! layout of `CREATE TABLE` — an order equality ignores, and which
//! [`state_checksum`] therefore sorts away before hashing (DECISIONS 238).

pub mod baseline_evidence;
pub mod change;
mod column_wire;
pub mod data;
pub mod declared;
pub mod dependency;
pub mod diagnostic;
pub mod drift;
pub mod escape;
pub mod finding;
pub mod ids;
pub mod intent;
pub mod managed_type;
pub mod module;
pub mod name;
pub mod operation;
pub mod part_effects;
pub mod partitions;
pub mod physical_sources;
pub mod plan;
pub mod plan_names;
pub mod reason;
pub mod rebuild;
pub mod rebuild_ops;
pub mod rename;
pub mod resolver;
pub mod role;
pub mod schema;
pub mod state;
pub mod storage;
pub mod strategy;
pub mod text;
pub mod types;
pub mod uid;
pub mod zone;

pub use change::{
    AddedRole, CellAfter, Change, ChangeSet, ColumnField, ColumnPromise, DetachedKind,
    DetachedName, Dropped, IndexPart, ModuleAfter, NumberingKind, Part, PartAfter, PartChange,
    PartDefinition, PermissionChange, PlannedChange, Presence, Provenance, PublicAccess, RiskClass,
    RoutineOrigin, RowAfter, RowSecurityOrigin,
};
pub use data::{
    Cell, DataMode, DataScope, DataScopes, ObservedRow, ObservedRows, ObservedTable, Row,
    RowConflict, RowKey, RowScope, TableData, Value,
};
pub use declared::{Binding, Bindings, Declared, DeclaredExpressions, DeclaredPolicyExpressions};
pub use drift::{DriftBaseline, DriftReport};
pub use finding::{Finding, Severity};
pub use ids::{Conversion, IdsFile, IdsIndex, Move, Tombstone, ValidIds};
pub use intent::Intent;
pub use managed_type::{EnumPlacement, ManagedType};
pub use module::{
    Hints, Module, ModuleDeps, ModuleId, ModuleIdError, ModuleKind, ObjectName, PublicExecute,
    RoutineArg, RoutineArgError, RoutineId,
};
pub use name::{
    ColumnName, ColumnRef, NameError, ObjectLocalName, RoleName, TableName, check_segment,
};
pub use partitions::Partitions;
pub use plan::{
    DeclaredTypeBefore, DeclaredTypePin, PlanBaseline, PlanMode, PlanOrigin, RoutinePin,
    RoutinePins, SavedPlan, TypePin, TypeRoutine, state_checksum, wire_fingerprint,
};
pub use reason::{Reason, ReasonError};
pub use rebuild::{Rebuild, RebuildIdentities};
pub use rebuild_ops::{
    AnalyzeTableTree, BuildPartitionedIndex, CopyColumn, CopySource, CopyTableRows,
    IntermediateAddress, PartitionIndexClone, PartitionIndexDefinition, RebuildPartitionedTable,
    RebuildRelation,
};
pub use rename::Renames;
pub use role::{GrantTarget, Permission, Role};
pub use schema::{
    BoundDatum, CheckConstraint, Clustered, Collation, Column, ComputedColumn, ForeignKey,
    Generated, Identity, Index, IndexColumn, IndexKey, IndexMethod, ListDatum, OwnedSequence,
    PartitionBound, PartitionBy, PartitionColumn, PartitionOf, PartitionStrategy, PolicyCommand,
    PolicyRoles, PrimaryKey, ReferentialAction, ReplicaIdentity, Retention, RetentionUnit,
    RowPolicy, RowSecurity, Schema, SequenceOptions, SystemTime, SystemVersioning, Table,
    UniqueConstraint, ValueSource,
};
pub use state::{StagedProgress, StateKind, StateSnapshot, Unreadable, check_readable_version};
pub use strategy::{Strategies, Strategy};
pub use text::Text;
pub use types::{CollationName, ColumnType, TypeArg, TypeParseError, TypeRef};
pub use uid::{Uid, UidError, UidKind};
pub use zone::{SessionZone, ZoneError};
