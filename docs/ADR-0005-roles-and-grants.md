# ADR-0005: Roles and grants — and the generalized identity criterion

- Status: accepted and built (Phase 4; see "Implementation status")
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
# schema/roles/app_reader.yml
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
  the reason this ADR exists ahead of its Phase 4 implementation.

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
  Phase 5 touchstone material, alongside function overloading from ADR-0002.

## Implementation status

Built: the `role:` file and its `fmt` round trip, `Role` and its grants in the
model and in `Schema` equality, the `roles` section of the ids file with `r_`
uids, the three intent channels (`rename-role` / `drop-role`, `renamed_from:`,
the prompt), the differ, the `revoke` and `grant-widen` classes, the T-SQL, the
catalog read-back (`sys.database_principals`, `sys.database_permissions`), the
drift comparison under the managed set, `validate`'s target rule, `pull`, and
the docs section. Live tests cover the round trip, a hand-made `GRANT` seen as
drift, a `DENY` reported rather than folded, and a rename keeping its member.

Decisions taken during implementation that this document did not anticipate:

1. **`grant-widen` is labelled but never gated, mechanically.** `RiskClass`
   gained `is_gated()`; the plan's risk list carries the class so every
   review layer sees it, and `unapproved_risks`, the `--allow` advice and
   `explain`'s approval command all use the gated subset. Advising a flag the
   gate never asks for would teach reviewers to type it by rote.
2. **A grant follows its object through a rename.** The differ brings the base
   side's grant targets forward through the plan's table renames by uid before
   comparing, so renaming a table does not come out as a revoke on the old
   name plus a grant on the new one — which is also what `sp_rename` does. A
   revoke on an object the same plan drops is not emitted: the drop takes the
   permission with it.
3. **Inside a managed role, only grants on managed objects are compared.** A
   grant on somebody else's table is that table's business; comparing it
   would have the next plan revoke a permission the declarations were never
   allowed to name. Schema-level grants are always compared, since they are
   declarable. A role the ids file does not name is unmanaged, like a table.
4. **What the model cannot hold is reported by `pull`, never dropped — and
   is drift on a managed role.** A `DENY`, a column-level grant, a permission
   outside the closed set (`CONTROL`, `TAKE OWNERSHIP`), a grant `WITH GRANT
   OPTION`, a database-level grant (`CREATE TABLE`, `CONTROL` on the
   database) or one of any other class, and a grant on an object the model
   does not hold (a sequence, a synonym, a module that could not be read) are
   each left out of the role's
   set and reported naming the role and the target: `pull` prints them as
   warnings, `verify` carries them as unexpressible drift, and `plan --db`
   refuses to plan over them (DECISIONS 95, 97), since the sets that remain
   would otherwise compare equal on a role that has changed. The
   last one is left out of the role rather than written, because a grant
   target has to be a declared table or module and `validate` would refuse
   the project `pull` had just written.
5. **The built-in roles are refused by `validate`.** `public` and the ten
   `db_*` roles cannot be created, dropped or renamed; declaring one would plan
   a statement the engine refuses.
6. **A dropped role's members are removed first, by name, in the connected
   plan.** The engine refuses to drop a role that still has members, and
   membership is each environment's own — so `plan --db` reads
   `sys.database_role_members` and writes each member into the `DropRole`
   change, which emits one `ALTER ROLE ... DROP MEMBER` per member before the
   `DROP ROLE`. The artifact the gate approves therefore says exactly who
   loses the role in that environment; nothing is looked up at apply time,
   where nobody would have reviewed it. An offline plan has no environment
   to ask, leaves the list empty, and says so. Membership is still never
   declared or compared: this is the one consequence of a drop the user
   already asked for, with a reason, made visible rather than left to the
   engine to refuse.
7. **`doctor` asks for the role permissions only of a project that has a
   role** — declared, recorded in the ids file, or held by the
   environment's recorded state, which is where a role a `drop-role` is
   about to remove still shows (tombstones are permanent audit records and
   are not read for this): `CREATE ROLE` and `ALTER ANY ROLE` at the
   database, and `CONTROL` on every object and schema a role is granted on,
   because a `GRANT` is authorized on the securable and `ALTER` on the schema
   does not cover it. The securables are the declared grants **plus whatever
   those roles hold** in the recorded state, and in the catalog where the
   login can see it: a grant gone from the declarations is a `REVOKE` the
   plan will write, and a role being dropped is a `REVOKE` of each of its
   grants — neither is visible from the declarations alone, and a readiness
   check that read only those said "ready" to an apply that then failed. The
   recorded state comes first because the catalog hides a securable from an
   account with no permission on it, which is the account being checked.
