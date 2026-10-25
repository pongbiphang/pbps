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

use anyhow::Context as _;

use pbps_db::Conn;
use pbps_dialect::Dialect;
use pbps_model::{Change, ChangeSet, IdsFile, Schema};

/// The state the plan's statements run on top of.
pub enum Base<'a> {
    /// The git baseline, lowered from empty: what `plan --dev` previews.
    Baseline {
        statements: &'a crate::emission::LoweredPlan,
        /// The identity mapping the baseline was built under, which names the
        /// objects the scratch database holds before the plan runs.
        ids: &'a IdsFile,
    },
}

impl<'a> Base<'a> {
    fn statements(&self) -> &'a crate::emission::LoweredPlan {
        match self {
            Base::Baseline { statements, .. } => statements,
        }
    }

    fn ids(&self) -> &'a IdsFile {
        match self {
            Base::Baseline { ids, .. } => ids,
        }
    }
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
    plan: &crate::emission::LoweredPlan,
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
    let declared = &refused?;

    let build = base.statements();
    for (i, stmt) in build.iter().enumerate() {
        conn.execute(&stmt.sql).await.map_err(|e| {
            anyhow::anyhow!(
                "the dev database rejected statement {} of the baseline:\n{}\n\n{e}\n\n\
                 The baseline itself does not compile, so nothing can be said about the plan.",
                i + 1,
                stmt.sql
            )
        })?;
    }
    for (i, stmt) in plan.iter().enumerate() {
        conn.execute(&stmt.sql).await.map_err(|e| {
            anyhow::anyhow!(
                "the dev database rejected statement {} of the plan:\n{}\n\n{e}\n\n\
                 This plan would fail at apply time; nothing was changed anywhere real.",
                i + 1,
                stmt.sql
            )
        })?;
    }

    let built = crate::engine::introspect(conn, crate::engine::Read::Snapshot)
        .await
        .context("cannot read the dev database back")?;
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
        .context("cannot read the declared rows back from the dev database")?;
    let engine = scoped
        .schema
        .with_observed_rows(&rows, &declared_data, declared)?;
    // As `plan --db` compares it: a collation equal to this database's
    // default reads back as none (#1175).
    let declared_here =
        crate::deploy::declarations_as_compared(declared, built.database_collation.as_deref());
    let remaining = pbps_diff::diff(
        pbps_diff::Side {
            schema: &engine,
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
            "the rehearsal produced {} inexpressible difference(s)",
            errs.len()
        )
    })?;

    let (structural, spelling) = classify(&remaining, &engine);
    Ok(Findings {
        built: build.len(),
        applied: plan.len(),
        structural,
        spelling,
        externals_staged: false,
    })
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
        }
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
}
