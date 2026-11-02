//! Which changes create, drop or rename a database-global named object, and
//! that the classifier and the `catalog_change!` pattern say the same thing
//! (#2447).

use pbps_model::{CatalogObject, Change, GrantTarget, ManagedType, Permission, catalog_change};
use std::path::Path;
use syn::Item;

fn enumeration() -> Box<ManagedType> {
    Box::new(ManagedType::Enum {
        values: vec!["a".into()],
    })
}

fn type_name() -> pbps_model::TypeRef {
    "app.mood".parse().unwrap()
}

fn catalog() -> Vec<(Change, CatalogObject)> {
    let role = |s: &str| pbps_model::RoleName::from(s);
    vec![
        (
            Change::CreateRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: role("reader"),
            },
            CatalogObject::Role,
        ),
        (
            Change::DropRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: role("reader"),
                members: Vec::new(),
            },
            CatalogObject::Role,
        ),
        (
            Change::RenameRole {
                uid: "r_aaaaaa".parse().unwrap(),
                from: role("reader"),
                to: role("viewer"),
            },
            CatalogObject::Role,
        ),
        (
            Change::CreateType {
                uid: "y_aaaaaa".parse().unwrap(),
                name: type_name(),
                definition: enumeration(),
            },
            CatalogObject::Type,
        ),
        (
            Change::DropType {
                uid: "y_aaaaaa".parse().unwrap(),
                name: type_name(),
                definition: enumeration(),
            },
            CatalogObject::Type,
        ),
        (
            Change::RenameType {
                uid: "y_aaaaaa".parse().unwrap(),
                from: type_name(),
                to: "app.feeling".parse().unwrap(),
                definition: enumeration(),
            },
            CatalogObject::Type,
        ),
    ]
}

/// Changes that act on a catalog object or are about one without creating,
/// dropping or renaming it. The enum edit is the near miss: it is a type's
/// change but not a type's identity.
fn others() -> Vec<Change> {
    vec![
        Change::DropTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: "app.t".parse().unwrap(),
            detach_from: None,
        },
        Change::AddEnumValue {
            uid: "y_aaaaaa".parse().unwrap(),
            name: type_name(),
            value: "b".into(),
            placement: None,
            ordinal: 1,
            definition: enumeration(),
        },
        Change::RenameEnumValue {
            uid: "y_aaaaaa".parse().unwrap(),
            name: type_name(),
            from: "a".into(),
            to: "b".into(),
            ordinal: 0,
            retained: Vec::new(),
            definition: enumeration(),
        },
        Change::Grant {
            role: "reader".into(),
            target: GrantTarget::Schema("app".into()),
            permissions: [Permission::Usage].into(),
        },
        Change::Revoke {
            role: "reader".into(),
            target: GrantTarget::Schema("app".into()),
            permissions: [Permission::Usage].into(),
        },
    ]
}

#[test]
fn each_catalog_variant_maps_to_its_own_kind() {
    for (change, kind) in catalog() {
        assert_eq!(change.catalog_object(), Some(kind), "{change:?}");
    }
}

#[test]
fn a_change_that_is_not_a_catalog_object_maps_to_none() {
    for change in others() {
        assert_eq!(change.catalog_object(), None, "{change:?}");
    }
}

/// The pattern and the classifier are one list: a change matches
/// `catalog_change!()` exactly when it has a kind, and `roles`/`types` split it
/// the way the classifier does.
#[test]
fn the_pattern_agrees_with_the_classifier_and_splits_by_kind() {
    for (change, kind) in catalog() {
        assert!(matches!(change, catalog_change!()), "{change:?}");
        assert_eq!(
            matches!(change, catalog_change!(roles)),
            kind == CatalogObject::Role,
            "{change:?}"
        );
        assert_eq!(
            matches!(change, catalog_change!(types)),
            kind == CatalogObject::Type,
            "{change:?}"
        );
    }
    for change in others() {
        assert!(!matches!(change, catalog_change!()), "{change:?}");
        assert!(!matches!(change, catalog_change!(roles)), "{change:?}");
        assert!(!matches!(change, catalog_change!(types)), "{change:?}");
    }
}

/// The classifier names every variant itself, so a new one is a decision made
/// where the kinds are defined. The compiler enforces that (no wildcard); this
/// pins it against someone adding one, because a wildcard is a warning level
/// lint that an `allow` could silence.
#[test]
fn the_classifier_names_every_variant_not_a_catalog_object() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/change.rs");
    let text = std::fs::read_to_string(path).unwrap();
    let file = syn::parse_file(&text).unwrap();
    let variants: Vec<String> = file
        .items
        .iter()
        .find_map(|item| {
            if let Item::Enum(e) = item
                && e.ident == "Change"
            {
                Some(e.variants.iter().map(|v| v.ident.to_string()).collect())
            } else {
                None
            }
        })
        .expect("`Change` is an enum in change.rs");
    let start = text.find("pub fn catalog_object").unwrap();
    let body = &text[start..text[start..].find("\n    }\n").map(|n| start + n).unwrap()];
    assert!(!body.contains("_ =>"), "a wildcard arm hides new variants");
    let catalog = [
        "CreateRole",
        "DropRole",
        "RenameRole",
        "CreateType",
        "DropType",
        "RenameType",
    ];
    for variant in &variants {
        let named = body.contains(&format!("Change::{variant} "))
            || body.contains(&format!("Change::{variant}("));
        // A catalog variant is named by the pattern macro, not here.
        assert_eq!(
            named,
            !catalog.contains(&variant.as_str()),
            "`{variant}` must be named in the classifier, except the catalog ones"
        );
    }
    assert!(variants.len() > catalog.len());
}
