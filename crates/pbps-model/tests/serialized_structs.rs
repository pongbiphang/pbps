//! Persisted objects must refuse fields they cannot interpret. Serde's rule on
//! an outer artifact does not propagate to its nested structs (#109).

use std::collections::BTreeSet;
use std::path::Path;

use syn::punctuated::Punctuated;
use syn::{Attribute, Item, Meta, Token};

fn options(attrs: &[Attribute], attribute: &str) -> Vec<Meta> {
    attrs
        .iter()
        .filter(|a| a.path().is_ident(attribute))
        .flat_map(|a| {
            a.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .expect("valid attribute options")
        })
        .collect()
}

fn has_option(attrs: &[Attribute], option: &str) -> bool {
    options(attrs, "serde")
        .iter()
        .any(|m| m.path().is_ident(option))
}

fn inspect(path: &Path, checked: &mut BTreeSet<String>, exceptions: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            inspect(&path, checked, exceptions);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let source = std::fs::read_to_string(&path).unwrap();
            let file = syn::parse_file(&source).unwrap();
            inspect_items(&file.items, checked, exceptions);
        }
    }
}

fn inspect_items(
    items: &[Item],
    checked: &mut BTreeSet<String>,
    exceptions: &mut BTreeSet<String>,
) {
    for item in items {
        if let Item::Mod(module) = item
            && let Some((_, items)) = &module.content
        {
            inspect_items(items, checked, exceptions);
        }
        let Item::Struct(item) = item else { continue };
        if !matches!(item.vis, syn::Visibility::Public(_))
            || !options(&item.attrs, "derive").iter().any(|m| {
                m.path()
                    .segments
                    .last()
                    .is_some_and(|s| s.ident == "Deserialize")
            })
        {
            continue;
        }
        let name = item.ident.to_string();
        // These objects are output reports, not input to StateSnapshot,
        // SavedPlan or IdsFile. Explicit names keep new artifacts in the check.
        if matches!(name.as_str(), "DriftBaseline" | "DriftReport") {
            exceptions.insert(name);
            continue;
        }
        // A string-converted or transparent wrapper has no object field names
        // of its own. Its underlying parser owns the accepted representation.
        if has_option(&item.attrs, "try_from") || has_option(&item.attrs, "transparent") {
            continue;
        }
        // PlannedChange flattens Change, whose tagged deserializer rejects
        // leftovers. unknown_change_fields_are_refused exercises that boundary.
        if name == "PlannedChange" {
            assert!(item.fields.iter().any(|f| has_option(&f.attrs, "flatten")));
            exceptions.insert(name);
            continue;
        }
        assert!(
            has_option(&item.attrs, "deny_unknown_fields"),
            "{name} must refuse unknown fields"
        );
        checked.insert(name);
    }
}

#[test]
fn persisted_structs_refuse_unknown_fields() {
    let mut checked = BTreeSet::new();
    let mut exceptions = BTreeSet::new();
    inspect(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut checked,
        &mut exceptions,
    );
    for root in [
        "StateSnapshot",
        "SavedPlan",
        "IdsFile",
        "Role",
        "DataScope",
        "Finding",
    ] {
        assert!(checked.contains(root), "the sweep must reach {root}");
    }
    assert_eq!(
        exceptions,
        ["DriftBaseline", "DriftReport", "PlannedChange"]
            .map(str::to_owned)
            .into_iter()
            .collect()
    );
}

#[test]
#[should_panic(expected = "Misspelled must refuse unknown fields")]
fn a_new_permissive_struct_is_caught() {
    let file =
        syn::parse_file("#[derive(serde::Deserialize)] pub struct Misspelled { pub grants: u32 }")
            .unwrap();
    inspect_items(&file.items, &mut BTreeSet::new(), &mut BTreeSet::new());
}
