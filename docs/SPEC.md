# PongBiphang Schema (`pbps`) — design specification

> Status: finalized and implementable; Phase 0 and Phase 1 are built
> Language: Rust
> Position: declarative database schema version control and deployment
> Primary dialect: SQL Server; secondary: PostgreSQL

---

## 1. Position and scope

### 1.1 What this is

A **declarative** database schema management tool. Users maintain declarations of
what they want the schema to look like, and the tool computes the difference,
generates the change script, applies it to each environment behind a controlled
gate, and records a complete audit trail.

Compared with what exists:

| Tool | Model | Gap |
|---|---|---|
| Flyway / Liquibase | Imperative | Verbose; the current schema is never visible at a glance; refactoring is hard |
| Atlas | Declarative | Renames rest on heuristic detection and the generated migration needs hand-editing; HCL learning curve; OSS/Pro feature split |
| Skeema | Declarative | MySQL only |
| DACPAC | Declarative | Tied to the SQL Server + Visual Studio ecosystem; rename and accidental-drop risk |

`pbps` does not differentiate on "declarative" itself, but on four things:

1. **Rename and drop intent is stated explicitly by a human**, recorded in version
   control, never guessed
2. **Changes are classified by risk**, and dangerous operations must be allowed
   explicitly at the command level
3. **Saved plan plus checksum**: the reviewed plan is pinned, so apply does
   neither more nor less
4. **State and the audit ledger live in the database itself**, which supports
   environments evolving independently as a matter of course

### 1.2 What v1 covers

**In**: tables (create / drop / rename), columns, primary keys, unique
constraints, foreign keys, check constraints, indexes.

**Deferred**: views, stored procedures, functions, triggers, permissions (GRANT),
data transformation (backfill).

Objects like views and stored procedures, where the definition simply *is* the
latest version, behave much like repeatable migrations. That is a different model,
left to a later phase.

### 1.3 Explicit non-goals

- **Data transformation (backfill) is not automated.** How data should be moved is
  a business decision and cannot be derived from a structural diff. When it is
  needed, a human runs the SQL and then re-baselines with `pbps baseline`.
- **No cross-dialect abstract type system.** One schema is bound to one dialect.
  "Supports multiple databases" means the tool can drive MSSQL and PostgreSQL, not
  that one set of files deploys to both.
- **History is not replayed.** A declarative tool has no migration history to
  re-run; DR and new-environment rebuilds go through `pbps bootstrap`, which
  generates the complete schema in one shot — faster and more reliable than
  replaying hundreds of scripts.

---

## 2. Core design principles

1. **The declarations contain only the desired state** — the columns you want, and
   that is all. No UIDs, no rename annotations, no tombstones.
2. **Only what cannot be derived needs a human** — rename and drop intent is the
   whole of that category; everything else is decided automatically.
3. **Intent is stated once and recorded permanently** — in the identity file in
   version control, decoupled from each environment's deployment progress.
4. **Human judgement happens at authoring time, not deployment time** — by the
   time a tag is deployed, every intent is already in git and CI is fully
   automatic.
5. **Dangerous operations must be allowed explicitly** — but the granularity of
   the approval is "this reviewed plan", not "this column".
6. **Declared state and verified state are kept apart** — before applying,
   the database's real state must be confirmed not to have diverged.

---

## 3. Conceptual model

Three persistent artifacts with strictly separated responsibilities:

| Artifact | Location | Contents | Maintained by |
|---|---|---|---|
| `schema/*.yml` | git | Desired state: tables, columns, constraints, indexes | Humans |
| `schema.ids.json` | git | Identity ledger: uid to name, plus tombstones | The tool |
| `__pbps_state` table | Each environment's database | That environment's verified state snapshot plus the audit ledger | The tool |

**Why the identity file goes into git**: rename intent has to survive until every
environment has applied it. Dev applied and prod five versions behind is the norm,
and if intent were consumed by the first apply, the information would no longer
exist anywhere by the time prod deployed. Putting it in git decouples intent from
each environment's progress.

