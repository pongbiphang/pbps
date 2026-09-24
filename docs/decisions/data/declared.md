# Reference data: declared values

The `data:` block: what a declared cell may hold, how it is spelled, and how it
is read back. Part of the [decision record](../../DECISIONS.md), which says how
to add an entry here.

<a id="decision-51"></a>

51. **The catalog reads rows under a scope the command supplies, never on its
    own.** A database holds rows, not a notion of which of them are declared,
    and a catalog that returned every row of every table would make the drift
    check compare business data. `verify`, `status` and `apply`'s drift check
    read under the **recorded** state's scope, because their question is
    whether the environment moved since it was recorded; `snapshot`,
    `baseline` and `bootstrap` read under the declarations'; `plan --db`
    reads the union of the two once (`read_scopes`) and projects each view out
    of it, so a `data:` block added, removed or switched between `exact` and
    `ensure` is seen by both the drift check and the differ. The projection
    filters an `ensure` table to its keys even when the read fetched more: a
    read that fetched every row for another scope's sake must not record the
    application's own inserts as state.

<a id="decision-53"></a>

53. **Values are read back in the engine's spelling, and a cell equal to its
    default is read back as omitted.** *The second half is superseded by 67:
    the read now reports both readings and the side chooses.* Every cell is rendered by the server
    with a fixed `CONVERT` style; only `bit` and the integer types come back
    typed, because those are the shapes a declaration writes unquoted. Per
    cell the engine is asked whether the value equals the column's default
    expression, and a match is omitted — that is what lets the omitted
    spelling round-trip, and it is the same "the database is the normalizer"
    rule that expressions already follow. The cost is the same one check
    constraints have: a declaration that spells a value or a default
    differently from the engine is restated on every connected plan, and the
    remedy is `pull --data`, which shows the engine's spelling. A default the
    engine cannot evaluate to the stored value (`SYSUTCDATETIME()`, `NEWID()`)
    is read back explicit for the same reason, visibly rather than guessed. A
    NULL in a column with no default is omitted too, because `cell()` already
    reads the two spellings as one there; two reads of one table have to
    produce one `Row`, or `StateSnapshot::matches` would disagree with itself.

<a id="decision-54"></a>

54. **A table whose live key is not a single column is unreadable, and the
    whole read fails.** Skipping it would record `data: None` — "declares no
    rows" — and the next drift check would be blind to the rows it exists to
    watch. "Absent", "empty" and "unreadable" are three answers, and only the
    read failing keeps them apart. `status` reports the failure as
    `unreachable` with the reason rather than as "ok".

<a id="decision-67"></a>

67. **A cell at its default is read in the spelling of whoever reads it.**
    The catalog holds `'Unlabelled'`, not whether the row was written as
    `label: Unlabelled` or by omitting `label`; folding "equals the default"
    into "omitted" at the read (53) made the explicit spelling compare
    unequal to itself and be restated on every connected plan. The read now
    returns the value *and* the flag (`ObservedRow`), and the projection
    into a side's view — the recorded snapshot for a drift check, the
    declarations for the plan base and the rehearsal, nothing for `pull` —
    keeps a cell explicit where that side spells it and omits it where that
    side omits it. Both spellings round-trip; a hand edit that sets an
    explicit cell to its default is still seen, because the reference spells
    it and the read keeps the value.

<a id="decision-68"></a>

68. **Only a literal default is compared by the read; every other default is
    taken at the declaration's word.** The `CASE` that asked "equals the
    default" ran the default expression once per row: `NEWID()` was
    harmless, a sequence was advanced by a drift check, and `NEXT VALUE FOR`
    is not legal in a `CASE` at all, so the table was unreadable. A default
    is compared only when it is a number, a string, `NULL` or a hex constant
    under any parentheses (`rows::is_constant`, conservative on purpose); a
    cell whose default is anything else is reported as at its default, so it
    is omitted where the row omits it and explicit where the row spells it.
    The cost is stated in the module docs: a hand edit to an omitted cell of
    that kind is not seen. The alternative — reading it explicit — restated
    `= DEFAULT` on every plan, and for `SYSUTCDATETIME()` that *rewrote the
    timestamp on every apply*, which is worse than not looking. *Refined by
    80: "reported as at its default" became "reported as unknown".*

