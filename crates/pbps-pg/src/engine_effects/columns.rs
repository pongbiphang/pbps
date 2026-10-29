//! Measured PostgreSQL policy over portable statement transitions.
use crate::Postgres;
use pbps_model::change::{ColumnProperty, ColumnTransition, ColumnValue};
use pbps_model::operation::symbolic::{Attribute, Clause, Fact, Scope, WritePolicy};
use pbps_model::{Change, ValueSource};

pub(super) fn unsupported(reason: impl Into<String>, sqlstate: Option<&str>) -> Clause {
    Clause::Require(Fact::Unsupported {
        reason: format!(
            "{}; change the columns and the partitions in separate plans",
            reason.into()
        ),
        sqlstate: sqlstate.map(str::to_owned),
    })
}

pub(super) fn nullability(version: Option<i64>, nullable: bool) -> Option<WritePolicy> {
    match version {
        Some(v) if v >= 180000 => Some(if nullable {
            WritePolicy::PreserveOwn
        } else {
            WritePolicy::StrengthenNotNull
        }),
        Some(v) if v >= 160000 => Some(WritePolicy::Overwrite),
        None | Some(_) => None,
    }
}

pub(super) fn policy(
    pg: &Postgres,
    transition: &ColumnTransition<'_>,
) -> Option<(Scope, WritePolicy)> {
    use Attribute::*;
    use ColumnProperty::*;
    use WritePolicy::*;
    Some(match (&transition.property, transition.value) {
        (Column(NotNull), ColumnValue::Nullable(nullable)) => (
            Scope::MembersAtOperation,
            nullability(pg.server_version_num, nullable)?,
        ),
        (Column(Numbering), ColumnValue::Definition(_) | ColumnValue::Absent) => {
            (Scope::ParentOnly, Overwrite)
        }
        (Address, _) | (Column(Numbering), _) => (Scope::ParentOnly, PreserveOwnership),
        (Column(Name | Type | Collation), _)
            if transition.lifetime == pbps_model::operation::symbolic::Lifetime::Retain =>
        {
            (Scope::MembersAtOperation, PreserveOwnership)
        }
        (Column(_), _) => (Scope::MembersAtOperation, Overwrite),
    })
}

#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn refusal(pg: &Postgres, change: &Change) -> Option<Clause> {
    match change {
        Change::AddColumn { column, .. }
            if matches!(column.value_source, ValueSource::Identity(_)) =>
        {
            Some(unsupported(
                "a new identity column on a moving partitioned parent is not representable by the reader",
                pg.server_version_num
                    .filter(|v| *v < 180000)
                    .map(|_| "42P16"),
            ))
        }
        Change::AlterColumnNumbering { from, to, .. } => {
            if matches!(from, ValueSource::Identity(_)) || matches!(to, ValueSource::Identity(_)) {
                Some(unsupported(
                    "identity numbering on a moving parent retains its capability/readback refusal (measured restrictions: 42P16, 55000, 42601)",
                    None,
                ))
            } else if !matches!(from, ValueSource::Sequence(_))
                && !matches!(to, ValueSource::Sequence(_))
            {
                Some(unsupported(
                    "the numbering change has no owned sequence input or output",
                    None,
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
