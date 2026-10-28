//! Execution-position context for catalog-backed row defaults (DEC-2456.1).
use crate::type_refs::renames::TypeRenames;
use pbps_model::{Change, ChangeSet, ColumnName, ColumnRef, IdsFile, TableName};
use std::collections::BTreeMap;

static EMPTY_RENAMES: TypeRenames = TypeRenames(BTreeMap::new());

/// Indexed once per emission; rows only look up a lifecycle boundary and the
/// first write of a default. Row payloads never enter the index.
#[derive(Default)]
pub(crate) struct RowDefaultIndex {
    prefixes: Vec<(usize, TypeRenames)>,
    written: BTreeMap<ColumnRef, usize>,
}

impl RowDefaultIndex {
    #[allow(clippy::wildcard_enum_match_arm)]
    pub fn new(changes: &ChangeSet) -> Self {
        #[cfg(test)]
        BUILDS.with(|count| count.set(count.get() + 1));
        let base = pbps_diff::identity::type_refs::before_changes(&IdsFile::default(), changes);
        let mut now = base.clone();
        let mut index = Self::default();
        for (position, planned) in changes.changes.iter().enumerate() {
            // The identity walk sees only lifecycle events after its one base
            // reconstruction, never a row-filled prefix (DEC-2456.1).
            if matches!(
                planned.change,
                Change::CreateType { .. }
                    | Change::DropType { .. }
                    | Change::RenameType { .. }
                    | Change::AddEnumValue { .. }
                    | Change::RenameEnumValue { .. }
                    | Change::AlterDomain { .. }
                    | Change::EnumRebuild { .. }
            ) {
                now = pbps_diff::identity::type_refs::after_steps(
                    &now,
                    std::slice::from_ref(planned),
                );
                let renamed = TypeRenames::resolved(&base, &now);
                let previous = index
                    .prefixes
                    .last()
                    .map_or(&EMPTY_RENAMES, |(_, names)| names);
                if renamed.0 != previous.0 {
                    index.prefixes.push((position, renamed));
                }
            }
            for column in pbps_diff::row_defaults::written(std::slice::from_ref(planned)) {
                index.written.entry(column).or_insert(position);
            }
        }
        index
    }

    pub fn at(&self, position: usize) -> RowDefaults<'_> {
        let end = self.prefixes.partition_point(|(at, _)| *at < position);
        RowDefaults {
            base: end
                .checked_sub(1)
                .map_or(&EMPTY_RENAMES, |i| &self.prefixes[i].1),
            written: (!self.written.is_empty()).then_some(&self.written),
            position,
        }
    }
}

pub(crate) struct RowDefaults<'a> {
    pub base: &'a TypeRenames,
    written: Option<&'a BTreeMap<ColumnRef, usize>>,
    position: usize,
}

impl Default for RowDefaults<'_> {
    fn default() -> Self {
        Self {
            base: &EMPTY_RENAMES,
            written: None,
            position: 0,
        }
    }
}

impl RowDefaults<'_> {
    pub fn after(&self, table: &TableName, column: &ColumnName) -> Option<&TypeRenames> {
        let written = self
            .written
            .and_then(|written| {
                written.get(&ColumnRef {
                    table: table.clone(),
                    name: column.clone(),
                })
            })
            .is_some_and(|at| *at < self.position);
        (!written).then_some(self.base)
    }
}

