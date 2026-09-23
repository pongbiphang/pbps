# Plans and their pinning

The saved plan: what it carries, how it is pinned to the apply, staged plans,
checkpoints and `bootstrap`. Part of the [decision record](../DECISIONS.md),
which says how to add an entry here.

<a id="decision-6"></a>

6. **Review has two layers**: the MR reviews the desired-state change (offline
   plan is a preview only); the deployment gate reviews the per-environment
   `plan --db`, and the checksum pins that plan to the apply. plan.sql is never
   hand-edited.

<a id="decision-7"></a>

7. **`plan` writes only the ids file, never the user's YAML**; `fmt` strips
   redundant `renamed_from`; `plan --check` is strictly read-only.

<a id="decision-10"></a>

10. **`apply` is one transaction per plan, all or nothing**: non-transactional
    statements fail at plan time unless the plan is staged (see 26);
    pre-flight (rename impact, SCHEMABINDING) runs before the first statement.

<a id="decision-22"></a>

22. **Probes are built per plan, not per change.** They run before the first
    statement, so every name in them must be the one the catalog still has —
    `preflight::AsStored` translates through the plan's renames, and tables the
    plan creates are skipped. Check expressions are deliberately *not*
    rewritten; that probe fails to run and is reported as unchecked.

<a id="decision-23"></a>

23. **A saved plan carries `origin`, `mode` and the post-plan `ids`.**
    `Preview` is a value in the file, so `apply` refuses it structurally; the
    ids make apply self-contained on a host with no checkout, and recording the
    baseline's mapping instead would say a rename never happened.

<a id="decision-24"></a>

24. **`apply` takes the lock before the pre-flight, and releases it on every
    path.** A check that passed while another pipeline was mid-apply was
    answered about a moving database; a lock left behind blocks the pipeline
    that would fix it.

<a id="decision-27"></a>

27. **The dev database is always optional** (`plan --dev`). Without one the
    preview degrades to lightweight normalization and says so; with one, the
    rehearsal reports structural differences as a failure and spelling
    differences *with the engine's stored form*, which is the only place that
    form can come from. `--dev` and `--db` are refused together: a rehearsal is
    a preview's question.

<a id="decision-32"></a>

32. **A staged plan is one logical change, and its mode lives in the file.**
    `apply --staged` runs it outside a transaction with a `staged` ledger entry
    per completed statement; `--resume` re-checks the live state against the
    checkpoint before continuing. An unfinished checkpoint makes the
    environment mid-deployment: `plan --db` and a fresh `apply` refuse, and
    `status` reports `staged`.

<a id="decision-33"></a>

33. **A checkpoint's `ids` are the names at that checkpoint**, not the plan's.
    One `RenameTable` can take two statements, and between them the table is at
    `[new schema].[old name]` — a name in neither the baseline nor the plan.
    Each `Statement` therefore declares its own renames, the staged loop
    replays them, and the checkpoint records what the catalog actually has.
    Deriving that name anywhere else would be a second copy of the emitter's
    statement order.

<a id="decision-42"></a>

42. **`db::git_sha` takes the project root.** It used to run git in the
    process's working directory, so `--project` elsewhere stamped plans and
    ledger entries with a commit from an unrelated repository.

<a id="decision-52"></a>

52. **The saved plan carries the declarations' data scope (`data`), and the
    plan version is 3.** `apply` records the database read back and needs a
    scope to read rows; deriving it from the plan's row changes would miss an
    `ensure` table whose declared rows were all already present, and
    deriving it from `SetDataMode` would need the differ to emit one for a
    table it is comparing against observed rows under the same mode. So the
    scope travels with the plan, exactly as `ids` does. The version is bumped
    because an older `apply` would run the DML and record a state with no rows
    in it, leaving every later `verify` blind to the rows just written — the
    same shape the state version was bumped for when modules arrived.

<a id="decision-75"></a>

