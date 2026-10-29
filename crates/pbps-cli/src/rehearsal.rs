//! The convergence rehearsal (SPEC §9.3.1): build a base state on a scratch
//! engine, run the plan's statements in order, read the engine back, and
//! compare it with the declarations.
//!
//! # Why this is not in `dev`
//!
//! `dev` decides where the throwaway engine comes from and owns its lifecycle,
//! which is `plan --dev`'s business. What happens once a connection exists is
//! engine-neutral and is the same question wherever the base state comes from,
//! so it lives here and takes the base as an input ([`Base`]). The comparison
//! is the ordinary differ followed by [`classify`], so a rehearsal cannot
//! disagree with `plan` about what a difference is.

pub mod connected;

use anyhow::Context as _;

use pbps_db::Conn;
use pbps_dialect::{Dialect, Statement};
use pbps_model::role_mapping::RoleMapping;
use pbps_model::{Change, ChangeSet, IdsFile, Schema, SessionZone};

/// The state the plan's statements run on top of.
pub enum Base<'a> {
    /// The git baseline, lowered from empty: what `plan --dev` previews.
    Baseline {
        statements: &'a [Statement],
        /// The identity mapping the baseline was built under, which names the
        /// objects the scratch database holds before the plan runs.
        ids: &'a IdsFile,
    },
    /// The target's current managed model and declared rows, lowered from
    /// empty by the same emitter: the base of a connected plan (#2379).
    Target {
        statements: &'a [Statement],
        /// The identity mapping the plan was computed against on its base
        /// side, which names what the scratch holds before the plan runs.
        ids: &'a IdsFile,
        /// The session the target's statements ran in, and so the scratch's.
        execution: Execution<'a>,
        /// The run's own names for the roles the plan and the base name,
        /// when the statements were lowered with them. `None` is a scratch
        /// that cannot hold roles: authorization is then neither run nor
        /// compared (see [`is_authorization`]).
        roles: Option<&'a RoleMapping>,
    },
}

/// How the statements are sent. `--dev` sends each as it is; a connected
/// rehearsal sends them as `apply` does, so a statement that only fails
/// inside the apply's transaction fails here too.
#[derive(Clone, Copy)]
pub struct Execution<'a> {
    /// The target's session time zone, which every offset-less value the
    /// statements write is read in (DEC-1916.1).
    pub zone: &'a SessionZone,
    /// Whether the plan's statements run in one transaction, as a staged
    /// plan's do not. The base always does: it is a bootstrap.
    pub transactional: bool,
}

impl<'a> Base<'a> {
    fn statements(&self) -> &'a [Statement] {
        match self {
            Base::Baseline { statements, .. } | Base::Target { statements, .. } => statements,
        }
    }

    fn ids(&self) -> &'a IdsFile {
        match self {
            Base::Baseline { ids, .. } | Base::Target { ids, .. } => ids,
        }
    }

    /// Whether the engine stores declared text in a spelling of its own, so
    /// that the text of an object cannot decide whether it converged.
    fn respells_text(&self) -> bool {
        matches!(self, Base::Target { .. })
    }

    /// Whether roles are part of what the scratch is compared on.
    fn compares_roles(&self) -> bool {
        match self {
            Base::Baseline { .. } => true,
            Base::Target { roles, .. } => roles.is_some(),
        }
    }

    /// The names the declarations' roles have on the scratch, when they are
    /// not their own.
    fn role_mapping(&self) -> Option<&'a RoleMapping> {
        match self {
            Base::Baseline { .. } => None,
            Base::Target { roles, .. } => *roles,
        }
    }

    fn execution(&self) -> Option<Execution<'a>> {
        match self {
            Base::Baseline { .. } => None,
            Base::Target { execution, .. } => Some(*execution),
        }
    }

    /// Whether the declared rows still have to be asked of the engine. A
    /// connected plan asked its target before it was computed, and its
    /// declarations are already in the engine's own spelling.
    fn asks_spelling(&self) -> bool {
        matches!(self, Base::Baseline { .. })
    }

    fn words(&self) -> Words {
        match self {
            Base::Baseline { .. } => Words {
                database: "the dev database",
                base: "baseline",
                broken: "The baseline itself does not compile, so nothing can be said about the plan.",
                read: "cannot read the dev database back",
                rows: "cannot read the declared rows back from the dev database",
            },
            Base::Target { .. } => Words {
                database: "the rehearsal scratch",
                base: "target's current state",
                broken: "The target's current managed state does not compile on the scratch, so \
                         nothing can be said about the plan.",
                read: "cannot read the rehearsal scratch back",
                rows: "cannot read the declared rows back from the rehearsal scratch",
            },
        }
    }
}

/// What the messages call the base and the engine it ran on.
struct Words {
    database: &'static str,
    base: &'static str,
    broken: &'static str,
    read: &'static str,
    rows: &'static str,
}

/// Changes about who may do what: the cluster's principals, the privileges
/// they hold, and the row-security policies that name them. A rehearsal runs
/// them under the run's own role names (`RoleMapping`); where the scratch
/// account may not create those roles they are left out of both the base and
/// the plan, and out of the comparison (SPEC §9.3.1), and the record says so:
/// a grant or a policy naming a role the target holds and the scratch does not
/// fails for a reason that says nothing about the plan.
pub fn is_authorization(change: &Change) -> bool {
    matches!(
        change,
        Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::CreatePolicy { .. }
            | Change::DropPolicy { .. }
            | Change::SetRowSecurity { .. }
    )
}

