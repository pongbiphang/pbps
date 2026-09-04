# Decisions

Every entry here was paid for. Each one records a choice that is not the obvious
one, and *why the obvious one is wrong* — which is the part that is expensive to
reconstruct and the reason this file is longer than a changelog.

Read it before changing the data model, the permission checks, the ledger, or
anything a command's exit code depends on. `CLAUDE.md` carries the rules; this
carries the reasons.

Companion files: [PITFALLS.md](PITFALLS.md) for the bugs and traps found the
hard way, [STATUS.md](STATUS.md) for where the product currently stands,
[SPEC.md](SPEC.md) for the design, and `ADR-*.md` for the standalone records.

## Changed from the original spec

SPEC is in sync with all of these.

> **The numbers are load-bearing.** Code comments cite them ("decision 25",
> "ADR-0003 decision 3"). Append; never renumber.


1. **Comparison matches by uid via two-sided identity files**, not "name + this
   revision's intent" — the latter breaks on jump-version deploys.
2. **Drops require a reason**: the tombstone must answer an audit's "why".
3. **Intents are idempotent**: a stale `renamed_from` is a no-op, not an error.
4. **`StateSnapshot` carries `ids`**: state and identity travel together.
5. **IDENTITY changes are blocked** (cannot be done with ALTER).
6. **Review has two layers**: the MR reviews the desired-state change (offline
   plan is a preview only); the deployment gate reviews the per-environment
   `plan --db`, and the checksum pins that plan to the apply. plan.sql is never
   hand-edited.
7. **`plan` writes only the ids file, never the user's YAML**; `fmt` strips
   redundant `renamed_from`; `plan --check` is strictly read-only.
8. **`validate` rejects one name mapping to multiple uids** — same-name adds
   on two branches auto-merge silently in the ids file.
9. **Drift compares the managed set only**; expressions are never parsed —
   after apply the DB's stored form is read back into `state_json`, and the
   differ side uses the dialect's lightweight normalization.
10. **`apply` is one transaction per plan, all or nothing**: non-transactional
    statements fail at plan time unless the plan is staged (see 26);
    pre-flight (rename impact, SCHEMABINDING) runs before the first statement.
11. **`__pbps_state` protects against mistakes, not tampering**: only the
    deployment account writes `__pbps_state` / `__pbps_lock`; the audit
    baseline is git + CI logs.

## Phase 2 — pull, normalization, strategy


12. **`ALTER COLUMN` restates the whole definition**, and an omitted
    `NULL`/`NOT NULL` means `NULL` — so `AlterColumnType` carries nullability,
    `AlterColumnNullability` carries the type, and the differ folds a
    type+nullability change into one `AlterColumnType`.
13. **Normalization targets what the catalog stores**, not what the user wrote
    (`numeric`→`decimal`, `float(24)`→`real`, `varchar`→`varchar(1)`);
    introspection reads the stored form back, and any gap is a phantom diff.
14. **`pull` never drops what it cannot express** (computed columns, UDTs,
    clustered indexes, unmanageable modules): each becomes a warning or an
    inventory entry, and a table with no expressible columns is left out whole.
    The round-trip `load(render(pulled)) == pulled` is pinned by
    `pbps-cli/tests/pull_roundtrip.rs`, for modules as well as tables.
15. **Default/check expressions are compared after peeling the engine's stored
    parentheses** (`((0))` → `0`), only when they wrap the whole string.
16. **`strategy:` is persistent, unlike `renamed_from`** — it lives beside the
    model (`Loaded.hints`, never in `Schema`, or constraint 1 breaks) and `fmt`
    preserves it. Unknown keys are rejected: a typo that became a no-op would
    leave the user believing a large table is altered online when it is not
    (ADR-0003).
17. **`pull` inventories what it cannot manage.** Everything it finds and
    cannot express is listed with the reason (ADR-0002); that is separate from
    `warnings`, which are defects in the pull itself.
18. **`docs` output must stay deterministic and self-contained** — identical
    declarations produce byte-identical files, and the HTML references nothing
    external (the air-gap rule applies to artifacts). That is why the ERD
    travels as Mermaid source rather than a script-rendered diagram.

## Phase 3 — the ledger, drift, apply


19. **The ledger's T-SQL lives in `pbps-mssql::state`**, its types and prune
    policy in `pbps-db::ledger`. The columns beside `state_json` are projected
    from the snapshot at record time, never passed separately, so they cannot
    come to disagree with it. `applied_at` is the *server's* clock, formatted
    as ISO 8601 text by the query — tiberius is built without `chrono`.
20. **Drift needs `observed_ids`, not the recorded mapping on both sides.**
    `diff` matches by uid, so two sides sharing one ids file see only attribute
    changes; a hand-added or hand-dropped column would be invisible. The live
    side is identified by what is there, with `Uid::derived` (deterministic, so
    a hook payload is stable) for objects that have none. Never use `derived`
    to mint a real identity — two branches would collide.
21. **`pbps.yml` names the env var, never the connection string** (`url_env:`),
    and nothing prints one: `db::redact` reduces it to server/database and
    degrades to a placeholder rather than echoing what it could not parse.
22. **Probes are built per plan, not per change.** They run before the first
    statement, so every name in them must be the one the catalog still has —
    `preflight::AsStored` translates through the plan's renames, and tables the
    plan creates are skipped. Check expressions are deliberately *not*
    rewritten; that probe fails to run and is reported as unchecked.
