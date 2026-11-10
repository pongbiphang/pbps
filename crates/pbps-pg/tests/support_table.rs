//! The support table (`pbps_dialect::support`) against the code it describes
//! and the document rendered from it (#2717, DEC-2717.1).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pbps_dialect::support::{self, Engine, Tier};

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

/// The kinds the census query names: the string literals of its `objects`
/// list that are not catalog names or one-character catalog codes.
fn census_kinds(source: &str) -> BTreeSet<String> {
    let start = source
        .find("objects(classid, oid, kind, model_owned) AS (")
        .expect("the census query lists its objects in one CTE");
    let end = source[start..]
        .find("SELECT o.kind")
        .expect("the census query selects from that CTE");
    // A comment line of the query is prose, and its apostrophes are not quotes.
    let sql = source[start..start + end]
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    sql.split('\'')
        .skip(1)
        .step_by(2)
        .filter(|literal| literal.len() > 1 && !literal.starts_with("pg_catalog."))
        .map(str::to_owned)
        .collect()
}

/// What the table says the census names.
fn recorded_census_kinds() -> BTreeSet<String> {
    support::records(Engine::Postgres)
        .flat_map(|r| r.census.iter().map(|k| (*k).to_owned()))
        .collect()
}

/// A kind the census query names and the table does not, or the reverse.
fn census_disagreements(source: &str) -> Vec<String> {
    let named = census_kinds(source);
    let recorded = recorded_census_kinds();
    named
        .symmetric_difference(&recorded)
        .map(|kind| {
            if named.contains(kind) {
                format!("the census names `{kind}` and no record does")
            } else {
                format!("a record names `{kind}` and the census does not")
            }
        })
        .collect()
}

#[test]
fn every_kind_the_census_names_has_a_record() {
    let source = std::fs::read_to_string(repo("crates/pbps-pg/src/catalog/census.rs")).unwrap();
    assert!(census_kinds(&source).len() > 20, "the query was not read");
    assert_eq!(census_disagreements(&source), Vec::<String>::new());
    // A census kind is named or partly held, never anything else.
    for record in support::records(Engine::Postgres).filter(|r| !r.census.is_empty()) {
        assert!(
            matches!(record.tier, Tier::Named | Tier::Partly),
            "{record:?}"
        );
    }
}

/// Negative: a kind the query gains, and a record the query no longer backs.
#[test]
fn a_census_kind_without_a_record_is_reported_and_so_is_the_reverse() {
    let source = std::fs::read_to_string(repo("crates/pbps-pg/src/catalog/census.rs")).unwrap();
    let gained = source.replacen(
        "'publication', false",
        "'publication', false FROM x UNION ALL SELECT c, oid, 'access method', false",
        1,
    );
    assert_eq!(
        census_disagreements(&gained),
        ["the census names `access method` and no record does"]
    );
    let lost = source.replacen("'cast', false", "'x', false", 1);
    assert_eq!(
        census_disagreements(&lost),
        ["a record names `cast` and the census does not"]
    );
}

/// The leading keys the loader accepts, read from the message it gives a file
/// with none of them.
fn loader_keys() -> Vec<String> {
    let errors = pbps_load::load_file_str(Path::new("x.yml"), "nothing: here\n")
        .expect_err("a file with no leading key is refused");
    let message = errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    message
        .split('`')
        .skip(1)
        .step_by(2)
        .filter_map(|quoted| quoted.strip_suffix(':'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_declaration_the_loader_accepts_is_held_by_an_engine() {
    let keys = loader_keys();
    assert_eq!(keys, support::DECLARATIONS, "the loader's leading keys");
    for key in &keys {
        let held = [Engine::Postgres, Engine::SqlServer]
            .into_iter()
            .filter_map(|engine| support::declared_tier(engine, key))
            .collect::<Vec<_>>();
        assert!(!held.is_empty(), "no engine has a record for `{key}:`");
        assert!(
            held.iter()
                .all(|tier| matches!(tier, Tier::Managed | Tier::Partly)),
            "`{key}:` is declared, so it is managed or partly held: {held:?}"
        );
    }
    // The other direction: a record declares nothing the loader does not load.
    for record in support::RECORDS {
        for key in record.declared_as {
            assert!(keys.iter().any(|k| k == key), "{record:?}");
        }
    }
    // Negative: a key no engine declares, and an engine that refuses one.
    assert_eq!(support::declared_tier(Engine::Postgres, "sequence"), None);
    assert_eq!(support::declared_tier(Engine::SqlServer, "enum"), None);
    assert_eq!(
        support::declared_tier(Engine::Postgres, "enum"),
        Some(Tier::Managed)
    );
}

/// `text` with each generated region replaced by its table.
fn regenerated(text: &str) -> String {
    let mut out = text.to_owned();
    for (name, table) in support::status_tables() {
        let open = format!("<!-- generated:{name} -->\n");
        let close = format!("<!-- /generated:{name} -->");
        let start = out
            .find(&open)
            .unwrap_or_else(|| panic!("docs/STATUS.md has no `{name}` region"))
            + open.len();
        let end = start
            + out[start..]
                .find(&close)
                .unwrap_or_else(|| panic!("the `{name}` region is not closed"));
        out.replace_range(start..end, &table);
    }
    out
}

#[test]
fn the_status_tables_are_the_ones_the_records_render() {
    let path = repo("docs/STATUS.md");
    let text = std::fs::read_to_string(&path).unwrap();
    let wanted = regenerated(&text);
    if std::env::var_os("PBPS_REGENERATE_STATUS").is_some() {
        std::fs::write(&path, &wanted).unwrap();
        return;
    }
    assert!(
        text == wanted,
        "docs/STATUS.md differs from pbps_dialect::support; run\n  \
         PBPS_REGENERATE_STATUS=1 cargo test -p pbps-pg --test support_table\n\
         and commit the result"
    );
}

/// Negative: a row edited by hand, and a row removed, are both differences.
#[test]
fn a_hand_edited_status_table_is_a_difference() {
    let text = std::fs::read_to_string(repo("docs/STATUS.md")).unwrap();
    let edited = text.replacen("| Publications |", "| Publications, by hand |", 1);
    assert_ne!(edited, text, "the census table has a publications row");
    assert_eq!(regenerated(&edited), text);
    let row = "| `brin` indexes | named | #2449 |\n";
    let removed = text.replacen(row, "", 1);
    assert_ne!(removed, text);
    assert_eq!(regenerated(&removed), text);
}
