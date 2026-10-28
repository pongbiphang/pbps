use pbps_model::{DropRequirement, IdsFile, Schema, Tombstone, state_checksum};

#[test]
fn unguarded_tombstones_keep_their_bytes_and_guarded_ones_change_the_checksum() {
    let old =
        r#"{"was":"app.t.old","dropped_at":"2026-10-28","reason":"copied","operator":"reviewer"}"#;
    let tombstone: Tombstone = serde_json::from_str(old).unwrap();
    assert_eq!(serde_json::to_string(&tombstone).unwrap(), old);
    let mut ids = IdsFile::default();
    let uid = "c_000001".parse().unwrap();
    ids.tombstones.insert(uid, tombstone);
    let before = state_checksum(&Schema::default(), &ids);
    let tombstone = ids.tombstones.values_mut().next().unwrap();
    tombstone.require = Some(DropRequirement {
        table_uid: "t_000001".parse().unwrap(),
        predicate: "old IS NULL".into(),
        required_at: "2026-10-28".into(),
        operator: "reviewer".into(),
    });
    ids.validate().unwrap();
    assert_ne!(state_checksum(&Schema::default(), &ids), before);
    assert_eq!(ids.version, 1);
    assert_eq!(pbps_model::state::CURRENT_VERSION, 35);
    ids.tombstones
        .values_mut()
        .next()
        .unwrap()
        .require
        .as_mut()
        .unwrap()
        .predicate = " \t".into();
    assert!(ids.validate().unwrap_err().to_string().contains("blank"));
}
