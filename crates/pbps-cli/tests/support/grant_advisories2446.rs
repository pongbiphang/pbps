use super::{
    ClusterRoles, Demo, ONE_COLUMN, OwnDatabase, TWO_COLUMNS, approved_apply, code, json_output,
    on_server, scalar, stderr, stdout, succeeds, text_of,
};
use serde_json::Value;

const ID: &str = "plan.unmanaged-grant";

fn plan(d: &Demo, connection: &str) -> Value {
    let path = d.dir.join("advisory.plan.json");
    let out = succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--format",
        "json",
        "--out",
        path.to_str().unwrap(),
    ]));
    assert!(!stderr(&out).contains(ID), "{}", stderr(&out));
    json_output(out)
}

fn advisories(report: &Value) -> Vec<Value> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["id"] == ID)
        .cloned()
        .collect()
}

fn check_named(report: &Value, role: &str, column: bool, option: bool) -> Vec<Value> {
    let found = advisories(report);
    assert_eq!(
        found.len(),
        usize::from(column) + usize::from(option),
        "{report}"
    );
    for f in &found {
        assert_eq!(f["severity"], "warning");
        let message = f["message"].as_str().unwrap();
        for name in [role, "app.t", "unmanaged"] {
            assert!(message.contains(name), "{f}");
        }
    }
    if column {
        assert!(
            found.iter().any(|f| {
                let text = f["message"].as_str().unwrap();
                text.contains("column-level grant") && text.contains("`id`")
            }),
            "{report}"
        );
    }
    if option {
        assert!(
            found
                .iter()
                .any(|f| f["message"].as_str().unwrap().contains("WITH GRANT OPTION")),
            "{report}"
        );
    }
    found
}

fn seed(connection: &str, slug: &str, role: Option<&str>) -> Demo {
    let d = Demo::new(slug);
    d.table(ONE_COLUMN);
    if let Some(role) = role {
        std::fs::write(
            d.dir.join("schema/role.yml"),
            format!("role: {role}\ngrants: {{}}\n"),
        )
        .unwrap();
    }
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "before unsupported ACL additions",
    ]));
    d
}

fn acl(connection: &str) -> String {
    text_of(
        connection,
        "SELECT json_build_array(\
         (SELECT relacl::text FROM pg_class WHERE oid = 'app.t'::regclass), \
         (SELECT attacl::text FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname = 'id'), \
         (SELECT count(*) FROM pg_default_acl))::text",
    )
}

pub fn check(admin: &str) {
    let role = format!("pbps_adv2446_{}", std::process::id());
    let _roles = ClusterRoles {
        server: admin.into(),
        names: vec![role.clone()],
    };
    on_server(admin, &format!("CREATE ROLE {role}"));
    for (shape, column, option) in [
        ("column", true, false),
        ("option", false, true),
        ("both", true, true),
    ] {
        let slug = format!("adv2446-{shape}");
        let own = OwnDatabase::new(admin, &slug);
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.t(id bigint NOT NULL, CONSTRAINT pk_t PRIMARY KEY(id))",
        );
        let d = seed(connection, &slug, None);
        let before = plan(&d, connection);
        assert!(advisories(&before).is_empty());
        let path = d.dir.join("advisory.plan.json");
        let before: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
        on_server(
            connection,
            &format!(
                "CREATE TABLE app.outside(id integer); \
             GRANT SELECT(id) ON app.outside TO {role}; \
             GRANT SELECT ON app.outside TO {role} WITH GRANT OPTION; \
             GRANT SELECT(id) ON app.t TO postgres; \
             GRANT SELECT(id) ON app.t TO PUBLIC; \
             ALTER DEFAULT PRIVILEGES IN SCHEMA app GRANT SELECT ON TABLES TO {role}"
            ),
        );
        if column {
            on_server(connection, &format!("GRANT SELECT(id) ON app.t TO {role}"));
        }
        if option {
            on_server(
                connection,
                &format!("GRANT SELECT ON app.t TO {role} WITH GRANT OPTION"),
            );
        }
        let expected_acl = acl(connection);
        let reported = plan(&d, connection);
        let expected = check_named(&reported, &role, column, option);
        assert_eq!(reported["data"]["changes"], 0);
        assert_eq!(
            reported["findings"].as_array().unwrap(),
            &expected,
            "{reported}"
        );
        let mut after: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        // Independently created plans seal their own timestamps. Hold only
        // that clock field fixed to compare the complete checksum input.
        after.created_at = before.created_at.clone();
        assert_eq!(
            after.wire_fingerprint(),
            before.wire_fingerprint(),
            "the advisory cannot change a plan checksum"
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains(ID) && !saved.contains(&role), "{saved}");
        assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
        let human = succeeds(d.run(&["plan", "--db", connection]));
        for finding in &expected {
            assert_eq!(
                stderr(&human)
                    .matches(finding["message"].as_str().unwrap())
                    .count(),
                1
            );
        }
        assert!(stdout(&human).contains("No changes."));
        succeeds(approved_apply(&d, connection, &path, &[]));
        let next = plan(&d, connection);
        assert_eq!(next["data"]["changes"], 0);
        assert_eq!(next["findings"].as_array().unwrap(), &expected);
        succeeds(d.run(&[
            "baseline",
            "--db",
            connection,
            "--reason",
            "retain platform-managed role privileges",
        ]));
        d.table(TWO_COLUMNS);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let changed = plan(&d, connection);
        assert_eq!(changed["data"]["changes"], 1);
        assert_eq!(advisories(&changed), expected);
        succeeds(approved_apply(&d, connection, &path, &[]));
        succeeds(d.run(&["verify", "--db", connection]));
        let next = plan(&d, connection);
        assert_eq!(next["data"]["changes"], 0);
        assert_eq!(next["findings"].as_array().unwrap(), &expected);
        assert_eq!(acl(connection), expected_acl);
        let state = text_of(
            connection,
            "SELECT state_json::text FROM public.__pbps_state ORDER BY id DESC LIMIT 1",
        );
        assert!(!state.contains(ID) && !state.contains(&role), "{state}");
        let ids: pbps_model::IdsFile =
            serde_json::from_slice(&std::fs::read(d.dir.join("schema.ids.json")).unwrap()).unwrap();
        assert!(ids.roles.is_empty());

        // Registration uses the ordinary policy settings and suppression path,
        // without putting findings onto changes in the saved artifact.
        let config = d.dir.join("pbps.yml");
        std::fs::write(
            &config,
            format!("dialect: postgres\npolicies:\n  rules:\n    {ID}: off\n"),
        )
        .unwrap();
        assert!(advisories(&plan(&d, connection)).is_empty());
        std::fs::write(&config, format!("dialect: postgres\npolicies:\n  suppress:\n    - rule: {ID}\n      reason: platform-owned ACL\n")).unwrap();
        assert!(advisories(&plan(&d, connection)).is_empty());
        std::fs::write(
            &config,
            format!("dialect: postgres\npolicies:\n  rules:\n    {ID}: error\n"),
        )
        .unwrap();
        std::fs::remove_file(&path).unwrap();
        let out = d.run(&[
            "plan",
            "--db",
            connection,
            "--format",
            "json",
            "--out",
            path.to_str().unwrap(),
        ]);
        assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
        let report = json_output(out);
        assert_eq!(report["result"], "findings");
        assert!(advisories(&report).iter().all(|f| f["severity"] == "error"));
        assert!(!path.exists());
    }
    managed_and_outside_controls(admin, &role);
}

