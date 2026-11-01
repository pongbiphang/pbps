use super::super::{
    Binding, CandidateSet, InputManifest, Membership, ObjectIdentity, ObjectOwnership,
    ObjectTransition, Prerequisite, ReadScope, RoutineLookup, Surface,
};
use crate::TableName;
use std::collections::BTreeSet;

const RULE: &str = "fixture-v1";

fn object(class: &str, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name: vec!["app".into(), name.into()],
        signature: vec![],
    }
}

fn record(o: &ObjectIdentity, properties: &str, bindings: &[&ObjectIdentity]) -> Prerequisite {
    Prerequisite {
        object: o.clone(),
        ownership: ObjectOwnership::Unqualified,
        canonicalization: RULE.into(),
        properties: properties.repeat(32),
        bindings: bindings
            .iter()
            .map(|target| Binding {
                node: "n".into(),
                path: vec![],
                target: (*target).clone(),
            })
            .collect(),
    }
}

fn relations() -> CandidateSet {
    CandidateSet {
        class: "pg_class".into(),
        namespace: Some("app".into()),
        name: None,
    }
}

fn manifest(
    mut records: Vec<Prerequisite>,
    members: &[&ObjectIdentity],
    lookup: Option<&ObjectIdentity>,
    session: &str,
) -> InputManifest {
    records.sort_by(|a, b| a.object.cmp(&b.object));
    let retained = records.iter().map(|r| r.object.clone()).collect();
    InputManifest::new(
        RULE.into(),
        18,
        "01".repeat(8),
        ReadScope {
            retained,
            candidates: BTreeSet::from([relations()]),
        },
        "02".repeat(32),
        session.repeat(32),
        records,
        vec![Membership {
            predicate: relations(),
            members: members.iter().map(|m| (*m).clone()).collect(),
        }],
        BTreeSet::new(),
        vec![RoutineLookup {
            signature: "f(integer)".into(),
            search_path: "app".into(),
            kind: "f".into(),
            resolved: lookup.cloned(),
        }],
    )
    .unwrap()
}

fn table() -> ObjectIdentity {
    object("pg_class", "t")
}
fn view() -> ObjectIdentity {
    object("pg_class", "v")
}
fn routine() -> ObjectIdentity {
    object("pg_proc", "f")
}

fn opening() -> InputManifest {
    manifest(
        vec![
            record(&table(), "04", &[]),
            record(&view(), "05", &[&table()]),
            record(&routine(), "06", &[]),
        ],
        &[&table(), &view()],
        Some(&routine()),
        "03",
    )
}

#[test]
fn an_unchanged_opening_capture_has_no_differences() {
    assert_eq!(
        opening().opening_differences(&opening()),
        Vec::<String>::new()
    );
}

#[test]
fn every_moved_opening_input_is_named() {
    let changed = manifest(
        vec![
            record(&table(), "14", &[]),
            record(&view(), "05", &[&table()]),
            record(&routine(), "06", &[]),
        ],
        &[&table(), &view()],
        Some(&routine()),
        "03",
    );
    assert_eq!(
        opening().opening_differences(&changed),
        ["pg_class app.t changed"]
    );
    // A candidate arriving beside the unchanged records.
    let arrived = object("pg_class", "u");
    let crowded = manifest(
        vec![
            record(&table(), "04", &[]),
            record(&view(), "05", &[&table()]),
            record(&routine(), "06", &[]),
            record(&arrived, "07", &[]),
        ],
        &[&table(), &view(), &arrived],
        Some(&routine()),
        "03",
    );
    let differences = opening().opening_differences(&crowded);
    assert!(differences.contains(&"the candidates pg_class app.*".to_owned()));
    // A lookup moved while every record it could name is unchanged: the
    // case a check of the old routine alone accepts (#616).
    let moved = manifest(
        vec![
            record(&table(), "04", &[]),
            record(&view(), "05", &[&table()]),
            record(&routine(), "06", &[]),
        ],
        &[&table(), &view()],
        None,
        "03",
    );
    assert_eq!(
        opening().opening_differences(&moved),
        ["`f(integer)` under `app` resolves to another routine"]
    );
    let session = manifest(
        vec![
            record(&table(), "04", &[]),
            record(&view(), "05", &[&table()]),
            record(&routine(), "06", &[]),
        ],
        &[&table(), &view()],
        Some(&routine()),
        "13",
    );
    assert_eq!(
        opening().opening_differences(&session),
        ["the session's roles and settings"]
    );
    // Negative: the ledger's latest entry moves with every failed attempt,
    // which changes nothing on the target.
    let mut retried = serde_json::to_value(opening()).unwrap();
    retried["baseline"] = serde_json::json!("12".repeat(32));
    let retried: InputManifest = serde_json::from_value(retried).unwrap();
    assert_eq!(
        opening().opening_differences(&retried),
        Vec::<String>::new()
    );
}

