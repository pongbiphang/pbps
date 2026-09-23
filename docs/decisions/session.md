# Session settings and the write path

The transaction framing, `search_path`, and the session settings every statement
runs under. Part of the [decision record](../DECISIONS.md), which says how to
add an entry here.

<a id="decision-194"></a>

194. **The transaction framing's text is the dialect's; `pbps-db` runs it.**
    `Conn::begin` held `SET XACT_ABORT ON; BEGIN TRANSACTION;` and `rollback`
    held `IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;` — T-SQL in the crate
    CLAUDE.md documents as holding none, kept there for reasons the code
    explained at length. The reasons stay, beside the text, in `pbps-mssql`;
    `Dialect::transaction_framing` returns the three statements and
    `Conn::begin`, `commit` and `rollback` take them. The method has no
    default: a default would have been one engine's answer under a neutral
    name, the shape ADR-0011 names three times, and PostgreSQL's `begin` is a
    bare `BEGIN;` (measured — any error already dooms its transaction). Ruled
    out: a trait for the framing, since nothing varies between engines but the
    text; and `begin(&str)`, which would make it a second `execute` and leave
    the framing owned by nobody. `pbps-db` depends on `pbps-dialect` for the
    type alone (ADR-0014 §2).

<a id="decision-259"></a>

259. **The write `search_path` is set per statement, in the statement's own
    batch, and given back in the same one.** ADR-0013 §3 decides the value —
    the object's own schema first, then the project's configured extras — and
    leaves how it is carried to the emitter. Three spellings were available and
    two of them fail somewhere.

    `SET LOCAL` is the precise one inside a transaction and a **no-op with a
    warning outside one**, and `bootstrap --sql` renders a script a human runs
    through `psql`, statement by statement, in no transaction at all. A scope
    that quietly does nothing on the disaster-recovery path is the failure the
    scope exists to prevent. A session-level `SET` in a *preceding* statement
    survives that, and does not survive a staged apply resuming on a new
    connection — the path would be missing for exactly the statement that
    needed it.

    So the `SET`, the statement and the `RESET` are one batch. Measured, that
    works: `SET LOCAL search_path = bt, btx; CREATE TABLE bt.t (… CHECK (f(id) >
    0))` binds `f` through the new path, because a simple query is *analysed*
    one statement at a time. Inside a transaction the `RESET` is rolled back
    with everything else; outside one it hands the connection back as the
    operator's environment left it.

<a id="decision-260"></a>

