//! A partition's copies of its ancestor's declared objects, as a catalog read
//! carries them on 16 and 18 (#3075, DEC-3075.1).

use super::*;
use pbps_model::resolver::{ObjectOwnership, Surface};
use pbps_model::{
    CheckConstraint, Column, ForeignKey, IdsFile, Index, IndexColumn, PartitionBound, PartitionBy,
    PartitionOf, Schema, Table, TableName,
};
use serde_json::json;

fn identity(class: &str, name: &[&str], signature: &[&ObjectIdentity]) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name: name.iter().map(|part| (*part).into()).collect(),
        signature: signature.iter().map(|id| (*id).clone()).collect(),
    }
}

fn relation(name: &str) -> ObjectIdentity {
    identity("pg_class", &["app", name], &[])
}

fn constraint(name: &str, table: &str) -> ObjectIdentity {
    let namespace = identity("pg_namespace", &["app"], &[]);
    identity(
        "pg_constraint",
        &[name],
        &[&namespace, &relation(table), &identity("pg_type", &[], &[])],
    )
}

fn depend(kind: &str, made: &ObjectIdentity, maker: &ObjectIdentity) -> ObjectIdentity {
    identity("pg_depend", &[kind], &[made, maker])
}

/// The records of one catalog read, added by identity and properties.
#[derive(Default)]
struct Read(BTreeMap<ObjectIdentity, Input>);

impl Read {
    fn add(&mut self, object: ObjectIdentity, properties: &[(&str, Value)]) -> ObjectIdentity {
        let properties = properties
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect();
        self.0.insert(
            object.clone(),
            Input {
                properties,
                bindings: Vec::new(),
            },
        );
        object
    }

    /// An index of `table`, with the metadata row that names the table.
    fn index(&mut self, name: &str, table: &str) -> ObjectIdentity {
        let index = self.add(relation(name), &[("relkind", json!("i"))]);
        self.add(
            identity("pg_index", &[], &[&index]),
            &[("indrelid", json!(relation(table)))],
        );
        index
    }

    /// What the engine writes for a copy of `of` on the partition `on`.
    fn copy(&mut self, made: &ObjectIdentity, of: &ObjectIdentity, on: &str) {
        self.add(depend("P", made, of), &[]);
        self.add(depend("S", made, &relation(on)), &[]);
    }
}

/// `app.ev`, partitioned, with an index, a check and a foreign key; `app.late`
/// its partition; `app.child`, whose own foreign key references `app.ev`.
fn declared() -> (Schema, IdsFile) {
    let columns = || Table {
        columns: [("m".into(), Column::new("integer".parse().unwrap()))].into(),
        ..Default::default()
    };
    let key = |to: &str| ForeignKey {
        columns: vec!["m".into()],
        references_table: TableName::new("app", to),
        references_columns: vec!["m".into()],
        on_delete: Default::default(),
        on_update: Default::default(),
    };
    let mut parent = columns();
    parent.partition_by = Some(PartitionBy {
        external: false,
        columns: vec!["m".into()],
        strategy: Default::default(),
    });
    parent.indexes.insert(
        "ev_m".into(),
        Index {
            columns: vec![IndexColumn::column("m")],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
            filegroup: None,
        },
    );
    parent.checks.insert(
        "ev_ck".into(),
        CheckConstraint {
            expression: "m > 0".into(),
        },
    );
    parent.foreign_keys.insert("ev_fk".into(), key("ref"));
    let partition = Table {
        partition_of: Some(PartitionOf {
            parent: TableName::new("app", "ev"),
            bound: PartitionBound::Default,
            columns: Default::default(),
        }),
        ..Default::default()
    };
    let mut child = columns();
    child.foreign_keys.insert("child_fk".into(), key("ev"));
    let mut schema = Schema::default();
    let mut ids = IdsFile::default();
    for (at, (name, table)) in [("ev", parent), ("late", partition), ("child", child)]
        .into_iter()
        .enumerate()
    {
        let name = TableName::new("app", name);
        ids.tables
            .insert(format!("t_00000{at}").parse().unwrap(), name.clone());
        if !table.columns.is_empty() {
            ids.columns
                .insert(format!("c_00000{at}").parse().unwrap(), name.column("m"));
        }
        schema.tables.insert(name, table);
    }
    (schema, ids)
}

