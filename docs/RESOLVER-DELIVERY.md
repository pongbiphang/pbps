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

## Remaining bounded deliveries and dependencies

| Delivery | Depends on | Required outcome and ADR acceptance cases |
|---|---|---|
| Selection and acquisition policy | Discovery report | CLI `--resolve-with` over environment/project configuration; trusted image/server sources, local/internal-registry and pull/no-pull policy; offline/check/explain exclusions and lazy acquisition. Finish the policy portions of 6. |
| Qualified runtime lifecycle, both engines | Selection/policy | Prove actual instance separation, authenticated transport, runtime containment and bounded run-owned cleanup before any declaration transfer or scratch DDL. Qualify dedicated-server and container profiles independently. Cases 11, 13, 19, 20; lifecycle portions of 6, 12, 14, 21. |
| Analysis-specific compatibility, both engines | Discovery and qualified lifecycle | Real backend/build/native-library identity or measured versioned build mappings; full relevant environment and deployment authorization, per-statement settings and stability. Compatible cases succeed; unknown/mismatched cases refuse. Cases 5, 14, 16, 21, 23. |
| PostgreSQL capture and binding surfaces | Qualified PostgreSQL runtime and compatibility | Observe current target bindings coherently; reconstruct complete desired namespace/candidate prerequisites without stubs; compare logical identities. Deliver views, SQL-standard bodies and each supported header/expression surface with its own live coverage. Cases 1–4, 9, 15, 16. |
| Sealed PostgreSQL planning and transactional apply | Binding surfaces and complete prerequisite coverage | Typed cross-kind ordering, versioned checksum-covered evidence, pre-publication recheck, offline explanation, locked pre-DDL and closing binding/prerequisite checks; modeled postconditions accept approved changes. Cases 7–9, 15, 22, 24. Apply never starts a resolver. |
| Confidential artifacts and inputs | **Separately accepted and implemented #594** plus qualified private-source lifecycle | Qualified source logging/storage/transport and protected artifact, launch, ledger/history and legacy-reader paths before enabling any confidential publication/apply/recording. Cases 10, 12, 17–19 plus #594 compatibility tests. Do not choose a ledger/access transition implicitly. |
| SQL Server binding design and adapter | Shared qualified environment support; separate engine-specific design | Establish SQL Server coverage, catalog consistency, reconstruction/order, authorization and transactional verification before enabling binding evidence. Run all applicable cases 1–24 independently; confidential paths also depend on #594. PostgreSQL success does not qualify SQL Server. |

Stages may need multiple surface-specific PRs. Every enabled capability needs
positive cases, failure/unknown cases and a counterfactual that removes the
protection and fails. Permanent refusal of an unfinished stage is not delivery.

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