75. **A connected plan is pinned under the recorded scopes plus the tables it
    covers for the first time.** The baseline checksum was the drift check's
    view — rows under the *recorded* scopes — so a table gaining its first
    `data:` block had its rows measured by the differ and pinned by nothing:
    a row inserted there between plan and apply passed `apply`'s check and,
    under `exact`, outlived the approved deletes. The drift check keeps its
    view (it compares against the recorded snapshot and must); the baseline
    and `apply` read the union (`pinned_scopes`: the recorded scope where
    there is one, the plan's where there is none) under the same reference,
    so the two checksums are computed over the same rows.

<a id="decision-77"></a>

77. **A connected plan reads the declarations under the names the database
    has now.** The recorded state, the scoped live schema and the rows are
    all keyed by the *old* name of a table this plan renames; the declared
    scopes were keyed by the new one, so `read_scopes` never joined the two
    and the declared scope found no table to read — an `ensure` -> `exact`
    switch in the same revision as a rename planned none of its deletes and
    applied cleanly. The declarations are re-keyed through the ids
    (`tables_under`: final name -> uid -> live name) before the read, the
    plan base and the pinning, and `apply` re-keys the plan's scopes the
    same way (`scopes_under`), so the two checksums see the same tables.

<a id="decision-86"></a>

86. **`bootstrap`'s empty-target guard counts roles, and `pull --data` runs
    the model's data rules.** Two guards that stopped one object short: a
    declared role already standing in the target passed the "is it empty"
    question and failed at `CREATE ROLE`; a pulled block that set a non-key
    IDENTITY column passed the dialect's check and was refused by the
    model's on the next `validate`. Both ask the whole question now.

<a id="decision-98"></a>

98. **The pinned baseline is the union of the recorded scope and the plan's,
    table by table.** 77 pinned the plan's scope for a table the recorded
    state did not cover, and kept the recorded scope wholesale where it did
    — so a key added to an `ensure` block, or an `ensure` -> `exact`
    switch, was read at plan time and never checked again before apply: a
    change to the new key in between was overwritten by an approved update
    nobody measured, and a row inserted in between made the approved insert
    fail. `DataScope::union` (exact if either is, every key either spells)
    is what both the checksum and `apply`'s check now read.

<a id="decision-100"></a>

100. **An object a statement creates enters the live identities the moment
    the statement commits.** A staged checkpoint scopes the environment by
    the identities the catalog had before the plan, and a table, a column
    or a role the plan had just created was outside every checkpoint until
    the closing entry — a grant the new role gained, or a row the new table
    gained, while the deployment was paused went unseen by `--resume` and
    was recorded as clean. The emitter says what each statement creates
    (`Statement::creates`), as it says what each renames (93), and the
    executor adopts it under the plan's uid before the checkpoint is taken.

<a id="decision-109"></a>

109. **`bootstrap` refuses a declared object the identity file does not
    know.** The guard asked whether the ids file named *any* table; a
    role-only project that had never run `pbps plan`, or a role added after
    the last plan, was skipped by the differ, built nothing, and recorded
    the empty state as the whole one. Every declared table and role needs
    its uid, and the ones without are named.

<a id="decision-145"></a>

145. **The artifact format versions reset to 1 at the first release.** The
    plan file reached 4 and the state snapshot 4 — with 3 accepted as an
    upgrade path — before this tool was ever released: the workspace is
    `0.0.0` and there is no tag. Every one of those bumps was correct by the
    rule that owns them (a reader that silently ignores a field it does not
    know is a reader that acts on half a plan), and the numbers still record a
    history nobody has: the "older pbps" whose ledger entries the state reader
    tolerates never existed. Before the release the cost of a bump is zero — a
    plan file lives for one deployment window, and no environment holds a
    ledger this project did not write in a test — so they stay cheap and
    honest until then, and at the first tagged release both constants go back
    to 1 and the pre-release upgrade path goes with them. Recorded in
    `docs/STATUS.md` under open items, because a decision that has to be acted
    on months later is worthless anywhere a reader does not look.

<a id="decision-155"></a>

155. **A baseline is read at the paths its own revision used.** `schema_dir`
    and `ids_file` are configuration, so a revision that moves the
    declarations records the move in its own `pbps.yml` — and both readers of
    a historical tree asked git for *today's* paths. The listing then comes
    back empty and the identity file missing, which is the shape this project
    keeps paying for: absent read as "nothing there". `plan` calls it an empty
    baseline and proposes creating the whole schema; `validate --since` calls
    every table and role changed, so a gradual-adoption policy fails
    declarations nobody has touched.
    `paths_at` reads `pbps.yml` at the revision and returns that revision's
    two paths. A revision with no `pbps.yml` falls back to today's, which is
    an answer and not a guess — the project did not exist then, so nothing it
    holds is at any path. One that *is* there and does not parse is an error;
    reading past it would silently be this bug again.
    Both readers, not one. `schema_at` held a second copy of the same path
    resolution, and fixing `load_from_git` alone left `--since` reading the
    wrong tree while `plan` read the right one.

<a id="decision-159"></a>

159. **A staged apply cannot roll back, so it stops instead — and `status`
    records rather than returns.** Two findings, one sentence apart in kind:
    a guard that was scoped away, and a check that ended the function.
    **`apply --staged`.** 147 and 150 both said staged runs were not covered
    "for the reason 147 gives" — no transaction, so nothing to roll back and a
    refusal at the end would only strand the environment. That reasoning was
    about *refusing*, and it answered a question nobody asked. Nothing in a
    staged run compared one read with the last, so an edit that landed between
    two statements went into that checkpoint, into every later read, and
    finally into the closing ordinary snapshot — which is the state `verify`
    measures against ever after. Measured through the CLI: a trigger writing
    into an undeclared-by-this-plan table during the one statement of a staged
    plan, and the run reports `Applied 1 change(s) ... recorded as entry #3`.
    Each checkpoint is now compared with the read before it, over everything
    the plan does not touch, and the run **records the checkpoint and then
    stops**. Recording first is the point: the statement has committed, and a
    refusal that skipped the checkpoint would lose the record of it, which is
    the one thing a resume needs to be true. The closing read is compared the
    same way before the ordinary entry is written, so the last window is
    covered too and the environment is left on its checkpoint —
    `refuse_mid_deployment` then makes every other command say so.
    The exempt set is what the *whole* plan touches, not the statement just
    run. It is the conservative direction, and this guard has been wrong four
    times in the other one (152 to 158): what it costs is a change to an
    object a later statement will touch, what it buys is that no correct
    staged apply is ever stopped by it.
    **`status`.** An unexpressible permission ended the function, so the row
    checksum, the limitations inventory and the unmanaged inventory below it
    never ran: one screen reported the permission and hid every other problem
    the environment had. Recorded and carried now, like the rest. The trunk
    had already made this correction twice on its own paths; this one came
    through the merge with the early return intact, which is its own lesson
    about resolving a conflict by keeping "our" side.

<a id="decision-180"></a>

180. **A historical tree is listed with `-z`, and the bug it hid was silence
    rather than an error.** `git ls-tree --name-only` applies `core.quotePath`,
    which is on by default: measured, `schema/dbo.té.yml` comes back as
    `"schema/dbo.t\303\251.yml"`, quotes included, and `git show` on that
    answers `fatal: path ... does not exist`.
    That error is not what happened. The quoted form does not end in `.yml`,
    so the extension filter skipped the file before anything tried to read it,
    and the revision read as **empty**: `plan` printed "Baseline: git HEAD (0
    objects)", warned that everything would be listed as newly created, and
    exited 0. A repository that is perfectly well formed produced a plan
    against nothing. This is the shape CLAUDE.md names — absent, empty and
    unreadable are three different things, only one is good news — and it is
    worth recording that the first test I wrote for it *passed*, because it
    asserted an exit code. The bug had no exit code.
    Both readers of a historical tree go through one `tree_paths` now, for the
    reason the two role-name filters did: written twice, they had the same bug
    twice. It splits on NUL and never trims, which is the same rule 177 and
    178 established for names — a path is what the tree spells it.