/// Every statement of `lowered`, in order: a plan lowered with the run's own
/// role names has nothing to leave out.
pub fn all_statements(lowered: &crate::emission::LoweredPlan) -> Vec<Statement> {
    lowered
        .groups()
        .iter()
        .flat_map(|group| group.statements().iter().cloned())
        .collect()
}

/// The statements of `lowered` that belong to a change about the database's
/// own objects, in order. `operations` are the ordered plan's, which `lowered`
/// was lowered from: a group names its operation, and an operation names the
/// change it performs.
pub fn local_statements(
    lowered: &crate::emission::LoweredPlan,
    operations: &pbps_model::operation::Operations,
) -> Vec<Statement> {
    let authorization: std::collections::BTreeSet<_> = operations
        .iter()
        .filter(|operation| is_authorization(operation.change()))
        .map(pbps_model::operation::Operation::id)
        .collect();
    lowered
        .groups()
        .iter()
        .filter(|group| !authorization.contains(&group.operation()))
        .flat_map(|group| group.statements().iter().cloned())
        .collect()
}

/// The statements that build `schema` from nothing, as `bootstrap` writes
/// them, with the changes they were lowered from.
///
/// Reusing the differ rather than a second code path is what keeps a
/// rehearsal a rehearsal of the real thing. The scheduler runs as bootstrap
/// runs it (#1955): whichever passes an engine has for a plan built from
/// nothing, the base gets.
pub fn lower_from_empty<D: crate::emission::Engine>(
    selected: &D,
    schema: &Schema,
    ids: &IdsFile,
    hints: &pbps_model::Hints,
) -> anyhow::Result<(
    crate::emission::LoweredPlan,
    pbps_model::operation::Operations,
)> {
    let dialect = selected.as_ref();
    // A type whose identity is recorded and whose declaration the base lacks
    // is a type the target does not hold yet (a declared type is recorded when
    // it is planned, not when it exists), and an empty apply may not claim an
    // identity it does not hold.
    let absent: Vec<_> = ids
        .types
        .values()
        .filter(|name| !schema.types.contains_key(*name))
        .cloned()
        .collect();
    let ids = &ids.without_types(&absent);
    let build = pbps_diff::diff(
        pbps_diff::Side {
            schema: &Schema::default(),
            ids: &IdsFile::default(),
        },
        pbps_diff::Side { schema, ids },
        dialect,
        // The base's own hints, from the revision it came from. The
        // strategies in them are irrelevant here (nothing has rows yet), but
        // `depends_on:` is the edge the identifier scan could not find — drop it
        // and the rehearsal fails while compiling a base that was always
        // valid, before it has said anything about the plan.
        hints,
    )
    .map_err(|errs| {
        anyhow::anyhow!(
            "the baseline cannot be expressed: {}",
            errs.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )
    })?;
    // SQL Server has no passes of its own today.
    let build = crate::dependents::schedule::schedule(
        build,
        &crate::dependents::schedule::Declarations {
            schema,
            sources: crate::dependents::Sources {
                before: pbps_diff::Side {
                    schema: &Schema::default(),
                    ids: &IdsFile::default(),
                },
                after: ids,
            },
            module_deps: &hints.module_deps,
        },
        &crate::dependents::schedule::Facts::default(),
        dialect,
    )?
    .changes;
    let ordered = build.ordered(pbps_model::PlanMode::Transactional, dialect);
    let lowered = crate::statements(&ordered, selected)?;
    Ok((lowered, pbps_dialect::PlanPayload::operations(&ordered)))
}

/// What a rehearsal found.
pub struct Findings {
    /// Statements used to build the baseline, then to apply the plan.
    pub built: usize,
    pub applied: usize,

    /// Differences between the desired state and what the engine actually
    /// holds, split by what they mean.
    ///
    /// A **structural** difference is a bug: the plan does not converge, and
    /// applying it for real would leave the database not matching the
    /// declarations. A **spelling** difference is the engine's normalization
    /// showing through — the declaration says `amount > 0` and the catalog
    /// stores `([amount]>(0))` — which costs one rebuilt constraint per apply
    /// and, until the declaration is rewritten, one line in every drift report.
    pub structural: Vec<String>,
    pub spelling: Vec<String>,
    pub externals_staged: bool,
}

