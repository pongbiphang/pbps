//! What each engine is, where the answer is a value and not a connected
//! operation (#2798).
//!
//! The functions of [`super`] that do no I/O used to `match` on the driver one
//! by one to pick a constant, a flag or a sentence, so a third engine meant an
//! arm in every one of them, scattered across the file. Their answers are the
//! fields of one [`Profile`] per engine, each written out in full (no
//! `..Default`), so a new engine's profile does not compile until it has an
//! answer for every field, and the compiler lists what it owes. [`profile`] is
//! the single `match` on the driver for this class of question.
//!
//! The readers keep their names and signatures in [`super`] and [`super::collected`],
//! and their doc comments say what the fact means; the comments here say why
//! the engines differ. The connected operations are not here: they are the
//! methods of `pbps_db::engine::Engine` ([`super::of`]), which cannot carry
//! these facts because several of them name types of `pbps-diff` and this
//! crate, which `pbps-db` cannot.

use super::*;

/// One engine's answers to the questions that need no connection.
///
/// A plain value where the fact is a constant, and a `fn` pointer only where
/// the fact needs an argument to answer.
pub(crate) struct Profile {
    /// Read by [`computed_column_screen`].
    pub computed_column_screen: pbps_diff::Screen,
    /// Whether [`counts_rows_an_enum_retype_strands`] looks at the plan at all.
    pub counts_stranded_rows_at_plan_time: bool,
    /// Read by [`vets_declared_domain_code`].
    pub vets_declared_domain_code: bool,
    /// Why drafting a resolver's externals file is refused for this engine, or
    /// `None` where it has a binding adapter. Read by
    /// [`refuse_externals_draft_unsupported`].
    pub externals_draft_refusal: Option<&'static str>,
    /// Read by [`scratch_compiles_base_namespace`].
    pub scratch_compiles_base_namespace: bool,
    /// Read by [`target_identity_unread`].
    pub target_identity_unread: fn(DbError) -> crate::resolution::Refused,
    /// Whether the resolver's scratch can land on the instance it rehearses, so
    /// that [`same_instance_consent`] has a choice to report.
    pub scratch_may_share_the_instance: bool,
    /// The name an unavailable cost report gives this engine
    /// ([`operational_cost`]).
    pub cost_report_engine: &'static str,
    /// Read by [`truncate_reason`].
    pub truncate_reason: fn(&str) -> String,
    /// Whether a catalog read inside the plan's own transaction must be
    /// revalidated before it is recorded ([`needs_readback_revalidation`]).
    pub revalidates_read_inside_own_transaction: bool,
    /// What the principal that lacks a grant is called in [`lock_remedy`]'s
    /// words.
    pub lock_principal_noun: &'static str,
    /// The placeholder [`lock_remedy`] prints for that principal.
    pub lock_principal_placeholder: &'static str,
    /// The remedy for one gap in reading the lock. `how` and `next` are the
    /// sentences every engine shares, and `principal` is the quoted
    /// placeholder.
    pub lock_remedy:
        fn(gap: pbps_db::doctor::LockReadGap, how: &str, next: &str, principal: &str) -> LockRemedy,
    /// Read by [`permission_read_advice`].
    pub permission_read_advice: fn() -> String,
    /// Read by [`online_depends_on_the_edition`].
    pub online_depends_on_the_edition: bool,
    /// Read by [`capabilities_unknown_message`], which passes the cause.
    pub capabilities_unknown_message: fn(cause: &str) -> String,
    /// Read by [`impact_notes`].
    pub impact_notes: fn(&ImpactReport) -> Vec<String>,
}

/// The profile of the engine `driver` names.
///
/// The one `match` on the driver for the questions that need no connection;
/// the readers in [`super`] go through it, so a third engine is a compile
/// error here and nowhere else for them (#2798).
pub(crate) const fn profile(driver: Driver) -> &'static Profile {
    match driver {
        Driver::Mssql => &MSSQL,
        Driver::Postgres => &POSTGRES,
    }
}

static MSSQL: Profile = Profile {
    computed_column_screen: pbps_diff::Screen::Catalog,
    // SQL Server has neither enums nor domains.
    counts_stranded_rows_at_plan_time: false,
    // SQL Server has no domains.
    vets_declared_domain_code: false,
    externals_draft_refusal: Some(
        "`pbps resolver externals` reads a PostgreSQL target; SQL Server has no binding \
         adapter yet (#619, #620)",
    ),
    scratch_compiles_base_namespace: true,
    target_identity_unread: mssql_identity_unread,
    scratch_may_share_the_instance: true,
    cost_report_engine: "sqlserver",
    truncate_reason: pbps_mssql::state::truncate_reason,
    revalidates_read_inside_own_transaction: false,
    lock_principal_noun: "user",
    lock_principal_placeholder: "database user",
    lock_remedy: mssql_lock_remedy,
    permission_read_advice: mssql_permission_read_advice,
    online_depends_on_the_edition: true,
    capabilities_unknown_message: mssql_capabilities_unknown_message,
    impact_notes: mssql_impact_notes,
};