260. **The two settings that decide how a definition *parses* are pinned by the
    transaction framing, not by the statement.** They cannot be pinned by the
    statement. Measured on 18.6, a multi-statement simple query is **lexed as a
    whole before any of it runs**:

    ```text
    one batch:  SET LOCAL standard_conforming_strings = off; SELECT length('it\'s here');
                -> syntax error — the batch was lexed under the old value
    two:        SET standard_conforming_strings = off;  then  the same SELECT
                -> one literal, length 9
    ```

    so a `SET` in front of the statement it is meant to protect protects
    nothing. The pin has to be established on an earlier batch, and `begin` is
    the earlier batch a transactional apply runs — the same place SQL Server's
    `SET XACT_ABORT ON` lives (DECISIONS 194), for the same structural reason.

    **A staged apply opens no transaction, so `begin` does not cover it**, and
    an earlier version of this entry said "every connection" and was wrong. The
    pin belongs on the connection there, and it cannot be moved into the
    emitter's own statements: a staged run checkpoints after each one, and a
    `--resume` on a fresh connection starts at the next unexecuted statement,
    which is exactly the one whose pin was two statements back. No staged apply
    can reach this dialect yet — `main.rs`'s `dialect()` refuses it and
    `apply_staged_under_lock` calls `pbps_mssql::state` by name — so the
    connection-level pin lands with the PostgreSQL ledger and staged path
    (issue #83), which is the step that builds the connection it belongs on.

    Both are the ones ADR-0013 §3 names. `standard_conforming_strings = on` is
    what makes ADR-0011's scanner rule true: measured under `off`, `CHECK (label
    <> 'it\'s  here')` is **accepted** as one literal while the normalizer
    closes it at the escaped quote, so a whitespace edit inside that literal
    compares equal and is never planned; under `on` the same text is a syntax
    error. `check_function_bodies = on` is what makes ADR-0009's opaque-caller
    exemption true: measured, a SQL body naming a relation that does not exist
    is created silently under `off` and fails the first time it is called.

    The third exception ADR-0013 names, the write `search_path`, is per
    statement and does work in the statement's own batch (259) — name
    resolution happens per statement where lexing does not.

<a id="decision-264"></a>

264. **The `DO` block's dollar-quote tag is chosen against the body it wraps.**
    Dropping a primary key the declaration did not name means asking the catalog
    for its name, which means dynamic SQL, which on this engine means a `DO`
    block. PostgreSQL's lexer looks for a dollar-quote's closing tag
    **literally**, without regard for quotes inside it — so a table named
    `x$pbps$y`, which is a legal identifier `pull` would adopt, ends the block
    where its name appears and hands the rest of it to the server as top-level
    SQL. The tag is therefore the first of `$pbps$`, `$pbps1$`, … that the body
    does not contain, which makes the failure unrepresentable rather than
    checked for.

<a id="decision-265"></a>

265. **The write path's extras live on the dialect value, and the `pbps.yml` key
    waits for a reader.** `Dialect::emit` takes a change and a strategy, and a
    strategy says how to get there and never where (ADR-0003), so the extras
    have to be state on `Postgres`. They are not yet configuration: the CLI
    refuses this dialect outright (`main.rs`'s `dialect()`), so a key in
    `pbps.yml` would be one nothing reads — which is worse than none, because a
    user who sets it would have every reason to believe it took effect. The key
    lands with the step that can read it.

<a id="decision-267"></a>

267. **Every setting that changes what a declared expression means is pinned in
    the transaction framing, not around each statement.** The three verbatim
    expressions the model carries — a column default, a check expression and an
    index filter (ADR-0013 §3) — are text the engine reads through an input
    function or a parser rule, and five settings decide what that text means.
    Two more decide what a *conversion* writes back, below.
    Measured on 18.6, the identical declaration created by two sessions:

    ```text
    CHECK (d >= '01/02/2026')       MDY -> '2026-01-02'   DMY -> '2026-02-01'
    CHECK (at >= '2026-01-02 00:00')
      on a timestamptz              UTC -> 00:00:00+00    New_York -> 05:00:00+00
    CHECK (i >= '-1 2:00:00')       postgres -> -1 days +02:00:00
                                    sql_standard -> -1 days -02:00:00
    CHECK (at >= '2026-01-15 12:00:00 CST')
                                    Default -> 18:00:00+00   Australia -> 02:30:00+00
    CHECK (x = NULL)                off -> (x = NULL::integer)   on -> (x IS NULL)
    ```

    A different day, a different instant, an interval with the opposite sign, a
    time fifteen and a half hours out, and a predicate that stopped being the
    one that was written. No error, no warning, and nothing afterwards can say
    which session decided it.

    **The list is a rule, not a set of temporal traps**, and the last two are
    what say so. `timezone_abbreviations` is a dictionary `TimeZone` does not
    cover, so pinning the zone does not pin the abbreviation; and
    `transform_null_equals` is not an input function at all but a *parser*
    rewrite, which changes the predicate rather than a value inside it. The
    rule is: **a setting that changes what the declared text means is pinned**.
    Both were found by review after the first three shipped, and a list closed
    against its rule would have taken a third round to find the fourth.

    **`bytea_output` and `extra_float_digits` are the seventh and eighth, and
    were left out on a measurement that was true of what it measured.** A
    declared expression stores the same constraint under `hex`/`1` and under
    `escape`/`0`, because nothing in `CHECK (b >= '\x0102')` runs a value
    through an *output* function. An `ALTER COLUMN … TYPE text` does:

    ```text
    bytea -> text             hex -> \x0102              escape -> \001\002
    double precision -> text  1 -> 0.12345678901234568   -3 -> 0.123456789012
    ```

    Same stored bytes, same approved statement, two different strings left in
    the table. The rule reaches this and the exclusion did not, because
    "output-only" was a statement about where the setting is *read* rather than
    about whether a plan's result depends on it. Pinning is the complete answer
    where refusing the conversion would be an enumeration: every cast to text
    goes through an output function, and the list of which ones consult a
    setting is exactly the list the pin makes irrelevant. The values are the
    read scope's, so what a plan writes is what the next `pull` reads back.

    `lc_monetary` is in the class and is deliberately out, for the reason
    `catalog.rs` gives on the read side: `SET` fails outright on a locale the
    server does not have, so pinning it would turn a database that deploys into
    one that cannot. Its reach is a `money` literal, and this dialect's type
    catalogue refuses `money`.

    They go in `begin` and not in the per-statement scope because they are
    *constants*: unlike `search_path`, which is the object's own schema and
    therefore varies per statement (DECISIONS 259), one value serves the whole
    plan. Nine `SET`s and nine `RESET`s around every line of `plan.sql` would
    bury the SQL a reviewer has to read (SPEC §14.1) to say the same thing
    once.

    `bytea_output` and `extra_float_digits` are named by ADR-0013 §3 and are
    deliberately **not** here. Measured, both are output-only — identical stored
    constraints under `hex`/`1` and under `escape`/`0` — so they belong to the
    read scope, which already sets them, and pinning them on the write side
    would suggest they decide something they do not.

    This is also the answer to a check constraint that a `Column::default`
    guard cannot reach (DECISIONS 261): a default is refused when its literal
    is bare because the column's type is known there, while a check names
    columns and carries no type, and no offline rule can tell `'01/02/2026'`
    inside one from a string that merely looks like a date. Pinning the reader
    is what makes the text mean one thing.

<a id="decision-275"></a>

275. **`$user` is refused as a schema name and as a write-path extra.**
    ADR-0013 §3 scopes every write statement with `SET search_path = <the
    object's own schema>, <the project's extras>`, and one name cannot travel
    in that list. **Measured on 18.6**, with a schema literally called `$user`
    holding a function, under role `postgres` with a `postgres` schema beside
    it:

    ```text
    SET search_path = "$user";
    SELECT current_setting('search_path'), which();
        -> "$user" | the role schema
    ```

    The quotes survive into the setting and change nothing: the engine
    substitutes that entry for the current role's own schema. So a table
    declared in a schema of that name would be created — its statements name it
    in full — and then every unqualified name inside a check, a filter or a
    default would resolve through whatever the deploying role owns, binding a
    different object or none. That is the property
    `an_unqualified_name_in_a_declared_expression_binds_through_the_write_path`
    exists to hold, silently inverted.

    Refused rather than worked around, because there is no spelling that makes
    the entry literal — quoting is the obvious attempt and it is the one
    measured above. Refused from **both** ends, since the path has two sources:
    the table's own schema (`validate_table`) and the configured extras
    (`emit::write_path`), and an extra is not seen by any table's validation.

    The comparison is exact and case-sensitive because the engine's is: `$USER`
    and `$users` are ordinary schema names, and refusing them would refuse a
    declaration that works.

<a id="decision-276"></a>

276. **`pg_catalog` is left out of the write path, so it is searched first.**
    ADR-0013 §3 says the write scope puts the object's own schema first, and
    **measured**, that is true only among the schemas the path names:

    ```text
    CREATE FUNCTION shad.lower(text) RETURNS text AS 'the project function';
    SET search_path = shad;               SELECT lower('X');  -> x
    SET search_path = shad, pg_catalog;   SELECT lower('X');  -> the project function
    ```

    PostgreSQL searches `pg_catalog` ahead of every listed schema whenever the
    path does not name it. So a project function or operator with the same
    signature as a built-in never wins inside a declared expression, and the
    ordering the ADR promised is not the whole ordering.

    **The obvious repair makes a worse hazard, and the emitter cannot defend
    against that one.** Naming `pg_catalog` last would let a project *type*
    shadow a built-in: a `CREATE DOMAIN app.text` in the table's own schema
    would change what every `c text` column in that schema means, silently, and
    the pull would then read the column back as a type the closed catalogue
    does not hold and report it unsupported. The emitter cannot write around it
    — `character varying`, `double precision` and `timestamp with time zone`
    have no schema-qualified spelling, so there is no `pg_catalog.` prefix to
    put on the names that matter. The type catalogue is a closed list of this
    engine's own names (ADR-0012 §1) and `text` has to keep meaning `text`.

    The hazard that is left has a remedy the user holds: an expression that
    means the project's `lower` can say `app.lower`. The one the repair would
    create has none. So the path stays as it is and the claim is corrected
    instead — in this file, in `emit.rs`'s module docs and in ADR-0013 §3,
    which all said "first" without saying first *among what*.

<a id="decision-277"></a>

277. **`pg_catalog` is refused as a write-path extra, not dropped from the
    path.** 276 records why the emitter leaves it out; a caller may still put
    it in through `Postgres::with_write_path_extras`, and then the path names
    it and the engine stops searching it first. The same measurement runs the
    other way:

    ```text
    SET search_path = "shad";                CHECK (lower(c) = c) -> pg_catalog.lower
    SET search_path = "shad", "pg_catalog";  CHECK (lower(c) = c) -> shad.lower
    ```

    Both statements are accepted, neither says anything, and the constraint
    stored by the second calls a different function from the one the
    declaration reads as.

    Refused rather than silently dropped, for the reason an unquotable extra
    is refused where it is used: a path one entry short — or one entry
    different — binds a name somewhere the caller did not ask for and says
    nothing. Refusing is also the only answer that stays true if the
    entry ever *does* mean what it says: dropping it would be right today and
    wrong the day the reason changed. It joins `$user` (275) as the second
    entry a path cannot hold, and for the mirror reason: `$user` is a name the
    engine reads as something else, `pg_catalog` is a name the engine reads
    differently for having been written at all.

<a id="decision-290"></a>

290. **A staged apply pins its session, through `Dialect::session_pins`.**
    `Postgres::transaction_framing`'s `begin` carries nine settings, because
    each of them decides what a declared expression *means* (DECISIONS 254 and
    the method's own comment). A staged apply opens no transaction, so `begin`
    is never sent — and `--resume` is worse, because it starts on a fresh
    connection partway through the plan. The same text is now available without
    the `BEGIN`, and the staged runner establishes it on the connection before
    its first statement.

    The trait's default is `None`, which is the right answer for SQL Server: its
    framing carries `SET XACT_ABORT ON`, which governs a transaction and means
    nothing outside one. One macro produces the text for both call sites here,
    so the transactional and staged paths cannot pin different things — the
    failure this replaces, where the staged path pinned nothing at all.

<a id="decision-312"></a>

312. **The transaction probe compares against a value it invented, not against
    a constant.** Both sides of it — the pull refusing a caller's transaction,
    the rebuild requiring one — are `set_config(…, is_local => true)` in one
    statement and `current_setting` in the next: inside a transaction the
    setting survives to be read, outside one the implicit transaction ends and
    it does not.

    Against the constant `'yes'` that read had a third outcome nobody asked
    for. A session carrying `SET pbps.in_a_transaction = 'yes'` answers `'yes'`
    on an autocommit connection, and the two sides fail in opposite directions:
    the rebuild believes its reads are serialized when `LOCK TABLE` has already
    been released at the end of its own statement, and the pull refuses a
    connection that has no transaction at all. The first is a guard still in
    the code and no longer guarding; the second is a valid plan refused.

    A value invented per call cannot be sitting in the session, so the read is
    equal only if *this* call's `set_config` survived — which is the question
    being asked. A type that cannot hold the bad value beats a branch that
    checks for it, and here the value is the type.

<a id="decision-458"></a>

458. **SQL scripts carry the same session pins as deployment (issue #174).**
     PR #205 supplied `Dialect::session_pins` for staged runs and a fresh
     connection resuming a checkpoint; #285 moved the pins ahead of probes.
     The remaining script path passed only a batch separator to the renderer,
     so offline/connected `plan --sql` and `bootstrap --sql` omitted the nine
     PostgreSQL settings. `render_script` now takes the dialect and obtains
     both its separator and its pins. The setup is an initial script batch,
     outside the saved changes, checksums and checkpoint statement counts.
     A dialect with no pins retains its output; an empty script stays empty.

     The client must execute that batch before parsing the DDL. Measured on
     PostgreSQL 18.6 with psql reading standard input, an initial
     `standard_conforming_strings=off` and `DateStyle=German,DMY` interpreted
     `01/02/2026` as February 1 and `a\n` as a string containing a newline.
     The shared pins make them January 2 and a literal backslash followed by
     `n`; `it\'s  here`, previously accepted, fails with `42601`. Concatenating
     setup and DDL into one simple query cannot establish the lexical setting
     in time (decision 260), so the output names the ordered-execution rule.
     `psql -f` and standard input satisfy it; `psql -c` with the whole file
     does not. This does not add approval or ledger recording to SQL previews.

     The CLI regression feeds all three actual output files to psql under
     hostile values of all nine settings and asks the server for the stored
     constraints. Its invalid-quote control must report `42601`. The local
     script and CI use the test container's psql; `PBPS_TEST_PSQL` selects a
     local client for a separately hosted test server. Another live case uses
     database-local hostile defaults so it leaves the shared login untouched:
     a single CreateTable change checkpoints before its CHECK statements,
     then a new CLI process resumes it. Accepted and refused rows distinguish
     the pinned date and string meanings on both staged and resumed paths.

     That live case exposed a valid-plan refusal in the checkpoint guard:
     after CREATE TABLE's first statement, the table exists in the previous
     read, and a later CHECK from the same CreateTable payload was treated as
     an unplanned addition. The payload's pending unique/foreign-key/check/
     index names now permit only their absent-to-present transition at a
     statement checkpoint. An already recorded part still compares by its
     complete read-back definition, removal is still movement, and a name
     outside the payload is still refused. The exemption does not apply to
     the closing read, where no statement ran. Negative unit cases pin those
     boundaries for all four kinds. This is the valid-plan-refusal case of
     the review rule, needed to exercise the issue's staged/resume path.

     A pending part's first observed definition must also answer to the
     CREATE payload. Review found that a ddl_command_end trigger could
     replace a newly added UNIQUE/index before its checkpoint, after which
     comparing that checkpoint against itself let the run close. The created
     table's existing structure validation now runs at every read, including
     resumed and closing reads, even when the previous checkpoint already
     holds the table. Foreign keys carried in the payload receive the same
     structural comparison as separately added foreign keys. Checks and
     filters still follow SPEC 7.6's expression-text exclusion. Live trigger
     regressions replace UNIQUE and index columns, require a retained staged
     failure and a refused resume, and keep successful clean deployments as
     controls. Restoring the reviewed guard makes the new test accept the
     replaced UNIQUE and fail its refusal assertion.

