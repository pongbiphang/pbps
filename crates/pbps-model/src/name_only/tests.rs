//! One table of rows per name-only kind: adding a kind adds a row here, and every
//! property below runs over it.

use super::*;
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
        let recorded = read_back
            .clone()
            .as_recorded_after_apply(&previous, &ChangeSet::default());
        assert_eq!(recorded.name_only(), previous.name_only(), "{}", row.label);
        // The plan's own result is what the read-back says of everything else.
        assert_eq!(recorded.tables, read_back.tables, "{}", row.label);
    }
}

fn planned(changes: impl IntoIterator<Item = Change>) -> ChangeSet {
    ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    }
}

fn created(name: &str) -> Change {
    let object = text_search(name, 0).search.into_iter().next().unwrap();
    Change::CreateSearch {
        uid: Uid::generate(UidKind::Search),
        name: object.0,
        definition: Box::new(object.1),
        unresolved: Vec::new(),
    }
}

fn dropped(name: &str) -> Change {
    let object = text_search(name, 0).search.into_iter().next().unwrap();
    Change::DropSearch {
        uid: Uid::generate(UidKind::Search),
        name: object.0,
        definition: Box::new(object.1),
    }
}

#[test]
fn an_apply_records_the_objects_it_creates_and_leaves_out_the_ones_it_drops() {
    let previous = merged(
        text_search("app.kept", 0),
        merged(text_search("app.retired", 0), table("int")),
    );
    let read_back = merged(
        text_search("app.kept", 1),
        merged(
            text_search("app.made", 0),
            merged(text_search("app.outside", 0), table("int")),
        ),
    );
    let applied = planned([created("app.made"), dropped("app.retired")]);
    let recorded = read_back
        .clone()
        .as_recorded_after_apply(&previous, &applied);
    let names: Vec<_> = recorded
        .search
        .keys()
        .map(|n| n.shown().to_string())
        .collect();
    // The created one is in as the database holds it, the dropped one is out, the one the
    // plan left alone keeps the recorded definition, and one made outside it is not in.
    assert_eq!(names.len(), 2, "{names:?}");
    assert_eq!(
        recorded.search,
        merged(text_search("app.kept", 0), text_search("app.made", 0)).search
    );
    // The read-back of an apply is kept to the objects the plan creates, so it holds the
    // created one and neither the one made outside the plan nor a dropped one.
    let reference = Schema::naming_what_a_plan_creates(&applied);
    assert_eq!(reference.search.len(), 1, "{:?}", reference.search);
    assert!(reference.tables.is_empty());
    let kept = read_back.clone().as_recorded(&reference);
    assert_eq!(kept.search.len(), 1, "{:?}", kept.search);
    // Negative: a creation the read-back does not hold (a staged checkpoint before it ran)
    // records nothing, and a drop of an object the record never held changes nothing.
    let none = table("int").as_recorded_after_apply(
        &previous,
        &planned([created("app.made"), dropped("app.never")]),
    );
    assert_eq!(none.search.len(), 2, "{:?}", none.search);
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
        // Negative: one an identity names is built.
        let mut ids = IdsFile::default();
        for name in whole.search.keys() {
            ids.search
                .insert(Uid::generate(UidKind::Search), name.clone());
        }
        assert!(
            matches!(whole.buildable(&ids), Cow::Borrowed(_)),
            "{}",
            row.label
        );
    }
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
