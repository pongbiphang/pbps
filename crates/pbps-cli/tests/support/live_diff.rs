//! `pbps diff` (#3028): the declarations against a live database with no
//! ledger, through both engine-facing CLI paths.
use super::stale_checkout::records;
use super::{Demo, OwnDatabase, code, on_server, stderr, stdout};
use pbps_model::{Change, ChangeSet, ColumnChange, TableChange};
use serde_json::Value;
use std::path::Path;
use std::process::Output;

fn schema(pg: bool) -> &'static str {
    if pg { "app" } else { "dbo" }
}

/// A database of this test's own, holding `statements` (`{s}` is the schema),
/// and a project that declares nothing yet. No command of pbps has touched
/// the database: what it holds, a person wrote.
fn fixture(server: &str, pg: bool, slug: &str, statements: &[&str]) -> (OwnDatabase, Demo) {
    let own = OwnDatabase::new(server, slug);
    if pg {
        on_server(own.connection(), "CREATE SCHEMA app");
    }
    for statement in statements {
        on_server(own.connection(), &statement.replace("{s}", schema(pg)));
    }
    (own, Demo::new(slug))
}

/// Declares `{schema}.{name}` with `rest` as everything under `table:`.
fn declare(d: &Demo, pg: bool, name: &str, rest: &str) {
    std::fs::write(
        d.dir.join(format!("schema/{name}.yml")),
        format!("table: {}.{name}\n{rest}", schema(pg)),
    )
    .unwrap();
}

const ID: &str = "columns:\n  id: {type: int, nullable: false}\n";
const ID_NAME: &str = "columns:\n  id: {type: int, nullable: false}\n  name: {type: int}\n";

/// The envelope of `diff --format json`, with the exit code it came with.
fn answer(d: &Demo, connection: &str, expected: i32) -> Value {
    let out = d.run(&["diff", "--db", connection, "--format", "json"]);
    assert_eq!(code(&out), expected, "{}{}", stdout(&out), stderr(&out));
    let report: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}{}", stdout(&out), stderr(&out)));
    assert_eq!(report["command"], "diff");
    // The current document alone, as `state diff`'s tests hold theirs: no
    // archived set knows this command, so none could accept its envelope.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/envelope.schema.json");
    let published: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    if let Err(e) = jsonschema::validator_for(&published)
        .unwrap()
        .validate(&report)
    {
        panic!("the envelope does not match the published schema: {e}\n{report:#}");
    }
    report
}

fn human(d: &Demo, connection: &str, expected: i32) -> Output {
    let out = d.run(&["diff", "--db", connection]);
    assert_eq!(code(&out), expected, "{}{}", stdout(&out), stderr(&out));
    out
}

fn changes(report: &Value) -> ChangeSet {
    serde_json::from_value(report["data"]["changes"].clone()).unwrap()
}

fn finding_ids(report: &Value) -> Vec<&str> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| finding["id"].as_str().unwrap())
        .collect()
}

/// This command's own findings. The reader's warnings about the catalog stand
/// beside them (`catalog.warning`), as they do in every command that reads
/// one, and which of those a server gives is the engine's business.
fn own_finding_ids(report: &Value) -> Vec<&str> {
    finding_ids(report)
        .into_iter()
        .filter(|id| id.starts_with("diff."))
        .collect()
}

fn listed(report: &Value, field: &str) -> Vec<String> {
    report["data"][field]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

/// How many tables the database holds whose name starts as the ledger's do.
fn ledger_objects(connection: &str, pg: bool) -> i64 {
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
            // Every schema: a ledger anywhere would be one this command made.
            conn.query(
                "SELECT CAST(COUNT(*) AS bigint) AS n FROM information_schema.tables \
                 WHERE LEFT(table_name, 6) = '__pbps'",
            )
            .await
            .unwrap()[0]
                .try_get::<i64>("n")
                .unwrap()
                .unwrap()
        })
}

