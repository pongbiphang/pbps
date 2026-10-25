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
//! must follow the functions it calls: PostgreSQL's "late table" question
//! over a smaller unit, the one computed column, never the table (SQL Server
//! may create it before the function that reads it) and not a table's
//! columns as a group. A key, index or primary key is held with the last
//! computed column it names and follows the others; a foreign key with
//! either end on a computed column waits for every module.
//!
//! Per column is sound because columns have no order among themselves (a
//! computed column may not use another, 1759, measured on 17.0): a column's
//! only constraints are the functions it calls before it and the modules that
//! read it after. A group adds every column's constraints to each: with
//! `d AS (id + 1)` and `c AS (f(id))` where `f` reads `d`, it calls `f` a
//! reader of what must follow `f`, though bare table, `d`, `f`, `c` compiles.
//! A cycle that remains is real: an existing table's column is bound when a
//! module is created (measured for scalar functions, schema-bound ones and
//! views), so `f` reading `c` while `c` calls `f` has no order.
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
        "scratch cannot order {0}: a computed column and a declared function each need the other"
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
    /// The computed columns a key or index over them waits for, by the id
    /// each column's step makes resolvable.
    follows: Vec<String>,
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

/// One computed column, and what stands over it, held until the functions it
/// calls exist. `id` stands for the column wherever the scratch route knows a
/// table: the column's own name made resolvable by its step and read by the
/// parts that name it.
struct Late {
    id: TableName,
    table: TableName,
    column: String,
    expression: String,
}

fn column_id(table: &TableName, column: &str) -> TableName {
    TableName::new(table.schema.clone(), format!("{}.{column}", table.name))
}

type Draft = Part<Metadata>;

/// Each table's computed columns, by table: a column that exists only once
/// its `ADD` has run, so a key or constraint that names one cannot be in the
/// `CREATE` that precedes it, nor before a key it references is built.
type Computed = BTreeMap<TableName, BTreeSet<String>>;

/// Whether `columns` of `table` name a computed column.
fn names_computed(
    computed: &Computed,
    table: &TableName,
    columns: &[pbps_model::ColumnName],
) -> bool {
    computed
        .get(table)
        .is_some_and(|held| columns.iter().any(|c| held.contains(c.raw())))
}

