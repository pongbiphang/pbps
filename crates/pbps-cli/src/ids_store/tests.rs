use super::*;
use pbps_load::DeclaredFile;
use pbps_model::diagnostic::Declaration as Source;
use serde_json::json;
use std::cell::Cell;

thread_local! {
    /// Counts staged writes down; the one that reaches 1 creates its file and fails.
    static WRITES_UNTIL_FAULT: Cell<usize> = const { Cell::new(0) };
}

/// Makes the `n`th staged write of this thread create its file and then fail.
fn fail_write_number(n: usize) {
    WRITES_UNTIL_FAULT.with(|c| c.set(n));
}

pub(super) fn fault_now() -> bool {
    WRITES_UNTIL_FAULT.with(|c| match c.get() {
        0 => false,
        n => {
            c.set(n - 1);
            n == 1
        }
    })
}

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
fn the_tombstones_of_a_table_that_becomes_a_partition_move_into_its_parents_file() {
    // `dbo.child` once had a column, dropped; it is then attached to `dbo.customer`
    // and has no file of its own to keep the tombstone in.
    let alone = ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer", "t_bbbbbb": "dbo.child"},
        "columns": {"c_aaaaa1": "dbo.customer.id"},
        "tombstones": {
            "c_bbbbb1": {"was": "dbo.child.extra", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
        },
    }));
    let own = declared(&[
        ("dbo.customer", "dbo/customer.yml", &["id"]),
        ("dbo.child", "dbo/child.yml", &[]),
    ]);
    let project = written("child-tombstone", &alone, &own);
    assert!(tree(&project)["dbo/child.json"].contains("c_bbbbb1"));
    let mut inside = declared(&[("dbo.customer", "dbo/customer.yml", &["id"])]);
    inside.children.insert(
        "dbo.child".parse().unwrap(),
        "dbo.customer".parse().unwrap(),
    );
    let write = plan(&project, &alone, &inside).unwrap();
    assert_eq!(write.removals, ["dbo/child.json"]);
    write.apply().unwrap();
    assert!(tree(&project)["dbo/customer.json"].contains("c_bbbbb1"));
    assert_eq!(read_opt(&project).unwrap(), Some(alone));
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

#[test]
fn a_move_that_turns_an_identity_file_into_a_directory_is_applied() {
    let flat = declared(&[("dbo.customer", "customer.yml", &["id", "name"])]);
    let project = written("file-to-dir", &customer(), &flat);
    assert_eq!(tree(&project).keys().collect::<Vec<_>>(), ["customer.json"]);
    let nested = declared(&[(
        "dbo.customer",
        "customer.json/customer.yml",
        &["id", "name"],
    )]);
    let write = plan(&project, &customer(), &nested).unwrap();
    assert_eq!(write.removals, ["customer.json"]);
    write.apply().unwrap();
    assert_eq!(
        tree(&project).keys().collect::<Vec<_>>(),
        ["customer.json/customer.json"]
    );
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    // Nothing of the move is left beside the tree.
    let names: Vec<_> = std::fs::read_dir(project.ids_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["customer.json"]);
    assert!(plan(&project, &customer(), &nested).unwrap().is_empty());
}

#[test]
fn a_move_that_turns_an_identity_directory_into_a_file_is_applied() {
    let nested = declared(&[(
        "dbo.customer",
        "customer.json/customer.yml",
        &["id", "name"],
    )]);
    let project = written("dir-to-file", &customer(), &nested);
    let flat = declared(&[("dbo.customer", "customer.yml", &["id", "name"])]);
    let write = plan(&project, &customer(), &flat).unwrap();
    assert_eq!(write.removals, ["customer.json/customer.json"]);
    write.apply().unwrap();
    assert_eq!(tree(&project).keys().collect::<Vec<_>>(), ["customer.json"]);
    assert!(project.ids_dir().join("customer.json").is_file());
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    let names: Vec<_> = std::fs::read_dir(project.ids_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["customer.json"]);
}

