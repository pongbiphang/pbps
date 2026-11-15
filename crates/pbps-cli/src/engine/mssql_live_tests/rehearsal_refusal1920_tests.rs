//! The convergence comparison of a rehearsal, against a real SQL Server (#1920).
//!
//! No project the CLI can load makes the post-run comparison refuse: the plan is
//! the differ's own output, so the same differ has already refused what it could
//! not express, and the plan-time refusal comes before any engine is started.
//! What a refusal there reports is a plan whose statements did not carry out a
//! change the declarations make, which is the defect a rehearsal exists to find.
//! So the plan is handed in short, to the production entry `rehearse`.
//!
//! It sits with the other SQL Server live tests of this binary because those are the
//! ones the live job runs by name.

use crate::db;
use crate::dev::{Spec, rehearse};
use pbps_diff::Side;
use pbps_model::{IdsFile, Schema};

/// Two tables whose computed column `z` reads the column `x`, typed as given:
/// SQL Server refuses to retype an input while a computed column stands.
fn project(x: &str) -> (Schema, pbps_model::Hints, IdsFile) {
    let files: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|name| {
            (
                std::path::PathBuf::from(format!("schema/dbo.{name}.yml")),
                format!(
                    "table: dbo.{name}\ncolumns:\n  id: {{type: int, nullable: false}}\n  \
                     x: {{type: {x}}}\n  y: {{type: int}}\ncomputed:\n  \
                     z: {{expression: \"[x]+[y]\"}}\n"
                ),
            )
        })
        .collect();
    let loaded = pbps_load::load_schema_files(&files).unwrap();
    let mut ids = IdsFile::default();
    for (name, table) in &loaded.schema.tables {
        ids.tables.insert(
            pbps_model::Uid::generate(pbps_model::uid::UidKind::Table),
            name.clone(),
        );
        for column in table.columns.keys() {
            ids.columns.insert(
                pbps_model::Uid::generate(pbps_model::uid::UidKind::Column),
                name.column(column),
            );
        }
    }
    (loaded.schema, loaded.hints, ids)
}

async fn databases(url: &str) -> std::collections::BTreeSet<String> {
    let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Mssql, url)
        .await
        .unwrap();
    conn.query("SELECT name FROM sys.databases")
        .await
        .unwrap()
        .iter()
        .map(|row| row.try_get::<&str>("name").unwrap().unwrap().to_owned())
        .collect()
}

#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_convergence_refusal_keeps_every_reason_once_and_the_scratch_is_released() {
    let url = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let selected = pbps_mssql::Mssql;
    let (baseline, baseline_hints, ids) = project("int");
    let (declared, hints, _) = project("bigint");
    // The plan the declarations call for is a retype of `x` on each table.
    // What is handed in is the plan that leaves both out.
    let side = || Side {
        schema: &baseline,
        ids: &ids,
    };
    let (short, _) =
        crate::rehearsal::lower_between(&selected, side(), side(), &baseline_hints).unwrap();
    let run = |declared: &Schema| {
        rehearse(
            &Spec::Connection(url.clone()),
            &baseline,
            &ids,
            &baseline_hints,
            declared,
            &ids,
            &short,
            &selected,
            &hints,
        )
    };
    let before = db::runtime().unwrap().block_on(databases(&url));

    let error = run(&declared).err().expect("the plan leaves both inputs");
    let text = format!("{error:#}");
    // Both tables' refusals, each once and in the differ's own words.
    for table in ["dbo.a", "dbo.b"] {
        let reason = format!("computed column {table}.z may read `x`");
        assert_eq!(text.matches(&reason).count(), 1, "{table}: {text}");
    }
    assert_eq!(
        text.matches("the rehearsal produced inexpressible differences")
            .count(),
        1,
        "{text}"
    );
    // One finding for the envelope, whose text is the human line's.
    let findings = crate::output::failure_findings(&error, "rehearsal.unavailable", vec![]);
    assert_eq!(findings.len(), 1, "{text}");
    assert_eq!(findings[0].id, "rehearsal.unavailable");
    assert_eq!(findings[0].message, text);
    let after = db::runtime().unwrap().block_on(databases(&url));
    assert_eq!(before, after, "the scratch outlived the refusal");

    // Negative: a plan that has nothing left to do prints no refusal.
    let converged = run(&baseline).expect("nothing differs");
    assert!(converged.converged(), "{:?}", converged.structural);
    let after = db::runtime().unwrap().block_on(databases(&url));
    assert_eq!(before, after, "the scratch outlived the run");
}
