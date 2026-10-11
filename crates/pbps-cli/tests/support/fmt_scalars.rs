use super::{Demo, code, stderr, stdout};
use pbps_model::{Change, Row, RowKey, SavedPlan, Value};

fn inserted_rows(d: &Demo, filename: &str) -> Vec<(RowKey, Row)> {
    let path = d.dir.join(filename);
    let out = d.run(&["plan", "--no-dev", "--out", path.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    let plan: SavedPlan = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    plan.changes
        .changes
        .into_iter()
        .filter_map(|c| {
            if let Change::InsertRow { key, row, .. } = c.change {
                Some((key, row))
            } else {
                None
            }
        })
        .collect()
}

pub fn check_numeric(dialect: &str) {
    let d = Demo::new("fmtnumeric1801");
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    let boolean = if dialect == "postgres" {
        "boolean"
    } else {
        "bit"
    };
    let values = [
        "0x1f",
        "0X1F",
        "-0x1f",
        "+0X1F",
        "0o17",
        "0O17",
        "-0o17",
        "+0O17",
        "0b101",
        "0B101",
        "-0b101",
        "+0b1",
        "1_000",
        "-1_0",
        "+1_000",
        "-0X1F",
        "+0x1f",
        "-0O17",
        "+0o17",
        "-0B101",
        "+0B101",
        "+.INF",
        "-.INF",
        ".NAN",
        "+.nan",
        "-.nan",
        "+.NaN",
        "-.NaN",
        ".inf",
        ".INF",
        "+.inf",
        "-.inf",
        ".nan",
        ".NaN",
        "001",
        "1e3",
        "2026-10-11",
        "12:30:00",
        "yes",
        "null",
        "~",
    ];
    let mut yaml = format!(
        "table: {schema}.t\ncolumns:\n  id: {{type: bigint, nullable: false}}\n  label: {{type: varchar(100)}}\n  amount: {{type: int}}\n  enabled: {{type: {boolean}}}\n  absent: {{type: varchar(100)}}\nprimary_key: [id]\ndata:\n  mode: exact\n  rows:\n"
    );
    for (i, value) in values.iter().enumerate() {
        yaml.push_str(&format!(
            "    '{}': {{label: {}, amount: 31, enabled: true, absent: null}}\n",
            i + 1,
            serde_json::to_string(value).unwrap()
        ));
    }
    d.table(&yaml);
    let before = inserted_rows(&d, "before.json");
    assert_eq!(before.len(), values.len());
    for (i, value) in values.iter().enumerate() {
        let (_, row) = before
            .iter()
            .find(|(key, _)| key.0 == (i + 1).to_string())
            .unwrap();
        assert_eq!(row.get("label"), Some(&Value::Text((*value).into())));
        assert_eq!(row.get("amount"), Some(&Value::Int(31)));
        assert_eq!(row.get("enabled"), Some(&Value::Bool(true)));
        assert_eq!(row.get("absent"), Some(&Value::Null));
    }
    for args in [
        &["validate"][..],
        &["fmt"],
        &["validate"],
        &["fmt", "--check"],
    ] {
        let out = d.run(args);
        assert_eq!(code(&out), 0, "{args:?}: {}{}", stdout(&out), stderr(&out));
    }
    assert_eq!(inserted_rows(&d, "after.json"), before);
}

pub fn check(dialect: &str) {
    let d = Demo::new("fmtscalars1795");
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    let mut yaml = format!(
        "table: {schema}.t\ndescription: |-\n  First table line\n  Second table line\ncolumns:\n  id: {{type: bigint, nullable: false}}\n  name:\n    type: varchar(100)\n    description: |-\n      First column line\n      Second column line\n    default: |-\n      'first\n      second'\nprimary_key: [id]\nchecks:\n  ck: |-\n    id >\n      0\ndata:\n  mode: exact\n  rows:\n    '1':\n      name: |-\n        First line\n        Second line\n"
    );
    let values = [
        "First line\nSecond line",
        "a\rb",
        "a\r\nb",
        "a\tb",
        " leading and trailing ",
        "a\"b\\c",
        "literal\\n",
        "0123",
        "no",
    ];
    for (i, value) in values.iter().enumerate().skip(1) {
        yaml.push_str(&format!(
            "    '{}': {{name: {}}}\n",
            i + 1,
            serde_json::to_string(value).unwrap()
        ));
    }
    d.table(&yaml);
    let path = d.dir.join(format!("schema/{schema}.t.yml"));
    let loaded = pbps_load::load_table_file(&path).unwrap();
    let before = inserted_rows(&d, "before.json");
    assert_eq!(before.len(), values.len());
    for (i, value) in values.iter().enumerate() {
        let row = before
            .iter()
            .find(|(key, _)| key.0 == (i + 1).to_string())
            .unwrap();
        assert_eq!(row.1.get("name"), Some(&Value::Text((*value).into())));
    }
    for args in [&["validate"][..], &["fmt"], &["validate"]] {
        let out = d.run(args);
        assert_eq!(code(&out), 0, "{args:?}: {}{}", stdout(&out), stderr(&out));
    }
    assert_eq!(inserted_rows(&d, "after.json"), before);
    assert_eq!(pbps_load::load_table_file(&path).unwrap(), loaded);
    let out = d.run(&["fmt", "--check"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));

    // Description escaping must not rewrite SQL's separate literal-block path.
    for (file, source) in [
        (
            "view.yml",
            format!(
                "view: {schema}.v\ndescription: |-\n  first\n  second\ndefinition: |-\n  SELECT 'first  \n  second' AS note\n"
            ),
        ),
        (
            "role.yml",
            "role: reader\ndescription: |-\n  first\n  second\n".into(),
        ),
    ] {
        let path = d.dir.join("schema").join(file);
        std::fs::write(&path, source).unwrap();
        let before = pbps_load::load_file(&path).unwrap();
        let out = d.run(&["fmt"]);
        assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        assert_eq!(pbps_load::load_file(&path).unwrap(), before);
    }
    for args in [&["validate"][..], &["fmt", "--check"]] {
        let out = d.run(args);
        assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    }

    let bad = Demo::new("fmtmalformed1795");
    let malformed = format!("table: {schema}.t\ncolumns: [\n");
    bad.table(&malformed);
    let out = bad.run(&["fmt", "--format", "json"]);
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "load.yaml"),
        "{report}"
    );
    assert_eq!(
        std::fs::read_to_string(bad.dir.join(format!("schema/{schema}.t.yml"))).unwrap(),
        malformed
    );
}
