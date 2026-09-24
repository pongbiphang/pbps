# Reference data: the pre-delete probe

Counting the rows a delete would orphan or cascade into, before it runs. Part of
the [decision record](../../DECISIONS.md), which says how to add an entry here.

<a id="decision-55"></a>

55. **The pre-delete probe asks `sys.foreign_keys` at run time, counts
    cascades, and leaves out the rows the plan itself moves.** A probe is
    built from the plan and nothing else, and the plan does not know which
    tables reference this one — nor should it trust the declarations to say,
    since a foreign key someone added by hand is exactly the one that will
    refuse the delete. So the probe is dynamic SQL: the referencing tables and
    columns come from the catalog through `QUOTENAME`, the key is bound as a
    parameter, and the count runs through `sp_executesql`. `ON DELETE CASCADE`
    children are counted although the engine would not refuse them: a
    reference row's delete cascading into an application table is the case
    the `data-delete` gate is for. A child row this plan updates or deletes is
    excluded by key, whatever column the update touches, because the plan
    runs the update *before* the delete precisely so the engine accepts it;
    an over-exclusion is refused by the engine inside the transaction, which
    is the loud direction to be wrong in.

<a id="decision-73"></a>

73. **The pre-delete probe excludes an updated child row only for the
    column its update sets.** The first cut excluded every row the plan
    updated, on the reading that "the plan moves it". A plan that set some
    other column of a row still pointing at the doomed parent then probed
    zero, and under `ON DELETE CASCADE` the engine deleted the child
    silently — the one outcome the probe exists to prevent. Which column a
    foreign key references is known only to the catalog query that runs the
    probe (`c.name`), so the exclusion is a `CASE` on it: a deleted row for
    every column, an updated row for the columns it sets. An update that
    sets the referencing column to the doomed parent itself is still
    excluded, and fails loudly at the constraint instead.

<a id="decision-85"></a>

85. **The pre-delete probe lets the engine say whether an update moves a
    row off the deleted key.** 73 excluded an updated row for the column its
    update sets; an update that set the referencing column to the same key
    under another spelling (`01` for `1`, `OLD` for `old` under a
    case-insensitive collation) was excluded too, and cascaded. The
    exclusion now asks the parent table, at run time, whether the value the
    row is set to *is* the deleted key — the comparison the engine makes —
    and a row set to `DEFAULT` or NULL, which no probe can compare, is
    counted. Measured on a live server with `OLD` against `old`.

<a id="decision-112"></a>

112. **The pre-delete probe counts the rows a plan puts *onto* the parent, not
    only the ones already there.** 71 built the probe out of `COUNT(*) ...
    WHERE fkcol = @key` and excluded the rows this plan moves away. The
    mirror image was missing: inserts and updates both run before the deletes
    (`order_key`), so a child this plan *adds* to the parent — an insert, or
    an update moving a stored row onto it — is invisible to a count taken
    before statement one. `ON DELETE CASCADE` then takes the new row straight
    back out, or `SET NULL` unpicks its reference, the apply succeeds and
    records the result, and the next plan proposes the same row again: the
    declarations and the database never converge, and nothing says so.
    Counted now, with the engine deciding whether the arriving value and the
    deleted key are one key — the same question `updated` already asks it, so
    `OLD` arriving on `old` counts under a case-insensitive collation. An
    updated row is guarded against being counted twice: only one not already
    sitting on the key is arriving. A column an insert omits is not counted:
    its value is the column's own default, which lives in the catalog and not
    in the plan.

    The other probes were swept for the same blindness and need nothing. The
    orderings put every one of them either before the row changes
    (`AlterColumnNullability`, `AlterColumnType`) or after them
    (`AddUnique`, `SetPrimaryKey`, `AddForeignKey`) with a *loud* failure —
    the engine refuses inside the transaction and the plan rolls back. The
    delete is the one whose failure is silent, which is why it is the one
    that has to look forward.

    **The sweep asked one question and there were two; see 151.** "Can a probe
    *miss* a violation" is answered above. "Can a probe *report* one the plan
    is about to remove" was never asked, and `AddForeignKey` answers it
    badly: it sorts after every row change, so a plan that supplies the
    missing parent rows or repairs the orphaned children is refused for the
    very violation it was written to remove.

<a id="decision-116"></a>

116. **The pre-delete probe asks about every foreign key to the table, not
    only the ones that reference the key column.** A foreign key may target
    any unique key of the parent, and filtering the catalog to
    `rc.name = <the key column>` dropped those constraints out of the query
    altogether — so `ON DELETE CASCADE` could take child rows, in an
    unmanaged application table as easily as a declared one, with the probe
    reporting nothing. The filter is gone; every fragment that meant "the
    parent row being deleted" now says so by *its* key column
    (`<key column> = @key`) and compares the child against
    `(SELECT <referenced column> FROM <parent> WHERE <key column> = @key)`,
    which is the same query for the primary-key case.

    A composite foreign key contributed one count per column until 121.

<a id="decision-117"></a>

117. **An inserted row carries the defaults of the columns it omits, and the
    pre-delete probe reads a defaulted write as an arrival.** 112 counted
    the rows a plan puts onto a parent it deletes, by the values the plan
    spells; a column an insert omits, or an update sets to `DEFAULT`, is
    written at the column's default, which the plan did not spell — and a
    default that names the doomed parent was an arrival the probe never
    saw, so the cascade took the declared row and apply recorded it. The
    differ now writes the omitted columns' defaults into `InsertRow`
    (`defaults`, absent from older plans and read as empty), `Cell::Default`
    already carried the update's, and the probe renders a literal default
    as the expression the catalog spells it in, for the engine to compare
    like any other value. A default that is not a literal (`NEXT VALUE
    FOR`, `NEWID()`) has no value before it runs, and is the one arrival no
    probe can ask about; `NULL` references no row.

<a id="decision-121"></a>

