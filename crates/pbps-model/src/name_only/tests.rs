//! One table of rows per name-only kind: adding a kind adds a row here, and every
//! property below runs over it.

use super::*;
use crate::SearchChange;
use crate::{
    Change, ChangeSet, Column, IdsFile, PlannedChange, SearchKind, Table, TableName, Uid, UidKind,
};

/// One name-only kind: how to make a schema that holds one object of it.
struct Row {
    label: &'static str,
    /// A schema holding the object called `name` in definition `variant` (0 or 1).
    holding: fn(&str, u8) -> Schema,
}

fn text_search(name: &str, variant: u8) -> Schema {
    let template = if variant == 0 {
        "pg_catalog.simple"
    } else {
        "pg_catalog.synonym"
    };
    let mut schema = Schema::default();
    schema.search.insert(
        SearchRef::new(SearchKind::Dictionary, name.parse().unwrap()),
        SearchObject::Dictionary {
            template: template.parse().unwrap(),
            options: BTreeMap::new(),
        },
    );
    schema
}

fn sequence(name: &str, variant: u8) -> Schema {
    let mut schema = Schema::default();
    schema.sequences.insert(
        name.parse().unwrap(),
        Sequence {
            ty: ColumnType::simple("bigint"),
            start: i64::from(variant) + 1,
            increment: 1,
            options: Default::default(),
        },
    );
    schema
}

/// Every kind a `Schema` holds by name only.
const ROWS: &[Row] = &[
    Row {
        label: "text search",
        holding: text_search,
    },
    Row {
        label: "sequence",
        holding: sequence,
    },
];

fn table(ty: &str) -> Schema {
    let mut schema = Schema::default();
    let mut table = Table::default();
    table
        .columns
        .insert("c".into(), Column::new(ty.parse::<ColumnType>().unwrap()));
    schema.tables.insert(TableName::new("app", "t"), table);
    schema
}

fn merged(mut base: Schema, other: Schema) -> Schema {
    base.search.extend(other.search);
    base.sequences.extend(other.sequences);
    base.tables.extend(other.tables);
    base
}

fn long_spelling(ty: &ColumnType) -> Option<ColumnType> {
    let int: ColumnType = "int".parse().unwrap();
    (*ty == int).then(|| "integer".parse().unwrap())
}

#[test]
fn classify_splits_the_kinds_and_joins_them_back() {
    for row in ROWS {
        let whole = merged(table("int"), (row.holding)("app.x", 0));
        let split = whole.clone().classify();
        assert!(split.plannable.search.is_empty(), "{}", row.label);
        assert!(split.plannable.sequences.is_empty(), "{}", row.label);
        assert!(!split.name_only.is_empty(), "{}", row.label);
        assert_eq!(split.plannable.tables, whole.tables, "{}", row.label);
        assert_eq!(split.into_schema(), whole, "{}", row.label);
    }
}

#[test]
fn an_undeclared_held_object_is_absent_from_the_compared_and_the_recorded_forms() {
    for row in ROWS {
        let live = merged(table("int"), (row.holding)("app.held", 0));
        let nothing_declared = table("int");
        let compared = live.compared_with(&nothing_declared, &|_| None);
        assert!(compared.name_only().is_empty(), "{}: compared", row.label);
        assert_eq!(compared.tables, live.tables, "{}", row.label);
        let recorded = live.clone().as_recorded(&nothing_declared);
        assert!(recorded.name_only().is_empty(), "{}: recorded", row.label);
        // Negative: a declared one stays, whatever its definition.
        let declared = (row.holding)("app.held", 1);
        assert!(
            !live
                .compared_with(&declared, &|_| None)
                .name_only()
                .is_empty()
        );
        assert_eq!(
            live.clone().as_recorded(&declared).name_only(),
            live.name_only()
        );
        // Negative: another name is not this one.
        let other = (row.holding)("app.other", 0);
        assert!(live.clone().as_recorded(&other).name_only().is_empty());
    }
}

