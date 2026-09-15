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
or permission to modify external production objects. Their full text is private,
ephemeral reconstruction input; only their fingerprints enter saved evidence
(decision 5).

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
user-supplied code can run. Before any resolver DDL or declaration transfer,
verify a versioned engine/platform execution-containment profile, for both
managed declarations and retained external definitions. Instance separation
and private source handling alone do not establish this profile.

The profile must deny workload-initiated access outside the isolated run,
including production services, the host network, cloud metadata and external
DNS/network destinations, even if copied source contains usable credentials.
Allow only the bounded incoming pbps database-control channel and its replies;
it must not provide a proxy for arbitrary workload requests. Trusted image and
extension acquisition happens before declaration transfer, outside the contained
compilation phase, under the acquisition policy in decision 4. That permission
does not grant the compiled workload network access.

Limit filesystem access to the qualified engine/runtime inputs and private
per-run scratch storage; write access stays within disposable run resources.
Do not expose project/home directories, production credentials, host devices,
container-runtime sockets or host mounts/privileges that escape this boundary.
The profile includes bounded resource use and lifetime, and is enforced outside
the SQL privilege boundary so routines, extensions, subprocesses or a database
administrator role inside scratch cannot relax it. A container label or SQL
permission check alone is not execution containment. Supplied servers need a
qualified, externally enforced boundary too, not just a separate database.

Verify effective controls before the workload starts and maintain them until
it and any descendants stop; reconnects or runtime replacement requalify.
Unknown or unenforceable controls refuse compilation before source transfer.
A containment violation or loss of control aborts the run, stops its workload
and enters cleanup without publishing evidence. Do not retry with broader
access. If faithful reconstruction requires blocked external effects, report
unsupported analysis rather than remove required semantics or fabricate a stub.
As with source handling, this assumes a trusted runtime and administrator;
supported profiles require real-engine/platform qualification, not a claim of
protection against every runtime vulnerability. No new sandbox service or
plugin execution system is introduced: these are admission requirements for
the existing planned resolver lifecycle. The resolver is neither a performance
test nor a production-data rehearsal, and does not replace apply's data probes.

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

**The provisioning administrator is not the deployment authorization context.**
Read and qualify the effective deployment role/principal and analysis-relevant
role membership, inheritance/impersonation, ownership, schema/object privileges
and default-schema/search-path visibility. The introspection login is not
automatically the deployer either. Reproduce the authorization context expected
at each covered deployment statement, including relevant preceding typed
authorization changes; a matching search-path string alone is insufficient.
Administrative credentials may prepare private scratch resources, but desired
compilation must use the qualified deployment context, not broader setup rights.

Reconstruct only the necessary authorization metadata using isolated run-local
roles/principals, preserving identity semantics where they affect resolution.
Do not copy production logins, authentication material or unrelated grants.
Each engine adapter must qualify the mapping and effective privilege checks;
do not assume another engine's role or ownership semantics. Missing/unreadable
privileges, unsupported impersonation or inability to reproduce the required
context prevents evidence. Seal the relevant authorization inputs in decision
5's manifest and recheck them before publication and against the actual apply
session before DDL. Reconnect qualification includes these facts; an equivalent
environment with broader privileges cannot silently pass. This is resolver
context reconstruction, not permission to modify target grants or a new
authorization-management feature.

Compatibility is an engine-specific, versioned rule with measured coverage,
not string equality on a Docker tag. Initially require matching engine/product
and relevant versions/settings unless a tested rule establishes a particular
difference is irrelevant to the covered analysis. "Same major" or "newer" is
not by itself an exemption. No unresolved required mismatch is downgraded to a
warning. SQL Server edition/capability checks still use the target; a Developer
scratch success cannot override them. Azure product versions are not boxed
SQL Server version selectors, and a Linux container is not automatically
equivalent to a deployment using platform-specific features.

**Qualification belongs to the actual backend/session and analysis run, not
the resolver URL.** Pin the concrete connection used for reconstruction and
binding reads. A reconnect, failover, pooled-session replacement or runtime
replacement invalidates its previous qualification and partial binding evidence;
it is not transparent recovery. Before further scratch DDL, declaration
transfer or evidence publication, re-establish instance separation, execution
containment, applicable source-handling controls and the full engine-specific
compatibility check on the replacement. Re-read version/product, extensions,
collation and all other required environment facts, and establish and verify
the effective deployment/session settings; do not inherit a "verified" result
from the old connection. Mismatch or unknown requirements refuse the operation.
Even a compatible replacement restarts complete reconstruction and compilation
in fresh run-owned scratch resources, discarding old partial results rather
than combining evidence across sessions. Publication must identify the backend
and qualification that actually produced the complete evidence and retain the
target-freshness recheck in decision 5.

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
uniquely named scratch databases on a separate instance/cluster.