static POSTGRES: Profile = Profile {
    computed_column_screen: pbps_diff::Screen::Text,
    counts_stranded_rows_at_plan_time: true,
    vets_declared_domain_code: true,
    externals_draft_refusal: None,
    scratch_compiles_base_namespace: false,
    target_identity_unread: crate::resolution::identity_unread,
    scratch_may_share_the_instance: false,
    cost_report_engine: "postgres",
    truncate_reason: pbps_pg::state::truncate_reason,
    revalidates_read_inside_own_transaction: true,
    lock_principal_noun: "role",
    lock_principal_placeholder: "database role",
    lock_remedy: postgres_lock_remedy,
    permission_read_advice: postgres_permission_read_advice,
    online_depends_on_the_edition: false,
    capabilities_unknown_message: postgres_capabilities_unknown_message,
    impact_notes: pbps_pg::impact::notes,
};

fn mssql_identity_unread(error: DbError) -> crate::resolution::Refused {
    crate::resolution::Refused::Unanswerable(anyhow::anyhow!(
        "the target's master family_guid could not be read: {error}"
    ))
}

fn mssql_lock_remedy(
    gap: pbps_db::doctor::LockReadGap,
    how: &str,
    next: &str,
    principal: &str,
) -> LockRemedy {
    use pbps_db::doctor::LockReadGap;
    match gap {
        // Measured on 2025 RTM: without any permission on it the table is
        // invisible, and whether it lacks only `SELECT` cannot be told. The
        // object's own `VIEW DEFINITION` makes it visible and nothing else:
        // the database-wide grant would also show every other schema's
        // objects and module definitions, which the narrower grants of
        // DECISIONS 505 keep hidden (review of #1641).
        LockReadGap::Hidden => LockRemedy {
            why: format!(
                "this user cannot see {table} at all, so what it lacks on it cannot be told \
                 yet; {how}; {next}; the statement fails if {table} does not exist, and then \
                 the database is not initialized as pbps expects",
                table = pbps_mssql::state::LOCK_TABLE
            ),
            statement: Some(format!(
                "GRANT VIEW DEFINITION ON OBJECT::{} TO {principal};",
                pbps_mssql::state::LOCK_TABLE
            )),
        },
        LockReadGap::TableSelect => LockRemedy {
            why: format!(
                "{} exists and this user may not read it; {how}; a DENY through a role this \
                 user is in overrides the grant, and leaves the lock unreadable",
                pbps_mssql::state::LOCK_TABLE
            ),
            statement: Some(format!(
                "GRANT SELECT ON OBJECT::{} TO {principal};",
                pbps_mssql::state::LOCK_TABLE
            )),
        },
        LockReadGap::SchemaUsage | LockReadGap::Unexplained => LockRemedy {
            why: format!(
                "nothing this user can see explains it: check what the login is mapped to in \
                 this database and that {} is intact, then rerun doctor",
                pbps_mssql::state::LOCK_TABLE
            ),
            statement: None,
        },
    }
}

fn postgres_lock_remedy(
    gap: pbps_db::doctor::LockReadGap,
    how: &str,
    next: &str,
    principal: &str,
) -> LockRemedy {
    use pbps_db::doctor::LockReadGap;
    match gap {
        LockReadGap::SchemaUsage => LockRemedy {
            why: format!(
                "this role may not use schema {schema}, which holds the ledger; {how}; {next}",
                schema = pbps_pg::state::LEDGER_SCHEMA
            ),
            statement: Some(format!(
                "GRANT USAGE ON SCHEMA {} TO {principal};",
                pbps_pg::state::LEDGER_SCHEMA
            )),
        },
        LockReadGap::TableSelect => LockRemedy {
            why: format!(
                "{} exists and this role may not read it; {how}",
                pbps_pg::state::LOCK_TABLE
            ),
            statement: Some(format!(
                "GRANT SELECT ON TABLE {} TO {principal};",
                pbps_pg::state::LOCK_TABLE
            )),
        },
        LockReadGap::Hidden | LockReadGap::Unexplained => LockRemedy {
            why: format!(
                "nothing this role can see explains it: check that the connection names the \
                 role that deploys and that {} is intact, then rerun doctor",
                pbps_pg::state::LOCK_TABLE
            ),
            statement: None,
        },
    }
}

fn mssql_permission_read_advice() -> String {
    "check that this user has VIEW DEFINITION in this database and what \
     the login is mapped to, then rerun doctor"
        .to_owned()
}

fn postgres_permission_read_advice() -> String {
    // The read asks `has_*_privilege` and the system catalogs, which
    // every role may read unless someone revoked it; the other way it
    // fails is a connection that is not the role meant to deploy. But
    // once the project has recorded identities (tables, columns or a
    // tombstone), `pbps_pg::doctor::permissions` first reads the last
    // recorded state, and that read needs the ledger's own grants: a
    // role without them was sent to the catalogs, which were fine
    // (#820). Named conditionally, because which read failed is not
    // carried this far, and the table may not exist at all.
    format!(
        "check that the connection names the role that deploys and that it can read the \
         system catalogs (`pg_catalog`); once this project has recorded identities, doctor \
         also reads its last recorded state, which needs USAGE on schema {} and, where that \
         table exists, SELECT on {}",
        pbps_pg::state::LEDGER_SCHEMA,
        pbps_pg::state::STATE_TABLE
    )
}

