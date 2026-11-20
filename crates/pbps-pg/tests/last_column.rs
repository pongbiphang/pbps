//! PostgreSQL allows a table with no column, so replacing every column of a
//! table keeps the order it has always had: the drop first (#2963, DEC-2963.1).
//! The order that adds first is for the engines that refuse the last column.

use pbps_dialect::Dialect;
use pbps_diff::{Context, DiffError, Side, diff, resolve};
use pbps_model::{Change, Column, ColumnChange, Hints, IdsFile, Intent, Schema, Table};
use pbps_pg::Postgres;

fn schema(columns: &[&str]) -> Schema {
    let mut table = Table::default();
    for column in columns {
        table
            .columns
            .insert((*column).into(), Column::new("integer".parse().unwrap()));
    }
    let mut schema = Schema::default();
    schema.tables.insert("app.t".parse().unwrap(), table);
    schema
}

/// The column changes of the plan from `was` to `now`, as `+x` and `-x`.
fn plan(was: &[&str], now: &[&str]) -> Result<Vec<String>, Vec<DiffError>> {
    let ctx = Context {
        operator: "operator".into(),
        today: "2026-11-20".into(),
    };
    let (base, declared) = (schema(was), schema(now));
    let base_ids = resolve(&base, &IdsFile::default(), &[], &ctx).unwrap().ids;
    let intents: Vec<Intent> = was
        .iter()
        .filter(|column| !now.contains(column))
        .map(|column| Intent::DropColumn {
            column: format!("app.t.{column}").parse().unwrap(),
            reason: "gone".parse().unwrap(),
        })
        .collect();
    let declared_ids = resolve(&declared, &base_ids, &intents, &ctx).unwrap().ids;
    let planned = diff(
        Side {
            schema: &base,
            ids: &base_ids,
        },
        Side {
            schema: &declared,
            ids: &declared_ids,
        },
        &Postgres::new(),
        &Hints::default(),
    )?;
    Ok(planned
        .changes()
        .changes
        .iter()
        .filter_map(|p| {
            if let Change::Column(ColumnChange::AddColumn { name, .. }) = &p.change {
                Some(["+", name.raw()].concat())
            } else if let Change::Column(ColumnChange::DropColumn { column, .. }) = &p.change {
                Some(["-", column.name.raw()].concat())
            } else {
                None
            }
        })
        .collect())
}

#[test]
fn postgresql_does_not_refuse_the_last_column() {
    assert!(!Postgres::new().refuses_dropping_last_column());
}

#[test]
fn replacing_the_only_column_still_drops_before_it_adds() {
    assert_eq!(plan(&["a"], &["b"]).unwrap(), ["-a", "+b"]);
}

#[test]
fn replacing_every_column_still_drops_every_column_before_it_adds_any() {
    let steps = plan(&["a", "b"], &["c", "d"]).unwrap();
    assert!(steps[..2].iter().all(|s| s.starts_with('-')), "{steps:?}");
    assert_eq!(steps[2..], ["+c", "+d"]);
}

#[test]
fn a_table_that_keeps_a_column_is_planned_as_before() {
    let steps = plan(&["a", "b"], &["b", "c"]).unwrap();
    assert_eq!(steps, ["-a", "+c"]);
}
