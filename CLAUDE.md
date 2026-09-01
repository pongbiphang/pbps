# CLAUDE.md

Guidance for Claude working in this repo.

## What this is

- `pbps` (PongBiphang Schema): declarative database schema version control.
  Users maintain YAML declaring the desired schema; the tool computes diffs,
  generates change scripts, and applies them behind a risk gate.
- Full design: **[docs/SPEC.md](docs/SPEC.md)** — read it before changing the
  data model or adding a kind of change. Decision records: `docs/ADR-*.md`.
- Differentiation vs Atlas/Skeema (both declarative): rename/drop intent is
  human-supplied and recorded in git; changes are risk-classified; saved plans
  are checksum-pinned; state and ledger live in the database itself.

## Development environment

- **Develop inside WSL** (Linux is the primary target). Project lives at
  `~/pbps`; do not move it to `/mnt/c`.
- Do not attempt `x86_64-pc-windows-gnu`: rustup's mingw lacks a GNU assembler,
  so `windows-sys` (pulled by `clap` and `miette`) fails; `gnullvm` needs an
  external llvm-mingw. Details in README.
- All three must pass before committing (a change touching only Markdown files
  is exempt):

```bash
cargo test --workspace
cargo clippy --workspace --all-targets    # must be warning-free
cargo fmt --all
```

## Architectural boundaries

```
pbps-model     Domain model. Dialect-agnostic, span-free, serializes to JSON:
               the ids file, the state snapshot, the saved plan, the drift report
pbps-config    Project configuration (pbps.yml): paths, environments, hooks
pbps-load      YAML -> model; the only crate that may depend on serde-saphyr
pbps-diff      model <-> ids comparison -> ChangeSet. Produces no SQL. Also owns
               the managed-set scope and observed identity
pbps-dialect   Dialect abstraction. Pure: types, validation, emit, preflight
               probes. Connection-bound work is free async fns in the dialect
               crate, not trait methods
pbps-mssql     SQL Server: type catalogue, validation, the T-SQL emitter (the
               only place a *change* becomes SQL), catalog introspection,
               the ledger/lock statements, rename impact
pbps-db        Connections (tiberius) plus transaction framing. Owns "there is
               a network"; ledger types and prune policy, no T-SQL
pbps-docs      Markdown / self-contained HTML / Mermaid ERD from the model.
               Pure: no dialect, no connection, no configuration
pbps-cli       clap, diagnostic output, the deployment commands, exec hooks
```

- Only `pbps-db` and the `pbps-mssql` modules that take a `Conn` (`catalog`,
  `state`, `impact`, `edition`) are async; the CLI `block_on`s them per command.
  Phase 5 adds `pbps-pg`.
- `spikes/` is workspace-`exclude`d: standalone evaluation crates, not product
  code.

## Inviolable constraints

Each of these was paid for — stop and think before breaking one.

1. **Two semantically identical `Schema`s must be `==`.** Diff and drift both
   build on `==`: no spans, no one-shot annotations like `renamed_from` in the
   model (`pbps-load` returns those separately); normalize type case before
   comparing.
2. **Containers hold names; elements do not.** `Table` / `Column` have no
   `name` — it is the parent map's key, so key/name disagreement is
   unwritable. Functions needing a name take `(name, table)`.
3. **SQL appears exactly once, in the dialect emitter.** `pbps-diff` produces a
   typed `ChangeSet`; risk classification, gating and impact analysis work on
   structured data, never on strings.
4. **Risk is data, not a method.** `Change::intrinsic_risks()` answers only
   what needs no dialect knowledge; type narrowing is computed by the differ
   (which holds a `Dialect`) and attached to `PlannedChange::risks`.
5. **Serialization is deterministic.** Collections are `BTreeMap` /
   `BTreeSet`; the ids file goes into git and shifting order manufactures
   phantom diffs. Sole exception: `Table::columns` is an `IndexMap`
   (declaration order matters for CREATE TABLE) but its equality ignores order.
6. **Only rename and drop need human intent.** Everything else is automatic.
   Never prompt non-interactively — fail with a copy-pastable command.
7. **Only data-bearing objects get identity.** Tables and columns have uids,
   tombstones and rename intent because a wrong guess destroys data. Modules
   (views, procedures, functions, triggers) carry none: they never enter the ids
   file, a rename is drop + add, and git is the audit trail (ADR-0002).
8. **Annotations travel beside the model, never inside it.** `strategy:` and
   `depends_on:` change *how* to get somewhere, not *where* — both are invisible
   in the database, so either one inside `Schema` breaks constraint 1. They come
   back from `pbps-load` as `Loaded.hints` and reach the differ as `Hints`.

## Product guardrails (SPEC 14.3)

Each of these refuses a path that is **shorter but bypasses the typed plan, the
checksum, a human's recorded intent, or the git audit trail**. They arrive as
reasonable-sounding requests; the reason they were refused is the part that is
expensive to reconstruct.

- **No `push`.** `plan` then `apply --plan` stays two steps. A shorter path would
  become the path everyone uses, and the reviewed one would die.
- **Rename suggestions, never rename decisions.** Similarity may order the
  candidates in the TTY prompt (SPEC 6.3, still unbuilt), one pair at a time.
  **No non-interactive flag may supply identity intent** — a confirmation that
  can be written once into a CI file has stopped being a confirmation.
- **`revert`, not rollback.** A historical state is exported and applied as a new
  forward plan through the ordinary gate. It restores structure, not data, and
  says so at the point of use. Never one step.
- **No policy SaaS.** Policies and reports are files or stdout, air-gapped. A
  policy outside git is a second gate nobody reviewed. This refuses a control
  plane that *holds the approval*, not a screen — an optional local UI is
  planned on ADR-0006's terms (renders the typed JSON, commits intent to git,
  stores nothing authoritative).
