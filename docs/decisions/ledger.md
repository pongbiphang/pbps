# The ledger and the state snapshot

The tables pbps keeps in the target, the lock, and the versions of the recorded
state. Part of the [decision record](../DECISIONS.md), which says how to add an
entry here.

<a id="decision-4"></a>

4. **`StateSnapshot` carries `ids`**: state and identity travel together.

<a id="decision-11"></a>

11. **`__pbps_state` protects against mistakes, not tampering**: only the
    deployment account writes `__pbps_state` / `__pbps_lock`; the audit
    baseline is git + CI logs.

<a id="decision-19"></a>

19. **The ledger's T-SQL lives in `pbps-mssql::state`**, its types and prune
    policy in `pbps-db::ledger`. The columns beside `state_json` are projected
    from the snapshot at record time, never passed separately, so they cannot
    come to disagree with it. `applied_at` is the *server's* clock, formatted
    as ISO 8601 text by the query — tiberius is built without `chrono`.

<a id="decision-46"></a>

46. **The lock is asked before initialization, in all four commands.** `status`,
    `doctor` and `explain` each asked `is_initialized` first, because
    `lock_holder` used to select from a table a never-initialized database does
    not have. Item 45 removed that reason; the ordering then only hid the
    half-present ledger. `doctor` called it "uninitialized" and exited **0**
    with an apply blocked, and `explain` printed the approval command for a
    target that was changing under it. A guard whose reason has gone is not
    harmless — it is a filter nobody re-reads.

<a id="decision-84"></a>

84. **The state snapshot is version 4, because the schema gained roles.**
    Adding `roles` to `Schema` under version 3 let an older binary read a
    newer snapshot with serde dropping the field: its `verify` compared
    every table and no role, and said "no drift" about grants it never
    looked at. A reader refuses a version it does not know, so the bump is
    what turns a partial reading into a refusal. The ids file stays at
    version 1 (its `roles` section is a compatible evolution by design), and
    an older binary meets the role *files* first, which it refuses to load.

<a id="decision-138"></a>

138. **A version 3 state snapshot is still read, as an environment with no
    managed roles.** 84 bumped the snapshot to version 4 when `Schema` grew
    `roles`, so that an *older* client would refuse a state it could only
    read in part — and the check was equality, so this client refused the
    older version too. Every environment recorded before this release holds
    a version 3 entry as its latest, and the recovery the message named,
    `pbps baseline --reason ...`, reads that entry first and failed the same
    way: a deployed environment had no path to the new version at all. The
    fields 4 added default to empty on read, and an environment recorded
    before roles were managed is exactly one with no managed roles, so 3 is
    read as its own; the next record writes 4. Anything older than 3 is
    still refused, for the reasons 2 and 3 give, and anything newer for the
    reason the check exists.

<a id="decision-203"></a>

203. **The state snapshot's oldest readable version becomes its current one.**
    A version 5 snapshot spells a trigger as `app.audit` with its table in a
    field beside it. This build reads that key as a view and has nowhere to
    put the table, so "no modules of that shape" would be a *missing* reading
    presented as a true one — the failure this tool exists to prevent. The
    meaning of the module map changed, not just its contents, so the snapshot
    is refused with the remedy (`pbps baseline`) rather than upgraded in
    place. Cheap because the format numbers are still pre-release and reset at
    the first tagged release (145). The saved plan goes 4 → 5 for the same
    change with the same reasoning, and refuses the same way.

<a id="decision-207"></a>

207. **The state keeps what was declared beside what it read back, and a
    version 6 state reads as having declared nothing.** `StateSnapshot`
    gains one field, `declared`, holding the three ADR-0009 counts: each
    managed module's definition as declared when last written, the three
    verbatim expressions — `Column::default`, `CheckConstraint::expression`,
    `Index::filter` — as declared, and the bindings of ADR-0013 §3. One
    struct rather than three loose fields, because the three move together:
    `bootstrap` records them from the declarations it created everything
    from; `apply` takes the previous state's record and advances it by the
    plan — from the plan alone, since `apply --plan` needs nothing else (SPEC
    §7.3): a change that writes an object carries the text it wrote, a drop
    forgets it, a rename re-keys it; a staged checkpoint carries the previous
    record, since nothing plans against a checkpoint; `baseline` and
    `snapshot` record none, because they applied nothing and either obvious
    filling is wrong (ADR-0009 §2.2). The version goes to 7, and 6 stays
    readable: a version 6 state recorded nothing as declared, so an empty
    record is what it truly says — the rule that keeps 4 readable (160) and
    not the one that refused 5 (203), where the same spelling had changed its
    meaning. The issue that scoped this said such a state would be refused;
    reading it is the better answer, and this entry is where the change of
    mind is written down.

<a id="decision-218"></a>

218. **An entry this build cannot read is carried, not thrown.** `state list`
    reads rows written by every version that ever touched the environment,
    including ones older than `OLDEST_READABLE_VERSION`. Parsing each row into
    a `StateSnapshot` and returning `Err` on the first failure meant one
    unreadable row erased the whole timeline above it — the newest entries, the
    ones a person is looking at the list to find.

    So the reader projects the ledger's own columns (id, time, kind, operator,
    provenance) and treats the recorded state as optional: a row that will not
    parse, or whose version this build does not read, keeps every column the
    ledger stores and carries the reason in `unreadable`, with a
    `state.entry-unreadable` finding naming the row. `history` keeps the
    stricter contract — a caller asking for states wants states — and the two
    doc comments point at each other. (The one finding id named here became
    two in 222, once the two ways a row can be unreadable were told apart.)

    This is the repository's absent/empty/unreadable rule applied one level
    down: it holds for a row as much as for a ledger.

<a id="decision-219"></a>

