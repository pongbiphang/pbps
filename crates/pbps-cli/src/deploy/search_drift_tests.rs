use super::*;
use pbps_model::{SearchKind, SearchObject, SearchRef};

fn dictionary(template: &str) -> SearchObject {
    SearchObject::Dictionary {
        template: template.parse().unwrap(),
        options: Default::default(),
    }
}
fn name(text: &str) -> SearchRef {
    SearchRef::new(SearchKind::Dictionary, text.parse().unwrap())
}
fn holding(objects: &[(&str, &str)]) -> Schema {
    let mut schema = Schema::default();
    for (object, template) in objects {
        schema.search.insert(name(object), dictionary(template));
    }
    schema
}

/// What `verify` finds when it compares `recorded` with `live`: the sentences of the one
/// report of recorded name-only objects, and what the differ finds on top of them.
fn found(recorded: &Schema, live: &Schema) -> (Vec<String>, usize) {
    let pg = pbps_postgres();
    let comparison = drift_comparison(recorded, live, &pg);
    let ids = pbps_model::IdsFile::default();
    let diffed = pbps_diff::diff_partial(
        pbps_diff::Side {
            schema: &comparison.recorded,
            ids: &ids,
        },
        pbps_diff::Side {
            schema: &comparison.live,
            ids: &ids,
        },
        &pg,
        &pbps_model::Hints::default(),
    );
    (
        comparison.name_only,
        diffed.errors.len() + diffed.changes.changes.len(),
    )
}

fn pbps_postgres() -> pbps_pg::Postgres {
    pbps_pg::Postgres::new()
}

/// A drift comparison names a recorded text search object that is gone (the differ walks
/// only the live side, so it said nothing) or changed (it said "arriving with a later
/// part"), once, and leaves out one the record does not hold (DEC-2728.1, DEC-2855.1).
#[test]
fn a_recorded_text_search_object_that_is_gone_or_changed_is_named_once_and_an_unrecorded_one_is_not()
 {
    let recorded = holding(&[("app.kept", "pg_catalog.simple")]);
    let (named, other) = found(&recorded, &recorded);
    assert!((named.is_empty(), other) == (true, 0), "{named:?} {other}");

    let (named, other) = found(&recorded, &Schema::default());
    assert_eq!(named.len(), 1, "{named:?}");
    assert!(named[0].contains("`app.kept` was recorded and is no longer in the database"));
    assert_eq!(other, 0, "the differ adds nothing of its own for it");

    let (named, other) = found(&recorded, &holding(&[("app.kept", "pg_catalog.synonym")]));
    assert_eq!(named.len(), 1, "{named:?}");
    assert!(named[0].contains("differs from the definition recorded for it"));
    assert_eq!(other, 0, "not also refused by the differ as a later part");

    // Negative: one the record never held, beside the recorded one, is not drift.
    let live = holding(&[
        ("app.kept", "pg_catalog.simple"),
        ("app.made_later", "pg_catalog.simple"),
    ]);
    let (named, other) = found(&recorded, &live);
    assert!((named.is_empty(), other) == (true, 0), "{named:?} {other}");
}

/// The same report names a recorded standalone sequence (DEC-2721.1, DEC-2855.1): gone or
/// changed once, an unrecorded one not at all, and the counter, which no field holds, can
/// never be the difference.
#[test]
fn a_recorded_sequence_that_is_gone_or_changed_is_named_once_and_an_unrecorded_one_is_not() {
    let sequence = |start| pbps_model::Sequence {
        ty: pbps_model::ColumnType::simple("bigint"),
        start,
        increment: 1,
        options: Default::default(),
    };
    let held = |objects: &[(&str, i64)]| {
        let mut schema = Schema::default();
        for (object, start) in objects {
            schema
                .sequences
                .insert(object.parse().unwrap(), sequence(*start));
        }
        schema
    };
    let recorded = held(&[("app.kept", 1)]);
    let (named, other) = found(&recorded, &recorded);
    assert!((named.is_empty(), other) == (true, 0), "{named:?} {other}");

    let (named, other) = found(&recorded, &Schema::default());
    assert_eq!(named.len(), 1, "{named:?}");
    assert!(named[0].contains("sequence `app.kept` was recorded and is no longer"));
    assert_eq!(other, 0, "the differ adds nothing of its own for it");

    let (named, other) = found(&recorded, &held(&[("app.kept", 50)]));
    assert_eq!(named.len(), 1, "{named:?}");
    assert!(named[0].contains("sequence `app.kept` differs from the definition recorded"));
    assert_eq!(other, 0, "not also refused by the differ as a later part");

    // Negative: one the record never held, beside the recorded one, is not drift.
    let (named, other) = found(&recorded, &held(&[("app.kept", 1), ("app.made_later", 1)]));
    assert!((named.is_empty(), other) == (true, 0), "{named:?} {other}");
}

