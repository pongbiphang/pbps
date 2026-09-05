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

**Decision: these settings are scoped to the statements that carry *values*, not
pinned on the session, and not wrapped around the writes at all.**

- **Reads** — the reference-data read-back and the catalog reads that return
  value text — run inside the canonical scope, set and restored, **except the
  default probe**, which is below.
- **Writes** carry their canonicalization *in the values themselves*, and the
  canonical form is produced **by the engine, at plan time, and baked into the
  artifact**. **No session setting is changed around a write.**

  A first version of this said "an unambiguous typed literal, or bound as a
  parameter", and neither is canonicalization. The model keeps a quoted value as
  `Value::Text`, "exactly what was written … keep it verbatim, never parse it"
  (`crates/pbps-model/src/data.rs`), so the emitter renders the same characters
  either way and PostgreSQL's type input function reads them under whatever
  `DateStyle` the target has. **Measured**, binding changes nothing:

  ```
  the same text bound as a parameter, under two DateStyles:
      2026-01-02, 2026-02-01
  ```

  What *is* setting-independent is a literal the engine has already resolved —
  **measured**:

  ```
  DATE '2026-01-02' under two DateStyles:  2026-01-02, 2026-01-02
  ```

  So `plan --db` asks the engine to convert each declared value to its canonical
  form under the canonical settings and writes **that** into the plan. The
  engine does the parsing, which is §8.2's own rule; the model still never
  parses; and the artifact says exactly what will run, which is what §7.3
  requires of it. An offline `plan` has nobody to ask and therefore cannot
  produce applyable reference-data DML for a lexically ambiguous value — the
  same shape as everything else §9.1 calls a preview.

  **And `bootstrap --sql` is offline too**, which a first version of this
  decision did not account for: `cmd_bootstrap` takes its target as an
  `Option`, so the disaster-recovery script is rendered with no connection at
  all. It cannot ask the engine, and it must not resolve the ambiguity by
  wrapping its statements in a settings scope — that is §3's trigger hazard, and
  a bootstrap script creates the very triggers it would then fire.

  So the rule is **decided by the column's declared type, which is knowable
  offline**: a quoted value on a `date`, `time`, **`timetz`**, `timestamp`,
  `timestamptz`, `interval`, `real` or `double precision` column is refused by an
  offline `bootstrap --sql`, naming the connected form as the way to get a script that
  includes rows — and so, since §3, is a plain-literal *default* on one of those
  columns, for the same reason. Every other value renders as before. That costs the DR script
  its reference data for those columns specifically, and saying so is better
  than emitting a script that stores January in one environment and February in
  another.

  **And a value can depend on a setting the type does not.** Writes take no
  settings scope (§3), so a rendered value carrying a backslash is read
  differently under `standard_conforming_strings` — **measured**, the two
  characters `\n` in a `text` value survive as two under `on` and collapse to one
  newline under `off`, and a `bytea` in canonical hex is *accepted* under `off`
  while storing three bytes where two were meant:

  ```
  'a\nb' under on:                     length 4
  the same INSERT under off:           length 3
  E'a\nb' under either:                length 4
  '\x0102'::bytea under off:  accepted, storing 3 byte(s)
  decode('0102','hex') under off:      accepted, storing 2 byte(s)
  ```

  The `bytea` line is the one that matters: no error, no refusal, a different
  value in the table. Adding these types to the refusal list would be the wrong
  fix, because the dependency is in **pbps's own rendering**, not in the
  declaration — so pbps renders a setting-independent literal instead: an
  `E'…'` form with backslashes doubled (E-strings take backslash escapes under
  either setting), and `decode('…','hex')` for `bytea`, which contains no
  backslash at all. That is an encoding rule, not a settings rule, and it is
  what "the values pbps renders are pbps's responsibility" has to mean.

  `timetz` was missing from a first version of that list, and it is not a
  rounding error: **measured**, `'12:00 CST'::timetz` is `12:00:00-06` under the
  `Default` abbreviation dictionary and `12:00:00+09:30` under `Australia` — a
  timezone-bearing value whose meaning moves with a setting `TimeZone` does not
  cover. The list is derived from *which types read text through a
  setting-sensitive input function*, and it is worth deriving it that way rather
  than recalling it, since the catalogue of §1 in
  [ADR-0012](ADR-0012-postgres-type-catalogue.md) is where the answer lives.
