# Reproducing the ordering closure evidence

Run from a clean worktree. These tools measure #1776/#1959; they do not gate
product compilation, add CI work or enable the release verifier (DEC-1959.6).
Keep generated logs under `target/s7-closure`, outside source control.

The tools' safety tests need no live engine:

```sh
python3 scripts/effect_mutations_test.py
python3 scripts/order_closure_measures_test.py
```

They pin source restoration on interruption, invalid/empty evidence, compiler
failures hidden by another test failure, and missing/ambiguous Git anchors.

## Maintenance inventory (no live engines)

```sh
python3 scripts/order-closure-measures.py --before 35c658e5 --after HEAD --output target/s7-closure/update-sites.json
```

The manifest is a bounded, manually audited list of ordering decisions to
update or review for a new kind/rule. Each row states when it applies. It
verifies one source anchor per file:symbol against both immutable Git commits;
an unreadable/missing/ambiguous anchor refuses the report. The conditional
rows are not a universal edit count. Common model/generation/emitter/audit
work is separate. New risk/validation/identity dispatch remains ordinary
feature work; it is not counted as a benefit of changing ordering ownership.
Existing base sort, compatibility and search review sites remain visible.

## Mutation evidence (live PostgreSQL and SQL Server)

Set `PBPS_TEST_PG_DB` and `PBPS_TEST_DB` to approved disposable test servers.
For SQL Server the tests pass the ADO string to `Config::from_ado_string`;
use `Server=localhost,PORT;User Id=sa;Password=...;TrustServerCertificate=true`.
The tools create/drop test databases, not containers. Do not run concurrent
Cargo builds/tests against this worktree while its effects source is mutated.

```sh
python3 scripts/effect-mutations.py --s7
python3 scripts/effect-mutations.py
python3 scripts/effect-mutations.py --audit
```

`--s7` repeats the original M01–M20 sample and command matrix from #1965,
including the previously caught effects. It uses the capped commutation
suites, clears inherited `PBPS_COMMUTE_ALL`, and records shadow findings
separately from independent failures. Its JSON report includes commands,
per-command logs, failed tests and an exact source backup/restoration digest.
After each mutation the exact source is restored before every named failure
is re-run individually with `--exact`, retaining ignored/live harness flags.
A kill requires all those tests to execute and pass; an empty failed-test list,
a persistent failure or an empty/invalid confirmation makes the mutation
invalid. Each mutation's `confirmation` records the command, captured log,
return code, status and duration of every rerun directly in `report.json`.
Choose a fresh `--output DIR` for a repeat; it refuses overwriting evidence.
The unit-only default and `--audit` retain #2091's former-miss sample. The
latter swaps all effect-independent pairs, and distinguishes effects the
current valid plans cannot show from missing unit coverage. A surviving
mutation is evidence, not authorization to change product behavior here.

The unmutated commands must pass first. A missing endpoint, zero selected
tests, missing anchor or compile failure is invalid evidence, not a catch.
Source restoration is in `finally`; after forcibly killing the interpreter,
compare with and restore its recorded `original-effects.rs` before continuing.

## Benchmark (no live engines)

Fetch `722c80e99c07a5f0a9c8321176f8e96fd6791b50`, #1965's corrected baseline,
into a separate detached worktree. In that and the current worktree:

```sh
cargo test --release -p pbps-diff --lib --no-run
PBPS_ORDER_BENCHMARK=1 cargo test --release -p pbps-diff --lib -- --nocapture order::benchmark
```

Collect three serial warmed runs per revision with the same compiler/flags,
host and recorded load. Report medians/ranges, operation/edge counts and
violations for independent, dense, chain and cycle. The pipeline column times
the partial differ without its test observer, not the completed deployment
route. The old resolver and current settle columns have different scopes;
do not divide them into a claimed scheduler speedup. Cycle refusal changed
since the baseline; label that separately. Do not reuse the superseded
benchmark that included the test observer in pipeline time.

## Acceptance

Keep existing cfg(test) old/new operation, SQL, metadata and checksum oracles,
seeded cases, UID harnesses and compiler-negative doctests intact. For the
capped live sample, with audit variables unset:

```sh
cargo test -p pbps-pg -p pbps-mssql --test live -- --ignored --nocapture pairs_the_effects_call_independent_commute_on_the_engine
cargo test -p pbps-mssql --test live -- --ignored --nocapture a_row_written_before_and_after_the_column_it_fills_fails_the_row_check
```

Repeat PostgreSQL with the approved older endpoint, recording applicability:
the checked-in sample targets PostgreSQL 18 and includes
`a_generation_expression_beside_storage_parameters` (`SET EXPRESSION`, 17+).
An older-endpoint run must retain/report that corpus capability failure. A
supplementary run with a temporary case-exclusion copy records the excluded
scenario and the engine's syntax refusal, keeps every assertion, and restores
the exact source afterwards; it is not an unqualified full-suite pass.
The sample checks catalog and column order, empty next plan, grants/ownership,
seeded rows, deterministic bytes and regenerated-UID convergence. Keep its cap
and CI placement unchanged.
Put measured values, immutable SHAs, remaining gaps and log locations in the
maintainer's closure report/comment; keep architectural guidance rule-based.