121. **The pre-delete probe matches a composite foreign key as one tuple.**
    116 kept one count per column of a key, and called the over-count the
    safe direction: a child on the surviving parent `(1, 2)` matched deleting
    `(1, 1)` by its first column, so a table with a composite alternate key
    had every delete refused — a gate nobody can pass is not a gate. The
    probe now takes one count per constraint, and the engine assembles the
    comparison from `sys.foreign_key_columns` at run time, `AND p.<referenced>
    = ch.<referencing>` for each column, so a child is counted only where
    its whole tuple is the deleted row's. The rows the plan writes are
    recorded per row rather than per column, and the same tuple is built for
    them — the value written where the update or insert sets a column, the
    stored cell elsewhere — so an update that sets two columns of one key is
    compared as one row after the update, not as two moves, and a row is
    left out of, or added to, a key's count only where the key spans a
    column the write sets. A key spanning a column an update sets to NULL,
    or to a default that is not a literal, keeps the row counted; one
    spanning a column an insert leaves that way is not asked about (117).
    The fragments now carry subqueries, so each statement is built in a
    derived table and aggregated outside it: an aggregate's argument may not
    hold one.

<a id="decision-124"></a>

124. **A write left to a default the probe cannot evaluate, on a column a
    foreign key to the deleted row's table spans, is refused.** 117 counted
    a literal default as an arrival and called a default that is not a
    literal "the one arrival no probe can ask about", and treated it as
    absent: `CONVERT(int, 1)` is deterministic, may well be the deleted
    key, and the cascade took the declared child while apply recorded
    success. Which key of the parent such a default names cannot be
    evaluated before it runs, so the write is refused instead — where the
    catalog says a foreign key from the child to the parent table spans the
    column. The refusal is a second probe that *counts* those columns, with
    the columns and rows in its description and the remedy (spell the
    value): a probe that errors is "unchecked" to `apply`, which then
    proceeds, so a `THROW` would have been a refusal nobody heard. `NULL`
    under any parentheses names no row and is neither an arrival nor
    refused. An `IDENTITY` column a child insert omits is the engine's to
    assign and is not in `defaults`; a foreign key on it to the deleted row
    is not refused here, and is noted rather than pretended away.

<a id="decision-128"></a>

128. **The pre-delete probe ignores the foreign keys the plan removes first.**
    `DropForeignKey` and `DropTable` both sort before `DeleteRow`
    (`order_key`), so a plan that drops a constraint and then deletes a row
    it pointed at is one the engine accepts — but the probe read the catalog
    as it stands *before* statement one, counted the child rows through a
    constraint that would be gone, and refused the plan. The constraints and
    tables the plan removes are now left out of the catalog read altogether.
    Each is named with its table, because two schemas may each hold a
    constraint of one name; a table this plan also renames is named as the
    catalog has it now, because the probe runs before the rename does.

<a id="decision-144"></a>

144. **A disabled foreign key is not counted when a row is deleted.**
    `NOCHECK CONSTRAINT` leaves the constraint in `sys.foreign_keys` and stops
    the engine enforcing it, so the delete probe and the delete's own
    `HOLDLOCK` guard were counting children of a constraint that would neither
    block the delete nor cascade through it — refusing, permanently, a
    deletion the engine allows. Measured rather than assumed, because "what
    does a disabled constraint still do" is exactly the kind of question this
    project has been wrong about: with `fk_cascade` disabled, `DELETE` on the
    parent succeeds, the child row is still there afterwards, and the
    `ON DELETE CASCADE` does not run. `fk.is_disabled = 0` now filters both the
    shared counting statement and the unprobeable-default probe. Enabled is
    the test, not trusted: a constraint re-enabled `WITH NOCHECK` is
    `is_not_trusted` but enforced from that moment on, and its children are
    the ones a delete really can take.

<a id="decision-320"></a>

320. **The pre-delete probe counts every foreign key, and `convalidated` is
    never read.** ADR-0013 §1. The SQL Server probe skips a key whose
    `is_disabled` is set, because `NOCHECK CONSTRAINT` leaves it in the catalog
    and stops the engine enforcing it (144). `pg_constraint.convalidated` has
    the same shape and **the opposite meaning**: measured, a `NOT VALID` key
    reads `convalidated = f` and still refuses both a violating insert and the
    parent's delete. Copying the rule would make the probe count zero for a row
    the engine will not let go — the live test asserts both halves, so the
    mistake fails rather than being argued about.

    This is the second time this design pass has met the shape (ADR-0011
    Amendment 2 was the first): **two engines expose a similarly-named flag
    whose meanings are opposites, and the dangerous direction is the one where
    the shared-looking code compiles.**

<a id="decision-325"></a>

325. **The pre-delete probe's dynamic SQL runs through `query_to_xml`.** A
    [`Probe`] is one `SELECT` returning one integer, and this driver sends one
    statement through the extended protocol — so the SQL Server shape,
    `DECLARE …; EXEC sp_executesql …; SELECT @n;`, has no counterpart here: a
    `DO` block returns nothing at all. `query_to_xml` is this engine's only way
    to run generated SQL from inside a `SELECT`, and the count comes back
    through `xpath`. Nothing user-written reaches the generated text: names come
    from the catalog through `quote_ident` and the key through 319's renderer.

    The key columns are joined with `generate_subscripts` and array subscripts
    rather than `unnest(conkey, confkey)`: the two-array form of `unnest` is
    grammar, legal only in a `FROM` clause and impossible to schema-qualify —
    measured, `pg_catalog.unnest(smallint[], smallint[]) does not exist` — and
    every name in this file is qualified, because the read scope pins an empty
    `search_path`.

<a id="decision-333"></a>

333. **The probe refuses an incomplete count too, and not only the delete's own
    guard.** 329 established that a count row-level security has filtered is
    not a count, and put the refusal in the statement's guard. The probe next
    to it — documented in the same module as *the same count* — kept answering
    `0`.

    `0` there means "nothing references this row" and the session meant "I
    cannot see what references this row": absent and unreadable, read as one
    answer, in the one place this project has a rule about it. A human then
    approves the plan on that number. In a staged apply every insert and update
    of the plan commits for good before the delete is reached, so the guard's
    refusal arrives after the deployment is half done and the plan can no
    longer be resumed — the refusal, too late to be the refusal.

    **A second probe, not a term in the count.** DECISIONS 124's reason: the
    count is a number a reader is meant to understand, and inflating it to
    force a refusal makes it a number about something else. This one counts
    referencing *tables* whose rows this session cannot see in full, so a
    reader meets the rows first and the reason second.

    `row_security_active`, not `relrowsecurity`, for 329's measured reason: the
    switch is on for the owner too, and the owner's count is complete. The live
    test asserts both halves from the two sessions — `relrowsecurity` true for
    both, the probe counting one for the filtered role and nothing for the
    owner.

    This is the fifth finding of one shape on this branch and the second of
    this one specifically: a rule applied at the site that motivated it and not
    at its sibling. PITFALLS carries the lesson.

