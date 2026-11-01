use pbps_diff::managed::observed_ids_with_reservations;
use pbps_model::{Column, DatabaseMap, IdsFile, Schema, Table, UidReservations};
fn live() -> Schema {
    let mut table = Table::default();
    table
        .columns
        .insert("id".into(), Column::new("int".parse().unwrap()));
    Schema {
        tables: [("dbo.same".parse().unwrap(), table)].into(),
        extensions: [(
            "package".into(),
            pbps_model::Extension {
                schema: "ext".into(),
                version: "1.0".into(),
            },
        )]
        .into(),
        ..Default::default()
    }
}
fn observed() -> DatabaseMap<IdsFile> {
    let files = DatabaseMap::new([
        ("alpha".parse().unwrap(), IdsFile::default()),
        ("beta".parse().unwrap(), IdsFile::default()),
    ])
    .unwrap();
    let mut pool = UidReservations::from_files(&files).unwrap();
    let result = files.map(|_, ids| observed_ids_with_reservations(&live(), ids, &mut pool));
    UidReservations::from_files(&result).unwrap();
    result
}

#[test]
fn shared_observed_reservations_separate_equal_names_and_are_deterministic() {
    let result = observed();
    let a = result.get(&"alpha".parse().unwrap()).unwrap();
    let b = result.get(&"beta".parse().unwrap()).unwrap();
    assert_ne!(
        a.table_uid(&"dbo.same".parse().unwrap()),
        b.table_uid(&"dbo.same".parse().unwrap())
    );
    assert_ne!(
        a.column_uid(&"dbo.same.id".parse().unwrap()),
        b.column_uid(&"dbo.same.id".parse().unwrap())
    );
    assert_ne!(
        a.extension_uid(&"package".into()),
        b.extension_uid(&"package".into())
    );
    assert_eq!(result, observed());
}

#[test]
fn a_shared_observation_never_reuses_a_foreign_recorded_identity() {
    // Reserve the unsalted identities in beta so alpha must redraw even
    // though its own recorded map is empty.
    let beta = pbps_diff::observed_ids(&live(), &IdsFile::default());
    let maps = DatabaseMap::new([
        ("alpha".parse().unwrap(), IdsFile::default()),
        ("beta".parse().unwrap(), beta),
    ])
    .unwrap();
    let mut pool = UidReservations::from_files(&maps).unwrap();
    let result = maps.map(|_, ids| observed_ids_with_reservations(&live(), ids, &mut pool));
    assert!(UidReservations::from_files(&result).is_ok());
    assert_ne!(
        result
            .get(&"alpha".parse().unwrap())
            .unwrap()
            .extension_uid(&"package".into()),
        result
            .get(&"beta".parse().unwrap())
            .unwrap()
            .extension_uid(&"package".into())
    );
}
