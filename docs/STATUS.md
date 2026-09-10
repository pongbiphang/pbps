# Status

Where the product stands, what it can do today, and what is deliberately left
open. The reasons behind the choices are in [DECISIONS.md](DECISIONS.md); the
bugs behind the scar tissue are in [PITFALLS.md](PITFALLS.md).

## Phase

**Phases 0-4 complete** (0, 1, 2, 3, 3.1, 3.5 and 4) for SQL Server. The test
and clippy bar is in CLAUDE.md's "Development environment"; counts change too
often to record here. The copy-pastable pipelines SPEC 14.1 lists as P1 are in
[CI.md](CI.md).

First-run: `init` (`--env` / `--from` / `--url-env`), with staged validation
and pbps.yml installed last so a failed onboarding run leaves no partial project.

Declarations may carry reference data (`data:`, ADR-0004), planned offline and
against a target alike; `max_data_rows` in `pbps.yml` sets when `validate` says
a block has stopped looking like reference data.

Declarations may carry database roles and their grants (`role:`, ADR-0005);
membership stays each environment's own.

Offline: `plan` (`--check` / `--since` / `--base` / `--out` / `--sql` / `--dev`),
`validate` (`--since`), `fmt` (`--check`), `rename`, `rename-table`, `rename-role`, `drop`,
`drop-table`, `drop-role`,
`docs` (`--format` / `--out` / `--title`), `explain` (`--plan`), `doctor`
(`--env`), `schema` (`--kind declaration|config|envelope`), `completions`,
`man`. Every read-only command
takes `--format human|json`; `--no-input` is global. The envelope those
commands emit has a published schema of its own (`schema --kind envelope`,
`schemas/envelope.schema.json`), and a test validates each command's real
output against it.

Connected (each takes `--db <connection string>` or `--env <name>`): `pull`
(`--force` / `--data`), `plan --db` (`--staged`), `apply` (`--plan` /
`--checksum` / `--allow` / `--staged` /
`--resume`), `verify` (`--format json`), `snapshot` (`--force`), `baseline`
(`--reason`), `bootstrap` (`--sql`), `state list` (`--limit` / `--format json`),
`state prune` (`--keep`), `unlock`,
`status` (`--format json`).

All three intent channels now exist: the CLI commands, the YAML annotations, and
the TTY prompt of SPEC 6.3.

## Live tests

**Live tests**: the SPEC §11.5 invariants plus the Phase 3, 3.5 and 3.1 ones (the
ledger round-trip, the lock admitting one holder, a failed statement rolling the
whole plan back, the rename-impact queries, the probes counting real rows, a
staged checkpoint surviving `state_json`, a cross-schema rename stopping at the
name its statement declared, the module round-trip through
`sys.sql_modules`, `doctor` reading a real edition and permission set, and the
readiness check against a **real least-privilege login** created and granted
inside the test container — `sa` holds `CONTROL` and short-circuits the whole
permission list, which is how three permission bugs survived the first live
test — and the reference-data path: the DML, the row read-back, drift on rows,
the pre-delete probe's dynamic SQL, and the binary end to end through
`bootstrap`, `verify`, `plan --db`, `apply` and `pull --data`; and a plan
that *creates* a table with a foreign key, applied through the real gate —
the shape the apply guard got wrong twice because nothing here applied one —
and one that adds a column, a key, a unique, an index and a foreign key to a
table already there, and one that returns a declared cell to its default) run
against a real SQL Server in Docker:
`scripts/live-tests.sh` (set `PBPS_TEST_PORT` if 14330 is taken; the engine is
pinned by digest there and in CI), or set
`PBPS_TEST_DB` and `cargo test -p pbps-mssql --test live -- --ignored`. The
script also runs `pbps-cli`'s ignored tests, which include the `plan --dev`
rehearsal. They are `#[ignore]`d so the ordinary suite stays offline; CI has a
dedicated job. PostgreSQL has its own suite and its own job:
`scripts/live-tests-pg.sh` (set `PBPS_TEST_PG_PORT` if 54320 is taken), or set
`PBPS_TEST_PG_DB` and `cargo test -p pbps-pg --test live -- --ignored`. Its
ledger tests each work in a database of their own, created and dropped by the
test: the ledger is one pair of tables in `public`, so two tests sharing a
database would take each other's lock. They cover the same §11.5 invariants —
the round trip, the lock admitting one holder and naming it to the second, a
staged checkpoint surviving `state_json`, and a ledger that was never
initialized, one that is empty and a server that cannot be reached staying three
answers — plus what only this engine can be asked: a lock contended inside a
caller's transaction that leaves the transaction alive, a recorded time that
does not move with the reading session's `DateStyle`, and `doctor` against a
**real least-privilege role** whose every table privilege still does not let it
`ALTER` the table it does not own. When touching the emitter, the catalog queries or the ledger, run
them — they have caught four bugs the unit suite structurally could not: FK
ordering between two new tables; `EXEC()` rejecting function calls in its
argument; `sql_expression_dependencies` returning one row per referenced
*column*; and check constraints arriving as dependencies of their own table.
The module round-trip is in the same category: only a real `sys.sql_modules` can
say whether what the emitter sent is what comes back.

