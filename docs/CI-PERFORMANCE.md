# Live-test performance

CPU-heavy PostgreSQL capture fixtures use Cargo's `live-test` profile. It inherits `test` and enables basic optimization (`opt-level = 1`)
while explicitly retaining debug assertions and overflow checks. Ordinary
`cargo test`, the quick CI suite, and the release profile are unchanged.

## Why this profile exists

In [CI run 36335570130](https://github.com/pongbiphang/pbps/actions/runs/36335570130),
`live-pg` took 40m47s, of which 32m33s was its private library regression step.
`resolver (mssql)` took 31m04s, of which 25m32s was dedicated-server
qualification. PostgreSQL's ordinary 301-case integration suite executed in
52.34s; its 52 private regressions executed in 1937.74s. Counts alone do not
explain the cost.

The private regressions repeatedly capture target and scratch catalogs on both
supported fixture versions. Each capture separately reads raw catalog data,
qualified rendering and fresh witnesses. SQL Server scope qualification also
hashed executable content, including large mapped engine packages, until
#1636 removed the measured profiles that compared it.

A representative PostgreSQL binding case generated 1,017 cursor FETCH
operations across the two engine versions. A local SQL-duration observation
summed to about 2.15s; a separate direct test-process observation took 10.09s
wall with 7.42s user and 0.23s system CPU. This identifies substantial
client-side CPU work, not a single slow SQL statement. It is not a claim
about release-binary performance or an exact function-level CPU profile.

## Paired measurements

The profile comparison used Rust 1.98.0 on a local Intel Core i9-14900F host,
two Cargo build jobs, debug information disabled, identical pinned PostgreSQL
16/18 fixtures, and debug assertions and overflow checks enabled in both
variants. Build targets started empty; the registry download cache was warm.
The host is shared, so these are observations rather than fixed timing gates.

| Workload | Unoptimized | Basic optimization |
| --- | ---: | ---: |
| Cold build of `pbps-pg --lib` tests | 70.89s | 141.30s |
| Binding regression, median of three alternating-order trials | 8.157s | 3.405s |
| Capture coherence regression, one paired trial | 69.881s | 28.475s |
| 637 MiB SHA-256 kernel, same input and digest | 8.501s | 0.291s |

The hash measurement excludes I/O, process/namespace inspection, SQL and
cleanup; its speedup is not the whole resolver job's speedup. The build
comparison shows the tradeoff explicitly: a longer initial compile for less
repeated test CPU work. Cargo's artifact records confirmed `debug_assertions`
and `overflow_checks` were true for both tested binaries.

## Run and compare

`scripts/live-tests-pg.sh` selects `live-test` for the private PostgreSQL library
suite; the matching CI step uses the same command. Other PostgreSQL commands
keep their existing profile. Run a particular capture case with the usual
fixture connection variables, for example:

```bash
cargo test --profile live-test -p pbps-pg --lib -- \
  --ignored --exact \
  resolver::binding_tests::a_later_object_of_another_kind_is_no_candidate \
  --test-threads=1
```

To compare execution with optimization removed while preserving the same
profile's assertions, set `CARGO_PROFILE_LIVE_TEST_OPT_LEVEL=0` for that command.
Use separate `CARGO_TARGET_DIR` directories for the two variants, record the
initial `--no-run` build separately, then run the warmed binaries in alternating
order. Do not mix compilation into a reported test-execution speedup. Keep the
same fixtures, engine versions, test selection and build-job limit.

The serial schedules, all positive and negative scenarios, both engine
versions, catalog rereads, content hashes and cleanup assertions remain in
place. A faster build mode does not authorize relaxing those checks. Further
parallelization would require independent fixtures for namespace mutation,
runtime failures, session census and server shutdown.

## The run's critical path

Before #1370 a `ci.yml` run took about 45 minutes, measured on merge-group
runs 36841115280, 36830024040 and 36817575465. Every job first waited about
seven minutes for `quick`. Then `live-pg` ran for 36–40 minutes while every
other job finished within 17. Two serial suites made up most of `live-pg`:
`flow_pg` with 150 cases in about 1,200s, and the private `pbps-pg` regressions
with 55 cases in about 770s. Compilation took one to three minutes per job.

The run is now shaped by DEC-1370.1:

| Job | Variants | Each runs |
| --- | --- | --- |
| `lint` | 1 | Python self-tests, fmt, clippy. Every other job waits for it |
| `live-pg` | `dialect`, `capture`, `flow-programs`, `flow-rest` | its share of the PostgreSQL suites, against its own PG18 + PG16 |

The splits are exact complements:

- `flow_pg` divides on `trigger routine function grant` against the
  `--skip` of each, into 72 and 78 cases (about 594s and 604s).
- The private library suite divides on `resolver::binding_tests`, into 34 and
  21 cases (about 335s and 435s). The binding half rides with the short
  dialect suites.

`scripts/ignored_test_inventory.py` refuses a split that leaves any ignored
case unselected. To rebalance, move a word between the filter and its
`--skip`, and keep the two lists equal. Then check the listed counts:

```bash
cargo test -p pbps-cli --test flow_pg -- --ignored --list --format terse \
  trigger routine function grant | grep -c ': test$'
```

## The commutation sample

The commutation suite (#1965) runs at most 30 pairs per engine. Each pair
takes three databases: one for the plan's own order, one for the swapped
order, and one for the plan made with new uids. Measured on the local host above, against one pinned
server each:

| Engine | Pairs | Wall time | Job and variant |
| --- | ---: | ---: | --- |
| PostgreSQL 18 | 30 | about 19s | `live-pg (capture)` |
| SQL Server | 30 | about 60s | `live (dialect)` |

Merge-group run 38839823549 gives the room each shard had:

- `live-pg (dialect)` was the longest live shard at 21 minutes, so the
  PostgreSQL sample runs in `capture` (7 minutes) instead. `dialect` skips it
  by name, `commutation::pairs`, and `capture` selects it by the same
  filter, so the two remain exact complements.
- SQL Server's `live (dialect)` took 5 minutes, beside `live (flow)` at 11.

Even several times slower on a runner, neither sample makes its shard the
longest. A pair added past the cap is a recorded change: it replaces one or
raises the cap, and then this table is re-measured.
