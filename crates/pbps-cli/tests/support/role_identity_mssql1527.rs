//! SQL Server role identity changes leave the database user's schema lookup
//! path unchanged. Selecting a resolver must retain case 2 (DEC-1527.1).
use super::*;

pub(super) fn check(server: &str) {
    let own = OwnDatabase::new(server, "roles1527");
    let d = Demo::new("roles1527");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/view.yml"),
        "view: dbo.v\ndefinition: SELECT id FROM dbo.t\n",
    )
    .unwrap();
    let output = d.run(&["plan"]);
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    d.commit();
    let output = d.run(&["bootstrap", "--db", own.connection()]);
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    std::fs::write(d.dir.join("schema/reader.yml"), "role: reader1527\n").unwrap();
    let output = d.run(&["plan"]);
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    d.commit();
    std::fs::write(
        d.dir.join("pbps.yml"),
        concat!(
            "dialect: mssql\nenvironments:\n  prod:\n    url_env: PBPS_TARGET_1527\n",
            "    resolve_with: scratch\n    fingerprint_key_file: nonexistent.key\n",
            "resolvers:\n  scratch:\n    kind: server\n    url_env: PBPS_UNSET_SCRATCH_1527\n",
        ),
    )
    .unwrap();
    let output = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["plan", "--env", "prod", "--no-rehearse", "--format", "json"])
        .env("PBPS_TARGET_1527", own.connection())
        .env_remove("PBPS_UNSET_SCRATCH_1527")
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(
        report["data"]["resolver_assessment"]["need"], "not_needed",
        "{report}"
    );
    assert!(report["data"]["changes"].as_u64().unwrap() > 0, "{report}");
    assert!(!d.dir.join("nonexistent.key").exists());
}
