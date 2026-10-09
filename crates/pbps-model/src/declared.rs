//! What was declared when an object was last written through this tool.
//!
//! PostgreSQL hands back its own spelling of whatever it was given — a view is
//! deparsed, a default comes back with a cast welded on, a check gains
//! parentheses — and SQL Server does the same to every expression it stores
//! (measured: `n > 0 AND label <> 'none'` is read back as
//! `[n]>(0) AND [label]<>'none'`, `GETDATE()` as `getdate()`). A differ that
//! compared the declaration against the read-back therefore restated an
//! unchanged check, and dropped and rebuilt an unchanged filtered index, on
//! every connected plan, for ever (ADR-0009 §2.2, ADR-0013 §4).
//!
//! So the recorded state keeps, beside the read-back, what was **declared**
//! when each object was last written: the differ compares declared-now against
//! declared-at-last-apply, and drift compares read-back against read-back.
//! Neither comparison puts a hand-written text beside a respelled one.
//!
//! Where no declared text is recorded — an environment adopted with
//! `baseline`, a snapshot from before this field, an object created by hand —
//! the differ falls back to the read-back, which is what it always compared.
//! On an engine that stores what it was given that is the same answer as
//! before; on one that respells, the object is restated once, and the apply
//! records what it declared (DECISIONS 208).
use std::collections::{BTreeMap, BTreeSet};

use crate::change::{Change, ChangeSet};
use crate::module::ModuleId;
use crate::name::TableName;
use crate::schema::Schema;

/// The three expressions the model holds verbatim, as declared (ADR-0013 §4).
///
/// Keyed by table and then by the column, constraint or index name, as the
/// schema is: containers hold names, elements do not.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredExpressions {
    /// `Column::default`, by table and column.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub defaults: BTreeMap<TableName, BTreeMap<String, String>>,
    /// `Generated::expression`, by table and column (DEC-1168.1).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub generated: BTreeMap<TableName, BTreeMap<String, String>>,
    /// `ComputedColumn::expression`, by table and computed column (#1174).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub computed: BTreeMap<TableName, BTreeMap<String, String>>,
    /// `CheckConstraint::expression`, by table and constraint name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<TableName, BTreeMap<String, String>>,
    /// `Index::filter`, by table and index name; only filtered indexes appear.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub filters: BTreeMap<TableName, BTreeMap<String, String>>,
    /// An index's keys, by table and index name, one entry per key: the
    /// declared text of an expression key and `None` for a column key. Only
    /// indexes with an expression key appear (DEC-1169.2).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<TableName, BTreeMap<String, Vec<Option<String>>>>,
}

impl DeclaredExpressions {
    pub fn is_empty(&self) -> bool {
        self.defaults.is_empty()
            && self.generated.is_empty()
            && self.computed.is_empty()
            && self.checks.is_empty()
            && self.filters.is_empty()
            && self.keys.is_empty()
    }
}

/// An index's keys as `DeclaredExpressions::keys` records them, or `None`
/// where every key is a column and there is nothing to record.
fn declared_keys(index: &crate::schema::Index) -> Option<Vec<Option<String>>> {
    index
        .columns
        .iter()
        .any(|c| c.key.expression().is_some())
        .then(|| {
            index
                .columns
                .iter()
                .map(|c| c.key.expression().map(str::to_owned))
                .collect()
        })
}

/// What one declaration's unqualified references resolved to when it was
/// created (ADR-0013 §3): for each referenced name, the candidate set of
/// same-named objects of the same catalog class visible on the effective
/// write path, by identity for routines. Any difference between this set and
/// the one the catalog will hold once a plan has run rebuilds the object once.
///
/// Recorded by the dialect that has a search path to resolve against. SQL
/// Server has none — an unqualified name binds to the caller's default schema
/// at execution, which is a property of the session and not of the object —
/// so it records nothing here (DECISIONS 209).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// The name as written in the declaration, to the qualified identities it
    /// could have resolved to.
    pub candidates: BTreeMap<String, BTreeSet<String>>,
}

/// The recorded bindings of every managed object that carries a body or an
/// expression the engine resolves at creation (ADR-0013 §3).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bindings {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub modules: BTreeMap<ModuleId, Binding>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub defaults: BTreeMap<TableName, BTreeMap<String, Binding>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<TableName, BTreeMap<String, Binding>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub filters: BTreeMap<TableName, BTreeMap<String, Binding>>,
}

impl Bindings {
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
            && self.defaults.is_empty()
            && self.checks.is_empty()
            && self.filters.is_empty()
    }
}

/// Everything a state records as *declared*, beside the schema it read back.
///
/// One struct rather than three loose fields on the snapshot, because the
/// three move together: an apply advances all of them by the plan it ran, a
/// baseline leaves all of them empty, and the differ overlays all of them on
/// the read-back it compares against.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declared {
    /// Each managed module's definition as declared when it was last written
    /// (ADR-0009 §2.2). A module absent here was never written through this
    /// tool — adopted, or created by hand — and is compared by its read-back.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub modules: BTreeMap<ModuleId, String>,
    #[serde(default, skip_serializing_if = "DeclaredExpressions::is_empty")]
    pub expressions: DeclaredExpressions,
    #[serde(default, skip_serializing_if = "Bindings::is_empty")]
    pub bindings: Bindings,
}

fn remove_nested<V>(
    map: &mut BTreeMap<TableName, BTreeMap<String, V>>,
    table: &TableName,
    name: &str,
) {
    if let Some(inner) = map.get_mut(table) {
        inner.remove(name);
        if inner.is_empty() {
            map.remove(table);
        }
    }
}