- **The default probe is a read that executes code, and runs under the write's
  environment.** §4 evaluates an omitted cell's default server-side to decide
  whether it round-trips omitted. **Measured**, an expression whose cast happens
  at evaluation sees the session it is evaluated in:

  ```
  ('01/02/2026'::text)::date under MDY:  2026-01-02
  the same expression under DMY:         2026-02-01
  ```

  Evaluating it inside the canonical read scope — or under the empty read path,
  where an unqualified call in it would not resolve at all — would have the
  probe observe a value the write will never produce, and report a phantom
  update or fail a plan that is fine. So the probe runs under the settings the
  *write* will run under, and only its returned representation is canonicalized.

  This is the exception that shows the read/write split was drawn on the wrong
  property: it is not that reads are safe and writes are not, but that **any
  statement which executes the user's code has to run in the user's
  environment**.

  **"The write's environment" is a session, and the probe does not run in it.**
  The probe runs while `plan --db` is connected; the write runs later, from
  `apply`, on another connection — after an `ALTER ROLE … SET`, from another
  operator, or over a pooled connection carrying different `PGOPTIONS`.
  **Measured**, the same INSERT omitting the same column against the same table
  writes a different value in the two sessions:

  ```
  a column omitted by an INSERT, in the probing session (MDY):  2026-01-02
  the same INSERT in another session (DMY):                     2026-02-01
  ```

  Nothing existing catches that. The plan checksum is
  `digest_of(SavedPlan)` — SHA-256 over the plan's own typed JSON
  (`crates/pbps-model/src/plan.rs:266`) — and a session setting is not in the
  model and must not be, by inviolable constraint 1. So the checksum is
  structurally the wrong instrument here, and an assertion has to be the right
  one.

  Two decisions, in the order this project prefers them:

  - **Where the default is a literal, do not depend on it at all; where it is
    not, refuse the omission.** A row pbps writes carries the value the engine
    canonicalized at plan time (§3 above) rather than omitting the column and
    letting `apply`'s session evaluate the default.

    A first version of this said "where the value is knowable" and "costs
    nothing here", and both were wrong. The `READ ONLY` probe above accepts
    `now()` and `random()` precisely because they are harmless to *evaluate* —
    and their answer belongs to the transaction that asked. **Measured**:

    ```
    now() probed in two separate read-only transactions:  two different values
    a constant-folding default probed the same way:       the same value
    ```

    Baking the first turns `DEFAULT now()` from "the moment of the apply" into
    "the moment of the plan", and the next plan probes a third value and updates
    the row again — a permanent diff, and a changed meaning, produced by an
    optimisation.

    A version of this then proposed running the probe twice and baking when both
    transactions agreed. **That is invalid, and the sentence admitting so was
    already written beside it**: two observations cannot establish stability,
    only refute it — and the correction went on to use them as a licence anyway.
    **Measured**, a default calling a read-only function over a configuration
    row agrees twice and is something else at apply time:

    ```
    probed in two read-only transactions:                the same value both times (bronze)
    what the apply stored, after that row changed:       gold
    ```

    No settings assertion can see that, because nothing about the *settings*
    changed. So there is no observational test, and pbps bakes nothing on the
    strength of one.

    **And keeping `DEFAULT` does not rescue the shape either.** An omitted cell
    over a time-varying default has no value to converge on at all:

    ```
    two rows that both omitted the same clock_timestamp() default:  two values
    the stored cell against the default evaluated again:            different, so the next plan schedules another update
    ```

    The next plan compares what the last apply stored against a freshly
    evaluated default, schedules an update, and that update writes a third
    value. Neither branch converges, which means the shape is not desired state
    — it is a request pbps cannot satisfy in either direction.

    **Decision.** A `data:` row that **omits** a cell whose column carries a
    default is refused unless that default is a plain literal — a quoted string
    or a number-shaped scalar, the distinction `pbps fmt` already draws over
    scalars and not an expression parse. The message names the fix, which is to
    declare the value. Where the default *is* a literal, the earlier half of
    this decision — write the value rather than omitting the column — applies,
    and the value comes from the conversion below.

    **"Where the default is a literal, nothing can move between plan and
    apply" was the first version of that sentence, and it is wrong by one
    step.** A quoted literal is setting-independent as *text*. The column's type
    reads it, and it reads it in the session that runs the DDL — this document's
    own §3 measured `DEFAULT '01/02/2026'` storing two dates under two
    `DateStyle`s, and then exempted the literal anyway. **Measured**, end to
    end, the way the rule would have run it:

    ```
    DEFAULT '01/02/2026' on a date column, created under DMY, as the catalog spells it:  '2026-02-01'::date
    the declared text baked under MDY, the default created under DMY: cell vs default:  2026-01-02 vs '2026-02-01'::date
    whether the next plan schedules an update for that row:                              yes
    ```

    The probe read the declared text in the planning session; the DDL typed the
    same text in the applying one; the row pbps wrote disagrees with the default
    it created beside it; and the assertion, kept only for the unprobeable path,
    is not there to see it. The same fold happens to a `timestamptz` default
    naming an abbreviation — fifteen and a half hours between the `Default` and
    `Australia` dictionaries — and once a default *exists* typed, the session is
    out of it:

    ```
    a row omitting that cell under MDY, once the default exists typed:  2026-02-01
    ```

    So the hazard lives in exactly one place: the DDL that types the literal,
    which is the DDL this plan emits. This is §2's backslash finding reflected —
    there, a value depended on a setting its type did not; here, a literal
    depends on a setting through its type.

    Two fixes were on the table: keep the settings assertion for typed literals
    too, or canonicalize the structural default before emitting its DDL. The
    second makes the failure unrepresentable and is machinery §2 already has,
    so it is the one taken. **Decision.** A plain-literal `Column::default` on a
    column whose type is on §2's setting-sensitive list is **resolved by the
    engine at plan time under the canonical settings** — the same conversion §2
    applies to a `data:` value — and the DDL carries the resolved, typed
    spelling, which is measured setting-independent at creation:

    ```
    DEFAULT DATE '2026-01-02' created under DMY, as the catalog spells it:  '2026-01-02'::date
    ```

    The omitted cell over that default is written with the same resolved value
    from the same conversion, so there is no probe on this path and nothing left
    for an assertion to cover; the plan carries the resolved spelling as it
    carries a canonicalized data value, under the checksum. The state keeps the
    declared text and the differ compares it (§4); the resolved spelling is what
    runs. Two consequences follow, and both are already the rule for data
    values:

    - A spelling the canonical settings cannot read is refused at `plan --db`,
      naming the unambiguous one. **Measured**, `'01/02/2026'` under `ISO, YMD`
      is *refused* — `date/time field value out of range` — which is the right
      answer: the declaration was ambiguous, and the operator's `DateStyle` was
      the only thing deciding it.
    - Offline `bootstrap --sql` has nobody to ask, so §2's refusal of a quoted
      value on those column types applies to a literal *default* on them too,
      naming the connected form. That costs the DR script a column where §2's
      version cost it a row, and the alternative is a script whose defaults
      mean January in one environment and February in another — the exact
      thing §2 refused to emit for rows.

    What this does not reach is a typed literal *inside* a check expression or
    an index filter. **Measured**, `CHECK (d > '01/02/2026')` created under
    DMY is stored as `CHECK ((d > '2026-02-01'::date))`, and the filter the
    same way — but those are expressions, not scalars, and resolving one means
    parsing it. They are created under the operator's settings like every
    opaque definition, and the gap is recorded in Limits rather than hidden.

    A first version of this paragraph said the decision "shrinks what the
    assertion below has to cover but does not remove it". It removes it — see
    the next bullet. What the decision does do is make the failure this section
    has now chased through three rounds unrepresentable instead of detected,
    which is the outcome that was available at the start and that three
    successive optimisations talked me out of.
  - **There is no second branch.** A first version kept one: for an
    "unprobeable default" the plan would record the settings it probed under
    and `apply` would assert them before the write, in the shape of
    [ADR-0009](ADR-0009-postgres-modules.md)'s assertions before the `DROP`. It
    cited a numbered decision for that path which exists on PR #10's branch
    (`bbe605b`, the pre-delete probe reading a defaulted write as an arrival)
    and not on this branch's base, and — more to the point — after the two
    decisions above no applyable plan reaches it: an omitted cell over a
    non-literal default is refused, and over a literal one the plan spells the
    value itself. Every cell an applyable plan writes is a value the plan
    spelled, so there is nothing at apply time that a session setting could
    move and nothing left to assert. The settings under which pbps reads its
    *own* rendered values are set by pbps around that read (§3 above) and
    need no assertion either. The requirement is withdrawn rather than kept
    for a scenario that cannot occur; an assertion nobody can trip is a filter
    nobody re-reads.

  **What is left of the probe after all of this is nothing on the data path.**
  `plan --db` no longer evaluates a default for any cell; it converts literals
  under its own settings and refuses the rest. The `READ ONLY` transaction
  below stays, because it was never only about defaults: SPEC §9.1 makes
  `plan` a preview, and every statement `plan --db` runs — the pre-delete
  probe of §1 included — runs inside it.

  **And a probe that executes user code can move the target, which planning may
  not do.** SPEC §9.1 makes `plan` a preview; a default of `nextval(…)` makes it
  a write. **Measured**, probing one inside a planning transaction that then
  rolls back leaves the sequence advanced — which is `R39`'s finding arriving
  from the other direction:

  ```
  probing a nextval() default in a rolled-back planning transaction:
      returned 1, sequence last_value now 1
  ```

  The obvious remedy is to call any default that may run volatile or
  user-defined code unprobeable — a line pbps would have had to draw itself, and
  that at the time still mattered for the omitted cell. **Measured, that gives
  away too much**, because the engine already draws the line exactly where it
  belongs:

  ```
  nextval() under SET TRANSACTION READ ONLY:                   refused: cannot execute nextval() in a read-only transaction
  a volatile function that writes, under READ ONLY:            refused: cannot execute INSERT in a read-only transaction
  a volatile function that only computes, under READ ONLY:     accepted
  now(), under READ ONLY:                                      accepted
  the sequence after the read-only probe:                      last_value 1
  ```

  `random()` and `now()` are volatile and harmless; a blanket volatility rule
  would refuse the common case to catch the rare one. So: **`plan --db` runs
  inside `SET TRANSACTION READ ONLY`, and the engine's refusal is the only
  definition of "would have written" pbps keeps.** pbps performs no volatility
  analysis, keeps no list of dangerous functions, and cannot fall behind the
  engine's — which is §8.2's rule again, one level up: the database is the
  authority on what its own expression does. With the omitted cell gone from
  the data path, the transaction guards what §9.1 says planning is rather than
  a probe that no longer runs.

  This is the third decision on this branch that replaces a judgement pbps would
  have to make with a question the engine answers, and the first one where the
  judgement had already been drafted and was wrong.

