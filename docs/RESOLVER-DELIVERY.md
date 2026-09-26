# Engine-assisted planning delivery

Tracking parent: [#595](https://github.com/pongbiphang/pbps/issues/595).
The requirements are [ADR-0016](ADR-0016-engine-assisted-planning.md) and
[SPEC §9.3.2–9.3.3](SPEC.md#932-engine-assisted-planning-accepted-not-implemented).
This plan divides their implementation without reducing the acceptance bar.
The parent stays open until both engine adapters and the confidential-path
prerequisite are delivered. Each bounded child follows the repository's full
local verification, review and CI gates.

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
- Unknown requirements for actual executable/native-library builds,
  analysis-specific environment compatibility, per-statement deployment
  settings/authorization, authenticated transport, instance separation,
  runtime containment, backend stability and binding coverage. Conditional
  private-input handling retains its source-privacy controls (#617); its
  fingerprints are keyed (DEC-952.1).

`session_*` observations do not describe the future apply context. Persisted
module settings, per-object collations, relevant native-library identities and
complete authorization/prerequisite inventories remain later work. SQL NULL
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
enters the saved deployment artifact. Offline/check/explain and doctor keep
their no-acquisition behavior; existing planning protections remain in force.
This delivers the policy portion of ADR case 6, not the lifecycle or binding
path. See SPEC §9.3.2 for configuration and the current limitation.

## Verified TLS foundation (#607)

`pbps-db::transport::PeerVerifiedConn` owns one connection whose TLS handshake
verified the certificate chain and expected peer name. It requires PostgreSQL
`sslmode=require` and SQL Server encryption with certificate validation. Failed
trust setup, plaintext/opportunistic settings and disabled peer checks cannot
construct it. Its opaque connection identity changes on reconnect; callers
cannot replace the underlying connection while retaining that identity.

Disposable PostgreSQL and SQL Server fixtures exercise trusted and untrusted
roots, wrong peer names, reconnects, successful relay traffic and corruption of
encrypted replies. These tests run in both local engine scripts and CI.

This is a library primitive, not resolver admission or an enabled CLI analysis
path. TLS verifies its peer; it cannot certify a backend hop behind a terminating
proxy. The Docker/server profiles in #608/#609 must establish actual runtime-
bound admission: every hop or qualified local private channel, read-only engine
identity comparison and instance separation, run binding and invalidation before
scratch DDL/source transfer. Full ADR cases 11, 19 and 20 remain with those
profiles. Ordinary connection defaults and discovery qualification are unchanged.

## Dedicated scratch servers (#609)

A server pbps did not provision is admitted by measuring a **named** externally
enforced profile, not by trusting its URL, its separate database or an
operator's assertion. `linux-dedicated-v1` covers both engines on native Linux:
an operator-started container of a known layout — an empty network namespace,
an unprivileged engine, cgroup bounds within the profile's ceilings, a
read-only image root and a mount table every row of which the profile names —
reached through a run-owned forwarder the way the Docker profile reaches its
own containers. Instance separation is decided from the actual processes
before the containment measurement and before any database, DDL or
declaration, so an alias of the target refuses as the target. Exclusivity is
the observed socket rows and holders in the engine's namespace plus a cumulative
engine session counter that only this run may have moved, so a session that opened
and closed between two checks still ends the run. Run-owned resources are one
uniquely named database and login, removed on every exit path, with
unconfirmed removals reporting exactly those names.

This is the lifecycle and exclusivity portion of ADR cases 6, 11, 13, 14 and
19–21 for supplied servers. It is not engine build or deployment-context
qualification (#610/#611), source handling (#617) or binding evidence: no
declaration is transferred and no SQL surface is exposed. See
[the runtime boundary](RESOLVER-RUNTIME.md) for the measured premises, including
#743's explicit trusted-provisioning assumption and non-exhaustive holder
observations (DECISIONS 533).

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

`ScratchRun::resolve` compiles the declarations on a qualified run's own
scratch database, as the reproduced deployer, and compares what they bind with
the target's observed bindings. Only a scope that qualified as verified can
resolve; a full run check brackets the compile and both captures, any failure
ends the analysis, and a run resolves once. SQL Server is refused by name until
its adapter exists (#619, #620).

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
header and listed separately. The analysis is managed-only: retained external
objects, including extension members, are not reconstructed until private
source handling exists (#617), so surfaces that could reach them are
unresolved rather than guessed. The comparison is an in-memory report of
logical identities; it publishes no plan, evidence or SQL (#614, #615).

The adapter's live suite runs on PostgreSQL 16 and 18: the motivating pair
with an unmanaged dependent, earlier-path candidates, other overloads,
qualified references, historical binding against a fresh bootstrap, object
number and rename controls, every expression surface, unmanaged and planted
candidates, a changed built-in cast, a missing prerequisite, a creation cycle
and a compile-order ambiguity. The dedicated-server fixture resolves through
the qualified lifecycle.

## Ordered implementation issues

Each issue describes its scope, ADR acceptance cases, positive/negative tests
and dependencies. The issue loop is sequential: the current PR must merge
before the next issue is claimed. #597 is complete; #606 through #613 are delivered.

| Step | Issue | Required outcome |
|---|---|---|
| 1 | [#606](https://github.com/pongbiphang/pbps/issues/606) | configure named profiles and lazy selection policy |
| 2 | [#607](https://github.com/pongbiphang/pbps/issues/607) | add peer-verified TLS connection primitives |
| 3 | [#608](https://github.com/pongbiphang/pbps/issues/608) | acquire and contain run-owned Docker environments |
| 4 | [#609](https://github.com/pongbiphang/pbps/issues/609) | qualify dedicated scratch-server lifecycle and exclusivity |
| 5 | [#610](https://github.com/pongbiphang/pbps/issues/610) | qualify analysis environment builds and deployment context |
| 6 | [#611](https://github.com/pongbiphang/pbps/issues/611) | qualify analysis environment builds and deployment context |
| 7 | [#612](https://github.com/pongbiphang/pbps/issues/612) | capture coherent binding inputs and complete prerequisite manifests |
| 8 | [#613](https://github.com/pongbiphang/pbps/issues/613) | reconstruct desired namespaces and extract covered creation bindings |
| 9 | [#614](https://github.com/pongbiphang/pbps/issues/614) | seal versioned evidence and deterministic cross-kind change ordering |
| 10 | [#615](https://github.com/pongbiphang/pbps/issues/615) | integrate lazy target planning and pre-publication verification |
| 11 | [#616](https://github.com/pongbiphang/pbps/issues/616) | verify sealed prerequisites and closing bindings during apply |
| 12 | [#617](https://github.com/pongbiphang/pbps/issues/617) | qualify private source logging transport and disposable storage |
| 13 | [#618](https://github.com/pongbiphang/pbps/issues/618) | integrate protected artifacts approval launch and history |
| 14 | [#619](https://github.com/pongbiphang/pbps/issues/619) | design creation binding coverage and catalog consistency |
| 15 | [#620](https://github.com/pongbiphang/pbps/issues/620) | implement qualified binding planning and transactional apply |
| 16 | [#621](https://github.com/pongbiphang/pbps/issues/621) | complete the cross-engine acceptance matrix and delivery documentation |

#594's legacy-reader boundary is no longer a prerequisite: #952 keyed the
fingerprints (DEC-952.1), so a resolver plan records no verifier to protect.
#617 still qualifies private reconstruction controls for the source itself. The SQL Server binding design precedes
its adapter, and final acceptance covers both engines independently. Every
agreed stage needs successful supported cases; permanent refusal is not delivery.

## Coordination and boundaries

- [#952](https://github.com/pongbiphang/pbps/issues/952) keys every
  external-input fingerprint to the environment (DEC-952.1), replacing #594's
  protected-storage design; sealing records the key identifier (#614).
- [#587](https://github.com/pongbiphang/pbps/issues/587) owns existing `--dev`
  containment. Code reuse does not certify or remove preview workflows.
- Keep #230 / PR #542, #173 / PR #487 and #232 / PR #546 protections until
  their tested replacements are delivered; this parent does not close them.
- Engine adapters supply facts. The differ and `Dialect` remain pure;
  deterministic evidence stays outside semantic `Schema` equality. The final
  typed plan remains the sole SQL-emission input, sealed before human approval.
- No planning DDL on the target, production-row cloning, arbitrary image
  synthesis, semantic SQL parser, runtime workload discovery, snapshot-derived
  deployable plan, resolver-backed staged apply or new service/plugin engine.
