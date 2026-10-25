//! Executable enum labels and type-based plan probe deferral (DEC-1635.6).
use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::Dialect;
use pbps_model::{
    Change, ChangeSet, ColumnType, EnumPlacement, Hints, IdsFile, ManagedType, Schema, TypeRef,
    Uid, ValueSource,
};

use crate::{
    DiffError, Side,
    identity::type_refs::{TypeIdentity, TypeResolver, TypeSide},
};

fn refusal(name: &TypeRef, reason: impl Into<String>) -> DiffError {
    DiffError::TypeLifecycleUnsupported {
        name: name.clone(),
        reason: reason.into(),
    }
}

pub(crate) fn changes(
    base: Side<'_>,
    desired: Side<'_>,
    hints: &Hints,
) -> (Vec<Change>, Vec<DiffError>) {
    let resolver = TypeResolver::new(base.ids, desired.ids);
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (uid, name) in &desired.ids.types {
        let Some(ManagedType::Enum { values: wanted }) = desired.schema.types.get(name) else {
            continue;
        };
        let Some(ManagedType::Enum { values: old }) = resolver
            .name(TypeSide::Base, uid)
            .and_then(|n| base.schema.types.get(n))
        else {
            // Creation has no old label identity, even if a name is reused.
            // Absorb history like identity::intent_is_absorbed (DEC-1635.6).
            continue;
        };
        if old == wanted
            && hints
                .enum_value_renames
                .get(name)
                .is_none_or(BTreeMap::is_empty)
        {
            continue;
        }
        let mut current = old.clone();
        let mut pending = BTreeMap::new();
        if let Some(renames) = hints.enum_value_renames.get(name) {
            let graph: BTreeMap<_, _> = renames.iter().map(|(to, from)| (from, to)).collect();
            let targets: BTreeSet<_> = renames.keys().collect();
            let mut applied_sources = BTreeSet::new();
            // A completed chain reuses its intermediate source names as final
            // labels. Absorb the whole chain before testing pairs individually;
            // cycles have no head and must still reach the occupied-target check.
            for head in graph.keys().copied().filter(|head| !targets.contains(head)) {
                let mut at = head;
                let mut chain = Vec::new();
                let mut applied = !old.contains(head);
                while let Some(&to) = graph.get(&at) {
                    chain.push(at);
                    applied &= old.contains(to);
                    at = to;
                }
                if applied {
                    applied_sources.extend(chain);
                }
            }
            let mut sources = BTreeSet::new();
            for (to, from) in renames {
                if !sources.insert(from) {
                    errors.push(refusal(
                        name,
                        format!(
                            "two labels claim renamed_from `{}`",
                            pbps_model::escape::Escaped(from)
                        ),
                    ));
                } else if !wanted.contains(to) {
                    errors.push(refusal(
                        name,
                        format!(
                            "renamed label `{}` is not declared",
                            pbps_model::escape::Escaped(to)
                        ),
                    ));
                } else if from == to {
                    errors.push(refusal(
                        name,
                        format!(
                            "enum label `{}` cannot rename itself",
                            pbps_model::escape::Escaped(from)
                        ),
                    ));
                } else if applied_sources.contains(&from) {
                    continue;
                } else if current.contains(from) {
                    pending.insert(from.clone(), to.clone());
                } else if !current.contains(to) {
                    // Like type rename annotations, an already applied spelling
                    // is absorbed; an absent source and absent target is a typo.
                    errors.push(refusal(
                        name,
                        format!(
                            "enum label `{}` named by renamed_from does not exist",
                            pbps_model::escape::Escaped(from)
                        ),
                    ));
                }
            }
        }
        let mut ordinal = 0;
        while !pending.is_empty() {
            let ready = pending
                .iter()
                .find(|(_, to)| !current.contains(to))
                .map(|(from, to)| (from.clone(), to.clone()));
            let Some((from, to)) = ready else {
                let labels = pending
                    .keys()
                    .map(|v| format!("`{}`", pbps_model::escape::Escaped(v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                errors.push(refusal(name, format!("enum label rename cycle (swap) or occupied target involving {labels} is refused; use separate plans")));
                break;
            };
            pending.remove(&from);
            let index = current
                .iter()
                .position(|v| v == &from)
                .expect("validated source");
            current[index] = to.clone();
            out.push(Change::RenameEnumValue {
                uid: uid.clone(),
                name: name.clone(),
                from,
                to,
                ordinal,
                definition: Box::new(ManagedType::Enum {
                    values: current.clone(),
                }),
            });
            ordinal += 1;
        }
        for value in &current {
            if !wanted.contains(value) {
                errors.push(refusal(name, format!("enum label `{}` was removed without rename intent; removal requires #1635 slice 3b", pbps_model::escape::Escaped(value))));
            }
        }
        let retained: Vec<_> = wanted.iter().filter(|v| current.contains(v)).collect();
        if retained != current.iter().collect::<Vec<_>>() {
            let labels = retained
                .iter()
                .map(|label| format!("`{}`", pbps_model::escape::Escaped(label)))
                .collect::<Vec<_>>()
                .join(", ");
            errors.push(refusal(name, format!("relative order of retained enum labels {labels} changed; reordering requires #1635 slice 3b")));
            continue;
        }
        for (index, value) in wanted.iter().enumerate() {
            if current.contains(value) {
                continue;
            }
            let placement = if index > 0 {
                Some(EnumPlacement::After(wanted[index - 1].clone()))
            } else {
                current.first().cloned().map(EnumPlacement::Before)
            };
            current.insert(index, value.clone());
            out.push(Change::AddEnumValue {
                uid: uid.clone(),
                name: name.clone(),
                value: value.clone(),
                placement,
                ordinal,
                definition: Box::new(ManagedType::Enum {
                    values: current.clone(),
                }),
            });
            ordinal += 1;
        }
    }
    (out, errors)
}

/// Resolve arrays and domain bases through the identity map of this side.
pub fn uses_enum(ty: &ColumnType, uid: &Uid, schema: &Schema, ids: &IdsFile) -> bool {
    let resolver = TypeResolver::new(ids, ids);
    let mut reference = ty.type_ref();
    let mut seen = BTreeSet::new();
    while let Some(name) = reference {
        if !seen.insert(name) {
            return false;
        }
        let TypeIdentity::Declared(at) = resolver.resolve(TypeSide::Desired, name) else {
            return false;
        };
        if at == uid {
            return true;
        }
        let Some(name) = resolver.name(TypeSide::Desired, at) else {
            return false;
        };
        reference = match schema.types.get(name) {
            Some(ManagedType::Domain { base, .. }) => base.type_ref(),
            Some(ManagedType::Enum { .. }) | None => None,
        };
    }
    false
}

/// A probe view only: the declaration used for diffing and recording stays intact.
/// A pending label's spelling does not exist yet. Defer expressions by their
/// resolved enum identity instead of inspecting or reversing their SQL text.
#[allow(clippy::wildcard_enum_match_arm)] // Only label renames defer probes.
pub fn probe_schema(base: Side<'_>, desired: Side<'_>, hints: &Hints) -> Schema {
    let renamed: BTreeSet<_> = changes(base, desired, hints)
        .0
        .into_iter()
        .filter_map(|change| match change {
            Change::RenameEnumValue { uid, .. } => Some(uid),
            _ => None,
        })
        .collect();
    let deferred = |ty: &ColumnType| {
        renamed
            .iter()
            .any(|uid| uses_enum(ty, uid, desired.schema, desired.ids))
    };
    let mut probe = desired.schema.clone();
    for (name, table) in &mut probe.tables {
        let mut owner = &desired.schema.tables[name];
        let mut seen = BTreeSet::new();
        while let Some(of) = &owner.partition_of {
            if !seen.insert(&of.parent) {
                break;
            }
            let Some(parent) = desired.schema.tables.get(&of.parent) else {
                break;
            };
            owner = parent;
        }
        if owner.columns.values().any(|c| deferred(&c.ty)) {
            // CHECKs belong to the table. Determining which column arbitrary
            // SQL reads would reintroduce the lexical binding guess.
            table.checks.clear();
        }
        for column in table.columns.values_mut() {
            if deferred(&column.ty) && matches!(column.value_source, ValueSource::Default(_)) {
                column.value_source = ValueSource::None;
            }
        }
        if let Some(of) = &mut table.partition_of {
            for (name, own) in &mut of.columns {
                if owner.columns.get(name).is_some_and(|c| deferred(&c.ty)) {
                    own.default = None;
                }
            }
        }
    }
    for definition in probe.types.values_mut() {
        if let ManagedType::Domain {
            base,
            default,
            checks,
            ..
        } = definition
            && deferred(base)
        {
            *default = None;
            checks.clear();
        }
    }
    probe
}

// Enum-array constants deparse as array text, including bounds and quoted or
// backslash-escaped elements. NULL is not an enum label unless it is quoted.
fn array_items(text: &str) -> Option<Vec<Option<String>>> {
    let mut i = text.find('{')?;
    if i > 0 && !(text.starts_with('[') && text[..i].ends_with('=')) {
        return None;
    }
    let mut out = Vec::new();
    let mut depth = 0usize;
    while i < text.len() {
        let ch = text[i..].chars().next()?;
        match ch {
            '{' => {
                depth += 1;
                i += 1;
            }
            '}' => {
                depth = depth.checked_sub(1)?;
                i += 1;
            }
            ',' => {
                i += 1;
            }
            ch if ch.is_whitespace() => {
                i += ch.len_utf8();
            }
            _ => {
                if depth == 0 {
                    return None;
                }
                let quoted = ch == '"';
                if quoted {
                    i += 1;
                }
                let mut value = String::new();
                let mut closed = !quoted;
                while i < text.len() {
                    let ch = text[i..].chars().next()?;
                    if ch == '\\' {
                        i += 1;
                        let next = text[i..].chars().next()?;
                        value.push(next);
                        i += next.len_utf8();
                    } else if quoted && ch == '"' {
                        i += 1;
                        closed = true;
                        break;
                    } else if !quoted && matches!(ch, ',' | '}') {
                        break;
                    } else {
                        value.push(ch);
                        i += ch.len_utf8();
                    }
                }
                if !closed {
                    return None;
                }
                let value = if quoted {
                    Some(value)
                } else if value.trim().eq_ignore_ascii_case("NULL") {
                    None
                } else {
                    Some(value.trim().to_owned())
                };
                out.push(value);
            }
        }
    }
    (depth == 0).then_some(out)
}

fn literal_uses(value: &str, label: &str) -> bool {
    value == label
        || array_items(value).is_some_and(|items| items.iter().any(|v| v.as_deref() == Some(label)))
}

fn text_uses(text: &str, label: &str, dialect: &(impl Dialect + ?Sized)) -> bool {
    dialect
        .lexicon()
        .string_literals(text)
        .iter()
        .any(|v| literal_uses(v, label))
}

/// Only changed surfaces are considered. PL/pgSQL bodies bind at invocation,
/// unlike views and SQL bodies, so their accepted creation is not refused.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn unsafe_uses(
    changes: &ChangeSet,
    desired: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
) -> Vec<DiffError> {
    let mut errors = Vec::new();
    for added in &changes.changes {
        let Change::AddEnumValue {
            uid, name, value, ..
        } = &added.change
        else {
            continue;
        };
        let typed = |ty: &ColumnType| uses_enum(ty, uid, desired.schema, desired.ids);
        let text = |s: &str| text_uses(s, value, dialect);
        let inherited = |table: &pbps_model::Table| {
            let mut columns = table;
            let mut seen = BTreeSet::new();
            while let Some(of) = &columns.partition_of {
                if !seen.insert(&of.parent) {
                    break;
                }
                let Some(parent) = desired.schema.tables.get(&of.parent) else {
                    break;
                };
                columns = parent;
            }
            columns.clone()
        };
        let table_uses = |table: &pbps_model::Table| {
            let columns = inherited(table);
            let has_enum = columns.columns.values().any(|c| typed(&c.ty));
            table.columns.values().any(|c| {
                typed(&c.ty) && matches!(&c.value_source, ValueSource::Default(s) if text(s))
            }) || (has_enum && table.checks.values().any(|c| text(&c.expression)))
        };
        for step in &changes.changes {
            let used = match &step.change {
                Change::CreateTable { table, .. }
                | Change::AttachPartition { shape: table, .. } => table_uses(table),
                Change::AddColumn { column, .. } => {
                    typed(&column.ty)
                        && matches!(&column.value_source, ValueSource::Default(s) if text(s))
                }
                Change::AlterColumnDefault {
                    column,
                    to: Some(to),
                    ..
                } => {
                    desired
                        .schema
                        .tables
                        .get(&column.table)
                        .and_then(|t| t.columns.get(&column.name))
                        .is_some_and(|c| typed(&c.ty))
                        && text(to)
                }
                Change::SetPartitionDefault {
                    table,
                    column,
                    to: Some(to),
                    ..
                } => {
                    desired.schema.tables.get(table).is_some_and(|t| {
                        inherited(t)
                            .columns
                            .get(column)
                            .is_some_and(|c| typed(&c.ty))
                    }) && text(to)
                }
                Change::AddCheck {
                    table,
                    constraint: check,
                    ..
                } => {
                    desired
                        .schema
                        .tables
                        .get(table)
                        .is_some_and(|t| inherited(t).columns.values().any(|c| typed(&c.ty)))
                        && text(&check.expression)
                }
                Change::CreateType { definition, .. } => match definition.as_ref() {
                    ManagedType::Domain {
                        base,
                        default,
                        checks,
                        ..
                    } => {
                        typed(base)
                            && (default.as_ref().is_some_and(|s| text(s))
                                || checks.values().any(|s| text(s)))
                    }
                    ManagedType::Enum { .. } => false,
                },
                Change::CreateModule { module, .. } | Change::AlterModule { module, .. } => {
                    let lexicon = dialect.lexicon();
                    let deferred = matches!(
                        module.kind,
                        pbps_model::ModuleKind::Function | pbps_model::ModuleKind::Procedure
                    ) && lexicon.routine_language(&module.definition).as_deref()
                        == Some("plpgsql");
                    let whole_definition = || {
                        text(&module.definition)
                            || lexicon
                                .string_literals(&module.definition)
                                .iter()
                                .any(|body| text(body))
                    };
                    if deferred {
                        // Only the PL/pgSQL body binds on invocation. Parameter
                        // defaults and other clauses bind when it is created.
                        // An unreadable body keeps the conservative whole scan.
                        lexicon.routine_body(&module.definition).map_or_else(
                            whole_definition,
                            |body| {
                                text(&module.definition[..body.start])
                                    || text(&module.definition[body.end..])
                            },
                        )
                    } else {
                        whole_definition()
                    }
                }
                _ => false,
            };
            if used {
                errors.push(DiffError::EnumValueUse {
                    name: name.clone(),
                    label: value.clone(),
                    subject: step.change.subject(),
                });
            }
        }
    }
    errors
}

/// Replay label payloads against approved before-definitions. A saved artifact
/// cannot substitute a final definition for an intermediate check.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn valid_steps(changes: &ChangeSet, pins: &[pbps_model::DeclaredTypePin]) -> bool {
    let mut definitions: BTreeMap<_, _> = pins
        .iter()
        .filter_map(|p| {
            p.before
                .as_ref()
                .map(|b| (p.uid.clone(), b.definition.clone()))
        })
        .collect();
    for step in &changes.changes {
        match &step.change {
            Change::CreateType {
                uid, definition, ..
            } => {
                definitions.insert(uid.clone(), (**definition).clone());
            }
            Change::AddEnumValue {
                uid,
                name,
                value,
                placement,
                definition,
                ..
            } => {
                if !pins.iter().any(|p| &p.uid == uid && &p.name == name) {
                    return false;
                }
                let Some(ManagedType::Enum { values }) = definitions.get_mut(uid) else {
                    return false;
                };
                if values.contains(value) {
                    return false;
                }
                let index = match placement {
                    None => values.len(),
                    Some(EnumPlacement::Before(neighbour)) => {
                        let Some(i) = values.iter().position(|v| v == neighbour) else {
                            return false;
                        };
                        i
                    }
                    Some(EnumPlacement::After(neighbour)) => {
                        let Some(i) = values.iter().position(|v| v == neighbour) else {
                            return false;
                        };
                        i + 1
                    }
                };
                values.insert(index, value.clone());
                if definitions.get(uid) != Some(definition.as_ref()) {
                    return false;
                }
            }
            Change::RenameEnumValue {
                uid,
                name,
                from,
                to,
                definition,
                ..
            } => {
                if !pins.iter().any(|p| &p.uid == uid && &p.name == name) {
                    return false;
                }
                let Some(ManagedType::Enum { values }) = definitions.get_mut(uid) else {
                    return false;
                };
                if values.contains(to) {
                    return false;
                }
                let Some(i) = values.iter().position(|v| v == from) else {
                    return false;
                };
                values[i] = to.clone();
                if definitions.get(uid) != Some(definition.as_ref()) {
                    return false;
                }
            }
            _ => {}
        }
    }
    pins.iter().all(|p| {
        !matches!(p.definition, ManagedType::Enum { .. })
            || definitions.get(&p.uid) == Some(&p.definition)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, DeclaredTypeBefore, DeclaredTypePin, PlannedChange, Table};

    fn enumeration(values: &[&str]) -> ManagedType {
        ManagedType::Enum {
            values: values.iter().map(|v| (*v).into()).collect(),
        }
    }
    fn world(values: &[&str]) -> (Schema, IdsFile) {
        (
            Schema {
                types: [("app.e".parse().unwrap(), enumeration(values))].into(),
                ..Default::default()
            },
            IdsFile {
                types: [("y_000001".parse().unwrap(), "app.e".parse().unwrap())].into(),
                ..Default::default()
            },
        )
    }
    fn planned(
        old: &[&str],
        new: &[&str],
        renames: &[(&str, &str)],
    ) -> (ChangeSet, Vec<DiffError>, Schema, IdsFile) {
        let (base, ids) = world(old);
        let (desired, _) = world(new);
        let hints = Hints {
            enum_value_renames: [(
                "app.e".parse().unwrap(),
                renames
                    .iter()
                    .map(|(to, from)| ((*to).into(), (*from).into()))
                    .collect(),
            )]
            .into(),
            ..Default::default()
        };
        let (steps, errors) = changes(
            Side {
                schema: &base,
                ids: &ids,
            },
            Side {
                schema: &desired,
                ids: &ids,
            },
            &hints,
        );
        (
            ChangeSet {
                changes: steps.into_iter().map(PlannedChange::new).collect(),
            },
            errors,
            desired,
            ids,
        )
    }

    fn annotated_creation(
        base: &(Schema, IdsFile),
        desired: &(Schema, IdsFile),
        renames: &[(&str, &str)],
    ) -> ChangeSet {
        let hints = Hints {
            enum_value_renames: [(
                "app.e".parse().unwrap(),
                renames
                    .iter()
                    .map(|(to, from)| ((*to).into(), (*from).into()))
                    .collect(),
            )]
            .into(),
            ..Default::default()
        };
        let before = Side {
            schema: &base.0,
            ids: &base.1,
        };
        let after = Side {
            schema: &desired.0,
            ids: &desired.1,
        };
        let plan = crate::diff(before, after, &pbps_dialect::MinimalDialect, &hints)
            .expect("a created type has final labels, not pending label identities");
        assert_eq!(probe_schema(before, after, &hints), desired.0);
        plan.changes().clone()
    }

    #[test]
    fn created_enum_labels_ignore_history_when_other_base_types_exist() {
        let mut base = world(&["kept"]);
        let definition = base.0.types.remove(&"app.e".parse().unwrap()).unwrap();
        base.0.types.insert("app.kept".parse().unwrap(), definition);
        *base.1.types.values_mut().next().unwrap() = "app.kept".parse().unwrap();
        let mut desired = base.clone();
        desired
            .0
            .types
            .insert("app.e".parse().unwrap(), enumeration(&["blue", "ok"]));
        desired
            .1
            .types
            .insert("y_000002".parse().unwrap(), "app.e".parse().unwrap());
        let plan = annotated_creation(&base, &desired, &[("blue", "sad")]);
        assert_eq!(plan.changes.len(), 1);
        assert!(matches!(&plan.changes[0].change,
            Change::CreateType { uid, name, definition }
                if uid.to_string() == "y_000002"
                    && name == &"app.e".parse::<pbps_model::TypeRef>().unwrap()
                    && **definition == enumeration(&["blue", "ok"])));
    }

    #[test]
    fn created_enum_labels_ignore_history_and_swaps_in_an_empty_bootstrap_base() {
        for (values, renames) in [
            (vec!["blue", "ok"], vec![("blue", "sad")]),
            (vec!["a", "b"], vec![("a", "b"), ("b", "a")]),
            (
                vec!["a", "b", "c"],
                vec![("a", "b"), ("b", "c"), ("c", "a")],
            ),
        ] {
            let base = (Schema::default(), IdsFile::default());
            let desired = world(&values);
            let plan = annotated_creation(&base, &desired, &renames);
            assert_eq!(plan.changes.len(), 1);
            assert!(matches!(&plan.changes[0].change,
                Change::CreateType { uid, name, definition }
                    if uid.to_string() == "y_000001"
                        && name == &"app.e".parse::<pbps_model::TypeRef>().unwrap()
                        && **definition == enumeration(&values)));
        }
    }

    #[test]
    fn created_enum_labels_use_the_new_uid_when_replacing_the_same_name() {
        let base = world(&["unrelated", "old"]);
        let mut desired = world(&["blue", "ok"]);
        desired.1.types = [("y_000002".parse().unwrap(), "app.e".parse().unwrap())].into();
        let plan = annotated_creation(&base, &desired, &[("blue", "sad")]);
        assert_eq!(plan.changes.len(), 2);
        assert!(plan.changes.iter().any(|step| matches!(&step.change,
            Change::DropType { uid, .. } if uid.to_string() == "y_000001")));
        assert!(plan.changes.iter().any(|step| matches!(&step.change,
            Change::CreateType { uid, name, definition }
                if uid.to_string() == "y_000002"
                    && name == &"app.e".parse::<pbps_model::TypeRef>().unwrap()
                    && **definition == enumeration(&["blue", "ok"]))));
    }

    #[test]
    fn existing_enum_labels_still_refuse_an_absent_rename_source_and_target() {
        let (_, errors, _, _) = planned(&["a", "b"], &["blue", "b"], &[("blue", "sad")]);
        assert!(
            errors.iter().any(|error| error
                .to_string()
                .contains("enum label `sad` named by renamed_from does not exist")),
            "{errors:?}"
        );
    }

    #[test]
    fn additions_walk_declared_positions_using_an_existing_neighbour() {
        for wanted in [
            vec!["x", "a", "b"],
            vec!["a", "x", "b"],
            vec!["a", "b", "x"],
            vec!["x", "y", "a", "m", "n", "b", "z"],
            vec![],
        ] {
            let (steps, errors, desired, ids) = planned(&["a", "b"], &wanted, &[]);
            if wanted.is_empty() {
                assert!(!errors.is_empty());
                continue;
            }
            assert!(errors.is_empty(), "{errors:?}");
            let (base, _) = world(&["a", "b"]);
            let pin = DeclaredTypePin {
                uid: ids.types.keys().next().unwrap().clone(),
                name: "app.e".parse().unwrap(),
                definition: desired.types.values().next().unwrap().clone(),
                before: Some(DeclaredTypeBefore {
                    name: "app.e".parse().unwrap(),
                    definition: base.types.values().next().unwrap().clone(),
                }),
            };
            assert!(valid_steps(&steps, std::slice::from_ref(&pin)));
            let mut broken = steps.clone();
            if let Some(step) = broken.changes.first_mut()
                && let Change::AddEnumValue { placement, .. } = &mut step.change
            {
                *placement = Some(EnumPlacement::After("missing".into()));
            }
            assert!(!valid_steps(&broken, &[pin]));
        }
        let (steps, errors, _, _) = planned(&[], &["a", "b"], &[]);
        assert!(errors.is_empty());
        assert!(matches!(
            &steps.changes[0].change,
            Change::AddEnumValue {
                placement: None,
                ..
            }
        ));
        assert!(
            matches!(&steps.changes[1].change, Change::AddEnumValue { placement: Some(EnumPlacement::After(v)), .. } if v == "a")
        );
    }

    #[test]
    fn applied_label_chains_absorb_every_pair_before_planning_new_labels() {
        let hints = [("b", "a"), ("c", "b")];
        let (steps, errors, _, _) = planned(&["b", "c"], &["b", "c", "d"], &hints);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(steps.changes.len(), 1);
        assert!(matches!(&steps.changes[0].change,
            Change::AddEnumValue { value, placement: Some(EnumPlacement::After(neighbour)), .. }
                if value == "d" && neighbour == "c"));
        let (noop, errors, _, _) = planned(&["b", "c"], &["b", "c"], &hints);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(noop.is_empty());
    }

    #[test]
    fn incomplete_label_chains_keep_only_remaining_links_and_do_not_absorb_occupied_targets() {
        let (steps, errors, _, _) = planned(&["a", "c"], &["b", "c"], &[("b", "a"), ("c", "b")]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(steps.changes.len(), 1);
        assert!(
            matches!(&steps.changes[0].change, Change::RenameEnumValue { from, to, .. } if from == "a" && to == "b")
        );

        let (steps, errors, _, _) = planned(
            &["b", "c", "x", "z"],
            &["b", "c", "y", "z", "d"],
            &[("b", "a"), ("c", "b"), ("y", "x"), ("z", "y")],
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(steps.changes.len(), 2);
        assert!(
            matches!(&steps.changes[0].change, Change::RenameEnumValue { from, to, .. } if from == "x" && to == "y")
        );
        assert!(
            matches!(&steps.changes[1].change, Change::AddEnumValue { value, .. } if value == "d")
        );

        for wanted in [vec!["b", "a"], vec!["a", "b"]] {
            let (_, errors, _, _) = planned(&["a", "b"], &wanted, &[("b", "a"), ("a", "b")]);
            assert!(
                errors
                    .iter()
                    .any(|error| error.to_string().contains("cycle (swap)")),
                "{errors:?}"
            );
        }
        let (_, errors, _, _) = planned(&["a", "c"], &["c"], &[("c", "a")]);
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("occupied target")),
            "{errors:?}"
        );
    }

    #[test]
    fn label_intent_preserves_identity_and_refuses_missing_sources_swaps_removals_and_reorder() {
        let (steps, errors, _, _) = planned(&["a", "b"], &["b", "c"], &[("b", "a"), ("c", "b")]);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            matches!(&steps.changes[0].change, Change::RenameEnumValue { from, to, .. } if from == "b" && to == "c")
        );
        assert!(
            matches!(&steps.changes[1].change, Change::RenameEnumValue { from, to, .. } if from == "a" && to == "b")
        );
        assert!(
            planned(&["b", "c"], &["b", "c"], &[("b", "a"), ("c", "b")])
                .0
                .is_empty()
        );
        for (new, intent, message) in [
            (vec!["x", "b"], vec![("x", "missing")], "missing"),
            (vec!["x", "y"], vec![("x", "a"), ("y", "a")], "two labels"),
            (vec!["b", "a"], vec![("a", "b"), ("b", "a")], "swap"),
            (vec!["b"], vec![], "3b"),
            (vec!["b", "a"], vec![], "3b"),
        ] {
            let (_, errors, _, _) = planned(&["a", "b"], &new, &intent);
            assert!(
                errors.iter().any(|e| e.to_string().contains(message)),
                "{errors:?}"
            );
        }
        assert!(planned(&["a", "b"], &["a", "b"], &[]).0.is_empty());
    }

    #[test]
    fn pending_label_renames_defer_only_expressions_with_the_renamed_type_identity() {
        let (mut base, mut ids) = world(&["sad", "ok"]);
        ids.types
            .insert("y_000002".parse().unwrap(), "app.other".parse().unwrap());
        ids.types
            .insert("y_000003".parse().unwrap(), "app.d".parse().unwrap());
        base.types
            .insert("app.other".parse().unwrap(), enumeration(&["sad", "ok"]));
        base.types.insert(
            "app.d".parse().unwrap(),
            ManagedType::Domain {
                base: "app.e[]".parse().unwrap(),
                not_null: false,
                default: Some("'{sad}'".into()),
                checks: [("kept".into(), "VALUE <> '{sad}'".into())].into(),
                collation: None,
            },
        );
        for (name, ty) in [
            ("direct", "app.e"),
            ("array", "app.e[]"),
            ("domain", "app.d"),
            ("other", "app.other"),
            ("text", "text"),
        ] {
            base.tables.insert(
                format!("app.{name}").parse().unwrap(),
                Table {
                    columns: [(
                        "m".into(),
                        Column {
                            value_source: ValueSource::Default("'sad'".into()),
                            ..Column::new(ty.parse().unwrap())
                        },
                    )]
                    .into(),
                    checks: [(
                        "kept".into(),
                        pbps_model::CheckConstraint {
                            expression: "m <> 'sad'".into(),
                        },
                    )]
                    .into(),
                    ..Default::default()
                },
            );
        }
        base.tables.insert(
            "app.child".parse().unwrap(),
            Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: "app.direct".parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: [(
                        "m".into(),
                        pbps_model::PartitionColumn {
                            default: Some("'sad'".into()),
                            not_null: false,
                        },
                    )]
                    .into(),
                }),
                ..Default::default()
            },
        );
        let mut desired = base.clone();
        desired
            .types
            .insert("app.e".parse().unwrap(), enumeration(&["blue", "ok"]));
        let hints = Hints {
            enum_value_renames: [(
                "app.e".parse().unwrap(),
                [("blue".into(), "sad".into())].into(),
            )]
            .into(),
            ..Default::default()
        };
        let original = desired.clone();
        let probe = probe_schema(
            Side {
                schema: &base,
                ids: &ids,
            },
            Side {
                schema: &desired,
                ids: &ids,
            },
            &hints,
        );
        for name in ["direct", "array", "domain"] {
            let table = &probe.tables[&format!("app.{name}").parse().unwrap()];
            assert_eq!(table.columns["m"].value_source, ValueSource::None, "{name}");
            assert!(table.checks.is_empty(), "{name}");
        }
        for name in ["other", "text"] {
            let name = format!("app.{name}").parse().unwrap();
            assert_eq!(probe.tables[&name], desired.tables[&name]);
        }
        assert!(
            probe.tables[&"app.child".parse().unwrap()]
                .partition_of
                .as_ref()
                .unwrap()
                .columns["m"]
                .default
                .is_none()
        );
        let ManagedType::Domain {
            default, checks, ..
        } = &probe.types[&"app.d".parse().unwrap()]
        else {
            panic!("domain")
        };
        assert!(default.is_none() && checks.is_empty());
        assert_eq!(
            desired, original,
            "probe deferral must not change what is planned or recorded"
        );
        assert_eq!(
            probe_schema(
                Side {
                    schema: &desired,
                    ids: &ids
                },
                Side {
                    schema: &desired,
                    ids: &ids
                },
                &hints
            ),
            desired,
            "already-applied annotations defer nothing"
        );
    }

    #[test]
    fn new_label_uses_follow_array_and_domain_identities_but_do_not_refuse_old_values() {
        let (mut steps, errors, mut schema, mut ids) = planned(&["old"], &["old", "new"], &[]);
        assert!(errors.is_empty());
        let domain_uid: Uid = "y_000002".parse().unwrap();
        ids.types
            .insert(domain_uid.clone(), "app.d".parse().unwrap());
        schema.types.insert(
            "app.d".parse().unwrap(),
            ManagedType::Domain {
                base: "app.e[]".parse().unwrap(),
                not_null: false,
                default: None,
                checks: BTreeMap::new(),
                collation: None,
            },
        );
        let mut external_ids = ids.clone();
        external_ids.types.remove(&domain_uid);
        assert!(!uses_enum(
            &"app.d".parse().unwrap(),
            &"y_000001".parse().unwrap(),
            &schema,
            &external_ids
        ));
        for ty in ["app.e", "app.e[]", "app.d", "app.d[]"] {
            assert!(uses_enum(
                &ty.parse().unwrap(),
                &"y_000001".parse().unwrap(),
                &schema,
                &ids
            ));
            assert!(!uses_enum(
                &ty.parse().unwrap(),
                &domain_uid,
                &Schema::default(),
                &IdsFile::default()
            ));
            for value in ["old", "new"] {
                steps.changes.truncate(1);
                steps.changes.push(PlannedChange::new(Change::CreateTable {
                    uid: "t_000001".parse().unwrap(),
                    name: "app.t".parse().unwrap(),
                    table: Box::new(Table {
                        columns: [(
                            "m".into(),
                            Column {
                                value_source: ValueSource::Default(format!("'{value}'")),
                                ..Column::new(ty.parse().unwrap())
                            },
                        )]
                        .into(),
                        ..Default::default()
                    }),
                    beside_default: None,
                    moves_at_most: None,
                }));
                let errors = unsafe_uses(
                    &steps,
                    Side {
                        schema: &schema,
                        ids: &ids,
                    },
                    &pbps_dialect::MinimalDialect,
                );
                assert_eq!(errors.is_empty(), value == "old", "{ty}: {errors:?}");
            }
        }
    }

    #[test]
    fn plpgsql_parameter_defaults_bind_at_creation_while_only_the_body_is_deferred() {
        use pbps_model::{Module, ModuleKind};

        for (kind, definition, refused) in [
            (
                ModuleKind::Function,
                "(x app.e DEFAULT 'new') RETURNS app.e LANGUAGE plpgsql AS 'BEGIN RETURN x; END'",
                true,
            ),
            (
                ModuleKind::Function,
                "(x app.e DEFAULT 'new'::app.e) RETURNS app.e LANGUAGE plpgsql AS 'BEGIN RETURN x; END'",
                true,
            ),
            (
                ModuleKind::Function,
                "(x app.e = 'new') RETURNS app.e LANGUAGE plpgsql AS 'BEGIN RETURN x; END'",
                true,
            ),
            (
                ModuleKind::Procedure,
                "(x app.e DEFAULT 'new') LANGUAGE plpgsql AS 'BEGIN END'",
                true,
            ),
            (
                ModuleKind::Procedure,
                "(x app.e = 'new'::app.e) LANGUAGE plpgsql AS 'BEGIN END'",
                true,
            ),
            (
                ModuleKind::Function,
                "(x integer DEFAULT 1) RETURNS app.e AS 'BEGIN RETURN ''new''::app.e; END' LANGUAGE plpgsql",
                false,
            ),
            (
                ModuleKind::Function,
                "(x app.e DEFAULT 'old') RETURNS app.e LANGUAGE plpgsql AS 'BEGIN RETURN x; END'",
                false,
            ),
            (
                ModuleKind::Function,
                "(x app.e DEFAULT 'old') RETURNS app.e AS 'BEGIN RETURN ''new''::app.e; END' LANGUAGE plpgsql",
                false,
            ),
            (
                ModuleKind::Procedure,
                "(x app.e DEFAULT 'old') LANGUAGE plpgsql AS 'BEGIN PERFORM ''new''::app.e; END'",
                false,
            ),
        ] {
            for alter in [false, true] {
                let (mut steps, errors, schema, ids) = planned(&["old"], &["old", "new"], &[]);
                assert!(errors.is_empty());
                let id = if kind == ModuleKind::Procedure {
                    "app.p(app.e)"
                } else if definition.starts_with("(x integer") {
                    "app.f(integer)"
                } else {
                    "app.f(app.e)"
                }
                .parse()
                .unwrap();
                let module = Box::new(Module {
                    kind,
                    description: None,
                    definition: definition.into(),
                });
                steps.changes.push(PlannedChange::new(if alter {
                    Change::AlterModule { id, module }
                } else {
                    Change::CreateModule { id, module }
                }));
                let errors = unsafe_uses(
                    &steps,
                    Side {
                        schema: &schema,
                        ids: &ids,
                    },
                    &pbps_dialect::MinimalDialect,
                );
                assert_eq!(
                    !errors.is_empty(),
                    refused,
                    "alter={alter}: {definition}: {errors:?}"
                );
                if refused {
                    assert!(
                        matches!(&errors[..], [DiffError::EnumValueUse { name, label, .. }] if name.raw().to_string() == "app.e" && label == "new")
                    );
                }
            }
        }
    }

    #[test]
    fn inherited_enum_columns_keep_checks_and_own_defaults_in_scope_on_native_partition_keys() {
        let (labels, errors, mut schema, ids) = planned(&["old"], &["old", "new"], &[]);
        assert!(errors.is_empty());
        let parent: pbps_model::TableName = "app.parent".parse().unwrap();
        let child: pbps_model::TableName = "app.child".parse().unwrap();
        schema.tables.insert(
            parent.clone(),
            Table {
                columns: [
                    ("id".into(), Column::new("integer".parse().unwrap())),
                    ("m".into(), Column::new("app.e".parse().unwrap())),
                ]
                .into(),
                partition_by: Some(pbps_model::PartitionBy {
                    strategy: pbps_model::PartitionStrategy::List,
                    columns: vec!["id".into()],
                    external: false,
                }),
                ..Default::default()
            },
        );
        schema.tables.insert(
            child.clone(),
            Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: parent.clone(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Default::default()
            },
        );
        for value in ["old", "new"] {
            for change in [
                Change::AddCheck {
                    table: child.clone(),
                    name: "kept".into(),
                    constraint: pbps_model::CheckConstraint {
                        expression: format!("m <> '{value}'"),
                    },
                },
                Change::SetPartitionDefault {
                    uid: "c_000001".parse().unwrap(),
                    table: child.clone(),
                    parent: parent.clone(),
                    column: "m".into(),
                    from: None,
                    to: Some(format!("'{value}'")),
                    fallback: None,
                },
            ] {
                let mut steps = labels.clone();
                steps.changes.push(PlannedChange::new(change));
                let errors = unsafe_uses(
                    &steps,
                    Side {
                        schema: &schema,
                        ids: &ids,
                    },
                    &pbps_dialect::MinimalDialect,
                );
                assert_eq!(errors.is_empty(), value == "old", "{errors:?}");
            }
        }
    }

    #[test]
    fn domain_owned_defaults_and_checks_refuse_new_labels_but_accept_existing_ones() {
        for base in ["app.e", "app.e[]"] {
            for check in [false, true] {
                for value in ["old", "new"] {
                    let (mut steps, errors, mut schema, mut ids) =
                        planned(&["old"], &["old", "new"], &[]);
                    assert!(errors.is_empty());
                    let text = if base.ends_with("[]") {
                        format!("ARRAY['{value}'::app.e]")
                    } else {
                        format!("'{value}'::app.e")
                    };
                    let definition = ManagedType::Domain {
                        base: base.parse().unwrap(),
                        not_null: false,
                        default: (!check).then(|| text.clone()),
                        checks: if check {
                            [("kept".into(), format!("VALUE <> {text}"))].into()
                        } else {
                            Default::default()
                        },
                        collation: None,
                    };
                    let name: TypeRef = "app.d".parse().unwrap();
                    let uid: Uid = "y_000002".parse().unwrap();
                    ids.types.insert(uid.clone(), name.clone());
                    schema.types.insert(name.clone(), definition.clone());
                    steps.changes.push(PlannedChange::new(Change::CreateType {
                        uid,
                        name,
                        definition: Box::new(definition),
                    }));
                    let errors = unsafe_uses(
                        &steps,
                        Side {
                            schema: &schema,
                            ids: &ids,
                        },
                        &pbps_dialect::MinimalDialect,
                    );
                    assert_eq!(
                        errors.is_empty(),
                        value == "old",
                        "{base} {check}: {errors:?}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod array_tests {
    use super::*;
    #[test]
    fn new_label_use_reads_array_elements_instead_of_matching_substrings() {
        for value in ["[0:2]={sad,NULL,ok}", "{{sad,ok},{sad,sad}}", "{\"sad\"}"] {
            assert!(literal_uses(value, "sad"));
        }
        assert!(literal_uses(r#"{"a,b"}"#, "a,b"));
        assert!(!literal_uses("{NULL}", "NULL"));
        assert!(!literal_uses("{sadness}", "sad"));
        assert!(!literal_uses("not an array", "sad"));
    }
}
