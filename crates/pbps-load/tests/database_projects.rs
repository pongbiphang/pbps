use pbps_load::{DatabaseDirs, ProjectLoadError, load_database_dirs, load_databases};
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

#[cfg(unix)]
#[test]
fn nested_cross_database_directory_links_are_refused() {
    use std::os::unix::fs::symlink;
    let dir = Dir::new();
    let paths = dir.paths();
    let alpha = paths.get(&db("alpha")).unwrap();
    let beta = paths.get(&db("beta")).unwrap();
    std::fs::write(beta.join("table.yml"), declaration()).unwrap();
    symlink("../beta", alpha.join("imported")).unwrap();
    let error = load_database_dirs(&paths).unwrap_err();
    assert!(matches!(error, ProjectLoadError::Layout(_)));
    assert!(error.to_string().contains("alpha"));
}

#[cfg(unix)]
#[test]
fn nested_cross_database_declaration_links_are_refused() {
    use std::os::unix::fs::symlink;
    let dir = Dir::new();
    let paths = dir.paths();
    let alpha = paths.get(&db("alpha")).unwrap();
    let beta = paths.get(&db("beta")).unwrap();
    std::fs::write(beta.join("table.yml"), declaration()).unwrap();
    symlink("../beta/table.yml", alpha.join("imported.yaml")).unwrap();
    let error = load_database_dirs(&paths).unwrap_err();
    assert!(matches!(error, ProjectLoadError::Layout(_)));
    assert!(error.to_string().contains("alpha"));
}

#[test]
fn regular_nested_declarations_keep_database_ownership_and_source_paths() {
    let dir = Dir::new();
    let paths = dir.paths();
    for (database, path) in paths.iter() {
        let nested = path.join("tables/nested");
        std::fs::create_dir_all(&nested).unwrap();
        let table_name = ["dbo.only_", database.raw()].concat();
        std::fs::write(
            nested.join("table.yaml"),
            declaration().replace("dbo.same", &table_name),
        )
        .unwrap();
    }
    let loaded = load_database_dirs(&paths).unwrap();
    for (database, path) in paths.iter() {
        let own = loaded.loaded().get(database).unwrap();
        assert_eq!(own.schema.tables.len(), 1);
        let table_name = ["dbo.only_", database.raw()].concat();
        assert!(own.schema.tables.contains_key(&table_name.parse().unwrap()));
        assert_eq!(own.sources.len(), 1);
        assert_eq!(
            own.sources.values().next().unwrap(),
            &path.join("tables/nested/table.yaml")
        );
    }
}

#[cfg(unix)]
#[test]
fn nested_directory_aliases_refuse_even_when_the_target_is_empty() {
    use std::os::unix::fs::symlink;
    let dir = Dir::new();
    let paths = dir.paths();
    let alpha = paths.get(&db("alpha")).unwrap();
    std::fs::create_dir(alpha.join("empty")).unwrap();
    symlink("empty", alpha.join("alias")).unwrap();
    assert!(matches!(
        load_database_dirs(&paths),
        Err(ProjectLoadError::Layout(_))
    ));
}

#[cfg(unix)]
#[test]
fn nested_directory_cycles_refuse_before_declaration_traversal() {
    use std::os::unix::fs::symlink;
    let dir = Dir::new();
    let paths = dir.paths();
    symlink(".", paths.get(&db("alpha")).unwrap().join("cycle")).unwrap();
    assert!(matches!(
        load_database_dirs(&paths),
        Err(ProjectLoadError::Layout(_))
    ));
}