8. **A role's file is `roles/<name>.yml`.** The `.role.yml` suffix of the
   first cut collided with a table called `<x>.role` (DECISIONS 76).
9. **A target this plan drops and creates again is granted from nothing.**
   `DROP` takes the permissions with it; a table replaced under the same
   name, or a module changing kind, comes back bare, and every declared
   permission on it is a `GRANT` after the `CREATE` (DECISIONS 72).
10. **A connected plan refuses to drop a role that owns anything**, of every
    class the catalog can assign an owner to — schemas, objects, types, XML
    schema collections, other roles, assemblies, certificates and keys, the
    full-text and Service Broker objects, credentials, event notifications,
    external languages and libraries — and names the `ALTER AUTHORIZATION`
    to run by hand. The engine will not drop an owning role, and a staged
    apply would have committed every `DROP MEMBER` before finding out; the
    list is read off the catalog's owner columns, not recalled (DECISIONS
    83, 88).
11. **A permission is checked against the kind of its target.** `execute` on
    a table, `select` on a procedure, anything on a trigger: the engine
    refuses each `GRANT`, and in a staged apply it would do so after the
    other changes had committed. `validate` applies the engine's own table,
    measured pair by pair, with a function's kind read off its `RETURNS`
    clause (DECISIONS 89).
12. **A role rename is gated.** `RenameRole` carries the `rename` risk like a
    table or column rename: the members stay, the old name does not, and
    `IS_ROLEMEMBER('old')` in a module or an application breaks on the spot
    (DECISIONS 91).
13. **`apply` asks about a dropped role again before statement one.** The
    members listed at plan time can be stale by apply time, and the checksum
    cannot see it; the preflight compares the live membership with the list
    and refuses on any difference, and asks about ownership again the same
    way (DECISIONS 92). A staged apply that found out at `DROP ROLE` would
    already have committed every `DROP MEMBER` the reviewer saw. A staged
    resume asks once more, for the members whose `DROP MEMBER` has not run,
    counted off the emitter's statements (102).
14. **A staged checkpoint follows a role rename, and adopts a role the plan
    creates.** The `ALTER ROLE ... WITH NAME` statement records the rename
    it performs, as a table rename's statements do, so the checkpoint after
    it scopes the role under its new name (DECISIONS 93); `CREATE ROLE`
    records what it creates, so the checkpoint after it scopes the new role
    in, and a grant it gains while the deployment is paused is seen by the
    resume rather than recorded as clean (100).
15. **A managed role's permission the model cannot hold is drift, not a
    warning.** Folded into the set, a grant `WITH GRANT OPTION` compared
    equal to the recorded grant; warned about and left out, a column-level
    grant or a `DENY` left the remaining sets equal — and `verify` called a
    role that had changed clean either way. Each is left out and carried
    beside the comparison: `verify` names it (with the `REVOKE GRANT OPTION
    FOR` to run by hand where that is the remedy), and `plan --db` refuses
    rather than plan over a role it cannot describe (DECISIONS 95, 97).
16. **`bootstrap` refuses a declared role the identity file does not
    know.** A role-only project that never ran `pbps plan` bootstrapped
    nothing and recorded the empty state as the whole one; every declared
    role, like every table, needs its uid first (DECISIONS 109).
17. **A declared role's name has to be free of every principal, as the
    database compares names.** Users, roles and application roles share one
    namespace; `plan --db` and `bootstrap` ask the engine, under the
    database collation, which principal holds a name a `CREATE ROLE` or a
    rename needs, and refuse it with the holder's kind and spelling before
    the statement could fail after the changes ordered before it. `apply`
    asks again before statement one and on a staged resume, for the names
    its remaining statements still need, since a principal created in
    between is invisible to the checksum (DECISIONS 118, 119). The names
    the plan needs free are compared with one another the same way first:
    `Reader` beside `reader` holds nothing in the catalog and fails at the
    second `CREATE ROLE` (DECISIONS 123).
18. **Two spellings of one grant target in a role file are refused.**
    `SCHEMA::dbo` and `schema::dbo` parse to one target, and the map kept
    whichever came last, with the other's permissions gone; the loader
    refuses the second by both spellings, as it refuses a column named twice
    (DECISIONS 126).
19. **`status` reports a permission the declarations cannot hold as drift,
    with the permission named**, under the same managed-role filter `verify`
    applies: the permission is carried beside the schema the checksum is
    computed from, so the checksum alone read it as clean (DECISIONS 125).

## Placement

Phase 4 for implementation. The ids-file `roles` section and the `r_` uid
prefix are pinned now, because identity formats are the most expensive thing
in this project to change late (SPEC §12, Phase 0's lesson).
