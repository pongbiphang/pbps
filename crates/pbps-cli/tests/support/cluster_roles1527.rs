//! Removing a PG role declaration does not drop its cluster identity or its
//! membership edges, so it cannot trigger resolver binding work (DEC-1527.1).
use super::*;

fn successful(output: Output) -> Output {
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    output
}

pub(super) fn check(server: &str) {
    let token = format!("{}_{:08x}", std::process::id(), rand::random::<u32>());
    let bridge = format!("r1527_bridge_{token}");
    let reader = format!("r1527_reader_{token}");
    let deployer = format!("r1527_deployer_{token}");
    let _roles = ClusterRoles {
        server: server.into(),
        names: vec![bridge.clone(), reader.clone(), deployer.clone()],
    };
    on_server(
        server,
        &format!(
            "CREATE ROLE {bridge}; CREATE ROLE {reader}; \
         CREATE ROLE {deployer} LOGIN INHERIT PASSWORD 'trigger-test'; \
         GRANT {reader} TO {bridge}; GRANT {bridge} TO {deployer}"
        ),
    );
    let own = OwnDatabase::new(server, "roles1527");
    let admin = own.connection();
    on_server(admin, "CREATE SCHEMA app");
    let d = Demo::new(&format!("roles1527-{token}"));
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/view.yml"),
        "view: app.v\ndefinition: SELECT id FROM app.t\n",
    )
    .unwrap();
    let role_file = d.dir.join("schema/bridge.yml");
    std::fs::write(&role_file, format!("role: {bridge}\n")).unwrap();
    successful(d.run(&["plan", "--no-dev"]));
    d.commit();
    successful(d.run(&["bootstrap", "--db", admin]));
    on_server(
        admin,
        &format!(
            "GRANT USAGE ON SCHEMA app TO {reader}; \
         GRANT SELECT ON ALL TABLES IN SCHEMA app TO {reader}; \
         GRANT SELECT ON public.__pbps_state TO {deployer}"
        ),
    );
    let target = as_role(admin, &deployer);
    let usable = || {
        scalar(
            &target,
            "SELECT has_schema_privilege(current_user, 'app', 'USAGE')::int::bigint",
        )
    };
    let chain = || {
        scalar(
            admin,
            &format!(
                "SELECT count(*) FROM pg_auth_members \
         WHERE (roleid='{reader}'::regrole AND member='{bridge}'::regrole) \
         OR (roleid='{bridge}'::regrole AND member='{deployer}'::regrole)"
            ),
        )
    };
    assert_eq!(chain(), 2);
    assert_eq!(usable(), 1);
    let kept = scalar(admin, "SELECT 'app.v'::regclass::oid::bigint");
    std::fs::remove_file(&role_file).unwrap();
    successful(d.run(&[
        "drop-role",
        &bridge,
        "--reason",
        "stop managing this external role",
    ]));
    std::fs::write(
        d.dir.join("pbps.yml"),
        concat!(
            "dialect: postgres\nenvironments:\n  prod:\n    url_env: PBPS_TARGET_1527\n",
            "    resolve_with: scratch\n    fingerprint_key_file: nonexistent.key\n",
            "resolvers:\n  scratch:\n    kind: server\n    url_env: PBPS_UNSET_SCRATCH_1527\n",
        ),
    )
    .unwrap();
    let output = successful(
        Command::new(BIN)
            .arg("--project")
            .arg(&d.dir)
            .args(["plan", "--env", "prod", "--no-rehearse", "--format", "json"])
            .env("PBPS_TARGET_1527", &target)
            .env_remove("PBPS_UNSET_SCRATCH_1527")
            .output()
            .unwrap(),
    );
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["result"], "ok", "{report}");
    assert_eq!(report["data"]["changes"], 0, "{report}");
    assert_eq!(
        report["data"]["resolver_assessment"]["need"], "not_needed",
        "{report}"
    );
    assert!(!d.dir.join("nonexistent.key").exists());
    // No cluster mutation happened: the inherited USAGE and kept view remain.
    assert_eq!(chain(), 2);
    assert_eq!(usable(), 1);
    assert_eq!(scalar(admin, "SELECT 'app.v'::regclass::oid::bigint"), kept);
}
