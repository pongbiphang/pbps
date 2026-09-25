# Identity and naming

How objects are identified across revisions: uids, the identity files, rename
intents, and the rules a declared name must meet. Part of the [decision
record](../DECISIONS.md), which says how to add an entry here.

<a id="decision-1"></a>

1. **Comparison matches by uid via two-sided identity files**, not "name + this
   revision's intent" — the latter breaks on jump-version deploys.

<a id="decision-2"></a>

2. **Drops require a reason**: the tombstone must answer an audit's "why".

<a id="decision-3"></a>

3. **Intents are idempotent**: a stale `renamed_from` is a no-op, not an error.

<a id="decision-8"></a>

8. **`validate` rejects one name mapping to multiple uids** — same-name adds
   on two branches auto-merge silently in the ids file.

<a id="decision-16"></a>

16. **`strategy:` is persistent, unlike `renamed_from`** — it lives beside the
    model (`Loaded.hints`, never in `Schema`, or constraint 1 breaks) and `fmt`
    preserves it. Unknown keys are rejected: a typo that became a no-op would
    leave the user believing a large table is altered online when it is not
    (ADR-0003).

<a id="decision-81"></a>

81. **`validate --since` compares the declarations at the revision, not only
    the identities.** Identity alone saw renames and new objects; a table
    that gained an index, changed a type or grew its `data:` block kept its
    uid and its name and was skipped — the one object the revision touched.
    The declarations at the revision are checked out of git into a scratch
    directory and loaded with the ordinary loader, and a table or role whose
    declaration differs (matched by uid, so a rename does not hide a change
    behind it) is evaluated too. Modules stay always-evaluated (ADR-0008
    implementation item 4).

<a id="decision-113"></a>

113. **A revision `--since` or `--base` cannot resolve is refused, not read
    as an empty history.** One case is not an error: `HEAD` in a repository
    with no commits, which genuinely has no previous version. Every other
    unresolvable revision is a name somebody got wrong, and the empty
    baseline is the loudest possible wrong answer to it — `plan` proposes
    creating the entire schema, and `--since` marks every object changed, so
    a gradual-adoption policy fails declarations nobody has touched. Both
    `load_from_git` and `schema_at` swallowed every `rev-parse` failure
    alike; `schema_at`'s own comment already described the distinction the
    code did not make.

<a id="decision-135"></a>

135. **Two declarations whose files differ only in case are refused before
    either is written.** A case-sensitive database holds `Reader` beside
    `reader`, and their encoded filenames differ only in case; on a
    case-insensitive filesystem `pull` wrote the second over the first, with
    the identity file still naming both — declarations that cannot round-trip
    to the database, and nothing said. Encoding the case away would fix the
    write and lose the readable name that makes these files reviewable, so
    the collision is detected instead, over every declaration a `pull` or an
    `init` is about to write, before the first one is.

    The refusal is unconditional rather than a property of the filesystem
    underneath: a declaration is written into git and has to resolve to the
    same file on every platform that checks the repository out. This is the
    file-level half of the shape 123 records at the database level.

<a id="decision-141"></a>

141. **Every command that reads declarations asks the same questions of them.**
    `validate` checked the dialect, the name collisions, the grant targets and
    the rows; `init` checked the first two of those against the project it had
    just staged; and the two commands that hand statements to a database that
    is not a rehearsal — `plan --db` and `bootstrap` — checked none of them. A
    role granting `execute` on a table therefore reached an applyable staged
    plan, and its `GRANT`, which the ordering puts after every table, row and
    module statement, would fail on a database those statements had already
    changed. Three enumerations of one list is a shape where the shortest of
    them is always the one nobody notices, so there is now one:
    `declaration_problems` returns `(finding id, message)` pairs, `validate`
    renders them as findings, and the other three refuse. What each command
    varies is what it does with the answer, never which questions it asks.
    Offline `plan` is deliberately not among them: its output is a
    `PlanOrigin::Preview` that `apply` refuses outright, and its companion for
    "are these valid" is `validate`.

