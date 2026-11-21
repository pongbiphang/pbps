use super::*;
use crate::identity::Context;
use pbps_dialect::MinimalDialect;
use pbps_model::SearchChange;
use pbps_model::{Column, Intent, SearchObject, SearchRef};

fn ctx() -> Context {
    Context {
        operator: "leon".into(),
        today: "2026-11-11".into(),
    }
}

fn name(text: &str) -> SearchRef {
    text.parse().unwrap()
}

fn words() -> SearchObject {
    SearchObject::Dictionary {
        template: "pg_catalog.simple".parse().unwrap(),
        options: Default::default(),
    }
}

fn mapping(dictionary: &str) -> SearchObject {
    SearchObject::Config {
        parser: "pg_catalog.\"default\"".parse().unwrap(),
        mappings: [("asciiword".to_owned(), vec![dictionary.parse().unwrap()])].into(),
    }
}

/// The schema with a configuration `app.docs` over `app.words`, whose subjects sort the
/// other way (`config` before `dictionary`), and a table that follows them both.
fn world() -> Schema {
    let mut schema = Schema::default();
    schema
        .search
        .insert(name("config app.docs"), mapping("app.words"));
    schema.search.insert(name("dictionary app.words"), words());
    schema.tables.insert(
        "app.t".parse().unwrap(),
        Table {
            columns: [("body".into(), Column::new("text".parse().unwrap()))]
                .into_iter()
                .collect(),
            ..Table::default()
        },
    );
    schema
}

fn plan(
    base: (&Schema, &IdsFile),
    declared: (&Schema, &IdsFile),
) -> Result<ChangeSet, Vec<DiffError>> {
    plan_against(base, declared, false)
}

/// `target_read` is a connected plan: the base is what the database holds.
fn plan_against(
    base: (&Schema, &IdsFile),
    declared: (&Schema, &IdsFile),
    target_read: bool,
) -> Result<ChangeSet, Vec<DiffError>> {
    diff(
        Side {
            schema: base.0,
            ids: base.1,
        },
        Side {
            schema: declared.0,
            ids: declared.1,
        },
        &MinimalDialect,
        &Hints {
            target_read,
            ..Hints::default()
        },
    )
    .map(|plan| plan.changes().clone())
}

fn positions(changes: &ChangeSet, wanted: &[&str]) -> Vec<usize> {
    wanted
        .iter()
        .map(|text| {
            changes
                .changes
                .iter()
                .position(|p| p.change.subject() == *text)
                .unwrap_or_else(|| panic!("{text} is not planned: {changes:?}"))
        })
        .collect()
}