#[test]
fn an_alias_spelling_and_the_catalog_spelling_have_one_compared_form() {
    for row in ROWS {
        let declared = merged(table("int"), (row.holding)("app.x", 0));
        let catalog = merged(table("integer"), (row.holding)("app.x", 0));
        assert_ne!(declared, catalog, "{}: raw spellings differ", row.label);
        assert_eq!(
            declared.compared_with(&catalog, &long_spelling),
            catalog.compared_with(&declared, &long_spelling),
            "{}",
            row.label
        );
        // Negative: a real difference survives the spelling.
        let changed = merged(table("bigint"), (row.holding)("app.x", 0));
        assert_ne!(
            declared.compared_with(&changed, &long_spelling),
            changed.compared_with(&declared, &long_spelling)
        );
    }
}

#[test]
fn a_recorded_object_that_is_gone_or_altered_is_reported_and_one_never_recorded_is_not() {
    for row in ROWS {
        let recorded = (row.holding)("app.kept", 0);
        let same = Schema::name_only_drift(&recorded, &recorded);
        assert!(same.is_empty(), "{}", row.label);
        let gone = Schema::name_only_drift(&recorded, &Schema::default());
        assert_eq!(gone.len(), 1, "{}", row.label);
        assert_eq!(gone[0].change, NameOnlyChange::Gone);
        assert!(gone[0].to_string().contains("app.kept"), "{}", gone[0]);
        let altered = Schema::name_only_drift(&recorded, &(row.holding)("app.kept", 1));
        assert_eq!(altered.len(), 1, "{}", row.label);
        assert_eq!(altered[0].change, NameOnlyChange::Changed);
        // Negative: an object only the database holds was never recorded.
        let extra = merged(
            (row.holding)("app.kept", 0),
            (row.holding)("app.made_later", 0),
        );
        assert!(
            Schema::name_only_drift(&recorded, &extra).is_empty(),
            "{}",
            row.label
        );
    }
}

#[test]
fn an_apply_carries_the_previous_record_and_not_what_the_database_holds() {
    for row in ROWS {
        let previous = merged(table("int"), (row.holding)("app.kept", 0));
        // The read-back holds the recorded object altered and one made outside the plan.
        let read_back = merged(
            merged(table("bigint"), (row.holding)("app.kept", 1)),
            (row.holding)("app.outside", 0),
        );
        // The recorded object still has its identity, as a declared one does.
        let recorded = read_back.clone().as_recorded_after_apply(
            &previous,
            &nothing_planned(),
            &live_of(&previous),
        );
        assert_eq!(recorded.name_only(), previous.name_only(), "{}", row.label);
        // The plan's own result is what the read-back says of everything else.
        assert_eq!(recorded.tables, read_back.tables, "{}", row.label);
    }
}

fn nothing_planned() -> ChangeSet {
    ChangeSet::default()
}

/// A plan that creates the search objects `schema` holds, as they stand in it.
fn creating(schema: &Schema) -> ChangeSet {
    ChangeSet {
        changes: schema
            .search
            .iter()
            .map(|(name, object)| {
                PlannedChange::new(Change::Search(SearchChange::CreateSearch {
                    uid: Uid::generate(UidKind::Search),
                    name: name.clone(),
                    definition: Box::new(object.clone()),
                    unresolved: Vec::new(),
                }))
            })
            .collect(),
    }
}

/// The identities as the database holds them: one per name given.
fn live(names: &[&str]) -> IdsFile {
    let mut ids = IdsFile::default();
    for name in names {
        let held = text_search(name, 0).search.into_keys().next().unwrap();
        ids.search.insert(Uid::generate(UidKind::Search), held);
    }
    ids
}

fn held(schema: &Schema) -> Vec<String> {
    schema
        .search
        .keys()
        .map(|name| name.shown().to_string())
        .collect()
}

