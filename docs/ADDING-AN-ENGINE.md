# Adding an engine

This guide walks through wiring a new database engine into `pbps`, in the order
the work is done. Each step names what to change and **who tells you if you
forget**: the Rust compiler, a named test, the CI consistency check
(`scripts/check-engine-lists.py`), or nobody.

The single source of truth for selectable engines is
`pbps_config::DialectName::ALL` (DEC-2798.18). Read
[ARCHITECTURE.md](ARCHITECTURE.md) for crate boundaries and
[ORDERING.md](ORDERING.md#adding-an-engine) for the ordering contract before
writing code.

## 1. Add the driver module and engine crate

- **Driver module in `pbps-db`**: add `crates/pbps-db/src/<engine>.rs` under the
  one-file-per-driver isolation rule ([ARCHITECTURE.md](ARCHITECTURE.md)
  constraint 9: nothing outside that module names the driver crate).
- **Enum variants in `pbps-db`**: add the variant to `pbps_db::Driver`,
  `pbps_db::Driver::ALL`, and `pbps_db::Conn` (`crates/pbps-db/src/lib.rs`).
- **Engine crate**: create `crates/pbps-<engine>` implementing
  `pbps_dialect::Dialect`, `pbps_dialect::PlanDialect`,
  `pbps_db::engine::Engine`, catalog introspection, ledger/lock state SQL,
  preflight probes, and rename impact. Add it to workspace `Cargo.toml` and
  `crates/pbps-cli/Cargo.toml`.

**Who tells you if you forget:**
- The **compiler** fails on every exhaustive `match` on `Driver` and `Conn`
  inside `pbps-db`. `Driver::ALL` is a list kept by hand: the test of step 3
  is what fails when it misses the new variant.
- **Nobody** forces you to create the crate before you register the dialect in
  step 2.

## 2. Register the dialect in `pbps-config`

- Add the variant to `pub enum DialectName`, to `DialectName::ALL`, and to
  `DialectName::as_str` in `crates/pbps-config/src/lib.rs`.
- Regenerate published JSON schemas under `schemas/` if the config schema enum
  changed.

**Who tells you if you forget:**
- The **compiler** fails on `DialectName::as_str`.
- `pbps_config::type_routine_tests::dialect_name_all_matches_schema_enum_and_round_trips`
  fails if `DialectName::ALL` disagrees with the `schemars` enum or fails to
  round-trip through `Config::parse`.
- `pbps-cli`'s published-schema test
  (`the_checked_in_schemas_are_what_this_binary_produces`) fails if
  `schemas/*.json` is not regenerated.
- `init --dialect` derives its valid values and completions from
  `DialectName::ALL`, so it updates automatically.

## 3. Wire CLI selection, sessions, and profile

- **`crates/pbps-cli/src/emission.rs`**: add the variant to `SelectedDialect`,
  implement `sealed::Engine` and `emission::Engine`, and add the arm to
  `with_plan_dialect!`.
- **`crates/pbps-cli/src/main.rs`**: add the arm in `dialect_for`.
- **`crates/pbps-cli/src/db.rs`**: add the arm in `driver_for`.
- **`crates/pbps-cli/src/engine.rs`**: add the arm in `of` and in
  `validate_plan_analysis`.
- **`crates/pbps-cli/src/engine/profile.rs`**: add a full `Profile` value (no
  `..Default`) and wire it into `profile(driver)`.
- **`crates/pbps-cli/src/engine/features.rs`**: implement `Features` for the
  concrete dialect, choosing each feature's implementation or `Absent`
  (DEC-2798.19).
- **`crates/pbps-cli/src/engine/plan_session.rs`** and
  **`crates/pbps-cli/src/engine/apply_session.rs`**: implement `PlanSession`
  and `ApplySession` for the concrete dialect, and add the arm in
  `apply_session::selected`.
- **`pbps_cli::resolver::server::PlanningDialect`**: implement it for the
  dialect (`type Resolver = NoResolver<Self>` if it has no resolver,
  DEC-2798.5).

**Who tells you if you forget:**
- The **compiler** fails on every exhaustive `match` on `DialectName`, `Driver`,
  and `SelectedDialect`, on any missing `Profile` field, when an engine has no
  `impl Features` or omits a feature associated type, and on any required
  `Engine`, `PlanSession`, `ApplySession`, or `PlanningDialect` trait item.
- `pbps-cli`'s `engine::engine_list_tests::every_cross_crate_engine_list_matches_dialect_name_all`
  fails if `Driver::ALL` or `support::Engine::ALL` is not in exact bijection
  with `DialectName::ALL`.
- `pbps-cli`'s `engine::of_tests` and `engine::resolver_name_tests` loop over
  `DialectName::ALL` and fail if `of(driver).driver()`, `of(driver).dialect()`,
  `of(driver).name()`, `dialect_for(d).name()`, or `PlanningDialect::NAME`
  disagree or collide.

## 4. Record object support and regenerate `docs/STATUS.md`

- Add the variant to `pbps_dialect::support::Engine`, `Engine::ALL`, and
  `Engine::heading` in `crates/pbps-dialect/src/support.rs`.
- Add the engine's records to `support::RECORDS` (at least its declared object
  kinds, plus any named/not-read kinds) and its per-engine table in
  `support::status_tables()` if it has non-declared kinds.
- Add the generated region to `docs/STATUS.md` if needed, and regenerate the
  tables with `PBPS_REGENERATE_STATUS=1 cargo test -p pbps-pg --test support_table`.

