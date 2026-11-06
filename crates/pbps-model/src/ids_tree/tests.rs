use super::*;
use crate::RebuildIdentities;

fn uid(s: &str) -> Uid {
    s.parse().unwrap()
}

fn tomb(was: &str) -> Tombstone {
    Tombstone {
        was: was.into(),
        dropped_at: "2026-08-30".into(),
        reason: "REG-2026-042 PII erasure".into(),
        operator: "leon".into(),
        require: None,
    }
}

/// Every kind of record the layout holds: a table with a retired column in
/// the middle, a partitioned table with a live and a retired child, a move,
/// a conversion, a dropped table, a rebuild across files, and one live and
/// one dropped object of each global kind.
pub(crate) fn sample() -> (IdsFile, Placement) {
    let mut f = IdsFile::default();
    let t = |s: &str| -> TableName { s.parse().unwrap() };
    let c = |s: &str| -> ColumnRef { s.parse().unwrap() };
    f.tables.insert(uid("t_aaaaaa"), t("dbo.customer"));
    f.columns
        .insert(uid("c_aaaaa1"), c("dbo.customer.customer_id"));
    f.columns
        .insert(uid("c_aaaaa2"), c("dbo.customer.full_name"));
    f.tombstones
        .insert(uid("c_aaaaa3"), tomb("dbo.customer.national_id"));
    f.columns.insert(uid("c_aaaaa4"), c("dbo.customer.email"));
    f.conversions.insert(
        uid("c_aaaaa4"),
        Conversion {
            from: "text".into(),
            to: "citext".into(),
            using: "lower(email)".into(),
            allowed_at: "2026-09-01".into(),
            operator: "leon".into(),
        },
    );
    f.tables.insert(uid("t_bbbbbb"), t("app.events"));
    f.columns.insert(uid("c_bbbbb1"), c("app.events.at"));
    f.tables.insert(uid("t_bbbbb1"), t("app.events_2026_01"));
    f.tombstones
        .insert(uid("t_bbbbb2"), tomb("app.events_2025_12"));
    f.moves.insert(
        uid("t_bbbbb1"),
        Move {
            max_rows: 5000,
            allowed_at: "2026-09-02".into(),
            reason: "backfill".into(),
            operator: "leon".into(),
        },
    );
    f.tombstones.insert(uid("t_cccccc"), tomb("dbo.legacy"));
    f.tombstones.insert(uid("c_ccccc1"), tomb("dbo.legacy.id"));
    // A rebuild: the source parent is retired, the replacement is live.
    f.tombstones.insert(uid("t_dddddd"), tomb("dbo.orders_old"));
    f.tombstones
        .insert(uid("c_ddddd1"), tomb("dbo.orders_old.id"));
    f.tables.insert(uid("t_eeeeee"), t("dbo.orders"));
    f.columns.insert(uid("c_eeeee1"), c("dbo.orders.id"));
    let ids = |tables: &[(&str, &str)], columns: &[(&str, &str)]| RebuildIdentities {
        tables: tables.iter().map(|(u, n)| (uid(u), t(n))).collect(),
        columns: columns.iter().map(|(u, n)| (uid(u), c(n))).collect(),
    };
    f.rebuilds.insert(
        uid("t_dddddd"),
        Rebuild {
            source: ids(
                &[("t_dddddd", "dbo.orders_old")],
                &[("c_ddddd1", "dbo.orders_old.id")],
            ),
            replacement: ids(
                &[("t_eeeeee", "dbo.orders")],
                &[("c_eeeee1", "dbo.orders.id")],
            ),
            replacement_root: uid("t_eeeeee"),
            continues: [
                (uid("t_eeeeee"), uid("t_dddddd")),
                (uid("c_eeeee1"), uid("c_ddddd1")),
            ]
            .into(),
            target_checksum: "0".repeat(64),
            max_rows: 100,
            allowed_at: "2026-09-03".into(),
            reason: crate::Reason::new("widen").unwrap(),
            operator: "leon".into(),
            types: BTreeMap::new(),
            conversions: BTreeMap::new(),
        },
    );
    f.roles.insert(uid("r_aaaaaa"), RoleName::new("app_reader"));
    f.tombstones.insert(uid("r_bbbbbb"), tomb("old_role"));
    f.types
        .insert(uid("y_aaaaaa"), "dbo.status".parse().unwrap());
    f.extensions
        .insert(uid("e_aaaaaa"), ObjectLocalName::new("pgcrypto"));
    f.validate().unwrap();

    let p = Placement {
        paths: [
            ("t_aaaaaa", "dbo/customer.json"),
            ("t_bbbbbb", "app/events.json"),
            ("t_cccccc", "dbo/legacy.json"),
            ("t_dddddd", "dbo/orders_old.json"),
            ("t_eeeeee", "dbo/orders.json"),
            ("r_aaaaaa", "roles/app_reader.json"),
            ("r_bbbbbb", "roles/old_role.json"),
            ("y_aaaaaa", "types/status.json"),
            ("e_aaaaaa", "extensions/pgcrypto.json"),
        ]
        .into_iter()
        .map(|(u, p)| (uid(u), p.to_owned()))
        .collect(),
        children: [("t_bbbbb1", "t_bbbbbb"), ("t_bbbbb2", "t_bbbbbb")]
            .into_iter()
            .map(|(c, p)| (uid(c), uid(p)))
            .collect(),
        order: [(
            uid("t_aaaaaa"),
            ["c_aaaaa1", "c_aaaaa2", "c_aaaaa3", "c_aaaaa4"]
                .map(uid)
                .to_vec(),
        )]
        .into(),
        owners: BTreeMap::new(),
    };
    (f, p)
}

