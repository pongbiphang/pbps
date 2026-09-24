# Dependencies and rename impact

Finding what depends on an object and what a rename or drop would break. Part of
the [decision record](../DECISIONS.md), which says how to add an entry here.

<a id="decision-245"></a>

245. **The dependency scan folds case for the whole alphabet, character for
    character, and is knowingly wider than the collation in three places.**
    `references` lower-cased both sides with `to_ascii_lowercase`. The two
    sides agreed with each other, so ASCII names were right; they did not
    agree with SQL Server, whose collations fold the rest of the alphabet too.
    Measured on SQL Server 2022 under `SQL_Latin1_General_CP1_CI_AS`,
    `Latin1_General_CI_AS` and `Latin1_General_100_CI_AS_SC` alike, and
    confirmed by creating each object under one spelling and selecting it
    under the other: `CAFÉ` and `café` are one table, and so are `Σum` and
    `σum`. The ASCII fold saw two names, found no edge, and let
    `creation_order` place a view before the table it reads — the failure of
    239 from another cause.

    The fold is `char`-by-`char` simple lower-casing rather than
    `str::to_lowercase` because full lower-casing may return more characters
    than it was given. `İ` (U+0130) becomes `i` plus a combining dot, and the
    engine does not read that as `i` — measured unequal, and the object did
    not resolve. The combining dot is not an identifier character, so
    `contains_word` finds a word boundary in the middle of what was one
    letter: with `to_lowercase`, `SELECT * FROM dbo.İ` is a reference to
    `dbo.i`, measured and pinned. One character in and one character out is
    also what a collation does, which is the comparison being approximated.

    The fold and the engine do not agree everywhere, and cannot be made to.
    Every single-character lower-case mapping in the BMP — 1180 of them — was
    put to SQL Server 2022: 216 are pairs the fold reads as one letter and at
    least one of the three collations does not, and for 149 of those the three
    collations disagree with *each other*. The Kelvin sign is not `k` and the
    Ohm sign is not `ω` under any of them, while U+212B is `å` under all
    three. There is no offline rule that gets this right, because the right
    answer is a property of the database and the loader has none to ask
    (§8.2). The fold is an approximation, and a much closer one than ASCII: it
    agrees on 964 of the 1180.

    What that costs is bounded by a question the loader *can* answer: did the
    fold's answer make a cycle? A fold is wider than a collation or equal to
    it and never narrower, so what it gets wrong it gets wrong by saying
    *yes* too often — asked whether a definition names `dbo.CAFÉ` it says yes
    to `dbo.café` as well, and two such over-answers make an ordering cycle
    out of modules that have none. A missing edge is invisible to
    `creation_order`; a false one is not, because a false one is exactly what
    stops Kahn's algorithm. So the order is taken with the whole-alphabet
    fold, whatever it leaves unplaced is re-scanned under the ASCII fold —
    the one every case-insensitive collation performs — and what is still
    unplaced is re-scanned under no fold at all, which only a case-sensitive
    database needs. A cycle no comparison separates is emitted in name order,
    as it was before.

    Narrowing where a cycle appeared and nowhere else is what keeps the price
    proportionate. An over-answer that merely orders two modules more
    strictly than the engine would have costs nothing — the `CREATE` still
    runs after everything it reads — and it is left alone. Only the pair
    whose over-answers closed a loop pays, and it pays with the widest
    comparison that opens the loop again, so a plan is never ordered by a
    narrower fold than its own evidence calls for.

    The obvious alternative — decide a comparison per name from the
    declarations, before the scan, wherever two of them fold together — was
    written first and gave up more than it bought. Two measured reasons. It
    cannot see a collision with a name it is not ordering: `creation_order`
    is given the modules, the tables are an earlier ordering class, and a
    view `dbo.ktbl` beside a table `dbo.Ktbl` is a pair no map of its
    arguments holds. And it narrows a name in every position because one
    position collided: `dbo.t` beside `sales.T` is one bare name in two
    spellings and two qualified names in one spelling each — a pair a
    case-insensitive database holds without complaint — and comparing the
    qualified form exactly loses the edge from any definition writing
    `WAREHOUSE.T`, which is 239's failure reached through the guard meant to
    prevent it. A cycle is later evidence than a collision, but it is
    evidence about the answer rather than about the question.

    One price is left, and it has `depends_on:` for an escape hatch. Where a
    cycle is broken, the edge dropped is the one the narrower comparison does
    not find, and on a case-insensitive database that edge may have been
    real — two declarations the engine reads as one name are a schema this
    tool cannot order correctly in any case, and the narrowing picks the
    spelling rather than the meaning. Declaring the dependency says what the
    scan cannot read.

