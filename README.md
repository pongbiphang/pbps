# PongBiphang Schema (`pbps`)

Declarative database schema version control. You maintain one description of what
the schema should look like; the tool works out the rest.

- Design specification: [docs/SPEC.md](docs/SPEC.md)
- Decision records: [docs/ADR-0001-yaml-crate.md](docs/ADR-0001-yaml-crate.md)
  through [docs/ADR-0007-connection-strategy.md](docs/ADR-0007-connection-strategy.md)

**Phases 0 through 3.5, and Phase 3.1, are complete** for SQL Server: tables,
columns, keys,
constraints and indexes, plus views, procedures, functions and triggers. Two
groups of commands:

| No database needed | Purpose |
|---|---|
| `init` (`--env` / `--from` / `--url-env`) | Create a complete project, optionally adopting an existing database |
| `plan` (`--check` / `--since` / `--base` / `--out` / `--sql`) | Compare against a baseline and produce a change plan |
| `plan --dev` | Rehearse the plan in a throwaway engine: does it compile, does it converge (optional) |
| `validate`, `fmt` (`--check`) | Static checks and canonical formatting |
| `rename`, `rename-table`, `drop`, `drop-table` | Record the intent only a human can supply |
| `docs` (`--format markdown\|html\|erd`) | Documentation and an ERD from the declarations |
| `explain --plan` | What a saved plan does, why it needs approval, and the exact command that approves it |
| `doctor` (`--env`) | Whether this project and its environments are ready to deploy from |
| `schema` (`--kind declaration\|config`) | JSON Schema for editors, generated from the loader's own types |
| `completions <shell>`, `man` | Shell completions and man pages |

| Needs a database | Purpose |
|---|---|
| `pull` | Reverse-generate declarations from an existing database |
| `plan --db` / `--env` (`--staged`) | The applyable plan for one environment |
| `apply --plan --allow` | Run an approved plan, in one transaction |
| `apply --staged` (`--resume`) | Run one logical change outside a transaction, checkpointing each statement |
| `verify` (`--format json`) | The drift check; exits 2 on drift |
| `snapshot`, `baseline`, `bootstrap`, `state prune`, `unlock` | The state ledger |
| `status` (`--format json`) | One screen across every configured environment |

Every read-only command takes `--format human|json` and emits the same typed
findings. Three exit codes, and the split matters: **0** clean, **2** the command
answered and found something to act on, **1** the command could not answer. A
pipeline that cannot tell 1 from 2 wakes the wrong person half the time.
`scripts/findings-to-github.py` turns the JSON into CI annotations — outside the
binary on purpose, so a vendor format that moves breaks a script rather than a
release.

Phase 3.1, the usability foundation, is complete: `init`, `doctor`, plan
summaries and `explain`, one typed JSON output across the read-only commands,
editor schemas, shell completions, man pages, and the interactive rename prompt
— the third intent channel, and the last part of "intent is recorded by a human,
in git" that was missing (see [SPEC §14](docs/SPEC.md)). Next,
Phase 4 deepens what one engine can express — reference data, roles and grants,
declarative policies — and PostgreSQL follows in Phase 5. Depth precedes the
second dialect on purpose: a team evaluating pbps for SQL Server is not blocked
by the absence of PostgreSQL, it is blocked by the parts of its own estate that
are not yet expressible.

### Starting a project

For a new declaration-first project:

```bash
pbps init --env prod --url-env PROD_CONN
```

To adopt the database named by `$PROD_CONN` in the same step:

```bash
pbps init --from prod --url-env PROD_CONN
```

After adopting an existing database, commit the generated files and initialize
its ledger before making the first connected plan:

```bash
pbps baseline --env prod --reason initial-adoption
```

When `--url-env` is omitted, the variable name is derived deterministically
(`prod` becomes `PROD_CONN`). `init` previews every path, stages and validates
the complete output, and installs `pbps.yml` last; it refuses existing project
files rather than overwriting them.

### Declaring a view

