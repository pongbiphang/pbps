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
