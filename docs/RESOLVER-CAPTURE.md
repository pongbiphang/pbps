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

The snapshot does not freeze PostgreSQL's catalog-rendering functions. A
private, same-capture tuple witness covers the catalogs that can supply their
names/properties. After rendering, the adapter commits and observes those
witnesses with a **fresh** read-only snapshot. The witness cursor holds that
new snapshot across all its fetches. A rename or change-and-restore
invalidates the read. Witnesses are deliberately conservative: unrelated
catalog writes can require another capture. These physical tuple coordinates
never enter logical fingerprints, cross-database binding comparison or any
saved artifact. There is no production DDL, production-data copy, target agent
or transaction held across image acquisition or scratch compilation.

Session settings whose lifetime is `user` are pinned transaction-locally.
Effective current/session roles are resolved against the snapshot's public
role rows. Configuration-reload observations and a closing session observation
bound the non-snapshot setting read; a missing required setting or unqualified
lifetime refuses. Actual provider versions are read for required collations
and the current database, separately from recorded catalog versions. Executable
content is qualified by the native boundary below, not by these SQL facts.

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

The closure includes class-specific type, cast, operator, routine, extension,
collation, relation and authorization properties; referenced prerequisites;
owned children; extension membership; and initial privileges/dependencies.
Explicit field dispositions distinguish scalar properties, logical references,
ACLs, canonical definitions and physical bookkeeping. NULL/default ACL and an
explicit empty ACL remain distinct. Public `pg_roles` supplies authorization
facts without reading password verifiers from `pg_authid`.

Actual stored node trees supply creation-time bindings, including pinned
builtins that `pg_depend` omits. Qualified surfaces include views, SQL-standard
routine bodies and supported header/default/CHECK/index expressions. Unknown
node fields, unsupported required object classes and unreadable definitions
refuse coverage. Runtime-bound string, procedural and C bodies are explicitly
reported as limitations; header/default observations do not certify their
whole-program dependencies.

Engine rendering is restricted to selected rows. Non-null constants and
missing-value arrays must have qualified output handlers before rendering;
type-modifier output is qualified too. The measured scalar handlers, enum,
array, domain and named-composite paths use known builtin routines. Unqualified
custom output, OID-alias constants and unsupported node/type surfaces refuse
rather than invoke arbitrary output code or hide an input as absent. Unrelated
unqualified objects are lookup rows, not certified or rendered prerequisites.

## Native connection and build boundary

`NativeTarget::capture_postgres` takes ownership of the whole bound connection
before its first await. Failure or cancellation drops it and expires existing
weak target witnesses. A canceled read cannot leave a transaction or cached
qualification available to a later scratch operation.

An initial coherent read supplies native-file requirements only. These include
C routines outside extension membership, extension libraries, preload settings
and the library search path. The lifecycle observes actual loaded content and
required late-load files, obtains a **fresh** coherent catalog capture, checks
that the input requirements stayed the same, and repeats executable/process/
socket checks. Unreadable loaded content, a disk copy substituted for a loaded
library, changed input or changed executable content refuses the result. The
returned capture is private and has no ordinary serialization.

This uses the existing native runtime's trusted-provisioning boundary. It does
not introduce a lifetime process census or claim to detect arbitrary hostile
administrator activity between target freshness checks. Scratch exclusivity
and reconstruction are separate obligations; target observations do not waive
them. `recapture_postgres` performs the complete fresh read/build qualification
again, without acquiring Docker resources.

## Private comparison and delivery limits

Rule `postgres-catalog-inputs-v1` defines the complete property and binding
representations. Comparisons use domain-framed SHA-256 fingerprints over
canonical properties, full engine definitions including literals, logical
bindings, recorded baseline and effective session inputs. Membership/absence
is explicit. Identity equality alone cannot hide a changed cast context, type
property, operator implementation/estimator, extension property or grant.

`CapturedInputs` and `CapturedTargetInputs` have no `Debug`, `Serialize`, public
verifier getter or persistence constructor. Ordinary comparisons return only
logical affected objects and failed conditions. These reports live outside
semantic `Schema` equality. Native-file requirements transfer privately to the
lifecycle; source-bearing data and guessing verifiers are not public output.

The capture closes before returning. A fresh recapture can invalidate it even
when managed declarations and historical bindings remain unchanged. Producing
an applyable plan still requires complete analysis scope selection, scratch
reconstruction/compilation, ordering and environment qualification. Publication,
explain, approval, apply and recording additionally require #594 and the
protected-output integration. This library adds none of those bypasses.

## Verification

The PostgreSQL live library fixtures run on both qualified majors, alongside
ordinary PostgreSQL integration tests. They cover historical bindings versus
fresh-bootstrap choices; missing builtin dependency rows; coherent rendering
and change-and-restore; unchanged input; candidate additions/removals; cast,
type, routine, operator and extension changes; authorization/session changes;
baseline states; unsupported layouts/versions; output qualification; local
setting restoration; and preservation of caller-owned transactions.

The disposable native TLS fixtures exercise loaded-content qualification,
disk-only refusal, fresh recapture, a real configuration reload between
component reads, and cancellation expiry. Protection-removal
controls accompany the catalog field, rendering witness/selection, candidate,
property, output, baseline-version, reload-epoch and connection-ownership
guards. No failed control is counted
as proof unless it reaches the intended regression, and restored code must pass.
