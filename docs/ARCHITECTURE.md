# Architecture

Crate responsibilities and model constraints used when changing the codebase.
See [SPEC §11](SPEC.md#11-architecture) for the overall design and
[DECISIONS.md](DECISIONS.md) for the reasons behind individual choices.

## Architectural boundaries

```text
pbps-model     Domain model. Dialect-agnostic, span-free, serializes to JSON:
               the ids file, the state snapshot, the saved plan, the drift report
pbps-config    Project configuration (pbps.yml): paths, environments, hooks; and the
               text-only `pbps test` manifest (no filesystem, no git)
pbps-load      YAML -> model, from a directory or from files already in memory; the only
               crate that may depend on serde-saphyr
pbps-diff      model <-> ids comparison -> immutable PlanningPlan. Produces no SQL. Also owns
               the managed-set scope and observed identity
pbps-dialect   Dialect abstraction. Pure: types, validation, emit, preflight
               probes, shared operational row-work answers. Connection-bound
               work is free async fns in the dialect
               crate, not trait methods; pbps-cli::engine routes to them.
               Lowering takes the engine's associated input and returns immutable
               operation groups branded by that emitter (DEC-1959.5)
pbps-mssql     SQL Server: type catalogue, validation, the T-SQL emitter (the
               only place a change becomes SQL), catalog introspection,
               the ledger/lock statements, rename impact. Depends on pbps-diff
               for sealed plan emission inputs, as pbps-pg does (DEC-1959.5)
pbps-pg        PostgreSQL: the same responsibilities as pbps-mssql, as far as
               Phase 5 has built them. Everything unbuilt refuses by name
pbps-db        Connections plus transaction framing. Owns "there is a network";
               the shapes a connected command consumes — ledger types and
               prune policy, what a pull found (`catalog`), what a rename
               touches (`impact`), what `doctor` asks (`doctor`) — and no
               engine's SQL. One module per driver, and nothing outside them
               names one. Driver isolation: see constraint 9
pbps-docs      Markdown / self-contained HTML / Mermaid ERD from the model.
               Pure: no dialect, no connection, no configuration
pbps-ui        Loopback UI over fixed CLI subprocess commands; currently read
               views. Owns envelope consumer types and the embedded page.
               Planned compose orchestration is defined below; no other
               workspace crate dependency (ADR-0015 decision 6)
pbps-cli       clap, diagnostic output, the deployment commands, exec hooks.
               output is the one typed findings envelope every read-only
               command speaks; prompt is the TTY intent channel of SPEC 6.3.
               engine is the connected seam: one function per question a
               command asks a database, routed to pbps-mssql or pbps-pg by
               the connection's driver (DECISIONS 417)
```

- Only `pbps-db` and the engine modules that take a `Conn` (`catalog`, `state`,
  `impact`, `doctor`, `edition`, `estimate`, `dependencies`, `resolver` on SQL Server; `catalog`, `state`, `impact`,
  `doctor`, `roles`, `modules`, `data_triggers`, `staged`, `statement`, `estimate`, `resolver`, `assertions` on PostgreSQL) are async; the CLI `block_on`s
  them per command, through `engine`.
- Two places in `pbps-cli` name an engine to *choose* it: `dialect_for` (the
  pure `Dialect`) and `db::driver_for` (the driver). `engine` names both to
  *route*, by what the connection turned out to be, and nothing else in the
  crate names either. A third engine is a compile error in every `engine`
  function until it has an answer for each.
- `spikes/` is workspace-`exclude`d: evaluation crates, not product code.
- Pure module and RLS policy dependency facts live in `pbps-diff::order::catalog`.
  PostgreSQL owns their reads and re-exports the shared value types. The
  canonical order route selects and settles policy-owner releases before
  granting execution authority; this adds no crate dependency (DEC-1959.5).
- Schema semantics and typed read reports reach the synchronous UI only through
  `current_exe()` run with `--no-input`. CLI supplies its launch token and the
  documentation style hash. The shipped viewer reads no project files itself;
  planned compose may capture opaque bytes as specified below, but never parse
  YAML/project configuration or link the renderer, loader or model.
- The dialect supplies transaction statements; `pbps-db` owns the transaction
  framing. See [ADR-0014 §2](ADR-0014-driver-seam-tested.md#2-begin-holds-t-sql-in-the-crate-that-is-documented-to-hold-none)
  for the boundary correction.

The deployment compiler boundary (DEC-1959.5) retains the successful differ's
sort/rename provenance and the completed resolver/shared scheduling routes.
`order::deployment` owns their pure declaration/catalog scheduling policy;
`order::catalog` holds neutral facts re-exported by the PostgreSQL read seam.
CLI retains catalog I/O, re-diff timing, planning probes and refusal reporting.
It cannot replace an executable payload with a raw draft. The shared stages,
connected continuations and resolver remap keep their existing sequence.

`SavedPlan` remains an untrusted model wire DTO. Its `wire_fingerprint` names
bytes for admission/attempt identity; order's sealed `ArtifactView` supplies
the writer and executable checksum. Admission preserves the approved wire
order without reordering it. Metadata/proof capabilities are never serialized.
Explain keeps descriptive validation separate from execution admission:
preview statement counts are engine metadata observations, never lowered SQL.
Certified inputs bind their mode; lowering reads it and sealing checks it.
CLI keeps concrete engine variants through immutable lowering and all apply
modes, so a caller-defined emitter cannot substitute an executable collection.
Lowering prepares an engine's plan-local position function once per emission,
then keeps its operation IDs, origins and indivisible groups. Statement-count
loops reuse the same preparation; context never becomes saved-plan data.
The normal `pbps-mssql -> pbps-diff` edge remains the only added workspace
crate dependency; no transport/engine dependency enters diff or dialect.

The symbolic partition contract (DEC-2296.1) keeps neutral property/version,
membership and ownership-transfer data in `pbps-model::operation::symbolic`.
An operation-local engine hook may describe recursion over membership at
execution time without enumerating endpoint children. `pbps-diff::order`
owns its cut compiler, non-executable projection and independent replay.
The contract PR enables no guarded consumer; engine production, pending
normalization and probe integration remain separate changes. A borrowed
projection cannot mint PlanView authority or serialize new saved-plan data.
No additional settlement or proof union is introduced. See
[the contract](ORDERING.md#membership-scoped-effect-contract) for unresolved
domains, remaining guards, lock-window reporting and readback obligations.

A transient annotation contract in `pbps-diff::order::annotations` binds the
model registry's references through the existing Owners/PlanNames boundary.
It describes current operation boundaries and accompanies drafts through the
existing settlements; it owns neither engine SQL nor a new ordering pass.
Legal decomposition delegates to the dialect's existing typed recipe. The
contract is not saved-plan data (DEC-2469.3).

## New-engine ordering contract

An engine supplies ordering facts and lowering semantics to the shared
compiler; it supplies no deployment reorder pass. PG scratch uses order's
branded trial program to choose immutable whole groups after engine refusals,
within fixed ranges and hard readiness (DEC-2465.1). Metadata is published only
on acknowledged commit, and completed observations gate assessment. The contract and pipeline are
in [ORDERING](ORDERING.md#adding-an-engine) (DEC-1959.6).

- `Dialect::engine_effects` adds pure, operation-local effects to the portable
  effects. Baseline/desired snapshots establish ownership and the change's
  other end, not substitute payloads.
- The shared [`pbps_dialect::slots::slots`](../crates/pbps-dialect/src/slots.rs#L366)
  mapper uses the engine's `Dialect::shares_namespace_with_tables`,
  `Dialect::indexes_share_namespace_with_tables`,
  `Dialect::constraints_share_namespace_with_tables` and `Dialect::overloads`
  hooks to map addresses into namespaces. Slot equality is a catalog fact
  under its identifier/collation rules. Unknown equality is reported/skipped,
  never interpreted as inequality.
- Engine catalog I/O supplies holders, bindings, dependencies, releases and
  capabilities as neutral facts. Unread and read-empty remain distinct;
  `pbps-diff::order` owns their pure constraints and structural application.
- The emitter supplies `Statement` transaction and batch requirements,
  intermediate rename/create/release names and execution metadata. Lowering
  retains source IDs/origins and indivisible statement groups, branded by the
  concrete engine. No consumer infers this metadata from SQL. Deployment retains
its certified order; PG scratch alone follows order-issued trial tokens
(DEC-2465.1), without permuting the lowered collection.
- Engine-specific obligations use typed expansions with equivalence,
  per-part risk, targeted progress and budget checks. Canonical scheduling,
  scratch blueprints and connected continuations finish inside order; only
  certified input reaches rendering, sealing and execution.

Ordinary transactional plans refuse non-transactional statements
([`reject_non_transactional`](../crates/pbps-cli/src/deploy.rs#L9907)).
A staged plan contains exactly one logical change
([planning limit](../crates/pbps-cli/src/deploy.rs#L7600)); its lowered
statements run in order, each committed, under the existing checkpoint and
resume protocol ([staged executor](../crates/pbps-cli/src/deploy.rs#L8908),
ADR-0003). A new engine whose single change lowers to several non-transactional
statements uses this staged route and must make each statement resumable or
idempotent under that protocol. Several logical changes needing
non-transactional execution remain refused until a separate multi-transaction
design exists. Batch boundaries do not authorize commits.
The release verifier remains off; tests/debug replay and live commutation
checks remain complementary guards, with mutation audits exposing missing
effects. This contract adds no dependency, rank, tie-break, settlement stage,
constraint union or saved-plan field.

## Isolated compose

[ADR-0017](ADR-0017-isolated-compose.md) is implemented and qualified (#745–#748).
The viewer enables it on Linux (#494) through fixed, gated `POST /api/compose/<action>`
requests, which are its only writes. Raw file capture is an explicit
orchestration responsibility, not permission to move schema interpretation into
the UI (#750).

| Responsibility | Owner and boundary |
| --- | --- |
| Project/path resolution, declaration meaning and identity intent | Existing CLI commands and their existing config/load/diff dependencies. The UI asks the running executable; it does not interpret YAML, infer identity or perform schema validation. |
| Raw Git/input capture | `pbps-ui` orchestrates guarded Git subprocesses and no-follow regular-file reads within the CLI-resolved project/input scope. Files are opaque bytes plus path/mode/membership metadata. Path/type admission protects capture; it does not decide schema validity. |
| Candidate tree and diff | `pbps-ui` uses Git raw objects and a private index to construct the exact candidate and invokes the ordinary CLI against its isolated snapshot for intent resolution and validation. The source index/HEAD/files are not written. |
| Candidate identity and storage | `pbps-ui` allocates and seals the operation/output-ref identity before showing the preview, owns bounded private storage and serves opaque candidate handles. The browser cannot supply replacement evidence or persist authority. |
| Publication, receipts and recovery | `pbps-ui` orchestrates the guarded Git ref/push operations and private operational evidence under ADR-0017. Git holds the authoritative commit/ref result; records carry credential-free identity and recovery obligations, never schema truth or approval. |
| Authentication | Existing approved environment/Git helper boundary, reacquired per invocation. The UI has no credential form or durable credential store; unsupported endpoint forms refuse before candidate sealing. |

These responsibilities require no dependency from `pbps-ui` to another workspace
crate and no YAML/model parser in the UI. Its own JSON types describe CLI reports
and operational candidate/recovery metadata, not a second schema model. Any new
semantic question must gain a CLI subprocess answer, not a UI-side validator.
Database transport and deployment remain outside this compose flow.
The #745 backend uses `doctor --paths-only` to establish contained input paths
before schema loading and `plan --no-dev` when it needs to resolve ids without
the optional rehearsal. Its cryptographic digest and Linux filesystem/process
dependencies do not interpret project/schema data. Node.js is a development
test runtime for the shipped browser module, not a product dependency.

## Planned engine-assisted planning

[ADR-0016](ADR-0016-engine-assisted-planning.md) and
[SPEC §9.3.2–9.3.3](SPEC.md#932-engine-assisted-planning-postgresql-planning-and-apply-delivered)
are accepted design; initial read-only `doctor` environment discovery (#597),
resolver selection/policy (#606), engine compatibility qualification (#610,
#611) and the resolver's server and Docker scratch runs (#1672, #1674) are
implemented, and PostgreSQL planning publishes sealed binding evidence (#1516)
that `explain` shows from the plan (#1517), and `apply` rechecks it from the
target connection alone (#616). The design
preserves the boundaries
above: CLI owns resolver lifecycle and reporting; the engine crates own
environment queries, compatibility rules, scratch DDL and binding extraction;
`pbps-db` owns transport, not SQL or provisioning. Connected work continues
through `pbps-cli::engine`, not I/O on the pure `Dialect` trait or in the differ.

Shared deterministic plan evidence belongs beside semantic `Schema`, never in
its equality or as driver-specific types. Only order's immutable OrderedPlan
or a separately admitted, reviewed wire artifact reaches deployment SQL
emission (DEC-1959.5). Target reads and scratch writes use
separate connections and credentials. The resolver needs only another
database, empty, behind another credential variable, or a container it starts
through the runner's `docker` command; the operator vouches for its isolation
(DEC-1528.1, DEC-1636.1). Separation is decided by the engines' own identity
before any scratch write, not by names. pbps enforces no containment and
observes no process, so the resolver reaches its scratch with SQL alone and
its [acceptance matrix](RESOLVER-ACCEPTANCE.md) separates live qualification for planning, offline explanation and resolved apply from build portability.
`apply` checks saved prerequisites without invoking the resolver or adding
changes after approval.

Environment discovery/recommendation/compatibility covers PostgreSQL and SQL
Server first. Binding adapters follow separately: PostgreSQL first, SQL Server
after engine-specific design and live tests. An unimplemented capability is
reported explicitly, never supplied by another engine's assumptions.

The initial discovery report lives in `pbps-db::resolver`, alongside other
connected report shapes. Its serialization/schema derives describe advisory
observations, not persisted plan evidence. Engine `resolver::discover` functions
own catalog SQL and candidate-family suggestions; CLI `engine` dispatches and
`doctor` renders. Discovery has no verified state, provisions nothing and
does not select dependencies. Its partial inventory cannot be reused as an
ADR-0016 coherent evidence capture (DECISIONS 492).

Named resolver configuration lives in `pbps-config::resolver`. Its tagged source
profiles and pure precedence lookup contain credential-variable names only.
CLI exposes the unacquired selection as optional command-summary data, never
as model evidence or acquisition/compatibility proof (DECISIONS 494).

`pbps-db::transport` holds the sealed query-connection seam engine-owned
resolver SQL runs through, implemented by the ordinary connection only. Only
the private driver modules interpret connection options or name driver types.
It contains no SQL or provisioning (DECISIONS 495).

Scratch reconstruction consumes immutable logical planning input. Its engine
owns an opaque SQL-free blueprint; order applies the existing function/phase
and generated settlements separately. Only the completed engine-branded
scratch input reaches its private lowering adapter. Private probes preserve
lexical metadata and refusal timing without retaining SQL before settlement.
The compiler prevents a fake engine or emitter from supplying the real
engine's scratch/lowered collection (DEC-1959.5).

The resolver's run lives in `pbps-cli::resolver::server::scratch`: it reaches
a supplied scratch server through its connection string, or starts and
removes a Docker container through the runner's `docker` command, and owns the
scratch resources' lifecycle. Its per-engine scratch and authorization SQL
stays in the engine crates (`pbps-pg::resolver`, `pbps-mssql::resolver`)
behind the sealed query-connection seam. Capture refusals and comparison
reports use `pbps-db::resolver::capture`; opaque PostgreSQL catalog
capabilities remain engine-owned private state, as with the engine-only state
in DECISIONS 417. SQL Server capture refuses by name until implemented. The
measured runtime layer — Docker API ownership, native process and channel
leases, kernel containment and executable identity — was removed by #1636
(DEC-1636.1).

Certified emission uses `PlanDialect`, while object-safe `Dialect` remains the
semantic interface. Ordered, admitted and sealed inputs retain the actual
planner/checker type; canonical completion cannot relabel that type. The CLI
dispatches to its selected concrete engine at the existing selection point and
keeps that type through sealing, lowering and execution. Its lowered collection
retains the engine and immutable configuration. Singleton fixture support is a
non-default diff feature enabled only by dev-dependencies; shipping builds have
no unchecked certificate factory (DEC-1959.5).

## Inviolable constraints

Keep these numbers stable: other documents cite them. Where a rule already
has a home, the entry links there instead of repeating its content.

1. **Two semantically identical `Schema`s must be `==`.** No spans, no one-shot
   annotations in the model; normalize type case before comparing.
2. **Containers hold names; elements do not.** `Table` / `Column` have no
   `name` — it is the parent map's key. Functions needing one take `(name, table)`.
3. See the typed `ChangeSet` and SQL layering rule in
   [SPEC §11.1](SPEC.md#111-crate-layout).
4. See the intrinsic and dialect-computed risk split in
   [SPEC §14.1](SPEC.md#141-the-gaps).
5. **Serialization is deterministic.** `BTreeMap` / `BTreeSet` throughout. Sole
   exception: `Table::columns` is an `IndexMap` whose equality ignores order.
6. See [SPEC §7.1](SPEC.md#71-decided-automatically-vs-needing-intent) for human
   intent and [SPEC §6.4](SPEC.md#64-behaviour-without-a-tty) for non-interactive
   behavior.
7. See the identity boundary in
   [ADR-0002](ADR-0002-module-model.md#the-dividing-principle).
8. **Annotations travel beside the model, never inside it.** `strategy:`,
   `depends_on:` and `public_execute:` come back as `Loaded.hints`, or
   constraint 1 breaks. See also
   [decision 16](decisions/identity.md#decision-16).
9. See the driver isolation rule in
   [ADR-0007 decision 5](ADR-0007-connection-strategy.md#decision) and its
   [measured limits in ADR-0014](ADR-0014-driver-seam-tested.md#the-claim-under-test).
   With two drivers it reads **one file per driver**: `pbps-db::mssql` names
   `tiberius`, `pbps-db::postgres` names `tokio_postgres`, and nothing else in
   the workspace names either — `pbps-db::Conn` dispatches between them and
   holds no driver type ([decision 225](decisions/connection.md#decision-225)).

Typed portable creation demands stay in planning state (DEC-2469.4).
`pbps-dialect::creation` selects facets and refines engine-provided recipes using
existing Changes; engine capability and legal group decisions stay in each
dialect. The annotation identity wrapper remains shared above `creation_parts`.
`pbps-diff::order::deployment::schedule::diff_annotated` admits the annotated
comparison only after canonical scheduling; connected computed-call collation
checks remain in the SQL Server planning adapter. No persisted model changes.
