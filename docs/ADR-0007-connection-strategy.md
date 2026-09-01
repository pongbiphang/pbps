# ADR-0007: The connection layer — what a universal driver can and cannot carry

- Status: decided in part. Dialect plugins are declined; a universal connection
  layer is not adopted now, and the ADBC evaluation stays open behind a named
  test
- Date: 2026-09-01
- Related: docs/SPEC.md §11.2, §11.3, §11.5, §12, §13.10, §14.3;
  [ADR-0002](ADR-0002-module-model.md);
  [ADR-0003](ADR-0003-execution-strategy.md)

## Background

Three proposals arrive looking like one idea: **let a universal layer carry the
databases, so the tool does not have to.** They are ADBC, ODBC, and dynamically
loaded dialect plugins in the style of dbt's adapters. Two motivations bring
them, and they need separating because they have different answers:

1. *Breadth* — a second and third engine at a fraction of the cost, which is
   what a cloud-native product needs.
2. *Maintenance* — `tiberius` is stale, and a widely used layer would take that
   risk off this project. Open question 10 records how sharp that has become.

The second motivation is the stronger one and it is not the one usually stated.

## The measurement that applies to all three

A connection layer carries bytes. It does not carry meaning. Measured on this
repository:

| Scope | Lines | Replaced by a universal layer |
|---|---|---|
| `pbps-mssql` — type catalogue, emitter, introspection, validation, preflight, live suite | ~7,000 | no |
| `pbps-model`, `pbps-diff`, `pbps-load`, `pbps-config`, `pbps-docs`, `pbps-dialect` | ~10,700 | no — already engine-agnostic |
| `pbps-cli` | ~7,300 | mostly no |
| **`pbps-db` — connect, query, transaction framing** | **~300** | **yes, all of it** |

Introspection is the clearest case: SQL Server answers from `sys.objects` and
`sys.sql_modules`, PostgreSQL from `pg_catalog`, MySQL from
`information_schema`. Those are three different queries returning three
different shapes, and no connectivity standard makes them one.

**So "adopt a universal driver to get more engines cheaply" is refused on the
arithmetic**: it removes about 4% of what a new engine costs. What it can
legitimately be adopted for is the *other* motivation — a better-maintained
driver — and that is a different decision with a different price.

## ADBC

**Correction, recorded because ADRs are read later and a wrong reason is worse
than none.** This evaluation first claimed ADBC had no SQL Server driver. That
was wrong: it looked only at the driver list in the Apache `arrow-adbc`
repository. The SQL Server driver is published by the **ADBC Driver Foundry**
(adbc-drivers.org), Apache-licensed, each driver in its own repository, and it
**wraps `go-mssqldb`, which Microsoft maintains officially.**

That fact matters more than any argument here, because it speaks directly to
motivation 2: it is a better maintenance lineage than the one open question 10
is about. Microsoft's own migration of Power BI and Fabric from ODBC to ADBC
makes the direction credible rather than speculative.

The costs are real and unchanged by the correction:

- **Distribution.** ADBC drivers are shared libraries, installed with a
  package manager (`dbc`). SPEC §11.3 bought the opposite property — one static
  binary, nothing to install — and it is what separates this tool from
  Liquibase's JVM for the on-prem estates it currently targets. That price was
  set for a world of copying a binary onto a locked-down host; for a
  container-first deployment it is a line in a Dockerfile, and pretending
  otherwise would be dishonest.
