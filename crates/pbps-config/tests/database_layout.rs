use pbps_config::{Config, Project};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "pbps-config-a1-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn config(&self, text: &str) -> PathBuf {
        let path = self.0.join("pbps.yml");
        std::fs::write(&path, text).unwrap();
        path
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
const NAMED: &str = "dialect: mssql\ndatabases: [alpha, beta]\n";

#[test]
fn named_project_paths_keep_an_identity_file_in_each_database_directory() {
    let dir = Dir::new();
    let path = dir.config(NAMED);
    let project = Project::load_project(&path).unwrap();
    let paths = project.database_paths().unwrap();
    for (db, paths) in paths.iter() {
        assert_eq!(paths.schema_dir, dir.0.join("schema").join(db.raw()));
        assert_eq!(paths.ids_file, paths.schema_dir.join("schema.ids.json"));
    }
    let text = serde_json::to_string(&project.config).unwrap();
    assert_eq!(Config::parse_project(&text, &path).unwrap(), project.config);
    assert!(Project::load(&path).is_err());
    assert!(
        Config::parse(NAMED, &path)
            .unwrap_err()
            .to_string()
            .contains("does not yet support instance-wide")
    );
}

#[test]
fn absent_databases_preserves_flat_configuration_bytes_and_paths() {
    let dir = Dir::new();
    let path = dir.config("dialect: mssql\n");
    let project = Project::load(&path).unwrap();
    assert_eq!(project.schema_dir(), dir.0.join("schema"));
    assert_eq!(project.ids_file(), dir.0.join("schema.ids.json"));
    assert!(project.database_paths().is_err());
    assert_eq!(
        serde_json::to_string(&project.config).unwrap(),
        r#"{"dialect":"mssql","schema_dir":"schema","ids_file":"schema.ids.json","hooks":{},"unmanaged":"ignore"}"#
    );
    assert_eq!(
        Config::parse_project("dialect: mssql\n", &path).unwrap(),
        project.config
    );
}

#[test]
fn malformed_duplicate_unusable_or_postgres_database_lists_are_refused() {
    for text in [
        "dialect: mssql\ndatabases: []",
        "dialect: mssql\ndatabases: null",
        "dialect: mssql\ndatabases: [a, a]",
        "dialect: postgres\ndatabases: [a]",
        "dialect: mssql\ndatabases: [../a]",
        "dialect: mssql\ndatabases: [C:one]",
        "dialect: mssql\ndatabases: [1]",
        "dialect: mssql\ndatabases: [a]\nids_file: flat.json",
        "dialect: mssql\ndatabases: [a]\ndatabases: [b]",
    ] {
        assert!(
            Config::parse_project(text, Path::new("pbps.yml")).is_err(),
            "{text}"
        );
    }
}

