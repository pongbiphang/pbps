# ADR-0003: Execution strategy and the staged apply

- Status: decided (design; format in Phase 2, staged apply in Phase 3.5)
- Date: 2026-08-30
- Related: docs/SPEC.md §7.3, §7.5, §12, §13.2

## Background

Open question 2 asked how ALTER on large tables should be executed: ONLINE
options, batching and off-peak scheduling are runtime decisions that a
structural diff cannot derive.

Skeema's answer is an external wrapper (`alter-wrapper` invoking gh-ost or
pt-online-schema-change). That path is ruled out here: an external tool
rewriting the SQL destroys both the checksum guarantee (§7.3) and "SQL appears
exactly once, in the emitter". The MSSQL ecosystem has no mainstream gh-ost
equivalent anyway, so the exclusion costs little. Genuinely exotic cases keep
the escape hatch that already exists: a DBA runs the SQL, then `pbps baseline`.

## Decision 1: `strategy:` describes how to get there, never where to go

A table-level annotation supplies the execution hint:

```yaml
# schema/dbo.order_line.yml
table: dbo.order_line
strategy:
  online: true      # emit WITH (ONLINE = ON) where the dialect supports it
```

**It is excluded from the model.** Strategy changes the emitted SQL, not the
desired state: it is invisible in the database, so it can never participate in
the drift comparison — and if it lived in `Schema`, two schemas identical in
the database would compare unequal, breaking inviolable constraint 1.

The precedent is `renamed_from`: `pbps-load` returns it beside the model, not
inside it. Load's contract becomes `(Schema, Intents, Strategies)`, the differ
attaches the relevant strategy to each `PlannedChange`, and the emitter
consumes it. One difference from `renamed_from`: strategy is a **persistent**
annotation ("this table is large — always operate online"), so `fmt` preserves
it rather than stripping it.

The key set starts at exactly `online` and grows conservatively; `validate`
rejects unknown keys so a typo cannot silently become a no-op.

## Decision 2: staged apply, instead of weakening "one plan, one transaction"

§7.5's rule stands: a plan containing a statement the emitter marks
non-transactional **fails at plan time**. What changes is that the error now
names the way out instead of a dead end.

A plan carries a mode: `transactional` (the default; semantics unchanged) or
`staged`. A staged plan:

- contains exactly **one logical change** — the operation that cannot run in a
  transaction, isolated in its own deployment;
- is applied with `pbps apply --staged`, which records each completed
  statement in the `__pbps_state` ledger, so a mid-way failure is visible
  rather than mysterious;
- resumes with `--resume`, which first verifies the live state against the
  recorded checkpoint — the drift discipline, applied to a half-finished plan;
- is still checksum-pinned and still reviewed at the deployment gate. The
  approval granularity does not move.

Rollout is deliberately split: strategy hints whose resulting statements stay
transactional (most ONLINE index operations) ship first and keep the
one-transaction model untouched; the staged machinery follows.

## Decision 3: risk is edition-dependent, and the dialect must know

The same DDL carries different risk on different editions of the same engine.
Two examples that will recur:

- Adding a NOT NULL column with a DEFAULT is a metadata-only operation on
  Enterprise edition but a size-of-data rewrite on Standard.
- ONLINE index operations exist only on Enterprise (and Developer, which is
  why a Docker dev database cannot prove them viable in production — see
  SPEC §9.3).

`plan --db` therefore reads `SERVERPROPERTY('Edition')` and classifies with
the real edition; an offline plan assumes the conservative edition and says so
in the preview.

This validates where constraint 4 drew the line: risk needs dialect knowledge,
and now also connection-time facts, and the differ holding a `Dialect` is
where both meet.

## Rollout

| Phase | What lands |
|---|---|
| Phase 2 | The `strategy:` block enters the YAML format and `pbps-load`'s return type — the cheapest moment, before the signature has many callers. `fmt` preserves it; `validate` rejects unknown keys |
| Phase 3 | The emitter honours `online: true` where the statement stays transactional; edition-aware classification at `plan --db` |
| Phase 3.5 | Staged apply (`--staged` / `--resume`, per-statement ledger records) |

## Ruled out

- **External OSC wrappers** (gh-ost style) — see Background.
- **Hand-editing plan.sql** — already ruled out in §7.3.
- **Inferring strategy from table size at plan time.** It needs a connection
  the offline path deliberately does not have, the answer changes as the data
  grows, and it turns a stable, MR-reviewed annotation into invisible
  behaviour. The annotation is intent; intent is reviewed.
