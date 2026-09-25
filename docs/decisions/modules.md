# Modules

Views, routines and triggers: identity, rebuilds, binding and ordering. Part of
the [decision record](../DECISIONS.md), which says how to add an entry here.

<a id="decision-28"></a>

28. **Modules are matched by name and have no uid** (constraint 7). The managed
    set for them is therefore supplied per command: `verify` passes the
    recorded state's modules, the commands that record a state pass the
    declared ones, and `apply` passes the recorded set plus what its own plan
    creates — which is what keeps it applyable with no checkout.

<a id="decision-29"></a>

29. **The emitter adds no terminator to a module.** The engine stores what was
    sent, introspection reads it back, and a semicolon the declaration did not
    have would come back inside the body and read as a change on every plan.

<a id="decision-30"></a>

30. **`introspect::split_module` may refuse.** It reads exactly as far as
    `emit::module_definition` writes; a view with `WITH SCHEMABINDING` has
    nowhere to keep its options, so the module is inventoried as unmanaged
    rather than recreated later without them.

<a id="decision-164"></a>

164. **A module leaves the managed set when its `DROP` runs, not when the plan
    is written — and an empty new parent is an answer.**
    **The scope.** `modules_after` built the set the *finished* plan leaves,
    and a staged run used it at every checkpoint. So a module whose `DROP` had
    not happened yet was already outside the managed set: absent from each
    checkpoint's schema, and invisible to `--resume`, which scopes the live
    side the same way and therefore compared two states that both omitted it.
    A pause, a hand edit to that module, and the remaining `DROP` ran against
    an object nobody had looked at since the plan was approved.
    The function now takes `Settled` — the same one the movement guard uses,
    because it is the same question — and only removes a dropped module under
    `Whole`. Keeping it mid-run needs no knowledge of which statements have
    run: one already dropped is simply absent from the catalog, which the read
    records truthfully; one still standing stays watched.
    One function with a required argument rather than two functions, on the
    reasoning the `Deployment` struct records: five call sites choose between
    these two answers, and a choice you have to write down is one you cannot
    make by not thinking about it.
    **The probe.** `rows_after` returned `None` for a table this plan creates
    that declares no rows, and `AddForeignKey` read that as "no question" and
    emitted no probe at all. But an empty parent is a very definite answer:
    *every* non-NULL reference on the child side is an orphan. Measured, the
    empty typed relation asks it — `SELECT TRY_CONVERT(varchar(10), NULL) AS
    k0 WHERE 1 = 0` counts 2 of 3 rows, the NULL exempt as the rule says. Under
    `apply --staged` this is the difference between a refusal before statement
    one and a table creation plus every row change committing before the
    constraint fails.
    Absent, empty and unreadable are three different things: this is the third
    time in this phase that "no rows to compare" was returned where "no rows"
    was the finding.

<a id="decision-200"></a>

200. **A module is identified by a typed `ModuleId`, and which fields carry
    that identity depends on the kind.** `Schema::modules` was keyed by
    `ObjectName`, which says every module in a schema has a distinct name.
    Measured on PostgreSQL, that is false twice over: functions and procedures
    overload, so `app.f(integer)` and `app.f(text)` are two objects with one
    name; and a trigger's name is unique only within its table, so `audit` on
    `orders` and `audit` on `customers` are two objects with one name in one
    schema. `ModuleId` is therefore an enum — `Named(ObjectName)` for a view,
    `Routine { name, args }`, `Trigger { on, name }` — and the shape of the
    key is the shape of the identity.

    **The signature is a `Vec<ColumnType>`, not a string.** Two semantically
    identical schemas must be `==` (the inviolable constraint), and
    `f(int)`/`f(integer)`/`f( INTEGER )` are one signature spelled three ways.
    A string key would have made three modules of one. The `Display`/`FromStr`
    pair exists for the JSON map key and for messages, and parses back to the
    same value; it is not the identity.

    **The trigger's table lives in the key, not in `Module`.** `Module::on` is
    removed. Containers hold names, elements do not: with the table in both
    places a snapshot could say `app.audit` is on `app.orders` in the key and
    on `app.customers` in the value, and nothing in the type would stop it.
    Removing the field makes that unrepresentable rather than checked. The
    declaration file keeps both lines — `trigger:` and `on:` — because that is
    where a human writes them; the loader folds them into one key and refuses
    an `on:` on a kind that has no table, or a trigger whose schema disagrees
    with its table's.

<a id="decision-202"></a>

202. **Routine identity is normalized by its own hook, and a collision is
    reported rather than merged.** PostgreSQL discards type modifiers when
    identifying a routine — measured, `f(varchar(10))` and `f(varchar(20))`
    are one function — which `normalize_type` must not do, because a column's
    modifier is part of the column. `normalize_routine_arg` is therefore a
    separate hook, applied to the loaded schema in one CLI pass after loading.
    When two declarations normalize to one id, the pass reports both and the
    command bails. Silently keeping the second would have left the first a
    declared module that no plan ever mentions — absent and unreadable are not
    the same, and only one of them is good news.

<a id="decision-204"></a>

204. **A guarantee the map key used to give is now a check, because removing
    the reason for one is not replacing it.** `Schema::modules` keyed by
    `ObjectName` made two modules with one name *unrepresentable*: the map
    held one entry per name and that was the end of it. Keyed by `ModuleId`
    they are representable — a trigger is told apart by its table, a routine
    by its signature — and on an engine that keeps every kind in one namespace
    per schema, `app.orders.audit` beside `app.customers.audit` is two objects
    it cannot both have. Nothing caught that: `check_module_names` compared
    each module against the *tables* and never against the other modules,
    because under the old key there was nothing to compare. `validate` passed,
    and the refusal arrived from the engine partway through a staged apply.

    So the check now groups the kinds the dialect keeps beside tables and
    refuses a repeated `object_name`, naming both identities — "one of these
    is wrong" is not a finding anyone can act on. The kinds with namespaces of
    their own are left alone, because there `ModuleId` *is* the whole identity
    and two keys are two objects. This is the guard-whose-reason-has-gone rule
    turned on the change that removed the reason: the key was the guard, and
    it had to be replaced in the same breath it was taken away.

<a id="decision-205"></a>

205. **A module whose name the id's string form cannot carry is inventoried,
    not recorded.** `ModuleId` crosses a snapshot, a plan and every message as
    a string in which the punctuation is structural: `.` separates the parts
    and `(` opens a signature. A legal quoted identifier may contain either —
    `[audit.v1]`, `[sales(archive)]` — and such an id would be written
    faithfully and read back as a *different* module: a trigger on
    `dbo.audit`, a routine with an argument named `archive`. Under the
    `ObjectName` key the same read failed loudly, because `dbo.audit.v1` was
    no shape a name could take; the typed key gave every such string a
    meaning, and so turned a loud failure into a quiet one. The check sits in
    introspection — the one place engine names enter the model — and asks the
    round trip itself, `id.to_string().parse() == id`, rather than listing
    forbidden characters, so it stays right if the string form changes. The
    module is inventoried with the reason, as every other shape the format
    cannot carry is, because a quiet refusal at the snapshot would leave the
    next plan proposing its destruction. Not in `ObjectName::new`: engine
    names arrive there for tables too, whose string form has the same
    property and is out of this change's scope. The same check guards a
    grant target read back from the catalog: `GrantTarget::Object` on
    `[dbo].[sales(archive)]` would be read back as a grant on a routine, so
    the permission is reported as unexpressible. The structured target is
    kept on the report — only its string form is ambiguous — because the
    target is what scopes an unexpressible permission to the managed set; a
    first version dropped it, and a grant on an *unmanaged* object of such a
    name was then reported, and refused, where an ordinary grant on the same
    object is ignored.

<a id="decision-209"></a>

209. **A binding is the dialect's to record, and SQL Server records none.**
    The candidate set of ADR-0013 §3 is a property of a search path, and SQL
    Server has none: an unqualified name binds to the *caller's* default
    schema when the statement runs, a property of the session and not of the
    object, so there is no set to record at creation that the catalog could
    later be asked about. Recording a guess — the identifier scan's qualified
    references, say — would give the field content that means nothing and
    could not move. The shape is in the format now, with a round-trip test,
    so the PostgreSQL crate adds the recording and the rebuild decision and
    changes no format.

<a id="decision-212"></a>

212. **Among the modules that share a routine's name, only `depends_on:`
    orders.** The identifier scan of ADR-0002 finds a reference by qualified
    name, and where a kind overloads, a name is not an identity (ADR-0009 §1
    says so of the scan itself). So `app.f` in a routine's body matched every
    `app.f(...)`: each overload got an automatic edge to all its siblings, and
    a body that mentions its own name — a recursive overload, or two that call
    each other one way — made a cycle of them. `creation_order` then fell back
    to name order, so `app.f(integer)` came before `app.f(text)` even where
    `depends_on:` said the opposite, and the escape hatch could not repair it:
    it adds an edge and cannot remove one.

    `creation_order` now takes no automatic edge from a routine to a module
    with the same referenced name. Two things follow, and both are chosen:

    - **A reference to an overloaded name from any other module still orders
      that module after every overload.** The scan cannot tell which one is
      meant, and creating a caller before one of them fails; over-ordering
      costs a position in the plan, under-ordering costs a failed apply.
    - **A routine that genuinely references a same-named module of another
      kind needs `depends_on:`.** PostgreSQL lets a view `app.f` and a
      function `app.f(integer)` coexist (ADR-0009 §1: views share `pg_class`
      with tables, routines do not), and the scan cannot tell that reference
      from a sibling's. The rarer case pays, and it pays with the hatch that
      exists for what a scan cannot see.

    No dialect overloads yet — MSSQL answers `overloads → false` — so nothing
    in the plan changes today. The rule is here rather than in the PostgreSQL
    crate because it is about what a *name-based scan* can know, which is the
    model's question and not an engine's.

