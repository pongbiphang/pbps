//! Unit tests of the capture on a hand-built catalog: how each dependency row
//! is classified, which candidates a runtime-bound name needs, what the seal
//! requires, and the wording of each refusal. The engine's behaviour itself
//! is pinned by the live tests in `tests/live/capture2141.rs`.

use super::read::{Baseline, Catalog, Dependency, Module, Object, Principal, Schema, Session};
use super::select;
use super::{CandidateClass, CandidateSet, CaptureScope, CapturedInputs, class, object};
use std::collections::{BTreeMap, BTreeSet};

const DBO: i64 = 1;
const APP: i64 = 5;
const DBO_USER: i64 = 1;
const OTHER_USER: i64 = 7;
const PRICE: i64 = 100;
const VIEW: i64 = 200;

fn catalog() -> Catalog {
    Catalog {
        major: 17,
        session: Session {
            user: "dbo".into(),
            default_schema: "dbo".into(),
            ansi_nulls: "1".into(),
            quoted_identifier: "1".into(),
        },
        baseline: Baseline::Absent,
        schemas: vec![
            Schema {
                id: DBO,
                name: "dbo".into(),
                owner: DBO_USER,
            },
            Schema {
                id: APP,
                name: "app".into(),
                owner: DBO_USER,
            },
        ],
        principals: vec![
            Principal {
                id: DBO_USER,
                name: "dbo".into(),
                kind: "S".into(),
                default_schema: Some("dbo".into()),
                owner: None,
            },
            Principal {
                id: OTHER_USER,
                name: "u_other".into(),
                kind: "S".into(),
                default_schema: Some("other".into()),
                owner: None,
            },
        ],
        objects: vec![
            Object {
                id: PRICE,
                schema: DBO,
                name: "price".into(),
                kind: "U".into(),
                parent: None,
                owner: None,
            },
            Object {
                id: VIEW,
                schema: APP,
                name: "v".into(),
                kind: "V".into(),
                parent: None,
                owner: None,
            },
        ],
        modules: BTreeMap::from([(
            VIEW,
            Module {
                definition: Some("CREATE VIEW app.v AS SELECT 1 AS one".into()),
                ansi_nulls: true,
                quoted_identifier: true,
                schema_bound: false,
                execute_as: None,
            },
        )]),
        columns: Vec::new(),
        parameters: Vec::new(),
        types: Vec::new(),
        synonyms: BTreeMap::new(),
        triggers: BTreeMap::new(),
        whole: BTreeMap::new(),
        table_type_objects: BTreeMap::new(),
        fulltext: BTreeMap::new(),
        numbered: BTreeSet::new(),
        data_spaces: BTreeMap::new(),
        xml_collections: BTreeMap::new(),
        encryption_keys: BTreeMap::new(),
        keys: BTreeMap::new(),
        unique_indexes: BTreeMap::new(),
        foreign_keys: BTreeMap::new(),
        table_types: BTreeMap::new(),
        constraints: BTreeMap::new(),
        dependencies: Vec::new(),
        objects_named: BTreeMap::from([(("app".into(), "v".into()), BTreeSet::from([VIEW]))]),
        types_named: BTreeMap::new(),
        system_named: BTreeMap::new(),
    }
}

fn reference(schema: Option<&str>, entity: &str, referenced: Option<i64>) -> Dependency {
    Dependency {
        referencing: VIEW,
        referencing_column: 0,
        class: 1,
        server: None,
        database: None,
        schema: schema.map(str::to_owned),
        entity: entity.into(),
        referenced,
        referenced_column: 0,
        schema_bound: false,
        caller_dependent: false,
    }
}

fn call(entity: &str) -> Dependency {
    let mut row = reference(None, entity, None);
    row.caller_dependent = true;
    row
}

fn scope() -> CaptureScope {
    CaptureScope {
        retained: BTreeSet::from([object("app", "v")]),
        candidates: BTreeSet::new(),
    }
}

fn condition(catalog: &Catalog) -> &'static str {
    match select::select(catalog, &scope()) {
        Ok(_) => panic!("the capture was expected to refuse"),
        Err(refusal) => refusal.condition,
    }
}

fn captured(catalog: Catalog, scope: CaptureScope) -> CapturedInputs {
    let selected = select::select(&catalog, &scope).expect("select");
    CapturedInputs::new(catalog, selected, scope)
}