/// `from`'s records copied to `to`, each key passed through `rename`.
fn copy_table<V: Clone>(
    map: &mut BTreeMap<TableName, BTreeMap<String, V>>,
    from: &TableName,
    to: &TableName,
    rename: impl Fn(&String) -> String,
) {
    let Some(records) = map.get(from) else {
        return;
    };
    let copied: BTreeMap<String, V> = records
        .iter()
        .map(|(k, v)| (rename(k), v.clone()))
        .collect();
    map.insert(to.clone(), copied);
}

fn rekey_table<V>(
    map: &mut BTreeMap<TableName, BTreeMap<String, V>>,
    from: &TableName,
    to: &TableName,
) {
    if let Some(inner) = map.remove(from) {
        map.insert(to.clone(), inner);
    }
}

fn rekey_nested<V>(
    map: &mut BTreeMap<TableName, BTreeMap<String, V>>,
    table: &TableName,
    from: &str,
    to: &str,
) {
    if let Some(inner) = map.get_mut(table)
        && let Some(v) = inner.remove(from)
    {
        inner.insert(to.to_owned(), v);
    }
}

impl Declared {
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty() && self.expressions.is_empty() && self.bindings.is_empty()
    }

    /// Every module text and expression a schema declares, recorded as
    /// declared. What `bootstrap` records, since it creates everything from
    /// the declarations it holds; bindings are the dialect's to add.
    pub fn from_schema(schema: &Schema) -> Self {
        let mut d = Self::default();
        for (id, module) in &schema.modules {
            d.modules.insert(id.clone(), module.definition.clone());
        }
        for (name, table) in &schema.tables {
            for (column, spec) in &table.columns {
                if let Some(default) = &spec.default {
                    d.expressions
                        .defaults
                        .entry(name.clone())
                        .or_default()
                        .insert(column.clone(), default.clone());
                }
                if let Some(generated) = &spec.generated {
                    d.expressions
                        .generated
                        .entry(name.clone())
                        .or_default()
                        .insert(column.clone(), generated.expression.clone());
                }
            }
            // A partition's own default, under the partition and the column, as
            // a column's is: the partition has no column of its own the two
            // could collide in (#1578).
            for (column, own) in table.partition_of.iter().flat_map(|of| &of.columns) {
                if let Some(default) = &own.default {
                    d.expressions
                        .defaults
                        .entry(name.clone())
                        .or_default()
                        .insert(column.clone(), default.clone());
                }
            }
            for (computed, spec) in &table.computed {
                d.expressions
                    .computed
                    .entry(name.clone())
                    .or_default()
                    .insert(computed.clone(), spec.expression.clone());
            }
            for (check, spec) in &table.checks {
                d.expressions
                    .checks
                    .entry(name.clone())
                    .or_default()
                    .insert(check.clone(), spec.expression.clone());
            }
            for (index, spec) in &table.indexes {
                if let Some(filter) = &spec.filter {
                    d.expressions
                        .filters
                        .entry(name.clone())
                        .or_default()
                        .insert(index.clone(), filter.clone());
                }
                if let Some(keys) = declared_keys(spec) {
                    d.expressions
                        .keys
                        .entry(name.clone())
                        .or_default()
                        .insert(index.clone(), keys);
                }
            }
        }
        d
    }

    /// Carries the record forward over the changes a plan ran.
    ///
    /// From the plan alone, because `apply --plan` needs nothing but the plan
    /// file (SPEC §7.3): a change that writes an object carries the declared
    /// text it wrote, a drop removes the record, a rename re-keys it, and
    /// everything the plan leaves alone keeps what the previous state said.
    /// A binding is the dialect's to record at creation, so here it only
    /// follows drops and renames.
    pub fn advance(&mut self, changes: &ChangeSet) {
        self.advance_over(changes, None);
    }

    /// [`Self::advance`], and a parent's column rename or drop carried into
    /// the records of its partitions, which the engine renames or drops with
    /// it (#1687). The plan names only the parent, so the partitions are read
    /// from `state`, the state the plan leaves (#1692 review).
    pub fn advance_with_partitions(&mut self, changes: &ChangeSet, state: &Schema) {
        self.advance_over(changes, Some(state));
    }

    fn advance_over(&mut self, changes: &ChangeSet, state: Option<&Schema>) {
        let partitions = |parent: &TableName| -> Vec<TableName> {
            state
                .into_iter()
                .flat_map(|s| &s.tables)
                .filter(|(_, t)| {
                    t.partition_of
                        .as_ref()
                        .is_some_and(|of| &of.parent == parent)
                })
                .map(|(name, _)| name.clone())
                .collect()
        };
        for planned in &changes.changes {
            match &planned.change {
                Change::CreateTable { name, table, .. } => {
                    let fresh = Self::from_schema(&Schema {
                        tables: [(name.clone(), (**table).clone())].into_iter().collect(),
                        ..Schema::default()
                    });
                    self.forget_table(name);
                    self.expressions.defaults.extend(fresh.expressions.defaults);
                    self.expressions
                        .generated
                        .extend(fresh.expressions.generated);
                    self.expressions.computed.extend(fresh.expressions.computed);
                    self.expressions.checks.extend(fresh.expressions.checks);
                    self.expressions.filters.extend(fresh.expressions.filters);
                    self.expressions.keys.extend(fresh.expressions.keys);
                }
                Change::DropTable { name, .. } => self.forget_table(name),
                // The detached table holds its parent's expressions, which
                // the engine copied into it, under its own name, and its
                // checks and index filters under the declared names (#1544).
                Change::DetachPartition {
                    table,
                    parent,
                    names,
                    shape,
                    ..
                } => {
                    // The declared text is the shape's, as a created table's
                    // is its payload's. The bindings are its parent's, which
                    // the engine copied into it, under the declared names.
                    let fresh = Self::from_schema(&Schema {
                        tables: [(table.clone(), (**shape).clone())].into_iter().collect(),
                        ..Schema::default()
                    });
                    self.forget_table(table);
                    self.expressions.defaults.extend(fresh.expressions.defaults);
                    self.expressions
                        .generated
                        .extend(fresh.expressions.generated);
                    self.expressions.computed.extend(fresh.expressions.computed);
                    self.expressions.checks.extend(fresh.expressions.checks);
                    self.expressions.filters.extend(fresh.expressions.filters);
                    self.expressions.keys.extend(fresh.expressions.keys);
                    let renamed = |kind: crate::DetachedKind, of: &String| {
                        names
                            .iter()
                            .find(|n| n.kind == kind && &n.parent == of)
                            .and_then(|n| n.name.clone())
                            .unwrap_or_else(|| of.clone())
                    };
                    copy_table(&mut self.bindings.defaults, parent, table, String::clone);
                    copy_table(&mut self.bindings.checks, parent, table, |of| {
                        renamed(crate::DetachedKind::Check, of)
                    });
                    copy_table(&mut self.bindings.filters, parent, table, |of| {
                        renamed(crate::DetachedKind::Index, of)
                    });
                }
                // The attached table keeps its own defaults, checks and
                // indexes where they stand, under its own name; an index its
                // parent's matches becomes a clone, which the parent's
                // records answer for (#1545). The declared text is the
                // shape's, as a created table's is its payload's, and a
                // binding stays only with an object the shape still holds as
                // its own: the partition's later changes re-record the rest.
                Change::AttachPartition { table, shape, .. } => {
                    let fresh = Self::from_schema(&Schema {
                        tables: [(table.clone(), (**shape).clone())].into_iter().collect(),
                        ..Schema::default()
                    });
                    let mut defaults = self.bindings.defaults.remove(table).unwrap_or_default();
                    let mut checks = self.bindings.checks.remove(table).unwrap_or_default();
                    let mut filters = self.bindings.filters.remove(table).unwrap_or_default();
                    self.forget_table(table);
                    let held = |map: &BTreeMap<TableName, BTreeMap<String, String>>,
                                key: &String| {
                        map.get(table).is_some_and(|m| m.contains_key(key))
                    };
                    defaults.retain(|column, _| held(&fresh.expressions.defaults, column));
                    checks.retain(|name, _| held(&fresh.expressions.checks, name));
                    filters.retain(|name, _| held(&fresh.expressions.filters, name));
                    for (map, kept) in [
                        (&mut self.bindings.defaults, defaults),
                        (&mut self.bindings.checks, checks),
                        (&mut self.bindings.filters, filters),
                    ] {
                        if !kept.is_empty() {
                            map.insert(table.clone(), kept);
                        }
                    }
                    self.expressions.defaults.extend(fresh.expressions.defaults);
                    self.expressions.checks.extend(fresh.expressions.checks);
                    self.expressions.filters.extend(fresh.expressions.filters);
                    self.expressions.keys.extend(fresh.expressions.keys);
                }
                Change::RenameTable { from, to, .. } => {
                    rekey_table(&mut self.expressions.defaults, from, to);
                    rekey_table(&mut self.expressions.generated, from, to);
                    rekey_table(&mut self.expressions.computed, from, to);
                    rekey_table(&mut self.expressions.checks, from, to);
                    rekey_table(&mut self.expressions.filters, from, to);
                    rekey_table(&mut self.expressions.keys, from, to);
                    rekey_table(&mut self.bindings.defaults, from, to);
                    rekey_table(&mut self.bindings.checks, from, to);
                    rekey_table(&mut self.bindings.filters, from, to);
                }
                Change::AddColumn {
                    table,
                    name,
                    column,
                    ..
                } => {
                    remove_nested(&mut self.expressions.defaults, table, name);
                    remove_nested(&mut self.expressions.generated, table, name);
                    remove_nested(&mut self.bindings.defaults, table, name);
                    if let Some(default) = &column.default {
                        self.expressions
                            .defaults
                            .entry(table.clone())
                            .or_default()
                            .insert(name.clone(), default.clone());
                    }
                    if let Some(generated) = &column.generated {
                        self.expressions
                            .generated
                            .entry(table.clone())
                            .or_default()
                            .insert(name.clone(), generated.expression.clone());
                    }
                }
                Change::DropColumn { column, .. } => {
                    remove_nested(&mut self.expressions.defaults, &column.table, &column.name);
                    remove_nested(&mut self.expressions.generated, &column.table, &column.name);
                    remove_nested(&mut self.bindings.defaults, &column.table, &column.name);
                    for partition in partitions(&column.table) {
                        remove_nested(&mut self.expressions.defaults, &partition, &column.name);
                        remove_nested(&mut self.bindings.defaults, &partition, &column.name);
                    }
                }
                Change::RenameColumn {
                    table, from, to, ..
                } => {
                    rekey_nested(&mut self.expressions.defaults, table, from, to);
                    rekey_nested(&mut self.expressions.generated, table, from, to);
                    rekey_nested(&mut self.bindings.defaults, table, from, to);
                    for partition in partitions(table) {
                        rekey_nested(&mut self.expressions.defaults, &partition, from, to);
                        rekey_nested(&mut self.bindings.defaults, &partition, from, to);
                    }
                }
                Change::AlterColumnDefault { column, to, .. } => {
                    remove_nested(&mut self.expressions.defaults, &column.table, &column.name);
                    remove_nested(&mut self.bindings.defaults, &column.table, &column.name);
                    if let Some(default) = to {
                        self.expressions
                            .defaults
                            .entry(column.table.clone())
                            .or_default()
                            .insert(column.name.clone(), default.clone());
                    }
                }
                // A partition's own default, recorded where `from_schema`
                // records it: under the partition and the column (#1581).
                Change::SetPartitionDefault {
                    table, column, to, ..
                } => {
                    remove_nested(&mut self.expressions.defaults, table, column);
                    remove_nested(&mut self.bindings.defaults, table, column);
                    if let Some(default) = to {
                        self.expressions
                            .defaults
                            .entry(table.clone())
                            .or_default()
                            .insert(column.clone(), default.clone());
                    }
                }
                Change::AlterColumnExpression { column, to, .. } => {
                    self.expressions
                        .generated
                        .entry(column.table.clone())
                        .or_default()
                        .insert(column.name.clone(), to.clone());
                }
                Change::AddComputedColumn {
                    table,
                    name,
                    computed,
                } => {
                    self.expressions
                        .computed
                        .entry(table.clone())
                        .or_default()
                        .insert(name.clone(), computed.expression.clone());
                }
                Change::DropComputedColumn { table, name, .. } => {
                    remove_nested(&mut self.expressions.computed, table, name);
                }
                Change::AddCheck {
                    table,
                    name,
                    constraint,
                } => {
                    remove_nested(&mut self.bindings.checks, table, name);
                    self.expressions
                        .checks
                        .entry(table.clone())
                        .or_default()
                        .insert(name.clone(), constraint.expression.clone());
                }
                Change::DropCheck { table, name } => {
                    remove_nested(&mut self.expressions.checks, table, name);
                    remove_nested(&mut self.bindings.checks, table, name);
                }
                Change::AddIndex {
                    table, name, index, ..
                } => {
                    remove_nested(&mut self.expressions.filters, table, name);
                    remove_nested(&mut self.expressions.keys, table, name);
                    remove_nested(&mut self.bindings.filters, table, name);
                    if let Some(filter) = &index.filter {
                        self.expressions
                            .filters
                            .entry(table.clone())
                            .or_default()
                            .insert(name.clone(), filter.clone());
                    }
                    if let Some(keys) = declared_keys(index) {
                        self.expressions
                            .keys
                            .entry(table.clone())
                            .or_default()
                            .insert(name.clone(), keys);
                    }
                }
                Change::DropIndex { table, name } => {
                    remove_nested(&mut self.expressions.filters, table, name);
                    remove_nested(&mut self.expressions.keys, table, name);
                    remove_nested(&mut self.bindings.filters, table, name);
                }
                Change::CreateModule { id, module } | Change::AlterModule { id, module } => {
                    self.bindings.modules.remove(id);
                    self.modules.insert(id.clone(), module.definition.clone());
                }
                Change::DropModule { id, .. } => {
                    self.modules.remove(id);
                    self.bindings.modules.remove(id);
                }
                // Nothing these write is compared as text against a declaration.
                Change::AlterColumnType { .. }
                | Change::AlterColumnNullability { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::SetIndexStorageParameters { .. }
                | Change::SetTablePersistence { .. }
                | Change::SetStorageParameters { .. }
                | Change::SetPartitionNotNull { .. }
                | Change::SetReplicaIdentity { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddForeignKey { .. }
                | Change::DropForeignKey { .. }
                | Change::InsertRow { .. }
                | Change::UpdateRow { .. }
                | Change::DeleteRow { .. }
                | Change::SetDataMode { .. }
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. }
                | Change::PublicExecution { .. } => {}
            }
        }
    }

    fn forget_table(&mut self, name: &TableName) {
        self.expressions.defaults.remove(name);
        self.expressions.generated.remove(name);
        self.expressions.computed.remove(name);
        self.expressions.checks.remove(name);
        self.expressions.filters.remove(name);
        self.expressions.keys.remove(name);
        self.bindings.defaults.remove(name);
        self.bindings.checks.remove(name);
        self.bindings.filters.remove(name);
    }

    /// The read-back with every recorded declared text put in place of the
    /// engine's spelling of it: the base a differ compares declarations against.
    ///
    /// Only where the read-back holds the object — a recorded module the
    /// database no longer has is not conjured back; the differ sees it missing
    /// and plans its creation. And only the texts, never the structure: the
    /// columns, types and names are the read-back's, which is what drift and
    /// the movement guard also see.
    pub fn overlay(&self, read_back: &Schema) -> Schema {
        let mut base = read_back.clone();
        for (id, module) in &mut base.modules {
            if let Some(declared) = self.modules.get(id) {
                module.definition = declared.clone();
            }
        }
        for (name, table) in &mut base.tables {
            if let Some(defaults) = self.expressions.defaults.get(name) {
                for (column, spec) in &mut table.columns {
                    if let Some(declared) = defaults.get(column)
                        && spec.default.is_some()
                    {
                        spec.default = Some(declared.clone());
                    }
                }
                // And a partition's own, where the read-back has one (#1578).
                for (column, own) in table.partition_of.iter_mut().flat_map(|of| &mut of.columns) {
                    if let Some(declared) = defaults.get(column)
                        && own.default.is_some()
                    {
                        own.default = Some(declared.clone());
                    }
                }
            }
            // Presence-compared, as a default is: only over a column the
            // read-back holds as generated (DEC-1168.1).
            if let Some(generated) = self.expressions.generated.get(name) {
                for (column, spec) in &mut table.columns {
                    if let Some(declared) = generated.get(column)
                        && let Some(read) = &mut spec.generated
                    {
                        read.expression = declared.clone();
                    }
                }
            }
            if let Some(computed) = self.expressions.computed.get(name) {
                for (column, spec) in &mut table.computed {
                    if let Some(declared) = computed.get(column) {
                        spec.expression = declared.clone();
                    }
                }
            }
            if let Some(checks) = self.expressions.checks.get(name) {
                for (check, spec) in &mut table.checks {
                    if let Some(declared) = checks.get(check) {
                        spec.expression = declared.clone();
                    }
                }
            }
            if let Some(filters) = self.expressions.filters.get(name) {
                for (index, spec) in &mut table.indexes {
                    if let Some(declared) = filters.get(index)
                        && spec.filter.is_some()
                    {
                        spec.filter = Some(declared.clone());
                    }
                }
            }
            // Only where the read-back has the recorded shape, an expression
            // where one was declared and a column where one was: the texts are
            // compared by presence, as a filter's are, and a key that became a
            // column or the other way round is a difference the differ must
            // see (DEC-1169.2).
            if let Some(keys) = self.expressions.keys.get(name) {
                for (index, spec) in &mut table.indexes {
                    let Some(declared) = keys.get(index) else {
                        continue;
                    };
                    let same_shape = declared.len() == spec.columns.len()
                        && declared
                            .iter()
                            .zip(&spec.columns)
                            .all(|(d, c)| d.is_some() == c.key.expression().is_some());
                    if same_shape {
                        for (d, c) in declared.iter().zip(&mut spec.columns) {
                            if let Some(text) = d {
                                c.key = crate::IndexKey::Expression(text.clone());
                            }
                        }
                    }
                }
            }
        }
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::PlannedChange;
    use crate::module::{Module, ModuleKind};
    use crate::name::ColumnRef;
    use crate::schema::{CheckConstraint, Column, Index, IndexColumn, Table};

    /// A partition's own default is recorded as declared and put back in
    /// place of the engine's spelling, under the partition and the column
    /// (#1578); one the read-back does not have is not conjured back.
    #[test]
    fn a_partitions_own_default_is_recorded_and_overlaid() {
        let partition = |default: Option<&str>| Table {
            partition_of: Some(crate::PartitionOf {
                parent: "app.ev".parse().unwrap(),
                bound: crate::PartitionBound::Default,
                columns: [(
                    "v".to_owned(),
                    crate::PartitionColumn {
                        default: default.map(str::to_owned),
                        not_null: true,
                    },
                )]
                .into_iter()
                .collect(),
            }),
            ..Table::default()
        };
        let name: TableName = "app.p".parse().unwrap();
        let schema = |t: Table| Schema {
            tables: [(name.clone(), t)].into_iter().collect(),
            ..Schema::default()
        };
        let declared = Declared::from_schema(&schema(partition(Some("'y'"))));
        assert_eq!(
            declared.expressions.defaults[&name]["v"], "'y'",
            "{declared:?}"
        );
        let base = declared.overlay(&schema(partition(Some("'y'::text"))));
        assert_eq!(base, schema(partition(Some("'y'"))));
        // Negative: the read-back's NOT NULL alone gains no default.
        let base = declared.overlay(&schema(partition(None)));
        assert_eq!(base, schema(partition(None)));
    }
    fn t() -> TableName {
        "dbo.t".parse().unwrap()
    }

    fn table() -> Table {
        let mut table = Table::default();
        let mut n = Column::new("int".parse().unwrap());
        n.default = Some("0".to_owned());
        table.columns.insert("n".to_owned(), n);
        table
            .columns
            .insert("m".to_owned(), Column::new("int".parse().unwrap()));
        table.checks.insert(
            "ck_n".to_owned(),
            CheckConstraint {
                expression: "n > 0".to_owned(),
            },
        );
        table.indexes.insert(
            "ix_n".to_owned(),
            Index {
                columns: vec![IndexColumn {
                    key: crate::IndexKey::Column("n".to_owned()),
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique: false,
                filter: Some("n > 0".to_owned()),
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        table.indexes.insert(
            "ix_m".to_owned(),
            Index {
                columns: vec![IndexColumn {
                    key: crate::IndexKey::Column("m".to_owned()),
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        table
    }

    fn view(body: &str) -> Module {
        Module {
            kind: ModuleKind::View,
            description: None,
            definition: body.to_owned(),
        }
    }

    fn schema() -> Schema {
        let mut s = Schema::default();
        s.tables.insert(t(), table());
        s.modules.insert("dbo.v".parse().unwrap(), view("SELECT 1"));
        s
    }

    fn changes(list: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes: list.into_iter().map(PlannedChange::new).collect(),
        }
    }

    /// An index with an expression key records one entry per key, the
    /// declared text for the expression and `None` for the column; the
    /// overlay puts that text over the engine's respelling only where the
    /// read-back has the same shape; and a plan's `AddIndex`, `DropIndex`
    /// and table rename carry the record as they carry a filter's
    /// (DEC-1169.2). An index of columns alone records nothing.
    #[test]
    fn an_expression_keys_declared_text_is_recorded_overlaid_and_advanced() {
        use crate::IndexKey;
        let key = |k: IndexKey| IndexColumn {
            key: k,
            descending: false,
            opclass: None,
        };
        let expression_index = |text: &str| Index {
            columns: vec![
                key(IndexKey::Expression(text.to_owned())),
                key(IndexKey::Column("n".to_owned())),
            ],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        let mut declared = schema();
        declared
            .tables
            .get_mut(&t())
            .unwrap()
            .indexes
            .insert("ix_expr".to_owned(), expression_index("n+m"));
        let d = Declared::from_schema(&declared);
        assert_eq!(
            d.expressions.keys[&t()]["ix_expr"],
            [Some("n+m".to_owned()), None]
        );
        // Negative: indexes of columns alone record nothing.
        assert_eq!(d.expressions.keys[&t()].len(), 1);

        // The engine's respelling is overlaid where the shape matches...
        let mut read = schema();
        read.tables
            .get_mut(&t())
            .unwrap()
            .indexes
            .insert("ix_expr".to_owned(), expression_index("(n + m)"));
        let base = d.overlay(&read);
        assert_eq!(
            base.tables[&t()].indexes["ix_expr"].columns[0].key,
            IndexKey::Expression("n+m".to_owned())
        );
        // ...and not where a key changed kind: that is a difference to plan.
        let mut changed = read.clone();
        changed.tables.get_mut(&t()).unwrap().indexes.insert(
            "ix_expr".to_owned(),
            Index {
                columns: vec![
                    key(IndexKey::Column("m".to_owned())),
                    key(IndexKey::Column("n".to_owned())),
                ],
                ..expression_index("unused")
            },
        );
        let base = d.overlay(&changed);
        assert_eq!(
            base.tables[&t()].indexes["ix_expr"].columns[0].key,
            IndexKey::Column("m".to_owned())
        );

        // A plan's index changes and a table rename carry the record.
        let mut advanced = Declared::default();
        advanced.advance(&changes(vec![Change::AddIndex {
            table: t(),
            name: "ix_expr".to_owned(),
            index: Box::new(expression_index("lower(x)")),
            clustered: false,
        }]));
        assert_eq!(
            advanced.expressions.keys[&t()]["ix_expr"][0].as_deref(),
            Some("lower(x)")
        );
        let u: TableName = "dbo.u".parse().unwrap();
        advanced.advance(&changes(vec![Change::RenameTable {
            uid: "t_a1b2c3".parse().unwrap(),
            from: t(),
            to: u.clone(),
            defaults: Vec::new(),
        }]));
        assert!(advanced.expressions.keys[&u].contains_key("ix_expr"));
        advanced.advance(&changes(vec![Change::DropIndex {
            table: u.clone(),
            name: "ix_expr".to_owned(),
        }]));
        assert!(
            advanced
                .expressions
                .keys
                .get(&u)
                .is_none_or(|m| m.is_empty())
        );
    }

    /// A table attached as a partition keeps the records of what it keeps of
    /// its own (#1545): a check, an expression index's keys and a default the
    /// shape still holds, each with its binding, and loses the records of
    /// what it no longer holds as its own, its adopted index's filter among
    /// them.
    #[test]
    fn an_attached_table_keeps_the_records_of_what_stays_its_own() {
        use crate::IndexKey;
        let mut ordinary = schema();
        let expression_index = Index {
            columns: vec![IndexColumn {
                key: IndexKey::Expression("n+m".to_owned()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        ordinary
            .tables
            .get_mut(&t())
            .unwrap()
            .indexes
            .insert("ix_expr".to_owned(), expression_index.clone());
        let mut d = Declared::from_schema(&ordinary);
        let bound = || Binding {
            candidates: [("f".to_owned(), ["dbo.f".to_owned()].into_iter().collect())]
                .into_iter()
                .collect(),
        };
        d.bindings
            .defaults
            .insert(t(), [("n".to_owned(), bound())].into_iter().collect());
        d.bindings
            .checks
            .insert(t(), [("ck_n".to_owned(), bound())].into_iter().collect());
        d.bindings
            .filters
            .insert(t(), [("ix_n".to_owned(), bound())].into_iter().collect());
        let shape = Table {
            checks: table().checks,
            indexes: [("ix_expr".to_owned(), expression_index)]
                .into_iter()
                .collect(),
            partition_of: Some(crate::PartitionOf {
                parent: "dbo.p".parse().unwrap(),
                bound: crate::PartitionBound::Default,
                columns: [(
                    "n".to_owned(),
                    crate::PartitionColumn {
                        default: Some("0".to_owned()),
                        not_null: false,
                    },
                )]
                .into_iter()
                .collect(),
            }),
            ..Table::default()
        };
        d.advance(&changes(vec![Change::AttachPartition {
            uid: "t_a1b2c3".parse().unwrap(),
            table: t(),
            parent: "dbo.p".parse().unwrap(),
            bound: crate::PartitionBound::Default,
            shape: Box::new(shape),
        }]));
        assert_eq!(
            d.expressions.keys[&t()]["ix_expr"],
            [Some("n+m".to_owned())]
        );
        assert_eq!(d.expressions.checks[&t()]["ck_n"], "n > 0");
        assert_eq!(d.expressions.defaults[&t()]["n"], "0");
        assert!(d.bindings.defaults[&t()].contains_key("n"));
        assert!(d.bindings.checks[&t()].contains_key("ck_n"));
        // Negative: the adopted index's filter and its binding are gone.
        assert!(d.expressions.filters.get(&t()).is_none_or(|m| m.is_empty()));
        assert!(!d.bindings.filters.contains_key(&t()));
    }

    /// A parent's column rename or drop reaches its partitions' records of
    /// their own defaults and bindings, which the engine renames or drops
    /// with it (#1692 review); the partitions are read from the state the
    /// plan leaves, since the plan names only the parent.
    #[test]
    fn a_parents_column_rename_and_drop_carry_its_partitions_records() {
        let parent: TableName = "app.ev".parse().unwrap();
        let partition: TableName = "app.ev_1".parse().unwrap();
        let other: TableName = "app.other".parse().unwrap();
        let mut state = Schema::default();
        state.tables.insert(
            partition.clone(),
            Table {
                partition_of: Some(crate::PartitionOf {
                    parent: parent.clone(),
                    bound: crate::PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Table::default()
            },
        );
        state.tables.insert(other.clone(), Table::default());
        let recorded = || {
            let mut d = Declared::default();
            for t in [&partition, &other] {
                d.expressions.defaults.insert(
                    t.clone(),
                    [
                        ("m".to_owned(), "1+2".to_owned()),
                        ("n".to_owned(), "3".to_owned()),
                    ]
                    .into_iter()
                    .collect(),
                );
                d.bindings.defaults.insert(
                    t.clone(),
                    [("m".to_owned(), Binding::default())].into_iter().collect(),
                );
            }
            d
        };
        let plan = changes(vec![
            Change::RenameColumn {
                uid: "c_a1b2c3".parse().unwrap(),
                table: parent.clone(),
                from: "m".into(),
                to: "m2".into(),
                table_was: None,
            },
            Change::DropColumn {
                uid: "c_d4e5f6".parse().unwrap(),
                column: crate::ColumnRef::new(parent.clone(), "n"),
            },
        ]);
        let mut d = recorded();
        d.advance_with_partitions(&plan, &state);
        let defaults = &d.expressions.defaults[&partition];
        assert_eq!(defaults.get("m2").map(String::as_str), Some("1+2"));
        assert!(!defaults.contains_key("m") && !defaults.contains_key("n"));
        assert!(d.bindings.defaults[&partition].contains_key("m2"));
        // Negative: a table that is no partition of the parent keeps its own.
        assert_eq!(d.expressions.defaults[&other].len(), 2);
        assert!(d.bindings.defaults[&other].contains_key("m"));
        // Negative: from the plan alone, the partitions' records stay.
        let mut alone = recorded();
        alone.advance(&plan);
        assert!(alone.expressions.defaults[&partition].contains_key("m"));
    }

    /// A generation expression is recorded as declared, overlaid on the
    /// engine's respelling only over a column read back as generated, and
    /// advanced by the change that rewrites it (DEC-1168.1).
    #[test]
    fn a_generation_expressions_declared_text_is_recorded_overlaid_and_advanced() {
        let with_generated = |expression: &str| {
            let mut s = schema();
            let mut b = Column::new("int".parse().unwrap());
            b.generated = Some(crate::schema::Generated {
                expression: expression.into(),
                stored: true,
            });
            s.tables
                .get_mut(&t())
                .unwrap()
                .columns
                .insert("b".into(), b);
            s
        };
        let d = Declared::from_schema(&with_generated("n*2"));
        assert_eq!(d.expressions.generated[&t()]["b"], "n*2");
        let base = d.overlay(&with_generated("(n * 2)"));
        assert_eq!(
            base.tables[&t()].columns["b"]
                .generated
                .as_ref()
                .unwrap()
                .expression,
            "n*2"
        );
        // Negative: nothing is conjured over a column read back ordinary.
        let plain = d.overlay(&schema());
        assert!(!plain.tables[&t()].columns.contains_key("b"));
        let mut advanced = d.clone();
        advanced.advance(&changes(vec![Change::AlterColumnExpression {
            uid: "c_a1b2c3".parse().unwrap(),
            column: t().column("b"),
            from: "n*2".into(),
            to: "n*3".into(),
        }]));
        assert_eq!(advanced.expressions.generated[&t()]["b"], "n*3");
    }

    /// Only what is compared as text is recorded: a default, a check, a
    /// filter and a module body. An unfiltered index and a column without a
    /// default leave nothing behind, so an empty record means "nothing was
    /// declared", not "something was declared empty".
    #[test]
    fn a_schema_records_exactly_the_texts_the_differ_compares() {
        let d = Declared::from_schema(&schema());
        assert_eq!(d.modules[&"dbo.v".parse::<ModuleId>().unwrap()], "SELECT 1");
        assert_eq!(d.expressions.defaults[&t()]["n"], "0");
        assert!(!d.expressions.defaults[&t()].contains_key("m"));
        assert_eq!(d.expressions.checks[&t()]["ck_n"], "n > 0");
        assert_eq!(d.expressions.filters[&t()]["ix_n"], "n > 0");
        assert!(!d.expressions.filters[&t()].contains_key("ix_m"));
        assert!(d.bindings.is_empty());
        assert!(Declared::from_schema(&Schema::default()).is_empty());
    }

    /// A plan carries the declared text it writes, and nothing else: what it
    /// leaves alone keeps the previous record, a drop forgets it, and a rename
    /// moves it under the new name — table and column alike.
    #[test]
    fn a_plan_advances_the_record_by_what_it_writes_drops_and_renames() {
        let mut d = Declared::from_schema(&schema());
        d.bindings.checks.entry(t()).or_default().insert(
            "ck_n".to_owned(),
            Binding {
                candidates: [("n".to_owned(), BTreeSet::from(["dbo.t.n".to_owned()]))]
                    .into_iter()
                    .collect(),
            },
        );
        let u: TableName = "dbo.u".parse().unwrap();
        d.advance(&changes(vec![
            Change::AlterColumnDefault {
                uid: "c_aaaaaa".parse().unwrap(),
                column: ColumnRef::new(t(), "n"),
                from: Some("0".to_owned()),
                to: Some("1".to_owned()),
            },
            Change::AddColumn {
                uid: "c_bbbbbb".parse().unwrap(),
                table: t(),
                name: "k".to_owned(),
                column: Box::new({
                    let mut c = Column::new("int".parse().unwrap());
                    c.default = Some("GETDATE()".to_owned());
                    c
                }),
            },
            Change::DropCheck {
                table: t(),
                name: "ck_n".to_owned(),
            },
            Change::AddCheck {
                table: t(),
                name: "ck_n".to_owned(),
                constraint: CheckConstraint {
                    expression: "n > 1".to_owned(),
                },
            },
            Change::RenameColumn {
                uid: "c_bbbbbb".parse().unwrap(),
                table: t(),
                from: "k".to_owned(),
                to: "kk".to_owned(),
                table_was: None,
            },
            Change::RenameTable {
                uid: "t_aaaaaa".parse().unwrap(),
                from: t(),
                to: u.clone(),
                defaults: Vec::new(),
            },
            Change::AlterModule {
                id: "dbo.v".parse().unwrap(),
                module: Box::new(view("SELECT 2")),
            },
            Change::CreateModule {
                id: "dbo.w".parse().unwrap(),
                module: Box::new(view("SELECT 3")),
            },
        ]));
        assert!(!d.expressions.defaults.contains_key(&t()), "{d:?}");
        assert_eq!(d.expressions.defaults[&u]["n"], "1");
        assert_eq!(d.expressions.defaults[&u]["kk"], "GETDATE()");
        assert_eq!(d.expressions.checks[&u]["ck_n"], "n > 1");
        assert_eq!(
            d.expressions.filters[&u]["ix_n"], "n > 0",
            "untouched, carried"
        );
        // The rewritten check's binding is the dialect's to record again.
        assert!(
            d.bindings
                .checks
                .get(&u)
                .is_none_or(|m| !m.contains_key("ck_n"))
        );
        assert_eq!(d.modules[&"dbo.v".parse::<ModuleId>().unwrap()], "SELECT 2");
        assert_eq!(d.modules[&"dbo.w".parse::<ModuleId>().unwrap()], "SELECT 3");

        d.advance(&changes(vec![
            Change::DropTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: u.clone(),
                detach_from: None,
            },
            Change::DropModule {
                id: "dbo.v".parse().unwrap(),
                kind: ModuleKind::View,
            },
        ]));
        assert!(d.expressions.is_empty(), "{d:?}");
        assert_eq!(d.modules.len(), 1);
        assert!(
            d.modules
                .contains_key(&"dbo.w".parse::<ModuleId>().unwrap())
        );
    }

    /// The overlay puts the declared text where the engine's spelling was,
    /// and only there: an object the read-back lacks is not conjured back, a
    /// text the record lacks stays as read, and the structure is untouched.
    #[test]
    fn the_overlay_replaces_recorded_texts_and_nothing_else() {
        let declared = Declared::from_schema(&schema());
        // What an engine reads back: respelled, and one object dropped by hand.
        let mut read_back = schema();
        let table = read_back.tables.get_mut(&t()).unwrap();
        table.columns.get_mut("n").unwrap().default = Some("((0))".to_owned());
        table.columns.get_mut("m").unwrap().default = Some("(1)".to_owned());
        table.checks.get_mut("ck_n").unwrap().expression = "([n]>(0))".to_owned();
        table.indexes.get_mut("ix_n").unwrap().filter = Some("([n]>(0))".to_owned());
        table.indexes.remove("ix_m");
        read_back
            .modules
            .get_mut(&"dbo.v".parse().unwrap())
            .unwrap()
            .definition = "SELECT (1)".to_owned();
        read_back
            .modules
            .insert("dbo.hand".parse().unwrap(), view("SELECT 9"));

        let base = declared.overlay(&read_back);
        let table = &base.tables[&t()];
        assert_eq!(table.columns["n"].default.as_deref(), Some("0"));
        // Not recorded: the read-back stands, and a default the record has
        // but the engine does not is not put back.
        assert_eq!(table.columns["m"].default.as_deref(), Some("(1)"));
        assert_eq!(table.checks["ck_n"].expression, "n > 0");
        assert_eq!(table.indexes["ix_n"].filter.as_deref(), Some("n > 0"));
        assert!(!table.indexes.contains_key("ix_m"));
        assert_eq!(
            base.modules[&"dbo.v".parse::<ModuleId>().unwrap()].definition,
            "SELECT 1"
        );
        assert_eq!(
            base.modules[&"dbo.hand".parse::<ModuleId>().unwrap()].definition,
            "SELECT 9"
        );
        // With nothing recorded, the overlay is the identity.
        assert_eq!(Declared::default().overlay(&read_back), read_back);
    }
}