- **Opaque DDL** runs under whatever the operator's database has — **with three
  explicit exceptions: `standard_conforming_strings = on`, the per-statement
  write `search_path`, and `check_function_bodies = on`**, set and restored
  around it.

  The second was missing, and the omission contradicted this same section. Opaque
  DDL is a write: a `CREATE VIEW` or `CREATE FUNCTION` whose body carries an
  unqualified reference binds it at creation, and under the operator's path it
  binds to whatever the deployment role happens to see while `bootstrap` binds
  the same declaration to the project's own schema. That is precisely the silent
  divergence the write path exists to prevent — measured on a view in `m_b` that
  caught `m_a.t` — so exempting opaque DDL from it exempted the one kind of
  statement the rule was written for. A rule with one exception invites a second
  one nobody re-reads; both are now named in the same sentence.

  The third has the same shape as the first: a rule stated elsewhere is true
  only while the setting holds. [ADR-0009](ADR-0009-postgres-modules.md)'s
  opaque-caller exemption suppresses a report because recreating a SQL-language
  caller makes the engine validate its references — and **measured**, that
  validation is gone under `check_function_bodies = off`, which reaches a
  session through `ALTER ROLE … SET` without anybody typing it. The apply then
  commits a caller that fails the first time it runs. That exception was stated two rounds earlier and dropped when this
  decision was rewritten, which is the second setting to fall out of a list
  during a rewrite about something else. It is not a rendering setting: it
  decides how the *definition text itself* parses, and
  [ADR-0011](ADR-0011-dialect-seam-under-a-second-engine.md)'s scanner rule —
  plain strings escape by doubling — is true only while it is `on`. On a target
  where it is `off`, a definition containing `'it\'s  here'` is accepted as one
  literal (measured, `R25`) while the normalizer closes it at the escaped quote,
  so a later whitespace edit *inside that literal* compares equal and is never
  planned. Pinning it `on` keeps the scanner's rule true, and the failure it
  introduces is the safe one: a definition that relied on `off` is refused at
  `CREATE`, loudly.

The write half was a session scope in an earlier version, and **measured, a
scope around a statement is also a scope around everything that statement
fires**:

```
-- the same trigger, the same literal '01/02/2026'::date, two rows
inserted inside pbps's scoped ISO, DMY:      the trigger wrote 2026-02-01
inserted under the database's ISO, MDY:      the trigger wrote 2026-01-02
```

§3 has already established that a PL/pgSQL body resolves and parses when it
*runs*; a scope held over the `INSERT` is therefore held over the user's trigger
too, and pbps ends up deciding what somebody else's code means. Canonicalizing
the values instead reaches exactly as far as it should: pbps's own text, and
nothing the engine does with it afterwards.

That measurement took two attempts, and the first said the opposite: run in one
session, the trigger's plan was cached from its first execution under the
database's own `DateStyle`, so the scope appeared not to reach it. Plan caching
masks a per-session effect, and the two rows above had to be inserted from two
fresh connections.

