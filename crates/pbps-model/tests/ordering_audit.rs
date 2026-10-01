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

#[test]
fn every_change_kind_has_a_row_in_the_ordering_audit() {
    let kinds = change_kinds();
    assert!(
        kinds.len() > 30,
        "the sweep must reach the whole enum: {kinds:?}"
    );
    let table = requires_table();
    let missing: Vec<&String> = kinds
        .iter()
        // Named alone, or with a field pattern: `SetPrimaryKey { to: None }`.
        .filter(|k| !table.contains(&format!("`{k}`")) && !table.contains(&format!("`{k} {{")))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/ORDERING.md has no row for {missing:?}: classify the new kind's \
         interactions before it ships"
    );
}

/// Negative: a name the enum does not have is not found, so the check is not
/// passing on any text at all.
#[test]
fn a_kind_the_audit_does_not_name_is_reported() {
    let table = requires_table();
    assert!(!table.contains("`NoSuchChange`"));
    assert!(table.contains("`AlterColumnExpression`"));
}