fn classified(
    read: Read,
    major: u32,
) -> Result<BTreeMap<ObjectIdentity, ObjectOwnership>, super::super::Uncovered> {
    let (schema, ids) = declared();
    let deployer = identity("pg_authid", &["deployer"], &[]);
    let numbers = read
        .0
        .keys()
        .filter(|id| id.class == "column")
        .map(|id| (id.clone(), 1))
        .collect();
    let captured = CapturedInputs {
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
        inputs: read.0,
        attribute_numbers: numbers,
        candidates: BTreeMap::new(),
        limitations: BTreeSet::new(),
        dropped: BTreeMap::new(),
    };
    super::super::ownership::classify(
        &captured,
        &super::super::RecordedOwnership {
            schema: &schema,
            ids: &ids,
            routines: &BTreeMap::new(),
            dropped: &BTreeMap::new(),
            namespaces: &BTreeSet::new(),
        },
    )
}

/// The tables, their columns, and the parent's own objects.
fn tree() -> (Read, ObjectIdentity, ObjectIdentity) {
    let mut read = Read::default();
    for (table, kind) in [("ev", "p"), ("late", "r"), ("child", "r")] {
        let table = read.add(relation(table), &[("relkind", json!(kind))]);
        read.add(
            identity("column", &["m"], &[&table]),
            &[
                ("attrelid", json!(table)),
                ("attname", json!("m")),
                ("attisdropped", json!(false)),
            ],
        );
    }
    let index = read.index("ev_m", "ev");
    let key = read.add(constraint("ev_fk", "ev"), &[("contype", json!("f"))]);
    (read, index, key)
}

fn on(table: &str) -> ObjectOwnership {
    ObjectOwnership::Surface(Surface::Table(TableName::new("app", table)))
}

#[test]
fn a_partitions_copies_of_declared_objects_belong_to_the_partition() {
    for major in [16, 18] {
        let (mut read, parent_index, parent_key) = tree();
        // An index the table had, taken under its own name, and the rows
        // that hang off it.
        let index = read.index("late_z", "late");
        read.copy(&index, &parent_index, "late");
        let metadata = identity("pg_index", &[], &[&index]);
        let inherits = read.add(identity("pg_inherits", &[], &[&index, &parent_index]), &[]);
        let attribute = read.add(
            identity("column", &["m"], &[&index]),
            &[
                ("attrelid", json!(index)),
                ("attname", json!("m")),
                ("attisdropped", json!(false)),
            ],
        );
        // A foreign key the engine made, and its internal trigger.
        let key = read.add(constraint("ev_fk", "late"), &[("contype", json!("f"))]);
        read.copy(&key, &parent_key, "late");
        let trigger = read.add(identity("pg_trigger", &[], &[&relation("late"), &key]), &[]);
        read.add(depend("i", &trigger, &key), &[]);
        // The check the parent declares, inherited.
        let check = read.add(
            constraint("ev_ck", "late"),
            &[("contype", json!("c")), ("coninhcount", json!(1))],
        );
        // The referencing table's copy of its key, for this partition.
        let declared_key = read.add(constraint("child_fk", "child"), &[("contype", json!("f"))]);
        let referenced = read.add(
            constraint("child_fk_1", "child"),
            &[
                ("contype", json!("f")),
                ("confrelid", json!(relation("late"))),
            ],
        );
        read.add(depend("i", &referenced, &declared_key), &[]);
        let owned = classified(read, major).unwrap();
        for object in [
            &index,
            &metadata,
            &inherits,
            &attribute,
            &key,
            &trigger,
            &check,
            &referenced,
            &depend("P", &index, &parent_index),
            &depend("S", &index, &relation("late")),
        ] {
            assert_eq!(owned[object], on("late"), "PG{major} {object:?}");
        }
        assert_eq!(owned[&declared_key], on("child"), "PG{major}");
    }
}

