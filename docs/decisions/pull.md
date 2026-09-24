# `pull` and the catalog read

Reading a database into declarations: what is read, what is omitted, and how
omissions are reported. Part of the [decision record](../DECISIONS.md), which
says how to add an entry here.

<a id="decision-13"></a>

13. **Normalization targets what the catalog stores**, not what the user wrote
    (`numeric`→`decimal`, `float(24)`→`real`, `varchar`→`varchar(1)`);
    introspection reads the stored form back, and any gap is a phantom diff.

<a id="decision-14"></a>

14. **`pull` never drops what it cannot express** (computed columns, UDTs,
    indexes whose physical kind the model has no place for — clustered,
    columnstore, XML, spatial, hash — unmanageable modules): each becomes a warning or an
    inventory entry, and a table with no expressible columns is left out whole.
    The round-trip `load(render(pulled)) == pulled` is pinned by
    `pbps-cli/tests/pull_roundtrip.rs`, for modules as well as tables.

<a id="decision-17"></a>

17. **`pull` inventories what it cannot manage.** Everything it finds and
    cannot express is listed with the reason (ADR-0002); that is separate from
    `warnings`, which are defects in the pull itself.

<a id="decision-60"></a>

60. **The catalog reports what the model cannot hold; it never folds it.** A
    `DENY` is not "no grant", a column-level `SELECT` is not a table-level one,
    and `WITH GRANT OPTION` is not a plain grant. Each becomes a `pull` warning
    naming the role and the target, and a plain grant is recorded only for the
    last of them (with the warning), because the declaration can express that
    much and the difference is one the next drift check will not be able to see
    — which is said, rather than hidden.

<a id="decision-177"></a>

177. **A name is kept as the database spells it; `trim()` asks whether there
    is one, and nothing more.**
    Measured on SQL Server 2025: `CREATE ROLE [ app_pad ]` stores the padding,
    and so does `[trail ]`. `needs_quotes` refuses any scalar that is not its
    own `trim()`, so `pull` writes such a name back quoted and YAML hands it
    to the loader intact — where `convert_role` trimmed it. A freshly pulled
    project therefore named a role the database does not have while the ids
    file named the one it does, which reads as an ambiguous replacement rather
    than a clean plan, and `init --from` fails its staged round trip. The
    `renamed_from` beside it had the same trim and the worse consequence: it
    names a principal the database *has*, and trimmed it renames one that is
    not there.
    Both keep the scalar now, with `trim()` used only for "is there a name at
    all". Swept: `TableName::from_str` and `ObjectName::from_str` never
    trimmed, so a padded table or module name already round-trips, and `seq`
    renders every element through `scalar`, so `columns: [" c "]` does too.
    **One asymmetry is left, and deliberately.** A foreign key's target is one
    composite scalar — `dbo.region(region_id)`, the column names joined with
    `", "` inside it — so a padded column name there is indistinguishable from
    the separator's own whitespace. The same column survives in `columns:` and
    does not in `references:`. Fixing it is a *format* change, not a stray
    trim: either the separator stops taking a space (which reinterprets every
    file already written) or the composite grows a quoting rule of its own.
    That is a decision, not a bug fix, and it is recorded here rather than
    made in passing.

<a id="decision-178"></a>

178. **177 one crate over, and the sweep that missed it.** `GrantTarget`
    trimmed too — the whole scalar, and again after the `schema::` prefix. I
    swept `pbps-load` for destructive trims and the name types beside it, and
    stopped at the crate boundary; the parser that turns a grant's map key
    into a target lives in `pbps-model`. Measured: `CREATE SCHEMA [ app]`
    keeps its padding, `pull` renders the key as `"schema:: app"` (quoted,
    because the `::` makes `needs_quotes` true whatever else is in it), and
    the reload named a schema the database does not have.
    **It amends 126.** That entry counted "a target with surrounding
    whitespace" as a third spelling of one target, beside `SCHEMA::` and
    `schema::`. The engine says otherwise: `[ app]` and `[app]` are two
    schemas, and `[ dbo].[t]` and `[dbo].[t]` two tables. So they are not two
    spellings to refuse but two targets to keep, and the test that pinned the
    old reading now pins this one. What survives of 126 is its mechanism and
    the case it was really about: the prefix is case-insensitive, two
    spellings of it are one key, and the loader still refuses the second
    rather than letting the map keep whichever came last.
    The cost is a stray space in a hand-written target no longer being caught
    by the duplicate check. It is caught later, as a grant on a securable the
    declarations do not have — which is where a name that does not exist
    belongs, and 126's message would have been the wrong one for it anyway.

