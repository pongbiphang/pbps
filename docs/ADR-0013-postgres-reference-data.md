# ADR-0013: Reference data on PostgreSQL — the collision ADR-0004 never recorded

- Status: proposed. Phase 5 design; nothing is built.
- Date: 2026-09-05
- Related: docs/SPEC.md §1.3, §7.5, §8.2, §12;
  [ADR-0004](ADR-0004-reference-data.md), which this revisits;
  [ADR-0012](ADR-0012-postgres-type-catalogue.md)

## Why this document exists at all

SPEC §12 says: *"ADR-0004 and ADR-0005 each already carry a recorded PostgreSQL
collision, so designing them against one decided engine is a different problem
from designing them against two open ones."*

ADR-0005 does carry one, under "Ruled out and limits". **ADR-0004 does not** —
read end to end, it contains no PostgreSQL paragraph and the word does not
appear in it. The sentence in SPEC is half true, and the missing half is the
half nobody would go looking for, because the SPEC says it is already there.

This is that record, arriving late. Two of its five findings are cases where
implementing the PostgreSQL half by following the SQL Server rule produces a
tool that is confidently wrong.

## What was measured

PostgreSQL 18.6 —
`docker.io/library/postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280`,
on 2026-09-05, in a database with `datcollate = en_US.utf8`,
`datlocprovider = c`. §2's contrast was measured too, against SQL Server 2025 —
`mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1`,
the digest CI and `scripts/live-tests.sh` pin — because the answer decided
whether that section describes a Phase 5 hazard or a bug in shipped code.
Everything not marked measured is reasoning and says so.

## 1. `NOT VALID` is not `NOCHECK`, and the pre-delete probe must not assume it is

This is the finding that would have shipped as a bug.

ADR-0004's pre-delete probe counts, before a `DeleteRow`, the foreign-key
references from other tables to that row. A recent review round refined it on
SQL Server: **skip disabled foreign keys**, because a key disabled with
`NOCHECK CONSTRAINT` remains in `sys.foreign_keys` while the engine no longer
enforces its delete action — so counting it refuses a delete that would have
succeeded.

PostgreSQL has a flag that looks exactly like that one. `pg_constraint` carries
`convalidated`, and a key added `NOT VALID` reads `convalidated = f`.

**Measured**, it means the opposite:

```
ALTER TABLE r.child ADD CONSTRAINT fk_child
  FOREIGN KEY (pid) REFERENCES r.parent(id) NOT VALID;

 conname  | convalidated
 fk_child | f

-- inserting a new violating row under that NOT VALID key:
ERROR:  insert or update on table "child" violates foreign key constraint "fk_child"
DETAIL:  Key (pid)=(998) is not present in table "parent".

-- and deleting a parent row a child points at:
ERROR:  update or delete on table "parent" violates foreign key constraint "fk_child"
DETAIL:  Key (id)=(5) is still referenced from table "child".
```

`NOT VALID` means "the rows that were already here were not checked". It does
**not** mean the constraint is off: new rows are checked, and the delete action
is enforced in full.

**Decision.** The PostgreSQL pre-delete probe counts **every** foreign key,
validated or not. `convalidated` is not consulted, and the reason is written at
the call site rather than left to be rediscovered, because the SQL Server code
one file away consults its own flag and looks like the pattern to copy.

The general shape is worth naming, since this is the second time this design
pass has hit it (ADR-0011 Amendment 2 was the first): **two engines expose a
similarly-named flag whose meanings are opposites, and the dangerous direction
is the one where the shared-looking code compiles.** Here, copying the SQL
Server rule makes the probe under-count, the plan pass preflight, and the engine
refuse the delete — which is at least loud. The mirror mistake, in a probe that
gates rather than counts, would not be.

## 2. A pinned identity key leaves the sequence behind

ADR-0004 allows a `data:` table whose key is an identity column, handled by "the
emitter wrapping the DML in `SET IDENTITY_INSERT`". PostgreSQL's equivalent is
`OVERRIDING SYSTEM VALUE`, and **measured**, it is not equivalent in the way
that matters:

```
CREATE TABLE r.pinned (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v text);
INSERT INTO r.pinned (id, v) OVERRIDING SYSTEM VALUE VALUES (1, 'pinned one');
INSERT INTO r.pinned (id, v) OVERRIDING SYSTEM VALUE VALUES (2, 'pinned two');

-- then an ordinary insert, letting the sequence choose:
ERROR:  duplicate key value violates unique constraint "pinned_pkey"
DETAIL:  Key (id)=(1) already exists.
```