impl Findings {
    pub fn converged(&self) -> bool {
        self.structural.is_empty()
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn rehearse_in(
    conn: &mut Conn,
    base: Base<'_>,
    plan: &[Statement],
    declared: &Schema,
    declared_ids: &IdsFile,
    dialect: &dyn Dialect,
    hints: &pbps_model::Hints,
) -> anyhow::Result<Findings> {
    // The rehearsal reads the rows back and compares; a spelling the engine
    // reads differently would fail that comparison without saying which
    // spelling to write. Asked first, as every connected command asks
    // (DECISIONS 101).
    // Under the names the rehearsal database has: the base was built from the
    // *previous* revision, so a table or key column this plan renames is still
    // spelt the old way here too (DECISIONS 148).
    let declared = if base.asks_spelling() {
        let mut unasked = Vec::new();
        let refused = crate::deploy::refuse_misspelt(
            conn,
            declared,
            &crate::deploy::catalogued_as(declared, declared_ids, base.ids()),
            &mut unasked,
            false, // SQL Server-only; PostgreSQL is refused before connecting.
        )
        .await;
        // Before the refusal, beside it, as the plan prints them (DEC-1726.1).
        crate::deploy::warn_unasked(&unasked);
        // What the rows are compared against: a date or time as the engine reads
        // it, which is what the plan's statements stored (DEC-1883.1).
        refused?
    } else {
        declared.clone()
    };
    let (declared, declared_ids) = compared_declarations(&base, declared, declared_ids);
    let (declared, declared_ids) = (&declared, &declared_ids);
    let words = base.words();

    let build = base.statements();
    run_statements(
        conn,
        build,
        base.execution().map(|e| (dialect, e.zone, true)),
        |i, sql, e| {
            Rejected::of(format!(
                "{} rejected statement {} of the {}:\n{}\n\n{e}\n\n{}",
                words.database,
                i + 1,
                words.base,
                sql,
                words.broken
            ))
        },
    )
    .await?;
    run_statements(
        conn,
        plan,
        base.execution().map(|e| (dialect, e.zone, e.transactional)),
        |i, sql, e| {
            Rejected::of(format!(
                "{} rejected statement {} of the plan:\n{}\n\n{e}\n\n\
                 This plan would fail at apply time; nothing was changed anywhere real.",
                words.database,
                i + 1,
                sql
            ))
        },
    )
    .await?;

    let built = crate::engine::introspect(conn, crate::engine::Read::Snapshot)
        .await
        .context(words.read)?;
    let modules: std::collections::BTreeSet<_> = declared.modules.keys().cloned().collect();
    let scoped = pbps_diff::scope(
        &built.schema,
        declared_ids,
        &modules,
        &pbps_diff::external_parents(declared),
    );

    // The engine side is identified by what the engine actually holds, not by
    // the identity file the declarations carry. Handing both sides the same
    // mapping would make the differ match every uid on both sides and then skip
    // any object the engine is missing — so a plan that failed to create a
    // declared table or column, which is the defect this rehearsal exists to
    // catch, would come back as convergence (the same reason drift needs
    // observed identity, SPEC 8.2).
    let observed = pbps_diff::observed_ids(&scoped.schema, declared_ids);

    // What is left after applying the plan is what has not converged. The
    // comparison is the ordinary one, so the rehearsal cannot disagree with the
    // differ about what a difference is.
    //
    // Rows included, read back under the declared scope (ADR-0004): the DML
    // ran, and whether what it wrote is what was declared — in the engine's
    // own spelling — is exactly what a rehearsal can answer that no offline
    // comparison can.
    let declared_data = declared.data_scopes();
    let read = declared_data
        .iter()
        .map(|(n, s)| (n.clone(), s.rows_to_read()))
        .collect();
    let rows = crate::engine::read_rows(conn, &scoped.schema, &read, crate::engine::Read::Snapshot)
        .await
        .context(words.rows)?;
    let mut engine = scoped
        .schema
        .with_observed_rows(&rows, &declared_data, declared)?;
    if !base.compares_roles() {
        engine.roles.clear();
        clear_row_security(&mut engine);
    }
    // As `plan --db` compares it: a collation equal to this database's
    // default reads back as none (#1175).
    let declared_here =
        crate::deploy::declarations_as_compared(declared, built.database_collation.as_deref());
    let compare = |engine: &Schema| {
        pbps_diff::diff(
            pbps_diff::Side {
                schema: engine,
                ids: &observed,
            },
            pbps_diff::Side {
                schema: &declared_here,
                ids: declared_ids,
            },
            dialect,
            hints,
        )
        .map_err(|errs| {
            anyhow::anyhow!(
                "the rehearsal produced inexpressible differences: {}",
                errs.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })
    };
    // Compared as read, the engine's deparsed text differs in nearly every
    // declaration, and a domain's differs where the differ cannot say so as a
    // change at all. That comparison is only the source of the respellings, so
    // a refusal of it is not the rehearsal's: the comparison that decides
    // follows, and still refuses.
    let (structural, spelling) = if base.respells_text() {
        let remaining = compare(&engine).ok();
        // PostgreSQL stores a check, a default, an index predicate and a
        // module body as its parser deparses them, never as written, so a
        // textual comparison finds a difference in nearly every declaration.
        // What is structural is what remains once the declared text stands
        // in for the stored one, as `plan --db` compares against the
        // ledger's declared forms (ADR-0009 §2.2); the respelling itself is
        // what the report prints (DEC-2379.2).
        let declared_forms = pbps_model::Declared::from_schema(&declared_here).overlay(&engine);
        let left = compare(&declared_forms)?;
        let structural = classify(&left, &declared_forms).0;
        let mut spelling = remaining
            .as_ref()
            .map(|remaining| respelt(remaining, &engine))
            .unwrap_or_default();
        spelling.extend(domain_respellings(&declared_here, &engine));
        (structural, spelling)
    } else {
        let remaining = compare(&engine)?;
        classify(&remaining, &engine)
    };
    Ok(Findings {
        built: build.len(),
        applied: plan.len(),
        structural,
        spelling,
        externals_staged: false,
    })
}

/// The declarations as the scratch is compared with them. They are compared
/// under the names the scratch's roles have: the run's own, where the plan was
/// lowered with them. A scratch that cannot hold roles holds no authorization
/// (see [`is_authorization`]), so neither side of the comparison does: a role
/// the declarations name is not a difference the plan failed to make.
fn compared_declarations(
    base: &Base<'_>,
    mut declared: Schema,
    ids: &IdsFile,
) -> (Schema, IdsFile) {
    if let Some(mapping) = base.role_mapping() {
        (mapping.schema(&declared), mapping.ids(ids))
    } else if base.compares_roles() {
        (declared, ids.clone())
    } else {
        declared.roles.clear();
        clear_row_security(&mut declared);
        let mut ids = ids.clone();
        ids.roles.clear();
        (declared, ids)
    }
}

/// The engine refused one of the statements. Kept apart from the failures
/// around it (a connection lost, a catalog unreadable), which say nothing
/// about the plan: only this one refuses a plan for its own sake.
#[derive(Debug)]
pub struct Rejected(String);

impl Rejected {
    fn of(message: String) -> anyhow::Error {
        anyhow::Error::new(Self(message))
    }

    #[cfg(test)]
    pub(crate) fn of_test(message: &str) -> Self {
        Self(message.to_owned())
    }
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Rejected {}

/// Sends `statements` in order. `framed` is the connected rehearsal's way:
/// the dialect's session pins, then, when `transactional`, one transaction as
/// `apply` opens it. `--dev` sends each statement as it is.
async fn run_statements(
    conn: &mut Conn,
    statements: &[Statement],
    framed: Option<(&dyn Dialect, &SessionZone, bool)>,
    failed: impl Fn(usize, &str, &pbps_db::DbError) -> anyhow::Error,
) -> anyhow::Result<()> {
    let framing = framed.and_then(|(dialect, zone, transactional)| {
        transactional.then(|| dialect.transaction_framing(zone))
    });
    if framing.is_none() && statements.iter().any(|s| s.drop_guard.is_some()) {
        return Err(Rejected::of("guarded drops require a transactional rehearsal; this unframed consumer cannot skip the guard".into()));
    }
    if let Some((dialect, zone, _)) = framed
        && let Some(pins) = dialect.session_pins(zone)
    {
        conn.execute(&pins)
            .await
            .context("cannot pin the rehearsal session")?;
    }
    if let Some(framing) = &framing {
        conn.begin(framing)
            .await
            .context("cannot open the rehearsal's transaction")?;
    }
    for (i, statement) in statements.iter().enumerate() {
        let executed = match crate::engine::check_drop_requirement(conn, statement).await {
            Ok(()) => conn.execute(&statement.sql).await,
            Err(error) => Err(error),
        };
        if let Err(e) = executed {
            if let Some(framing) = &framing {
                // Its own error must not replace the statement's.
                let _ = conn.rollback(framing).await;
            }
            return Err(failed(i, &statement.sql, &e));
        }
    }
    if let Some(framing) = &framing {
        conn.commit(framing)
            .await
            .context("the rehearsal's transaction could not be committed")?;
    }
    Ok(())
}

/// A schema without the row security its tables declare: see
/// [`is_authorization`].
fn clear_row_security(schema: &mut Schema) {
    for table in schema.tables.values_mut() {
        table.row_security = Default::default();
        table.row_policies.clear();
    }
}

#[cfg(test)]
mod drop_requirement_tests {
    use super::*;

    #[test]
    #[ignore = "needs live PostgreSQL 16 and 18"]
    fn guarded_rehearsals_execute_or_refuse_the_guard_on_both_postgres_majors() {
        crate::db::runtime().unwrap().block_on(async {
            for (major, variable) in [(18, "PBPS_TEST_PG_DB"), (16, "PBPS_TEST_PG_OLD_DB")] {
                let server = std::env::var(variable).unwrap();
                let name = format!("pbps_require_rehearsal2400_{major}_{}", std::process::id());
                let mut admin = Conn::connect(pbps_db::Driver::Postgres, &server).await.unwrap();
                admin.execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).await.unwrap();
                admin.execute(&format!("CREATE DATABASE {name}")).await.unwrap();
                let mut conn = Conn::connect(pbps_db::Driver::Postgres, &format!("{server} dbname={name}")).await.unwrap();
                conn.execute("CREATE SCHEMA app; CREATE TABLE app.t (amount integer); INSERT INTO app.t VALUES (0)").await.unwrap();
                let mut statement = Statement::new("ALTER TABLE app.t DROP COLUMN amount");
                statement.drop_guard = Some(pbps_model::DropGuard {
                    table_uid: "t_000001".parse().unwrap(), table: "app.t".parse().unwrap(), detach_from: None,
                    requirements: [("c_000001".parse().unwrap(), pbps_model::DropRequirement { table_uid: "t_000001".parse().unwrap(), predicate: "amount = 1".into(), required_at: "2026-10-28".into(), operator: "reviewer".into() })].into(),
                });
                let statements = [statement];
                let failed = |_: usize, _: &str, error: &pbps_db::DbError| anyhow::anyhow!(error.to_string());
                let unframed = run_statements(&mut conn, &statements, None, failed).await.unwrap_err();
                assert!(unframed.to_string().contains("unframed consumer cannot skip the guard"));
                let dialect = pbps_pg::Postgres::new();
                let zone = SessionZone::utc();
                let refused = run_statements(&mut conn, &statements, Some((&dialect, &zone, true)), failed).await.unwrap_err();
                assert!(refused.to_string().contains("1 row(s) of app.t violate"), "{refused}");
                conn.execute("UPDATE app.t SET amount=1").await.unwrap();
                run_statements(&mut conn, &statements, Some((&dialect, &zone, true)), failed).await.unwrap();
                let rows = conn.query("SELECT count(*)::bigint AS n FROM pg_attribute WHERE attrelid='app.t'::regclass AND attname='amount' AND NOT attisdropped").await.unwrap();
                assert_eq!(rows[0].try_get::<i64>("n").unwrap(), Some(0));
                drop(conn);
                admin.execute(&format!("DROP DATABASE {name} WITH (FORCE)")).await.unwrap();
            }
        });
    }
}

/// A domain's default and checks as the declarations wrote them and as the
/// engine stored them, where both are there and the text differs: the part of a
/// type's text that [`respelt`] cannot read out of a change, since the differ
/// refuses to express a changed domain.
fn domain_respellings(declared: &Schema, engine: &Schema) -> Vec<String> {
    use pbps_model::ManagedType::Domain;
    let mut lines = Vec::new();
    for (name, wanted) in &declared.types {
        let (
            Domain {
                default: wanted_default,
                checks: wanted_checks,
                ..
            },
            Some(Domain {
                default: stored_default,
                checks: stored_checks,
                ..
            }),
        ) = (wanted, engine.types.get(name))
        else {
            continue;
        };
        if let (Some(wanted), Some(stored)) = (wanted_default, stored_default)
            && wanted != stored
        {
            lines.push(format!(
                "domain {} default: declared `{wanted}`, the engine stores `{stored}`",
                name.shown()
            ));
        }
        for (check, wanted) in wanted_checks {
            if let Some(stored) = stored_checks.get(check)
                && wanted != stored
            {
                lines.push(format!(
                    "domain {} check {}: declared `{wanted}`, the engine stores `{stored}`",
                    name.shown(),
                    check.shown()
                ));
            }
        }
    }
    lines
}

/// What the engine respelt: the declared text and the form it stored, for each
/// object whose text differs and which is there. Only the engines that deparse
/// what they store reach this; an object that is not there is not a respelling
/// and is [`classify`]'s to report.
#[allow(clippy::wildcard_enum_match_arm)]
fn respelt(remaining: &ChangeSet, engine: &Schema) -> Vec<String> {
    let mut lines = classify(remaining, engine).1;
    for planned in &remaining.changes {
        match &planned.change {
            Change::AlterModule { id, module, .. } => {
                if let Some(stored) = engine.modules.get(id) {
                    lines.push(format!(
                        "{} {id}: declared `{}`, the engine stores `{}`",
                        module.kind,
                        module.definition.trim(),
                        stored.definition.trim(),
                        id = id.shown()
                    ));
                }
            }
            Change::AddIndex {
                table, name, index, ..
            } => {
                if let Some(stored) = engine.tables.get(table).and_then(|t| t.indexes.get(name))
                    && index.filter != stored.filter
                {
                    lines.push(format!(
                        "index {name} on {table}: declared where `{}`, the engine stores `{}`",
                        index.filter.as_deref().unwrap_or("(none)"),
                        stored.filter.as_deref().unwrap_or("(none)"),
                        name = name.shown(),
                        table = table.shown(),
                    ));
                }
            }
            _ => {}
        }
    }
    lines
}

/// Splits what is left over into "the plan is wrong" and "the engine spells it
/// differently", and describes each in the terms that make it actionable.
#[allow(clippy::wildcard_enum_match_arm)]
fn classify(remaining: &ChangeSet, engine: &Schema) -> (Vec<String>, Vec<String>) {
    let mut structural = Vec::new();
    let mut spelling = Vec::new();

    for p in &remaining.changes {
        // Exhaustiveness would be the wrong instrument here: a change kind
        // added later is structural until somebody shows it is a spelling, and
        // the safe default is the one that fails the rehearsal loudly.
        match &p.change {
            Change::AddCheck {
                table,
                name,
                constraint,
            } => {
                // A spelling difference needs two spellings. When the engine has
                // no constraint by that name the plan simply did not create it,
                // which is non-convergence — and `converged()` looks only at
                // `structural`, so calling it a spelling would pass the
                // rehearsal with the constraint missing.
                let stored = engine
                    .tables
                    .get(table)
                    .and_then(|t| t.checks.get(name))
                    .map(|c| c.expression.as_str());
                match stored {
                    Some(stored) => spelling.push(format!(
                        "check {name} on {table}: declared `{}`, the engine stores `{stored}`",
                        constraint.expression,
                        table = table.shown(),
                        name = name.shown(),
                    )),
                    None => structural.push(format!(
                        "check {name} on {table} is declared but the plan did not create it",
                        table = table.shown(),
                        name = name.shown(),
                    )),
                }
            }
            Change::AlterColumnDefault { column, to, .. } => spelling.push(format!(
                "default on {column}: declared `{}`, the engine stores `{}`",
                to.as_deref().unwrap_or("(none)"),
                engine
                    .tables
                    .get(&column.table)
                    .and_then(|t| t.columns.get(&column.name))
                    .and_then(|c| match &c.value_source {
                        pbps_model::ValueSource::Default(default) => Some(default.as_str()),
                        pbps_model::ValueSource::None
                        | pbps_model::ValueSource::Identity(_)
                        | pbps_model::ValueSource::Sequence(_)
                        | pbps_model::ValueSource::Generated(_) => None,
                    })
                    .unwrap_or("(none)"),
                column = column.shown()
            )),
            // A module whose stored body still differs is not layout: the
            // difference already survived `Dialect::normalize_definition`, and
            // the engine stores a module's text verbatim rather than rewriting
            // it the way it rewrites an expression. So it is the emitter or the
            // introspection that is wrong, and swallowing it here would let a
            // view with the wrong body pass as converged.
            Change::AlterModule { id, module, .. } => structural.push(format!(
                "{} {id}: declared `{}`, the engine stores `{}`",
                module.kind,
                module.definition.trim(),
                engine
                    .modules
                    .get(id)
                    .map(|m| m.definition.trim())
                    .unwrap_or("(absent)"),
                id = id.shown()
            )),
            // The other half of a check's drop+add pair: not a second finding.
            Change::DropCheck { .. } => {}
            // A row whose every differing cell is "the default" on both sides
            // is the default's own spelling showing through — `N'x'` declared,
            // `'x'` stored — which is the check-constraint case one level
            // down. Any cell with a real value on either side is a row the
            // plan did not write as declared, and stays structural.
            Change::UpdateRow {
                table,
                key,
                columns,
                ..
            } if columns.values().all(|(from, to)| {
                matches!(
                    (from, to),
                    (pbps_model::Cell::Default(_), pbps_model::Cell::Default(_))
                )
            }) =>
            {
                for (column, (from, to)) in columns {
                    spelling.push(format!(
                        "row {key} of {table}, column {column}: declared {to}, the engine stores {from}",
                        table = table.shown(),

                        column = column.shown(),
                    ));
                }
            }
            // Deprecation is a fact about the declarations, and the emitter
            // writes nothing for it on purpose. The engine has nowhere to keep
            // it, so it comes back on every rehearsal of every declaration that
            // uses it — as non-convergence it would mean `plan --dev` could
            // never pass on a schema that documents a deprecated column.
            Change::SetColumnDeprecated { .. } => {}
            other => structural.push(crate::report::describe(other)),
        }
    }
    (structural, spelling)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn planned(c: Change) -> pbps_model::PlannedChange {
        pbps_model::PlannedChange::new(c)
    }

    fn view(definition: &str) -> pbps_model::Module {
        pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            description: None,
            definition: definition.into(),
            options: Default::default(),
        }
    }

    fn domain(default: Option<&str>, check: &str) -> pbps_model::ManagedType {
        pbps_model::ManagedType::Domain {
            base: "text".parse().unwrap(),
            not_null: false,
            default: default.map(str::to_owned),
            checks: [("allowed".into(), check.to_owned())].into(),
            collation: None,
        }
    }

    /// The text of a domain the engine deparsed is a respelling to print, not
    /// a difference to fail on; text that agrees, a domain the engine lacks and
    /// an enum say nothing.
    #[test]
    fn a_domains_deparsed_text_is_reported_as_a_respelling() {
        let name: pbps_model::TypeRef = "app.choice".parse().unwrap();
        let mut declared = Schema::default();
        declared
            .types
            .insert(name.clone(), domain(Some("'a'"), "VALUE IN ('a','b')"));
        declared.types.insert(
            "app.mood".parse().unwrap(),
            pbps_model::ManagedType::Enum {
                values: vec!["ok".into()],
            },
        );
        let mut engine = Schema::default();
        engine.types.insert(
            name.clone(),
            domain(
                Some("'a'::text"),
                "(VALUE = ANY (ARRAY['a'::text, 'b'::text]))",
            ),
        );
        let lines = domain_respellings(&declared, &engine);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("default: declared `'a'`, the engine stores `'a'::text`"));
        assert!(lines[1].contains("check allowed: declared `VALUE IN ('a','b')`"));
        // Negative: the same text, and a type the engine does not hold.
        let mut same = Schema::default();
        same.types
            .insert(name.clone(), declared.types[&name].clone());
        assert!(domain_respellings(&declared, &same).is_empty());
        assert!(domain_respellings(&declared, &Schema::default()).is_empty());
    }

    /// A declared type is recorded when it is planned, so the target's base can
    /// record an identity for a type it does not hold yet. Built from empty,
    /// that is not an identity to claim; the base builds without it.
    #[test]
    fn a_type_the_base_records_and_does_not_hold_does_not_stop_it_building() {
        let pg = pbps_pg::Postgres::new();
        let hints = pbps_model::Hints::default();
        let name: pbps_model::TypeRef = "app.choice".parse().unwrap();
        let mut ids = IdsFile::default();
        ids.types.insert(
            pbps_model::Uid::generate(pbps_model::uid::UidKind::Type),
            name.clone(),
        );
        assert!(lower_from_empty(&pg, &Schema::default(), &ids, &hints).is_ok());
        // Negative: a type the base holds is built, with its identity.
        let mut held = Schema::default();
        held.types.insert(name, domain(None, "VALUE <> ''"));
        let (lowered, _) = lower_from_empty(&pg, &held, &ids, &hints).unwrap();
        assert!(
            lowered
                .statements()
                .iter()
                .any(|s| s.sql.contains("CREATE DOMAIN")),
            "{:?}",
            lowered.statements()
        );
    }

    fn declarations() -> (Schema, IdsFile) {
        let mut schema = Schema::default();
        schema
            .roles
            .insert("reader".into(), pbps_model::role::Role::default());
        let mut table = pbps_model::schema::Table::default();
        table.row_policies.insert(
            "mine".into(),
            pbps_model::schema::RowPolicy {
                to: pbps_model::schema::PolicyRoles::Roles(["reader".into()].into()),
                ..Default::default()
            },
        );
        schema.tables.insert("app.t".parse().unwrap(), table);
        let mut ids = IdsFile::default();
        ids.roles.insert(
            pbps_model::Uid::generate(pbps_model::uid::UidKind::Role),
            "reader".into(),
        );
        (schema, ids)
    }

    fn target<'a>(
        statements: &'a [Statement],
        ids: &'a IdsFile,
        roles: Option<&'a RoleMapping>,
    ) -> Base<'a> {
        Base::Target {
            statements,
            ids,
            execution: Execution {
                zone: Box::leak(Box::new(SessionZone::utc())),
                transactional: true,
            },
            roles,
        }
    }

    /// The scratch is compared with the declarations under its own role names
    /// when the plan ran on them, with no roles at all when the account could
    /// not make any, and as written for the git baseline.
    #[test]
    fn the_declarations_are_compared_under_the_names_the_scratch_has() {
        let (schema, ids) = declarations();
        let mapping = RoleMapping::generate(&schema.named_roles(), "run", "tok", |_| false);
        let none: Vec<Statement> = Vec::new();

        let (mapped, mapped_ids) =
            compared_declarations(&target(&none, &ids, Some(&mapping)), schema.clone(), &ids);
        assert!(mapped.roles.contains_key("run_0_tok"));
        assert!(!mapped.roles.contains_key("reader"));
        assert_eq!(
            mapped.tables.values().next().unwrap().row_policies["mine"].to,
            pbps_model::schema::PolicyRoles::Roles(["run_0_tok".into()].into())
        );
        assert_eq!(mapped_ids.roles.values().next().unwrap().raw(), "run_0_tok");

        // Without a mapping a connected rehearsal compares none: the roles and
        // the policies that name them are left out of both sides.
        let (cut, cut_ids) =
            compared_declarations(&target(&none, &ids, None), schema.clone(), &ids);
        assert!(cut.roles.is_empty() && cut_ids.roles.is_empty());
        assert!(cut.tables.values().all(|t| t.row_policies.is_empty()));

        // Negative: the baseline compares what it was given, untouched.
        let baseline = Base::Baseline {
            statements: &none,
            ids: &ids,
        };
        let (same, same_ids) = compared_declarations(&baseline, schema.clone(), &ids);
        assert_eq!(same, schema);
        assert_eq!(same_ids, ids);
    }

    /// A body that still differs after the plan has run is the emitter or the
    /// introspection being wrong, not the engine spelling things its own way:
    /// unlike an expression, a module's text is stored verbatim. Swallowing it
    /// would let a view with the wrong body rehearse as converged.
    #[test]
    fn a_module_the_engine_stored_differently_fails_the_rehearsal() {
        let name: pbps_model::ModuleId = "dbo.v".parse().unwrap();
        let mut engine = Schema::default();
        engine.modules.insert(name.clone(), view("SELECT 1 AS one"));

        let remaining = ChangeSet {
            changes: vec![planned(Change::AlterModule {
                id: name.clone(),
                module: Box::new(view("SELECT 2 AS two")),
            })],
        };
        let (structural, spelling) = classify(&remaining, &engine);
        assert!(spelling.is_empty(), "{spelling:?}");
        assert_eq!(structural.len(), 1, "{structural:?}");
        // Both sides have to be in the message, or the reader cannot tell which
        // half is wrong.
        assert!(structural[0].contains("SELECT 2 AS two"), "{structural:?}");
        assert!(structural[0].contains("SELECT 1 AS one"), "{structural:?}");
    }

    /// Deprecation is a fact about the declarations; the emitter writes nothing
    /// for it on purpose and the engine has nowhere to keep it. Counting it as
    /// non-convergence would mean `plan --dev` could never pass on a schema
    /// that documents a deprecated column.
    #[test]
    fn a_deprecation_never_fails_the_rehearsal() {
        let remaining = ChangeSet {
            changes: vec![planned(Change::SetColumnDeprecated {
                uid: "c_a1b2c3".parse().unwrap(),
                column: "dbo.t.old".parse().unwrap(),
                reason: Some("superseded by email".into()),
            })],
        };
        let (structural, spelling) = classify(&remaining, &Schema::default());
        assert!(structural.is_empty(), "{structural:?}");
        assert!(spelling.is_empty(), "{spelling:?}");
    }

    /// A spelling difference needs two spellings. When the engine has nothing
    /// by that name the plan did not create the constraint at all — and
    /// `converged()` looks only at `structural`, so calling it a spelling would
    /// pass the rehearsal with the check missing from the database.
    #[test]
    fn a_check_the_plan_never_created_is_structural() {
        let table: pbps_model::TableName = "dbo.t".parse().unwrap();
        let remaining = ChangeSet {
            changes: vec![planned(Change::AddCheck {
                table: table.clone(),
                name: "ck_positive".into(),
                constraint: pbps_model::CheckConstraint {
                    expression: "qty > 0".into(),
                },
            })],
        };

        // Nothing in the engine: the plan failed to create it.
        let (structural, spelling) = classify(&remaining, &Schema::default());
        assert_eq!(structural.len(), 1, "{structural:?}");
        assert!(spelling.is_empty(), "{spelling:?}");

        // The same name present, spelled the engine's way: a spelling after all.
        let mut engine = Schema::default();
        let mut t = pbps_model::Table::default();
        t.checks.insert(
            "ck_positive".into(),
            pbps_model::CheckConstraint {
                expression: "([qty]>(0))".into(),
            },
        );
        engine.tables.insert(table, t);
        let (structural, spelling) = classify(&remaining, &engine);
        assert!(structural.is_empty(), "{structural:?}");
        assert_eq!(spelling.len(), 1, "{spelling:?}");
    }

    /// The safe default is the loud one: a change kind nobody has classified
    /// must fail the rehearsal rather than pass quietly.
    #[test]
    fn an_unclassified_change_is_structural() {
        let remaining = ChangeSet {
            changes: vec![planned(Change::DropColumn {
                uid: "c_a1b2c3".parse().unwrap(),
                column: "dbo.t.gone".parse().unwrap(),
            })],
        };
        let (structural, spelling) = classify(&remaining, &Schema::default());
        assert_eq!(structural.len(), 1, "{structural:?}");
        assert!(spelling.is_empty(), "{spelling:?}");
    }
    fn index_on(table: &str, name: &str, filter: Option<&str>) -> pbps_model::Index {
        let _ = (table, name);
        pbps_model::Index {
            columns: Vec::new(),
            include: Vec::new(),
            unique: false,
            filter: filter.map(str::to_owned),
            method: Default::default(),
            storage_parameters: Default::default(),
        }
    }

    fn engine_with_index(filter: Option<&str>) -> Schema {
        let mut engine = Schema::default();
        let mut table = pbps_model::Table::default();
        table
            .indexes
            .insert("ix".into(), index_on("app.t", "ix", filter));
        engine.tables.insert("app.t".parse().unwrap(), table);
        engine
    }

    fn adding_index(filter: Option<&str>) -> ChangeSet {
        ChangeSet {
            changes: vec![planned(Change::AddIndex {
                table: "app.t".parse().unwrap(),
                name: "ix".into(),
                index: Box::new(index_on("app.t", "ix", filter)),
                clustered: false,
            })],
        }
    }

    /// An engine that deparses what it stores respells a module body and an
    /// index predicate; only an object that is there can be respelt.
    #[test]
    fn a_respelling_needs_the_object_to_be_there_and_the_text_to_differ() {
        let remaining = adding_index(Some("note IS NOT NULL"));
        let lines = respelt(&remaining, &engine_with_index(Some("(note IS NOT NULL)")));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("the engine stores `(note IS NOT NULL)`"),
            "{lines:?}"
        );
        // Negative: the engine holds no such index, so nothing was respelt.
        assert!(respelt(&remaining, &Schema::default()).is_empty());
        // Negative: the same predicate is not a difference to report.
        let same = respelt(&remaining, &engine_with_index(Some("note IS NOT NULL")));
        assert!(same.is_empty(), "{same:?}");
        // Negative: a module the engine lacks is the plan's failure, and
        // `classify` reports it.
        let module = ChangeSet {
            changes: vec![planned(Change::AlterModule {
                id: "app.v".parse().unwrap(),
                module: Box::new(view("SELECT 1 AS one")),
            })],
        };
        assert!(respelt(&module, &Schema::default()).is_empty());
        let mut engine = Schema::default();
        engine
            .modules
            .insert("app.v".parse().unwrap(), view("SELECT 1 AS one;"));
        assert_eq!(respelt(&module, &engine).len(), 1);
    }

    /// The scratch is shared, so what a rehearsal may not create is exactly
    /// the cluster's principals and the privileges they hold.
    #[test]
    fn only_changes_about_who_may_do_what_are_left_out_of_a_rehearsal() {
        use pbps_model::{Uid, UidKind};
        let role = || Uid::generate(UidKind::Role);
        for change in [
            Change::CreateRole {
                uid: role(),
                name: "r".into(),
            },
            Change::DropRole {
                uid: role(),
                name: "r".into(),
                members: Vec::new(),
            },
            Change::RenameRole {
                uid: role(),
                from: "r".into(),
                to: "s".into(),
            },
            Change::CreatePolicy {
                table: "app.t".parse().unwrap(),
                name: "p".into(),
                policy: Default::default(),
                origin: pbps_model::RowSecurityOrigin::Created,
            },
            Change::DropPolicy {
                table: "app.t".parse().unwrap(),
                name: "p".into(),
                policy: Default::default(),
            },
            Change::SetRowSecurity {
                uid: Uid::generate(UidKind::Table),
                table: "app.t".parse().unwrap(),
                from: Default::default(),
                to: Default::default(),
                origin: pbps_model::RowSecurityOrigin::Created,
            },
        ] {
            assert!(is_authorization(&change), "{change:?}");
        }
        // Negative: the database's own objects are rehearsed.
        for change in [
            Change::DropColumn {
                uid: "c_a1b2c3".parse().unwrap(),
                column: "dbo.t.gone".parse().unwrap(),
            },
            Change::DropIndex {
                table: "app.t".parse().unwrap(),
                name: "ix".into(),
            },
        ] {
            assert!(!is_authorization(&change), "{change:?}");
        }
    }
}
