//! #2963: SQL Server will not drop a table's only data column (Msg 4923), so a
//! plan that replaces every column of a table adds before it drops the last
//! one. Each case runs the plan the differ writes on the engine, the old order
//! on the same baseline where it matters, and reads the catalog back.

use super::*;

fn table(columns: &[(&str, &str)]) -> Table {
    let mut t = Table::default();
    for (name, spec) in columns {
        t.columns.insert((*name).into(), Column::new(ty(spec)));
    }
    t
}

fn schema_of(t: Table) -> Schema {
    let mut s = Schema::default();
    s.tables.insert(TableName::new("dbo", "t"), t);
    s
}

/// The column changes of a plan as `+x` and `-x`, in plan order.
fn steps(plan: &pbps_model::ChangeSet) -> Vec<String> {
    plan.changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::Column(pbps_model::ColumnChange::AddColumn {
                name, ..
            }) = &p.change
            {
                Some(["+", name.raw()].concat())
            } else if let pbps_model::Change::Column(pbps_model::ColumnChange::DropColumn {
                column,
                ..
            }) = &p.change
            {
                Some(["-", column.name.raw()].concat())
            } else {
                None
            }
        })
        .collect()
}

async fn rows_of_t(db: &mut TestDb) -> i64 {
    db.conn
        .query("SELECT CONVERT(bigint, COUNT(*)) FROM dbo.t")
        .await
        .expect("count")[0]
        .try_get_at::<i64>(0)
        .unwrap()
        .unwrap()
}

/// The baseline built by the plan from nothing, two rows in it, and the plan
/// from it to `declared` with the identity the differ keeps.
async fn setup(
    db: &mut TestDb,
    base: &Schema,
    declared: &Schema,
    renewed: &[&str],
) -> (Schema, pbps_model::ChangeSet) {
    let base_ids = mint_ids(base, &IdsFile::default(), &[]);
    apply(
        &mut db.conn,
        &plan(&Schema::default(), &IdsFile::default(), base, &base_ids),
    )
    .await;
    db.conn
        .execute("INSERT INTO dbo.t DEFAULT VALUES; INSERT INTO dbo.t DEFAULT VALUES")
        .await
        .expect("two rows");
    let intents: Vec<Intent> = base.tables[&TableName::new("dbo", "t")]
        .columns
        .keys()
        .filter(|c| {
            !declared.tables[&TableName::new("dbo", "t")]
                .columns
                .contains_key(*c)
        })
        .map(|c| Intent::DropColumn {
            column: TableName::new("dbo", "t").column(c.raw()),
            reason: "gone".parse().unwrap(),
        })
        .collect();
    let mut declared_ids = mint_ids(declared, &base_ids, &intents);
    // A new identity under an old name cannot come from a name match.
    for (n, name) in renewed.iter().enumerate() {
        let at = TableName::new("dbo", "t").column(*name);
        let old = declared_ids.column_uid(&at).cloned().unwrap();
        declared_ids.columns.remove(&old);
        declared_ids
            .columns
            .insert(format!("c_new{n:03}").parse().unwrap(), at);
        declared_ids.tombstones.insert(
            old,
            pbps_model::Tombstone {
                was: format!("dbo.t.{name}"),
                dropped_at: "2026-11-20".into(),
                reason: "gone".into(),
                operator: "live-test".into(),
                require: None,
            },
        );
    }
    let state = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect the baseline");
    let migration = plan(&state.schema, &base_ids, declared, &declared_ids);
    (state.schema, migration)
}

