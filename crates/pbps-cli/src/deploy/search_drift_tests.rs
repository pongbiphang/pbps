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
