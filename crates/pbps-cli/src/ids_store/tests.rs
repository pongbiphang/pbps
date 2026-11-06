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
fn a_declaration_whose_file_moved_is_written_at_its_new_path_and_the_old_one_removed() {
    let project = written("relocation", &customer(), &customer_declared());
    let moved = declared(&[("dbo.customer", "sales/client.yml", &["id", "name"])]);
    let write = plan(&project, &customer(), &moved).unwrap();
    assert_eq!(
        write.files.keys().collect::<Vec<_>>(),
        ["sales/client.json"]
    );
    assert_eq!(write.removals, ["dbo/customer.json"]);
    // The mapping is unchanged, so only the declaration moved.
    write.apply().unwrap();
    assert_eq!(
        tree(&project).keys().collect::<Vec<_>>(),
        ["sales/client.json"]
    );
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    // The directory the move emptied is gone, and the file's bytes are the
    // writer's, not a copy of the old ones.
    assert!(!project.ids_dir().join("dbo").exists());
    assert!(plan(&project, &customer(), &moved).unwrap().is_empty());
}

#[test]
fn a_dropped_file_whose_path_a_new_declaration_takes_is_sent_aside() {
    let project = written("displacement", &customer(), &customer_declared());
    // The table is dropped, and a different table is declared where it was.
    let after = ids(json!({
        "version": 1,
        "tables": {"t_bbbbbb": "dbo.other"},
        "columns": {"c_bbbbb1": "dbo.other.id"},
        "tombstones": {
            T: {"was": "dbo.customer", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa1": {"was": "dbo.customer.id", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa2": {"was": "dbo.customer.name", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
        },
    }));
    let paths = declared(&[("dbo.other", "dbo/customer.yml", &["id"])]);
    let write = plan(&project, &after, &paths).unwrap();
    assert_eq!(
        write.files.keys().collect::<Vec<_>>(),
        ["dbo.customer.t_aaaaaa.json", "dbo/customer.json"]
    );
    assert!(
        write.removals.is_empty(),
        "the path is rewritten, not removed"
    );
    write.apply().unwrap();
    let after_tree = tree(&project);
    assert!(after_tree["dbo/customer.json"].contains("\"dbo.other\""));
    assert!(after_tree["dbo.customer.t_aaaaaa.json"].contains("\"dropped\""));
    assert_eq!(read_opt(&project).unwrap(), Some(after));
    assert!(
        plan(&project, &read_opt(&project).unwrap().unwrap(), &paths)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn two_live_declarations_for_one_file_are_refused_and_nothing_is_written() {
    let project = written("one-file-two-tables", &customer(), &customer_declared());
    let both = ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer", "t_bbbbbb": "dbo.other"},
        "columns": {"c_aaaaa1": "dbo.customer.id", "c_aaaaa2": "dbo.customer.name"},
    }));
    let paths = declared(&[
        ("dbo.customer", "dbo/customer.yml", &["id", "name"]),
        ("dbo.other", "dbo/customer.yml", &[]),
    ]);
    refusal(&project, &both, &paths, "are both declared at");
}

#[test]
fn a_dropped_object_the_tree_never_held_gets_the_aside_name() {
    let project = project("never-held");
    let after = ids(json!({
        "version": 1,
        "tombstones": {
            T: {"was": "dbo.gone", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
        },
    }));
    plan(&project, &after, &DeclaredPaths::default())
        .unwrap()
        .apply()
        .unwrap();
    assert_eq!(
        tree(&project).keys().collect::<Vec<_>>(),
        ["dbo.gone.t_aaaaaa.json"]
    );
    assert_eq!(read_opt(&project).unwrap(), Some(after));
}

#[test]
fn a_table_that_becomes_a_partition_moves_into_its_parents_file_and_back() {
    let alone = ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer", "t_bbbbbb": "dbo.child"},
        "columns": {"c_aaaaa1": "dbo.customer.id"},
    }));
    let own = declared(&[
        ("dbo.customer", "dbo/customer.yml", &["id"]),
        ("dbo.child", "dbo/child.yml", &[]),
    ]);
    let project = written("attach", &alone, &own);
    assert_eq!(tree(&project).len(), 2);
    // `dbo.child` is declared inside `dbo.customer` now: an attach.
    let mut inside = declared(&[("dbo.customer", "dbo/customer.yml", &["id"])]);
    inside.children.insert(
        "dbo.child".parse().unwrap(),
        "dbo.customer".parse().unwrap(),
    );
    let write = plan(&project, &alone, &inside).unwrap();
    assert_eq!(write.removals, ["dbo/child.json"]);
    assert_eq!(
        write.files.keys().collect::<Vec<_>>(),
        ["dbo/customer.json"]
    );
    write.apply().unwrap();
    assert_eq!(
        tree(&project).keys().collect::<Vec<_>>(),
        ["dbo/customer.json"]
    );
    assert!(tree(&project)["dbo/customer.json"].contains("t_bbbbbb"));
    assert_eq!(read_opt(&project).unwrap(), Some(alone.clone()));
    // And the other way: detached, it has a file of its own again.
    let write = plan(&project, &alone, &own).unwrap();
    assert_eq!(
        write.files.keys().collect::<Vec<_>>(),
        ["dbo/child.json", "dbo/customer.json"]
    );
    assert!(write.removals.is_empty());
    write.apply().unwrap();
    assert_eq!(tree(&project).len(), 2);
    assert_eq!(read_opt(&project).unwrap(), Some(alone));
}

