# ADR-0012: The PostgreSQL type catalogue — and why "safe" and "cheap" are different axes

- Status: accepted and built. The catalogue landed with Phase 5 step 2
  (Amendment 1) and §3's estimate with step 9 (Amendment 2); the PostgreSQL
  live suite re-measures both.
- Date: 2026-09-05, amended 2026-09-10
- Related: docs/SPEC.md §7.2, §11.2, §12, 14.1 (the P1 estimate row);
  [ADR-0003](ADR-0003-execution-strategy.md);
  [ADR-0009](ADR-0009-postgres-modules.md);
  [ADR-0011](ADR-0011-dialect-seam-under-a-second-engine.md)

The type catalogue is the largest single piece of a dialect — SPEC §12 counts it
first among the things a second engine re-implements. This decides its shape
before it is written, and it turned up one finding that is not about types at
all: **`TypeChangeRisk` answers its question correctly and PostgreSQL proves the
question is not the only one worth asking.**

## What was measured, and against what

PostgreSQL 18.6 —
`docker.io/library/postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280`,
on 2026-09-05. Rewrites were detected by comparing `pg_class.relfilenode` either
side of the statement, which is the engine's own answer to "was this table
rebuilt", not a proxy for it.

**No SQL Server measurements were taken for this document.** Nothing below
claims anything about SQL Server's costs; where the contrast would matter it is
named as an open question rather than asserted.

## 1. The catalogue: what the engine spells back

**Measured.** Thirty-four declared spellings and what `format_type` returns:

| Declared | Read back | | Declared | Read back |
|---|---|---|---|---|
| `int`, `int4`, `integer` | `integer` | | `bool` | `boolean` |
| `int2`, `smallint` | `smallint` | | `char(5)`, `character(5)` | `character(5)` |
| `int8`, `bigint` | `bigint` | | `varchar` | `character varying` |
| `decimal(10,2)` | `numeric(10,2)` | | `varchar(9)` | `character varying(9)` |
| `numeric` | `numeric` | | `time` | `time without time zone` |
| `real` | `real` | | `timetz` | `time with time zone` |
| `float`, `float8`, `double precision` | `double precision` | | `timestamp` | `timestamp without time zone` |
| `float(1)`, `float(24)` | **`real`** | | `interval`, `json`, `jsonb`, `uuid` | unchanged |
| `float(25)`, `float(53)` | **`double precision`** | | `serial` | **`integer`** + an owned sequence |
| `text`, `date`, `bytea` | unchanged | | `timestamptz`, `timestamp with time zone` | `timestamp with time zone` |
| | | | `text[]` | unchanged by the catalog, and **unloadable** — see below |

Two of these are traps rather than aliases:

- **`float(n)` is two types wearing one spelling.** At `n ≤ 24` it is `real`; at
  `n ≥ 25` it is `double precision`. `normalize_type` has to resolve the
  argument, not carry it.