23. **A saved plan carries `origin`, `mode` and the post-plan `ids`.**
    `Preview` is a value in the file, so `apply` refuses it structurally; the
    ids make apply self-contained on a host with no checkout, and recording the
    baseline's mapping instead would say a rename never happened.
24. **`apply` takes the lock before the pre-flight, and releases it on every
    path.** A check that passed while another pipeline was mid-apply was
    answered about a moving database; a lock left behind blocks the pipeline
    that would fix it.
25. **`verify` exits 2 on drift**, and drift includes what the differ cannot
    phrase: `DriftReport.unexpressible` carries those, so the findings, the
    envelope and the **`on_drift` hook** all see them. A parallel path for them
    exited 2 correctly and skipped the hook, which is the one thing a scheduled
    drift-watch exists for. `unmanaged: error` is the same shape: the database
    was read successfully and the project's own policy refused it, so it is a
    finding (exit 2), not `environment.unreachable` (exit 1). Distinct from 1 for a tool failure: a
    scheduled drift-watch wakes different people for each. `status` always
    exits 0 — it is a report, and one unreachable environment must not cost
    the operator the other five lines.
26. **ONLINE is edition-dependent, and only a connection knows the edition.**
    The emitter writes `WITH (ONLINE = ON)` where the statement takes one — a
    UNIQUE constraint is index-backed and does, a foreign key and a check are
    metadata only and the clause is a syntax error there — and `plan --db`
    reads `SERVERPROPERTY('Edition')` and refuses before writing a plan the
    server would reject. An offline plan says the hint is unverified.
27. **The dev database is always optional** (`plan --dev`). Without one the
    preview degrades to lightweight normalization and says so; with one, the
    rehearsal reports structural differences as a failure and spelling
    differences *with the engine's stored form*, which is the only place that
    form can come from. `--dev` and `--db` are refused together: a rehearsal is
    a preview's question.

## Phase 3.5 — modules and staged apply


28. **Modules are matched by name and have no uid** (constraint 7). The managed
    set for them is therefore supplied per command: `verify` passes the
    recorded state's modules, the commands that record a state pass the
    declared ones, and `apply` passes the recorded set plus what its own plan
    creates — which is what keeps it applyable with no checkout.
29. **The emitter adds no terminator to a module.** The engine stores what was
    sent, introspection reads it back, and a semicolon the declaration did not
    have would come back inside the body and read as a change on every plan.
30. **`introspect::split_module` may refuse.** It reads exactly as far as
    `emit::module_definition` writes; a view with `WITH SCHEMABINDING` has
    nowhere to keep its options, so the module is inventoried as unmanaged
    rather than recreated later without them.
31. **Module changes bracket the table changes.** Drops sort first (a
    SCHEMABINDING view blocks a rename), creates last (they select columns that
    must exist), and within each group the order comes from an identifier scan
    over the definition text — of the **code only**, so a name in a comment or
    a literal invents no edge — with `depends_on:` as the escape hatch.
    *Known limitation*: an alter that **releases** a schema-bound dependency
    would have to run first, and one rank cannot serve both directions. Left
    unfixed on purpose; the reasoning and the trade are in ADR-0002 under
    "Known limitation".
32. **A staged plan is one logical change, and its mode lives in the file.**
    `apply --staged` runs it outside a transaction with a `staged` ledger entry
    per completed statement; `--resume` re-checks the live state against the
    checkpoint before continuing. An unfinished checkpoint makes the
    environment mid-deployment: `plan --db` and a fresh `apply` refuse, and
    `status` reports `staged`.
33. **A checkpoint's `ids` are the names at that checkpoint**, not the plan's.
    One `RenameTable` can take two statements, and between them the table is at
    `[new schema].[old name]` — a name in neither the baseline nor the plan.
    Each `Statement` therefore declares its own renames, the staged loop
    replays them, and the checkpoint records what the catalog actually has.
    Deriving that name anywhere else would be a second copy of the emitter's
    statement order.

## Phase 3.1 — exit codes, the findings envelope, doctor and explain


34. **There are three exit codes, and the split is the feature.** 0 clean, 2 the
    command answered and found something to act on, 1 the command could not
    answer. `Found` (was `DriftFound`) carries the 2; an empty message means the
    detail is already printed. A pipeline that cannot tell 1 from 2 wakes the
    wrong person half the time.
35. **One findings envelope, not one shape per command** (`cli::output`). A
    command's own payload rides in `data` — `verify`'s drift report, `status`'s
    rows, `explain`'s explanation — and never replaces the envelope. `id` is
    stable and is what a future `policies:` block will re-weight, so it must
    survive a reworded message. **The hook payload is deliberately not wrapped**:
    a script's input must not change shape because someone added a flag for
    their own eyes.
36. **`status` findings are warnings on purpose.** It always exits 0 (decision
    25), so an error-severity finding would make `result` disagree with the exit
    code. The per-environment truth is in `state`.
37. **The vendor annotation formats stay outside the binary**
    (`scripts/findings-to-github.py`). Each one compiled in has to be kept
    working forever, including for users who run neither.
