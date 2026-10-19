//! A new relation's name exists before its generated expressions bind
//! (DEC-1503.1). All callers lower these same typed parts.

use pbps_dialect::DialectError;
use pbps_model::{Change, ColumnName, ColumnRef, IdsFile, ModuleId, ValueSource};

pub(crate) fn parts(
    change: &Change,
    ids: &IdsFile,
    functions: &[ModuleId],
    version: Option<i64>,
) -> Result<Option<Vec<Change>>, DialectError> {
    let Change::CreateTable {
        uid,
        name,
        table,
        beside_default,
        moves_at_most,
    } = change
    else {
        return Ok(None);
    };
    let calls = |text: &str| functions.iter().any(|id| super::may_call(text, id.name()));
    let Some(cut) = table.columns.values().position(|c| {
        matches!(&c.value_source,
        ValueSource::Generated(g) if calls(&g.expression))
    }) else {
        return Ok(None);
    };
    let suffix: Vec<_> = table.columns.keys().skip(cut).cloned().collect();
    // Prefix generations also lose suffix inputs. A forward read within
    // the retained prefix is safe because CREATE still supplies both columns
    // together (DEC-1503.2).
    let later_input = table
        .columns
        .iter()
        .enumerate()
        .any(|(at, (_, c))| {
            matches!(&c.value_source, ValueSource::Generated(g) if table.columns.keys().skip(cut.max(at + 1))
            .any(|column| super::may_read(&g.expression, column.raw())))
        });
    let partition_key = table
        .partition_by
        .as_ref()
        .is_some_and(|key| key.columns.iter().any(|c| suffix.contains(c)));
    let placeholder = partition_key || later_input;
    if placeholder && version.is_none_or(|v| v < super::SET_EXPRESSION_ARRIVED_IN) {
        return Ok(None);
    }
    let pending = if placeholder {
        table
            .columns
            .iter()
            .filter_map(|(column, c)| {
                matches!(&c.value_source, ValueSource::Generated(g) if calls(&g.expression))
                    .then_some(column.clone())
            })
            .collect::<Vec<_>>()
    } else {
        suffix
    };
    let late_column = |column: &ColumnName| pending.iter().any(|c| c == column);
    let late_text =
        |text: &str| calls(text) || pending.iter().any(|c| super::may_read(text, c.raw()));
    let column_uid = |column: &ColumnName| {
        ids.column_uid(&name.column(column))
            .cloned()
            .ok_or_else(|| DialectError::Invalid {
                dialect: "postgres",
                message: format!(
                    "{} has no recorded column identity for its creation parts",
                    name.column(column).shown()
                ),
            })
    };
    let mut bare = (**table).clone();
    let mut tail = Vec::new();
    if placeholder {
        for (column, spec) in &mut bare.columns {
            if let ValueSource::Generated(g) = &mut spec.value_source
                && calls(&g.expression)
            {
                let from = crate::emit::generation_placeholder(&spec.ty)?;
                tail.push(Change::AlterColumnExpression {
                    uid: column_uid(column)?,
                    column: ColumnRef::new(name.clone(), column.clone()),
                    from: from.clone(),
                    to: std::mem::replace(&mut g.expression, from),
                });
            }
        }
    } else {
        let columns = std::mem::take(&mut bare.columns);
        for (at, (column, spec)) in columns.into_iter().enumerate() {
            if at < cut {
                bare.columns.insert(column, spec);
            } else {
                tail.push(Change::AddColumn {
                    uid: column_uid(&column)?,
                    table: name.clone(),
                    name: column,
                    column: Box::new(spec),
                });
            }
        }
    }
    // Prefix-only validators already have their inputs in CREATE. Splitting
    // them would add statements without exposing another needed name.
    if bare
        .primary_key
        .as_ref()
        .is_some_and(|key| key.columns.iter().any(&late_column))
    {
        tail.push(Change::SetPrimaryKey {
            table: name.clone(),
            from: None,
            to: bare.primary_key.take(),
            nonclustered: !table.primary_key_is_clustered(),
        });
    }
    for (key, constraint) in std::mem::take(&mut bare.unique) {
        if constraint.columns.iter().any(&late_column) {
            tail.push(Change::AddUnique {
                table: name.clone(),
                clustered: table.unique_is_clustered(&key),
                name: key,
                constraint,
            });
        } else {
            bare.unique.insert(key, constraint);
        }
    }
    // The differ already separates foreign keys from every new table.
    for (key, constraint) in std::mem::take(&mut bare.foreign_keys) {
        tail.push(Change::AddForeignKey {
            table: name.clone(),
            name: key,
            constraint: Box::new(constraint),
        });
    }
    for (key, constraint) in std::mem::take(&mut bare.checks) {
        if late_text(&constraint.expression) {
            tail.push(Change::AddCheck {
                table: name.clone(),
                name: key,
                constraint,
            });
        } else {
            bare.checks.insert(key, constraint);
        }
    }
    for (key, index) in std::mem::take(&mut bare.indexes) {
        let deferred = index.columns.iter().any(|c| {
            c.key.column().is_some_and(late_column) || c.key.expression().is_some_and(late_text)
        }) || index.include.iter().any(&late_column)
            || index.filter.as_deref().is_some_and(late_text);
        if deferred {
            tail.push(Change::AddIndex {
                table: name.clone(),
                clustered: table.index_is_clustered(&key),
                name: key,
                index: Box::new(index),
            });
        } else {
            bare.indexes.insert(key, index);
        }
    }
    let identity_late = match &bare.replica_identity {
        Some(pbps_model::ReplicaIdentity::Index(target)) => !bare.indexes.contains_key(target),
        Some(pbps_model::ReplicaIdentity::Unique(target)) => !bare.unique.contains_key(target),
        Some(pbps_model::ReplicaIdentity::PrimaryKey) => bare.primary_key.is_none(),
        None | Some(pbps_model::ReplicaIdentity::Full | pbps_model::ReplicaIdentity::Nothing) => {
            false
        }
    };
    if identity_late {
        tail.push(Change::SetReplicaIdentity {
            uid: uid.clone(),
            table: name.clone(),
            to: bare.replica_identity.take(),
        });
    }
    for (column, spec) in &mut bare.columns {
        if let ValueSource::Default(text) = &mut spec.value_source
            && calls(text)
        {
            let to = std::mem::take(text);
            spec.value_source = ValueSource::None;
            tail.push(Change::AlterColumnDefault {
                uid: column_uid(column)?,
                column: name.column(column),
                from: None,
                to: Some(to),
            });
        }
    }
    let mut parts = vec![Change::CreateTable {
        uid: uid.clone(),
        name: name.clone(),
        table: Box::new(bare),
        beside_default: beside_default.clone(),
        moves_at_most: *moves_at_most,
    }];
    parts.extend(tail);
    Ok(Some(parts))
}