fn set(class: CandidateClass, schema: &str, name: &str) -> CandidateSet {
    CandidateSet {
        class,
        schema: schema.into(),
        name: name.into(),
    }
}

#[test]
fn a_two_part_name_is_a_binding_and_its_target_is_read_too() {
    let mut catalog = catalog();
    catalog
        .dependencies
        .push(reference(Some("dbo"), "price", Some(PRICE)));
    let captured = captured(catalog, scope());
    let bound: Vec<_> = captured
        .bound_objects(&object("app", "v"))
        .unwrap()
        .collect();
    assert_eq!(bound, [&object("dbo", "price")]);
    assert!(captured.objects().any(|o| *o == object("dbo", "price")));
    assert_eq!(captured.runtime_bound().count(), 0);
}

#[test]
fn a_one_part_name_is_runtime_bound_and_binds_nothing_even_when_it_resolves() {
    let mut catalog = catalog();
    // The row names what `price` resolves to now: no binding for all that.
    catalog
        .dependencies
        .push(reference(None, "price", Some(PRICE)));
    let captured = captured(catalog, scope());
    assert_eq!(
        captured.bound_objects(&object("app", "v")).unwrap().count(),
        0
    );
    assert_eq!(
        captured.runtime_bound().collect::<Vec<_>>(),
        [&object("app", "v")]
    );
    assert_eq!(
        captured.runtime_scope().candidates,
        BTreeSet::from([
            set(CandidateClass::Object, "app", "price"),
            set(CandidateClass::Object, "dbo", "price"),
        ])
    );
}

#[test]
fn a_one_part_type_is_runtime_bound_and_asks_for_types_not_objects() {
    let mut catalog = catalog();
    let mut row = reference(None, "amount", Some(300));
    row.class = 6;
    catalog.dependencies.push(row);
    let captured = captured(catalog, scope());
    assert_eq!(
        captured.runtime_bound().collect::<Vec<_>>(),
        [&object("app", "v")]
    );
    assert_eq!(
        captured.runtime_scope().candidates,
        BTreeSet::from([
            set(CandidateClass::Type, "app", "amount"),
            set(CandidateClass::Type, "dbo", "amount"),
        ])
    );
}

#[test]
fn a_call_has_sys_dbo_and_every_principals_schema_but_never_its_own() {
    let mut catalog = catalog();
    catalog.dependencies.push(call("q"));
    assert_eq!(
        captured(catalog, scope()).runtime_scope().candidates,
        BTreeSet::from([
            set(CandidateClass::SystemProcedure, "sys", "q"),
            set(CandidateClass::Object, "dbo", "q"),
            set(CandidateClass::Object, "other", "q"),
        ])
    );
}

#[test]
fn a_fixed_execute_as_has_only_that_principals_schema() {
    let mut catalog = catalog();
    catalog.modules.get_mut(&VIEW).unwrap().execute_as = Some(OTHER_USER);
    catalog.dependencies.push(call("q"));
    let schemas: BTreeSet<_> = captured(catalog, scope())
        .runtime_scope()
        .candidates
        .into_iter()
        .filter(|c| c.class == CandidateClass::Object)
        .map(|c| c.schema)
        .collect();
    assert_eq!(schemas, BTreeSet::from(["dbo".into(), "other".into()]));
}

#[test]
fn an_execute_as_owner_call_uses_the_owners_default_schema() {
    let mut catalog = catalog();
    catalog.modules.get_mut(&VIEW).unwrap().execute_as = Some(-2);
    catalog.objects[1].owner = Some(OTHER_USER);
    catalog.dependencies.push(call("q"));
    let wanted = captured(catalog, scope()).runtime_scope().candidates;
    assert!(wanted.contains(&set(CandidateClass::Object, "other", "q")));
}

#[test]
fn every_refusal_names_its_reason() {
    let mut far = catalog();
    let mut row = reference(Some("dbo"), "t", None);
    row.database = Some("otherdb".into());
    far.dependencies.push(row);
    assert_eq!(
        condition(&far),
        "a reference names an object in another database or server"
    );

    let mut deferred = catalog();
    deferred
        .dependencies
        .push(reference(Some("app"), "not_yet", None));
    assert_eq!(
        condition(&deferred),
        "a reference names an object that does not exist yet (deferred name resolution)"
    );

    let mut unknown = catalog();
    let mut row = reference(Some("dbo"), "x", Some(PRICE));
    row.class = 10;
    unknown.dependencies.push(row);
    assert_eq!(
        condition(&unknown),
        "a reference is of a class this capture does not read"
    );

    let mut nameless = catalog();
    let mut row = reference(None, "x", Some(999));
    row.schema_bound = true;
    nameless.dependencies.push(row);
    assert_eq!(
        condition(&nameless),
        "a reference names an object the capture cannot identify"
    );
}