/// The seal's closing manifest: the plan installs a table `n`, whose key
/// `n_pkey` the engine names, and rebinds the view to it.
fn closing() -> (InputManifest, Vec<ObjectTransition>) {
    let new = object("pg_class", "n");
    let key = object("pg_class", "n_pkey");
    let placeholder = |o: &ObjectIdentity, bindings: &[&ObjectIdentity]| {
        let mut p = record(o, "00", bindings);
        p.ownership = ObjectOwnership::Surface(Surface::Table(TableName::new("app", "n")));
        p.managed_closing()
    };
    let sealed = manifest(
        vec![
            record(&table(), "04", &[]),
            placeholder(&view(), &[&new]),
            placeholder(&new, &[]),
            placeholder(&key, &[]),
            record(&routine(), "06", &[]),
        ],
        &[&table(), &view(), &new, &key],
        Some(&routine()),
        "03",
    );
    let transitions = vec![ObjectTransition {
        surface: Surface::Table(TableName::new("app", "n")),
        before: BTreeSet::new(),
        after: BTreeSet::from([new.clone(), key.clone()]),
        references: BTreeSet::new(),
    }];
    (sealed, transitions)
}

/// The target after the plan: the installed records with their own
/// properties, the key under `key`'s name.
fn closed(key: &str, extra: Option<(Prerequisite, bool)>) -> InputManifest {
    let new = object("pg_class", "n");
    let key = object("pg_class", key);
    let owned = |mut p: Prerequisite| {
        p.ownership = ObjectOwnership::Surface(Surface::Table(TableName::new("app", "n")));
        p
    };
    let mut records = vec![
        record(&table(), "04", &[]),
        owned(record(&view(), "15", &[&new])),
        owned(record(&new, "16", &[])),
        owned(record(&key, "17", &[])),
        record(&routine(), "06", &[]),
    ];
    let mut members = vec![table(), view(), new.clone(), key.clone()];
    if let Some((record, member)) = extra {
        if member {
            members.push(record.object.clone());
        }
        records.push(record);
    }
    manifest(
        records,
        &members.iter().collect::<Vec<_>>(),
        Some(&routine()),
        "03",
    )
}

#[test]
fn installed_records_close_with_their_own_properties_and_an_engine_chosen_name() {
    let (sealed, transitions) = closing();
    assert_eq!(
        sealed.vanishable(),
        BTreeSet::from([object("pg_class", "n_pkey")])
    );
    assert_eq!(
        sealed.closing_differences(&closed("n_pkey", None), &transitions),
        Vec::<String>::new()
    );
    // The key the target's engine named otherwise, as another relation
    // already held `n_pkey` there.
    assert_eq!(
        sealed.closing_differences(&closed("n_pkey1", None), &transitions),
        Vec::<String>::new()
    );
}

