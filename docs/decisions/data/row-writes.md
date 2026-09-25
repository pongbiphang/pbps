# Reference data: row writes

How a row insert, update or delete is guarded and held to what the plan
recorded. Part of the [decision record](../../DECISIONS.md), which says how to
add an entry here.

<a id="decision-122"></a>

122. **A row `UPDATE` holds the row to what the plan recorded, and a row
    `UPDATE` or `DELETE` has to reach exactly one row.** The plan is reviewed
    against a recorded state, and the checksum pins that state up to the
    moment `apply` reads it — not to the moment each statement runs. A row
    another session changed or deleted in between was overwritten by the
    `UPDATE`, or missed by it with the statement counting as success, and
    the read-back then recorded the result as if the reviewed plan had done
    it. The differ now writes each updated column's type in the base state
    into `UpdateRow` (`types`, absent from older plans and read as empty,
    which holds the key alone), and the emitter puts each recorded cell into
    the predicate, compared by the very rendering that read it
    (`rows::read_expr`, chosen by that type) under a binary collation, so a
    change of case alone is a change, as it is to the drift check; a NULL
    as `IS NULL`; a cell at a literal default as the read-back compared it,
    and only where the read-back did — a default the engine would have to
    run, and a type without `=`, hold nothing, as in 117 and 94. A column
    the base does not have is not compared: its `before` is what this plan's
    `AddColumn` leaves there, which the engine may fill from the default.
    Both statements end in `IF @@ROWCOUNT <> 1 THROW`, in the same batch,
    so the transaction rolls back with the row named; a `DELETE` carries no
    recorded content (the pinned baseline holds it) and is held to the row's
    existence. Measured with an `AFTER` trigger on the table: `@@ROWCOUNT`
    after the statement is the statement's own. Holding the baseline read
    and the DML under one serializable transaction would close the same
    window for structure too, and is the shape to reach for if the row
    predicate ever proves too narrow; it reorganises every apply path, and
    the predicate is what the reviewed plan actually asserts.

    **Amended by 471: the type is no longer one of the things that hold
    nothing.** The comparison became one of text on both sides, so the types
    without an `=` are held like every other; a default the engine would have
    to run, and a column the base does not have, are still the whole list.

<a id="decision-129"></a>

129. **A row delete carries its own guard, under locks it keeps.** The probe
    counts before the first statement and the delete runs later; a child row
    committed in between was taken silently by `ON DELETE CASCADE`, after
    which the closing snapshot recorded the damaged state as a success. The
    delete now re-counts, inside its own transaction and with `HOLDLOCK` on
    the child scans, and throws if anything references the row. By then every
    insert, update and child delete of the plan has run, so *any* remaining
    reference is one the probe did not account for and the guard needs none
    of the plan's exclusions. Measured: with the guard's transaction open, a
    concurrent insert of a child row blocks until the delete commits, so the
    window the probe left is closed rather than narrowed.

    The guard and the delete are wrapped in a transaction of their own,
    because a staged apply runs each statement outside one (SPEC §7.5) and
    the range locks would otherwise be released before the delete they
    protect. Inside the transactional apply it merely nests. The `CATCH`
    rolls back and rethrows, so a failed guard never leaves a staged run
    holding an open transaction.

<a id="decision-130"></a>

130. **The key-alias guard resolves the plan's table name through the
    identities first.** `AlterColumnType` names the table as the plan leaves
    it, while the declarations and the rows read back are keyed by the name
    the database has now (`tables_under`). A table renamed in the same
    revision was found in neither, so a key column changing from `int` to
    `varchar` while a declared key `01` stood for a stored `1` passed the
    guard that exists to refuse exactly that. The mapping both collections
    were built through is now a named function, and the guard goes through
    it.

<a id="decision-132"></a>

132. **A row write holds itself to what it wrote.** The engine reporting a
    successful `INSERT` or `UPDATE` is not the same as the row being what the
    plan says: an `AFTER` trigger runs inside the statement and may rewrite
    the row or take it away again, and `DELETE` has the mirror shape — a
    trigger that puts the row back. The apply then read the result back,
    recorded *that*, and reported success, so `verify` was clean against a
    state nobody declared and every plan after it proposed the same change
    again. Each row statement now ends with a postcondition — the row exists
    and holds the cells the plan spelled, or for a delete is still gone — and
    throws otherwise, inside the write's own transaction, so the plan rolls
    back instead of blessing the result.

    Only the cells the plan spells are checked, by the rendering that reads
    them back (the comparison of 122). A cell left to a default has no value
    in the plan to hold the row to; a column the plan never names is the
    application's business. A connected plan cannot fail this on spelling
    alone — `plan --db` refuses a declaration the engine reads back
    differently before the plan exists (101) — but an offline plan carries no
    such promise, so a value the engine stores differently now stops here
    rather than being applied, recorded, and proposed for ever. The message
    names all three causes.

    The envelope is the delete guard's (129), now shared: `BEGIN TRANSACTION`
    / `TRY` / `CATCH` around the write and its checks, because a staged apply
    runs each statement outside a transaction and a postcondition that merely
    threw would leave the write it rejected committed. `SET IDENTITY_INSERT`
    goes off before the check can throw: it is a session setting, not a
    transactional one, and a rollback would leave it on for the connection.

<a id="decision-133"></a>

133. **A row write is held to the columns it left to their defaults too.** 132
    checked only the cells a plan spells, so an insert that omits a defaulted
    column, plus an `AFTER INSERT` trigger that rewrites *that* column,
    passed: apply recorded the rewritten value, reported success, and every
    plan after it proposed the row again — the same silence 132 closed, one
    column over. A column left to a **constant** default is now compared
    against that default, on the insert as on an update that sets a cell to
    `DEFAULT`. Anything the engine would have to run to answer — `NEWID()`,
    `getdate()`, `NEXT VALUE FOR` — is not asked: it has no value before it
    runs, and asking a sequence would consume one.

    `InsertRow` carries the type of each defaulted column for this, as
    `UpdateRow` has carried its updated columns' types since 122. The type is
    what says whether the comparison exists at all — `xml`, `text` and the
    spatial types have no `=`, and asking for one is an error rather than a
    false answer. An older plan carries no types and checks nothing here,
    exactly as an older `UpdateRow` holds the row to its key alone.

    **Amended by 471: the type no longer says anything.** The comparison is of
    text on both sides and has been for some time, so the six types without an
    `=` are held like every other. Only the *default* decides now.

<a id="decision-136"></a>

