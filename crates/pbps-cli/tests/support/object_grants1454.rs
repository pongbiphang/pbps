use super::*;

#[test]
#[ignore = "needs PostgreSQL 18; set PBPS_TEST_PG_DB"]
fn an_object_grant_moves_the_sealed_authorization_on_pg18() {
    check(&server());
}

#[test]
#[ignore = "needs PostgreSQL 16; set PBPS_TEST_PG_OLD_DB"]
fn an_object_grant_moves_the_sealed_authorization_on_pg16() {
    check(&std::env::var("PBPS_TEST_PG_OLD_DB").unwrap());
}

fn check(server: &str) {
    let role = format!(
        "pbps_g1454_{}_{:08x}",
        std::process::id(),
        rand::random::<u32>()
    );
    // The databases are dropped before their cluster role, including after
    // a refusal: a failed assertion must not leave authorization behind.
    let _roles = ClusterRoles {
        server: server.to_owned(),
        names: vec![role.clone()],
    };
    on_server(server, &format!("CREATE ROLE {role} NOLOGIN"));
    let before = format!("role: {role}\ngrants:\n  schema::app: [usage]\n");
    let desired = format!("{before}  app.granted1454: [select]\n");
    let sealed = Sealed::revised(
        server,
        "grants1454",
        Revision {
            bootstrapped: &[
                (
                    "app.granted1454.yml",
                    "table: app.granted1454\ncolumns:\n  id: {type: integer}\n",
                ),
                ("roles/grants1454.yml", &before),
            ],
            files: &[("roles/grants1454.yml", &desired)],
            ..Revision::default()
        },
    );
    let plan: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&sealed.artifact).unwrap()).unwrap();
    let target = pbps_model::ObjectName::new("app", "granted1454");
    assert!(plan.changes.changes.iter().any(|p| matches!(
        &p.change,
        pbps_model::Change::Grant {
            target: pbps_model::GrantTarget::Object(object), ..
        } if object == &target
    )));
    // A schema grant already moves the old condition; it cannot stand in
    // for evidence about this plan's object grant.
    assert!(!plan.changes.changes.iter().any(|p| matches!(
        &p.change,
        pbps_model::Change::Grant {
            target: pbps_model::GrantTarget::Schema(_),
            ..
        } | pbps_model::Change::Revoke {
            target: pbps_model::GrantTarget::Schema(_),
            ..
        }
    )));
    let pbps_model::resolver::PlanAnalysis::Resolved(evidence) = &plan.analysis else {
        panic!("the connected plan must carry resolver evidence");
    };
    assert_ne!(
        evidence.authorization().before,
        evidence.authorization().after,
        "an object grant must move its sealed authorization postcondition",
    );
    // Inspect the specific table question: the fixture also changes PUBLIC
    // execution on rebuilt routines, which cannot witness this table grant.
    let wire = serde_json::to_value(&plan).unwrap();
    let index = plan
        .changes
        .changes
        .iter()
        .position(|p| {
            matches!(
                &p.change,
                pbps_model::Change::Grant {
                    target: pbps_model::GrantTarget::Object(object), ..
                } if object == &target
            )
        })
        .unwrap();
    let question = wire["analysis"]["evidence"]["authorization"]["object_grants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["change"].as_u64() == Some(index as u64))
        .unwrap();
    assert_ne!(
        question["before"]["fingerprint"],
        question["after"]["fingerprint"]
    );
    successful(sealed.apply(&sealed.artifact));

    // Invoke the resolver closing guard directly, independently of the
    // ordinary schema/grant guard. The fresh read must see this transaction's
    // own revoke; a rollback leaves the successful apply intact.
    let desired = pbps_load::load_schema_dir(&sealed.d.dir.join("schema"))
        .unwrap()
        .schema;
    let key = pbps_db::fingerprint::EnvironmentFingerprintKey::from_file(&sealed.d.dir.join("key"))
        .unwrap();
    let keyed = pbps_cli::resolver::apply::Keyed::check(&plan, Ok(key), "grant closing")
        .unwrap()
        .unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, sealed.connection())
            .await
            .unwrap();
        let guard = keyed.open(&mut conn).await.unwrap();
        conn.execute("BEGIN").await.unwrap();
        guard
            .before_recording(&mut conn, &desired, &plan.ids)
            .await
            .unwrap();
        conn.execute(&format!("REVOKE SELECT ON app.granted1454 FROM {role}"))
            .await
            .unwrap();
        let result = guard.before_recording(&mut conn, &desired, &plan.ids).await;
        conn.execute("ROLLBACK").await.unwrap();
        let error =
            result.expect_err("resolver closing must independently detect the removed grant");
        assert!(
            error.to_string().contains("object grant postcondition"),
            "{error:#}"
        );
    });
}
