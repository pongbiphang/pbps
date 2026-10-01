//! The ordering audit names every kind of change (#1351). A new `Change`
//! variant without a row there is a kind whose interactions nobody has
//! classified, which is how ordering corners were found one review round at a
//! time.

use std::path::Path;

use syn::Item;

/// The variants of `pbps_model::Change`, read from its source rather than
/// listed here, so the list cannot fall behind the enum.
fn change_kinds() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/change.rs");
    let file = syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap();
    file.items
        .iter()
        .find_map(|item| {
            if let Item::Enum(e) = item
                && e.ident == "Change"
            {
                Some(
                    e.variants
                        .iter()
                        .map(|v| v.ident.to_string())
                        .collect::<Vec<_>>(),
                )
            } else {
                None
            }
        })
        .expect("`Change` is an enum in change.rs")
}

/// The requires table of `docs/ORDERING.md`: the lines between its heading
/// and the next one.
fn requires_table() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/ORDERING.md");
    let text = std::fs::read_to_string(path).unwrap();
    let start = text
        .find("## What each change requires")
        .expect("the audit has its requires table");
    let rest = &text[start + 3..];
    rest[..rest.find("\n## ").unwrap_or(rest.len())].to_owned()
}

/// The kinds named in the first cell of each row, alone or with a field
/// pattern (`SetPrimaryKey { to: None }`). A kind mentioned only in another
/// cell, or in prose, has no row.
fn row_kinds(table: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in table.lines().filter(|l| l.starts_with("| `")) {
        let first = line.split('|').nth(1).unwrap_or("");
        for named in first.split('`').skip(1).step_by(2) {
            let kind = named.split([' ', '{']).next().unwrap_or("");
            out.push(kind.to_owned());
        }
    }
    out
}

#[test]
fn every_change_kind_has_a_row_in_the_ordering_audit() {
    let kinds = change_kinds();
    assert!(
        kinds.len() > 30,
        "the sweep must reach the whole enum: {kinds:?}"
    );
    let named = row_kinds(&requires_table());
    let missing: Vec<&String> = kinds.iter().filter(|k| !named.contains(*k)).collect();
    assert!(
        missing.is_empty(),
        "docs/ORDERING.md has no row for {missing:?}: classify the new kind's \
         interactions before it ships"
    );
}

/// Negative: a kind named only in another row's cells, or in prose, is not a
/// row of its own.
#[test]
fn a_kind_named_outside_the_first_cell_has_no_row() {
    let table = "| Change | Class | Requires |\n\
                 |---|---|---|\n\
                 | `AddColumn` | 8 | `CreateTable` first |\n\
                 Prose naming `DropTable`.\n";
    let named = row_kinds(table);
    assert_eq!(named, ["AddColumn"]);
    assert!(row_kinds(&requires_table()).contains(&"AlterColumnExpression".to_owned()));
}
