use super::*;
use pbps_load::DeclaredFile;
use pbps_model::diagnostic::Declaration as Source;
use serde_json::json;

fn project(tag: &str) -> Project {
    let root = std::env::temp_dir().join(format!("pbps-ids-store-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("schema")).unwrap();
    std::fs::write(root.join("pbps.yml"), "dialect: postgres\n").unwrap();
    Project::load(&root.join("pbps.yml")).unwrap()
}

fn ids(value: serde_json::Value) -> IdsFile {
    serde_json::from_value(value).unwrap()
}

/// The declarations: a table, the file it is in (relative to `schema/`) and its
/// columns in YAML order.
fn declared(tables: &[(&str, &str, &[&str])]) -> DeclaredPaths {
    let mut out = DeclaredPaths::default();
    for (name, path, columns) in tables {
        out.files.insert(
            Source::Table(name.parse().unwrap()),
            DeclaredFile {
                path: (*path).to_owned(),
                columns: columns.iter().map(|c| (*c).into()).collect(),
            },
        );
    }
    out
}

const T: &str = "t_aaaaaa";

fn customer() -> IdsFile {
    ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer"},
        "columns": {"c_aaaaa1": "dbo.customer.id", "c_aaaaa2": "dbo.customer.name"},
    }))
}

fn customer_declared() -> DeclaredPaths {
    declared(&[("dbo.customer", "dbo/customer.yml", &["id", "name"])])
}

/// Writes `after` for the first time and returns the project with its tree.
fn written(tag: &str, after: &IdsFile, paths: &DeclaredPaths) -> Project {
    let project = project(tag);
    plan(&project, after, paths).unwrap().apply().unwrap();
    project
}

fn tree(project: &Project) -> BTreeMap<String, String> {
    read_ids_tree(&project.ids_dir()).unwrap()
}

fn refusal(project: &Project, after: &IdsFile, paths: &DeclaredPaths, case: &str) {
    let before = tree(project);
    let error = format!("{:#}", plan(project, after, paths).unwrap_err());
    assert!(error.contains(case), "{error}");
    assert!(error.contains("#2600 PR 5b"), "{error}");
    assert!(error.contains("nothing was written"), "{error}");
    assert_eq!(tree(project), before, "a refusal must write nothing");
}

#[test]
fn a_fresh_project_gets_its_tree_at_the_mirrored_paths_and_names_ids_dir() {
    let project = project("fresh");
    assert_eq!(read_opt(&project).unwrap(), None);
    let write = plan(&project, &customer(), &customer_declared()).unwrap();
    // Nothing is on disk until it is applied.
    assert!(!project.ids_dir().exists());
    write.apply().unwrap();
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    assert_eq!(
        tree(&project).keys().collect::<Vec<_>>(),
        ["dbo/customer.json"]
    );
    let config = std::fs::read_to_string(project.config_file()).unwrap();
    assert!(config.contains("ids_dir: .pbps/ids"), "{config}");
    // The same mapping again is no write at all, and the config is left alone.
    let again = plan(&project, &customer(), &customer_declared()).unwrap();
    assert!(again.files.is_empty() && again.config.is_none());
}