#[test]
fn an_old_file_nothing_removes_is_kept_aside_and_named() {
    let project = project("aside-kept");
    let dir = project.ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.json"), "old").unwrap();
    // An unrelated file already has the aside's name: it must not be replaced.
    std::fs::write(dir.join(".a.json.aside"), "other").unwrap();
    let write = TreeWrite {
        dir: dir.clone(),
        files: [("a.json/a.json".to_owned(), "new".to_owned())].into(),
        ..TreeWrite::default()
    };
    let e = write.apply().unwrap_err().to_string();
    assert!(e.contains(".a.json.aside1"), "{e}");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.json/a.json")).unwrap(),
        "new"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".a.json.aside1")).unwrap(),
        "old"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".a.json.aside")).unwrap(),
        "other"
    );
}

fn two_tables() -> IdsFile {
    ids(json!({
        "version": 1,
        "tables": {T: "dbo.customer", "t_bbbbbb": "dbo.other"},
        "columns": {"c_aaaaa1": "dbo.customer.id", "c_aaaaa2": "dbo.customer.name"},
    }))
}

#[test]
fn a_file_and_a_directory_at_one_identity_path_are_refused_by_name_before_any_write() {
    let project = written("file-and-dir", &customer(), &customer_declared());
    // `a.yml` mirrors to `a.json`; `a.json/b.yml` mirrors to `a.json/b.json`.
    let paths = declared(&[
        ("dbo.customer", "a.yml", &["id", "name"]),
        ("dbo.other", "a.json/b.yml", &[]),
    ]);
    let before = tree(&project);
    let error = format!("{:#}", plan(&project, &two_tables(), &paths).unwrap_err());
    for needle in [
        "dbo.customer",
        "dbo.other",
        "a.json",
        "a.json/b.json",
        "rename one",
    ] {
        assert!(error.contains(needle), "{needle}: {error}");
    }
    assert!(error.contains("nothing was written"), "{error}");
    assert_eq!(tree(&project), before);
    // Siblings that merely share a prefix are fine: `a.json` and `a.json2/b.json`.
    let fine = declared(&[
        ("dbo.customer", "a.yml", &["id", "name"]),
        ("dbo.other", "a.json2/b.yml", &[]),
    ]);
    plan(&project, &two_tables(), &fine).unwrap();
}

