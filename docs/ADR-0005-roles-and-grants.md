# ADR-0005: Roles and grants — and the generalized identity criterion

- Status: decided (design; implementation targeted at Phase 5, ids-file
  extension pinned now)
- Date: 2026-08-31
- Related: docs/SPEC.md §5, §8.1, §8.2, §13.4, §13.7, §12;
  [ADR-0002](ADR-0002-module-model.md);
  [ADR-0004](ADR-0004-reference-data.md)

## The generalized identity criterion

ADR-0002 drew the line at "modules carry no data". Designing roles and
reference data exposed the sharper form:

**Does drop + add destroy state that lives only in the environment and cannot
be restored from the declarations?**

| Object | What drop + add destroys | Identity machinery |
|---|---|---|
| Column | Data | uid + intent (v1) |
| Module | Nothing — the definition is in git | None (ADR-0002) |
| Reference row | Nothing — content fully declared; the dangerous case is blocked loudly by FKs (ADR-0004) | None; the key is the identity |
| Role | Per-environment membership, which pbps does not manage and cannot restore | uid + intent (this ADR) |

Every answer ADR-0002 already gave is unchanged; the refinement produces an
answer where the old phrasing had none.

## Background

Open question 7 noted that declarative permission management has value but a
different risk model and wide dialect variance. Atlas shipped it as "Database
Security as Code" in v1.1 (2026), Pro-gated.

Naive GRANT-as-code dies on one fact: **principals are environment-specific.**
Dev and prod have different users, and logins are server-level objects that no
portable declaration can describe.

## The portable unit is the role

| | Managed by pbps | Stays environment-local |
|---|---|---|
| Database roles | Yes — existence and grants | |
| Grants to roles | Yes | |
| Logins, users | | Yes — server-level, per-environment |
| Role membership | | Yes — which users hold a role is each environment's reality |

## Format

```yaml
# schema/app_reader.role.yml
role: app_reader
grants:
  dbo.customer:     [select]
  dbo.order_status: [select]
```

- One role per file; identity from the field, file name meaningless — the
  convention of tables and modules.
- Grants are structured data (a `BTreeMap` of object → `BTreeSet` of
  permissions), never SQL strings. Constraints 3, 4 and 5 hold unchanged; the
  emitter renders GRANT / REVOKE.
- `validate` checks grant targets exist among the declarations — the
  FK-target rule; schema-level grants (`schema::dbo`) pass through.

## Roles enter the ids file

By the criterion above, roles sit on the column side of the line: dropping and
recreating one destroys per-environment membership. Therefore:

- Role renames need intent. `sp_rename` has an equivalent here —
  `ALTER ROLE ... WITH NAME` — and the three intent channels extend to
  `pbps rename-role`; the emitter uses ALTER, never drop + add.
- Role drops take `--reason` and leave a tombstone, exactly like column
  drops: "why was this access removed" is precisely an auditor's question.
- The ids file gains a `roles` section with `r_`-prefixed uids — a compatible
  evolution under the existing `version` field. Pinning this format now is
  the reason this ADR exists ahead of its Phase 5 implementation.

## Risk classes

Open question 7's "different risk model" made concrete — a new family:

- **`revoke`** — availability risk: a running application loses access
  mid-flight. Gated behind `--allow revoke`. A role drop is classified here
  too (its whole effect is revocation), on top of its tombstone reason.
- **`grant-widen`** — security risk: access expands. Loudly labelled in every
  plan and plan.sql so both review layers (§7.3) see it, but not gated:
  granting is the normal case, the MR reviews the YAML diff, and the
  deployment gate approves the pinned plan. Gating it would add friction, not
  safety.

## Drift

The managed set only, as everywhere (§8.2): a declared role's grants must
match the database exactly (introspected from `sys.database_permissions`);
undeclared roles and users are ignored by default, and the `unmanaged:` knob
extends coverage. The classic permissions drift — an incident-time GRANT
nobody ever revoked — becomes visible instead of folklore.

## Self-hosting

§8.1's trust model — only the deployment account writes `__pbps_state` /
`__pbps_lock` — stops being a documented exhortation and becomes a shippable
role declaration, drift-checked like everything else. Open question 4's
access-control direction gets its mechanism.

## Ruled out and limits

- **`DENY` is excluded.** Its precedence semantics are heavy, it is rare in
  well-run shops, and modelling it wrong is worse than not modelling it.
- **Column-level grants deferred**; object-level first.
- **Server-level objects** (logins, server roles) are out of scope.
- **PostgreSQL variance** (default privileges, schema and sequence grants) is
  Phase 4 touchstone material, alongside function overloading from ADR-0002.

## Placement

Phase 5 for implementation. The ids-file `roles` section and the `r_` uid
prefix are pinned now, because identity formats are the most expensive thing
in this project to change late (SPEC §12, Phase 0's lesson).
