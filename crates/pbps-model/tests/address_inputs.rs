//! Payload address bindings must not redirect fixed baseline FK reads.
use pbps_model::operation::{
    self, ModuleEdits, ObjectKey, Operations, Origin, OwnerError, Owners, PlanOwners, Snapshot,
    Source, Spelling,
};
use pbps_model::plan_names::PlanNames;
use pbps_model::{Change, ChangeSet, IdsFile, PlannedChange, Schema, TableName, Uid};
use std::cell::RefCell;

struct Observed<'a> {
    inner: PlanOwners<'a>,
    payload: bool,
    queries: RefCell<Vec<(TableName, Spelling)>>,
}
impl Owners for Observed<'_> {
    fn table(&self, name: &TableName, spelling: Spelling) -> Result<Uid, OwnerError> {
        self.queries.borrow_mut().push((name.clone(), spelling));
        if self.payload && name == &table("app.u2") && spelling == Spelling::Base {
            return Ok(uid("t_aaaaaa"));
        }
        self.inner.table(name, spelling)
    }
    fn column(&self, name: &pbps_model::ColumnRef, spelling: Spelling) -> Result<Uid, OwnerError> {
        self.inner.column(name, spelling)
    }
    fn role(&self, name: &pbps_model::RoleName, spelling: Spelling) -> Result<Uid, OwnerError> {
        self.inner.role(name, spelling)
    }
}
fn uid(s: &str) -> Uid {
    s.parse().unwrap()
}
fn table(s: &str) -> TableName {
    s.parse().unwrap()
}