<a id="decision-334"></a>

334. **A relation is counted the way its foreign key covers it, and a key is
    asked about as the tuple it is.** Two ways the pre-delete probe counted
    something the engine does not.

    **`ONLY` for an ordinary table, in full for a partitioned one, and never
    the catalog's copies of one key.** A foreign key is not inherited.
    **Measured on 18.6**, a row in an inheritance child holding the deleted key
    is bound by nothing, the parent deletes, and an unqualified `FROM
    schema.table` scanned that child and counted it — refusing a delete the
    engine performs. The same measurement found a second thing nobody had
    looked for: a partitioned referencing table carries the key **twice** in
    `pg_constraint`, once on the partitioned table and once on each partition,
    so one referencing row was counted twice.

    ```text
    ordinary parent with an inheritance child, one row in the child:
      FROM ref        -> 1        FROM ONLY ref -> 0        the delete: succeeds
    partitioned referencing table, one row:
      pg_constraint rows into the parent: 2 (relkind p, conparentid = 0
                                            relkind r, conparentid <> 0)
    ```

    One rule covers all three: read only the constraints that are nobody's copy
    (`con.conparentid = 0`), and scan `ONLY` the relation unless it is
    partitioned (`cl.relkind = 'p'`), where the key does reach every partition
    and the relation itself stores nothing.

    The *arrival* terms keep their unqualified scan deliberately: they count
    rows this plan writes, the plan writes them to the table it names, and a
    non-`ONLY` scan finds them whether that table is partitioned or an
    inheritance parent. `ONLY` there would under-count an arrival, which is the
    direction that loses a row.

    **A NULL anywhere in a key answers the whole tuple.** `MATCH SIMPLE` is
    this engine's default, so a composite foreign key with a NULL in any of its
    columns is not checked at all — measured, the child is accepted against a
    parent row that does not exist and the parent then deletes with that child
    sitting there. DECISIONS 124's refusal looked at the unprobeable column
    alone and refused a plan the engine accepts however the default evaluates.
    It now asks per row: refuse where the key spans a column this row writes to
    an unevaluable default **and** spans no column this row writes to NULL.

    The shape is only reachable through DECISIONS 116 — a foreign key
    referencing some unique key rather than the primary key — because rows are
    keyed by one column (ADR-0004), so a table with a composite primary key
    carries no `data:` block and can never be a `DeleteRow`'s parent. The live
    test says so, since a reader will otherwise try to build the fixture the
    obvious way and find it refused.

<a id="decision-335"></a>

335. **The pre-delete probe sees every key the delete will meet: the catalog's,
    the ones this session cannot count through, and the ones this plan adds.**
    Three ways the probe answered `0` for a delete the engine — or the plan's
    own next statement — would refuse. Each measured on 18.6.

    **A referencing table the session cannot read is not one with no rows.** A
    role with `DELETE` on the parent and no `SELECT` on the child gets
    `permission denied` from the count, which the probe runner reports as
    *unchecked* and `apply` walks past — while the engine's own key still sees
    the child and refuses the delete. Unreadable, read as absent, in the one
    place this project has a rule about it. 333's refusal over
    `row_security_active` now also asks `has_table_privilege(cl.oid,
    'SELECT')`: the two are one question — can this session count that table
    — and one probe.

    **A `DEFAULT` whose default is NULL is a NULL the probe compares.**
    `constant_default` excluded it — "it references no row", which is true and
    is 329's wrong conclusion over again: `None` there means *cannot compare*,
    so a child sent back to a NULL default kept its stored reference in the
    count and the ordinary update-then-delete plan was refused, while the same
    plan spelled `null:` was allowed. `p.col = (NULL)` is UNKNOWN and the row
    leaves the count. `is_null_default` went with it, its two callers having
    become dead the moment a NULL default was a literal.

    **A foreign key this plan adds is a constraint row, not a special case.**
    `DeleteRow` runs at rank 12 and `AddForeignKey` at 13, so the delete runs
    against a catalog that does not hold the key, the probe counted nothing,
    and the `ALTER` that follows validates every stored child row and fails on
    the one the delete just orphaned:

    ```text
    DELETE FROM parent WHERE code = 'old';                       -- succeeds
    ALTER TABLE child ADD FOREIGN KEY (parent) REFERENCES parent; -- 23503
    ```

    In a staged apply the delete has committed by then. The planned key is
    built as a synthetic `pg_constraint` row — `conrelid`, `confrelid`, the
    key arrays resolved through `pg_attribute`, `contype = 'f'`,
    `conparentid = 0` — and `UNION ALL`ed beside the stored rows in every probe
    that reads them. So every rule written for a catalog row reaches it with no
    second copy: `ONLY` by `relkind` (334), the exclusions and arrivals, the
    hidden-children and unprobeable refusals. The live test's negative half is
    exactly that: the same plan with the child row undeclared, where the
    exclusion for rows the plan deletes takes it out of the count through the
    planned key as it would through a stored one.

    Two things vanish on purpose. A key on a column this plan adds (rank 8)
    resolves no attnum and the row disappears — a column the database does not
    have cannot hold a stored reference, the same rule the arrivals use. And a
    key on a child this plan creates has a NULL `to_regclass` and disappears
    too; such a child has no stored rows, and its inserted ones meet the
    engine's own key, created with the table at rank 7, before the delete —
    the guard refuses there. The synthetic row's `conname` is one no user could
    write, so a replaced key's new shape cannot be filtered out by `gone_keys`
    as the old one it drops.

    The delete's own guard keeps reading the catalog alone. The planned key is
    not there when the guard runs either, but the probe has already refused the
    plan for it before the first statement, which is the only place a staged
    apply can still be stopped; in the guard it would be one more reason to
    abort after the plan is half applied.

<a id="decision-336"></a>