<a id="decision-247"></a>

247. **A catalog row of a kind the reader does not know is reported, never
    folded into the nearest kind it does.** `pg_constraint.contype` is an open
    set at the engine's end, and PostgreSQL 18 proved it: every `NOT NULL` now
    has a constraint row of kind `n`. A reader that had parsed the characters it
    knew into an enum and let the rest fall through to "a check" would have
    started reporting one phantom check per `NOT NULL` column on an engine
    upgrade — and the differ would have planned to drop each one. So the kind
    travels as the engine's own character, the assembler matches the kinds it
    holds, and anything else becomes a named limitation carrying the
    constraint's own definition. The cost is a warning on a database using a
    feature pbps does not manage; the alternative is a plan against a phantom.

<a id="decision-248"></a>

248. **A foreign key whose referential action the model cannot spell is left
    out and named, not read back as the nearest action it can.** PostgreSQL has
    `RESTRICT` and `ReferentialAction` does not. The two are close enough to
    tempt: `NO ACTION` and `RESTRICT` both refuse the delete. They are not the
    same — `NO ACTION` is checked at the end of the statement and can be
    deferred, `RESTRICT` fires immediately — so a pull that folded one into the
    other would let a plan replace a key's behaviour while reporting no change
    at all. The key is therefore absent from the pull and present in the
    warnings, which is the shape every other unexpressible fact takes here.

<a id="decision-249"></a>

249. **A foreign key's referenced columns are read out of
    `pg_get_constraintdef`, not resolved with a second catalog join.**
    `confkey` holds attnums on the *referenced* table, which the constrained
    table's attnum map cannot answer for. Resolving them properly means another
    join, and it would put the answer inside the query file — where no test can
    reach it without a server. The definition already spells them and its shape
    is fixed, so the assembler parses it there, in the pure half, and refuses
    the parse when the column count disagrees with `confkey`. A misparse
    becomes a named limitation rather than a foreign key over the wrong
    columns.

<a id="decision-250"></a>

250. **The whole catalog read is one `REPEATABLE READ READ ONLY` transaction,
    and the canonical search path is set inside it.** Five autocommit
    statements are five snapshots. A table dropped between the tables query and
    the columns query comes back as a live table with no columns — which
    assembles cleanly, compares as a table whose every column was deleted, and
    plans accordingly; nothing about it looks like a failure. One snapshot
    makes the five reads unable to disagree about what exists. `READ ONLY` is
    the engine enforcing what a comment would otherwise only promise (measured:
    `cannot execute CREATE TABLE in a read-only transaction`). And the path is
    set with `is_local`, so **ending** the transaction restores it — measured on
    `COMMIT` and on `ROLLBACK` alike. That is the difference between handling
    "a read failed halfway and left the session changed" and making it
    unrepresentable.

<a id="decision-251"></a>

251. **A property of an object the model cannot hold means the object is left
    out; a fact about the rows already there means it is carried.** Both are
    named either way, and the line decides which way the resulting plan is
    wrong. `RESTRICT`, `DEFERRABLE`, a `gin` index: carried, each compares
    equal to an object that behaves differently, so a plan reports no change
    while the behaviour stays wrong — the failure this tool exists to prevent.
    Left out, the plan tries to create something that is already there and
    fails on apply, loudly, with the warning saying why. `NOT VALID` is the
    other kind: the constraint itself is exactly what the model says, and what
    recreating it changes is which rows get checked. Carried and named, that is
    a plan that may fail on apply rather than one that lies.

<a id="decision-252"></a>