**`SET LOCAL` is not enough on its own, because it is transaction-scoped and a
plan is one transaction.** **Measured**, the setting outlives the statement it
was meant for, and the differ's own ordering puts a value-carrying statement
before an opaque one — `InsertRow | UpdateRow => 9` and
`CreateModule | AlterModule => 12` in `crates/pbps-diff/src/schema_diff.rs`:

```
BEGIN;
SET LOCAL DateStyle = 'ISO, DMY';
INSERT INTO kk.t VALUES ('01/02/2026');
    -- after the value-carrying statement, DateStyle is still: ISO, DMY
CREATE TABLE kk.later (d date DEFAULT '01/02/2026');
    -- stored as '2026-02-01'::date
```

So a scope that is only opened silently reinterprets every opaque definition
created after it in the same plan — the exact failure this decision exists to
prevent, reintroduced by the mechanism chosen to prevent it. **The previous
value is read and restored around each value-carrying statement**, so the scope
closes as well as opens.

An earlier version of this decision drew the line at read versus write, and that
was the wrong line. **Measured**, the same declared literal stores two different
dates depending on the session that inserts it:

```
'01/02/2026' inserted under MDY -> 2026-01-02
the same literal under DMY      -> 2026-02-01
```

ADR-0004 permits a quoted value precisely so the engine converts the text, which
makes that conversion pbps's business: without the scope on the write, one
approved plan stores different data on different targets, and the checksum that
was supposed to mean "exactly this will run" pins a statement whose *effect*
depends on the deployment role's settings.

The line that survives is not read versus write but **whose text it is**: a
value pbps renders into a plan and parses back is the tool's responsibility, and
a module body is the user's — so the first is canonicalized on both sides and
the second is left as the operator's database reads it.

Named **with values**, because a decision about a set has to say what is in it
and what each one is set to — and the read scope is worth nothing if the
implementation has to guess between `MDY` and `DMY`:

| Setting | Canonical value | |
|---|---|---|
| `DateStyle` | `ISO, YMD` | unambiguous in and out |
| `IntervalStyle` | `iso_8601` | `P1DT2H`, no locale in it |
| `bytea_output` | `hex` | |
| `TimeZone` | `UTC` | |
| `timezone_abbreviations` | `Default` | |
| `extra_float_digits` | `3` | **measured**, `double precision` and `real` both round-trip through their text at this value; `0` is lossy |
| `standard_conforming_strings` | `on` | ADR-0011's scanner rule is true only here |

**Measured**, one row written and read back under exactly those values:

```
2026-01-02 | P1DT2H | \x0102 | 0.12345678901234568 | 0.12345678 | 2026-01-15 12:00:00+00
float round-trips: true, real round-trips: true
```

And what each setting does to a value, which is why it is on the list at all:

| Setting | Why it is in the read scope |
|---|---|
| `bytea_output` | `\x0102` or `\001\002` for one stored value |
| `DateStyle` | `2026-09-05` or `05/09/2026` |
| `IntervalStyle` | `1 day 02:00:00` or `1 2:00:00` |
| `TimeZone` | the text of an unchanged `timestamptz` moves with it |
| `timezone_abbreviations` | **measured**, `'2026-01-15 12:00:00 CST'` is `2026-01-15 18:00:00+00` under `Default` and `2026-01-15 02:30:00+00` under `Australia` — fifteen and a half hours apart, from a dictionary `TimeZone` does not cover. ADR-0004 permits a quoted `timestamptz`, so one approved plan would store two different instants |
| `extra_float_digits` | **measured**, one stored `double precision` renders three ways: `0.123456789012346` at `0`, `0.12345678901234568` at `3`, `0.123456789012` at `-3` — and ADR-0004 permits a quoted non-integer value, so a `real` or `double precision` cell compares differently between a plan and a `verify` run by a role with another default |

`extra_float_digits` was in an earlier version of this list and fell out when the
decision was rewritten around scope; it is here again because the rewrite was
about *where* the settings apply, not *which*.

Two earlier versions of this decision got the *presence* of each setting right
and its *scope* wrong, in opposite directions, and both are measured.

**A session-wide pin changes what DDL means.** `DateStyle` is not only an output
format; it decides how an ambiguous literal is read:

```
MDY: '01/02/2026'::date = 2026-01-02
DMY: '01/02/2026'::date = 2026-02-01
```

and that reading is what gets stored, so the same definition text recreated
under a pinned session yields a different object than the target's own settings
would:

```
DEFAULT '01/02/2026' created under MDY  ->  stored as '2026-01-02'::date
the same text created under DMY         ->  stored as '2026-02-01'::date
```

Since §3 of [ADR-0009](ADR-0009-postgres-modules.md) makes every module edit a
drop and create, a pinned session would silently rewrite dates inside opaque
definitions on every rebuild — a month apart, with no error anywhere.

**And removing a setting entirely breaks the read-back it was there for.** An
earlier version dropped `TimeZone` from the list outright. **Measured**, the text
of an unchanged `timestamptz` moves with the session while the stored instant
does not:

```
read under UTC:              2026-01-15 12:00:00+00
read under America/New_York: 2026-01-15 07:00:00-05
the stored instant is the same: true
```

Without a canonical zone for reads, the same row compares differently between a
plan and a `verify` run by a role with another default, and produces phantom
updates and phantom drift.

So the two failures have one fix: **scope**. Rendering is canonicalized where
rendering happens; the DDL session is left as the operator's database has it, so
an opaque definition means what it means there.

`standard_conforming_strings` is the one that stays on both sides, and it is
worth saying why rather than leaving it to be found: it *does* change how DDL
text parses, but in the safe direction — pinned `on`, a definition that relied
on `off` fails loudly at `CREATE` instead of being silently misread, and
ADR-0011's scanner rule is only true while it is `on`.

**What no scope can fix stays refused.** A `timestamp` → `timestamptz` change
asks which zone the existing wall-clock data was recorded in — **measured**, the
same value lands five hours apart under two zones — and that is a data decision
SPEC §1.3 puts outside a structural diff. It is refused until the zone can be
declared, like `serial` and arrays in
[ADR-0012](ADR-0012-postgres-type-catalogue.md).

