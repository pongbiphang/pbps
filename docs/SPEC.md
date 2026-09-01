# PongBiphang Schema (`pbps`) — design specification

> Status: living specification; Phases 0-3.5 are built for SQL Server
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
| Flyway | Imperative | Verbose; the current schema is never visible at a glance; refactoring is hard. A pure executor — review, approval and audit are left to the user. Since the Teams tier ended in 2025, undo, drift detection and code analysis are Enterprise-only |
| Liquibase | Imperative | The same imperative gap, plus a changelog dialect to learn and a JVM to carry. Community moved to the Functional Source License in 5.0; policy checks, the drift report and the tamper-evident audit trail are in the paid Secure tier |
| Atlas | Declarative | Rename intent exists (`renamed_from`, v0.22+) but matches by name with no identity anchor, so two branches' renames can merge silently; HCL learning curve; SQL Server, saved-plan approval and drift detection sit behind the Pro plan and its cloud registry, as do views, functions, triggers and grants. The declarative path expects a connection and approval in real time, which a disconnected environment cannot give |
| Bytebase | Governance platform | Answers the review question well — 100+ built-in rules, an approval queue, background drift detection — but as **a server that is the source of truth**: policies, approvals and history live in its own database rather than in git, so it is a second system of record, and it is an operated service rather than a binary in the pipeline |
| Skeema | Declarative | MySQL only |
| DACPAC | Declarative | Tied to the SQL Server + Visual Studio ecosystem; rename and accidental-drop risk |

`pbps` does not differentiate on "declarative" itself, but on four things:

