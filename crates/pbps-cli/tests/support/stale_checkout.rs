//! The same checkout-provenance contract through both real-engine CLI paths.
use super::{Demo, OwnDatabase, code, on_server, stderr, stdout};
use std::path::Path;
use std::process::{Command, Output};

fn ok(out: Output) -> Output {
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    out
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
    stdout(&out).trim().to_owned()
}
fn head(d: &Demo) -> String {
    git(&d.dir, &["rev-parse", "HEAD"])
}
fn table(schema: &str, established: bool, coupon: bool, wide: bool) -> String {
    format!(
        "table: {schema}.customer\ncolumns:\n  id: {{type: int, nullable: false}}\n{}{}",
        if established {
            format!(
                "  established: {{type: {}}}\n",
                if wide { "bigint" } else { "int" }
            )
        } else {
            String::new()
        },
        if coupon {
            "  coupon: {type: int}\n"
        } else {
            ""
        }
    )
}
fn write(d: &Demo, schema: &str, established: bool, coupon: bool, wide: bool) {
    std::fs::write(
        d.dir.join("schema/customer.yml"),
        table(schema, established, coupon, wide),
    )
    .unwrap();
}
fn offline(d: &Demo) {
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
}
fn plan(d: &Demo, connection: &str, path: &Path) -> pbps_model::SavedPlan {
    ok(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn apply(d: &Demo, connection: &str, path: &Path, plan: &pbps_model::SavedPlan) -> Output {
    let checksum = plan.wire_fingerprint();
    let risks = plan
        .changes
        .gated_risks()
        .iter()
        .map(|r| r.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let mut args = vec![
        "apply",
        "--db",
        connection,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        &checksum,
    ];
    if !risks.is_empty() {
        args.extend(["--allow", &risks]);
    }
    d.run(&args)
}
fn ledger(pg: bool) -> &'static str {
    if pg {
        "public.__pbps_state"
    } else {
        "dbo.__pbps_state"
    }
}
fn records(connection: &str, pg: bool) -> Vec<(i64, String, Option<String>)> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(
                if pg {
                    pbps_db::Driver::Postgres
                } else {
                    pbps_db::Driver::Mssql
                },
                connection,
            )
            .await
            .unwrap();
            conn.query(&format!(
                "SELECT id, state_json, git_sha FROM {} ORDER BY id",
                ledger(pg)
            ))
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.try_get::<i64>("id").unwrap().unwrap(),
                    row.try_get::<&str>("state_json")
                        .unwrap()
                        .unwrap()
                        .to_owned(),
                    row.try_get::<&str>("git_sha").unwrap().map(str::to_owned),
                )
            })
            .collect()
        })
}
fn configure(d: &Demo, pg: bool) {
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!(
            "dialect: {}\nenvironments:\n  target:\n    url_env: PBPS_STALE2397_DB\n",
            if pg { "postgres" } else { "mssql" }
        ),
    )
    .unwrap();
}
fn expected(sha: &str, list: &str, missing: bool) -> String {
    let reason = if missing {
        format!("deployed commit `{sha}` is not a commit this repository has.")
    } else {
        format!("this checkout does not contain deployed commit `{sha}`.")
    };
    let remedy = if missing {
        format!(
            "Fetch the missing history, then merge or rebase onto `{sha}` and plan again.\nThis is a shallow clone; fetch the missing history. In CI, set `fetch-depth: 0`."
        )
    } else {
        format!("Merge or rebase onto `{sha}`, then plan again.")
    };
    format!("Cannot plan deletions against `target`: {reason}\nDeletions:\n  {list}\n{remedy}")
}
fn refused(d: &Demo, connection: &str, pg: bool, message: &str) {
    let before_ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
    let before_ledger = records(connection, pg);
    let out = d.dir.join("sentinel-plan.json");
    let sql = d.dir.join("sentinel.sql");
    std::fs::write(&out, "existing plan\n").unwrap();
    std::fs::write(&sql, "existing sql\n").unwrap();
    for json in [false, true] {
        let mut args = vec![
            "plan",
            "--env",
            "target",
            "--out",
            out.to_str().unwrap(),
            "--sql",
            sql.to_str().unwrap(),
        ];
        if json {
            args.extend(["--format", "json"]);
        }
        let result = d.run_with_env(&args, &[("PBPS_STALE2397_DB", connection)]);
        assert_eq!(code(&result), 2, "{}{}", stdout(&result), stderr(&result));
        if json {
            let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
            assert_eq!(report["result"], "findings");
            assert_eq!(report["findings"].as_array().unwrap().len(), 1);
            assert_eq!(report["findings"][0]["id"], "plan.checkout-ancestry");
            assert_eq!(report["findings"][0]["message"], message);
            assert_eq!(stderr(&result), "");
        } else {
            assert_eq!(stderr(&result), format!("  error: {message}\n"));
            assert_eq!(stdout(&result), "");
        }
    }
    assert_eq!(std::fs::read(&out).unwrap(), b"existing plan\n");
    assert_eq!(std::fs::read(&sql).unwrap(), b"existing sql\n");
    std::fs::remove_file(out).unwrap();
    std::fs::remove_file(sql).unwrap();
    assert_eq!(
        std::fs::read(d.dir.join("schema.ids.json")).unwrap(),
        before_ids
    );
    assert_eq!(records(connection, pg), before_ledger);
}
fn note(d: &Demo, connection: &str, message: &str) {
    let result = ok(d.run_with_env(
        &["plan", "--env", "target", "--format", "json"],
        &[("PBPS_STALE2397_DB", connection)],
    ));
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let notes: Vec<_> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["id"] == "plan.checkout-unchecked")
        .collect();
    assert_eq!(notes.len(), 1, "{report}");
    assert_eq!(notes[0]["message"], message);
    let human = ok(d.run_with_env(
        &["plan", "--env", "target"],
        &[("PBPS_STALE2397_DB", connection)],
    ));
    assert!(
        stderr(&human)
            .lines()
            .any(|line| line == format!("  note: {message}")),
        "{}",
        stderr(&human)
    );
}
struct Role {
    server: String,
    name: String,
    pg: bool,
}
impl Drop for Role {
    fn drop(&mut self) {
        if self.pg {
            let _ = super::try_on_server(&self.server, &format!("DROP ROLE {}", self.name));
        }
    }
}
fn extra(d: &Demo, schema: &str, role: &str, pg: bool) {
    std::fs::write(
        d.dir.join("schema/owned.yml"),
        format!("table: {schema}.owned\ncolumns:\n  k: {{type: int}}\n"),
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/role.yml"),
        format!(
            "role: {role}\ngrants:\n  {schema}.customer: [select]\n{}",
            if pg { "  schema::app: [usage]\n" } else { "" }
        ),
    )
    .unwrap();
}
fn drop_all(d: &Demo, schema: &str, role: &str) {
    write(d, schema, false, true, false);
    std::fs::remove_file(d.dir.join("schema/owned.yml")).unwrap();
    std::fs::remove_file(d.dir.join("schema/role.yml")).unwrap();
    ok(d.run(&[
        "intent",
        "--drop",
        &format!("{schema}.customer.established"),
        "retired",
        "--drop-table",
        &format!("{schema}.owned"),
        "retired",
        "--drop-role",
        role,
        "retired",
    ]));
    d.commit();
}

