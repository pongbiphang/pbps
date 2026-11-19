//! The `PlanDialect` surface (#2950): the guard a drop carries, the two plan
//! modes, and the counts the scheduler asks for.

use std::collections::BTreeMap;

use pbps_dialect::Dialect;
use pbps_model::{DropGuard, Strategy, TableChange};

use super::golden::{add_column, create_table_change, uid};
use super::*;

fn drop_table() -> Change {
    Change::Table(TableChange::DropTable {
        uid: uid(UidKind::Table, "t"),
        name: tn("t"),
        detach_from: None,
    })
}

fn guard() -> DropGuard {
    DropGuard {
        table_uid: uid(UidKind::Table, "t"),
        table: tn("t"),
        detach_from: None,
        requirements: BTreeMap::new(),
    }
}

fn lowered(
    change: Change,
    guard: Option<DropGuard>,
    mode: PlanMode,
) -> Result<Vec<Statement>, DialectError> {
    let d = Sqlite::default();
    let mut planned = PlannedChange::new(change);
    planned.drop_guard = guard;
    let ordered = pbps_diff::order::singleton(planned, mode, &d);
    lower(&ordered as &dyn PlanView<Sqlite>, 0, &d).map(|l| l.into_statements())
}

#[test]
fn a_guarded_drop_carries_its_guard_on_the_statement_that_drops() {
    let out = lowered(drop_table(), Some(guard()), PlanMode::Transactional).expect("emits");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].drop_guard, Some(guard()));
    assert_eq!(
        lowered(drop_table(), None, PlanMode::Transactional).expect("emits")[0].drop_guard,
        None
    );
}

#[test]
fn a_guarded_drop_is_refused_in_a_staged_plan() {
    let err = lowered(drop_table(), Some(guard()), PlanMode::Staged).expect_err("refused");
    assert!(err.to_string().contains("transactional"), "{err}");
    // The same drop without a guard is allowed staged: SQLite has no statement
    // that must stand alone, so the two modes write the same text.
    let staged = lowered(drop_table(), None, PlanMode::Staged).expect("emits");
    let whole = lowered(drop_table(), None, PlanMode::Transactional).expect("emits");
    assert_eq!(sql_text(&staged), sql_text(&whole));
}

#[test]
fn every_statement_runs_in_the_plan_transaction() {
    for change in [
        create_table_change("t", &table(&[("a", col("integer"))])),
        add_column("t", "x", col("text")),
        drop_table(),
    ] {
        for statement in lowered(change, None, PlanMode::Transactional).expect("emits") {
            assert!(statement.transactional, "{}", statement.sql);
            assert!(!statement.own_batch, "{}", statement.sql);
        }
    }
}

#[test]
fn the_counts_the_scheduler_asks_for_are_the_statements_written() {
    let d = Sqlite::default();
    let mut rich = table(&[("a", col("integer")), ("b", col("integer"))]);
    rich.indexes
        .insert("i".into(), super::golden::index(&["a"], false, None));
    rich.indexes
        .insert("j".into(), super::golden::index(&["b"], false, None));
    for (what, change, expected) in [
        (
            "a table with two indexes",
            create_table_change("t", &rich),
            3,
        ),
        ("a drop", drop_table(), 1),
        ("an added column", add_column("t", "x", col("text")), 1),
    ] {
        for mode in [PlanMode::Transactional, PlanMode::Staged] {
            assert_eq!(
                d.statement_count(&change, Strategy::default(), mode),
                Ok(expected),
                "{what}"
            );
        }
        assert_eq!(
            d.intermediate_tables(&change, Strategy::default()),
            Ok(Vec::new()),
            "{what}"
        );
    }
}

fn child(parent: &str) -> Table {
    let mut t = table(&[("id", col("integer")), ("owner", col("integer"))]);
    t.foreign_keys.insert(
        pbps_model::ObjectLocalName::new("f_owner"),
        pbps_model::ForeignKey {
            columns: vec!["owner".into()],
            references_table: tn(parent),
            references_columns: vec!["id".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    t
}

fn parent() -> Table {
    let mut t = table(&[("id", col("integer"))]);
    t.primary_key = Some(pbps_model::PrimaryKey {
        name: None,
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    });
    t
}

/// Plans `base` to `declared` on the differ's route, runs it on `base_sql` and
/// compares with `declared_sql` built directly.
fn reaches(base: &Schema, base_sql: &str, declared: &Schema, declared_sql: &str) -> Vec<String> {
    let d = Sqlite::default();
    let out = plan((base, &ids(base, &[])), (declared, &ids(declared, &[])), &d).expect("plans");
    let mut db = built(base_sql);
    run(db.connection(), &out).unwrap_or_else(|e| panic!("{e}\nplan: {:#?}", sql_text(&out)));
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built(declared_sql)),
        "the plan reaches the declared schema",
    );
    sql_text(&out).into_iter().map(str::to_owned).collect()
}

const PARENT: &str = "CREATE TABLE p(id integer, PRIMARY KEY (id))";
const CHILD: &str = "CREATE TABLE c(id integer, owner integer, \
                     CONSTRAINT f_owner FOREIGN KEY (owner) REFERENCES p (id))";

/// The differ plans a new table's foreign key as its own change. SQLite cannot
/// add one, so it is written in the table's `CREATE`, through the real route.
#[test]
fn a_new_tables_foreign_key_is_written_in_its_create_on_the_planning_route() {
    let base = schema(&[("p", parent())], &[]);
    let declared = schema(&[("p", parent()), ("c", child("p"))], &[]);
    let out = reaches(&base, PARENT, &declared, &[PARENT, CHILD].join(";"));
    assert_eq!(out.len(), 1, "{out:#?}");
    assert!(out[0].contains("FOREIGN KEY"), "{out:#?}");
    // Both new at once: a `CREATE` may reference a table that is not there
    // yet, so the order of the two does not matter (measured below).
    let out = reaches(
        &Schema::default(),
        "",
        &declared,
        &[PARENT, CHILD].join(";"),
    );
    assert_eq!(out.len(), 2, "{out:#?}");
    // The key is never counted as a statement of its own.
    let key = Change::Constraint(pbps_model::ConstraintChange::AddForeignKey {
        table: tn("c"),
        name: pbps_model::ObjectLocalName::new("f_owner"),
        constraint: Box::new(child("p").foreign_keys.into_values().next().expect("one")),
    });
    assert_eq!(
        pbps_dialect::Dialect::statement_count(
            &Sqlite::default(),
            &key,
            Default::default(),
            pbps_model::PlanMode::Transactional
        ),
        Ok(0)
    );
    let mut db = built(&["CREATE TABLE c(owner integer REFERENCES p (id))", PARENT].join(";"));
    db.connection()
        .execute_batch(
            "PRAGMA foreign_keys = ON; INSERT INTO p VALUES (1); INSERT INTO c VALUES (1)",
        )
        .expect("the child was created before its parent and the key holds");
}

/// Negative: a key added to a table the plan does not create is the rebuild's,
/// and never an empty success.
#[test]
fn a_foreign_key_added_to_a_standing_table_is_still_refused() {
    let plain = table(&[("id", col("integer")), ("owner", col("integer"))]);
    let base = schema(&[("p", parent()), ("c", plain)], &[]);
    let declared = schema(&[("p", parent()), ("c", child("p"))], &[]);
    let d = Sqlite::default();
    let error = plan(
        (&base, &ids(&base, &[])),
        (&declared, &ids(&declared, &[])),
        &d,
    )
    .expect_err("refused");
    assert!(error.contains("rebuild"), "{error}");
}