**Why state lives in the database**: each environment's database remembers its own
state, so dev, staging and prod are independent by construction, with no artifact
passing and no environment-mapping strategy.

---

## 4. Declaration format

### 4.1 Project configuration

```yaml
# pbps.yml
dialect: mssql
schema_dir: schema/
ids_file: schema.ids.json
```

### 4.2 Table definitions

One table per file. File names carry no meaning; the table name comes from
`table:`.

```yaml
# schema/dbo.customer.yml
table: dbo.customer
description: Customer master

columns:
  customer_id:
    type: bigint
    nullable: false
    identity: [1, 1]

  full_name:
    type: nvarchar(100)
    nullable: false
    description: The customer's full name

  email:
    type: nvarchar(255)

  balance:
    type: bigint
    nullable: false
    default: 0

  created_at:
    type: datetime2(3)
    nullable: false
    default: SYSUTCDATETIME()

  legacy_code:
    type: varchar(20)
    deprecated: superseded by email as the identifier

primary_key: [customer_id]

unique:
  uq_customer_email: [email]

foreign_keys:
  fk_customer_region:
    columns: [region_id]
    references: dbo.region(region_id)
    on_delete: no_action

checks:
  ck_customer_balance: balance >= 0

indexes:
  ix_customer_created:
    columns: [created_at]
    include: [full_name]
    where: legacy_code IS NULL
```

### 4.3 Format rules

- **`columns` is a map, not a list**, and the key is the column name. One less
  level of nesting, and duplicate names become impossible to express.
- **`nullable` defaults to `true`** and is written only when needed. The field
  cannot be called `null` — that is YAML's null literal; see
  [ADR-0001](ADR-0001-yaml-crate.md).
- **`type` uses the dialect's native type string**, and the tool normalizes it
  (`INT` / `int` / `integer` are one type).
- **`deprecated` needs only a reason**; the date comes from git and is not written
  by hand.
- **Comments belong in `description` fields only.** The tool owns the file format
  and `pbps fmt` rewrites files canonically, so ordinary YAML comments are lost.
  `description` doubles as the source for data-catalogue integration.
- **`pbps fmt` must quote string scalars on output**, covering boolean-ish
  literals (`true`/`false`/`yes`/`no`/`on`/`off`), null-ish ones (`null`/`~`) and
  number-shaped strings. Otherwise a file the tool writes would come back as a
  different type on the next read.

### 4.4 Column lifecycle

```
(absent) ──add──► [Active] ──add deprecated──► [Deprecated]
                     │                              │
                     │ removed from the file        │ removed from the file
                     ▼                              ▼
                  [Dropped] ◄──────────────────────-┘
                  (the tombstone lives in the ids file, not the YAML)
```

- **Active**: an ordinary column.
- **Deprecated**: still present in the database and still in the YAML, because it
  genuinely is part of the desired state, but it should receive no further
  attribute changes (see L009). It may optionally be written to an extended
  property for a data catalogue to pick up.
- **Dropped**: removing it from the YAML means deleting it. The tool emits `DROP
  COLUMN`, requires `--allow destructive`, leaves a tombstone in the ids file and
  a record in the `__pbps_state` ledger.

`Deprecated` is not a terminal state, and going straight from Active to Dropped is
allowed — it simply faces the same destructive gate. This design resolves two
problems at once, "compliance requires PII to actually be deleted" and "the
declarations accumulate zombie columns": **the files never hold zombies, and the
audit trail lives in the ids file and the database ledger, where it is
queryable**.

---

## 5. The identity file (`schema.ids.json`)

### 5.1 Format

