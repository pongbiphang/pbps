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
  private-input handling retains the #594 dependency.

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

## Ordered implementation issues

Each issue describes its scope, ADR acceptance cases, positive/negative tests
and dependencies. The issue loop is sequential: the current PR must merge
before the next issue is claimed. #597 is complete; #606 starts this sequence.

| Step | Issue | Required outcome |
|---|---|---|
| 1 | [#606](https://github.com/pongbiphang/pbps/issues/606) | configure named profiles and lazy selection policy |
| 2 | [#607](https://github.com/pongbiphang/pbps/issues/607) | authenticate evidence channels and prove instance separation |
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

#594 must separately complete its accepted design, implementation and legacy-
reader compatibility tests before #618 is claimed. It is an additional hard
prerequisite, not a waived or implicitly chosen ledger/access transition.
#617 qualifies private reconstruction controls; it does not enable confidential
publication/apply/recording by itself. The SQL Server binding design precedes
its adapter, and final acceptance covers both engines independently. Every
agreed stage needs successful supported cases; permanent refusal is not delivery.

## Coordination and boundaries

- [#594](https://github.com/pongbiphang/pbps/issues/594) blocks confidential
  paths, not ordinary discovery or all managed-only work. Its separate design
  acceptance is a prerequisite; this plan chooses no physical ledger layout.
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