38. **`doctor` reimplements nothing and writes nothing.** It calls
    `validate_findings`, the same function `validate` runs — a readiness command
    that disagreed with `validate` would be worse than one that never looked.
    Permissions are *asked for* (`sys.fn_my_permissions`, `HAS_PERMS_BY_NAME`),
    never tried, and named one by one with what each is for **and at the
    securable where it is needed**: the four `CREATE`s at the database (they
    cannot be granted lower), `ALTER` / `VIEW DEFINITION` and the probes'
    `SELECT` per **managed** schema, and `INSERT` / `DELETE` plus the ledger's
    own `SELECT` on the ledger and lock *objects* (falling back to their schema
    only until those tables exist), plus `ALTER` on the ledger's schema **while
    the ledger does not yet exist** — `CREATE TABLE` at the database does not by
    itself let an account create a table in a schema. `SELECT` is listed twice on purpose: the
    probes read managed tables and the ledger read is two tables in `dbo`, and
    one entry made the wrong demand in both directions. The ledger's schema is
    **not** forced into the managed set — a project managing only `app` never
    touches a `dbo` table. Asking at database scope alone reports gaps a
    least-privilege account does not have, and the remedy it then invites is
    exactly the "make it db_owner" this list exists to avoid. A securable that
    does not exist yet is left unasked — that is every first deployment, and the
    create-time `ALTER` is required per ledger table still missing, not once for
    the pair. `REFERENCES` is on the list because a foreign key is authorized on
    the *referenced* table and `ALTER` does not imply it — and when that table is
    **outside the managed schemas**, `REFERENCES` and the probe's `SELECT` are
    asked on the object itself (`Needed::Referenced`), because nothing asked
    about the managed schemas can see it. That query deliberately omits the
    `OBJECT_ID` existence filter the ledger's uses: metadata visibility cannot
    tell absent from invisible, and here dropping the object would under-report
    instead of falling back. **`CONTROL` is
    deliberately absent**: a cross-schema rename needs it (`ALTER SCHEMA ...
    TRANSFER`), but `doctor` sees no plan, so demanding it would require
    near-ownership of every managed schema always — the claim is narrowed to
    "most changes" instead, and the real check belongs in the plan-aware
    pre-flight. A declared schema the database **lacks** is a readiness *error*
    (pbps never emits `CREATE SCHEMA`), which is a different question from
    leaving it unasked for permissions. `CONTROL` on the database is also **not
    a shortcut past the list**: the inputs are `HAS_PERMS_BY_NAME` answers,
    which already account for inheritance, so an owner comes back clean without
    one — while a `DENY` at a narrower securable beats an inherited `CONTROL`,
    still leaves `CONTROL` listed in `sys.fn_my_permissions`, and really does
    make the DDL fail. Returning early on that one signal called such an
    account ready.
39. **`explain` always exits 0 and needs no connection.** It is the reviewer's
    command, and the reviewer may have no checkout and no credentials; the gate
    is `apply --allow`. A target is optional and answers only the question no
    file can — whether that environment is mid-deployment.
40. **The prompt is a wrapper, never a shortcut.** Answers become ordinary
    `Intent`s through the same `resolve`, so the artifact is identical.
    Similarity orders the candidates and never decides; nothing is charitably
    interpreted; a partial answer records nothing. `--no-input` declines a
    prompt and can never answer one — that is why it is safe in an alias, and
    why SPEC 14.3 still refuses `--assume-renames`.
41. **The editor schemas are generated from the loader's own types**, so
    `deny_unknown_fields` reaches an editor as `additionalProperties: false`.
    A schema that accepted more than the loader would be worse than none. The
    copies in `schemas/` are pinned to the binary by a test; regenerate with
    `pbps schema --kind <k> --out schemas/<file>`. Where a *semantic* rule
    outlives the derive's shape, it is stated as a `oneOf`: a module declares
    exactly one kind (and `on:` only on a trigger), and `dev:` names exactly one
    backend. Both were blessing documents the tool then refuses. **Every branch
    must exclude every key the others require**, and with `{"type": "null"}`
    rather than `false`: `oneOf` means *exactly one* branch matches, so a branch
    that constrains only its own key lets a two-kind document through the moment
    another branch is disqualified for an unrelated reason — and `false` would
    refuse `procedure:` written empty, which serde reads as absent.
42. **`db::git_sha` takes the project root.** It used to run git in the
    process's working directory, so `--project` elsewhere stamped plans and
    ledger entries with a commit from an unrelated repository.
43. **`--check` is read-only in every direction.** It refuses `--dev`, `--out`
    and `--sql` rather than ignoring them: a CI check that skipped the write
    would leave the previous run's plan.sql on disk for the job to review. The
    refusals sit with the other flag validations, before the command runs, so
    they reach the JSON envelope — the `--dev` half used to be a `bail!` after
    the writes, and the `--out` half was reached whenever the ids file happened
    to be current.
44. **A drift report keeps both halves.** `diff` returns `Err(errs)` and throws
    away the changes it *had* expressed; `diff_partial` returns both, and
    `verify` uses it. `plan` keeps the `Result`: a plan that cannot express
    every difference must not be applied at all, and that is the one place the
    two callers differ.
45. **`status` reads the lock even when the ledger is empty.** `state::lock`
    calls `ensure_tables`, so a lock held over an empty ledger is what a *first*
    `bootstrap` looks like while it runs — and what an interrupted one leaves
    behind. Not in the `NotInitialized` branch, though: `dbo.__pbps_state` being
    absent means nothing ever took a lock **by any path the tool controls** —
    but a hand-dropped state table leaves the lock behind, so both branches read
    it. What makes that safe on a first run is that `lock_holder` and `unlock`
    now attempt the statement and read the server's error number: **208**
    (invalid object name) means absent and answers "no lock", **229**
    (permission denied) and everything else stay errors. `unlock` had the same
    confusion and could not release a lock that outlived its state table.
    **Never ask `OBJECT_ID` instead**: metadata visibility hides an object from a
    principal with no permission on it, so it answers NULL for a lock table that
    exists and is held — turning "not authorized to look" into "no lock".
    `HAS_PERMS_BY_NAME` does not separate them either (measured: 0 for both).
    `DbError::server_error_number` exists so `pbps-mssql` can read the code
    without a second crate naming `tiberius`.