ADR-0012 §4 had measured that same setting deciding whether the conversion
rewrites the table, and filed it as a caveat on the *estimate*. Filing a
correctness input in the cost column is what let it survive into a session pin,
which is worth more than the fix. The values a
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
  the project's configured extras — and the path each module was created under
  is recorded with it.** The extras are ordered, so their order is part of what
  a declaration *means*, and **measured**, a view keeps the binding it was
  created with while a fresh one takes the new order:

  ```
  created under extras (m_ea, m_eb):        from m_ea
  the same view after reordering to
      (m_eb, m_ea):                         from m_ea — unchanged, nothing rebuilt it
  a bootstrap of the same declaration
      under the new order:                  from m_eb
  ```

  **That binding is only real for modules PostgreSQL parses at creation.**
  **Measured**, an opaque PL/pgSQL body resolves an unqualified name when it
  *runs*, under whatever path the caller has:

  ```
  created under (nn, nn_a), called under the same:   from nn_a
  the same function called under (nn, nn_b):         from nn_b
  a BEGIN ATOMIC body called under (nn, nn_b):       from nn_a
  ```

  So recording the creation path makes a view or a `BEGIN ATOMIC` routine
  deterministic and does **not** make a PL/pgSQL one deterministic — its
  resolution is decided by whoever calls it, which pbps does not control and
  cannot record. The engine's own remedy is a function-local path, and
  **measured**, it works:

  ```
  ALTER FUNCTION nn2.pinned() SET search_path = nn2, nn2a;
  called under (nn2, nn2b):  from nn2a
  ```

  pbps does not add one, for the reason it adds no `GRANT USAGE`: it would be
  configuration nobody declared. It belongs in the PostgreSQL documentation
  beside the `BEGIN ATOMIC` recommendation, as the second thing a project does
  to make its routines say what they mean.

  For the objects the recording *does* cover, reordering the extras silently
  divides the environment from a `bootstrap` of the same revision, and nothing
  exposes it: the declarations are unchanged,
  so the differ sees nothing, and both sides of the drift comparison read the
  same live object. (The deparsed text does record the resolved binding, but
  only when the read path makes qualification necessary — so it is not something
  a comparison can be built on.)

  **And a module is not the only thing PostgreSQL binds at creation.** Every
  expression parsed at creation keeps what it resolved against — **measured** on
  a generated column, which is the cheapest way to show it: it answers from
  `uu_a` for every later row, while a fresh table carrying the same declaration
  takes the new order:

  ```
  a generated column created under extras (uu_a, uu_b):    uu_a row 1
  a new row after reordering to (uu_b, uu_a):              uu_a row 2
  a bootstrap of the same declaration under the new order:  uu_b row 1
  ```

  Writing the rule around modules was the narrowing this branch has now made
  five times: taking the example in front of me for the shape.

  **The correction to that correction is that the rule cannot be wider than the
  model.** A first version of this paragraph named generated columns and
  expression indexes, and pbps can declare neither: `Column` carries
  `ty`, `nullable`, `default`, `identity`, `description`, `deprecated` and no
  generated-expression field (`crates/pbps-model/src/schema.rs:103`), and
  `IndexColumn` is a name and a direction (`schema.rs:278`). Recording a path
  for an object pbps has no declaration of gives the emitter nothing to rebuild
  and `bootstrap` nothing to reproduce. The generated column above is
  **evidence about the engine**, not a managed object.

  So the rule is scoped to the verbatim expressions the model actually holds:
  **`Column::default`, `CheckConstraint::expression` and `Index::filter`**,
  alongside module bodies. Generated columns and expression indexes stay
  unmanaged, and if either is ever declared the rule extends with it. Widening
  a rule past what the model can represent is the same mistake as writing it
  around one example — one round apart, in opposite directions.

  The state snapshot therefore records what each of those bound to, beside the
  declared text it already keeps (§2.2 of
  [ADR-0009](ADR-0009-postgres-modules.md)), and an object that would bind
  differently today is rebuilt.

  **What it records is the binding, not the path.** A first version of this rule
  compared the recorded path string, and the path is a proxy for the property
  that matters. **Measured**, a same-named object appearing *earlier* on an
  unchanged path moves the binding without moving the string:

  ```
  a view created under (xa, xb) while only xb.helper() exists:  xb
  the view after xa.helper() appears, path string unchanged:    xb
  a bootstrap of the same view declaration, same path:          xa
  ```

  Nothing exposes that: the declaration is unchanged, so the differ is silent,
  and both sides of the drift comparison read the same live object. The
  comparison the rule needs is available, though — PostgreSQL records the
  resolution it made:

  ```
  what the catalog records about the view's binding:  xb.helper
  ```

  So the state stores the resolved bindings. That is the comparison's left-hand
  side, and **a first version of this rule had no right-hand side**: "the set
  the current path resolves to" is not something pbps can obtain. The catalog
  reports what the *existing* object bound to, not what an unchanged
  declaration would bind to if recreated now; deriving that would mean parsing
  the declaration, which §1 refuses, or creating the object speculatively,
  which is a write and is refused three paragraphs above. A rule whose two
  sides cannot both be computed is not a rule.

  **What can be computed is the shadow.** For each recorded dependency, ask the
  catalog whether a same-named object now sits *earlier* on the write path than
  the schema the object actually bound to. No parsing, no DDL, no write —
  `pg_depend` against the configured path. **Measured**:

  ```
  the shadow query before ya.helper() exists:  none
  the same query after ya.helper() appears:    ya.helper
  ```

  and check constraints record their bindings too, so it reaches the
  expressions §4 covers:

  ```
  what the catalog records about a check constraint's binding:  yb.helper()
  ```

  **Two things about that query were wrong, and both were wrong in the
  direction that stays quiet.**

  *It has to follow each dependency's catalog class.* A first version looked at
  `pg_proc` only, and a view that binds a *table* has no `pg_proc` dependency at
  all. **Measured**, with a view bound to `zb.zt` and `za.zt` appearing earlier
  on an unchanged path:

  ```
  the routines-only test on that case:               none
  the class-aware test on the same case:             rebuild: shadowed by za.zt
  which table a bootstrap of that declaration binds: za.zt
  ```

  The last line is the divergence itself, and the first line is the check
  reporting nothing about it. Relations, types and operators are bound the same
  way; the test is driven by `refclassid` and looks in the catalog that class
  names, not in `pg_proc`.

  *And a bound schema that has left the path is not "no candidates".*
  `array_position(path, bound_schema)` is `NULL` when the schema is no longer
  configured, `NULL < anything` is `NULL`, and the comparison filters every
  candidate out — so the one case where the binding is *certainly* unreachable
  reads as the case where nothing changed. **Measured**, a view bound to
  `zb.helper` after `zb` left the extras while `za.helper` is visible:

  ```
  the ordering test alone:            none
  class-aware and missing-aware:      rebuild: zb.helper left the path
  ```

  A recorded binding whose schema is no longer on the write path forces a
  rebuild, before any ordering question is asked. Both of these are the same
  mistake as the one they were fixing — a comparison that answers "nothing to
  do" when it should answer "I cannot tell" — and SQL's three-valued logic
  supplied the second one silently.

  **That rebuild then has to be asked about the *effective* path, and only
  about bindings the path could have reached.** Written against the configured
  array it fires on things that were never on it, and a rebuild that fires
  every plan is the permanent restatement this section exists to prevent —
  reintroduced by the fix for the previous instance of it.

  The obvious trigger turns out not to be one. **Measured**, PostgreSQL records
  no dependency at all on the built-in functions and operators a parsed
  expression uses, because system objects are pinned:

  ```
  a view using only built-in functions and operators:  no recorded dependency on any of them
  ```

  so `pg_catalog` never arrives as a recorded binding and the rule never sees
  it. The correction is still owed, for two reasons that do arise. The path the
  question must be asked about is the effective one — `pg_catalog` is searched
  implicitly and a temp schema joins it when one exists:

  ```
  the effective path against the configured one:  pg_catalog,m against m
  ```

  And a **qualified** reference into a schema that was never on the path is
  recorded like any other, which is the real version of this failure — an
  extension installed in its own schema is the ordinary case:

  ```
  a view qualifying a function in a schema off the write path:      ax.digest
  whether ax was on the effective path when that view was created:  no
  ```

  Against the rule as stated, that view rebuilds on every connected plan, for
  ever. The last line is what separates the two cases without parsing anything:
  a binding whose schema was **already off the effective path when the object
  was created** was necessarily written qualified, and the path losing a schema
  it never had is not news. So the state records, beside each binding, whether
  it was on the effective path at creation, and only the ones that were can
  trigger "left the path".

  **It is conservative, and the cost is measured rather than guessed:**

  ```
  the same query for a view that qualified yb.helper() explicitly:  ya.helper
  ```

  A declaration that wrote `yb.helper()` in full cannot be affected by
  `ya.helper`, and this rule rebuilds it anyway. A first version called that
  "an unnecessary rebuild on a rare event" and accepted the trade, and the
  trade was mis-measured: the event is rare, the rebuild is not. **Measured**,
  the rebuild changes neither side of the comparison, so the next plan asks the
  same question and gets the same answer:

  ```
  a view that qualified qb.helper(): binding / shadow test, after qa.helper() appears:  qb.helper / qa.helper
  the same after the rebuild that test scheduled:                                       qb.helper / qa.helper
  and after the next one:                                                               qb.helper / qa.helper
  ```

  That is a drop and create — with its locks and ADR-0009's grant restoration
  — on every connected plan, for ever, for a view, a routine, a default, a
  check or a filter that was never wrong. A rebuild that fires every plan is
  the permanent restatement this section exists to prevent, and this one was
  introduced as a *conservative* choice.

  The first version also said distinguishing a qualified reference "needs the
  unqualified spelling and only the declaration text has it". It does not: it
  needs one creation with the shadow present, and the rebuild is exactly that.
  **Measured**, what the engine proved at that creation:

  ```
  what that creation proved: a candidate sat earlier and the object bound past it:  yes, so the reference is qualified
  ```

  An unqualified reference cannot bind past a same-named object earlier on the
  path; one that did was written qualified, and no shadow can ever move it. So
  the state records, beside each binding and next to the on-path flag, **whether
  a same-named object of the same class already sat earlier on the effective
  path when the object was created**. A binding that carries that flag is
  proven qualified and the shadow test skips it. The unqualified case converges
  in the one rebuild it always needed and records no shadow, because after the
  rebuild it binds the earliest candidate:

  ```
  the unqualified view: binding / shadow test, before its rebuild:  qb.helper / qa.helper
  the same after its rebuild:                                       qa.helper / none
  ```

  This is the same device as the on-path flag one paragraph up: a fact the
  engine established at creation, recorded so the next plan does not re-ask a
  question whose answer cannot change. A false rebuild is still loud, recorded
  in the plan, and gated — but it happens once.

  **And the catalog it is asked of is the one this plan will leave, not the one
  it found.** A first version asked the live catalog, and the live catalog
  cannot contain what the plan is about to create. **Measured**, with an
  existing view bound to `wb.helper` and a plan that adds `wa.helper()` earlier
  on its path:

  ```
  the shadow test, asked of the catalog before the plan creates wa.helper():  none
  the view after that plan applied with no rebuild scheduled:                 wb
  a bootstrap of the same declaration, wa.helper() created first:             wa
  the same test asked of the catalog after apply, one plan late:              wa.helper
  ```

  The divergence this test exists to expose is produced by the apply the test
  cleared, and reported by the next one — after any bootstrap of the revision
  in between has already disagreed with the environment. The plan's typed
  `ChangeSet` knows every name it will create or move — a module's declared
  kind and schema, a table's — so the candidate set is the catalog **minus what
  the plan drops, plus what it creates and the destinations of what it
  renames**, still without parsing anything. The rebuild it then schedules is
  ranked after the change that introduces the shadow, which is the position
  ADR-0002's name scan gives the same object in a bootstrap. This is the mirror
  of a shape master's own review found (`c9157f4`: the catalog asked about
  names the plan had not yet given it); both sides of "which catalog" fail
  quietly.

  A changed path string is then one way to reach the same conclusion rather than
  the definition of it. This is the fifth instance on this branch of the shape
  ADR-0009 collects: **a rule built on a cheap proxy for the real property** —
  and the first where the proxy was introduced by a fix for an earlier instance
  of the same shape. The sixth followed immediately: replacing the proxy with a
  comparison whose other half did not exist. The seventh is above: the right
  comparison, asked of the wrong catalog.

  **For an opaque body there is no such record, and the divergence runs the
  other way.** A `plpgsql` function re-resolves its unqualified names when it
  runs, so it does not keep the old binding — it silently acquires the new one,
  in a deployed environment, with no plan and no drift report:

  ```
  the same declaration through an opaque plpgsql body:           xb
  the same body after xa.helper() appears (plans discarded):     xa
  what the catalog records about the opaque body's binding:      nothing
  ```

  Nothing pbps can store fixes that, because the resolution has not happened
  yet at the time there is anything to store. This is the second thing §3's
  `ALTER FUNCTION … SET search_path` recommendation buys, and it promotes that
  recommendation from advice to **the only construct that makes an opaque
  body's meaning stable**. pbps still does not add one unasked — it would be
  configuration nobody declared — so this is stated as a known gap, and it is
  the honest limit of the whole write-path decision.** One global list ordered by anything else is
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
compare texts across the boundary; ask the engine for the value.** The engine
already compares a cell's value server-side (ADR-0004's implementation notes),
and §3 has since taken the omitted cell off the evaluating path altogether —
refused over a non-literal default, spelled by the plan over a literal one —
so what carries over is a comparison of values, dialect-agnostic in shape.
What does not carry over is any attempt to shortcut it with string equality,
which happens to work often enough on SQL Server to look like it works.