<a id="decision-314"></a>

314. **`pg_depend` holds a row per column a dependent uses, not a row per
    dependent.** Measured, a routine reading three columns of a view has three
    edges to it:

    ```text
    dependent | refobjsubid | edges
    mq.uses() |     1       |   3
    mq.uses() |     2       |   3
    mq.uses() |     3       |   3
    ```

    Where a query returns one row per edge and the reader deduplicates by name,
    that is harmless. Where a query *joins* on the edge, it is not: the
    argument query cross-joined `unnest(proargtypes)` once per edge and rebuilt
    a one-argument routine as `f(integer,integer,integer)` — an identity no
    declaration holds, so an otherwise manageable rebuild was refused and the
    walk could not resolve the object it had just named.

    So the edge is deduplicated before the join, and the dependents query is
    `DISTINCT` over its whole union as well. The second was already harmless —
    the caller deduplicates by description — and it is done anyway: a reader
    that has to remember to deduplicate is one edit away from not doing it.

    **Duplication that repeats a row is a tidiness problem; duplication that
    feeds a join is a wrong value.** The two look the same in the query and
    nothing distinguishes them but knowing what the rows are for.

<a id="decision-315"></a>

315. **The dependency scan lexes with the dialect's rules.** `creation_order`
    reads what is left of a definition after literals and comments are
    blanked, and the shared `code_only` blanked them by one engine's rules:
    `[…]` a quoted identifier, `'…'` closed by the next single quote, no
    `E'…'`, no `$tag$…$tag$`. Measured, `CREATE VIEW es.b AS SELECT E'x\' ,
    es.a' AS s` is one literal to PostgreSQL; the shared scanner closed it at
    the `\'`, read `, es.a` as code, and drew an edge from `b` to `a`. With
    `a` selecting from `b` that edge closed a cycle, the members were emitted
    in name order, `a` came first, and its `CREATE VIEW` failed inside the
    plan's transaction — a valid plan refused, with no `depends_on:` able to
    remove the edge that refused it.

    The `Lexicon` a dialect already supplies for the comparison scanner
    (ADR-0011 Amendment 2) is what decides where a region ends, so it now
    supplies `code_only` too, and `Dialect::code_only` hands it to
    `creation_order_with` and to the emitter's name scans. Not a second table
    of delimiters in the model: the model keeps its `code_only` for the
    callers that have no dialect, and the differ, which has one, lexes with
    it.

    A dollar-quoted string is a literal to this scan too, blanked, with one
    exception: a routine's body. On this engine the body is itself one — `AS
    $$ SELECT app.f(1) $$` — and the name scans exist to read what the body
    says; blanked, every routine would call nothing and depend on nothing. A
    dollar-quoted *datum* is not the body, and reading it as code drew an
    edge from the view that holds it to the view it names — with the other
    direction real, a cycle, and the dependent created first. What tells the
    two apart is the word before the string: a body follows `AS`, and a datum
    never does — measured, `CREATE VIEW v AS SELECT 1 AS $x$` is a syntax
    error, so in a definition the engine accepts a dollar-quoted string after
    `AS` can only be a body. The body is lexed by the same rules, its own
    literals, comments and dollar-quoted data blanked. A comment between the
    keyword and the body is already blank by the time the question is asked:
    measured, `AS /* c */ $$ SELECT 1 $$` is a body. The body's tags are
    blanked with the literals: a delimiter is not a name, and one read as
    code was a mention of a module named `$a$` from inside every routine it
    delimited. And the gap before the
    string is the dialect's: a type named `as\u{a0}` applied to a string —
    measured, `SELECT as\u{a0} $$app.a$$` is a valid view — is not the
    keyword, and the trim that looks for it discards only whitespace the
    dialect does not count as a name byte. The body written as a plain `'…'`
    literal stays blanked, which is #228.

    **Amended: the boundaries and the prefixes are the dialect's too.** A
    literal's prefix is part of its token — measured, `N'x'`, `B'101'`,
    `X'1F'`, `U&'d\0061ta'` and `E'y'` are literals, while `note'x'` is the
    type `note` applied to a string — so `Lexicon` names the prefixes and
    `code_only` blanks one with its literal; left as code, the `E` matched a
    view named `e`. And where a word ends is the engine's rule: every
    non-ASCII byte continues an identifier on PostgreSQL, so `x\u{a0}y` is
    one alias, and the scan that read the byte as a gap found a word `y` in
    it. The model's `Lexis` carries both the dialect's `code_only` and its
    identifier rule; the differ and the emitter's name scans hand it the
    dialect's, and the shared scanner keeps SQL Server's as its own.

    **Amended: a doubled quote does not cost a literal its prefix.** The
    scanner closed a `U&'…'` at the first quote of a doubled pair and reopened
    an ordinary literal, which then had no prefix and swallowed no clause.
    Measured, `U&'a''b' uescape '!'` is the string `a'b` — one literal, one
    clause — and the word left as code matched a module named `uescape`. A
    doubled quote is now read as a quote *inside* the literal, which is what
    the engine reads it as.

    **Amended: a prefix letter is a prefix only where a name does not end
    there.** The emitter's own scan guarded `$` against opening a
    dollar-quoted literal in the middle of a name and guarded nothing else.
    Measured, with a domain `dq.code`, `CREATE FUNCTION dq.f(a dq.code
    DEFAULT dq.code'x\', b integer DEFAULT 1)` is accepted and the default
    reads back as `'x\'::text` — the type applied to the plain string `x\`.
    Read from the `e'`, the scan took an escape string, the `\'` did not
    close it, the rest of the declaration was swallowed, and the gate refused
    a routine the engine creates under exactly the declared key. The quote
    itself still opens a plain literal there, which is the engine's reading of
    `note'x'` and the rule the shared lexer already had.

    **Amended: a `UESCAPE` clause goes with the literal it follows.** Measured,
    `U&'d!0061ta' UESCAPE '!'` is the string `data`, and the clause is part of
    that token. The scan blanked the literal and left the word `UESCAPE` as
    code, where it matched a module named `uescape`; with that module selecting
    from the view holding the literal, the invented edge closed a cycle and the
    dependent was created first. `code_only` remembers that a literal opened
    with the `u&` prefix and blanks the clause with it — the escape character
    itself changes nothing, because the contents are blanked either way.

    **Amended: a Unicode-escaped identifier is read as the name it spells.**
    Measured, `SELECT * FROM dq.U&"\007a"` and `FROM U&"dq".U&"!007a"
    UESCAPE '!'` both select from `dq.z`; the scan read the spelling on the
    page, found no `z` in it, and with `a` sorting first created `a` over a
    view that did not exist yet. `Lexicon` says whether the engine has the
    form, and `code_only` replaces `U&"…"` and its clause with the quoted
    name they decode to, padded so that offsets survive; a spelling that does
    not decode is left as it is, for the engine to refuse. SQL Server has no
    such form.