```json
{
  "version": 1,
  "tables": {
    "t_a9k2mq": "dbo.customer"
  },
  "columns": {
    "c_k7x2mq": "dbo.customer.customer_id",
    "c_p3n8vd": "dbo.customer.full_name",
    "c_t8h4bn": "dbo.customer.legacy_code"
  },
  "tombstones": {
    "c_v2c9ql": {
      "was": "dbo.customer.national_id",
      "dropped_at": "2026-08-30",
      "reason": "REG-2026-042 PII erasure request",
      "operator": "leon"
    }
  }
}
```

It stores only **what cannot be derived from the YAML**: the identity mapping and
the tombstones. Types, nullability and index definitions are never stored — the
YAML already has them.

A 200-column project is 200 lines of `uid: name`, and a rename's diff is:

```diff
-    "c_p3n8vd": "dbo.customer.customer_name",
+    "c_p3n8vd": "dbo.customer.full_name",
```

One name changing under one uid — an unambiguous rename signal in an MR review.

### 5.2 UID rules

- Format: a `c_` / `t_` prefix plus six random base32 characters
- Globally unique, not merely unique within a table
- **Users never have to type one or see one**; it is purely an internal identity
  anchor
- Random rather than sequential, so two branches cannot hand out the same number

Comparison uses the identity file from **each side** and matches by uid, rather
than "name plus this revision's intent". The latter breaks on a jump-version
deploy: when an environment is five versions behind, that rename intent left the
working tree long ago. With an identity file on both sides the base's ids say
`c_x → customer_name`, the declared ids say `c_x → full_name`, and one comparison
gives the rename directly — in one step, with no need to walk the chain of names
version by version. This is also why `StateSnapshot` carries `ids`.

### 5.3 Cross-branch conflicts

Two branches doing different things to one column both edit the same uid's line in
the ids file, which is a **git merge conflict** and stops right there. No separate
"referential integrity between annotations" checking rules are needed.

---

## 6. Expressing intent

Only two kinds of change need human intent: **rename** and **drop** (when one
table both loses and gains a column, the two cannot be told apart automatically).

There are three equivalent inputs, all of which converge on the ids file.

### 6.1 CLI commands (the primary interface)

```bash
pbps rename dbo.customer.customer_name full_name
pbps rename-table dbo.customer dbo.client
pbps drop dbo.customer.national_id --reason "REG-2026-042 PII erasure request"
```

Fully non-interactive, scriptable, and **needs no database connection**.

### 6.2 Transient YAML annotations

```yaml
columns:
  full_name:
    type: nvarchar(100)
    nullable: false
    renamed_from: customer_name    # transient: removed once pbps plan absorbs it
```

`pbps plan` reads it, writes the fact into the ids file and removes the line from
the YAML. This is the escape hatch for **an editor and nothing else**, and it does
not accumulate in the files.

### 6.3 Interactive prompt

A convenience wrapper for when a TTY is detected; what it actually runs is the
commands from 6.1.

```
$ pbps plan

  dbo.customer
    ? customer_name disappeared and full_name is new
      > this is a rename: customer_name -> full_name
        no, drop customer_name and add full_name
```

### 6.4 Behaviour without a TTY

With no TTY it **never prompts**; it fails and prints a copy-pastable command:

```
$ pbps plan
error: 1 change could not be decided automatically

  dbo.customer: customer_name disappeared, full_name is new

  if renamed:  pbps rename dbo.customer.customer_name full_name
  if dropped:  pbps drop dbo.customer.customer_name --reason "<why>"
```

---

## 7. Change classification and the risk gate

### 7.1 Decided automatically vs needing intent

| Situation | Needs human intent | Notes |
|---|---|---|
| A pure addition of a column, table or index | No | Nothing disappeared, so it must be an add |
| Type widening (INT→BIGINT) | No | A safe change |
| Narrowing, adding NOT NULL, adding a constraint | No | The intent is clear, but the class is dangerous |
| A pure deletion with no additions in the same table | **Yes** (a reason) | Unambiguous as an operation, but the tombstone has to answer an audit's "why", which no algorithm can produce |
| **A disappearance and an addition in the same table** | **Yes** | Rename or drop+add; indistinguishable |