#[test]
fn a_closing_record_the_plan_does_not_install_is_refused() {
    let (sealed, transitions) = closing();
    // An unmanaged candidate committed by another session: its ownership
    // proves no surface.
    let arrived = record(&object("pg_class", "intruder"), "08", &[]);
    assert_eq!(
        sealed.closing_differences(&closed("n_pkey", Some((arrived, true))), &transitions),
        [
            "pg_class app.intruder arrived, and no change of the plan owns it",
            "the candidates pg_class app.*"
        ]
    );
    // Owned by a declared surface no transition installs.
    let mut other = record(&object("pg_class", "other_pkey"), "08", &[]);
    other.ownership = ObjectOwnership::Surface(Surface::Table(TableName::new("app", "other")));
    assert_eq!(
        sealed.closing_differences(&closed("n_pkey", Some((other, true))), &transitions),
        [
            "pg_class app.other_pkey arrived, and no change of the plan owns it",
            "the candidates pg_class app.*"
        ]
    );
}

/// A part of a table the plan rebuilds, an index for one, leaves records
/// the table owns, such as its dependency rows on the rebuilt index: the
/// index's change owns them, as a part of the same table (#616). A record
/// another table owns is not the change's.
#[test]
fn a_record_of_the_table_a_rebuilt_part_belongs_to_is_owned_by_that_change() {
    let (sealed, _) = closing();
    let rebuilt = vec![ObjectTransition {
        surface: Surface::Index {
            table: TableName::new("app", "n"),
            name: "ix".into(),
        },
        before: BTreeSet::new(),
        after: BTreeSet::new(),
        references: BTreeSet::new(),
    }];
    let held_by = |table: &str| {
        let mut p = record(&object("pg_depend", &format!("{table}_ix")), "09", &[]);
        p.ownership = ObjectOwnership::Surface(Surface::Table(TableName::new("app", table)));
        p
    };
    assert_eq!(
        sealed.closing_differences(&closed("n_pkey", Some((held_by("n"), false))), &rebuilt),
        Vec::<String>::new()
    );
    assert_eq!(
        sealed.closing_differences(&closed("n_pkey", Some((held_by("other"), false))), &rebuilt),
        ["pg_depend app.other_ix arrived, and no change of the plan owns it"]
    );
}

/// Another table's foreign key that a rename rewrites rides on the renamed
/// table's change as a reference (DEC-1274.1): the closing read finds it
/// owned by its own table, unsealed, and it is the plan's (#616). A record
/// the change does not list, or one listed but owned by no table, is not.
#[test]
fn a_record_another_tables_change_carries_as_a_reference_is_the_plans() {
    let (sealed, _) = closing();
    let key = object("pg_constraint", "fk_c_p");
    let renamed = |references: BTreeSet<ObjectIdentity>| {
        vec![ObjectTransition {
            surface: Surface::Table(TableName::new("app", "q")),
            before: BTreeSet::new(),
            after: references.clone(),
            references,
        }]
    };
    let child = |ownership: ObjectOwnership| {
        let mut p = record(&key, "09", &[]);
        p.ownership = ownership;
        p
    };
    let child_table = ObjectOwnership::Surface(Surface::Table(TableName::new("app", "c")));
    assert_eq!(
        sealed.closing_differences(
            &closed("n_pkey", Some((child(child_table.clone()), false))),
            &renamed(BTreeSet::from([key.clone()]))
        ),
        Vec::<String>::new()
    );
    let refused = ["pg_constraint app.fk_c_p arrived, and no change of the plan owns it"];
    assert_eq!(
        sealed.closing_differences(
            &closed("n_pkey", Some((child(child_table), false))),
            &renamed(BTreeSet::new())
        ),
        refused
    );
    assert_eq!(
        sealed.closing_differences(
            &closed("n_pkey", Some((child(ObjectOwnership::Unqualified), false))),
            &renamed(BTreeSet::from([key.clone()]))
        ),
        refused
    );
}

