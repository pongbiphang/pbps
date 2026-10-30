//! Transient annotation identities, reprojected at each existing settlement.
use std::sync::Arc;

use super::*;
use pbps_model::diagnostic::Declaration;
use pbps_model::object_ref::{BoundRoot, Registry};
use pbps_model::operation::symbolic::Attribute;
use pbps_model::operation::{
    ColumnKey, ModuleEdits, ObjectKey, Operations, Origin, Owners, PartKey, PlanOwners, Snapshot,
    Spelling,
};
use pbps_model::plan_names::{At, PlanNames};
use pbps_model::{IdsFile, Schema, Table, Uid};

type Ref = ObjectRef<BoundRoot>;

#[derive(Debug, Clone)]
struct Side {
    schema: Schema,
    ids: IdsFile,
    members: BTreeMap<Ref, BTreeSet<Ref>>,
    roots: BTreeMap<Uid, Uid>,
    references: BTreeSet<Ref>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Creation {
    pub references: BTreeSet<ObjectRef>,
    pub columns: BTreeMap<ObjectRef, pbps_model::ColumnName>,
    pub dependencies: Arc<BTreeMap<ObjectRef, BTreeSet<ObjectRef>>>,
}

/// Descriptive identity evidence beside a draft, never saved-plan data.
#[derive(Debug, Clone)]
pub struct Contract {
    logical: pbps_model::ChangeSet,
    modules: ModuleEdits,
    before: Arc<Side>,
    after: Arc<Side>,
    base: Vec<Annotation<BoundRoot>>,
    goal: Vec<Annotation<BoundRoot>>,
    labels: BTreeMap<Ref, String>,
    demanded: BTreeMap<Root, BTreeSet<ObjectRef>>,
    creation: BTreeMap<pbps_model::operation::Source, (BTreeSet<ObjectRef>, Creation)>,
    pub(crate) extended: bool,
}

fn containing(declaration: &Declaration) -> ObjectRef {
    let root = match declaration {
        Declaration::Table(n) => Root::Table(n.clone()),
        Declaration::Module(n) => Root::Module(n.clone()),
        Declaration::Type(n) => Root::Type(n.clone()),
        Declaration::Role(n) => Root::Role(n.clone()),
    };
    ObjectRef {
        root,
        part: Selector::Whole,
    }
}

impl Side {
    fn of(snapshot: Snapshot<'_>, owners: &dyn Owners, spelling: Spelling) -> Result<Self, String> {
        let registry = Registry::new(snapshot.schema);
        let mut side = Self {
            schema: snapshot.schema.clone(),
            ids: snapshot.ids.clone(),
            members: BTreeMap::new(),
            roots: BTreeMap::new(),
            references: BTreeSet::new(),
        };
        for reference in registry.references() {
            let bound = registry
                .bind(reference, owners, spelling)
                .map_err(|e| e.to_string())?;
            let declaration =
                containing(registry.declaration(reference).map_err(|e| e.to_string())?);
            let container = registry
                .bind(&declaration, owners, spelling)
                .map_err(|e| e.to_string())?;
            side.members
                .entry(container)
                .or_default()
                .insert(bound.clone());
            side.members
                .entry(ObjectRef {
                    root: bound.root.clone(),
                    part: Selector::Whole,
                })
                .or_default()
                .insert(bound.clone());
            side.references.insert(bound);
        }
        let ids = snapshot.ids.index();
        for name in snapshot.schema.tables.keys() {
            let chain = pbps_model::partitions::ancestor_chain(name, &snapshot.schema.tables)
                .ok_or_else(|| {
                    format!("partition ancestry for `{}` is unavailable", name.shown())
                })?;
            let root = chain.last().expect("an ancestor chain includes its root").0;
            let table = ids
                .table_uid(name)
                .ok_or_else(|| format!("table identity for `{}` is unavailable", name.shown()))?;
            let root = ids
                .table_uid(root)
                .ok_or_else(|| format!("root identity for `{}` is unavailable", root.shown()))?;
            side.roots.insert(table.clone(), root.clone());
        }
        Ok(side)
    }
}

impl Contract {
    pub(crate) fn of<S>(
        plan: &super::super::PlanningPlan<S>,
        before: Snapshot<'_>,
        after: Snapshot<'_>,
        base: &ObjectDeps,
        goal: &ObjectDeps,
        modules: ModuleEdits,
    ) -> Result<Self, String> {
        let owners = PlanOwners::new(
            PlanNames::of(&plan.changes, after.ids).map_err(|e| e.to_string())?,
            before.ids,
            after.ids,
        );
        let mut labels = BTreeMap::new();
        let mut bind = |snapshot: Snapshot<'_>, dependencies: &ObjectDeps, spelling| {
            let registry = Registry::new(snapshot.schema);
            let problems = registry.validate(dependencies, Some((&owners, spelling)));
            if !problems.is_empty() {
                return Err(problems
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"));
            }
            let mut annotations = Vec::new();
            for (owner, providers) in &dependencies.0 {
                let bound_owner = registry
                    .bind(owner, &owners, spelling)
                    .map_err(|e| e.to_string())?;
                labels.insert(bound_owner.clone(), owner.shown().to_string());
                for provider in providers {
                    let bound_provider = registry
                        .bind(provider, &owners, spelling)
                        .map_err(|e| e.to_string())?;
                    labels.insert(bound_provider.clone(), provider.shown().to_string());
                    annotations.push(Annotation {
                        owner: bound_owner.clone(),
                        provider: bound_provider,
                    });
                }
            }
            Ok(annotations)
        };
        let registry = Registry::new(after.schema);
        let mut demanded: BTreeMap<Root, BTreeSet<ObjectRef>> = BTreeMap::new();
        for reference in goal
            .0
            .iter()
            .flat_map(|(owner, providers)| std::iter::once(owner).chain(providers))
        {
            if reference.part != Selector::Whole || matches!(reference.root, Root::Column(_)) {
                let root =
                    containing(registry.declaration(reference).map_err(|e| e.to_string())?).root;
                demanded.entry(root).or_default().insert(reference.clone());
            }
        }
        let base_edges = bind(before, base, Spelling::Base)?;
        let goal_edges = bind(after, goal, Spelling::Final)?;
        let legacy = |r: &ObjectRef| {
            matches!(
                r,
                ObjectRef {
                    root: Root::Module(_),
                    part: Selector::Whole
                }
            )
        };
        let extended = [base, goal]
            .into_iter()
            .flat_map(|d| &d.0)
            .any(|(owner, providers)| !legacy(owner) || providers.iter().any(|p| !legacy(p)));
        let names = PlanNames::of(&plan.changes, after.ids).map_err(|e| e.to_string())?;
        let dependencies: Arc<BTreeMap<_, _>> = Arc::new(goal.0.iter().cloned().collect());
        let mut creation = BTreeMap::new();
        for (step, origin) in plan.changes.iter().zip(&plan.origins) {
            if !matches!(
                step.change,
                Change::CreateTable { .. }
                    | Change::AddColumn { .. }
                    | Change::AddComputedColumn { .. }
            ) {
                continue;
            }
            let Some(table) = step.change.table() else {
                continue;
            };
            let spelling = if matches!(step.change, Change::CreateTable { .. }) {
                origin.spelling().produced()
            } else {
                origin.spelling()
            };
            let uid = owners.table(table, spelling).map_err(|e| e.to_string())?;
            let canonical = Root::Table(after.ids.tables.get(&uid).unwrap_or(table).clone());
            let mut requested = BTreeSet::new();
            let mut references = BTreeSet::new();
            let mut columns = BTreeMap::new();
            for reference in demanded.get(&canonical).into_iter().flatten() {
                let mut actual = reference.clone();
                match &mut actual.root {
                    Root::Table(name) => *name = table.clone(),
                    Root::Column(column) => {
                        column.table = table.clone();
                        let bound = registry
                            .bind(reference, &owners, Spelling::Final)
                            .map_err(|e| e.to_string())?;
                        let column_uid = match &bound.root {
                            BoundRoot::Recorded(uid)
                            | BoundRoot::InheritedColumn { column: uid, .. } => Some(uid),
                            BoundRoot::LocalColumn { .. }
                            | BoundRoot::Module(_)
                            | BoundRoot::Trigger { .. } => None,
                        };
                        if let (Some(uid), Spelling::At(at)) =
                            (column_uid, origin.spelling().produced())
                            && let Some((_, name)) = names.column_at(uid, at)
                        {
                            column.name = name.clone();
                        }
                    }
                    Root::Type(_) | Root::Module(_) | Root::Role(_) => {}
                }
                if let Root::Column(column) = &actual.root {
                    columns.insert(reference.clone(), column.name.clone());
                }
                // Provider-only existence/generation can stay in the legal prefix.
                if !matches!(reference.root, Root::Column(_))
                    || !matches!(reference.part, Selector::Whole | Selector::Generation)
                    || dependencies.get(reference).is_some_and(|p| !p.is_empty())
                {
                    requested.insert(actual);
                }
                if matches!(reference.part, Selector::Generation | Selector::Whole)
                    && matches!(reference.root, Root::Column(_))
                {
                    references.extend(dependencies.get(reference).into_iter().flatten().cloned());
                }
            }
            creation.insert(
                origin.source(),
                (
                    requested,
                    Creation {
                        references,
                        columns,
                        dependencies: dependencies.clone(),
                    },
                ),
            );
        }
        Ok(Self {
            creation,
            logical: plan.changes().clone(),
            modules,
            before: Arc::new(Side::of(before, &owners, Spelling::Base)?),
            after: Arc::new(Side::of(after, &owners, Spelling::Final)?),
            base: base_edges,
            goal: goal_edges,
            labels,
            demanded,
            extended,
        })
    }

