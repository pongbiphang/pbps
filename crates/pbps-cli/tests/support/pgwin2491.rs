//! PostgreSQL CLI flows on the Windows runner's native clusters (#2491, W2): the
//! connected rehearsal of a plan on a server scratch (#2379) with its offline
//! explanation and its checksum-approved apply, and the resolved-plan
//! publication and apply flows (#616, #1516).
//!
//! These run only in the `resolver-pg-windows` job, as its dedicated ordinary
//! user, against two native clusters it creates (`scripts/pgwin_qualify.py`):
//! `PBPS_TEST_PG_DB` is the target cluster and `PBPS_TEST_PG_SCRATCH_DB` the
//! cluster of its own the server scratch kinds use. The bodies are the ones the
//! Linux jobs run; only the fixture differs. The Docker scratch kind has no
//! Windows counterpart here (hosted Windows runs Windows containers only), and
//! the PostgreSQL 16 half of the two-server tests has no counterpart (the image
//! ships one major), so those stay unexercised rows of
//! docs/RESOLVER-ACCEPTANCE.md.

use super::rehearsal2379::{self, Kind};
use super::resolver_publication;
use super::server;

/// The scratch placements a native cluster offers without Docker.
const NATIVE: [Kind; 2] = [Kind::Server, Kind::Supplied];
const NATIVE_FULL: [Kind; 3] = [Kind::Server, Kind::Supplied, Kind::SuppliedRoles];
const NATIVE_ALL: [Kind; 4] = [
    Kind::Server,
    Kind::Supplied,
    Kind::SuppliedRoles,
    Kind::SuppliedPlain,
];

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_converges_on_every_native_scratch() {
    for kind in NATIVE {
        rehearsal2379::converges(&server(), kind);
    }
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_shares_one_scratch_with_the_resolver() {
    for kind in NATIVE {
        rehearsal2379::shares_one_scratch(&server(), kind);
    }
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_refuses_a_statement_the_engine_rejects() {
    for kind in NATIVE {
        rehearsal2379::refuses_a_statement_the_engine_rejects(&server(), kind);
    }
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_off_switches_seal_a_plan_as_not_rehearsed() {
    for kind in NATIVE {
        rehearsal2379::off_switches_seal_off(&server(), kind);
    }
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_without_an_entry_carries_no_record() {
    rehearsal2379::no_entry_no_record(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_supplied_database_is_empty_after_every_use() {
    rehearsal2379::supplied_database_is_empty_after_each_use(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_stages_the_entrys_externals_and_compares_them() {
    rehearsal2379::stages_and_compares_externals(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_authorization_is_rehearsed_on_roles_made_for_the_run() {
    for kind in NATIVE_FULL {
        rehearsal2379::authorization_is_rehearsed(&server(), kind);
    }
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_a_wrong_policy_is_refused_and_leaves_no_role() {
    for kind in NATIVE_FULL {
        rehearsal2379::a_wrong_policy_is_refused(&server(), kind);
    }
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_an_account_without_createrole_is_downgraded() {
    rehearsal2379::an_account_without_createrole_downgrades_and_says_so(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_killed_run_is_cleaned_up_by_the_next_run() {
    for kind in [Kind::Server, Kind::SuppliedRoles] {
        rehearsal2379::a_killed_rehearsal_is_cleaned_up_by_the_next_run(&server(), kind);
    }
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_killed_same_cluster_run_leaves_nothing_naming_the_real_role() {
    rehearsal2379::a_killed_same_cluster_rehearsal_leaves_nothing_naming_the_real_role(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_a_policy_naming_a_role_in_text_converges_on_every_native_placement() {
    for kind in NATIVE_ALL {
        rehearsal2379::a_policy_naming_a_role_in_text_converges_on_every_placement(&server(), kind);
    }
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_concurrent_plans_do_not_collide() {
    rehearsal2379::concurrent_plans_do_not_collide(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_a_declared_domain_is_rehearsed() {
    rehearsal2379::a_declared_domain_is_rehearsed(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_a_declared_enum_is_rehearsed() {
    rehearsal2379::a_declared_enum_is_rehearsed(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_rehearsal_a_kept_module_is_rebuilt_under_its_recorded_dependencies() {
    rehearsal2379::a_kept_module_is_rebuilt_under_its_recorded_dependencies(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_resolver_publishes_its_rechecked_plan() {
    resolver_publication::published(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_resolver_plans_pass_the_ordinary_connected_checks() {
    resolver_publication::ordinary_checks(&server(), false);
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_resolver_plans_apply_with_their_sealed_checks() {
    resolver_publication::applied(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_resolver_plan_is_refused_before_ddl_when_a_sealed_input_moved() {
    resolver_publication::refused_before_ddl(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_resolver_plan_closes_over_what_it_newly_reaches() {
    resolver_publication::newly_reached(&server());
}

#[test]
#[ignore = "needs the native PostgreSQL clusters of scripts/pgwin_qualify.py"]
fn pg_resolver_plan_rolls_back_when_its_closing_check_finds_another_writer() {
    resolver_publication::closing_refusals(&server());
}
