//! What a saved plan's changes read and write, pinned (#2790).
//!
//! `Change` is about to become an enum of families, with hand-written
//! serialisation. A plan on disk must not notice: each change is one flat
//! object tagged by `op`, an unknown op, field or type is refused with the
//! words below, and the position the decoder names is the one it names today.
//! The compiler sees none of this, so these tests state it. A refusal's words
//! may change only by a decision recorded where the change is made.

use pbps_model::{Change, PlannedChange, SavedPlan};
use serde_json::Value;
use std::path::Path;

/// Every op, in the order `Change` declares its kinds: the list an unknown op is
/// refused with (`expected one of ...`), so a family that regroups the enum must
/// still say them in this order.
const OPS: &[&str] = &[
    "rebuild_partitioned_table",
    "copy_table_rows",
    "analyze_table_tree",
    "build_partitioned_index",
    "add_enum_value",
    "rename_enum_value",
    "alter_domain",
    "enum_rebuild",
    "create_type",
    "drop_type",
    "rename_type",
    "create_extension",
    "drop_extension",
    "update_extension",
    "set_extension_schema",
    "create_table",
    "drop_table",
    "detach_partition",
    "attach_partition",
    "rename_table",
    "add_column",
    "drop_column",
    "rename_column",
    "alter_column_type",
    "alter_column_nullability",
    "alter_column_default",
    "alter_column_expression",
    "alter_column_numbering",
    "add_computed_column",
    "drop_computed_column",
    "set_column_deprecated",
    "set_replica_identity",
    "set_comment",
    "set_storage_parameters",
    "set_table_persistence",
    "set_partition_default",
    "set_partition_not_null",
    "set_index_storage_parameters",
    "create_policy",
    "drop_policy",
    "set_row_security",
    "set_primary_key",
    "add_unique",
    "drop_unique",
    "set_exclusion",
    "add_foreign_key",
    "drop_foreign_key",
    "add_check",
    "drop_check",
    "add_index",
    "drop_index",
    "insert_row",
    "update_row",
    "delete_row",
    "set_data_mode",
    "set_ensure_keys",
    "create_module",
    "alter_module",
    "drop_module",
    "create_role",
    "drop_role",
    "rename_role",
    "grant",
    "revoke",
    "public_execution",
];