- **Trust surface.** Adopting it means a young Rust FFI layer
  (`adbc_core`'s driver manager) plus a Go shared library, in place of one pure
  Rust crate. Each piece is better maintained; there are more pieces.

**The open question, and the test that settles it:** ADBC's centre of gravity is
Arrow-columnar transport for analytics. This tool's central promise is *one
plan, one transaction, all or nothing* (SPEC §7.5), with a staged mode for the
statements that cannot honour it (ADR-0003). Whether the SQL Server driver's
transaction control and arbitrary-DDL execution are faithful enough for that is
not answerable from documentation. **A spike answers it; nothing else does.**
Until then ADBC is neither adopted nor refused.

## ODBC

The same trade with a longer history: Microsoft's `msodbcsql` is
well-maintained, `odbc-api` is a capable Rust binding, and ODBC drivers exist
for nearly everything. It carries the same distribution cost as ADBC and none
of ADBC's momentum, so if the single-binary property is going to be spent, it
should be spent on ADBC.

Worth stating plainly: **pure Rust is not the constraint for the engines the
roadmap names.** `tokio-postgres` and `mysql_async` are healthy. A universal
layer would be adopted for SQL Server's sake specifically, which is a much
narrower claim than "it gets us many engines".

## Dialect plugins

The proposal is dbt's shape: a small core, with each database's specifics in a
loadable adapter. **The architecture already is this** — `Dialect` (SPEC §11.2)
is the adapter, and the engine-agnostic crates above are the core. The real
question is only whether the adapter should be compiled in, dynamically loaded,
or data-driven.

- **Dynamically loaded: declined for now.** Rust has no stable ABI, so a plugin
  boundary means a C ABI, which means flattening `Change`, `Schema` and `Risk`
  into a serialized form and losing every type guarantee the trait currently
  gives. Worse, it is a stability commitment: once third-party dialect plugins
  exist, `ChangeSet` is a public API that cannot be refactored — and the model
  is still moving, having just taken the plan format to 2 and the state format
  to 3. Note this does **not** collide with §14.3's plugin-engine guardrail:
  an emitter runs *before* a plan exists, so the checksum still describes what
  runs. The objection is supply chain — the SQL in your plan would come from
  third-party code — and premature API freeze.
- **Data-driven: partly possible, less than it looks.** dbt's adapters are thin
  because dbt's per-engine surface is thin: it emits `CREATE TABLE AS SELECT`
  and the user writes the SELECT. It does not diff schemas, classify risk, fold
  a type-and-nullability change into one `ALTER COLUMN`, or derive preflight
  probes. Inspecting `pbps-mssql`'s type catalogue, roughly 15% is genuinely
  tabular (`CATALOGUE`, `ALIASES`); the rest is engine-specific logic —
  `varchar` without a length becoming `varchar(1)`, `float(24)` becoming
  `real`, `decimal` defaulting to `(18,0)`, which types admit `max`.

**So the extraction is worth doing, but only with two implementations in hand.**
Designing a universal adapter format from one engine produces an abstraction
that is SQL Server wearing a costume — the same mistake SPEC §12 warns about for
Phase 0, in the other direction.

## Decision

1. **No universal layer is adopted to reduce dialect work.** The arithmetic does
   not support it, and any future adoption is justified by driver maintenance
   instead.
2. **Dialect plugins stay compiled-in** until the model has stopped moving *and*
   somebody outside this project wants to write one. In-tree adapters are
   cheaper for everyone until both are true.
3. **The data-driven extraction waits for the second dialect**, and is done from
   what two implementations actually share.
4. **ADBC gets a spike, not a verdict**, and the spike's question is transaction
   and DDL fidelity, not connectivity.
5. **The seam is the deliverable that makes all of this cheap.** `pbps-db` now
   owns `Row`, `FromColumn` and `Param`, and `tiberius` is named in exactly one
   file — so ADBC, ODBC, `tiberius-ng` or staying put are all changes to one
   crate. That was built before the decision on purpose: the cost of being wrong
   is what makes a decision hard to take, and this lowers it.

## What would reopen this

- The ADBC spike showing faithful transaction control **and** a target engine
  arriving with no healthy pure-Rust driver. Either alone is not enough: the
  first without the second buys nothing, and the second without the first would
  trade a maintained driver for a broken guarantee.
- A decision that the product's primary target is container-first, at which
  point the distribution cost in §11.3 should be re-priced explicitly rather
  than inherited.