<a id="decision-70"></a>

70. **A `data:` block may not put a value into a binary column.** Every row
    value goes to the engine as a string literal and the engine converts it
    (decision 53's "the database is the normalizer"), which is right for
    numbers, dates and text and **wrong for bytes**: `N'0x01'` into a
    `varbinary` stores the characters `0x01`, not the byte, and a key read
    back as `0x01` then fails to select its own row. The emitter is handed a
    change and no types, on purpose (a plan applies with no checkout), so it
    cannot render the literal differently; and inventing a typed literal in
    the plan format for one column family is a format change this review
    fix does not get to make. `validate` therefore refuses a binary key and
    a row that sets a binary cell, by name, and a `pull --data` of such a
    table produces a declaration that says why it is refused. Lifting this
    means a typed value in the model (`Value::Binary`), which is an ADR.

<a id="decision-71"></a>

71. **A declared key is read back under the declaration's spelling, and the
    engine says which row it names.** `01` for an `int` key comes back as
    `1`, and the row differ saw two keys: an insert of `01`, a delete of
    `1`, every connected plan — and an `ensure` block never found its row.
    Deciding on this side which spellings are equal would be a second
    normalizer beside the engine (53); instead the read sends every key a
    side spells (`RowScope::Every { known }`, `Keys`) through a `VALUES`
    join, the engine answers with its own spelling of the row each one
    names, and `ObservedTable::aliases` carries the answer. A side then reads
    its rows back under its own keys (`rows_as`, `row`), `pull` under the
    engine's, and the comparison the read makes is the one the emitter's
    `WHERE [id] = N'01'` will make. Measured on a live server.

<a id="decision-74"></a>

74. **Two spellings of one row on one side are refused, never reconciled.**
    With keys read back through the engine (71), `1` and `01` declared side
    by side both alias the row `1`; keeping either plans an insert of the
    other, which the primary key refuses at apply. `validate` cannot see it
    (which spellings are equal is the engine's call, and `a`/`A` depends on
    the collation), so the projection refuses it (`RowConflict`) and every
    connected command reports it by both names. One side spelling `01` and
    the other `1` is not a conflict: each side is asked about its own keys.

<a id="decision-80"></a>

80. **A cell whose default was never asked about is unknown, not at its
    default.** 68 folded the two: a `NEWID()` key and a `GETDATE()` stamp
    were reported at their default, and `pull --data`, which has no row of
    its own to consult, omitted every such cell — a block that, rebuilt,
    generated fresh keys and broke every child row's foreign key. The read
    now reports three answers (`ObservedRow::at_default`, `unknown`, or
    neither), and the unknown cell is omitted only where the side's own row
    omits it; with no side at all it is kept, because a generated value is a
    value the block has to carry. A confirmed default is still omitted for
    `pull`: it *is* the default, and the shortest true block says so.

<a id="decision-87"></a>

87. **A declared cell must be of the kind its column reads back as, and a
    `sql_variant` cannot hold a declared value.** A bare `1` in a `varchar`
    column is stored as text and read back as text; a quoted `"1"` in an
    `int`, a `1` in a `bit`, come back as another kind — and every connected
    plan then restates the same update, forever, against a database that
    already agrees. Normalizing the declaration needs the column types in
    the differ, which is dialect knowledge it does not hold, and normalizing
    the read would hide the mismatch rather than remove it; so `validate`
    refuses the cell by name, with the spelling that fits (70's shape). A
    `sql_variant` cell is refused for 70's reason turned around: the text
    goes in, but the variant's base type does not come back out, and a
    pulled `int` variant would be written back as an `nvarchar` one — the
    same digits, a different value to `SQL_VARIANT_PROPERTY`.

<a id="decision-90"></a>

90. **A spatial cell cannot hold a declared value.** `geometry` and
    `geography` read back as WKT through `ToString()`, which carries no SRID;
    written back as text the engine assigns the default one, and `verify`
    keeps reading the same WKT and calling it clean. 70's shape again:
    refused by name as a key and as a set cell, with a typed value in the
    model as the way to lift it.

<a id="decision-94"></a>

94. **A non-key IDENTITY column is never read back.** A declaration cannot
    set it (the model refuses the cell) and an `UPDATE` cannot change it, so
    it is the engine's column and nobody else's. Read back, its generated
    value met the omission every declared row has to make, and every
    connected plan restated an `UPDATE` the engine refuses — a project with
    such a table could plan and never apply. The row query leaves the column
    out, so both sides omit it and omission agrees with omission; `pull
    --data` writes a block without it, which is the block the model accepts.
    The key is the one identity a row may pin (ADR-0004), and it is read as
    before.

<a id="decision-99"></a>

99. **A row key has to be spellable in its key column's type.** The key
    travels as a string literal like any cell (70), and the alias query
    that lets the engine judge a spelling (71) has no table to ask on a
    table this plan creates — so `not-an-int` for an `int` key passed
    `validate` and the accepted plan failed at its first insert. `validate`
    checks each key against the kind the column reads back as: an integer
    key is digits with an optional sign, a `bit` key one of its four
    spellings, text takes anything (a decimal reads back as text and stays
    the engine's to judge).

<a id="decision-101"></a>

101. **A declared text is refused before it is written unless the engine
    reads it back as written.** `"1.5"` in a `decimal(5,2)` is stored and
    read back as `1.50`, `"ab "` in a `char(5)` as `ab`, and every connected
    plan then restated an update that changed nothing (the same kind on both
    sides this time, 87's fix could not see it). Neither the model nor the
    dialect can spell a value the engine's way without becoming the engine —
    71 refused to invent a normalizer for keys for exactly this reason — so
    every connected command that writes or compares rows (`plan --db`,
    `bootstrap`, the `dev` rehearsal) first sends each declared text cell
    through `TRY_CONVERT` into its column's type and back through the
    read-back's own rendering, and refuses the declaration with the spelling
    to write. A text the type cannot read at all (NULL from `TRY_CONVERT`)
    is refused the same way, which also covers a key on a table this plan
    creates: the alias query (71) has no table to ask yet. Keys are checked
    only for that, since their spelling is aliased at read time.

<a id="decision-103"></a>

103. **`validate` refuses a key whose text no spelling of its type has.**
    A number with letters, a GUID of the wrong length, a date with no
    digit: conservative on purpose, since the engine accepts more spellings
    than any rule here would list (`20260903` is a date), and the engine is
    asked before anything is written (101). What this catches, it catches
    offline; what it lets through, the connected commands do not.

<a id="decision-104"></a>

104. **An integer outside what its column holds is refused offline.** `256`
    in a `tinyint`, `-1` in one, `32768` in a `smallint`: the right kind
    (87), and the engine refuses the insert at apply; the spelling probe
    (101) asks the engine only about text, since an integer is spelled by
    the model. The bounds are the type's own and never change, so `validate`
    names them — a key as well as a cell — and a `bigint` holds every value
    the model can carry.

<a id="decision-106"></a>

106. **Two declared keys the engine reads as one row are refused before
    anything is written.** 74 refused them through the alias query, which
    asks the engine which existing row each spelling names — and on a table
    the plan creates, or one that holds neither row yet, nothing joins and
    both pass, so the plan inserts twice and fails on the second. The
    spelling probe (101) now also groups the keys by what the engine reads
    them as (`GROUP BY TRY_CONVERT(<type>, key)`, under the database's own
    collation) and refuses each group of more than one with the same
    message: `1` and `01` for an `int`, `a` and `A` under a
    case-insensitive collation. A column collation that differs from the
    database's is not modelled, and is the one thing this grouping cannot
    see.

<a id="decision-108"></a>

108. **A plan that changes the type of a key column is refused while a
    declared key is spelled differently from the stored one.** The alias
    mapping (71) holds under the type the column has now — `01` is the
    stored `1` under `int` — and a plan that makes the column `varchar`
    would carry that mapping into a type that does not make it: the base
    row is rekeyed to `01`, no row change is emitted, and the next `ensure`
    plan inserts a second row. Refused by name, with the remedy in order:
    write the keys as the engine spells them, apply, then change the type.

<a id="decision-115"></a>

115. **`money` and `smallmoney` are read with conversion style 2.** The type
    holds four decimal places and the default style renders two: measured on
    a live server, `1.0001` came back `1.00` and `2.5678` came back `2.57`.
    A `pull` then wrote a declaration for a value the table does not hold,
    and `verify` compared the two truncations and reported clean — a cell
    that could never be seen to differ. Style 2 renders all four and
    round-trips. It also renders `1234567.891` as `1234567.8910`, which is
    the engine's spelling and therefore the one a declaration has to use;
    the spelling probe (101) already says so before anything is written.

<a id="decision-131"></a>

131. **Two declared keys are compared under the key column's collation, not
    the database's.** The collision query (106) converts `VALUES` literals
    and groups them, and a literal carries the *database's* default
    collation. On a case-sensitive database with a case-insensitive key
    column, `a` and `A` are one row and the query reported no collision — the
    plan then emitted two inserts, the second of which the primary key
    refuses; on a case-insensitive database with a case-sensitive column, two
    distinct keys were refused as one. The statement now reads the column's
    own collation from `sys.columns` and collates the grouping by it.

    A collation is a name, not a value, so it cannot be bound and the
    statement is built around it: only names of letters, digits and `_` are
    concatenated, which every real collation name is. A table the plan has
    yet to create has no column to read, and falls back to the database's
    default — which is the collation its column will be created with, since
    the emitter writes no `COLLATE`. Nothing outside text has a collation,
    and `COLLATE` on a number is an error rather than a no-op, so a non-text
    key keeps the plain query.

<a id="decision-235"></a>

235. **Reference data is asked for its own DML, on the table, and only for
    what its declaration can emit.** `ALTER ON SCHEMA` confers no `INSERT`,
    `UPDATE` or `DELETE`, and a `data:` block makes the emitter write all three
    against the **managed** tables. Neither appeared in `doctor`'s list at any
    scope that covered them: the `INSERT` and `DELETE` on it were
    `Needed::Ledger`, on the two `dbo` tables, and `UPDATE` was absent
    altogether. So an account granted exactly the list `doctor` printed passed
    readiness with exit 0, `apply` took the lock and ran the DDL, and the first
    row died on "INSERT permission was denied" — under `--staged`, after
    earlier checkpoints had already committed, which is the failure this
    command exists to prevent.

    **Asked on the table, not on its schema.** The first version of this asked
    at schema scope, because that is what the issue suggested and what the
    `Managed` entries beside it do. Measured on the pinned image, it is wrong
    in both directions:

    | held | `OBJECT` | `SCHEMA` | statement runs |
    | --- | --- | --- | --- |
    | `GRANT INSERT ON app.t` alone | 1 | 0 | yes |
    | `GRANT INSERT ON SCHEMA::app` + `DENY INSERT ON app.t` | 0 | 1 | no |

    The first row is a careful DBA granting on exactly the table that carries
    declared rows — reported as a gap they do not have, which is the
    "make it db_owner" pressure this list exists to refuse, and the same
    mistake `Needed::Ledger` was moved to object scope to fix. The second is
    worse and is this entry's own bug one securable out: `doctor` says ready
    and `apply` dies on the first row.

    The `Managed` entries stay at schema scope and that is not the same shape:
    `ALTER` and the probes' `SELECT` are needed on every table in the schema,
    including the ones the plan is about to create, so there is no finite list
    of objects to ask about. Reference data has one — the tables that declare
    rows — which is what makes the narrower question askable at all.

    Before the table exists there is no object to ask about (`HAS_PERMS_BY_NAME`
    on a name the catalog does not hold answers 0, also measured), so the
    question falls back to the table's schema — the only place a grant *can*
    sit in advance of the deployment that creates the table. That is the
    ledger's shape exactly, dedup included: five tables in one schema that all
    fall back to it produce one gap per permission, not five.

    **Demanded of a project that declares rows and of no other**, for the
    reason `Needed::RoleAdmin` is: whether the project needs it is visible in
    the declarations `doctor` already reads, and DML on a table someone else's
    application also writes to is not a permission to ask for on spec.

    **And only for what the declaration can emit.** What a table demands is
    read off its `data:` block and its columns, which is all `doctor` can see —
    it never looks at a plan. The three permissions are asked for
    independently, because a declaration reaches one and not another:

    | asked for | when |
    | --- | --- |
    | `INSERT` | the block declares a row |
    | `UPDATE` | it declares a row **and** the table has a column a row can hold a value in |
    | `DELETE` | `mode: exact`, declared rows or not |

    `ensure` never emits a `DELETE` — that is the promise the mode makes to a
    table the application also writes to (ADR-0004) — so asking for one would
    demand row-removal rights on the very table that mode was chosen to keep
    pbps out of. `exact` with no declared row is the mirror: "this table must
    be empty" removes and never writes. An `ensure` block with no declared row
    manages no row at all and is asked for nothing.

    `UPDATE` is the one that is easiest to get wrong. The differ builds an
    `UPDATE` only from the columns a row can hold a value in — every one but
    the column the key lives in and the engine's own `IDENTITY`s — and emits it
    only if that came out non-empty. So an enumeration table whose only column
    is its code, which is the commonest reference-data shape there is, inserts
    and deletes and can never update. Demanding `UPDATE` of it reports a gap
    against an account that can run every statement the declaration can
    produce. The rule has one spelling, `Table::row_columns`, which the differ
    uses for the comparison and `doctor` asks from the other side; a second
    copy in the readiness check would drift the first time the differ learned
    to skip another kind of column, and drift in the direction that says
    "ready".

    `DataDemand` is built only through `DataDemand::of`, which reads the
    declaration and answers `None` when it could emit nothing at all — so
    "declares rows and demands nothing" is an absence from the map rather than
    a value in it, and the reading happens in one place rather than at each
    caller. `None` also covers a `data:` block on a table with no
    single-column primary key, which the differ refuses outright: a broken
    declaration, not an empty one, and `validate` reports it beside this.

    **Not covered, and recorded rather than silently missed:** a table this
    plan *renames*. `doctor` asks about declared names, and until `apply` runs
    the object still carries its old one, so the object question finds nothing
    and falls back to the schema. Grants and denies follow an object through
    `sp_rename`, so an object-only grant on the old name is reported missing
    and a `DENY` on it is not seen. It is a narrow conjunction — a rename of a
    data table in the pending plan, plus a grant or deny placed on that one
    object — and the same blind spot is older than this entry and wider than
    it: every name `doctor` asks about comes from the declarations. Filed
    rather than fixed here, so that one fix covers every site.

    A data table whose *schema* the database does not have produces no gap at
    all. Nothing was asked about it — there is no securable to ask about — and
    "unasked" is not "holds nothing"; `absent_schemas` reports it, and
    inventing a gap there would name a securable no `GRANT` can reach yet.

<a id="decision-319"></a>

319. **A declared value is rendered as an `E'…'` with its backslashes doubled,
    and a `bytea` as `decode('…','hex')` — an encoding rule, not a settings
    rule.** ADR-0013 §3. A write takes no settings scope of its own (the scope
    would also be a scope over every trigger the write fires), so
    `standard_conforming_strings` reaches the literals pbps renders.
    **Measured on 18.6**, the same statement under the two settings:

    ```text
    'a\nb'  under on: length 4      under off: length 3
    E'a\\nb' under either:           length 4
    '\x0102'::bytea under off:  accepted, storing 3 bytes
    decode('0102','hex') under off:  accepted, storing 2 bytes
    ```

    The `bytea` line is why a refusal list cannot cover this: no error, no
    refusal, a different value in the table. The dependency is in **pbps's own
    rendering**, not in the declaration, so the fix belongs in the renderer. An
    `E'…'` takes backslash escapes under either setting, and `decode` carries
    no backslash at all.

    The form stays `unknown` to the type system — measured, `pg_typeof(E'1')`
    is `unknown`, exactly as for a plain literal — so `"id" = E'1'` on an
    `integer` column is still the integer comparison, and a value never has to
    carry a type the plan may not have.

<a id="decision-321"></a>

321. **An identity-keyed `data:` block is refused on PostgreSQL, by `validate`
    and again by the emitter.** ADR-0013 §2. `SET IDENTITY_INSERT` keeps SQL
    Server's seed at least as high as the value written; **measured**, this
    engine's `OVERRIDING SYSTEM VALUE` does not — two pinned rows leave the
    sequence at its start and the *application's* next insert fails on the
    primary key, after a deployment that verified clean. ADR-0013 §2 takes six
    measured obstacles to close that by advancing the sequence and closes none
    of them: `nextval` walks past any lock, a sequence cannot be locked at all,
    an allocation already handed out cannot be recalled, the advance survives
    the rollback of a failed apply, and a `CYCLE` sequence makes it
    non-terminating. Refusing removes all six at once, which is the house
    preference — make the failure unrepresentable rather than handled.

    The message names the sequence (`<table>_<column>_seq`, which is the name
    this engine derives) and both ways forward: a natural key, or `pbps
    baseline` over rows kept outside pbps.

<a id="decision-322"></a>

322. **The row read-back renders every column with one expression, `CAST(… AS
    text)`, and the canonical settings are what fix the spelling.** The SQL
    Server reader pins a `CONVERT` style per type family; this engine has no
    per-expression style and a handful of session settings that decide every
    value of a type at once. So the read runs inside the same canonical scope
    the pull uses (`catalog::read_rows` opens its own transaction for it,
    because the scope is `set_config(…, is_local)`), and a second mechanism on
    top of it would be a second thing to keep true. Measured under those
    settings: `numeric(5,2)` renders `1.50`, `bytea` `\x0102`, `interval`
    `P1DT2H`, and a `character(5)` holding `'ab'` renders `ab` — the padding
    dropped, which is what the engine itself ignores when it compares.

<a id="decision-324"></a>

324. **The spelling queries are fenced with `OFFSET 0`, and the fence is
    load-bearing for exactly the types that fold.** The check asks the engine
    what it makes of each declared literal, guarding the cast with
    `pg_input_is_valid` inside a `CASE`. **Measured**, the `CASE` protects the
    cast with two rows in the `VALUES` and does not with one: the planner folds
    a single-row list into a `Result` node and evaluates the cast while
    planning, so the query raises `invalid input syntax` instead of reporting
    the value it exists to report. Which types it can fold is the type's own:
    `numeric`, `integer` and `uuid` read their text through an immutable input
    function and fold; `date` and `character varying` do not.

    That second sentence is why the live test declares a `numeric` cell. The
    first version of it used a `date`, passed with the fence removed, and
    proved nothing — a guard that holds for two rows and not for one, pinned by
    a test that could not tell.

<a id="decision-327"></a>

327. **Offline `validate` says it did not judge the row keys, rather than
    reporting clean.** ADR-0013 §5. Whether two declared keys are one row is the
    engine's question — by the type's conversion, and for a character type by
    the live column's collation, which this ADR keeps out of `pbps-model`.
    **Measured**, the flag that looks like the answer is not one: under one
    nondeterministic collation `'New'` and `'new'` are one key and the second
    insert fails, and under another they are two valid keys, so reading
    `collisdeterministic` would refuse a perfectly good declaration. The
    connected check asks the engine about the actual keys, under the column's
    own collation, through a `COLLATE` clause written only where the type takes
    one — measured, `COLLATE` on a `numeric` is an error, not a no-op.

    That leaves offline `validate` with a question it cannot ask, and "no
    findings" would be the wrong answer to it. `Dialect::declaration_notes` is
    a third channel beside errors and warnings: not a problem with the
    declarations, a statement about what this run could not check. Empty by
    default, so a dialect whose offline checks are complete says nothing.

<a id="decision-452"></a>

452. **Declaration key rules follow PostgreSQL's engine, not the sibling
     validator.** Issue #156 adds the missing structural checks to
     `Postgres::validate_table`: nonempty existing local key columns, matching
     nonempty foreign-key lists, and nonempty check/filter expressions. The nullable primary-key
     rule remains 266's distinct refusal of a silent rewrite.

     **Measured on PostgreSQL 18.6:** primary and unique constraints reject a
     repeated column (`42701`), but an index accepts repeated keys, repeated
     included columns, and a key repeated in `INCLUDE`. A foreign key also
     accepts repeated local columns against a distinct composite unique key.
     Refusing these would reject valid declarations. The stock build's limit is 32 columns
     **including INCLUDE**, not 32 key columns plus unlimited payload: 32
     accepts and 33 refuses with `54011`, for indexes and constraint-backed
     indexes alike. `SHOW max_index_keys` pins that test engine's capability:
     the limit is a build setting, not an offline dialect invariant. Wider
     declarations are therefore left to the target engine rather than refused
     against a stock-build constant. The tests assert offline acceptance at
     32 and 33 and stock-engine refusal at 33; they do not claim a rebuilt
     64-column server was tested.

     Stock PostgreSQL refuses `json` keys for lack of a default
     btree operator class (`42704`), but accepts json included payload and the
     other admitted type families as keys. That is **not an offline refusal**:
     measured, installing a default json btree operator class makes primary,
     unique, foreign and index keys legal, and the catalog reader resolves
     installed default classes rather than hard-coding the stock ones. Key-type
     eligibility therefore stays with the server; refusing json offline would
     reject a valid declaration on such a server. Unknown types retain the
     ordinary closed-catalogue finding, independently of their use in a key.

     Empty expressions mean ASCII whitespace only: measured, a non-breaking
     space can name a boolean column and is a legal unquoted check/filter
     expression. Rust's Unicode `trim` would refuse that valid declaration, so
     both expression checks use `trim_ascii` and pin Unicode identifier cases.
     **Amended by 504**: `trim_ascii` is the right *danger* and the wrong
     *set* — it omits the vertical tab, which this engine does separate tokens
     with, and no character class sees a comment. Both checks now ask the
     engine's own lexis (`Lexicon::expression_in`); the Unicode identifier
     cases this paragraph pins are unchanged and are why literals are never
     blanked.

     The unit tests pin each structural rule and aggregate independent errors.
     The live declaration matrix sends the emitter's statements to the engine,
     asserting the exact SQLSTATE for refusals and successful creation for
     legal repetitions, type families, and the 32-column boundary. This is why
     the broader #156 and the overlapping #175 cannot be implemented by copying
     SQL Server's `key_columns` unchanged.

<a id="decision-469"></a>

469. **Offline row-key spelling is a separate question from collisions (issue #211).**
     A single key can fail to convert or read back differently without having
     a second key to collide with. Both dialects emit a `dialect.not-checked`
     note for a nonempty data block with a single-column key whose conversion
     is not identity. PostgreSQL retains its existing multi-key collision note;
     neither note changes validation success or replaces the connected check.

     Normalize the declared type, not its `ValueKind`: that row-value enum
     also calls UUID, date and decimal renderings text. PostgreSQL `text` and
     unbounded `character varying` preserve spelling. SQL Server's unbounded
     `nvarchar` preserves Unicode, while `varchar` also depends on the live
     code page. Bounded varying text can truncate and fixed-width text can
     pad or trim, so these still need the note. An invalid key shape or unknown
     type is left to the existing validation errors rather than adding a note
     about a conversion the declaration cannot identify.

     Measurements on PostgreSQL 18.6 and SQL Server 17.0.4075.5 show integer
     `01` becoming `1`, bounded text truncation and fixed-width text changes.
     PostgreSQL also canonicalizes UUID case; SQL Server can replace Unicode
     characters when converting to a non-Unicode code page. The CLI regression
     pins the single-key diagnostic and unchanged successful exit, including
     PostgreSQL malformed UUID and identity-text negative cases. Unit cases
     cover aliases, non-string scalar renderings, empty data and invalid key
     shapes; the live collation test retains the separate collision question.

<a id="decision-508"></a>

508. **The identity-text exemption is about the conversion, and one key defeats
     it; SQL Server's note may not promise a spelling check it does not make
     (issues #526, #528).** 469 exempted the conversions that hand a key back
     unchanged — PostgreSQL `text` and unbounded `varchar`, SQL Server
     `nvarchar(max)` — from `validate`'s offline key note, because there is
     nothing to warn about when nothing can change. Two reviews then found the
     two ends of that sentence wrong in two different ways.

     **PostgreSQL cannot hold U+0000 at all**, whatever the conversion would do
     with it, so the exemption let a declaration through that the engine refuses
     outright. Measured on 18.6, four routes and four answers:

     | Route | Answer |
     | --- | --- |
     | `SELECT chr(0)::text` | `54000: null character not permitted` |
     | `INSERT ... VALUES (E'bad\000key')` | `22021: invalid byte sequence for encoding "UTF8": 0x00` |
     | `INSERT ... VALUES (U&'bad\0000key')` | `42601`, the parser: invalid Unicode escape value |
     | a real NUL byte in the statement text | never reaches the server — the driver refuses to encode the message |

     The fourth is the one a deployment actually takes, and it is the reason a
     note rather than silence matters here: there is no SQLSTATE to report and
     no server sentence to quote, so an offline note is the only thing that can
     name such a declaration before someone runs into it. The exemption now
     stands unless a declared key carries U+0000; an ordinary unbounded-text key
     keeps it, which is what stops this from becoming a blanket note on `text`.

     A test of this has to prove its own fixture first. YAML `"bad\0key"` is one
     character only if the loader reads that escape, and a test that silently
     carried a backslash and a zero would pass for the wrong reason — so the
     fixture asserts that `"bad\0key"` and `"bad\u0000key"` are the **same**
     mapping key before asserting anything about the note. The same trap caught
     this investigation twice: a shell heredoc turned `\000` into two characters,
     and a Rust literal turned `E'bad\000key'` into a real NUL byte in the
     statement text — which is how the driver route above was found.

     **SQL Server's half of the note was a promise it does not keep.** Both
     dialects said `plan --db` "checks the key conversion and spelling against
     the live column". On that engine a key's spelling is aliased at read time
     by design (71, 101): `catalog::misspelt` treats any readable spelling as
     agreement for a key — `(None, Some(_))` — and compares spellings only for a
     cell. Measured on 17.0.4075.5, an `int` key declared `01` is stored and
     read back as `1`, `01` and `1` are one row (`Msg 2627`), and `'nope'` is
     `Msg 245`. So two of the three questions are real and one is not, and the
     note now names the two: conversion, and convergence. PostgreSQL keeps the
     stronger sentence, because `pbps-pg::catalog::misspelt` really does require
     the read-back to equal the declared spelling for a key as well as a cell.

     One thing the SQL Server side does not need: a NUL carve-out. Measured,
     `nvarchar` holds U+0000 without complaint (`LEN` 7, `UNICODE` of the fourth
     character 0), so the difference is PostgreSQL's, not a gap in the sibling
     validator. And no SQL Server declaration reaches the exemption anyway —
     `nvarchar(max)` is the only conversion that preserves a key there and the
     engine will not take it as a key column, which is why the offline test's
     negative controls are both PostgreSQL's.

<a id="decision-540"></a>

540. **SQL Server floats are rendered through a bounded `varchar(99)` before
     widening to `nvarchar(max)`.** (#286, #288.) SPEC §4.6 has a cell spelled
     the engine's way, and DECISIONS 149 has every recorded-cell comparison
     share one rendering, so `float` and `real` are read with the
     round-trippable style 3. The direct form is not usable on SQL Server
     17.0.4075.5 (2025 RTM-CU8), measured on the pinned test image:

     ```text
     CONVERT(nvarchar(max), CAST(-255 AS float), 3)
       Msg 8115: Arithmetic overflow error converting expression to data type nvarchar.
     CONVERT(varchar(max), CAST(-255 AS float), 3)
       Msg 232: Arithmetic overflow error for type varchar, value = -255.000000.
     CONVERT(nvarchar(max), CONVERT(varchar(99), CAST(-255 AS float), 3))
       -2.5500000000000000e+002
     ```

     The same holds for `-1.7976931348623157E+308`, while `0.1` and a `real`
     `1.5` render identically on all three paths. So it is a fault of the MAX
     conversion path on valid values, not of style 3 or of the value, and the
     inner conversion to a bounded type is the workaround; the outer one only
     widens text and applies no style.

     Why 99 rather than an exact width: style 3 always prints a signed
     17-digit mantissa and a signed three-digit exponent, so the longest
     rendering is 24 characters (`-1.7976931348623157e+308`). Any bound at or
     above that is equivalent, and one below it fails loudly rather than
     truncating: measured, `varchar(23)` raises the same Msg 232 for that
     value and `varchar(24)` renders it. A generous bound costs nothing and
     keeps a future engine that pads differently from failing every read of
     an extreme value. The rendering read back is unchanged, so no recorded
     state moves.
