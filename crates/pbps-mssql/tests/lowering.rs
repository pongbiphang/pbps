//! Lowering an operation on SQL Server (#1961): the statements its change
//! emits, grouped under it, with the emitter's own facts about running them.
//! Lowering is `emit` regrouped, so a plan's statements are byte-identical
//! whether they are read per change or per operation.

#[cfg(test)]
use crate::certified_fixture::FixtureEmission as _;

use pbps_dialect::Released;
use pbps_dialect::lower::{Batch, Lowered, Transaction, lower};
use pbps_model::{
    Change, Column, Index, IndexColumn, IndexKey, Module, ModuleId, ModuleKind, PlannedChange,
    Strategy, Table, TableName,
};
use pbps_mssql::Mssql;

fn t(s: &str) -> TableName {
    s.parse().unwrap()
}

fn planned(change: Change, strategy: Strategy) -> PlannedChange {
    let mut p = PlannedChange::new(change);
    p.strategy = strategy;
    p
}

fn one(change: Change, strategy: Strategy) -> Lowered {
    let plan = pbps_diff::order::singleton(
        planned(change, strategy),
        pbps_model::PlanMode::Transactional,
        &Mssql,
    );
    lower(&plan as &dyn pbps_diff::order::PlanView<Mssql>, 0, &Mssql).unwrap()
}

fn rename(from: &str, to: &str) -> Change {
    Change::RenameTable {
        uid: "t_aaaaaa".parse().unwrap(),
        from: t(from),
        to: t(to),
        defaults: vec![],
    }
}

fn view() -> (ModuleId, Module) {
    (
        "app.v".parse().unwrap(),
        Module {
            kind: ModuleKind::View,
            description: None,
            definition: "SELECT 1 AS x".into(),
        },
    )
}

const ONLINE: Strategy = Strategy { online: true };

/// A move to another schema is `ALTER SCHEMA ... TRANSFER`, then
/// `sp_rename`: one operation, two statements, and between them the table
/// is under a name neither the baseline nor the plan holds.
#[test]
fn a_cross_schema_rename_lowers_to_two_statements_and_shows_the_name_between() {
    let moved = one(rename("dbo.old_name", "app.new_name"), Strategy::default());
    assert_eq!(moved.statements().len(), 2, "{moved:?}");
    assert_eq!(moved.intermediate_tables(), [&t("app.old_name")]);
    // Negative: a rename within its schema is one statement, nothing between.
    let plain = one(rename("dbo.old_name", "dbo.new_name"), Strategy::default());
    assert_eq!(plain.statements().len(), 1, "{plain:?}");
    assert!(plain.intermediate_tables().is_empty());
}

/// A batch of its own is not a step outside the transaction: `CREATE VIEW`
/// must start its batch, and it still runs inside the plan's transaction.
/// Here the two requirements part, which is why they are two.
#[test]
fn a_statement_in_its_own_batch_stays_inside_the_transaction() {
    let (id, module) = view();
    let created = one(
        Change::CreateModule {
            id,
            module: Box::new(module),
        },
        Strategy::default(),
    );
    let steps: Vec<_> = created.steps().map(|s| s.execution).collect();
    assert_eq!(steps.len(), 1, "{created:?}");
    assert_eq!(steps[0].batch, Batch::Own);
    assert_eq!(steps[0].transaction, Transaction::Inside);
    // Negative: an online index build is neither; SQL Server's `ONLINE = ON`
    // is an option of an ordinary transactional statement.
    let online = one(
        Change::AddIndex {
            table: t("app.t"),
            name: "ix".into(),
            index: Box::new(Index {
                columns: vec![IndexColumn {
                    key: IndexKey::Column("n".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            }),
            clustered: false,
        },
        ONLINE,
    );
    assert!(
        online
            .steps()
            .all(|s| s.execution.transaction == Transaction::Inside),
        "{online:?}"
    );
}

/// A dropped module says which module it releases; a created one releases
/// nothing.
#[test]
fn a_module_drop_releases_the_module_and_a_create_does_not() {
    let (id, module) = view();
    let dropped = one(
        Change::DropModule {
            id: id.clone(),
            kind: ModuleKind::View,
        },
        Strategy::default(),
    );
    assert_eq!(
        dropped.releases().collect::<Vec<_>>(),
        [(0, &Released::Module(id.clone()))]
    );
    let altered = one(
        Change::AlterModule {
            id,
            module: Box::new(module),
        },
        Strategy::default(),
    );
    // `CREATE OR ALTER` keeps the object: nothing is released.
    assert_eq!(altered.releases().count(), 0, "{altered:?}");
}

/// Lowering every operation gives, statement for statement, what emitting
/// every change under its own strategy gives.
#[test]
fn lowering_a_plan_writes_exactly_the_statements_emit_writes() {
    let (id, module) = view();
    let mut table = Table::default();
    table
        .columns
        .insert("n".into(), Column::new("int".parse().unwrap()));
    let index = Change::AddIndex {
        table: t("app.t"),
        name: "ix".into(),
        index: Box::new(Index {
            columns: vec![IndexColumn {
                key: IndexKey::Column("n".into()),
                descending: false,
                opclass: None,
            }],
            include: vec![],
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        }),
        clustered: false,
    };
    let changes = [
        planned(
            Change::CreateTable {
                uid: "t_bbbbbb".parse().unwrap(),
                name: t("app.t"),
                table: Box::new(table),
                beside_default: None,
                moves_at_most: None,
            },
            Strategy::default(),
        ),
        planned(index, ONLINE),
        planned(rename("dbo.a", "app.b"), Strategy::default()),
        planned(
            Change::CreateModule {
                id: id.clone(),
                module: Box::new(module),
            },
            Strategy::default(),
        ),
        planned(
            Change::DropModule {
                id,
                kind: ModuleKind::View,
            },
            Strategy::default(),
        ),
    ];
    let emitted: Vec<_> = changes
        .iter()
        .flat_map(|p| Mssql.fixture_emit(&p.change, p.strategy).unwrap())
        .collect();
    let lowered: Vec<_> = changes
        .iter()
        .flat_map(|p| {
            let plan =
                pbps_diff::order::singleton(p.clone(), pbps_model::PlanMode::Transactional, &Mssql);
            pbps_dialect::lower::lower_all(&plan as &dyn pbps_diff::order::PlanView<Mssql>, &Mssql)
                .unwrap()
                .statements()
                .to_vec()
        })
        .collect();
    assert_eq!(lowered, emitted);
    // Negative: the comparison sees a strategy. Lowered offline, the online
    // index is another statement.
    let offline: Vec<_> = changes
        .iter()
        .flat_map(|p| one(p.change.clone(), Strategy::default()).into_statements())
        .collect();
    assert_ne!(offline, emitted);
}

#[cfg(test)]
mod certified_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pbps-diff/tests/support/emission.rs"
    ));
}
