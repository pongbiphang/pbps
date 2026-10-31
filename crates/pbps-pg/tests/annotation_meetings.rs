//! Both independently supplied inputs survive the real canonical meeting route.
#[path = "support/annotation_meetings.rs"]
mod meetings;

#[test]
fn an_annotated_partition_column_consumer_keeps_its_canonical_edge() {
    let scene = meetings::scene();
    for version in [160_015, 180_006] {
        meetings::canonical(&scene, version, Some(&scene.stored)).unwrap();
    }
}

#[test]
fn annotation_intent_does_not_supply_an_unread_membership_catalog() {
    let scene = meetings::scene();
    let empty = Default::default();
    for version in [160_015, 180_006] {
        // An observed inventory with no entry for this column cannot prove its
        // function binding either; an absent entry is not a read empty default.
        for catalog in [None, Some(&empty)] {
            let error = meetings::canonical(&scene, version, catalog).unwrap_err();
            assert!(error.contains("catalog fact was not read"), "{error}");
        }
    }
}