336. **A key this plan adds on a column it also adds is counted through the
    value the column is added with, and a NULL the plan writes decides the
    tuple before an unevaluable default does.** Two ways the pre-delete probe
    and its guard disagreed with the engine, both measured on 18.6.

    **The column the database does not have holds a reference the moment it
    exists.** 335 let a planned key on a planned column vanish — "a column the
    database does not have cannot hold a stored reference" — and that is true
    of the catalog and false of the plan: `ADD COLUMN … DEFAULT 'old'`
    backfills every stored row, and the plan runs it at rank 8, the delete at
    12, the key at 13:

    ```text
    ALTER TABLE child ADD COLUMN parent text DEFAULT 'old';
    SELECT parent FROM child;                      -- 'old', every row
    DELETE FROM parent WHERE code = 'old';         -- succeeds
    ALTER TABLE child ADD FOREIGN KEY (parent) REFERENCES parent(code);
      ERROR 23503: Key (parent)=(old) is not present in table "parent"
    ```

    In a staged apply the delete has committed by then. The synthetic
    constraint row cannot carry such a key — it is built from `pg_attribute`,
    and there is nothing to build from — so the question is asked from the
    plan alone, in `planned_key_probes`: the count of stored child rows whose
    tuple, with the planned column as the default it is added with and every
    other column as the stored cell, is the deleted row's. `DEFAULT NULL` and
    no default at all are a NULL in the tuple and the engine says it
    references nothing (`p.code = (NULL)` is UNKNOWN, 334, 335); no branch
    here repeats it. The rows the plan itself deletes, updates and inserts in
    that child get the same exclusions and arrivals the catalog keys get,
    spelled statically because the key's columns are known. A default the
    probe cannot evaluate — backfilled, or written by one of the plan's rows
    to a column of the key — is refused as 124 asks, counted as the rows it
    reaches; and the hidden-children refusal (333, 335) names such a child
    outright, since no constraint row, stored or synthetic, would find it.
    `ONLY` by `relkind`, still, through the same `query_to_xml` assembly.

    **A NULL anywhere in the key decides it.** 334 made the *refusal* over
    unevaluable defaults ask per row whether the same key holds a NULL the
    same row writes. The *count's* exclusion did not: its guard gave up on the
    unevaluable column first, so a stored child row this plan updates to a
    NULL in one column of the key and to an unevaluable default in another
    stayed counted with its stored tuple, and the ordinary update-then-delete
    plan was refused for a row the engine will not check at all (`MATCH
    SIMPLE`). The arm for a written NULL now comes first in `guarded`, for the
    exclusion and the arrival alike. The reachable shape is a default changed
    to an expression in the same plan, with the declared row omitting that
    cell: the update writes `DEFAULT`, which the probe cannot compare, beside
    the NULL, which it can.

<a id="decision-337"></a>

337. **A planned key is backfilled on the referenced side as well, and a NULL
    the probe already holds is a NULL however it was spelled.** Three ways
    336's answer was one column, or one spelling, short. Measured on 18.6.

    **The parent's new column is backfilled too.** 336 asked about a key this
    plan adds on a column it adds to the *child*. A key into a column it adds
    to the *parent* is the same hazard from the other side: the deleted row
    holds the default in the new column — as does every other parent row —
    and a child holding that value references the deleted row and every
    survivor alike:

    ```text
    ALTER TABLE parent ADD COLUMN alt text DEFAULT 'x';
    DELETE FROM parent;                                   -- every row
    ALTER TABLE parent ADD CONSTRAINT parent_alt UNIQUE (alt);
    ALTER TABLE child ADD FOREIGN KEY (ref) REFERENCES parent(alt);
      ERROR 23503: Key (ref)=(x) is not present in table "parent"
    -- and with one parent row left in place: the key is added
    ```

    So `planned_key_probes` spells the parent side of each column the same
    way it spells the child side — the stored cell, or the backfill — and
    subtracts the child rows a parent row this plan does not delete still
    satisfies, because the engine counts those as referencing the survivor.
    The unique key the planned foreign key needs there is not the probe's to
    check: an `ADD UNIQUE` over two survivors fails loudly, inside the
    transaction. `spans_a_planned_column` asks about both sides, and it is
    the one predicate the synthetic rows, the hidden-children refusal and the
    planned-key probes share, so the next side cannot be forgotten by one of
    them alone.

    **A column an insert omits and the table gives no default is a NULL the
    probe knows.** The plan carries every such column in the insert's `types`
    (136), and `pbps-mssql` reads them as arriving at NULL (117). This
    dialect recorded only the cells the row spells and the defaults it takes,
    so an omitted no-default column of a key was invisible to the exclusion,
    the arrival and the refusal over unevaluable defaults — which then
    refused an insert leaving a sibling column of the same key to such a
    default, for a tuple `MATCH SIMPLE` never checks. Recorded as `NULL`, the
    value the engine puts there.

    **`NULL::text` is NULL.** The backfill of a planned column declared
    `DEFAULT NULL::text` reached the probe as `(NULL::text)`, and the test
    for "this side is a NULL" compared the string to `NULL`. `rows::unwrapped`
    is the one place a spelling is reduced to its value, and every such test
    in the probe now goes through it — the third time a NULL was compared by
    its spelling in this file (329, 334).

<a id="decision-338"></a>

338. **A surviving parent row is the row this plan leaves there.** 337's
    survivor check read the parent rows as they stand — the stored cells and
    the backfill — and the plan writes to them before the delete runs
    (`order_key`: updates and inserts at rank 11, the delete at 12). An update
    that moves the surviving row off the backfilled value leaves the child
    referencing no row, and the key fails on it after the delete has
    committed; a parent row the plan inserts with that value is a survivor the
    engine accepts, and a check that could not see it refused a valid plan.
    Both by the plan's own apply, which is the measurement.

    So the survivor's side of each referenced column is spelled as the plan
    leaves it — `CASE q.<key> WHEN <row> THEN <after> … ELSE <stored or
    backfill> END` for the rows an update sets it in — and the rows the plan
    inserts on the parent are constant tuples `OR`ed beside the stored
    survivors, where every referenced column is one the insert spells, leaves
    to a known default, or the plan backfills. An inserted row whose
    referenced column the probe cannot spell is not a survivor it can see,
    and an update to an unevaluable default reads as NULL there: both are the
    over-counting direction, a refusal the engine would not have made, and
    the deleted row's own updates do not outlive it. The child side already
    saw its own updates and inserts (336); this is the same rule on the other
    table.

<a id="decision-339"></a>