**Isolation is an instance/cluster boundary, not a database-name check.**
Before any scratch DDL, including database creation, or transfer of retained
external source, establish that the resolver is outside the target PostgreSQL
cluster or SQL Server instance. Each engine adapter must qualify an identity
comparison using read-only instance/cluster facts and trusted provisioning or
endpoint evidence for its supported deployment types. Different database names,
credentials, connection strings or DNS aliases alone do not prove separation.
Reject the same instance/cluster and missing, unreadable or ambiguous identity
evidence without a write probe or target-side configuration change. Bind the
decision to the actual connections; reconnects, endpoint replacements and
failovers must requalify before further scratch DDL or source transfer.

Clean up only resources created for the run on success, failure or cancellation;
report cleanup failures without touching pre-existing objects. Do not propagate
target authentication material into scratch or serialize it in evidence.
Retained source stays private, and its derived fingerprints require the
confidential artifact handling in decision 5; absence of plaintext is not a
secret-free guarantee.

### 5. Seal evidence before approval; never resolve during apply

Extend the saved-plan format, not semantic `Schema` equality, to carry:

- the analysis/adapter version, coverage and unresolved limitations;
- the target facts required by the decision: logical identities, complete
  candidate sets within its read scope, relevant non-secret settings, and
  versioned canonical fingerprints of every required external resolution
  input's properties, including retained definitions without exporting source;
- the resolver's observed environment fingerprint and image digest/platform
  when applicable;
- the current/desired logical bindings and resulting explicit typed changes;
- a derived confidentiality classification covered by the checksum; external
  input fingerprints require confidential handling, never a caller-waived flag;
- typed descriptions of the derived pre-apply predicates and expected
  post-apply bindings, not user-supplied SQL to execute after approval.

**External source is reconstruction input, not a review artifact.** Unmanaged
function/view definitions can contain credentials or confidential literals.
Regardless of whether a secret is recognized, keep their complete source only
in private per-run reconstruction inputs and scratch resources, discarded on
completion or failure. Do not export it through saved-plan fields, generated
deployment SQL, `explain`, human/JSON diagnostics, logs or persistent caches.
Source-bearing engine errors must not bypass this boundary. This applies to
retained external prerequisites, not to the user's managed declarations and
explicit typed changes that the reviewed deployment already needs to contain.

**Qualify source handling before sending the source, not after an error.**
Before requesting private target inputs or sending them to scratch, qualify
the actual transport and peer on both legs, including apply's target re-reads.
Remote transport requires authenticated encryption with verified peer identity
using an approved trust root or pinned peer key/certificate. Plaintext,
opportunistic downgrade and encryption with certificate/identity checks disabled
do not qualify. An authenticated tunnel qualifies only if every remaining hop
is encrypted/authenticated or inside a qualified private boundary; an unverified
backend hop after TLS termination is not silently exempt. Pin this qualification
to the actual connections and re-establish it on reconnect or endpoint changes.

A local socket or run-private control channel may qualify through tested peer
authentication and host/runtime isolation instead of TLS, but `localhost`, a
container label or an exposed host port alone is not proof. Unknown protection
or peer identity refuses before a source-bearing query/transfer, not after data
has crossed the channel. Secure acquisition of per-run trust material must not
fall back to accepting any certificate. This extends the private-input resolver
path, not the defaults of unrelated existing connection workflows, and adds no
resolver connection during apply. Transport facts remain the connection layer's
responsibility; engine identity/compatibility and caller policy remain separate.

For both containers and supplied scratch servers, a versioned engine/platform
safety profile must establish effective suppression of source-bearing statement,
parameter, audit, trace and failed-statement logging. Cover the server and any
configured extensions, proxies, container stdout/stderr collectors, logging
drivers and forwarding destinations that can retain the reconstruction input.
Any unavoidable source-bearing diagnostic buffer must be private, per-run and
ephemeral, with no durable or remote export and cleanup on success, failure or
cancellation. Source-free operational diagnostics are still allowed. Private
scratch storage containing definitions or recovery records must likewise remain
within the qualified disposable lifecycle; dropping a database alone does not
establish that retained copies disappeared.