<a id="decision-316"></a>

316. **A bare reserved word is not a reference.** The name scans read a bare
    word as a possible mention of a module of that name, because a
    definition written inside its own schema very often omits the qualifier.
    A view named `select` made every other view mention it: `select 1` drew
    an edge from `z` to `select`, `select` selecting from `z` drew the real
    one back, and the cycle was broken by name order, which created `select`
    over a view that did not exist yet — a valid plan refused, with no
    `depends_on:` able to remove an edge.

    A word the engine refuses as a bare name cannot be one. **Measured** on
    PostgreSQL 18.6, with a table, a function and a type of each name: every
    word of `pg_get_keywords() WHERE catcode = 'R'` is refused as `FROM word`,
    `word(1)` and `::word`, except `current_catalog`, `current_date`,
    `current_role`, `current_time`, `current_timestamp`, `current_user`,
    `localtime`, `localtimestamp`, `session_user`, `system_user` and `user`,
    which `FROM word` accepts; the type-or-function-name category is not
    reserved in this sense (`FROM between`, `FROM join` are accepted); and
    after a dot any word is a name (`FROM app.select` is accepted). On SQL
    Server 2022, every documented reserved keyword is refused as `FROM word`,
    and as `FROM dbo.word` all but `disk`, `dump`, `load`, `precision` and
    `securityaudit`.

    So `Lexicon` carries `reserved`, each dialect's measured table, and the
    scan matches a reserved name only in its quoted spelling — `"select"`,
    `[select]` — or qualified, which is a name whatever the word. The shared
    scanner reserves nothing: the loader has no engine to ask, and an edge it
    draws too many of is one the differ, which has one, does not draw. The
    table is read from the engine, not from memory, and a word a later engine
    reserves is a bare mention the scan still reads — an edge too many, in the
    direction the scan has always erred.

