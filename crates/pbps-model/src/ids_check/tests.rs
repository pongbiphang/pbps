use super::*;
use crate::ids::IdsFile;
use crate::ids_tree::tests::{sample, tree};
use crate::ids_tree::{Placement, split};

const LAYOUT: Layout<'static> = Layout {
    schema_dir: "schema",
    ids_dir: ".pbps/ids",
};

fn decl(yaml: &str, kind: UidKind, name: &str) -> Declaration {
    Declaration {
        yaml: yaml.into(),
        kind,
        name: name.into(),
        renamed_from: None,
    }
}

/// The declarations that go with the live files of `tree()`; the dropped
/// ones have none.
fn decls() -> Vec<Declaration> {
    vec![
        decl("schema/dbo/customer.yml", UidKind::Table, "dbo.customer"),
        decl("schema/app/events.yml", UidKind::Table, "app.events"),
        decl("schema/dbo/orders.yaml", UidKind::Table, "dbo.orders"),
        decl("schema/roles/app_reader.yml", UidKind::Role, "app_reader"),
        decl("schema/types/status.yml", UidKind::Type, "dbo.status"),
        decl(
            "schema/extensions/pgcrypto.yml",
            UidKind::Extension,
            "pgcrypto",
        ),
    ]
}

fn run(
    ids: &BTreeMap<String, String>,
    declared: &[Declaration],
    legacy_file: bool,
) -> Vec<Finding> {
    check(&Input {
        layout: LAYOUT,
        declared,
        ids,
        legacy_file,
    })
}

fn codes(findings: &[Finding]) -> Vec<Code> {
    findings.iter().map(|f| f.code).collect()
}

fn only(findings: Vec<Finding>, code: Code) -> Finding {
    assert_eq!(codes(&findings), vec![code], "{findings:#?}");
    findings.into_iter().next().unwrap()
}

/// Replaces `from` once and proves it did.
fn edit(text: &str, from: &str, to: &str) -> String {
    assert_eq!(text.matches(from).count(), 1, "{from:?} must occur once");
    text.replacen(from, to, 1)
}

fn with(path: &str, text: String) -> BTreeMap<String, String> {
    let mut t = tree();
    t.insert(path.into(), text);
    t
}

fn set_line(text: &str, containing: &str, line: &str) -> String {
    let mut hit = 0;
    let out: Vec<&str> = text
        .lines()
        .map(|l| {
            if l.contains(containing) {
                hit += 1;
                line
            } else {
                l
            }
        })
        .collect();
    assert_eq!(hit, 1, "{containing:?} must match one line");
    out.join("\n") + "\n"
}

#[test]
fn a_consistent_tree_with_its_declarations_has_no_findings() {
    assert_eq!(run(&tree(), &decls(), false), vec![]);
}

#[test]
fn every_code_has_a_distinct_stable_text() {
    let all = [
        Code::YamlWithoutIds,
        Code::Orphan,
        Code::NameMismatch,
        Code::DuplicateUid,
        Code::DroppedButDeclared,
        Code::LiveColumnInDropped,
        Code::DuplicateName,
        Code::PathCollision,
        Code::CaseCollision,
        Code::ConflictMarkers,
        Code::Malformed,
        Code::SentinelMissing,
        Code::Format,
        Code::LegacyLayout,
        Code::LegacyBesideTree,
        Code::NotCanonical,
        Code::InvalidMapping,
    ];
    let texts: BTreeSet<&str> = all.iter().map(|c| c.as_str()).collect();
    assert_eq!(texts.len(), all.len());
    assert!(texts.iter().all(|t| t.starts_with("ids.")));
}

#[test]
fn a_declaration_without_an_identity_file_is_reported() {
    let mut ids = tree();
    ids.remove("dbo/customer.json");
    let f = only(run(&ids, &decls(), false), Code::YamlWithoutIds);
    assert_eq!(f.path, "schema/dbo/customer.yml");
    assert_eq!(
        f.message,
        "dbo.customer (schema/dbo/customer.yml) has no identity file at .pbps/ids/dbo/customer.json"
    );
    assert_eq!(f.remedy, "pbps plan");
}

#[test]
fn a_moved_declaration_is_one_orphan_not_also_a_missing_file() {
    let mut d = decls();
    d[0].yaml = "schema/sales/customer.yml".into();
    let f = only(run(&tree(), &d, false), Code::Orphan);
    assert_eq!(f.path, ".pbps/ids/dbo/customer.json");
    assert!(f.message.contains("dbo.customer"), "{}", f.message);
}