#[cfg(unix)]
#[test]
fn declaration_file_links_within_the_database_keep_ownership() {
    use std::os::unix::fs::symlink;
    let dir = Dir::new();
    let paths = dir.paths();
    let alpha = paths.get(&db("alpha")).unwrap();
    let beta = paths.get(&db("beta")).unwrap();
    std::fs::write(alpha.join("definition.txt"), declaration()).unwrap();
    std::fs::write(beta.join("notes.txt"), "not a declaration").unwrap();
    symlink("definition.txt", alpha.join("table.yml")).unwrap();
    symlink("../beta/notes.txt", alpha.join("notes.md")).unwrap();
    let loaded = load_database_dirs(&paths).unwrap();
    assert_eq!(
        loaded
            .loaded()
            .get(&db("alpha"))
            .unwrap()
            .schema
            .tables
            .len(),
        1
    );
    assert!(
        loaded
            .loaded()
            .get(&db("beta"))
            .unwrap()
            .schema
            .tables
            .is_empty()
    );
    assert_eq!(
        loaded
            .loaded()
            .get(&db("alpha"))
            .unwrap()
            .sources
            .values()
            .next()
            .unwrap(),
        &alpha.join("table.yml")
    );
}

#[cfg(unix)]
#[test]
fn flat_directory_links_keep_the_existing_loader_behavior() {
    use std::os::unix::fs::symlink;
    let flat = Dir::new();
    let external = Dir::new();
    std::fs::write(external.0.join("table.yml"), declaration()).unwrap();
    symlink(&external.0, flat.0.join("imported")).unwrap();
    assert_eq!(
        pbps_load::load_schema_dir(&flat.0)
            .unwrap()
            .schema
            .tables
            .len(),
        1
    );
}

#[test]
fn declared_extensions_and_held_identities_stay_in_their_database() {
    let dir = Dir::new();
    let paths = dir.paths();
    for (name, uid, schema, version) in [
        ("alpha", "e_aaaaaa", "ext", "1.4"),
        ("beta", "e_bbbbbb", "other", "1.6"),
    ] {
        let path = paths.get(&db(name)).unwrap();
        std::fs::write(
            path.join("package.yml"),
            format!("extension: package\nschema: {schema}\nversion: '{version}'\n"),
        )
        .unwrap();
        write_ids(
            &paths,
            name,
            &IdsFile {
                extensions: [(uid.parse().unwrap(), "package".into())].into(),
                ..Default::default()
            },
        );
    }
    let mut project = load_database_dirs(&paths).unwrap();
    assert_eq!(
        project
            .loaded()
            .get(&db("alpha"))
            .unwrap()
            .schema
            .extensions["package"]
            .schema,
        "ext"
    );
    assert_eq!(
        project.loaded().get(&db("beta")).unwrap().schema.extensions["package"].version,
        "1.6"
    );
    for (name, uid) in [("alpha", "e_aaaaaa"), ("beta", "e_bbbbbb")] {
        assert_eq!(
            project
                .ids()
                .get(&db(name))
                .unwrap()
                .extension_uid(&"package".into()),
            Some(&uid.parse().unwrap())
        );
        assert!(project.reservations().contains(&uid.parse().unwrap()));
    }
    write_ids(
        &paths,
        "beta",
        &project.ids().get(&db("alpha")).unwrap().clone(),
    );
    let error = load_database_dirs(&paths).unwrap_err().to_string();
    for name in ["alpha", "beta", "e_aaaaaa"] {
        assert!(error.contains(name), "{error}");
    }
}