219. **Presence is asked by attempting the statement, never by `OBJECT_ID`.**
    `is_initialized` asked the catalog whether `dbo.__pbps_state` exists. The
    lock reader had asked the same way and was fixed one shape earlier; the
    ledger reader was not swept with it.

    Measured against the pinned server, with a contained user holding no
    permission on an existing `__pbps_state`: `OBJECT_ID` answers NULL and
    `HAS_PERMS_BY_NAME` answers 0 — the same answers an absent table gives.
    Attempting `SELECT TOP (0) 1 AS present FROM dbo.__pbps_state` separates
    them: **208** when the table is absent, **229** when it exists and is
    hidden, **207** when it is there with a shape this build does not know, and
    every other failure stays a failure. `TOP (0)` still resolves the object and
    still checks the permission, so the probe costs no rows.

    Fixed in `is_initialized` itself rather than at the new call site: `latest`,
    `history`, `timeline`, `prune`, `doctor` and `explain` all asked through it,
    and each turned "not authorized to look" into "this database has no pbps
    ledger" — `doctor` reporting `uninitialized`, `explain` offering
    `bootstrap`, `state list` printing an empty history for an environment with
    years of it. All six inherit the fix with no signature change, and both
    outside callers already routed an error correctly.

<a id="decision-222"></a>

222. **"Older than this build reads" and "damaged" are two answers, not one.**
    `timeline_from_row` parses the recorded state and then version-checks it,
    and both failures were carried as a string. The warning built from that
    string said the entry "was recorded by a version this build cannot read" —
    so a row whose JSON is truncated told the operator to go and find a newer
    pbps, which is not a thing that exists for a damaged row.

    `TimelineEntry` now holds `Result<StateSnapshot, Unreadable>`, with
    `Unreadable::UnsupportedVersion` and `Unreadable::Malformed`. The envelope
    carries the same split as a tagged object — `{"kind": "malformed",
    "detail": ...}` — because a page that draws "upgrade pbps" must be able to
    decide *not* to draw it, and reading that out of a sentence is not something
    a schema can promise. Two finding ids for the same reason: an id is what a
    consumer keys on, and these two are different jobs.

    A `Result` rather than a snapshot beside an optional reason: exactly one of
    the two is true of every row, and the struct that could hold both needed a
    comment saying it never would.

    **One reader, not two.** The version-before-shape order `from_json` was
    given for #50 is exactly what this needs, so `StateSnapshot::read_json` *is*
    that reader with its failure typed, and `from_json` is `read_json` with the
    two flattened into the one sentence a reader of a single state wants. A
    second copy of the ordering in the ledger crate would have been a second
    place to get it wrong.

    That ordering is also what makes the distinction worth drawing. Without it,
    an old row fails on whichever field of its older shape serde reaches first
    and is `Malformed` — measured, on a base before it, and it had a fixture of
    this test passing while proving the wrong thing. With it there are three
    cases and the reader gets all three right: below the range is
    `UnsupportedVersion` and says which command fixes it; unparseable is
    `Malformed`; and a *readable* version carrying an unknown field is
    `Malformed` too, because within a version this build reads, a field it does
    not know is a hand-edited or corrupt row. The live test carries one of each.

<a id="decision-284"></a>

284. **The PostgreSQL ledger lives in `public`, and the qualified names move
    out of `pbps-db` into the dialects.** SPEC §8.1 puts the two tables in
    `dbo`, which is a SQL Server schema, and `pbps_db::ledger` held
    `dbo.__pbps_state` as a constant — in the crate documented as holding no
    engine SQL. It now holds `STATE_TABLE_NAME` and `LOCK_TABLE_NAME`, the two
    words that are the same on every engine because they are this tool's own,
    and each dialect holds the qualified spelling beside the statements that
    use it. Each is asserted to be its own `LEDGER_SCHEMA` plus that name, so a
    schema edited in one place and not the other cannot leave two constants
    that each look right.

    `public` rather than a `pbps` schema of its own, decided on what it costs
    the deployment role: creating a schema needs `CREATE` **on the database**,
    which covers creating *any* schema, while creating two tables in `public`
    needs `CREATE` on that one schema. The narrower grant is the one a tool
    that argues against `db_owner` should be asking for. `public` is also the
    schema every database is created with, which is what `dbo` is on the other
    engine.

    Measured on 18.6, and it is why `doctor` asks about it: since PostgreSQL 15
    `public` is `{pg_database_owner=UC/pg_database_owner,=U/pg_database_owner}`
    — every role has `USAGE` and none has `CREATE` — so a fresh deployment role
    cannot create the ledger until somebody grants it.

    This is what #185 was waiting for: the pull's exclusion filter hides
    `__pbps_state` in *every* schema because it has no schema to name. It has
    one now, and qualifying the filter stays that issue's work with that
    issue's tests.

<a id="decision-285"></a>

285. **The lock is a table on this engine too, not an advisory lock.**
    `pg_advisory_lock` is the obvious PostgreSQL answer and it answers a
    different question: it is held by a *session*, and measured, `pg_locks`
    shows nothing of a `pg_try_advisory_lock(42)` once the connection that took
    it has gone. SPEC §8.1's lock is the one thing here that must **survive**
    the pipeline that took it — a pipeline killed mid-apply is exactly when the
    next one must not start — which is also why `pbps unlock` exists as a
    command rather than as a timeout. A row in a table is what a dead process
    leaves behind.

<a id="decision-286"></a>

286. **The lock is taken with `INSERT ... ON CONFLICT (id) DO NOTHING`, and
    zero rows affected is the refusal.** The SQL Server ledger inserts and, when
    the insert *fails*, reads the holder to name it. That shape cannot be
    ported: measured on 18.6, a failed statement aborts the whole transaction,
    so the `SELECT` that would name the holder comes back
    `25P02: current transaction is aborted, commands ignored until end of
    transaction block`. A lock taken inside a caller's transaction would report
    nothing and destroy the transaction on the way.

    `ON CONFLICT` is still a gate rather than a check-then-act: a second
    inserter blocks on the primary key's index until the first commits and then
    does nothing, so exactly one caller ever sees a row count of 1. If the first
    rolled back, the second gets the lock. The live suite pins both halves,
    including that the contending caller's transaction is still usable
    afterwards.

    The one case the read cannot answer is a holder that released between the
    insert and the read. That is reported as contention without a name rather
    than as a lock this call did not win, because claiming it would be a claim
    that is false.

<a id="decision-287"></a>

