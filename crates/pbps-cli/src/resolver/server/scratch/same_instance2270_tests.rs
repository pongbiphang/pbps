//! The live marker, rather than URL spelling, decides where file consent applies.

use super::*;
use pbps_model::resolver::{OutsideWrites, SameInstance};

fn policy(same_instance: SameInstance) -> Provenance {
    Provenance {
        same_instance,
        ..provenance()
    }
}

async fn file_case(file: &'static str) {
    let server = server("PBPS_TEST_DB");
    let (target_name, target) = create_database(&server).await;
    let (scratch_name, scratch) = create_database(&server).await;
    let result = tokio::spawn(async move {
        for outside_writes in [OutsideWrites::Refused, OutsideWrites::Allowed] {
            let p = Provenance { outside_writes, ..policy(SameInstance::Refused) };
            let refused = Box::pin(refusal_policy_inner(&target, &scratch, Some(file), &p)).await;
            assert!(matches!(&refused, ProduceError::Run(Error::Binding(reason)) if reason.contains("same_instance: allow")), "{refused}");
        }
        let missing = Box::pin(refusal_policy_inner(&target, &scratch, None, &policy(SameInstance::NotApplicable))).await;
        assert!(matches!(&missing, ProduceError::Run(Error::Unusable(reason)) if reason.contains("same_instance runtime")), "{missing}");
        let allowed = Box::pin(outcome_policy_inner(&target, &scratch, Some(file), &policy(SameInstance::Allowed))).await;
        if file.is_empty() {
            assert!(allowed.is_ok(), "{}", why(&allowed));
        } else {
            assert!(matches!(&allowed, Err(ProduceError::Run(Error::Externals(reasons))) if reasons.iter().any(|r| r.contains("file-ran-2270"))), "{}", why(&allowed));
        }
        assert!(sql::inventory::read(&mut connection(&scratch).await, None).await.unwrap().is_empty());
    }).await;
    drop_database(&server, &scratch_name).await;
    drop_database(&server, &target_name).await;
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn same_instance_file_requires_explicit_allow() {
    file_case("THROW 51000,'file-ran-2270',1;").await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn empty_file_still_requires_same_instance_allow() {
    file_case("").await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn target_database_is_always_refused() {
    let server = server("PBPS_TEST_DB");
    let (name, target) = create_database(&server).await;
    let result = tokio::spawn(async move {
        for choice in [SameInstance::Refused, SameInstance::Allowed] {
            for file in [None, Some(""), Some("THROW 51000,'must-never-run-2270',1;")] {
                let outcome = Box::pin(refusal_policy_inner(&target, &target, file, &policy(choice))).await;
                assert!(matches!(&outcome, ProduceError::Run(Error::Unusable(reason)) if reason.contains("target database")), "{outcome}");
            }
        }
        let missing = Box::pin(refusal_policy_inner(&target, &target, Some(""), &policy(SameInstance::NotApplicable))).await;
        assert!(matches!(missing, ProduceError::Run(Error::Unusable(_))), "{missing}");
    }).await;
    drop_database(&server, &name).await;
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn same_instance_allow_keeps_containment_and_comparison() {
    let server = server("PBPS_TEST_DB");
    let (target_name, target) = create_database(&server).await;
    let (scratch_name, scratch) = create_database(&server).await;
    let target_database = target_name.clone();
    let result = tokio::spawn(async move {
        let cases = [
            ("CREATE TABLE dbo.absent2270(i int);".to_owned(), "absent2270"),
            (format!("CREATE TABLE [{target_database}].dbo.escape2270(i int);"), "access"),
            ("REVERT;".into(), "revert"),
            ("CREATE LOGIN must_never_exist2270 WITH PASSWORD='NotUsed2270!';".into(), "permission"),
        ];
        for (file, expected) in cases {
            let outcome = Box::pin(refusal_policy_inner(&target, &scratch, Some(&file), &policy(SameInstance::Allowed))).await;
            assert!(matches!(&outcome, ProduceError::Run(Error::Externals(reasons)) if reasons.iter().any(|r| r.to_lowercase().contains(expected))), "{outcome}");
            assert!(sql::inventory::read(&mut connection(&scratch).await, None).await.unwrap().is_empty());
        }
        assert!(connection(&target).await.query("SELECT name FROM sys.objects WHERE name=N'escape2270';").await.unwrap().is_empty());
    }).await;
    drop_database(&server, &scratch_name).await;
    drop_database(&server, &target_name).await;
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn another_instance_does_not_require_file_consent() {
    let target_server = server("PBPS_TEST_DB");
    let scratch_server = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (name, target) = create_database(&target_server).await;
    let result = tokio::spawn(async move {
        for choice in [SameInstance::Refused, SameInstance::Allowed] {
            let outcome = Box::pin(refusal_policy_inner(&target, &scratch_server, Some("THROW 51000,'separate-file-2270',1;"), &policy(choice))).await;
            assert!(matches!(&outcome, ProduceError::Run(Error::Externals(reasons)) if reasons.iter().any(|r| r.contains("separate-file-2270"))), "{outcome}");
        }
    }).await;
    drop_database(&target_server, &name).await;
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn same_instance_automatic_externals_use_catalog_rendering_only() {
    for default in ["", " DEFAULT (42)"] {
        automatic_externals(default).await;
    }
}

async fn automatic_externals(default: &'static str) {
    use pbps_mssql::resolver::capture;
    let server = server("PBPS_TEST_DB");
    let (target_name, target_url) = create_database(&server).await;
    let (scratch_name, scratch_url) = create_database(&server).await;
    let result = tokio::spawn(async move {
        let mut target = connection(&target_url).await;
        target.execute(&format!("CREATE TABLE dbo.external2270(i int{default});")).await.unwrap();
        target.execute("CREATE VIEW dbo.managed2270 AS SELECT i FROM dbo.external2270;").await.unwrap();
        let mut schema = Schema::default();
        schema.modules.insert("dbo.managed2270".parse().unwrap(), pbps_model::Module {
            kind: pbps_model::ModuleKind::View, description: None,
            definition: "CREATE VIEW dbo.managed2270 AS SELECT i FROM dbo.external2270;".into(),
            options: Default::default(),
        });
        let ids = IdsFile::default();
        let side = pbps_diff::Side { schema: &schema, ids: &ids };
        let bootstrap = pbps_diff::resolver::ordinary(side, side, &pbps_mssql::Mssql, &Hints::default()).unwrap();
        let binding = BindingRequest { dependencies: &Default::default(), bootstrap: &bootstrap, base: &schema, desired: &schema };
        let request = super::super::super::ScopeRequest { schemas: vec!["dbo".into()], ..Default::default() };
        let tokens = super::super::Tokens::generate();
        let p = policy(SameInstance::Refused);
        let identity = sql::cluster_identity(&mut target).await.unwrap();
        let mut admin = connection(&scratch_url).await;
        let (provisioning, mut state) = super::super::engine::scratch(
            Driver::Mssql, &mut target, &mut admin, &tokens, &identity, &scratch_url,
            &request, &Standard::Mssql, None, &p,
        ).await.unwrap();
        assert_eq!(provisioning, super::super::Provisioning::Supplied);
        let (catalog, authorization) = crate::resolver::scope::read_target(
            &mut target, Driver::Mssql, &request.schemas, &[], &request.schemas, &[],
        ).await.unwrap();
        let prepared = state.prepare(provisioning, &mut admin, &mut target, &catalog, &authorization, &request.schemas, &binding, &[], Some(&crate::resolver::server::scratch::mssql_live_tests::capture_key())).await;
        let checked = async {
            let super::super::engine::Prepared::Mssql(mut prepared) = prepared? else { panic!("wrong engine"); };
            assert!(prepared.staged.compared().contains(&capture::object("dbo", "external2270")));
            // The target's authored default is deliberately absent: the renderer
            // sends catalog structure, rather than replaying source-bearing DDL.
            let rows = prepared.compile.query("SELECT default_object_id FROM sys.columns WHERE object_id=OBJECT_ID(N'dbo.external2270');").await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].try_get::<i32>("default_object_id").unwrap(), Some(0));
            assert!(prepared.compile.execute("REVERT;").await.is_err());
            Ok::<(), Error>(())
        }.await;
        state.release(&mut admin).await.unwrap();
        checked.unwrap();
        assert!(sql::inventory::read(&mut connection(&scratch_url).await, None).await.unwrap().is_empty());
    }).await;
    drop_database(&server, &scratch_name).await;
    drop_database(&server, &target_name).await;
    result.unwrap();
}
