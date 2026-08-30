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
- All three must pass before committing:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets    # must be warning-free
cargo fmt --all
```

## Architectural boundaries

```
pbps-model     Domain model. Dialect-agnostic, span-free, serializes to JSON
pbps-config    Project configuration (pbps.yml)
pbps-load      YAML -> model; the only crate that may depend on serde-saphyr
pbps-diff      model <-> ids comparison -> ChangeSet. Produces no SQL
pbps-dialect   Dialect abstraction. Pure functions; DB-bound work waits for
               Phase 3's DialectDb
pbps-cli       clap, interactive prompts, diagnostic output
```

- Phase 2 adds `pbps-mssql` / `pbps-db`; Phase 4 adds `pbps-pg`.
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

**Phase 0 and Phase 1 complete.** The test and clippy bar is in "Development
environment" above; counts change too often to record here.

Commands: `plan` (`--check` / `--since` / `--base` / `--out`), `validate`,
`fmt` (`--check`), `rename`, `rename-table`, `drop`, `drop-table`.

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

**Not done in Phase 1**: the interactive prompt (third intent channel, TTY
only). CLI commands and YAML annotations both work; nothing is blocked.

**Phase 2 (next)**: `pbps-mssql` type normalization, risk judgement, SQL
emitter, introspection, and `pbps pull` (reverse-generation — the adoption
key). `pbps-dialect::MinimalDialect` is a test stand-in to be replaced.