pub(crate) fn tree() -> BTreeMap<String, String> {
    let (ids, p) = sample();
    split(&ids, &p).unwrap()
}

/// Replaces `from` once and proves it did (a scripted edit that matched
/// nothing reads like one that worked).
fn edit(text: &str, from: &str, to: &str) -> String {
    assert_eq!(text.matches(from).count(), 1, "{from:?} in {text}");
    text.replacen(from, to, 1)
}

fn with(path: &str, text: String) -> BTreeMap<String, String> {
    let mut t = tree();
    t.insert(path.into(), text);
    t
}

fn bad_name(err: TreeError) -> String {
    let TreeError::BadName { name, .. } = err else {
        panic!("not a bad name: {err:?}");
    };
    name
}

fn refuses(path: &str, text: String) -> TreeError {
    assemble(&with(path, text)).unwrap_err()
}

// ---- canonical bytes ----------------------------------------------------

#[test]
fn generated_line_is_exactly_400_bytes_without_characters_json_would_escape() {
    assert_eq!(GENERATED.len(), 400);
    assert!(
        GENERATED
            .bytes()
            .all(|b| b.is_ascii() && b >= b' ' && b != b'"' && b != b'\\')
    );
}

#[test]
fn a_table_file_has_the_canonical_bytes_with_the_tombstone_in_place() {
    let expected = format!(
        r#"{{
  "_generated": "{GENERATED}",
  "format": 1,
  "uid": "t_aaaaaa",
  "table": "dbo.customer",
  "columns": {{

    "c_aaaaa1": "customer_id",

    "c_aaaaa2": "full_name",

    "c_aaaaa3": {{"was": "dbo.customer.national_id", "dropped_at": "2026-08-30", "reason": "REG-2026-042 PII erasure", "operator": "leon"}},

    "c_aaaaa4": "email",

    "_end": true

  }},
  "conversions": {{

    "c_aaaaa4": {{"from": "text", "to": "citext", "using": "lower(email)", "allowed_at": "2026-09-01", "operator": "leon"}},

    "_end": true

  }}
}}
"#
    );
    assert_eq!(tree()["dbo/customer.json"], expected);
}

#[test]
fn a_table_without_columns_still_writes_its_section_and_sentinel() {
    let mut ids = IdsFile::default();
    ids.tables.insert(uid("t_aaaaaa"), "dbo.t".parse().unwrap());
    let p = Placement {
        paths: [(uid("t_aaaaaa"), "dbo/t.json".to_owned())].into(),
        ..Placement::default()
    };
    let text = &split(&ids, &p).unwrap()["dbo/t.json"];
    assert!(
        text.ends_with("  \"columns\": {\n\n    \"_end\": true\n\n  }\n}\n"),
        "{text}"
    );
}