/// A foreign key waits for every module when either end is a computed
/// column: its own columns exist after their `ADD`, and the key it references
/// is a key over a computed column, built after that column. Measured on
/// 17.0: the key before either exists fails 1769 and 1776. Every other key
/// stays with the tables, which is where a module that reads it expects it.
fn foreign_key_phase(computed: &Computed, table: &TableName, fk: &pbps_model::ForeignKey) -> Phase {
    if names_computed(computed, table, &fk.columns)
        || names_computed(computed, &fk.references_table, &fk.references_columns)
    {
        Phase::Expressions
    } else {
        Phase::Keys
    }
}

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
            follows: Vec::new(),
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
        let computed: Computed = prepared
            .changes()
            .changes
            .iter()
            .filter_map(|planned| match &planned.change {
                Change::CreateTable { name, table, .. } if !table.computed.is_empty() => Some((
                    name.clone(),
                    table.computed.keys().map(|c| c.raw().to_owned()).collect(),
                )),
                _ => None,
            })
            .collect();
        let mut steps: Vec<Draft> = Vec::new();
        let mut late: Vec<(Late, std::ops::Range<usize>)> = Vec::new();
        let mut functions: Vec<(usize, String)> = Vec::new();
        for (operation, planned) in prepared.changes().changes.iter().enumerate() {
            let change = &planned.change;
            let first = steps.len();
            match change {
                Change::CreateTable { name, table, .. } => {
                    table_steps(name, table, change, &computed, &mut steps, &mut late)?;
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
                Change::AddForeignKey {
                    table, constraint, ..
                } => steps.push(part(
                    foreign_key_phase(&computed, table, constraint),
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
                table: late.id,
                generated: vec![late.expression],
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
    all_computed: &Computed,
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
    // The computed columns, by position, that `columns` name.
    let naming = |columns: Vec<&str>| -> Vec<usize> {
        computed
            .keys()
            .enumerate()
            .filter(|(_, c)| columns.contains(&c.raw()))
            .map(|(at, _)| at)
            .collect()
    };
    // What cannot be in the first `CREATE` because it names a computed
    // column that does not exist yet (the primary key: 1911, measured), each
    // with the columns it names, to be added under the name and layout it is
    // declared with.
    let mut over: Vec<(Vec<usize>, Draft)> = Vec::new();
    let mut keep = |named: Vec<usize>, subject: String, change: Change| {
        part(Phase::Keys, &subject, change, Vec::new(), Vec::new())
            .map(|draft| over.push((named, draft)))
    };
    let nonclustered = !table.primary_key_is_clustered();
    if let Some(pk) = bare
        .primary_key
        .take_if(|pk| !naming(pk.columns.iter().map(|c| c.raw()).collect()).is_empty())
    {
        keep(
            naming(pk.columns.iter().map(|c| c.raw()).collect()),
            format!("primary key of {}", name.shown()),
            Change::SetPrimaryKey {
                table: name.clone(),
                from: None,
                to: Some(pk),
                nonclustered,
            },
        )?;
    }
    for (n, constraint) in std::mem::take(&mut bare.unique) {
        let named = naming(constraint.columns.iter().map(|c| c.raw()).collect());
        if named.is_empty() {
            bare.unique.insert(n, constraint);
            continue;
        }
        keep(
            named,
            format!("unique {} on {}", n.shown(), name.shown()),
            Change::AddUnique {
                table: name.clone(),
                clustered: table.unique_is_clustered(n.raw()),
                name: n,
                constraint,
            },
        )?;
    }
    for (n, index) in std::mem::take(&mut bare.indexes) {
        // An expression key or a filter can name any computed column.
        let named = if index.holds_expression() {
            (0..computed.len()).collect()
        } else {
            naming(
                index
                    .column_keys()
                    .iter()
                    .flatten()
                    .chain(&index.include)
                    .map(|c| c.raw())
                    .collect(),
            )
        };
        if named.is_empty() {
            bare.indexes.insert(n, index);
            continue;
        }
        keep(
            named,
            format!("index {} on {}", n.shown(), name.shown()),
            Change::AddIndex {
                table: name.clone(),
                clustered: table.index_is_clustered(n.raw()),
                name: n,
                index: Box::new(index),
            },
        )?;
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
            foreign_key_phase(all_computed, name, &constraint),
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
    // Each computed column is its own held unit, then the keys that end at it.
    // Phase Tables, so that without a declared callee the column is added
    // right after its table, before any module can read it.
    let ids: Vec<TableName> = computed.keys().map(|c| column_id(name, c.raw())).collect();
    for (at, (column, expression)) in computed.iter().enumerate() {
        let from = steps.len();
        steps.push(part(
            Phase::Tables,
            &format!("computed column {}", name.column(column.clone()).shown()),
            Change::AddComputedColumn {
                table: name.clone(),
                name: column.clone(),
                computed: expression.clone(),
            },
            vec![ids[at].name.clone()],
            Vec::new(),
        )?);
        let (mine, rest) = std::mem::take(&mut over)
            .into_iter()
            .partition(|(named, _)| named.last() == Some(&at));
        over = rest;
        for (named, mut dependent) in mine {
            dependent.metadata.follows = named.iter().map(|&n| ids[n].name.clone()).collect();
            steps.push(dependent);
        }
        late.push((
            Late {
                id: ids[at].clone(),
                table: name.clone(),
                column: column.raw().to_owned(),
                expression: expression.expression.clone(),
            },
            from..steps.len(),
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
/// uses: a declared name it may call, and the computed columns it may read.
/// A part reads a column by naming its table and the column (any kind of
/// module: the column is bound when it is created), or its table and every
/// column at once (`*`, `alias.*`) in a view or an inline table-valued
/// function, whose creation binds the column list (measured on 17.0: a view
/// `SELECT * FROM t` created before `ADD c` never exposes `c`, and a view
/// over it naming `c` fails 207). A procedure or a trigger expands its `*`
/// when it runs, and a scalar function's `EXISTS (SELECT * ..)` projects no
/// list, so a function any column of the table calls (necessarily scalar) is
/// never a wildcard reader: that rule cannot refuse a valid order.
fn observe(dialect: &Mssql, steps: &mut [Draft], late: &[(Late, std::ops::Range<usize>)]) {
    let ids: BTreeSet<&String> = late.iter().map(|(l, _)| &l.id.name).collect();
    let names: BTreeSet<String> = steps
        .iter()
        .flat_map(|s| s.names.iter())
        .filter(|name| !ids.contains(name))
        .cloned()
        .chain(late.iter().map(|(l, _)| l.table.name.clone()))
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
        part.calls = names
            .iter()
            .filter(|n| names_text(n))
            .cloned()
            .chain(part.metadata.follows.iter().cloned())
            .collect();
        part.tables = late
            .iter()
            .filter(|(l, _)| {
                let called = callee_name.is_some_and(|name| {
                    late.iter().any(|(other, _)| {
                        other.table == l.table
                            && dialect.generation_may_call(&other.expression, name)
                    })
                });
                names_text(&l.table.name) && (names_text(&l.column) || (wildcard && !called))
            })
            .map(|(l, _)| l.id.clone())
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
