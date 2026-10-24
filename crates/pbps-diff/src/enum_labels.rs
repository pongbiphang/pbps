//! Executable enum label changes and their bounded comparison adapter (DEC-1635.6).
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
            if let Some(renames) = hints.enum_value_renames.get(name) {
                for from in renames.values() {
                    errors.push(refusal(
                        name,
                        format!(
                            "enum label `{}` named by renamed_from does not exist",
                            pbps_model::escape::Escaped(from)
                        ),
                    ));
                }
            }
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
    enum_shape(ty, uid, schema, ids).is_some()
}

fn enum_shape(ty: &ColumnType, uid: &Uid, schema: &Schema, ids: &IdsFile) -> Option<bool> {
    let resolver = TypeResolver::new(ids, ids);
    let mut reference = ty.type_ref();
    let mut array = ty.is_array();
    let mut seen = BTreeSet::new();
    while let Some(name) = reference {
        if !seen.insert(name) {
            return None;
        }
        let TypeIdentity::Declared(at) = resolver.resolve(TypeSide::Desired, name) else {
            return None;
        };
        if at == uid {
            return Some(array);
        }
        let name = resolver.name(TypeSide::Desired, at)?;
        reference = match schema.types.get(name) {
            Some(ManagedType::Domain { base, .. }) => {
                array |= base.is_array();
                base.type_ref()
            }
            Some(ManagedType::Enum { .. }) | None => None,
        };
    }
    None
}

// Enum-array constants deparse as array text, including bounds and quoted or
// backslash-escaped elements. NULL is not an enum label unless it is quoted.
fn array_items(text: &str) -> Option<Vec<(usize, usize, Option<String>)>> {
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
                let start = i;
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
                out.push((start, i, value));
            }
        }
    }
    (depth == 0).then_some(out)
}

fn literal_uses(value: &str, label: &str) -> bool {
    value == label
        || array_items(value)
            .is_some_and(|items| items.iter().any(|(_, _, v)| v.as_deref() == Some(label)))
}

fn rename_array(value: &str, from: &str, to: &str) -> Option<String> {
    let mut out = value.to_owned();
    let mut changed = false;
    for (start, end, element) in array_items(value)?.into_iter().rev() {
        if element.as_deref() == Some(from) {
            let quoted = to.is_empty()
                || to.eq_ignore_ascii_case("NULL")
                || to
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '{' | '}' | ',' | '"' | '\\'));
            let replacement = if quoted {
                format!("\"{}\"", to.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                to.to_owned()
            };
            out.replace_range(start..end, &replacement);
            changed = true;
        }
    }
    changed.then_some(out)
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
                    let deferred = matches!(
                        module.kind,
                        pbps_model::ModuleKind::Function | pbps_model::ModuleKind::Procedure
                    ) && dialect
                        .lexicon()
                        .routine_language(&module.definition)
                        .as_deref()
                        == Some("plpgsql");
                    !deferred
                        && (text(&module.definition)
                            || dialect
                                .lexicon()
                                .string_literals(&module.definition)
                                .iter()
                                .any(|body| text(body)))
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

// Read only the two identifier parts and the array marker of a cast. Quoted
// identifiers may contain whitespace or punctuation; splitting on spaces would
// silently lose the type's uid in precisely those cases.
fn cast_type(suffix: &str) -> Option<ColumnType> {
    fn identifier(text: &str) -> Option<usize> {
        if text.starts_with('"') {
            let mut i = 1;
            while i < text.len() {
                if text.as_bytes()[i] == b'"' {
                    i += 1;
                    if text.as_bytes().get(i) == Some(&b'"') {
                        i += 1;
                    } else {
                        return Some(i);
                    }
                } else {
                    i += text[i..].chars().next()?.len_utf8();
                }
            }
            None
        } else {
            let len = text
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$' || !c.is_ascii())
                .map(char::len_utf8)
                .sum();
            (len > 0).then_some(len)
        }
    }
    let suffix = suffix.trim_start();
    let text = if let Some(text) = suffix.strip_prefix("::") {
        text.trim_start()
    } else if suffix
        .get(..2)
        .is_some_and(|word| word.eq_ignore_ascii_case("as"))
        && suffix[2..].starts_with(char::is_whitespace)
    {
        suffix[2..].trim_start()
    } else {
        return None;
    };
    let first = identifier(text)?;
    let rest = text[first..].trim_start();
    let rest = rest.strip_prefix('.')?.trim_start();
    let second = identifier(rest)?;
    let end = text.len() - rest.len() + second;
    let array = text[end..].trim_start().starts_with("[]");
    format!(
        "{}.{}{}",
        &text[..first],
        &rest[..second],
        if array { "[]" } else { "" }
    )
    .parse()
    .ok()
}