#[test]
fn only_the_engines_tie_to_a_declared_object_and_a_declared_partition_makes_a_copy() {
    for major in [16, 18] {
        let (mut read, parent_index, _) = tree();
        // An index someone made on the partition, copying nothing.
        let own = read.index("late_other", "late");
        // A copy of an index the parent does not declare.
        let undeclared = read.index("ev_undeclared", "ev");
        let of_undeclared = read.index("late_copy", "late");
        read.copy(&of_undeclared, &undeclared, "late");
        // Tied to a declared index, but on a table that is no partition.
        let elsewhere = read.index("child_copy", "child");
        read.copy(&elsewhere, &parent_index, "child");
        // Tied to what it copies and to no table.
        let loose = read.index("late_loose", "late");
        read.add(depend("P", &loose, &parent_index), &[]);
        // A check of the parent's name that is the table's own, and an
        // inherited one the parent does not declare.
        let local = read.add(
            constraint("ev_ck", "late"),
            &[("contype", json!("c")), ("coninhcount", json!(0))],
        );
        let stranger = read.add(
            constraint("other_ck", "late"),
            &[("contype", json!("c")), ("coninhcount", json!(1))],
        );
        // A copy standing for a table that is no partition.
        let declared_key = read.add(constraint("child_fk", "child"), &[("contype", json!("f"))]);
        let referenced = read.add(
            constraint("child_fk_1", "child"),
            &[
                ("contype", json!("f")),
                ("confrelid", json!(relation("ev"))),
            ],
        );
        read.add(depend("i", &referenced, &declared_key), &[]);
        let owned = classified(read, major).unwrap();
        for object in [
            &own,
            &undeclared,
            &of_undeclared,
            &elsewhere,
            &loose,
            &local,
            &stranger,
            &referenced,
        ] {
            assert_eq!(
                owned[object],
                ObjectOwnership::Unqualified,
                "PG{major} {object:?}"
            );
        }
    }
}

/// A check whose count of parents cannot be read is not taken either way.
#[test]
fn an_inherited_check_with_an_unreadable_count_refuses() {
    let (mut read, _, _) = tree();
    read.add(constraint("ev_ck", "late"), &[("contype", json!("c"))]);
    assert!(classified(read, 18).is_err());
}

/// A copy's record binds nothing of its own, whatever name it has; the
/// original's, and an index that copies nothing, keep what they bind.
#[test]
fn only_a_partition_copy_loses_its_bindings() {
    let (mut read, parent_index, _) = tree();
    let copy = read.index("late_copy", "late");
    read.copy(&copy, &parent_index, "late");
    let trigger = read.add(
        identity("pg_trigger", &["audit"], &[&relation("late")]),
        &[],
    );
    read.copy(&trigger, &parent_index, "late");
    let own = read.index("late_own", "late");
    let metadata = |index: &ObjectIdentity| identity("pg_index", &[], &[index]);
    let binding = Binding {
        node: "OPEXPR".into(),
        path: Vec::new(),
        target: relation("late"),
    };
    let bound = [
        metadata(&parent_index),
        metadata(&copy),
        trigger.clone(),
        metadata(&own),
        relation("late"),
    ];
    for object in &bound {
        let record = read.0.get_mut(object).expect("a record of the read");
        record.bindings.push(binding.clone());
    }
    unbind_partition_copies(&mut read.0);
    let binds = |object: &ObjectIdentity| !read.0[object].bindings.is_empty();
    assert!(!binds(&metadata(&copy)));
    assert!(!binds(&trigger));
    for kept in [metadata(&parent_index), metadata(&own), relation("late")] {
        assert!(binds(&kept), "{kept:?}");
    }
}