Verify the profile's effective controls and keep them enforced for the whole
source-bearing operation. An image tag, a user-supplied "safe" boolean, client
error redaction or a successful cleanup is not that verification. If required
controls are unknown, unreadable, incompatible or cannot be maintained, refuse
source-bearing reconstruction before transmitting any external definition and
name the unmet requirement without echoing source. Loss of an enforced control
during a run stops further source transfer, invalidates that run and enters
cleanup without publishing evidence. Do not disable production or pre-existing
shared audit/logging policies to make a resolver qualify. A supplied server
must already meet a supported profile; configuration changes
are limited to resources owned by this run under its provisioning policy.
Qualify these profiles with real-engine/platform tests, not an assumption that
one session setting controls every collector. This contract assumes a trusted
runtime and administrator, not protection against a hostile host secretly
recording inputs. A plan needing no external reconstruction does not acquire
this source-handling prerequisite merely because a resolver is configured.

**Pin all resolution inputs, not only objects with source text.** The versioned
input manifest covers every external prerequisite in the supported analysis
read scope, including relevant cast, type, operator and extension properties,
and the effective deployment authorization inputs from decision 4, whether or
not it is a direct current dependency. Each adapter defines the
complete class-specific properties required for reconstruction and resolution,
including referenced prerequisites, and qualifies that coverage before use.
Identity/candidate membership alone is not a fingerprint of those properties.
Record the complete per-class membership and required absence predicates too,
so additions/removals cannot disappear behind unchanged surviving identities.
Unknown or unreadable required properties, or an input whose relevant semantics
cannot be represented and rechecked, prevent verified evidence.

For each prerequisite, save its logical identity, a cryptographic digest and
the class-specific digest/canonicalization version needed to repeat the
comparison. Hash all required canonical properties, normalizing object
references to logical identities rather than physical or scratch OIDs.
Source-bearing inputs include their complete engine-canonical definition under
the recorded read settings, including literals; do not remove suspected
secrets before hashing and thereby hide changes to them. Canonicalization must
not guess SQL equivalence. A fingerprint detects changes; it is not encryption
or a promise that review artifacts are suitable for public disclosure.

**External-input fingerprints are confidential verifiers, not declassified
source.** Knowing the surrounding definition can let a reader test guesses of
a low-entropy literal against its digest. A public salt or another hash does
not remove that property. Conservatively classify every plan containing these
fingerprints as confidential/secret-bearing, without attempting to recognize
secrets in SQL. Validate this classification from the evidence shape; a missing
or downgraded label cannot make such an artifact valid for public handling.

Store and transfer it only through access-controlled artifact paths for people
authorized for the underlying inputs. Those reviewers still need no production
credentials and can review offline; lack of credentials does not imply lack
of confidentiality obligations. Refuse publication to an unprotected destination
or a workflow whose required confidential handling cannot be established.
Preserve existing output files on refusal. Artifact integration must define and
qualify its access controls rather than treat a warning as enforcement; this
does not create a new approval service or key-management system. pbps cannot
prevent an authorized recipient from redistributing a file after receipt.

Ordinary diagnostics, logs and `explain` report the classification, affected
objects and failed conditions without external digests or equivalent guessing
verifiers. Derived identifiers/checksums that expose an equivalent verifier
inherit the confidential classification; hiding the input digest while
publishing such a derivative does not declassify it. Any output carrying them
needs the same protected handling. Required evidence may not be removed to
manufacture a publicly shareable applyable plan.

**The plan checksum keeps its approval/audit job and inherits confidentiality.**
For these new confidential resolver artifacts, qualification must cover every
checksum consumer before the feature can publish a plan, not just `plan.json`:

- `plan` summaries, human/JSON `explain`, generated commands/SQL comments, UI,
  hooks and CI logs may show a literal checksum only through a qualified
  protected output. Ordinary output uses a clearly non-executable placeholder
  for the confidential checksum; it does not claim to supply the complete
  approval command. This condition qualifies SPEC §9.6's existing display rule.