<a id="decision-317"></a>

317. **A bare name is a reference only where the engine would look it up.**
    The scan read a bare word as a mention of a same-named module in *any*
    schema. With no extras, `a.x AS SELECT * FROM b.z` over `b.z AS SELECT 1
    AS x` is a valid plan, and the alias `x` in `b.z` was read as a mention
    of `a.x`: a cycle with the real edge, broken by name order, which
    created `a.x` over a view that did not exist yet.

    **Measured.** On PostgreSQL a bare name in a definition resolves through
    the write path every statement runs under — the object's own schema and
    the configured extras (276) — and nowhere else. On SQL Server 2022, with
    `b.z` and `dbo.z` both present, `CREATE VIEW a.x AS SELECT * FROM z`
    reads `dbo.z`; with `a.z` present it reads `a.z`; with only `b.z` it is
    refused. So `Dialect::resolves_bare_name` says which schemas a bare name
    in a definition in a given schema may resolve in — PostgreSQL's own and
    extras, SQL Server's own and `dbo` — and `creation_order_with` reads the
    bare form only for a candidate in one of them; the qualified form is an
    edge wherever it points. SQL Server's second step is the caller's default
    schema, which is `dbo` for a login given no other; a deployer whose
    default schema is another one says the edge with `depends_on:`, which
    can add an edge and never has to remove one. The shared scanner, and the
    emitter's mention scans (ADR-0013 §3), keep reading a bare name
    everywhere: a report and a rebind check are over-inclusive by design.

    **Amended: the path is ordered, so the answer is a rank and not a yes.**
    The engine resolves a bare name in the *first* entry of the path that
    holds one. Measured, with `z.p` and `a.p` both present, a bare `p` binds
    `z.p` under `SET search_path = "z", "a"` and `a.p` under `"a", "z"`; on
    SQL Server 2022, with `dbo.p` and `a.p` both present, `CREATE VIEW a.x AS
    SELECT * FROM p` reads `a.p`, so the own schema outranks `dbo`. Read as
    two candidates, a bare `p` in `z.x` drew an edge to `a.p` as well as to
    `z.p`; with `a.p` selecting from `z.x` that closed a cycle, and name order
    created `a.p` first. `Dialect::bare_name_rank` gives the position on the
    path, and among the declared modules sharing a bare name only the
    best-placed one takes the bare form.

<a id="decision-318"></a>

318. **A quoting character doubled inside a name is one character of it.**
    The name scans dropped every `"` from the lexed definition, so
    `app."z""q"` read as `app.zq` while the candidate's own name was `z"q`:
    the needle matched nothing, no edge was drawn, and with the dependent
    sorting first its `CREATE` came before the view it selects from — a valid
    plan refused. Measured, `CREATE VIEW dq."z""q"` names the view `z"q` and
    a view over it is written `FROM dq."z""q"`; `[a]]b]` is the same rule one
    engine over, and the emitters have always written a name that way
    (`quote` doubles what it must). The unquoting keeps a doubled delimiter as
    the single character it stands for, and the needle built from the declared
    name carries it too.