#[test]
fn a_reused_payload_name_keeps_the_distinct_baseline_foreign_key_target() {
    let mut before = Schema::default();
    before.tables.insert(
        table("app.u2"),
        serde_json::from_value(serde_json::json!({
            "columns":{"k":{"type":"integer","nullable":false}},
            "primary_key":{"columns":["k"]}
        }))
        .unwrap(),
    );
    before.tables.insert(table("app.u"), serde_json::from_value(serde_json::json!({
        "columns":{"k":{"type":"integer","nullable":false}},
        "primary_key":{"columns":["k"]},
        "foreign_keys":{"fk":{"columns":["k"],"references_table":"app.u2","references_columns":["k"]}}
    })).unwrap());
    let base = IdsFile {
        tables: [
            (uid("t_aaaaaa"), table("app.u")),
            (uid("t_bbbbbb"), table("app.u2")),
        ]
        .into(),
        columns: [
            (uid("c_aaaaaa"), "app.u.k".parse().unwrap()),
            (uid("c_bbbbbb"), "app.u2.k".parse().unwrap()),
        ]
        .into(),
        ..Default::default()
    };
    let goal = IdsFile {
        tables: [
            (uid("t_aaaaaa"), table("app.u2")),
            (uid("t_bbbbbb"), table("app.v")),
        ]
        .into(),
        columns: [
            (uid("c_aaaaaa"), "app.u2.k".parse().unwrap()),
            (uid("c_bbbbbb"), "app.v.k".parse().unwrap()),
        ]
        .into(),
        ..Default::default()
    };
    let mut after = Schema::default();
    after
        .tables
        .insert(table("app.v"), before.tables[&table("app.u2")].clone());
    let mut kept = before.tables[&table("app.u")].clone();
    kept.foreign_keys.clear();
    after.tables.insert(table("app.u2"), kept);
    let steps = ChangeSet {
        changes: vec![
            Change::RenameTable {
                uid: uid("t_bbbbbb"),
                from: table("app.u2"),
                to: table("app.v"),
                defaults: vec![],
            },
            Change::RenameTable {
                uid: uid("t_aaaaaa"),
                from: table("app.u"),
                to: table("app.u2"),
                defaults: vec![],
            },
            Change::DropForeignKey {
                table: table("app.u2"),
                name: "fk".into(),
            },
        ]
        .into_iter()
        .map(PlannedChange::new)
        .collect(),
    };
    let operations = Operations::expanded(
        3,
        steps.changes.iter().enumerate().map(|(i, p)| {
            (
                if i == 2 {
                    Origin::Teardown(Source::Change(2))
                } else {
                    Origin::Whole(i)
                },
                p.clone(),
            )
        }),
        ModuleEdits::Rebuilt,
    );
    let op = &operations.as_slice()[2];
    for payload in [false, true] {
        let owners = Observed {
            inner: PlanOwners::new(PlanNames::of(&steps.changes, &goal).unwrap(), &base, &goal),
            payload,
            queries: RefCell::new(Vec::new()),
        };
        let fx = operation::effects(
            op,
            &owners,
            Snapshot {
                schema: &before,
                ids: &base,
            },
            Snapshot {
                schema: &after,
                ids: &goal,
            },
        )
        .unwrap();
        let ObjectKey::Part { table: owner, .. } = op.key(&owners).unwrap() else {
            panic!()
        };
        let targets: Vec<_> = fx
            .bindings
            .iter()
            .map(|b| b.target.uid().to_string())
            .collect();
        println!(
            "payload={payload}: owner={}, FK targets={targets:?}; identical queries={:?}",
            owner.uid(),
            owners.queries.borrow()
        );
        if payload {
            assert_eq!(owner.uid(), &uid("t_aaaaaa"));
            assert_eq!(
                targets,
                ["t_aaaaaa"],
                "redirecting the payload lookup also redirects its baseline FK target"
            );
            assert_ne!(
                targets,
                ["t_bbbbbb"],
                "the required baseline binding must target t_bbbbbb"
            );
        } else {
            assert_eq!(
                owner.uid(),
                &uid("t_bbbbbb"),
                "strict Base mis-owns the emitted DROP"
            );
            assert!(targets.is_empty());
        }
        assert!(
            owners
                .queries
                .borrow()
                .iter()
                .filter(|(n, s)| n == &table("app.u2") && *s == Spelling::Base)
                .count()
                >= 3
        );
    }
    let names = PlanNames::of(&steps.changes, &goal).unwrap();
    let chain = operation::AddressChain::bind(
        &names,
        &operations,
        &base,
        &goal,
        &Default::default(),
        &[op.id()].into(),
    );
    let qualified = operations.with_address_chain(chain);
    let op = &qualified.as_slice()[2];
    let owners = PlanOwners::new(PlanNames::of(&steps.changes, &goal).unwrap(), &base, &goal);
    let fx = operation::effects(
        op,
        &owners,
        Snapshot {
            schema: &before,
            ids: &base,
        },
        Snapshot {
            schema: &after,
            ids: &goal,
        },
    )
    .unwrap();
    let ObjectKey::Part { table: owner, .. } = op.key(&owners).unwrap() else {
        panic!()
    };
    assert_eq!(owner.uid(), &uid("t_aaaaaa"));
    assert_eq!(
        fx.bindings
            .iter()
            .map(|b| b.target.uid().clone())
            .collect::<Vec<_>>(),
        [uid("t_bbbbbb")]
    );
    assert_eq!(op.logical_origin().spelling(), Spelling::Base);
    assert_eq!(op.origin().spelling(), Spelling::Final);
}

