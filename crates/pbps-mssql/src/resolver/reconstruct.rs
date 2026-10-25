//! The desired managed namespace, compiled on scratch (ADR-0016 decision 2;
//! docs/RESOLVER-MSSQL.md §5; issue #2374).
//!
//! The statements are the emitter's own: a fresh bootstrap of the declared
//! schema. This module supplies the order constraints and `pbps_diff`'s
//! scratch route owns the order (DEC-1959.6); no SQL Server reorder pass is
//! written here.
//!
//! The ordinary plan order is not a compile order. A table's `CREATE`
//! carries its computed columns and defaults inline, and a computed column
//! that names a declared function fails there with 4121, as a plan that
//! creates both is refused outright. So a table is created bare, the modules
//! follow, and what can name a module (defaults, checks, computed columns,
//! and the keys and indexes over a computed column) comes last. Measured on
//! 17.0: table, then function, then `ALTER TABLE .. ADD c AS f(..)` compiles,
//! even where `f` is schema-bound and reads the table.
//!
//! A module that reads a computed column must follow it, and the column
//! must follow the functions it calls. That is PostgreSQL's "late table"
//! question over a smaller unit: the held part is only the computed column
//! and what is built over it, never the table, which SQL Server may create
//! before the function that reads it.
//!
//! Roles, grants and rows are not reproduced. None changes what a name
//! binds to, and the plan's schema grants, the only kind it carries, are
//! replayed before anything compiles (`authorization::apply_planned`).