- **`serial` cannot round-trip at all** — it is a macro, not a type. ADR-0011
  Amendment 3 states the contract this violates ("`normalize_type`'s output is
  what introspection reads back"), and ADR-0009 refuses it at load time with
  `GENERATED … AS IDENTITY` named as the replacement.

Those last four were missing from a first version of this table, and a closed
catalogue that omits `text` and `date` rejects most schemas anybody would
write. They were not an oversight of measurement — they are the types
[ADR-0013](ADR-0013-postgres-reference-data.md) spends whole decisions on:
`date` and `timestamptz` are on its offline refusal list, `text` and `bytea`
are the two whose rendering it makes setting-independent. The list was recalled
rather than derived, which is the failure §1 of that document names about its
own refusal list one round earlier.

So the derivation rule, which is worth more than the five rows: **the catalogue
must admit every type the other ADRs' rules name.** A rule about `bytea`
columns in a dialect that cannot declare one is not implementable, and that is
checkable by reading, without remembering anything.

**Decision.** The catalogue is closed, as the SQL Server one is, and normalizes
to the catalog's own spelling — `integer`, not `int`; `character varying`, not
`varchar`. Users may write either; two files that differ only in the spelling
produce equal `Schema`s, which is inviolable constraint 1.

**Arrays are out of scope**, and a first draft of this ADR had them in on the
strength of `text[]` round-tripping through `format_type`. It does — and the
declaration never reaches that far. **Measured**, against this repository's own
parser:

```
"text[]"     -> Err(BadBaseName("text[]"))
"integer[]"  -> Err(BadBaseName("integer[]"))
"text ARRAY" -> Ok(ColumnType { base: "text array", args: [] })
```

`ColumnType::from_str` allows only `[A-Za-z0-9_ ]` in a base name, so the common
spelling is refused — and **the SQL-standard spelling is not**, because spaces
are legal in a base name (`double precision`, `timestamp with time zone`). That
second line is the dangerous one: `text ARRAY` loads happily as the base name
`text array`, which no catalog will ever return, so the column is reported as
changed on every single run and no plan can ever fix it.

So arrays need a representation in `ColumnType` — a dimension flag, not a
spelling — before they can be declared at all, and that is a `pbps-model` change
this document is not taking. Until it is taken, **the dialect refuses both
spellings explicitly**, `text ARRAY` included, rather than letting one of them
through into a comparison it cannot win.

Domains, enums and composite types are out of scope for a different reason: they
are user-defined *objects* with their own creation, ownership and drop
semantics, not spellings of a built-in, and modelling them is a separate ADR.
`pull` reports a column of such a type as unmanaged rather than guessing a base
type — the alternative is a declaration that silently widens a domain's
constraint away.

## 2. `TypeChangeRisk` answers its question, and it answers it correctly

The enum's criterion is stated in the trait: *"whether this kind of change can
fail at all, not whether today's data happens to be safe"*. PostgreSQL's failure
behaviour maps onto it cleanly, **measured**:

| Change | What the engine did | Class |
|---|---|---|
| `varchar(20)` → `varchar(5)` over a 10-character value | `ERROR: value too long for type character varying(5)` | `Narrowing` — and the §7.5 probe is what catches it before the plan runs |
| `text` → `int` | `ERROR: column "c" cannot be cast automatically to type integer` | `Incompatible` |
| `int` → `bigint`, `varchar(10)` → `varchar(20)` | accepted | `Safe` |

with one wrinkle worth naming: PostgreSQL refuses `text → int` **at DDL time,
before looking at a single row**, where SQL Server would attempt the conversion
and fail on the data. The classification is the same; the moment of failure is
not. Both are inside the plan's transaction, so §7.5's promise holds either way.

**No change to `TypeChangeRisk`.** It is the one part of the seam that a second
engine confirmed without amendment.

## 3. But `Safe` does not mean cheap, and PostgreSQL makes the gap impossible to ignore

**Measured**, on the same statements:

| Change | Table rebuilt? |
|---|---|
| `int` → `bigint` | **REWRITE** |
| `bigint` → `int` | **REWRITE** |
| `varchar(10)` → `varchar(20)` | no rewrite |
| `varchar(20)` → `varchar(10)` | **REWRITE** |
| `varchar(20)` → `text` | no rewrite |
| `text` → `varchar(20)` | **REWRITE** |
| `numeric(10,2)` → `numeric(12,2)` | no rewrite |
| `numeric(10,2)` → `numeric(10,4)` | **REWRITE** |
| `ADD COLUMN d int DEFAULT 7` | no rewrite |
| `ADD COLUMN d uuid DEFAULT gen_random_uuid()` | **REWRITE** |
| `SET NOT NULL`, `DROP COLUMN`, `ADD COLUMN` with no default | no rewrite |

**Measured**, what a rewrite costs and what it holds:

```
-- one million rows
ALTER TABLE t.big ALTER COLUMN v TYPE varchar(20);   Time:   0.662 ms
ALTER TABLE t.big ALTER COLUMN i TYPE bigint;        Time: 410.042 ms

-- and the lock the rewrite holds, from pg_locks:
 AccessExclusiveLock | t        -- ALTER COLUMN … TYPE: blocks readers too
 ShareLock           | t        -- CREATE INDEX (non-concurrent): blocks writers only
```

Six hundred times slower on a million rows, holding a lock that blocks **reads**
— and it is `int → bigint`, the textbook widening. **Measured**, by running this
repository's own dialect (the SQL Server one, since it is the only one that
exists; the mapping from class to gate is shared):