#[test]
fn a_dependency_of_a_table_types_own_objects_refuses() {
    use super::read::Type;
    let mut catalog = catalog();
    catalog.types.push(Type {
        id: 400,
        schema: APP,
        name: "rows".into(),
        base: 243,
        max_length: -1,
        precision: 0,
        scale: 0,
        collation: None,
        nullable: false,
        user_defined: true,
        table_type: true,
    });
    catalog.table_types.insert(400, Default::default());
    // The type's own table object (900) and a constraint on it (901).
    catalog
        .table_type_objects
        .insert(400, BTreeSet::from([900, 901]));
    let mut row = reference(Some("dbo"), "f", Some(PRICE));
    row.referencing = 901;
    catalog.dependencies.push(row);
    let scope = CaptureScope {
        retained: BTreeSet::from([super::super::capture::ObjectIdentity {
            class: class::TYPE.into(),
            name: vec!["app".into(), "rows".into()],
            signature: Vec::new(),
        }]),
        candidates: BTreeSet::new(),
    };
    catalog
        .types_named
        .insert(("app".into(), "rows".into()), BTreeSet::from([400]));
    match select::select(&catalog, &scope) {
        Ok(_) => panic!("the capture was expected to refuse"),
        Err(refusal) => assert_eq!(
            refusal.condition,
            "a table type's expression names an object, which the capture does not follow"
        ),
    }
}

#[test]
fn a_heap_and_an_index_named_like_a_stand_in_are_both_kept() {
    let mut catalog = catalog();
    let row = |id: &str, name: Option<&str>| {
        super::read::WholeRow::from([
            ("object_id".to_owned(), Some(PRICE.to_string())),
            ("index_id".to_owned(), Some(id.to_owned())),
            ("name".to_owned(), name.map(str::to_owned)),
            ("type".to_owned(), Some(id.to_owned())),
        ])
    };
    catalog
        .whole
        .insert("indexes", vec![row("0", None), row("2", Some("(heap)"))]);
    let mut scope = scope();
    scope.retained.insert(object("dbo", "price"));
    catalog
        .objects_named
        .insert(("dbo".into(), "price".into()), BTreeSet::from([PRICE]));
    let captured = captured(catalog, scope);
    let indexes = &captured.inputs[&object("dbo", "price")].properties["sys.indexes"];
    assert_eq!(indexes.as_array().map(Vec::len), Some(2), "{indexes}");
}

#[test]
fn a_system_procedure_set_naming_another_schema_refuses() {
    let mut scope = scope();
    scope
        .candidates
        .insert(set(CandidateClass::SystemProcedure, "dbo", "sp_who"));
    match select::select(&catalog(), &scope) {
        Ok(_) => panic!("the capture was expected to refuse"),
        Err(refusal) => assert_eq!(
            refusal.condition,
            "a system-procedure candidate names a schema other than sys"
        ),
    }
}

#[test]
fn a_clr_object_refuses_by_name() {
    let mut catalog = catalog();
    catalog.objects[1].kind = "PC".into();
    catalog.modules.clear();
    assert_eq!(
        condition(&catalog),
        "a CLR object's code is in an assembly, which the capture does not read"
    );
    let mut external = self::catalog();
    external.objects[1].kind = "ET".into();
    external.modules.clear();
    assert_eq!(
        condition(&external),
        "an external table's shape is not read by the capture"
    );
}

#[test]
fn a_stoplist_is_compared_by_its_words_not_its_name_alone() {
    use super::read::fulltext_entry;
    let words = |list: &[&str]| Some(list.iter().map(|w| (*w).to_owned()).collect());
    assert_ne!(
        fulltext_entry("sl".into(), words(&["1033:foo"])),
        fulltext_entry("sl".into(), words(&["1033:foo", "1033:bar"]))
    );
    assert_eq!(
        fulltext_entry("sl".into(), None),
        fulltext_entry("sl".into(), words(&[]))
    );
}

