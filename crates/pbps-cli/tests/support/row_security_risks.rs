//! Approval is enforced before connecting for standing-policy delivery.
use super::*;
use pbps_model::{
    Change, ChangeSet, IdsFile, PlanBaseline, PlanOrigin, PlannedChange, RowPolicy, RowSecurity,
    RowSecurityOrigin, SavedPlan, TableName, Uid,
};

fn fixture(changes: Vec<Change>) -> SavedPlan {
    let mut ids = IdsFile::default();
    ids.tables
        .insert("t_117300".parse().unwrap(), TableName::new("app", "docs"));
    let mut changes = ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    };
    let risks = pbps_diff::order::generated::risks(&changes, &ids, &pbps_pg::Postgres::new());
    for (step, risks) in changes.changes.iter_mut().zip(risks) {
        step.risks = risks;
    }
    SavedPlan::new(
        PlanOrigin::Database,
        "postgres",
        "2026-10-22T00:00:00Z",
        PlanBaseline {
            description: "as queried".into(),
            checksum: "0".repeat(64),
            time_zone: None,
            database_collation: None,
        },
        changes,
        ids,
    )
}

#[test]
fn row_security_apply_requires_approval_before_connecting() {
    let d = Demo::new("row-security-gate1173");
    std::fs::write(d.dir.join("pbps.yml"), "dialect: postgres\n").unwrap();
    let table = TableName::new("app", "docs");
    let uid: Uid = "t_117300".parse().unwrap();
    let policy = RowPolicy {
        using: Some("id > 0".into()),
        ..Default::default()
    };
    let create = |restrictive| Change::CreatePolicy {
        table: table.clone(),
        name: "p".into(),
        policy: RowPolicy {
            restrictive,
            ..policy.clone()
        },
        origin: RowSecurityOrigin::Standing,
    };
    let drop = |restrictive| Change::DropPolicy {
        table: table.clone(),
        name: "p".into(),
        policy: RowPolicy {
            restrictive,
            ..policy.clone()
        },
    };
    let flags = Change::SetRowSecurity {
        uid,
        table: table.clone(),
        from: RowSecurity {
            enabled: true,
            forced: true,
        },
        to: RowSecurity::default(),
        origin: RowSecurityOrigin::Standing,
    };
    for (changes, required) in [
        (vec![create(false)], "row-security"),
        (vec![drop(true)], "row-security"),
        (vec![flags], "row-security"),
        (vec![drop(false), create(true)], "revoke,row-security"),
        (vec![drop(true), create(false)], "revoke,row-security"),
    ] {
        let plan = fixture(changes);
        let path = d.dir.join("standing.json");
        std::fs::write(&path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
        let checksum = plan.wire_fingerprint();
        let invoke = |allow: Option<&str>| {
            let mut args = vec![
                "apply",
                "--db",
                "host=127.0.0.1 port=1 user=u password=p dbname=nowhere",
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &checksum,
            ];
            if let Some(allow) = allow {
                args.extend(["--allow", allow]);
            }
            d.run(&args)
        };
        for allow in [None, Some("grant-widen")] {
            let out = invoke(allow);
            let err = stderr(&out);
            assert_eq!(code(&out), 1, "{err}");
            assert!(err.contains("risks that were not approved"), "{err}");
            assert!(err.contains("row-security"), "{err}");
            assert!(!err.contains("SQL delivery arrives"), "{err}");
        }
        if required.contains(',') {
            for (allow, missing) in [("revoke", "row-security"), ("row-security", "revoke")] {
                let out = invoke(Some(allow));
                let err = stderr(&out);
                assert!(
                    err.contains(&format!("risks that were not approved: {missing}.")),
                    "{err}"
                );
            }
        }
        let out = invoke(Some(required));
        let err = stderr(&out);
        assert_eq!(code(&out), 1, "{err}");
        assert!(err.contains("connect") || err.contains("refused"), "{err}");
        assert!(!err.contains("risks that were not approved"), "{err}");
    }
}

#[test]
fn standing_policy_plans_require_the_current_plan_version() {
    let d = Demo::new("row-security-version1173");
    std::fs::write(d.dir.join("pbps.yml"), "dialect: postgres\n").unwrap();
    let change = Change::DropPolicy {
        table: TableName::new("app", "docs"),
        name: "p".into(),
        policy: RowPolicy::default(),
    };
    for version in [
        pbps_model::plan::CURRENT_VERSION - 1,
        pbps_model::plan::CURRENT_VERSION + 1,
    ] {
        let mut plan = fixture(vec![change.clone()]);
        plan.version = version;
        let path = d.dir.join("wrong-version.json");
        std::fs::write(&path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
        let out = d.run(&[
            "apply",
            "--db",
            "host=127.0.0.1 port=1 user=u password=p dbname=nowhere",
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &plan.wire_fingerprint(),
            "--allow",
            "revoke,row-security",
        ]);
        let err = stderr(&out);
        assert_eq!(code(&out), 1, "{err}");
        assert!(err.contains(&format!("version {version} plan")), "{err}");
        assert!(
            err.contains(&format!(
                "understands version {}",
                pbps_model::plan::CURRENT_VERSION
            )),
            "{err}"
        );
    }
}