/// Acceptance 1: a database a person made to match answers "no differences"
/// with exit 0, and the command leaves neither a ledger in the database nor an
/// identity in the project.
pub fn a_matching_database_has_no_difference_and_gains_nothing(server: &str, pg: bool) {
    let (own, d) = fixture(
        server,
        pg,
        "livediff3028a",
        &["CREATE TABLE {s}.t (id int NOT NULL, name int NULL)"],
    );
    declare(&d, pg, "t", ID_NAME);
    let ids = crate::idtree::snapshot(&d.dir);

    let out = human(&d, own.connection(), 0);
    assert!(stdout(&out).contains("No differences."), "{}", stdout(&out));
    let report = answer(&d, own.connection(), 0);
    assert_eq!(report["result"], "ok", "{report:#}");
    assert!(changes(&report).is_empty(), "{report:#}");
    assert_eq!(report["data"]["text_differences"], serde_json::json!([]));

    assert_eq!(ledger_objects(own.connection(), pg), 0);
    assert_eq!(crate::idtree::snapshot(&d.dir), ids);
}

/// Acceptance 2: a column the database lacks is exactly one added column,
/// exit 2, and `result` is `findings`.
pub fn a_missing_column_is_one_added_column(server: &str, pg: bool) {
    let (own, d) = fixture(
        server,
        pg,
        "livediff3028b",
        &["CREATE TABLE {s}.t (id int NOT NULL)"],
    );
    declare(&d, pg, "t", ID_NAME);

    let report = answer(&d, own.connection(), 2);
    assert_eq!(report["result"], "findings", "{report:#}");
    let found = changes(&report);
    let [only] = found.changes.as_slice() else {
        panic!("one change, got {report:#}");
    };
    assert!(
        matches!(
            &only.change,
            Change::Column(ColumnChange::AddColumn { name, .. })
                if name.shown().to_string() == "name"
        ),
        "{only:?}"
    );
    assert!(finding_ids(&report).contains(&"diff.differs"), "{report:#}");
    // One added column is not said to be a possible rename.
    assert!(
        !finding_ids(&report).contains(&"diff.names-only"),
        "{report:#}"
    );

    let out = human(&d, own.connection(), 2);
    assert!(
        !stdout(&out).contains("No differences."),
        "{}",
        stdout(&out)
    );
    assert!(stdout(&out).contains("name"), "{}", stdout(&out));
    assert_eq!(ledger_objects(own.connection(), pg), 0);
}

/// Acceptance 3 and 5: an object outside the declarations is named and not
/// compared, under the project's `unmanaged:` policy; and a part of a managed
/// table the reader leaves out is listed after the answer without becoming a
/// difference.
pub fn what_was_not_compared_is_listed_and_is_no_difference(server: &str, pg: bool) {
    let (own, d) = fixture(
        server,
        pg,
        "livediff3028c",
        &[
            "CREATE TABLE {s}.t (id int NOT NULL)",
            "CREATE TABLE {s}.other (id int NOT NULL, extra int NULL)",
        ],
    );
    declare(&d, pg, "t", ID);
    let dialect = if pg { "postgres" } else { "mssql" };

    let report = answer(&d, own.connection(), 0);
    assert!(changes(&report).is_empty(), "{report:#}");
    let unmanaged = listed(&report, "unmanaged");
    assert!(
        unmanaged.len() == 1 && unmanaged[0].contains("other"),
        "{report:#}"
    );
    assert_eq!(own_finding_ids(&report), ["diff.unmanaged"], "{report:#}");

    // `unmanaged: error` refuses, and the comparison is still the answer.
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: {dialect}\nunmanaged: error\n"),
    )
    .unwrap();
    let report = answer(&d, own.connection(), 2);
    assert_eq!(report["result"], "findings", "{report:#}");
    assert!(changes(&report).is_empty(), "{report:#}");
    assert!(
        finding_ids(&report).contains(&"diff.unmanaged-refused"),
        "{report:#}"
    );
    assert!(
        !finding_ids(&report).contains(&"diff.differs"),
        "{report:#}"
    );

    // `unmanaged: warn` names it to a person and refuses nothing.
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: {dialect}\nunmanaged: warn\n"),
    )
    .unwrap();
    let out = human(&d, own.connection(), 0);
    assert!(stderr(&out).contains("other"), "{}", stderr(&out));
    assert!(stdout(&out).contains("No differences."), "{}", stdout(&out));
    std::fs::write(d.dir.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();

    // A part the reader cannot hold, on the managed table. On SQL Server the
    // index is unique: an ordinary disabled index is read as an index, since
    // only a unique one decides which writes survive.
    let (part, statements): (&str, &[&str]) = if pg {
        (
            "uq_t_id",
            &["ALTER TABLE app.t ADD CONSTRAINT uq_t_id UNIQUE (id) DEFERRABLE"],
        )
    } else {
        (
            "ix_t_id",
            &[
                "CREATE UNIQUE INDEX ix_t_id ON dbo.t (id)",
                "ALTER INDEX ix_t_id ON dbo.t DISABLE",
            ],
        )
    };
    for statement in statements {
        on_server(own.connection(), statement);
    }
    let report = answer(&d, own.connection(), 0);
    assert_eq!(report["result"], "ok", "{report:#}");
    assert!(changes(&report).is_empty(), "{report:#}");
    let not_compared = listed(&report, "not_compared");
    assert!(
        not_compared.len() == 1 && not_compared[0].contains(part),
        "{report:#}"
    );
    let warned = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["id"] == "diff.not-compared")
        .unwrap_or_else(|| panic!("{report:#}"));
    assert_eq!(warned["severity"], "warning");
    assert!(warned["message"].as_str().unwrap().contains(part));
    // A person is told too, after "No differences.", which stays true.
    let out = human(&d, own.connection(), 0);
    assert!(stdout(&out).contains("No differences."), "{}", stdout(&out));
    assert!(stderr(&out).contains(part), "{}", stderr(&out));

    // A table the reader leaves out whole, outside the declarations, is
    // still unmanaged: `unmanaged: error` refuses it like any other.
    // `other` goes first, so that `ruled` alone is what is refused.
    if pg {
        on_server(own.connection(), "DROP TABLE app.other");
        on_server(own.connection(), "CREATE TABLE app.ruled (id int NOT NULL)");
        on_server(
            own.connection(),
            "CREATE RULE ruled_insert AS ON INSERT TO app.ruled DO INSTEAD NOTHING",
        );
        let report = answer(&d, own.connection(), 0);
        let unmanaged = listed(&report, "unmanaged");
        assert!(
            unmanaged.len() == 1 && unmanaged[0].contains("ruled"),
            "{report:#}"
        );
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!("dialect: {dialect}\nunmanaged: error\n"),
        )
        .unwrap();
        let report = answer(&d, own.connection(), 2);
        assert!(
            finding_ids(&report).contains(&"diff.unmanaged-refused"),
            "{report:#}"
        );
        std::fs::write(d.dir.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
    }
}

