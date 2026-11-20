//! #2963: PostgreSQL allows a table with no column, so a plan that replaces
//! every column of a table keeps the order it has always had, the drop first,
//! and the other order gives the same table (DEC-2963.1). The engine is why the
//! order is left alone, and this pins both halves of that.

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
    s.tables.insert(TableName::new("public", "t"), t);
    s
}

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

/// The result of `migration`, run as the differ wrote it or with its column
/// changes in the other order, on a table of two rows: the schema read back,
/// the rows and the column positions the engine kept.
async fn run(tag: &str, reversed: bool) -> (Schema, i32, String, Vec<String>) {
    let pg = Postgres::new();
    let base = schema_of(table(&[("a", "integer")]));
    let declared = schema_of(table(&[("b", "integer")]));
    let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
    let declared_ids = mint_ids(
        &declared,
        &base_ids,
        &[Intent::DropColumn {
            column: TableName::new("public", "t").column("a"),
            reason: "gone".parse().unwrap(),
        }],
    );
    let mut db = TestDb::create(tag).await;
    on_public(&mut db.conn).await;
    apply(
        &mut db.conn,
        &pg,
        &plan(&Schema::default(), &IdsFile::default(), &base, &base_ids),
    )
    .await;
    db.conn
        .execute("INSERT INTO public.t VALUES (1), (2)")
        .await
        .expect("two rows");
    let state = pbps_pg::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect the baseline");
    let mut migration = plan(&state.schema, &base_ids, &declared, &declared_ids);
    let order = steps(&migration);
    if reversed {
        migration.changes.reverse();
    } else {
        assert_eq!(order, ["-a", "+b"], "the order PostgreSQL has always had");
    }
    apply(&mut db.conn, &pg, &migration).await;
    let after = pbps_pg::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect the result");
    let ours = ours_only(&after, "public");
    let rows = number(&mut db.conn, "SELECT count(*)::int FROM public.t").await;
    let positions = text(
        &mut db.conn,
        "SELECT string_agg(attname || ':' || attnum, ',' ORDER BY attnum) \
         FROM pg_attribute WHERE attrelid = 'public.t'::regclass AND attnum > 0 \
         AND NOT attisdropped",
    )
    .await;
    db.drop().await;
    (ours, rows, positions, order)
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn replacing_the_only_column_keeps_the_drop_first_and_applies() {
    let (ours, rows, positions, _) = run("last_column2963_drop_first", false).await;
    let declared = schema_of(table(&[("b", "integer")]));
    assert_eq!(ours, normalized(&declared));
    assert_eq!(rows, 2, "the rows stay");
    assert_eq!(positions, "b:2", "the new column is the second slot");
}

/// Why the order can stay: the add-first order is observably the same table.
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn adding_first_gives_postgresql_the_same_table_so_nothing_asks_for_it() {
    let first = run("last_column2963_add_first", true).await;
    let second = run("last_column2963_drop_first_again", false).await;
    assert_eq!((first.0, first.1, first.2), (second.0, second.1, second.2));
}
