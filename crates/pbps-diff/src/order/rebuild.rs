//! Non-executable factory preparation (DEC-2298.8); Expansion owns execution.
use super::{
    catalog::{Dependent, Holds, Part, graph::*},
    slots,
};
use pbps_dialect::{
    Dialect,
    slots::{CatalogFacts, Namespace, Occupant, SlotKey},
};
use pbps_model::operation::Source as OriginSource;
use pbps_model::{operation::*, plan_names::PlanNames, *};
use std::collections::{BTreeMap, BTreeSet};
mod run;
pub use run::{Expanded, expand};

/// Engine naming receives shared release facts and the validated proposed ledger.
/// Its recipes are checked against every approved part and reserved member.
pub type Recipes<'a> = dyn FnMut(
        &slots::Slots,
        &IdsFile,
        &BTreeSet<ObjectKey>,
    ) -> Result<Vec<BuildPartitionedIndex>, String>
    + 'a;
pub struct Input<'a> {
    pub root: &'a RebuildPartitionedTable,
    pub before: crate::Side<'a>,
    pub desired: crate::Side<'a>,
    pub catalog: &'a Catalog,
    pub naming: &'a Naming,
    pub hints: &'a Hints,
    pub mode: PlanMode,
}
/// Purposes name completion obligations, never scheduling ranks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Tree,
    Copy,
    Own,
    Analyze,
    Swap,
    Retire,
    Teardown,
    Restoration,
}
#[derive(Debug)]
pub struct PartStep {
    pub change: PlannedChange,
    pub purpose: Purpose,
}
/// A read-only inventory, not a plan or permission to consume the recorded file.
/// ```compile_fail
/// fn upgrade(p: pbps_diff::order::rebuild::Prepared) -> pbps_diff::order::PlanningPlan { p }
/// ```
#[derive(Debug)]
pub struct Prepared {
    logical: Vec<PlannedChange>,
    target: ValidIds,
    addresses: Vec<IntermediateAddress>,
    parts: Vec<PartStep>,
    closure: Closure,
    slots: slots::Slots,
    names: CatalogFacts,
}
impl Prepared {
    pub fn target(&self) -> &ValidIds {
        &self.target
    }
    pub fn addresses(&self) -> &[IntermediateAddress] {
        &self.addresses
    }
    pub fn parts(&self) -> &[PartStep] {
        &self.parts
    }
    pub fn closure(&self) -> &Closure {
        &self.closure
    }
    pub fn slots(&self) -> &slots::Slots {
        &self.slots
    }
    pub fn names(&self) -> &CatalogFacts {
        &self.names
    }
    pub fn operations(&self) -> Operations {
        Operations::expanded(
            1,
            self.parts.iter().map(|p| {
                (
                    match p.purpose {
                        Purpose::Teardown => Origin::Teardown(OriginSource::Change(0)),
                        Purpose::Restoration => Origin::Restoration(OriginSource::Change(0)),
                        Purpose::Tree
                        | Purpose::Copy
                        | Purpose::Own
                        | Purpose::Analyze
                        | Purpose::Swap
                        | Purpose::Retire => Origin::SplitPart(0),
                    },
                    p.change.clone(),
                )
            }),
            ModuleEdits::Rebuilt,
        )
    }
    pub fn owners<'a>(&'a self, input: &'a Input<'a>) -> Result<PlanOwners<'a>, String> {
        PlanOwners::new(
            PlanNames::of(&self.logical, &self.target).map_err(|e| e.to_string())?,
            input.before.ids,
            &self.target,
        )
        .with_rebuild_addresses(input.root, input.desired.ids, &self.addresses)
        .map_err(|e| e.to_string())
    }
}
struct Factory<'a> {
    input: &'a Input<'a>,
    dialect: &'a dyn Dialect,
    out: Prepared,
    catalog_root: Root,
}
impl Factory<'_> {
    fn add(&mut self, change: Change, purpose: Purpose) {
        let risks = self.dialect.change_risks(&change);
        self.out.parts.push(PartStep {
            change: PlannedChange {
                risks,
                ..PlannedChange::new(change)
            },
            purpose,
        });
    }
    fn address(&self, uid: &Uid) -> RebuildRelation {
        RebuildRelation {
            uid: uid.clone(),
            name: self
                .out
                .addresses
                .iter()
                .find(|a| &a.uid == uid)
                .unwrap()
                .address
                .clone(),
        }
    }
    fn describe(&self) -> Result<slots::Slots, String> {
        let owners = self.out.owners(self.input)?;
        let before = Snapshot {
            schema: self.input.before.schema,
            ids: self.input.before.ids,
        };
        let result = slots::with_catalog_parts(
            slots::analyse(
                self.out.operations().as_slice(),
                &owners,
                before,
                self.dialect,
            ),
            self.input.catalog,
            &self.catalog_root,
            &owners,
            before,
            self.dialect,
        )?;
        if !result.unowned.is_empty() || !result.unlowered.is_empty() {
            return Err(format!(
                "{}: rebuild slots are unavailable: {:?} {:?}",
                self.input.root.table.shown(),
                result.unowned,
                result.unlowered
            ));
        }
        Ok(result)
    }
}