#[test]
fn an_apply_records_an_object_exactly_when_its_identity_is_held_and_the_database_holds_it() {
    let previous = merged(
        text_search("app.kept", 0),
        merged(text_search("app.retired", 0), table("int")),
    );
    // The read-back holds the recorded object altered, one the plan made, one made
    // outside the plan, and the dropped one is gone.
    let read_back = merged(
        text_search("app.kept", 1),
        merged(
            text_search("app.made", 0),
            merged(text_search("app.outside", 0), table("int")),
        ),
    );
    // The identities after the plan: the one it kept and the one it made. The dropped one
    // has none, and neither has the one made outside it.
    let ids = live(&["app.kept", "app.made"]);
    let recorded = read_back.clone().as_recorded_after_apply(
        &previous,
        &creating(&text_search("app.made", 0)),
        &ids,
    );
    let names = held(&recorded);
    // The created one is in as the database holds it, the dropped one is out, the one the
    // plan left alone keeps the recorded definition, and one made outside it is not in.
    assert_eq!(names.len(), 2, "{names:?}");
    assert_eq!(
        recorded.search,
        merged(text_search("app.kept", 0), text_search("app.made", 0)).search
    );
    // The read-back of an apply is kept to the objects an identity names, so it holds the
    // made one and neither the one made outside the plan nor a dropped one.
    let reference = Schema::naming_what_identities_name(&ids);
    assert_eq!(reference.search.len(), 2, "{:?}", reference.search);
    assert!(reference.tables.is_empty());
    let kept = read_back.as_recorded(&reference);
    assert_eq!(held(&kept).len(), 2, "{:?}", kept.search);
    // Negative: an identity whose object neither the record nor the database holds records
    // nothing (a create that has not run), and an object with no identity is not recorded
    // however the database holds it.
    let recorded = table("int").as_recorded_after_apply(
        &previous,
        &nothing_planned(),
        &live(&["app.not_run"]),
    );
    assert!(recorded.search.is_empty(), "{:?}", recorded.search);
}

#[test]
fn a_declaration_that_adopts_an_existing_object_records_it_with_its_new_identity() {
    // The object stands on the target and the declaration matches it, so the plan makes no
    // change for it and the previous record does not hold it: the identity is new, the
    // object is the database's. Both kinds.
    let previous = table("int");
    for (name, other) in [
        ("app.adopted", text_search("app.adopted", 0)),
        ("app.adopted_config", config("app.adopted_config")),
    ] {
        let read_back = merged(other.clone(), table("int"));
        let recorded = read_back.clone().as_recorded_after_apply(
            &previous,
            &nothing_planned(),
            &live_of(&other),
        );
        assert_eq!(recorded.search, other.search, "{name}");
        // Negative: the same object with no identity is the database's own, and not recorded.
        let recorded =
            read_back.as_recorded_after_apply(&previous, &nothing_planned(), &IdsFile::default());
        assert!(recorded.search.is_empty(), "{name}: {:?}", recorded.search);
    }
}

/// A configuration over the built-in dictionary.
fn config(name: &str) -> Schema {
    let mut schema = Schema::default();
    schema.search.insert(
        SearchRef::new(SearchKind::Config, name.parse().unwrap()),
        SearchObject::Config {
            parser: "pg_catalog.default".parse().unwrap(),
            mappings: BTreeMap::from([(
                "word".to_owned(),
                vec!["pg_catalog.simple".parse().unwrap()],
            )]),
        },
    );
    schema
}

/// The identities of every search object `schema` holds.
fn live_of(schema: &Schema) -> IdsFile {
    let mut ids = IdsFile::default();
    for name in schema.search.keys() {
        ids.search
            .insert(Uid::generate(UidKind::Search), name.clone());
    }
    ids
}

#[test]
fn the_buildable_form_has_only_the_objects_an_identity_names_and_borrows_when_there_is_none_to_leave_out()
 {
    for row in ROWS {
        let whole = merged(table("int"), (row.holding)("app.x", 0));
        let built = whole.buildable(&IdsFile::default());
        assert!(built.name_only().is_empty(), "{}", row.label);
        assert_eq!(built.tables, whole.tables, "{}", row.label);
        assert!(matches!(
            table("int").buildable(&IdsFile::default()),
            Cow::Borrowed(_)
        ));
    }
    // Negative: a text search object an identity names is built, and is not left out.
    let whole = merged(table("int"), text_search("app.x", 0));
    let mut ids = IdsFile::default();
    for name in whole.search.keys() {
        ids.search
            .insert(Uid::generate(UidKind::Search), name.clone());
    }
    assert!(matches!(whole.buildable(&ids), Cow::Borrowed(_)));
    // A sequence has no identity yet (DEC-2721.1), so none is built whatever the file holds.
    let with_sequence = merged(whole, sequence("app.s", 0));
    let built = with_sequence.buildable(&ids);
    assert!(built.sequences.is_empty() && built.search.len() == 1);
}