#[cfg(test)]
thread_local! { static BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

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
        let index = RowDefaultIndex::new(&changes);
        let context = index.at(3);
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
        let index = RowDefaultIndex::new(&changes);
        let context = index.at(2);
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

    fn update(key: usize, renamed: bool) -> PlannedChange {
        PlannedChange::new(Change::UpdateRow {
            table: "app.t".parse().unwrap(),
            key_column: "id".into(),
            key: RowKey::from(key.to_string()),
            columns: [(
                "note".into(),
                (
                    pbps_model::Cell::Value(pbps_model::Value::Text("a".into())),
                    pbps_model::Cell::Value(pbps_model::Value::Text("b".into())),
                ),
            )]
            .into(),
            unchanged: [(
                "next".into(),
                pbps_model::Cell::Default("('sad'::text)::app.mood".into()),
            )]
            .into(),
            types: [
                ("note".into(), "text".parse().unwrap()),
                ("next".into(), "app.mood".parse().unwrap()),
            ]
            .into(),
            after_types: if renamed {
                [("next".into(), "app.feeling".parse().unwrap())].into()
            } else {
                Default::default()
            },
            key_type: Some("integer".parse().unwrap()),
        })
    }

    fn admitted(
        changes: &ChangeSet,
        renamed: bool,
    ) -> pbps_diff::order::AdmittedPlan<crate::Postgres> {
        let name: pbps_model::TypeRef = if renamed { "app.feeling" } else { "app.mood" }
            .parse()
            .unwrap();
        let definition = ManagedType::Enum {
            values: vec!["sad".into()],
        };
        let mut wire = pbps_model::SavedPlan::new(
            pbps_model::PlanOrigin::Database,
            "postgres",
            "2026-10-28T00:00:00Z",
            pbps_model::PlanBaseline {
                description: "row context byte oracle".into(),
                checksum: "base".into(),
                database_collation: None,
                time_zone: None,
            },
            changes.clone(),
            IdsFile {
                types: [("y_000001".parse().unwrap(), name.clone())].into(),
                tables: [("t_000001".parse().unwrap(), "app.t".parse().unwrap())].into(),
                columns: [
                    ("c_000001".parse().unwrap(), "app.t.id".parse().unwrap()),
                    ("c_000002".parse().unwrap(), "app.t.next".parse().unwrap()),
                    ("c_000003".parse().unwrap(), "app.t.note".parse().unwrap()),
                ]
                .into(),
                ..Default::default()
            },
        );
        wire.declared_type_pins.push(pbps_model::DeclaredTypePin {
            uid: "y_000001".parse().unwrap(),
            name,
            definition: definition.clone(),
            before: Some(pbps_model::DeclaredTypeBefore {
                name: "app.mood".parse().unwrap(),
                definition,
            }),
        });
        let checksum = wire.wire_fingerprint();
        pbps_diff::order::admit(
            wire,
            &crate::Postgres::new(),
            std::path::Path::new("rows.plan.json"),
            &checksum,
        )
        .unwrap()
    }

    /// Frozen per-position algorithm: this is an SQL/metadata oracle, never
    /// used by production emission or counted as a prepared index build.
    fn legacy_emit(
        changes: &ChangeSet,
        position: usize,
        mode: pbps_model::PlanMode,
    ) -> Vec<pbps_dialect::Statement> {
        let base = pbps_diff::identity::type_refs::before_changes(&IdsFile::default(), changes);
        let prefix = &changes.changes[..position];
        let now = pbps_diff::identity::type_refs::after_steps(&base, prefix);
        let renames = TypeRenames::resolved(&base, &now);
        let written = pbps_diff::row_defaults::written(prefix)
            .into_iter()
            .map(|c| (c, 0))
            .collect();
        let context = RowDefaults {
            base: &renames,
            written: Some(&written),
            position: 1,
        };
        let step = &changes.changes[position];
        let pg = crate::Postgres::new();
        match mode {
            pbps_model::PlanMode::Transactional => {
                super::super::emit_in(&pg, &step.change, step.strategy, &context)
            }
            pbps_model::PlanMode::Staged => {
                super::super::emit_staged_in(&pg, &step.change, step.strategy, &context)
            }
        }
        .unwrap()
    }

    #[test]
    fn mixed2456_many_rows_build_one_context_and_lookup_the_executed_rename() {
        use pbps_dialect::PlanDialect;
        let pg = crate::Postgres::new();
        let mut changes = ChangeSet {
            changes: (0..5000).map(|key| update(key, key >= 2500)).collect(),
        };
        changes
            .changes
            .insert(2500, rename("y_000001", "app.mood", "app.feeling"));
        let plan = admitted(&changes, true);
        let before = BUILDS.with(std::cell::Cell::get);
        let lowered = pbps_dialect::lower::lower_all(
            &plan as &dyn pbps_diff::order::PlanView<crate::Postgres>,
            &pg,
        )
        .unwrap();
        assert_eq!(
            BUILDS.with(std::cell::Cell::get) - before,
            1,
            "one context per emission, not per row"
        );
        assert_eq!(lowered.groups().len(), 5001);
        for (position, group) in lowered.groups().iter().enumerate() {
            if position == 2500 {
                continue;
            }
            let cast = if position < 2500 {
                "::\"app\".\"mood\""
            } else {
                "::\"app\".\"feeling\""
            };
            assert!(
                group.statements().iter().any(|s| s.sql.contains(cast)),
                "position {position}"
            );
        }
        for position in [0, 2499, 2500, 2501, 5000] {
            assert_eq!(
                lowered.groups()[position].statements(),
                legacy_emit(&changes, position, pbps_model::PlanMode::Transactional)
            );
        }
        // Staged byte observation uses the same certified input, without
        // granting a multi-change staged artifact execution authority.
        let before = BUILDS.with(std::cell::Cell::get);
        let emit = pg.prepare_emission(&plan, pbps_model::PlanMode::Staged);
        for position in 0..changes.changes.len() {
            let actual = emit(position).unwrap();
            if [0, 2499, 2500, 2501, 5000].contains(&position) {
                assert_eq!(
                    actual,
                    legacy_emit(&changes, position, pbps_model::PlanMode::Staged)
                );
            }
        }
        assert_eq!(BUILDS.with(std::cell::Cell::get) - before, 1);
    }

    #[test]
    fn mixed2456_no_lifecycle_keeps_empty_context_and_default_write_boundaries() {
        let pg = crate::Postgres::new();
        let mut after = update(1, false);
        let Change::UpdateRow {
            columns, unchanged, ..
        } = &mut after.change
        else {
            unreachable!()
        };
        unchanged.remove("next");
        columns.insert(
            "next".into(),
            (
                pbps_model::Cell::Default("('sad'::text)::app.mood".into()),
                pbps_model::Cell::Default("'sad'::varchar::app.mood".into()),
            ),
        );
        let changes = ChangeSet {
            changes: vec![
                update(0, false),
                PlannedChange::new(Change::AlterColumnDefault {
                    uid: "c_000002".parse().unwrap(),
                    column: "app.t.next".parse().unwrap(),
                    from: Some("'sad'::text::app.mood".into()),
                    to: Some("'sad'::varchar::app.mood".into()),
                }),
                after,
            ],
        };
        let index = RowDefaultIndex::new(&changes);
        assert!(index.prefixes.is_empty());
        for position in 0..changes.changes.len() {
            assert!(index.at(position).base.0.is_empty());
        }
        assert!(
            index
                .at(1)
                .after(&"app.t".parse().unwrap(), &"next".into())
                .is_some()
        );
        assert!(
            index
                .at(2)
                .after(&"app.t".parse().unwrap(), &"next".into())
                .is_none()
        );
        let plan = admitted(&changes, false);
        let before = BUILDS.with(std::cell::Cell::get);
        let lowered = pbps_dialect::lower::lower_all(
            &plan as &dyn pbps_diff::order::PlanView<crate::Postgres>,
            &pg,
        )
        .unwrap();
        assert_eq!(BUILDS.with(std::cell::Cell::get) - before, 1);
        for position in 0..changes.changes.len() {
            assert_eq!(
                lowered.groups()[position].statements(),
                legacy_emit(&changes, position, pbps_model::PlanMode::Transactional)
            );
        }
        // A real staged input still obtains its mode from the certificate.
        let plan = pbps_diff::order::singleton(update(0, false), pbps_model::PlanMode::Staged, &pg);
        let before = BUILDS.with(std::cell::Cell::get);
        let staged = pbps_dialect::lower::lower_all(
            &plan as &dyn pbps_diff::order::PlanView<crate::Postgres>,
            &pg,
        )
        .unwrap();
        assert_eq!(BUILDS.with(std::cell::Cell::get) - before, 1);
        assert_eq!(
            staged.statements(),
            legacy_emit(
                &ChangeSet {
                    changes: vec![update(0, false)]
                },
                0,
                pbps_model::PlanMode::Staged
            )
        );
    }

    #[test]
    fn mixed2456_rows_without_defaults_or_lifecycle_build_once_with_empty_context() {
        let pg = crate::Postgres::new();
        let mut changes = ChangeSet {
            changes: (0..5000).map(|key| update(key, false)).collect(),
        };
        for step in &mut changes.changes {
            let Change::UpdateRow {
                unchanged, types, ..
            } = &mut step.change
            else {
                unreachable!()
            };
            unchanged.clear();
            types.remove("next");
        }
        let index = RowDefaultIndex::new(&changes);
        assert!(index.prefixes.is_empty() && index.written.is_empty());
        let plan = admitted(&changes, false);
        let before = BUILDS.with(std::cell::Cell::get);
        let lowered = pbps_dialect::lower::lower_all(
            &plan as &dyn pbps_diff::order::PlanView<crate::Postgres>,
            &pg,
        )
        .unwrap();
        assert_eq!(BUILDS.with(std::cell::Cell::get) - before, 1);
        for position in [0, 2499, 4999] {
            assert_eq!(
                lowered.groups()[position].statements(),
                legacy_emit(&changes, position, pbps_model::PlanMode::Transactional)
            );
        }
    }
}
