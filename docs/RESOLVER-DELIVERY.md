# Engine-assisted planning delivery

Tracking parent: [#595](https://github.com/pongbiphang/pbps/issues/595).
The requirements are [ADR-0016](ADR-0016-engine-assisted-planning.md) and
[SPEC §9.3.2–9.3.3](SPEC.md#932-engine-assisted-planning-postgresql-planning-and-apply-delivered).
This plan divides their implementation without reducing the acceptance bar.
The parent stays open until both engine adapters are delivered. Each bounded
child follows the repository's full local verification, review and CI gates.

**Re-planned by #1528 and #1636 (DEC-1528.1, DEC-1636.1).** There is one
resolver, on a server or Docker scratch the operator vouches for (#1672,
#1674), with external objects from a reviewed externals file (#1673). The measured
profiles (#608, #609) were removed by #1636, and the confidential source
transfer (#617) is closed. #615 is delivered for PostgreSQL planning by
#1514–#1517: the resolver's plan is published after the fresh
pre-publication recheck, from the sealed order (DEC-1516.1), and `explain`
shows its evidence offline (DEC-1517.1). #616 applies it in one transaction
(DEC-616.1, below). The remaining order:
1. [#1778](https://github.com/pongbiphang/pbps/issues/1778) is delivered:
   the draft command, `pbps resolver externals` (DEC-1778.1), and fill of
   what the externals file lacks (DEC-1778.2);
2. SQL Server: [#620](https://github.com/pongbiphang/pbps/issues/620)
   implements [the measured design](RESOLVER-MSSQL.md) #619 delivered, then
   [#621](https://github.com/pongbiphang/pbps/issues/621).

The [acceptance mapping](RESOLVER-ACCEPTANCE.md) is #2487's A slice of #621.
Its mechanical lint checks names and ownership, not semantic coverage or
execution. #2488 owns plaintext/dictionary trace controls; #2489 follows #2144
for the final cross-engine run audit and #595 close-out. An accepted pending
cell remains open; a named test or a green job that skipped it does not fill it.

The sections below record each delivered step as it shipped. Where one
describes containment, process observation, executable identity or verified
channels, it describes the measured profiles #1636 removed.

## First implementation: read-only discovery (#597)

Connected `doctor`, in human and JSON output, now reports:

- PostgreSQL server version, database encoding/provider/locale/collation
  metadata, installed extension names/schemas/versions, and selected settings
  and identity observations of the introspection connection.
- SQL Server product/build/update/edition/family, server/database collations,
  database compatibility level, and selected session settings and identity.
- Known official image-family suggestions, explicitly unverified. Unknown
  versions and hosted SQL Server families get a reason instead of a guessed
  boxed image. No image is inspected, pulled or started.
- Unknown requirements for per-statement deployment settings/authorization,
  the candidate's compatibility, scratch separation and binding coverage.
  #1636 removed the measured profiles' requirements (executable builds,
  verified transport, containment, run stability and private input
  handling) from this list.

`session_*` observations do not describe the future apply context. Persisted
module settings, per-object collations and complete authorization/prerequisite
inventories remain later work. SQL NULL
is not a known absence; query failures are unanswerable rather than empty
reports. This inventory is advisory and cannot be reused as a coherent sealed
capture. Ordinary readiness does not require a resolver.

This is partial coverage of ADR acceptance cases **5 and 6**, not completion
of either case or the shared-environment stage. Binding resolution is still
unimplemented on both engines. Existing planning protections remain in force.

## Named selection and policy (#606)

Target planning accepts `--resolve-with <profile>`, overriding the selected
environment then project default. Tagged Docker/server profiles carry explicit
trusted image/pull policy or a separate credential-variable reference. Selection
looks up no credentials, acquires no resources and certifies no compatibility.
The connected summary reports the selected policy as `not_acquired`; it never
enters the saved deployment artifact. Since #1515 a plan the lightweight
assessment answers reports `resolver_assessment` and opens nothing the
resolver owns, and one that needs the resolver runs it (DEC-1515.1). Offline/check/explain and doctor keep
their no-acquisition behavior; existing planning protections remain in force.
This delivers the policy portion of ADR case 6, not the lifecycle or binding
path. See SPEC §9.3.2 for configuration and the current limitation.

## Verified TLS foundation (#607; removed)

#607 delivered `PeerVerifiedConn`, a connection type that refused any
string not asking for a verified handshake. Its only users were the measured
profiles, and #1636 removed it with them (DEC-1636.1). The resolver uses
whatever TLS each connection string asks for, through the ordinary
connection. The disposable TLS fixtures stay: they now pin that ordinary
connection's chain and host-name verification and its rejection of altered
or replayed records, in both local engine scripts and CI.

## Measured runtime profiles (#608, #609; removed)

The measured Docker profile `linux-amd64-v1` (#608) and the dedicated-server
profiles `linux-dedicated-v1` and `linux-dedicated-pg16-v1` (#609) measured
containment, observed the engines' processes and sockets, and compared their
executables. #1636 removed them with their code, CI job and fixtures
(DEC-1636.1). The resolver's server scratch (#1672) and Docker scratch
(#1674) replace them; see SPEC §9.3.2.

## PostgreSQL target input capture (#612)

The private library captures actual target bindings, complete requested
candidate/property closures, recorded identity/state and authorization on an
owned PostgreSQL 16/18 snapshot. Fresh rendering/session checks and native
executable qualification bound inputs outside that snapshot. Fresh recapture
reports changed logical inputs without exposing source or fingerprints.
See [the capture boundary](RESOLVER-CAPTURE.md) for supported surfaces, named
refusals, confidentiality and verification.

This is the target-input portion of ADR cases 3, 4, 9, 15, 16 and 23. Desired
namespace reconstruction/compilation, complete planning and protected artifact
integration remain subsequent steps; no resolver-backed CLI plan/apply path is
enabled by this library.

## PostgreSQL desired namespace and binding comparison (#613)

The resolver compiles the declarations on its scratch database, as the
reproduced deployer or as the scratch login (SPEC §9.3.2), and compares what
they bind with the target's observed bindings. Only a scope that qualified as
verified can resolve, and a run resolves once. SQL Server is refused by name
until its adapter exists (#619, #620).

The namespace is the differ's bootstrap through the ordinary emitter. Tables
are created bare, foreign keys follow every table, modules keep the differ's
name-scan order, and defaults, CHECKs, index predicates and triggers come last,
when every routine they can name exists. A module that bound a name first made
nameable later is unresolved, not trusted (DEC-613.1). Grants, roles and rows
are not reproduced; none changes a binding.

Both sides are captured by the #612 rule under one scope derived from what
scratch bound: every candidate for those names along each surface's write path
and in the schema it bound into, plus every cast. Each shared surface is then
**unaffected** (equal logical bindings), **rebuild** (different bindings) or
**unresolved** with a named condition: a target candidate scratch did not
reproduce, an engine object whose properties differ, a role in a binding or a
compile-order ambiguity (DEC-613.2). Runtime-bound bodies are compared by
header and listed separately. External objects come from the externals file
(#1673); surfaces that could reach an object it does not stage are
unresolved rather than guessed. The comparison is an in-memory report of
logical identities; it publishes no plan, evidence or SQL (#614, #615).

The adapter's live suite runs on PostgreSQL 16 and 18: the motivating pair
with an unmanaged dependent, earlier-path candidates, other overloads,
qualified references, historical binding against a fresh bootstrap, object
number and rename controls, every expression surface, unmanaged and planted
candidates, a changed built-in cast, a missing prerequisite, a creation cycle
and a compile-order ambiguity. The resolver's own live suite runs the same
comparison on its server and Docker scratch.

## Ordered implementation issues

Each issue describes its scope, ADR acceptance cases, positive/negative tests
and dependencies. The issue loop is sequential: the current PR must merge
before the next issue is claimed. #597 is complete; #606 through #614 are
delivered, #608's and #609's profiles were removed by #1636, and #617 and
#618 are closed. The remaining order is at the top of this page.

| Step | Issue | Required outcome |
|---|---|---|
| 1 | [#606](https://github.com/pongbiphang/pbps/issues/606) | configure named profiles and lazy selection policy |
| 2 | [#607](https://github.com/pongbiphang/pbps/issues/607) | add peer-verified TLS connection primitives (removed by #1636) |
| 3 | [#608](https://github.com/pongbiphang/pbps/issues/608) | acquire and contain run-owned Docker environments (removed by #1636) |
| 4 | [#609](https://github.com/pongbiphang/pbps/issues/609) | qualify dedicated scratch-server lifecycle and exclusivity (removed by #1636) |
| 5 | [#610](https://github.com/pongbiphang/pbps/issues/610) | qualify analysis environment builds and deployment context |
| 6 | [#611](https://github.com/pongbiphang/pbps/issues/611) | qualify analysis environment builds and deployment context |
| 7 | [#612](https://github.com/pongbiphang/pbps/issues/612) | capture coherent binding inputs and complete prerequisite manifests |
| 8 | [#613](https://github.com/pongbiphang/pbps/issues/613) | reconstruct desired namespaces and extract covered creation bindings |
| 9 | [#614](https://github.com/pongbiphang/pbps/issues/614) | seal versioned evidence and deterministic cross-kind change ordering |
| 10a | [#1273](https://github.com/pongbiphang/pbps/issues/1273) | share scratch analysis lifecycle across supplied-server and run-owned container controls |
| 10b | [#1274](https://github.com/pongbiphang/pbps/issues/1274) | construct qualified resolver evidence for the shared analysis run |
| 10 | [#615](https://github.com/pongbiphang/pbps/issues/615) | integrate lazy target planning and pre-publication verification: delivered for PostgreSQL by #1514–#1517 |
| 11 | [#616](https://github.com/pongbiphang/pbps/issues/616) | verify sealed prerequisites and closing bindings during apply: delivered for PostgreSQL |
| 12 | [#617](https://github.com/pongbiphang/pbps/issues/617) | closed: the baseline replaces a source transfer (#1673) |
| 13 | [#618](https://github.com/pongbiphang/pbps/issues/618) | closed: its checks go to #616 and #621 |
| 14 | [#619](https://github.com/pongbiphang/pbps/issues/619) | design creation binding coverage and catalog consistency: delivered as [the SQL Server design](RESOLVER-MSSQL.md) |
| 15 | [#620](https://github.com/pongbiphang/pbps/issues/620) | implement qualified binding planning and transactional apply |
| 16 | [#621](https://github.com/pongbiphang/pbps/issues/621) | complete the cross-engine acceptance matrix and delivery documentation |

#594's legacy-reader boundary is no longer a prerequisite: #952 keyed the
fingerprints (DEC-952.1), so a resolver plan records no verifier to protect.
No target source is transferred, so no private-source controls are needed
(#617 is closed). The SQL Server binding design precedes
its adapter, and final acceptance covers both engines independently. Every
agreed stage needs successful supported cases; permanent refusal is not delivery.

## Coordination and boundaries

- [#952](https://github.com/pongbiphang/pbps/issues/952) keys every
  external-input fingerprint to the environment (DEC-952.1), replacing #594's
  protected-storage design; sealing records the key identifier (#614).
- [#587](https://github.com/pongbiphang/pbps/issues/587) owns existing `--dev`
  containment. Code reuse does not certify or remove preview workflows.
- Keep #230 / PR #542 and #232 / PR #546 protections until their tested
  replacements are delivered; this parent does not close them. #173 closed
  outside the resolver, as DEC-1756.1's connected refusal; PR #487 is closed.
- Engine adapters supply facts. The differ and `Dialect` remain pure;
  deterministic evidence stays outside semantic `Schema` equality. The final
  typed plan remains the sole SQL-emission input, sealed before human approval.
- No planning DDL on the target, production-row cloning, arbitrary image
  synthesis, semantic SQL parser, runtime workload discovery, snapshot-derived
  deployable plan, resolver-backed staged apply or new service/plugin engine.

## Sealed planning foundations (#614)

Saved-plan version 12 requires an explicit ordinary/resolved analysis contract.
Resolved evidence records versioned qualified-environment fingerprints,
authorization pre-/postconditions, complete input properties and candidate
membership/absence, current/desired logical bindings and the final typed order.
All persisted input fingerprints use the target environment's configured HMAC
key and identify that key without recording it. Missing keys name
`pbps key generate`. See DEC-614.1 and the amended DEC-952.1 contract.

The pure resolver planner combines proven bindings with structural, identity,
data, authorization and restoration constraints. It emits explicit typed
expression/module rebuilds, supports table-before-routine-before-expression
orders and refuses cycles. Ordinary plans retain their existing ordering.
A projected closing manifest permits approved catalog/grant changes while
preserving untouched external inputs. Each transitioned record must carry an
independently captured typed owner on its opening/closing prerequisite. An
inventory cannot confer ownership merely by listing an object. Aggregate table
and column owners and explicitly approved rename endpoints remain valid. Rename
owners resolve from the typed statement position to each snapshot separately,
following the recorded UID through intermediate names. A closing name reused
by another UID is never replayed as an opening name.
Every catalog mutation inventory must include the changed owner and all of its
affected qualified records in each applicable manifest. Table constraint/ACL
changes and column nullability changes affect the owner itself, including its
proved internal records, while preserving separately owned child surfaces.
Table/column lifecycles include their children; column type conversion also
includes its default. Independent child mutations retain their own authority. Only an approved creation/removal
can explain an absent endpoint; a grant on a newly created target remains valid.
An aggregate label cannot hide an omitted owner behind a listed child, or
authorize records outside the changed owners and their applicable endpoints.
For example, a table transition for a column default replacement may include
that default, but must preserve the other columns, defaults, checks and indexes.
References to external objects are not ownership. Raw catalog captures mark
ownership unqualified and cannot authorize transitions until #615's qualified
adapter bridge supplies that evidence. Missing ownership refuses on read.
Serialization, version and downgrade negatives accompany engine-backed creation, replacement and grant/data cases
on PostgreSQL 16 and 18.

These are internal foundations. Connected selection still does not publish a
resolved plan: #615 must connect the qualified lifecycle, complete adapter
ownership/binding inventory and fresh closing capture to artifact publication.
#616 must implement transactional pre-/postcondition checks; this build refuses
resolved apply before DDL until those checks exist. #619/#620 own the
independently qualified SQL Server adapter. No protected verifier table or legacy-reader
confidentiality proof is reintroduced.

Indexes extracted from a newly created empty table use the ordinary creation
strategy. A table-level online hint cannot turn that extraction into concurrent
DDL and invalidate the resolver plan's transactional boundary. Index changes
on existing tables retain their requested strategy.

## PostgreSQL planning integration (#615: #1514–#1517)

#615 shipped in four steps. #1514 read the target natively from `--db`;
#1515 assessed a plan lazily and opened a resolver only for a question the
assessment could not answer (DEC-1515.1); #1516 published the resolver's
plan after the ordinary connected checks and a fresh recheck of the target,
from the sealed order (DEC-1516.1).

#1517 makes the evidence reviewable from the plan alone (DEC-1517.1).
`explain`, in human and JSON output and with no project, key or resolver,
shows:
- which `pbps.yml` entry ran, its scratch kind and its layout, now sealed in
  the evidence's runtime (evidence version 4);
- the qualification's rule and key identifier, and the facts it seals, by
  name only;
- the limitations it accepted (#1700's `explain` half);
- each surface's verdict and every order edge with its reason;
- the prerequisites by identity and fingerprint presence.

The JSON is the field the human line and the viewer read.

Three gates refuse by name, as `resolver.unresolved`, before deployable
output exists: SQL Server resolution (#619, #620); `--staged` with a plan that
needs the resolver; and a retained external object of any candidate class
that the externals file does not stage, named with the declaration that could bind
it, whether or not the entry has an externals file.

## PostgreSQL resolved apply (#616)

`apply --plan` applies a resolved plan with the ordinary approval command,
through the target connection alone: no resolver runs, and no scratch or
Docker socket is reached, so it has no platform limitation of its own
(DEC-616.1). The key is checked before connecting, the cluster before the
deployment lock, and under the lock the target's facts, the deployer's
authorization and the opening manifest are captured in the apply's own
`READ COMMITTED` transaction before its first statement; after its last
statement the closing manifest, sealed from the approved changes, is
captured and compared before recording (SPEC §7.6). Each capture brackets
its signature lookups between a one-statement manifest read and a
one-statement witness read, the form the step 6 experiment on #616 chose.
`explain` offers the ordinary approval command. A staged resolved plan is
refused by name (#620).
