//! Target fixtures for the independent #1274 evidence producer tests.
//!
//! The target is created by hand. In particular, `unmanaged_ix` exists in the
//! catalog but is absent from the declaration inventory: its automatic
//! dependency on `t.c` must never grant pbps ownership. The desired overload
//! arrives only in the declared schema, so the engine decides which existing
//! expressions would bind differently after a fresh creation.

use pbps_model::{
    CheckConstraint, Column, Index, IndexColumn, IndexKey, Module, ModuleKind, Schema, Table,
};

pub(super) const SCHEMA: &str = "pbps_evidence1274";

pub(super) const RESET: &str = "DROP SCHEMA IF EXISTS pbps_evidence1274 CASCADE";
pub(super) const EXTRA_RESET: &str = "DROP SCHEMA IF EXISTS pbps_evidence1274_extra CASCADE";

/// A same-named relation of the wrong PostgreSQL kind cannot inherit a
/// declared table UID. The producer must inspect `relkind` before hashing
/// private properties or granting ownership to the target record.
pub(super) const WRONG_KIND_TARGET_SETUP: &[&str] = &[
    "CREATE SCHEMA pbps_evidence1274",
    "CREATE FUNCTION pbps_evidence1274.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    "CREATE VIEW pbps_evidence1274.t AS SELECT 1::numeric AS c",
];

pub(super) const TARGET_SETUP: &[&str] = &[
    "CREATE SCHEMA pbps_evidence1274",
    "CREATE FUNCTION pbps_evidence1274.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    "CREATE TABLE pbps_evidence1274.t (c numeric DEFAULT pbps_evidence1274.f(1), CONSTRAINT ck CHECK (c > pbps_evidence1274.f(1)))",
    "CREATE INDEX ix ON pbps_evidence1274.t (c) WHERE c > pbps_evidence1274.f(1)",
    "CREATE INDEX unmanaged_ix ON pbps_evidence1274.t (c)",
    "CREATE VIEW pbps_evidence1274.v AS SELECT pbps_evidence1274.f(1) AS x",
    "CREATE VIEW pbps_evidence1274.control AS SELECT pbps_evidence1274.f(1::numeric) AS x",
];

/// The managed view reads an undeclared table, but neither that reference
/// nor shared column spelling gives that table or either undeclared index
/// authority. PostgreSQL supplies the system attributes on each relation.
pub(super) const COLUMN_OWNERSHIP_TARGET_SETUP: &[&str] = &[
    "CREATE SCHEMA pbps_evidence1274",
    "CREATE TABLE pbps_evidence1274.t (c integer)",
    "CREATE INDEX ix ON pbps_evidence1274.t (c)",
    "CREATE INDEX unmanaged_ix ON pbps_evidence1274.t (c)",
    "CREATE TABLE pbps_evidence1274.foreign_t (c integer)",
    "CREATE INDEX foreign_ix ON pbps_evidence1274.foreign_t (c)",
    "CREATE VIEW pbps_evidence1274.ref AS SELECT c FROM pbps_evidence1274.foreign_t",
];

