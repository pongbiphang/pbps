//! Which changes create, drop or rename a database-global named object, and
//! that the classifier and the catalog family patterns pattern say the same thing
//! (#2447).

use pbps_model::ExtensionChange;
use pbps_model::GrantChange;
use pbps_model::RoleChange;
use pbps_model::SearchChange;
use pbps_model::TableChange;
use pbps_model::TypeChange;
use pbps_model::TypeValueChange;
use pbps_model::{CatalogObject, Change, GrantTarget, ManagedType, Permission};
#[path = "support/change_kinds.rs"]
mod change_kinds_of_file;

use std::path::Path;

fn enumeration() -> Box<ManagedType> {
    Box::new(ManagedType::Enum {
        values: vec!["a".into()],
    })
}

fn type_name() -> pbps_model::TypeRef {
    "app.mood".parse().unwrap()
}

fn search_name() -> pbps_model::SearchRef {
    "dictionary app.words".parse().unwrap()
}

fn dictionary() -> Box<pbps_model::SearchObject> {
    Box::new(pbps_model::SearchObject::Dictionary {
        template: "pg_catalog.simple".parse().unwrap(),
        options: Default::default(),
    })
}

fn catalog() -> Vec<(Change, CatalogObject)> {
    let role = |s: &str| pbps_model::RoleName::from(s);
    vec![
        (
            Change::Role(RoleChange::CreateRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: role("reader"),
            }),
            CatalogObject::Role,
        ),
        (
            Change::Role(RoleChange::DropRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: role("reader"),
                members: Vec::new(),
            }),
            CatalogObject::Role,
        ),
        (
            Change::Role(RoleChange::RenameRole {
                uid: "r_aaaaaa".parse().unwrap(),
                from: role("reader"),
                to: role("viewer"),
            }),
            CatalogObject::Role,
        ),
        (
            Change::Extension(ExtensionChange::CreateExtension {
                uid: "e_aaaaaa".parse().unwrap(),
                name: "citext".into(),
                definition: pbps_model::Extension {
                    schema: "ext".into(),
                    version: "1.6".into(),
                },
                requires: Default::default(),
            }),
            CatalogObject::Extension,
        ),
        (
            Change::Extension(ExtensionChange::DropExtension {
                uid: "e_aaaaaa".parse().unwrap(),
                name: "citext".into(),
                definition: pbps_model::Extension {
                    schema: "ext".into(),
                    version: "1.6".into(),
                },
                requirers: Default::default(),
                releases: Default::default(),
                index_releases: Default::default(),
                search_releases: Default::default(),
            }),
            CatalogObject::Extension,
        ),
        (
            Change::Extension(ExtensionChange::UpdateExtension {
                uid: "e_aaaaaa".parse().unwrap(),
                name: "citext".into(),
                from: pbps_model::Extension {
                    schema: "ext".into(),
                    version: "1.4".into(),
                },
                to: pbps_model::Extension {
                    schema: "ext".into(),
                    version: "1.6".into(),
                },
                requires: Default::default(),
                releases: Default::default(),
            }),
            CatalogObject::Extension,
        ),
        (
            Change::Extension(ExtensionChange::SetExtensionSchema {
                uid: "e_aaaaaa".parse().unwrap(),
                name: "citext".into(),
                from: pbps_model::Extension {
                    schema: "ext".into(),
                    version: "1.6".into(),
                },
                to: pbps_model::Extension {
                    schema: "app".into(),
                    version: "1.6".into(),
                },
                releases: Default::default(),
            }),
            CatalogObject::Extension,
        ),
        (
            Change::Type(TypeChange::CreateType {
                uid: "y_aaaaaa".parse().unwrap(),
                name: type_name(),
                definition: enumeration(),
            }),
            CatalogObject::Type,
        ),
        (
            Change::Type(TypeChange::DropType {
                uid: "y_aaaaaa".parse().unwrap(),
                name: type_name(),
                definition: enumeration(),
            }),
            CatalogObject::Type,
        ),
        (
            Change::Type(TypeChange::RenameType {
                uid: "y_aaaaaa".parse().unwrap(),
                from: type_name(),
                to: "app.feeling".parse().unwrap(),
                definition: enumeration(),
            }),
            CatalogObject::Type,
        ),
        (
            Change::Search(SearchChange::CreateSearch {
                uid: "s_aaaaaa".parse().unwrap(),
                name: search_name(),
                definition: dictionary(),
                unresolved: Vec::new(),
            }),
            CatalogObject::Search,
        ),
        (
            Change::Search(SearchChange::DropSearch {
                uid: "s_aaaaaa".parse().unwrap(),
                name: search_name(),
                definition: dictionary(),
            }),
            CatalogObject::Search,
        ),
    ]
}

