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
    indexes whose physical kind the model has no place for — clustered,
    columnstore, XML, spatial, hash — unmanageable modules): each becomes a warning or an
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
    `DbError::server_error_code` (`server_error_number` until 193) exists so
    `pbps-mssql` can read the code without a second crate naming `tiberius`.
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

    **The sweep asked one question and there were two; see 151.** "Can a probe
    *miss* a violation" is answered above. "Can a probe *report* one the plan
    is about to remove" was never asked, and `AddForeignKey` answers it
    badly: it sorts after every row change, so a plan that supplies the
    missing parent rows or repairs the orphaned children is refused for the
    very violation it was written to remove.
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

    A composite foreign key contributed one count per column until 121.
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
118. **A declared role's name is checked against every database principal
    before a connected plan or a bootstrap is written.** The managed set
    knows the roles, and a role named like an existing *user* or
    application role looked free — SQL Server keeps users, roles and
    application roles in one namespace, and the `CREATE ROLE` failed after
    the tables and rows ordered before it had run. `sys.database_principals`
    of every type but `R` is read once, and a taken name is refused with
    the principal's kind, before anything runs.
119. **Which principal holds a role's name is the engine's call, for a
    rename's target as much as a creation's, and it is asked again before
    apply.** 118 read the principals into a map and looked the created names
    up in it: a rename onto a user's name was never looked up, and `Shadow`
    was free to the map while `shadow` was taken to the database, whose
    collation says they are one name. The names a plan's remaining
    statements need free (`CREATE ROLE`, the new name of a rename) and the
    ones they free first (`DROP ROLE`, the old name of a rename) go to the
    engine in one query, compared under `COLLATE DATABASE_DEFAULT`, and the
    answer names the holder in its own spelling. Asked by `plan --db` and
    `bootstrap`, and again by `apply` before statement one and on a staged
    resume: a principal is outside the managed state, so the checksum cannot
    see one created in between, as 92 and 102 say of a member.
120. **`pull --data` draws its row line through `validate`'s own
    evaluation, not a count of its own.** 111 and 114 read the rule's
    severity and count, but the loop was `pull`'s and never consulted the
    suppressions, so a table the project had excused by name was refused at
    the moment it was pulled and accepted by the next `validate`. The rule is
    evaluated by the same function `validate` calls, over the pulled schema
    and narrowed to `data.max-rows`, so the two commands cannot disagree
    again; a block with problems contributes nothing, as it contributes
    nothing to a plan.
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
123. **The names a plan's remaining statements need free are compared with
    one another, not only with the catalog.** 119 asked the engine which
    existing principal holds each wanted name; two declared roles the
    database reads as one name — `Reader` and `reader` under a
    case-insensitive collation — held nothing in the catalog, passed, and
    the second `CREATE ROLE` failed after everything before it had run,
    committed under a staged apply. The wanted names go to the engine
    numbered, joined to themselves under `COLLATE DATABASE_DEFAULT` on
    `a.i < b.i`, and any pair refuses the plan by name before the catalog
    is asked. Declared tables and modules have the same latent shape —
    `dbo.Foo` beside `dbo.foo` — but it predates this phase and is not
    engine-checked anywhere yet; it is recorded here rather than fixed in a
    review round.
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
125. **`status` reports a permission the declarations cannot hold as drift,
    as `verify` does.** 95 and 105 carried such a permission beside the
    schema, not in it, so `verify` could report it and every command that
    records a state could refuse it — and `status`, which computes the
    same checksum from the same schema, could not see it: a `DENY` on a
    managed role read "ok" on the status screen and "drift" from `verify`.
    `status` now applies `verify`'s own filter (a managed role's, not an
    unmanaged one's) to what introspection could not express, and records
    drift with the permission named, before the checksum it cannot enter.
126. **Two spellings of one grant target in a role file are refused, not
    merged.** `SCHEMA::dbo` and `schema::dbo`, or a target with
    surrounding whitespace, parse to one `GrantTarget`, and the map kept
    whichever the loader met last: the other's permissions were gone, and
    the next connected plan revoked them. The loader remembers the spelling
    each parsed target was first written in and refuses the second by both
    spellings, the way a column named twice is refused; merging the two
    lists would hide a declaration that says two different things.
127. **Roles dropped together are dropped parent before member.** A connected
    plan removes a dropped role's members by name before its `DROP ROLE`, and
    every `DropRole` sorted at the same rank — so the order fell to the name
    tiebreaker. Dropping member role `a` before parent role `z` left `ALTER
    ROLE [z] DROP MEMBER [a]` naming a principal that was already gone, and
    the engine refuses that by name: an otherwise valid transactional apply
    rolled back in full. Measured on a live server rather than reasoned: the
    `DROP MEMBER` after the member is gone fails, while dropping a role that
    is a *member* of another succeeds and takes the membership with it. The
    rank is the depth among the dropped roles alone, so a chain of three is
    ordered too, and a member the plan does not drop contributes nothing.
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
130. **The key-alias guard resolves the plan's table name through the
    identities first.** `AlterColumnType` names the table as the plan leaves
    it, while the declarations and the rows read back are keyed by the name
    the database has now (`tables_under`). A table renamed in the same
    revision was found in neither, so a key column changing from `int` to
    `varchar` while a declared key `01` stood for a stored `1` passed the
    guard that exists to refuse exactly that. The mapping both collections
    were built through is now a named function, and the guard goes through
    it.
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
134. **A permission on a schema is probed before the plan runs.** `validate`
    accepts a schema target it cannot see inside — an external schema has no
    declared objects — and this tool never creates a schema, so a grant on
    one the database does not have is a statement the engine refuses. Under
    `apply --staged` every change before it has committed by then, including
    the `CREATE ROLE`. The probe counts one for a schema `SCHEMA_ID` cannot
    find, which lets the engine decide what one name is, here as everywhere
    else. A `REVOKE` names its securable the same way and is probed the same
    way. An *object* target needs no probe: it is declared, so it exists or
    this plan creates it.
135. **Two declarations whose files differ only in case are refused before
    either is written.** A case-sensitive database holds `Reader` beside
    `reader`, and their encoded filenames differ only in case; on a
    case-insensitive filesystem `pull` wrote the second over the first, with
    the identity file still naming both — declarations that cannot round-trip
    to the database, and nothing said. Encoding the case away would fix the
    write and lose the readable name that makes these files reviewable, so
    the collision is detected instead, over every declaration a `pull` or an
    `init` is about to write, before the first one is.

    The refusal is unconditional rather than a property of the filesystem
    underneath: a declaration is written into git and has to resolve to the
    same file on every platform that checks the repository out. This is the
    file-level half of the shape 123 records at the database level.
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
138. **A version 3 state snapshot is still read, as an environment with no
    managed roles.** 84 bumped the snapshot to version 4 when `Schema` grew
    `roles`, so that an *older* client would refuse a state it could only
    read in part — and the check was equality, so this client refused the
    older version too. Every environment recorded before this release holds
    a version 3 entry as its latest, and the recovery the message named,
    `pbps baseline --reason ...`, reads that entry first and failed the same
    way: a deployed environment had no path to the new version at all. The
    fields 4 added default to empty on read, and an environment recorded
    before roles were managed is exactly one with no managed roles, so 3 is
    read as its own; the next record writes 4. Anything older than 3 is
    still refused, for the reasons 2 and 3 give, and anything newer for the
    reason the check exists.
139. **The role drops are re-ordered after `plan --db` fills their members.**
    127 ranked the drops parent before member in the differ's sort — where
    every `DropRole` still has no members, because they are read from the
    environment afterwards and written into the plan. The rank was every
    role at depth zero, the name tiebreaker decided, and the live case that
    measured 127 passed because it handed the emitter changes with the
    members already in. The same ranking is now applied again, over the
    slots the drops already hold, once the members are known; a plan whose
    dropped roles hold none of each other keeps its order, and nothing else
    moves. Measured end to end this time, through `plan --db` and `apply`,
    with names chosen so that the name order is the wrong one.
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

141. **Every command that reads declarations asks the same questions of them.**
    `validate` checked the dialect, the name collisions, the grant targets and
    the rows; `init` checked the first two of those against the project it had
    just staged; and the two commands that hand statements to a database that
    is not a rehearsal — `plan --db` and `bootstrap` — checked none of them. A
    role granting `execute` on a table therefore reached an applyable staged
    plan, and its `GRANT`, which the ordering puts after every table, row and
    module statement, would fail on a database those statements had already
    changed. Three enumerations of one list is a shape where the shortest of
    them is always the one nobody notices, so there is now one:
    `declaration_problems` returns `(finding id, message)` pairs, `validate`
    renders them as findings, and the other three refuse. What each command
    varies is what it does with the answer, never which questions it asks.
    Offline `plan` is deliberately not among them: its output is a
    `PlanOrigin::Preview` that `apply` refuses outright, and its companion for
    "are these valid" is `validate`.

142. **A `schema::` grant target the database spells differently is refused,
    before a plan is written that could never converge.** Every other name in
    a declaration is matched by an identity: a table renamed or recased is the
    same uid, so the two sides agree by construction. A schema has no uid
    (ADR-0002) and a grant target names it as text, so `schema::DBO` against a
    database whose schema is `dbo` is compared as text and differs. Measured
    rather than reasoned: on the case-insensitive test server `GRANT SELECT ON
    SCHEMA::[DBO]` succeeds, the catalog reports the schema as `dbo`, and the
    plan immediately after a successful bootstrap proposes `revoke select on
    schema::dbo` and `grant select on schema::DBO` — for ever, since applying
    it changes nothing about how it reads back. Case-folding the comparison
    would be the wrong fix twice over: on a case-sensitive database the two
    are different schemas, and only the server knows which kind it is. So the
    server is asked — `SCHEMA_NAME(SCHEMA_ID(N'DBO'))` — and a spelling that
    is not the one it returns is refused by `plan --db` and `bootstrap`, the
    sibling of `refuse_misspelt` (101) for the one name with no identity
    behind it. Absent is a different answer from differently spelt, and keeps
    its own message: create it, rather than write it as the database does. The
    pre-flight probe asks the same question again at apply time, for a schema
    created or renamed since the plan was reviewed.

    The sweep found the worse instance: the schema half of a qualified table
    name. The table itself is matched by uid, but the *managed set* is scoped
    by name, so `DBO.customer` on a database whose schema is `dbo` was created
    as `dbo.customer`, recorded as a state holding no tables at all —
    "Bootstrapped: 0 table(s) created", reported as success — and named as
    drift by the `verify` immediately after. Both halves ask the server the
    same question, so both are refused by the same check.

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

154. **A plan a policy refuses writes nothing, the identity file included.**
    ADR-0008 says an `error` refuses to produce the plan before any file is
    written, and the code said so too — in a comment above the policy gate,
    with `write_ids` a hundred lines *higher*. So a revision that minted a uid
    and then failed a rule left `pbps.ids.json` changed for a plan that does
    not exist, and the next run compared against identities no reviewed plan
    ever used. The `--check` branch stays where it is, since it writes
    nothing and answers a different question; only the write moves, past the
    gate.

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

156. **A role the plan touches is exempt down to the permissions it moves, and
    no further.** 153 made this correction for a table's rows and left the
    other half of the model as 150 had it: a role named by any change was
    exempt whole. So a plan that adds one grant covered every *other*
    permission that role holds — and nothing in the run speaks for those. The
    statements grant and revoke what the plan asked for and say nothing about
    what they left alone; the pinned checksum is answered before they run. A
    session that revokes the role's unchanged `SELECT` in between therefore
    had it recorded as this plan's own result, with `verify` clean against the
    revocation ever after.
    The comparison now takes each target the role holds permissions on, on
    either side, subtracts the permissions this plan grants or revokes there,
    and requires what is left to be equal. `Change::grant` names them, the
    counterpart of `Change::row` and for the same caller. Both ends of a role
    rename are paired first: `ALTER ROLE ... WITH NAME` keeps the grants, and
    the grant changes beside a rename name the role as it will be
    (`order_key` puts the rename first).
    Measured through the CLI, on the shape that made it stageable: `REVOKE`
    runs inside an `AFTER INSERT` trigger. With the whole-role exemption in
    place, a revision that widens the role while inserting a row reports
    `Applied 2 change(s) ... recorded as entry #4` and blesses the
    revocation; with the fix the transaction rolls back naming the role, and
    the grant the trigger took is back.
    **The shape, twice now:** an exemption written for "the plan is answerable
    for this object" is wider than the thing the plan is actually answerable
    for. The unit of a plan's responsibility is what its statements name — a
    row, a permission on a target — never the container those live in.

157. **A grant target is spelled the way the plan will leave it, before the
    roles are compared.** Measured on SQL Server 2025: `sp_rename` carries an
    object-level grant to the new name — `GRANT SELECT ON dbo.rn_old` reads
    back as a grant on `rn_new` with nothing else changed. So a plan that
    renames a granted table changes no permission, `diff_roles` emits no grant
    change, and the role is not one any change names.
    Which is exactly why 150's comparison broke on it. The role went down the
    *untouched* path and was compared whole, with the baseline holding
    `dbo.old` and the read-back holding `dbo.new` — one grant read as two, and
    **every rename of a granted table refused and rolled back**. A guard
    written to catch someone else's change invented one of its own, which is
    the worse direction of the two.
    So the baseline's grants are re-keyed through the plan's renames before
    either comparison — the whole-role one and 156's per-target one. The
    planned grants need no such treatment: `order_key` puts renames first, so
    a `Grant` or `Revoke` beside one already names the object as it will be.
    **The sweep this missed.** 150 paired both ends of a table rename and 156
    paired both ends of a role rename, because an object is one object under
    two names. A grant *target* is a third place the same rename shows up, and
    naming the first two made it look done. When a rename can be seen from
    three sides, fixing two of them is not fixing it.

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

164. **A module leaves the managed set when its `DROP` runs, not when the plan
    is written — and an empty new parent is an answer.**
    **The scope.** `modules_after` built the set the *finished* plan leaves,
    and a staged run used it at every checkpoint. So a module whose `DROP` had
    not happened yet was already outside the managed set: absent from each
    checkpoint's schema, and invisible to `--resume`, which scopes the live
    side the same way and therefore compared two states that both omitted it.
    A pause, a hand edit to that module, and the remaining `DROP` ran against
    an object nobody had looked at since the plan was approved.
    The function now takes `Settled` — the same one the movement guard uses,
    because it is the same question — and only removes a dropped module under
    `Whole`. Keeping it mid-run needs no knowledge of which statements have
    run: one already dropped is simply absent from the catalog, which the read
    records truthfully; one still standing stays watched.
    One function with a required argument rather than two functions, on the
    reasoning the `Deployment` struct records: five call sites choose between
    these two answers, and a choice you have to write down is one you cannot
    make by not thinking about it.
    **The probe.** `rows_after` returned `None` for a table this plan creates
    that declares no rows, and `AddForeignKey` read that as "no question" and
    emitted no probe at all. But an empty parent is a very definite answer:
    *every* non-NULL reference on the child side is an orphan. Measured, the
    empty typed relation asks it — `SELECT TRY_CONVERT(varchar(10), NULL) AS
    k0 WHERE 1 = 0` counts 2 of 3 rows, the NULL exempt as the rule says. Under
    `apply --staged` this is the difference between a refusal before statement
    one and a table creation plus every row change committing before the
    constraint fails.
    Absent, empty and unreadable are three different things: this is the third
    time in this phase that "no rows to compare" was returned where "no rows"
    was the finding.

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
    still-declared name is `UnusedIntent` and a rename onto an occupied name
    is too. Rather than add a case for it, the rule it depends on is now
    pinned by a test that names the guard, so if identity ever stops working
    that way the guard is what to revisit. **A hazard made unrepresentable
    still needs the invariant written down** — otherwise the next reader adds
    the case, or removes the rule.

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