    pub(crate) fn goal_ids(&self) -> &IdsFile {
        &self.after.ids
    }

    #[allow(clippy::wildcard_enum_match_arm)]
    pub(crate) fn demands(
        &self,
        change: &Change,
        origin: &pbps_model::operation::Origin,
    ) -> BTreeSet<ObjectRef> {
        if let Some((references, _)) = self.creation.get(&origin.source()) {
            return references.clone();
        }
        let root = match change {
            Change::CreateTable { name, .. } => Root::Table(name.clone()),
            Change::CreateModule { id, .. } | Change::AlterModule { id, .. } => {
                Root::Module(id.clone())
            }
            Change::CreateType { name, .. } => Root::Type(name.clone()),
            Change::AddColumn { table, .. } | Change::AddComputedColumn { table, .. } => {
                Root::Table(table.clone())
            }
            _ => return BTreeSet::new(),
        };
        self.demanded.get(&root).cloned().unwrap_or_default()
    }

    pub(crate) fn prerequisites(&self, origin: &pbps_model::operation::Origin) -> Creation {
        self.creation
            .get(&origin.source())
            .map_or_else(Creation::default, |(_, prerequisites)| {
                prerequisites.clone()
            })
    }

    pub(crate) fn constraints<S>(
        &self,
        plan: &super::super::PlanningPlan<S>,
    ) -> Result<Vec<Constraint>, String> {
        // Snapshot names follow logical sources; retained operations separately
        // carry their immutable input address versions (DEC-2296.9).
        let mut logical = self.logical.changes.clone();
        let additions: BTreeMap<_, _> = plan
            .origins
            .iter()
            .zip(&plan.changes)
            .filter_map(|(origin, step)| {
                if let Origin::Whole(at) = origin {
                    Some((*at, step.clone()))
                } else {
                    None
                }
            })
            .collect();
        for at in logical.len()..plan.logical {
            logical.push(additions.get(&at).cloned().ok_or_else(|| {
                format!("annotation name timeline for logical source {at} is unavailable")
            })?);
        }
        let names = PlanNames::of(&logical, &self.after.ids).map_err(|e| e.to_string())?;
        let owners = PlanOwners::new(
            PlanNames::of(&logical, &self.after.ids).map_err(|e| e.to_string())?,
            &self.before.ids,
            &self.after.ids,
        );
        let operations = plan.identities.as_ref().map_or_else(
            || {
                Operations::expanded(
                    plan.logical,
                    plan.origins
                        .iter()
                        .cloned()
                        .zip(plan.changes.iter().cloned()),
                    self.modules,
                )
            },
            |operations| operations.with_steps(&plan.changes),
        );
        let mut index = Index {
            standing: self.before.references.clone(),
            ..Default::default()
        };
        index
            .containment
            .extend(self.after.members.iter().flat_map(|(container, members)| {
                members
                    .iter()
                    .filter(move |r| *r != container)
                    .map(move |r| (container.clone(), r.clone()))
            }));
        let mut changed: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
        for op in operations.as_slice() {
            if let Some(uid) = op.change().parent_column_uid() {
                changed.entry(uid.clone()).or_default().extend(
                    op.change()
                        .column_transitions()
                        .into_iter()
                        .filter_map(|t| {
                            if let pbps_model::change::ColumnProperty::Column(a) = t.property {
                                Some(a)
                            } else {
                                None
                            }
                        }),
                );
            }
        }
        for reference in self
            .base
            .iter()
            .chain(&self.goal)
            .flat_map(|a| [&a.owner, &a.provider])
        {
            if let BoundRoot::InheritedColumn { column, .. } = &reference.root
                && let Some(attributes) = changed.get(column)
                && match reference.part {
                    Selector::Whole => [Attribute::Type, Attribute::Collation, Attribute::Name]
                        .iter()
                        .any(|attribute| attributes.contains(attribute)),
                    Selector::Default | Selector::Generation => {
                        attributes.contains(&Attribute::Default)
                    }
                    Selector::NotNull => attributes.contains(&Attribute::NotNull),
                    Selector::Sequence => attributes.contains(&Attribute::Numbering),
                    Selector::Part(_) => false,
                }
            {
                return Err(format!(
                    "cannot project `{}`: recursive engine boundary metadata is required",
                    self.labels[reference]
                ));
            }
        }
        for (at, operation) in operations.as_slice().iter().enumerate() {
            let inputs = operation.input_owners(&owners);
            for definition in operation.defines(&owners).map_err(|e| e.to_string())? {
                let reference = self.reference(&definition.key, operation, &inputs, &names)?;
                if let Some(reference) = reference {
                    self.record(&mut index, reference, at, definition.transition);
                }
            }
            self.facets(&mut index, at, operation, &inputs, &names)?;
        }
        let installers: BTreeSet<_> = index
            .boundaries
            .iter()
            .flat_map(|(reference, boundary)| {
                boundary
                    .installs
                    .iter()
                    .map(|at| (operations.as_slice()[*at].source(), reference.clone()))
            })
            .collect();
        for (at, operation) in operations.as_slice().iter().enumerate() {
            self.payload(
                &mut index,
                at,
                operation,
                &operation.input_owners(&owners),
                &installers,
            )?;
        }
        index.project(&self.base, &self.goal).map_err(|error| {
            let (number, reason) = match error {
                ProjectionError::Missing(n) => (n, "an endpoint has no installation boundary or recorded baseline presence"),
                ProjectionError::InternalCycle(n) => (n, "an internal creation cycle: the indivisible owner's contained surface requires the owner first"),
            };
            let annotation = &self.goal[number];
            let setup = [&annotation.owner, &annotation.provider].iter().any(|r| matches!(&r.root, BoundRoot::Recorded(uid) if self.after.ids.roles.contains_key(uid)) && !self.before.references.contains(*r));
            let reason=if matches!(error,ProjectionError::Missing(_)) && setup { "a fixed external setup boundary has no installation operation; provision the role before the baseline" } else {reason};
            format!("cannot project annotation `{}` depends on `{}`: {reason}", self.labels[&annotation.owner], self.labels[&annotation.provider])
        })
    }