<a id="decision-301"></a>

301. **A routine argument type is its own text type, not a `ColumnType`.**
    `RoutineId` held `Vec<ColumnType>`, which was right while the only dialect
    was SQL Server, where a parameter's type is a column's type. PostgreSQL
    identifies a routine by the types in `proargtypes`, printed through
    `format_type` — the same text `oid::regprocedure` writes — and that text is
    a wider language than a column type is.

    Measured on 18.6, from the identity of two functions declared with ordinary
    parameters:

    ```text
    id.a  ->  id.a(character varying,"char",integer,numeric,
                   timestamp with time zone,integer[],text[])
    id.d  ->  id.d(id.pos,time without time zone,interval,
                   bit varying,character)
    ```

    Of those twelve, three parse as a `ColumnType`. The rest are arrays, a
    quoted name (`"char"` is a real type and is *not* `character`), a
    schema-qualified domain, and spellings a column type does not model. Making
    them `ColumnType` would mean either widening `ColumnType` with things no
    column declaration may hold, or refusing routines this tool must be able to
    read back.

    **The identity is text, and the text is compared.** `RoutineArg` is a
    validated string: it must be writable into an identity string and readable
    back out of one, so it refuses an empty argument, a top-level comma, and
    unbalanced `()`, `[]` or `"`. It canonicalizes only what every engine
    agrees on — outside double quotes, case folds down and the whitespace
    beside `(`, `)`, `[`, `]` and `,` is dropped; inside them nothing is
    touched, because `"char"` and `"CHAR"` are two types. So `INT` and `int`
    and `decimal(10, 2)` and `decimal(10,2)` are one key, and the canonical
    spelling is the one the identity string carries.

    **`normalize_routine_arg` stays the dialect's.** A dialect that does model
    its parameters as column types parses the text, normalizes it as a column
    type, and prints it back; text that does not parse as one is left alone.
    That keeps SQL Server's `INT` -> `int` folding (72) and lets PostgreSQL
    answer with the catalog's own spelling.

    Not a `ColumnType` extended with a "raw" variant: that variant would be
    reachable from a column declaration, where none of these spellings is
    valid, and the loader would have to refuse it there. A type that cannot
    hold the bad value beats a branch that checks for it.

    Closes the question 59 left open.

    **Amended: the dot is punctuation too.** Measured, the engine accepts a
    space around a qualified type's dot and never writes one back —
    `CREATE FUNCTION md.spaced(a md . my_type)` reads back as
    `md.spaced(md.my_type)`. Left unfolded, the declared key and the catalog
    key are two keys for one routine, and every plan drops it and creates it
    again: the cry-wolf loop ADR-0002 names as the failure to avoid. The fold
    now drops the whitespace beside `.` as well, outside quotes only, so a
    quoted name keeps whatever it holds.

    **Amended: the standard's array spelling is peeled too.** Measured,
    `text ARRAY`, `text ARRAY[4]`, `int ARRAY [2]` and `character varying
    array` are identified as `text[]`, `text[]`, `integer[]` and `character
    varying[]`, while `text ARRAY[]` and `text[] ARRAY` are syntax errors. So
    the word is peeled once, after the brackets and never before them. Left
    in, a declared `text ARRAY` was keyed as a routine the catalog spells
    `text[]`, and the routine the `CREATE` had just made was not found under
    its own key — a valid plan refused.

    **Amended: a Unicode-escaped identifier is the plain quoted name it
    spells.** The engine accepts `U&"…"` wherever an identifier goes, a
    routine's argument type included — measured, `r12.a(v r12.U&"\006doney")`
    has the identity `r12.a(r12.money)`, and with `UESCAPE '!'` the escape is
    the one given. `RoutineArg` decodes the form the way the engine does
    (`\XXXX`, `\+XXXXXX`, a doubled escape, a surrogate pair) and canonicalizes
    it to `"…"`, so one spelling of a name is one key; a form that does not
    decode is refused as text that is not one argument, which is what the
    engine says of it too. The decoder lives in the model, and the emitter's
    trigger scan (302) reads it from there. The escape character may itself
    be the punctuation an identity is split on — measured, `UESCAPE ','` and
    `UESCAPE ')'` are accepted — so the identity's own split steps over the
    clause as the three characters it is. The quoted form is then spelled
    the way the engine spells a quoted name in an identity: bare where its
    `quote_identifier` leaves it bare — `[a-z_][a-z0-9_]*` and not a keyword
    the grammar reserves in some position — and quoted everywhere else.
    Measured, `zq."my_type"` and `zq."zone"` are `zq.my_type` and `zq.zone`,
    while `zq."select"`, `zq."Order"` and `zq."möney"` keep their quotes; the
    keyword table is read from `pg_get_keywords()`, not from memory. The
    same rule runs the other way for an unquoted name: measured, `s.Ätype`
    and `r8.a\u{a0}b` declared bare are identified as `s."Ätype"` and
    `r8."a\u{a0}b"`, and the key kept bare was one `module_oid` compared
    against `format_type` and never matched — so the routine a plan had just
    created was not in the catalog to the next.

    **Amended: the parameter scan steps over the clause too.** A parameter's
    mode may follow its name, and a Unicode-escaped name carries its
    `UESCAPE 'x'` after it: measured, `CREATE FUNCTION dq.f(U&"n!0061me"
    UESCAPE '!' OUT integer)` has the identity `dq.f()`, and with `INOUT` or
    a bare type after the clause, `dq.g(integer)` and `dq.f(integer)`. The
    scan looked for the mode where the clause was, counted the parameter,
    and refused a correctly keyed routine.

<a id="decision-302"></a>

302. **A PostgreSQL trigger's table is in its identity *and* in its
    definition, and a declaration where the two disagree is refused.**
    ADR-0002 fixed where a module's `definition:` begins by what the emitter
    can derive, and for a trigger that was `CREATE OR ALTER TRIGGER <name> ON
    <table>` — T-SQL's grammar. PostgreSQL's is not the same shape:

    ```text
    CREATE TRIGGER audit AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION …
    ```

    The table comes **after** the event list, which is text only the
    declaration holds. Splitting the prefix there would mean finding the end of
    the event list, and that is parsing SQL (§8.2). So the emitted prefix is
    `CREATE TRIGGER <name>` and the table appears twice.

    Measured, the duplication is not caught by anything else: the engine
    accepts `CREATE TRIGGER audit AFTER INSERT ON app.other` under the key
    `app.t.audit` without a word, and the mismatch surfaces a plan later as
    `DROP TRIGGER audit ON app.t` finding nothing. So `validate_module` refuses
    a trigger whose definition does not name the table its identity does, by
    ADR-0002's own best-effort identifier scan.

    **The scan is used in the direction where a miss is loud.** A declaration
    that does name its table in a spelling the scan cannot see is refused here,
    and the fix is to qualify the name; the alternative — accepting and
    discovering it later — creates the trigger somewhere else and leaves the
    key pointing at nothing.

    Also measured, and the reason the trigger's own name is emitted bare:

    ```text
    CREATE TRIGGER m1.audit AFTER INSERT ON m1.t …   syntax error at or near "."
    ```

    A trigger name is not schema-qualified on this engine, which is the same
    fact ADR-0009 §1 records from the other side — the schema in
    `ModuleId::Trigger` is the table's, and there is nowhere else for it to
    come from.

    **Amended in review: the check reads the `ON` clause, not the whole text.**
    The first version asked ADR-0002's identifier scan whether the definition
    mentioned the table *anywhere*, and that answers yes to

    ```text
    AFTER UPDATE OF t ON app.other ...        under the identity app.t.audit
    ```

    because the column list mentions `t`. Measured, the engine accepts it
    without a word and creates the trigger on `app.other` — the exact silent
    mismatch the check exists to prevent, waved through by the check. What
    decides the outcome is the name after `ON`, so that is what is read: a
    lexical scan that steps over literals and comments with the emitter's own
    `skip_datum`, counts parentheses so an `ON` inside a `WHEN (…)` is not the
    clause, and takes the first bare `on` at depth zero. The grammar puts
    nothing else there, and `INSTEAD OF` is `OF`.

    A definition with no readable `ON` is refused rather than guessed at, which
    is the direction a scan may be wrong in: the remedy is to write the clause
    where the engine expects it.

    **Amended: the dot is a token of its own.** Measured, `ON app . orders`,
    and a comment or a line break on either side of the dot, all create the
    trigger on `app.orders`. The scan wanted the dot glued to the name, read
    `app` as the table, and refused a valid declaration; it now steps through
    the same gap on both sides of every dot — in the scan that finds the name
    and in the comparison that reads it, which are two readers of one slice.
    The gap after `ON` itself is one too: measured, `ON /* c */ app.t` creates
    the trigger on `app.t`, and the scan steps through it as well.

    **Amended: `U&"…"` is one identifier.** Measured, `ON U&"r11".U&"\0074"`,
    `u&"r11"."t"` and `U&"r11".U&"!0074" UESCAPE '!'` all create the trigger
    on `r11.t`, and `U & "r11"` with a space is a syntax error. The scan read
    `U` as the table and refused a valid declaration. A Unicode-escaped part
    is read whole now, with its own `UESCAPE` or the default, and decoded the
    way the engine decodes it — `\XXXX`, `\+XXXXXX`, a doubled escape, a
    surrogate pair. A part that does not decode is one the gate cannot be
    certain about, and it refuses only what it is certain about: the engine
    refuses that spelling by name, and the catalog assertion after the
    `CREATE` covers the rest.