pub fn acceptance(server: &str, pg: bool) {
    let role = Role {
        server: server.into(),
        name: format!("pbps_stale2397_{}", std::process::id()),
        pg,
    };
    if pg {
        on_server(server, &format!("CREATE ROLE {} NOLOGIN", role.name));
    }
    let own = OwnDatabase::new(server, "stale2397");
    let connection = own.connection();
    let schema = if pg { "app" } else { "dbo" };
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    let d = Demo::new("stale2397");
    let artifacts = Demo::new("stale2397-artifacts");
    configure(&d, pg);
    write(&d, schema, false, false, false);
    offline(&d);
    let common = head(&d);
    ok(d.run(&["bootstrap", "--db", connection]));
    d.git(&["checkout", "-qb", "deployed"]);
    write(&d, schema, true, false, false);
    extra(&d, schema, &role.name, pg);
    offline(&d);
    let deployed = head(&d);
    let artifact = artifacts.dir.join("plan.json");
    let a = plan(&d, connection, &artifact);
    ok(apply(&d, connection, &artifact, &a));
    assert!(plan(&d, connection, &artifact).changes.is_empty()); // equal

    d.git(&["checkout", "-qb", "stale", &common]);
    write(&d, schema, false, true, false);
    offline(&d);
    let list = format!(
        "column {schema}.customer.established\n  role {}\n  table {schema}.owned",
        role.name
    );
    refused(&d, connection, pg, &expected(&deployed, &list, false));

    // Importing reviewed identities and recording a drop never supplies Git
    // ancestry: this is exactly the stale checkout the guard must still refuse.
    let ids = git(&d.dir, &["show", &format!("{deployed}:schema.ids.json")]);
    std::fs::write(d.dir.join("schema.ids.json"), ids).unwrap();
    write(&d, schema, true, true, false);
    extra(&d, schema, &role.name, pg);
    offline(&d);
    let additive = plan(&d, connection, &artifact);
    assert!(
        additive
            .changes
            .changes
            .iter()
            .any(|c| matches!(c.change, pbps_model::Change::AddColumn { .. }))
    );
    assert!(!additive.changes.changes.iter().any(|c| matches!(
        c.change,
        pbps_model::Change::DropColumn { .. } | pbps_model::Change::DropTable { .. }
    )));
    drop_all(&d, schema, &role.name);
    refused(&d, connection, pg, &expected(&deployed, &list, false));
    let stale = head(&d);

    let shallow = Demo::new("stale2397-shallow");
    std::fs::remove_dir_all(&shallow.dir).unwrap();
    git(
        &d.dir,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            "--single-branch",
            "--branch",
            "stale",
            &format!("file://{}", d.dir.display()),
            shallow.dir.to_str().unwrap(),
        ],
    );
    refused(&shallow, connection, pg, &expected(&deployed, &list, true));

    // Isolate role disappearance: on PostgreSQL this emits revokes, not DROP ROLE.
    d.git(&["checkout", "-q", "--detach", &deployed]);
    std::fs::remove_file(d.dir.join("schema/role.yml")).unwrap();
    ok(d.run(&["drop-role", &role.name, "--reason", "retired"]));
    d.commit();
    // This descendant is up to date; changing its parentage to the stale
    // branch while retaining its exact tree makes only the ancestry differ.
    let tree = git(&d.dir, &["rev-parse", "HEAD^{tree}"]);
    let sibling = git(
        &d.dir,
        &[
            "commit-tree",
            &tree,
            "-p",
            &common,
            "-m",
            "role removal on sibling",
        ],
    );
    d.git(&["checkout", "-q", "--detach", &sibling]);
    refused(
        &d,
        connection,
        pg,
        &expected(&deployed, &format!("role {}", role.name), false),
    );

    d.git(&["checkout", "-q", "--detach", &deployed]);
    drop_all(&d, schema, &role.name);
    let legitimate = plan(&d, connection, &artifact);
    assert!(
        legitimate
            .changes
            .changes
            .iter()
            .any(|c| matches!(c.change, pbps_model::Change::DropColumn { .. }))
    );
    let export = Demo::new("stale2397-export");
    std::fs::remove_dir_all(&export.dir).unwrap();
    git(
        &d.dir,
        &[
            "clone",
            "-q",
            "--no-local",
            d.dir.to_str().unwrap(),
            export.dir.to_str().unwrap(),
        ],
    );
    std::fs::remove_dir_all(export.dir.join(".git")).unwrap();
    note(
        &export,
        connection,
        "Checkout ancestry was not checked for `target`: the planning project has no Git checkout. Planning continues without checkout-ancestry protection.",
    );

    // A no-Git baseline is actual NULL evidence, not a missing object. Record
    // the complete live A declaration scope, then plan the stale removal tree.
    for file in [
        "schema/customer.yml",
        "schema/owned.yml",
        "schema/role.yml",
        "schema.ids.json",
    ] {
        let contents = git(&d.dir, &["show", &format!("{deployed}:{file}")]);
        std::fs::write(export.dir.join(file), format!("{contents}\n")).unwrap();
    }
    ok(export.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "recorded without Git",
    ]));
    let rows = records(connection, pg);
    assert_eq!(rows.last().unwrap().2, None);
    d.git(&["checkout", "-q", "--detach", &stale]);
    note(
        &d,
        connection,
        &format!(
            "Checkout ancestry was not checked for `target`: ledger entry #{} has no Git commit. Planning continues without checkout-ancestry protection.",
            rows.last().unwrap().0
        ),
    );
    absorbed_rename_still_allows_a_modification(server, pg);
}