The sequence never learns that 1 and 2 were used. pbps's apply succeeds, the
plan verifies clean, and **the application's next insert fails** — a failure that
appears after the deployment, in someone else's code, with nothing in the plan
that mentions it. Measured, and also plainly stated by the error: this is the
worst failure shape in the SPEC's vocabulary, the one §7.5's whole design exists
to avoid.

Two things follow, and the first matters more:

- **The emitter must restart the sequence** after pinned inserts, in the same
  transaction — **above every key the table holds and above where the sequence
  already stands**, not merely above what this plan wrote.

  A first draft of this section said "to the maximum key it wrote plus one", and
  **measured, that is worse than doing nothing**, because it can move the
  sequence *backwards*. In `ensure` mode the table keeps rows pbps does not
  declare, and one of them can hold a higher key:

  ```
  INSERT ... OVERRIDING SYSTEM VALUE VALUES (100, 'an undeclared row');
  INSERT ... OVERRIDING SYSTEM VALUE VALUES (1,   'the row this plan writes');
  ALTER TABLE v.seq ALTER COLUMN id RESTART WITH 2;   -- max(written) + 1
  INSERT INTO v.seq (v) VALUES ('the next one');       -> accepted
  ```

  The next insert is **accepted**, which is the trap: it takes id 2, and the
  collision waits until the sequence has walked back up to 100. A failure that
  arrives immediately is a bug report; one that arrives ninety-eight inserts
  later is an incident with no obvious cause. The restart value has to be
  `max(the table's own keys, the sequence's current value) + 1`, which is a
  question for the engine at apply time rather than arithmetic over the plan.
- **The declaration should be discouraged, not merely supported.** ADR-0004
  already says an identity primary key "hands out different values per
  environment, so declaring rows by id would be a lie by default". PostgreSQL
  adds a second reason. `validate` should carry a rule at `warn` naming both.

**And the contrast, measured**, because if SQL Server behaved the same way this
would be a live bug in shipped code rather than a Phase 5 note. It does not:

```
SET IDENTITY_INSERT dbo.pinned ON;
INSERT INTO dbo.pinned (id, v) VALUES (1, 'pinned one'), (2, 'pinned two');
SET IDENTITY_INSERT dbo.pinned OFF;

IDENT_CURRENT('dbo.pinned') = 2      -- the seed followed
INSERT INTO dbo.pinned (v) VALUES ('identity chooses');   -- got id 3
```

and pinning a value *above* the current seed moves it there: with three rows and
a seed of 3, inserting id 99 leaves `IDENT_CURRENT = 99` and the next ordinary
insert takes 100. SQL Server keeps the seed at least as high as the highest
value written. **So this is a PostgreSQL-only hazard, and the existing SQL
Server path is sound.**

The contrast is the reason to state the remedy as a dialect decision rather than
a shared one: the two engines differ not in what the tool must promise but in
how much of that promise the engine already keeps.

## 3. What a value looks like when read back is a property of the session

ADR-0004's connected half reads declared rows back "in the engine's spelling
(fixed CONVERT styles)" — the SQL Server emitter pins the conversion style so
that a value written and a value read compare equal.

**Measured**, PostgreSQL's rendering moves with four session settings at once:

| Value | Default session | After `SET bytea_output='escape'`, `datestyle='SQL, DMY'`, `intervalstyle='sql_standard'` |
|---|---|---|
| `'\x0102'::bytea` | `\x0102` | `\001\002` |
| `'2026-09-05'::date` | `2026-09-05` | `05/09/2026` |
| `'1 day 2 hours'::interval` | `1 day 02:00:00` | `1 2:00:00` |

and ADR-0012 §4 already measured a fifth, `TimeZone`, deciding whether a type
change rebuilds a table.

**Decision.** The PostgreSQL connection pins its session on every connect —
`bytea_output`, `DateStyle`, `IntervalStyle`, `extra_float_digits`, `TimeZone`
— and does it in `pbps-postgres`, not in `pbps-db`, because *which* settings
matter is dialect knowledge. The values a plan writes and the values it reads
back then live in one space, which is what ADR-0004 requires and what "fixed
CONVERT styles" achieves on the other engine.

**`search_path` is pinned too, and for a different reason.** Every name pbps
emits is schema-qualified, so it needs no search path — but a `search_path` an
operator left pointing at their own schema silently changes which object an
*unqualified* name in a default expression or a function body resolves to. Pin
it to something empty and explicit rather than inheriting whatever the role has.