A module is one file, like a table; the leading key is both the kind and the
name. Modules carry no data, so they carry no identity: they never appear in
`schema.ids.json`, and a rename is a lossless drop plus add whose audit trail is
the commit that did it.

```yaml
# schema/dbo.active_customer.view.yml
view: dbo.active_customer
description: Customers that are not legacy records
definition: |-
  SELECT customer_id, full_name
  FROM dbo.customer
  WHERE legacy_code IS NULL
```

The emitter composes `CREATE OR ALTER VIEW [dbo].[active_customer] AS ...` — one
statement that is idempotent and, unlike drop plus create, keeps the permissions
granted on the object.

### Configuring environments

A connection string carries a password and `pbps.yml` is committed, so the
config names the **variable** that holds the string, never the string:

```yaml
dialect: mssql
environments:
  prod:
    url_env: PROD_CONN
hooks:
  on_drift: ./scripts/alert.sh    # receives the drift report as JSON on stdin
dev:                              # optional: the throwaway engine for `plan --dev`
  docker: mcr.microsoft.com/mssql/server:2022-latest
```

The dev database is always optional. Without one, previews fall back to
lightweight normalization and say so — the tool has to remain usable where there
is no Docker and no network.

## What pbps deliberately does not do

Every item here is a request that arrives sounding reasonable. They are refused
for one shared reason: each would be **shorter, but would route around the typed
plan, its checksum, a human's recorded intent, or the git audit trail** — which
between them are the product. The full reasoning is in
[SPEC §14.3](docs/SPEC.md).

| Not shipped | Why, in one line |
|---|---|
| A one-step `push` / `sync` | If a shorter path existed it would become the path everyone uses, and the reviewed one would quietly die. `plan`, then `apply --plan`, stays two steps |
| Automatic rename detection | Similarity may *order the candidates* for a human; it may never decide. A wrong guess drops a column, and no flag may supply the answer non-interactively — a confirmation that can be written once into a CI file has stopped being a confirmation |
| One-step rollback | A historical state is exported and applied as a **new forward plan** through the ordinary gate, the way `git revert` writes a commit rather than rewriting history. It restores structure, not data, and says so where you use it |
| A hosted policy or approval service | Policies, reports and schemas are files; they work air-gapped. A policy outside git is a second gate nobody reviewed. A *local* UI is a different thing and is planned ([ADR-0006](docs/ADR-0006-optional-ui.md)) |
| A plugin execution engine | The test is whether the extension must run *between* "plan approved" and "statements executed". If it must, it makes the checksum describe something other than what runs. Before a plan and after an apply are already served by hooks and CI |
| Reading the schema from your ORM's models | The second source wins every disagreement silently, and identity, drop reasons and execution strategy have nowhere to live in a model class. Generate declarations once with `pull`, then own them |
| Editing `plan.sql` by hand | It is an artifact, not a source file; the checksum exists so that what was reviewed is what runs |

## Development

Linux is the primary development and release target; Windows coverage comes from
the CI matrix.

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

### On Windows, develop inside WSL2

Do not use `x86_64-pc-windows-gnu`. Rustup's self-contained mingw ships no GNU
assembler, so crates that use raw-dylib — `windows-sys` among them — fail to
build, and both `clap` and `miette` pull `windows-sys` in, which means no CLI
with coloured output escapes it. `gnullvm` needs an external llvm-mingw just the
same.

There are two workable paths on Windows:

1. **WSL2** (what this project uses): `sudo apt install build-essential
   pkg-config`, then install stable via rustup. Keep the project inside the WSL
   filesystem (`~/pbps`) rather than under `/mnt/c` — that is a 9p filesystem,
   and cargo's many small reads and writes get noticeably slower on it.
2. **MSVC**: install the VC++ workload of the Visual Studio Build Tools.

### Line endings

Everything in version control is LF (see `.gitattributes`). This tool writes
files itself (`pbps fmt`), and letting the platform decide line endings would
break the guarantee that the tool owns the file format on Windows, as well as
manufacturing phantom git diffs.

## Licence

MIT OR Apache-2.0
