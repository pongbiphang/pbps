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
               probes. Connection-bound work is free async fns in the dialect
               crate, not trait methods
pbps-mssql     SQL Server: type catalogue, validation, the T-SQL emitter (the
               only place a change becomes SQL), catalog introspection,
               the ledger/lock statements, rename impact
pbps-pg        PostgreSQL: the same responsibilities as pbps-mssql, as far as
               Phase 5 has built them. Everything unbuilt refuses by name
pbps-db        Connections plus transaction framing. Owns "there is a network";
               ledger types and prune policy, no engine's SQL. One module per
               driver, and nothing outside them names one.
               Driver isolation: see constraint 9
pbps-docs      Markdown / self-contained HTML / Mermaid ERD from the model.
               Pure: no dialect, no connection, no configuration
pbps-cli       clap, diagnostic output, the deployment commands, exec hooks.
               output is the one typed findings envelope every read-only
               command speaks; prompt is the TTY intent channel of SPEC 6.3
```

- Only `pbps-db` and the `pbps-mssql` modules that take a `Conn` (`catalog`,
  `state`, `impact`, `edition`) are async; the CLI `block_on`s them per command.
- `spikes/` is workspace-`exclude`d: evaluation crates, not product code.
- The dialect supplies transaction statements; `pbps-db` owns the transaction
  framing. See [ADR-0014 §2](ADR-0014-driver-seam-tested.md#2-begin-holds-t-sql-in-the-crate-that-is-documented-to-hold-none)
  for the boundary correction.

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
8. **Annotations travel beside the model, never inside it.** `strategy:` and
   `depends_on:` come back as `Loaded.hints`, or constraint 1 breaks. See also
   [decision 16](DECISIONS.md#phase-2--pull-normalization-strategy).
9. See the driver isolation rule in
   [ADR-0007 decision 5](ADR-0007-connection-strategy.md#decision) and its
   [measured limits in ADR-0014](ADR-0014-driver-seam-tested.md#the-claim-under-test).
   With two drivers it reads **one file per driver**: `pbps-db::mssql` names
   `tiberius`, `pbps-db::postgres` names `tokio_postgres`, and nothing else in
   the workspace names either — `pbps-db::Conn` dispatches between them and
   holds no driver type ([decision 225](DECISIONS.md)).