fn absorbed_rename_still_allows_a_modification(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "absorbed2397");
    let connection = own.connection();
    let schema = if pg { "app" } else { "dbo" };
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    let d = Demo::new("absorbed2397");
    let artifacts = Demo::new("absorbed2397-artifacts");
    configure(&d, pg);
    write(&d, schema, false, false, false);
    offline(&d);
    let common = head(&d);
    std::fs::write(d.dir.join("schema/customer.yml"), format!(
        "table: {schema}.client\nrenamed_from: {schema}.customer\ncolumns:\n  id: {{type: int, nullable: false}}\n"
    )).unwrap();
    offline(&d);
    let reused = d.dir.join("schema/reused.yml");
    std::fs::write(
        &reused,
        format!("table: {schema}.customer\ncolumns:\n  reused: {{type: int}}\n"),
    )
    .unwrap();
    offline(&d);
    ok(d.run(&["bootstrap", "--db", connection]));
    // Keep the absorbed annotation and both identities, but omit the deployed
    // commit from this checkout's ancestry. Only a real removal may refuse it.
    let tree = git(&d.dir, &["rev-parse", "HEAD^{tree}"]);
    let sibling = git(
        &d.dir,
        &[
            "commit-tree",
            &tree,
            "-p",
            &common,
            "-m",
            "retained annotation on sibling",
        ],
    );
    d.git(&["checkout", "-q", "--detach", &sibling]);
    std::fs::write(
        &reused,
        format!("table: {schema}.customer\ncolumns:\n  reused: {{type: bigint}}\n"),
    )
    .unwrap();
    d.commit();
    let planned = plan(&d, connection, &artifacts.dir.join("plan.json"));
    assert!(!planned.changes.is_empty());
    assert!(!planned.changes.changes.iter().any(|change| matches!(
        change.change,
        pbps_model::Change::DropColumn { .. } | pbps_model::Change::DropTable { .. }
    )));
}

