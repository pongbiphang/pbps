//! Engine-required CREATE surfaces and their existing local prerequisites.
use super::{Demand, Request};
use crate::{Dialect, DialectError};
use pbps_model::object_ref::{ObjectRef, Root, Selector};
use pbps_model::operation::PartKey;
use pbps_model::{Change, ColumnName, Table, TableName, ValueSource};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, Default)]
pub struct Inline {
    pub unavailable: Option<&'static str>,
    pub required: BTreeMap<ObjectRef, &'static str>,
    /// NOT NULL belongs to these columns' physical installation, even when ADD is legal.
    pub column_not_null: BTreeMap<ColumnName, &'static str>,
}
impl Inline {
    pub fn column(&mut self, table: &TableName, column: &ColumnName, why: &'static str) {
        self.require(Root::Column(table.column(column)), Selector::Whole, why);
    }
    pub fn not_null(&mut self, table: &TableName, column: &ColumnName, why: &'static str) {
        self.require(Root::Column(table.column(column)), Selector::NotNull, why);
    }
    pub fn part(&mut self, table: &TableName, part: PartKey, why: &'static str) {
        self.require(Root::Table(table.clone()), Selector::Part(part), why);
    }
    fn require(&mut self, root: Root, part: Selector, why: &'static str) {
        self.required.entry(ObjectRef { root, part }).or_insert(why);
    }
    pub fn contains(&self, root: Root, part: Selector) -> bool {
        self.required.contains_key(&ObjectRef { root, part })
    }
    pub fn keeps_column(&self, table: &TableName, column: &ColumnName) -> bool {
        self.contains(Root::Column(table.column(column)), Selector::Whole)
    }
    pub fn keeps_generation(&self, table: &TableName, column: &ColumnName) -> bool {
        self.contains(Root::Column(table.column(column)), Selector::Generation)
    }
    pub fn keeps_default(&self, table: &TableName, column: &ColumnName) -> bool {
        self.contains(Root::Column(table.column(column)), Selector::Default)
    }
    pub fn keeps_part(&self, table: &TableName, part: &PartKey) -> bool {
        self.contains(Root::Table(table.clone()), Selector::Part(part.clone()))
    }
    pub fn explanation(&self) -> Option<String> {
        let mut reasons: BTreeSet<_> = self.required.values().copied().collect();
        reasons.extend(self.unavailable);
        for reference in self.required.keys() {
            if let Root::Column(column) = &reference.root
                && reference.part == Selector::Whole
                && let Some(reason) = self.column_not_null.get(&column.name)
            {
                reasons.insert(*reason);
            }
        }
        (!reasons.is_empty()).then(|| {
            format!(
                "engine requires inline at CREATE: {}",
                reasons.into_iter().collect::<Vec<_>>().join("; ")
            )
        })
    }

