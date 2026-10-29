//! Approved row-guard expectations, separate from authored comparison text.
use crate::Side;
use pbps_model::{Cell, Change, ColumnRef, PlannedChange};
use std::collections::{BTreeMap, BTreeSet};

/// Defaults written by this plan are after-side expressions. Retained defaults
/// instead bind in the base snapshot's namespace (DEC-2456.1).
#[allow(clippy::wildcard_enum_match_arm)]
pub fn written(changes: &[PlannedChange]) -> BTreeSet<ColumnRef> {
    let mut written = BTreeSet::new();
    for step in changes {
        match &step.change {
            Change::AlterColumnDefault { column, .. } => {
                written.insert(column.clone());
            }
            Change::AddColumn { table, name, .. } => {
                written.insert(ColumnRef {
                    table: table.clone(),
                    name: name.clone(),
                });
            }
            Change::CreateTable { name, table, .. } => {
                written.extend(table.columns.keys().map(|column| ColumnRef {
                    table: name.clone(),
                    name: column.clone(),
                }));
            }
            _ => {}
        }
    }
    written
}

/// Enrich only guard payloads, after deciding which cells changed. Neither
/// omission intent nor authored-default equality may depend on catalog layout.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn catalog(
    changes: &mut [PlannedChange],
    base: Side<'_>,
    desired: Side<'_>,
    defaults: &BTreeMap<ColumnRef, String>,
) {
    if defaults.is_empty() {
        return;
    }
    let writes = written(changes);
    let index = desired.ids.index();
    let before = |column: &ColumnRef| {
        let uid = index.column_uid(column)?;
        defaults.get(base.ids.columns.get(uid)?)
    };
    for step in changes {
        let replace = |column: &ColumnRef, cell: &mut Cell| {
            if let Cell::Default(text) = cell
                && let Some(catalog) = before(column)
            {
                *text = catalog.clone();
            }
        };
        match &mut step.change {
            Change::InsertRow {
                table, defaults, ..
            } => {
                for (name, text) in defaults {
                    let column = ColumnRef {
                        table: table.clone(),
                        name: name.clone(),
                    };
                    if !writes.contains(&column)
                        && let Some(catalog) = before(&column)
                    {
                        *text = catalog.clone();
                    }
                }
            }
            Change::UpdateRow {
                table,
                columns,
                unchanged,
                ..
            } => {
                for (name, (from, to)) in columns {
                    let column = ColumnRef {
                        table: table.clone(),
                        name: name.clone(),
                    };
                    replace(&column, from);
                    if !writes.contains(&column) {
                        replace(&column, to);
                    }
                }
                for (name, cell) in unchanged {
                    replace(
                        &ColumnRef {
                            table: table.clone(),
                            name: name.clone(),
                        },
                        cell,
                    );
                }
            }
            Change::DeleteRow { table, row, .. } => {
                for (name, cell) in row {
                    replace(
                        &ColumnRef {
                            table: table.clone(),
                            name: name.clone(),
                        },
                        cell,
                    );
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{IdsFile, RowKey, Schema, Uid, Value};

    #[test]
    fn mixed2456_catalog_enrichment_follows_column_uids_and_preserves_new_defaults_and_values() {
        let old: ColumnRef = "app.old.before".parse().unwrap();
        let new: ColumnRef = "moved.new.after".parse().unwrap();
        let uid: Uid = "c_000001".parse().unwrap();
        let base_ids = IdsFile {
            columns: [(uid.clone(), old.clone())].into(),
            ..Default::default()
        };
        let desired_ids = IdsFile {
            columns: [(uid.clone(), new.clone())].into(),
            ..Default::default()
        };
        let schema = Schema::default();
        let b = Side {
            schema: &schema,
            ids: &base_ids,
        };
        let d = Side {
            schema: &schema,
            ids: &desired_ids,
        };
        let authored = "'sad'::text::app.retired";
        let read_back = "('sad'::text)::app.current";
        let defaults = [(old, read_back.into())].into();
        let make = || {
            PlannedChange::new(Change::UpdateRow {
                table: new.table.clone(),
                key_column: "id".into(),
                key: RowKey::from("1"),
                columns: [(
                    new.name.clone(),
                    (
                        Cell::Default(authored.into()),
                        Cell::Default(authored.into()),
                    ),
                )]
                .into(),
                unchanged: [("explicit".into(), Cell::Value(Value::Text("saved".into())))].into(),
                types: Default::default(),
                after_types: Default::default(),
                key_type: None,
            })
        };
        let mut retained = vec![make()];
        catalog(&mut retained, b, d, &defaults);
        let Change::UpdateRow {
            columns, unchanged, ..
        } = &retained[0].change
        else {
            panic!()
        };
        assert_eq!(
            columns[&new.name],
            (
                Cell::Default(read_back.into()),
                Cell::Default(read_back.into())
            )
        );
        assert_eq!(
            unchanged["explicit"],
            Cell::Value(Value::Text("saved".into()))
        );
        let mut changed = vec![
            PlannedChange::new(Change::AlterColumnDefault {
                uid,
                column: new.clone(),
                from: Some(authored.into()),
                to: Some("'ok'::text::app.new".into()),
            }),
            make(),
        ];
        catalog(&mut changed, b, d, &defaults);
        let Change::UpdateRow { columns, .. } = &changed[1].change else {
            panic!()
        };
        assert_eq!(
            columns[&new.name],
            (
                Cell::Default(read_back.into()),
                Cell::Default(authored.into())
            )
        );
        // A recycled name or missing identity cannot borrow another column's
        // approved catalog expression, and missing catalog evidence stays missing.
        let different = IdsFile::default();
        let mut missing = vec![make()];
        catalog(
            &mut missing,
            b,
            Side {
                schema: &schema,
                ids: &different,
            },
            &defaults,
        );
        assert_eq!(missing, vec![make()]);
        catalog(&mut missing, b, d, &BTreeMap::new());
        assert_eq!(missing, vec![make()]);
    }
}
