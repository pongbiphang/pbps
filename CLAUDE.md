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
  `state`, `impact`) are async; the CLI `block_on`s them per command. Phase 4
  adds `pbps-pg`.
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

**Phases 0-3 complete** for SQL Server. The test and clippy bar is in
"Development environment" above; counts change too often to record here.

Offline: `plan` (`--check` / `--since` / `--base` / `--out` / `--sql`),
`validate`, `fmt` (`--check`), `rename`, `rename-table`, `drop`, `drop-table`,
`docs` (`--format` / `--out` / `--title`).

Connected (each takes `--db <connection string>` or `--env <name>`): `pull`,
`plan --db`, `apply` (`--plan` / `--allow`), `verify` (`--format json`),
`snapshot` (`--force`), `baseline` (`--reason`), `bootstrap` (`--sql`),
`state prune` (`--keep`), `unlock`, `status` (`--format json`).

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
    statements fail at plan time; pre-flight (rename impact, SCHEMABINDING)
    runs before the first statement.
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
    clustered indexes): each becomes a warning, and a table with no expressible
    columns is left out whole. The round-trip `load(render(pulled)) == pulled`
    is pinned by `pbps-cli/tests/pull_roundtrip.rs`.
15. **Default/check expressions are compared after peeling the engine's stored
    parentheses** (`((0))` → `0`), only when they wrap the whole string.
16. **`strategy:` is persistent, unlike `renamed_from`** — it lives beside the
    model (`Loaded.strategies`, never in `Schema`, or constraint 1 breaks) and
    `fmt` preserves it. Unknown keys are rejected: a typo that became a no-op
    would leave the user believing a large table is altered online when it is
    not (ADR-0003). The emitter honours `online` in Phase 3.
17. **`pull` inventories what it cannot manage.** Views, procedures, functions
    and triggers are listed as unmanaged rather than ignored (ADR-0002); they
    are separate from `warnings`, which are defects in the pull itself.
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
23. **A saved plan carries `origin` and the post-plan `ids`.** `Preview` is a
    value in the file, so `apply` refuses it structurally; the ids make apply
    self-contained on a host with no checkout, and recording the baseline's
    mapping instead would say a rename never happened.
24. **`apply` takes the lock before the pre-flight, and releases it on every
    path.** A check that passed while another pipeline was mid-apply was
    answered about a moving database; a lock left behind blocks the pipeline
    that would fix it.
25. **`verify` exits 2 on drift**, distinct from 1 for a tool failure: a
    scheduled drift-watch wakes different people for each. `status` always
    exits 0 — it is a report, and one unreachable environment must not cost
    the operator the other five lines.

**Not done in Phase 1**: the interactive prompt (third intent channel, TTY
only). CLI commands and YAML annotations both work; nothing is blocked.

**Live tests**: the SPEC §11.5 invariants plus the Phase 3 ones (the ledger
round-trip, the lock admitting one holder, a failed statement rolling the whole
plan back, the rename-impact queries, the probes counting real rows) run
against a real SQL Server in Docker: `scripts/live-tests.sh` (set
`PBPS_TEST_PORT` if 14330 is taken), or set `PBPS_TEST_DB` and `cargo test -p
pbps-mssql --test live -- --ignored`. They are `#[ignore]`d so the ordinary
suite stays offline; CI has a dedicated job. When touching the emitter, the
catalog queries or the ledger, run them — they have caught four bugs the unit
suite structurally could not: FK ordering between two new tables; `EXEC()`
rejecting function calls in its argument; `sql_expression_dependencies`
returning one row per referenced *column*; and check constraints arriving as
dependencies of their own table.

**Not done in Phase 3**: the optional dev database of §9.3 (a throwaway engine
for higher-fidelity previews — always optional, and a dev-verified plan is
still a preview), and the emitter honouring `strategy: online` with
edition-aware classification at `plan --db` (ADR-0003). Neither blocks
anything: the first only sharpens a preview, and `strategy:` is parsed,
preserved and validated already.

**Phase 3.5 next**: the module model for views / SPs / functions / triggers
(ADR-0002), and staged apply for non-transactional operations (ADR-0003).
`Statement::transactional` and `--staged`'s refusal path already exist; what
is missing is the per-statement ledger record and `--resume`.
`pbps-dialect::MinimalDialect` remains only as pbps-diff's test stand-in.