#[test]
fn equal_as_compared_ignores_only_the_objects_the_other_side_does_not_name() {
    for row in ROWS {
        let plain = table("int");
        let with = merged(table("int"), (row.holding)("app.x", 0));
        assert!(
            with.equal_as_compared(&plain),
            "{}: undeclared is not a difference",
            row.label
        );
        assert!(plain.equal_as_compared(&with), "{}", row.label);
        // Negative: the same name held two ways is a difference, as is a table.
        let altered = merged(table("int"), (row.holding)("app.x", 1));
        assert!(!with.equal_as_compared(&altered), "{}", row.label);
        assert!(!with.equal_as_compared(&table("bigint")), "{}", row.label);
    }
}

#[test]
fn a_record_matches_the_database_unless_a_recorded_object_moved() {
    for row in ROWS {
        let recorded = merged(table("int"), (row.holding)("app.kept", 0));
        assert!(recorded.recorded_matches(&recorded), "{}", row.label);
        let extra = merged(recorded.clone(), (row.holding)("app.made_later", 0));
        assert!(
            recorded.recorded_matches(&extra),
            "{}: never recorded",
            row.label
        );
        // Negative: gone, altered, and a changed table are all drift.
        assert!(
            !recorded.recorded_matches(&table("int")),
            "{}: gone",
            row.label
        );
        let altered = merged(table("int"), (row.holding)("app.kept", 1));
        assert!(
            !recorded.recorded_matches(&altered),
            "{}: altered",
            row.label
        );
        let moved = merged(table("bigint"), (row.holding)("app.kept", 0));
        assert!(!recorded.recorded_matches(&moved), "{}: table", row.label);
    }
}

/// `snapshot` compares its read with the previous record before replacing it. The read
/// keeps what that record names beside what the declarations do, so an object whose
/// declaration was removed, and which the database still holds unchanged, is no
/// difference; what is then recorded names only the declarations.
#[test]
fn a_re_recording_reads_what_the_record_names_and_records_what_is_declared() {
    for row in ROWS {
        let recorded = merged(table("int"), (row.holding)("app.undeclared_since", 0));
        let declared = merged(table("int"), (row.holding)("app.declared", 0));
        let reads_for = declared.naming_also_what_is_recorded(&recorded);
        let live = |variant| {
            let held = merged(table("int"), (row.holding)("app.undeclared_since", variant));
            let held = merged(held, (row.holding)("app.declared", 0));
            merged(held, (row.holding)("app.nobodys", 0))
        };
        let read = live(0).as_recorded(&reads_for);
        assert!(recorded.recorded_matches(&read), "{}", row.label);
        assert_eq!(read.as_recorded(&declared), declared, "{}", row.label);
        // Kept to the declarations alone, the unchanged database reads as drifted.
        assert!(
            !recorded.recorded_matches(&live(0).as_recorded(&declared)),
            "{}: the read this replaces",
            row.label
        );
        // Negative: the object changed or gone is still a difference, declared or not.
        assert!(
            !recorded.recorded_matches(&live(1).as_recorded(&reads_for)),
            "{}: altered",
            row.label
        );
        assert!(
            !recorded.recorded_matches(&declared.clone().as_recorded(&reads_for)),
            "{}: gone",
            row.label
        );
        // The plannable kinds are the declarations', and a name both hold keeps the
        // declared definition.
        let both = merged(table("bigint"), (row.holding)("app.declared", 1));
        let named = declared.naming_also_what_is_recorded(&both);
        assert_eq!(named, declared, "{}", row.label);
    }
}

/// A sequence names its integer type, which the catalog spells in full and a declaration
/// may abbreviate: both reach one compared form, and a different type does not (DEC-2721.1).
#[test]
fn a_sequences_type_is_spelled_like_a_columns() {
    let typed = |ty: &str| {
        let mut schema = sequence("app.s", 0);
        schema.sequences.values_mut().next().unwrap().ty = ty.parse().unwrap();
        schema
    };
    let (declared, catalog) = (typed("int"), typed("integer"));
    assert_ne!(declared, catalog, "raw spellings differ");
    assert_eq!(
        declared.compared_with(&catalog, &long_spelling),
        catalog.compared_with(&declared, &long_spelling)
    );
    // Negative: another type is a difference whatever the spelling.
    assert_ne!(
        declared.compared_with(&typed("bigint"), &long_spelling),
        typed("bigint").compared_with(&declared, &long_spelling)
    );
}