#[test]
fn a_dropped_file_in_the_way_of_a_directory_goes_aside_instead_of_refusing() {
    let flat = declared(&[("dbo.customer", "a.yml", &["id", "name"])]);
    let project = written("dropped-ancestor", &customer(), &flat);
    // `dbo.customer` is dropped (its file stays at `a.json`); `dbo.other` is
    // declared below that path.
    let after = ids(json!({
        "version": 1,
        "tables": {"t_bbbbbb": "dbo.other"},
        "tombstones": {
            T: {"was": "dbo.customer", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa1": {"was": "dbo.customer.id", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa2": {"was": "dbo.customer.name", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
        },
    }));
    let paths = declared(&[("dbo.other", "a.json/b.yml", &[])]);
    plan(&project, &after, &paths).unwrap().apply().unwrap();
    let names: Vec<_> = tree(&project).into_keys().collect();
    assert_eq!(names, ["a.json/b.json", "dbo.customer.t_aaaaaa.json"]);
    assert_eq!(read_opt(&project).unwrap().unwrap().tables.len(), 1);
}

/// A directory the write cannot create files in, so that a later staged write
/// fails after the earlier ones succeeded. `None` when the account ignores
/// permissions (root), where the fault cannot be injected.
fn unwritable(dir: &Path) -> Option<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let probe = dir.join(".probe");
    if std::fs::write(&probe, "x").is_ok() {
        let _ = std::fs::remove_file(probe);
        return None;
    }
    Some(())
}

fn writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// What is on disk under `dir`, empty directories included.
fn listing(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(at).unwrap().flatten() {
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .display()
                .to_string();
            if entry.path().is_dir() {
                out.push(format!("{rel}/"));
                walk(root, &entry.path(), out);
            } else {
                out.push(format!(
                    "{rel}={}",
                    std::fs::read_to_string(entry.path()).unwrap()
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn a_failed_staging_write_leaves_the_tree_as_it_was_when_a_file_became_a_directory() {
    let project = project("rollback-file-to-dir");
    let dir = project.ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.json"), "old").unwrap();
    if unwritable(&dir.join("zz")).is_none() {
        return;
    }
    let before = listing(&dir);
    // `a.json` becomes a directory (first), then a write into `zz` fails.
    let write = TreeWrite {
        dir: dir.clone(),
        files: [
            ("a.json/a.json".to_owned(), "new".to_owned()),
            ("zz/n.json".to_owned(), "n".to_owned()),
        ]
        .into(),
        removals: vec!["a.json".to_owned()],
        ..TreeWrite::default()
    };
    let error = write.apply().unwrap_err();
    writable(&dir.join("zz"));
    assert!(format!("{error:#}").contains("cannot write"), "{error:#}");
    assert_eq!(listing(&dir), before);
    assert_eq!(std::fs::read_to_string(dir.join("a.json")).unwrap(), "old");
}

#[test]
fn a_failed_staging_write_leaves_the_tree_as_it_was_when_a_directory_became_a_file() {
    let project = project("rollback-dir-to-file");
    let dir = project.ids_dir();
    std::fs::create_dir_all(dir.join("a.json")).unwrap();
    std::fs::write(dir.join("a.json/a.json"), "old").unwrap();
    if unwritable(&dir.join("zz")).is_none() {
        return;
    }
    let before = listing(&dir);
    let write = TreeWrite {
        dir: dir.clone(),
        files: [
            ("a.json".to_owned(), "new".to_owned()),
            ("zz/n.json".to_owned(), "n".to_owned()),
        ]
        .into(),
        removals: vec!["a.json/a.json".to_owned()],
        ..TreeWrite::default()
    };
    write.apply().unwrap_err();
    writable(&dir.join("zz"));
    assert_eq!(listing(&dir), before);
}

#[test]
fn a_rollback_that_cannot_restore_an_aside_says_where_it_is() {
    let project = project("rollback-stuck");
    let dir = project.ids_dir();
    std::fs::create_dir_all(dir.join("a.json")).unwrap();
    // The original path is occupied by a directory that is not empty, so the
    // aside cannot be renamed back onto it.
    std::fs::write(dir.join("a.json/foreign"), "x").unwrap();
    std::fs::write(dir.join(".a.json.aside"), "old").unwrap();
    let left = roll_back(
        &[],
        &[dir.join("a.json")],
        &[(dir.join("a.json"), dir.join(".a.json.aside"))],
    )
    .unwrap_err();
    assert!(left.contains(".a.json.aside"), "{left}");
    assert!(left.contains("rename it back"), "{left}");
    assert!(left.contains("could not remove"), "{left}");
    // Nothing was deleted on the way.
    assert_eq!(
        std::fs::read_to_string(dir.join(".a.json.aside")).unwrap(),
        "old"
    );
    assert!(dir.join("a.json/foreign").exists());
}

#[test]
fn a_removal_that_names_a_file_this_write_placed_is_kept_and_a_different_old_file_is_removed() {
    let project = project("same-file");
    let dir = project.ids_dir();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("new.json"), "old bytes").unwrap();
    std::fs::write(dir.join("gone.json"), "old").unwrap();
    // `sub/../new.json` is `new.json` by another spelling, which stands in for
    // `Customer.json` and `customer.json` on a case-insensitive filesystem.
    let write = TreeWrite {
        dir: dir.clone(),
        files: [("new.json".to_owned(), "new bytes".to_owned())].into(),
        removals: vec!["sub/../new.json".to_owned(), "gone.json".to_owned()],
        ..TreeWrite::default()
    };
    write.apply().unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("new.json")).unwrap(),
        "new bytes"
    );
    assert!(!dir.join("gone.json").exists(), "a different old file goes");
    assert!(
        dir.join("sub").is_dir(),
        "the placed file's directory tree is untouched"
    );
}

/// Whether the temp directory folds case: create `A`, look for `a`.
fn case_insensitive(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("CaseProbe"), "x").unwrap();
    let folds = dir.join("caseprobe").exists();
    let _ = std::fs::remove_file(dir.join("CaseProbe"));
    folds
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn a_case_only_rename_loses_nothing_on_a_case_insensitive_filesystem() {
    let probe = project("case-probe");
    if !case_insensitive(&probe.ids_dir()) {
        eprintln!(
            "skipped: the temp directory is case-sensitive, so a case-only rename cannot collide"
        );
        return;
    }
    // A file: `Customer.yml` -> `customer.yml`.
    let before = declared(&[("dbo.customer", "Customer.yml", &["id", "name"])]);
    let project = written("case-file", &customer(), &before);
    assert_eq!(names_in(&project.ids_dir()), ["Customer.json"]);
    let after = declared(&[("dbo.customer", "customer.yml", &["id", "name"])]);
    plan(&project, &customer(), &after)
        .unwrap()
        .apply()
        .unwrap();
    assert_eq!(names_in(&project.ids_dir()), ["customer.json"]);
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    assert!(plan(&project, &customer(), &after).unwrap().is_empty());
    // A directory: `Sales/` -> `sales/`.
    let before = declared(&[("dbo.customer", "Sales/customer.yml", &["id", "name"])]);
    let project = written("case-dir", &customer(), &before);
    let after = declared(&[("dbo.customer", "sales/customer.yml", &["id", "name"])]);
    plan(&project, &customer(), &after)
        .unwrap()
        .apply()
        .unwrap();
    assert_eq!(names_in(&project.ids_dir()), ["sales"]);
    assert_eq!(
        names_in(&project.ids_dir().join("sales")),
        ["customer.json"]
    );
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    assert!(plan(&project, &customer(), &after).unwrap().is_empty());
    // A dropped object's `Customer.json` and a new object's `customer.json`:
    // the old identity and its tombstones must survive in the aside file.
    let before = declared(&[("dbo.customer", "Customer.yml", &["id", "name"])]);
    let project = written("case-dropped", &customer(), &before);
    let replaced = dropped_customer_replaced();
    let paths = declared(&[("dbo.other", "customer.yml", &["id"])]);
    plan(&project, &replaced, &paths).unwrap().apply().unwrap();
    assert_eq!(read_opt(&project).unwrap(), Some(replaced.clone()));
    assert!(plan(&project, &replaced, &paths).unwrap().is_empty());
    // A file becomes a directory under another spelling: `a.yml` -> `A.json/x.yml`.
    let before = declared(&[("dbo.customer", "a.yml", &["id", "name"])]);
    let project = written("case-file-to-dir", &customer(), &before);
    let after = declared(&[("dbo.customer", "A.json/x.yml", &["id", "name"])]);
    plan(&project, &customer(), &after)
        .unwrap()
        .apply()
        .unwrap();
    assert_eq!(names_in(&project.ids_dir()), ["A.json"]);
    assert_eq!(names_in(&project.ids_dir().join("A.json")), ["x.json"]);
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    assert!(plan(&project, &customer(), &after).unwrap().is_empty());
}

#[test]
fn a_staged_write_that_fails_after_creating_its_file_leaves_no_temp_file_when_a_file_became_a_directory()
 {
    let project = project("partial-file-to-dir");
    let dir = project.ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.json"), "old").unwrap();
    let before = listing(&dir);
    let write = TreeWrite {
        dir: dir.clone(),
        files: [
            ("a.json/a.json".to_owned(), "new".to_owned()),
            ("a.json/b.json".to_owned(), "new too".to_owned()),
        ]
        .into(),
        removals: vec!["a.json".to_owned()],
        ..TreeWrite::default()
    };
    // The second write creates `a.json/.b.json.tmp` half-way and fails.
    fail_write_number(2);
    let error = write.apply().unwrap_err();
    assert!(format!("{error:#}").contains("injected"), "{error:#}");
    assert_eq!(listing(&dir), before);
    assert_eq!(read_ids_tree(&dir).unwrap().len(), 1);
    // Negative: the same write, unarmed, goes through.
    write.apply().unwrap();
    assert!(dir.join("a.json/b.json").is_file());
}

#[test]
fn a_staged_write_that_fails_after_creating_its_file_leaves_no_temp_file_when_a_directory_became_a_file()
 {
    let project = project("partial-dir-to-file");
    let dir = project.ids_dir();
    std::fs::create_dir_all(dir.join("a.json")).unwrap();
    std::fs::write(dir.join("a.json/a.json"), "old").unwrap();
    let before = listing(&dir);
    let write = TreeWrite {
        dir: dir.clone(),
        files: [
            ("a.json".to_owned(), "new".to_owned()),
            ("b.json".to_owned(), "new too".to_owned()),
        ]
        .into(),
        removals: vec!["a.json/a.json".to_owned()],
        ..TreeWrite::default()
    };
    fail_write_number(2);
    write.apply().unwrap_err();
    assert_eq!(listing(&dir), before);
    write.apply().unwrap();
    assert!(dir.join("a.json").is_file());
}

#[test]
fn a_config_write_that_fails_leaves_no_temp_file() {
    let project = project("config-tmp");
    let dir = project.ids_dir();
    // The target is a non-empty directory, so the final rename fails.
    std::fs::create_dir_all(dir.join("pbps.yml")).unwrap();
    std::fs::write(dir.join("pbps.yml/keep"), "x").unwrap();
    let before = listing(&dir);
    write_atomic(&dir.join("pbps.yml"), "ids_dir: x\n").unwrap_err();
    assert_eq!(listing(&dir), before);
}

#[test]
fn a_destination_named_like_a_temp_file_does_not_collide_with_it() {
    // `-a.yml` and `.-a.json.tmp/b.yml` mirror to `-a.json` and
    // `.-a.json.tmp/b.json`. `-` sorts before `.`, so `-a.json` is staged first
    // and its temp file would take exactly the name of the next destination's
    // directory. (The other order is caught by the disk alone.)
    for (tag, first, second) in [
        ("temp-name", "-a.yml", ".-a.json.tmp/b.yml"),
        ("temp-name-reverse", "a.yml", ".a.json.tmp/b.yml"),
    ] {
        let project = project(tag);
        let paths = declared(&[
            ("dbo.customer", first, &["id", "name"]),
            ("dbo.other", second, &[]),
        ]);
        plan(&project, &two_tables(), &paths)
            .unwrap()
            .apply()
            .unwrap();
        assert_eq!(read_opt(&project).unwrap(), Some(two_tables()), "{tag}");
        // Nothing of the staging is left, and a second plan has nothing to do.
        assert_eq!(names_in(&project.ids_dir()).len(), 2, "{tag}");
        assert!(plan(&project, &two_tables(), &paths).unwrap().is_empty());
    }
}

#[test]
fn a_destination_named_like_an_aside_does_not_collide_with_it() {
    let project = project("aside-name");
    let dir = project.ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("-a.json"), "old").unwrap();
    // `-a.json` becomes a directory (its old file goes aside first) while
    // another destination is the directory `.-a.json.aside`.
    let write = TreeWrite {
        dir: dir.clone(),
        files: [
            ("-a.json/x.json".to_owned(), "x".to_owned()),
            (".-a.json.aside/c.json".to_owned(), "c".to_owned()),
        ]
        .into(),
        removals: vec!["-a.json".to_owned()],
        ..TreeWrite::default()
    };
    write.apply().unwrap();
    assert_eq!(
        names_in(&dir),
        ["-a.json", ".-a.json.aside"],
        "the old file was removed from wherever it was set aside"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".-a.json.aside/c.json")).unwrap(),
        "c"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("-a.json/x.json")).unwrap(),
        "x"
    );
}

