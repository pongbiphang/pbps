use super::*;
use pbps_model::{CheckConstraint, Column, Hints, Module, ModuleId, ModuleKind, Table};

fn table() -> TableName {
    TableName::new("app", "t")
}

/// `app.t (id int [, <extra> int ...])`.
fn schema_of(tables: &[(&TableName, &[&str])]) -> Schema {
    let mut schema = Schema::default();
    for (name, columns) in tables {
        let mut shape = Table::default();
        for column in *columns {
            shape
                .columns
                .insert((*column).into(), Column::new("int".parse().unwrap()));
        }
        schema.tables.insert((*name).clone(), shape);
    }
    schema
}

fn one_table(columns: &[&str]) -> Schema {
    schema_of(&[(&table(), columns)])
}

/// The comparison as the command makes it: no identity file but what the
/// declarations give, on SQL Server's dialect.
fn compared(declared: &Schema, live: &Schema) -> Compared {
    compared_in(declared, &IdsFile::default(), live)
}

fn compared_in(declared: &Schema, project: &IdsFile, live: &Schema) -> Compared {
    compare(
        declared,
        &Hints::default(),
        &scope_ids(declared, project),
        live,
        None,
        &pbps_mssql::Mssql,
    )
}

fn with_check(mut schema: Schema, expression: &str) -> Schema {
    schema.tables.get_mut(&table()).unwrap().checks.insert(
        "ck_t_id".into(),
        CheckConstraint {
            expression: expression.into(),
        },
    );
    schema
}

fn view(definition: &str) -> Module {
    Module {
        kind: ModuleKind::View,
        description: None,
        definition: definition.into(),
        options: Default::default(),
    }
}

#[test]
fn a_database_that_holds_the_declarations_has_no_difference() {
    let declared = one_table(&["id", "name"]);
    let found = compared(&declared, &declared.clone());
    assert!(found.changes.is_empty(), "{:?}", found.changes);
    assert_eq!(found.text_differences, []);
    assert_eq!(found.unexpressible, Vec::<String>::new());
    assert!(!may_hold_a_rename(&found.changes));
}

/// With no identity file at all, a name on both sides is still one object:
/// the two sides must not each derive an identity of their own.
#[test]
fn a_column_the_database_lacks_is_one_added_column_and_nothing_else() {
    let found = compared(&one_table(&["id", "name"]), &one_table(&["id"]));
    let [only] = found.changes.changes.as_slice() else {
        panic!("one change, got {:?}", found.changes);
    };
    assert!(
        matches!(
            &only.change,
            Change::Column(ColumnChange::AddColumn { table: on, name, .. })
                if *on == table() && name.shown().to_string() == "name"
        ),
        "{only:?}"
    );
    // Negative: one added column is no rename, whatever else the table holds.
    assert!(!may_hold_a_rename(&found.changes));
}

#[test]
fn a_column_under_another_name_is_a_drop_and_an_add_and_is_said_to_be_names_only() {
    let found = compared(&one_table(&["id", "title"]), &one_table(&["id", "name"]));
    let dropped = found.changes.changes.iter().any(|planned| {
        matches!(
            &planned.change,
            Change::Column(ColumnChange::DropColumn { column, .. })
                if column.name.shown().to_string() == "name"
        )
    });
    let added = found.changes.changes.iter().any(|planned| {
        matches!(
            &planned.change,
            Change::Column(ColumnChange::AddColumn { name, .. })
                if name.shown().to_string() == "title"
        )
    });
    assert!(dropped && added, "{:?}", found.changes);
    assert_eq!(found.changes.changes.len(), 2);
    assert!(may_hold_a_rename(&found.changes));
}

/// Negative for the hint: a column leaving one table and a column joining
/// another are not one column under two names.
#[test]
fn a_drop_in_one_table_and_an_add_in_another_is_not_called_a_rename() {
    let other = TableName::new("app", "u");
    let declared = schema_of(&[(&table(), &["id"]), (&other, &["id", "note"])]);
    let live = schema_of(&[(&table(), &["id", "name"]), (&other, &["id"])]);
    let found = compared(&declared, &live);
    assert_eq!(found.changes.changes.len(), 2, "{:?}", found.changes);
    assert!(!may_hold_a_rename(&found.changes));
}

/// The identity file still names `app.old`, the declarations name `app.t`,
/// and the database holds `app.old`: what `plan` would ask about is a drop
/// and a create here. That `app.old` reaches the comparison at all is the
/// scope's work, which the next test pins.
#[test]
fn a_table_renamed_in_the_declarations_is_a_drop_and_a_create() {
    let old = TableName::new("app", "old");
    let before = schema_of(&[(&old, &["id"])]);
    let project = pbps_diff::observed_ids(&before, &IdsFile::default());
    let found = compared_in(&one_table(&["id"]), &project, &before);
    let dropped = found.changes.changes.iter().any(|planned| {
        matches!(&planned.change, Change::Table(TableChange::DropTable { name, .. }) if *name == old)
    });
    let created = found.changes.changes.iter().any(|planned| {
        matches!(
            &planned.change,
            Change::Table(TableChange::CreateTable { name, .. }) if *name == table()
        )
    });
    assert!(dropped && created, "{:?}", found.changes);
    assert!(may_hold_a_rename(&found.changes));
}

