//! The desired managed namespace, compiled on scratch (ADR-0016 decision 2;
//! issue #613).
//!
//! The statements are the emitter's own: a fresh bootstrap of the declared
//! schema, so what scratch binds is what these declarations bind when they are
//! created. This module supplies one part per change and nothing else; the
//! scheduler owns their order (DEC-2389.1). The ordinary plan order puts
//! tables, with their defaults, checks and indexes, ahead of every module,
//! and an expression calling a managed function then either fails or binds
//! whatever else of that name exists already — the very answer this analysis
//! must not get wrong. So a table is created with what holds no expression,
//! and each default, check, key and index that does is a part of its own,
//! ordered after the routines its text names (and an index that holds an
//! expression after every module).
//!
//! Between modules the emitter's order stands: the name scan that already
//! orders a plan's modules (`creation_order_with`). A scan can miss a
//! reference, and a module compiled before a same-named object it would have
//! preferred binds something else without complaint. That cannot be
//! prevented here without parsing SQL, so it is detected afterwards from what
//! the engine actually bound: [`Reconstruction::later_names`] is what the
//! assessment checks each module's bindings against. Scratch order is only
//! how the namespace is built; it decides nothing about deployment order.
//!
//! Grants, roles and rows are not reproduced: none of them changes what a
//! name binds to. What does — the deployer's schema privileges and path — is
//! reproduced by the analysis scope before this runs (#610). Row-security
//! setup also stays in the deployment plan: it creates no nameable object,
//! and policy predicate bindings remain #595's named coverage limitation.

use pbps_db::resolver::capture::ObjectIdentity;
use pbps_dialect::Dialect;
use pbps_diff::order::scratch::Part;
use pbps_model::ValueSource;
use pbps_model::{Change, ModuleId, ModuleKind};
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
    /// A catalog read around a step failed: nothing was learned about the
    /// declaration, so it is not a [`ReconstructError::Compile`] verdict
    /// (SPEC 9.8, #1575).
    #[error("reading back what compiling {declaration} made failed: {reason}")]
    Read { declaration: String, reason: String },
    /// A compile statement failed for a reason about the server or the
    /// connection rather than the statement ([`judged_the_statement`]): no
    /// verdict on the declaration either (SPEC 9.8, #1597).
    #[error("scratch compilation of {declaration} was interrupted: {reason}")]
    Interrupted { declaration: String, reason: String },
    #[error("scratch compilation could not {0} its transaction")]
    Transaction(&'static str),
    /// The scratch server does not know the target's time zone, so nothing it
    /// compiles would read an offset-less value as the target does
    /// (DEC-1916.1).
    #[error("the scratch server cannot use the target's time zone {zone:?}: {reason}")]
    Zone { zone: String, reason: String },
    #[error(
        "scratch cannot order the declarations: {0}. Each of them names another when the \
         engine creates it (a generated column calling a function that reads the table, \
         routines resolved at creation calling one another), so no order compiles them"
    )]
    Cycle(String),
}

/// What kind of object a step makes nameable, and so which bindings it
/// could have taken over had it existed earlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Nameable {
    /// A table or view: a relation, and a row type of the same name, which
    /// can also turn a call `t(x)` without an exact match into a cast.
    Relation,
    /// An index: a relation with no row type.
    Index,
    /// A function or procedure. A call written `t(x)` is also how a type
    /// named `t` is cast to, so a routine can take a type's place too.
    Routine,
    /// A type alone, such as a relation's generated array type: never a
    /// relation, and a call only as a cast.
    Type,
}

impl Nameable {
    /// Whether an object of this kind could be what a binding of the
    /// captured catalog `class` resolves to instead. A row type takes a
    /// routine call only as a cast, so only when a one-argument call can
    /// reach the routine (`one_argument`).
    pub fn shadows(self, class: &str, one_argument: bool) -> bool {
        match (self, class) {
            (Self::Relation, "pg_proc") => one_argument,
            (Self::Relation, class) => matches!(class, "pg_class" | "pg_type"),
            (Self::Index, class) => class == "pg_class",
            (Self::Routine, class) => matches!(class, "pg_proc" | "pg_type"),
            (Self::Type, "pg_proc") => one_argument,
            (Self::Type, class) => class == "pg_type",
        }
    }
}

