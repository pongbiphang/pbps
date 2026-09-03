# ADR-0004: Declarative reference data — the `data:` block

- Status: accepted (Phase 4; built — see "Implementation status")
- Date: 2026-08-31
- Related: docs/SPEC.md §1.3, §7.5, §8.2, §12;
  [ADR-0002](ADR-0002-module-model.md);
  [ADR-0005](ADR-0005-roles-and-grants.md) (the generalized identity criterion)

## Background

Lookup tables (order statuses, country codes, role kinds) sit in a gap: the
application logic references specific rows by code, so a bootstrapped
environment without those rows is broken even though every structural object
exists. Atlas shipped this as "Declarative Data Management" in v1.1 (2026),
Pro-gated.

SPEC §1.3 excludes data transformation — but its rationale ("how data should
be moved is a business decision and cannot be derived from a structural diff")
targets *transforming existing business data*. Declaring "this lookup table's
contents are exactly these rows" is not a transformation; it is desired state,
the same semantics as declaring a column.

## The line: reference data is code, business data is history

Reference rows are part of the program — the application references them the
way it references a column name. Business rows are user history. The boundary
is made mechanical: **the tool touches no table's rows unless the table has a
`data:` block.** §1.3's stance on business data does not move an inch; the
block is the explicit opt-in.

## Format

```yaml
# schema/dbo.order_status.yml
table: dbo.order_status
columns:
  code:  {type: varchar(20), nullable: false}
  label: {type: nvarchar(50), nullable: false}
primary_key: [code]

data:
  mode: exact          # exact | ensure
  rows:
    new:       {label: New}
    shipped:   {label: Shipped}
    cancelled: {label: Cancelled}
```

- **Rows are keyed by primary-key value** — inviolable constraint 2 reused at
  row granularity: the map key is the identity, so a duplicate key is
  unwritable. The key column's value is not repeated in the row body.
- **A `data:` table must have a declared, single-column primary key.**
  Composite keys are deferred. An IDENTITY primary key hands out different
  values per environment, so declaring rows by id would be a lie by default;
  tables that genuinely need pinned ids are handled by the emitter wrapping
  the DML in `SET IDENTITY_INSERT` — SQL still appears exactly once, in the
  emitter.
- **Unlike `strategy:` (ADR-0003), the block belongs in the model.** Declared
  rows are desired state, visible in the database, so they participate in
  `Schema` equality and in the drift comparison. Constraint 1 is satisfied,
  not threatened.

## Two modes

| Mode | Meaning | DELETE emitted? |
|---|---|---|
| `exact` | The declared rows are the whole table; an extra row is drift | Yes (gated) |
| `ensure` | Declared rows must exist with the declared values; other rows are ignored (a seeded core in an app-writable table) | Never |

This is §8.2's "drift compares the managed set only", applied at row
granularity.

## Changes and risk

- The ChangeSet gains typed row changes: `InsertRow` / `UpdateRow` /
  `DeleteRow`. The emitter renders DML; the plan's transaction covers DDL and
  DML together.
- New risk classes `data-update` and `data-delete`, gated behind `--allow`;
  inserts are safe.
- The preflight probes of §7.5 extend for free: before a `DeleteRow`, count
  the foreign-key references from other tables to that row and abort with the
  real number.
- The differ orders data changes with structure: create table → insert rows →
  add the FK that references them.

## Drift and state

After a successful apply, `exact` tables' rows are read back into
`state_json` — the same "the database is the normalizer" principle — and
`verify` compares them; `ensure` tables compare declared keys only.

Guardrail: `validate` warns when a `data:` block exceeds a configurable row
count (default 1000) — "this does not look like reference data". The
philosophy is enforced by the tool, not left to user discipline.

## Primary-key value changes

A changed key is delete + insert, gated as `data-delete`. No row-level rename
intent: by the generalized criterion (ADR-0005), a row's entire content is
declared, so recreation is lossless — and the genuinely dangerous case,
business rows referencing the old key, is blocked loudly by the FK itself and
goes through the existing escape hatch (a DBA runs the SQL, then
`pbps baseline`). If real demand for key renames appears, the three intent
channels can be extended; not now.

## Adoption

`pull --data <table>` reverse-generates the block from an existing table, the
same onboarding story as structure.

## Implementation status

Built: the block in the model and in `Schema` equality, the loader and `fmt`
round trip, the model-level rules `validate` reports, the typed `InsertRow` /
`UpdateRow` / `DeleteRow` / `SetDataMode` changes, the `data-update` and
`data-delete` risk classes, the T-SQL DML, the ordering — including the
foreign-key order *between* two tables that both declare rows — and the
connected half: the row read-back into `state_json`, the row half of the drift
comparison, the pre-delete probe and `pull --data`.

Decisions taken during implementation that this document did not anticipate:

1. **A bare non-integer number is refused**, with a diagnostic asking for
   quotes. There is no floating-point arm in the model's `Value` at all: `f64`
   is not `Eq`, which `Schema` equality needs, and passing `1.10` through a
   binary float does not promise to give `1.10` back — a declaration that
   disagrees with its own database on every plan is worse than no declaration.
   Quoted, it is text, and the engine converts it.
2. **An omitted column means the column's default, or NULL** — not "unspecified".
   The alternative would leave part of an `exact` table undeclared, which is
   exactly what `exact` claims not to be, and would undermine this document's
   own argument that a row can be recreated losslessly. The two spellings are
   different statements to the engine: an omitted column is left out of the
   INSERT and the default fills it; an explicit `null` is *sent*, and no
   default applies to a value that was sent. So the differ compares a `Cell`
   (a value, or *the default*) resolved against each side's own table, an
   UPDATE to an omitted column says `DEFAULT`, and `validate` refuses an
   explicit `null` on a NOT NULL column whatever its default. The cell carries
   the default *expression*, compared as text: when the default changes, a row
   that omits the column should hold the new one, and `ALTER` does not
   backfill. (A read-back that spells the default the engine's way will
   therefore restate `DEFAULT` on every plan until the declaration is written
   in the stored form — the same spelling cost check constraints already
   have, and the same remedy.)
   The key column is never compared: its value is the map key, and resolving
   it through the omission rule read a default added to the key column as
   "set every key to DEFAULT". Rows are matched to the base by column **uid**,
   so a renamed column keeps its values; a primary key that *moves* to a
   different column is refused, because the two key sets have nothing in
   common.
3. **The catalog reads rows back under a scope the caller supplies.** A
   database holds rows, not a notion of which of them are declared, so every
   connected command says which tables' rows it wants and how: every row of an
   `exact` table, the declared keys of an `ensure` one. `verify`, `status` and
   `apply`'s drift check use the **recorded** state's scope — the question is
   whether the environment moved since pbps last recorded it; `snapshot`,
   `baseline` and `bootstrap` use the declarations'; `plan --db` reads the
   **union** of the two once and projects each view out of it, so a block that
   was added, removed or switched between modes since the last state is seen
   by both the drift check and the differ. The saved plan carries the
   declarations' scope (`data`), for the same reason it carries its `ids`:
   `apply` records the database read back, and needs no checkout to know which
   rows to read.
4. **Values come back in the engine's spelling, and a cell that holds its
   default is read in the spelling of whoever reads it.** Every cell is
   rendered by the server (`CONVERT` with a fixed style; `bit` and the
   integer types are the only ones read back typed), so both sides of a drift
   check see one spelling and a declaration that wants to match writes it
   that way — `pull --data` shows it. The catalog cannot tell `label:
   Unlabelled` from an omitted `label` when the default is `'Unlabelled'`, so
   the read reports *both*: the value, and whether it equals the default
   (`ObservedRow`). The side that looks at the row then chooses — a cell it
   writes explicitly stays explicit, a cell it omits and that is at its
   default is omitted — so each spelling round-trips and neither is restated
   on every connected plan. (The first cut folded the choice into the read,
   "equal to the default means omitted", and an explicit value equal to its
   default was restated forever.) Only a **literal** default is compared:
   `SYSUTCDATETIME()`, `NEWID()` or `NEXT VALUE FOR` are never put in the
   query, because the `CASE` that compares would run them once per row —
   and `NEXT VALUE FOR` is not even legal there. Such a cell is taken at the
   declaration's word: at its default where the row omits it, the stored
   value where the row spells it, and the stored value again where there is
   no declaration to consult (`pull`), since a generated key is a value the
   block has to carry (DECISIONS 80); a hand edit to an omitted cell of that
   kind is not seen, and the remedy for a column that matters is to write
   the value. A table whose live key is not a single column is **unreadable**,
   and the read fails rather than answering "no rows".
   A binary column (`binary`, `varbinary`, `image`, `timestamp`) cannot hold
   a declared value at all: row values travel as string literals, and the
   engine's text-to-binary conversion stores the characters, not the bytes.
   A `sql_variant` column cannot either, the other way round: the text goes
   in, but the variant's base type does not come back out; nor can a spatial
   one, whose text has no SRID (90). `validate` refuses each as a key and as
   a set cell (DECISIONS 70, 87, 90), and refuses a
   scalar of the wrong kind for its column — a bare `1` in a `varchar`, a
   quoted `"1"` in an `int` — because it would read back as another kind
   and drift on every plan (87). A typed value in the model is the way to
   lift those, and it is an ADR of its own. A key the engine spells differently from the
   declaration (`01` for an `int` `1`) is read back under the declaration's
   spelling: the read sends each declared key through a `VALUES` join and
   the engine says which row it names (DECISIONS 71), so neither side ever
   invents a second normalizer; two declared spellings of one row are
   refused rather than reconciled (74). The pre-delete probe counts a child
   row the plan updates unless the update sets the referencing column
   itself (73).
   A non-key `IDENTITY` column is the engine's and is never read back: a
   declaration cannot set it and an `UPDATE` cannot change it, so both sides
   omit it and omission agrees with omission (DECISIONS 94); the key is the
   one identity a row may pin.
5. **A table the declarations take over is measured against what it holds.**
   The first connected plan for a table with a new `data:` block reads its
   rows, updates the ones that differ, inserts the ones missing, and — for
   `exact` — deletes the ones nobody declared, behind the gate. `plan --db`
   says so on stdout, because the plan shows the consequences and not the
   takeover.
6. **The pre-delete probe finds the referencing tables in the catalog at run
   time**, through `sys.foreign_keys` and dynamic SQL, rather than trusting
   the declarations to list them — a foreign key someone added by hand is
   exactly the one that will refuse the delete. `ON DELETE CASCADE` children
   are counted too: the engine would not refuse that delete, it would take
   the child rows with it, which is the disaster the `data-delete` gate is
   for. Rows the same plan updates or deletes are left out of the count, so a
   child moved to a new parent in the same revision does not refuse the plan
   that the ordering was designed to make acceptable; an over-exclusion fails
   loudly in the transaction instead.

The live tests cover the whole path: the DML (`reference_data_reaches_the_engine_in_an_order_it_accepts`),
the read-back, the drift on rows, the `ensure` read staying inside its keys,
the probe's dynamic SQL and a keyless table refusing to be read
(`declared_rows_read_back_as_declared_and_hand_edits_are_seen`), and the
binary end to end — bootstrap, verify, a hand edit, a connected plan, apply,
`pull --data` (`reference_data_round_trips_through_a_real_target`).

An `IDENTITY` key is pinned by wrapping the insert in `SET IDENTITY_INSERT ...
ON` / `OFF` as a single statement, so the switch — a session setting at most one
table may hold — is off again before the next table's insert. Only the key may
pin an identity value; `validate` refuses a row that writes any other IDENTITY
column.

## Placement

Phase 4. The block is an additive, optional extension of the table file, so —
unlike `strategy:`, which changed load's return type — nothing forces it to
land early.
