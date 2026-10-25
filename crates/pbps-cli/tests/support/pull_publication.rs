//! Forced pull publishes validated bytes without following old filename aliases.
use super::{Demo, OwnDatabase, code, on_server, stderr, stdout};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn ok(out: std::process::Output) {
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
}

fn schema(pg: bool) -> &'static str {
    if pg { "app" } else { "dbo" }
}

fn prepared(server: &str, pg: bool, name: &str) -> (OwnDatabase, Demo) {
    let own = OwnDatabase::new(server, name);
    if pg {
        on_server(own.connection(), "CREATE SCHEMA app");
    }
    on_server(
        own.connection(),
        &format!("CREATE TABLE {}.country (id int NOT NULL)", schema(pg)),
    );
    let demo = Demo::new(name);
    ok(demo.run(&["pull", "--db", own.connection()]));
    (own, demo)
}

// Include directory entries and link targets: equal file contents alone would
// miss publication that deleted a link or left a new empty directory behind.
fn project_bytes(root: &Path) -> BTreeMap<PathBuf, (u8, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, (u8, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().unwrap() == ".git" {
                continue;
            }
            let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
            let value = if kind.is_symlink() {
                (
                    2,
                    std::fs::read_link(&path)
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                )
            } else if kind.is_dir() {
                walk(root, &path, out);
                (0, Vec::new())
            } else {
                (1, std::fs::read(&path).unwrap())
            };
            out.insert(path.strip_prefix(root).unwrap().to_owned(), value);
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

pub fn aliases(server: &str, pg: bool, cased: bool) {
    let simulations: &[bool] = if cased { &[false, true] } else { &[true] };
    for simulate in simulations {
        let (own, d) = prepared(server, pg, &format!("pull2311_alias_{cased}_{simulate}"));
        let dir = d.dir.join("schema");
        let output = dir.join(format!("{}.country.yml", schema(pg)));
        let old = dir.join(if cased {
            format!("{}.Country.yml", schema(pg))
        } else {
            "legacy.yml".to_owned()
        });
        let expected = pbps_load::load_schema_dir(&dir).unwrap().schema;
        let config = std::fs::read(d.dir.join("pbps.yml")).unwrap();
        let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
        std::fs::rename(&output, &old).unwrap();
        // A real case-insensitive filesystem already resolves the generated name
        // to the old entry. On Unix case-sensitive filesystems a symlink exercises
        // the same write-through-then-delete failure, as #2310's export test does.
        #[cfg(unix)]
        if *simulate && !output.exists() {
            std::os::unix::fs::symlink(old.file_name().unwrap(), &output).unwrap();
        }
        std::fs::write(dir.join("notes.txt"), b"keep non-declarations\n").unwrap();
        let before = project_bytes(&d.dir);
        let refused = d.run(&["pull", "--db", own.connection()]);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(stderr(&refused).contains("already has declarations"));
        assert_eq!(project_bytes(&d.dir), before, "unforced pull changed files");

        let pulled = d.run(&["pull", "--force", "--db", own.connection()]);
        assert_eq!(code(&pulled), 0, "{}{}", stdout(&pulled), stderr(&pulled));
        assert!(stderr(&pulled).contains("removed 1 declaration file(s)"));
        assert_eq!(
            pbps_load::schema_files(&dir).unwrap().as_slice(),
            std::slice::from_ref(&output)
        );
        assert_eq!(pbps_load::load_schema_dir(&dir).unwrap().schema, expected);
        assert!(!std::fs::symlink_metadata(&output).unwrap().is_symlink());
        ok(d.run(&["validate"]));
        assert_eq!(std::fs::read(d.dir.join("pbps.yml")).unwrap(), config);
        assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
        assert_eq!(
            std::fs::read(dir.join("notes.txt")).unwrap(),
            b"keep non-declarations\n"
        );
    }
}

pub fn invalid_staging(server: &str, pg: bool) {
    let (own, d) = prepared(server, pg, "pull2311_invalid");
    let dialect = if pg { "postgres" } else { "mssql" };
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!(
            concat!(
                "dialect: {dialect}\npolicies:\n  rules:\n",
                "    naming.table: {{severity: error, pattern: '^country$'}}\n"
            ),
            dialect = dialect,
        ),
    )
    .unwrap();
    ok(d.run(&["validate"]));
    on_server(
        own.connection(),
        &format!("CREATE TABLE {}.rejected (id int NOT NULL)", schema(pg)),
    );
    let dir = d.dir.join("schema");
    let output = dir.join(format!("{}.country.yml", schema(pg)));
    let old = dir.join("custom.yml");
    std::fs::rename(&output, &old).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("custom.yml", &output).unwrap();
    std::fs::create_dir(dir.join("empty")).unwrap();
    std::fs::write(dir.join("notes.txt"), b"keep exactly\n").unwrap();
    let before = project_bytes(&d.dir);
    let refused = d.run(&["pull", "--force", "--db", own.connection()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    let said = stderr(&refused);
    assert!(
        said.contains("naming.table") && said.contains("rejected"),
        "{said}"
    );
    assert!(said.contains("Nothing was written"), "{said}");
    assert_eq!(
        project_bytes(&d.dir),
        before,
        "invalid staging changed the project"
    );
}

#[cfg(unix)]
pub fn protected_aliases(server: &str, pg: bool) {
    for protected in ["pbps.yml", "schema.ids.json"] {
        for stale in [false, true] {
            let name = format!("pull2311_protected_{protected}_{stale}");
            let (own, d) = prepared(server, pg, &name);
            let dir = d.dir.join("schema");
            let path = dir.join(if stale {
                "legacy.yml".to_owned()
            } else {
                format!("{}.country.yml", schema(pg))
            });
            if !stale {
                std::fs::remove_file(&path).unwrap();
            }
            std::os::unix::fs::symlink(format!("../{protected}"), &path).unwrap();
            let before = project_bytes(&d.dir);
            let refused = d.run(&["pull", "--force", "--db", own.connection()]);
            assert_eq!(
                code(&refused),
                1,
                "{}{}",
                stdout(&refused),
                stderr(&refused)
            );
            let said = stderr(&refused);
            assert!(said.contains("aliases protected project input"), "{said}");
            assert!(said.contains(protected), "{said}");
            assert_eq!(
                project_bytes(&d.dir),
                before,
                "protected alias changed files"
            );
        }
    }
    let (own, d) = prepared(server, pg, "pull2311_ids_filename");
    let output = format!("schema/{}.country.yml", schema(pg));
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!(
            "dialect: {}\nids_file: {output}\n",
            if pg { "postgres" } else { "mssql" }
        ),
    )
    .unwrap();
    std::fs::rename(d.dir.join("schema.ids.json"), d.dir.join(&output)).unwrap();
    let before = project_bytes(&d.dir);
    let refused = d.run(&["pull", "--force", "--db", own.connection()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("aliases protected project input"));
    assert_eq!(project_bytes(&d.dir), before);
}

pub fn output_directories(server: &str, pg: bool) {
    let variants: &[bool] = if cfg!(unix) { &[false, true] } else { &[false] };
    for &linked in variants {
        let (own, d) = prepared(server, pg, &format!("pull2311_directory_{linked}"));
        let dir = d.dir.join("schema");
        let output = dir.join(format!("{}.country.yml", schema(pg)));
        std::fs::rename(&output, dir.join("custom.yml")).unwrap();
        let directory = if linked {
            dir.join("directory")
        } else {
            output.clone()
        };
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("notes.txt"), b"keep this directory too\n").unwrap();
        #[cfg(unix)]
        if linked {
            std::os::unix::fs::symlink(directory.file_name().unwrap(), &output).unwrap();
        }
        ok(d.run(&["validate"]));
        let before = project_bytes(&d.dir);
        let refused = d.run(&["pull", "--force", "--db", own.connection()]);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert_eq!(
            project_bytes(&d.dir),
            before,
            "directory output deleted declarations"
        );
        let said = stderr(&refused);
        assert!(
            said.contains("cannot create declaration")
                && said.contains("move any existing")
                && said.contains("the project is unchanged"),
            "{said}"
        );
    }
}

#[cfg(unix)]
pub fn restrictive_permissions(server: &str, pg: bool) {
    use std::os::unix::fs::PermissionsExt;
    for (mode, linked) in [(0o600, false), (0o640, true), (0o644, false)] {
        let name = format!("pull2311_permissions_{mode}_{linked}");
        let own = OwnDatabase::new(server, &name);
        if pg {
            on_server(own.connection(), "CREATE SCHEMA app");
        }
        on_server(
            own.connection(),
            &format!(
                "CREATE TABLE {schema}.country (code varchar(20) PRIMARY KEY, label varchar(80) NOT NULL); \
                 INSERT INTO {schema}.country VALUES ('private', 'secret-before')",
                schema = schema(pg),
            ),
        );
        let d = Demo::new(&name);
        let table = format!("{}.country", schema(pg));
        ok(d.run(&["pull", "--db", own.connection(), "--data", &table]));
        let dir = d.dir.join("schema");
        let output = dir.join(format!("{table}.yml"));
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(mode)).unwrap();
        if linked {
            std::fs::rename(&output, dir.join("legacy.yml")).unwrap();
            std::os::unix::fs::symlink("legacy.yml", &output).unwrap();
        }
        on_server(
            own.connection(),
            &format!("UPDATE {table} SET label = 'secret-after' WHERE code = 'private'"),
        );
        let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
        ok(d.run(&[
            "pull",
            "--force",
            "--db",
            own.connection(),
            "--data",
            &table,
        ]));
        let actual = std::fs::metadata(&output).unwrap().permissions().mode() & 0o777;
        assert_eq!(actual, mode, "forced pull broadened declaration access");
        assert!(!std::fs::symlink_metadata(&output).unwrap().is_symlink());
        let text = std::fs::read_to_string(&output).unwrap();
        assert!(text.contains("secret-after"), "{text}");
        assert!(!text.contains("secret-before"), "{text}");
        assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
        ok(d.run(&["validate"]));
    }
}

#[cfg(unix)]
pub fn dangling_output_parent(server: &str, pg: bool) {
    struct PgRole<'a>(&'a str, String);
    impl Drop for PgRole<'_> {
        fn drop(&mut self) {
            on_server(self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let name = format!("pbps_parent2311_{}", std::process::id());
    // Drop the private database before the cluster role, including on failure.
    let _role = pg.then(|| PgRole(server, name.clone()));
    let own = OwnDatabase::new(server, "pull2311_parent");
    on_server(own.connection(), "CREATE SCHEMA sales");
    on_server(
        own.connection(),
        "CREATE TABLE sales.country (id int NOT NULL)",
    );
    let d = Demo::new("pull2311_parent");
    ok(d.run(&["pull", "--db", own.connection()]));
    on_server(own.connection(), &format!("CREATE ROLE {name}"));
    if pg {
        on_server(
            own.connection(),
            &format!("GRANT USAGE ON SCHEMA sales TO {name}"),
        );
    }
    on_server(
        own.connection(),
        &format!("GRANT SELECT ON sales.country TO {name}"),
    );
    let roles = d.dir.join("schema/roles");
    std::os::unix::fs::symlink("missing-roles", &roles).unwrap();
    ok(d.run(&["validate"]));
    let before = project_bytes(&d.dir);
    let refused = d.run(&["pull", "--force", "--db", own.connection()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert_eq!(
        project_bytes(&d.dir),
        before,
        "output-parent failure deleted declarations"
    );

    std::fs::remove_file(&roles).unwrap();
    ok(d.run(&["pull", "--force", "--db", own.connection()]));
    assert!(roles.join(format!("{name}.yml")).is_file());
    assert!(d.dir.join("schema/sales.country.yml").is_file());
    ok(d.run(&["validate"]));
}

#[cfg(unix)]
pub fn restorable_commands(server: &str, pg: bool) {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let (own, d) = prepared(server, pg, "publication2351_commands");
    d.commit();
    ok(d.run(&[
        "baseline",
        "--db",
        own.connection(),
        "--reason",
        "publication fixture",
    ]));
    let dir = d.dir.join("schema");
    let output = dir.join(format!("{}.country.yml", schema(pg)));
    let expected = pbps_load::load_schema_dir(&dir).unwrap().schema;
    std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o640)).unwrap();
    if let Some(group) = rustix::process::getgroups()
        .unwrap()
        .into_iter()
        .find(|g| *g != rustix::process::getegid())
    {
        rustix::fs::chown(&output, None, Some(group)).unwrap();
    }
    let before = std::fs::metadata(&output).unwrap();
    std::fs::hard_link(&output, dir.join("legacy.yml")).unwrap();
    ok(d.run(&["pull", "--force", "--db", own.connection()]));
    let after = std::fs::metadata(&output).unwrap();
    assert_eq!(
        (after.mode() & 0o7777, after.gid()),
        (before.mode() & 0o7777, before.gid())
    );
    assert_eq!(
        pbps_load::schema_files(&dir).unwrap().as_slice(),
        std::slice::from_ref(&output)
    );
    let list = d.run(&[
        "state",
        "list",
        "--db",
        own.connection(),
        "--format",
        "json",
    ]);
    assert_eq!(code(&list), 0, "{}{}", stdout(&list), stderr(&list));
    let list: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let id = list["data"]["entries"][0]["id"]
        .as_i64()
        .unwrap()
        .to_string();
    std::fs::rename(&output, dir.join("custom.yml")).unwrap();
    std::fs::create_dir(&output).unwrap();
    std::fs::write(output.join("notes.txt"), "keep the directory too").unwrap();
    let before = project_bytes(&d.dir);
    for args in [
        vec!["pull", "--force", "--db", own.connection()],
        vec![
            "state",
            "export",
            &id,
            "--force",
            "--db",
            own.connection(),
            "--format",
            "json",
        ],
    ] {
        let refused = d.run(&args);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        let said = format!("{}{}", stdout(&refused), stderr(&refused));
        assert!(said.contains("the project is unchanged"), "{said}");
        assert_eq!(
            project_bytes(&d.dir),
            before,
            "{args:?} damaged the project"
        );
    }
    std::fs::remove_dir_all(&output).unwrap();
    let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
    ok(d.run(&["state", "export", &id, "--force", "--db", own.connection()]));
    assert_eq!(pbps_load::load_schema_dir(&dir).unwrap().schema, expected);
    assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
    ok(d.run(&["validate"]));

    // IDs are written after declaration publication. Preparing their parent
    // must not leave a new schema subdirectory behind when publication fails.
    let fresh = Demo::new("publication2351_ids_parent");
    let config = fresh.dir.join("pbps.yml");
    let mut yaml = std::fs::read_to_string(&config).unwrap();
    yaml.push_str("\nids_file: schema/new-identities/ids.json\n");
    std::fs::write(&config, yaml).unwrap();
    let fresh_dir = fresh.dir.join("schema");
    std::fs::write(
        fresh_dir.join("custom.yml"),
        std::fs::read(&output).unwrap(),
    )
    .unwrap();
    std::fs::create_dir(fresh_dir.join(format!("{}.country.yml", schema(pg)))).unwrap();
    let before = project_bytes(&fresh.dir);
    let refused = fresh.run(&["pull", "--force", "--db", own.connection()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("the project is unchanged"));
    assert_eq!(
        project_bytes(&fresh.dir),
        before,
        "IDs-parent preparation escaped restoration"
    );
}
