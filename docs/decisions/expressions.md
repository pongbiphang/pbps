# Expressions and defaults

Scanning, normalizing and comparing default and check expressions the way the
engine reads them. Part of the [decision record](../DECISIONS.md), which says
how to add an entry here.

<a id="decision-15"></a>

15. **Default/check expressions are compared after peeling the engine's stored
    parentheses** (`((0))` → `0`), only when they wrap the whole string.

<a id="decision-195"></a>

195. **The shared definition scanner tracks block-comment depth.** Block
    comments nest in T-SQL and in PostgreSQL alike — measured, `SELECT /* a /*
    b */ c */ 1` returns 1 on SQL Server, and one closer for two openers is
    "Missing end comment mark" — and `normalize_definition` left a comment at
    the first `*/`. What followed was read as code: an apostrophe in it opened
    a literal that was not there, the `'` that really opened one closed it,
    and the literal's spacing was folded as layout, so two module bodies
    returning different strings compared equal and the change was never
    planned. The repository already had a nesting-aware scanner for the
    identifier scan in `pbps-model` (3b5c9de); this was the same defect in the
    second scanner, with the quieter failure. The rest of ADR-0011 Amendment 2
    — the scanner taking a description of the engine's literals, the default
    going — waits for the PostgreSQL crate, which is what needs it.

<a id="decision-226"></a>

226. **`normalize_definition` takes a description of the engine's literals, and
    every dialect must supply one.** The default was one engine's scanner
    wearing a neutral name: it opened a quoted region on `'`, `"` or `[`, and
    knew neither `E'…'` nor `$tag$…$tag$` (ADR-0011 Amendment 2). Two of the
    three failures that caused were **silent** — two definitions returning
    different strings compared equal, so the change was never planned at all,
    which is what `whitespace_inside_a_literal_is_data` exists to prevent on
    SQL Server and what arrived on PostgreSQL through the shared default.

    `Dialect::lexicon` is required; `normalize_definition` keeps a default that
    is the shared scanner driven by it. The ADR says the default is removed,
    and the reason it gives is that "a new dialect cannot silently inherit
    another engine's answer" — which a required `lexicon` secures, without each
    dialect restating the call. What the ADR ruled out was *leaving the default
    in place and overriding it in the PostgreSQL dialect*; a default with no
    engine in it is not that.

    The description carries **termination rules, not only delimiters**, because
    two of the three failures were termination rules: `\'` does not close an
    `E'…'` string, and a `$tag$` region ends only at its own tag. A table of
    delimiters would have fixed the third alone. What is *not* a field is
    equally deliberate — the doubled-quote string, the `--` comment and the
    **nesting** `/*…*/` are shared because both engines were measured and both
    answered the same.

    Two rules keep a `$` from opening a literal that is not there: a tag
    follows the rules of an unquoted identifier (so `$1.00` is money, not a
    tag), and a `$` that continues an identifier opens nothing (measured,
    PostgreSQL lexes `a$b$c` as one name). The bracket row of the ADR's table
    is fixed as a side effect and not as a compromise: `[` is a quote in SQL
    Server's lexicon and absent from PostgreSQL's, so a reindent inside
    `a[1 + 2]` stops reading as a changed module.

<a id="decision-233"></a>

233. **A dollar-quote tag follows the engine's grammar, which is over bytes.**
    PostgreSQL's lexer spells it `dolq_start [A-Za-z\200-\377_]` and
    `dolq_cont` the same plus the digits: **any** byte with the high bit set is
    a tag character. `char::is_alphanumeric` is a different and smaller set —
    measured on 18.6, `$á$` written as `a` and a combining acute is a tag the
    engine accepts, and Rust classifies U+0301 as neither letter nor number.
    Refusing it there scanned the literal's body as code and folded its spacing
    away: the silent failure of 226 again, one level down, arriving through the
    rule written to prevent it.

    "Not ASCII" is the whole of the high-bit half, so the test is
    `is_ascii_alphabetic() || '_' || !is_ascii()`, plus the digits after the
    first character. A Unicode class is the wrong tool for a grammar written in
    bytes, however much it looks like the right one.

<a id="decision-239"></a>

239. **The gap around a dot is closed from the text already emitted, not from
    the text still to come.** `scannable` folds `[Dbo] . [V]` and `dbo.v` into
    one string so the dependency scan can look for a qualified name. It read
    the character *before* a whitespace out of the input, where that is the
    first character of a run, and the character *after* it by skipping the
    whole run — so a leading gap closed and a trailing one did not. A
    formatter that breaks `dbo.` from its object across a line left
    `dbo.<indent>customer`, and the qualified needle found nothing.

    What decides then is the bare name, which is the weaker question the scan
    falls back on for a definition written inside its own schema. It answers
    `sales.dbo.customer` yes for `dbo.customer` — the qualified needle exists
    precisely to say no there — so the half-closed gap does not merely lose an
    edge, it invents one, and an invented edge can close a cycle. Members of a
    cycle are emitted in name order, and then a `CREATE VIEW` fails inside the
    plan's transaction: everything rolls back, so nothing is damaged, but a
    valid plan was refused and the only way past it is a hand-written
    `depends_on:`.

    Reading `before` from `out` is the whole fix: what precedes this character
    in the result is the dot itself once the whitespace between them has been
    dropped, however long the run. The tests ask `scannable` directly rather
    than going through `references`, because the bare-name fallback matches a
    half-closed gap too and would hide the difference.

    **Amended: a bracket between two words leaves a space behind.** On
    PostgreSQL `[` is a subscript and `ARRAY[` an array constructor, not a
    quote — measured, `CREATE VIEW ao.a AS SELECT (ARRAY[ao.z()])[1]` is
    refused until `ao.z()` exists. Dropped as a quote, the bracket glued
    `ARRAY[ao.z` into `arrayao.z`, the needle found no word boundary, no edge
    ordered the function first, and the plan's `CREATE VIEW` failed. The
    space is put only where two identifier characters would otherwise touch,
    so `[dbo].[v]` still folds to `dbo.v`.