pub(super) fn column_ownership_schema() -> Schema {
    let mut schema = Schema::default();
    let mut table = Table::default();
    table
        .columns
        .insert("c".into(), Column::new("integer".parse().unwrap()));
    table.indexes.insert(
        "ix".into(),
        Index {
            columns: vec![IndexColumn {
                key: IndexKey::Column("c".into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
        },
    );
    schema
        .tables
        .insert("pbps_evidence1274.t".parse().unwrap(), table);
    module(
        &mut schema,
        "pbps_evidence1274.ref",
        ModuleKind::View,
        "SELECT c FROM pbps_evidence1274.foreign_t",
    );
    schema
}

/// PostgreSQL index names are schema-unique, so a same-name `ix` on `other`
/// replaces the declared `t.ix` in this target. The captured pg_index parent,
/// not its spelling or relkind, must decide whether it belongs to `t`.
pub(super) const WRONG_TABLE_INDEX_TARGET_SETUP: &[&str] = &[
    "CREATE SCHEMA pbps_evidence1274",
    "CREATE FUNCTION pbps_evidence1274.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    "CREATE TABLE pbps_evidence1274.t (c numeric DEFAULT pbps_evidence1274.f(1), CONSTRAINT ck CHECK (c > pbps_evidence1274.f(1)))",
    "CREATE TABLE pbps_evidence1274.other (c numeric)",
    "CREATE INDEX ix ON pbps_evidence1274.other (c)",
    "CREATE VIEW pbps_evidence1274.v AS SELECT pbps_evidence1274.f(1) AS x",
    "CREATE VIEW pbps_evidence1274.control AS SELECT pbps_evidence1274.f(1::numeric) AS x",
];

/// The old view has both relation and column grants, but the product's
/// typed AlterModule expands to DROP+CREATE. The replacement must receive
/// creation defaults rather than inherit these old grants. The grantee is
/// built in, so no shared role is created outside the disposable schema.
pub(super) const REBUILT_VIEW_TARGET_SETUP: &[&str] = &[
    "CREATE SCHEMA pbps_evidence1274",
    "CREATE VIEW pbps_evidence1274.v AS SELECT 1::integer AS x",
    "GRANT SELECT ON pbps_evidence1274.v TO pg_monitor WITH GRANT OPTION",
    "GRANT SELECT(x) ON pbps_evidence1274.v TO pg_monitor WITH GRANT OPTION",
];

pub(super) fn rebuilt_view_pair() -> (Schema, Schema) {
    let mut base = Schema::default();
    module(
        &mut base,
        "pbps_evidence1274.v",
        ModuleKind::View,
        "SELECT 1::integer AS x",
    );
    let mut desired = base.clone();
    desired
        .modules
        .get_mut(&"pbps_evidence1274.v".parse().unwrap())
        .unwrap()
        .definition = "SELECT 2::integer AS x".into();
    (base, desired)
}

/// The first #614 phase begins with an empty managed target. The table's
/// default, CHECK and predicate call f(), while f() itself reads the table.
/// Ordinary bootstrap must be reconstructed as bare table, routines, then
/// creation-time expressions by ScratchRun rather than by this fixture.
pub(super) fn empty_cross_kind_pair() -> (Schema, Schema) {
    let base = Schema::default();
    let mut desired = Schema::default();
    let mut table = Table::default();
    let mut column = Column::new("integer".parse().unwrap()).not_null();
    column.default = Some("pbps_evidence1274.f()".into());
    table.columns.insert("id".into(), column);
    table.checks.insert(
        "positive".into(),
        CheckConstraint {
            expression: "id >= pbps_evidence1274.f()".into(),
        },
    );
    table.indexes.insert(
        "ix".into(),
        Index {
            columns: vec![IndexColumn {
                key: IndexKey::Column("id".into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: Some("id >= pbps_evidence1274.f()".into()),
            method: Default::default(),
        },
    );
    desired
        .tables
        .insert("pbps_evidence1274.t".parse().unwrap(), table);
    module(
        &mut desired,
        "pbps_evidence1274.f()",
        ModuleKind::Function,
        "() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN (SELECT count(*)::integer FROM pbps_evidence1274.t)",
    );
    module(
        &mut desired,
        "pbps_evidence1274.a()",
        ModuleKind::Function,
        "() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN pbps_evidence1274.f()",
    );
    (base, desired)
}

fn module(schema: &mut Schema, id: &str, kind: ModuleKind, definition: &str) {
    schema.modules.insert(
        id.parse().unwrap(),
        Module {
            kind,
            description: None,
            definition: definition.into(),
        },
    );
}

/// The model owns exactly the named default, check and predicate. The
/// unmanaged index is a read prerequisite, not part of this declaration.
pub(super) fn pair_with_cross_kind_surfaces() -> (Schema, Schema) {
    let mut base = Schema::default();
    module(
        &mut base,
        "pbps_evidence1274.f(numeric)",
        ModuleKind::Function,
        "(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    );
    module(
        &mut base,
        "pbps_evidence1274.v",
        ModuleKind::View,
        "SELECT f(1) AS x",
    );
    module(
        &mut base,
        "pbps_evidence1274.control",
        ModuleKind::View,
        "SELECT f(1::numeric) AS x",
    );
    let mut table = Table::default();
    let mut column = Column::new("numeric".parse().unwrap());
    column.default = Some("f(1)".into());
    table.columns.insert("c".into(), column);
    table.checks.insert(
        "ck".into(),
        CheckConstraint {
            expression: "c > f(1)".into(),
        },
    );
    table.indexes.insert(
        "ix".into(),
        Index {
            columns: vec![IndexColumn {
                key: IndexKey::Column("c".into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: Some("c > f(1)".into()),
            method: Default::default(),
        },
    );
    base.tables
        .insert("pbps_evidence1274.t".parse().unwrap(), table);
    let mut desired = base.clone();
    module(
        &mut desired,
        "pbps_evidence1274.f(integer)",
        ModuleKind::Function,
        "(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    );
    (base, desired)
}

pub(super) const REPLACEMENT_TARGET_SETUP: &[&str] = &[
    "CREATE SCHEMA pbps_evidence1274",
    "CREATE TABLE pbps_evidence1274.t (id integer NOT NULL)",
    "CREATE FUNCTION pbps_evidence1274.f() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN (SELECT count(*)::integer FROM pbps_evidence1274.t)",
    "CREATE FUNCTION pbps_evidence1274.a() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN pbps_evidence1274.f()",
    "ALTER TABLE pbps_evidence1274.t ALTER COLUMN id SET DEFAULT pbps_evidence1274.f()",
    "ALTER TABLE pbps_evidence1274.t ADD CONSTRAINT positive CHECK (id >= pbps_evidence1274.f())",
    "CREATE INDEX ix ON pbps_evidence1274.t (id) WHERE id >= pbps_evidence1274.f()",
];

pub(super) const RENAME_TARGET_SETUP: &[&str] = &[
    "CREATE SCHEMA pbps_evidence1274",
    "CREATE TABLE pbps_evidence1274.t (id integer NOT NULL)",
    "CREATE FUNCTION pbps_evidence1274.f() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 8",
    "CREATE FUNCTION pbps_evidence1274.a() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN pbps_evidence1274.f()",
    "ALTER TABLE pbps_evidence1274.t ALTER COLUMN id SET DEFAULT pbps_evidence1274.f()",
    "ALTER TABLE pbps_evidence1274.t ADD CONSTRAINT positive CHECK (id >= pbps_evidence1274.f())",
    "CREATE INDEX ix ON pbps_evidence1274.t (id) WHERE id >= pbps_evidence1274.f()",
    "GRANT SELECT ON TABLE pbps_evidence1274.t TO pg_monitor WITH GRANT OPTION",
    "GRANT SELECT(id) ON TABLE pbps_evidence1274.t TO pg_monitor WITH GRANT OPTION",
];

/// #614 replaces a same-named function while dependent routine, default,
/// CHECK and predicate still bind it.
pub(super) fn replacement_pair() -> (Schema, Schema) {
    let (_, base) = empty_cross_kind_pair();
    let mut desired = base.clone();
    desired
        .modules
        .get_mut(&"pbps_evidence1274.f()".parse().unwrap())
        .unwrap()
        .definition = "() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 8".into();
    (base, desired)
}

/// The identity mapping records table and column renames. No name heuristic
/// may invent this intent from the target catalog.
pub(super) fn rename_pair() -> (Schema, Schema, pbps_model::IdsFile, pbps_model::IdsFile) {
    let (_, base) = replacement_pair();
    let base_ids = pbps_diff::resolve(
        &base,
        &pbps_model::IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "1274-test".into(),
            today: "2026-09-29".into(),
        },
    )
    .unwrap()
    .ids;
    let old: pbps_model::TableName = "pbps_evidence1274.t".parse().unwrap();
    let new: pbps_model::TableName = "pbps_evidence1274.u".parse().unwrap();
    let mut desired = base.clone();
    let mut table = desired.tables.remove(&old).unwrap();
    let column = table.columns.shift_remove("id").unwrap();
    table.columns.insert("n".into(), column);
    table.checks.get_mut("positive").unwrap().expression = "n >= pbps_evidence1274.f()".into();
    let index = table.indexes.get_mut("ix").unwrap();
    index.columns[0].key = IndexKey::Column("n".into());
    index.filter = Some("n >= pbps_evidence1274.f()".into());
    desired.tables.insert(new.clone(), table);
    desired
        .modules
        .get_mut(&"pbps_evidence1274.f()".parse().unwrap())
        .unwrap()
        .definition = "() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 9".into();
    let mut desired_ids = base_ids.clone();
    desired_ids.rename_table(&old, &new);
    let uid = base_ids.column_uid(&old.column("id")).unwrap();
    desired_ids.columns.get_mut(uid).unwrap().name = "n".into();
    (base, desired, base_ids, desired_ids)
}

/// An established write-path lookup: app's bare call initially chooses the
/// exact integer overload in the later extra schema. Adding the earlier
/// overload changes that binding; an explicitly qualified call is the control.
pub(super) const EXTRA_LOOKUP_TARGET_SETUP: &[&str] = &[
    "CREATE SCHEMA pbps_evidence1274",
    "CREATE SCHEMA pbps_evidence1274_extra",
    "CREATE FUNCTION pbps_evidence1274.f(numeric) RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 20",
    "CREATE FUNCTION pbps_evidence1274_extra.f(integer) RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 10",
    "SET search_path = pbps_evidence1274, pbps_evidence1274_extra",
    "CREATE VIEW pbps_evidence1274.v AS SELECT f(1) AS x",
    "CREATE VIEW pbps_evidence1274.control AS SELECT pbps_evidence1274_extra.f(1) AS x",
];

pub(super) fn extra_lookup_pair() -> (Schema, Schema) {
    let mut base = Schema::default();
    module(
        &mut base,
        "pbps_evidence1274.f(numeric)",
        ModuleKind::Function,
        "(numeric) RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 20",
    );
    module(
        &mut base,
        "pbps_evidence1274_extra.f(integer)",
        ModuleKind::Function,
        "(integer) RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 10",
    );
    module(
        &mut base,
        "pbps_evidence1274.v",
        ModuleKind::View,
        "SELECT f(1) AS x",
    );
    module(
        &mut base,
        "pbps_evidence1274.control",
        ModuleKind::View,
        "SELECT pbps_evidence1274_extra.f(1) AS x",
    );
    let mut desired = base.clone();
    module(
        &mut desired,
        "pbps_evidence1274.f(integer)",
        ModuleKind::Function,
        "(integer) RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 30",
    );
    (base, desired)
}

/// The recorded old table/column identities move to u.n, while the desired
/// declaration reuses both old spellings for two freshly allocated objects.
pub(super) fn rename_and_reuse_pair() -> (Schema, Schema, pbps_model::IdsFile, pbps_model::IdsFile)
{
    let (base, mut desired, base_ids, renamed_ids) = rename_pair();
    let old: pbps_model::TableName = "pbps_evidence1274.t".parse().unwrap();
    let new: pbps_model::TableName = "pbps_evidence1274.u".parse().unwrap();
    desired
        .tables
        .get_mut(&new)
        .unwrap()
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    let mut replacement = Table::default();
    replacement
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    desired.tables.insert(old, replacement);
    let desired_ids = pbps_diff::resolve(
        &desired,
        &renamed_ids,
        &[],
        &pbps_diff::Context {
            operator: "1274-test".into(),
            today: "2026-09-29".into(),
        },
    )
    .unwrap()
    .ids;
    (base, desired, base_ids, desired_ids)
}

/// The three catalog outcomes measured on both pinned engines: a type rewrite
/// retains the old NOT NULL name on PG18, DROP removes it, and SET creates one
/// at the final spelling. All three tables and columns have recorded UIDs.
pub(super) const TYPED_RENAME_TARGET_SETUP: &[&str] = &[
    "CREATE SCHEMA pbps_evidence1274",
    "CREATE TABLE pbps_evidence1274.type_case (id integer NOT NULL)",
    "CREATE TABLE pbps_evidence1274.drop_case (id integer NOT NULL)",
    "CREATE TABLE pbps_evidence1274.add_case (id integer)",
];

pub(super) fn typed_rename_pair() -> (Schema, Schema, pbps_model::IdsFile, pbps_model::IdsFile) {
    let mut base = Schema::default();
    for (stem, required) in [("type", true), ("drop", true), ("add", false)] {
        let mut table = Table::default();
        let mut column = Column::new("integer".parse().unwrap());
        if required {
            column = column.not_null();
        }
        table.columns.insert("id".into(), column);
        base.tables.insert(
            format!("pbps_evidence1274.{stem}_case").parse().unwrap(),
            table,
        );
    }
    let base_ids = pbps_diff::resolve(
        &base,
        &pbps_model::IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "1274-test".into(),
            today: "2026-09-29".into(),
        },
    )
    .unwrap()
    .ids;
    let mut desired = base.clone();
    let mut desired_ids = base_ids.clone();
    for stem in ["type", "drop", "add"] {
        let old: pbps_model::TableName = format!("pbps_evidence1274.{stem}_case").parse().unwrap();
        let new: pbps_model::TableName = format!("pbps_evidence1274.{stem}_final").parse().unwrap();
        let mut table = desired.tables.remove(&old).unwrap();
        let mut column = table.columns.shift_remove("id").unwrap();
        match stem {
            "type" => column.ty = "bigint".parse().unwrap(),
            "drop" => column.nullable = true,
            "add" => column.nullable = false,
            _ => unreachable!(),
        }
        table.columns.insert("n".into(), column);
        desired.tables.insert(new.clone(), table);
        desired_ids.rename_table(&old, &new);
        let uid = base_ids.column_uid(&old.column("id")).unwrap();
        desired_ids.columns.get_mut(uid).unwrap().name = "n".into();
    }
    (base, desired, base_ids, desired_ids)
}
