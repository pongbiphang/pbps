# The measurements behind the Phase 5 design ADRs

Every claim marked **measured** in ADR-0009 through ADR-0014 was produced here.
This directory exists so a reader can re-run them rather than believe them, and
so a future engine version that answers differently shows up as a diff instead
of as a document that has quietly gone stale.

```
./run.sh              # both engines
./run.sh postgres
./run.sh sqlserver
```

Each run starts a digest-pinned container, runs the script, removes the
container, and overwrites `observed-postgres.txt` / `observed-sqlserver.txt`.
**A clean `git diff` on those two files is the pass condition.** A dirty one is
not a failure — it is the finding, and whichever ADR quoted that line needs
re-reading.

`PG_PORT`, `MSSQL_PORT`, `PG_IMAGE`, `MSSQL_IMAGE`, `RUNTIME` and `SA_PASSWORD`
override the defaults; the ports differ from `scripts/live-tests.sh`'s so the
two can run side by side.

## Why the output looks like this and not like a test suite

Each line is `<id> | <claim> | <observed>` — the observation, not a verdict. A
`PASS`/`FAIL` column would need an expected value baked into the script, and the
whole point of these measurements was that the author's expectation was wrong
twice (ADR-0009 §3 and ADR-0012 §3 both reversed a conclusion after the engine
answered). Recording what the engine said, and diffing it, keeps the expectation
outside the thing that measures.

Errors are captured as `refused: <the engine's own message>` rather than
abbreviated, because in several cases the message *is* the finding — `function
name "m.f" is not unique` and `cannot alter type of a column used by a view or
rule` are each quoted in an ADR.

## What is here

| File | |
|---|---|
| `postgres.sql` | 236 measurements: modules and overloading (A), privileges (B), type-change cost (T), reference data (R) |
| `sqlserver.sql` | 3 contrasts (M), and only 3 — see below |
| `observed-*.txt` | what those produced on the pinned engines when the ADRs were written |

**Why only three SQL Server measurements.** These are the claims where "the two
engines differ" *is* the finding, so asserting the difference against an
unmeasured other half would be exactly the mistake these documents were written
to avoid. Everything else the ADRs say about SQL Server comes from this
repository's existing code, tests and ADRs, and is cited as such. M3 is the one
that mattered most: it decided whether ADR-0013 §2 describes a PostgreSQL hazard
or a bug in shipped code. It is a hazard.

## Which ADR reads which lines

| Lines | ADR |
|---|---|
| `A1`–`A75` | [ADR-0009](../../docs/ADR-0009-postgres-modules.md) — deparsing, overloading, what `CREATE OR REPLACE` cannot do, dependency refusals |
| `B1`–`B12`, `M1`, `M2` | [ADR-0010](../../docs/ADR-0010-postgres-privileges.md) — `USAGE`, schema grants, default privileges, cluster-wide roles, the ACL |
| `T-01`–`T-21`, `R108` | [ADR-0012](../../docs/ADR-0012-postgres-type-catalogue.md) — what rewrites a table, what a session decides, the catalogue's spellings |
| `R1`–`R107`, `R109`–`R128`, `M3` | [ADR-0013](../../docs/ADR-0013-postgres-reference-data.md) — defaults, identity keys, `NOT VALID`, collation, session-dependent rendering |
| — | [ADR-0014](../../docs/ADR-0014-driver-seam-tested.md) measures the driver seam instead, in `../pg-driver` |

## Two things this is not

It is **not a live suite**. It asserts nothing, gates nothing, and runs in no
pipeline. When `pbps-postgres` exists, its live suite is the descendant of this
file and should replace it — the SPEC §11.5 invariants first, the way
`crates/pbps-mssql/tests/live.rs` does, with these behaviours as the cases that
suite already knows are worth covering.

It is **not product code**. Like `spikes/yaml-span` and `spikes/pg-driver`, it
goes when what it was built to decide has been decided.

## The 2026-09-05 additions

`A21`, `A22`, `B11`, `B12` and `R9` were added after a review of PR #12 found
five defects in the ADRs. Four of them needed an engine to settle, and all four
confirmed the reviewer: a type modifier does not distinguish two routines, a
function nobody granted is executable by `PUBLIC`, and restarting a sequence at
the highest key *this plan* wrote can move it backwards past a row the plan
never touched. They live here so the corrections are as reproducible as the
claims they replaced.

## One caveat found by running it

`postgres.sql` sets `search_path` to its own schema, and a first draft of `A17`
read wrong because of it: with the schema on the search path, `pg_get_viewdef`
omits the qualification, so a check for `m.client` failed against a definition
that said `client`. The script now resets the path around that assertion.

That is [ADR-0013](../../docs/ADR-0013-postgres-reference-data.md) §3's
`search_path` hazard — an unqualified name resolving somewhere other than
intended — arriving in the measurement script written to demonstrate it. It is
recorded here because the same shape will reach the introspection code, where
nothing prints the answer for a human to notice.
