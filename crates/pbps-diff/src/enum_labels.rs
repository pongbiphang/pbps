//! Executable enum labels and type-based plan probe deferral (DEC-1635.6).
use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::Dialect;
use pbps_model::{
    Cell, Change, ChangeSet, ColumnName, ColumnType, EnumPlacement, EnumRebuildStep, Hints,
    IdsFile, ManagedType, Schema, TableName, TypeRef, Uid, Value, ValueSource,
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
        // A label absent from the declaration, or retained labels in another
        // order, is not an `ALTER TYPE`: PostgreSQL has neither. The type is
        // rebuilt under its uid, mapping every label by its final name.
        // Spelled as the database has them now: a renamed label is never
        // removed, and the rows are counted before the plan's renames run.
        let removed: Vec<String> = current
            .iter()
            .zip(old)
            .filter(|(v, _)| !wanted.contains(*v))
            .map(|(_, stored)| stored.clone())
            .collect();
        let kept: Vec<_> = current.iter().filter(|v| wanted.contains(*v)).collect();
        let retained: Vec<_> = wanted.iter().filter(|v| current.contains(v)).collect();
        if !removed.is_empty() || kept != retained {
            match rebuild_type(
                base,
                desired,
                hints,
                (uid, name),
                Replacement {
                    definition: ManagedType::Enum {
                        values: wanted.clone(),
                    },
                    removed,
                    over: None,
                    why: None,
                    conversion: pbps_model::RetypeConversion::Text,
                    base_change: None,
                    ordinal,
                    depth: 0,
                },
            ) {
                Ok(steps) => out.extend(steps),
                Err(why) => errors.push(refusal(name, why)),
            }
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

/// The table that holds a partition's columns: its parent, however deep.
fn root_of<'a>(schema: &'a Schema, table: &'a pbps_model::Table) -> &'a pbps_model::Table {
    let mut owner = table;
    let mut seen = BTreeSet::new();
    while let Some(of) = &owner.partition_of {
        if !seen.insert(&of.parent) {
            break;
        }
        let Some(parent) = schema.tables.get(&of.parent) else {
            break;
        };
        owner = parent;
    }
    owner
}

/// A name no declaration or live type of the project holds. The temporary
/// name is derived from the uid, so a resumed plan finds its own replacement;
/// a taken name moves on to the next suffix, and the connected census refuses
/// a live occupant the declarations do not know (DEC-1635.7).
fn replacement_name(base: Side<'_>, desired: Side<'_>, uid: &Uid, name: &TypeRef) -> TypeRef {
    let taken = |candidate: &TypeRef| {
        [base.schema, desired.schema].iter().any(|schema| {
            schema.types.contains_key(candidate)
                || schema
                    .tables
                    .keys()
                    .any(|t| t.schema == candidate.schema() && t.name == candidate.name())
                || schema.modules.keys().any(|m| {
                    matches!(m, pbps_model::ModuleId::Named(_))
                        && m.schema() == candidate.schema()
                        && m.name() == candidate.name()
                })
        })
    };
    (0..)
        .find_map(|k| {
            let text = if k == 0 {
                format!("pbps_rebuild_{uid}")
            } else {
                format!("pbps_rebuild_{uid}_{k}")
            };
            TypeRef::new(name.schema(), &text)
                .ok()
                .filter(|candidate| !taken(candidate))
        })
        .expect("an unbounded search")
}

/// What a rebuild creates and how its columns convert.
pub(crate) struct Replacement {
    /// The final declaration the replacement is created from.
    pub definition: ManagedType,
    /// The labels the rebuild removes from the enum at the root of the chain,
    /// which a row of a column of any type built on it must not hold.
    pub removed: Vec<String>,
    /// The replacement of the type this one is built on, when that is rebuilt.
    pub over: Option<TypeRef>,
    /// Why it is rebuilt, for the plan's reader.
    pub why: Option<String>,
    pub conversion: pbps_model::RetypeConversion,
    /// A domain's base before and after, for the plan-time narrowing probe.
    pub base_change: Option<(ColumnType, ColumnType)>,
    /// After the label renames of the same uid, and after the replacement of
    /// the type this one is built on.
    pub ordinal: usize,
    /// How many rebuilt types this one is built on.
    pub depth: usize,
}

/// Whether `ty` is `uid` itself or an array of it, and not a type built on it:
/// a domain over the type is rebuilt on its own account and holds its own
/// columns.
pub(crate) fn holds_directly(ty: &ColumnType, uid: &Uid, ids: &IdsFile) -> bool {
    ty.type_ref().is_some_and(|name| {
        matches!(
            TypeResolver::new(ids, ids).resolve(TypeSide::Desired, name),
            TypeIdentity::Declared(at) if at == uid
        )
    })
}

/// Rebuild an enum or a domain under its uid (DEC-1635.7, DEC-1635.9): the
/// replacement, one retype per column that holds the type, and the swap.
/// Everything that must come down for the retype is the ordinary differ's:
/// checks, foreign keys and indexes by the dialect's retype policy, defaults
/// by [`rebuild_defaults`], modules by the connected rebuild of their
/// dependents.
pub(crate) fn rebuild_type(
    base: Side<'_>,
    desired: Side<'_>,
    hints: &Hints,
    (uid, name): (&Uid, &TypeRef),
    replacement: Replacement,
) -> Result<Vec<Change>, String> {
    let resolver = TypeResolver::new(base.ids, desired.ids);
    let is_domain = matches!(replacement.definition, ManagedType::Domain { .. });
    let (kind, what) = if is_domain {
        ("domain", "this domain")
    } else {
        ("enum", "this enum")
    };
    if resolver.name(TypeSide::Base, uid) != Some(name) {
        return Err(format!(
            "the type is renamed or moved in this plan, and a rebuild under a new name is not planned (DEC-1635.9); rename it in one plan, then change its {} in the next",
            if is_domain { "definition" } else { "labels" }
        ));
    }
    let spelled = name.raw().to_string();
    for id in desired.schema.modules.keys() {
        if let pbps_model::ModuleId::Routine(routine) = id
            && routine.args.iter().any(|arg| {
                let arg = arg.raw().replace('"', "");
                arg.trim().trim_end_matches("[]").trim() == spelled.replace('"', "")
            })
        {
            return Err(format!(
                "routine {} takes this {kind} as an argument, which blocks DROP TYPE; declare it without the type first",
                id.shown()
            ));
        }
    }
    let mut columns = Vec::new();
    for (table_name, table) in &desired.schema.tables {
        for (column_name, column) in &table.columns {
            if !holds_directly(&column.ty, uid, desired.ids) {
                continue;
            }
            let reference = pbps_model::ColumnRef {
                table: table_name.clone(),
                name: column_name.clone(),
            };
            let Some(column_uid) = desired.ids.column_uid(&reference) else {
                return Err(format!("{} has no recorded identity", reference.shown()));
            };
            let standing = base
                .ids
                .columns
                .get(column_uid)
                .and_then(|at| base.schema.tables.get(&at.table)?.columns.get(&at.name))
                .is_some_and(|c| holds_directly(&c.ty, uid, base.ids));
            if !standing {
                return Err(format!(
                    "{} is created or retyped to {what} in the same plan; rebuild the {kind} first, then change the column",
                    reference.shown()
                ));
            }
            if matches!(column.value_source, ValueSource::Generated(_)) {
                return Err(format!(
                    "{} is a generated column of this {kind}, which PostgreSQL cannot retype",
                    reference.shown()
                ));
            }
            if table.partition_by.as_ref().is_some_and(|by| by.external) {
                return Err(format!(
                    "{} belongs to a table with external partitions, which a rebuild cannot reach",
                    reference.shown()
                ));
            }
            if table
                .partition_by
                .as_ref()
                .is_some_and(|by| by.columns.contains(column_name))
            {
                return Err(format!(
                    "{} is a partition key of {what}, which PostgreSQL cannot retype; rebuild the partitioned table (#2298), then change the {}",
                    reference.shown(),
                    if is_domain { "definition" } else { "labels" }
                ));
            }
            columns.push((column_uid.clone(), reference, column.ty.clone()));
        }
    }
    for (partition, table) in &desired.schema.tables {
        let Some(of) = &table.partition_of else {
            continue;
        };
        let root = root_of(desired.schema, table);
        for (column, own) in &of.columns {
            if own.default.is_some()
                && root
                    .columns
                    .get(column)
                    .is_some_and(|c| holds_directly(&c.ty, uid, desired.ids))
            {
                return Err(format!(
                    "partition {} keeps its own default for {}, which a rebuild does not restore",
                    partition.shown(),
                    column.shown()
                ));
            }
        }
    }
    let tmp = replacement_name(base, desired, uid, name);
    let Replacement {
        definition,
        removed,
        over,
        why,
        conversion,
        base_change,
        ordinal,
        depth,
    } = replacement;
    let over_swap = over.clone();
    let mut out = vec![Change::EnumRebuild {
        uid: uid.clone(),
        name: name.clone(),
        tmp: tmp.clone(),
        step: EnumRebuildStep::Create {
            definition: Box::new(definition.clone()),
            removed: removed.clone(),
            ordinal,
            over,
            why,
        },
    }];
    out.extend(columns.into_iter().map(|(column_uid, column, to)| {
        let value_arrays = resolver
            .values_shape(TypeSide::Desired, &to, desired.schema)
            .arrays;
        Change::EnumRebuild {
            uid: uid.clone(),
            name: name.clone(),
            tmp: tmp.clone(),
            step: EnumRebuildStep::Retype {
                column_uid,
                column,
                array: to.is_array(),
                to,
                removed: removed.clone(),
                conversion,
                base_change: base_change.clone().map(Box::new),
                value_arrays,
            },
        }
    }));
    out.push(Change::EnumRebuild {
        uid: uid.clone(),
        name: name.clone(),
        tmp,
        step: EnumRebuildStep::Swap {
            definition: Box::new(definition),
            comment: hints.enum_comments.get(name).cloned(),
            depth,
            over: over_swap,
        },
    });
    Ok(out)
}

/// A default written for the old type blocks the retype, one for the new type
/// follows it: the same two halves the differ makes of a replaced default on a
/// retyped column, here for the default the declaration keeps as it is. A
/// default the declaration changes in the same plan joins the pair, so the
/// old one is never standing at the retype and the new one is written after.
pub(crate) fn rebuild_defaults(base: Side<'_>, desired: Side<'_>, changes: &mut Vec<Change>) {
    let retyped: Vec<_> = changes
        .iter()
        .filter_map(|change| {
            if let Change::EnumRebuild {
                step:
                    EnumRebuildStep::Retype {
                        column_uid, column, ..
                    },
                ..
            } = change
            {
                Some((column_uid.clone(), column.clone()))
            } else {
                None
            }
        })
        .collect();
    let default = |column: Option<&pbps_model::Column>| match column.map(|c| &c.value_source) {
        Some(ValueSource::Default(text)) => Some(text.clone()),
        _ => None,
    };
    for (uid, column) in retyped {
        changes.retain(
            |change| !matches!(change, Change::AlterColumnDefault { column: c, .. } if *c == column),
        );
        let was = default(
            base.ids
                .columns
                .get(&uid)
                .and_then(|at| base.schema.tables.get(&at.table)?.columns.get(&at.name)),
        );
        let will = default(
            desired
                .schema
                .tables
                .get(&column.table)
                .and_then(|t| t.columns.get(&column.name)),
        );
        if let Some(text) = was {
            changes.push(Change::AlterColumnDefault {
                uid: uid.clone(),
                column: column.clone(),
                from: Some(text),
                to: None,
            });
        }
        if let Some(text) = will {
            changes.push(Change::AlterColumnDefault {
                uid,
                column,
                from: None,
                to: Some(text),
            });
        }
    }
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

/// The label renames a plan makes, by enum, in the order they run.
pub type LabelRenames<'a> = BTreeMap<&'a Uid, Vec<(usize, &'a str, &'a str)>>;

pub fn label_renames<'a>(changes: impl IntoIterator<Item = &'a Change>) -> LabelRenames<'a> {
    let mut out: LabelRenames<'_> = BTreeMap::new();
    for change in changes {
        if let Change::RenameEnumValue {
            uid,
            from,
            to,
            ordinal,
            ..
        } = change
        {
            out.entry(uid).or_default().push((*ordinal, from, to));
        }
    }
    out.values_mut().for_each(|pairs| pairs.sort());
    out
}

/// Which way a label is carried through a plan's renames.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Carry {
    /// A label the database has now to the label it has when the plan is done.
    Forward,
    /// A label the plan leaves to the label the database has now.
    Backward,
}

