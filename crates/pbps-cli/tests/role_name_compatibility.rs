//! Captured on unchanged master 9c9e6084 before #2084 step 3's production edits.
//! Role identities and grant-target spellings retain the approved artifact bytes.

use pbps_dialect::Dialect;
use pbps_model::*;

#[path = "../src/declaration_file.rs"]
mod declaration_file;

const NAME: &str = "role\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";
const SHOWN: &str = r"role\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";

fn artifacts(text: &str, dialect: &dyn Dialect) -> (Schema, SavedPlan, StateSnapshot) {
    let target = GrantTarget::Object(TableName::new("app", text));
    let mut role = Role {
        description: Some("a role's original text".into()),
        grants: [
            (target.clone(), [Permission::Select].into()),
            (
                GrantTarget::Schema(text.into()),
                [Permission::Execute].into(),
            ),
        ]
        .into(),
    };
    if dialect.name() == "postgres" {
        role.grants.insert(
            GrantTarget::Routine(RoutineId::new(
                TableName::new("app", text),
                vec!["integer".parse().unwrap()],
            )),
            [Permission::Execute].into(),
        );
    }
    let schema = Schema {
        roles: [(text.into(), role)].into(),
        ..Default::default()
    };
    let ids = IdsFile {
        roles: [("r_aaaaaa".parse().unwrap(), text.into())].into(),
        ..Default::default()
    };
    let mut changes = Vec::new();
    if dialect.name() == "mssql" {
        changes.push(PlannedChange::new(Change::CreateRole {
            uid: "r_aaaaaa".parse().unwrap(),
            name: text.into(),
        }));
        changes.push(PlannedChange::new(Change::RenameRole {
            uid: "r_bbbbbb".parse().unwrap(),
            from: format!("{text}old").into(),
            to: format!("{text}new").into(),
        }));
        changes.push(PlannedChange::new(Change::DropRole {
            uid: "r_bbbbbb".parse().unwrap(),
            name: format!("{text}new").into(),
            members: vec![format!("{text}member").into()],
        }));
    }
    for (target, permissions) in &schema.roles.values().next().unwrap().grants {
        changes.push(PlannedChange::new(Change::Grant {
            role: text.into(),
            target: target.clone(),
            permissions: permissions.clone(),
        }));
    }
    changes.push(PlannedChange::new(Change::Revoke {
        role: text.into(),
        target,
        permissions: [Permission::Update].into(),
    }));
    let plan = SavedPlan::new(
        PlanOrigin::Database,
        dialect.name(),
        "2026-10-19T00:00:00Z",
        PlanBaseline {
            time_zone: None,
            description: "role-name master fixture".into(),
            checksum: state_checksum(&schema, &ids),
            database_collation: None,
        },
        ChangeSet { changes },
        ids.clone(),
    );
    let mut state = StateSnapshot::new(StateKind::Apply, schema.clone(), ids, "fixture");
    state.declared = Declared::from_schema(&schema);
    state.plan_checksum = Some(plan.checksum());
    (schema, plan, state)
}

