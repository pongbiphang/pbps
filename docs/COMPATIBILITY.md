# Database compatibility

The qualified core matrix is Linux/amd64 on these pinned container builds.
A feature's minimum engine version is a capability floor, not a claim that
all catalogs, operations, editions or versions above that floor are tested.
The SQL Server 2022 development example in the README is covered by the
2022 Developer row below.

| Cell | Actual server version | Edition / server collation | Qualification |
| --- | --- | --- | --- |
| `mssql2022` | 16.0.4295.3 | Developer / SQL_Latin1_General_CP1_CI_AS | Compact core contract |
| `mssql2025` | 17.0.4075.5 | Enterprise Developer / SQL_Latin1_General_CP1_CI_AS | Compact contract and primary full suites |
| `mssql2025-express` | 17.0.4075.5 | Express / Latin1_General_100_CS_AS | Compact contract and edition/collation regressions |
| `pg16` | 16.15 (`160015`) | PostgreSQL | Compact contract and selected pre-17 regressions |
| `pg18` | 18.6 (`180006`) | PostgreSQL | Compact contract and primary full suites |

The immutable image references and expected server identities live in
[`scripts/compatibility-matrix.json`](../scripts/compatibility-matrix.json).
`CI / compatibility (<cell>)` runs every row on its own runner and feeds
`ci-gate`; a missing or failed row fails the gate. Existing full suites remain
in their engine jobs. The ignored-test execution owners remain the full
`flow` and `flow_pg` jobs; the compatibility job additionally selects exactly
`compatibility_core_contract` in each of those targets.

The compact contract exercises bootstrap, catalog pull and verify, a connected
saved plan and checksum-approved apply, a rejected checksum, the real deployment
lock and a successful retry after release, and ledger preservation on refusal.
It creates a numeric column, enforces a primary key, adds a varchar column while
preserving data, and creates/applies role grants (using an existing cluster
role on PostgreSQL). SQL Server executes an ONLINE index on capable editions
and refuses it on Express. PostgreSQL applies MAINTAIN on 18 and refuses it on
16 without replacing the plan artifact or appending history. Test-owned databases
and cluster roles are registered before creation and explicitly removed and
checked at completion; partial failures also attempt cleanup.

This is a bounded contract, not the exhaustive primary suite on every version.
It does not qualify managed database services, other architectures, every SQL
Server edition, or a complete resolver binding. The native Windows and packaged
Linux executable/TLS qualification belongs to the separate release jobs; it
must not be inferred from these container results. Feature-specific limitations
in SPEC and the decision records still apply.

## Running a cell

Run `scripts/compatibility-tests.sh pg16` (or another cell from the table).
It leaves its named disposable container running for reuse. Set
`PBPS_COMPAT_CONTAINER` and `PBPS_COMPAT_PORT` to select an existing fixture;
the default name is `pbps-compat-<cell>` and default host port is 15432, so choose
a different port when retaining multiple cells. The fixture credentials are
local test credentials, never deployment credentials.

Before any test writes, the runner checks the running container's concrete
image ID against the resolved digest, its published localhost port, architecture,
and actual engine identity. The Rust contract independently reads the identity
through the same connection string used by the CLI. Missing/unreadable metadata,
a stopped fixture, a wrong image, version, edition, collation or endpoint all
fail qualification. The primary local live-test scripts enforce the same
container admission. They do not remove or replace a mismatched existing
fixture: its owner must inspect it and explicitly replace it if appropriate.
When reusing an engine originally started by a tag, first pull the matrix's
pinned image so its concrete identity can be inspected.

Image updates are deliberate: update the matrix and matching primary fixtures,
measure actual server properties, and rerun the affected contracts and full
primary suites. A floating tag or parsed version-string unit test is not new
engine qualification (DEC-1134.1).
