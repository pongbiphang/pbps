# Live-test performance

CPU-heavy PostgreSQL capture fixtures use Cargo's `live-test` profile. It inherits `test` and enables basic optimization (`opt-level = 1`)
while explicitly retaining debug assertions and overflow checks. Ordinary
`cargo test` and the quick CI suite keep optimization level 0. The shared
[test-build debug setting](#test-build-debug-information) retains file/line
backtraces; the release profile is unchanged.

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

## Test-build debug information

Measured #2117 on `25d2eeedd678166896c746f4718de1c99448b396`, on the shared
Intel Core i7-8850H host with Rust 1.99.0, two build jobs and incremental
compilation enabled. The baseline used Cargo's full debug information; the
candidate set `CARGO_PROFILE_DEV_DEBUG=line-tables-only`. Both used the same
absolute target path, initially empty, and a warm registry cache. The target
was deleted between candidates and after measurement. No two measurement
builds ran together.

Each candidate ran this command three times: cold, after appending
`pub const ISSUE_2117_RELINK_PROBE: u8 = 1;` to `crates/pbps-model/src/lib.rs`, then
after appending a one-line passing test to `crates/pbps-cli/tests/flow.rs`. The
model probe stayed in place for the leaf run; both probes were then removed.
The leaf run rebuilt only the `flow` test target. The model run rebuilt 52
workspace artifacts and no third-party artifacts in either candidate.

```bash
CARGO_TARGET_DIR=/path/to/empty-target CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=1 \
  /usr/bin/time -f '%e %U %S %M' \
  cargo test --locked --workspace --all-targets --no-run --message-format=json \
  > artifacts.jsonl
```

| Build | Debug information | Wall | User CPU | System CPU | Max RSS, GiB |
| --- | --- | ---: | ---: | ---: | ---: |
| Cold | Full | 614.21 s | 1065.53 s | 109.48 s | 2.353 |
| Cold | Line tables only | 452.09 s | 782.42 s | 74.78 s | 1.573 |
| Model change | Full | 83.92 s | 115.94 s | 39.22 s | 2.359 |
| Model change | Line tables only | 69.24 s | 77.70 s | 26.17 s | 1.065 |
| Leaf change | Full | 11.05 s | 12.64 s | 2.37 s | 1.764 |
| Leaf change | Line tables only | 6.14 s | 5.97 s | 1.06 s | 0.827 |

RSS is GNU time's maximum process RSS, not aggregate memory across concurrent
compiler processes. Relink runs include compilation and linking; even the
isolated leaf measurement is not linker-only RSS. The cold runs started at
load averages 23.67 and 18.46, respectively. Swap was essentially full, with
negligible swap traffic during these builds and no OOM kills. These are one
paired observation per workload on a shared host, not fixed timing gates.

Allocated disk sizes, with hard links counted once, were:

| Debug information | Target state | Total, GiB | `debug/deps`, GiB | `debug/incremental`, GiB | Third-party cache proxy, GiB | Largest test, MiB |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Full | Cold | 14.020 | 8.430 | 5.456 | 1.075 | 568.304 |
| Line tables only | Cold | 6.791 | 3.475 | 3.181 | 0.868 | 215.567 |
| Full | After both changes | 15.958 | 8.430 | 7.393 | 1.075 | 568.304 |
| Line tables only | After both changes | 8.658 | 3.475 | 5.048 | 0.868 | 215.567 |

Both cold targets held 43 test executables; the largest was the `pbps` binary's
test harness. The lanes' long-lived target directories here reach 25–58 GB as
incremental state and the `live-test` profile accumulate beyond the fresh 14 GiB,
so the fresh-build reduction ratio is the comparison to apply when rebuilding
them. Executable sizes are apparent file bytes. The cache proxy removes
all incremental state and workspace files in `debug/deps`, `debug/.fingerprint`
and `debug/build`, plus workspace executables outside those directories.
Cargo's workspace target inventory and artifact records identify those files.
This approximates `rust-cache`'s third-party-only contents as uncompressed local
disk usage, not the compressed archive GitHub stores.

Line tables only reduces the post-relink target by 45.7%, incremental state by
31.7%, the cache proxy by 19.3% and the largest test executable by 62.1%.
The leaf run's user CPU and maximum RSS fall by 52.8% and 53.1%, respectively.
These disk and memory savings, with lower measured compilation CPU, justify
the shared default (DEC-2117.1). Full type and local-variable debug information
can still be selected locally with `CARGO_PROFILE_DEV_DEBUG=2`.

With the setting in `Cargo.toml` and no profile environment overrides,
`cargo check --locked --workspace --lib --profile <profile> --message-format=json`
checked all 12 workspace libraries under each profile, including every artifact
variant emitted by `test`:

| Profile | Workspace optimization | Debug information, before → after | Debug assertions / overflow checks |
| --- | ---: | --- | --- |
| `dev`, `test` | 0 | `2` → `line-tables-only` | true / true |
| `live-test` | 1 | `2` → `line-tables-only` | true / true |
| `release` | 3 | `0` → `0` | false / false |

Release artifact profiles were identical before and after. Both SCRAM crates
retained optimization level 3. The timed cold builds also confirmed unchanged
optimization, assertions and overflow checks for all 56 workspace artifacts,
including test harnesses. `test` inherits `dev`, and `live-test` inherits `test`,
so a single dev setting supplies the shared debug-information default.

A temporary model test ran from Cargo's recorded executable with
`RUST_BACKTRACE=1` and the exact filter
`issue_2117_backtrace_has_file_and_line`. It exited 101 as expected; both the
panic and its stack frame retained the probe's file and line:

```text
panicked at crates/pbps-model/src/lib.rs:98:55:
issue-2117 backtrace probe
   2: pbps_model::issue_2117_backtrace_has_file_and_line
             at ./crates/pbps-model/src/lib.rs:98:55
```

The probe was removed, and the original 414 model library tests passed.

## JSON parsing crate comparison

Measured #2129 on `591474f560ff38ed8bb8fb136785840fa416e3b4`, including
#2113, #2114 and #2117, on the shared Intel Core i7-8850H host with Rust
1.99.0, two build jobs, incremental compilation and line-tables-only debug
information. Both variants used lane 4's pinned PostgreSQL 18.6/16.15 targets
and separate PostgreSQL 18.6 scratch. A kept the existing profiles; B set
`profile.dev.package.serde_json.opt-level=3` and
`profile.dev.package.indexmap.opt-level=3` through Cargo's `--config`, in a
separate target directory. Builds and timed executions never overlapped.

Crate screening used the production scratch case
`resolver_run_owned_keeps_what_the_reproduction_put_on_its_database` under
`test`, with a warm-up and three alternating pairs per set. Median paired
user-CPU ratios to baseline were 0.763 for all three candidates, 0.764 without
`hashbrown`, 0.847 without `indexmap`, 0.800 for `serde_json` alone and 1.018
without `serde_json`. The noisy single-crate comparison was followed by three
direct pairs: adding `indexmap` to `serde_json` reduced user CPU from
18.69/19.89/19.51 s to 17.31/17.88/17.45 s. The full comparisons therefore
used only `serde_json` 1.0.151 and `indexmap` 2.14.1. No `itoa`, `memchr` or
`zmij` frames appeared within sampled deserialization stacks; `ryu` was absent
from the lockfile.

These build observations use `/usr/bin/time -f '%e %U %S'` and report wall /
user CPU seconds. Cold targets started empty; scratch compilation followed
the ordinary workspace build. Incremental runs appended a model constant,
then a passing test in `crates/pbps-cli/tests/flow.rs`, keeping the model
probe for the leaf run; both probes were removed afterwards. Cargo recorded
52 workspace artifacts rebuilt for each model change and only `flow` for
each leaf change, with no third-party compilation.

| Build | A wall / user CPU | B wall / user CPU |
| --- | ---: | ---: |
| Cold workspace, `test --workspace --all-targets --no-run` | 762.47 / 1130.15 s | 473.61 / 830.02 s |
| Additional scratch, `test --profile live-test -p pbps-cli --lib --no-run` | 887.15 / 1447.18 s | 606.58 / 1116.26 s |
| Model change, workspace | 51.50 / 76.09 s | 42.81 / 65.93 s |
| Leaf change, workspace | 6.55 / 6.42 s | 6.55 / 8.66 s |
| Dependency cache hit, workspace rebuild | 303.60 / 569.29 s | 414.65 / 706.35 s |
| Dependency cache hit, scratch rebuild | 332.22 / 630.56 s | 328.55 / 627.94 s |

The cache-hit measurements cleaned only the 12 workspace packages for each
profile. Cargo confirmed 56 rebuilt workspace artifacts under `test`, 12
under `live-test`, and zero third-party rebuilds in all four observations.
The observed ordinary workspace rebuild premium was 137.06 s user CPU;
the scratch rebuild difference was -2.62 s. These are single observations
on a shared host; the faster cold candidate builds do not establish a causal
compile-time saving. Third-party compilation is a cache-miss cost, whereas
workspace recompilation recurs with CI's dependency-only cache (DEC-1370.1).

After a full warm-up per workload and variant, execution order was A B A B.
The ordinary workspace command was `cargo test --locked --workspace
--all-targets`, without live-fixture variables; its measured runs compiled
nothing. The other workloads used Cargo's recorded test executables with
`--ignored --test-threads=1`: the two complementary `flow_pg` CI selections,
all ignored `pbps` bin tests, and the scratch module under `live-test`.
User CPU includes child processes and decides the comparison; wall time is
reported alongside it. Warm-ups are excluded below.

| Workload | Pair | A wall / user CPU | B wall / user CPU |
| --- | ---: | ---: | ---: |
| Workspace | 1 | 135.57 / 278.78 s | 169.27 / 285.74 s |
| Workspace | 2 | 187.33 / 302.92 s | 128.36 / 271.40 s |
| `flow-programs` | 1 | 255.69 / 46.57 s | 312.46 / 51.08 s |
| `flow-programs` | 2 | 259.14 / 47.54 s | 260.89 / 44.83 s |
| `flow-rest` | 1 | 1340.72 / 595.39 s | 1142.97 / 463.31 s |
| `flow-rest` | 2 | 1179.34 / 517.28 s | 1174.79 / 492.94 s |
| `pbps` bin | 1 | 4.20 / 0.53 s | 4.35 / 0.50 s |
| `pbps` bin | 2 | 4.35 / 0.54 s | 4.22 / 0.50 s |
| Scratch, `live-test` | 1 | 655.31 / 240.11 s | 639.36 / 233.52 s |
| Scratch, `live-test` | 2 | 721.90 / 242.91 s | 668.43 / 242.54 s |

Every warm-up and measured execution passed. Each workspace trial ran the
same 3,503 tests across 43 harnesses, the flow shards ran 86/204 cases, the
bin ran 11 and scratch ran all 89. Test-name lists matched between variants.

Cargo's artifact records matched all 324 `test` and 270 `live-test` records
between A and B, including targets, features and other profile fields.
Only the optimization levels of
`serde_json` (host and runtime variants) and `indexmap` changed. All 56
workspace artifacts under `test`, including 43 harnesses, retained opt 0;
all 12 workspace artifacts under `live-test` retained opt 1. Both kept
line-tables-only debug information, debug assertions and overflow checks.
`hashbrown` stayed at 0/1, and both SCRAM overrides stayed at 3.

One full-module GDB session per variant and profile selected the same 89
scratch cases. The sampler slept one second between observations, selected
threads in state `R` before attaching, and collected `thread apply all bt 80`.
The denominator below excludes stacks whose top frame contains `syscall`;
shares count visible inclusive frames for `capture_identifying`,
`capture::read::batch`, and deserialization inside that batch.

| Profile / variant | On-CPU stacks | Capture | Batch | Deserializer in batch |
| --- | ---: | ---: | ---: | ---: |
| `test`, A | 985 | 58.3% | 36.1% | 31.7% |
| `test`, B | 564 | 55.1% | 20.7% | 16.7% |
| `live-test`, A | 175 | 47.4% | 9.7% | 7.4% |
| `live-test`, B | 132 | 49.2% | 10.6% | 9.1% |

The 80-frame limit was reached by 193/82 `test` stacks and 39/20 `live-test`
stacks (A/B). Inlining and truncated ancestry limit these visible shares,
especially with the small `live-test` sample counts. Profiled timings are
excluded from the execution comparison. Only A's `test` session hit #2121:
its role-cost gap was 39.45 s under GDB against the 20 s bound; one exact
unprofiled retry passed with a 14.81 s gap. All other sampled cases passed.

The two-crate candidate is **not adopted**. Summing one execution of each
workload gives user-CPU savings of 127.23/58.98 s across the two pairs,
short of the 134.44 s premium for the measured cache-hit workspace and
scratch rebuilds together (137.06 - 2.62 s). These observations do not
establish CI payback. Ordinary workspace gains changed direction;
`flow-rest` saved 132.08/24.34 s, while scratch already at opt 1 saved only
6.59/0.37 s.
The microcase improvement therefore does not justify this profile change.
The manifest, release settings and DEC-2113.1's override scope remain as they
were.

## Ledger trust growth comparison

The ledger-trust integration regression (#2166, DEC-2166.1) uses the ordinary
`test` profile. Unchanged-source checks at 10,000 login roles ranged from
0.25 to 1.07 seconds with nearby host CPU samples between 28% and 99%.
PostgreSQL's main SQL execution accounted for about 71–96% of the timer;
role creation and removal were outside it, and the catalog observations found
no persistent crowd or heap growth. The reported 2.1-second failure under
heavier load is classified as the same host-contention shape.

The regression measures each privilege case three times at 1,000 roles and
three times at 10,000 roles, keeping the same crowd, database and connection
through the samples at each size. Each sample checks the expected 0/n/n named
roles. Only the check is timed; crowd creation and removal are excluded, and
removal precedes the correctness and timing assertions. The minimum at 10,000
roles must take less than 25 times the minimum at 1,000. A transient load spike
inflates an individual sample, while a quadratic editor walk inflates every
sample. The concurrent-owner, killed-owner and dropped-guard cleanup tests
remain in the full integration suite.

The measurements below use Rust 1.99.0, two Cargo build jobs and the pinned
PostgreSQL 18.6 fixture on a shared 12-logical-CPU host. Each cell is the
minimum of three samples in seconds. CPU is a nearby five-second sample,
not an instantaneous reading; other issue lanes supplied background load.

| Variant | Case | 1,000 roles | 10,000 roles | Growth | Host CPU |
| --- | --- | ---: | ---: | ---: | ---: |
| Production | No grant | .033665498 | .231180409 | 6.87× | 31.3% |
| Production | TRIGGER to PUBLIC | .036212794 | .258285257 | 7.13× | 31.3% |
| Production | UPDATE to PUBLIC | .045759995 | .227589361 | 4.97× | 31.3% |
| Trigger shortcut removed | TRIGGER to PUBLIC | .243209617 | 9.987414583 | 41.07× | 67.0% |
| Sequence shortcut removed | UPDATE to PUBLIC | .160671190 | 9.911997676 | 61.69× | 37.5% |

Each deliberately quadratic control removes only its own hashed self-editor
shortcut in `crates/pbps-pg/src/state.rs`, preserving the membership walk and
correctness. Remove `(l.oid, e.oid) IN (...) OR` from `TRIGGER_EDITOR` for the
trigger control, or `(d.objid, e.oid) IN (...) OR` from the sequence branch for
the sequence control. Restore each before testing the other. All 18 correctness
assertions passed before each control's growth assertion failed; restoring
production SQL byte-for-byte passed the positive run.

The 25× multiplier leaves 3.51× passing margin above the positive maximum of
7.13× and 1.64× / 2.47× rejection margins below the trigger / sequence controls.
The earlier single-sample calibration and two complete PG wrapper runs also
passed their positive comparisons under load; those observations motivated
the growth comparison, while the table above qualifies the final estimator.

Run with the fixture connection variables and
`cargo test -p pbps-pg --test live -- --ignored --exact
the_ledger_trust_check_stays_linear_in_the_clusters_roles --nocapture`.
Both local wrappers and CI continue to select the ordinary `test` profile.

## Resolver capture closure index

Measured #2128 against `5b885dd395bdb5c79b48f465b3b39d64cec7eabc` on
2026-10-22, using the shared Intel Core i7-8850H host, Rust 1.99.0, two build
jobs, the warm lane-2 target, PostgreSQL 18.6/16.15 targets and a separate
PostgreSQL 18.6 scratch. No profile, dependency or `RUSTFLAGS` setting changed.

A was the preserved baseline executable; B was an indexed executable. Both
used the CI scratch module's `live-test` profile (opt 1, line-tables-only,
debug assertions and overflow checks enabled). After full-module warm-ups,
each indexed implementation ran A B A B, without compilation or profiling:

```bash
/usr/bin/time -f '%e %U %S %M' "$executable" \
  --ignored --test-threads=1 resolver::server::scratch::live_tests --show-output
```

The simple implementation retains `Catalog::identity` for the ordered
validation pass, discards those temporary identities, and stores comparison
keys and row positions. The alternative uses one shared validation/construction
walk, retaining only comparison keys during validation; it adds 61 production
lines over the simple implementation. Both index candidates and child groups,
select dependency membership by local row position, and avoid allocating an
OID lookup's class key. Every successful run selected the same 92 tests.
User CPU includes child processes; the load columns show one-minute averages
at each run's start and end. All three load averages were recorded.

| Implementation | Pair | A wall / user CPU, s | B wall / user CPU, s | A load, start → end | B load, start → end |
| --- | ---: | ---: | ---: | ---: | ---: |
| Simple | 1 | 893.63 / 273.96 | 907.34 / 251.84 | 2.04 → 3.10 | 3.10 → 2.03 |
| Simple | 2 | 1140.15 / 355.52 | 952.59 / 267.51 | 2.03 → 2.48 | 2.48 → 13.67 |
| Shared walk | 1 | 1616.68 / 494.42 | 1243.09 / 324.75 | 13.67 → 11.54 | 11.54 → 16.65 |
| Shared walk | 2 | 1178.16 / 368.16 | 1128.70 / 320.52 | 9.10 → 4.64 | 4.64 → 11.37 |

The simple pairs saved 8.1%/24.8% user CPU (median 16.4%); the shared walk
saved 34.3%/12.9% (median 23.6%). The simple implementation retains about
69% of that observed median saving and is kept: validation still uses the
original identity rules directly, with no second validator. The separate
candidate-name projection is checked against those rules for every candidate
class. Load varied sharply, reaching 26.14 in sampled one-minute observations;
baseline user CPU alone ranged from 273.96 to 494.42 seconds. These paired
observations do not isolate the alternative's incremental benefit or establish
a fixed CI speedup.

One baseline attempt in the shared-walk block passed 88 tests and failed four:
PostgreSQL reported SQLSTATE 53100 (`No space left on device`), Podman could
not write layer metadata, and one scratch startup timed out. Space had
recovered to 67 GiB when checked. One unchanged full-module retry passed all
92 tests; the failed attempt is excluded from the table. Both warm-ups and
all eight successful measured runs passed, including the Docker cases.

A full-module GDB profile before and after the selected implementation used
the `test` profile (opt 0), a one-second sleep between observations, threads
observed in state `R`, and `thread apply all bt 80`. The denominator excludes
stacks with a syscall top frame or no readable top frame. Both profiled runs
passed all 92 tests; profiled timings are excluded from the execution table.
The visible inclusive shares are:

| Frame group | Before, % of on-CPU stacks | After, % of on-CPU stacks |
| --- | ---: | ---: |
| Capture | 55.55 | 52.09 |
| Catalog batch | 39.87 | 45.14 |
| Deserialization in batch | 33.35 | 40.37 |
| Scope preparation | 28.17 | 20.02 |
| Identity construction during preparation | 17.47 | 10.22 |
| Children during preparation | 2.89 | 1.01 |
| Dependencies during preparation | 2.24 | 1.26 |

There were 2,009/1,194 eligible on-CPU stacks and 514/338 stacks reaching the
80-frame limit (before/after). Sampled one-minute load ranged from
5.10–29.95 before and 3.09–5.67 after. Identity construction accounted for
62.01%/51.05% of preparation stacks; children accounted for 39.19%/25.53%
of expansion stacks. Inclusive frames overlap, and truncated ancestry limits
these shares; they are not exact function CPU percentages. Catalog reads,
rendering and canonicalization were unchanged.

With one fixed key and the same capture fixtures, the selected implementation's
sealed manifests, including membership, bindings and fingerprints, matched the
baseline byte for byte: 545,231 bytes on PostgreSQL 16 and 588,663 on
PostgreSQL 18. The 67 targeted unit tests and 26 live capture regressions passed.
Independently removing the unrelated-duplicate guard and skipping unrelated
identity validation each made its new negative test fail; restoring the guards
returned the suite to green.