#[test]
fn role_type_and_extension_files_have_a_header_and_one_name_line() {
    let t = tree();
    let head = format!("{{\n  \"_generated\": \"{GENERATED}\",\n  \"format\": 1,\n");
    assert_eq!(
        t["roles/app_reader.json"],
        format!("{head}  \"uid\": \"r_aaaaaa\",\n  \"role\": \"app_reader\"\n}}\n")
    );
    assert_eq!(
        t["types/status.json"],
        format!("{head}  \"uid\": \"y_aaaaaa\",\n  \"type\": \"dbo.status\"\n}}\n")
    );
    assert_eq!(
        t["extensions/pgcrypto.json"],
        format!("{head}  \"uid\": \"e_aaaaaa\",\n  \"extension\": \"pgcrypto\"\n}}\n")
    );
}

#[test]
fn a_dropped_object_keeps_its_file_with_a_dropped_line() {
    let t = tree();
    let dropped = r#"{"was": "old_role", "dropped_at": "2026-08-30", "reason": "REG-2026-042 PII erasure", "operator": "leon"}"#;
    assert!(
        t["roles/old_role.json"].ends_with(&format!(
            "  \"role\": \"old_role\",\n  \"dropped\": {dropped}\n}}\n"
        )),
        "{}",
        t["roles/old_role.json"]
    );
    let legacy = &t["dbo/legacy.json"];
    assert!(
        legacy.contains("  \"table\": \"dbo.legacy\",\n  \"dropped\": {\"was\": \"dbo.legacy\"")
    );
    assert!(legacy.contains("    \"c_ccccc1\": {\"was\": \"dbo.legacy.id\""));
    assert!(!legacy.contains("\"c_ccccc1\": \"id\""));
}

#[test]
fn partitions_moves_and_rebuilds_land_in_the_file_that_owns_them() {
    let t = tree();
    let events = &t["app/events.json"];
    let order: Vec<_> = ["\"columns\"", "\"partitions\"", "\"moves\""]
        .iter()
        .map(|k| events.find(k).unwrap())
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{events}");
    assert!(events.contains("    \"t_bbbbb1\": \"app.events_2026_01\",\n"));
    assert!(events.contains("    \"t_bbbbb2\": {\"was\": \"app.events_2025_12\""));
    assert!(
        events.contains("    \"t_bbbbb1\": {\"max_rows\": 5000, \"allowed_at\": \"2026-09-02\"")
    );
    // The rebuild is keyed by the retired source and stored with the live
    // replacement; the source's own file holds no copy.
    assert!(t["dbo/orders.json"].contains("\"rebuilds\": {"));
    assert!(t["dbo/orders.json"].contains("    \"t_dddddd\": {\"source\": {"));
    assert!(!t["dbo/orders_old.json"].contains("rebuilds"));
}

#[test]
fn optional_sections_are_absent_when_empty() {
    let t = tree();
    for key in ["partitions", "moves", "conversions", "rebuilds"] {
        assert!(!t["dbo/legacy.json"].contains(key), "{key}");
    }
}