## 4. A default comes back with a cast welded on

ADR-0004's read-back rule: *"a cell equal to its column's default is read back
omitted, so the omitted spelling round-trips."* That needs the default's value,
and on PostgreSQL what the catalog stores is a deparsed expression:

**Measured**, for `label text DEFAULT 'unnamed'`, `n int DEFAULT 0`,
`c varchar(9) DEFAULT 'x'`:

```
 label | 'unnamed'::text
 n     | 0
 c     | 'x'::character varying
```

A declared `'unnamed'` is stored as `'unnamed'::text`. Comparing the declaration
to the catalog's text will never match for any string or typed literal, only for
bare numerics.

This is the same family as ADR-0009 §2 — the engine deparses and hands back its
own spelling — and the answer is the same one §8.2 already gives: **do not
compare texts across the boundary; ask the engine for the value.** The probe
machinery already evaluates expressions server-side to decide whether a cell
equals its default (ADR-0004's implementation notes, and the "unprobeable
default" path of DECISIONS 117 for the cases where it cannot). That path is
dialect-agnostic in shape and carries over; what does not carry over is any
attempt to shortcut it with string equality, which happens to work often enough
on SQL Server to look like it works.

## 5. The row map's identity is the engine's equality, and PostgreSQL answers differently

ADR-0004 makes the primary-key value the row's identity — inviolable constraint
2 at row granularity. Whether two keys are one row is therefore the engine's
question, and **measured**, PostgreSQL's default answer differs from SQL
Server's default:

```
CREATE TABLE r.keys (code varchar(20) PRIMARY KEY);
INSERT INTO r.keys VALUES ('New');
INSERT INTO r.keys VALUES ('new');
 rows_for_New_and_new
                    2
```

Both rows exist: `en_US.utf8`, a deterministic collation, is case-sensitive. On
SQL Server's usual case-insensitive default those two declarations collide, and
this repository already has the machinery for it — a review round added judging
key collisions by the key column's own collation.

**So this one needs no new mechanism, and that is the finding.** The question
"are these two declared keys the same row" is already asked of the engine rather
than answered in the model, so PostgreSQL simply returns a different answer.
Two notes for the implementation:

- PostgreSQL's answer comes from the column's collation and
  `pg_collation.collisdeterministic`; a **nondeterministic** ICU collation (PG
  12+) makes `'New'` and `'new'` one row, which is SQL Server's usual behaviour
  arriving on PostgreSQL as an opt-in. Both answers must be reachable; neither
  may be assumed.
- A `data:` block valid on one engine can therefore be refused on the other.
  That is correct — the declaration means "these are distinct rows", and an
  engine that cannot hold them distinct must say so — but it is the first place
  where a portable-looking declaration file is not portable, and `validate`'s
  message should say which engine's collation refused it and why.

## 6. What needs no change, stated so nobody reaches for it

PostgreSQL has `INSERT … ON CONFLICT` and, since 15, `MERGE`. Neither is
adopted. ADR-0004's design emits **typed** `InsertRow` / `UpdateRow` /
`DeleteRow` with stale-row guards in their predicates, and the guards are the
point: they are what makes the plan refuse a row another session changed after
the plan was reviewed. An upsert collapses insert-or-update into one statement
whose outcome depends on the state at execution time — which is exactly the
review the guards exist to enforce, discarded for a shorter statement. This is
SPEC 14.3's shape, and it will arrive as a reasonable suggestion.

## What this changes

| | |
|---|---|
| `pbps-model` | Nothing |
| ADR-0004's design | Nothing structural; §2 adds a statement to the emitter's identity path, §3 adds a connect-time session pin |
| The pre-delete probe | A PostgreSQL rule that is **not** the SQL Server rule (§1) |
| `validate` | Two rules: an identity-keyed `data:` block warns (§2); a key collision names the collation that decided it (§5) |

## Limits

- **Composite keys are still deferred**, as in ADR-0004.
- **Nondeterministic collations were not measured** (§5), only their existence
  reasoned from the catalog's `collisdeterministic` column.
- **Everything here is proposed**, and falsifiable by the PostgreSQL live suite.

## Placement

Phase 5, with the emitter and the probes. Nothing here needs to land sooner:
§2's one candidate for being a live bug was measured and is not one.

One item is not Phase 5 work at all and should be fixed whenever SPEC §12 is
next edited: that sentence claims a PostgreSQL collision is recorded in ADR-0004,
and until this document existed, none was.
