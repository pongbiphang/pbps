//! The desired managed namespace, compiled on scratch (ADR-0016 decision 2;
//! docs/RESOLVER-MSSQL.md §5; issue #2374).
//!
//! The statements are the emitter's own: a fresh bootstrap of the declared
//! schema, ordered by `pbps_diff`'s scratch route (DEC-1959.6). The route
//! first splits a new table whose generation calls a planned function
//! through `Mssql::generated_table_parts` (DEC-1503.1, `generated.rs`), then
//! orders the parts by the constraints it derives for every split table:
//! the bare table, each late computed column in declared order, and what
//! names them. This module supplies one part per change and nothing else.
//!
//! Declared order is a hard chain: a computed column's position is compared
//! as reported, the engine appends it, and nothing reorders columns, so a
//! column added before an earlier-declared one lands elsewhere than on the
//! target. A table whose earlier column calls a function that reads a later
//! one has therefore no order that compiles and reproduces the positions,
//! and refuses by name ([`ReconstructError::Cycle`]), with the remedy.
//!
//! The order is the plan's, which is the schedule's tie-break, and a
//! constraint moves a part later and nothing else does.
//!
//! Roles, grants and rows are not reproduced. None changes what a name
//! binds to, and the plan's schema grants, the only kind it carries, are
//! replayed before anything compiles (`authorization::apply_planned`).

use super::capture::{ObjectIdentity, class, object};
use crate::Mssql;
use pbps_diff::order::scratch::{Input, Part};
use pbps_model::{Change, Strategy};
use std::collections::{BTreeMap, BTreeSet};

/// Why the desired namespace could not be built on scratch. A named limit,
/// never a partially compiled namespace that evidence could be read from.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReconstructError {
    #[error("the desired namespace cannot be reconstructed through a {0} change")]
    Unsupported(String),
    #[error("the emitter cannot write {declaration}: {reason}")]
    Emit { declaration: String, reason: String },
    #[error("scratch compilation of {declaration} failed: {reason}")]
    Compile { declaration: String, reason: String },
    /// A statement failed for a reason about the server or the connection
    /// rather than the statement ([`judged_the_statement`]): no verdict on
    /// the declaration either (SPEC 9.8, #1597).
    #[error("scratch compilation of {declaration} was interrupted: {reason}")]
    Interrupted { declaration: String, reason: String },
    #[error(
        "scratch cannot order {0}. Either a computed column and a declared function each need \
         the other, or a column that calls a function reads a column declared after it: \
         columns are added in the order they are declared, which fixes their positions, and \
         SQL Server cannot reorder them, so declare the column the function reads first. A \
         function is read as needing a column wherever it names the column, so one that only \
         shares a column's name (an alias, another table's column, a table of the same name in \
         another schema) needs it too: rename the alias or the column"
    )]
    Cycle(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// What an error names: the declaration, never the statement text.
    declaration: String,
    /// The roots this step creates: a table, a module or a type. A child
    /// arrives with its parent and is not listed.
    created: Vec<ObjectIdentity>,
}

/// SQL-free lowering input. The lexical observations are finite answers from
/// a private emitter probe, never a retained statement collection.
pub struct ScratchBlueprint(Input<Metadata>);

impl pbps_diff::order::scratch::Engine for Mssql {
    type Input = ScratchBlueprint;
    type Metadata = Metadata;
    fn blueprint(&self, input: Self::Input) -> Input<Metadata> {
        input.0
    }
}

#[derive(Debug, Clone)]
struct ScratchEmitter;
impl pbps_dialect::lower::Lowering for ScratchEmitter {
    type Input = pbps_diff::order::scratch::ScratchPlan<Mssql>;
    fn emit_position(
        &self,
        input: &Self::Input,
        position: usize,
    ) -> Result<Vec<pbps_dialect::Statement>, pbps_dialect::DialectError> {
        let part = &input.parts()[position].change;
        crate::emit::emit(&part.change, part.strategy)
    }
}

