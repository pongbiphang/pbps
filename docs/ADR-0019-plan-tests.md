# ADR-0019: Disposable plan tests with pinned fixtures and SQL assertions

- Status: accepted contract; runner implementation follows in #1165
  (PostgreSQL first, then SQL Server; both are required).
- Date: 2026-10-26
- Issue: #1164; parent: #1156; consumers: #1165, #2401 and #1166.
- Related: [SPEC §7.5](SPEC.md#75-the-execution-model-of-apply),
  [§9.9](SPEC.md#99-disposable-plan-tests), §9.8, §14.1 and §14.3;
  [ADR-0008](ADR-0008-policies.md),
  [ADR-0016](ADR-0016-engine-assisted-planning.md), especially its current
  resolver amendment; DECISIONS 465.

## 1. Purpose and scope

A team should be able to demonstrate that a candidate declaration change
preserves sample data and satisfies domain invariants. `pbps test` constructs
an owned disposable database, loads SQL fixtures at their schema revisions,
replays to the base, plans and applies the candidate there, runs SQL assertions,
and removes the resources. It reports the exact executed test-plan checksum.
It does not certify production data, application compatibility or the identity
of a later production plan.

This records the maintainer's [2026-10-26 decisions on #1156](https://github.com/pongbiphang/pbps/issues/1156#issuecomment-6401634798),
which supersede the September discussion. The examples below describe the
contract to implement, not an existing command.

Production `apply` continues to execute only the approved typed plan. No fixture,
assertion file, repository hook or external backfill script runs between its
approval and statements (SPEC §7.5). The three contract-verification layers of
§14.1 are constraints, recorded drop preconditions (#2400), and reviewed target
assertions **at plan time** (#2401). This test runner is additional evidence;
it replaces none of those layers. The preview/rehearsal of §9.3 remains separate.

## 2. Command and manifest

```text
pbps test [--manifest <path>] [--format human|json]
pbps test fixtures repin <name> --at <revision> [--manifest <path>] [--format human|json]
pbps test --cleanup [--format human|json]
```

The default manifest is `tests/pbps-tests.yml`, relative to the project root.
`--cleanup` performs only the owned-leftover reconciliation in §3, without
reading or running fixture/assertion SQL. It cannot be combined with a manifest
or re-pin. These commands take no `--db`, `--env`, supplied-server connection,
production plan, `--out`, or SQL-output option. They never write an applyable
plan file. A report redirected to a file is still a report, not a `SavedPlan`.

Version 1 uses the following closed YAML mapping. Unknown keys and duplicate
mapping keys are errors; the loader uses the ordinary LF/UTF-8 format rules.

```yaml
version: 1
dialect: postgres
base: 0123456789abcdef0123456789abcdef01234567
candidate: working-tree
engine:
  image: postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280
fixtures:
  - name: orders
    file: fixtures/orders.sql
    at: 0123456789abcdef0123456789abcdef01234567
assertions:
  - name: preserved-orders
    file: assertions/preserved-orders.sql
timeouts:
  startup_ms: 120000
  step_ms: 120000
  fixture_ms: 30000
  assertion_ms: 10000
  run_ms: 600000
  cleanup_ms: 30000
```

`version`, `dialect`, `base`, `engine.image`, `fixtures` and `assertions` are
required. Replace the illustrative base/fixture commit above with the full
immutable commit id in your project. `candidate` is optional and defaults to
`working-tree`, reading the current candidate inputs as `plan` does; an explicit
git revision such as `HEAD` instead selects its resolved committed contents.
`dialect` is `postgres` or `mssql` and must match each input schema's project
configuration and the image. Every fixture has exactly
`name`, `file`, `at`; every assertion has exactly `name`, `file`. Names are
nonempty and unique within their list; duplicate files within either list
are refused. Fixtures may be empty; assertions must contain at least one entry.
Paths are relative to the manifest directory, must stay within the project,
and must resolve to readable regular files; symlink escapes are refused.

`timeouts` and each of its fields are optional, with the defaults above. Values
are positive integer milliseconds bounded by a 32-bit unsigned integer.
`step_ms` bounds each bootstrap/plan/apply hop. The run deadline bounds all
work after validation; a phase also obeys its smaller remaining deadline.
Cleanup has its own bounded allowance after work stops, including on timeout.
No driver or container-runtime command may wait indefinitely.

The image must be a digest reference, not a mutable tag. Resolve and record the
acquired image's actual identity and engine build; a dialect mismatch refuses
before SQL is transferred. Image/runtime acquisition uses the resolver scratch
adapter (Docker or Podman), not a second container subsystem. Version 1 has no
supplied-server mode and no persistent dev database.

### Input identity and validation before provisioning

Require base and fixture `at` pins to be full immutable commit ids. Resolve an
explicit committed candidate revision unambiguously, without fetching implicitly;
a missing/ambiguous revision refuses with the exact input and remedy. Resolve
`HEAD` once for provenance. Load historical config, declarations, ids and hints
through the ordinary revision-aware loader, without checking out revisions in
the user's worktree or executing hooks. For the default working-tree candidate,
use the ordinary `plan` input loader and freeze every consumed candidate file,
including new/untracked inputs and changed ids/config/data. Uncommitted edits
are supported; no commit is required. Fixture, assertion and manifest bytes
are the explicitly read working files.

Identify a working-tree candidate as `kind: working_tree`, `uncommitted: true`,
with its captured HEAD commit and the complete candidate input path/hash list.
This label remains uncommitted even if its bytes happen to match HEAD: it does
not claim that git supplied the tested contents. An explicitly selected commit
is `kind: commit`, `uncommitted: false`, with its resolved commit and consumed
file hashes. CI may require the committed kind; the runner does not impose that
policy or reject useful pre-commit iteration.

Before creating any resource, read and freeze all those inputs, check the
manifest/version/paths, dialects, pin ancestry (§4), required ids and known
intent, and validate declarations with the ordinary loader/policies. Validate
all files, including ones that an earlier failure would later skip. No template
expansion, SQLCMD shell commands or environment-driven target selection occurs.
Unresolvable inputs, unknown versions, unreadable files, missing intent and
contradictory flags never trigger provisioning. Checks needing a live database
remain ordinary plan/apply checks, not fabricated preflight successes.

The result identifies the manifest path and SHA-256 of its exact bytes, resolved
base/pin commits and the candidate identity above, and a sorted list of every
consumed file's project
path, revision (or `working`), role and SHA-256. This includes config, declarations,
ids, hints, fixture/assertion SQL and referenced declaration data. Hash raw
bytes, without newline or SQL normalization. Record tool version, manifest
version, dialect, requested/acquired image digest, observed engine build and
timeouts. Freeze input membership as well as bytes; before publishing a
qualifying result, re-enumerate/re-read working inputs and recheck captured HEAD.
An added/deleted/changed input or changed HEAD refuses qualification rather than
mixing identities or old hashes with new content.
No password, connection string, fingerprint key or complete saved plan goes
into the report. A missing file is not an empty file.

## 3. Confinement, ownership and cleanup

The command can connect only through a capability returned by the scratch
acquisition that **this run** performed. Pass that owned handle to bootstrap,
planner, executor and assertion adapters; do not accept an arbitrary connection
string at those call sites. Never resolve a user project's environments for
targets or credentials. A private internal project/environment and generated
fingerprint key may be used where ordinary PostgreSQL routine pinning requires
one; it maps solely to this run's scratch and disappears during cleanup.

Provisioning/admin, apply, fixture and assertion credentials are separate,
generated per run, and held privately. Replace both user and password when
changing identity. Never run user SQL with provisioning credentials. The runtime
gets no project/home bind mount, host credential directory, device, privileged
mode or container-runtime socket. Only the generated database control endpoint
is published on loopback. No host SQL client, repository hook or external data
script is invoked. Use the database drivers for user SQL.

Images, declared executable routines and the runtime/operator are trusted.
SQL privileges and ownership labels are not a hostile-code or network sandbox.
The current resolver's operator-vouched isolation premise still applies;
this ADR does not revive its removed measured containment profiles or claim
that a container label prevents outbound network access. The operator must
provide the isolation appropriate to reviewed source and images, without
production credentials or connectivity. No successful test is evidence that
arbitrary extensions or runtime administrators were contained.

### Evidence of ownership

Keep the resolver's `pbps.resolver=scratch` and
`pbps.resolver.process=<pid>-<process-nonce>` labels. Add a test-purpose label
and an unpredictable, at least 128-bit per-run identifier. A name prefix, PID,
image match or database name alone is insufficient. Verify the labels and
immutable container ID through the selected runtime before connecting or
removing it; verify every additional database belongs to that same owned server.

Before creation, atomically persist a private, user-owned run record with the
run id, process identity (including host boot/process-start identity), selected
runtime identity/endpoint and requested resource labels. Hold its exclusive
process lock for the run. Record the returned immutable container ID and its
private volumes as soon as known. Use private directories/files (0700/0600),
no symlink following, and crash-safe replacement. The record contains ownership
metadata, not SQL, passwords or reusable deployment credentials.

Records live in the per-user OS state directory under `pbps/test-runs/`
(`$XDG_STATE_HOME/pbps/test-runs`, default `~/.local/state/pbps/test-runs`, on
Unix; the equivalent private per-user application-data directory on Windows).
Use an owner-only ACL where POSIX modes do not apply. Recovery reads this
registry, not arbitrary repository paths or every similarly named container.

Persisting intent **before** creation closes the SIGKILL window between creation
and recording its returned ID: a later run can enumerate exact matching labels
on the recorded runtime and reconcile the pending record. Do not adopt an
unrecorded container merely because its name resembles a test container.

### Normal teardown and later recovery

Stop work on any failure, cancellation or deadline and enter cleanup. Close
owned clients; stop/remove the exact owned container and its disposable volumes
using the shared scratch removal/confirmation path. Confirm container **and**
volume absence. Confirmed runtime absence, an unreadable inspection and a command
that timed out are three different results. Removal requested is not removal
confirmed. No success is published before confirmation; Drop remains a fallback.

Each new test, re-pin or `--cleanup` invocation first reconciles this user's
recorded leftovers within a bounded cleanup deadline. Check the process lock
and full recorded process identity, not just whether the numeric PID exists.
Leave live/concurrent runs alone. For a demonstrably dead owner, inspect the
recorded runtime and exact run labels, reconcile IDs including pending creates,
and remove only proven owned resources. Check volume ownership/references before
removal; never follow a foreign bind mount or remove another container's volume.
A name now belonging to a different immutable ID is not the original resource.

Unknown owner liveness, inaccessible runtime/history, conflicting labels/IDs,
or unprovable volume ownership stops reconciliation and reports `cleanup_failure`
with the run/resource identifiers and manual inspection remedy. Do not delete
ambiguous resources or interpret failed enumeration as an empty set. Keep the
record until absence is confirmed; only then remove it and private credentials/
project files. SIGKILL itself cannot emit a final report: its later recovery
report explicitly identifies the interrupted run. Failure to recover is a
nonzero command result, not a warning hidden behind a new passing test.

## 4. Pinned replay and the ordinary plan/checksum path

Distinct fixture pins must form **one ancestry chain ending at the base**.
Use `git merge-base --is-ancestor` to establish each adjacent relation; equal
pins form one group. Require base to be an ancestor of the selected candidate
commit, or the captured HEAD for a working-tree candidate. Working-tree changes
then form the final declaration-set jump; they need no new commit. Do not
require first-parent ancestry: a merged commit can be a valid ancestor. Refuse
incomparable pins, name them, and direct the user to re-pin onto a common chain.
No arbitrary topological sort may turn diverging histories into apparent drops.
Planning compares two declaration sets using each side's ids (SPEC §5.2).

Order groups by ancestry; within a pin use manifest order. Bootstrap the oldest
pin's declared schema and ledger through the ordinary bootstrap path into the
empty scratch, including declared `data:` rows. Load that pin's fixtures.
Plan/apply the jump to the next pin with that revision's declarations, ids and
recorded rename/drop/conversion intent; load its group; continue to the base.
With no fixtures, bootstrap at base. These are endpoint jumps at the distinct
pins and base, not a first-parent walk or execution of every historical commit.
The normal differ decides each hop; do not invent a rename, drop reason, value
conversion, external dependency or business backfill to make replay succeed.
If a required intent cannot be read or is absent, refuse with the exact hop,
object and ordinary intent-recording command, without prompting inside CI.

At base, capture the prepared database through ordinary connected planning and
compile/seal the base-to-candidate test plan from the frozen committed or
working-tree candidate inputs of §2. Bind the reported candidate identity and
file hashes to that execution, without pretending an uncommitted tree is a
commit. Keep the plan in memory. `SavedPlan` is
an untrusted wire DTO; use the normal sealed/admitted artifact boundary, not a
DTO handed directly to an executor. Verify the seal's checksum and pass the
same accepted artifact, unchanged, to ordinary apply. The runner grants the
scratch-only risk approval for its sealed typed changes and reports that risk
set; it never changes risk classification or supplies human identity intent.
Run normal deployment locks, baseline/ids checks, version/origin validation,
probes, routine/resolver prerequisites, execution-mode rules, postconditions
and ledger recording. A missing fingerprint key is solved with the private
runner key, not by disabling routine pinning. Unsupported staged/resolver cases
remain refused; this command does not expand their existing support.

Apply this same lifecycle to replay hops. A fixture connection is closed before
the next capture/apply. Temporary helpers cannot become undeclared persistent
routines in the plan. Refresh fixture/assertion grants for the current managed
objects after each hop, keeping runner infrastructure identities outside the
user's managed declaration scope; qualify that ordinary captures/checksums and
ledger postconditions remain accurate with those identities present.

A saved plan has no server/database identity. Baseline equality and routine
pins cannot make a file "test-only" for old production readers. Therefore the
structural separation is **no serialized applyable artifact at all**: no import
of a production plan, hidden plan file, `--out`, or debug dump. Reports expose
checksums and summaries, never the `SavedPlan` payload. Do not edit a production
plan's baseline, target context or checksum, or promote preview SQL to a plan.
A later production plan is independently generated and reviewed against that
target; its checksum need not equal the test checksum.

Report bootstrap time, each hop's from/to commits, git revision distance
(`git rev-list --count from..to`, including merged history), planned change/
statement counts, elapsed plan/apply time and checksum. Report total replay
hops/time separately from candidate work. Distance is a cost indicator, not
an instruction to apply every intervening commit or a production cost estimate.

## 5. Advancing fixture pins safely

The user edits fixture SQL. `fixtures repin` validates that edit and atomically
updates only the selected fixture's `at` pin in the manifest; it never generates
or rewrites SQL. The new revision must advance along the ancestry chain and
remain at or before base. An invalid chain is refused with the pins to repair.

The committed `HEAD` manifest and fixture bytes are the old input snapshot,
independent of the schema commit in `at`. Require a committed old entry/SQL and
an unchanged manifest; name a missing baseline and ask the user to commit the
original fixture first. Read edited SQL from the working file. Freeze/hash both
versions and use the same image/settings. Other fixture entries/files must
remain unchanged during this operation; advance another fixture separately.

In separate fresh owned databases, (a) bootstrap at the old pin, load its old
SQL and replay ordinarily to the new revision; (b) bootstrap at the new revision
and load the edited SQL. Compare **all managed tables' rows and allocators**.
For a manifest with shared fixture context, reconstruct the complete original
and proposed prefixes through the new revision: the original prefix loads the
old file at its old pin; the proposed prefix loads it only at its new pin, in
manifest order there. Reproduce unchanged entries at their own pins by ordinary
replay, not by interpreting their old SQL against a newer schema. Compare after
all entries at the new revision have loaded. This both supplies prerequisite
data and detects moving SQL past another group's noncommuting operations.
An edit/context that cannot be loaded is a refusal, not an occasion to generate
replacement SQL. Later fixtures/assertions are still qualified by a subsequent
full test run, not certified by re-pin alone.

Row comparison is complete and type-aware, including NULL, duplicate counts,
all columns and `data:` tables. Use a deterministic multiset comparison, not
row order, a primary-key-only sample, collation-folded text or a bounded prefix.
Show bounded differences and their table/column identities; truncation limits
the report, never the equality test. An unreadable/unsupported value or a
comparison deadline means equivalence could not be established and refuses.

For **every sequence used by a managed table**, whether owned by it or referenced
by a default, and **every identity column**, compare the next value each would
hand out. Enumerate dependencies completely. Do not guess `last_value + 1` or
`IDENT_CURRENT + 1`: fresh/restarted/exhausted sequences, increments and caches
need the engine's actual semantics. Disposable probes may consume values only
in comparison databases that will be discarded. Report each allocator's table/
column or sequence identity and next value. A difference refuses; an unreadable,
non-standard or unestablishable allocator also refuses and names it. Never
silently omit an allocator, assume an unassigned state is zero, or use a scalar
function's opaque implementation as proof of standard allocation.

Why rows alone are insufficient (measured on all three engine builds in §8):
the old fixture inserts `(1,'kept')`, inserts `(2,'discarded')`, then deletes the
second row. The edited fixture inserts only `(1,'kept')`. Their remaining rows
are equal, but the next generated ids are **3 and 2**. A later identical fixture
therefore produces different data. Rows **plus allocator next values** are the
re-pin equivalence, under the same new schema and engine settings.

Any difference or unanswerable comparison leaves the manifest unchanged and
reports the difference/unestablished component. Only after equivalence **and
confirmed cleanup** re-read the manifest and edited SQL hashes, then atomically
replace the manifest with the new resolved commit in `at`. A changed input or
failed replacement refuses without claiming the pin moved. The command does
not commit files, change declarations/ids, or run arbitrary migration scripts.
Its report uses the `test` envelope branch with operation `repin`, both input
hash sets, comparison results and the old/new pins; it exports no plan.

## 6. Identities and fixture privileges

| Identity | PostgreSQL | SQL Server |
|---|---|---|
| Provision | Generated administrator, creates owned database/roles; never executes fixture/assertion text | Generated administrator, creates owned database/logins and changes database mode; never executes fixture/assertion text |
| Apply | Ordinary deploy permissions in the owned database, including ledger/lock and declared object management | Ordinary deploy permissions in the owned database, including ledger/lock and declared object management |
| Fixture | Non-superuser; schema USAGE; EXECUTE on managed routines; SELECT/INSERT/UPDATE/DELETE on managed tables except `data:`; SELECT only on `data:`; database TEMP for temporary helpers | Non-sysadmin; SELECT/INSERT/UPDATE/DELETE on managed tables except `data:`; SELECT only on `data:`; no permanent CREATE or ALTER |
| Assertion | Separate non-superuser with SELECT on managed tables, no table DML or schema CREATE; runner-owned READ ONLY transaction (§7) | Separate least-privilege SELECT login, no DML/CREATE/ALTER, plus scratch database READ_ONLY (§7) |

No fixture/assertion login owns managed objects or can assume the apply/admin
identity, create roles/databases, access other user databases, or acquire host
file/program/bulk privileges. On PostgreSQL exclude `pg_execute_server_program`,
`pg_read_server_files` and `pg_write_server_files`; on SQL Server exclude
sysadmin and bulk-administration rights. A broad default/public grant is not
an excuse to silently grant a more privileged login. Verify effective grants,
not just the names of roles the runner created. Declared routines/triggers are
trusted schema code: a table privilege is not proof that their indirect effects
are data-only, which is also why assertions need more than SELECT grants.

Fixture SQL may contain multiple statements and use temporary objects. The
runner enforces data privileges through the engine, not a SELECT/INSERT-prefix
filter. PostgreSQL database TEMP permits `pg_temp` tables and functions without
permanent schema CREATE. SQL Server `#temp` tables/procedures work without
permanent CREATE grants, even with explicit tempdb CREATE denials (§8).
Temporary state belongs to that group's fixture session and is discarded when
it closes. Files at the same pin share that session; later pin groups do not.
Fixtures cannot modify declared reference rows (`data:` exact or ensure).
Such rows are bootstrapped/applied from declarations and are available to SELECT.

SQL Server fixture files may use a standalone `GO` batch separator for temporary
procedures; use dialect-aware framing, not splitting on every semicolon.
`GO <count>` and SQLCMD metacommands/includes/shell execution are not supported.
Assertions have no batch separators or client commands. PostgreSQL helpers are
sent as SQL through the driver, not expanded by psql.

SQL Server fixture logins cannot supply explicit identity values: ordinary
INSERT gives error 544; `SET IDENTITY_INSERT ON` requires ALTER and is refused
(1088 in the measurement). Do not grant ALTER to avoid that limitation. Insert
parents with generated identities, capture them through `OUTPUT ... INTO #ids`,
and insert children through `INSERT ... SELECT` using those ids or a stable
business-key lookup. PostgreSQL fixture identity/default-sequence privileges
must be limited to generating managed-table fixture values, not broad grants
on arbitrary sequences. Neither engine's fixtures may reset allocators by DDL.

## 7. Assertion contract, shared with target planning

An assertion entry names one UTF-8 SQL file containing **one query** in the
selected dialect. It must return exactly one tabular result set with **at least
one output column**. A zero-column result is an invalid
assertion, whether it contains zero rows or any rows; it is neither pass nor
violation. Refuse it with the remedy `SELECT 1 ...` (DEC-1165.1). With a valid
result shape, zero rows is pass. Any returned row is a violation, including a
row of NULLs, an empty string, `false`, or `COUNT(*) = 0`. There is no truthy scalar
interpretation, expected-failure flag or per-file pass convention. An empty
file, a command without a row result or multiple result sets is invalid.

On PostgreSQL, after preparing/describing the original statement and before
executing it, the runner reads each result type's `pg_catalog.pg_type.typsend`
in the same READ ONLY transaction. Resolve domains to their base types and
arrays to their element types recursively. If any type in that chain lacks
binary output, the assertion is **InvalidResult**, even for NULL or zero rows;
name the column and its type and say "cast it to text, e.g. `col::text`".
Nothing executes, so this is neither pass, SQL error nor violation. An
unreadable or missing capability is **Unanswerable**. PostgreSQL 16.15 and 18.6
both raised 42883 for binary `aclitem` output; the native driver always requests
binary results. Refuse this described shape rather than fork the driver or use
the simple protocol (DEC-1165.3). Other types without a client text renderer
remain marked unrenderable cells and their rows still establish violations.

The engine/driver must complete the query and result stream successfully to
report an empty result. Capture up to 20 evidence rows and 64 KiB per assertion,
with per-value truncation recorded; preserve column names/types and NULL as
JSON null. For a valid result shape, reaching the evidence bound establishes a
violation: cancel and drain or close within the deadline, record that more rows
may exist, and
never run another query on an uncertain connection. Cancellation/read failure
remains visible; a timeout or unreadable stream cannot be recast as pass.
Zero-row success always requires the complete stream and boundary verification.

SQL errors (including attempted writes), transaction-boundary violations,
invalid result shape, driver timeouts, transport loss and unreadable results
have distinct outcomes (§9). A timeout must be an error from the runner's
**driver**; the runner never shells out to sqlcmd/psql. The measured sqlcmd
exit-0 timeout is evidence for that choice, not an alternative success rule.

### PostgreSQL: own and verify the transaction, not just the call

For each assertion, open a fresh runner-owned connection as the assertion login
and begin READ ONLY. Before any user statement, perform the runner's own read
to obtain a transaction identity and confirm read-only state. The identity must
distinguish successive transactions on that connection; a backend PID, a NULL
unassigned xid, or an "in transaction" flag alone is insufficient.

Send the complete assertion through a single extended-protocol Parse/execute
path, with no simple-query fallback or split-and-run loop. PostgreSQL refuses
multiple statements there with 42601. After the statement, verify that **that
same transaction** is still open, has the recorded identity, and remains READ
ONLY. If it ended, changed or cannot be established, fail the run as an
assertion-boundary violation and discard the connection. Roll back/close after
a valid query too; never reuse assertion sessions across files. A query error
that aborts a transaction is an assertion error, never a pass.

The runner's first read prevents a leading `SET TRANSACTION READ WRITE` (25001
in the measurements). The postcheck catches standalone COMMIT/ROLLBACK: a
one-statement protocol by itself does not stop sequential transaction escape.
A privileged SECURITY DEFINER function's persistent write is refused by an
intact READ ONLY transaction (25006), even when the assertion login has EXECUTE.
Without that transaction it can write despite the caller's lack of INSERT.
This is a transaction/data boundary, not a host/network sandbox for extensions.
The implementation must qualify its identity read/postcheck on both PG versions.

### SQL Server: read-only database in scratch, privilege boundary on a target

After the candidate apply, close all runner connections to the owned database,
including apply, fixture and capture clients. From the provisioning connection
to master, set that database READ_ONLY with a bounded wait and verify the mode.
Do **not** use ROLLBACK IMMEDIATE: there should be no connection to evict.
A remaining connection/timeout is a setup failure, not permission to kill it.
Only then open assertion-login connections and execute one query/file. Use
dialect-aware framing to refuse multiple top-level statements; do not treat
framing or a SELECT prefix as enforcement of read-only behavior. Require the
one-result-set shape and consume driver errors as above.

READ_ONLY blocks persistent writes even through an owner-executing procedure
or administrator, while temporary objects in tempdb remain writable. Snapshots
also work but are not the v1 runner design: this database is disposable and
owned, so closing clients and changing mode suffices (§8). Cleanup removes it;
there is no need to make it READ_WRITE again for another user.

For #2401, select assertions with repeatable
`pbps plan --db <connection> --assert <file>` or
`pbps plan --env <name> --assert <file>`; the file format and input hashing above
are the same, without fixtures, pins or scratch provisioning. Use a **separate**
assertion connection from `PBPS_ASSERTION_CONN`; if absent, refuse naming that
variable, never fall back to the apply/planning login. Verify it identifies the
same target database through ordinary endpoint/database identity reads.
PostgreSQL retains the owned READ ONLY transaction protocol above. A SQL Server
target cannot be switched read-only: **its boundary is the least-privilege login
only**. Refuse effective EXECUTE on procedure-like objects (`P`, `PC`, `X`,
`RF`), including grants inherited from roles, schemas or the database: ordinary
ownership chaining can persist writes without EXECUTE AS or caller table DML.
Name the procedure in the refusal. Refuse any user-defined assembly in the
target database (`sys.assemblies.is_user_defined = 1`), naming it; an unreadable
answer refuses. Do not trust permission_set: CAS is not a boundary under CLR
strict security. Over-refusing databases with CLR is accepted because
ownership-chained T-SQL function/view wrappers can execute CLR without direct
CLR grants. Without CLR, T-SQL functions (`FN`, `IF`, `TF`) retain their allowed
execution rights; T-SQL functions cannot change persistent database state.

Audit the same effective privilege inventory in every accessible system
database (`master`, `msdb`, `model`, `tempdb`) before returning to the target.
Guest access itself is allowed; write-capable rights on permanent objects and
CREATE TABLE refuse by database and object/permission name. Ordinary temporary
objects stay allowed. Complete metadata is required in every audited database;
map a read-only user there, grant VIEW DEFINITION, and deny inherited procedure
EXECUTE. Include system procedures from `sys.system_objects` in the same
inventory. Their object permissions are managed in master (native 4629); an
object DENY there applies when the procedure is invoked from another database,
whereas database-wide DENY in master alone does not. Default guest ownership can include permanent CREATE SEQUENCE and is refused as a real capability.
Legacy DEFAULT/RULE object permission rows are nominal: native DROP and GRANT
are denied without schema rights, which the same inventory still checks. Table
types are checked in the TYPE namespace rather than as OBJECT securables.

The assertion login must have no access to any other user database on the
instance. Check HAS_DBACCESS for every other user database, by database id;
unreadable answers refuse. Server VIEW ANY DATABASE is required so a hidden
accessible database cannot appear absent. Refuse a linked server with a mapping
for the login or all logins, naming the server. A default mapping is hidden
without server VIEW ANY DEFINITION; require that read permission before treating
a visible server's mapping inventory as complete. A mapped remote identity can
write through OPENQUERY or EXEC AT, even when the outer query returns zero rows.

Verify and report these effective privileges; framing is not read-only
enforcement and the target is never switched to scratch READ_ONLY mode. No user SQL
runs during apply. Target assertion hashes/outcomes enter the plan checksum,
`explain` and ledger (#2401); failures/errors write no plan. The accepted
plan-to-apply window is closed only for the specific predicates covered by
constraints or recorded drop preconditions, not arbitrary target assertions.

#2401 PR A supplies the target engine entry points; the CLI selection and saved
observation fields follow in PR B. SQL Server's assertion credential also needs
`VIEW DEFINITION` on the target database so hidden securables cannot appear to
have no write permissions. The server metadata requirements above likewise
prevent hidden databases or default linked-server mappings from reading empty.
Unknown identity or permission reads refuse before assertion SQL; no grants are added by the runner. The existing PostgreSQL
scratch entry point and its transaction protocol remain unchanged.

## 8. Measurements supporting the boundaries

Measured on 2026-10-26 (2026-10-25 UTC), in fresh owned containers, using distinct
admin/apply/fixture/assertion passwords and recording actual session identities:

| Engine build | Exact image |
|---|---|
| PostgreSQL 18.6, Debian 18.6-1.pgdg13+2, x86_64 | `postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280` |
| PostgreSQL 16.15, Debian 16.15-1.pgdg13+2, x86_64 | `postgres@sha256:485935f94cc7165afa896978809c37b592dc07f0a37d2c8f645f12412d0212c8` |
| SQL Server 2025 RTM-CU8, 17.0.4075.5, Enterprise Developer, Ubuntu 24.04.4 | `mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1` |

### PostgreSQL: separate transaction escape from privilege-only execution

Both versions produced the same results. Apply owned `app`; fixture/assertion
logins were NOSUPERUSER/NOCREATEDB/NOCREATEROLE/NOREPLICATION/NOBYPASSRLS.
Fixture had table DML; assertion had SELECT, no INSERT, and an explicit EXECUTE
on a superuser-owned SECURITY DEFINER writer. Host-file/program roles were absent.
The measured definer setup below was executed by `postgres` after creating
`app.managed(id integer PRIMARY KEY, value text)` under the apply owner and
granting the assertion login SELECT/schema USAGE. These are separate calls:

```sql
CREATE FUNCTION app.definer_write(n integer) RETURNS integer LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,app AS $body$
BEGIN INSERT INTO app.managed VALUES(n,'definer'); RETURN n; END $body$;
GRANT EXECUTE ON FUNCTION app.definer_write(integer) TO pbps_assert;

-- pbps_assert, extended protocol, no READ ONLY: persists row (107,'definer').
SELECT app.definer_write(107);

-- pbps_assert, fresh connection; each line a separate extended call:
BEGIN READ ONLY;
SELECT app.definer_write(108);                  -- 25006; row 108 absent
ROLLBACK;

-- pbps_assert, fresh connection; each line a separate extended call:
BEGIN READ ONLY;
COMMIT;
SELECT app.definer_write(109);                  -- row 109 persists

-- pbps_fixture, fresh connection; separate extended calls:
BEGIN READ ONLY;
SELECT 1;                                      -- runner reads first
SET TRANSACTION READ WRITE;                     -- 25001
ROLLBACK;
```

| Case (separate connections/reset state) | Outcome on 16.15 and 18.6 |
|---|---|
| One simple Query string `COMMIT; INSERT INTO app.managed VALUES(101,'simple escape');` after BEGIN READ ONLY, as fixture | COMMIT and INSERT succeed; row persists |
| Extended Parse of `COMMIT; INSERT INTO app.managed VALUES(103,'must not happen')` | 42601; no row |
| Extended standalone COMMIT, then separate INSERT on reused fixture connection | Row persists |
| Extended standalone ROLLBACK, then separate INSERT on reused fixture connection | Row persists |
| Extended leading SET TRANSACTION READ WRITE before a runner read | Succeeds; subsequent write persists |
| Runner SELECT 1 first, then SET TRANSACTION READ WRITE | 25001 |
| SELECT-only caller invokes definer writer without READ ONLY | One persistent row |
| Same caller/function inside intact READ ONLY | 25006; zero rows persisted |
| Standalone COMMIT then definer writer | Persistent write; discarding and starting a fresh READ ONLY connection restores refusal |
| Fixture INSERT into managed table / into `data:` reference table | Allowed / 42501 |
| Fixture permanent CREATE TABLE or FUNCTION | 42501 |
| CREATE TEMP TABLE / CREATE FUNCTION pg_temp helper before database TEMP grant | Both 42501 |
| Same temporary helpers after `GRANT TEMP ON DATABASE ... TO pbps_fixture` | Both work, without permanent schema CREATE |
| `COPY app.program_probe FROM PROGRAM 'printf 1164'` | Admin positive control works; fixture/assertion 42501 |
| Zero rows / one NULL row / divide by zero | Pass-shaped empty / one violation row / 22012 |
| Statement timeout 100 ms / self-terminated backend | 57014 / 57P01 plus connection loss; neither is empty success |

The September comments conflated two boundaries. Privilege elevation alone did
**not** escape an intact READ ONLY transaction in these tests. The transaction
escapes were real and were tested separately. psql `-c` used simple Query;
preceding the next complete query with psql `\bind` forced extended protocol.
A final ROLLBACK can leave psql's exit status zero after a query error, so raw
SQLSTATE and persisted rows, not shell exit alone, determined these outcomes.

### SQL Server: fixture rights and the database boundary

The apply identity owned `app`; fixture/assertion logins were non-sysadmin,
without bulk rights or permanent CREATE/ALTER. Assertion had SELECT and an
explicit grant to an EXECUTE AS OWNER writer. Actual login, effective table/
tempdb permissions, errors and persisted rows were recorded.

| Case | Observed outcome |
|---|---|
| Fixture managed DML / reference INSERT | Allowed / 229 |
| Permanent CREATE TABLE, PROCEDURE or FUNCTION, including master table | 262 |
| CREATE SCHEMA | Refused, 15247 and 2759 (not the harness's predicted 262) |
| `#temp` table and `#temp` procedure, no CREATE grants | Both create/use successfully |
| Same helpers after explicit tempdb DENY CREATE TABLE/PROCEDURE/FUNCTION | Still work; ordinary tempdb dbo CREATE TABLE remains 262 |
| Explicit identity INSERT / SET IDENTITY_INSERT without ALTER | 544 / 1088 |
| INSERT parent OUTPUT INTO #ids; INSERT child SELECT generated id | Two children correctly linked |
| `OPENROWSET(BULK '/var/opt/mssql/data/issue1164-sentinel.txt', SINGLE_CLOB)` | Admin reads sentinel; fixture/assertion 4834 |
| Direct assertion INSERT in READ_WRITE database | 229 |
| Granted EXECUTE AS OWNER writer in READ_WRITE / READ_ONLY | Writes / 3906 with zero persisted rows |
| Persistent writes to READ_ONLY database or snapshot, including admin | 3906 |
| `#temp` table/procedure with READ_ONLY database or snapshot | Usable; tempdb stays writable |
| Source update after snapshot | Source changes; snapshot retains old value |
| Empty query / one NULL row / divide by zero | Zero rows / one row / 8134 |
| sqlcmd `-t 1` with `WAITFOR DELAY '00:00:03'; SELECT 1;` | Prints `Timeout expired`, no row, **exit 0**; not an assertion pass |

The measured mode change was `ALTER DATABASE pbps_measure SET READ_ONLY;` from
master after all normal runner clients exited: **725.493 ms** including container
CLI/client login. Forced `SET READ_ONLY WITH ROLLBACK IMMEDIATE` took **3785.961
ms** and disconnected a deliberately held fixture connection. Snapshot creation
took **1410.323 ms** and preserved that connection. Connection identity was
tracked as session id **plus connection GUID**, since session ids can be reused.
These single-run wall times are observations, not a performance promise.
The snapshot's 8 MiB logical backing file occupied 16 KiB, then 32 KiB after one
source update. This supports plain READ_ONLY after draining, not forced eviction.

### Evidence identity and limits

Retained raw JSON identifies every case's exact SQL, login, protocol, expected
result, stdout/stderr, elapsed time and post-observation. There are 39 cases
per PostgreSQL version and 52 SQL Server cases. Content-addressed evidence:

| Evidence content | SHA-256 |
|---|---|
| PostgreSQL 18.6 raw cases | `956b3df49007f2698d6b8f409364e990514a6b44cbc5f3837e799c76b6d788fe` |
| PostgreSQL 16.15 raw cases | `945da1879c2965252c12036c8edc6435e1ffb43e2ddb7357f46f40c9e6e88d08` |
| SQL Server final raw cases | `823e7fdf34f00fafb3d7d41f6906d8fe3d25a044931e375ea766c79bbdf33564` |
| Complete 130-case catalog (SQL, identities and raw/post observations) | `2a61608e12047426ff3e37aa307d42e0178710340d6b51a5dda2052442228998` |
| PostgreSQL setup/primary measurement script | `7565063b2b3330d657bc24bf616c67c8be794796efcf09cbda3a7083d193000e` |
| PostgreSQL extended/privilege follow-up script | `386910aa680c6091c1e3680b5d889225a497c43793b99dd48b31c80505ae39de` |
| SQL Server final measurement script | `eab25813bf0dfa2b8251972db6b10e8d51b01b64e01518ac37cb23d0e3fe26a2` |

Cases `repin_equal_rows_different_allocator_state` and
`repin_later_fixture_exposes_allocator_difference` in each raw set establish
§5's counterexample. These measurements qualify the engine facts above, not a
runner implementation, transaction-identity API, comprehensive allocator reader
or host/network sandbox. #1165 must live-test those implementation mechanisms.

One SQL Server startup failed at `inotify_init` (errno 24), not OOM; the shared
host's per-user inotify limit was 128. The failed container and all completed
measurement containers were removed with their volumes. One partial SQL run
had an observer using only a reusable session id; it was preserved, corrected
to include the connection GUID, and rerun. The final raw set retains its two
expectation mismatches (CREATE SCHEMA's error number and sqlcmd's exit-0 timeout)
rather than claiming a wholly green harness. No further SQL Server container
measurements are needed by this documentation change.

## 9. Results and CI semantics

Both human and JSON output retain each phase outcome, even when a later failure
dominates. `--format json` emits exactly one existing §9.8 envelope with
`command: "test"`, normal typed findings and a typed `data` branch. `operation`
is `run`, `repin` or `cleanup`. Do not invent a second report protocol.
For re-pin, add named row/allocator comparison phase records and manifest-write
status; for cleanup-only, omit inapplicable SQL phases rather than inventing
an executed candidate. The shared outcome/precedence rules still apply.

`data` includes §2's input/engine identity, run id and recovery records; ordered
phase records (`input`, `provision`, `bootstrap`, `fixture`, `replay`,
`candidate_plan`, `candidate_apply`, `assertion`, `cleanup`); replay costs;
candidate sealed and accepted/executed checksums; per-assertion path/hash,
outcome, column metadata, evidence rows and truncation; and cleanup's resource
confirmation/diagnostics. Each phase has an outcome (`passed`, `failed`,
`unanswerable`, `skipped`), elapsed time, relevant fixture/assertion/hop identity
and findings. Never mark a skipped assertion passed. Partial engine/plan fields
are null with a reason; no made-up checksum after failed planning. Distinguish
an accepted plan from one whose statements actually started, and retain the
ordinary executor/ledger result for interrupted/staged execution.

Per-assertion outcomes are `pass`, `violation`, `sql_error`,
`boundary_violation`, `unanswerable` or `skipped`. Empty success requires both a
complete readable result and the verified boundary. A returned-row violation
and later cleanup failure are both retained. Stop work on the first failing
phase/assertion; mark subsequent work skipped and always attempt cleanup.

| Completed observation | Phase/finding identity | Envelope / exit |
|---|---|---|
| All work and confirmed cleanup pass | `test` success | `ok` / 0 |
| Known input/intent/ancestry refusal | `test.input` | `findings` / 2 |
| Fixture SQL rejected by engine, including permissions/constraints | `test.fixture` | `findings` / 2 |
| Ordinary replay/candidate checksum, stale baseline, policy or risk refusal | `test.plan` or `test.apply`, retaining ordinary finding IDs | `findings` / 2 |
| Assertion returns rows | `test.assertion.rows` | `findings` / 2 |
| Assertion SQL error, invalid shape or transaction escape | `test.assertion.sql` or `test.assertion.boundary` | `findings` / 2 |
| Unreadable/unknown-version input, image/start/connect/read failure, transport loss, driver/deadline timeout | Phase-specific `test.input`, `test.infrastructure` or `test.assertion.unanswerable` | `unanswerable` / 1 |
| Any unconfirmed/failed cleanup, even after assertions pass | `test.cleanup` | `unanswerable` / 1 |

A driver/transport failure during a transaction postcheck records that the
boundary could not be verified and is unanswerable; a proven changed/ended
transaction records the boundary violation. Neither can qualify an assertion.

Unanswerable dominates findings, which dominates ok. Preserve the original
fixture/assertion/apply finding when cleanup fails; do not replace its cause
with a generic infrastructure message. A re-pin mismatch is a finding (2);
a comparison that cannot be established is unanswerable (1); neither moves the
pin. A successful cleanup-only invocation can be ok even if it reports a prior
interruption, but it does not retroactively turn that interrupted test into pass.

Human output gives the tested base/candidate, fixture pins, replay cost,
executed candidate checksum, assertion pass/violation/error counts and cleanup
status, with bounded evidence and the next remedy. JSON and human output
classify the same facts; successful phases never imply overall success early.
For example: `assertions: 1 pass; cleanup: failed; result: unanswerable (exit 1)`.

An illustrative failure envelope (nested identities/phase detail omitted
here for space, required in the implementation's generated type) is:

```json
{
  "schema_version": 1,
  "tool_version": "<running version>",
  "command": "test",
  "result": "findings",
  "findings": [{
    "id": "test.assertion.rows",
    "severity": "error",
    "message": "preserved-orders returned a violation row",
    "location": {"file": "tests/assertions/preserved-orders.sql"}
  }],
  "data": {
    "operation": "run",
    "assertions": [{
      "name": "preserved-orders",
      "path": "tests/assertions/preserved-orders.sql",
      "sha256": "<exact input hash>",
      "outcome": "violation",
      "columns": [{"name": "code", "type": "text"}, {"name": "total", "type": "integer"}],
      "rows": [["kept", 1164]],
      "truncated": false
    }],
    "cleanup": {"outcome": "passed", "confirmed_absent": true}
  }
}
```

Adding this serialized command branch changes the published schema set.
**#1165 must bump `integration::SCHEMA_VERSION` to the next free set and archive
a complete generated set** (config, declarations, envelope) per DECISIONS 465,
with schema command/checked-in output parity and archive checks. The envelope's
wire `schema_version` follows §9.8's compatibility rule (currently pre-release
version 1); it is not the schema-set number. This ADR-only change modifies no
serialized types or generated schemas and makes no version bump itself.
#2401 must similarly version its new plan/checksum/ledger evidence when delivered.

## 10. Passing and failing examples

The following examples use a base containing managed parent/child tables, with
an unchanged unique business key `code`, and a candidate that renames the
parent's `amount` to `total` with recorded uid/rename intent. The fixture stays
pinned to the old schema and the ordinary plan preserves its values. They are
executable SQL inputs for #1165 acceptance, not fabricated runner log output.

### PostgreSQL

For the §2 manifest, use a base table `app.orders(id bigint GENERATED ALWAYS AS
IDENTITY PRIMARY KEY, code text UNIQUE, amount integer NOT NULL)`. Record the
rename using the ordinary intent command; the default candidate reads those
working-tree declarations/ids before a commit. Fixture:

```sql
INSERT INTO app.orders(code, amount) VALUES ('kept', 1164);
```

Passing `assertions/preserved-orders.sql` at candidate:

```sql
SELECT code, total FROM app.orders WHERE code = 'kept' AND total <> 1164
UNION ALL
SELECT 'missing', NULL WHERE NOT EXISTS
  (SELECT 1 FROM app.orders WHERE code = 'kept');
```

Run `pbps test --manifest tests/pbps-tests.yml --format json`. The pinned
fixture loads against `amount`; the candidate assertion sees `total`, returns
zero rows, and confirmed cleanup permits `ok`/0. To demonstrate failure, change
`total <> 1164` to `total <> 999`; the returned `('kept',1164)` is bounded evidence,
`test.assertion.rows` and `findings`/2, with cleanup still reported separately.

A fixture `CREATE TABLE app.forbidden(n integer);` fails as `test.fixture`/2.
An assertion `COMMIT; INSERT INTO app.orders(code,total) VALUES ('escape',1);`
fails with 42601; a standalone COMMIT fails the transaction/result boundary and
is never followed by another file on that connection. A division-by-zero query
is a SQL error, not pass; `SELECT NULL` is one violation row, not empty.

### SQL Server

Use `dialect: mssql` and the SQL Server digest from §8. Base tables include
`app.orders(id int IDENTITY PRIMARY KEY, code nvarchar(40) UNIQUE, amount int
NOT NULL)` and `app.lines(order_id int REFERENCES app.orders(id), qty int)`.
Fixture at base, with no hard-coded generated identity:

```sql
CREATE TABLE #ids(id int NOT NULL, code nvarchar(40) NOT NULL);
INSERT INTO app.orders(code, amount)
OUTPUT inserted.id, inserted.code INTO #ids(id, code)
VALUES (N'kept', 1164);
INSERT INTO app.lines(order_id, qty)
SELECT id, 2 FROM #ids WHERE code = N'kept';
```

Passing candidate assertion after the same recorded `amount` → `total` rename:

```sql
SELECT o.code, o.total
FROM app.orders AS o
WHERE o.code = N'kept' AND
  (o.total <> 1164 OR NOT EXISTS
    (SELECT 1 FROM app.lines AS l WHERE l.order_id = o.id AND l.qty = 2))
UNION ALL
SELECT N'missing', NULL WHERE NOT EXISTS
  (SELECT 1 FROM app.orders WHERE code = N'kept');
```

Zero rows plus READ_ONLY and confirmed cleanup is `ok`/0. Changing the expected
quantity from 2 to 3 returns the parent as violation evidence and exits 2.
Explicit identity INSERT, or enabling IDENTITY_INSERT without ALTER, fails as
a fixture error. An assertion attempting persistent INSERT fails (permission
or read-only error), not pass; a driver query timeout exits 1, even with no rows.

### Checksum, stale state and cleanup controls, both engines

#1165's lifecycle tests must fault the internal handoff, without adding public
unsafe flags: supply a wrong approved checksum to the unchanged sealed candidate
and observe ordinary checksum refusal before its statements; change the managed
baseline after planning and observe stale-plan refusal under the apply lock.
Neither may rewrite/reseal the artifact or publish test success. Report sealed
checksum, refusal and skipped assertions, then cleanup. A valid control must
execute that same accepted candidate and record its actual checksum.

Make all assertions pass but fail/timeout container removal: assertion outcomes
stay pass, cleanup is failed/unanswerable, the envelope is unanswerable and exit
is 1. SIGKILL after creation leaves an owned record/label recoverable by a later
`pbps test --cleanup`; foreign labels, live owners and name/ID reuse are never
removed. These controls pin the difference between passing SQL and a completed
successful test run.

## 11. Implementation acceptance and consequences

#1165 may be split by engine, but closes only after both satisfy this contract:

- Manifest failures occur before provisioning; exact hashes/commits survive
  every report, including partial failures. Unsupported versions/ambiguous refs,
  changed inputs, unknown files and SQLCMD commands do not silently pass.
  An uncommitted candidate (including new files and changed ids/config) works
  before commit, records HEAD plus every input hash, and is labelled uncommitted;
  an explicit committed candidate is distinguished for CI policy.
- Live passing and deliberately failing preservation/domain assertions use the
  fixture replay and ordinary candidate checksum/apply path on both engines.
  Wrong checksum, stale baseline, missing intent and unsupported modes retain
  normal refusals. No applyable file exists on any success/failure path.
- Pinned rename/drop/conversion replay, merged-but-not-first-parent ancestry,
  incomparable pin refusal and group order are covered. Re-pin compares full
  rows plus next allocator values, including owned/default sequences and
  identities; the measured equal-rows/different-next-id case must fail.
  Unreadable/non-standard allocation, other-group reordering and atomic manifest
  publication failures leave pins unchanged. No test generates fixture SQL.
- Actual fixture/assertion identities have the intended privileges, `data:`
  writes and permanent CREATE are refused, temporary helpers work, and host
  program/bulk negative tests have admin positive controls. SQL Server's
  OUTPUT/INSERT SELECT example works without ALTER.
- PostgreSQL covers simple multi-statement escape, extended 42601, standalone
  COMMIT/ROLLBACK across consecutive files, leading READ WRITE, unchanged
  transaction identity, and privileged definer writes inside/outside READ ONLY.
  Removing the identity/postcheck/first-read protections makes their tests fail.
- SQL Server closes owned clients before READ_ONLY, verifies the mode, refuses
  persistent owner-procedure writes and retains temporary-object behavior.
  #2401 separately refuses write-capable target credentials, including
  procedure EXECUTE through ownership chaining; it never switches target mode.
- Zero rows, NULL row, multiple/no result sets, bounded violations, SQL errors,
  transport loss, driver timeouts and unreadable results retain distinct facts
  and correct exit codes. Cleanup failure always prevents overall success.
- Normal removal, deadline, cancellation, SIGKILL before/after ID bookkeeping,
  active concurrent runs, runtime/volume inspection failure, name reuse and
  foreign resources qualify cleanup/recovery. No fixture/assertion runs on a
  user-selected database or with admin credentials.
- JSON schema generation/archive parity and human/JSON classification agree.
  #1166's external data-work evidence is separate from this schema-test result;
  #2402 records external work without this command executing it.

This adds orchestration at the CLI boundary, reusing revision loading, typed
planning, sealing/admission, drivers and the shared resolver scratch. It adds
no production apply hook, policy execution language or alternative emitter.
Pinned replay has an explicit time cost; verified re-pin trades that cost for
reviewed fixture maintenance. Scratch evidence remains sample evidence, while
production approval, probes and the ledger retain their existing responsibilities.
