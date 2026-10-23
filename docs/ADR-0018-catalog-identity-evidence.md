# ADR-0018: Catalog evidence narrows the pending-partition-drop baseline refusal

- Status: accepted (A′ + F)
- Date: 2026-10-23
- Related: SPEC §1.3 and §14.3; DEC-1852.1; DEC-2179.1; #2179

## Problem

A drop intent removes a table's uid from the working identity file before
the database drops it. When its parent becomes `partitions: external`, a
baseline could hand that partition to another tool undropped. DEC-1852.1
refuses that handover using the recorded parent's uid and the live bounds.
Names and bounds cannot find a held partition after an out-of-band rename,
rebind or detach/attach. Its conservative parent refusal also blocks other
partitions, and persists after a manual drop until the intent is reverted.

## Decision

**A′: record catalog evidence, and use it for this refusal alone.** Every
PostgreSQL apply and baseline snapshot records the cluster's
`pg_control_system().system_identifier`, the database's `pg_database.oid`,
and the `pg_class.oid` of every managed table. This includes ordinary tables,
partitions and their parents: the parent's oid lets the check reject a
replacement parent with the same name. Staged checkpoints and bootstrap use
the same catalog read-back as apply and baseline. State version 31 adds this optional
record; versions 6–30 remain readable and supply no evidence.

The recorded uid still decides which table a drop intent names. Only when
both the cluster and database match can the evidence narrow DEC-1852.1:

- If the recorded relation oid is present and is still a partition of the
  same recorded parent, under that parent's working name, refuse only that
  partition, naming its live name regardless of its current bound.
- If that oid is absent from the complete catalog read, the held partition
  was dropped. Allow the handover without reverting its drop intent.
- If it is present but detached or under another parent, retain the parent
  refusal. Missing table or parent evidence, or a cluster/database mismatch,
  also retains the existing parent-and-bound check unchanged.

A refusal still writes no baseline entry. Its diagnostic holds back only the
identified partition; after that held partition is dealt with, the parent's
other external partitions can be handed over unchanged.

**F: name both remedies, with commands.** Retain DEC-1852.1's explanation
that the drop intent removes the partition from the identity file before it
is dropped. Finish the pending drop with `pbps plan` then
`pbps apply --plan <plan>`, keeping the parent's partitions declared until
then; alternatively, for a manual drop without usable evidence, undo the
intent with `git revert <drop-intent-commit>` and baseline the missing table.

## Type boundary

`pbps-model::baseline_evidence::BaselineEvidence` is an opaque snapshot
sidecar, outside `Schema`, `Table`, `IdsFile` and saved plans. Its fields are
private, with no oid, iterator or identity-map getter. Serialization permits
ledger storage; it grants no matching API. A compile-fail example pins that
the table evidence is private.

The recording constructor accepts the engine's table oids; `for_managed`
returns an opaque subset without exposing any numbers. The non-serializable
`BaselineCatalog` accepts the engine's complete catalog read and exposes only
`held_partition`, whose closed result is `ParentRefusal`, `Present(name)` or
`Dropped`. All comparisons of catalog numbers occur inside that one check.
The CLI's baseline refusal consumes that result. Rename detection, diff and
plan have no accessor through which they can read the numbers. Identity
remains the human-supplied uid in git; this evidence cannot create a rename
decision or change a plan.

The recording side reuses the table oids already selected by the catalog
batch, adding cluster/database identity to that same statement with no extra
round trip. `Pulled` and `Scoped` carry only opaque evidence, restricted to
the managed tables before it is attached to the closing snapshot. No evidence
from before the deployment is carried forward as a successful apply result.
The baseline lookup separately reads every `pg_class` relation, including
unsupported ones, so omission from the semantic schema is never mistaken for
absence. That lookup reads identity and relations in one statement. SQL and
connected work remain in `pbps-pg`, routed by CLI `engine`; neither model nor
diff gains I/O or a driver dependency.

## Measurements

Measured in separate scratch databases on PostgreSQL **16.15** and **18.6**
on 2026-10-23, using the already-running test servers:

| Observation | PostgreSQL 16.15 | PostgreSQL 18.6 |
| --- | --- | --- |
| Deployer (`postgres`) can read `pg_control_system()` | yes | yes |
| Plain LOGIN role with no grants can read it | yes | yes |
| Cluster `system_identifier` | `7697669565946585126` | `7697669565347053611` |
| One timed control read (server/client timing, not a benchmark) | 0.418 ms | 0.316 ms |
| Rename preserves the relation oid | yes | yes |
| Detach/attach under another bound preserves it | yes | yes |
| Attach under another parent preserves it but changes parent oid | yes | yes |
| Detached relation remains present with no partition parent | yes | yes |
| Drop/recreate gets a new oid; old oid is absent | yes | yes |
| Dump/restore into another database on the same cluster changes database oid | yes | yes |
| `CREATE DATABASE ... TEMPLATE` copy changes database oid | yes | yes |
| Renaming the restored database retains its new oid and the fallback | yes | yes |

The live acceptance tests copy the ledger as well as the schema. Restored and
template databases retain the same cluster identifier but fail the database
comparison, and retain the parent refusal even when the held oid is absent.
The other live cases prove a renamed, rebound partition is named alone, and
a manual drop admits the baseline without changing the drop intent. Apply
and baseline both write version 31 evidence.

## Limits

- **Oid reuse after wraparound can only over-refuse.** Two live relations
  cannot share an oid. If the original held partition remains, another
  relation cannot take its oid. If it was dropped and its oid is reused, the
  check either falls back (different/no parent), or holds back the replacement
  partition under the same parent. It never releases the original undropped
  partition on the basis of reuse. Parent evidence can over-refuse too.
- A dump-restored database that was also renamed out of band falls back:
  database names cannot substitute for matching catalog evidence. F supplies
  the remedies when names and bounds no longer identify the held partition.
- **`pg_upgrade` is unmeasured and was not attempted.** The expected behavior
  is retained relation oids but a new `system_identifier`, hence the parent
  refusal. This is an expectation, not an upgrade compatibility measurement.
- PostgreSQL only: `partitions: external` is PostgreSQL only (DEC-1533.1).
- A failed or incomplete identity/catalog read is an error, never empty
  evidence or proof that a relation was dropped. No name, bound or catalog
  number supplies human rename intent (SPEC §14.3).
