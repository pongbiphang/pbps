#[path = "support/part_effect_cases.rs"]
mod corpus;

use pbps_model::part_effects::{DefaultChange, Effect, Origin, Part};
use std::collections::BTreeSet;

#[test]
fn the_equivalence_corpus_covers_every_change_kind() {
    let file = syn::parse_file(include_str!("../src/change.rs")).unwrap();
    let expected: BTreeSet<_> = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Enum(item) if item.ident == "Change" => {
                Some(item.variants.iter().map(|v| v.ident.to_string()).collect())
            }
            _ => None,
        })
        .unwrap();
    let cases = corpus::cases();
    let actual = cases
        .iter()
        .map(|(name, _)| name.split('/').next().unwrap().to_owned())
        .collect();
    assert_eq!(expected, actual);
    for (name, change) in cases {
        let kind = name.split('/').next().unwrap();
        let mut wire_name = String::new();
        for (at, ch) in kind.chars().enumerate() {
            if at > 0 && ch.is_ascii_uppercase() {
                wire_name.push('_');
            }
            wire_name.push(ch.to_ascii_lowercase());
        }
        assert_eq!(
            serde_json::to_value(&change).unwrap()["op"],
            wire_name,
            "{name}"
        );
        let _ = change.part_effects();
    }
}

#[test]
fn renaming_or_redefining_a_column_does_not_remove_a_part() {
    for (name, change) in corpus::cases().into_iter().filter(|(name, _)| {
        name.starts_with("Rename") || name.starts_with("AlterColumnNullability")
    }) {
        assert!(
            change
                .part_effects()
                .iter()
                .all(|e| e.effect != Effect::Removed),
            "{name}"
        );
    }
}

#[test]
fn inherited_presence_does_not_erase_the_own_default_transition() {
    let (_, change) = corpus::cases()
        .into_iter()
        .find(|(name, _)| name == "SetPartitionDefault/True-False-True")
        .unwrap();
    let effects = change.part_effects();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].effect, Effect::Rewritten);
    assert!(matches!(
        &effects[0].part,
        Part::Default {
            change: DefaultChange::Partition {
                from: Some(_),
                to: None,
                fallback: Some(_)
            },
            ..
        }
    ));
}

#[test]
fn contained_defaults_are_not_direct_restoration_steps() {
    let cases = corpus::cases();
    for name in ["CreateTable", "AddColumn/d", "CreateTable/partition"] {
        let change = &cases.iter().find(|(case, _)| case == name).unwrap().1;
        let effects = change.part_effects();
        let defaults: Vec<_> = effects
            .iter()
            .filter(|fact| matches!(fact.part, Part::Default { .. }))
            .collect();
        assert!(!defaults.is_empty(), "{name}");
        assert!(
            defaults
                .iter()
                .all(|fact| fact.origin == Origin::Contained && fact.effect == Effect::Created),
            "{name}"
        );
    }
    for name in ["AddColumn/i", "AddColumn/s", "AddColumn/n"] {
        let change = &cases.iter().find(|(case, _)| case == name).unwrap().1;
        assert!(
            change
                .part_effects()
                .iter()
                .all(|fact| !matches!(fact.part, Part::Default { .. } | Part::Generated(_))),
            "{name}"
        );
    }
}