fn with_options(options: &[(&str, &str)]) -> SearchObject {
    SearchObject::Dictionary {
        template: "pg_catalog.simple".parse().unwrap(),
        options: options
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    }
}

fn mapping_to(dictionary: &str) -> SearchObject {
    SearchObject::Config {
        parser: "pg_catalog.default".parse().unwrap(),
        mappings: [("asciiword".to_owned(), vec![dictionary.parse().unwrap()])].into(),
    }
}

/// A text search object the plan alters in place stands at the definition it was approved
/// over or at the one it writes, before its statement and at a checkpoint, and at the close
/// at the one it writes (SPEC §7.6, DEC-2728.3; the definitions between are the next test's). Without it the read-back was taken as the
/// alteration's result, and a change another session made to the same object was recorded as
/// applied. Negatives: an object the plan does not alter is not read (DEC-2855.1).
#[test]
fn a_text_search_object_a_plan_alters_stands_at_an_approved_definition_and_at_the_close_at_the_result()
 {
    let config = SearchRef::new(SearchKind::Config, "app.c".parse().unwrap());
    // (object, approved over, the plan's result, a definition another session might leave)
    let cases = [
        (
            name("app.d"),
            with_options(&[("stopwords", "a")]),
            with_options(&[("stopwords", "b")]),
            with_options(&[("stopwords", "other")]),
        ),
        (
            config,
            mapping_to("app.d"),
            mapping_to("pg_catalog.simple"),
            mapping_to("app.other"),
        ),
    ];
    for (object, approved, wrote, other) in cases {
        let (uid, before, after) = (
            "s_000001".parse().unwrap(),
            Box::new(approved.clone()),
            Box::new(wrote.clone()),
        );
        let object_name = object.clone();
        let changes = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Search(
                if object.kind == SearchKind::Config {
                    pbps_model::SearchChange::AlterSearchMapping {
                        uid,
                        name: object_name,
                        before,
                        after,
                        opaque: Vec::new(),
                    }
                } else {
                    pbps_model::SearchChange::AlterSearchDictionary {
                        uid,
                        name: object_name,
                        before,
                        after,
                        opaque: Vec::new(),
                    }
                },
            ))],
        };
        let holding_it = |definition: &SearchObject| {
            let mut schema = Schema::default();
            schema.search.insert(object.clone(), definition.clone());
            schema
        };
        let (was, result, moved) = (
            holding_it(&approved),
            holding_it(&wrote),
            holding_it(&other),
        );
        let ids = guard::fixture_ids(&changes, &was);
        let plan = guard::Plan {
            changes: &changes,
            ids: &ids,
        };
        let staged = guard::Staged::new(plan).unwrap();
        let close = |before: &Schema, after: &Schema| {
            let read = guard::Read::fixture_transactional(plan, before);
            refuse_pinned_movement(&pbps_pg::Postgres::new(), read, before, after, "fixture")
                .map_err(|e| e.to_string())
        };
        let midway = |before: &Schema, after: &Schema| {
            let read = staged.fixture_read(
                StagedRead::Checkpoint {
                    completed: 1,
                    total: 2,
                },
                before,
            );
            refuse_pinned_movement(&pbps_pg::Postgres::new(), read, before, after, "fixture")
                .map_err(|e| e.to_string())
        };
        let shown = object.shown().to_string();
        close(&was, &result).expect("what the plan wrote, from what it was approved over");
        midway(&was, &was).expect("the statement has not run yet at an earlier checkpoint");
        midway(&result, &result).expect("a resumed run starts from the result");
        // Moved since the plan was approved: the live definition is neither of the two.
        let error = close(&moved, &result).unwrap_err();
        assert!(
            error.contains(&shown) && error.contains("what its plan promised"),
            "{error}"
        );
        // The statement overwrote another session's change: the close is not the plan's result.
        assert!(close(&was, &moved).unwrap_err().contains(&shown));
        // A statement that never took effect is not the result at the close.
        assert!(close(&was, &was).unwrap_err().contains(&shown));
        // A checkpoint refuses what is neither definition.
        assert!(midway(&was, &moved).unwrap_err().contains(&shown));
        // Absent is not a definition of the plan.
        assert!(close(&was, &Schema::default()).is_err());
        // Negative: an object no change of the plan names is not read (DEC-2855.1).
        let (mut before, mut after) = (was.clone(), result.clone());
        before
            .search
            .insert(name("app.untouched"), dictionary("pg_catalog.simple"));
        after
            .search
            .insert(name("app.untouched"), dictionary("pg_catalog.synonym"));
        close(&before, &after).expect("an untouched object is held by name only");
    }
}

