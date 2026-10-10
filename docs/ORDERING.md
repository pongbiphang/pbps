# How a plan's order is decided

An audit of every pair of `Change` kinds that can meet in one plan, by how the
order between them is decided (#1351). It exists because a fixed position per
kind of change cannot express a direction that depends on content. Found one
review round at a time, those combinations do not run out: #1285 took about
twenty rounds. The set of change kinds is finite, so the question can be
answered once for all of them.

Read this before adding a `Change` variant, a sort class or a reordering pass.
A new variant needs a row in [the table below](#what-each-change-requires);
`every_change_kind_has_a_row_in_the_ordering_audit` refuses one that has none.

## The mechanism

The differ sorts every planned change by, in order:

1. **Class**, `order_key` in `crates/pbps-diff/src/schema_diff.rs`:

   | Class | Changes |
   |---|---|
   | 0 | `DropModule`, `DropRole`; P: `SetReplicaIdentity` to a target that stands, at (0, 1) |
   | 1 | `RenameTable`, `RenameRole` |
   | 2 | `DropIndex`, `DropUnique`, `DropForeignKey`, `DropCheck`, `SetPrimaryKey { to: None }`; `DropComputedColumn` at the class's end (2, 4) |
   | 3 | `RenameColumn` |
   | 4 | `Revoke` |
   | 5 | `DropColumn` |
   | 6 | `DropTable` |
   | 7 | `CreateTable` |
   | 8 | `AddColumn` |
   | 9 (`COLUMN_ALTERATIONS`) | `AlterColumnType`, `AlterColumnNullability`, `AlterColumnDefault`, `AlterColumnExpression`, `SetPartitionDefault`, `SetPartitionNotNull`; `AddComputedColumn` at the class's end (9, 3); P: `SetTablePersistence` after it, by the foreign-key graph |
   | 10 | `SetColumnDeprecated`; P: `SetStorageParameters`, `SetIndexStorageParameters` |
   | 11 | `InsertRow`, `UpdateRow` |
   | 12 | `DeleteRow` |
   | 13 | A partitioned parent's `AddIndex` first (13, 0); `SetPrimaryKey { to: Some }`, `AddUnique`, `AddForeignKey`, `AddCheck`, `AddIndex`; P: the other `SetReplicaIdentity`, last |
   | 14 | `CreateModule`, `AlterModule`; `PublicExecution` immediately after its routine's create (#687) |
   | 15 | `CreateRole` |
   | 16 | `Grant` |
   | 17 | `SetDataMode` |

2. **A rank inside the class**, `sort_class` and `dependency_rank` in the same
   file:
   - a drop that frees a name a rename claims goes before the rename (DECISIONS
     474), and a generated column's drop goes first among them (DEC-1168.1);
   - a generated column is dropped before the ordinary columns of class 5, and
     added after every in-place alteration of class 9 (DEC-1168.1);
   - inside class 9, an old default is dropped before a retype, and a new one
     set after it. Nullability is relaxed before an expression change and
     tightened after it (DEC-1168.1);
   - modules are ranked by `creation_order_with` (DECISIONS 311, 315);
   - reference rows by the foreign keys between their tables;
   - role drops by membership, holder before member (`member_depth`,
     DECISIONS 127);
   - a foreign key comes after the key it references;
   - a partition is created after every other creation of class 7, its
     parent's included, at (7, 2) (DEC-1170.1);
   - a clustered index comes before the other indexes of its table
     (DEC-1178.1);
   - a replica identity whose target the plan adds, or whose columns become
     NOT NULL in it, comes after every addition of class 13; any other comes
     first, at (0, 1) (DEC-1444.1).
3. **`subject()`, then the change's rendering**, which only breaks ties.

Then, in this order:

- **`rename_order::order`** (`crates/pbps-diff/src/rename_order.rs`) puts a
  table rename after the drops that release its target name (DECISIONS 496,
  DEC-496.1, DEC-536.1): a dropped table everywhere, an index on PostgreSQL
  (indexes share the relation namespace), a named constraint on SQL Server.
  It orders chains. A table rename cycle is left out of its graph, and the
  engine refuses it. A column rename cycle is refused at diff time
  (`ColumnRenameCycle`); a column chain is ordered by `chain_depth`, and
  on a connected SQL Server plan by the collation's answer (DEC-1366.3).
- **On a connected SQL Server plan, `order_created_object_names`** (in
  `crates/pbps-cli/src/engine.rs`, last of the passes that reorder) first
  orders each table's column renames by the collation's answer (DEC-1366.3),
  then walks the plan over the catalog's `sys.objects` names. Where that walk refuses the
  order, it tries the other orders of the table renames among the drops and
  keeps the first one it clears (`object_order`, DEC-1366.1). A name only a
  later class frees is refused with that change named (DEC-1366.2).
- **On a connected plan, `order_role_drops`** (called from `deploy.rs`, once
  `plan --db` has read the dropped roles' members) ranks the role drops by
  membership again. The differ saw no members when it sorted (DECISIONS 127,
  139).
- **On a connected PostgreSQL plan** (`module_dependents` in
  `crates/pbps-cli/src/engine.rs`):
  - `weave` puts each catalog dependent of a dropped or rebuilt module on the
    right side of the drop: removed before it, restored after its create
    (DECISIONS 311, DEC-942.1). A partition's part is released by its
    parent's drop of the column, `DROP DEFAULT` or new generation
    expression, and named through its parent's column rename (DEC-1699.1).
    `after_its_release` then moves a function's
    drop after what releases its generated columns (DEC-1168.1).
    `release_generated_inputs` then moves the retype or drop of a column a
    generated column reads after the expression change that stops reading it,
    and what needs the retype after it, by edges from the catalog and the
    expressions' text (DEC-1316.1, DEC-1391.1).
  - `split_new_tables` splits the parts of a new table whose text names a
    function the plan creates out of it, ahead of that create.
  - `after_the_rebuilds` moves the expression-bearing additions whose text
    names a function the plan creates after the last function create, and
    places a new column whose expression names one after that create, with
    what may read the column after it (DEC-942.1, DEC-1364.1).
  - `after_their_parents_defaults` then sets each partition's own default
    again after a parent's default the plan sets, which reaches every
    partition (DEC-1581.1). It runs in bootstrap too.
- **On SQL Server**, a retype or recollation drops and re-adds the keys,
  indexes and checks over the column, through `retype_dependents` and
  `recollate_dependents` (#1175, DECISIONS 515). `CREATE OR ALTER` means a
  module edit drops nothing.

## Method

Two changes can only constrain each other's order through what one of them
requires and the other provides or takes away. So the audit is built in two
steps:

1. List, for each change kind, what has to hold before it runs, and what it
   creates, removes or rewrites ([the table below](#what-each-change-requires)).
2. A pair of kinds interacts when one side's effect meets the other side's
   requirement. Every other pair is **independent**. Each entry of the
   *Requires before it* column is matched against the *Creates, removes or
   rewrites* column of every kind, so each requirement has at least one row
   below, or none of the 33 kinds can affect it. Every arm of
   `dependency_rank` and every exception in `sort_class` that is not zero has
   a row too, since each one is a pair whose order its class alone does not
   decide. Each interacting pair falls
   into one of four classes:
   - **Fixed direction.** Whenever they interact, one must precede the other,
     whatever the content. Position can express it, and the audit checks that
     the current order does.
   - **Content-dependent direction.** The direction depends on what an
     expression or a body reads or calls. Position cannot express it. It
     belongs to dependency-graph ordering (#615), and its corners are recorded
     on #1350.
   - **Value flow.** A pre-flight probe of the later change reads values the
     earlier change rewrites. The general rule: project the change where the
     probe can (a retype through a cast); otherwise report the probe unchecked,
     never judged on the old values.
   - **Engine-sourced.** The edge comes from the catalog (`pg_depend`,
     `sys.sql_expression_dependencies`) at plan time, so it is exact, not
     positional.

## What each change requires

`P` is PostgreSQL and `S` is SQL Server; no marker means both. Every change
also requires the names it uses to be the ones in force at its position. That
requirement is common to all of them, so it is listed once,
[below](#names-in-force), rather than in every row.

| Change | Class | Requires before it | Creates, removes or rewrites |
|---|---|---|---|
| `DropModule` | 0 | Its dependents gone. P: views, routines, triggers, defaults, checks, indexes and generated columns, found in the catalog. S: schema-bound views | Removes the module, frees its name |
| `DropRole` | 0 | P: performed by hand (ADR-0010 §3). S: no members, no owned objects | Removes the role |
| `RenameTable` | 1 | The target name free | The table's new name |
| `RenameRole` | 1 | P: performed by hand. S: the target name free | The role's new name |
| `DropIndex`, `DropUnique`, `DropForeignKey`, `DropCheck`, `SetPrimaryKey { to: None }` | 2 | A foreign key on a key is dropped before the key | Frees names; releases columns for a rename, drop or retype |
| `DropComputedColumn` | 2 (2, 4) | S: the indexes, uniques and checks over it gone (4922) | Removes a computed column, frees its name and releases the columns and functions it reads (DEC-1174.1) |
| `RenameColumn` | 3 | The target name free. S: no check and no filtered index naming the column (DECISIONS 474) | The column's new name. P: dependents' text follows the rename |
| `Revoke` | 4 | The target exists (a revoke on an object the plan drops is not emitted) | Removes a permission |
| `DropColumn` | 5 | Its indexes, constraints, inbound foreign keys and modules gone. P: generated columns reading it gone | Removes the column, frees its name |
| `DropTable` | 6 | Inbound foreign keys and dependent modules gone | Removes the table, frees its name |
| `DetachPartition` | 6 | P: no row referencing the partition through a foreign key to its parent (pre-flight) | The partition leaves its parent with its rows, as an ordinary table under the declared names; frees its range (DEC-1544.1) |
| `CreateTable` | 7; a partition (7, 2), or (9, 5) under a parent whose columns the plan changes | The name free, its types. Functions its defaults, checks and generated columns call: *content* | The table, columns, key, uniques, checks, indexes |
| `AttachPartition` | 7; (11, 1) when its parent's foreign key references a table the plan writes rows into or attaches a table to | The parent standing; the table already its parent's columns, order and parent's checks (refused otherwise). P: no row of the table outside its range, no trigger or column grant on it, none in the parent's DEFAULT partition inside it (pre-flight) | The table becomes a partition with its rows; its columns become its parent's, its matching keys and indexes clones, and the rest its own; takes its range (DEC-1545.1) |
| `AddColumn` | 8 | The table, the name free. A generated column's inputs; functions its default or expression calls: *content* | The column, backfilled |
| `AlterColumnType` | 9 | What blocks a retype gone. S: keys, indexes, checks, foreign keys. P: views and rules (`weave`), generated readers (refused). An old default dropped first | Converted values |
| `AlterColumnNullability` | 9 | Tightening: the values non-null | Accepts or refuses NULL |
| `AlterColumnDefault` | 9 | Functions the default calls: *content* | The default |
| `SetPartitionDefault` | 9; (9, 4) after its parent's default or NOT NULL change on the column | Functions the default calls: *content*. P: after its parent's default set by the plan (`after_their_parents_defaults`) | One partition's default: its own, or its parent's again (DEC-1581.1) |
| `SetPartitionNotNull` | 9; (9, 4) after its parent's default or NOT NULL change on the column | Tightening: the partition's values non-null | One partition's column accepts or refuses NULL (DEC-1581.1) |
| `AlterColumnExpression` | 9 | P: its inputs, a relaxation of its own column. Functions it calls: *content* | Recomputed stored values |
| `AddComputedColumn` | 9 (9, 3) | S: the columns it reads, in their final type. Functions it calls exist (one this plan creates is refused by name) | A computed column at the end of its table (DEC-1174.1) |
| `SetColumnDeprecated` | 10 | Nothing | Metadata only |
| `SetTablePersistence` | 9 (9, 10 + depth) | P: the foreign keys that stand: to logged, the tables it references logged first; to unlogged, the tables referencing it unlogged first (refused the other way round, measured) | Rewrites the table and its indexes under `AccessExclusiveLock` (DEC-1443.1) |
| `SetStorageParameters` | 10 | P: the table, under its final name. Nothing reads a storage parameter | The table's heap storage parameters; no rewrite (DEC-1441.1) |
| `SetIndexStorageParameters` | 10 | P: the index, which stands: one the plan adds or rebuilds takes its parameters in its `CREATE` instead | An index's storage parameters, in place; no rebuild (DEC-1442.1) |
| `InsertRow`, `UpdateRow` | 11 | The columns written, defaults the row takes, parent rows | Rows |
| `DeleteRow` | 12 | Child rows moved away or cascaded | Removes a row |
| `SetPrimaryKey { to: Some }`, `AddUnique`, `AddIndex` | 13 | Columns, NOT NULL, unique data; a clustered index first | A key or index |
| `AddForeignKey` | 13 | Its columns, the referenced key, valid data | A foreign key |
| `SetReplicaIdentity` | 0 (0, 1) or 13, last | P: the key, unique constraint or unique index it names, over NOT NULL columns. At (0, 1) when that stands before the plan, before any drop of the old identity's index; after the additions otherwise | The table's replica identity (DEC-1444.1) |
| `AddCheck` | 13 | Its columns, valid data. Functions it calls: *content* | A check |
| `CreateModule`, `AlterModule` | 14 | What its definition names: other modules, by lexed name (DECISIONS 315); tables, columns and types | A module. P `AlterModule`: a drop and a create |
| `CreateRole` | 15 | The name free | A role |
| `PublicExecution` | 14, at its routine's create rank | The routine exists | `PUBLIC`'s execute on a routine, settled in the statement after its `CREATE` (#687) |
| `Grant` | 16 | The role and the target exist | A permission |
| `SetDataMode` | 17 | The table's rows written | The table's data mode |

## Pairs that interact

Interacting pairs fall into the categories below. Each category has one rule
that the classes satisfy by construction, stated at its head. A pair of
kinds in a category follows that rule unless its table says otherwise. The
tables list:
- every pair whose order something other than its classes decides (a rank,
  a pass, a refusal);
- every pair the classes get wrong;
- the pairs a reviewer is likely to ask about.

A pair in no category is independent: neither side's effect meets the
other's requirement in the table above. Columns:
- **Kind:** *fixed*, *content*, *value* or *engine*, as defined under
  [Method](#method).
- **Order now:** what the mechanism does today.
- **Verdict:** ✓ where the order is right, with its evidence; ✗ where it is
  wrong, with its follow-up issue; ⧗ where it is left to the engine, with its
  record on #1350.

### Names changing hands

**Rule:** a change that frees a name runs in a lower class than any change that
takes it. Drops and renames away (classes 0 to 6) run before creates and adds
(7, 8, 13, 14, 15). The rule is by class, not by kind, so it holds across the
namespaces kinds share: on SQL Server tables and modules share one object
namespace, and on PostgreSQL views, tables and indexes share the relation
namespace. Renames *into* a name (classes 1 and 3) are the exception, and
`rename_order` and `frees_a_renamed_column` order them.

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| `DropTable` → `CreateTable` of its name | fixed | class 6 before 7 | ✓ |
| `RenameTable` away → `CreateTable` of the vacated name | fixed | class 1 before 7 | ✓ |
| `DropModule` (a view) → `CreateTable` of its name | fixed | class 0 before 7 | ✓ |
| `DropTable` → `CreateModule` of its name | fixed | class 6 before 14 | ✓ |
| `DropTable` or a rename away → `RenameTable` into its name | fixed | `rename_order` | ✓ DEC-536.1 |
| `DropColumn` → `RenameColumn` into its name | fixed | the drop moves to (2, 3), ahead of class 3 | ✓ DECISIONS 474 |
| `DropColumn` or `RenameColumn` away → `AddColumn` of its name | fixed | class 5 or 3 before 8 | ✓ |
| A column rename chain | fixed | `chain_depth` inside class 3; on a connected SQL Server plan, reordered by the collation's answer | ✓ DEC-1366.3 |
| A column rename cycle (a swap) | — | refused at diff time, `ColumnRenameCycle` | ✓ refused by design |
| A table rename chain | fixed | `rename_order` | ✓ DEC-536.1 |
| A table rename cycle | — | left out of `rename_order`'s graph; refused at `plan --db` with the remedy | ✓ refused by design, DEC-1366.2 |
| A catalog-only holder (adopted or fallback default, unrecorded object, collation pair) → `RenameTable` into its name | engine-sourced | `object_order` on a connected SQL Server plan | ✓ DEC-1366.1 |
| `RenameColumn` or another later class freeing a default name → `RenameTable` into it | — | refused at `plan --db` with the remedy | ✓ refused by design, DEC-1366.2 |
| Index (P) or named constraint (S) drop → `RenameTable` into its name | fixed | `rename_order` | ✓ DECISIONS 496, DEC-496.1 |
| Constraint or index drop → add of the same name | fixed | class 2 before 13 | ✓ DECISIONS 168 (namespaces) |
| `DropModule` → `CreateModule` of its id | fixed | class 0 before 14 | ✓ |
| `DropRole` or `RenameRole` away → `CreateRole` of its name | fixed | class 0 or 1 before 15 | ✓ |
| `DropRole` of a holder → `DropRole` of its member | fixed | `member_depth`, then `order_role_drops` once the members are read | ✓ DECISIONS 127, 139 |
| Trigger and grants keyed by a table name that passes to another table | fixed | rebuilt as the occupant's | ✓ DEC-1118.1 |

### Names in force

Every change names its objects as they are named at its own position, so a
rename provides the name every later change uses.

**Rule:** a rename (class 1 or 3) runs after the module drops (class 0) that
name the old name. It runs before every change that names the new one.

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| `RenameTable` → `RenameColumn` and every later change naming the table | fixed | class 1 before 3 and the rest; `RenameColumn` carries the new table name | ✓ |
| `RenameTable` → class-2 drops naming the table | fixed | class 1 before 2; the drops use the new name | ✓ |
| Module drops (class 0) → renames | fixed | class 0 first; the drops use the old names | ✓ |
| S: a module drop the computed edges ordered (among the drops, or ahead of them with a pair) ↔ table renames | engine-sourced | on a connected plan whose renames `object_order` searches, the drop moves between what the edges order it after and before when the catalog holds an object under its name and a rename can claim it, and a table rename may move around it; any other keeps its place among the drops. A module's drop names the module, never a renamed table, and the walk frees its name where it runs | ✓ DEC-1461.1, DEC-1680.1 |
| `RenameColumn` → later changes naming the column (classes 4 to 17) | fixed | class 3 before them | ✓ |
| Constraint and index drops naming a column → its rename | fixed | class 2 before 3; the drops use the old name | ✓ DECISIONS 474 |
| `RenameRole` → `Revoke` and `Grant` naming the role | fixed | class 1 before 4 and 16. P: performed by hand, and grants follow the role's oid | ✓ ADR-0010 §3 |

### Something created before what needs it

**Rule:** a change that creates an object runs in a lower class than every
change that needs it. Tables (7) and columns (8) come before alterations (9),
rows (11), constraints (13), modules (14) and grants (16). Roles (15) come
before grants (16). The exceptions, listed here, are the pairs inside one class
and the expression-bearing changes that need a function.

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| `CreateTable` → rows, keys, foreign keys, modules and grants on it | fixed | class 7 before 11, 13, 14, 16 | ✓ |
| `CreateTable` of a partitioned parent → `CreateTable` of its partition | fixed | (7, 2) after the rest of class 7 | ✓ DEC-1170.1 |
| `AlterColumnDefault`, `AlterColumnNullability`, or an `AlterColumnType` changing nullability, of a parent → its partitions' `SetPartitionDefault` and `SetPartitionNotNull` on that column | fixed | (9, 4) after the parent's class 9; the parent's change recurses over the partition's own (DEC-1687.1) | ✓ DEC-1687.1 |
| A parent's column change (`AddColumn`, `DropColumn`, `RenameColumn`, `AlterColumn*`) → `CreateTable` of a partition under it | fixed | (9, 5) after them: the partition is created with its own entries on the parent's columns as they end (DEC-1687.1) | ✓ DEC-1687.1 |
| `AttachPartition` → the partition's own `SetPartitionDefault`, `SetPartitionNotNull`, `SetTablePersistence`, `SetStorageParameters`, `AddIndex` and `AddCheck` | fixed | class 7 before 9, 10 and 13; each acts on the partition alone once attached. After an attach at (11, 1), its own alterations of classes 9 and 10 move to (11, 2) | ✓ DEC-1545.1 |
| A partitioned parent's `AddIndex` → a partition's own `AddIndex` | fixed | (13, 0) before the rest of class 13, a structural edge in the resolver's graph, first among the indexes the dependents' weave restores around a rebuilt function, and `before_its_partitions_indexes`, the last reordering of `plan --db`, which moves the parent's ahead of a partition index the passes left before it, or refuses by name: the parent's index built after a partition's own of its shape takes it as its clone (DEC-1688.1) | ✓ DEC-1688.1 |
| `AddColumn` → rows, constraints, modules naming it | fixed | class 8 before 11, 13, 14 | ✓ |
| `AddColumn` (input) → generated `AddColumn` reading it | fixed | (9, 2) after class 8 | ✓ DEC-1168.1 |
| `AlterColumnType` of an existing input → generated `AddColumn` reading it | fixed | (9, 2) after the in-place alterations: a standing generated reader blocks the retype | ✓ DEC-1168.1 |
| S: clustered `SetPrimaryKey`, `AddUnique` or `AddIndex` → the table's other added indexes | fixed (cost) | rank inside class 13. Built after them, the clustered layout would rebuild each one; correctness needs nothing here | ✓ DEC-1178.1 |
| `AddColumn` or `RenameColumn` (input) → `AlterColumnExpression` reading it | fixed | class 8 or 3 before 9 | ✓ DEC-1168.1 |
| S: `AddColumn`, `AlterColumnType` or `AlterColumnNullability` (input) → `AddComputedColumn` reading it | fixed | (9, 3) after class 8 and the class's alterations | ✓ DEC-1174.1 |
| S: `AddComputedColumn` → the index, unique or check over it | fixed | class 9 before 13 | ✓ DEC-1174.1 |
| S: function created → `AddComputedColumn` calling it | content, over-approximated | refused by name (`may_name`): the function is class 14 | ✓ DEC-1174.1 |
| Retype of a generated column → its `AlterColumnExpression` | fixed | rank −1 before 0: the new expression is computed in the final type | ✓ DEC-1168.1 (`a_generated_columns_nullability_relaxes_before_and_tightens_after_its_expression`) |
| Referenced key → `AddForeignKey` | fixed | rank inside class 13 | ✓ |
| P: `SetPrimaryKey`, `AddUnique` or `AddIndex` of the identity's index, new or rebuilt → `SetReplicaIdentity` | fixed | rank 2, last in class 13. A rebuilt index is not the identity until it is set again, so the differ plans the identity wherever the plan re-adds its index | ✓ DEC-1444.1 |
| P: `SetReplicaIdentity` to a target that stands → `DropIndex`, `DropUnique` or `SetPrimaryKey { to: None }` of the old identity's index | fixed | (0, 1) before class 2. Dropped first, the old index leaves the table identifying no row until the identity is set | ✓ DEC-1444.1 |
| `CreateRole` → `Grant` to it | fixed | class 15 before 16 | ✓ |
| `CreateModule` or `AlterModule` → `Grant` on the routine | fixed | class 14 before 16. P: a rebuild's lost grants are restated after it | ✓ ADR-0010 §5 |
| `CreateModule` or `AlterModule` → `PublicExecution` on the routine | fixed | same class, same create rank and subject: the next statement, so an autocommit `--sql` script never leaves the routine executable by `PUBLIC` past its own `CREATE` | ✓ #687 |
| Tightening → primary key or unique key over the column | fixed | class 9 before 13 | ✓ |
| Row writes → `SetDataMode` of their table | fixed | class 11 and 12 before 17 | ✓ |
| Module → module that names it | content, over-approximated | `creation_order_with`, lexed names (DECISIONS 315) | ✓ |
| Function created or rebuilt → check, filtered index or set default calling it | content, over-approximated | `after_the_rebuilds`: after the last function create, when its text names the function | ✓ DEC-942.1, DEC-1364.1 |
| Function created or rebuilt → `AlterColumnExpression` calling it | content, over-approximated | `after_the_rebuilds`, when its text names the function | ✓ DEC-1168.1, DEC-1364.1 |
| Function created or rebuilt → `AddColumn` whose default or generation expression calls it | content, over-approximated | `after_their_functions`: after the create its text names, and what may read the column, or a partition holding it, after it; a cycle is refused by name | ✓ DEC-1364.1 |
| `AddColumn` → a module created that reads it | content, over-approximated | class 8 before 14; reordered after a column that moves, when it names the column or its table | ✓ DEC-1364.1 |
| Function rebuilt → default a row of the plan takes | content | stays ahead of the rows | ⧗ #1030 |
| Function created → a new table's expression-bearing parts | content, over-approximated | `split_new_tables`, then `after_the_rebuilds`, when their text names the function | ✓ #1027, DEC-1364.1 |
| Function created → a new table's generated column calling it | content | stays inside `CREATE TABLE`: split out, the column would move to the end of the table | ⧗ DEC-1364.1 |

### Something removed before what it blocks

**Rule:** a change that removes a dependent runs in a lower class than the
drop, rename or alteration it would block. Module drops (0) and constraint,
index and key drops (2) come before renames (1, 3), column and table drops
(5, 6) and alterations (9). The exceptions, listed here, are the pairs inside
one class and the dependents a class cannot see.

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| Dependent module → its table, column or retyped column | fixed | class 0 first. P: rebuilt around a retype from the catalog | ✓ DECISIONS 311; `to_rebuild` |
| Catalog dependents of a dropped or rebuilt module | engine | `weave` | ✓ DECISIONS 311, DEC-942.1 |
| `DropModule` → `DropModule` of a module it names | fixed | `drop_rank`, deepest first inside class 0, from the base's lexed names | ✓ DECISIONS 311, 315 |
| Generated column's release (expression change, column or table drop) → drop of the function it calls | engine | `after_its_release`, after the drops of dependent modules too | ✓ DEC-1168.1 |
| Function drop moved after its release ← a column its own body reads, dropped or retyped earlier | content | the drop stays after the release | ⧗ #1350 |
| Foreign key → the key it references | fixed | rank inside class 2 | ✓ |
| Constraint, index or key drop → `DropColumn`, `RenameColumn` (S) | fixed | class 2 before 3 and 5 | ✓ DECISIONS 474 |
| Inbound foreign key → `DropTable` | fixed | class 2 before 6 | ✓ |
| `DropTable` of a partition → `CreateTable` of a partition over its range | fixed | class 6 before 7; the new range's pre-flight count leaves the dropped partition's rows out | ✓ DEC-1171.1 |
| `DropTable` → `DetachPartition` claiming a name the dropped table's index or key holds | fixed | (6, 1) before (6, 2) | ✓ DEC-1544.1 |
| `DetachPartition` → `CreateTable` of a partition over its range | fixed | class 6 before 7; the new range's pre-flight count leaves the detached partition's rows out | ✓ DEC-1544.1 |
| `DropTable` or `DetachPartition` of a partition → `AttachPartition` over its range | fixed | class 6 before 7; the new range's pre-flight count leaves the leaving partition's rows out | ✓ DEC-1545.1 |
| A table's own `DropIndex` or `DropCheck` → its `AttachPartition` | fixed | class 2 before 7; the engine takes either order, an own index or check staying the table's across the attach | ✓ DEC-1545.1 |
| `SetPrimaryKey { to: None }` → relaxing a key column's nullability | fixed | class 2 before 9 | ✓ DECISIONS 269 |
| Generated column → its input's drop | fixed | (5, 0); (2, 2) beside a rename | ✓ DEC-1168.1 |
| S: index, unique or check over a computed column → its drop, and its re-add around an expression change | fixed | dropped in class 2 before (2, 4), re-added in 13 (`recreate_retyped_dependents`) | ✓ DEC-1174.1 |
| S: computed column dropped, alone or with its table → drop of a function it calls | catalog (`sys.sql_expression_dependencies`), connected | the function's drop moves after the column's or the table's drop, and what it is schema-bound to after it (3729 otherwise) | ✓ DEC-1431.1 |
| S: that function drop → the drop of a table it is schema-bound to, when the rename search reorders the plan | catalog, connected | `object_order` keeps the pairs `computed_order::drop_precedence` gives: the table drop waits for the drop of the function schema-bound to it | ✓ DEC-1461.1, DEC-1680.1 |
| S: drop of a module schema-bound to a computed column → that column's drop → the drop of a column it reads | catalog, connected | the computed column's drop moves after the module's, and the drop of a column it reads after it; the rename search keeps both as pairs | ✓ DEC-1680.1 |
| S: standing or re-added computed column → rename, drop, retype or nullability change of a column it reads; alter or drop of a function it calls | fixed, over-approximated | refused by name (`may_name`), with a two-plan remedy; a computed column the plan drops or changes is out of the way at (2, 4) | ✓ DEC-1174.1 |
| S: key, index, check or foreign key over a column → its retype or recollation | fixed | dropped in class 2, re-added in 13 (`retype_dependents`) | ✓ #1175, DECISIONS 515 |
| S: index key, `INCLUDE` column, filtered predicate or unique constraint over a column → tightening its nullability; filtered predicate → relaxing it | fixed | dropped in class 2, re-added in 13 (`nullability_dependents`), alone or inside a retype | ✓ DEC-1363.1 |
| P: generated column → retype or drop of its input | fixed | refused by name, with a two-plan remedy, unless the plan drops the generated column or changes its expression to one that does not name the input | ✓ DEC-1168.1 |
| P: expression change releasing an input → that input's retype or drop | engine, over-approximated | `release_generated_inputs` moves the retype or drop right after the release. The old reads are `pg_depend`'s; a new text naming the input keeps it refused (`may_read`) | ✓ DEC-1316.1 |
| P: retype → another generated column's expression change that starts reading it | engine, over-approximated | an edge in `release_generated_inputs`; with a drop instead, refused by name | ✓ DEC-1391.1 |
| Old default removed → retype → new default set | fixed | dependency ranks −2, −1, 0 inside class 9. S: the old default blocks the retype (5074). Both: a default written for the new type is invalid under the old one | ✓ (`a_default_written_for_the_new_type_is_set_after_the_type_is`) |
| `Revoke` on an object the plan drops | fixed | not emitted | ✓ |

### Values one change writes and a later one reads

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| Retype → probes of later keys, checks and deletes | value | the probe projects through the new type, or the probe is unchecked | ✓ DECISIONS 340, 410 |
| `AddColumn` backfill → probes | value | a literal is projected; an expression, identity or generated column is unchecked | ✓ DECISIONS 336, 339 |
| Row writes → probes of keys, foreign keys and checks | value | the probe reads the rows after the plan's writes | ✓ DECISIONS 335 |
| Row writes setting a column, or deleting the rows that hold its NULLs → tightening it to NOT NULL | fixed, value | (12, 2), after the rows of its table; split from a retype; the probe reads the rows after the plan's writes | ✓ DEC-1367.1 |
| `AlterColumnExpression` → tightening, keys, checks, filtered indexes, deletes | value | the probe is unchecked; the order is tighten and keys after the recomputation | ✓ DEC-1168.1 |
| Default set → a row that takes it | value | the default goes first; it stays ahead of a rebuild | ✓ #1030 |
| Parent's default set → a partition's own default on that column | value | `after_their_parents_defaults`: the partition's after the parent's, which overwrites it | ✓ DEC-1581.1 |
| Nullability relaxed → expression recomputing a NULL | value | rank −1 before 0 | ✓ DEC-1168.1 |
| Expression recomputed → nullability tightened | value | rank 0 before +1; split from a retype | ✓ DEC-1168.1 |

### Rows among themselves

**Rule:** rows follow the foreign keys between their tables. A referenced
table's rows go in first and come out last. Deletes run after every insert
and update (SPEC §4.6).

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| Parent row → child row (insert, update) | fixed | data rank inside class 11 | ✓ |
| Child `DeleteRow` → parent `DeleteRow` | fixed | the data rank, negated inside class 12 | ✓ |
| Row update moving a reference away → `DeleteRow` of the old parent | fixed | class 11 before 12 | ✓ |
| `InsertRow` or `UpdateRow` into a table a partitioned parent's foreign key references, or `AttachPartition` under that table → `AttachPartition` under the parent, which validates the key over the rows it brings | fixed | (11, 1), at the parent's data rank: with attaches in the plan, the data order covers their parents too (`supply_order`) | ✓ DEC-1545.1 |
| `AttachPartition` → `InsertRow` or `UpdateRow` into a table referencing its parent, and `AddForeignKey` into the parent | fixed | class 7 before 11 and 13; at (11, 1), the referencing table ranks after every table the parent references. P: the parent's rows counted with the attached table's | ✓ DEC-1545.1 |
| `InsertRow` or `UpdateRow` taking a value another row holds under a UNIQUE elsewhere → `DeleteRow` of that row | content | the write first (class 11 before 12): it is refused inside the transaction, loudly. Deleting first could cascade a moving child away silently | ✓ by design, SPEC §4.6 |

## Content-dependent pairs

Every content-dependent pair is the same question: does this expression or
body name that function or column? The planner never answers it from the
text's meaning (DECISIONS 174). It answers with **a lexical
over-approximation**: the text is read through the dialect's lexer, and every
name it could refer to is taken as an edge. An edge too many costs nothing but
a later position; one too few puts a change before what it needs. The answer
is conservative, so it can never be wrong in the dangerous direction.

- `creation_order_with` reads each module's definition this way (DECISIONS
  315).
- `after_the_rebuilds` reads a check's, an index's, a default's and a
  generation expression's text for the functions the plan creates or rebuilds
  (DEC-1364.1). Only an addition whose text names one moves after the last
  create; the rest keep the differ's place. A new column whose expression
  names one follows that create, and what may read the column follows the
  column: a module naming it, or naming its table. Where the
  function itself may read the column, or a row the plan writes needs it,
  no order performs both, and the plan is refused by name with a two-plan
  remedy. Over-approximated in both directions, a refusal can name a cycle the
  engine would not have, almost always in a plan the earlier positional rule
  also left to fail (DEC-1364.1 names the exception).

- `names_a_later_relation` reads the same texts' literals for a relation the
  plan creates after them, which PostgreSQL resolves as the expression is
  created: `'app.ix'::regclass`. It does not move anything; the plan is refused
  with a two-plan remedy (DEC-1576.1).

What is left to the engine is recorded on #1350, each with a two-plan remedy:
- an expression change still calling a function the plan rebuilds;
- a function dropped after its release whose own body reads a column dropped
  or retyped earlier;
- a default a plan row takes calling a rebuilt function (#1030);
- a generated column of a table the plan creates calling a function the plan
  creates or rebuilds: taken out of `CREATE TABLE`, the column would change
  the table's column order (DEC-1364.1).

## Value flow

Each value-flow pair has its own guard today:
- a retype is projected through a cast (DECISIONS 340, 410);
- a literal backfill is projected (DECISIONS 339);
- planned row writes are read as the rows after the plan (DECISIONS 335);
- a recomputed generated column is reported unchecked (DEC-1168.1).

They all follow one rule: **a probe that reads a value an earlier change of the
plan rewrites either projects that change, or is reported unchecked.** No
probe judges the old value. What the per-guard form lacks is a single place that
names, for each change kind, which values it rewrites. A new kind that rewrites
values, such as a SQL Server computed column (#1174), has to remember every
probe that might read them. Folding the guards into one "values this plan
rewrites before position *i*" table in `pbps-pg::preflight` and its SQL Server
counterpart would make that table the one row a new kind adds.

## Findings

| Finding | Kind | Disposition |
|---|---|---|
| S: tightening a column an index or a unique constraint covers is refused at apply | fixed, wrong | fixed, DEC-1363.1 |
| Tightening runs before the reference rows that fill its NULLs, and its probe ignores them | fixed, wrong | fixed, DEC-1367.1 |
| Content-dependent corners of expression-bearing changes around function creates, rebuilds and drops | content | #1350; the lexical over-approximation above closes the new-column corner (DEC-1364.1) |
| Value-flow guards are per-probe, not one rule | value | recorded above; a refactor when a new value-rewriting kind arrives (#1174) |

Every other interacting pair is ordered correctly. Its evidence is the
decision or the issue named in its row.

## Keeping this current

- A new `Change` variant adds a row to [the table](#what-each-change-requires).
  `every_change_kind_has_a_row_in_the_ordering_audit` lists the variants from
  the enum and refuses one this file does not name.
- A new sort class, rank or reordering pass updates [the mechanism](#the-mechanism)
  and re-reads the pairs its changes appear in.
- A new interacting pair found in review goes into the matching table with its
  verdict. It is not fixed in isolation: decide whether it is fixed or
  content-dependent first.