<a id="decision-464"></a>

464. **The PostgreSQL write path names `pg_temp` once, after every project
     schema (issue #190).** Measured on PostgreSQL 18.6, an omitted temporary
     schema is searched before even the implicit `pg_catalog` for relation and
     type names. A session holding temporary `t` and domain `d` therefore binds
     a declared default `'t'::regclass::oid::bigint + 1::d` to those objects even
     when the explicit path names the project's schema containing its own
     `t` and `d`. Naming `pg_temp` last makes both bindings use the project.
     Deployment connections create no temporary objects themselves, but an
     operator running a rendered script can already have them in that session.

     The common statement scope appends the alias after the object's schema
     and the configured extras. An exact `pg_temp` already among those entries
     moves to the final position rather than being refused or left earlier:
     appending a duplicate would leave the earlier occurrence ahead of later
     project schemas. Ordinary identifiers retain their spelling and order.
     `pg_catalog` remains implicit and first under decisions 276/277; the
     `$user` refusal remains unchanged. Canonical read paths and the existing
     unscoped concurrent-index boundary are unchanged. No model or saved-plan
     fields are added, and the configured extras remain recorded as supplied.

     The live regression inspects both stored default dependencies rather than
     equal result values, tests the object's own schema and an extra schema,
     and includes an early/repeated `pg_temp` configuration. The same emitted
     statement with the final alias removed binds both temporary objects,
     pinning the negative case. Unit cases keep ordinary names, quoting and
     extra ordering intact.

<a id="decision-537"></a>

537. **A pre-flight probe runs in a read-only transaction of its own on
     PostgreSQL.** (#274.) A probe interpolates the declared expression into a
     count over the live rows, and PostgreSQL accepts a volatile function in a
     `CHECK`: measured on 18.6, `CHECK (nextval('s') > 0)` is accepted, the
     probe advances the sequence once per row, and the advance survives the
     rollback — a write between approval and the plan's first statement that
     no reviewer saw and no abort undoes. Inside `BEGIN READ ONLY` the engine
     refuses `nextval` itself and an ordinary probe is untouched.

     That makes the side effect unrepresentable rather than detected. The
     alternatives each judge the expression: a volatility check needs the
     catalog's answer for every function it names, including ones the plan
     creates, and a list of known-writing functions is the open set this
     project keeps refusing to maintain. A refused probe is reported unchecked
     by name, as any probe that cannot run already is, and the engine enforces
     the constraint inside the apply's transaction.

     One transaction per probe, opened by `begin` and always closed by
     `rollback`, so a refused probe cannot abort the next. Opening or closing
     it failing stops the apply instead of counting the probe unchecked: a
     transaction left open would take the plan's own `BEGIN` inside it, which
     PostgreSQL only warns about. The session pins stay plain `SET`s on the
     connection (415), so the probe still runs under them.

     `Dialect::probe_framing` is required, not defaulted, for the reason
     `transaction_framing` is. SQL Server answers `None`: a T-SQL function
     cannot modify data, so a declared `CHECK` has nothing to set off.