287. **The ledger's times are defaulted from `clock_timestamp()` and read
    through `to_char`, never cast.** Two measurements, one per half.

    `now()` is the transaction's start time and does not move inside it —
    measured, two reads 300ms apart in one transaction return the same value —
    so a staged checkpoint and the entry that closes it would carry the same
    instant and read as simultaneous. `clock_timestamp()` is the statement's,
    which is what `SYSUTCDATETIME()` gives the other ledger.

    And the text is rendered rather than cast, because a cast is the reading
    session's business: measured, under `DateStyle = 'German, DMY'` the same
    value casts to `31.08.2026 09:14:22.517` — not sortable as text, not
    parseable as ISO 8601, and produced by an operator's own setting rather
    than by anything this tool did. `to_char(applied_at, 'YYYY-MM-DD"T"HH24:MI:SS.MS')`
    holds no locale-sensitive field (`TM` is what would make one), so it renders
    the same under every `DateStyle` and every `lc_time`. The column is
    `timestamp(3)` holding a UTC wall clock rather than a `timestamptz`, for the
    same reason one step further out: a `timestamptz` is rendered in whatever
    `TimeZone` the reading session has.

<a id="decision-288"></a>

288. **`is_initialized` attempts a statement here too, and this engine answers
    the three cases apart in the SQLSTATE.** DECISIONS 219 had to find that out
    by measurement on SQL Server, whose catalog *hides* an object a login has no
    permission on: `OBJECT_ID` and `HAS_PERMS_BY_NAME` both answer as if the
    table were absent, so "not authorized to look" arrived at every caller as
    "there is no ledger". Measured on 18.6, PostgreSQL separates them itself:
    `42P01` for a relation that is not there, `42501` for one that is and may
    not be read — and `42501` again where the *schema* is closed, which is the
    same answer for the same reason.

    The lookup is still not used, and that is measured too: `to_regclass` does
    not return NULL for an object in a schema this role cannot enter, it raises
    `42501`. So the guard would have to handle an error anyway, and where it
    does not raise it is silent about the difference. An absent *schema* answers
    `42P01` like an absent table, which is the right answer to "is there a
    ledger": there is not, and `bootstrap` makes both absences visible.

<a id="decision-291"></a>

291. **A reason is cut by characters here and by UTF-16 units there.** The
    column is `varchar(1000)` and this engine counts characters: measured,
    `varchar(4)` accepts four emoji and reports `length = 4,
    octet_length = 16`, where `NVARCHAR(4)` refuses them. A shared helper would
    have to be wrong on one of the two engines, so each dialect has its own with
    its own unit and its own test.

    The engine has two answers about overflow and only one of them is loud:
    measured, `'😀😀😀😀😀'::varchar(4)` truncates silently and returns four,
    while inserting the same value into a `varchar(4)` column is
    `22001: value too long`. The user-supplied `--reason` therefore reaches the
    column as it was written and fails loudly; only the best-effort audit paths
    truncate, where a failed attempt that cannot be recorded is the opposite of
    what the row exists for.

<a id="decision-292"></a>

292. **`ensure_tables` treats a concurrent creator's failure as success.**
    `CREATE TABLE IF NOT EXISTS` is not atomic against another session doing the
    same thing: the check and the create are two steps, and the loser gets
    `23505` on `pg_type_typname_nsp_index` — the row for the table's implicit
    composite type is where the collision lands — or `42P07`. Both mean the table is
    there now, which is what the caller asked for; anything else is still a
    failure. Every command that writes a ledger row calls this, so two
    pipelines starting together really do race on it.

<a id="decision-293"></a>

293. **The ledger's DDL is not sent when there is nothing to create**, and that
    is a permission decision rather than an optimization. Measured on 18.6: a
    role holding `SELECT`, `INSERT` and `DELETE` on both ledger tables and no
    `CREATE` on their schema gets `42501: permission denied for schema public`
    from `CREATE TABLE IF NOT EXISTS public.__pbps_state` — **even though the
    table is already there.** The engine checks the schema privilege before it
    notices the relation exists.

    That is exactly the least-privilege configuration SPEC §8.1 asks for and
    `pbps_pg::doctor` reports as ready, once 289's create-time requirement has
    been spent. Every `record` and every `lock` calls `ensure_tables`, so the
    whole deployment failed on a grant `doctor` had correctly said was no longer
    needed.

    So `ensure_tables` asks the catalog first, and this is **not** the question
    219/288 refuses to ask there. That one is "does *this caller* have a ledger
    to read", which only a statement can answer without turning "not authorized
    to look" into "nothing there". This one is "would `CREATE TABLE IF NOT
    EXISTS` do anything", which is about the database and not about the caller —
    so it is asked as a join on `pg_class` and `pg_namespace`, which are
    world-readable, and asked in the statement's own terms: *any* relation of
    that name, whatever its kind, because that is what `IF NOT EXISTS` looks
    for. A narrower predicate would send DDL the engine is about to skip, which
    is the failure this removes.

    The race is unchanged and still tolerated (292): between the probe and the
    `CREATE`, another session may create the table, and `23505`/`42P07` still
    mean it is there now.

<a id="decision-296"></a>

296. **Tolerating the creation race is not enough inside a transaction, so the
    `CREATE` runs under a savepoint.** 292 tolerates the loser's `23505`/`42P07`
    because the table is there now, which is what the caller asked for. Measured
    on 18.6, that is only true in autocommit: the error **aborts the loser's
    transaction**, so a bare `Ok(())` hands back a connection whose every next
    statement is `25P02: current transaction is aborted`.

    ```text
    A: BEGIN; CREATE TABLE IF NOT EXISTS public.__pbps_state (...);   -- holds the lock
    B: BEGIN; CREATE TABLE IF NOT EXISTS public.__pbps_state (...);   -- blocks
    A: COMMIT;
    B:   -> 23505 duplicate key ... pg_type_typname_nsp_index
       SELECT 1  -> 25P02: current transaction is aborted
       COMMIT    -> ROLLBACK
    ```

    That is not a hypothetical caller: `record` runs inside the apply's own
    transaction (147), so a deployment would have failed on a race this arm
    claims to have handled — and failed two statements later, with an error
    about a transaction rather than about the race.

    The savepoint is taken by **trying** it: `SAVEPOINT` outside a transaction
    block is `25P01` and harms nothing, so one round trip both establishes the
    savepoint and tells this call whether it is in a transaction at all. It is
    taken only on the path that sends DDL, which 293 has already made rare. The
    untolerated failures roll back to it too, so a caller can still run the
    diagnostics it wants to print — without that, even those come back `25P02`.

    This is the third instance of one shape in this dialect: an error path
    written for an engine where a failed statement costs a statement, on an
    engine where it costs the transaction. The other two are the lock (286) and
    the reason it does not read its holder after a failed insert.

