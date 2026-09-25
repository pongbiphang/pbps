# The apply guard

What `apply` checks after its statements run: postconditions, the read-back, and
what the plan itself wrote. Part of the [decision record](../DECISIONS.md),
which says how to add an entry here.

<a id="decision-147"></a>

147. **The read-back and the ledger entry are inside the apply's own
    transaction.** What `apply` records is the database read back, not the
    plan applied to the old state (SPEC §8.2) — and that read used to happen
    after the commit. Between the two, another session could edit a declared
    row or a managed role's grants, and the read would take that in and record
    it as this plan's result: `apply` reporting success, `verify` clean
    against the newly blessed state, and only the next connected plan
    proposing the declaration back. The per-statement postconditions (132,
    136, 143) close the window *inside* each statement; this one is after the
    last of them.
    So `run_uncommitted` leaves the transaction open, the read-back and
    `state::record` run in it, and `commit` is the last thing that happens.
    Two things fall out. The ledger entry becomes as atomic as the change it
    describes — before, a failure to write it left an environment changed with
    nothing saying so — and a failure in the read-back now rolls the
    statements back rather than leaving them applied and unrecorded. The old
    `run_in_transaction`, whose whole shape was "commit immediately", is gone
    rather than left available: it is the misuse this entry is about.
    `bootstrap` had the same shape and is fixed with it; the lock is still
    released after the commit, either way.
    **`apply --staged` keeps the window, by construction.** It runs each
    statement outside a transaction so that a mid-way failure leaves a
    resumable checkpoint (ADR-0003 decision 2), and there is no transaction to
    put its closing read-back in. What guards a staged run is the same set of
    per-statement postconditions, plus the drift check every `--resume` makes
    against the checkpoint.

<a id="decision-148"></a>

148. **The spelling checks name the catalog's objects, not the plan's.**
    `refuse_misspelt` runs before a statement of the plan has run, so the
    database still has the names the *previous* revision left. Almost every
    query it builds converts a literal and names nothing, but one does: the
    key column's collation read (131), which is the whole reason two spellings
    of one key can be caught. Under a renamed table or key column its
    `OBJECT_ID` found nothing, `@coll` came back NULL, and the comparison fell
    back to the database's default collation — silently, which is the shape
    this project keeps paying for: absent read as "nothing to see". Measured
    on a case-sensitive database with a case-insensitive key column: the
    correct names report `a` and `A` as one row, the declared names report no
    conflict at all, and the plan would then hold two inserts the primary key
    refuses.
    So the callers pass what the catalog calls each declared table and its key
    column — `rows::CatalogNames`, built from the two identity files, absent
    entries meaning "as declared". `plan --db` builds it from the resolved and
    recorded ids; `bootstrap` passes none, because nothing it declares exists
    yet; the dev rehearsal builds it from the baseline's ids, since its
    scratch database was built from the previous revision too. Only the table
    and the key column travel: every other declared value reaches the engine
    as a converted literal, under no name at all.

<a id="decision-150"></a>

150. **What `apply` records has to be the baseline plus the plan, and the
    part of that the tool can check exactly is everything the plan does not
    touch.** 147 moved the read-back inside the transaction, which closed the
    window after the commit. The window *before* the statements was still
    open: the pinned checksum is answered at the top of `apply_under_lock`,
    and between that answer and the read-back sit `refuse_unexpressible`, the
    whole of `preflight` — many round trips — and the statements themselves.
    A session that revokes a grant the declarations still hold, or edits a
    declared row of a table this plan never mentions, in that window is taken
    in by the read-back and written down as this plan's own result: `apply`
    reports success, `verify` is clean against it ever after, and only the
    next connected plan proposes the declaration back.
    Neither of the obvious repairs works. Opening the transaction earlier
    moves nothing: SQL Server's metadata reads are read-committed whatever the
    isolation level, so membership in a transaction is not what keeps another
    session out, and raising the isolation level to hold catalog locks for the
    length of a deployment is a cure worse than the disease. Comparing the
    read-back against "the plan applied to the baseline" is not available
    either — computing that is exactly what the read-back exists to avoid,
    since only the engine's stored form compares equal on the next drift check
    (SPEC §8.2).
    What *is* exact needs no dialect knowledge at all: **for every managed
    object no change of this plan names, the state after is the state before.**
    `refuse_unplanned_movement` compares the two, over tables, modules and
    roles, and the apply's own transaction rolls back on a difference. The
    objects the plan does name are exempt because changing them is the point,
    and they are held by the plan's own preconditions and postconditions (132,
    136, 143) and by the locks its statements take. Both ends of a rename
    count as named — the recorded state knows the object by one name and the
    read-back by the other, and a comparison that took one end would read
    every rename as a table vanishing and another appearing.
    Two reads, one question. The baseline is read once and projected twice
    (`baseline_state`): the checksum under the recorded state's spelling and
    the pinned union of scopes, because that is what the plan pinned (98); the
    comparison under the plan's scopes with no reference at all, which is
    exactly how the read-back will be projected. Projecting them differently
    is the false-refusal trap — a cell at its default has three spellings
    (`ObservedRow`) and an `ensure` block that drops a key reads fewer rows
    than the recorded scope, so two views taken under two questions differ
    without anything having moved. Reading the engine twice would be worse
    still: it would ask the same thing twice and could get two answers, which
    is the very thing being detected.
    `refuse_unexpressible` runs on the read-back too, for the same reason it
    runs on every state a command writes down (110): a `WITH GRANT OPTION`
    that arrives mid-apply is as unrecordable as one that was there at the
    start.
    **`apply --staged` is not covered, for the reason 147 gives.** It runs
    each statement outside a transaction on purpose, so there is nothing to
    roll back and a refusal at the end would only strand the environment
    mid-deployment. Its guards remain the per-statement postconditions, the
    checkpoint written after each statement, and the drift check every
    `--resume` makes against that checkpoint.
    The race cannot be staged through two sessions in a test, so the live case
    uses an `AFTER INSERT` trigger that writes a row into a *different*
    declared table: a change landing inside the apply's own transaction,
    between the two reads, which is the window exactly. With the comparison
    reverted, `apply` reports success and records it.

<a id="decision-153"></a>

153. **A table the plan touches is exempt down to the rows the plan names, and
    no further.** 150 compared everything no change of the plan named, and
    exempted a named table whole. An `AFTER` trigger on a declared table
    reaches that table's *other* rows from inside the very statement that
    writes the one the plan asked for — and the statement's postcondition
    speaks for that row alone (132, 136, 143). So the one place a trigger can
    reach was the one place the comparison did not look: the apply committed,
    recorded the trigger's rewrite as its own result, and `verify` called it
    clean. Measured through the CLI with a trigger that rewrites another row
    of the table being inserted into; with the exemption in place the apply
    reports success and records entry #2.
    The table's *shape* stays exempt where the plan names it: altering a
    column is what the plan is for, and a concurrent DDL on the same table
    has to wait for the schema lock this plan's own statements hold. Rows are
    what something can move while the apply is running.

<a id="decision-158"></a>