<a id="decision-394"></a>

394. **What a rename breaks on this engine is invisible to the dependency
    graph, and what the graph holds is what survives.** The SQL Server module
    of the same name reads `sys.sql_expression_dependencies` and reports what
    it finds, because that engine stores module text. **Measured on 18.6, this
    engine is the exact inverse**: `pg_depend` holds one edge from a
    `BEGIN ATOMIC` SQL function to the table it reads and **zero** from a
    `plpgsql` function to the same table. After
    `ALTER TABLE customer RENAME COLUMN email TO contact_email` the view reads
    `contact_email AS email`, the atomic body reads
    `customer.contact_email AS email`, the check reads `total >= 0`, the
    generated column reads `upper(ident)` and the row-level policy reads
    `contact_email <> 'blocked'` — while the `plpgsql` body still reads
    `email` and fails the next time anybody calls it, `column "email" does not
    exist`. A report that queried `pg_depend` and stopped would list every
    object that is fine and no object that is broken, which is worse than no
    report because it looks like one. So the advisory list is a **name scan
    over the bodies the engine never parsed**, found by `prosqlbody IS NULL` —
    the engine's own record of which bodies it parsed, and not a language
    list, because measured, `sql` appears on both sides of that line.

<a id="decision-395"></a>

395. **The objects a rename is carried into are reported, as their own list.**
    Three answers need three lists: what breaks, what the engine refuses, and
    what follows the rename and keeps working. The third is half of "what does
    this rename affect", it is the half this engine is better at, and an
    operator who cannot see it has to assume the worst about every view in the
    database. One item of it is worth saying out loud — **measured**, a view
    keeps its *old output column name* as an alias, so the view's own consumers
    see no change at all. Putting those objects in `advisory` instead was ruled
    out: advisory means "this will break", and a report that flags what is fine
    is one people learn to override.

<a id="decision-396"></a>

396. **Nothing blocks a rename on this engine, and the empty list says so.**
    **Measured**: a column a view depends on renames without complaint, while
    `DROP COLUMN` on the same column is `cannot drop column ident of table t
    because other objects depend on it`. `ImpactReport::blocking` is kept
    because the abstraction has it and the drop side is real, and the module
    documentation says why nothing fills it — an empty field a reader has to
    guess about is a query that might have failed.

<a id="decision-397"></a>

397. **A module drop is not asked about here.** A module rename reaches the
    plan as a drop plus a create (ADR-0002), and what the catalog holds against
    a module about to be dropped is `crate::modules`' question, answered there
    in full — every reverse `pg_depend` edge, the classes with no rule, the
    cycle, and what a rebuild cannot carry (DECISIONS 306). Asking it a second
    time in `impact` would be a second implementation of one question, and the
    two would disagree the first time either was fixed. `RenameTarget` has two
    arms here where the SQL Server one has three.

<a id="decision-398"></a>

398. **A column the catalog does not have is an error, not an empty report.**
    An unknown column would otherwise produce a report with nothing in it,
    which reads as "nothing breaks" — the one answer that must never arrive by
    accident. And the lookup excludes `attisdropped`: a dropped column keeps
    its slot with a placeholder name (ADR-0012 §6), so the slot is not a column
    anybody can rename and must not answer as one.

<a id="decision-407"></a>

407. **A column a plan renames is named to the catalog with *both* halves taken
    back.** A `RenameColumn` carries the declared, post-rename **table**:
    `pbps-diff`'s `order_key` gives it a class of its own after the table
    renames precisely because the statement it becomes names the table and must
    run second. So a plan that renames `app.client` to `app.customer` and its
    `email` to `contact_email` describes the column as `app.customer.email`,
    and `impact::rename_impact`, which runs before any statement, looked up a
    table the catalog does not have yet and answered `ImpactError::Name` — a
    refusal of a plan the engine would accept. `RenameTarget::from_changes`
    therefore builds the table map over the whole plan first and translates the
    column's table through it, the same translation `preflight::AsStored` makes
    one rank further on. Built over the whole plan rather than as it walks,
    because the order that puts the table rename first is `order_key`'s
    guarantee and not this list's to lean on.

