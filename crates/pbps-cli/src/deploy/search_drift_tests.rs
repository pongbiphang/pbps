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