    fn record(
        &self,
        index: &mut Index<BoundRoot>,
        reference: Ref,
        at: usize,
        transition: Transition,
    ) {
        index.record(reference.clone(), at, transition);
        if transition == Transition::Ends && reference.part == Selector::Whole {
            for member in self.before.members.get(&reference).into_iter().flatten() {
                index.record(member.clone(), at, transition);
            }
        }
    }

    fn reference(
        &self,
        key: &ObjectKey,
        operation: &Operation,
        owners: &dyn Owners,
        names: &PlanNames<'_>,
    ) -> Result<Option<Ref>, String> {
        let spelling = operation.origin().spelling();
        let column = |key: &ColumnKey| -> Result<BoundRoot, String> {
            // Scratch defaults retain a table-UID carrier for reconstruction.
            // Resolve it through the same recorded ancestor binding as a
            // partition operation, without changing any engine's recipe.
            let carrier =
                if let (ColumnKey::Recorded(table), Change::AlterColumnDefault { column, .. }) =
                    (key, operation.change())
                    && table.uid().kind() == pbps_model::UidKind::Table
                {
                    let owner = owners
                        .table(&column.table, spelling)
                        .map_err(|e| e.to_string())?;
                    if &owner != table.uid() {
                        return Err(
                            "scratch default carrier does not match its recorded table".into()
                        );
                    }
                    Some(ColumnKey::Partition {
                        table: table.clone(),
                        column: column.name.clone(),
                    })
                } else {
                    None
                };
            Ok(match carrier.as_ref().unwrap_or(key) {
                ColumnKey::Recorded(uid) => BoundRoot::Recorded(uid.uid().clone()),
                ColumnKey::Partition { table, column } => {
                    let sides = if spelling == Spelling::Base {
                        [&self.before, &self.after]
                    } else {
                        [&self.after, &self.before]
                    };
                    let (side, root) = sides
                        .into_iter()
                        .find_map(|s| s.roots.get(table.uid()).map(|root| (s, root)))
                        .ok_or_else(|| "partition column ownership was not recorded".to_owned())?;
                    // Root column declarations belong to the logical snapshots.
                    // The child's input address never substitutes for its root's
                    // recorded column anchor (DEC-2296.9).
                    let table_name = match operation.logical_origin() {
                        Origin::Teardown(_) => self.before.ids.tables.get(root),
                        Origin::Restoration(_) => self.after.ids.tables.get(root),
                        Origin::Whole(at) => names.table_at(root, At::Before(*at)),
                        Origin::SplitPart(at) => names.table_at(root, At::After(*at)),
                    }
                    .or_else(|| side.ids.tables.get(root))
                    .ok_or_else(|| "root table address is unavailable".to_owned())?;
                    let uid = owners
                        .column(
                            &table_name.column(column),
                            operation.logical_origin().spelling(),
                        )
                        .map_err(|e| e.to_string())?;
                    if root == table.uid() {
                        BoundRoot::Recorded(uid)
                    } else {
                        BoundRoot::InheritedColumn {
                            table: table.uid().clone(),
                            column: uid,
                        }
                    }
                }
            })
        };
        let reference = match key {
            ObjectKey::Table(uid) | ObjectKey::Type(uid) | ObjectKey::Role(uid) => ObjectRef {
                root: BoundRoot::Recorded(uid.uid().clone()),
                part: Selector::Whole,
            },
            ObjectKey::Column(key) => ObjectRef {
                root: column(key)?,
                part: Selector::Whole,
            },
            ObjectKey::Default(key) => ObjectRef {
                root: column(key)?,
                part: if matches!(operation.change(), Change::AlterColumnExpression { .. }) {
                    Selector::Generation
                } else {
                    Selector::Default
                },
            },
            ObjectKey::NotNull(key) => ObjectRef {
                root: column(key)?,
                part: Selector::NotNull,
            },
            ObjectKey::Module(pbps_model::ModuleId::Trigger { on, name }) => ObjectRef {
                root: BoundRoot::Trigger {
                    table: owners.table(on, spelling).map_err(|e| e.to_string())?,
                    name: name.clone(),
                },
                part: Selector::Whole,
            },
            ObjectKey::Module(id) => ObjectRef {
                root: BoundRoot::Module(id.clone()),
                part: Selector::Whole,
            },
            ObjectKey::Part {
                table,
                part: PartKey::Computed(name),
            } => ObjectRef {
                root: BoundRoot::LocalColumn {
                    table: table.uid().clone(),
                    name: name.clone(),
                },
                part: Selector::Whole,
            },
            ObjectKey::Part { table, part } => ObjectRef {
                root: BoundRoot::Recorded(table.uid().clone()),
                part: Selector::Part(part.clone()),
            },
            ObjectKey::Row { .. }
            | ObjectKey::Rows { .. }
            | ObjectKey::ColumnOrder(_)
            | ObjectKey::Grant { .. } => return Ok(None),
        };
        Ok(Some(reference))
    }

