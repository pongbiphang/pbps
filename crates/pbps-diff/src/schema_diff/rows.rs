//! Rows: the row changes of a table and the facts about them.

use super::*;

enum BaselineDataKey<'a> {
    Absent,
    Single(&'a ColumnName),
    Multiple(&'a [ColumnName]),
}

impl<'a> BaselineDataKey<'a> {
    fn of(table: &'a Table) -> Self {
        match table.primary_key.as_ref().map(|pk| pk.columns.as_slice()) {
            None => Self::Absent,
            Some([column]) => Self::Single(column),
            Some(columns) => Self::Multiple(columns),
        }
    }
}

/// The tables whose rows this plan writes or deletes: a tightening of one of
/// their columns runs after those rows (#1367).
pub(super) fn tables_with_row_changes(changes: &[Change]) -> BTreeSet<TableName> {
    changes
        .iter()
        .filter_map(|c| row_table(c).cloned())
        .collect()
}

/// The tables a plan inserts declared rows into.
pub(super) fn tables_with_inserts<'a>(
    changes: impl Iterator<Item = &'a Change>,
) -> BTreeSet<TableName> {
    changes
        .filter_map(|c| {
            if let Change::Row(RowChange::InsertRow { table, .. }) = c {
                Some(table.clone())
            } else {
                None
            }
        })
        .collect()
}

/// The table a row change writes or deletes in.
pub(super) fn row_table(change: &Change) -> Option<&TableName> {
    if let Change::Row(RowChange::InsertRow { table, .. })
    | Change::Row(RowChange::UpdateRow { table, .. })
    | Change::Row(RowChange::DeleteRow { table, .. }) = change
    {
        Some(table)
    } else {
        None
    }
}

