//! How `effects` reads the three answers to a name (DEC-2993.1).
//!
//! The owners here answer from a script, so one name can be made to say each
//! thing a lookup can say: held, undeclared, vacant, or one of the three
//! refusals that are never an answer.

use super::*;
use crate::change::DeleteCause;
use crate::operation::{ModuleEdits, Named, Operations, baseline_names};
use crate::{ComputedColumn, Index};
use crate::{
    ConstraintChange, ForeignKey, GrantChange, IndexColumn, PartitionBound, PartitionOf,
    PlannedChange, RoleName, RowKey, UidKind, UniqueConstraint,
};

fn uid(kind: UidKind, name: &str) -> crate::Uid {
    crate::Uid::derived(kind, name, 0)
}
fn t(name: &str) -> TableName {
    TableName::new("app", name)
}
fn recorded(kind: UidKind, name: &str) -> Recorded {
    Recorded(uid(kind, name))
}
fn table_key(name: &str) -> ObjectKey {
    ObjectKey::Table(recorded(UidKind::Table, name))
}
fn column_key(name: &str) -> ObjectKey {
    ObjectKey::Column(ColumnKey::Recorded(recorded(UidKind::Column, name)))
}

/// What a name answers when it is asked.
#[derive(Clone)]
enum Reply {
    Held(crate::Uid),
    /// Held in the desired state and vacant at every other moment, which is
    /// how a partition parent answers a step that runs before its creation.
    HeldAtFinalOnly(crate::Uid),
    Fail(OwnerError),
}

impl Reply {
    fn answer(&self, name: &str, spelling: Spelling) -> Result<crate::Uid, OwnerError> {
        match self {
            Reply::Held(uid) => Ok(uid.clone()),
            Reply::HeldAtFinalOnly(uid) if matches!(spelling, Spelling::Final) => Ok(uid.clone()),
            Reply::HeldAtFinalOnly(_) => Err(OwnerError::Vacant {
                name: name.to_string(),
                spelling,
            }),
            Reply::Fail(why) => Err(why.clone()),
        }
    }
}

/// The three refusals and the vacant name: every answer that is not held and
/// not undeclared. `Outside` carries no name, so its refusal cannot name one.
fn faults(name: &str) -> Vec<OwnerError> {
    vec![
        OwnerError::Vacant {
            name: name.to_string(),
            spelling: Spelling::Base,
        },
        OwnerError::Names {
            name: name.to_string(),
            spelling: Spelling::Base,
            why: "two holders".to_string(),
        },
        OwnerError::Outside {
            spelling: Spelling::Base,
        },
        OwnerError::Rebuild {
            name: name.to_string(),
            why: "no such rebuild",
        },
    ]
}

#[derive(Default, Clone)]
struct Script {
    tables: Vec<(TableName, Reply)>,
    columns: Vec<(ColumnRef, Reply)>,
    roles: Vec<(RoleName, Reply)>,
    types: Vec<(crate::TypeRef, Reply)>,
}

impl Script {
    fn table(mut self, name: &str, reply: Reply) -> Self {
        self.tables.retain(|(n, _)| *n != t(name));
        self.tables.push((t(name), reply));
        self
    }
    fn held_table(self, name: &str) -> Self {
        self.table(name, Reply::Held(uid(UidKind::Table, name)))
    }
    fn column(mut self, table: &str, name: &str, reply: Reply) -> Self {
        let column = t(table).column(name);
        // A later answer replaces an earlier one for the same name.
        self.columns.retain(|(c, _)| *c != column);
        self.columns.push((column, reply));
        self
    }
    fn role(mut self, name: &str, reply: Reply) -> Self {
        self.roles.push((RoleName::new(name), reply));
        self
    }
    fn ty(mut self, name: &str, reply: Reply) -> Self {
        self.types.push((mood(name), reply));
        self
    }
    fn held_column(self, table: &str, name: &str) -> Self {
        let uid = uid(UidKind::Column, name);
        self.column(table, name, Reply::Held(uid))
    }
}

impl Owners for Script {
    fn managed_type(
        &self,
        name: &crate::TypeRef,
        spelling: Spelling,
    ) -> Result<crate::Uid, OwnerError> {
        let shown = name.shown().to_string();
        match self.types.iter().find(|(n, _)| n == name) {
            Some((_, reply)) => reply.answer(&shown, spelling),
            None => Err(OwnerError::Unmanaged {
                name: shown,
                spelling,
            }),
        }
    }

    fn table(&self, name: &TableName, spelling: Spelling) -> Result<crate::Uid, OwnerError> {
        let shown = format!("{}.{}", name.schema, name.name);
        match self.tables.iter().find(|(n, _)| n == name) {
            Some((_, reply)) => reply.answer(&shown, spelling),
            None => Err(OwnerError::Unmanaged {
                name: shown,
                spelling,
            }),
        }
    }

    fn column(&self, column: &ColumnRef, spelling: Spelling) -> Result<crate::Uid, OwnerError> {
        let shown = format!(
            "{}.{}.{}",
            column.table.schema,
            column.table.name,
            column.name.shown()
        );
        match self.columns.iter().find(|(c, _)| c == column) {
            Some((_, reply)) => reply.answer(&shown, spelling),
            None => Err(OwnerError::Unmanaged {
                name: shown,
                spelling,
            }),
        }
    }

