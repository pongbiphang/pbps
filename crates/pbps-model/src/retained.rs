//! Approved expression spelling changes whose bound objects are retained.
use crate::{Change, ChangeSet, ColumnRef, ObjectLocalName, Schema, TableName, Uid, ValueSource};
use serde::{Deserialize, Serialize};

/// A logical lifecycle step, independent of its scheduled position.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameAnchor {
    pub uid: Uid,
    pub ordinal: usize,
}

/// The target is tagged so another expression surface needs no payload redesign.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExpressionTarget {
    ColumnDefault {
        uid: Uid,
        column: ColumnRef,
    },
    Check {
        uid: Uid,
        table: TableName,
        name: ObjectLocalName,
    },
}

impl ExpressionTarget {
    pub fn table(&self) -> &TableName {
        match self {
            Self::Check { table, .. } => table,
            Self::ColumnDefault { column, .. } => &column.table,
        }
    }
    pub fn shown(&self) -> String {
        match self {
            Self::Check { table, name, .. } => format!("CHECK {}.{}", table.shown(), name.shown()),
            Self::ColumnDefault { column, .. } => {
                format!("DEFAULT {}.{}", column.table.shown(), column.name.shown())
            }
        }
    }
    pub fn expression<'a>(&self, schema: &'a Schema) -> Option<&'a String> {
        let table = schema.tables.get(self.table())?;
        match self {
            Self::Check { name, .. } => Some(&table.checks.get(name)?.expression),
            Self::ColumnDefault { column, .. } => {
                match &table.columns.get(&column.name)?.value_source {
                    ValueSource::Default(expr) => Some(expr),
                    ValueSource::None
                    | ValueSource::Identity(_)
                    | ValueSource::Sequence(_)
                    | ValueSource::Generated(_) => None,
                }
            }
        }
    }
    pub fn expression_mut<'a>(&self, schema: &'a mut Schema) -> Option<&'a mut String> {
        let table = schema.tables.get_mut(self.table())?;
        match self {
            Self::Check { name, .. } => Some(&mut table.checks.get_mut(name)?.expression),
            Self::ColumnDefault { column, .. } => {
                match &mut table.columns.get_mut(&column.name)?.value_source {
                    ValueSource::Default(expr) => Some(expr),
                    ValueSource::None
                    | ValueSource::Identity(_)
                    | ValueSource::Sequence(_)
                    | ValueSource::Generated(_) => None,
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedExpression {
    pub target: ExpressionTarget,
    pub before: String,
    pub after: String,
    pub anchors: Vec<RenameAnchor>,
}

pub fn expressions(changes: &ChangeSet) -> impl Iterator<Item = &RetainedExpression> {
    changes.changes.iter().flat_map(|p| {
        if let Change::RenameEnumValue { retained, .. } = &p.change {
            retained.as_slice()
        } else {
            &[]
        }
    })
}

pub fn validate(plan: &crate::SavedPlan) -> Result<(), String> {
    let mut targets = std::collections::BTreeSet::new();
    let steps: std::collections::BTreeSet<_> = plan
        .changes
        .changes
        .iter()
        .filter_map(|p| {
            if let Change::RenameEnumValue { uid, ordinal, .. } = &p.change {
                Some(RenameAnchor {
                    uid: uid.clone(),
                    ordinal: *ordinal,
                })
            } else {
                None
            }
        })
        .collect();
    for p in &plan.changes.changes {
        if let Change::RenameEnumValue {
            uid,
            ordinal,
            retained,
            ..
        } = &p.change
        {
            for e in retained {
                let identity_matches = match &e.target {
                    ExpressionTarget::Check { uid, table, .. } => {
                        plan.ids.tables.get(uid) == Some(table)
                    }
                    ExpressionTarget::ColumnDefault { uid, column } => {
                        plan.ids.columns.get(uid) == Some(column)
                    }
                };
                let altered = plan
                    .changes
                    .changes
                    .iter()
                    .any(|p| match (&e.target, &p.change) {
                        (
                            ExpressionTarget::Check { table, name, .. },
                            Change::DropCheck { table: t, name: n }
                            | Change::AddCheck {
                                table: t, name: n, ..
                            },
                        ) => t == table && n == name,
                        (
                            ExpressionTarget::ColumnDefault { column, .. },
                            Change::AlterColumnDefault { column: c, .. },
                        ) => c == column,
                        _ => false,
                    });
                if plan.dialect != "postgres"
                    || !identity_matches
                    || !targets.insert(&e.target)
                    || e.before.trim().is_empty()
                    || e.after.trim().is_empty()
                    || e.before == e.after
                    || e.anchors.is_empty()
                    || e.anchors
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        != e.anchors.len()
                    || !e.anchors.iter().all(|a| steps.contains(a))
                    || !e.anchors.contains(&RenameAnchor {
                        uid: uid.clone(),
                        ordinal: *ordinal,
                    })
                    || altered
                {
                    return Err(format!("invalid retained {}", e.target.shown()));
                }
            }
        }
    }
    Ok(())
}