<a id="decision-201"></a>

201. **Namespace sharing and overloading are dialect questions, asked of the
    dialect.** `check_names` refused two modules with one name and a module
    sharing a table's name, as one rule for every engine. Both halves are
    engine-specific — on PostgreSQL views share the table namespace and
    routines do not, and routines overload — and `pbps-model` may not know
    which engine it is describing. So the rule moves to
    `pbps_dialect::check_module_names`, over `shares_namespace_with_tables`
    and `overloads`; `check_names` keeps only what is true of every engine.
    MSSQL answers as before, so no declaration that was valid becomes invalid.

<a id="decision-230"></a>

230. **Identifier rules are the engine's, measured, and neither is inherited
    from the SQL Server side.** Two of them, both silent when wrong, both
    pinned by the live suite against PostgreSQL 18.6:

    - **Case folding is ASCII-only.** `CREATE TABLE AÄ` makes the relation
      `aÄ`, not `aä` — the server downcases byte by byte and leaves the high
      bit alone. Rust's `to_lowercase` is Unicode-aware and folded one
      character too many, so a declaration would have been keyed as a name
      introspection never returns: drift no apply can settle, and a `CREATE`
      that makes an object under a name nobody asked for.
    - **The length limit is 63 *bytes*, and it is enforced here because the
      server does not enforce it.** It truncates and says so in a `NOTICE`
      nothing reads. Measured: a longer name records itself at one length and
      reads back at another, and two names differing only after byte 63 collide
      — the second `CREATE TABLE` fails with `relation … already exists`,
      naming a table the declarations do not contain. The SQL Server
      counterpart counts **characters** (128), so copying its shape would have
      been wrong in both directions: 32 `ä` is 32 characters and 64 bytes.
      been wrong in both directions: 32 `ä` is 32 characters and 64 bytes.

    **Amended: the limit holds for the names a routine argument spells.** The
    module gate refused a routine's own name over 63 bytes and let a type
    name in its argument list through. Measured, with a type `dq.t…t` of 63
    bytes, `CREATE FUNCTION dq.f(a dq.t…tx)` spelling one byte more is
    accepted with a `NOTICE`, the routine is identified as `dq.f(dq.t…t)`,
    and the same statement run again is refused as already existing. The
    declared key is the untruncated spelling, so `module_oid` finds nothing
    under it, the routine is planned as absent on every plan, and the
    `CREATE` it emits is the one the engine refuses — a plan applied once and
    refused ever after. `validate_module` refuses an argument holding a name
    over the limit, quoted or bare, before anything connects.

<a id="decision-246"></a>

