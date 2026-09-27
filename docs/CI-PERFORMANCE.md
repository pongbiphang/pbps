# Live-test build performance

CPU-heavy PostgreSQL capture and resolver fixtures use Cargo's `live-test`
profile. It inherits `test` and enables basic optimization (`opt-level = 1`)
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
hashes executable content, including large mapped engine packages, on both
sides and at subsequent observations. These checks exercise different
invariants and remain intact.

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

`scripts/live-resolver.py`, `scripts/live-resolver-target.py` and the nested
`resolver-daemon-fixture.py` build with the same profile. On a disposable native Linux host, the root fixtures still use
a prebuilt test executable discovered from Cargo's JSON artifacts:

```bash
cargo test --profile live-test -p pbps-cli --lib --no-run --message-format=json
```

Select the executable artifact whose target name is `pbps_cli`, as the CI
workflow does, and pass its absolute path with `--test-binary`. Do not guess a
path under `target/debug`: the custom profile has its own output directory.
Required root permissions and owned fixture isolation remain the same as
[resolver runtime verification](RESOLVER-RUNTIME.md#verification).

The serial schedules, all positive and negative scenarios, both engine
versions, catalog rereads, content hashes and cleanup assertions remain in
place. A faster build mode does not authorize relaxing those checks. Further
parallelization would require independent fixtures for namespace mutation,
runtime failures, session census and server shutdown.