/// One label carried through the renames of its enum, in the order they run
/// forwards and in the reverse order backwards, by exact match: a chain
/// (`b→c`, then `a→b`) takes `b` to `c` and `a` to `b`.
pub fn carried_label(pairs: &[(usize, &str, &str)], label: &str, way: Carry) -> String {
    match way {
        Carry::Forward => pairs.iter().fold(label.to_owned(), |now, (_, from, to)| {
            if now == *from { (*to).to_owned() } else { now }
        }),
        Carry::Backward => pairs
            .iter()
            .rev()
            .fold(label.to_owned(), |now, (_, from, to)| {
                if now == *to { (*from).to_owned() } else { now }
            }),
    }
}

/// A standing table's observed rows as the plan's label renames leave them
/// (DEC-2346.2). The engine renames a label in the rows that hold it
/// (measured on 16 and 18), so the observed key and the observed cells of a
/// column over a renamed enum are carried through the renames, by exact label,
/// to be compared with the declared ones: a rename then changes no row.
/// `None` when no column of the table is over an enum the plan renames.
pub fn forwarded_rows(
    table: &pbps_model::Table,
    (resolver, side): (&TypeResolver<'_>, TypeSide),
    renames: &LabelRenames<'_>,
) -> Option<pbps_model::Table> {
    let data = table.data.as_ref().filter(|_| !renames.is_empty())?;
    let pairs: BTreeMap<&pbps_model::ColumnName, &Vec<(usize, &str, &str)>> = table
        .columns
        .iter()
        .filter(|(_, c)| !c.ty.is_array())
        .filter_map(|(name, c)| {
            let reference = c.ty.type_ref()?;
            match resolver.resolve(side, reference) {
                TypeIdentity::Declared(uid) => renames.get(uid).map(|pairs| (name, pairs)),
                TypeIdentity::ExternalOrUnrecorded(_) => None,
            }
        })
        .collect();
    if pairs.is_empty() {
        return None;
    }
    let key = table
        .primary_key
        .as_ref()
        .and_then(|pk| match pk.columns.as_slice() {
            [only] => Some(only),
            _ => None,
        });
    let carry = |column: &pbps_model::ColumnName, label: &str| match pairs.get(column) {
        Some(pairs) => carried_label(pairs, label, Carry::Forward),
        None => label.to_owned(),
    };
    let rows = data
        .rows
        .iter()
        .map(|(row_key, row)| {
            let row_key = match key {
                Some(column) => pbps_model::RowKey(carry(column, row_key.as_str())),
                None => row_key.clone(),
            };
            let cells = row
                .0
                .iter()
                .map(|(column, value)| match value {
                    Value::Text(label) => (column.clone(), Value::Text(carry(column, label))),
                    Value::Null | Value::Bool(_) | Value::Int(_) => (column.clone(), value.clone()),
                })
                .collect();
            (row_key, pbps_model::Row(cells))
        })
        .collect();
    let mut forwarded = table.clone();
    forwarded.data = Some(pbps_model::TableData {
        mode: data.mode,
        rows,
    });
    Some(forwarded)
}

/// The renames of the enum a table's single-column primary key is over, if
/// it is a scalar column of an enum the plan renames labels of.
fn key_renames<'a>(
    table: &pbps_model::Table,
    (resolver, side): (&TypeResolver<'_>, TypeSide),
    renames: &'a LabelRenames<'_>,
) -> Option<&'a Vec<(usize, &'a str, &'a str)>> {
    let column = table
        .primary_key
        .as_ref()
        .and_then(|pk| match pk.columns.as_slice() {
            [only] => table.columns.get(only),
            _ => None,
        })
        .filter(|c| !c.ty.is_array())?;
    match resolver.resolve(side, column.ty.type_ref()?) {
        TypeIdentity::Declared(uid) => renames.get(uid),
        TypeIdentity::ExternalOrUnrecorded(_) => None,
    }
}