<a id="decision-297"></a>

297. **Every answer this ledger gives by tolerating an error is taken under a
    savepoint, and the tolerated `42P07` is verified rather than believed.** 296
    fixed one arm; a sweep of the file — which is what PITFALLS' own entry says
    to do about this shape — found three more, and one of them was tolerating
    the wrong thing.

    **The savepoint, everywhere.** `is_initialized`'s answer *is* an error
    (`42P01`, there is no ledger), and so are `unlock`'s and `lock_holder`'s
    against a missing lock table. Measured, each aborts the transaction it
    happens in, so each returned `Ok(...)` on a connection whose next statement
    is `25P02` and whose `COMMIT` is a `ROLLBACK`. `latest`, `history`,
    `timeline` and `prune` all reach `is_initialized`, so one guard covers the
    readers. It is a struct (`Recoverable`) rather than four copies of the same
    three lines, because the next arm someone adds should have somewhere to
    reach for.

    **And the collision is checked, not inferred.** `42P07` names *a* relation
    the DDL would create, not the ledger. Measured, an unrelated
    `public.pk___pbps_state` — the name this DDL gives the ledger's primary key,
    and an ordinary name for something else to have — makes
    `CREATE TABLE IF NOT EXISTS public.__pbps_state` fail with `42P07` while the
    ledger is **still absent**. Read as "somebody else created it", that is
    `ensure_tables` reporting success over a database with no ledger, and the
    next `record` failing with `42P01` about a table this call said it had made
    sure of. So the answer comes from the catalog — both tables there, or the
    engine's own error, which names the squatter.

    The cost is one round trip per call that can tolerate something: the
    `SAVEPOINT` attempt, which outside a transaction is `25P01` and does
    nothing. That is the same round trip a separate "am I in a transaction"
    probe would cost, and it answers both questions.

<a id="decision-386"></a>

386. **The ledger is hidden by name *and* by kind.** SPEC §8.1 names two
    **tables**, and `modules_query` already reads that way: a view is kept
    whatever it is called, and only an ordinary table is filtered by name. The
    grants query applied the name filter to every `pg_class` row, so a project
    declaring a view `app.__pbps_state` had it pulled as a module while its
    `relacl` row was thrown away — the grant on it read back as absent, the
    apply's own read-back refused the plan for not having achieved its
    postcondition, and every plan after it proposed the same `GRANT` again.

    The filter is `relkind = 'r'` first and the two names second, so nothing
    but the tool's own kind of object can be hidden by carrying one of its
    names. `validate_table` refuses a declared *table* of either name
    (DECISIONS 274), which is why the table half needs no second thought and
    the view half needed this one.

<a id="decision-441"></a>

441. **A relation of another kind occupying a ledger name is caught after
    `ensure_tables`'s DDL, not by narrowing `ledger_is_there`'s probe.**
    Deferred from a review finding on #205 (issue #217): that probe joins
    `pg_class` on the two ledger names without filtering `relkind`, so a view
    of either name counts, and `ensure_tables` returns `Ok(())` for a database
    that has no ledger table at all.

    The narrow fix does not fix it, and this is measured on 18.6, not assumed.
    Filtering `relkind = 'r'` in the probe only makes it answer 0 where a view
    occupies the name; `ensure_tables` then still runs `CREATE TABLE IF NOT
    EXISTS public.__pbps_state`, which *also* skips a view of that name
    (`NOTICE: relation "__pbps_state" already exists, skipping`) and reports
    success. The probe deliberately asks the same question the DDL asks
    (DECISIONS 288) — any relation of that name, whatever its kind, because the
    point is to predict whether the DDL would do anything — so narrowing the
    probe does not stop the wrong prediction, it only moves which of the two
    questions gives it.

    So the confirmation goes **after** the DDL block instead — once a relation
    of each name exists, freshly created or already there, one catalog
    question filtered to `relkind = 'r'` decides whether both are ordinary
    tables, and names the occupant and its kind when one is not. That is a
    question the probe cannot answer earlier, because before the DDL has run
    or been skipped a view and an about-to-be-created table look the same to
    it.

    A kind check does not make the recording trustworthy, and is not asked to.
    Measured on 18.6: a matching auto-updatable view over a table with the
    ledger's own columns absorbs the insert (`INSERT 0 1`, `RETURNING id`
    yields a row, the row lands in the view's base table) — a *kind* check
    never sees this case fail, because the view answers correctly for `id`, and
    an ordinary table of the ledger's name and columns passes `relkind = 'r'`
    and does exactly the same silent redirection. What the confirmation buys is
    a clear, named refusal in the cases that already fail loudly one step
    later — `must be owner of table t` from `migrate_timeline_columns`'s
    `ALTER` on a view, or a confusing insert failure — not a guarantee that
    whatever answers `relkind = 'r'` is the tool's own ledger.

    Nor does the confirmation cover every path: it runs on `ensure_tables`,
    which only `record` and `lock` call. `prune` and `unlock` reach
    `is_initialized` (or, for `unlock`, nothing) instead and never call
    `ensure_tables`, so a decoy view still lets `prune`'s `DELETE_UP_TO` and
    `unlock`'s `DELETE_LOCK` write through it into the view's base table —
    measured on 18.6 on both, not assumed. Adding an `ensure_tables` call to
    either would make a command whose whole point is to delete also create
    ledger infrastructure, which this PR does not do; the gap is tracked as
    issue #396.

    A live test creates a view named `public.__pbps_state` over a table with
    the ledger's columns and asserts `ensure_tables` fails naming the view's
    kind, and that `record`'s `INSERT` never runs — the view's base table
    stays empty. A negative case beside it holds two ordinary ledger tables
    through both the creating and the already-there path of `ensure_tables`,
    and confirms `record` still succeeds.

    `pbps_mssql` does not share this shape, measured the same way: T-SQL's own
    guard is `OBJECT_ID(N'dbo.__pbps_state', N'U') IS NULL`, already filtered
    to `'U'` (user table), so a decoy view makes it evaluate true and send
    `CREATE TABLE dbo.__pbps_state`, which then fails loudly — measured,
    `Msg 2714: There is already an object named '__pbps_state' in the
    database.` — rather than skip. No corresponding change was made there.