#[test]
fn ordinary_moves_keep_the_plain_temp_and_aside_names() {
    let mut reserved = BTreeSet::new();
    let path = Path::new("/ids/a.json");
    let key = PathKey::default();
    let first = crate::ids_cmd::unused_sibling(path, "tmp", &mut reserved, key);
    assert_eq!(first, Path::new("/ids/.a.json.tmp"));
    // The same name twice in one write gets the next one.
    let second = crate::ids_cmd::unused_sibling(path, "tmp", &mut reserved, key);
    assert_eq!(second, Path::new("/ids/.a.json.tmp1"));
}

// ------------------------------------------------ the path key (#2770) ----

/// `dbo.customer` is dropped; `dbo.other` is declared where it was, spelled
/// with other case.
fn dropped_customer_replaced() -> IdsFile {
    ids(json!({
        "version": 1,
        "tables": {"t_bbbbbb": "dbo.other"},
        "columns": {"c_bbbbb1": "dbo.other.id"},
        "tombstones": {
            T: {"was": "dbo.customer", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa1": {"was": "dbo.customer.id", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa2": {"was": "dbo.customer.name", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
        },
    }))
}

#[test]
fn a_dropped_object_and_a_new_one_that_differ_only_in_case_do_not_share_a_file_when_case_folds() {
    let before = declared(&[("dbo.customer", "Customer.yml", &["id", "name"])]);
    let project = written("fold-dropped", &customer(), &before);
    let after = dropped_customer_replaced();
    let paths = declared(&[("dbo.other", "customer.yml", &["id"])]);
    // Case folds: the new object takes `customer.json`, the old file's path, so
    // the dropped one goes aside instead of being overwritten.
    let write = plan_keyed(&project, &after, &paths, PathKey::folding()).unwrap();
    assert_eq!(
        write.files.keys().collect::<Vec<_>>(),
        ["customer.json", "dbo.customer.t_aaaaaa.json"]
    );
    assert!(write.files["dbo.customer.t_aaaaaa.json"].contains("\"dropped\""));
    assert_eq!(write.removals, ["Customer.json"]);
    // Case is significant: two files, no aside, nothing renamed.
    let write = plan_keyed(&project, &after, &paths, PathKey::default()).unwrap();
    // (`Customer.json` is rewritten in place, as dropped; nothing is set aside.)
    assert_eq!(
        write.files.keys().collect::<Vec<_>>(),
        ["Customer.json", "customer.json"]
    );
    assert!(
        write.removals.is_empty(),
        "the dropped file keeps its path: {:?}",
        write.removals
    );
}

#[test]
fn two_declarations_differing_only_in_case_are_one_file_when_case_folds() {
    let project = project("fold-two-live");
    let paths = declared(&[
        ("dbo.customer", "Customer.yml", &["id", "name"]),
        ("dbo.other", "customer.yml", &[]),
    ]);
    let error = format!(
        "{:#}",
        plan_keyed(&project, &two_tables(), &paths, PathKey::folding()).unwrap_err()
    );
    for needle in [
        "dbo.customer",
        "dbo.other",
        "Customer.json",
        "customer.json",
        "nothing was written",
    ] {
        assert!(error.contains(needle), "{needle}: {error}");
    }
    plan_keyed(&project, &two_tables(), &paths, PathKey::default()).unwrap();
}

#[test]
fn a_file_and_a_directory_differing_only_in_case_are_refused_when_case_folds() {
    let project = project("fold-file-dir");
    let paths = declared(&[
        ("dbo.customer", "A.yml", &["id", "name"]),
        ("dbo.other", "a.json/b.yml", &[]),
    ]);
    let error = format!(
        "{:#}",
        plan_keyed(&project, &two_tables(), &paths, PathKey::folding()).unwrap_err()
    );
    assert!(
        error.contains("a file cannot also be a directory"),
        "{error}"
    );
    plan_keyed(&project, &two_tables(), &paths, PathKey::default()).unwrap();
}

#[test]
fn a_case_only_rename_is_planned_as_one_file_and_proved_when_case_folds() {
    let before = declared(&[("dbo.customer", "Customer.yml", &["id", "name"])]);
    let project = written("fold-rename", &customer(), &before);
    let after = declared(&[("dbo.customer", "customer.yml", &["id", "name"])]);
    let write = plan_keyed(&project, &customer(), &after, PathKey::folding()).unwrap();
    assert_eq!(write.files.keys().collect::<Vec<_>>(), ["customer.json"]);
    assert_eq!(write.removals, ["Customer.json"]);
    // The removal names a file the write placed: with the key it is skipped,
    // with case significant it is the old file and goes.
    let dir = project.ids_dir();
    write.apply().unwrap();
    assert!(
        dir.join("Customer.json").exists(),
        "skipped as the placed file"
    );
    std::fs::remove_file(dir.join("customer.json")).unwrap();
    let write = plan_keyed(&project, &customer(), &after, PathKey::default()).unwrap();
    write.apply().unwrap();
    assert_eq!(names_in(&dir), ["customer.json"]);
}

#[test]
fn generated_names_avoid_a_planned_path_that_differs_only_in_case_when_case_folds() {
    // The write plans `<dest>` as a directory; its file `<file>` stages a temp
    // file that would take the directory's name in another case.
    for (file, dest) in [
        ("-a.json", ".-A.json.tmp/b.json"),
        ("-A.json", ".-a.json.tmp/b.json"),
    ] {
        for (key, expect) in [(PathKey::folding(), "tmp1"), (PathKey::default(), "tmp")] {
            let write = TreeWrite {
                dir: PathBuf::from("/ids"),
                files: [
                    (file.to_owned(), String::new()),
                    (dest.to_owned(), String::new()),
                ]
                .into(),
                key,
                ..TreeWrite::default()
            };
            let mut reserved = write.reserved_names();
            let tmp = crate::ids_cmd::unused_sibling(
                &Path::new("/ids").join(file),
                "tmp",
                &mut reserved,
                key,
            );
            assert_eq!(
                tmp.file_name().unwrap().to_string_lossy(),
                format!(".{file}.{expect}"),
                "{file} {dest} {key:?}"
            );
        }
    }
}

#[test]
fn the_probe_agrees_with_the_filesystem_with_or_without_an_entry_to_look_at() {
    let project = project("probe");
    let dir = project.ids_dir();
    // A directory that does not exist yet probes its nearest ancestor.
    let folds = PathKey::probe(&dir).unwrap();
    assert_eq!(
        folds == PathKey::folding(),
        case_insensitive(&project.ids_dir())
    );
    // And with a lettered entry in it (no file needs to be created).
    std::fs::write(dir.join("Entry.json"), "x").unwrap();
    assert_eq!(PathKey::probe(&dir).unwrap(), folds);
    assert_eq!(
        names_in(&dir),
        ["Entry.json"],
        "the probe left nothing behind"
    );
}

#[test]
fn a_probe_that_cannot_read_is_an_error_not_case_sensitive() {
    use std::os::unix::fs::PermissionsExt;
    let project = project("probe-unreadable");
    let dir = project.ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read_dir(&dir).is_ok();
    let result = PathKey::probe(&dir);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        return; // an account that ignores permissions
    }
    assert!(result.is_err(), "{result:?}");
}

// ------------------------------------- the sweep of raw path comparisons ----

#[test]
fn a_removal_is_redirected_to_an_aside_made_under_the_other_spelling_when_case_folds() {
    // `a.yml` -> `A.json/x.yml`: on a folding filesystem the existing file is
    // `A.json` whichever way it is spelled; stand in for that with the file on
    // disk under the new spelling and the removal under the old.
    for (key, folds) in [(PathKey::folding(), true), (PathKey::default(), false)] {
        let project = project(if folds {
            "redirect-fold"
        } else {
            "redirect-exact"
        });
        let dir = project.ids_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("A.json"), "old").unwrap();
        let write = TreeWrite {
            dir: dir.clone(),
            files: [("A.json/x.json".to_owned(), "x".to_owned())].into(),
            removals: vec!["a.json".to_owned()],
            key,
            ..TreeWrite::default()
        };
        let result = write.apply();
        if folds {
            result.unwrap();
            assert_eq!(names_in(&dir), ["A.json"], "no aside is left behind");
            assert_eq!(names_in(&dir.join("A.json")), ["x.json"]);
            assert_eq!(read_ids_tree(&dir).unwrap().len(), 1);
        } else {
            // Case is significant: `a.json` is not the file that went aside, so
            // the removal finds nothing and the aside is reported, not lost.
            let error = format!("{:#}", result.unwrap_err());
            assert!(error.contains("a.json"), "{error}");
            assert!(dir.join(".A.json.aside").exists(), "the old file is kept");
        }
    }
}

