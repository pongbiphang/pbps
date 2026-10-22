use pbps_model::{Change, ChangeSet, IdsFile};

use super::{super::Settled, super::StagedRead, Plan};

enum Mode {
    Transactional,
    StagedMid,
    StagedWhole,
    StagedClosing,
}

/// A read carries its admitted plan: an arbitrary ChangeSet cannot acquire
/// checkpoint exemptions just by being paired with `SoFar` (DEC-1950.1).
pub(in crate::deploy) struct Read<'a> {
    plan: Plan<'a>,
    mode: Mode,
}

impl<'a> Read<'a> {
    pub(in crate::deploy) fn transactional(plan: Plan<'a>) -> Self {
        Self {
            plan,
            mode: Mode::Transactional,
        }
    }

    fn staged(plan: Plan<'a>, read: StagedRead) -> Self {
        let mode = match read {
            StagedRead::Checkpoint { completed, total } if completed < total => Mode::StagedMid,
            StagedRead::Checkpoint { .. } => Mode::StagedWhole,
            StagedRead::Closing { .. } => Mode::StagedClosing,
        };
        Self { plan, mode }
    }

    pub(in crate::deploy) fn parts(self) -> (Plan<'a>, Settled) {
        // A transactional read, like the staged closing read, is scoped by
        // the plan alone. Its omitted default cells therefore mean something
        // a checkpoint's omissions cannot (DECISIONS 191).
        let settled = match self.mode {
            Mode::Transactional | Mode::StagedClosing => Settled::Closing,
            Mode::StagedMid => Settled::SoFar,
            Mode::StagedWhole => Settled::Whole,
        };
        (self.plan, settled)
    }
}

/// Validate once, before executing SQL; statement views preserve this proof
/// without discovering a new admission error after a statement has committed.
pub(in crate::deploy) struct Staged<'a>(Plan<'a>);

impl<'a> Staged<'a> {
    pub(in crate::deploy) fn new(plan: Plan<'a>) -> anyhow::Result<Self> {
        let logical = crate::logical_changes(plan.changes);
        anyhow::ensure!(
            logical == 1,
            "a staged guard requires exactly one logical change; this plan has {logical}"
        );
        Ok(Self(plan))
    }

    pub(in crate::deploy) fn read(&self, read: StagedRead) -> Read<'a> {
        Read::staged(self.0, read)
    }

    pub(in crate::deploy) fn statement(
        &self,
        statement: &pbps_dialect::Statement,
    ) -> StatementPlan {
        let mut changes = self.0.changes.clone();
        let mut ids = self.0.ids.clone();
        // One logical rename can emit a schema transfer followed by a rename.
        // Its checkpoint must use the emitter's actual endpoints, including
        // the intermediate address absent from the plan (DECISIONS 436).
        // Move the pinned UID with the same adapter; observations never choose
        // either its identity or its spelling.
        if let [change] = changes.changes.as_mut_slice()
            && let Change::RenameTable { from, to, .. } = &mut change.change
            && let [(statement_from, statement_to)] = statement.renames.as_slice()
        {
            ids.rename_table(to, statement_to);
            *from = statement_from.clone();
            *to = statement_to.clone();
        }
        StatementPlan { changes, ids }
    }
}

pub(in crate::deploy) struct StatementPlan {
    changes: ChangeSet,
    ids: IdsFile,
}