/// The observed rows of a keyed `ensure` table, kept to those the plan still
/// manages once its renames have run (DEC-2346.2): the baseline is read under
/// the widened scope, and a chain (`b→c`, `a→b`) over a table that declares
/// only `b` reads the rows `a` and `b`, which the plan leaves as `b` and
/// `c`. The closing read asks for `b` alone, so `c` is not managed and must not
/// be compared as if it were. A row is kept when its key, carried forward
/// through the renames, is in the scope. The rows themselves stay as read; the
/// comparison carries them. `exact` tables read every row and are left alone,
/// as is any table whose key is not over a renamed enum.
pub fn trim_to_the_final_scope(
    schema: &mut Schema,
    scopes: &pbps_model::DataScopes,
    (resolver, side): (&TypeResolver<'_>, TypeSide),
    renames: &LabelRenames<'_>,
) {
    if renames.is_empty() {
        return;
    }
    for (name, table) in &mut schema.tables {
        let Some(scope) = scopes
            .get(name)
            .filter(|s| s.mode == pbps_model::DataMode::Ensure)
        else {
            continue;
        };
        let Some(pairs) = key_renames(table, (resolver, side), renames) else {
            continue;
        };
        if let Some(data) = &mut table.data {
            data.rows.retain(|key, _| {
                scope.keys.contains(&pbps_model::RowKey(carried_label(
                    pairs,
                    key.as_str(),
                    Carry::Forward,
                )))
            });
        }
    }
}

/// The scopes of the tables keyed by an enum, widened with the labels their
/// keys have before the plan's renames (DEC-2346.2): a declared key is a label
/// the plan leaves, and the database holds the label it has now, so a read by
/// the declared key alone would not find the row the rename carries along.
/// The declared keys stay; a chain (`b→c`, `a→b`) takes `b` back to `a`.
/// `schema` is the observed side, whose type references `side` resolves.
pub fn scopes_before_the_plan(
    scopes: &pbps_model::DataScopes,
    schema: &Schema,
    (resolver, side): (&TypeResolver<'_>, TypeSide),
    renames: &LabelRenames<'_>,
) -> pbps_model::DataScopes {
    let mut out = scopes.clone();
    if renames.is_empty() {
        return out;
    }
    for (name, scope) in &mut out {
        let Some(pairs) = schema
            .tables
            .get(name)
            .and_then(|table| key_renames(table, (resolver, side), renames))
        else {
            continue;
        };
        let before: Vec<_> = scope
            .keys
            .iter()
            .map(|key| pbps_model::RowKey(carried_label(pairs, key.as_str(), Carry::Backward)))
            .collect();
        scope.keys.extend(before);
    }
    out
}

/// The same widening for a plan's own read, which is taken before the plan
/// exists: the labels a declared key was renamed from (`renamed_from` on the
/// label) are asked as well, to the first of a chain and back (DEC-2346.2).
/// `schema` is the declarations under the names the database has now. Only a
/// read by key is widened; a read of every row finds the renamed row anyway.
pub fn read_before_the_renames(
    read: &mut BTreeMap<pbps_model::TableName, pbps_model::RowScope>,
    schema: &Schema,
    hints: &Hints,
) {
    for (name, scope) in read {
        let pbps_model::RowScope::Keys(keys) = scope else {
            continue;
        };
        let Some(origins) = schema
            .tables
            .get(name)
            .and_then(|table| table.primary_key.as_ref())
            .and_then(|pk| match pk.columns.as_slice() {
                [only] => schema.tables[name].columns.get(only),
                _ => None,
            })
            .filter(|c| !c.ty.is_array())
            .and_then(|c| hints.enum_value_renames.get(c.ty.type_ref()?))
        else {
            continue;
        };
        // `origins` is keyed by the new label.
        let mut frontier: Vec<String> = keys.iter().map(|k| k.as_str().to_owned()).collect();
        while let Some(label) = frontier.pop() {
            if let Some(from) = origins.get(&label)
                && keys.insert(pbps_model::RowKey(from.clone()))
            {
                frontier.push(from.clone());
            }
        }
    }
}

/// A partition's bound with every label of an enum key carried through the
/// plan's renames of that enum (DEC-2346.1). `renames_of` gives, for a key
/// position, the renames of the enum that column has, in the order they run;
/// a label is carried by exact match, one rename at a time in that order
/// forwards and in the reverse order backwards, so a chain (`b→c`, then
/// `a→b`) and a swap through a temporary label come out where the engine puts
/// them. `MINVALUE`, `MAXVALUE`, `NULL` and a bound with no label are kept.
pub fn carried_bound<'a>(
    bound: &pbps_model::PartitionBound,
    renames_of: impl Fn(usize) -> Option<&'a Vec<(usize, &'a str, &'a str)>>,
    way: Carry,
) -> pbps_model::PartitionBound {
    use pbps_model::{BoundDatum, ListDatum, PartitionBound};
    let carry = |at: usize, label: &String| -> String {
        match renames_of(at) {
            Some(pairs) => carried_label(pairs, label, way),
            None => label.clone(),
        }
    };
    let datum = |at: usize, datum: &BoundDatum| match datum {
        BoundDatum::Value(label) => BoundDatum::Value(carry(at, label)),
        BoundDatum::MinValue | BoundDatum::MaxValue => datum.clone(),
    };
    match bound {
        PartitionBound::Range { from, to } => PartitionBound::Range {
            from: from
                .iter()
                .enumerate()
                .map(|(at, d)| datum(at, d))
                .collect(),
            to: to.iter().enumerate().map(|(at, d)| datum(at, d)).collect(),
        },
        PartitionBound::List { values } => PartitionBound::List {
            values: values
                .iter()
                .map(|value| match value {
                    ListDatum::Value(label) => ListDatum::Value(carry(0, label)),
                    ListDatum::Null => ListDatum::Null,
                })
                .collect(),
        },
        PartitionBound::Default | PartitionBound::Hash { .. } => bound.clone(),
    }
}

/// A standing partition's bound as the plan's label renames leave it
/// (DEC-2346.1). The engine renames a label where a bound holds it (measured
/// on 16 and 18), so the observed bound is carried through the renames, in the
/// order they run and by exact label, to be compared with the declared one: a
/// rename then changes no bound. `parent` is the partition's parent on the
/// observed side, whose type references `side` resolves.
pub fn forwarded_bound(
    bound: &pbps_model::PartitionBound,
    parent: &pbps_model::Table,
    (resolver, side): (&TypeResolver<'_>, TypeSide),
    renames: &LabelRenames<'_>,
) -> pbps_model::PartitionBound {
    let Some(by) = parent.partition_by.as_ref().filter(|_| !renames.is_empty()) else {
        return bound.clone();
    };
    carried_bound(
        bound,
        |at| {
            by.columns
                .get(at)
                .and_then(|column| parent.columns.get(column))
                .filter(|column| !column.ty.is_array())
                .and_then(|column| column.ty.type_ref())
                .and_then(|reference| match resolver.resolve(side, reference) {
                    TypeIdentity::Declared(uid) => renames.get(uid),
                    TypeIdentity::ExternalOrUnrecorded(_) => None,
                })
        },
        Carry::Forward,
    )
}

/// What a plan renames in the labels of its enums, read once, for the probes
/// that evaluate a declared bound against rows nothing has renamed yet
/// (DEC-2346.1): the bound's labels are the plan's, and the rows hold the
/// database's.
pub struct PlanLabels<'a> {
    renames: LabelRenames<'a>,
    /// Each enum a label rename names, by the spelling it has once the plan's
    /// type renames have run.
    uids: BTreeMap<&'a TypeRef, &'a Uid>,
    /// The plan's type renames, in the order they run.
    moves: Vec<(&'a TypeRef, &'a TypeRef)>,
}

impl<'a> PlanLabels<'a> {
    pub fn of(changes: impl IntoIterator<Item = &'a Change> + Clone) -> Self {
        Self {
            renames: label_renames(changes.clone()),
            uids: changes
                .clone()
                .into_iter()
                .filter_map(|change| {
                    if let Change::RenameEnumValue { uid, name, .. } = change {
                        Some((name, uid))
                    } else {
                        None
                    }
                })
                .collect(),
            moves: changes
                .into_iter()
                .filter_map(|change| {
                    if let Change::RenameType { from, to, .. } = change {
                        Some((from, to))
                    } else {
                        None
                    }
                })
                .collect(),
        }
    }

    /// `bound`, a declared bound over a table keyed by columns of the types
    /// `key_types` (as the database spells them now), with each label of an
    /// enum the plan renames carried back to the label the database holds
    /// now. A chain (`b→c`, `a→b`) takes `b` back to `a`. Any other bound is
    /// returned as it is.
    pub fn before_the_plan(
        &self,
        bound: &pbps_model::PartitionBound,
        key_types: &[Option<TypeRef>],
    ) -> pbps_model::PartitionBound {
        if self.renames.is_empty() {
            return bound.clone();
        }
        carried_bound(
            bound,
            |at| {
                let now = key_types.get(at)?.as_ref()?;
                let after = self.moves.iter().fold(
                    now,
                    |at, (from, to)| {
                        if at == *from { *to } else { at }
                    },
                );
                self.renames.get(self.uids.get(after)?)
            },
            Carry::Backward,
        )
    }
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
        // A partition's bound names labels of its parent's key columns, and
        // `FOR VALUES IN ('new')` is refused in the transaction that adds the
        // label, as a default is (measured on 16 and 18, DEC-2346.1).
        let bound_uses = |table: &pbps_model::Table| {
            use pbps_model::{BoundDatum, ListDatum, PartitionBound};
            let Some(of) = &table.partition_of else {
                return false;
            };
            let Some(parent) = desired.schema.tables.get(&of.parent) else {
                return false;
            };
            let Some(by) = &parent.partition_by else {
                return false;
            };
            by.columns.iter().enumerate().any(|(at, column)| {
                parent.columns.get(column).is_some_and(|c| typed(&c.ty))
                    && match &of.bound {
                        PartitionBound::Range { from, to } => [from.get(at), to.get(at)]
                            .into_iter()
                            .flatten()
                            .any(|datum| matches!(datum, BoundDatum::Value(v) if v == value)),
                        PartitionBound::List { values } => values
                            .iter()
                            .any(|datum| matches!(datum, ListDatum::Value(v) if v == value)),
                        PartitionBound::Default | PartitionBound::Hash { .. } => false,
                    }
            })
        };
        let table_uses = |table: &pbps_model::Table| {
            let columns = inherited(table);
            let has_enum = columns.columns.values().any(|c| typed(&c.ty));
            table.columns.values().any(|c| {
                typed(&c.ty) && matches!(&c.value_source, ValueSource::Default(s) if text(s))
            }) || (has_enum && table.checks.values().any(|c| text(&c.expression)))
                || bound_uses(table)
        };
        // A row's cell, or its key, over a column of this enum: the label is
        // unusable in the transaction that adds it, as in a default (measured
        // on 16 and 18: `unsafe use of new value`, DEC-2346.2). A scalar
        // declared enum is the only enum column a row can carry.
        let column_uses = |table: &TableName, column: &ColumnName| {
            desired
                .schema
                .tables
                .get(table)
                .and_then(|t| t.columns.get(column))
                .is_some_and(|c| typed(&c.ty))
        };
        let cell_uses = |cell: &Value| matches!(cell, Value::Text(t) if t == value);
        for step in &changes.changes {
            let used = match &step.change {
                Change::InsertRow {
                    table,
                    key_column,
                    key,
                    row,
                    ..
                } => {
                    (key.as_str() == value && column_uses(table, key_column))
                        || row
                            .columns()
                            .any(|(column, v)| cell_uses(v) && column_uses(table, column))
                }
                Change::UpdateRow {
                    table,
                    key_column,
                    key,
                    columns,
                    ..
                } => {
                    (key.as_str() == value && column_uses(table, key_column))
                        || columns.iter().any(|(column, (_, after))| {
                            matches!(after, Cell::Value(v) if cell_uses(v))
                                && column_uses(table, column)
                        })
                }
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
                // A rebuilt domain's replacement is created from its final
                // declaration, as a new domain is (DEC-1635.9).
                Change::CreateType { definition, .. }
                | Change::EnumRebuild {
                    step: pbps_model::EnumRebuildStep::Create { definition, .. },
                    ..
                } => match definition.as_ref() {
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
                // Only what the half introduces: the default `Loosen` sets and
                // the checks `Tighten` adds or changes. A dropped or untouched
                // expression is no use of the new label (DEC-1635.8).
                Change::AlterDomain { from, to, half, .. } => {
                    match (to.as_ref(), pbps_model::DomainDelta::of(from, to)) {
                        (ManagedType::Domain { base, .. }, Some(delta)) => {
                            typed(base)
                                && match half {
                                    pbps_model::DomainHalf::Loosen => {
                                        delta.default.flatten().is_some_and(text)
                                    }
                                    pbps_model::DomainHalf::Tighten => {
                                        delta.add_checks.iter().any(|(_, s)| text(s))
                                    }
                                }
                        }
                        _ => false,
                    }
                }
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
    let anchors = definitions.clone();
    let mut replacements: BTreeMap<Uid, ManagedType> = BTreeMap::new();
    let mut rebuilt_domains: BTreeSet<Uid> = BTreeSet::new();
    for step in &changes.changes {
        match &step.change {
            Change::CreateType {
                uid, definition, ..
            } => {
                definitions.insert(uid.clone(), (**definition).clone());
            }
            // The statements derive from `(from, to, half)` alone, so the
            // payload is held to what the approved before-definition and the
            // sealed final definition say (DEC-1635.8): the sealed one is
            // `to`, `from` has the before-definition's NOT NULL, collation,
            // check names and default presence (its texts are the differ's
            // own, the catalog's before-definition being another spelling of
            // them), and the half has a statement to run.
            Change::AlterDomain {
                uid,
                name,
                from,
                to,
                half,
            } => {
                if !pins
                    .iter()
                    .any(|p| &p.uid == uid && &p.name == name && p.definition == **to)
                {
                    return false;
                }
                let (
                    Some(ManagedType::Domain {
                        not_null,
                        default,
                        checks,
                        collation,
                        ..
                    }),
                    ManagedType::Domain {
                        not_null: from_not_null,
                        default: from_default,
                        checks: from_checks,
                        collation: from_collation,
                        ..
                    },
                ) = (anchors.get(uid), from.as_ref())
                else {
                    return false;
                };
                if not_null != from_not_null
                    || default.is_some() != from_default.is_some()
                    || collation != from_collation
                    || !checks.keys().eq(from_checks.keys())
                {
                    return false;
                }
                let Some(delta) = pbps_model::DomainDelta::of(from, to) else {
                    return false;
                };
                let left = match half {
                    pbps_model::DomainHalf::Loosen if delta.loosens() => {
                        pbps_model::DomainDelta::loosened(from, to)
                    }
                    pbps_model::DomainHalf::Tighten if delta.tightens() => Some((**to).clone()),
                    pbps_model::DomainHalf::Loosen | pbps_model::DomainHalf::Tighten => None,
                };
                let Some(left) = left else {
                    return false;
                };
                definitions.insert(uid.clone(), left);
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
            // The replacement's labels are the old type's minus `removed`,
            // plus whatever the declaration adds; the swap installs exactly
            // them. A step out of that order cannot certify itself.
            Change::EnumRebuild {
                uid, name, step, ..
            } => {
                if !pins.iter().any(|p| &p.uid == uid && &p.name == name) {
                    return false;
                }
                match step {
                    // A domain's replacement is its sealed final definition.
                    EnumRebuildStep::Create { definition, .. }
                        if matches!(definition.as_ref(), ManagedType::Domain { .. }) =>
                    {
                        if !matches!(definitions.get(uid), Some(ManagedType::Domain { .. }))
                            || !pins
                                .iter()
                                .any(|p| &p.uid == uid && p.definition == **definition)
                            || replacements
                                .insert(uid.clone(), (**definition).clone())
                                .is_some()
                        {
                            return false;
                        }
                        rebuilt_domains.insert(uid.clone());
                    }
                    EnumRebuildStep::Create {
                        definition,
                        removed,
                        ..
                    } => {
                        let Some(ManagedType::Enum { values }) = definitions.get(uid) else {
                            return false;
                        };
                        let ManagedType::Enum { values: wanted } = definition.as_ref() else {
                            return false;
                        };
                        let gone = values.iter().filter(|v| !wanted.contains(v)).count();
                        if replacements
                            .insert(uid.clone(), (**definition).clone())
                            .is_some()
                            || gone != removed.len()
                        {
                            return false;
                        }
                    }
                    EnumRebuildStep::Retype { .. } => {
                        if !replacements.contains_key(uid) {
                            return false;
                        }
                    }
                    EnumRebuildStep::Swap { definition, .. } => {
                        if replacements.remove(uid).as_ref() != Some(definition.as_ref()) {
                            return false;
                        }
                        definitions.insert(uid.clone(), (**definition).clone());
                    }
                }
            }
            _ => {}
        }
    }
    replacements.is_empty()
        && pins.iter().all(|p| {
            !(matches!(p.definition, ManagedType::Enum { .. }) || rebuilt_domains.contains(&p.uid))
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
        ] {
            let (steps, errors, desired, ids) = planned(&["a", "b"], &wanted, &[]);
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
    fn label_intent_preserves_identity_and_refuses_missing_sources_swaps_and_two_claimants() {
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
                    options: Default::default(),
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

    /// A parent keyed by the enum column `m` and a partition `app.child` of it.
    fn keyed_by_enum(
        schema: &mut Schema,
        key: &str,
        bound: pbps_model::PartitionBound,
    ) -> (pbps_model::TableName, Table) {
        let parent: pbps_model::TableName = "app.parent".parse().unwrap();
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
                    columns: vec![key.into()],
                    external: false,
                }),
                ..Default::default()
            },
        );
        let child = Table {
            partition_of: Some(pbps_model::PartitionOf {
                parent,
                bound,
                columns: Default::default(),
            }),
            ..Default::default()
        };
        ("app.child".parse().unwrap(), child)
    }

    #[test]
    fn a_partition_bound_naming_a_label_this_plan_adds_is_refused_on_each_form_of_the_partition() {
        use pbps_model::{BoundDatum, ListDatum, PartitionBound};
        let list = |label: &str| PartitionBound::List {
            values: [ListDatum::Null, ListDatum::Value(label.into())].into(),
        };
        let range = |label: &str| PartitionBound::Range {
            from: vec![BoundDatum::MinValue],
            to: vec![BoundDatum::Value(label.into())],
        };
        for (key, bound, refused) in [
            ("m", list("new"), true),
            ("m", range("new"), true),
            // The label a standing partition already holds, and a label of
            // the enum that is not the new one: the bound is usable.
            ("m", list("old"), false),
            ("m", range("old"), false),
            // `new` on a key column of another type is no use of the label.
            ("id", list("new"), false),
        ] {
            let (labels, errors, mut schema, ids) = planned(&["old"], &["old", "new"], &[]);
            assert!(errors.is_empty());
            let (name, child) = keyed_by_enum(&mut schema, key, bound.clone());
            let creating = Change::CreateTable {
                uid: "t_000001".parse().unwrap(),
                name: name.clone(),
                table: Box::new(child.clone()),
                beside_default: None,
                moves_at_most: None,
            };
            let attaching = Change::AttachPartition {
                uid: "t_000001".parse().unwrap(),
                table: name,
                parent: "app.parent".parse().unwrap(),
                bound,
                key: vec![],
                shape: Box::new(child),
            };
            for change in [creating, attaching] {
                for with_label in [true, false] {
                    let mut steps = if with_label {
                        labels.clone()
                    } else {
                        ChangeSet::default()
                    };
                    steps.changes.push(PlannedChange::new(change.clone()));
                    let errors = unsafe_uses(
                        &steps,
                        Side {
                            schema: &schema,
                            ids: &ids,
                        },
                        &pbps_dialect::MinimalDialect,
                    );
                    // Without the label's `AddEnumValue` in the plan the same
                    // bound is nothing to refuse.
                    assert_eq!(
                        !errors.is_empty(),
                        refused && with_label,
                        "{key} {change:?} {errors:?}"
                    );
                    if refused && with_label {
                        assert!(
                            matches!(&errors[..], [DiffError::EnumValueUse { name, label, .. }] if name.raw().to_string() == "app.e" && label == "new"),
                            "{errors:?}"
                        );
                    }
                }
            }
        }
    }

    fn code(not_null: bool, default: Option<&str>, checks: &[(&str, &str)]) -> ManagedType {
        ManagedType::Domain {
            base: "text".parse().unwrap(),
            not_null,
            default: default.map(str::to_owned),
            checks: checks
                .iter()
                .map(|(name, text)| ((*name).into(), (*text).to_owned()))
                .collect(),
            collation: None,
        }
    }

    /// A saved plan's domain alterations replay against the approved
    /// before-definition and the sealed final one: the payload cannot say a
    /// different difference, and each half has a statement to run.
    #[test]
    fn a_saved_domain_alteration_replays_against_its_pins_and_cannot_certify_itself() {
        use pbps_model::DomainHalf::{Loosen, Tighten};
        let uid: Uid = "y_000002".parse().unwrap();
        let name: TypeRef = "app.d".parse().unwrap();
        let before = code(false, Some("'a'"), &[("c1", "x")]);
        let to = code(true, Some("'b'"), &[("c1", "y"), ("c2", "z")]);
        let pin = DeclaredTypePin {
            uid: uid.clone(),
            name: name.clone(),
            definition: to.clone(),
            before: Some(DeclaredTypeBefore {
                name: name.clone(),
                definition: before.clone(),
            }),
        };
        let step = |from: &ManagedType, to: &ManagedType, half| {
            PlannedChange::new(Change::AlterDomain {
                uid: uid.clone(),
                name: name.clone(),
                from: Box::new(from.clone()),
                to: Box::new(to.clone()),
                half,
            })
        };
        let plan = |steps: Vec<PlannedChange>| ChangeSet { changes: steps };
        let whole = || vec![step(&before, &to, Loosen), step(&before, &to, Tighten)];
        assert!(valid_steps(&plan(whole()), std::slice::from_ref(&pin)));
        // Either half alone replays when it has something to do.
        assert!(valid_steps(
            &plan(whole()[..1].to_vec()),
            std::slice::from_ref(&pin)
        ));
        // Negatives: each of these would make the emitter write other
        // statements than the approved definitions call for.
        let lie = code(true, Some("'b'"), &[("c1", "y"), ("c2", "other")]);
        assert!(!valid_steps(
            &plan(vec![step(&before, &lie, Loosen)]),
            std::slice::from_ref(&pin)
        ));
        let fewer = code(false, Some("'a'"), &[]);
        assert!(!valid_steps(
            &plan(vec![step(&fewer, &to, Loosen), step(&fewer, &to, Tighten)]),
            std::slice::from_ref(&pin)
        ));
        // NOT NULL, the default's presence, or the collation said otherwise
        // than the before-definition has them.
        for other in [
            code(true, Some("'a'"), &[("c1", "x")]),
            code(false, None, &[("c1", "x")]),
        ] {
            assert!(!valid_steps(
                &plan(vec![step(&other, &to, Loosen)]),
                std::slice::from_ref(&pin)
            ));
        }
        // The texts of the checks are the differ's own spelling of the
        // before-definition's, and are not compared with them.
        let respelled = code(false, Some("'a'"), &[("c1", "(x)")]);
        assert!(valid_steps(
            &plan(vec![step(&respelled, &to, Loosen)]),
            std::slice::from_ref(&pin)
        ));
        // A half with nothing to do: this declaration adds nothing.
        let narrower = DeclaredTypePin {
            definition: code(false, None, &[]),
            ..pin.clone()
        };
        let nothing = code(false, None, &[]);
        assert!(!valid_steps(
            &plan(vec![step(&before, &nothing, Tighten)]),
            std::slice::from_ref(&narrower)
        ));
        // No pin, another name, no before-definition, or an enum: refused.
        assert!(!valid_steps(&plan(whole()), &[]));
        let other_name = DeclaredTypePin {
            name: "app.other".parse().unwrap(),
            ..pin.clone()
        };
        assert!(!valid_steps(&plan(whole()), &[other_name]));
        let created = DeclaredTypePin {
            before: None,
            ..pin.clone()
        };
        assert!(!valid_steps(&plan(whole()), &[created]));
        let enum_pin = DeclaredTypePin {
            definition: enumeration(&["a"]),
            before: Some(DeclaredTypeBefore {
                name: "app.d".parse().unwrap(),
                definition: enumeration(&["a"]),
            }),
            ..pin
        };
        assert!(!valid_steps(&plan(whole()), &[enum_pin]));
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

    /// Slice 3a's refusal reaches an in-place domain alteration, for what each
    /// half introduces and nothing else (DEC-1635.8).
    #[test]
    fn an_altered_domain_refuses_a_new_label_in_what_its_halves_introduce() {
        use pbps_model::{DomainDelta, DomainHalf};

        let domain =
            |base: &str, default: Option<&str>, checks: &[(&str, &str)]| ManagedType::Domain {
                base: base.parse().unwrap(),
                not_null: false,
                default: default.map(str::to_owned),
                checks: checks
                    .iter()
                    .map(|(n, t)| ((*n).into(), (*t).to_owned()))
                    .collect(),
                collation: None,
            };
        let none = domain("app.e", None, &[]);
        let with_default = |v: &str| domain("app.e", Some(&format!("'{v}'::app.e")), &[]);
        let with_check =
            |v: &str| domain("app.e", None, &[("c", &format!("VALUE <> '{v}'::app.e"))]);
        // (from, to, refused when it names the new label)
        let cases: [(ManagedType, ManagedType, bool); 6] = [
            // A default set, a check added, a check changed.
            (none.clone(), with_default("new"), true),
            (none.clone(), with_check("new"), true),
            (with_check("old"), with_check("new"), true),
            // A check dropped, one left alone, a default dropped.
            (with_check("new"), none.clone(), false),
            (
                with_check("new"),
                domain(
                    "app.e",
                    None,
                    &[("c", "VALUE <> 'new'::app.e"), ("d", "true")],
                ),
                false,
            ),
            (with_default("new"), none.clone(), false),
        ];
        for (n, (from, to, refused)) in cases.iter().enumerate() {
            for half in [DomainHalf::Loosen, DomainHalf::Tighten] {
                let delta = DomainDelta::of(from, to).unwrap();
                let has = match half {
                    DomainHalf::Loosen => delta.loosens(),
                    DomainHalf::Tighten => delta.tightens(),
                };
                if !has {
                    continue;
                }
                let (mut steps, errors, schema, ids) = planned(&["old"], &["old", "new"], &[]);
                assert!(errors.is_empty());
                steps.changes.push(PlannedChange::new(Change::AlterDomain {
                    uid: "y_000002".parse().unwrap(),
                    name: "app.d".parse().unwrap(),
                    from: Box::new(from.clone()),
                    to: Box::new(to.clone()),
                    half,
                }));
                let errors = unsafe_uses(
                    &steps,
                    Side {
                        schema: &schema,
                        ids: &ids,
                    },
                    &pbps_dialect::MinimalDialect,
                );
                // The default is `Loosen`'s, a check `Tighten`'s.
                let introduces = *refused
                    && match half {
                        DomainHalf::Loosen => delta.default.flatten().is_some(),
                        DomainHalf::Tighten => !delta.add_checks.is_empty(),
                    };
                assert_eq!(
                    !errors.is_empty(),
                    introduces,
                    "case {n} {half:?}: {errors:?}"
                );
            }
        }
        // The same edits with an existing label.
        for to in [with_default("old"), with_check("old")] {
            for half in [DomainHalf::Loosen, DomainHalf::Tighten] {
                let (mut steps, _, schema, ids) = planned(&["old"], &["old", "new"], &[]);
                steps.changes.push(PlannedChange::new(Change::AlterDomain {
                    uid: "y_000002".parse().unwrap(),
                    name: "app.d".parse().unwrap(),
                    from: Box::new(none.clone()),
                    to: Box::new(to.clone()),
                    half,
                }));
                let errors = unsafe_uses(
                    &steps,
                    Side {
                        schema: &schema,
                        ids: &ids,
                    },
                    &pbps_dialect::MinimalDialect,
                );
                assert!(errors.is_empty(), "{half:?}: {errors:?}");
            }
        }
        // A domain over a different enum does not use this enum's new label.
        let (mut steps, _, schema, ids) = planned(&["old"], &["old", "new"], &[]);
        steps.changes.push(PlannedChange::new(Change::AlterDomain {
            uid: "y_000002".parse().unwrap(),
            name: "app.d".parse().unwrap(),
            from: Box::new(domain("app.other", None, &[])),
            to: Box::new(domain("app.other", Some("'new'::app.other"), &[])),
            half: DomainHalf::Loosen,
        }));
        let errors = unsafe_uses(
            &steps,
            Side {
                schema: &schema,
                ids: &ids,
            },
            &pbps_dialect::MinimalDialect,
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// `app.t(m app.e PRIMARY KEY, next app.e, note text)`: the shape of a
    /// lookup table whose rows are over an enum (DEC-2346.2).
    fn lookup(schema: &mut Schema) {
        let mut table = Table::default();
        for (name, ty) in [("m", "app.e"), ("next", "app.e"), ("note", "text")] {
            table
                .columns
                .insert(name.into(), Column::new(ty.parse().unwrap()));
        }
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["m".into()],
            storage_parameters: Default::default(),
        });
        schema.tables.insert("app.t".parse().unwrap(), table);
    }

    fn row(cells: &[(&str, &str)]) -> pbps_model::Row {
        pbps_model::Row(
            cells
                .iter()
                .map(|(c, v)| ((*c).into(), Value::Text((*v).into())))
                .collect(),
        )
    }

    fn rename(from: &str, to: &str, ordinal: usize) -> Change {
        Change::RenameEnumValue {
            uid: "y_000001".parse().unwrap(),
            name: "app.e".parse().unwrap(),
            from: from.into(),
            to: to.into(),
            ordinal,
            definition: Box::new(enumeration(&["a"])),
        }
    }

    /// DEC-2346.2: a row's key or cell over the enum is a use of the label,
    /// and a cell of another column, or one the update leaves, is not.
    #[test]
    fn a_row_using_a_label_added_in_the_same_plan_is_refused() {
        let insert = |key: &str, cells: &[(&str, &str)]| Change::InsertRow {
            table: "app.t".parse().unwrap(),
            key_column: "m".into(),
            identity_key: false,
            key: key.into(),
            row: row(cells),
            defaults: Default::default(),
            types: Default::default(),
        };
        let update =
            |key: &str, changed: &[(&str, &str, &str)], held: &[(&str, &str)]| Change::UpdateRow {
                table: "app.t".parse().unwrap(),
                key_column: "m".into(),
                key: key.into(),
                columns: changed
                    .iter()
                    .map(|(c, before, after)| {
                        (
                            (*c).into(),
                            (
                                Cell::Value(Value::Text((*before).into())),
                                Cell::Value(Value::Text((*after).into())),
                            ),
                        )
                    })
                    .collect(),
                unchanged: held
                    .iter()
                    .map(|(c, v)| ((*c).into(), Cell::Value(Value::Text((*v).into()))))
                    .collect(),
                types: Default::default(),
                after_types: Default::default(),
                key_type: None,
            };
        let cases: Vec<(&str, Change, bool)> = vec![
            ("a new label as a key", insert("new", &[]), true),
            (
                "a new label in an enum cell",
                insert("old", &[("next", "new")]),
                true,
            ),
            (
                "an existing label",
                insert("old", &[("next", "old")]),
                false,
            ),
            (
                "the same text in a text column",
                insert("old", &[("note", "new")]),
                false,
            ),
            (
                "an update to a new label",
                update("old", &[("next", "old", "new")], &[]),
                true,
            ),
            (
                "an update keyed by a new label",
                update("new", &[("note", "a", "b")], &[]),
                true,
            ),
            (
                "a cell the update leaves",
                update("old", &[("note", "a", "b")], &[("next", "new")]),
                false,
            ),
            (
                "an update to an existing label",
                update("old", &[("next", "new", "old")], &[]),
                false,
            ),
        ];
        for (what, change, refused) in cases {
            let (mut steps, errors, mut schema, ids) = planned(&["old"], &["old", "new"], &[]);
            assert!(errors.is_empty());
            lookup(&mut schema);
            steps.changes.push(PlannedChange::new(change));
            let errors = unsafe_uses(
                &steps,
                Side {
                    schema: &schema,
                    ids: &ids,
                },
                &pbps_dialect::MinimalDialect,
            );
            assert_eq!(!errors.is_empty(), refused, "{what}: {errors:?}");
        }
        // The label of another enum is no use of this one's.
        let (mut steps, _, mut schema, ids) = planned(&["old"], &["old", "new"], &[]);
        lookup(&mut schema);
        schema
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns["next"]
            .ty = "app.other".parse().unwrap();
        steps
            .changes
            .push(PlannedChange::new(insert("old", &[("next", "new")])));
        let errors = unsafe_uses(
            &steps,
            Side {
                schema: &schema,
                ids: &ids,
            },
            &pbps_dialect::MinimalDialect,
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// DEC-2346.2: the observed rows are carried through the plan's renames,
    /// the key and the cells of the enum's columns, by exact label.
    #[test]
    fn observed_rows_are_carried_through_the_plans_label_renames() {
        let (mut schema, ids) = world(&["a", "b", "c"]);
        lookup(&mut schema);
        let table = schema.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
        table.data = Some(pbps_model::TableData {
            mode: pbps_model::DataMode::Exact,
            rows: [
                ("a".into(), row(&[("next", "b"), ("note", "a")])),
                ("b".into(), row(&[("next", "c")])),
                ("z".into(), row(&[])),
            ]
            .into(),
        });
        let resolver = TypeResolver::new(&ids, &ids);
        let forwarded = |changes: &[Change]| {
            forwarded_rows(
                &schema.tables[&"app.t".parse::<pbps_model::TableName>().unwrap()],
                (&resolver, TypeSide::Base),
                &label_renames(changes),
            )
        };
        // A chain: `b` becomes `c`, then `a` becomes `b`.
        let chain = forwarded(&[rename("b", "c", 0), rename("a", "b", 1)]).unwrap();
        let rows = chain.data.unwrap().rows;
        assert_eq!(
            rows.keys().map(|k| k.as_str()).collect::<Vec<_>>(),
            ["b", "c", "z"]
        );
        // `a`'s row is now `b`, and its cell `b` is now `c`; the text column
        // holding `a` is not a label and stays.
        assert_eq!(rows[&"b".into()], row(&[("next", "c"), ("note", "a")]));
        assert_eq!(rows[&"c".into()], row(&[("next", "c")]));
        // A swap through a temporary label comes out where the engine puts it.
        let swap = forwarded(&[
            rename("a", "t", 0),
            rename("b", "a", 1),
            rename("t", "b", 2),
        ])
        .unwrap();
        let rows = swap.data.unwrap().rows;
        assert_eq!(rows[&"b".into()], row(&[("next", "a"), ("note", "a")]));
        // Negatives: no rename, another enum's rename, and a table of no enum
        // leave the rows alone.
        assert!(forwarded(&[]).is_none());
        let mut other = rename("a", "b", 0);
        if let Change::RenameEnumValue { uid, .. } = &mut other {
            *uid = "y_000009".parse().unwrap();
        }
        assert!(forwarded(&[other]).is_none());
        let mut plain = schema.tables[&"app.t".parse::<pbps_model::TableName>().unwrap()].clone();
        for column in plain.columns.values_mut() {
            column.ty = "text".parse().unwrap();
        }
        assert!(
            forwarded_rows(
                &plain,
                (&resolver, TypeSide::Base),
                &label_renames(&[rename("a", "b", 0)])
            )
            .is_none()
        );
    }

    /// DEC-2346.2: the scope of an enum-keyed table also names the labels its
    /// keys have before the plan, so the rows a rename carries are read.
    #[test]
    fn a_scope_names_the_labels_its_keys_have_before_the_plan() {
        let (mut schema, ids) = world(&["a", "b", "c"]);
        lookup(&mut schema);
        let name: pbps_model::TableName = "app.t".parse().unwrap();
        let scopes: pbps_model::DataScopes = [(
            name.clone(),
            pbps_model::DataScope {
                mode: pbps_model::DataMode::Ensure,
                keys: ["b".into(), "c".into()].into(),
            },
        )]
        .into();
        let resolver = TypeResolver::new(&ids, &ids);
        let keys = |changes: &[Change]| -> Vec<String> {
            scopes_before_the_plan(
                &scopes,
                &schema,
                (&resolver, TypeSide::Base),
                &label_renames(changes),
            )[&name]
                .keys
                .iter()
                .map(|k| k.as_str().to_owned())
                .collect()
        };
        // `b→c`, `a→b`: the declared `b` is held under `a`, `c` under `b`.
        assert_eq!(
            keys(&[rename("b", "c", 0), rename("a", "b", 1)]),
            ["a", "b", "c"]
        );
        assert_eq!(keys(&[rename("a", "b", 0)]), ["a", "b", "c"]);
        // No rename: the keys are as declared.
        assert_eq!(keys(&[]), ["b", "c"]);

        // The plan's own read, before any plan exists, asks from the hints.
        let mut read: BTreeMap<pbps_model::TableName, pbps_model::RowScope> = [(
            name.clone(),
            pbps_model::RowScope::Keys(["b".into(), "c".into()].into()),
        )]
        .into();
        let hints = Hints {
            enum_value_renames: [(
                "app.e".parse().unwrap(),
                [
                    ("b".to_owned(), "a".to_owned()),
                    ("c".to_owned(), "b".to_owned()),
                ]
                .into(),
            )]
            .into(),
            ..Default::default()
        };
        read_before_the_renames(&mut read, &schema, &hints);
        assert_eq!(
            read[&name]
                .known()
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        // Negatives: a read of every row finds the renamed row anyway, and no
        // hint leaves the read alone.
        let mut every: BTreeMap<_, _> = [(
            name.clone(),
            pbps_model::RowScope::Every {
                known: ["c".into()].into(),
            },
        )]
        .into();
        read_before_the_renames(&mut every, &schema, &hints);
        assert_eq!(every[&name].known().len(), 1);
        let mut none: BTreeMap<_, _> = [(
            name.clone(),
            pbps_model::RowScope::Keys(["b".into()].into()),
        )]
        .into();
        read_before_the_renames(&mut none, &schema, &Hints::default());
        assert_eq!(none[&name].known().len(), 1);
    }

    /// DEC-2346.2: the baseline of an `ensure` table is read under the widened
    /// scope and then kept to the rows the plan still manages: a chain
    /// (`b→c`, `a→b`) over a table that declares only `b` reads `a` and `b`,
    /// and `b` is left as `c`, which the closing read does not ask for.
    #[test]
    fn the_baseline_is_kept_to_the_rows_the_plan_still_manages() {
        let (mut schema, ids) = world(&["a", "b", "c"]);
        lookup(&mut schema);
        let name: pbps_model::TableName = "app.t".parse().unwrap();
        let rows = |keys: &[&str]| pbps_model::TableData {
            mode: pbps_model::DataMode::Ensure,
            rows: keys.iter().map(|k| ((*k).into(), row(&[]))).collect(),
        };
        schema.tables.get_mut(&name).unwrap().data = Some(rows(&["a", "b", "z"]));
        let scopes = |mode, keys: &[&str]| -> pbps_model::DataScopes {
            [(
                name.clone(),
                pbps_model::DataScope {
                    mode,
                    keys: keys.iter().map(|k| (*k).into()).collect(),
                },
            )]
            .into()
        };
        let resolver = TypeResolver::new(&ids, &ids);
        let kept = |scope: &pbps_model::DataScopes, changes: &[Change]| -> Vec<String> {
            let mut state = schema.clone();
            trim_to_the_final_scope(
                &mut state,
                scope,
                (&resolver, TypeSide::Base),
                &label_renames(changes),
            );
            state.tables[&name]
                .data
                .as_ref()
                .unwrap()
                .rows
                .keys()
                .map(|k| k.as_str().to_owned())
                .collect()
        };
        let chain = [rename("b", "c", 0), rename("a", "b", 1)];
        // Only `b` is declared: the row `a` becomes `b` and is managed, the row
        // `b` becomes `c` and is not, and `z` is outside the widened read.
        assert_eq!(
            kept(&scopes(pbps_model::DataMode::Ensure, &["b"]), &chain),
            ["a"]
        );
        // Both declared: both rows are managed.
        assert_eq!(
            kept(&scopes(pbps_model::DataMode::Ensure, &["b", "c"]), &chain),
            ["a", "b"]
        );
        // Negatives: no rename leaves the rows; an `exact` table reads every
        // row and keeps them; a table with no scope keeps them.
        assert_eq!(
            kept(&scopes(pbps_model::DataMode::Ensure, &["b"]), &[]),
            ["a", "b", "z"]
        );
        assert_eq!(
            kept(&scopes(pbps_model::DataMode::Exact, &["b"]), &chain),
            ["a", "b", "z"]
        );
        assert_eq!(kept(&Default::default(), &chain), ["a", "b", "z"]);
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

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)] // Fixtures name only the rebuild's changes.
mod rebuild_tests {
    use super::*;
    use pbps_dialect::MinimalDialect;
    use pbps_model::{
        Column, ColumnRef, DeclaredTypeBefore, DeclaredTypePin, PlannedChange, Table, TableName,
        UidKind,
    };

    fn enumeration(values: &[&str]) -> ManagedType {
        ManagedType::Enum {
            values: values.iter().map(|v| (*v).into()).collect(),
        }
    }

    fn type_uid() -> Uid {
        "y_000001".parse().unwrap()
    }

    /// `app.t(id, m app.e, tags app.e[])` beside `app.e`, with identities.
    fn world(values: &[&str]) -> (Schema, IdsFile) {
        let mut table = Table::default();
        for (name, ty) in [("id", "integer"), ("m", "app.e"), ("tags", "app.e[]")] {
            table
                .columns
                .insert(name.into(), Column::new(ty.parse().unwrap()));
        }
        let t: TableName = "app.t".parse().unwrap();
        let mut ids = IdsFile {
            types: [(type_uid(), "app.e".parse().unwrap())].into(),
            tables: [(Uid::derived(UidKind::Table, "t", 0), t.clone())].into(),
            ..Default::default()
        };
        for column in ["id", "m", "tags"] {
            ids.columns.insert(
                Uid::derived(UidKind::Column, column, 0),
                ColumnRef {
                    table: t.clone(),
                    name: column.into(),
                },
            );
        }
        (
            Schema {
                types: [("app.e".parse().unwrap(), enumeration(values))].into(),
                tables: [(t, table)].into(),
                ..Default::default()
            },
            ids,
        )
    }

    type World = (Schema, IdsFile);

    fn side<'a>(world: &'a World) -> Side<'a> {
        Side {
            schema: &world.0,
            ids: &world.1,
        }
    }

    fn rebuilds(
        old: &[&str],
        new: &[&str],
        renames: &[(&str, &str)],
    ) -> (Vec<Change>, Vec<DiffError>) {
        let (base, desired) = (world(old), world(new));
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
        changes(side(&base), side(&desired), &hints)
    }

    fn steps(changes: &[Change]) -> Vec<&EnumRebuildStep> {
        changes
            .iter()
            .filter_map(|c| match c {
                Change::EnumRebuild { step, .. } => Some(step),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn removing_a_label_rebuilds_under_the_uid_in_the_order_create_retypes_swap() {
        let (out, errors) = rebuilds(&["sad", "ok", "happy"], &["ok", "happy"], &[]);
        assert!(errors.is_empty(), "{errors:?}");
        let [create, first, second, swap] = steps(&out)[..] else {
            panic!("{out:?}");
        };
        let EnumRebuildStep::Create {
            definition,
            removed,
            ordinal,
            ..
        } = create
        else {
            panic!("{create:?}");
        };
        assert_eq!(definition.as_ref(), &enumeration(&["ok", "happy"]));
        assert_eq!((removed.as_slice(), *ordinal), (&["sad".to_owned()][..], 0));
        let columns: Vec<_> = [first, second]
            .into_iter()
            .map(|s| match s {
                EnumRebuildStep::Retype {
                    column,
                    array,
                    removed,
                    ..
                } => (column.name.raw().to_owned(), *array, removed.clone()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            columns,
            [
                ("m".to_owned(), false, vec!["sad".to_owned()]),
                ("tags".to_owned(), true, vec!["sad".to_owned()])
            ]
        );
        assert!(matches!(
            swap,
            EnumRebuildStep::Swap { comment: None, definition, .. } if **definition == enumeration(&["ok", "happy"])
        ));
        // One uid throughout, never a new one, and a name no label step uses.
        for change in &out {
            let Change::EnumRebuild { uid, name, tmp, .. } = change else {
                panic!("{change:?}");
            };
            assert_eq!(uid, &type_uid());
            assert_eq!(name.raw().to_string(), "app.e");
            assert_eq!(
                tmp.raw().to_string(),
                format!("app.pbps_rebuild_{}", type_uid())
            );
        }
    }

    #[test]
    fn reordering_retained_labels_rebuilds_with_nothing_removed_and_equal_labels_plan_nothing() {
        let (out, errors) = rebuilds(&["a", "b", "c"], &["c", "a", "b"], &[]);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(matches!(
            steps(&out)[0],
            EnumRebuildStep::Create { removed, .. } if removed.is_empty()
        ));
        assert_eq!(out.len(), 4);
        // The same order is no change at all, and an added label alone is the
        // ordinary `ADD VALUE`, not a rebuild.
        assert!(rebuilds(&["a", "b"], &["a", "b"], &[]).0.is_empty());
        let (added, errors) = rebuilds(&["a", "b"], &["a", "c", "b"], &[]);
        assert!(errors.is_empty());
        assert!(matches!(added.as_slice(), [Change::AddEnumValue { .. }]));
    }

    #[test]
    fn a_removal_with_additions_creates_them_in_the_replacement_without_add_value() {
        let (out, errors) = rebuilds(&["a", "b"], &["new", "b"], &[]);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(out.iter().all(|c| matches!(c, Change::EnumRebuild { .. })));
        assert!(matches!(
            steps(&out)[0],
            EnumRebuildStep::Create { removed, definition, .. }
                if removed == &["a".to_owned()] && **definition == enumeration(&["new", "b"])
        ));
    }

    #[test]
    fn renames_run_first_and_the_rebuild_maps_by_final_names_and_counts_stored_ones() {
        // sad -> blue is a rename; `ok` is what goes. The rows to count are
        // those the database holds today, under the old spellings.
        let (out, errors) = rebuilds(&["sad", "ok"], &["blue"], &[("blue", "sad")]);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(matches!(
            out.first(),
            Some(Change::RenameEnumValue { from, to, ordinal: 0, .. }) if from == "sad" && to == "blue"
        ));
        let EnumRebuildStep::Create {
            removed, ordinal, ..
        } = steps(&out)[0]
        else {
            panic!("{out:?}");
        };
        assert_eq!((removed.as_slice(), *ordinal), (&["ok".to_owned()][..], 1));
        // Renaming a label to another's spelling in a chain never counts as removal.
        let (chain, errors) = rebuilds(&["a", "b"], &["b", "c"], &[("b", "a"), ("c", "b")]);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            chain
                .iter()
                .all(|c| matches!(c, Change::RenameEnumValue { .. })),
            "{chain:?}"
        );
    }

    #[test]
    fn what_a_rebuild_cannot_do_is_refused_by_name_with_no_step() {
        let refuses = |edit: &dyn Fn(&mut World, &mut World), expect: &str| {
            let mut base = world(&["sad", "ok"]);
            let mut desired = world(&["ok"]);
            edit(&mut base, &mut desired);
            let (out, errors) = changes(side(&base), side(&desired), &Hints::default());
            let text = errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains("app.") && text.contains(expect),
                "{expect}: {text}"
            );
            assert!(steps(&out).is_empty(), "{expect}: {out:?}");
        };
        let t: TableName = "app.t".parse().unwrap();
        // A routine that takes the type blocks DROP TYPE.
        refuses(
            &|_, d| {
                d.0.modules.insert(
                    "app.f(app.e)".parse().unwrap(),
                    pbps_model::Module {
                        kind: pbps_model::ModuleKind::Function,
                        description: None,
                        definition: "(x app.e) RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$"
                            .into(),
                        options: Default::default(),
                    },
                );
            },
            "as an argument",
        );
        // A column the plan adds, or retypes to the enum, is not rebuilt around.
        refuses(
            &|b, _| {
                b.1.columns.retain(|_, c| c.name.raw() != "m");
            },
            "app.t.m",
        );
        refuses(
            &|b, _| {
                b.0.tables.get_mut(&t).unwrap().columns["m"].ty = "integer".parse().unwrap();
            },
            "created or retyped",
        );
        // A generated column of the type cannot be retyped by the engine.
        refuses(
            &|_, d| {
                d.0.tables.get_mut(&t).unwrap().columns["m"].value_source =
                    pbps_model::ValueSource::Generated(pbps_model::Generated {
                        expression: "'ok'::app.e".into(),
                        stored: true,
                    });
            },
            "generated",
        );
        // External partitions are not reached by a retype.
        refuses(
            &|_, d| {
                d.0.tables.get_mut(&t).unwrap().partition_by = Some(pbps_model::PartitionBy {
                    strategy: pbps_model::PartitionStrategy::List,
                    columns: vec!["id".into()],
                    external: true,
                });
            },
            "external partitions",
        );
        // A type renamed in the same plan is not also rebuilt.
        refuses(
            &|_, d| {
                let ty = d.0.types.remove(&"app.e".parse().unwrap()).unwrap();
                d.0.types.insert("app.f".parse().unwrap(), ty);
                for table in d.0.tables.values_mut() {
                    for column in table.columns.values_mut() {
                        column.ty.base = column.ty.base.replace("app.e", "app.f");
                    }
                }
                d.1.types.insert(type_uid(), "app.f".parse().unwrap());
            },
            "renamed",
        );
    }

    #[test]
    fn a_partition_keeping_its_own_default_for_a_moved_column_is_refused() {
        let mut base = world(&["sad", "ok"]);
        let mut desired = world(&["ok"]);
        for w in [&mut base, &mut desired] {
            let partition = Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: "app.t".parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Default::default()
            };
            w.0.tables.insert("app.p".parse().unwrap(), partition);
        }
        desired
            .0
            .tables
            .get_mut(&"app.p".parse().unwrap())
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns
            .insert(
                "m".into(),
                pbps_model::PartitionColumn {
                    default: Some("'ok'".into()),
                    not_null: false,
                },
            );
        let (out, errors) = changes(side(&base), side(&desired), &Hints::default());
        assert!(steps(&out).is_empty());
        assert!(
            errors[0].to_string().contains("keeps its own default"),
            "{errors:?}"
        );
        // The partition without an override is simply reached through its root.
        desired
            .0
            .tables
            .get_mut(&"app.p".parse().unwrap())
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns
            .clear();
        let (out, errors) = changes(side(&base), side(&desired), &Hints::default());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(steps(&out).len(), 4);
    }

    /// DEC-2346.1: the probes read rows nothing has renamed, so a declared
    /// bound goes back through the plan's renames, the reverse of how the
    /// observed one goes forward, by the one table the forward way reads.
    #[test]
    fn a_declared_bound_is_carried_back_through_the_label_renames_the_plan_runs() {
        use pbps_model::{BoundDatum, ListDatum, PartitionBound};
        let rename = |name: &str, ordinal: usize, from: &str, to: &str| Change::RenameEnumValue {
            uid: type_uid(),
            name: name.parse().unwrap(),
            from: from.into(),
            to: to.into(),
            ordinal,
            definition: Box::new(enumeration(&[])),
        };
        let list = |labels: &[&str]| PartitionBound::List {
            values: labels
                .iter()
                .map(|l| ListDatum::Value((*l).into()))
                .chain([ListDatum::Null])
                .collect(),
        };
        let key = |name: &str| vec![Some(name.parse::<TypeRef>().unwrap()), None];
        let back = |plan: &[Change], bound: &PartitionBound, key: &[Option<TypeRef>]| {
            PlanLabels::of(plan).before_the_plan(bound, key)
        };
        // The finding: `b` becomes `c`, then `a` becomes `b`; a bound on `b`
        // is the rows that are `a` now, and one on `c` the rows that are `b`.
        let chain = [rename("app.e", 0, "b", "c"), rename("app.e", 1, "a", "b")];
        assert_eq!(back(&chain, &list(&["b"]), &key("app.e")), list(&["a"]));
        assert_eq!(back(&chain, &list(&["c"]), &key("app.e")), list(&["b"]));
        assert_eq!(
            back(&chain, &list(&["b", "c"]), &key("app.e")),
            list(&["a", "b"])
        );
        // And forward again, by the same table: a round trip is the bound, for
        // the labels the plan leaves.
        for labels in [&["b"][..], &["c"], &["b", "c"]] {
            let declared = list(labels);
            let before = back(&chain, &declared, &key("app.e"));
            let mut renames = label_renames(&chain);
            let pairs = renames.get_mut(&type_uid()).unwrap();
            let again = carried_bound(&before, |_| Some(&*pairs), Carry::Forward);
            assert_eq!(again, declared);
        }
        // A swap through a temporary label ends where its labels end.
        let swap = [
            rename("app.e", 0, "a", "t"),
            rename("app.e", 1, "b", "a"),
            rename("app.e", 2, "t", "b"),
        ];
        assert_eq!(back(&swap, &list(&["a"]), &key("app.e")), list(&["b"]));
        assert_eq!(back(&swap, &list(&["b"]), &key("app.e")), list(&["a"]));
        // The type's own rename is followed to the spelling the label
        // renames name.
        let moved = [
            Change::RenameType {
                uid: type_uid(),
                from: "app.old".parse().unwrap(),
                to: "app.e".parse().unwrap(),
                definition: Box::new(enumeration(&[])),
            },
            rename("app.e", 0, "a", "b"),
        ];
        assert_eq!(back(&moved, &list(&["b"]), &key("app.old")), list(&["a"]));
        // A range: both ends, and its unbounded ends kept.
        let range = PartitionBound::Range {
            from: vec![BoundDatum::MinValue],
            to: vec![BoundDatum::Value("b".into())],
        };
        assert_eq!(
            back(&chain, &range, &key("app.e")),
            PartitionBound::Range {
                from: vec![BoundDatum::MinValue],
                to: vec![BoundDatum::Value("a".into())],
            }
        );
        // Negatives: no renames; another enum's; a key that is no enum, or
        // another column's type; a bound with no label.
        assert_eq!(back(&[], &list(&["b"]), &key("app.e")), list(&["b"]));
        let other = [rename("app.other", 0, "a", "b")];
        assert_eq!(back(&other, &list(&["b"]), &key("app.e")), list(&["b"]));
        assert_eq!(back(&chain, &list(&["b"]), &[None]), list(&["b"]));
        assert_eq!(back(&chain, &list(&["b"]), &[]), list(&["b"]));
        let rest = PartitionBound::Hash {
            modulus: 4,
            remainder: 1,
        };
        assert_eq!(back(&chain, &rest, &key("app.e")), rest);
        assert_eq!(
            back(&chain, &PartitionBound::Default, &key("app.e")),
            PartitionBound::Default
        );
    }

    #[test]
    fn a_label_removal_or_reorder_under_a_partition_key_is_refused_by_name() {
        for (old, new) in [
            (&["sad", "ok", "happy"][..], &["ok", "happy"][..]),
            (&["sad", "ok", "happy"], &["happy", "ok", "sad"]),
        ] {
            for key in ["m", "id"] {
                let (mut base, mut desired) = (world(old), world(new));
                for w in [&mut base, &mut desired] {
                    w.0.tables
                        .get_mut(&"app.t".parse().unwrap())
                        .unwrap()
                        .partition_by = Some(pbps_model::PartitionBy {
                        strategy: pbps_model::PartitionStrategy::List,
                        columns: vec![key.into()],
                        external: false,
                    });
                }
                let (out, errors) = changes(side(&base), side(&desired), &Hints::default());
                if key == "m" {
                    let why = errors[0].to_string();
                    assert!(steps(&out).is_empty(), "{out:?}");
                    assert!(
                        why.contains("app.e")
                            && why.contains("app.t.m")
                            && why.contains("partition key")
                            && why.contains("rebuild the partitioned table"),
                        "{why}"
                    );
                } else {
                    // The enum column is not the key: it is retyped as any is.
                    assert!(errors.is_empty(), "{errors:?}");
                    assert_eq!(steps(&out).len(), 4);
                }
            }
        }
    }

    #[test]
    fn a_standing_partitions_bound_is_carried_through_the_label_renames_in_the_order_they_run() {
        use pbps_model::{BoundDatum, ListDatum, PartitionBound};
        let uid = type_uid();
        let rename = |ordinal: usize, from: &str, to: &str| Change::RenameEnumValue {
            uid: uid.clone(),
            name: "app.e".parse().unwrap(),
            from: from.into(),
            to: to.into(),
            ordinal,
            definition: Box::new(enumeration(&[])),
        };
        let (base, _) = world(&["a", "b"]);
        let mut parent = base.tables[&"app.t".parse::<TableName>().unwrap()].clone();
        parent.partition_by = Some(pbps_model::PartitionBy {
            strategy: pbps_model::PartitionStrategy::List,
            columns: vec!["m".into()],
            external: false,
        });
        let ids = world(&["a"]).1;
        let list = |labels: &[&str]| PartitionBound::List {
            values: labels
                .iter()
                .map(|l| ListDatum::Value((*l).into()))
                .chain([ListDatum::Null])
                .collect(),
        };
        let carried = |renames: &[Change], bound: &PartitionBound| {
            forwarded_bound(
                bound,
                &parent,
                (&TypeResolver::new(&ids, &ids), TypeSide::Base),
                &label_renames(renames),
            )
        };
        // A chain runs the end first: `b` becomes `c`, then `a` becomes `b`.
        let chain = [rename(0, "b", "c"), rename(1, "a", "b")];
        assert_eq!(carried(&chain, &list(&["a"])), list(&["b"]));
        assert_eq!(carried(&chain, &list(&["b"])), list(&["c"]));
        assert_eq!(carried(&chain, &list(&["a", "b"])), list(&["b", "c"]));
        // A swap through a temporary label ends where the labels end.
        let swap = [
            rename(0, "a", "t"),
            rename(1, "b", "a"),
            rename(2, "t", "b"),
        ];
        assert_eq!(carried(&swap, &list(&["a"])), list(&["b"]));
        assert_eq!(carried(&swap, &list(&["b"])), list(&["a"]));
        // A label the plan does not rename stays; so do NULL and no renames.
        assert_eq!(carried(&chain, &list(&["z"])), list(&["z"]));
        assert_eq!(carried(&[], &list(&["a"])), list(&["a"]));
        let range = PartitionBound::Range {
            from: vec![BoundDatum::MinValue],
            to: vec![BoundDatum::Value("a".into())],
        };
        assert_eq!(
            carried(&chain, &range),
            PartitionBound::Range {
                from: vec![BoundDatum::MinValue],
                to: vec![BoundDatum::Value("b".into())],
            }
        );
        // The renames of another enum do not reach a key of this one.
        let other = Change::RenameEnumValue {
            uid: "y_000009".parse().unwrap(),
            name: "app.other".parse().unwrap(),
            from: "a".into(),
            to: "x".into(),
            ordinal: 0,
            definition: Box::new(enumeration(&[])),
        };
        assert_eq!(carried(&[other], &list(&["a"])), list(&["a"]));
        // A key that is not the enum keeps its text.
        let mut by_id = parent.clone();
        by_id.partition_by.as_mut().unwrap().columns = vec!["id".into()];
        assert_eq!(
            forwarded_bound(
                &list(&["a"]),
                &by_id,
                (&TypeResolver::new(&ids, &ids), TypeSide::Base),
                &label_renames(&chain)
            ),
            list(&["a"])
        );
    }

    #[test]
    fn the_temporary_name_moves_on_when_a_declaration_holds_it() {
        let base = world(&["sad", "ok"]);
        let mut desired = world(&["ok"]);
        let first: TypeRef = format!("app.pbps_rebuild_{}", type_uid()).parse().unwrap();
        desired.0.types.insert(first, enumeration(&["x"]));
        desired.0.tables.insert(
            format!("app.pbps_rebuild_{}_1", type_uid())
                .parse()
                .unwrap(),
            Table::default(),
        );
        let (out, errors) = changes(side(&base), side(&desired), &Hints::default());
        assert!(errors.is_empty(), "{errors:?}");
        let Change::EnumRebuild { tmp, .. } = &out[0] else {
            panic!("{out:?}");
        };
        assert_eq!(
            tmp.raw().to_string(),
            format!("app.pbps_rebuild_{}_2", type_uid())
        );
    }

    #[test]
    fn the_comment_the_catalog_gave_the_hints_rides_the_swap_only() {
        let (base, desired) = (world(&["sad", "ok"]), world(&["ok"]));
        let hints = Hints {
            enum_comments: [("app.e".parse().unwrap(), "moods".to_owned())].into(),
            ..Default::default()
        };
        let (out, _) = changes(side(&base), side(&desired), &hints);
        let comments: Vec<_> = steps(&out)
            .into_iter()
            .map(|s| matches!(s, EnumRebuildStep::Swap { comment: Some(c), .. } if c == "moods"))
            .collect();
        assert_eq!(comments, [false, false, false, true]);
    }

    #[test]
    fn declared_defaults_come_off_before_the_retype_and_go_back_after_even_when_unchanged() {
        let uid = Uid::derived(UidKind::Column, "m", 0);
        let column: ColumnRef = ColumnRef {
            table: "app.t".parse().unwrap(),
            name: "m".into(),
        };
        let retype = Change::EnumRebuild {
            uid: type_uid(),
            name: "app.e".parse().unwrap(),
            tmp: "app.tmp".parse().unwrap(),
            step: EnumRebuildStep::Retype {
                column_uid: uid.clone(),
                column: column.clone(),
                to: pbps_model::ColumnType::reference("app.e".parse().unwrap()),
                array: false,
                removed: vec![],
                base_change: None,
                conversion: Default::default(),
                value_arrays: 0,
            },
        };
        let (mut base, mut desired) = (world(&["a"]), world(&["a"]));
        for (w, text) in [(&mut base, "'a'"), (&mut desired, "'b'")] {
            w.0.tables
                .get_mut(&"app.t".parse().unwrap())
                .unwrap()
                .columns["m"]
                .value_source = pbps_model::ValueSource::Default(text.into());
        }
        // A default the declaration also edits joins the pair; the edit alone
        // would have run after the retype, with the old one standing.
        let mut plan = vec![
            retype.clone(),
            Change::AlterColumnDefault {
                uid: uid.clone(),
                column: column.clone(),
                from: Some("'a'".into()),
                to: Some("'b'".into()),
            },
        ];
        rebuild_defaults(side(&base), side(&desired), &mut plan);
        let defaults: Vec<_> = plan
            .iter()
            .filter_map(|c| match c {
                Change::AlterColumnDefault { from, to, .. } => Some((from.clone(), to.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            defaults,
            [
                (Some("'a'".to_owned()), None),
                (None, Some("'b'".to_owned()))
            ]
        );
        // No default on either side: no change, and no step, no pair.
        let (plain, mut plan) = (world(&["a"]), vec![retype]);
        rebuild_defaults(side(&plain), side(&plain), &mut plan);
        assert_eq!(plan.len(), 1);
        let mut none = Vec::new();
        rebuild_defaults(side(&plain), side(&plain), &mut none);
        assert!(none.is_empty());
    }

    fn pin(before: &[&str], after: &[&str]) -> DeclaredTypePin {
        DeclaredTypePin {
            uid: type_uid(),
            name: "app.e".parse().unwrap(),
            definition: enumeration(after),
            before: Some(DeclaredTypeBefore {
                name: "app.e".parse().unwrap(),
                definition: enumeration(before),
            }),
        }
    }

    fn sequence(out: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes: out.into_iter().map(PlannedChange::new).collect(),
        }
    }

    #[test]
    fn a_saved_rebuild_replays_against_its_pins_and_no_step_can_certify_itself() {
        let (out, errors) = rebuilds(&["sad", "ok"], &["ok", "new"], &[]);
        assert!(errors.is_empty());
        let pins = [pin(&["sad", "ok"], &["ok", "new"])];
        assert!(valid_steps(&sequence(out.clone()), &pins));
        // Out of order, missing a part, repeated, or lying about what goes.
        let (create, retype, swap) = (out[0].clone(), out[1].clone(), out[3].clone());
        for broken in [
            vec![swap.clone()],
            vec![retype.clone(), create.clone(), swap.clone()],
            vec![create.clone(), retype.clone()],
            vec![create.clone(), create.clone(), swap.clone()],
            vec![create.clone(), swap.clone(), swap.clone()],
        ] {
            assert!(!valid_steps(&sequence(broken), &pins));
        }
        let mut lie = out.clone();
        if let Change::EnumRebuild {
            step: EnumRebuildStep::Create { removed, .. },
            ..
        } = &mut lie[0]
        {
            removed.clear();
        }
        assert!(!valid_steps(&sequence(lie), &pins));
        let mut other = out.clone();
        if let Change::EnumRebuild {
            step: EnumRebuildStep::Swap { definition, .. },
            ..
        } = &mut other[3]
        {
            **definition = enumeration(&["ok"]);
        }
        assert!(!valid_steps(&sequence(other), &pins));
        // A pin for a different type certifies nothing.
        let mut elsewhere = pins.clone();
        elsewhere[0].uid = "y_000009".parse().unwrap();
        assert!(!valid_steps(&sequence(out), &elsewhere));
    }

    /// A dialect whose rebuild takes checks, foreign keys and filtered
    /// indexes down, as PostgreSQL's does.
    struct Rebuilds;
    impl pbps_dialect::Dialect for Rebuilds {
        fn name(&self) -> &'static str {
            "rebuilds"
        }
        fn enum_rebuild_dependents(&self) -> pbps_dialect::RetypeDependents {
            pbps_dialect::RetypeDependents {
                keys_and_indexes: false,
                checks: true,
                filtered_indexes: true,
                foreign_keys: true,
            }
        }
        fn quote_ident(&self, i: &str) -> Result<String, pbps_dialect::DialectError> {
            MinimalDialect.quote_ident(i)
        }
        fn intermediate_tables(
            &self,
            c: &Change,
            s: pbps_model::Strategy,
        ) -> Result<Vec<TableName>, pbps_dialect::DialectError> {
            MinimalDialect.intermediate_tables(c, s)
        }
        fn normalize_type(&self, t: &ColumnType) -> Result<ColumnType, pbps_dialect::DialectError> {
            MinimalDialect.normalize_type(t)
        }
        fn type_change_risk(&self, f: &ColumnType, t: &ColumnType) -> pbps_dialect::TypeChangeRisk {
            MinimalDialect.type_change_risk(f, t)
        }
        fn fold_ident<'a>(&self, i: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(i)
        }
        fn lexicon(&self) -> pbps_dialect::Lexicon {
            MinimalDialect.lexicon()
        }
        fn validate_table(&self, n: &TableName, t: &Table) -> Vec<pbps_dialect::DialectError> {
            MinimalDialect.validate_table(n, t)
        }
        fn transaction_framing(
            &self,
            z: &pbps_model::SessionZone,
        ) -> pbps_dialect::TransactionFraming {
            MinimalDialect.transaction_framing(z)
        }
        fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
            MinimalDialect.probe_framing()
        }
    }

    #[test]
    fn the_whole_plan_drops_and_restores_what_names_a_moved_column_and_orders_the_steps() {
        let (mut base, mut desired) = (world(&["sad", "ok"]), world(&["ok"]));
        for w in [&mut base, &mut desired] {
            let table = w.0.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
            table.columns["m"].value_source = pbps_model::ValueSource::Default("'ok'".into());
            table.checks.insert(
                "named".into(),
                pbps_model::CheckConstraint {
                    expression: "m IN ('ok')".into(),
                },
            );
            table.checks.insert(
                "other".into(),
                pbps_model::CheckConstraint {
                    expression: "id > 0".into(),
                },
            );
        }
        // A partition keeps its own check over the column it takes from the
        // root, and the root's retype reaches it.
        for w in [&mut base, &mut desired] {
            let uid = Uid::derived(UidKind::Table, "p", 0);
            let name: TableName = "app.p".parse().unwrap();
            w.1.tables.insert(uid, name.clone());
            let mut partition = Table {
                partition_of: Some(pbps_model::PartitionOf {
                    parent: "app.t".parse().unwrap(),
                    bound: pbps_model::PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Default::default()
            };
            partition.checks.insert(
                "pown".into(),
                pbps_model::CheckConstraint {
                    expression: "m IN ('ok')".into(),
                },
            );
            w.0.tables.insert(name, partition);
        }
        let plan = crate::diff(side(&base), side(&desired), &Rebuilds, &Hints::default())
            .expect("a rebuild plans");
        let rendered: Vec<String> = plan
            .changes()
            .changes
            .iter()
            .map(|p| match &p.change {
                Change::EnumRebuild { step, .. } => match step {
                    EnumRebuildStep::Create { .. } => "create".to_owned(),
                    EnumRebuildStep::Retype { column, .. } => {
                        format!("retype {}", column.name.shown())
                    }
                    EnumRebuildStep::Swap { .. } => "swap".to_owned(),
                },
                Change::DropCheck { name, .. } => format!("drop check {}", name.shown()),
                Change::AddCheck { name, .. } => format!("add check {}", name.shown()),
                Change::AlterColumnDefault { to: None, .. } => "drop default".to_owned(),
                Change::AlterColumnDefault { to: Some(_), .. } => "set default".to_owned(),
                other => format!("{other:?}"),
            })
            .collect();
        let at = |what: &str| {
            rendered
                .iter()
                .position(|r| r == what)
                .unwrap_or_else(|| panic!("{what} in {rendered:?}"))
        };
        // Everything that names a moved column comes down before the retypes,
        // the swap follows every retype, and what came down goes back after.
        assert!(at("drop check pown") < at("retype m"));
        assert!(at("retype tags") < at("add check pown"));
        assert!(at("drop check named") < at("create"));
        assert!(at("create") < at("drop default"));
        assert!(at("drop default") < at("retype m"));
        assert!(at("retype m") < at("retype tags"));
        assert!(at("retype tags") < at("swap"));
        assert!(at("swap") < at("set default"));
        assert!(at("set default") < at("add check named"));
        // The lexer cannot rule a check out by its text, so every check of a
        // table with a moved column is rebuilt (`id > 0` names no label).
        assert!(
            rendered.contains(&"drop check other".to_owned()),
            "{rendered:?}"
        );
        // A dialect that lets the engine rebuild its dependents plans none.
        let plain = crate::diff(
            side(&base),
            side(&desired),
            &MinimalDialect,
            &Hints::default(),
        )
        .expect("a rebuild plans");
        assert!(
            !plain
                .changes()
                .changes
                .iter()
                .any(|p| matches!(p.change, Change::DropCheck { .. })),
        );
        // Every step asks for the risk class, so the plan is gated.
        assert!(
            plan.changes()
                .risks()
                .contains(&pbps_model::RiskClass::TypeRebuild)
        );
    }

    /// A view the same plan redefines, over a moved column, comes down before
    /// the retypes and goes back after the swap, from its new text; one the
    /// census did not find stays where the differ puts a restatement.
    #[test]
    fn a_module_the_plan_also_changes_comes_down_before_the_retypes_and_returns_with_its_new_text()
    {
        let id: pbps_model::ModuleId = "app.v".parse().unwrap();
        let module = |text: &str| pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            description: None,
            definition: text.into(),
            options: Default::default(),
        };
        let (mut base, mut desired) = (world(&["sad", "ok"]), world(&["ok"]));
        base.0
            .modules
            .insert(id.clone(), module("SELECT m FROM app.t"));
        desired
            .0
            .modules
            .insert(id.clone(), module("SELECT m, id FROM app.t"));
        let order = |hints: &Hints| -> Vec<String> {
            crate::diff(side(&base), side(&desired), &Rebuilds, hints)
                .expect("plans")
                .changes()
                .changes
                .iter()
                .filter_map(|p| match &p.change {
                    Change::EnumRebuild {
                        step: EnumRebuildStep::Retype { column, .. },
                        ..
                    } => Some(format!("retype {}", column.name.shown())),
                    Change::EnumRebuild {
                        step: EnumRebuildStep::Swap { .. },
                        ..
                    } => Some("swap".to_owned()),
                    Change::DropModule { .. } => Some("drop view".to_owned()),
                    Change::CreateModule { module, .. } => {
                        Some(format!("create view {}", module.definition))
                    }
                    Change::AlterModule { module, .. } => {
                        Some(format!("restate view {}", module.definition))
                    }
                    _ => None,
                })
                .collect()
        };
        let found = Hints {
            enum_dependents: [id].into(),
            ..Default::default()
        };
        let steps = order(&found);
        let at = |what: &str| {
            steps
                .iter()
                .position(|s| s == what)
                .unwrap_or_else(|| panic!("{what} in {steps:?}"))
        };
        assert!(at("drop view") < at("retype m"));
        assert!(at("retype tags") < at("swap"));
        assert!(at("swap") < at("create view SELECT m, id FROM app.t"));
        assert!(
            !steps.iter().any(|s| s.starts_with("restate view")),
            "{steps:?}"
        );
        // Not found by the census, the restatement stays in its class, after
        // the retypes, which is the plan the engine would refuse.
        let plain = order(&Hints::default());
        assert!(
            plain.contains(&"restate view SELECT m, id FROM app.t".to_owned()),
            "{plain:?}"
        );
        assert!(!plain.contains(&"drop view".to_owned()));
    }
}
