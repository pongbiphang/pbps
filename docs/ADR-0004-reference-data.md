# ADR-0004: Declarative reference data — the `data:` block

- Status: accepted (Phase 4; the offline half is built — see "Implementation status")
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

Built, offline: the block in the model and in `Schema` equality, the loader and
`fmt` round trip, the model-level rules `validate` reports, the typed
`InsertRow` / `UpdateRow` / `DeleteRow` / `SetDataMode` changes, the
`data-update` and `data-delete` risk classes, the T-SQL DML, and the ordering —
including the foreign-key order *between* two tables that both declare rows.

Two decisions were taken during implementation that this document did not
anticipate, both of them narrowings:

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
3. **A connected plan refuses a declaration with `data:` blocks**, before it
   connects. The catalog does not read rows back yet, so its tables all say
   `data: None` — which means "declares no rows", not "did not look" — and the
   differ would insert every declared row on every run; the second apply fails
   on the primary key. The dev rehearsal compares structure only for the same
   reason, after having *run* the DML. Both are temporary and both say so;
   planning the structure and quietly leaving the rows out would be a partial
   apply nobody asked for.

Not built yet, and deliberately together because they share one question — what
the database actually holds:

- the pre-delete probe counting the foreign-key references to the row;
- reading `exact` tables' rows back into `state_json`, and the row half of the
  drift comparison — which is also what lifts the `plan --db` refusal above,
  and has to evaluate a `Cell::Default` against what the engine stored;
- `pull --data <table>`;
- `SET IDENTITY_INSERT` around an insert into an IDENTITY key column, which
  needs the emitter to know the column is one.

Until then a delete is gated as `data-delete` and the foreign key itself refuses
it loudly, which is the same protection, later.

## Placement

Phase 4. The block is an additive, optional extension of the table file, so —
unlike `strategy:`, which changed load's return type — nothing forces it to
land early.