158. **Two more sides of the same rename, and one of a drop.** 157 said a
    rename is visible from three sides and named the third. It was still short
    by two, and the shape is now unmistakable: **a plan does things to a
    container, and those things show up somewhere the plan never mentions.**
    **A column change re-shapes every row of its table.** A row is keyed by
    column name and each cell reads back in its column's own rendering, so a
    column this plan renames is under one name in the baseline and another in
    the read-back, one it adds is in neither, and one it retypes reads back
    differently. The differ emits only the column change — no row change says
    anything — so 153's row comparison called every row of that table somebody
    else's work and rolled back a valid apply. Rows are compared on the columns
    the plan leaves alone now. Every column-level change counts, not the subset
    that can be argued to alter a rendering: naming one too many only narrows a
    comparison, naming one too few refuses a valid plan.
    **A dropped securable takes its permissions with it.** Measured: a role
    granted `SELECT` on a table holds nothing once the table is dropped. So
    `diff_roles` emits no `REVOKE`, the role is named by no change, and 150's
    whole-role comparison saw the grant vanish and refused. The baseline's
    grants now lose the ones whose object this plan drops, beside 157's
    forwarding of the ones it renames — the two belong together and are written
    together.
    **The count so far, because it is the point:** four rounds of review on
    this one guard, and after the first the findings were all in the same
    direction — the guard inventing movement rather than missing it. Every one
    was a place the plan's own effect reaches past the object the change names.
    The check that would have found them without a reviewer is not "did I
    handle renames" but "for each kind of change, what does it alter that no
    change of its own describes".

<a id="decision-160"></a>

160. **What the plan itself wrote is checked, not excused — and "empty" is
    only safe where it is true.** Three findings, and the first two are one
    mistake made twice: 156 and 150 both *exempted* what the plan touches,
    and an exemption is only correct where something else does the checking.
    For a row there is something else — every row write carries a
    postcondition (132, 136, 143). For a permission and for a module there is
    nothing. `GRANT` reports success and says nothing about what the role now
    holds; `CREATE OR ALTER` reports success and says nothing about what is
    now stored. So a session that reversed either straight afterwards — or a
    database DDL trigger, which does it deterministically — was read back and
    recorded as the plan's own result, with `verify` clean over it ever after.
    **Permissions** are now reconstructed rather than subtracted: what the
    role held, less what this plan revokes, plus what it grants, compared with
    what it holds. That is exact — the statements are `GRANT p ON t TO r`,
    with nothing for the engine to decide — and it subsumes 156, which was
    the same comparison with both sides blinded to the interesting part.
    `PermissionChange` carries the direction, because "the plan moves these"
    cannot say what the role should end up with.
    **Modules** are held to the definition the plan wrote. Comparing exactly
    is safe for a reason the tool already depends on: a module read back
    equals the declaration that produced it, and if it did not, every apply
    would be followed by drift for ever — the live round-trip test says so in
    those words.
    **The third is the opposite shape.** `OLDEST_READABLE_VERSION` reached 3
    through the merge, and a version 3 snapshot predates `module_deps`. For
    `roles` the default is a *true* reading — an environment recorded before
    roles were managed is one with no managed roles — and 138's argument for
    reading it holds. For `module_deps` it is a *missing* one: a revision
    removing several dependent modules has no declaration left carrying their
    `depends_on:` edges, so the order comes from the snapshot, and defaulted
    to empty it falls back to name order and can drop a schema-bound
    dependency before its dependent. Refused, with the re-record
    `check_version` already names. Absent, empty and unreadable are three
    different things, and this is the version boundary being asked to tell
    them apart.
    **And a test that pinned nothing.** The version-boundary test was written
    against `OLDEST_READABLE_VERSION`, so it followed the constant wherever it
    went — it passed unchanged with the boundary put back to 3. A historical
    format version is a fixed thing and the test now names it.

<a id="decision-161"></a>

161. **A postcondition is only fair once the statement has run, and only
    against the net result.** 160 gave the plan's own permissions and modules
    a postcondition, which was right, and wired it in two places where it
    could not hold.
    **A module replacement is one name and two changes.** `diff_modules`
    emits `DropModule` then `CreateModule` for a module that changes kind, or
    a trigger that changes its target. Checked change by change, the create
    satisfied `Standing` and the drop then reported the module "still there" —
    because the create had put it back. **Every replacement was refused.**
    Collapsed by name now: the plan is in `order_key` order, which puts the
    drop first, so the last word on a name is the net one.
    **A staged checkpoint is not the end of the plan.** `staged_movement`
    passed the whole `ChangeSet` into a check that had just learned to demand
    the plan's results, so at checkpoint 1 of N it required grants and modules
    from statements still to come, and the run stopped at its first
    checkpoint. `Settled` now says how much has run: `SoFar` compares movement
    alone, `Whole` adds what the plan achieved, and only the last read of a
    staged run gets `Whole`.
    **Two gaps 160 left, of the same kind it closed.** A table the plan
    creates, drops or renames was exempt from the whole-table comparison and
    only its *rows* were checked underneath — so a created table dropped in
    the window was recorded as success. A role with no grants has nothing but
    its name, so the per-target grant comparison had nothing to compare and a
    `CREATE ROLE` another session undid read as success too. Both are
    existence checks now, and existence *only*: the shape of a table comes
    back from the catalog precisely because the engine's stored form is the
    one that compares equal on the next drift check, and holding it to the
    declared shape would refuse valid applies.
    **And a revert check that proved nothing.** The staged wiring was covered
    by a test calling `refuse_unplanned_movement` directly, so reverting
    `staged_movement`'s choice of mode left it green. A test of a pure
    function does not pin the call site that chooses its arguments; the wiring
    has its own test now. That is the second time this round — the version
    boundary in 160 was the first — that a revert did not bite, and both were
    a test written against the thing under it rather than the thing being
    fixed.

<a id="decision-162"></a>

162. **Three places the guard looked, rather than three things it compared.**
    Every one of these is the check being correct about the wrong domain.
    **A target only the plan names.** The per-target grant comparison iterated
    the targets *either side* holds permissions on. A plan that adds the first
    permission a role has on a target puts it in neither set the moment that
    grant is reversed — so the one thing being verified was the one thing the
    loop never visited. The planned targets are unioned in now.
    **A row after its statement commits.** A row write holds itself to what it
    wrote (132, 136, 143), and that is enough inside one transaction: the row
    stays locked until the commit, so nothing else can reach it. A staged run
    commits each statement, and the row is loose from then until the
    checkpoint read. Rows the plan writes are held to being *there* or *gone*
    at the settled comparison. Their **contents** are not, and that is a
    limit rather than an omission: predicting a cell's read-back spelling is
    what 149 exists to avoid, and a declared value equal to its column's
    default is *omitted* from the read-back, so comparing spelled cells would
    refuse valid applies.
    **A column change that moves no reading.** 158 put every column-level
    change into the skip set, arguing that naming one too many only narrows a
    comparison. It does, and the narrowing has a price: a skipped column is a
    cell nothing compares. Nullability rewrites no stored value, and the
    read-back's omission rule turns on whether a column has a *default*, not
    on whether it accepts NULL; a deprecation is a description. Both are out
    of the set now, and a change to the default stays in it.
    Two of the three are the argument of 158 running out: "conservative" is
    the right instinct against a guard that has invented movement six times,
    and it is not free. Where the conservative reading can be shown to cost a
    real check and the aggressive one can be shown to be exact — as here, from
    the omission rule and from the plan's own statements — the exact one wins.

<a id="decision-163"></a>

163. **A loop over the baseline never visits what the plan creates.** Twice,
    on the two halves of the model, and it is 162's lesson again one turn
    later: the comparison was right and its *domain* was wrong.
    A table this plan creates has no entry in the state it is measured
    against, so the row comparison — which walks the baseline — never reached
    it. Its rows were checked only for the *planned* keys being present, so an
    undeclared row that arrived in a table one statement old (a DDL trigger,
    or another session between a staged `CREATE TABLE` and its checkpoint) was
    recorded into an `exact` snapshot and read as clean ever after. The same
    for a role: a grant that landed on one the plan had just created was
    checked by nothing, since only its existence was.
    Both domains now include what the plan creates, with the baseline such an
    object actually had: **no rows, and no grants**. That is not a
    stand-in — a table that did not exist held no rows — and every branch
    already written then does the right thing with it, which is why neither
    fix needed a new comparison.
    An `ensure` table needs no special case and gets none: its read covers the
    declared keys only, so an undeclared row is never read on either side and
    can neither be reported nor missed. The mode falls out of the scope
    instead of being tested for.