246. **Two rename intents claiming one name are refused, before anything else
    is judged.** The three matching loops in `identity.rs` consume from
    `disappeared` and `appeared`, and nothing checked that two intents named
    one column, table or role. The first to be reached took it; the rest fell
    through.

    The source case was **silent**. Measured: with `renamed_from: old` on both
    `aaa` and `zzz`, `resolve` returned `Ok`, the column holding the data was
    renamed to `aaa`, and `zzz` was created as a new empty column beside it —
    so the data answered to a name the author did not choose and the name they
    expected held nothing. The second intent went unreported because its target
    had by then been minted into the ids file, which is what the absorbed check
    reads. Which of the two won was declaration order.

    The target case did reach `Err`, but as `UnusedIntent` — "matches nothing
    in either the declarations or the identity file, likely a typo" — about an
    intent whose every name exists. A message that sends the reader looking for
    a misspelling there is none of is worse than the count of blockers
    suggests.

    **One guard for all three kinds, not one per resolver.** The loops are the
    same code over three types, and a rule with three homes is three chances to
    be fixed once (`docs/PITFALLS.md`, "One rule, spelled in three places").
    The key carries the kind, and a column's carries its table, so a table and
    a role that share a name — or one column name in two tables — stay two
    claims rather than becoming one.

    **Only claims that could match here.** The guard sees the rename intents
    whose source is in `disappeared` and whose target is in `appeared`, and
    that filter is the whole difference between a conflict and a leftover. Its
    first form grouped every raw intent and was measured wrong on this: a
    `renamed_from` lives on until `pbps fmt` strips it — which is what
    `intent_is_absorbed` exists for — so an annotation recording a rename that
    already happened is *expected* to be in the file, and once its vacated
    source name has been reused by a new object that this revision renames, the
    two share a source and nothing else. The stale one cannot match, because
    its target is on both sides of the declarations. Grouping them refused a
    valid plan for an annotation nobody had got around to deleting, which is
    the failure this repo weighs heaviest.

    It runs before its scope's matching loop, raises what it finds, and the
    loop then runs anyway. Skipping it — the shape `AmbiguousColumns` uses —
    was the obvious move and the wrong one twice over: the loop's
    order-dependent decision goes nowhere in any case, because `resolve`
    discards the whole `Resolution` when it returns `Err`, while skipping it
    leaves every *other* intent of that kind unmatched, so an unrelated and
    perfectly matchable rename or drop beside the contest was reported as
    "matches nothing … likely a typo".

    **The contenders are excluded from that sweep by equality, not by
    bookkeeping.** Marking their indexes used was the first answer and it was
    defeated twice — once by the early return that stranded their neighbours,
    once by the repeat-collapsing that dropped an index before it was recorded.
    Three rounds, three ways for an index to go missing, one shape: an intent
    can be reported as contested *and* as a likely typo. The sweep now skips
    every intent a `ConflictingRenameIntents` names, so the two are mutually
    exclusive by construction and there is no fourth way. A property held by a
    check beats one held by four call sites remembering to record something.

    **Identical intents are not a conflict.** The same rename reaches `resolve`
    twice whenever a `renamed_from` annotation is also answered at the
    interactive prompt, and refusing that would refuse a valid plan for saying
    one true thing twice. Only distinct intents contending for a name are
    ambiguous.

    **And it offers no choice.** The prompt's candidates would be exactly the
    intents the user has already written down, so it would ask them to pick one
    of their own contradictory statements and then record it — turning a caught
    mistake into a committed one. The file is where the contradiction lives and
    where it has to be resolved.

    A chain (`a -> b` beside `b -> c`) claims no name twice on either side and
    is not this guard's business; it is already refused, because `b` is in both
    the declarations and the ids file and so is in neither `appeared` nor
    `disappeared`. A test pins that, so nobody widens the guard onto a case
    that is covered.

<a id="decision-273"></a>

273. **A table declared in a schema the pull never reads is refused offline.**
    The reader skips `pg_catalog`, `information_schema` and every schema whose
    name begins with `pg_` (`catalog.rs`), so a table declared in one is
    created and then invisible: absent from the pulled schema, planned again as
    a `CREATE` the engine refuses for already existing. The rule is derived
    from the reader's own list rather than written beside it, which is why it
    lives in this dialect and not in the loader — the excluded set is this
    engine's.

    **`pg_temp` is why the rule is not only about visibility**, and it is what
    makes this the class DECISIONS 266 wrote an offline rule for rather than
    one to leave to the engine. Measured, `pg_temp` is the parser's alias for
    the session's temporary schema:

    ```text
    CREATE TABLE "pg_temp"."t" (id integer);       accepted
    the relation afterwards:  pg_temp_58.t, relpersistence = 't'
    ```

    A session-local table, under a name the declaration never wrote, gone when
    the connection closes. An engine that refuses by name can be left to refuse
    — that is the line #175 and #179 are answered on — and one that hands back
    something else cannot.

    The negative half is in the test and is the reason the check is `starts_with
    ("pg_")` and not a looser match: the reader compares the first three
    characters, so a project's schema called `pga` is a project's schema, and a
    validation that refused it would refuse a declaration the pull reads
    perfectly well (the same trap `catalog.rs` avoids by not writing the filter
    as a `LIKE` pattern, DECISIONS 254).

    **Amended: modules are refused by the same rule.** The module gate checked
    only that the names could be quoted. Measured, `CREATE FUNCTION
    information_schema.f()` is accepted and identified as
    `information_schema.f()`, and `CREATE VIEW pg_temp.v` leaves `pg_temp_4.v`
    with `relpersistence = 't'` — the same two failures one namespace over, and
    `validate_module` now says so offline. A trigger is keyed by the table it
    is on, so the table's schema is the one the rule reads. (`pg_catalog`
    itself the engine refuses — "system catalog modifications are currently
    disallowed" — and the gate refuses it a statement earlier, which is where
    the offline command exists to speak.)