252. **A foreign key's referenced columns are resolved against the referenced
    table's own columns, which the pull already has. Supersedes 249.** That
    entry chose to parse them out of `pg_get_constraintdef`, on the grounds that
    `confkey` names attnums on the *other* table and a second catalog join would
    put the answer where no test can reach it. The parse is wrong on a legal
    name: `FOREIGN KEY (x, y) REFERENCES q(x, "a)b")` stops at the `)` inside
    the quoted identifier, produces two items, passes its own count check
    against `confkey`, and records the column `"a`. The count check was the
    guard, and it agreed with the wrong answer. What 249 missed is that the
    columns of every table in the pull are already in the assembler: no parse,
    no second query, and an attnum with nothing behind it is the same named
    limitation as everywhere else.

<a id="decision-253"></a>

253. **A pull inside the caller's own transaction is refused, not
    accommodated.** PostgreSQL does not nest transactions: inside an open one a
    plain `BEGIN` is a warning, so the `COMMIT` that ends a successful read
    would commit whatever the caller had written, while the `REPEATABLE READ
    READ ONLY` snapshot 250 exists for was never established. A savepoint would
    give back the framing but not the meaning — a read inside somebody's
    transaction answers from their uncommitted writes, which is not what "what
    the database looks like" is. So the pull asks first and refuses.

    The asking is a `SET LOCAL` on a custom GUC, read back in a second
    statement: a local setting outlives its own statement only inside a
    transaction block. Every cheaper question was measured and reads the same in
    both states **through this driver** — `xact_start = query_start` and
    `transaction_timestamp() = statement_timestamp()` are both false even
    outside a transaction, because the extended query protocol opens the
    implicit transaction before the statement's own clock starts. Measured with
    `psql`, which speaks the simple protocol, both looked like reliable
    detectors.

<a id="decision-254"></a>

254. **The pull's canonical scope pins how values print, not only how names
    do.** 250 set `search_path` empty so that a rendered name does not depend on
    the reader's session. The expressions this pull carries are carried verbatim
    (ADR-0013 §4), and the same argument applies to every setting the deparser
    consults: measured on 18.6, `quote_all_identifiers` turns `id > 0` into
    `"id" > 0`, `DateStyle` turns `'2020-01-02'::date` into `'02.01.2020'::date`,
    `TimeZone` moves a `timestamptz` default to another wall clock,
    `IntervalStyle` turns `'1 day 02:00:00'` into `'1 2:00:00'`, and
    `bytea_output` turns `'\x0102'` into `'\\001\\002'`. Two operators with
    different sessions would otherwise see drift on an unchanged database, and a
    plan would rebuild every constraint and index it touched.

    `extra_float_digits` is pinned on the same argument without a case that
    demonstrated it. `lc_monetary` is deliberately **not**: it belongs to the
    same class, and `SET` fails outright on a locale the server does not have,
    which would turn a readable database into an unreadable one for a difference
    nobody has yet shown.

<a id="decision-255"></a>

255. **A name is round-tripped through the declaration format, not checked
    against a rule.** `TableName` is written `schema.name` and read back by
    splitting on every `.`; `ColumnRef` the same with three parts. PostgreSQL
    will hand out a schema called `"a.b"`, and then a pull that succeeded
    produces a schema whose own file does not load — or worse, one where
    `a.b` + `t` and `a` + `b.t` write to the same key. The pull performs the
    round trip on every table name and every column name it is about to record,
    and a name that does not survive takes its whole table out with a warning.

    Performing it rather than validating against a list of forbidden characters:
    the format is what decides, the format changes, and a rule written here
    would be a second opinion that can fall out of step with it. The whole table
    goes, not the offending column, because a table missing one column is a
    table a plan would add it to.

<a id="decision-256"></a>

256. **The foreign keys are assembled in a second pass, after everything that
    could take their uniqueness away.** A foreign key is legal only against a
    unique index on the referenced table, and `conindid` says which one. That
    index may not reach the pull — its key constraint carries an `INCLUDE`
    payload, or is `NULLS NOT DISTINCT`, or is deferrable, or any of the other
    reasons 251 leaves an object out — and a key recorded against it describes a
    schema that cannot be built: adding the key back fails for want of a
    uniqueness nothing mentions.

    Whether it survived is not knowable from the constraint's own row, only from
    what the constraint and index arms did, so the arms report it: they return
    whether they recorded the object, and the foreign keys run afterwards
    against the set that did. Asking the arms rather than re-deriving the
    predicates, because a second copy of "which indexes this file refuses" is a
    second opinion that can fall out of step with the first.

    This is the third time in this file that a decision was reachable through a
    map built before the decision was made — 252's `confkey`, round 8's refused
    table, and this. The shape is in PITFALLS.