pub fn failed_attempt(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "failed2397");
    let connection = own.connection();
    let schema = if pg { "app" } else { "dbo" };
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    let d = Demo::new("failed2397");
    let artifacts = Demo::new("failed2397-artifacts");
    configure(&d, pg);
    write(&d, schema, true, false, false);
    offline(&d);
    let common = head(&d);
    ok(d.run(&["bootstrap", "--db", connection]));
    d.git(&["checkout", "-qb", "attempt"]);
    write(&d, schema, false, false, false);
    ok(d.run(&[
        "drop",
        &format!("{schema}.customer.established"),
        "--reason",
        "retired",
    ]));
    d.commit();
    let attempted = head(&d);
    let c_path = artifacts.dir.join("old.json");
    let c = plan(&d, connection, &c_path);

    d.git(&["checkout", "-qb", "newer", &common]);
    write(&d, schema, true, false, true);
    offline(&d);
    let newer = head(&d);
    let path = artifacts.dir.join("new.json");
    let newer_plan = plan(&d, connection, &path);
    ok(apply(&d, connection, &path, &newer_plan));
    d.git(&["checkout", "attempt"]);
    // Apply still needs no Git. Its ordinary checksum guard sees the target
    // advance, and the failure must not make the attempted SHA the new evidence.
    std::fs::rename(d.dir.join(".git"), d.dir.join("git-held")).unwrap();
    let failed = apply(&d, connection, &c_path, &c);
    std::fs::rename(d.dir.join("git-held"), d.dir.join(".git")).unwrap();
    assert_eq!(code(&failed), 1, "{}{}", stdout(&failed), stderr(&failed));
    assert!(stderr(&failed).contains("Something changed since the plan was approved."));
    let rows = records(connection, pg);
    let last = rows.last().unwrap();
    let snapshot: pbps_model::StateSnapshot = serde_json::from_str(&last.1).unwrap();
    assert_eq!(snapshot.kind, pbps_model::StateKind::Failed);
    assert_eq!(last.2.as_deref(), Some(attempted.as_str()));
    assert_eq!(snapshot.schema.tables[&format!("{schema}.customer").parse().unwrap()].columns["established"].ty.base, "bigint");
    refused(
        &d,
        connection,
        pg,
        &expected(
            &newer,
            &format!("column {schema}.customer.established"),
            false,
        ),
    );

    // Pruning all successful evidence is a known absence, not an unreadable row.
    on_server(
        connection,
        &format!("DELETE FROM {} WHERE kind <> 'failed'", ledger(pg)),
    );
    note(
        &d,
        connection,
        "Checkout ancestry was not checked for `target`: the ledger has no retained non-failed entry. Planning continues without checkout-ancestry protection.",
    );
}