### 7.2 Risk classes

| Class | Trigger | Risk |
|---|---|---|
| `rename` | A column or table is renamed | Dependent objects break (see 7.4) |
| `destructive` | DROP COLUMN / DROP TABLE / DROP INDEX | Data loss |
| `narrowing` | Type narrowing or an incompatible conversion | Truncation, failed conversion |
| `not-null` | nullable → NOT NULL with no DEFAULT | Existing NULLs violate it |
| `constraint` | Adding UNIQUE / FK / CHECK | Existing rows may not satisfy it |

The criterion is **whether this kind of change can fail at all**; data is not read
to decide whether this particular run happens to be safe. Data-level validation is
a runtime concern and outside the declarative layer's responsibility.

### 7.3 Saved plan plus checksum

```
pbps plan  -> plan.json (the change list plus a checksum of the state it was
                         computed against)
              plan.sql  (human-readable, attached to the MR for review)

pbps apply --plan plan.json --allow rename,destructive
```

`apply` first verifies that the database's current checksum still equals the
baseline the plan was computed against, and aborts otherwise — the drift check.

Because the change set is pinned by checksum, a coarse flag like `--allow` is
safe: **what gets approved is exactly the plan reviewed in the MR, no more and no
less**. The flag lives in the CI configuration, in plain sight and auditable.

### 7.4 Rename impact report

When `plan` detects a rename and a connection is available, it queries
dependencies and prints a report:

| Source (MSSQL) | How | Consequence |
|---|---|---|
| views / SPs / functions / triggers | `sys.sql_expression_dependencies` | Lists every referrer |
| SCHEMABINDING views | As above plus `is_schema_bound` | **Blocks the rename outright**; must be dropped first |
| Computed columns | `sys.computed_columns` | The definition breaks |
| DEFAULT / CHECK definitions | `sys.check_constraints` | The definition text holds the old name |
| Index / constraint names | `sys.indexes` | The objects are fine, but names may embed the old column name (naming drift) |

Dialect differences have to be absorbed by the abstraction: PostgreSQL stores
resolved dependencies and `RENAME COLUMN` updates views automatically, whereas SQL
Server stores definition text and `sp_rename` **does not**. That is exactly why
`rename_impact` belongs on the `Dialect` trait.

Impact outside the database — applications, reports, downstream ELT — is invisible
to the tool; a checklist is printed and attached to the MR for a human to sign
off.

---

## 8. Environment state and drift

### 8.1 `__pbps_state`

```sql
CREATE TABLE dbo.__pbps_state (
    id            BIGINT IDENTITY PRIMARY KEY,
    applied_at    DATETIME2(3)   NOT NULL,
    kind          VARCHAR(16)    NOT NULL,   -- apply | baseline | bootstrap
    git_sha       VARCHAR(40)    NULL,
    plan_checksum CHAR(64)       NULL,
    state_json    NVARCHAR(MAX)  NOT NULL,   -- the whole schema snapshot plus the
                                             -- identity mapping as of that moment
    operator      NVARCHAR(128)  NOT NULL,
    reason        NVARCHAR(1000) NULL
);

CREATE TABLE dbo.__pbps_lock (
    id         INT PRIMARY KEY CHECK (id = 1),
    locked_by  NVARCHAR(256) NOT NULL,
    locked_at  DATETIME2(3)  NOT NULL
);
```

The **whole snapshot** is stored rather than a delta or a checksum: drift
detection can then compare in full, the snapshot doubles as a backup, and it can
answer "what did this table look like three months ago?". `pbps state prune --keep
50` handles cleanup.

`__pbps_lock` stops two pipelines applying at once.

### 8.2 The drift check

Before `apply`: query `sys.columns` / `INFORMATION_SCHEMA`, compare against the
newest `state_json` row, and abort on any mismatch, asking for manual
reconciliation.