use super::capture::{ObjectIdentity, class, object};
use crate::Mssql;
use pbps_dialect::Dialect;
use pbps_diff::order::scratch::{HeldTable, Input, Part, Phase};
use pbps_model::{Change, ModuleKind, PlannedChange, Strategy, TableName, ValueSource};
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
        "{0} has a computed column calling a declared function that itself needs the column; \
         no order compiles both"
    )]
    Cycle(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    phase: Phase,
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
    fn phase(metadata: &mut Metadata, phase: Phase) {
        metadata.phase = phase;
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

/// A table whose computed columns, and what stands over them, are held
/// until the functions they call exist.
struct Late {
    table: TableName,
    computed: BTreeSet<String>,
}

type Draft = Part<Metadata>;

fn part(
    phase: Phase,
    declaration: &str,
    change: Change,
    names: Vec<String>,
    created: Vec<ObjectIdentity>,
) -> Result<Draft, ReconstructError> {
    // Validate each part where it is made, so a declaration the emitter
    // cannot write is refused by name before anything is ordered.
    crate::emit::emit(&change, Strategy::default()).map_err(|error| ReconstructError::Emit {
        declaration: declaration.to_owned(),
        reason: error.to_string(),
    })?;
    Ok(Part {
        operation: 0,
        slot: 0,
        change: PlannedChange::new(change),
        phase,
        declaration: declaration.to_owned(),
        names,
        calls: BTreeSet::new(),
        tables: BTreeSet::new(),
        metadata: Metadata {
            phase,
            declaration: declaration.to_owned(),
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
        let mut late: Vec<(Late, std::ops::Range<usize>)> = Vec::new();
        let mut functions: Vec<(usize, String)> = Vec::new();
        for (operation, planned) in prepared.changes().changes.iter().enumerate() {
            let change = &planned.change;
            let first = steps.len();
            match change {
                Change::CreateTable { name, table, .. } => {
                    table_steps(name, table, change, &mut steps, &mut late)?;
                }
                Change::CreateModule { id, module } => {
                    if module.kind == ModuleKind::Function {
                        functions.push((steps.len(), id.name().to_owned()));
                    }
                    steps.push(part(
                        Phase::Modules,
                        &format!("{} {}", module.kind, id.shown()),
                        change.clone(),
                        vec![id.name().to_owned()],
                        vec![object(id.schema(), id.name())],
                    )?);
                }
                Change::CreateType { name, .. } => {
                    steps.push(part(
                        Phase::Tables,
                        &format!("type {}", name.shown()),
                        change.clone(),
                        vec![name.name().to_owned()],
                        vec![ObjectIdentity {
                            class: class::TYPE.into(),
                            name: vec![name.schema().to_owned(), name.name().to_owned()],
                            signature: Vec::new(),
                        }],
                    )?);
                }
                // The differ splits a new table's foreign keys out of its
                // CREATE. A key names only tables and their keys, so it
                // follows every table.
                Change::AddForeignKey { table, .. } => steps.push(part(
                    Phase::Keys,
                    &format!("a foreign key of {}", table.shown()),
                    change.clone(),
                    Vec::new(),
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
            for made in &mut steps[first..] {
                made.operation = operation;
            }
        }
        observe(dialect, &mut steps, &late);
        let held = late
            .into_iter()
            .map(|(late, range)| HeldTable {
                table: late.table,
                generated: range
                    .clone()
                    .filter_map(|at| {
                        if let Change::AddComputedColumn { computed, .. } = &steps[at].change.change
                        {
                            Some(computed.expression.clone())
                        } else {
                            None
                        }
                    })
                    .collect(),
                steps: range,
            })
            .collect::<Vec<_>>();
        let input = ScratchBlueprint(Input::new(
            prepared,
            steps,
            held,
            functions,
            BTreeSet::new(),
        ));
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

/// The table, bare, then what waits for the modules.
fn table_steps(
    name: &TableName,
    table: &pbps_model::Table,
    original: &Change,
    steps: &mut Vec<Draft>,
    late: &mut Vec<(Late, std::ops::Range<usize>)>,
) -> Result<(), ReconstructError> {
    let Change::CreateTable { uid, .. } = original else {
        unreachable!("the caller matched a CreateTable");
    };
    let declaration = format!("table {}", name.shown());
    let mut bare = table.clone();
    let computed = std::mem::take(&mut bare.computed);
    let checks = std::mem::take(&mut bare.checks);
    let keys = std::mem::take(&mut bare.foreign_keys);
    let over_computed = |columns: Vec<&str>| {
        columns
            .iter()
            .any(|column| computed.keys().any(|c| c.raw() == *column))
    };
    let mut unique = Vec::new();
    for (n, u) in std::mem::take(&mut bare.unique) {
        if over_computed(u.columns.iter().map(|c| c.raw()).collect()) {
            let clustered = table.unique_is_clustered(n.raw());
            unique.push((n, u, clustered));
        } else {
            bare.unique.insert(n, u);
        }
    }
    let mut indexes = Vec::new();
    for (n, i) in std::mem::take(&mut bare.indexes) {
        let keyed = i.column_keys();
        // An expression key or a filter can name a computed column too.
        let over = i.holds_expression()
            || over_computed(
                keyed
                    .iter()
                    .flatten()
                    .chain(&i.include)
                    .map(|c| c.raw())
                    .collect(),
            );
        if over && !computed.is_empty() {
            let clustered = table.index_is_clustered(n.raw());
            indexes.push((n, i, clustered));
        } else {
            bare.indexes.insert(n, i);
        }
    }
    let mut defaults = Vec::new();
    for (column, spec) in &mut bare.columns {
        if let ValueSource::Default(default) = &mut spec.value_source {
            defaults.push((column.clone(), std::mem::take(default)));
            spec.value_source = ValueSource::None;
        }
    }
    steps.push(part(
        Phase::Tables,
        &declaration,
        Change::CreateTable {
            uid: uid.clone(),
            name: name.clone(),
            table: Box::new(bare),
            beside_default: None,
            moves_at_most: None,
        },
        vec![name.name.clone()],
        vec![object(&name.schema, &name.name)],
    )?);
    for (key, constraint) in keys {
        steps.push(part(
            Phase::Keys,
            &declaration,
            Change::AddForeignKey {
                table: name.clone(),
                name: key,
                constraint: Box::new(constraint),
            },
            Vec::new(),
            Vec::new(),
        )?);
    }
    // The held part: each computed column, then what stands over it. Phase
    // Tables, so that without a declared callee the column is added right
    // after its table, before any module can read it.
    let held_from = steps.len();
    for (column, expression) in &computed {
        steps.push(part(
            Phase::Tables,
            &format!("computed column {}", name.column(column.clone()).shown()),
            Change::AddComputedColumn {
                table: name.clone(),
                name: column.clone(),
                computed: expression.clone(),
            },
            Vec::new(),
            Vec::new(),
        )?);
    }
    for (n, constraint, clustered) in unique {
        steps.push(part(
            Phase::Keys,
            &format!("unique {} on {}", n.shown(), name.shown()),
            Change::AddUnique {
                table: name.clone(),
                name: n,
                constraint,
                clustered,
            },
            Vec::new(),
            Vec::new(),
        )?);
    }
    for (n, index, clustered) in indexes {
        steps.push(part(
            Phase::Keys,
            &format!("index {} on {}", n.shown(), name.shown()),
            Change::AddIndex {
                table: name.clone(),
                name: n,
                index: Box::new(index),
                clustered,
            },
            Vec::new(),
            Vec::new(),
        )?);
    }
    if !computed.is_empty() {
        late.push((
            Late {
                table: name.clone(),
                computed: computed.keys().map(|c| c.raw().to_owned()).collect(),
            },
            held_from..steps.len(),
        ));
    }
    for (column, default) in defaults {
        steps.push(part(
            Phase::Expressions,
            &format!("the default of {}", name.column(column.clone()).shown()),
            Change::AlterColumnDefault {
                uid: uid.clone(),
                column: pbps_model::ColumnRef::new(name.clone(), column),
                from: None,
                to: Some(default),
            },
            Vec::new(),
            Vec::new(),
        )?);
    }
    for (check, constraint) in checks {
        steps.push(part(
            Phase::Expressions,
            &format!("check {} on {}", check.shown(), name.shown()),
            Change::AddCheck {
                table: name.clone(),
                name: check,
                constraint,
            },
            Vec::new(),
            Vec::new(),
        )?);
    }
    Ok(())
}

/// What each part may name, by the same lexical test the ordinary planner
/// uses: a declared name it may call, and a table whose computed column it
/// may read. A held table is read by a part naming it and either one of the
/// held columns or every column at once (`*`, `alias.*`) in a module whose
/// creation binds its column list: a view or an inline table-valued function
/// (measured on 17.0: a view `SELECT * FROM t` created before `ALTER TABLE ..
/// ADD c` never exposes `c`, and a view over it naming `c` fails 207, though
/// the same two views compile with the column first). A schema-bound
/// function that reads the table's other columns is no reader (measured: it
/// compiles before the column is added), and neither is a procedure, a
/// trigger or a scalar function, whose body binds when it runs.
///
/// The wildcard rule cannot make a cycle that refuses a valid order. A cycle
/// needs a reader that is also a callee of the held column, and a column
/// calls scalar functions only, which cannot list columns; so a function
/// the held expressions call is never taken for a wildcard reader (measured:
/// a scalar function with `EXISTS (SELECT * FROM t)` compiles before the
/// column). What remains is the refusal every reader already carries: a
/// callee that names a reader module is refused by name as a cycle, which
/// is never a wrong order.
fn observe(dialect: &Mssql, steps: &mut [Draft], late: &[(Late, std::ops::Range<usize>)]) {
    let names: BTreeSet<String> = steps
        .iter()
        .flat_map(|s| s.names.iter().cloned())
        .chain(late.iter().map(|(l, _)| l.table.name.clone()))
        .collect();
    let held: Vec<Vec<String>> = late
        .iter()
        .map(|(_, range)| {
            range
                .clone()
                .filter_map(|at| {
                    if let Change::AddComputedColumn { computed, .. } = &steps[at].change.change {
                        Some(computed.expression.clone())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect();
    for part in steps {
        let statements = crate::emit::emit(&part.change.change, part.change.strategy)
            .expect("this part's private validation probe already succeeded");
        let names_text = |name: &str| {
            statements
                .iter()
                .any(|s| dialect.generation_may_call(&s.sql, name))
        };
        // Only a module whose creation binds a column list can see `*`.
        let lists_columns = matches!(
            &part.change.change,
            Change::CreateModule { module, .. }
                if matches!(module.kind, ModuleKind::View | ModuleKind::Function)
        );
        let callee_name = if let Change::CreateModule { id, module } = &part.change.change
            && module.kind == ModuleKind::Function
        {
            Some(id.name())
        } else {
            None
        };
        let wildcard = lists_columns
            && statements
                .iter()
                .any(|s| selects_all(&dialect.code_only(&s.sql)));
        part.calls = names.iter().filter(|n| names_text(n)).cloned().collect();
        part.tables = late
            .iter()
            .zip(&held)
            .filter(|((l, _), expressions)| {
                let called = callee_name.is_some_and(|name| {
                    expressions
                        .iter()
                        .any(|expression| dialect.generation_may_call(expression, name))
                });
                names_text(&l.table.name)
                    && (l.computed.iter().any(|c| names_text(c)) || (wildcard && !called))
            })
            .map(|((l, _), _)| l.table.clone())
            .collect();
    }
}

/// Whether `code`, a definition with strings and comments blanked
/// (`Dialect::code_only`), projects every column: a `*` that ends a select
/// list or stands before a comma (`SELECT *`, `TOP 5 * FROM`, `*, a`), or
/// follows `alias.` or `SELECT`/`DISTINCT`/`ALL`. `COUNT(*)` and a
/// multiplication are none of these: their `*` is followed by `)` or an
/// operand, and preceded by `(` or an operand.
fn selects_all(code: &str) -> bool {
    // The identifier next to `*`, reading outward from it.
    let word = |chars: &mut dyn Iterator<Item = char>| -> String {
        chars
            .skip_while(|c| c.is_whitespace())
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$'))
            .collect()
    };
    let chars: Vec<char> = code.chars().collect();
    (0..chars.len()).filter(|&at| chars[at] == '*').any(|at| {
        let before = chars[..at].iter().rev().find(|c| !c.is_whitespace());
        let next = chars[at + 1..].iter().find(|c| !c.is_whitespace());
        let after = word(&mut chars[at + 1..].iter().copied());
        let preceding: String = word(&mut chars[..at].iter().rev().copied())
            .chars()
            .rev()
            .collect();
        before == Some(&'.')
            || next == Some(&',')
            || after.eq_ignore_ascii_case("FROM")
            || ["SELECT", "DISTINCT", "ALL"]
                .iter()
                .any(|keyword| preceding.eq_ignore_ascii_case(keyword))
    })
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
