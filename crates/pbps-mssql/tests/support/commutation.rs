//! The commutation suite on SQL Server (#1965, slice S1b-iii of #1776): this
//! engine's sample and its half of the harness. What the suite checks, and
//! why, is in the shared half.

#[path = "../../../pbps-diff/tests/support/commutation.rs"]
mod core;

use core::{Engine, Pulled, SCHEMA, Scenario};
use pbps_model::{Schema, TableName};

use super::TestDb;

/// The sample: at most `core::CAP` pairs, every kind this engine plans in at
/// least one. A database role is the database's here, so each database holds
/// its own and the role kinds are this engine's to cover.
const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "an_index_and_a_check_on_one_table",
        pair: ["AddIndex", "AddCheck"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
checks:
  ck_b: '[b]>(0)'
indexes:
  ix_a:
    columns: [a]
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
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
checks:
  ck_b: '[b]>(0)'
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
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_column_dropped_beside_an_index",
        pair: ["DropColumn", "AddIndex"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
  c: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
indexes:
  ix_a:
    columns: [a]
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
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
  id: {type: int, nullable: false}
  a: {type: int}
  b2: {type: int, renamed_from: b}
primary_key: {name: pk_t, columns: [id]}
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
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: bigint}
  c: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
        base: r#"
table: app.p
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_p, columns: [id]}
data:
  mode: exact
  rows:
    1: {}
    3: {}

---
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
  id: {type: int, nullable: false}
primary_key: {name: pk_p, columns: [id]}
data:
  mode: exact
  rows:
    1: {}
    3: {}

---
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int, nullable: false}
primary_key: {name: pk_t, columns: [id]}
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
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int, default: '0'}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int, deprecated: use b}
  b: {type: int, default: '1'}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_computed_column_added_beside_an_index",
        pair: ["AddComputedColumn", "AddIndex"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
computed:
  g: {expression: '[a]+(1)'}
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
        name: "a_computed_column_dropped_beside_a_check",
        pair: ["DropComputedColumn", "AddCheck"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
computed:
  g: {expression: '[a]+(1)'}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
checks:
  ck_b: '[b]>(0)'
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
        base: r#"
table: app.k
columns:
  id: {type: int, nullable: false}
  c: {type: int}
primary_key: {name: pk_k, columns: [id]}
"#,
        desired: r#"
table: app.k
columns:
  id: {type: int, nullable: false}
  c: {type: int}
primary_key: {name: pk_k2, columns: [id]}
checks:
  ck_c: '[c]>(0)'
"#,
    },
    Scenario {
        name: "a_row_inserted_beside_an_index",
        pair: ["InsertRow", "AddIndex"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
        name: "a_row_deleted_beside_an_index",
        pair: ["DeleteRow", "AddIndex"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: ensure
  rows:
    1: {a: 1, b: 2}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
checks:
  ck_a: '[a]>(0)'
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
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_u, columns: [id]}

---
view: app.v
definition: SELECT t.id, t.a FROM app.t
"#,
    },
    Scenario {
        name: "a_table_dropped_beside_a_view_redefined",
        pair: ["DropTable", "AlterModule"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_u, columns: [id]}

---
view: app.v
definition: SELECT t.id, t.a FROM app.t
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_u, columns: [id]}

---
view: app.v
definition: SELECT t.id, t.a FROM app.t
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u2
renamed_from: app.u
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_u, columns: [id]}
"#,
    },
    Scenario {
        name: "a_grant_beside_a_revoke",
        pair: ["Grant", "Revoke"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_u, columns: [id]}

---
role: {role}
grants:
  app.t: [select]
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_u, columns: [id]}

---
role: {role}
grants:
  app.u: [select]
