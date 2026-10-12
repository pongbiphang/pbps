use pbps_model::{PartitionColumn, Schema, Table, TableName};
use std::borrow::Cow;

use super::{
    Req,
    expect::{Expected, Implied, Own, OwnExpected, place},
};

pub(crate) fn implied(expected: &Implied<'_>, now: &Schema, moved: &mut Vec<String>) {
    for (name, presence) in &expected.0 {
        let present = now.tables.contains_key(*name);
        if !presence.accepts(&present) {
            moved.push(if present {
                format!("{name} is there, and no change of this plan creates it")
            } else {
                format!("{name} is gone, and no change of this plan drops it")
            });
        }
    }
}

pub(crate) fn own(
    expected: &OwnExpected<'_>,
    now: &Table,
    name: &TableName,
    moved: &mut Vec<String>,
) {
    let empty = Default::default();
    let columns = now
        .partition_of
        .as_ref()
        .map(|of| &of.columns)
        .unwrap_or(&empty);
    let absent = Own::default();
    let changed = expected.0.keys().chain(columns.keys()).any(|name| {
        let required = expected.0.get(name).unwrap_or(&absent);
        let (default, not_null) = columns
            .get(name)
            .map_or((None, false), |PartitionColumn { default, not_null }| {
                (default.as_deref(), *not_null)
            });
        !required.default.accepts(default) || !required.not_null.accepts(&not_null)
    });
    if changed {
        moved.push(format!(
            "{name}'s own column defaults or NOT NULLs are not the ones this plan leaves"
        ));
    }
}

pub(crate) fn created_place(
    declared: &Table,
    now: &Table,
    name: &TableName,
    moved: &mut Vec<String>,
) {
    // Connected planning and bootstrap check bound spellings before DDL;
    // an offline preview cannot be applied. Hold the pinned declaration
    // even at the first staged read, before it could become a baseline.
    if !Req::Planned(place(declared)).accepts(&place(now)) {
        moved.push(format!(
            "{name}'s partition parent, bound or key is not the one this plan creates"
        ));
    }
}

pub(crate) fn table(
    expected: &Expected<'_>,
    now: &Table,
    placed: &Table,
    name: &TableName,
    moved: &mut Vec<String>,
) {
    if !expected.identity.accepts(&now.replica_identity.as_ref()) {
        moved.push(match expected.identity {
            Req::Keep(_) => {
                format!("{name} replica identity changed, and no change of this plan sets it")
            }
            Req::Planned(_) | Req::PlannedOrKeep(_, _) | Req::Unpromised => {
                format!("{name} replica identity is not the one this plan sets")
            }
        });
    }
    if !expected
        .storage
        .accepts(&Cow::Borrowed(&now.storage_parameters))
    {
        moved.push(format!(
            "{name} storage parameters are not the ones this plan leaves"
        ));
    }
    if !expected.persistence.accepts(&now.unlogged) {
        moved.push(format!(
            "{name} is {} and this plan does not leave it so",
            if now.unlogged { "unlogged" } else { "logged" }
        ));
    }
    if !expected.place.accepts(&place(placed)) {
        moved.push(format!(
            "{name}'s partition parent, bound or key is not the one the plan was approved over"
        ));
    }
}

// Keep the existing diagnostic order around partition-own checks until their
// expectation slice moves them here too.
pub(crate) fn system_time(
    expected: &Expected<'_>,
    now: &Table,
    name: &TableName,
    moved: &mut Vec<String>,
) {
    if !expected.system_time.accepts(&now.system_time.as_ref()) {
        moved.push(format!(
            "{name} period or system versioning changed, and this plan does not change it"
        ));
    }
}