136. **A row write is held to the whole declared row, not to the cells it
    changes.** 132 and 133 checked what the statement spelled and what it
    left to a default; two columns were still nobody's. An `UpdateRow`
    carries only the columns that differ — restating the rest would make
    plan.sql claim a change that is not one — so an `AFTER UPDATE` trigger
    rewriting a declared cell the plan did not touch passed the check, and
    an insert that omitted a column the table gives *no* default left it to
    NULL without holding it there, so a trigger filling it passed too. Both
    ended the same way as before: the rewritten value read back, recorded,
    reported as success, and proposed again by every plan after.

    `UpdateRow` now carries the declared cells it leaves alone (`unchanged`),
    resolved by the omission rule, with their base types beside the changed
    columns'; the `SET` still names only what changes, but the stale
    predicate and the postcondition hold every declared cell. That closes a
    second gap with the same statement: a hand edit to an untouched cell
    since the plan was made now names itself as a stale baseline rather than
    being read back as the plan's own. `InsertRow` types now name every
    omitted column, and one absent from `defaults` is held to `IS NULL` —
    which needs no `=` on the type, so an `xml` column left to nothing is
    held where a defaulted one could not be. A plan made before either
    travelled carries neither, and checks what it always did.

    A non-key `IDENTITY` column is in neither set. It is never written by a
    row and never read back (94), so both sides resolve it to NULL and it
    compared equal — and holding it to NULL would have refused every update
    on the table, since the engine assigned it. The live round-trip caught
    this before the unit tests did: the fixture table has one.

<a id="decision-137"></a>

137. **A row write holds its cells by the rendering that reads them back,
    under a binary collation — an insert as an update.** 132 held an insert
    to its spelled cells with the engine's own `=`, which is the column's
    collation: a trigger folding `New` to `new` on a case-insensitive column,
    or adding a trailing space, was equal to it, and the read-back then
    recorded the rewrite as the plan's own — while an update had compared by
    `read_expr` under `Latin1_General_BIN2` since 122. `InsertRow.types` now
    names every non-key column, spelled ones included, and the insert's
    postcondition goes through the same comparison the update's does. A
    default is compared the same way, on both sides: the default converted
    to the column's type and then rendered as the column is, so a
    `'2026-01-01'` default on a `datetime2` column renders as the stored
    value does and the comparison stays about the value, not about how two
    types spell it. An older plan carries no type for a spelled cell and
    compares as it did.

<a id="decision-140"></a>

140. **A row update reads each cell by two types: the one the base recorded,
    and the one the column will have when the statement runs.** 122 held the
    update to what the recorded state held, and 136 extended that to the cells
    the plan leaves alone — both through one `UpdateRow.types`, the base
    side's, whose absence for a column doubled as "the base has no recorded
    cell here, so hold the row to nothing". That reading is right *before* the
    write and wrong after it. When one revision adds a column and populates it
    in the same declared row, `AddColumn` sorts at 8 and the row changes at 11,
    so by the time the `UPDATE` runs the column exists and holds the declared
    type — but the postcondition looked the column up in the base's map, found
    nothing, and held the new cell to nothing at all. An `AFTER UPDATE`
    trigger could rewrite exactly that cell, the apply would read the rewrite
    back and record it, and the next connected plan would propose the same
    update forever: the silence 132 exists to close, reopened for the one
    column the revision was about. The same shape hid a second case, since
    `AlterColumnType` sorts at 9: a column retyped in the same plan had its
    result compared by the rendering of the type it no longer had.
    `after_types` now carries the post-plan type wherever it differs from the
    base's — the added column and the retyped one — and the emitter resolves
    the precondition through `types` and the postcondition through
    `after_types` falling back to `types`, as two named lookups rather than
    one map consulted twice, so neither check can quietly borrow the other's
    type. Only the differing entries travel: a plan already carries the whole
    column list once per changed row, and twice is a cost with no reader.

<a id="decision-143"></a>

143. **A row delete is keyed *and* held to the row the plan recorded.** The
    baseline checksum pins the environment up to the moment `apply` reads it,
    and the `DELETE` runs later still — after the lock is taken, but an
    application session writes to these tables all the same. A key-only
    `DELETE` therefore removed whatever stood under the key when it ran, and
    `@@ROWCOUNT = 1` reported that as the reviewed row: an unreviewed loss the
    apply then recorded as its own result. Measured by reverting the predicate
    and rewriting the row from a second connection mid-statement — the delete
    succeeded and took the application's version with it. An update has held
    every declared cell since 136; a delete has more to lose, because what it
    removes cannot be compared afterwards. So `DeleteRow` carries the
    baseline's cells and the types they were read by, and the predicate is
    built by the same `recorded_cell` the update's precondition uses: each
    cell compared by the rendering that read it, a NULL as `IS NULL`, and a
    cell whose type has no comparison (`xml`, `text`, the spatial types)
    carried but not held — the same limit, in the same place, as an update's.
    A mismatch is `@@ROWCOUNT <> 1`, which already says "changed or deleted
    since the plan was made. Plan again."

    **Amended by 471: the type is no longer what leaves a cell unheld.** That
    limit was the native `=`, and the comparison became one of text, so a cell
    of a type without an operator is held by the rendering that read it like
    every other. One case remains, and it is narrower: a column *this plan
    retypes* whose old type cannot be spelled back from its own rendering —
    `image`, which has no conversion from text at all, and `geometry` and
    `geography`, whose text leaves out the SRID. All three measured. Unretyped,
    a cell of any of them is held.

<a id="decision-146"></a>

146. **A cell whose column this plan retypes is carried, and held by
    nothing.** `AlterColumnType` sorts at 9 and the row changes at 11 and 12,
    so by the time an `UPDATE` or a `DELETE` runs the engine has already
    converted the column — and the recorded text is the spelling the *old*
    type gave it. Measured on SQL Server 2025: a `decimal(5,2)` holding `1.50`
    reads back as `1` once the column is `int` (a truncation, not a rounding:
    `1.60` also becomes `1`), so a predicate holding the cell to `N'1.50'`
    matches nothing and the statement throws "changed or deleted since the
    plan was made" — in a staged apply after the conversion has committed.
    The obvious repair is worse than the fault: converting the recorded text
    into the new type is not the conversion the engine performed on the value,
    and `CONVERT(int, N'1.50')` is an error (Msg 245), so it would trade a
    false refusal for a hard failure. Neither type can compare the cell, so
    the precondition simply does not carry one for it — the same answer, on
    the same path, as a type with no comparison at all (`xml`, `text`, the
    spatial types). Both `UpdateRow` and `DeleteRow`: the update's
    precondition had the fault too, which 140 did not reach because it was
    about the postcondition. The postcondition is unaffected — it compares
    the *declared* value, and `after_types` already reads it by the type the
    column ends up with. The whole type is compared, not just its base:
    `decimal(5,2)` to `decimal(9,4)` renders `1.50` as `1.5000`, so a widening
    within one base type is no safer than a change of base.

    **Superseded by 149.** The reasoning above is right about every direct
    comparison and wrong about the conclusion: the conversion the engine
    performed *is* expressible, just not as "the recorded text in the new
    type". Carrying no type dropped the stale-row guard with the comparison.