339. **A backfilled literal is compared through its column's type, and an
    identity column is a backfill no probe can evaluate.** Two more things a
    column this plan adds holds once the `ADD COLUMN` has run, both measured
    on 18.6.

    **Through the type.** 336 spelled a planned column's backfill as the
    literal in parentheses, which is right against a stored column — the
    engine coerces the unknown literal to the column's type — and wrong
    against another planned column: two unknown literals compare as text, so
    `'2026-01-02' = '01/02/2026'` is false while the two are one `date`, and
    a child added with the second spelling was reported as referencing
    nothing. `CAST((literal) AS <normalized type>)`, the cast this crate
    makes everywhere else a default is compared (323, 329). A NULL stays
    bare, so that it reads as one (337). This is the fifth instance of the
    PITFALLS shape "both sides compared as text, only one of them through
    the engine", and it arrived in code written *after* the shape was
    recorded.

    **The identity.** A column added `GENERATED … AS IDENTITY` has no
    default and was read as backfilled NULL. The engine hands every stored
    row a value from the sequence during the `ADD COLUMN` — `1`, `2`, … —
    and a key from it into a parent whose row `1` this plan deletes fails
    after the delete (23503). Recorded as `identity` on the added column and
    treated as a backfill the probe cannot evaluate: refused as 124 asks,
    named as "its identity, assigned to every stored row" rather than "its
    default", because the remedy differs — there is no value to spell.

<a id="decision-340"></a>

340. **A planned key on a column this plan retypes compares the converted
    values, and a session that can read the columns the count reads can
    count.** Two more from review; both measured on 18.6.

    **The retype runs first.** `ALTER COLUMN … TYPE` is rank 9, the delete
    12, the key 13. A child `numeric(5,2)` holding `1.04` and the doomed
    parent's `1.00`, both narrowed to `numeric(5,1)` by the plan, are one
    value `1.0` when the key is validated — and the key fails on it — while
    as stored they are two, and the synthetic constraint row compared the
    stored ones. `AsStored` now records the columns the plan retypes with
    their destination type, `spans_a_planned_column` routes a planned key
    over one of them to `planned_key_probes` beside the added columns, and
    each such side is spelled `CAST(<cell> AS <normalized type>)`, on the
    child and the parent alike. Nothing is done for a *stored* key over a
    retyped column: the engine revalidates it in the `ALTER` itself, which
    fails loudly inside the transaction, before the delete.

    **Column grants count.** The hidden-children refusal (335) asked
    `has_table_privilege(cl.oid, 'SELECT')`, and refused a deploying role
    granted `SELECT` on the key's columns alone — whose count runs and
    answers, measured, because the generated statement reads only those
    columns and the row key of a child whose rows the plan names. The
    refusal now asks exactly that: table-level `SELECT`, or column-level
    `SELECT` on every column of the key (`has_column_privilege` over
    `conkey`) and on the row key where the plan deletes or updates rows of
    that child. The planned-key children are asked the same question over
    the columns their static count reads. A role with neither is still
    refused, and a `count(*)` that reads a column it may not is still
    `42501`, which the runner reports as unchecked — the question the
    refusal answers is whether that will happen.

<a id="decision-341"></a>

