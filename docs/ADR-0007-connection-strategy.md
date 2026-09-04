# ADR-0007: The connection layer — what a universal driver can and cannot carry

- Status: decided. Dialect plugins are declined; a universal connection layer is
  not adopted; ADBC for SQL Server is refused on the spike's result below
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
repository. A SQL Server driver exists, published by the **ADBC Driver
Foundry** (adbc-drivers.org). The follow-up description was wrong too, in the
opposite direction — it called that driver Apache-licensed and open source,
which the spike below disproves.

Microsoft's own migration of Power BI and Fabric from ODBC to ADBC makes the
standard's direction credible rather than speculative, and that is not in
dispute. What was in dispute was whether this project can use it.

### The spike, 2026-09-01

The question this ADR set was whether the driver's transaction control and
arbitrary-DDL execution could honour §7.5. **That question was never reached.**
The driver fails an earlier gate, and the gate is not a matter of taste.

Fetched through the Go module proxy, `github.com/adbc-drivers/mssql` contains
three files — `README.md`, `LICENSE.txt`, `NOTICE.txt` — and no source. The
README states it plainly:

> This repository is only for issue tracking and community feedback. […] The
> source code for this project is private and is not included in this
> repository.

`LICENSE.txt` is not Apache-2.0. It is the **Permissive Binary License 1.0**,
copyright Columnar Technologies Inc.: binary redistribution only, and
explicitly *"no reverse engineering, decompilation, or disassembly of this
software is permitted."* The driver is obtained as a pre-built binary from one
vendor's CDN via `dbc`. The module has no tagged release — the newest version
the proxy knows is a pseudo-version dated 2025-11-09.

This is specific to SQL Server, and worth stating precisely rather than
condemning ADBC as a whole. Checked the same way: `github.com/adbc-drivers/mysql`
is Apache-2.0, tagged v0.1.0, with source present; `github.com/adbc-drivers/postgresql`
is Apache-2.0. **The one engine this project most needs is the proprietary
one.**

### Why that settles it, on grounds stronger than the original question

- **Auditability.** This tool's entire claim is that the reviewed plan is
  exactly what runs (§7.3). Having an unauditable binary execute the statements
  contradicts it at the root, and the licence forbids even looking.
- **The cloud-registry refusal.** §1.1's third differentiator is a saved plan
  pinned "as a free, file-based mechanism, with no cloud registry in the loop",
  and §14.3 refuses a vendor holding a piece of the pipeline. `dbc install
  mssql` against one company's CDN is that vendor, one layer down.
- **Licence policy.** `deny.toml` allows a permissive open-source set. The PBL
  is not in it, and adding a binary-only, no-reverse-engineering licence is a
  real decision, not a list edit.
- **The motivation inverts.** ADBC was worth considering *because* a stale crate
  is a maintenance risk. But a stale open-source crate can be forked — that is
  exactly what `tiberius-ng` is. A proprietary binary from a single vendor
  cannot. If Columnar stops, the recourse is strictly less than today's.
- **Air-gap, demonstrated rather than argued.** The spike could not obtain the
  binary at all: the vendor CDN is unreachable from a restricted-egress
  environment. That is the scenario §11.3 was written for, arriving on the
  first attempt.

**So ADBC is refused for SQL Server.** For PostgreSQL and MySQL its drivers are
open source, but there the healthy pure-Rust drivers remove the motivation
entirely — adopting a shared-library stack to reach engines that already have
one would be paying the distribution cost for nothing.

**What remains untested, stated plainly:** transaction and DDL fidelity. The
binary could not be obtained, so no behavioural claim is made here in either
direction. If the licensing ever changes, that spike is still the one to run,
and the acceptance criterion is the §11.5 invariant that a plan whose second
statement fails leaves the first one's effect nowhere.

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
  is still moving, having since taken the plan format to 3 and the state format
  to 4. Note this does **not** collide with §14.3's plugin-engine guardrail:
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
4. **ADBC is refused for SQL Server**, on source availability and distribution
   rather than on behaviour — the spike never reached the behavioural question,
   and says so. For the engines whose ADBC drivers *are* open source, the
   motivation is absent.
5. **The seam is the deliverable that makes all of this cheap.** `pbps-db` now
   owns `Row`, `FromColumn` and `Param`, and `tiberius` is named in exactly one
   file — so ADBC, ODBC, `tiberius-ng` or staying put are all changes to one
   crate. That was built before the decision on purpose: the cost of being wrong
   is what makes a decision hard to take, and this lowers it.

## What would reopen this

- The SQL Server ADBC driver being published under an open-source licence, at
  which point the behavioural spike becomes worth running — **and** a target
  engine arriving with no healthy pure-Rust driver. Either alone is not enough:
  the first without the second buys nothing, and the second without the first
  would trade a maintained driver for an unauditable one.
- A decision that the product's primary target is container-first, at which
  point the distribution cost in §11.3 should be re-priced explicitly rather
  than inherited.