#[test]
fn a_checkpoint_records_only_the_search_changes_that_have_run() {
    // A plan that drops a configuration and then the dictionary it maps, stopped between
    // the two: the configuration is gone, and the dictionary, which stands and whose
    // identity is still live, stays recorded.
    let previous = merged(text_search("app.docs", 0), text_search("app.words", 0));
    let read_back = text_search("app.words", 0);
    let recorded = read_back.clone().as_recorded_after_apply(
        &previous,
        &nothing_planned(),
        &live(&["app.words"]),
    );
    let names = held(&recorded);
    assert!(
        names.len() == 1 && names[0].contains("app.words"),
        "{names:?}"
    );
    // Negative: once the dictionary's drop has run too, nothing is recorded.
    let recorded = Schema::default().as_recorded_after_apply(
        &previous,
        &nothing_planned(),
        &IdsFile::default(),
    );
    assert!(recorded.search.is_empty(), "{:?}", recorded.search);
    // A create that has not run is not recorded as made, even where the database holds
    // the object (it was made outside the plan): its identity is not live yet.
    let recorded = text_search("app.made", 0).as_recorded_after_apply(
        &previous,
        &nothing_planned(),
        &IdsFile::default(),
    );
    assert!(
        !held(&recorded).iter().any(|name| name.contains("app.made")),
        "{:?}",
        recorded.search
    );
}

#[test]
fn a_create_that_was_stopped_and_resumed_is_recorded_as_it_was_finished() {
    // The previous record is the checkpoint, which holds the configuration half made; the
    // plan creates it, and the read-back holds it finished: the finished one is recorded.
    // An object the plan does not create keeps its recorded definition (negative).
    let half = config_with("app.docs", &["pg_catalog.simple"]);
    let finished = config_with("app.docs", &["pg_catalog.simple", "pg_catalog.simple"]);
    let ids = live_of(&finished);
    let recorded = finished
        .clone()
        .as_recorded_after_apply(&half, &creating(&finished), &ids);
    assert_eq!(recorded.search, finished.search);
    let recorded = finished
        .clone()
        .as_recorded_after_apply(&half, &nothing_planned(), &ids);
    assert_eq!(recorded.search, half.search);
}

/// A configuration whose `word` token maps these dictionaries in order.
fn config_with(name: &str, dictionaries: &[&str]) -> Schema {
    let mut schema = Schema::default();
    schema.search.insert(
        SearchRef::new(SearchKind::Config, name.parse().unwrap()),
        SearchObject::Config {
            parser: "pg_catalog.default".parse().unwrap(),
            mappings: BTreeMap::from([(
                "word".to_owned(),
                dictionaries.iter().map(|d| d.parse().unwrap()).collect(),
            )]),
        },
    );
    schema
}

/// What a plan alters in place is the plan's own result, like what it creates (DEC-2728.3):
/// the record takes it as the database holds it after the apply, while an object the plan left
/// alone keeps the definition it was recorded with, even where the read-back says otherwise.
/// Negative: an alteration that has not run (the object is gone from the read-back) records
/// nothing new, and falls back to the previous record.
#[test]
fn an_object_a_plan_alters_in_place_is_recorded_as_the_database_holds_it() {
    let previous = merged(
        text_search("app.altered", 0),
        text_search("app.untouched", 0),
    );
    let read_back = merged(
        text_search("app.altered", 1),
        text_search("app.untouched", 1),
    );
    let ids = live(&["app.altered", "app.untouched"]);
    let name = SearchRef::new(SearchKind::Dictionary, "app.altered".parse().unwrap());
    let altering = ChangeSet {
        changes: vec![PlannedChange::new(Change::Search(
            SearchChange::AlterSearchDictionary {
                uid: Uid::generate(UidKind::Search),
                name: name.clone(),
                before: Box::new(previous.search[&name].clone()),
                after: Box::new(read_back.search[&name].clone()),
            },
        ))],
    };
    let recorded = read_back
        .clone()
        .as_recorded_after_apply(&previous, &altering, &ids);
    assert_eq!(recorded.search[&name], read_back.search[&name]);
    let untouched = SearchRef::new(SearchKind::Dictionary, "app.untouched".parse().unwrap());
    assert_eq!(recorded.search[&untouched], previous.search[&untouched]);
    let not_run = table("int").as_recorded_after_apply(&previous, &altering, &ids);
    assert_eq!(not_run.search[&name], previous.search[&name]);
}