341. **The survivors of a retyped referenced column are asked too, and every
    literal the probe compares to another literal goes through the column's
    type.** Two more from review, both a valid plan refused.

    **Retyped survivors.** 340 converted the doomed row's side of a planned
    key over a retyped column and 338 asked the survivors only where the
    referenced column was *added*. Two parent rows `1.04` and `1.00` both
    narrow to `1.0`; the plan deletes the first and adds the unique key and
    the foreign key after — the child's converted `1.0` references the
    survivor and the engine takes the plan whole, while the probe attributed
    the child to the doomed row. The survivor check now runs where the
    referenced column is added *or* retyped, and its side is spelled through
    `converted` like the doomed row's.

    **Literal against literal.** A parent row this plan inserts and a child
    row it inserts, or an update's after-value, reach the survivor check and
    the arrivals as the literals the plan carries, and two unknown literals
    compare as text (339). The engine's own answer is by the column's type:
    `1.00` and `1.0` are one `numeric`, both read back as written, and a key
    from the second into the first holds. `AsStored` now keeps the type every
    row change carries for a column (`InsertRow::types`, `UpdateRow` and
    `DeleteRow`'s `types` and `after_types`), `final_type` answers with the
    retype, the added column's type, or that, and `typed` wraps a literal in
    `CAST(… AS <normalized type>)` wherever the other side may also be a
    literal. A NULL stays bare (337). What no row change types — the row key
    — stays a bare literal, which the engine coerces against a column and
    compares as text against another bare literal; a planned key from one
    inserted row's key column to another's is the residue, and its two
    spellings of one value would have to both read back as written, which
    for a key column is what ADR-0004's spelling rules already forbid.

    The `date` version of this the review proposed — `'2026-01-02'` beside
    `'01/02/2026'` — is not a plan this tool can carry: the row's
    postcondition refuses a value the engine stores in another spelling
    (137), so it cannot have been the test. `numeric` without a typmod is,
    because both spellings survive the round trip.

<a id="decision-342"></a>

342. **A probe answers in `int4`, the width the runner reads; and a table the
    session may not reach through its schema is one it cannot count.** Two
    more from review, measured on 18.6.

    **`int4`.** `deploy::preflight` reads every probe's count with
    `try_get_at::<i32>`, and the driver does not widen: an `int8` column is
    an error deserializing, which the runner reports as *unchecked* and walks
    past. Every probe of this dialect answered `count(*)::bigint`. The CLI
    does not route this dialect yet (step 10), so no plan has met the runner
    — but the probe is this step's deliverable and the runner its only
    reader, and a contract that holds only until it is first used is not
    one. Every probe now casts its whole answer `::int`, the type SQL
    Server's `COUNT(*)` already has, and the live suite's `counted` reads an
    `i32` so that every probe in it is checked at the runner's width; the
    cast has to wrap the whole `a + b` — `a + b::int` casts `b`. A count
    above `int4` is a count of more than two billion referencing rows, and
    an error there is the right answer.

    **Schema `USAGE`.** 335 and 340 asked `has_table_privilege` and
    `has_column_privilege`, by oid — and by oid the answer is `true` for a
    role that cannot name the table at all: `has_table_privilege` says
    nothing about the schema, and `SELECT count(*) FROM other.child` is
    `permission denied for schema` for a role granted `SELECT` on the table
    and no `USAGE` on the schema. Measured; the by-name form of the same
    function fails the same way, which is why the catalog form was in use.
    `has_schema_privilege(cl.relnamespace, 'USAGE')` is now the first term of
    "can this session count that table", for the catalog's children and the
    planned ones alike. Unreadable, once more, was about to read as absent.

<a id="decision-343"></a>

343. **A child this plan creates is a child whose arrivals are counted; a key
    column an insert leaves to the engine is refused; and a probe's answer is
    clamped before it is narrowed.** Three more from review, measured on 18.6.

    **The created child.** 335 let a key on a child this plan creates vanish
    with the reasoning that "its inserted rows meet the engine's own key,
    created with the table at rank 7, before the delete". That is not how the
    differ plans it: a new table's foreign keys are split out of the `CREATE`
    into `AddForeignKey`, which sorts after the deletes (rank 13), and its
    rows into `InsertRow`, which sorts before them (rank 11). Measured in
    that order, the table is created, the row inserted, the parent row
    deleted, and the key then fails on the orphan. 335's sentence is
    corrected here, not there: the created child now takes the planned-key
    path, named as the plan names it, with a stored count of `0` and the
    arrivals of its inserted rows counted like any child's.

    **The identity the insert leaves alone.** `InsertRow::types` leaves
    identity columns out on purpose — the engine owns them (94, 117) — and
    337 recorded a NULL for what is in `types` and omitted, so an identity
    column of a key was neither a value the probe compared nor a default it
    refused, and a key reaching it was not asked about at all. The sequence
    hands the row a value that may be the deleted row's key. So a column of
    a key that an inserted row neither spells nor leaves to a default the
    plan carries is refused as a write the probe cannot evaluate: in the
    catalog path, by asking the catalog whether a key reaches beyond every
    column the plan recorded for that row; in the planned-key path, by
    asking the same of the key's columns in Rust. Named "left to the engine
    to assign", because there is no value to spell — the remedy is a NULL
    in another column of the key, or a different key.

    **`LEAST(…, 2147483647)`.** 342 narrowed every answer to `int4`, and an
    answer past `int4` is an out-of-range error at the cast — which the
    runner reads as *unchecked* and walks past, the one thing 342 was
    fixing. The answer is clamped first: two billion referencing rows and
    two billion and one are the same refusal.

<a id="decision-344"></a>

344. **A foreign key whose delete action will not run in this session is not
    one the delete meets.** A foreign key on this engine is triggers, and
    `ALTER TABLE parent DISABLE TRIGGER ALL` stops them while `pg_constraint`
    goes on saying validated and enforced — **measured on 18.6**, the parent
    row then deletes with its child sitting there; the same under
    `session_replication_role = replica`, where `O`-mode triggers do not
    fire. It is the parent-side trigger for the *delete* (`tgrelid =
    confrelid`, `tgtype & 8`) that decides: with only the update-action
    trigger off the delete is still refused, and with the child's own
    triggers off it is too. Introspection already leaves such a key out of
    the model as one whose checks are not running, and the SQL Server probe
    skips `is_disabled` keys (151); this dialect's probe and guard counted
    through it and refused a delete the engine takes.

    Every read of the catalog's keys — the count and the guard, the
    hidden-children refusal and its RLS twin in the guard, the unevaluable-
    default refusal — now asks `DELETE_ACTION_FIRES`: no parent-side delete
    trigger of the constraint that this session will not run, spelled from
    `tgenabled` and `current_setting('session_replication_role')` the way
    the engine decides it. The synthetic rows for planned keys carry `oid =
    0` and have no triggers, so the same predicate passes them; `oid` joined
    `CONSTRAINT_COLUMNS` for this. `convalidated` stays out (320): a `NOT
    VALID` key's triggers run.

    The child left behind is the operator's: a key with its delete action
    off is a key the operator switched off, and the engine's answer to the
    delete is the one this tool relays.

<a id="decision-345"></a>

345. **Every key this plan adds is asked about from the plan; the synthetic
    constraint row is gone.** Two more from review, and the end of a line.

    335 made a planned key a synthetic `pg_constraint` row beside the stored
    ones, so that every rule for a catalog row would reach it without a
    second copy. Five rounds then found what a catalog row cannot say: a
    column this plan adds (336), one it adds to the parent (337), the parent
    rows the plan writes before the delete (338), a column it retypes (340),
    a child it creates (343) — and each was routed to `planned_key_probes`,
    the static path that spells the key from the plan, until this round's
    P2: a planned key on columns the plan leaves alone, where two parent rows
    share the referenced value today, the plan deletes one and adds the
    unique key and the foreign key after, and the survivor still holds the
    child's value. The engine takes that plan; the synthetic row could only
    say "this child's value is the deleted row's". The survivor check (338,
    341) is what the catalog path lacked, and putting it there would have
    meant the second copy 335 was avoiding. So every planned key now takes
    the static path, the survivor check runs for every one of them — cheap
    against a key the engine keeps unique, decisive where it does not yet —
    and `AsStored::constraints`, `CONSTRAINT_COLUMNS` and the synthetic row
    are removed; every catalog read is of `pg_catalog.pg_constraint` alone.
    What the static path has that the catalog path had — `ONLY` by
    `relkind`, the exclusions and arrivals, the hidden-children refusal by
    name and the columns its count reads, the unevaluable-default refusals —
    is what the rounds above put there.

    **And a row arriving against a backfill the probe cannot evaluate.** 339
    refused the stored rows a parent-side identity or expression backfill
    reaches, and the child row the plan *inserts* was counted nowhere: its
    tuple against the parent's backfill is a NULL placeholder in the count,
    and it was not among the refused. If the sequence hands the deleted
    parent row the value the child spells, the key fails after the delete.
    Such a row now joins the refusal, unless it writes a NULL to a column of
    the key, which references nothing however the backfill evaluates (334).

<a id="decision-346"></a>

346. **The delete's guard asks about the referencing relations, not the
    catalog's copies of their keys.** 334 filtered `con.conparentid = 0` out
    of the count and the hidden-children probe: a partitioned child holds one
    constraint row per partition beside its own, the count scans the
    partitioned relation once, and a partition's policy does not apply to a
    scan of its parent — **measured on 18.6**, a role the leaf's policy hides
    every row from still counts the row through the parent, while
    `row_security_active` is true for the leaf. The guard's own RLS check
    (`a_hidden_child`, 329) was written before 334 and never got the filter,
    so it asked every row, met the leaf's copy, and refused a delete whose
    count was complete — after the plan's earlier statements had run. The
    same filter, in the one place it was missing: the guard and the probe
    are the same question, and this is the sweep 333 asked for, a round
    late.

<a id="decision-347"></a>

347. **Whether a written row's tuple holds a NULL is decided from that row,
    not from the table.** 336 decided "this key references nothing" once per
    planned key, from the backfills: a column the plan adds with `DEFAULT
    NULL` on either side made every stored row's tuple a NULL, and it is.
    A row the plan *writes* is not every stored row: it may spell a value
    into that column, and its tuple then holds no NULL. Against a parent
    backfill the probe cannot evaluate — an identity, an expression — such
    a row may be arriving on the deleted row, and it was let through on the
    stored rows' answer; in a staged apply the insert and the delete commit
    before the key says so.

    `tuple_null` now answers per row: the cell the row spells, or the
    backfill where it spells none, over every column of the key, with a
    NULL backfill on the *parent* side still deciding for every row — the
    deleted row's own tuple holds it. It replaces the written-NULL test in
    the update and insert loops, so a row that spells nothing into a
    NULL-backfilled column is still a NULL there; and a stored row the plan
    updates to a value there, against a parent backfill the probe cannot
    evaluate, joins the refusal beside the arriving ones — 345 had only the
    arrivals. The stored rows the plan leaves alone keep the table-wide
    answer, because for them it is the row's answer.

<a id="decision-348"></a>

348. **A stored row's tuple is read from the row, too.** 347 left the stored
    rows the plan does not write on the table-wide answer, "because for
    them it is the row's answer" — and it is not. A stored column of the
    key can hold NULL in one row and a value in the next; under
    `MATCH SIMPLE` the first references nothing, whatever backfill the
    parent side gets. **Measured**: the parent gains an identity column,
    every child `ref` is NULL, the parent row is deleted, the key is added,
    and the engine accepts it. The refusal over "every surviving stored
    row" refused that valid plan.

    The refusal's stored-row term now reaches only rows whose stored
    columns of the key `IS NOT NULL` — the row's own tuple, as 347 reads a
    written one — and leaves out the rows the plan rewrites in a column of
    the key, which are counted as written (refused, arriving, or narrowed)
    and were being counted twice. A backfilled column stays table-wide: its
    value is one value for every row, and 336's `backfill_null` already
    answers it.

<a id="decision-349"></a>

349. **A cell an update leaves alone is part of the tuple the update writes.**
    347 and 348 read a row's NULL from the row — the cells the update
    spells, or the stored columns — and `UpdateRow.unchanged` was read by
    neither: the declared cells the row already holds, which the statement
    holds the row to before and after it runs (136). A composite key with
    one column left alone at NULL and another written to a default no probe
    can evaluate is a tuple with a NULL in it, and under `MATCH SIMPLE` it
    references nothing however the default evaluates. **Measured**: the
    update to the default, the delete, and the key are all accepted with
    that NULL sitting there; the refusal over unevaluable defaults, and the
    planned key's own per-row refusal, both refused the valid plan.

    `Moved::held_null` records, per updated row, the unchanged cells that
    are NULL — declared NULL, or left to a default that is NULL — and both
    decisions read it: `nulls_of` beside the NULLs the row writes, and the
    planned key's `tuple_null` between the written cell and the column's
    backfill. Only NULLs are carried: a value left alone is a stored cell
    the count reads from the table, and the refusal has nothing to learn
    from it.

<a id="decision-366"></a>

366. **A parent row this plan inserts meets an arriving child under the
    referenced column's collation.** 353 spliced the referenced column's
    collation into the planned key's count where a stored parent column met
    a stored child column, and left a literal to take the column's
    collation on its own — which it does against a column, and not against
    another literal: the survivor check of 338 compares the tuple a parent
    insert spells with the tuple a child insert spells, two literals, under
    the database's collation. **Measured** on 18.6: with both columns under
    one nondeterministic case-insensitive collation, a child `'A'` inserted
    beside a parent `'a'` references it and the key is added once the
    stored `'A'` parent is gone, while `'a'::text = 'A'::text` on its own is
    false — so a valid plan was refused, the arriving child attributed to
    the doomed row and to no survivor. (345's refusal of a key between
    *different* collations, one nondeterministic, stands; here both are
    one.) The inserted-survivor comparison now carries the mark whenever
    the referenced column is a stored one, and the child-insert term of the
    count, which stood outside the engine-assembled body, is assembled the
    same way — only where it carries a mark, so that a probe with no
    collation to ask about keeps its plain text. Against a stored child
    column the literal already took a collation, the child's; where the two
    columns' collations differ the engine accepts the key only when both are
    deterministic, under which equality is one answer, so nothing changes
    there.

<a id="decision-468"></a>

468. **SQL Server delete counts refuse active row filters and unreadable policy
     metadata (issue #208).** Measured on SQL Server 17.0.4075.5, a FILTER
     predicate hides a referencing child from `SELECT COUNT(*)` while the
     parent's `ON DELETE CASCADE` still deletes it and `ON DELETE SET NULL`
     still changes it. Both preflight and the emitted delete guard use the
     same counting statement, so both refuse before trusting that count.

     Unlike PostgreSQL's intrinsic owner bypass (329), SQL Server applies an
     enabled FILTER predicate even to dbo. An arbitrary predicate's current
     result is not proof that it admits every row; the refusal therefore
     follows enabled FILTER predicates, without an owner exemption. Disabled
     policies and BLOCK predicates do not filter SELECT and retain the normal
     count. Disabled foreign keys and keys removed earlier by the plan remain
     outside the probe's catalog read; the execution guard sees the keys left
     after those statements actually run.

     A policy can live outside the child's schema. Measured, child DML plus
     schema VIEW DEFINITION exposes the FK but hides that policy. An empty
     policy catalog is therefore trustworthy only with database VIEW
     DEFINITION and no effective object/schema metadata DENY, using the same
     visibility reasoning as key impacts (460). Check this before discovering
     FKs: a parent-only deployer sees neither the FK nor the policy and can
     still cascade into the hidden child. Every delete count therefore needs
     this catalog visibility, including one that ultimately finds no retained
     FK. A policy hidden by a direct or role-inherited DENY is refused rather
     than treated as absent; an owner whose effective grant overrides a DENY
     is not refused merely because the DENY row exists.

     Live regressions measure both referential actions and inspect the child
     before rolling back a deliberately attempted delete. They pin preflight
     and execution refusals, cross-schema policy visibility, direct and
     role-inherited metadata denials, an invisible referencing key, unfiltered
     counts, and deletes with disabled or explicitly removed keys. Reverting
     the shared count change makes the regressions fail by accepting a
     filtered zero. Moving only the visibility check back after FK discovery
     reproduces the parent-only deployer's silent cascade.

<a id="decision-470"></a>

470. **SQL Server's delete guard excludes the doomed row from its own
     self-reference.** Measured on SQL Server 17.0.4075.5, a row whose foreign
     key points at itself can be deleted: the one statement removes both
     sides of the reference. The preflight already excluded planned deletes,
     but the execution guard counted that row before deleting it and refused
     a valid plan. This is the SQL Server counterpart of decision 330.

     `still_referenced` now supplies a per-child exclusion only where
     `fk.parent_object_id = fk.referenced_object_id`. It excludes exactly
     `ch.<plan key> = @key`, using the same native comparison and parameter as
     the parent lookup. The plan's key identifies the row even when the FK
     references another unique key, including a composite one. Identifier
     quoting and SQL literal escaping remain separate because the exclusion
     is text within the generated child-count statement.

     A different row in the same table still counts, as does a row with equal
     key spelling in another table. The complete metadata and row-filter
     checks of 468 precede this count unchanged, and retained child ranges
     still use `HOLDLOCK`. Plan-wide exclusions and arrivals remain the
     preflight's responsibility.

     Live regressions compare direct and emitted deletes, then add another
     self-referencing child and an external CASCADE child with the same key.
     They cover a primary-key reference, a composite unique-key reference,
     and quoted identifiers and values. Both negative cases must raise the
     pbps guard before the engine constraint or cascade, and effects are
     read before rollback. Restoring the old guard makes both tests fail by
     refusing the self-only delete.

<a id="decision-489"></a>

489. **A referential-action lock follows the engine's inheritance boundary.**
     DECISIONS 451's trigger closure already follows ONLY for foreign-key
     actions, but its lock recursively included plain inheritance children the
     action cannot reach (#438). Measured on PostgreSQL 16.15 and 18.6, another
     session's SHARE lock on such a child delayed the guard even though the
     cascade never wrote it. The deployment role needs no privilege on that
     child for PostgreSQL's recursive lock; the defect is unnecessary blocking,
     not a missing child privilege.

     For an ordinary action target, take `LOCK TABLE ONLY ... IN ROW EXCLUSIVE
     MODE`. For a partitioned target, retain the existing recursive lock as
     one server statement. Both engines refuse plain inheritance from a
     partitioned table or one of its partitions, and refuse a partitioned
     table that also inherits from an ordinary table. Thus a recursive
     partition lock contains no plain-inheritance descendants to exclude.
     Replacing it with an enumerated list of separately locked partitions
     would introduce a gap without narrowing the legal relation set. A regular
     relation cannot become partitioned while retaining its oid; the existing
     post-lock name-to-oid check still rejects name replacement.

     The named row statement keeps its recursive lock: it has no ONLY and
     can write ordinary inheritance children. The action target's existing
     INSERT/UPDATE/DELETE/TRUNCATE privilege requirement is unchanged.
     Live controls on both engines hold an unrelated child's SHARE lock while
     the guard and cascade complete, inspect the writer's locks on both a
     regular action target and a partitioned one, and verify the untouched
     child's row. The same lock still blocks a direct write's guard. A new
     partition of the FK's referencing side times out on ATTACH while the
     guard holds its locks; ATTACH succeeds immediately after release. Restoring
     the old recursive regular-table lock makes the first concurrency assertion
     fail with lock timeout.

<a id="decision-511"></a>

511. **The delete count's children are found in the catalog, because that is
     where the probe finds them.**

     Before removing an undeclared row, `preflight::delete_probe` counts the
     rows still pointing at it, one `COUNT(*)` per table with an **enabled**
     foreign key into the parent, read from `sys.foreign_keys` at run time. It
     deliberately does not trust the declarations to list those children — a
     foreign key someone added by hand is exactly the one that will refuse the
     delete — so the readiness question cannot trust them either (issue #515).

     A child need not be declared, recorded, or even in a schema this project
     manages, and nothing else in the list reaches it: `Needed::ManagedTable`
     asks about the declared and recorded tables, and `Needed::Referenced` about
     the targets the declarations point *out* at, which is the opposite
     direction. Measured on 17.0.4075.5: with `app.t` declared `mode: exact` and
     an undeclared `app.unmanaged(id)` referencing it, a deployer holding
     everything else passed `doctor` with no gaps and the count failed with
     error 229.

     The discovery query matches the probe exactly — `is_disabled = 0`, because
     a `NOCHECK`ed constraint is not enforced and its child is not counted (144)
     — and runs only for a project that can remove a row at all.

     **Asked over the child's foreign-key columns, not its whole catalog**, the
     way an external target is (466). The count names a child only in the tuple
     the catalog gives it, and the fragments that name a child's *own* key are
     written only for a child the plan moves — which is a declared table, and
     therefore deduplicated out of this list before it is asked about. So
     everything left here is read through its key tuple and nothing else.
     Measured on 17.0.4075.5: a login holding `SELECT` on nothing but the
     foreign-key column is refused a plain `SELECT COUNT(*) FROM app.kid`
     (error 230, on a column the engine picks for `COUNT(*)` itself) and runs
     the count the probe actually writes. Demanding every catalog column would
     have reported a gap against an account that can run every statement the
     declaration produces, which is the over-demand this whole list exists to
     avoid.

     A child the managed question already asks about is not asked twice, which
     is also what makes that narrow column list right — **except when this plan
     moves it to another schema**. The guard a row delete carries
     (`preflight::still_referenced`) discovers the surviving keys and reads the
     child inside the delete's own transaction, and a table rename is
     `order_key` 1 while a row delete is 12: the transfer has already run and
     taken every permission on that object with it (512). The managed question
     answers for the source, and a child with no `data:` block has nothing in
     `data_gaps` to answer for its destination — so that child's destination
     schema is demanded here, and the dedupe drops only the children that stay
     put. A child this login
     cannot see produces no row and the report is silent about it: that is the
     boundary of a read-only check, not a gap it could print a `GRANT` for, and
     the count's own `VIEW DEFINITION` demands (505) report the visibility half.