#[test]
fn text_is_escaped_and_kept_as_utf8() {
    let (mut ids, p) = sample();
    ids.tombstones.get_mut(&uid("c_aaaaa3")).unwrap().reason = "line\n\"quoted\" \u{7aef}".into();
    let t = split(&ids, &p).unwrap();
    assert!(t["dbo/customer.json"].contains(r#""reason": "line\n\"quoted\" 端""#));
    assert_eq!(assemble(&t).unwrap(), ids);
}

// ---- round trip, placement ---------------------------------------------

#[test]
fn assemble_undoes_split() {
    let (ids, p) = sample();
    assert_eq!(assemble(&split(&ids, &p).unwrap()).unwrap(), ids);
}

#[test]
fn split_is_byte_stable() {
    let (ids, p) = sample();
    let first = split(&ids, &p).unwrap();
    assert_eq!(split(&assemble(&first).unwrap(), &p).unwrap(), first);
}

#[test]
fn an_empty_mapping_is_an_empty_tree() {
    let t = split(&IdsFile::default(), &Placement::default()).unwrap();
    assert!(t.is_empty());
    assert_eq!(assemble(&t).unwrap(), IdsFile::default());
}

#[test]
fn renaming_a_table_changes_exactly_one_line() {
    let (mut ids, mut p) = sample();
    // The tombstone's `was` still names the old table, so the owner is stated.
    p.owners.insert(uid("c_aaaaa3"), uid("t_aaaaaa"));
    let before = split(&ids, &p).unwrap();
    ids.rename_table(
        &"dbo.customer".parse().unwrap(),
        &"dbo.client".parse().unwrap(),
    );
    let after = split(&ids, &p).unwrap();
    let changed: Vec<_> = before.keys().filter(|k| before[*k] != after[*k]).collect();
    assert_eq!(changed, ["dbo/customer.json"]);
    let (a, b) = (&before["dbo/customer.json"], &after["dbo/customer.json"]);
    let diff: Vec<_> = a.lines().zip(b.lines()).filter(|(x, y)| x != y).collect();
    assert_eq!(a.lines().count(), b.lines().count());
    assert_eq!(
        diff,
        [(
            "  \"table\": \"dbo.customer\",",
            "  \"table\": \"dbo.client\","
        )]
    );
    assert_eq!(assemble(&after).unwrap(), ids);
}

#[test]
fn the_placement_order_decides_the_record_order() {
    let (ids, mut p) = sample();
    p.order
        .insert(uid("t_aaaaaa"), ["c_aaaaa4", "c_aaaaa1"].map(uid).to_vec());
    let text = &split(&ids, &p).unwrap()["dbo/customer.json"];
    let at = |u: &str| text.find(&format!("\"{u}\": ")).unwrap();
    // Named records first in the order given, the rest in uid order.
    assert!(at("c_aaaaa4") < at("c_aaaaa1") && at("c_aaaaa1") < at("c_aaaaa2"));
    assert!(at("c_aaaaa2") < at("c_aaaaa3"));
}

#[test]
fn a_tombstone_whose_table_was_renamed_is_placed_by_its_stated_owner() {
    let (mut ids, mut p) = sample();
    ids.rename_table(
        &"dbo.customer".parse().unwrap(),
        &"dbo.client".parse().unwrap(),
    );
    assert_eq!(
        split(&ids, &p).unwrap_err(),
        TreeError::UnownedTombstone {
            uid: uid("c_aaaaa3")
        }
    );
    p.owners.insert(uid("c_aaaaa3"), uid("t_aaaaaa"));
    assert_eq!(assemble(&split(&ids, &p).unwrap()).unwrap(), ids);
}

// ---- record order ------------------------------------------------------

fn uids(s: &[&str]) -> Vec<Uid> {
    s.iter().map(|u| uid(u)).collect()
}

#[test]
fn a_new_column_follows_its_yaml_predecessor_and_a_tombstone_keeps_its_place() {
    let existing = uids(&["c_aaaaa1", "c_aaaaa2", "c_aaaaa3", "c_aaaaa4"]);
    let retired: BTreeSet<_> = [uid("c_aaaaa3")].into();
    let declared = uids(&["c_aaaaa1", "c_aaaaa5", "c_aaaaa2", "c_aaaaa4"]);
    assert_eq!(
        next_order(&existing, &declared, &retired),
        uids(&["c_aaaaa1", "c_aaaaa5", "c_aaaaa2", "c_aaaaa3", "c_aaaaa4"])
    );
}

#[test]
fn a_first_column_goes_first_and_a_removed_live_record_goes_away() {
    let existing = uids(&["c_aaaaa1", "c_aaaaa2"]);
    let declared = uids(&["c_aaaaa0", "c_aaaaa2"]);
    assert_eq!(
        next_order(&existing, &declared, &BTreeSet::new()),
        uids(&["c_aaaaa0", "c_aaaaa2"])
    );
}

#[test]
fn survivors_take_the_yaml_order_in_the_slots_they_hold() {
    let existing = uids(&["c_aaaaa1", "c_aaaaa3", "c_aaaaa2"]);
    let retired: BTreeSet<_> = [uid("c_aaaaa3")].into();
    let declared = uids(&["c_aaaaa2", "c_aaaaa1"]);
    assert_eq!(
        next_order(&existing, &declared, &retired),
        uids(&["c_aaaaa2", "c_aaaaa3", "c_aaaaa1"])
    );
}

// ---- loader refusals ---------------------------------------------------

#[test]
fn spacing_is_not_enforced() {
    let compact = serde_json::to_string(
        &serde_json::from_str::<serde_json::Value>(&tree()["dbo/customer.json"]).unwrap(),
    )
    .unwrap();
    let (ids, _) = sample();
    assert_eq!(
        assemble(&with("dbo/customer.json", compact))
            .unwrap()
            .columns,
        ids.columns
    );
}

#[test]
fn conflict_markers_are_named_before_the_json_error() {
    let text = format!(
        "<<<<<<< ours\n{}=======\n>>>>>>> theirs\n",
        tree()["roles/app_reader.json"]
    );
    assert_eq!(
        refuses("roles/app_reader.json", text),
        TreeError::ConflictMarkers {
            path: "roles/app_reader.json".into()
        }
    );
}

#[test]
fn invalid_json_and_unknown_fields_are_refused() {
    let t = tree();
    assert!(matches!(
        refuses("roles/app_reader.json", "{".into()),
        TreeError::Malformed { .. }
    ));
    let extra = edit(
        &t["roles/app_reader.json"],
        "\"role\"",
        "\"extra\": 1,\n  \"role\"",
    );
    assert!(matches!(
        refuses("roles/app_reader.json", extra),
        TreeError::Malformed { message, .. } if message.contains("unknown field")
    ));
}

#[test]
fn a_repeated_key_is_refused_at_every_level() {
    let t = tree();
    let top = edit(
        &t["roles/app_reader.json"],
        "\"format\": 1,",
        "\"format\": 1,\n  \"format\": 1,",
    );
    assert_eq!(
        refuses("roles/app_reader.json", top),
        TreeError::DuplicateKey {
            path: "roles/app_reader.json".into(),
            key: "format".into()
        }
    );
    let rec = edit(
        &t["dbo/customer.json"],
        "    \"c_aaaaa2\": \"full_name\",\n",
        "    \"c_aaaaa2\": \"full_name\",\n    \"c_aaaaa2\": \"other\",\n",
    );
    assert_eq!(
        refuses("dbo/customer.json", rec),
        TreeError::DuplicateKey {
            path: "dbo/customer.json".into(),
            key: "c_aaaaa2".into()
        }
    );
}

#[test]
fn another_layout_format_is_refused() {
    let t = tree();
    let text = edit(
        &t["roles/app_reader.json"],
        "\"format\": 1",
        "\"format\": 2",
    );
    assert_eq!(
        refuses("roles/app_reader.json", text),
        TreeError::Format {
            path: "roles/app_reader.json".into(),
            found: 2
        }
    );
}

#[test]
fn a_uid_with_the_wrong_prefix_is_refused_in_the_header_and_in_a_section() {
    let t = tree();
    let header = edit(&t["roles/app_reader.json"], "\"r_aaaaaa\"", "\"t_aaaaaa\"");
    assert!(matches!(
        refuses("roles/app_reader.json", header),
        TreeError::KindMismatch { .. }
    ));
    let record = edit(&t["dbo/customer.json"], "\"c_aaaaa1\"", "\"t_aaaaa1\"");
    assert!(matches!(
        refuses("dbo/customer.json", record),
        TreeError::KindMismatch { uid, .. } if uid == self::uid("t_aaaaa1")
    ));
}

#[test]
fn a_section_must_end_with_its_sentinel_as_the_last_entry() {
    let t = tree();
    let sentinel = "\"email\",\n\n    \"_end\": true\n\n  },\n  \"conversions\"";
    let missing = edit(
        &t["dbo/customer.json"],
        sentinel,
        "\"email\"\n\n  },\n  \"conversions\"",
    );
    assert_eq!(
        refuses("dbo/customer.json", missing),
        TreeError::SentinelMissing {
            path: "dbo/customer.json".into(),
            section: "columns"
        }
    );
    // A merge that left the sentinel above a record.
    let not_last = edit(
        &t["dbo/customer.json"],
        "    \"c_aaaaa4\": \"email\",\n\n    \"_end\": true\n",
        "    \"_end\": true,\n\n    \"c_aaaaa4\": \"email\"\n",
    );
    assert_eq!(
        refuses("dbo/customer.json", not_last),
        TreeError::SentinelNotLast {
            path: "dbo/customer.json".into(),
            section: "columns"
        }
    );
}

#[test]
fn a_table_file_without_columns_is_refused() {
    let t = tree();
    let head = t["dbo/legacy.json"]
        .split("  \"columns\"")
        .next()
        .unwrap()
        .trim_end_matches(",\n");
    assert_eq!(
        refuses("dbo/legacy.json", format!("{head}\n}}\n")),
        TreeError::SentinelMissing {
            path: "dbo/legacy.json".into(),
            section: "columns"
        }
    );
}

#[test]
fn a_file_names_exactly_one_object_and_only_a_table_has_sections() {
    let t = tree();
    let two = edit(
        &t["roles/app_reader.json"],
        "\"role\"",
        "\"extension\": \"x\",\n  \"role\"",
    );
    assert!(matches!(
        refuses("roles/app_reader.json", two),
        TreeError::Malformed { .. }
    ));
    let sections = edit(
        &t["roles/app_reader.json"],
        "\"role\": \"app_reader\"",
        "\"role\": \"app_reader\",\n  \"columns\": {\"_end\": true}",
    );
    assert!(matches!(
        refuses("roles/app_reader.json", sections),
        TreeError::Malformed { .. }
    ));
}

#[test]
fn a_dropped_table_with_a_live_column_or_intent_is_refused() {
    let t = tree();
    let live = edit(
        &t["dbo/legacy.json"],
        "    \"c_ccccc1\": {\"was\": \"dbo.legacy.id\", \"dropped_at\": \"2026-08-30\", \"reason\": \"REG-2026-042 PII erasure\", \"operator\": \"leon\"},",
        "    \"c_ccccc1\": \"id\",",
    );
    assert_eq!(
        refuses("dbo/legacy.json", live),
        TreeError::DroppedWithLive {
            path: "dbo/legacy.json".into()
        }
    );
    let mv = edit(
        &t["dbo/legacy.json"],
        "  }\n}\n",
        "  },\n  \"moves\": {\"t_cccccc\": {\"max_rows\": 1, \"allowed_at\": \"x\", \"reason\": \"x\", \"operator\": \"x\"}, \"_end\": true}\n}\n",
    );
    assert_eq!(
        refuses("dbo/legacy.json", mv),
        TreeError::DroppedWithLive {
            path: "dbo/legacy.json".into()
        }
    );
}

#[test]
fn a_conversion_move_or_rebuild_in_the_wrong_file_is_refused() {
    let t = tree();
    let conversion = t["dbo/customer.json"].clone();
    let section = conversion.split("  \"conversions\"").nth(1).unwrap();
    let moved = format!(
        "{}  \"conversions\"{section}",
        t["app/events.json"].trim_end_matches("\n}\n").to_owned() + ",\n"
    );
    assert!(matches!(
        refuses("app/events.json", moved),
        TreeError::Misplaced {
            section: "conversions",
            ..
        }
    ));
    let mv = edit(
        &t["app/events.json"],
        "\"moves\": {\n\n    \"t_bbbbb1\"",
        "\"moves\": {\n\n    \"t_zzzzzz\"",
    );
    assert!(matches!(
        refuses("app/events.json", mv),
        TreeError::Misplaced {
            section: "moves",
            ..
        }
    ));
    let rebuild = edit(
        &t["dbo/orders.json"],
        "\"replacement_root\": \"t_eeeeee\"",
        "\"replacement_root\": \"t_ffffff\"",
    );
    assert!(matches!(
        refuses("dbo/orders.json", rebuild),
        TreeError::Misplaced {
            section: "rebuilds",
            ..
        }
    ));
}

#[test]
fn a_uid_in_two_files_is_refused() {
    let t = tree();
    let again = edit(&t["roles/app_reader.json"], "\"app_reader\"", "\"other\"");
    let mut files = t;
    files.insert("roles/copy.json".into(), again);
    assert_eq!(
        assemble(&files).unwrap_err(),
        TreeError::DuplicateUid {
            uid: uid("r_aaaaaa"),
            a: "roles/app_reader.json".into(),
            b: "roles/copy.json".into()
        }
    );
    // A column uid repeated under another table is the same refusal.
    let col = edit(
        &files["dbo/customer.json"],
        "\"c_aaaaa1\": \"customer_id\"",
        "\"c_bbbbb1\": \"customer_id\"",
    );
    assert!(matches!(
        refuses("dbo/customer.json", col),
        TreeError::DuplicateUid { uid, .. } if uid == self::uid("c_bbbbb1")
    ));
}

#[test]
fn the_assembled_mapping_must_still_be_valid() {
    let t = tree();
    // Two uids, one name, in two files.
    let same = edit(&t["roles/app_reader.json"], "\"r_aaaaaa\"", "\"r_cccccc\"");
    let mut files = t;
    files.insert("roles/dup.json".into(), same);
    assert!(matches!(
        assemble(&files).unwrap_err(),
        TreeError::Ids(IdsError::DuplicateName { .. })
    ));
}

#[test]
fn a_name_the_mapping_cannot_read_back_is_refused_wherever_the_file_stores_one() {
    let t = tree();
    let bad = |path: &str, from: &str, to: &str| bad_name(refuses(path, edit(&t[path], from, to)));
    let customer = "dbo/customer.json";
    // The table in the header, with too many or too few parts or an empty one.
    assert_eq!(
        bad(
            customer,
            "\"table\": \"dbo.customer\"",
            "\"table\": \"no_schema\""
        ),
        "no_schema"
    );
    assert_eq!(
        bad(
            customer,
            "\"table\": \"dbo.customer\"",
            "\"table\": \"dbo.\""
        ),
        "dbo."
    );
    // A live column: empty, or holding the separator.
    assert_eq!(bad(customer, "\"full_name\"", "\"\""), "");
    assert_eq!(bad(customer, "\"full_name\"", "\"a.b\""), "a.b");
    // A live partition child is a table name.
    assert_eq!(
        bad("app/events.json", "\"app.events_2026_01\"", "\"app.a.b\""),
        "app.a.b"
    );
    assert_eq!(
        bad("app/events.json", "\"app.events_2026_01\"", "\"events\""),
        "events"
    );
    // The names of the other kinds.
    assert_eq!(bad("roles/app_reader.json", "\"app_reader\"", "\"\""), "");
    assert_eq!(bad("extensions/pgcrypto.json", "\"pgcrypto\"", "\"\""), "");
    assert_eq!(
        bad("types/status.json", "\"dbo.status\"", "\"status\""),
        "status"
    );
    // A `was` is only required to say something.
    let was =
        |path: &str, from: &str| bad_name(refuses(path, edit(&t[path], from, "\"was\": \"\"")));
    assert_eq!(was("dbo/legacy.json", "\"was\": \"dbo.legacy\""), "");
    assert_eq!(was("roles/old_role.json", "\"was\": \"old_role\""), "");
    assert_eq!(
        was("dbo/customer.json", "\"was\": \"dbo.customer.national_id\""),
        ""
    );
    assert_eq!(
        was("app/events.json", "\"was\": \"app.events_2025_12\""),
        ""
    );
}

// ---- splitter refusals -------------------------------------------------

#[test]
fn an_object_without_a_place_is_refused() {
    let (ids, mut p) = sample();
    p.paths.remove(&uid("r_aaaaaa"));
    assert_eq!(
        split(&ids, &p).unwrap_err(),
        TreeError::NoPath {
            uid: uid("r_aaaaaa")
        }
    );
    let (ids, mut p) = sample();
    p.children.remove(&uid("t_bbbbb1"));
    assert_eq!(
        split(&ids, &p).unwrap_err(),
        TreeError::NoPath {
            uid: uid("t_bbbbb1")
        }
    );
}

#[test]
fn a_place_for_an_object_the_mapping_lacks_is_refused() {
    let (ids, mut p) = sample();
    p.paths.insert(uid("t_ffffff"), "dbo/ghost.json".into());
    assert_eq!(
        split(&ids, &p).unwrap_err(),
        TreeError::UnknownPath {
            uid: uid("t_ffffff")
        }
    );
}

#[test]
fn two_objects_cannot_share_a_path() {
    let (ids, mut p) = sample();
    p.paths.insert(uid("r_aaaaaa"), "types/status.json".into());
    assert!(matches!(
        split(&ids, &p).unwrap_err(),
        TreeError::PathCollision { .. }
    ));
}

#[test]
fn a_name_the_mapping_cannot_read_back_is_not_written() {
    for (column, why) in [("a.b", "separator"), ("", "empty")] {
        let (mut ids, p) = sample();
        ids.columns.insert(
            uid("c_aaaaa2"),
            ColumnRef::new("dbo.customer".parse().unwrap(), column),
        );
        assert!(
            matches!(split(&ids, &p).unwrap_err(), TreeError::BadName { name, .. } if name == column),
            "{why}"
        );
    }
    let (mut ids, p) = sample();
    ids.tombstones.get_mut(&uid("r_bbbbbb")).unwrap().was = String::new();
    assert!(matches!(
        split(&ids, &p).unwrap_err(),
        TreeError::BadName { .. }
    ));
    let (mut ids, p) = sample();
    ids.roles.insert(uid("r_aaaaaa"), RoleName::new(""));
    assert!(matches!(
        split(&ids, &p).unwrap_err(),
        TreeError::BadName { .. }
    ));
}

#[test]
fn an_invalid_mapping_is_not_written() {
    let (mut ids, p) = sample();
    ids.moves
        .insert(uid("t_zzzzzz"), ids.moves[&uid("t_bbbbb1")].clone());
    assert!(matches!(
        split(&ids, &p).unwrap_err(),
        TreeError::Ids(IdsError::OrphanMove { .. })
    ));
}

#[test]
fn a_table_tombstone_name_that_collides_with_a_live_one_needs_its_owner() {
    // A dropped table and a live one that reuse a name: a tombstone `was`
    // finds both, so the writer asks rather than guesses.
    let (mut ids, mut p) = sample();
    ids.tables
        .insert(uid("t_gggggg"), "dbo.legacy".parse().unwrap());
    p.paths
        .insert(uid("t_gggggg"), "dbo/legacy.aside.json".into());
    assert_eq!(
        split(&ids, &p).unwrap_err(),
        TreeError::UnownedTombstone {
            uid: uid("c_ccccc1")
        }
    );
    p.owners.insert(uid("c_ccccc1"), uid("t_cccccc"));
    assert_eq!(assemble(&split(&ids, &p).unwrap()).unwrap(), ids);
}

#[test]
fn dotted_role_and_extension_names_round_trip_live_and_dropped() {
    // The loader allows a `.` in both; they serialize as standalone strings.
    let (mut ids, p) = sample();
    ids.roles
        .insert(uid("r_aaaaaa"), RoleName::new("app.reader"));
    ids.extensions
        .insert(uid("e_aaaaaa"), ObjectLocalName::new("uuid.ossp"));
    ids.tombstones.insert(uid("r_bbbbbb"), tomb("old.role"));
    let mut p = p;
    p.paths
        .insert(uid("e_bbbbbb"), "extensions/gone.json".into());
    ids.tombstones.insert(uid("e_bbbbbb"), tomb("gone.ext"));
    let t = split(&ids, &p).unwrap();
    assert!(t["roles/app_reader.json"].contains("\"role\": \"app.reader\""));
    assert!(t["roles/old_role.json"].contains("\"role\": \"old.role\""));
    assert!(t["extensions/gone.json"].contains("\"was\": \"gone.ext\""));
    assert_eq!(assemble(&t).unwrap(), ids);
}

#[test]
fn a_dropped_object_may_remember_a_name_nothing_parses() {
    // `was` is compared as text, never parsed, so a dropped table, type or
    // column that remembers an odd spelling still round-trips.
    let (mut ids, p) = sample();
    ids.tombstones.get_mut(&uid("c_aaaaa3")).unwrap().was = "dbo.customer.a.b".into();
    let mut p = p;
    p.owners.insert(uid("c_aaaaa3"), uid("t_aaaaaa"));
    assert_eq!(assemble(&split(&ids, &p).unwrap()).unwrap(), ids);
}

/// A file's own spelling reaches the refusal through serde's text and through
/// its path; neither may start a line of it or rewrite what came before
/// (DEC-2032.1).
#[test]
fn a_refusal_shows_the_file_s_own_text_escaped() {
    let hostile = "x\n\u{1b}[31mred";
    let text = format!(
        "{{\"_generated\": \"g\", \"format\": 1, \"uid\": \"t_aaaaaa\", \"table\": \"dbo.t\", \"{hostile}\": 1}}"
    )
    .replace('\n', "\\n")
    .replace('\u{1b}', "\\u001b");
    let path = "dir\n/a\u{1b}[0m.json";
    let message = parse(path, &text).unwrap_err().to_string();
    assert!(!message.chars().any(char::is_control), "{message:?}");
    assert!(
        message.contains("\\n") && message.contains("\\u{1b}"),
        "{message}"
    );
}