<a id="decision-303"></a>

303. **A routine argument this dialect's catalogue does not know is passed
    through, not refused.** `Postgres::normalize_type` refuses an unknown
    column type, and the obvious move was to answer the same way for a
    routine's arguments. It is wrong here for a reason that does not apply
    there: a column's type has to be one this tool can spell, and a routine's
    argument only has to be the text the engine's identity carries.

    Measured, of one function's twelve parameters the catalogue knows three by
    name, and the rest are a domain, an enum, `"char"`, arrays and spellings a
    column may not hold. Refusing them would refuse ADR-0009 §1's own example
    and make the routines of an ordinary database unreadable.

    The bargain is ADR-0009 §1's, stated there and taken here: *"the declared
    signature is what the emitter writes into `DROP FUNCTION` … and a mismatch
    produces a `CREATE` the engine refuses or an object the next plan reports
    as one to drop and one to add. Both are loud, both are inside the plan's
    transaction, and neither is silent."* The user writes what `pull` showed
    them, which is the engine's own text.

    What the fold does do is what every engine agrees on and ADR-0009 §1
    measured: a modifier is discarded (`varchar(10)` is `character varying`),
    an array collapses to one `[]` (`text[][]` is `text[]`), and everything
    else goes through the column catalogue's alias table so `int4` is
    `integer`. `float(24)` is why the modifier is not thrown away *before* the
    catalogue is asked: it is `real`, and `float` is `double precision`.

    **Amended in review: the modifier goes whether the catalogue knows the type
    or not.** "Returned unchanged" was written as one rule and turned out to be
    two. Discarding a modifier is what the engine does to *every* routine
    argument; consulting a closed catalogue of column types is a different
    question that only some arguments have an answer to. Tying the first to the
    second left a declared `bit varying(4)` — a perfectly ordinary parameter,
    absent from the column catalogue because ADR-0012 §1 has no need of it —
    permanently unequal to the `bit varying` the catalog reads back, so the
    routine was one to create and one to drop on **every** connected plan.

    That is not the bargain this entry struck. DECISIONS 303 accepts one loud
    mismatch that a user fixes by writing what `pull` showed them; it does not
    accept the cry-wolf loop ADR-0002 names as the failure to avoid. So the
    fold is three attempts — with the modifier, without it, and without it and
    unfolded — and only the *fold* is the catalogue's. Measured on 18.6, the
    identity of one function's five parameters:

    ```text
    bit varying(4)  bit(3)  interval hour to minute  interval second(3)  m8."odd(name)"
    bit varying     bit     interval                 interval            m8."odd(name)"
    ```

    Two more rules fell out of that measurement. `interval` is the one type
    this grammar follows with words rather than a parenthesis, and its field
    qualifier goes the way a modifier does. And a quoted name's own parentheses
    are part of the name — `m8."odd(name)"` keeps them — so the modifier is
    found outside quotes or not at all.

    **Amended: an alias the engine identifies as something else is not a
    spelling the catalogue does not know.** Passing `varbit` through as
    written keyed a routine the `CREATE` never made — measured, `varbit(4)`
    is identified as `bit varying` — and `module_oid` resolved nothing. The
    aliases the column catalogue does not carry (`varbit`, `bpchar`, `nchar`,
    the `national …` spellings, `char varying`) are folded to the identity
    `format_type` writes, each measured, in a table a test keeps disjoint
    from the column catalogue so that one rule is not spelled twice. A
    domain, an enum, a composite still pass through: the engine is still the
    normalizer for what this table does not know. And a built-in written with
    its schema is the built-in: measured, `pg_catalog.int4`, `PG_CATALOG.INT4`,
    `"pg_catalog".int4` and `pg_catalog."int4"` are all `integer`, so the
    qualifier is dropped before the name is folded. So is the catalog's own
    name for a built-in's array: measured, `_int4`, `_varbit` and
    `_numeric(10,2)` are `integer[]`, `bit varying[]` and `numeric[]`. Built-ins
    only — `ar._my_type` is `ar.my_type[]` but `ar._solo` is `ar._solo`, and
    which of the two a user's name is cannot be decided offline, so it passes
    through as written and the engine decides.

<a id="decision-304"></a>