## Open items

### Artifact format versions reset at the first release

The plan file, the state snapshot and the published editor schemas are each
several versions in, and the snapshot reader carries an upgrade path from the
older ones — but this tool has never been released: the workspace is `0.0.0`
and there is no tag. Those numbers therefore record a history nobody has, and
the upgrade path leads from versions no deployment ever wrote. **At the first
tagged release, reset `plan::CURRENT_VERSION`, `state::CURRENT_VERSION` and
`integration::SCHEMA_VERSION` to 1 and drop `state::OLDEST_READABLE_VERSION`'s
back-compatibility with the pre-release numbering** (DECISIONS 145). Until
then, bump freely: a plan file lives for the length of one deployment window,
and there is nothing in the field to invalidate.

### Return the driver to `tiberius` once it ships a release

(SPEC open question 10.) The driver is `tiberius-ng`, adopted because
`tiberius` 0.12.3 pins a `rustls` stack with four open advisories and had no
release since 2024. The original crate has since moved to the community-owned
`tiberius-rs/tiberius` repository and is active again (commits on 2026-09-02),
so the plan is to go back — after a crates.io release newer than 0.12.3 whose
`rustls` feature resolves `rustls >= 0.23`. As of 2026-09-04 there is no such
release and `main` still pins `tokio-rustls 0.24`, so moving back now would
reinstate every advisory exception. The move is one line in the workspace
`Cargo.toml`, then `cargo deny check`, then the live suite.

### No universal connection layer

(ADR-0007, open question 11.) ODBC and ADBC
sound like an answer both to that item and to dialect breadth; they answer only
the first. A connection layer replaces `pbps-db`'s ~300 lines and none of the
type catalogue, emitter, introspection, validation or probes. ADBC is refused
for SQL Server specifically: its driver's source is not published and its
licence forbids reverse engineering, which a tool claiming "the reviewed plan is
exactly what runs" cannot accept — and a proprietary binary cannot be forked,
so it is *less* recourse than the stale crate, not more. Dialect plugins are
declined separately: no stable Rust ABI, and a plugin API would freeze
`ChangeSet` while the model still moves.

## Roadmap

**Phase 3.1 is complete** — the usability foundation of SPEC 14, and every P0 row
of 14.1. It was placed ahead of the next dialect deliberately: broadening the
object model improves coverage, but these improve the first hour and every
failure after it.

**Phase 4, depth on SQL Server before breadth across engines**, is complete as
scoped: declarative reference data (ADR-0004), roles and grants (ADR-0005),
the `policies:` block and the first built-in analyzer catalogue (ADR-0008).