    fn role(&self, name: &RoleName, spelling: Spelling) -> Result<crate::Uid, OwnerError> {
        match self.roles.iter().find(|(r, _)| r == name) {
            Some((_, reply)) => reply.answer(name.raw(), spelling),
            None => Err(OwnerError::Unmanaged {
                name: name.raw().to_string(),
                spelling,
            }),
        }
    }
}

fn mood(name: &str) -> crate::TypeRef {
    crate::TypeRef::new("app", name).unwrap()
}

/// The effects of one step over the scripted names and the desired state.
fn run(change: Change, owners: &Script, after: &Schema) -> Result<Effects, OwnerError> {
    run_over(
        change,
        owners,
        &Schema::default(),
        &IdsFile::default(),
        after,
    )
}

/// The same, over a baseline that holds tables.
fn run_over(
    change: Change,
    owners: &Script,
    before_schema: &Schema,
    no_ids: &IdsFile,
    after: &Schema,
) -> Result<Effects, OwnerError> {
    let ops = Operations::of_plan(&[PlannedChange::new(change)], ModuleEdits::InPlace);
    let mut after_ids = IdsFile::default();
    for (name, reply) in &owners.tables {
        if let Reply::Held(uid) | Reply::HeldAtFinalOnly(uid) = reply {
            after_ids.tables.insert(uid.clone(), name.clone());
        }
    }
    effects(
        &ops.as_slice()[0],
        owners,
        Snapshot {
            schema: before_schema,
            ids: no_ids,
        },
        Snapshot {
            schema: after,
            ids: &after_ids,
        },
    )
}

fn plain(change: Change, owners: &Script) -> Result<Effects, OwnerError> {
    run(change, owners, &Schema::default())
}