- `apply --checksum` still receives the checksum a human approved, not a
  substitute token or one computed automatically by the deployment job. Its
  launch must use a qualified private runner/session whose process arguments,
  shell history/tracing and job metadata have authorized-only visibility. This
  boundary must hold before the checksum enters those paths; an `apply` check
  after process creation cannot undo an argument leak. An integration that
  cannot provide that boundary cannot run a confidential plan.
- The existing ledger `plan_checksum` and any snapshot copies remain the audit
  record. Before publication and again before apply DDL, establish that ledger
  readers and the applicable database audit/log/export paths meet the same
  authorization requirement. CLI redaction is not a substitute for protecting
  direct database reads. Unknown or broader exposure refuses the confidential
  operation; pbps does not silently change pre-existing grants or audit policy.
  Persist the classification with the checksum in versioned ledger/snapshot
  metadata; do not require the original plan file to recover it later.
  `status`, state/history/list/show/export, diagnostics and ledger fan-out must
  propagate the classification or omit the verifier from ordinary output.

The implementation must qualify this end-to-end propagation, including failure
paths, before enabling confidential resolver artifacts. Historical records that
cannot establish a checksum's safe classification must not assume it public.
Proven legacy formats without this evidence retain their ordinary-plan handling;
an absent label alone is not proof of that provenance.
These are handling requirements for the new evidence-bearing format, not a new
approval mechanism, ledger service or retrofit of ordinary plans. Plans without
this confidential evidence retain their existing checksum/CLI behavior.

During the coherent pre-publication and pre-apply captures below, re-read each
required target input's properties and recompute its fingerprint using the same
versioned procedure, along with rechecking membership/absence predicates. A
mismatch, missing/unreadable property or unsupported fingerprint version
prevents publication/application; a changed prerequisite requires replanning
and approval even if its identity, the managed checksum and existing bindings
are unchanged. This also protects a resolver's decision not to rebuild: the
closing observation of an old binding cannot waive a failed prerequisite.
Neither the original source in `plan.json` nor a resolver at apply time is
needed. The protected artifact carries the fingerprints; explanations name the
object and failed condition without echoing private source, properties or
verifiers.

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
10. **`external_definition_evidence_keeps_source_private` (planned):** use a
    retained external function/view whose source contains a confidential marker.
    Reconstruction can use its full definition, but the marker and source never
    appear in plan/SQL artifacts, `explain`, human/JSON diagnostics,
    durable/exported logs or persistent caches, including when scratch
    compilation fails. Changing only the literal changes the fingerprint and
    refuses publication/apply with the
    same object identity and candidate set. Missing/unreadable definitions and
    unsupported fingerprint versions do not pass; an unchanged prerequisite
    verifies without Docker or the original source in the saved plan.
11. **`resolver_rejects_the_target_instance_before_writes` (planned):** on both
    engines, point the resolver at another database on the target instance or
    cluster, including through an alias and different credentials. Refuse before
    database creation, source transfer or other scratch DDL. Missing/unreadable
    identity facts and a reconnect or failover to the target also fail closed.
    A separately identified, compatible scratch instance passes, with only
    run-owned resources created and removed; the target remains read-only.
12. **`resolver_source_logging_is_qualified_before_transfer` (planned):** use
    a confidential marker in a retained external definition and exercise server
    statement/audit/trace and failure logging, container stderr collection and
    forwarding. A persistent source collector or unknown controls refuse before
    source transfer; a qualified profile permits reconstruction without the
    marker reaching durable/exported logs or copies,
    including failed compilation and cancellation. Cover private ephemeral
    diagnostic/storage cleanup and its failure reporting. Losing an enforced
    control aborts without further source transfer or published evidence, and no
    pre-existing audit policy is disabled. Qualify Docker and supplied-server
    paths independently for each supported engine/platform; removing the
    pre-transfer gate must make the negative case fail.
13. **`resolver_compilation_cannot_escape_its_run` (planned):** exercise
    engine-supported creation-time evaluation from managed and retained
    external definitions, including extension/subprocess paths where supported.
    Use disposable network and filesystem sentinels, never real production
    services, to prove outbound connections, DNS/metadata access, host-file
    reads/writes and runtime-socket access cannot succeed, even with a marker
    credential in the source. Unknown controls refuse before transfer; violations
    and control loss stop the workload and publish no evidence. Cover descendant
    cleanup and resource/time limits; an ordinary contained declaration must
    still compile and produce its expected bindings. Qualify containers and
    supplied servers independently on each supported engine/platform, with a
    negative control showing the sentinel is reachable when containment is
    removed in the test fixture. Compilation requiring a blocked effect is
    refused without stubs or a less restrictive retry.