<a id="decision-165"></a>

165. **The cells a plan spells, and the difference between an empty table and
    an unspellable one.**
    **The row contents.** 162 held a planned row to its *presence* and named
    its contents as a limit, twice, on the grounds that predicting a
    read-back's spelling is what 149 exists to avoid. Half of that was right
    and half was an excuse. The unpredictable half is a cell **at its
    column's default**: the read-back omits it ([`data::cell`]), and a plan
    value that happens to equal that default lands in the same place — so
    demanding it would refuse a valid apply. The predictable half is
    everything else, and it is exact rather than merely likely: `plan --db`
    refuses a declaration the engine reads back differently (101), and the
    write itself is held to that rendering (132, 136, 137).
    So a planned row is now held to the cells the plan spells, **where the
    read-back carries them**. An omitted cell says nothing; a present one must
    match. What is still not caught is another session setting such a cell to
    exactly its column's default, which makes the read-back omit it — named
    here rather than left to be found, and the smallest limit this comparison
    has had.
    **The probe.** 164 turned "no rows to select from" into an empty relation,
    on the reading that it meant "a table this plan creates that declares
    none". It also meant a table that declares rows whose key cells no probe
    can evaluate — a default that is not a literal (117) — and that table will
    *not* be empty. Called empty, every matching child was counted an orphan
    and a foreign key the engine would have created was refused. The two are
    told apart now, and the unspellable case goes back to no answer, which is
    what every other unprobeable default gets.
    Both halves are the same mistake in opposite directions: 164 read "no
    answer" as "empty" and got a false refusal; 162 read "cannot predict all
    of it" as "cannot predict any of it" and got a missing check. Absence has
    to be classified before it is acted on — which is this file's oldest
    entry, and it keeps needing to be applied one level further in.

<a id="decision-166"></a>

166. **A touched table answers for the shape the plan leaves alone, and a
    historical path is composed rather than asked about.**
    **The shape.** 161 checked a touched table's *existence* and stopped
    there, on the reasoning that holding it to its declared shape would refuse
    valid applies — the engine's stored form is the one that compares equal on
    the next drift check (SPEC §8.2), and the declaration is not it. That
    reasoning is sound and it answers a question nobody asked. The comparison
    that matters is not declaration against read-back; it is **the recorded
    state against the read-back**, both already in the stored form, over
    everything this plan does not move. So a touched table's columns,
    constraints and indexes are compared entry by entry now, minus the ones
    the plan names — the same narrowing its rows got in 153 and its role's
    grants in 156, one level up. Nothing here predicts anything.
    What the plan's own alterations *achieved* is still not checked, and that
    is the part which would need the stored form: a retyped column is
    compared by nobody, and the column and constraint names the plan adds or
    removes are held to being present or absent instead. Third time this
    distinction has had to be drawn — 162 for rows, 165 for their cells — and
    it is the same one: predict nothing, compare two reads.
    **The path.** 155 read a revision's own `schema_dir` and then converted it
    with `relative_to`, which answers by running git *inside the path's own
    parent*. A revision that kept its declarations in `legacy/schema`, since
    removed, has no such parent — `fatal: cannot change to '.../legacy'`, and
    `plan` and `validate --since` failed outright instead of reading the old
    tree. Composed from the project root's own prefix now, which is the one
    directory that is always there. The first fix asked the working tree about
    a path that only history has; the whole point of `paths_at` is that those
    are different.

<a id="decision-167"></a>

167. **Three refinements of 166, and one of them is a gate that should never
    have been there.**
    **By definition, not by name.** The constraint and index comparison asked
    whether each name was on both sides. A constraint dropped and recreated
    under the same name with a different body is on both sides, and the test
    called that unchanged. Compared by value now — which predicts nothing,
    since both values come from a read-back, exactly as the columns beside
    them already did.
    **At every read, not the last one.** The shape comparison was gated on
    `Settled::Whole` out of caution, and the caution was misplaced twice over.
    It compares two read-backs over what the plan does not move, so it needs
    no part of the plan to have run — and each checkpoint's read becomes
    `previous`, so a change that landed before an earlier checkpoint was baked
    into the baseline of every comparison after it. The final `Whole` read
    then measured the contaminated shape against itself. `Settled` is for the
    checks that ask what the plan *achieved*; it was never for the ones that
    ask what moved.
    **`..` is resolved, not dropped.** 166 composed a historical path from the
    project's own prefix and kept only `Normal` components, so a project in a
    subdirectory whose old revision said `schema_dir: ../shared/schema` got
    `<project>/shared/schema` — a path the repository does not have, read as an
    empty baseline, every object new. Resolved against the prefix now, with a
    path that reaches past the repository root, or an absolute one, refused by
    name rather than silently turned into something else.
    The middle one is worth keeping in view: **a guard added "to be safe" cost
    a check and created a way for one contaminated read to poison every read
    after it.** Caution about a comparison is not free, and after eight rounds
    of this guard being wrong in both directions the question to ask of every
    condition on it is which of the two it is protecting against.

<a id="decision-168"></a>

168. **What the plan does to a table's parts is checked, the skip set knows
    which namespace it is in, and a failed read does not unfind what was
    already found.**
    **The parts.** 166 excluded the columns and constraints this plan moves
    from the shape comparison, which is right, and left nothing else saying
    what became of them — `tables_after` answers for the table, not its
    contents. So a column added and dropped again before the checkpoint read
    was recorded as the plan's own result. `columns_after` and the `Presence`
    on `PartChange` name the outcome now, and it is existence only, for the
    reason 166 gives about shape: what a column *is* comes back in the
    engine's spelling and holding it to the declaration would refuse valid
    applies; whether it is there has no such ambiguity.
    This is 160's lesson for the third time — an exemption is only correct
    where something else does the checking — and it is worth saying plainly
    that the pattern is now known: **every time this guard excludes something,
    the next question is what checks it instead.**
    **The namespace.** Indexes and constraints are separate namespaces to SQL
    Server: a table may hold an index `x` and a check `x` at once. The skip
    set was keyed by the bare name, so planning a change to either exempted
    both from every comparison. `Part` travels with the name now.
    **The read.** `status` assigned `state` and `detail` directly on the two
    row-read failure paths, which overwrote an unexpressible permission the
    introspection above had already established. It was known independently of
    that read, and a read that fails does not unfind it. Both go through
    `record_unreachable`, like every other outcome on that path. Swept: the
    only remaining direct assignments are inside the recorders themselves and
    on a row that has just been built, where there is nothing to preserve.

<a id="decision-169"></a>

169. **A postcondition is keyed by what it is about, not collected per change.**
    A constraint or an index whose *definition* changes is a drop and an add
    under one table, kind and name — `diff_constraints` has no `ALTER` to emit
    for either, so `by_name!` pushes both. 168 gave those parts a `Presence`
    each and held it in a `Vec`, which meant the final read-back satisfied the
    add and then necessarily failed the drop's `Absent`: a transactional apply
    rolled back the replacement it had just installed correctly, and a staged
    one stopped. Every redefinition of a unique, a foreign key, a check or an
    index was refused.
    This is 161 one field over, and one entry after it: modules were collapsed
    by name for exactly this reason, and the parts beside them were not. The
    columns already were, by being a `BTreeMap<ColumnRef, _>` — which is the
    tell. **A postcondition collection keyed by identity gets the collapsing
    for free; one keyed by nothing has to remember to.** The plan is in
    `order_key` order, which puts the drops first, so the last word on a
    `(table, part, name)` is the net one.
    Swept: the guard now holds five outcome collections — tables, roles,
    columns, parts and modules — and every one of them is a map keyed by the
    identity it speaks about. Rows and grants need no collapsing and it is
    worth recording why, so the next reader does not add it: the row differ
    branches on presence and emits at most one change per key, and `diff_roles`
    builds the granted and revoked permission sets by `difference` in both
    directions, so they are disjoint and the guard's "held, less revoked, plus
    granted" is already the net.

