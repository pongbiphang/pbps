# ADR-0003: Execution strategy and the staged apply

- Status: accepted; format in Phase 2, emitter and edition-awareness in Phase 3, staged apply in Phase 3.5
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

## What was learned building it

- **The hint travels on the change, not beside the plan.** The plan file is
  what the deployment gate approves, so an approver has to be able to see that
  this index will be rebuilt online. A strategy resolved at emit time against a
  YAML file the deployment host may not even have is a hint nobody reviewed.
- **`WITH (ONLINE = ON)` is not accepted everywhere the intuition says.**
  Building a unique constraint online works, because it is index-backed. Every
  *drop* refuses it unless the index behind it is clustered — and this emitter
  writes no `CLUSTERED`, so `DROP INDEX`, `DROP CONSTRAINT` for a unique key,
  and a primary-key drop all take no clause at all. A foreign key and a check
  are metadata only, where it is a syntax error rather than a no-op.
- **Whether a statement carries the clause is asked of the emitter, and by
  comparing two emissions rather than searching the text.** A second list of
  change kinds would drift from the emitter; searching the SQL would read the
  user's own expressions, and a column default of `'ONLINE = ON'` would have
  refused a valid plan on Standard edition.
- **A staged plan's mode belongs in the file, and the flag has to agree with
  it.** Running whichever the operator typed would be the tool choosing the
  loser of a disagreement about what was approved.
- **An unfinished staged apply makes the environment mid-deployment.** The
  recorded baseline is then a checkpoint, not a state anybody signed off, so
  `plan --db` and a fresh `apply` both refuse until it is finished or
  baselined, and `status` reports it as its own state rather than as `ok`.
  Drift does not displace that: an environment that is both staged and moved
  stays `staged`, because the way out is `--resume` or a new baseline and the
  drift workflow cannot finish a half-applied plan.
- **A checkpoint's identity file is the mapping at that checkpoint.** One
  `RenameTable` can need two statements — `sp_rename` cannot move a table
  between schemas and `ALTER SCHEMA TRANSFER` cannot rename it — and between
  them the table sits at `[new schema].[old name]`, which appears in neither
  the baseline nor the plan. Scoping the checkpoint by the plan's mapping left
  the table out of the record entirely, so a change made to it while the
  deployment was paused was invisible to `--resume` and was carried into the
  closing entry as the applied state.

  The name cannot be derived downstream without keeping a second copy of the
  emitter's statement order, so each `Statement` declares the renames it
  performs and the staged loop replays them. That the declaration matches what
  the engine does is a live test: a unit test would be comparing the emitter
  with itself.

  What this does **not** close is the window itself. `__pbps_lock` protects
  against mistakes, not tampering, so a second connection can change a table
  mid-apply whatever the checkpoint records. It closes the part that was
  pbps's own: the change is now seen and refused instead of blessed.

## Ruled out

- **External OSC wrappers** (gh-ost style) — see Background.
- **Hand-editing plan.sql** — already ruled out in §7.3.
- **Inferring strategy from table size at plan time.** It needs a connection
  the offline path deliberately does not have, the answer changes as the data
  grows, and it turns a stable, MR-reviewed annotation into invisible
  behaviour. The annotation is intent; intent is reviewed.