    /// Close the required group over the same typed local edges and may-read
    /// inputs used for extracted parts. External providers stay separate;
    /// settlement decides whether they can precede this indivisible CREATE.
    #[allow(clippy::wildcard_enum_match_arm)]
    pub(super) fn close(
        &mut self,
        name: &TableName,
        full: &Table,
        request: Request<'_>,
        dialect: &(impl Dialect + ?Sized),
    ) {
        let mut queue: VecDeque<_> = self.required.iter().map(|(r, w)| (r.clone(), *w)).collect();
        let mut seen = BTreeSet::new();
        while let Some((reference, why)) = queue.pop_front() {
            if !seen.insert(reference.clone()) {
                continue;
            }
            let mut add = |r: ObjectRef| {
                if !self.required.contains_key(&r) {
                    self.required.insert(r.clone(), why);
                    queue.push_back((r, why));
                }
            };
            if let Some(graph) = request.dependencies {
                for provider in graph.get(&reference).into_iter().flatten() {
                    if matches!(&provider.root,Root::Table(t) if t==name)
                        || matches!(&provider.root,Root::Column(c) if &c.table==name)
                    {
                        add(provider.clone());
                    }
                }
            }
            let mut inputs = BTreeSet::new();
            match (&reference.root, &reference.part) {
                (Root::Column(column), selector) => {
                    if *selector == Selector::Whole
                        && self.column_not_null.contains_key(&column.name)
                    {
                        // The field was measured as a column group by the engine.
                        // When that column belongs to CREATE, so must its NOT NULL.
                        add(ObjectRef {
                            root: reference.root.clone(),
                            part: Selector::NotNull,
                        });
                    }
                    if *selector != Selector::Whole {
                        inputs.insert(column.name.clone());
                    }
                    let text = match selector {
                        Selector::Generation => full
                            .computed
                            .get(&column.name)
                            .map(|c| c.expression.as_str())
                            .or_else(|| {
                                full.columns
                                    .get(&column.name)
                                    .and_then(|c| match &c.value_source {
                                        ValueSource::Generated(g) => Some(g.expression.as_str()),
                                        _ => None,
                                    })
                            }),
                        Selector::Default => full.columns.get(&column.name).and_then(|c| match &c
                            .value_source
                        {
                            ValueSource::Default(t) => Some(t.as_str()),
                            _ => None,
                        }),
                        Selector::Whole if full.computed.contains_key(&column.name) => {
                            add(ObjectRef {
                                root: reference.root.clone(),
                                part: Selector::Generation,
                            });
                            None
                        }
                        _ => None,
                    };
                    if let Some(text) = text {
                        inputs.extend(
                            super::inputs::reads([text], name, full, dialect).unwrap_or_else(
                                || {
                                    full.columns
                                        .keys()
                                        .chain(full.computed.keys())
                                        .cloned()
                                        .collect()
                                },
                            ),
                        );
                    }
                }
                (Root::Table(_), Selector::Part(part)) => {
                    let change = match part {
                        PartKey::PrimaryKey => {
                            full.primary_key.as_ref().map(|key| Change::SetPrimaryKey {
                                table: name.clone(),
                                from: None,
                                to: Some(key.clone()),
                                nonclustered: !full.primary_key_is_clustered(),
                            })
                        }
                        PartKey::Unique(key) => full.unique.get(key).map(|c| Change::AddUnique {
                            table: name.clone(),
                            name: key.clone(),
                            constraint: c.clone(),
                            clustered: full.unique_is_clustered(key.raw()),
                        }),
                        PartKey::ForeignKey(key) => {
                            full.foreign_keys.get(key).map(|c| Change::AddForeignKey {
                                table: name.clone(),
                                name: key.clone(),
                                constraint: Box::new(c.clone()),
                            })
                        }
                        PartKey::Check(key) => full.checks.get(key).map(|c| Change::AddCheck {
                            table: name.clone(),
                            name: key.clone(),
                            constraint: c.clone(),
                        }),
                        PartKey::Index(key) => full.indexes.get(key).map(|c| Change::AddIndex {
                            table: name.clone(),
                            name: key.clone(),
                            index: Box::new(c.clone()),
                            clustered: full.index_is_clustered(key.raw()),
                        }),
                        PartKey::Computed(column) => {
                            add(ObjectRef {
                                root: Root::Column(name.column(column)),
                                part: Selector::Whole,
                            });
                            None
                        }
                        PartKey::Policy(_) | PartKey::RowSecurity => None,
                    };
                    if let Some(change) = change {
                        inputs.extend(
                            super::inputs::columns(&change, name, full, dialect).unwrap_or_else(
                                || {
                                    full.columns
                                        .keys()
                                        .chain(full.computed.keys())
                                        .cloned()
                                        .collect()
                                },
                            ),
                        );
                    }
                }
                _ => {}
            }
            for column in inputs {
                add(ObjectRef {
                    root: Root::Column(name.column(&column)),
                    part: Selector::Whole,
                });
                // A validator or index in CREATE needs the real value expression,
                // rather than a later placeholder's existence-only boundary.
                if full.computed.contains_key(&column)
                    || full
                        .columns
                        .get(&column)
                        .is_some_and(|c| matches!(c.value_source, ValueSource::Generated(_)))
                {
                    add(ObjectRef {
                        root: Root::Column(name.column(&column)),
                        part: Selector::Generation,
                    });
                }
            }
        }
    }

    pub(super) fn suffix(
        &self,
        name: &TableName,
        full: &Table,
        demand: &Demand,
    ) -> BTreeSet<ColumnName> {
        if full.computed.keys().any(|c| self.keeps_column(name, c)) {
            return BTreeSet::new();
        }
        let wanted = full.columns.keys().position(|c| demand.columns.contains(c));
        let kept = full
            .columns
            .keys()
            .enumerate()
            .filter(|(_, c)| self.keeps_column(name, c))
            .map(|(i, _)| i + 1)
            .max()
            .unwrap_or(0);
        wanted.map_or_else(BTreeSet::new, |cut| {
            full.columns.keys().skip(cut.max(kept)).cloned().collect()
        })
    }
    pub fn holds_columns<'a>(
        &self,
        name: &TableName,
        columns: impl Iterator<Item = &'a ColumnName>,
    ) -> bool {
        columns.into_iter().any(|c| self.keeps_column(name, c))
    }

    /// Presence uses the declaration registry, as annotation binding does.
    /// Neither this check nor the recipe invents a second facet vocabulary.
    pub fn check(&self, parts: &[Change], dialect: &'static str) -> Result<(), DialectError> {
        if let Some(reason) = self.unavailable {
            return Err(DialectError::Unsupported {
                dialect,
                feature: reason.into(),
            });
        }
        if self.required.is_empty() {
            return Ok(());
        }
        let schema = pbps_model::Schema {
            tables: parts
                .iter()
                .filter_map(|change| {
                    if let Change::CreateTable { name, table, .. } = change {
                        Some((name.clone(), table.as_ref().clone()))
                    } else {
                        None
                    }
                })
                .collect(),
            ..Default::default()
        };
        let registry = pbps_model::object_ref::Registry::new(&schema);
        for (reference, reason) in &self.required {
            if registry.declaration(reference).is_err() {
                return Err(DialectError::Unsupported {
                    dialect,
                    feature: format!(
                        "{reason} inline at CREATE; cannot defer `{}`",
                        reference.shown()
                    ),
                });
            }
        }
        Ok(())
    }
}