#[test]
fn a_live_child_no_declaration_names_stays_in_its_parents_file() {
    let two = ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer", "t_bbbbbb": "dbo.child"},
    }));
    let mut with_child = declared(&[("dbo.customer", "dbo/customer.yml", &[])]);
    with_child.children.insert(
        "dbo.child".parse().unwrap(),
        "dbo.customer".parse().unwrap(),
    );
    let project = written("undeclared-child", &two, &with_child);
    // A withdrawn rebuild restores a name the declarations no longer spell.
    let bare = declared(&[("dbo.customer", "dbo/customer.yml", &[])]);
    assert!(plan(&project, &two, &bare).unwrap().is_empty());
}

#[test]
fn a_child_that_changes_parent_moves_its_record() {
    let ids3 = ids(json!({
        "version": 1,
        "tables": {T: "dbo.a", "t_bbbbbb": "dbo.b", "t_cccccc": "dbo.child"},
    }));
    let mut under_a = declared(&[("dbo.a", "dbo/a.yml", &[]), ("dbo.b", "dbo/b.yml", &[])]);
    under_a
        .children
        .insert("dbo.child".parse().unwrap(), "dbo.a".parse().unwrap());
    let project = written("reparent", &ids3, &under_a);
    let mut under_b = under_a.clone();
    under_b
        .children
        .insert("dbo.child".parse().unwrap(), "dbo.b".parse().unwrap());
    let write = plan(&project, &ids3, &under_b).unwrap();
    assert_eq!(
        write.files.keys().collect::<Vec<_>>(),
        ["dbo/a.json", "dbo/b.json"]
    );
    assert!(write.removals.is_empty());
    write.apply().unwrap();
    assert!(!tree(&project)["dbo/a.json"].contains("t_cccccc"));
    assert!(tree(&project)["dbo/b.json"].contains("t_cccccc"));
    assert_eq!(read_opt(&project).unwrap(), Some(ids3));
}

#[test]
fn an_identity_the_mapping_no_longer_has_loses_its_file() {
    let two = ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer", "t_bbbbbb": "dbo.order"},
        "columns": {"c_aaaaa1": "dbo.customer.id", "c_bbbbb1": "dbo.order.id"},
    }));
    let both = declared(&[
        ("dbo.customer", "dbo/customer.yml", &["id"]),
        ("dbo.order", "dbo/order.yml", &["id"]),
    ]);
    let project = written("vanish", &two, &both);
    // A withdrawn rebuild is the case: the replacement's identities go and the
    // tree follows the mapping.
    let one = ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer"},
        "columns": {"c_aaaaa1": "dbo.customer.id"},
    }));
    let only = declared(&[("dbo.customer", "dbo/customer.yml", &["id"])]);
    let write = plan(&project, &one, &only).unwrap();
    assert_eq!(write.removals, ["dbo/order.json"]);
    assert!(write.files.is_empty());
    write.apply().unwrap();
    assert_eq!(read_opt(&project).unwrap(), Some(one));
    assert_eq!(
        tree(&project).keys().collect::<Vec<_>>(),
        ["dbo/customer.json"]
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
    assert!(error.contains("nothing was written"), "{error}");
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
