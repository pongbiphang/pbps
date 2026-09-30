# Live-test performance

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

## Arrival-triggered cancellation

The administrative-session cancellation regression (#1295) previously held
each operation indefinitely, then dropped it at a 60-second deadline. That
deadline included reaching the hook; only the subsequent wait was unnecessary.
The test now signals arrival after the run stores its owned administrative
session and the hook establishes the real forwarder/observer fault. It drops
the exact boxed operation on arrival. The 60-second timeout bounds missing
arrival only. Production code and all cleanup observations remain unchanged.

Paired local runs used the same `live-test` profile, pinned SQL Server 2025
and PostgreSQL 18.6 images, native Docker 29.8.1, and maintained dedicated-server
fixture. The baseline added arrival instrumentation without changing the old
deadline. Prebuilt baseline and replacement binaries were retained separately;
compilation is excluded from the following measurements.

| Workload | SQL Server before | SQL Server after | PostgreSQL before | PostgreSQL after |
| --- | ---: | ---: | ---: | ---: |
| Cancellation case, including retries and cleanup | 139.45s | 17.85s | 193.71s | 12.62s |
| Complete dedicated-server segment, 22 fixture invocations | 612.46s | 367.53s | 449.74s | 252.66s |

The baseline's measured post-arrival waits totaled 117.32s for SQL Server's
two variants and 177.14s for PostgreSQL's three. After replacement, arrival
to dropping the operation took 34–52 microseconds and 14–38 microseconds,
respectively. These intervals distinguish a test-imposed wait from engine work.
The shared host and other cases' variable runtimes prevent attributing every
segment-level difference to this change. A segment is not the whole resolver
CI job, and overlapping jobs' savings cannot be added as pipeline wall time.

Reproduce each complete segment by discovering the compiled `pbps_cli` artifact
as above, retaining each variant's executable outside the repository, and
running the maintained fixture on a qualified native host:

```bash
sudo python3 scripts/live-resolver-server.py mssql --test-binary "$PBPS_FIXTURE_BINARY"
sudo python3 scripts/live-resolver-server.py pg --test-binary "$PBPS_FIXTURE_BINARY"
```

Use `--socket` when the owned native daemon has a non-default socket. Keep the
same fixture source and pinned images, and record build time, each variant's
arrival/post-arrival interval, case runtime, complete segment runtime, and CI
job runtime separately. The four ordinary controls require same-poll
cancellation and refuse missing arrival, a closed channel, or an operation
that completed instead of remaining held. The existing real-engine assertions
still require retry refusal, unchanged roles, actual leftover detection,
reported recovery names, exact owned-resource removal, and target integrity.