<a id="decision-274"></a>

274. **The two table names this tool owns are refused in every schema.** The
    reader hides `__pbps_state` and `__pbps_lock` wherever they appear
    (`catalog.rs`), so a declaration using one is created and then invisible:
    the pull reports it absent, the next plan creates it again, and the engine
    refuses that for already existing. The apply reported success and the
    recording says the table is as declared — the shape DECISIONS 273 refused a
    schema for, now for a name.

    **By name and never by prefix**, which is the reader's own hard-won
    narrowing; its comment records the bug, that `NOT LIKE '\_\_pbps\_%'` also hid
    a project's `app.__pbps_customers` and nothing refused *that* declaration
    either. So the validation is derived from the same list, `catalog::OURS`,
    and `the_filter_hides_exactly_the_names_the_validation_refuses` ties them
    together in both directions: a name the validation refuses that the filter
    does not hide is a false refusal, and a name the filter hides that the
    validation does not refuse is this bug again.

    SPEC §8.1 defines the two, and a step that adds a third adds it to `OURS`,
    where the filter and the validation both read it.

<a id="decision-444"></a>

444. **A name containing pbps's own `.` separator is refused where it enters,
    not escaped in the serialized form.**
    `TableName`, `ColumnRef` and `ModuleId` join their parts with `.` and
    parse back by splitting on it, so a schema, table, column, or module
    (view/routine/trigger) part that itself contains a `.` cannot round-trip:
    a column literally named `a.b` on `dbo.customer` serializes as
    `dbo.customer.a.b`, indistinguishable from a mistyped five-part name once
    written, and a view literally named `a.b` serializes as `schema.a.b` —
    three dotted parts, which `ModuleId::from_str`'s own shape rule reads back
    as a *trigger*, not a refused view (#108). Escaping the separator in the
    serialized form was considered and rejected: the `.`-joined spelling is
    already an on-disk format ledger rows hold, so introducing an escape would
    change what every existing row means, for the sake of a name nobody
    actually wants to keep.

    The check (`pbps_model::check_segment`) lives at the places a part is
    built from something that never passed through this crate's own
    `FromStr`: `pbps-load::convert`'s column loop for a declared column, and
    `pbps-diff::identity::resolve` for a table, a column, or a module —
    tables and columns at fresh-identity minting, modules in a dedicated
    `resolve_modules` pass that re-validates every declared module on every
    call, since a module carries no persisted identity to mint or compare
    against (ADR-0002, DECISIONS 200). The choke point is a departure from the
    issue's own suggestion, which named each dialect's introspection as the
    seam for `pull`. `resolve` is instead the single point both `pbps pull`
    and `pbps init --from` pass through to mint an identity — or, for a
    module, simply accept the name — for whatever a dialect's introspection
    handed back, so the check lives there: it catches every name family
    `pull` can hand back without adding a line to either dialect's
    introspection module. A routine's own argument *types* are deliberately
    excluded from the module check: `RoutineArg` legitimately holds a `.` for
    a schema-qualified type (`dl.money_type`), stores it as opaque text, and
    never splits it apart the way a name is split — checking it would refuse
    a routine that already round-trips correctly, the false-refusal direction
    this decision exists to avoid, not produce.

    This does not rescue a project whose `ids.yaml` already holds a dotted
    table or column name: such a string already fails `TableName`/`ColumnRef`'s
    `FromStr` on the very next read (the wrong segment count), so that
    project's identity file is already unloadable today, independent of this
    change, and repairing existing corruption is out of scope. What does
    change: a not-yet-planned declaration with a dotted column key now fails
    at load instead of loading silently and bricking `ids.yaml` the first
    time `plan` minted its identity — a deliberate fail-fast.

<a id="decision-453"></a>

453. **An index — and the index behind a named primary key or unique
    constraint — is a third case of 201's rule, not a new one.** `check_names`
    already moved namespace-sharing questions to the dialect because both
    halves are engine-specific; an index is the same shape a third time.
    Measured on PostgreSQL 18.6 (issue #176): `CREATE INDEX ix_n` on two
    tables in one schema, an index named after a table, a named unique
    constraint sharing a name with an index, and a unique constraint named
    after a table are all refused with `relation "..." already exists` — an
    index lives in `pg_class` beside tables and views, and a named primary key
    or unique constraint is backed by an index of that name. On SQL Server an
    index name only has to be unique per table, so the same declarations are
    valid there, which is why `Dialect::indexes_share_namespace_with_tables`
    defaults to `false` and only PostgreSQL overrides it. A check or
    foreign-key constraint has no backing index and stays out of the
    question — measured beside the collisions above, `ALTER TABLE ... ADD
    CONSTRAINT t2 CHECK (n > 0)` is accepted where the table is named `t2` —
    those are per-table and are issue #179's subject.

    This is not in tension with 452's server-dependent limits: a name
    collision in `pg_class` is deterministic and knowable from the
    declaration alone, the same way 452's own structural checks are, while
    index width and key-type eligibility depend on the server's build and
    installed operator classes, which is exactly why 452 leaves those to the
    engine instead of guessing them offline.

    `pbps_dialect::check_index_names` seeds the namespace with every table and
    every module kind `shares_namespace_with_tables` already says belongs
    there (a view, on PostgreSQL), then folds in each table's declared index,
    primary-key and unique-constraint names, checking each against everything
    already claimed. It is called from `declaration_problems` beside
    `check_module_names`, so `validate`, `init`, `plan --db` and `bootstrap`
    all ask it (141).

<a id="decision-459"></a>

459. **Declared constraint kinds share a table-local name check (issue #179).**
     Primary-key, unique, foreign-key and check declarations occupy separate
     model fields, so their maps cannot refuse the same explicit name used by
     two kinds. `Table::constraint_name_conflicts` compares those names once;
     both dialect validators consume it through the existing declaration
     gate. It reports every later claimant against the first, naming both
     kinds. An unnamed primary key contributes no guessed generated name.
     This changes no model serialization, identity, diff or emission behavior.

     Measured on PostgreSQL 18.6 and SQL Server 2025, CHECK and FK sharing a
     name on one table are refused, while a CHECK and an ordinary index of
     that name are accepted. Indexes therefore remain outside this check;
     PostgreSQL's schema relation/index check from decision 453 still handles
     index and named key-backing-index collisions. Generated PostgreSQL names
     remain the separate issue #465.

<a id="decision-506"></a>

506. **A foreign key's two column lists are two different rules, and its width
     is neither of them (issues #475, #476).** 452 settled the shape: a rule of
     this engine is refused offline; a limit of this *build* is left to the
     server. Two reviews then asked for the two halves of one foreign key, and
     the answers differ.

     **The referenced list is refused.** Measured on PostgreSQL 18.6,
     `FOREIGN KEY (a, b) REFERENCES p (x, x)` is `42830: foreign key
     referenced-columns list must not contain duplicates`, and it is refused
     even where a unique index really does repeat that key — `CREATE UNIQUE
     INDEX … (x, x)` is itself legal here — so this is an analysis rule of the
     engine rather than a missing-key error. It is reported once per repeated
     name rather than once per repetition: `(x, x, x)` is one mistake and one
     remedy, while two different repeated names are two mistakes and are each
     named.

     **The local list is not**, and that is the same measurement 452 already
     recorded: `FOREIGN KEY (a, a) REFERENCES p (x, y)` against a composite
     unique key is **accepted**. Refusing it would reject a valid declaration —
     the one direction these checks may not fail in. Two lists, two rules, and
     the tests hold the pair side by side so neither can be "tidied" into the
     other.

     **The width is the server's.** #475 read the validator as checking index
     widths but not foreign-key widths, and the premise is wrong: neither is
     checked here, deliberately (452). Measured on 18.6, `SHOW max_index_keys`
     = 32, a 33-column index is `54011: cannot use more than 32 columns in an
     index` and a 33-column foreign key is `54011: cannot have more than 32 keys
     in a foreign key` — the same number, the same SQLSTATE, and the same
     compile-time constant behind both. An offline refusal at 33 would reject a
     valid declaration on a server built with a larger `INDEX_MAX_KEYS`, which
     is exactly what 452 wrote the rule to avoid. SQL Server's validator carries
     a width rule because there 32 is a product invariant, not a build setting;
     the asymmetry the review read as a gap is the decision. #475 is closed as
     wrong rather than deferred, because a `deferred-review` issue would record
     a fix that must not be made.

     One thing the width measurement adds, and the live test now holds: the
     refusal comes **before** the referenced-key lookup. A 33-column foreign key
     against a parent with no matching unique constraint at all — and on this
     build there cannot be one — is still the width error rather than a missing
     -key one, so the boundary can be measured without the referenced side
     confounding it.

     SQL Server refuses duplicates on **both** lists with one message
     (`Msg 8136`, measured on 17.0.4075.5), and its validator already catches
     the local half. The referenced half is issue #667, not scope here.

<a id="decision-507"></a>

507. **A named primary key and a unique constraint of one table sharing a name
     is the table-local rule's to report, not the relation namespace's
     (issue #498).** Two checks saw the same defect. `Table::constraint_name_conflicts`
     compares a table's primary key, unique, foreign-key and check names against
     each other; `check_index_names` folds every declared index, named primary
     key and unique constraint into the schema's relation namespace, because
     PostgreSQL keeps tables, views and indexes in one (453). Where both a
     primary key and a unique constraint are named `shared`, both fired: one
     `dialect.rejected` and one `schema.name-collision`, on `validate` and again
     on the connected planning and bootstrap gates that run the same list (141).
     The declaration was correctly refused and the operator was handed the same
     mistake twice.

     **The table-local rule keeps it**, on two grounds. It is the narrower
     statement — "constraint names must be distinct within a table" — and it is
     engine-independent: it holds on SQL Server, where `check_index_names`
     returns nothing at all because indexes there have no shared namespace. A
     deduplication in the CLI gate would have had to match prose to find the
     pair; skipping it where the namespace check can see that both claims are
     named key constraints *of the same table* is the same decision made where
     the facts are.

     Nothing else moves, and the tests pin each boundary rather than the
     implementation: an index against a key constraint on one table, a key
     constraint against a table or a view, and the same two key constraints on
     two *different* tables are all still the namespace check's — the last
     because no table-local rule can see across tables — while a check or
     foreign key sharing a name was never in that namespace and stays the
     table-local rule's alone (ADR-0009 §1, 459).

<a id="dec-403-1"></a>

**DEC-403.1. An intent's provenance is either a declaration's annotation or a
current decision, and `resolve` hands every intent in as a current decision
(#403).** `resolve_with_provenance` answers several questions by provenance:
the absorption sweep, and the occupied-target guards for tables and columns. An
annotation of a rename that already happened is expected to sit in a file until
`fmt` removes it, so it may be quiet. A command or a prompt answer this run is
a decision, so one that changed nothing is an `UnusedIntent`, and one that names
an occupied target is refused. `resolve` passed `annotation_count: None`, an
unknown provenance. Every gate read `None` as "not a current decision" and "not
a stale annotation", which is the lenient answer twice. So a library caller
that handed `resolve` an explicit `RenameColumn { from: a, to: b }`, with both
names still declared and identified, got `Ok` and nothing recorded. Before #350
it had got `UnusedIntent`.

The convention is settled in one place, not gate by gate (DECISIONS 246, and
PITFALLS' "one rule, spelled in three places"). `resolve` is
`resolve_with_provenance(…, 0)`: no annotations, so every intent is a current
decision. `annotation_count` is a plain `usize`, so no gate can be handed an
unknown provenance to read its own way. Callers whose intents are annotations
say how many through `resolve_with_annotations`, as `plan`, `validate` and
`deploy` already do.

No command changes behaviour. `pull`, `init` and the dialect helpers pass no
intents at all, so no gate has anything to judge. The tests that described
annotation cases through `resolve` now say so through
`resolve_with_annotations`, so they pin the provenance they mean, not the
lenient default. Pinned by
`a_rename_handed_to_resolve_is_a_current_decision_and_an_annotation_is_not`.

<a id="dec-496-1"></a>

**DEC-496.1. On SQL Server a declared constraint name must be free in its
schema, not only in its table (#496; the fourth case of the namespace rule
DECISIONS 201 and 453 moved to the dialect).** Every primary key, unique,
check, foreign-key and default constraint is a row in `sys.objects`, whose
names are unique per schema together with tables, views, routines and triggers.
Measured on 17.0.4075.5: two tables in one schema declaring a check `c`, a
foreign key named like another table's check, and a constraint named like a
table, view, procedure, function or trigger are each refused (Msg 2714, then
1750). An index of that name, or the same check in another schema, is
accepted. PostgreSQL 18.6 keeps a check or foreign-key name per table, and puts
only the index behind a key in its relation namespace (453).

So `Dialect::constraints_share_namespace_with_tables`, `true` by default as
SQL Server's answer and `false` on PostgreSQL, gates
`check_constraint_names`, which runs beside `check_index_names` in the one list
every command asks (DECISIONS 141). Two constraints of one table are left to
`Table::constraint_name_conflicts`, so one defect is reported once (#498).
Names are compared exactly, because validation is offline and cannot know the
collation. A pair differing only in case is refused by a case-insensitive
engine loudly, before anything else of the statement runs, and is a legal pair
on a case-sensitive one, both pinned live.

The same namespace orders a plan. A table renamed onto a name that another
table's dropped check, foreign key, unique or primary key releases has to wait
for that drop, as it waits for a freed index on PostgreSQL (DECISIONS 496).
Measured on 17.0.4075.5: `sp_rename 'dbo.old', 'c'` is refused (Msg 15335)
while a check `c` exists, and succeeds after it is dropped. `rename_order` now
takes the dialect's two answers: an index releases a name only where indexes
share the namespace, and a check or foreign key only where constraints do.

The default constraints the emitter names itself are in that namespace too
(#969), and `DF_{table}_{column}` did not say where the table ended:
`dbo.a_b.c` and `dbo.a.b_c` both spelled `DF_a_b_c`, and the second
`CREATE TABLE` was refused. The emitter sees one change at a time, so it cannot
keep a short name only where nothing collides. So the name is now
`DF_pbps_{table}_{column}` when the table's name has no underscore, which makes
the first `_` after the prefix the boundary. Otherwise it is
`DF_pbps_{table}_{column}_{digest}`, with the digest of the qualified column
that a name too long to fit already carried. `pbps_` marks the name as this
tool's, apart from the `DF_<table>_<column>` many teams write by hand. No
environment had been deployed under the old shape, and nothing reads a
generated name back: a drop looks the name up in the catalog. The generated
names are deliberately **not** folded into `check_constraint_names`.
Validation is offline, and cannot tell a column whose default `pbps` will
create from an adopted one whose default already has another name. Claiming
the would-be name for every defaulted column refused a valid database that
held a constraint of that spelling (review of #969). What is left is a declared
constraint spelled `DF_pbps_…` beside a default `pbps` does create, which the
engine refuses at apply; `pbps_` is what makes it unlikely.
A short name never ends like a digested one (`_` and sixteen hex digits, in
either case, since a case-insensitive database folds them). A name that would
is digested as well, so that `dbo.a.b_c_<the digest of dbo.a_b.c>` cannot
spell the digested name of `dbo.a_b.c`.
