use super::{Demo, code, stderr, stdout, unexpressible};

/// #1966: two tables trading names through a third, over three revisions,
/// net to a swap no engine performs. The differ refuses it by name on every
/// path, before any file is written: an offline `--json` plan used to write
/// `--out` and `--sql` and only then fail on the address tracker, and a
/// connected plan refused it only as a name clash with a remedy that does not
/// serve a swap. `schema` holds tables `a` and `b`, adopted as the baseline
/// and committed.
pub fn swap(d: &Demo, connection: &str, schema: &str) {
    let name = |table: &str| format!("{schema}.{table}");
    let moved = |from: &str, to: &str| {
        let (from, to) = (name(from), name(to));
        let path = d.dir.join(format!("schema/{from}.yml"));
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(
            d.dir.join(format!("schema/{to}.yml")),
            text.replace(&format!("table: {from}"), &format!("table: {to}")),
        )
        .unwrap();
        let o = d.run(&["rename-table", &from, &to]);
        assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    };
    moved("a", "c");
    moved("b", "a");
    moved("c", "b");
    // Left uncommitted: an offline plan's baseline is git HEAD, the adopted
    // state, so it sees the netted swap as the connected plan does.

    // Connected and offline JSON, human output, and both plan files kept.
    unexpressible::check(
        d,
        connection,
        &["each need a name another of them still holds"],
    );
    let offline = d.run(&["--no-input", "plan", "--format", "json"]);
    for rename in [
        format!("`{} -> {}`", name("a"), name("b")),
        format!("`{} -> {}`", name("b"), name("a")),
    ] {
        assert!(stdout(&offline).contains(&rename), "{}", stdout(&offline));
    }
    // Offline with both files asked for: the refusal comes first.
    let plan = d.dir.join("swap-plan.json");
    let sql = d.dir.join("swap-plan.sql");
    let o = d.run(&[
        "--no-input",
        "plan",
        "--format",
        "json",
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(!plan.exists() && !sql.exists(), "{}", stdout(&o));
}