/// A mapping change of several statements passes through definitions that are neither end, and a
/// staged apply checkpoints on each: every one of them is admitted at a checkpoint, a resume from
/// any of them closes, and only the last is the close (SPEC §7.6, DEC-2728.3). A definition on
/// no step of the chain is refused at a checkpoint too, so the pin is the chain and not
/// "anything between". Negatives: the same object read at an intermediate definition at the
/// close, and an off-chain definition at a checkpoint, are refused.
#[test]
fn a_staged_checkpoint_between_the_statements_of_a_mapping_change_stands_on_the_chain() {
    let config = SearchRef::new(SearchKind::Config, "app.c".parse().unwrap());
    let mapped = |pairs: &[(&str, &str)]| SearchObject::Config {
        parser: "pg_catalog.default".parse().unwrap(),
        mappings: pairs
            .iter()
            .map(|(alias, d)| ((*alias).to_owned(), vec![d.parse().unwrap()]))
            .collect(),
    };
    // Drops `email`, alters `word`, adds `url`: three statements.
    let approved = mapped(&[
        ("asciiword", "app.a"),
        ("email", "app.a"),
        ("word", "app.a"),
    ]);
    let wrote = mapped(&[("asciiword", "app.a"), ("url", "app.b"), ("word", "app.b")]);
    let chain = approved.alteration_chain(&wrote);
    assert_eq!(chain.len(), 3, "{chain:?}");
    assert_eq!(chain.last(), Some(&wrote));
    let changes = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Search(
            pbps_model::SearchChange::AlterSearchMapping {
                uid: "s_000001".parse().unwrap(),
                name: config.clone(),
                before: Box::new(approved.clone()),
                after: Box::new(wrote.clone()),
                opaque: Vec::new(),
            },
        ))],
    };
    let holding = |definition: &SearchObject| {
        let mut schema = Schema::default();
        schema.search.insert(config.clone(), definition.clone());
        schema
    };
    let ids = guard::fixture_ids(&changes, &holding(&approved));
    let plan = guard::Plan {
        changes: &changes,
        ids: &ids,
    };
    let staged = guard::Staged::new(plan).unwrap();
    let midway = |before: &SearchObject, after: &SearchObject| {
        let (before, after) = (holding(before), holding(after));
        let read = staged.fixture_read(
            StagedRead::Checkpoint {
                completed: 1,
                total: 3,
            },
            &before,
        );
        refuse_pinned_movement(&pbps_pg::Postgres::new(), read, &before, &after, "fixture")
            .map_err(|e| e.to_string())
    };
    for step in &chain {
        midway(&approved, step).expect("a checkpoint on the chain");
        midway(step, step).expect("a resume from a checkpoint on the chain");
    }
    // Another session's definition is on no step, even one that mixes the two ends.
    let mixed = mapped(&[
        ("asciiword", "app.b"),
        ("email", "app.a"),
        ("word", "app.a"),
    ]);
    assert!(!chain.contains(&mixed));
    assert!(midway(&approved, &mixed).unwrap_err().contains("app.c"));
    // Only the last step is the close.
    let close = |after: &SearchObject| {
        let (before, after) = (holding(&approved), holding(after));
        let read = guard::Read::fixture_transactional(plan, &before);
        refuse_pinned_movement(&pbps_pg::Postgres::new(), read, &before, &after, "fixture")
            .map_err(|e| e.to_string())
    };
    close(&wrote).expect("the close at the last step");
    assert!(
        close(&chain[0]).is_err(),
        "an intermediate step is not the close"
    );
}
