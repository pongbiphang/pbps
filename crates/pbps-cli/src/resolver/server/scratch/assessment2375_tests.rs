//! The #2375 contract, on server and Docker scratch, including refusals.

use super::*;
use pbps_db::resolver::capture::{CaptureError, Verdict};
use pbps_mssql::resolver::{capture, reconstruct::Reconstruction};
use std::collections::BTreeSet;

fn roots(names: &[&str]) -> capture::CaptureScope {
    capture::CaptureScope {
        retained: names.iter().map(|n| capture::object("dbo", n)).collect(),
        candidates: BTreeSet::new(),
    }
}

async fn statements(conn: &mut Conn, sql: &[&str]) {
    for sql in sql {
        conn.execute(sql).await.unwrap();
    }
}

async fn pair<F, Fut>(target: &str, scratch: &str, run: F)
where
    F: FnOnce(Conn, Conn) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (name, url) = create_database(scratch).await;
    let result = tokio::spawn(run(connection(target).await, connection(&url).await)).await;
    drop_database(scratch, &name).await;
    result.unwrap();
}

const TABLES: &[&str] = &[
    "CREATE TABLE dbo.t(i int NOT NULL PRIMARY KEY, v int NOT NULL UNIQUE, w int DEFAULT (7), CHECK (w <> 731923));",
    "CREATE TABLE dbo.child(i int REFERENCES dbo.t(i));",
    "CREATE VIEW dbo.bound WITH SCHEMABINDING AS SELECT i, v FROM dbo.t;",
];

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_unnamed_constraints_and_their_indexes_match_by_declaration() {
    super::reconstruct2374::both(|target, scratch| Box::pin(async move {
        pair(target, scratch, |mut target, mut scratch| async move {
            let key = capture_key();
            statements(&mut target, TABLES).await;
            statements(&mut scratch, TABLES).await;
            let scope = roots(&["t", "child", "bound"]);
            let a = capture::capture(&mut target, &scope, &key).await.unwrap();
            let b = capture::capture(&mut scratch, &scope, &key).await.unwrap();
            assert!(a.compare(&b).is_empty(), "{:?}", a.compare(&b));
            for kind in ["PRIMARY KEY", "UNIQUE", "CHECK", "DEFAULT", "FOREIGN KEY"] {
                assert!(a.objects().any(|id| id.class == format!("pbps-unnamed-{kind}")), "{kind}");
            }
            // Each independent shape change must still differ. Recreate the
            // two tables to avoid comparing a previous negative's side effects.
            for (tag, table, child) in [
                ("unique", "CREATE TABLE dbo.t(i int NOT NULL PRIMARY KEY, v int NOT NULL, w int DEFAULT (7), CHECK (w <> 731923), UNIQUE(w));", "CREATE TABLE dbo.child(i int REFERENCES dbo.t(i));"),
                ("check", "CREATE TABLE dbo.t(i int NOT NULL PRIMARY KEY, v int NOT NULL UNIQUE, w int DEFAULT (7), CHECK (w <> 731924));", "CREATE TABLE dbo.child(i int REFERENCES dbo.t(i));"),
                ("default", "CREATE TABLE dbo.t(i int NOT NULL PRIMARY KEY, v int NOT NULL UNIQUE, w int DEFAULT (8), CHECK (w <> 731923));", "CREATE TABLE dbo.child(i int REFERENCES dbo.t(i));"),
                ("foreign", TABLES[0], "CREATE TABLE dbo.child(i int REFERENCES dbo.t(v));"),
            ] {
                statements(&mut scratch, &["DROP VIEW dbo.bound; DROP TABLE dbo.child; DROP TABLE dbo.t;", table, child, TABLES[2]]).await;
                let b = capture::capture(&mut scratch, &scope, &key).await.unwrap();
                let differences = a.compare(&b);
                assert!(!differences.is_empty(), "{tag}");
                assert!(!format!("{differences:?}").contains("731923"));
                assert!(!serde_json::to_string(&differences).unwrap().contains("731923"));
            }
            target.execute("ALTER TABLE dbo.t ADD UNIQUE(v);").await.unwrap();
            let refusal = capture::capture(&mut target, &scope, &key).await.err().unwrap();
            assert!(matches!(&refusal, CaptureError::Coverage(c) if c.condition.contains("two identical unnamed")));
            let message = refusal.to_string();
            assert!(message.contains("UNIQUE") && message.contains("dbo.t") && message.contains("hint: name the constraint"), "{message}");
        }).await;
    })).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_unnamed_check_tokens_are_keyed_stable_and_source_free() {
    super::reconstruct2374::both(|target, scratch| Box::pin(async move {
        pair(target, scratch, |mut target, mut scratch| async move {
            let key = capture_key();
            let sql = "CREATE TABLE dbo.private_check(v int, CHECK (v <> 731923), CHECK (v <> 731924));";
            statements(&mut target, &[sql]).await;
            statements(&mut scratch, &[sql]).await;
            let scope = roots(&["private_check"]);
            let a = capture::capture(&mut target, &scope, &key).await.unwrap();
            let b = capture::capture(&mut scratch, &scope, &key).await.unwrap();
            let checks = |c: &capture::CapturedInputs| c.objects().filter(|id| id.class == "pbps-unnamed-CHECK").cloned().collect::<BTreeSet<_>>();
            let ids = checks(&a); assert_eq!(ids.len(), 2); assert_eq!(ids, checks(&b));
            let (_, differences) = capture::recapture(&mut target, &a).await.unwrap();
            assert!(differences.is_empty());
            target.execute("ALTER TABLE dbo.private_check ADD CHECK (v <> 731925);").await.unwrap();
            let extended = capture::capture(&mut target, &scope, &key).await.unwrap();
            assert!(ids.is_subset(&checks(&extended)), "adding a CHECK must not renumber the others");
            let other = capture_key_from(b"AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI=");
            let c = capture::capture(&mut target, &scope, &other).await.unwrap();
            assert!(ids.is_disjoint(&checks(&c)));
            for id in &ids {
                let text = format!("{id:?}");
                assert!(text.contains("an unnamed CHECK `#") && text.contains("dbo.private_check") && text.contains("CONSTRAINT <name> CHECK (...)"), "{text}");
                assert!(!text.contains(&id.name[0]), "only a short prefix is diagnostic");
            }
            let schema = Schema::default(); let ids_file = IdsFile::default(); let namespaces = BTreeSet::new();
            let recorded = capture::RecordedOwnership { schema: &schema, ids: &ids_file, namespaces: &namespaces };
            let (_, manifest) = capture::capture_qualified(&mut target, &scope, &key, &recorded).await.unwrap();
            let json = serde_json::to_string(&manifest).unwrap();
            assert_eq!(manifest, serde_json::from_str(&json).unwrap());
            for secret in ["731923", "731924", "731925"] { assert!(!json.contains(secret)); assert!(!format!("{ids:?}").contains(secret)); }
            target.execute("ALTER TABLE dbo.private_check ADD CHECK (v <> 731923);").await.unwrap();
            let refusal = capture::capture(&mut target, &scope, &key).await.err().unwrap();
            let message = refusal.to_string();
            assert!(message.contains("unnamed CHECK `#") && message.contains("hint: name the constraint"));
            assert!(!message.contains("731923") && !format!("{refusal:?}").contains("731923"));
        }).await;
    })).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_file_staged_unnamed_keys_match_and_different_keys_refuse() {
    super::reconstruct2374::both(|target, scratch| Box::pin(async move {
        pair(target, scratch, |mut target, mut scratch| async move {
            let key = capture_key();
            let script = "CREATE TABLE dbo.external_key(i int NOT NULL PRIMARY KEY, v int NOT NULL UNIQUE);";
            statements(&mut target, &[script]).await;
            pbps_mssql::resolver::externals::run(&mut scratch, script).await.unwrap();
            let roots = roots(&["external_key"]).retained;
            let staged = capture::compare_staged(&mut scratch, &mut target, &roots, &[], &key).await.unwrap();
            assert!(staged.compared().contains(&capture::object("dbo", "external_key")));
            // Render the same anonymous keys after disabling the unique
            // index. The template must read the engine's new name itself.
            let uq = target.query("SELECT name FROM sys.indexes WHERE object_id=OBJECT_ID(N'dbo.external_key') AND is_unique_constraint=1").await.unwrap()[0].try_get::<&str>("name").unwrap().unwrap().to_owned();
            target.execute(&format!("ALTER INDEX [{}] ON dbo.external_key DISABLE;", uq.replace(']', "]]"))).await.unwrap();
            target.execute("CREATE VIEW dbo.managed AS SELECT i FROM dbo.external_key;").await.unwrap();
            let mut managed = Schema::default();
            managed.modules.insert("dbo.managed".parse().unwrap(), pbps_model::Module { kind:pbps_model::ModuleKind::View, definition:"SELECT i FROM dbo.external_key".into(), description:None, options:Default::default() });
            let inputs = capture::required_externals(&mut target, &[&managed], &key).await.unwrap();
            scratch.execute("DROP TABLE dbo.external_key;").await.unwrap();
            let rendered = pbps_mssql::resolver::externals::render::missing(&inputs, &BTreeSet::new()).unwrap();
            for item in rendered { for statement in item.statements { scratch.execute(&statement).await.unwrap(); } }
            capture::compare_staged(&mut scratch, &mut target, &roots, &[&managed], &key).await.unwrap();
            statements(&mut scratch, &["DROP TABLE dbo.external_key; CREATE TABLE dbo.external_key(i int NOT NULL PRIMARY KEY, v int NOT NULL, w int UNIQUE);"]).await;
            let refused = capture::compare_staged(&mut scratch, &mut target, &roots, &[], &key).await;
            assert!(matches!(&refused, Err(capture::StageError::Refused(_))), "{refused:?}");
        }).await;
    })).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_assessment_distinguishes_bindings_creation_and_unreconstructed_candidates() {
    super::reconstruct2374::both(|target, scratch| Box::pin(async move {
        pair(target, scratch, |mut target, mut scratch| async move {
            let key = capture_key();
            let mut schema = Schema::default();
            for name in ["t", "other", "child"] {
                let mut table = pbps_model::Table::default();
                let mut column = pbps_model::Column::new("int".parse().unwrap()); column.nullable = false;
                table.columns.insert("i".into(), column);
                if name == "t" { table.columns.get_mut(&pbps_model::ColumnName::from("i")).unwrap().value_source = pbps_model::ValueSource::Default("7".into()); table.primary_key = Some(pbps_model::PrimaryKey { name: None, columns: vec!["i".into()], storage_parameters: Default::default() }); }
                schema.tables.insert(pbps_model::TableName::new("dbo",name), table);
            }
            for (name, kind, definition) in [
                ("bound", pbps_model::ModuleKind::View, "CREATE VIEW dbo.bound WITH SCHEMABINDING AS SELECT i FROM dbo.t;"),
                ("changed", pbps_model::ModuleKind::View, "CREATE VIEW dbo.changed WITH SCHEMABINDING AS SELECT i FROM dbo.t;"),
                ("new_view", pbps_model::ModuleKind::View, "CREATE VIEW dbo.new_view AS SELECT i FROM dbo.t;"),
                ("runtime", pbps_model::ModuleKind::Procedure, "CREATE PROCEDURE dbo.runtime AS SELECT i FROM ghost;"),
            ] {
                let id = if kind == pbps_model::ModuleKind::Procedure { format!("dbo.{name}()") } else { format!("dbo.{name}") };
                schema.modules.insert(id.parse().unwrap(), pbps_model::Module { kind, definition: definition.split_once(" AS ").map(|(_, body)| if kind == pbps_model::ModuleKind::Procedure { format!("AS {body}") } else { body.to_owned() }).unwrap(), description: None, options: pbps_model::ModuleOptions { schemabinding: name == "bound" || name == "changed", ..Default::default() } });
            }
            let ids = pbps_diff::resolve(&schema, &IdsFile::default(), &[], &pbps_diff::Context { operator:"assessment-test".into(), today:"2026-10-27".into() }).unwrap().ids;
            let empty = Schema::default(); let empty_ids = IdsFile::default();
            let plan = pbps_diff::resolver::ordinary(pbps_diff::Side { schema:&empty, ids:&empty_ids }, pbps_diff::Side { schema:&schema, ids:&ids }, &pbps_mssql::Mssql, &Hints::default()).unwrap();
            let order = Reconstruction::new(&pbps_mssql::Mssql, &plan, &ids, &Default::default()).unwrap();
            {
                let namespaces = BTreeSet::new();
                let recorded = capture::RecordedOwnership { schema:&schema, ids:&ids, namespaces:&namespaces };
                let absent = capture::capture_desired_for_plan(&mut scratch, &capture::managed_scope(&[&schema]), &Default::default(), &key, &recorded, &order).await;
                assert!(matches!(absent, Err(CaptureError::Incomplete)), "a compiled object missing from its read is not a binding verdict");
            }
            order.compile_steps(&mut scratch, 0..order.len()).await.unwrap();
            statements(&mut target, &[
                "CREATE TABLE dbo.t(i int NOT NULL DEFAULT (7) PRIMARY KEY); CREATE TABLE dbo.other(i int NOT NULL); CREATE TABLE dbo.child(i int NOT NULL); CREATE TABLE dbo.ghost(i int);",
                "CREATE VIEW dbo.bound WITH SCHEMABINDING AS SELECT i FROM dbo.t;",
                "CREATE VIEW dbo.changed WITH SCHEMABINDING AS SELECT i FROM dbo.other;",
                "CREATE PROCEDURE dbo.runtime AS SELECT i FROM ghost;",
            ]).await;
            for conn in [&mut target, &mut scratch] { statements(conn, &["ALTER TABLE dbo.child ADD FOREIGN KEY(i) REFERENCES dbo.t(i);"]).await; }
            let managed = capture::managed_scope(&[&schema]);
            let first = capture::capture(&mut scratch, &managed, &key).await.unwrap();
            let mut scope = first.runtime_scope();
            let opening = capture::capture(&mut target, &scope, &key).await.unwrap();
            scope.candidates.extend(opening.runtime_scope().candidates);
            let namespaces = BTreeSet::new();
            let recorded = capture::RecordedOwnership { schema:&schema, ids:&ids, namespaces:&namespaces };
            let compiled = capture::capture_desired_for_plan(&mut scratch, &scope, &Default::default(), &key, &recorded, &order).await.unwrap();
            let staged = capture::compare_staged(&mut scratch, &mut target, &BTreeSet::new(), &[], &key).await.unwrap();
            let assessment = compiled.assess_staged(&opening, &managed, &order, &staged);
            assert_eq!(assessment.surfaces[&capture::object("dbo","bound")], Verdict::Unaffected);
            assert_eq!(assessment.surfaces[&capture::object("dbo","changed")], Verdict::Rebuild);
            assert_eq!(assessment.surfaces[&capture::object("dbo","new_view")], Verdict::Created);
            assert!(matches!(&assessment.surfaces[&capture::object("dbo","runtime")], Verdict::Unresolved { condition } if condition.contains("ghost")));
            assert!(assessment.runtime_bound.contains(&capture::object("dbo","runtime")));
            let foreign = opening.objects().find(|id| id.class == "pbps-unnamed-FOREIGN KEY").unwrap();
            assert_eq!(assessment.surfaces[foreign], Verdict::Unaffected);
            let records = compiled.planning_records().unwrap();
            assert!(records.iter().any(|record| matches!(record.ownership, pbps_model::resolver::ObjectOwnership::Surface(pbps_model::resolver::Surface::Default(_)))), "the emitted named default retains its declared column ownership");
            let (_, qualified) = capture::capture_qualified(&mut target, &roots(&["t"]), &key, &recorded).await.unwrap();
            assert!(qualified.prerequisites().iter().any(|record| record.object.class == "pbps-unnamed-DEFAULT" && matches!(record.ownership, pbps_model::resolver::ObjectOwnership::Surface(pbps_model::resolver::Surface::Default(_)))), "an unnamed target default retains its declared column ownership");
            assert!(records.iter().any(|record| record.object.class == "pbps-unnamed-PRIMARY KEY" && matches!(record.ownership, pbps_model::resolver::ObjectOwnership::Surface(pbps_model::resolver::Surface::Table(_)))), "an unnamed primary key retains its declared table ownership");
            let manifest = compiled.seal_for_plan().unwrap();
            assert_eq!(manifest.adapter(), "mssql-capture-v4");
        }).await;
    })).await;
}