<a id="decision-261"></a>

261. **A bare-literal default on a setting-sensitive column is refused, offline,
    with the resolved spelling named.** ADR-0013 §3 says such a default reaches
    the server as the resolved typed spelling, canonicalized by the engine at
    plan time, and that the offline path refuses instead. The emitter is the
    offline path: it has no connection and cannot ask.

    Measured, the identical `CREATE TABLE` under two `DateStyle`s stores two
    different dates and says nothing either way:

    ```text
    DEFAULT '01/02/2026' on a date, created under MDY:  '2026-01-02'::date
    the same declaration created under DMY:             '2026-02-01'::date
    DEFAULT '2026-01-02'::date under either:            '2026-01-02'::date
    ```

    The test is *whole expression is one string literal*, and nothing more: an
    expression carrying a cast, a call or an operator is emitted as written.

    **A cast resolves nothing, and a first version of this entry said it did.**
    Measured, `'01/02/2026'::date`, `DATE '01/02/2026'` and `CAST('01/02/2026'
    AS date)` all store 2026-01-02 under MDY and 2026-02-01 under DMY, exactly
    as the uncast spelling does. What decides the value is whether the
    *spelling* is ambiguous, which is a question about a value and therefore the
    engine's to answer.

    So the boundary this draws is between *provably* unresolved and *possibly*
    resolved. Measured, this engine reads every string default back with a cast
    welded on — `'unnamed'` becomes `'unnamed'::text` — so a bare literal is
    certainly not the engine's own rendering and certainly not canonical, and
    refusing it costs nothing a declaration could want. A cast form may be that
    rendering, and usually is: it is what `pull` writes.

    A **typed** literal that is not the engine's rendering keeps the session
    dependence and is not refused, and that gap is deliberate: the only offline
    rule that closes it also refuses `'2026-01-02'::date`, which is a correct
    declaration and the one `pull` itself writes, with no remedy a message could
    name. ADR-0013 §3 closes it at plan time, connected, and that resolver
    arrives with the step that has a caller for it (issue #173).

    **None of this is about the plan converging**, and a first version of this
    paragraph said it was. The state records what each object was declared as
    beside what it read back (DECISIONS 207–209), so a declaration in any
    spelling goes quiet after the apply that records it — that is what closed
    the shipped SQL Server loop (DECISIONS 208), and it closes this one too.
    What it does not close is a column defaulting to February in one environment
    and January in another.

    **One literal has four spellings here, and a first version knew one.**
    Measured, `'01/02/2026'`, `E'01/02/2026'`, `$$01/02/2026$$` and
    `U&'01/02/2026'` on a `date` all store 2026-01-02 under MDY and 2026-02-01
    under DMY — identical behaviour, and three of them would have walked past a
    check written around the quote character. The one form left over is
    `U&'…' UESCAPE '…'`, which is two literals with a keyword between them; it
    answers "not a bare literal" and is named in the code so the gap is
    recorded rather than unnoticed.

    Which types are on the list is ADR-0013's derivation and not this file's
    judgement: `date`, `time`, `timetz`, `timestamp`, `timestamptz`,
    `interval`, `real` and `double precision`. `time` is on it because the ADR
    put it there, and narrowing a recorded list because today's probe did not
    reach one of its rows is how a list stops being the rule it came from.

    **The question is asked in `validate_table`, and the emitter's copy is the
    later of the two.** `Change::AlterColumnDefault` carries a column reference
    and two expressions and *no type*, so on the one path that changes a default
    on a column already there, `emit` cannot tell a bare `'01/02/2026'` on a
    `date` from the same text on a `text` — and only one of those is a value
    the applying session decides. Putting the rule in the emitter alone would
    have covered `CreateTable` and `AddColumn` and left the third open, which is
    the sweep this project has failed before. `validate_table` sees the
    declaration with its types, and every command that hands statements to a
    database runs it (DECISIONS 141), so the change is never planned at all.

    **A string constant continued across a newline is one literal**, and the
    continuation rule is narrower than "another literal beside it". Measured:

    ```text
    'a' ⏎ 'b'       -> ab        E'a' ⏎ 'b'  -> ab       U&'a' ⏎ 'b' -> ab
    'a'   'b'       -> syntax error: on one line they are two constants
    'a' ⏎ E'b'      -> syntax error: a continuation is a plain literal
    $$a$$ ⏎ $$b$$   -> syntax error: dollar quoting does not continue
    'a' ⏎ 'b\'c'    -> unterminated: the backslash does not escape in a
                       continuation, even after an `E'…'` first piece
    ```

    So `DEFAULT '01/02/'` ⏎ `'2026'` on a `date` is the same declaration as the
    one-piece spelling and stores the same session-decided value — measured,
    `'2026-01-02'::date` under MDY — and a guard that stopped at the first
    closing quote let it past. The scanner now reads pieces: the first in
    whichever of the three forms opened it, each later one plain and preceded
    by whitespace that contains a newline.

    **Grouping parentheses are taken off first, and that is the boundary of
    what this guard reads.** Measured, `DEFAULT ('01/02/2026')` on a `date`
    stores `'2026-01-02'` under MDY and `'2026-02-01'` under DMY exactly as the
    unparenthesised form does, with the parentheses dropped from what the
    engine keeps — so a test that looked only at the first character let it
    past. `without_grouping` strips balanced outer parentheses by a depth that
    must not return to zero before the end, counting every parenthesis
    including ones inside literals. Counting them is what keeps this from
    becoming a parser: a stray parenthesis in a literal can only make the test
    *fail*, which costs a refusal and never causes one, and stripping the first
    and last characters can produce a single complete literal only if what was
    there was `(` literal `)`. Anything more structural — a cast, a
    concatenation, a function call — stays outside on purpose (DECISIONS 174),
    covered by the settings the framing pins.

<a id="decision-278"></a>

278. **A comment is whitespace to the bare-literal guard, and the two comment
    forms are not the same whitespace.** The unresolved-default rule (266,
    ADR-0013 §3) asks whether a declared default is one bare literal. The
    scanner read the gap between two pieces of a continued string constant
    with `trim_start`, which is Rust's idea of whitespace and not this
    engine's. **Measured** on 18.6:

    ```text
    '01/02/' -- c ⏎ '2026'        -> 01/02/2026     one constant
    '01/02/' -- /* x ⏎ '2026'     -> 01/02/2026     the block opener is comment text
    '01/02/2026' -- c             -> 01/02/2026     trailing, after the last piece
    '01/02/2026' /* c */          -> 01/02/2026     trailing, either form
    '01/02/' /* c */ ⏎ '2026'     -> syntax error   a block comment ends the
    '01/02/' ⏎ /* c */ '2026'     -> syntax error   possibility of a continuation
    '01/02/' /* /* x */ */ ⏎ '2026' -> syntax error and they nest
    ```

    So a `--` comment is part of the gap *and* supplies the newline a
    continuation needs, while a `/* … */` comment is whitespace everywhere
    except in that gap. The guard now scans both, at either end of the
    expression as well as between pieces, because a declaration that hides an
    ambiguous literal behind a comment is the same hazard as one that does
    not — and this guard's silence means *accepted*, so every form it cannot
    read is a form that gets through.

    An unterminated `/*` is left alone: the engine refuses that by name
    (`unterminated /* comment`), and 266 draws the line there.

<a id="decision-279"></a>

279. **The grouping unwrap counts only the parentheses that are code.** The
    same guard as 278, the same polarity, one round later. `without_grouping`
    takes `DEFAULT ('01/02/2026')` down to the literal it wraps, by finding a
    paren depth that does not return to zero before the end. It counted every
    parenthesis, including ones inside literals and comments, and the code said
    that was deliberate: a stray one can only make the test *fail*, failing to
    unwrap only costs a refusal, and this guard is allowed to be wrong in that
    direction.

    The first half is true and the second is backwards. The guard refuses when
    it answers *yes*, so an expression it cannot unwrap is one it **permits**,
    and a single parenthesis that is data is enough to write the hazard down:

    ```text
    CREATE TABLE t (d date DEFAULT (/* ) */ '01/02/2026'))
        -> stores 2026-01-02 under DateStyle MDY, 2026-02-01 under DMY
    ```

    Measured, and accepted silently either way. So the scan now steps over
    literals and comments through one helper — the same constructs the
    bare-literal scanner already knows, listed once — and it is still not an
    expression parser: it never asks what any of it means. What stays outside
    remains outside (174): a cast, a concatenation or a function is structure,
    and this guard does not read structure.

<a id="decision-281"></a>

281. **A declared expression is followed by a newline before any syntax the
    emitter owns, and a line ends at `\r` as much as at `\n`.** Two halves of
    one fact about comments, found in the same round.

    **The emission.** This dialect writes three things verbatim — a default, a
    check expression, an index filter (ADR-0013 §3) — and put its own syntax
    behind them on the same line. Measured, a comment at the end of the user's
    text takes it away:

    ```text
    CREATE TABLE t (n int, CONSTRAINT ck CHECK (n > 0 -- reason));
      -> ERROR: syntax error at end of input
    CREATE TABLE t (a int DEFAULT 1 -- why, b int);
      -> ERROR: syntax error at end of input
    CREATE TABLE t (n int, CONSTRAINT ck CHECK (n > 0 -- reason ⏎ ));
      -> accepted, and stored as CHECK ((n > 0))
    ```

    So a valid declaration produced a statement that cannot run, at each of the
    five sites that interpolate one. One newline is the whole fix; it lives in
    a `verbatim` helper rather than at each site, so that a sixth site has
    somewhere to reach for. The engine keeps none of the comments — the stored
    definition is the parsed expression — which is why nothing but the apply
    could have shown this.

    **The scan.** The gap scanner of 278 asked for `'\n'`, and this lexer ends
    a line at either character. Measured, `'01/02/' <CR> '2026'` is the one
    constant `01/02/2026`, and so is `'01/02/' -- c <CR> '2026'`: a bare
    carriage return both ends a line comment and supplies the newline a
    continued constant needs. A default written that way walked past the guard
    that exists to refuse it.

    **Both scanners, and the first fix only reached one.** `skip_datum` — the
    one 279 added, which the grouping unwrap uses to step over data — kept its
    own `find('\n')` through that commit, so `( -- ) <CR> '01/02/2026')` had
    its closing parenthesis swallowed by a comment that had already ended,
    the grouping went unwrapped, and the same default walked past the same
    guard by the other road. Measured, that expression is 2026-01-02 under MDY
    and 2026-02-01 under DMY. The character class is a named constant now
    (`NEWLINE`) rather than a literal at each site, which is what makes the
    next scanner's omission visible.

    The repo had already recorded this shape for SQL Server (PITFALLS, "A
    comment ends at a carriage return"), which is the part worth keeping: a
    scanner written years later went in with one line ending anyway, and then
    the fix for it missed its own sibling one screen away. Two more instances
    of the same family are filed against the SQL Server pull's header scanner
    (#197), which ends a line comment at LF alone and does not nest block
    comments.

<a id="decision-282"></a>

282. **Trailing whitespace and comments are stripped before the grouping test,
    by walking the expression forward.** 278 taught the guard that a comment is
    whitespace and stripped it from the front; 279 taught the grouping unwrap
    to step over data. Between them was a gap neither closed: the unwrap is a
    test about the expression's **last character**, and a trailing comment is
    what the last character then is.

    ```text
    CREATE TABLE t (d date DEFAULT ('01/02/2026') -- note ⏎ )
      -> stored as '2026-01-02'::date under DateStyle MDY,
         '2026-02-01'::date under DMY
    ```

    Measured, along with `('01/02/2026') /* note */`,
    `(('01/02/2026') -- inner ⏎ ) -- outer` and
    `$$01/02/2026$$ /* note */`: all four are accepted, all four move with the
    session, and all four answered "not a literal" because the grouping could
    not be unwrapped.

    **Forward, not backward.** A `--` comment is recognisable only from its
    opening, so there is no trailing-trivia trim that works from the end: the
    scan walks the whole expression, steps over literals through `skip_datum`
    so that a `--` inside one is not read as a comment, and remembers where the
    last code character was. An unterminated `/*` is left standing as code, the
    same answer `after_the_gap` gives, so the engine refuses it by name
    (266).

    The three strippers — leading gap, trailing trivia, grouping — now run to a
    fixed point in `is_a_bare_literal`, because each can expose work for
    another: `(('x') -- inner ⏎ ) -- outer` needs all three, twice.

<a id="decision-300"></a>

300. **The PostgreSQL datum scanner recognizes national-character literals even
    where the current caller would reject them later.** `N'…'` is one string
    literal to the lexer, and a parenthesis inside it is data. Leaving that
    opener out of `skip_datum` therefore makes the grouping scan treat data as
    syntax; leaving it out of `is_a_bare_literal` makes the two views of the
    same token disagree.

    Measured on PostgreSQL 18.6, a bare `N'01/02/2026'` default on a `date`
    column is rejected because the literal is typed `character` and there is
    no assignment cast. The unresolved-default guard now refuses it earlier
    with its own actionable `DateStyle` diagnostic. That earlier refusal is
    not the reason for recognizing the token: the scanner records lexical
    structure, independent of which types or callers happen to accept the
    expression today. Both upper- and lower-case openers are accepted, and
    neither gives backslashes escape semantics.

    **Amended by the module emitter (302).** A module's whole `definition` is
    verbatim text, so the statement's own `;` is on a line of its own too. The
    write scope puts a `RESET search_path;` after every statement, and a
    definition ending in `-- note` swallowed the terminator and ran on into it.
    The rule is not about expressions; it is about where the user's text ends
    and this tool's begins.

<a id="decision-323"></a>

323. **A literal default is recognised through the cast the catalog welds on.**
    ADR-0013 §4. This engine hands a default back deparsed and typed:
    `'unnamed'` comes back `'unnamed'::text` and `'x'` on a `varchar(9)` comes
    back `'x'::character varying`. A reader that did not look through the cast
    would call every string default an expression, never ask the engine whether
    a cell equals it, and read every such cell back as "cannot be told from its
    default" — so a declaration that omits the column and one that spells it
    would stop comparing equal. The strip is conservative in the other
    direction on purpose: it removes only a trailing `::type` at the top level,
    so `'a'::text || 'b'` is not a literal, and `nextval('s'::regclass)` — which
    ends in a cast and would consume a sequence value if asked about — is not
    either.

<a id="decision-350"></a>

350. **A default spelled `CAST(x AS type)` is read through the cast, as
    `x::type` is.** The reader of defaults looks through the cast the
    catalog welds on (ADR-0013 §4), and the catalog spells every cast
    `…::type` — so that was the only spelling it knew. The SQL-standard form
    never comes out of the catalog; it comes out of a declaration, which
    keeps the user's text verbatim, and **measured**, `DEFAULT CAST(NULL AS
    text)` the engine does not even store: `pg_attrdef` holds no row for
    it. Left to that default, a row was read as written to an expression no
    probe can evaluate, and the plan that unpicks a reference before
    deleting its parent was refused for a NULL.

    `unwrapped` now takes off an enclosing `CAST(… AS type)` too, on the
    same terms as `::type`: the `AS` is the last one at the top level
    outside a string, the closing parenthesis is the opening one's, and what
    follows the `AS` is a type name and nothing else. `CAST(now() AS text)`
    unwraps to `now()` and stays an expression; `CAST('a' AS text) ||
    CAST('b' AS text)` does not unwrap at all. Every reader of a default —
    the read-back's constant test, the emitter's postcondition, the probes'
    NULL test and the planned key's backfill — goes through it.

<a id="decision-351"></a>

351. **A default that is NULL is refused at the declaration, because this
    engine does not keep one.** **Measured** on 18.6: `DEFAULT NULL`,
    `DEFAULT (NULL)`, `DEFAULT NULL::text` and `DEFAULT CAST(NULL AS text)`
    are all accepted, and every one leaves the column with no `pg_attrdef`
    row at all — to this engine a column with no default *is* a column
    whose default is NULL. The pull therefore reads the column back with no
    default, the next connected plan sets it again, and the deploy's check
    of what the apply left behind, which compares whether a default is
    there at all (185, 186), refuses the column the plan just wrote: a
    declaration that can never converge, on the pattern 227 refused
    `serial` for. SQL Server keeps `(NULL)` as a default constraint of its
    own, so the rule is this dialect's.

    `validate_table` refuses such a column, in every spelling `unwrapped`
    reads as NULL (350), and names what to declare instead: no default.
    Refused rather than folded to none, because the loader hands the
    dialect the declaration to check and not to rewrite, and a silent
    rewrite is what 266 wrote this class of rule against. Every command
    that hands statements to a database runs these checks (141), so the
    change is never planned.

<a id="decision-352"></a>

352. **A stored key is compared as the engine's own check compares it: under
    the referenced column's collation, through the operator the constraint
    records.** The count over the referencing tables spelled every column of
    a key `p.<referenced> = <child side>`, and `=` between two columns is the
    parser's to resolve. **Measured**: a child column collated explicitly and
    differently from the column it references — the parent `"C"`, the child
    `"en_US.utf8"` — is a key the engine accepts and enforces, the delete
    refused; and the plain comparison between them fails the moment a row is
    compared, `could not determine which collation to use`, which the probe
    runner reads as unchecked (342) — a delete guard that walks past the one
    child it was written for. A default-collated side takes the other's
    collation and never conflicts, so it takes a managed parent collated
    itself, which the pull notes (introspect) and still manages.

    The parent side of each column is now spelled with `COLLATE` the
    referenced column's collation when it has one — the collation the
    engine's referential check uses, and an explicit one wins over the
    child's implicit one — and compared through `OPERATOR(<schema>.<op>)`
    from `conpfeqop`, so that the comparison is the constraint's and not
    whatever `=` resolves to between the two types. Both are read from the
    catalog inside the probe, as the rest of the key is. The planned keys
    (345) keep the plain spelling: their columns are this plan's, in a model
    that holds no collation.

<a id="decision-353"></a>

353. **A planned key compares two stored columns under the referenced
    column's collation, spliced in by the engine.** 352 spelled the stored
    keys' comparisons from the catalog, and left the planned keys (345) on
    the plain `=`, "their columns being this plan's, in a model that holds
    no collation". The columns are this plan's; their collations may not
    be: a column collated outside this tool is one the pull notes and still
    manages, and **measured**, `ADD CONSTRAINT … FOREIGN KEY` between a
    parent `"C"` and a child `"en_US.utf8"` is accepted, while the plain
    comparison between them fails the moment a row is compared — the
    planned key's count read as unchecked, and under a staged apply the
    delete committed before the key failed on its orphan.

    The planned key's count over stored rows was already assembled by the
    engine, for `relkind`; it now assembles the collation too. Where a
    stored parent column meets a stored child column, the parent side
    carries a mark, and the assembled body replaces it with `COLLATE
    <schema>.<collation>` read from `pg_attribute` — the referenced
    column's own, which is what the engine's check will use, and explicit
    on one side is enough. A column this plan retypes carries the clause
    only where the new type has a collation. The arrivals of updated rows,
    the one static count that compared two stored columns, now run through
    the same assembly; a literal takes the column's collation on its own
    and needs nothing. A mark that reaches a probe as text is refused
    before the probe is returned, because as text it is a syntax error
    the runner would read as unchecked.

<a id="decision-354"></a>

354. **The parent's own readability is asked before the children are
    counted.** The refusal over children the session cannot count (329,
    342) asked about the referencing relations and never about the table
    the row is deleted from — and every count reads that table: the
    deleted row by its key column, and the referenced columns of every
    key, which may not be the key column at all (116). `DELETE` grants
    none of those reads. **Measured**: with `DELETE` and `SELECT` on the
    key column alone, the count of children through a key into another
    column fails `permission denied for table`, the delete runs, and `ON
    DELETE CASCADE` takes the child the count never saw.

    The refusal now carries a parent-side term: schema `USAGE`, and table
    `SELECT` or column `SELECT` on the key column, on every referenced
    column of every stored key the delete meets (read from `confkey`), and
    on every stored column a planned key references; a row-security policy
    on the parent refuses on the same terms as one on a child, because a
    `SELECT` policy and a `DELETE` policy need not agree on the row.

<a id="decision-355"></a>

355. **A default is a literal in every spelling this engine reads one.** The
    reader of defaults admitted a string only as `'…'`, which is the one
    spelling the catalog deparses every string to — `E'…'`, `U&'…'`,
    `N'…'` and `$tag$…$tag$` all come back `'…'::text`, measured — so the
    other spellings reach the plan only from a declaration, verbatim, and
    there they were read as an expression no probe can evaluate: a row left
    to `DEFAULT E'old'` was refused, and with it the plan that unpicks a
    reference before deleting its parent (the shape of 350, one spelling
    further along).

    `is_constant` now hands a string in any of those spellings to the
    emitter's own reader, `is_a_bare_literal`, which already knows where
    each one ends — an escape in `E'…'`, a tag in `$tag$…$tag$`, a literal
    continued across a newline — and that `E'a' || 'b'` is not one. One
    reader for both questions, because the emitter's postcondition and the
    probes' comparison are the same question about the same text.

<a id="decision-356"></a>

356. **`U&'…' UESCAPE '…'` is one literal.** The reader of string literals
    left it as a recorded gap — two literals with a keyword between them,
    answering "not a bare literal" — and could afford to, because the
    guard it served refuses when it says *yes*; the gap only let a
    session-decided date through in one more spelling. 355 made the same
    reader the one that decides whether a default is a constant the probes
    can compare, and there "no" refuses a valid plan: a row left to
    `DEFAULT U&'keep' UESCAPE '!'` was refused as an expression.

    **Measured** on 18.6: the keyword ends the literal — nothing continues
    it past the escape character, while a plain continuation before it is
    still one literal; the character may be spelled `'!'` or `E'!'`, with
    any trivia or none before the keyword and between it and the
    character; and two characters, none, a hex digit, `+`, a quote or a
    space are each refused by the engine. The reader now admits exactly
    that shape, in the one place both questions are asked. The guard over
    session-decided defaults refuses one more spelling as a result, which
    is the direction it is allowed to move in.

<a id="decision-357"></a>

357. **The cast scanners read a string the way the engine does, and the
    prefix test reads bytes.** Two readers of a default's text were written
    around the quote character alone. `without_a_cast` and
    `without_a_cast_call` toggled "inside a string" on every `'`, so
    `E'it\'s'::text` ended its string at the apostrophe and kept its cast,
    and `$$it's$$::text` never closed one; read as expressions, a row left
    to either was refused. And `is_constant` looked at its first character
    through a one-byte slice, which is not a character boundary when the
    character is `é` — a default `é()` is an expression like any other,
    and the reader panicked on it while classifying a row.

    One `string_end` now serves both scanners: a `'` opened by an `E` that
    is not the tail of an identifier takes backslash escapes, a doubled
    `''` is one quote in any string, and a `$tag$` string ends at its own
    tag with nothing inside it — a quote or a `::` — read as structure. The
    prefix test compares bytes. Both were second instances of a known
    shape: the literal reader (355, 356) already knew every one of these
    rules, and these two readers were asked the same question about the
    same text.

<a id="decision-358"></a>

358. **A comment in a default is whitespace to the reader of defaults, as it
    is to the engine.** `unwrapped` took off parentheses and casts and left
    a comment where it stood, so `NULL /* note */::text` unwrapped to `NULL
    /* note */`, which is not `NULL` to a string comparison — and 351's
    refusal let the declaration through. **Measured**: `NULL /* note
    */::text`, `CAST(NULL /* note */ AS text)`, `/* lead */ NULL` and a
    parenthesised `NULL -- line` all leave the column with no default at
    all, as a bare `NULL` does, so the declaration is the one 351 refuses
    for never converging; a commented value default, `'x' /* c */::text`,
    is kept as `'x'::text`.

    `unwrapped` now strips a comment at either end of the text on every
    pass, through the emitter's own readers of trivia — which already know
    that a line comment runs to its newline, that a block comment ends at
    its close, and that an unterminated one is not trivia but text the
    engine refuses by name (266). One more reader asked the literal
    reader's question about the same text, and answered it the literal
    reader's way (355–357).

<a id="decision-359"></a>

359. **The cast scanners read a comment as a gap, wherever it stands.** 358
    stripped a comment from either end of the text and left the scanners
    themselves reading `/` and `-` as characters of a type name or a
    structure: `CAST(NULL AS text /* note */)` handed `text /* note */` to
    the type test, which refused it; `CAST /* c */ (…)` found no
    parenthesis; and a comment holding a parenthesis, `CAST(NULL AS text
    /* ) */)`, counted it. **Measured**: every one of those, a comment
    between `AS` and the type, one inside the parentheses at either end,
    a nested block comment and a line comment before the close, leaves the
    column with no default at all — the declaration 351 refuses — and a
    commented value default is kept.

    Both scanners now step over a comment as they step over a string,
    through the emitter's block-comment reader, which counts nesting; an
    unterminated one is not a gap and the text is left to the engine to
    refuse by name (266). The type after `::` or `AS`, and the text after
    `CAST`, are read past their trivia. The same shape as 357, one token
    further along.

<a id="decision-360"></a>

360. **A default is a number in every spelling this engine reads one.** The
    reader admitted decimal digits, a point and an exponent, and nothing
    else: `2_55` and `0xFF` were expressions, a row left to either was
    refused as unevaluable, and the plan that moves a child to `255` before
    deleting `1` was refused with it. **Measured** on 18.6: an underscore
    stands between two digits of the integer part, the fraction or the
    exponent, and right after a base prefix, never at either end or
    doubled; `0x`, `0o` and `0b` open an integer with no fraction and no
    exponent, and `e` after `0x` is a digit. The reader admits exactly
    that, and refuses `1_`, `_1`, `1__0`, `1._5`, `0xFF.5`, `0x` and `1e_5`
    as the engine does.

<a id="decision-361"></a>

361. **A typed NULL default is refused only where the engine erases it: a
    NULL of the column's own unmodified type.** 351 said every spelling of
    a NULL default leaves no `pg_attrdef` row, from four measurements that
    happened to cast to the column's own type. **Measured** wider: the
    engine erases the default when, coerced to the column, it is a bare
    null constant — `NULL`, `NULL::text` on `text`, `NULL::int` on
    `integer`, `NULL::timestamp with time zone` on `timestamptz` — and
    keeps it whenever the coercion leaves a step behind: a NULL of another
    type (`NULL::varchar` on `text`, `NULL::bigint` on `integer`), of the
    column's type with a modifier the column lacks (`NULL::timestamp(3)
    with time zone` on `timestamptz`), or of any type where the column
    itself carries a modifier (`NULL::varchar(10)` on `varchar(10)`,
    `NULL::numeric(5,2)` on `numeric(5,2)`). A kept default reads back
    under the catalog's spelling, and declared that way it converges.

    The refusal now reads the type of the outermost cast taken off the
    default (`unwrapped_with_type`) and refuses only a bare NULL, or one
    whose cast type normalizes to the column's own with no argument on the
    column; a cast type the model cannot spell — `timestamp(3) with time
    zone`, which puts its argument in the middle — is a type the column's
    is not, and the default is left alone. The type test behind both cast
    scanners admits what the engine casts to: words after a modifier
    (`timestamp(3) with time zone`, `interval day to second(3)`) and array
    markers in every spelling, which is the finding that opened this
    entry — `CAST(NULL AS timestamp(3) with time zone)` was neither
    unwrapped to NULL for the probes nor, as it turns out, erased.

<a id="decision-362"></a>

362. **A comment inside a type's text is the whitespace it is to the
    engine.** 359 read a comment as a gap at either end of a type and
    between the tokens of a cast, and left one between the *words* of a
    type — `double /* note */ precision` — to the type test, which refused
    the slash. **Measured**: `CAST(NULL AS double /* note */ precision)` on
    `double precision`, `NULL::double -- c ⏎ precision`, `timestamp /* c */
    with time zone` and `text /* c */ []` are each erased as the bare NULL
    of the column's own type is (361); `numeric(5, /* c */ 2)` on
    `numeric(5,2)` is kept, as the modified column's is. The type text
    behind both scanners is now read with every comment replaced by one
    space and runs of whitespace by one, before it is tested as a type or
    parsed as the model's, so that the validator compares the type and not
    the trivia; an unterminated block comment is still not a type, and is
    left to the engine to refuse by name. The last of the readers written
    around a character instead of the engine's lexical rules (355–361).

<a id="decision-363"></a>

363. **The cast scanners end a line comment where the engine does, and read
    a comment as the gap around `AS`.** Two more places the scanners of
    359–362 were written around a character: their line comment ran to
    `\n` alone, so `CAST(NULL -- note ⏎(CR) AS text)` read the rest of the
    expression as commented — the shape PITFALLS already records for the
    module scanner, and `emit::NEWLINE` already ends one at either
    character; and the keyword `AS` was recognised only between two
    whitespace bytes, so `CAST(NULL/**/AS/**/text)`, whose separators are
    comments, was no cast. **Measured**: both are erased on a `text`
    column as the bare NULL is, and a value with a carriage-return comment
    is kept. The scanners now end a line comment at `emit::NEWLINE`, and
    read `AS` as the keyword when what stands before it is whitespace or a
    comment just skipped and what follows is whitespace or a comment
    opening. Nothing in these readers is left that looks at a byte where
    the engine looks at a token.

<a id="decision-364"></a>

364. **The cast scanner ends a token where the lexer does, and the
    validator reads a cast type as the grammar spells it.** Two last shapes
    of 355–363. The keyword `AS` was read only after a gap, so `CAST('keep'AS
    text)`, `CAST($$x$$AS text)` and `CAST((NULL)AS text)` — each a
    literal the engine accepts, since a string, a `)` and a quoted
    identifier close their own token — were no cast, and an update to such
    a default was refused as an expression the count cannot evaluate;
    `CAST(NULL AS"text")` was no cast either. And the validator's rule of
    361 parsed the cast type as the model's, which has no `.` or `"`, so
    `NULL::pg_catalog.text` on a `text` column was not the column's own
    type and the erased default was left to be set on every plan. **Measured**
    on 18.6: `1AS` is "trailing junk after numeric literal" and `NULLAS` one
    word, so a bare word or number still needs the gap; `AS(text)` is a
    syntax error, so `(` opens nothing; `NULL::pg_catalog.text`,
    `NULL::"text"`, `"pg_catalog" . "text"`, `pg_catalog/**/./**/text`,
    `PG_CATALOG.INT4` on `integer`, `"bool"` on `boolean` and
    `pg_catalog.timestamptz` on `timestamptz` are each erased as the bare
    NULL is; `pg_catalog.integer`, `"integer"` and `"TEXT"` are no type at
    all; `NULL::app.text` over a domain in another schema, `NULL::bpchar` on
    `character` (the modifier is not the column's) and `NULL::"char"` (the
    engine's one-byte type, not `character`) are kept. The scanner now reads
    `AS` after a string, a quoted identifier or a `)` as after a gap, and
    before a `"` as before one; the validator takes a `pg_catalog.`
    qualification and quotes off the type first, and only for the names the
    catalog has under that spelling — the grammar words `integer`, `boolean`,
    `character varying` and `double precision` resolve to nothing quoted or
    qualified — so that another schema's type, a domain among them, stays
    another type. Not a general name resolver: `search_path` is not
    consulted, and a type the catalog has under a spelling this dialect
    does not know is left to the engine.

<a id="decision-365"></a>

365. **A sign is read through the trivia, groupings and casts between it and
    its operand.** The constant test of 360 took one leading `-` or `+` off
    the text and read what followed as the number, so `- 1`, `+ 1`, `- /*
    c */ 1` and `- - 1` were expressions no probe can evaluate, and an update
    that left a child to such a default while a parent went was refused;
    worse, the catalog itself deparses a declared `+1` as `(+ 1)` and `+-1`
    as `(+ '-1'::integer)`, so every read-back positive default was one.
    **Measured** on 18.6: each of those spellings, and `-(-1)`, `-(+ 1)`,
    `-'1'::integer` and `-NULL::integer`, is accepted and folded or kept as
    `(- 1)`, `(- (+ 1))`, `(- NULL::integer)`; a sign before a bare string,
    `NULL` or a boolean is refused by name (`operator is not unique: -
    unknown`), so what the signs stand before is a number, or a cast the
    engine already resolved. The test now takes off each sign and, after it,
    the trivia, parentheses and casts the readers of 358–364 already take
    off, and asks the same question of what is left; a sign before nothing,
    or before an expression, is still no constant.

<a id="decision-367"></a>

367. **A typed literal is the constant it is.** The constant test of 355–365
    read a string in every spelling the lexer has and a number in every
    base, and let the SQL-standard `DATE '2026-02-01'` — a type name and a
    string — fall through to the number test, so an update that returned a
    child to such a default while its former parent went was refused as one
    no probe can evaluate. **Measured** on 18.6: `DATE '2026-02-01'`,
    `TIMESTAMP(0) '…'`, `NUMERIC(5,2) '1.5'`, `INTERVAL '1' HOUR TO MINUTE`,
    `pg_catalog.date E'…'`, `"text" $$…$$`, `VARCHAR(10) U&'…' UESCAPE '!'`
    and `date'…'` with no gap are each accepted and stored as the cast
    literal the catalog deparses, `'2026-02-01'::date`, so the spelling
    reaches the reader only from a declaration; `DATE ('…')` and `TEXT[]
    '{a}'` are syntax errors, `foo 'x'` and `lower 'x'` are refused by name,
    and `TEXT 'a' || 'b'` is an expression. The test now reads a type name —
    as the cast readers admit one, arrays aside — followed by one string in
    any of its spellings, with an interval's field words and nothing else
    after it, as a constant. Not the emitter's bare-literal rule, which
    keeps `DATE '01/02/2026'` outside on purpose: that a spelling is
    session-decided is a question about its value, and this one is about
    whether a probe can compare it.

<a id="decision-368"></a>

368. **What follows a typed literal's string is an interval qualifier or
    nothing.** 367 admitted any words after the string so that `INTERVAL '1'
    HOUR TO MINUTE` would read as the constant it is, and the whitelist of
    letters and parentheses let `(BOOLEAN 'false' OR flip())` — a valid
    default once parenthesised, a bare `DEFAULT x OR y` being a syntax error
    — pass as a literal, so the read-back would have evaluated a volatile
    expression a second time and refused a correct row write, or run a side
    effect twice. **Measured** on 18.6: `DAY`, `HOUR TO MINUTE`, `SECOND(3)`,
    `DAY TO SECOND (2)`, `YEAR TO MONTH` and a qualifier with comments
    around it are each accepted; `DAYS` and `TO DAY` are syntax errors. The
    reader now takes, after the string, one of the six field words, or two
    joined by `TO`, with one `(n)` after a trailing `SECOND` — and nothing
    else. A qualifier the grammar refuses by combination, `MINUTE TO DAY`,
    is left to the engine, as every declaration it refuses by name is.

<a id="decision-369"></a>

369. **A Unicode-escaped type name is the name it spells.** The cast readers
    of 355–368 admitted a quoted type name and a qualified one, and the
    validator of 361 read them as the grammar does (364); a Unicode-escaped
    identifier, `U&"te\0078t"`, failed the type test on its `&` and `\`, so
    `NULL::U&"te\0078t"` on a `text` column was no cast to the reader, the
    NULL of the column's own type went unrefused, and the erased default was
    set on every plan. **Measured** on 18.6: `NULL::U&"te\0078t"`,
    `CAST(NULL AS U&"te\0078t")`, `NULL::U&"te!0078t" UESCAPE '!'`,
    `NULL::U&"pg_catalog".U&"text"`, `NULL::u&"text"` and
    `NULL::U&"te\+000078t"` each leave a `text` column with no default, as
    `NULL::text` does. The type text is now read with every Unicode-escaped
    identifier replaced by the plain quoted identifier it decodes to, through
    the decoder the module identity already uses (`pbps_model::module::
    decode_unicode_escapes`), its `UESCAPE` clause honoured by the rule the
    emitter reads one by; an escape that does not decode, or a clause
    spelled wrong, is no type, and is left to the engine to refuse by name.

<a id="decision-475"></a>

475. **Definition layout follows the engine's whitespace class.** The shared
     normalizer used Unicode trim and collapse for both engines. PostgreSQL
     treats the non-ASCII characters in Unicode White_Space as identifier
     bytes: changing `SELECT 1 AS x` to `SELECT 1 AS x\u{a0}` changes the view's
     output column, and folding the suffix away made the differ emit no
     `AlterModule` (issue #231). The reverse edit was missed too.

     Measured on PostgreSQL 18.6 and SQL Server 17.0.4075.5, for every character
     in Unicode White_Space: the six ASCII separators (space, tab, CR, LF, FF
     and VT) separate tokens on both engines. Every non-ASCII member remains
     part of a PostgreSQL alias, including at its end; SQL Server treats every
     tested member as a separator. `SELECT 1 AS a<character>b` is one alias on
     PostgreSQL for those non-ASCII characters, while SQL Server refuses the
     two aliases. Result-column metadata pins the actual name, rather than a
     display that could hide trailing spaces. PostgreSQL rejects those same
     characters between `1` and `+2`; SQL Server accepts them.

     `Lexicon` (formerly `ScanSettings`) therefore carries
     `whitespace_is_ascii`: true for PostgreSQL, false for SQL Server. The
     normalizer uses it for the outer trim and code-region collapse. The
     predicate intersects Unicode White_Space with ASCII when required;
     `is_ascii_whitespace` alone omits VT, which both engines accepted in the
     measurement. The synthetic ANSI lexicon retains its previous whitespace
     behavior. No lexical-name scan or creation/rebind ordering changes here.

     Inside literals and quoted identifiers, whitespace remains data. Inside
     comments, the existing layout collapse remains safe and unchanged; the
     CR/LF line terminator and nested-block structure still decide where code
     resumes. This is a comparison rule, not a rewrite of the emitted body.
     A live view regression plans and applies identifier edits in both
     directions, verifies the actual output name, and leaves real indentation
     changes as an empty plan. The shared unit test pins both engine settings
     across the measured character set and preserves quoted negative cases.

<a id="decision-504"></a>

504. **An expression is empty when the *engine's* lexis finds nothing in it,
     which is neither of Rust's whitespace classes (issues #480, #482).** 452
     settled that a non-breaking space is a legal unquoted check/filter
     expression on PostgreSQL, so Unicode `trim` would refuse a valid
     declaration, and moved both checks to `trim_ascii`. That was right about
     the danger and wrong about the set, twice over.

     **Measured** on PostgreSQL 18.6, one `ALTER TABLE … CHECK (<char>)` per
     character of Unicode White_Space. Exactly six separate tokens — space,
     tab, LF, **vertical tab**, FF and CR — each answering `syntax error at or
     near ")"`. Every non-ASCII member names a column instead: `column " "
     does not exist` for NBSP, the em space, U+2028, U+3000 and the rest, and
     `column "" does not exist` for NEL. Rust's ASCII-whitespace class omits
     the vertical tab, so a vertical-tab-only expression passed validation and
     reached the engine as an empty `CHECK` (#480). The set that is right is
     the one 475 already named for definition layout: Unicode White_Space
     intersected with ASCII.

     **And comments are layout without being whitespace bytes.** Measured on
     the same server, `CHECK (/* x */)`, `CHECK (-- x\n)`, the nesting form
     `/* a /* b */ c */` and any mixture of them with whitespace are each the
     same `syntax error at or near ")"` — for a check constraint and a partial
     index's `WHERE` alike. No character class can see those, which is why the
     question moved to `Lexicon::expression_in` (#482).

     **It does not lex the literals, and that is the point.** A literal, a
     quoted identifier and a dollar-quoted body are all *content*: the moment
     one opens, the answer is `Present` and the scan stops. So there is no
     literal to skip and no second lexis to drift from `code_only`'s — the
     mistake 315 paid for — and the `--` inside `'-- not a comment'` cannot be
     read as a comment, because the scan stopped at the quote.

     That half is not a nicety. Measured, `CHECK ('true')` and `CHECK ("flag")`
     are **accepted** outright, and `CHECK ('-- not a comment')` is refused on
     *value* grounds (`invalid input syntax for type boolean`, `22P02`) rather
     than as a missing expression. Blanking literals the way `code_only` does
     would therefore refuse a valid declaration, which is the one direction
     this check must never fail in.

     **Absent, empty and unreadable are three different things**, so the answer
     is three variants rather than a bool. Text that ends inside a block
     comment that never closes holds *unknown*, not *nothing*: measured, the
     engine calls that `unterminated /* comment`, and a validator that refused
     it as an empty expression would name a cause the engine disagrees with.
     `Unreadable` is left to the engine, which has the better sentence for it.

     The live declaration matrix sends each shape to the server and asserts the
     SQLSTATE, so the class above is pinned by the engine rather than by this
     entry. `22P02` joins `42704`, `42804` and `54011` in the set of refusals
     the offline validator is expected *not* to make: an expression the engine
     parsed and then rejected as a value is one it had no business refusing.

     SQL Server's own validator still asks Rust's `trim()`. Its whitespace half
     is right there — 475 measured that engine accepting Unicode White_Space as
     a separator — but it has the comment half of this bug, and that is issue
     #661 rather than scope here.
