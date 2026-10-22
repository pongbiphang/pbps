//! Lowering an operation on PostgreSQL (#1961): the statements its change
//! emits, grouped under it, with the emitter's own facts about running them.
//! Lowering is `emit` regrouped, so a plan's statements are byte-identical
//! whether they are read per change or per operation.

use pbps_dialect::Dialect;
use pbps_dialect::Released;
use pbps_dialect::lower::{Batch, Execution, Lowered, Transaction, lower};
use pbps_model::{
    Change, Column, Index, IndexColumn, IndexKey, Module, ModuleId, ModuleKind, PlannedChange,
    Strategy, Table, TableName,
};
use pbps_pg::Postgres;

fn t(s: &str) -> TableName {
    s.parse().unwrap()
}

fn one(change: Change, strategy: Strategy) -> Lowered {
    let plan = pbps_diff::order::singleton(
        planned(change, strategy),
        pbps_model::PlanMode::Transactional,
        &Postgres::new(),
    );
    lower(
        &plan as &dyn pbps_diff::order::PlanView<pbps_pg::Postgres>,
        0,
        &Postgres::new(),
    )
    .unwrap()
}

fn planned(change: Change, strategy: Strategy) -> PlannedChange {
    let mut p = PlannedChange::new(change);
    p.strategy = strategy;
    p
}

fn rename(from: &str, to: &str) -> Change {
    Change::RenameTable {
        uid: "t_aaaaaa".parse().unwrap(),
        from: t(from),
        to: t(to),
        defaults: vec![],
    }
}

fn index(key: IndexKey, filter: Option<&str>) -> Change {
    Change::AddIndex {
        table: t("app.t"),
        name: "ix".into(),
        index: Box::new(Index {
            columns: vec![IndexColumn {
                key,
                descending: false,
                opclass: None,
            }],
            include: vec![],
            unique: false,
            filter: filter.map(str::to_owned),
            method: Default::default(),
            storage_parameters: Default::default(),
        }),
        clustered: false,
    }
}

fn function() -> (ModuleId, Module) {
    (
        "app.f(integer)".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "(a integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN a".into(),
        },
    )
}

const ONLINE: Strategy = Strategy { online: true };

/// A move to another schema is `SET SCHEMA`, then `RENAME TO`: one
/// operation, two statements, and between them the table is under a name
/// neither the baseline nor the plan holds, which the lowering shows.
#[test]
fn a_cross_schema_rename_lowers_to_two_statements_and_shows_the_name_between() {
    let moved = one(rename("app.a", "dst.b"), Strategy::default());
    assert_eq!(moved.statements().len(), 2, "{moved:?}");
    assert_eq!(moved.intermediate_tables(), [&t("dst.a")]);
    assert!(moved.steps().all(
        |s| s.operation == moved.operation() && s.execution.transaction == Transaction::Inside
    ));
    // Negative: a rename within its schema, or a move that keeps the name,
    // is one statement with nothing between.
    for (from, to) in [("app.a", "app.b"), ("app.a", "dst.a")] {
        let plain = one(rename(from, to), Strategy::default());
        assert_eq!(plain.statements().len(), 1, "{plain:?}");
        assert!(plain.intermediate_tables().is_empty(), "{plain:?}");
    }
}

