//! Captured child and index metadata must justify the projected catalog owner.
//! These private fixtures exercise the same identities carried by PG16/18 reads.

use super::*;
use crate::resolver::authorization::{AuthorizationContext, RoleMap};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::environment::DeploymentPrincipal;
use pbps_model::resolver::{ManifestError, ObjectOwnership, Surface};
use pbps_model::{Change, ChangeSet, IdsFile, PlannedChange, TableName};
use serde_json::json;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

static KEY_FILE: AtomicU64 = AtomicU64::new(0);

fn key() -> EnvironmentFingerprintKey {
    let path = std::env::temp_dir().join(format!(
        "pbps-1274-manifest-guards-{}-{}.key",
        std::process::id(),
        KEY_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .unwrap();
    drop(file);
    let key = EnvironmentFingerprintKey::from_file(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    key
}

fn identity(class: &str, name: &[&str], signature: Vec<ObjectIdentity>) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name: name.iter().map(|part| (*part).into()).collect(),
        signature,
    }
}

fn input(properties: BTreeMap<String, Value>) -> Input {
    Input {
        properties,
        bindings: Vec::new(),
    }
}

fn capture(major: u32, inputs: BTreeMap<ObjectIdentity, Input>) -> CapturedInputs {
    let deployer = identity("pg_authid", &["deployer"], vec![]);
    CapturedInputs {
        baseline: super::super::baseline::Baseline::Absent,
        session: super::super::session::Facts {
            current_role: deployer.clone(),
            session_role: deployer,
            settings: BTreeMap::new(),
        },
        rule: properties::RULE,
        major,
        scope: CaptureScope {
            retained: BTreeSet::new(),
            candidates: BTreeSet::new(),
        },
        inputs,
        role_pinned: BTreeMap::new(),
        attribute_numbers: BTreeMap::new(),
        candidates: BTreeMap::new(),
        limitations: BTreeSet::new(),
        dropped: BTreeMap::new(),
    }
}

fn rename_without_children(
    major: u32,
    not_null: bool,
) -> (CompiledCapture, CapturedInputs, ChangeSet, IdsFile, IdsFile) {
    let table = TableName::new("app", "t");
    let before = table.column("id");
    let after = table.column("renamed");
    let relation = relation_identity(&table);
    let old_column = identity("column", &["id"], vec![relation.clone()]);
    let new_column = identity("column", &["renamed"], vec![relation]);
    let table_uid = "t_000000".parse().unwrap();
    let column_uid: pbps_model::Uid = "c_000000".parse().unwrap();
    let mut base_ids = IdsFile::default();
    base_ids.tables.insert(table_uid, table.clone());
    base_ids.columns.insert(column_uid.clone(), before);
    let mut desired_ids = base_ids.clone();
    desired_ids.columns.insert(column_uid.clone(), after);
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(Change::RenameColumn {
            uid: column_uid,
            table,
            from: "id".into(),
            to: "renamed".into(),
            table_was: None,
        })],
    };
    let column_input = || input(BTreeMap::from([("attnotnull".into(), json!(not_null))]));
    let opening = capture(major, BTreeMap::from([(old_column, column_input())]));
    let compiled = capture(major, BTreeMap::from([(new_column, column_input())]));
    let context = AuthorizationContext {
        principal: DeploymentPrincipal {
            login: "deployer".into(),
            effective: "deployer".into(),
            superuser: false,
        },
        schemas: BTreeMap::new(),
        roles: BTreeMap::new(),
        settings: BTreeMap::new(),
    };
    let roles = RoleMap::generate(&context, &[], "deployer", "guardtest");
    let compiled = CompiledCapture::new(compiled, &key(), &roles, BTreeMap::new());
    (compiled, opening, changes, base_ids, desired_ids)
}

#[test]
fn pg18_nonnullable_rename_refuses_missing_automatic_children() {
    let (mut compiled, opening, changes, base_ids, desired_ids) = rename_without_children(18, true);
    assert_eq!(
        compiled.retain_renamed_not_null(&opening, &changes, &[], &base_ids, &desired_ids),
        Err(ManifestError::Incomplete),
        "attnotnull=true cannot be treated as a nullable column when both PG18 children are absent"
    );
}

