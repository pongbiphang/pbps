//! The paths the identity tree mirrors come from the loader's own sources.
use pbps_load::{DeclaredPathError, load_schema_dir, load_schema_files};
use pbps_model::diagnostic::Declaration;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "pbps-declared-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn write(&self, rel: &str, text: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn table(name: &str, columns: &[&str]) -> String {
    let columns: String = columns
        .iter()
        .map(|c| format!("  {c}: {{type: int}}\n"))
        .collect();
    format!("table: {name}\ncolumns:\n{columns}")
}

fn table_decl(name: &str) -> Declaration {
    Declaration::Table(name.parse().unwrap())
}

#[test]
fn a_nested_declaration_is_recorded_by_its_path_below_the_schema_directory() {
    let dir = Dir::new();
    dir.write("sales/eu/customer.yml", &table("dbo.customer", &["id"]));
    dir.write("orders.yml", &table("dbo.orders", &["id"]));
    let loaded = load_schema_dir(&dir.0).unwrap();
    let paths = loaded.declared_paths(&dir.0).unwrap();
    let path = |name| paths.files[&table_decl(name)].path.as_str();
    assert_eq!(path("dbo.customer"), "sales/eu/customer.yml");
    assert_eq!(path("dbo.orders"), "orders.yml");
    assert_eq!(paths.files.len(), 2);
}

#[test]
fn yml_and_yaml_files_are_recorded_with_the_extension_they_have() {
    let dir = Dir::new();
    dir.write("a.yml", &table("dbo.a", &["id"]));
    dir.write("b.yaml", &table("dbo.b", &["id"]));
    let paths = load_schema_dir(&dir.0)
        .unwrap()
        .declared_paths(&dir.0)
        .unwrap();
    assert_eq!(paths.files[&table_decl("dbo.a")].path, "a.yml");
    assert_eq!(paths.files[&table_decl("dbo.b")].path, "b.yaml");
}

#[test]
fn a_tables_columns_keep_the_order_of_the_yaml_not_the_name_order() {
    let dir = Dir::new();
    dir.write("t.yml", &table("dbo.t", &["zeta", "alpha", "mid"]));
    let paths = load_schema_dir(&dir.0)
        .unwrap()
        .declared_paths(&dir.0)
        .unwrap();
    let columns: Vec<_> = paths.files[&table_decl("dbo.t")]
        .columns
        .iter()
        .map(|c| c.raw().to_owned())
        .collect();
    assert_eq!(columns, ["zeta", "alpha", "mid"]);
}

#[test]
fn only_declarations_that_carry_a_uid_have_a_path() {
    let dir = Dir::new();
    dir.write("t.yml", &table("dbo.t", &["id"]));
    dir.write("r.yml", "role: app_reader\n");
    dir.write("e.yml", "enum: app.mood\nvalues: [happy, sad]\n");
    dir.write(
        "x.yml",
        "extension: citext\nschema: public\nversion: \"1.6\"\n",
    );
    dir.write("v.yml", "view: dbo.v\ndefinition: SELECT 1\n");
    let paths = load_schema_dir(&dir.0)
        .unwrap()
        .declared_paths(&dir.0)
        .unwrap();
    let kinds: Vec<&str> = paths
        .files
        .keys()
        .map(|d| match d {
            Declaration::Table(_) => "table",
            Declaration::Role(_) => "role",
            Declaration::Type(_) => "type",
            Declaration::Extension(_) => "extension",
            Declaration::Module(_) => "module",
            Declaration::Search(_) => "search",
        })
        .collect();
    assert!(!kinds.contains(&"module"), "{kinds:?}");
    assert_eq!(kinds.len(), 4, "{kinds:?}");
    assert!(paths.files.values().any(|f| f.path == "r.yml"));
    assert!(paths.files.values().any(|f| f.path == "e.yml"));
    assert!(paths.files.values().any(|f| f.path == "x.yml"));
}

#[test]
fn a_partition_child_has_no_file_of_its_own_and_names_its_parent() {
    let dir = Dir::new();
    dir.write(
        "events.yml",
        "table: app.events\ncolumns:\n  ts: {type: timestamptz, nullable: false}\n\
         partition_by: [ts]\npartitions:\n  app.events_a: {from: ['2026-01-01'], to: ['2026-02-01']}\n",
    );
    let paths = load_schema_dir(&dir.0)
        .unwrap()
        .declared_paths(&dir.0)
        .unwrap();
    assert!(paths.files.contains_key(&table_decl("app.events")));
    assert!(!paths.files.contains_key(&table_decl("app.events_a")));
    let child = "app.events_a".parse().unwrap();
    assert_eq!(paths.children[&child], "app.events".parse().unwrap());
}

#[test]
fn files_loaded_from_memory_are_recorded_by_their_own_relative_paths() {
    let files = vec![(PathBuf::from("a/b.yml"), table("dbo.t", &["id"]))];
    let paths = load_schema_files(&files)
        .unwrap()
        .declared_paths(Path::new(""))
        .unwrap();
    assert_eq!(paths.files[&table_decl("dbo.t")].path, "a/b.yml");
}

#[test]
fn a_declaration_outside_the_named_directory_is_refused_not_guessed() {
    let dir = Dir::new();
    dir.write("t.yml", &table("dbo.t", &["id"]));
    let loaded = load_schema_dir(&dir.0).unwrap();
    let error = loaded.declared_paths(&dir.0.join("elsewhere")).unwrap_err();
    assert!(
        matches!(error, DeclaredPathError::Outside { .. }),
        "{error}"
    );
}
