//! Execution-position context for catalog-backed row defaults (DEC-2456.1).
use crate::type_refs::renames::TypeRenames;
use pbps_model::{Change, ChangeSet, ColumnName, ColumnRef, IdsFile, TableName};
use std::collections::BTreeSet;

#[derive(Default)]
pub(crate) struct RowDefaults {
    pub base: TypeRenames,
    written: BTreeSet<ColumnRef>,
}

impl RowDefaults {
    pub fn at(changes: &ChangeSet, position: usize) -> Self {
        if !matches!(
            changes.changes[position].change,
            Change::InsertRow { .. } | Change::UpdateRow { .. } | Change::DeleteRow { .. }
        ) {
            return Self::default();
        }
        // Lifecycle operations carry admitted source UIDs. Resolve once from
        // those original names to the names established by this prefix; never
        // repeatedly substitute the output of an earlier rename.
        let base = pbps_diff::identity::type_refs::before_changes(&IdsFile::default(), changes);
        let prefix = &changes.changes[..position];
        let now = pbps_diff::identity::type_refs::after_steps(&base, prefix);
        Self {
            base: TypeRenames::resolved(&base, &now),
            written: pbps_diff::row_defaults::written(prefix),
        }
    }

    pub fn after(&self, table: &TableName, column: &ColumnName) -> Option<&TypeRenames> {
        (!self.written.contains(&ColumnRef {
            table: table.clone(),
            name: column.clone(),
        }))
        .then_some(&self.base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{ColumnType, ManagedType, PlannedChange, Row, RowKey};

    fn rename(uid: &str, from: &str, to: &str) -> PlannedChange {
        PlannedChange::new(Change::RenameType {
            uid: uid.parse().unwrap(),
            from: from.parse().unwrap(),
            to: to.parse().unwrap(),
            definition: Box::new(ManagedType::Enum {
                values: vec!["sad".into()],
            }),
        })
    }

    fn row() -> PlannedChange {
        PlannedChange::new(Change::InsertRow {
            table: "app.t".parse().unwrap(),
            key_column: "id".into(),
            identity_key: false,
            key: RowKey::from("1"),
            row: Row::default(),
            defaults: Default::default(),
            types: Default::default(),
        })
    }

    #[test]
    fn mixed2456_prefix_forwarding_preserves_builtin_casts_and_each_original_uid() {
        let changes = ChangeSet {
            changes: vec![
                rename("y_000002", "app.feeling", "moved.last"),
                rename("y_000001", "app.mood", "app.feeling"),
                PlannedChange::new(Change::CreateType {
                    uid: "y_000003".parse().unwrap(),
                    name: "app.mood".parse().unwrap(),
                    definition: Box::new(ManagedType::Enum {
                        values: vec!["replacement".into()],
                    }),
                }),
                row(),
            ],
        };
        let context = RowDefaults::at(&changes, 3);
        for (from, to) in [
            (
                "('sad'::text)::app.mood",
                "('sad'::text)::\"app\".\"feeling\"",
            ),
            (
                "(('sad'::app.mood)::varchar(2))::app.mood",
                "(('sad'::\"app\".\"feeling\")::varchar(2))::\"app\".\"feeling\"",
            ),
            (
                "'{sad}'::text[]::app.mood[]",
                "'{sad}'::text[]::\"app\".\"feeling\"[]",
            ),
            ("'other'::app.feeling", "'other'::\"moved\".\"last\""),
            (
                "'::app.mood'::text /* ::app.mood */",
                "'::app.mood'::text /* ::app.mood */",
            ),
        ] {
            assert_eq!(context.base.expression(from), to);
        }
        // The original app.feeling belongs to the second UID; forwarding the
        // first UID's output again would silently select that second type.
        assert!(
            !context
                .base
                .expression("'sad'::text::app.mood")
                .contains("last")
        );
    }

    #[test]
    fn mixed2456_only_executed_renames_and_base_side_defaults_are_forwarded() {
        let table: TableName = "app.t".parse().unwrap();
        let changed: ColumnName = "changed".into();
        let changes = ChangeSet {
            changes: vec![
                rename("y_000001", "\"App\".\"M\"\"ood\"", "\"Moved\".\"select\""),
                PlannedChange::new(Change::AlterColumnDefault {
                    uid: "c_000001".parse().unwrap(),
                    column: ColumnRef {
                        table: table.clone(),
                        name: changed.clone(),
                    },
                    from: None,
                    to: Some("'sad'::text::app.future".into()),
                }),
                row(),
                rename("y_000002", "app.future", "app.later"),
            ],
        };
        let context = RowDefaults::at(&changes, 2);
        assert_eq!(
            context.base.expression("'sad'::text::\"App\".\"M\"\"ood\""),
            "'sad'::text::\"Moved\".\"select\""
        );
        assert_eq!(
            context.base.expression("'sad'::text::app.future"),
            "'sad'::text::\"app\".\"future\""
        );
        assert!(context.after(&table, &changed).is_none());
        assert!(context.after(&table, &"retained".into()).is_some());
        assert!(RowDefaults::default().base.0.is_empty());
    }

    #[test]
    fn mixed2456_forwarding_never_evaluates_nonliteral_guard_defaults() {
        let ty: ColumnType = "app.mood".parse().unwrap();
        let renames = TypeRenames::default();
        for text in ["lower('SAD')::app.mood", "nextval('app.s')::text::app.mood"] {
            assert!(
                super::super::defaulted_cell(
                    "next",
                    text,
                    Some(super::super::Held::same(&ty)),
                    Some(&renames)
                )
                .unwrap()
                .is_none()
            );
        }
    }
}