**And that answer covers the cell, not the column.** A cell is compared as a
value; the *structural* default is compared somewhere else entirely, and there
it is compared as text.
`crates/pbps-diff/src/schema_diff.rs:385` reads:

```rust
if base_col.default != col.default {
    changes.push(Change::AlterColumnDefault { ... });
}
```

And the base side *is* introspection: `pbps_model::data::plan_base` opens with
`let mut base = live.clone()` and only replaces `table.data`, so a connected
plan's `base_col.default` is whatever the catalog deparsed. With that side
holding `'unnamed'::text` and the declaration holding `'unnamed'`, **every
connected plan emits that change again, for ever — including the plan taken
immediately after a successful apply.** It is
the permanent restatement §2.2 of
[ADR-0009](ADR-0009-postgres-modules.md) fixed for module bodies, in the one
place I did not look for it because §4 was about cells.

**And the default is not the only expression compared as text.** PostgreSQL
respells every one of them — **measured**, a declared `label <> 'none'` comes
back with both parentheses and a cast:

```
a declared check expression:  CHECK ((label <> 'none'::text))
a declared index filter:      (label <> 'none'::text)
```

and `diff_constraints`'s `by_name!` macro
(`crates/pbps-diff/src/schema_diff.rs:444`) compares the whole
`CheckConstraint` / `Index` with `!=` and, on a mismatch, pushes a **drop
followed by an add**. So the cost here is worse than the default case: an
unchanged check is dropped and revalidated over the whole table on every
connected plan, and an unchanged filtered index is dropped and rebuilt.

