# CLAUDE.md

Guidance for Claude working in this repo.

## What this is

`pbps` (PongBiphang Schema) is a declarative database schema version control
tool. Users maintain YAML describing what the schema should look like; the tool
computes the difference, generates the change script, and applies it to each
environment behind a risk gate.

The full design lives in **[docs/SPEC.md](docs/SPEC.md)** — read it before
starting, especially when changing the data model or adding a kind of change.
Decision records are in `docs/ADR-*.md`.

The competition is Atlas and Skeema, both declarative. The differentiation is not
"declarative" itself but four things: rename/drop intent is supplied by a human
and recorded in version control, changes are classified by risk, a saved plan is
pinned by checksum, and state and ledger live in the database itself.

## Development environment

**Develop inside WSL** (Linux is the primary target). The project lives at
`~/pbps`; do not move it to `/mnt/c`.

Do not attempt `x86_64-pc-windows-gnu`: rustup's self-contained mingw has no GNU
assembler, so `windows-sys` — which both `clap` and `miette` pull in — does not
build, and `gnullvm` needs an external llvm-mingw. See the README for details.

All three of these must pass before committing:

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

Phase 2 adds `pbps-mssql` / `pbps-db`; Phase 4 adds `pbps-pg`.

`spikes/` is `exclude`d from the workspace. It holds standalone evaluation
crates, not product code.

## Inviolable constraints

If a change starts breaking any of these, stop and think it through — each one
was paid for.

**1. `Schema` must satisfy "two semantically identical schemas are equal".**
Both diff and drift detection are built on `==`. So the model carries no spans and
no one-shot intent annotations such as `renamed_from` (those are returned
separately by `pbps-load`), and type case must be normalized before comparison.

**2. Containers hold names; elements do not.**
`Table` has no `name` and `Column` has no `name` — the name is the key in the
parent map. This makes "the map key disagrees with the inner name" unwritable.
Functions that need a name take `(name, table)` as two arguments, as
`Dialect::validate_table` does.

**3. SQL appears exactly once, in the dialect emitter.**
`pbps-diff` produces a typed `ChangeSet`, not strings. Risk classification, gate
decisions and impact analysis all happen on structured data.

**4. Risk is data, not a method.**
`Change::intrinsic_risks()` answers only what can be decided without dialect
knowledge (a drop is destructive, a rename is a rename). Type narrowing needs
types compared, so the differ — which holds a `Dialect` — computes it and attaches
it to `PlannedChange::risks`. The model layer must not guess type risks.

**5. Serialization must be deterministic.**
The identity file goes into git, and shifting order manufactures phantom diffs.
Every collection is a `BTreeMap` / `BTreeSet`. The one exception is
`Table::columns`, which uses `IndexMap` to preserve declaration order (it affects
the column layout of CREATE TABLE), but its equality ignores order.

**6. Only rename and drop need human intent.**
Everything else is decided automatically. Never prompt in a non-interactive
environment — fail and print a copy-pastable command instead.

## Format traps (all found the hard way)

- **`null` cannot be a YAML key**; it is the null literal. The field is named
  `nullable`.
- **`no` / `yes` / `on` / `off` parse as booleans.** When `pbps fmt` writes a
  string scalar it must quote proactively, covering the boolean-ish literals, the
  null-ish ones (`null` / `~`) and number-shaped strings.
- **Everything in version control is LF** (see `.gitattributes`). This tool writes
  files itself, and leaving line endings to the platform would break the "the tool
  owns the file format" guarantee on Windows.

## Writing conventions

- Code comments, documentation and commit messages are in **English**. Only the
  conversation with the user is in Chinese.
- Comments explain **why**, not what the code is doing — especially "why not the
  other, more obvious-looking approach", which is what gets broken later.
- Test names describe the property being verified, not the function being called
  (`column_order_does_not_affect_equality`, not `test_eq`).
- Every test module includes **negative cases**: not just that valid input passes,
  but that invalid input is rejected. This tool's failure mode is doing the wrong
  thing silently.
- Commits follow conventional commits; the body explains the trade-offs.

## Current status

**Phase 0 and Phase 1 are complete.** 135 tests, zero clippy warnings.

Available commands: `plan` (with `--check` / `--since` / `--base` / `--out`),
`validate`, `fmt` (with `--check`), `rename`, `rename-table`, `drop`,
`drop-table`.

Decisions from the original spec that changed during Phase 1 (SPEC is in sync):

1. **Comparison matches by uid using an identity file from each side**, rather
   than "name + this revision's intent". The latter breaks on a jump-version
   deploy: when an environment is five versions behind, that intent left the
   working tree long ago.
2. **A drop requires a reason too.** The original spec said pure deletion is
   unambiguous and needs no intent, but the tombstone has to answer an audit's
   "why", and no algorithm can produce that.
3. **Intents must be idempotent.** A `renamed_from` in the declarations stays
   there after the identity file is updated; treating it as an unmatched intent
   would hand the user a baffling failure right after a successful rename.
4. **`StateSnapshot` carries `ids`.** State and identity have to travel together,
   for the same reason as 1.
5. **IDENTITY changes are blocked explicitly**, since IDENTITY cannot be modified
   with ALTER.

**Not done in Phase 1**: the interactive prompt (the third intent channel, for
TTYs). Both the CLI commands and the YAML annotations work, and non-interactive is
the path that must be supported, so this blocks nothing.

**Phase 2 (next)**: `pbps-mssql`'s type normalization, risk judgement, SQL emitter
and introspection, plus `pbps pull` (reverse-generating declarations from an
existing database — the key to the adoption threshold).
`pbps-dialect::MinimalDialect` is a test stand-in to be replaced with the real
thing in Phase 2.