46. **The lock is asked before initialization, in all four commands.** `status`,
    `doctor` and `explain` each asked `is_initialized` first, because
    `lock_holder` used to select from a table a never-initialized database does
    not have. Item 45 removed that reason; the ordering then only hid the
    half-present ledger. `doctor` called it "uninitialized" and exited **0**
    with an apply blocked, and `explain` printed the approval command for a
    target that was changing under it. A guard whose reason has gone is not
    harmless — it is a filter nobody re-reads.
47. **A flag is honoured or refused, never accepted and dropped.**
    `plan --db --format json` took the flag and ignored it — prose on success,
    empty stdout on failure, while the flag validations in the same invocation
    answered JSON properly. It is refused now, not implemented: `plan --db`
    produces an *artifact*, and its typed form already exists and is better than
    an envelope — `--out plan.json`, read back with `explain --plan --format
    json`. A second typed rendering would give a reviewer two documents to
    disagree about.
48. **A path is spelled with `to_str`, never `display()`, before it is put in a
    command.** On Unix a filename is bytes; `display()` substitutes U+FFFD,
    which `shell_arg` neither refuses nor leaves bare — so a lossy path came
    back neatly quoted, naming a file that does not exist. A path that cannot be
    spelled is in the same position as one that cannot be quoted: the
    placeholder, with the literal printed where nothing can execute it. The same
    path must also stay out of `output::Location`, whose `file` is a **`String`**
    for this reason: serde's `Path` impl *fails* on non-UTF-8, so a `PathBuf`
    there made that failure the whole envelope's — `explain --format json`
    printed nothing at all. `Finding::at` drops the location rather than store a
    lossy one that points at a different file.
49. **A guard built twice is a guard that fires early.** `dev::Container::start`
    built its cleanup guard, then *shadowed* it with a second one holding the
    same container id. A shadowed binding is not dropped early — it lives to the
    end of the function — so the first guard's `Drop` ran `docker rm -f` on the
    container just returned, and `plan --dev docker://...` failed with
    "connection refused" from the day the feature was written. It survived
    because **every `--dev` test passes a connection string**: the docker path
    had no coverage at all. `scripts/live-tests.sh` now sets
    `PBPS_TEST_DEV_IMAGE` to cover it; CI's live job deliberately does not, as a
    second SQL Server on that runner is a CI decision of its own.
50. **`shell_arg` has now been wrong about shells five times.** Single quotes in
    `cmd`; backslashes; `!` under delayed expansion; a leading `-`, which no
    quoting can carry because the shell strips the quotes and clap then reads a
    flag (the `--opt=value` form would work, and was declined — it changes every
    advertised command to buy a hyphen-leading name); and a leading `@`, which
    PowerShell splats in argument position. Every one was found by someone
    testing rather than by reasoning, which is the argument for the placeholder
    being the default answer rather than the last resort.

## Phase 4 — reference data against a target (ADR-0004)

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
54. **A table whose live key is not a single column is unreadable, and the
    whole read fails.** Skipping it would record `data: None` — "declares no
    rows" — and the next drift check would be blind to the rows it exists to
    watch. "Absent", "empty" and "unreadable" are three answers, and only the
    read failing keeps them apart. `status` reports the failure as
    `unreachable` with the reason rather than as "ok".
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

## Phase 4 — roles and grants (ADR-0005)

56. **A role change has no table; `Change::table()` became an `Option` and
    `subject()` is the label.** Every earlier change acted on an object in the
    tables-and-modules namespace, and `table()` returning a name unconditionally
    encoded that. A role is a principal. Returning a synthetic
    `TableName::new("role", name)` would have compiled and grouped plans
    correctly, and would have put a fake object name into strategy lookups and
    the edition check. The `Option` makes every caller say which it wanted.
57. **`grant-widen` is a risk class that is never gated.** `RiskClass::is_gated`
    exists so a class can be labelled for both review layers without being a
    flag: `unapproved_risks`, the `--allow` advice under a plan, `apply`'s
    refusal and `explain`'s approval command all use `gated_risks()`. A flag
    typed on every deployment that adds a permission is a flag typed by rote,
    which protects nothing and teaches the wrong habit.
58. **Grants are compared under the target's post-plan name, and never revoked
    on an object the plan drops.** The base side's grants are brought forward
    through the plan's table renames by uid before the per-target comparison,
    because `sp_rename` carries the permissions with the object; without that a
    renamed table produced a `REVOKE` on a name that no longer exists at the
    point the revokes run. A `REVOKE` on a table or module this plan drops is
    skipped for the mirror reason: the drop removes the permission, and the
    statement would fail after it.
59. **Inside a managed role, only grants on managed objects are compared.** The
    declarations may not name an undeclared object (the `validate` rule), so a
    grant on one could never be declared — and comparing it would have every
    plan revoke it. `scope()` drops such grants from the live side; schema
    grants stay, and a role outside the ids file is unmanaged like a table. The
    cost is that a hand-made grant on somebody else's table is invisible to
    `verify`, which is the same line the managed set already draws for the
    table itself.
