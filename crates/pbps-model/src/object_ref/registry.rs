use super::{ObjectRef, Root, Selector};
use crate::diagnostic::Declaration;
use crate::escape::Escaped;
use crate::operation::{Owners, PartKey, Spelling};
use crate::{ColumnName, ModuleDeps, ModuleId, Schema, Uid, ValueSource};
use std::collections::{BTreeMap, BTreeSet};

/// No UID is invented for name-keyed objects or inherited/computed columns.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum BoundRoot {
    Recorded(Uid),
    LocalColumn { table: Uid, name: ColumnName },
    InheritedColumn { table: Uid, column: Uid },
    Module(ModuleId),
    Trigger { table: Uid, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    UnknownOwner(ObjectRef),
    UnknownTarget(ObjectRef),
    MissingPart(ObjectRef),
    WrongSelector(ObjectRef),
    Ambiguous(ObjectRef),
    DuplicateOwner(ObjectRef),
    SelfDependency(ObjectRef),
    Cycle(Vec<(ObjectRef, ObjectRef)>),
    OutsideDatabase(String),
    Unrecorded {
        reference: Box<ObjectRef>,
        reason: String,
    },
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (reference, reason) = match self {
            Self::UnknownOwner(r) => (r, "unknown dependency owner"),
            Self::UnknownTarget(r) => (r, "unknown dependency target"),
            Self::MissingPart(r) => (r, "the dependency part is not declared"),
            Self::WrongSelector(r) => (r, "the dependency selector does not apply to this root"),
            Self::Ambiguous(r) => (r, "the dependency is ambiguous after normalization"),
            Self::DuplicateOwner(r) => (r, "duplicate dependency owner after normalization"),
            Self::SelfDependency(r) => (r, "the dependency owner lists itself"),
            Self::Cycle(legs) => {
                return write!(
                    f,
                    "the dependency declarations close a cycle: {}",
                    legs.iter()
                        .map(|(a, b)| format!("{} -> {}", a.shown(), b.shown()))
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            Self::OutsideDatabase(name) => {
                return write!(
                    f,
                    "{} is in another database; deploy its prerequisite there first, then validate the declared dependency here",
                    Escaped(name)
                );
            }
            Self::Unrecorded { reference, reason } => {
                return write!(f, "cannot bind {}: {reason}", reference.shown());
            }
        };
        write!(f, "{reason}: {}", reference.shown())
    }
}
impl std::error::Error for Problem {}

/// Keep the input sequence: collecting into a map would hide duplicate owners.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectDeps(pub Vec<(ObjectRef, BTreeSet<ObjectRef>)>);

impl ObjectDeps {
    pub fn from_legacy(deps: &ModuleDeps) -> Self {
        let module = |id: &ModuleId| ObjectRef::whole(Root::Module(id.clone()));
        Self(
            deps.iter()
                .map(|(id, targets)| (module(id), targets.iter().map(module).collect()))
                .collect(),
        )
    }

    /// This is the only write adapter until non-module persistence is enabled.
    pub fn to_legacy(&self) -> Result<ModuleDeps, Problem> {
        let mut out = ModuleDeps::new();
        for (owner, targets) in &self.0 {
            let id = owner
                .module_id()
                .ok_or_else(|| Problem::WrongSelector(owner.clone()))?;
            if out.contains_key(id) {
                return Err(Problem::DuplicateOwner(owner.clone()));
            }
            let targets = targets
                .iter()
                .map(|r| {
                    r.module_id()
                        .cloned()
                        .ok_or_else(|| Problem::WrongSelector(r.clone()))
                })
                .collect::<Result<_, _>>()?;
            out.insert(id.clone(), targets);
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
struct Entry {
    reference: ObjectRef,
    declaration: Declaration,
    local_column: bool,
    inherited_from: Option<crate::ColumnRef>,
}

/// One index over all declared kinds. Normalization belongs to the caller's
/// dialect; it must run on declarations and references before uniqueness is decided.
pub struct Registry<'a> {
    entries: BTreeMap<ObjectRef, Vec<Entry>>,
    normalize: Box<dyn Fn(&ObjectRef) -> ObjectRef + 'a>,
}

impl<'a> Registry<'a> {
    pub fn new(schema: &Schema) -> Self {
        Self::normalized(schema, Clone::clone)
    }

    pub fn normalized(schema: &Schema, normalize: impl Fn(&ObjectRef) -> ObjectRef + 'a) -> Self {
        let mut out = Self {
            entries: BTreeMap::new(),
            normalize: Box::new(normalize),
        };
        for (name, table) in &schema.tables {
            let declaration = Declaration::Table(name.clone());
            let root = Root::Table(name.clone());
            out.add(root.clone(), Selector::Whole, &declaration, false);
            out.add(
                root.clone(),
                Selector::Part(PartKey::RowSecurity),
                &declaration,
                false,
            );
            if table.primary_key.is_some() {
                out.add(
                    root.clone(),
                    Selector::Part(PartKey::PrimaryKey),
                    &declaration,
                    false,
                );
            }
            for part in table
                .unique
                .keys()
                .cloned()
                .map(PartKey::Unique)
                .chain(table.foreign_keys.keys().cloned().map(PartKey::ForeignKey))
                .chain(table.checks.keys().cloned().map(PartKey::Check))
                .chain(table.indexes.keys().cloned().map(PartKey::Index))
                .chain(table.row_policies.keys().cloned().map(PartKey::Policy))
            {
                out.add(root.clone(), Selector::Part(part), &declaration, false);
            }
            // Only the root declares columns and owns their UIDs. Every level's
            // overrides contribute facets: None/false inherit (DEC-1578.1).
            if let Some(chain) = crate::partitions::ancestor_chain(name, &schema.tables) {
                let (root_name, shape) = chain.last().expect("a chain includes its root");
                for (column, spec) in &shape.columns {
                    let root = Root::Column(name.column(column));
                    let inherited = table.partition_of.is_some();
                    let mut register_column = |part| {
                        let entry = out.add(root.clone(), part, &declaration, inherited);
                        entry.inherited_from = inherited.then(|| root_name.column(column));
                    };
                    register_column(Selector::Whole);
                    let mut overrides = chain.iter().filter_map(|(_, ancestor)| {
                        ancestor.partition_of.as_ref()?.columns.get(column)
                    });
                    if !spec.nullable || overrides.clone().any(|c| c.not_null) {
                        register_column(Selector::NotNull);
                    }
                    let facet = match &spec.value_source {
                        ValueSource::Default(_) => Some(Selector::Default),
                        ValueSource::Generated(_) => Some(Selector::Generation),
                        ValueSource::Sequence(_) if !inherited => Some(Selector::Sequence),
                        ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => {
                            None
                        }
                    };
                    if let Some(facet) = facet {
                        register_column(facet);
                    }
                    if overrides.any(|c| c.default.is_some())
                        && !matches!(&spec.value_source, ValueSource::Default(_))
                    {
                        register_column(Selector::Default);
                    }
                }
            }
            for (column, spec) in &table.computed {
                let root = Root::Column(name.column(column));
                out.add(root.clone(), Selector::Whole, &declaration, true);
                out.add(root.clone(), Selector::Generation, &declaration, true);
                if spec.not_null {
                    out.add(root.clone(), Selector::NotNull, &declaration, true);
                }
                // Existing PartKey and the portable generation spelling denote one facet.
                out.add(
                    Root::Table(name.clone()),
                    Selector::Part(PartKey::Computed(column.clone())),
                    &declaration,
                    true,
                )
                .reference = ObjectRef {
                    root,
                    part: Selector::Generation,
                };
            }
        }
        for (name, ty) in &schema.types {
            let root = Root::Type(name.clone());
            let declaration = Declaration::Type(name.clone());
            out.add(root.clone(), Selector::Whole, &declaration, false);
            if let crate::ManagedType::Domain {
                default,
                checks,
                not_null,
                ..
            } = ty
            {
                if default.is_some() {
                    out.add(root.clone(), Selector::Default, &declaration, false);
                }
                if *not_null {
                    out.add(root.clone(), Selector::NotNull, &declaration, false);
                }
                for name in checks.keys() {
                    out.add(
                        root.clone(),
                        Selector::Part(PartKey::Check(name.clone())),
                        &declaration,
                        false,
                    );
                }
            }
        }
        for (id, module) in &schema.modules {
            let root = Root::Module(id.clone());
            let declaration = Declaration::Module(id.clone());
            out.add(root.clone(), Selector::Whole, &declaration, false);
            for name in module.options.indexes.keys() {
                out.add(
                    root.clone(),
                    Selector::Part(PartKey::Index(name.clone())),
                    &declaration,
                    false,
                );
            }
        }
        for name in schema.roles.keys() {
            out.add(
                Root::Role(name.clone()),
                Selector::Whole,
                &Declaration::Role(name.clone()),
                false,
            );
        }
        out
    }

    fn add(
        &mut self,
        root: Root,
        part: Selector,
        declaration: &Declaration,
        local_column: bool,
    ) -> &mut Entry {
        let reference = ObjectRef { root, part };
        let entries = self
            .entries
            .entry((self.normalize)(&reference))
            .or_default();
        entries.push(Entry {
            reference,
            declaration: declaration.clone(),
            local_column,
            inherited_from: None,
        });
        entries.last_mut().expect("just registered")
    }

    fn entry(&self, reference: &ObjectRef) -> Result<&Entry, Problem> {
        let key = (self.normalize)(reference);
        if let Some(entries) = self.entries.get(&key) {
            return match entries.as_slice() {
                [entry] => Ok(entry),
                _ => Err(Problem::Ambiguous(reference.clone())),
            };
        }
        if !self
            .entries
            .contains_key(&ObjectRef::whole(key.root.clone()))
        {
            return Err(Problem::UnknownTarget(reference.clone()));
        }
        let valid = matches!(
            (&key.root, &key.part),
            (Root::Table(_), Selector::Part(_))
                | (
                    Root::Column(_),
                    Selector::Default
                        | Selector::Generation
                        | Selector::Sequence
                        | Selector::NotNull
                )
                | (
                    Root::Type(_),
                    Selector::Default | Selector::NotNull | Selector::Part(PartKey::Check(_))
                )
                | (Root::Module(_), Selector::Part(PartKey::Index(_)))
        );
        Err(if valid {
            Problem::MissingPart(reference.clone())
        } else {
            Problem::WrongSelector(reference.clone())
        })
    }

    /// Context can make otherwise valid shorthand collide with a legacy module.
    /// The checked declaration vocabulary, including canonical facet aliases.
    pub fn references(&self) -> impl Iterator<Item = &ObjectRef> {
        self.entries.keys()
    }

    pub fn render(&self, reference: &ObjectRef) -> Result<String, Problem> {
        self.entry(reference)?;
        let short = reference.raw();
        if self.parse(&short, &Default::default()).as_ref() == Ok(reference) {
            Ok(short)
        } else {
            Ok(format!(
                "object::{}",
                serde_json::to_string(reference).expect("reference serializes")
            ))
        }
    }

    pub fn declaration(&self, reference: &ObjectRef) -> Result<&Declaration, Problem> {
        self.entry(reference).map(|entry| &entry.declaration)
    }

    /// Parse with declaration context: an old module spelling cannot silently
    /// acquire a typed meaning, and external assumptions have no local producer.
    pub fn parse(
        &self,
        input: &str,
        external: &crate::dependency::Dependencies,
    ) -> Result<ObjectRef, String> {
        let typed = input.split_once("::");
        if let Some((kind, value)) = typed {
            if kind != "module"
                && kind != "object"
                && let Ok(module) = ObjectRef::legacy(input)
                && self.entry(&module).is_ok()
            {
                return Err(Problem::Ambiguous(module).to_string());
            }
            if matches!(kind, "table" | "module" | "type")
                && !input
                    .parse::<ObjectRef>()
                    .is_ok_and(|r| self.entry(&r).is_ok())
                && value
                    .parse::<crate::dependency::DependencyName>()
                    .is_ok_and(|n| external.contains_key(&n))
            {
                return Err(Problem::OutsideDatabase(value.into()).to_string());
            }
        }
        input.parse::<ObjectRef>().map_err(|e| e.to_string())
    }

    /// Owners supplies Base/Final or Origin spelling through PlanNames; a missing
    /// UID is never replaced with a name-derived identity.
    pub fn bind(
        &self,
        reference: &ObjectRef,
        owners: &dyn Owners,
        spelling: Spelling,
    ) -> Result<ObjectRef<BoundRoot>, Problem> {
        let entry = self.entry(reference)?;
        let recorded =
            |result: Result<Uid, crate::operation::OwnerError>| result.map(BoundRoot::Recorded);
        let root = match &entry.reference.root {
            Root::Table(n) => recorded(owners.table(n, spelling)),
            Root::Type(n) => recorded(owners.managed_type(n, spelling)),
            Root::Role(n) => recorded(owners.role(n, spelling)),
            Root::Column(c) if entry.inherited_from.is_some() => {
                let root_column = entry
                    .inherited_from
                    .as_ref()
                    .expect("inherited root column anchor");
                owners.table(&c.table, spelling).and_then(|table| {
                    owners
                        .column(root_column, spelling)
                        .map(|column| BoundRoot::InheritedColumn { table, column })
                })
            }
            Root::Column(c) if entry.local_column => {
                owners
                    .table(&c.table, spelling)
                    .map(|table| BoundRoot::LocalColumn {
                        table,
                        name: c.name.clone(),
                    })
            }
            Root::Column(c) => recorded(owners.column(c, spelling)),
            Root::Module(ModuleId::Trigger { on, name }) => {
                owners.table(on, spelling).map(|table| BoundRoot::Trigger {
                    table,
                    name: name.clone(),
                })
            }
            Root::Module(id) => Ok(BoundRoot::Module(id.clone())),
        }
        .map_err(|e| Problem::Unrecorded {
            reference: Box::new(reference.clone()),
            reason: e.to_string(),
        })?;
        Ok(ObjectRef {
            root,
            part: entry.reference.part.clone(),
        })
    }

    pub fn validate(
        &self,
        deps: &ObjectDeps,
        binding: Option<(&dyn Owners, Spelling)>,
    ) -> Vec<Problem> {
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
        enum Key {
            Named(ObjectRef),
            Bound(ObjectRef<BoundRoot>),
        }
        let key = |r: &ObjectRef| -> Result<Key, Problem> {
            let entry = self.entry(r)?;
            match binding {
                Some((owners, spelling)) => self.bind(r, owners, spelling).map(Key::Bound),
                None => Ok(Key::Named((self.normalize)(&entry.reference))),
            }
        };
        let mut problems = Vec::new();
        let mut graph = BTreeMap::<Key, BTreeSet<Key>>::new();
        let mut labels = BTreeMap::new();
        for (owner, targets) in &deps.0 {
            let owner_key = match key(owner) {
                Ok(key) => key,
                Err(Problem::UnknownTarget(_)) => {
                    problems.push(Problem::UnknownOwner(owner.clone()));
                    for target in targets {
                        if let Err(problem) = key(target) {
                            problems.push(problem);
                        }
                    }
                    continue;
                }
                Err(p) => {
                    problems.push(p);
                    continue;
                }
            };
            if graph.contains_key(&owner_key) {
                problems.push(Problem::DuplicateOwner(owner.clone()));
            }
            graph.entry(owner_key.clone()).or_default();
            labels.insert(owner_key.clone(), owner.clone());
            for target in targets {
                match key(target) {
                    Ok(target_key) => {
                        if target_key == owner_key {
                            problems.push(Problem::SelfDependency(owner.clone()));
                        }
                        labels
                            .entry(target_key.clone())
                            .or_insert_with(|| target.clone());
                        graph
                            .entry(owner_key.clone())
                            .or_default()
                            .insert(target_key);
                    }
                    Err(p) => problems.push(p),
                }
            }
        }
        for component in components(&graph) {
            if component.len() < 2 {
                continue;
            } // self has a more precise finding
            let members: BTreeSet<_> = component.into_iter().collect();
            let legs = graph
                .iter()
                .filter(|(a, _)| members.contains(*a))
                .flat_map(|(a, bs)| {
                    bs.iter()
                        .filter(|b| members.contains(*b))
                        .map(|b| (labels[a].clone(), labels[b].clone()))
                })
                .collect();
            problems.push(Problem::Cycle(legs));
        }
        problems
    }
}

// Iterative Kosaraju avoids recursion and retained all-pairs reachability.
fn components<K: Clone + Ord>(graph: &BTreeMap<K, BTreeSet<K>>) -> Vec<Vec<K>> {
    let mut reverse = BTreeMap::<K, BTreeSet<K>>::new();
    for (a, bs) in graph {
        reverse.entry(a.clone()).or_default();
        for b in bs {
            reverse.entry(b.clone()).or_default().insert(a.clone());
        }
    }
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    for start in reverse.keys() {
        let mut stack = vec![(start.clone(), false)];
        while let Some((node, finish)) = stack.pop() {
            if finish {
                order.push(node);
            } else if seen.insert(node.clone()) {
                stack.push((node.clone(), true));
                if let Some(next) = graph.get(&node) {
                    stack.extend(next.iter().rev().cloned().map(|n| (n, false)));
                }
            }
        }
    }
    seen.clear();
    let mut out = Vec::new();
    for start in order.into_iter().rev() {
        if seen.contains(&start) {
            continue;
        }
        let mut stack = vec![start];
        let mut members = Vec::new();
        while let Some(node) = stack.pop() {
            if seen.insert(node.clone()) {
                stack.extend(reverse[&node].iter().cloned());
                members.push(node);
            }
        }
        members.sort();
        out.push(members);
    }
    out
}
