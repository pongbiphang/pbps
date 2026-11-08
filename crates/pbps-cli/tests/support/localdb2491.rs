//! SQL Server CLI flows on Windows LocalDB (#2491, W1): the connected
//! rehearsal of a plan on a server scratch (#2379), with its offline
//! explanation and its checksum-approved apply, and the raised-guard messages
//! on every apply path.
//!
//! These run only in the `resolver-localdb` job, as its dedicated ordinary
//! user, against its private instances (`scripts/localdb_qualify.py`): the
//! target and the harness's own scratch connection are named-pipe strings, and
//! pbps reaches the scratch by its `(localdb)` selector. The bodies are the
//! ones the Linux jobs run; only the fixture differs. The Docker scratch kind
//! has no Windows counterpart here (hosted Windows runs Windows containers
//! only), so it stays an unexercised row of docs/RESOLVER-ACCEPTANCE.md.

use super::rehearsal2379_mssql::{self as cases, Kind};

fn target() -> String {
    std::env::var("PBPS_TEST_DB").unwrap()
}

#[test]
#[ignore = "needs the LocalDB instances of scripts/localdb_qualify.py"]
fn localdb_rehearsal_converges_on_a_server_scratch() {
    cases::converges(&target(), Kind::Server);
}

#[test]
#[ignore = "needs the LocalDB instances of scripts/localdb_qualify.py"]
fn localdb_rehearsal_refuses_a_rejected_statement_on_a_server_scratch() {
    cases::refuses_a_statement_the_engine_rejects(&target(), Kind::Server);
}

#[test]
#[ignore = "needs the LocalDB instances of scripts/localdb_qualify.py"]
fn localdb_rehearsal_off_switches_seal_off_on_a_server_scratch() {
    cases::off_switches_seal_off(&target(), Kind::Server);
}

#[test]
#[ignore = "needs the LocalDB instances of scripts/localdb_qualify.py"]
fn localdb_rehearsal_killed_on_a_server_scratch_is_cleaned_up_by_the_next_run() {
    cases::a_killed_rehearsal_is_cleaned_up_by_the_next_run(&target());
}

#[test]
#[ignore = "needs the LocalDB instances of scripts/localdb_qualify.py"]
fn localdb_rehearsal_without_an_entry_leaves_no_record() {
    cases::no_entry_no_record(&target());
}

#[test]
#[ignore = "needs the LocalDB instances of scripts/localdb_qualify.py"]
fn localdb_rehearsal_builds_a_base_that_calls_its_own_function_on_a_server_scratch() {
    cases::a_base_that_calls_its_own_function_rehearses(&target(), Kind::Server);
}

#[test]
#[ignore = "needs the LocalDB instances of scripts/localdb_qualify.py"]
fn localdb_rehearsal_runs_roles_and_grants_without_a_server_principal_on_a_server_scratch() {
    cases::authorization_is_rehearsed_and_makes_no_server_principal(&target(), Kind::Server);
}

#[test]
#[ignore = "needs the LocalDB instances of scripts/localdb_qualify.py"]
fn localdb_raised_guard_messages_are_shown_once_on_every_apply_path() {
    super::raised_messages::check(&target(), false);
}