/// Prepare only typed parts and facts. No settle, SQL, certification or persistence.
pub fn prepare(
    input: &Input<'_>,
    dialect: &dyn Dialect,
    recipes: &mut Recipes<'_>,
) -> Result<Prepared, String> {
    // The copy of a chain of approvals composes their conversions and
    // honours every link's cap; lowering it is a later change (DEC-2298.12).
    if !input.root.earlier.is_empty() {
        return Err(format!(
            "{}: a rebuild composed over several approved links cannot be prepared yet",
            input.root.table.shown()
        ));
    }
    if dialect.name() != "postgres" || input.mode != PlanMode::Transactional {
        return Err(format!(
            "{}: rebuild requires PostgreSQL transactional mode",
            input.root.table.shown()
        ));
    }
    let target =
        crate::rebuild::target_identities(input.before, input.desired, input.root, dialect)
            .map_err(|e| e.to_string())?;
    let r = input.root;
    let catalog_root = Root::Table(input.before.ids.table_uid(&r.table).unwrap().clone());
    let closure = input
        .catalog
        .checked(&catalog_root, input.before.schema)
        .map_err(|why| format!("{}: {why}", r.table.shown()))?;
    let naming = input
        .naming
        .require(&format!("{}: namespace occupancy", r.table.shown()))?;
    for table in r.before.keys() {
        let found: Vec<_> = input
            .catalog
            .relations
            .iter()
            .filter(|(_, rel)| &rel.name == table)
            .collect();
        if found.len() != 1
            || !closure.nodes.contains_key(found[0].0)
            || naming
                .get(&table.schema)
                .and_then(|n| n.relations.get(table.name.as_str()))
                != Some(found[0].0)
        {
            return Err(format!(
                "{}: source relation identity is absent or ambiguous",
                table.shown()
            ));
        }
    }
    let mut addresses = Vec::new();
    let mut occupied: BTreeMap<String, BTreeSet<String>> = naming
        .iter()
        .map(|(schema, facts)| {
            (
                schema.clone(),
                facts
                    .relations
                    .keys()
                    .chain(facts.types.keys())
                    .map(|n| n.raw().to_owned())
                    .collect(),
            )
        })
        .collect();
    for table in r.before.keys().chain(r.after.keys()) {
        if !naming.get(&table.schema).is_some_and(|facts| facts.exists) {
            return Err(format!(
                "{}: namespace was not read or does not exist",
                table.shown()
            ));
        }
    }
    for (table, spec) in &input.desired.schema.tables {
        if let Some(used) = occupied.get_mut(&table.schema) {
            used.insert(table.name.clone());
            used.extend(
                spec.indexes
                    .keys()
                    .chain(spec.unique.keys())
                    .map(|n| n.raw().to_owned()),
            );
            used.extend(
                spec.primary_key
                    .iter()
                    .filter_map(|k| k.name.as_ref())
                    .map(|n| n.raw().to_owned()),
            );
        }
    }
    for (uid, table) in r
        .intent
        .source
        .tables
        .iter()
        .chain(&r.intent.replacement.tables)
    {
        let used = occupied.get_mut(&table.schema).unwrap();
        let base = format!("__pbps_rebuild_{uid}");
        let mut name = base.clone();
        let mut ordinal = 0;
        while used.contains(&name) {
            ordinal += 1;
            name = format!("{base}_{ordinal}");
        }
        used.insert(name.clone());
        addresses.push(IntermediateAddress {
            uid: uid.clone(),
            address: TableName::new(&table.schema, name),
        });
    }
    let names = CatalogFacts {
        occupants: naming
            .iter()
            .flat_map(|(schema, names)| {
                names.relations.iter().map(|(name, id)| Occupant {
                    slot: SlotKey {
                        namespace: Namespace::Relations(schema.clone()),
                        name: name.raw().into(),
                    },
                    kind: "relation".into(),
                    owner: input.catalog.relations.get(id).map(|r| r.name.clone()),
                    column: None,
                })
            })
            .collect(),
        ..CatalogFacts::default()
    };
    let mut rule = Factory {
        catalog_root,
        input,
        dialect,
        out: Prepared {
            logical: vec![PlannedChange::new(Change::RebuildPartitionedTable(
                Box::new(r.clone()),
            ))],
            target,
            addresses,
            parts: vec![],
            closure,
            slots: slots::Slots::default(),
            names,
        },
    };
    let parent_uid = rule.out.target.table_uid(&r.table).unwrap().clone();
    let parent = rule.address(&parent_uid);
    for (table, spec) in &r.after {
        let relation = rule.address(rule.out.target.table_uid(table).unwrap());
        let mut bare = spec.clone();
        bare.primary_key = None;
        bare.unique.clear();
        bare.checks.clear();
        bare.indexes.clear();
        bare.foreign_keys.clear();
        if let Some(of) = &mut bare.partition_of {
            of.parent = parent.name.clone();
        }
        rule.add(
            Change::CreateTable {
                uid: relation.uid,
                name: relation.name,
                table: Box::new(bare),
                beside_default: None,
                moves_at_most: None,
            },
            Purpose::Tree,
        );
    }
    let leaves = |ids: &RebuildIdentities| {
        ids.tables
            .iter()
            .filter(|(_, n)| **n != r.table)
            .map(|(uid, _)| rule.address(uid))
            .collect::<Vec<_>>()
    };
    let source = RebuildRelation {
        uid: input.before.ids.table_uid(&r.table).unwrap().clone(),
        name: r.table.clone(),
    };
    let source_partitions = r
        .intent
        .source
        .tables
        .iter()
        .filter(|(_, n)| **n != r.table)
        .map(|(uid, n)| RebuildRelation {
            uid: uid.clone(),
            name: n.clone(),
        })
        .collect();
    let target_partitions = leaves(&r.intent.replacement);
    let columns = r.after[&r.table]
        .columns
        .iter()
        .map(|(name, c)| CopyColumn {
            source: r.before[&r.table].columns.get(name).map(|was| CopySource {
                column: r.table.column(name),
                ty: was.ty.clone(),
                collation: was.collation.clone(),
            }),
            target: parent.name.column(name),
            cast: c.ty.clone(),
            collation: c.collation.clone(),
        })
        .collect();
    rule.add(
        Change::CopyTableRows(Box::new(CopyTableRows {
            source,
            target: parent.clone(),
            columns,
            source_partitions,
            target_partitions: target_partitions.clone(),
            max_rows: r.intent.max_rows,
        })),
        Purpose::Copy,
    );
    rule.add(
        Change::AnalyzeTableTree(Box::new(AnalyzeTableTree {
            parent: parent.clone(),
            partitions: target_partitions.clone(),
        })),
        Purpose::Analyze,
    );
    for identities in [&r.intent.source, &r.intent.replacement] {
        for (uid, final_name) in &identities.tables {
            let address = rule.address(uid).name;
            let old = r.intent.source.tables.contains_key(uid);
            let (from, to) = if old {
                (final_name.clone(), address)
            } else {
                (address, final_name.clone())
            };
            rule.add(
                Change::RenameTable {
                    uid: uid.clone(),
                    from,
                    to,
                    defaults: vec![],
                },
                Purpose::Swap,
            );
        }
    }
    let view_ids = crate::rebuild::logical_view(input.desired.ids, r);
    let sources = super::deployment::Sources {
        before: input.before,
        after: &view_ids,
    };
    let logical = rule.out.logical.clone();
    let view = sources.project(&logical)?;
    let location = |d: &Dependent, at| super::deployment::default_location(&view, &d.holds, at);
    for dependent in rule.out.closure.surfaces.clone().into_values() {
        rule.add(
            super::deployment::removal_of(
                &dependent,
                input.before.schema,
                location(&dependent, pbps_model::plan_names::At::Start),
            )?,
            Purpose::Teardown,
        );
        let own = matches!(&dependent.holds, Holds::TablePart { table, .. } if r.before.contains_key(table));
        if !own {
            rule.add(
                super::deployment::restoration_of(
                    &dependent,
                    input.desired.schema,
                    location(&dependent, pbps_model::plan_names::At::End),
                )?,
                Purpose::Restoration,
            );
        }
    }
    for (table, spec) in &r.after {
        let relation = rule.address(rule.out.target.table_uid(table).unwrap());
        let mut own: Vec<_> = spec.checks.keys().cloned().map(Part::Check).collect();
        if spec.partition_of.is_some() {
            own.extend(spec.indexes.keys().cloned().map(Part::Index));
            own.extend(spec.unique.keys().cloned().map(Part::Unique));
            own.extend(spec.primary_key.iter().cloned().map(Part::PrimaryKey));
        }
        let at = Schema {
            tables: [(relation.name.clone(), spec.clone())].into(),
            ..Default::default()
        };
        for part in own {
            let dependent = Dependent {
                described: table.shown().to_string(),
                holds: Holds::TablePart {
                    table: relation.name.clone(),
                    part,
                },
            };
            rule.add(
                super::deployment::restoration_of(&dependent, &at, None)?,
                Purpose::Own,
            );
        }
        for (name, fk) in &spec.foreign_keys {
            rule.add(
                Change::AddForeignKey {
                    table: table.clone(),
                    name: name.clone(),
                    constraint: Box::new(fk.clone()),
                },
                Purpose::Restoration,
            );
        }
    }
    let ops = rule.out.operations();
    let slot_description = rule.describe()?;
    let released = ops
        .iter()
        .zip(&slot_description.effects)
        .filter(|(o, _)| o.role() == pbps_model::operation::Role::Teardown)
        .flat_map(|(_, fx)| fx.releases.iter().map(|(_, h)| h.clone()))
        .collect();
    let expected = index_definitions(&r.after[&r.table]);
    let invalid_recipe = |why| format!("{}: {why}", r.table.shown());
    let mut seen = BTreeSet::new();
    for mut recipe in recipes(&slot_description, &rule.out.target, &released)? {
        recipe.validate().map_err(invalid_recipe)?;
        let part = recipe.part();
        let mut definition = recipe.definition.clone();
        if let PartitionIndexDefinition::PrimaryKey(key) = &mut definition
            && r.after[&r.table]
                .primary_key
                .as_ref()
                .is_some_and(|k| k.name.is_none())
        {
            key.name = None;
        }
        if !seen.insert(part.clone())
            || expected.get(&part) != Some(&definition)
            || recipe.parent.uid != parent_uid
            || recipe.parent.name != r.table
            || recipe.clones.len() + 1 != r.intent.replacement.tables.len()
            || recipe.clones.iter().any(|c| {
                c.leaf.name == r.table
                    || r.intent.replacement.tables.get(&c.leaf.uid) != Some(&c.leaf.name)
            })
        {
            return Err(format!(
                "{}: index recipe differs from the approved parts or reserved lineage",
                r.table.shown()
            ));
        }
        recipe.parent = parent.clone();
        for clone in &mut recipe.clones {
            clone.leaf = rule.address(&clone.leaf.uid);
        }
        recipe.validate().map_err(invalid_recipe)?;
        rule.add(
            Change::BuildPartitionedIndex(Box::new(recipe)),
            Purpose::Own,
        );
    }
    if seen != expected.keys().cloned().collect() {
        return Err(format!(
            "{}: index recipe coverage is incomplete",
            r.table.shown()
        ));
    }
    let modules: BTreeSet<_> = rule
        .out
        .closure
        .surfaces
        .values()
        .filter_map(|d| {
            if let Holds::Module(m) = &d.holds {
                Some(m.clone())
            } else {
                None
            }
        })
        .collect();
    for (role, definition) in &input.desired.schema.roles {
        for (target, permissions) in definition.declarable_grants() {
            let affected = match target {
                GrantTarget::Object(n) => {
                    r.before.contains_key(n) || modules.contains(&ModuleId::Named(n.clone()))
                }
                GrantTarget::Routine(r) => modules.contains(&ModuleId::Routine(r.clone())),
                GrantTarget::Schema(_) | GrantTarget::Sequence(_) => false,
            };
            if affected {
                rule.add(
                    Change::Grant {
                        role: role.clone(),
                        target: target.clone(),
                        permissions: permissions.clone(),
                    },
                    Purpose::Restoration,
                );
            }
        }
    }
    for module in modules {
        if let ModuleId::Routine(routine) = &module {
            rule.add(
                Change::PublicExecution {
                    routine: routine.clone(),
                    access: if input.hints.public_execute.contains(&module) {
                        PublicAccess::Kept
                    } else {
                        PublicAccess::Revoked
                    },
                    origin: RoutineOrigin::Rebuilt,
                },
                Purpose::Restoration,
            );
        }
    }
    // Declare implicit content removals last for the existing write-write contract.
    for uid in r.intent.source.tables.keys() {
        let relation = rule.address(uid);
        let parent = (uid != input.before.ids.table_uid(&r.table).unwrap()).then(|| {
            rule.address(input.before.ids.table_uid(&r.table).unwrap())
                .name
        });
        rule.add(
            Change::DropTable {
                uid: uid.clone(),
                name: relation.name,
                detach_from: parent,
            },
            Purpose::Retire,
        );
    }
    rule.out.slots = rule.describe()?;
    Ok(rule.out)
}

/// Parent-owned surfaces in declaration order. Engine naming adds no logical part.
pub fn index_definitions(table: &Table) -> BTreeMap<IndexPart, PartitionIndexDefinition> {
    table
        .primary_key
        .iter()
        .map(|k| {
            (
                IndexPart::PrimaryKey,
                PartitionIndexDefinition::PrimaryKey(k.clone()),
            )
        })
        .chain(table.unique.iter().map(|(n, k)| {
            (
                IndexPart::Unique(n.clone()),
                PartitionIndexDefinition::Unique(k.clone()),
            )
        }))
        .chain(table.indexes.iter().map(|(n, k)| {
            (
                IndexPart::Index(n.clone()),
                PartitionIndexDefinition::Index(k.clone()),
            )
        }))
        .collect()
}
