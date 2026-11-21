//! A line is named as the revision that wrote it spelled it (DEC-2469.10): a
//! table that keeps its identity across a rename is `app.old` in the
//! baseline's lines and `app.t` in the goal's.
use super::*;
use pbps_model::ModuleChange;

#[test]
fn a_line_is_named_as_the_revision_that_wrote_it_spelled_it() {
    let mut f = Fixture::new();
    // The baseline holds the table as `app.old`; the goal renames it.
    let mut before = Schema::default();
    let table = f.after.tables[&"app.t".parse().unwrap()].clone();
    before.tables.insert("app.old".parse().unwrap(), table);
    before.modules.insert("app.f".parse().unwrap(), view());
    f.before = before;
    f.base = IdsFile {
        tables: [("t_aaaaaa".parse().unwrap(), "app.old".parse().unwrap())].into(),
        columns: [("c_aaaaaa".parse().unwrap(), "app.old.x".parse().unwrap())].into(),
        ..Default::default()
    };
    f.after.tables = [(
        "app.t".parse().unwrap(),
        f.after.tables[&"app.t".parse().unwrap()].clone(),
    )]
    .into();
    let changes = vec![
        Change::Module(ModuleChange::DropModule {
            id: "app.f".parse().unwrap(),
            kind: ModuleKind::View,
        }),
        Change::Module(ModuleChange::CreateModule {
            id: "app.g".parse().unwrap(),
            module: Box::new(view()),
        }),
        Change::Table(TableChange::DropTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: "app.old".parse().unwrap(),
            detach_from: None,
        }),
        Change::Table(TableChange::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: "app.t".parse().unwrap(),
            table: Box::new(f.after.tables[&"app.t".parse().unwrap()].clone()),
            beside_default: None,
            moves_at_most: None,
        }),
    ];
    let base = dependencies(&[("app.f", &["table::app.old"])]);
    let goal = dependencies(&[("app.g", &["table::app.t"])]);
    f.after.modules.clear();
    f.after.modules.insert("app.g".parse().unwrap(), view());
    let plan = f.plan(changes, &base, &goal).unwrap();
    let (_, lines) = plan.annotations.as_ref().unwrap().settling(&plan).unwrap();
    // The baseline's line removes the view before the table it named `app.old`.
    assert_eq!(
        lines[&(0, 2)].iter().collect::<Vec<_>>(),
        ["annotation `module::app.f` depends on `table::app.old`"]
    );
    // Negative: the goal's line over the same identity keeps the goal's name.
    assert_eq!(
        lines[&(3, 1)].iter().collect::<Vec<_>>(),
        ["annotation `module::app.g` depends on `table::app.t`"]
    );
}