172. **The editor schema spells the rule catalogue, because a schema that
    blesses what the loader refuses is worse than no schema.**
    `policies.rules` is a `BTreeMap<String, RuleSetting>`, and the derive turns
    that into `additionalProperties: {$ref: RuleSetting}` — any key at all. So
    an editor completing and validating against the published schema accepted
    `naming.tabel`, marked the file correct, and left the typo for
    `pbps validate` to find later. The catalogue is closed *on purpose*
    (`rules.rs`: "a typo in `policies:` is refused by name rather than
    silently configuring nothing"), which is exactly the knowledge the schema
    was throwing away.
    This is the rule `integration.rs` already states about `deny_unknown_fields`
    — "a schema that made it optional would let an editor bless a file the
    loader rejects, worse than shipping none" — applied to keys instead of
    fields. `rules_schema` writes the ten ids out of `rules::RULES`, with
    `additionalProperties: false` and each key carrying the catalogue's own
    sentence, so there is still one list and not two. Swept: a suppression's
    `rule` is the same closed set and gets the same treatment, because
    suppressing a rule that does not exist suppresses nothing.
    **Not swept into severity, deliberately.** `severity` and the bare-word
    form are also a closed set to the loader — but `Severity::from_str` trims
    and lowercases, so a JSON Schema `enum` of the four words would refuse
    `Error`, which the loader accepts. A schema *stricter* than the loader is
    the mirror image of the bug above, not a further fix of it: it reports a
    valid file as wrong. Both halves of "the schema is the loader" have to
    hold, and only the ids can state it exactly — `rules::rule` compares with
    `==`.
    `SCHEMA_VERSION` goes to 5 for the reason it exists: an editor notices, in
    the way that matters most to it.

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

176. **An unsupported permission on somebody else's object is somebody else's
    business, exactly as the ordinary one beside it is.**
    `pbps_diff::scope` drops a managed role's grant on an object outside the
    managed set, and its reason is recorded there: "a grant on somebody else's
    table is that table's business, and comparing it would have the next plan
    revoke a permission the declarations were never allowed to name." But
    `pulled.unexpressible` carried only `(role, message)` — the securable was
    rendered into the text and then gone — so the filter beside it could ask
    one question, and asked the only one it could. A `DENY`, a column-level
    grant or a `WITH GRANT OPTION` on an unmanaged table therefore made
    `verify` report drift and `plan --db` refuse, while the *plain* grant on
    that same table was dropped without comment. One securable, two answers.
    The fix is to keep the target: `Unexpressible { role, target, what }`, and
    one filter both callers use — `verify`'s and `status`'s copies had been
    written twice and could have drifted apart, which is the second half of
    this entry and the reason the helper is shared rather than corrected
    twice.
    Three things stay, and each for its own reason. A **schema** target is
    declarable (`grants: schema::dbo:`), so a DENY on one is a difference the
    declarations genuinely cannot hold. A **targetless** permission — on the
    database itself, or of a class the model cannot name — belongs to no
    object at all, and a role that gained one has changed (DECISIONS 105).
    And `pull` still warns about **every** one of them unfiltered: it is
    writing the declarations rather than comparing them, so there is no
    managed set yet for anything to be outside of.
    Membership is tested against the managed set as *declared* — the ids
    file's tables and the module set — and not against the cut schema. A
    managed module the catalog could not read back is missing from the second
    and is still ours (491edd9); testing against what came back would have
    excused exactly the case that commit exists for.

177. **A name is kept as the database spells it; `trim()` asks whether there
    is one, and nothing more.**
    Measured on SQL Server 2025: `CREATE ROLE [ app_pad ]` stores the padding,
    and so does `[trail ]`. `needs_quotes` refuses any scalar that is not its
    own `trim()`, so `pull` writes such a name back quoted and YAML hands it
    to the loader intact — where `convert_role` trimmed it. A freshly pulled
    project therefore named a role the database does not have while the ids
    file named the one it does, which reads as an ambiguous replacement rather
    than a clean plan, and `init --from` fails its staged round trip. The
    `renamed_from` beside it had the same trim and the worse consequence: it
    names a principal the database *has*, and trimmed it renames one that is
    not there.
    Both keep the scalar now, with `trim()` used only for "is there a name at
    all". Swept: `TableName::from_str` and `ObjectName::from_str` never
    trimmed, so a padded table or module name already round-trips, and `seq`
    renders every element through `scalar`, so `columns: [" c "]` does too.
    **One asymmetry is left, and deliberately.** A foreign key's target is one
    composite scalar — `dbo.region(region_id)`, the column names joined with
    `", "` inside it — so a padded column name there is indistinguishable from
    the separator's own whitespace. The same column survives in `columns:` and
    does not in `references:`. Fixing it is a *format* change, not a stray
    trim: either the separator stops taking a space (which reinterprets every
    file already written) or the composite grows a quoting rule of its own.
    That is a decision, not a bug fix, and it is recorded here rather than
    made in passing.

178. **177 one crate over, and the sweep that missed it.** `GrantTarget`
    trimmed too — the whole scalar, and again after the `schema::` prefix. I
    swept `pbps-load` for destructive trims and the name types beside it, and
    stopped at the crate boundary; the parser that turns a grant's map key
    into a target lives in `pbps-model`. Measured: `CREATE SCHEMA [ app]`
    keeps its padding, `pull` renders the key as `"schema:: app"` (quoted,
    because the `::` makes `needs_quotes` true whatever else is in it), and
    the reload named a schema the database does not have.
    **It amends 126.** That entry counted "a target with surrounding
    whitespace" as a third spelling of one target, beside `SCHEMA::` and
    `schema::`. The engine says otherwise: `[ app]` and `[app]` are two
    schemas, and `[ dbo].[t]` and `[dbo].[t]` two tables. So they are not two
    spellings to refuse but two targets to keep, and the test that pinned the
    old reading now pins this one. What survives of 126 is its mechanism and
    the case it was really about: the prefix is case-insensitive, two
    spellings of it are one key, and the loader still refuses the second
    rather than letting the map keep whichever came last.
    The cost is a stray space in a hand-written target no longer being caught
    by the duplicate check. It is caught later, as a grant on a securable the
    declarations do not have — which is where a name that does not exist
    belongs, and 126's message would have been the wrong one for it anyway.

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

187. **A policy rule sees types in the dialect's spelling.**
    `column.no-deprecated-type` matched `spec.ty.base` against `text`, `ntext`
    and `image`, and `national text` *is* `ntext` — the alias is in
    `types.rs`. A default-on rule was bypassed by writing the deprecated type
    the long way.
    `pbps-policy` does not depend on any dialect and should not: what a type
    *is* is dialect knowledge, what to think of it is the policy's. So the
    normalizing happens at the boundary, in the caller that already holds a
    dialect — `validate_findings` hands the evaluator a schema whose column
    types are canonical. The rule is unchanged, and so is every future rule
    that names a type.
    A type the dialect cannot normalize is passed through exactly as written:
    it is already an error from `refuse_invalid_declarations`, and rewriting
    what a finding quotes would make the message name something the file does
    not say.

188. **Each rule's schema is that rule's own shape.** 172 closed the rule
    *catalogue* so an editor could not bless `naming.tabel`. Every rule then
    got the same `RuleSetting` schema, so the editor still blessed
    `naming.table: {rows: 5}` — a parameter that rule does not take and
    `Policies::check` refuses. Each entry is generated from the catalogue's
    own `params` now, with `additionalProperties: false`, so the schema and
    the checker draw the same line. The boolean form is `false` alone: `true`
    says nothing about the severity and is refused.
    Two things stay open on purpose. The **severity word** is a plain string,
    not an enum, for 172's reason — `Severity::from_str` trims and lowercases,
    and a schema stricter than the loader is the mirror image of the bug being
    fixed. And a rule's **required** parameters are not expressed as a JSON
    Schema `required`, because "required unless the severity is off" needs the
    same case-insensitive test and would refuse `naming.table: Off`.
    A parameter's *type* is still written down once, in `RuleConfig`; the
    per-rule schemas say which parameters, never what they are. A test ties
    every generated entry back to `rules::RULES` and fails if a parameter is
    added to the catalogue without a shape.

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


192. **`status` decides the row verdict before it writes the inventory, and
    lands a failed read last.** The third instance of the shape 159 and 168
    named: a check that ended the function hid every check after it. The row
    read was the last such return, and what followed it — the managed
    limitations, the unreadable modules, the objects `unmanaged: warn/error`
    sees — needs only the catalog, which had already succeeded, so a read that
    failed was reported *instead of* a stray object rather than beside it. The
    obvious fix, moving those checks above the read, changes what the row
    says on the ordinary path: `record_drift` yields to a state already on the
    row, so a row that moved beside an `unmanaged: warn` would have read
    "warning" with drift demoted to a supplemental issue. So the verdict is
    computed first, as a `Result<bool, String>`, and recorded in two places:
    drift immediately, keeping its rank over the warning; the failure at the
    very end, through `record_unreachable`, which keeps whatever is on the row
    and moves the previous primary state into the issues. The assembly is a
    sync function handed the read's result, so a test can hand it a failure;
    the two tests that do fail against the old return with exactly the
    missing finding.
193. **`DbError` reports the server's error code as text.**
    `server_error_number() -> Option<u32>` was the one place `pbps-db` held a
    T-SQL shape under a neutral name: PostgreSQL's SQLSTATE is five characters
    that may be letters (`42P01`), so a `u32` could never carry the second
    engine's answer, and every caller comparing against it would have been
    written against the first engine's. It is `server_error_code() ->
    Option<String>` now, and the single caller compares against `"208"`. Owned
    rather than borrowed because this driver hands back a number and the error
    holds no string for a `&str` to borrow from (ADR-0014 §1). Landed ahead of
    the PostgreSQL crate, with 194 and 195, so that crate's diff carries only
    what is new.
194. **The transaction framing's text is the dialect's; `pbps-db` runs it.**
    `Conn::begin` held `SET XACT_ABORT ON; BEGIN TRANSACTION;` and `rollback`
    held `IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;` — T-SQL in the crate
    CLAUDE.md documents as holding none, kept there for reasons the code
    explained at length. The reasons stay, beside the text, in `pbps-mssql`;
    `Dialect::transaction_framing` returns the three statements and
    `Conn::begin`, `commit` and `rollback` take them. The method has no
    default: a default would have been one engine's answer under a neutral
    name, the shape ADR-0011 names three times, and PostgreSQL's `begin` is a
    bare `BEGIN;` (measured — any error already dooms its transaction). Ruled
    out: a trait for the framing, since nothing varies between engines but the
    text; and `begin(&str)`, which would make it a second `execute` and leave
    the framing owned by nobody. `pbps-db` depends on `pbps-dialect` for the
    type alone (ADR-0014 §2).
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

196. **A message names a command only to a caller who can run it, and the
    target carries which one that is.** The staged checkpoint refusal was given
    a pointer to `pbps status` (192's sibling, #15): the checkpoint records the
    movement, so `verify` reads clean, and `status` shows the refusal again as
    the failed entry's reason. True for half the callers. `status` takes no
    target at all — it reports on the environments `pbps.yml` configures
    (SPEC 9.2) — so a run that named its database with `--db` was sent to a
    command that answers about other databases, or says none are configured.
    The refusal exists to say where the record is; a pointer that cannot be
    followed is worse there than no pointer.

    The condition is not guessable from what the message had. `Target::label`
    is the environment name for `--env` and a redacted `server/database` for
    `--db`, and the two are the same *kind* of string: reading the origin off
    the label is a heuristic. So `Target` carries `environment: Option<String>`,
    set where `--db` and `--env` are already told apart, and the message asks
    it. The sentence stands without the pointer — "the list above is the record
    of what moved" is the answer — so the `--db` form loses nothing but a dead
    end. Measured against the live server, the same shape sits in `doctor`,
    whose remedies spell `--env <redacted label>` for a `--db` target; that is a
    separate command and a separate issue, not scope here.

197. **A remedy spells the target the way its caller named it.** 196 stopped a
    message naming a command the caller could not run; this is the same shape
    one level down, in the commands a remedy *is*. `doctor`'s per-environment
    remedies — `baseline`, `apply --staged --resume`, `unlock` — each require
    one of `--db` and `--env`, and all three spelled `--env` with
    `EnvDiagnosis::environment`. That field is the display name: the
    environment for an `--env` target, and `db::redact`'s `server/database` for
    a `--db` one. Measured against the live server, `doctor --db` offered
    `pbps unlock --env "localhost,14330"` — an environment `pbps.yml` does not
    have, in a line advertised as copy-pastable.

    So the diagnosis carries `env_name: Option<String>` beside the display
    name, `Some` only where the caller gave one (the `--env` argument, or a key
    of `pbps.yml` in the all-environments loop), and one `target_arg` builds
    the flag: the quoted name, or `--db <connection string>`. The connection
    string is a placeholder rather than the string itself — it carries the
    password, and remedies are printed into CI logs (`explain` already made
    this choice for its approval command, which is why it was not wrong).
    The field is not serialized: it repeats `environment` where it is `Some`
    and says nothing where it is `None`.

    Watching the reverted fix fail is what showed the live half was not being
    measured at all: `doctor_does_not_report_ready_while_the_lock_is_held`
    takes its lock through `docker exec`, and on a host where `docker` cannot
    reach the container it returns early and reports a pass. Under a shim it
    fails against the old spelling with the bad remedy in its output.

198. **A live test arranges its state over its own connection, in its own
    database.** Four tests in `flow.rs` needed state the tool will not produce —
    a lock held by somebody else, a table with an `IDENTITY` no `ALTER` can add
    — and reached for it with `docker exec pbps-test-mssql … sqlcmd`. Each
    treated a failure to reach the container as a reason to `return`, so on a
    host whose `docker` cannot see it (this suite also runs under podman) they
    reported a pass having executed nothing. Measured: with `docker` replaced by
    a program that exits 1, all four were green; the bug one of them was written
    to catch went unmeasured for as long as that was true.

    They now run their statements through `pbps_db::Conn` on the connection the
    test already has, which the file does in a dozen other places, and a failure
    is a panic: setup is a precondition, and a test that cannot arrange its
    state has not passed. Against an unreachable server the four now fail with
    `cannot reach the server under test`.

    Making them run exposed the second half. All four worked in whatever
    database `PBPS_TEST_DB` names, shared with every other test in the file, and
    the ledger entries and tables they leave behind made *other* tests fail —
    which ones depending on the order they ran in. So each takes a database of
    its own, as every other state-writing live test here already does. Teardown
    stays tolerant: after the assertions, a failed `DROP` hides nothing, while a
    panic there would replace the failure the test actually found.

199. **One helper owns a per-test database, and a guard drops it.** 198 gave
    four live tests a database of their own by copying what eleven others did
    by hand, and the copy inherited both defects the hand-rolled shape carries.

    The connection string was built as `format!("{server};Database={name}")`
    with `PBPS_TEST_DB` verbatim. An ADO.NET string is a list of `key=value;`,
    so a trailing separator is legal and makes `;;`, which tiberius refuses:
    "Key must not be empty". Measured — every one of those tests creates its
    database and then cannot connect to it. `with_key` trims the trailing
    separator and any whitespace, and is the only place a key is appended.

    The drop was each test's last statement, so an assertion that panicked
    skipped it, and the name carries the pid, so the next run created a
    differently named database rather than reclaiming the old one. Counted on
    the shared container: 58 left behind. `OwnDatabase` drops in `Drop`, which
    runs while unwinding. The teardown stays tolerant for a second reason
    there: a panic during unwinding aborts the process and would take the rest
    of the suite with it.

    Both halves are pinned by tests that fail when reverted — the string one
    without a server, since the defect is in the string.

## Phase 5 prep — module identity (ADR-0009 §1)

200. **A module is identified by a typed `ModuleId`, and which fields carry
    that identity depends on the kind.** `Schema::modules` was keyed by
    `ObjectName`, which says every module in a schema has a distinct name.
    Measured on PostgreSQL, that is false twice over: functions and procedures
    overload, so `app.f(integer)` and `app.f(text)` are two objects with one
    name; and a trigger's name is unique only within its table, so `audit` on
    `orders` and `audit` on `customers` are two objects with one name in one
    schema. `ModuleId` is therefore an enum — `Named(ObjectName)` for a view,
    `Routine { name, args }`, `Trigger { on, name }` — and the shape of the
    key is the shape of the identity.

    **The signature is a `Vec<ColumnType>`, not a string.** Two semantically
    identical schemas must be `==` (the inviolable constraint), and
    `f(int)`/`f(integer)`/`f( INTEGER )` are one signature spelled three ways.
    A string key would have made three modules of one. The `Display`/`FromStr`
    pair exists for the JSON map key and for messages, and parses back to the
    same value; it is not the identity.

    **The trigger's table lives in the key, not in `Module`.** `Module::on` is
    removed. Containers hold names, elements do not: with the table in both
    places a snapshot could say `app.audit` is on `app.orders` in the key and
    on `app.customers` in the value, and nothing in the type would stop it.
    Removing the field makes that unrepresentable rather than checked. The
    declaration file keeps both lines — `trigger:` and `on:` — because that is
    where a human writes them; the loader folds them into one key and refuses
    an `on:` on a kind that has no table, or a trigger whose schema disagrees
    with its table's.

201. **Namespace sharing and overloading are dialect questions, asked of the
    dialect.** `check_names` refused two modules with one name and a module
    sharing a table's name, as one rule for every engine. Both halves are
    engine-specific — on PostgreSQL views share the table namespace and
    routines do not, and routines overload — and `pbps-model` may not know
    which engine it is describing. So the rule moves to
    `pbps_dialect::check_module_names`, over `shares_namespace_with_tables`
    and `overloads`; `check_names` keeps only what is true of every engine.
    MSSQL answers as before, so no declaration that was valid becomes invalid.

202. **Routine identity is normalized by its own hook, and a collision is
    reported rather than merged.** PostgreSQL discards type modifiers when
    identifying a routine — measured, `f(varchar(10))` and `f(varchar(20))`
    are one function — which `normalize_type` must not do, because a column's
    modifier is part of the column. `normalize_routine_arg` is therefore a
    separate hook, applied to the loaded schema in one CLI pass after loading.
    When two declarations normalize to one id, the pass reports both and the
    command bails. Silently keeping the second would have left the first a
    declared module that no plan ever mentions — absent and unreadable are not
    the same, and only one of them is good news.

203. **The state snapshot's oldest readable version becomes its current one.**
    A version 5 snapshot spells a trigger as `app.audit` with its table in a
    field beside it. This build reads that key as a view and has nowhere to
    put the table, so "no modules of that shape" would be a *missing* reading
    presented as a true one — the failure this tool exists to prevent. The
    meaning of the module map changed, not just its contents, so the snapshot
    is refused with the remedy (`pbps baseline`) rather than upgraded in
    place. Cheap because the format numbers are still pre-release and reset at
    the first tagged release (145). The saved plan goes 4 → 5 for the same
    change with the same reasoning, and refuses the same way.

204. **A guarantee the map key used to give is now a check, because removing
    the reason for one is not replacing it.** `Schema::modules` keyed by
    `ObjectName` made two modules with one name *unrepresentable*: the map
    held one entry per name and that was the end of it. Keyed by `ModuleId`
    they are representable — a trigger is told apart by its table, a routine
    by its signature — and on an engine that keeps every kind in one namespace
    per schema, `app.orders.audit` beside `app.customers.audit` is two objects
    it cannot both have. Nothing caught that: `check_module_names` compared
    each module against the *tables* and never against the other modules,
    because under the old key there was nothing to compare. `validate` passed,
    and the refusal arrived from the engine partway through a staged apply.

    So the check now groups the kinds the dialect keeps beside tables and
    refuses a repeated `object_name`, naming both identities — "one of these
    is wrong" is not a finding anyone can act on. The kinds with namespaces of
    their own are left alone, because there `ModuleId` *is* the whole identity
    and two keys are two objects. This is the guard-whose-reason-has-gone rule
    turned on the change that removed the reason: the key was the guard, and
    it had to be replaced in the same breath it was taken away.

205. **A module whose name the id's string form cannot carry is inventoried,
    not recorded.** `ModuleId` crosses a snapshot, a plan and every message as
    a string in which the punctuation is structural: `.` separates the parts
    and `(` opens a signature. A legal quoted identifier may contain either —
    `[audit.v1]`, `[sales(archive)]` — and such an id would be written
    faithfully and read back as a *different* module: a trigger on
    `dbo.audit`, a routine with an argument named `archive`. Under the
    `ObjectName` key the same read failed loudly, because `dbo.audit.v1` was
    no shape a name could take; the typed key gave every such string a
    meaning, and so turned a loud failure into a quiet one. The check sits in
    introspection — the one place engine names enter the model — and asks the
    round trip itself, `id.to_string().parse() == id`, rather than listing
    forbidden characters, so it stays right if the string form changes. The
    module is inventoried with the reason, as every other shape the format
    cannot carry is, because a quiet refusal at the snapshot would leave the
    next plan proposing its destruction. Not in `ObjectName::new`: engine
    names arrive there for tables too, whose string form has the same
    property and is out of this change's scope. The same check guards a
    grant target read back from the catalog: `GrantTarget::Object` on
    `[dbo].[sales(archive)]` would be read back as a grant on a routine, so
    the permission is reported as unexpressible. The structured target is
    kept on the report — only its string form is ambiguous — because the
    target is what scopes an unexpressible permission to the managed set; a
    first version dropped it, and a grant on an *unmanaged* object of such a
    name was then reported, and refused, where an ordinary grant on the same
    object is ignored.

## Repository process

206. **CI is a gate started by hand, not feedback on every push.** The
    workflow no longer has a `pull_request` trigger. A review round takes
    several commits, and running the full matrix on each of them spent the
    private repository's minutes on states nobody would merge. Instead, the
    checks CI runs are run locally before every push (CLAUDE.md), and CI is
    started once, on the commit about to be merged, with
    `gh workflow run ci.yml --ref <branch>`. A repository ruleset
    (`ci-before-merge`, outside the repo — hence this entry) requires the
    `ci-gate` commit status to be green on the PR head, so a push after the
    run clears it and the run has to be repeated. The ruleset is strict: the
    branch must contain the latest `master` before the merge, so the tree CI
    ran on is the tree the merge commit holds, and this workflow runs nothing
    on `master` after a merge. (The dependency audit is a separate workflow and
    keeps its own `master` trigger: its subject is the advisory database, which
    moves without the tree.) The price is that a PR waiting while `master`
    moves has to rebase and run CI again; with one issue in flight at a time
    that is rare, and the alternative — a post-merge run on `master` as a
    safety net — doubled the minutes of every merge to cover it.

    **The first version of this did not work, and looked as though it did.**
    It required the five job names directly, on the theory that a check run
    on the head commit is a check run. It is not: a check run reaches a pull
    request through the *check suite* that holds it, and a suite is
    associated with the PR only when the run's event is one of
    `pull_request`, `pull_request_target`, `push` or `merge_group`. A
    `workflow_dispatch` suite is associated with nothing. So the Actions tab
    showed five green jobs on the PR head while the PR's own checks list
    showed none and the merge box sat on "Expected — Waiting for status to be
    reported" forever. The gate this entry describes had locked out the very
    pull request that introduced it.

    The repair keeps the trigger and changes what is reported: a `gate` job
    needing all five writes one `ci-gate` **commit status** to the dispatched
    SHA. A commit status has no suite — it is addressed to a commit and read
    off that commit — so the association problem cannot arise. It is also
    what makes the expiry exact rather than incidental: a status belongs to
    one SHA and is never inherited, so the next push starts with no `ci-gate`
    and the gate is shut without anyone clearing anything.

    Not the Checks API, which is the more commonly suggested workaround for
    the same limitation: a check run created that way still lands in a suite,
    and a suite is the mechanism that just failed here. Not a
    `pull_request` trigger with a label or `ready_for_review` guard either —
    a required job skipped by `if:` reports `skipped`, which GitHub counts as
    success, so the guard that saves the minutes is also the guard that opens
    the gate. Not a merge queue: `merge_group` would be associated correctly,
    but merge queues need an organisation-owned or public repository and this
    one is neither.

## Phase 5 prep — the declared record (ADR-0009 §2.2, ADR-0013 §3–§4)

207. **The state keeps what was declared beside what it read back, and a
    version 6 state reads as having declared nothing.** `StateSnapshot`
    gains one field, `declared`, holding the three ADR-0009 counts: each
    managed module's definition as declared when last written, the three
    verbatim expressions — `Column::default`, `CheckConstraint::expression`,
    `Index::filter` — as declared, and the bindings of ADR-0013 §3. One
    struct rather than three loose fields, because the three move together:
    `bootstrap` records them from the declarations it created everything
    from; `apply` takes the previous state's record and advances it by the
    plan — from the plan alone, since `apply --plan` needs nothing else (SPEC
    §7.3): a change that writes an object carries the text it wrote, a drop
    forgets it, a rename re-keys it; a staged checkpoint carries the previous
    record, since nothing plans against a checkpoint; `baseline` and
    `snapshot` record none, because they applied nothing and either obvious
    filling is wrong (ADR-0009 §2.2). The version goes to 7, and 6 stays
    readable: a version 6 state recorded nothing as declared, so an empty
    record is what it truly says — the rule that keeps 4 readable (160) and
    not the one that refused 5 (203), where the same spelling had changed its
    meaning. The issue that scoped this said such a state would be refused;
    reading it is the better answer, and this entry is where the change of
    mind is written down.
208. **The differ compares the declarations against what was declared when
    each object was last written, and falls back to the read-back where
    nothing was recorded.** `Declared::overlay` lays the recorded texts over
    the read-back the connected plan (and a snapshot `--base`) compares
    against; a git baseline is declarations already. Where the record has no
    text for an object — an environment adopted with `baseline`, a version 6
    state, an object created by hand — the read-back stands, which is what
    the differ always compared. On an engine that stores what it was given
    that is the same answer as before this change; on one that respells, the
    object is restated once and the apply records what it declared, which is
    the "restated once" ADR-0009 §2.2 asks for, reached by a fallback rather
    than a rule of its own. And the read-back is not the engine that stores
    what it was given: **measured**, SQL Server reads a default `GETDATE()`
    back as `(getdate())`, a check `n > 0 AND label <> 'none'` as
    `([n]>(0) AND [label]<>'none')`, and a filtered index's predicate the
    same, so `plan --db` straight after a `bootstrap` restated the default,
    dropped and re-added the check and dropped and rebuilt the index —
    marked destructive — on every run. ADR-0013's Limits called that a
    reasoned worry, unmeasured on SQL Server; it was a shipped bug, and the
    live test pins the fix.
209. **A binding is the dialect's to record, and SQL Server records none.**
    The candidate set of ADR-0013 §3 is a property of a search path, and SQL
    Server has none: an unqualified name binds to the *caller's* default
    schema when the statement runs, a property of the session and not of the
    object, so there is no set to record at creation that the catalog could
    later be asked about. Recording a guess — the identifier scan's qualified
    references, say — would give the field content that means nothing and
    could not move. The shape is in the format now, with a round-trip test,
    so the PostgreSQL crate adds the recording and the rebuild decision and
    changes no format.

## Phase 5 prep — permissions (ADR-0010 §3, §6)

210. **`Permission` is the union of the engines' words, and each dialect
    refuses the ones its engine lacks — in three places, from one table.**
    `usage`, `create`, `truncate`, `trigger` and `maintain` join SQL Server's
    eight; `alter` and `view-definition` stay, and become PostgreSQL's to
    refuse. Not a per-dialect enum, for the reason ADR-0010 §6 gives:
    inviolable constraint 1 needs one model in which two identical schemas
    compare equal, and a dialect inside the type would break that. The new
    words are appended, because the derived order is the order `fmt` writes a
    grant's permissions in, and inserting one would rewrite every role file.
    On SQL Server the engine's set is one constant, `validate::PERMISSIONS`,
    and three consumers apply it: `validate_role` refuses a word by name on
    any target (the engine's parser stops at the word before it looks at the
    securable — measured, `GRANT USAGE ON dbo.t TO r` and each of the other
    four, on an object and on a schema, are Msg 102 "Incorrect syntax", not
    Msg 4606), `emit` returns `Unsupported` rather than render a statement
    that parser would stop at, and the catalog read-back reports a parsed
    word the engine lacks as unexpressible rather than fold it into a role.
    The third is belt and braces: measured, `sys.fn_builtin_permissions`
    names none of the five in any class, so SQL Server cannot return one
    today — but a model that spells more words than the engine has is a new
    shape, and the read-back is the one consumer where "parses" used to mean
    "is this engine's". The editor schema lists the union (schema version 7):
    an editor that accepted any string blessed `contrl`, and one listing a
    single engine's words would refuse a PostgreSQL project's `usage`; which
    word a dialect lacks is `validate`'s finding, not the editor's.
211. **Role existence is a dialect capability, `Dialect::manages_roles`, and
    SQL Server's answer is `true`.** A SQL Server database role lives inside
    the one database the tool is connected to, so ADR-0005 manages its
    existence and nothing here moves. A PostgreSQL role is a cluster object,
    granted in every database of the cluster and visible from each; a tool
    whose blast radius is one database must not own an object whose blast
    radius is the cluster (ADR-0010 §3). That dialect will answer `false`, and
    on it `plan --db` refuses a declared role the cluster lacks with the
    `CREATE ROLE` to run by hand, while `drop-role` revokes the declared
    grants and leaves the `DROP ROLE` to a human. Nothing reads the answer
    yet — the CLI's role-existence paths are written for one engine — and
    the reading lands with the dialect that first answers `false`, where the
    live suite can watch it. Grants are managed either way: the capability is
    about the principal, not what it holds.

212. **Among the modules that share a routine's name, only `depends_on:`
    orders.** The identifier scan of ADR-0002 finds a reference by qualified
    name, and where a kind overloads, a name is not an identity (ADR-0009 §1
    says so of the scan itself). So `app.f` in a routine's body matched every
    `app.f(...)`: each overload got an automatic edge to all its siblings, and
    a body that mentions its own name — a recursive overload, or two that call
    each other one way — made a cycle of them. `creation_order` then fell back
    to name order, so `app.f(integer)` came before `app.f(text)` even where
    `depends_on:` said the opposite, and the escape hatch could not repair it:
    it adds an edge and cannot remove one.

    `creation_order` now takes no automatic edge from a routine to a module
    with the same referenced name. Two things follow, and both are chosen:

    - **A reference to an overloaded name from any other module still orders
      that module after every overload.** The scan cannot tell which one is
      meant, and creating a caller before one of them fails; over-ordering
      costs a position in the plan, under-ordering costs a failed apply.
    - **A routine that genuinely references a same-named module of another
      kind needs `depends_on:`.** PostgreSQL lets a view `app.f` and a
      function `app.f(integer)` coexist (ADR-0009 §1: views share `pg_class`
      with tables, routines do not), and the scan cannot tell that reference
      from a sibling's. The rarer case pays, and it pays with the hatch that
      exists for what a scan cannot see.

    No dialect overloads yet — MSSQL answers `overloads → false` — so nothing
    in the plan changes today. The rule is here rather than in the PostgreSQL
    crate because it is about what a *name-based scan* can know, which is the
    model's question and not an engine's.

213. **The declaration schema says what the loader accepts, and completes from
    what `fmt` writes.** `Permission::from_str` folds case and reads `_` or a
    space where the canonical word has `-`, so that a user who types what the
    engine prints (`VIEW DEFINITION`, `view_definition`) is not corrected
    (ADR-0010 §6). The editor schema listed the canonical words alone, so a
    schema-aware editor flagged `dbo.t: [SELECT]` while `pbps validate`
    accepted it — the editor stricter than the loader, and the editor is what
    a user reads first.

    The `items` schema is now two branches. A `pattern` says which spellings
    validate; an `enum` of the canonical words is what an editor completes
    from. One list could not do both: a closed list wide enough to accept
    every spelling would offer all of them as completions, and the word `fmt`
    writes would be one suggestion among four.

    The pattern is written out per character — `[Ss][Ee][Ll]...` — rather than
    with a case-insensitive flag, because JSON Schema's patterns are ECMA-262,
    which has no inline `(?i)`; a schema carrying one would be a pattern every
    validator reads differently. Both branches are generated from
    `Permission::ALL`, so neither can drift from the loader, and the tests
    assert each spelling against `Permission::from_str` as well as against the
    pattern — a pattern nothing runs is a claim, not a check.

    The padding is not `\s` either, for the same reason at a smaller scale.
    ECMA-262's `\s` is neither a subset nor a superset of Rust's
    `char::is_whitespace`, which is what `trim` asks: it omits U+0085, which
    `trim` removes, and includes U+FEFF, which `trim` leaves in place. Written
    with `\s` the schema would have broken *both* halves of 172 at once —
    refusing a padded declaration `validate` accepts, and blessing one it
    refuses — two characters wide in each direction. The class is scanned out
    of `char::is_whitespace` and written with literal characters, because the
    two readers of this pattern share no escape syntax: ECMA-262 spells U+1680
    `\u1680` and has no `\x{...}`, and the `regex` family the tests run it with
    is the mirror image. A literal character is what both read.

    `SCHEMA_VERSION` goes to 8, for the reason it exists: version 7 published a
    closed list that refused `SELECT`, and this one accepts it. A consumer
    keying on that number to cache or select an artifact could not otherwise
    tell the two apart, which is the drift detection the field is for.

## Phase 6 — the envelope contract (ADR-0015, #64 step 2)

214. **The envelope's schema is published as one document with a branch per
    command, selected by `command`.** SPEC §9.8's shape has been a Rust type
    and an example since Phase 3.1; ADR-0015 decision 1 makes it the whole
    contract between the UI and the tool, and a contract nothing publishes is
    one nobody can check against.

    One document rather than one per command, because what a consumer holds is
    *an envelope*: it reads `command` and only then knows what `data` is.
    Publishing them separately would make it choose a schema before reading
    the field that decides which one applies. Each branch pins `command` to a
    constant, so `oneOf` picks exactly one and a payload that happens to fit
    another command's shape is rejected rather than silently read as that
    command's.

    Every branch is generated from the type the command serializes, the way
    the declaration and config schemas are generated from the loader's and the
    config's types (SPEC §14.1). The list of command-to-payload pairs is
    written once, in `envelope_branches!`, and a flow test reads the command
    names back out of the published schema and compares them with the
    commands `--help` says take `--format json` — so a command given the flag
    without a line in that list, or a line without the flag, is a failing test
    rather than an envelope nothing describes.

215. **`plan --db` stays outside the envelope set.** #64 asked whether the
    connected plan should join it now that a UI will trigger one (step 5).

    It does not, and the reason is the one SPEC §9.8 already gives: `plan --db`
    is not a read-only command. It connects, reads the ledger, and *writes the
    deployment artifact* — the file the checksum gate pins. What a reviewer
    reads from that artifact is `explain`, which does speak the envelope, and
    which reads the file rather than the run that produced it. A UI that
    rendered the producing run's own JSON would be showing a description of the
    artifact that was not computed from the artifact, which is precisely the
    gap the checksum exists to close.

    So the UI's trigger path stays: run `plan --db`, then read the plan back
    with `explain --plan --format json`. ADR-0015 decision 1's list of what
    speaks the envelope is exact and unchanged.

216. **`state list` carries the ledger's columns, never the recorded schema.**
    The timeline the UI draws needs an id, a time, a kind, an operator and the
    provenance fields; the snapshot's `schema` and `ids` are the whole database
    twice over. A payload that carried them would send megabytes to a page
    drawing a list of dates, and they are what `state show` and `state export`
    are for — the second half of SPEC §14.1's row, deliberately not built here.

    `initialized` sits beside `entries` because an empty list is the one
    rendering that must never stand for "this database has no ledger" or "the
    server could not be reached". The three are a note plus `initialized:
    false`, a note plus `initialized: true`, and `unanswerable` with no `data`
    at all — three answers, as the repository's rule for absent, empty and
    unreadable requires.

217. **A `--limit` too large saturates; it does not wrap and does not refuse.**
    `TOP (n)` takes a signed 32-bit count and the flag takes a `u32`, so
    `--limit 4294967295` cast with `as` became `TOP (-1)` and the server
    refused the whole query. A number meaning "more than there could ever be"
    turning into an error is the wrong answer twice over: the caller asked for
    everything and got nothing, and the message named a syntax error rather
    than a limit.

    Two guards, because they fail differently. The flag's parser refuses `0`
    and anything above `i32::MAX`, so a person who types a number the ledger
    cannot mean is told so by name. The reader saturates, because it is a
    library function whose caller need not be the CLI, and "as many as the
    server can return" is the only reading of a count larger than any table.

218. **An entry this build cannot read is carried, not thrown.** `state list`
    reads rows written by every version that ever touched the environment,
    including ones older than `OLDEST_READABLE_VERSION`. Parsing each row into
    a `StateSnapshot` and returning `Err` on the first failure meant one
    unreadable row erased the whole timeline above it — the newest entries, the
    ones a person is looking at the list to find.

    So the reader projects the ledger's own columns (id, time, kind, operator,
    provenance) and treats the recorded state as optional: a row that will not
    parse, or whose version this build does not read, keeps every column the
    ledger stores and carries the reason in `unreadable`, with a
    `state.entry-unreadable` finding naming the row. `history` keeps the
    stricter contract — a caller asking for states wants states — and the two
    doc comments point at each other. (The one finding id named here became
    two in 222, once the two ways a row can be unreadable were told apart.)

    This is the repository's absent/empty/unreadable rule applied one level
    down: it holds for a row as much as for a ledger.

219. **Presence is asked by attempting the statement, never by `OBJECT_ID`.**
    `is_initialized` asked the catalog whether `dbo.__pbps_state` exists. The
    lock reader had asked the same way and was fixed one shape earlier; the
    ledger reader was not swept with it.

    Measured against the pinned server, with a contained user holding no
    permission on an existing `__pbps_state`: `OBJECT_ID` answers NULL and
    `HAS_PERMS_BY_NAME` answers 0 — the same answers an absent table gives.
    Attempting `SELECT TOP (0) 1 AS present FROM dbo.__pbps_state` separates
    them: **208** when the table is absent, **229** when it exists and is
    hidden, **207** when it is there with a shape this build does not know, and
    every other failure stays a failure. `TOP (0)` still resolves the object and
    still checks the permission, so the probe costs no rows.

    Fixed in `is_initialized` itself rather than at the new call site: `latest`,
    `history`, `timeline`, `prune`, `doctor` and `explain` all asked through it,
    and each turned "not authorized to look" into "this database has no pbps
    ledger" — `doctor` reporting `uninitialized`, `explain` offering
    `bootstrap`, `state list` printing an empty history for an environment with
    years of it. All six inherit the fix with no signature change, and both
    outside callers already routed an error correctly.

220. **An `unanswerable` envelope exits 1, and never 2.** `state list`'s
    ledger-read failure built its own findings and returned `Found::reported()`,
    which `main` maps to `EXIT_FINDING`. The JSON said the question could not be
    answered while the exit code said there was something to act on — decision
    34's two audiences, given contradictory instructions by the same run.

    The branch goes through `output::or_unanswerable` like every other step in
    the command, so the envelope and the exit code are produced by one thing.
    That is the general rule: a command that reaches for `Found` on a path where
    it also emits `unanswerable` has routed a tool failure to the wrong person.

221. **A table cell is escaped for the terminal; the JSON keeps the original.**
    `state list --format human` lays its columns out by counting characters, and
    `operator` and `reason` are free text: `--reason $'ticket-9\nwhy'` reaches
    the ledger as written, and a `failed` entry can carry a driver's multi-line
    message. A cell holding a line break ended its row early, so the rest of the
    row began again at column 1 and read as an entry of its own — a table that
    did not say "this reason had a newline in it" but said something false about
    how many times the database had been deployed to, in a command whose whole
    output is that list.

    Control characters are shown escaped (`\n`, `\r`, `\t`, `\u{7}`) rather than
    stripped: what was recorded is the point of the column, and a silently
    shortened reason is the same class of lie one column over. `--format json`
    is untouched — a consumer parsing the envelope wants the bytes the operator
    typed, and JSON has its own escaping for them.

    Applied to every cell rather than to the two that are free text today. The
    rule the repository keeps arriving at: prefer making the bad value
    unrepresentable over checking for it at the sites that happen to hold it now.

222. **"Older than this build reads" and "damaged" are two answers, not one.**
    `timeline_from_row` parses the recorded state and then version-checks it,
    and both failures were carried as a string. The warning built from that
    string said the entry "was recorded by a version this build cannot read" —
    so a row whose JSON is truncated told the operator to go and find a newer
    pbps, which is not a thing that exists for a damaged row.

    `TimelineEntry` now holds `Result<StateSnapshot, Unreadable>`, with
    `Unreadable::UnsupportedVersion` and `Unreadable::Malformed`. The envelope
    carries the same split as a tagged object — `{"kind": "malformed",
    "detail": ...}` — because a page that draws "upgrade pbps" must be able to
    decide *not* to draw it, and reading that out of a sentence is not something
    a schema can promise. Two finding ids for the same reason: an id is what a
    consumer keys on, and these two are different jobs.

    A `Result` rather than a snapshot beside an optional reason: exactly one of
    the two is true of every row, and the struct that could hold both needed a
    comment saying it never would.

    **One reader, not two.** The version-before-shape order `from_json` was
    given for #50 is exactly what this needs, so `StateSnapshot::read_json` *is*
    that reader with its failure typed, and `from_json` is `read_json` with the
    two flattened into the one sentence a reader of a single state wants. A
    second copy of the ordering in the ledger crate would have been a second
    place to get it wrong.

    That ordering is also what makes the distinction worth drawing. Without it,
    an old row fails on whichever field of its older shape serde reaches first
    and is `Malformed` — measured, on a base before it, and it had a fixture of
    this test passing while proving the wrong thing. With it there are three
    cases and the reader gets all three right: below the range is
    `UnsupportedVersion` and says which command fixes it; unparseable is
    `Malformed`; and a *readable* version carrying an unknown field is
    `Malformed` too, because within a version this build reads, a field it does
    not know is a hand-edited or corrupt row. The live test carries one of each.

223. **A field `serde` may omit is a field `schemars` must call optional.**
    `EnvDiagnosis` skips `permissions_unknown` when it is false and
    `absent_schemas` when it is empty — the good news, which is what an
    operator sees most. `schemars` has no way to know that: it reads
    `skip_serializing_if` as nothing at all and lists the field under
    `required`. So the published envelope schema rejected the healthy `doctor`
    output, and the contract ADR-0015 decision 1 rests on was broken by the
    first command to exercise it.

    `#[serde(default, skip_serializing_if = ...)]` is the pairing that keeps
    them honest, and it is what every other such field in the workspace already
    carried — these two were the only pair without it, which is why the sweep
    for this shape found nothing else. `default` on a type that is only
    serialized reads oddly for a moment and then reads correctly: the value the
    field takes when it is absent is exactly what `skip_serializing_if` says it
    was.

    The test that missed it was validating `doctor`'s envelope already — with
    no environments configured, so the type in question was never serialized.
    Covering a command is not covering its payload's nested types, and the
    envelope validation now runs `doctor` against a real environment, where the
    two fields are absent because the news is good.

224. **The published envelope pins its own version, as it pins the command.**
    Each branch already fixed `command` with a `const`, because that is the
    field `oneOf` selects on. `schema_version` was left as a plain integer, so
    this document — the one a consumer written against envelope version 1
    validates with — accepted an envelope from a later version whose extra
    fields it does not describe.

    SPEC §9.8 says what that field is for: it is the version of the envelope
    alone, and it "moves when a consumer would have to change". A schema that
    accepts any value there says "fine" about the one case the field exists to
    refuse, which is this repository's oldest mistake in a new place — a reader
    told "not readable" and answering "nothing wrong".

    Pinned, a version bump is a validation error the consumer already has a
    branch for, at the moment the envelope arrives, rather than a payload
    silently read as something it is not. It also makes the bump a deliberate
    act on this side: `output::SCHEMA_VERSION` moves, the checked-in schema
    moves with it, and the drift test refuses to let one move without the other.
## Phase 5 — the connected boundary and the PostgreSQL crate (ADR-0011, ADR-0014)

225. **`Conn` becomes an enum over two drivers — not a trait object, not a type
    parameter.** ADR-0014 ruled out deciding *how* while there was one real
    driver ("the useful abstraction is the one drawn from two implementations
    that both exist"); this is the second, so the deferral ends. Measured on
    `4abb917`, the enum leaves all 22 `pbps-mssql` functions that take
    `conn: &mut Conn` untouched. A trait object needs `async fn` in a
    dyn-compatible trait, which Rust has not got — so hand-rolled boxed futures
    or a new dependency, to abstract over exactly two implementations that both
    live in this workspace. Generics spread a type parameter across those 22
    functions and everything calling them, and the dialect is a runtime value
    out of `pbps.yml`, so the dispatch would only move to the CLI. The enum is
    also what `Param` chose in this same file, for this same reason, before
    there was a second driver.

    The cost is the `FromColumn` blanket impl: two of them, one per driver,
    overlap and coherence refuses them. The closed set that replaces it is
    `&str`, `i32`, `i64`, `i16`, `u8` and `bool` — **from the compiler, not
    from reading the source.** Counted by eye it looked like three, because
    `bool`, `i16` and `u8` reach the seam through `get(&row, "max_length")`
    with the type inferred from the struct field and never spelled at the call
    site. The set is not engine-neutral either: `u8` is SQL Server's `tinyint`,
    and PostgreSQL's arm refuses it rather than inventing a conversion.

    ADR-0007 decision 5's "exactly one file" becomes **one file per driver**:
    `pbps-db::mssql` names `tiberius`, `pbps-db::postgres` names
    `tokio_postgres`, and nothing else in the workspace names either.
    `DbError::Driver` stops carrying one driver's error type and carries text
    plus an optional code, so the seam's own error names no driver.

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

228. **One rustls crypto provider in the tree, and the connector names it
    anyway.** Asking for `ring` while `tiberius-ng` resolves `rustls` with its
    own default compiled **both** providers in. rustls then cannot determine a
    process-level provider and **panics** — not errs — the first time a
    `ClientConfig::builder()` runs, which is inside a connection, where the
    seam has no way to report it. Nothing caught this: it builds, it lints, and
    `cargo deny` is green; the PostgreSQL live suite hit it on its first
    connection to a real server, which is the argument for that suite existing
    before there is a dialect to test.

    So the PostgreSQL TLS stack takes `aws-lc-rs`, the one already in the tree,
    and `pbps-db::postgres` builds its config with `builder_with_provider`
    rather than the process default — the same shape `tiberius` uses on the
    other side of the seam. One provider makes the ambiguity impossible; naming
    it makes a future second provider unable to change which one this connector
    uses, or to reintroduce the panic.
    uses, or to reintroduce the panic.

229. **The connection seam dials one TCP endpoint, and refuses every string
    that means anything else.** `Conn::connect` opens the socket itself, which
    is what keeps `Connect` and `ConnectTimeout` two errors instead of one
    (ADR-0014 §3) — so it has to choose a host and a port, and libpq strings can
    name things that choice cannot honour: a Unix socket path, a `hostaddr` the
    driver would dial while `host` is what TLS checks, or several hosts to try
    in turn. Each of those is now a `BadConnectionString` that says which.

    It was a silent fallback to `localhost:5432`, with a comment claiming the
    connection would then "fail to connect saying so". It would not: a machine
    configured with a Unix socket is exactly the machine with a server on
    `localhost:5432`, so the fallback **succeeded**, against a different
    endpoint under a different authentication method. Supporting these properly
    is a feature and belongs to whoever needs it; guessing is not the smaller
    version of it.

230. **Identifier rules are the engine's, measured, and neither is inherited
    from the SQL Server side.** Two of them, both silent when wrong, both
    pinned by the live suite against PostgreSQL 18.6:

    - **Case folding is ASCII-only.** `CREATE TABLE AÄ` makes the relation
      `aÄ`, not `aä` — the server downcases byte by byte and leaves the high
      bit alone. Rust's `to_lowercase` is Unicode-aware and folded one
      character too many, so a declaration would have been keyed as a name
      introspection never returns: drift no apply can settle, and a `CREATE`
      that makes an object under a name nobody asked for.
    - **The length limit is 63 *bytes*, and it is enforced here because the
      server does not enforce it.** It truncates and says so in a `NOTICE`
      nothing reads. Measured: a longer name records itself at one length and
      reads back at another, and two names differing only after byte 63 collide
      — the second `CREATE TABLE` fails with `relation … already exists`,
      naming a table the declarations do not contain. The SQL Server
      counterpart counts **characters** (128), so copying its shape would have
      been wrong in both directions: 32 `ä` is 32 characters and 64 bytes.
      been wrong in both directions: 32 `ä` is 32 characters and 64 bytes.

    **Amended: the limit holds for the names a routine argument spells.** The
    module gate refused a routine's own name over 63 bytes and let a type
    name in its argument list through. Measured, with a type `dq.t…t` of 63
    bytes, `CREATE FUNCTION dq.f(a dq.t…tx)` spelling one byte more is
    accepted with a `NOTICE`, the routine is identified as `dq.f(dq.t…t)`,
    and the same statement run again is refused as already existing. The
    declared key is the untruncated spelling, so `module_oid` finds nothing
    under it, the routine is planned as absent on every plan, and the
    `CREATE` it emits is the one the engine refuses — a plan applied once and
    refused ever after. `validate_module` refuses an argument holding a name
    over the limit, quoted or bare, before anything connects.

231. **`target_session_attrs` is reproduced at the seam, not refused and not
    dropped.** `Config::connect` runs a `SHOW transaction_read_only` probe
    *after* the handshake; this seam calls `connect_raw` — which is what keeps
    the three connection failures three (ADR-0014 §3) — and inherits none of
    it. Dropped silently, a string saying "never a writable primary" would have
    got one and run DDL on it.

    Reproduced rather than refused, unlike `hostaddr` and multiple hosts in 229,
    and the difference is which failure each choice risks: refusing
    `target_session_attrs=read-write` would refuse a string that works, and
    refusing a valid input is the one thing this project's review rules put
    first. The probe is fifteen lines and needs nothing new.

    It gets its own `DbError::WrongSession`, because it is the one connection
    failure that is not about *reaching* a server — `cannot reach {addr}` would
    be false, and the fix is a different server rather than an open port. That
    does not make ADR-0014's three into four: those three are how a socket can
    fail, and this is a server that answered.

    What the seam still drops is in issue #113: `keepalives`, `tcp_user_timeout`
    and `connect_timeout` are applied by the driver's own `connect_socket` and
    by nothing here. Left there rather than fixed with this one, because
    honouring them needs a new dependency and refusing them refuses strings that
    work — a choice, not a bug fix.
    work — a choice, not a bug fix.

232. **Opening the socket is one function, shared by both drivers, and it gives
    every resolved address a chance inside one budget.** `TcpStream::connect(host)`
    resolves the name and tries the addresses **in turn**, returning the last
    error, so a timeout wrapped around it bounds the *whole loop*: one address
    that drops packets spends the entire budget and a healthy second address is
    never tried. A dual-stack endpoint whose IPv6 address is black-holed is the
    ordinary case of that, and reporting a server that is up as unreachable
    refuses work — the failure this project's review rules put first.

    `open_socket` resolves first and tries each address itself. The budget is
    divided as it is spent — each attempt gets what is left over how many
    addresses are left — so the total is still `CONNECT_TIMEOUT` however many
    there are, an address that refuses at once hands its share to the rest, and
    the last one gets the remainder. Fixed shares would make a slow-but-
    answering server fail behind a dead one, and a full budget each would make
    `CONNECT_TIMEOUT` mean *N* times what it says.

    A refusal from some address outranks the clock. `Connect` names something
    with a fix the reader can act on, and only when no address answered at all
    is this the dropped-packets case that `ConnectTimeout` describes.

    In `pbps-db` itself rather than in either driver, because "there is a
    network" is this crate's (ARCHITECTURE) — and because the defect was on both
    sides of the seam. `pbps-db::mssql` had the same line, and fixing only the
    engine under review is how a shape becomes a second finding.

    The tests build the black hole out of a listening socket whose accept queue
    is full, and take the address list and the budget as parameters: resolution
    order is the operating system's, and a test that depends on it passes or
    fails by luck. What is *not* pinned by a failing test is the resolution half
    on its own — a hostname resolving to two addresses this test controls is not
    portable (`localhost` is one address on some machines and two on others), so
    the loop is pinned and the resolving is read.

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

234. **A TLS stack is built only when the connection may use one, and ALPN is
    offered only for direct SSL.** Two halves of the same mistake: doing TLS
    work that the connection string has already ruled out, and not doing the
    TLS work it asks for.

    Building the stack reads the host's certificate store, and `tls()` refuses
    when that cannot be read — deliberately, because a trust store that failed
    to load is not an empty one. Under `sslmode=disable` that refused a
    connection over certificates it was never going to look at, on exactly the
    minimal image SPEC §11.3's single static binary is for. `connect` now
    branches, and the disabled path hands the driver `NoTls`.

    The other half is ALPN. **Measured on PostgreSQL 18.6**: a direct SSL
    connection that offers no ALPN is refused — `received direct SSL connection
    request without ALPN protocol negotiation extension` in the server log —
    while the TLS handshake itself *completes*, so the failure lands after it
    and reads as the connection dropping rather than as a protocol requirement.
    Neither `tokio-postgres` nor `tokio-postgres-rustls` sets it, so
    `sslnegotiation=direct` could not connect at all. Offered only for `Direct`,
    because the `SSLRequest` negotiation the default uses asks for none and
    libpq offers none there either.

    Both are pinned by unit tests over the parsed config rather than by the live
    suite. The suite's server has TLS off, as CI's does, and giving it a
    certificate this client trusts is its own piece of work; what is measured
    here was measured with `openssl s_client` against the same image and is
    written down above rather than asserted.
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
236. **A unique index is gated and counted as the constraint it is, and a
    filtered one only over the rows its predicate keeps — or not at all.**
    SQL Server enforces a `UNIQUE` constraint *with* a unique index: the two
    are one object, and which YAML key the uniqueness was written under
    (`unique:` or `indexes: {…, unique: true}`) cannot decide whether the
    change faces the gate or is counted. It did: `AddIndex` had no risk class
    and no probe, so adding `unique: true` over a column holding duplicates
    planned as a no-risk change and failed at the engine mid-apply — where,
    under `--staged`, every earlier checkpoint has already committed.

    The filter is the part that is not obvious. A filtered index constrains
    only the rows its predicate keeps, so counting the others reports
    collisions the engine exempts and refuses a plan it would have accepted —
    the "invent a violation" direction, and the worse of the two. But the
    predicate is arbitrary SQL over the whole row, while the relation
    `rows_after` builds is a few projected columns unioned with rows that are
    not in the table yet. It can only be asked of the stored branch, and only
    where this plan leaves that branch alone: not where the plan writes a row
    (an update that touches no key column still moves a row in or out of the
    filtered set), not where it renames a column of the table (the text then
    names nothing, or — when a second column is renamed into that spelling —
    silently names the wrong one), not where it adds one (not there to be
    read), and not where it retypes one. That last is 152's trap on the other
    side of the query: there `UNION ALL` reconciled the branches by data-type
    precedence, here it is comparison precedence, and no conversion can help,
    because the predicate is the user's text over columns this projection
    never selects. Measured: `flag int` holding `1` twice under
    `WHERE [flag] = '01'` keeps both rows, so the probe counts a collision and
    refuses the plan; retype `flag` to `varchar` and the stored values read
    `'1'`, which the predicate excludes and the engine creates the index over
    nothing.

    Where any of those hold the probe disappears, which is the answer every
    other unspellable value gets here (165, 171). It disappears *silently*: a
    probe that is never built is not among the ones `apply` reports as
    unchecked, since that count is of probes that ran and could not answer.
    That is the standing behaviour of every `Ok(Vec::new())` in `preflight`
    and not a property of this change, so it is not fixed here — see issue
    #145. The gate is what carries the change either way: the risk class does
    not depend on whether a count could be taken, so the approval still asks a
    human. The alternative — counting unfiltered and
    calling it conservative — is not conservative at all in this direction; it
    refuses valid plans.
237. **The constraint and index drops run before the column renames, all of
    them.** `order_key` renamed a column first and dropped the constraints
    around it after. For most of them that is right and #123 had just stopped
    them being restated at all — measured, `sp_rename` carries a primary key,
    a unique constraint, an index's key and `INCLUDE` columns and a child's
    `references_table` for free. Two kinds it cannot carry, and there the
    order was backwards: measured on SQL Server 2025, a rename of a column a
    **check constraint** names is refused with 15336, and one a **filtered
    index's predicate** names with 5074, and 4922 behind it. The plan a valid
    revision produced —
    rename, drop the check, add it back with the new spelling — could not run,
    and no declaration the user could write fixed it. The escape was two
    revisions.

    **The whole group moves, not the two kinds that need it.** Moving only
    `DropCheck` and the filtered `DropIndex` means asking which columns a
    check's expression names, and this tool deliberately never parses that
    expression (174). Moving all four costs nothing instead: measured, a
    rename is never blocked by a constraint that does not name the column, so
    `DropUnique`, `DropForeignKey` and an unfiltered `DropIndex` are as safe
    one class earlier as one class later. They still precede `DropColumn`,
    which is the other thing their old position was for, and
    `DropForeignKey`'s `dependency_rank` of -1 travels with them and still
    puts it ahead of the key it references.

    **They move ahead of the column renames, not ahead of the table rename.**
    A drop names its table, so a drop emitted before `sp_rename` on the table
    would name a table that no longer exists — the shape #118 fixed. Measured,
    neither a check nor a filtered index blocks a *table* rename, so there is
    nothing to gain by going further.

    **`Revoke` could not travel with them.** It shared the drops' class and
    has the opposite need: it names the object as the renames leave it. So it
    stayed where it was and took a class of its own, which is the negative
    case the ordering tests pin.

    The re-add stays in the constraint class far below, after the renames,
    which is where the new spelling can be written. Only the drop moves.

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
246. **Two rename intents claiming one name are refused, before anything else
    is judged.** The three matching loops in `identity.rs` consume from
    `disappeared` and `appeared`, and nothing checked that two intents named
    one column, table or role. The first to be reached took it; the rest fell
    through.

    The source case was **silent**. Measured: with `renamed_from: old` on both
    `aaa` and `zzz`, `resolve` returned `Ok`, the column holding the data was
    renamed to `aaa`, and `zzz` was created as a new empty column beside it —
    so the data answered to a name the author did not choose and the name they
    expected held nothing. The second intent went unreported because its target
    had by then been minted into the ids file, which is what the absorbed check
    reads. Which of the two won was declaration order.

    The target case did reach `Err`, but as `UnusedIntent` — "matches nothing
    in either the declarations or the identity file, likely a typo" — about an
    intent whose every name exists. A message that sends the reader looking for
    a misspelling there is none of is worse than the count of blockers
    suggests.

    **One guard for all three kinds, not one per resolver.** The loops are the
    same code over three types, and a rule with three homes is three chances to
    be fixed once (`docs/PITFALLS.md`, "One rule, spelled in three places").
    The key carries the kind, and a column's carries its table, so a table and
    a role that share a name — or one column name in two tables — stay two
    claims rather than becoming one.

    **Only claims that could match here.** The guard sees the rename intents
    whose source is in `disappeared` and whose target is in `appeared`, and
    that filter is the whole difference between a conflict and a leftover. Its
    first form grouped every raw intent and was measured wrong on this: a
    `renamed_from` lives on until `pbps fmt` strips it — which is what
    `intent_is_absorbed` exists for — so an annotation recording a rename that
    already happened is *expected* to be in the file, and once its vacated
    source name has been reused by a new object that this revision renames, the
    two share a source and nothing else. The stale one cannot match, because
    its target is on both sides of the declarations. Grouping them refused a
    valid plan for an annotation nobody had got around to deleting, which is
    the failure this repo weighs heaviest.

    It runs before its scope's matching loop, raises what it finds, and the
    loop then runs anyway. Skipping it — the shape `AmbiguousColumns` uses —
    was the obvious move and the wrong one twice over: the loop's
    order-dependent decision goes nowhere in any case, because `resolve`
    discards the whole `Resolution` when it returns `Err`, while skipping it
    leaves every *other* intent of that kind unmatched, so an unrelated and
    perfectly matchable rename or drop beside the contest was reported as
    "matches nothing … likely a typo".

    **The contenders are excluded from that sweep by equality, not by
    bookkeeping.** Marking their indexes used was the first answer and it was
    defeated twice — once by the early return that stranded their neighbours,
    once by the repeat-collapsing that dropped an index before it was recorded.
    Three rounds, three ways for an index to go missing, one shape: an intent
    can be reported as contested *and* as a likely typo. The sweep now skips
    every intent a `ConflictingRenameIntents` names, so the two are mutually
    exclusive by construction and there is no fourth way. A property held by a
    check beats one held by four call sites remembering to record something.

    **Identical intents are not a conflict.** The same rename reaches `resolve`
    twice whenever a `renamed_from` annotation is also answered at the
    interactive prompt, and refusing that would refuse a valid plan for saying
    one true thing twice. Only distinct intents contending for a name are
    ambiguous.

    **And it offers no choice.** The prompt's candidates would be exactly the
    intents the user has already written down, so it would ask them to pick one
    of their own contradictory statements and then record it — turning a caught
    mistake into a committed one. The file is where the contradiction lives and
    where it has to be resolved.

    A chain (`a -> b` beside `b -> c`) claims no name twice on either side and
    is not this guard's business; it is already refused, because `b` is in both
    the declarations and the ids file and so is in neither `appeared` nor
    `disappeared`. A test pins that, so nobody widens the guard onto a case
    that is covered.
247. **A catalog row of a kind the reader does not know is reported, never
    folded into the nearest kind it does.** `pg_constraint.contype` is an open
    set at the engine's end, and PostgreSQL 18 proved it: every `NOT NULL` now
    has a constraint row of kind `n`. A reader that had parsed the characters it
    knew into an enum and let the rest fall through to "a check" would have
    started reporting one phantom check per `NOT NULL` column on an engine
    upgrade — and the differ would have planned to drop each one. So the kind
    travels as the engine's own character, the assembler matches the kinds it
    holds, and anything else becomes a named limitation carrying the
    constraint's own definition. The cost is a warning on a database using a
    feature pbps does not manage; the alternative is a plan against a phantom.

248. **A foreign key whose referential action the model cannot spell is left
    out and named, not read back as the nearest action it can.** PostgreSQL has
    `RESTRICT` and `ReferentialAction` does not. The two are close enough to
    tempt: `NO ACTION` and `RESTRICT` both refuse the delete. They are not the
    same — `NO ACTION` is checked at the end of the statement and can be
    deferred, `RESTRICT` fires immediately — so a pull that folded one into the
    other would let a plan replace a key's behaviour while reporting no change
    at all. The key is therefore absent from the pull and present in the
    warnings, which is the shape every other unexpressible fact takes here.

249. **A foreign key's referenced columns are read out of
    `pg_get_constraintdef`, not resolved with a second catalog join.**
    `confkey` holds attnums on the *referenced* table, which the constrained
    table's attnum map cannot answer for. Resolving them properly means another
    join, and it would put the answer inside the query file — where no test can
    reach it without a server. The definition already spells them and its shape
    is fixed, so the assembler parses it there, in the pure half, and refuses
    the parse when the column count disagrees with `confkey`. A misparse
    becomes a named limitation rather than a foreign key over the wrong
    columns.

250. **The whole catalog read is one `REPEATABLE READ READ ONLY` transaction,
    and the canonical search path is set inside it.** Five autocommit
    statements are five snapshots. A table dropped between the tables query and
    the columns query comes back as a live table with no columns — which
    assembles cleanly, compares as a table whose every column was deleted, and
    plans accordingly; nothing about it looks like a failure. One snapshot
    makes the five reads unable to disagree about what exists. `READ ONLY` is
    the engine enforcing what a comment would otherwise only promise (measured:
    `cannot execute CREATE TABLE in a read-only transaction`). And the path is
    set with `is_local`, so **ending** the transaction restores it — measured on
    `COMMIT` and on `ROLLBACK` alike. That is the difference between handling
    "a read failed halfway and left the session changed" and making it
    unrepresentable.

251. **A property of an object the model cannot hold means the object is left
    out; a fact about the rows already there means it is carried.** Both are
    named either way, and the line decides which way the resulting plan is
    wrong. `RESTRICT`, `DEFERRABLE`, a `gin` index: carried, each compares
    equal to an object that behaves differently, so a plan reports no change
    while the behaviour stays wrong — the failure this tool exists to prevent.
    Left out, the plan tries to create something that is already there and
    fails on apply, loudly, with the warning saying why. `NOT VALID` is the
    other kind: the constraint itself is exactly what the model says, and what
    recreating it changes is which rows get checked. Carried and named, that is
    a plan that may fail on apply rather than one that lies.

252. **A foreign key's referenced columns are resolved against the referenced
    table's own columns, which the pull already has. Supersedes 249.** That
    entry chose to parse them out of `pg_get_constraintdef`, on the grounds that
    `confkey` names attnums on the *other* table and a second catalog join would
    put the answer where no test can reach it. The parse is wrong on a legal
    name: `FOREIGN KEY (x, y) REFERENCES q(x, "a)b")` stops at the `)` inside
    the quoted identifier, produces two items, passes its own count check
    against `confkey`, and records the column `"a`. The count check was the
    guard, and it agreed with the wrong answer. What 249 missed is that the
    columns of every table in the pull are already in the assembler: no parse,
    no second query, and an attnum with nothing behind it is the same named
    limitation as everywhere else.

253. **A pull inside the caller's own transaction is refused, not
    accommodated.** PostgreSQL does not nest transactions: inside an open one a
    plain `BEGIN` is a warning, so the `COMMIT` that ends a successful read
    would commit whatever the caller had written, while the `REPEATABLE READ
    READ ONLY` snapshot 250 exists for was never established. A savepoint would
    give back the framing but not the meaning — a read inside somebody's
    transaction answers from their uncommitted writes, which is not what "what
    the database looks like" is. So the pull asks first and refuses.

    The asking is a `SET LOCAL` on a custom GUC, read back in a second
    statement: a local setting outlives its own statement only inside a
    transaction block. Every cheaper question was measured and reads the same in
    both states **through this driver** — `xact_start = query_start` and
    `transaction_timestamp() = statement_timestamp()` are both false even
    outside a transaction, because the extended query protocol opens the
    implicit transaction before the statement's own clock starts. Measured with
    `psql`, which speaks the simple protocol, both looked like reliable
    detectors.

254. **The pull's canonical scope pins how values print, not only how names
    do.** 250 set `search_path` empty so that a rendered name does not depend on
    the reader's session. The expressions this pull carries are carried verbatim
    (ADR-0013 §4), and the same argument applies to every setting the deparser
    consults: measured on 18.6, `quote_all_identifiers` turns `id > 0` into
    `"id" > 0`, `DateStyle` turns `'2020-01-02'::date` into `'02.01.2020'::date`,
    `TimeZone` moves a `timestamptz` default to another wall clock,
    `IntervalStyle` turns `'1 day 02:00:00'` into `'1 2:00:00'`, and
    `bytea_output` turns `'\x0102'` into `'\\001\\002'`. Two operators with
    different sessions would otherwise see drift on an unchanged database, and a
    plan would rebuild every constraint and index it touched.

    `extra_float_digits` is pinned on the same argument without a case that
    demonstrated it. `lc_monetary` is deliberately **not**: it belongs to the
    same class, and `SET` fails outright on a locale the server does not have,
    which would turn a readable database into an unreadable one for a difference
    nobody has yet shown.

255. **A name is round-tripped through the declaration format, not checked
    against a rule.** `TableName` is written `schema.name` and read back by
    splitting on every `.`; `ColumnRef` the same with three parts. PostgreSQL
    will hand out a schema called `"a.b"`, and then a pull that succeeded
    produces a schema whose own file does not load — or worse, one where
    `a.b` + `t` and `a` + `b.t` write to the same key. The pull performs the
    round trip on every table name and every column name it is about to record,
    and a name that does not survive takes its whole table out with a warning.

    Performing it rather than validating against a list of forbidden characters:
    the format is what decides, the format changes, and a rule written here
    would be a second opinion that can fall out of step with it. The whole table
    goes, not the offending column, because a table missing one column is a
    table a plan would add it to.

256. **The foreign keys are assembled in a second pass, after everything that
    could take their uniqueness away.** A foreign key is legal only against a
    unique index on the referenced table, and `conindid` says which one. That
    index may not reach the pull — its key constraint carries an `INCLUDE`
    payload, or is `NULLS NOT DISTINCT`, or is deferrable, or any of the other
    reasons 251 leaves an object out — and a key recorded against it describes a
    schema that cannot be built: adding the key back fails for want of a
    uniqueness nothing mentions.

    Whether it survived is not knowable from the constraint's own row, only from
    what the constraint and index arms did, so the arms report it: they return
    whether they recorded the object, and the foreign keys run afterwards
    against the set that did. Asking the arms rather than re-deriving the
    predicates, because a second copy of "which indexes this file refuses" is a
    second opinion that can fall out of step with the first.

    This is the third time in this file that a decision was reachable through a
    map built before the decision was made — 252's `confkey`, round 8's refused
    table, and this. The shape is in PITFALLS.

257. **The pull's own SQL carries no backslash escape.** 254 pins the settings
    that decide how the engine *prints* an answer. This is the other direction:
    `standard_conforming_strings` decides how the engine *reads* the query's own
    string literals, and with it off a backslash in an ordinary literal is
    consumed — measured, with a warning nothing here reads. `'pg\_%'` becomes
    the pattern `pg_%`, its `_` becomes a wildcard, and a project's schema
    called `pga` disappears from the pull, which is a plan that creates tables
    that are already there.

    The setting is now pinned in the canonical scope, and no query depends on
    that having worked: the schema filter is `left(nspname, 3) <> 'pg_'`, and a
    test asserts that no query contains a backslash at all. A filter with no
    escape in it cannot be read two ways, which is worth more than a filter that
    is correct as long as a `SET` succeeded.

258. **The declaration round trip asks for the same value, not for a value.**
    255 made the pull perform the round trip rather than reason about it, and
    the first version of it for a column's type asked the wrong question: does
    the spelling parse. Measured, `bit(3)` is a legal type this catalogue does
    not hold, so it is stored opaque — the base `bit(3)` with no arguments — and
    it writes out as `bit(3)` and parses back as the base `bit` with the
    argument `3`. That parses, and it is a different type: a schema written and
    reloaded is not the schema that was pulled, and wherever equality falls back
    to the raw spelling it is a difference no plan can act on.

    The check is now `render, parse, compare equal`, and it is asked of the
    value the column will actually be recorded with — one function decides that
    value for both the guard and the construction, because a check on something
    *like* what is stored is a check on nothing.

259. **The write `search_path` is set per statement, in the statement's own
    batch, and given back in the same one.** ADR-0013 §3 decides the value —
    the object's own schema first, then the project's configured extras — and
    leaves how it is carried to the emitter. Three spellings were available and
    two of them fail somewhere.

    `SET LOCAL` is the precise one inside a transaction and a **no-op with a
    warning outside one**, and `bootstrap --sql` renders a script a human runs
    through `psql`, statement by statement, in no transaction at all. A scope
    that quietly does nothing on the disaster-recovery path is the failure the
    scope exists to prevent. A session-level `SET` in a *preceding* statement
    survives that, and does not survive a staged apply resuming on a new
    connection — the path would be missing for exactly the statement that
    needed it.

    So the `SET`, the statement and the `RESET` are one batch. Measured, that
    works: `SET LOCAL search_path = bt, btx; CREATE TABLE bt.t (… CHECK (f(id) >
    0))` binds `f` through the new path, because a simple query is *analysed*
    one statement at a time. Inside a transaction the `RESET` is rolled back
    with everything else; outside one it hands the connection back as the
    operator's environment left it.

260. **The two settings that decide how a definition *parses* are pinned by the
    transaction framing, not by the statement.** They cannot be pinned by the
    statement. Measured on 18.6, a multi-statement simple query is **lexed as a
    whole before any of it runs**:

    ```text
    one batch:  SET LOCAL standard_conforming_strings = off; SELECT length('it\'s here');
                -> syntax error — the batch was lexed under the old value
    two:        SET standard_conforming_strings = off;  then  the same SELECT
                -> one literal, length 9
    ```

    so a `SET` in front of the statement it is meant to protect protects
    nothing. The pin has to be established on an earlier batch, and `begin` is
    the earlier batch a transactional apply runs — the same place SQL Server's
    `SET XACT_ABORT ON` lives (DECISIONS 194), for the same structural reason.

    **A staged apply opens no transaction, so `begin` does not cover it**, and
    an earlier version of this entry said "every connection" and was wrong. The
    pin belongs on the connection there, and it cannot be moved into the
    emitter's own statements: a staged run checkpoints after each one, and a
    `--resume` on a fresh connection starts at the next unexecuted statement,
    which is exactly the one whose pin was two statements back. No staged apply
    can reach this dialect yet — `main.rs`'s `dialect()` refuses it and
    `apply_staged_under_lock` calls `pbps_mssql::state` by name — so the
    connection-level pin lands with the PostgreSQL ledger and staged path
    (issue #83), which is the step that builds the connection it belongs on.

    Both are the ones ADR-0013 §3 names. `standard_conforming_strings = on` is
    what makes ADR-0011's scanner rule true: measured under `off`, `CHECK (label
    <> 'it\'s  here')` is **accepted** as one literal while the normalizer
    closes it at the escaped quote, so a whitespace edit inside that literal
    compares equal and is never planned; under `on` the same text is a syntax
    error. `check_function_bodies = on` is what makes ADR-0009's opaque-caller
    exemption true: measured, a SQL body naming a relation that does not exist
    is created silently under `off` and fails the first time it is called.

    The third exception ADR-0013 names, the write `search_path`, is per
    statement and does work in the statement's own batch (259) — name
    resolution happens per statement where lexing does not.

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

262. **`online` builds an index concurrently only when it has no filter.**
    Measured, `CREATE INDEX CONCURRENTLY` cannot share a batch with anything at
    all — `cannot run inside a transaction block` — so it cannot carry the write
    `search_path` of 259, and a path set by a preceding statement is not there
    after a staged apply resumes on a new connection. An index *with* a filter
    is therefore built the ordinary way and the hint is dropped, which is the
    trait's own rule for a hint a dialect cannot honour on this statement: the
    destination is the same either way, and refusing would turn a performance
    hint into an outage. An index *without* one has no expression to bind and
    needs no path, so nothing is lost by leaving the scope off it.

    The concurrent statement says both things about itself —
    `Statement::non_transactional` and `Statement::own_batch` — so a plan
    carrying one is refused at plan time with the whole plan intact, rather than
    halfway through an apply. That also makes `own_batch`'s own comment false
    where it said PostgreSQL has no batch restriction; it has exactly this one.

    A unique constraint gets no concurrent path either, and for a different
    reason: the online spelling is `CREATE UNIQUE INDEX CONCURRENTLY` followed
    by `ADD CONSTRAINT … USING INDEX`, whose halves commit separately. One
    declared constraint arriving as two committed steps is a state the gate
    never approved.

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

264. **The `DO` block's dollar-quote tag is chosen against the body it wraps.**
    Dropping a primary key the declaration did not name means asking the catalog
    for its name, which means dynamic SQL, which on this engine means a `DO`
    block. PostgreSQL's lexer looks for a dollar-quote's closing tag
    **literally**, without regard for quotes inside it — so a table named
    `x$pbps$y`, which is a legal identifier `pull` would adopt, ends the block
    where its name appears and hands the rest of it to the server as top-level
    SQL. The tag is therefore the first of `$pbps$`, `$pbps1$`, … that the body
    does not contain, which makes the failure unrepresentable rather than
    checked for.

265. **The write path's extras live on the dialect value, and the `pbps.yml` key
    waits for a reader.** `Dialect::emit` takes a change and a strategy, and a
    strategy says how to get there and never where (ADR-0003), so the extras
    have to be state on `Postgres`. They are not yet configuration: the CLI
    refuses this dialect outright (`main.rs`'s `dialect()`), so a key in
    `pbps.yml` would be one nothing reads — which is worse than none, because a
    user who sets it would have every reason to believe it took effect. The key
    lands with the step that can read it.

266. **A nullable primary key column is refused on PostgreSQL too, and for the
    opposite reason.** SQL Server refuses the table at `CREATE`, so its rule
    (`validate.rs`) only moves the failure earlier. **Measured, this engine
    accepts it** and sets `NOT NULL` itself:

    ```text
    CREATE TABLE t (id integer, CONSTRAINT pk PRIMARY KEY (id));  accepted
    the column afterwards:                                        attnotnull = t
    ALTER TABLE t ALTER COLUMN id DROP NOT NULL;
        -> ERROR 42P16: column "id" is in a primary key
    ```

    So the declaration and the database disagree from the moment the table
    exists, the pull reads `nullable: false`, every plan proposes the
    `DROP NOT NULL` that would put the declaration back, and the engine refuses
    that one for ever. A declaration an engine silently rewrites is worse than
    one it rejects, and the rule is more necessary here than on the engine it
    came from.

    Written from the measurement rather than inherited: `pbps-pg` had no
    key-column checks at all, which is PITFALLS' "the second implementation did
    not inherit the first one's scar" with the scar in the wrong shape as well
    as missing. The rest of `pbps-mssql`'s `key_columns` — a key naming a column
    the table does not have, naming one twice, naming none, or naming one whose
    type cannot be part of a key — is missing here too and is issue #175: those
    four fail loudly at the server, which is late but not silent, and this one
    does not fail at all.

267. **Every setting that changes what a declared expression means is pinned in
    the transaction framing, not around each statement.** The three verbatim
    expressions the model carries — a column default, a check expression and an
    index filter (ADR-0013 §3) — are text the engine reads through an input
    function or a parser rule, and five settings decide what that text means.
    Two more decide what a *conversion* writes back, below.
    Measured on 18.6, the identical declaration created by two sessions:

    ```text
    CHECK (d >= '01/02/2026')       MDY -> '2026-01-02'   DMY -> '2026-02-01'
    CHECK (at >= '2026-01-02 00:00')
      on a timestamptz              UTC -> 00:00:00+00    New_York -> 05:00:00+00
    CHECK (i >= '-1 2:00:00')       postgres -> -1 days +02:00:00
                                    sql_standard -> -1 days -02:00:00
    CHECK (at >= '2026-01-15 12:00:00 CST')
                                    Default -> 18:00:00+00   Australia -> 02:30:00+00
    CHECK (x = NULL)                off -> (x = NULL::integer)   on -> (x IS NULL)
    ```

    A different day, a different instant, an interval with the opposite sign, a
    time fifteen and a half hours out, and a predicate that stopped being the
    one that was written. No error, no warning, and nothing afterwards can say
    which session decided it.

    **The list is a rule, not a set of temporal traps**, and the last two are
    what say so. `timezone_abbreviations` is a dictionary `TimeZone` does not
    cover, so pinning the zone does not pin the abbreviation; and
    `transform_null_equals` is not an input function at all but a *parser*
    rewrite, which changes the predicate rather than a value inside it. The
    rule is: **a setting that changes what the declared text means is pinned**.
    Both were found by review after the first three shipped, and a list closed
    against its rule would have taken a third round to find the fourth.

    **`bytea_output` and `extra_float_digits` are the seventh and eighth, and
    were left out on a measurement that was true of what it measured.** A
    declared expression stores the same constraint under `hex`/`1` and under
    `escape`/`0`, because nothing in `CHECK (b >= '\x0102')` runs a value
    through an *output* function. An `ALTER COLUMN … TYPE text` does:

    ```text
    bytea -> text             hex -> \x0102              escape -> \001\002
    double precision -> text  1 -> 0.12345678901234568   -3 -> 0.123456789012
    ```

    Same stored bytes, same approved statement, two different strings left in
    the table. The rule reaches this and the exclusion did not, because
    "output-only" was a statement about where the setting is *read* rather than
    about whether a plan's result depends on it. Pinning is the complete answer
    where refusing the conversion would be an enumeration: every cast to text
    goes through an output function, and the list of which ones consult a
    setting is exactly the list the pin makes irrelevant. The values are the
    read scope's, so what a plan writes is what the next `pull` reads back.

    `lc_monetary` is in the class and is deliberately out, for the reason
    `catalog.rs` gives on the read side: `SET` fails outright on a locale the
    server does not have, so pinning it would turn a database that deploys into
    one that cannot. Its reach is a `money` literal, and this dialect's type
    catalogue refuses `money`.

    They go in `begin` and not in the per-statement scope because they are
    *constants*: unlike `search_path`, which is the object's own schema and
    therefore varies per statement (DECISIONS 259), one value serves the whole
    plan. Nine `SET`s and nine `RESET`s around every line of `plan.sql` would
    bury the SQL a reviewer has to read (SPEC §14.1) to say the same thing
    once.

    `bytea_output` and `extra_float_digits` are named by ADR-0013 §3 and are
    deliberately **not** here. Measured, both are output-only — identical stored
    constraints under `hex`/`1` and under `escape`/`0` — so they belong to the
    read scope, which already sets them, and pinning them on the write side
    would suggest they decide something they do not.

    This is also the answer to a check constraint that a `Column::default`
    guard cannot reach (DECISIONS 261): a default is refused when its literal
    is bare because the column's type is known there, while a check names
    columns and carries no type, and no offline rule can tell `'01/02/2026'`
    inside one from a string that merely looks like a date. Pinning the reader
    is what makes the text mean one thing.

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

269. **A primary key that is only dropped is ordered with the constraint drops;
    one that is replaced is not.** `order_key` had every `SetPrimaryKey` in the
    addition class (13), below every column change, because one variant carries
    both directions. So a declaration that gives up a key and relaxes the
    column it held produced a plan whose first statement neither engine would
    perform:

    ```text
    ALTER TABLE t ALTER COLUMN id DROP NOT NULL;   -- 42P16 on PostgreSQL:
                                                   -- column "id" is in a primary key
    ALTER TABLE t DROP CONSTRAINT pk_t;            -- never reached
    ```

    **Measured on both engines**, which is what makes this the differ's problem
    and not a dialect's: SQL Server refuses the same shape with 5074, "the
    object 'pk_pkord' is dependent on column 'id'", and 4922 behind it. A
    valid, reviewed plan, refused.

    The drop now sits in class 2 with `DropIndex`, `DropUnique`,
    `DropForeignKey` and `DropCheck` — where a constraint drop belongs, and
    where it also lands ahead of `DropColumn` at 5, the other statement a
    standing key blocks. No new class and no renumbering: `dependency_rank`
    already keeps a foreign-key drop ahead of the key it references *inside*
    this class, which is the order the engine requires and the reason that rank
    was written (DECISIONS 237).

    **Conditioned on `to: None`, not on the variant**, because a key being
    replaced is no longer one change — see DECISIONS 270.

270. **A replaced primary key is planned as two changes, its drop and its
    add.** DECISIONS 269 put a key's drop with the constraint drops by keying
    the class on `to: None`, and left the replacement where it was. Review
    found the half that leaves open, and it is the same defect: a declaration
    turning `PRIMARY KEY (id)` into `PRIMARY KEY (other)` while relaxing `id`
    still ran `DROP NOT NULL` against a column `pk_t` held — `42P16` on
    PostgreSQL, 5074 with 4922 behind it on SQL Server.

    One change cannot be ordered correctly here, and no class can rescue it.
    The drop must precede every column change a standing key blocks; the add
    must follow every column its new shape may name, including one this same
    plan adds at class 8. Opposite ends, so: two changes.

    Nothing else moves. The model is unchanged, and both emitters already
    emitted the two statements independently — `if let Some(pk) = from` then
    `if let Some(pk) = to` — so the SQL is the SQL it was and only the
    positions change. `from: None` on the add half is accurate where it runs,
    because the drop half has already taken the key away.

    **The risk classes follow, and that is the intended consequence rather
    than a side effect.** A replacement used to answer `Constraint` alone;
    now its drop answers `Destructive` and its add `Constraint`. That is what
    the database does — the old key and its index are gone — and a gate that
    was told only about the constraint was told half of it. A policy that
    denies `Destructive` will now stop a key replacement, which is the
    conversation that should have been happening.

    The plan a reviewer reads changes shape with it: one line becomes two, in
    different places, each with its own risk. That is more to read and it is
    the truth about what runs; the alternative is one line that hides a drop
    among the additions.

271. **A column with a default and a new type is three phases, one rank each.**
    Both change kinds are ordering class 9, so the tiebreaker decided which ran
    first, and the tiebreaker is the change's `Debug` rendering — which puts
    `AlterColumnDefault` ahead of `AlterColumnType` by the alphabet and nothing
    else. Each end of that is refused, by a different engine, and both are
    measured:

    ```text
    the default first, PostgreSQL:
      ALTER TABLE t ALTER COLUMN n SET DEFAULT 'abc';
          -> ERROR: invalid input syntax for type integer: "abc"
    the type first, SQL Server:
      ALTER TABLE t ALTER COLUMN n bigint NULL;
          -> Msg 5074: the object 'df_dn2' is dependent on column 'n'
    ```

    So neither order works and the answer is three phases: **drop the default
    the old type gave meaning to, change the type, install the default written
    for the new one.** `dependency_rank` answers `-2` for
    `AlterColumnDefault { to: None }`, `-1` for `AlterColumnType` and `0` for
    `AlterColumnDefault { to: Some(_) }` — the same instrument, and for the same
    reason, as the foreign key's rank: the dependency is a layering rather than
    a graph, so a constant says it exactly. No new class and no renumbering.

    A *replaced* default is two changes when the column is retyped, because the
    type change has to run between the halves — the third instance of "one
    variant carrying both directions cannot be ordered by direction"
    (DECISIONS 270, PITFALLS). **Only** when the type moves: a default replaced
    on a column that keeps its type needs no drop, since `SET DEFAULT` replaces
    on PostgreSQL and the SQL Server emitter already drops and adds inside its
    one statement, and splitting it would put two lines at opposite ends of a
    plan where one says it better (SPEC §14.1).

    The nullability needs no rank of its own: `Change::AlterColumnType` carries
    both ends, so the differ never emits a nullability change beside a type
    change on one column — and measured, SQL Server accepts the nullability
    form of `ALTER COLUMN` with a default standing, so the dependency is the
    type's alone.

    **The second measurement arrived by breaking it.** The rank went in with
    only PostgreSQL's end measured, `AlterColumnType` at `-1`, and that reversed
    an order SQL Server had been relying on by accident: with the default's drop
    sorted first by the alphabet, a retyped column's constraint had always
    happened to be gone. Nothing in the suite covered it. The regression test is
    `a_retyped_column_gives_up_its_old_default_before_the_type_moves`, on the
    engine that refuses it.

    The tiebreaker's own comment said this would happen: "anything with a real
    order between them belongs in separate classes; this tiebreaker cannot
    express it." A rank is the third way, and it is the one that costs no
    renumbering.

    What this does not reach is a type change on a column whose default the
    plan does not touch: there is no `AlterColumnDefault` to order, and SQL
    Server refuses that statement too. It is that engine's emitter to fix —
    issue #180 — because emitting a drop-and-re-add for an unchanged default
    would put two lines in every plan that widens a defaulted column, on both
    engines, for one engine's constraint model.
272. **`CREATE TABLE` names its access method, and does so in the statement.**
    The reader accepts a table only when `relam` is heap (`catalog.rs`), which
    is deliberate — everything below the model, from column storage to the way
    a page is read, is heap's. An unqualified `CREATE TABLE` takes its method
    from `default_table_access_method`, so under a role whose setting names
    another installed method the table is created *successfully* and then reads
    back as an unsupported object: absent from the pulled schema, planned again
    as a `CREATE` the engine refuses for already existing, and the deployment
    cannot converge. The apply reported success and the recording says the
    table is as declared.

    **In the statement, not in the transaction framing** beside the session
    pins of DECISIONS 267, and the difference is the point: those settings
    cannot be said in the statement they affect — there is no way to spell
    `DateStyle` inside a check constraint — while this one can. A clause cannot
    be answered differently by a session, on any path, including the rendered
    `--sql` script an operator runs through `psql` with no framing around it
    (issue #174). Where both are available, the one that cannot be defeated is
    the one to write.

    `default_tablespace` is the same kind of setting and is deliberately left
    alone: the reader does not filter on it, nothing in the model speaks about
    where a table's storage lives, and a plan that neither declares nor records
    it has nothing to be wrong about. There is no equivalent for indexes —
    measured, `default_table_access_method` is the only such setting on 18.6,
    and an index's method comes from its own `USING` with `btree` as the
    grammar's default rather than a session's.

    **The divergence itself is not measured, and that is stated rather than
    hidden.** The pinned image ships exactly one table access method, so there
    is no second one to create a divergent table with. Both halves are
    measured — the setting exists and is validated against the installed
    methods, and `USING heap` fixes `relam` — and the reader's rule is code.

273. **A table declared in a schema the pull never reads is refused offline.**
    The reader skips `pg_catalog`, `information_schema` and every schema whose
    name begins with `pg_` (`catalog.rs`), so a table declared in one is
    created and then invisible: absent from the pulled schema, planned again as
    a `CREATE` the engine refuses for already existing. The rule is derived
    from the reader's own list rather than written beside it, which is why it
    lives in this dialect and not in the loader — the excluded set is this
    engine's.

    **`pg_temp` is why the rule is not only about visibility**, and it is what
    makes this the class DECISIONS 266 wrote an offline rule for rather than
    one to leave to the engine. Measured, `pg_temp` is the parser's alias for
    the session's temporary schema:

    ```text
    CREATE TABLE "pg_temp"."t" (id integer);       accepted
    the relation afterwards:  pg_temp_58.t, relpersistence = 't'
    ```

    A session-local table, under a name the declaration never wrote, gone when
    the connection closes. An engine that refuses by name can be left to refuse
    — that is the line #175 and #179 are answered on — and one that hands back
    something else cannot.

    The negative half is in the test and is the reason the check is `starts_with
    ("pg_")` and not a looser match: the reader compares the first three
    characters, so a project's schema called `pga` is a project's schema, and a
    validation that refused it would refuse a declaration the pull reads
    perfectly well (the same trap `catalog.rs` avoids by not writing the filter
    as a `LIKE` pattern, DECISIONS 254).

    **Amended: modules are refused by the same rule.** The module gate checked
    only that the names could be quoted. Measured, `CREATE FUNCTION
    information_schema.f()` is accepted and identified as
    `information_schema.f()`, and `CREATE VIEW pg_temp.v` leaves `pg_temp_4.v`
    with `relpersistence = 't'` — the same two failures one namespace over, and
    `validate_module` now says so offline. A trigger is keyed by the table it
    is on, so the table's schema is the one the rule reads. (`pg_catalog`
    itself the engine refuses — "system catalog modifications are currently
    disallowed" — and the gate refuses it a statement earlier, which is where
    the offline command exists to speak.)


274. **The two table names this tool owns are refused in every schema.** The
    reader hides `__pbps_state` and `__pbps_lock` wherever they appear
    (`catalog.rs`), so a declaration using one is created and then invisible:
    the pull reports it absent, the next plan creates it again, and the engine
    refuses that for already existing. The apply reported success and the
    recording says the table is as declared — the shape DECISIONS 273 refused a
    schema for, now for a name.

    **By name and never by prefix**, which is the reader's own hard-won
    narrowing; its comment records the bug, that `NOT LIKE '\_\_pbps\_%'` also hid
    a project's `app.__pbps_customers` and nothing refused *that* declaration
    either. So the validation is derived from the same list, `catalog::OURS`,
    and `the_filter_hides_exactly_the_names_the_validation_refuses` ties them
    together in both directions: a name the validation refuses that the filter
    does not hide is a false refusal, and a name the filter hides that the
    validation does not refuse is this bug again.

    SPEC §8.1 defines the two, and a step that adds a third adds it to `OURS`,
    where the filter and the validation both read it.

275. **`$user` is refused as a schema name and as a write-path extra.**
    ADR-0013 §3 scopes every write statement with `SET search_path = <the
    object's own schema>, <the project's extras>`, and one name cannot travel
    in that list. **Measured on 18.6**, with a schema literally called `$user`
    holding a function, under role `postgres` with a `postgres` schema beside
    it:

    ```text
    SET search_path = "$user";
    SELECT current_setting('search_path'), which();
        -> "$user" | the role schema
    ```

    The quotes survive into the setting and change nothing: the engine
    substitutes that entry for the current role's own schema. So a table
    declared in a schema of that name would be created — its statements name it
    in full — and then every unqualified name inside a check, a filter or a
    default would resolve through whatever the deploying role owns, binding a
    different object or none. That is the property
    `an_unqualified_name_in_a_declared_expression_binds_through_the_write_path`
    exists to hold, silently inverted.

    Refused rather than worked around, because there is no spelling that makes
    the entry literal — quoting is the obvious attempt and it is the one
    measured above. Refused from **both** ends, since the path has two sources:
    the table's own schema (`validate_table`) and the configured extras
    (`emit::write_path`), and an extra is not seen by any table's validation.

    The comparison is exact and case-sensitive because the engine's is: `$USER`
    and `$users` are ordinary schema names, and refusing them would refuse a
    declaration that works.

276. **`pg_catalog` is left out of the write path, so it is searched first.**
    ADR-0013 §3 says the write scope puts the object's own schema first, and
    **measured**, that is true only among the schemas the path names:

    ```text
    CREATE FUNCTION shad.lower(text) RETURNS text AS 'the project function';
    SET search_path = shad;               SELECT lower('X');  -> x
    SET search_path = shad, pg_catalog;   SELECT lower('X');  -> the project function
    ```

    PostgreSQL searches `pg_catalog` ahead of every listed schema whenever the
    path does not name it. So a project function or operator with the same
    signature as a built-in never wins inside a declared expression, and the
    ordering the ADR promised is not the whole ordering.

    **The obvious repair makes a worse hazard, and the emitter cannot defend
    against that one.** Naming `pg_catalog` last would let a project *type*
    shadow a built-in: a `CREATE DOMAIN app.text` in the table's own schema
    would change what every `c text` column in that schema means, silently, and
    the pull would then read the column back as a type the closed catalogue
    does not hold and report it unsupported. The emitter cannot write around it
    — `character varying`, `double precision` and `timestamp with time zone`
    have no schema-qualified spelling, so there is no `pg_catalog.` prefix to
    put on the names that matter. The type catalogue is a closed list of this
    engine's own names (ADR-0012 §1) and `text` has to keep meaning `text`.

    The hazard that is left has a remedy the user holds: an expression that
    means the project's `lower` can say `app.lower`. The one the repair would
    create has none. So the path stays as it is and the claim is corrected
    instead — in this file, in `emit.rs`'s module docs and in ADR-0013 §3,
    which all said "first" without saying first *among what*.

277. **`pg_catalog` is refused as a write-path extra, not dropped from the
    path.** 276 records why the emitter leaves it out; a caller may still put
    it in through `Postgres::with_write_path_extras`, and then the path names
    it and the engine stops searching it first. The same measurement runs the
    other way:

    ```text
    SET search_path = "shad";                CHECK (lower(c) = c) -> pg_catalog.lower
    SET search_path = "shad", "pg_catalog";  CHECK (lower(c) = c) -> shad.lower
    ```

    Both statements are accepted, neither says anything, and the constraint
    stored by the second calls a different function from the one the
    declaration reads as.

    Refused rather than silently dropped, for the reason an unquotable extra
    is refused where it is used: a path one entry short — or one entry
    different — binds a name somewhere the caller did not ask for and says
    nothing. Refusing is also the only answer that stays true if the
    entry ever *does* mean what it says: dropping it would be right today and
    wrong the day the reason changed. It joins `$user` (275) as the second
    entry a path cannot hold, and for the mirror reason: `$user` is a name the
    engine reads as something else, `pg_catalog` is a name the engine reads
    differently for having been written at all.

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
## Phase 5 — the ledger, the lock and `doctor` (step 8, #83)

284. **The PostgreSQL ledger lives in `public`, and the qualified names move
    out of `pbps-db` into the dialects.** SPEC §8.1 puts the two tables in
    `dbo`, which is a SQL Server schema, and `pbps_db::ledger` held
    `dbo.__pbps_state` as a constant — in the crate documented as holding no
    engine SQL. It now holds `STATE_TABLE_NAME` and `LOCK_TABLE_NAME`, the two
    words that are the same on every engine because they are this tool's own,
    and each dialect holds the qualified spelling beside the statements that
    use it. Each is asserted to be its own `LEDGER_SCHEMA` plus that name, so a
    schema edited in one place and not the other cannot leave two constants
    that each look right.

    `public` rather than a `pbps` schema of its own, decided on what it costs
    the deployment role: creating a schema needs `CREATE` **on the database**,
    which covers creating *any* schema, while creating two tables in `public`
    needs `CREATE` on that one schema. The narrower grant is the one a tool
    that argues against `db_owner` should be asking for. `public` is also the
    schema every database is created with, which is what `dbo` is on the other
    engine.

    Measured on 18.6, and it is why `doctor` asks about it: since PostgreSQL 15
    `public` is `{pg_database_owner=UC/pg_database_owner,=U/pg_database_owner}`
    — every role has `USAGE` and none has `CREATE` — so a fresh deployment role
    cannot create the ledger until somebody grants it.

    This is what #185 was waiting for: the pull's exclusion filter hides
    `__pbps_state` in *every* schema because it has no schema to name. It has
    one now, and qualifying the filter stays that issue's work with that
    issue's tests.

285. **The lock is a table on this engine too, not an advisory lock.**
    `pg_advisory_lock` is the obvious PostgreSQL answer and it answers a
    different question: it is held by a *session*, and measured, `pg_locks`
    shows nothing of a `pg_try_advisory_lock(42)` once the connection that took
    it has gone. SPEC §8.1's lock is the one thing here that must **survive**
    the pipeline that took it — a pipeline killed mid-apply is exactly when the
    next one must not start — which is also why `pbps unlock` exists as a
    command rather than as a timeout. A row in a table is what a dead process
    leaves behind.

286. **The lock is taken with `INSERT ... ON CONFLICT (id) DO NOTHING`, and
    zero rows affected is the refusal.** The SQL Server ledger inserts and, when
    the insert *fails*, reads the holder to name it. That shape cannot be
    ported: measured on 18.6, a failed statement aborts the whole transaction,
    so the `SELECT` that would name the holder comes back
    `25P02: current transaction is aborted, commands ignored until end of
    transaction block`. A lock taken inside a caller's transaction would report
    nothing and destroy the transaction on the way.

    `ON CONFLICT` is still a gate rather than a check-then-act: a second
    inserter blocks on the primary key's index until the first commits and then
    does nothing, so exactly one caller ever sees a row count of 1. If the first
    rolled back, the second gets the lock. The live suite pins both halves,
    including that the contending caller's transaction is still usable
    afterwards.

    The one case the read cannot answer is a holder that released between the
    insert and the read. That is reported as contention without a name rather
    than as a lock this call did not win, because claiming it would be a claim
    that is false.

287. **The ledger's times are defaulted from `clock_timestamp()` and read
    through `to_char`, never cast.** Two measurements, one per half.

    `now()` is the transaction's start time and does not move inside it —
    measured, two reads 300ms apart in one transaction return the same value —
    so a staged checkpoint and the entry that closes it would carry the same
    instant and read as simultaneous. `clock_timestamp()` is the statement's,
    which is what `SYSUTCDATETIME()` gives the other ledger.

    And the text is rendered rather than cast, because a cast is the reading
    session's business: measured, under `DateStyle = 'German, DMY'` the same
    value casts to `31.08.2026 09:14:22.517` — not sortable as text, not
    parseable as ISO 8601, and produced by an operator's own setting rather
    than by anything this tool did. `to_char(applied_at, 'YYYY-MM-DD"T"HH24:MI:SS.MS')`
    holds no locale-sensitive field (`TM` is what would make one), so it renders
    the same under every `DateStyle` and every `lc_time`. The column is
    `timestamp(3)` holding a UTC wall clock rather than a `timestamptz`, for the
    same reason one step further out: a `timestamptz` is rendered in whatever
    `TimeZone` the reading session has.

288. **`is_initialized` attempts a statement here too, and this engine answers
    the three cases apart in the SQLSTATE.** DECISIONS 219 had to find that out
    by measurement on SQL Server, whose catalog *hides* an object a login has no
    permission on: `OBJECT_ID` and `HAS_PERMS_BY_NAME` both answer as if the
    table were absent, so "not authorized to look" arrived at every caller as
    "there is no ledger". Measured on 18.6, PostgreSQL separates them itself:
    `42P01` for a relation that is not there, `42501` for one that is and may
    not be read — and `42501` again where the *schema* is closed, which is the
    same answer for the same reason.

    The lookup is still not used, and that is measured too: `to_regclass` does
    not return NULL for an object in a schema this role cannot enter, it raises
    `42501`. So the guard would have to handle an error anyway, and where it
    does not raise it is silent about the difference. An absent *schema* answers
    `42P01` like an absent table, which is the right answer to "is there a
    ledger": there is not, and `bootstrap` makes both absences visible.

289. **`doctor` asks about ownership on this engine, because no privilege
    authorizes DDL.** Measured on 18.6, as a role holding
    `GRANT ALL PRIVILEGES ON own.t`:

    ```text
    has_table_privilege('own.t', 'SELECT,INSERT,UPDATE,DELETE,REFERENCES,TRIGGER')  ->  t
    ALTER TABLE own.t ADD COLUMN c int   ->  42501: must be owner of table t
    CREATE INDEX ix_t ON own.t (v)       ->  42501: must be owner of table t
    DROP TABLE own.t                     ->  42501: must be owner of table t
    ```

    Every privilege the engine has to give, held, and not one statement a plan
    is made of could run. The SQL Server list ported across would have asked
    `has_table_privilege` about a vocabulary that is real here, got `true` for
    all of it, and reported an environment ready that cannot alter a single
    table — the under-demand `pbps_mssql::doctor::Needed` exists to remove,
    arriving through the front door. The question is
    `pg_has_role(current_user, relowner, 'USAGE')`, which is the one the engine
    asks itself, and the remedy is `ALTER TABLE ... OWNER TO` or
    `GRANT <owner> TO <deployer>` rather than a `GRANT` on the table.

    The scopes are this engine's for the same reason: a schema takes `USAGE` and
    `CREATE` and nothing else, there is no database-scoped `CREATE TABLE`, and
    `CREATE` on the database is deliberately not demanded — the emitter never
    writes `CREATE SCHEMA`, so an absent managed schema is reported as absent
    (as on the other engine) rather than covered by a grant that also permits
    creating any schema at all.

290. **A staged apply pins its session, through `Dialect::session_pins`.**
    `Postgres::transaction_framing`'s `begin` carries nine settings, because
    each of them decides what a declared expression *means* (DECISIONS 254 and
    the method's own comment). A staged apply opens no transaction, so `begin`
    is never sent — and `--resume` is worse, because it starts on a fresh
    connection partway through the plan. The same text is now available without
    the `BEGIN`, and the staged runner establishes it on the connection before
    its first statement.

    The trait's default is `None`, which is the right answer for SQL Server: its
    framing carries `SET XACT_ABORT ON`, which governs a transaction and means
    nothing outside one. One macro produces the text for both call sites here,
    so the transactional and staged paths cannot pin different things — the
    failure this replaces, where the staged path pinned nothing at all.

291. **A reason is cut by characters here and by UTF-16 units there.** The
    column is `varchar(1000)` and this engine counts characters: measured,
    `varchar(4)` accepts four emoji and reports `length = 4,
    octet_length = 16`, where `NVARCHAR(4)` refuses them. A shared helper would
    have to be wrong on one of the two engines, so each dialect has its own with
    its own unit and its own test.

    The engine has two answers about overflow and only one of them is loud:
    measured, `'😀😀😀😀😀'::varchar(4)` truncates silently and returns four,
    while inserting the same value into a `varchar(4)` column is
    `22001: value too long`. The user-supplied `--reason` therefore reaches the
    column as it was written and fails loudly; only the best-effort audit paths
    truncate, where a failed attempt that cannot be recorded is the opposite of
    what the row exists for.

292. **`ensure_tables` treats a concurrent creator's failure as success.**
    `CREATE TABLE IF NOT EXISTS` is not atomic against another session doing the
    same thing: the check and the create are two steps, and the loser gets
    `23505` on `pg_type_typname_nsp_index` — the row for the table's implicit
    composite type is where the collision lands — or `42P07`. Both mean the table is
    there now, which is what the caller asked for; anything else is still a
    failure. Every command that writes a ledger row calls this, so two
    pipelines starting together really do race on it.

293. **The ledger's DDL is not sent when there is nothing to create**, and that
    is a permission decision rather than an optimization. Measured on 18.6: a
    role holding `SELECT`, `INSERT` and `DELETE` on both ledger tables and no
    `CREATE` on their schema gets `42501: permission denied for schema public`
    from `CREATE TABLE IF NOT EXISTS public.__pbps_state` — **even though the
    table is already there.** The engine checks the schema privilege before it
    notices the relation exists.

    That is exactly the least-privilege configuration SPEC §8.1 asks for and
    `pbps_pg::doctor` reports as ready, once 289's create-time requirement has
    been spent. Every `record` and every `lock` calls `ensure_tables`, so the
    whole deployment failed on a grant `doctor` had correctly said was no longer
    needed.

    So `ensure_tables` asks the catalog first, and this is **not** the question
    219/288 refuses to ask there. That one is "does *this caller* have a ledger
    to read", which only a statement can answer without turning "not authorized
    to look" into "nothing there". This one is "would `CREATE TABLE IF NOT
    EXISTS` do anything", which is about the database and not about the caller —
    so it is asked as a join on `pg_class` and `pg_namespace`, which are
    world-readable, and asked in the statement's own terms: *any* relation of
    that name, whatever its kind, because that is what `IF NOT EXISTS` looks
    for. A narrower predicate would send DDL the engine is about to skip, which
    is the failure this removes.

    The race is unchanged and still tolerated (292): between the probe and the
    `CREATE`, another session may create the table, and `23505`/`42P07` still
    mean it is there now.

294. **`doctor` asks for `USAGE` on a schema wherever objects in it are used,
    not only where they are created.** `Needed::LedgerCreation` asked for
    `CREATE` on the ledger's schema while the ledger did not exist, and
    `Needed::Ledger` asked for the DML on the two tables once it did — and
    between them nobody asked whether the role could enter the schema at all.

    Measured on 18.6, with `USAGE` on `public` revoked from a role holding
    `SELECT`, `INSERT` and `DELETE` on both tables: `has_table_privilege`
    answers **`t`** — the question is asked by oid and never resolves the name —
    while every statement naming the ledger is `42501: permission denied for
    schema public`. This is 289's shape a second time, and from the same
    direction: a privilege question with a true answer, about something the
    engine decides elsewhere.

    Swept for, as CLAUDE.md asks, and found again one securable out: a foreign
    key into `shared.parent` needs `USAGE` on `shared`, and nothing asked about
    the *managed* schemas can see that. `Needed::ReferencedSchema` is that
    entry. The same sweep found the plain omission beside it — `Needed::
    Referenced` asked for `REFERENCES` and not for the `SELECT` the probe for
    that key performs, which the SQL Server list has carried since it was
    written.

295. **A referenced foreign-key target may be a partitioned table; a managed
    table may not.** `doctor`'s table question filtered `relkind = 'r'` for
    every list it asks about, taking the rule from the pull — where it is right,
    because an index and a sequence are rows in `pg_class` too and a partitioned
    table is one this model cannot hold at all.

    A target of somebody else's is not this project's table, so what the model
    can hold says nothing about it. Measured on 18.6:

    ```text
    shared.parent PARTITION BY RANGE (id)             ->  relkind = 'p'
    CREATE TABLE app.child (..., pid integer REFERENCES shared.parent(id))
      as a role with no grant on the parent            ->  42501: permission denied for table parent
      once REFERENCES is granted on it                 ->  CREATE TABLE
    ```

    Read at `r` alone, the target came back *absent* — and an absent securable
    is asked for nothing, by design (there is nothing to grant on) — so the
    check demanded neither the `REFERENCES` the key needs nor the `SELECT` its
    probe performs, and reported an environment ready that the very next
    statement refuses. The kinds are the caller's now: `r` for what this project
    manages and for the ledger, `r` and `p` for what a declared key points at,
    which are the two kinds this engine lets a key reference.

296. **Tolerating the creation race is not enough inside a transaction, so the
    `CREATE` runs under a savepoint.** 292 tolerates the loser's `23505`/`42P07`
    because the table is there now, which is what the caller asked for. Measured
    on 18.6, that is only true in autocommit: the error **aborts the loser's
    transaction**, so a bare `Ok(())` hands back a connection whose every next
    statement is `25P02: current transaction is aborted`.

    ```text
    A: BEGIN; CREATE TABLE IF NOT EXISTS public.__pbps_state (...);   -- holds the lock
    B: BEGIN; CREATE TABLE IF NOT EXISTS public.__pbps_state (...);   -- blocks
    A: COMMIT;
    B:   -> 23505 duplicate key ... pg_type_typname_nsp_index
       SELECT 1  -> 25P02: current transaction is aborted
       COMMIT    -> ROLLBACK
    ```

    That is not a hypothetical caller: `record` runs inside the apply's own
    transaction (147), so a deployment would have failed on a race this arm
    claims to have handled — and failed two statements later, with an error
    about a transaction rather than about the race.

    The savepoint is taken by **trying** it: `SAVEPOINT` outside a transaction
    block is `25P01` and harms nothing, so one round trip both establishes the
    savepoint and tells this call whether it is in a transaction at all. It is
    taken only on the path that sends DDL, which 293 has already made rare. The
    untolerated failures roll back to it too, so a caller can still run the
    diagnostics it wants to print — without that, even those come back `25P02`.

    This is the third instance of one shape in this dialect: an error path
    written for an engine where a failed statement costs a statement, on an
    engine where it costs the transaction. The other two are the lock (286) and
    the reason it does not read its holder after a failed insert.

297. **Every answer this ledger gives by tolerating an error is taken under a
    savepoint, and the tolerated `42P07` is verified rather than believed.** 296
    fixed one arm; a sweep of the file — which is what PITFALLS' own entry says
    to do about this shape — found three more, and one of them was tolerating
    the wrong thing.

    **The savepoint, everywhere.** `is_initialized`'s answer *is* an error
    (`42P01`, there is no ledger), and so are `unlock`'s and `lock_holder`'s
    against a missing lock table. Measured, each aborts the transaction it
    happens in, so each returned `Ok(...)` on a connection whose next statement
    is `25P02` and whose `COMMIT` is a `ROLLBACK`. `latest`, `history`,
    `timeline` and `prune` all reach `is_initialized`, so one guard covers the
    readers. It is a struct (`Recoverable`) rather than four copies of the same
    three lines, because the next arm someone adds should have somewhere to
    reach for.

    **And the collision is checked, not inferred.** `42P07` names *a* relation
    the DDL would create, not the ledger. Measured, an unrelated
    `public.pk___pbps_state` — the name this DDL gives the ledger's primary key,
    and an ordinary name for something else to have — makes
    `CREATE TABLE IF NOT EXISTS public.__pbps_state` fail with `42P07` while the
    ledger is **still absent**. Read as "somebody else created it", that is
    `ensure_tables` reporting success over a database with no ledger, and the
    next `record` failing with `42P01` about a table this call said it had made
    sure of. So the answer comes from the catalog — both tables there, or the
    engine's own error, which names the squatter.

    The cost is one round trip per call that can tolerate something: the
    `SAVEPOINT` attempt, which outside a transaction is `25P01` and does
    nothing. That is the same round trip a separate "am I in a transaction"
    probe would cost, and it answers both questions.

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

## Phase 5 — modules (step 5, #80)

301. **A routine argument type is its own text type, not a `ColumnType`.**
    `RoutineId` held `Vec<ColumnType>`, which was right while the only dialect
    was SQL Server, where a parameter's type is a column's type. PostgreSQL
    identifies a routine by the types in `proargtypes`, printed through
    `format_type` — the same text `oid::regprocedure` writes — and that text is
    a wider language than a column type is.

    Measured on 18.6, from the identity of two functions declared with ordinary
    parameters:

    ```text
    id.a  ->  id.a(character varying,"char",integer,numeric,
                   timestamp with time zone,integer[],text[])
    id.d  ->  id.d(id.pos,time without time zone,interval,
                   bit varying,character)
    ```

    Of those twelve, three parse as a `ColumnType`. The rest are arrays, a
    quoted name (`"char"` is a real type and is *not* `character`), a
    schema-qualified domain, and spellings a column type does not model. Making
    them `ColumnType` would mean either widening `ColumnType` with things no
    column declaration may hold, or refusing routines this tool must be able to
    read back.

    **The identity is text, and the text is compared.** `RoutineArg` is a
    validated string: it must be writable into an identity string and readable
    back out of one, so it refuses an empty argument, a top-level comma, and
    unbalanced `()`, `[]` or `"`. It canonicalizes only what every engine
    agrees on — outside double quotes, case folds down and the whitespace
    beside `(`, `)`, `[`, `]` and `,` is dropped; inside them nothing is
    touched, because `"char"` and `"CHAR"` are two types. So `INT` and `int`
    and `decimal(10, 2)` and `decimal(10,2)` are one key, and the canonical
    spelling is the one the identity string carries.

    **`normalize_routine_arg` stays the dialect's.** A dialect that does model
    its parameters as column types parses the text, normalizes it as a column
    type, and prints it back; text that does not parse as one is left alone.
    That keeps SQL Server's `INT` -> `int` folding (72) and lets PostgreSQL
    answer with the catalog's own spelling.

    Not a `ColumnType` extended with a "raw" variant: that variant would be
    reachable from a column declaration, where none of these spellings is
    valid, and the loader would have to refuse it there. A type that cannot
    hold the bad value beats a branch that checks for it.

    Closes the question 59 left open.

    **Amended: the dot is punctuation too.** Measured, the engine accepts a
    space around a qualified type's dot and never writes one back —
    `CREATE FUNCTION md.spaced(a md . my_type)` reads back as
    `md.spaced(md.my_type)`. Left unfolded, the declared key and the catalog
    key are two keys for one routine, and every plan drops it and creates it
    again: the cry-wolf loop ADR-0002 names as the failure to avoid. The fold
    now drops the whitespace beside `.` as well, outside quotes only, so a
    quoted name keeps whatever it holds.

    **Amended: the standard's array spelling is peeled too.** Measured,
    `text ARRAY`, `text ARRAY[4]`, `int ARRAY [2]` and `character varying
    array` are identified as `text[]`, `text[]`, `integer[]` and `character
    varying[]`, while `text ARRAY[]` and `text[] ARRAY` are syntax errors. So
    the word is peeled once, after the brackets and never before them. Left
    in, a declared `text ARRAY` was keyed as a routine the catalog spells
    `text[]`, and the routine the `CREATE` had just made was not found under
    its own key — a valid plan refused.

    **Amended: a Unicode-escaped identifier is the plain quoted name it
    spells.** The engine accepts `U&"…"` wherever an identifier goes, a
    routine's argument type included — measured, `r12.a(v r12.U&"\006doney")`
    has the identity `r12.a(r12.money)`, and with `UESCAPE '!'` the escape is
    the one given. `RoutineArg` decodes the form the way the engine does
    (`\XXXX`, `\+XXXXXX`, a doubled escape, a surrogate pair) and canonicalizes
    it to `"…"`, so one spelling of a name is one key; a form that does not
    decode is refused as text that is not one argument, which is what the
    engine says of it too. The decoder lives in the model, and the emitter's
    trigger scan (302) reads it from there. The escape character may itself
    be the punctuation an identity is split on — measured, `UESCAPE ','` and
    `UESCAPE ')'` are accepted — so the identity's own split steps over the
    clause as the three characters it is. The quoted form is then spelled
    the way the engine spells a quoted name in an identity: bare where its
    `quote_identifier` leaves it bare — `[a-z_][a-z0-9_]*` and not a keyword
    the grammar reserves in some position — and quoted everywhere else.
    Measured, `zq."my_type"` and `zq."zone"` are `zq.my_type` and `zq.zone`,
    while `zq."select"`, `zq."Order"` and `zq."möney"` keep their quotes; the
    keyword table is read from `pg_get_keywords()`, not from memory. The
    same rule runs the other way for an unquoted name: measured, `s.Ätype`
    and `r8.a\u{a0}b` declared bare are identified as `s."Ätype"` and
    `r8."a\u{a0}b"`, and the key kept bare was one `module_oid` compared
    against `format_type` and never matched — so the routine a plan had just
    created was not in the catalog to the next.

    **Amended: the parameter scan steps over the clause too.** A parameter's
    mode may follow its name, and a Unicode-escaped name carries its
    `UESCAPE 'x'` after it: measured, `CREATE FUNCTION dq.f(U&"n!0061me"
    UESCAPE '!' OUT integer)` has the identity `dq.f()`, and with `INOUT` or
    a bare type after the clause, `dq.g(integer)` and `dq.f(integer)`. The
    scan looked for the mode where the clause was, counted the parameter,
    and refused a correctly keyed routine.

302. **A PostgreSQL trigger's table is in its identity *and* in its
    definition, and a declaration where the two disagree is refused.**
    ADR-0002 fixed where a module's `definition:` begins by what the emitter
    can derive, and for a trigger that was `CREATE OR ALTER TRIGGER <name> ON
    <table>` — T-SQL's grammar. PostgreSQL's is not the same shape:

    ```text
    CREATE TRIGGER audit AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION …
    ```

    The table comes **after** the event list, which is text only the
    declaration holds. Splitting the prefix there would mean finding the end of
    the event list, and that is parsing SQL (§8.2). So the emitted prefix is
    `CREATE TRIGGER <name>` and the table appears twice.

    Measured, the duplication is not caught by anything else: the engine
    accepts `CREATE TRIGGER audit AFTER INSERT ON app.other` under the key
    `app.t.audit` without a word, and the mismatch surfaces a plan later as
    `DROP TRIGGER audit ON app.t` finding nothing. So `validate_module` refuses
    a trigger whose definition does not name the table its identity does, by
    ADR-0002's own best-effort identifier scan.

    **The scan is used in the direction where a miss is loud.** A declaration
    that does name its table in a spelling the scan cannot see is refused here,
    and the fix is to qualify the name; the alternative — accepting and
    discovering it later — creates the trigger somewhere else and leaves the
    key pointing at nothing.

    Also measured, and the reason the trigger's own name is emitted bare:

    ```text
    CREATE TRIGGER m1.audit AFTER INSERT ON m1.t …   syntax error at or near "."
    ```

    A trigger name is not schema-qualified on this engine, which is the same
    fact ADR-0009 §1 records from the other side — the schema in
    `ModuleId::Trigger` is the table's, and there is nowhere else for it to
    come from.

    **Amended in review: the check reads the `ON` clause, not the whole text.**
    The first version asked ADR-0002's identifier scan whether the definition
    mentioned the table *anywhere*, and that answers yes to

    ```text
    AFTER UPDATE OF t ON app.other ...        under the identity app.t.audit
    ```

    because the column list mentions `t`. Measured, the engine accepts it
    without a word and creates the trigger on `app.other` — the exact silent
    mismatch the check exists to prevent, waved through by the check. What
    decides the outcome is the name after `ON`, so that is what is read: a
    lexical scan that steps over literals and comments with the emitter's own
    `skip_datum`, counts parentheses so an `ON` inside a `WHEN (…)` is not the
    clause, and takes the first bare `on` at depth zero. The grammar puts
    nothing else there, and `INSTEAD OF` is `OF`.

    A definition with no readable `ON` is refused rather than guessed at, which
    is the direction a scan may be wrong in: the remedy is to write the clause
    where the engine expects it.

    **Amended: the dot is a token of its own.** Measured, `ON app . orders`,
    and a comment or a line break on either side of the dot, all create the
    trigger on `app.orders`. The scan wanted the dot glued to the name, read
    `app` as the table, and refused a valid declaration; it now steps through
    the same gap on both sides of every dot — in the scan that finds the name
    and in the comparison that reads it, which are two readers of one slice.
    The gap after `ON` itself is one too: measured, `ON /* c */ app.t` creates
    the trigger on `app.t`, and the scan steps through it as well.

    **Amended: `U&"…"` is one identifier.** Measured, `ON U&"r11".U&"\0074"`,
    `u&"r11"."t"` and `U&"r11".U&"!0074" UESCAPE '!'` all create the trigger
    on `r11.t`, and `U & "r11"` with a space is a syntax error. The scan read
    `U` as the table and refused a valid declaration. A Unicode-escaped part
    is read whole now, with its own `UESCAPE` or the default, and decoded the
    way the engine decodes it — `\XXXX`, `\+XXXXXX`, a doubled escape, a
    surrogate pair. A part that does not decode is one the gate cannot be
    certain about, and it refuses only what it is certain about: the engine
    refuses that spelling by name, and the catalog assertion after the
    `CREATE` covers the rest.

303. **A routine argument this dialect's catalogue does not know is passed
    through, not refused.** `Postgres::normalize_type` refuses an unknown
    column type, and the obvious move was to answer the same way for a
    routine's arguments. It is wrong here for a reason that does not apply
    there: a column's type has to be one this tool can spell, and a routine's
    argument only has to be the text the engine's identity carries.

    Measured, of one function's twelve parameters the catalogue knows three by
    name, and the rest are a domain, an enum, `"char"`, arrays and spellings a
    column may not hold. Refusing them would refuse ADR-0009 §1's own example
    and make the routines of an ordinary database unreadable.

    The bargain is ADR-0009 §1's, stated there and taken here: *"the declared
    signature is what the emitter writes into `DROP FUNCTION` … and a mismatch
    produces a `CREATE` the engine refuses or an object the next plan reports
    as one to drop and one to add. Both are loud, both are inside the plan's
    transaction, and neither is silent."* The user writes what `pull` showed
    them, which is the engine's own text.

    What the fold does do is what every engine agrees on and ADR-0009 §1
    measured: a modifier is discarded (`varchar(10)` is `character varying`),
    an array collapses to one `[]` (`text[][]` is `text[]`), and everything
    else goes through the column catalogue's alias table so `int4` is
    `integer`. `float(24)` is why the modifier is not thrown away *before* the
    catalogue is asked: it is `real`, and `float` is `double precision`.

    **Amended in review: the modifier goes whether the catalogue knows the type
    or not.** "Returned unchanged" was written as one rule and turned out to be
    two. Discarding a modifier is what the engine does to *every* routine
    argument; consulting a closed catalogue of column types is a different
    question that only some arguments have an answer to. Tying the first to the
    second left a declared `bit varying(4)` — a perfectly ordinary parameter,
    absent from the column catalogue because ADR-0012 §1 has no need of it —
    permanently unequal to the `bit varying` the catalog reads back, so the
    routine was one to create and one to drop on **every** connected plan.

    That is not the bargain this entry struck. DECISIONS 303 accepts one loud
    mismatch that a user fixes by writing what `pull` showed them; it does not
    accept the cry-wolf loop ADR-0002 names as the failure to avoid. So the
    fold is three attempts — with the modifier, without it, and without it and
    unfolded — and only the *fold* is the catalogue's. Measured on 18.6, the
    identity of one function's five parameters:

    ```text
    bit varying(4)  bit(3)  interval hour to minute  interval second(3)  m8."odd(name)"
    bit varying     bit     interval                 interval            m8."odd(name)"
    ```

    Two more rules fell out of that measurement. `interval` is the one type
    this grammar follows with words rather than a parenthesis, and its field
    qualifier goes the way a modifier does. And a quoted name's own parentheses
    are part of the name — `m8."odd(name)"` keeps them — so the modifier is
    found outside quotes or not at all.

    **Amended: an alias the engine identifies as something else is not a
    spelling the catalogue does not know.** Passing `varbit` through as
    written keyed a routine the `CREATE` never made — measured, `varbit(4)`
    is identified as `bit varying` — and `module_oid` resolved nothing. The
    aliases the column catalogue does not carry (`varbit`, `bpchar`, `nchar`,
    the `national …` spellings, `char varying`) are folded to the identity
    `format_type` writes, each measured, in a table a test keeps disjoint
    from the column catalogue so that one rule is not spelled twice. A
    domain, an enum, a composite still pass through: the engine is still the
    normalizer for what this table does not know. And a built-in written with
    its schema is the built-in: measured, `pg_catalog.int4`, `PG_CATALOG.INT4`,
    `"pg_catalog".int4` and `pg_catalog."int4"` are all `integer`, so the
    qualifier is dropped before the name is folded. So is the catalog's own
    name for a built-in's array: measured, `_int4`, `_varbit` and
    `_numeric(10,2)` are `integer[]`, `bit varying[]` and `numeric[]`. Built-ins
    only — `ar._my_type` is `ar.my_type[]` but `ar._solo` is `ar._solo`, and
    which of the two a user's name is cannot be decided offline, so it passes
    through as written and the engine decides.

304. **A module whose deparsed statement this reader cannot cut is named and
    left out, never recorded with an empty body.** The declaration holds
    everything after the object's name, and PostgreSQL hands back the whole
    statement, so the pull has to cut it. Measured, the three shapes:

    ```text
    CREATE OR REPLACE FUNCTION m4."odd Name"(a integer)⏎ RETURNS integer …
    CREATE OR REPLACE PROCEDURE m4.p(a integer)⏎ LANGUAGE sql …
    CREATE TRIGGER "audit x" AFTER INSERT ON m4.t FOR EACH ROW …
    ```

    The name is **stepped over**, not searched for: looking for the first `(`
    finds the wrong one in `"f(x)"."g"`, and rebuilding the name to compare
    against would mean reproducing the deparser's own quoting rules, which is
    the deparser's job and not this reader's.

    Where the text is not that shape, the module is left out with a warning
    naming it. The alternative — an empty `definition` — is the failure mode
    this project keeps finding: absent, empty and unreadable are three
    different things, and an empty body is one the next plan writes back over a
    working object.

    The same round trip the tables are asked for applies to the identity: a
    view called `f(int)` reads back as a routine with an argument list, so a
    module id that does not survive `ModuleId::from_str(&id.to_string())` takes
    its object out of the pull rather than into a schema that will not load.

    **Amended: a trigger is held only where its relation is.** The trigger
    arm read every user trigger; the table reader does not hold every table.
    Measured, the engine allows a trigger on a partitioned table and on an
    `UNLOGGED` one, and read back without its relation the trigger was a
    module whose `on:` named a table the schema did not have — `check_names`
    refused the pull whole. The arm now selects by the table reader's own
    predicate (or a view the view reader holds — measured, a user's `INSTEAD
    OF` trigger on a view an extension owns is not extension-owned itself),
    the complement is named as a limitation beside the relation's own, and a
    test ties the three to one string.

305. **Extension-owned objects are left out of the pull silently, and that is
    not the "absent, empty and unreadable" failure.** `CREATE EXTENSION …
    SCHEMA app` puts an extension's functions and views in a project's schema.
    A reader without the `pg_depend deptype = 'e'` filter reports every one of
    them as an undeclared module, and the next plan offers to drop objects
    whose declaration lives in a `.sql` file the extension owns and this
    project does not have.

    Not reported as a limitation, unlike a materialized view: a limitation is
    something the *model* cannot hold, and these are somebody else's objects.
    `DROP EXTENSION` is how one goes away. Reporting them would put a line per
    extension object in front of every reader, which is how a report stops
    being read.

    **Amended: the limitation reader is a reader.** The filter was on the
    module queries and not on the unheld-module query, so an extension's
    materialized view or aggregate in a project schema came back as a
    limitation — which is worse than noise, because `managed_limitations`
    refuses every command for a limitation whose name is in the managed set. A
    rule the ordinary reader applies and the reader beside it does not is a
    rule with a hole in it, and the hole is on the path that refuses.

306. **On this dialect every carried attribute refuses the rebuild today,
    because there is no declared grant for one to come back from.**
    ADR-0009 §3 decides that a grant to a **declared** role survives a module
    replacement, by the machinery ADR-0005 built — and roles and grants are
    Phase 5 step 6. Until that lands, `pbps-pg` has no `Grant` to emit, so an
    object carrying anything at all is one this dialect cannot rebuild.

    The conservative direction is the only one available, and it is also the
    right one to start from: warning and proceeding would put "the application
    lost access" behind a line of output nobody reads at 3am. The step that
    adds grants narrows this to what the declarations still cannot reproduce;
    it does not remove it, because ADR-0010 §5 records that pbps cannot express
    "revoked from `PUBLIC`" and therefore must not take it away.

    What is enumerated is the catalog and not a list — the ADR's own rule,
    after three review rounds each found the same shape one attribute further
    out. Measured on this branch, and each a refusal: a grant in `relacl` or
    `proacl`; a revocation from `PUBLIC`, which is the *absence* of a row and
    so invisible to a check that compares rows; an owner other than the
    deploying account, which a `DROP` and `CREATE` silently transfers and which
    turns a `SECURITY DEFINER` routine into a privileged one; `reloptions`;
    a view column default in `pg_attrdef`; a trigger's `tgenabled`; and the
    grants a *new* object would arrive with from `pg_default_acl`, which no
    comparison against the old object can see.

    **Amended in review, twice, and both are the same sentence proving itself
    again.** A fifth attribute: a grant on one *column* lives in
    `pg_attribute.attacl`, and measured, `GRANT SELECT (a) ON v TO r` leaves
    `pg_class.relacl` **NULL** — so an object-level ACL check reports nothing
    carried, the rebuild goes ahead, and the column grant is gone. An object
    with an empty ACL is the easiest case to wave through, for the second time
    in this entry.

    And the §4 half had the same shape one catalog over: the dependent
    enumeration had an arm per catalog it had thought of, and measured, a
    function behind a cast has its reverse edge in `pg_cast` and one behind an
    operator in `pg_operator`. Neither had an arm, so the edge was dropped
    entirely, `dependents` reported the rebuild unblocked, and the emitted
    `DROP FUNCTION` failed at apply — the applyable-and-predictably-fails
    outcome SPEC §7.5 exists to prevent. There is now a fallback arm, and the
    class list the arms handle is one constant the fallback excludes, pinned to
    the arms by a test.

    **A list obeys "enumerate from the catalog, not from memory" only when it
    has a fallback.** Three enumerations in this design have now been written
    as the cases somebody thought of, and each was corrected by finding the next
    one. What ends that sequence is not a longer list.

    **Amended: a comment is a carried attribute, and the list is now closed by
    measurement rather than by memory.** `COMMENT ON` puts a row in
    `pg_description`; a `DROP` takes it and a `CREATE` does not bring it back —
    measured on all three kinds and on a view's column. `Module` has a
    `description`, but nothing writes it to the database (the `COMMENT ON`
    round trip is a decision of its own, and `SetColumnDeprecated` says so from
    the other side), so there is nothing to restore it from and it refuses like
    the rest.

    That was the second time this enumeration was short — a column ACL was the
    first — so the other half was measured too: `pg_get_functiondef` writes the
    volatility, `SECURITY DEFINER`, `LEAKPROOF`, `COST` and every `SET` clause,
    so a routine's settings come back with its body; a view's column cannot
    hold `attoptions` at all, since `ALTER VIEW … ALTER COLUMN … SET` is `not
    supported for views`; and a trigger or a rule attached to a view is a
    *dependent*, enumerated and refused on its own terms.

    **And that argument was still wrong.** The round after it found a security
    label, and the round after that would have found something else: a `DROP`
    takes every row the catalog keys by the object's *address*, and there is no
    amount of thinking that turns a remembered list of those into a complete
    one. So the list is now asked of the engine:

    ```sql
    SELECT c.relname FROM pg_class c
     WHERE c.relnamespace = 'pg_catalog'::regnamespace AND c.relkind = 'r'
       AND EXISTS (SELECT 1 FROM pg_attribute a
                    WHERE a.attrelid = c.oid AND a.attname = 'classoid')
       AND EXISTS (SELECT 1 FROM pg_attribute a
                    WHERE a.attrelid = c.oid AND a.attname = 'objoid');
    ```

    Five on 18.6 — `pg_description`, `pg_seclabel`, `pg_init_privs`, and the
    two `pg_sh*` ones — and the reader reads all five, including the two a
    module can never be in, because "a module cannot be there" is the shape of
    claim that has been wrong every time. A live test runs that query and
    compares it with the reader's list, so a sixth catalog in a later release
    fails a test instead of passing unnoticed.

    Beside them, one thing `pg_depend` keys the other way: `ALTER FUNCTION …
    DEPENDS ON EXTENSION` writes a `deptype = 'x'` row *from* the routine, and
    measured, `pg_get_functiondef` does not write the clause — so a rebuild
    creates a routine that outlives the extension it was tied to. Measured too,
    the grammar allows it on a routine and a trigger and not on a view; the
    query runs for all three anyway, for the reason above.

    **A list is closed by a test against the engine, not by an argument about
    what is on it.** Three arguments were made here and three were wrong.

    **Amended: a fallback covers a missing arm, not a leaky one.** The next
    case arrived inside a class the list already knew. A domain's check
    constraint is a `pg_constraint` row with `conrelid = 0` — measured, it
    names its domain through `contypid` — so the arm's own inner join to
    `pg_class` threw it away, and the fallback could not see it because
    `pg_constraint` is on the known list. `NOT tg.tgisinternal` did the same to
    a trigger the engine owns. **Every arm is now total over its class**: a row
    an arm cannot represent comes back with a sentence saying so, never as no
    row at all. A filter inside an arm turns "there is something here this
    project cannot put back" into "there is nothing there", and the second is
    what makes a plan applyable and predictably failing.

    **Amended again: an arm can be total and still name the wrong object.** A
    user rule on a view (`CREATE RULE ins AS ON INSERT TO app.v …`) has its
    `pg_depend` edges in `pg_rewrite`, whose `ev_class` is the view the rule
    is on — the view itself. The arm reported the view as its own dependent,
    and the walk discarded that as the root. Measured, `DROP VIEW` deletes the
    rule and `CREATE VIEW` does not restore it: the rebuild went ahead and the
    rule was silently gone. Only the engine's `_RETURN` rule *is* the view;
    any other now comes back as a dependent this model cannot put back, and
    refuses. The same silence as the filter's, reached another way — a row
    that was there, named as something the reader already held.

    **Amended: a view's row type is a reference to the view.** Measured, a
    routine that takes `v` or `v[]` as an argument, or returns `v`, depends on
    `type v` or `type v[]` with `deptype` `n`, and the type depends on the
    view with `i`; `DROP VIEW v` names all three routines. Filtering the
    internal edge is right — the type is not a dependent anybody drops — but
    never asking about the type as a *reference* left those routines unseen,
    and the walk called the rebuild unblocked. The reverse-edge predicate now
    names the view, its row type and the row type's array type, in one
    spelling shared by the dependents query and the argument query.

307. **The rebind test is a name and a path, not a position on it.**
    ADR-0013 §3 requires that a same-named object a plan introduces rebuilds
    the modules it could capture, in that same plan. The obvious
    implementation asks which candidate is *earlier* on the write path than the
    current binding, and it cannot be written: an overload in the same schema
    captures a call without anything moving, and what an unchanged declaration
    would bind to today cannot be computed without parsing it (§8.2 forbids) or
    creating it (planning must not).

    So the test is: this plan brings an object into a schema on that module's
    effective write path, and the module's text mentions that object's bare
    name. One rebuild, once. A declaration that qualified the name in full is
    rebuilt too — deliberately conservative, and the ADR says so.

    Measured, all three states, which is what makes "one plan late" a cost and
    not a phrase:

    ```text
    before anything arrives:                    caller() = 'shared'
    after the shadow arrives, with no rebuild:  caller() = 'shared'
    after the rebuild:                          caller() = 'app'
    ```

    The middle line is a whole plan cycle in which the environment means one
    thing and the declarations mean another, with nothing in the plan that
    created the shadow having said so.

    **Amended: a trigger arriving is not a shadow.** Nothing calls a trigger
    by name, so the test asks for the name a body would reference the
    arriving object by (`ModuleId::referenced_name`), which a trigger does not
    have. Asked for the object name instead, a trigger `app.orders.audit`
    rebuilt every caller of `audit()` for a binding that cannot move — and
    where such a caller has dependents, that rebuild is a refusal of a plan
    that was valid.

308. **A routine's parameter list is checked against its identity, and only
    where the disagreement is certain.** The emitter writes
    `CREATE FUNCTION <name>` and the declaration writes everything after the
    name (301, ADR-0009 §1), so the identity's argument types live in the key
    *and* in the body — the same split the trigger's `ON` clause has, and the
    same silent failure. Measured: `CREATE FUNCTION app.f\n(x text) …` under
    the key `app.f(integer)` is accepted without a word and creates
    `app.f(text)`. The key names an object that does not exist, and every later
    plan creates it again and drops nothing.

    So `validate_module` reads the list. Three facts from the engine make that
    a scan and not a parse:

    ```text
    CREATE FUNCTION me.noparens RETURNS int …   syntax error at or near "RETURNS"
    CREATE FUNCTION me.o(out int) …             identity  me.o()
    CREATE FUNCTION mf.a(a out int) …           identity  mf.a()
    ```

    The list is mandatory, `OUT` is the one mode that keeps a parameter out of
    `proargtypes`, and a mode may be written on either side of the name. With
    the mode off, a parameter is `type` or `name type` and the grammar offers
    nothing else, so exactly two readings are tried.

    **Only a certain disagreement refuses.** A count is always certain. A type
    is not: `format_type` under the empty read path always qualifies a user
    type (ADR-0013 §3), so a key reading `app.f(md.my_type)` over a body
    reading `(a my_type)` is one object whenever the write path reaches `md` —
    and this dialect cannot know whether it does. Refusing that would refuse a
    valid plan, which is the one direction this gate may not be wrong in. A
    spelling the dialect cannot parse counts as agreement for the same reason:
    a scan that cannot read a spelling has not learned that it is wrong.

    What stands behind the cases it cannot decide is the catalog assertion
    after the `CREATE` (ADR-0009 §3), which is keyed by the identity and fails
    inside the transaction. An offline gate that decides what it can and a
    connected assertion that decides the rest is the split; a gate that guessed
    would be neither.

    **Amended: the second reading is not offered for a spelling the catalogue
    knows.** Trying both readings unconditionally introduced an ambiguity of
    its own. `(double precision)` splits into a parameter named `double` of
    type `precision`, and with a user type of that name the qualification rule
    above then accepts the body under the key `f(app.precision)` — while the
    engine creates `f(double precision)`. Measured, with `mq.precision` in the
    database:

    ```text
    CREATE FUNCTION mq.b(double precision) …   ->  mq.b(double precision)
    ```

    The engine does not offer that reading, so neither may the gate: where the
    whole remainder is a spelling this catalogue knows, that is the type and
    there is no second reading. A gate is allowed to be undecided; it is not
    allowed to invent a reading the grammar does not have.

    **Amended: every gap in the parameter is a gap.** The first comment case
    was fixed at the front of the parameter and nowhere else. Measured,
    `(value /* note */ OUT integer)`, `(OUT /* note */ value integer)`,
    `(value OUT /* note */ integer)` and a line comment between the name and
    the mode all have the identity `()`, and `(IN /* note */ x /* note */
    int)` has `(integer)`. The scan now steps through the gap on every side of
    the mode, not only the first; a scan that knew comments were whitespace
    in one position and not the next was refusing a valid declaration for a
    count only it got wrong.

    **Amended: the scans' whitespace is ASCII.** The rule of 313, applied to
    the emitter's own scans: measured, `CREATE FUNCTION r10.f(a r10.x\u{a0}, b
    int)` has the identity `r10.f(r10."x\u{a0}",integer)`, the non-breaking
    space being the last byte of the type's name. `str::trim` at a parameter's
    boundary, before a default and after a gap cut that byte off and compared
    `r10.x` with a catalog that says `r10."x\u{a0}"`. Every trim in these
    scans is an ASCII one now.

    **Amended: a `$` after an identifier byte is part of the name.** `$`
    continues an identifier on this engine, and measured, `CREATE FUNCTION
    dq.f(foo$tag$ integer)` is accepted with the identity `dq.f(integer)`.
    The per-character scans asked the literal test from the `$` alone, read
    `$tag$` as the opener of a dollar-quoted literal nothing closed, and
    refused the routine. The literal test now knows the byte before it — the
    rule the dialect's normalizer already applied — in one helper every such
    scan goes through.

309. **A module the deparse could not find is the catalog moving, not a reader
    out of step with its query.** The pull reads the catalog in one
    `REPEATABLE READ READ ONLY` transaction so that it cannot report half of a
    change as a whole schema, and 267's guard turns the `XX000` a moved catalog
    raises into a retryable message. The module queries opened a second way for
    the catalog to move, and it does not raise. Measured:

    ```text
    pg_get_viewdef(999999, true)  ->  NULL
    pg_get_functiondef(999999)    ->  NULL
    ```

    A deparser resolves its oid through the syscache against a *fresh*
    snapshot, so an object dropped between the scan and the deparse comes back
    as a row with a name and no definition. Read through the ordinary
    `missing` helper that said "the query and this code have gone out of step",
    which is the one diagnosis that is certainly wrong — nothing is out of
    step, and a reader sent to look for a renamed column will not find one.
    **Absent, empty and unreadable are three different things**, and a vanished
    object is the third.

    So the modules read takes the definition as optional and turns `NULL` into
    the same "the catalog changed while it was being read" the `XX000` path
    gives. Not into a limitation and not into a skipped module (304): 304 is
    for a statement this reader cannot *cut*, which is a fact about the object
    and stays true on the next pull. This is a fact about the moment, and the
    answer to it is to read again.

    It was found by the live suite going red under its own parallelism, which
    is what that suite is for: every test builds a schema and drops it, so a
    pull is nearly always running across somebody's `DROP`. A defect that only
    appears when two things happen at once has no other way to be found.

310. **A pull and a rebuild can deadlock, and the answer is a sentence rather
    than a lock order.** Reading a module's definition means deparsing it, and
    `pg_get_viewdef` opens the view — so a pull holds `ACCESS SHARE` on every
    view in the database for as long as that query runs. A rebuild takes
    `ACCESS EXCLUSIVE` on the object it is about to replace (ADR-0009 §3).
    Neither can be reordered: the pull's order is the catalog's, and the
    rebuild's is one object. Measured, from the server log:

    ```text
    deadlock detected
    Process A: LOCK TABLE "app"."granted" IN ACCESS EXCLUSIVE MODE
    Process B: SELECT … pg_get_viewdef(c.oid, true) …
    ```

    The engine detects the cycle, picks a victim and rolls it back **whole** —
    there is no half-read schema and no half-applied plan, which is the only
    property that matters here. What was missing was the words: `40P01` reached
    an operator as `db error` and nothing else, on both paths.

    So both say it: the pull's guard (267) gains a `40P01` arm beside its
    `XX000` one, and `before_a_rebuild` wraps the lock it takes. Each says the
    same three things — it was a tie, nothing changed, run it again — and the
    rebuild's names the likely other side, because "a `pull` or a `status`
    opens every view in the database" is not something an operator can be
    expected to know.

    Not solved by taking a weaker lock: the lock is what makes the catalog
    enumeration before the `DROP` mean anything. Not solved by holding the
    deployment lock either — that serializes deployers, and a `pull` is not
    one.

    Found by the live suite, where fifty tests read the catalog while a handful
    lock objects. The suite retries, and says in the helper that the retry is
    its own concurrency rather than the product's.

311. **The drop order for dependents is a topological order, not a depth.**
    A breadth-first walk gives each dependent the depth of the *shortest* path
    to it, and two dependents at one depth come out in whatever order the
    catalog gave. Measured, that is wrong the moment a diamond appears:

    ```text
    a and b are both views over v, and b is also over a
        DROP VIEW mj.a  ->  cannot drop view mj.a because other objects
                            depend on it
                            DETAIL:  view mj.b depends on view mj.a
    ```

    which is the applyable-and-predictably-fails outcome SPEC §7.5 exists to
    prevent — the same one the depth walk was added to fix, one shape further
    out. So the walk records the *edges* and the order comes from them: a node
    is ready when everything that depends on it has already gone, ties broken
    by name so that a plan is the same plan twice.

    **A cycle is a case, not an impossibility.** `CREATE OR REPLACE` closes one
    between two `BEGIN ATOMIC` routines — measured, `pg_depend` then holds both
    directions and neither routine can be dropped first. There is no order, so
    each member is named as something the plan cannot put back rather than
    emitted in an order that fails. The same for a cycle that runs through the
    module being rebuilt: it is not one of its own dependents, and what the
    walk coming back to it really says is that no rebuild of it is possible
    without `CASCADE`, which SPEC 14.3 does not offer.

    A depth is the answer to "how far", and the question was "in what order".

312. **The transaction probe compares against a value it invented, not against
    a constant.** Both sides of it — the pull refusing a caller's transaction,
    the rebuild requiring one — are `set_config(…, is_local => true)` in one
    statement and `current_setting` in the next: inside a transaction the
    setting survives to be read, outside one the implicit transaction ends and
    it does not.

    Against the constant `'yes'` that read had a third outcome nobody asked
    for. A session carrying `SET pbps.in_a_transaction = 'yes'` answers `'yes'`
    on an autocommit connection, and the two sides fail in opposite directions:
    the rebuild believes its reads are serialized when `LOCK TABLE` has already
    been released at the end of its own statement, and the pull refuses a
    connection that has no transaction at all. The first is a guard still in
    the code and no longer guarding; the second is a valid plan refused.

    A value invented per call cannot be sitting in the session, so the read is
    equal only if *this* call's `set_config` survived — which is the question
    being asked. A type that cannot hold the bad value beats a branch that
    checks for it, and here the value is the type.

313. **A routine argument folds ASCII case only.** `RoutineArg` lower-cased
    with `char::to_lowercase`, which is Unicode's fold and not this engine's.
    Measured:

    ```text
    CREATE FUNCTION mn.f(a mn.Ätype) …   ->  mn.f(mn."Ätype")
    ```

    The engine left the byte alone and *quoted* the name rather than folding
    it. A Unicode fold turns the declared spelling into `ätype`, which names a
    type that does not exist — so the key points at nothing, `module_oid`
    resolves nothing, and the object is planned as absent. The emitters' own
    `unquoted` has always been `to_ascii_lowercase`; this is the same rule in
    the model, where the two were quietly disagreeing.

    **Amended: whitespace is ASCII too.** The same rule, one character class
    over. `char::is_whitespace` is Unicode's answer, and a non-breaking space
    is whitespace to it; to this engine every non-ASCII byte is an identifier
    character. Measured, `CREATE FUNCTION r8.f(v r8.a\u{a0}b)` is accepted with
    the identity `r8.f(r8."a\u{a0}b")`, and `r8.a b` with a plain space names
    no type at all — so the fold turned a valid key into one that resolved
    nothing. The fold trims and collapses ASCII whitespace only, and the
    whitelist admits any non-ASCII byte, which is `continues_ident`'s rule.
    The dialect's normalizer follows the same rule wherever it looks for a
    gap — before the array keyword, after `interval`, around a modifier — so
    `a\u{a0}array` is a type name and not `a[]`. And so does the emitter's
    trim of a module body: measured, `CREATE VIEW v AS SELECT 1 AS x\u{a0}`
    names the column `x\u{a0}`, and `str::trim` had taken the byte off the
    end of the body before the `CREATE`, so the view the plan made had a
    column the declaration does not name. And the identity's own test for an
    empty argument list: measured, a type may be named by one non-breaking
    space and `g(\u{a0})` is a routine of one argument, which a Unicode trim
    read as `g()`. And `$`, which `continues_ident` names and the whitelist
    did not: measured, `CREATE FUNCTION dl.h(a dl.money$type)` is accepted
    with the identity `dl.h(dl."money$type")`, and the argument was refused
    before it reached the engine.

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