304. **A module whose deparsed statement this reader cannot cut is named and
    left out, never recorded with an empty body.** The declaration holds
    everything after the object's name, and PostgreSQL hands back the whole
    statement, so the pull has to cut it. Measured, the three shapes:

    ```text
    CREATE OR REPLACE FUNCTION m4."odd Name"(a integer)⏎ RETURNS integer …
    CREATE OR REPLACE PROCEDURE m4.p(a integer)⏎ LANGUAGE sql …
    CREATE TRIGGER "audit x" AFTER INSERT ON m4.t FOR EACH ROW …
    ```

    The name is **stepped over**, not searched for: looking for the first `(`
    finds the wrong one in `"f(x)"."g"`, and rebuilding the name to compare
    against would mean reproducing the deparser's own quoting rules, which is
    the deparser's job and not this reader's.

    Where the text is not that shape, the module is left out with a warning
    naming it. The alternative — an empty `definition` — is the failure mode
    this project keeps finding: absent, empty and unreadable are three
    different things, and an empty body is one the next plan writes back over a
    working object.

    The same round trip the tables are asked for applies to the identity: a
    view called `f(int)` reads back as a routine with an argument list, so a
    module id that does not survive `ModuleId::from_str(&id.to_string())` takes
    its object out of the pull rather than into a schema that will not load.

    **Amended: a trigger is held only where its relation is.** The trigger
    arm read every user trigger; the table reader does not hold every table.
    Measured, the engine allows a trigger on a partitioned table and on an
    `UNLOGGED` one, and read back without its relation the trigger was a
    module whose `on:` named a table the schema did not have — `check_names`
    refused the pull whole. The arm now selects by the table reader's own
    predicate (or a view the view reader holds — measured, a user's `INSTEAD
    OF` trigger on a view an extension owns is not extension-owned itself),
    the complement is named as a limitation beside the relation's own, and a
    test ties the three to one string.

<a id="decision-306"></a>

306. **On this dialect every carried attribute refuses the rebuild today,
    because there is no declared grant for one to come back from.**
    ADR-0009 §3 decides that a grant to a **declared** role survives a module
    replacement, by the machinery ADR-0005 built — and roles and grants are
    Phase 5 step 6. Until that lands, `pbps-pg` has no `Grant` to emit, so an
    object carrying anything at all is one this dialect cannot rebuild.

    The conservative direction is the only one available, and it is also the
    right one to start from: warning and proceeding would put "the application
    lost access" behind a line of output nobody reads at 3am. The step that
    adds grants narrows this to what the declarations still cannot reproduce;
    it does not remove it, because ADR-0010 §5 records that pbps cannot express
    "revoked from `PUBLIC`" and therefore must not take it away.

    What is enumerated is the catalog and not a list — the ADR's own rule,
    after three review rounds each found the same shape one attribute further
    out. Measured on this branch, and each a refusal: a grant in `relacl` or
    `proacl`; a revocation from `PUBLIC`, which is the *absence* of a row and
    so invisible to a check that compares rows; an owner other than the
    deploying account, which a `DROP` and `CREATE` silently transfers and which
    turns a `SECURITY DEFINER` routine into a privileged one; `reloptions`;
    a view column default in `pg_attrdef`; a trigger's `tgenabled`; and the
    grants a *new* object would arrive with from `pg_default_acl`, which no
    comparison against the old object can see.

    **Amended in review, twice, and both are the same sentence proving itself
    again.** A fifth attribute: a grant on one *column* lives in
    `pg_attribute.attacl`, and measured, `GRANT SELECT (a) ON v TO r` leaves
    `pg_class.relacl` **NULL** — so an object-level ACL check reports nothing
    carried, the rebuild goes ahead, and the column grant is gone. An object
    with an empty ACL is the easiest case to wave through, for the second time
    in this entry.

    And the §4 half had the same shape one catalog over: the dependent
    enumeration had an arm per catalog it had thought of, and measured, a
    function behind a cast has its reverse edge in `pg_cast` and one behind an
    operator in `pg_operator`. Neither had an arm, so the edge was dropped
    entirely, `dependents` reported the rebuild unblocked, and the emitted
    `DROP FUNCTION` failed at apply — the applyable-and-predictably-fails
    outcome SPEC §7.5 exists to prevent. There is now a fallback arm, and the
    class list the arms handle is one constant the fallback excludes, pinned to
    the arms by a test.

    **A list obeys "enumerate from the catalog, not from memory" only when it
    has a fallback.** Three enumerations in this design have now been written
    as the cases somebody thought of, and each was corrected by finding the next
    one. What ends that sequence is not a longer list.

    **Amended: a comment is a carried attribute, and the list is now closed by
    measurement rather than by memory.** `COMMENT ON` puts a row in
    `pg_description`; a `DROP` takes it and a `CREATE` does not bring it back —
    measured on all three kinds and on a view's column. `Module` has a
    `description`, but nothing writes it to the database (the `COMMENT ON`
    round trip is a decision of its own, and `SetColumnDeprecated` says so from
    the other side), so there is nothing to restore it from and it refuses like
    the rest.

    That was the second time this enumeration was short — a column ACL was the
    first — so the other half was measured too: `pg_get_functiondef` writes the
    volatility, `SECURITY DEFINER`, `LEAKPROOF`, `COST` and every `SET` clause,
    so a routine's settings come back with its body; a view's column cannot
    hold `attoptions` at all, since `ALTER VIEW … ALTER COLUMN … SET` is `not
    supported for views`; and a trigger or a rule attached to a view is a
    *dependent*, enumerated and refused on its own terms.

    **And that argument was still wrong.** The round after it found a security
    label, and the round after that would have found something else: a `DROP`
    takes every row the catalog keys by the object's *address*, and there is no
    amount of thinking that turns a remembered list of those into a complete
    one. So the list is now asked of the engine:

    ```sql
    SELECT c.relname FROM pg_class c
     WHERE c.relnamespace = 'pg_catalog'::regnamespace AND c.relkind = 'r'
       AND EXISTS (SELECT 1 FROM pg_attribute a
                    WHERE a.attrelid = c.oid AND a.attname = 'classoid')
       AND EXISTS (SELECT 1 FROM pg_attribute a
                    WHERE a.attrelid = c.oid AND a.attname = 'objoid');
    ```

    Five on 18.6 — `pg_description`, `pg_seclabel`, `pg_init_privs`, and the
    two `pg_sh*` ones — and the reader reads all five, including the two a
    module can never be in, because "a module cannot be there" is the shape of
    claim that has been wrong every time. A live test runs that query and
    compares it with the reader's list, so a sixth catalog in a later release
    fails a test instead of passing unnoticed.

    Beside them, one thing `pg_depend` keys the other way: `ALTER FUNCTION …
    DEPENDS ON EXTENSION` writes a `deptype = 'x'` row *from* the routine, and
    measured, `pg_get_functiondef` does not write the clause — so a rebuild
    creates a routine that outlives the extension it was tied to. Measured too,
    the grammar allows it on a routine and a trigger and not on a view; the
    query runs for all three anyway, for the reason above.

    **A list is closed by a test against the engine, not by an argument about
    what is on it.** Three arguments were made here and three were wrong.

    **Amended: a fallback covers a missing arm, not a leaky one.** The next
    case arrived inside a class the list already knew. A domain's check
    constraint is a `pg_constraint` row with `conrelid = 0` — measured, it
    names its domain through `contypid` — so the arm's own inner join to
    `pg_class` threw it away, and the fallback could not see it because
    `pg_constraint` is on the known list. `NOT tg.tgisinternal` did the same to
    a trigger the engine owns. **Every arm is now total over its class**: a row
    an arm cannot represent comes back with a sentence saying so, never as no
    row at all. A filter inside an arm turns "there is something here this
    project cannot put back" into "there is nothing there", and the second is
    what makes a plan applyable and predictably failing.

    **Amended again: an arm can be total and still name the wrong object.** A
    user rule on a view (`CREATE RULE ins AS ON INSERT TO app.v …`) has its
    `pg_depend` edges in `pg_rewrite`, whose `ev_class` is the view the rule
    is on — the view itself. The arm reported the view as its own dependent,
    and the walk discarded that as the root. Measured, `DROP VIEW` deletes the
    rule and `CREATE VIEW` does not restore it: the rebuild went ahead and the
    rule was silently gone. Only the engine's `_RETURN` rule *is* the view;
    any other now comes back as a dependent this model cannot put back, and
    refuses. The same silence as the filter's, reached another way — a row
    that was there, named as something the reader already held.

    **Amended: a view's row type is a reference to the view.** Measured, a
    routine that takes `v` or `v[]` as an argument, or returns `v`, depends on
    `type v` or `type v[]` with `deptype` `n`, and the type depends on the
    view with `i`; `DROP VIEW v` names all three routines. Filtering the
    internal edge is right — the type is not a dependent anybody drops — but
    never asking about the type as a *reference* left those routines unseen,
    and the walk called the rebuild unblocked. The reverse-edge predicate now
    names the view, its row type and the row type's array type, in one
    spelling shared by the dependents query and the argument query.

<a id="decision-307"></a>

307. **The rebind test is a name and a path, not a position on it.**
    ADR-0013 §3 requires that a same-named object a plan introduces rebuilds
    the modules it could capture, in that same plan. The obvious
    implementation asks which candidate is *earlier* on the write path than the
    current binding, and it cannot be written: an overload in the same schema
    captures a call without anything moving, and what an unchanged declaration
    would bind to today cannot be computed without parsing it (§8.2 forbids) or
    creating it (planning must not).

    So the test is: this plan brings an object into a schema on that module's
    effective write path, and the module's text mentions that object's bare
    name. One rebuild, once. A declaration that qualified the name in full is
    rebuilt too — deliberately conservative, and the ADR says so.

    Measured, all three states, which is what makes "one plan late" a cost and
    not a phrase:

    ```text
    before anything arrives:                    caller() = 'shared'
    after the shadow arrives, with no rebuild:  caller() = 'shared'
    after the rebuild:                          caller() = 'app'
    ```

    The middle line is a whole plan cycle in which the environment means one
    thing and the declarations mean another, with nothing in the plan that
    created the shadow having said so.

    **Amended: a trigger arriving is not a shadow.** Nothing calls a trigger
    by name, so the test asks for the name a body would reference the
    arriving object by (`ModuleId::referenced_name`), which a trigger does not
    have. Asked for the object name instead, a trigger `app.orders.audit`
    rebuilt every caller of `audit()` for a binding that cannot move — and
    where such a caller has dependents, that rebuild is a refusal of a plan
    that was valid.

<a id="decision-308"></a>

308. **A routine's parameter list is checked against its identity, and only
    where the disagreement is certain.** The emitter writes
    `CREATE FUNCTION <name>` and the declaration writes everything after the
    name (301, ADR-0009 §1), so the identity's argument types live in the key
    *and* in the body — the same split the trigger's `ON` clause has, and the
    same silent failure. Measured: `CREATE FUNCTION app.f\n(x text) …` under
    the key `app.f(integer)` is accepted without a word and creates
    `app.f(text)`. The key names an object that does not exist, and every later
    plan creates it again and drops nothing.

    So `validate_module` reads the list. Three facts from the engine make that
    a scan and not a parse:

    ```text
    CREATE FUNCTION me.noparens RETURNS int …   syntax error at or near "RETURNS"
    CREATE FUNCTION me.o(out int) …             identity  me.o()
    CREATE FUNCTION mf.a(a out int) …           identity  mf.a()
    ```

    The list is mandatory, `OUT` is the one mode that keeps a parameter out of
    `proargtypes`, and a mode may be written on either side of the name. With
    the mode off, a parameter is `type` or `name type` and the grammar offers
    nothing else, so exactly two readings are tried.

    **Only a certain disagreement refuses.** A count is always certain. A type
    is not: `format_type` under the empty read path always qualifies a user
    type (ADR-0013 §3), so a key reading `app.f(md.my_type)` over a body
    reading `(a my_type)` is one object whenever the write path reaches `md` —
    and this dialect cannot know whether it does. Refusing that would refuse a
    valid plan, which is the one direction this gate may not be wrong in. A
    spelling the dialect cannot parse counts as agreement for the same reason:
    a scan that cannot read a spelling has not learned that it is wrong.

    What stands behind the cases it cannot decide is the catalog assertion
    after the `CREATE` (ADR-0009 §3), which is keyed by the identity and fails
    inside the transaction. An offline gate that decides what it can and a
    connected assertion that decides the rest is the split; a gate that guessed
    would be neither.

    **Amended: the second reading is not offered for a spelling the catalogue
    knows.** Trying both readings unconditionally introduced an ambiguity of
    its own. `(double precision)` splits into a parameter named `double` of
    type `precision`, and with a user type of that name the qualification rule
    above then accepts the body under the key `f(app.precision)` — while the
    engine creates `f(double precision)`. Measured, with `mq.precision` in the
    database:

    ```text
    CREATE FUNCTION mq.b(double precision) …   ->  mq.b(double precision)
    ```

    The engine does not offer that reading, so neither may the gate: where the
    whole remainder is a spelling this catalogue knows, that is the type and
    there is no second reading. A gate is allowed to be undecided; it is not
    allowed to invent a reading the grammar does not have.

    **Amended: every gap in the parameter is a gap.** The first comment case
    was fixed at the front of the parameter and nowhere else. Measured,
    `(value /* note */ OUT integer)`, `(OUT /* note */ value integer)`,
    `(value OUT /* note */ integer)` and a line comment between the name and
    the mode all have the identity `()`, and `(IN /* note */ x /* note */
    int)` has `(integer)`. The scan now steps through the gap on every side of
    the mode, not only the first; a scan that knew comments were whitespace
    in one position and not the next was refusing a valid declaration for a
    count only it got wrong.

    **Amended: the scans' whitespace is ASCII.** The rule of 313, applied to
    the emitter's own scans: measured, `CREATE FUNCTION r10.f(a r10.x\u{a0}, b
    int)` has the identity `r10.f(r10."x\u{a0}",integer)`, the non-breaking
    space being the last byte of the type's name. `str::trim` at a parameter's
    boundary, before a default and after a gap cut that byte off and compared
    `r10.x` with a catalog that says `r10."x\u{a0}"`. Every trim in these
    scans is an ASCII one now.

    **Amended: a `$` after an identifier byte is part of the name.** `$`
    continues an identifier on this engine, and measured, `CREATE FUNCTION
    dq.f(foo$tag$ integer)` is accepted with the identity `dq.f(integer)`.
    The per-character scans asked the literal test from the `$` alone, read
    `$tag$` as the opener of a dollar-quoted literal nothing closed, and
    refused the routine. The literal test now knows the byte before it — the
    rule the dialect's normalizer already applied — in one helper every such
    scan goes through.

<a id="decision-309"></a>

309. **A module the deparse could not find is the catalog moving, not a reader
    out of step with its query.** The pull reads the catalog in one
    `REPEATABLE READ READ ONLY` transaction so that it cannot report half of a
    change as a whole schema, and 267's guard turns the `XX000` a moved catalog
    raises into a retryable message. The module queries opened a second way for
    the catalog to move, and it does not raise. Measured:

    ```text
    pg_get_viewdef(999999, true)  ->  NULL
    pg_get_functiondef(999999)    ->  NULL
    ```

    A deparser resolves its oid through the syscache against a *fresh*
    snapshot, so an object dropped between the scan and the deparse comes back
    as a row with a name and no definition. Read through the ordinary
    `missing` helper that said "the query and this code have gone out of step",
    which is the one diagnosis that is certainly wrong — nothing is out of
    step, and a reader sent to look for a renamed column will not find one.
    **Absent, empty and unreadable are three different things**, and a vanished
    object is the third.

    So the modules read takes the definition as optional and turns `NULL` into
    the same "the catalog changed while it was being read" the `XX000` path
    gives. Not into a limitation and not into a skipped module (304): 304 is
    for a statement this reader cannot *cut*, which is a fact about the object
    and stays true on the next pull. This is a fact about the moment, and the
    answer to it is to read again.

    It was found by the live suite going red under its own parallelism, which
    is what that suite is for: every test builds a schema and drops it, so a
    pull is nearly always running across somebody's `DROP`. A defect that only
    appears when two things happen at once has no other way to be found.

<a id="decision-310"></a>

310. **A pull and a rebuild can deadlock, and the answer is a sentence rather
    than a lock order.** Reading a module's definition means deparsing it, and
    `pg_get_viewdef` opens the view — so a pull holds `ACCESS SHARE` on every
    view in the database for as long as that query runs. A rebuild takes
    `ACCESS EXCLUSIVE` on the object it is about to replace (ADR-0009 §3).
    Neither can be reordered: the pull's order is the catalog's, and the
    rebuild's is one object. Measured, from the server log:

    ```text
    deadlock detected
    Process A: LOCK TABLE "app"."granted" IN ACCESS EXCLUSIVE MODE
    Process B: SELECT … pg_get_viewdef(c.oid, true) …
    ```

    The engine detects the cycle, picks a victim and rolls it back **whole** —
    there is no half-read schema and no half-applied plan, which is the only
    property that matters here. What was missing was the words: `40P01` reached
    an operator as `db error` and nothing else, on both paths.

    So both say it: the pull's guard (267) gains a `40P01` arm beside its
    `XX000` one, and `before_a_rebuild` wraps the lock it takes. Each says the
    same three things — it was a tie, nothing changed, run it again — and the
    rebuild's names the likely other side, because "a `pull` or a `status`
    opens every view in the database" is not something an operator can be
    expected to know.

    Not solved by taking a weaker lock: the lock is what makes the catalog
    enumeration before the `DROP` mean anything. Not solved by holding the
    deployment lock either — that serializes deployers, and a `pull` is not
    one.

    Found by the live suite, where fifty tests read the catalog while a handful
    lock objects. The suite retries, and says in the helper that the retry is
    its own concurrency rather than the product's.

<a id="decision-311"></a>

311. **The drop order for dependents is a topological order, not a depth.**
    A breadth-first walk gives each dependent the depth of the *shortest* path
    to it, and two dependents at one depth come out in whatever order the
    catalog gave. Measured, that is wrong the moment a diamond appears:

    ```text
    a and b are both views over v, and b is also over a
        DROP VIEW mj.a  ->  cannot drop view mj.a because other objects
                            depend on it
                            DETAIL:  view mj.b depends on view mj.a
    ```

    which is the applyable-and-predictably-fails outcome SPEC §7.5 exists to
    prevent — the same one the depth walk was added to fix, one shape further
    out. So the walk records the *edges* and the order comes from them: a node
    is ready when everything that depends on it has already gone, ties broken
    by name so that a plan is the same plan twice.

    **A cycle is a case, not an impossibility.** `CREATE OR REPLACE` closes one
    between two `BEGIN ATOMIC` routines — measured, `pg_depend` then holds both
    directions and neither routine can be dropped first. There is no order, so
    each member is named as something the plan cannot put back rather than
    emitted in an order that fails. The same for a cycle that runs through the
    module being rebuilt: it is not one of its own dependents, and what the
    walk coming back to it really says is that no rebuild of it is possible
    without `CASCADE`, which SPEC 14.3 does not offer.

    A depth is the answer to "how far", and the question was "in what order".

<a id="decision-313"></a>

313. **A routine argument folds ASCII case only.** `RoutineArg` lower-cased
    with `char::to_lowercase`, which is Unicode's fold and not this engine's.
    Measured:

    ```text
    CREATE FUNCTION mn.f(a mn.Ätype) …   ->  mn.f(mn."Ätype")
    ```

    The engine left the byte alone and *quoted* the name rather than folding
    it. A Unicode fold turns the declared spelling into `ätype`, which names a
    type that does not exist — so the key points at nothing, `module_oid`
    resolves nothing, and the object is planned as absent. The emitters' own
    `unquoted` has always been `to_ascii_lowercase`; this is the same rule in
    the model, where the two were quietly disagreeing.

    **Amended: whitespace is ASCII too.** The same rule, one character class
    over. `char::is_whitespace` is Unicode's answer, and a non-breaking space
    is whitespace to it; to this engine every non-ASCII byte is an identifier
    character. Measured, `CREATE FUNCTION r8.f(v r8.a\u{a0}b)` is accepted with
    the identity `r8.f(r8."a\u{a0}b")`, and `r8.a b` with a plain space names
    no type at all — so the fold turned a valid key into one that resolved
    nothing. The fold trims and collapses ASCII whitespace only, and the
    whitelist admits any non-ASCII byte, which is `continues_ident`'s rule.
    The dialect's normalizer follows the same rule wherever it looks for a
    gap — before the array keyword, after `interval`, around a modifier — so
    `a\u{a0}array` is a type name and not `a[]`. And so does the emitter's
    trim of a module body: measured, `CREATE VIEW v AS SELECT 1 AS x\u{a0}`
    names the column `x\u{a0}`, and `str::trim` had taken the byte off the
    end of the body before the `CREATE`, so the view the plan made had a
    column the declaration does not name. And the identity's own test for an
    empty argument list: measured, a type may be named by one non-breaking
    space and `g(\u{a0})` is a routine of one argument, which a Unicode trim
    read as `g()`. And `$`, which `continues_ident` names and the whitelist
    did not: measured, `CREATE FUNCTION dl.h(a dl.money$type)` is accepted
    with the identity `dl.h(dl."money$type")`, and the argument was refused
    before it reached the engine.

    **Amended again: `$` is admitted only where an unquoted identifier is
    already open** (issue #204). The character was admitted at any position,
    which gave back the property the whitelist exists for. The whitelist is not
    about which types exist — it is what makes the identity safe to interpolate
    verbatim into `DROP FUNCTION` and `GRANT` by construction rather than by
    review, and a `$` that opens a token is where the interpolated text stops
    being a name. Measured on 18.6:

    ```text
    CREATE DOMAIN dq.a$$b AS numeric …   ->  dq.f(dq."a$$b")
    CREATE DOMAIN dq.$x   AS numeric     ->  syntax error at or near "$"
    DROP FUNCTION dq.f($$)               ->  unterminated dollar-quoted string
                                             at or near "$$); SELECT 1;"
    ```

    Inside a word the `$` is a byte of the name — the identifier is the longer
    match, so `a$$b` is one name and not a quote opening — and that spelling
    stays accepted. A token that *opens* with one names no type in any case, so
    refusing it costs nothing and takes the swallowed statement with it.

    **And "an identifier is open" is state, not the last character emitted.**
    The first rule read back one character and asked whether a name may contain
    it, which a digit may — so `1$$` passed, and review round 1 on #520 was
    right that it should not. An ASCII digit continues an identifier and cannot
    begin one, so a token that starts with one is a numeric constant and the
    `$$` after it opens a quote like any other. Measured:

    ```text
    SELECT 1$$;                              ->  unterminated dollar-quoted
                                                 string at or near "$$;"
    DROP FUNCTION app.f(numeric(10$$)); …    ->  unterminated dollar-quoted
                                                 string at or near
                                                 "$$)); SELECT 1;"
    ```

    The second is the one that matters: the parse is still live inside the
    modifier, so nothing refuses the statement before the quote swallows its
    suffix. The parser now carries which token is open — closed, an identifier,
    or a numeric constant — updated in one place so no arm of the whitelist can
    forget it.

    **Three states and not two**, which review round 2 was right to ask for
    even though its reasoning does not hold on any server this suite runs.
    `1e2$$` passed a two-state parser, because the `e` made a token that had
    begun with a digit look like an identifier. Measured on 18.6 and 16.15,
    that spelling does *not* open a quote — `SELECT 1e2$$;` and `SELECT 0x1$$;`
    are `trailing junk after numeric literal`, and so is `1e2$q$a$q$` — so
    there the engine refuses it and nothing is swallowed. The junk check
    arrived in PostgreSQL 15 and this tool sets no lower bound on the server it
    will talk to, so the whitelist models the lexer rather than that check: a
    token that opened with a digit stays numeric however many letters follow,
    because an exponent and a base prefix are part of the number. A letter
    inside a name is still a name (`a1$b`), which is the case the two-state
    parser got right and this one keeps.

    **And the decimal point does not end the number, though it does end a
    name.** Round 3 found `1.e2$$`, where the dot closed the token and the
    exponent's `e` opened an identifier. Measured on 18.6 and 16.15, that
    spelling is junk like the others — but `SELECT 1.5$$;` is `unterminated
    dollar-quoted string` on both, with no junk check to catch it, because a
    number followed by a quote is exactly what it is. So the dot is carried
    through a numeric token and closes an identifier one, which is the lexer's
    own asymmetry: `1.e2` is one constant and `a.b` is two names. With that,
    every character the engine counts as part of a number — digits, the point,
    an exponent's letter, the `0x`/`0o`/`0b` prefixes and the `_` separator —
    is inside the numeric state, and the family is closed rather than patched
    one round at a time.

<a id="decision-420"></a>

420. **PostgreSQL module rebuilds reach the carried-state check, on both sides
    of the DDL.** The connected planner checks explicit alterations and
    synthesized drop/create replacements in a transaction it rolls back. Apply
    checks before dropping and after creating, inside the transaction that
    also records the result. The second check catches an event trigger or new
    default privilege that changes what the new object carries. Every carried
    item still refuses under ADR-0009 §3; grant restoration remains #248.
    A staged rebuild refuses because its checkpoints cannot preserve the
    check/lock/DDL transaction. Ordinary module drops are not rebuilds.

    The successful live rebuild also exposed a false refusal: its PostgreSQL
    deparsed definition differed from the declaration, and 160's comparison
    assumed SQL Server's preserved text. The pure dialect now answers whether
    a read-back matches a written module. PostgreSQL checks kind and existence,
    not deparsed text or repository descriptions (SPEC §7.6's explicit limit);
    SQL Server keeps its exact comparison. Untouched modules still compare two
    catalog reads exactly. This does not claim to detect another writer's
    change to the text of a module this plan itself writes.

<a id="decision-422"></a>

422. **Rebinding is part of the typed diff, before ordering and approval.**
    The connected seam reached the carried-state guard but not the pure
    `modules::rebound_by_this_plan` rule (307). Measured through the CLI:
    a parsed caller of `f(1)` stayed bound to `f(bigint)` after a plan added
    `f(integer)`; apply recorded success and verify was clean.

    The pure dialect trait now supplies the unchanged modules to rebuild for
    arriving module names, new table names, and table-rename destinations.
    PostgreSQL uses the existing write-path/name scan; the default adds none.
    The differ deduplicates those modules and adds ordinary `AlterModule`
    changes before risk classification and dependency ordering. Offline and
    connected plans therefore agree on the rebuild, and the approved plan
    carries it through the existing transactional carried-state checks. No
    SQL or new state field is added outside the dialect's existing emitter.

    A live CLI regression observes the new overload's result in the same
    apply, no repeated rebuild on the next plan, and refusal for carried
    comments or staged execution. Removing the diff integration restores the
    old runtime binding despite a successful apply. The prior engine test
    retains that bad middle state by deliberately omitting the synthesized
    alteration, then executes the alteration from the actual typed plan.

<a id="decision-447"></a>

447. **A module rebuild restates a declared role's grant because the plan says
     so, not because the declarations do.** ADR-0009 §3 refused any module
     carrying an ACL at all, with the reason DECISIONS 306 gave: step 6 (#81)
     had made a grant expressible but nothing yet re-emitted it after the
     `CREATE`. That reason held only while true, and it stopped being true when
     `pbps_diff::diff_roles` learned to treat an `AlterModule` as a drop for
     grant-comparison purposes — a new `Dialect::rebuilds_modules` capability,
     `false` by default (SQL Server's `CREATE OR ALTER` needs no such thing)
     and `true` on PostgreSQL, folds the rebuilt identity into the same
     `dropped` set a `DropModule` already populates, so every declared
     permission on the target is granted again after the rebuild by the
     mechanism ADR-0005 built for exactly this shape (table replacement).

     The connected check in `pbps_pg::modules::before_a_rebuild` therefore asks
     the plan's own `ChangeSet` what will be restored — the `Grant` entries the
     differ already built for this `GrantTarget` — rather than re-deriving the
     answer from `Schema.roles`. Asking the plan is what the postcondition
     after the `CREATE` re-checks against, and a check that consulted the
     declarations directly could pass while restating something the plan in
     hand does not actually carry.

     The ACL comparison itself moved from "any non-empty ACL refuses" to a
     structural read: the live ACL and the engine's own `acldefault` for the
     object are each exploded by `aclexplode` (never parsed as text, the same
     instrument `pull`'s `RawGrant` reader already uses) and compared row by
     row, in both directions, on **every** rebuild carrying any ACL at all —
     not only when every remaining row can be explained away. That symmetry is
     what still catches `REVOKE EXECUTE … FROM PUBLIC`: after the revoke the
     only row left is the owner's own, which would read as nothing left to
     explain if the check stopped there, and only the comparison against
     `acldefault` surfaces the row that is *missing* rather than one that is
     present. `PUBLIC` stays off the restorable path *for good*: there is no
     grantee named `PUBLIC` for a declaration to hold, so no `Grant` can ever
     restate on its behalf, whatever else the plan restates on the same object.

     What still refuses: a grant to a role the plan's `ChangeSet` does not
     carry on this exact target, and the same role holding more than that
     `Grant` promises — an out-of-band permission would not come back, so
     losing it is exactly the state this guard exists to name. And, unchanged
     from before #248 because none of them is expressible at all, six other
     shapes: `WITH GRANT OPTION` (held even by a role the plan does grant
     plainly — the model has no flag for it), a column-level grant, an owner a
     rebuild would transfer, `reloptions`, a view column default and a
     trigger's `tgenabled`.

<a id="decision-450"></a>

450. **Trigger-function trust includes inherited ownership rights.** The direct
     owner check in 445 misses PostgreSQL memberships with INHERIT TRUE and
     SET FALSE: a member can replace the function without being able to act as
     the deployer. Function replacement preserves its OID and does not conflict
     with the guarded table's ROW EXCLUSIVE lock. Authenticate every role for
     which `pg_has_role(role, function_owner, 'USAGE')` is true, including the
     direct owner and transitive inheritors; each must have a SET path to the
     deploying role. Both planning and the immediate pre-write check use this
     predicate. Disabled inheritance confers no effective ownership rights;
     inheritors that can already act as the deployer remain trusted. This
     narrows the execution policy without granting any additional privileges
     or extending the guard to transitive routine calls and indirect writes.

<a id="decision-473"></a>

473. **A user constraint trigger is one trigger module, including its
     `CONSTRAINT` marker, rather than a second table constraint.** Measured on
     PostgreSQL 18.6, `CREATE CONSTRAINT TRIGGER` creates a non-internal
     `pg_trigger` row and a `pg_constraint` row with `contype = 't'`. The latter
     has an internal `pg_depend` edge (`deptype = 'i'`) to the trigger:
     `DROP TRIGGER` removes both, while `ALTER TABLE ... DROP CONSTRAINT`
     refuses and tells the operator to drop the trigger instead. Reading the
     companion as a separate unsupported constraint both duplicated the object
     and prevented its table from being managed (issue #229).

     The trigger keeps the identity and drop/create path of ADR-0009. Its
     opaque `definition` begins `CONSTRAINT AFTER ...`; the emitter moves that
     leading marker into `CREATE CONSTRAINT TRIGGER <name>` and keeps the
     remaining text, including `DEFERRABLE INITIALLY DEFERRED`, intact. A new
     model kind or PostgreSQL-specific flag would duplicate a distinction the
     opaque definition already holds. Ordinary trigger declarations retain
     their existing spelling. DECISIONS 302's `ON` scan still validates the
     same identity against the same clause.

     `constraints_query` excludes only `contype = 't'` for this reason. The
     module query still excludes `tgisinternal`, so an FK's internal
     `RI_ConstraintTrigger` objects remain represented by their FK, not by
     modules. The reference-data guard uses the same conversion as the pull:
     stripping only `CREATE TRIGGER` would refuse a recorded managed constraint
     trigger, while stripping the constraint marker would equate different
     definitions. The live CLI fixtures cover serialized pull, adoption with
     an empty connected plan, bootstrap, module alteration, deferred execution,
     internal FK exclusion and enforcement, and the guard's acceptance of the
     exact record and refusal of an ordinary or absent one.

     **All deferred trigger work must finish before the success record.** The
     first implementation passed the inventory round trip but, measured on
     18.6, a managed `INITIALLY DEFERRED` trigger could rewrite an inserted
     label at `COMMIT`: `apply` reported success and `verify` immediately
     reported drift. That is a wrong recording with a single deployer, so it
     is part of this fix under the review rule. Every PostgreSQL transactional
     apply now flushes `SET CONSTRAINTS ALL IMMEDIATE`
     after all plan statements, while its locks and transaction still hold,
     then rereads the managed state. A change from the state that passed the
     row statements' checks rolls back the entire apply. Only the settled
     inventory may feed the remaining absence checks and success record.
     Flushing after each row would refuse a valid constraint requiring two
     rows the same plan inserts; the positive fixture requires both, and the
     negative fixture rewrites a label after the ordinary statement check.
     Staged row writes already commit before their checkpoint read, so they
     have no pre-commit success snapshot to settle by this rule.

     **The authentication closure is not a pending-work inventory.** Draft
     review measured an approved ordinary trigger inserting into another
     managed table, whose deferred constraint trigger then rewrote the first
     table's declared row. The direct-DML and FK-action closure authenticated
     the ordinary trigger but never reached the second table through its
     routine; conditioning the flush on a constraint trigger in that closure
     therefore committed a stale record. Flush on every PostgreSQL
     transactional apply, including an apply without a directly reachable
     constraint trigger. Remove the obsolete guard flag instead of extending
     a lexical scan into a promise about routine effects.

     **A deparsed trigger body does not carry its enable mode.** Second draft
     review found that omitting the companion also removed its warning about
     a disabled, replica-only or always-enabled constraint trigger. Measured,
     `pg_get_triggerdef` has the same CREATE text in all four modes; pulling a
     non-ordinary mode as a module would recreate an ordinary enabled trigger.
     The same shape applies to ordinary user triggers, so the module reader
     now holds only `tgenabled = 'O'` and the omission inventory names every
     other mode on a held parent. This is a module limitation, not a duplicate
     table constraint. Internal FK triggers remain excluded and extension-owned
     triggers retain their existing exclusion. A managed non-ordinary trigger
     makes verify report drift and baseline refuse to erase the distinction;
     restoring ordinary mode makes the original record verify clean again.
     ADR-0009's rebuild guard still checks carried enable state under its lock.

<a id="decision-499"></a>

499. **A module rebuild re-resolves its name once the lock is held (issue
     #232).** `before_a_rebuild` resolves the module's oid **once** and keys
     every carried-state read by it — deliberately, because two independent
     name matches are two chances to disagree and the direction they fail in is
     the one that waves a rebuild through. But the lock that serializes those
     reads is taken by **name**: `LOCK TABLE <schema>.<view> IN ACCESS
     EXCLUSIVE MODE` for a view, and its parent table's for a trigger. A name
     is not an object, and PostgreSQL resolves a `LOCK TABLE`'s name when it
     runs the statement. **Measured** on 18.6, A being the rebuild and B
     another session:

     ```text
     A: BEGIN;  -- module_oid resolves m.v to 16389
     B: ALTER VIEW m.v RENAME TO v_old;  CREATE VIEW m.v AS …   -- 16393
     A: LOCK TABLE m.v IN ACCESS EXCLUSIVE MODE;   -- LOCK TABLE
        pg_locks: A holds ACCESS EXCLUSIVE on 16393
     ```

     So the answer said `Serialized::By("the view's own ACCESS EXCLUSIVE
     lock")` while the lock was on the replacement and every read described the
     original — and the `DROP VIEW` that follows goes by name, so it would have
     destroyed the replacement. The same window is one relation over for a
     trigger, measured the same way: rename the parent table away, create a
     replacement, and the lock lands on a table whose trigger is not the one
     the reads are keyed to.

     **The fix is the second resolve, not a second lock.** With the lock held,
     the name is asked once more and has to still mean the same object;
     nothing can move it in between, because a rename needs the lock this
     transaction now has. This is the device `data_triggers::lock_by_oid`
     already uses for the mirror-image problem — there the *oid* is known and
     the name read for the lock, here the name is known and the oid read for
     the reads — and 445 records why it is enough: once the lock is on the
     intended relation, it keeps it that way.

     **Refused, not retried.** The caller is inside the apply's transaction
     (ADR-0009 §3), and what the refusal buys over SPEC §7.6's read-back is not
     safety but a message: §7.6 catches this at the end of the apply, as two
     unrelated-looking surprises — an object appearing that the plan does not
     touch, and a change to one it never approved over — where this says which
     two objects, one statement after the cause. The message names both by oid
     beside `pg_describe_object`'s words for them, because by then they share a
     name and "view m.v" twice would say nothing; an oid that is no longer in
     the catalog is said as that, since an object dropped rather than renamed
     aside is exactly as much of a mismatch.

     The routine arm needs none of this *for the question this entry asks*: its
     lock is `SELECT … FROM pg_proc WHERE p.oid = $1 FOR UPDATE`, taken by oid,
     so there is no name between the resolve and the lock to move, and the lock
     cannot land on the wrong row. That is where this entry stopped, and it is
     only half the question — the `DROP FUNCTION` one statement later still
     goes by name. 503 asks the other half, for every arm, and the routine arm
     does the second resolve too.

<a id="decision-503"></a>

503. **A rebuild's lock pins an object; the `DROP` that follows names a
     qualified name, and the two halves of that name are held by different
     things (issues #547, #548).** 499 closed the window between the module's
     oid resolve and its lock by asking the name again with the lock held. It
     answered the question for the relation arms and stopped one statement
     short of the rest of it: what the plan actually runs next is
     `DROP VIEW <schema>.<view>`, `DROP FUNCTION <schema>.<name>(<args>)` or
     `DROP TRIGGER <name> ON <schema>.<table>` — emitted at plan time and
     pinned by the plan's checksum, so the apply cannot substitute an
     oid-derived identity for it without running a statement the plan does not
     contain (SPEC 14.3). The name is what runs, so the name is what has to
     hold still.

     **The local half holds itself.** Measured on PostgreSQL 18.6: with the
     routine arm's `SELECT … FROM pg_catalog.pg_proc WHERE oid = $1 FOR UPDATE`
     held, another session's `ALTER FUNCTION r.f(int) RENAME TO f_old` is
     `canceling statement due to lock timeout … while updating tuple … in
     relation "pg_proc"`. A rename rewrites the very row the lock holds, so it
     queues behind it exactly as a relation rename queues behind `ACCESS
     EXCLUSIVE`.

     That measurement is why the routine arm now asks 499's question too, and
     why the answer stays `By`. #547 proposed the second resolve but expected
     it to buy only a narrowed window — "no lock the routine arm can take stops
     a rename after the check" — and offered `Serialized::Not` as the honest
     alternative. The engine says otherwise: the row lock does stop it, so the
     second resolve here pins rather than merely checks. What the lock by oid
     already guaranteed was that the lock lands on the right *row*; what it
     never guaranteed was that the *name* still reaches it, and those are two
     different facts about the same statement.

     **The schema half holds nothing.** `ALTER SCHEMA … RENAME` updates a
     `pg_namespace` row and takes nothing at all on the relations or routines
     inside the schema. Measured, with A the rebuild and B another session:

     ```text
     A: BEGIN; LOCK TABLE q.v IN ACCESS EXCLUSIVE MODE;   -- q.v is 152249
     B: ALTER SCHEMA q RENAME TO q_old;                   -- accepted
        CREATE SCHEMA q; CREATE TABLE q.t(i int);
        CREATE VIEW q.v AS SELECT i * 3 AS i FROM q.t;    -- 152257
     A: DROP VIEW q.v;  COMMIT;
        -- 152257 destroyed; 152249, the object A locked and read, survives
     ```

     The routine arm loses the same way one statement later, measured the same
     way: with the `pg_proc` row lock held on 152289, B renames the schema and
     creates a replacement, and A's `DROP FUNCTION m7.f(int)` destroys the
     replacement while 152289 survives under the old schema's new name. Every
     arm has this hole, and it is the same hole.

     **So the schema's own row is locked, first.** `SELECT … FROM
     pg_catalog.pg_namespace WHERE nspname = $1 FOR UPDATE`, before the
     object's lock and before the second resolve — a pin taken afterwards would
     leave open exactly the window the second resolve exists to close. Measured
     on the same server, it costs only what it must: the rename waits, while
     another session's `CREATE TABLE q.other(i int)` is accepted and the
     rebuild's own `DROP`/`CREATE` of both a view and a routine in that schema
     run unaffected. Only a statement that rewrites the schema's own row waits.

     **And it is out of reach for the accounts this tool is built for**, which
     is the routine lock's problem in a second place. Measured as the
     non-superuser owner of the schema, every row-lock strength is `permission
     denied for table pg_namespace` — `FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR
     SHARE` and `FOR KEY SHARE` alike, because a row lock of any strength needs
     `UPDATE` on the table. There is no weaker request to fall back to, so the
     fallback is a sentence, not another lock, and the attempt goes inside a
     savepoint for the reason the routine arm's does: a failed statement dooms
     a PostgreSQL transaction, and this one is *expected* to fail.

     **The fallback says `Not`, and says which half is held.** A rebuild whose
     schema can still move is not serialized in the only sense the answer is
     read for — whether what the `DROP` destroys is what the plan approved —
     even though its reads are perfectly well held. `By` there would answer a
     question nobody asked in the words of the one they did. The message names
     the lock that *is* held beside the one that is not, because either fact
     alone misleads: "not serialized" would deny a lock this transaction holds,
     and naming only the missing one would not say what an operator still has.
     The routine arm says both for the same reason in the other direction: the
     two locks are refused by the same privilege, so that account is usually
     missing both, and the sentence that called the `pg_proc` row lock "the
     only lock that would serialize it" — true until this entry — would now
     read as a claim that the schema half is held.
     SPEC §7.6's read-back remains the backstop it always was — this prevents
     for the accounts that can and reports for the accounts that cannot.

     **Not fixed here: the savepoint the routine arm leaves standing.** The new
     pin releases its marker on both paths; the routine lock beside it rolls
     back to its own and does not, which is issue #534 and stays there. Two
     savepoint names, two owners.

<a id="dec-314-1"></a>

**DEC-314.1. A PostgreSQL plan puts every dependent of a module it drops on
the right side of that drop, and the apply refuses a plan that no longer does
(#314).** Every module change on this engine is a drop and a create (ADR-0009
§3), and the engine refuses the `DROP` while anything depends on the module.
ADR-0009 §4 said so, and `modules::dependents`, `unmanaged_refusal` and
`to_rebuild` were written for it, but nothing called them. So a function edit
with a check constraint on it was an applyable plan that predictably failed
(SPEC §7.5).

`plan --db` now reads the dependents of every module the plan drops, whether
it is rebuilt or dropped for good, inside its planning transaction and before
`before_a_rebuild`. Each dependent is removed before the drop, and one the
declarations keep is restored after the create.

- **Moved, not duplicated.** A change the plan already has is moved rather than
  added again: a view the declarations drop goes before the function under it,
  and a view they edit is split into its drop and its create, one on each side.
- **Synthesized from the declarations.** What the plan lacks is built from the
  declarations, the only place pbps can put an object back from. A declared
  module the plan does not touch is handed back to the differ to rebuild
  (`pbps_diff::diff_rebuilding`), as `rebound_modules` does (DECISIONS 422).
  A rebuild is more than its two statements: the grants it takes with it and
  the `PUBLIC` execute it gets back come from the differ's permission passes,
  and a drop-and-create pair added after those passes ran would restore
  neither. `before_a_rebuild` would then refuse the rebuild.
- **Taken away with its owner.** A dependent whose table or column the plan
  drops before the module's drop is already removed. The differ sorts
  `DropColumn` and `DropTable` ahead of `AlterModule`. Nothing is synthesized
  for it, because a `DropCheck` written after its table is gone would fail.
- **Placed next to the module.** The changes are inserted beside the module
  rather than left to the differ's order. A `DropCheck` sorts after a
  `DropModule`, and an `AddCheck` sorts before a `CreateModule`, so the order
  that works everywhere else is exactly wrong here.
- **Reversed for the creates.** Removals go in `dependents`' order, deepest
  first, and restorations in its reverse (DECISIONS 311).
- **Refused by name.** A dependent the model cannot represent, one the project
  does not declare and the plan does not remove, and a declared dependent of a
  module dropped for good all refuse the plan. `CASCADE` is not offered (SPEC
  14.3).

A synthesized change carries the dialect's own risks for its kind, so a view
dropped to be rebuilt asks for `--allow destructive`. Saved-plan verification
recomputes each change's risks from the change and would refuse any other
answer, and the approver is in fact approving a `DROP VIEW`. Restoring a check
asks for `constraint` for the same reason: it revalidates the table.

The apply asks only whether the saved plan still removes, before each module's
drop, everything that depends on it now. A dependent created after planning is
one the approver never saw, so the plan is refused rather than extended. A
staged apply runs outside a transaction and cannot ask this. It is covered at
planning: staged plans already refuse rebuilds, and a plain drop's dependents
are put in order before the plan is saved.

<a id="dec-322-1"></a>

**DEC-322.1. A `SECURITY DEFINER` routine a plan writes must set
`search_path` with `pg_temp` last, checked from the stored `proconfig` inside
the transaction that wrote it (#322).** A definer routine runs with its
owner's privileges. An unqualified name in its body binds when a caller
invokes it, through the `search_path` in force then, which is the caller's
unless the routine pins its own. `pg_temp` is searched first unless the path
names it. So a caller who can create in any schema on their own path, or in
their own temporary schema, can supply the helper that the owner's privileges
then run. PostgreSQL's manual prescribes the remedy under "Writing SECURITY
DEFINER Functions Safely": a `SET search_path` on the routine, with `pg_temp`
named and last. That form is required, and nothing weaker.

*Why the stored setting and not the declaration.* The declaration is the
user's text, passed to the engine verbatim (`create_module`). Finding
`SECURITY DEFINER` and a `SET` clause in it would mean parsing a header this
tool never parses (DECISIONS 517 gives the same reason for closing every
routine to `PUBLIC`). The engine's own answer is `prosecdef` and `proconfig`
after the `CREATE`. The check therefore runs inside the transaction that
wrote the routine, and a refusal rolls it back:
- a transactional apply, after its statements;
- `bootstrap`, after its statements;
- a staged run, in a transaction of its own for each transactional step of a
  plan that writes a routine, so that nothing unsafe commits.

A path value is split as the engine writes it, so a quoted schema holding a
comma is one entry, and `"pg_temp"` and `pg_temp` are the same.

*Why not pin the bindings instead.* #322 asked for either this refusal or
pinning a definer's external dependencies. A binding is made at run time, long
after `apply` has checked anything. DEC-319.1 pins what an apply can observe,
and a caller's later path is not one of those things. The maintainer chose
the refusal (2026-09-25).

*What it does not judge.*
- Whether a role other than the owner can create in a schema the pinned path
  names. That would re-derive the engine's authorization (DECISIONS 521), and
  it is #1004.
- A definer routine the plan does not write. One already in the database in
  the unsafe form is not this plan's change, and refusing an unrelated plan
  for it would refuse a valid plan.
- Transitive helpers. A helper that a definer calls is resolved on the
  definer's pinned path, which is what this entry fixes. A helper that is itself
  a definer is held to the same form only when the plan writes it.


<a id="dec-942-1"></a>

**DEC-942.1. When a PostgreSQL plan rebuilds a function, what the plan itself
adds that can call one goes after the last function it creates (#942).** DEC-314.1
places what the catalog says depends on a dropped module. An addition this plan
makes is not in the catalog yet, so that pass never sees it. The differ puts a
check or an index in class 13 and a default in class 9, ahead of every module
in class 14. A check calling the rebuilt function was therefore created against
the old one, and the rebuild's `DROP FUNCTION` was refused because of it.

Which function an expression calls cannot be known without parsing it, and the
planner does not parse expressions (DECISIONS 174). So the rule is positional
rather than per call. When the plan rebuilds a function, these move after the
last function the plan creates, keeping their order. A routine counts as a
rebuilt function when the plan drops and creates it and either side is a
function. A procedure that becomes a function counts (#1024), and so does a
function that becomes a procedure, since the same revision may create another
function a new check calls (#1047). What moves:

- every check;
- every index with a filter, since an index's columns are names and its filter
  is the only place a call can be;
- every default being set.

A unique index with no filter stays where it is: it holds no expression, and a
foreign key in its class may rest on it.

Two shapes are left to the engine, and they fail loudly: the apply is refused
and rolls back.

- **A default on a table whose rows the plan writes.** Row writes come before
  the modules, and a row inserted before the move would take the old default,
  which records rows the declarations did not ask for.
- **A column added with a default that calls the function.** The column has to
  exist before any module that reads it.

A plan that rebuilds no function keeps the differ's order. Measured on
PostgreSQL: a function edit together with a new check, filtered index and
default calling it applies, `verify` is clean, and planning again reports no
changes. Before the move, the plan put the check ahead of `CREATE FUNCTION`.