impl StatementPlan {
    pub(in crate::deploy) fn staged(&self) -> Staged<'_> {
        // Only `Staged::statement` constructs this owned view, and its sole
        // edit preserves the admitted logical change's kind and identity.
        Staged(Plan {
            changes: &self.changes,
            ids: &self.ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::refuse_pinned_movement;
    use pbps_model::{
        Column, Index, IndexColumn, IndexKey, IndexPart, PlannedChange, Schema, Table, TableName,
    };

    fn changes(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        }
    }

    fn index() -> Index {
        Index {
            columns: vec![IndexColumn {
                key: IndexKey::Column("id".into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: Some("id > 0".into()),
            method: Default::default(),
            storage_parameters: Default::default(),
        }
    }

    fn table() -> Table {
        Table {
            columns: [("id".into(), Column::new("integer".parse().unwrap()))].into(),
            ..Default::default()
        }
    }

    fn create(name: &TableName, table: Table) -> Change {
        Change::CreateTable {
            uid: "t_000001".parse().unwrap(),
            name: name.clone(),
            table: Box::new(table),
            beside_default: None,
            moves_at_most: None,
        }
    }

    fn schema(name: &TableName, table: Table) -> Schema {
        Schema {
            tables: [(name.clone(), table)].into(),
            ..Default::default()
        }
    }

    #[test]
    fn created_parts_with_later_logical_changes_cannot_get_checkpoint_exemptions() {
        let name = TableName::new("app", "t");
        let ids = IdsFile::default();
        assert!(
            Staged::new(Plan {
                changes: &ChangeSet::default(),
                ids: &ids
            })
            .is_err()
        );
        for carried in [false, true] {
            let mut declared = table();
            if carried {
                declared.indexes.insert("ix".into(), index());
            }
            let mut steps = vec![create(&name, declared)];
            if carried {
                steps.push(Change::DropIndex {
                    table: name.clone(),
                    name: "ix".into(),
                });
            }
            steps.extend([
                Change::AddIndex {
                    table: name.clone(),
                    name: "ix".into(),
                    index: Box::new(index()),
                    clustered: false,
                },
                Change::SetIndexStorageParameters {
                    table: name.clone(),
                    target: IndexPart::Index("ix".into()),
                    method: Default::default(),
                    set: [("fillfactor".into(), "80".into())].into(),
                    reset: Default::default(),
                },
            ]);
            let planned = changes(steps);
            let error = Staged::new(Plan {
                changes: &planned,
                ids: &ids,
            })
            .err()
            .expect("CREATE and later part changes cannot be staged together");
            assert!(
                error
                    .to_string()
                    .contains(if carried { "has 4" } else { "has 3" }),
                "{error:#}"
            );
        }
    }

    #[test]
    fn a_routines_own_public_execution_companion_is_one_staged_change() {
        let routine = || Change::CreateModule {
            id: "app.f(integer)".parse().unwrap(),
            module: Box::new(pbps_model::Module {
                kind: pbps_model::ModuleKind::Function,
                description: None,
                definition: "(integer) RETURNS integer LANGUAGE sql AS $$ SELECT $1 $$".into(),
            }),
        };
        let access = |name: &str| Change::PublicExecution {
            routine: name.parse().unwrap(),
            access: pbps_model::PublicAccess::Revoked,
            origin: pbps_model::RoutineOrigin::Created,
        };
        let ids = IdsFile::default();
        let own = changes(vec![routine(), access("app.f(integer)")]);
        Staged::new(Plan {
            changes: &own,
            ids: &ids,
        })
        .expect("a companion is not a second logical change");
        for other in ["app.f(text)", "app.g(integer)"] {
            let unrelated = changes(vec![routine(), access(other)]);
            assert!(
                Staged::new(Plan {
                    changes: &unrelated,
                    ids: &ids
                })
                .is_err(),
                "{other}"
            );
        }
    }

    #[test]
    fn one_create_accepts_pending_parts_but_holds_observed_definitions() {
        let name = TableName::new("app", "t");
        let mut declared = table();
        declared.indexes.insert("ix".into(), index());
        let planned = changes(vec![create(&name, declared.clone())]);
        let ids = super::super::fixture_ids(&planned, &Schema::default());
        let staged = Staged::new(Plan {
            changes: &planned,
            ids: &ids,
        })
        .unwrap();
        let checkpoint = StagedRead::Checkpoint {
            completed: 1,
            total: 2,
        };
        let empty = Schema::default();
        let bare = schema(&name, table());
        let complete = schema(&name, declared.clone());
        let dialect = pbps_pg::Postgres::new();
        refuse_pinned_movement(&dialect, staged.read(checkpoint), &empty, &bare, "test")
            .expect("the index is emitted after CREATE in the same logical change");
        refuse_pinned_movement(&dialect, staged.read(checkpoint), &bare, &complete, "test")
            .expect("the pending index can appear at a later checkpoint");
        refuse_pinned_movement(
            &dialect,
            staged.read(StagedRead::Closing { total: 2 }),
            &complete,
            &complete,
            "test",
        )
        .expect("the promised parts are present at the close");
        let error = refuse_pinned_movement(
            &dialect,
            staged.read(StagedRead::Closing { total: 2 }),
            &bare,
            &bare,
            "test",
        )
        .expect_err("missing parts are no longer pending at the close");
        assert!(format!("{error:#}").contains("index `ix`"), "{error:#}");
        for already_observed in [false, true] {
            let mut wrong = declared.clone();
            let part = wrong.indexes.get_mut("ix").unwrap();
            if already_observed {
                part.filter = Some("id > 1".into());
            } else {
                part.columns[0].descending = true;
            }
            let before = if already_observed { &complete } else { &bare };
            let error = refuse_pinned_movement(
                &dialect,
                staged.read(checkpoint),
                before,
                &schema(&name, wrong),
                "test",
            )
            .expect_err("declared structure and already-recorded expression text stay held");
            assert!(format!("{error:#}").contains("index `ix`"), "{error:#}");
        }
    }

    #[test]
    fn one_cross_schema_rename_keeps_its_uid_at_each_emitted_address() {
        let from = TableName::new("app", "old");
        let to = TableName::new("archive", "new");
        let planned = changes(vec![Change::RenameTable {
            uid: "t_000001".parse().unwrap(),
            from: from.clone(),
            to: to.clone(),
            defaults: Vec::new(),
        }]);
        let baseline = schema(&from, table());
        let ids = super::super::fixture_ids(&planned, &baseline);
        let staged = Staged::new(Plan {
            changes: &planned,
            ids: &ids,
        })
        .unwrap();
        for dialect in [
            &pbps_mssql::Mssql as &dyn pbps_dialect::Dialect,
            &pbps_pg::Postgres::new(),
        ] {
            let statements =
                crate::statements(&planned, dialect, pbps_model::PlanMode::Staged).unwrap();
            assert_eq!(statements.len(), 2, "a transfer and a rename");
            let mut before = baseline.clone();
            for (i, statement) in statements.iter().enumerate() {
                let at_statement = staged.statement(statement);
                let [(.., address)] = statement.renames.as_slice() else {
                    panic!("emitted rename metadata")
                };
                let observed = schema(address, table());
                let read = StagedRead::Checkpoint {
                    completed: i + 1,
                    total: statements.len(),
                };
                refuse_pinned_movement(
                    dialect,
                    at_statement.staged().read(read),
                    &before,
                    &observed,
                    "test",
                )
                .expect("both the planned name and pinned UID follow the emitted statement");
                let mut wrong = table();
                wrong.columns.get_mut("id").unwrap().nullable = false;
                let error = refuse_pinned_movement(
                    dialect,
                    at_statement.staged().read(read),
                    &before,
                    &schema(address, wrong),
                    "test",
                )
                .expect_err("the intermediate address keeps every unchanged column field");
                assert!(format!("{error:#}").contains("nullability"), "{error:#}");
                before = observed;
            }
            refuse_pinned_movement(
                dialect,
                staged.read(StagedRead::Closing { total: 2 }),
                &before,
                &schema(&to, table()),
                "test",
            )
            .expect("the closing read keeps the full logical plan");
        }
    }
}
