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

/// A drift comparison leaves out a text search object the record does not hold, and
/// still compares one it does: a change to a recorded one is named (DEC-2728.1).
#[test]
fn a_text_search_object_the_record_lacks_is_not_compared_and_a_recorded_one_is() {
    let mut recorded = Schema::default();
    recorded
        .search
        .insert(name("app.kept"), dictionary("pg_catalog.simple"));
    let mut live = recorded.clone();
    live.search
        .insert(name("app.made_later"), dictionary("pg_catalog.simple"));
    only_recorded_search_objects(&recorded, &mut live);
    assert_eq!(live, recorded, "the unrecorded object is left out");
    // Negative: a recorded object that changed stays in, so the differ names it.
    live.search
        .insert(name("app.kept"), dictionary("pg_catalog.synonym"));
    only_recorded_search_objects(&recorded, &mut live);
    assert_ne!(live, recorded);
    assert!(live.search.contains_key(&name("app.kept")));
}