#[test]
fn pg16_nonnullable_and_pg18_nullable_renames_need_no_child() {
    for (major, not_null) in [(16, true), (18, false)] {
        let (mut compiled, opening, changes, base_ids, desired_ids) =
            rename_without_children(major, not_null);
        assert_eq!(
            compiled.retain_renamed_not_null(&opening, &changes, &[], &base_ids, &desired_ids),
            Ok(()),
            "major {major}, attnotnull={not_null}"
        );
    }
}

fn child(column: &ObjectIdentity, name: &str) -> (ObjectIdentity, Input, ObjectIdentity) {
    let relation = column.signature[0].clone();
    let namespace = identity("pg_namespace", &["app"], vec![]);
    let no_type = identity("pg_type", &[], vec![]);
    let constraint = identity("pg_constraint", &[name], vec![namespace, relation, no_type]);
    let row = input(BTreeMap::from([
        ("contype".into(), json!("n")),
        ("conname".into(), json!(name)),
        ("conkey".into(), json!([column])),
    ]));
    let edge = identity(
        "pg_depend",
        &["a"],
        vec![constraint.clone(), column.clone()],
    );
    (constraint, row, edge)
}

#[test]
fn a_not_null_child_needs_one_exact_automatic_edge() {
    let table = TableName::new("app", "t");
    let column = identity("column", &["id"], vec![relation_identity(&table)]);
    let (first, row, edge) = child(&column, "t_id_not_null");
    let base = BTreeMap::from([
        (
            column.clone(),
            input(BTreeMap::from([("attnotnull".into(), json!(true))])),
        ),
        (first.clone(), row),
        (edge.clone(), input(BTreeMap::new())),
    ]);
    assert_eq!(
        not_null_child(&capture(18, base.clone()), &column),
        Ok(Some(first))
    );

    let mut missing_edge = base.clone();
    missing_edge.remove(&edge);
    assert_eq!(
        not_null_child(&capture(18, missing_edge), &column),
        Err(ManifestError::Incomplete)
    );

    let (second, row, edge) = child(&column, "other_not_null");
    let mut duplicate = base;
    duplicate.insert(second, row);
    duplicate.insert(edge, input(BTreeMap::new()));
    assert_eq!(
        not_null_child(&capture(18, duplicate), &column),
        Err(ManifestError::Invalid)
    );
}

fn explicit_index_fixture() -> (
    CapturedInputs,
    BTreeMap<ObjectIdentity, ObjectOwnership>,
    BTreeSet<Surface>,
    ObjectIdentity,
) {
    let table = TableName::new("app", "t");
    let parent = relation_identity(&table);
    let index = identity("pg_class", &["app", "ix"], vec![]);
    let metadata = identity("pg_index", &[], vec![index.clone()]);
    let owner = identity("pg_authid", &["ordinary_owner"], vec![]);
    let inputs = BTreeMap::from([
        (
            parent.clone(),
            input(BTreeMap::from([("relowner".into(), json!(&owner))])),
        ),
        (
            index.clone(),
            input(BTreeMap::from([
                ("relkind".into(), json!("i")),
                ("relowner".into(), json!(&owner)),
            ])),
        ),
        (
            metadata,
            input(BTreeMap::from([("indrelid".into(), json!(&parent))])),
        ),
    ]);
    let surface = Surface::Index {
        table,
        name: "ix".into(),
    };
    (
        capture(18, inputs),
        BTreeMap::from([(index.clone(), ObjectOwnership::Surface(surface.clone()))]),
        BTreeSet::from([surface]),
        index,
    )
}

#[test]
fn an_explicit_index_needs_its_recorded_parent_owner_and_no_owner_edge() {
    let (valid, ownership, surfaces, index) = explicit_index_fixture();
    assert_eq!(
        qualified_explicit_indexes(&valid, &ownership, &surfaces),
        Ok(BTreeSet::from([index.clone()]))
    );

    let (mut wrong_owner, ownership, surfaces, index) = explicit_index_fixture();
    wrong_owner
        .inputs
        .get_mut(&index)
        .unwrap()
        .properties
        .insert(
            "relowner".into(),
            json!(identity("pg_authid", &["other_owner"], vec![])),
        );
    assert_eq!(
        qualified_explicit_indexes(&wrong_owner, &ownership, &surfaces),
        Err(ManifestError::Invalid)
    );

    let (mut wrong_parent, ownership, surfaces, index) = explicit_index_fixture();
    let metadata = identity("pg_index", &[], vec![index.clone()]);
    wrong_parent
        .inputs
        .get_mut(&metadata)
        .unwrap()
        .properties
        .insert(
            "indrelid".into(),
            json!(relation_identity(&TableName::new("app", "other"))),
        );
    assert_eq!(
        qualified_explicit_indexes(&wrong_parent, &ownership, &surfaces),
        Err(ManifestError::Invalid)
    );

    let (mut invented_edge, ownership, surfaces, index) = explicit_index_fixture();
    let owner = identity("pg_authid", &["ordinary_owner"], vec![]);
    let edge = identity("pg_shdepend", &["o"], vec![index, owner]);
    invented_edge.inputs.insert(edge, input(BTreeMap::new()));
    assert_eq!(
        qualified_explicit_indexes(&invented_edge, &ownership, &surfaces),
        Err(ManifestError::Invalid)
    );
}

