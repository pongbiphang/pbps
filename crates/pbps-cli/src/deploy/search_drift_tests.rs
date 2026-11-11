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

/// A drift comparison leaves out a text search object no recorded identity names, and
/// still compares one that is named: a change to a managed one is reported by name
/// (DEC-2728.1, DEC-2728.2).
#[test]
fn a_text_search_object_without_an_identity_is_not_compared_and_a_managed_one_is() {
    let mut ids = IdsFile::default();
    ids.search.insert(
        pbps_model::Uid::generate(pbps_model::UidKind::Search),
        name("app.kept"),
    );
    let mut live = Schema::default();
    live.search
        .insert(name("app.kept"), dictionary("pg_catalog.synonym"));
    live.search
        .insert(name("app.made_later"), dictionary("pg_catalog.simple"));
    only_managed_search_objects(&ids, &mut live);
    // Negative: the managed object stays in, changed, so the differ names it.
    assert_eq!(live.search.keys().collect::<Vec<_>>(), [&name("app.kept")]);
    // Nothing is managed in a state that recorded no search identity: nothing is compared.
    only_managed_search_objects(&IdsFile::default(), &mut live);
    assert!(live.search.is_empty());
}