60. **The catalog reports what the model cannot hold; it never folds it.** A
    `DENY` is not "no grant", a column-level `SELECT` is not a table-level one,
    and `WITH GRANT OPTION` is not a plain grant. Each becomes a `pull` warning
    naming the role and the target, and a plain grant is recorded only for the
    last of them (with the warning), because the declaration can express that
    much and the difference is one the next drift check will not be able to see
    — which is said, rather than hidden.

## Phase 4 — follow-ups

61. **An unnamed declared primary key matches any stored name.** A declaration
    that writes `primary_key: [id]` leaves the name to the engine, and the
    engine invents one that the recorded state then carries. Comparing names
    there restated `SetPrimaryKey` — a `constraint` risk — on every connected
    plan until somebody copied `PK__t__357D4CF8...` into the file, which is a
    demand nobody outside `pull` would meet. So an unnamed declaration compares
    columns only; a *named* declaration is compared in full, because renaming
    a constraint is a change the plan has to carry. The cost is that the
    differ cannot express "give this key a name" for a declaration that has
    none — which is the declaration saying it does not care.
62. **`doctor` asks for the role permissions only of a project that declares a
    role.** *Widened by 68: "declares" became "has", and the securables
    include what the roles hold live.* `CREATE ROLE`, `ALTER ANY ROLE` and `CONTROL` on every granted
    securable are security-shaped, and the list refuses over-demand everywhere
    else — but an account short of them fails on the first `GRANT` of a
    project that does declare roles, after `doctor` said ready. So `REQUIRED`
    gained its first entries that depend on the declarations: `Needed::RoleAdmin`
    is switched off by `Held::roles_declared`, and `Needed::Granted` is asked
    per object and per schema the roles are granted on, the way foreign-key
    targets outside the managed schemas already are. `CONTROL` is what is
    asked for, because `HAS_PERMS_BY_NAME` cannot ask "held with grant option".
63. **A dropped role's members are written into the connected plan, never
    found at apply time.** The engine refuses `DROP ROLE` while members
    remain, and the two obvious answers were both wrong: leaving it to the
    engine made every role drop fail in production, and removing members with
    dynamic SQL at apply time would run statements the reviewed plan never
    showed. `plan --db` reads `sys.database_role_members` for the roles it
    drops and puts each member into `DropRole::members`, so the plan lists
    who is removed and the emitter writes one `ALTER ROLE ... DROP MEMBER`
    per name before the drop. Membership stays undeclared and uncompared;
    this is the consequence of a drop the user asked for with a reason.

## Phase 4 — policies (ADR-0008)

64. **A rule's finding carries the rule's id, and the envelope's id became a
    `String`.** Findings from the catalogue are what a project re-weights and
    suppresses by, so the id they reach CI under is the rule's own
    (`naming.column`), not a `policy.*` wrapper around it. The envelope's
    `Finding::id` was a `&'static str`; every id was a literal until now, but
    a rule id read out of a plan file is not, and a leak or a lookup table to
    keep it static would have been a workaround for a type that no longer
    described the data.
65. **A block with problems is refused whole at the plan point.** `validate`
    lists what is wrong with `policies:`; `plan` evaluates none of it until
    that is fixed. Running the rules that parsed would report against a
    configuration the project did not manage to write, and a plan that
    passed under it would pass for the wrong reason.
66. **A rule setting accepts the YAML boolean `false` as `off`.** The word
    `off` is a boolean to the loader (so are `on`, `yes` and `no`), and
    `naming.table: off` is what an operator will write. Refusing it as a type
    error, or demanding quotes, would make the most common setting the one
    that fails; `true` is refused instead, because "on" names no severity.
    ADR-0008 "Implementation status" item 6.
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
69. **`doctor` asks about what the managed roles hold live, not only what the
    declarations grant.** A revision that removes a role's last grant, or the
    role, plans a `REVOKE` or a `DROP ROLE` whose securable the declarations
    no longer name — so `grant_targets` was `None` and `doctor` said ready to
    an apply that then failed on the `REVOKE`. The managed roles (declared, in
    the ids file, or held by the environment's recorded state — never the
    tombstones, which are permanent and would keep the requirements on for
    a drop applied years ago) are asked about, and the connected check
    reads their grants from the **recorded state** first
    — that is what the next plan revokes against, and the ledger is readable
    by any account that can deploy — and from `sys.database_permissions`
    second, for grants adopted by hand. The catalog alone was tried first and
    measured useless: metadata visibility hides a securable from an account
    with no permission on it, which is exactly the account `doctor` is
    checking, so the live query saw nothing precisely where the gap was.
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
72. **A securable this plan drops and creates again is granted from
    nothing.** `DROP` takes an object's permissions with it, so a table
    replaced under the same name, or a module changing kind, comes back bare;
    the grant differ compared the base's grants to the declared ones as
    text, found them equal, and wrote no `GRANT` — a successful apply that
    silently removed a role's access. On a dropped target the base side is
    now empty, so every declared permission is a `GRANT`, ordered after the
    `CREATE`, and the `REVOKE` stays unwritten as before (58).
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
74. **Two spellings of one row on one side are refused, never reconciled.**
    With keys read back through the engine (71), `1` and `01` declared side
    by side both alias the row `1`; keeping either plans an insert of the
    other, which the primary key refuses at apply. `validate` cannot see it
    (which spellings are equal is the engine's call, and `a`/`A` depends on
    the collation), so the projection refuses it (`RowConflict`) and every
    connected command reports it by both names. One side spelling `01` and
    the other `1` is not a conflict: each side is asked about its own keys.
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
76. **Role files live in `roles/`, not under a `.role.yml` suffix.** A table
    `app_reader.role` is a legal name and its file is `app_reader.role.yml`
    — the role `app_reader`'s file exactly — so `pull` wrote the role over
    the table without a word. No table file is ever written under `roles/`,
    so the two can no longer name one path; the loader finds every `.yml`
    under the schema directory and tells a role by its content, so a
    hand-written role file anywhere still loads.
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
78. **A role name may contain a dot.** `[app.reader]` is a legal principal
    name; the loader refused it as "not in a schema", and `pull` wrote it
    back as it is, so the freshly pulled project failed to load. The emitter
    quotes the name; nothing parses a dot in it.
