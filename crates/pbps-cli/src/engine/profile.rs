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
//! The readers keep their names and signatures in [`super`], and their doc
//! comments say what the fact means; the comments here say why the engines
//! differ. A fact only one session reads is not a field: it is that session's
//! own answer, in the engine's own body.
//!
//! The connected operations are not here: they are the methods of
//! `pbps_db::engine::Engine` ([`super::of`]), which cannot carry these facts
//! because several of them name types of `pbps-diff` and this crate, which
//! `pbps-db` cannot.

use super::*;

/// A command or flag feature whose availability is a static fact about the
/// engine, checked through [`Profile`] before demanding environment variables,
/// project files or database connections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandFeature {
    Resolver,
    TargetAssertions,
    DevPreview,
    Roles,
    Extensions,
    TextSearch,
    ExclusionConstraints,
    Partitions,
    DeclaredTypes,
    ManagedData,
    TableRebuild,
    DatabaseCreate,
}

/// One engine's answers to the questions that need no connection.
///
/// A plain value where the fact is a constant, and a `fn` pointer only where
/// the fact needs an argument to answer.
pub(crate) struct Profile {
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
    /// The name an unavailable cost report gives this engine.
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
    /// What `doctor` calls the subject whose permissions are checked
    /// (`"account"` for server engines, `"process"` for SQLite).
    pub permission_subject: &'static str,
    /// What `doctor` advises checking when an environment cannot be reached.
    pub unreachable_remedy: &'static str,
    /// What `doctor` labels the engine's version line (`"server"` or
    /// `"library"`).
    pub version_label: &'static str,
    /// Read by [`online_depends_on_the_edition`].
    pub online_depends_on_the_edition: bool,
    /// Read by [`capabilities_unknown_message`], which passes the cause.
    pub capabilities_unknown_message: fn(cause: &str) -> String,
    /// Read by [`impact_notes`].
    pub impact_notes: fn(&ImpactReport) -> Vec<String>,
    /// What `pbps test` needs of an engine that owns its scratch server, or
    /// `None` where this engine has none (read by [`require_owned_test`]).
    pub owned_test: Option<&'static OwnedTest>,
    /// Why a command or flag feature is refused early for this engine, or
    /// `None` when supported (read by [`require_command_feature`]).
    pub unsupported_command: fn(CommandFeature) -> Option<&'static str>,
    /// Finding id attached to `ServerLimits::edition.warnings` in `plan --db`.
    pub preflight_warning_id: &'static str,
    /// Whether an empty `apply` plan still connects under the lock because
    /// identity-only or cluster-scoped state outside `plan.changes` may need
    /// checking (PostgreSQL cluster roles, types, extensions, search).
    pub checks_empty_plan_under_lock: bool,
    /// Whether the target's `--db` string is a local file path (carrying no
    /// secret and shown as given) rather than a server connection string.
    pub connection_is_path: bool,
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
        Driver::Sqlite => &SQLITE,
    }
}

static MSSQL: Profile = Profile {
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
    permission_subject: "account",
    unreachable_remedy: "check the variable named by `url_env:`, the host, and the firewall",
    version_label: "server",
    online_depends_on_the_edition: true,
    capabilities_unknown_message: mssql_capabilities_unknown_message,
    impact_notes: mssql_impact_notes,
    owned_test: Some(&owned_test::MSSQL),
    unsupported_command: mssql_unsupported_command,
    preflight_warning_id: "plan.edition",
    checks_empty_plan_under_lock: false,
    connection_is_path: false,
};

static POSTGRES: Profile = Profile {
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
    permission_subject: "account",
    unreachable_remedy: "check the variable named by `url_env:`, the host, and the firewall",
    version_label: "server",
    online_depends_on_the_edition: false,
    capabilities_unknown_message: postgres_capabilities_unknown_message,
    impact_notes: pbps_pg::impact::notes,
    owned_test: Some(&owned_test::POSTGRES),
    unsupported_command: postgres_unsupported_command,
    preflight_warning_id: "plan.edition",
    checks_empty_plan_under_lock: true,
    connection_is_path: false,
};