What this catches is somebody having SSHed in and changed the schema by hand.

### 8.3 Two ways out of drift

| Path | Command | When |
|---|---|---|
| Accept reality: fold the manual change back into the declarations | `pbps pull --table X`, then merge by hand | The manual change was right and should be kept |
| Restore the declarations: change the database back | Fix, then run `plan` / `apply` normally | The manual change was a mistake |
| Rebaseline: do not pursue the difference, take the current state as the new starting point | `pbps baseline --reason ... --operator ...` | A DBA has already dealt with it under a specific approval |

`baseline` queries the database, writes a new `state_json`, and records the
reason, operator and timestamp in the ledger.

---

## 9. The command set

### 9.1 No database connection required

| Command | Purpose |
|---|---|
| `pbps plan` | Resolve identity ambiguities, update the identity file, and compare against a baseline to produce a change plan |
| `pbps plan --base <file>` | Use a state snapshot file as the baseline instead (for environments without git) |
| `pbps plan --check` | CI mode: fail only when intent is missing, and never prompt |
| `pbps fmt` / `fmt --check` | Canonicalize the declaration format |
| `pbps rename` / `rename-table` / `drop` / `drop-table` | Record intent into the ids file |
| `pbps validate` | Static checks: type validity, FK targets exist, naming rules |

That `plan` needs no database is deliberate: **when production cannot be reached
directly, a developer can still do the whole job locally**.

Baseline precedence:

1. `--base <file>`: an explicitly named state snapshot. It is **never read or
   written automatically** — an escape hatch for environments without git
   (export-style checkouts, air-gapped archives), not a second artifact to
   maintain.
2. A revision in git (`--since`, default `HEAD`), where what the baseline is stays
   obvious.
3. An empty baseline, with a loud warning — everything is listed as newly created,
   which is dangerous if mistaken for a real plan.

**Anything computed offline is a preview.** A plan that is actually going to be
applied to an environment must be based on that environment's database as queried
(Phase 3). The identity file stores only uid to name, from which type changes
cannot be derived; comparing attributes always needs a baseline that carries the
full state.

### 9.2 Database connection required