```
int -> bigint:                Safe,      gate asks for None
varchar(10) -> varchar(20):   Safe,      gate asks for None
varchar(20) -> varchar(10):   Narrowing, gate asks for Some(Narrowing)
```

So on PostgreSQL **the gate would wave a multi-minute total outage through with
no approval at all**, on a large table, and be right to by its own rule.

### The decision: do not fold cost into the risk class

The tempting fix is to reclassify `int → bigint` as risky on PostgreSQL. That is
wrong twice over:

- It **lies about what the class means.** `Narrowing` exists so a reviewer knows
  the change may *fail or lose data*. A rewrite loses nothing and cannot fail.
  Folding an availability cost into a correctness class teaches reviewers that
  the classes do not mean what they say, and the next genuinely dangerous change
  gets the same shrug as this one.
- It **breaks the class's own criterion**, which is deliberately
  data-independent (§7.2). A rewrite's cost is entirely a function of how many
  rows there are, which is data.

SPEC 14.1 already has the right home and the right words for it: *"Risk says
whether a change can fail, never how long it may block or how much it may
rewrite"* — the **P1 estimate**, explicitly "kept apart from correctness", which
"never loosens a gate and never reclassifies a dangerous operation as safe".

So: **the cost axis stays out of `TypeChangeRisk`, and PostgreSQL supplies the
estimate's first real dataset.** Two facts the dialect can state *statically*,
from the typed change alone and without reading a single row — so §7.2 is
untouched:

1. **whether the statement rewrites the table** — for the changes where that is
   a property of the change alone;
2. **which lock it takes**, measured rather than recalled.

**The first is not always static, and §4 below measures why.** `timestamp` →
`timestamptz` rewrites or does not depending on the session's `TimeZone`, and
`ADD COLUMN … DEFAULT` depends on the expression's volatility, which this tool
does not parse. An estimator implementing "static from the typed change" would
call both metadata-only and be wrong on a live table.

So the static table covers the context-independent rows only. For the rest the
estimate answers **`unknown`**, or answers from connected context where there is
a connection to ask — and `unknown` is a real answer here rather than a gap:
§4's point is that an estimate which guesses *cheap* is worse than one which
admits it does not know.

Row counts, and therefore durations, are the connected half and are the
estimate's business, not this ADR's. What this ADR fixes is that the two facts
above are *dialect knowledge* and have to be produced where the SQL is produced.
This ADR does not design the estimate; it records that PostgreSQL is what makes
building it necessary rather than nice, and that the data it needs is a property
of the emitted statement.

## 4. The cost is not a function of the declaration alone

Three measured cases where the same declared change costs differently:

- **The session's timezone decides.** `timestamp → timestamptz` is free under
  `UTC` and **rewrites** under `America/New_York`:

  ```
  UTC session:       no rewrite
  New_York session:  REWRITE (table rebuilt)
  ```

  A connection-level setting, invisible to the declarations, changes whether a
  change is instant or a full rebuild. Any estimate must therefore say the
  session it assumes, or decline to answer — and `doctor`, which already reports
  what only a connection can answer, is where the session's timezone belongs.

- **The default's volatility decides.** `ADD COLUMN … DEFAULT 7` is free;
  `DEFAULT gen_random_uuid()` rewrites. Both are `AddColumn` with a default in
  the typed change, and only the expression tells them apart — which the tool
  does not parse (§8.2). The honest answer for an unparsed expression is
  "unknown", not "free": an estimate that guesses cheap is worse than one that
  admits it does not know, which is the same direction every probe in this
  project already leans.

- **Precision widens for free; scale does not.** `numeric(10,2) → numeric(12,2)`
  is free and `numeric(10,2) → numeric(10,4)` rewrites. Nothing about the
  declaration's shape suggests the asymmetry; only the engine knows it, which is
  the argument for measuring the whole table rather than reasoning about it.

## 5. `USING`: the clause this model has no place for

`text → int` is refused with `column "c" cannot be cast automatically`. The
engine's remedy is `USING c::integer` — an expression that says *what the data
becomes*.