<a id="decision-257"></a>

257. **The pull's own SQL carries no backslash escape.** 254 pins the settings
    that decide how the engine *prints* an answer. This is the other direction:
    `standard_conforming_strings` decides how the engine *reads* the query's own
    string literals, and with it off a backslash in an ordinary literal is
    consumed — measured, with a warning nothing here reads. `'pg\_%'` becomes
    the pattern `pg_%`, its `_` becomes a wildcard, and a project's schema
    called `pga` disappears from the pull, which is a plan that creates tables
    that are already there.

    The setting is now pinned in the canonical scope, and no query depends on
    that having worked: the schema filter is `left(nspname, 3) <> 'pg_'`, and a
    test asserts that no query contains a backslash at all. A filter with no
    escape in it cannot be read two ways, which is worth more than a filter that
    is correct as long as a `SET` succeeded.

<a id="decision-258"></a>

258. **The declaration round trip asks for the same value, not for a value.**
    255 made the pull perform the round trip rather than reason about it, and
    the first version of it for a column's type asked the wrong question: does
    the spelling parse. Measured, `bit(3)` is a legal type this catalogue does
    not hold, so it is stored opaque — the base `bit(3)` with no arguments — and
    it writes out as `bit(3)` and parses back as the base `bit` with the
    argument `3`. That parses, and it is a different type: a schema written and
    reloaded is not the schema that was pulled, and wherever equality falls back
    to the raw spelling it is a difference no plan can act on.

    The check is now `render, parse, compare equal`, and it is asked of the
    value the column will actually be recorded with — one function decides that
    value for both the guard and the construction, because a check on something
    *like* what is stored is a check on nothing.

<a id="decision-305"></a>

