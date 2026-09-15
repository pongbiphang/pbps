# ADR-0016: Engine-assisted planning with optional, verified resolver environments

- Status: accepted design; not implemented. Delivery stages are listed below.
- Date: 2026-09-15
- Related: [SPEC §9.3](SPEC.md#93-the-dev-database-optional), §7.3, §7.6,
  §8.2, §9.8 and §11.5; [ARCHITECTURE.md](ARCHITECTURE.md);
  [ADR-0009](ADR-0009-postgres-modules.md),
  [ADR-0013](ADR-0013-postgres-reference-data.md); DECISIONS 490.

## Background

An unchanged declaration can mean something different after a namespace change.
ADR-0013 records a view retaining its old binding while a fresh bootstrap of the
same source picks an arriving relation or overload. The live catalog answers
what was bound, not what recreating the source would bind next.

Issue #230 / PR #542 exposed the other side: treating an arriving routine as a
possible relation reference can schedule an unnecessary rebuild, which then
hits an unmanaged-dependent refusal. Distinguishing every such reference by
expanding a definition scanner makes the scanner increasingly responsible for
the SQL grammar and resolution rules that the architecture deliberately leaves
to the engine. A conservative candidate is not an established dependency.

Existing `--dev` solves a different problem. It normalizes, bootstraps and
rehearses a preview on SQL Server, but cannot participate in target-aware
planning. This decision adds that participation under a separate contract;
it does not promote a dev-verified preview to a deployment artifact.

These are accepted requirements, not results from a new implementation or
experiment. The earlier ADRs supply the motivating measurements. The acceptance
tests below must establish that the new path works before it replaces any
existing protection. Using a real engine is a shared technical foundation with
tools such as Atlas, not evidence of feature parity or proof that another tool
handles every binding-only change.

## Decision

### 1. Three workflows, not one mandatory sandbox

Keep offline authoring/preview, optional preview rehearsal, and target-aware
planning distinct. `--resolve-with` selects an isolated engine for the last
workflow; `--dev` retains its preview-only meaning and target exclusion.
Configuration may select a resolver per project/environment, with an explicit
CLI selection taking precedence. No resolver configuration may cause `plan
--check` or offline `explain` to connect or acquire an image.

For supported creation-time binding questions, lightweight assessment returns
proven unaffected, proven rebuild, or requires resolution. The implementation
must establish why a fast-path answer is sound from the typed model and catalog
facts. A heuristic can widen the suspect set, but cannot prove safety by absence
of a guessed grammar pattern. Ordinary table changes are not categorically
exempt: defaults, checks and index predicates can have bindings too.

When required, run the configured resolver lazily. Without adequate evidence,
refuse deployable output and identify the affected declaration and remedy.
Conservative extra resolver requests are an accepted usability cost, not a
promise that every safe change is decidable without an engine. Do not turn an
uncertain case into an unconditional rebuild merely to avoid that boundary.
Offline reports retain uncertainty as advisory; runtime-only impact scans
retain their existing warning/refusal distinction outside this proof domain.

No `--allow`, suppressed warning, name qualification or `depends_on` annotation
is a substitute for required evidence. `depends_on` informs dependency ordering;
it cannot select an overload. A declaration repair must be assessed again.

### 2. Compare observed current bindings with compiled desired bindings

Read the target's actual managed state, identity mappings, bindings and required
external catalog context without DDL. Then build the desired namespace in
scratch and ask the engine what the declarations bound to. Compare logical
object addresses: class, schema, name, routine/operator signature and relevant
column/subobject identity, using existing uids where rename identity requires
them. Raw OIDs from separate databases never compare as identities.

Do not bootstrap old YAML and call it the target baseline. Historical bindings
may survive even though that bootstrap would bind differently. If a future
implementation also rehearses the migration from a reconstructed baseline,
that baseline must first match the target's observed bindings; rehearsal is
not required to manufacture the left side of this comparison.

The scratch namespace must include the relevant retained external objects,
extensions, types, operators, casts and candidate overloads as well as desired
managed objects. Following current dependency edges alone is insufficient:
an unbound candidate can become the selected overload. Captured external
definitions remain prerequisites, never another source of managed declarations
or permission to modify external production objects.

Use supported catalog reconstruction and engine-specific ordering to prepare
that namespace. Reconstructed retained context must preserve the relevant
observed identities, definitions and dependencies. An unavailable definition,
unreproducible dependency or unresolved creation-order cycle is a named limit,
not permission to introduce fake tables, stub functions or parser heuristics.
Only differences in established desired bindings can supply binding-driven
rebuild decisions. Their dependency closure, ordering, ACL/owner restoration,
unmanaged protection and risk classification remain ordinary typed planning.
No hidden statement and no `DROP ... CASCADE` is introduced.

### 3. Bound the evidence to what the engine exposes

The first binding adapter is PostgreSQL. Its intended surfaces are views,
SQL-standard function bodies and supported routine-header, default, CHECK and
index-predicate bindings. Each surface needs a tested, explicit coverage rule
before it can produce evidence. A successful CREATE alone does not establish
that all dependencies were observed; an empty catalog result is not a universal
negative proof, including where built-in objects have no ordinary dependency
entry.

String-bodied SQL, PL/pgSQL runtime references, dynamic SQL, and behavior that
depends on production data or unreproducible external state are not made
fully analyzable by installing Docker. Do not invoke routines or application
workloads to discover those dependencies. Report the existing limitations
without marking them resolved, and do not make every runtime-opaque routine
unconditionally require a resolver that could never answer its question.

DDL compilation can itself evaluate expressions or invoke extension code.
Isolation is therefore an operational trust boundary, not a claim that no
user-supplied code can run. The resolver is neither a performance test nor a
production-data rehearsal, and does not replace apply's data probes.

### 4. Discover requirements for both engines; verify the chosen environment

Environment discovery and candidate recommendation cover PostgreSQL and SQL
Server from the first delivery stage. Both adapters report a shared distinction:
observed requirements, a suggested candidate, verified compatibility for a
specified analysis scope, known mismatch, or unknown. Only the verified result
can support resolution, and environment compatibility alone is not binding
evidence. An unimplemented binding adapter must refuse that capability by name.

PostgreSQL discovery includes version, relevant extensions and their versions
and schemas, encoding, collation provider/locale/version, and the effective
deployment settings/search paths. SQL Server discovery includes product
version/build/update information, Edition and EngineEdition/product family,
server/database collation, compatibility level and relevant module/session
settings. Use the existing version/capability readers where possible.

Collect only analysis-relevant metadata, with explicit unknowns for unreadable
or unsupported facts. Do not copy all server settings, deployment credentials,
secrets, logins or production rows. Effective deployment settings, including
per-statement or persisted module settings where relevant, are not necessarily
the defaults of the connection used for discovery.

Compatibility is an engine-specific, versioned rule with measured coverage,
not string equality on a Docker tag. Initially require matching engine/product
and relevant versions/settings unless a tested rule establishes a particular
difference is irrelevant to the covered analysis. "Same major" or "newer" is
not by itself an exemption. No unresolved required mismatch is downgraded to a
warning. SQL Server edition/capability checks still use the target; a Developer
scratch success cannot override them. Azure product versions are not boxed
SQL Server version selectors, and a Linux container is not automatically
equivalent to a deployment using platform-specific features.

Suggest known official images or team-configured trusted images/registries;
do not search arbitrary registries or automatically assemble custom images.
Metadata can describe what is needed without identifying the production
installation's original image or guaranteeing any matching image exists.
Verify the running candidate, not just its advertised tag. If none qualifies,
report missing requirements and allow a suitable dedicated scratch server.

`doctor` shares read-only discovery and candidate reporting, but does not pull,
start or provision resources. Merely connecting to production is not opt-in to
those actions. Explicit resolver configuration/selection permits lazy
provisioning during planning under the configured acquisition policy. Support
local preloaded images, internal registries and a no-pull policy; offline
preview never consults a registry. Pin the image actually used by digest and
platform in evidence, not just a mutable tag. A server-backed resolver records
its observed fingerprint without inventing an image digest.

Run containers on the pbps host or CI runner. Scratch connections and
credentials are separate from the target, and a supplied server gets fresh,
uniquely named scratch databases. Reject using the target database as scratch.
Clean up only resources created for the run on success, failure or cancellation;
report cleanup failures without touching pre-existing objects. Do not propagate
production credentials into a container or include secrets in saved evidence.

### 5. Seal evidence before approval; never resolve during apply

Extend the saved-plan format, not semantic `Schema` equality, to carry:

- the analysis/adapter version, coverage and unresolved limitations;
- the target facts required by the decision, including relevant definitions,
  settings, identities and complete candidate sets within its read scope;
- the resolver's observed environment fingerprint and image digest/platform
  when applicable;
- the current/desired logical bindings and resulting explicit typed changes;
- typed descriptions of the derived pre-apply predicates and expected
  post-apply bindings, not user-supplied SQL to execute after approval.

Deterministic serialization and the plan checksum cover this evidence. The
implementation must bump affected artifact/wire formats and generated schemas;
readers unable to enforce the evidence contract refuse it rather than ignore
it. A plan that requires resolution but lacks complete evidence is invalid;
an optional metadata bag or a caller-supplied "verified" boolean cannot waive
that requirement. This documentation change does not itself bump any runtime
format or retroactively certify existing saved plans. No new snapshot-to-applyable
path, automatic export artifact or separate persistent approval store is
introduced.

**Each target-evidence capture must be coherent in its own right.** The initial
capture and pre-publication recheck each use a fresh engine-appropriate
consistent snapshot, shared by every query for snapshot-visible bindings,
definitions, identity/state records and candidate sets. Independent autocommit
queries or a read-only transaction at READ COMMITTED do not establish that
contract. Comparing two mixed-time collections does not make either one valid.

For PostgreSQL's owned planning captures, retain `REPEATABLE READ READ ONLY`
and transaction-local canonical settings (DECISIONS 250), keeping all relevant
catalog readers on that capture's connection and snapshot. Do not assume the
snapshot also freezes session/global settings or catalog rendering functions;
pin the applicable session settings and validate other required inputs, or
refuse evidence whose consistency cannot be established. The rendering-function
limit is recorded in [PITFALLS](PITFALLS.md#the-snapshot-the-rendering-functions-do-not-read-from).
The future SQL Server adapter must establish its own catalog-consistency
strategy and prerequisites before producing evidence, not assume that a
similarly named isolation mode covers all metadata.

Release the initial read transaction before scratch work, then perform the
fresh coherent recheck before publishing the plan. Do not hold a production
DDL lock or long transaction while downloading an image or compiling the
declarations. Failure publishes neither partial plan nor partial SQL and
preserves existing output files. SPEC §9.8 distinguishes a known requirement or
incompatibility (finding) from a failed read/provisioning operation (unanswerable).

`apply` checks baseline and resolution prerequisites in one coherent capture
under its existing deployment lock before DDL. The closing binding read also
needs a coherent, fresh view that includes the transaction's own changes; do
not freeze the whole apply at its pre-DDL snapshot. PostgreSQL's caller-owned
READ COMMITTED transaction uses the one-statement catalog capture and
revalidation boundary of DECISIONS 423, extended to the resolution-evidence
scope. A savepoint or repeated non-atomic reads are not a substitute. Relevant
external candidates must be included even when their change would not affect
the ordinary managed-schema checksum.
Changed premises require replanning and renewed approval, never rerunning the
resolver or choosing a different migration at apply time. Within the initial
transactional path, compare covered result bindings before commit and success
recording; a mismatch rolls back. Keep SPEC §7.6's single-deployer assumptions
and its other limitations. Resolver-backed staged plans are outside the first
delivery, not silently downgraded to weaker evidence.

`explain` reads evidence, prerequisites, rebuilt objects and uncovered cases
from the artifact without a project, credentials or a resolver. Approval stays
file-based and the final emitter remains the only source of deployment SQL.

## Architectural placement

The existing [crate boundaries](ARCHITECTURE.md#architectural-boundaries) remain:

- `pbps-cli` orchestrates profile collection, resolver selection/lifecycle,
  artifact publication and diagnostics through its `engine` dispatch seam.
- `pbps-pg` and `pbps-mssql` own their catalog SQL, scratch DDL, compatibility
  rules, binding extraction and verification. Connection-bound work remains
  free async functions routed by `engine`, not new I/O methods on `Dialect`.
- `pbps-db` owns transport and transaction framing, not engine SQL or Docker.
- `pbps-model` carries deterministic evidence/plan data, without credentials,
  driver types or environment annotations embedded in semantic `Schema`.
- `pbps-diff` stays a pure typed comparison. Resolver facts are explicit inputs
  to planning, never hidden network calls from the differ; emitters still
  consume the final typed changes.
- `pbps-config` owns source/trust/pull-policy configuration. The shared
  interface covers both engines without pretending their resolution semantics
  or compatibility tests are interchangeable.

## Acceptance tests

These are required future tests, not tests claimed to pass in this change.
Use real engines for semantics and negative controls that fail when the
protection under test is removed.

1. An arriving routine that cannot capture an existing view's relation
   reference does not rebuild that view, even with an unmanaged dependent;
   an arriving relation that really changes the binding does schedule the
   explicit rebuild and retains the unmanaged-dependent gate.
2. Earlier-path candidates, same-schema overloads, qualified references,
   routine-header references and supported expressions get the engine's
   actual answer. Replanning after successful apply does not repeatedly rebuild
   an unchanged object merely because a conservative candidate remains visible.
3. A target's historical binding differs from a fresh bootstrap of its old
   source. Comparison still uses the observed target binding. Cross-database
   OIDs do not create false equality or differences; renames use logical identity.
4. Missing external prerequisites, unreadable metadata, unsupported coverage
   and candidate sets omitted from reconstruction never yield verified evidence.
   Runtime-opaque and built-in dependency cases cannot mistake no catalog rows
   for a complete proof. Cycles or unsupported DDL fail without fake stubs.
5. Environment reports on both engines distinguish match, mismatch and unknown.
   Include PostgreSQL extension/path/collation mismatches and SQL Server
   collation/compatibility-level/edition differences, plus product families
   without a justified Docker mapping. A candidate tag is never a passed check.
6. Safe lightweight plans and offline commands do not invoke Docker, connect
   to scratch or access a registry. `doctor` only reports. Explicitly enabled
   provisioning respects no-pull mode, a local cache, internal image sources,
   separate credentials and per-run cleanup, including failed startup.
7. Changes to evidence alter the checksum. Missing/unsupported evidence versions
   are refused. A relevant external candidate or environment prerequisite
   changing before publication or apply invalidates the plan, even when ordinary
   managed drift is empty. Failed planning preserves prior output files.
8. Apply works without Docker and executes only the approved statements. Wrong
   result bindings roll back without recording success. Evidence can be
   explained offline; human and JSON refusal reports agree. No staged or
   unsupported-engine path bypasses the evidence requirement.
9. **`target_evidence_capture_never_seals_mixed_catalog_states` (planned):**
   use two connections to change a covered binding, definition or candidate set
   between component reads of the initial capture, pre-publication recheck and
   apply precondition capture. Each collection must represent one coherent
   state or fail; none may seal an impossible combination. Cover non-snapshot
   settings/rendering inputs separately, preserve prior artifacts on failure,
   and prove the closing read sees the apply's own DDL and uses the existing
   revalidation boundary. Qualify each engine independently; this test does not
   promise to prevent external DDL after the last observation.

## Delivery and supersession

1. **Shared environment support, both engines:** discovery, candidate reporting,
   compatibility checks and trusted/local acquisition infrastructure. Existing
   SQL Server preview rehearsal is a reusable capability, not already a binding
   adapter; environment support does not silently enable one.
2. **PostgreSQL resolver:** supported creation-time bindings, typed rebuild
   decisions, versioned saved evidence and transactional apply/result guards.
   Deliver each supported surface with its own coverage and live tests.
3. **SQL Server resolver:** shared infrastructure, but a separately designed
   binding adapter and real SQL Server tests before connected planning can use
   its evidence. PostgreSQL tests do not qualify it.

This supersedes ADR-0009's blanket exclusion of a scratch engine from connected
planning only for the new resolver path. It does not change that ADR's
drop-and-create strategy for PostgreSQL module changes, target-DDL prohibition,
restore obligations, or established-versus-name-only dependency rules.

For covered binding questions, it replaces ADR-0013's inference that every
candidate-set change must cause a rebuild with candidate-triggered resolution
and comparison of actual bindings. Until the implementation and tests land,
existing behavior and protections remain in force. This ADR neither removes
PR #542's protections nor claims its regressions have been solved.

Deferred: arbitrary image synthesis, complete production cloning, automatic
execution to discover runtime dependencies, snapshot-derived deployable plans,
resolver-backed staged apply, general SQL parsing, and changes to human identity
intent, risk approval or deployment hooks.

## Source references

These explain discoverable environment facts and existing engine limits; they
do not substitute for the acceptance tests above.

- PostgreSQL [version and session information](https://www.postgresql.org/docs/current/functions-info.html),
  [extension catalog](https://www.postgresql.org/docs/current/catalog-pg-extension.html)
  and [database locale metadata](https://www.postgresql.org/docs/current/catalog-pg-database.html).
- SQL Server [SERVERPROPERTY](https://learn.microsoft.com/en-us/sql/t-sql/functions/serverproperty-transact-sql)
  and [sys.databases](https://learn.microsoft.com/en-us/sql/relational-databases/system-catalog-views/sys-databases-transact-sql).
- Docker [immutable image digests](https://docs.docker.com/reference/cli/docker/image/pull/#pull-an-image-by-digest-immutable-identifier)
  and the [official PostgreSQL image's extension/locale configuration](https://hub.docker.com/_/postgres/).