#[test]
fn a_flat_identity_file_cannot_silently_disappear_when_switching_layout() {
    let dir = Dir::new();
    let path = dir.config(NAMED);
    std::fs::write(dir.0.join("schema.ids.json"), "{}").unwrap();
    assert!(
        Project::load_project(&path)
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
    std::fs::remove_file(dir.0.join("schema.ids.json")).unwrap();
    assert!(Project::load_project(&path).is_ok());
}

fn parse(text: &str) -> Result<Config, String> {
    Config::parse_project(text, Path::new("pbps.yml")).map_err(|e| e.to_string())
}

#[test]
fn ids_dir_defaults_to_the_identity_tree_beside_the_project_and_is_not_serialized() {
    let dir = Dir::new();
    let project = Project::load(&dir.config("dialect: postgres\n")).unwrap();
    assert_eq!(project.config.ids_dir, None);
    assert_eq!(project.ids_dir(), dir.0.join(".pbps/ids"));
    // The legacy file is still the one in use, and nothing new is written out.
    assert_eq!(project.ids_file(), dir.0.join("schema.ids.json"));
    assert!(
        !serde_json::to_string(&project.config)
            .unwrap()
            .contains("ids_dir")
    );
}

#[test]
fn a_named_ids_dir_is_kept_relative_to_the_project_root() {
    let dir = Dir::new();
    let project = Project::load(&dir.config("dialect: postgres\nids_dir: meta/ids\n")).unwrap();
    assert_eq!(project.ids_dir(), dir.0.join("meta/ids"));
    let text = serde_json::to_string(&project.config).unwrap();
    assert_eq!(parse(&text).unwrap(), project.config);
}

#[test]
fn ids_file_alone_stays_valid_until_the_switch() {
    let config = parse("dialect: postgres\nids_file: db/ids.json\n").unwrap();
    assert_eq!(config.ids_file, PathBuf::from("db/ids.json"));
    assert_eq!(config.ids_dir, None);
}

#[test]
fn moving_ids_file_while_naming_ids_dir_is_refused_but_its_default_is_not() {
    let error =
        parse("dialect: postgres\nids_file: db/ids.json\nids_dir: .pbps/ids\n").unwrap_err();
    assert!(error.contains("`ids_dir` replaces `ids_file`"), "{error}");
    // A config written out always carries the default `ids_file`, so a
    // round trip through the typed value must read back.
    let config = parse("dialect: postgres\nids_file: schema.ids.json\nids_dir: .pbps/ids\n");
    assert!(config.is_ok(), "{config:?}");
}

#[test]
fn an_ids_dir_that_is_the_project_or_holds_the_declarations_is_refused() {
    for ids_dir in [".", "./", "..", "../..", "./."] {
        let error = parse(&format!("dialect: postgres\nids_dir: \"{ids_dir}\"\n")).unwrap_err();
        assert!(error.contains("project root"), "{ids_dir:?}: {error}");
    }
    for (ids_dir, schema_dir) in [
        ("schema", "schema"),
        ("./schema/", "schema"),
        ("db", "db/tables"),
    ] {
        let text = format!("dialect: postgres\nschema_dir: {schema_dir}\nids_dir: {ids_dir}\n");
        let error = parse(&text).unwrap_err();
        assert!(error.contains("holding it"), "{ids_dir:?}: {error}");
    }
    // Beside, inside or elsewhere is fine: only the declarations' own
    // directory is off limits.
    for (ids_dir, schema_dir) in [
        (".pbps/ids", "schema"),
        ("schema/ids", "schema"),
        ("schemata", "schema"),
        ("db/ids", "db/tables"),
    ] {
        let text = format!("dialect: postgres\nschema_dir: {schema_dir}\nids_dir: {ids_dir}\n");
        assert!(parse(&text).is_ok(), "{ids_dir:?} beside {schema_dir:?}");
    }
}

#[test]
fn an_ids_dir_reaching_the_project_by_a_symlink_or_parent_is_refused_on_disk() {
    let dir = Dir::new();
    std::fs::create_dir(dir.0.join("schema")).unwrap();
    for ids_dir in [
        dir.0.display().to_string(),
        dir.0.join("schema").display().to_string(),
        format!("{}/schema/..", dir.0.display()),
    ] {
        let path = dir.config(&format!("dialect: postgres\nids_dir: '{ids_dir}'\n"));
        let error = Project::load(&path).unwrap_err().to_string();
        assert!(error.contains("`ids_dir`"), "{ids_dir}: {error}");
    }
    let path = dir.config(&format!(
        "dialect: postgres\nids_dir: '{}'\n",
        dir.0.join("elsewhere").display()
    ));
    assert!(Project::load(&path).is_ok());
}

#[test]
fn a_database_project_puts_each_identity_tree_under_ids_dir_by_database_name() {
    let dir = Dir::new();
    for (config, base) in [
        (NAMED.to_owned(), ".pbps/ids"),
        (format!("{NAMED}ids_dir: meta/ids\n"), "meta/ids"),
    ] {
        let project = Project::load_project(&dir.config(&config)).unwrap();
        for (db, paths) in project.database_paths().unwrap().iter() {
            assert_eq!(paths.ids_dir, dir.0.join(base).join(db.raw()));
            // The legacy file keeps its place until the switch.
            assert_eq!(paths.ids_file, paths.schema_dir.join("schema.ids.json"));
        }
    }
}

#[test]
fn a_database_project_still_refuses_a_project_level_ids_file_override() {
    let error = parse(&format!("{NAMED}ids_file: ids.json\n")).unwrap_err();
    assert!(error.contains("ids_file override"), "{error}");
}