Reference data is **built**, both halves: the `data:` block, its `exact` and
`ensure` modes, the round trip through `fmt`, the rules `validate` reports, the
typed row changes with the `data-update` and `data-delete` risk classes, the
DML and its ordering; and against a target, the row read-back into `state_json`
under the recorded or declared scope, the row half of the drift comparison, a
saved plan that carries which rows the recorded state must cover, the
pre-delete probe counting the rows that still reference the row, and
`pull --data`. ADR-0004 lists the decisions taken on the way under
"Implementation status".

Roles and grants (ADR-0005) are **built**: the `role:` file, `r_` uids in the
ids file, `rename-role` / `drop-role` and the other two intent channels, the
differ with `revoke` (gated) and `grant-widen` (labelled, never gated), the
T-SQL, the catalog read-back, drift under the managed set, `validate`'s
target rule and `pull`. ADR-0005 lists the decisions taken on the way.

The `policies:` block and the first analyzer catalogue (ADR-0008) are
**built**: rules and suppressions in `pbps.yml`, `validate` (with `--since`)
at the declaration point, `plan` and `plan --db` at the plan point with the
findings carried in the saved plan, and refusal of the plan at `error` before
anything is written. ADR-0008 lists the decisions taken on the way.

Depth before breadth was chosen against the obvious ordering — engine count is
what every comparison table measures — because a second dialect doubles the
surface every later feature is built twice for, and does it while the first
engine still cannot express an organization's own rules. The reasoning is in
SPEC 12 and open question 9.

**Phase 5** is the PostgreSQL dialect, the touchstone for the `Dialect`
abstraction. Its design is recorded, grounded in measurements taken on a real
PostgreSQL before any dialect code exists — each ADR's "Limits" section names
what was not measured, and the PostgreSQL live suite is Phase 5's first
deliverable:
[ADR-0009](ADR-0009-postgres-modules.md) (modules — overloading makes the
identity name plus argument types, which ADR-0002 anticipated),
[ADR-0010](ADR-0010-postgres-privileges.md) (privileges — the role is not the
portable unit, the collision ADR-0005 recorded),
[ADR-0011](ADR-0011-dialect-seam-under-a-second-engine.md) (the dialect seam:
what Phase 0 got right, and three amendments),
[ADR-0012](ADR-0012-postgres-type-catalogue.md) (the type catalogue, and why
"safe" and "cheap" are different axes),
[ADR-0013](ADR-0013-postgres-reference-data.md) (reference data — the
collision ADR-0004 never recorded) and
[ADR-0014](ADR-0014-driver-seam-tested.md) (what a second driver costs
`pbps-db`, measured with a spike). Each ADR's "Placement" section says what
lands with the first PostgreSQL commits and what must land before them; the
model changes come first, because format is the most expensive thing here to
change late. The seam preparation the ADRs place *before* the crate has landed
(DECISIONS 193–195): the server error code is text, the transaction framing is
the dialect's, the shared definition scanner nests block comments, and the
dialect crate's header states reasons that hold. ADR-0009 §1's model change has
landed with them (DECISIONS 200–203): a module is keyed by a typed `ModuleId`,
namespace sharing and overloading are dialect questions, and the state snapshot
and the saved plan bumped to 6 and 5 — every older snapshot refused, because
the meaning of the module map changed and a partial reading of it would be a
silent wrong answer. So has the declared record of ADR-0009 §2.2 and ADR-0013
§3–§4 (DECISIONS 207–209): the state keeps what each object was declared as
beside what it read back, the differ compares declared against declared and
falls back to the read-back where nothing was recorded, and the snapshot is 7
with 6 still readable. That fixed a shipped loop on SQL Server — a check or a
filtered index restated on every connected plan — and left the bindings'
recording, which needs a search path, to the PostgreSQL crate. And the
permission widening of ADR-0010 §6 (DECISIONS 210–211): `Permission` is the
union of the engines' words, SQL Server refuses the five it lacks from one
table in `validate`, `emit` and the read-back, role existence is a dialect
capability SQL Server answers `true` to, and the editor schema lists the words
(schema version 7). With that, the three model-format steps of Phase 5 are in.