static SQLITE: Profile = Profile {
    externals_draft_refusal: Some(
        "`pbps resolver externals` reads a PostgreSQL target; SQLite has no resolver or binding \
         adapter",
    ),
    scratch_compiles_base_namespace: false,
    target_identity_unread: sqlite_identity_unread,
    scratch_may_share_the_instance: false,
    cost_report_engine: "sqlite",
    truncate_reason: pbps_sqlite::state::truncate_reason,
    revalidates_read_inside_own_transaction: false,
    // SQLite has no database principals or GRANTs; the operating-system
    // process and file permissions control access.
    lock_principal_noun: "process",
    lock_principal_placeholder: "operating-system user",
    lock_remedy: sqlite_lock_remedy,
    permission_read_advice: sqlite_permission_read_advice,
    permission_subject: "process",
    unreachable_remedy: "check the path in `url_env:` (or `--db`), that the file exists, and file and directory permissions",
    version_label: "library",
    online_depends_on_the_edition: false,
    capabilities_unknown_message: sqlite_capabilities_unknown_message,
    impact_notes: sqlite_impact_notes,
    owned_test: None,
    unsupported_command: sqlite_unsupported_command,
    preflight_warning_id: "plan.version-floor",
    checks_empty_plan_under_lock: false,
    connection_is_path: true,
};

fn mssql_unsupported_command(feature: CommandFeature) -> Option<&'static str> {
    (feature == CommandFeature::DatabaseCreate).then_some(
        "SQL Server does not support `--create`: a server's database is created by its administrator",
    )
}

fn postgres_unsupported_command(feature: CommandFeature) -> Option<&'static str> {
    (feature == CommandFeature::DatabaseCreate).then_some(
        "PostgreSQL does not support `--create`: a server's database is created by its administrator",
    )
}

fn sqlite_unsupported_command(feature: CommandFeature) -> Option<&'static str> {
    match feature {
        CommandFeature::DatabaseCreate => None,
        CommandFeature::Resolver => {
            Some("SQLite has no resolver; remove `--resolve-with` and `resolve_with:`")
        }
        CommandFeature::TargetAssertions => Some("SQLite does not support plan target assertions"),
        CommandFeature::DevPreview => Some(
            "SQLite does not support `--dev` preview rehearsal; use `pbps plan --db` to rehearse \
             on an in-process copy, or remove `--dev` and `dev:`",
        ),
        CommandFeature::Roles => {
            Some("SQLite has no database roles, so role rename and drop commands do not apply")
        }
        CommandFeature::Extensions => {
            Some("SQLite has no extensions, so extension commands and flags do not apply")
        }
        CommandFeature::TextSearch => Some(
            "SQLite has no text search configurations or dictionaries, so text search commands \
             do not apply",
        ),
        CommandFeature::ExclusionConstraints => {
            Some("SQLite has no exclusion constraints, so `drop-exclusion` does not apply")
        }
        CommandFeature::Partitions => {
            Some("SQLite has no partitioned tables, so partition commands and flags do not apply")
        }
        CommandFeature::DeclaredTypes => {
            Some("SQLite has no declared enum or domain types, so type intents do not apply")
        }
        CommandFeature::ManagedData => {
            Some("SQLite does not support managed reference data (`--data`) yet")
        }
        CommandFeature::TableRebuild => Some(
            "SQLite has no partitioned tables, so `rebuild-table` (partitioned-table rewrite) \
             does not apply",
        ),
    }
}

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

fn sqlite_identity_unread(error: DbError) -> crate::resolution::Refused {
    crate::resolution::Refused::Unanswerable(anyhow::anyhow!(
        "SQLite has no resolver target identity to read: {error}"
    ))
}