<a id="decision-170"></a>

170. **Two questions that shared one answer, one question asked a row too
    late, and one hazard that turned out to be unrepresentable.**
    **The columns.** `Change::columns` answers "whose *reading* did this
    move", for the caller comparing a table's rows across an apply; 162
    narrowed it by dropping nullability and deprecation, correctly, because
    neither rewrites a cell. 166 then reused that same set to excuse the
    plan's own edits from the *shape* comparison — a different question with a
    different answer. The catalog reads `is_nullable` back, so the shape
    comparison saw the plan's own `ALTER COLUMN ... NOT NULL` as somebody
    else's work and **every nullability-only plan was refused**.
    `columns_redefined` is the second question now. Deprecation is in neither
    set, and that is not an oversight: it emits no statement at all and the
    catalog reads back neither it nor the description, so it can move nothing
    in a read-back — excusing its column would drop a real comparison to buy
    nothing. This is CLAUDE.md's "a guard whose reason has gone is a filter
    nobody re-reads", in the form where the *reason* moved rather than went:
    when a set acquires a second caller, the question to ask is whether both
    callers are asking it the same thing.
    **The probe.** `rows_after` returns the rows a table will hold, and 165
    taught it that a row whose key cells no probe can spell is not an empty
    table — no answer is the honest one. But it asked that question after
    returning whatever branches it *had* built, so it only fired when *every*
    branch fell away. One unspellable row beside one spellable one produced a
    relation holding just the second: a subset presented as the whole. Which
    way it lies depends only on which side of the constraint it is — on the
    parent side a child matching the missing row reads as an orphan and a
    valid foreign key is refused; on the child side an orphan hidden in the
    missing row is not counted, the probe reports zero, and under
    `apply --staged` the table and row statements commit before
    `ADD FOREIGN KEY` fails. The check is hoisted above the branches.
    **The one that was not there.** A review also reported that a table
    replaced under its own name — `DropTable` plus `CreateTable` for one name,
    with a new uid — would have the guard compare two unrelated tables with an
    empty skip set. It would; the plan cannot exist. `resolve` binds a
    declared name to the uid that name already has, so a drop intent for a
    still-declared name is `UnusedIntent`; a rename onto an occupied name is
    reported as a target-collision blocker before it can consume its source.
    Rather than add a case for replacing a still-declared table, the rule it depends on is now
    pinned by a test that names the guard, so if identity ever stops working
    that way the guard is what to revisit. **A hazard made unrepresentable
    still needs the invariant written down** — otherwise the next reader adds
    the case, or removes the rule.

<a id="decision-171"></a>

171. **A probe may only name what the catalog holds now — and a column this
    plan adds is not that.**
    Found by sweeping 170's shape rather than reported: `rows_after` had just
    been taught that a relation missing a row is not that table's contents,
    and the same function was building the stored branch out of
    `alias.[column]` for a column the plan had yet to add. `AsStored` exists
    to translate a plan's names into the catalog's, and `AsStored::column`
    falls back to the declared name when it knows no other — which is right
    for a rename and wrong for an addition. Measured: the probe fails with
    `Msg 207, Invalid column name`. A probe that throws is reported as
    unchecked and the apply proceeds (124), so the effect is the silence, not
    a failure.
    It was three probes, not one: the foreign-key relation, `AddUnique`'s
    duplicate probe and `SetPrimaryKey`'s null and duplicate probes. And the
    skipped check is precisely the one worth having — a key over a column that
    has just arrived, where every existing row holds the same value in it, is
    the case that *fails*.
    What those rows will hold needs no asking, and the rule is an engine fact:
    **SQL Server backfills only a NOT NULL column.** Measured on 2025 —
    `ADD col NULL DEFAULT 'zz'` leaves every existing row NULL, while
    `ADD col NOT NULL DEFAULT 'yy'` writes `yy` into all of them, which is
    also why NOT NULL is the only kind whose value source the engine insists
    on (`has_required_add_value_source`). So `Added` is three cases: `Null`,
    `Backfilled(constant)`, and `Unspellable` for an identity or a default
    that is not a constant — the same three-way answer an unprobeable default
    gets everywhere else (117).
    Substitution is not uniformly literal, which is the part only the engine
    could say. A constant is fine in a `SELECT` list and in `WHERE x IS NULL`,
    but `GROUP BY NULL` is `Msg 164, Each GROUP BY expression must contain at
    least one column that is not an outer reference`. A column every row
    agrees on groups nothing, so it leaves the `GROUP BY` list altogether —
    grouping by `(a, k)` where every row shares `k` is grouping by `(a)` — and
    an empty list means one group holding every row, which is
    `CASE WHEN COUNT(*) > 1 THEN COUNT(*) ELSE 0 END`.
    The live test is the point of this entry. Every claim above is a claim
    about the engine, and a unit test can only confirm that the SQL says what
    I think it says — which it did, while the engine refused it.

<a id="decision-173"></a>