/// What a step, or a fill between two steps, makes resolvable: its kind,
/// schema and name.
pub type Named = (Nameable, String, String);

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct Metadata {
    table_part: Option<pbps_model::TableName>,
    /// What an error names: the declaration, never the statement text.
    declaration: String,
    /// What this step makes resolvable, for the order check: its kind,
    /// schema and name.
    names: Vec<(Nameable, String, String)>,
    /// The columns whose generated expressions this step compiles: a
    /// table's, in its `CREATE TABLE`.
    generated: Vec<pbps_model::ColumnName>,
    /// The module this step compiles, if it is one.
    module: Option<ModuleId>,
    /// The routine the engine created for this step, as the capture names
    /// it. A routine's declared signature is type spellings, not catalog
    /// identities, so the step's own object is read back after it compiles:
    /// the overload the order check measures from must be this one, not the
    /// first of its name.
    created: Option<ObjectIdentity>,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
struct Step {
    metadata: Metadata,
    #[cfg(test)]
    statements: Vec<String>,
}
impl std::ops::Deref for Step {
    type Target = Metadata;
    fn deref(&self) -> &Metadata {
        &self.metadata
    }
}
impl std::ops::DerefMut for Step {
    fn deref_mut(&mut self) -> &mut Metadata {
        &mut self.metadata
    }
}

/// The ordered scratch statements for one desired schema.
#[derive(Debug, Clone)]
pub struct Reconstruction {
    steps: Vec<Step>,
    lowered: pbps_dialect::lower::LoweredPlan<ScratchEmitter>,
    /// Declared routines the plan drops, which are never compiled, and the
    /// catalog identity each declared signature names in the target
    /// capture's snapshot: `None` where it names none (#1063, #1124, #1148).
    dropped: BTreeMap<ModuleId, Option<ObjectIdentity>>,
    /// Names something other than a step made resolvable just before the
    /// step at each position: a fill's chained shapes (#1778).
    between: Vec<(usize, Vec<Named>)>,
}

impl Reconstruction {
    /// `bootstrap` is the plan from an empty schema to the desired one, as
    /// the differ produces it; its order among modules is kept.
    ///
    /// ```compile_fail
    /// fn raw(pg: &pbps_pg::Postgres, changes: &[pbps_model::Change], ids: &pbps_model::IdsFile) {
    ///     pbps_pg::resolver::reconstruct::Reconstruction::new(pg, changes, ids);
    /// }
    /// ```
    pub fn new<S: pbps_diff::order::RouteState<crate::Postgres>>(
        dialect: &crate::Postgres,
        bootstrap: &pbps_diff::order::PlanningPlan<S>,
        ids: &pbps_model::IdsFile,
    ) -> Result<Self, ReconstructError> {
        Self::with_dependencies(dialect, bootstrap, ids, &pbps_model::ModuleDeps::new())
    }

    #[allow(clippy::wildcard_enum_match_arm)]
    pub fn with_dependencies<S: pbps_diff::order::RouteState<crate::Postgres>>(
        dialect: &crate::Postgres,
        bootstrap: &pbps_diff::order::PlanningPlan<S>,
        ids: &pbps_model::IdsFile,
        dependencies: &pbps_model::ModuleDeps,
    ) -> Result<Self, ReconstructError> {
        let prepared = pbps_diff::order::scratch::prepare(bootstrap, ids, dependencies, dialect)
            .map_err(ReconstructError::Cycle)?;
        let expanded = prepared.changes();
        let mut steps = Vec::new();
        for (operation, planned) in expanded.changes.iter().enumerate() {
            let change = &planned.change;
            let is_part = pbps_diff::order::generated::is_part(planned);
            let make = |declaration: &str,
                        names: Vec<(Nameable, String, String)>,
                        module: Option<ModuleId>| {
                part(dialect, operation, planned, declaration, names, module)
            };
            match change {
                Change::RebuildPartitionedTable(_)
                | Change::BuildPartitionedIndex(_)
                | Change::CopyTableRows(_)
                | Change::AnalyzeTableTree(_) => {
                    return Err(ReconstructError::Unsupported(format!(
                        "{} for `{}`",
                        change.pending_rebuild().unwrap().0,
                        change.table().unwrap().shown()
                    )));
                }
                Change::AddColumn {
                    table,
                    name,
                    column,
                    ..
                } => {
                    let mut added = make(
                        &format!("column {}", table.column(name).shown()),
                        Vec::new(),
                        None,
                    )?;
                    if matches!(column.value_source, ValueSource::Generated(_)) {
                        added.metadata.generated.push(name.clone());
                    }
                    added.metadata.table_part = Some(table.clone());
                    steps.push(added);
                }
                Change::AlterColumnExpression { column, .. } => {
                    let mut installed = make(
                        &format!("generation of {}", column.shown()),
                        Vec::new(),
                        None,
                    )?;
                    installed.metadata.generated.push(column.name.clone());
                    installed.metadata.table_part = Some(column.table.clone());
                    steps.push(installed);
                }
                // A bootstrap starts from nothing, so no partition is made
                // beside a standing DEFAULT (#1547).
                Change::CreateTable { name, table, .. } => {
                    // The indexes it keeps are made with it: a relation each.
                    let mut names = relation(&name.schema, &name.name);
                    names.extend(table.indexes.keys().map(|index| {
                        (Nameable::Index, name.schema.clone(), index.raw().to_owned())
                    }));
                    let mut created =
                        make(&format!("table {name}", name = name.shown()), names, None)?;
                    // Binding checks belong to the real expression's step,
                    // never to a temporary typed-NULL placeholder.
                    created.metadata.generated = table
                        .columns
                        .iter()
                        .filter(|(column, spec)| {
                            matches!(spec.value_source, ValueSource::Generated(_))
                                && !expanded.changes.iter().any(|p| {
                                    matches!(&p.change,
                                    Change::AlterColumnExpression { column: installed, .. }
                                        if installed == &name.column(*column))
                                })
                        })
                        .map(|(column, _)| column.clone())
                        .collect();
                    steps.push(created);
                }
                // An index is a relation: it can shadow a name a module
                // bound, so its name joins the order check (#1042).
                Change::AddIndex { table, name, .. } if is_part => {
                    steps.push(make(
                        &format!("index {name} on {}", table.shown(), name = name.shown()),
                        vec![(Nameable::Index, table.schema.clone(), name.raw().to_owned())],
                        None,
                    )?);
                }
                Change::AddCheck { table, name, .. } if is_part => {
                    steps.push(make(
                        &format!(
                            "check {check} on {table}",
                            check = name.shown(),
                            table = table.shown()
                        ),
                        Vec::new(),
                        None,
                    )?);
                }
                Change::AlterColumnDefault { column, .. } if is_part => {
                    steps.push(make(
                        &format!("the default of {}", column.shown()),
                        Vec::new(),
                        None,
                    )?);
                }
                Change::SetReplicaIdentity { table, .. } if is_part => {
                    steps.push(make(
                        &format!("replica identity of {table}", table = table.shown()),
                        Vec::new(),
                        None,
                    )?);
                }
                Change::SetPrimaryKey { .. } | Change::AddUnique { .. } if is_part => {
                    steps.push(make(
                        &pbps_model::escape::Escaped(&change.subject()).to_string(),
                        Vec::new(),
                        None,
                    )?);
                }
                // The differ splits every new table's foreign keys out of its
                // CREATE, so a bootstrap of related tables carries them as
                // their own changes, and so does the split of a table that
                // has them inline.
                Change::AddForeignKey { table, .. } => {
                    steps.push(make(
                        &format!("table {table}", table = table.shown()),
                        Vec::new(),
                        None,
                    )?);
                }
                // A declared enum or domain precedes every part that names it,
                // by a constraint. A bootstrap holds only creations of them, so
                // the label operations and the drops stay refused.
                Change::CreateType { name, .. } => {
                    steps.push(make(
                        &format!("type {name}", name = name.shown()),
                        vec![(
                            Nameable::Type,
                            name.schema().to_owned(),
                            name.name().to_owned(),
                        )],
                        None,
                    )?);
                }
                Change::CreateModule { id, module } => {
                    // A trigger is named by nothing and names its function
                    // with an exact signature.
                    let (names, compiled) = if module.kind == ModuleKind::Trigger {
                        (Vec::new(), None)
                    } else {
                        let names = if matches!(id, ModuleId::Routine(_)) {
                            vec![(
                                Nameable::Routine,
                                id.schema().to_owned(),
                                id.name().to_owned(),
                            )]
                        } else {
                            relation(id.schema(), id.name())
                        };
                        (names, Some(id.clone()))
                    };
                    steps.push(make(
                        &format!("{} {id}", module.kind, id = id.shown()),
                        names,
                        compiled,
                    )?);
                }
                // Policies create no nameable object, and their predicates
                // are not resolver surfaces (#595). Installing them here
                // would also require recipient roles scratch need not hold.
                // The typed deployment plan still emits policies and flags.
                Change::DropPolicy { .. }
                | Change::CreatePolicy { .. }
                | Change::SetRowSecurity { .. } => {}
                // None of these changes what a name resolves to.
                Change::CreateRole { .. }
                | Change::Grant { .. }
                | Change::PublicExecution { .. }
                | Change::InsertRow { .. }
                | Change::SetDataMode { .. } => {}
                Change::SetEnsureKeys { .. } => {}
                // A bootstrap from an empty schema has no renames, drops or
                // alterations; one that does is not a bootstrap.
                Change::AddEnumValue { .. }
                | Change::RenameEnumValue { .. }
                | Change::EnumRebuild { .. }
                | Change::DropType { .. }
                | Change::RenameType { .. }
                | Change::DropTable { .. }
                | Change::DetachPartition { .. }
                | Change::AttachPartition { .. }
                | Change::RenameTable { .. }
                | Change::DropColumn { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnType { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::AlterColumnNumbering { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::SetIndexStorageParameters { .. }
                | Change::SetTablePersistence { .. }
                | Change::SetPartitionDefault { .. }
                | Change::SetPartitionNotNull { .. }
                | Change::SetStorageParameters { .. }
                | Change::SetReplicaIdentity { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::DropForeignKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
                | Change::AddComputedColumn { .. }
                | Change::DropComputedColumn { .. }
                | Change::DropIndex { .. }
                | Change::UpdateRow { .. }
                | Change::DeleteRow { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Revoke { .. } => {
                    return Err(ReconstructError::Unsupported(
                        serde_json::to_value(change)
                            .ok()
                            .and_then(|op| op["op"].as_str().map(str::to_owned))
                            .unwrap_or_default(),
                    ));
                }
            }
        }
        let input = ScratchBlueprint(pbps_diff::order::scratch::Input::new(prepared, steps));
        let ordered =
            pbps_diff::order::scratch::settle(dialect, input).map_err(ReconstructError::Cycle)?;
        let lowered = pbps_dialect::lower::lower_all(&ordered, &ScratchEmitter(dialect.clone()))
            .expect("the identical private probes already validated these lowering parts");
        let steps = ordered
            .parts()
            .iter()
            .zip(lowered.groups())
            .map(|(part, group)| {
                let _ = group;
                Step {
                    metadata: part.metadata.clone(),
                    #[cfg(test)]
                    statements: group.statements().iter().map(|s| s.sql.clone()).collect(),
                }
            })
            .collect::<Vec<_>>();

        Ok(Self {
            steps,
            lowered,
            dropped: BTreeMap::new(),
            between: Vec::new(),
        })
    }

    /// The names made resolvable after the module that `owner` belongs to
    /// was compiled: later modules and every index. A binding of that module
    /// to an object of one of these names may have been made before a better
    /// candidate existed. A view is found by its name; a routine by the exact
    /// overload the engine created for its step, since overloads of one name
    /// are compiled at different steps. Names made resolvable between two
    /// steps (see [`Self::created_before`]) count for every module compiled
    /// before them. `None` for an object no compiled module is.
    pub fn later_names(
        &self,
        owner: &ObjectIdentity,
    ) -> Option<std::collections::BTreeSet<(Nameable, &str, &str)>> {
        let at = self
            .steps
            .iter()
            .position(|step| match owner.class.as_str() {
                "pg_proc" => step.created.as_ref() == Some(owner),
                "pg_class" => step.module.as_ref().is_some_and(|id| {
                    matches!(id, ModuleId::Named(_)) && [id.schema(), id.name()] == *owner.name
                }),
                _ => false,
            })?;
        Some(self.names_after(at))
    }

    /// The names made resolvable after the step at `at`: by every later
    /// step, and between steps after it.
    fn names_after(&self, at: usize) -> std::collections::BTreeSet<(Nameable, &str, &str)> {
        self.steps[at + 1..]
            .iter()
            .map(|step| &step.names)
            .chain(
                self.between
                    .iter()
                    .filter(|(before, _)| at < *before)
                    .map(|(_, names)| names),
            )
            .flat_map(|names| {
                names
                    .iter()
                    .map(|(kind, schema, name)| (*kind, schema.as_str(), name.as_str()))
            })
            .collect()
    }

    /// [`Self::later_names`] for the generated expression of `column` on the
    /// table `schema.table`, which compiles in that table's own step: a
    /// table whose expression calls a declared function is created among the
    /// modules, so later modules, and a fill's chained shapes, may come
    /// after it (#1778). `None` where no step compiles that expression, as
    /// for a plain default, which compiles after every module.
    pub fn later_names_generated(
        &self,
        schema: &str,
        table: &str,
        column: &str,
    ) -> Option<std::collections::BTreeSet<(Nameable, &str, &str)>> {
        let at = self.steps.iter().position(|step| {
            step.module.is_none()
                && step
                    .generated
                    .iter()
                    .any(|generated| generated.raw() == column)
                && (step
                    .table_part
                    .as_ref()
                    .is_some_and(|t| t.schema == schema && t.name == table)
                    || step.names.iter().any(|(kind, s, n)| {
                        *kind == Nameable::Relation && s == schema && n == table
                    }))
        })?;
        Some(self.names_after(at))
    }

    /// Records `names` as made resolvable just before the step at
    /// `position`, by something no step compiles: a fill's chained shapes,
    /// created between two parts of the compile (#1778, DEC-1778.2). A module
    /// compiled earlier may have bound one of these names to another object,
    /// and [`Self::later_names`] then says so.
    pub fn created_before(&mut self, position: usize, names: Vec<Named>) {
        self.between.push((position, names));
    }

    /// The catalog identity of the routine compiled for `id`, once the
    /// reconstruction has compiled; `None` for a module it does not compile.
    pub fn created(&self, id: &ModuleId) -> Option<&ObjectIdentity> {
        self.steps
            .iter()
            .find(|step| step.module.as_ref() == Some(id))
            .and_then(|step| step.created.as_ref())
    }

    /// Declared routines the plan drops, with the identity each names on
    /// the target, as the target capture read them in its snapshot.
    pub fn identified(&mut self, dropped: BTreeMap<ModuleId, Option<ObjectIdentity>>) {
        self.dropped = dropped;
    }

    /// The catalog identity a dropped routine's declared signature names, when
    /// every argument type was identified; `None` for one that was not.
    pub fn dropped(&self, id: &ModuleId) -> Option<&ObjectIdentity> {
        self.dropped.get(id).and_then(Option::as_ref)
    }

    /// The identity a declared routine holds on the target: the one its
    /// signature names there if the plan drops it, even where the plan
    /// creates it again under another kind, else the one scratch compiled.
    pub fn known(&self, id: &ModuleId) -> Option<&ObjectIdentity> {
        if self.dropped.contains_key(id) {
            self.dropped(id)
        } else {
            self.created(id)
        }
    }

    /// Runs every step in one transaction under the dialect's session pins,
    /// as whatever role the session currently is — the reproduced deployer,
    /// which the scope has entered. Committed only when all of it compiled;
    /// a failure leaves nothing behind and names its declaration.
    ///
    /// `zone` is the **target's**, not the scratch server's own: a declared
    /// offset-less default compiles to the instant the target would store
    /// (DEC-1916.1). A scratch server that does not know the zone is refused
    /// by name rather than compiled in another one.
    pub async fn compile(
        &mut self,
        dialect: &crate::Postgres,
        zone: &pbps_model::SessionZone,
        conn: &mut impl pbps_db::transport::ExecuteConnection,
    ) -> Result<(), ReconstructError> {
        self.compile_steps_in(dialect, zone, conn, 0..self.steps.len())
            .await
    }

    /// The position just after the last step that creates one of
    /// `relations`, by schema and name: where a fill's chained shapes, which
    /// name those relations' row types, can be created (#1778,
    /// DEC-1778.2). A table whose generated column calls a declared
    /// function comes after that function, so the position is the step's,
    /// not a phase's. `None` when one of them is created by no step.
    pub fn after_creating(&self, relations: &BTreeSet<(String, String)>) -> Option<usize> {
        relations
            .iter()
            .map(|(schema, name)| {
                let created = self.steps.iter().any(|step| {
                    step.names
                        .iter()
                        .any(|(kind, s, n)| *kind == Nameable::Relation && s == schema && n == name)
                });
                created
                    .then(|| {
                        self.steps.iter().rposition(|step| {
                            step.table_part
                                .as_ref()
                                .is_some_and(|t| &t.schema == schema && &t.name == name)
                                || step.names.iter().any(|(kind, s, n)| {
                                    *kind == Nameable::Relation && s == schema && n == name
                                })
                        })
                    })
                    .flatten()
            })
            .try_fold(0, |last, position| Some(last.max(position? + 1)))
    }

    /// The number of steps, for [`Self::compile_steps_in`].
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// [`Self::compile`] for the steps in `range` only, in a transaction of
    /// its own: a fill's chained shapes are created between two of them,
    /// outside the deployer's transaction (#1778, DEC-1778.2).
    pub async fn compile_steps_in(
        &mut self,
        dialect: &crate::Postgres,
        zone: &pbps_model::SessionZone,
        conn: &mut impl pbps_db::transport::ExecuteConnection,
        range: std::ops::Range<usize>,
    ) -> Result<(), ReconstructError> {
        let framing = dialect.transaction_framing(zone);
        conn.execute(&framing.begin).await.map_err(|e| {
            // `invalid_parameter_value`, measured on 18.6 for an unknown
            // `TimeZone`. The zone is the one setting in the pins that is not
            // a constant, so it is the one this can be about.
            if let pbps_db::DbError::Driver {
                code: Some(code), ..
            } = &e
                && code == "22023"
            {
                ReconstructError::Zone {
                    zone: zone.to_string(),
                    reason: e.to_string(),
                }
            } else {
                ReconstructError::Transaction("begin")
            }
        })?;
        let outcome = self.compile_steps(conn, range).await;
        if outcome.is_err() {
            let _ = conn.execute(&framing.rollback).await;
            return outcome;
        }
        conn.execute(&framing.commit)
            .await
            .map_err(|_| ReconstructError::Transaction("commit"))
    }

    async fn compile_steps(
        &mut self,
        conn: &mut impl pbps_db::transport::ExecuteConnection,
        range: std::ops::Range<usize>,
    ) -> Result<(), ReconstructError> {
        for (step, group) in self.steps[range.clone()]
            .iter_mut()
            .zip(&self.lowered.groups()[range])
        {
            let declaration = step.declaration.clone();
            let failed = |reason: String| ReconstructError::Compile {
                declaration: declaration.clone(),
                reason,
            };
            let unread = |reason: String| ReconstructError::Read {
                declaration: declaration.clone(),
                reason,
            };
            let interrupted = |reason: String| ReconstructError::Interrupted {
                declaration: declaration.clone(),
                reason,
            };
            let routine = match &step.module {
                Some(id @ ModuleId::Routine(_)) => {
                    Some((id.schema().to_owned(), id.name().to_owned()))
                }
                Some(ModuleId::Named(_) | ModuleId::Trigger { .. }) | None => None,
            };
            let before = match &routine {
                Some((schema, name)) => routines(conn, schema, name).await.map_err(unread)?,
                None => Vec::new(),
            };
            for statement in group.statements() {
                conn.execute(&statement.sql).await.map_err(|error| {
                    if judged_the_statement(&error) {
                        failed(error.to_string())
                    } else {
                        interrupted(error.to_string())
                    }
                })?;
            }
            let relation = step
                .names
                .iter()
                .find(|(kind, _, _)| *kind == Nameable::Relation)
                .map(|(_, schema, name)| (schema.clone(), name.clone()));
            if let Some((schema, name)) = relation
                && let Some((array_schema, array)) =
                    array_type(conn, &schema, &name).await.map_err(unread)?
            {
                step.names.push((Nameable::Type, array_schema, array));
            }
            if let Some((schema, name)) = &routine {
                let mut after = routines(conn, schema, name).await.map_err(unread)?;
                after.retain(|created| !before.contains(created));
                let [created] = after.as_slice() else {
                    return Err(failed(
                        "the routine it created could not be told from its overloads".into(),
                    ));
                };
                step.created = Some(created.clone());
            }
        }
        Ok(())
    }
}

/// Whether a failed compile statement is the server's verdict on it, and so
/// on its declaration (#1597).
///
/// Measured on 18: a backend terminated mid-statement fails it with `57P01`,
/// and every later statement on that connection fails with no code at all
/// (`connection closed`).
/// - A failure with no SQLSTATE never reached the server's judgement.
/// - These classes describe the server's state, not the statement
///   (PostgreSQL's SQLSTATE appendix): `08` connection exception, `40`
///   transaction rollback, `53` insufficient resources, `57` operator
///   intervention, `58` system error, and `XX` internal error.
/// - So do the two session timeouts that end the connection: `25P03`
///   idle-in-transaction and `25P04` transaction timeout (measured on 18:
///   `terminating connection due to transaction timeout`, mid-statement).
///   The rest of class `25`, such as `25001` for a statement that cannot
///   run in a transaction block, is the statement's.
///
/// Every other class, such as `42` syntax or access rule, `0A` not
/// supported or `22` data exception, is the statement's.
pub(crate) fn judged_the_statement(error: &pbps_db::DbError) -> bool {
    error.server_error_code().is_some_and(|code| {
        !matches!(code.as_str(), "25P03" | "25P04")
            && !matches!(
                code.get(..2),
                Some("08" | "40" | "53" | "57" | "58" | "XX") | None
            )
    })
}

/// A dropped routine's declared signature, spelled as the plan's `DROP` spells
/// it and with the write path that `DROP` runs under, for the target capture
/// to identify in its own snapshot (#1124, #1148). `None` for a module that is
/// not a function or procedure; an error for a name or path the emitter
/// cannot write, which is a `DROP` no plan contains. Only the declared kind is
/// that routine: the plan's `DROP FUNCTION` refuses an aggregate (#1126).
pub fn dropped_signature(
    dialect: &crate::Postgres,
    id: &ModuleId,
    kind: ModuleKind,
) -> Result<Option<crate::resolver::capture::DroppedSignature>, ReconstructError> {
    let ModuleId::Routine(routine) = id else {
        return Ok(None);
    };
    let kind = match kind {
        ModuleKind::Function => "f",
        ModuleKind::Procedure => "p",
        ModuleKind::View | ModuleKind::Trigger => return Ok(None),
    };
    let unwritable = |error: pbps_dialect::DialectError| ReconstructError::Emit {
        declaration: id.shown().to_string(),
        reason: error.to_string(),
    };
    Ok(Some(crate::resolver::capture::DroppedSignature {
        spelled: format!(
            "{}.{}({})",
            dialect
                .quote_ident(&routine.name.schema)
                .map_err(unwritable)?,
            dialect
                .quote_ident(&routine.name.name)
                .map_err(unwritable)?,
            routine
                .args
                .iter()
                .map(pbps_model::RoutineArg::raw)
                .collect::<Vec<_>>()
                .join(",")
        ),
        path: crate::emit::write_path(dialect, &routine.name.schema).map_err(unwritable)?,
        kind,
    }))
}

/// Every routine of one schema-qualified name, named as the capture names
/// them: schema and name, then each argument type by schema and name.
async fn routines(
    conn: &mut impl pbps_db::transport::ExecuteConnection,
    schema: &str,
    name: &str,
) -> Result<Vec<ObjectIdentity>, String> {
    let literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let rows = conn
        .query(&format!(
            "SELECT p.proname AS name, COALESCE((SELECT pg_catalog.json_agg(pg_catalog.json_build_array(tn.nspname, t.typname) ORDER BY a.ord) \
             FROM pg_catalog.unnest(p.proargtypes::pg_catalog.oid[]) WITH ORDINALITY AS a(typ, ord) \
             JOIN pg_catalog.pg_type t ON t.oid = a.typ JOIN pg_catalog.pg_namespace tn ON tn.oid = t.typnamespace), '[]')::text AS args \
             FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = {} AND p.proname = {}",
            literal(schema),
            literal(name)
        ))
        .await
        .map_err(|error| error.to_string())?;
    rows.iter()
        .map(|row| {
            let args = row
                .try_get::<&str>("args")
                .ok()
                .flatten()
                .and_then(|args| serde_json::from_str::<Vec<[String; 2]>>(args).ok())
                .ok_or("a created routine's argument types are unreadable")?;
            Ok(ObjectIdentity {
                class: "pg_proc".into(),
                name: vec![schema.to_owned(), name.to_owned()],
                signature: args
                    .into_iter()
                    .map(|type_name| ObjectIdentity {
                        class: "pg_type".into(),
                        name: type_name.into(),
                        signature: Vec::new(),
                    })
                    .collect(),
            })
        })
        .collect()
}

fn relation(schema: &str, name: &str) -> Vec<(Nameable, String, String)> {
    vec![(Nameable::Relation, schema.to_owned(), name.to_owned())]
}

/// The array type the engine generated for a relation's row type, by the
/// name it chose: `_name` usually, but clipped to the identifier limit, or
/// another spelling when that one was taken. `None` when it made none.
async fn array_type(
    conn: &mut impl pbps_db::transport::QueryConnection,
    schema: &str,
    name: &str,
) -> Result<Option<(String, String)>, String> {
    let literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let rows = conn
        .query(&format!(
            "SELECT an.nspname AS schema, a.typname AS name \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_type r ON r.oid = c.reltype \
             JOIN pg_catalog.pg_type a ON a.oid = r.typarray \
             JOIN pg_catalog.pg_namespace an ON an.oid = a.typnamespace \
             WHERE n.nspname = {} AND c.relname = {}",
            literal(schema),
            literal(name)
        ))
        .await
        .map_err(|error| error.to_string())?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => {
            let text = |field| {
                row.try_get::<&str>(field)
                    .ok()
                    .flatten()
                    .map(str::to_owned)
                    .ok_or("a relation's array type is unreadable")
            };
            Ok(Some((text("schema")?, text("name")?)))
        }
        _ => Err("a relation's array type is ambiguous".into()),
    }
}

/// One operation as a part, validated where it is made, so a declaration the
/// emitter cannot write is refused by name before anything is ordered.
fn part(
    dialect: &crate::Postgres,
    operation: usize,
    planned: &pbps_model::PlannedChange,
    declaration: &str,
    names: Vec<(Nameable, String, String)>,
    module: Option<ModuleId>,
) -> Result<Part<Metadata>, ReconstructError> {
    crate::emit::emit(dialect, &planned.change, planned.strategy).map_err(|error| {
        ReconstructError::Emit {
            declaration: declaration.to_owned(),
            reason: error.to_string(),
        }
    })?;
    Ok(Part {
        operation,
        change: planned.clone(),
        metadata: Metadata {
            table_part: None,
            declaration: declaration.to_owned(),
            names,
            generated: Vec::new(),
            module,
            created: None,
        },
    })
}

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn fixture_ids(bootstrap: &[Change]) -> pbps_model::IdsFile {
    let mut schema = pbps_model::Schema::default();
    for change in bootstrap {
        match change {
            Change::CreateTable { name, table, .. } => {
                schema.tables.insert(name.clone(), (**table).clone());
            }
            Change::CreateModule { id, module } => {
                schema.modules.insert(id.clone(), (**module).clone());
            }
            _ => {}
        }
    }
    let mut ids = pbps_diff::resolve(
        &schema,
        &pbps_model::IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "reconstruction-fixture".into(),
            today: "2026-10-19".into(),
        },
    )
    .unwrap()
    .ids;
    for change in bootstrap {
        if let Change::CreateTable { uid, name, .. } = change {
            ids.tables.retain(|_, n| n != name);
            ids.tables.insert(uid.clone(), name.clone());
        }
    }
    ids
}

#[cfg(test)]
mod function_tests;
#[cfg(test)]
mod rule_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn a_dropped_signature_keeps_raw_sql_names_and_arguments() {
        let part = "x\n\\\u{200b}";
        let id: ModuleId = format!("app.{part}(s.\"{part}\")").parse().unwrap();
        let found = dropped_signature(&crate::Postgres::new(), &id, ModuleKind::Function)
            .unwrap()
            .unwrap();
        assert_eq!(found.spelled, format!("\"app\".\"{part}\"(s.\"{part}\")"));
        assert_ne!(found.spelled, id.shown().to_string());
        assert!(
            dropped_signature(
                &crate::Postgres::new(),
                &"app.v".parse().unwrap(),
                ModuleKind::View
            )
            .unwrap()
            .is_none()
        );
    }
}

/// Engine-owned scratch input. Only reconstruction can assemble its parts.
///
/// ```compile_fail
/// fn forge(pg: &pbps_pg::Postgres,
///          input: pbps_diff::order::scratch::Input<pbps_pg::resolver::reconstruct::Metadata>) {
///     pbps_diff::order::scratch::settle(pg, input).unwrap();
/// }
/// ```
pub struct ScratchBlueprint(pbps_diff::order::scratch::Input<Metadata>);
impl pbps_diff::order::scratch::Engine for crate::Postgres {
    type Input = ScratchBlueprint;
    type Metadata = Metadata;
    fn blueprint(&self, input: Self::Input) -> pbps_diff::order::scratch::Input<Metadata> {
        input.0
    }
}
#[derive(Debug, Clone)]
struct ScratchEmitter(crate::Postgres);
impl pbps_dialect::lower::Lowering for ScratchEmitter {
    type Input = pbps_diff::order::scratch::ScratchPlan<crate::Postgres>;
    fn emit_position(
        &self,
        input: &Self::Input,
        position: usize,
    ) -> Result<Vec<pbps_dialect::Statement>, pbps_dialect::DialectError> {
        let part = &input.parts()[position].change;
        crate::emit::emit(&self.0, &part.change, part.strategy)
    }
}

#[cfg(test)]
pub(crate) fn fixture_plan(changes: &[Change]) -> pbps_diff::order::PlanningPlan {
    pbps_diff::order::propose(pbps_model::ChangeSet {
        changes: changes
            .iter()
            .cloned()
            .map(pbps_model::PlannedChange::new)
            .collect(),
    })
}