1. **Rename and drop intent is stated once by a human and anchored to identity**:
   the ids file records it against a uid, not a name, so conflicting intents on
   two branches surface as git merge conflicts (see 5.3) instead of merging
   silently — protection that name-matched annotations (Atlas's `renamed_from`)
   cannot give
2. **Changes are classified by risk**, and dangerous operations must be allowed
   explicitly at the command level
3. **Saved plan plus checksum**: the reviewed plan is pinned, so apply does
   neither more nor less — as a free, file-based mechanism, with no cloud
   registry in the loop
4. **State and the audit ledger live in the database itself**, which supports
   environments evolving independently as a matter of course — and keeps the
   whole product usable air-gapped

A fifth thread runs through all four, and through the ADRs: **primitives over
platform**. Where Atlas answers docs, monitoring and approval with a hosted
platform (cloud registry, dashboards, agents), pbps ships composable
primitives — files, exit codes, JSON output, exec hooks — that plug into the
infrastructure a team already runs: git, CI, schedulers, webhooks. For the
regulated environments this tool targets, that is not a cheaper substitute; it
is the requirement. The boundaries that thread draws — and the shortcuts it
rules out even when they would be convenient — are listed in 14.3.

### 1.2 What v1 covers

**In**: tables (create / drop / rename), columns, primary keys, unique
constraints, foreign keys, check constraints, indexes — and, since Phase 3.5,
**modules**: views, stored procedures, functions and triggers.

**Deferred**: permissions (GRANT), data transformation (backfill).

Objects like views and stored procedures, where the definition simply *is* the
latest version, behave much like repeatable migrations. That is a different
model — the module model, designed in [ADR-0002](ADR-0002-module-model.md) and
built in Phase 3.5: they carry no data, so they get none of the identity
machinery and never appear in the ids file (see 4.5).

### 1.3 Explicit non-goals

- **Data transformation (backfill) is not automated.** How data should be moved is
  a business decision and cannot be derived from a structural diff. When it is
  needed, a human runs the SQL and then re-baselines with `pbps baseline`.
  The boundary runs between business data (history — never touched) and
  reference data (code — rows the application logic depends on): the latter
  is declarable through an explicit `data:` block, designed in
  [ADR-0004](ADR-0004-reference-data.md) and targeted at Phase 4.
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

### 4.5 Module definitions

One module per file, as for tables; the leading key is both the kind and the
name (`view:` / `procedure:` / `function:` / `trigger:`), and triggers name
their table with `on:`.

```yaml
# schema/dbo.active_customer.view.yml
view: dbo.active_customer
description: Customers that are not legacy records
definition: |-
  SELECT customer_id, full_name
  FROM dbo.customer
  WHERE legacy_code IS NULL
```

The emitter composes the whole `CREATE OR ALTER` statement, so SQL still appears
exactly once. `definition:` holds everything after the part the emitter can
derive:

| Kind | Emitted prefix | `definition:` starts at |
|---|---|---|
| view | `CREATE OR ALTER VIEW <name> AS` | the `SELECT` |
| trigger | `CREATE OR ALTER TRIGGER <name> ON <on>` | `AFTER INSERT ...` |
| procedure | `CREATE OR ALTER PROCEDURE <name>` | the parameter list, then `AS` |
| function | `CREATE OR ALTER FUNCTION <name>` | the parameter list, then `RETURNS` |

A parameter list stays inside `definition` because it is part of the object's
contract, and modelling T-SQL parameter syntax would mean parsing SQL — which
this tool does not do (8.2).

Two more rules, both consequences of the model rather than choices:

- **`depends_on:` is an annotation, not state.** Creation order is invisible in
  the database, so it lives beside the model exactly as `strategy:` does. It is
  needed only where the identifier scan of ADR-0002 cannot see a dependency.
- **A module name may not collide with a table or another module.** SQL Server
  keeps them in one `sys.objects` namespace per schema, so `pbps validate`
  answers this before the engine does — at apply time the answer arrives on a
  database that is already half-changed.

A module created `WITH ENCRYPTION`, a CLR object, one created with
`QUOTED_IDENTIFIER` or `ANSI_NULLS` OFF (the engine persists those with the
module and re-applies them on every execution, so the same text recreated under
the deployment connection's settings would not mean the same thing), and a view
carrying options the model cannot hold (`WITH SCHEMABINDING`) have no
manageable form. `pull` inventories them with the reason and leaves them alone;
it never recreates one without the option nobody noticed it had.

**One ordering case is knowingly unserved.** Module drops sort before the table
changes and creates and alters after them, which is right for an alter that
begins using a column the same plan adds, and wrong for an alter that *releases*
a schema-bound dependency the same plan then drops. The second needs to run
first, one rank cannot be both, and telling them apart needs facts the model
does not carry. The plan fails at apply, loudly and with nothing changed; the
alternative — drop and recreate the module around the table changes — succeeds
while destroying its GRANTs, which pbps cannot see, restore, or warn about
until ADR-0005. ADR-0002 records the trade in full.

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

There is one hole a line-level conflict does not catch: two branches **each add a
column with the same name**. Each hands out a different random uid, the two land
on different lines of the ids file, and git will likely auto-merge them cleanly —
leaving two uids pointing at one name, a silently corrupted identity mapping.
`validate` therefore must include the rule: **one name may not map to more than
one uid**. On violation it fails and asks a human to decide which uid survives.

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
    renamed_from: customer_name    # transient: pbps plan absorbs it into the ids
                                   # file; pbps fmt strips it once redundant
```

`pbps plan` reads it and writes the fact into the ids file. This is the escape
hatch for **an editor and nothing else**.

The side effects are deliberately split: **`plan` writes only the ids file** (a
tool-owned artifact) and **never rewrites the user's YAML**. Removing the
annotation line is `pbps fmt`'s job — a `renamed_from` whose fact is already in
the ids file is redundant, and canonicalization strips it. An annotation left in
place is harmless: one that agrees with the ids file is a no-op, not an error
(intent is idempotent). `plan --check` in CI is strictly read-only; when an
annotation exists whose fact is not yet in the ids file, it fails with a
copy-pastable instruction (run `pbps plan` locally, commit the ids file).

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

**No flag may stand in for that answer.** Similarity is allowed to order the
candidates in 6.3's prompt, one pair at a time, but an `--assume-renames` would
be written once into a CI file or a shell alias and then never looked at again —
and a confirmation that can become a line of configuration has stopped being a
confirmation (14.3). Whichever channel supplies it, the artifact is the same: one
entry in the ids file, in git, reviewed in the merge request. That is what still
exists when prod deploys the rename five versions later.

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

### 7.3 Saved plan plus checksum: two layers of review

Review happens twice, and the two layers answer different questions:

| Layer | What is reviewed | What it answers |
|---|---|---|
| MR | The YAML diff, the ids diff (intent), and an offline plan.sql **preview** | Do we want this change at all? |
| Deployment | plan.json + plan.sql computed against the target environment as queried | On that environment's current state, what exactly will run? |

```
pbps plan --db $ENV -> plan.json (the change list plus a checksum of the state it
                                  was computed against)
                       plan.sql  (human-readable, for the deployment gate's approver)

pbps apply --db $ENV --plan plan.json --allow rename,destructive
```

`apply` first verifies that the database's current checksum still equals the
baseline the plan was computed against, and aborts otherwise — the drift check.

**The checksum pins "the plan approved at the deployment gate" to "what actually
runs" — not the MR to the apply.** Jump-version deploys (prod five versions
behind) are the norm; the plan computed at deployment time naturally covers the
merged diff of every skipped version, and intent surviving in the ids file is
precisely what makes that possible. A plan computed offline (without `--db`) is
always a preview and is never accepted by `apply`.

Because the change set is pinned by checksum, a coarse flag like `--allow` is
safe: **what gets approved is exactly the plan approved at the deployment gate,
no more and no less**. The flag lives in the CI configuration, in plain sight and
auditable.

plan.sql is the emitter's output and **is never hand-edited** — that would
destroy both the checksum guarantee and "SQL appears exactly once, in the
emitter". Customizing execution goes through the declaration layer's `strategy:`
annotation (see open question 2); anything that genuinely needs manual handling
uses the escape hatch that already exists: a DBA runs the SQL, then
`pbps baseline`.

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

### 7.5 The execution model of `apply`

**One plan, one transaction, all or nothing.** Almost all MSSQL DDL can run
inside a transaction; on failure everything rolls back, the environment is
unchanged, the ledger records the failed attempt, and the drift check stays
clean. Two supporting rules:

- The emitter marks every `Statement` as transactional or not. A plan containing
  a statement that cannot run inside a transaction (certain ONLINE operations,
  full-text, ...) **fails at plan time**, asking for it to be split into its own
  deployment — rather than being discovered halfway through an apply.
- Before executing the first statement, `apply` runs pre-flight checks (a
  connection is guaranteed at this point): the rename impact queries and the
  SCHEMABINDING check happen here. Whatever is going to blow up should blow up
  **before** anything has run, filling in the information an offline plan cannot
  see.

Pre-flight also runs **probes derived automatically from the plan itself**.
The differ's output is a typed `ChangeSet`, so the tool already knows how each
change can fail, and `Dialect::preflight(change)` turns that knowledge into
queries — no user-authored assertions needed (compare Atlas, whose
pre-migration checks are hand-written SQL):

| Risk class | Probe |
|---|---|
| `not-null` | Count the existing NULLs in the column |
| `constraint` | Count the rows that violate the new UNIQUE / FK / CHECK |
| `narrowing` | Count the values that fail or truncate under conversion |
| `rename` | The impact queries of 7.4 |

On a non-zero count, `apply` aborts before the first statement, reporting the
real number ("4,213 rows violate ck_customer_balance"). This does not
contradict 7.2's "data is not read to decide the class": classification stays
static; the probes are the last line of defence at apply time, where a
connection is guaranteed and reading data is exactly the job.

**Nothing user-supplied runs between the approval and the statements.** The
pre-flight is derived from the plan; the exec hooks of 13.5 run after an apply
has finished. That gap is closed deliberately: anything executing inside it
would make the checksum describe something other than what ran, and anything it
changed in the database outside the declarations would become permanent drift
that the next plan tries to remove (14.3).

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

**Trust model**: `__pbps_state` protects against mistakes and process disorder,
**not** against deliberate tampering by someone with DDL rights — whoever can
change the schema by hand can change this table too. The real audit baseline is
git (intent, plans, MR approvals) plus the CI logs; the in-database ledger is
that environment's operational record. Permissions should reflect this: **only
the dedicated deployment account may write `__pbps_state` / `__pbps_lock`**, and
human accounts get read-only — which also answers the access-control question
for `baseline`: it must run under the pipeline identity. If tamper-evidence is
ever needed, apply / baseline can additionally emit each ledger entry to an
append-only destination outside the database (a CI artifact, a webhook), and the
two records must agree.

### 8.2 The drift check

Before `apply`: query `sys.columns` / `INFORMATION_SCHEMA`, compare against the
newest `state_json` row, and abort on any mismatch, asking for manual
reconciliation.

What this catches is somebody having SSHed in and changed the schema by hand.

**The scope of the comparison is the managed set**: the tables that appear in the
declarations (with their columns, constraints and indexes) plus `__pbps_state`
itself. Other objects in the same database are ignored by default — which is what
lets pbps coexist with existing tooling in one database, the precondition for
gradual adoption. Teams that want whole-database control tune it with
`unmanaged: ignore | warn | error` in `pbps.yml`, and scope can also be drawn at
the schema level (manage `dbo` only).

**Expressions (check / default / index WHERE) are never parsed; the database
itself is the normalizer.** Immediately after a successful apply, the tool reads
the definition back and stores the dialect's stored form (MSSQL's
`([balance]>=(0))`) in `state_json`. Both sides of the drift check — the live
query and `state_json` — are then in the database's normalized space, and `==`
holds directly. That is the hottest path and the one that must not produce false
positives. The declaration-versus-baseline comparison (the differ's side) uses a
lightweight best-effort normalization supplied by the `Dialect` (whitespace,
redundant parentheses, `[]`, case); anything still different after normalization
is treated as a constraint change and emitted as drop+add — the cost of a false
positive is rebuilding one constraint, which is cheap and idempotent. When a
dev database is configured (see 9.3), the offline preview upgrades this
best-effort normalization to a real-engine round-trip.

### 8.3 Three ways out of drift

| Path | Command | When |
|---|---|---|
| Accept reality: fold the manual change back into the declarations | `pbps pull --table X`, then merge by hand | The manual change was right and should be kept |
| Restore the declarations: change the database back | Fix, then run `plan` / `apply` normally | The manual change was a mistake |
| Rebaseline: do not pursue the difference, take the current state as the new starting point | `pbps baseline --reason ... --operator ...` | A DBA has already dealt with it under a specific approval |

`baseline` queries the database, writes a new `state_json`, and records the
reason, operator and timestamp in the ledger.

A fourth situation looks similar and is not drift at all: **the change applied
exactly as planned, and the change itself was wrong.** The database matches its
declarations, so nothing here fires. The answer is to take a past state out of
the ledger and go forward into it — `state export` then the ordinary
`plan --db` / `apply` (14.1) — because what changes is the declarations, so git
and the database go back together. There is no one-step rollback, and what comes
back is structure: a column that was dropped returns empty (14.3).

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
| `pbps validate` | Static checks: type validity, FK targets exist, naming rules, identity consistency (one name may not map to more than one uid, see 5.3), module shape and namespace collisions (4.5), plus advisory lints (a revision that both adds and drops or narrows in one table usually wants expand/contract staging, see 13.3) |
| `pbps docs` | Render documentation and an ERD from the declarations (see 9.4) |

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
| `pbps plan --db` | Compute an applyable plan against the target environment as queried (the deployment layer, see 7.3). `--staged` produces a staged plan for one logical change (ADR-0003) |
| `pbps verify` | The drift check: the live database against `__pbps_state`. `--format json` emits the typed drift diff, and found drift fires the `on_drift` hook (see 9.4) |
| `pbps apply --plan plan.json --allow ...` | Apply a plan. `--staged` runs a staged plan statement by statement outside a transaction, recording each completion; `--staged --resume` continues one that stopped |
| `pbps snapshot` | Query the database and write a new `__pbps_state` |
| `pbps baseline --reason --operator` | Reset the state baseline |
| `pbps bootstrap` | Generate the complete CREATE script from the declarations (DR, new environments) |
| `pbps state prune --keep N` | Clean up historical snapshots |
| `pbps status` | One screen across environments: last apply, git sha, drift state, last verified (see 9.4) |

`pbps pull` is the key to the adoption threshold: every new user's first step is
"I already have a database". Without it, the cost of adoption is transcribing two
hundred tables by hand.

**The onboarding workflow**: `pull` designates one **source-of-truth environment**
(usually prod — the one piece of reality that must not be broken) and generates
the declarations and the ids file from it. Then diff against every other
environment to lay the divergences out, deciding per environment whether to
"plan / apply it into agreement with the declarations" or "this environment's
difference is right, fold it back into the declarations". Onboarding is done when
every environment has been `snapshot`ted and drifts by nothing.

### 9.3 The dev database (optional)

`plan` accepts an optional throwaway engine for higher-fidelity previews:

```bash
pbps plan --dev docker://mcr.microsoft.com/mssql/server:2022-latest
pbps plan --dev "Server=...;User Id=...;Password=..."   # a server already running
# or `dev: { docker: ... }` / `dev: { url_env: ... }` in pbps.yml
```

A container is started with a per-run password on a host-chosen port and removed
on every path out; against a server that is already running, pbps creates one
scratch database and drops it. `--dev` and `--db` are refused together: a
rehearsal answers a preview's question, and combining the two would invite a
dev-verified plan to be read as a target-verified one.

The container is used three ways, all on the preview side:

1. **Normalization round-trip** — types and expressions are created in the
   real engine and read back, replacing 8.2's best-effort normalization for
   the preview.
2. **Bootstrap validation** — the declarations must actually compile.
3. **Convergence rehearsal** — bootstrap the baseline state, apply the plan,
   introspect, and compare against the desired state. This is invariant 3 of
   11.5 (migration convergence) surfaced as a user-facing pre-check.

What is left over after the rehearsal is reported in two groups, because they
mean different things. A **structural** difference is a plan that does not
converge, and it fails the command. A **spelling** difference is the engine's
normalization showing through — the declaration says `amount > 0` and the
catalog stores `([amount]>(0))` — and it is reported *with the stored form*,
which is the one thing no offline normalization can produce and exactly what is
needed to silence it.

Three stances, all deliberate and all different from Atlas (whose dev database
is required for many operations):

- **Always optional.** The air-gap promise of 9.1 stands: with no Docker the
  preview degrades to lightweight normalization and says so.
- **A dev-database-verified plan is still a preview.** Applyable plans come
  only from `plan --db` against the target (7.3); the two-layer review model
  does not move.
- **Edition honesty.** The container runs Developer edition (the Enterprise
  feature set) while production may be Standard, so the dev database validates
  syntax and convergence, not edition capabilities — and the tool says so
  rather than pretending otherwise (see [ADR-0003](ADR-0003-execution-strategy.md)
  on edition-dependent risk).

### 9.4 Docs, status, and drift alerting

Three commands, one stance — **primitives over platform** (see 1.1): pbps
provides deterministic outputs and exec points; scheduling and delivery belong
to the infrastructure the team already runs.

**`pbps docs`** renders documentation from the declarations, offline. The
declarations are already documentation-grade source, holding four things a
live-database introspection can never produce:

| Source | Becomes |
|---|---|
| `description` fields (4.3) | Table and column documentation |
| `deprecated` + reason | A "do not use" section |
| The ids file's tombstones | A graveyard: who dropped what, when, and why |
| FK declarations | The edges of the ERD |

Output targets: Markdown, a single self-contained HTML file (no CDN
dependencies — the air-gap rule applies to artifacts too), and a Mermaid
`erDiagram`, which GitLab and GitHub render natively. Output is
deterministic: the same declarations produce byte-identical files.

This command is also what makes 4.3's "comments live in `description` only"
trade-off pay off — the discipline is rewarded, not merely demanded. Together
with `pull` it forms the first-contact story: point pbps at an existing
database and get browsable documentation and an ERD in one step. A later
extension: the preview stage (10) can attach a change-coloured ERD to the MR,
since the typed ChangeSet already knows which tables changed.

**`pbps verify --format json`** emits the drift diff in typed form, and found
drift invokes the `on_drift` hook (13.5) with that JSON on stdin. pbps never
speaks Slack or Teams: it execs a command and the command does the talking —
no credentials to hold, no chat APIs to chase.

**`pbps status`** reads each configured environment's `__pbps_state` and
prints one screen: environment, last apply, git sha, drift state, last
verified. The dashboard's database already exists — every environment
self-reports (8.1) — so there is nothing to host; `--format json` serves
anyone who wants to render their own web view.

Deliberately not built: a hosted service (it would contradict the product's
premise), a resident daemon (pbps is a CLI; a daemon changes the security and
operations profile entirely), and built-in chat integrations (an exec hook
outlives any API).

### 9.5 Readiness (`doctor`)

**`pbps doctor [--env <name>]`** answers "can I deploy from here", in one run.
Before it existed, connection, engine edition, permissions, paths and ledger
readiness each failed later, at a different command — a user adopting their first
database learned about them in the order the commands happened to need them,
often across five runs and two days.

It checks the project (which `pbps.yml` is in force, where the declarations and
the identity file are, whether this is a git checkout and whether it has any
commits) and then each environment: reachability, server version, edition and
therefore whether `strategy: online` can be honoured here, the database-scoped
permissions the account is missing *and what each is for*, and whether the
environment is uninitialized, locked or mid-deployment on a staged checkpoint.

Two rules hold it in place:

- **It reimplements nothing.** The declaration checks are `validate`'s own, run
  through the same function. A readiness command that disagreed with `validate`
  about whether the declarations are valid would be worse than one that never
  looked.
- **It writes nothing.** This is the command someone runs when they are not yet
  sure what they are pointed at, which is quite possibly production — so the
  permissions are *asked for* (`sys.fn_my_permissions`) rather than tried, and
  the ledger is read rather than created. It cannot leave a project or a database
  changed.

Permissions are named individually rather than as "make it `db_owner`". An
organization that grants the deployment account exactly what it needs should be
able to see the list; "make it an owner" is the advice that makes that
organization say no to the tool.

### 9.6 Explaining a plan

**`pbps explain --plan plan.json`** is the deployment gate's own view of a saved
plan, and it is not for the author of the change — the author has the
declarations, the diff and the merge request. It is for whoever holds `plan.json`
and has to decide whether to type `--allow destructive`: a DBA, a release
manager, an auditor, who may have no checkout, no credentials and no intention of
reading T-SQL. Until this command existed, the first human-facing artifact they
met was effectively `plan.sql`.

It answers, from the file alone:

| Question | From |
|---|---|
| Is this applyable at all? | `origin` — a preview says so in its own terms (7.3) |
| What does it change? | the typed ChangeSet, grouped by table |
| Why does it need approval? | each risk class present, **with what can go wrong**, and the changes that carry it |
| How will it run? | `mode`: one transaction all-or-nothing, or staged (ADR-0003) |
| What is checked first? | the derived pre-flight probes (7.5), by description |
| What exactly do I type? | the `apply` command, `--allow` and `--staged` filled in |
| What am I approving? | the plan checksum `apply` will recompute |

A target is **optional**: `--db` / `--env` adds the one question no file can
answer — whether that environment is mid-deployment on a staged checkpoint. It
stays optional because a command needing credentials is a command the reviewer
cannot run, which puts them back to being briefed by the person asking for the
approval.

`explain` always exits 0. A plan full of destructive changes is what it exists to
describe well; the gate is `apply --allow`, and having two commands fail on the
same condition would make the reviewer's own tool look like the failure.

Every `plan` also opens with the same summary — how many changes across how many
tables, then each risk class with its explanation — so the first screenful says
what shape the plan is rather than what its first line happens to be.

### 9.7 Machine-readable output and exit codes

Every read-only command takes `--format human|json` and, in JSON, emits one
envelope:

```json
{
  "schema_version": 1,
  "tool_version": "0.1.0",
  "command": "validate",
  "result": "findings",
  "findings": [
    {
      "id": "load.semantic",
      "severity": "error",
      "message": "invalid index column: `sideways` is not asc or desc",
      "location": { "file": "schema/dbo.t.yml", "line": 6 },
      "remedy": "pbps fmt"
    }
  ],
  "data": { "dialect": "mssql", "tables": 12, "columns": 94, "modules": 3 }
}
```

**One shape, not one per command.** A consumer — a CI annotator, the optional UI
of [ADR-0006](ADR-0006-optional-ui.md), a team's own dashboard — renders what
every command found without growing a parser per command. What a command
uniquely produces rides in `data` (`verify`'s drift report, `status`'s
environment rows) rather than replacing the envelope.

`id` is stable and is the identifier a later `policies:` block raises or lowers
the severity of (14.1), so it must survive a reworded message. `schema_version`
is the version of the envelope alone; it moves when a consumer would have to
change, which the tool version does not.

Three exit codes, and the split is the point:

| Code | Meaning | Who it wakes |
|---|---|---|
| 0 | The command answered and found nothing | nobody |
| 2 | The command answered and found something to act on — invalid declarations, an unformatted file, a stale ids file, unresolved identity, drift, a plan that does not converge | whoever owns the change or the schema |
| 1 | The command could not answer — an unreachable database, an unreadable file, contradictory flags | whoever runs CI |

A pipeline that cannot tell 1 from 2 sends half of every alert to the wrong
person. `status` is the deliberate exception and always exits 0: it is a report
rather than a gate (9.4), so its JSON carries findings at warning severity and
the per-environment truth stays in `state`.

**The vendor formats are converted outside the binary.** GitHub's
`::error file=,line=::` and GitLab's code-quality JSON change on someone else's
schedule, and each one compiled in has to be kept working by this project
forever, including for users who run neither. `scripts/findings-to-github.py`
reads the envelope from stdin and preserves the exit code; a team whose CI is
the third system copies and edits it (14.3).

---

## 10. The CI/CD flow

```yaml
stages: [check, preview, verify, plan, apply, record]

check:
  script:
    - pbps plan --check          # fails only on missing intent; the rest is automatic
    - pbps validate
    - pbps fmt --check

preview:                         # the MR layer: offline, for review only
  script:
    - pbps plan --out preview.json --sql preview.sql
  artifacts:
    paths: [preview.sql]         # attached to the MR: "do we want this change?"

verify:
  script:
    - pbps verify --db "$PROD_CONN"
  only: [/^prod-v.*$/]

plan:                            # the deployment layer: baselined on the target
  script:                        # environment as queried
    - pbps plan --db "$PROD_CONN" --out plan.json --sql plan.sql
  artifacts:
    paths: [plan.json, plan.sql] # what the gate's approver actually reads
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
decision point — what the approver confirms is the deployment-layer plan.sql,
the concrete plan for that specific environment (the two layers of 7.3).

**Monitoring is a scheduled pipeline, not a service.** The same primitives
compose into drift alerting with nothing hosted:

```yaml
# a scheduled pipeline, one per environment
drift-watch:
  script:
    - pbps verify --db "$PROD_CONN" --format json
  # on drift: verify exits non-zero, and the on_drift hook (13.5) has already
  # delivered — a webhook, a chat message, a ticket; its command decides
```

Atlas answers this need with an agent reporting to its cloud; here the
scheduler is the team's CI, the delivery is the team's webhook, and the state
never leaves the team's database.

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
    pbps-pg/        PostgreSQL (Phase 5)
    pbps-db/        Connection abstraction, __pbps_state access, locking
    pbps-docs/      Documentation and ERD rendering (9.4); pure, no dialect
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
| SQL Server | `tiberius` | Pure Rust; **no driver to install** — one static binary, decisive for air-gapped environments. No upstream release since 2024-07, and its pinned TLS stack now carries findings no upgrade can reach: see open question 10, which is a live item rather than a note |
| PostgreSQL | `tokio-postgres` | Phase 5 |
| Async | `tokio` plus `tokio-util` (tiberius compat) | |
| CLI | `clap` (derive) | |
| Diagnostics | `miette` | Errors with source spans; the heart of the Phase 1 product experience |
| Errors | `thiserror` (libraries) / `anyhow` (binaries) | |
| Serialization | `serde` plus `serde_json` | ids and state need canonical output: `BTreeMap`, fixed ordering |
| YAML | `serde-saphyr` | Settled; see [ADR-0001](ADR-0001-yaml-crate.md). Errors carry an `offset + len` span and a source excerpt, and duplicate-key detection is built in. This sets the MSRV at 1.89 |
| Interactive prompt | `dialoguer` or `inquire` | |
| Testing | `insta` | Snapshot tests for the AST, diagnostic output and generated SQL |

**Explicitly not used**: `sqlx` (compile-time checking is meaningless for dynamic
DDL, and its MSSQL backend was removed in 0.7 and has not returned), `diesel`,
and any SQL parser (unnecessary once the format is YAML). A universal connection
layer — ODBC, ADBC — is a separate question with its own answer; see
[ADR-0007](ADR-0007-connection-strategy.md) and open question 11.

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

Phase 3 adds four more that only a live engine can settle, since each is a
promise about the engine's behaviour rather than about the tool's own logic:

5. **Ledger round-trip**: a recorded `StateSnapshot` comes back identical.
   Drift, the plan checksum and `status` all read that row.
6. **Lock exclusivity**: the second holder is refused and told who holds it.
7. **All or nothing**: a plan whose second statement fails leaves the first
   one's effect behind nowhere. Without `XACT_ABORT ON` it would.
8. **Probe accuracy**: the counts a probe reports are the rows the engine would
   actually refuse. The whole value of a probe is its number.

Phase 3.5 adds one more, for the same reason:

9. **Module round-trip**: a module emitted, executed, and read back out of
   `sys.sql_modules` equals the declaration that produced it. If it did not,
   every apply would be followed by a drift report that never goes quiet.

Separately from correctness, CI enforces **supply chain** with `cargo-deny`:
advisories, licences, sources and duplicate versions. It is a scheduled job as
well as a push job, because the interesting failure is a change this repository
did not make — an advisory published against a dependency that was fine
yesterday.

It earned its place on its first run. The exposure of open question 10 had been
checked by hand an hour earlier and pronounced clean; the hand check had looked
at the driver and its direct TLS dependencies and never thought to look at
`rustls-webpki`, where all three vulnerabilities actually were. That is the
argument for the job in one sentence: a person checks the crates they think of.

The live suite carries the other half of that division of labour. `cargo-deny`
can say a dependency is unsafe; only a real engine can say a *replacement* is
safe, which is why the driver question in open question 10 is settled here and
not by a version number.

---

## 12. Phases

| Phase | Contents | Value delivered |
|---|---|---|
| **Phase 0** | Workspace skeleton, the `pbps-model` data model, finalizing the YAML and ids formats, the `Dialect` trait, verifying the YAML crate's span capabilities | The foundation for everything, and the most expensive to change |
| **Phase 1** | `load` / `fmt` / `diff` / the ids file / the three intent channels / `plan` / `plan --check` / `validate` | Files only, zero risk. Already produces a plan.sql for a human to run |
| **Phase 2** | The MSSQL emitter, introspection and **`pbps pull`**; `pbps docs` (9.4); the `strategy:` block enters the format ([ADR-0003](ADR-0003-execution-strategy.md)) and `pull` inventories unmanaged modules ([ADR-0002](ADR-0002-module-model.md)) | Reverse generation removes the adoption barrier — and with `docs`, first contact yields browsable documentation and an ERD in one step |
| **Phase 3** | `__pbps_state` / locking / `verify` (with `--format json`) / `apply` / the `--allow` gate / the rename impact report and automatic preflight probes (7.5) / `snapshot` / `baseline` / `bootstrap` / the `on_apply` and `on_drift` hooks / `status` (9.4); the emitter honours `strategy: online` and `plan --db` classifies by the server's real edition; the optional dev database (9.3) | The complete product |
| **Phase 3.1** | The usability foundation of 14: `init`, `doctor`, plan summaries and `explain`, one typed JSON output across the read-only commands, editor schemas and shell completions, and **the interactive prompt of 6.3** — the third intent channel, and the last place where a competitor's rename detection looks more finished than ours | Makes the safe path the shortest path without changing the deployment model |
| **Phase 3.5** | The module model for views / SPs / functions / triggers ([ADR-0002](ADR-0002-module-model.md)); staged apply for non-transactional operations ([ADR-0003](ADR-0003-execution-strategy.md)) | The other half of a real estate becomes manageable |
| **Phase 4** | Depth on the engine already supported: declarative reference data ([ADR-0004](ADR-0004-reference-data.md)), roles & grants ([ADR-0005](ADR-0005-roles-and-grants.md)), the `policies:` block and the wider built-in analyzer catalogue of 14.1 | Two of Atlas's Pro-gated features land in the free core, and the estate one deployment covers stops being only tables and modules |
| **Phase 5** | The PostgreSQL dialect; then further dialects, one at a time | The touchstone for whether the abstraction is right. PG was used as the hypothetical case while designing Phase 0 |
| **Phase 6** | The optional local UI ([ADR-0006](ADR-0006-optional-ui.md)): a single-user viewer over the typed JSON of 3.1 that can compose intent and commit it, holding no state of its own. Multi-tenant and hosted deployment are out of the open-source scope by decision, and get their own ADR | The people who review database change are not all terminal users; this reaches them without becoming a second system of record |

When designing the `Dialect` trait in Phase 0, **PostgreSQL has to be considered
at the same time**, even though it is not implemented. If Phase 5 forces a large
change to `pbps-model`, the Phase 0 abstraction was drawn in the wrong place.

**Why depth precedes the second dialect.** The obvious ordering is the opposite:
breadth of engines is the number every comparison table counts, and Flyway and
Liquibase win it outright. Two costs argue against taking it first, and it is
worth stating them at their real size rather than their rhetorical one.

The first is **the engine-specific surface**, which is smaller than "everything
doubles" but lands unevenly. Measured on this repository, `pbps-mssql` is about
7,000 lines against roughly 10,700 in the dialect-agnostic crates and 7,300 in
the CLI: a second dialect re-implements what *becomes SQL* and what *reads the
catalog* — the type catalogue, the emitter, introspection, validation, the
probes and the live suite — while the model, the differ, the loader, `docs` and
the policy engine are written once. Phase 4 is placed first because that split
falls badly for it: reference data and grants are dominated by exactly the
per-engine half, so building them after a second dialect builds them twice,
while `policies:` and the analyzer catalogue would not have been.

The second cost is larger and is not measured in lines: **every model decision
must then be resolved for two engines before it can ship**, and it is paid in
design, which is the expensive phase. This is not hypothetical — ADR-0004 and
ADR-0005 each already carry a recorded PostgreSQL collision, so designing them
against one decided engine is a different problem from designing them against
two open ones.

Against both stands the fact that a team evaluating pbps for SQL Server today is
not blocked by the absence of PostgreSQL; it is blocked by the parts of *its
own* estate pbps still cannot manage. Dialects are added afterwards, one at a
time, once what a dialect has to implement has stopped moving — **unless a
specific user is blocked on PostgreSQL**, which is the one input that reverses
the whole ordering, since every argument above assumes an existing user on the
engine already supported.

---

## 13. Known open questions

1. **The supply-chain risk of `serde-saphyr`** — chosen
   ([ADR-0001](ADR-0001-yaml-crate.md)), but it is a young single-maintainer
   crate. The mitigation is the isolation the architecture already has: only
   `pbps-load` depends on it directly. Its maintenance needs watching.

2. **The execution strategy for ALTER on large tables** — settled; see
   [ADR-0003](ADR-0003-execution-strategy.md). A persistent table-level
   `strategy:` annotation lives beside the model (never in it, so Schema
   equality is untouched), the emitter consumes it, and non-transactional
   operations get a dedicated staged apply rather than a weakening of "one
   plan, one transaction". External OSC wrappers (Skeema's `alter-wrapper`
   path) and hand-editing plan.sql stay ruled out — both destroy the checksum
   guarantee and "SQL appears exactly once" (see 7.3); genuinely manual cases
   go through a DBA running the SQL plus `pbps baseline`.

3. **Coordinating application and database deployment timing** — zero-downtime
   usually needs schema changes and application versions staggered. The tool does
   not manage that, but `--allow` and the saved plan make "which version is applied
   when" controllable. Multi-stage flows (expand → dual-write → backfill →
   contract) get **no engine, by decision**: in a declarative model each stage
   is simply a commit, and every stage is already individually supported —
   add: automatic; backfill: the DBA + `baseline` escape hatch; NOT NULL:
   automatic, caught by the preflight probes of 7.5; drop: intent plus a
   reason. What ships instead is a documented staging guide plus an advisory
   lint: `validate` warns when one revision both adds and drops or narrows in
   the same table, which usually wants splitting.

4. **Access control for `baseline`** — the direction is settled: only the
   dedicated deployment account may write `__pbps_state` / `__pbps_lock` (see the
   trust model in 8.1), so `baseline` must run under the pipeline identity. The
   concrete hookup to GitLab approvals is Phase 3 design work.

5. **Data transformation (backfill) and hooks** — backfill stays out of scope
   (see 1.3). The two adjacent needs are settled separately. Pre-apply
   assertions are **derived automatically** from the typed ChangeSet — the
   preflight probes of 7.5 — rather than hand-written by users. User hooks
   stay deliberately minimal: an exec-only family in `pbps.yml` — `on_apply`
   (receiving the plan path, checksum and outcome; also implements 8.1's
   append-only ledger fan-out for tamper-evidence) and `on_drift` (receiving
   `verify`'s typed drift JSON, see 9.4). The CI pipeline remains the real
   hook system (see 10).

6. **How views and SPs should be handled** — settled; see
   [ADR-0002](ADR-0002-module-model.md). Modules (views, procedures,
   functions, triggers) carry no data, so they get a second, identity-free
   model: the declared definition is the desired state, renames are lossless
   drop+add, and git history is the audit trail. Built in Phase 3.5;
   `pull`'s inventory of what it cannot manage landed with Phase 2.

7. **Whether permissions (GRANT) belong here** — settled; see
   [ADR-0005](ADR-0005-roles-and-grants.md). The portable unit is the database
   role: grants to roles are declarable, while logins, users and role
   membership stay environment-local. The differing risk model becomes two new
   classes (`grant-widen`, `revoke`), and roles join the ids file because
   dropping one destroys per-environment membership — the generalized identity
   criterion. Implementation targets Phase 4; the ids-file format extension is
   pinned now.

8. **Whether a UI belongs here** — settled; see
   [ADR-0006](ADR-0006-optional-ui.md). The refusal in 14.3 is of a *hosted
   control plane that holds the approval*, not of a screen. An optional local
   companion is admissible under one constraint: every action it takes ends as
   a git commit or an ordinary CLI invocation, and it stores no authoritative
   state of its own. It is placed in Phase 6 because a UI built before the
   typed JSON of 3.1 would have to parse human output or reimplement
   validation, which 14.2 forbids.

9. **How many dialects, and when** — settled by ordering rather than by
   design: depth on the engine already supported comes first (Phase 4), the
   second dialect after it (Phase 5). Breadth is the number every comparison
   table counts and the one competitors built on JDBC-style abstractions win
   outright; matching it is not the goal, because here each dialect is a real
   implementation — a type catalogue, an emitter, introspection, normalization
   and a live suite. The reasoning is with the phase table in 12. What is
   *not* deferred is the abstraction: PostgreSQL's shape continues to be the
   test applied to every model decision, and two known collisions are already
   recorded (function overloading in ADR-0002, default and schema privileges
   in ADR-0005).

10. **The driver supply chain** — a recorded risk that has already come due.
    `tiberius` was chosen for the property in 11.3 (pure Rust, nothing to
    install) and has had no release since 2024-07-19. It pins `tokio-rustls
    0.24`, which resolves `rustls 0.21` and with it `rustls-webpki 0.101.7`.

    On 2026-09-01 the first `cargo-deny` run against that tree reported **three
    vulnerabilities, none of them reachable by `cargo update`**:
    RUSTSEC-2026-0098 and RUSTSEC-2026-0099 (name constraints accepted where
    they should be rejected — certificate validation, which is the decision
    every connection to a production database rests on) and RUSTSEC-2026-0104
    (a reachable panic parsing a CRL, not believed reachable here since no CRL
    checking is configured). Every fix requires `rustls-webpki >= 0.103`, which
    requires `rustls 0.22` or newer, which the pinned driver forbids. The
    exposure is therefore not theoretical and not deferrable by patching: **the
    driver is the fix.**

    Two things make that tractable rather than alarming. The architecture
    already isolates the driver — after the seam was tightened it is named in
    exactly one file, and rows and parameters cross the boundary as this
    project's own types, so a replacement touches `pbps-db` and nothing else.
    And a drop-in continuation exists: `tiberius-ng` keeps the library name, so
    only the dependency line changes, and a trial swap on 2026-09-01 resolved
    all four findings (`rustls 0.23`, `rustls-webpki 0.103`) and passed the
    whole offline suite.

**The live suite of 11.5 is the acceptance test for this change** — a driver is
    exactly the layer whose defects a unit test cannot see — and on 2026-09-01
    it was run: against SQL Server 2025 in Docker, the continuation passed all
    fourteen live tests, the same set and the same result as the driver in use.
    The convergence, ledger, lock, all-or-nothing, probe-accuracy and module
    round-trip invariants therefore hold on it, not merely the offline suite.

    So what is left is a decision, not an unknown. Until it is taken,
    `deny.toml` carries the four findings as documented exceptions with this
    entry as their reason, which is a statement about where the fix lives, not
    about how much they matter.

11. **Whether a universal connection layer belongs here** — settled; see
    [ADR-0007](ADR-0007-connection-strategy.md). ODBC and ADBC arrive sounding
    like an answer to question 10 and to dialect breadth at once, and they are
    only ever an answer to the first. A connection layer replaces `pbps-db` —
    about 300 lines — and none of the type catalogue, emitter, introspection,
    validation or probes that make up the real per-engine cost; three engines
    answer "what objects exist" from three different catalogs, and no
    connectivity standard makes those one query. So a universal layer is never
    adopted to reduce dialect work.

    That left driver maintenance as the one motivation worth testing, and a
    spike on 2026-09-01 settled it against ADBC for this engine. The SQL Server
    ADBC driver's **source is not published**: the repository carries only a
    README and a licence, and that licence is the Permissive Binary License —
    binary redistribution from a single vendor's CDN, with reverse engineering
    forbidden. A tool whose claim is that the reviewed plan is exactly what runs
    (7.3) cannot have an unauditable binary execute the statements, and the
    vendor CDN is the "cloud registry in the loop" that 1.1 and 14.3 refuse.
    The maintenance argument also inverts: a stale open-source crate can be
    forked, which is what the escape hatch in question 10 is, while a
    proprietary binary cannot. The behavioural question — whether ADBC honours
    7.5 — was never reached and no claim is made about it.

    The Foundry's MySQL and PostgreSQL drivers *are* Apache-2.0, so this is a
    finding about one driver rather than about ADBC; but for those engines the
    healthy pure-Rust drivers remove the motivation. Dialect plugins are
    declined separately and for unrelated reasons (no stable Rust ABI, and a
    plugin API would freeze `ChangeSet` while the model is still moving).

---

## 14. Usability gap review

This review compares the **workflow**, not merely the object checklist. Atlas has
a strong lint / policy / CI story and a guided migration workflow; Skeema makes
the common inspect-and-push loop deliberately small and supplies practical lints
and workspace validation; Bytebase reaches reviewers who never open a terminal;
Flyway is trivial to start. pbps is already stronger where its product thesis is
strongest — explicit identity, reviewable saved plans, per-environment state and
air-gapped operation — but several ordinary tasks still require the user to
understand the architecture before they can succeed at all.

Reviewed against Flyway, Liquibase, Atlas, Bytebase and Skeema as of
2026-09-01. Three findings drove changes elsewhere in this document rather than
rows below: the ordering of Phases 4 and 5 (12, open question 9), the terms on
which a UI is admissible ([ADR-0006](ADR-0006-optional-ui.md), open question
8), and two refusals that had not been written down where a user would see
them — ORM-model loaders and the distinction between a screen and a control
plane (14.3).

Competitor capabilities change, so this is a **point-in-time product review, not
a compatibility contract**. A capability belongs here only when it makes pbps
easier to adopt or safer to operate *without* introducing a hosted control
plane, making a database mandatory for offline work, or creating a second path
around the typed plan and its checksum.

### 14.1 The gaps

| User job | Current friction | Proposed capability | Priority |
|---|---|---|---|
| Start a project from an existing database | The user must create `pbps.yml`, choose paths, discover the `pull` workflow and work out when to snapshot | `pbps init` detects an empty or new project, asks at most for dialect and environment, previews every file, then writes config + declarations + ids atomically; `--from <env>` chains `pull` and prints the exact next commands | **P0** |
| Find out why setup fails | Connection, engine version, permissions, paths and ledger readiness each fail later, at a different command | `pbps doctor [--env <name>]` checks what only a connection can answer: reachability, dialect and edition, the minimum permissions, ledger and lock access, whether the environment is mid-deployment, and optionally Docker for 9.3. It runs `validate` rather than reimplementing it. Every failure carries a copy-pastable remedy and connection strings stay redacted | **P0** |
| Understand a plan without reading SQL | The typed plan exists, but the first human-facing artifact is effectively plan.sql | Every `plan` prints a stable summary grouped by table and risk; `pbps explain --plan plan.json` answers **what**, **why**, the risk classes, the probes that will run, the execution mode (transactional or staged, see ADR-0003) and whether the target is mid-deployment — all without a connection, because the reviewer at the deployment gate may not have one | **P0** |
| Diagnose CI in the code-review UI | Span diagnostics are good locally, while CI users must open raw logs; and only `verify` currently separates a finding (exit 2) from a tool failure (exit 1), so `plan --check`, `fmt --check` and `validate` report both the same way | Every read-only command supports the same `--format human\|json` and emits the *same typed findings*, with file and line spans. Exit codes are standardized as success / finding / tool failure. **Vendor-native annotations are produced by a converter in `scripts/`, never by the binary** — see 14.3 | **P0** |
| Discover the declaration format while typing | Users move between YAML and this document, and a misspelled key is found only by running `validate` | Ship versioned JSON Schemas for `pbps.yml` and the declarations, **generated from the loader's own types** so the two cannot drift, plus a `pbps schema` exporter for air-gapped editors, shell completions and generated man pages | **P0** |
| Apply organization-specific safety rules | Built-in validation cannot express local naming, size or change-window rules | A declarative `policies:` block selects built-in rules and severities. Suppression requires a rule id, a reason and an optional expiry; no embedded code in v1 (14.3). `validate --since` evaluates only changed objects, so a large estate can adopt it gradually. The block is a new format surface and gets an ADR before it is built, as reference data and roles did | **P1** |
| Know whether a change is operationally expensive | Risk says whether a change *can* fail, never how long it may block or how much it may rewrite | Connected `plan` adds an **estimate**, kept apart from correctness: row and page counts, likely scan or rebuild, lock class, and a confidence. A threshold may *tighten* the gate only when the threshold itself is declared in a reviewed file in the repository; an estimate never loosens one and never reclassifies a dangerous operation as safe. ADR-0003 rules out inferring *behaviour* from table size, and this does not reopen it: the estimate informs a human | **P1** |
| Recover from a change that applied successfully and turned out to be wrong | Git plus the ledger holds the answer, but reconstructing the historical declarations is manual | `pbps state show / diff / export <id>` exposes the ledger and writes a historical state back out as declarations. Recovery is then `export` → commit → `plan --db` → `apply`: because what changes is the **declarations**, git and the database go back together, and the next plan does not try to undo the recovery. There is no one-step rollback and no bypass around the probes or the gate (14.3) | **P1** |
| Bootstrap CI without transcribing documentation | The example pipeline in 10 must be translated by every team | A complete, copy-pastable pipeline per platform lives in the documentation, with the required secrets listed. A generator is deliberately *not* shipped: a generated pipeline that has since been edited can never be upgraded, so the generator ends up maintained for nobody | **P1** |
| Be warned about a hazard the risk class does not name | Risk answers whether a change *can* fail; a competitor's lint catalogue also names *why* — a narrowing that depends on the data already stored, an add that will be rejected by existing rows, a change that breaks a reader still deployed | Widen the built-in analyzer catalogue over the typed ChangeSet, keeping the existing split: `Change::intrinsic_risks()` for what needs no dialect, dialect-computed findings attached to `PlannedChange::risks`. Every finding stays structured data with a stable id, so `policies:` can raise or lower its severity and `explain` can print it. This is catalogue depth, not a new mechanism — it must not become string inspection of emitted SQL | **P1** |
| Take part in a review without a terminal | Every artifact is reachable only through the CLI, so a DBA, an auditor or a release manager either learns it or is briefed second-hand by someone who has | An optional local UI ([ADR-0006](ADR-0006-optional-ui.md)) renders the typed JSON of the read-only commands, composes intent as a commit, and triggers the same checksum-pinned plan. It holds no state and never holds the approval — the audit trail stays git plus the ledger | **P2** |
| Wire pbps into a pipeline that stays upgradeable | The documented pipelines of 10 are copy-pasted, and a copy cannot be upgraded — the same objection that rules out a generator | A first-party, versioned CI component (a GitHub Action, a GitLab CI template) wrapping the existing commands and their exit codes. It differs from a generator in the one way that matters: it is *referenced* by version, so a fix reaches every user, and it adds no capability the CLI lacks | **P2** |
| Assert domain invariants beyond structural convergence | The derived probes cover the hazards a change implies; teams also have rules no diff can imply ("every order has a customer") | A later `tests:` format runs read-only SQL assertions in the optional dev database and at target pre-flight. It is **deliberately separate from the probes of 7.5**, which are derived from the typed ChangeSet and are never replaced by hand-written ones — a probe nobody remembered to write is a probe that does not exist. `tests:` covers what no diff can imply, cannot mutate data, and gets an ADR before it is built | **P2** |

### 14.2 The recommended first slice

Phase 3.1 should be delivered as one end-to-end journey rather than ten isolated
flags:

```text
pbps init --from prod
  -> declarations + ids + config (atomically)
  -> "run pbps doctor --env prod"

pbps doctor --env prod
  -> readiness report (9.5)
  -> "run pbps plan --db ..."

pbps plan --db ... --out plan.json --sql plan.sql
  -> a five-line summary and the exact approval command

pbps explain --plan plan.json
  -> the reviewer's explanation, no credentials required (9.6)
```

Implementation status: the `init` link of this journey is built, including
`--from`, staged round-trip validation, an every-file preview and installing
`pbps.yml` last. The typed findings envelope and the three exit codes of 9.7 are
built and cover `validate`, `fmt`, `plan`, `verify`, `status` and `explain`. The
plan summary and `explain` (9.6) are built, and `doctor` (9.5) with them. The remaining Phase 3.1 links are in
progress.

Acceptance criteria for that slice:

1. A user with a supported existing database can reach a reviewable plan without
   opening this specification.
2. No successful onboarding command partially rewrites the working tree: output
   is staged, validated, then renamed into place atomically.
3. Every diagnostic names the failing environment or file, says what to do next,
   and redacts connection strings.
4. The human, JSON and CI-annotation views describe the same typed findings; a
   frontend never reimplements validation logic.
5. Interactive convenience is optional. Every prompt has a flag, no flag ever
   supplies rename or drop intent (14.3), and a run with no TTY stays
   deterministic.
6. Generated artifacts carry the pbps version and the schema version, so editor
   assistance and templates cannot silently drift from the installed binary.

### 14.3 Product guardrails

The easiest product is not the one with the fewest confirmations; it is the one
whose safe path explains itself. Each boundary below refuses a path that would
be **shorter but would bypass the typed plan, the checksum, a human's recorded
intent, or the git audit trail**. They are recorded here because a refusal is
harder to reconstruct later than a feature: without them, every one of these
arrives again as a reasonable-sounding request.

- **No `push` shortcut.** `plan` then `apply --plan` stays visible. Collapsing
  the two would leave nothing for a human to have read, which is the whole
  difference between this and a schema-sync tool — and if a shorter path
  existed, it would become the path everyone actually uses, and the reviewed one
  would quietly die.
- **Rename suggestions, never rename decisions.** Similarity may *order the
  candidates* in the interactive prompt of 6.3, one pair at a time. Identity
  intent is recorded only by a human, and **no non-interactive flag may supply
  it**: a confirmation that can be written once into a CI file or a shell alias
  has stopped being a confirmation. Whichever way it is given, the artifact is
  the same — one entry in the ids file, in git, reviewed in the merge request,
  which is what still exists when prod deploys that rename five versions later.
- **`revert`, not rollback.** The ledger holds every past state, so a historical
  state can be exported and applied — as a **new forward plan** through the
  ordinary gate, the way `git revert` writes a new commit rather than rewriting
  history. It restores *structure*: a column that was dropped comes back empty,
  and the tool says so at the point of use rather than in a footnote nobody
  reads at three in the morning. It is never one step, because the plan run in a
  panic must not be the least reviewed one.
- **No policy SaaS dependency.** Policies, suppressions, schemas, annotations and
  reports are files or stdout, and they work air-gapped. Beyond the adoption
  argument of 1.1 there is a structural one: the gate's granularity is *this
  reviewed plan*, and a policy living outside git would be a second gate that
  nobody reviewed and that can change an outcome without anyone noticing.
  **What this refuses is a control plane that holds the approval, not a
  screen**: an optional local UI is admissible on the terms of
  [ADR-0006](ADR-0006-optional-ui.md) — it renders the typed JSON, composes
  intent as a commit, and stores nothing authoritative. The test is the same
  one: after it is switched off, is every decision still in git?
- **One source of truth for the declarations.** The declaration files are it.
  Reading the desired schema out of an ORM's models instead — the loader
  ecosystem that is a competitor's main adoption engine — is refused, because
  the second source wins every disagreement silently: identity, drop reasons
  and `strategy:` have nowhere to live in a model class, and the reviewable
  artifact stops being the thing that is deployed. Generating declarations
  *once*, from a database (`pull`) or by hand, and then owning them, is the
  supported path.
- **No second plugin execution engine.** Organization-specific orchestration
  stays in CI and in the exec hooks; built-in policy stays declarative and
  bounded. The test is one question: **does the extension need to run between
  "this plan was approved" and "these statements executed"?** If it does, it is
  refused — it makes the checksum describe something other than what runs (this
  is why ADR-0003 rules out Skeema's `alter-wrapper` path), and anything it
  changes in the database outside the declarations becomes permanent drift that
  the next plan will try to remove. Everything before a plan or after an apply
  is already supported.
- **Progressive disclosure.** The default output is a short result plus the next
  command; `--verbose`, JSON and `explain` reveal the identities, probes,
  normalization and checksums underneath when they are wanted.

This ordering is deliberate. Broadening the object model improves coverage, and
Phase 3.5 did exactly that — but `init` / `doctor` / `explain` improve the first
hour and every failure after it. The same argument, applied twice more, produces
the order in 12: usability (3.1), then the depth of what one engine can express
(4), then the second dialect (5), then the screen that makes all of it legible
to someone without a terminal (6). Each step is refused a place earlier than
that for the same reason — it would be built against a surface that is still
moving, by users who cannot yet succeed at the step before it.

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