**Decision: refuse the change and name the `USING` clause in the error; do not
emit one.** A cast pbps chose is a data transformation nobody declared, nobody
reviewed in the merge request, and nobody can find in git afterwards — the same
objection ADR-0010 §1 makes to an inferred `GRANT USAGE`, and the same one
SPEC 14.3 makes to every shortcut around recorded intent. `strategy:` cannot
carry it either: ADR-0003 is explicit that a strategy says *how* to get there
and never *where to go*, and a `USING` expression decides where.

This makes pbps **stricter on PostgreSQL than on SQL Server** for exactly one
class of change, and that asymmetry is recorded rather than smoothed over: on
SQL Server the same declaration runs and the engine picks the conversion, which
is not obviously the better behaviour — it is the same decision taken by
something that cannot be reviewed. Revisit if a real user is blocked; the
replacement is a declared transformation with its own ADR, not a flag.

## 6. Two introspection traps, measured

- **A dropped column does not leave.** After `DROP COLUMN gone`:

  ```
   attnum |           attname            | attisdropped |   ty
        1 | keep                         | f            | integer
        2 | ........pg.dropped.2........ | t            | -
  ```

  The catalog keeps the slot, with a placeholder name and **no readable type**.
  A reader that does not filter `attisdropped` reports a phantom column whose
  type cannot be parsed — and `attnum` keeps the hole, so column order has gaps
  and `attnum` is not a position. This is CLAUDE.md's rule again: absent, empty
  and unreadable are three different things, and this row is the third wearing
  the clothes of the first.

- **`DROP COLUMN` does not reclaim the space** (no rewrite, measured). Nothing
  in pbps depends on that today; it is recorded because "the drop was cheap" and
  "the table got smaller" are different claims, and an estimate that conflates
  them would be wrong in the direction that surprises an operator.

## What this changes

| | |
|---|---|
| `pbps-model` | **Nothing, because arrays are out.** `ColumnType` holds a base and arguments, which fits every spelling this catalogue admits. Admitting `text[]` would need a dimension in `ColumnType` — a real model change, and the reason arrays wait (§1). Nothing *for this document's decisions*: [ADR-0013](ADR-0013-postgres-reference-data.md) does add three `StateSnapshot` fields |
| `pbps-dialect` | `normalize_type`'s contract, already named in [ADR-0011](ADR-0011-dialect-seam-under-a-second-engine.md) Amendment 3 |
| `TypeChangeRisk` | **Nothing** — §2 |
| The estimate (14.1 P1) | Gains its first measured dataset and a stated boundary — §3 |
| `pbps-postgres` | The catalogue itself, which does not exist yet |

## Ruled out

- **Reclassifying a rewrite as `Narrowing`** (§3). Lies about the class.
- **Emitting a `USING` cast** (§5). A transformation nobody declared.
- **Guessing "cheap" for an unparsed default expression** (§4).
- **Mapping `serial`** to `integer` plus an inferred sequence (§1). It reads back
  as something else on every run.
- **Folding domains and enums into their base types** (§1). Silently drops a
  constraint the user wrote.
- **Admitting arrays on the strength of the catalog round-trip** (§1). The
  catalog is not the part that refuses them; the loader is, and one of the two
  spellings is refused while the other is silently accepted as a base name no
  catalog will ever return.

## Limits

- **SQL Server's `int → bigint` cost remains unmeasured.** The PostgreSQL
  matrix in [`the_estimate_says_what_the_engine_does_about_rebuilding_the_table`](../crates/pbps-pg/tests/live.rs)
  does not answer whether SQL Server performs a metadata-only change. #255
  tracks that engine's measurements and estimator; neither a successful ALTER
  nor a PostgreSQL `relfilenode` result supplies the missing answer.
- **A stable `relfilenode` still does not mean “no scan.”**
  [`a_change_that_rebuilds_nothing_may_still_read_every_row`](../crates/pbps-pg/tests/live.rs)
  measures no rewrite for both `SET NOT NULL` and `varchar(10) → varchar(20)`,
  but 100,000 rows read for the former and zero for the latter. The estimate
  therefore carries scans separately. A validated `CHECK (v IS NOT NULL)` can
  avoid that scan: [`a_check_the_engine_may_prove_the_column_from_takes_the_scan_back_to_unknown`](../crates/pbps-pg/tests/live.rs)
  measures zero rows with the validated proof and 100,000 without it or with
  a `NOT VALID` check; the connected estimate reports that proof-dependent
  case as unknown rather than parsing arbitrary checks.