79. **A suppression's `until` is checked against the calendar.** Compared as
    text, `2026-02-31` kept a suppression alive to the end of February and
    expired it on March 1, for a date that never comes; `2025-02-29` the
    same. The day is checked against its month, leap years included.
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
81. **`validate --since` compares the declarations at the revision, not only
    the identities.** Identity alone saw renames and new objects; a table
    that gained an index, changed a type or grew its `data:` block kept its
    uid and its name and was skipped — the one object the revision touched.
    The declarations at the revision are checked out of git into a scratch
    directory and loaded with the ordinary loader, and a table or role whose
    declaration differs (matched by uid, so a rename does not hide a change
    behind it) is evaluated too. Modules stay always-evaluated (ADR-0008
    implementation item 4).
82. **A rule switched on without what it runs on is refused, in either
    spelling.** `naming.table: error` checked no name — an error-level
    policy that accepted everything — and `change.window: error` had no
    window and refused every plan; the bare-word form skipped the parameter
    checks entirely. The catalogue names each rule's `required` parameters,
    and the block's check refuses an enabled setting that lacks one, whether
    it was switched on by a word, by its own severity, or by the catalogue's
    default.
83. **A connected plan refuses to drop a role that owns a securable.** The
    engine refuses the `DROP ROLE`, and a staged apply would already have
    committed every `DROP MEMBER` before finding out — users without access,
    the role still there. Ownership is read with the memberships and the
    plan is refused by name with the `ALTER AUTHORIZATION` to run by hand:
    moving ownership is a decision about who owns a schema, not a
    consequence of a drop pbps gets to make.
84. **The state snapshot is version 4, because the schema gained roles.**
    Adding `roles` to `Schema` under version 3 let an older binary read a
    newer snapshot with serde dropping the field: its `verify` compared
    every table and no role, and said "no drift" about grants it never
    looked at. A reader refuses a version it does not know, so the bump is
    what turns a partial reading into a refusal. The ids file stays at
    version 1 (its `roles` section is a compatible evolution by design), and
    an older binary meets the role *files* first, which it refuses to load.
85. **The pre-delete probe lets the engine say whether an update moves a
    row off the deleted key.** 73 excluded an updated row for the column its
    update sets; an update that set the referencing column to the same key
    under another spelling (`01` for `1`, `OLD` for `old` under a
    case-insensitive collation) was excluded too, and cascaded. The
    exclusion now asks the parent table, at run time, whether the value the
    row is set to *is* the deleted key — the comparison the engine makes —
    and a row set to `DEFAULT` or NULL, which no probe can compare, is
    counted. Measured on a live server with `OLD` against `old`.
86. **`bootstrap`'s empty-target guard counts roles, and `pull --data` runs
    the model's data rules.** Two guards that stopped one object short: a
    declared role already standing in the target passed the "is it empty"
    question and failed at `CREATE ROLE`; a pulled block that set a non-key
    IDENTITY column passed the dialect's check and was refused by the
    model's on the next `validate`. Both ask the whole question now.
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
88. **The owned-securable check names every class the catalog carries an
    owner for.** 83 listed the classes that came to mind — schema, object,
    type, assembly, certificate, keys — and missed a role that owns another
    role (`sys.database_principals.owning_principal_id`), exactly the drop a
    staged apply would commit every `DROP MEMBER` for before failing. The
    list is now the catalog views with a `principal_id` or
    `owning_principal_id` column, read off a live server rather than
    recalled: roles, XML schema collections, full-text catalogs and
    stoplists, search property lists, the Service Broker objects,
    database-scoped credentials, event notifications, external languages
    and libraries. The views that arrived after 2008 are probed for first,
    so an older engine answers with the classes it has. A class enumerated
    from memory is a filter nobody re-reads.
89. **A grant's permissions are checked against what the target is, with the
    engine's table.** `GRANT EXECUTE` on a table and `GRANT SELECT` on a
    procedure both pass the name checks and are both refused by the engine
    (Msg 4606) — in a staged apply after every earlier change has committed.
    `validate` now applies the engine's rule, measured on a live SQL Server
    2025 for every pair a declaration can spell: a table, a view and an
    inline table-valued function take everything but `execute`; a procedure
    and a scalar function take `execute`, `references`, `alter` and
    `view-definition`; a multi-statement table-valued function takes
    `select`, `references`, `alter` and `view-definition`; a trigger takes
    nothing (Msg 15151: `GRANT` cannot even name it); a schema takes all. The
    three kinds of function are told apart by the `RETURNS` clause, read off
    the code so a comment or a literal cannot pass for it. An object the
    declarations do not have is the model's finding and gets no kind here.