fn mssql_capabilities_unknown_message(cause: &str) -> String {
    format!(
        "this server's version or edition could not be read ({cause}), so whether it \
         accepts `CREATE OR ALTER` and online index operations is undetermined"
    )
}

fn postgres_capabilities_unknown_message(cause: &str) -> String {
    format!(
        "this PostgreSQL server's version could not be read ({cause}), so doctor cannot \
         report it; what the version decides (`MAINTAIN` grants, changing a generated \
         column's expression) is checked again when a plan is made against this server"
    )
}

fn mssql_impact_notes(report: &ImpactReport) -> Vec<String> {
    if report.advisory.is_empty() {
        Vec::new()
    } else {
        vec![
            "Nothing outside the database is visible here: applications and \
             downstream consumers need a human's checklist."
                .to_owned(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_db::doctor::LockReadGap;

    const DRIVERS: [Driver; 2] = [Driver::Mssql, Driver::Postgres];

    #[test]
    fn the_two_profiles_differ_on_the_facts_that_are_meant_to_differ() {
        let (ms, pg) = (profile(Driver::Mssql), profile(Driver::Postgres));
        assert_ne!(ms.computed_column_screen, pg.computed_column_screen);
        assert_ne!(ms.vets_declared_domain_code, pg.vets_declared_domain_code);
        assert_ne!(
            ms.scratch_compiles_base_namespace,
            pg.scratch_compiles_base_namespace
        );
        assert_ne!(
            ms.online_depends_on_the_edition,
            pg.online_depends_on_the_edition
        );
        assert_ne!(ms.cost_report_engine, pg.cost_report_engine);
        assert_ne!(ms.lock_principal_noun, pg.lock_principal_noun);
    }

    #[test]
    fn each_reader_returns_what_its_profile_field_says_for_both_drivers() {
        for driver in DRIVERS {
            let facts = profile(driver);
            assert_eq!(computed_column_screen(driver), facts.computed_column_screen);
            assert_eq!(
                vets_declared_domain_code(driver),
                facts.vets_declared_domain_code
            );
            assert_eq!(
                scratch_compiles_base_namespace(driver),
                facts.scratch_compiles_base_namespace
            );
            assert_eq!(
                online_depends_on_the_edition(driver),
                facts.online_depends_on_the_edition
            );
            assert_eq!(
                needs_readback_revalidation(driver, Read::InsideOwnTransaction),
                facts.revalidates_read_inside_own_transaction
            );
            assert_eq!(
                permission_read_advice(driver),
                (facts.permission_read_advice)()
            );
            assert_eq!(
                capabilities_unknown_message(driver, "boom"),
                (facts.capabilities_unknown_message)("boom")
            );
            assert_eq!(
                truncate_reason(driver, "short"),
                (facts.truncate_reason)("short")
            );
            assert_eq!(
                refuse_externals_draft_unsupported(driver)
                    .err()
                    .map(|e| e.to_string()),
                facts.externals_draft_refusal.map(str::to_owned)
            );
        }
    }

    #[test]
    fn a_read_outside_the_plans_own_transaction_is_never_revalidated() {
        for driver in DRIVERS {
            assert!(!needs_readback_revalidation(driver, Read::Snapshot));
        }
    }

    #[test]
    fn an_engine_without_enums_counts_no_rows_for_an_enum_retype() {
        let nothing = ChangeSet::default();
        for driver in DRIVERS {
            assert!(!counts_rows_an_enum_retype_strands(driver, &nothing));
        }
        assert!(!profile(Driver::Mssql).counts_stranded_rows_at_plan_time);
    }

    #[test]
    fn a_lock_remedy_names_its_engines_principal_and_grants_only_what_was_seen() {
        for driver in DRIVERS {
            let facts = profile(driver);
            let seen = lock_remedy(driver, Some(LockReadGap::TableSelect));
            assert!(
                seen.why.contains(facts.lock_principal_noun),
                "the remedy speaks of the engine's own principal: {}",
                seen.why
            );
            assert!(seen.statement.is_some());
            // Nothing seen never becomes a grant, whichever engine is asked.
            for gap in [None, Some(LockReadGap::Unexplained)] {
                assert_eq!(lock_remedy(driver, gap).statement, None);
            }
        }
    }

    #[test]
    fn only_the_engine_that_has_a_binding_adapter_drafts_externals() {
        assert!(refuse_externals_draft_unsupported(Driver::Postgres).is_ok());
        let refused = refuse_externals_draft_unsupported(Driver::Mssql).unwrap_err();
        assert!(refused.to_string().contains("SQL Server"), "{refused}");
    }
}