#[test]
fn a_new_column_goes_after_its_yaml_predecessor_and_no_other_file_changes() {
    let two = ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer", "t_bbbbbb": "dbo.order"},
        "columns": {
            "c_aaaaa1": "dbo.customer.id", "c_aaaaa2": "dbo.customer.name",
            "c_bbbbb1": "dbo.order.id",
        },
    }));
    let paths = declared(&[
        ("dbo.customer", "dbo/customer.yml", &["id", "name"]),
        ("dbo.order", "dbo/order.yml", &["id"]),
    ]);
    let project = written("column", &two, &paths);
    let before = tree(&project);

    let mut grown = two.clone();
    grown.columns.insert(
        "c_aaaaa3".parse().unwrap(),
        "dbo.customer.email".parse().unwrap(),
    );
    let paths = declared(&[
        ("dbo.customer", "dbo/customer.yml", &["id", "email", "name"]),
        ("dbo.order", "dbo/order.yml", &["id"]),
    ]);
    let write = plan(&project, &grown, &paths).unwrap();
    assert_eq!(
        write.files.keys().collect::<Vec<_>>(),
        ["dbo/customer.json"]
    );
    assert!(write.config.is_none(), "the tree already exists");
    write.apply().unwrap();
    let after = tree(&project);
    assert_eq!(after["dbo/order.json"], before["dbo/order.json"]);
    let file = &after["dbo/customer.json"];
    let at = |uid: &str| file.find(uid).unwrap();
    assert!(
        at("c_aaaaa1") < at("c_aaaaa3") && at("c_aaaaa3") < at("c_aaaaa2"),
        "{file}"
    );
    assert_eq!(read_opt(&project).unwrap(), Some(grown));
}

#[test]
fn a_renamed_table_changes_its_header_only_and_a_dropped_one_stays_where_it_is() {
    let project = written("rename-drop", &customer(), &customer_declared());
    let mut renamed = customer();
    renamed
        .tables
        .insert(T.parse().unwrap(), "dbo.client".parse().unwrap());
    for column in renamed.columns.values_mut() {
        column.table = "dbo.client".parse().unwrap();
    }
    let paths = declared(&[("dbo.client", "dbo/customer.yml", &["id", "name"])]);
    plan(&project, &renamed, &paths).unwrap().apply().unwrap();
    assert!(tree(&project)["dbo/customer.json"].contains("\"dbo.client\""));

    // The table is dropped: its YAML is gone, its file stays and says so.
    let dropped = ids(json!({
        "version": 1,
        "tombstones": {
            T: {"was": "dbo.client", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa1": {"was": "dbo.client.id", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa2": {"was": "dbo.client.name", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
        },
    }));
    plan(&project, &dropped, &DeclaredPaths::default())
        .unwrap()
        .apply()
        .unwrap();
    assert_eq!(
        tree(&project).keys().collect::<Vec<_>>(),
        ["dbo/customer.json"]
    );
    assert!(tree(&project)["dbo/customer.json"].contains("\"dropped\""));
    assert_eq!(read_opt(&project).unwrap(), Some(dropped));
}

#[test]
fn a_declaration_whose_file_moved_is_refused_as_a_relocation() {
    let project = written("relocation", &customer(), &customer_declared());
    let moved = declared(&[("dbo.customer", "dbo/client.yml", &["id", "name"])]);
    refusal(&project, &customer(), &moved, "moving an identity file");
}

#[test]
fn a_new_declaration_over_a_path_another_identity_holds_is_refused_as_a_displacement() {
    let project = written("displacement", &customer(), &customer_declared());
    let both = ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer", "t_bbbbbb": "dbo.other"},
        "columns": {"c_aaaaa1": "dbo.customer.id", "c_aaaaa2": "dbo.customer.name"},
    }));
    let paths = declared(&[
        ("dbo.customer", "dbo/customer.yml", &["id", "name"]),
        ("dbo.other", "dbo/customer.yml", &[]),
    ]);
    // Two declarations for one file: the old one keeps it, so the new one has
    // nowhere to go.
    refusal(&project, &both, &paths, "another identity holds");
}

