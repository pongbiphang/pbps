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

- **The construct is refused on PostgreSQL, and the rest of this bullet is why.**
  What follows is six measured obstacles in the order they were found, each one
  the answer to the last — and the decision they arrive at is at the end of the
  bullet, not here. Nothing in between is a design to implement; every
  imperative in it was superseded by the next paragraph.

  The first attempt: **restart the sequence** after pinned inserts, in the same
  transaction — above every key the table holds and above where the sequence
  already stands, not merely above what this plan wrote.

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

  **And asking the engine is not enough on its own: the table has to be locked
  before the question, not by it.** Reading `max(id)` and then restarting is a
  time-of-check-to-time-of-use race against an application that is still
  inserting — being inside the plan's transaction does not close it, because
  MVCC lets the other session commit in between. **Measured**, the locks arrive
  in the wrong order:

  ```
  BEGIN;
  SELECT max(id) FROM m.lockseq;                        -- AccessShareLock
  ALTER TABLE m.lockseq ALTER COLUMN id RESTART WITH 2; -- AccessExclusiveLock
  ```

  The lock *modes* are measured; that `AccessShareLock` does not conflict with
  the `RowExclusiveLock` an `INSERT` takes is PostgreSQL's documented conflict
  table, which is reasoning and is marked as such. The two together are the
  window: nothing the read holds keeps another session out, and the lock that
  would is taken only when the restart runs.

  So the emitter takes an exclusive table lock **first**, before reading either
  value. **Measured**, with `LOCK TABLE … IN EXCLUSIVE MODE` held by another
  session, an ordinary `INSERT` blocks until it times out. That closes the
  ordinary path.

  **It does not close the other one, and no lock does.** A session can call
  `nextval()` on the identity's sequence directly, and — **measured** — that
  needs nothing the table lock holds:

  ```
  -- while another session holds LOCK TABLE z.t IN EXCLUSIVE MODE:
  INSERT INTO z.t (v) VALUES ('via INSERT');   ERROR: canceling statement due to lock timeout
  SELECT nextval('z.t_id_seq');                 2
  ```

  and there is nothing to escalate to, because PostgreSQL refuses to lock a
  sequence at all:

  ```
  LOCK TABLE m.lockable_id_seq IN ACCESS EXCLUSIVE MODE;
      refused: cannot lock relation "lockable_id_seq"
  SELECT last_value FROM m.lockable_id_seq FOR UPDATE;
      refused: cannot lock rows in sequence "lockable_id_seq"
  ```

  `nextval` is non-transactional by design; that is the property the whole
  sequence mechanism is built on, and it is not something a caller may opt out
  of. **So the race cannot be closed by exclusion, and a design that says
  "take a lock that conflicts with allocation" is prescribing something the
  engine does not offer.**

  **Decision: make the write unable to do harm instead of making it exclusive.**
  A `nextval` only ever moves a sequence forward, so the only damage pbps can do
  is move it *backwards*.

  A first version of this paragraph proposed
  `setval(seq, GREATEST(<target>, nextval(seq)))` and called it atomic. **It is
  not, and measured, it produces exactly the duplicate this section exists to
  prevent** — `nextval` and `setval` are two operations, and one allocation
  between them is enough:

  ```
  sequence stands at 104
  inner nextval returns          105
  another session then allocates 106
  setval(GREATEST(101, 105))     105     -- writes the inner value back
  the next caller receives       106     -- which the other session already holds
  ```

  The lesson is narrow and worth keeping: `GREATEST` made the expression *look*
  monotonic, and monotonicity has to be a property of every step, not of the
  value being written.

  **So: never `setval`. Advance with `nextval` until the value is past every key
  in use.** Every operation then moves the sequence forward and nothing pbps
  does can lower it. The work is bounded by the gap, which for a reference-data
  table is the handful of rows the declarations pin.

  **That closes the reissue hazard and not the outstanding one, and nothing
  closes the second.** An earlier version of this paragraph claimed no
  interleaving could produce a duplicate. **Measured, with two sessions**, one
  still can — because a value already handed out cannot be recalled:

  ```
  the sequence stands at 4; pbps locks the table and pins id 5
  another session calls nextval() directly       -> it receives 5
  pbps advances the sequence past the pinned row and commits
  that session then inserts the value it was given:
      ERROR:  duplicate key value violates unique constraint "t_pkey"
      DETAIL:  Key (id)=(5) already exists.
  ```

  The advance stops pbps from issuing 5 again; it cannot un-issue the 5 the
  other session is already holding. And the table lock does not help, because —
  measured two rounds earlier — `nextval` walks past it, and PostgreSQL offers
  no lock that does not.

  **And two more obstacles finish this off, both measured.** The advance is
  itself outside the transaction the whole apply depends on:

  ```
  BEGIN; five nextvals; ROLLBACK;
      last_value = 5 — the advance is still there
  ```

  A sequence is non-transactional — the property this design was *relying* on
  for monotonicity — so a plan that fails at a later statement rolls back the
  pinned rows and the state entry and **leaves the advance behind**. SPEC §7.5
  promises the environment is unchanged after a failed apply; it would not be,
  and each retry would consume another range.

  And the loop is not guaranteed to terminate. On a sequence someone has
  altered to `CYCLE`, `nextval` is not monotonic at all:

  ```
  a CYCLE sequence with MAXVALUE 10, starting at 8:  8, 9, 10, 1, 2, 3
  ```

  If the table holds the maximum, "advance until past every key in use" walks
  for ever over values already taken. `pg_sequence` exposes `seqcycle`, `seqmin`
  and `seqmax`, so this is detectable — and `Identity` records only a seed and
  an increment, so it is not otherwise refused.

  ### The decision: refuse an identity-keyed `data:` block on PostgreSQL

  That is six independent obstacles on one feature, every one of them measured
  on this branch:

  | | |
  |---|---|
  | 1 | a pinned insert does not advance the sequence |
  | 2 | no lock conflicts with `nextval`, and a sequence cannot be locked at all |
  | 3 | `setval` cannot be made atomic with the read that chooses its argument |
  | 4 | an allocation already handed out cannot be recalled, so the operation needs quiescence |
  | 5 | the advance survives a rollback, so a failed apply leaves the environment changed |
  | 6 | a `CYCLE` sequence makes the advance non-monotonic and non-terminating |

  Four rounds of this document tried to engineer around 1–4 and each fix was
  correct about the hazard in front of it. What 5 and 6 make plain is that the
  feature does not fit inside the promises the tool is built on: §7.5's
  all-or-nothing, and a plan whose effects are exactly what a reviewer approved.
  A construct that requires the application to be quiesced and leaves debris
  after a rollback is not a construct this tool can offer, however carefully the
  statements are ordered.

  **So `validate` refuses a `data:` block whose key is an identity column on
  PostgreSQL**, naming the sequence and pointing at the two ways forward: declare
  the rows by a natural key, or place them outside pbps and `pbps baseline` —
  which is the escape hatch ADR-0004 already names for the analogous case of a
  key that has to change.

  ADR-0004 says an identity primary key "hands out different values per
  environment, so declaring rows by id would be a lie by default". On this engine
  that stops being a stylistic objection and becomes a mechanical one, and the
  house preference decides the rest: **prefer making a failure unrepresentable
  over handling it.** Refusing removes all six at once — no advance, no loop, no
  lock, no precondition an operator has to remember.

  The rest of this bullet is kept because it is the case for the refusal, not a
  design still to be built.

  Widening the step first — `ALTER SEQUENCE … INCREMENT BY <gap>`, one `nextval`,
  then restore — makes the common case a single call, and **measured**, it is
  safe to combine with the rule because `ALTER SEQUENCE` does not move
  `last_value` in either direction:

  ```
  before ALTER: last_value 101
  after  ALTER: last_value 101 — nothing was lowered
  ```

  It is an optimisation and not the primitive, because it has a corner:
  **measured**, on a sequence that has never been called (`is_called = false`)
  the first `nextval` returns the start value and ignores the widened
  increment — `first nextval 1, second 101`. A loop that checks the value it
  received is what makes the rule true; the widened step only makes it quick.

  The table lock stays — it makes the table's own maximum stable while the
  target is computed — but it is no longer load-bearing for the sequence half.

