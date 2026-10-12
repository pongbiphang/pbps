use pbps_model::{Table, TableName};
use std::borrow::Cow;

use super::{
    Req,
    expect::{Expected, place},
};

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