    /// The registry enumerates the operation's actual payload, so an extracted
    /// bare table never claims a facet still waiting on another operation.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn payload(
        &self,
        index: &mut Index<BoundRoot>,
        at: usize,
        op: &Operation,
        owners: &dyn Owners,
        installers: &BTreeSet<(pbps_model::operation::Source, Ref)>,
    ) -> Result<(), String> {
        let mut schema = Schema::default();
        let root = match op.change() {
            Change::CreateTable { name, table, .. } => {
                if let Some(chain) =
                    pbps_model::partitions::ancestor_chain(name, &self.after.schema.tables)
                {
                    for (name, shape) in chain {
                        schema.tables.insert(name.clone(), shape.clone());
                    }
                }
                // Policies are installed by their own Change, never CREATE TABLE.
                let mut payload = (**table).clone();
                payload.row_policies.clear();
                schema.tables.insert(name.clone(), payload);
                Root::Table(name.clone())
            }
            Change::AddColumn {
                table,
                name,
                column,
                ..
            } => {
                schema.tables.insert(
                    table.clone(),
                    Table {
                        columns: [(name.clone(), (**column).clone())].into_iter().collect(),
                        ..Default::default()
                    },
                );
                Root::Column(table.column(name))
            }
            Change::AddComputedColumn {
                table,
                name,
                computed,
            } => {
                schema.tables.insert(
                    table.clone(),
                    Table {
                        computed: [(name.clone(), computed.clone())].into(),
                        ..Default::default()
                    },
                );
                Root::Column(table.column(name))
            }
            Change::CreateModule { id, module } | Change::AlterModule { id, module } => {
                schema.modules.insert(id.clone(), (**module).clone());
                Root::Module(id.clone())
            }
            Change::CreateType {
                name, definition, ..
            } => {
                schema.types.insert(name.clone(), (**definition).clone());
                Root::Type(name.clone())
            }
            _ => return Ok(()),
        };
        let registry = Registry::new(&schema);
        for reference in registry.references() {
            let declaration =
                containing(registry.declaration(reference).map_err(|e| e.to_string())?);
            if reference.root != root && declaration.root != root {
                continue;
            }
            let bound = registry
                .bind(reference, owners, op.origin().spelling().produced())
                .map_err(|e| e.to_string())?;
            // A typed recipe part owns its final surface. CREATE can already
            // expose an inherited fallback without installing the extracted
            // declaration belonging to the same logical source (DEC-2469.3).
            if !installers.contains(&(op.source(), bound.clone())) {
                self.record(index, bound, at, Transition::Produces);
            }
        }
        if matches!(op.change(), Change::AlterModule { .. }) {
            let reference = ObjectRef {
                root,
                part: Selector::Whole,
            };
            let bound = registry
                .bind(&reference, owners, op.origin().spelling())
                .map_err(|e| e.to_string())?;
            self.record(index, bound.clone(), at, Transition::Ends);
            // Both policies replace the declaration, but only InPlace keeps
            // the provider available to existing consumers (ADR-0009 §3).
            if op.module_edits() == ModuleEdits::InPlace {
                index.continuous.insert((bound, at));
            }
        }
        Ok(())
    }