fn rename_reader(
    final_name: &str,
    read_after_last: bool,
) -> (Schema, IdsFile, Schema, IdsFile, ChangeSet, Operations) {
    let mut before = Schema::default();
    before.tables.insert(
        table("app.a"),
        serde_json::from_value(serde_json::json!({
            "columns":{"k":{"type":"integer","nullable":true}},"checks":{"ck":{"expression":"k > 0"}}
        }))
        .unwrap(),
    );
    let base = IdsFile {
        tables: [(uid("t_aaaaaa"), table("app.a"))].into(),
        columns: [(uid("c_aaaaaa"), "app.a.k".parse().unwrap())].into(),
        ..Default::default()
    };
    let goal = IdsFile {
        tables: [(uid("t_aaaaaa"), table(final_name))].into(),
        columns: [(uid("c_aaaaaa"), table(final_name).column("k"))].into(),
        ..Default::default()
    };
    let mut shape = before.tables[&table("app.a")].clone();
    shape.checks.clear();
    let after = Schema {
        tables: [(table(final_name), shape)].into(),
        ..Default::default()
    };
    let mut changes = vec![Change::RenameTable {
        uid: uid("t_aaaaaa"),
        from: table("app.a"),
        to: table("app.b"),
        defaults: vec![],
    }];
    let second = Change::RenameTable {
        uid: uid("t_aaaaaa"),
        from: table("app.b"),
        to: table(final_name),
        defaults: vec![],
    };
    let drop = Change::DropCheck {
        table: table(if read_after_last { final_name } else { "app.b" }),
        name: "ck".into(),
    };
    changes.extend(if read_after_last {
        vec![second, drop]
    } else {
        vec![drop, second]
    });
    let steps = ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    };
    let reader = if read_after_last { 2 } else { 1 };
    let raw = Operations::expanded(
        3,
        steps.changes.iter().enumerate().map(|(i, p)| {
            (
                if i == reader {
                    Origin::Teardown(Source::Change(i))
                } else {
                    Origin::Whole(i)
                },
                p.clone(),
            )
        }),
        ModuleEdits::Rebuilt,
    );
    let chain = operation::AddressChain::bind(
        &PlanNames::of(&steps.changes, &goal).unwrap(),
        &raw,
        &base,
        &goal,
        &Default::default(),
        &[raw.as_slice()[reader].id()].into(),
    );
    let operations = raw.with_address_chain(chain);
    (before, base, after, goal, steps, operations)
}

#[test]
fn an_intermediate_address_consumes_its_exact_version_without_changing_provenance() {
    let (before, base, after, goal, steps, operations) = rename_reader("app.c", false);
    let op = &operations.as_slice()[1];
    assert_eq!(op.logical_origin(), &Origin::Teardown(Source::Change(1)));
    assert!(matches!(op.origin().spelling(),Spelling::Input(id) if id==op.id()));
    let owners = PlanOwners::new(PlanNames::of(&steps.changes, &goal).unwrap(), &base, &goal);
    let ObjectKey::Part { table: owner, .. } = op.key(&owners).unwrap() else {
        panic!()
    };
    assert_eq!(owner.uid(), &uid("t_aaaaaa"));
    let fx = operation::effects(
        op,
        &owners,
        Snapshot {
            schema: &before,
            ids: &base,
        },
        Snapshot {
            schema: &after,
            ids: &goal,
        },
    )
    .unwrap();
    assert!(!fx.removes.is_empty());
    assert_eq!(op.address_before(&operations.as_slice()[0]), Some(false));
    assert_eq!(op.address_before(&operations.as_slice()[2]), Some(true));
    let input = op.input_owners(&owners);
    assert!(matches!(
        input.table(&table("app.a"), op.origin().spelling()),
        Err(OwnerError::Vacant { .. })
    ));
    assert!(matches!(
        input.table(&table("app.unknown"), op.origin().spelling()),
        Err(OwnerError::Unmanaged { .. })
    ));
    assert!(
        matches!(
            owners.table(&table("app.b"), op.origin().spelling()),
            Err(OwnerError::Outside { .. })
        ),
        "missing chain is not empty evidence"
    );
    let moved = Operations::from_operations(operations.iter().rev().cloned());
    assert_eq!(
        moved.as_slice()[1].key(&owners).unwrap(),
        op.key(&owners).unwrap()
    );
}

#[test]
fn equal_endpoint_names_do_not_erase_the_intervening_address_versions() {
    let (_, base, _, goal, steps, operations) = rename_reader("app.a", true);
    let op = &operations.as_slice()[2];
    assert_eq!(op.logical_origin().spelling(), Spelling::Base);
    assert_eq!(op.origin().spelling(), Spelling::Final);
    assert_eq!(op.address_before(&operations.as_slice()[0]), Some(false));
    assert_eq!(op.address_before(&operations.as_slice()[1]), Some(false));
    let owners = PlanOwners::new(PlanNames::of(&steps.changes, &goal).unwrap(), &base, &goal);
    let ObjectKey::Part { table: owner, .. } = op.key(&owners).unwrap() else {
        panic!()
    };
    assert_eq!(owner.uid(), &uid("t_aaaaaa"));
}
