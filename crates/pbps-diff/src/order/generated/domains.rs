//! Only consumers that copy a default or evaluate domain values need its facets.
//! A routine naming a bare domain can precede its checks (DEC-2469.5).
use super::*;
use pbps_model::{ColumnType, DomainDelta, DomainHalf, ManagedType, TypeRef};

#[derive(Clone, Copy)]
enum Need {
    Type,
    Column,
    Default,
    Values,
}

#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn constraints(
    cs: &ChangeSet,
    declared: &Schema,
    empty: &BTreeSet<usize>,
    out: &mut Schedule,
) {
    let mut creates = BTreeMap::new();
    let mut facets: BTreeMap<&TypeRef, Vec<(usize, bool)>> = BTreeMap::new();
    for (at, planned) in cs.changes.iter().enumerate() {
        match &planned.change {
            Change::CreateType { name, .. } => {
                creates.insert(name, at);
            }
            Change::AlterDomain {
                name,
                from,
                to,
                half,
                ..
            } if is_part(planned) => {
                if let Some(delta) = DomainDelta::of(from, to) {
                    facets
                        .entry(name)
                        .or_default()
                        .push((at, *half == DomainHalf::Loosen && delta.default.is_some()));
                }
            }
            _ => {}
        }
    }
    if facets.is_empty() {
        return;
    }
    for (name, parts) in &facets {
        if let Some(first) = creates.get(name) {
            for (part, _) in parts {
                out.edge(*first, *part, Source::Effect, OrderReason::Structural);
            }
        }
    }
    // Default inheritance is a creation-time copy for a domain over another
    // domain. Tables instead use the current domain default when rows arrive.
    let mut require = |ty: &ColumnType, consumer: usize, need: Need| {
        let mut next = ty.type_ref();
        let mut array = ty.is_array();
        let mut copied_default = true;
        let mut seen = BTreeSet::new();
        while let Some(name) = next.filter(|name| seen.insert(*name)) {
            if let Some(&created) = creates.get(name)
                && created != consumer
            {
                out.edge(created, consumer, Source::Effect, OrderReason::Structural);
            }
            if let Some(parts) = facets.get(name) {
                for (part, default) in parts {
                    let selected = match need {
                        Need::Type => false,
                        Need::Column => array && !*default,
                        Need::Default => copied_default && !array && *default,
                        Need::Values => true,
                    };
                    if *part != consumer && selected {
                        out.edge(*part, consumer, Source::Effect, OrderReason::Structural);
                    }
                }
            }
            next = match declared.types.get(name) {
                Some(ManagedType::Domain { base, default, .. }) => {
                    // A nearer explicit default shadows every deeper default,
                    // but the base type identities still have to exist.
                    copied_default &= default.is_none();
                    array |= base.is_array();
                    base.type_ref()
                }
                _ => None,
            };
        }
    };
    for (at, planned) in cs.changes.iter().enumerate() {
        match &planned.change {
            Change::CreateType { definition, .. } => {
                if let ManagedType::Domain { base, default, .. } = definition.as_ref() {
                    require(
                        base,
                        at,
                        if default.is_none() {
                            Need::Default
                        } else {
                            Need::Type
                        },
                    );
                }
            }
            Change::CreateTable {
                name,
                table,
                beside_default,
                ..
            } => {
                let shape = if table.partition_of.is_some() {
                    root_table(declared, name).unwrap_or(table)
                } else {
                    table
                };
                for column in shape.columns.values() {
                    let values =
                        beside_default.is_some() || column.value_source != ValueSource::None;
                    require(
                        &column.ty,
                        at,
                        if values { Need::Values } else { Need::Column },
                    );
                }
            }
            Change::AddColumn { column, .. } => {
                let values = !empty.contains(&at) || column.value_source != ValueSource::None;
                require(
                    &column.ty,
                    at,
                    if values { Need::Values } else { Need::Column },
                );
            }
            Change::AlterColumnType { to, .. } => require(to, at, Need::Values),
            Change::InsertRow { table, .. }
            | Change::UpdateRow { table, .. }
            | Change::DeleteRow { table, .. } => {
                if let Some(table) = root_table(declared, table) {
                    for column in table.columns.values() {
                        require(&column.ty, at, Need::Values);
                    }
                }
            }
            Change::CopyTableRows(copy) => {
                for column in &copy.columns {
                    require(&column.cast, at, Need::Values);
                }
            }
            _ => {}
        }
    }
}

fn root_table<'a>(declared: &'a Schema, name: &TableName) -> Option<&'a pbps_model::Table> {
    let chain = pbps_model::partitions::ancestor_chain(name, &declared.tables)?;
    chain.last().map(|(_, table)| *table)
}

/// The existing half's expressions, never another facet's snapshot text.
pub(super) fn texts(change: &Change) -> Vec<&str> {
    let Change::AlterDomain { from, to, half, .. } = change else {
        return Vec::new();
    };
    let Some(delta) = DomainDelta::of(from, to) else {
        return Vec::new();
    };
    match half {
        DomainHalf::Loosen => delta.default.flatten().into_iter().collect(),
        DomainHalf::Tighten => delta
            .add_checks
            .into_iter()
            .map(|(_, expression)| expression)
            .collect(),
    }
}
