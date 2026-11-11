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

thread_local! {
    /// Counts set-asides down; the one that reaches 1 fails.
    static ASIDES_UNTIL_FAULT: Cell<usize> = const { Cell::new(0) };
}

/// Makes the `n`th set-aside of this thread fail.
fn fail_set_aside_number(n: usize) {
    ASIDES_UNTIL_FAULT.with(|c| c.set(n));
}

pub(super) fn set_aside_fault_now() -> bool {
    ASIDES_UNTIL_FAULT.with(|c| match c.get() {
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
fn an_empty_directory_at_a_destination_is_removed_not_reported_as_content() {
    let project = project("empty-dir-destination");
    // Empty directories, nested, stand where `dbo/customer.json` goes.
    let in_the_way = project.ids_dir().join("dbo/customer.json");
    std::fs::create_dir_all(in_the_way.join("a/b")).unwrap();
    std::fs::create_dir_all(in_the_way.join("c")).unwrap();
    plan(&project, &customer(), &customer_declared())
        .unwrap()
        .apply()
        .unwrap();
    assert!(in_the_way.is_file());
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    // The config is written, and nothing is left beside the file.
    let config = std::fs::read_to_string(project.config_file()).unwrap();
    assert!(config.contains("ids_dir: .pbps/ids"), "{config}");
    assert_eq!(names_in(&project.ids_dir().join("dbo")), ["customer.json"]);
}

#[test]
fn a_directory_holding_a_file_at_a_destination_still_stops_the_write() {
    let project = project("full-dir-destination");
    let in_the_way = project.ids_dir().join("dbo/customer.json");
    std::fs::create_dir_all(in_the_way.join("a")).unwrap();
    std::fs::write(in_the_way.join("a/keep.txt"), "mine").unwrap();
    // The reader refuses a stray entry before a write is planned or the
    // directory moved; either way the file is never lost.
    let outcome = plan(&project, &customer(), &customer_declared()).and_then(|w| w.apply());
    assert!(outcome.is_err());
    let kept: Vec<_> = walk_files(&project.ids_dir());
    assert!(
        kept.iter().any(|f| f.ends_with("keep.txt")),
        "the file survives: {kept:?}"
    );
    assert!(
        !project.config_file().exists() || {
            !std::fs::read_to_string(project.config_file())
                .unwrap()
                .contains("ids_dir")
        }
    );
}

#[test]
fn remove_empty_tree_keeps_a_tree_with_any_entry_and_removes_only_empty_ones() {
    let dir = project("empty-tree").ids_dir();
    std::fs::create_dir_all(dir.join("x/y")).unwrap();
    assert!(remove_empty_tree(&dir.join("x")).unwrap());
    assert!(!dir.join("x").exists());
    std::fs::create_dir_all(dir.join("p/q")).unwrap();
    std::fs::write(dir.join("p/q/f"), "").unwrap();
    assert!(!remove_empty_tree(&dir.join("p")).unwrap());
    assert!(dir.join("p/q/f").is_file());
    // Unreadable is an error, never "empty".
    assert!(remove_empty_tree(&dir.join("missing")).is_err());
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let path = e.unwrap().path();
        if path.is_dir() {
            out.extend(walk_files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn an_old_file_nothing_removes_is_kept_aside_and_named() {
    let project = project("aside-kept");
    let dir = project.ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.json"), "old").unwrap();
    // An unrelated file already has the aside's name: it must not be replaced.
    std::fs::write(dir.join(".pbps-aside0"), "other").unwrap();
    let write = TreeWrite {
        dir: dir.clone(),
        files: [("a.json/a.json".to_owned(), "new".to_owned())].into(),
        ..TreeWrite::default()
    };
    let e = write.apply().unwrap_err().to_string();
    assert!(e.contains(".pbps-aside1"), "{e}");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.json/a.json")).unwrap(),
        "new"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".pbps-aside1")).unwrap(),
        "old"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".pbps-aside0")).unwrap(),
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

// Unix only, with the tests that call it: the fault is injected with
// permission bits, which Windows directories do not have.
/// A directory the write cannot create files in, so that a later staged write
/// fails after the earlier ones succeeded. `None` when the account ignores
/// permissions (root), where the fault cannot be injected.
#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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
    std::fs::write(dir.join(".pbps-aside0"), "old").unwrap();
    let left = roll_back(
        &[],
        &[dir.join("a.json")],
        &[(dir.join("a.json"), dir.join(".pbps-aside0"))],
    )
    .unwrap_err();
    assert!(left.contains(".pbps-aside0"), "{left}");
    assert!(left.contains("rename it back"), "{left}");
    assert!(left.contains("could not remove"), "{left}");
    // Nothing was deleted on the way.
    assert_eq!(
        std::fs::read_to_string(dir.join(".pbps-aside0")).unwrap(),
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
    // The second write creates `a.json/.pbps-tmp0` half-way and fails.
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
    // `-a.yml` and `.pbps-tmp0/b.yml` mirror to `-a.json` and
    // `.pbps-tmp0/b.json`. `-` sorts before `.`, so `-a.json` is staged first
    // and its temp file would take exactly the name of the next destination's
    // directory. (The other order is caught by the disk alone.)
    for (tag, first, second) in [
        ("temp-name", "-a.yml", ".pbps-tmp0/b.yml"),
        ("temp-name-reverse", "a.yml", ".pbps-tmp0/b.yml"),
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
    // another destination is the directory `.pbps-aside0`.
    let write = TreeWrite {
        dir: dir.clone(),
        files: [
            ("-a.json/x.json".to_owned(), "x".to_owned()),
            (".pbps-aside0/c.json".to_owned(), "c".to_owned()),
        ]
        .into(),
        removals: vec!["-a.json".to_owned()],
        ..TreeWrite::default()
    };
    write.apply().unwrap();
    assert_eq!(
        names_in(&dir),
        ["-a.json", ".pbps-aside0"],
        "the old file was removed from wherever it was set aside"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".pbps-aside0/c.json")).unwrap(),
        "c"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("-a.json/x.json")).unwrap(),
        "x"
    );
}

#[test]
fn temp_names_are_a_fixed_stem_and_a_counter_whatever_the_destination_is_called() {
    let mut reserved = BTreeSet::new();
    let path = Path::new("/ids/a.json");
    let key = PathKey::default();
    let first = crate::ids_cmd::unused_sibling(path, "tmp", "", &mut reserved, key);
    assert_eq!(first.unwrap(), Path::new("/ids/.pbps-tmp0"));
    // The same name twice in one write gets the next one.
    let second = crate::ids_cmd::unused_sibling(path, "tmp", "", &mut reserved, key);
    assert_eq!(second.unwrap(), Path::new("/ids/.pbps-tmp1"));
    // Another destination does not change the stem, so the length is constant.
    let long = Path::new("/ids").join(format!("{}.json", "x".repeat(246)));
    let third = crate::ids_cmd::unused_sibling(&long, "tmp", "", &mut reserved, key);
    assert_eq!(third.unwrap(), Path::new("/ids/.pbps-tmp2"));
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
    // Case folds: the new object takes the old file itself, so the dropped one
    // goes aside instead of being overwritten. The file is written where the
    // disk has it and then given the spelling its declaration asks for.
    let write = plan_keyed(&project, &after, &paths, PathKey::folding()).unwrap();
    assert_eq!(
        write.files.keys().collect::<Vec<_>>(),
        ["Customer.json", "dbo.customer.t_aaaaaa.json"]
    );
    assert!(write.files["Customer.json"].contains("dbo.other"));
    assert!(write.files["dbo.customer.t_aaaaaa.json"].contains("\"dropped\""));
    assert!(write.removals.is_empty(), "{:?}", write.removals);
    assert_eq!(write.respell, ["customer.json"]);
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

// A folding key on a filesystem that does not fold: the plan is the one a
// folding filesystem gets, and the two-step rename is an ordinary rename here,
// so these run everywhere. What a folding filesystem itself does with them is
// `a_case_only_rename_loses_nothing_on_a_case_insensitive_filesystem`.

#[test]
fn a_case_only_rename_is_a_respelling_that_writes_and_removes_no_file_when_case_folds() {
    let before = declared(&[("dbo.customer", "Customer.yml", &["id", "name"])]);
    let project = written("fold-rename", &customer(), &before);
    let after = declared(&[("dbo.customer", "customer.yml", &["id", "name"])]);
    let dir = project.ids_dir();
    let write = plan_keyed(&project, &customer(), &after, PathKey::folding()).unwrap();
    // One file under another spelling: nothing to write, nothing to remove.
    assert!(write.files.is_empty() && write.removals.is_empty());
    assert_eq!(write.respell, ["customer.json"]);
    assert!(!write.is_empty(), "the spelling alone is a difference");
    write.apply().unwrap();
    assert_eq!(names_in(&dir), ["customer.json"]);
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    // Negative: done once, it is not asked for again.
    assert!(
        plan_keyed(&project, &customer(), &after, PathKey::folding())
            .unwrap()
            .is_empty()
    );
    // Negative: where case is significant it is another path, so the file
    // moves and no spelling is involved.
    let project = written("fold-rename-plain", &customer(), &before);
    let write = plan_keyed(&project, &customer(), &after, PathKey::default()).unwrap();
    assert_eq!(write.files.keys().collect::<Vec<_>>(), ["customer.json"]);
    assert_eq!(write.removals, ["Customer.json"]);
    assert!(write.respell.is_empty());
}

#[test]
fn a_file_that_changes_under_another_spelling_is_written_once_and_respelled() {
    let before = declared(&[("dbo.customer", "Customer.yml", &["id"])]);
    let project = written("fold-rename-edit", &customer(), &before);
    // The declaration is renamed and its columns reordered in one edit.
    let after = declared(&[("dbo.customer", "customer.yml", &["name", "id"])]);
    let write = plan_keyed(&project, &customer(), &after, PathKey::folding()).unwrap();
    // Written under the spelling the disk has: the same write whether or not
    // the directory this test runs in folds case.
    assert_eq!(write.files.keys().collect::<Vec<_>>(), ["Customer.json"]);
    assert!(write.removals.is_empty(), "the file written is the old one");
    assert_eq!(write.respell, ["customer.json"]);
    write.apply().unwrap();
    assert_eq!(names_in(&project.ids_dir()), ["customer.json"]);
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    assert!(
        plan_keyed(&project, &customer(), &after, PathKey::folding())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_respelled_directory_comes_before_what_it_holds() {
    let before = declared(&[("dbo.customer", "Sales/Customer.yml", &["id", "name"])]);
    let project = written("fold-rename-dir", &customer(), &before);
    let after = declared(&[("dbo.customer", "sales/customer.yml", &["id", "name"])]);
    let dir = project.ids_dir();
    let write = plan_keyed(&project, &customer(), &after, PathKey::folding()).unwrap();
    assert!(write.files.is_empty() && write.removals.is_empty());
    assert_eq!(write.respell, ["sales", "sales/customer.json"]);
    write.apply().unwrap();
    assert_eq!(names_in(&dir), ["sales"]);
    assert_eq!(names_in(&dir.join("sales")), ["customer.json"]);
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    assert!(
        plan_keyed(&project, &customer(), &after, PathKey::folding())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_spelling_the_disk_has_stays_while_any_declaration_asks_for_it() {
    // Two declarations spell one directory two ways (they were loaded from a
    // filesystem that tells them apart). Either spelling would do; the one on
    // disk stays, or every plan would ask for the other.
    let before = declared(&[
        ("dbo.customer", "Sales/a.yml", &["id", "name"]),
        ("dbo.other", "Sales/b.yml", &[]),
    ]);
    let project = written("fold-two-spellings", &two_tables(), &before);
    let after = declared(&[
        ("dbo.customer", "Sales/a.yml", &["id", "name"]),
        ("dbo.other", "sales/b.yml", &[]),
    ]);
    let write = plan_keyed(&project, &two_tables(), &after, PathKey::folding()).unwrap();
    assert!(write.is_empty(), "{:?}", write.respell);
    // Negative: when no declaration asks for it any more, it goes.
    let after = declared(&[
        ("dbo.customer", "sales/a.yml", &["id", "name"]),
        ("dbo.other", "sales/b.yml", &[]),
    ]);
    let write = plan_keyed(&project, &two_tables(), &after, PathKey::folding()).unwrap();
    assert_eq!(write.respell, ["sales"]);
}

#[test]
fn a_dropped_objects_file_asks_for_no_spelling_and_follows_its_directory() {
    // `dbo.customer` is dropped and keeps `Sales/customer.json`; the live
    // `dbo.other` beside it is now declared under `sales/`.
    let before = declared(&[
        ("dbo.customer", "Sales/customer.yml", &["id", "name"]),
        ("dbo.other", "Sales/other.yml", &[]),
    ]);
    let project = written("fold-dropped-dir", &two_tables(), &before);
    let after_ids = ids(json!({
        "version": 1,
        "tables": {"t_bbbbbb": "dbo.other"},
        "tombstones": {
            T: {"was": "dbo.customer", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa1": {"was": "dbo.customer.id", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
            "c_aaaaa2": {"was": "dbo.customer.name", "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
        },
    }));
    let paths = declared(&[("dbo.other", "sales/other.yml", &[])]);
    let dir = project.ids_dir();
    let write = plan_keyed(&project, &after_ids, &paths, PathKey::folding()).unwrap();
    assert!(write.removals.is_empty(), "{:?}", write.removals);
    assert_eq!(write.respell, ["sales"]);
    write.apply().unwrap();
    assert_eq!(names_in(&dir), ["sales"]);
    assert_eq!(
        names_in(&dir.join("sales")),
        ["customer.json", "other.json"]
    );
    assert_eq!(read_opt(&project).unwrap(), Some(after_ids.clone()));
    assert!(
        plan_keyed(&project, &after_ids, &paths, PathKey::folding())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_respelling_never_takes_the_place_of_another_entry() {
    let before = declared(&[("dbo.customer", "Customer.yml", &["id", "name"])]);
    let project = written("fold-rename-twins", &customer(), &before);
    let dir = project.ids_dir();
    if PathKey::probe(&dir).unwrap() == PathKey::folding() {
        return; // two spellings of one name cannot both be here
    }
    // The key says these are one name and this directory holds both: it does
    // not fold them, and neither is the other's spelling.
    std::fs::write(dir.join("customer.json"), "other").unwrap();
    let respell = |rel: &str| TreeWrite {
        dir: dir.clone(),
        respell: vec![rel.to_owned()],
        key: PathKey::folding(),
        ..TreeWrite::default()
    };
    respell("customer.json").apply().unwrap();
    assert_eq!(names_in(&dir), ["Customer.json", "customer.json"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("customer.json")).unwrap(),
        "other"
    );
    // An entry that is not there (the write removed it) is nothing to rename.
    respell("gone/customer.json").apply().unwrap();
    respell("gone.json").apply().unwrap();
    assert_eq!(names_in(&dir), ["Customer.json", "customer.json"]);
}

#[test]
fn a_rename_through_a_free_name_refuses_a_name_that_is_taken_and_puts_the_entry_back() {
    let dir = project("rename-through").ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Customer.json"), "mine").unwrap();
    std::fs::write(dir.join("other.json"), "theirs").unwrap();
    let via = dir.join(".pbps-respell0.json");
    // Where the wanted name is another entry (an alias the key does not
    // model), a plain rename would replace it without a word.
    let err =
        rename_through(&dir.join("Customer.json"), &via, &dir.join("other.json")).unwrap_err();
    assert!(
        format!("{err:#}").contains("could not be renamed"),
        "{err:#}"
    );
    assert_eq!(names_in(&dir), ["Customer.json", "other.json"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("other.json")).unwrap(),
        "theirs"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("Customer.json")).unwrap(),
        "mine"
    );
    // A free name is taken, and nothing is left in the middle.
    rename_through(&dir.join("Customer.json"), &via, &dir.join("client.json")).unwrap();
    assert_eq!(names_in(&dir), ["client.json", "other.json"]);
}

#[test]
fn a_move_between_two_spellings_of_one_name_is_told_apart_from_any_other_move() {
    let before = declared(&[("dbo.customer", "Sales/Customer.yml", &["id", "name"])]);
    let moved = |name: &str, to: &str, key: PathKey| {
        let project = written(name, &customer(), &before);
        let after = declared(&[("dbo.customer", to, &["id", "name"])]);
        let write = plan_keyed(&project, &customer(), &after, key).unwrap();
        write
            .case_only_move()
            .map(|(old, new)| (old.to_owned(), new.to_owned()))
    };
    // Case is significant (a CI checkout): the tree has the old spelling, the
    // declaration the new one, and the plan is a move between the two.
    assert_eq!(
        moved("case-move-file", "Sales/customer.yml", PathKey::default()),
        Some((
            "Sales/Customer.json".to_owned(),
            "Sales/customer.json".to_owned()
        ))
    );
    assert_eq!(
        moved("case-move-dir", "sales/Customer.yml", PathKey::default()),
        Some((
            "Sales/Customer.json".to_owned(),
            "sales/Customer.json".to_owned()
        ))
    );
    // Negative: another name is an ordinary move.
    assert_eq!(
        moved("case-move-other", "Sales/client.yml", PathKey::default()),
        None
    );
    // Negative: where case folds there is no move at all, only a spelling.
    assert_eq!(
        moved("case-move-fold", "Sales/customer.yml", PathKey::folding()),
        None
    );
}

#[test]
fn a_respelling_interrupted_halfway_reads_back_and_the_next_plan_finishes_it() {
    // The name in the middle of the two renames, as `respell_entries` picks it.
    let middle = |path: &Path| {
        crate::ids_cmd::unused_sibling(
            path,
            "respell",
            ".json",
            &mut BTreeSet::new(),
            PathKey::folding(),
        )
        .unwrap()
    };
    // A file.
    let before = declared(&[("dbo.customer", "Customer.yml", &["id", "name"])]);
    let project = written("fold-rename-crash", &customer(), &before);
    let dir = project.ids_dir();
    let via = middle(&dir.join("Customer.json"));
    assert_eq!(via, dir.join(".pbps-respell0.json"));
    std::fs::rename(dir.join("Customer.json"), &via).unwrap();
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    let after = declared(&[("dbo.customer", "customer.yml", &["id", "name"])]);
    for key in [PathKey::folding(), PathKey::default()] {
        let write = plan_keyed(&project, &customer(), &after, key).unwrap();
        assert_eq!(write.files.keys().collect::<Vec<_>>(), ["customer.json"]);
    }
    plan_keyed(&project, &customer(), &after, PathKey::folding())
        .unwrap()
        .apply()
        .unwrap();
    assert_eq!(names_in(&dir), ["customer.json"]);
    // A directory: what it holds is read where it is, and moves out of it.
    let before = declared(&[("dbo.customer", "Sales/customer.yml", &["id", "name"])]);
    let project = written("fold-rename-crash-dir", &customer(), &before);
    let dir = project.ids_dir();
    let via = middle(&dir.join("Sales"));
    std::fs::rename(dir.join("Sales"), &via).unwrap();
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    let after = declared(&[("dbo.customer", "sales/customer.yml", &["id", "name"])]);
    plan_keyed(&project, &customer(), &after, PathKey::folding())
        .unwrap()
        .apply()
        .unwrap();
    assert_eq!(names_in(&dir), ["sales"]);
    assert_eq!(names_in(&dir.join("sales")), ["customer.json"]);
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
}

#[test]
fn an_aside_name_is_never_one_windows_keeps_for_a_device() {
    let uid: Uid = T.parse().unwrap();
    let aside = |was: &str| crate::ids_cmd::aside(was, &uid);
    // Windows takes the part before the first dot, in any case.
    for was in ["aux.gone", "CON.x", "nul", "Prn.a.b", "com1.x", "LPT9.x"] {
        assert_eq!(aside(was), format!("_{was}.t_aaaaaa.json"));
    }
    // Negative: a longer or shorter name is nobody's device.
    for was in [
        "auxiliary.gone",
        "dbo.aux",
        "com.x",
        "com10.x",
        "lpt.x",
        "console.x",
    ] {
        assert_eq!(aside(was), format!("{was}.t_aaaaaa.json"));
    }
}

/// `PathKey::path` joins with the platform's separator and a relative path
/// keeps its `/`; on Windows that is one path spelled two ways.
#[cfg(windows)]
#[test]
fn a_path_key_spells_both_separators_one_way() {
    let key = PathKey::default();
    assert_eq!(
        key.path(&Path::new("ids").join("a/b.json")),
        key.path(&Path::new("ids").join("a").join("b.json"))
    );
    assert_eq!(key.path(Path::new(r"ids\a\b.json")), "ids/a/b.json");
}

#[test]
fn generated_names_avoid_a_planned_path_that_differs_only_in_case_when_case_folds() {
    // The write plans `<dest>` as a directory; its file `<file>` stages a temp
    // file that would take the directory's name in another case.
    for (file, dest) in [
        ("-a.json", ".PBPS-tmp0/b.json"),
        ("-A.json", ".pbps-TMP0/b.json"),
    ] {
        for (key, expect) in [(PathKey::folding(), "tmp1"), (PathKey::default(), "tmp0")] {
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
                "",
                &mut reserved,
                key,
            )
            .unwrap();
            assert_eq!(
                tmp.file_name().unwrap().to_string_lossy(),
                format!(".pbps-{expect}"),
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

#[cfg(unix)]
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
            assert!(dir.join(".pbps-aside0").exists(), "the old file is kept");
        }
    }
}

#[test]
fn the_redirect_compares_components_by_key() {
    let asides = [(
        PathBuf::from("/ids/A.json"),
        PathBuf::from("/ids/.pbps-aside0"),
    )];
    let at = |p: &str, key| redirected(Path::new(p), &asides, key);
    let fold = PathKey::folding();
    assert_eq!(at("/ids/a.json", fold), Path::new("/ids/.pbps-aside0"));
    assert_eq!(
        at("/ids/a.JSON/b.json", fold),
        Path::new("/ids/.pbps-aside0/b.json")
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
        // Where the real filesystem folds case, a spelling that differs from
        // the aside's name only in case IS that name there, so a plan made with
        // the case-significant key cannot be written on it (the aside's
        // directory and the aside file are one entry); the spelling in the
        // aside's own case still can.
        let writable_here =
            !case_insensitive(&project.ids_dir()) || spelling == spelling.to_lowercase();
        if key == PathKey::default() && writable_here {
            // The real filesystem here treats the names as the key does: write
            // it, read it back.
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

// -------------------------------------------- the probe, read-only first ----

#[cfg(unix)]
#[test]
fn an_empty_read_only_tree_directory_is_probed_without_a_write() {
    let project = project("probe-readonly");
    let dir = project.ids_dir();
    if unwritable(&dir).is_none() {
        return;
    }
    let before = listing(project.root());
    // The key comes from an entry in a directory above (`pbps.yml`); nothing is
    // written, so a no-op plan on a read-only checkout is a no-op, not an error.
    let key = PathKey::probe(&dir).unwrap();
    assert_eq!(key == PathKey::folding(), case_insensitive(project.root()));
    let write = plan(&project, &IdsFile::default(), &DeclaredPaths::default()).unwrap();
    writable(&dir);
    assert!(write.is_empty());
    assert_eq!(listing(project.root()), before, "nothing was written");
}

#[cfg(unix)]
#[test]
fn an_ancestor_on_another_device_says_nothing_about_this_filesystem() {
    let project = project("probe-device");
    let dir = project.ids_dir();
    if unwritable(&dir).is_none() {
        return;
    }
    let root = project.root().to_path_buf();
    let same = |_: &Path| Ok(7u64);
    // Only the tree's own directory is on this device; everything above is a
    // mount of something else, so there is nothing to look at and nothing may be
    // written: the answer that cannot lose a file.
    let other = |p: &Path| Ok(if p == dir { 7u64 } else { 8u64 });
    let looked = PathKey::probe_on(&dir, &same).unwrap();
    let blind = PathKey::probe_on(&dir, &other).unwrap();
    writable(&dir);
    assert_eq!(looked == PathKey::folding(), case_insensitive(&root));
    assert_eq!(blind, PathKey::folding());
    if !case_insensitive(&root) {
        assert_ne!(looked, blind, "the other device's entry was not used");
    }
}

#[test]
fn a_writable_directory_with_nothing_to_look_at_anywhere_gets_a_probe_file_and_loses_it() {
    let project = project("probe-write");
    let dir = project.ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let other = |p: &Path| Ok(if p == dir { 7u64 } else { 8u64 });
    let key = PathKey::probe_on(&dir, &other).unwrap();
    assert_eq!(key == PathKey::folding(), case_insensitive(&dir));
    assert!(names_in(&dir).is_empty(), "the probe file is removed again");
}

#[cfg(unix)]
#[test]
fn two_listed_spellings_of_one_name_are_a_case_sensitive_answer_even_when_they_are_one_file() {
    use std::os::unix::fs as ufs;
    for how in ["hard link", "symlink"] {
        let project = project(&format!("probe-linked-{}", how.replace(' ', "-")));
        let dir = project.ids_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // Asked before the second spelling is made: where case folds, `a` and
        // `A` cannot both exist, and the link fails with "already exists".
        if case_insensitive(&dir) {
            continue;
        }
        std::fs::write(dir.join("a"), "x").unwrap();
        if how == "hard link" {
            std::fs::hard_link(dir.join("a"), dir.join("A")).unwrap();
        } else {
            ufs::symlink("a", dir.join("A")).unwrap();
        }
        // `a` and `A` are one file and two entries: the directory tells them apart.
        assert_eq!(PathKey::probe(&dir).unwrap(), PathKey::default(), "{how}");
        // Negative: a single spelling is judged by whether its twin resolves.
        std::fs::remove_file(dir.join("A")).unwrap();
        assert_eq!(PathKey::probe(&dir).unwrap(), PathKey::default(), "{how}");
    }
}

#[cfg(unix)]
#[test]
fn a_dangling_symlink_spelled_like_the_twin_is_an_entry_not_a_fold() {
    use std::os::unix::fs as ufs;
    let project = project("probe-dangling");
    let dir = project.ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a"), "x").unwrap();
    if case_insensitive(&dir) {
        return;
    }
    ufs::symlink("nowhere", dir.join("A")).unwrap();
    assert_eq!(PathKey::probe(&dir).unwrap(), PathKey::default());
}

#[test]
fn the_protected_inputs_of_an_unreadable_tree_are_an_error_never_fewer_files() {
    let fresh = project("protected-fresh");
    assert_eq!(
        protected(&fresh).unwrap(),
        vec![fresh.config_file(), fresh.ids_file()],
        "a tree that is not there has no files to protect"
    );

    let project = written("protected-unreadable", &customer(), &customer_declared());
    let file = project
        .ids_dir()
        .join(tree(&project).keys().next().unwrap());
    assert!(protected(&project).unwrap().contains(&file));
    // A stray entry makes the listing fail. The identity file beside it can
    // still be written over through a hard link, so leaving it out of the list
    // would let a forced output through.
    std::fs::write(project.ids_dir().join("notes.txt"), "x").unwrap();
    let error = format!("{:#}", protected(&project).unwrap_err());
    assert!(error.contains("notes.txt"), "{error}");
}

// ------------------------------------------- one owner for a write (#2806) ----

/// A directory `a.json` is displaced by the file `a.json` (a move the other way
/// round), and `b.json` is a second new file, so two temp files are staged.
fn directory_becomes_file(dir: &Path) -> TreeWrite {
    std::fs::create_dir_all(dir.join("a.json")).unwrap();
    std::fs::write(dir.join("a.json/a.json"), "old").unwrap();
    TreeWrite {
        dir: dir.to_owned(),
        files: [
            ("a.json".to_owned(), "new".to_owned()),
            ("b.json".to_owned(), "new too".to_owned()),
        ]
        .into(),
        removals: vec!["a.json/a.json".to_owned()],
        ..TreeWrite::default()
    }
}

#[test]
fn a_failed_set_aside_of_a_displaced_directory_leaves_no_staged_file() {
    let project = project("set-aside-fails");
    let dir = project.ids_dir();
    let write = directory_becomes_file(&dir);
    let before = listing(&dir);
    // The only set-aside is the displaced directory's, in the second loop,
    // after both temp files exist.
    fail_set_aside_number(1);
    let error = format!("{:#}", write.apply().unwrap_err());
    assert!(error.contains("cannot move"), "{error}");
    assert!(error.contains("injected"), "{error}");
    assert!(!error.contains("could not be undone"), "{error}");
    assert_eq!(listing(&dir), before, "no staged entry is left");
    // The tree still reads (a stray `.tmp` would have refused it).
    assert_eq!(read_ids_tree(&dir).unwrap().len(), 1);
    // Negative: the same write, unarmed, goes through.
    write.apply().unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.json")).unwrap(), "new");
    assert_eq!(
        std::fs::read_to_string(dir.join("b.json")).unwrap(),
        "new too"
    );
}

#[test]
fn a_failure_after_the_first_placed_file_keeps_it_and_undoes_only_what_is_pending() {
    let project = project("fails-after-first-placement");
    let dir = project.ids_dir();
    std::fs::create_dir_all(dir.join("a.json")).unwrap();
    std::fs::write(dir.join("a.json/a.json"), "old a").unwrap();
    std::fs::create_dir_all(dir.join("c.json")).unwrap();
    std::fs::write(dir.join("c.json/c.json"), "old c").unwrap();
    let write = TreeWrite {
        dir: dir.clone(),
        files: [
            ("a.json".to_owned(), "new a".to_owned()),
            ("c.json".to_owned(), "new c".to_owned()),
        ]
        .into(),
        removals: vec!["a.json/a.json".to_owned(), "c.json/c.json".to_owned()],
        ..TreeWrite::default()
    };
    // `a.json` is placed (its directory set aside first); the second
    // set-aside, for `c.json`, fails.
    fail_set_aside_number(2);
    let error = format!("{:#}", write.apply().unwrap_err());
    // The placed file's aside is not pending, so the undo does not try to
    // put it back over the file.
    assert!(!error.contains("could not be undone"), "{error}");
    // The placed file stays, with the old copy beside it, as after a crash.
    assert_eq!(
        std::fs::read_to_string(dir.join("a.json")).unwrap(),
        "new a"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".pbps-aside0/a.json")).unwrap(),
        "old a"
    );
    // What was pending is as it was: no temp file, the directory in place.
    assert_eq!(
        std::fs::read_to_string(dir.join("c.json/c.json")).unwrap(),
        "old c"
    );
    assert_eq!(names_in(&dir), [".pbps-aside0", "a.json", "c.json"]);
}

#[test]
fn a_write_that_succeeds_keeps_every_file_after_the_guard_drops() {
    let project = project("guard-commits");
    let dir = project.ids_dir();
    let write = TreeWrite {
        dir: dir.clone(),
        files: [
            ("new/a.json".to_owned(), "a".to_owned()),
            ("b.json".to_owned(), "b".to_owned()),
        ]
        .into(),
        ..TreeWrite::default()
    };
    write.apply().unwrap();
    // The directory the write created is not taken away with the guard.
    assert_eq!(names_in(&dir), ["b.json", "new"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("new/a.json")).unwrap(),
        "a"
    );
    // A guard dropped unsettled undoes, one that was committed does not.
    let kept = dir.join("kept.json");
    let lost = dir.join("lost.json");
    std::fs::write(&kept, "k").unwrap();
    std::fs::write(&lost, "l").unwrap();
    let mut committed = Staged::default();
    committed.temp(&kept);
    committed.commit();
    drop(committed);
    let mut dropped = Staged::default();
    dropped.temp(&lost);
    drop(dropped);
    assert!(kept.exists(), "a committed write keeps its files");
    assert!(
        !lost.exists(),
        "an unsettled one takes its staged file away"
    );
}

#[test]
fn an_undo_that_fails_is_reported_with_the_error_that_caused_it() {
    let project = project("guard-undo-fails");
    let dir = project.ids_dir();
    std::fs::create_dir_all(dir.join("a.json")).unwrap();
    std::fs::write(dir.join("a.json/foreign"), "x").unwrap();
    std::fs::write(dir.join(".pbps-aside0"), "old").unwrap();
    let mut staged = Staged::default();
    staged.created([dir.join("a.json")]);
    staged
        .asides
        .push((dir.join("a.json"), dir.join(".pbps-aside0")));
    let error = format!(
        "{:#}",
        staged
            .settle::<()>(Err(anyhow::anyhow!("the original failure")))
            .unwrap_err()
    );
    assert!(error.contains("the original failure"), "{error}");
    assert!(error.contains("could not be undone"), "{error}");
    assert!(error.contains(".pbps-aside0"), "{error}");
    assert_eq!(
        std::fs::read_to_string(dir.join(".pbps-aside0")).unwrap(),
        "old"
    );
}

// ---------------------------------- a name that cannot be inspected (#2816) ----

#[cfg(unix)]
#[test]
fn a_sibling_that_cannot_be_inspected_is_an_error_naming_it_and_an_absent_one_is_picked() {
    use std::os::unix::fs::PermissionsExt;
    let dir = project("sibling-uninspectable").ids_dir();
    std::fs::create_dir_all(dir.join("closed")).unwrap();
    let path = dir.join("closed/a.json");
    let pick = |path: &Path| {
        crate::ids_cmd::unused_sibling(path, "tmp", "", &mut BTreeSet::new(), PathKey::default())
    };
    // Negative: a candidate that is not there is picked.
    assert_eq!(pick(&path).unwrap(), dir.join("closed/.pbps-tmp0"));
    // The directory cannot be searched, so nothing in it can be looked at.
    std::fs::set_permissions(dir.join("closed"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let probe = std::fs::symlink_metadata(dir.join("closed/.pbps-tmp0"));
    if probe.is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        // Root ignores permission bits, so the fault cannot be injected.
        std::fs::set_permissions(dir.join("closed"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        return;
    }
    let error = format!("{:#}", pick(&path).unwrap_err());
    // The caller sees it too: the atomic write stops before it creates anything.
    let written = format!("{:#}", write_atomic(&path, "x").unwrap_err());
    std::fs::set_permissions(dir.join("closed"), std::fs::Permissions::from_mode(0o755)).unwrap();
    for shown in [&error, &written] {
        assert!(shown.contains("closed/.pbps-tmp0"), "{shown}");
        assert!(shown.contains("cannot inspect"), "{shown}");
    }
    assert_eq!(names_in(&dir.join("closed")), Vec::<String>::new());
}

// ------------------------------------------- names that fit (#2817) ----

/// A declaration stem of this many bytes mirrors to `<stem>.json`.
fn long_stem(len: usize) -> String {
    "x".repeat(len)
}

#[test]
fn a_declaration_with_a_246_byte_stem_plans_and_writes_its_identity_file() {
    let stem = long_stem(246);
    let file = format!("{stem}.json");
    let paths = declared(&[("dbo.customer", &format!("{stem}.yml"), &["id", "name"])]);
    let project = project("long-stem");
    // The staging name used to be `.<251 bytes>.tmp` (256 bytes).
    plan(&project, &customer(), &paths)
        .unwrap()
        .apply()
        .unwrap();
    assert_eq!(names_in(&project.ids_dir()), std::slice::from_ref(&file));
    assert_eq!(read_opt(&project).unwrap(), Some(customer()));
    assert!(plan(&project, &customer(), &paths).unwrap().is_empty());
    // Negative: an ordinary name is written as before.
    let project = written("short-stem", &customer(), &customer_declared());
    assert_eq!(names_in(&project.ids_dir()), ["dbo"]);
}

#[test]
fn a_move_of_a_file_to_a_directory_with_a_long_name_sets_the_old_file_aside_under_a_name_that_fits()
{
    let name = format!("{}.json", long_stem(246));
    let project = project("long-set-aside");
    let dir = project.ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(&name), "old").unwrap();
    let write = TreeWrite {
        dir: dir.clone(),
        files: [(format!("{name}/x.json"), "new".to_owned())].into(),
        removals: vec![name.clone()],
        ..TreeWrite::default()
    };
    // The aside used to be `.<name>.aside` (257 bytes).
    write.apply().unwrap();
    assert_eq!(names_in(&dir), std::slice::from_ref(&name));
    assert_eq!(
        std::fs::read_to_string(dir.join(&name).join("x.json")).unwrap(),
        "new"
    );
}

#[test]
fn a_respelling_of_a_long_name_goes_through_a_name_that_fits() {
    let project = project("long-respell");
    let dir = project.ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    // Where names are distinct (Linux) `Xxx.json` is another entry than
    // `xxx.json`; the folding key makes the write treat it as the old spelling.
    let old = format!("X{}.json", long_stem(245));
    let wanted = format!("x{}.json", long_stem(245));
    std::fs::write(dir.join(&old), "mine").unwrap();
    let write = TreeWrite {
        dir: dir.clone(),
        respell: vec![wanted.clone()],
        key: PathKey::folding(),
        ..TreeWrite::default()
    };
    // The middle name used to be `.<old>.respell.json` (265 bytes).
    write.apply().unwrap();
    assert_eq!(names_in(&dir), std::slice::from_ref(&wanted));
    assert_eq!(std::fs::read_to_string(dir.join(&wanted)).unwrap(), "mine");
}

#[test]
fn a_dropped_object_with_a_long_name_goes_aside_to_a_name_that_fits_and_reads_back() {
    let uid: Uid = T.parse().unwrap();
    let was = format!("dbo.{}", long_stem(250));
    let name = crate::ids_cmd::aside(&was, &uid);
    assert!(name.len() <= 240, "{} bytes", name.len());
    assert!(
        name.starts_with("~pbps-") && name.ends_with(".t_aaaaaa.json"),
        "{name}"
    );
    // The table is dropped and another is declared at its path, so its file
    // is kept under the aside name; the filesystem takes it and it reads back.
    let before = ids(json!({"version": 1, "tables": {T: &was}}));
    let project = written(
        "long-aside",
        &before,
        &declared(&[(&was, "dbo/c.yml", &[])]),
    );
    let after = ids(json!({
        "version": 1,
        "tables": {"t_bbbbbb": "dbo.other"},
        "tombstones": {
            T: {"was": &was, "dropped_at": "2026-11-06", "reason": "r", "operator": "o"},
        },
    }));
    let paths = declared(&[("dbo.other", "dbo/c.yml", &[])]);
    plan(&project, &after, &paths).unwrap().apply().unwrap();
    assert_eq!(
        tree(&project).keys().collect::<Vec<_>>(),
        [&"dbo/c.json".to_owned(), &name]
    );
    assert_eq!(read_opt(&project).unwrap(), Some(after.clone()));
    assert!(plan(&project, &after, &paths).unwrap().is_empty());
}

#[test]
fn aside_names_keep_their_readable_form_up_to_the_limit_and_are_hashed_past_it() {
    let uid: Uid = T.parse().unwrap();
    let suffix = ".t_aaaaaa.json".len();
    // Exactly at the limit is still today's name.
    let at_limit = "n".repeat(240 - suffix);
    assert_eq!(
        crate::ids_cmd::aside(&at_limit, &uid),
        format!("{at_limit}.t_aaaaaa.json")
    );
    // One byte more is hashed, deterministically, and differently per name.
    let past = "n".repeat(240 - suffix + 1);
    let other = format!("m{}", &past[1..]);
    let (a, b) = (
        crate::ids_cmd::aside(&past, &uid),
        crate::ids_cmd::aside(&other, &uid),
    );
    assert!(
        a.starts_with("~pbps-") && a.ends_with(".t_aaaaaa.json"),
        "{a}"
    );
    assert_eq!(a, crate::ids_cmd::aside(&past, &uid));
    assert_ne!(a, b);
    // A `~` in the object name is replaced, so no readable name is a hashed one.
    assert_eq!(
        crate::ids_cmd::aside("~pbps-x", &uid),
        "_pbps-x.t_aaaaaa.json"
    );
}

#[test]
fn the_names_a_write_derives_are_refused_or_read_by_the_rules_they_always_had() {
    let dir = project("derived-name-rules").ids_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.json"), "{}").unwrap();
    // A staging name and an aside are not identity files: the reader refuses a
    // tree that holds one, and names it.
    for stray in [".pbps-tmp0", ".pbps-aside0", ".pbps-aside12"] {
        std::fs::write(dir.join(stray), "x").unwrap();
        let error = read_ids_tree(&dir).unwrap_err().to_string();
        assert!(error.contains(stray), "{stray}: {error}");
        std::fs::remove_file(dir.join(stray)).unwrap();
    }
    // The respelling step's name is one, so a crash between its two renames
    // reads back (`a_respelling_interrupted_halfway_reads_back...`).
    assert!(ids_tree::is_identity_file(".pbps-respell0.json"));
    assert!(!ids_tree::is_identity_file(".pbps-tmp0"));
    assert!(!ids_tree::is_identity_file(".pbps-aside0"));
    assert_eq!(read_ids_tree(&dir).unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn an_uninspectable_ancestor_stops_the_write_with_an_error_naming_it_and_leaves_the_tree_as_it_was()
{
    use std::os::unix::fs::PermissionsExt;
    let dir = project("ancestor-uninspectable").ids_dir();
    std::fs::create_dir_all(dir.join("d/e")).unwrap();
    std::fs::write(dir.join("d/e/old.json"), "old").unwrap();
    let before = listing(&dir);
    let write = TreeWrite {
        dir: dir.clone(),
        // `a.json` is staged first, so there is a temp file to take away.
        files: [
            ("a.json".to_owned(), "a".to_owned()),
            ("d/e/x.json".to_owned(), "x".to_owned()),
        ]
        .into(),
        ..TreeWrite::default()
    };
    // `d` cannot be searched, so `d/e` cannot be looked at: not "absent".
    std::fs::set_permissions(dir.join("d"), std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::symlink_metadata(dir.join("d/e")).is_ok() {
        // Root ignores permission bits, so the fault cannot be injected.
        std::fs::set_permissions(dir.join("d"), std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let error = format!("{:#}", write.apply().unwrap_err());
    std::fs::set_permissions(dir.join("d"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(error.contains("cannot inspect"), "{error}");
    assert!(error.contains("d/e"), "{error}");
    assert!(!error.contains("could not be undone"), "{error}");
    assert_eq!(listing(&dir), before, "the staged temp file is gone");
    // Negative: with the directory searchable again the same write goes through.
    write.apply().unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("d/e/x.json")).unwrap(),
        "x"
    );
}