fn tree_file(ids_dir: &std::path::Path, rel: &str, uid: &str, name: &str) {
    use pbps_model::ids_tree::{self, Placement};
    let ids = IdsFile {
        tables: [(uid.parse().unwrap(), name.parse().unwrap())].into(),
        ..Default::default()
    };
    let placement = Placement {
        paths: [(uid.parse().unwrap(), rel.to_owned())].into(),
        ..Default::default()
    };
    for (path, text) in ids_tree::split(&ids, &placement).unwrap() {
        let path = ids_dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

fn schema_dirs(databases: &DatabaseMap<DatabaseDirs>) -> DatabaseMap<PathBuf> {
    databases.map(|_, dirs| dirs.schema_dir.clone())
}

/// The identity trees beside, not inside, the declarations root: the loader
/// refuses any other entry there.
struct Trees(PathBuf);
impl Trees {
    fn of(dir: &Dir) -> Self {
        Self(dir.0.with_extension("ids"))
    }
}
impl Drop for Trees {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dirs(dir: &Dir, trees: &Trees, with_tree: bool) -> DatabaseMap<DatabaseDirs> {
    let tree = trees.0.clone();
    dir.paths().map(|name, schema_dir| DatabaseDirs {
        schema_dir: schema_dir.clone(),
        ids_dir: with_tree.then(|| tree.join(name.raw())),
    })
}

#[test]
fn a_databases_identities_are_read_from_its_own_tree_when_it_has_one() {
    let dir = Dir::new();
    let trees = Trees::of(&dir);
    let databases = dirs(&dir, &trees, true);
    for (name, dirs) in databases.iter() {
        std::fs::write(dirs.schema_dir.join("t.yml"), declaration()).unwrap();
        let uid = if name.raw() == "alpha" {
            "t_aaaaaa"
        } else {
            "t_bbbbbb"
        };
        tree_file(dirs.ids_dir.as_ref().unwrap(), "t.json", uid, "dbo.same");
    }
    let loaded = load_databases(&databases).unwrap();
    for (name, uid) in [("alpha", "t_aaaaaa"), ("beta", "t_bbbbbb")] {
        let ids = loaded.ids().get(&db(name)).unwrap();
        assert_eq!(
            ids.tables
                .keys()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [uid]
        );
    }
}

#[test]
fn a_database_without_a_tree_keeps_reading_schema_ids_json() {
    let dir = Dir::new();
    let trees = Trees::of(&dir);
    let databases = dirs(&dir, &trees, true);
    for (_, dirs) in databases.iter() {
        std::fs::write(dirs.schema_dir.join("t.yml"), declaration()).unwrap();
    }
    write_ids(&schema_dirs(&databases), "alpha", &identities("t_aaaaaa"));
    let loaded = load_databases(&databases).unwrap();
    assert_eq!(loaded.ids().get(&db("alpha")).unwrap().tables.len(), 1);
    // A tree directory that is absent, or present but empty, is no tree.
    assert!(loaded.ids().get(&db("beta")).unwrap().tables.is_empty());
    std::fs::create_dir_all(trees.0.join("beta")).unwrap();
    assert!(load_databases(&databases).is_ok());
}

#[test]
fn a_tree_beside_the_legacy_file_is_refused_as_ambiguous() {
    let dir = Dir::new();
    let trees = Trees::of(&dir);
    let databases = dirs(&dir, &trees, true);
    for (_, dirs) in databases.iter() {
        std::fs::write(dirs.schema_dir.join("t.yml"), declaration()).unwrap();
    }
    write_ids(&schema_dirs(&databases), "alpha", &identities("t_aaaaaa"));
    tree_file(&trees.0.join("alpha"), "t.json", "t_aaaaaa", "dbo.same");
    let error = load_databases(&databases).unwrap_err().to_string();
    assert!(error.contains("both") && error.contains("alpha"), "{error}");
}

#[test]
fn a_damaged_or_unexpected_tree_entry_is_refused_not_skipped() {
    let dir = Dir::new();
    let trees = Trees::of(&dir);
    let databases = dirs(&dir, &trees, true);
    for (_, dirs) in databases.iter() {
        std::fs::write(dirs.schema_dir.join("t.yml"), declaration()).unwrap();
    }
    let alpha = trees.0.join("alpha");
    tree_file(&alpha, "t.json", "t_aaaaaa", "dbo.same");
    std::fs::write(alpha.join("notes.txt"), "x").unwrap();
    let error = load_databases(&databases).unwrap_err().to_string();
    assert!(error.contains("notes.txt"), "{error}");
    std::fs::remove_file(alpha.join("notes.txt")).unwrap();
    std::fs::write(alpha.join("t.json"), "{").unwrap();
    assert!(matches!(
        load_databases(&databases),
        Err(ProjectLoadError::Identities { .. })
    ));
}
