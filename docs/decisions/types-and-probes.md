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

    *Amended by [DEC-1631.1](#dec-1631-1): the loader holds a bare `type:
    serial` as its integer type and an owned sequence, which is what reads
    back; the refusal stays for a `serial` that reaches the dialect as a type.*

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

    *Amended by [DEC-2331.1](#dec-2331-1): the refusal stands for a change with
    no recorded conversion, and its message now names `pbps convert`. A change
    with one is made with the recorded `USING`.*

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

    *Amended by [DEC-2331.1](#dec-2331-1): a conversion recorded for the change
    is the declared, reviewed statement of what the values mean, so the change is
    made with it, and #1919's idea of pbps writing the recorded zone in itself
    stays ruled out.*

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

<a id="dec-399-1"></a>

**DEC-399.1. Planned risks come from the selected dialect's `change_risks`, and
`Change::intrinsic_risks` is kept only as the dialect-free default (#399;
closes the gap DECISIONS 433 left to #343).** DECISIONS 433 recorded that the
model's `Change::intrinsic_risks` read every dialect's defaults by SQL Server's
identifier boundary. It called that a known gap and left it to #343. #397 closed
it with the parameterized shape DECISIONS 433 and ADR-0011 Amendment 2 had
already established for `normalize_definition` and the dependency scans:

- `Change::intrinsic_risks_with(continues_ident)` takes the boundary.
- `Dialect::change_risks` calls it with the engine's own
  `Lexicon::identifier_continues` and adds the type-change risk.
- Both halves that have to agree ask the one `Dialect::change_risks`: the
  differ, which sets each `PlannedChange`'s risks (`pbps_diff::diff`), and
  saved-plan validation, which re-derives them and refuses a plan whose file
  says otherwise (`validate_saved_plan`). A plan cannot be classified by one
  rule and checked by another. Every change a connected planner adds after the
  diff is classified the same way, for the same reason (DEC-314.1).

`Change::intrinsic_risks()` and `PlannedChange::new` keep SQL Server's boundary
as their default. That is an API-compatibility role for dialect-free callers,
not the classification any plan is saved with: every change that reaches a
saved plan has its risks overwritten by the selected dialect's answer before
the plan is written. The historical entry (433) keeps its wording, and this
entry records that the gap is closed. Pinned by
`required_add_risks_follow_the_dialects_identifier_boundary` and
`planned_required_add_uses_postgres_default_identifier_boundaries` in
`crates/pbps-pg/src/lib.rs` (planning), and by
`saved_add_column_risks_use_the_selected_dialect_and_reject_edits` in
`crates/pbps-cli/src/main.rs` (saved-plan validation).

<a id="dec-415-1"></a>

**DEC-415.1. A PostgreSQL temporal type's precision change is estimated as a
rebuild only when the precision narrows, and an omitted precision is the
engine's 6 (#415).** Admitting temporal precision (#130, #414) made
`timestamp(3) -> timestamp(6)` and its family reachable. The estimator's
`rewrites` had no arm for them, so every such change fell through to
`Rewrite::Yes`, an answer nobody had measured. ADR-0012 §3–4 ask for the
engine's own answer.

Measured on 18.6 and 16.15, with `pg_class.relfilenode` compared before and
after each `ALTER COLUMN … TYPE`:

- the four types are `time`, `timetz`, `timestamp` and `timestamptz`;
- every ordered pair of an omitted precision and `(0)`..`(6)` was tried;
- each pair ran on an empty table and on a populated one;
- that is 512 statements, all accepted, and the two engines answered
  identically.

The rule has no exception. The table is rebuilt exactly when the target
precision is below the source's, counting an omitted precision as 6. So
`timestamp -> timestamp(6)` is free in both directions, and
`timestamp -> timestamp(5)` rebuilds. Populated and empty tables agree: the
rewrite is a property of the statement.

The column's indexes were measured with it. The altered column carried both
a plain index and a primary key's. Each index's `relfilenode` moved in exactly
the 216 narrowing statements, and in none of the 296 that hold or widen the
precision, on both engines. So the index side of ADR-0012's boundary needs no
new answer. `against` takes an indexed column's estimate back to unknown only
where the table is rebuilt, which drags the index along. A widening that
rebuilds nothing rebuilds no index either, and its `No` stands.

The estimator now answers this family by that rule, for the same base only. A
change into another base keeps its existing answer, including the
session-dependent `timestamp -> timestamptz`. `TypeChangeRisk` is untouched,
because cost stays apart from correctness (DECISIONS 399). The live suite
re-measures the whole matrix and holds the estimate to every statement, so
the rule cannot go stale without a red build.

<a id="dec-419-1"></a>

**DEC-419.1. Narrowing `timestamp` or `timestamptz` precision gets no count
and no extra refusal, although a value at the very top of the range rounds to
one the engine will not re-read (#419).** Measured on 18.6 and 16.15, with the
same results on both, for `timestamp(6)` and `timestamptz(6)` narrowed to
every precision 0 through 5, under `UTC`, `Pacific/Kiritimati` and
`America/Adak`:
- **Every `ALTER` succeeds.** Nothing is refused at the upper bound, at the
  half-rounding threshold, or at the BC lower bound.
- **Rounding carries past the upper bound.** A value within half a unit of the
  bound (`23:59:59.5` at precision 0, `.95` at 1, up to `.999995` at 5, on
  `294276-12-31`) is stored as `294277-01-01 00:00:00`, rendered in the session
  zone for `timestamptz`. That text is then refused as a literal with
  `timestamp out of range`.
- **Nothing else loses the round trip.** One microsecond below each threshold,
  the value keeps its day and round-trips, and so does the BC lower bound.

There is no count because SPEC §15 counts what the engine refuses, and the
engine refuses nothing here. A predicate that labelled these rows "would fail"
would report a failure the `ALTER` does not have. There is no extra refusal
either. Every precision reduction is already in the `narrowing` class, so it
passes the gate only with `--allow narrowing`, which is the place where lost
fractional seconds are accepted. The carried value is also loud rather than
silent: a later statement that spells it as a literal fails on the engine. The
live fixture `timestamp_precision_narrowing_rounds_past_the_upper_bound_without_refusing`
pins both sides of each threshold and the lower bound. If a release starts
refusing, it fails there, and a count then belongs in the probes.

<a id="dec-605-1"></a>

**DEC-605.1. Adding an identity column rebuilds the table and reaches every
row, which narrows Decision 401's "`ADD COLUMN` reads nothing" (#605).**
Decision 401 and ADR-0012 Amendment 2's scan table were measured on plain and
literal-default additions. An identity addition is different, although it
too carries no explicit default: the sequence backfills a value into every
stored row. Measured on PostgreSQL 18.6 and 16.15, `ADD COLUMN d int
GENERATED ALWAYS AS IDENTITY` changes the table's `relfilenode`, even when
the table is empty, and every populated row receives its own sequence value.
`pbps_pg::estimate` therefore answers `Rewrite::Yes` and `Reads::EveryRow`
for it. Plain and literal-default additions stay metadata-only, and an
expression default stays `unknown` (DECISIONS 403). Decision 401 keeps its
text and number; this entry is the correction. The unit test
`adding_an_identity_backfills_rows_without_an_explicit_default` and the live
`the_estimate_says_what_the_engine_does_about_rebuilding_the_table` pin both
sides, with the plain and literal-default additions as negative controls.

<a id="dec-1167-1"></a>

**DEC-1167.1. A PostgreSQL array column is `T[]`: an array marker on its
element type, not a rank. An element change is taken only where every element
keeps its value (#1167).**

Measured on 16.15 and 18.6, with identical results:

- `int[3]`, `int[][]`, `integer ARRAY[4]` and `_int4` all read back as
  `integer[]`, with `attndims` of 1, 2, 1 and 0. Neither a declared size nor a
  dimension count is enforced. Element typmods are kept, a modifier inside the
  name included: `timestamp(3) with time zone[]`.
- Every element type change rewrites the table, the widenings a scalar takes in
  place included: `integer[]→bigint[]`, `varchar(10)[]→varchar(20)[]` and
  `numeric(10,2)[]→numeric(12,2)[]` each gave the table a new `relfilenode`.

**The model.** `ColumnType` carries one array marker. A declaration writes
`T[]`. The other spellings are refused by name, because they would read back as
`T[]` and so be either rewritten silently or drift forever. Scalars serialize
exactly as before. A routine's argument is a `RoutineArg`, not a `ColumnType`,
so routine signatures are untouched. The reader keeps a type only where it
parses and normalizes back to itself. So an array of an element pbps cannot
name, such as a quoted `"char"` or a user-defined type (#1523), stays a named
limitation and is never read as a scalar.

**Changes.** An array keeps its element's risk class. A change between a
scalar and an array is `Incompatible`, and the emitter refuses it for the
`USING` it would need (ADR-0012 §5). The emitter also refuses an element change
that is not `Safe`. Narrowing needs element-wise probes over NULL arrays, NULL
elements, empty arrays, nondefault bounds and shape, and none count them yet,
so the narrowing probe is not built for an array at all. The plan's note says
the change is refused rather than counted. The estimate is a rewrite that reads
every row under `AccessExclusiveLock`.

**Rows.** `data:` on a table with an array column is refused. A row's `Value`
is scalar, and an array's equality includes its bounds (`'[0:1]={1,2}'` is not
`'{1,2}'`), so rows beside an array wait for a codec of their own.

**SQL Server** refuses an array type by name, rather than reading `int[]` as
`int`.

**Every check that reads a type's base name was audited for arrays once**
(#1525 review), because an array's base is its element's.
- An identity on `integer[]` is refused.
- So is a GIN index over `jsonb[]`, in validation and in the reader, which
  leaves one out.
- A default whose meaning depends on the session is held to the same rule for
  an array of such elements, which is right as it stands.
- The row reader refuses a table with an array column by name, before any
  query. So does `pull --data`, which reads rows before any `data:` is
  declared, rather than reading `{1,2}` as a malformed integer.
- Every other check is behind an array test already: the estimate, the risk
  classification and the probe.

<a id="dec-1631-1"></a>

**DEC-1631.1. A PostgreSQL `serial` is held as its integer type and a
sequence the column owns, declared `type: serial`, and read back as one only
where it is exactly the shape `serial` makes (#1631).**

Amends DECISIONS 227. `serial` is still not a type: what changed is that the
loader no longer passes it to the dialect as one. In the model it is one more
exclusive value source, `ValueSource::Sequence`, beside a default, an identity
and a generation expression, so a `serial` with any of those is a second
source, refused by name. `type: serial`, `serial4`,
`bigserial`, `serial8`, `smallserial` and `serial2`, as the column's whole
bare type, load as `integer`, `bigint` or `smallint` with an `OwnedSequence
{start, increment, name}`, and that is what the reader builds from the
catalog. So the declared column and the read-back one are the same value, and
DECISIONS 227's phantom change cannot arise. A `serial` reaching the dialect
as a type (`serial[]`) is still refused.

Measured on 16.15 and 18.6:

- `serial` makes the column `NOT NULL`, a sequence `AS` the column's type in
  the table's schema and persistence, with cache 1, no cycle and the
  type's positive bounds, a default exactly `nextval('<seq>'::regclass)`, and
  an `a` dependency of the sequence on the column. So the model's column is
  `NOT NULL`, with `nullable:` left out or `false`; `nullable: true` is
  refused, since a NULL in it is a row the sequence was never asked for. The
  reader holds a column as a `serial` only where every one of those holds,
  its sequence is used by no other default, and the table is not
  partitioned (a partition
  carries its own copy of the parent's default, so the parent's sequence is
  called twice; #1893). Anything else,
  a shared sequence, one the column does not own, or another schema, stays
  the raw default it is, with the sequence named, as before. A cache, bounds
  and a cycle were named too, and are held since DEC-1921.1.
- `ADD COLUMN` with a `nextval` default rewrites the table and numbers the
  stored rows from the sequence's start by its increment. The emitter creates
  the sequence by name with `START WITH` and `INCREMENT BY` first, then the
  column with that default, then `ALTER SEQUENCE … OWNED BY`, all in one
  statement. Writing `serial` in the DDL could not carry the start, which an
  added column numbers its rows from. `OWNED BY` needs the sequence and the
  table to have one owner, and `CREATE SEQUENCE` gives the sequence to the
  running role, so for a table that already exists the sequence is given to
  the table's owner first, read from the catalog at apply time, as the
  engine's own `serial` does. Measured on 16 and 18: without it, a superuser
  or a member of the owning role was refused. The estimate is a rewrite, and the pre-flight treats the
  backfill as the identity's: a value no probe can evaluate.
- Dropping the table or the column drops an owned sequence with it, so the
  plan has nothing to drop.

**The name** is held only where it is not the generated `<table>_<column>_seq`,
and compared only where both sides give one. A rename keeps the sequence's
name, so the generated one stops matching without anything having changed.
That makes a missing name mean two things. In a read it is the name the
engine generated under the table and column read, so it is known, and a base
read from the database or a recorded snapshot is named in full before the
differ compares it. In a declaration, and so in a git baseline, it is not
known: the sequence may carry a name an earlier table generated, so a later
pull's name for it is no change. A declared name equal to the generated one
is refused as redundant. A declared name is claimed in the declarations'
namespace, after every claim the engine retries, since it never retries
this one. An unnamed sequence claims nothing there: the name its table and
column would generate may be no sequence's after a rename, and claiming it
refused a table declared under it. The name a plan creates one under is the
connected plan's to claim (#1899).

**Refused**, each by name:

- A start or increment outside an identity's bounds for the type, a nullable
  `serial`, one beside a `default:`, `identity:` or `generated:`, one on a
  partitioned table or partition, and a type no `serial` stands for.
- Adding, removing or changing the sequence of a kept column, and retyping a
  kept `serial`. These are #1889's: a sequence added to a column that holds
  values starts where it may meet them, and one left behind by a retype keeps
  its old type's bounds. Creating a table and adding a column are supported.

  *Amended by [DEC-1889.1](#dec-1889-1): each is planned now, as one
  change that continues past the column's values.*
- A `serial` added to an `UNLOGGED` table, whose persistence the change does
  not carry.
- A `data:` block keyed by a `serial`, for DECISIONS 321's reason: a pinned
  key leaves the sequence behind, and the application's next insert fails.
  A `serial` beside the key is the engine's to number, as an identity is
  (DECISIONS 94): a row that sets it is refused, every row leaves it out, and
  the row reader never reads it. Read and held to an omission instead, an
  inserted row is held to NULL where the sequence wrote a number, and the
  apply rolls back (`a_data_block_beside_a_serial_leaves_it_to_the_sequence`).
- A `serial` on SQL Server, whose form is `identity:`.

**Kept distinct from identity.** A role with `INSERT` alone cannot insert
into a `serial` column (ADR-0010 §7), and a column with a default cannot be
given an identity in place, so a pulled `serial` is never rewritten as an
identity. A role's `INSERT` on the table does not yet carry the sequence
grant it needs, and a pulled sequence grant is still reported (#1892). The
CDC effect of numbering a populated table's rows, which logical decoding does
not see, is #1890.

The live `a_serial_round_trips_and_numbers_the_rows_it_is_added_to` pins the
round trip on both versions, the numbering of an added column, and the
shared and unowned sequences as negative cases.

<a id="dec-1775-1"></a>

**DEC-1775.1. Preflight projects the prefix before each statement; the shared
walk records facts and the dialect interprets them.** The checksum-pinned
change list supplies the boundary, rather than a second copy of the planner's
sort classes. `PlanFacts` in `pbps-dialect` borrows `PlanNames` and classifies
every `Change` exhaustively once. `before(k)` contains steps `[0, k)`;
`before(len)` is the whole-plan endpoint, including for an empty plan. A
removal keeps the stored identity resolved at its own step (DEC-1770.1).
Column additions and retypes remain separate events: retaining only the latest
one would lose a backfill followed by a conversion.

Row writes share tuple bookkeeping, unknown assignments, gone-key unions and
omission classification. SQL literal readers, assignment conversion,
collation, added-column backfill and PostgreSQL partition reach remain in the
dialects. In particular, PostgreSQL backfills a nullable added column's default;
SQL Server does not (decision 171). Each probe materializes its prefix only
when it needs engine state, so changes with no probe do not repeatedly build
row projections and no plan-sized snapshots are retained at every boundary.

Measured on SQL Server and PostgreSQL 18/16: a NOT NULL statement before the
update that fills a NULL must count that NULL; the reverse order counts zero
and succeeds. A PostgreSQL parent tightening before an attach must not count
the unattached child's NULL, while a tightening after it must. The old
whole-plan projection gave the opposite answers in these cases. The live
`tightening_reads_prior_writes_and_never_future_writes` and
`tightening_sees_an_attached_child_only_after_the_attach` fail with the
whole-plan projection restored and pass with positional reads.
`an_attach_cannot_borrow_a_later_detachs_empty_range` also measures that a
DEFAULT partition's rows remain range blockers until its detach has run.

The current statement still owns its simultaneous effects: a DELETE removes
its target's self-reference, and a CREATE's inline foreign keys see the
columns it defines. Neither requires making later statements visible.
A foreign key added after a row deletion validates that deletion at the key's
own step. Its current definition is explicit input to the dialect's planned-key
checks; it is not inserted into the prefix as if already installed. This keeps
the existing converted-value, backfill, survivor and unreadable-child guards
without making a future key visible to an earlier DELETE. Measured on
PostgreSQL 18/16, repairing the child between DELETE and ADD FOREIGN KEY makes
the key succeed, while the same repair after ADD does not:
`a_new_key_validates_writes_before_its_own_step_and_never_after_it`.

The extraction was first checked against 192 existing preflight reports with
byte-identical SQL and diagnostics before changing the read boundaries.


<a id="dec-1977-1"></a>

**DEC-1977.1. A delete owns its row's removal at its probe boundary.**

`PlanFacts::before(step)` excludes the current statement. Keep that boundary
for names, columns, defaults and preceding writes; overlay the current DELETE
on the row projection used by its probes. The target cannot remain a surviving
parent or a referencing child, including a reference introduced by an earlier
update to that same row. `RowWrites::delete` records the removal and discards
that row's earlier assignments and unknown-value evidence. Both dialects use
the same operation for preceding deletes and for the current delete, replacing
the separate SQL fragments that excluded only the stored self-reference.
Other rows' assignments and unknowns remain visible.

Measured on PostgreSQL 18/16, adding a NO ACTION key and then deleting its
referenced parent counted zero while the engine refused: the target was still
its own surviving parent. A preceding repair permits the delete; the same
repair after it does not. Planned keys are also retired when their own DROP
runs. Keeping an already dropped key after fixing the survivor projection
would refuse a valid delete; a later key with the same name must participate
again. Constraint names therefore remain raw facts on `Event::AddForeignKey`.

Measured on SQL Server and PostgreSQL 18/16, updating a row from a reference to
another row into a self-reference and then deleting it counted one while the
engine accepted. A second referencing row counted two instead of one. Clearing
the deleted row's earlier writes makes these counts zero and one respectively.
`a_catalog_key_counts_only_references_that_survive_the_current_delete` runs the
same cases against both dialects; PostgreSQL also checks planned keys in
`a_planned_key_counts_only_references_that_survive_the_current_delete`.
`a_new_key_validates_writes_before_its_own_step_and_never_after_it` retains the
before/after key controls and checks the DELETE boundary and retired keys.
Name reads never move past the current statement, and later repairs remain
invisible. Conversion, collation, backfill and key comparison stay engine-owned.


<a id="dec-1634-1"></a>

**DEC-1634.1. `inet`, `cidr`, `macaddr`, `tsvector` and `xml` are PostgreSQL
column types: `cidr -> inet` is `Safe`, `inet -> cidr` narrows and has a probe,
each of the five into a string narrows, every other pair is `Incompatible`, and
every write and the read scope pin `xmloption = content` (#1634).**

Before this a column of one of them was read as opaque with a limitation, and
the emitter then raised `postgres has no type inet` (`UnknownType`) for the
table, which stopped the GitLab measurement of #1616. It was a catalogue gap and
nothing else: row values are read with `CAST(… AS text)`, defaults through any
`::type`, generated expressions are carried verbatim, and the resolver already
listed all five output functions. Everything is in `types.rs`, modelled on
`uuid`, plus one pair in `estimate.rs` and the pin.

**The catalogue.** Five rows, `ArgShape::None`. Measured on 16.15 and 18.6:
none takes a modifier or has an alias, and `format_type` spells each by its bare
name. They are also in `CATALOG_NAMES`, so `NULL::pg_catalog.inet` is the
default the engine keeps, and the routine-argument fold now reads `_inet` as
`inet[]` as the catalog does. `Family` gets `Inet { cidr }`, `MacAddr`,
`TsVector` and `Xml`; none may be `Unknown`, because both the to-text arm of
`change_risk` and the length probe's allow-list key on that.

**The classes**, from `pg_cast` on both versions and a 25-by-25 matrix of
`ALTER … TYPE` on an empty table (the live matrix now holds them):

| Change | Engine | Class |
|---|---|---|
| `cidr -> inet` | implicit, binary-coercible | `Safe` |
| `inet -> cidr` | assignment, function | `Narrowing`, with a probe |
| any of the five -> `text`, `varchar(n)`, `char(n)` | assignment (the string casts every type has) | `Narrowing` |
| `text`, `varchar`, `char` -> any of the five | none, or explicit only | `Incompatible` |
| every other pair | none | `Incompatible` |

`macaddr` and `tsvector` appear in `pg_cast` not at all, and `xml` only to
strings; the strings are reached by I/O conversion, which is an assignment
cast. A change between two of them is therefore never "no change": the diff
compares normalized types, they are different types, and `change_risk`
short-circuits only on equality.

**`inet -> cidr` has a probe although it does not raise.** Measured, the
`ALTER` succeeds and `10.0.0.1/8` becomes `10.0.0.0/8`, `2001:db8::1/64`
becomes `2001:db8::/64`: the host bits are cleared and nothing says so, and two
addresses of one network become one value, which a key or a join then treats as
the same. The narrowings `preflight` leaves without a probe (a `numeric`
scale, a float into an integer) have no row to point at, only a precision lost
on nearly every row; here the rows that change are exactly the rows one
comparison names, `a <> a::cidr::inet`
(measured: false for `10.0.0.0/8`, a `/32`, `::1` and `NULL`, true for the two
above). So `cannot_become` answers it, the apply stops before the first
statement on a count, and the class stays `Narrowing` so the gate still asks.
It is the one narrowing that does not raise which is counted, and only because
the predicate is exact. `cidr -> inet` has none: nothing moves.

**The length probe is accurate for all five**, so they enter
`renders_alike_under_the_pins`. Measured, `ALTER … TYPE varchar(n)` stores the
same string as `::text` (`10.0.0.1` is `10.0.0.1/32`, a `tsvector` is its sorted,
quoted lexemes), `varchar(L)` is accepted and `varchar(L - 1)` refused at the
rendered length `L` for each. None of their output functions reads a pinned
setting; in particular `xml_out` ignores `xmloption`, which decides what
*input* is read as.

**Rewrites.** Measured with `relfilenode`, `cidr -> inet`, `xml -> text` and
`xml -> character varying` rewrite nothing, and `xml -> character varying(n)`,
`xml -> character(n)`, `inet -> cidr` and every other string target do. The
first two are binary-coercible casts with no length check; all three are on
`estimate.rs`'s free list. The issue's assessment named only the first, and the live
estimate matrix refused the estimate for the other two until they were listed.

**`xmloption = content` is a pin.** It is the parser class of DECISIONS 267: a
setting that decides what declared text means. Measured, `'abc'::xml` and
`'<a/><b/>'::xml` are accepted under `content` and are `invalid XML document`
under `document`, so the same `DEFAULT` or row is a valid plan from one session
and a refusal from another; a fragment is the ordinary case. `content` reads
every document and every fragment, so it never refuses what `document` takes,
and a text both accept is stored as the same value: `xml` is therefore not on
`emit::SETTING_SENSITIVE`, whose rule is that the same characters mean
different values.
The server's default is already `content`, which is why nothing failed before:
only a database or role that set the other would. It is in the write pins
(`session_pins`, so the framing, staged and resumed writes and every rendered
script) and in the canonical read scope, because the spelling check
`pg_input_is_valid(v.s, 'xml')` runs there and must judge a cell as the apply
will store it. `flow_pg`'s hostile-session script and its staged variant now set
`xmloption=document` and declare an `xml` default of a fragment.

**Left out, and reported.** `macaddr8` stays opaque with its limitation (its
casts to and from `macaddr` are implicit, and `macaddr8 -> macaddr` can fail at
run time, so it needs its own arms and a probe). A GIN index over a `tsvector`
stays a limitation, because it changes the published index text (DEC-1169.1):
#1984. `gist` over `inet` is left out too. `xml` has no `=`, so a key or a btree
over it is refused by the engine (42704) exactly as `json` is, and pbps's own
key probes are reached only in that refused case; the live key test now holds
`xml` beside `json`. A generated `tsvector`
(`to_tsvector('english'::regconfig, body)`) already works: created under the
empty `search_path` it reads back as exactly that, and the one-argument
`to_tsvector(body)` is refused by the engine as not immutable, so pbps adds no
rule for it.

Pinned by `the_five_added_types_are_classified_by_the_casts_the_engine_has`,
`only_inet_into_cidr_counts_the_rows_whose_host_bits_would_be_cleared`, the
live `inet_into_cidr_is_a_narrowing_whose_probe_counts_the_rows_with_host_bits`,
`inet_cidr_macaddr_tsvector_and_xml_columns_pull_recreate_and_repull_to_an_empty_plan`,
`a_declared_xml_or_network_value_is_read_under_the_pins_and_not_the_sessions_xmloption`
and the extended matrix, spelling, estimate and key tests.


<a id="dec-1889-1"></a>

**DEC-1889.1. A PostgreSQL column's numbering source, an identity or an owned
sequence, is put on, taken off, retuned or converted in place by one change,
and one put on continues past the values the column holds (#1889).**

Amends DECISIONS 5 and DEC-1631.1 on PostgreSQL. SQL Server has no `ALTER`
for an identity, and there DECISIONS 5 stands.

`AlterColumnNumbering` carries the column's source before and after: an
identity or an owned sequence on at least one side, and on the other one of
them or nothing. A plain default given up for a number, or taken in place of
one, is an `AlterColumnDefault` beside it, so it is ordered as every default
is: set after a retype to a type only the new one holds, and after the
functions it calls (#1930 review). To or from generated still has no
in-place form (DEC-1168.1). Neither has a kept source whose increment changes
sign: its sequence keeps the bounds of the direction it was made for, which
a start in the other refuses, and is refused by name
(`NumberingDirectionChange`). The change is one statement, a `DO` block, because a
staged apply checkpoints between statements, and a checkpoint between giving
up the old source and installing the new one, or between installing it and
moving it past the column's values, leaves a column that numbers into the
rows it holds.

**Where it continues.** Measured on 16 and 18, a source put on a column that
holds values starts at its declared start, where it meets them; and neither
`RESTART`-free form of a retune moves the counter. So a source put on, alone
or by conversion, is set (`setval`) to the greatest of the declared start,
the column's greatest value plus the increment, and for a conversion that
counts the same way the old source's next number; for a negative increment,
the least of the mirror values. An old source counting the other way says
nothing of where the new one goes, and one step past its exhausted counter
is past its type (#1930 review). The declared start stays the sequence's `START WITH`, so the next
plan reads the start it declared and is empty: the start is a floor, and a
start above the column's values is honoured exactly. Moving the sequence by
hand after the change, as the obvious alternative leaves it, has a gap in
which a row can arrive and collide, and a step that can be forgotten.

This is not what ADR-0013 §2 refused. DECISIONS 321 found six obstacles to
advancing a sequence past rows a plan pinned, and they are the obstacles of
a sequence that outlives the statement: a `nextval` walks past any lock, an
allocation cannot be recalled, an advance survives a rollback. Here the
table is locked `ACCESS EXCLUSIVE` first, so every value read is in the
table or rolled back, and the sequence moved is always one the block itself
created, so a rollback takes the move with it. A sequence that cycles is
held since DEC-1921.1, but never under a key, so its wrap cannot reach a
value the block continued past.

**The plan text does not depend on the data.** The block computes the
continuation at apply time, so an offline `plan` and `plan --db` give the
same plan. A connected plan's cost report says where numbering continues
where a b-tree index leads with the column, in one index probe (a hash or
BRIN index gives no ordered probe); without one it
says the apply reads it, since the read would scan every row. The pre-flight
counts the values too near the type's bound for numbering to continue past
them, which would otherwise refuse the `setval` mid-apply. For a conversion
that counts the same way it also reads the old counter: called, its next
number is `last_value + increment`, computed in `numeric`, and past the new
source's bound it leaves nothing to continue at. A counter this login cannot
read is counted as exhausted, as for a bound moved in place, and a retype the
plan makes first leaves the change unchecked (#2005). Pinned by the live
`a_conversion_past_an_exhausted_counter_is_refused_before_apply`, on 16 and
18.

**The gate**, `numbering`, asks only for the changes that can make a writer
working today fail:

| Change | Gated | Why |
|---|---|---|
| An owned sequence put on | no | A writer supplying its own value still can, and the sequence starts past it. A writer leaving the column out needs `USAGE` on the new sequence (ADR-0010 §7). On a column with no source, no such writer works today. One relying on a plain default the sequence replaces is that default's change, ungated as every default dropped is, and its grant is #1892's. |
| An identity put on | yes | pbps's identity is `GENERATED ALWAYS`: an insert supplying a value is refused (measured: "cannot insert a non-DEFAULT value"). |
| Owned sequence → identity | yes | The same refusal, and the sequence's name goes, so `nextval('<seq>')` and a grant on it break. |
| Identity → owned sequence | yes | A writer leaving the column to the identity needed no privilege. The sequence is a grantable object the identity was not, and without `USAGE` on it that insert fails (ADR-0010 §7, #1930 review). |
| A source given up | yes | An insert relying on the source fails `NOT NULL`. |
| Start or increment changed | no | Without `RESTART`, no number already issued is issued again; a changed start affects only a rebuilt table. |
| A known sequence name changed | `rename` | The default reaches it by oid, but a caller by name loses it, as a renamed table's do. |

A conversion is one change, not a removal and an addition: the counter has
to cross from the old source to the new inside one statement.

An identity put on always follows its column's tightening, which the engine
requires. Where the plan inserts rows into the table, both stay in class 9,
ahead of the inserts, instead of the tightening following the rows as it
otherwise would (#1367): a declared row never writes an identity column, so
it takes its number once the identity is on. Where the plan only updates or
deletes the table's rows, both follow them, at the end of the deletes'
class, because a delete may remove the rows holding NULL (#1930 review).
Where it both deletes and inserts them, the deletes, the tightening, the
identity and then the table's inserts run in that order, all at the end of
the deletes' class. Another table's rows referencing that table would then
be written before the rows they may reference, so they are refused by name
(`RowsReferenceLateIdentityRows`), and the row changes go in a plan of
their own first. A key the plan adds itself, a created table's among them,
is added after every row, so it refuses nothing.

An owned sequence given up, renamed, or turned into an identity on a column
the plan neither tightens (so already `NOT NULL`) nor retypes (the identity
takes the column's type when added), goes at the end of class 6, before the
tables and columns this plan creates, one of which may take its name; none
needs a class-9 change first. Among them, a rename to a name another frees
follows it, down the chain. A cycle of renames has no such order and is
refused by name at diff time (`SequenceRenameCycle`), as a table or column
rename cycle is (DEC-541.1, DEC-1966.1), not left to the engine mid-apply
(#1930 review).

A `serial` put on follows the table's deletes as an identity does, without
the tightening: it continues past the values left, and the greatest may be
one a delete removes (#1930 review). The bound probe is reported unchecked for a table whose
rows the plan deletes, since the values stored now are not all the ones
the numbering reads.

**Order.** In class 9, a source given up, and an identity turned into a
sequence, go first, at −3: measured, `DROP NOT NULL` on an identity column
is refused ("is an identity column"), and a plain default taking a
sequence's place is set at 0, after the statement that drops the old one.
Every other numbering change goes last, at +2: an identity is put only on a
column already `NOT NULL` ("must be declared NOT NULL before identity can be
added"), and a sequence is created or moved `AS` the type the column ends
with, so after the tightening and the retype (ORDERING.md).

**A retype under a kept `serial`** moves its sequence with `ALTER SEQUENCE
… AS`: measured, the engine moves an identity's sequence with its column and
leaves an owned sequence's at the old type, which the reader then no longer
holds as a `serial`.

**Found, not named.** The sequence a column has is found by its dependency on
the column (`pg_get_serial_sequence`), not by a name the plan holds, which a
base that never saw the sequence renamed does not know (DEC-1631.1). A name
changes only where both sides give one. Every read of pbps's own runs on the
empty path under the catalog's operators (DEC-1564.1), and a sequence put on
a table another role owns is given to that role first, as an added `serial`'s
is (DEC-1631.1).

**Refused, each by name:** a numbering change on a partitioned table or a
partition, whose partitions each carry their parent's source (#1681, #1893);
an owned sequence put on an `UNLOGGED` table, since it is created logged
(DEC-1631.1); and a nullable identity or `serial`, by validation. A
sequence's min, max, cycle and cache, named limitations here, are held since
DEC-1921.1.

The live `numbering_sources_change_in_place_and_continue_past_the_rows`
pins every kind on 16 and 18, the continuation from values, from a
conversion's counter and counting down, the gate, and the cost report;
`a_serial_added_to_another_roles_table_is_that_roles` pins the owner and
the operators.

<a id="dec-1921-1"></a>

**DEC-1921.1. A PostgreSQL identity's or owned sequence's bounds, cycle and
cache are declared, read back and changed in place, and only a change that
ends the numbers sooner is gated (#1921).**

Amends DEC-1631.1 and DEC-1889.1, which left them named limitations: a
column whose sequence had any of them pulled as a raw default, or as an
identity that read back with the engine's defaults, and could not be
declared at all.

**Declared.** `identity:` takes the `[seed, increment]` pair or a mapping
`{seed, increment, min, max, cycle, cache}`; a `serial`'s `sequence:` takes
`min`, `max`, `cycle` and `cache` beside `start`, `increment` and `name`.
The pair stays the spelling of an identity with none of them, and the
writer keeps it. Each option left out is the engine's own, so the model
holds `None`, not a number: measured on 16 and 18, a retype moves a default
bound with the type and keeps an explicit one, which `None` reproduces and a
stored number would not. Spelling the default (`min: 1` counting up, a
`cache: 1`) is refused with "leave out", so one sequence has one
declaration and a pull of it plans to nothing.

**Read** only where the engine made the sequence: an identity's sequence
altered `AS` another type than its column's has that type's bounds, which
read as declared would let a retune through that the engine refuses. It is
named, and reads back with the engine's defaults, as before.

**Validated** against what the engine refuses or reads back otherwise: a
bound outside the type, bounds that do not cross (`min < max`), a start
outside the declared bounds (the engine's `START value … cannot be less than
MINVALUE`, against the bounds the declaration sets rather than the type's),
and a cache below 2. **A cycle under a key is refused,** naming the primary
key, unique constraint or unique index: measured, once a cycling sequence
wraps, the next insert fails as a duplicate key, so on a key a cycle only
trades "reached maximum value" for a later and less clear failure. A unique
index whose key is an expression naming the column, such as `(id + 0)`, is
sometimes a key too. Measured on 18: under `(id + 0)` the wrap fails as a
duplicate key, and under `NULLIF(id, id)`, which is always NULL and so never
a duplicate under the default `NULLS DISTINCT`, it does not. Which one an
expression is takes reading it, and the model does not parse expressions.
So such an index refuses nothing: `validate` notes it as not checked, naming
the column and the index. The expression names the column by the rules a
stored body is read with (literals and comments are not names). Refusing it
would refuse a valid plan (#2014, maintainer's choice on its review).
On a column under no key it is accepted. SQL Server's `IDENTITY` has only a seed
and an increment, and refuses every option by name.

**Changed in place.** A create, an added column and a source put on write
the options into the `CREATE SEQUENCE` or the identity clause. A source
that cycles, continued past its bound by the values or the old counter,
starts at its other bound, as `nextval` would, rather than having `setval`
refused; nothing it reaches again can collide, since a cycle is never under
a key. The continuation is computed in `numeric`, so a step past
the end of `bigint` is a number to wrap, not an overflow. A retune
writes only the options that change, a returning one as `NO MINVALUE`, `NO
MAXVALUE`, `NO CYCLE` or `CACHE 1`, which measured reset each to the
engine's own; a retune of start or increment alone reads as before. No new
change kind or pass: options travel on `CreateTable`, `AddColumn` and
`AlterColumnNumbering` (#1776). One rank: a kept identity on a column this
plan narrows is retuned first in class 9, before the retype. Measured on 16
and 18, the engine moves an identity's sequence with its column and refuses
the retype while a bound set for the wider type stands; an identity's
column is an integer, so a retype that is not safe narrows, and bounds
declared for the narrower type fit the wider. A widening keeps the retype
first, where the old bounds fit (ORDERING.md).

**Gated** (maintainer, 2026-10-14) as `numbering` only where a writer
working today fails sooner: with a non-cycling destination, the bound the
sequence counts toward set or lowered (raised, counting down), or its cycle
turned off. A tightened bound with a cycling destination only wraps sooner
and is not gated. A bound raised or removed, the other bound, a cycle turned
on, and a cache changed are not gated either: none can fail an insert that
succeeds now. The counter probe below still checks a bound moved while
cycling, since the engine refuses one its counter is already past.

**Probed.** Measured, the engine refuses a bound the counter is already
past (`RESTART value (10) cannot be greater than MAXVALUE (5)`), mid-apply.
The pre-flight counts a kept source whose bounds move with its counter
outside them. The counter the engine checks is the stored `last_value`,
called or not: `setval(…, false)`, which a source put on by this tool
leaves, moves it with no call, so `pg_sequence_last_value` (NULL until
called) and the start are both wrong readings. The probe reads the sequence
itself through `query_to_xml`, as the other counts do, and one this session
may not `SELECT` counts as in the way (DECISIONS 335). A source put on now
continues up to its declared bound, not the type's, and the values probe
counts against that; one that cycles has nothing in its way.
Both read the counter or the values before apply, which no earlier change
in a plan moves: a column has one numbering change, and no statement a plan
runs before it calls the sequence.

Plan format 40, state 24 and schema set 46 carry the options; an older
build would read them and drop them.

Pinned by `sequence_options_round_trip_and_a_counter_past_a_new_bound_is_counted`
(live, 16 and 18: read back, created, put on and retuned, the gate, the
counter probe, an unreadable counter, the engine's refusal, the next plan
empty), `what_the_model_cannot_hold_is_named_and_never_silently_dropped`,
`sequence_options_the_engine_refuses_or_reads_back_otherwise_are_refused`,
`a_cycling_sequence_under_a_key_is_refused_naming_the_key`,
`a_retune_is_gated_only_where_its_numbers_end_sooner`,
`sequence_options_are_created_and_retuned_only_where_they_change`,
`sequence_options_round_trip_and_an_identity_keeps_its_pair_without_them`,
`an_identity_holds_its_sequences_bounds_cycle_and_cache` and
`an_identity_with_sequence_options_is_refused_on_sql_server`.

<a id="dec-2217-1"></a>

**DEC-2217.1. A partitioned parent's owned sequence is held as a serial when
its other callers are only same-column, same-text partition defaults.**
(#2217; extends DEC-1631.1)

Measured on PostgreSQL 16 and 18: `CREATE TABLE p (id bigserial, v integer)
PARTITION BY RANGE (v)` owns one sequence. Each `CREATE TABLE ... PARTITION
OF p` copies the parent's default into its own `pg_attrdef`, with its own
normal dependency on that sequence. Inserts through the parent and directly
into a partition created this way number from the same sequence. Attaching
an ordinary table whose column has no default does not give it one: inserts
through the parent still number, but direct inserts into that partition fail
its NOT NULL. `ALTER TABLE p ADD COLUMN n serial` gives every partition the
new default, including that attached table.

The catalog's serial-user count excludes only defaults on partitions in the
owning parent's tree, for the same column name and with exactly the parent's
`pg_get_expr` text. Every other DEC-1631.1 shape check stays. A shared call
from an ordinary table, another column, or different text still counts; a
partition's default on another sequence stays a limitation. Matching copies
stay the parent's under DEC-1578.1 and produce no limitation: otherwise
DEC-2072.1 would leave the whole tree out of a pull. A partition declares no
serial or columns of its own. Validation admits a serial on a partitioned
parent; creation uses the ordinary table's sequence-before-table and
`OWNED BY`-after-table statement, before the engine copies the default into
its partitions. No model or saved-format change is needed.

A partition may override that serial's default with an ordinary default of
its own, held on `PartitionOf::columns` under DEC-1578.1. Measured on 16.15
and 18.6, both `PARTITION OF p (id DEFAULT 0)` at create and `ALTER TABLE p1
ALTER COLUMN id SET DEFAULT 0` afterward are accepted. A direct insert takes
0 without advancing the parent's sequence; an insert through the parent
still takes its next sequence value, even when routed to that partition.
Validation therefore allows an own default under a serial parent, and the
partition emitter's existing post-create `SET DEFAULT` recreates it. The
identity refusal stays: on 18 that post-create `SET DEFAULT` is refused for
an identity column; an inline default is accepted but direct inserts still
take the identity's value instead. A serial's ordinary default has neither
restriction.

This is #1893 part A, read and create. Part B (#2218) leaves attaching a
table without the parent's serial default for its own change. DEC-2219.1
extends this to adding or dropping a serial column on an existing
partitioned parent, retuning it, and changing between a serial and no source
or a plain default. Identity changes retain the #1681 refusal.

Pinned by `a_partitioned_parents_serial_holds_only_its_same_column_same_text_copies`,
`a_serial_that_is_nullable_defaulted_on_a_partition_or_misnamed_is_refused`,
`a_created_partitioned_parent_owns_its_serial_and_its_partition_owns_none`,
`a_standing_parents_serial_changes_plan_but_identity_conversions_are_refused`,
`a_partitions_own_default_can_override_a_serial_but_not_an_identity`
and, live on 16 and 18,
`a_partitioned_parents_serial_round_trips_without_partition_drift` (SQL-created
tree, connected create into an empty database, empty next plan, byte-identical
re-pull, numbered inserts, a partition's own non-sequence default used only
by direct inserts, shared and differing-default limitations, and the
measured attach/add behavior). Restoring the old reader restriction makes
the live test fail.

<a id="dec-2218-1"></a>

**DEC-2218.1. Attaching a table without its serial parent's default restores
the same fallback as a plain-default parent.** (#2218; #1893 part B)

Measured on PostgreSQL 16 and 18: `PARTITION OF` copies a `bigserial`
parent's `nextval` default, but `ATTACH PARTITION` of an ordinary table
without a default does not. Inserts through the parent still number from
its sequence; direct inserts into the attached partition fail NOT NULL.
So pbps's attach sets that missing copy, unless the partition declares an
own default. Removing an own default later restores the same fallback.
An identity keeps its existing rules, and a table owning a sequence of its
own is not treated as an unnumbered table to attach.

The differ asks the dialect for an owned sequence's default. PostgreSQL's
emitter computes the name through `owned_sequence` and `generated_name`,
including truncation or an explicit `sequence: {name: ...}`, and quotes the
qualified sequence in `pg_catalog.nextval(...::regclass)`. The concrete
expression travels in the existing `SetPartitionDefault::fallback` string,
as a plain default does. Neither the Change shape nor any saved format
changes; the dialect-neutral differ computes no PostgreSQL name or SQL.

Pull keeps DEC-1578.1's tree predicate unchanged. A live partition without
its parent's default leaves the whole tree out, naming the parent and
every partition, for serial and plain-default parents alike. External
partitions remain unmanaged under DEC-1533.1: pbps sets nothing on them.
The reader applies that same predicate before external handover, so
`pull --partitions-external` naming either missing-copy parent refuses it
as no partitioned parent by that name that pull keeps. External does not
bypass the existing plain-default limitation.

Pinned by
`an_attached_partition_takes_its_serial_parents_default_unless_it_declares_its_own`
(dialect-supplied attach and restored fallback, own-default and identity
negatives),
`a_partitions_serial_fallback_calls_its_parents_generated_or_explicit_sequence`
(generated, truncated and explicit names, quoting, own-default precedence
and default removal), and, live on 16 and 18,
`an_attached_partition_takes_its_serial_parents_default_on_both_servers`
(managed parent creation, SQL-created table attached by a plan, numbered
direct inserts, empty next plan, clean verify, own-default removal, and
whole-tree pull and external refusals for serial and plain defaults).

<a id="dec-2219-1"></a>

**DEC-2219.1. A standing partitioned parent's serial changes recurse into
all its partitions, and declared own defaults are restored afterward.**
(#2219; #1893 part C; extends DEC-2217.1)

Measured on PostgreSQL 16 and 18 with populated partitions, one created by
`PARTITION OF` and one ordinary table attached afterward: `ADD COLUMN n
bigserial` gives both the parent's `nextval` default and numbers their
existing rows. Retuning that sequence with `MAXVALUE 1000` keeps numbering.
The parent's `DROP DEFAULT` and `SET DEFAULT nextval(...)` recurse into both
partitions, overwriting an own default (DEC-1578.1). `DROP COLUMN` takes the
owned sequence with it. Identity inheritance differs: adding an identity
after giving up a serial reaches the partitions on 18, restarting below
existing values, but does not reach them on 16, where direct inserts fail
NOT NULL.

The differ therefore admits adding a serial column, retuning its options,
giving it up for no source or a plain default, putting it on a column with
no source or a plain default, and dropping its column. The existing
`AddColumn`, `AlterColumnNumbering`, `AlterColumnDefault` and `DropColumn`
carry these changes; there is no new change kind, sort class or reordering
pass. A source put on retains DEC-1889.1's locked continuation past the
column's MAX (MIN counting down), whose parent read includes every
partition's rows. Engine recursion covers external partitions as well
(DEC-1533.1); pbps declares and changes none of their own defaults. The
drop-blocker probe likewise counts the defaults that recursive `DROP COLUMN`
or a serial's `DROP DEFAULT` removes before `DROP SEQUENCE`, matching
partition columns by name rather than attribute number. Dependencies outside
the tree still block the drop.

`diff_partition_columns` restores a declared partition's own default after
the parent's change, including a new serial column and numbering changes.
The existing parent-column classification includes numbering changes, both
for that ordering and for the unchanged #1692 refusal of a parent's column
change beside an attach, detach or partition drop. The existing later rank
for a partition's own default follows numbering changes as it follows plain
default changes. A serial on a partition itself stays refused. Every
identity change on a partitioned table or partition, including either
direction of serial/identity conversion, retains
`NumberingChangeOnPartitionedTable`, naming #1681. No producer of
`SerialChangeOnPartitionedParent` remains, so that variant is removed.

Tests: `a_standing_parents_serial_changes_restore_its_partitions_own_default`
covers every admitted change, external parents, restored own defaults,
unchanged declarations, identity conversions and the attach/detach/drop
same-plan negatives.
`a_standing_parents_serial_changes_plan_but_identity_conversions_are_refused`
covers the PostgreSQL plans and named conversion refusals. Live on 16 and
18, `a_standing_parents_serial_changes_reach_every_partition_on_both_servers`
applies every change to declared, plan-attached and external partitions,
re-plans to nothing and verifies cleanly after each apply, checks direct and
parent-routed writers, restores own defaults, continues beyond rows in the
attached and external partitions, confirms source and column removal, and
refuses an identity conversion by name and a sequence dependency outside
the partition tree.

<a id="dec-1172-3"></a>

**DEC-1172.3. A row count filtered by active row security is unchecked.**
(#1172 PR C.) PostgreSQL counting probes ask `row_security_active` in the
executing session for every relation they read; a foreign-key count asks both
ends. An active filter returns a NULL count, which the existing pre-flight
reader reports as unchecked, never passed. The count SQL stays text until the
guard permits it: a plain CASE subquery is still planned and can constant-fold
an immutable policy function even when its count is skipped. Measured on both
versions, the plain form advanced a sequence from such a function; delayed
compilation through query_to_xml did not. Catalog compatibility questions
remain checkable. Existing hidden-child delete refusals remain stronger than
this fallback. pbps never sets `row_security = off` or changes principal to
make a count look complete. Measured on PostgreSQL 16.15 and 18.6: a hidden
NULL gave a zero count, while SET NOT NULL still refused it; a hidden primary
key duplicate still refused INSERT. Owner bypass, FORCE and NOINHERIT owner
membership make the session's effective answer necessary, rather than the
catalog's enabled flag. The engine enforces constraints transactionally when
pre-flight cannot prove them (SPEC §7.5).

<a id="dec-1523-1"></a>

**DEC-1523.1. PostgreSQL columns reference existing enums, domains and standalone composites, scalar or array.** pbps manages no type lifecycle and refuses retypes, row data and partition keys over references. Measured on 16 and 18, ADD COLUMN evaluates domain CHECKs on NULL but no attributes, elements or bounds of a NULL container. Its gate judges potential input code and requires trusted owners throughout the path, including code-free enums; `trusted_roles` extends owner trust only (#2046).

<a id="dec-1523-4"></a>

**DEC-1523.4. Added reference columns are checked regardless of row presence.** Measured on 16 and 18, ADD COLUMN evaluates a non-volatile domain default even on an empty table and stores its value in `attmissingval`. Planning, apply, staged steps and resume therefore validate the input path without an emptiness exemption. An empty table whose added column has no default anywhere would run no type code, but the gate refuses it anyway for one rule independent of row presence; `trusted_roles` admits an untrusted owner when the input code is built in.


<a id="dec-2181-1"></a>

**DEC-2181.1. Eligibility belongs to every referenced type's potential input path.** Check construction and the declared type's own DEFAULT for every reference in the baseline, desired schema and carried change types, including inherited partition columns. This covers every change kind, probes, rewrites, generated-expression changes, rebuilds, staged execution and settle without a per-kind execution list. On PostgreSQL 16 and 18 fast defaults evaluate even without rows; PostgreSQL 18 also evaluates domain CHECKs when a generated expression is replaced. Fill shape, row presence and a no-op plan grant no exemption; code-free paths still require trusted owners (DEC-2181.3). Apply requires each recorded baseline reference's pin even when its change carries no type; an omitted pin list cannot erase that reference. `bootstrap --sql` refuses declarations containing references, connected or offline: generation cannot certify the replay target; use connected bootstrap or target-specific plan/apply. This deliberately refuses safe NULL and empty-container cases unless their potential path is eligible; #2155 still classifies fills for default erasure, not admission.

Pinned by `potential_type_paths_are_checked_even_without_an_added_column`, `an_existing_reference_cannot_be_unpinned_from_a_saved_default_change`, `bootstrap_scripts_refuse_type_references_on_postgres_16`, `bootstrap_scripts_refuse_type_references_on_postgres_18` and the live type-reference regressions on 16 and 18.

<a id="dec-2181-4"></a>

**DEC-2181.4. An input-path node needs an execution proof as well as a known layout.** The semantic node match refuses by default. Measured on 16 and 18, I/O coercions reconstruct the target's nested domains, while stored typed constants and relabels do not; array coercions execute their explicit element expression. PostgreSQL 18 SQL/JSON return coercions also run domain checks without an explicit coercion child and remain unreadable until qualified.

Pinned by `io_coercions_execute_nested_checks_and_require_trust_on_postgres_16`, `io_coercions_execute_nested_checks_and_require_trust_on_postgres_18`, `json_return_coercion_runs_a_domain_check_and_is_unreadable_on_postgres_18` and `a_known_node_layout_without_an_execution_proof_is_refused`.

<a id="dec-1898-1"></a>

**DEC-1898.1. A serial's explicitly declared sequence name is a fixed
relation claim in its schema, and an omitted name claims nothing offline.**
Amends DEC-1631.1's namespace implementation: two names created by
`CREATE SEQUENCE` cannot be treated as two implicit first choices that the
engine resolves by suffixing the later one.

Measured on PostgreSQL 16.15 and 18.6: `CREATE SEQUENCE` fails with `42P07`,
`relation "..." already exists`, when its name belongs to another sequence,
a table, a view, an index, or the index backing a named primary key or unique
constraint. The same sequence name in two schemas succeeds. With serial-owned
sequences already taking `t_id_seq` and `t_id_seq1`, creating an identity on
`t.id` instead chooses `t_id_seq2`: its implicit name and fallbacks yield to
existing relations.

`check_index_names_with_sources` claims each explicit `sequence: {name: ...}`
against tables, views, declared indexes, constraint-backed indexes and other
explicit sequence names in that schema, naming both owners on refusal. It
also retains sources beside a rejected column for diagnostic validation.
These fixed names are outside the implicit claimant list: an identity's
first choice or fallback may equal one without refusing the declaration,
while fallback reach still accounts for a fixed name already being taken.
The existing checks for other declared relations meeting generated names
remain in force. Connected `plan --db` already runs the full declaration
check through `deploy::refuse_invalid_declarations`, `declaration_problems`
and `check_index_names_with_sources`. Offline `plan` previously ran none of
these relation-namespace checks. It now calls only `check_fixed_sequence_names`
before resolving identities or writing artifacts, so this change adds only
fixed sequence-name refusals there. Ordinary index collisions remain
previewable offline and refused by connected planning. Both entry points
share the fixed-claim implementation and report the same text once.

An unnamed serial stays unclaimed offline. Renaming its table or column
keeps its old sequence name, so generating a new name from the declaration
would claim a name that may belong to no sequence. A declared serial name
equal to another serial's generated name, including its 63-byte truncation,
therefore remains accepted here. The generated-name collision test from
this issue belongs to #1899's connected check, which knows which columns
the plan actually creates; it is deliberately not implemented offline.

Tests: `declared_serial_sequence_names_are_unique_per_schema_and_name_both_columns`
covers same-table and cross-table duplicates, with different schemas,
different case and omitted names as negatives.
`declared_serial_sequence_names_collide_with_each_declared_relation_kind`
covers tables, views, indexes and both constraint-backed indexes, with checks
and an unnamed serial's generated spelling as negatives.
`diagnostic_serial_sequence_claims_keep_rejected_column_sources` covers
diagnostic recovery. `a_declared_serial_sequence_can_take_an_identitys_first_choice_or_fallback`
covers implicit first choices and fallbacks, plus an unnamed serial's
truncated spelling and a declared table still blocking an identity fallback.
Live on both supported servers,
`duplicate_declared_serial_sequence_names_are_refused_before_planning_on_both_servers`
requires offline and connected `plan` to name both columns, write no plan or
SQL artifact and create no relation, with the same single diagnostic as
`validate`; distinct explicit names still plan. It also keeps ordinary
duplicate indexes previewable offline and refused by connected planning.
`fixed_sequence_checks_exclude_other_collisions_and_share_each_refusal_once`
covers mixed index/sequence collisions and identical, unduplicated diagnostics.
`two_tables_with_one_index_name_are_refused` keeps the offline-preview boundary
covered without a live server.

<a id="dec-1899-1"></a>

**DEC-1899.1. A connected plan claims every serial sequence it creates or
renames at its exact emitted name, without fallback.** An unnamed serial
claims nothing in the declarations (DEC-1631.1, DEC-1898.1), since a kept
column can still own a sequence named before its table or column was
renamed. A `CreateTable` or `AddColumn`, however, always emits a new
`CREATE SEQUENCE`, so its name is known and fixed.

Measured on PostgreSQL 16 and 18: both `CREATE SEQUENCE` and
`ALTER SEQUENCE ... RENAME TO` fail with `42P07` when a sequence, table or
index holds the target name. A table rename followed by a column rename
leaves the owned sequence's name unchanged. Native `serial` creation agrees
with the emitter's generated name at the 63-byte limit, including UTF-8
character boundaries. Once a held target is freed, the same serial creation,
sequence installation or rename plans and applies; a differently named
loose sequence and an explicit alternative sequence name also succeed.

`sequence_names_claimed` uses the existing PostgreSQL
`Dialect::owned_sequence_name`, which calls `emit::owned_sequence`: an
explicit name or the generated name, in the change's table schema, using
the table and column names at that step. `AlterColumnNumbering` claims a
sequence it puts on, including a conversion from identity, and a rename
whose old and new sequence names are both known and differ. A retune, an
omitted name on either side of a kept sequence, a removal and a conversion
to identity claim no fixed sequence target, matching the emitter.

The connected inventory asks for these names among sequences, indexes and
composite types, and separately among tables and views, managed or not.
The namespace replay checks each fixed claim against the occupants still
present and names earlier steps created, naming the sequence and its column.
Later table and index creations meet an earlier sequence claim too. Drops
and moves that already release a name in the replay keep releasing it
before the claim is checked. No new change kind, sort class or reordering
pass is introduced.

The callers retain distinct responsibilities:

- `created_relation_names` combines `relation_names_created` with fixed
  sequence claims for `engine::refuse_created_name_occupants`'s `asked`
  inventory. The existing rebuilt-view exclusion still applies.
- The identity-release loop in `order_carried_moves` keeps using
  `relation_names_created` without sequences: adding them would introduce
  new explicit identity handover edges beyond DEC-1986.1's existing callers.
- The detach effect-closure selection in `order_carried_moves` also keeps
  the existing relation creations; numbering changes and added serial
  columns do not become new participants in that ordering rule.
- `name_clashes` retains those relation creations and separately replays
  fixed sequence claims, including collisions within one plan. Its result
  remains the existing ordering pass's refusal evidence, not a new edge rule.
- The `DetachPartition`-filtered `at_names` call in
  `engine::refuse_created_name_occupants` keeps its existing clone targets.
  A separate extension adds fixed sequence targets to `at_names`, so a
  loose table or view at a sequence target is read even when no schema
  move or detach occurs.

Tests: `serial_claims_use_the_emitted_name_and_leave_ordering_names_unchanged`
compares create/add claims with emitted SQL, including long ASCII and UTF-8
names and renamed spellings; identity is a negative.
`fixed_sequence_claims_meet_catalog_and_planned_relations_after_releases`
covers create/add, installation, identity conversion and rename targets,
catalog and planned occupants, both creation orders, releases, different
schemas, explicit alternatives and kept-source negatives.
`connected_serial_sequence_claims_refuse_occupants_before_ddl_on_both_servers`
measures both servers, requires create/add refusals against loose sequences
and declared generated-name tables before artifacts or DDL, checks numbering
rename/installation against a loose table, and applies the free-name,
alternative-name and same-plan release cases.

<a id="dec-1635-1"></a>

**DEC-1635.1. Enums and domains are shared declared objects with type files and identities.**
A column's qualified `TypeRef` remains the reference spelling; membership in
`Schema.types` distinguishes a managed definition from an external reference.
`ManagedType` holds an enum's ordered values or a domain's column-style base,
NOT NULL, default, named checks and explicit non-base collation. Collations
share qualified identifier parsing with references but retain their own name
type: an underscore prefix is legal for a collation and reserved on a type
reference. Names belong to the containing map. `schema/<schema>.<name>.type.yml` follows the module
file's leading-key rule and the ordinary scalar formatter. The optional
`types` map adds `y_` uids under identity-file version 1, as `roles` did.
State 30 records the definitions; states 6–29 remain readable. Saved plan 48 carries the `types` map in its identity file. Published schema
set 54 adds the two declaration shapes.

PostgreSQL's catalog batch reuses `user_types`' label and constraint readers
inside its existing statement snapshot, excluding extension members and engine
schemas. Definition state excludes OIDs, owners and constraint validation;
the existing reference pin retains those facts. A domain's inherited base
collation is absent, an override is qualified. Per-type definition decode
failures are named type limitations, leaving unrelated objects readable while
a declared counterpart is refused as unrepresentable rather than absent.
Undeclared type limitations remain in the unreadable catalog inventory for connected commands' unmanaged policy, while declared ones belong only to managed limitations.
A catalog base outside the column type catalog, such as `int4range` or
`int4multirange`, is preserved as a
qualified reference, including its array marker. This does not admit range
columns or add range definitions to the model. SQL Server refuses declarations
by name. Composite definitions and ranges stay outside the managed model;
standalone composite references remain admitted, range references refused.
#1523 already lifted the enum/domain column omissions in the catalog and pure
assembler: slice one preserves that admission and adds the type files beside
the tables. Pull preserves existing type identities across recreation. Type
files use the same encoding and pre-write case-collision refusal as other
declarations (DECISIONS 135); diagnostics retain both original file spellings.

Declared definitions now supply checksum-pinned evidence across plan, apply,
staged execution, resume and bootstrap, with bound domain code vetted inside
the applying transaction; see DEC-1635.4. External reference rules are unchanged.

Tests: `declared_types_round_trip_ordered_labels_and_verbatim_domain_text`,
`invalid_type_shapes_and_duplicate_labels_are_refused`,
`two_type_files_cannot_declare_the_same_identity`,
`type_ids_are_optional_under_version_one_and_validate_their_own_namespace`,
`collation_names_keep_sql_identity_without_type_only_restrictions`,
`type_declaration_schema_and_loader_agree_on_kinds_and_unique_labels`,
`missing_type_inventory_is_not_an_empty_catalog`,
`domain_bases_outside_the_column_catalog_remain_qualified_references`,
`a_sql_server_project_refuses_a_declared_type_by_name`, and
`managed_types_and_their_columns_survive_pull_recreation_and_repull` cover the
format, compatibility, negative shapes and catalog projection on 16 and 18.
`existing_enum_domain_and_composite_columns_round_trip_as_scalars_and_arrays`
keeps distinct quoted type identities and pins when one is managed and one is
external. `type_files_differing_only_in_case_refuse_pull_before_any_project_write`
checks DECISIONS 135 on both servers, preserving an empty or existing project.

<a id="dec-1635-2"></a>

**DEC-1635.2. Slice one refuses every managed type difference before emitting a plan.**
A definition added, removed, replaced in kind, or changed in any attribute is
a named refusal saying that lifecycle support arrives in later slices of
#1635. No `Change`, rank, reordering pass or type DDL is introduced. Connected
planning checks type definitions before reference prerequisites, so a missing
declared enum does not become merely a missing column-type error. An equal
definition plans no operation. A retained type uid without a declaration also
refuses: a manual drop followed by a baseline must not let an empty plan
record an identity the database does not hold. A recorded type missing from a
pull's read is a named blocker before any project write, with a catalog
limitation reported as unrepresentable and its reason rather than as absence.
Newly managed live types enter the pinned baseline by their actual definition;
already recorded ones still participate
in drift and read-back movement checks. An undeclared, unrecorded live type
stays unmanaged under the existing ignore/warn/error policy. Offline planning
can compare a git/file baseline, but cannot obtain live proof; a declared type
absent from that baseline is refused, and reference prerequisite restrictions
remain unchanged.

Tests in slice one: `every_type_lifecycle_difference_refuses_a_plan_by_name` covered enum
addition, removal, reorder and rename, domain base, collation, NOT NULL,
default and named-check differences, kind replacement, missing and removed
definitions, and equal-definition negatives.
`type_scope_leaves_unknown_types_to_the_unmanaged_policy` pins ownership scope.
`an_empty_plan_cannot_keep_an_undeclared_type_identity` covers a retained uid
after both the declaration and live definition have disappeared.
`pulled_type_files_converge_and_each_type_change_is_refused_without_sql` runs
pull/recreate/re-pull, equal plans, first-time type adoption through an
unrelated staged column change, and each refusal through the CLI on 16 and
18, requiring that no rejected plan artifact is written.

Lifecycle refusals are superseded by DEC-1635.3. The retained attribute refusals
are now pinned by `type_attribute_changes_remain_refused_by_name` and
`pulled_type_files_converge_and_type_attribute_changes_are_refused_without_sql`.


<a id="dec-1635-3"></a>

**DEC-1635.3. Whole declared types use uid identity, ordinary drop intent and two overlapping PostgreSQL name namespaces.**
CreateType creates an enum or domain, DropType removes it without CASCADE, and
RenameType preserves the uid and dependent columns, including SET SCHEMA before
RENAME. Explicit annotations or recorded intent supply identity. Formatting
retains a pending renamed_from annotation and strips it only after identity
absorption, as for tables and roles. A type drop
requires the same nonblank reason, tombstone and destructive risk approval as a
table drop. Swaps refuse by name. Existing enum label, domain attribute, base
and collation differences still refuse by name with their slice 3/4 boundary.
An unrecorded, undeclared type remains unmanaged. Offline git/file comparison
can preview lifecycle changes and mint identities, but has no target evidence
and cannot make an applicable plan.

Measured on PostgreSQL 16.15 and 18.6, `pg_class.reltype <> 0` is:

| relkind | row type |
|---|---|
| table r, partitioned table p, view v, materialized view m, composite c | yes |
| index i, sequence S | no |

CREATE TYPE beside a same-named index or sequence succeeds; beside a table it
fails. Therefore a type cannot share Relations(schema), which already contains
indexes and sequences. A new Types(schema) slot holds declared types; tables
and views hold both slots, while indexes/sequences hold only Relations.
Catalog composites occupy Types. Live pg_type occupants are checked even when
pg_class has no counterpart; generated array names retain #1503's behavior.
The existing slot machinery orders release/claim handovers, including a dropped
table followed by a same-named type, and checks intermediate move names.

Creation is class 7 before tables, ranked by domain-base depth. Rename is
class 1 with domain-base depth rank; drop is class 18 after defaults, checks and modules, ranked by reverse
baseline domain-base depth so dependent domains disappear first. Typed columns and
domains require their declared type's final incarnation. Drops require the
last dependent table/column/domain to disappear, or the column's final retype.
Those effects and name-slot edges join the existing schedule and shadow
verifier. Type handovers join after_the_rebuilds, including a class-18 type drop
before a class-7 table; type changes do not inherit the old table-less blanket
dependency on every table. No new ordering pass is introduced. ORDERING.md
records all pairs.

Tests: `type_identity_drives_creation_removal_and_rename_while_swaps_refuse`,
`fmt_keeps_a_pending_type_rename_and_removes_only_absorbed_intent`,
`a_created_type_precedes_its_table_in_the_schedule_and_shadow_verifier`,
`a_dropped_type_follows_its_last_table_in_the_schedule_and_shadow_verifier`,
`domain_bases_are_created_first_and_dropped_last_even_when_names_sort_the_other_way`,
`a_type_meets_table_and_view_row_types_but_not_indexes_or_sequences`,
`a_table_to_type_handoff_is_an_existing_slot_edge_and_the_reverse_is_rejected`,
`pg_type_names_refuse_row_types_and_admit_indexes_and_sequences_before_emission`,
and `type_drops_follow_the_last_table_and_table_names_can_handoff_to_types`
cover emission-independent names, both verifier orders and real 16/18 plans.

<a id="dec-1635-4"></a>

**DEC-1635.4. Declared definitions are approved evidence; a domain's bound code is vetted in its creating transaction.**
A saved plan carries each desired type's uid and definition, and the actual
before-definition/name when it exists. The checksum pins this evidence; rename
follows the uid. External references keep DEC-2181's live OID/definition/routine
pins and every existing negative case. Existing declared types keep the same
owner rule, and a newly created type is naturally owned by the deployer; no
owner-trust exception is added. Saved plan 49, state 32 (declared type expression
provenance), and schema set 56 carry these additions.

Plan seals declarations without creating scratch objects or scanning SQL for
code. An enum executes no code. Human and JSON output explicitly defer domain
checks/defaults to apply. Before statements, existing declarations must equal
the recorded before-definition, and new names must be absent after any planned
slot predecessor releases them. Right after each CREATE/ALTER, apply re-reads
the live type in the same transaction. Enum labels and their order compare
exactly. A domain's normalized base type, collation, NOT NULL and CHECK-name
set compare exactly; the default and each check expression compare by presence,
not text, following SPEC §7.6's read-back rule. The statement that ran is the
checksum-pinned plan's text, and another session cannot replace this uncommitted
creation. Security is checked on what was actually bound: the existing
input-path reader vets conbin/typdefaultbin with its built-in-only decoder.
The catalog's deparsed spelling is retained in snapshots and the original in
Declared provenance, as for table expressions. Before-statement drift compares
catalog evidence against the recorded catalog evidence, including expression
text; expression presence is only the declared-versus-read-back comparison.

Domain convergence follows decisions 207 and 208: `Declared.types`, keyed by
qualified type name, retains the applied default and named CHECK texts.
CreateType records its declaration, RenameType moves an existing record, and
DropType forgets it. Rename never invents provenance for an adopted domain.
Bootstrap records authored text; pull/baseline adopt the engine's text without
claiming it was authored. `Declared::overlay` restores only expressions still
present in the verified read-back before type differences are computed. It
does not restore missing defaults/checks or overwrite base, collation, NOT NULL
or names. Layout-only declaration edits remain differences (domain edits are
DEC-1635.8's). The checksum compares unmodified catalog readings
before overlay, so real CHECK/default drift cannot be hidden by provenance.
State 31 adds this record to state 30; older readable states have no domain
record and fall back to engine text, with no migration.

The existing built-in-only decoder and owner check are reused, not copied.
A domain calling a non-built-in function refuses at apply and rolls back the
whole transaction, before dependent statements. This accepted limitation is
explicit; scratch-based plan-time vetting belongs to #2309. Staged type DDL
always opens a transaction and vets before commit; checkpoints record its uid
and current name. Resume checks the checkpoint and re-vets the already-created
type. Immediate bootstrap uses the same executor and trusted roles. Offline
bootstrap SQL admits enums and code-free domains but retains DEC-2181.1's
refusal for domains with checks/defaults, since there is no connection to vet
their bound code. Slices 3/4 can reuse the statement metadata and declared pins.

Tests: `declared_evidence_follows_the_uid_and_records_absence_without_a_catalog_pin`,
`readback_expressions_compare_by_presence_while_structural_fields_compare_exactly`,
`declared_type_bootstrap_scripts_admit_enums_and_refuse_unvetted_domain_code`,
`staged_type_creation_vets_before_commit_and_resume_accepts_a_vetted_step`,
and `declared_type_lifecycles_converge_and_vetting_is_atomic` pin checksum
identity, missing versus existing evidence, full enum/domain/table creation,
convergence, rename/data preservation, before-definition drift and transactional
refusal on PostgreSQL 16/18. DEC-2181's external-reference tests retain their
existing assertions.

Tests: `domain_expression_records_follow_creation_rename_and_drop_without_adopting_text`,
`domain_overlay_restores_only_existing_expressions_and_preserves_catalog_structure`,
`a_version_30_state_keeps_catalog_domain_text_without_a_declaration_record`,
`domain_expression_records_converge_without_masking_layout_changes_or_live_drift`,
and `adopted_domains_and_older_states_keep_engine_expression_text` pin the
declaration record, serialized round-trip and old-state fallback. On both
PostgreSQL 16 and 18, handwritten `VALUE > 0`, `VALUE IN ('a','b')`, and default
`'a'` converge after apply, layout changes remain differences, rename preserves
the authored spelling, and hand-altered CHECKs refuse as drift. Disabling the
domain overlay makes the convergence test fail before the next plan can seal.


<a id="dec-2321-1"></a>

**DEC-2321.1. Type references resolve through the identity map of their own side.**
`pbps-diff::identity::type_refs::TypeResolver` resolves a base reference through
base ids and a desired reference through declared ids. Its explicit
ExternalOrUnrecorded result carries no managed identity and never means absent.
Both execution engines and the CLI may use diff under the existing crate
boundaries; no model or saved-plan field is added. Domain-base remapping,
retained column comparison, executed pin positions and movement guards share
this resolver. Remapping resolves each source once, so rename-chain output
cannot be mistaken for another original name.

A retained dependent must exist on both sides by its own table or type uid,
and its base reference must resolve to the retiring uid. New tables/domains
resolve on the desired side. Retained columns keep their original binding when
a different uid takes the same spelling; an explicit retype to a different
reference can release it. Column identity follows a recorded column rename.
External pin requirements retain their side-specific classification even if a
lifecycle operation on the other side uses the same name. Connected planning
supplies both uid maps to pin collection; carried before/after operation types
retain their source side, and replay reconstructs those maps from approved pins
and uid-bearing operations. CREATE establishes desired identity even when a
bootstrap or per-statement check has no declared pin bundle; an absent type
cannot supply a catalog pin. Row keys resolve in the desired map because their
comparisons run after structural changes. Kept and renamed identities still
require their declared evidence, and a drop carries its checksum-pinned
before-definition under the retiring uid. A missing declaration cannot borrow another uid's
same-named baseline definition. Required declared pins match the uid or an
explicit drop of that uid, including on no-op and resumed paths. The Types name-slot layer and every
scheduling/handover rule remain unchanged (DEC-1964.2, DEC-1635.3).

A DropType and CreateType of different uids at one name produce the advisory
`plan.type-replacement` finding, warning severity, in connected and offline
plan output. Human output uses stderr; JSON carries it in the findings envelope.
It names the type and both uids, explains that identity history does not carry
over, and suggests keeping the original declaration file and uid, editing its
definition there, and using renamed_from for a rename. It also explains what
changes in place under a kept uid: enum labels (#1635 slice 3) and a domain's
NOT NULL, default and checks (slice 4a, DEC-1635.8); a domain's base type and
collation wait for the rebuild (slice 4b).
Rename alone, drop alone and create alone produce no replacement warning.
This never supplies identity intent (SPEC 14.3).

Unit and PostgreSQL 16/18 tests distinguish new and retained table/domain
references, including arrays, preserve rename chains and intermediate-holder
handoffs, and refuse changed live definitions. Warning tests cover both
report channels and all three warning negatives. Each repaired site is checked
by temporarily reverting its behavior with a copy of the file retained aside.

<a id="dec-1635-6"></a>

**DEC-1635.6. Slice 3a adds and explicitly renames enum labels; reference-data and partition-key admission stays in #2346.**
`AddEnumValue` emits ALTER TYPE ADD VALUE, and `RenameEnumValue` emits
ALTER TYPE RENAME VALUE. A label accepts a string or `{name, renamed_from}`;
one-shot intent stays beside Schema, never in its equality. Adding a label
carries no intrinsic risk, as CreateType does. Renaming carries Rename, as
RenameType does, because existing stored values return a different spelling.
With no base definition for its uid, a type is created with its final labels
and ignores label rename hints, like `identity::intent_is_absorbed`; loader
validation still applies.
Both operations use the existing class-7 type subrank and approved ordinal;
whole-type rename stays class 1. Exists/final-type rewrite effects and the
existing schedule place labels before their dependents; no pass is added.

Renames execute while the target is free. PostgreSQL 16.15 and 18.6 accept a
chain with the last destination freed first and reject a direct swap with
"enum label already exists". As for type names, chains work and cycles or
occupied targets refuse by name. After renames, retained labels keep their
relative order. Additions walk the declared order from left to right: AFTER
the previous desired label, BEFORE the first existing label at the start, or
no neighbour for the first label of an empty enum. Each operation carries its
exact intermediate after-definition for per-statement checks, staging and
resume through the side-specific resolver (DEC-2321.1).

The measured BEGIN/ADD VALUE/use results are the same on PG 16.15 and 18.6:

| Use | enum | enum array | domain over enum | Declaration scope |
|---|---|---|---|---|
| Column default | unsafe use refused | unsafe use refused | unsafe use refused | declarable |
| CHECK | unsafe use refused | unsafe use refused | unsafe use refused | declarable |
| Row insert | unsafe use refused | unsafe use refused | unsafe use refused | not declarable today (DEC-1523.1, #2346) |
| LIST partition bound | unsafe use refused | unsafe use refused | unsafe use refused | not declarable today (DEC-1523.1, #2346) |

Creating a domain with its own default or CHECK over an enum or enum array
also refuses, measured on both versions. Views, SQL string-body functions
and SQL BEGIN ATOMIC functions also refuse
same-transaction evaluation. Only the bodies of PL/pgSQL functions and
procedures are deferred until invocation. Their parameter defaults bind at
creation: both cast and uncast defaults using a newly added label refuse with
unsafe use on PG 16.15 and 18.6, including procedure defaults. A PL/pgSQL body
containing the new label with an existing-label parameter default is accepted.
Planning therefore exempts only the AS string or dollar-quoted body, located
with the existing lexicon; quoted labels elsewhere in the definition remain
subject to the same conservative use check, including DEFAULT and = defaults.
Planning refuses the still-declarable uses: column defaults/CHECKs over the
enum, arrays or domains, a domain's own default/CHECK, and created/altered views
or SQL-bodied routines containing the quoted label, plus function/procedure
parameter defaults even when the language is PL/pgSQL. The named remedy is
"apply the label first, then the rest". The ordinary table validation rejects
`data:` on every user-defined reference or array, and rejects reference-valued
partition keys, before this use check can arise. DEC-1523.1 is unchanged;
admitting those surfaces belongs to #2346, not slice 3a.

PostgreSQL stores enum constants by oid, but pbps does not rewrite SQL text
through a label rename. An updated CHECK or default differs from its recorded
declaration and the ordinary diff lists its rebuild or alteration. DropCheck
(class 2) precedes the label rename (class 7); defaults follow in class 9 and
AddCheck in class 13. These statements succeed in one transaction on PG 16.15
and 18.6: renamed labels, unlike newly added values, are usable immediately.
The cost is explicit: AddCheck scans its table under ACCESS EXCLUSIVE until
commit. An engine-typed, scan-free alternative is tracked in #2354.

Connected spelling probes defer defaults and CHECKs by the resolved identity
of an enum whose label this plan renames, including arrays and domains over
it, without reading or reversing their SQL text. The probe view never changes
the declaration used for diffing or recording. The engine enforces statements
at apply; existing post-statement declared-type vetting and closing readback
checks remain in force. No expression provenance is forwarded or invented.
A retained expression that still uses the old label must surface a named
error rather than silently recording a guessed new spelling.

Unannotated label removal and retained-label reordering stay named refusals
with the #1635 slice 3b remedy. Domain alterations stay slice 4. Saved plan 52
is retained because its checksum-pinned change payload now includes the two
label operations and their intermediate definitions. Published schema set 59
admits the YAML label intent. No row codec, contextual table-validation
exception, speculative type inventory, absent-type value deferral, transient
uid or connected value-probe reversal is introduced by this slice.

<a id="dec-2331-1"></a>

**DEC-2331.1. A type change the engine cannot make by itself is made with a
conversion a person recorded, kept in the identity file and pruned by the tool;
pbps never chooses the cast (#2331).** ADR-0012 section 5 refuses `text` to
`integer`, because a cast pbps chose is a transformation nobody declared, and
left the user a two-step remedy that turns one routine change into several
commits and applies. It said the replacement is a declared transformation, not
a flag. This is it, and it is recorded the way a rename is rather than written
in the declarations, because a conversion is one-time per environment: kept in
the YAML it would have to be deleted by hand once every environment moved.

- **The record** is `conversions` in `schema.ids.json`, keyed by the column's
  uid: `from` and `to` as the dialect spells them, `using` verbatim, and the
  `operator` and `allowed_at` of `move-rows`. It follows the column through a
  rename, is reviewed in the merge request, and is in the plan's checksum twice:
  as the identities the plan carries and as `using` on the change.
- **Supplied** by `pbps convert <column> --using <expr>|- [--from <type>]`:
  offline, `from` read from the declarations at `HEAD` unless named, `to` the
  declared type. `-` reads standard input the way #2332 makes `drop --reason -` do:
  UTF-8, one final line ending removed, blank refused before discovery. With a
  terminal `plan` asks for the expression of each change that needs one; the
  example on the prompt is a hint and a blank answer records nothing. Without
  one, and under `plan --check`, it is the error finding
  `plan.conversion-required` carrying the command. No flag of `plan` or `apply`
  can supply it (SPEC 14.3).
- **Consumed** by the differ, and only for a change of that column that is
  exactly `from` to `to`. An environment already on `to` has no such change and
  never reads the record, so each environment applies it once and nothing marks
  it consumed. An environment on another type that needs an expression to get
  there is refused by name with the command to record one for its start; one
  whose engine converts the change itself ignores the record, so an expression
  written for one source type is never applied to another. A column renamed in
  the same plan is refused, because the expression names the column once. One
  record per column: recording another replaces it.
- **Pruned** when identities are written, by `plan` (and `plan --db`, which
  insists the file already says so) and the intent commands: the column's
  declared type is no longer `to`, or the column is gone. `integer` to `text` to
  `integer` therefore never revives an old expression, and an environment still
  on the old type needs a record for the new pair.
- **Vetted, then evaluated, then applied.** The expression runs as the deploying
  role, so it is judged before anything runs it, in the apply, ahead of the
  pre-flight. It is bound against a temporary table holding only the old column,
  wrapped as `CHECK (((<using>) IS NULL) OR true)`, in a transaction that is
  rolled back. The engine keeps the whole tree unsimplified in `conbin` (the user
  function's `funcid` stays, measured on 16 and 18 by the live test), and
  refuses any name but the column's own as a binding error, so `using` may
  reference only the column and the result cannot depend on another column's
  retype. The tree then goes through DEC-2181.1's decoder: no function, operator
  or cast that a role wrote, none on `RUNS_SQL`, no user-defined or domain type.
  No text scanner (DEC-1635.4). A refusal names the code and the SQLSTATE and
  never the server's text.
- **Applied.** The change is classified `destructive` in addition to whatever
  its pair of types says, through the ordinary `--allow`. The whole expression
  is in the plan, `--sql` and `explain`. The pre-flight evaluates it over every
  stored row, read-only, before the first statement, and a row it cannot convert
  refuses the plan by name with the SQLSTATE and without the server's text,
  which quotes the value; for this probe a failing query is the finding, where
  for every other it is "unchecked". `ALTER COLUMN ... TYPE ... USING` is emitted
  from the record and from nothing else, and the emitter's three refusals (no
  automatic cast, the session zone, an array's elements) do not apply to a
  change that has one. A user-defined type reference still cannot be retyped
  (DEC-1523.1). A column default is taken down and put back around the
  conversion even when its text is unchanged, because the engine cannot cast the
  old default to the result (measured: see the live test).
- **The session zone** (DEC-1916.1) is unchanged. `timestamp` to `timestamptz`
  is allowed with a record; the expression can say `AT TIME ZONE '...'`, and
  without a record the refusal stays, with #1919 open for that case.
- **SQL Server** has no `USING` and converts implicitly, so it records nothing:
  `convert` refuses and a file holding `conversions` is refused by the differ.
  Partition-key columns keep their existing refusal; #2298's rebuild takes the
  expression as its copy projection.

Not decided here and not done: the same conversion for a domain's base type.
The foreign keys on either side of a converted column are DEC-2359.1.

<a id="dec-1635-7"></a>

**DEC-1635.7. Slice 3b removes and reorders enum labels by rebuilding the enum under its uid.**
PostgreSQL has no `DROP VALUE` and no reorder, so a declaration that removes a
label, or changes the relative order of retained ones, plans one
`Change::EnumRebuild` per step instead of refusing (DEC-1635.6 kept its
refusals for this slice). Label renames of the same uid run first as
`RENAME VALUE` on the old type, so every label maps by its final name.
Additions need no step of their own: the replacement is created with the
declared labels.

The sequence, in one transaction or one staged step per statement:

1. `Create`: `CREATE TYPE <tmp> AS ENUM (<declared labels>)`, class 7 after the
   renames. `<tmp>` is `pbps_rebuild_<uid>`, moved on to a suffix when a
   declaration holds the name; a live occupant refuses by name. The Types name
   slot is claimed here and released by the swap.
2. DropCheck, DropForeignKey and DropIndex for what names a retyped column,
   by `Dialect::enum_rebuild_dependents` and the differ's
   `recreate_retyped_dependents` (checks, foreign keys, filtered and
   expression indexes; a partition's own checks and indexes are included),
   and a drop of each default, all from the declaration. Views, SQL bodies and
   triggers over a moved column come down and go back through the existing
   module rebuild (`diff_rebuilding`, `weave`), found by the catalog census.
3. `Retype`: one `ALTER COLUMN .. TYPE <tmp> USING col::text::<tmp>` per
   column, ranked with a retype in class 9 (after the default drops at −2,
   before the restored defaults at 0). A partitioned table is retyped at its
   root once.
4. `Swap`: `DROP TYPE <name>`, `ALTER TYPE <tmp> RENAME TO <name>`, then
   `COMMENT ON TYPE` when the type had one. Class 9 beside the retypes, after
   every retype of its type. The type's comment is read at plan time, carried
   in the plan payload, and written after the rename. The uid in
   `schema.ids.json` never changes.
5. The differ's ordinary AddCheck, AddForeignKey, AddIndex, CreateModule and
   default sets restore everything from the **declaration**, never from catalog
   text; no SQL is rewritten lexically.

`::text::` is the only `USING` pbps writes (ADR-0012 Amendment 5). Each label
maps by name and no value changes.

Measured on PostgreSQL 16.15 and 18.6 (`docs/measurements/enum-rebuild-1635.md`): the retype is refused beside a default, a check, a foreign key between
two enum columns, a partial or expression index, a view or rule, a
`BEGIN ATOMIC` or `RETURN` routine body, a trigger `WHEN` or `UPDATE OF`, a
policy, a generated column reading it, a generated column of the type, a
publication row filter or column list, and a partition's own default; it is
accepted beside plain, unique, primary-key, exclusion, INCLUDE and GIN
indexes, extended statistics, string-bodied SQL routines and PL/pgSQL bodies,
and recurses through partitions and inheritance children. `DROP TYPE` is
refused while a domain, a routine signature, a range type or a composite
attribute uses the type.

| Dependent | Handling |
|---|---|
| default, check, foreign key, partial or expression index | dropped and restored from the declaration |
| view, `BEGIN ATOMIC` or `RETURN` routine, trigger | module rebuild |
| plain index, unique, key, exclusion, statistics, string-bodied routine, PL/pgSQL | the engine |
| policy, generated column, materialized view, rule, publication, composite attribute, domain, routine signature, range, cast, undeclared table | refused by name before any SQL |

Census: every `pg_depend` edge on the type and its array, and every column of
either, is read in the planning transaction and classified. A dependent that is
not in the table above refuses by name, so the engine never aborts mid-plan on
a kind the table does not list. A non-default owner or a non-empty `typacl`
refuses, because the replacement would not keep it. A same-plan table, column
or routine that uses the type, a type renamed in the same plan, and a
partition with its own default for a moved column also refuse by name.

Rows holding a removed label refuse at plan time. Each `Retype` carries the
removed labels as the database stores them, and its probe counts the rows (the
elements, for an array) per table and column; the plan stops with the counts
and nothing runs.

Risk: the new class `type-rebuild`, gated like the others, on every step. The
plan lists each table it rewrites and says each takes an ACCESS EXCLUSIVE lock
and a full rewrite, even for a pure reorder. Cost is not folded into a class
(ADR-0012 §3).

Pins and guards: the create statement certifies no declared type; the swap's
two statements certify the uid at the temporary name and then at its own, as
`SET SCHEMA` and `RENAME` do, so a per-statement check, a staged step or a
resume accepts every intermediate state through the side-specific resolver
(DEC-2321.1). The movement guard allows the temporary type between creation and
swap and requires it absent at the close. Saved plan 53 carries the steps;
published schema set 59 is unchanged (the risk class is a string in every envelope).

Still refused, by name: a domain over the enum (slice 4), a routine signature
using the type (a follow-up issue), `data:` rows and partition keys of the enum
(DEC-1523.1, #2346), and everything the table sends to refusal.

Pull and re-pull keep the live label order; an unchanged type plans nothing.

A dependent module the same plan restates (`AlterModule`) is not handled by that:
restated in place it would run after the retype with the old text still
standing. The census puts it in the rebuilt set like an unchanged one, the plan
is made again, and the differ turns its restatement into a drop before the
retypes and a create from the desired text after the swap; at apply a module
the plan still only restates is refused. A module the plan drops already comes
down in class 0, and a module it creates, or a default, check, key or index it
adds or changes, already runs after the swap (classes 9, 13, 14), so none of
those needed a change. Pinned by
`a_module_the_plan_also_changes_comes_down_before_the_retypes_and_returns_with_its_new_text`
and the live `enum_rebuild_applies_with_*` tests.

The census reads the database before the plan runs, so a table or column the
same plan renames is still under its old name there while the plan's drops of
its default, check and index are written for the names the declarations leave.
Every match of a catalog name against the plan goes through the plan's own
renames first (`Renamed`). A partitioned or inherited column moves with its
root, so what stands on every such column is read, not only on the root's: a
view of a partition's column that shows no enum is still one the retype
aborts on. Pinned by `a_default_the_plan_drops_under_the_names_it_renames_to_is_the_one_the_catalog_has`,
`a_partitions_column_follows_its_root_and_a_plain_table_does_not` and the live
`enum_rebuild_applies_beside_a_table_and_column_rename_*` and
`enum_rebuild_rebuilds_a_view_over_a_partitions_column_*` tests.

A moved column's type is exempt from the checkpoint comparison while the
rebuild runs, since it holds the replacement until the swap names it back. The
exemption is paired with a promise (SPEC §7.6), as every excluded field is:
each `Retype` carries the type the declaration gives the column, and the
closing read holds the column to it, so a session that retypes it in between
leaves the swap finished and the success unrecorded, with the column named.
The restored default, check, key and index are plain `AlterColumnDefault`,
`AddCheck`, `AddForeignKey` and `AddIndex` changes and are held by the
promises and part checks those already carry; a partition's column is its
root's and is moved only with it. Pinned by
`a_rebuilds_retype_promises_its_column_the_final_type_and_no_other_step_promises`,
`a_rebuilds_moved_column_is_held_to_its_final_type_at_the_closing_read` and the
live `enum_rebuild_staged_closing_read_refuses_*` test.

Plan-time probes: each `Retype` is also an event of its column in the preflight
stream (`RebuildEnumColumn`), and the existing retype exemption (`retypes_in`)
covers it. The engine judges a CHECK or an index predicate added beside a
reorder against the replacement's label order, and a stored `happy` fails
`m <= 'ok'` under `[sad, ok, happy]` and passes under `[happy, ok]`, so a probe
over the old order refused a valid plan. The check is skipped as unchecked, as
after any retype, and the engine decides it at apply: a check the stored data
really violates under the new order is refused there, in the apply's
transaction, with the old type standing and no applied state recorded. The event
is not an `AlterColumnType`: the stored labels do not change, so key and
foreign-key probes (equality, order-free) keep running, and the replacement may
carry a name that a rename in the same plan has not yet given the catalog. The
removed-label row count is the `Retype` change's own probe and still runs first,
against the old type. The NOT NULL and unique probes read null-ness and
equality, which the rebuild preserves, and a DEFAULT has no row probe, so none
of them shares the assumption. Pinned by
`an_enum_rebuilds_retype_step_is_an_event_of_its_column_and_no_other_step_is`,
`a_check_beside_an_enum_rebuild_is_not_probed_against_the_old_label_order` and
the live `enum_rebuild_adds_an_order_sensitive_check_*` test.

A column the plan removes is handled, not retyped and not refused: its own
`DropColumn` (class 5) or the `DropTable` of its table (class 6) runs before the
replacement is created (7) and the swap (9), so by the swap nothing of it holds
the old type. The census reads this from the plan's `changes` alone, like
`module_verdict`'s "the plan drops it", through the same `Renamed` names, so
planning and apply, which both pass the same `changes`, give one verdict. A
default on the dropped column or any part of a dropped table goes with it and is
not restored; a check or index on a dropped column is dropped by the plan's own
`DropCheck`/`DropIndex`, which `drops` already reads. A column the plan
retypes away from the enum is still refused: the emitter does not render an
`AlterColumnType` off a user-defined type reference ("retyping a user-defined
type reference is not supported"), and nothing orders such a retype before the
swap, so excusing it would only move the refusal. A partitioned table cannot be
dropped by a plan yet, so a dropped table's partitions reach the census only
through the ancestor walk the unit test pins. Pinned by
`a_column_the_plan_drops_before_the_swap_is_not_a_blocker_and_another_is`,
`a_table_the_plan_drops_takes_its_columns_and_its_partitions_with_it`,
`a_default_check_or_index_goes_with_its_dropped_column_or_table` and the live
`enum_rebuild_drops_a_column_*` and `enum_rebuild_drops_a_table_*` tests.

Census at apply: the same census runs again where the plan runs, in the apply's
own transaction after the lock and before the first statement. A staged run
reads it in a transaction it rolls back, at its start and at every resume (a
run past `DROP TYPE` has only the replacement left and skips it). Nothing is
declared there, so a view, body, trigger, check, key, index, default or any
other dependent that the plan does not already drop and restore appeared after
the plan was made, and is refused by name before the first statement. The live
test `enum_rebuild_apply_refuses_a_dependent_created_after_the_plan_by_name_and_applies_nothing`
plans, creates an undeclared view over the column in another session, and
applies transactionally and staged on 16 and 18: refused, the type untouched.
A refusal inside the transaction is recorded as the failed attempt, like any
other; no applied state follows it.

Risk: restoring a check, key or index asks for `constraint` or `destructive`
beside `type-rebuild`, as it does in any plan that drops and re-adds them;
`--staged` counts the rebuild's steps as one change, so it admits a rebuild
with no companion to restore and refuses one that has any.

Tests: `removing_a_label_rebuilds_under_the_uid_in_the_order_create_retypes_swap`,
`reordering_retained_labels_rebuilds_with_nothing_removed_and_equal_labels_plan_nothing`,
`renames_run_first_and_the_rebuild_maps_by_final_names_and_counts_stored_ones`,
`what_a_rebuild_cannot_do_is_refused_by_name_with_no_step`,
`a_saved_rebuild_replays_against_its_pins_and_no_step_can_certify_itself` and
`the_whole_plan_drops_and_restores_what_names_a_moved_column_and_orders_the_steps`
pin the differ and the saved-plan replay;
`enum_rebuild_steps_write_the_replacement_the_one_using_cast_and_a_certified_swap`,
`a_rebuilds_retype_counts_rows_and_array_elements_holding_a_removed_label`,
`a_rebuilds_swap_is_accepted_at_every_boundary_and_each_drift_or_skip_is_refused`
and the census verdict tests pin emission, the row probe and the guards; the
`enum_rebuild_*` live tests in `managed_types` run the rebuild on PostgreSQL 16
and 18: removal and reorder with comment, defaults, checks, keys, indexes and
partitions; the per-column row counts; each refused dependent; views, bodies and
triggers; a rename in the same plan; and staged apply with resume across the
steps. Each repaired site was checked by temporarily reverting its behavior
with a copy of the file retained aside.

<a id="dec-2359-1"></a>

**DEC-2359.1. A type change that leaves the types a key can join takes the
foreign key down around it, a recorded conversion and an ordinary retype alike;
a key the plan would leave between types the engine cannot join is refused at
plan time (#2359).** DEC-2331.1 left the keys on either side of a converted
column undecided, and the maintainer decided (2026-10-26) to apply, not refuse:
a recorded conversion takes the path an ordinary retype takes.

- **Measured, PostgreSQL 16 and 18.** The dialect answered nothing for
  `retype_dependents`, so the differ's drop-and-add of a key around a retype
  (`recreate_retyped_dependents`, written for SQL Server) never ran here. A
  `text` column converted to `integer` with `USING`, as the referencing column,
  as the referenced one or both, planned and then failed on the first `ALTER`:
  `foreign key constraint "fk_c_p" cannot be implemented`, the apply rolled
  back whole. An ordinary `integer` to `text` on both ends of a key fails the
  same way, so it is not a property of conversions. Inside one group of types
  (`integer` to `bigint`, `varchar(5)` to `varchar(9)`, on either or both ends)
  the engine rebuilds the key itself and checks it against the rows, and
  nothing is needed.
- **One answer for both.** `types::retype_dependents` names the foreign keys
  when the change leaves the group of types a key can join, in either
  direction. The differ then does what it does for any dialect that names them:
  the key is dropped before the first of the retypes, added after the last, and
  the engine's effects order the rest (DEC-1998.1). There is no path of its own
  for a conversion. A pair the table cannot place (an array, a user-defined
  type, `json`, `tsvector`, `xml`) keeps the engine's handling, as before. The
  re-added key is a `constraint` change, gated like any added key; the drop
  carries no class.
- **The table is measured, and directional.** A key was created between every
  pair of 22 types on both versions (484 pairs, the same answer on both,
  SQLSTATE 42804 for each refusal): whole numbers reference whole numbers,
  `numeric` and floats, never the other way round; `text`, `varchar` and `char`
  join one another; `date`, `timestamp` and `timestamptz` join one another;
  `time` also references `timetz` and `interval`; `inet` and `cidr` join; the
  rest join only themselves. `foreign_key_type_compatibility_matches_the_engine`
  re-measures it. `Some(false)` is the engine's refusal, and `None` is "not in
  the table", which refuses nothing.
- **The one-sided case is refused by name where the plan is built.** A key
  whose columns the plan retypes and whose declared types the dialect knows it
  cannot join is `DiffError::ForeignKeyTypesDiffer`, raised by `diff` itself, so
  `plan`, `plan --db` and every planner that diffs refuse it before any
  statement. The message names the constraint and its table, both columns with
  their declared types, and the next step: change the other end in the same
  plan (recording its conversion where the dialect records them), declare the
  column with the type it has today, or remove the key; with both ends retyped
  it asks the declarations to agree. A dialect with no table says nothing.
- **The pre-flight is unchanged.** The NULL probe, the key probes and the vet
  read the column and never a key, so a dropped and re-added key changes none of
  them. The key's own probe counts orphans through the cast on both sides for an
  ordinary retype, as it did. For a recorded conversion it stays "unchecked",
  named in the report, because the converted value is the expression's over the
  table's own row and the probe's alias cannot carry it (DEC-2331.1); the engine
  validates the key when it is added, and a converted value that has no parent
  rolls the whole apply back (pinned by
  `a_converted_value_that_breaks_the_reference_rolls_the_whole_plan_back`).
  `--staged` takes one logical change and is not offered this plan.
- **Not done.** A key declared between types the engine cannot join, with no type
  change in the plan (a new key or table), still reaches the apply; an
  expression-aware orphan probe for a recorded column would turn the rolled-back
  apply into a refusal by name.

<a id="dec-2346-1"></a>

**DEC-2346.1. A scalar declared enum may be a LIST or RANGE partition key; every other reference keeps DEC-1523.1's refusal (#2346 part 1, #2112).**
Maintainer, 2026-10-24: the first of two PRs, partition keys with the staged
attach's key rule; `data:` rows over an enum are the second. It narrows
DEC-1523.1 and nothing else of it.

- **Admission is a lookup by name.** A partition key column whose type is a
  scalar reference to a key of `Schema.types` that is a `ManagedType::Enum`,
  on the same side, is admitted for a LIST or RANGE parent. A domain (over an
  enum or over anything), a composite, an external or unknown name and every
  array keep DEC-1523.1's message, as does a HASH key, which the engine
  computes from the enum's oid, a number a rebuild or a restore changes. The
  dialect hook is `Dialect::validate_table_in_schema`, defaulted to
  `validate_table_sources`; `validate_table_sources` and `validate_table`
  call it with no declared types, so a table seen alone admits nothing. SQL
  Server does not override it.
- **A bound is a declared label.** A bound datum of a partition of such a key
  must be one of the enum's `values`; `MINVALUE`, `MAXVALUE` and `NULL` name
  none. Anything else is refused where the declarations are read, naming the
  partition, the label and the declared labels. No engine is asked, so a
  type created in the same plan is judged as a standing one is, and the
  plan-time reads that cast a bound to the key type (the spelling and the
  LIST collision queries) skip an enum key: its label has one spelling, and
  the type may not exist yet.
- **A label added in the plan cannot bound a partition in it** (measured on
  16 and 18: `FOR VALUES IN ('new')` after `ADD VALUE` in one transaction is
  "unsafe use of new value"). The refusal is DEC-1635.6's, with its remedy
  "apply the label first, then the rest": the partition's bound, in a create
  or an attach, joins the surfaces `unsafe_uses` reads.
- **A rename carries the bounds.** The engine renames a label inside the
  bounds that hold it (measured on 16 and 18: `pg_get_expr(relpartbound)`
  follows `RENAME VALUE`). The observed bound is carried through the plan's
  own `RenameEnumValue` pairs, in the order they run and by exact label,
  before it is compared with the declared one, by the differ and by the
  apply's movement check; a plan that renames the label and no more holds no
  detach, attach or recreate.
- **The probes ask the rows in the labels the rows hold.** A bound is declared
  in the labels the plan leaves, and the pre-flight counts rows before any
  statement has run. For an enum key every probe that evaluates a bound of a
  partition the plan creates or attaches (the move and reference counts beside
  a DEFAULT, the range counts of a create or an attach, the rows of an attached
  DEFAULT against the partitions made beside it) is given the bound with its
  labels carried back through the plan's own renames of that enum, once and by
  exact match, so `b→c` then `a→b` takes a declared `b` back to `a`. The
  parent's key types ride on `Partitions`. Post-rename statements (the staged
  attach's CHECK, the move itself) keep the final labels.
- **A removal or reorder is refused by name** when the enum keys a
  partitioned table, because a partition key cannot be retyped: the remedy
  is to rebuild the partitioned table (#2298). Labels can still be added and
  renamed.
- **#2112: the staged attach keeps its key where its CHECK is valid.**
  `Dialect::attach_scan_key` receives the key columns' types and the declared
  types. PostgreSQL carries the key for a built-in type, scalar or array, and
  for a declared scalar enum, whose bare operators and untyped literals
  resolve to the type's own (measured: the CHECK is created, validated, and
  the attach logs `partition constraint … is implied by existing
  constraints`, for LIST and RANGE); it carries none for any other reference,
  since a domain over an enum has no operator at all (`operator does not
  exist: dmood = unknown`) and a cast to its base type creates a CHECK that
  does not imply the bound. DEC-2126.1's held-name rule is unchanged, and an
  empty key is the single `ATTACH`, so no plan format changes.
<a id="dec-1635-8"></a>

**DEC-1635.8. Slice 4a alters a declared domain's checks, default and NOT NULL in place, in two halves.**
A retained domain whose declaration differs plans one `Change::AlterDomain
{ uid, name, from, to, half }` per half that has a statement, and the emitter
derives every statement from `(from, to, half)` alone. A changed check is a drop
and an add under its name, as a table's is, and a check whose text differs only
in layout is changed too: the text recorded is the text applied (DEC-1635.4).
No `renamed_from` is added for checks. Maintainer decisions of 2026-10-24: a
tightening uses `ADD … NOT VALID`, the vet, then `VALIDATE`; a standing `NOT
VALID` check compares equal; the base type and collation wait for 4b.

- **Loosen**, ordered as `DropCheck` (class 2, after the type's rename):
  `DROP CONSTRAINT` for each check removed or changed, `DROP NOT NULL`, and
  `SET DEFAULT` or `DROP DEFAULT`. The default is here and not with the
  tightening because it has to be in force before the plan's inserts that omit
  the column. It carries no risk class, as `DropCheck` carries none (DECISIONS
  486).
- **Tighten**, ordered as `AddCheck` (class 13, after the plan's rows): `SET NOT
  NULL`, then `ADD CONSTRAINT … NOT VALID` for each check added or changed, one
  statement. Measured on 16 and 18: a plain `ADD CONSTRAINT` calls the check
  over every existing row inside the `ALTER`, as the deployer, so the vet could
  only follow code that had already run; `NOT VALID` binds the expression and
  evaluates nothing. The statement's `DeclaredTypeStatement` carries the final
  definition and `after_vet`, the `VALIDATE CONSTRAINT` of each added check,
  which the vet runs in the same transaction after it has read the bound code
  with the built-in-only decoder (DEC-1635.4). A check that calls a function of
  the project is refused there, the function never run, and nothing is left
  behind. In a staged apply the half is one statement and so one step: it adds,
  vets and validates in that step's transaction, and a resume past it finds the
  domain as the step left it. A tightening is `constraint` for an added or
  changed check and `not-null` for NOT NULL; the plan lists, as a connected
  check, the tables whose rows the validation scans under a lock held to
  commit.
- **Evidence.** The sealed final definition is the pin, and the half's
  statement states what stands after it: the re-read compares the base,
  collation, NOT NULL and check names exactly and the default and check texts by
  presence (DEC-1635.4). An executed alteration replaces the baseline's catalog
  text as the thing the live domain is held to; before any statement the
  baseline still is. A saved plan's alterations replay against the approved
  before-definition and the sealed one, so a payload cannot say another
  difference or a half with nothing to do. `Declared.types` records `to`, as
  creation does. A rename of the domain in the same plan carries the definition
  before the alteration.
- **Refused by name.** A tightening while an array or a composite of the domain
  (or of a domain built on it) is a column of a table: PostgreSQL answers
  `cannot alter type "d" because column "t.x" uses it`, over an empty table too,
  at `ADD` and at `VALIDATE`; `NOT VALID` alone passes. The census is read from
  the catalog at plan time and again inside the apply's transaction before the
  first statement, so a column that appeared in between is refused too. A
  composite that no column uses does not block (measured). A base type or
  collation change is refused, with a pointer to the domain rebuild (slice 4b),
  and so is a domain whose identity is not recorded. A domain the plan rebuilds
  gets no `AlterDomain`: its replacement is created from the final declaration.
- **A standing `NOT VALID` check** compares equal to its declaration, as the
  model holds no validation (DECISIONS 251). `plan --db` reports it as
  `plan.domain-check-not-valid`, a warning, naming the check and the statement
  a person runs: `ALTER DOMAIN … VALIDATE CONSTRAINT …`.
- **Not done.** A domain an external composite holds as an attribute is planned,
  and the apply refuses it as `the referenced type … changed since planning`:
  the composite's pin digests its attributes' domains, and the alteration
  changes that digest between the plan and the statements. Plan format 55.

Labels of the enum a domain is built on (round 2 of the PR). The same-plan
new-label check (slice 3a) reads what each half introduces: the default
`Loosen` sets and each check `Tighten` adds or changes, by the lexical test
that reads a created domain's; what a half drops or leaves is no use of the
label. A default naming a label the plan renames must run after the rename, so
a `Loosen` that sets a default is ordered in class 7 after every label change
(the one that drops a default or only drops checks stays in class 2), and the
apply's per-statement check holds a domain the plan has yet to alter to its
structure once an enum statement has run, since the engine then spells the
renamed label in the unaltered default and checks. That holds for a domain
the plan has *yet to alter*, whatever renames of it have run: the domain's own
rename changes only its name, which the uid resolves, so it does not end the
allowance. A domain's `RenameType` therefore requires nothing of the incarnation
of the enum it is built on (only its creation does), which let the order
effects contradict the plan's own order for a domain renamed beside a rename of
its base enum's label.

What a reviewer approves shows the validation. `render_script` (the one
renderer under `plan --sql`, a bootstrap script and a staged listing) prints a
statement's post-vet SQL (`after_vet`) after the statement and before the
next, behind a line saying that pbps vets the bound code at that point; the
executor keeps its order and runs it, not the text. The tail is derived from
the change (`from`, `to`, `half`), which the plan checksum covers through the
change payload (the checksum is over the saved plan, not over the rendered
script), so the validation shown is the one the checksum approves. A loosening
and a `SET NOT NULL` alone have no tail and show none. Tests:
`a_statements_post_vet_sql_is_rendered_after_it_and_before_the_next` and
`the_rendered_script_shows_the_validation_after_the_not_valid_add`.

Tests: `a_renamed_domain_is_still_one_the_plan_has_yet_to_alter` and
`a_renamed_domain_over_a_renamed_label_takes_the_default_that_uses_it` (the
domain's rename, the label's rename and the default in one plan),
`an_altered_domain_refuses_a_new_label_in_what_its_halves_introduce`,
`a_new_label_in_what_a_domain_alteration_introduces_is_refused_until_the_label_is_applied`,
`a_domain_default_or_check_naming_a_renamed_label_runs_after_the_rename`, and
`domain_alterations_converge_in_place_keep_the_uid_and_record_the_authored_text`,
`a_tightening_over_a_violating_row_rolls_the_apply_back_and_keeps_the_old_check`,
`a_check_calling_a_project_function_is_vetted_before_it_runs_and_leaves_nothing`
(the pinned negative: emitted as a plain `ADD CONSTRAINT`, it fails because the
function ran), `a_tightening_is_rebuilt_for_an_array_of_the_domain_and_refused_for_a_composite`,
`a_staged_domain_tightening_is_one_step_and_a_resume_accepts_it` and
`a_loosening_comes_before_the_columns_the_plan_adds_and_a_tightening_after` pin
it on 16 and 18, with unit tests for the delta, the emitter, the sort, the
replay and the per-statement checks.

<a id="dec-2346-2"></a>

**DEC-2346.2. A scalar declared enum may carry `data:` rows; every other reference keeps DEC-1523.1's refusal (#2346 part 2).**
Maintainer, 2026-10-24: the second of two PRs, after the partition keys
(DEC-2346.1). It narrows DEC-1523.1 and nothing else of it.

- **Admission** is DEC-2346.1's lookup by name: a column whose type is a
  scalar reference to a declared enum, on the same side, may carry rows as a
  data column or as the primary key. A domain (over an enum or over anything),
  a composite, an external or unknown name and every array keep the refusal
  and its text, and the row reader refuses any array column whatever its type
  (#1167). `Dialect::validate_table_in_schema` carries the declared types to
  the validation, and `pull --data` calls it with the pulled ones; the row
  reader takes them as `query_in_schema`, `query` seeing none.
- **A cell is a declared label.** A cell, or the row key of an enum key, is one
  of the enum's `values`, or NULL where the column is nullable. Anything else
  is refused where the declarations are read, naming the table, the row, the
  column and the label, beside the check of a partition bound. No engine is
  asked: the spelling probes skip these cells and keys, so a type created in
  the same plan is judged as a standing one.
- **A read by key compares an enum key as text** (design M1, measured on 16
  and 18): `k IN ('nonesuch')` over an enum is the error `invalid input value
  for enum`, and the plan's read runs before the plan's `ADD VALUE` or `RENAME
  VALUE`, so a declared key naming a label this plan adds or renames would
  abort the plan with the engine's message. `CAST(k AS text) IN (…)` is no
  row, and the same for the key aliases. Writes run after the type's changes
  and keep the native comparison.
- **A label added in the plan cannot be used by a row of it** (the label is
  unusable in the transaction that adds it: `unsafe use of new value`). The
  key or a changed cell of an `InsertRow` or `UpdateRow` naming it joins the
  surfaces `unsafe_uses` reads, with its message and remedy, "apply the label
  first, then the rest"; a cell an update leaves, and a text column holding the
  same string, are no use.
- **A rename carries the rows.** The engine renames a label in the rows that
  hold it (measured on 16 and 18), and the declarations spell the final
  label. The observed rows are therefore carried through the plan's own
  `RenameEnumValue` pairs, by exact label and in the order they run (a chain
  `b→c`, `a→b` takes `b` to `c` and `a` to `b`), the key and the cells of
  columns over the renamed enum, before they are compared with the declared
  ones: the differ plans no `UpdateRow`, delete or insert for a rename, and
  the apply's movement check carries the baseline the same way
  (`forwarded_rows`, beside `forwarded_bound`; the single label walk is
  `carried_label`, which `carried_bound` uses too). The rows a scoped read
  must find are named by the labels they hold: the apply asks the declared
  keys and the labels they had before the plan's renames
  (`scopes_before_the_plan`), and the plan's own read, taken before any plan
  exists, asks for the labels a declared key was renamed from
  (`read_before_the_renames`, from `renamed_from`). An `ensure` table whose
  recorded key set is replaced records it as a `SetEnsureKeys`, which is
  bookkeeping and not a row change.
- **Every apply asks the held keys, staged or not** (`held_before_the_plan`):
  the transactional baseline, the staged baseline of a fresh run and of a
  resume, and the checkpoint read name the declared keys and the labels they
  held before the plan's renames, so a row is found whichever side of the
  rename statement the read falls on. The rename of the label an `ensure`
  table's row is keyed by also replaces the table's recorded key set
  (`SetEnsureKeys`), a second logical change, so such a plan is not a staged
  one (measured); a staged plan can rename a label that rows hold in a
  cell, or that no declared row names, and those apply and resume.
- **The checksum pins the row a takeover under a renamed key depends on**
  (`pinned_baseline_scopes`): the plan's baseline checksum is taken over the
  recorded scopes and the planned ones, and the planned ones are widened by the
  labels their keys hold before the plan's renames, on the side that pins
  (`plan`) and the side that verifies (`apply`, transactional and staged,
  fresh) through the one function. Without it the row `sad` that a takeover
  renames to `blue` was in the comparison but not in the checksum, and a change
  or a delete of it between `plan` and `apply` passed unseen into the baseline
  the apply recorded (SPEC 7.6). A plan that renames no label a key holds pins
  exactly `pinned_scopes`, so its checksum does not change. The resume of a
  staged run is checked against the checkpoint's own recorded rows and needs
  nothing; the plan's read is widened from the `renamed_from` hints, which name
  the same pairs the plan's renames do.
- **No staged plan holds a chain of renames** (measured on 16 and 18): each
  `RenameEnumValue` is a logical change, so a chain is two or more, and
  `plan --staged` and the apply of an artifact edited into a staged one both
  refuse it ("a staged plan must contain exactly one logical change"), alone
  or beside an `ensure` table's rows. The per-step reads of a staged run need
  no scope derived from the renames still ahead: the one rename a staged plan
  can hold is the statement before the read.
- **The widened baseline is trimmed to what the plan still manages**
  (`trim_to_the_final_scope`): the baseline of an `ensure` table is read under
  the declared keys and the labels they held, and a chain (`b→c`, `a→b`) over
  a table that declares only `b` reads the rows `a` and `b`, which the plan
  leaves as `b` and `c`. The closing read asks for `b` alone, so `c` still
  exists but is not managed, and the baseline keeps a row only when its key,
  carried forward through the renames, is a key of the plan's scope; the
  rows keep their labels and the comparison carries them. The per-step reads
  of a staged run ask the plan's own keys, as the closing read does, since
  a widened one would read an unmanaged row that a swap of labels leaves
  under a held name.
- **Slice 3b needs nothing**: a rebuild converts a column of the enum, a key
  among them, through `col::text::tmp`, and its row probe counts the live
  rows that hold a label the plan removes, a declared row of an `exact` table
  that is no longer declared included; the declared-label check refuses a
  declared row that names a removed label before that.
- **Limits.** A row that omits a column whose default names a label the plan
  renames is restated by the default's changed text, and its guard names the
  old label in the engine, which no longer has it: the apply is rolled back.
  Writing the cell explicitly (`m: blue`) is the remedy. Rows under an
  enum array or a domain stay refused until #1167's array codec exists and
  the domain's operators do (a domain over an enum has no `=`).

A type this plan renames or moves (ready round 2). The rename runs before any
row change, and the recorded cells are held by casting through the column's
type, so a guard that named the type the rename retires failed at apply and
rolled a valid plan back. The row changes keep `types` on the base side and
`after_types` on the desired side, which the type pins depend on; the emitter's
`Held::of` decides: a declared type read as one type and held as another reads
the recorded text as `text` and goes to the type the column then has, never
through the retired name. A rename is thereby no conversion; a retype to a
built-in type goes through text the same way; a built-in retype keeps its
cast chain. Verdict per row statement: an insert carries the declared types
(execution-time names); an update's set values use `after_types` before
`types`, its guard is fixed here; a delete's guard is fixed here; the key is
held by the declared `key_type`; `ensure` keys emit no SQL and compare by
`key_type` at read; the label-rename forwarding rewrites recorded text only and
casts nothing. A schema move of the enum is the same case.

Rebase review: a default. A row that omits an enum cell is held to the default
the column then gives, and the recorded default is text the catalog deparsed
with its type welded on (`'sad'::app.mood`), which the guard embedded as it
stood; the type is renamed before the row statement runs, so the cast named a
type that no longer exists. One rule for every site that embeds recorded
default text (the update's and the delete's guard, the unchanged cells and the
insert's postcondition all go through `defaulted_cell`): when every cast a
default ends in is to a declared, schema-qualified type, array or not, the
guard holds the literal alone and the column's own cast, the engine's, reads
it; any other default keeps its text and its built-in cast. Measured, identical
on 16 and 18: after the rename the catalog spells the default
`'sad'::feeling`, and `column::text = CAST(CAST('sad' AS text) AS feeling)::text`
is true for an untouched row, false for one changed outside pbps, and holds for
`'{sad,ok}'::mood[]`, a domain over the enum and a `numeric(5,2)` default (which
keeps its cast). Sites: explicit cells carry no type text; default cells in
guards and the postcondition are this rule; keys are compared with the declared
`key_type`; `SET … = DEFAULT` is the keyword; the label-rename forwarding
rewrites recorded values and casts nothing. A default naming a label the plan
renames stays the limit above.
`rows_that_omit_an_enum_cell_with_a_default_are_guarded_under_the_renamed_type`
pins it on 16 and 18, including a row changed outside pbps still refused.

Tests: `only_a_scalar_declared_enum_carries_data_rows`,
`a_cell_over_an_enum_column_names_declared_labels_only` and
`a_declared_enum_is_read_and_its_key_is_compared_as_text` (pbps-pg),
`a_row_using_a_label_added_in_the_same_plan_is_refused`,
`observed_rows_are_carried_through_the_plans_label_renames`,
`a_scope_names_the_labels_its_keys_have_before_the_plan` and
`rows_that_follow_a_label_rename_are_no_change_of_rows` (pbps-diff), and, on 16
and 18, `a_lookup_table_over_enums_converges_updates_and_deletes_its_rows`,
`pulled_enum_rows_recreate_and_re_pull_the_same`,
`a_label_rename_under_rows_plans_no_row_change_and_finds_the_renamed_key`,
`a_cell_that_is_not_a_declared_label_or_uses_a_new_one_is_refused_and_the_split_applies`,
`rows_taken_over_under_a_renamed_key_are_found_by_the_label_they_hold`,
`a_default_naming_a_renamed_label_does_not_abort_the_read_of_the_rows`,
`a_staged_label_rename_under_ensure_rows_applies_and_resumes`,
`a_staged_plan_cannot_hold_the_rename_of_an_ensure_tables_key`,
`a_chain_of_label_renames_leaves_an_unmanaged_row_alone_in_a_narrow_ensure_table`,
`a_takeover_under_a_renamed_key_pins_the_row_it_depends_on`,
`a_staged_plan_cannot_hold_a_chain_of_label_renames`,
`rows_of_a_renamed_enum_type_are_inserted_updated_and_deleted_under_its_new_name`
(a rename and a schema move; pbps-pg `row_guards_cast_a_renamed_declared_type_by_its_new_name_only`) and
`a_rebuilt_enum_keeps_the_rows_over_it_and_a_removed_label_a_row_holds_is_refused`,
with `every_other_reference_keeps_the_data_refusal_beside_a_declared_enum`
for the refusals.

<a id="dec-1635-9"></a>

**DEC-1635.9. Slice 4b rebuilds a declared domain under its uid, with the enum rebuild's mechanism.**
PostgreSQL has no `ALTER DOMAIN … SET DATA TYPE` or `COLLATE`, cannot repoint a
domain at another type, and refuses a new check or NOT NULL in place while an
array of the domain is a column. The keep-uid rebuild of DEC-1635.7 is
generalised, not repeated: `Change::EnumRebuild` carries a domain's definition
as it carries an enum's, with the same three steps (`Create`, a `Retype` per
dependent column, `Swap`), the same temporary-name chooser, the same
`type-rebuild` class, the same dependent drop and restore and the same module
rebuild. A domain is rebuilt when one of four things holds, and the plan says
which in the `Create` line (`Create.why`):

- **Its base type changed.** Each column moves with a plain `ALTER COLUMN … TYPE
  <replacement>`, no `USING`, on the engine's own assignment cast, exactly when
  `change_risk(old base, new base)` is not `Incompatible` and both bases are
  built-in. Any other base change is refused by name, pointing at #2331
  (ADR-0012 Amendment 6). A base change that narrows carries `Narrowing`
  beside `type-rebuild`, and the plan counts the rows that do not fit with the
  existing `cannot_become` probe, per column (an array's elements are reported
  unchecked). The new domain's checks are not probed at plan time (DEC-1635.4);
  a row appearing between plan and apply, or failing the checks, rolls the whole
  apply back with the old domain standing. Plan format 56.
- **Its collation changed.** The same, with no narrowing pair.
- **The enum or domain it is built on is rebuilt**, transitively, in base-depth
  order (`Why::Derived`). The replacement is created over the base's
  replacement (`Create.over`, `Swap.over`) and each column moves with 3b's
  `::text::<replacement>` (arrays `::text[]::<replacement>[]`); rows holding a
  label the base drops are counted by 3b's probe through the domain's columns.
  The swaps run deepest first, so the old base is dropped last. A domain created
  in the same plan over a type the plan rebuilds is refused (it would hold the
  type the swap drops); 3b's refusal of a declared domain over a rebuilt enum is
  removed.
- **A check or NOT NULL is added while an array of the domain is a column**
  (`Hints.domain_array_holders`, read from the catalog at plan time), which
  slice 4a refused. The replacement is created from the final declaration, so
  moving the columns validates the arrays. A composite attribute of the domain
  stays refused.

**The conversion rule, measured on PG 16.15 and 18.6, identical on both.** A
castable retype to a text-based domain leaves the indexes (btree, expression,
unique), the primary key, extended statistics, the table `CHECK` (re-deparsed as
`((v)::text <> 'zz'::text)`), a castable column `DEFAULT`, a foreign key
between two columns converted in the same plan, an array column and a
partitioned table's non-key column working with no manual step. A retype to a
domain over integer has no assignment cast (`column "v" cannot be cast
automatically to type d_int`) and is refused before it is tried. A `varchar(20)`
value of 16 characters into `varchar(10)` fails with `value too long`. A
generated column, a view, an `UPDATE OF` trigger, a partition key, a function
signature, a domain over the domain and a composite attribute each block, with
the messages in the design. The replacement carries no ACL and no comment.

**Dependents.** For an `Assign` retype nothing is dropped and restored; the
`Text` retype of a derived rebuild drops and restores checks, filtered indexes
and foreign keys by the dialect's rebuild policy, as 3b does. Views and bodies
come down and go back through the module rebuild. The retained set is read from
the catalog at plan time (`type_rebuild::census`) and again before the first
statement: a dependent the plan cannot restore is refused by name, never
`CASCADE`d. The census skips the domains the plan itself rebuilds, and a check
that stands on one of them, which go with it.

**Decision 4, for domains and enums alike.** The comment is read before the
rebuild and set on the replacement. A non-default owner or privilege list on
the old type is refused by name. Nothing needed to change for the enum rebuild:
3b's census already refused both.

**Limits recorded.** A domain check with an explicit cast to a rebuilt enum
binds the old enum and blocks `DROP TYPE` at apply, which rolls back loudly. A
domain renamed in the same plan as its rebuild is refused. Every table that
holds the domain is rewritten under `ACCESS EXCLUSIVE`, even for `varchar(20)`
to `text`; the plan names the tables.

Tests: `a_domains_base_or_collation_change_is_rebuilt_under_its_uid_with_its_columns_converted`
(table, array, partitioned table, index, table check; `text`, `varchar(30)` and
`"C"`), `a_narrowed_domain_base_reports_the_rows_that_do_not_fit_and_rolls_back_a_late_one`,
`a_base_the_engine_cannot_convert_is_refused_by_name_and_points_at_the_author_stated_conversion`,
`an_enum_rebuild_rebuilds_the_domains_over_it_in_depth_order_and_says_why`,
`a_domain_rebuild_refuses_each_dependent_it_cannot_restore_by_name_before_any_sql`,
`a_domain_rebuild_carries_the_comment_to_the_replacement`,
`a_tightening_is_rebuilt_for_an_array_of_the_domain_and_refused_for_a_composite`
`a_domain_beside_an_enum_partition_key_is_tightened_or_rebuilt_like_any_other_holder`,
`a_chain_of_eleven_domains_over_a_rebuilt_enum_is_swapped_deepest_first` and
`a_domain_renamed_in_the_plan_that_rebuilds_it_is_refused_by_name_before_any_sql`
on 16 and 18, with unit tests in the model, differ, emitter, preflight, census
and report.

Round 1 of the PR. The swaps sort by `Swap.depth` reversed at a fixed width
(`usize::MAX - depth`, twenty digits), not by a one-digit complement that gave
every depth past nine the same value and left the order to the names; the
create order (`Create.ordinal`) and the drop order (`DropType` rank) are plain
integers and never had a cap. The planning passes over a chain are bounded by
the number of domains, not by 64, and a domain whose base could not be ordered
is reported instead of dropped. A domain renamed in the plan that rebuilds it is
refused by name (the rename guard compares the desired name, as the enum's
does), also when it is rebuilt only because its base is: rename it in one plan,
then change it in the next.

Round 2 of the PR. A domain rebuilt because the one it is built on is rebuilt
(`Why::Derived`) undergoes the change of its root's base, through every level
between, so its columns carry the same old and new base type
(`Replaced.effective` is inherited down the chain). The narrowing risk and the
`cannot_become` probe then see a column that only holds the derived domain,
exactly as for the root's own columns; the conversion stays `::text::<tmp>`.
A derived domain over a base the engine cannot convert is refused with the root
(#2331): the root is never planned, so nothing under it is. A collation change
carries no base pair on either path, as before.
`a_column_of_a_domain_built_on_a_narrowed_one_is_counted_and_needs_the_narrowing_allow`
pins it on 16 and 18.

Round 3 of the PR: one rule for both gaps. The base change a column undergoes
is derived from the chain under its type, not from why the type is rebuilt: a
domain takes its own base's pair if its base changed, else the pair it
inherits from the type it is built on, whatever its reason (a collation change
and an array fallback as much as the plain derivation). A link that is an
array of the type below it makes the pair one of arrays
(`varchar(20)[]` to `varchar(10)[]`), through any number of links; the dialect
keeps the element's risk, and preflight reports such a column's elements
unchecked, by the mechanism a direct array column already has, instead of
length-checking the array's text as a scalar. Over a rebuilt type the values go
through text for every reason, since an enum has no assignment cast to its
replacement. An enum below carries no pair.
`a_domain_over_an_array_of_a_narrowed_one_is_not_length_checked_as_a_scalar` and
`a_derived_collation_change_over_a_narrowed_root_still_counts_the_rows_that_do_not_fit`
pin it on 16 and 18.

Round 4 of the PR. A replacement domain over a type the plan does not rebuild
was created at operation ordinal 0, tied with the enum's first label statement
and ordered by name, so a default naming a renamed label could run before the
rename. It now takes the ordinal 3b's enum `Create` takes, the count of the
label operations of the enum it uses, directly or through the domains between
(rebuilt or not), so it follows every rename and addition; one over a rebuilt
type keeps its place after that type's creation. The effects say the same: a
domain's `Create` requires the type it is built on in its final incarnation,
unless it is created over a replacement. The same-plan new-label check now
reads a rebuilt domain's `Create` as it reads a new domain's, since a default
or check naming a label added in the same transaction is refused by the
engine. The domain's `Retype`, `Swap` and the restored defaults and checks are
classes 9 and 13, after the label statements of class 7.
`an_array_held_domain_rebuilt_beside_a_label_rename_is_created_after_it` pins it
on 16 and 18. This supersedes the base and collation refusals of DEC-1635.8.
