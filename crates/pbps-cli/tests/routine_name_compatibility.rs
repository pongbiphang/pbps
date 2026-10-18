//! Captured on master d4752d3f before #2084 step 1b changed routine/module formatting.

use pbps_dialect::Dialect;
use pbps_model::{
    Change, ChangeSet, Declared, IdsFile, Module, ModuleId, ModuleKind, PlanBaseline, PlanOrigin,
    PlannedChange, RoutineId, SavedPlan, Schema, StateKind, StateSnapshot, TableName,
    state_checksum,
};

#[path = "../src/declaration_file.rs"]
mod declaration_file;

const NAME: &str = "name\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";
const SHOWN: &str = r"name\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";

fn artifacts(text: &str, dialect: &dyn Dialect) -> (Schema, SavedPlan, StateSnapshot) {
    let view = ModuleId::Named(TableName::new("app", format!("view{text}")));
    let name = TableName::new("app", format!("routine{text}"));
    let (routine, definition) = if dialect.name() == "postgres" {
        let arg = format!("app.\"{text}\"");
        (
            ModuleId::Routine(RoutineId::new(name, vec![arg.parse().unwrap()])),
            format!("(v {arg}) RETURNS integer LANGUAGE sql RETURN 1"),
        )
    } else {
        (
            ModuleId::Named(name),
            "(@v int) RETURNS int AS BEGIN RETURN @v END".into(),
        )
    };
    let schema = Schema {
        modules: [
            (
                view.clone(),
                Module {
                    kind: ModuleKind::View,
                    definition: "SELECT 1 AS id".into(),
                    description: None,
                },
            ),
            (
                routine.clone(),
                Module {
                    kind: ModuleKind::Function,
                    definition,
                    description: None,
                },
            ),
        ]
        .into(),
        ..Default::default()
    };
    let mut changes: Vec<_> = schema
        .modules
        .iter()
        .map(|(id, module)| {
            PlannedChange::new(Change::CreateModule {
                id: id.clone(),
                module: Box::new(module.clone()),
            })
        })
        .collect();
    if let ModuleId::Routine(routine) = &routine {
        changes.push(PlannedChange::new(Change::PublicExecution {
            routine: routine.clone(),
            access: pbps_model::PublicAccess::Revoked,
            origin: pbps_model::RoutineOrigin::Created,
        }));
    }
    let ids = IdsFile::default();
    let mut plan = SavedPlan::new(
        PlanOrigin::Database,
        dialect.name(),
        "2026-10-18T00:00:00Z",
        PlanBaseline {
            time_zone: None,
            description: "routine/module master fixture".into(),
            checksum: state_checksum(&Schema::default(), &ids),
            database_collation: None,
        },
        ChangeSet { changes },
        ids.clone(),
    );
    plan.module_deps.insert(view, [routine].into());
    let mut state = StateSnapshot::new(StateKind::Apply, schema.clone(), ids, "fixture");
    state.declared = Declared::from_schema(&schema);
    state.module_deps = plan.module_deps.clone();
    state.plan_checksum = Some(plan.checksum());
    (schema, plan, state)
}

#[test]
fn routine_and_module_artifacts_keep_master_bytes_and_filename_components() {
    for dialect in [
        &pbps_pg::Postgres::new() as &dyn Dialect,
        &pbps_mssql::Mssql,
    ] {
        let (schema, mut plan, mut state) = artifacts(NAME, dialect);
        let current_checksum = plan.checksum();
        let sql = plan
            .changes
            .changes
            .iter()
            .flat_map(|p| dialect.emit(&p.change, Default::default()).unwrap())
            .map(|s| s.sql)
            .collect::<Vec<_>>()
            .join("\n");
        let files = schema
            .modules
            .iter()
            .map(|(id, module)| {
                declaration_file::module_path(std::path::Path::new("schema"), id, module.kind)
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let yaml = schema
            .modules
            .iter()
            .map(|(id, module)| {
                pbps_load::fmt::render_module(
                    id,
                    module,
                    plan.module_deps.get(id).unwrap_or(&Default::default()),
                    false,
                )
            })
            .collect::<Vec<_>>()
            .join("\n---\n");
        let dir = std::env::temp_dir().join(format!(
            "pbps-routine-compatibility-{}",
            rand::random::<u64>()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
        let explain = std::process::Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&dir)
            .args(["explain", "--plan", "plan.json", "--format", "json"])
            .output()
            .unwrap();
        assert!(explain.status.success(), "{explain:?}");
        std::fs::remove_dir_all(dir).unwrap();
        // Keep the captured artifact bytes at their historical versions;
        // the CLI invocation above still exercises today's applicable plan.
        plan.version = 42;
        state.version = 25;
        state.plan_checksum = Some(plan.checksum());
        let outputs = [
            ("schema.json", serde_json::to_string(&schema).unwrap()),
            ("plan.json", serde_json::to_string(&plan).unwrap()),
            ("state.json", serde_json::to_string(&state).unwrap()),
            ("state.sha256", state_checksum(&schema, &state.ids)),
            ("plan.sha256", plan.checksum()),
            ("statements.sql", sql),
            ("filenames.txt", files),
            ("declarations.yml", yaml),
            ("explain.json", String::from_utf8(explain.stdout).unwrap()),
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/name-compatibility/routines")
            .join(dialect.name());
        for (name, actual) in outputs {
            let expected = std::fs::read_to_string(root.join(name)).unwrap();
            let expected = if name == "explain.json" {
                expected.replace(&plan.checksum(), &current_checksum)
            } else {
                expected
            };
            assert_eq!(actual, expected, "{name}");
        }
        assert_eq!(
            serde_json::from_str::<SavedPlan>(&serde_json::to_string(&plan).unwrap()).unwrap(),
            plan
        );
        assert_eq!(
            StateSnapshot::from_json(&serde_json::to_string(&state).unwrap()).unwrap(),
            state
        );
        let (_, mut ordinary, _) = artifacts("ordinary", dialect);
        ordinary.version = plan.version;
        assert_ne!(ordinary.checksum(), plan.checksum());
    }
}

#[test]
fn explain_shows_each_routine_name_and_argument_once() {
    let (_, plan, _) = artifacts(NAME, &pbps_pg::Postgres::new());
    let dir = std::env::temp_dir().join(format!("pbps-routine-explain-{}", rand::random::<u64>()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pbps"))
        .current_dir(&dir)
        .args(["explain", "--plan", "plan.json", "--format", "text"])
        .output()
        .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let signature = format!("app.routine{SHOWN}(app.\"{SHOWN}\")");
    for expected in [
        format!("\n  app.view{SHOWN}\n"),
        format!("\n  {signature}\n"),
        format!("    - revoke execute on {signature} from PUBLIC\n"),
    ] {
        assert!(
            text.contains(&expected),
            "expected {expected:?} in {text:?}"
        );
    }
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "{text:?}"
    );
}

#[test]
fn a_routine_refusal_is_escaped_exactly_once() {
    for (part, shown) in [(NAME, SHOWN), ("名前_ünï", "名前_ünï")] {
        let id: ModuleId = format!("app.{part}(app.\"{part}\")").parse().unwrap();
        let change = Change::CreateModule {
            id,
            module: Box::new(Module {
                kind: ModuleKind::Function,
                description: None,
                definition: String::new(),
            }),
        };
        let error = pbps_pg::Postgres::new()
            .emit(&change, Default::default())
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("postgres: module `app.{shown}(app.\"{shown}\")` has an empty definition")
        );
    }
}