/// The ordered scratch statements for one desired schema.
#[derive(Debug, Clone)]
pub struct Reconstruction {
    steps: Vec<Metadata>,
    lowered: pbps_dialect::lower::LoweredPlan<ScratchEmitter>,
}

type Draft = Part<Metadata>;

/// One part: validated where it is made, so a declaration the emitter cannot
/// write is refused by name before anything is ordered.
fn part(
    operation: usize,
    planned: &pbps_model::PlannedChange,
    declaration: String,
    created: Vec<ObjectIdentity>,
) -> Result<Draft, ReconstructError> {
    crate::emit::emit(&planned.change, Strategy::default()).map_err(|error| {
        ReconstructError::Emit {
            declaration: declaration.clone(),
            reason: error.to_string(),
        }
    })?;
    Ok(Part {
        operation,
        change: planned.clone(),
        metadata: Metadata {
            declaration,
            created,
        },
    })
}

impl Reconstruction {
    /// `bootstrap` is the plan from an empty schema to the desired one, as
    /// the differ produces it.
    ///
    /// ```compile_fail
    /// fn raw(dialect: &pbps_mssql::Mssql, changes: &[pbps_model::Change]) {
    ///     pbps_mssql::resolver::reconstruct::Reconstruction::new(dialect, changes);
    /// }
    /// ```
    // The one wildcard: every change that is not a creation, a skipped kind
    // or a split part is refused by name, which is the safe direction for a
    // variant added later.
    #[allow(clippy::wildcard_enum_match_arm)]
    pub fn new<S: pbps_diff::order::RouteState<Mssql>>(
        dialect: &Mssql,
        bootstrap: &pbps_diff::order::PlanningPlan<S>,
        ids: &pbps_model::IdsFile,
        dependencies: &pbps_model::ModuleDeps,
    ) -> Result<Self, ReconstructError> {
        let prepared = pbps_diff::order::scratch::prepare(bootstrap, ids, dependencies, dialect)
            .map_err(ReconstructError::Cycle)?;
        let mut steps: Vec<Draft> = Vec::new();
        for (operation, planned) in prepared.changes().changes.iter().enumerate() {
            let step = |declaration: String, created: Vec<ObjectIdentity>| {
                part(operation, planned, declaration, created)
            };
            match &planned.change {
                Change::CreateTable { name, .. } => steps.push(step(
                    format!("table {}", name.shown()),
                    vec![object(&name.schema, &name.name)],
                )?),
                Change::CreateModule { id, module } => steps.push(step(
                    format!("{} {}", module.kind, id.shown()),
                    vec![object(id.schema(), id.name())],
                )?),
                Change::CreateType { name, .. } => steps.push(step(
                    format!("type {}", name.shown()),
                    vec![ObjectIdentity {
                        class: class::TYPE.into(),
                        name: vec![name.schema().to_owned(), name.name().to_owned()],
                        signature: Vec::new(),
                    }],
                )?),
                // What a split table is made of, and the differ's own foreign
                // keys: created after what they name by the route's constraints.
                Change::AddComputedColumn { table, name, .. } => steps.push(step(
                    format!("computed column {}", table.column(name.clone()).shown()),
                    Vec::new(),
                )?),
                Change::SetPrimaryKey { table, .. } => steps.push(step(
                    format!("primary key of {}", table.shown()),
                    Vec::new(),
                )?),
                Change::AddUnique { table, name, .. } => steps.push(step(
                    format!("unique {} on {}", name.shown(), table.shown()),
                    Vec::new(),
                )?),
                Change::AddIndex { table, name, .. } => steps.push(step(
                    format!("index {} on {}", name.shown(), table.shown()),
                    Vec::new(),
                )?),
                Change::AddForeignKey { table, name, .. } => steps.push(step(
                    format!("foreign key {} of {}", name.shown(), table.shown()),
                    Vec::new(),
                )?),
                Change::AlterColumnDefault { column, .. } => steps.push(step(
                    format!("the default of {}", column.shown()),
                    Vec::new(),
                )?),
                Change::AddCheck { table, name, .. } => steps.push(step(
                    format!("check {} on {}", name.shown(), table.shown()),
                    Vec::new(),
                )?),
                // None of these changes what a name resolves to.
                Change::CreateRole { .. }
                | Change::Grant { .. }
                | Change::PublicExecution { .. }
                | Change::InsertRow { .. }
                | Change::SetDataMode { .. }
                | Change::SetEnsureKeys { .. }
                | Change::CreatePolicy { .. }
                | Change::DropPolicy { .. }
                | Change::SetRowSecurity { .. } => {}
                // A bootstrap from an empty schema has no renames, drops or
                // alterations; one that does is not a bootstrap.
                other => {
                    return Err(ReconstructError::Unsupported(
                        serde_json::to_value(other)
                            .ok()
                            .and_then(|op| op["op"].as_str().map(str::to_owned))
                            .unwrap_or_default(),
                    ));
                }
            }
        }
        let input = ScratchBlueprint(Input::new(prepared, steps));
        let ordered =
            pbps_diff::order::scratch::settle(dialect, input).map_err(ReconstructError::Cycle)?;
        let lowered = pbps_dialect::lower::lower_all(&ordered, &ScratchEmitter)
            .expect("the identical private probes already validated these lowering parts");
        let steps = ordered
            .parts()
            .iter()
            .map(|part| part.metadata.clone())
            .collect();
        Ok(Self { steps, lowered })
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Which step creates each root the declarations name: a table, a module
    /// or a type. The capture of scratch must hold every one of them.
    pub fn created(&self) -> BTreeMap<ObjectIdentity, usize> {
        self.steps
            .iter()
            .enumerate()
            .flat_map(|(at, step)| step.created.iter().map(move |made| (made.clone(), at)))
            .collect()
    }

    /// The roots a compiled declaration created that `present` lacks, in the
    /// order of their steps. A root that compiled and is missing from its own
    /// capture is an incomplete read, never a declaration with nothing to
    /// compare.
    pub fn absent_from(&self, present: &BTreeSet<&ObjectIdentity>) -> Vec<&ObjectIdentity> {
        self.steps
            .iter()
            .flat_map(|step| &step.created)
            .filter(|made| !present.contains(made))
            .collect()
    }

    /// Runs the `range` of steps, one `Statement` per batch and each in
    /// autocommit, as whatever principal the session currently is: the
    /// reproduced deployer, which the caller has entered. Measured on 17.0:
    /// a failed statement leaves nothing behind and the session usable, so
    /// no transaction is needed and a later reader sees each step at once.
    pub async fn compile_steps(
        &self,
        conn: &mut impl pbps_db::transport::ExecuteConnection,
        range: std::ops::Range<usize>,
    ) -> Result<(), ReconstructError> {
        for (step, group) in self.steps[range.clone()]
            .iter()
            .zip(&self.lowered.groups()[range])
        {
            for statement in group.statements() {
                conn.execute(&statement.sql).await.map_err(|error| {
                    let declaration = step.declaration.clone();
                    let reason = error.to_string();
                    if judged_the_statement(&error) {
                        ReconstructError::Compile {
                            declaration,
                            reason,
                        }
                    } else {
                        ReconstructError::Interrupted {
                            declaration,
                            reason,
                        }
                    }
                })?;
            }
        }
        Ok(())
    }
}

/// Whether a failed statement is a verdict on the declaration. A deadlock,
/// a lock or resource limit and a lost connection answer nothing about it.
/// A refusal with no server code is a transport failure, never a verdict.
pub fn judged_the_statement(error: &pbps_db::DbError) -> bool {
    const NOT_ABOUT_THE_STATEMENT: &[&str] = &[
        "233", "596", "701", "802", "1101", "1105", "1204", "1205", "1222", "8645", "8651", "9002",
    ];
    error
        .server_error_code()
        .is_some_and(|code| !NOT_ABOUT_THE_STATEMENT.contains(&code.as_str()))
}

#[cfg(test)]
mod tests;