305. **Extension-owned objects are left out of the pull silently, and that is
    not the "absent, empty and unreadable" failure.** `CREATE EXTENSION …
    SCHEMA app` puts an extension's functions and views in a project's schema.
    A reader without the `pg_depend deptype = 'e'` filter reports every one of
    them as an undeclared module, and the next plan offers to drop objects
    whose declaration lives in a `.sql` file the extension owns and this
    project does not have.

    Not reported as a limitation, unlike a materialized view: a limitation is
    something the *model* cannot hold, and these are somebody else's objects.
    `DROP EXTENSION` is how one goes away. Reporting them would put a line per
    extension object in front of every reader, which is how a report stops
    being read.

    **Amended: the limitation reader is a reader.** The filter was on the
    module queries and not on the unheld-module query, so an extension's
    materialized view or aggregate in a project schema came back as a
    limitation — which is worse than noise, because `managed_limitations`
    refuses every command for a limitation whose name is in the managed set. A
    rule the ordinary reader applies and the reader beside it does not is a
    rule with a hole in it, and the hole is on the path that refuses.

    **Amended again: the table readers are readers too** (issue #201). The
    filter reached the module queries and the unheld-module query and stopped
    there, so `CREATE EXTENSION … SCHEMA app` and `ALTER EXTENSION … ADD TABLE`
    — measured, both put an extension's table in a project's schema — left it
    pulled as an ordinary undeclared table. Under `unmanaged: error` the next
    command refuses a database nothing is wrong with, and no declaration can
    claim it back: its definition lives in the extension's own `.sql` file. The
    filter is now on the ordinary table reader, on the limitation reader beside
    it, and on the predicate that decides which relation a trigger is read
    with — a user's trigger on an extension's table is the user's, and it is
    named as unheld rather than pulled naming a table the schema does not have.
    The grants reader is deliberately not among them: a grant on an extension's
    object is a grant a role really holds, and hiding it would be *absent*
    reading as *empty*.

<a id="decision-385"></a>

385. **A schema the pull does not read is a schema a declaration may not
    name.** The rule was already there for a table and a module — a table
    declared in `information_schema` is created and then invisible — and a
    grant target reaches the same schemas by a shorter road, because
    `schema::x` names one directly and nothing else has to exist. Measured,
    `GRANT USAGE ON SCHEMA information_schema TO r` runs; the pull skips the
    schema, so the grant reads back as absent, the apply's own read-back
    refuses it for not having achieved its postcondition, and every plan after
    it proposes the same `GRANT` again.

    The three copies of the filter — the table check, the module check and now
    the grant check — became one function beside the SQL it mirrors
    (`catalog::a_projects_schema`), and the live test
    `the_schemas_the_reader_skips_are_the_ones_a_declaration_may_not_name`
    reads every schema the cluster has and requires the reader's answer and the
    validator's to agree about each. A list written in this repo would be the
    thing that drifts.

<a id="decision-423"></a>

423. **A transactional PostgreSQL catalog read has one statement snapshot
    and its managed recording is revalidated.** The caller remains READ
    COMMITTED so a closing read sees concurrent commits and its own DDL.
    A savepoint does not give separate catalog queries a common snapshot: a
    constraint committed after its query could be absent from a successful
    recording. All catalog relations now travel in one UNION ALL statement,
    as checked JSON row batches decoded into the existing catalog model.
    This is internal transport; declarations and saved plans do not change.

    Before bootstrap or transactional apply records the result, the CLI
    captures the managed schema and rows again. A changed managed projection
    refuses rather than recording an unstable view (SPEC §7.6). Unmanaged
    inventory is excluded under §8.2, and both captures reject unsupported
    managed facts. This is optimistic revalidation, not a lock on arbitrary
    DDL after the last observation. Owned reads retain their read-only framing;
    the shared framing also protects multi-statement row reads.

    A live two-connection regression commits a CHECK on an untouched managed
    table between captures and observes refusal, then commits unrelated
    unmanaged DDL and observes acceptance. It also proves the reader sees
    its own uncommitted table, preserves the caller transaction, and rolls
    that table back while the independent writer's constraint remains.

<a id="decision-424"></a>

424. **Read absent PostgreSQL constraint flags under their older semantics.**
    `conenforced` and `conperiod` arrived in PostgreSQL 18. Direct column
    references made every catalog-backed CLI command fail on PostgreSQL 16,
    including an otherwise valid bootstrap. Looking them up in the catalog
    row's JSON representation keeps one query across these versions: an
    absent enforcement flag means enforced, and an absent period flag means
    non-temporal. PostgreSQL 18 still supplies its actual flags, so unsupported
    NOT ENFORCED and temporal constraints remain refusals.

    The ordinary CLI deployment loop now also runs on the pinned PostgreSQL
    16 server: bootstrap, connected plan, apply, clean verify, and ledger
    entries. A manually added CHECK must then appear as drift on both
    versions. Removing the compatible lookup makes the older-server test
    fail at bootstrap. This measures that path, not every feature on every
    PostgreSQL release; the separate permission-version preflight remains
    deferred in #321.

<a id="decision-425"></a>

425. **An introspection limitation keeps its object's namespace.** A
    PostgreSQL aggregate called `app.t(bigint)` is not a defect of the table
    `app.t` or of the routine `app.t(integer)`. Storing every limitation under
    a `TableName` made connected plans and recorders refuse those valid
    managed objects. The connected catalog result now carries a relation
    target, a structured module identity, or an unnameable module diagnostic.
    Relations share the table/view namespace; routines retain argument types
    and triggers retain their parent. A declaration cannot name an unnameable
    identity, so that diagnostic is not assigned to another same-named object.

    Both PostgreSQL catalog exclusions and pure module assembly preserve the
    target, and the CLI matches it against the corresponding managed scope.
    SQL Server table limitations keep their relation targets and its existing
    unreadable-module inventory keeps its separate handling. No saved schema
    or plan format changes; the target belongs to the connected read result.

    The live regression keeps a managed table and routine beside an unmanaged
    aggregate overload: plan, snapshot, baseline and verify succeed. Making
    the table UNLOGGED still refuses, as does replacing the exact managed
    routine signature with an aggregate. Reverting to a name-only table
    filter restores the false refusal. The broader unmanaged module inventory
    work remains #303; this change scopes limitations rather than completing
    that policy integration.

<a id="decision-426"></a>

426. **An omitted PostgreSQL module is still in the unmanaged inventory.**
    Both the catalog filter (materialized views, aggregates, window functions,
    triggers on omitted relations) and the assembler (unrepresentable identity
    or deparsed definition) return an `UnmanagedModule` alongside the warning
    and limitation. Otherwise `unmanaged: error` treats the omitted object as
    absent (issue #303). Inventory targets retain the namespaces from 425:
    routine arguments, trigger parent, or the shared relation namespace; an
    unnameable identity never matches a declaration. The CLI uses those targets
    for unmanaged policy and unreadable-module matching. SQL Server keeps its
    existing shared-name behavior. This inventory does not assess whether a
    trigger is safe to execute; that separate policy is tracked in #324.
    Pinned by the live catalog omitted-module and trigger tests, the malformed
    deparse unit test, and `flow_pg::omitted_modules_obey_unmanaged_policy_even_beside_a_managed_overload`.

<a id="decision-427"></a>

427. **SQL Server's unreadable trigger still occupies a shared module name.**
    A trigger declaration carries its parent, but an encrypted catalog row
    cannot supply that parent. `SharedModule` therefore matches by object name
    across declaration kinds, preserving the SQL Server behavior before 426.
    PostgreSQL `Relation` remains distinct: a view named `audit` must not match
    the unrelated trigger `t.audit`. Conflating those namespaces either records
    a partial SQL Server schema or refuses an unrelated PostgreSQL object.
    Pinned by `a_declared_unreadable_trigger_is_never_recorded_or_planned_over`
    and the shared-module versus relation identity test (PR #325 review).

<a id="decision-443"></a>

443. **A column's collation is reported against the connected database's own
    default, not against every explicit `COLLATE`.**
    `sys.columns.collation_name` was read nowhere before this fix (#94): a
    column `COLLATE`d away from its declared type read back as the plain
    type, with no word, so a bootstrap onto a fresh database silently changed
    what every comparison, unique constraint and index seek on it meant. The
    model still holds no collation — modelling it ripples into the emitter,
    the differ and the risk classifier, a larger and separate issue — so the
    fix reports a `Limitation` naming the column, the shape already used for
    a clustered index and a computed column, rather than modelling it.

    The baseline is `DATABASEPROPERTYEX(DB_NAME(), 'Collation')`, the
    connected (source) database's own default — not "any explicit `COLLATE`"
    and not "every collated column." Measured on a live server: a character
    column declared with no `COLLATE` at all, and one declared with an
    explicit `COLLATE` that happens to repeat the database's own default, are
    byte-for-byte identical in `sys.columns.collation_name` — the catalog does
    not remember whether a matching collation was inherited or spelled out.
    "Any explicit `COLLATE`" as the baseline would flag the second column
    though the emitter reproduces it for free by writing none at all; "every
    collated column" would flag both, and every ordinary character column in
    the database besides, drowning the one that matters in noise.

    This closes only the column-level gap. A column that matches its *source*
    database's default still lands under whatever default the *target* a
    bootstrap runs against happens to have, and nothing here compares the two
    — that mismatch is #406, not this entry.

<a id="decision-482"></a>

482. **Omitted catalog objects have one managed scope and one fact per omission.**
     The CLI projects both inventories retained by 425–426 without changing
     either catalog collection. A typed limitation supersedes an inventory
     fallback only when both its target and detail match (#326). Another fact
     on the same target, or the same wording on another target, stays visible;
     SQL Server inventory-only unreadable modules retain their refusal.

     Both managed limitations and unmanaged policy use the declared table
     identities plus module identities (#327). An omitted relation replacing
     an identity-file table remains managed even though absent from the pulled
     schema. Routine signatures, trigger parents and SQL Server shared-module
     names retain their existing namespace rules. Every caller supplies the
     same ids used for its catalog scope, including verify and status's recorded
     identities. No saved artifact, declaration model or engine boundary changes.

     Unit controls preserve distinct facts and namespaces. PostgreSQL CLI tests
     replace a managed function with an aggregate and a table with a materialized
     view: verify and status report one managed omission under warn/error, while
     unrelated omitted relations and same-name routines remain unmanaged. Plan,
     baseline and snapshot still refuse the omitted managed state without adding
     a ledger entry; restoring the original objects restores clean verification.

<a id="decision-491"></a>

491. **The source-default collation reminder follows emitted character columns.**
     SQL Server's declarations do not carry the source database's default
     collation. A bootstrap target with another default may therefore give
     retained character columns different comparison semantics. Report the
     source default once through `Pulled::onboarding_notices`, shown during
     pull/init adoption, rather than attaching a managed-object limitation
     or a routine connected-read warning (#432). The source default is
     onboarding context, not evidence of drift: making it a limitation would
     refuse valid deployments even when the target has the same default.

     The reminder describes the emitted declarations, not every character
     column present in the source catalog (#434). Match collated raw columns
     by table object id and column name against the assembled, supported
     table/column inventory. Temporal/history tables, tables retaining only
     a period, computed columns, UDT columns and wholly omitted tables cannot
     trigger it by themselves. System and ledger columns are outside that
     inventory too. Their existing omission reports remain intact, as required
     by DECISIONS 14 and 17. A retained character column with a non-default
     collation still receives DECISIONS 443's separate column limitation and
     triggers the source-default reminder; a numeric-only declaration does not.

     This documents the reporting boundary, without modelling collation or
     comparing the source and target defaults. Unit cases cover active
     versioning, history, period-only tables and omitted columns with and
     without a retained numeric sibling. Live SQL Server fixtures measure
     character collations in omitted temporal/history, computed and alias
     columns, retain their limitation reports, and introduce a supported
     character column to require exactly one reminder. Existing matching and
     non-matching column controls preserve DECISIONS 443's baseline.

<a id="dec-902-1"></a>

**DEC-902.1. An adoption asks the validator what it may write, and leaves out
and names each object the validator refuses.** `pull` and `init --from` turned a
catalog read into files without asking `declaration_problems` (DECISIONS 141),
so wherever the reader and the validator disagreed the adoption wrote a project
its next command refused: an identity increment past its type's span (#504), a
table or grant in a schema named `$user` (#705), a trigger on a ledger table the
reader filters out (#891), a grant in a schema its role holds no `usage` on.
Teaching the reader each validator rule would keep the two in step only until
the next rule; asking the validator keeps them in step by construction.

Per object, not per project. `init --from` did ask, and refused the whole
adoption over one table in a schema nobody uses — honest, but it leaves the
operator nothing to start from. So each table, module or role the dialect's
`validate_*` refuses is taken out and reported where the adoption already
reports what it left behind (DECISIONS 14, 17): a table in the warnings, a
module in the unmanaged inventory, a grant among the unexpressible permissions.
A trigger whose table did not make it goes with the table. What no single
object carries — two names that collide, say — is still refused whole: the
files are staged outside the project, loaded back, and put through
`declaration_problems`, and a problem there means nothing is written.

A grant is judged by what removing it fixes, not by validating it alone. Some
rules are about a role's grants together — PostgreSQL's "a grant in a schema
needs `usage` on it" — and a grant checked in isolation would fail that rule
for want of a `usage` its role does hold. A grant is left out only if the role
has fewer problems without it; a role still refused with every grant gone is
left out whole.

The price is that a pull that used to "succeed" now reports omissions. That is
the point: the success was a project `validate` rejected.

<a id="dec-921-1"></a>

**DEC-921.1. A `--data` table that `validate` refuses is left out like any other
object, and its rows are not read (#921; extends DEC-902.1).** `pull --data`
validated the requested table before DEC-902.1's filter ran. So a table the
validator refuses stopped the whole pull when its rows were asked for, and was
merely left out when they were not. The same catalog got two answers depending
on a flag about rows.

The filter now runs before any rows are read. A requested table it leaves out is
named twice: once as left out, with the validator's reason, and once to say its
rows were not read. The other `--data` tables in the same command still get
their blocks.

A refusal was the other candidate, and it was rejected. The operator did ask for
those rows, but the table cannot be declared at all, so there is no `data:` block
it could carry, and a refusal would only send them to rerun without the flag.
What stays a refusal is a name the database does not have. That is a mistake on
the command line, not a fact about the catalog. The dialect check that follows
the row read stays too: by then it can only fail on the rows themselves.