#[test]
fn role_artifacts_keep_master_bytes_checksums_sql_and_declarations() {
    for dialect in [
        &pbps_pg::Postgres::new() as &dyn Dialect,
        &pbps_mssql::Mssql,
    ] {
        let (schema, plan, state) = artifacts(NAME, dialect);
        let sql = plan
            .changes
            .changes
            .iter()
            .flat_map(|p| dialect.emit(&p.change, Default::default()).unwrap())
            .map(|s| s.sql)
            .collect::<Vec<_>>()
            .join("\n");
        let pending = [Intent::RenameRole {
            from: format!("{NAME}before").into(),
            to: NAME.into(),
        }];
        let yaml = schema
            .roles
            .iter()
            .map(|(name, role)| pbps_load::fmt::render_role(name, role, &pending))
            .collect::<Vec<_>>()
            .join("\n---\n");
        // Historical fixture paths use `/` on every host.
        let files = schema
            .roles
            .keys()
            .map(|name| {
                declaration_file::role_path(std::path::Path::new("schema"), name)
                    .unwrap()
                    .strip_prefix("schema")
                    .unwrap()
                    .components()
                    .map(|part| part.as_os_str().to_str().unwrap())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let dir =
            std::env::temp_dir().join(format!("pbps-role-compatibility-{}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
        let explain = std::process::Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&dir)
            .args(["explain", "--plan", "plan.json", "--format", "json"])
            .output()
            .unwrap();
        let text = std::process::Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&dir)
            .args(["explain", "--plan", "plan.json", "--format", "text"])
            .output()
            .unwrap();
        assert!(text.status.success(), "{text:?}");
        let text = String::from_utf8(text.stdout).unwrap();
        assert!(text.contains(&format!("role {SHOWN}")), "{text:?}");
        assert!(!text.contains(NAME), "{text:?}");
        assert!(
            !text.contains(&pbps_model::escape::Escaped(SHOWN).to_string()),
            "{text:?}"
        );
        std::fs::remove_dir_all(dir).unwrap();
        assert!(explain.status.success(), "{explain:?}");
        let outputs = [
            ("schema.json", serde_json::to_string(&schema).unwrap()),
            ("ids.json", serde_json::to_string(&state.ids).unwrap()),
            ("plan.json", serde_json::to_string(&plan).unwrap()),
            ("state.json", serde_json::to_string(&state).unwrap()),
            ("plan.sha256", plan.checksum()),
            ("state.sha256", state_checksum(&schema, &state.ids)),
            ("statements.sql", sql),
            ("declarations.yml", yaml),
            ("filenames.txt", files),
            ("explain.json", String::from_utf8(explain.stdout).unwrap()),
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/name-compatibility/roles")
            .join(dialect.name());
        for (name, actual) in outputs {
            assert_eq!(
                actual,
                std::fs::read_to_string(root.join(name)).unwrap(),
                "{name}"
            );
        }
        assert_eq!(
            serde_json::from_str::<SavedPlan>(&serde_json::to_string(&plan).unwrap()).unwrap(),
            plan
        );
        assert_eq!(
            StateSnapshot::from_json(&serde_json::to_string(&state).unwrap()).unwrap(),
            state
        );
        let (_, other, _) = artifacts(SHOWN, dialect);
        assert_ne!(plan.checksum(), other.checksum());
    }
}

#[test]
fn role_refusals_escape_roles_and_grant_targets_once_at_the_whole_message_boundary() {
    let name = RoleName::from(NAME);
    let role = Role {
        grants: [(
            GrantTarget::Schema(NAME.into()),
            [Permission::Maintain].into(),
        )]
        .into(),
        ..Default::default()
    };
    let errors = pbps_pg::roles::unsupported_permissions(160015, &name, &role);
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].to_string(),
        format!(
            "postgres: role `{SHOWN}`: `maintain` on `schema::{SHOWN}` needs PostgreSQL 17 or later, and this server is 16.15; on an older one the engine answers `unrecognized privilege type \"maintain\"` -- measured on 16.15 -- which is a statement that would fail after everything ordered before it had run. Remove it, or upgrade the server"
        )
    );
    assert!(pbps_pg::roles::unsupported_permissions(180006, &name, &role).is_empty());
    assert_eq!(
        pbps_pg::roles::refuse_missing(&name).to_string(),
        format!(
            "postgres: role `{SHOWN}` is declared here and the cluster does not have it. A PostgreSQL role is a cluster object, so pbps manages what a role is granted in this database and not whether the role exists (ADR-0010 section 3). Run it by hand, then plan again:\n\n    CREATE ROLE \"{SHOWN}\";"
        )
    );
}

#[test]
fn role_filename_keys_use_original_bytes_for_encoding_and_the_hashed_fallback() {
    let dir = std::path::Path::new("schema");
    let raw = declaration_file::role_path(dir, &RoleName::from(NAME)).unwrap();
    let lookalike = declaration_file::role_path(dir, &RoleName::from(SHOWN)).unwrap();
    assert_ne!(raw, lookalike);
    assert!(raw.to_str().unwrap().contains("%0A"));
    let long = RoleName::from("r\n".repeat(150));
    assert_eq!(
        declaration_file::role_path(dir, &long).unwrap(),
        dir.join(
            "roles/~pbps-41a07d171c06d3b0f1a4c21d938041319925cade4fbe598586c3bc22071d413f.yml"
        )
    );
}
