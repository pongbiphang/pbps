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
   | 0 | `DropModule`, `DropRole` |
   | 1 | `RenameTable`, `RenameRole` |
   | 2 | `DropIndex`, `DropUnique`, `DropForeignKey`, `DropCheck`, `SetPrimaryKey { to: None }` |
   | 3 | `RenameColumn` |
   | 4 | `Revoke` |
   | 5 | `DropColumn` |
   | 6 | `DropTable` |
   | 7 | `CreateTable` |
   | 8 | `AddColumn` |
   | 9 (`COLUMN_ALTERATIONS`) | `AlterColumnType`, `AlterColumnNullability`, `AlterColumnDefault`, `AlterColumnExpression` |
   | 10 | `SetColumnDeprecated` |
   | 11 | `InsertRow`, `UpdateRow` |
   | 12 | `DeleteRow` |
   | 13 | `SetPrimaryKey { to: Some }`, `AddUnique`, `AddForeignKey`, `AddCheck`, `AddIndex` |
   | 14 | `CreateModule`, `AlterModule` |
   | 15 | `CreateRole` |
   | 16 | `Grant`, `PublicExecution` |
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
   - a clustered index comes before the other indexes of its table
     (DEC-1178.1).
3. **`subject()`, then the change's rendering**, which only breaks ties.

Then, in this order:

- **`rename_order::order`** (`crates/pbps-diff/src/rename_order.rs`) puts a
  rename chain or swap in an order the engine accepts (DEC-536.1).
