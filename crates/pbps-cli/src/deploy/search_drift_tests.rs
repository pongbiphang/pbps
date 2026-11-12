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
    live.keep_only_named_unidentified(&recorded);
    assert_eq!(live, recorded, "the unrecorded object is left out");
    // Negative: a recorded object that changed stays in, so the differ names it.
    live.search
        .insert(name("app.kept"), dictionary("pg_catalog.synonym"));
    live.keep_only_named_unidentified(&recorded);
    assert_ne!(live, recorded);
    assert!(live.search.contains_key(&name("app.kept")));
}

/// The same for a standalone sequence (DEC-2721.1): one the record lacks is not compared,
/// one it holds is, and the counter, which no field holds, can never be the difference.
#[test]
fn a_standalone_sequence_the_record_lacks_is_not_compared_and_a_recorded_one_is() {
    let sequence = |start| pbps_model::Sequence {
        ty: pbps_model::ColumnType::simple("bigint"),
        start,
        increment: 1,
        options: Default::default(),
    };
    let kept: pbps_model::ObjectName = "app.kept".parse().unwrap();
    let mut recorded = Schema::default();
    recorded.sequences.insert(kept.clone(), sequence(1));
    let mut live = recorded.clone();
    live.sequences
        .insert("app.made_later".parse().unwrap(), sequence(1));
    live.keep_only_named_unidentified(&recorded);
    assert_eq!(live, recorded, "the unrecorded sequence is left out");
    // Negative: a recorded sequence that changed stays in, so the differ names it.
    live.sequences.insert(kept.clone(), sequence(50));
    live.keep_only_named_unidentified(&recorded);
    assert_ne!(live, recorded);
    assert!(live.sequences.contains_key(&kept));
}