fn uncast_comparison(text: &str, dialect: &(impl Dialect + ?Sized)) -> bool {
    let code = dialect.lexicon().header(text);
    if code.contains("::") {
        return false;
    }
    for (at, _) in code.match_indices('(') {
        let before = code[..at].trim_end();
        let word: String = before
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if !word.is_empty() && !["in", "any", "all"].contains(&word.to_ascii_lowercase().as_str()) {
            return false;
        }
    }
    true
}

fn plain_literal(text: &str, dialect: &(impl Dialect + ?Sized)) -> bool {
    dialect
        .lexicon()
        .header(text)
        .chars()
        .all(|c| c.is_whitespace() || matches!(c, '(' | ')'))
}

/// Forward only expected expressions containing approved enum constants. Enum definitions are checked
/// independently against the exact executed statement's intermediate payload.
pub fn forward_schema(
    schema: &mut Schema,
    ids: &IdsFile,
    changes: &ChangeSet,
    dialect: &(impl Dialect + ?Sized),
    catalog: bool,
) {
    for step in &changes.changes {
        let Change::RenameEnumValue { uid, from, to, .. } = &step.change else {
            continue;
        };
        let world = schema.clone();
        let typed = |ty: &ColumnType| uses_enum(ty, uid, &world, ids);
        let shape = |ty: &ColumnType| enum_shape(ty, uid, &world, ids);
        let rewrite = |text: &str, contextual: Option<bool>| {
            dialect
                .lexicon()
                .rewrite_string_literals(text, |value, suffix| {
                    let bound = cast_type(suffix).and_then(|ty| shape(&ty));
                    let uncast = !suffix.trim_start().starts_with("::")
                        && !suffix.trim_start().to_ascii_lowercase().starts_with("as ");
                    let shape =
                        bound.or_else(|| (!catalog && uncast).then_some(contextual).flatten())?;
                    if shape {
                        rename_array(value, from, to)
                    } else {
                        (value == from).then(|| to.clone())
                    }
                })
        };
        for table in schema.tables.values_mut() {
            for column in table.columns.values_mut() {
                let contextual = shape(&column.ty);
                if let ValueSource::Default(text) = &mut column.value_source {
                    *text = rewrite(text, contextual.filter(|_| plain_literal(text, dialect)));
                }
                if let ValueSource::Generated(g) = &mut column.value_source {
                    g.expression = rewrite(&g.expression, None);
                }
            }
            for check in table.checks.values_mut() {
                let mentioned: Vec<_> = table
                    .columns
                    .iter()
                    .filter(|(n, _)| dialect.may_name(&check.expression, n.raw()))
                    .collect();
                let contextual = !mentioned.is_empty()
                    && mentioned.iter().all(|(_, c)| typed(&c.ty))
                    && uncast_comparison(&check.expression, dialect);
                check.expression = rewrite(
                    &check.expression,
                    contextual.then(|| mentioned.iter().any(|(_, c)| shape(&c.ty) == Some(true))),
                );
            }
            for index in table.indexes.values_mut() {
                if let Some(text) = &mut index.filter {
                    *text = rewrite(text, None);
                }
                for column in &mut index.columns {
                    if let pbps_model::IndexKey::Expression(text) = &mut column.key {
                        *text = rewrite(text, None);
                    }
                }
            }
            for policy in table.row_policies.values_mut() {
                for text in [&mut policy.using, &mut policy.with_check]
                    .into_iter()
                    .flatten()
                {
                    *text = rewrite(text.raw(), None).into();
                }
            }
            if let Some(of) = &mut table.partition_of
                && let Some(parent) = world.tables.get(&of.parent)
            {
                for (column, own) in &mut of.columns {
                    if let Some(spec) = parent.columns.get(column)
                        && let Some(text) = &mut own.default
                    {
                        *text = rewrite(text, shape(&spec.ty));
                    }
                }
            }
        }
        for definition in schema.types.values_mut() {
            if let ManagedType::Domain {
                base,
                default,
                checks,
                ..
            } = definition
            {
                let contextual = shape(base);
                if let Some(text) = default {
                    *text = rewrite(text, contextual.filter(|_| plain_literal(text, dialect)));
                }
                for text in checks.values_mut() {
                    *text = rewrite(
                        text,
                        contextual.filter(|_| uncast_comparison(text, dialect)),
                    );
                }
            }
        }
        for module in schema.modules.values_mut() {
            module.definition = rewrite(&module.definition, None);
        }
    }
}

