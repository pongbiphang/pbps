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

*Amended by [DEC-1916.1](#dec-1916-1): `TimeZone` is pinned to the target
database's own zone, which the plan records, not to UTC. The rule is
unchanged — the zone is still pinned — only whose zone it is.*

*Extended by [DEC-1634.1](types-and-probes.md#dec-1634-1): `xmloption =
content` joins the list, for the same reason — it decides what an `xml`
literal is read as — in the write pins and in the canonical read scope.*

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

<a id="dec-1564-1"></a>

**DEC-1564.1. pbps's own PostgreSQL SQL runs on an empty `search_path`: every
connection empties it, every scope ends on it, and pbps's statements inside a
scope leave it (#1564).** An operator whose argument types match exactly is
chosen over a built-in that needs a cast or takes a polymorphic argument, even
with `pg_catalog` searched first. Measured on 16 and 18, with a raising
operator created in a schema on the path: `oid = 'x'::regclass`,
`oid = pg_catalog.to_regclass(...)`, `conparentid = 0`, `'x' || relname`,
`relname || 'x'` and `int2[] = int2[]` all call it, and 276 protects only an
identical signature. pbps's SQL runs as the deployer, so anyone who may
create in such a schema could run code with the deployer's rights, inside the
apply's transaction where its writes commit with the plan; DEC-319.1's pins
catch a routine changed after planning, not one that was already there.

The obvious fix is the issue's own: spell every comparison with an exact
built-in signature (`::pg_catalog.oid`, `pg_catalog.format`) and lint for the
two shapes found. It was measured to be one instance of a class with at least
four more members (an `oid` against an integer literal, `text ||` any
non-text, array `=`, `aclitem[] ||`), spread over about a hundred sites, and
a lint over source text cannot know a column's type. So the path is removed
instead of the operators: on an empty path only `pg_catalog` is searched for
operators and functions, and nothing a user created is reachable whatever the
statement spells.

- `pbps-db` empties the path on every PostgreSQL connection, so a read that
  forgets to pin one is safe anyway. A plain `SET` and not a startup option,
  which a pooler may refuse or drop.
- `session_pins` carries it too, so a rendered script run by `psql` starts
  there.
- A scope (`SET search_path = <write path>`) ends with `SET search_path = ''`
  rather than `RESET`, which would return to the role's path.
- A statement that is pbps's alone, with no declared expression to resolve
  (an unnamed key's drop, its index's storage, a replica identity through
  the key, a detach), is emitted outside any scope.
- A `DO` block that also holds a declared expression stays scoped, and its
  pbps-only statements (a row delete's referencing guard) move to the empty
  path and back with a local `set_config`. plpgsql plans each statement under
  the path in force when it first runs.

The probes already ran on the session's path, not the write path (259); an
unqualified name in a declared check that the deployer's path used to reach
in `public` now reports the probe unchecked, as any other unqualified name
already did. The live test `pbps_sql_calls_no_operator_a_schema_user_could_add`
plants raising operators in `public` and in the managed schema and runs a
pull, a connected plan with probes, an apply and `doctor` on 16 and 18.

<a id="dec-1564-2"></a>

**DEC-1564.2. A row write finds its row by `pg_catalog`'s `=` where the key's
type is a built-in one, and by the unqualified `=` otherwise; the plan carries
the key's type to say which (#1564).** An update's and a delete's `WHERE`, and
the check that a deleted row stayed gone, compare the key column with the
declared key on the table's schema's path, which the statement needs for the
declared expressions it carries (DEC-1564.1). Measured on 18: a `varchar` key's
`"k" = E'a'` called an `=(varchar, varchar)` planted in that schema, because
`varchar` has no `=` of its own and the built-in one is reached through
`text`. The row-count check beside it, `bigint <> integer`, has an exact
built-in and called nothing.

Spelling every key comparison `OPERATOR(pg_catalog.=)` closes it for every
key, and was measured to change which row some keys mean: `citext` then
compares case-sensitively, and `ltree` has no such operator at all, so its
plan is refused. So the diff records the key column's declared type on the
row change (`key_type`, the type the `WHERE` meets once any retype has run),
and the emitter names `pg_catalog`'s operator only for a type the dialect's
catalogue knows, where it is the one the unqualified `=` meant: the same
comparison, the same index. A key of another type keeps its own `=`; one of
those (an enum, a domain) is still reached through a polymorphic or base-type
built-in on the schema's path. A plan made before `key_type` existed has none
and keeps the unqualified `=`. SQL Server has no user-defined operators and
ignores the field. The live test above updates and deletes rows by a
`varchar` key under that trap.

<a id="dec-1916-1"></a>

**DEC-1916.1. An offset-less date/time value is read in the target database's
own time zone: read on connect, recorded in the plan, replayed into every pin
and read scope, and required again at apply (#1916).** 267 pinned `TimeZone`
to UTC and 254 read back in UTC, so a hand-written `2026-02-01` on a
`timestamptz` was stored as UTC midnight — 08:00 to everyone looking at a
database in `Asia/Taipei` — and `pull` wrote `+00` for values every other tool
on that database shows as `+08`. A zone in `pbps.yml` (the first design in
#1757) was rejected by the maintainer: the zone is the database's, set by
whoever runs it (`postgresql.conf`, a parameter group), it governs every
application that connects, and a second statement of the same fact is one that
can disagree with the first. #1917 adds `timezone:` as a check only.

- **Which zone.** The one a fresh session of the deploying connection gets:
  server, then database, then the role's setting, then the connection's
  startup options. Measured on 18, a role's setting for a database wins over
  the database's own. `pbps-db` reads it with `SHOW TimeZone` on connect,
  before any pin, because the pins are plain `SET`s that outlive their
  transaction: after the first one the session reports the pin.
- **The text is replayed, never parsed.** Measured on 18.6: `SET TimeZone =
  '+08:00'` is a POSIX zone eight hours *west* of UTC, `<+08>-08` one eight
  hours east, and `localtime` is the server's system zone. The exact text is
  the one reading that is the target's by construction. So zones compare by
  their text: `UTC` and `Etc/UTC` are refused as two zones. Which spellings
  agree is a fact about the engine's tz database that pbps cannot see, and a
  spelling costs a DBA one `ALTER` where a wrong instant costs every value
  written.
- **Recorded** in `PlanBaseline.time_zone` (plan version 38) and named in the
  plan's output, text and JSON (schema set 43): nothing in a statement shows
  it, and the same `2026-02-01` is another instant in another zone.
- **Pinned** by the write pins, the transaction framing, a rendered script,
  the canonical read scope and the #1883 spelling probe. The dialect takes the
  zone as a parameter of `transaction_framing` and `session_pins` rather than
  as a field: the dialect is chosen before any connection exists, and a field
  set afterwards is one forgotten call away from UTC again. Here the compiler
  asks every caller.
- **Required at apply, before anything is read**, transactional, staged and
  resumed alike. In another zone every offset-less value reads back as other
  text, so the drift check would report the zone as a change nobody made —
  measured in the live test, before the check was moved first, the refusal
  was "no longer the database this plan was computed against". A plan that
  records no zone for an engine that has one is refused too.
- **The ambiguity probe** (DEC-1756.1) stops varying `TimeZone`: the recorded
  zone is the intended reading, so an offset-less `timestamptz` has one. It
  still varies `DateStyle`, `IntervalStyle` and `timezone_abbreviations`.
- **The resolver's scratch compiles in the target's zone**, not its own, so a
  declared offset-less default compiles to the instant the target would store.
  A scratch server that does not know the zone is refused by name (SQLSTATE
  `22023`, measured) rather than compiled in another one. The resolver's
  *captures* read in UTC: they are compared only with one another, on servers
  whose own zones can differ, and UTC renders a stored instant the same
  whichever session reads it. `doctor`'s runway reader (DEC-1789.2) reads in
  UTC too: it computes with `timestamptz` bounds as `+00` text, the spelling
  `partitions extend` writes until #1915, and in the target's zone it would
  report every such series on a database outside UTC as a question it could
  not answer.
- **Unchanged.** pbps's own audit times stay UTC (287); `now()` and other
  function defaults; SQL Server, which has no session zone. Offline paths
  (`plan` without `--db`, a target-less `bootstrap --sql`) keep UTC until
  #1918 gives them the declared zone or a refusal; `partitions extend` writes
  `+00` bounds until #1915; the `timestamp` ↔ `timestamptz` refusal of 268 is
  revisited in #1919. pbps is unreleased, so no ledger snapshot read under UTC
  needs migrating.

Pinned by `an_offset_less_value_is_read_in_the_target_databases_own_zone` (CLI,
on 16 and 18: a database in New York with its role in Taipei; a row, a default
and a partition bound stored at Taipei's instants, verify and plan clean, `pull`
writing `+08`, a `DateStyle` reading still refused, and transactional, staged and
resumed applies refused after the zone moved) and
`the_scratch_compiles_in_the_targets_zone_or_not_at_all` (pbps-pg, on 16 and 18)
and `doctor_reads_a_runway_in_utc_on_a_database_in_another_zone` (CLI, on 16 and
18).

<a id="dec-1165-1"></a>

**DEC-1165.1. An assertion must return at least one output column.** On both
PostgreSQL 16.15 and 18.6, `SELECT WHERE false` returns zero rows with zero
columns, while `SELECT FROM generate_series(1,1)` returns one zero-column row.
The existing driver (tokio-postgres 0.7.18) collapses a zero-field
RowDescription and NoData into the
same empty column list; after an empty stream it cannot distinguish the first
query from a command without a result set. Treating that shape as empty success
would also pass commands. ADR-0019 §7 and SPEC §9.9 therefore require at least
one output column, of arbitrary types. A zero-column result is invalid for both
row shapes, distinct from pass and violation, with `SELECT 1 ...` as its remedy.
This also applies when the evidence row bound closes a zero-column stream.
The runner still executes transaction control and verifies completed streams,
so COMMIT/ROLLBACK retain their transaction-boundary diagnosis.

<a id="dec-1165-2"></a>

**DEC-1165.2. Assertion identity includes the virtual transaction, without assigning an xid.**
ADR-0019 §7 requires the first read to prevent a leading READ WRITE switch
and a postcheck that catches COMMIT/ROLLBACK even when the replacement
transaction is read-only. On PostgreSQL 16.15 and 18.6, backend PID,
transaction timestamp and the session's exclusive `virtualxid` lock were
stable within one transaction; all 50 successive read-only transactions
had distinct timestamps and virtual IDs while their assigned xid stayed
NULL. The runner compares PID, timestamp (epoch text, independent of session
date/time formatting) and virtual ID, and requires read-only plus a NULL
`pg_current_xact_id_if_assigned()` before and after the original statement.
A NULL xid or backend PID alone cannot distinguish these transactions;
assigning an xid to obtain identity defeats the NULL check.

The driver prepares the original SQL through the extended protocol,
preserves column metadata for empty results, and streams bounded evidence.
Cells are NULL, bounded rendered text with an explicit truncation flag, or
explicitly unrenderable binary types. Types with server binary output but no
client renderer remain violation evidence; types without server binary output
are refused before execution under DEC-1165.3. No SQL rewriting is needed. Twenty rows
and 64 KiB bound retained cell text, with metadata/cell slots additionally
bounded by PostgreSQL's column limit. Individual wire rows and the driver's
bounded receive buffers can exceed 64 KiB; the result is never collected in
full. Reaching either evidence bound closes the unfinished connection,
records possible further rows and cannot produce pass. Zero-row pass needs
successful stream completion and the unchanged transaction postcheck.
The runner sets LOCAL statement_timeout immediately after BEGIN READ ONLY
to the remaining deadline, rounded up to milliseconds (minimum one, capped
at PostgreSQL's integer limit). Closing the client alone does not interrupt
a backend that is still computing; this server limit also stops it without
waiting for another socket write. Driver/deadline timeout, server cancellation
(57014) and connection termination are unanswerable; other server SQL errors
remain distinct. Dropping a PostgreSQL connection aborts
its owned socket task, including cancellation paths, rather than detaching
the task by dropping only its JoinHandle.

Qualification evidence: the two pinned image builds from ADR-0019 §8,
with stable-identity, standalone COMMIT/ROLLBACK, replacement READ ONLY,
leading READ WRITE (25001), extended multi-statement (42601), and 50 fresh
read-only transactions on each. Live regressions exercise both builds and
distinguish boundary refusal from invalid shape and from SQL error.

<a id="dec-1165-3"></a>

**DEC-1165.3. Refuse result types without server binary output before execution.**
PostgreSQL 16.15 and 18.6 both return 42883, "no binary output function available
for type aclitem", for non-NULL and NULL `aclitem` rows through the native
extended-protocol driver. Zero rows otherwise pass, while an explicit `::text`
cast works. tokio-postgres 0.7.18 always requests binary result fields; a client
unrenderable marker cannot recover a row the server cannot send. Carrying a
driver fork for these internal types costs more than it buys, and the simple
protocol does not preserve the assertion boundary.

After preparing the original statement, before executing it, the runner reads
`pg_type.typsend` for every result type, recursively resolving domain bases and
array elements. This runner query uses the same READ ONLY transaction and does
not change the identity postcheck. Missing or unreadable capability is
Unanswerable. A type lacking binary output makes the result InvalidResult,
even for NULL or zero rows, naming the column and type with the remedy "cast it
to text, e.g. `col::text`". The user statement never executes on that path; it
is neither pass, SQL error nor violation. Native regressions qualify both
builds, domain and array nesting, refusal before an expression can error, the
cast passing, and unreadable catalog access. ADR-0019 §7 and SPEC §9.9 state
this exception alongside the one-output-column rule. Types with server binary
output but no client renderer still produce marked unrenderable evidence.

<a id="dec-1165-4"></a>

**DEC-1165.4. The test manifest is read strictly and every pin names one commit.**
The manifest parser takes only what the text can answer and returns every
problem in one pass; paths, containment and git facts belong to the caller. The
version is read first, so a manifest from a later version is refused for its
version rather than for the first key this build does not know. `candidate:
working-tree` is reserved: a branch of that name is selected by its full commit
id. An image is `name@sha256:<64 lowercase hex>` and may not also carry a tag,
since a tag that disagrees with the digest is a second claim about which image
is meant (ADR-0019 §2 asks for a digest reference, not a mutable tag). Pins
are 40- or 64-digit hexadecimal ids, stored lowercase.

Inputs are read, hashed and kept in one step: a fixture's text is the bytes
that were hashed, never reopened by the runner. A path is judged by what it
resolves to, so a symlink leaving the project and one file spelled two ways are
both caught. An unknown version, an unreadable file and an input changed after
it was read cannot be answered (`unanswerable`, exit 1); every other refusal is
a finding (exit 2). A file that is missing, not regular, outside the project,
not UTF-8 and unreadable are five different facts, and an empty file is none of
them. The recheck compares what each path resolves to as well as its hash, so a
symlink moved to a file with the same bytes is noticed.

<a id="dec-1165-5"></a>

**DEC-1165.5. Commits are resolved once, and their order is asked of git, never assumed.**
A pin, the base and an explicit candidate are named by the commit id git
resolves once; a branch moves between this call and the run, so the report
keeps the id and never the spelling. Git still exits 0 for a name that is both a
branch and a tag and only warns, and `--quiet` hides the warning, so any output
beside the answer refuses the revision instead of matching the warning's words.
A pin must be an ancestor of the base, which must be an ancestor of the
candidate commit or of the `HEAD` captured for a working tree. Each ordered pair
is asked of git once, merged history counts and a first-parent walk is not
used, and two pins neither of which contains the other are refused by name
rather than sorted. An answer git could not give (a shallow boundary, an
unreadable history) is never read as "not an ancestor": it is the unanswerable
outcome, not a finding. `baseline::ancestry::check` becomes the two-commit
`check_between` and keeps its evidence classes, so the two callers cannot
disagree about what git said.

<a id="dec-1165-6"></a>

**DEC-1165.6. A revision's inputs are read once, and "clean offline" is not "will plan".**
Config, declarations and the identity file of a commit are read through one
tree listing and one `cat-file --batch` (the baseline reader), hashed as raw
bytes and loaded from those same bytes. An entry git has no record of is
absent, which is not empty; one it cannot read is a refusal, and so is any
symlink or submodule below the declarations directory whatever it is called,
because the working-tree loader follows a link to a directory and git does not,
and a submodule's files are not in the commit. A working-tree declaration whose
name is not UTF-8 is refused too: the name is the declaration's key. A working-tree
path is recorded as it was traversed, never with `..` collapsed after a symlink,
and an identity file of a version this tool does not read is unanswerable, not
inconsistent or malformed: the version is read before the shape, as the
manifest's is. `baseline::ids_from` reads any failed `git show` as an empty identity
file (#1814) and is not used here, nor fixed here. A commit with no `pbps.yml`
is refused, not read as a project that did not exist yet, because the manifest's
dialect must match each revision's own configuration. A working tree takes the
same path from the files `plan` reads, so equal bytes give equal hashes, and its
membership is rechecked as well as its bytes: a declaration added or removed
after the read is as disqualifying as one edited.

For every distinct revision the proof is that its declarations load, its
dialect accepts them, and its committed identity file already equals what
resolution would record with no decision pending: the check `plan --check` makes
without a database. It is **not** a proof that the jump between two revisions
plans; only the replay's database capture can say that, and the ordinary differ
decides each hop there. The resolution's operator and date only stamp tombstones
that would make the file differ and refuse anyway, so neither is read from the
machine. The project's declaration policies bind the candidate alone: an older
pin may predate a rule and must still replay.

<a id="dec-1165-7"></a>

**DEC-1165.7. A test run owns its scratch by a private record, not by a clock or a name.**
`pbps test` adds to the resolver's Docker recovery (DEC-2410.3) only what
ADR-0019 section 3 asks beyond it, and builds no second sweep. The container is
still the resolver's (`Held::start_owned`: its labels, its keeper, its removal),
so a resolver run's sweep sees a live test run as alive. What is new is on the
client: a run id of 128 random bits, a record written atomically **before**
`docker run` and advanced to the container's id once the runtime returns it, and
an exclusive lock held for the run. The state directory is
`$XDG_STATE_HOME/pbps/test-runs` (a relative value is ignored), owned by the user
and mode 0700, and a link in place of it or of a record is refused, because a
record that can be redirected is not evidence. The record holds ownership only:
no SQL and no credential.

A later run calls the owner dead only if its lock is free **and** the recorded
pid is gone, or has another start time, or the boot differs. A free lock with
the same live process proves nothing (a filesystem that ignores locks looks the
same) and is reported, never acted on. A record with no id is resolved by
listing the exact run label on the recorded runtime, never by name; an id is
inspected and removed by that immutable id, and only when its labels, name and
mounts match the record and no other container uses its volumes. A different
container now holding the recorded name is not the original and is left alone.
The runtime recorded must be the runtime asked: its endpoint and an identity
read with the first `info` template that works (Podman 5.7 rejects `{{.ID}}`).

One function, `removal::remove_run`, does every removal and every record
deletion (normal teardown, recovery, a failed start), and states the rule once:
evidence (container id and volumes) is persisted before any removal, and a
record is deleted only after confirmed absence of the container and of every
recorded volume. A finding that a path forgets or misjudges evidence is fixed
in that function, never beside it.
A removal that reports names it cannot identify (the start's own, before the
container's id is known) leaves them on the record as `leftovers`, and such a
record is never forgotten automatically: only a person resolves it. A start
whose record already holds the id and volumes is an ordinary retry instead, and
until the id is recorded the container's drop fallback stands down
(`Owner::recoverable`), and until then the start does not remove the container
either on failure: the owner's journaled removal does, after recording the
evidence. So a cancelled or failed start leaves the container for the label
listing and never one half-removed with its volume unnamed.

Absence, an unreadable inspection and a timeout are three results. Only the
runtime saying "no such container" and "no such volume" is absence; the record
is deleted after that and not before. Anything else is a `CleanupFailure` naming
the run, the resources and the manual removal, and it fails the command whatever
the assertions did, which the resolver's sweep deliberately never does.
`TestScratch::finish` is the only normal teardown and returns it; both types are
`#[must_use]`.

**Residual window.** A run killed after the client sent `docker run` and before
the daemon created the container, found by a label listing that returns nothing,
is read as never created, and the record goes. The daemon can in principle still
complete that create. ADR-0019 accepts this; the resolver's own sweep still
covers such a container by its keeper and grace. Linux only: the process
identity is `/proc` and the boot id, and every other platform refuses with one
typed error rather than record a weaker proof.


<a id="dec-1165-8"></a>

**DEC-1165.8. Deployment commands share loaded-input and in-memory artifact boundaries.**
The ordinary bootstrap and connected planner consume declarations and ids read
once by their command wrappers; the planner returns its sealed artifact,
optional script and existing report data before publication. Apply's existing
post-decode admission and execution path is shared without moving its public
hooks out of the command. SQL is still written before the plan, bootstrap's
script write still participates in its build transaction, and committed results
still report lock cleanup separately. This behaviour-preserving extraction
adds no test command, runner callback, progress protocol or format field;
ADR-0019's owned runner and verified re-pin follow separately. Regression tests
pin original script bytes, refusal/publication order, exact hook payloads and
transactional, staged and empty-plan command results on both engines.


<a id="dec-1165-9"></a>

**DEC-1165.9. Owned test execution composes the ordinary deployment services.**
D2a exposes asynchronous entry points to the same bootstrap, seal/admit and
apply bodies that ordinary commands use; it adds no statement ordering,
emitter or public hook. Its consumer is D2b's PostgreSQL `pbps test` command.
The owned session derives disjoint role names from its run id and generates
separate apply, fixture and assertion credentials
and derives its target only from that run's verified `TestScratch`. It provisions
the namespace prerequisites from bootstrap's existing inventory. Fixtures
receive DML only on managed non-`data:` tables and USAGE only on their owned or
default-dependent sequences; an effective PUBLIC grant that broadens the
DML or permanent CREATE boundary refuses. The private context retains declaration/risk/trust settings but discards user
runtime selectors, including project-level resolver defaults that may name an
unowned server; unsupported syntax-only cases keep their ordinary refusals.
The private fingerprint key is a journal-owned file, removed
by the same confirmed normal/dead-owner cleanup as the resource record.
Startup and failed-start cleanup have separate deadlines; the large work
futures stay boxed after a composed unoptimized test overflowed its stack.
Admission and statement submission after the final guards are distinct
in-memory observations. An owned invocation collects ordinary progress as
findings; existing commands keep their exact text. Evidence is typed `data` in the
existing `output::Report` envelope, with ordinary findings and no plan payload
or separate renderer. Live PostgreSQL16/18 lifecycle tests pin wrong-checksum
and stale-baseline refusals, retained values, actual SQL identities/permissions,
assertion results and confirmed cleanup. D2b2 adds command/replay/publication and
the published results schema; verified fixture re-pinning remains E. D2a changes no published format.

The owned run's exact provision and generated execution roles are infrastructure, excluded from the
unmanaged-policy inventory without weakening that policy for other identities.
Fixtures execute managed routines as an ordinary writer, including invoker callees
whose dependencies PostgreSQL does not record; declared executable code is trusted
under ADR-0019 §3/§6. PUBLIC and assertion EXECUTE stay unchanged; fixtures
retain NOCREATEROLE and cannot assume the apply identity.

Before each bootstrap or plan, the owned administrator supplies that revision's
namespace prerequisites and missing `Schema::named_roles` (including policy
recipients) through the existing inventories and presence check. New roles are
NOLOGIN without elevated privileges; present roles keep their attributes and
ordinary managed/unmanaged scope. Preparing future revisions together would
falsely make future roles unmanaged under `unmanaged: error`. Ordinary PostgreSQL
role rules outside the owned cluster stay unchanged (ADR-0010 §3; ADR-0019 §6).
Lock cleanup is a separate finding and cannot replace the primary deployment
cause or erase a committed entry from phase evidence (ADR-0019 §9).

Run-owned routine EXECUTE grants are removed before ordinary planning/apply and
refreshed for fixtures; other principals' grants retain the ordinary rebuild
check. Effective write checks include column privileges for both identities
(PG16/18 routine-rebuild and column-grant regressions). Interrupted creation
without a returned id retains the existing unconfirmed-leftover evidence; a
missing before-create record or an identified absent container is still
cleanable (ADR-0019 §3/§6).


<a id="dec-1165-10"></a>

**DEC-1165.10. Owned work shares one cancellation observation and keeps partial evidence.**
D2b1 prepares D2b2's command without publishing a command or schema. One sticky
cancellation observation spans acquisition, provision and work; failed-start
settlement and final cleanup retain their separate bounded allowances. A
`StartCleanup` carries its own run/resource/remedy evidence beside startup's
primary cause. Each ordinary bootstrap, plan and apply starts its own step
budget under the run deadline. D2a's non-inlined boxing seam keeps each bounded
service future off its caller's frame; size bounds and PG16/18 lifecycle tests
cover both the ordinary test stack and the production CLI-thread boundary.
Dropping a work future cannot drop its borrowed
admission/submission observations or findings, and refreshing grants adds to
phase time instead of replacing it. The existing readers retain each available
file hash and resolved commit even when a later input refuses; missing identities
have explicit reasons. Qualification requires input re-verification and confirmed
cleanup, with both original failure and cleanup outcomes retained in the existing
report envelope (ADR-0019 §2/§3/§9). PG16/18 direct D2a-service tests pin retained
values, ordinary refusals, cancellation and cleanup; input/evidence tests pin
partial refusals and unavailable identities. Replay and public publication are
D2b2; the prerequisite-design class remains #2552 and the combined bootstrap/
unlock evidence follow-up remains #2547.


<a id="dec-1165-11"></a>

**DEC-1165.11. The public PostgreSQL test command replays frozen endpoints through the owned services.**
D2b2 exposes run and cleanup through the existing findings envelope, with no
connection selector or applyable plan output. Fixture groups follow git ancestry;
files sharing a pin keep manifest order and one fixture session. Ordinary sealed
plans replay endpoint jumps and then the candidate, preserving admission and
submission observations separately. Namespace prerequisites cover all frozen
revisions; roles still wait for their hop so future identities cannot poison an
earlier unmanaged check. D2b1 owns cancellation, budgets, partial evidence and
confirmed cleanup, and publication also rechecks the frozen inputs. The new
command is outside older closed envelope branches, so DEC-1734.1 raises the
archive compatibility floor to this schema set while wire version 1 stays. Public CLI
PG16/18 regressions cover retained rows, replay order, candidate checksums,
assertion/fixture refusals, cancellation and input changes. SQL Server and verified
fixture re-pinning remain later #1165 slices; #2552 and the deferred reporting
findings retain their existing scope (ADR-0019).

<a id="dec-1165-12"></a>

**DEC-1165.12. A fixture re-pin compares rows and allocators in two fresh scratches.**
`pbps test fixtures repin` reuses the pinned replay of DEC-1165.11 twice, each
time in its own fresh owned scratch: the original side loads the committed
(`HEAD`) fixture at its old pin and replays through the new revision, the
proposed side loads the edited working file at the new revision, and both load
every other fixture pinned at or before it in manifest order. Separate scratches
keep a replayed role rename from leaking across sides; the cost is one more
container start. Rows are compared as multisets of each column's text, columns
sorted by name (a hop appends a column that a bootstrap declares in order), with
NULL, duplicates, case and whitespace all differences. Allocators are every user
sequence except the ledger tables' own, so one reached only through a default
function is still compared, each by an engine `nextval` probe in the discarded
database, never `last_value + 1`; the table and column that own it are reported
when there is one. A difference is a finding (2); an unreadable or exhausted
allocator, an unreadable table, differing engines or a changed input is
unanswerable (1). The envelope gains a `repin` record and a phase `side` (schema
set 89; the manifest write status is text so the write that follows needs no new
set). SQL Server re-pin remains a later #1165 slice.

<a id="dec-1165-13"></a>

**DEC-1165.13. The re-pin writes the manifest only after a verified match and confirmed cleanup.**
The selected `at` scalar is spliced by its YAML byte span and proved by reparsing
to the old manifest with exactly that pin changed, so another occurrence of the
same id (another fixture, or `base`) can never be the one written. The result is
written to a synced temporary file beside the manifest, checked against the
inputs and `HEAD` once more, renamed over it and read back. It happens only when
every other phase passed and both cleanups are confirmed; otherwise the manifest
keeps its exact bytes and the report says `not_attempted` or `failed`. A crash
leaves the whole old or whole new manifest and at most one orphan
`.…repin-….tmp`. A writer that replaces the manifest between the last check and
the rename is not detected: POSIX offers no compare-and-swap for a file, and a
lock would not bind editors, so that window is documented, not closed.

<a id="dec-1165-14"></a>

**DEC-1165.14. The SQL Server test session is the same `Session`, with its SQL in `pbps-mssql`.**
`Session` keeps its method names and dispatches by the scratch's engine, so the
ordinary plan, apply and replay adapters, the fixture loop and `Control` run
SQL Server unchanged; the PostgreSQL arm is the original code. The SQL is in
`pbps_mssql::test_scratch` (dialect boundary), the connections and cleanup in the
CLI. Three generated logins exist per run and only the administrator creates
them. The apply login is `dbo` of the owned database by
`ALTER AUTHORIZATION ON DATABASE` (measured: bootstrap, plan and apply need no
server-level right), and declared schemas are created `AUTHORIZATION dbo`, so
every managed object has one owner and ownership chaining reaches the
functions a default or check calls without a fixture EXECUTE grant. The fixture
and assertion users hold exactly ADR-0019 §6 on the tables (`data:` tables
SELECT only), re-granted after every hop; each grant refresh then reads
`sys.fn_my_permissions` *as* each user for the database, every schema and
every managed table, and the logins' server permissions and database access
are read once at provisioning, so a role, `public` or inherited right is found
as readily as a direct one. A fixture file is split at standalone `GO` lines
outside any literal, identifier or comment and run batch by batch in one
session (`#temp` objects survive a `GO`); `GO <count>` and SQLCMD lines are
refused before anything runs. A fixture that leaves a transaction open fails:
closing its session would discard the writes with no error. SQL Server has no
statement timeout, so a fixture's bound is the deadline dropping its connection.
`engine_build` is the first `@@VERSION` line plus the server collation, so the
two sides of a re-pin must agree on the one setting the image chooses that
changes how text compares; no format version changes. Measured: the SQL Server
image declares no volume, so a run has no volume to confirm gone.

<a id="dec-1165-15"></a>

**DEC-1165.15. A SQL Server run seals its owned database READ_ONLY, and error numbers are read by engine.**
`pbps test` runs on SQL Server through the same `Session`, which gains two
methods the runner calls without naming an engine: `seal`, after the last
grants refresh and before the first assertion, and `run_assertion`. PostgreSQL's
write boundary is the READ ONLY transaction each assertion opens, so its `seal`
does nothing. SQL Server has no such transaction, so the boundary is the database
mode. The seal reads `sys.dm_exec_sessions` first and refuses while any other
user session is on the database, naming it, and never evicts one: a plain
`ALTER DATABASE … SET READ_ONLY` waits without limit behind a held session and
`LOCK_TIMEOUT` and `NO_WAIT` do not bound it (measured), so the refusal comes
first and the statement runs under the driver deadline. It then reads
`DATABASEPROPERTYEX(…, 'Updateability')`. Each assertion connection reads that
mode again before its SQL, so a zero-row pass never rests on a database nothing
sealed. A failed seal is a `test.infrastructure` finding and no assertion runs.
There is no report or manifest field for it: the typed result of a sealed run is
the same as before, and no format version changes. `Phase` takes the run's
engine at construction (`on`) or declares itself database-free (`offline`), and
`unanswerable` reads a server error code by it: SQLSTATE classes 08, 53, 58 and
XX on PostgreSQL, a short list of engine fault numbers on SQL Server (resources,
storage, a session the server ended). A SQL Server number is not a SQLSTATE: a
user `THROW 53300` is the user's own, and a permission error (229) or a write to
the sealed database (3906) is a finding. The same reading applies to the error
an assertion itself returns: a coded engine fault there is unanswerable (exit 1),
not a finding about the SQL, on either engine. `fixtures repin` followed in F3
(DEC-1165.16).

<a id="dec-1165-16"></a>

**DEC-1165.16. A SQL Server re-pin reads typed rows and really-taken allocator values, as the apply login.**
`fixtures repin` runs on SQL Server through the same two fresh scratches, replay
and comparison as PostgreSQL; only the read of a comparison database is the
engine's. Rows come back as one text cell per column, chosen by the column's
base type (a float to 17 digits, a money value to 4 decimals, dates in a fixed
style, binary and spatial values as hex), and are counted in Rust, not by a
server `GROUP BY`: the default collation folds case and trailing spaces, and the
comparison must not. `NULL` stays apart from the empty string and columns are
compared sorted by name. A table that cannot be compared makes the re-pin
unanswerable and names the table and column: a `rowversion`, a system-versioned
period or ledger column differs between any two databases by construction, and a
`sql_variant` or an unknown type has no faithful text. It is never skipped
silently (ADR-0019 §5), so a table with a temporal or rowversion column cannot be
re-pinned until that is declared another way.

A table with an enabled row-level security FILTER predicate is refused too, before
its rows are read. Measured: SQL Server has no owner bypass, so the apply login
(`dbo`) and even `sa` read only the rows the predicate lets through (1 of 3 in
the measurement), two databases differing only in hidden rows would read as equal,
and a wrong pin would be written. The policy catalog counts as evidence only when
fully visible (database `VIEW DEFINITION` and no effective metadata DENY, the test
`drop_requirements` uses); an unreadable catalog is not an empty one. A disabled
policy and a BLOCK-only policy hide nothing and still read. PostgreSQL has no such
gap in this path: its re-pin reads as the scratch superuser, which bypasses row
security even under `FORCE ROW LEVEL SECURITY` (3 of 3 rows, measured, against 1
for a non-bypassing owner). Found by ready review round 1 on the head the first
full gate covered.

The next value of an identity column is the one the engine hands out. Measured:
`IDENT_CURRENT` alone is ambiguous (after `DBCC CHECKIDENT … RESEED, 50` a column
that never generated a value hands out 50, one that had hands out 51) and
`last_value` is `NULL` after `TRUNCATE`; a failed `INSERT` still consumes a value
(NOT NULL and UNIQUE violations both). So the probe is `INSERT … DEFAULT VALUES`
in `TRY/CATCH` inside `BEGIN TRANSACTION … ROLLBACK`; the identity survives the
rollback and the row and a trigger's writes do not, and the pair (`IDENT_CURRENT`,
`last_value`) must have moved, and by exactly what one allocation takes from the
state just before (the current value of a column that never generated one, else
the last plus the increment), or the allocator is unestablishable and named: an
exhausted column, a failure before a value is taken, an `INSTEAD OF` trigger that
swallows the insert, a trigger that inserts into the same table.
A probe can also move other allocators (a trigger inserting into another table,
possibly only while that table is in some state; a default taking a value of a
sequence), and identity values survive the rollback. So no allocator is reported
from a probe: every identity column's next value is taken from its state before
any probe, and the probe only has to agree with the state it started from
(draft review rounds 1 and 2 of #2690 each found a way a probe could otherwise let
two different allocator states compare equal). A column a trigger moved earlier
is not probed again (a probe of a column whose last value another table's trigger
took would exhaust it and refuse an equivalent re-pin): the move is checked
against its state read first (onward, in whole increments), and the shifted value
never reaches the report. Disabling the table's triggers during the probe was
tried and rejected: the DDL discards the table's cached identity range and
the next value jumps (measured: 1002 for a next value of 3). A sequence is read
with `NEXT VALUE FOR`, before any identity probe, and compared under the one
column whose default names it, else by its own name. Every table's rows are read
before any probe.

Both reads run on the apply login, the owner of the objects, not on the
administrator: a table can carry a computed column or a trigger, which is user
code. PostgreSQL's re-pin keeps reading as the administrator. No report or
manifest field is added and no format version changes. The cause of a failed
scratch start is no longer lost in the lifecycle test (#2683).

<a id="dec-2547-1"></a>

**DEC-2547.1. A failed owned bootstrap carries its lock release beside its cause.**
The shared bootstrap service returns the outcome of the deployment-lock release
with a failure taken under the lock, as `apply` does with `Attempt::Failed`
(`Bootstrap::Failed`); a refusal before the lock stays an ordinary error, since
there is no lock to release. The command prints its existing "lock was not
released" warning from the wrapper, so its stderr and exit codes do not change.
The owned adapter hands the same outcome to the shared `finish_attempt`, which
records `test.cleanup`, marks the phase unanswerable and keeps a committed
ledger entry, and only then returns the primary error (ADR-0019 §9). The owned
run therefore no longer prints that warning on stderr for a bootstrap, as it
already did not for an apply: the finding is its report. No format version
changes.

<a id="dec-2552-1"></a>

**DEC-2552.1. Owned prerequisites follow the recorded identities of the two hops they sit between.**
Preparation takes the previous hop (absent at the first bootstrap, stated, not
defaulted) and the incoming one, and asks their recorded identity mappings
before the catalog. A recorded role rename is the same uid under two names, one
rule (`IdsFile::role_renames`) shared with the ordinary cluster-role evidence
check. Inside the owned scratch only, a rename whose old name is present and
new name absent is completed with `ALTER ROLE ... RENAME TO`, so the principal
keeps its oid, attributes, memberships, ownership, grants and policy
recipients. Every other state is left to the ordinary check: an occupied
destination (`BothPresent`), a vanished source (`NeitherPresent`) and an
unreadable catalog refuse as they do outside the owned cluster, and a spelling
resemblance never supplies identity intent. Roles are created only when the
incoming hop names them and the previous hop did not and no rename accounts for
them; a retained name the catalog lacks is the ordinary presence check's to
refuse, not preparation's to recreate. A rename chain inside one hop (a->b with
b->c) is not completed and keeps the ordinary refusal; across hops it is.
The recorded baseline follows a renamed role into the row-security policies
that apply to it (`Schema::rename_role`), as the engine binds a policy by oid;
without it the drift check read the renamed recipient as a change, for the
ordinary plan as well as the owned one. No format version changes, and SQL
Server, whose roles move through the typed plan, is untouched.

<a id="dec-2670-1"></a>

**DEC-2670.1. PostgreSQL fixtures use the existing transaction probe before their session can close.**
After each fixture file, on its own connection and within its fixture deadline,
`Session::run_fixture` calls `pbps_pg::catalog::in_transaction` and refuses an
open transaction as `test.fixture`, as SQL Server already does (DEC-1165.14).
The replay carries the file path into that finding and stops before any later
fixture, capture, apply or assertion could treat discarded rows as empty input.
The probe reuses the per-call token and transaction-local setting of decision
312; it adds no SQL or probe mechanism to the CLI. Measured through
`pbps_db::Conn` on PostgreSQL 16.15 and 18.6: timestamp inequality reports true
even after autocommit and COMMIT because the extended protocol starts its
implicit transaction before Execute, while both transaction-ID probes return
NULL for an empty or read-only open transaction. The existing probe correctly
distinguishes all of those cases, committed savepoints and ROLLBACK, including
a session that already holds `pbps.in_a_transaction = 'yes'`. No format version
changes.

<a id="dec-2459-1"></a>

**DEC-2459.1. A Podman test-run identity includes the hostname, reported socket and graph root.**
The Docker path stays `DOCKER_HOST` (or `default`) plus the daemon's id and root.
Podman 5.7 rejects that id template, and its graph root alone is a path another
host can share. `CONTAINER_HOST`, `CONTAINER_CONNECTION` and the configured
remote default can all change the runtime without changing `DOCKER_HOST`.
The fallback therefore reads `docker info --format '{{json .}}'`: the reported
`host.remoteSocket.path` is the endpoint, and a tagged JSON array of
`host.hostname` and `store.graphRoot` is the identity. JSON keeps values that
contain delimiters distinct. Every part must be a nonempty string; a missing,
empty, wrongly typed or unreadable answer refuses discovery before recovery
can inspect resources or delete a record. No graph-root-only fallback remains.

Measured on this host's rootless Podman 5.7.0 (`docker` executes Podman): the
unmodified connection list was `[]`, the hostname was `leonx1`, the graph root
was `/home/lll86789/.local/share/containers/storage`, and no host UUID was
reported. Two temporary API services on different private Unix sockets shared
that hostname and graph root, while `host.remoteSocket.path` distinguished
their endpoints for explicit hosts, named connections and the remote default.
The eight successful info probes also confirmed that equivalent selections
report the same socket, that a configured default alone does not enable remote
mode, and that a named connection takes precedence over `CONTAINER_HOST`.
The isolated connection file and probe sockets did not change the user's
connection configuration; both service processes were stopped.

The record's two opaque strings and `VERSION = 1` stay unchanged in shape.
There are no users yet, so conservatively refusing older Podman records that
hold only a graph root is acceptable: recovery keeps them for manual inspection
instead of inferring their runtime. Existing Docker records still match.
