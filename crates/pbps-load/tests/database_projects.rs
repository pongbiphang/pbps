use pbps_load::{ProjectLoadError, load_database_dirs};
use pbps_model::{DatabaseMap, DatabaseName, IdsFile, UidKind};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "pbps-load-a1-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn paths(&self) -> DatabaseMap<PathBuf> {
        DatabaseMap::new(["alpha", "beta"].map(|name| {
            let path = self.0.join(name);
            std::fs::create_dir(&path).unwrap();
            (name.parse().unwrap(), path)
        }))
        .unwrap()
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn db(name: &str) -> DatabaseName {
    name.parse().unwrap()
}
fn declaration() -> &'static str {
    "table: dbo.same\ncolumns:\n  id: {type: int}\n"
}
fn identities(uid: &str) -> IdsFile {
    IdsFile {
        tables: [(uid.parse().unwrap(), "dbo.same".parse().unwrap())].into(),
        ..Default::default()
    }
}
fn write_ids(paths: &DatabaseMap<PathBuf>, name: &str, ids: &IdsFile) {
    std::fs::write(
        paths.get(&db(name)).unwrap().join("schema.ids.json"),
        serde_json::to_string(ids).unwrap(),
    )
    .unwrap();
}

#[test]
fn equal_names_load_into_distinct_schema_source_and_identity_views() {
    let dir = Dir::new();
    let paths = dir.paths();
    for (_, path) in paths.iter() {
        std::fs::write(path.join("table.yml"), declaration()).unwrap();
    }
    write_ids(&paths, "alpha", &identities("t_aaaaaa"));
    write_ids(&paths, "beta", &identities("t_bbbbbb"));
    let mut loaded = load_database_dirs(&paths).unwrap();
    assert_eq!(
        loaded.schemas().get(&db("alpha")),
        loaded.schemas().get(&db("beta"))
    );
    assert_ne!(
        loaded.ids().get(&db("alpha")),
        loaded.ids().get(&db("beta"))
    );
    assert_ne!(
        loaded.loaded().get(&db("alpha")).unwrap().sources,
        loaded.loaded().get(&db("beta")).unwrap().sources
    );
    let fresh = loaded.reservations().allocate(UidKind::Table);
    assert_ne!(fresh, "t_aaaaaa".parse().unwrap());
    assert_ne!(fresh, "t_bbbbbb".parse().unwrap());
    assert!(loaded.reservations().contains(&fresh));
}

#[test]
fn a_duplicate_inside_one_database_still_refuses_without_merging_the_others() {
    let dir = Dir::new();
    let paths = dir.paths();
    let alpha = paths.get(&db("alpha")).unwrap();
    std::fs::write(alpha.join("one.yml"), declaration()).unwrap();
    std::fs::write(alpha.join("two.yml"), declaration()).unwrap();
    let ProjectLoadError::Declarations { database, errors } =
        load_database_dirs(&paths).unwrap_err()
    else {
        panic!("the declarations must refuse")
    };
    assert_eq!(database, db("alpha"));
    assert!(!errors.is_empty());
}

#[test]
fn missing_ids_are_new_but_empty_malformed_and_unreadable_ids_are_errors() {
    let dir = Dir::new();
    let paths = dir.paths();
    assert!(load_database_dirs(&paths).is_ok());
    let file = paths.get(&db("alpha")).unwrap().join("schema.ids.json");
    for bytes in [Vec::new(), b"not json".to_vec(), vec![0xff]] {
        std::fs::write(&file, bytes).unwrap();
        assert!(load_database_dirs(&paths).is_err());
    }
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert!(load_database_dirs(&paths).is_err());
}

#[test]
fn a_cross_file_uid_collision_refuses_before_exposing_an_allocator() {
    let dir = Dir::new();
    let paths = dir.paths();
    for name in ["alpha", "beta"] {
        write_ids(&paths, name, &identities("t_aaaaaa"));
    }
    let error = load_database_dirs(&paths).unwrap_err().to_string();
    assert!(error.contains("alpha") && error.contains("beta") && error.contains("t_aaaaaa"));
    write_ids(&paths, "beta", &identities("t_bbbbbb"));
    assert!(load_database_dirs(&paths).is_ok());
}

#[test]
fn loose_declarations_and_undeclared_directories_are_not_silently_ignored() {
    let dir = Dir::new();
    let paths = dir.paths();
    std::fs::write(dir.0.join("old.yml"), declaration()).unwrap();
    assert!(load_database_dirs(&paths).is_err());
    std::fs::remove_file(dir.0.join("old.yml")).unwrap();
    std::fs::create_dir(dir.0.join("unlisted")).unwrap();
    assert!(load_database_dirs(&paths).is_err());
}

#[cfg(unix)]
#[test]
fn database_directory_aliases_and_escapes_and_dangling_ids_refuse() {
    use std::os::unix::fs::symlink;
    let dir = Dir::new();
    let paths = dir.paths();
    let (alpha, beta) = (
        paths.get(&db("alpha")).unwrap(),
        paths.get(&db("beta")).unwrap(),
    );
    std::fs::remove_dir(beta).unwrap();
    symlink(alpha, beta).unwrap();
    assert!(load_database_dirs(&paths).is_err());
    std::fs::remove_file(beta).unwrap();
    let outside = Dir::new();
    symlink(&outside.0, beta).unwrap();
    assert!(load_database_dirs(&paths).is_err());
    std::fs::remove_file(beta).unwrap();
    std::fs::create_dir(beta).unwrap();
    symlink(dir.0.join("missing"), alpha.join("schema.ids.json")).unwrap();
    assert!(load_database_dirs(&paths).is_err());
}

#[test]
fn rename_intent_is_kept_in_its_database_and_cannot_name_another_database() {
    let dir = Dir::new();
    let paths = dir.paths();
    let text = "table: dbo.same\nrenamed_from: dbo.old\ncolumns:\n  id: {type: int}\n";
    for (_, path) in paths.iter() {
        std::fs::write(path.join("table.yml"), text).unwrap();
    }
    let loaded = load_database_dirs(&paths).unwrap();
    let alpha = loaded.loaded().get(&db("alpha")).unwrap();
    let beta = loaded.loaded().get(&db("beta")).unwrap();
    assert!(!alpha.intents.is_empty());
    assert_eq!(alpha.intents, beta.intents);
    assert_ne!(alpha.sources, beta.sources);
    std::fs::write(
        paths.get(&db("alpha")).unwrap().join("table.yml"),
        text.replace("dbo.old", "beta.dbo.old"),
    )
    .unwrap();
    let ProjectLoadError::Declarations { database, .. } = load_database_dirs(&paths).unwrap_err()
    else {
        panic!("cross-database rename must refuse")
    };
    assert_eq!(database, db("alpha"));
}
