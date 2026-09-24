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
`SET ROLE postgres; SET ROLE <deployer>` reaches it. `NOLOGIN` roles are not actors
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
