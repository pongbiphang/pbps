# PostgreSQL target input capture

The library boundary delivered by #612 is the target-input portion of
[ADR-0016](ADR-0016-engine-assisted-planning.md). It is not a CLI binding planner,
a saved-plan format, a scratch compiler or an enabled resolver-backed apply
path. [The delivery sequence](RESOLVER-DELIVERY.md) keeps those steps separate.
Existing planning protections remain in force.

## One capture, one catalog snapshot

`pbps-pg::resolver::capture::capture` takes an explicit `CaptureScope`: actual
retained logical roots and complete candidate predicates. A routine predicate
includes every overload sharing its name. A predicate with no matching members
records absence; it does not omit that predicate. A scope is a read request,
not a certificate that an arbitrary SQL analysis requested every necessary
candidate set.

The adapter refuses a caller-owned transaction before opening an owned
`REPEATABLE READ READ ONLY` transaction. It pins canonical rendering settings
locally, reads the actual recorded state and identity map, public authorization
catalogs, complete catalog descriptors and prerequisite rows on that snapshot,
and derives the requested closure before rendering definitions. Catalog rows
are fetched in bounded cursor batches on that same snapshot, with explicit
empty-catalog markers. Lookup rows remain in memory through closure resolution
and rendering, so total memory still scales with the catalogs read. Neither
unrelated catalog volume nor a supported recorded snapshot inherits an aggregate
JSON size ceiling. No old YAML is
bootstrapped as a stand-in for target bindings. Absent and empty ledgers are
separate inputs; unreadable, malformed or unsupported state is a refusal.

Before accepting either an empty or recorded ledger, capture reuses the
ordinary write path's complete recipe and effective-editor qualification
(SPEC §8.1, DEC-313.1 and its amendments). Both ledger tables and their
source/type closures qualify before recipe rendering, inside the capture's
owned canonical snapshot and rendering-stability boundary. No table creation,
migration or grant runs: supported older timeline-column subsets remain
unchanged. The baseline row comes from `ONLY public.__pbps_state`. A refusal
names ledger qualification without exposing private defaults or role details.

The snapshot does not freeze PostgreSQL's catalog-rendering functions. A
private, same-capture tuple witness covers the catalogs that can supply their
names/properties. After rendering, the adapter commits and observes those
witnesses with a **fresh** read-only snapshot. The witness cursor holds that
new snapshot across all its fetches. A rename or change-and-restore in a
tuple-witnessed catalog invalidates the read. Witnesses are deliberately
conservative: unrelated catalog writes can require another capture. These physical tuple coordinates
never enter logical fingerprints, cross-database binding comparison or any
saved artifact.

The public `pg_roles` view has no tuple-version witness. Its complete set of
qualified public fields, including role identities and properties, is compared
on that same fresh snapshot instead; neither `pg_authid` nor password verifiers
are read. Changed values and added/removed roles refuse the capture, as do
unreadable facts. Role-membership edges retain their physical witnesses in
`pg_auth_members`. Equality of public role values does not detect a change that
was fully restored before the closing snapshot; it is not a continuous
privilege-history certificate (DEC-882.1).

There is no production DDL, production-data copy, target agent
or transaction held across image acquisition or scratch compilation.

