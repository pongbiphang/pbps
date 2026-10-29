use super::{SchemaKind, schema};
use serde_json::json;

#[test]
fn database_layout_schema_and_project_parser_agree() {
    let validator = jsonschema::validator_for(&schema(SchemaKind::Config)).unwrap();
    for (config, valid) in [
        (json!({"dialect":"mssql"}), true),
        (json!({"dialect":"postgres"}), true),
        (
            json!({"dialect":"mssql","databases":["alpha","beta"]}),
            true,
        ),
        (
            json!({"dialect":"mssql","databases":["db.2026","顧客"]}),
            true,
        ),
        (json!({"dialect":"mssql","databases":[]}), false),
        (json!({"dialect":"mssql","databases":null}), false),
        (json!({"dialect":"mssql","databases":["a","a"]}), false),
        (json!({"dialect":"mssql","databases":[1]}), false),
        (json!({"dialect":"mssql","databases":[""]}), false),
        (json!({"dialect":"mssql","databases":[".."]}), false),
        (json!({"dialect":"mssql","databases":["../a"]}), false),
        (json!({"dialect":"mssql","databases":["C:one"]}), false),
        (json!({"dialect":"mssql","databases":["a\nb"]}), false),
        (json!({"dialect":"postgres","databases":["a"]}), false),
        (
            json!({"dialect":"mssql","databases":["a"],"ids_file":"flat.json"}),
            false,
        ),
    ] {
        assert_eq!(validator.is_valid(&config), valid, "schema: {config}");
        assert_eq!(
            pbps_config::Config::parse_project(
                &config.to_string(),
                std::path::Path::new("pbps.yml")
            )
            .is_ok(),
            valid,
            "parser: {config}"
        );
    }
}
