# PongBiphang Schema (`pbps`)

Declarative database schema version control. You maintain one description of what
the schema should look like; the tool works out the rest.

- Design specification: [docs/SPEC.md](docs/SPEC.md)
- Decision records: [docs/ADR-0001-yaml-crate.md](docs/ADR-0001-yaml-crate.md)
  through [docs/ADR-0005-roles-and-grants.md](docs/ADR-0005-roles-and-grants.md)

**Phases 0 through 3.5 are complete**, for SQL Server: tables, columns, keys,
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

| Needs a database | Purpose |
|---|---|
| `pull` | Reverse-generate declarations from an existing database |
| `plan --db` / `--env` (`--staged`) | The applyable plan for one environment |
| `apply --plan --allow` | Run an approved plan, in one transaction |
| `apply --staged` (`--resume`) | Run one logical change outside a transaction, checkpointing each statement |
| `verify` (`--format json`) | The drift check; exits 2 on drift |
| `snapshot`, `baseline`, `bootstrap`, `state prune`, `unlock` | The state ledger |
| `status` (`--format json`) | One screen across every configured environment |

Phase 3.1 is in progress: `init` is complete; `doctor`, plan summaries,
`explain`, typed read-only output, editor schemas and completions are next (see
[SPEC §14](docs/SPEC.md)). PostgreSQL is Phase 4.

### Starting a project

For a new declaration-first project:

```bash
pbps init --env prod --url-env PROD_CONN
```

To adopt the database named by `$PROD_CONN` in the same step:

```bash
pbps init --from prod --url-env PROD_CONN
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