173. **An exclusion the size of its reason: per field, not per column — and
    "narrow" includes NOT NULL.**
    **The column.** 166 made a touched table's shape comparable and excused
    the columns the plan moves; 170 gave that exclusion its own question. Both
    excused the whole `Column`. The reason is smaller than that: only the
    engine's stored form can say what a *retyped* column became, which is why
    the type is excused at all — but that column's default, nullability,
    identity and description still came back from two reads, like everything
    else on the table. So a default another session added beside the plan's
    own `ALTER COLUMN` was recorded as this plan's result, and `verify`
    reported clean ever after. `ColumnField` makes the exclusion field-sized.
    This is the third form of one lesson, and worth stating as the general
    rule: **an exclusion is correct only where something else does the
    checking, and it must be no wider than the thing that is unknowable.** 160
    found the first, 168 the second ("every time this guard excludes
    something, the next question is what checks it instead"), and this one
    says the exclusion's *shape* is part of the question, not only its
    existence.
    Two things fell out of asking which fields actually move. A type change
    folds a nullability change into itself (§12), so it excuses the
    nullability only where `from_nullable != to_nullable` — a restatement that
    changes nothing leaves a value two reads still agree on. And measured on
    the engine: `ALTER COLUMN` leaves the default constraint's stored
    definition untouched, so a retyped column still answers for its default.
    Identity and description are excused by nothing at all, because no change
    in this model moves either; the whole-column exclusion had been hiding
    both.
    **The contraction.** `change.expand-contract` counted a drop and a
    narrowing type change, and its own description says "add and drop **or
    narrow**". A column that stops accepting NULL accepts less than it did —
    `intrinsic_risks` calls it "the same data hazard as tightening an existing
    nullable column" — and it was neither half. Two spellings were missing:
    `AlterColumnNullability` to NOT NULL, and the type change that folds one
    in, which then carries `NotNull` rather than `Narrowing`. A project
    raising the rule to `error` could still ship exactly the plan it forbids.
    Keyed on the change and not on `RiskClass::NotNull`, deliberately: a NOT
    NULL column *addition* with no value source carries that risk too
    (SPEC §7.1), and it is an add. Asking the risk would make one added column
    both sides of the pattern and fire the rule on it alone — which is why the
    named arms in that match are worth the length they cost.

<a id="decision-174"></a>

174. **One read for the staged baseline, and a probe over the rows its own
    statement will meet.**
    **The read.** [`baseline_state`] already carried the rule in its own doc —
    "one read and two projections, never two reads: two reads would ask the
    engine the same thing twice and could get two answers, which is the very
    thing the comparison exists to detect" — and the staged path took two.
    Between them ran the preflight probes and, on a resume, the role checks.
    Anything another session changed in that window was already in the second
    read, so it became the baseline every later checkpoint was measured
    against: never reported, and finally written down by the closing ordinary
    snapshot as this plan's own result, which is the state `verify` compares
    against ever after.
    A staged apply needs a third difference the transactional one does not: a
    checkpoint watches every module the plan *names*, including ones it has
    yet to create (164), while the checksum must be taken over exactly the
    managed set the plan was pinned to or no plan would validate at all. So
    `staged_baseline` takes two *cuts* of one read rather than one cut of two.
    `pull` and `cut` were split out for it, and the two branches now hand the
    baseline back beside the statement to start at, so there is no way to
    reach the loop with a baseline from some other read.
    Note what the refactor nearly dropped: each of the two reads refused
    `managed_limitations` over its own module set, and the wider one is what
    catches a module this plan is about to write that the catalog cannot read
    back (491edd9). One read refuses over the union, which is the same thing —
    and a `debug_assert` records that the union is the watched set.
    **The probe.** `order_key` runs every row change (11, 12) before every
    constraint a plan adds (13), and `AddCheck`'s probe counted the rows
    standing now. A plan that deletes its own violations and then tightens was
    refused for violations that will be gone; a plan that writes violating
    rows was told there were none. The ordering is what draws the boundary,
    and it is worth stating: **only a probe whose statement sorts after the
    row changes has this problem.** `AlterColumnType`, `AlterColumnNullability`
    and `AddColumn` all sort before them (8-10), so reading the current table
    is exactly right for those. At 13 with `AddCheck` sit `AddUnique` and
    `SetPrimaryKey`, which have it too.
    The check's own fix cannot be the foreign key's. `rows_after` builds the
    rows a plan will leave *for a named column list*, because a foreign key's
    columns are the constraint; a check is an arbitrary predicate over columns
    the plan does not carry, and the expression is deliberately never
    rewritten. So: minus the rows it deletes, which is exact — and no answer
    at all where it inserts or updates, which is what an unspellable row gets
    everywhere else (117, 165, 171).

<a id="decision-179"></a>

179. **The read-back omitting a NULL excuses its absence, and nothing else.**
    `Change::row` dropped every cell the plan writes as an explicit NULL from
    the expectation, because a NULL in a column with no default is omitted
    from the read-back (`canonical` returns `Ok(None)` and the cell never
    reaches `cells`). True — and it justifies not *demanding* the cell, which
    the caller already handles: it skips a column the read-back does not
    carry. Dropping the expectation instead threw away the other half. A
    session that wrote a value into that cell between the DML and the
    checkpoint read left it *present*, and nothing looked; the broad row
    comparison skips a key the plan names, so that was the only check there
    was. Keeping the NULL in `RowAfter::Holding` costs no false refusal and
    catches it, in both shapes — a column with no default omits the cell, and
    one *with* a default reads the NULL back explicitly (`Ok(Some(Null))`),
    where the comparison now matches it outright.
    A cell set to `DEFAULT` stays out, and it is worth saying why it is not
    the same case: its value is omitted only where the engine *confirmed* it
    at the default, while one the engine could not evaluate (`NEWID()`) comes
    back carrying its value. Presence there disproves nothing, and demanding a
    value this change cannot name would refuse a valid apply (117, 165).

<a id="decision-181"></a>

181. **A table this plan creates answers for its shape, by name.** 163 gave a
    created table a synthetic baseline of *no rows*, so an undeclared row that
    arrived in one was caught. Its shape had no such baseline: the comparison
    needs a `before` entry and a created table has none, and `CreateTable`
    names no column and no part of its own, so `columns_after` and
    `constraints` answered for nothing either. Between the two, the only thing
    checked about a table this plan had just created was that it existed. A
    DDL trigger, or another session between a staged `CREATE TABLE` and its
    checkpoint, could add a column or an index to it and have that written
    into the checkpoint as this plan's own result — after which `verify`
    reported clean for good.
    **By name, never by value**, and that is the whole reason this is possible
    at all. What a created column *is* comes back in the engine's spelling —
    which is why 166 left a touched table's shape alone until it had two reads
    to compare, and why a created table has never been held to its
    declaration. A *name* has no such ambiguity: the plan declared these
    columns and these parts, and a name that is not among them was put there
    by somebody else. Measured beforehand, because the engine adds names of
    its own where it can: the index query already excludes
    `is_primary_key = 1` and `is_unique_constraint = 1`, so the indexes a
    primary key and a unique constraint create do not come back as indexes and
    cannot read as unplanned.
    The missing direction is gated on `Settled::Whole` and the extra one is
    not, for a reason the two do not share: a foreign key is split out of the
    `CREATE` into a change of its own, so at a checkpoint it may legitimately
    not be there yet — while a column nobody declared is somebody else's work
    whenever it appears.

<a id="decision-182"></a>

182. **A created table's `CREATE` payload is not everything it will hold.**
    181 compared a created table's component names against
    `CreateTable.table`, and that payload has had its foreign keys taken out
    of it: `diff_partial` does `std::mem::take(&mut table.foreign_keys)` and
    emits an `AddForeignKey` for each, because they sort after every create —
    a new table's key may reference another new table. So the expectation read
    off the payload was empty, and the key the plan itself adds came back as
    movement: **every created table with a foreign key refused**, one commit
    after the check was added.
    The expectation is the payload plus what the plan's own part changes add.
    Keyed by `Part`, so a future split of a unique, a check or an index needs
    no second fix; the columns are left to the payload, and that is not an
    oversight — the differ splits only foreign keys and rows out of a
    `CREATE`, and rows are not shape.
    **The real finding is in the test suite.** Nothing in the live CLI flow
    ever applied a plan that *creates* a table with a foreign key — the word
    `references` did not appear in `flow.rs` at all — which is why 181 shipped
    with the mistake and why 40 live tests stayed green over it. The check I
    reported as retiring the false-refusal risk could not have. A live test
    applies one now, over two tables the same plan creates so the split is
    real, and it fails against 181's code. **A guard that has no live plan
    exercising the shape it guards is not covered by the suite being green.**

<a id="decision-183"></a>

183. **A part is not just a name, where the declaration says what it is.**
    181 compared a created table's components by name, on the argument that
    what they *are* comes back in the engine's spelling. That argument is
    right about two fields and wrong about the rest: a primary key put back on
    different columns, or under a different declared name, is `Some` on both
    sides and a presence check accepts it. So is a unique constraint moved to
    another column under its own name.
    The line is which fields the engine renders for itself. A **check** is
    nothing but an expression and SQL Server rewrites it — 167's problem — and
    an **index's filter** is one too; those two stay with the name comparison.
    Everything else is structure the declaration states outright: a unique
    constraint's columns, an index's columns, includes and uniqueness, a
    primary key's columns, and its name **where the declaration gives one** —
    `name: None` leaves the naming to the database, and `PK__t__3213E83F` is
    not movement.
    Verified against the engine rather than argued: the live apply of a
    created table now carries a named primary key, a unique constraint and an
    index with an `INCLUDE`, and it passes — so the read-back really does
    match the declaration in every field this compares. That test proves the
    absence of a false refusal; the unit test proves the detection. Neither
    proves the other, and after 182 it is worth writing down that they are two
    different claims.

<a id="decision-184"></a>

184. **The last of the created table's parts: a foreign key's definition.**
    182 restored the foreign keys the differ takes out of a `CREATE`'s payload
    — but only their *names*, because that entry was about the false refusal.
    183 then gave every other part a value comparison and left this one where
    182 had put it, so a key replaced under the planned name, pointing at
    different columns or carrying a referential action nobody approved, was
    accepted and recorded.
    Nothing about it is the engine's to render — the child columns, the parent
    and its columns, and the two actions are all structure — so all of it is
    compared. Its definition comes off the `AddForeignKey` change rather than
    the payload, which is the only reason it needed a collection of its own.
    Measured, not assumed: the live created-table apply now declares
    `on_delete: cascade` on one of its keys, so the comparison runs against a
    non-default action that the catalog has to read back faithfully, and it
    passes.
    Three entries to finish one guard is worth noting for what it says about
    the shape rather than the bug: **each was a smaller version of the same
    question — what does this plan promise about the object it creates — and
    each answered it for one more field.** The remaining two, a check's
    expression and an index's filter, are answered by nothing here on purpose,
    because SQL Server rewrites them (167).

<a id="decision-185"></a>

185. **A created column's stable fields, and the measurement that decided
    which ones they are.** 181 compared a created table's columns by name
    alone, on the argument that what a column *is* comes back in the engine's
    spelling. Two P1s later that argument has been split properly: the
    spelling problem is real for exactly two fields, and everything else was
    being excused for nothing.
    Compared now: **nullability**, **identity**, and **whether the column has
    a default at all** — the default's *text* is the engine's, `0` comes back
    `((0))`. And for an index, **whether it has a filter**: the predicate's
    text is rewritten like a check's, but its presence decides which rows the
    index covers and is not the engine's to change.
    **The type is not compared, and this is the entry's real content.** I
    tried it, because the reviewer named it and my own reason for excluding it
    was vague. It passed every test — including a live apply declaring
    `decimal(18,2)`, `char(3)`, `datetime2(3)`, `nvarchar(max)`,
    `varbinary(16)`, `bit` and `int` — and then failed the moment that test
    grew a column declared as bare `decimal`. Measured on the engine: SQL
    Server fills in a type's defaulted arguments, so `decimal` is stored
    `decimal(18,0)`, `char` as `char(1)`, `float` as `float(53)` and
    `nvarchar` as `nvarchar(1)`. Comparing the declared type against the
    catalog's refuses an apply that is exactly right. Those four columns stay
    in the live test so that the next person to think this is safe finds out
    in one run.
    The general form is worth keeping: **"the engine renders this" is not one
    property of a value, it is a property of each field**, and the way to find
    out which is to compare and see what the engine refuses.

<a id="decision-186"></a>

186. **A created column's type is compared, normalized.** 185 concluded the
    type could not be compared at all, on a measurement: SQL Server fills in a
    type's defaulted arguments, so a declared `decimal` is stored
    `decimal(18,0)`. The measurement was right and the conclusion was one step
    short — `Dialect::normalize_type` expands *exactly* those same arguments,
    which is what it is for. Normalized on both sides, a bare `decimal` and a
    stored `decimal(18,0)` are one type and `int` becoming `bigint` is not.
    So the guard takes a dialect now. It had none, which is why the question
    looked settled: the reach of a comparison was being decided by what was in
    scope. A type the dialect cannot normalize gets no answer rather than a
    wrong one, like every other unspellable thing here.
    The live created-table test keeps its bare `decimal`, `char`, `float` and
    `nvarchar` columns — they were added in 185 to prove the comparison
    impossible and now prove it correct, which is the better job for them.

<a id="decision-189"></a>

189. **A planned column or part is held to what the plan gives it, not to
    being there.** 168 gave the parts and columns a plan moves a presence
    check, because the shape comparison excludes exactly those and nothing
    else said what became of them. Presence was the wrong size for the gap
    (173): the exclusion is of a *definition*, so a column another session
    retyped after the plan's own `ALTER`, or a constraint it dropped and
    recreated under the plan's name with other columns, was there — and was
    recorded as the plan's result. The created-table block of 181–186 had
    meanwhile learned to compare the same fields by value; the columns and
    parts of an existing table were the second instance of the shape.
    Each column change now promises a value for each field it excludes
    (`columns_promised`, the mirror of `columns_redefined`, with a test that
    holds the two together), and each part change carries the definition it
    adds — `PartAfter::Standing(PartDefinition)`, so a part cannot be checked
    for presence without the checker holding what it was meant to be. The
    comparison is the one the created-table block already makes: the
    normalized type, the nullability, the identity and the default's presence
    for a column; the columns for a unique, all of a foreign key, the
    structure and the filter's presence for an index, the columns and the
    declared name for a primary key. A check has nothing but an expression
    the engine rewrites, and keeps its name check (183).
    The other half is the `Whole` exclusion itself. "The column is on one side
    only" is true of exactly one read — the one spanning the statement that
    adds or renames it. On every later read of a staged run the column is on
    both sides, a read-back each, and excusing it by name left it exempt for
    the rest of the run. A renamed column is now followed from its old name
    to its new one across that read, and an added or renamed column is
    compared like any other wherever it is on both sides.
    Proved against the engine both ways: a live plan adds a column with a
    bare `decimal` and a default, retypes, loosens, defaults, replaces the
    key with an unnamed one, and adds a unique, an index and a foreign key
    to a table already there, and applies — before it, no live plan had added
    any of those to an existing table at all (182).

<a id="decision-190"></a>

190. **A refusal names the remedy of the read that found the change.** A
    staged run compares each read with the one before it and refuses on
    movement (159). The message said the same thing at every read: "the
    checkpoint holds the database as it stands, this change included —
    resuming accepts it." That is true at a checkpoint, whose read *is* what
    the checkpoint records. It is false at the closing read: that read comes
    after the last checkpoint was written and nothing records it, so a
    `--resume` measures the live database against a checkpoint that does not
    hold the change and refuses it as moved — the very remedy the message
    named cannot work. `staged_movement` now takes `StagedRead::Checkpoint`
    or `StagedRead::Closing` and says, at the close, that no checkpoint holds
    the change and a resume will refuse: undo it and resume, or baseline and
    plan from there.
    Measured: the live staged test makes a hand change after the checkpoint
    and the resume refuses it with "has moved since the checkpoint"; undone,
    the same resume accepts the checkpointed change and closes. The closing
    window itself cannot be hit by a test — the only statement in it is the
    ledger insert, whose `OUTPUT` clause forbids a trigger on the table — so
    the message is pinned by a unit test and the resume behaviour by the live
    one.
    The same message carried a second wrong remedy, older: the guard's own
    refusal ended "nothing has been applied — the transaction was rolled back;
    then apply again", and every staged refusal wrapped it, one line above
    "nothing was rolled back". The guard now states the finding and the
    reason and no remedy; the transactional caller and the two staged reads
    each append their own. A shared function does not know what its caller
    can do about what it found.

<a id="decision-191"></a>

191. **A cell the plan leaves to its default is held to being at it, at the
    closing read.** `RowAfter::Holding` carried only the cells the plan
    *spells*; a `DEFAULT` cell was dropped because the plan cannot name the
    value the engine will put there, and a read-back omits a cell at its
    default, so there seemed to be nothing to compare (165). Dropping it also
    dropped the other half: a value another session wrote into that cell
    between a staged `UPDATE` and its checkpoint read was present in the
    read-back and compared with nothing (the same shape as 179, for NULL).
    The cell stays, as `CellAfter::AtDefault`, and the guard holds it to
    *being omitted* — under two conditions that are the whole of what makes
    "omitted" mean "at the default". First, the read has to be the closing
    one: it is spelled against no recorded row, so a cell the engine confirmed
    at its default is omitted and one that is there is not at it. A checkpoint
    read is spelled against the checkpoint before it, and keeps an at-default
    cell explicit wherever that checkpoint spelled it, so it can say nothing;
    `Settled::Closing` names the difference. Second, the column's default has
    to be one the engine is asked to confirm — a literal, on a type with `=`.
    A `NEWID()` cell comes back with its value on every read, and holding it
    to omission would refuse every plan that touches such a row. The row
    reader already draws exactly that line to build its query; the guard asks
    the dialect the same question (`Dialect::reads_back_at_default`, one
    function behind both), because two spellings of the line would drift.
    Measured: a live plan sets a `nvarchar` cell with a literal default to
    `DEFAULT` beside a `uniqueidentifier` left to `NEWID()` and applies clean.
    Before it no live plan had set a cell to `DEFAULT` at all.

<a id="decision-280"></a>

280. **The apply guard keys a column's promises by field, and the last one
    wins.** 271 splits a retyped column's default into two changes — the old
    default out, the type changed, the new one in — and the apply guard
    collects what a plan promises about each column field, holding the closing
    read to every entry. Two changes about one field meant two promises about
    it, `Default(false)` and `Default(true)`, and no read satisfies both:
    measured through the CLI, the statements applied and the guard then called
    its own result movement —

    ```text
    error: `…/pbps_cli_retypedefault` moved while this plan was running, and
    not because of it:
      dbo.t column `n` does not have the default this plan gives it
    ```

    — and rolled the transaction back, so the migration could not be applied at
    all.

    This is DECISIONS 169 one field along: there, a constraint redefined under
    one name promised `Absent` from its drop and `Present` from its add, and the
    fix was to key the parts by name and keep the last word. The column fields
    were a `Vec` only because, until 271, no plan said two things about one
    field. They are keyed by `(column, field)` now, and the plan is in
    `order_key` order, so the last promise about a field is the net one.

    The pairing of a promise to its field lives on `ColumnPromise::field` in
    the model rather than at the guard: the caller that must key them is not the
    place to decide what each promise is about, and a second caller would have
    spelled it again.

<a id="decision-418"></a>

418. **The apply's read-back on PostgreSQL runs inside the apply transaction
    under a savepoint, and the command says which kind of read it wants.** 147
    puts the read-back and the ledger record inside the transaction that
    built what they record, and 253 makes the pull refuse to run inside a
    caller's transaction. Both are right, and together they refused every
    `bootstrap` and `apply` on PostgreSQL through the binary — the first
    end-to-end run through the seam of 417 ended in `this connection already
    has an open transaction, and a pull cannot run inside one`. SQL Server
    never asked the question: its catalog reads the same inside and outside
    a transaction, so the CLI had never had to say.

    The resolution is two entry points, not a flag on the pull, and a `Read`
    the command names at every catalog read: `Snapshot`, the pull of 250,
    which takes its own `REPEATABLE READ READ ONLY` transaction and keeps
    refusing inside anybody else's; and `InsideOwnTransaction`, which
    *requires* an open transaction and reads under a savepoint. They answer
    different questions — "what does the database look like" against "what
    did this transaction build" — and 253's point stands: the second is not
    an accommodation of the first, and a caller that asks it outside a
    transaction is refused by name, because "nothing uncommitted to see" is
    the first question's answer and not this one's. The CLI passes
    `InsideOwnTransaction` at exactly two sites, the read-backs of
    `bootstrap` and `apply`, and `Snapshot` everywhere else, including the
    baseline read the apply takes *before* its transaction opens.

    Measured on 18.6: the savepoint carries `SET LOCAL transaction_read_only
    = on`, and a `CREATE TABLE` under it fails with SQLSTATE 25006; after
    `ROLLBACK TO SAVEPOINT` the caller's transaction is writable again, its
    `search_path` is what the caller set, `transaction_read_only` is `off`,
    and `txid_current_if_assigned()` is still non-null — the read left
    nothing behind and ended nothing. The canonical settings 250 and 254 pin
    are `set_config(…, is_local)` and go back with the savepoint, which is why
    the savepoint is rolled back on the success path too: a read has nothing
    to keep and everything it set has to go. The read sees the transaction's
    own uncommitted `CREATE TABLE` and the row it inserted, which is the
    whole reason it exists. Reverting the CLI's two `InsideOwnTransaction`
    sites to `Snapshot` brings the original refusal back in both flow tests.

<a id="dec-319-1"></a>

**DEC-319.1. A PostgreSQL plan pins every routine not held only by
superusers, under the environment's key, and `apply` refuses a changed pin before its
probes, before its DDL and before it commits (#319; planned).** The drift check
(SPEC §7.6) covers the managed set, and SPEC §8.2 leaves everything else out of
the comparison. It says nothing about the code a plan runs. A CHECK the plan adds
can call `ext.helper(x)`, which the plan does not manage. The helper's owner can
replace it after approval, and the deployer runs the new body twice: once in a
pre-flight probe, and again in the DDL that validates the constraint. #805 made
the probe read-only (DECISIONS 537), so a write from the probe no longer
persists. The DDL runs inside the apply transaction, though, and whatever it
writes commits with the approved changes.

*Why not the obvious fixes.*
- *Lock the routines.* A deployer that is not a superuser cannot: `SELECT …
  FROM pg_proc … FOR SHARE` is `permission denied for table pg_proc`, measured
  on 18.6 for this entry and earlier for module rebuilds (DECISIONS 420). The
  apply can only detect a replacement, not prevent one.
- *Compare the text a plan saw.* A plan file that carries an external routine's
  body, or a bare SHA-256 of it, publishes a guessing oracle for any literal in
  that body. DEC-952.1 answers exactly this: the fingerprint is an HMAC under the
  environment's key.
- *Pin only what the plan can reach.* The first drafts of this entry did that.
  They started from the plan's text and followed calls, triggers, foreign-key
  actions and stored expressions. Each review round of PR #985 found another
  surface through which the engine runs a routine that no statement names:
  - a cascade's target table;
  - a table a trigger body writes;
  - a domain's default;
  - a row-security policy;
  - an `INSTEAD OF` trigger, or a view rewritten to its base table.

  The list does not end there. Operators, casts, a type's I/O functions and
  names built at run time lie on the same road. Every entry on it is a
  `pg_proc` row, though. Pinning the rows, rather than the paths to them, ends
  the enumeration. This is the lesson of DEC-952.1 (#880) in another place: stop
  re-deriving what the engine will do, and pin what it does it with.
- *Resolve bindings as the engine does.* A declaration the plan has not created
  yet has no bindings to read, and a PL/pgSQL body binds when it runs. Engine
  resolution belongs to the resolver (SPEC §9.3.2, #614).

*What is pinned.* Every routine (`pg_proc` row), managed or not, that meets
both conditions:
- it does not live in a temporary schema. The engine answers this with
  `pg_is_other_temp_schema(pronamespace)` and `pg_my_temp_schema()`, so no
  schema is excluded by name. The deployment session cannot resolve another
  session's `pg_temp_N` routine, and the engine drops that routine when its
  session ends, so a plan made while one existed would refuse after ordinary
  cleanup. pbps creates no temporary routine of its own;
- it is not held only by superusers. It is held only by superusers when its
  owner is a superuser and every role that is a member of the owner, by any
  path and with any grant options (`pg_has_role(role, owner, 'MEMBER')`), is
  a superuser too. Every other routine is pinned, whatever its owner can log
  in to, create or be granted.

  The test deliberately asks less than "who can replace this routine today".
  Two review rounds of PR #985 narrowed it that way. The first counted only
  roles with the owner's rights through `USAGE` or `SET`. The second counted
  only actors in DEC-862.1's sense, meaning login roles and roles with a
  session. The next round found three holes:
  - `CREATE OR REPLACE` also needs `CREATE` on the schema;
  - a backend that ran `SET ROLE` to the owner keeps that role after its
    membership is revoked, while `pg_stat_activity` still names its login role;
  - the answer changes the moment a grant does.

  Each narrowing re-derives the engine's authorization, the second
  implementation DECISIONS 521 and DEC-952.1 warn against, and each can only
  drop a routine that should be pinned. The broad rule's error runs the other
  way. A routine that no one could in fact replace is pinned anyway, and the
  cost is that the environment needs a fingerprint key, which DEC-952.1
  already asks every environment to have. The one exclusion that stays is
  superuser-only ownership. Nothing pbps checks restrains a superuser, and
  measured on 18.6, `MEMBER` counts even `INHERIT FALSE, SET FALSE` grants,
  so no member that could act slips through it. A session that assumed a
  superuser role while it was a member, and kept it after the revoke, is a
  superuser session.

No other schema is exempt. The built-in routines in `pg_catalog` and
`information_schema` belong to the bootstrap superuser and fall out of the set
by the second condition. A routine that an administrator created or re-owned
there, where a plain role can replace it, stays in.

The last condition is the one that keeps the set small. Only the owner, a
member of the owner's role, or a superuser can replace or re-own a routine, and
nothing pbps checks restrains a superuser. Membership counts even into a
superuser's role. A plain role granted `postgres` replaced a `postgres`-owned
function with `CREATE OR REPLACE`, measured on 18.6, while `rolsuper` stayed
false for it. Extension members are included on
the same terms. A trusted extension that a non-superuser installs still creates
its routines owned by the bootstrap superuser: pgcrypto's 37, installed by a
plain role with `CREATE` on the database, measured on 18.6. Owning the
member routines is not the only way to change them, though. The extension's
owner, here the plain role, can run `ALTER EXTENSION … UPDATE`, whose trusted
script replaces member routines and leaves their owner alone, or it can drop
the extension and create it at another version. So an extension member is also
in the set when its extension is not held only by superusers, by the same
test applied to the extension's owner. Its pin input then carries the extension's OID and
`extversion` beside the routine. A routine that changes owner,
or whose owner's membership changes, can enter or leave the set, and that is a
change like any other.

*What a pin holds.* One entry per schema, keyed by the schema's OID. Each is
an HMAC under the environment's key (DEC-952.1), with rule
`pbps/external-routine-pin/v1` and the namespace OID as component. The input
is the canonical list of the schema's in-scope routines, in OID order. Each
routine contributes:
- its OID and its **whole `pg_proc` row**, read with `to_jsonb` minus `oid`
  and minus `proacl`. That covers the body, volatility, `proparallel`,
  strictness, `SECURITY DEFINER`, `proconfig`, cost and the owner. The column
  list is not chosen property by property, because a property left off a
  hand-picked list is a replacement the pin cannot see. An aggregate that
  changes only `PARALLEL` differs only in `proparallel`, which no
  `pg_aggregate` column holds. A column a later PostgreSQL release adds joins
  the input without a code change. A plan and an apply on different server
  versions refuse before any comparison, because the release is recorded
  beside the pins;
- its `proacl` as `aclexplode` rows of grantor OID, grantee OID, privilege and
  grant option;
- for an aggregate, its **whole `pg_aggregate` row** too;
- for a member of an extension a non-superuser controls, the extension's OID
  and `extversion`.

Every reference in the input is an OID and never a name. Every `reg*` column is
cast to `oid`: the `regproc` ones (`prosupport`, the `pg_aggregate` support
functions) and the `regoperator` sort operator `aggsortop`. `to_jsonb`
renders them as names, and an operator's name carries its operand types'
names. A name is what an approved plan
legitimately changes. Measured on 18.6: a routine taking a table's row type
kept a byte-identical row, less `proacl`, while the plan-shaped `ALTER TABLE …
RENAME` and `ALTER ROLE … RENAME` ran. Its `pg_get_function_identity_arguments`
went from `t1` to `t2`, and its `proacl` text changed with the grantee's name.
Pinning names would make the closing check blame an approved rename on an
external change. The engine binds these references by OID, so the OID is also
the faithful input. A routine re-pointed to a different object differs, and a
renamed object does not. Nothing is deparsed: `pg_get_functiondef` renders
what the row already holds, and it refuses an aggregate (SQLSTATE 42809).

The deployer can read all of these for another role's routine, even one whose
`EXECUTE` is revoked from it (measured on 18.6). The saved plan records
each namespace OID with its name at plan time (for messages only), its digest
and its routine count, plus the key identifier, in a new plan version. The plan checksum covers them as it covers the rest of the
file. One entry per schema keeps a plan small when an extension brings hundreds
of routines. It still lets a refusal say where the change is.

*When the plan has no key.* `plan` refuses a database-origin plan whose pin set
is not empty when its environment has no fingerprint key. The remedy names
`pbps key generate` and `fingerprint_key_env` / `fingerprint_key_file`. A
database in which every unmanaged routine is held only by superusers needs
no key. Its plan records only the unkeyed managed pins. Pins apply
under every `unmanaged` mode, `error` included. Extension members are left out
of the unmanaged inventory (DECISIONS 305), so a re-owned extension routine can
sit in an `error` plan's database without refusing it.

*When `apply` checks.* `apply` recomputes the set, then compares it with the
plan's pins in three places:
- under the deployment lock, after the drift check and before `preflight`, in
  a snapshot read of its own;
- inside the apply transaction, after `BEGIN` and before the first statement;
- after the read-back and before `record`, so a replacement during the DDL
  rolls the apply back.

*Managed routines are pinned too.* The drift check and the read-back compare
a managed routine's definition, not its owner or its ACL. A managed
`SECURITY DEFINER` routine that another session re-owns after approval would
run the same body as a different principal. The first two checks hold every
pinned routine, managed ones included, because no statement has run yet. The
closing check leaves out the routines the plan itself touches: those it
creates, replaces, rebuilds, renames or drops. Their new state is what the
read-back compares. It holds every other routine, managed or not, to its
plan-time pin.

A managed routine's pin needs no key. Its body is the declaration, which the
plan file and git already carry, so a digest of its row reveals nothing the
plan does not. Managed routines are therefore pinned apart: a SHA-256 per
routine OID over the same row input, which the plan checksum covers. The
keyed per-schema entries hold only unmanaged routines, whose bodies are
private (DEC-952.1). Moving a routine between the two sets, by the plan
creating or dropping it or by `ids` naming it, is the plan's own change and
follows the closing-check rule above. A digest mismatch, a schema that gained or lost pinned
routines, an unreadable catalog row and a key identifier that is not the plan's
all refuse. None of them reads as "nothing changed" (AGENTS.md: absent, empty
and unreadable differ). Each refusal names the schema and gives the remedy,
which is to replan.

A staged plan is checked under the lock before pre-flight, and again before
each step's statements and after each step, before its checkpoint is recorded.
The last step is covered too. It gets no transaction that spans its steps. A
step's statement is permanent once it commits, so a mismatch found after it is
a post-commit guard failure under DECISIONS 159: the step's checkpoint is
recorded, and then the apply stops with the refusal. A replacement is caught
at the step it happened in, not rolled back past it.

The price is over-inclusion. A change between plan and apply to any pinned routine
refuses the apply, even one this plan never runs. The
remedy is a replan, as it is for drift in the managed set. The window between
plan and apply is short, and an external routine that changes inside it is
exactly what an operator should look at before running approved DDL.

*What it does not cover.*
- A new call site attached after approval to a routine that has not changed,
  such as a new policy, domain default or trigger on an unmanaged object. That
  is not a replaced routine. Unmanaged objects other than routines stay outside
  the comparison (SPEC §8.2). Triggers on written tables are DECISIONS 445/451's
  execution-trust check.
- A routine that only superusers can replace. A superuser can change anything
  this check reads, and the check itself.
- A replace-and-restore that falls entirely between two checks. Probes are
  read-only and the last check precedes the commit, so what such a body can
  still do is non-transactional: the SPEC §7.6 limit, "protection against
  arbitrary external writes after the last observation".
- SQL Server. A CHECK there can call a scalar UDF too; its pins are #984.
- #322, which is about binding at run time. A `SECURITY DEFINER` routine the
  plan created binds its unqualified helpers when a user calls it, long after
  `apply` has checked anything.
