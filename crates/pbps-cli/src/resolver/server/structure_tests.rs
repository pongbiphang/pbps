//! Files of the resolver that name no engine: a scan of their production code,
//! so a driver branch or an engine path cannot come back without a test saying
//! so. Comments may say why an engine differs; code may not choose by it.

/// The first engine mark in `source`'s production code, comments aside.
///
/// Code behind `#[cfg(test)]` is not production code. Such an item is skipped
/// whole, from its attribute to the closing brace at the margin (or to its `;`
/// for a one-line item such as `mod tests;`), rather than cutting the file at
/// the first one: `resolution.rs` has a test-only helper with production code
/// after it, which a cut there would leave unread.
fn engine_named(source: &str) -> Option<&'static str> {
    let mut code = String::new();
    // `Some(true)`: the item's first line is next; `Some(false)`: inside a
    // braced item.
    let mut skipping = None;
    for line in source.lines() {
        match skipping {
            Some(true) => {
                skipping = (!line.ends_with(';')).then_some(false);
                continue;
            }
            Some(false) => {
                skipping = (line != "}").then_some(false);
                continue;
            }
            None => {}
        }
        if line == "#[cfg(test)]" {
            skipping = Some(true);
            continue;
        }
        if !line.trim_start().starts_with("//") {
            code.push_str(line);
            code.push('\n');
        }
    }
    ["Driver", "pbps_pg", "pbps_mssql", "Postgres", "Mssql"]
        .into_iter()
        .find(|mark| code.contains(mark))
}

#[test]
fn the_surface_resolution_leaves_every_engine_decision_to_the_engine() {
    assert_eq!(engine_named(include_str!("resolution.rs")), None);
}

#[test]
fn the_transition_derivation_leaves_every_engine_decision_to_the_engine() {
    assert_eq!(engine_named(include_str!("transitions.rs")), None);
}

#[test]
fn the_scratch_run_leaves_every_engine_decision_to_the_engine() {
    assert_eq!(engine_named(include_str!("scratch.rs")), None);
}

#[test]
fn the_engine_scan_sees_a_driver_branch_and_an_engine_path() {
    for (source, mark) in [
        (
            "fn f() { match D::DRIVER { Driver::Mssql => {} } }",
            "Driver",
        ),
        (
            "fn f() { pbps_pg::resolver::binding_classes(s); }",
            "pbps_pg",
        ),
        (
            "fn f() { pbps_mssql::resolver::dependent_object(); }",
            "pbps_mssql",
        ),
        ("fn f() { run::<Postgres>(); }", "Postgres"),
        ("fn f() { Authorization::Mssql(c); }", "Mssql"),
        // A test-only item before production code does not hide what follows.
        (
            "#[cfg(test)]\nfn t() {\n    Driver::Mssql;\n}\nfn f() { Driver::Postgres; }",
            "Driver",
        ),
    ] {
        assert_eq!(engine_named(source), Some(mark), "{source}");
    }
    // A one-line test-only item does not swallow the production code after it.
    assert_eq!(
        engine_named("#[cfg(test)]\nmod live_tests;\nfn f() { Driver::Mssql; }"),
        Some("Driver")
    );
    // Comments and test-only items are not production code.
    assert_eq!(
        engine_named("// Driver::Mssql only in a comment\n/// pbps_pg too\nfn f() {}"),
        None
    );
    assert_eq!(
        engine_named("fn f() {}\n#[cfg(test)]\nfn t() {\n    pbps_pg::x();\n}\nfn g() {}"),
        None
    );
}