<a id="decision-326"></a>

326. **The delete's own guard locks the parent row, where SQL Server's locks a
    range of the child.** The guard re-counts the referencing rows inside the
    delete's own statement, so a child committed between the probe and the
    apply cannot be cascaded away unseen (129); that count is only worth
    something if nothing can arrive after it. SQL Server takes serializable
    range locks over the child scan. This engine's own foreign-key machinery
    supplies something narrower and exact — **measured**, an `INSERT` into the
    child reports the statement it blocked in:

    ```text
    while another session holds SELECT … FROM parent WHERE id = 5 FOR UPDATE:
    INSERT INTO child VALUES (99, 5);
      ERROR: canceling statement due to lock timeout
      CONTEXT: while locking tuple (0,3) in relation "parent"
      SQL statement "SELECT 1 FROM ONLY "m4"."parent" x WHERE "id" = $1 FOR KEY SHARE OF x"
    ```

    So the guard takes `FOR UPDATE` on the one parent row, and a row arriving on
    it waits for the delete instead of racing it.

<a id="decision-328"></a>

328. **A row statement is a `DO` block, and its refusals are `RAISE EXCEPTION
    USING MESSAGE`.** The write and the checks that hold it to what the plan
    reviewed have to be one statement: a staged apply runs each statement
    outside a transaction (SPEC §7.5), so a check that merely raised would
    leave the write it rejected committed. **Measured**, a `DO` block that
    writes and then raises leaves the table as it was with no transaction open
    at all, and inside the transactional apply it nests, where the outer
    transaction still decides everything. There is no `CATCH`: this engine's
    failure is the whole block's, so nothing is left half-done for a later
    statement to find.

    `USING MESSAGE` rather than `RAISE EXCEPTION '<text>'`, because the second
    form's first argument is a format string and a `%` in a table name, a key
    or a value would be read as a placeholder. Measured, `USING MESSAGE`
    reports a `%` and a `\` untouched. One less thing to escape is one less
    thing to escape wrongly.

<a id="decision-329"></a>

329. **Every side of a comparison the engine will make goes through the
    engine's own type first, and every value a plan writes is one the probe can
    compare.** Five refusals of valid plans, all of the same shape: something
    pbps knew about a value was compared with something the engine knew, and
    the two were not the same thing. Each was measured on 18.6.

    * **A default is compared as the column stores it.** `numeric(5,2) DEFAULT
      1` stores `1.00` and `pg_get_expr` deparses it as `1`, so a text
      comparison of the stored cell against the declared expression was false
      for a row the engine had just written correctly — the insert's own
      postcondition rejected it. The declared side is now cast to the column's
      type before it is read as text, which is the same conversion the engine
      made on the way in, asked of the engine.
    * **A bare `true` is a literal.** This engine deparses `boolean DEFAULT
      true` as the one word, and a predicate that admitted NULL, quoted strings
      and numbers called it an expression. The cell was then read back as one
      nobody can tell from its default, so a hand edit that flipped it was
      projected as an omission and no plan proposed to put it back — drift the
      tool reported as convergence.
    * **A count filtered by a policy is not a count.** Row-level security
      filters a `SELECT`; this engine's referential actions ignore it.
      Measured, a session the policy filters sees `count(*) = 0` on a child
      table, deletes the parent, and the `ON DELETE CASCADE` destroys the
      hidden row — the exact silent loss the pre-delete probe exists to
      prevent. The probe now asks `row_security_active` about every referencing
      table and refuses rather than trusting a count it cannot complete. Not
      `relrowsecurity`: that is true for a table under a policy even in the
      session that owns it and sees every row, and refusing there would refuse
      a plan that is safe.
    * **An explicit NULL is a value the probe can compare.** A plan that
      unpicks a child's reference before deleting its parent writes `NULL` into
      the child, and the probe excludes rows this plan has already moved.
      Mapping `Value::Null` to "cannot compare" made the whole updated row
      uncomparable, generated no exclusion for it, counted its *stored*
      reference and refused the ordinary shape. `p.code = NULL` is UNKNOWN,
      measured, so a NULL tuple simply never matches the parent — which is the
      right answer, written down.
    * **`\x` is a `bytea`, not an empty string.** It is this engine's spelling
      of the zero-length value and what `pull --data` writes for one, and
      `decode('', 'hex')` is valid. An emptiness test in the emitter refused a
      declaration that `validate` had passed cleanly.

    The generated per-child statement is `SELECT (SELECT count(*) FROM …
    WHERE …) <arrivals> AS n`, and the parentheses are load-bearing: the
    arrival terms are added to the count, and written after the `WHERE` clause
    of an unparenthesised `count(*)` they are a syntax error. No test saw it
    until the NULL fix above, because every live probe until then had a plan
    that moved no child row, and the unit test that inspects the SQL does not
    run it.

<a id="decision-330"></a>

330. **The read of a defaulted cell asks the same question the write does, and
    the delete's own guard knows the row it is about to remove.** Two more of
    329's shape, one on each side of it.

    **The read-back's `at_default` was the column's comparison, not pbps's.**
    `emit::defaulted_cell` holds a cell to its default as text, byte for byte
    under `COLLATE "C"`, and through the column's type; `rows::query` asked the
    same question with a native `=` on the raw column. A collation makes those
    two different questions. **Measured on 18.6**, a `text` column under
    `und-u-ks-level2` holding `New` beside `DEFAULT 'new'`:

    ```text
    label = ('new')                      ->  t
    the same, as text under COLLATE "C"  ->  f
    ```

    The first answer marks the cell at its default, so `ObservedRow::as_seen_by`
    leaves it out of the read-back, the drift is not in the observed state at
    all, and neither a connected plan nor `verify` ever proposes to put the
    value back. 329 fixed the write half of exactly this and did not sweep the
    read half — which is `CLAUDE.md`'s own rule about sweeping every call site,
    failed on the very next call site.

    **A row that references itself is not a child that survives its own
    delete.** The delete's guard counts the rows still referencing the parent
    and runs before the `DELETE` in the same block, so a self-referencing row
    still points at itself when it is counted. **Measured**, the engine takes
    that delete without complaint, because the one statement removes both sides
    of the reference:

    ```text
    INSERT INTO t VALUES (5, 5);          -- t.parent REFERENCES t.id
    the guard's count before the delete:  1
    DELETE FROM t WHERE id = 5;           succeeded
    ```

    So the guard refused a plan the engine accepts. The exclusion is written
    into the generated per-child statement under `con.conrelid =
    con.confrelid` — the count already filters on `confrelid`, so that
    equality *is* the self-reference — and it takes out exactly one key, the
    one being deleted. Another row pointing at the doomed one through the same
    self-reference is a real child and is still counted; the probe already drew
    the line in the same place.

<a id="decision-331"></a>

331. **A guard for a native `=` outlived every native `=` it guarded, and the
    probe this dialect ported kept only half of what 124 asks for.** Two more
    of 329's and 330's shape.

    **`json` is compared like everything else.** `rows::comparable` answered
    "does this type have `=`?" and was asked in three places, because the
    comparisons those places wrote were native ones. 329 and 330 made every one
    of them a comparison of *text* — `CAST(… AS text)` on both sides, under
    `COLLATE "C"`, through the column's type — and the guard stayed. It is the
    shape `CLAUDE.md` calls "a guard whose reason has gone is a filter nobody
    re-reads", and the filter cost the read-back a whole type: a `json` cell
    with a literal default was never asked about, so it was read back as one
    nobody can tell from its default, `ObservedRow::as_seen_by` dropped it, and
    a hand-edited document was drift no plan proposed to settle.

    **Measured on 18.6**, both halves of why removing it is safe:

    ```text
    '{"a":1}'::json = '{"a":1}'::json   ->  ERROR: operator does not exist
    the read-back's comparison, as text ->  t for the default, f for an edit
    CREATE TABLE t (d json PRIMARY KEY) ->  ERROR: no default operator class
    ```

    The last line is what makes the sweep complete rather than hopeful. The
    native `=` this dialect still writes is on key and foreign-key columns —
    `WHERE "key" = E'…'`, the probe's tuple — and `json` cannot be one of
    those: with no default `btree` operator class it can carry neither a
    primary key nor a unique constraint, so nothing can reference it either.
    There is no type left for the guard to protect, and `Held::as_stored` no
    longer returns an `Option`.

    **A write to a default the probe cannot evaluate is refused here too.** 124
    established the rule and `pbps-mssql` implements it; this dialect's port of
    the pre-delete probe counted such a write as absent and stopped there. The
    hazard is 124's, one staged apply later: the insert or update runs first,
    commits for good, and the delete's own guard is the first thing to see the
    reference — so the deployment is half applied and the plan it was applying
    can no longer be resumed. The refusal is a second probe that *counts* the
    foreign-key columns in the catalog, for 124's reason: a probe that errors
    reads as "unchecked" to `apply`, which then proceeds.

    Two differences from the SQL Server statement, both already documented in
    this module. There is no `convalidated` filter, because a `NOT VALID` key
    here enforces the delete action in full and is not this engine's
    `NOCHECK`. And the key's columns are reached through
    `generate_subscripts(con.conkey, 1)` rather than a two-array `unnest`,
    which is grammar rather than a function and cannot be schema-qualified.

<a id="decision-332"></a>

332. **The key a write puts there is a cell, and is held to its exact spelling
    like every other one.** The only reason it was not among the cells
    `wrote_the_row` compares is that a `Row` carries it as the map's key, and
    that is a fact about a Rust type, not about the database.

    The postcondition used the key column's own `=`. **Measured on 18.6**, with
    a `text` key under `und-u-ks-level2` and an `AFTER INSERT` trigger that
    lowercases what was written:

    ```text
    INSERT ... VALUES ('New', 'a');   the trigger leaves:  new
    the postcondition with the column's `=`:               passes
    the same, as text under COLLATE "C":                   fails
    ```

    So the write reported success, `apply` recorded a key nobody declared, and
    the alias read then mapped the declaration's `New` onto the stored `new`
    and agreed with it for ever after — DECISIONS 132's hazard, reaching the
    one cell 132's own check did not cover.

    The literal needs no cast through the key column's type. `plan --db`
    refuses a declared key the engine would spell differently before anything
    is written (DECISIONS 101), so by the time the statement runs the key is
    the engine's own spelling — measured, the text comparison holds for an
    `integer` key written `E'1'` and a `numeric(5,2)` one written `E'1.50'`.
    It is the same coupling `recorded_cell` already relies on for values.

    **Three sibling comparisons keep the column's `=`, deliberately.** The
    `WHERE` clauses of the update and the delete ask "which row does this
    engine call this key", which is the engine's question to answer
    (ADR-0013 §5) — comparing those as text would refuse a plan built from the
    engine's own answer. And `gone_row` asks whether anything the engine calls
    the deleted row is back; there the looser comparison is the safer one,
    because a trigger reinserting `Old` under a case-insensitive collation must
    still be caught. Same operator, three different questions, and only one of
    them was wrong.

<a id="decision-442"></a>

442. **Deleted rows carry dropped baseline cells in a separate review map.**
    The UID intersection used to compare rows contains surviving columns only,
    so it cannot supply the cells of columns this plan drops (#126). Adding
    those cells to `DeleteRow::row` under their baseline names is ambiguous:
    an environment can skip a revision that drops `note`, then deploy a later
    revision that renames surviving `label` to `note`. The deleted row has two
    different baseline values that would occupy the same map key. A new column
    reusing a dropped name likewise has no claim to the old column's cell.

    `DeleteRow::dropped` therefore carries dropped cells under their baseline
    names for review only; `row`, `types` and `after_types` keep their existing
    surviving-column names and predicate semantics (143, 149). Both values can
    be serialized without a decorated name or an overwritten predicate cell.
    Emitters never inspect `dropped`. Primary keys remain separate and non-key
    identity values remain engine-owned, as in the existing row guards.

    The saved-plan version moves from 7 to 8 because older `Change` readers
    deny unknown fields. The new field defaults to an empty map, so changes
    with no dropped cells remain compact. The state and ids formats do not
    change. Regressions resolve the intermediate declaration revisions before
    diffing the original baseline against the final state, pin both name-reuse
    cases, and require both dialects to ignore dropped cells even when their
    names coincide with surviving predicate cells.

<a id="decision-445"></a>

445. **Reference-data writes do not authorize ambient PostgreSQL triggers.**
     Measured on PostgreSQL 18.6 with separate non-superuser roles: a principal
     with TRIGGER on a reference-data table, but no SELECT on a secret table,
     can attach an invoker trigger that copies the secret under the deployer's
     privileges. The approved row still matches and the old CLI records Apply.
     `unmanaged: ignore` or `warn` controls comparison, not this execution trust.
     Connected planning and apply now refuse an active non-internal trigger for
     the row operation unless its definition matches a recorded managed trigger
     and its function owner can SET ROLE to the deployer. That owner test means
     a less-privileged function owner cannot replace the invoker body after the
     check; a managed name or a current SECURITY DEFINER flag alone is no proof,
     since the owner can replace the body or change that flag without a table lock.
     External/transitive helper authentication remains separate (#319, #322).

     TRIGGER privilege also permits CREATE OR REPLACE of an existing trigger,
     measured even when the actor does not own the table. Take ROW EXCLUSIVE
     before authenticating and retain it through the write: it conflicts with
     trigger DDL, permits ordinary DML, and is available to an INSERT-only role.
     Authenticate in the old table names before a plan renames them, retaining
     catalog trigger/function identities within that transaction; recheck each
     emitted row target, including a newly created table, before executing it.
     The emitter supplies typed row-operation metadata so the runner never
     guesses a write from SQL text. A trigger the plan drops gets no allowance
     and must actually be absent at the write. Internal constraint triggers are
     engine machinery; disabled triggers and other events do not run that write.

     UPDATE OF uses the emitted SET columns, resolved through column uids before
     planned renames, rather than the UPDATE event bit alone. A trigger naming
     only untouched columns must not refuse a valid write. Generated columns
     also count when they depend on a SET column; measured on PostgreSQL 18,
     the presence of any BEFORE ROW UPDATE trigger makes PostgreSQL include all
     generated columns, even when that BEFORE trigger is disabled. The guard
     follows both rules, with positive/negative live controls on 16 and 18.

     An ordinary apply holds its locks through the enclosing transaction and
     ledger record. A staged row gets a transaction only around its guard and
     one already-atomic row statement, committing before the existing checkpoint;
     non-transactional DDL and the plan's per-statement commit contract remain.
     Bootstrap checks each created row target with no pre-existing allowance.
     The live regressions cover all three DML kinds, row/statement triggers,
     ignore/warn, both apply modes, no secret leak or success ledger on refusal,
     successful retry, managed-trigger execution, post-approval replacement and
     an independent connection blocked only until the write releases its lock.

<a id="decision-451"></a>

451. **The named table is not the write set: a row operation is guarded over the
     foreign keys whose actions write for it.** 445 authenticated the triggers
     of the table the plan names and its inheritance descendants. That is not
     where the write ends. **Measured on PostgreSQL 18.6**, with the same
     separate non-superuser roles as 445 and reproduced end to end through the
     CLI (#412): a deployer's `UPDATE` of a referenced UNIQUE value makes the
     engine write the referencing table for it, and an invoker trigger the
     attacker attached there with nothing but `TRIGGER` on it copies a secret
     the attacker cannot read — `current_user` inside that trigger is the
     deployer. PostgreSQL runs a referential action as the *referencing* table's
     owner, so the escalation is exactly the ordinary case where the deployment
     role owns the table the action writes. `unmanaged: ignore` and `warn` do
     not authorize it, for the reason 445 already gave: scope is comparison,
     not execution.

     The guard therefore walks the write-producing closure of each row
     operation: `CASCADE`, `SET NULL` and `SET DEFAULT` produce a write,
     `NO ACTION` and `RESTRICT` only refuse, and every statement the walk finds
     is itself walked, so a cascade two foreign keys away is authenticated by
     the same rule as the first. What the engine actually writes, measured:

     - An `ON UPDATE` action sets **every** column of the foreign key, even when
       only one referenced column changed; an `ON DELETE SET NULL`/`SET DEFAULT`
       with a column list sets that list (`confdelsetcols`, PostgreSQL 15 and
       later). `UPDATE OF` on the referencing side follows those columns, by
       name: a partition may number its columns differently from its root.
     - A referenced column that is generated changes when a `SET` column it
       derives from does, so the closure's key test reuses the same touched-
       column rule 445 defined for `UPDATE OF` rather than matching the SET list
       alone.
     - An action fires on the row the write *leaves*, not on the statement's
       `SET` list. The referenced side's own constraint trigger carries no
       column list — `tgattr` is empty, measured — and compares the old and new
       key values, so a `BEFORE ROW UPDATE` trigger that rewrites a key nothing
       set makes it cascade: measured, `UPDATE p SET ukey = ...` with a BEFORE
       trigger assigning `NEW.other` moved the child's `other` and fired the
       child's trigger. Every key of a relation such a trigger can rewrite is
       therefore in the closure. Two asymmetries, both measured and both the
       other way round from the rule beside them: a *disabled* BEFORE trigger
       rewrites nothing (that rule is a value written at run time, while the
       generated-column one is the planner's column list, which a disabled
       trigger still widens), and a user trigger's own `UPDATE OF` list is not
       widened by a rewrite at all — `UPDATE OF ukey` stayed silent when only
       the BEFORE trigger touched `ukey` — so the rule belongs to the
       foreign-key edge and nowhere else. A rewriter's *own* `UPDATE OF` list
       is read exactly as the trigger scan reads one: a `BEFORE UPDATE OF code`
       trigger does not run for a statement that sets `ukey`, so it rewrites
       nothing and widens nothing.
     - An UPDATE that can change a partition key does not update the row: it
       moves it. Measured on 18.6, the move fires the row-level BEFORE and
       AFTER **DELETE** triggers of the partition the row leaves and the
       row-level **INSERT** triggers of the one it lands in, and no
       statement-level DELETE or INSERT trigger anywhere. Of the partition's
       own UPDATE triggers only the row-level **BEFORE** one fires — it is
       what picks the destination — and the row-level AFTER UPDATE one does
       not. That takes nothing out of the closure: `can_move` says the
       statement *can* move a row, never that every row it touches does, and
       measured on the same table the same statement fires BEFORE **and**
       AFTER UPDATE on a row it leaves where it is. Narrowing a movable
       statement to its BEFORE UPDATE triggers would hand an attacker the one
       trigger the guard no longer looks at, on rows that never move. So an
       update statement whose columns reach a partition key carries two more
       events, row-level only, over the partitions it reaches — and that is
       true of the emitted row statement as much as of an action's, which is
       how 445's own guard turned out to have this gap for a declared
       partitioned table. The halves produce no referential actions of their
       own: measured, a moved row's children are cascaded as an UPDATE and not
       deleted. A partition key written as an expression counts as always
       movable, because which columns feed it is a question this tool does not
       parse (174). A BEFORE ROW UPDATE trigger picks the destination partition
       too — measured, one assigning the key moved a row whose statement
       touched nothing near it — and the closure deliberately does *not* carry
       that rule: such a trigger is on a partitioned relation or a partition,
       and `ORDINARY_TABLE` holds no relation that appears in `pg_inherits` at
       all, so it can never be a recorded managed trigger and the guard has
       already refused it by the time the question could arise. A filter that
       cannot change an answer is one nobody re-reads.
     - The action's statement runs even when it matches no row, so a
       statement-level trigger on the referencing table fires with zero
       referencing rows. That is the *least* a delete reaches, not the most,
       and the delete side keeps its row-level triggers and its recursion for
       the concurrent case. The row-delete preflight and the statement guard
       that matches it (333) refuse a plan whose declared row still has
       children — with one deployer and nobody else writing, the action's
       statement therefore matches nothing and only the statement trigger
       fires, measured. **Measured on 18.6**, a second session that inserts a
       child and a grandchild and commits while that guarded `DELETE` waits on
       the parent's row lock defeats both: the `NOT EXISTS` guard still admits
       the delete, the cascade removes the rows the other session committed,
       and the child's row trigger, the grandchild's row trigger and both
       statement triggers all fire. (The other order is safe on its own —
       an insert that starts *after* the delete has the parent row blocks and
       then fails the foreign key.) Narrowing the delete side to statement
       triggers, or refusing to walk past the first action because its
       statement "must" be empty, would hand an attacker exactly the trigger
       the guard had stopped looking at.
     - Referential actions carry `ONLY`. They reach partitions of the
       referencing side and name the root of its partition tree — a statement
       trigger on a partition does not fire, one on the root does — but they
       never reach a plain inheritance descendant, which the emitted row
       statement, carrying no `ONLY`, does. The two expansions are therefore
       kept apart rather than shared: one rule for both would either miss a
       partition or refuse for an inheritance child the engine leaves alone.

     A reached relation carrying a **rewrite rule** is refused outright rather
     than followed. Measured on 18.6, an `ON UPDATE … DO ALSO` rule on the
     table a cascade writes inserted into a third table and fired its
     statement trigger — a write the closure had neither locked nor
     authenticated, and one an attacker with `TRIGGER` on that third table
     could wait for. Following it would mean reading the rule's action, which
     is SQL this tool does not parse (174), so the boundary is drawn where the
     model already draws it: `ORDINARY_TABLE` holds no relation with rules, and
     now neither does a write's closure. The test is the rule's own event and
     enabled state, so another event's rule and a disabled one refuse nothing,
     and only the relation the statement *names*: measured, the rewriter runs
     before partition routing and before inheritance expansion, so a rule on a
     partition or on an inheritance child does not fire for a statement naming
     their parent, and refusing for one would refuse a plan whose write cannot
     reach it. A row movement's halves are not asked at all, for the same
     reason: measured, rules on the partitions a row leaves and lands in do not
     fire, because the movement is one statement's doing and not a statement of
     its own.

     Each reached table is locked `ROW EXCLUSIVE` before its triggers are read
     and held for the write, as 445 requires of the named one; the same lock on
     the referenced side is what keeps a new foreign key from being added to it
     mid-walk, since `ALTER TABLE ... ADD FOREIGN KEY` takes
     `SHARE ROW EXCLUSIVE` on both sides. Reached tables are followed by oid,
     never by name: a name is needed only to write `LOCK TABLE`, and a rename
     between reading that name and locking it would otherwise hand the guard a
     lock on some other relation, so the lock is proved to have landed on the
     intended oid before its triggers count for anything.

     The cost is one refusal that is not about a trigger: `ROW EXCLUSIVE` needs
     `INSERT`, `UPDATE`, `DELETE` or `TRUNCATE` on the table, and measured,
     `SELECT` alone is `permission denied`. Because the action runs as the
     referencing table's owner, a deployment role can have a valid plan whose
     cascade reaches a table it may not write. Refusing is the safe direction —
     there is no weaker lock that conflicts with `CREATE TRIGGER` — and the
     refusal names the table and the privilege rather than surfacing the
     engine's bare `permission denied`. Narrowing the closure to tables whose
     owner can act as the deployer would avoid it and was not taken: the
     effective-role rule is the thing an attacker would have to fool, and a
     guard that is right about who may write beats one that is clever about
     when it need not look.

     Two things take a constraint back out of the closure, and both are the
     shape 128 and `DELETE_ACTION_FIRES` already established one crate over.
     **A foreign key the plan removes before the row statement** — a
     `DropForeignKey`, or a `DropTable` carrying one, both ordered at
     `order_key` 2 and 6 against the row classes 11 and 12 — cannot write when
     that statement runs, and following it would refuse a plan for a table the
     write never reaches. The allowance is `prepare`'s alone: `check` reads the
     catalog immediately before the write, where what the plan promised to
     remove has to actually be absent, exactly as 445 requires of a dropped
     trigger. The same allowance covers the two predicates that *widen* a write
     rather than follow one: the BEFORE ROW UPDATE trigger that makes
     PostgreSQL touch every generated column, and the one that can rewrite a
     referenced key the statement does not set. A plan that drops the only such
     trigger and updates another column of that table would otherwise be
     refused for a table its write cannot reach once the drop has run —
     `DropModule` is `order_key` 0, so the widening is asked of the catalog as
     the plan leaves it, not as it stands when the guard looks.
     **An action whose own trigger does not fire** writes nothing
     either; measured on 18.6, neither a disabled parent-side constraint
     trigger nor an origin trigger under `session_replication_role = replica`
     cascades at all, and the child row is left untouched. The closure applies
     the same enabled test `preflight::DELETE_ACTION_FIRES` makes, by the event
     this write raises rather than by the delete event alone — but asks it of
     the constraint that *owns* the trigger, which the probe one crate over
     never has to: every probe there filters `con.conparentid = 0` and so only
     ever holds a declared row. Measured on 18.6, a partitioned referencing
     side puts one pair of action triggers on the referenced table for the
     declared constraint and none at all for the copies, so asking a copy for
     its own trigger finds nothing and reads a disabled action as a firing one;
     a partitioned *referenced* side is the other way about, each level owning
     its pair on its own relation. The test is therefore the constraint's
     ancestry chain narrowed by the relation the trigger is on, which is right
     in both shapes and in the one where both sides are partitioned.

     A partitioned side is catalogued as the declared foreign key plus a copy
     per partition, and a copy can carry another name (`qc_a_id_fkey_1`,
     measured). The declared row answers both questions asked of a constraint:
     it is the only name the plan can remove, so a removal is matched against
     it rather than against the copy the walk happened to reach, and its
     `conrelid` is the relation the action's own statement names — measured,
     the cascade into a partitioned referencing table ran against the
     partitioned parent and fired its statement trigger, not the partition's.
     Following `pg_inherits` upward instead was the first shape and is wrong
     the other way: a foreign key somebody declared on a single partition is
     its own declared row, and promoting it to the root drags in the root's
     statement triggers and every sibling partition, refusing a plan for
     tables that action cannot touch. The copies are not skipped outright
     either: when the write names a partition of the *referenced* side, the
     copy is the only row that matches it at all.

     The lock a reached table takes is `LOCK TABLE` without `ONLY`, which also
     locks descendants the action itself will not write. Measured, that costs
     no privilege — PostgreSQL checks the named table's and takes the
     descendants' locks regardless — and it is atomic over the descendant set,
     which enumerate-then-lock is not.

     Live regressions: the escalation through `ON UPDATE CASCADE`, `SET NULL`
     and `SET DEFAULT` and through `ON DELETE CASCADE` and `SET NULL`, at row
     and statement level, two foreign keys deep, under both apply modes and
     both `unmanaged` settings, each with a control proving the action really
     reaches the table and that the same plan applies once the trigger is gone;
     a table of closure membership cases pairing every non-writing action,
     unreached event, untouched column and unreachable descendant with the
     writing case next to it; a concurrent `CREATE OR REPLACE TRIGGER` on a
     cascade-reached table blocked until the write commits; the cannot-lock
     refusal naming its table; and the removals — of a foreign key, of the table
     carrying one, and of each of the two triggers that widen a write — and the
     two not-firing actions, the row-level and grandchild triggers a delete
     action reaches,
     each paired with the state in which the same closure does follow it, and
     each again with the referencing side partitioned, where the action trigger
     belongs to the declared constraint and not to the copy the walk matched; the
     partitions a row movement leaves and lands in, against the partition key
     the write does not touch and the statement-level trigger no movement
     fires, on an action's target and on the named table alike; a foreign key
     declared on one partition, against a trigger on its sibling and a
     statement trigger on their root; the rewrite rule on a cascade's target,
     with the engine's own third-table write asserted first, against the same
     rule on another event, on a partition of the relation the action names,
     and disabled; and
     the key a BEFORE trigger rewrites, with the engine's own behaviour
     asserted first and the trigger doing the rewriting recorded and approved,
     so the refusal can only be the table its cascade reaches. They
     pass on 16.15 as well as 18.6: the one catalogue column the closure needs
     that is not ancient, `confdelsetcols`, arrived in 15.

<a id="decision-471"></a>

471. **This dialect's read of a defaulted cell asks the same question the write
     does, and the guard that asked about the type had nothing left to guard.**
     The port of 330 and 331 to `pbps-mssql`, and worse here than there because
     it needs no unusual collation at all.

     **The read-back's `at_default` was the column's comparison, not pbps's.**
     `emit::defaulted_cell` holds a cell to its default as text under
     `COLLATE Latin1_General_BIN2` and through the column's type; `rows::query`
     asked the same question with a native `=` on the raw column. **Measured on
     `SQL_Latin1_General_CP1_CI_AS`** — the server's default collation, and the
     one this suite's container runs on — with `label varchar(20) DEFAULT 'new'`
     holding `New`:

     ```text
     label = ('new')                                  ->  at_default
     the same, as text under Latin1_General_BIN2      ->  drift
     ```

     The first answer marks the cell at its default, so `ObservedRow::as_seen_by`
     leaves it out of the read-back, the drift is not in the observed state at
     all, and neither a connected plan nor `verify` proposes to put the value
     back. On PostgreSQL the same defect needs a `CREATE COLLATION`; here the
     default server collation is case-insensitive, so every installation that
     never chose one is exposed.

     **And `rows::comparable` went with it.** It answered "does this type have
     `=`?" for `xml`, `geometry`, `geography`, `text`, `ntext` and `image`, and
     was asked in three places because the comparisons there were native. Every
     one of them is a comparison of text now — `read_expr` on both sides, under
     the binary collation — so the guard cost those six types their read-back
     and bought nothing: a cell of one was never asked about, was read as
     indistinguishable from its default, and a hand edit to it was drift no plan
     would settle.

     **Measured**, both halves of why removing it is safe:

     ```text
     CONVERT(nvarchar(max), doc) = CONVERT(nvarchar(max), TRY_CONVERT(xml, '<a/>'))
                                                    ->  at_default for the default,
                                                        drift for an edit
     CREATE TABLE k (c xml PRIMARY KEY)             ->  Msg 1919 … invalid for use
                                                        as a key column in an index
     CREATE TABLE u (c xml UNIQUE)                  ->  Msg 1919, the same
     ```

     The last two lines are what make the sweep complete rather than hopeful,
     and they were run for all six types, not for `xml` alone. The native `=`
     this dialect still writes is on key columns — `WHERE [key] = N'…'` — and
     none of the six can be one: refused as a `PRIMARY KEY` and as a `UNIQUE`,
     so no foreign key can reference one either. There is no type left for the
     guard to protect.

     What a plan holds therefore widens: an `INSERT` that leaves an `xml`
     column to a literal default now carries a predicate holding it to that
     default, where before it carried none.

     **One case the widening must not reach, found in review: a retyped
     `image` column.** Holding a cell across a retype means putting the
     recorded text back through the type that rendered it, and `image` has no
     way back. **Measured**, and not a conversion that merely fails:

     ```text
     TRY_CONVERT(image, N'0x02')      ->  Msg 529: Explicit conversion from data
     TRY_CONVERT(image, N'0x02', 1)       type nvarchar to image is not allowed
     ```

     So the expression does not run at all rather than answering NULL, and a
     statement built from it raises where it should refuse — which is the one
     thing `TRY_CONVERT` is in that predicate to avoid. `rows::from_text`
     returns an `Option` now. `Held::as_stored` keeps its `Option` for that one
     reason — a retype it cannot invert — rather than the old one, a type
     without an operator, and an `image` column this plan leaves alone is held
     like any other, because nothing is converted.

     **Two more renderings are not inverses, and the review found both.**
     `geometry` and `geography` convert back and come back *different*:
     `ToString()` is the well-known text and the SRID is not in it. Measured, a
     `geometry` built at SRID 4326 renders `POINT (1 2)` and reads back at SRID
     0; `geography` round-trips only because 4326 is its own default. Both join
     `image` in returning `None`. `text`, `ntext`, `xml`, `hierarchyid`
     (`/1/2/` returns `/1/2/`), `timestamp` and `sql_variant` were each measured
     converting back and stay invertible.

     **And one rendering is lossy where it is compared, not only where it is
     rebuilt.** A `sql_variant` writes its value and not its base type, so
     measured, a variant holding `nvarchar` `N'1'` and one holding `int` `1`
     render the same string while the engine calls them different values:

     ```text
     @stored = @default                            ->  different
     the same, as text under Latin1_General_BIN2   ->  equal
     ```

     Moving that column from the native `=` to a comparison of text alone would
     have hidden exactly the drift this entry is about, on the one type where
     the old comparison was the better one. So the default question carries the
     engine's `=` beside the text for `sql_variant` — and for no other type,
     because adding it everywhere would refuse matches no measurement says are
     wrong. It is one function, `rows::same_value`, asked by the read path and
     the write path alike, so the two cannot drift apart again.

     What this does **not** reach: a spatial value whose SRID alone differs from
     its default still compares equal, because the comparison is of a rendering
     that never carried the SRID. That is not a regression — those types were
     not asked about at all before — and fixing it means changing what
     `read_expr` writes for them, which is the recorded text of every spatial
     cell. Filed rather than done here.

<a id="decision-472"></a>

472. **SQL Server row writes hold assigned text and preserve existing key
     aliases (issue #218).** Native equality can accept a trigger changing
     `New` to `new` or adding spaces. Retain native equality for identity and
     precision, and additionally compare key text under a binary collation
     with byte lengths, since even binary equality pads away trailing spaces.
     UPDATE/DELETE selection and the delete postcondition stay native (132).

     INSERT assigns the key. Its expected text is `CONVERT(nvarchar(max),
     CASE WHEN 1 = 0 THEN <key column> ELSE <declared literal> END)`: Unicode
     text retains spelling and width, while the engine converts non-text
     keys. A second expression, `ISNULL(CASE WHEN 1 = 0 THEN <key column> END,
     <declared literal>)`, supplies the exact column type's padded length.
     Require actual length to equal that padded length and be at least the
     full expected length. Assignment and ISNULL can both discard over-width
     trailing spaces; the lower bound catches that loss. Compare contents
     against the full text, because ISNULL also repeats lossy code-page
     conversion. Varying spaces that fit and char/nchar padding remain valid.

     UPDATE does not assign the key: it may locate stored `new` through
     declared `New`, as 71/101 promise. Holding that update to the declaration
     would refuse a valid label change. Instead capture the stored generic
     text in a variable assignment within UPDATE's SET list and compare the
     postcondition to that text and length. Measured on SQL Server
     17.0.4075.5, the capture sees the value before AFTER triggers, preserves
     the statement's row count, and does not mark the key in `UPDATE(key)`.
     Capturing inside the write avoids a separate read and its race. An
     update is its own exported batch because T-SQL variables have batch
     scope; consecutive updates otherwise redeclare the capture variable.

     Numeric aliases such as int `01` and decimal `1.5` remain valid for both
     writes (71/101). Native equality is still needed: generic rendering can
     collapse distinct money/float values. The change adds no key-type field,
     catalog permission or saved-plan format, and does not restate the key.

     Live regressions cover ordinary and aliased updates with and without
     case/space triggers, including a trigger rewriting an alias to the
     declaration itself. INSERT negatives cover over-width spaces and
     varchar code-page substitution; UPDATE preserves those existing
     aliases. Canonical non-text and padded text remain valid, and a money
     precision change is refused. Exported consecutive updates execute as
     separate batches. Reverting the fix refuses the ordinary alias update;
     omitting the update text check accepts trigger rewrites; removing the
     batch boundary makes the exported script fail. Restoring the guards and
     batch boundary makes these regressions pass.

<a id="decision-488"></a>

488. **Operational row work is shared; storage mechanics remain engine-specific
     (issue #255).** SQL Server's catalogue measurements found in-place updates
     with unchanged heap/allocation IDs, and data-dependent log growth even when
     the update path processes every row. PostgreSQL's replacement-storage
     observation cannot be generalized into a promise that every rewrite needs
     a second copy. Shared `pbps-dialect::estimate::{Rewrite, Reads}` describes
     row-processing work independently of physical bytes, disk space and risk.
     PostgreSQL reexports the types; its measured answers remain unchanged.

     SQL Server estimates offline column type/nullability changes, using catalog
     compression and approximate row counts. Its description names in-place
     updates. Unmeasured operations, ONLINE, other versions, special storage and
     index/constraint context stay explicitly unknown or unavailable. Catalog
     dependencies are not projected through planned drops, so no second planner
     grows inside the estimate. Whole-plan provenance retains stored names
     across renames and prevents newly created/retyped identities borrowing
     current storage facts from a name that will be reused.

     Locks, catalog queries, provenance and row-statistics semantics stay in
     each engine; only the common answers cross the pure dialect seam. The
     diagnostic counters/log-volume measurements live only in tests. Planning
     needs no new server-performance permission, and a failed estimate never
     rejects a valid plan. Existing JSON variants, saved formats, risk classes
     and approval gates remain unchanged. ADR-0012 Amendment 3 and the per-engine
     live matrices record the measured boundary.

<a id="decision-538"></a>

538. **A baseline with no primary key has its retained rows matched on the
     declared key, but only when the same plan restores that key on a column
     the baseline already has.** (#124, #280; ADR-0004 §2.) Row identity in a
     data block is the key column's value, and each side's row keys are
     values of that side's key. A baseline without a primary key has no key
     of its own to compare against, and the obvious reading — "the key moved"
     — is wrong: nothing moved, it is absent, and the diagnostic said
     something false.

     Matching on the declared key is sound in exactly one shape: the plan
     carries `SetPrimaryKey { from: None, to }` for that table with the
     single declared key column, and that column maps by uid to a column the
     baseline already has. Then the retained rows' keys are values of the
     same column on both sides, and `UpdateRow`, `InsertRow` and `DeleteRow`
     compare like with like. The differ can see the restoration because the
     constraint diff runs before the data diff and its changes are already
     in the list.

     Every other shape is refused, each with its own diagnostic rather than
     a moved-key one: no restoration in the plan (`DataBaselineKeyAbsent`),
     a restored key on a column added by this plan, whose values the
     baseline's rows predate (`DataKeyColumnChanged`), and a baseline key
     with more than one column (`DataBaselineKeyNotSingle`). Inferring a key
     without the restoration was rejected: a plan that leaves the table
     keyless would then have its row changes matched on a uniqueness the
     engine does not enforce.

<a id="decision-539"></a>

539. **A key restored on a column the baseline does not have is refused with
     its own diagnostic, not as a moved key.** (#808, amending 538.) Entry
     538 said each refused shape gets a diagnostic "rather than a moved-key
     one", and one of the three it listed was refused with
     `DataKeyColumnChanged`, whose message says the primary key moved to
     another column and advises renaming the column instead. Both halves were
     false for that shape: the baseline had no key to move, and there is no
     column to rename back.

     `DataBaselineKeyOnNewColumn` names the table and the new key column and
     advises the two ways out that do work: restore the key on a column the
     table already has (538's accepted shape), or remove the block, apply the
     key change, and declare the rows again. With this, 538's sentence holds
     as written; the refusal itself is unchanged.

<a id="dec-976-1"></a>

**DEC-976.1. A row block's variables are reached through its label, and an
unqualified name in its statements means the column (#976).** The update and
delete blocks declare `pbps_rows` and `pbps_referencing`, and their statements
name the table's columns unqualified. A data table may have a column spelled
like either variable, and under PostgreSQL's default
`plpgsql.variable_conflict = error` that statement fails as ambiguous mid-apply,
after validation and planning passed. Each block therefore opens with
`#variable_conflict use_column` and the label `<<pbps>>`, and every read or
write of a variable is written `pbps.<variable>`, which no column can shadow.
Renaming the variables to something less likely was rejected: it would only
move the collision to another spelling. Measured on PostgreSQL 16 and 18,
including a user schema named `pbps` beside the label.