/// Acceptance 4, as amended: names are all this command pairs by.
pub fn a_rename_is_a_drop_and_a_create(server: &str, pg: bool) {
    // In the database: the declared table stands under another name, and a
    // declared column under another name.
    let (own, d) = fixture(
        server,
        pg,
        "livediff3028d",
        &[
            "CREATE TABLE {s}.moved (id int NOT NULL)",
            "CREATE TABLE {s}.c (id int NOT NULL, name int NULL)",
        ],
    );
    declare(&d, pg, "t", ID);
    declare(
        &d,
        pg,
        "c",
        "columns:\n  id: {type: int, nullable: false}\n  title: {type: int}\n",
    );
    let report = answer(&d, own.connection(), 2);
    let found = changes(&report);
    let created = |wanted: &str| {
        found.changes.iter().any(|planned| {
            matches!(
                &planned.change,
                Change::Table(TableChange::CreateTable { name, .. })
                    if name.shown().to_string().ends_with(wanted)
            )
        })
    };
    let dropped_table = found.changes.iter().any(|planned| {
        matches!(
            &planned.change,
            Change::Table(TableChange::DropTable { .. })
        )
    });
    let dropped_column = found.changes.iter().any(|planned| {
        matches!(
            &planned.change,
            Change::Column(ColumnChange::DropColumn { column, .. })
                if column.name.shown().to_string() == "name"
        )
    });
    let added_column = found.changes.iter().any(|planned| {
        matches!(
            &planned.change,
            Change::Column(ColumnChange::AddColumn { name, .. })
                if name.shown().to_string() == "title"
        )
    });
    assert!(
        created(".t") && dropped_column && added_column,
        "{report:#}"
    );
    assert_eq!(found.changes.len(), 3, "{report:#}");
    // The table under the other name is nobody's: named, never dropped.
    assert!(!dropped_table, "{report:#}");
    let unmanaged = listed(&report, "unmanaged");
    assert!(
        unmanaged.len() == 1 && unmanaged[0].contains("moved"),
        "{report:#}"
    );
    assert!(
        finding_ids(&report).contains(&"diff.names-only"),
        "{report:#}"
    );

    // In the declarations: the identity file still names `old`, which the
    // database holds, and the declarations now say `t`.
    let (own, d) = fixture(
        server,
        pg,
        "livediff3028e",
        &["CREATE TABLE {s}.old (id int NOT NULL)"],
    );
    declare(&d, pg, "old", ID);
    let planned = d.run(&["plan", "--no-dev"]);
    assert_eq!(
        code(&planned),
        0,
        "{}{}",
        stdout(&planned),
        stderr(&planned)
    );
    std::fs::remove_file(d.dir.join("schema/old.yml")).unwrap();
    declare(&d, pg, "t", ID);
    let ids = crate::idtree::snapshot(&d.dir);
    let report = answer(&d, own.connection(), 2);
    let found = changes(&report);
    let dropped = found.changes.iter().any(|planned| {
        matches!(
            &planned.change,
            Change::Table(TableChange::DropTable { name, .. })
                if name.shown().to_string().ends_with(".old")
        )
    });
    let created = found.changes.iter().any(|planned| {
        matches!(
            &planned.change,
            Change::Table(TableChange::CreateTable { name, .. })
                if name.shown().to_string().ends_with(".t")
        )
    });
    assert!(dropped && created, "{report:#}");
    assert_eq!(found.changes.len(), 2, "{report:#}");
    assert_eq!(listed(&report, "unmanaged"), Vec::<String>::new());
    assert!(
        finding_ids(&report).contains(&"diff.names-only"),
        "{report:#}"
    );
    // It asked nothing and recorded no answer.
    assert_eq!(crate::idtree::snapshot(&d.dir), ids);
}