fn managed_and_outside_controls(admin: &str, role: &str) {
    let own = OwnDatabase::new(admin, "adv2446-controls");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE app.t(id bigint NOT NULL, CONSTRAINT pk_t PRIMARY KEY(id)); CREATE TABLE app.outside(id integer)",
    );
    let d = seed(connection, "adv2446-controls", Some(role));
    // Neither an owner's ACL, PUBLIC, default ACL context nor a grant on an
    // unmanaged object acquires a role or target identity through the advisory.
    on_server(
        connection,
        &format!(
            "GRANT SELECT(id) ON app.outside TO {role}; \
         GRANT SELECT ON app.outside TO {role} WITH GRANT OPTION; \
         GRANT SELECT(id) ON app.t TO postgres; \
         GRANT SELECT(id) ON app.t TO PUBLIC; \
         ALTER DEFAULT PRIVILEGES IN SCHEMA app GRANT SELECT ON TABLES TO {role}"
        ),
    );
    assert!(advisories(&plan(&d, connection)).is_empty());
    succeeds(d.run(&["verify", "--db", connection]));
    // A managed grantee keeps the fatal refusal, for each shape and both.
    for (column, option) in [(true, false), (false, true), (true, true)] {
        if column {
            on_server(connection, &format!("GRANT SELECT(id) ON app.t TO {role}"));
        }
        if option {
            on_server(
                connection,
                &format!("GRANT SELECT ON app.t TO {role} WITH GRANT OPTION"),
            );
        }
        let out = d.run(&["plan", "--db", connection]);
        assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
        let why = stderr(&out);
        assert!(why.contains(role) && why.contains("app.t"), "{why}");
        if column {
            assert!(
                why.contains("column-level grant") && why.contains("`id`"),
                "{why}"
            );
        }
        if option {
            assert!(why.contains("WITH GRANT OPTION"), "{why}");
        }
        on_server(
            connection,
            &format!("REVOKE SELECT(id) ON app.t FROM {role}; REVOKE SELECT ON app.t FROM {role}"),
        );
    }
    std::fs::write(
        d.dir.join("schema/role.yml"),
        format!("role: {role}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    let changed = plan(&d, connection);
    assert!(advisories(&changed).is_empty());
    let path = d.dir.join("advisory.plan.json");
    succeeds(approved_apply(&d, connection, &path, &[]));
    let next = plan(&d, connection);
    assert_eq!(next["data"]["changes"], 0);
    assert!(advisories(&next).is_empty());
    succeeds(d.run(&["verify", "--db", connection]));
    assert_eq!(
        scalar(
            connection,
            &format!(
                "SELECT CASE WHEN has_table_privilege('{role}', 'app.t', 'SELECT') THEN 1 ELSE 0 END::int8"
            )
        ),
        1
    );
}