<a id="dec-313-1"></a>

**DEC-313.1. Before its first write, a PostgreSQL command compares the ledger's two
tables with the recipe pbps creates them from, and refuses when they
differ.** (#313, amending SPEC §8.1's trust model.) A role with `CREATE`
on `public` can create `__pbps_state` and `__pbps_lock` before pbps
does, grant them to `PUBLIC`, and attach a trigger; the deployment
account's next `lock` or `record` then runs that trigger with its own
privileges. Measured on 18.6 with separate attacker and deployment roles:
the marker the trigger writes appears on the first `lock` without this
check, and never with it. #217 had already refused a ledger name held by
a view; an ordinary table of the right name passed.

**A known layout, compared whole.** The ways a write can run code are
catalog rows — a trigger, a rule, a row-security policy, a column
default, a CHECK expression, an index expression, a column type with its
own input function — and listing them would be an open set. The recipe
is two `CREATE TABLE` statements whose catalog projection (columns with
type, nullability, identity and default; non-`NOT NULL` constraints;
index definitions) is closed and was measured identical on 18.6 and
16.15, apart from the `contype = 'n'` rows 18 adds, which are left out
because `attnotnull` carries them. Triggers, rules, policies and row
security are listed so any of them is a difference. The facts are
deparsed under the catalog reader's canonical settings
(`catalog::canonical_query`), since a setting the deployment role
carries — `quote_all_identifiers = on`, measured — otherwise renders the
recipe pbps created as someone else's. A `__pbps_state`
missing any of #103's five timeline columns is the recipe too: each is
a plain nullable integer with no default, and the migration adds
whichever are missing.

**And the owner**, because the shape alone is checked once: an owner can
add a trigger after the check passes. Trusted is the deployment account,
a role whose privileges it inherits or that it can `SET ROLE` to, the
database owner, or a superuser — owners whose powers the account already
has. Both membership tests: `pg_has_role(…, 'USAGE')` is false for a
`NOINHERIT` membership, which has `SET` (measured on 18.6 and 16.15). A least-privilege
deployment whose ledger a superuser or the database owner created keeps
working; one whose ledger belongs to some other role is refused by name,
with the owner in the message.

The comparison reads only world-readable catalogs, so `doctor` asks it
of the least-privileged account and reports `ledger.untrusted`, through
a field kept out of the published envelope schema (like `env_name`) so
that no schema version moves for a finding. `prune` and `unlock` do not
take the `ensure_tables` path and are #396's; SQL Server's ledger is not
this decision's.

<a id="dec-834-1"></a>

**DEC-834.1. Every login role that can add a trigger to the ledger must be
able to become the deployment account (#834, #838; amends DEC-313.1's owner
rule).** DEC-313.1 compared the ledger's shape once and trusted its owner when
the deployment account could inherit it or `SET ROLE` to it. That left two
ways to add a trigger after the check that the owner test never saw: a
`TRIGGER` grant — to one role, to `PUBLIC`, or through default privileges —
which PostgreSQL lets a grantee use without owning the table; and a group
owner whose *other* members inherit it. Either trigger then runs with the
deployment account's privileges on its next `lock` or `record`.

The rule is now the one `crates/pbps-pg/src/data_triggers.rs` applies to a
reference-data table's triggers: find every login role that can act — by
inheritance or `SET ROLE` — as a role owning a ledger table or holding
`TRIGGER` on it, and require each to be able to `SET ROLE` to the deployment
account or to a superuser role, or to be the database owner. Such a role can
add nothing that runs a privilege it lacked. The superuser path is asked on its
own because `pg_has_role` does not follow it: measured, a login role granted
`postgres` has `SET` on `postgres` and not on the deployment account, yet
`SET ROLE postgres; SET ROLE <deployer>` reaches it. And the role acted as must
also have `USAGE` on the ledger's schema: without it `CREATE TRIGGER` is
refused at the schema (`permission denied for schema public`, measured), so a
`TRIGGER` grant it cannot use is no reason to refuse a ledger. `NOLOGIN` roles are not actors
themselves: they act only through their members, each of whom is asked in its
own right, so a group role can still own the ledger while the deployment
account is its only login member. A login role that owns the ledger and cannot
become the deployment account is now refused even when the deployment account
can become it — it could log in and add the trigger itself.

Measured on 18.6 and 16.15 with separate deployment, attacker and group roles:
a `TRIGGER` grant to the attacker, a `TRIGGER` grant to `PUBLIC`, and a group
owner the attacker also inherits are each refused naming the attacker, while
a `TRIGGER` grant to the deployment account and a `NOLOGIN` group owner the
deployment account holds `NOINHERIT` are accepted.

<a id="dec-862-1"></a>

**DEC-862.1. A role with a live session is an actor even when it can no longer
log in (#862; amends DEC-834.1).** DEC-834.1 counted only `rolcanlogin` roles
as editors of the ledger, on the reasoning that a `NOLOGIN` role acts only
through its members. `ALTER ROLE … NOLOGIN` does not end a session the role
already has, so a role that connected while it could log in and was then made
`NOLOGIN` keeps its ownership or `TRIGGER` grant in that session, and can add a
trigger between the check and the write. An editor is now a login role or any
role with a backend in `pg_stat_activity` connected to this database
(`usesysid` and `datid`, which every role may read — a session cannot change
database, so one elsewhere cannot use a grant here) and, where the
deployment account can see it, a client backend rather than a background
worker. Another role's `backend_type` is visible only with `pg_read_all_stats`
or as a superuser (measured; it is NULL otherwise), so an unclassifiable row
is counted — a least-privileged account refuses while such a worker runs, and
`pg_read_all_stats` lets it tell the two apart; a `NOLOGIN` group with no session of its own is still not one, so a
group owner whose only login member is the deployment account keeps working.

<a id="dec-868-1"></a>

**DEC-868.1. A confidential record keeps its whole snapshot and checksum in a
separate protected ledger table, and leaves nothing a pre-feature reader can
project (#594, #868; chooses the layout ADR-0016 decision 5 left open).** A
pre-feature binary cannot be changed, and it reads two places: its timeline
query projects `__pbps_state.plan_checksum` for every row, whatever the state
version, and `latest`, `history`, its fallback query and `status` parse
`state_json`, which holds a second copy — `status` prints it literally in a
staged-resume command. Any design that leaves a confidential verifier in either
place is exposed by readers that will never learn a new flag, so versioned
metadata, redaction in new clients, a warning or an operator assertion cannot
be the boundary.

*Chosen: protected storage.* For a confidential record the ordinary row in
`__pbps_state` keeps its identity and non-verifier columns (`id`, `applied_at`,
`kind`, `git_sha`, `operator`), writes `plan_checksum` and `reason` as NULL —
`reason` because a failure's tool-written reason can quote a checksum (a staged
resume against a different plan names the checkpoint's), so it is not a
non-verifier column — and
stores a stub `state_json` carrying only a new state-format version and the
confidential classification, with the matching `state_version`. A pre-feature
reader therefore sees a NULL checksum and an unsupported version; its `status`
and `plan` refuse on the version instead of printing anything. The complete
snapshot — not a list of fields thought to be sensitive, which would be the
open set this project refuses to maintain — the plan checksum and the reason are written
to `__pbps_state_confidential` (`public` on PostgreSQL, `dbo` on SQL Server),
keyed by the ordinary row's `id`, in the same transaction as the ordinary row,
so neither exists without the other. Ordinary records, and every existing row,
are unchanged: no migration touches them, and the protected table is created
only when the first confidential record is written.

*Rejected: qualifying readers of the existing ledger.* Proving that everyone
who can read `__pbps_state` is authorized, and refusing otherwise, needs no
new storage but requires revoking the human read-only access SPEC §8.1
recommends before any confidential plan, and a later `SELECT` grant would
expose every historical confidential checksum at once. Protected storage keeps
the ordinary ledger readable by the people it serves today.

*Who may read it.* Before a confidential plan is published and again before
apply DDL, pbps qualifies the protected table from the catalog, read-only: every
role that can read it — through ownership, `SELECT`, a predefined role such as
`pg_read_all_data` or `db_datareader`, a server-level or ownership-chain path,
or membership — must be able to become the deployment account or be a
superuser/`sysadmin`, and the ledger-integrity rules (DEC-313.1, DEC-834.1)
apply to it as to the other two tables. There is no database-owner exemption
(#863). Statement and audit capture that would record the INSERT's values —
PostgreSQL `log_statement` of `mod` or `all`, statement-duration logging,
`auto_explain` with parameters, `pgaudit` object or write auditing; a SQL Server
audit specification or extended-event session covering the table, its schema or
the database — counts as a reader it cannot qualify. Anything it cannot
establish, including a catalog it may not read, refuses the confidential
operation; pbps never changes a grant or an audit policy to pass. A principal
that can take a backup is a reader too, whatever its `SELECT`: on SQL Server
`db_backupoperator`, `db_owner` and holders of `BACKUP DATABASE`/`BACKUP LOG`;
on PostgreSQL roles with `REPLICATION` (a base backup copies every table) and
`pg_read_all_data`/`pg_read_server_files`/`pg_execute_server_program`. Each must
meet the same rule or the operation refuses. Once a qualified principal has
taken a backup, the copy's storage is its holder's responsibility, as a
downloaded plan file is (ADR-0016 decision 5); pbps cannot observe it.

*Before the table exists.* It is created on first use, so the first
confidential plan meets no table to qualify — and neither refusing that plan
nor reading absence as safety is acceptable. An absent table is qualified as
the table the deployment account would create: its owner is that account; its
initial readers are the grants the ledger schema's default privileges give a
new table of that owner (`pg_default_acl` on PostgreSQL; on SQL Server the
`SELECT`/`CONTROL` held on schema `dbo` or the database, which a new table
inherits); plus the global readers (superusers/`sysadmin`, `pg_read_all_data`,
`db_datareader`, `db_owner`) and the statement and audit capture above. Each
must meet the same rule as for an existing table. The apply then creates the
table inside its own transaction and qualifies the *actual* table again before
the first confidential write, so a default privilege or grant added between the
two checks still refuses.

*Readers.* New binaries join the two tables. A confidential row whose protected
half is unreadable is reported `Denied`, never with a checksum; one whose
protected half is missing is `Malformed` (absent, empty and unreadable stay
three answers). `state list` and `status` show the classification and a
non-executable placeholder, never the checksum, outside a qualified protected
output. The UI calls the CLI of the same build, so a field added to its row
contract ships with the parser that accepts it.

*Failure and recovery.* Both halves of a record are one transaction on both
engines, so an interrupted write leaves neither. The gate is re-established
before a staged apply's first statement and on `--resume`; losing the
qualification mid-run refuses the next statement, and the failed attempt is
recorded as a confidential record or not at all.

*The test contract for parts 2–4.* Live on both engines: an unqualified
protected table (a stray reader, statement logging on) refuses before output,
DDL or recording; the ordinary row of a confidential record has a NULL checksum
and a stub `state_json`; removing the gate or writing the checksum to the
ordinary row fails a negative control. And a pre-feature binary built from a
pinned commit, run against ledgers holding ordinary and confidential records —
human and JSON `state list`, `status` including a staged checkpoint, the
unsupported-version and fallback paths, and direct `SELECT *` on the ordinary
table — never outputs a confidential checksum, while ordinary records' history
and approval by the same SHA-256 keep working.

*Superseded by [DEC-952.1](#dec-952-1): the fingerprints are keyed, so there is
no verifier to store apart and no protected table.*

<a id="dec-878-1"></a>

**DEC-878.1. Each engine writes a confidential record's two halves in one
statement, and prunes them together behind the protected table's integrity
gate (#878; implements DEC-868.1's storage).** PostgreSQL inserts the ordinary
row in a data-modifying CTE whose `RETURNING id` feeds the protected insert;
SQL Server's ordinary insert carries `OUTPUT INSERTED.id … INTO
dbo.__pbps_state_confidential` beside `OUTPUT INSERTED.id` for the caller. One
statement, so a protected insert that fails — measured with the protected
table's `INSERT` withheld from a least-privileged account — leaves no ordinary
row either. On SQL Server the same clause is a guard: the engine refuses
`OUTPUT … INTO` a table with an enabled trigger (Msg 331), so a trigger added to
the protected table cannot run inside a confidential insert.

No foreign key joins the halves. It would need `REFERENCES` on `__pbps_state`
from a least-privileged deployment account, it adds engine-created triggers on
PostgreSQL, and on SQL Server it would forbid the `OUTPUT … INTO` above; the
one-statement write and the one-statement prune keep the halves together
instead. Prune deletes both halves up to the same id in one statement on
PostgreSQL and in one transaction on SQL Server, after the protected table is
held to its recipe (DEC-313.1's method; on SQL Server a new shape, trigger,
security-predicate and owner check, since DEC-313.1 was PostgreSQL's). The SQL
Server batch takes an exclusive table lock *before* re-checking for a trigger,
so none can be added between the check and the delete. The protected table is
in both engines' ledger-name filters, so the pull never takes it for managed
schema. Reader qualification is #879 and #880.

*Superseded by [DEC-952.1](#dec-952-1); #952 removed the protected table this
entry describes.*

<a id="dec-901-1"></a>

**DEC-901.1. The ledger comparison reads the table's own row, the identity
column's sequence and every relation at a ledger name, and `prune` and `unlock`
ask it before they delete (#901; amends DEC-313.1).** DEC-313.1 compared what
hangs off the two tables and called that projection closed. It was not: an
`UNLOGGED` ledger (truncated after a crash), a typed one (`ALTER TYPE …
CASCADE` changes it without owning it), one in an inheritance tree (reads reach
a table never inspected), a descending identity (`ORDER BY id DESC` stops
meaning newest-first) and a table with no columns (no facts, so skipped as
absent) all kept every column, constraint and index of the recipe and passed.
The facts now include persistence, access method, replica identity, a type the
table was created `OF`, either end of `pg_inherits`, and the identity
sequence's type, start, increment, bounds, cache and cycle. The recipe's
values were measured identical on 18.6 and 16.15, and `pg_sequence` is
world-readable there, so `doctor` can still ask this of the least-privileged
account. A view or another kind at a ledger name is a problem in the words the
write path's refusal already used (#217), so `doctor` names it rather than
reading an absent ledger.

`prune` and `unlock` run the same check before their DELETE, which fires a
trigger and follows a view exactly as `lock`'s INSERT does (#396). Not through
`ensure_tables`: a command that deletes should not create the ledger as a side
effect, and an absent lock table still answers "not held". Every ordinary read
and delete names the tables with `ONLY`, so a read that runs no check, such as
`latest`, cannot take a descendant's row as the newest snapshot (#900).

A subset of #103's timeline columns stays the recipe, but the migration now
asks for all five rather than `state_version` alone, so a ledger lacking one of
the others is migrated before `record` writes to it (#841).

Because the recipe now requires `heap`, both `CREATE TABLE`s say `USING heap`
instead of taking `default_table_access_method`: a deployment account whose
setting names another installed method would otherwise create a ledger that the
check then refuses. `emit.rs` writes the clause on managed tables for the same
reason. Measured on 18.6 with a second method built on the heap handler.

The protected table DEC-878.1 added was held to the same table-level facts and
created `USING heap` too, until #952 removed it (DEC-952.1). `prune`'s one gate now covers all three tables, so a
view at the protected name is refused before the combined delete (#885). A
table inheriting from the protected one is refused rather than deleted around
with `ONLY`, as for the other two.

<a id="dec-912-1"></a>

**DEC-912.1. A login role that can reset the ledger's id sequence must be able
to become the deployment account too (#912; extends DEC-834.1's editor rule).**
DEC-901.1 compares the identity sequence's configuration, not who may use it.
`setval` needs only `UPDATE` on the sequence, and a value set below the newest
id makes the next records sort under older ones, so `latest`, `history` and
`prune` read the wrong row as newest. Every login role, or role with a live
session, that can act as a holder of that `UPDATE` (directly, through
`PUBLIC`, or through a role it can `SET ROLE` to) and cannot become the
deployment account or a superuser is now a problem, named as able to reset the
sequence.

Unlike the trigger rule, this one has no schema `USAGE` clause. `CREATE TRIGGER`
resolves the table by name, so a grantee without `USAGE` cannot use its grant,
but `setval` also takes an oid, which needs no name lookup: measured on 18.6, a
role with `UPDATE` and no `USAGE` on the schema moved the sequence. `USAGE`
alone allows only `nextval`, which moves forward and reorders nothing. `ALTER
SEQUENCE` needs ownership, which follows the table's owner and cannot be
changed apart from it, measured on 18.6 and 16.15, so the trigger rule's owner
test already covers it. A role that rule names is not named twice.

And the role must be able to call `setval`. `UPDATE` alone is not enough once
`EXECUTE` on both of its overloads is revoked from `PUBLIC`: measured on 18.6
and 16.15, each call is then refused, and naming such a role would refuse a
ledger nobody can reorder. One role reached by `SET ROLE` must hold both the
`UPDATE` and the `EXECUTE`, since the call runs with that role's privileges.

<a id="dec-894-1"></a>

**DEC-894.1. On SQL Server, a project table that the database collation folds
onto an ordinary ledger name is refused before the ledger is written, not
adopted (#894; extends DEC-878.1's spelling fact to `__pbps_state` and
`__pbps_lock`).** `OBJECT_ID` compares names under the database collation, so
on a case-insensitive database `dbo.__pbps_state` resolves to a project's own
`dbo.__PBPS_STATE`. The pull and validation reserve only the exact spelling,
and `CREATE_STATE`'s `IF OBJECT_ID(…) IS NULL` then skipped creating the ledger,
so `record`, `lock`, `prune` and `unlock` wrote to the project's table.
`ensure_tables`, `prune` and `unlock` now compare `OBJECT_NAME(OBJECT_ID(…))`
with the exact spelling under `Latin1_General_BIN2`, and refuse by the found
name when they differ. The table is the project's and stays the project's:
reserving every folded spelling instead would take a name from projects on
case-sensitive databases, where `dbo.__PBPS_STATE` is a different table and
`OBJECT_ID` is exact (measured on the pinned server, both collations). Reads
are not gated. A folded table without the ledger's columns fails them loudly,
and the next write refuses by name.

<a id="dec-954-1"></a>

**DEC-954.1. SQL Server ledger names are compared with a `|` appended to both
sides, because `=`, `<>` and `IN` pad trailing spaces even under
`Latin1_General_BIN2` (#954; completes DEC-878.1 and DEC-894.1).** Measured on
the pinned server: with a project table `dbo.[__pbps_state ]`, `OBJECT_ID(N'dbo.__pbps_state')`
resolves to it on every collation, `OBJECT_NAME(…) COLLATE Latin1_General_BIN2
<> N'__pbps_state'` is false, and an `INSERT INTO dbo.__pbps_state` lands in
it. So the spelling checks passed a padded name, and the pull's filter hid the
project's table as the ledger. Appending the same character to both sides puts
any trailing space before it, where padding cannot reach. It is one idiom for
all three comparisons. A `DATALENGTH` test would work too, but only paired
with its own name: an `IN` list of lengths lets `'__pbps_lock '` through,
because it is exactly as long as `'__pbps_state'`.

The object-name side is tested `IS NOT NULL` before the sentinel is appended.
Under `CONCAT_NULL_YIELDS_NULL OFF`, which a server's `user options` can make
the default and pbps does not pin, `NULL + N'|'` is `N'|'` (measured), so an
absent table would otherwise read as misspelt and refuse creating the ledger.
The pull's filter compares `sys` names, which are never NULL.

<a id="dec-372-1"></a>

**DEC-372.1. A ledger row holding a value `record` never writes is refused for
that row, and a negative `state_version` is such a value (#372).** `timeline()`
reads the projected columns of every row in one batch. A negative or NULL
`tables_count`/`modules_count`, a negative staged count, or a negative
`state_version` was a `BadRow` that `?`d out of `projected_row`. So one
hand-edited or foreign-written row failed the whole call, and every readable
row went with it. That is the "thrown, not carried" shape DECISIONS 218 rules
out, and #353 had already fixed it twice on this same function, for an
unsupported version and for a half-populated staged pair. Each of these is now
the row's `Unreadable::Malformed`, naming the column and the value. The NULL
count is included, although it failed one step earlier, at the "must not be
NULL" read, because it is another route into the same kind of bad row. Only a
failure to read a column at all is still thrown.

`state_version` was the open question. It is this crate's own monotonic
marker, so a negative one could be read as evidence that the whole read is
wrong: schema drift, or another writer sharing the table name. It is carried
like the counts anyway. A query that had drifted from the table would show on
every row and every column, not on one value in one row, and the version
check that already refuses an unsupported version carries that row too.
Making the one malformed marker a hard failure would recreate, for this
column, the batch-wide outage the rest of this entry removes.

<a id="dec-952-1"></a>

**DEC-952.1. External-input fingerprints are HMAC-SHA256 under a per-environment
key that pbps generates and checks but does not keep; nothing a resolver plan
records is a verifier to hide, so the ledger stores it as it stores any plan
(#952; supersedes DEC-868.1 and DEC-878.1, amends ADR-0016 decision 5).**
The secret was never the fingerprint. A bare SHA-256 over a definition that
carries a literal is a guessing oracle for anyone who can compute it, and
DEC-868.1 answered by hiding the oracle. It kept the checksum in a protected
table and proved from the catalog that every reader of that table could become
the deployment account. That proof re-derives an engine's effective
permissions, the second implementation DECISIONS 521 warns against. On SQL
Server it did not converge: #880 / PR #928 ran 16 review rounds, and nearly
every round found another real authorization path. It also could not have held.
Measured on SQL Server 2025 (#918), the INSERT's parameters reach the plan cache
and Query Store, readable by principals the proof never asks about.

Keying removes the oracle rather than hiding it. Without the key, a fingerprint
tests no guess, and neither does a checksum or snapshot derived from it.

*The key.*
- It is per environment, never shared, never defaulted. A key one environment's
  fingerprints were made under is no evidence for another's.
- An environment names its source exactly as it names its connection string:
  `fingerprint_key_env` (a variable) or `fingerprint_key_file` (a file readable
  by its owner only). Both is refused when `pbps.yml` is read.
- It is base64 of at least 32 bytes, HMAC-SHA256's own output length.
  `pbps key generate` prints one, or writes a new mode-0600 file and never
  overwrites an existing one, because a replaced key voids every pending plan.
- `pbps doctor` reports its key identifier: the first 8 bytes of an HMAC under
  the key over a fixed label. The identifier tells keys apart and reveals
  nothing usable.

*Why pbps does not keep it.* The only places pbps could keep a key are the
target database, whose readers are the problem, or the local disk, which the
machine that plans and the machine that applies do not share. A service would
be the approval or key-management system ADR-0016 refuses, and SPEC 14.3's
air-gapped rule refuses it too. The operator's secret tooling already delivers
the connection string. It delivers the key the same way.

*Two kinds of key.* A fingerprint that outlives the process, sealed into a plan
and compared at apply, is made under the environment's key. The sealed plan
records the key identifier, and `apply` under another key refuses with a replan
remedy (#614, #616, which seal and recheck and did not exist when this was
decided). A fingerprint compared and dropped within one process needs no shared
key: the capture comparison and the authorization-context digests use a random
per-process key (`FingerprintKey::process`). No bare SHA-256 over a private input
exists anywhere, not even in memory.

*The primitive.* It is RustCrypto's `hmac` 0.13 over `sha2` 0.11, the pair
`postgres-protocol` already resolves (DECISIONS 434), rather than a new `hmac`
0.12 for the workspace's `sha2` 0.10. RFC 4231 test case 2 pins it. Every part
is length-framed, so moving a boundary between rule, component and input changes
the message.

*What stays.* The source of retained definitions is still private while it is
reconstructed (#617). Keying the verifiers does not declassify the definitions
they cover. A leaked key restores the guessing exposure to its holders, who
already hold the credentials that read the definitions.