/// `CREATE INDEX CONCURRENTLY` cannot run in a transaction block, and it is
/// sent alone: both requirements, from the strategy the operation is lowered
/// under.
#[test]
fn a_concurrent_index_build_runs_alone_and_outside_the_transaction() {
    let column = || index(IndexKey::Column("n".into()), None);
    let online = one(column(), ONLINE);
    let built: Vec<_> = online
        .steps()
        .filter(|s| s.statement.index_build.is_some())
        .collect();
    assert_eq!(built.len(), 1, "{online:?}");
    assert!(
        built[0].statement.sql.contains("CONCURRENTLY"),
        "{online:?}"
    );
    assert_eq!(
        built[0].execution,
        Execution {
            transaction: Transaction::Outside,
            batch: Batch::Own,
        }
    );
    // Negative: the same change offline, and an online request the engine
    // builds the ordinary way (a filter, an expression key), stay inside
    // the transaction.
    for lowered in [
        one(column(), Strategy::default()),
        one(index(IndexKey::Column("n".into()), Some("n > 0")), ONLINE),
        one(index(IndexKey::Expression("lower(m)".into()), None), ONLINE),
    ] {
        assert!(
            lowered
                .steps()
                .all(|s| s.execution.transaction == Transaction::Inside),
            "{lowered:?}"
        );
        assert!(
            !lowered
                .statements()
                .iter()
                .any(|s| s.sql.contains("CONCURRENTLY"))
        );
    }
}

/// A module change is a `DROP` and a `CREATE`, one indivisible group, and
/// the `DROP` says which module it releases: the name the create then takes
/// back (ADR-0009 §3).
#[test]
fn a_module_change_lowers_to_a_drop_that_releases_it_and_a_create() {
    let (id, module) = function();
    let altered = one(
        Change::AlterModule {
            id: id.clone(),
            module: Box::new(module.clone()),
        },
        Strategy::default(),
    );
    assert_eq!(altered.statements().len(), 2, "{altered:?}");
    assert_eq!(
        altered.releases().collect::<Vec<_>>(),
        [(0, &Released::Module(id.clone()))]
    );
    let dropped = one(
        Change::DropModule {
            id: id.clone(),
            kind: ModuleKind::Function,
        },
        Strategy::default(),
    );
    assert_eq!(
        dropped.releases().collect::<Vec<_>>(),
        [(0, &Released::Module(id.clone()))]
    );
    // Negative: a create releases nothing.
    let created = one(
        Change::CreateModule {
            id,
            module: Box::new(module),
        },
        Strategy::default(),
    );
    assert_eq!(created.releases().count(), 0);
}

/// A plan of many kinds of change, some under the online strategy.
fn corpus() -> Vec<PlannedChange> {
    let (id, module) = function();
    let mut table = Table::default();
    table
        .columns
        .insert("n".into(), Column::new("int".parse().unwrap()));
    vec![
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
        planned(index(IndexKey::Column("n".into()), None), ONLINE),
        planned(index(IndexKey::Column("n".into()), Some("n > 0")), ONLINE),
        planned(rename("app.a", "dst.b"), Strategy::default()),
        planned(rename("app.c", "app.d"), Strategy::default()),
        planned(
            Change::AlterModule {
                id: id.clone(),
                module: Box::new(module),
            },
            Strategy::default(),
        ),
        planned(
            Change::DropModule {
                id,
                kind: ModuleKind::Function,
            },
            Strategy::default(),
        ),
        planned(
            Change::DropTable {
                uid: "t_cccccc".parse().unwrap(),
                name: t("app.gone"),
                detach_from: None,
            },
            Strategy::default(),
        ),
    ]
}

/// Lowering every operation gives, statement for statement, what emitting
/// every change under its own strategy gives: the layer changes no byte of
/// a plan's statements.
#[test]
fn lowering_a_plan_writes_exactly_the_statements_emit_writes() {
    let pg = Postgres::new();
    let changes = corpus();
    let emitted: Vec<_> = changes
        .iter()
        .flat_map(|p| pg.legacy_emit(&p.change, p.strategy).unwrap())
        .collect();
    let lowered: Vec<_> = changes
        .iter()
        .flat_map(|p| {
            let plan = pbps_diff::order::singleton(
                p.clone(),
                pbps_model::PlanMode::Transactional,
                &Postgres::new(),
            );
            pbps_dialect::lower::lower_all(
                &plan as &dyn pbps_diff::order::PlanView<pbps_pg::Postgres>,
                &pg,
            )
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