/// Advance only provenance that already exists; adopting a catalog expression
/// must not manufacture an authored declaration record.
pub fn forward_declared(
    record: &mut pbps_model::Declared,
    state: &Schema,
    ids: &IdsFile,
    changes: &ChangeSet,
    dialect: &(impl Dialect + ?Sized),
) {
    let mut schema = record.overlay(state);
    forward_schema(&mut schema, ids, changes, dialect, false);
    let forwarded = pbps_model::Declared::from_schema(&schema);
    macro_rules! expressions {
        ($($field:ident),*) => { $(
            for (table, values) in &mut record.expressions.$field {
                for (name, value) in values {
                    if let Some(now) = forwarded.expressions.$field.get(table).and_then(|values| values.get(name)) { *value = now.clone(); }
                }
            }
        )* };
    }
    expressions!(
        defaults, generated, computed, checks, filters, keys, policies
    );
    for (name, value) in &mut record.types {
        if let Some(now) = forwarded.types.get(name) {
            *value = now.clone();
        }
    }
    for (name, value) in &mut record.modules {
        if let Some(now) = forwarded.modules.get(name) {
            *value = now.clone();
        }
    }
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

        // Reused b is already the first label's final identity. Raw hints must
        // not forward its constants to c when the computed plan only adds d.
        let (mut schema, ids) = world(&["b", "c"]);
        schema.tables.insert(
            "app.t".parse().unwrap(),
            Table {
                columns: [(
                    "m".into(),
                    Column {
                        value_source: ValueSource::Default("'b'::app.e".into()),
                        ..Column::new("app.e".parse().unwrap())
                    },
                )]
                .into(),
                checks: [(
                    "kept".into(),
                    pbps_model::CheckConstraint {
                        expression: "m <> 'b'::app.e".into(),
                    },
                )]
                .into(),
                ..Default::default()
            },
        );
        let before = schema.clone();
        forward_schema(
            &mut schema,
            &ids,
            &steps,
            &pbps_dialect::MinimalDialect,
            true,
        );
        assert_eq!(schema, before);
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
    fn comparison_forwards_bound_constants_and_typed_authored_expressions_without_hiding_other_literals()
     {
        let (steps, errors, mut schema, ids) =
            planned(&["sad", "ok"], &["blue", "ok"], &[("blue", "sad")]);
        assert!(errors.is_empty());
        schema.tables.insert(
            "app.t".parse().unwrap(),
            Table {
                columns: [
                    (
                        "m".into(),
                        Column {
                            value_source: ValueSource::Default("'sad'".into()),
                            ..Column::new("app.e".parse().unwrap())
                        },
                    ),
                    ("text".into(), Column::new("text".parse().unwrap())),
                ]
                .into(),
                checks: [
                    (
                        "typed".into(),
                        pbps_model::CheckConstraint {
                            expression: "m <> 'sad'".into(),
                        },
                    ),
                    (
                        "text".into(),
                        pbps_model::CheckConstraint {
                            expression: "text <> 'sad'".into(),
                        },
                    ),
                ]
                .into(),
                ..Default::default()
            },
        );
        let original = schema.clone();
        forward_schema(
            &mut schema,
            &ids,
            &steps,
            &pbps_dialect::MinimalDialect,
            false,
        );
        let table = &schema.tables[&"app.t".parse().unwrap()];
        assert_eq!(
            table.columns["m"].value_source,
            ValueSource::Default("'blue'".into())
        );
        assert_eq!(table.checks["typed"].expression, "m <> 'blue'");
        assert_eq!(table.checks["text"].expression, "text <> 'sad'");
        let mut catalog = original.clone();
        catalog
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .checks
            .get_mut("typed")
            .unwrap()
            .expression = "m <> 'sad'::app.e AND text <> 'sad'::text /* 'sad' */".into();
        forward_schema(
            &mut catalog,
            &ids,
            &steps,
            &pbps_dialect::MinimalDialect,
            true,
        );
        assert_eq!(
            catalog.tables[&"app.t".parse().unwrap()].checks["typed"].expression,
            "m <> 'blue'::app.e AND text <> 'sad'::text /* 'sad' */"
        );
        let mut record = pbps_model::Declared::from_schema(&original);
        forward_declared(
            &mut record,
            &original,
            &ids,
            &steps,
            &pbps_dialect::MinimalDialect,
        );
        assert_eq!(
            record.expressions.checks[&"app.t".parse().unwrap()]["typed"],
            "m <> 'blue'"
        );
        let mut adopted = pbps_model::Declared::default();
        forward_declared(
            &mut adopted,
            &schema,
            &ids,
            &steps,
            &pbps_dialect::MinimalDialect,
        );
        assert_eq!(adopted, pbps_model::Declared::default());
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
        let (renames, errors, _, _) = planned(&["old"], &["renamed"], &[("renamed", "old")]);
        assert!(errors.is_empty());
        schema
            .tables
            .get_mut(&child)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns
            .insert(
                "m".into(),
                pbps_model::PartitionColumn {
                    default: Some("'old'::app.e".into()),
                    not_null: false,
                },
            );
        forward_schema(
            &mut schema,
            &ids,
            &renames,
            &pbps_dialect::MinimalDialect,
            true,
        );
        assert_eq!(
            schema.tables[&child].partition_of.as_ref().unwrap().columns["m"]
                .default
                .as_deref(),
            Some("'renamed'::app.e")
        );
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

    #[test]
    fn enum_array_defaults_and_checks_follow_oid_renames_without_rewriting_text_constants() {
        let (steps, errors, mut schema, ids) =
            planned(&["sad", "ok"], &["blue", "ok"], &[("blue", "sad")]);
        assert!(errors.is_empty());
        schema.tables.insert(
            "app.t".parse().unwrap(),
            Table {
                columns: [
                    (
                        "a".into(),
                        Column {
                            value_source: ValueSource::Default(
                                "'[0:1]={sad,NULL}'::app.e[]".into(),
                            ),
                            ..Column::new("app.e[]".parse().unwrap())
                        },
                    ),
                    ("text".into(), Column::new("text".parse().unwrap())),
                ]
                .into(),
                checks: [(
                    "kept".into(),
                    pbps_model::CheckConstraint {
                        expression: "a <> '{sad,ok}'::app.e[] AND text <> 'sad'::text".into(),
                    },
                )]
                .into(),
                ..Default::default()
            },
        );
        forward_schema(
            &mut schema,
            &ids,
            &steps,
            &pbps_dialect::MinimalDialect,
            true,
        );
        let table = &schema.tables[&"app.t".parse().unwrap()];
        assert_eq!(
            table.columns["a"].value_source,
            ValueSource::Default("'[0:1]={blue,NULL}'::app.e[]".into())
        );
        assert_eq!(
            table.checks["kept"].expression,
            "a <> '{blue,ok}'::app.e[] AND text <> 'sad'::text"
        );
    }
}

#[cfg(test)]
mod array_tests {
    use super::*;
    #[test]
    fn array_labels_preserve_dimensions_nulls_escaping_and_unrelated_elements() {
        assert_eq!(
            rename_array("[0:2]={sad,NULL,ok}", "sad", "blue"),
            Some("[0:2]={blue,NULL,ok}".into())
        );
        assert_eq!(
            rename_array("{{sad,ok},{sad,sad}}", "sad", "NULL"),
            Some("{{\"NULL\",ok},{\"NULL\",\"NULL\"}}".into())
        );
        assert_eq!(
            rename_array(r#"{"a,b","a\"b","a\\b"}"#, "a,b", "x,y"),
            Some(r#"{"x,y","a\"b","a\\b"}"#.into())
        );
        assert!(literal_uses(r#"{"a,b"}"#, "a,b"));
        assert!(!literal_uses("{NULL}", "NULL"));
        assert!(!literal_uses("{sadness}", "sad"));
        assert_eq!(rename_array("{ok}", "sad", "blue"), None);
        assert_eq!(rename_array("not an array", "sad", "blue"), None);
    }

    #[test]
    fn cast_names_keep_quoted_whitespace_and_do_not_accept_unqualified_or_broken_names() {
        assert_eq!(
            cast_type("::\"weird schema\".\"e.x\"[] AND true").unwrap(),
            "\"weird schema\".\"e.x\"[]".parse::<ColumnType>().unwrap()
        );
        assert!(cast_type("::text").is_none());
        assert!(cast_type("::\"unfinished").is_none());
        assert!(cast_type(" AS text").is_none());
        assert_eq!(
            cast_type(" AS app.e)").unwrap(),
            "app.e".parse::<ColumnType>().unwrap()
        );
        assert!(cast_type(" ASide app.e").is_none());
    }
}
