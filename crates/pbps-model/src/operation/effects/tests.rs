//! How `effects` reads the three answers to a name (DEC-2993.1).
//!
//! The owners here answer from a script, so one name can be made to say each
//! thing a lookup can say: held, undeclared, vacant, or one of the three
//! refusals that are never an answer.

use super::*;
use crate::change::DeleteCause;
use crate::operation::{ModuleEdits, Operations};
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
}

impl Script {
    fn table(mut self, name: &str, reply: Reply) -> Self {
        self.tables.push((t(name), reply));
        self
    }
    fn held_table(self, name: &str) -> Self {
        self.table(name, Reply::Held(uid(UidKind::Table, name)))
    }
    fn column(mut self, table: &str, name: &str, reply: Reply) -> Self {
        self.columns.push((t(table).column(name), reply));
        self
    }
    fn held_column(self, table: &str, name: &str) -> Self {
        let uid = uid(UidKind::Column, name);
        self.column(table, name, Reply::Held(uid))
    }
}

impl Owners for Script {
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

/// The effects of one step over the scripted names and the desired state.
fn run(change: Change, owners: &Script, after: &Schema) -> Result<Effects, OwnerError> {
    let ops = Operations::of_plan(&[PlannedChange::new(change)], ModuleEdits::InPlace);
    let no_ids = IdsFile::default();
    let mut after_ids = IdsFile::default();
    for (name, reply) in &owners.tables {
        if let Reply::Held(uid) | Reply::HeldAtFinalOnly(uid) = reply {
            after_ids.tables.insert(uid.clone(), name.clone());
        }
    }
    let before_schema = Schema::default();
    effects(
        &ops.as_slice()[0],
        owners,
        Snapshot {
            schema: &before_schema,
            ids: &no_ids,
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