#[test]
fn two_files_that_could_take_the_moved_declaration_leave_it_reported_missing() {
    let mut ids = tree();
    let twin = ids["dbo/customer.json"]
        .replace("t_aaaaaa", "t_aaaaab")
        .replace("c_aaaaa1", "c_aaaaa5")
        .replace("c_aaaaa2", "c_aaaaa6")
        .replace("c_aaaaa3", "c_aaaaa7")
        .replace("c_aaaaa4", "c_aaaaa8");
    ids.insert("dbo/customer_copy.json".into(), twin);
    let mut d = decls();
    d[0].yaml = "schema/sales/customer.yml".into();
    let found = run(&ids, &d, false);
    assert!(codes(&found).contains(&Code::YamlWithoutIds), "{found:#?}");
}

#[test]
fn a_live_file_without_a_declaration_is_an_orphan() {
    let mut d = decls();
    d.remove(0);
    let f = only(run(&tree(), &d, false), Code::Orphan);
    assert_eq!(f.path, ".pbps/ids/dbo/customer.json");
    assert!(f.message.contains("names dbo.customer"), "{}", f.message);
}

#[test]
fn a_dropped_file_without_a_declaration_is_not_an_orphan() {
    // legacy.json, orders_old.json and old_role.json are dropped and
    // undeclared in the clean tree.
    assert!(!codes(&run(&tree(), &decls(), false)).contains(&Code::Orphan));
}

#[test]
fn a_declaration_that_says_another_name_is_a_mismatch() {
    let mut d = decls();
    d[0].name = "dbo.client".into();
    let f = only(run(&tree(), &d, false), Code::NameMismatch);
    assert_eq!(f.path, ".pbps/ids/dbo/customer.json");
    assert!(f.message.contains("dbo.customer") && f.message.contains("dbo.client"));
}

#[test]
fn renamed_from_covers_a_name_that_has_not_been_synced_yet() {
    let mut d = decls();
    d[0].name = "dbo.client".into();
    d[0].renamed_from = Some("dbo.customer".into());
    assert_eq!(run(&tree(), &d, false), vec![]);
}

#[test]
fn a_declaration_of_another_kind_than_its_file_is_a_mismatch() {
    let mut d = decls();
    d[3].kind = UidKind::Extension;
    assert_eq!(codes(&run(&tree(), &d, false)), vec![Code::NameMismatch]);
}

#[test]
fn one_uid_in_two_files_is_reported_with_both_paths() {
    let mut ids = tree();
    ids.insert("dbo/copy.json".into(), ids["dbo/customer.json"].clone());
    let mut d = decls();
    d.push(decl("schema/dbo/copy.yml", UidKind::Table, "dbo.customer"));
    let found = run(&ids, &d, false);
    let f = found.iter().find(|f| f.code == Code::DuplicateUid).unwrap();
    assert!(
        f.message.contains(".pbps/ids/dbo/copy.json")
            && f.message.contains(".pbps/ids/dbo/customer.json"),
        "{}",
        f.message
    );
}

#[test]
fn one_uid_listed_twice_in_a_file_is_a_duplicate_uid() {
    let t = tree();
    let text = edit(&t["dbo/customer.json"], "\"c_aaaaa2\"", "\"c_aaaaa1\"");
    let f = only(
        run(&with("dbo/customer.json", text), &decls(), false),
        Code::DuplicateUid,
    );
    assert_eq!(f.path, ".pbps/ids/dbo/customer.json");
}

#[test]
fn a_dropped_file_whose_declaration_exists_is_reported() {
    let mut d = decls();
    d.push(decl("schema/dbo/legacy.yml", UidKind::Table, "dbo.legacy"));
    let f = only(run(&tree(), &d, false), Code::DroppedButDeclared);
    assert_eq!(f.path, ".pbps/ids/dbo/legacy.json");
    assert!(
        f.message.ends_with("schema/dbo/legacy.yml exists"),
        "{}",
        f.message
    );
}

#[test]
fn a_live_column_in_a_dropped_file_is_reported() {
    let t = tree();
    let text = set_line(
        &t["dbo/legacy.json"],
        "\"c_ccccc1\"",
        "    \"c_ccccc1\": \"id\",",
    );
    let f = only(
        run(&with("dbo/legacy.json", text), &decls(), false),
        Code::LiveColumnInDropped,
    );
    assert_eq!(f.path, ".pbps/ids/dbo/legacy.json");
}