/// Declared reference data (ADR-0004).
///
/// The comparison is by primary-key value, because that is all the identity a
/// row has — no uid, no tombstone and no rename intent, since a row's entire
/// content is declared and recreating one is therefore lossless (the
/// generalized criterion of ADR-0005).
///
/// The asymmetry between the two modes is the whole point of having two:
/// `exact` claims the declaration is the table, so a row the declaration does
/// not mention is deleted; `ensure` claims only that the declared rows are
/// there, so an undeclared row is invisible — SPEC §8.2's managed set, at row
/// granularity.
pub(super) fn diff_data(
    name: &TableName,
    base: &Table,
    declared: &Table,
    base_name_of: &BTreeMap<ColumnName, ColumnName>,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    if base.data.as_ref().map(|d| d.mode) != declared.data.as_ref().map(|d| d.mode) {
        changes.push(Change::Row(RowChange::SetDataMode {
            table: name.clone(),
            from: base.data.as_ref().map(|d| d.mode),
            to: declared.data.as_ref().map(|d| d.mode),
        }));
    }

    // Every row change names the column its key goes in, so without one there
    // is nothing to emit and nothing to compare.
    let key_column = match &declared.primary_key {
        Some(pk) if pk.columns.len() == 1 => pk.columns[0].clone(),
        _ if declared.data.is_some() => {
            errs.push(DiffError::DataWithoutKey {
                table: name.clone(),
            });
            return;
        }
        _ => return,
    };

    let identity_key = declared
        .columns
        .get(&key_column)
        .is_some_and(|c| match &c.value_source {
            ValueSource::Identity(_) => true,
            // A `serial` key is refused by its dialect (#1631).
            ValueSource::None
            | ValueSource::Default(_)
            | ValueSource::Sequence(_)
            | ValueSource::Generated(_) => false,
        });

    let Some(declared_data) = &declared.data else {
        // The block was removed. Nothing is deleted and nothing is inserted:
        // removing the opt-in means pbps stops managing these rows, and reading
        // it as "delete them all" would make deleting a *declaration* destroy
        // data — the one thing this tool must never do quietly.
        return;
    };

    // No block on the base side means the table was not managed for rows
    // before, so every declared row is compared against nothing and inserted.
    // `exact` still deletes nothing here: what is in the table is unknown to
    // the baseline, and only the connected drift comparison can see it.
    let base_rows = base.data.as_ref().map(|d| &d.rows);

    // The row keys on each side are values of *that side's* key column. They
    // can only be matched when it is the same column — the same uid — on both
    // sides; renamed is fine, since the values did not move. A key that moved
    // to a different column leaves two sets of keys with nothing in common,
    // and matching them by text would update and delete the wrong rows.
    if base_rows.is_some() {
        let error = match BaselineDataKey::of(base) {
            BaselineDataKey::Absent => {
                // The constraint diff runs first. Retained rows can still be
                // compared using the declared key when this very plan restores
                // it on a column the baseline already has (DECISIONS 538).
                let restored = changes.iter().any(|change| {
                    matches!(change,
                    Change::Constraint(ConstraintChange::SetPrimaryKey { table, from: None, to: Some(pk), .. })
                    if table == name && pk.columns == [key_column.clone()])
                });
                if !restored {
                    Some(DiffError::DataBaselineKeyAbsent {
                        table: name.clone(),
                    })
                } else if !base_name_of
                    .get(&key_column)
                    .is_some_and(|column| base.columns.contains_key(column))
                {
                    // Adding a new key column cannot recover the identity of
                    // retained rows: their map keys predate that column.
                    Some(DiffError::DataBaselineKeyOnNewColumn {
                        table: name.clone(),
                        column: key_column.clone(),
                    })
                } else {
                    None
                }
            }
            BaselineDataKey::Multiple(columns) => Some(DiffError::DataBaselineKeyNotSingle {
                table: name.clone(),
                columns: columns.to_vec(),
            }),
            BaselineDataKey::Single(base_key) => {
                (base_name_of.get(&key_column).map(ColumnName::raw) != Some(base_key.raw())).then(
                    || DiffError::DataKeyColumnChanged {
                        table: name.clone(),
                    },
                )
            }
        };
        if let Some(error) = error {
            errs.push(error);
            return;
        }
    }

    for (key, row) in &declared_data.rows {
        match base_rows.and_then(|r| r.get(key)) {
            None => {
                let (defaults, types) = omitted_defaults(declared, key_column.raw(), row);
                changes.push(Change::Row(RowChange::InsertRow {
                    table: name.clone(),
                    key_column: key_column.clone(),
                    identity_key,
                    key: key.clone(),
                    row: row.clone(),
                    defaults,
                    types,
                }))
            }
            Some(before) => {
                // Only the columns that differ. An UPDATE restating a column
                // that did not change would overwrite a value the declaration
                // and the database already agree on, and would make plan.sql
                // claim a change that is not one.
                let mut columns = BTreeMap::new();
                let mut unchanged = BTreeMap::new();
                let mut types = BTreeMap::new();
                let mut after_types = BTreeMap::new();
                for (column, spec) in &declared.columns {
                    // The key lives in the map key, not in either row, so it
                    // has nothing to compare — and resolving it through the
                    // omission rule would read a default added to the key
                    // column as "set every key to DEFAULT".
                    if *column == key_column {
                        continue;
                    }
                    // A non-key `IDENTITY` column, or a generated one
                    // (DEC-1168.1), is the engine's: never
                    // written by a row (`validate` refuses it) and never read
                    // back (DECISIONS 94), so both sides resolve it to NULL
                    // and it is neither a change nor a cell the row can be
                    // held to — the engine assigned it.
                    if match spec.value_source {
                        pbps_model::ValueSource::Identity(_)
                        | pbps_model::ValueSource::Sequence(_)
                        | pbps_model::ValueSource::Generated(_) => true,
                        pbps_model::ValueSource::None | pbps_model::ValueSource::Default(_) => {
                            false
                        }
                    } {
                        continue;
                    }
                    // Each side against *its own* table, and the base side
                    // under the name the base knew the column by: a renamed
                    // column keeps its values, and looking it up by the new
                    // name would find nothing and restate every row. A column
                    // the base does not have at all is NULL there. A column
                    // that gains a default in this same plan resolves to NULL
                    // on the base and to the default on the declared side,
                    // which is an UPDATE — and the right one: adding a default
                    // does not backfill existing rows, and the declaration
                    // says the row should hold it.
                    let base_spec = base_name_of
                        .get(column)
                        .and_then(|base_column| base.columns.get(base_column));
                    let b = match base_name_of.get(column) {
                        Some(base_column) => {
                            cell(before, base_column.raw(), base.columns.get(base_column))
                        }
                        None => Cell::Value(Value::Null),
                    };
                    let d = cell(row, column.raw(), Some(spec));
                    // The base column's type, so the emitter can compare each
                    // cell by the rendering that read it (DECISIONS 122). A
                    // column the base lacks has no recorded cell to hold the
                    // update to.
                    //
                    // A column this plan *retypes* is carried too, and its
                    // new type goes into `after_types` below: the pair is
                    // what lets the emitter hold the row without spelling
                    // the converted value itself. 146 carried neither and
                    // held nothing, because the recorded text is the old
                    // type's spelling of a value the `AlterColumnType` has
                    // since converted — measured, a `decimal(5,2)` holding
                    // `1.50` reads back as `1` once the column is `int`, and
                    // comparing `N'1.50'` against it is a mismatch either
                    // way round. But dropping the predicate dropped the
                    // stale-row guard with it: a cell another session changed
                    // between the plan's read and the apply is converted by
                    // the `ALTER` and then overwritten by this `UPDATE` with
                    // nothing saying so. The engine can answer what the tool
                    // cannot — it converts the recorded text the same way it
                    // converted the column (DECISIONS 149).
                    if let Some(base_spec) = base_spec {
                        types.insert(column.clone(), base_spec.ty.clone());
                    }
                    // And the type it has once this plan has run, where the
                    // two differ: the column this plan adds, and the column
                    // whose type it changes. Both are in place by the time
                    // the row changes run, so the write's *postcondition*
                    // reads the cell back the way the column will actually
                    // hold it — where carrying only the base type left an
                    // added column held to nothing at all (DECISIONS 140).
                    if base_spec.map(|s| &s.ty) != Some(&spec.ty) {
                        after_types.insert(column.clone(), spec.ty.clone());
                    }
                    if b == d {
                        // Not restated, but still held: the declaration
                        // claims this cell as much as the changed ones, and
                        // the statement checks the whole row (DECISIONS 136).
                        unchanged.insert(column.clone(), d);
                    } else {
                        columns.insert(column.clone(), (b, d));
                    }
                }
                if !columns.is_empty() {
                    changes.push(Change::Row(RowChange::UpdateRow {
                        table: name.clone(),
                        key_column: key_column.clone(),
                        key: key.clone(),
                        columns,
                        unchanged,
                        types,
                        after_types,
                        // The declared type: any retype of the key sorts
                        // before the row changes, so it is the one the
                        // `WHERE` meets (DEC-1564.2).
                        key_type: declared.columns.get(&key_column).map(|c| c.ty.clone()),
                    }));
                }
            }
        }
    }

    // `exact` only. `ensure` never emits a DELETE — that is the promise the
    // mode makes to a table the application also writes to.
    if declared_data.mode == DataMode::Exact
        && let Some(base_rows) = base_rows
    {
        // The baseline names a surviving identity still has, asked once for
        // the table: the answer is the same for every deleted row, and a scan
        // per baseline column of every row made an `exact` table's deletes
        // cost rows × columns × columns in string comparisons (#402).
        let surviving: BTreeSet<&str> = base_name_of.values().map(ColumnName::raw).collect();
        for (key, before) in base_rows {
            if !declared_data.rows.contains_key(key) {
                // The recorded row travels with the delete, so the statement
                // removes what the reviewer saw rather than whatever holds
                // the key when it runs (DECISIONS 143).
                //
                // Values come from the *base* row, under the names the base
                // knew them by; the predicate names them as the table has
                // them when the DELETE runs, which is the declared name.
                // Every column change sorts before the row changes
                // (`order_key`), so a column renamed by this same plan is
                // already renamed by then, and one this plan drops is gone —
                // it holds nothing, and naming it would be a predicate on a
                // column that no longer exists.
                let mut row = BTreeMap::new();
                let mut types = BTreeMap::new();
                let mut after_types = BTreeMap::new();
                for (declared_column, base_column) in base_name_of {
                    // The key is the map key, and a non-key IDENTITY is the
                    // engine's — neither is a cell a row can be held to, for
                    // the same reasons as in an update.
                    if *declared_column == key_column {
                        continue;
                    }
                    let Some(spec) = base.columns.get(base_column) else {
                        continue;
                    };
                    if match spec.value_source {
                        pbps_model::ValueSource::Identity(_)
                        | pbps_model::ValueSource::Sequence(_)
                        | pbps_model::ValueSource::Generated(_) => true,
                        pbps_model::ValueSource::None | pbps_model::ValueSource::Default(_) => {
                            false
                        }
                    } {
                        continue;
                    }
                    row.insert(
                        declared_column.clone(),
                        cell(before, base_column.raw(), Some(spec)),
                    );
                    // The type the recorded text was read in, and — where
                    // this plan retypes the column — the type the column has
                    // by the time the `DELETE` runs, since `AlterColumnType`
                    // sorts before every row change. The emitter needs both
                    // to hold the row: one spelling of the cell is the
                    // recorded one and the other is the stored one, and only
                    // the engine can turn the first into the second
                    // (DECISIONS 149, where 146 held nothing at all).
                    //
                    // Only surviving identities have predicate types. Dropped
                    // columns are carried separately under their base names.
                    let Some(declared_ty) = declared.columns.get(declared_column).map(|d| &d.ty)
                    else {
                        continue;
                    };
                    types.insert(declared_column.clone(), spec.ty.clone());
                    if *declared_ty != spec.ty {
                        after_types.insert(declared_column.clone(), declared_ty.clone());
                    }
                }
                // The UID intersection above deliberately has no name for a
                // dropped column. Keep its baseline cell for review under the
                // only name it had, without inventing a declared identity or
                // a predicate on a column that is already gone. A historical
                // rename may reuse this base name for a surviving column, so
                // reviewer-only cells need their own map (DECISIONS 442).
                let mut dropped = BTreeMap::new();
                for (base_column, spec) in &base.columns {
                    if (match spec.value_source {
                        pbps_model::ValueSource::Identity(_)
                        | pbps_model::ValueSource::Sequence(_)
                        | pbps_model::ValueSource::Generated(_) => true,
                        pbps_model::ValueSource::None | pbps_model::ValueSource::Default(_) => {
                            false
                        }
                    }) || surviving.contains(base_column.raw())
                    {
                        continue;
                    }
                    dropped.insert(
                        base_column.clone(),
                        cell(before, base_column.raw(), Some(spec)),
                    );
                }
                changes.push(Change::Row(RowChange::DeleteRow {
                    table: name.clone(),
                    key_column: key_column.clone(),
                    key: key.clone(),
                    cause: DeleteCause::Undeclared,
                    row,
                    dropped,
                    types,
                    after_types,
                    key_type: declared.columns.get(&key_column).map(|c| c.ty.clone()),
                }));
            }
        }
    }
}