#[test]
fn a_retained_identity_the_capture_cannot_look_up_refuses() {
    let mut scope = scope();
    scope
        .retained
        .insert(super::super::capture::ObjectIdentity {
            class: class::COLUMN.into(),
            name: vec!["v".into()],
            signature: vec![object("app", "v")],
        });
    match select::select(&catalog(), &scope) {
        Ok(_) => panic!("the capture was expected to refuse"),
        Err(refusal) => assert_eq!(
            refusal.condition,
            "a retained identity is of a class or shape the capture does not look up"
        ),
    }
}

#[test]
fn an_engine_object_is_read_by_identity_only() {
    let mut catalog = catalog();
    // `sys.objects` is the engine's own: no row of it is in the catalog read.
    catalog
        .dependencies
        .push(reference(Some("sys"), "objects", Some(999)));
    let captured = captured(catalog, scope());
    let engine = object("sys", "objects");
    assert!(captured.objects().any(|o| *o == engine));
    assert_eq!(captured.bound_objects(&engine).unwrap().count(), 0);
}

#[test]
fn a_changed_property_binding_or_membership_is_a_difference_and_nothing_else_is() {
    use pbps_db::resolver::capture::InputChange;
    let base = || {
        let mut catalog = catalog();
        catalog
            .dependencies
            .push(reference(Some("dbo"), "price", Some(PRICE)));
        catalog
    };
    let before = captured(base(), scope());
    assert_eq!(before.compare(&captured(base(), scope())), Vec::new());

    let mut changed = base();
    changed.modules.get_mut(&VIEW).unwrap().quoted_identifier = false;
    let differences = before.compare(&captured(changed, scope()));
    assert_eq!(differences.len(), 1);
    assert_eq!(differences[0].change, InputChange::Properties);

    let mut rebound = base();
    rebound.dependencies.clear();
    let differences = before.compare(&captured(rebound, scope()));
    assert!(
        differences
            .iter()
            .any(|d| d.object == Some(object("app", "v")) && d.change == InputChange::Bindings)
    );

    let mut runtime = base();
    runtime.dependencies.push(reference(None, "other", None));
    let differences = before.compare(&captured(runtime, scope()));
    assert!(
        differences
            .iter()
            .any(|d| d.change == InputChange::Membership)
    );
}

#[test]
fn a_session_change_is_an_environment_difference() {
    use pbps_db::resolver::capture::InputChange;
    let before = captured(catalog(), scope());
    let mut moved = catalog();
    moved.session.default_schema = "app".into();
    let differences = before.compare(&captured(moved, scope()));
    assert_eq!(differences.len(), 1);
    assert_eq!(differences[0].change, InputChange::Environment);
}

#[test]
fn only_a_retained_object_or_a_candidate_is_read() {
    let mut catalog = catalog();
    catalog.objects.push(Object {
        id: 300,
        schema: APP,
        name: "unrelated".into(),
        kind: "U".into(),
        parent: None,
        owner: None,
    });
    let captured = captured(catalog, scope());
    assert!(!captured.objects().any(|o| *o == object("app", "unrelated")));
    let principals = captured
        .objects()
        .filter(|o| o.class == class::PRINCIPAL)
        .count();
    assert_eq!(principals, 2, "principals are a complete set");
}

/// Unix only, like `pbps-pg`'s seal test: the key file is written with Unix
/// permissions, which the key loader requires.
#[cfg(unix)]
#[test]
fn the_seal_holds_every_binding_target_and_member_and_no_definition() {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut catalog = catalog();
    catalog
        .dependencies
        .push(reference(Some("dbo"), "price", Some(PRICE)));
    catalog
        .system_named
        .insert("sp_who".into(), BTreeSet::from(["sp_who".to_owned()]));
    let mut scope = scope();
    scope
        .candidates
        .insert(set(CandidateClass::SystemProcedure, "sys", "sp_who"));
    let captured = captured(catalog, scope);
    let path = std::env::temp_dir().join(format!("pbps2141-unit-key-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all(b"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=")
        .unwrap();
    drop(file);
    let key = pbps_db::fingerprint::EnvironmentFingerprintKey::from_file(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    let manifest = captured
        .seal(&key, &BTreeMap::new())
        .expect("the manifest's own checks pass");
    let serialized = serde_json::to_string(&manifest).unwrap();
    assert!(
        !serialized.contains("SELECT 1 AS one"),
        "a definition never leaves the capture"
    );
    assert!(
        manifest
            .prerequisites()
            .iter()
            .any(|p| p.object == object("sys", "sp_who"))
    );
}