#[test]
fn a_moved_untouched_record_or_installed_binding_or_lookup_is_refused() {
    let (sealed, transitions) = closing();
    let mut fresh = serde_json::to_value(closed("n_pkey", None)).unwrap();
    // The untouched table's properties.
    fresh["prerequisites"][2]["properties"] = serde_json::json!("14".repeat(32));
    let fresh: InputManifest = serde_json::from_value(fresh).unwrap();
    assert_eq!(
        sealed.closing_differences(&fresh, &transitions),
        ["pg_class app.t changed"]
    );
    // The installed view bound to the old table instead.
    let mut rebound = serde_json::to_value(closed("n_pkey", None)).unwrap();
    rebound["prerequisites"][3]["bindings"][0]["target"] = serde_json::to_value(table()).unwrap();
    let rebound: InputManifest = serde_json::from_value(rebound).unwrap();
    assert_eq!(
        sealed.closing_differences(&rebound, &transitions),
        ["pg_class app.v's bindings changed"]
    );
    let mut lookup = serde_json::to_value(closed("n_pkey", None)).unwrap();
    lookup["identifications"][0]["resolved"] = serde_json::Value::Null;
    let lookup: InputManifest = serde_json::from_value(lookup).unwrap();
    assert_eq!(
        sealed.closing_differences(&lookup, &transitions),
        ["`f(integer)` under `app` resolves to another routine"]
    );
}

#[test]
fn an_installed_record_something_binds_to_cannot_vanish() {
    let (sealed, transitions) = closing();
    // `n` is the view's binding target, so the read insists on it.
    assert!(!sealed.vanishable().contains(&object("pg_class", "n")));
    let mut fresh = serde_json::to_value(closed("n_pkey", None)).unwrap();
    let records = fresh["prerequisites"].as_array_mut().unwrap();
    records.retain(|r| r["object"]["name"][1] != "n");
    let members = fresh["membership"][0]["members"].as_array_mut().unwrap();
    members.retain(|m| m["name"][1] != "n");
    // Without `n` the view's binding names a record the manifest lacks.
    assert!(serde_json::from_value::<InputManifest>(fresh).is_err());
    // And a vanishable key the target lacks altogether is accepted, but
    // only that one.
    let mut keyless = serde_json::to_value(closed("n_pkey", None)).unwrap();
    let records = keyless["prerequisites"].as_array_mut().unwrap();
    records.retain(|r| r["object"]["name"][1] != "n_pkey");
    let members = keyless["membership"][0]["members"].as_array_mut().unwrap();
    members.retain(|m| m["name"][1] != "n_pkey");
    keyless["scope"]["retained"]
        .as_array_mut()
        .unwrap()
        .retain(|m| m["name"][1] != "n_pkey");
    let keyless: InputManifest = serde_json::from_value(keyless).unwrap();
    assert_eq!(
        sealed.closing_differences(&keyless, &transitions),
        Vec::<String>::new()
    );
}

/// An engine-shipped record the plan's closure binds to is a kept record like
/// any other: if the target's differs at closing, apply refuses (DEC-2143.6).
#[test]
fn a_kept_engine_built_in_record_that_changed_is_refused_at_closing() {
    let int = ObjectIdentity {
        class: "sys.types".into(),
        name: vec!["sys".into(), "int".into()],
        signature: vec![],
    };
    let column = object("sys.columns", "c");
    let sealed = manifest(
        vec![record(&int, "09", &[]), record(&column, "0a", &[&int])],
        &[],
        None,
        "03",
    );
    let same = manifest(
        vec![record(&int, "09", &[]), record(&column, "0a", &[&int])],
        &[],
        None,
        "03",
    );
    assert_eq!(sealed.closing_differences(&same, &[]), Vec::<String>::new());
    let changed = manifest(
        vec![record(&int, "0b", &[]), record(&column, "0a", &[&int])],
        &[],
        None,
        "03",
    );
    let found = sealed.closing_differences(&changed, &[]);
    assert!(
        found
            .iter()
            .any(|d| d.contains("changed") && d.contains("int")),
        "{found:?}"
    );
}