/// A declared text the engine stores under its own spelling is no change and
/// one text difference with both texts, exit 2; the declarations `pull`
/// writes have none.
pub fn a_respelled_text_is_a_text_difference_until_pulled(server: &str, pg: bool) {
    let (own, d) = fixture(
        server,
        pg,
        "livediff3028f",
        &["CREATE TABLE {s}.t (id int NOT NULL, \
           CONSTRAINT ck_t_id CHECK (id > 0 AND id < 10))"],
    );
    declare(
        &d,
        pg,
        "t",
        "columns:\n  id: {type: int, nullable: false}\nchecks:\n  ck_t_id: id > 0 AND id < 10\n",
    );
    let report = answer(&d, own.connection(), 2);
    assert_eq!(report["result"], "findings", "{report:#}");
    assert!(changes(&report).is_empty(), "{report:#}");
    let texts = report["data"]["text_differences"].as_array().unwrap();
    assert_eq!(texts.len(), 1, "{report:#}");
    assert!(texts[0]["object"].as_str().unwrap().contains("ck_t_id"));
    assert_eq!(texts[0]["declared"], "id > 0 AND id < 10");
    let stored = texts[0]["stored"].as_str().unwrap();
    assert!(
        stored != "id > 0 AND id < 10" && stored.contains("10"),
        "{stored}"
    );
    assert!(
        finding_ids(&report).contains(&"diff.text-differences"),
        "{report:#}"
    );
    let out = human(&d, own.connection(), 2);
    assert!(
        stdout(&out).contains("Text differences (1):"),
        "{}",
        stdout(&out)
    );
    assert!(stdout(&out).contains(stored), "{}", stdout(&out));

    let pulled = d.run(&["pull", "--db", own.connection(), "--force"]);
    assert_eq!(code(&pulled), 0, "{}{}", stdout(&pulled), stderr(&pulled));
    let report = answer(&d, own.connection(), 0);
    assert_eq!(report["data"]["text_differences"], serde_json::json!([]));
    assert!(changes(&report).is_empty(), "{report:#}");
}