| Command | Purpose |
|---|---|
| `pbps pull` | Reverse-generate YAML declarations from an existing database (a new user's first step) |
| `pbps verify` | The drift check: the live database against `__pbps_state` |
| `pbps apply --plan plan.json --allow ...` | Apply a plan |
| `pbps snapshot` | Query the database and write a new `__pbps_state` |
| `pbps baseline --reason --operator` | Reset the state baseline |
| `pbps bootstrap` | Generate the complete CREATE script from the declarations (DR, new environments) |
| `pbps state prune --keep N` | Clean up historical snapshots |

`pbps pull` is the key to the adoption threshold: every new user's first step is
"I already have a database". Without it, the cost of adoption is transcribing two
hundred tables by hand.

---

## 10. The CI/CD flow

```yaml
stages: [check, plan, verify, apply, record]

check:
  script:
    - pbps plan --check          # fails only on missing intent; the rest is automatic
    - pbps validate
    - pbps fmt --check

plan:
  script:
    - pbps plan --out plan.json --sql plan.sql
  artifacts:
    paths: [plan.json, plan.sql]   # plan.sql is attached to the MR for review

verify:
  script:
    - pbps verify --db "$PROD_CONN"
  only: [/^prod-v.*$/]

apply:
  script:
    - pbps apply --db "$PROD_CONN" --plan plan.json --allow rename,narrowing
  when: manual
  only: [/^prod-v.*$/]

record:
  script:
    - pbps snapshot --db "$PROD_CONN"
  only: [/^prod-v.*$/]
```

**The deployment stage requires no human judgement at all.** Rename and drop
intent was resolved and committed to git when the developer wrote the MR; at tag
time CI merely follows instructions. `when: manual` is an approval gate, not a
decision point.

---

## 11. Architecture

### 11.1 Crate layout

```
pbps/
  crates/
    pbps-model/     Domain model: Schema, Table, Column, ColumnType, Uid,
                    ChangeSet, RiskClass; serialization of the ids file and state
    pbps-config/    The pbps.yml project configuration
    pbps-load/      YAML loading plus span-carrying diagnostics; fmt rendering
    pbps-diff/      YAML <-> ids comparison -> ChangeSet (pure data, no SQL)
    pbps-dialect/   The Dialect trait plus shared helpers
    pbps-mssql/     MSSQL: type normalization, SQL generation, introspection,
                    dependency queries
    pbps-pg/        PostgreSQL (Phase 4)
    pbps-db/        Connection abstraction, __pbps_state access, locking
    pbps-cli/       clap, interactive prompts, diagnostic output
```

**The key to the layering**: what `diff` produces is a typed `ChangeSet`, not SQL
strings. Risk classification, gate decisions and impact analysis all happen on
structured data; SQL appears exactly once, in the dialect crate's emitter.

### 11.2 The `Dialect` trait

```rust
pub trait Dialect {
    fn name(&self) -> &'static str;

    /// Type string normalization: INT / int / integer -> one ColumnType
    fn parse_type(&self, s: &str) -> Result<ColumnType>;
    fn render_type(&self, t: &ColumnType) -> String;

    /// How safe a type change is (widening / narrowing / incompatible)
    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> Risk;

    fn quote_ident(&self, s: &str) -> String;

    /// ChangeSet -> executable statements
    fn emit(&self, change: &Change) -> Result<Vec<Statement>>;

    fn introspect(&self, conn: &mut Conn) -> Result<Schema>;

    /// The dependency impact of a rename (needed for MSSQL, nearly empty for PG)
    fn rename_impact(&self, conn: &mut Conn, target: &RenameTarget)
        -> Result<ImpactReport>;
}
```

`pbps-model`, `pbps-diff` and `pbps-load` are entirely dialect-agnostic. Adding a
database is work with clearly drawn boundaries.

### 11.3 Principal dependencies

| Purpose | Crate | Notes |
|---|---|---|
| SQL Server | `tiberius` | Pure Rust; **no ODBC driver to install** — decisive for air-gapped environments |
| PostgreSQL | `tokio-postgres` | Phase 4 |
| Async | `tokio` plus `tokio-util` (tiberius compat) | |
| CLI | `clap` (derive) | |
| Diagnostics | `miette` | Errors with source spans; the heart of the Phase 1 product experience |
| Errors | `thiserror` (libraries) / `anyhow` (binaries) | |
| Serialization | `serde` plus `serde_json` | ids and state need canonical output: `BTreeMap`, fixed ordering |
| YAML | `serde-saphyr` | Settled; see [ADR-0001](ADR-0001-yaml-crate.md). Errors carry an `offset + len` span and a source excerpt, and duplicate-key detection is built in. This sets the MSRV at 1.89 |
| Interactive prompt | `dialoguer` or `inquire` | |
| Testing | `insta` | Snapshot tests for the AST, diagnostic output and generated SQL |

**Explicitly not used**: `sqlx` (compile-time checking is meaningless for dynamic
DDL), `diesel`, and any SQL parser (unnecessary once the format is YAML).

### 11.4 Distribution

A single static binary. Windows runners use `x86_64-pc-windows-msvc`; Linux
runners use `x86_64-unknown-linux-musl` for a fully static file. Since `tiberius`
is pure Rust, both achieve "copy one file and run it", with no runtime to install
on an air-gapped host.

### 11.5 Testing strategy

A tool like this can easily end up with all tests passing while it loses data, so
four invariants must be machine-verified:

1. **Format round-trip**: `load(fmt(schema)) == schema`
2. **Bootstrap consistency**: declarations -> `bootstrap` into an empty database ->
   `introspect` -> equals the declared state. This guarantees "the declarations are
   the database".
3. **Migration convergence**: a database in state A -> apply `plan(A→B)` ->
   `introspect` -> equals state B. **The most important one**, run against a real
   SQL Server in Docker.
4. **Diagnostic snapshots**: every error message pinned with `insta`.

---

## 12. Phases

| Phase | Contents | Value delivered |
|---|---|---|
| **Phase 0** | Workspace skeleton, the `pbps-model` data model, finalizing the YAML and ids formats, the `Dialect` trait, verifying the YAML crate's span capabilities | The foundation for everything, and the most expensive to change |
| **Phase 1** | `load` / `fmt` / `diff` / the ids file / the three intent channels / `plan` / `plan --check` / `validate` | Files only, zero risk. Already produces a plan.sql for a human to run |
| **Phase 2** | The MSSQL emitter, introspection and **`pbps pull`** | Reverse generation removes the adoption barrier, which is the key to being used at all |
| **Phase 3** | `__pbps_state` / locking / `verify` / `apply` / the `--allow` gate / the rename impact report / `snapshot` / `baseline` / `bootstrap` | The complete product |
| **Phase 4** | The PostgreSQL dialect | The touchstone for whether the abstraction is right. PG was used as the hypothetical case while designing Phase 0 |
| **Phase 5** | The repeatable model for views and SPs, extended properties and data-catalogue integration, more dialects | |

When designing the `Dialect` trait in Phase 0, **PostgreSQL has to be considered
at the same time**, even though it is not implemented. If Phase 4 forces a large
change to `pbps-model`, the Phase 0 abstraction was drawn in the wrong place.

---

## 13. Known open questions

1. **The supply-chain risk of `serde-saphyr`** — chosen
   ([ADR-0001](ADR-0001-yaml-crate.md)), but it is a young single-maintainer
   crate. The mitigation is the isolation the architecture already has: only
   `pbps-load` depends on it directly. Its maintenance needs watching.

2. **The execution strategy for ALTER on large tables** — ONLINE options,
   batching and off-peak scheduling are runtime decisions that a diff cannot
   derive. Possible directions: a table-level `strategy:` annotation, or allowing
   plan.sql to be hand-edited during the MR before being applied.

3. **Coordinating application and database deployment timing** — zero-downtime
   usually needs schema changes and application versions staggered. The tool does
   not manage that, but `--allow` and the saved plan make "which version is applied
   when" controllable. Multi-stage flows such as expand → dual-write → backfill →
   contract span several deployments and are currently unsupported.

4. **Access control for `baseline`** — who may run it, and how it hooks into
   GitLab approvals, is still to be designed.

5. **Data transformation (backfill)** — explicitly out of scope for now (see 1.3).
   If a repeating pattern accumulates, a hook mechanism can be designed against
   real cases.

6. **How views and SPs should be handled** — for these objects the definition *is*
   the latest version, which is closer to repeatable migration and different from
   the identity-tracking model used for columns. It needs its own design.

7. **Whether permissions (GRANT) belong here** — declarative permission management
   has value, but its risk model differs from schema change and the dialects vary
   widely.

---

## Appendix A: name research

Findings for `pbps` as of 2026-08:

- crates.io: **no crate of that name** (`pbm` is also free, but `pbs` is taken by
  an OpenPBS FFI binding)
- No CLI binary of that name anywhere, so no PATH conflict
- The only namesake is PBPS (Performance Based Prevention System, a US substance
  abuse prevention reporting system) — a different field, not a CLI, and no
  practical problem

Candidates that were ruled out:

- `pbm` — collides with Percona Backup for MongoDB (**also a database tool, the
  worst kind of collision**), the Netpbm image format, and Petabridge.Cmd
- `pbs` — already taken on crates.io; collides with Portable Batch System (the HPC
  scheduler) and the PYBOSSA CLI

**Publishing a `0.0.0` placeholder on crates.io to reserve the name is worth doing
soon.**