The crate itself has started: step 1 of ten (issue #75) makes the **connected**
boundary polymorphic — `pbps_db::Conn` is an enum over `tiberius` and
`tokio_postgres`, one module per driver, and the 22 `pbps-mssql` functions that
take `&mut Conn` are untouched (DECISIONS 225). `pbps-pg` exists, holds no
driver, and refuses by name for every part not yet built. Two long-standing
seam defects landed with it: `normalize_definition` now takes each engine's own
lexis instead of inheriting SQL Server's scanner (DECISIONS 226), and
`normalize_type` has a stated contract under which `serial` is refused rather
than normalized (DECISIONS 227). A PostgreSQL live suite runs against a
digest-pinned server, and found on its first connection that the seam panicked
where two rustls providers were compiled in (DECISIONS 228).

Step 2 of ten (issue #77) is the **type catalogue**: what the engine spells
back, and how safe it is to change one type into another. Every row was
measured on the pinned server and the live suite re-measures it — each declared
spelling created and read back through `format_type`, and a twenty-by-twenty
matrix of `ALTER COLUMN ... TYPE` on an empty table, which is what defines
`Incompatible` (DECISIONS 240, 243). The catalogue is closed and its bounds are
the engine's rather than SQL Server's, including two the engine does not
enforce for you. Two declarations are refused for a reason that is not about
PostgreSQL at all: an array, and a precision on `time` or `timestamp`, which
this engine spells inside the name (`timestamp(3) with time zone`) and a
`ColumnType` has nowhere to put — the model change that lifts both is issue
#130 (DECISIONS 242). ADR-0012 §3's boundary is written down with the catalogue
rather than with the estimate that will use it: `integer -> bigint` rewrites a
million-row table under a lock that blocks readers and is still `Safe`, because
risk is about loss and never about duration (DECISIONS 241).

Step 3 of ten (issue #78) is **introspection**, and it keeps the SQL Server
crate's split for the reason that split exists: only `catalog` runs SQL, and
the assembler it hands rows to is a function of its input that every rule can
be tested through. Reads pin the canonical empty `search_path` and put back
whatever the session had — measured, the same database renders `REFERENCES
p(id)` under one path and `REFERENCES public.p(id)` under another, and two
operators comparing those took a snapshot each of drift nobody caused
(ADR-0013 §3). The three verbatim expressions arrive as the engine respelled
them and are carried through untouched, because the state's `declared` record
is what makes that comparison honest and a normalizer here would hide it.

The whole read is one `REPEATABLE READ READ ONLY` transaction (DECISIONS 250):
five autocommit statements are five snapshots, and a table dropped between two
of them comes back as a live table with no columns at all — which assembles
cleanly and plans as a table whose every column was deleted.

What the model cannot hold is **named rather than missing**, which is most of
this step. Whether the object is also carried depends on what the difference is
about — a property of the object is left out, a fact about the rows already
there is carried (DECISIONS 251): a partitioned or foreign table, a domain or a type the catalogue
cannot spell, a generated column, `GENERATED BY DEFAULT`, a `gin` or expression
index, a null ordering that is not its direction's default, a `NOT VALID`
check, and a foreign key whose action is `RESTRICT` — a word `ReferentialAction`
does not have, and one that is not `NO ACTION` however close it looks
(DECISIONS 248). A constraint of a kind this reader has never seen is reported
with its own definition rather than folded into the nearest kind it knows; the
`NOT NULL` rows PostgreSQL 18 added to `pg_constraint` are what that rule is
for (DECISIONS 247).

Step 4 of ten (issue #79) is the **emitter**: `CREATE`, `ALTER` and `DROP` for
tables, columns, keys, constraints and indexes. Every statement it writes sets
the write `search_path` — the object's own schema, then the project's
configured extras — and gives it back in the same batch, because the three
verbatim expressions the model holds bind an unqualified name at creation and
all three are *refused* under a path that is only the object's own schema
(ADR-0013 §3, DECISIONS 259). The two settings that decide how a definition
parses cannot ride there at all: a simple query is lexed as a whole before any
of it runs, so `standard_conforming_strings = on` and `check_function_bodies =
on` are pinned by the transaction framing, the earlier batch a transactional
apply runs (DECISIONS 260), and so are the seven that decide what a declared
expression *means* or what a conversion writes back — `DateStyle`, `TimeZone`,
`IntervalStyle`, `timezone_abbreviations`, `transform_null_equals`,
`bytea_output` and `extra_float_digits`. They are constants rather
than per-object, and measured, the same check constraint stores a different day,
a different instant, an interval with the opposite sign, a time fifteen and a
half hours out, and a predicate that is no longer the one that was written —
without a word (DECISIONS 267). A staged apply opens no
transaction and needs the same pin on its connection; it lands with the ledger
in step 8, which is the step that builds that connection.

Four refusals are the step's substance rather than its edges. A bare-literal
default on a setting-sensitive column is refused offline with the resolved
spelling named — measured, the same declaration stores 2026-01-02 under
`DateStyle` MDY and 2026-02-01 under DMY, silently either way (DECISIONS 261). A
type change this engine will not make on its own is refused where the plan is
built, with `USING` named and the two-step remedy, and never performed under a
cast nobody declared (ADR-0012 §5, DECISIONS 263), and so is one it *will*
make but only by consulting the session: a change that gains or loses the time
zone stores a different instant depending on the zone the applying session
happens to hold, so it is refused by name with the zone written out in the
remedy (DECISIONS 268). And `online` becomes
`CONCURRENTLY` only for an index with no filter, because a concurrent build
cannot share a batch with the path a filter would be bound under — the
statement says `non_transactional` and `own_batch` about itself, so a plan
carrying one is refused at plan time rather than halfway through an apply
(DECISIONS 262).

Two rules came out of the emitter's own fixpoint rather than from the issue.
A table the pull would never read is refused offline — a schema of
`pg_catalog`, `information_schema` or any name beginning with `pg_`, or a table
called `__pbps_state` or `__pbps_lock`, which the reader hides in every schema
(DECISIONS 274) — and a schema named `$user`, which every `search_path` reads
as the deploying role's own schema however it is quoted, so the table would be
created and its unqualified names would bind somewhere else (DECISIONS 275).
The pull never reads the first group — and
`pg_temp` is worse than invisible: measured, it is the parser's alias for the
session's temporary schema, so the declaration yields a `pg_temp_58.t` that
disappears with the connection (DECISIONS 273). And a nullable primary key
column. SQL Server refuses the table; measured, this engine
accepts it and sets `NOT NULL` itself, so the declaration and the database
disagree from the moment the table exists and the `DROP NOT NULL` that would put
it back is refused for ever (DECISIONS 266). The rest of the other dialect's
key-column checks are absent here and are issue #175 — they fail at the server,
which is late but not silent.

`CREATE TABLE` names `USING heap` rather than leaving the access method to
`default_table_access_method`: the reader accepts only heap, so a table created
under another method is created successfully and then unreadable as a managed
one. It is a clause and not a session pin because a clause survives the rendered
`--sql` script, which carries no framing (DECISIONS 272).

The live suite runs the three shapes issue #79 named — a created table with a
foreign key, a table already there gaining a column, a key, a unique, an index
and a foreign key, and a bootstrap whose next plan must be empty — each as
emit, read back, compare, against a plan the differ produced.

Step 5 of ten (issue #80) is **modules**, and its finding is that
`CREATE OR ALTER` was buying grant preservation on the other engine and this one
will not sell it. So every module change is a drop and a create, always — the
conditional was removed because *which* edits `CREATE OR REPLACE` can express is
decided by text §8.2 forbids parsing, and measured, the replace path drops
`reloptions` anyway (ADR-0009 §3). A trigger is the one kind whose emitted
prefix is not the SQL Server shape: this grammar puts the table after the event
list, so the table is in the identity **and** in the body, and a declaration
where the two disagree is refused offline — measured, the engine accepts it
without a word and the mismatch only surfaces a plan later (DECISIONS 302).
A routine argument became its own text type, which closes issue #59: measured,
of one function's twelve identity arguments the column catalogue knows three by
name, and the rest are arrays, a quoted name whose case the engine keeps, and a
schema-qualified domain (DECISIONS 301, 303).

The pull reads views, routines and triggers back, cutting the declaration out of
the statement the engine deparses by stepping over the name rather than
searching for a bracket; a statement it cannot cut is named and left out, never
recorded with an empty body (DECISIONS 304). Extension-owned objects are left
out silently, because they are somebody else's objects and not a limitation of
the model (DECISIONS 305).

What a connected plan has to know before it rebuilds one is read from the
catalog and not from a list: an ACL, a revocation from `PUBLIC` that is the
*absence* of a row, an owner a rebuild would transfer, `reloptions`, a view
column default in `pg_attrdef`, a trigger's `tgenabled`, and the grants a *new*
object would arrive with from `pg_default_acl`. Every one of them refuses today,
because roles and grants are step 6 and there is no declared grant for one to
come back from; step 6 narrows that and ADR-0010 §5 keeps `PUBLIC` on the
refusing side for good (DECISIONS 306). The read is taken under the object's own
lock where the account can take one — a view's own, a trigger's parent table's,
a routine's `pg_proc` row lock — and says so where nothing serialized it, which
is the honest answer for the accounts this tool is built for. The dependency
enumeration is over every reverse `pg_depend` edge and not only modules: an
undeclared or unrepresentable dependent refuses and is named, a declared one is
what the plan has to drop and restore around the rebuild, `DROP … CASCADE` is
never written, and a caller only a name scan can see is **reported** — a name is
not an identity where routines overload. And a same-named object this plan
introduces on a module's write path rebuilds that module in the same plan rather
than one plan late, on a name and a path rather than a position (DECISIONS 307).
None of this is reached by a command yet: the CLI still refuses the dialect, so
the live suite is what exercises it.

**Step 8 is in**: the ledger, the lock and `doctor` (DECISIONS 284–292). The
two tables live in `public` — the schema every database is created with, and
the one grant narrower than the `CREATE` on the database a `pbps` schema of its
own would need — and the qualified spelling now belongs to each dialect, with
`pbps-db` keeping only the two names that are the same on both. The lock stays
a table, because an advisory lock dies with the session that took it and this
one exists to survive a pipeline that did; it is taken with `ON CONFLICT DO
NOTHING`, because a failed statement here aborts the caller's whole transaction
and the other dialect's "insert, then name the holder" would leave nothing to
answer with. Times are defaulted from `clock_timestamp()` and rendered by
`to_char`, so two entries of one transaction are not simultaneous and a session
that renders dates its own way does not move the history. And `doctor` asks
about **ownership**: measured, a role holding every privilege PostgreSQL has to
give on a table cannot alter it, so the SQL Server list ported across would have
called that environment ready. The staged apply's session pins land with it
(`Dialect::session_pins`), which is what `transaction_framing` said step 8 owed
it.

Five things review found, each measured (DECISIONS 293–297):
`CREATE TABLE IF NOT EXISTS` is refused for want of `CREATE` on the schema **even when the table is already there**, so the ledger's
DDL is not sent when the catalog says there is nothing to create; and
`has_table_privilege` answers `true` for a table in a schema the role may not
enter, so `USAGE` is required wherever objects are used and not only where they
are created — on the ledger's schema, and on the schema of a referenced foreign
key target, which also wants the `SELECT` its probe performs. And that target
may be a **partitioned** table, which this model cannot hold and somebody else's
schema may perfectly well contain: filtered to ordinary tables it read as
absent, and an absent securable is asked for nothing. The last two are the ledger
again, and one shape: tolerating an error is only tolerable in autocommit,
because inside a transaction that error aborts it — so every arm that turns a
failure into a value runs under a savepoint, `is_initialized`'s `42P01`
included; and a tolerated `42P07` is verified against the catalog rather than
believed, because it names *a* relation the DDL would create and not the
ledger.

Step 7 of ten (issue #82) is **reference data**, the connected half of ADR-0004
on this engine. Two of ADR-0013's findings are places where following the SQL
Server rule produces a tool that is confidently wrong. The pre-delete probe
counts **every** foreign key and never reads `convalidated`: it looks exactly
like the `is_disabled` flag the other probe skips a key by, and measured, it
means the opposite — a `NOT VALID` key still refuses both a violating insert and
the parent's delete (DECISIONS 320). And an identity-keyed `data:` block is
**refused** rather than pinned: measured, `OVERRIDING SYSTEM VALUE` leaves the
sequence behind, so the deployment verifies clean and the application's next
insert fails on the primary key (DECISIONS 321).

What pbps renders is setting-independent by construction rather than by scope —
`E'…'` with backslashes doubled, `decode('…','hex')` for a `bytea` — because a
write takes no settings scope and canonical hex under
`standard_conforming_strings = off` is *accepted* while storing different bytes
(DECISIONS 319). The read-back renders every column with one expression and
lets the canonical scope fix the spelling, which is where this engine differs
from the other's per-type `CONVERT` styles (DECISIONS 322), and it recognises a
literal default through the cast this engine welds on, or a declaration that
omits a column and one that spells it would stop comparing equal
(DECISIONS 323). Each row statement is a `DO` block, so the write and the
checks that hold it to what the plan reviewed are one statement even under a
staged apply (DECISIONS 328), and the delete's guard locks the parent row —
this engine's own foreign-key machinery takes a `FOR KEY SHARE` there, so a
child arriving after the probe waits for the delete instead of racing it
(DECISIONS 326). The probe's dynamic SQL runs through `query_to_xml`, which is
this engine's only way to run generated SQL from inside the one `SELECT` a
probe is (DECISIONS 325).

Every comparison this step makes is a comparison of *text* — both sides through
the column's type, byte for byte under `COLLATE "C"` — on the read path and the
write path alike, so a column whose collation calls two spellings equal cannot
hide a hand edit, and a `json` cell is compared like any other despite the type
having no `=` at all (DECISIONS 329, 330, 331). The delete's guard leaves out
the one row it is about to remove, so a self-referencing row can be deleted
(330), and refuses rather than trusting a count row-level security has filtered,
because this engine's referential actions are not filtered by it (329). A write
left to a default the probe cannot evaluate, on a column a live foreign key into
the deleted row's table spans, is refused before the first statement, as
DECISIONS 124 asks (331). The probe reads every key the delete will meet: a
relation only where its key reaches it, `ONLY` for an inheritance parent and
in full for a partitioned one (334); a table the session cannot count through,
filtered by a policy or unreadable outright, as a refusal rather than a zero
(333, 335); and every key this plan itself adds, asked about from the plan
alone — the synthetic catalog row of 335 gave way to it (345) — counted
through the value a column it adds is backfilled with, against the parent rows
the plan leaves there, through the column's type and the type the plan gives
it (336–341, 345); and a session is refused for what its grants leave it unable to
count — the schema first, then the table or its columns — not for lacking a
grant on the whole table (340, 342). Every probe answers in `int4`, the width
the runner reads, clamped rather than overflowing (342, 343); a child the plan
creates is counted for the rows it inserts before its key exists, and a key
column an insert leaves to an identity is refused (343); a key whose delete
action this session will not run — triggers disabled, or the replica role —
is not one the delete meets (344); and the guard's own policy check asks
about the referencing relations, not the catalog's per-partition copies of
their keys (346); and whether a row the plan writes holds a NULL in a key is
decided from that row, not from what the table is backfilled with (347),
and a stored row's from its own stored columns, so a NULL there is never
refused for a backfill it cannot meet (348); a NULL an update leaves alone
is a NULL of the tuple it writes (349), and a default spelled `CAST(NULL AS
type)` is that NULL (350) — and, this engine keeping no NULL default at all, a
column declaring one is refused at the declaration (351). A stored key is
compared under the referenced column's collation and through the operator the
constraint records (352), a planned key under the referenced column's collation
spliced in by the engine (353), and the parent's own readability is asked before
its children are counted (354); a default is a literal in every spelling this
engine reads one (355), `UESCAPE` included (356), and the cast scanners read a
string as the engine does (357), a comment in a default being whitespace (358), inside a cast too (359), and a
number a number in every base (360); a typed NULL default is refused only where
the engine erases it, a NULL of the column's own unmodified type (361), read past
any comment inside the type (362), a line comment ending at either newline and a
comment standing as the gap around `AS` (363), a string, a `)` or a quoted identifier closing its own token before `AS` and a `pg_catalog.` qualification or quotes on the cast type read as the grammar reads them (364), and a sign read through the trivia, groupings and casts before its operand (365), and an inserted parent row meeting an arriving child under the referenced column's collation (366), and a typed literal read as the constant it is (367), with only an interval qualifier admitted after its string (368), and a Unicode-escaped type name read as the name it spells (369).

The key-collision check is the engine's, asked under the key column's own
collation — measured, `collisdeterministic` says nothing about case, so a rule
reading it would refuse a valid declaration — and offline `validate` therefore
**says it did not check** rather than reporting clean, through a new
`Dialect::declaration_notes` (DECISIONS 327).

Not in this step: the CLI still refuses the `postgres` dialect outright
(`main.rs`'s `dialect()`), so the end-to-end path through `bootstrap`,
`verify`, `plan --db`, `apply` and `pull --data` cannot be exercised through the
binary until step 10 wires it. Everything above is covered by the dialect's own
live suite instead. Composite keys remain deferred, as in ADR-0004.

What remains is steps 6, 9 and 10: roles, probes, and the suite in full.

**Phase 6** is the optional local UI (ADR-0006). The guardrail against a policy
SaaS refuses *a control plane that holds the approval*, not a screen: the UI
renders the typed JSON of Phase 3.1, composes intent as a git commit, triggers
the same checksum-pinned plan, and stores nothing authoritative. It is late
because a UI built before that JSON exists would have to parse human output or
reimplement validation. **Scope is single-user and local, and that is the
open-source boundary** — multi-tenant or hosted is reserved as a possible
commercial offering and gets its own ADR. It does not inherit permission from
ADR-0006: commercial pressure pushes hardest towards the UI holding the
approval, which is the one thing ADR-0006 refuses. How it is built is decided
in [ADR-0015](ADR-0015-local-ui-implementation.md): the UI runs the `pbps`
binary as a subprocess and links none of the crates, serves a page embedded in
the binary with no build step, refuses any request without its per-launch
token, reads no credential itself, and commits through the user's own `git`.
What remains is steps 6, 9 and 10: roles, probes, and the suite in full.

The steps are in issue #64. Step 1 is that ADR. Step 2 froze what the page
reads: the envelope's schema is published and checked against real output, and
`state list` gives the ledger timeline `status`'s newest-entry row cannot
(DECISIONS 214-216).