<a id="decision-408"></a>

408. **A scan for an identifier steps by a character, not a byte.** `mentions`
    walks a routine body looking for the renamed name bounded by non-identifier
    characters, and stepped past a rejected match by one byte. Identifiers here
    are not ASCII: `is_ident_byte` counts every non-ASCII byte as part of a
    name, deliberately, so a column may be named `ä`. Scanning `xä` for `ä`
    finds it at byte 1, rejects it because `x` precedes it, and a one-byte step
    lands inside the two bytes `ä` occupies — where `body[from..]` panics,
    because Rust will not slice a string off a character boundary. The step is
    the width of the name's first character, and an empty name returns `false`
    before the loop rather than matching at every position.

<a id="decision-416"></a>

416. **SQL Server rename impact takes both halves of a column name back to the
    catalog's spelling.** `RenameColumn` carries the declared, post-rename
    table because its statement runs after `RenameTable`; impact runs before
    either statement. A plan renaming `dbo.client` to `dbo.customer` and
    `email` to `contact_email` therefore described its impact target as
    `dbo.customer.email`. `OBJECT_ID` returned NULL for that not-yet-existing
    table, every dependency query joined against NULL, and even a
    SCHEMABINDING view that blocks the rename came back as an empty report.

    `RenameTarget::from_changes` builds the complete table-rename map first and
    translates a column target through it. It does not rely on change order:
    putting table renames before column renames is the emitter's concern, while
    this code's concern is the catalog state before the plan begins. This is
    the SQL Server instance of DECISIONS 407; unlike PostgreSQL's loud missing-
    name error, SQL Server's NULL lookup made the wrong answer look clean.

<a id="decision-431"></a>

