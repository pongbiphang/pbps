# Type changes, probes and estimates

Type-change risk, the conversion and key probes that measure existing rows, and
cost estimates. Part of the [decision record](../DECISIONS.md), which says how
to add an entry here.

<a id="decision-149"></a>

149. **A retyped column carries both of its types, and the engine converts
    between them.** 146 read "neither type can compare this cell" as "hold it
    to nothing", and that dropped more than a comparison: the precondition on
    a row write is also the *stale-row* guard (122, 136, 143). A revision that
    retypes a column and changes one of that table's declared rows therefore
    updated or deleted the row without checking it at all — and that is the
    cell most likely to have moved, since a column being retyped is a column
    somebody is working on. Another session edits it after the plan's read;
    `AlterColumnType` converts whatever it now holds; the `UPDATE` overwrites
    it or the `DELETE` removes it, `@@ROWCOUNT` is 1, and the apply records
    the loss as its own result.
    What 146 ruled out was converting the recorded text straight into the new
    type — correctly: `CONVERT(int, N'1.50')` is Msg 245, and the value the
    column holds came from a `decimal(5,2)`, not from that text. What it did
    not try is the two-step the engine itself took: the recorded text back
    into the type that *rendered* it, and then the conversion the `ALTER` ran.
    So `UpdateRow` and `DeleteRow` carry the pair — `types` the type the text
    was read in, `after_types` the type the column has when the statement runs
    — and the emitter writes
    `read_expr(col, now) = read_expr(TRY_CONVERT(now, TRY_CONVERT(read, N'…')), now)`.
    The tool computes no conversion; it asks for the one already performed.
    Measured on SQL Server 2025, across every rendering that carries a style:
    `decimal(5,2)`→`int` (`1.50`→`1`, truncating, and `TRY_CONVERT` truncates
    identically), `varchar(10)`→`date` under style 126, `varbinary`→`binary`
    under style 1, `char(10)`↔`varchar(10)` with the padding both sides agree
    on, `float`→`decimal` under style 3 and `money`→`decimal` under style 2.
    Every untouched row matched; a row edited to `9.25` beforehand did not,
    and the apply rolled back naming the row.
    `TRY_CONVERT`, not `CONVERT`: a recorded value the new type cannot hold
    means the cell is not what the plan recorded, and "the row is not as the
    plan recorded it" is a better answer than a conversion error raised from
    inside the write. A type with no comparison at either end — `xml`, `text`,
    the spatial types — still holds nothing, which is 146's answer kept for
    the case it was actually right about.
    **One thing no predicate can see**, and it is named rather than left to be
    discovered: an edit the conversion erases. `1.50` and `1.99` are both `1`
    once the column is `int`, so a session that moved the cell between them is
    invisible — the column no longer holds what would tell them apart. The
    baseline checksum still covers it up to the moment `apply` reads the
    state.

    **Amended by 471: "a type with no comparison at either end" is no longer a
    case.** That sentence kept 146's answer for `xml`, `text` and the spatial
    types; the comparison is of text now, so all of them are held. What is left
    of 146's answer is narrower, and is about the retype alone: a column this
    plan retypes whose old type cannot be spelled back from its own rendering
    — `image`, `geometry`, `geography` — is carried and held by nothing.

<a id="decision-151"></a>

151. **The new foreign key is probed against the rows the plan will leave,
    not the ones it finds.** `AddForeignKey` sorts at 13 and the row changes
    at 11 and 12, so the probe — which runs before statement one — was
    answering about a table that will not exist in that shape by the time the
    constraint is created. Two faults, and the first is the one that matters:
    a plan that inserts the parent rows its children need, or repairs the
    orphans by update or delete, was **refused for the violation it was
    written to remove**. Measured: five customers, three of them orphaned, and
    a plan that adds the missing region, points one orphan at an existing one
    and deletes the third — the probe counted 3 and the engine, run in plan
    order, created the constraint without complaint. There is no workaround
    but splitting one revision into two deployments. The second fault is the
    mirror: a child row the plan itself inserts with no parent was counted by
    nothing, so under `apply --staged` the insert commits and the constraint
    then fails, with the environment left half-changed.
    112's sweep did not see either, because it asked only whether a probe
    could *miss* a violation and answered that for this one correctly: inside
    a transaction the engine's refusal is loud and total. A false refusal is
    not loud — it is a plan that never runs.
    The fix asks the engine for the arithmetic rather than doing it here.
    `rows_after` builds each side of the comparison as a derived table: what
    is stored, minus the rows this plan deletes and the ones it rewrites *in
    the key's own columns*, union what it writes — a literal row per insert,
    and per rewriting update a row taking the changed cells from the plan and
    the rest from the table. Both sides go through it, so the parent's rows
    include the ones the plan inserts and exclude the ones it deletes, and a
    table this plan creates simply has no stored branch. That last case was
    unprobed before and is ADR-0004's own flow: create the parent, insert its
    rows, add the key that references them.
    A row is dropped from the comparison where the plan writes a value the
    probe cannot evaluate into one of the key's columns — a default that is
    not a literal, which has no value before it runs (117). That is the
    direction every other probe leans, and here it is the safe one: the engine
    refuses such a row loudly inside the transaction, where a guess could
    refuse a plan that is perfectly good.
    **Still not counted, and deliberately:** a parent row this plan deletes is
    excluded from the parent side, so a child left pointing at it *is* now
    reported — but a parent row an `UpdateRow` moves *off* a referenced value
    is not, because a foreign key may reference a `UNIQUE` rather than the
    primary key and an update can write those columns. The probe stays
    over-permissive there, which is 112's accepted mode for this probe: the
    engine refuses, loudly, inside the transaction.

<a id="decision-152"></a>

152. **A probe that unions rows names its columns and states their type.**
    151 built each side of the new-foreign-key probe as a `UNION ALL` of what
    is stored and what the plan writes, and left both to the engine's
    defaults. Both defaults are wrong, and each fails in the direction that
    hides it.
    **Names.** A derived table takes its column names from whichever branch
    comes first, and for a table this plan *creates* the first branch is a
    literal projection — which has none. Measured: `Msg 8155, No column name
    was specified for column 1 of 'r'`. `preflight` reports a probe that
    errors as *unchecked* and `apply` proceeds, so the case 151 was proudest
    of adding — ADR-0004's create the parent, insert its rows, add the key —
    was the one case it silently never checked. Every branch is aliased now,
    not just the first, so no reordering can bring it back.
    **Types.** `UNION ALL` reconciles its branches by data-type precedence.
    Measured: a child column of `int` beside a planned `N'01'` makes the
    literal integer `1`, which matched a parent holding `'1'`; the probe
    counted none, and `ALTER TABLE ... ADD CONSTRAINT` then failed, because
    once the column is `varchar` the two are different values. Every branch
    is projected through the type the column will have when the constraint is
    created, for the columns this plan gives a type to — one it retypes, one
    it adds, every column of a table it creates. A column it leaves alone
    already holds its final type, and the engine coerces a literal to it the
    same way the `INSERT` will.
    `TRY_CONVERT`, not `CONVERT`: a stored value the new type cannot hold
    makes `CONVERT` throw, and a probe that throws is the *unchecked* silence
    above. Such a row cannot survive the `AlterColumnType` either, and it is
    that change's own conversion probe which counts it and names the column.

<a id="decision-175"></a>

175. **The key probes moved onto the relation the foreign key probe already
    used — and it subsumed 171's substitution.**
    174 named the shape and fixed one of the three: `AddCheck`, `AddUnique`
    and `SetPrimaryKey` all sort after the row changes, and all three read the
    table as it stands. The check could only subtract its deletes, because its
    predicate is arbitrary. These two are the opposite case: a unique
    constraint's columns *are* the constraint, exactly as a foreign key's are,
    so `rows_after` — stored rows minus the deleted and rewritten, union the
    planned ones — is already the relation their statements will meet
    (DECISIONS 151, 164, 165, 171). They group over that now, keyed `k0..kn`
    like the foreign key probe.
    Two things fell out of it, both improvements nobody asked for.
    **171's special case disappeared.** Probing a key over a column this plan
    adds had needed a substitution — the column's post-`ALTER` value in place
    of a read, and dropped from the `GROUP BY` because `GROUP BY NULL` is
    `Msg 164`. Inside a derived table it is a *column*, `k0`, which groups
    like any other; `rows_after` already spells added columns (171), so the
    general mechanism covers the special case and the substitution machinery
    is gone.
    **A table this plan creates is probed now**, where it was skipped
    entirely. That skip's stated reason — "probing it would only produce
    invalid object name" — was true of a probe that named the table and is
    not true of one built from `rows_after`, which spells the declared rows,
    or the typed empty relation where there are none. It is 164's argument one
    constraint over: "none" is an answer rather than the absence of one. A
    plan creating a table with two rows under one unique key is now refused
    before it runs rather than at `ADD CONSTRAINT`.
    That is worth naming as its own lesson, because a test encoded the old
    reason and had to be rewritten rather than repaired: **when a guard's
    reason goes, the test that pins it is testing the reason, not the
    property.** The right move was to state the better property, not to keep
    the old assertion alive.

<a id="decision-227"></a>

227. **`normalize_type`'s contract is stated on the trait, and the `serial`
    family is refused rather than normalized.** The contract: normalization is
    idempotent, **and its output is what introspection reads back for a column
    declared that way**; a spelling for which that is impossible is an error.
    Measured on PostgreSQL 18.6, `smallserial`, `serial` and `bigserial` read
    back as `smallint`, `integer` and `bigint`, each with an owned
    `<table>_<col>_seq`. No normalization makes the declared spelling equal the
    read-back one, so left alone it is a schema that differs from itself on
    every run — the permanent phantom change.

    The refusal is `Invalid`, not `NotBuilt`: "the type catalogue is not built
    yet" sends its reader away to wait for a release, and this one is a
    declaration to change today. It is raised at `validate_table` as well as at
    `normalize_type`, naming the column, and it names what to declare instead —
    which per ADR-0010 §7 also disposes of the sequence-grant problem, since an
    identity column needs no sequence privilege and a `serial` one does.

<a id="decision-240"></a>

