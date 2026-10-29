use pbps_model::{
    DatabaseMap, DatabaseName, IdsFile, InDatabase, Schema, Uid, UidKind, UidReservations,
};
fn db(name: &str) -> DatabaseName {
    name.parse().unwrap()
}
fn ids(uid: &str) -> IdsFile {
    IdsFile {
        tables: [(uid.parse().unwrap(), "dbo.same".parse().unwrap())].into(),
        ..Default::default()
    }
}

#[test]
fn names_are_directory_components_without_defining_engine_equality() {
    for name in [
        "sales",
        "Sales",
        "db.2026",
        "顧客",
        "db with spaces",
        ".git-data",
        ".gitignore",
        "git",
    ] {
        let value = db(name);
        assert_eq!(value.raw(), name);
        assert_eq!(
            serde_json::from_str::<DatabaseName>(&serde_json::to_string(&value).unwrap()).unwrap(),
            value
        );
    }
    for name in [
        "",
        ".",
        "..",
        ".git",
        ".GIT",
        ".GiT",
        ".git.",
        ".git ",
        "git~1",
        "a/b",
        "a\\b",
        "C:one",
        "line\nfeed",
        "nul\0byte",
    ] {
        assert!(name.parse::<DatabaseName>().is_err(), "{name:?}");
    }
    assert_ne!(db("sales"), db("Sales"));
    assert!(
        "line\nfeed"
            .parse::<DatabaseName>()
            .unwrap_err()
            .contains(r"line\nfeed")
    );
}

#[test]
fn project_maps_require_at_least_one_unique_coordinate() {
    assert!(DatabaseMap::<Schema>::new([]).is_err());
    assert!(
        DatabaseMap::new([(db("a"), Schema::default()), (db("a"), Schema::default())]).is_err()
    );
    let maps = DatabaseMap::new([(db("b"), 2), (db("a"), 1)]).unwrap();
    assert_eq!(
        maps.keys().map(DatabaseName::raw).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(maps.map(|_, value| value + 10).get(&db("a")), Some(&11));
    assert!(
        maps.try_map(|name, _| if name == &db("b") {
            Err("missing")
        } else {
            Ok(1)
        })
        .is_err()
    );
    assert_eq!(maps.get(&db("b")), Some(&2));
}

#[test]
fn equal_local_names_are_different_named_resources_in_different_databases() {
    let a = InDatabase::new(db("a"), "dbo.v");
    let b = InDatabase::new(db("b"), "dbo.v");
    assert_ne!(a, b);
    assert_eq!(a.database(), &db("a"));
    assert_eq!(*a.value(), "dbo.v");
    assert_eq!(b.into_parts(), (db("b"), "dbo.v"));
}

#[test]
fn duplicate_identity_files_name_both_databases_instead_of_flattening_names() {
    let files = DatabaseMap::new([(db("a"), ids("t_aaaaaa")), (db("b"), ids("t_aaaaaa"))]).unwrap();
    let error = UidReservations::from_files(&files).unwrap_err();
    assert!(error.contains("`a`") && error.contains("`b`") && error.contains("t_aaaaaa"));
    let files = DatabaseMap::new([(db("a"), ids("t_aaaaaa")), (db("b"), ids("t_bbbbbb"))]).unwrap();
    let mut pool = UidReservations::from_files(&files).unwrap();
    assert_eq!(
        files
            .get(&db("a"))
            .unwrap()
            .table_uid(&"dbo.same".parse().unwrap())
            .unwrap()
            .to_string(),
        "t_aaaaaa"
    );
    let made = pool.allocate(UidKind::Table);
    assert_ne!(made, "t_aaaaaa".parse::<Uid>().unwrap());
    assert_ne!(made, "t_bbbbbb".parse::<Uid>().unwrap());
    assert!(pool.contains(&made));
}

#[test]
fn a_tombstone_in_one_database_reserves_the_identity_in_every_other_database() {
    let mut retired = IdsFile::default();
    retired.tombstones.insert(
        "t_aaaaaa".parse().unwrap(),
        pbps_model::ids::Tombstone {
            was: "dbo.retired".into(),
            dropped_at: "2026-10-01".into(),
            reason: "retired".into(),
            operator: "fixture".into(),
            require: None,
        },
    );
    let files = DatabaseMap::new([(db("a"), retired), (db("b"), ids("t_aaaaaa"))]).unwrap();
    assert!(
        UidReservations::from_files(&files)
            .unwrap_err()
            .contains("t_aaaaaa")
    );
    let bad = DatabaseMap::new([(db("a"), ids("c_aaaaaa"))]).unwrap();
    assert!(UidReservations::from_files(&bad).is_err());
}

#[test]
fn single_database_schema_and_identity_wire_bytes_are_unchanged() {
    assert_eq!(
        serde_json::to_string(&Schema::default()).unwrap(),
        r#"{"tables":{}}"#
    );
    assert_eq!(
        serde_json::to_string(&IdsFile::default()).unwrap(),
        r#"{"version":1,"tables":{},"columns":{}}"#
    );
}
