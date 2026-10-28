//! Logical placement facts only. Templates are lowered after settlement.
use super::*;
use pbps_dialect::Dialect;
use pbps_diff::order::scratch::{Auxiliary, Node, Prepared};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Kind {
    Rendered(ObjectIdentity),
    Deferred,
}

#[derive(Debug, Clone, Copy)]
pub enum AuxiliaryStep<'a> {
    Rendered(&'a ObjectIdentity),
    Deferred,
}

pub(super) fn blueprint(
    prepared: &Prepared,
    steps: &[Draft],
    inputs: Option<&super::super::capture::ExternalInputs>,
    deferred: bool,
) -> Result<Vec<Auxiliary<Metadata>>, ReconstructError> {
    let mut auxiliary = Vec::new();
    if deferred {
        auxiliary.push(Auxiliary {
            name: "deferred externals section".into(),
            after: (0..prepared.changes().changes.len())
                .map(Node::Operation)
                .collect(),
            before: vec![],
            metadata: Metadata {
                declaration: "deferred externals section".into(),
                created: vec![],
                auxiliary: Some(Kind::Deferred),
            },
        });
    }
    let Some(inputs) = inputs else {
        return Ok(auxiliary);
    };
    let chains = inputs.prerequisites();
    let chain_ids = chains
        .keys()
        .enumerate()
        .map(|(i, id)| (id.clone(), i + usize::from(deferred)))
        .collect::<BTreeMap<_, _>>();
    let dependencies = inputs.external_prerequisites();
    for (id, required) in chains {
        let mut after = Vec::new();
        for prerequisite in required {
            let providers: Vec<_> = steps
                .iter()
                .filter(|step| {
                    step.metadata.created.contains(&prerequisite)
                        || supplies_key(&step.change.change, &prerequisite)
                        || (matches!(
                            step.change.change,
                            Change::SetPrimaryKey { .. }
                                | Change::AddUnique { .. }
                                | Change::AddIndex { .. }
                        ) && step
                            .change
                            .change
                            .table()
                            .is_some_and(|t| object(&t.schema, &t.name) == prerequisite))
                })
                .map(|step| Node::Operation(step.operation))
                .collect();
            if providers.is_empty() {
                return Err(ReconstructError::Unsupported(format!(
                    "external {id:?} depends on managed {prerequisite:?}, which reconstruction never creates"
                )));
            }
            after.extend(providers);
        }
        after.extend(
            dependencies
                .get(&id)
                .into_iter()
                .flatten()
                .filter_map(|id| chain_ids.get(id).copied())
                .map(Node::Auxiliary),
        );
        // The emitter probe is discarded. Only the finite lexical answer
        // enters the blueprint, just as for the generated parts' readers.
        let before: Vec<_> = steps
            .iter()
            .filter(|step| {
                let [schema, name] = id.name.as_slice() else {
                    return false;
                };
                crate::emit::emit(&step.change.change, step.change.strategy).is_ok_and(|sql| {
                    sql.iter()
                        .any(|s| Mssql.may_name_qualified(&s.sql, schema, name))
                })
            })
            .map(|step| Node::Operation(step.operation))
            .collect();
        auxiliary.push(Auxiliary {
            name: format!("external {id:?}"),
            after,
            before,
            metadata: Metadata {
                declaration: format!("external {id:?}"),
                created: vec![],
                auxiliary: Some(Kind::Rendered(id)),
            },
        });
    }
    if deferred {
        defer_unconsumed(&mut auxiliary);
    }
    Ok(auxiliary)
}

fn defer_unconsumed(auxiliary: &mut [Auxiliary<Metadata>]) {
    // A renderer with only an external consumer still has to precede the
    // suffix when that consumer eventually supplies a managed step. Follow
    // the already-derived prerequisite facts before adding suffix edges.
    let mut pending: Vec<_> = auxiliary
        .iter()
        .enumerate()
        .filter(|(_, node)| !node.before.is_empty())
        .map(|(index, _)| index)
        .collect();
    let mut consumed = BTreeSet::new();
    while let Some(index) = pending.pop() {
        if consumed.insert(index) {
            pending.extend(auxiliary[index].after.iter().filter_map(|node| match node {
                Node::Auxiliary(index) => Some(*index),
                Node::Operation(_) => None,
            }));
        }
    }
    for (index, node) in auxiliary.iter_mut().enumerate().skip(1) {
        if !consumed.contains(&index) {
            node.after.push(Node::Auxiliary(0));
        }
    }
}

// Unhandled creates already refuse in Reconstruction before key facts are derived.
#[allow(clippy::wildcard_enum_match_arm)]
fn supplies_key(change: &Change, id: &ObjectIdentity) -> bool {
    let key = |table: &pbps_model::TableName, name: Option<&pbps_model::ObjectLocalName>| {
        name.is_some_and(|name| object(&table.schema, name.raw()) == *id)
    };
    match change {
        Change::CreateTable { name, table, .. } => {
            key(
                name,
                table.primary_key.as_ref().and_then(|p| p.name.as_ref()),
            ) || table.unique.keys().any(|n| key(name, Some(n)))
        }
        Change::SetPrimaryKey { table, to, .. } => {
            key(table, to.as_ref().and_then(|p| p.name.as_ref()))
        }
        Change::AddUnique { table, name, .. } => key(table, Some(name)),
        _ => false,
    }
}