Session settings whose lifetime is `user` are pinned transaction-locally.
Effective current/session roles are resolved against the snapshot's public
role rows. Configuration-reload observations and a closing session observation
bound the non-snapshot setting read; a missing required setting or unqualified
lifetime refuses. Actual provider versions are read for required collations
and the current database, separately from recorded catalog versions. Executable
content is not compared (#1636).

## Logical identities, complete properties and coverage

The supported catalog/node layouts are qualified for PostgreSQL 16 and 18.
Actual catalog descriptors must match the known complete field layouts,
including the fields' type namespaces. Selecting known columns alone cannot
certify a server that added another resolution-relevant property.

Logical identities use a catalog class, separate identifier components,
logical signatures and named subobjects. Relation columns retain live semantic
order without persisting physical attribute slots. OIDs resolve only through
snapshot rows; live-cache name helpers cannot name an older snapshot's objects.
An unreadable required reference cannot become an optional identity slot.

Derived query outputs use local query/range positions. A whole derived row
retains its row type and every output label in order, checked against the
range's complete output count. The existing full-tree bindings and catalog
closure retain expression types, collations and routine/composite result
descriptors; a row label list alone is not a completeness certificate. Missing,
unreadable or truncated output shapes refuse coverage.

The closure includes class-specific type, cast, operator, routine, extension,
collation, relation and role properties; referenced prerequisites; owned
children; extension membership; and dependencies. Explicit field dispositions
distinguish scalar properties, logical references, canonical definitions,
authorization metadata and physical bookkeeping. Owners, ACLs, shared
(`pg_shdepend`) owner/ACL rows, initial privileges and default ACLs are
authorization metadata: they do not change what a creation binds, so they are
not read as prerequisites (DEC-1274.1). The deployer's effective privileges,
which do change name lookup, are the separate authorization condition. Public
`pg_roles` supplies role facts without reading password verifiers from
`pg_authid`. A constraint's internal triggers, whose names embed
their OID, are identified by relation, constraint and trigger function. A
table's TOAST relation, whose name embeds the table's OID, is out-of-line
storage that no expression binds: it and its index, columns and dependency rows
are physical bookkeeping and stay out of the closure (DEC-1274.1).

Actual stored node trees supply creation-time bindings, including pinned
builtins that `pg_depend` omits. Qualified surfaces include views, SQL-standard
routine bodies and supported header/default/CHECK/index expressions. Unknown
node fields, unsupported required object classes and unreadable definitions
refuse coverage. Composite assignments retain the written fields' logical
identities, even when their right-hand expressions read no fields. Stored
SQL/JSON constructors and predicates qualify on both majors; PostgreSQL 18's
SQL/JSON query and table nodes retain returning types, defaults, passing
expressions and nested paths. Runtime-bound string, procedural and C bodies are explicitly
reported as limitations; header/default observations do not certify their
whole-program dependencies.

Engine rendering is restricted to selected rows. Non-null constants and
missing-value arrays must have qualified output handlers before rendering;
type-modifier output is qualified too. The measured scalar handlers, enum,
array, domain and named-composite paths use known builtin routines. Scalar
qualification checks both the SQL function identity and its implementation
symbol, including builtin aliases and the specialized integer/OID vectors.
Measured network, geometry, XML, text-search and snapshot constants qualify
without permitting arbitrary type I/O. Range and multirange constants qualify
the exact generic wrappers and recursively qualify each subtype's output.
Range type-cache initialization prepares comparator, canonicalizer and subtype
difference callbacks before rendering bounds. Their dispatch must use internal
or SQL functions; callbacks that can load C/PL code are an explicit coverage
refusal, even though output does not execute those callbacks. Missing, ambiguous
or recursive range metadata also refuses coverage. Unqualified custom output,
OID-alias constants and unsupported node/type surfaces refuse
rather than invoke arbitrary output code or hide an input as absent. Unrelated
unqualified objects are lookup rows, not certified or rendered prerequisites.

## Native connection and build boundary (removed)

The measured profiles' native target capture (`NativeTarget::capture_postgres`),
its source-bearing `RuntimeInputs` capability and the executable checks it fed
were removed by #1636 (DEC-1636.1). The resolver captures the target over its
ordinary connection, and compares no executable.

## Private comparison and delivery limits

Rule `postgres-catalog-inputs-v2` defines the complete property and binding
representations; v1, which fingerprinted owners and ACLs, is no longer read. Comparisons use HMAC-SHA256 fingerprints, framed by the rule
and the input's component, over canonical properties, full engine definitions
including literals, logical bindings, recorded baseline and effective session
inputs. Membership/absence is explicit. Identity equality alone cannot hide a
changed cast context, type property, operator implementation/estimator or
extension property.

The key is `FingerprintKey::process()`: random, generated once per process and
never written anywhere (DEC-952.1). A capture's fingerprints are compared and
dropped inside the process that made them, so no fingerprint of a private input
exists that anyone without that key could test a guess against. A fingerprint
that is kept, such as sealed plan evidence, is made under the target
environment's key instead, loaded from `fingerprint_key_env` or
`fingerprint_key_file` (#614). A process-key fingerprint is never persisted,
and never compared with one made in another process.

`CapturedInputs` and `CapturedTargetInputs` have no `Debug`, `Serialize`, public
fingerprint getter or persistence constructor. Ordinary comparisons return only
logical affected objects and failed conditions. These reports live outside
semantic `Schema` equality. Source-bearing data and fingerprints are not public
output.

The capture closes before returning. A fresh recapture can invalidate it even
when managed declarations and historical bindings remain unchanged. Producing
an applyable plan still requires complete analysis scope selection, scratch
reconstruction/compilation, ordering and environment qualification. Publication,
explain, approval, apply and recording additionally require sealing the
evidence under the environment's fingerprint key (#614, DEC-952.1). This library adds none of those bypasses.

## Verification

The PostgreSQL live library fixtures run on both qualified majors, alongside
ordinary PostgreSQL integration tests. They cover historical bindings versus
fresh-bootstrap choices; missing builtin dependency rows; coherent rendering
and change-and-restore; unchanged input; candidate additions/removals; cast,
type, routine, operator and extension changes; role/session changes;
baseline states; unsupported layouts/versions; output qualification; local
setting restoration; and preservation of caller-owned transactions.

`pbps-db::resolver::capture::CaptureError` is the shared, source-free refusal
shape, re-exported at the existing PostgreSQL API path. Private captured
catalogs stay in the engine. SQL Server returns a named unsupported-capture
refusal. A raw route leaves an existing caller transaction untouched.

The live fixtures exercise fresh recapture, a real configuration reload
between component reads, and cancellation. Protection-removal
controls accompany the catalog field, rendering witness/selection, candidate,
property, output, baseline-version, reload-epoch and connection-ownership
guards. No failed control is counted
as proof unless it reaches the intended regression, and restored code must pass.
