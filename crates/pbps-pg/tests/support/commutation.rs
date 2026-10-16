//! The commutation suite on PostgreSQL (#1965, slice S1b-iii of #1776): this
//! engine's sample and its half of the harness. What the suite checks, and
//! why, is in the shared half.

#[path = "../../../pbps-diff/tests/support/commutation.rs"]
mod core;

use core::{Engine, Pulled, SCHEMA, Scenario};
use pbps_dialect::Dialect;
use pbps_model::{Schema, TableName};

use super::{TestDb, connected_base, fresh, pull};

/// The sample: at most `core::CAP` pairs, every kind this engine plans in at
/// least one. PostgreSQL plans no role of its own (ADR-0010 §3), so the role
/// kinds are SQL Server's to cover.
const SCENARIOS: &[Scenario] = &[
    // `AddIndex` and `AddCheck` each have pairs of their own below; the
    // first pair is a foreign key added, which no other pair plans (#1965).
    Scenario {
        name: "a_foreign_key_added_beside_an_index",
        pair: ["AddForeignKey", "AddIndex"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.p
columns:
  id: {type: integer, nullable: false}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {}
    3: {}

---
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.p
columns:
  id: {type: integer, nullable: false}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {}
    3: {}

---
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
foreign_keys:
  fk_a:
    columns: [a]
    references: app.p(id)
indexes:
  ix_b:
    columns: [b]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "two_columns_added_to_one_table",
        pair: ["AddColumn", "AddColumn"],
        gap: Some(2080),
        uid_order: Some(2080),
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
  c: {type: integer}
  d: {type: text}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "an_index_and_a_check_dropped_from_one_table",
        pair: ["DropIndex", "DropCheck"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
checks:
  ck_b: (b > 0)
indexes:
  ix_a:
    columns: [a]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_column_dropped_beside_storage_parameters",
        pair: ["DropColumn", "SetStorageParameters"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
  c: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
storage_parameters:
  fillfactor: 70
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_column_renamed_beside_a_dropped_unique_constraint",
        pair: ["RenameColumn", "DropUnique"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
unique:
  uq_a: [a]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b2: {type: integer, renamed_from: b}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b2: 2}
    2: {a: 3, b2: 4}
"#,
    },
    Scenario {
        name: "a_column_retyped_beside_a_new_column",
        pair: ["AlterColumnType", "AddColumn"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: bigint}
  c: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_column_made_not_null_beside_a_dropped_foreign_key",
        pair: ["AlterColumnNullability", "DropForeignKey"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.p
columns:
  id: {type: integer, nullable: false}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {}
    3: {}

---
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
foreign_keys:
  fk_a:
    columns: [a]
    references: app.p(id)
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.p
columns:
  id: {type: integer, nullable: false}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {}
    3: {}

---
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer, nullable: false}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_default_changed_beside_a_deprecation",
        pair: ["AlterColumnDefault", "SetColumnDeprecated"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer, default: '0'}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer, deprecated: use b}
  b: {type: integer, default: '1'}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "identity_options_beside_a_replica_identity",
        pair: ["AlterColumnNumbering", "SetReplicaIdentity"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
  n: {type: integer, nullable: false, identity: [1, 1]}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
  n: {type: integer, nullable: false, identity: {seed: 1, increment: 2}}
primary_key: [id]
replica_identity: full
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_generation_expression_beside_storage_parameters",
        pair: ["AlterColumnExpression", "SetStorageParameters"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
  g: {type: integer, generated: {expression: (a + 1), stored: true}}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
  g: {type: integer, generated: {expression: (a + 2), stored: true}}
primary_key: [id]
storage_parameters:
  fillfactor: 70
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "persistence_beside_index_storage_parameters",
        pair: ["SetTablePersistence", "SetIndexStorageParameters"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
indexes:
  ix_a:
    columns: [a]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
unlogged: true
indexes:
  ix_a:
    columns: [a]
    storage_parameters:
      fillfactor: 80
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_primary_key_renamed_beside_a_check",
        pair: ["SetPrimaryKey", "AddCheck"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.k
columns:
  id: {type: integer, nullable: false}
  c: {type: integer}
primary_key: {name: pk_k, columns: [id]}
"#,
        desired: r#"
table: app.k
columns:
  id: {type: integer, nullable: false}
  c: {type: integer}
primary_key: {name: pk_k2, columns: [id]}
checks:
  ck_c: (c > 0)
"#,
    },
    Scenario {
        name: "a_row_inserted_beside_an_index",
        pair: ["InsertRow", "AddIndex"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
indexes:
  ix_b:
    columns: [b]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
    3: {a: 5, b: 6}
"#,
    },
    Scenario {
        name: "a_row_updated_beside_a_new_column",
        pair: ["UpdateRow", "AddColumn"],
        gap: Some(2079),
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
  c: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 7, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_row_deleted_beside_an_index",
        pair: ["DeleteRow", "AddIndex"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
indexes:
  ix_a:
    columns: [a]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
"#,
    },
    Scenario {
        name: "a_data_mode_changed_beside_an_index",
        pair: ["SetDataMode", "AddIndex"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
indexes:
  ix_b:
    columns: [b]
data:
  mode: ensure
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "ensure_keys_beside_a_check",
        pair: ["SetEnsureKeys", "AddCheck"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: ensure
  rows:
    1: {a: 1, b: 2}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
checks:
  ck_a: (a > 0)
data:
  mode: ensure
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_table_created_beside_a_view",
        pair: ["CreateTable", "CreateModule"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u
columns:
  id: {type: integer, nullable: false}
primary_key: [id]

---
view: app.v
definition: SELECT t.id, t.a FROM app.t
"#,
    },
    Scenario {
        name: "a_table_dropped_beside_a_view_redefined",
        pair: ["DropTable", "AlterModule"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u
columns:
  id: {type: integer, nullable: false}
primary_key: [id]

---
view: app.v
definition: SELECT t.id, t.a FROM app.t
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
view: app.v
definition: SELECT t.id, t.b FROM app.t
"#,
    },
    Scenario {
        name: "a_table_renamed_beside_a_view_dropped",
        pair: ["RenameTable", "DropModule"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u
columns:
  id: {type: integer, nullable: false}
primary_key: [id]

---
view: app.v
definition: SELECT t.id, t.a FROM app.t
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u2
renamed_from: app.u
columns:
  id: {type: integer, nullable: false}
primary_key: [id]
"#,
    },
    Scenario {
        name: "a_grant_beside_a_revoke",
        pair: ["Grant", "Revoke"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u
columns:
  id: {type: integer, nullable: false}
primary_key: [id]

---
role: {role}
grants:
  app.t: [select]
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u
columns:
  id: {type: integer, nullable: false}
primary_key: [id]

---
role: {role}
grants:
  app.u: [select]
"#,
    },
    Scenario {
        name: "public_execution_beside_an_index",
        pair: ["PublicExecution", "AddIndex"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
indexes:
  ix_a:
    columns: [a]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
function: app.f()
definition: |-
  () RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$
"#,
    },
    Scenario {
        name: "a_partition_attached_beside_a_check",
        pair: ["AttachPartition", "AddCheck"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.m
columns:
  k: {type: integer, nullable: false}
  v: {type: integer}
partition_by: [k]
partitions:
  m_p1: {from: [0], to: [10]}

---
table: app.m_p2
columns:
  k: {type: integer, nullable: false}
  v: {type: integer}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
checks:
  ck_a: (a > 0)
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.m
columns:
  k: {type: integer, nullable: false}
  v: {type: integer}
partition_by: [k]
partitions:
  m_p1: {from: [0], to: [10]}
  m_p2: {from: [10], to: [20]}
"#,
    },
    Scenario {
        name: "a_partition_detached_beside_an_index",
        pair: ["DetachPartition", "AddIndex"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.m
columns:
  k: {type: integer, nullable: false}
  v: {type: integer}
partition_by: [k]
partitions:
  m_p1: {from: [0], to: [10]}
  m_p2: {from: [10], to: [20]}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
indexes:
  ix_a:
    columns: [a]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.m
columns:
  k: {type: integer, nullable: false}
  v: {type: integer}
partition_by: [k]
partitions:
  m_p1: {from: [0], to: [10]}

---
table: app.m_p2
columns:
  k: {type: integer, nullable: false}
  v: {type: integer}
"#,
    },
    Scenario {
        name: "a_partition_default_beside_a_partition_not_null",
        pair: ["SetPartitionDefault", "SetPartitionNotNull"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.m
columns:
  k: {type: integer, nullable: false}
  v: {type: integer}
partition_by: [k]
partitions:
  m_p1: {from: [0], to: [10]}
  m_p2: {from: [10], to: [20]}
"#,
        desired: r#"
table: app.m
columns:
  k: {type: integer, nullable: false}
  v: {type: integer}
partition_by: [k]
partitions:
  m_p1: {from: [0], to: [10], columns: {v: {default: '5'}}}
  m_p2: {from: [10], to: [20], columns: {v: {nullable: false}}}
"#,
    },
    Scenario {
        name: "a_column_added_beside_a_row_insert",
        pair: ["AddColumn", "InsertRow"],
        gap: Some(2079),
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
  c: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
    3: {a: 5, b: 6}
"#,
    },
    Scenario {
        name: "an_index_dropped_beside_a_row_update",
        pair: ["DropIndex", "UpdateRow"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
indexes:
  ix_a:
    columns: [a]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 9}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_view_created_beside_a_column_added",
        pair: ["CreateModule", "AddColumn"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
  c: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
view: app.v
definition: SELECT t.id, t.a FROM app.t
"#,
    },
    Scenario {
        name: "a_unique_constraint_dropped_beside_another_added",
        pair: ["DropUnique", "AddUnique"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
unique:
  uq_a: [a]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
unique:
  uq_b: [b]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_grant_beside_a_column_added",
        pair: ["Grant", "AddColumn"],
        gap: None,
        uid_order: None,
        base: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: integer, nullable: false}
  a: {type: integer}
  b: {type: integer}
  c: {type: integer}
primary_key: [id]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
role: {role}
grants:
  app.t: [select]
"#,
    },
];

struct Pg(pbps_pg::Postgres);

impl Engine for Pg {
    type Db = TestDb;

    fn name(&self) -> &'static str {
        "pg"
    }

    fn dialect(&self) -> &dyn Dialect {
        &self.0
    }

    async fn database(&self, tag: &str) -> TestDb {
        let mut db = TestDb::create(tag).await;
        fresh(&mut db.conn, SCHEMA).await;
        db
    }

    async fn drop(&self, db: TestDb) {
        db.drop().await;
    }

    async fn execute(&self, db: &mut TestDb, sql: &str) -> Result<(), String> {
        db.conn
            .execute(sql)
            .await
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    }

    async fn texts(&self, db: &mut TestDb, sql: &str) -> Result<Vec<String>, String> {
        let rows = db
            .conn
            .query(sql)
            .await
            .map_err(|e| format!("{e:?} in {sql}"))?;
        Ok(rows
            .iter()
            .map(|r| {
                r.try_get_at::<&str>(0)
                    .expect("a text column")
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect())
    }

    async fn pull(&self, db: &mut TestDb, desired: &Schema) -> Result<Pulled, String> {
        let pulled = pull(db).await;
        let mut catalog = Schema::default();
        for (name, table) in &pulled.schema.tables {
            if name.schema == SCHEMA {
                catalog.tables.insert(name.clone(), table.clone());
            }
        }
        for (id, module) in &pulled.schema.modules {
            if id.schema() == SCHEMA {
                catalog.modules.insert(id.clone(), module.clone());
            }
        }
        // A role is the cluster's: only what this schema declares is ours.
        for (name, role) in &pulled.schema.roles {
            if desired.roles.contains_key(name) {
                catalog.roles.insert(name.clone(), role.clone());
            }
        }
        let mut base = connected_base(db, desired, SCHEMA).await;
        base.roles = catalog.roles.clone();
        Ok(Pulled { catalog, base })
    }

    fn access_sql(&self) -> String {
        format!(
            "SELECT c.relname || ' ' || c.relkind::text || ' ' || pg_get_userbyid(c.relowner) \
                    || ' ' || coalesce(c.relacl::text, '-') \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = '{SCHEMA}' \
             UNION ALL \
             SELECT c.relname || '.' || a.attname || ' ' || coalesce(a.attacl::text, '-') \
             FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = '{SCHEMA}' AND a.attnum > 0 AND NOT a.attisdropped \
               AND a.attacl IS NOT NULL \
             UNION ALL \
             SELECT p.oid::regprocedure::text || ' ' || pg_get_userbyid(p.proowner) \
                    || ' ' || coalesce(p.proacl::text, '-') \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = '{SCHEMA}' \
             UNION ALL \
             SELECT n.nspname || ' ' || pg_get_userbyid(n.nspowner) \
                    || ' ' || coalesce(n.nspacl::text, '-') \
             FROM pg_namespace n WHERE n.nspname = '{SCHEMA}' \
             ORDER BY 1"
        )
    }

    fn rows_sql(&self, table: &TableName) -> String {
        // `jsonb` sorts its keys, so the text is the row's and not the order
        // its columns were added in: that is the column check's.
        format!(
            "SELECT to_jsonb(t)::text FROM \"{}\".\"{}\" t ORDER BY 1",
            table.schema, table.name
        )
    }
}

/// A role of this run's own, created on the cluster for the grant scenarios
/// and dropped after them: PostgreSQL's roles are the cluster's.
async fn with_role<F: std::future::Future<Output = ()>>(run: impl FnOnce(String) -> F) {
    let role = format!("pbps_commute_{}", std::process::id());
    let mut admin = super::connect().await;
    let _ = admin.execute(&format!("DROP ROLE IF EXISTS {role}")).await;
    admin
        .execute(&format!("CREATE ROLE {role}"))
        .await
        .expect("the scenarios' role");
    run(role.clone()).await;
    let _ = admin.execute(&format!("DROP ROLE IF EXISTS {role}")).await;
}

#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
async fn pairs_the_effects_call_independent_commute_on_the_engine() {
    let pg = Pg(pbps_pg::Postgres::new());
    with_role(|role| async move { core::suite(&pg, SCENARIOS, &role).await }).await;
}
