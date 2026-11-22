//! Detaching a partition: what its keys, constraints and indexes are called once it stands alone.

use super::super::*;

/// What each of `parent`'s keys, constraints and indexes is called on the
/// partition declared as `declared` once detached, or what keeps the
/// declaration from being the shape a detach gives it: the parent's columns
/// in order, every key, constraint and index matched one to one by
/// definition, its name aside (#1544, DEC-1544.1), and the checks, indexes
/// (#1577) and foreign keys (#1632) `partition` has of its own, which a
/// detach leaves as they are, under their own names.
pub(in super::super) fn detached_names(
    parent: &Table,
    partition: &Table,
    declared: &Table,
    dialect: &(impl Dialect + ?Sized),
    pending_columns: bool,
) -> Result<Vec<DetachedName>, Vec<String>> {
    let mut names = Vec::new();
    let mut what = Vec::new();
    // Its own are matched by name before the parent's by definition: a
    // detach renames only what it gave the partition, so an own check or
    // index declared under another name, or changed, is a change this plan
    // does not make (#1581).
    // Its columns are its parent's with what it holds of them otherwise
    // (#1578): a detach keeps its own defaults and NOT NULLs. Its persistence
    // and storage parameters are its own, the parent having none (#1580),
    // and a detach keeps them too.
    let mut parent = parent.clone();
    parent.unlogged = partition.unlogged;
    parent
        .storage_parameters
        .clone_from(&partition.storage_parameters);
    if let Some(of) = &partition.partition_of {
        for (column, own) in &of.columns {
            if let Some(c) = parent.columns.get_mut(column) {
                if let Some(default) = &own.default {
                    c.value_source = pbps_model::ValueSource::Default(default.clone());
                }
                if own.not_null {
                    c.nullable = false;
                }
            }
        }
    }
    let parent = &parent;
    let mut declared = declared.clone();
    for (name, own) in &partition.checks {
        if declared.checks.get(name) == Some(own) {
            declared.checks.remove(name);
        } else {
            what.push(format!(
                "its own check `{name}` is not declared as it stands",
                name = name.shown(),
            ));
        }
    }
    for (name, own) in &partition.indexes {
        if declared.indexes.get(name) == Some(own) {
            declared.indexes.remove(name);
        } else {
            what.push(format!(
                "its own index `{name}` is not declared as it stands",
                name = name.shown(),
            ));
        }
    }
    // Its own foreign keys, as its own checks (#1632): measured on 16 and
    // 18, a detach keeps them under their names beside the parent's clones,
    // which it makes local.
    for (name, own) in &partition.foreign_keys {
        if declared.foreign_keys.get(name) == Some(own) {
            declared.foreign_keys.remove(name);
        } else {
            what.push(format!(
                "its own foreign key `{name}` is not declared as it stands",
                name = name.shown(),
            ));
        }
    }
    // Its own exclusions, as its own indexes (DEC-2448.7): a detach keeps them under their
    // names beside the parent's clone, which it makes local.
    for (name, own) in &partition.exclude {
        if declared.exclude.get(name) == Some(own) {
            declared.exclude.remove(name);
        } else {
            what.push(format!(
                "its own exclusion constraint `{name}` is not declared as it stands",
                name = name.shown(),
            ));
        }
    }
    let declared = &declared;
    // Types in the engine's spelling, as `diff_columns` compares them: the
    // parent's base reads `integer` back where its file says `int`. A
    // description is prose `diff` does not compare, and a connected base
    // never holds one, so it is not compared here either.
    let columns = |t: &Table| -> Vec<(ColumnName, pbps_model::Column)> {
        t.columns
            .iter()
            .map(|(name, c)| {
                let ty = dialect
                    .normalize_type(&c.ty)
                    .unwrap_or_else(|_| c.ty.clone());
                let column = pbps_model::Column {
                    ty,
                    description: None,
                    ..c.clone()
                };
                (name.clone(), column)
            })
            .collect()
    };
    if !pending_columns && columns(parent) != columns(declared) {
        what.push("its columns are not its parent's, in its parent's order".to_owned());
    }
    match (&parent.primary_key, &declared.primary_key) {
        (None, None) => {}
        (Some(from), Some(to))
            if from.columns == to.columns && from.storage_parameters == to.storage_parameters =>
        {
            names.push(DetachedName {
                kind: DetachedKind::PrimaryKey,
                parent: from.name.clone().unwrap_or_default(),
                name: to.name.clone(),
            });
        }
        (Some(_), _) | (None, Some(_)) => {
            what.push("its primary key is not its parent's".to_owned());
        }
    }
    fn matched<V: PartialEq>(
        kind: DetachedKind,
        words: &str,
        parent: &BTreeMap<pbps_model::ObjectLocalName, V>,
        declared: &BTreeMap<pbps_model::ObjectLocalName, V>,
        names: &mut Vec<DetachedName>,
        what: &mut Vec<String>,
    ) {
        let mut unused: Vec<(&pbps_model::ObjectLocalName, &V)> = declared.iter().collect();
        for (from, definition) in parent {
            match unused.iter().position(|(_, d)| *d == definition) {
                Some(at) => {
                    let (to, _) = unused.remove(at);
                    names.push(DetachedName {
                        kind,
                        parent: from.clone(),
                        name: Some(to.clone()),
                    });
                }
                None => what.push(format!(
                    "its parent's {words} `{from}` is missing",
                    from = from.shown(),
                )),
            }
        }
        for (to, _) in unused {
            what.push(format!(
                "the {words} `{to}` is not its parent's",
                to = to.shown(),
            ));
        }
    }
    matched(
        DetachedKind::Unique,
        "unique constraint",
        &parent.unique,
        &declared.unique,
        &mut names,
        &mut what,
    );
    matched(
        DetachedKind::ForeignKey,
        "foreign key",
        &parent.foreign_keys,
        &declared.foreign_keys,
        &mut names,
        &mut what,
    );
    matched(
        DetachedKind::Check,
        "check",
        &parent.checks,
        &declared.checks,
        &mut names,
        &mut what,
    );
    matched(
        DetachedKind::Index,
        "index",
        &parent.indexes,
        &declared.indexes,
        &mut names,
        &mut what,
    );
    // Its clone of the parent's exclusion becomes its own on a detach, under the name the
    // engine gave it, which the plan renames to the declared one (measured on 18.6).
    matched(
        DetachedKind::Exclusion,
        "exclusion constraint",
        &parent.exclude,
        &declared.exclude,
        &mut names,
        &mut what,
    );
    // Everything else is the parent's too, the partitioning aside.
    let rest = |t: &Table| Table {
        columns: Default::default(),
        primary_key: None,
        unique: Default::default(),
        foreign_keys: Default::default(),
        checks: Default::default(),
        indexes: Default::default(),
        exclude: Default::default(),
        partition_by: None,
        partition_of: None,
        description: None,
        ..t.clone()
    };
    if rest(parent) != rest(declared) {
        what.push(
            "it declares a setting or `data:` that is neither its parent's nor its own".to_owned(),
        );
    }
    if what.is_empty() {
        Ok(names)
    } else {
        Err(what)
    }
}

/// Column facts only: annotations and dependents do not make a meeting (DEC-2296.10).
pub(in super::super) fn parent_shape_changes(old: &Table, new: &Table) -> bool {
    old.computed != new.computed
        || !old
            .columns
            .iter()
            .map(|(n, c)| (n, &c.ty, &c.collation, c.nullable, &c.value_source))
            .eq(new
                .columns
                .iter()
                .map(|(n, c)| (n, &c.ty, &c.collation, c.nullable, &c.value_source)))
}