fn sqlite_lock_remedy(
    gap: pbps_db::doctor::LockReadGap,
    _how: &str,
    _next: &str,
    _principal: &str,
) -> LockRemedy {
    // SQLite has no database principals or SQL GRANTs, and `LockReadGap` has
    // no file-permission or busy-file variant. Never fabricate a GRANT
    // statement: point the operator at the database file, its permissions and
    // the ledger lock table instead.
    let _ = gap;
    LockRemedy {
        why: format!(
            "SQLite has no database grants; check that this process can read the database file \
             and its directory, that no other writer holds it busy, and that {} is intact, then \
             rerun doctor",
            pbps_sqlite::state::LOCK_TABLE
        ),
        statement: None,
    }
}

fn sqlite_permission_read_advice() -> String {
    format!(
        "check that the database file and its directory are readable and writable by this \
         process, that the file is not held busy by another writer, and that {} is intact, then \
         rerun doctor",
        pbps_sqlite::state::STATE_TABLE
    )
}

fn sqlite_capabilities_unknown_message(cause: &str) -> String {
    format!(
        "this SQLite library's version could not be read ({cause}), so doctor cannot report it; \
         connection settings, library support and file writability are checked again before a \
         plan is applied"
    )
}

fn sqlite_impact_notes(report: &ImpactReport) -> Vec<String> {
    let mut out = Vec::new();
    if !report.carried.is_empty() {
        out.push(
            "SQLite rewrites those references to the new name when `legacy_alter_table` is off."
                .to_owned(),
        );
    }
    if !report.advisory.is_empty() || !report.carried.is_empty() {
        out.push(
            "Nothing outside the database file is visible here: applications and downstream \
             consumers need a human's checklist."
                .to_owned(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_db::doctor::LockReadGap;

    #[test]
    fn the_two_profiles_differ_on_the_facts_that_are_meant_to_differ() {
        let (ms, pg) = (profile(Driver::Mssql), profile(Driver::Postgres));
        assert_ne!(
            pbps_mssql::Mssql.computed_column_screen(),
            pbps_pg::Postgres::new().computed_column_screen()
        );
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
        // Network-server engines only; SQLite's in-process profile is pinned below.
        for driver in crate::engine::server_drivers() {
            let facts = profile(driver);
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
            assert_eq!(permission_subject(driver), facts.permission_subject);
            assert_eq!(unreachable_remedy(driver), facts.unreachable_remedy);
            assert_eq!(version_label(driver), facts.version_label);
            assert_eq!(has_resolver(driver), facts.owned_test.is_some());
            assert!(has_resolver(driver));
            assert_eq!(preflight_warning_id(driver), "plan.edition");
            assert!(!connection_is_path(driver));
            assert_eq!(
                checks_empty_plan_under_lock(driver),
                driver == Driver::Postgres
            );
            assert!(
                require_command_feature(driver, CommandFeature::DatabaseCreate)
                    .unwrap_err()
                    .to_string()
                    .contains(of(driver).name())
            );
            assert!(require_command_feature(driver, CommandFeature::Resolver).is_ok());
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
        for driver in crate::engine::all_drivers() {
            assert!(!needs_readback_revalidation(driver, Read::Snapshot));
        }
    }

    #[test]
    fn a_lock_remedy_names_its_engines_principal_and_grants_only_what_was_seen() {
        // Network-server engines emit GRANT statements; SQLite has no database grants
        // and is pinned in `sqlite_profile_readers_return_its_own_facts_and_never_fabricate_a_grant`.
        for driver in crate::engine::server_drivers() {
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
        let refused = refuse_externals_draft_unsupported(Driver::Sqlite).unwrap_err();
        assert!(refused.to_string().contains("SQLite"), "{refused}");
    }

    #[test]
    fn sqlite_profile_readers_return_its_own_facts_and_never_fabricate_a_grant() {
        let facts = profile(Driver::Sqlite);
        assert_eq!(facts.cost_report_engine, "sqlite");
        assert!(!scratch_compiles_base_namespace(Driver::Sqlite));
        assert!(!online_depends_on_the_edition(Driver::Sqlite));
        assert!(!needs_readback_revalidation(
            Driver::Sqlite,
            Read::InsideOwnTransaction
        ));
        assert!(!needs_readback_revalidation(Driver::Sqlite, Read::Snapshot));
        assert_eq!(
            permission_read_advice(Driver::Sqlite),
            (facts.permission_read_advice)()
        );
        assert_eq!(permission_subject(Driver::Sqlite), "process");
        assert_eq!(version_label(Driver::Sqlite), "library");
        assert!(!has_resolver(Driver::Sqlite));
        assert_eq!(preflight_warning_id(Driver::Sqlite), "plan.version-floor");
        assert!(!checks_empty_plan_under_lock(Driver::Sqlite));
        assert!(connection_is_path(Driver::Sqlite));
        assert!(require_command_feature(Driver::Sqlite, CommandFeature::DatabaseCreate).is_ok());
        for feature in [
            CommandFeature::Resolver,
            CommandFeature::TargetAssertions,
            CommandFeature::DevPreview,
            CommandFeature::Roles,
            CommandFeature::Extensions,
            CommandFeature::TextSearch,
            CommandFeature::ExclusionConstraints,
            CommandFeature::Partitions,
            CommandFeature::DeclaredTypes,
            CommandFeature::ManagedData,
            CommandFeature::TableRebuild,
        ] {
            let err = require_command_feature(Driver::Sqlite, feature).unwrap_err();
            assert!(err.to_string().contains("SQLite"), "{feature:?}: {err}");
        }
        assert!(
            unreachable_remedy(Driver::Sqlite).contains("path")
                && !unreachable_remedy(Driver::Sqlite).contains("firewall")
        );
        assert!(permission_read_advice(Driver::Sqlite).contains(pbps_sqlite::state::STATE_TABLE));
        assert_eq!(
            capabilities_unknown_message(Driver::Sqlite, "boom"),
            (facts.capabilities_unknown_message)("boom")
        );
        assert!(capabilities_unknown_message(Driver::Sqlite, "boom").contains("SQLite"));
        // Character-based truncation matches `pbps_sqlite::state::truncate_reason`.
        let long = "é".repeat(pbps_sqlite::state::REASON_CHARS + 5);
        assert_eq!(
            truncate_reason(Driver::Sqlite, &long).chars().count(),
            pbps_sqlite::state::REASON_CHARS
        );
        // SQLite has no database grants, so every LockReadGap produces no SQL statement.
        for gap in [
            None,
            Some(LockReadGap::Hidden),
            Some(LockReadGap::TableSelect),
            Some(LockReadGap::SchemaUsage),
            Some(LockReadGap::Unexplained),
        ] {
            let remedy = lock_remedy(Driver::Sqlite, gap);
            assert_eq!(remedy.statement, None, "{gap:?}");
            assert!(
                remedy.why.contains("SQLite")
                    && remedy.why.contains(pbps_sqlite::state::LOCK_TABLE),
                "{}",
                remedy.why
            );
        }
        // Negative: SQLite owns no test scratch container and refuses by name.
        let owned = require_owned_test(Driver::Sqlite)
            .err()
            .expect("SQLite owns no test scratch");
        assert!(owned.to_string().contains("SQLite"), "{owned}");
        // Negative: an empty impact report produces no note, while a carried or
        // advisory referrer produces SQLite's notes.
        assert!(impact_notes(Driver::Sqlite, &ImpactReport::default()).is_empty());
        let with_carried = ImpactReport {
            target: "main.t".into(),
            carried: vec![pbps_db::impact::Referrer {
                kind: "view".into(),
                name: "main.v".into(),
                detail: None,
                removed_with: None,
            }],
            ..Default::default()
        };
        let notes = impact_notes(Driver::Sqlite, &with_carried);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("legacy_alter_table"), "{notes:?}");
        // Target identity read failure names SQLite as unanswerable.
        match target_identity_unread(Driver::Sqlite, DbError::Refused("no resolver".into())) {
            crate::resolution::Refused::Unanswerable(err) => {
                assert!(err.to_string().contains("SQLite"), "{err}");
            }
            crate::resolution::Refused::Finding(f) => {
                panic!("expected Unanswerable, got finding: {f:?}");
            }
        }
    }
}