#[test]
fn two_uids_with_one_name_are_reported_with_both_files() {
    let t = tree();
    let text = edit(
        &t["app/events.json"],
        "\"table\": \"app.events\"",
        "\"table\": \"dbo.customer\"",
    );
    let found = run(&with("app/events.json", text), &decls(), false);
    let f = found
        .iter()
        .find(|f| f.code == Code::DuplicateName)
        .unwrap();
    assert!(
        f.message.contains("dbo/customer.json") && f.message.contains("app/events.json"),
        "{}",
        f.message
    );
}

#[test]
fn two_declarations_on_one_ids_path_collide() {
    let mut d = decls();
    d.push(decl(
        "schema/dbo/customer.yaml",
        UidKind::Table,
        "dbo.customer",
    ));
    let found = run(&tree(), &d, false);
    let f = found
        .iter()
        .find(|f| f.code == Code::PathCollision)
        .unwrap();
    assert_eq!(
        f.message,
        "schema/dbo/customer.yml and schema/dbo/customer.yaml both map to .pbps/ids/dbo/customer.json"
    );
}

#[test]
fn different_paths_do_not_collide() {
    assert!(!codes(&run(&tree(), &decls(), false)).contains(&Code::PathCollision));
}

#[test]
fn declaration_paths_equal_ignoring_case_collide() {
    let mut d = decls();
    d.push(decl("schema/DBO/customer.yml", UidKind::Table, "dbo.other"));
    let found = run(&tree(), &d, false);
    assert!(codes(&found).contains(&Code::CaseCollision), "{found:#?}");
}

#[test]
fn declarations_differing_in_case_and_extension_collide_on_their_ids_paths() {
    let mut d = decls();
    d.push(decl(
        "schema/DBO/customer.yaml",
        UidKind::Table,
        "dbo.other",
    ));
    let found = run(&tree(), &d, false);
    let f = found
        .iter()
        .find(|f| f.code == Code::CaseCollision)
        .unwrap();
    assert_eq!(
        f.message,
        ".pbps/ids/DBO/customer.json and .pbps/ids/dbo/customer.json differ only in case"
    );
}

#[test]
fn a_yml_and_a_yaml_in_one_directory_collide_on_one_ids_path_not_on_case() {
    let mut d = decls();
    d.push(decl(
        "schema/dbo/customer.yaml",
        UidKind::Table,
        "dbo.other",
    ));
    let codes = codes(&run(&tree(), &d, false));
    assert!(codes.contains(&Code::PathCollision));
    assert!(!codes.contains(&Code::CaseCollision));
}

#[test]
fn one_path_declared_once_is_not_a_case_collision() {
    assert!(!codes(&run(&tree(), &decls(), false)).contains(&Code::CaseCollision));
}

#[test]
fn identity_paths_equal_ignoring_case_collide() {
    let mut ids = tree();
    ids.insert("DBO/customer.json".into(), ids["dbo/customer.json"].clone());
    let found = run(&ids, &decls(), false);
    assert!(codes(&found).contains(&Code::CaseCollision), "{found:#?}");
}

#[test]
fn conflict_markers_are_reported_and_name_the_file() {
    let t = tree();
    let text = format!("<<<<<<< ours\n{}", t["roles/app_reader.json"]);
    let f = only(
        run(&with("roles/app_reader.json", text), &decls(), false),
        Code::ConflictMarkers,
    );
    assert_eq!(f.path, ".pbps/ids/roles/app_reader.json");
}

#[test]
fn invalid_json_is_malformed() {
    let f = only(
        run(&with("roles/app_reader.json", "{".into()), &decls(), false),
        Code::Malformed,
    );
    assert!(
        f.message.starts_with(".pbps/ids/roles/app_reader.json"),
        "{}",
        f.message
    );
}

#[test]
fn a_section_without_its_end_entry_is_a_missing_sentinel() {
    let t = tree();
    let text = edit(
        &t["app/events.json"],
        "\"c_bbbbb1\": \"at\",\n\n    \"_end\": true\n",
        "\"c_bbbbb1\": \"at\"\n",
    );
    let f = only(
        run(&with("app/events.json", text), &decls(), false),
        Code::SentinelMissing,
    );
    assert_eq!(f.path, ".pbps/ids/app/events.json");
}

#[test]
fn another_layout_format_is_reported() {
    let t = tree();
    let text = edit(
        &t["roles/app_reader.json"],
        "\"format\": 1",
        "\"format\": 2",
    );
    only(
        run(&with("roles/app_reader.json", text), &decls(), false),
        Code::Format,
    );
}