#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn the_only_column_is_replaced_by_adding_before_dropping_and_the_old_order_is_refused() {
    let base = schema_of(table(&[("a", "int")]));
    let declared = schema_of(table(&[("b", "nvarchar(20)")]));
    let mut db = TestDb::create("last_column2963_only").await;
    let (_, migration) = setup(&mut db, &base, &declared, &[]).await;
    assert_eq!(steps(&migration), ["+b", "-a"], "{migration:?}");

    // The order the differ used to write, refused by the engine and rolled
    // back: the evidence that the add-first order is the one that works.
    let mut old = migration.clone();
    old.changes.reverse();
    let refused = try_apply(&mut db.conn, &old)
        .await
        .expect_err("the engine must refuse the only data column's drop");
    assert!(refused.contains("only data column"), "{refused}");
    assert_eq!(rows_of_t(&mut db).await, 2, "the refusal changed nothing");

    apply(&mut db.conn, &migration).await;
    let after = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect the result");
    let rows = rows_of_t(&mut db).await;
    db.drop().await;
    assert_eq!(after.schema, normalized(&declared));
    assert_eq!(rows, 2, "the rows stay");
}

#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn replacing_two_columns_keeps_a_data_column_at_every_statement() {
    let base = schema_of(table(&[("a", "int"), ("b", "int")]));
    let declared = schema_of(table(&[("c", "int"), ("d", "nvarchar(20)")]));
    let mut db = TestDb::create("last_column2963_two").await;
    let (_, migration) = setup(&mut db, &base, &declared, &[]).await;
    let order = steps(&migration);
    assert_eq!(order.len(), 4, "{order:?}");
    assert!(
        order.iter().position(|s| s.starts_with('+')).unwrap()
            < order.iter().rposition(|s| s.starts_with('-')).unwrap(),
        "an add comes before the last drop: {order:?}"
    );
    try_apply(&mut db.conn, &migration)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let after = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect the result");
    db.drop().await;
    assert_eq!(after.schema, normalized(&declared));
}

#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_table_that_keeps_one_column_drops_first_as_it_always_did() {
    let base = schema_of(table(&[("a", "int"), ("k", "int")]));
    let declared = schema_of(table(&[("k", "int"), ("b", "int")]));
    let mut db = TestDb::create("last_column2963_keeps").await;
    let (_, migration) = setup(&mut db, &base, &declared, &[]).await;
    assert_eq!(steps(&migration), ["-a", "+b"], "{migration:?}");
    apply(&mut db.conn, &migration).await;
    let after = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect the result");
    db.drop().await;
    assert_eq!(after.schema, normalized(&declared));
}

/// Names that differ only by case are ordered add first, and the database's
/// collation decides (DEC-2963.1): a case-sensitive one keeps both names and
/// takes the plan; a case-insensitive one refuses the add with its own
/// duplicate-name error, atomically, and the table is as it was.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_name_that_differs_only_by_case_is_added_first_and_the_collation_decides() {
    let base = schema_of(table(&[("a", "int")]));
    let declared = schema_of(table(&[("A", "int")]));

    let mut sensitive = TestDb::create_collated("last_column2963_cs", "Latin1_General_CS_AS").await;
    let (_, migration) = setup(&mut sensitive, &base, &declared, &[]).await;
    assert_eq!(steps(&migration), ["+A", "-a"], "{migration:?}");
    apply(&mut sensitive.conn, &migration).await;
    let after = pbps_mssql::catalog::introspect(&mut sensitive.conn)
        .await
        .expect("introspect the result");
    sensitive.drop().await;
    assert_eq!(after.schema, normalized(&declared));

    let mut insensitive =
        TestDb::create_collated("last_column2963_ci", "SQL_Latin1_General_CP1_CI_AS").await;
    let (before, migration) = setup(&mut insensitive, &base, &declared, &[]).await;
    assert_eq!(steps(&migration), ["+A", "-a"], "{migration:?}");
    let refused = try_apply(&mut insensitive.conn, &migration)
        .await
        .expect_err("a case-insensitive database holds `a` and `A` as one name");
    assert!(refused.contains("must be unique"), "{refused}");
    let rows = rows_of_t(&mut insensitive).await;
    let unchanged = pbps_mssql::catalog::introspect(&mut insensitive.conn)
        .await
        .expect("introspect after the refusal");
    insensitive.drop().await;
    assert_eq!(unchanged.schema, before, "atomic: nothing changed");
    assert_eq!(rows, 2);
}