90. **A spatial cell cannot hold a declared value.** `geometry` and
    `geography` read back as WKT through `ToString()`, which carries no SRID;
    written back as text the engine assigns the default one, and `verify`
    keeps reading the same WKT and calling it clean. 70's shape again:
    refused by name as a key and as a set cell, with a typed value in the
    model as the way to lift it.
91. **A role rename faces the `rename` gate.** It keeps its membership, which
    is why it is a rename and not drop + add — but the old name is gone the
    same way a table's is, and a module or an application that asks
    `IS_ROLEMEMBER('old')` breaks the moment the statement commits. Nothing
    about keeping the members makes that safe, so `RenameRole` carries
    `RiskClass::Rename` and a plan with one needs `--allow rename`, like
    every other rename.
92. **`apply` reads a dropped role's members and ownership again before
    statement one.** The plan lists the members at `plan --db` time so a
    reviewer sees who loses the role (ADR-0005 item 6), and the baseline
    checksum cannot notice a member added since — membership is outside the
    managed state on purpose. A staged apply would then commit every `DROP
    MEMBER` the reviewer saw and fail on the one nobody did: users without
    access, the role still there. So the preflight compares the live
    membership with the listed one and refuses on any difference, added or
    gone, asking for a new plan; ownership (83) is asked about again the
    same way. The transactional apply would have rolled back, but a refusal
    that names the member beats a failed statement that does not.
93. **A statement that renames a role says so, as one that renames a table
    does.** `Statement::renames` exists because a staged checkpoint has to
    find an object under the name the catalog has *now* (ADR-0002 staged
    mode); `ALTER ROLE ... WITH NAME` moved a name the same way and said
    nothing, so a checkpoint after it scoped the role out under its old
    name and a resume could not see what changed on it while paused. The
    second instance of a shape the first one had already named.
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
95. **A grant `WITH GRANT OPTION` is unexpressible drift, never the plain
    grant.** The catalog spells it `W`, and folding it into the role's set
    beside a `G` let `verify` compare a role that can now delegate equal to
    the recorded plain grant and say "no drift"; a warning on stderr does not
    make that verdict safe. It is left out of the set and carried beside the
    comparison (`Scoped::unexpressible`), where `verify` reports it as drift
    with the `REVOKE GRANT OPTION FOR` to run by hand and `plan --db` refuses
    to plan over it — restating the plain `GRANT` would leave the option in
    place and the plan never converging. Representing the bit in the model
    is a format change with an emitter half (`WITH GRANT OPTION` on the way
    out) and belongs to an ADR, not a review fix.
96. **A clock field in a window is two digits, checked before it is read.**
    Rust's integer parse takes a sign, so `+01:-30` was thirty minutes
    east and `+9:00` a valid hour, and the window was measured at a time
    nobody wrote. The offset's and the time's fields are two ASCII digits
    each, and anything else is refused by name — the shape `parse_date`
    already had (78).
97. **Every permission the model cannot hold on a managed role is
    unexpressible drift, not a warning.** 95 made the grant option so and
    left a DENY, a column-level grant, a permission outside the closed set
    and a grant on an unmodelled object as warnings on stderr — and a
    managed role that gained a column-level `SELECT` on a sensitive column
    out of band compared equal on the sets that remained. All of them now
    travel the same way: left out of the set, carried beside the comparison,
    reported by `verify` as drift and refused by `plan --db`. `pull` still
    prints them as warnings, since nothing is being compared yet. The
    second instance of 95's shape, swept the same day.
98. **The pinned baseline is the union of the recorded scope and the plan's,
    table by table.** 77 pinned the plan's scope for a table the recorded
    state did not cover, and kept the recorded scope wholesale where it did
    — so a key added to an `ensure` block, or an `ensure` -> `exact`
    switch, was read at plan time and never checked again before apply: a
    change to the new key in between was overwritten by an approved update
    nobody measured, and a row inserted in between made the approved insert
    fail. `DataScope::union` (exact if either is, every key either spells)
    is what both the checksum and `apply`'s check now read.