#[test]
fn a_table_that_becomes_or_stops_being_a_partition_is_refused() {
    let two = ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer", "t_bbbbbb": "dbo.child"},
        "columns": {"c_aaaaa1": "dbo.customer.id", "c_bbbbb1": "dbo.child.id"},
    }));
    let own_files = declared(&[
        ("dbo.customer", "dbo/customer.yml", &["id"]),
        ("dbo.child", "dbo/child.yml", &["id"]),
    ]);
    let project = written("attach", &two, &own_files);
    // `dbo.child` is declared inside `dbo.customer` now: an attach.
    let mut attached = declared(&[("dbo.customer", "dbo/customer.yml", &["id"])]);
    attached.children.insert(
        "dbo.child".parse().unwrap(),
        "dbo.customer".parse().unwrap(),
    );
    refusal(
        &project,
        &two,
        &attached,
        "attaching or detaching a partition",
    );

    // And the other way round: a child recorded in its parent's file whose
    // declaration is a table of its own again. A child has no column uids.
    let child_ids = ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer", "t_bbbbbb": "dbo.child"},
        "columns": {"c_aaaaa1": "dbo.customer.id"},
    }));
    let mut as_child = declared(&[("dbo.customer", "dbo/customer.yml", &["id"])]);
    as_child.children.insert(
        "dbo.child".parse().unwrap(),
        "dbo.customer".parse().unwrap(),
    );
    let child_project = written("detach", &child_ids, &as_child);
    refusal(
        &child_project,
        &child_ids,
        &own_files,
        "attaching or detaching a partition",
    );
}

#[test]
fn an_identity_file_the_new_mapping_no_longer_holds_is_refused() {
    let project = written("removal", &customer(), &customer_declared());
    refusal(
        &project,
        &IdsFile::default(),
        &DeclaredPaths::default(),
        "removing an identity file",
    );
}

#[test]
fn a_declaration_with_an_identity_and_no_file_to_mirror_is_refused() {
    let project = project("no-place");
    let before = tree(&project);
    let error = format!(
        "{:#}",
        plan(&project, &customer(), &DeclaredPaths::default()).unwrap_err()
    );
    assert!(error.contains("no declaration to mirror"), "{error}");
    assert!(error.contains("#2600 PR 5b"), "{error}");
    assert_eq!(tree(&project), before);
    assert!(!project.ids_dir().exists());
}

#[test]
fn an_inconsistent_mapping_is_refused_before_anything_else() {
    let project = project("invalid");
    let mut invalid = customer();
    invalid
        .tables
        .insert("t_bbbbbb".parse().unwrap(), "dbo.customer".parse().unwrap());
    let error = format!(
        "{:#}",
        plan(&project, &invalid, &customer_declared()).unwrap_err()
    );
    assert!(error.contains("inconsistent"), "{error}");
    assert!(!project.ids_dir().exists());
}

#[test]
fn only_the_legacy_file_is_a_refusal_that_says_what_to_run_and_both_layouts_is_another() {
    let project = project("legacy");
    std::fs::write(project.ids_file(), "{\"version\":1}\n").unwrap();
    let error = format!("{:#}", read_opt(&project).unwrap_err());
    assert!(error.contains("pbps ids migrate"), "{error}");
    // A write beside the legacy file would split the history.
    let error = format!(
        "{:#}",
        plan(&project, &customer(), &customer_declared()).unwrap_err()
    );
    assert!(error.contains("pbps ids migrate"), "{error}");
    assert!(!project.ids_dir().exists());
    // With a tree as well, the migration did not finish.
    write_ids_tree(&project, &customer());
    let error = format!("{:#}", read_opt(&project).unwrap_err());
    assert!(error.contains("did not finish"), "{error}");
}

#[test]
fn a_path_that_is_not_a_directory_is_never_no_identities() {
    let project = project("not-a-directory");
    std::fs::create_dir_all(project.ids_dir().parent().unwrap()).unwrap();
    std::fs::write(project.ids_dir(), "").unwrap();
    let error = format!("{:#}", read_opt(&project).unwrap_err());
    assert!(error.contains("not a directory"), "{error}");
}

#[test]
fn what_is_applied_reads_back_whatever_the_directory_depth() {
    let deep = ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer"},
        "columns": {"c_aaaaa1": "dbo.customer.id"},
    }));
    let paths = declared(&[("dbo.customer", "a/b/c/customer.yaml", &["id"])]);
    let project = written("deep", &deep, &paths);
    assert_eq!(
        tree(&project).keys().collect::<Vec<_>>(),
        ["a/b/c/customer.json"]
    );
    assert_eq!(read_opt(&project).unwrap(), Some(deep));
}