#[test]
fn the_redirect_compares_components_by_key() {
    let asides = [(
        PathBuf::from("/ids/A.json"),
        PathBuf::from("/ids/.A.json.aside"),
    )];
    let at = |p: &str, key| redirected(Path::new(p), &asides, key);
    let fold = PathKey::folding();
    assert_eq!(at("/ids/a.json", fold), Path::new("/ids/.A.json.aside"));
    assert_eq!(
        at("/ids/a.JSON/b.json", fold),
        Path::new("/ids/.A.json.aside/b.json")
    );
    // Not under it: a sibling that merely shares a prefix.
    assert_eq!(at("/ids/a.json2", fold), Path::new("/ids/a.json2"));
    assert_eq!(
        at("/ids/a.json", PathKey::default()),
        Path::new("/ids/a.json")
    );
}

#[test]
fn an_output_inside_the_identity_tree_is_refused_by_the_trees_key() {
    let project = project("output-inside");
    let inside = project.ids_dir().join("sub").join("x.json");
    let shouted = PathBuf::from(
        inside
            .to_string_lossy()
            .replacen("ids", "IDS", 1)
            .replace(".pbps", ".PBPS"),
    );
    assert!(refuse_output_inside_keyed(&project, &inside, PathKey::default()).is_err());
    // A differently cased spelling is the same place only where case folds.
    assert!(refuse_output_inside_keyed(&project, &shouted, PathKey::folding()).is_err());
    assert!(refuse_output_inside_keyed(&project, &shouted, PathKey::default()).is_ok());
    // Negative: a sibling that shares a prefix is outside.
    let outside = project.root().join(".pbps").join("ids2").join("x.json");
    assert!(refuse_output_inside_keyed(&project, &outside, PathKey::folding()).is_ok());
}