- **On a connected plan, `order_role_drops`** (called from `deploy.rs`, once
  `plan --db` has read the dropped roles' members) ranks the role drops by
  membership again. The differ saw no members when it sorted (DECISIONS 127,
  139).
- **On a connected PostgreSQL plan** (`module_dependents` in
  `crates/pbps-cli/src/engine.rs`):
  - `weave` puts each catalog dependent of a dropped or rebuilt module on the
    right side of the drop: removed before it, restored after its create
    (DECISIONS 311, DEC-942.1). `after_its_release` then moves a function's
    drop after what releases its generated columns (DEC-1168.1).
  - `split_new_tables` splits the expression-bearing parts out of a new table
    ahead of a function create.
  - `after_the_rebuilds` moves expression-bearing additions after the last
    function create (DEC-942.1).
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
   requirement. Every other pair is **independent**. Each interacting pair falls
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

`P` is PostgreSQL and `S` is SQL Server; no marker means both.

| Change | Class | Requires before it | Creates, removes or rewrites |
|---|---|---|---|
| `DropModule` | 0 | Its dependents gone. P: views, routines, triggers, defaults, checks, indexes and generated columns, found in the catalog. S: schema-bound views | Removes the module, frees its name |
| `DropRole` | 0 | P: performed by hand (ADR-0010 §3). S: no members, no owned objects | Removes the role |
| `RenameTable` | 1 | The target name free | The table's new name |
| `RenameRole` | 1 | P: performed by hand. S: the target name free | The role's new name |
| `DropIndex`, `DropUnique`, `DropForeignKey`, `DropCheck`, `SetPrimaryKey { to: None }` | 2 | A foreign key on a key is dropped before the key | Frees names; releases columns for a rename, drop or retype |
| `RenameColumn` | 3 | The target name free. S: no check and no filtered index naming the column (DECISIONS 474) | The column's new name. P: dependents' text follows the rename |
| `Revoke` | 4 | The target exists (a revoke on an object the plan drops is not emitted) | Removes a permission |
| `DropColumn` | 5 | Its indexes, constraints, inbound foreign keys and modules gone. P: generated columns reading it gone | Removes the column, frees its name |
| `DropTable` | 6 | Inbound foreign keys and dependent modules gone | Removes the table, frees its name |
| `CreateTable` | 7 | The name free, its types. Functions its defaults, checks and generated columns call: *content* | The table, columns, key, uniques, checks, indexes |
| `AddColumn` | 8 | The table, the name free. A generated column's inputs; functions its default or expression calls: *content* | The column, backfilled |
| `AlterColumnType` | 9 | What blocks a retype gone. S: keys, indexes, checks, foreign keys. P: views and rules (`weave`), generated readers (refused). An old default dropped first | Converted values |
| `AlterColumnNullability` | 9 | Tightening: the values non-null | Accepts or refuses NULL |
| `AlterColumnDefault` | 9 | Functions the default calls: *content* | The default |
| `AlterColumnExpression` | 9 | P: its inputs, a relaxation of its own column. Functions it calls: *content* | Recomputed stored values |
| `SetColumnDeprecated` | 10 | Nothing | Metadata only |
| `InsertRow`, `UpdateRow` | 11 | The columns written, defaults the row takes, parent rows | Rows |
| `DeleteRow` | 12 | Child rows moved away or cascaded | Removes a row |
| `SetPrimaryKey { to: Some }`, `AddUnique`, `AddIndex` | 13 | Columns, NOT NULL, unique data; a clustered index first | A key or index |
| `AddForeignKey` | 13 | Its columns, the referenced key, valid data | A foreign key |
| `AddCheck` | 13 | Its columns, valid data. Functions it calls: *content* | A check |
| `CreateModule`, `AlterModule` | 14 | What its definition names: other modules, by lexed name (DECISIONS 315); tables, columns and types | A module. P `AlterModule`: a drop and a create |
| `CreateRole` | 15 | The name free | A role |
| `Grant`, `PublicExecution` | 16 | The role and the target exist | A permission |
| `SetDataMode` | 17 | The table's rows written | The table's data mode |

## Pairs that interact

Every pair not listed here is independent: neither side's effect meets the
other's requirement in the table above. Columns:
- **Kind:** *fixed*, *content*, *value* or *engine*, as defined under
  [Method](#method).
- **Order now:** what the mechanism does today.
- **Verdict:** ✓ where the order is right, with its evidence; ✗ where it is
  wrong, with its follow-up issue; ⧗ where it is left to the engine, with its
  record on #1350.

### Names changing hands

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| `DropTable` → `CreateTable` of its name | fixed | class 6 before 7 | ✓ |
| `DropTable` or a rename away → `RenameTable` into its name | fixed | `rename_order` | ✓ DEC-536.1 |
| `DropColumn` → `RenameColumn` into its name | fixed | the drop moves to (2, 3), ahead of class 3 | ✓ DECISIONS 474 |
| `DropColumn` or `RenameColumn` away → `AddColumn` of its name | fixed | class 5 or 3 before 8 | ✓ |
| A rename chain or swap among columns or tables | fixed | `rename_order`, `chain_depth` | ✓ DEC-536.1 |
| Constraint or index drop → add of the same name | fixed | class 2 before 13 | ✓ DECISIONS 168 (namespaces) |
| `DropModule` → `CreateModule` of its id | fixed | class 0 before 14 | ✓ |
| `DropRole` or `RenameRole` away → `CreateRole` of its name | fixed | class 0 or 1 before 15 | ✓ |
| `DropRole` of a holder → `DropRole` of its member | fixed | `member_depth`, then `order_role_drops` once the members are read | ✓ DECISIONS 127, 139 |
| Trigger and grants keyed by a table name that passes to another table | fixed | rebuilt as the occupant's | ✓ DEC-1118.1 |

### Something created before what needs it

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| `CreateTable` → rows, keys, foreign keys, modules and grants on it | fixed | class 7 before 11, 13, 14, 16 | ✓ |
| `AddColumn` → rows, constraints, modules naming it | fixed | class 8 before 11, 13, 14 | ✓ |
| `AddColumn` (input) → generated `AddColumn` reading it | fixed | (9, 2) after class 8 | ✓ DEC-1168.1 |
| `AddColumn` or `RenameColumn` (input) → `AlterColumnExpression` reading it | fixed | class 8 or 3 before 9 | ✓ DEC-1168.1 |
| Referenced key → `AddForeignKey` | fixed | rank inside class 13 | ✓ |
| Parent row → child row | fixed | data rank inside class 11 | ✓ |
| `CreateRole` → `Grant` to it | fixed | class 15 before 16 | ✓ |
| Module → module that names it | content, over-approximated | `creation_order_with`, lexed names (DECISIONS 315) | ✓ |
| Function created or rebuilt → check, filtered index or set default calling it | content, over-approximated | `after_the_rebuilds`: after the last function create | ✓ DEC-942.1 |
| Function created or rebuilt → `AlterColumnExpression` calling it | content, over-approximated | `after_the_rebuilds` | ✓ DEC-1168.1 |
| Function created or rebuilt → `AddColumn` whose default or generation expression calls it | content | stays ahead: the function may read the column | ⧗ DEC-942.1, DEC-1168.1; #1350 |
| Function rebuilt → default a row of the plan takes | content | stays ahead of the rows | ⧗ #1030 |
| Function created → a new table's expression-bearing parts | content, over-approximated | `split_new_tables`, then `after_the_rebuilds` | ✓ #1027 |

### Something removed before what it blocks

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| Dependent module → its table, column or retyped column | fixed | class 0 first. P: rebuilt around a retype from the catalog | ✓ DECISIONS 311; `to_rebuild` |
| Catalog dependents of a dropped or rebuilt module | engine | `weave` | ✓ DECISIONS 311, DEC-942.1 |
| Generated column's release (expression change, column or table drop) → drop of the function it calls | engine | `after_its_release`, after the drops of dependent modules too | ✓ DEC-1168.1 |
| Function drop moved after its release ← a column its own body reads, dropped or retyped earlier | content | the drop stays after the release | ⧗ #1350 |
| Foreign key → the key it references | fixed | rank inside class 2 | ✓ |
| Constraint, index or key drop → `DropColumn`, `RenameColumn` (S) | fixed | class 2 before 3 and 5 | ✓ DECISIONS 474 |
| Inbound foreign key → `DropTable` | fixed | class 2 before 6 | ✓ |
| Generated column → its input's drop | fixed | (5, 0); (2, 2) beside a rename | ✓ DEC-1168.1 |
| S: key, index, check or foreign key over a column → its retype or recollation | fixed | dropped in class 2, re-added in 13 (`retype_dependents`) | ✓ #1175, DECISIONS 515 |
| **S: index key, `INCLUDE` column, filtered predicate or unique constraint over a column → tightening its nullability** | fixed | nothing is dropped: the `ALTER COLUMN … NOT NULL` is refused (5074, 4922), measured on 17.0 (`tightening_nullability_is_refused_by_what_indexes_the_column`) | **✗ #1363** |
| P: generated column → retype or drop of its input | fixed | refused by name, with a two-plan remedy | ✓ DEC-1168.1, #1316 |
| S: old default → retype | fixed | dependency rank −2 before −1 | ✓ |
| `Revoke` on an object the plan drops | fixed | not emitted | ✓ |

### Values one change writes and a later one reads

| Pair | Kind | Order now | Verdict |
|---|---|---|---|
| Retype → probes of later keys, checks and deletes | value | the probe projects through the new type, or the probe is unchecked | ✓ DECISIONS 340, 410 |
| `AddColumn` backfill → probes | value | a literal is projected; an expression, identity or generated column is unchecked | ✓ DECISIONS 336, 339 |
| Row writes → probes of keys and foreign keys | value | the probe reads the rows after the plan's writes | ✓ DECISIONS 335 |
| `AlterColumnExpression` → tightening, keys, checks, filtered indexes, deletes | value | the probe is unchecked; the order is tighten and keys after the recomputation | ✓ DEC-1168.1 |
| Default set → a row that takes it | value | the default goes first; it stays ahead of a rebuild | ✓ #1030 |
| Row update moving references away → `DeleteRow` | value | class 11 before 12 | ✓ |
| Nullability relaxed → expression recomputing a NULL | value | rank −1 before 0 | ✓ DEC-1168.1 |
| Expression recomputed → nullability tightened | value | rank 0 before +1; split from a retype | ✓ DEC-1168.1 |

## Content-dependent pairs

Every content-dependent pair is the same question: does this expression or
body name that function or column? The planner never answers it from the
text's meaning (DECISIONS 174). It uses one of two stand-ins:

- **A lexical over-approximation.** `creation_order_with` reads each module's
  definition through the dialect's lexer and takes every name it could refer
  to as an edge (DECISIONS 315). An edge too many costs nothing but a later
  position; one too few puts a module before what it needs. The answer is
  conservative, so it can never be wrong in the dangerous direction.
- **A positional stand-in.** `after_the_rebuilds` assumes every check, filtered
  index, default and expression change calls a function the plan creates, and
  moves them all after the last create. It is right unless the assumption
  breaks a reverse edge. That is why an added column, generated or with a
  default, stays ahead: a function the plan creates may read it.

What is left to the engine is recorded on #1350, each with a two-plan remedy:
- a new column whose default or generation expression calls a function the
  plan creates or rebuilds;
- an expression change still calling a function the plan rebuilds;
- a function dropped after its release whose own body reads a column dropped
  or retyped earlier;
- a default a plan row takes calling a rebuilt function (#1030).

**The lexical over-approximation could close most of them without a parser and
without the resolver.** The lexer `creation_order_with` already uses would read
a default's or a generation expression's text for the function names it could
call. With that, `after_the_rebuilds` would move only the additions whose text
names a created function, and leave the rest in place. A column whose
expression names no created function could then stay ahead of the creates, as
it must for a function that reads it. One whose expression does name one would
follow it, which is right unless that same function also reads the column. That
last case is a genuine cycle, which the engine cannot perform in one step
either, and a named refusal is the correct answer for it.

The cost is a second consumer of the lexer, and positions that depend on text.
That is the same trade DECISIONS 315 already made for modules. It is a design
change to DEC-942.1, tracked by #1364.

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
| S: tightening a column an index or a unique constraint covers is refused at apply | fixed, wrong | #1363 |
| Content-dependent corners of expression-bearing changes around function creates, rebuilds and drops | content | #1350; the lexical over-approximation above as the structural fix, #1364 |
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
