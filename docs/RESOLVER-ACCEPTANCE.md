# Resolver acceptance mapping

Parent: `#621`. Slice A is [#2487](https://github.com/pongbiphang/pbps/issues/2487);
[trace controls, B](https://github.com/pongbiphang/pbps/issues/2488) and
[final integration, C](https://github.com/pongbiphang/pbps/issues/2489) remain open.
This table is an explicit requirement-to-test mapping. It is not a completion
claim for #621 or #595. A pending accepted cell stays an obligation.

Three kinds of evidence stay separate:

- The mapping is a human judgement about what a test exercises.
- `scripts/check_resolver_acceptance.py` checks names, source definitions,
  owner/job/platform and the closed dispositions. It proves neither semantics
  nor execution. A green job that skipped a named test proves no cell.
- An execution receipt must identify the exact head, engine/build, scratch
  kind and platform and show the named test ran, not skipped. Local receipts
  live on #2487. C independently audits the final green merge-group run.

Test IDs below join each ADR row to an explicit per-test row. Coordinates are
not a cross product: a PostgreSQL server test does not qualify Docker, another
major, SQL Server, Windows or macOS. An engine-component fixture does not
qualify the CLI's scratch lifecycle. Test identity includes the recorded source
and qualified module name; equal leaf names in other targets are not duplicates.
`tests` means a mapped witness, not
that every assertion of the ADR case has completed qualification.

## ADR-0016 cases

The stable case numbers refer to [ADR-0016](ADR-0016-engine-assisted-planning.md#acceptance-tests).
Retired rows use its [one-resolver amendment](ADR-0016-engine-assisted-planning.md#amendment-the-resolver-1528-1636).
Each test row carries its build, scratch kind, platform and delivering PR.

<!-- acceptance-cases -->
| Case | Disposition | Tests | Reference | Engine/build | Scratch | Platform |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | tests | am-03, docker-answer | Observed motivating pair; SQL Server integration remains [#2143](https://github.com/pongbiphang/pbps/issues/2143) | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 2 | tests | am-03, historical, cross-kind, sql-surfaces | Creation-binding components; remaining surface/runtime cells stay with [#2489](https://github.com/pongbiphang/pbps/issues/2489) | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 3 | tests | historical, sql-binding | Observed current bindings, independent of a fresh bootstrap | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 4 | tests | am-27, am-11, am-19, sql-unreadable | Unreadable, unsupported and unfilled inputs do not become verified | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 5 | tests | reported-facts, preload, am-26 | Reported facts only; SQL2022 qualification remains open (measurement below) | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 6 | tests | selection, lazy, docker-start, docker-failure | Selection/acquisition components; end-to-end no-resolver flows remain [#2489](https://github.com/pongbiphang/pbps/issues/2489) | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 7 | tests | am-28, reader, am-25, apply-opening, connected-checks | Evidence/publication components; final negative-control audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 8 | tests | apply, approval, apply-closing | PostgreSQL apply; SQL Server waits for [#2144](https://github.com/pongbiphang/pbps/issues/2144) | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 9 | tests | snapshot, sql-observation | Engine-specific observation bounds, SPEC 7.6; no after-last-read guarantee | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 10 | measured profiles only, retired by #1636 | - | [ADR amendment](ADR-0016-engine-assisted-planning.md#amendment-the-resolver-1528-1636); plaintext trace replacement remains [#2488](https://github.com/pongbiphang/pbps/issues/2488) | Retired requirement; replacement coordinates below | Operator-vouched where SPEC says so | No measured-profile platform qualification |
| 11 | measured profiles only, retired by #1636 | am-01, am-04 | [SPEC 9.3.2](SPEC.md#932-engine-assisted-planning-postgresql-planning-and-apply-delivered): separation replaces the old same-instance ban | Retired requirement; replacement coordinates below | Operator-vouched where SPEC says so | No measured-profile platform qualification |
| 12 | measured profiles only, retired by #1636 | - | SPEC 9.3.2: the operator vouches for logging | Retired requirement; replacement coordinates below | Operator-vouched where SPEC says so | No measured-profile platform qualification |
| 13 | measured profiles only, retired by #1636 | - | SPEC 9.3.2: the operator vouches for containment | Retired requirement; replacement coordinates below | Operator-vouched where SPEC says so | No measured-profile platform qualification |
| 14 | tests | backend | Compatibility inputs only; SPEC 9.3.2 invalidates a changed backend rather than resuming its partial run | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 15 | tests | am-25, am-24, apply-opening | Stale external inputs refuse; complete engine/property controls remain in the closing audit | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 16 | tests | am-22, am-05 | PostgreSQL reproduction; SQL Server integration remains [#2143](https://github.com/pongbiphang/pbps/issues/2143) | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 17 | superseded by DEC-952.1 | - | [DEC-952.1](decisions/ledger.md#dec-952-1); keyed evidence; trace obligation [#2488](https://github.com/pongbiphang/pbps/issues/2488) remains open | Retired requirement; replacement coordinates below | Operator-vouched where SPEC says so | No measured-profile platform qualification |
| 18 | superseded by DEC-952.1 | approval | [DEC-952.1](decisions/ledger.md#dec-952-1); ordinary approval; trace obligation [#2488](https://github.com/pongbiphang/pbps/issues/2488) remains open | Retired requirement; replacement coordinates below | Operator-vouched where SPEC says so | No measured-profile platform qualification |
| 19 | measured profiles only, retired by #1636 | - | SPEC 9.3.2: the operator vouches for channels; ordinary connection TLS tests are not measured-profile qualification | Retired requirement; replacement coordinates below | Operator-vouched where SPEC says so | No measured-profile platform qualification |
| 20 | measured profiles only, retired by #1636 | - | SPEC 9.3.2: the operator vouches for channels | Retired requirement; replacement coordinates below | Operator-vouched where SPEC says so | No measured-profile platform qualification |
| 21 | measured profiles only, retired by #1636 | - | SPEC 9.3.2: in-place scratch mutation/exclusion is operator-vouched | Retired requirement; replacement coordinates below | Operator-vouched where SPEC says so | No measured-profile platform qualification |
| 22 | tests | apply-closing | PostgreSQL closing manifest; SQL Server waits for [#2144](https://github.com/pongbiphang/pbps/issues/2144) | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |
| 23 | measured profiles only, retired by #1636 | preload | SPEC 9.3.2: executable identity is operator-vouched; readable preload settings are reported-facts inputs, not executable proof | Retired requirement; replacement coordinates below | Operator-vouched where SPEC says so | No measured-profile platform qualification |
| 24 | tests | cross-kind, am-28 | Typed order; SQL Server ordering/evidence integration remains [#2143](https://github.com/pongbiphang/pbps/issues/2143) | Per-test rows below; other combinations unqualified | Per-test rows below; no cross-kind inference | Per-test rows below |

## Named test mappings and amendment rows

`am-01` through `am-28` give every original amendment acceptance
test its own row. The preload and backend rows add its compatibility controls.
The saved plan/SQL/checksum is not a plaintext guessing oracle after DEC-952.1;
the separate B trace obligation still needs execution evidence.

<!-- acceptance-tests -->
| ID | Test | Source | Target | Owner | Job | Platform | Engine/build | Scratch | Evidence | Delivery |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| am-01 | resolver::server::scratch::live_tests::resolver_refuses_the_target_as_scratch | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1678](https://github.com/pongbiphang/pbps/pull/1678); prefix [PR #1864](https://github.com/pongbiphang/pbps/pull/1864) |
| am-02 | resolver::server::scratch::live_tests::resolver_refuses_a_scratch_that_is_not_empty | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1678](https://github.com/pongbiphang/pbps/pull/1678); prefix [PR #1864](https://github.com/pongbiphang/pbps/pull/1864) |
| am-03 | resolver::server::scratch::live_tests::resolver_answers_a_managed_question_without_root_or_observation | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1678](https://github.com/pongbiphang/pbps/pull/1678); prefix [PR #1864](https://github.com/pongbiphang/pbps/pull/1864) |
| am-04 | resolver::server::scratch::live_tests::resolver_refuses_a_same_cluster_scratch_with_an_unconfined_account | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1678](https://github.com/pongbiphang/pbps/pull/1678); prefix [PR #1864](https://github.com/pongbiphang/pbps/pull/1864) |
| am-05 | resolver::server::scratch::live_tests::resolver_runs_in_a_precreated_database_with_a_confined_account | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1678](https://github.com/pongbiphang/pbps/pull/1678); prefix [PR #1864](https://github.com/pongbiphang/pbps/pull/1864) |
| am-06 | resolver::server::scratch::live_tests::resolver_empties_the_supplied_database_when_the_run_refuses | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1678](https://github.com/pongbiphang/pbps/pull/1678); prefix [PR #1864](https://github.com/pongbiphang/pbps/pull/1864) |
| am-07 | resolver::server::scratch::live_tests::resolver_compiles_a_managed_view_over_an_externals_table | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-08 | resolver::server::scratch::live_tests::resolver_refuses_an_externals_file_that_differs_from_the_target | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-09 | resolver::server::scratch::live_tests::resolver_refuses_an_externals_file_that_creates_a_managed_object | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-10 | resolver::server::scratch::live_tests::resolver_runs_the_externals_file_before_any_managed_object | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-11 | resolver::server::scratch::live_tests::resolver_refuses_an_externals_file_object_left_uncompared | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-12 | resolver::server::scratch::live_tests::resolver_compares_an_external_table_without_its_foreign_keys_and_triggers | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-13 | resolver::server::scratch::live_tests::resolver_lifts_a_chain_through_an_object_fill_stages | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2130](https://github.com/pongbiphang/pbps/pull/2130) |
| am-14 | resolver::server::scratch::live_tests::resolver_refuses_a_chain_through_an_object_the_externals_file_holds_naming_it | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2130](https://github.com/pongbiphang/pbps/pull/2130) |
| am-15 | resolver::server::scratch::live_tests::resolver_fills_the_external_objects_no_externals_file_stages | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2130](https://github.com/pongbiphang/pbps/pull/2130) |
| am-16 | resolver::server::scratch::live_tests::resolver_refuses_a_routine_fill_cannot_stage_naming_it | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2130](https://github.com/pongbiphang/pbps/pull/2130) |
| am-17 | resolver::server::scratch::live_tests::resolver_compiles_a_managed_view_over_an_external_view_staged_as_a_shape_view | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-18 | resolver::server::scratch::live_tests::resolver_refuses_an_external_view_staged_as_a_table | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-19 | resolver::server::scratch::live_tests::resolver_refuses_a_question_that_needs_routine_source_the_externals_file_lacks | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-20 | resolver::server::scratch::live_tests::resolver_reports_a_write_through_a_shape_view_with_the_real_definition_remedy | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-21 | resolver::server::scratch::live_tests::resolver_stages_a_cast_and_an_operator_over_external_types_in_the_externals_file | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-22 | resolver::server::scratch::live_tests::resolver_reproduces_the_deployers_usage_of_a_schema_the_externals_file_creates | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-23 | resolver::server::scratch::live_tests::resolver_recheck_binds_the_target_by_engine_identity | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1678](https://github.com/pongbiphang/pbps/pull/1678); prefix [PR #1864](https://github.com/pongbiphang/pbps/pull/1864) |
| am-24 | resolver::server::scratch::live_tests::resolver_recheck_refuses_an_external_shape_changed_after_planning | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1754](https://github.com/pongbiphang/pbps/pull/1754) |
| am-25 | resolver::server::scratch::live_tests::resolver_recheck_refuses_a_target_changed_after_the_run | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1678](https://github.com/pongbiphang/pbps/pull/1678); prefix [PR #1864](https://github.com/pongbiphang/pbps/pull/1864) |
| am-26 | resolver::server::scratch::live_tests::resolver_seals_an_accepted_build_limitation_and_rechecks_it | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1678](https://github.com/pongbiphang/pbps/pull/1678); prefix [PR #1864](https://github.com/pongbiphang/pbps/pull/1864) |
| am-27 | resolver::binding_tests::an_unstaged_external_object_of_any_class_is_refused_by_name | crates/pbps-pg/src/resolver/binding_tests.rs | pbps-pg/lib/pbps_pg | pg | live-pg | linux | PG 16.15; PG 18.6 | engine fixture; runtime kinds not inferred | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1026](https://github.com/pongbiphang/pbps/pull/1026) |
| am-28 | resolver_publishes_its_rechecked_plan_in_the_exact_order | crates/pbps-cli/tests/flow_pg.rs | pbps-cli/test/flow_pg | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1945](https://github.com/pongbiphang/pbps/pull/1945) |
| docker-start | resolver::server::scratch::live_tests::resolver_docker_starts_and_removes_its_container_without_root | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | docker | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1843](https://github.com/pongbiphang/pbps/pull/1843) |
| docker-failure | resolver::server::scratch::live_tests::resolver_docker_removes_its_container_when_the_run_fails | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | docker | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1843](https://github.com/pongbiphang/pbps/pull/1843) |
| docker-answer | resolver::server::scratch::live_tests::resolver_docker_answers_a_managed_question | crates/pbps-cli/src/resolver/server/scratch/live_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | docker | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1843](https://github.com/pongbiphang/pbps/pull/1843) |
| historical | resolver::binding_tests::the_observed_historical_binding_is_compared_not_a_fresh_bootstrap_of_the_old_source | crates/pbps-pg/src/resolver/binding_tests.rs | pbps-pg/lib/pbps_pg | pg | live-pg | linux | PG 16.15; PG 18.6 | engine fixture; runtime kinds not inferred | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1026](https://github.com/pongbiphang/pbps/pull/1026) |
| cross-kind | resolver::capture::ordering_tests::resolver_orders_cross_kind_binding_changes | crates/pbps-pg/src/resolver/capture/ordering_tests.rs | pbps-pg/lib/pbps_pg | pg | live-pg | linux | PG 16.15; PG 18.6 | engine fixture; runtime kinds not inferred | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1212](https://github.com/pongbiphang/pbps/pull/1212) |
| approval | a_resolved_plan_is_offered_the_ordinary_approval_command | crates/pbps-cli/tests/explain_resolver.rs | pbps-cli/test/explain_resolver | quick | quick | linux | offline model; no engine/build qualification | no resolver | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1945](https://github.com/pongbiphang/pbps/pull/1945) |
| reader | a_resolved_plan_explains_its_provenance_and_sealed_qualification | crates/pbps-cli/tests/explain_resolver.rs | pbps-cli/test/explain_resolver | quick | quick | linux | offline model; no engine/build qualification | no resolver | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1945](https://github.com/pongbiphang/pbps/pull/1945) |
| apply-opening | a_resolved_plan_is_refused_before_ddl_when_a_sealed_input_moved | crates/pbps-cli/tests/flow_pg.rs | pbps-cli/test/flow_pg | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2055](https://github.com/pongbiphang/pbps/pull/2055) |
| apply-closing | a_resolved_plan_rolls_back_when_its_closing_check_finds_another_writer | crates/pbps-cli/tests/flow_pg.rs | pbps-cli/test/flow_pg | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2055](https://github.com/pongbiphang/pbps/pull/2055) |
| apply | resolver_plans_apply_with_their_sealed_checks | crates/pbps-cli/tests/flow_pg.rs | pbps-cli/test/flow_pg | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2055](https://github.com/pongbiphang/pbps/pull/2055) |
| connected-checks | resolver_plans_pass_the_ordinary_connected_checks | crates/pbps-cli/tests/flow_pg.rs | pbps-cli/test/flow_pg | pg | live-pg | linux | PG 16.15; PG 18.6 | server | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1945](https://github.com/pongbiphang/pbps/pull/1945) |
| sql-surfaces | capture2141::mssql_each_surface_gets_the_engines_answer | crates/pbps-mssql/tests/live/capture2141.rs | pbps-mssql/test/live | mssql | live | linux | SQL Server 2025 17.0.4075.5 Developer | engine fixture; runtime kinds not inferred | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2150](https://github.com/pongbiphang/pbps/pull/2150) |
| sql-binding | capture2141::mssql_a_schema_bound_binding_is_the_targets_observed_one | crates/pbps-mssql/tests/live/capture2141.rs | pbps-mssql/test/live | mssql | live | linux | SQL Server 2025 17.0.4075.5 Developer | engine fixture; runtime kinds not inferred | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2150](https://github.com/pongbiphang/pbps/pull/2150) |
| sql-unreadable | capture2141::mssql_unreadable_dependency_metadata_never_verifies | crates/pbps-mssql/tests/live/capture2141.rs | pbps-mssql/test/live | mssql | live | linux | SQL Server 2025 17.0.4075.5 Developer | engine fixture; runtime kinds not inferred | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2150](https://github.com/pongbiphang/pbps/pull/2150) |
| sql-observation | binding619::a_commit_between_component_reads_reaches_a_later_read_or_follows_the_earlier_one | crates/pbps-mssql/tests/live/binding619.rs | pbps-mssql/test/live | mssql | live | linux | SQL Server 2025 17.0.4075.5 Developer | engine fixture; runtime kinds not inferred | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2076](https://github.com/pongbiphang/pbps/pull/2076) |
| preload | resolver::server::scratch::live_tests::acceptance621_tests::resolver_refuses_a_scratch_whose_preloaded_libraries_differ_from_the_target | crates/pbps-cli/src/resolver/server/scratch/live_tests/acceptance621_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server; docker | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2494](https://github.com/pongbiphang/pbps/pull/2494) |
| backend | resolver::server::scratch::live_tests::acceptance621_tests::resolver_replacement_backend_invalidates_the_registered_session | crates/pbps-cli/src/resolver/server/scratch/live_tests/acceptance621_tests.rs | pbps-cli/lib/pbps_cli | pg | live-pg | linux | PG 16.15; PG 18.6 | server/Docker connection guard; no resumed-plan claim | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2494](https://github.com/pongbiphang/pbps/pull/2494) |
| snapshot | resolver::capture::read::tests::the_capture_in_a_callers_transaction_refuses_a_commit_between_its_reads | crates/pbps-pg/src/resolver/capture/read.rs | pbps-pg/lib/pbps_pg | pg | live-pg | linux | PG 16.15; PG 18.6 | engine fixture; runtime kinds not inferred | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #2055](https://github.com/pongbiphang/pbps/pull/2055) |
| reported-facts | resolver_tests::scope610::the_rule_refuses_version_and_collation_differences_between_real_servers_and_verifies_a_matching_side | crates/pbps-pg/tests/support/resolver.rs | pbps-pg/test/live | pg | live-pg | linux | PG 16.15; PG 18.6 | engine fixture; runtime kinds not inferred | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #688](https://github.com/pongbiphang/pbps/pull/688) |
| selection | resolver::tests::explicit_selection_overrides_environment_then_project_without_credentials | crates/pbps-config/src/resolver.rs | pbps-config/lib/pbps_config | quick | quick | linux | offline model; no engine/build qualification | no resolver | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #622](https://github.com/pongbiphang/pbps/pull/622) |
| lazy | resolution::tests::without_a_selection_the_plan_is_never_assessed | crates/pbps-cli/src/resolution.rs | pbps-cli/lib/pbps_cli | quick | quick | linux | offline model; no engine/build qualification | no resolver | Receipt in [#2487](https://github.com/pongbiphang/pbps/issues/2487); final CI audit [#2489](https://github.com/pongbiphang/pbps/issues/2489) | [PR #1526](https://github.com/pongbiphang/pbps/pull/1526) |

## Engine, scratch and platform boundaries

| Operation | PostgreSQL | SQL Server | Windows | macOS |
| --- | --- | --- | --- | --- |
| Connected resolver planning | Linux PG16.15/18.6 witnesses above; each server/Docker cell needs its own receipt | Capture, separation and assessment components exist on pinned 2025 Developer; connected planning/evidence remains #2143 | Compile/dispatch is not live planning qualification; LocalDB is tracked by #2271 | Not exercised; not advertised as live qualified |
| Offline explanation | Ordinary evidence-reader tests; starts no resolver | Reader/evidence integration remains #2143 | Ordinary tests can exercise the portable reader; their run receipt is independent of planning/apply | Not exercised |
| Resolved apply | #616, independently checked on target; starts no resolver | Remains #2144 | No OS gate is not execution evidence; resolved apply remains unqualified without a live receipt | Not exercised |

The exact qualified core image/build/edition pins are in
[COMPATIBILITY](COMPATIBILITY.md). A compact core contract does not qualify
resolver binding. Express's edition/collation tests do not qualify a resolver
runtime, and Developer does not qualify Express.

### SQL Server 2022 measurement

A used two private instances of the pinned `16.0.4295.3` Developer build.
Both immutable-image, loopback endpoint, build, edition and collation checks
passed. The unchanged `resolver-mssql` scratch selection then yielded
3 passed and 11 failed: SQL2022 lacks `sys.external_models` and measured
SQL2025 model/`sys.databases` options, including
`is_optimized_locking_on`. The
[measurement receipt](https://github.com/pongbiphang/pbps/issues/2487#issuecomment-6476827275)
records the experiment.

The approved condition for adding SQL2022 as only an existing-job image
parameter is therefore false. No 2022 resolver CI cell is enabled. Versioned
catalog/option qualification remains an explicit accepted integration gap in
[#2489](https://github.com/pongbiphang/pbps/issues/2489#issuecomment-6476828915).
This is neither a green cell nor a waiver at #595 close-out.

### Confidentiality and retired profiles

`#594` legacy-reader cases are **not required after DEC-952.1**.
The plan records keyed external fingerprints, not a public small-dictionary
oracle; the ordinary approved checksum remains public.
Cases 17/18 do not reintroduce a protected ledger, launch path or history field.
B must still scan every output sink and exercise its planted-leak and protection
controls. Its unfinished work is not qualified by #618 being closed.

For cases 12, 13, 19, 20 and 21, the operator vouches for logging, containment,
channels and in-place scratch stability under SPEC 9.3.2. There is no
replacement measured-profile test. Case 11 has the named separation replacements
above. Case 23 has only reported-facts replacements; preload matching is not
executable identity.

### Platform qualification tracking

[Windows qualification, #2491](https://github.com/pongbiphang/pbps/issues/2491)
builds on #2271's LocalDB route without treating it as complete connected
planning/apply qualification.
[macOS qualification, #2492](https://github.com/pongbiphang/pbps/issues/2492)
coordinates with #626's ordinary TLS work. Both are unclaimed platform follow-ups,
as approved by #621's P-C decision; neither is a passing matrix cell.

## Closing audit

C follows #2144 and A/B. It names one final green merge-group run and head and
confirms each mapped test actually executed in its owner job on the stated
combination. It records each accepted gap and the maintainer's per-issue
disposition. Neither an inventory entry nor another engine/kind/platform can
fill a missing cell, and no unrelated historical issue closes by keyword.