**Who tells you if you forget:**
- `pbps-cli`'s `engine::engine_list_tests::every_cross_crate_engine_list_matches_dialect_name_all`
  fails if `support::Engine::ALL` is missing the engine, has no declared record,
  or is missing from the `declared` status table header.
- `pbps_dialect::support::tests::every_listed_engine_has_its_own_column_in_the_declared_table`
  fails if two engines share a heading or one has no column.
- `crates/pbps-pg/tests/support_table.rs` fails if `docs/STATUS.md` does not
  match `support::status_tables()`.

## 5. Wire CI, live runners, registries, and architecture docs

- Add the engine to `scripts/engine-ci.json`:
  - **If it is a server engine** (`"mode": "server"`): add its live job and
    compatibility matrix cell to `.github/workflows/ci.yml` (and add the live
    job to `gate.needs`), add its cell(s) to `scripts/compatibility-matrix.json`
    and `scripts/compatibility-tests.sh`, add its live runner script
    (`scripts/live-tests-<engine>.sh`), register it in `scripts/live-shards.py`
    (`RUNNERS` and `FIXTURES`), and register its `#[ignore]` owner/rules in
    `scripts/ignored-test-owners.json` and `scripts/ignored-test-rules.json`.
  - **If it runs in process with no container** (`"mode": "in-process"`): record
    a non-empty `"reason"` explaining why it needs no container or `#[ignore]`
    suite.
- Add the engine's driver dependency row to `docs/SPEC.md` (§11.3 Principal
  dependencies) and its crate entry to `docs/ARCHITECTURE.md` (Architectural
  boundaries).

**Who tells you if you forget:**
- `scripts/check-engine-lists.py` (run in the `lint` job of
  `.github/workflows/ci.yml`) fails naming the engine, the file, and the missing
  entry whenever an engine in `DialectName` lacks either its complete server CI
  wiring or an explicit `in-process` marker with a reason, or is missing from
  `docs/SPEC.md` §11.3 or `docs/ARCHITECTURE.md`.
- `scripts/check_gate_needs.py` fails if a new CI job is missing from
  `gate.needs`.
- `scripts/ignored_test_inventory.py` fails if any `#[ignore]` test has no
  owner or rule.
- `scripts/local_gate_scope_test.py` fails if a live runner invokes a `scripts/`
  helper that `local_gate_scope.py::KEEP_FULL` does not cover.

## 6. What nobody tells you (check these by hand)

Even with the compiler, Rust tests, and `scripts/check-engine-lists.py` green,
these areas have no mechanical check and must be audited explicitly:

1. **Dialect-name comparisons in neutral code.** Inequality checks such as
   `dialect != DialectName::Mssql` (`pbps-config`), `plan.dialect != "postgres"`
   (`pbps-model` wire validators `adoption.rs`, `retained.rs`,
   `resolver/evidence.rs`), `dialect.name() != "postgres"` (`pbps-cli`
   PostgreSQL-only reference words), or `dialect.name() == "sqlite"`
   (`pbps-diff/src/order/deployment/schedule.rs`) do not fail to compile when a
   new engine is added. Grep for `DialectName::`, `.dialect`, and `.name() ==` /
   `!=` and decide whether each check applies to the new engine.
2. **Messages and help strings written for a server.** Diagnostics, `doctor`
   advice, `--help` descriptions, and refusal text often assume a network server
   ("host", "server", "connection string", "login", "GRANT"). If the new engine
   is embedded, file-based, or uses different nouns, audit user-facing strings
   and `Profile` wording.
3. **Command and flag refusal order.** Commands that only apply to server
   engines (or to a subset of engines) must check `Profile::unsupported_command`
   / `require_command_feature` *before* demanding environment variables,
   connection strings, project files, or connecting. Otherwise the operator sees
   "missing env var" instead of "not supported on <engine>".
4. **End-to-end behaviour on the real engine.** Build the binary with the new
   dialect selectable and run every command (`init`, `validate`, `plan`,
   `plan --db`, `explain`, `bootstrap`, `apply`, `verify`, `status`, `pull`,
   `doctor`, `fmt`, `docs`, `--staged`). Subtle bugs only surface at run time:
   read-only `plan --db` taking a write lock, `apply` dropping preflight
   warnings, redaction mangling a connection string or file path, or an engine
   stripping/rewriting view or module terminators and trailing newlines so
   read-back verification rolls back a valid apply.
5. **Named test subsets.** Tests over all engines loop over `DialectName::ALL`
   (or `engine::all_drivers()`) and pick up the new engine automatically. Tests
   over a deliberate subset (`CONTAINER_SCRATCH_DRIVERS`, `SERVER_DIALECTS`, `ROLE_DIALECTS`,
   `MULTI_SCHEMA_DIALECTS`, `IDENTITY_AND_DATA_DIALECTS`,
   `managed_data_dialects()`) do not: decide for each subset whether the new
   engine supports that feature and belongs in it.
6. **Optional server qualification scripts.** If the new engine supports TLS
   transport tests (`scripts/live-transport.py`), release binary qualification
   (`scripts/qualify-release.py`, `scripts/release-windows-fixture.ps1`),
   effect-mutation audits (`scripts/effect-mutations.py`), or `pbps test` owned
   containers (`crates/pbps-cli/src/resolver/server/engine/container.rs`), wire
   them explicitly.
7. **`scripts/check-raw-in-messages.py` SQL-builder allowlist.** Its
   `SQL_BUILDERS` set lists `emit`, `preflight`, `rows`, and `catalog` for
   `pg` and `mssql`. You will only hear from it once the new engine's SQL
   builder formats a `.raw()` identifier; add the new crate's SQL builders
   deliberately when needed.