- **And `+ 1` assumes the identity counts upwards, which the model does not.**
  `Identity` is `{ seed: i64, increment: i64 }` and the only rule on it refuses
  an increment of `0`, so `identity: [100, -1]` is a declaration this project
  accepts today. Every formula above is then pointing the wrong way.

  **Measured**, PostgreSQL closes most of that hole itself, and in a way the
  model cannot work around:

  ```
  CREATE TABLE ... (id int GENERATED ALWAYS AS IDENTITY (START WITH 100 INCREMENT BY -1) ...)
  ERROR:  START value (100) cannot be greater than MAXVALUE (-1)
  ```

  A descending sequence defaults its `MAXVALUE` to `-1`, so a descending
  identity needs an explicit one — and `Identity` has no field for it. **A
  negative increment is therefore not expressible on PostgreSQL from this
  model at all**: the `CREATE TABLE` is refused outright, loudly, inside the
  plan's transaction.

  Where the bound *is* supplied, the direction matters exactly as one would
  fear — measured, `RESTART WITH 101` against `MAXVALUE 100` is refused, and
  the next insert then collides with the pinned row, while `RESTART WITH 99`
  (min − 1, the right direction) works.

  **Decision.** PostgreSQL's `validate` refuses a negative increment outright,
  naming the `MAXVALUE` the model cannot carry. That is worth more than
  teaching the restart formula to count downwards: it makes the ascending
  assumption **unrepresentable** rather than merely documented, which is the
  house preference (CLAUDE.md, "prefer making a failure *unrepresentable* over
  handling it"). If `Identity` ever grows a bound, this refusal is the thing to
  revisit, and the measured direction rule above is what it should be replaced
  with.

- **`+ 1` is wrong for a second reason, which the refusal above does not cover:
  the step need not be one.** `identity: [1, 2]` is an ordinary declaration,
  positive and so not refused, and it is how a deployment partitions a key space
  between writers. **Measured**, the restart lands the generator in somebody
  else's half:

  ```
  the keys a step-2 generator owns:  1, 3, 5, 7, 9, 11
  -- a declared row is pinned at 13, and the plan restarts at max(keys) + 1 = 14
  keys now: 1, 3, 5, 7, 9, 11, 13, 14, 16   — it has crossed onto the even series
  ```

  Every key from then on belongs to the other writer, and the collision arrives
  whenever that writer next allocates. So the restart point is not
  `max(…) + 1` but **the next value in the generator's own series at or past
  every key in use** — the declared `seed` and `increment` decide it, and the
  sequence's current value is one of the inputs rather than the answer.

  The three corrections in this bullet and the two above it have one cause:
  `+ 1` treated the sequence as a counter when it is a *series with a declared
  shape*. Where the shape cannot be honoured, the refusal is the answer; where
  it can, it has to be computed from the declaration rather than from the
  maximum.
- **The declaration is refused on this engine, not merely discouraged.**
  ADR-0004 already says an identity primary key "hands out different values per
  environment, so declaring rows by id would be a lie by default". This was the
  second reason, and by the end of this bullet there are six — which is where
  `validate` stops warning and refuses. The rule is stated at the decision that
  concludes this bullet; it is named here so that a reader who stops at this
  paragraph does not leave with the weaker version.

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
`bytea_output`, `DateStyle`, `IntervalStyle`, `extra_float_digits` and
**`standard_conforming_strings`** — and does it in `pbps-postgres`, not in
`pbps-db`, because *which* settings matter is dialect knowledge.

**`TimeZone` is deliberately not on that list, and an earlier version of this
decision had it there.** Every setting above changes how a value is *written
down*; `TimeZone` changes what a value *is*. **Measured**, the same wall-clock
value converted from `timestamp` to `timestamptz` under two zones lands five
hours apart:

```
'2026-01-15 12:00:00' converted under UTC:               2026-01-15 12:00:00+00
the same value converted under America/New_York:         2026-01-15 17:00:00+00
```

Pinning it would mean pbps deciding, for every environment, which zone the
existing wall-clock data was recorded in — and getting that wrong moves every
row silently, with no error and nothing for `verify` to compare against, since
both sides would be read back under the same wrong pin.

That is a data decision, and SPEC §1.3 draws the line there: how data should be
*moved* is a business decision a structural diff cannot derive. So the pin
covers rendering only, and **a `timestamp` → `timestamptz` change is refused
until the zone can be declared** — the same shape as the `serial` and array
refusals in [ADR-0012](ADR-0012-postgres-type-catalogue.md), and for the same
reason: the tool would otherwise be choosing what the data means.

ADR-0012 §4 measured the other half of this — the zone also decides whether
that conversion rewrites the table — and recorded it as a caveat on the
estimate. It is worth noticing that the same setting turned out to be a
correctness input and not only a cost one, which is why the estimate caveat was
not enough on its own. The values a
plan writes and the values it reads back then live in one space, which is what
ADR-0004 requires and what "fixed CONVERT styles" achieves on the other engine.

The last of those is not about rendering at all; it is
[ADR-0011](ADR-0011-dialect-seam-under-a-second-engine.md)'s scanner reaching
into this list. That amendment fixes PostgreSQL's plain-string rule as
doubled-quotes-only, which is right **only while `standard_conforming_strings`
is `on`**. **Measured**, on a target where it is `off`, the same text is one
literal:

```
standard_conforming_strings=on  -> refused: syntax error at or near "s"
standard_conforming_strings=off -> one literal of length 10
```

So a definition containing `'it\'s  here'` would execute as one literal while
the normalizer closed it at the escaped quote and folded the data whitespace —
the silent no-plan failure of ADR-0011 Amendment 2, arriving through a session
setting rather than through a missing delimiter. Pinning it `on` is what makes
that amendment's rule true.

**`search_path` is pinned too, and for a different reason — but it is not pinned
to nothing.** Every name pbps *emits* is schema-qualified, so the statements it
writes need no search path, and a `search_path` an operator left pointing at
their own schema would silently change which object an unqualified name resolves
to. A first draft concluded from that: pin it empty.

**Measured, empty breaks ordinary declarations**, because the names inside a
definition are the user's and this tool does not parse them:

```
search_path = ''  ->  CREATE VIEW m.sp_v AS SELECT id, a FROM sp_t
                      refused: relation "sp_t" does not exist
search_path = m   ->  accepted
```

An unqualified same-schema reference in a view body, a `BEGIN ATOMIC` function
or a default expression is legal, common, and exactly the sort of text ADR-0002
promised to keep opaque. Emptying the path makes pbps refuse declarations the
engine would accept.

**But the object's own schema is not enough either**, and a second draft that
said so was wrong for the mirror reason. **Measured**, a view in one schema
calling an unqualified function installed in another — the ordinary shape of an
extension in `public` — is refused under a path of only its own schema:

```
search_path = m           ->  refused: function helper(integer) does not exist
search_path = m, m_ext    ->  accepted
```

PostgreSQL would accept that declaration under the project's normal path. pbps
overriding it turns a working schema into one the tool refuses, which is the
same imposition as emptying the path, one notch smaller.

**And the path decides what introspection *reads*, not only what DDL resolves.**
**Measured**, `pg_get_viewdef` qualifies or omits according to the current path:

```
search_path = ''   ->  SELECT id, a FROM m.sp2;
search_path = m    ->  SELECT id, a FROM sp2;
```

A snapshot taken under one path and a `verify` run under another therefore
compare two spellings of the same view and report drift that is not there. This
is the same fact that made an early draft of `A17` in
`spikes/pg-measurements` read wrong, which is a reasonable warning about how
easy it is to miss.

**But one path cannot serve both jobs, because a project setting moves.** A
first version of this decision used a single project path, defaulting to the
schemas the project manages, for DDL and for introspection alike. That default
changes the moment a revision starts managing another schema — and **measured**,
the deparse changes with it:

```
empty path:        SELECT id, a FROM m.dp_t;
project path = m:  SELECT id, a FROM dp_t;
```

A snapshot recorded under the old path and a `plan --db` run under the new one
then compare two spellings of an unchanged view, report drift that is not there,
and refuse the very revision that widened the path. The setting meant to make
reads deterministic would have made them depend on the declarations.

**Decision, in two halves that do not share a value:**

- **Reads use a canonical path: empty.** It is independent of the declarations,
  identical in every revision, and — **measured** — makes the deparser fully
  qualify every name, which is the spelling a snapshot should hold. `pg_catalog`
  stays reachable with an empty path (measured: the catalog query answers), so
  introspection itself is unaffected.
- **Writes use a path built per statement: the object's own schema first, then
  the project's configured extras.** One global list ordered by anything else is
  unsafe the moment two managed schemas hold the same name — **measured**, a
  view in `m_b` created under a path ordered `(m_a, m_b)` binds to `m_a.t`:

  ```
  a view in m_b created under (m_a, m_b): binds to schema m_a,
      and its stored definition is only: SELECT id, marker FROM t;
  the same view with its own schema first: binds to schema m_b
  ```

  The stored definition records `FROM t` either way, so nothing in the text says
  which table it caught, and the wrong one is a silent answer rather than an
  error. Putting the module's own schema first makes the unqualified
  same-schema reference — the case the previous round was about — resolve to the
  schema the author was writing in, and leaves the configured extras (an
  extension in `public`, say) reachable behind it.

Deterministic on both sides, never inherited from the role, and the read side no
longer moves when the project's shape does. The snapshot therefore always holds
fully-qualified text, which is also the form a human reading a state file would
want.

Refusing unqualified definitions outright is the alternative and is worse: it is
a parsing rule enforced by an engine error, applied to exactly the text §8.2
says pbps does not read.

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
| ADR-0004's design | One construct **refused on this engine** — a `data:` block keyed by an identity column (§2) — and §3 adds a connect-time session pin and a project search path |
| The pre-delete probe | A PostgreSQL rule that is **not** the SQL Server rule (§1) |
| `validate` | One rule: an identity-keyed `data:` block is **refused** (§2), naming the sequence and the two ways forward. The key-collision rule moves to `plan --db` — see below |
| `plan --db` | The key-collision check (§5). `cmd_validate` is offline and the collation lives on the live column, which this ADR keeps out of `pbps-model`, so offline `validate` cannot answer it — and under a nondeterministic collation it would answer *wrongly*, accepting keys whose inserts collide. It says it did not check rather than reporting clean (§9.1: offline is a preview) |

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
