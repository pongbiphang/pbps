# ADR-0002: The module model — views, procedures, functions, triggers

- Status: accepted; built in Phase 3.5 for SQL Server
- Date: 2026-08-30
- Related: docs/SPEC.md §1.2, §7.4, §8.2, §12, §13.6

## Background

The v1 scope deferred views, stored procedures, functions and triggers, noting
only that for these objects "the definition simply *is* the latest version",
which resembles repeatable migrations more than the identity-tracking model
used for columns (SPEC §1.2, open question 6). The design direction was left
open.

The pressure to close it is adoption. Real SQL Server estates are full of these
objects, and a tool that cannot manage them forces users to keep a second tool
for half their schema — which halves the value of adopting the first one.

## The dividing principle

**The identity machinery exists because columns carry data.** Uids, tombstones
and rename intent all serve one purpose: preventing a rename from being
mistaken for a drop, because a wrong guess destroys data irreversibly.

Views, procedures, functions and triggers carry no data. Dropping and
recreating one is semantically lossless, and the complete definition lives in
git. They therefore do **not** get the identity machinery — not as an economy
measure, but as a principled line:

| Object family | Model | Rename | Drop audit |
|---|---|---|---|
| Data-bearing (tables, columns) | Identity tracking: uids, tombstones, human intent | Explicit intent, recorded in the ids file | Tombstone with a reason |
| Definition-is-the-object — "modules" (views, procs, functions, triggers) | The declared definition is the desired state | Drop + add (lossless, documented) | Git history |

Modules never appear in `schema.ids.json`.

([ADR-0005](ADR-0005-roles-and-grants.md) later sharpened this criterion to
"does drop + add destroy state that lives only in the environment?" — the
answer for modules is unchanged.)

## Declaration format

One module per file; as with tables, the file name carries no meaning and the
identity comes from the kind field (`view:` / `procedure:` / `function:` /
`trigger:`; triggers additionally name their table with `on:`).

```yaml
# schema/dbo.active_customer.view.yml
view: dbo.active_customer
description: Customers that are not legacy records
definition: |
  SELECT customer_id, full_name
  FROM dbo.customer
  WHERE legacy_code IS NULL
```

The emitter composes the full CREATE statement from the fields; `definition`
holds only the body. SQL still appears exactly once, in the emitter.

`validate` gains one rule: a module name may not collide with a table or with
another module — SQL Server keeps them all in one `sys.objects` namespace per
schema, so the collision would otherwise surface only at apply time.

## Comparison and drift

The principle is the one already paid for in SPEC §8.2: **expressions are never
parsed; the database itself is the normalizer.** After a successful apply the
stored definition is read back from `sys.sql_modules` into `state_json`, and
both sides of the drift check live in the database's stored-text space.

The luck is better here than for constraints: `sys.sql_modules` stores the
definition **verbatim** (SQL Server does not rewrite it the way it rewrites
check-constraint expressions), so the round-trip is near-exact and the drift
false-positive risk is lower than the constraint case, not higher.

On the differ's side (declaration versus baseline), the same lightweight
normalization applies; anything still different after it is re-emitted as
`CREATE OR ALTER` — idempotent and lossless, the module analogue of §8.2's
"the cost of a false positive is rebuilding one constraint".

Limit: a module created `WITH ENCRYPTION` has no readable definition and
cannot be managed. `pull` reports it and leaves it unmanaged.

## Changes and risk

The ChangeSet gains `CreateModule` / `AlterModule` / `DropModule`.

- **Emit `CREATE OR ALTER`** (SQL Server 2016 SP1+) for create and alter. It
  is idempotent, and — unlike drop + create — it **preserves permissions**, a
  known DACPAC pain point worth naming in comparisons.
- **`AlterModule` needs no gate.** A failed statement rolls back with the
  plan's transaction (§7.5); the environment is unchanged.
- **`DropModule` is classified `destructive`** and faces the usual `--allow`
  gate — what it destroys is the validity of dependents, not data — but it
  requires **no tombstone and no reason**: the full definition is in git
  history, which is the audit trail for this object family.
- **Renames are drop + add**, face the same gate, and are documented as such.
  The §7.4 impact machinery (`sys.sql_expression_dependencies`) already
  answers "who references this" before anything runs.

## Ordering

Views referencing views must be created in dependency order. §8.2's "never
parse" is a drift-comparison constraint and is not violated by a best-effort
**identifier scan**: the definition text is scanned for the names of managed
objects only — no SQL semantics — to produce a topological order.

The failure mode is safe by construction: a wrong order fails the CREATE
inside the plan's transaction, everything rolls back, and the environment is
unchanged. The escape hatch is an explicit `depends_on:` list in the module
file.

### Known limitation: an alter that releases a schema-bound dependency

Module changes bracket the table changes: drops first, creates and alters
last. "Alters last" is right for the common case — a view that starts
selecting a column the same plan adds must be restated *after* the column
exists — and wrong for one case that also occurs:

> A `WITH SCHEMABINDING` function references `customer.phone`. One revision
> both rewrites the function to stop referencing it **and** drops the column.
> The engine refuses the drop while the old, still schema-bound definition
> stands, so the alter has to run **first**.

One ordering rank cannot serve both, and a plan may contain one of each.
Telling them apart needs two things the model does not have: whether the
module is schema-bound (a SCHEMABINDING *view* has nowhere to keep the option
and is inventoried as unmanaged, so only functions reach here carrying it),
and a comparison of the old and new definitions' references against the tables
this plan touches. `depends_on:` cannot express it either — it orders modules
among themselves, not against table changes.

**Not fixed, deliberately.** The fix that works is to stop using
`CREATE OR ALTER` for such a module and bracket the table changes with a drop
and a create, which is already how a module rename is ordered. That destroys
the object's GRANTs — the DACPAC pain point this ADR chose `CREATE OR ALTER`
to avoid — and pbps does not record permissions, so it could neither warn
about the loss nor put them back (they arrive with roles and grants, ADR-0005).

The trade is therefore between two failure modes, and the current one is the
better of the two:

| | today | with drop + create |
|---|---|---|
| when it fails | at apply, loudly | after apply, at the next query |
| database | rolled back, untouched | changed, GRANTs gone |
| what the tool reports | an error | success, and no drift |
| recovery | none needed | a human, from a backup |

An apply that succeeds, verifies clean, and leaves the application unable to
read its own view is worse than one that refuses. Revisit this with ADR-0005,
when permissions are something pbps can carry across a drop.

## Onboarding

Phase 2's `pull` must inventory modules **before** management lands: they are
reported as unmanaged (the `unmanaged: warn` setting naturally covers them).
A pull that silently ignores half the database breaks the adoption story that
justifies pull in the first place.

## Costs and limits

- **MSSQL-first.** PostgreSQL identifies a function by name **plus argument
  types** (overloading), so "the name is the identity" needs revisiting in
  Phase 5. Flagged now as a touchstone item for the `Dialect` abstraction.
- **GRANTs stay unmanaged** (open question 7); `CREATE OR ALTER` merely avoids
  destroying them.
- **Only DML triggers on managed tables.** DDL and server-level triggers are
  out of scope.
- **Encrypted modules are unmanageable** (above).

## Placement

Phase 3.5 — the model needs the emitter and apply machinery of Phases 2–3.
The `pull` inventory of unmanaged modules lands earlier, with Phase 2.
