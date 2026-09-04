# Status

Where the product stands, what it can do today, and what is deliberately left
open. The reasons behind the choices are in [DECISIONS.md](DECISIONS.md); the
bugs behind the scar tissue are in [PITFALLS.md](PITFALLS.md).

## Phase

**Phases 0-3.5 and 3.1 complete** for SQL Server. The test and clippy bar is in
CLAUDE.md's "Development environment"; counts change too often to record here.

First-run: `init` (`--env` / `--from` / `--url-env`), with staged validation
and pbps.yml installed last so a failed onboarding run leaves no partial project.

Declarations may carry reference data (`data:`, ADR-0004), planned offline and
against a target alike; `max_data_rows` in `pbps.yml` sets when `validate` says
a block has stopped looking like reference data.

Declarations may carry database roles and their grants (`role:`, ADR-0005);
membership stays each environment's own.

Offline: `plan` (`--check` / `--since` / `--base` / `--out` / `--sql` / `--dev`),
`validate` (`--since`), `fmt` (`--check`), `rename`, `rename-table`, `rename-role`, `drop`,
`drop-table`, `drop-role`,
`docs` (`--format` / `--out` / `--title`), `explain` (`--plan`), `doctor`
(`--env`), `schema` (`--kind`), `completions`, `man`. Every read-only command
takes `--format human|json`; `--no-input` is global.

Connected (each takes `--db <connection string>` or `--env <name>`): `pull`
(`--force` / `--data`), `plan --db` (`--staged`), `apply` (`--plan` /
`--checksum` / `--allow` / `--staged` /
`--resume`), `verify` (`--format json`), `snapshot` (`--force`), `baseline`
(`--reason`), `bootstrap` (`--sql`), `state prune` (`--keep`), `unlock`,
`status` (`--format json`).

All three intent channels now exist: the CLI commands, the YAML annotations, and
the TTY prompt of SPEC 6.3.

## Live tests

**Live tests**: the SPEC §11.5 invariants plus the Phase 3, 3.5 and 3.1 ones (the
ledger round-trip, the lock admitting one holder, a failed statement rolling the
whole plan back, the rename-impact queries, the probes counting real rows, a
staged checkpoint surviving `state_json`, a cross-schema rename stopping at the
name its statement declared, the module round-trip through
`sys.sql_modules`, `doctor` reading a real edition and permission set, and the
readiness check against a **real least-privilege login** created and granted
inside the test container — `sa` holds `CONTROL` and short-circuits the whole
permission list, which is how three permission bugs survived the first live
test — and the reference-data path: the DML, the row read-back, drift on rows,
the pre-delete probe's dynamic SQL, and the binary end to end through
`bootstrap`, `verify`, `plan --db`, `apply` and `pull --data`; and a plan
that *creates* a table with a foreign key, applied through the real gate —
the shape the apply guard got wrong twice because nothing here applied one)
run
against a real SQL Server in Docker:
`scripts/live-tests.sh` (set `PBPS_TEST_PORT` if 14330 is taken; the engine is
pinned by digest there and in CI), or set
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

## Open items

### Artifact format versions reset at the first release

The plan file, the state snapshot and the published editor schemas are each
several versions in, and the snapshot reader carries an upgrade path from the
older ones — but this tool has never been released: the workspace is `0.0.0`
and there is no tag. Those numbers therefore record a history nobody has, and
the upgrade path leads from versions no deployment ever wrote. **At the first
tagged release, reset `plan::CURRENT_VERSION`, `state::CURRENT_VERSION` and
`integration::SCHEMA_VERSION` to 1 and drop `state::OLDEST_READABLE_VERSION`'s
back-compatibility with the pre-release numbering** (DECISIONS 145). Until
then, bump freely: a plan file lives for the length of one deployment window,
and there is nothing in the field to invalidate.

### Return the driver to `tiberius` once it ships a release

(SPEC open question 10.) The driver is `tiberius-ng`, adopted because
`tiberius` 0.12.3 pins a `rustls` stack with four open advisories and had no
release since 2024. The original crate has since moved to the community-owned
`tiberius-rs/tiberius` repository and is active again (commits on 2026-09-02),
so the plan is to go back — after a crates.io release newer than 0.12.3 whose
`rustls` feature resolves `rustls >= 0.23`. As of 2026-09-04 there is no such
release and `main` still pins `tokio-rustls 0.24`, so moving back now would
reinstate every advisory exception. The move is one line in the workspace
`Cargo.toml`, then `cargo deny check`, then the live suite.

### No universal connection layer

(ADR-0007, open question 11.) ODBC and ADBC
sound like an answer both to that item and to dialect breadth; they answer only
the first. A connection layer replaces `pbps-db`'s ~300 lines and none of the
type catalogue, emitter, introspection, validation or probes. ADBC is refused
for SQL Server specifically: its driver's source is not published and its
licence forbids reverse engineering, which a tool claiming "the reviewed plan is
exactly what runs" cannot accept — and a proprietary binary cannot be forked,
so it is *less* recourse than the stale crate, not more. Dialect plugins are
declined separately: no stable Rust ABI, and a plugin API would freeze
`ChangeSet` while the model still moves.

## Roadmap

**Phase 3.1 is complete** — the usability foundation of SPEC 14, and every P0 row
of 14.1. It was placed ahead of the next dialect deliberately: broadening the
object model improves coverage, but these improve the first hour and every
failure after it.

**Phase 4, depth on SQL Server before breadth across engines**, is complete as
scoped: declarative reference data (ADR-0004), roles and grants (ADR-0005),
the `policies:` block and the first built-in analyzer catalogue (ADR-0008).

Reference data is **built**, both halves: the `data:` block, its `exact` and
`ensure` modes, the round trip through `fmt`, the rules `validate` reports, the
typed row changes with the `data-update` and `data-delete` risk classes, the
DML and its ordering; and against a target, the row read-back into `state_json`
under the recorded or declared scope, the row half of the drift comparison, a
saved plan that carries which rows the recorded state must cover, the
pre-delete probe counting the rows that still reference the row, and
`pull --data`. ADR-0004 lists the decisions taken on the way under
"Implementation status".

Roles and grants (ADR-0005) are **built**: the `role:` file, `r_` uids in the
ids file, `rename-role` / `drop-role` and the other two intent channels, the
differ with `revoke` (gated) and `grant-widen` (labelled, never gated), the
T-SQL, the catalog read-back, drift under the managed set, `validate`'s
target rule and `pull`. ADR-0005 lists the decisions taken on the way.

The `policies:` block and the first analyzer catalogue (ADR-0008) are
**built**: rules and suppressions in `pbps.yml`, `validate` (with `--since`)
at the declaration point, `plan` and `plan --db` at the plan point with the
findings carried in the saved plan, and refusal of the plan at `error` before
anything is written. ADR-0008 lists the decisions taken on the way.

Depth before breadth was chosen against the obvious ordering — engine count is
what every comparison table measures — because a second dialect doubles the
surface every later feature is built twice for, and does it while the first
engine still cannot express an organization's own rules. The reasoning is in
SPEC 12 and open question 9.

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
reimplement validation. **Scope is single-user and local, and that is the
open-source boundary** — multi-tenant or hosted is reserved as a possible
commercial offering and gets its own ADR. It does not inherit permission from
ADR-0006: commercial pressure pushes hardest towards the UI holding the
approval, which is the one thing ADR-0006 refuses.