fn foreign_key(from: &str, to: &str) -> Change {
    Change::Constraint(ConstraintChange::AddForeignKey {
        table: t(from),
        name: "fk".into(),
        constraint: Box::new(ForeignKey {
            columns: vec!["c".into()],
            references_table: t(to),
            references_columns: vec!["id".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        }),
    })
}

fn delete_row(table: &str) -> Change {
    Change::Row(crate::RowChange::DeleteRow {
        table: t(table),
        key_column: "id".into(),
        key: RowKey::from("1"),
        cause: DeleteCause::Undeclared,
        row: Default::default(),
        dropped: Default::default(),
        types: Default::default(),
        after_types: Default::default(),
        key_type: None,
    })
}

fn grant(role: &str) -> Change {
    Change::Permission(GrantChange::Grant {
        role: RoleName::new(role),
        target: GrantTarget::Schema("app".to_string()),
        permissions: Default::default(),
    })
}

fn unique_on(table: &str, column: &str) -> Change {
    Change::Constraint(ConstraintChange::AddUnique {
        table: t(table),
        name: "uq".into(),
        constraint: UniqueConstraint {
            columns: vec![column.into()],
            storage_parameters: Default::default(),
            filegroup: None,
        },
        clustered: false,
    })
}

fn add_index(table: &str) -> Change {
    Change::Index(crate::IndexChange::AddIndex {
        table: t(table),
        name: "ix".into(),
        index: Box::new(index_on("id")),
        clustered: false,
    })
}

fn index_on(column: &str) -> Index {
    Index {
        columns: vec![IndexColumn {
            key: IndexKey::Column(column.into()),
            descending: false,
            opclass: None,
            collation: None,
            opclass_parameters: Default::default(),
        }],
        include: vec![],
        unique: false,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
        filegroup: None,
    }
}

fn exists(fx: &Effects, key: &ObjectKey) -> bool {
    fx.requires.contains(&Requirement::Exists(key.clone()))
}

fn ensure_refuses(site: &str, name: &str, run_with: impl Fn(Reply) -> Result<Effects, OwnerError>) {
    ensure_these_refuse(site, name, faults(name), run_with);
}

fn ensure_these_refuse(
    site: &str,
    name: &str,
    which: Vec<OwnerError>,
    run_with: impl Fn(Reply) -> Result<Effects, OwnerError>,
) {
    for why in which {
        let Err(got) = run_with(Reply::Fail(why.clone())) else {
            panic!("{site}: `{why}` was read as an answer");
        };
        assert_eq!(got, why, "{site} must return the original refusal");
        if !matches!(why, OwnerError::Outside { .. }) {
            assert!(
                got.to_string().contains(name),
                "{site}: the refusal `{got}` does not name `{name}`"
            );
        }
    }
}

// A table the step points at.

#[test]
fn a_held_referenced_table_is_required_to_exist() {
    let script = Script::default()
        .held_table("child")
        .held_table("parent")
        .held_column("child", "c");
    let fx = plain(foreign_key("child", "parent"), &script).unwrap();
    assert!(exists(&fx, &table_key("parent")));
    assert!(exists(&fx, &table_key("child")));
}

#[test]
fn an_undeclared_referenced_table_states_nothing_and_keeps_the_steps_other_facts() {
    let script = Script::default()
        .held_table("child")
        .held_column("child", "c");
    let fx = plain(foreign_key("child", "parent"), &script).unwrap();
    assert!(exists(&fx, &table_key("child")));
    assert!(exists(&fx, &column_key("c")));
    assert!(
        fx.requires
            .contains(&Requirement::Reads(table_key("child")))
    );
    let tables: Vec<_> = fx
        .requires
        .iter()
        .filter(|r| matches!(r.key(), ObjectKey::Table(_)))
        .map(Requirement::key)
        .collect();
    assert!(
        tables.iter().all(|key| **key == table_key("child")),
        "nothing is said of the undeclared table: {tables:?}"
    );
}

#[test]
fn an_unreadable_or_vacant_referenced_table_refuses_and_names_it() {
    ensure_refuses("AddForeignKey", "app.parent", |reply| {
        let script = Script::default()
            .held_table("child")
            .held_column("child", "c")
            .table("parent", reply);
        plain(foreign_key("child", "parent"), &script)
    });
}

// The table the step changes.

#[test]
fn the_table_a_step_changes_must_be_held() {
    let script = Script::default();
    assert!(matches!(
        plain(foreign_key("child", "parent"), &script),
        Err(OwnerError::Unmanaged { .. })
    ));
    ensure_refuses("AddForeignKey subject", "app.child", |reply| {
        plain(
            foreign_key("child", "parent"),
            &Script::default().table("child", reply),
        )
    });
}

// A column the step points at.

#[test]
fn a_held_column_is_required_to_exist() {
    let script = Script::default().held_table("t").held_column("t", "id");
    let fx = plain(delete_row("t"), &script).unwrap();
    assert!(exists(&fx, &column_key("id")));
}

#[test]
fn an_undeclared_column_states_nothing_and_keeps_the_steps_other_facts() {
    let fx = plain(delete_row("t"), &Script::default().held_table("t")).unwrap();
    assert!(
        !fx.requires
            .iter()
            .any(|r| matches!(r.key(), ObjectKey::Column(_)))
    );
    assert!(
        !fx.rewrites.is_empty(),
        "the rows it rewrites are still stated"
    );
    assert!(exists(&fx, &table_key("t")));
}

#[test]
fn an_unreadable_or_vacant_column_refuses_and_names_it() {
    ensure_refuses("DeleteRow key column", "app.t.id", |reply| {
        plain(
            delete_row("t"),
            &Script::default().held_table("t").column("t", "id", reply),
        )
    });
}

// A role the step points at.

#[test]
fn a_held_role_is_required_to_exist() {
    let mut script = Script::default();
    script
        .roles
        .push((RoleName::new("r"), Reply::Held(uid(UidKind::Role, "r"))));
    let fx = plain(grant("r"), &script).unwrap();
    assert!(exists(&fx, &ObjectKey::Role(recorded(UidKind::Role, "r"))));
}

#[test]
fn an_undeclared_role_states_nothing_and_keeps_the_grant() {
    let fx = plain(grant("cluster_role"), &Script::default()).unwrap();
    assert!(
        !fx.requires
            .iter()
            .any(|r| matches!(r.key(), ObjectKey::Role(_)))
    );
    assert_eq!(fx.rewrites.len(), 1, "the grant itself is still stated");
}

#[test]
fn an_unreadable_or_vacant_role_refuses_and_names_it() {
    ensure_refuses("Grant role", "r", |reply| {
        let mut script = Script::default();
        script.roles.push((RoleName::new("r"), reply));
        plain(grant("r"), &script)
    });
}

// A table a key binds to, and a table a grant is on.

#[test]
fn a_created_table_with_a_key_into_an_undeclared_table_binds_nothing_and_is_created() {
    let uid = uid(UidKind::Table, "child");
    let mut table = Table::default();
    table.foreign_keys.insert(
        "fk".into(),
        ForeignKey {
            columns: vec!["c".into()],
            references_table: t("far"),
            references_columns: vec!["id".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    let create = Change::Table(TableChange::CreateTable {
        uid: uid.clone(),
        name: t("child"),
        table: Box::new(table),
        beside_default: None,
        moves_at_most: None,
    });
    let script = Script::default().held_table("child");
    let fx = plain(create.clone(), &script).unwrap();
    assert!(fx.bindings.is_empty());
    assert!(
        fx.creates
            .contains(&Resource::Object(table_key("child"), Incarnation::Final))
    );
    // A vacant target is not the same as an undeclared one.
    let vacant = script.table(
        "far",
        Reply::Fail(OwnerError::Vacant {
            name: "app.far".to_string(),
            spelling: Spelling::Base,
        }),
    );
    assert!(matches!(
        plain(create, &vacant),
        Err(OwnerError::Vacant { .. })
    ));
}

#[test]
fn a_grant_on_an_undeclared_table_states_no_target() {
    let mut after = Schema::default();
    after.tables.insert(t("x"), Table::default());
    let grant_on = Change::Permission(GrantChange::Grant {
        role: RoleName::new("r"),
        target: GrantTarget::Object(t("x")),
        permissions: Default::default(),
    });
    let fx = run(grant_on.clone(), &Script::default(), &after).unwrap();
    assert!(
        !fx.requires
            .iter()
            .any(|r| matches!(r.key(), ObjectKey::Table(_)))
    );
    assert_eq!(fx.rewrites.len(), 1);
    ensure_refuses("Grant target", "app.x", |reply| {
        run(
            grant_on.clone(),
            &Script::default().table("x", reply),
            &after,
        )
    });
}

// The partitioned parent of an index.

fn partition_of_parent(parent: &str) -> Schema {
    let mut after = Schema::default();
    after.tables.insert(
        t("child"),
        Table {
            partition_of: Some(PartitionOf {
                parent: t(parent),
                bound: PartitionBound::Default,
                columns: Default::default(),
            }),
            ..Table::default()
        },
    );
    let mut parent_table = Table::default();
    parent_table.indexes.insert("px".into(), index_on("id"));
    after.tables.insert(t(parent), parent_table);
    after
}

#[test]
fn an_index_on_a_partition_asks_its_parent_in_the_desired_state() {
    let after = partition_of_parent("parent");
    let owners = Script::default()
        .held_table("child")
        .held_column("child", "id")
        .table(
            "parent",
            Reply::HeldAtFinalOnly(uid(UidKind::Table, "parent")),
        );
    let fx = run(add_index("child"), &owners, &after).unwrap();
    assert!(fx.requires.contains(&Requirement::Is(
        ObjectKey::Part {
            table: recorded(UidKind::Table, "parent"),
            part: PartKey::Index("px".into()),
        },
        Incarnation::Final,
    )));
}

#[test]
fn an_index_on_a_partition_of_an_undeclared_parent_states_no_parent_index() {
    let after = partition_of_parent("parent");
    let owners = Script::default()
        .held_table("child")
        .held_column("child", "id");
    let fx = run(add_index("child"), &owners, &after).unwrap();
    assert!(exists(&fx, &table_key("child")));
    assert!(!fx.requires.iter().any(|r| matches!(
        r,
        Requirement::Is(
            ObjectKey::Part {
                part: PartKey::Index(_),
                ..
            },
            _
        )
    )));
    ensure_refuses("AddIndex parent", "app.parent", |reply| {
        let owners = Script::default()
            .held_table("child")
            .held_column("child", "id")
            .table("parent", reply);
        run(add_index("child"), &owners, &after)
    });
}

// A computed column has no column uid.

fn with_computed(name: &str) -> Schema {
    let mut after = Schema::default();
    let mut table = Table::default();
    table.computed.insert(
        name.into(),
        ComputedColumn {
            expression: "1".to_string(),
            persisted: false,
            not_null: false,
        },
    );
    after.tables.insert(t("t"), table);
    after
}

fn computed_part() -> ObjectKey {
    ObjectKey::Part {
        table: recorded(UidKind::Table, "t"),
        part: PartKey::Computed("c".into()),
    }
}

#[test]
fn a_computed_column_is_found_among_the_computed_parts_whichever_answer_the_column_gave() {
    let after = with_computed("c");
    // Never an ordinary column: undeclared. Replacing one of the same name,
    // once that one is dropped: vacant.
    let vacant = Reply::Fail(OwnerError::Vacant {
        name: "app.t.c".to_string(),
        spelling: Spelling::Base,
    });
    for answer in [None, Some(vacant)] {
        let mut owners = Script::default().held_table("t");
        if let Some(reply) = answer {
            owners = owners.column("t", "c", reply);
        }
        let fx = run(unique_on("t", "c"), &owners, &after).unwrap();
        assert!(exists(&fx, &computed_part()));
        assert!(exists(&fx, &table_key("t")), "the step's other facts stay");
    }
}

#[test]
fn a_column_that_is_no_computed_column_states_nothing_when_undeclared_and_refuses_when_vacant() {
    let owners = Script::default().held_table("t");
    let fx = run(unique_on("t", "c"), &owners, &with_computed("other")).unwrap();
    assert!(!fx.requires.iter().any(|r| matches!(
        r.key(),
        ObjectKey::Column(_)
            | ObjectKey::Part {
                part: PartKey::Computed(_),
                ..
            }
    )));
    assert!(exists(&fx, &table_key("t")));
    let vacant = OwnerError::Vacant {
        name: "app.t.c".to_string(),
        spelling: Spelling::Base,
    };
    let owners = owners.column("t", "c", Reply::Fail(vacant.clone()));
    assert_eq!(
        run(unique_on("t", "c"), &owners, &with_computed("other")).unwrap_err(),
        vacant
    );
}

#[test]
fn an_unreadable_column_refuses_even_where_a_computed_column_has_its_name() {
    let after = with_computed("c");
    // Vacant is the one fault a computed column answers: it is a replaced
    // ordinary column's history, which the computed part explains.
    let unreadable = faults("app.t.c")
        .into_iter()
        .filter(|why| !matches!(why, OwnerError::Vacant { .. }))
        .collect();
    ensure_these_refuse("AddUnique column", "app.t.c", unreadable, |reply| {
        let owners = Script::default().held_table("t").column("t", "c", reply);
        run(unique_on("t", "c"), &owners, &after)
    });
}

// The lookups that only point at a name (DEC-2993.2).

/// A step's context over scripted owners, to ask the lookups themselves.
fn with_cx<R>(
    owners: &Script,
    before: (&Schema, &IdsFile),
    after: (&Schema, &IdsFile),
    ask: impl FnOnce(&mut Cx<'_>) -> R,
) -> R {
    with_cx_for(delete_row("t"), owners, before, after, ask)
}

fn with_cx_for<R>(
    change: Change,
    owners: &Script,
    before: (&Schema, &IdsFile),
    after: (&Schema, &IdsFile),
    ask: impl FnOnce(&mut Cx<'_>) -> R,
) -> R {
    let ops = Operations::of_plan(&[PlannedChange::new(change)], ModuleEdits::InPlace);
    let op = &ops.as_slice()[0];
    let mut cx = Cx {
        op,
        owners,
        before: Snapshot {
            schema: before.0,
            ids: before.1,
        },
        after: Snapshot {
            schema: after.0,
            ids: after.1,
        },
        spelling: op.origin().spelling(),
        fx: Effects::default(),
    };
    ask(&mut cx)
}

fn empty() -> (Schema, IdsFile) {
    (Schema::default(), IdsFile::default())
}

fn column_ref(table: &str, name: &str) -> ColumnRef {
    t(table).column(name)
}

#[test]
fn the_pointing_lookups_give_the_key_when_held_and_nothing_when_undeclared() {
    let (schema, ids) = empty();
    let script = Script::default().held_table("p").held_column("p", "c");
    with_cx(&script, (&schema, &ids), (&schema, &ids), |cx| {
        let table = Some(recorded(UidKind::Table, "p"));
        let column = Some(ColumnKey::Recorded(recorded(UidKind::Column, "c")));
        assert_eq!(cx.base_table(&t("p")).unwrap(), table);
        assert_eq!(cx.final_table(&t("p")).unwrap(), table);
        assert_eq!(cx.base_column(&column_ref("p", "c")).unwrap(), column);
        assert_eq!(cx.final_column(&column_ref("p", "c")).unwrap(), column);
        assert_eq!(cx.base_table(&t("far")).unwrap(), None);
        assert_eq!(cx.final_table(&t("far")).unwrap(), None);
        assert_eq!(cx.base_column(&column_ref("far", "c")).unwrap(), None);
        assert_eq!(cx.final_column(&column_ref("far", "c")).unwrap(), None);
    });
}

#[test]
fn the_pointing_lookups_return_the_original_refusal_for_a_vacant_or_unreadable_name() {
    let (schema, ids) = empty();
    for why in faults("app.p") {
        let script = Script::default()
            .table("p", Reply::Fail(why.clone()))
            .column("p", "c", Reply::Fail(why.clone()));
        with_cx(&script, (&schema, &ids), (&schema, &ids), |cx| {
            assert_eq!(cx.base_table(&t("p")), Err(why.clone()), "base_table");
            assert_eq!(cx.final_table(&t("p")), Err(why.clone()), "final_table");
            let column = column_ref("p", "c");
            assert_eq!(cx.base_column(&column), Err(why.clone()), "base_column");
            assert_eq!(cx.final_column(&column), Err(why.clone()), "final_column");
        });
    }
}

#[test]
fn a_vacant_table_a_drop_foreign_key_points_at_refuses_and_an_undeclared_one_does_not() {
    let mut before = Schema::default();
    let mut child = Table::default();
    child.foreign_keys.insert(
        "fk".into(),
        ForeignKey {
            columns: vec!["c".into()],
            references_table: t("parent"),
            references_columns: vec!["id".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    before.tables.insert(t("child"), child);
    let mut ids = IdsFile::default();
    ids.tables.insert(uid(UidKind::Table, "child"), t("child"));
    let drop = Change::Constraint(ConstraintChange::DropForeignKey {
        table: t("child"),
        name: "fk".into(),
    });
    let script = Script::default().held_table("child");
    let after = Schema::default();
    // `Cx::base_table` at the step `DropForeignKey` asks for its target.
    assert!(run_over(drop.clone(), &script, &before, &ids, &after).is_ok());
    let vacant = OwnerError::Vacant {
        name: "app.parent".to_string(),
        spelling: Spelling::Base,
    };
    let script = script.table("parent", Reply::Fail(vacant.clone()));
    assert_eq!(
        run_over(drop, &script, &before, &ids, &after).unwrap_err(),
        vacant
    );
}

#[test]
fn the_renames_of_a_table_keep_the_rest_when_a_key_target_is_undeclared_and_refuse_when_unreadable()
{
    let mut child = Table::default();
    child.foreign_keys.insert(
        "fk".into(),
        ForeignKey {
            columns: vec!["c".into()],
            references_table: t("far"),
            references_columns: vec!["id".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    let mut before = Schema::default();
    before.tables.insert(t("child"), child.clone());
    let mut before_ids = IdsFile::default();
    before_ids
        .tables
        .insert(uid(UidKind::Table, "child"), t("child"));
    // The plan renames `child` to `kid`.
    let mut after_ids = IdsFile::default();
    after_ids
        .tables
        .insert(uid(UidKind::Table, "child"), t("kid"));
    let after = Schema::default();
    let script = Script::default().held_table("child");
    let renames = with_cx(
        &script,
        (&before, &before_ids),
        (&after, &after_ids),
        |cx| cx.renames_of(&t("child"), &child),
    )
    .unwrap();
    let mut expected = crate::rename::Renames::default();
    expected.rename_table(t("child"), t("kid"));
    assert_eq!(renames, expected, "the undeclared target leaves the rename");
    for why in faults("app.far") {
        let script = script.clone().table("far", Reply::Fail(why.clone()));
        let got = with_cx(
            &script,
            (&before, &before_ids),
            (&after, &after_ids),
            |cx| cx.renames_of(&t("child"), &child),
        );
        assert_eq!(got, Err(why));
    }
}

fn type_input_of(
    script: &Script,
    after: (&Schema, &IdsFile),
) -> Result<Vec<Requirement>, OwnerError> {
    let (schema, ids) = empty();
    with_cx(script, (&schema, &ids), after, |cx| {
        cx.type_input(&crate::ColumnType::reference(mood("mood")))?;
        Ok(cx.fx.requires.clone())
    })
}

fn package_in_app() -> (Schema, IdsFile) {
    let mut schema = Schema::default();
    schema.extensions.insert(
        "citext".into(),
        crate::Extension {
            schema: "app".to_string(),
            version: "1".to_string(),
        },
    );
    let mut ids = IdsFile::default();
    ids.extensions
        .insert(uid(UidKind::Extension, "citext"), "citext".into());
    (schema, ids)
}

#[test]
fn a_held_type_is_required_in_its_final_incarnation_and_no_package_is() {
    let (schema, ids) = package_in_app();
    let script = Script::default().ty("mood", Reply::Held(uid(UidKind::Type, "mood")));
    let required = type_input_of(&script, (&schema, &ids)).unwrap();
    let ty = ObjectKey::Type(recorded(UidKind::Type, "mood"));
    assert!(required.contains(&Requirement::Is(ty, Incarnation::Final)));
    assert_eq!(required.len(), 1, "{required:?}");
}

#[test]
fn an_undeclared_type_waits_for_a_package_in_its_schema_and_for_nothing_else() {
    let (schema, ids) = package_in_app();
    let script = Script::default();
    let required = type_input_of(&script, (&schema, &ids)).unwrap();
    let package = ObjectKey::Extension(recorded(UidKind::Extension, "citext"));
    assert_eq!(
        required.into_iter().collect::<Vec<_>>(),
        [Requirement::Is(package, Incarnation::Final)]
    );
    let (schema, ids) = empty();
    assert!(type_input_of(&script, (&schema, &ids)).unwrap().is_empty());
}

#[test]
fn a_vacant_or_unreadable_type_refuses_and_is_never_taken_for_a_packages() {
    let (schema, ids) = package_in_app();
    for why in faults("app.mood") {
        let script = Script::default().ty("mood", Reply::Fail(why.clone()));
        assert_eq!(type_input_of(&script, (&schema, &ids)), Err(why));
    }
}

fn copy_rows(source: &str, target: &str) -> Change {
    let relation = |name: &str| crate::RebuildRelation {
        uid: uid(UidKind::Table, name),
        name: t(name),
    };
    Change::Partition(crate::PartitionChange::CopyTableRows(Box::new(
        crate::CopyTableRows {
            source: relation("src"),
            target: relation("dst"),
            columns: vec![crate::CopyColumn {
                source: Some(crate::CopySource {
                    column: column_ref("src", source),
                    ty: crate::ColumnType::simple("int"),
                    collation: None,
                }),
                target: column_ref("dst", target),
                cast: crate::ColumnType::simple("int"),
                collation: None,
                stages: Vec::new(),
            }],
            source_partitions: Vec::new(),
            target_partitions: Vec::new(),
            max_rows: 1,
        },
    )))
}

fn copy_script() -> Script {
    Script::default()
        .held_table("src")
        .held_table("dst")
        .held_column("src", "a")
        .held_column("dst", "b")
}

/// What the step's own effects say, before the names it reads are listed:
/// `read_names` refuses the same columns again, so only this one shows which
/// lookup refused.
fn copy_effects(change: Change, owners: &Script) -> Result<Effects, OwnerError> {
    let (schema, ids) = empty();
    with_cx_for(change, owners, (&schema, &ids), (&schema, &ids), |cx| {
        cx.by_kind().map(|()| cx.fx.clone())
    })
}

#[test]
fn the_columns_a_copy_moves_are_read_and_written_when_held() {
    let fx = copy_effects(copy_rows("a", "b"), &copy_script()).unwrap();
    assert!(fx.requires.contains(&Requirement::Reads(column_key("a"))));
    assert!(
        fx.requires
            .contains(&Requirement::Is(column_key("b"), Incarnation::Final))
    );
}

#[test]
fn a_copy_refuses_a_column_that_is_undeclared_or_vacant_and_keeps_the_original_text() {
    // The source column nobody declares.
    let Err(OwnerError::Unmanaged { name, .. }) =
        copy_effects(copy_rows("gone", "b"), &copy_script())
    else {
        panic!("an undeclared source column must refuse");
    };
    assert_eq!(name, "app.src.gone");
    // The target column nobody declares.
    let Err(OwnerError::Unmanaged { name, .. }) =
        copy_effects(copy_rows("a", "gone"), &copy_script())
    else {
        panic!("an undeclared target column must refuse");
    };
    assert_eq!(name, "app.dst.gone");
    ensure_refuses("CopyTableRows source column", "app.src.a", |reply| {
        copy_effects(
            copy_rows("a", "b"),
            &copy_script().column("src", "a", reply),
        )
    });
    ensure_refuses("CopyTableRows target column", "app.dst.b", |reply| {
        copy_effects(
            copy_rows("a", "b"),
            &copy_script().column("dst", "b", reply),
        )
    });
}

fn base_of_child() -> (Schema, IdsFile) {
    let mut schema = Schema::default();
    schema.tables.insert(t("child"), Table::default());
    (schema, IdsFile::default())
}

#[test]
fn a_baseline_table_and_its_columns_are_found_when_held_and_missing_when_undeclared() {
    let (before, ids) = base_of_child();
    let snapshot = Snapshot {
        schema: &before,
        ids: &ids,
    };
    let name = t("child");
    let script = Script::default()
        .held_table("child")
        .held_column("child", "c");
    let found = base_table_named(&script, snapshot, &name).unwrap().unwrap();
    assert_eq!(found.table_key(), table_key("child"));
    assert_eq!(found.column(&script, "c").unwrap(), Some(column_key("c")));
    assert_eq!(found.column(&script, "gone").unwrap(), None);
    // A table the baseline lacks, or the project does not declare, is none.
    assert!(
        base_table_named(&script, snapshot, &t("absent"))
            .unwrap()
            .is_none()
    );
    let undeclared = Script::default();
    assert!(
        base_table_named(&undeclared, snapshot, &name)
            .unwrap()
            .is_none()
    );
}

#[test]
fn an_unreadable_baseline_table_or_column_is_the_original_refusal() {
    let (before, ids) = base_of_child();
    let snapshot = Snapshot {
        schema: &before,
        ids: &ids,
    };
    let name = t("child");
    for why in faults("app.child") {
        let script = Script::default().table("child", Reply::Fail(why.clone()));
        assert_eq!(
            base_table_named(&script, snapshot, &name).err(),
            Some(why.clone())
        );
        let script =
            Script::default()
                .held_table("child")
                .column("child", "c", Reply::Fail(why.clone()));
        let found = base_table_named(&script, snapshot, &name).unwrap().unwrap();
        assert_eq!(found.column(&script, "c").err(), Some(why));
    }
}

// The names beside the effects (`slots.rs`).

fn baseline_with_everything() -> (Schema, IdsFile) {
    let mut schema = Schema::default();
    let mut table = Table::default();
    table.columns.insert(
        "c".into(),
        crate::Column::new(crate::ColumnType::simple("int")),
    );
    schema.tables.insert(t("child"), table);
    schema
        .roles
        .insert(RoleName::new("ops"), Default::default());
    (schema, IdsFile::default())
}

fn baseline_names_of(script: &Script) -> Result<Vec<Named>, OwnerError> {
    let (before, ids) = baseline_with_everything();
    baseline_names(
        script,
        Snapshot {
            schema: &before,
            ids: &ids,
        },
    )
}

#[test]
fn baseline_names_skip_what_the_project_does_not_declare_and_keep_the_rest() {
    let script = Script::default()
        .held_table("child")
        .held_column("child", "c");
    let held = baseline_names_of(&script).unwrap();
    assert!(held.iter().any(|n| n.holder == column_key("c")));
    // No role is declared: the table and its column stay.
    let nothing = baseline_names_of(&Script::default()).unwrap();
    assert!(nothing.is_empty(), "{nothing:?}");
    let table_only = baseline_names_of(&Script::default().held_table("child")).unwrap();
    assert!(table_only.iter().any(|n| n.holder == table_key("child")));
    assert!(table_only.iter().all(|n| n.holder != column_key("c")));
}

#[test]
fn baseline_names_refuse_an_unreadable_table_column_or_role_and_name_it() {
    for why in faults("app.child") {
        let table = Script::default().table("child", Reply::Fail(why.clone()));
        assert_eq!(baseline_names_of(&table).err(), Some(why.clone()), "table");
        let column =
            Script::default()
                .held_table("child")
                .column("child", "c", Reply::Fail(why.clone()));
        assert_eq!(
            baseline_names_of(&column).err(),
            Some(why.clone()),
            "column"
        );
        let role = Script::default().role("ops", Reply::Fail(why.clone()));
        assert_eq!(baseline_names_of(&role).err(), Some(why), "role");
    }
}

#[test]
fn baseline_keys_keep_held_names_and_refuse_unreadable_names() {
    let (mut schema, ids) = baseline_with_everything();
    schema.tables.insert(t("external"), Table::default());
    let before = Snapshot {
        schema: &schema,
        ids: &ids,
    };
    let held = Script::default().held_table("child");
    let table_only = baseline_keys(before, &held).unwrap();
    assert!(table_only.contains(&table_key("child")));
    assert!(!table_only.contains(&column_key("c")));
    assert!(!table_only.contains(&table_key("external")));
    let held = held.held_column("child", "c");
    let keys = baseline_keys(before, &held).unwrap();
    assert!(keys.contains(&table_key("child")));
    assert!(keys.contains(&column_key("c")));
    for why in faults("app.child") {
        let owners = held.clone().table("child", Reply::Fail(why.clone()));
        assert_eq!(baseline_keys(before, &owners), Err(why));
    }
    for why in faults("app.child.c") {
        let owners = held.clone().column("child", "c", Reply::Fail(why.clone()));
        assert_eq!(baseline_keys(before, &owners), Err(why));
    }
}

#[test]
fn baseline_bindings_keep_held_targets_and_refuse_unreadable_names() {
    let (mut schema, ids) = baseline_with_everything();
    let child = schema.tables.get_mut(&t("child")).unwrap();
    for name in ["parent", "external"] {
        let Change::Constraint(ConstraintChange::AddForeignKey { constraint, .. }) =
            foreign_key("child", name)
        else {
            unreachable!()
        };
        child.foreign_keys.insert(name.into(), *constraint);
    }
    let before = Snapshot {
        schema: &schema,
        ids: &ids,
    };
    let held = Script::default().held_table("parent");
    assert!(baseline_bindings(before, &held).unwrap().is_empty());
    let held = held.held_table("child");
    assert_eq!(
        baseline_bindings(before, &held).unwrap(),
        [ForeignKeyBinding {
            surface: ObjectKey::Part {
                table: recorded(UidKind::Table, "child"),
                part: PartKey::ForeignKey("parent".into()),
            },
            incarnation: Incarnation::Base,
            target: recorded(UidKind::Table, "parent"),
        }]
    );
    for name in ["child", "parent", "external"] {
        for why in faults(&format!("app.{name}")) {
            let owners = held.clone().table(name, Reply::Fail(why.clone()));
            assert_eq!(baseline_bindings(before, &owners), Err(why));
        }
    }
}

fn baseline_with_type() -> (Schema, IdsFile) {
    let mut schema = Schema::default();
    schema.types.insert(
        mood("mood"),
        crate::ManagedType::Enum {
            values: vec!["a".into()],
        },
    );
    (schema, IdsFile::default())
}

#[test]
fn baseline_names_refuse_an_unreadable_type() {
    let (before, ids) = baseline_with_type();
    let snapshot = Snapshot {
        schema: &before,
        ids: &ids,
    };
    assert!(
        baseline_names(&Script::default(), snapshot)
            .unwrap()
            .is_empty(),
        "an undeclared type holds no name"
    );
    for why in faults("app.mood") {
        let script = Script::default().ty("mood", Reply::Fail(why.clone()));
        assert_eq!(baseline_names(&script, snapshot).err(), Some(why));
    }
}

fn create_with_column() -> Change {
    let mut table = Table::default();
    table.columns.insert(
        "c".into(),
        crate::Column::new(crate::ColumnType::simple("int")),
    );
    Change::Table(TableChange::CreateTable {
        uid: uid(UidKind::Table, "child"),
        name: t("child"),
        table: Box::new(table),
        beside_default: None,
        moves_at_most: None,
    })
}

fn names_of(change: Change, script: &Script) -> Result<crate::operation::Names, OwnerError> {
    let ops = Operations::of_plan(&[PlannedChange::new(change)], ModuleEdits::InPlace);
    let (before, ids) = (Schema::default(), IdsFile::default());
    crate::operation::names(
        &ops.as_slice()[0],
        script,
        Snapshot {
            schema: &before,
            ids: &ids,
        },
    )
}

#[test]
fn a_created_tables_column_claims_its_name_when_held_not_when_undeclared_and_refuses_when_unreadable()
 {
    let held = Script::default()
        .held_table("child")
        .held_column("child", "c");
    let claimed = names_of(create_with_column(), &held).unwrap();
    assert!(claimed.claims.iter().any(|n| n.holder == column_key("c")));
    let undeclared = Script::default().held_table("child");
    let claimed = names_of(create_with_column(), &undeclared).unwrap();
    assert!(claimed.claims.iter().all(|n| n.holder != column_key("c")));
    assert!(
        claimed
            .claims
            .iter()
            .any(|n| n.holder == table_key("child"))
    );
    ensure_these_names_refuse("CreateTable column", "app.child.c", |reply| {
        let script = Script::default()
            .held_table("child")
            .column("child", "c", reply);
        names_of(create_with_column(), &script)
    });
}

fn drop_trigger() -> Change {
    Change::Module(crate::ModuleChange::DropModule {
        id: crate::ModuleId::Trigger {
            on: t("child"),
            name: "trg".into(),
        },
        kind: crate::ModuleKind::Trigger,
    })
}

#[test]
fn a_trigger_on_an_undeclared_table_has_no_scope_and_one_on_an_unreadable_table_refuses() {
    let held = Script::default().held_table("child");
    let released = names_of(drop_trigger(), &held).unwrap();
    assert_eq!(released.releases.len(), 1);
    assert_eq!(
        released.releases[0].address.table,
        Some(table_key("child")),
        "a held table is the trigger's scope"
    );
    let released = names_of(drop_trigger(), &Script::default()).unwrap();
    assert!(
        released.releases.is_empty(),
        "an undeclared table is nobody's scope: {released:?}"
    );
    ensure_these_names_refuse("DropModule trigger scope", "app.child", |reply| {
        names_of(drop_trigger(), &Script::default().table("child", reply))
    });
}

fn ensure_these_names_refuse<T>(
    site: &str,
    name: &str,
    run_with: impl Fn(Reply) -> Result<T, OwnerError>,
) {
    for why in faults(name) {
        let Err(got) = run_with(Reply::Fail(why.clone())) else {
            panic!("{site}: `{why}` was read as an answer");
        };
        assert_eq!(got, why, "{site} must return the original refusal");
    }
}

#[test]
fn the_public_base_table_needs_a_held_name_and_returns_every_other_answer_as_the_refusal() {
    let mut before = Schema::default();
    before.tables.insert(t("child"), Table::default());
    let mut ids = IdsFile::default();
    ids.tables.insert(uid(UidKind::Table, "child"), t("child"));
    let snapshot = Snapshot {
        schema: &before,
        ids: &ids,
    };
    let ops = Operations::of_plan(&[PlannedChange::new(delete_row("t"))], ModuleEdits::InPlace);
    let op = &ops.as_slice()[0];
    let held = Script::default().held_table("child");
    let found = base_table(op, &held, snapshot, &t("child"))
        .unwrap()
        .unwrap();
    assert_eq!(found.table_key(), table_key("child"));
    // Unlike the scans, a step that names an undeclared table has nothing to
    // order: the answer is the refusal, as it was before the door.
    assert!(matches!(
        base_table(op, &Script::default(), snapshot, &t("child")),
        Err(OwnerError::Unmanaged { .. })
    ));
    for why in faults("app.child") {
        let script = Script::default().table("child", Reply::Fail(why.clone()));
        assert_eq!(
            base_table(op, &script, snapshot, &t("child")).err(),
            Some(why)
        );
    }
}