    #[allow(clippy::wildcard_enum_match_arm)]
    fn facets(
        &self,
        index: &mut Index<BoundRoot>,
        at: usize,
        op: &Operation,
        owners: &dyn Owners,
        names: &PlanNames<'_>,
    ) -> Result<(), String> {
        let key = op.key(owners).map_err(|e| e.to_string())?;
        let Some(mut reference) = self.reference(&key, op, owners, names)? else {
            return Ok(());
        };
        let original_part = reference.part.clone();
        let mut event = |part, transition| {
            reference.part = part;
            self.record(index, reference.clone(), at, transition);
        };
        match op.change() {
            Change::AlterColumnNullability { to_nullable, .. } => event(
                Selector::NotNull,
                if *to_nullable {
                    Transition::Ends
                } else {
                    Transition::Produces
                },
            ),
            Change::SetPartitionNotNull { not_null, .. } => event(
                Selector::NotNull,
                if *not_null {
                    Transition::Produces
                } else {
                    Transition::Ends
                },
            ),
            Change::AlterColumnType { .. } => event(Selector::Whole, Transition::Produces),
            Change::AlterColumnNumbering { from, to, .. } => {
                if matches!(from, pbps_model::ValueSource::Sequence(_)) {
                    event(Selector::Sequence, Transition::Ends);
                }
                if matches!(to, pbps_model::ValueSource::Sequence(_)) {
                    event(Selector::Sequence, Transition::Produces);
                }
            }
            Change::AddEnumValue { .. } | Change::RenameEnumValue { .. } => {
                event(Selector::Whole, Transition::Produces)
            }
            Change::SetIndexStorageParameters { .. } | Change::SetRowSecurity { .. } => {
                event(original_part, Transition::Produces)
            }
            Change::AlterDomain { from, to, half, .. } => {
                let delta = pbps_model::DomainDelta::of(from, to)
                    .ok_or_else(|| "domain boundary metadata is unavailable".to_owned())?;
                match half {
                    pbps_model::DomainHalf::Loosen => {
                        for name in delta.drop_checks {
                            event(
                                Selector::Part(PartKey::Check(name.clone())),
                                Transition::Ends,
                            );
                        }
                        if delta.drop_not_null {
                            event(Selector::NotNull, Transition::Ends);
                        }
                        if let Some(default) = delta.default {
                            if matches!(
                                from.as_ref(),
                                pbps_model::ManagedType::Domain {
                                    default: Some(_),
                                    ..
                                }
                            ) {
                                event(Selector::Default, Transition::Ends);
                            }
                            event(
                                Selector::Default,
                                if default.is_some() {
                                    Transition::Produces
                                } else {
                                    Transition::Ends
                                },
                            );
                        }
                    }
                    pbps_model::DomainHalf::Tighten => {
                        for (name, _) in delta.add_checks {
                            event(
                                Selector::Part(PartKey::Check(name.clone())),
                                Transition::Produces,
                            );
                        }
                        if delta.set_not_null {
                            event(Selector::NotNull, Transition::Produces);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