/// The defaults an inserted row is left to: every column the row omits, the
/// key aside, that the table gives a default. An `IDENTITY` column is the
/// engine's own and never one of these (DECISIONS 94, 117).
///
/// The types cover every non-key column, spelled or omitted: a spelled cell
/// is held by the rendering that reads it back (DECISIONS 137), a defaulted
/// one to its default (133), and a column the table gives no default is
/// left at NULL and held to that (136). An `IDENTITY` column is none of
/// these.
pub(super) fn omitted_defaults(
    table: &Table,
    key_column: &str,
    row: &pbps_model::Row,
) -> (
    BTreeMap<ColumnName, String>,
    BTreeMap<ColumnName, ColumnType>,
) {
    let types = table
        .row_columns(key_column)
        .map(|(c, spec)| (c.clone(), spec.ty.clone()))
        .collect();
    let defaults = omitted_columns(table, key_column, row)
        .filter_map(|(c, spec)| match &spec.value_source {
            ValueSource::Default(value) => Some((c.clone(), value.clone())),
            ValueSource::None
            | ValueSource::Identity(_)
            | ValueSource::Sequence(_)
            | ValueSource::Generated(_) => None,
        })
        .collect();
    (defaults, types)
}

/// The columns an insert leaves to the table: not the key, not spelled by
/// the row, and not an `IDENTITY` column, which is the engine's own.
fn omitted_columns<'a>(
    table: &'a Table,
    key_column: &'a str,
    row: &'a pbps_model::Row,
) -> impl Iterator<Item = (&'a ColumnName, &'a pbps_model::Column)> {
    table
        .row_columns(key_column)
        .filter(move |(c, _)| !row.0.contains_key(*c))
}