240. **The PostgreSQL catalogue is closed, and every bound in it is the
    engine's own — including the two the engine does not enforce.** A name the
    table does not hold is refused, never passed through. Passed through, a
    base name no catalog ever returns makes a column that reads as changed on
    every run and no plan can fix: ADR-0012 §1 names that trap for `text ARRAY`,
    which loads happily as the base name `text array` because spaces are legal
    in one. Both array spellings are refused for the same reason, and so is
    `bpchar` — measured, `bpchar(5)` reads back as `character(5)` and a bare
    `bpchar` reads back as `bpchar`, so any single alias for it would be right
    in one case and wrong in the other.

    The bounds are the engine's and **not SQL Server's**, which is the half that
    had to be measured rather than recalled. `numeric(10,-5)` is legal here and
    reads back as itself, so the SQL Server rule `0 <= scale <= precision` would
    refuse a column this engine will happily make; the scale's real range is
    -1000..=1000 and the precision's is 1..=1000. A bare `numeric` stays
    unbounded rather than gaining the `(18,0)` SQL Server fills in, `character`
    gains the `(1)` that SQL Server also gives it, and `character varying`
    gains nothing — three different answers to "what does an omitted argument
    mean", and one rule for all three would have been wrong twice.

    Two bounds are enforced here **although the engine does not enforce them**,
    and that is the identifier-truncation shape one layer down: measured,
    `interval(7)` is stored as `interval(6)` and `time(7)` as `time(6)`, with no
    error. A declaration the engine quietly reduces records itself at one value
    and reads back at another, which is a drift report that never goes quiet.

<a id="decision-241"></a>

241. **The cost of a change stays out of `TypeChangeRisk`.** ADR-0012 §3's
    boundary, written down with the catalogue rather than with the estimate that
    will use it, because that is the moment the pressure to blur it is lowest.

    **Measured on PostgreSQL 18.6**, by comparing `pg_class.relfilenode` either
    side of the statement — the engine's own answer to "was this table rebuilt",
    not a proxy for it:

    | Change | Rebuilt? | Risk |
    |---|---|---|
    | `integer` → `bigint` | **REWRITE** | `Safe` |
    | `character varying(10)` → `character varying(20)` | no | `Safe` |
    | `character varying(20)` → `text` | no | `Safe` |
    | `text` → `character varying(20)` | **REWRITE** | `Narrowing` |
    | `numeric(10,2)` → `numeric(12,2)` | no | `Safe` |
    | `numeric(10,2)` → `numeric(10,4)` | **REWRITE** | `Narrowing` |
    | `ADD COLUMN d integer DEFAULT 7` | no | — |
    | `ADD COLUMN d uuid DEFAULT gen_random_uuid()` | **REWRITE** | — |
    | `SET NOT NULL`, `DROP COLUMN`, `ADD COLUMN` with no default | no | — |

    `DROP COLUMN` is in that table for a reason that is not about risk at all:
    it rewrites nothing, and it also **does not reclaim the space**. "The drop
    was cheap" and "the table got smaller" are different claims, and an estimate
    that conflated them would be wrong in the direction that surprises an
    operator (ADR-0012 §6).

    The first row is the whole argument: the textbook widening rebuilds a
    million-row table in 410ms against 0.662ms for one that does not, under an
    `AccessExclusiveLock` that blocks readers — and it is `Safe`, correctly,
    because a rewrite loses nothing and cannot fail. Reclassifying it as
    `Narrowing` would lie twice: `Narrowing` tells a reviewer the change may
    *fail or lose data*, and the class's criterion is deliberately
    data-independent (SPEC §7.2) while a rewrite's cost is entirely a function
    of how many rows there are.

    The rewrite is also **not always a property of the declaration**, which is
    why this is a boundary and not a second column in the same table. Measured:
    `timestamp` → `timestamptz` is free under a `UTC` session and rewrites under
    `America/New_York`, and `ADD COLUMN ... DEFAULT` depends on the expression's
    volatility, which this tool does not parse. The estimate (SPEC 14.1) answers
    `unknown` for those rather than guessing *cheap*, which is the direction
    every probe in this project already leans. Phase 5 step 9 builds it; this
    entry is what it is built against.

<a id="decision-242"></a>

