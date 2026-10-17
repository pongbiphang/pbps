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

## SCRAM in test builds

Every PostgreSQL connection authenticates with SCRAM-SHA-256: 4096 rounds of
HMAC-SHA-256. Unoptimized, that cost about 100 ms of CPU per connection, and a
`flow_pg` case opens about 15 of them (#2113). `Cargo.toml` therefore optimizes
two dependencies under `profile.dev.package` (`opt-level = 3`):
`postgres-protocol`, which holds the loop and the generic `hmac` code compiled
into it, and `sha2 0.11`, which holds the block compression. `test` and
`live-test` inherit `dev`, so both pick them up; `release` does not.

Which crates matter was measured, not read off the dependency tree: with the
two optimized, adding `hmac`, `digest`, `block-buffer`, `crypto-common`,
`hybrid-array`, `cpufeatures`, `stringprep`, `base64` or `rand` changed nothing,
while leaving out `postgres-protocol` or `sha2` kept most of the cost.
`sha2 0.10` is the plan checksum, not on this path.

Same host (Intel i9-14900F, shared, load average 5 to 14), the same pinned
PostgreSQL 18 digest in a private container, `pg_hba` switched between
`scram-sha-256` and `trust` on the host lines. The 19 cases are the
`flow_pg` `through_the_cli` tests that need only `PBPS_TEST_PG_DB`, run with
`--test-threads=1` from the test binary, so compilation is not in the figures.
CPU is user time of the test process and its children; wall time moves with
the neighbours' load, user time much less.

| Workload | Before | After | Trust, for reference |
| --- | ---: | ---: | ---: |
| One `plan --db`, user CPU, 10 interleaved runs | 0.126 s | 0.021 s | 0.017 s |
| 19 `flow_pg` cases, user CPU | 124.3 s, 75.2 s | 20.7 s, 20.6 s | 16.1 s |
| 19 `flow_pg` cases, wall | 282.2 s, 156.0 s | 107.0 s, 106.2 s | 104.6 s |

Two runs are listed where two were made. The two "before" runs differ because
the first ran at load 12 on 12 cores; the "after" runs do not.

The cost of the extra optimization, cold target directories, `cargo test
--workspace --all-targets --no-run`:

| Build | Wall | User CPU |
| --- | ---: | ---: |
| Without | 209.5 s | 1542.5 s |
| With the two crates | 215.2 s | 1481.2 s |

The workspace figures differ by less than the host's noise. Isolated, the
closure of `postgres-protocol` costs 4.3 s more user CPU (12.2 s to 16.6 s) and
0.9 s more wall.

Workspace crates keep `opt-level = 0` (`1` under `live-test`), debug
assertions and overflow checks. Cargo's `compiler-artifact` records for the
`test`, `dev` and `live-test` profiles show it, and a `--release` check shows
the release profile with its own settings only:

```bash
cargo check --profile test -p pbps-cli --bin pbps --message-format json
```

Optimizing every dependency (`profile.dev.package."*"`, `opt-level = 1`) was
measured as a comparison and is not adopted (DEC-2113.1).

## Resolver scratch profile comparison

Measured #2114 on `8e6a6d344f6a13ac5be412285c7327a9fa5a9565`, including
#2113, on a shared Intel Core i7-8850H host with Rust 1.99.0, two build jobs
and debug information disabled in both profiles. Both used lane 4's same
pinned PostgreSQL 18.6/16.15 targets and separate PostgreSQL 18.6 scratch.

Build cost is marginal to the existing job sequence. Untimed preparation used
`--no-run`: `pbps-pg --lib` under `live-test`, and `pbps-pg --test live`,
`pbps-db --test live_pg`, `pbps-cli --bin pbps` and `pbps-cli --test flow_pg`
under `test`. Each timed candidate was `cargo test --locked --profile
<profile> -p pbps-cli --lib --no-run --message-format json`.

CI's per-suite dependency cache hits in full (DEC-1370.1). `rust-cache` keeps
third-party artifacts and rebuilds workspace crates by default
(`cache-workspace-crates: false`). The cache-hit measurement populated both
candidates first, then used `cargo clean --profile <profile> -p <package>`
only for workspace packages each candidate had newly compiled. Cargo's
records confirmed zero third-party rebuilds in both timed candidates.
`test` rebuilt `pbps-cli`; `live-test` rebuilt `pbps-cli`, `pbps-config`,
`pbps-db`, `pbps-dialect`, `pbps-diff`, `pbps-docs`, `pbps-load`, `pbps-model`,
`pbps-mssql`, `pbps-pg`, `pbps-policy` and `pbps-ui`.

The dependency-cache-miss comparison instead copied the prepared state once
and restored it at the same absolute path between candidates, without first
populating the extra CLI dependency variants. The temporary copy was deleted.

| Build after existing steps | Profile | Wall | User CPU | System CPU |
| --- | --- | ---: | ---: | ---: |
| Dependency cache hit; workspace rebuild | `test` | 9.85 s | 12.40 s | 1.21 s |
| Dependency cache hit; workspace rebuild | `live-test` | 270.26 s | 521.75 s | 11.45 s |
| Dependency cache miss | `test` | 8.97 s | 10.96 s | 1.10 s |
| Dependency cache miss | `live-test` | 426.98 s | 811.19 s | 22.60 s |

After one full warm-up per profile, Cargo's artifact executables ran in
`test`, `live-test`, `test`, `live-test` order, without compilation:

```bash
/usr/bin/time -f '%e %U %S' "$executable" \
  --ignored --test-threads=1 resolver::server::scratch::live_tests --show-output
```

| Trial | Profile | Wall | User CPU | 1,000 roles | 10,000 roles |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | `test` | 1380.86 s | 947.44 s | 14.94 s | 29.68 s |
| 1 | `live-test` | 623.97 s | 232.81 s | 5.94 s | 12.25 s |
| 2 | `test` | 1338.98 s | 935.08 s | 14.35 s | 30.47 s |
| 2 | `live-test` | 639.91 s | 240.47 s | 6.04 s | 12.23 s |

Both warm-ups and all four trials passed the identical 76 scratch tests,
including the Docker cases. No known flake occurred. The role-cost test's
20 s difference bound had gaps of 14.74/16.12 s under `test` and 6.31/6.20 s
under `live-test`. Cargo's artifact records showed optimization levels 0/1
and debug information 0 for the harnesses and all workspace libraries, with
`debug_assertions` and `overflow_checks` true. Both profiles retained #2113's
optimization level 3 for `postgres-protocol` and `sha2 0.11`.

Both paired user-CPU savings, 714.63/694.61 s, exceed the cache-hit workspace
build premium of 509.35 s. This premium is an upper bound because cleaning
also removed `pbps-model`, `pbps-dialect`, `pbps-db`, `pbps-diff` and
`pbps-load`, which the job's earlier `pbps-pg --lib` step already builds under
`live-test`, so the actual premium is lower and strengthens the decision.
The scratch runners therefore select `live-test`
(DEC-1208.1), saving a median 195.27 s of CPU after compilation. The per-suite
`live-pg` job is on the merge-group critical path: median execution wall
saving is 727.98 s against 260.41 s extra build wall, net 467.57 s. CPU remains
the deciding metric. With the extra dependency variants missing, the 800.23 s
CPU build premium takes two module executions to amortize; dependency-cache
reuse is part of this tradeoff.