/// Acceptance 6: a ledger changes nothing. The database pbps deployed and one
/// a person made the same way get the same answer, the ledger is not an
/// object of the comparison, and no record is written.
pub fn a_ledger_does_not_change_the_answer(server: &str, pg: bool) {
    let (deployed, d) = fixture(server, pg, "livediff3028g", &[]);
    declare(&d, pg, "t", ID_NAME);
    let planned = d.run(&["plan", "--no-dev"]);
    assert_eq!(
        code(&planned),
        0,
        "{}{}",
        stdout(&planned),
        stderr(&planned)
    );
    d.commit();
    let deploy = d.run(&["bootstrap", "--db", deployed.connection()]);
    assert_eq!(code(&deploy), 0, "{}{}", stdout(&deploy), stderr(&deploy));
    assert!(ledger_objects(deployed.connection(), pg) > 0);
    let recorded = records(deployed.connection(), pg);

    let report = answer(&d, deployed.connection(), 0);
    assert!(changes(&report).is_empty(), "{report:#}");
    assert_eq!(listed(&report, "unmanaged"), Vec::<String>::new());

    // Changed behind the ledger's back: the answer comes from the catalog.
    on_server(
        deployed.connection(),
        &format!("ALTER TABLE {}.t DROP COLUMN name", schema(pg)),
    );
    let with_ledger = answer(&d, deployed.connection(), 2);
    assert_eq!(changes(&with_ledger).changes.len(), 1, "{with_ledger:#}");

    let by_hand = OwnDatabase::new(server, "livediff3028h");
    if pg {
        on_server(by_hand.connection(), "CREATE SCHEMA app");
    }
    on_server(
        by_hand.connection(),
        &format!("CREATE TABLE {}.t (id int NOT NULL)", schema(pg)),
    );
    let without = answer(&d, by_hand.connection(), 2);
    for field in [
        "changes",
        "text_differences",
        "unexpressible",
        "not_compared",
        "unmanaged",
    ] {
        assert_eq!(
            with_ledger["data"][field], without["data"][field],
            "{field}"
        );
    }
    assert_eq!(ledger_objects(by_hand.connection(), pg), 0);
    assert_eq!(records(deployed.connection(), pg), recorded);
}

fn unreachable(pg: bool) -> &'static str {
    if pg {
        "host=127.0.0.1 port=9 user=unused dbname=unused sslmode=disable"
    } else {
        "Server=127.0.0.1,9;User Id=unused;Password=unused;TrustServerCertificate=true"
    }
}

/// Acceptance 7 to 9, with no database: a database that cannot be reached is
/// exit 1 and never "no differences"; declarations that do not load are
/// refused before any connection; and no option makes it write SQL or a plan.
pub fn without_a_database(pg: bool) {
    let d = Demo::new("livediff3028-offline");
    declare(&d, pg, "t", ID);

    let report = answer(&d, unreachable(pg), 1);
    assert_eq!(report["result"], "unanswerable", "{report:#}");
    assert_eq!(report["findings"][0]["id"], "environment.unreachable");
    assert!(report["data"].is_null(), "{report:#}");
    let out = human(&d, unreachable(pg), 1);
    assert!(
        !stdout(&out).contains("No differences."),
        "{}",
        stdout(&out)
    );
    assert!(!stderr(&out).is_empty());

    // The same unreachable address, so that an attempt to connect first
    // would be the finding.
    let d = Demo::new("livediff3028-invalid");
    declare(
        &d,
        pg,
        "t",
        "columns:\n  id: {type: int, nullable: maybe}\n",
    );
    let report = answer(&d, unreachable(pg), 1);
    assert_eq!(report["result"], "unanswerable", "{report:#}");
    assert!(!finding_ids(&report).is_empty(), "{report:#}");
    assert!(
        !finding_ids(&report).contains(&"environment.unreachable"),
        "{report:#}"
    );
    let out = human(&d, unreachable(pg), 1);
    assert!(stderr(&out).contains("t.yml"), "{}", stderr(&out));

    let d = Demo::new("livediff3028-options");
    declare(&d, pg, "t", ID);
    for option in ["--sql", "--out", "--plan"] {
        let file = d.dir.join("written");
        let out = d.run(&[
            "diff",
            "--db",
            unreachable(pg),
            option,
            file.to_str().unwrap(),
        ]);
        assert_ne!(code(&out), 0, "{option}: {}{}", stdout(&out), stderr(&out));
        assert!(
            stderr(&out).contains("unexpected argument"),
            "{option}: {}",
            stderr(&out)
        );
        assert!(!file.exists(), "{option}");
    }
    for flag in ["--apply", "--dev", "--no-dev", "--yes"] {
        let out = d.run(&["diff", "--db", unreachable(pg), flag]);
        assert!(
            stderr(&out).contains("unexpected argument"),
            "{flag}: {}",
            stderr(&out)
        );
    }
}