- **One source of truth for the declarations.** No ORM-model loaders: the second
  source wins every disagreement silently, and identity, drop reasons and
  `strategy:` have nowhere to live in a model class.
- **No plugin execution engine.** The test: does it need to run *between* "plan
  approved" and "statements executed"? Then no — it makes the checksum describe
  something other than what runs, and anything it changes outside the
  declarations becomes permanent drift. Before a plan or after an apply is
  already served by the exec hooks and CI.

## Format traps (all found the hard way)

- **`null` cannot be a YAML key** (it is the null literal); the field is
  `nullable`.
- **`no` / `yes` / `on` / `off` parse as booleans.** `pbps fmt` must quote
  boolean-ish, null-ish (`null` / `~`) and number-shaped string scalars.
- **LF everywhere in version control** (see `.gitattributes`); the tool writes
  files itself, and platform line endings would break "the tool owns the file
  format" on Windows.

## Writing conventions

- Code comments, documentation and commit messages in **English**; only the
  conversation with the user is in Chinese.
- Comments explain **why** — especially "why not the more obvious approach".
- Test names state the property, not the function
  (`column_order_does_not_affect_equality`, not `test_eq`).
- Every test module includes **negative cases**; this tool's failure mode is
  doing the wrong thing silently.
- Conventional commits; the body explains the trade-offs.

## Current status

**Phases 0-3.5 complete** for SQL Server. The test and clippy bar is in
"Development environment" above; counts change too often to record here.

First-run: `init` (`--env` / `--from` / `--url-env`), with staged validation
and pbps.yml installed last so a failed onboarding run leaves no partial project.

Offline: `plan` (`--check` / `--since` / `--base` / `--out` / `--sql` / `--dev`),
`validate`, `fmt` (`--check`), `rename`, `rename-table`, `drop`, `drop-table`,
`docs` (`--format` / `--out` / `--title`).

Connected (each takes `--db <connection string>` or `--env <name>`): `pull`,
`plan --db` (`--staged`), `apply` (`--plan` / `--allow` / `--staged` /
`--resume`), `verify` (`--format json`), `snapshot` (`--force`), `baseline`
(`--reason`), `bootstrap` (`--sql`), `state prune` (`--keep`), `unlock`,
`status` (`--format json`).

Decisions that changed from the original spec (SPEC is in sync):

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

Phase 2 additions worth knowing before touching them:

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

Phase 3 additions worth knowing before touching them:

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
25. **`verify` exits 2 on drift**, distinct from 1 for a tool failure: a
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

Phase 3.5 additions worth knowing before touching them:

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

**Not done in Phase 1**: the interactive prompt (third intent channel, TTY
only). CLI commands and YAML annotations both work; nothing is blocked.

**Live tests**: the SPEC §11.5 invariants plus the Phase 3 and 3.5 ones (the
ledger round-trip, the lock admitting one holder, a failed statement rolling the
whole plan back, the rename-impact queries, the probes counting real rows, a
staged checkpoint surviving `state_json`, a cross-schema rename stopping at the
name its statement declared, and the module round-trip through
`sys.sql_modules`) run against a real SQL Server in Docker:
`scripts/live-tests.sh` (set `PBPS_TEST_PORT` if 14330 is taken), or set
`PBPS_TEST_DB` and `cargo test -p pbps-mssql --test live -- --ignored`. The
script also runs `pbps-cli`'s ignored tests, which include the `plan --dev`
rehearsal. They are `#[ignore]`d so the ordinary suite stays offline; CI has a
dedicated job. When touching the emitter, the catalog queries or the ledger, run
them — they have caught four bugs the unit suite structurally could not: FK
ordering between two new tables; `EXEC()` rejecting function calls in its
argument; `sql_expression_dependencies` returning one row per referenced
*column*; and check constraints arriving as dependencies of their own table.
The module round-trip is in the same category: only a real `sys.sql_modules` can
say whether what the emitter sent is what comes back.

**In progress**: Phase 3.1, the usability foundation of SPEC 14. `init` is
built; next are `doctor`, plan summaries and `explain`, one typed JSON output
across the read-only commands, editor schemas, shell completions and the
interactive rename prompt of SPEC 6.3 — the third intent channel, and the only
part of "intent is recorded by a human, in git" still missing. It is placed
ahead of the next dialect deliberately: broadening the object model improves
coverage, but these improve the first hour and every failure after it.

Then **Phase 4, depth on SQL Server before breadth across engines**: declarative
reference data (ADR-0004), roles and grants (ADR-0005), the `policies:` block
and a wider built-in analyzer catalogue. The ordering was chosen against the
obvious one — engine count is what every comparison table measures — because a
second dialect doubles the surface every later feature is built twice for, and
does it while the first engine still cannot express an organization's own rules.
The reasoning is in SPEC 12 and open question 9.

**Phase 5** is the PostgreSQL dialect, the touchstone for the `Dialect`
abstraction; two collisions are already known to be waiting — PostgreSQL
identifies a function by name **plus argument types**, so "the name is the
identity" needs revisiting (ADR-0002), and default and schema privileges do the
same to ADR-0005. Deferring the dialect does not defer the abstraction: PG stays
the test applied to every model decision.

**Phase 6** is the optional local UI (ADR-0006). The guardrail against a policy
SaaS refuses *a control plane that holds the approval*, not a screen: the UI
renders the typed JSON of Phase 3.1, composes intent as a git commit, triggers
the same checksum-pinned plan, and stores nothing authoritative. It is late
because a UI built before that JSON exists would have to parse human output or
reimplement validation.