// PG16/18 did not expose owner edges for their implicit row/array types in
// the measured fixture. This invented edge exercises the private fail-closed
// deletion branch, without claiming that PostgreSQL creates such an edge.
fn synthetic_automatic_child_owner_edge(
    pinned: Option<bool>,
) -> Result<(pbps_model::resolver::InputManifest, ObjectIdentity), ManifestError> {
    use pbps_model::resolver::ObjectTransition;

    let table = TableName::new("app", "t");
    let surface = Surface::Table(table);
    let child = identity("pg_type", &["app", "t"], vec![]);
    let creator = identity("pg_authid", &["postgres"], vec![]);
    let context = AuthorizationContext {
        principal: DeploymentPrincipal {
            login: "postgres".into(),
            effective: "postgres".into(),
            superuser: true,
        },
        schemas: BTreeMap::new(),
        roles: BTreeMap::new(),
        settings: BTreeMap::new(),
    };
    let roles = RoleMap::generate(&context, &[], "synthetic_login", "childguard");
    let run_creator = roles.deployer(&context).unwrap();
    let scratch_edge = identity(
        "pg_shdepend",
        &["o"],
        vec![
            child.clone(),
            identity("pg_authid", &[&run_creator], vec![]),
        ],
    );
    let expected_edge = identity("pg_shdepend", &["o"], vec![child.clone(), creator.clone()]);
    let mut opening = capture(18, BTreeMap::new());
    if let Some(pinned) = pinned {
        opening.role_pinned.insert(creator, pinned);
    }
    let compiled = capture(
        18,
        BTreeMap::from([
            (child.clone(), input(BTreeMap::new())),
            (scratch_edge.clone(), input(BTreeMap::new())),
        ]),
    );
    let ownership = BTreeMap::from([
        (child.clone(), ObjectOwnership::Surface(surface.clone())),
        (scratch_edge, ObjectOwnership::Surface(surface.clone())),
    ]);
    let transition = ObjectTransition {
        surface,
        before: BTreeSet::new(),
        after: BTreeSet::from([child]),
    };
    let sealed = CompiledCapture::new(compiled, &key(), &roles, ownership).seal_for_plan_inner(
        &opening,
        &ChangeSet::default(),
        &[transition],
        &IdsFile::default(),
        &IdsFile::default(),
        &context,
    )?;
    Ok((sealed, expected_edge))
}

#[test]
fn a_synthetic_pinned_automatic_child_owner_edge_is_removed_but_an_ordinary_edge_survives() {
    let (pinned, edge) = synthetic_automatic_child_owner_edge(Some(true)).unwrap();
    assert!(
        pinned.prerequisites().iter().all(|row| row.object != edge),
        "the invented scratch owner edge cannot become a pinned target dependency"
    );
    assert_eq!(
        pinned
            .prerequisites()
            .iter()
            .filter(|row| row.object.class == "pg_type")
            .count(),
        1,
        "the child itself remains in the synthetic inventory"
    );

    let (ordinary, same_edge) = synthetic_automatic_child_owner_edge(Some(false)).unwrap();
    assert_eq!(edge, same_edge);
    assert_eq!(
        ordinary
            .prerequisites()
            .iter()
            .filter(|row| row.object == edge)
            .count(),
        1,
        "an ordinary creator's observed scratch edge must not be deleted"
    );
    assert!(matches!(
        synthetic_automatic_child_owner_edge(None),
        Err(ManifestError::Incomplete)
    ));
}