#[test]
fn a_git_path_is_matched_to_the_configured_directory_by_key() {
    let fold = PathKey::folding();
    let exact = PathKey::default();
    assert_eq!(
        fold.strip_prefix_str(".PBPS/IDS/a.json", ".pbps/ids/"),
        Some("a.json")
    );
    assert_eq!(
        exact.strip_prefix_str(".PBPS/IDS/a.json", ".pbps/ids/"),
        None
    );
    assert_eq!(
        exact.strip_prefix_str(".pbps/ids/a.json", ".pbps/ids/"),
        Some("a.json")
    );
    // Shorter than the prefix, or not on a character boundary: no match, no panic.
    assert_eq!(fold.strip_prefix_str(".pbps", ".pbps/ids/"), None);
    assert_eq!(fold.strip_prefix_str("é", "e"), None);
}

// ------------------------------------ one check on the final destinations ----

/// `dbo.customer` (file `a.json`) is dropped; `dbo.other` is declared below
/// that path, so the dropped file must go aside, and `dbo.third` is declared in
/// a directory named like that aside.
fn dropped_in_the_way_of_two() -> IdsFile {
    ids(json!({
        "version": 1,
        "tables": {"t_bbbbbb": "dbo.other", "t_cccccc": "dbo.third"},
        "columns": {},
        "tombstones": {
            T: {"was": "dbo.customer", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa1": {"was": "dbo.customer.id", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa2": {"was": "dbo.customer.name", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
        },
    }))
}

#[test]
fn an_aside_that_collides_with_a_destination_takes_the_next_free_name() {
    let flat = declared(&[("dbo.customer", "a.yml", &["id", "name"])]);
    let after = dropped_in_the_way_of_two();
    for (spelling, key, expect) in [
        // Exactly the aside's name as a directory.
        (
            "dbo.customer.t_aaaaaa.json/c.yml",
            PathKey::default(),
            "dbo.customer.t_aaaaaa.1.json",
        ),
        (
            "dbo.customer.t_aaaaaa.json/c.yml",
            PathKey::folding(),
            "dbo.customer.t_aaaaaa.1.json",
        ),
        // Only the same name once case is folded.
        (
            "DBO.customer.T_aaaaaa.JSON/c.yml",
            PathKey::folding(),
            "dbo.customer.t_aaaaaa.1.json",
        ),
        // Negative: case is significant, so no collision and the plain name.
        (
            "DBO.customer.T_aaaaaa.JSON/c.yml",
            PathKey::default(),
            "dbo.customer.t_aaaaaa.json",
        ),
    ] {
        let project = written("aside-collides", &customer(), &flat);
        let paths = declared(&[
            ("dbo.other", "a.json/b.yml", &[]),
            ("dbo.third", spelling, &[]),
        ]);
        let write = plan_keyed(&project, &after, &paths, key).unwrap();
        assert!(
            write.files.contains_key(expect),
            "{spelling} {key:?}: {:?}",
            write.files.keys().collect::<Vec<_>>()
        );
        assert!(write.files[expect].contains("\"dropped\""), "{spelling}");
        assert_eq!(write.removals, ["a.json"], "{spelling} {key:?}");
        if key == PathKey::default() {
            // The real filesystem here is case-sensitive: write it, read it back.
            write.apply().unwrap();
            assert_eq!(read_opt(&project).unwrap().unwrap().tombstones.len(), 3);
            assert!(
                plan_keyed(&project, &after, &paths, key)
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

#[test]
fn a_live_pair_is_still_refused_after_the_substitutions() {
    // Two live destinations in conflict are refused whatever is dropped.
    let project = project("final-live-pair");
    let paths = declared(&[
        ("dbo.customer", "a.yml", &["id", "name"]),
        ("dbo.other", "a.json/b.yml", &[]),
    ]);
    let error = format!(
        "{:#}",
        plan_keyed(&project, &two_tables(), &paths, PathKey::default()).unwrap_err()
    );
    assert!(
        error.contains("a file cannot also be a directory"),
        "{error}"
    );
}