fn ops_listed() -> String {
    OPS.iter()
        .map(|op| format!("`{op}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// (what it is, input, what `PlannedChange` and `Change` write back, which is the
/// same flat object). A change that has no extras is written without them.
const ACCEPTED: &[(&str, &str, &str)] = &[
    (
        r"ext create",
        r#"{"op":"create_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"}}"#,
        r#"{"op":"create_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"}}"#,
    ),
    (
        r"ext create requires",
        r#"{"op":"create_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"},"requires":["e_bbbbbb"]}"#,
        r#"{"op":"create_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"},"requires":["e_bbbbbb"]}"#,
    ),
    (
        r"ext drop",
        r#"{"op":"drop_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"},"requirers":["e_cccccc"],"releases":[],"index_releases":[]}"#,
        r#"{"op":"drop_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"},"requirers":["e_cccccc"]}"#,
    ),
    (
        r"ext update",
        r#"{"op":"update_extension","uid":"e_aaaaaa","name":"citext","from":{"schema":"public","version":"1.5"},"to":{"schema":"public","version":"1.6"}}"#,
        r#"{"op":"update_extension","uid":"e_aaaaaa","name":"citext","from":{"schema":"public","version":"1.5"},"to":{"schema":"public","version":"1.6"}}"#,
    ),
    (
        r"ext set schema",
        r#"{"op":"set_extension_schema","uid":"e_aaaaaa","name":"citext","from":{"schema":"public","version":"1.6"},"to":{"schema":"ext","version":"1.6"}}"#,
        r#"{"op":"set_extension_schema","uid":"e_aaaaaa","name":"citext","from":{"schema":"public","version":"1.6"},"to":{"schema":"ext","version":"1.6"}}"#,
    ),
    (
        r"ok drop_check",
        r#"{"op":"drop_check","table":"app.t","name":"ck"}"#,
        r#"{"op":"drop_check","table":"app.t","name":"ck"}"#,
    ),
    (
        r"ok drop_index",
        r#"{"op":"drop_index","table":"app.t","name":"ix"}"#,
        r#"{"op":"drop_index","table":"app.t","name":"ix"}"#,
    ),
    (
        r"ok drop_role",
        r#"{"op":"drop_role","uid":"r_aaaaaa","name":"x","members":[]}"#,
        r#"{"op":"drop_role","uid":"r_aaaaaa","name":"x"}"#,
    ),
    (
        r"ok drop_table",
        r#"{"op":"drop_table","uid":"t_aaaaaa","name":"app.t"}"#,
        r#"{"op":"drop_table","uid":"t_aaaaaa","name":"app.t"}"#,
    ),
    (
        r"ok set_data_mode",
        r#"{"op":"set_data_mode","table":"app.t","from":null,"to":"exact"}"#,
        r#"{"op":"set_data_mode","table":"app.t","from":null,"to":"exact"}"#,
    ),
];

/// (what is wrong, input, the refusal by `PlannedChange`, the refusal by `Change`).
/// `{OPS}` stands for the list of every op. The two differ because `PlannedChange`
/// flattens `Change` and so reads the object itself first; both are pinned.
const REFUSED: &[(&str, &str, &str, &str)] = &[
    (
        r"planned ext create bad risk",
        r#"{"op":"create_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"},"risks":["nope"]}"#,
        r"unknown variant `nope`, expected one of `rename`, `destructive`, `narrowing`, `not-null`, `constraint`, `data-update`, `data-delete`, `revoke`, `grant-widen`, `numbering`, `row-security`, `type-rebuild`, `extension-code` at line 1 column 122",
        r"unknown field `risks`, expected one of `uid`, `name`, `definition`, `requires`",
    ),
    (
        r"err unknown op",
        r#"{"op":"nope","uid":"e_aaaaaa"}"#,
        r"unknown variant `nope`, expected one of {OPS} at line 1 column 30",
        r"unknown variant `nope`, expected one of {OPS} at line 1 column 12",
    ),
    (
        r"err unknown op, case",
        r#"{"op":"Create_Extension","uid":"e_aaaaaa"}"#,
        r"unknown variant `Create_Extension`, expected one of {OPS} at line 1 column 42",
        r"unknown variant `Create_Extension`, expected one of {OPS} at line 1 column 24",
    ),
    (
        r"err missing op",
        r#"{"uid":"e_aaaaaa","name":"citext"}"#,
        r"missing field `op` at line 1 column 34",
        r"missing field `op` at line 1 column 34",
    ),
    (
        r"err op number",
        r#"{"op":5}"#,
        r"missing field `uid` at line 1 column 8",
        r"invalid type: integer `5`, expected variant identifier at line 1 column 7",
    ),
    (
        r"err op null",
        r#"{"op":null}"#,
        r"invalid type: null, expected variant identifier at line 1 column 11",
        r"invalid type: null, expected variant identifier at line 1 column 10",
    ),
    (
        r"err op array",
        r#"{"op":["a"]}"#,
        r"invalid type: sequence, expected variant identifier at line 1 column 12",
        r"invalid type: sequence, expected variant identifier at line 1 column 6",
    ),
    (
        r"err not an object",
        r"[1,2]",
        r"invalid type: sequence, expected struct PlannedChange at line 1 column 0",
        r"invalid type: integer `1`, expected variant identifier at line 1 column 2",
    ),
    (
        r"err string",
        r#""create_extension""#,
        r#"invalid type: string "create_extension", expected struct PlannedChange at line 1 column 18"#,
        r#"invalid type: string "create_extension", expected internally tagged enum Change at line 1 column 18"#,
    ),
    (
        r"err ext unknown field",
        r#"{"op":"create_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"},"bogus":1}"#,
        r"unknown field `bogus`, expected one of `uid`, `name`, `definition`, `requires` at line 1 column 117",
        r"unknown field `bogus`, expected one of `uid`, `name`, `definition`, `requires`",
    ),
    (
        r"err ext field of another kind",
        r#"{"op":"create_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"},"requirers":[]}"#,
        r"unknown field `requirers`, expected one of `uid`, `name`, `definition`, `requires` at line 1 column 122",
        r"unknown field `requirers`, expected one of `uid`, `name`, `definition`, `requires`",
    ),
    (
        r"err ext field of another family",
        r#"{"op":"create_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"},"table":"app.t"}"#,
        r"unknown field `table`, expected one of `uid`, `name`, `definition`, `requires` at line 1 column 123",
        r"unknown field `table`, expected one of `uid`, `name`, `definition`, `requires`",
    ),
    (
        r"err ext name type",
        r#"{"op":"create_extension","uid":"e_aaaaaa","name":5,"definition":{"schema":"public","version":"1.6"}}"#,
        r"invalid type: integer `5`, expected a string at line 1 column 100",
        r"invalid type: integer `5`, expected a string",
    ),
    (
        r"err ext missing definition",
        r#"{"op":"create_extension","uid":"e_aaaaaa","name":"citext"}"#,
        r"missing field `definition` at line 1 column 58",
        r"missing field `definition`",
    ),
    (
        r"err ext definition unknown field",
        r#"{"op":"create_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"p","version":"1","x":1}}"#,
        r"unknown field `x`, expected `schema` or `version` at line 1 column 106",
        r"unknown field `x`, expected `schema` or `version`",
    ),
    (
        r"err ext duplicate op",
        r#"{"op":"create_extension","op":"drop_extension","uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"}}"#,
        r"duplicate field `op` at line 1 column 129",
        r"duplicate field `op` at line 1 column 29",
    ),
    (
        r"err other family unknown field",
        r#"{"op":"drop_table","uid":"t_aaaaaa","name":"app.t","bogus":1}"#,
        r"unknown field `bogus`, expected one of `uid`, `name`, `detach_from` at line 1 column 61",
        r"unknown field `bogus`, expected one of `uid`, `name`, `detach_from`",
    ),
    (
        r"err other family missing field",
        r#"{"op":"drop_table","uid":"t_aaaaaa"}"#,
        r"missing field `name` at line 1 column 36",
        r"missing field `name`",
    ),
    (
        r"err op last",
        r#"{"uid":"e_aaaaaa","name":"citext","definition":{"schema":"public","version":"1.6"},"op":"nope"}"#,
        r"unknown variant `nope`, expected one of {OPS} at line 1 column 95",
        r"unknown variant `nope`, expected one of {OPS} at line 1 column 94",
    ),
    (
        r"err indexes unknown field",
        r#"{"op":"drop_index","table":"app.t","name":"ix","bogus":1}"#,
        r"unknown field `bogus`, expected `table` or `name` at line 1 column 57",
        r"unknown field `bogus`, expected `table` or `name`",
    ),
    (
        r"err constraints missing field",
        r#"{"op":"drop_check","table":"app.t"}"#,
        r"missing field `name` at line 1 column 35",
        r"missing field `name`",
    ),
    (
        r"err constraints wrong type",
        r#"{"op":"drop_check","table":5,"name":"ck"}"#,
        r"invalid type: integer `5`, expected a string at line 1 column 41",
        r"invalid type: integer `5`, expected a string",
    ),
    (
        r"err roles field of another kind",
        r#"{"op":"drop_role","uid":"r_aaaaaa","name":"x","requires":[]}"#,
        r"unknown field `requires`, expected one of `uid`, `name`, `members` at line 1 column 60",
        r"unknown field `requires`, expected one of `uid`, `name`, `members`",
    ),
    (
        r"err rows unknown field",
        r#"{"op":"set_data_mode","table":"app.t","from":null,"to":null,"bogus":1}"#,
        r"unknown field `bogus`, expected one of `table`, `from`, `to` at line 1 column 70",
        r"unknown field `bogus`, expected one of `table`, `from`, `to`",
    ),
];

#[test]
fn an_accepted_change_is_written_back_as_the_same_flat_object() {
    for (what, input, written) in ACCEPTED {
        let planned: PlannedChange =
            serde_json::from_str(input).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(&serde_json::to_string(&planned).unwrap(), written, "{what}");
        let change: Change = serde_json::from_str(input).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(&serde_json::to_string(&change).unwrap(), written, "{what}");
        let object: Value = serde_json::from_str(written).unwrap();
        assert!(
            object.get("op").is_some(),
            "{what}: the change is one object tagged by `op`"
        );
    }
}

#[test]
fn a_refused_change_is_refused_with_the_words_it_always_was() {
    let listed = ops_listed();
    for (what, input, planned, change) in REFUSED {
        let planned = planned.replace("{OPS}", &listed);
        let change = change.replace("{OPS}", &listed);
        assert_eq!(
            serde_json::from_str::<PlannedChange>(input)
                .unwrap_err()
                .to_string(),
            planned,
            "{what}, as a PlannedChange"
        );
        assert_eq!(
            serde_json::from_str::<Change>(input)
                .unwrap_err()
                .to_string(),
            change,
            "{what}, as a Change"
        );
    }
}

/// Negative: the two tables do not overlap, and every kind that is refused for an
/// unknown field is the same kind that is accepted without it.
#[test]
fn the_tables_say_different_things() {
    for (what, input, ..) in REFUSED {
        assert!(
            !ACCEPTED.iter().any(|(_, ok, _)| ok == input),
            "{what} is both accepted and refused"
        );
    }
    let unknown = REFUSED
        .iter()
        .filter(|(what, ..)| what.contains("unknown field"))
        .count();
    assert!(
        unknown >= 3,
        "unknown fields are pinned for several kinds, not one"
    );
}

fn json_files(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if name != "target" && !name.starts_with('.') {
                json_files(&path, found);
            }
        } else if name.ends_with(".json") {
            found.push(path);
        }
    }
}

/// The objects tagged by `op` in a document, outermost first.
fn tagged(value: &Value, out: &mut Vec<Value>) {
    if let Value::Object(map) = value {
        if map.get("op").is_some_and(Value::is_string) {
            out.push(value.clone());
        } else {
            map.values().for_each(|v| tagged(v, out));
        }
    } else if let Value::Array(items) = value {
        items.iter().for_each(|v| tagged(v, out));
    }
}

/// Every change in the repository's plan-shaped fixtures reads and writes back in a
/// stable form (the form pbps wrote, where the fixture is one), and every whole saved
/// plan to the bytes it came from.
#[test]
fn the_plan_fixtures_round_trip() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    json_files(&root, &mut files);
    let (mut changes, mut plans) = (0, 0);
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        if !text.contains("\"op\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let mut found = Vec::new();
        tagged(&value, &mut found);
        for original in found {
            // Some fixtures hold `op` objects of another type; they are not changes.
            if let Ok(planned) = serde_json::from_value::<PlannedChange>(original.clone()) {
                let again = serde_json::to_value(&planned).unwrap();
                // A fixture that pbps wrote is already in the written form; a
                // hand-written one may leave defaults out, and then only the
                // written form has to be stable.
                if path.to_string_lossy().contains("name-compatibility") {
                    assert_eq!(again, original, "{}", path.display());
                }
                let twice: PlannedChange = serde_json::from_value(again.clone()).unwrap();
                assert_eq!(
                    serde_json::to_string(&twice).unwrap(),
                    serde_json::to_string(&planned).unwrap()
                );
                changes += 1;
            }
        }
        if let Ok(plan) = serde_json::from_str::<SavedPlan>(&text) {
            assert_eq!(
                serde_json::to_string(&plan).unwrap(),
                text.trim_end(),
                "{} is not written back byte for byte",
                path.display()
            );
            plans += 1;
        }
    }
    assert!(
        changes >= 100,
        "the sweep found only {changes} changes: the fixtures moved?"
    );
    assert!(
        plans >= 5,
        "the sweep found only {plans} whole saved plans: the fixtures moved?"
    );
}