#[test]
fn the_old_file_alone_is_the_legacy_layout() {
    let f = only(run(&BTreeMap::new(), &[], true), Code::LegacyLayout);
    assert_eq!(
        f.message,
        "schema.ids.json is the old layout; this pbps reads .pbps/ids/"
    );
    assert_eq!(f.remedy, "pbps ids migrate");
}

#[test]
fn the_old_file_beside_a_tree_is_reported_as_such() {
    let f = only(run(&tree(), &decls(), true), Code::LegacyBesideTree);
    assert_eq!(f.message, "both schema.ids.json and .pbps/ids/ exist");
}

#[test]
fn no_old_file_means_no_legacy_finding() {
    assert_eq!(run(&BTreeMap::new(), &[], false), vec![]);
}

#[test]
fn hand_edited_spacing_is_not_canonical_but_still_loads() {
    let t = tree();
    let text = edit(
        &t["roles/app_reader.json"],
        "\"format\": 1,\n",
        "\"format\": 1,\n\n\n",
    );
    let f = only(
        run(&with("roles/app_reader.json", text), &decls(), false),
        Code::NotCanonical,
    );
    assert_eq!(f.remedy, "pbps ids fmt");
}

#[test]
fn a_mapping_the_core_validator_refuses_is_reported_with_its_file() {
    // The rebuild's source must be a tombstone; t_aaaaaa is live.
    let t = tree();
    let text = edit(
        &t["dbo/orders.json"],
        "\"source\": {\"tables\": {\"t_dddddd\"",
        "\"source\": {\"tables\": {\"t_aaaaaa\"",
    );
    let f = only(
        run(&with("dbo/orders.json", text), &decls(), false),
        Code::InvalidMapping,
    );
    assert_eq!(f.path, ".pbps/ids/dbo/orders.json");
}

#[test]
fn findings_come_back_sorted_and_all_of_them() {
    let mut ids = tree();
    ids.remove("dbo/customer.json");
    ids.insert("roles/app_reader.json".into(), "{".into());
    let found = run(&ids, &decls(), true);
    let got = codes(&found);
    assert!(
        got.contains(&Code::YamlWithoutIds)
            && got.contains(&Code::Malformed)
            && got.contains(&Code::LegacyBesideTree)
    );
    let keys: Vec<_> = found.iter().map(|f| (f.path.clone(), f.code)).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

#[test]
fn a_malformed_file_does_not_hide_the_others_checks() {
    let mut d = decls();
    d[1].name = "app.other".into();
    let found = run(&with("roles/app_reader.json", "{".into()), &d, false);
    let got = codes(&found);
    assert!(
        got.contains(&Code::Malformed) && got.contains(&Code::NameMismatch),
        "{found:#?}"
    );
}

/// Models the splitter wrote from scratch must never trip a check, whatever
/// their shape: tables with and without columns and tombstones, roles, types.
#[test]
fn a_split_model_never_trips_any_check() {
    let (sample_ids, sample_placement) = sample();
    let mut models = vec![(sample_ids, sample_placement)];
    for tables in 0..4u32 {
        for columns in 0..4u32 {
            let mut ids = IdsFile::default();
            let mut placement = Placement::default();
            for t in 0..tables {
                let letter = char::from(b'a' + u8::try_from(t).unwrap());
                let tu: Uid = format!("t_{letter}{letter}{letter}{letter}{letter}{letter}")
                    .parse()
                    .unwrap();
                ids.tables
                    .insert(tu.clone(), format!("dbo.t{t}").parse().unwrap());
                placement.paths.insert(tu, format!("dbo/t{t}.json"));
                for c in 0..columns {
                    let cu: Uid = format!("c_{letter}{letter}{letter}{letter}{letter}{c}")
                        .parse()
                        .unwrap();
                    ids.columns
                        .insert(cu, format!("dbo.t{t}.col{c}").parse().unwrap());
                }
            }
            ids.validate().unwrap();
            models.push((ids, placement));
        }
    }
    for (ids, placement) in models {
        let files = split(&ids, &placement).unwrap();
        // One declaration per live file, named as the file says.
        let declared: Vec<Declaration> = files
            .iter()
            .filter_map(|(rel, text)| {
                let file = parse(rel, text).unwrap();
                file.dropped.is_none().then(|| {
                    let yaml = format!("schema/{}", rel.replace(".json", ".yml"));
                    decl(&yaml, file.uid.kind(), &name_of(&file))
                })
            })
            .collect();
        assert_eq!(run(&files, &declared, false), vec![], "{files:#?}");
    }
}