/// Changes that act on a catalog object or are about one without creating,
/// dropping or renaming it. The enum edit is the near miss: it is a type's
/// change but not a type's identity.
fn others() -> Vec<Change> {
    vec![
        Change::Table(TableChange::DropTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: "app.t".parse().unwrap(),
            detach_from: None,
        }),
        Change::TypeValue(TypeValueChange::AddEnumValue {
            uid: "y_aaaaaa".parse().unwrap(),
            name: type_name(),
            value: "b".into(),
            placement: None,
            ordinal: 1,
            definition: enumeration(),
        }),
        Change::TypeValue(TypeValueChange::RenameEnumValue {
            uid: "y_aaaaaa".parse().unwrap(),
            name: type_name(),
            from: "a".into(),
            to: "b".into(),
            ordinal: 0,
            retained: Vec::new(),
            definition: enumeration(),
        }),
        Change::Permission(GrantChange::Grant {
            role: "reader".into(),
            target: GrantTarget::Schema("app".into()),
            permissions: [Permission::Usage].into(),
        }),
        Change::Permission(GrantChange::Revoke {
            role: "reader".into(),
            target: GrantTarget::Schema("app".into()),
            permissions: [Permission::Usage].into(),
        }),
    ]
}

#[test]
fn each_catalog_variant_maps_to_its_own_kind() {
    for (change, kind) in catalog() {
        assert_eq!(change.catalog_object(), Some(kind), "{change:?}");
    }
}

/// A package is named by its own subject, which plan groupings, policies and
/// refusals key on: not blank, and not a role's or a table's.
#[test]
fn a_create_names_its_package_as_the_subject() {
    for (change, kind) in catalog() {
        if kind == CatalogObject::Extension {
            assert_eq!(change.subject(), "extension citext");
        } else {
            assert!(!change.subject().starts_with("extension "), "{change:?}");
        }
    }
}

#[test]
fn a_change_that_is_not_a_catalog_object_maps_to_none() {
    for change in others() {
        assert_eq!(change.catalog_object(), None, "{change:?}");
    }
}

/// The pattern and the classifier are one list: a change matches
/// `Change::Role(_) | Change::Type(_) | Change::Search(_) | Change::Extension(_)` exactly when it has a kind, and `roles`/`types` split it
/// the way the classifier does.
#[test]
fn the_pattern_agrees_with_the_classifier_and_splits_by_kind() {
    for (change, kind) in catalog() {
        assert!(
            matches!(
                change,
                Change::Role(_) | Change::Type(_) | Change::Search(_) | Change::Extension(_)
            ),
            "{change:?}"
        );
        assert_eq!(
            matches!(change, Change::Role(_)),
            kind == CatalogObject::Role,
            "{change:?}"
        );
        assert_eq!(
            matches!(change, Change::Type(_)),
            kind == CatalogObject::Type,
            "{change:?}"
        );
        assert_eq!(
            matches!(change, Change::Search(_)),
            kind == CatalogObject::Search,
            "{change:?}"
        );
        assert_eq!(
            matches!(change, Change::Extension(_)),
            kind == CatalogObject::Extension,
            "{change:?}"
        );
    }
    for change in others() {
        assert!(
            !matches!(
                change,
                Change::Role(_) | Change::Type(_) | Change::Search(_) | Change::Extension(_)
            ),
            "{change:?}"
        );
        assert!(!matches!(change, Change::Role(_)), "{change:?}");
        assert!(!matches!(change, Change::Type(_)), "{change:?}");
        assert!(!matches!(change, Change::Search(_)), "{change:?}");
        assert!(!matches!(change, Change::Extension(_)), "{change:?}");
    }
}

/// Every variant is listed in exactly one family, where the kinds are defined,
/// and the two classifiers built from the families have no wildcard arm. The
/// compiler enforces both (a missing variant does not compile, a repeated one
/// is unreachable); this pins them against an `allow`, because both are
/// warning-level lints (DEC-2790.1).
#[test]
fn every_variant_is_listed_in_exactly_one_family() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/change.rs");
    let text = std::fs::read_to_string(path).unwrap();
    let file = syn::parse_file(&text).unwrap();
    let kinds = change_kinds_of_file::change_kinds(&file);
    let unique: std::collections::BTreeSet<_> = kinds.iter().collect();
    assert_eq!(
        kinds.len(),
        unique.len(),
        "a kind belongs to exactly one family"
    );
    assert!(kinds.len() > 3);
    let change = file
        .items
        .iter()
        .find_map(|item| {
            if let syn::Item::Enum(e) = item
                && e.ident == "Change"
            {
                Some(e)
            } else {
                None
            }
        })
        .unwrap();
    for classifier in ["pub fn catalog_object", "pub fn family"] {
        let start = text.find(classifier).unwrap();
        let body = &text[start..];
        let body = &body[..body.find("\n    }\n").unwrap()];
        assert!(
            !body.contains("_ =>"),
            "a wildcard arm in `{classifier}` hides new families"
        );
        for family in &change.variants {
            let pattern = format!("Change::{}(_)", family.ident);
            assert_eq!(
                body.matches(&pattern).count(),
                1,
                "`{classifier}` must decide family `{}` exactly once",
                family.ident
            );
        }
    }
}
