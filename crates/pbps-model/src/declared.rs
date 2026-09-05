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
    /// `CheckConstraint::expression`, by table and constraint name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<TableName, BTreeMap<String, String>>,
    /// `Index::filter`, by table and index name; only filtered indexes appear.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub filters: BTreeMap<TableName, BTreeMap<String, String>>,
}

impl DeclaredExpressions {
    pub fn is_empty(&self) -> bool {
        self.defaults.is_empty() && self.checks.is_empty() && self.filters.is_empty()
    }
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
        for planned in &changes.changes {
            match &planned.change {
                Change::CreateTable { name, table, .. } => {
                    let fresh = Self::from_schema(&Schema {
                        tables: [(name.clone(), (**table).clone())].into_iter().collect(),
                        ..Schema::default()
                    });
                    self.forget_table(name);
                    self.expressions.defaults.extend(fresh.expressions.defaults);
                    self.expressions.checks.extend(fresh.expressions.checks);
                    self.expressions.filters.extend(fresh.expressions.filters);
                }
                Change::DropTable { name, .. } => self.forget_table(name),
                Change::RenameTable { from, to, .. } => {
                    rekey_table(&mut self.expressions.defaults, from, to);
                    rekey_table(&mut self.expressions.checks, from, to);
                    rekey_table(&mut self.expressions.filters, from, to);
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
                    remove_nested(&mut self.bindings.defaults, table, name);
                    if let Some(default) = &column.default {
                        self.expressions
                            .defaults
                            .entry(table.clone())
                            .or_default()
                            .insert(name.clone(), default.clone());
                    }
                }
                Change::DropColumn { column, .. } => {
                    remove_nested(&mut self.expressions.defaults, &column.table, &column.name);
                    remove_nested(&mut self.bindings.defaults, &column.table, &column.name);
                }
                Change::RenameColumn {
                    table, from, to, ..
                } => {
                    rekey_nested(&mut self.expressions.defaults, table, from, to);
                    rekey_nested(&mut self.bindings.defaults, table, from, to);
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
                Change::AddIndex { table, name, index } => {
                    remove_nested(&mut self.expressions.filters, table, name);
                    remove_nested(&mut self.bindings.filters, table, name);
                    if let Some(filter) = &index.filter {
                        self.expressions
                            .filters
                            .entry(table.clone())
                            .or_default()
                            .insert(name.clone(), filter.clone());
                    }
                }
                Change::DropIndex { table, name } => {
                    remove_nested(&mut self.expressions.filters, table, name);
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
                | Change::Revoke { .. } => {}
            }
        }
    }

    fn forget_table(&mut self, name: &TableName) {
        self.expressions.defaults.remove(name);
        self.expressions.checks.remove(name);
        self.expressions.filters.remove(name);
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
                    name: "n".to_owned(),
                    descending: false,
                }],
                include: Vec::new(),
                unique: false,
                filter: Some("n > 0".to_owned()),
            },
        );
        table.indexes.insert(
            "ix_m".to_owned(),
            Index {
                columns: vec![IndexColumn {
                    name: "m".to_owned(),
                    descending: false,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
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
            },
            Change::RenameTable {
                uid: "t_aaaaaa".parse().unwrap(),
                from: t(),
                to: u.clone(),
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
