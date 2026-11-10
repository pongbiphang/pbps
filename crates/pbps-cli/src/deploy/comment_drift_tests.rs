use super::*;

/// A comment the database holds and the record does not is no drift; a comment on only
/// one side, in either direction, is gone from the comparison. Negative: a column that
/// differs still does.
#[test]
fn a_comment_on_one_side_only_is_not_a_difference_and_a_column_still_is() {
    let table = TableName::new("app", "t");
    let mut recorded = Schema::default();
    let mut shape = pbps_model::Table::default();
    shape.columns.insert(
        "id".into(),
        pbps_model::Column::new("integer".parse().unwrap()),
    );
    recorded.tables.insert(table.clone(), shape);
    let mut live = recorded.clone();
    live.tables.get_mut(&table).unwrap().description = Some("ops note".into());
    live.tables.get_mut(&table).unwrap().columns["id"].description = Some("also".into());
    assert_ne!(recorded, live);
    let (mut was, mut now) = (recorded.clone(), live.clone());
    without_comments(&mut was, &mut now);
    assert_eq!(was, now);
    let (mut was, mut now) = (live.clone(), recorded.clone());
    without_comments(&mut was, &mut now);
    assert_eq!(was, now);
    live.tables.get_mut(&table).unwrap().columns["id"].ty = "bigint".parse().unwrap();
    let (mut was, mut now) = (recorded, live);
    without_comments(&mut was, &mut now);
    assert_ne!(was, now);
}