<a id="decision-238"></a>

238. **The state fingerprint sorts a table's columns; the plan's does not.**
    Two rules answered "has this environment moved" and gave different
    answers. `Schema`'s `==` ignores column order — `Table::columns` is an
    `IndexMap`, compared as a map — while `state_checksum` serialized that map
    in declaration order, so the order was in the hash. A DBA who drops a
    column and adds it back identically (fixing a collation, say) moves it to
    the end of `sys.columns`, and the two rules then disagreed about the same
    database: the differ reported no changes and put nothing in
    `unexpressible`, and the checksum said the state had moved. `verify`
    announced drift and listed nothing that drifted, `plan --db` refused, and
    the only way forward was a re-baseline — the escape the drift gate exists
    to make unnecessary.

    The fingerprint is the side that gives way, because it is the side whose
    sensitivity buys nothing. Live column order decides no statement pbps
    emits: `CREATE TABLE` lays out the columns of the *declarations*, and no
    change this tool plans reorders an existing table. So the order is
    recorded — it stays in `state_json`, which is a snapshot and a backup, and
    `state export` still hands back the layout the database had — and it is
    sorted away in the one place that asks whether anything changed. Making
    the differ agree with the checksum instead (an `unexpressible` entry
    naming the table) was the other shape on offer. It reports the same thing
    more honestly and still leaves the operator with no forward path: the
    checksum mismatch, not the change list, is what `plan --db` refuses on.

    The sort is in `state_checksum` alone. `plan_checksum` hashes the plan
    file whole, and a `CreateTable` carries the table it will emit, column
    order included: two plans that would run two different `CREATE TABLE`
    statements must remain two different artifacts, and that is pinned by a
    test of its own. Determinism is unaffected — the sorted order is a
    function of the key set, and two schemas that compare equal have the same
    key set, so equal schemas now hash equally by construction rather than by
    coincidence of insertion order.

    The saved plan's format version moves with the algorithm, to 6. A plan is
    the one artifact carrying a fingerprint written by one build and
    recomputed by another — `apply` compares `baseline.checksum` against what
    it computes from the live database — so without the bump a plan from the
    previous build would be refused as *drift*, against a database nobody had
    touched, and a plan from this one refused the same way by an older build.
    Both refusals name the wrong problem and send an operator to reconcile
    nothing. Refused as a format instead, before anything is connected to,
    with the remedy a stale artifact always had: run `plan --db` again and
    take the new plan through the gate. Nothing else stores a state
    fingerprint — the ledger keeps whole snapshots, and every comparison
    recomputes both sides with the same binary — so the plan file is the whole
    of the compatibility question. A test pins the fingerprint of a fixture
    beside the version number, so changing one without the other fails there.