99. **A row key has to be spellable in its key column's type.** The key
    travels as a string literal like any cell (70), and the alias query
    that lets the engine judge a spelling (71) has no table to ask on a
    table this plan creates — so `not-an-int` for an `int` key passed
    `validate` and the accepted plan failed at its first insert. `validate`
    checks each key against the kind the column reads back as: an integer
    key is digits with an optional sign, a `bit` key one of its four
    spellings, text takes anything (a decimal reads back as text and stays
    the engine's to judge).
100. **An object a statement creates enters the live identities the moment
    the statement commits.** A staged checkpoint scopes the environment by
    the identities the catalog had before the plan, and a table, a column
    or a role the plan had just created was outside every checkpoint until
    the closing entry — a grant the new role gained, or a row the new table
    gained, while the deployment was paused went unseen by `--resume` and
    was recorded as clean. The emitter says what each statement creates
    (`Statement::creates`), as it says what each renames (93), and the
    executor adopts it under the plan's uid before the checkpoint is taken.
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
102. **A staged resume re-checks a role drop for the members whose
    statements have not run.** 92 read the membership again before
    statement one; a checkpoint taken between two `DROP MEMBER`s left the
    same window open until `--resume`, whose drift check cannot see a
    member (membership is outside the checksum on purpose). The expectation
    is counted off the emitter's own statements — every listed member, minus
    the ones whose `DROP MEMBER` committed, and none once the `DROP ROLE`
    has — so the resume asks about the role as the plan left it.
103. **`validate` refuses a key whose text no spelling of its type has.**
    A number with letters, a GUID of the wrong length, a date with no
    digit: conservative on purpose, since the engine accepts more spellings
    than any rule here would list (`20260903` is a date), and the engine is
    asked before anything is written (101). What this catches, it catches
    offline; what it lets through, the connected commands do not.
104. **An integer outside what its column holds is refused offline.** `256`
    in a `tinyint`, `-1` in one, `32768` in a `smallint`: the right kind
    (87), and the engine refuses the insert at apply; the spelling probe
    (101) asks the engine only about text, since an integer is spelled by
    the model. The bounds are the type's own and never change, so `validate`
    names them — a key as well as a cell — and a `bigint` holds every value
    the model can carry.
105. **The permission read takes every class, and the ones the model does
    not hold are unexpressible drift.** The catalog query kept classes 1 and
    3 (an object, a schema), so a `GRANT CONTROL TO role` or `GRANT CREATE
    TABLE TO role` (class 0, the database itself) never reached 97's
    handling: it was filtered out before anything could report it, and a
    managed role that had gained the whole database compared equal on the
    grants it still held. The query filters nothing by class now; the
    assembler names the database-level ones and, for every other class, the
    catalog's own class name. The third instance of 95's shape — a
    permission the model cannot hold, dropped before the comparison — found
    one filter further up each time.
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
107. **A suppression's `until` is compared as it was validated.**
    `parse_date` trims, the lexical comparison did not, and `" 2026-12-31"`
    sorted before today: a future suppression expired at once, and a
    trailing space kept one alive on its own day. One text for both.
108. **A plan that changes the type of a key column is refused while a
    declared key is spelled differently from the stored one.** The alias
    mapping (71) holds under the type the column has now — `01` is the
    stored `1` under `int` — and a plan that makes the column `varchar`
    would carry that mapping into a type that does not make it: the base
    row is rekeyed to `01`, no row change is emitted, and the next `ensure`
    plan inserts a second row. Refused by name, with the remedy in order:
    write the keys as the engine spells them, apply, then change the type.
109. **`bootstrap` refuses a declared object the identity file does not
    know.** The guard asked whether the ids file named *any* table; a
    role-only project that had never run `pbps plan`, or a role added after
    the last plan, was skipped by the differ, built nothing, and recorded
    the empty state as the whole one. Every declared table and role needs
    its uid, and the ones without are named.
110. **A permission the declarations cannot express stops every command that
    records a state, not only `plan --db`.** 95 carried a managed role's
    `WITH GRANT OPTION`, and 97 and 105 the column-level, `DENY` and
    database-level grants, beside the comparison rather than inside it, and
    `plan --db` refused to plan over them. `snapshot` and `baseline` still
    wrote the schema down without them, so a privilege change could be
    recorded as a clean state that the very next `verify` reported as drift.
    All three now stop at one guard. `snapshot --force` is not an escape: it
    answers "record a state that differs from the recorded one", and this is
    "record a state pbps cannot express at all", which has no right answer to
    force.
    `apply` stops at the same guard, before statement one rather than at its
    closing snapshot: the baseline checksum cannot see such a permission
    either, so one that appeared between plan and apply arrives as a clean
    baseline and would be written down by the entry that closes the
    deployment — and a refusal after the statements committed would be the
    worse failure of the two.
111. **`pull` draws its row-count line from the `data.max-rows` rule, not
    from `max_data_rows`.** That field is only the rule's default parameter
    (ADR-0008). Read directly, a project that raised the threshold or turned
    the rule off was warned by `pull` about files `validate` accepts, and one
    that lowered it was handed files the next `validate` rejects — the two
    commands disagreeing about the same declaration, which is the failure
    ADR-0008 exists to remove.
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
113. **A revision `--since` or `--base` cannot resolve is refused, not read
    as an empty history.** One case is not an error: `HEAD` in a repository
    with no commits, which genuinely has no previous version. Every other
    unresolvable revision is a name somebody got wrong, and the empty
    baseline is the loudest possible wrong answer to it — `plan` proposes
    creating the entire schema, and `--since` marks every object changed, so
    a gradual-adoption policy fails declarations nobody has touched. Both
    `load_from_git` and `schema_at` swallowed every `rev-parse` failure
    alike; `schema_at`'s own comment already described the distinction the
    code did not make.
114. **`pull` refuses to write when `data.max-rows` is `error`.** 111 made
    `pull` read the rule instead of the field but kept only the row count,
    so a project that had set the rule to `error` still got a warning and
    the files — which the very next `validate` rejects. The severity travels
    with the count now, and at `error` nothing is written at all: refusing
    before the write is the difference between "no files" and "files you
    must now delete by hand".
115. **`money` and `smallmoney` are read with conversion style 2.** The type
    holds four decimal places and the default style renders two: measured on
    a live server, `1.0001` came back `1.00` and `2.5678` came back `2.57`.
    A `pull` then wrote a declaration for a value the table does not hold,
    and `verify` compared the two truncations and reported clean — a cell
    that could never be seen to differ. Style 2 renders all four and
    round-trips. It also renders `1234567.891` as `1234567.8910`, which is
    the engine's spelling and therefore the one a declaration has to use;
    the spelling probe (101) already says so before anything is written.
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

    A composite foreign key still contributes one count per column, so a
    child matching one column of a two-column key is counted. That
    over-counts, which refuses a delete that might have been fine; the
    direction that under-counts is the one that loses rows.
