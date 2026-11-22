//! Small helpers that more than one kind of object needs.

use super::*;

pub(super) fn columns_of(
    ids: &pbps_model::IdsIndex<'_>,
    table: &TableName,
) -> BTreeMap<Uid, ColumnRef> {
    ids.columns_of(table)
        .map(|(u, c)| (u.clone(), c.clone()))
        .collect()
}

pub(super) fn partition_columns_by_uid<'a, T: 'a>(
    columns: impl IntoIterator<Item = (&'a ColumnName, &'a T)>,
    before: &TableName,
    after: &TableName,
    index: &Indexes<'_>,
) -> BTreeMap<ColumnName, &'a T> {
    // What became of each of the parent's base columns, by uid: its declared
    // name, or `None` where the plan drops it. By uid and not by name, since
    // one plan can drop a column and rename another into its name (#1692
    // review). A column no identity names is taken as it stands.
    let now_by_uid = columns_of(&index.declared, after);
    let fate: BTreeMap<ColumnName, Option<ColumnName>> = columns_of(&index.base, before)
        .into_iter()
        .map(|(uid, c)| (c.name, now_by_uid.get(&uid).map(|d| d.name.clone())))
        .collect();
    let is_now = |c: &ColumnName| -> Option<ColumnName> {
        match fate.get(c) {
            Some(now) => now.clone(),
            None => Some(c.clone()),
        }
    };
    columns
        .into_iter()
        .filter_map(|(c, value)| Some((is_now(c)?, value)))
        .collect()
}

/// The renames this plan performs, as a map the model can bring a base-side
/// table's constraints forward through.
///
/// The uid is what says two differently-spelled names are one object, exactly
/// as it does for rows (see `base_name_of` at the call site). What the map is
/// then allowed to rewrite — and what it must not — is [`Renames::apply`]'s
/// business, and the measurement that decides it lives there.
pub(super) fn renames_of(base: Side, declared: Side) -> Renames {
    let mut renames = Renames::default();
    for (uid, declared_name) in &declared.ids.tables {
        if let Some(base_name) = base.ids.tables.get(uid)
            && base_name != declared_name
        {
            renames.rename_table(base_name.clone(), declared_name.clone());
        }
    }
    for (uid, declared_ref) in &declared.ids.columns {
        if let Some(base_ref) = base.ids.columns.get(uid)
            && base_ref.name != declared_ref.name
        {
            renames.rename_column(base_ref.clone(), declared_ref.name.clone());
        }
    }
    renames
}

/// A table's columns this plan renames, base name to declared name, by uid.
pub(super) fn renamed_columns(
    index: &Indexes<'_>,
    base_name: &TableName,
    declared_name: &TableName,
) -> BTreeMap<ColumnName, ColumnName> {
    let now = columns_of(&index.declared, declared_name);
    columns_of(&index.base, base_name)
        .into_iter()
        .filter_map(|(uid, was)| {
            now.get(&uid)
                .filter(|c| c.name != was.name)
                .map(|c| (was.name, c.name.clone()))
        })
        .collect()
}

/// The text that orders two changes tied in class, rank and subject: the kind and
/// its payload as `Debug` writes them, so changes of one kind sit together and
/// kinds sit in alphabetical order, across families (a saved plan's order
/// depends on it). A named function so that the move of the kinds into family
/// enums (#2790) cannot change it by changing `Debug`; `change_in_words` reads
/// the kind's own name from it.
pub(super) fn tie_break_text(change: &Change) -> String {
    format!("{change:?}")
}

/// A change's kind as words, `drop column` for `DropColumn`: the variant's
/// own name, so a kind added later is named without anyone remembering to.
pub(super) fn change_in_words(change: &Change) -> String {
    let debug = tie_break_text(change);
    let kind = debug
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default();
    let mut words = String::new();
    for (i, c) in kind.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            words.push(' ');
        }
        words.push(c.to_ascii_lowercase());
    }
    words
}

// Physical teardown for a conversion or a dependency is separate from whether
// the declaration changes its default. SQL expressions stay opaque (SPEC §8.2).
pub(super) fn defaults_differ(before: Option<&String>, after: Option<&String>) -> bool {
    before != after
}