#[test]
fn the_scope_is_the_declared_names_and_the_identity_files_and_holds_no_role() {
    let old = TableName::new("app", "old");
    let mut before = schema_of(&[(&old, &["id"])]);
    before
        .roles
        .insert("reader".into(), pbps_model::role::Role::default());
    let project = pbps_diff::observed_ids(&before, &IdsFile::default());
    assert!(!project.roles.is_empty(), "the fixture records a role");

    let mut declared = one_table(&["id"]);
    declared
        .roles
        .insert("reader".into(), pbps_model::role::Role::default());
    let scope = scope_ids(&declared, &project);
    assert!(scope.table_uid(&table()).is_some());
    assert_eq!(scope.table_uid(&old), project.table_uid(&old));
    assert!(scope.roles.is_empty());
    // Negative: a name neither side knows is outside it.
    assert!(
        scope
            .table_uid(&TableName::new("app", "stranger"))
            .is_none()
    );
}

/// Roles are outside the comparison in both directions. Negative: the column
/// beside the role still counts.
#[test]
fn a_role_on_one_side_only_is_no_difference() {
    let mut declared = one_table(&["id"]);
    declared
        .roles
        .insert("reader".into(), pbps_model::role::Role::default());
    let found = compared(&declared, &one_table(&["id"]));
    assert!(found.changes.is_empty(), "{:?}", found.changes);
    let found = compared(&one_table(&["id"]), &declared);
    assert!(found.changes.is_empty(), "{:?}", found.changes);

    let found = compared(&declared, &one_table(&["id", "extra"]));
    assert_eq!(found.changes.changes.len(), 1, "{:?}", found.changes);
}

/// The engine's spelling of a declared check is a text difference with both
/// texts, and no change: compared as read it would be a drop and an add.
#[test]
fn a_check_spelled_otherwise_is_a_text_difference_and_no_change() {
    let declared = with_check(one_table(&["id"]), "id > 0");
    let live = with_check(one_table(&["id"]), "([id]>(0))");
    let found = compared(&declared, &live);
    assert!(found.changes.is_empty(), "{:?}", found.changes);
    assert_eq!(
        found.text_differences,
        [TextDifference {
            object: "check ck_t_id on app.t".into(),
            declared: "id > 0".into(),
            stored: "([id]>(0))".into(),
        }]
    );
    // Negative: the same text on both sides is no text difference.
    let found = compared(&declared, &declared.clone());
    assert_eq!(found.text_differences, []);
}

/// A check on one side only has no pair of texts: it is a change, and only a
/// change.
#[test]
fn a_check_the_database_lacks_is_a_change_and_no_text_difference() {
    let declared = with_check(one_table(&["id"]), "id > 0");
    let found = compared(&declared, &one_table(&["id"]));
    assert_eq!(found.changes.changes.len(), 1, "{:?}", found.changes);
    assert_eq!(found.text_differences, []);
    let found = compared(&one_table(&["id"]), &declared);
    assert_eq!(found.changes.changes.len(), 1, "{:?}", found.changes);
    assert_eq!(found.text_differences, []);
}

#[test]
fn a_module_body_differs_by_more_than_layout_or_not_at_all() {
    let id: ModuleId = "app.v".parse().unwrap();
    let with_view = |definition: &str| {
        let mut schema = one_table(&["id"]);
        schema.modules.insert(id.clone(), view(definition));
        schema
    };
    let declared = with_view("SELECT id FROM app.t");
    // Negative: layout alone.
    let found = compared(&declared, &with_view("SELECT id\n  FROM app.t"));
    assert!(found.changes.is_empty(), "{:?}", found.changes);
    assert_eq!(found.text_differences, []);

    let found = compared(&declared, &with_view("SELECT t.id FROM app.t"));
    assert!(found.changes.is_empty(), "{:?}", found.changes);
    assert_eq!(
        found.text_differences,
        [TextDifference {
            object: "definition of app.v".into(),
            declared: "SELECT id FROM app.t".into(),
            stored: "SELECT t.id FROM app.t".into(),
        }]
    );
}

/// A text arrives from a catalog, so the report never prints one raw.
#[test]
fn the_text_report_escapes_what_it_prints_and_is_empty_for_none() {
    assert_eq!(text_report(&[]), "");
    let report = text_report(&[TextDifference {
        object: "check ck on app.t".into(),
        declared: "id > 0".into(),
        stored: "id > 0 \u{1b}[2J".into(),
    }]);
    assert!(report.contains("Text differences (1):"), "{report}");
    assert!(report.contains("declared: id > 0\n"), "{report}");
    assert!(!report.contains('\u{1b}'), "{report:?}");
}