So the same fix carries, and carries to all three: **the state records the
declared expression beside the one read back** — for `Column::default`,
`CheckConstraint::expression` and `Index::filter` alike — the differ compares
declared-now against declared-at-last-apply, and drift compares read-back
against read-back. That is a third field in the
snapshot, and it belongs in ADR-0009's tally with the other two — all three exist
for one reason, which is that PostgreSQL hands back its own spelling of whatever
it was given.

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

- PostgreSQL's answer comes from the column's collation, and **not** from
  `pg_collation.collisdeterministic`, which an earlier version of this bullet
  treated as the signal. **Measured**, the flag says nothing about case:

  ```
  nd_sensitive: collisdeterministic = false, 'New' = 'new' is false
  nd_ci:        collisdeterministic = false, 'New' = 'new' is true
  ```

  Nondeterminism disables the bytewise tie-break *after* the provider has
  compared; the ICU locale and strength decide equality. Under the first of
  those two, `'New'` and `'new'` are two valid keys — measured, the second
  insert is accepted — so a rule reading the flag as proof of collision would
  refuse a perfectly good declaration.

  The connected check therefore **asks the engine about the actual keys**, under
  the column's own collation, rather than inferring from a property of the
  collation. That is the same move as everywhere else here: the engine answers
  questions about the engine.
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
| `pbps-model` | **Three fields in `StateSnapshot`, and a format bump** — the same three ADR-0009 counts, two of which this document is the reason for: the declared module text (ADR-0009 §2.2); the **resolved bindings** of every managed object, each flagged with whether its schema was on the effective write path at creation and whether a same-named object of the same class already sat earlier on it (§3) — the second flag is the engine's proof that the reference was qualified, without which a qualified reference is rebuilt on every plan for ever; and the **declared expressions** — `Column::default`, `CheckConstraint::expression`, `Index::filter` (§4). The differ then compares declared-now against declared-at-last-apply, and drift compares read-back against read-back. This row said "Nothing" for four rounds after the first field was added, which is the stale-summary shape this branch keeps finding: the paragraph moved and the table that summarizes it did not |
| ADR-0004's design | One construct **refused on this engine** — a `data:` block keyed by an identity column (§2). §3 adds no session pin at all. The canonical settings (with their values, §3) are set and restored around the **reads that render values**; the **writes** carry values the engine canonicalized at plan time, baked into the artifact — and so does a plain-literal default on a setting-sensitive column, emitted as the resolved typed spelling, because the DDL that types a literal is where a session reads it (§3); the **default probe is gone from the data path** — an omitted cell is refused over a non-literal default and spelled by the plan over a literal one, so `plan --db` evaluates no user code for a cell and `apply` has nothing to assert — while `plan --db` itself stays inside a `READ ONLY` transaction, because §9.1 makes it a preview; and **opaque DDL** runs under the operator's settings **with three restored exceptions, `standard_conforming_strings = on`**, which is what makes ADR-0011's scanner rule true, **the per-statement write `search_path`**, without which opaque DDL binds its unqualified references differently from `bootstrap`, **and `check_function_bodies = on`**, without which a stale reference in a recreated SQL body is accepted silently instead of refused at `CREATE` (ADR-0009; the exemption that first motivated the pin is gone, the loud failure is what it is for now). A scope around a write would also be a scope around every trigger that write fires |
| The search path | Two values, not one (§3): a **canonical empty path for every introspection read**, so a snapshot's spelling does not move when the project's shape does, and a **per-statement write path** — the object's own schema first, then the project's configured extras. For module bodies **and the three verbatim expressions the model holds** (`Column::default`, `CheckConstraint::expression`, `Index::filter`), the state records **the resolved binding, not the path string**: a new same-named object earlier on an unchanged path moves the binding and leaves the string alone. The test is asked of the catalog **as this plan will leave it** — minus what it drops, plus what it creates and the destinations of what it renames, since a shadow the same plan introduces is otherwise found one plan late — is a same-named object of the same catalog class then earlier on the path than the schema this object bound to, and is that schema still on the **effective** path at all, asked only of bindings whose schema was on it when the object was created — because what an unchanged declaration *would* bind to today cannot be computed without parsing it or creating it. That is conservative: a declaration that qualified the name in full is rebuilt too. An opaque body records nothing, re-resolves at call time, and is the decision's stated gap |
| `plan --db`'s transaction | `READ ONLY` (§3). The engine refuses exactly the statements that would move the target — `nextval()`, a function that writes — and accepts the volatile ones that do not, so pbps analyses nothing. It was introduced for a default probe that §3 has since removed from the data path, and it stays for what §9.1 says planning is |
| The default probe's session | Only valid inside itself (§3). The write happens from `apply`, on another connection, and the checksum covers the plan's typed JSON, not a session setting — so an omitted cell over a non-literal default is **refused** — neither baking nor `DEFAULT` converges, measured both ways — and where the default is a literal pbps writes the value — resolved under the canonical settings at plan time when the column's type is setting-sensitive, the one conversion feeding both the cell and the default's DDL, so no path has a probe and none needs an assertion. A first version kept a recorded-and-asserted settings branch for an "unprobeable default", citing a decision that exists on PR #10's branch and not on this one; no applyable plan reaches that branch, and it is withdrawn |
| Rendering a value | Setting-independent by construction, not by scope (§2): `E'…'` with backslashes doubled for `text`, `decode('…','hex')` for `bytea`. Canonical hex under `standard_conforming_strings = off` is *accepted* while storing the wrong bytes, so a refusal list cannot cover this — the dependency is in pbps's rendering, not in the declaration |
| A column's structural default, a check expression, an index filter | All three stored **as declared** beside the read-back (§4), for the reason module text is (ADR-0009 §2.2): they are compared as text and PostgreSQL respells all of them. A plain-literal default on a setting-sensitive column is additionally *emitted* resolved (§3); the declared text is still what the differ compares. The check and the filter are the expensive ones — `diff_constraints` answers a mismatch with drop-then-add, so an unchanged check is revalidated and an unchanged index rebuilt on every connected plan |
| The pre-delete probe | A PostgreSQL rule that is **not** the SQL Server rule (§1) |
| `validate` | One rule: an identity-keyed `data:` block is **refused** (§2), naming the sequence and the two ways forward. The key-collision rule moves to `plan --db` — see below |
| `plan --db` | The key-collision check (§5). `cmd_validate` is offline and the collation lives on the live column, which this ADR keeps out of `pbps-model`, so offline `validate` cannot answer it — and under a nondeterministic collation it would answer *wrongly*, accepting keys whose inserts collide. It says it did not check rather than reporting clean (§9.1: offline is a preview) |