<a id="decision-432"></a>

432. **Connected role and rebuild checks report their actual checked scope.**
    The engine facade returns a named capability result with completed cluster
    role rename evidence and module rebuild checks. Connected planning includes
    these results in the same human/JSON check list as permission support and
    table/column drop impact. PostgreSQL refusals name the check; SQL Server
    names its different mechanism as inapplicable: database-role DDL and
    CREATE OR ALTER do not need PostgreSQL's external-role or rebuild evidence.

    Incoming/renamed cluster-role presence is reconciled in the facade from
    the same queried schema used for the baseline checksum. It is not asked
    again against a potentially different catalog, and the reported count is
    only that checked scope. The prior behavior for an unchanged recorded role
    remains the separate baseline-absence question in #312. The role rename
    map and its successful report travel together; a failed check produces no
    success report. CLI regressions pin named JSON refusals and successful
    answers for all three existing checks, alongside their apply paths (#305).

<a id="decision-436"></a>

436. **A staged checkpoint compares the rename that its statement performed.**
    A PostgreSQL cross-schema rename emits a schema transfer and then a name
    change. The checkpoint between them correctly retained the intermediate
    identity, but its movement check still used the original-to-final logical
    rename. On resume, the final rename therefore reported the intermediate
    table as unexpectedly gone and refused an otherwise valid deployment.

    The checkpoint comparison now substitutes the emitted statement's exact
    `renames` endpoints for that one logical table rename. It still compares
    the table's untouched shape, rows, grants and incoming references; it does
    not exempt intermediate names from checking. The closing read keeps the
    full plan. Live CLI tests force a second-statement failure, read the first
    checkpoint back on another connection, reject changes made while paused,
    and resume to the declared destination. A companion test adds an unplanned
    column during the first transfer and requires refusal at that checkpoint,
    rather than letting the name change hide it (#299).

<a id="decision-486"></a>

486. **Constraint-drop approval distinguishes uniqueness from FK/CHECK
     relaxation (issue #241).** Removing a UNIQUE constraint carries the
     `destructive` risk because it removes a uniqueness guarantee, even if
     every existing row stays unchanged. Dropped indexes and removed primary
     keys already face that gate; declaring uniqueness as a constraint must
     not make its removal bypass approval (issue #110, PR #237).

     DROP FOREIGN KEY and DROP CHECK retain their existing empty intrinsic
     risk set. They relax validation of future writes without themselves
     rewriting stored values. The extra approval for lost uniqueness is an
     explicit product boundary, not a general classification of every lost
     integrity guarantee. FK/CHECK removal can admit later inconsistent data
     or encounter operational failures; ungated does not promise otherwise.
     The declaration diff and checksum-pinned plan still expose those drops
     for review, and any other changes retain their own risks (SPEC 7.2–7.3).

     Keep this distinction in the model's intrinsic classification for both
     dialects. Changing which constraint relaxations require approval needs
     an explicit policy change rather than grouping all constraint drops by
     their similar SQL spelling. The existing focused tests
     `dropping_a_unique_constraint_is_destructive` and
     `dropping_foreign_keys_and_checks_remains_ungated` pin both sides;
     `destructive_rationale_covers_data_and_uniqueness_loss` pins the gate's
     explanation. This entry records the existing behavior; it changes no
     risk class or execution rule.