- **Partitioned tables, inheritance parents and type changes on indexed
  columns remain outside the measured cost matrix**, including the original
  `ALTER TYPE … USING` question. DECISIONS 403–405 keep uncertainty explicit.
  [`a_shape_the_measurements_never_covered_is_not_answered_from_them`](../crates/pbps-pg/tests/live.rs)
  verifies the connected estimator withdraws its static rewrite answer for
  those catalog shapes and distinguishes never-analyzed rows from an analyzed
  count. It tests the uncertainty boundary; it does **not** execute those
  type changes and measure their costs. Ordinary indexed type changes are
  excluded too, not only changes with `USING`.
- **These are scoped PostgreSQL measurements, not a universal cost guarantee.**
  Amendment 2 records the measured matrix and locks; unmeasured expressions,
  shapes and engines must keep their unknown answer.

## Amendment 1: what building it added to §1

Written when the catalogue landed (Phase 5 step 2, issue #77), against the same
pinned image. Nothing below contradicts §1; each is a row it did not have.

- **A modifier can go inside the name.** `timestamptz(3)` reads back as
  `timestamp(3) with time zone`, and `timestamp with time zone(3)` is a *syntax
  error*. §1 measured the four time types only without a precision, so this
  never came up. A `ColumnType` cannot hold it, and the declaration is refused
  rather than folded into the unmodified type — DECISIONS 242, model change
  in issue #130.
- **`numeric`'s scale may be negative, and may exceed the precision.**
  `numeric(10,-5)` reads back as itself. SQL Server's `0 <= scale <= precision`
  would refuse a column this engine makes (DECISIONS 240).
- **An omitted argument means three different things.** `character` gains
  `(1)`, `character varying` gains nothing, and bare `numeric` stays unbounded
  rather than gaining the `(18,0)` SQL Server fills in.
- **`bpchar` is not an alias of `character`.** `bpchar(5)` reads back as
  `character(5)` and a bare `bpchar` reads back as `bpchar`, so it is left out
  of the closed catalogue rather than aliased half-correctly.
- **The engine adjusts two arguments silently.** `interval(7)` is stored as
  `interval(6)` and `time(7)` as `time(6)`, with no error — the identifier
  truncation §1 records, one layer down. The dialect enforces both bounds
  itself.
- **§2's classification is now a measured matrix**, twenty types by twenty on an
  empty table (DECISIONS 243). It confirmed §2 and corrected a rule derived from
  it: `timestamptz` converts to `timetz` and `timestamp` does not, and
  `interval` converts to `time` and to neither `timetz` nor `timestamp`.
- **`numeric(10,2)` → `numeric(10,4)` can fail** — `numeric field overflow` —
  though §3 lists the same change among the rewrites. The two axes really are
  different questions, in both directions.

## Amendment 2: the estimate, built — and §3's dataset in full

Written when the pre-flight, the rename impact and the estimate landed (Phase 5
step 9, issue #84), against the same pinned image on 2026-09-10. Nothing below
contradicts §3 or §4; §3's eleven rows are eleven of these.

### The whole matrix, not eleven rows of it

**Measured**: every ordered pair of the catalogue's spellings plus the three
`numeric` shapes that separate a precision from a scale — 676 pairs, of which
the engine accepts 263 — with `pg_class.relfilenode` either side of the
statement. An empty table is enough, and that is measured rather than assumed:
the same change on an empty table and on a hundred-row one gives the same
verdict, because the rewrite is a property of the statement.

**Ten pairs of distinct types rewrite nothing.** That is the whole list:

| From | To | |
|---|---|---|
| `character varying(5)` | `character varying(10)` | the bound only widens |
| `character varying(5)` | `character varying` | the bound goes away |
| `character varying(5)` | `text` | as above |
| `character varying` | `text` | the same type twice |
| `text` | `character varying` | unbounded both ways |
| `numeric(10,2)` | `numeric` | the bound goes away |
| `numeric(10,2)` | `numeric(12,2)` | the precision only widens |
| `timestamp` | `timestamptz` | **the session decides** — §4 |
| `timestamptz` | `timestamp` | as above |

**Everything else rebuilds**, including four that look free and are not:
`integer -> bigint`, `real -> double precision`, `character(5) ->
character(10)` — the blank padding is in every row — and `numeric(10,2) ->
numeric(10,4)`, where widening the *precision* is free and widening the *scale*
is not.

So the rule §3 was reaching for, and the one the dialect implements: **a rewrite
is avoided only where the target imposes no new constraint on the bytes already
stored.** The live suite re-measures the matrix and holds the dialect to every
accepted pair, so this table cannot go stale without a red build.

### The limit §3 states, measured

`relfilenode` is exact about the rebuild and says nothing about a scan, and the
gap is not small. **Measured** on 100,000 rows through `pg_stat_user_tables`:

| Statement | Rebuilt | Rows read |
|---|---|---|
| `ALTER COLUMN v SET NOT NULL` | no | **100,000** |
| `ALTER COLUMN w TYPE varchar(20)` (from `varchar(10)`) | no | **0** |
| `ADD CONSTRAINT … CHECK` | no | 100,000 |
| `ADD CONSTRAINT … UNIQUE` / `PRIMARY KEY` | no | 100,000 |
| `ADD CONSTRAINT … FOREIGN KEY` | no | 100,000 |
| `ADD COLUMN`, with or without a literal default | no | 0 |
| `DROP COLUMN` | no | 0 |

Carried as one fact the first two rows are the same change, and one of them is
free. So the estimate holds them apart, and `SET NOT NULL` — the example the
Limits section names — is the row that proves it has to.

### The locks, measured rather than recalled

Read from `pg_locks` inside each statement's own transaction, and for the
concurrent build from a second session, because it cannot be in one:

| Statement | Lock | Blocks |
|---|---|---|
| `ALTER TABLE … ALTER COLUMN …` (type, nullability, default) | `AccessExclusiveLock` | reads and writes |
| `ADD COLUMN`, `DROP COLUMN`, `RENAME` | `AccessExclusiveLock` | reads and writes |
| `ADD CONSTRAINT … CHECK` / `UNIQUE` / `PRIMARY KEY` | `AccessExclusiveLock` | reads and writes |
| `ADD CONSTRAINT … FOREIGN KEY` | `ShareRowExclusiveLock` | writes |
| `CREATE INDEX` | `ShareLock` | writes |
| `CREATE INDEX CONCURRENTLY` | `ShareUpdateExclusiveLock` | neither |

**The foreign key takes its lock on the referenced table too**, and takes no
exclusive lock anywhere. That is a cost on a table the change does not mention —
a key added to a small child blocks every write to a parent that may be
enormous — so `Estimate` carries the referenced table by name.

### §4 confirmed, and where `unknown` is the answer

The three cases §4 measured are unchanged, and each is a place the estimate
declines rather than guesses: the session's `TimeZone` (re-measured — no rewrite
under `UTC`, a rebuild under `America/New_York`), the default's volatility
(`DEFAULT 7` free, `DEFAULT gen_random_uuid()` a rebuild), and precision against
scale. The dialect answers `unknown` with the reason for each, and for the three
shapes this document's Limits record as unmeasured — a partitioned table, an
inheritance parent, and `ALTER TYPE` on an indexed column — whatever the static
table said. A number quoted about a shape it was not measured on carries the
authority of a measurement it did not come from.

One more, which is not a cost but is read like one: **`reltuples = -1` means
nobody has analyzed the table**, measured, with a thousand rows in it and
`relpages` still `0`. Read as a row count it says the change is free on the
largest table in the database.

### What is still open

**No SQL Server cost measurements were taken here either.** Whether
`int -> bigint` is metadata-only there remains the open question the Limits
section records; this step measured one engine and says so.

## Placement

Phase 5, with the catalogue. The one item that should not wait for it is §3's
boundary: if the estimate is built before it is written down, the pressure to
put "this rewrites the table" into the risk class will be at its highest exactly
when nobody has measured what that costs.
