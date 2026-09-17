# Architecture

Crate responsibilities and model constraints used when changing the codebase.
See [SPEC §11](SPEC.md#11-architecture) for the overall design and
[DECISIONS.md](DECISIONS.md) for the reasons behind individual choices.

## Architectural boundaries

```text
pbps-model     Domain model. Dialect-agnostic, span-free, serializes to JSON:
               the ids file, the state snapshot, the saved plan, the drift report
pbps-config    Project configuration (pbps.yml): paths, environments, hooks
pbps-load      YAML -> model; the only crate that may depend on serde-saphyr
pbps-diff      model <-> ids comparison -> ChangeSet. Produces no SQL. Also owns
               the managed-set scope and observed identity
pbps-dialect   Dialect abstraction. Pure: types, validation, emit, preflight
               probes, shared operational row-work answers. Connection-bound
               work is free async fns in the dialect
               crate, not trait methods; pbps-cli::engine routes to them
pbps-mssql     SQL Server: type catalogue, validation, the T-SQL emitter (the
               only place a change becomes SQL), catalog introspection,
               the ledger/lock statements, rename impact
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
pbps-ui        Loopback read views over fixed CLI subprocess commands. Owns its
               envelope consumer types and embedded page; depends on no other
               workspace crate (ADR-0015 decision 6)
pbps-cli       clap, diagnostic output, the deployment commands, exec hooks.
               output is the one typed findings envelope every read-only
               command speaks; prompt is the TTY intent channel of SPEC 6.3.
               engine is the connected seam: one function per question a
               command asks a database, routed to pbps-mssql or pbps-pg by
               the connection's driver (DECISIONS 417)
```

- Only `pbps-db` and the engine modules that take a `Conn` (`catalog`, `state`,
  `impact`, `doctor`, `edition`, `estimate`, `resolver` on SQL Server; `catalog`, `state`, `impact`,
  `doctor`, `roles`, `modules`, `data_triggers`, `staged`, `estimate`, `resolver` on PostgreSQL) are async; the CLI `block_on`s
  them per command, through `engine`.
- Two places in `pbps-cli` name an engine to *choose* it: `dialect_for` (the
  pure `Dialect`) and `db::driver_for` (the driver). `engine` names both to
  *route*, by what the connection turned out to be, and nothing else in the
  crate names either. A third engine is a compile error in every `engine`
  function until it has an answer for each.
- `spikes/` is workspace-`exclude`d: evaluation crates, not product code.
- The synchronous local UI's only project-data source is `current_exe()` run
  with `--no-input`. CLI supplies its launch token and the documentation style
  hash; the UI neither reads configuration nor links the renderer or model.
- The dialect supplies transaction statements; `pbps-db` owns the transaction
  framing. See [ADR-0014 §2](ADR-0014-driver-seam-tested.md#2-begin-holds-t-sql-in-the-crate-that-is-documented-to-hold-none)
  for the boundary correction.

## Planned engine-assisted planning

[ADR-0016](ADR-0016-engine-assisted-planning.md) and
[SPEC §9.3.2–9.3.3](SPEC.md#932-engine-assisted-planning-accepted-not-implemented)
are accepted design; initial read-only `doctor` environment discovery (#597)
and named profile selection/policy (#606) are implemented. The internal Docker
runtime supports acquisition, native target separation and contained private
channels (#608). Engine compatibility qualification and binding evidence remain
planned. The design preserves the boundaries
above: CLI owns resolver lifecycle and reporting; the engine crates own
environment queries, compatibility rules, scratch DDL and binding extraction;
`pbps-db` owns transport, not SQL or provisioning. Connected work continues
through `pbps-cli::engine`, not I/O on the pure `Dialect` trait or in the differ.

Shared deterministic plan evidence belongs beside semantic `Schema`, never in
its equality or as driver-specific types. The final typed ChangeSet remains the
only input to deployment SQL emission. Target reads and scratch writes use
separate instances/clusters, connections and credentials. Engine identity checks
and trusted provisioning evidence must establish separation before scratch DDL;
a different database name is insufficient. CLI lifecycle and engine-specific
profiles also qualify runtime-enforced network/filesystem containment before
any compiled source is sent, plus source handling for external definitions,
including server/container log capture and disposable storage (ADR-0016).
Containment is enforced outside SQL privileges by the qualified runtime, not by
adding a plugin engine or moving provisioning into database transport.
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
ADR-0016 coherent evidence capture (DECISIONS 491).

Named resolver configuration lives in `pbps-config::resolver`. Its tagged source
profiles and pure precedence lookup contain credential-variable names only.
CLI exposes the unacquired selection as optional command-summary data, never
as model evidence or acquisition/compatibility proof (DECISIONS 494).

The verified TLS primitive lives in `pbps-db::transport`; its opaque connection
identity belongs to one successfully authenticated handshake. Only the private
driver modules interpret connection options or name driver types. It contains
no SQL or provisioning and grants no resolver admission: runtime and engine
profiles must qualify every backend hop, separation and run binding before
using its replies as evidence (DECISIONS 495).

The internal Docker runtime layer lives in `pbps-cli::resolver`: local Docker
API ownership and lifecycle remain outside database transport. Its `engine`
routing modules select fixed source-free bootstrap recipes and dispatch the
engine-owned identity queries. `pbps-db::transport::StreamConn` provides only
database protocol over a supplied stream, with no host lookup, reconnect or
claim of peer qualification. It is not a fallback for failed TLS. Engine
identity SQL remains in `pbps-pg::resolver` and `pbps-mssql::resolver` through
the sealed query-connection seam. Native process/channel and kernel leases are
ephemeral CLI capabilities, never semantic schema or saved-plan evidence.
See [the runtime boundary](RESOLVER-RUNTIME.md) for its staged startup,
supported profile and the remaining environment/binding gates.

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
   [decision 16](DECISIONS.md#phase-2--pull-normalization-strategy).
9. See the driver isolation rule in
   [ADR-0007 decision 5](ADR-0007-connection-strategy.md#decision) and its
   [measured limits in ADR-0014](ADR-0014-driver-seam-tested.md#the-claim-under-test).
   With two drivers it reads **one file per driver**: `pbps-db::mssql` names
   `tiberius`, `pbps-db::postgres` names `tokio_postgres`, and nothing else in
   the workspace names either — `pbps-db::Conn` dispatches between them and
   holds no driver type ([decision 225](DECISIONS.md)).