/// A dictionary is created before the configuration that maps it though the subjects sort
/// the other way, and both before the table, in class 7. The drop is the reverse, in
/// class 18, after the table's. Negative: neither is planned when the target holds them.
#[test]
fn a_dictionary_is_created_first_and_dropped_last() {
    let declared = world();
    let declared_ids = crate::resolve(&declared, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let empty = (&Schema::default(), &IdsFile::default());
    let created = plan(empty, (&declared, &declared_ids)).unwrap();
    let at = positions(
        &created,
        &["dictionary app.words", "config app.docs", "app.t"],
    );
    assert!(at[0] < at[1] && at[1] < at[2], "{created:?}");
    assert!(
        plan((&declared, &declared_ids), (&declared, &declared_ids))
            .unwrap()
            .changes
            .is_empty()
    );
    // Dropped: the configuration, then the dictionary, after the table.
    let bare = Schema {
        tables: declared.tables.clone(),
        ..Schema::default()
    };
    let mut kept = declared_ids.clone();
    kept.search.clear();
    let dropped = plan((&declared, &declared_ids), (&bare, &kept)).unwrap();
    let at = positions(&dropped, &["config app.docs", "dictionary app.words"]);
    assert!(at[0] < at[1], "{dropped:?}");
}

/// A drop needs a recorded reason, and with one it is planned and the identity becomes a
/// tombstone. Negative: an intent that names nothing is not accepted for it.
#[test]
fn a_text_search_drop_without_its_recorded_reason_is_refused() {
    let declared = world();
    let ids = crate::resolve(&declared, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let base = Side {
        schema: &declared,
        ids: &ids,
    };
    let none = Schema {
        tables: declared.tables.clone(),
        ..Schema::default()
    };
    let refused =
        crate::identity::resolve_with_baseline(&none, &ids, &[], 0, base, &ctx()).unwrap_err();
    assert!(
        refused
            .iter()
            .filter(|b| matches!(b, crate::Blocker::DropSearchNeedsReason { .. }))
            .count()
            == 2,
        "{refused:?}"
    );
    let reason: pbps_model::Reason = "no longer searched".parse().unwrap();
    let intents: Vec<Intent> = ["config app.docs", "dictionary app.words"]
        .into_iter()
        .map(|text| Intent::DropSearch {
            name: name(text),
            reason: reason.clone(),
        })
        .collect();
    let resolved =
        crate::identity::resolve_with_baseline(&none, &ids, &intents, 0, base, &ctx()).unwrap();
    assert_eq!(resolved.dropped_search.len(), 2);
    assert!(resolved.ids.search.is_empty());
    assert_eq!(resolved.ids.tombstones.len(), 2);
    let wrong = [Intent::DropSearch {
        name: name("config app.other"),
        reason,
    }];
    assert!(
        crate::identity::resolve(&declared, &ids, &wrong, &ctx()).is_err(),
        "an intent that matches nothing is an unused intent"
    );
}

/// A table's missing reason and a search object's are one answer: a connected plan names
/// both remedies at once, as the checkout does, and does not make the user plan again to
/// learn the second. Negative: the table is named once, not once per source.
#[test]
fn a_connected_plan_names_the_table_and_the_search_remedies_together() {
    let declared = world();
    let ids = crate::resolve(&declared, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let base = Side {
        schema: &declared,
        ids: &ids,
    };
    let refused =
        crate::identity::resolve_with_baseline(&Schema::default(), &ids, &[], 0, base, &ctx())
            .unwrap_err();
    let count = |f: fn(&crate::Blocker) -> bool| refused.iter().filter(|b| f(b)).count();
    assert_eq!(
        count(|b| matches!(b, crate::Blocker::DropTableNeedsReason { .. })),
        declared.tables.len(),
        "{refused:?}"
    );
    assert_eq!(
        count(|b| matches!(b, crate::Blocker::DropSearchNeedsReason { .. })),
        2,
        "{refused:?}"
    );
}

/// A reference resolves only to a dictionary the plan keeps. The target holding one the
/// plan drops is no answer: the create runs before the drop (class 7, class 18), and
/// `DROP TEXT SEARCH ... RESTRICT` fails while anything maps it, so a plan that adds a
/// configuration over it, or leaves one standing over it, is refused at plan by name, offline
/// and connected alike. Negative: the same configuration over a dictionary the plan keeps is
/// planned.
#[test]
fn a_mapping_to_a_dictionary_the_plan_drops_is_refused_and_one_it_keeps_is_not() {
    let dictionary = name("dictionary app.words");
    let docs = name("config app.docs");
    let mut base = Schema::default();
    base.search.insert(dictionary.clone(), words());
    let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    // The dictionary is given up and a new configuration maps it.
    let mut declared = Schema::default();
    declared.search.insert(docs.clone(), mapping("app.words"));
    let declared_ids = crate::resolve(&declared, &base_ids, &[], &ctx())
        .unwrap()
        .ids;
    for target_read in [false, true] {
        let refused =
            plan_against((&base, &base_ids), (&declared, &declared_ids), target_read).unwrap_err();
        let said: Vec<String> = refused.iter().map(ToString::to_string).collect();
        assert!(
            said.len() == 1
                && said[0].contains("app.docs")
                && said[0].contains("app.words")
                && said[0].contains("drops"),
            "{target_read}: {said:?}"
        );
    }
    // A configuration that stands over it is refused the same way.
    let mut standing = base.clone();
    standing.search.insert(docs.clone(), mapping("app.words"));
    let standing_ids = crate::resolve(&standing, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let mut kept_config = Schema::default();
    kept_config
        .search
        .insert(docs.clone(), mapping("app.words"));
    let mut ids = standing_ids.clone();
    let words_uid = ids.search_uid(&dictionary).unwrap().clone();
    ids.search.remove(&words_uid);
    let refused = plan_against((&standing, &standing_ids), (&kept_config, &ids), true).unwrap_err();
    assert!(
        refused
            .iter()
            .any(|e| e.to_string().contains("app.words") && e.to_string().contains("drops")),
        "{refused:?}"
    );
    // Negative: the dictionary kept (declared, with its identity) and a new configuration
    // over it plan fine.
    let mut kept = Schema::default();
    kept.search.insert(dictionary.clone(), words());
    kept.search.insert(docs.clone(), mapping("app.words"));
    let kept_ids = crate::resolve(&kept, &base_ids, &[], &ctx()).unwrap().ids;
    for target_read in [false, true] {
        let created = plan_against((&base, &base_ids), (&kept, &kept_ids), target_read).unwrap();
        assert!(
            created.changes.iter().any(
                |p| matches!(&p.change, Change::Search(SearchChange::CreateSearch { name, .. }) if *name == docs)
            ),
            "{created:?}"
        );
    }
}

/// An object retired and declared again before the drop reached the target is refused by
/// name, as a package is: resolution gives the new declaration a new identity and keeps the
/// old one's tombstone, and an equal definition would skip the create while the drop of the
/// old identity still came (ready round 1). Offline and connected alike. Negative: the same
/// declaration under its recorded identity plans nothing, and a name the target holds with
/// no recorded identity at all is still adopted.
#[test]
fn an_object_retired_and_declared_again_before_its_drop_lands_is_refused() {
    let dictionary = name("dictionary app.words");
    let mut base = Schema::default();
    base.search.insert(dictionary.clone(), words());
    let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let old = base_ids.search_uid(&dictionary).unwrap().clone();
    let again = |uid: &str| {
        let mut ids = IdsFile::default();
        ids.search.insert(uid.parse().unwrap(), dictionary.clone());
        ids.tombstones.insert(
            old.clone(),
            pbps_model::Tombstone {
                require: None,
                was: dictionary.raw().to_string(),
                dropped_at: "2026-11-12".into(),
                reason: "retired".into(),
                operator: "test".into(),
            },
        );
        ids
    };
    for target_read in [false, true] {
        let refused =
            plan_against((&base, &base_ids), (&base, &again("s_zzzzzz")), target_read).unwrap_err();
        assert!(
            refused.iter().any(|e| {
                let said = e.to_string();
                said.contains("app.words") && said.contains("declared again")
            }),
            "{target_read}: {refused:?}"
        );
    }
    // Negative: the recorded identity itself plans nothing.
    let same = plan_against((&base, &base_ids), (&base, &base_ids), true).unwrap();
    assert!(same.is_empty(), "{same:?}");
    // Negative: an object the target holds with no recorded identity is adopted.
    let adopted = plan_against(
        (&base, &IdsFile::default()),
        (&base, &again("s_zzzzzz")),
        true,
    )
    .unwrap();
    assert!(
        !adopted
            .changes
            .iter()
            .any(|p| matches!(p.change, Change::Search(SearchChange::DropSearch { .. }))),
        "{adopted:?}"
    );
}

/// A configuration over a package's dictionary, with the package given up, is refused when
/// planned against the database: the package is no longer declared, so its schema resolves
/// nothing. (Retiring the configuration with it is the release rule's, DEC-2728.2.)
#[test]
fn a_mapping_to_the_dictionary_of_a_dropped_package_is_refused_against_the_target() {
    let mut declared = Schema::default();
    declared
        .search
        .insert(name("config app.docs"), mapping("ext.intdict"));
    let ids = crate::resolve(&declared, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let refused = plan_against(
        (&Schema::default(), &IdsFile::default()),
        (&declared, &ids),
        true,
    )
    .unwrap_err();
    assert!(
        refused
            .iter()
            .any(|e| e.to_string().contains("ext.intdict")),
        "{refused:?}"
    );
}

/// Against the database, a mapping to a dictionary neither declared nor held by the
/// target is refused at plan by name; one in `pg_catalog` is the engine's and is not.
#[test]
fn a_mapping_to_a_missing_dictionary_is_refused_against_the_target() {
    let mut declared = Schema::default();
    declared
        .search
        .insert(name("config app.docs"), mapping("app.nope"));
    declared
        .search
        .insert(name("config app.fine"), mapping("pg_catalog.simple"));
    let ids = crate::resolve(&declared, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let errors = plan_against(
        (&Schema::default(), &IdsFile::default()),
        (&declared, &ids),
        true,
    )
    .unwrap_err();
    let said: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].contains("app.docs") && said[0].contains("app.nope"),
        "{said:?}"
    );
    // The target holding the dictionary is the one thing that settles it.
    let mut held = Schema::default();
    held.search.insert(name("dictionary app.nope"), words());
    let held_ids = IdsFile::default();
    let created = plan_against((&held, &held_ids), (&declared, &ids), true).unwrap();
    assert!(
        created.changes.iter().all(|p| matches!(
            &p.change,
            Change::Search(SearchChange::CreateSearch { unresolved, .. }) if unresolved.is_empty()
        )),
        "{created:?}"
    );
}

/// Offline the baseline only stands for the target, and a dictionary made on the database
/// outside pbps is not in it: the plan goes on and the create names what it could not
/// check, for the plan against the database to settle (DEC-2728.2). Negative: a
/// dictionary the project declares and the engine's own are checked and
/// leave nothing to say.
#[test]
fn a_mapping_to_an_unknown_dictionary_is_an_advisory_offline() {
    let mut declared = Schema::default();
    declared
        .search
        .insert(name("config app.docs"), mapping("app.nope"));
    declared
        .search
        .insert(name("config app.fine"), mapping("pg_catalog.simple"));
    declared
        .search
        .insert(name("config app.own"), mapping("app.mine"));
    declared.search.insert(name("dictionary app.mine"), words());
    let ids = crate::resolve(&declared, &IdsFile::default(), &[], &ctx())
        .unwrap()
        .ids;
    let planned = plan((&Schema::default(), &IdsFile::default()), (&declared, &ids)).unwrap();
    let unresolved = |subject: &str| -> Vec<SearchRef> {
        let subject = name(subject);
        planned
            .changes
            .iter()
            .find_map(|p| match &p.change {
                Change::Search(SearchChange::CreateSearch {
                    name, unresolved, ..
                }) if *name == subject => Some(unresolved.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{} not planned: {planned:?}", subject.raw()))
    };
    assert_eq!(unresolved("config app.docs"), [name("dictionary app.nope")]);
    assert!(unresolved("config app.fine").is_empty());
    assert!(unresolved("config app.own").is_empty());
    assert!(unresolved("dictionary app.mine").is_empty());
}

/// A configuration that stops mapping a dictionary the plan retires is altered before the
/// dictionary is dropped, in every run: the drop waits only for the configurations that go
/// away or still map it (DEC-2728.3), and the order shadow refuses a plan whose effects a
/// step leaves unmet. Negative: the configuration that keeps mapping it is refused by name,
/// and a configuration that is dropped with it is dropped first, as before.
#[test]
fn a_dictionary_is_dropped_after_the_configuration_that_stopped_mapping_it() {
    use pbps_model::SearchObject;
    let dictionary = || SearchObject::Dictionary {
        template: "pg_catalog.simple".parse().unwrap(),
        options: Default::default(),
    };
    let config = |dictionary: &str| SearchObject::Config {
        parser: "pg_catalog.\"default\"".parse().unwrap(),
        mappings: [("word".to_owned(), vec![dictionary.parse().unwrap()])].into(),
    };
    let held = |old: Option<&str>, docs: Option<&str>| {
        let mut schema = Schema::default();
        for (name, object) in [
            ("dictionary app.old", old.map(|_| dictionary())),
            ("dictionary app.new", Some(dictionary())),
            ("config app.docs", docs.map(config)),
        ] {
            if let Some(object) = object {
                schema.search.insert(name.parse().unwrap(), object);
            }
        }
        schema
    };
    let base = held(Some(""), Some("app.old"));
    let retire = [Intent::DropSearch {
        name: "dictionary app.old".parse().unwrap(),
        reason: pbps_model::Reason::new("retired").unwrap(),
    }];
    let run = |declared: &Schema, intents: &[Intent]| {
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let declared_ids = crate::resolve(declared, &base_ids, intents, &ctx())
            .unwrap()
            .ids;
        let docs: pbps_model::SearchRef = "config app.docs".parse().unwrap();
        diff(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: declared,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints {
                target_read: true,
                search_dependents: [(docs, Default::default())].into(),
                ..Hints::default()
            },
        )
    };
    let kinds = |plan: &ChangeSet| -> Vec<String> {
        plan.changes
            .iter()
            .map(|p| match &p.change {
                Change::Search(SearchChange::AlterSearchMapping { .. }) => "alter".to_owned(),
                Change::Search(SearchChange::DropSearch { name, .. }) => {
                    format!("drop {}", name.name.name())
                }
                other => format!("{other:?}"),
            })
            .collect()
    };
    for _ in 0..20 {
        let plan = run(&held(None, Some("app.new")), &retire).unwrap();
        assert_eq!(kinds(plan.changes()), ["alter", "drop old"]);
    }
    // Negative: kept and still mapping it, the configuration is refused by name.
    let errors = run(&held(None, Some("app.old")), &retire).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.to_string().contains("app.docs") && e.to_string().contains("app.old")),
        "{errors:?}"
    );
    // Negative: dropped with it, the configuration goes first.
    let both = [
        retire[0].clone(),
        Intent::DropSearch {
            name: "config app.docs".parse().unwrap(),
            reason: pbps_model::Reason::new("retired").unwrap(),
        },
    ];
    let plan = run(&held(None, None), &both).unwrap();
    assert_eq!(kinds(plan.changes()), ["drop docs", "drop old"]);
}