14. **`resolver_reconnect_requalifies_all_evidence` (planned):** replace a
    qualified resolver connection with a separate backend that still passes
    target-instance separation but differs in a required version, extension,
    collation or effective session setting. Each mismatch or unknown fact must
    refuse further DDL/source transfer and evidence publication; the old
    qualification cannot pass. Cover failover and pooled-session replacement.
    A compatible replacement must requalify all safety and compatibility inputs,
    discard partial bindings and restart complete compilation in fresh scratch
    resources; only the new backend's complete evidence may be sealed. Pin both
    engines with supported runtime fixtures and a negative control that fails
    when cached qualification or old partial bindings are reused.
15. **`external_resolution_properties_invalidate_stale_evidence` (planned):**
    produce an otherwise applyable plan whose resolver decides not to rebuild
    an affected object. Change an unmanaged cast's conversion context without
    changing its logical source/target identity, the overload candidate names,
    managed checksum or the object's existing binding. Establish on the real
    engine that fresh compilation now binds differently; publication/pre-apply
    rechecks must refuse stale evidence before applying or recording success.
    Cover identity-preserving relevant type/operator/extension property changes,
    input additions/removals, unreadable properties and unsupported coverage
    versions, with no concurrent writer during apply. Unchanged complete inputs
    still pass. Each supported adapter needs its own applicable fixtures; a
    negative control that checks only identities or routine/view source hashes
    must miss the changed-property case and fail the test.
16. **`resolver_uses_the_deployment_authorization_context` (planned):** use
    a least-privilege deployment role and a more privileged scratch setup role.
    On PostgreSQL, place competing objects on a path where the deployer lacks
    schema USAGE; establish the engine's effective visibility and binding, then
    require scratch to reproduce that result rather than the setup role's.
    Cover schema/object grants, ownership, inherited/switched roles and relevant
    per-statement authorization changes with engine-specific fixtures, including
    SQL Server's own principal/default-schema cases before enabling its adapter.
    Unreadable or unreproducible context refuses evidence; changed grants or a
    non-equivalent apply context invalidate it before DDL. A matching context
    must apply without a spurious rebuild/binding rollback, and running the
    compilation as the setup administrator must fail the negative control.
17. **`external_fingerprints_require_confidential_artifact_handling` (planned):**
    use known surrounding source and a small literal dictionary to demonstrate
    that candidate hashes can match the saved fingerprint. Such evidence must
    be classified confidential even without a recognized secret; public/unknown
    handling and a missing/downgraded label refuse publication while preserving
    prior files. Ordinary logs/diagnostics/explanations must omit the verifier
    and equivalent derived verifiers. An authorized, access-controlled artifact
    can still be reviewed offline and applied without new key-management or
    resolver access; omitting mandatory evidence cannot produce a public plan.
18. **`confidential_plan_checksums_stay_in_protected_paths` (planned):** trace
    a confidential plan's exact checksum through plan/explain human and JSON
    output, command previews, UI/hooks, CI trace/metadata, process arguments and
    shell history, ledger/snapshot persistence and status/history exports.
    Ordinary outputs must not expose it or print a runnable checksum command;
    protected review and launch must still use that exact human-approved SHA-256.
    An unauthorized direct ledger reader or unqualified audit/runner path refuses
    publication/application before disclosure or DDL, as appropriate, rather
    than passing because CLI output was masked. Cover failure paths, unknown
    historical classification and normal non-confidential plan behavior. Removing
    a consumer's protection must expose the fixture verifier and fail its test.
19. **`private_resolver_inputs_require_verified_transport` (planned):** cover
    both target capture/recheck and scratch transfer with disposable endpoints.
    Plaintext, downgrade, an untrusted/wrong peer and an unprotected backend hop
    must refuse before a private definition or property crosses the channel;
    prove this using a controlled interception fixture and confidential marker.
    Valid authenticated encryption and a separately qualified local private
    channel must pass. Include reconnect/endpoint substitution, failed trust
    setup and apply-time target re-reads without any scratch connection. A
    certificate-validation bypass or localhost-only exemption must fail the
    negative control, independently for each supported engine/transport.

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