242. **A precision on `time` or `timestamp` is refused, because this model
    cannot hold the engine's own spelling of it.** Measured: `timestamptz(3)`
    reads back as `timestamp(3) with time zone` — the modifier goes **inside**
    the name — and `timestamp with time zone(3)` is a *syntax error*. A
    `ColumnType` is a base name followed by its arguments, so there is no value
    `normalize_type` could return that both introspection reads back and the
    emitter can spell, and ADR-0011 Amendment 3 says a spelling for which that
    is impossible is an error rather than something to normalize.

    Refused rather than normalized to the unmodified type: `timestamp(3)` and
    `timestamp` are different columns to the catalog, so folding one into the
    other would be the phantom change `serial` already demonstrated. Refused as
    `NotBuilt` rather than `Unsupported`, because the engine has the feature and
    pbps does not — and a reader sent to PostgreSQL's documentation for a
    limitation of this tool looks in the wrong place. Lifting it is a
    `pbps-model` change, exactly as arrays are (issue #130), and the refusal
    names the declaration to write in the meantime.

<a id="decision-243"></a>

243. **`Incompatible` is defined by a measured matrix, not by a rule.** The
    class means "this engine refuses the conversion outright", so what it must
    agree with is the engine. Twenty types, four hundred ordered pairs, each
    `ALTER TABLE ... ALTER COLUMN ... TYPE` run on an **empty** table — with no
    rows the only thing that can fail is the conversion itself, which is exactly
    the question the class answers. The matrix is in `pbps-pg`'s unit tests and
    the live suite re-measures it against a real server, so the two disagree the
    moment either the engine or the classification moves.

    A rule would have been wrong, and it was: `timestamptz` converts to `timetz`
    and `timestamp` does not, and no property of the two types predicts it.
    `interval` converts to `time` and to neither `timetz` nor `timestamp`, so a
    classification phrased as "anything with a time part" called two refusals a
    narrowing — found by the matrix, in the first run.

    Getting it wrong is expensive in both directions: a pair called
    `Incompatible` that the engine accepts refuses a plan that would have
    worked, and a pair called `Narrowing` that the engine refuses fails half way
    through an apply, after the changes before it have run.

<a id="decision-244"></a>

244. **`Safe` is decided by what a type *holds*, not by how many digits it
    has.** Two rules that look right and are not, both found by review on the
    PostgreSQL catalogue (#131) and both measured:

    - **A digit count is not a magnitude.** `numeric(10,0)` and `integer` are
      both "ten digits", and `9999999999` into an `integer` is `integer out of
      range`. The same holds for `numeric(5,0)` into `smallint` and
      `numeric(19,0)` into `bigint`. The integer types are not powers of ten, so
      the classification carries the largest value each one holds and compares
      *that*.
    - **A decimal that prints back is not a decimal the float holds.** `0.1` in
      a `real` is `0.10000000149011612`, and ten of them sum to `1.0000001`
      where the exact sum is `1.0`. The engine renders the shortest decimal that
      reads back as the same float, so `0.1::real::text` is `0.1` and every
      round trip through text says the value survived. So the question is
      whether the float holds the value **exactly** — no fraction, and no gap
      below its magnitude, which is 2^24 for `real` and 2^53 for `double
      precision`. Measured: `16777217` into a `real` reads back as `16777200`.

    The next round found two more of the same shape, and both are about
    `numeric` being a wider thing than its width says:

    - **`NaN` is a value every `numeric` holds**, whatever its precision —
      measured, `'NaN'::numeric(4,0)` is accepted — and no integer type has one:
      `cannot convert NaN to smallint`. So a `numeric` never reaches an integer
      type safely, however narrow it is, and the widths are not the question.
      (`NaN` and infinity both pass into a float unchanged, so that direction
      keeps its bound.)
    - **A scale larger than the precision still bounds the value.**
      `numeric(2,3)` holds values below `0.1` and `numeric(2,4)` below `0.01`,
      and measured, `0.099` into the second is `numeric field overflow`. The
      integer-part exponent is `p - s` and it is kept **signed**; clamping it at
      "no integer part" made two different capacities compare equal.

    The round after that found the same shape in the date types, where the
    property standing in for the answer was *which components a type stores*:

    - **A `date` reaches further than a `timestamp`.** Measured,
      `'5874897-01-01'::date` is accepted, `'294276-12-31'` is the last date
      that converts, and `'300000-01-01'::date::timestamp` is `date out of range
      for timestamp`. Adding a time to a date looks like the textbook widening
      and is not one. With that, **no change between two date-or-time types is
      `Safe` except a type to itself** — each of the rest drops a component,
      moves with the session's time zone, or runs off the end of the calendar.

    Every one was `Safe`, which is the class that bypasses the gate entirely, so
    every one was a plan approved by nobody that fails or silently changes data
    at the apply. The common shape is worth naming: each rule described a type
    by *one* of its properties — its digit count, its components, its width —
    and each time the property was true and not the whole answer. Both are also **in `pbps-mssql`**, measured on SQL Server 2022:
    `decimal(10,0)` into `int` is `Arithmetic overflow error converting
    expression to data type int`, and `decimal(2,1)` into `real` stores
    `1.000000014901161e-001`. That is issue #135; it is a shipped dialect and a
    change to it needs its own measurements.

    The general lesson is the one the live suite is built around: a
    classification cannot be checked against itself. What catches these is a row
    at the boundary — the largest value the source holds, and a value the target
    cannot represent — put through a real server, with `Safe` asserted as *the
    statement runs and the value does not change*.

<a id="decision-263"></a>

263. **A type change this engine refuses outright is refused by the emitter,
    with the clause named.** ADR-0012 §5 decides that no `USING` is emitted; the
    placement is this step's. The catalogue already knows which conversions the
    engine will not make — `TypeChangeRisk::Incompatible` is defined as exactly
    those (DECISIONS 243) — so the refusal is made where the plan is built, in
    the message that names `USING` and the two-step remedy, rather than left to
    a server error halfway through an apply.

    Both ends are normalized before the catalogue is asked, and that is not
    hygiene: the families are keyed on the spelling the engine gives back, so an
    unnormalized `varchar(10)` is a type the catalogue does not know and *every*
    change from one reads as `Incompatible`. A widening would have been refused
    for needing a clause it does not need. The trait says the caller normalizes
    first; a dialect that only works when it is called correctly is a trap, and
    normalizing twice is free (the same correction as DECISIONS 244).

<a id="decision-268"></a>

268. **A type change the session's `TimeZone` would answer is refused, the way
    a `USING` clause is.** `timestamp` → `timestamptz` and its relatives do not
    fail on this engine — they are *answered* from the session's `TimeZone`.
    Measured, one stored value under one `ALTER`, twice:

    ```text
    timestamp -> timestamptz, stored 2026-01-02 12:00
      TimeZone = UTC               -> 2026-01-02 12:00:00 UTC
      TimeZone = America/New_York  -> 2026-01-02 17:00:00 UTC

    timestamptz -> timetz, stored 2026-01-02 12:00:00+00
      TimeZone = UTC               -> 12:00:00+00
      TimeZone = America/New_York  -> 07:00:00-05
    ```

    **The second shape arrived a round later and it is what the rule is.** The
    predicate was first written as "the offset is gained or lost", which is the
    shape the first measurement had. `timestamptz` → `timetz` keeps its offset
    on both sides and is still the session's answer, because what moves is the
    *date* part: a value is being read out of a day, and a zone decides which
    day it was in. So the question is not "does the offset change" but "does
    the session decide", and the predicate is now `(ao || bo) && (ao != bo ||
    ad != bd)` — a zone has to be involved at all, and then either end of it
    moves.

    **With one exception, and finding it took a third round.** `timetz` → `time`
    involves a zone and moves it and is still *not* the session's: `timetz`
    stores a local time and its offset side by side, so dropping the offset
    keeps the time that is already there. Measured, `12:00:00+03` becomes
    `12:00:00` from a `UTC` session and from an `America/New_York` one alike.
    `timestamptz` is the opposite, and that is why the exception is exactly
    this narrow: it holds an *instant*, so writing it without a zone means
    choosing one — measured, the same value into `timestamp` is `12:00:00`
    under UTC and `07:00:00` under New York. What decides it is what the type
    holds, not which way the offset went. Refusing the projection would have
    refused a valid plan, and the loss it does carry is what `Narrowing` is
    for.

    `types::change_risk` already knows the shape and answers `Narrowing`, with a
    comment naming exactly this. That is not enough: `Narrowing` is a risk class
    a human clears at the gate, and what the human cleared was the *loss*. The
    zone was never in the plan to approve.

    The framing (267) pins `TimeZone` to UTC, which makes the result
    reproducible — and reproducible is not declared. Under the pin the change
    would silently reinterpret every stored value as UTC, which is a data
    transformation nobody wrote down and nobody reviewed: the same ground
    ADR-0012 §5 refuses a `USING` clause on. So the emitter asks a separate
    question, `types::depends_on_the_session_time_zone`, and refuses on it by
    name with the two-step remedy — add the column, fill it in a declared step
    with the zone written out, drop the old one.

<a id="decision-283"></a>

283. **`timestamp` is classified as opaque, not as a binary type with the width
    `sys.types` reports.** SQL Server's `timestamp` (`rowversion`) sat in the
    `varbinary` arm of the type-family classifier, so every change it took part
    in was answered by comparing capacities. Measured on the pinned image, the
    engine refuses `ALTER COLUMN` on either end of it, whatever those capacities
    are: `Msg 4928` leaving the type, `Msg 4927` arriving at it — including
    `ALTER COLUMN v timestamp` on a column that is already one, which fails at
    compile time and takes the whole batch with it.

    Giving the alias its real width is the repair that suggests itself and it is
    the wrong one. The classifier was deriving the width from absent arguments
    and getting 1; correcting that to the 8 `sys.types` reports turns
    `timestamp -> varbinary(8)` from an accidental `Safe` into a deliberate one.
    The capacity is not what governs. Opaque is, because every pair an opaque
    type takes part in falls to the `Incompatible` arm, and `Incompatible` is
    already this codebase's answer for a conversion the engine can refuse.

    The identity is untouched. `change_risk` returns `Safe` for `from == to`
    before any family is asked for, and `rowversion` normalizes to `timestamp`,
    so a column that keeps its type still produces no change — which matters
    more than it looks: a phantom change on every table carrying a `rowversion`
    would be proposed for ever and could never be applied.

    **And the pre-flight probe for such a change is not built.** A probe answers
    a question about the rows, and there is no such question here — the
    prohibition is on the column. Worse, the probe answers confidently: measured,
    `CONVERT(timestamp, 0xAB)` succeeds and returns `0xAB00000000000000`, so
    `TRY_CONVERT` finds nothing to report and the pre-flight line reads as a
    pass under a statement that will not compile. A probe that cannot see a
    prohibition must not stand beside the classification that can (#141).

<a id="decision-298"></a>

298. **A `timestamp` / `rowversion` declaration may be nullable, but its
    nullability may never be altered.** Measured on SQL Server 2025,
    `CREATE TABLE ... rowversion NULL` succeeds and the catalog records the
    column as nullable. The same engine refuses every `ALTER COLUMN` that names
    either spelling with 4927, even when the type is unchanged and only
    nullability is restated.

    The refusal therefore belongs to the nullability-change arm of the dialect
    emitter, using the same `alter_column_is_refused` fact as type-change
    planning. Refusing the declaration in offline `validate` would reject a
    state the engine does represent; classifying the intrinsic nullability risk
    differently would merely ask for approval before emitting SQL that cannot
    run. Ordinary `varbinary(8)` remains alterable, so binary capacity is not
    used as a proxy for the engine's `timestamp` rule.

<a id="decision-299"></a>

299. **A narrowing into a non-Unicode character type is probed by exact round
    trip, in addition to any character count.** `LEN` answers whether the source has
    more characters than a bounded target. It cannot see a legacy code page replacing
    `王小明` with `???`, or a UTF-8 `varchar(4)` exceeding its byte capacity.
    Measured, the legacy ALTER succeeds and silently stores the changed value,
    while the UTF-8 ALTER refuses with truncation; `LEN` reports 3 in each case.

    The second probe converts through the non-Unicode target and back to
    `nvarchar(max)`, then compares under `Latin1_General_BIN2`. The binary
    collation is part of the question: the source column's own collation may
    call two spellings equal, which would turn another loss into a clean count.
    The inner value is collated to `DATABASE_DEFAULT` before conversion because
    that is the target collation the emitter's `ALTER COLUMN` establishes when
    it writes no `COLLATE`; measured, a source column with an explicit UTF-8
    collation moves to the database default after that ALTER. This does not
    solve the separate declaration/read-back gap for explicit collations
    (#94); it makes the preflight match the statement this emitter writes.

    The existing length probe stays beside it. It is the direct, readable count
    for ordinary length loss, while the round trip is added only when the
    source is any supported character type and the target is `char`, `varchar`
    or legacy `text`. This includes non-Unicode sources because an explicitly
    UTF-8 or otherwise non-default source collation moves to `DATABASE_DEFAULT`
    under the emitted ALTER and can lose characters. A max or `text` target
    gets only the round trip because it has no length question, but it can
    still replace characters under a legacy code page.
    Both endpoints are normalized before classification, so accepted aliases
    such as `character varying(max)` ask the same probe as `varchar(max)`,
    matching the type spelling the emitter will use.
    This code-page question is independent of the ordinary narrowing risk:
    even widening `varchar(20)` to `varchar(max)` can replace an explicitly
    UTF-8 value when the ALTER resets the column to a legacy database default.
    Such a widening gets the round-trip probe without a meaningless length
    probe.
    A legacy `text` source is first converted to `varchar(max)` under its
    source collation before applying `DATABASE_DEFAULT`; SQL Server refuses a
    direct cross-code-page `COLLATE` on `text` itself.
    Unicode-to-Unicode and non-Unicode-to-Unicode changes keep their one length
    probe rather than paying for a question they do not ask.
    Because SQL Server does not accept `LEN(ntext)` or `LEN(text)`, those legacy
    sources are first converted to their corresponding max type for the length
    count; the round-trip comparison likewise converts `ntext` before using
    the comparison operator.

<a id="decision-387"></a>

387. **A cast is not the assignment the `ALTER` performs, so the conversion
    probe measures the value.** The obvious probe for a narrowing type change
    is "count the rows a cast rejects", and on this engine it reports a table
    clean that the statement then refuses. **Measured** on 18.6:
    `SELECT 'abcde'::varchar(4)` is `'abcd'` and
    `ALTER TABLE t ALTER COLUMN v TYPE varchar(4)` over the same value is
    `value too long for type character varying(4)` — the cast is an *explicit*
    conversion, which truncates, and the `ALTER` is an *assignment*, which
    does not. So `types::cannot_become` returns a predicate over the value and
    never a cast: for a bounded string target,
    `length(rtrim(v, ' ')) > n`. **Trailing spaces only**, because that is the
    engine's own exception — `'abc  '` into `varchar(3)` is `'abc'` and
    `E'abc\t'` into the same is refused — and `length` rather than
    `octet_length`, because the bound is characters: `'王小明'` is three of
    them and nine bytes and fits `varchar(3)`.

<a id="decision-388"></a>

388. **A `NaN` and an infinity sort greatest here rather than outside the
    order, so a range test finds them and a target that accepts one has to
    take it back out.** **Measured**: `'NaN'::numeric > 1e131071` is true, and
    `'NaN'::float8 = 'NaN'::float8` is true where C says neither. So the
    integer-target predicate is a plain range test and catches all three
    failures the engine names (`integer out of range`, `cannot convert NaN to
    bigint`, `cannot convert infinity to bigint`). But `numeric(10,2)` **keeps**
    a `NaN` and refuses an infinity — measured, `'NaN'::float8` into it is
    `NaN` and `'Infinity'::numeric` into it is `numeric field overflow` — so
    the bounded-`numeric` predicate excludes `NaN` by name, and the binary
    float predicate excludes `NaN` and both infinities, which pass through
    unchanged. Left in, each would have counted a row the engine keeps and
    refused a change it makes.

<a id="decision-389"></a>

389. **The engine tests the value it would store, so the probe rounds first —
    the way that target rounds.** **Measured**: `2147483647.4::numeric` into
    `integer` is accepted and `2147483647.6` is `integer out of range`; a
    `numeric(10,2)` holding `999999.995` is already `1000000.00` and fails
    into `numeric(10,4)`, whose message names the test — "must round to an
    absolute value less than 10^6". And the two families round differently:
    measured, `round()` on a `numeric` is half-away-from-zero while a float
    into an integer is half-to-even (`0.5`, `1.5` and `2.5` become `0`, `2`
    and `2`). So the float boundary is written out asymmetrically rather than
    derived — `>= 2147483647.5` fails and `< -2147483648.5` fails, because
    measured, `-2147483648.5::float8` into `integer` is `-2147483648`, which
    fits. Reading it as a symmetric bound counted a row the engine keeps.

<a id="decision-390"></a>

390. **A float target overflows at the midpoint above its largest value, and
    the threshold is written as the engine's own arithmetic.** **Measured** by
    bisection: the largest `double precision` that becomes a `real` is
    `3.4028235677973362e38` and the smallest that overflows is exactly
    `2^128 - 2^103`; the same construction one exponent range up is the
    `double precision` bound, and the value just below `2^1024 - 2^970`
    converts while the value at it does not. Written as
    `2::numeric^128 - 2::numeric^103` rather than as a decimal literal, which
    for the second is three hundred digits long. The largest finite value is
    the wrong bound: `3.4028235e38` is above it and converts, so a probe using
    it refuses a change the engine makes.

<a id="decision-391"></a>

391. **`NULLS DISTINCT` is this engine's rule and `GROUP BY`'s is the
    opposite, so the duplicate count excludes a key holding any NULL.**
    **Measured** on 18.6: two rows holding NULL are accepted under
    `UNIQUE (a)`, and two rows holding `(1, NULL)` are accepted under
    `UNIQUE (a, b)`; `GROUP BY` over those very rows reports two duplicates.
    The SQL Server probe one crate away groups without excluding them and is
    right to — there a `UNIQUE` treats two NULLs as one value — and ported
    across it counted a collision the engine would never produce and refused a
    plan the engine accepts, which is the worse of the two directions
    (PITFALLS #5).

<a id="decision-392"></a>

392. **A probe over a key spanning a column this plan narrows is not built.**
    Projecting a stored value through the new type means a `CAST` that can
    raise, and a probe that raises is reported as *unchecked* while the apply
    proceeds — the silence, arriving through the fix for something else. A
    widening cannot raise, by `TypeChangeRisk::Safe`'s own criterion, so it is
    written out and a narrowing is not. The row that would raise cannot
    survive the `ALTER COLUMN … TYPE` either, and that change's own conversion
    probe (387) is what counts it and names the column.

<a id="decision-393"></a>

393. **The orphan count compares under the referenced column's collation,
    spliced in from the catalog at the comparison site.** **Measured**, two
    stored columns collated differently cannot be compared at all —
    `q.k0 = r.k0` between a `"C"` column and an `"en_US"` one is `could not
    determine which collation to use for string hashing` — so the probe never
    answered on the plan shape it exists for. The clause has to be *at the
    comparison*: measured, a `COLLATE` inside the derived table's select list
    does not survive into the join and the same error comes back. Only the
    catalog can spell it, so the body carries the mark of DECISIONS 353 and
    the count is assembled by the engine, which is the machinery the
    pre-delete probe already had.

<a id="decision-399"></a>

399. **The estimate is a separate axis and carries no risk class.** ADR-0012 §3
    decides it and this is where it is built: `integer -> bigint` is `Safe` and
    rewrites a million-row table under a lock that blocks readers, and folding
    that into `Narrowing` would lie about what the class means and break the
    class's data-independent criterion. Nothing in `estimate.rs` reads or
    produces a `TypeChangeRisk` or a `RiskClass`, and a test asserts it over
    the module's own source, because the pressure to connect the two axes is
    highest exactly when somebody is looking at a large table.

<a id="decision-400"></a>

400. **A rewrite is avoided only where the target constrains no byte already
    stored.** **Measured**: every ordered pair of the catalogue's spellings,
    263 of them accepted by the engine, with `pg_class.relfilenode` either side
    of the statement. Ten pairs of distinct types rewrite nothing —
    `character varying(5)` into a wider one, into an unbounded one and into
    `text`; `text` into an unbounded `character varying`; `numeric(10,2)` into
    `numeric` and into `numeric(12,2)`; and `timestamp` against `timestamptz`
    both ways, which the session decides. Everything else rebuilds, including
    `integer -> bigint`, `real -> double precision`, `character(5) ->
    character(10)` — the padding is in every row — and `numeric(10,2) ->
    numeric(10,4)`, where widening the *precision* is free and widening the
    *scale* is not. Nothing about the declaration's shape suggests that
    asymmetry, which is the argument for measuring the whole table rather than
    reasoning about it. The live suite re-measures the matrix and holds the
    dialect to every accepted pair.

<a id="decision-401"></a>

401. **Whether the table is rebuilt and whether every row is read are two
    facts, because one is invisible to the other.** ADR-0012's Limits state it
    and this is the measurement: on a hundred thousand rows,
    `ALTER COLUMN v SET NOT NULL` rebuilds nothing and reads **all** of them,
    while `ALTER COLUMN w TYPE varchar(20)` from `varchar(10)` rebuilds nothing
    and reads **none**. Carried as one fact those two changes are the same
    change, and one of them is free. `ADD CHECK`, `ADD UNIQUE` and
    `SET PRIMARY KEY` each rebuild nothing and read every row; `ADD COLUMN`,
    `DROP COLUMN`, `SET DEFAULT` and both renames read nothing.

<a id="decision-402"></a>

402. **A foreign key locks the table nobody named.** **Measured** from inside
    the statement's own transaction: `ADD CONSTRAINT … FOREIGN KEY` takes
    `ShareRowExclusiveLock` on the **referenced** table as well as on the one
    the constraint is written on, and takes no `AccessExclusiveLock` at all —
    so it blocks writes to a parent that may be enormous and blocks no reader
    anywhere. Every other `ALTER TABLE` subcommand measured here takes
    `AccessExclusiveLock`, a non-concurrent `CREATE INDEX` takes `ShareLock`,
    and a concurrent one takes `ShareUpdateExclusiveLock` — measured from a
    second session, because it cannot run in a transaction. `Estimate` carries
    the referenced table in `also_locks` for that reason: it is a cost on an
    object the change does not mention.

<a id="decision-403"></a>

403. **An unparsed default expression is `unknown`, never free.** **Measured**,
    `ADD COLUMN d integer DEFAULT 7` rebuilds nothing and reads nothing, and
    `ADD COLUMN d uuid DEFAULT gen_random_uuid()` rebuilds every row. Both are
    one `AddColumn` carrying a default and only the expression tells them
    apart, which this tool does not parse (SPEC §8.2). ADR-0012 §4 rules out
    the guess by name, and the constant test the row reader already owns
    (`rows::is_constant`) is what decides which side a default falls on.

<a id="decision-404"></a>

404. **A shape the measurements never covered takes the answer back to
    `unknown`, whatever the static half said.** ADR-0012's Limits name three —
    a partitioned table, an inheritance parent, and `ALTER TYPE` on an indexed
    column — and `estimate::against` reads `relkind`, `relhassubclass` and
    `pg_index` and returns each with its reason. An estimate measured on
    ordinary tables and quoted about a partitioned one is worse than no
    estimate, because the number carries the authority of a measurement it did
    not come from. A table the database does not have is `unknown` too, and
    not a table with no rows in it.

<a id="decision-405"></a>

405. **`reltuples = -1` is "nobody has looked", not "no rows".** **Measured**,
    a table holding a thousand rows that has never been analyzed reads
    `reltuples = -1` and `relpages = 0`, and reads `1000` and `5` straight
    after `ANALYZE`. Read as a count it says the change is free on the largest
    table in the database, so `Rows` has an arm of its own for it and no
    caller can spell it as zero.

<a id="decision-406"></a>

406. **A probe may only measure a rendering that every session renders alike.**
    A probe is issued *before* the deployment's transaction framing is
    established, so it runs under the operator's own settings while the
    statement it clears runs under the ones the framing pins (DECISIONS 267).
    For a length taken over `::text` that is not a detail: **measured on
    18.6**, `'\x0102'::bytea` prints 6 characters under `bytea_output = hex`
    and 8 under `escape`; `'1 day 02:00:00'::interval` prints 14 under
    `IntervalStyle = postgres` and 9 under `sql_standard`; a `timestamp` prints
    19 under `DateStyle = ISO` and 24 under `Postgres`; the same value as a
    `timestamptz` prints 22 under `TimeZone = UTC` and 25 under `Asia/Kolkata`;
    and `1.0/3.0::float8` prints 18, 17 and 12 under `extra_float_digits` of
    1, 0 and -5. The pinned rendering is not consistently the longer or the
    shorter of a pair, and that is the point: a `bytea` or a `timestamp`
    renders *longer* unpinned, so a probe measuring the operator's session
    counts rows this engine accepts and refuses a valid plan, while an
    `interval` or a `float8` renders *shorter*, so the same probe counts
    nothing and clears a statement the engine then refuses — a probe passing
    for the wrong reason, which is the failure probes exist to prevent. There
    is no direction to correct for, so the bounded-string conversion probe is
    gated on an **allow-list** of source families whose text no pinned setting
    moves — measured unmoved with all of those settings changed at once: `json`,
    `jsonb`, `uuid`, `boolean`, `numeric`, the integers and the string types
    themselves. A `date` is excluded with the rest of its family even though
    every `DateStyle` prints ten characters for one: that is a coincidence of
    the styles this engine happens to have, not a promise. The ordering itself
    — that probes run outside the framing — is a wider question than this
    catalogue, and is filed as its own issue rather than answered here; if it
    is ever reversed, this gate is what may be lifted.

<a id="decision-409"></a>

409. **An estimate names its table twice: as the plan has it, and as the
    catalog does.** The same translation as DECISIONS 407, one module over. A
    plan may rename a table and then alter one of its columns, and the
    `AlterColumnType` and `RenameColumn` both carry the *declared*,
    post-rename table. `estimate::against` reads `relkind`, `relhassubclass`
    and `reltuples` from the catalog before any statement has run, so asked
    about that name it found no row and reported "this database has no table by
    that name to measure" — a rename read as an absence, on a table sitting
    there with its rows in it. `Estimate` therefore carries `table`, which the
    operator reads and which is the name the table will have when the statement
    runs, and a private `stored`, which is the only name `against` may query.
    Private is the design and not an accident: `Estimate` has no public
    constructor, and the single-change `estimate` is not public either, so the
    only way to hold one is `estimates(&ChangeSet, Strategy)` — the only
    function that can see the rest of the plan. A caller mapping the
    single-change form over a change set would rebuild exactly the estimate
    that cannot be measured, and now cannot write it.

<a id="decision-410"></a>

410. **A retype is the one plan change that leaves a probe able to run and
    wrong, so a probe over a retyped table is skipped rather than allowed to
    answer.** `AlterColumnType` runs at rank 9 and `AddCheck` at rank 13, so
    the engine tests a check against the *converted* value while a probe built
    before any statement tests the stored one. **Measured on 18.6**: a
    `numeric(10,2)` holding `1.50` and `2.25`, converted to `numeric(10,0)` and
    then given `CHECK (v = round(v))`, stores `2` and `2` and the engine accepts
    the constraint — while `WHERE NOT (v = round(v))` over the stored values
    counts both rows and refuses the plan. A skip and not a projection: the
    predicate is arbitrary SQL naming its own columns, and supplying converted
    values would mean rewriting that text by substitution, which this module
    refuses on principle (DECISIONS 259's neighbourhood). A skip and not a
    failing probe either, which is how the same probe treats a renamed or an
    added column: those make the probe *fail to run* and the runner reports
    them by name, which is the more visible silence. A retype produces no error
    at all, which is the one outcome no report can catch. The partial-index
    probe already asked this question; the check probe did not, and one spelling
    (`AsStored::retypes_in`) now serves both so they cannot drift apart again.

    The rule is **per table, not per column**, and it is the coarser of two
    answers on purpose. Narrowing it to "does this check's expression name the
    retyped column" needs a name scan over arbitrary SQL, and that scan's
    failures run the wrong way: a miss — a name spelled in another case, or
    quoted with its quotes doubled, both of which the existing scan in
    `crate::impact` gets wrong today — keeps a probe that refuses a valid plan,
    while the coarse rule's failure only loses a probe the engine still
    enforces. AGENTS.md's finding rules make the first mandatory to fix and the
    second not, so the coarse rule is the safer error, and it is what the
    partial-index probe beside it already does. The cost is real and is paid
    knowingly: a plan that retypes any column of a table gets no check probe on
    that table, even for a check over a column it does not touch.

<a id="decision-411"></a>

411. **A binary float is measured in its own domain, never through `numeric`.**
    `float8::numeric` on this engine goes by way of the float's shortest
    round-tripping decimal rather than its exact value, so it is a *rounding*,
    and the rounding is largest exactly where a conversion probe's boundary
    tests sit. **Measured on 18.6**: `(-9223372036854775808::float8)::numeric`
    is `-9223372036854780000`, four thousand million past the value, so the
    probe for `double precision -> bigint` counted `-2^63` — a value the engine
    stores exactly — as out of range and refused a valid plan. The same root
    cause reached the `real` target: `3.4028235677973362e38`, the largest
    `double precision` that converts, reads as
    `340282356779734000000000000000000000000` in `numeric` and clears the
    threshold `340282356779733661637539395458142568448`, while in the float
    domain it does not and the engine takes it. Both tests now compare as
    `float8`. Every threshold either needs is exactly representable there: the
    integer bounds by construction, and `2^128 - 2^103` because it asks for 25
    of the 53 mantissa bits. Only the `real` target is reachable from a float
    source — `real -> double precision` is a widening `change_risk` has already
    called `Safe` — so `2^1024` never has to be written as a `float8`.

    The `numeric` route stays for an **exact** source, and not by omission: an
    `ALTER` from `numeric` or an integer type to a float converts the exact
    value, so the exact domain is the one the engine is working in. The rule is
    not "prefer floats", it is "measure in the domain the statement measures
    in" — the same rule DECISIONS 387 draws between a cast and an assignment,
    one level down.

<a id="decision-412"></a>

412. **The calendar probe takes `infinity` out by name, because the target
    keeps it.** Every other range test in `cannot_become` already excludes the
    sentinels its target accepts — 411's float arms, and the bounded-`numeric`
    arm which counts an infinity and spares a `NaN`, each measured. The
    temporal arm was written as a bare `value > 'last-12-31'::date` and did
    not, so the one value that sorts after every finite date was counted as a
    row the conversion cannot carry. **Measured on 18.6**: a `date` column
    holding `infinity` becomes a `timestamp` holding `infinity`, and the
    `ALTER` does not raise — while the probe counted 1 and refused the plan.
    `'294277-01-01'` is still counted, and still `22008` at the engine, so the
    bound itself is intact.

    One spelling covers the whole family rather than one per source type:
    **measured**, `'infinity'::date` compares *equal* to `'infinity'::timestamp`
    and to `'infinity'::timestamptz`, so the literal need not be written three
    times. `-infinity` is left alone deliberately — it cannot satisfy a `>`
    against a finite bound, and there is no lower test for it to escape,
    `Family::Temporal` carrying no `first_year`. This is the third instance of
    a shape this file already names twice; the sweep that found it is the rule
    in AGENTS.md, not a lucky read.

<a id="decision-413"></a>

413. **A created table's columns are remembered from the plan, because its key
    is the one column no row change types.** `InsertRow` carries "the type of
    every non-key column the table has" and no more, by its own documentation
    — and the row key is exactly what a foreign key points at. For a table
    that already exists the catalog answers; for one this plan creates there is
    no catalog row, so both sides of such a key reached the probe as unknown
    literals, which the engine resolves to `text` in a select list.

    **Measured on 18.6**, through the differ rather than a hand-built plan: a
    parent declaring its `numeric(5,1)` key `1.0` and a child declaring its
    `numeric(5,2)` one `1.00` — each of them the engine's own rendering, which
    is what the spelling check requires a declaration to use — produced
    `NOT EXISTS (... WHERE q.k0 = r.k0)` over `E'1.0'` and `E'1.00'`, counted
    **one orphan**, and the engine then took the very same plan. The count and
    the verdict disagreed, and the count was the one that was wrong.

    The types are in the plan already: `Change::CreateTable` carries the whole
    `Table`. `crates/pbps-mssql` has remembered them at that arm all along, so
    this is the pg crate catching up rather than a new idea, and it is the
    reasoning of DECISIONS 339 and 341 — two unknown literals compare as text,
    and `'2026-01-02'` and `'01/02/2026'` are one `date` — arriving at the one
    column those entries could not reach.

<a id="decision-414"></a>

414. **The missing-value count is kept only for a value this crate can read
    without running anything.** `has_required_add_value_source` is lexical and
    deliberately so: `crates/pbps-model/src/schema.rs` looks for the words
    `null`, `nullif`, `try_cast`, `try_convert` and `try_parse` in the default
    expression, and answers "this column may arrive without a value" if it
    finds one. That is the right *conservative* answer to give a risk class.
    It is the wrong answer to hand a probe, which does not classify but counts
    — and a count aborts the apply.

    **Measured on 18.6**, one word and both answers:

    ```text
    ADD COLUMN c integer NOT NULL DEFAULT NULLIF(1, 2)   every row reads 1
    ADD COLUMN c integer NOT NULL DEFAULT NULLIF(1, 1)   23502, contains null values
    ```

    So the count now stands only where there is no default at all, or a
    default that is the literal `NULL` — the two cases `constant_default` can
    read. An expression is the case DECISIONS 124 already answers with no
    probe rather than a guess, and the trade is stated rather than hidden: a
    `NULLIF(1, 1)` reaches the engine and is refused there, inside the plan's
    own transaction, instead of being refused at the gate.

    The alternative was rejected on purpose. Asking the engine
    `SELECT CASE WHEN (expr) IS NULL THEN (SELECT count(*) ...) ELSE 0 END`
    would answer exactly, and would also **run the operator's own expression
    before the plan is approved** — the hazard #274 records, and something no
    probe in this crate does; `backfill_of` splices a default only through
    `constant_default` for the same reason. SQL Server has the same defect at
    the same predicate, measured on the pinned image, and it is filed as #283
    rather than carried here.

<a id="decision-415"></a>

415. **A probe pins its own session, and the allow-list of 406 widens to every
    type the catalogue holds.** The pins were established by
    `transaction_framing().begin` on one path and by a `session_pins` call on
    the other, and **both ran after `deploy::preflight` returned**. So every
    probe was answered under whatever settings the operator's session carried,
    while the statement it cleared ran under the nine pinned ones. Three
    mechanisms, each measured on 18.6 and each of them refusing a plan this
    engine takes:

    - **A value rendered to text.** `'\x0102'::bytea` is 6 characters under the
      pinned `hex` and 8 under `escape`; `'1 day 02:00:00'::interval` is 14
      under the pinned `postgres` and 9 under `sql_standard`. Not consistently
      one direction, so a length probe goes wrong both ways.
    - **A literal the plan carries.** `CAST('01/02/2026' AS date)` is 2 January
      under the pinned `MDY` and 1 February under `DMY`.
    - **The operator's own declared expression.** One stored row `2026-01-15`
      and a declared `CHECK (d < '02/01/2026')`: the probe counts 1 under
      `DMY` and 0 under the pinned `MDY`, and the engine accepts the
      constraint.

    The fix is the ordering, not a guard. `preflight` pins as its first act, so
    the edition read, the role checks and the rename impact scans are pinned
    too; and `run_probes` — the probe loop, extracted — **pins again rather
    than trusting its caller**, because it is the function whose answers depend
    on it and a second `SET` batch costs nothing. The staged path keeps its own
    call for the case that skips `preflight` entirely: a `--resume`.

    A per-probe `SET` prefix stays refused, and DECISIONS 260 is why: measured,
    `DateStyle` is read at parse analysis and *does* take effect for the next
    statement of the same batch, while `standard_conforming_strings` is read by
    the lexer and does not. A prefix that fixes five settings and misses the
    one deciding where a string ends is worse than no prefix, and `Probe` is
    one statement by contract.

    **406's allow-list then had no reason left.** It becomes
    `renders_alike_under_the_pins`, which admits every family the catalogue
    holds — measured under the pins, the probe and the engine agree on the same
    character for all nine sources that had lost it:

    ```text
    bytea 6   interval 14   date 10   time 8   timetz 11
    timestamp 19   timestamptz 22   real 3   float8 18
    ```

    each `varchar(L)` accepted and each `varchar(L - 1)` refused. It stays a
    check rather than being deleted because one reason survives and is not the
    ordering: `lc_monetary` is **not** among the nine, and measured with all
    nine pinned and only the locale changed, `1234.56::money` is `$1,234.56`
    under `en_US.utf8` and `1.234,56 €` under `de_DE.utf8` — nine characters
    and ten. `money` cannot reach a probe today because ADR-0012 §1 keeps the
    catalogue closed; the test is driven from `CATALOGUE` itself so that
    admitting it later fails loudly rather than measuring it wrong in silence.

    What the ordering does **not** fix: a probe still evaluates the declared
    expression, so a volatile function in a `CHECK` advances a sequence before
    the plan's first statement. That is #274, and pinning a session does
    nothing about a side effect.

<a id="decision-430"></a>

430. **Connected cost is advisory output beside the plan, never part of its risk.**
    `plan --db` asks the engine facade for operational cost while its connection
    is open, then renders the same answer in a separate human section or JSON
    `data.cost`. PostgreSQL supplies the measured rewrite, scan and lock facts
    and its approximate catalog row count (ADR-0012 §3). Each estimate retains
    its original change index and uses that change's strategy, so an unmeasured
    change cannot shift the column queried by the connected estimator and an
    ordinary index cannot inherit another index's concurrent-build lock.

    Every planned change has either an estimate or a named unavailable answer.
    SQL Server reports the unmeasured capability by name (#255). A failed
    catalog query makes the affected estimate unavailable; it neither guesses
    from the static half nor refuses a valid plan. Unknown rewrite/scan reasons,
    never-analyzed tables, other locked tables and absent row-count reasons
    survive into both formats. The existing global-strategy estimator API keeps
    its contract; the command uses the per-change API. Cost never enters
    SavedPlan, its checksum, risk classification, or an apply approval (#305).

<a id="decision-433"></a>

433. **The required-add-value scan's identifier boundary is the caller's, not
    always SQL Server's.** `Column::has_required_add_value_source` split a
    default's text on `is_regular_identifier_continue` — SQL Server's rule —
    to look for a bare `NULL`-producing word, and PostgreSQL's preflight
    called it unchanged. Measured on 18.6, a combining mark (`\u{301}`)
    continues a PostgreSQL identifier and is not alphanumeric, so
    `null\u{301}x()` — a plain call to one name the engine accepts unquoted —
    split at the mark under the shared rule into the bare word `null`, and a
    column with a perfectly good default was read as having none.

    `has_required_add_value_source_with` takes the identifier boundary as a
    parameter, the same split ADR-0011 Amendment 2 made for
    `normalize_definition` and DECISIONS 315 made for the dependency and
    reference scans. The no-argument form keeps SQL Server's rule as its
    default: right for `pbps-mssql`'s own preflight, because that caller is
    SQL Server. The model's dialect-free `Change::intrinsic_risks` also keeps
    the no-argument form, but not because SQL Server's rule is right for it —
    it classifies risk before any dialect is chosen, across many risk classes
    besides this one, and still reads every dialect's defaults by SQL
    Server's boundary. That is a known gap, not a decision that it is
    correct; threading a boundary through it is a larger, differently shaped
    change than this one, and it is tracked separately as issue #343.
    `pbps-pg`'s preflight is the caller this decision closes: it now asks
    with `Lexicon::identifier_continues`, PostgreSQL's own byte rule.

    The model's dependency and reference scans (`creation_order_with`,
    `references_with`) already took this shape. Of the two production
    callers of `is_regular_identifier_continue` this issue named, `pbps-pg`'s
    preflight is now closed; `Change::intrinsic_risks` is not, and is #343's
    to close (issue #128).

    **The `pbps-pg` wiring is unpinnable today.** The row-count probe this
    arm builds is also gated by `constant_default`/`rows::is_constant`, which
    recognizes only a closed set of literal shapes — `NULL`, a boolean, a
    quoted string (its content already blanked before the keyword scan
    runs), a typed literal, a signed number — and refuses every bare
    identifier or function call, `null\u{301}x()` included. Every shape
    `is_constant` accepts either carries no unquoted letters or has its
    lettered content quoted and blanked before the keyword scan sees it, so
    the two boundary rules can never disagree on a default that reaches the
    inner check: `NULL` itself is unambiguous under both. The outer guard's
    answer therefore never moves this arm's output, for any default.
    Reverting only `crates/pbps-pg/src/preflight.rs`'s guard to the
    no-argument form, with the model's fix left in place, left every
    existing test green — measured, not assumed. The wiring stands as
    defence-in-depth against that inner gate ever being relaxed, not
    something a test can fail today.

    Pinned by
    `a_combining_mark_is_read_as_a_name_byte_under_postgresqls_boundary_and_a_gap_under_the_shared_one`
    in the model, which holds SQL Server's own answer unchanged, and by
    `the_required_add_guard_reads_a_combining_mark_by_this_engines_own_boundary`
    in `pbps-pg`, which pins the guard's own answer over the boundary this
    crate actually wires in (`crate::LEXICON.identifier_continues`) — not
    `build()`'s output, which the paragraph above explains cannot move.

<a id="decision-438"></a>

438. **A `numeric` with a negative scale is judged by its granularity, not its
    magnitude alone, when the target is a binary float.** 244 already carries
    the shape "`Safe` is decided by what a type *holds*, not by how many
    digits it has" for the integer-and-`NaN` cases; `exact_in_float`
    (`pbps-pg`'s `types.rs`) had one more instance of the same mistake, found
    by review (#138).

    `numeric(p, s)` with `s <= 0` holds `m * 10^|s|` for `|m| < 10^p`, and the
    classification asked only whether the *magnitude* `10^(p - s) - 1` fit
    under the float's mantissa gap — treating a negative scale as if it meant
    "round to a whole number and then judge the size." It does not:
    `10^|s| = 2^|s| * 5^|s|`, and the `2^|s|` half is free — a binary float's
    exponent carries any power of two at no mantissa cost. So the question is
    whether `m * 5^|s|` fits, not `m * 10^|s|`. Measured on 18.6,
    `numeric(1,-7) -> real` was called `Narrowing` (its largest value,
    `90000000`, is above `real`'s 2^24) although every one of its ten values
    is exactly representable:
    `SELECT bool_and((k*10000000)::numeric(1,-7)::real::double precision =
    (k*10000000)::double precision) FROM generate_series(-9,9) k` is `t`.

    No separate check against the float's own exponent range is needed
    alongside the mantissa one. `max_exact_int` is at most 2^53, and the
    `pow10`/`pow5` arithmetic is checked against `i128` (~1.7e38) rather than
    against where the engine actually overflows (~3.4e38 for `real`, far
    larger for `double precision`), so an `i128` overflow already refuses
    everything anywhere near where overflow could matter. And short of that,
    the mantissa test is strictly the tighter one: by exhaustive search over
    `p` and `|s|`, the largest total magnitude that can still pass it at all
    is nine orders of magnitude below either float's overflow point.

    Pinned by a unit test on both directions and both float widths —
    `numeric(1,-7)`/`numeric(1,-8)` Safe into `real`, `numeric(1,-9)`/
    `numeric(9,-7)` Narrowing, and the matching boundary pair for
    `double precision`'s 2^53 at `numeric(1,-21)`/`numeric(1,-22)` — and by
    three rows in the live suite's
    `a_change_the_dialect_calls_safe_neither_fails_nor_alters_a_value`. That
    live comparison needed its own correction: a `real`'s own shortest
    round-trip printing only promises to re-parse to the same bits at
    `real`'s own precision (measured, a `real` actually holding `8999999488`
    still prints `9e+09`, since that is shorter and still parses back to the
    same `real`), so the test now widens a `real` result through `double
    precision` before reading it as text, and compares a binary float target
    as the number its text spells rather than as the characters — the same
    value may be spelled `90000000` coming out of `numeric` and `9e+07`
    coming out of a float, and that difference is not the finding.

    The two other open issues touching this catalogue at the time — #233
    (`_int4[]` folding to `integer[][]`) and #281 (a narrowing that rounds
    rather than raises drops the preflight probe) — are untouched here: #281
    in particular already owns the case where a `numeric` remains genuinely
    `Narrowing` into a float and the probe that should catch a rounding row
    does not; that is a preflight-contract gap, not a risk-classification one.

<a id="decision-446"></a>

446. **A column type's argument position is a word boundary in its base name.**
    Supersedes 242 for PostgreSQL's four time and timestamp forms (#130).
    `ColumnType` keeps its complete base name for dialect lookup and comparison,
    and a validated optional position puts its arguments between name words.
    The model knows no PostgreSQL suffixes: `timestamp(3) with time zone` is
    syntax, while accepted names, positions and precision bounds belong to
    the dialect. SQL Server continues to require arguments after the full name.
    A trailing identifier can therefore be syntactically representable without
    being a type either closed catalogue accepts.

    Serialization remains a string. Existing types retain their exact encoded
    shape and meaning, so schema, plan and state versions do not move; older
    readers reject the new in-name strings rather than discard their modifier.
    Arrays still need an independent dimension representation and remain
    refused: an argument position does not stand in for an array suffix.

    PostgreSQL admits precision 0 through 6 and preserves explicit 6, as
    `format_type` does. Reducing it rounds stored values and is narrowing;
    widening keeps them. Precision does not shorten a timestamp's calendar,
    so the date-range preflight predicate is restricted to an actual calendar
    narrowing. A last-calendar-day timestamp with reduced precision remains
    a valid change. Live tests pin all four canonical forms and their aliases,
    rendering back into executable SQL, bounds, and precision loss.

<a id="decision-449"></a>

449. **A key this plan adds over a column it narrows excludes the rows its
    `CAST` would raise on, one row at a time — it does not skip the whole
    key.** DECISIONS 392's rule for this shape — no probe is built for a
    narrowing key, because the row that would make the `CAST` raise cannot
    survive the `AlterColumnType` change's own conversion probe either, and
    that probe is what counts it — is true **per row**, not per column, and
    a gate built at the column's type pair rather than at the row is wrong
    in two different ways, found across two rounds of review on the same PR.

    **First wrong direction: too narrow.** Gating on `TypeChangeRisk::Safe`
    alone breaks two pre-existing live tests protecting DECISIONS 340 (a key
    spanning a retyped column compares the converted values) over a
    rounding-only narrowing whose `CAST` can never overflow. **Measured**:
    `CAST(999.99::numeric(5,2) AS numeric(5,1))` is `1000.0`, no error, and
    over the fixture
    `a_key_this_plan_adds_on_a_column_it_retypes_compares_the_converted_values`
    pins — both `child.amount` and `parent.amount` narrowed from
    `numeric(5,2)` to `numeric(5,1)` — the `AlterColumnType` conversion
    probe on each column counts **zero**. Nothing there catches the hazard
    that test exists for: two stored values, `1.04` and `1.00`, round to the
    same `1.0` and collide once the key is validated. A gate that skips this
    pair's key probe entirely would have deleted the only thing that sees
    it.

    **Second wrong direction, and the one this entry's design answers:
    too broad.** A first attempt narrowed the gate to a bounded, measured
    `numeric`-only predicate (`types::narrowing_cast_cannot_overflow`,
    since deleted) admitting a pair only where its `CAST` could *never*
    raise for any value, and skipping the pair's whole key otherwise —
    matching origin/master's own pre-#253 behaviour of always building the
    key probe, just made conditional on the type pair. Review caught the
    same conflation one level down: for `numeric(20,0) -> integer`, a child
    value of `2` and the referenced parent row `2` deleted by the same plan,
    the `AlterColumnType` conversion probe counts **zero** — `2` fits an
    `integer` fine — and the coarse skip built from the type pair took the
    key probe with it anyway, because *some* value of that pair (`5000000000`,
    say) can overflow. Before #253 touched anything, a table whose rows all
    fit had a working orphan check, because the unconditional `CAST`
    happened not to raise for any of them. A coarse, type-level skip took
    that check away from every such table, not only the one row that
    overflows — the same shape as the first wrong direction, one level down:
    a comparison made against the case that fails, when the case that
    succeeds is the one being taken away.

    **The fix asks the question `cannot_become` already answers, per row —
    and excludes, rather than blanks.** `AsStored::converted` still admits
    `TypeChangeRisk::Safe` outright and still builds the plain `CAST` for
    everything else — origin/master's own behaviour, unconditionally. A
    first version of this fix instead wrapped that `CAST` in `CASE WHEN
    <cannot_become predicate> THEN NULL ELSE CAST(...) END`, reasoning that
    a `NULL` there already means "matches nothing" everywhere this feeds a
    key comparison (DECISIONS 337, 348). **A second round of review found
    that reasoning incomplete: this query already gives `NULL` that one
    meaning, and a manufactured `NULL` is a second, different fact — "this
    value could not be tested" — riding the same wire.** On the parent
    side specifically, a manufactured `NULL` does not merely read as
    absent: it makes that *specific* parent row stop matching as a survivor
    for *every* child that might otherwise have matched it there, which
    over-counts the very probe DECISIONS 449 exists to keep honest — and
    over-reporting a pre-delete count is refusing a plan that may be valid,
    which this project's finding rules treat as the most serious of the
    three mandatory-fix cases. (It turns out to be moot here specifically —
    see below — but "moot because something else fails first" was rightly
    judged a reason to write the correct thing, not a reason to leave the
    wrong one in place.)

    So the design excludes instead: `AsStored::raise_guard(column, expr)`
    answers, separately from `converted`, the predicate over `expr` that
    names the rows a column's `CAST` would raise on, and each of
    `planned_key_probes`'s three row contexts — the child's own stored row
    (`ch`), the specific parent row a key's probe asks about by primary key
    (`p`), and every surviving parent row a comparison might match against
    (`q`) — gets its own `AND NOT (<raise_guard predicate over that row's
    own alias>)` term, added to that context's own `WHERE` clause rather
    than folded into any one comparison. A `NULL` keeps its single meaning
    throughout; the excluded row is not silently dropped from the plan's
    safety net, it is counted and named by the `AlterColumnType` conversion
    probe running beside this one, which is the premise DECISIONS 392
    actually needs, true per row instead of per column. No whole-key skip
    exists in this design at all — every key this plan adds over a narrowed
    column gets its own probe, unconditionally.

    **Why the parent-side exclusion's over-count risk is moot, in the one
    case that can trigger it.** `raise_guard` only ever fires for a column
    this same plan retypes (it reads `retyped_from`/`retyped` directly), so
    a parent row it excludes on the referenced column is a row belonging to
    a table whose `AlterColumnType` change carries its own conversion probe
    (387) — a second, independent probe, scanning that whole table
    unconditionally. That `ALTER` runs at rank 9, strictly before any
    `DELETE` (DECISIONS 340), so every row still present at that point —
    including one a later statement in the same plan is about to delete —
    has to survive it or the deploy is already refused, regardless of what
    this key's own orphan probe does or does not count. The excluded
    parent row is therefore always independently caught, on the same plan,
    by a probe that does not depend on `planned_key_probes` at all.

    **A later round found a second, distinct parent-side gap: the exclusion
    is a count-time filter, not an evaluation barrier, and this entry
    should not be read as claiming otherwise.** `excluded_p`/`excluded_q`
    are `AND NOT` terms in the *same* flat `WHERE`-clause conjunct list,
    inside the *same* correlated `EXISTS` subquery, as the
    `CAST(p.<column> AS ...)` / `CAST(q.<column> AS ...)` that `tuple()`
    emits for the comparison itself. PostgreSQL does not promise an
    evaluation order for the conjuncts of a `WHERE` clause: nothing here
    guarantees the guard runs before the cast beside it, so on the parent
    side an out-of-range value can still raise, leaving this key's own
    probe reported as *unchecked* rather than counted or excluded. **This
    repo has already been bitten by exactly this class**, and the fence for
    it is a precedent, not a surprise: DECISIONS 324 records the spelling
    queries needed an `OFFSET 0` fence because the planner folded a
    single-row `VALUES` list into a `Result` node and evaluated a cast at
    planning time, before the guard meant to protect it ran. Same shape —
    an `AND`/`CASE` beside a raising expression is not a barrier unless
    something forces the order — a different query and a different guard
    here.

    The bound is the same rank-9 argument just given, aimed at *this* risk
    instead of the over-count one: `raise_guard` only fires for a column
    the same plan retypes, so a parent row this ordering hazard can reach
    belongs to a table whose `AlterColumnType` change carries its own,
    unconditional conversion probe (387) — one that counts by evaluating
    `cannot_become`'s predicate directly, never attempting the `CAST`
    itself, so it cannot raise the way this key's own guarded comparison
    can. That `ALTER` runs at rank 9, strictly before any `DELETE`
    (DECISIONS 340), so a row that cannot survive the retype has already
    refused the whole plan before any delete this key's probe exists to
    protect could run. What a parent-side raise costs here is only this
    key's own report of a row a different, unconditional probe was always
    going to refuse the plan over — not an orphan escaping undetected, not
    a wrong recording — which is why it is deferred rather than fixed on
    this PR (issue #435), not answered by the rank-9 bound being reused
    from above: the bound is the same, the risk it is bounding is not.

    **The child-side guard is not exposed to this.** `excluded_ch` is not a
    conjunct inside the correlated `EXISTS`'s own `WHERE` list at all — it
    is ANDed against the *result* of the whole parenthesized
    `EXISTS`-based expression, one level outside it (`as_backfilled =
    format!("({}){excluded_ch}", references(&|_| None))`). The child's own
    `CAST`, reached only from inside that subquery, does not share a flat
    conjunct list with its guard the way `p`/`q` do, so this hazard has no
    child-side counterpart. This PR's own live coverage narrows only the
    child column — the shape that cannot hit this gap — so it exercises
    none of what this paragraph describes; a fixture that narrows the
    *parent*'s column is what issue #435 needs before an expression barrier
    there can be verified.

    **Where `cannot_become` returns `None` for a pair that is `Narrowing`
    (not `Safe`), this builds the plain, unguarded `CAST` — exactly
    `origin/master`'s own behaviour, left alone rather than papered over.**
    `None` there is *unexpressed*, not *safe*: **measured**,
    `CAST(3000000000::bigint AS integer)` raises `22003 integer out of
    range` on `w80b-pg`, yet `cannot_become` has no match arm for
    `Exact::Integer -> Exact::Integer` at all (only `Exact::Numeric` and
    `Approx` sources are covered), so it falls through to its catch-all
    `None`. Building the unguarded `CAST` here can raise, and the probe
    runner reports that by name — issue #253's own finding, applied to a
    pair this PR cannot close. This project's declared failure mode is
    doing the wrong thing *silently*; a probe that is present and loud is
    preferred over one silently narrowed to skip a pair with no evidence
    behind the skip, and a whole-key skip for this pair would have thrown
    away the working check for every table whose `bigint` values all fit
    `integer` — the same regression this entry exists to name. The
    `cannot_become` gap itself is filed separately (issue #429): it
    predates this PR, it also affects the `AlterColumnType` change's own
    conversion probe on this same pair, and closing it needs its own
    measurement matrix across every `Exact::Integer` narrowing, not only
    the one this issue happened to need.

    `types::narrowing_cast_cannot_overflow`, the strict/scale-aware,
    `numeric`-only type-level predicate an earlier round of this same PR
    built and measured, is retired: nothing in this design consults a
    type-level "can this pair ever raise" answer any more, only the
    row-level one `cannot_become` already gave. Its doc comment recorded
    two real, hard-won measurements worth keeping here instead: `CAST(
    999.99::numeric(5,2) AS numeric(4,1))` — equal `int_digits`, `3 -> 3` —
    raises `22003 numeric field overflow`, detail "A field with precision
    4, scale 1 must round to an absolute value less than 10^3", while the
    same cast on `12.34` succeeds as `12.3`; a type-level predicate that
    once admitted that pair by a non-strict comparison would have passed
    any fixture whose rows happened to be small. `cannot_become`'s own
    bounded-numeric-target arm asks the equivalent, per-row question
    directly — round the value to the target's scale and compare against
    the bound — so this shape is now answered without a second predicate to
    keep in sync with the first (issue #253).

    **The guarded probe says so.** Issue #253 asked that a probe skipped for
    this reason name the fact in its own description; that request went
    unmet through the whole-key-skip design, because a probe never built has
    no description to carry it (the reasoning behind #270, which this
    shape was compared against and then retracted from once the skip it
    named stopped existing). Under this design the probe is always built,
    and its count now means something narrower than it used to — "orphans
    among the rows whose conversion cannot fail" rather than "orphans" — so
    `planned_key_probes` appends a clause to the probe's own description
    wherever a guard actually excludes something, naming the
    `AlterColumnType` conversion probe as the one that counts what this one
    does not. A probe with nothing to exclude carries no such clause: the
    count still means exactly what it always did.

<a id="decision-462"></a>

462. **An identity increment must fit PostgreSQL's directional sequence span
     (issue #181).** The existing identity check already uses the sequence's
     own default bounds: `1..=type_max` when ascending, `type_min..=-1` when
     descending. It now refuses an increment whose magnitude exceeds the
     difference between those endpoints. Equality is allowed: measured on
     PostgreSQL 18.6, `smallint` starting at 1 by 32766 produces 1 and 32767,
     and starting at -1 by -32767 produces -1 and -32768. The corresponding
     integer and bigint boundaries work in both directions too.

     PostgreSQL accepts larger increments at `CREATE TABLE` and stores them
     unchanged; the second insert then fails with sequence exhaustion
     (`2200H`) for every valid seed. This is an explicit data-path validation
     rule requested by #181, not a claim that the engine rejects or silently
     rewrites the declaration (decision 266). The diagnostic says which step,
     type, bounds and usable span caused the refusal. It does not narrow the
     seed rule: a seed at an endpoint with an ordinary increment stays legal.

     Arithmetic widens to i128 before subtraction or absolute value, so the
     model's i64 minimum increment is refused without overflow. Unit cases
     cover all three integer types, both directions, aliases and endpoints.
     CLI tests pin refusal before connecting and bootstrap the six exact-span
     boundaries, insert two generated rows and verify convergence. Reverting
     the span check makes the new refusal regressions fail.

<a id="decision-476"></a>

476. **A planned PostgreSQL key gets a collation compatibility probe even when
     its row values cannot be projected.** A foreign key between different
     collation OIDs is rejected when either collation is nondeterministic,
     including an empty child table and two separate collation objects with
     identical provider and locale settings. Orphan counts cannot detect this
     metadata condition (issue #234); SPEC 7.5 asks before the first statement.

     The probe covers every column pair in both `AddForeignKey` and a created
     table's keys. Stored columns use `pg_attribute` under their pre-rename
     names. Created, added and retyped columns use `pg_type.typcollation`:
     measured on PostgreSQL 18.6, `ALTER COLUMN ... TYPE varchar(20)` resets a
     text column's explicit collation to the type default. Retaining the old
     OID would refuse valid plans. Missing catalog metadata returns an unreadable
     count, distinct from compatibility; a noncollatable column's OID zero is
     known metadata. Row-projection failures do not suppress this independent
     check. Live tests compare 25 collation pairs with actual constraint DDL
     and cover renames, new columns, created tables, retypes and missing objects.

<a id="decision-478"></a>

478. **Connected PostgreSQL estimates own their catalog provenance.** The
     operator reads the plan's table and column names; catalog shape queries
     must read the original names. The old table-only translation left the
     column caller-supplied, so renaming and retyping an indexed column, or
     tightening one with a validated CHECK, lost its unknown estimate
     (issues #267 and #347). Collect the whole plan's column renames by UID
     before estimating, and remove the column argument from `against`.
     The CLI consumes this same representation without reconstructing names;
     original change indices and per-statement strategies remain intact.

     A created table has a distinct source variant with no catalog name to
     query (issue #296). Resolve table provenance by UID before associating
     declared names with it: a new table can reuse an existing table's old
     name, but the existing table's rename still measures its own UID. A
     rename followed by creation in declaration history, deployed together,
     demonstrates the difference on the engine. Missing existing objects
     remain unknown; created objects retain their static cost with no row
     count, because intervening planned inserts need not leave them empty.

     This extends 409's name separation without changing 399/430's boundary:
     estimates stay outside correctness risk, saved artifacts and checksums.
     Index-expression coverage, CHECK lifecycle and other cost refinements
     remain their own questions; no expression is parsed to infer a proof.

<a id="decision-479"></a>

479. **Nullability scans follow the statement and its surviving CHECKs.**
     A type change can carry a nullability tightening in the same ALTER TABLE
     (ADR-0011 Amendment 1). Its `SET NOT NULL` still scans when the type's
     widening rewrites nothing: the live regression measures 100,000 rows
     beside a plain widening's zero, with both relfilenodes unchanged (#271).

     A validated CHECK covering that column is a possible proof, not an
     expression this tool interprets (SPEC §8.2). The existing named regression
     `a_check_the_engine_may_prove_the_column_from_takes_the_scan_back_to_unknown`
     measures ordinary / validated proof / NOT VALID at 100,000 / 0 / 100,000
     rows. Only Reads becomes unknown; known Rewrite and Lock answers survive.
     A non-proving CHECK beside a folded tightening also stays unknown (#292).

     CHECK lifetime is the executed plan prefix, unlike the whole-plan catalog
     spelling map (478). Estimates carry earlier CHECK removals through table
     renames and keep newly created identities separate. The connected query
     excludes those names before selecting a candidate, so another surviving
     CHECK remains visible; a future drop or another table's drop cannot hide
     it. The canonical replacement plan adds its new CHECK after tightening,
     so the old catalog CHECK cannot supply that statement's proof (#291).

     Constraint names travel as a bound JSON array, preserving arbitrary
     identifier spelling without a separator convention or SQL interpolation.
     This refines advisory estimates only: 399/430's correctness-risk, approval
     and saved-artifact/checksum boundaries remain intact.

<a id="decision-480"></a>

480. **Narrowing projections exclude unconvertible rows, not whole keys.**
     `rows_after` supplies the post-plan rows to UNIQUE, unique-index, primary-key
     and added-FK probes. Its old `TypeChangeRisk::Safe` gate lost all of those
     questions when a column narrowed, including fitting rows and conversions
     that only round (#281, #424). The narrower type-level predicate originally
     proposed for #281 would still lose fitting rows of a pair that can raise;
     #424's updated row-level design and 449's `cannot_become` predicate keep
     those rows testable without adding another type catalogue.

     Stored branches and unchanged cells in updated rows carry an exclusion
     over the same raw value the projection casts. Inserted and explicitly
     updated cells already have their final declared type; they keep their
     ordinary projection. A predicate that is unknown on NULL is not a failed
     conversion: `IS NOT TRUE` retains that row for the primary-key NULL probe.
     No excluded value is replaced by a fabricated NULL. The existing type
     conversion probe still counts and names values the ALTER cannot accept.

     The exclusion alone is not an evaluation barrier. Live duplicate, orphan
     and updated-row probes all raised with a flat WHERE: PostgreSQL pushed the
     outer NULL test or key comparison beside the guard. An `OFFSET 0` fence
     around each guarded projection branch keeps that comparison above the
     exclusion. Unguarded and literal branches need no fence. This is confined
     to `rows_after`; #435's separate `planned_key_probes` aliases remain their
     own issue.

     The live matrix measures rounding collisions, fitting and overflowing
     rows on each FK side, NULLs, composite keys, planned inserts/updates and
     renamed catalog spellings. Duplicate counts name affected rows (375):
     `1.04` and `1.00` becoming `1.0` count two rows, not one group. The connected
     CLI regression uses a sequence-backed DDL witness, whose increments
     survive rollback, to prove refusal before statement one and successful
     reuse of the approved plan after the offending row is removed.

<a id="decision-493"></a>

493. **Unicode conversion probes measure the capacity ALTER enforces.**
    SQL Server's `nchar(n)`, `nvarchar(n)` and `sysname` capacities count UTF-16
    units, while `LEN` under an SC collation counts an astral surrogate pair
    once. Measure the unbounded `nvarchar(max)` conversion with
    `DATALENGTH(RTRIM(...))` against twice the target bound; the unbounded
    conversion avoids measuring an already truncated result, and trimming
    preserves ALTER's acceptance of trailing U+0020 spaces. Keep the target
    `TRY_CONVERT(...) IS NULL` test alongside capacity, so conversion validity
    does not disappear when a bounded Unicode target takes this branch.

    Bounded binary and varbinary sources have a different truncation rule. Measured
    on SQL Server, 258 bytes encoding 129 ordinary Unicode characters are
    refused by ALTER to `sysname`, while 200 bytes encoding 100 characters fit.
    A suffix of zero bytes beyond the 256-byte capacity may be discarded, but
    a suffix encoding a Unicode space may not. Inspect that binary suffix with
    `SUBSTRING(...) <> 0x`: SQL Server's binary comparison zero-pads its shorter
    operand, accepting only an all-zero suffix. This also handles fixed binary
    padding and odd-length suffixes without confusing source bytes with target
    character counts. In contrast, `varbinary(max)` follows the converted-text
    rule: a trailing Unicode space may be discarded, but a trailing zero unit
    may not. It therefore uses the unbounded Unicode measurement above. No
    catalog collation lookup or source mutation is needed.

    The live matrix compares every generated probe with the actual ALTER for
    all three Unicode targets, SC and UTF8 character sources, legacy ntext,
    binary/varbinary/max sources, ordinary numeric conversion, exact capacities,
    supplementary characters, spaces, zero padding, odd bytes, empty values
    and NULL. An oversized value is refused with 2628; accepted padding stays
    unblocked. Existing non-Unicode code-page probes, unbounded targets,
    timestamp refusal and static risk classification keep their boundaries.

<a id="decision-515"></a>

515. **A retype asks the catalog for the keys standing on its column, not only
     for the keys the plan drops (issue #503).**

     `key_drop_blockers` was built around a key *removal*: a `SetPrimaryKey`,
     `DropUnique` or `DropIndex` gives it something to ask the catalog about,
     and it names the foreign keys bound to that index (460). A bounded
     `varchar`/`nvarchar`/`varbinary` widening keeps its key and its index, so
     such a plan carries no removal at all — and the differ's own retype
     maintenance recreates only the foreign keys inside the managed projection,
     because that projection is the recorded schema. A key held by a table this
     project does not declare is therefore in nobody's list, and the first thing
     that notices it is the engine.

     Measured on 17.0.4075.5: widening `varchar(10)` to `varchar(20)` on a
     column an undeclared child references fails with 5074 naming the
     constraint, followed by 4922; the child's own column fails the same way,
     and the same column with no key on it widens. So the check now also takes
     every `AlterColumnType` whose dialect answer demands foreign keys removed
     (`RetypeDependents::foreign_keys`), and asks `sys.foreign_key_columns` for
     the keys that reference that column *and* the keys that column is part of
     — both refuse — minus everything the plan removes before it. What is left
     is named and refused during connected planning and again before apply,
     through the path the key side already uses; nothing is added to the
     approved plan, and no change is ever synthesized for the unmanaged table.

     **The name asked about is the environment's, not the declarations'.** A
     `RenameColumn` is `order_key` 3 and an `AlterColumnType` 9, so by the time
     the statement runs the column has its declared name — but this check runs
     before any statement. The reversal walks the plan backwards by the
     column's *uid* rather than by name, because a plan may rename two columns
     into each other's names and a name-matched walk can follow the wrong one.
     An absent column is a failed read and refuses, exactly as an absent key
     does; the table half already stops at `stored_key_table` for a table this
     plan creates.

     **PostgreSQL needs none of this.** Measured on 18.6, the same widening
     under the same external foreign key succeeds — the engine rebuilds the
     constraint itself. What it refuses is a type the key cannot be implemented
     over at all (`varchar` to `integer`), which is an incompatibility rather
     than a dependency standing in the way, and is not what this check is for.

<a id="decision-516"></a>

516. **The guard a narrowing key's probe carries is the condition of a `CASE`,
     not a conjunct beside the `CAST` it protects (issue #435).**

     A key whose referenced column this plan narrows compares through a `CAST`
     that can raise, and the row it would raise on is excluded with `AND NOT
     (<guard>)` (449). On the two parent aliases — the row a delete targets
     (`p`) and every row that survives it (`q`) — that exclusion sat in the
     same flat `WHERE`-clause conjunct list as the `CAST`, and a conjunct is
     not a barrier: PostgreSQL orders the quals of one clause by cost.

     **Measured on 18.6.** With the guard written first and made the costlier
     of the two, the plan came back reordered — `Filter: (((c)::integer = 7)
     AND (NOT pricey(c)))` — and the query raised `integer out of range` on a
     row the guard was there to keep it away from. Written through a `CASE`
     whose condition is the guard, the same query answers. A probe that raises
     is reported as *unchecked* rather than as a violation, and `apply`
     proceeds, so the failure is silent in the direction that matters.

     Today's guards are cheap builtins and today's plans order safely; the
     change is that the order stops being the planner's to choose. This repo
     has been here before: DECISIONS 324 records a cast folded to planning time
     ahead of its `pg_input_is_valid` guard, fenced with `OFFSET 0`. The orphan
     probe beside this one still uses that fence, which is why the two shapes
     differ — the fence suits a subquery's target list, the `CASE` suits an
     expression in a `WHERE`.

     **The exclusion stays a separate term.** A guarded row must be *absent*
     from the comparison, not merely compare as `NULL`, which already means
     something else in this query (449): a parent row that silently stopped
     matching every child would over-count every child pointing at it.

     **The child side is left alone**, and measured rather than assumed. Its
     guard is not a sibling of the `CAST` but of the whole `EXISTS` the `CAST`
     sits inside, and on 18.6 that `EXISTS` is pulled up into a join whose
     condition carries the cast — evaluated after the scan filter that carries
     the guard. Measured: the same fixture answers rather than raising.
