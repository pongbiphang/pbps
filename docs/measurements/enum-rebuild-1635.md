# #1635 slice 3b: dependents of an enum column, measured on PG 16.15 and 18.6

The *Handling* column is what the implementation does (DEC-1635.7). The restores of checks, foreign
keys and indexes ask for the `constraint` and `destructive` classes beside `type-rebuild`, as they do
in any plan that drops and re-adds them.

Date: 2026-10-25. Containers `pbps-test-pg16` (16.15) and `pbps-test-pg` (18.6), scratch
database `m1635`, one schema per scenario. Harness: `gate1635d` scratchpad `meas/gen.py`
(setup, then `CREATE TYPE e_new AS ENUM ('a','c')`, retype, `DROP TYPE e`,
`ALTER TYPE e_new RENAME TO e`, then a probe in a **fresh session**). Rows never held the
removed label unless the row says so. 16 and 18 gave identical answers except where marked.

Retype statement used everywhere: `ALTER TABLE t ALTER COLUMN c TYPE e_new USING c::text::e_new`
(`c::text::e_new[]` for an array column; `c::text[]::e_new[]` measured equal).

Legend of the last column:
* **PG** — the engine rebuilds it during the retype; pbps emits nothing extra.
* **drop/restore** — the retype (or `DROP TYPE`) is refused while it stands; pbps drops it
  before the retype and restores it from the **declaration** afterwards, in the plan.
* **module rebuild** — same, through the existing module drop-and-recreate (`weave`).
* **refuse** — pbps refuses by name at plan time, before any SQL.

| Dependent kind | 16 / 18 result | Engine message when left standing | Handling |
|---|---|---|---|
| plain enum column | retype ok | — | retype |
| array column `e[]` | retype ok (`::text::e_new[]`) | — | retype |
| labels needing array quoting (`a b`, `c,d`, `e"f`, `{g}`, `\h`) | ok in an array | — | retype |
| row holding a removed label | **cast fails** (scalar and array element) | `invalid input value for enum e_new: "b"` | plan-time row probe, refuse with per-column count |
| DEFAULT (`'a'` or `'a'::e`) | refused | `default for column "c" cannot be cast automatically to type e_new` | drop/restore (declared default) |
| CHECK `c IN ('a','c')`, `c <> 'b'` | refused | `operator does not exist: e_new = e` (stored constants are typed `e`) | drop/restore (declared check) |
| CHECK `c::text <> 'zz'` | ok | — | PG (but dropped/restored anyway: expression text is not parsed) |
| view over the column | refused | `cannot alter type of a column used by a view or rule` | module rebuild (a declared view, with the views over it by `weave`); an undeclared view: refuse |
| materialized view, user rule over the column | refused (a matview's indexes too) | `cannot alter type of a column used by a view or rule` | refuse (no declaration holds them) |
| view over other columns only | ok | — | PG |
| SQL function, string body | ok, works after | — (no dependency recorded) | PG |
| SQL function `BEGIN ATOMIC` / `RETURN` body reading the column | refused | `cannot alter type of a column used by a function or procedure` | module rebuild |
| PL/pgSQL body naming the type (`x e := 'a'`) | ok; works from a new session | — | PG |
| plain index; INCLUDE index; unique; PK; exclusion constraint; gin on `e[]` | ok, rebuilt by the engine | — | PG |
| partial index `WHERE c = 'a'`; expression index `((c = 'a'::e))` | refused | `operator does not exist: e_new = e` | drop/restore (declared index, expression keys included); an undeclared one: refuse (the plan does not drop it) |
| FK enum → enum (either retype order, or one ALTER) | refused | `foreign key constraint "…" cannot be implemented` | drop/restore both ends (declared FK) |
| generated column reading `c` | refused | `cannot alter type of a column used by a generated column` | refuse |
| generated column **of** the enum type | refused | `cannot specify USING when altering type of generated column` | refuse (virtual enum columns do not exist on 18: `virtual generated column "g" cannot have a user-defined type`) |
| RLS policy over `c` | refused | `cannot alter type of a column used in a policy definition` | refuse (a rebuild does not restore policies) |
| trigger `WHEN` / `UPDATE OF c` | refused | `cannot alter type of a column used in a trigger definition` | module rebuild (a declared trigger) |
| trigger with no column reference | ok | — | PG |
| extended statistics (columns or expression) | ok, kept | — | PG |
| publication column list / row filter | refused (filter on an enum column cannot even be created) | `cannot alter type of a column used by a publication WHERE clause` | refuse (not managed) |
| column comment, column ACL, NOT NULL, replica identity, statistics target | kept | — | PG |
| **type** comment | **lost** on the replacement | — | carried: read at plan time, kept in the plan payload, `COMMENT ON TYPE` re-emitted on the replacement after the rename back (maintainer, 2026-10-24) |
| **type** owner other than the deployer / non-empty `typacl` | **lost** on the replacement | — | refuse by name (maintainer, 2026-10-24) |
| partitioned root, DEFAULT/CHECK on root | root retype propagates to all partitions | default/check as above | retype the root once |
| partition retyped directly | refused | `cannot alter inherited column "c"` | never emitted |
| partition with its own DEFAULT or CHECK | refused until dropped | default message | drop/restore |
| partition key on the enum column | refused | `cannot alter column "c" because it is part of the partition key` | refuse (already unreachable: DEC-1523.1) |
| inheritance child (non-partition) | retype recurses | — | PG |
| composite type attribute (no table uses it) | `DROP TYPE e` refused until `ALTER TYPE comp ALTER ATTRIBUTE x TYPE e_new` | `cannot drop type e because other objects depend on it` | refuse |
| composite type attribute used by a table column | attribute retype refused | `cannot alter type "comp" because column "t.c" uses it` | refuse |
| domain over the enum (and a column of it) | `DROP TYPE` refused | `cannot drop type e because other objects depend on it` | refuse (slice 4) |
| function / procedure signature using the type (arg, return, array arg, default arg) | `DROP TYPE` refused | same | refuse (follow-up issue) |
| range type over the enum; binary-coercible cast | `DROP TYPE` refused / cast cannot be created (`enum data types are not binary-compatible`) | same | refuse |
| temporary name taken (`e_new` exists) | `CREATE TYPE` refused | `type "e_new" already exists` | chooser skips taken names |
| array type name | `_e_new` is renamed with `e_new` | — | PG |

Notes

* A stale plpgsql plan: a PL/pgSQL function that declares a variable of the type and was
  *compiled earlier in the same session* fails after the rebuild (`cache lookup failed for type …`);
  a fresh session is fine. pbps never calls such a function between the retype and the end of the
  plan, and rebuilt routines are only recreated, not called.
* `typacl`, `obj_description` and `typowner` of the type are not copied by `CREATE TYPE`.
  Decided by the maintainer on 2026-10-24: the comment is carried (read at plan time, stored in
  the plan payload, `COMMENT ON TYPE` after the rename back); a non-default owner or a
  non-empty `typacl` is refused by name.
* Dependency census: the plan-time refusal is **catalog-driven** (every `pg_depend` edge on the
  type, its array type and the columns that use them), not a hand-written list. An edge whose
  kind is not in the "retype", "PG", "drop/restore" or "module rebuild" rows above is refused by
  name, so the engine can never abort mid-plan on a kind this table does not list.