431. **Table and column drop impact follows catalog object addresses and removal order.**
    PostgreSQL's `pg_depend` has several edges for one object: a table CHECK
    can depend both normally and automatically on the column it checks.
    `impact::drop_blockers` walks reverse edges and internal owners, preserving
    view rules, row types, domain constraints and catalog classes with no
    hand-written classifier. Automatic removal wins over a normal edge for the
    same dependent. An earlier typed removal also removes its automatic parts,
    so dropping a child table first clears its foreign key into the parent.
    Dependencies of a removed dependent must themselves be removed in time;
    merely appearing somewhere in the plan does not establish that order.

    The read sees the current catalog. Earlier table/column renames project
    statement-time names back to stored identities; an earlier creation is
    distinguished from a missing existing target, which is an error. Module
    replacements and default/key replacements remove their old dependencies;
    dependencies introduced by new definitions remain the engine's execution
    check. No speculative DDL, CASCADE, extra privilege-demanding locks or
    stronger concurrency guarantee is introduced by this reader.

    The engine facade runs the check inside connected planning and again before
    apply writes, including pending staged statements. A closing-only staged
    resume has no remaining drop to inspect. Blockers name the target and its
    dependents; successful connected reports name the check. SQL Server names
    its missing table/column reader as unavailable instead of borrowing the
    rename reader's answer. Live tests pair the report with DROP RESTRICT's
    refusal and with valid earlier removal; CLI tests pin refusal before a
    saved artifact or DDL; failed applies retain their ordinary failed-attempt
    audit entry without recording a successful deployment (#254, #305).

<a id="decision-448"></a>

448. **The rename-impact text-body scan folds an unquoted mention, never a
     quoted one, and only when the target's own name could have come from an
     unquoted spelling.** 230 and 313 already settle *which* fold this engine
     uses on an unquoted identifier — ASCII, byte by byte, high bit untouched;
     measured, `CREATE TABLE AÄ` makes the relation `aÄ`, not `aä`. They do not
     settle *when* `impact::mentions` should fold at all, which is this entry.

     A quoted identifier is stored exactly as written, so `"EMAIL"` and
     `email` are two different columns; folding the quoted spelling too would
     report a routine that does not actually break, which is the finding
     #260 was opened over. So the fold runs on the bare scan only, and the
     `"name"` exact-quoted check stays unfolded beside it.

     A target whose own catalog name still carries an ASCII uppercase letter
     — `Email`, not `email` — got that name from being created quoted: 230's
     `aÄ` measurement shows an unquoted spelling can leave a *non-ASCII*
     uppercase letter in place, but never an ASCII one, since the engine
     downcases exactly that range. So no unquoted spelling in a body could
     ever refer to such a target, and a bare mention that happens to match it
     case-insensitively is always naming a different, lower-spelled column.
     The scan skips the fold entirely for such a target rather than run it and
     rely on the quoted check to save it — a filter whose reason has gone is
     one nobody re-reads, and the next person to touch this code would have
     no way to tell a defensive skip from a forgotten one.

     **Amended: the SQL prefilter's fold has to be pinned to `COLLATE "C"`,
     not left to the database's default.** `prosrc` is plain `text`, so an
     unqualified `lower()` in `TEXT_BODIED_ROUTINES` ran under whatever
     collation the database was created with — and on one with Turkish
     casing rules that is measurably a different fold than the engine's own
     identifier rule:

     ```text
     lower('I')                                                        -> i
     lower('I' COLLATE "tr-TR-x-icu")                                  -> ı
     lower('I' COLLATE "C")                                            -> i
     strpos(lower('SELECT I FROM t' COLLATE "tr-TR-x-icu"),
            lower('i' COLLATE "tr-TR-x-icu"))                          -> 0
     strpos(lower('SELECT I FROM t' COLLATE "C"),
            lower('i' COLLATE "C"))                                    -> 8
     ```

     A routine on such a database naming the target with a bare, differently
     cased ASCII letter was excluded by the prefilter before the ASCII-correct
     Rust scan ever saw it — the exact silence #260 exists to remove, produced
     by the fix meant to remove it. The first cut of this entry called the
     unqualified `lower()` a "safe superset" and reasoned that any fold is
     wider than none; that assumed the fold was locale-independent, and
     measured, it is not.

     `COLLATE "C"` on both `strpos` arguments is what fixes it: `C` folds
     ASCII only and never depends on the database's locale, which is *exactly*
     the engine's own identifier fold (230, 313) rather than an approximation
     of it. That makes the earlier "keep the exact quoted test beside the
     folded one, in case the fold loses a real match" hedge pointless rather
     than merely redundant: an ASCII, per-byte fold cannot turn a string that
     contains `$1` into one that does not, for any `$1` or body, so the
     folded test is now a superset by construction and not by hope, and there
     is nothing left for a second test to catch. The exact-quoted `strpos`
     test is removed rather than kept — a filter whose reason has gone is one
     nobody re-reads, the same rule this entry already applied to the
     mixed-case-target skip above, and keeping a now-pointless test beside a
     provably sufficient one would only invite the next reader to wonder what
     it was for. 245's Greek-final-sigma counterexample was never wrong; it
     was the reason for pinning the collation, not for keeping a redundant
     test beside an unpinned fold.

     **Amended again: `"C"`, unqualified, is a name and not a fact — it
     resolves through `search_path` like any other identifier, and a schema
     earlier on the path can hold its own collation named `"C"`.** Measured:

     ```text
     CREATE SCHEMA shad;
     CREATE COLLATION shad."C" (provider = icu, locale = 'tr-TR', deterministic = false);
     SET search_path = shad, pg_catalog;
     SELECT lower('I' COLLATE "C");               -- ı      <- hijacked
     SELECT lower('I' COLLATE pg_catalog."C");    -- i      <- pinned
     ```

     With a shadow collation on the path, `COLLATE "C"` folds `I` to `ı`
     again — the identical defect this entry exists to fix, one schema-lookup
     away. The "superset by construction" claim two paragraphs up was true of
     the *fold*, ASCII versus Unicode, and silently assumed the collation
     named `"C"` was always `pg_catalog`'s; under a hostile or merely unusual
     `search_path` it is not, and the claim was false until the name was
     qualified. `pg_catalog."C"` cannot be shadowed by anything on the path,
     and it matches every other name in this query, all of them already
     schema-qualified (`pg_catalog.strpos`, `pg_catalog.lower`,
     `pg_catalog.pg_proc`, `pg_catalog.pg_depend`) — the collation was the one
     unqualified name in a query whose whole style is that nothing resolves
     through the path, and it is now qualified the same way. The unit
     assertion pinning this SQL literal was tightened to require the
     qualified spelling: the unqualified one would have passed it.

     The scan's character-width stepping (408) needed no change either way:
     ASCII-only folding never changes a string's byte length or its char
     boundaries, so the same stepping rule runs unchanged on the folded body.

<a id="decision-477"></a>

477. **PostgreSQL rename-impact advisories match complete identifier tokens
     after masking literals and comments.** A raw text match reports a
     reserved keyword such as `SELECT` as the column `select`, a fragment of
     `"other EMAIL"` as `email`, and literal or comment contents as references
     (issues #421, #425, #427). The dialect's existing `code_only` scanner
     removes data regions; quoted tokens compare exactly using the emitter's
     escaping, and bare tokens retain 448's ASCII-only fold. The existing
     reserved-word rule applies to bare names but permits a name after a dot.
     Measured on PostgreSQL 18.6, `t.select` names the quoted column `select`.

     This supersedes 448's SQL substring prefilter: even its locale-independent
     fold cannot find catalog name `a"b` in the stored spelling `"a""b"`
     (issue #264). Read eligible text bodies without a name predicate and
     apply the lexical rule once in Rust. Maintaining a second SQL lexer or
     spelling approximation would create another way to silently lose a body.
     Catalog, internal-language and extension exclusions remain in the query.
     Live regressions read `prosrc`, compare actual rename failures with the
     advisory list, and retain unaffected routines as negative cases.

     This is still SPEC 7.4's advisory name scan: it does not resolve which
     same-named object a routine uses or interpret dynamically assembled SQL.
     No plan gate, recorded state or dependency ordering changes.

<a id="decision-519"></a>

519. **A rename impact report resolves its relation once, and an absent one is
     a refusal rather than an empty report.** `rename_impact` held two
     positions on the same question. A **column** target resolved its `attnum`
     first and refused outright when the catalog did not have it — "a question
     that could not be asked, and the caller has to hear it as one". A
     **table** target skipped that step: `attnum` was 0, every query's
     `to_regclass` answered NULL, all of them joined to nothing, and the
     operator was told a rename affects nothing about a rename that could not
     be evaluated at all. The comment beside them argued the opposite of the
     column path, that "a raise in the middle of an impact report is an error
     where the honest answer is an empty list".

     The column path is the one kept. Absent, empty and unreadable are three
     different things, and a `RenameTable`'s `from` is by construction a name
     the catalog held when the plan was made — so its absence is not "nothing
     depends on this", it is evidence that something happened to the object
     between the plan and the connection.

     `to_regclass` is still the right function, for a reason the module did not
     give before: a `::regclass` cast *raises*, and a raise arrives at a caller
     as `ImpactError::Query`, which says a query could not be run. That is a
     third answer again, and it is not the true one. The NULL is read in Rust
     and turned into `ImpactError::Name` in this module's own words.

     Resolved **once**, and the oid — not the name — is what the other three
     queries take. That is what makes the failure unrepresentable rather than
     merely checked: no later query can be handed a name that resolves to
     nothing, and a relation dropped and recreated between two of them cannot
     make half a report about one object and half about another. The oid
     travels as `int8`, because an oid is unsigned 32-bit and measured on 18.6
     `4000000000::oid::int4` is `-294967296` while `::int8` is exact; the
     comparisons spell `($1::int8)::oid` so the engine infers the parameter as
     `int8`, and measured, they keep their index scans. `ATTNUM` loses its own
     existence question with this: an empty result there now means the column
     is absent and nothing else (#269).
