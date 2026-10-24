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