"#,
    },
    Scenario {
        name: "a_role_created_beside_a_table",
        pair: ["CreateRole", "CreateTable"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
table: app.u
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_u, columns: [id]}

---
role: {role}
"#,
    },
    Scenario {
        name: "a_role_dropped_beside_an_index",
        pair: ["DropRole", "AddIndex"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
role: {role}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
indexes:
  ix_a:
    columns: [a]
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
    Scenario {
        name: "a_role_renamed_beside_a_check",
        pair: ["RenameRole", "AddCheck"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
role: {role}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
checks:
  ck_a: '[a]>(0)'
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}

---
role: {role}_2
renamed_from: {role}
"#,
    },
    Scenario {
        name: "a_foreign_key_added_beside_an_index",
        pair: ["AddForeignKey", "AddIndex"],
        gap: None,
        base: r#"
table: app.p
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_p, columns: [id]}
data:
  mode: exact
  rows:
    1: {}
    3: {}

---
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.p
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_p, columns: [id]}
data:
  mode: exact
  rows:
    1: {}
    3: {}

---
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
        name: "a_unique_constraint_dropped_beside_another_added",
        pair: ["DropUnique", "AddUnique"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
        name: "an_index_dropped_beside_a_row_update",
        pair: ["DropIndex", "UpdateRow"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
  c: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
        name: "a_grant_beside_a_column_added",
        pair: ["Grant", "AddColumn"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
  c: {type: int}
primary_key: {name: pk_t, columns: [id]}
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
    Scenario {
        name: "a_check_dropped_beside_a_new_column",
        pair: ["DropCheck", "AddColumn"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
primary_key: {name: pk_t, columns: [id]}
checks:
  ck_b: '[b]>(0)'
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  b: {type: int}
  c: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1, b: 2}
    2: {a: 3, b: 4}
"#,
    },
];

/// The audit's plans that hold a dependency an effect states which only
/// this engine plans (#2091): a module edited in place, and a computed
/// column. PostgreSQL's list holds the rest; what neither engine can show is
/// said there. A bound caller requires its callee's final definition, so
/// removing the callee's rewrite lets the audit run the order SQL Server
/// refuses (DEC-2192.1).
const HELD: &[Scenario] = &[
    Scenario {
        name: "held_a_function_edited_before_a_bound_caller",
        pair: ["AlterModule", "CreateModule"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_t, columns: [id]}

---
function: app.f(int)
definition: |-
  (@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @a END
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_t, columns: [id]}

---
function: app.f(int)
definition: |-
  (@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @a + 1 END

---
function: app.g(int)
definition: |-
  (@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN app.f(@a) END
"#,
    },
    Scenario {
        name: "held_a_computed_column_dropped_with_its_index",
        pair: ["DropIndex", "DropComputedColumn"],
        gap: None,
        base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
primary_key: {name: pk_t, columns: [id]}
computed:
  g: {expression: '[a]+(1)'}
indexes:
  ix_g:
    columns: [g]
"#,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
primary_key: {name: pk_t, columns: [id]}
"#,
    },
];

struct Ms;

impl Engine for Ms {
    type Db = TestDb;

    fn name(&self) -> &'static str {
        "mssql"
    }

    fn dialect(&self) -> &dyn crate::certified_fixture::FixtureEmission {
        &pbps_mssql::Mssql
    }

    async fn database(&self, tag: &str) -> TestDb {
        let mut db = TestDb::create(tag).await;
        db.conn
            .execute(&format!("CREATE SCHEMA [{SCHEMA}];"))
            .await
            .expect("the scenarios' schema");
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
        let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
            .await
            .map_err(|e| format!("pull: {e:?}"))?;
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
        catalog.roles = pulled.schema.roles.clone();
        let scopes = desired.data_scopes();
        let rows = pbps_mssql::catalog::read_rows(
            &mut db.conn,
            &catalog,
            &pbps_model::data::read_scopes(&Default::default(), &scopes),
        )
        .await
        .map_err(|e| format!("read the rows: {e:?}"))?;
        let mut base = pbps_model::data::plan_base(&catalog, &rows, &Default::default(), desired)
            .map_err(|e| format!("the rows as a base: {e:?}"))?;
        base.modules = catalog.modules.clone();
        base.roles = catalog.roles.clone();
        Ok(Pulled { catalog, base })
    }

    fn access_sql(&self) -> String {
        format!(
            "SELECT CONCAT(o.name, ' ', o.type, ' ', \
                    COALESCE(USER_NAME(o.principal_id), USER_NAME(s.principal_id))) \
                    COLLATE Latin1_General_BIN2 \
             FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = N'{SCHEMA}' AND o.is_ms_shipped = 0 \
             UNION ALL \
             SELECT CONCAT(OBJECT_NAME(p.major_id), ' ', p.minor_id, ' ', p.permission_name, \
                    ' ', p.state_desc, ' ', USER_NAME(p.grantee_principal_id)) \
                    COLLATE Latin1_General_BIN2 \
             FROM sys.database_permissions p \
             WHERE p.class = 1 AND OBJECT_SCHEMA_NAME(p.major_id) = N'{SCHEMA}' \
             UNION ALL \
             SELECT CONCAT(SCHEMA_NAME(p.major_id), ' ', p.permission_name, ' ', p.state_desc, \
                    ' ', USER_NAME(p.grantee_principal_id)) \
                    COLLATE Latin1_General_BIN2 \
             FROM sys.database_permissions p \
             WHERE p.class = 3 AND SCHEMA_NAME(p.major_id) = N'{SCHEMA}' \
             UNION ALL \
             SELECT CONCAT('role ', r.name, ' ', USER_NAME(r.owning_principal_id)) \
                    COLLATE Latin1_General_BIN2 \
             FROM sys.database_principals r \
             WHERE r.type = 'R' AND r.is_fixed_role = 0 AND r.name <> N'public' \
             ORDER BY 1"
        )
    }

    fn rows_sql(&self, table: &TableName) -> String {
        format!(
            "SELECT (SELECT t.* FOR JSON PATH, WITHOUT_ARRAY_WRAPPER, INCLUDE_NULL_VALUES) AS r \
             FROM [{}].[{}] t ORDER BY r",
            table.schema, table.name
        )
    }
}

#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB"]
async fn pairs_the_effects_call_independent_commute_on_the_engine() {
    core::suite(&Ms, SCENARIOS, HELD, "pbps_commute").await;
}

/// A binding must follow the callee's edit: the engine distinguishes the
/// forced order, while an ordinary caller is an independent control.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB"]
async fn schema_bound_callers_block_reversed_edits_and_plain_callers_commute() {
    let bound = &HELD[0];
    match core::check(&Ms, bound, "pbps_commute").await.pair {
        Err(core::Finding::Stale(_)) => {}
        other => panic!("the effects must order the bound caller: {other:?}"),
    }
    let (swaps, failures) = core::audit(&Ms, bound, "pbps_commute").await;
    assert_eq!(swaps, 0, "a bound caller is not an independent pair");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    match core::forced(&Ms, bound, "pbps_commute").await {
        Err(core::Finding::Differs(error)) => {
            assert!(error.contains("3729") && error.contains("g"), "{error}");
        }
        other => panic!("the engine must refuse the callee's late edit: {other:?}"),
    }
    let plain = Scenario {
        name: "an_unbound_caller_and_its_callees_edit",
        pair: bound.pair,
        gap: None,
        base: bound.base,
        desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
primary_key: {name: pk_t, columns: [id]}

---
function: app.f(int)
definition: |-
  (@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @a + 1 END

---
function: app.g(int)
definition: |-
  (@a int) RETURNS int /* WITH SCHEMABINDING */ AS
  BEGIN
    DECLARE @note nvarchar(100) = N'WITH SCHEMABINDING';
    RETURN app.f(@a)
  END
"#,
    };
    let checked = core::check(&Ms, &plain, "pbps_commute").await;
    assert!(checked.pair.is_ok(), "{:?}", checked.pair);
    assert!(checked.reminted.is_none(), "{:?}", checked.reminted);
}

/// A column added with a default the insert's postcondition cannot hold it
/// to, and a row the plan inserts leaving the column to it: before the column
/// exists the row is inserted without it, and SQL Server adds a nullable
/// column with a default without filling the rows already there, so the row
/// ends `NULL`; after, it takes the default. Both orders run, and only the
/// rows tell them apart.
const NEGATIVE: Scenario = Scenario {
    name: "a_row_filled_by_the_default_of_a_column_added",
    pair: ["AddColumn", "InsertRow"],
    gap: None,
    base: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1}
"#,
    desired: r#"
table: app.t
columns:
  id: {type: int, nullable: false}
  a: {type: int}
  c: {type: int, default: abs((5))}
primary_key: {name: pk_t, columns: [id]}
data:
  mode: exact
  rows:
    1: {a: 1}
    3: {a: 5}
"#,
};

/// The equivalence negative (#1965): two orders of a dependent pair, a column
/// added before and after a row write that fills it, fail the row check.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB"]
async fn a_row_written_before_and_after_the_column_it_fills_fails_the_row_check() {
    // Dependent by the effects: the insert fills the column's default.
    match core::check(&Ms, &NEGATIVE, "pbps_commute").await.pair {
        Err(core::Finding::Stale(_)) => {}
        other => panic!("the effects do not order the pair: {other:?}"),
    }
    match core::forced(&Ms, &NEGATIVE, "pbps_commute").await {
        Err(core::Finding::Differs(e)) => assert!(e.contains("the rows differ"), "{e}"),
        other => panic!("the row check did not tell the orders apart: {other:?}"),
    }
}