## Limits

- **Composite keys are still deferred**, as in ADR-0004.
- **`READ ONLY` bounds the database, not the world.** It refuses writes to this
  database's tables and sequences, which is what §3 needs; a function that a
  planning read happens to invoke and that calls out through `dblink`, raises a
  `NOTIFY`, or writes a file is not stopped by it and is not stopped by anything
  else pbps can do. The guard is exact for the failure that was found and
  honest about the one it does not cover.
- **§4's expression-comparison finding is not measured against SQL Server.** The
  differ is shared, and all three expressions are read raw with no normalizer
  anywhere in the workspace: `sys.default_constraints.definition`
  (`catalog.rs:189`), `sys.check_constraints.definition` (`catalog.rs:71`) and
  `sys.indexes.filter_definition` (`catalog.rs:229`). The same comparisons run
  on both engines, so the same loops may already exist on the shipped dialect —
  and the check and filter ones cost a revalidation and an index rebuild per
  plan, not just a restated `ALTER`. That is a *reasoned* worry, not an
  observation: nothing here ran it. It wants one live check on SQL Server, and
  it is the fourth item in this design pass to point at shipped code rather than
  at Phase 5.
- **A typed literal inside a check expression or an index filter is folded
  under the operator's settings** — measured, `CHECK (d > '01/02/2026')`
  created under DMY is stored as `'2026-02-01'::date` — and §3's resolution
  does not reach it, because it is an expression and resolving one means
  parsing it. Two operators with different `DateStyle` get two constraints from
  one declaration, and §4's declared-text comparison cannot see it. This is the
  opaque-DDL rule's stated gap arriving in a shape the model does hold.
- **Nondeterministic collations are now measured** (§5), and the flag turned out
  not to mean what this document first assumed.
- **Everything here is proposed**, and falsifiable by the PostgreSQL live suite.

## Placement

Phase 5, with the emitter and the probes. Nothing here needs to land sooner:
§2's one candidate for being a live bug was measured and is not one.

One item is not Phase 5 work at all and should be fixed whenever SPEC §12 is
next edited: that sentence claims a PostgreSQL collision is recorded in ADR-0004,
and until this document existed, none was.
