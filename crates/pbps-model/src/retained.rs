//! Approved expression spelling changes whose bound objects are retained.
use crate::{Change, ChangeSet, ObjectLocalName, TableName, Uid};
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
    Check {
        uid: Uid,
        table: TableName,
        name: ObjectLocalName,
    },
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
                let ExpressionTarget::Check {
                    uid: table_uid,
                    table,
                    name,
                } = &e.target;
                if plan.dialect != "postgres" || plan.ids.tables.get(table_uid) != Some(table)
                    || !targets.insert(&e.target) || e.before.trim().is_empty() || e.after.trim().is_empty()
                    || e.before == e.after || e.anchors.is_empty()
                    || e.anchors.iter().collect::<std::collections::BTreeSet<_>>().len() != e.anchors.len()
                    || !e.anchors.iter().all(|a| steps.contains(a))
                    || !e.anchors.contains(&RenameAnchor { uid: uid.clone(), ordinal: *ordinal })
                    || plan.changes.changes.iter().any(|p| matches!(&p.change,
                        Change::DropCheck { table: t, name: n } | Change::AddCheck { table: t, name: n, .. }
                        if t == table && n == name))
                {
                    return Err(format!("invalid retained CHECK {}.{}", table.shown(), name.shown()));
                }
            }
        }
    }
    Ok(())
}
