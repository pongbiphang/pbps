use std::collections::BTreeMap;

use pbps_db::sqlite::{Connection, OpenMode};
use pbps_dialect::slots::{Namespace, Same, SlotKey, TableSpace, same, slots};
use pbps_model::operation::{
    Address, ModuleEdits, NameKind, Operations, PlanOwners, Snapshot, module_key,
};
use pbps_model::plan_names::PlanNames;
use pbps_model::{
    Change, ChangeSet, Column, ColumnChange, IdsFile, Index, IndexChange, IndexColumn, Module,
    ModuleChange, ModuleId, ModuleKind, PlannedChange, PrimaryKey, Schema, Table, TableChange,
    TableName, Uid, UidKind, UniqueConstraint,
};

use super::*;
use crate::dialect::Sqlite;
use crate::tests::Fresh;

fn uid(kind: UidKind, label: &str) -> Uid {
    Uid::derived(kind, label, 0)
}

fn int_col() -> Column {
    Column::new("integer".parse().expect("type"))
}

fn plain_table(cols: &[&str]) -> Table {
    let mut t = Table::default();
    for &c in cols {
        t.columns.insert(c.into(), int_col());
    }
    t
}

/// Real-SQLite measurements on temporary files for every namespace, constraint,
/// generated-column and intermediate-name rule this module relies on.
#[test]
fn real_sqlite_file_measurements_confirm_namespace_key_constraint_column_and_intermediate_rules() {
    // 1. Tables, views and indexes share one ASCII case-insensitive namespace;
    // triggers hold a separate namespace; non-ASCII case is distinct.
    {
        let mut db = Fresh::file();
        let c = db.connection();
        c.execute_batch("CREATE TABLE items (id INTEGER)")
            .expect("table");
        assert!(
            c.execute_batch("CREATE TABLE ITEMS (id INTEGER)").is_err(),
            "table vs table ASCII fold"
        );
        assert!(
            c.execute_batch("CREATE VIEW Items AS SELECT 1").is_err(),
            "table vs view ASCII fold"
        );
        assert!(
            c.execute_batch("CREATE INDEX ITEMS ON items(id)").is_err(),
            "table vs index ASCII fold"
        );
        // Trigger shares no namespace with tables/views/indexes.
        c.execute_batch("CREATE TRIGGER ITEMS AFTER INSERT ON items BEGIN SELECT 1; END")
            .expect("trigger shares name with table");
        assert!(
            c.execute_batch("CREATE TRIGGER items AFTER DELETE ON items BEGIN SELECT 1; END")
                .is_err(),
            "trigger vs trigger ASCII fold"
        );
        // Non-ASCII case is distinct, while the ASCII prefix still folds.
        c.execute_batch(
            "CREATE TABLE \"café\" (id INTEGER); \
             CREATE VIEW \"cafÉ\" AS SELECT 1; \
             CREATE INDEX \"café_ix\" ON \"café\"(id)",
        )
        .expect("non-ASCII case distinct across table and view");
        assert!(
            c.execute_batch("CREATE INDEX \"CAFÉ\" ON \"café\"(id)")
                .is_err(),
            "ASCII prefix folds (`CAFÉ` collides with `cafÉ`)"
        );
        c.execute_batch("CREATE INDEX \"CAFé\" ON \"café\"(id)")
            .expect_err("ASCII prefix folds (`CAFé` collides with `café`)");
    }

    // 2. CONSTRAINT k PRIMARY KEY / UNIQUE creates no sqlite_schema row named k,
    // so a table, view or index named k coexists.
    {
        let mut db = Fresh::file();
        let c = db.connection();
        c.execute_batch(
            "CREATE TABLE holder (\
                 id INTEGER, \
                 code TEXT, \
                 CONSTRAINT k_pk PRIMARY KEY (id), \
                 CONSTRAINT k_uq UNIQUE (code)\
             )",
        )
        .expect("holder");
        let rows = c
            .query(
                "SELECT type, name FROM main.sqlite_schema \
                 WHERE name COLLATE NOCASE IN ('k_pk', 'k_uq')",
                &[],
            )
            .expect("query");
        assert!(
            rows.is_empty(),
            "neither constraint name appears in sqlite_schema"
        );
        let auto = c
            .query(
                "SELECT name FROM main.sqlite_schema WHERE type = 'index' AND tbl_name = 'holder' ORDER BY name",
                &[],
            )
            .expect("autoindex");
        assert!(
            auto.iter().all(|r| r
                .text(0)
                .expect("text")
                .starts_with("sqlite_autoindex_holder_")),
            "backing indexes are sqlite_autoindex_holder_*"
        );
        c.execute_batch(
            "CREATE TABLE k_pk (x INTEGER); \
             CREATE INDEX k_uq ON k_pk(x)",
        )
        .expect("table and index named after PK/UNIQUE coexist");
    }

    // 3. Two constraints on one table may share a name in SQLite (R7).
    {
        let mut db = Fresh::file();
        let c = db.connection();
        c.execute_batch(
            "CREATE TABLE dup_check (\
                 a INTEGER, \
                 b INTEGER, \
                 CONSTRAINT c CHECK (a > 0), \
                 CONSTRAINT c CHECK (b > 0)\
             )",
        )
        .expect("SQLite accepts duplicate CHECK constraint names on one table");
        c.execute_batch(
            "CREATE TABLE dup_key (\
                 a INTEGER, \
                 b INTEGER, \
                 CONSTRAINT k PRIMARY KEY (a), \
                 CONSTRAINT k UNIQUE (b)\
             )",
        )
        .expect("SQLite accepts duplicate PK and UNIQUE constraint names on one table");
    }

    // 4. Adding or renaming a column to match a hidden or generated column
    // fails, and PRAGMA main.table_xinfo reports hidden = 2 (virtual) / 3 (stored).
    {
        let mut db = Fresh::file();
        let c = db.connection();
        c.execute_batch(
            "CREATE TABLE gen (\
                 a INTEGER, \
                 b INTEGER, \
                 gv INTEGER GENERATED ALWAYS AS (a + 1) VIRTUAL, \
                 gs INTEGER GENERATED ALWAYS AS (a + 2) STORED\
             )",
        )
        .expect("generated table");
        let inv = read(c, &["gen"]).expect("read");
        let TableColumns::Columns(cols) = &inv.columns["gen"] else {
            panic!("expected Columns, got {:?}", inv.columns["gen"]);
        };
        assert_eq!(
            cols,
            &vec![
                ColumnEntry {
                    name: "a".into(),
                    pk: 0,
                    hidden: 0
                },
                ColumnEntry {
                    name: "b".into(),
                    pk: 0,
                    hidden: 0
                },
                ColumnEntry {
                    name: "gv".into(),
                    pk: 0,
                    hidden: 2
                },
                ColumnEntry {
                    name: "gs".into(),
                    pk: 0,
                    hidden: 3
                },
            ]
        );
        for stmt in [
            "ALTER TABLE main.gen ADD COLUMN gv INTEGER",
            "ALTER TABLE main.gen ADD COLUMN GV INTEGER",
            "ALTER TABLE main.gen ADD COLUMN gs INTEGER",
            "ALTER TABLE main.gen ADD COLUMN GS INTEGER",
            "ALTER TABLE main.gen RENAME COLUMN b TO GV",
            "ALTER TABLE main.gen RENAME COLUMN b TO GS",
        ] {
            let err = c.execute_batch(stmt).expect_err(stmt).to_string();
            assert!(
                err.contains("duplicate column name"),
                "{stmt}: unexpected error {err}"
            );
        }
        // Case-only rename of one column succeeds in place on SQLite.
        c.execute_batch("ALTER TABLE main.gen RENAME COLUMN b TO B")
            .expect("case-only column rename succeeds in place");
    }

    // 5. A stray table, view or index (exact or case-folded) at the case-only
    // rename's intermediate name refuses the first RENAME TO; a stray trigger
    // does not.
    {
        let table_uid = uid(UidKind::Table, "main.t");
        let inter = format!("{}{}", crate::names::RENAME_PREFIX, table_uid.as_str());
        for (what, ddl) in [
            ("table", format!("CREATE TABLE \"{inter}\"(x INTEGER)")),
            ("view", format!("CREATE VIEW \"{inter}\" AS SELECT 1")),
            ("index", format!("CREATE INDEX \"{inter}\" ON t(a)")),
            (
                "case-folded table",
                format!("CREATE TABLE \"{}\"(x INTEGER)", inter.to_ascii_uppercase()),
            ),
        ] {
            let mut db = Fresh::file();
            let c = db.connection();
            c.execute_batch(&format!("CREATE TABLE t(a INTEGER); {ddl}"))
                .expect(what);
            let err = c
                .execute_batch(&format!("ALTER TABLE main.t RENAME TO \"{inter}\""))
                .expect_err(what)
                .to_string();
            assert!(
                err.contains("already another table or index"),
                "{what}: {err}"
            );
        }
        // Negative: a stray trigger at the intermediate name does NOT block RENAME TO.
        let mut db = Fresh::file();
        let c = db.connection();
        c.execute_batch(&format!(
            "CREATE TABLE t(a INTEGER); \
             CREATE TRIGGER \"{inter}\" AFTER INSERT ON t BEGIN SELECT 1; END"
        ))
        .expect("trigger setup");
        c.execute_batch(&format!(
            "ALTER TABLE main.t RENAME TO \"{inter}\"; \
             ALTER TABLE main.\"{inter}\" RENAME TO \"T\""
        ))
        .expect("stray trigger does not block case-only rename");
    }
}

#[test]
fn read_captures_whole_file_inventory_without_loading_missing_virtual_module_and_fails_on_locked_file()
 {
    let mut db = Fresh::file();
    {
        let c = db.connection();
        c.execute_batch(
            "CREATE TABLE plain (id INTEGER PRIMARY KEY AUTOINCREMENT, a INTEGER, g INTEGER GENERATED ALWAYS AS (a * 2) VIRTUAL); \
             CREATE VIEW v_plain AS SELECT id FROM plain; \
             CREATE INDEX ix_expr ON plain ((a + 1)); \
             CREATE VIRTUAL TABLE docs USING fts5(body); \
             CREATE TRIGGER tr_plain AFTER INSERT ON plain BEGIN SELECT 1; END; \
             CREATE TABLE __pbps_state (id INTEGER PRIMARY KEY); \
             CREATE TABLE __pbps_rename_stray (x INTEGER)",
        )
        .expect("setup");
        // Insert a virtual table whose module does not exist in this library.
        c.execute_batch(
            "PRAGMA writable_schema = ON; \
             INSERT INTO sqlite_schema (type, name, tbl_name, rootpage, sql) \
             VALUES ('table', 'ghost_vtab', 'ghost_vtab', 0, 'CREATE VIRTUAL TABLE ghost_vtab USING no_such_module(a)'); \
             PRAGMA writable_schema = OFF",
        )
        .expect("inject missing-module virtual table");
    }
    db.reopen();

    // Read inside the caller's transaction; verify missing-module virtual table
    // is seen in `entries` and reported `TableColumns::Virtual` without error.
    let inv = db
        .connection()
        .transaction(&crate::READ, |tx| {
            read(tx, &["plain", "ghost_vtab", "v_plain", "missing_tbl"])
        })
        .expect("inventory reads inside caller transaction");

    let has =
        |kind: EntryKind, name: &str| inv.entries.iter().any(|e| e.kind == kind && e.name == name);
    assert!(has(EntryKind::Table, "plain"));
    assert!(has(EntryKind::Table, "sqlite_sequence"));
    assert!(has(EntryKind::Table, "docs"));
    assert!(has(EntryKind::Table, "docs_content"));
    assert!(has(EntryKind::Table, "ghost_vtab"));
    assert!(has(EntryKind::Table, "__pbps_state"));
    assert!(has(EntryKind::Table, "__pbps_rename_stray"));
    assert!(has(EntryKind::View, "v_plain"));
    assert!(has(EntryKind::Index, "ix_expr"));
    assert!(has(EntryKind::Trigger, "tr_plain"));

    assert_eq!(inv.columns["ghost_vtab"], TableColumns::Virtual);
    assert_eq!(inv.columns["v_plain"], TableColumns::View);
    assert_eq!(inv.columns["missing_tbl"], TableColumns::Absent);
    assert_eq!(
        inv.columns["plain"],
        TableColumns::Columns(vec![
            ColumnEntry {
                name: "id".into(),
                pk: 1,
                hidden: 0
            },
            ColumnEntry {
                name: "a".into(),
                pk: 0,
                hidden: 0
            },
            ColumnEntry {
                name: "g".into(),
                pk: 0,
                hidden: 2
            },
        ])
    );

    // Negative: an unreadable (exclusively locked) catalog returns Err, never
    // an empty Inventory.
    let blocker = Connection::open(db.path(), OpenMode::ExistingOnly).expect("second conn");
    blocker.execute_batch("BEGIN EXCLUSIVE").expect("lock");
    let err = read(db.connection(), &[]).expect_err("locked file must fail");
    assert_eq!(err.server_error_code().as_deref(), Some("5"), "{err}");
    blocker.execute_batch("ROLLBACK").expect("unlock");
}

#[test]
fn sqlite_key_slot_claims_no_relation_namespace_and_check_index_names_allows_it() {
    let dialect = Sqlite::default();
    let table_key = module_key(&"main.orders".parse::<ModuleId>().expect("id"));
    let key_slots = slots(
        &dialect,
        &Address {
            kind: NameKind::Key,
            schema: "main".into(),
            table: Some(table_key.clone()),
            name: "items".into(),
            signature: None,
        },
    );
    assert_eq!(
        key_slots,
        vec![SlotKey {
            namespace: Namespace::Table(table_key, TableSpace::Constraints),
            name: "items".into(),
        }]
    );

    // A table named `items` and another table whose PK/UNIQUE is named `items`
    // must pass `check_index_names` on SQLite (while a real index named `items`
    // is still refused).
    let mut schema = Schema::default();
    schema
        .tables
        .insert(TableName::new("main", "items"), plain_table(&["id"]));
    let mut orders = plain_table(&["id", "code"]);
    orders.primary_key = Some(PrimaryKey {
        name: Some("items".into()),
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    });
    orders.unique.insert(
        "other_tbl".into(),
        UniqueConstraint {
            columns: vec!["code".into()],
            storage_parameters: Default::default(),
            filegroup: None,
        },
    );
    schema
        .tables
        .insert(TableName::new("main", "other_tbl"), plain_table(&["id"]));
    schema
        .tables
        .insert(TableName::new("main", "orders"), orders);
    assert!(
        pbps_dialect::check_index_names(&schema, &dialect).is_empty(),
        "PK/UNIQUE names do not collide with tables on SQLite"
    );

    // Negative: an explicit index named `items` DOES collide with table `items`.
    schema
        .tables
        .get_mut(&TableName::new("main", "orders"))
        .expect("orders")
        .indexes
        .insert(
            "items".into(),
            Index {
                columns: vec![IndexColumn::column("id")],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
                filegroup: None,
            },
        );
    assert!(
        !pbps_dialect::check_index_names(&schema, &dialect).is_empty(),
        "explicit index collides with table on SQLite"
    );
}

#[test]
fn catalog_facts_decides_every_ascii_case_pair_with_zero_skipped_and_keeps_non_ascii_distinct() {
    let dialect = Sqlite::default();
    let old_name = TableName::new("main", "Foo");
    let new_name = TableName::new("main", "foo");
    let accented_a = TableName::new("main", "café");
    let accented_b = TableName::new("main", "cafÉ");

    let old_uid = uid(UidKind::Table, "main.Foo");
    let new_uid = uid(UidKind::Table, "main.foo");
    let acc_a_uid = uid(UidKind::Table, "main.café");
    let acc_b_uid = uid(UidKind::Table, "main.cafÉ");

    let mut base_schema = Schema::default();
    base_schema
        .tables
        .insert(old_name.clone(), plain_table(&["id"]));
    base_schema
        .tables
        .insert(accented_a.clone(), plain_table(&["id"]));
    let mut base_ids = IdsFile::default();
    base_ids.tables.insert(old_uid.clone(), old_name.clone());
    base_ids
        .tables
        .insert(acc_a_uid.clone(), accented_a.clone());

    let mut desired_schema = Schema::default();
    desired_schema
        .tables
        .insert(new_name.clone(), plain_table(&["id"]));
    desired_schema
        .tables
        .insert(accented_a.clone(), plain_table(&["id"]));
    desired_schema
        .tables
        .insert(accented_b.clone(), plain_table(&["id"]));
    let mut desired_ids = IdsFile::default();
    desired_ids.tables.insert(new_uid.clone(), new_name.clone());
    desired_ids.tables.insert(acc_a_uid, accented_a.clone());
    desired_ids
        .tables
        .insert(acc_b_uid.clone(), accented_b.clone());

    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::Table(TableChange::DropTable {
                uid: old_uid,
                name: old_name,
                detach_from: None,
            })),
            PlannedChange::new(Change::Table(TableChange::CreateTable {
                uid: new_uid,
                name: new_name,
                table: Box::new(plain_table(&["id"])),
                beside_default: None,
                moves_at_most: None,
            })),
            PlannedChange::new(Change::Table(TableChange::CreateTable {
                uid: acc_b_uid,
                name: accented_b,
                table: Box::new(plain_table(&["id"])),
                beside_default: None,
                moves_at_most: None,
            })),
        ],
    };

    let inventory = Inventory {
        entries: vec![
            SchemaEntry {
                kind: EntryKind::Table,
                name: "Foo".into(),
                table: "Foo".into(),
                sql: Some("CREATE TABLE Foo(id INTEGER)".into()),
            },
            SchemaEntry {
                kind: EntryKind::Table,
                name: "café".into(),
                table: "café".into(),
                sql: Some("CREATE TABLE \"café\"(id INTEGER)".into()),
            },
            SchemaEntry {
                kind: EntryKind::View,
                name: "unmanaged_v".into(),
                table: "unmanaged_v".into(),
                sql: Some("CREATE VIEW unmanaged_v AS SELECT 1".into()),
            },
        ],
        columns: BTreeMap::new(),
        ..Inventory::default()
    };

    let base = pbps_diff::Side {
        schema: &base_schema,
        ids: &base_ids,
    };
    let desired = pbps_diff::Side {
        schema: &desired_schema,
        ids: &desired_ids,
    };
    let facts = catalog_facts(&inventory, &changes, base, desired, &dialect).expect("facts");

    let slot = |n: &str| SlotKey {
        namespace: Namespace::Relations("main".into()),
        name: n.into(),
    };
    assert_eq!(
        same(&dialect, Some(&facts), &slot("Foo"), &slot("foo")),
        Same::Yes
    );
    assert_eq!(
        same(&dialect, Some(&facts), &slot("café"), &slot("cafÉ")),
        Same::No
    );
    assert!(
        facts
            .occupants
            .iter()
            .any(|o| o.slot == slot("unmanaged_v") && o.kind == "view")
    );

    // Replay through `pbps_diff::order::slots::verify` produces zero violations
    // and zero skipped comparisons.
    let plan_names = PlanNames::of(&changes.changes, &desired_ids).expect("names");
    let owners = PlanOwners::new(plan_names, &base_ids, &desired_ids);
    let ops = Operations::of_plan(&changes.changes, ModuleEdits::InPlace);
    let before = Snapshot {
        schema: &base_schema,
        ids: &base_ids,
    };
    let analysed = pbps_diff::order::slots::analyse(ops.as_slice(), &owners, before, &dialect);
    let (violations, skipped) = pbps_diff::order::slots::verify(
        ops.as_slice(),
        &analysed,
        &dialect,
        Some(&facts),
        &owners,
        before,
    )
    .expect("verify");
    assert!(violations.is_empty(), "{violations:?}");
    assert!(skipped.is_empty(), "{skipped:?}");

    // Negative: reversing DropTable and CreateTable triggers a slot violation.
    let reversed = ChangeSet {
        changes: vec![
            changes.changes[1].clone(),
            changes.changes[0].clone(),
            changes.changes[2].clone(),
        ],
    };
    let rev_names = PlanNames::of(&reversed.changes, &desired_ids).expect("names");
    let rev_owners = PlanOwners::new(rev_names, &base_ids, &desired_ids);
    let rev_ops = Operations::of_plan(&reversed.changes, ModuleEdits::InPlace);
    let rev_analysed =
        pbps_diff::order::slots::analyse(rev_ops.as_slice(), &rev_owners, before, &dialect);
    let (rev_violations, rev_skipped) = pbps_diff::order::slots::verify(
        rev_ops.as_slice(),
        &rev_analysed,
        &dialect,
        Some(&facts),
        &rev_owners,
        before,
    )
    .expect("verify");
    assert!(
        !rev_violations.is_empty(),
        "reversed order must violate slots"
    );
    assert!(rev_skipped.is_empty(), "{rev_skipped:?}");
}

#[test]
fn verify_names_catches_catalog_occupants_and_allows_freed_names_and_key_names() {
    let dialect = Sqlite::default();
    let inventory = Inventory {
        entries: vec![
            SchemaEntry {
                kind: EntryKind::Table,
                name: "existing".into(),
                table: "existing".into(),
                sql: Some("CREATE TABLE existing(id INTEGER)".into()),
            },
            SchemaEntry {
                kind: EntryKind::Index,
                name: "ix_owned".into(),
                table: "existing".into(),
                sql: Some("CREATE INDEX ix_owned ON existing(id)".into()),
            },
            SchemaEntry {
                kind: EntryKind::Table,
                name: "__pbps_state".into(),
                table: "__pbps_state".into(),
                sql: Some("CREATE TABLE __pbps_state(id INTEGER)".into()),
            },
            SchemaEntry {
                kind: EntryKind::Trigger,
                name: "tr_only".into(),
                table: "existing".into(),
                sql: Some(
                    "CREATE TRIGGER tr_only AFTER INSERT ON existing BEGIN SELECT 1; END".into(),
                ),
            },
        ],
        columns: [(
            "existing".to_owned(),
            TableColumns::Columns(vec![
                ColumnEntry {
                    name: "id".into(),
                    pk: 0,
                    hidden: 0,
                },
                ColumnEntry {
                    name: "gen_c".into(),
                    pk: 0,
                    hidden: 2,
                },
            ]),
        )]
        .into(),
        ..Inventory::default()
    };

    let existing_uid = uid(UidKind::Table, "main.existing");
    let existing_id_uid = uid(UidKind::Column, "main.existing.id");
    let mut base = Schema::default();
    base.tables
        .insert(TableName::new("main", "existing"), plain_table(&["id"]));
    let mut base_ids = IdsFile::default();
    base_ids
        .tables
        .insert(existing_uid.clone(), TableName::new("main", "existing"));
    base_ids.columns.insert(
        existing_id_uid,
        TableName::new("main", "existing").column("id"),
    );
    let base_side = pbps_diff::Side {
        schema: &base,
        ids: &base_ids,
    };

    // Creating a table named like a trigger, with a PK named like `existing`,
    // succeeds on SQLite.
    let mut ok_table = plain_table(&["id"]);
    ok_table.primary_key = Some(PrimaryKey {
        name: Some("existing".into()),
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    });
    let tr_uid = uid(UidKind::Table, "main.tr_only");
    let mut ok_desired = base.clone();
    ok_desired
        .tables
        .insert(TableName::new("main", "tr_only"), ok_table.clone());
    let mut ok_ids = base_ids.clone();
    ok_ids
        .tables
        .insert(tr_uid.clone(), TableName::new("main", "tr_only"));
    ok_ids.columns.insert(
        uid(UidKind::Column, "main.tr_only.id"),
        TableName::new("main", "tr_only").column("id"),
    );
    let ok = ChangeSet {
        changes: vec![PlannedChange::new(Change::Table(
            TableChange::CreateTable {
                uid: tr_uid,
                name: TableName::new("main", "tr_only"),
                table: Box::new(ok_table),
                beside_default: None,
                moves_at_most: None,
            },
        ))],
    };
    let ok_side = pbps_diff::Side {
        schema: &ok_desired,
        ids: &ok_ids,
    };
    let facts = catalog_facts(&inventory, &ok, base_side, ok_side, &dialect).expect("facts");
    verify_names(
        &inventory, &ok, base_side, ok_side, &dialect, &facts, "test.db",
    )
    .expect("trigger and PK names are free");

    // Dropping `existing` frees both `existing` and its unmanaged index `ix_owned`.
    let view_id = ModuleId::Named(TableName::new("main", "IX_OWNED"));
    let mut freed_desired = Schema::default();
    freed_desired.modules.insert(
        view_id.clone(),
        Module {
            kind: ModuleKind::View,
            description: None,
            definition: "SELECT 1".into(),
            options: Default::default(),
        },
    );
    let freed_ids = IdsFile::default();
    let freed = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::Table(TableChange::DropTable {
                uid: existing_uid,
                name: TableName::new("main", "existing"),
                detach_from: None,
            })),
            PlannedChange::new(Change::Module(ModuleChange::CreateModule {
                id: view_id,
                module: Box::new(Module {
                    kind: ModuleKind::View,
                    description: None,
                    definition: "SELECT 1".into(),
                    options: Default::default(),
                }),
            })),
        ],
    };
    let freed_side = pbps_diff::Side {
        schema: &freed_desired,
        ids: &freed_ids,
    };
    let facts = catalog_facts(&inventory, &freed, base_side, freed_side, &dialect).expect("facts");
    verify_names(
        &inventory, &freed, base_side, freed_side, &dialect, &facts, "test.db",
    )
    .expect("dropping table frees its indexes");

    // Negative: adding a column colliding with a hidden/generated column refuses
    // naming SQLite and both names.
    let gen_uid = uid(UidKind::Column, "main.existing.GEN_C");
    let mut add_desired = base.clone();
    add_desired
        .tables
        .get_mut(&TableName::new("main", "existing"))
        .expect("table")
        .columns
        .insert("GEN_C".into(), int_col());
    let mut add_ids = base_ids.clone();
    add_ids.columns.insert(
        gen_uid.clone(),
        TableName::new("main", "existing").column("GEN_C"),
    );
    let add_gen = ChangeSet {
        changes: vec![PlannedChange::new(Change::Column(
            ColumnChange::AddColumn {
                uid: gen_uid,
                table: TableName::new("main", "existing"),
                name: "GEN_C".into(),
                column: Box::new(int_col()),
            },
        ))],
    };
    let add_side = pbps_diff::Side {
        schema: &add_desired,
        ids: &add_ids,
    };
    let facts = catalog_facts(&inventory, &add_gen, base_side, add_side, &dialect).expect("facts");
    let err = verify_names(
        &inventory, &add_gen, base_side, add_side, &dialect, &facts, "test.db",
    )
    .unwrap_err();
    assert!(
        err.contains("SQLite") && err.contains("GEN_C") && err.contains("gen_c"),
        "{err}"
    );

    // Negative: creating an index colliding with ledger table `__pbps_state`.
    let ix = Index {
        columns: vec![IndexColumn::column("id")],
        include: Vec::new(),
        unique: false,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
        filegroup: None,
    };
    let mut ix_desired = base.clone();
    ix_desired
        .tables
        .get_mut(&TableName::new("main", "existing"))
        .expect("table")
        .indexes
        .insert("__PBPS_STATE".into(), ix.clone());
    let ledger_ix = ChangeSet {
        changes: vec![PlannedChange::new(Change::Index(IndexChange::AddIndex {
            table: TableName::new("main", "existing"),
            name: "__PBPS_STATE".into(),
            index: Box::new(ix),
            clustered: false,
        }))],
    };
    let ix_side = pbps_diff::Side {
        schema: &ix_desired,
        ids: &base_ids,
    };
    let facts = catalog_facts(&inventory, &ledger_ix, base_side, ix_side, &dialect).expect("facts");
    let err = verify_names(
        &inventory, &ledger_ix, base_side, ix_side, &dialect, &facts, "test.db",
    )
    .unwrap_err();
    assert!(
        err.contains("SQLite") && err.contains("__PBPS_STATE") && err.contains("__pbps_state"),
        "{err}"
    );
}

#[test]
fn a_column_change_on_anything_but_an_ordinary_table_is_refused_by_name() {
    let dialect = Sqlite::default();
    let (schema, ids) = (Schema::default(), IdsFile::default());
    let side = pbps_diff::Side {
        schema: &schema,
        ids: &ids,
    };
    let changes = ChangeSet::default();
    let facts = CatalogFacts::default();
    let asked = |columns: Vec<(&str, TableColumns)>| {
        let inventory = Inventory {
            entries: Vec::new(),
            columns: columns
                .into_iter()
                .map(|(name, found)| (name.to_owned(), found))
                .collect(),
            ..Inventory::default()
        };
        verify_names(
            &inventory, &changes, side, side, &dialect, &facts, "test.db",
        )
    };
    // The inventory holds the tables whose columns the plan changes: a virtual
    // table, a view and a missing table each refuse, whatever the change is.
    let refused = asked(vec![
        ("fts", TableColumns::Virtual),
        ("report", TableColumns::View),
        ("gone", TableColumns::Absent),
    ])
    .unwrap_err();
    for expected in [
        "`main.fts` is a virtual table in SQLite",
        "`main.report` is a view in SQLite",
        "`main.gone` does not exist in SQLite",
    ] {
        assert!(refused.contains(expected), "{refused}");
    }
    // Negative: an ordinary table's columns are not a problem by themselves.
    asked(vec![(
        "items",
        TableColumns::Columns(vec![ColumnEntry {
            name: "id".into(),
            pk: 0,
            hidden: 0,
        }]),
    )])
    .expect("an ordinary table passes");
}

/// Real-SQLite measurements on temporary files confirming how SQLite behaves
/// when a view read by another view or trigger (or holding an INSTEAD OF
/// trigger) is dropped or recreated.
#[test]
fn real_sqlite_file_measurements_confirm_view_drop_broken_view_and_trigger_rules() {
    // 1. DROP VIEW v while view u reads v: DROP succeeds; SELECT from u fails;
    //    recreating v with the column u reads restores u; recreating v without
    //    that column leaves SELECT from u failing with no such column.
    {
        let mut db = Fresh::file();
        let c = db.connection();
        c.execute_batch(
            "CREATE TABLE t(a INTEGER, b INTEGER); \
             INSERT INTO t VALUES (1, 2); \
             CREATE VIEW v AS SELECT a, b FROM t; \
             CREATE VIEW u AS SELECT b FROM v",
        )
        .expect("setup");
        c.execute_batch("DROP VIEW v")
            .expect("SQLite permits DROP VIEW v while u reads v");
        let missing = c
            .query("SELECT * FROM u", &[])
            .expect_err("SELECT from u fails while v is gone")
            .to_string();
        assert!(missing.contains("no such table"), "{missing}");

        c.execute_batch("CREATE VIEW v AS SELECT a, b FROM t")
            .expect("recreate v with b");
        let rows = c.query("SELECT * FROM u", &[]).expect("u works again");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].integer(0).expect("b"), 2);

        c.execute_batch("DROP VIEW v; CREATE VIEW v AS SELECT a FROM t")
            .expect("recreate v without b");
        let bad_col = c
            .query("SELECT * FROM u", &[])
            .expect_err("SELECT from u fails when v lacks b")
            .to_string();
        assert!(bad_col.contains("no such column"), "{bad_col}");
    }

    // 2. A broken view anywhere in the file makes ALTER TABLE RENAME TO,
    //    RENAME COLUMN and DROP COLUMN on an unrelated table fail.
    {
        let mut db = Fresh::file();
        let c = db.connection();
        c.execute_batch(
            "CREATE TABLE t(a INTEGER); \
             CREATE TABLE unrelated(x INTEGER, y INTEGER); \
             CREATE VIEW v AS SELECT a FROM t; \
             CREATE VIEW u AS SELECT a FROM v; \
             DROP VIEW v",
        )
        .expect("setup");
        for stmt in [
            "ALTER TABLE unrelated RENAME TO unrelated2",
            "ALTER TABLE unrelated RENAME COLUMN x TO x2",
            "ALTER TABLE unrelated DROP COLUMN y",
        ] {
            let err = c.execute_batch(stmt).expect_err(stmt).to_string();
            assert!(
                err.contains("error in view u") && err.contains("no such table"),
                "{stmt}: {err}"
            );
        }
    }

    // 3. A trigger whose body reads v: DROP VIEW v succeeds at drop time;
    //    firing the trigger fails; and the broken trigger also blocks
    //    ALTER TABLE RENAME / DROP COLUMN on an unrelated table.
    {
        let mut db = Fresh::file();
        let c = db.connection();
        c.execute_batch(
            "CREATE TABLE t(a INTEGER); \
             CREATE TABLE src(x INTEGER); \
             CREATE TABLE log(v_val INTEGER); \
             CREATE TABLE unrelated(p INTEGER, q INTEGER); \
             CREATE VIEW v AS SELECT a FROM t; \
             CREATE TRIGGER tr_reads_v AFTER INSERT ON src BEGIN \
               INSERT INTO log SELECT a FROM v; \
             END",
        )
        .expect("setup");
        c.execute_batch("DROP VIEW v")
            .expect("SQLite permits DROP VIEW v while trigger body reads v");
        let fire_err = c
            .execute_batch("INSERT INTO src VALUES (1)")
            .expect_err("firing trigger fails when v is gone")
            .to_string();
        assert!(fire_err.contains("no such table"), "{fire_err}");
        for stmt in [
            "ALTER TABLE unrelated RENAME TO unrelated2",
            "ALTER TABLE unrelated RENAME COLUMN p TO p2",
            "ALTER TABLE unrelated DROP COLUMN q",
        ] {
            let err = c.execute_batch(stmt).expect_err(stmt).to_string();
            assert!(
                err.contains("error in trigger tr_reads_v") && err.contains("no such table"),
                "{stmt}: {err}"
            );
        }
    }

    // 4. An INSTEAD OF trigger ON v: DROP VIEW v silently drops the trigger
    //    from sqlite_schema alongside the view.
    {
        let mut db = Fresh::file();
        let c = db.connection();
        c.execute_batch(
            "CREATE TABLE t(a INTEGER); \
             CREATE VIEW v AS SELECT a FROM t; \
             CREATE TRIGGER tr_instead INSTEAD OF INSERT ON v BEGIN \
               INSERT INTO t VALUES (NEW.a); \
             END",
        )
        .expect("setup");
        let before = c
            .query(
                "SELECT tbl_name FROM main.sqlite_schema WHERE type = 'trigger' AND name = 'tr_instead'",
                &[],
            )
            .expect("before");
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].text(0).expect("tbl"), "v");

        c.execute_batch("DROP VIEW v")
            .expect("SQLite permits DROP VIEW v with an INSTEAD OF trigger");
        let after = c
            .query(
                "SELECT name FROM main.sqlite_schema WHERE type = 'trigger' AND name = 'tr_instead'",
                &[],
            )
            .expect("after");
        assert!(
            after.is_empty(),
            "SQLite silently dropped INSTEAD OF trigger with DROP VIEW v: {after:?}"
        );
    }
}

#[test]
fn view_dependents_finds_direct_transitive_diamond_case_folded_and_trigger_readers() {
    let mut db = Fresh::file();
    let c = db.connection();
    c.execute_batch(
        "CREATE TABLE t(a INTEGER); \
         CREATE TABLE log(x INTEGER); \
         CREATE VIEW base_v AS SELECT a FROM t; \
         CREATE VIEW MidView AS SELECT a FROM BASE_V; \
         CREATE VIEW top_v AS SELECT a FROM midview; \
         CREATE VIEW diamond_v AS SELECT m.a FROM base_v b JOIN MidView m ON b.a = m.a; \
         CREATE VIEW ext_v AS SELECT a FROM base_v; \
         CREATE VIEW unrelated_v AS SELECT a FROM t; \
         CREATE TRIGGER tr_body AFTER INSERT ON t BEGIN INSERT INTO log SELECT a FROM BASE_V; END; \
         CREATE TRIGGER tr_instead INSTEAD OF INSERT ON base_v BEGIN INSERT INTO t VALUES (NEW.a); END; \
         CREATE TRIGGER base_v AFTER DELETE ON t BEGIN INSERT INTO log VALUES (OLD.a); END; \
         PRAGMA writable_schema = ON; \
         INSERT INTO sqlite_schema(type, name, tbl_name, rootpage, sql) VALUES \
           ('table', 'missing_vtab', 'missing_vtab', 0, \
            'CREATE VIRTUAL TABLE missing_vtab USING no_such_module_ever()'); \
         PRAGMA writable_schema = OFF",
    )
    .expect("seed");

    let inv = read(c, &[]).expect("inventory read must ignore missing-module virtual table");
    let v_id = ModuleId::Named(TableName::new("main", "base_v"));
    let mid_id = ModuleId::Named(TableName::new("main", "midview"));
    let top_id = ModuleId::Named(TableName::new("main", "top_v"));
    let diamond_id = ModuleId::Named(TableName::new("main", "diamond_v"));
    let unrelated_id = ModuleId::Named(TableName::new("main", "unrelated_v"));
    let known: BTreeSet<ModuleId> = [
        v_id.clone(),
        mid_id.clone(),
        top_id.clone(),
        diamond_id.clone(),
        unrelated_id.clone(),
    ]
    .into_iter()
    .collect();

    let found = view_dependents(&inv, &[v_id.clone(), unrelated_id.clone()], &known)
        .expect("view_dependents");

    // Unreferenced view has an empty reader list.
    assert!(
        found[&unrelated_id].is_empty(),
        "{:?}",
        found[&unrelated_id]
    );

    let deps = &found[&v_id];
    let names: Vec<&str> = deps.iter().map(|d| d.described.as_str()).collect();
    assert!(names.contains(&"view main.MidView"), "{names:?}");
    assert!(names.contains(&"view main.top_v"), "{names:?}");
    assert!(names.contains(&"view main.diamond_v"), "{names:?}");
    assert!(names.contains(&"view main.ext_v"), "{names:?}");
    assert!(
        names.contains(&"trigger main.tr_body on main.t"),
        "{names:?}"
    );
    assert!(
        names.contains(&"trigger main.tr_instead on main.base_v"),
        "{names:?}"
    );
    // Negative: an unrelated view is not reported.
    assert!(
        !names.iter().any(|n| n.contains("unrelated_v")),
        "{names:?}"
    );
    // The match is lexical over the whole statement, so a trigger that is
    // itself named like the view is reported: the conservative side of the
    // documented limit, pinned so that narrowing it is a deliberate change.
    assert!(
        names.contains(&"trigger main.base_v on main.t"),
        "{names:?}"
    );
    // Deepest-first order: both `top_v` and `diamond_v` depend on `MidView`
    // and must come before `MidView`.
    let pos = |label: &str| names.iter().position(|n| *n == label).expect(label);
    assert!(
        pos("view main.top_v") < pos("view main.MidView"),
        "{names:?}"
    );
    assert!(
        pos("view main.diamond_v") < pos("view main.MidView"),
        "{names:?}"
    );
    // Known ASCII case-folded view resolves to the declared ModuleId spelling.
    let mid_dep = deps
        .iter()
        .find(|d| d.described == "view main.MidView")
        .expect("mid");
    assert_eq!(mid_dep.holds, Holds::Module(mid_id));
    // Triggers are Unrepresentable, and INSTEAD OF names silent drop by SQLite.
    let instead_dep = deps
        .iter()
        .find(|d| d.described == "trigger main.tr_instead on main.base_v")
        .expect("instead");
    assert!(
        matches!(&instead_dep.holds, Holds::Unrepresentable(why) if why.contains("INSTEAD OF")),
        "{instead_dep:?}"
    );
}

#[test]
fn view_dependents_treats_absent_view_null_sql_and_foreign_modules_as_errors() {
    let v_id = ModuleId::Named(TableName::new("main", "v"));
    let empty_known = BTreeSet::new();

    // Absent root view is an error, never "no readers".
    let empty_inv = Inventory::default();
    let absent =
        view_dependents(&empty_inv, std::slice::from_ref(&v_id), &empty_known).unwrap_err();
    assert!(
        absent.to_string().contains("SQLite") && absent.to_string().contains("main.v"),
        "{absent}"
    );

    // NULL or empty stored SQL on any view or trigger is a BadRow error.
    for (kind, sql) in [
        (EntryKind::View, None),
        (EntryKind::View, Some("   ".to_owned())),
        (EntryKind::Trigger, None),
    ] {
        let inv = Inventory {
            entries: vec![
                SchemaEntry {
                    kind: EntryKind::View,
                    name: "v".into(),
                    table: "v".into(),
                    sql: Some("CREATE VIEW v AS SELECT 1".into()),
                },
                SchemaEntry {
                    kind,
                    name: "broken".into(),
                    table: "t".into(),
                    sql,
                },
            ],
            columns: BTreeMap::new(),
            ..Inventory::default()
        };
        let err = view_dependents(&inv, std::slice::from_ref(&v_id), &empty_known).unwrap_err();
        assert!(
            matches!(err, DbError::BadRow(_)) && err.to_string().contains("broken"),
            "{err}"
        );
    }

    // Foreign schema or non-view ModuleId refuses naming SQLite.
    let inv = Inventory {
        entries: vec![SchemaEntry {
            kind: EntryKind::View,
            name: "v".into(),
            table: "v".into(),
            sql: Some("CREATE VIEW v AS SELECT 1".into()),
        }],
        columns: BTreeMap::new(),
        ..Inventory::default()
    };
    let foreign_schema = ModuleId::Named(TableName::new("other", "v"));
    let schema_err = view_dependents(&inv, &[foreign_schema], &empty_known).unwrap_err();
    assert!(
        schema_err.to_string().contains("SQLite") && schema_err.to_string().contains("main"),
        "{schema_err}"
    );
    let trigger_id = ModuleId::Trigger {
        on: TableName::new("main", "t"),
        name: "tr".into(),
    };
    let trig_err = view_dependents(&inv, &[trigger_id], &empty_known).unwrap_err();
    assert!(trig_err.to_string().contains("SQLite"), "{trig_err}");
}

#[test]
fn a_reader_is_found_whatever_its_ascii_case_and_an_attached_trigger_says_it_is_lost() {
    let entry = |kind, name: &str, table: &str, sql: &str| SchemaEntry {
        kind,
        name: name.to_owned(),
        table: table.to_owned(),
        sql: Some(sql.to_owned()),
    };
    let inventory = Inventory {
        entries: vec![
            entry(
                EntryKind::View,
                "base_v",
                "base_v",
                "CREATE VIEW base_v AS SELECT a FROM t",
            ),
            entry(
                EntryKind::View,
                "shouting",
                "shouting",
                "CREATE VIEW shouting AS SELECT a FROM BASE_V",
            ),
            entry(
                EntryKind::View,
                "accented",
                "accented",
                "CREATE VIEW accented AS SELECT a FROM \"basé_v\"",
            ),
            entry(
                EntryKind::Trigger,
                "on_insert",
                "BASE_V",
                "CREATE TRIGGER on_insert INSTEAD OF INSERT ON BASE_V BEGIN SELECT 1; END",
            ),
        ],
        columns: BTreeMap::new(),
        ..Inventory::default()
    };
    let root = ModuleId::Named(TableName::new("main", "base_v"));
    let found = view_dependents(&inventory, std::slice::from_ref(&root), &BTreeSet::new())
        .expect("the inventory is readable");
    let readers = &found[&root];
    let described: Vec<&str> = readers.iter().map(|d| d.described.as_str()).collect();
    assert!(described.contains(&"view main.shouting"), "{described:?}");
    // Negative: folding is ASCII only, so another spelling is another name.
    assert!(!described.contains(&"view main.accented"), "{described:?}");
    let attached = readers
        .iter()
        .find(|d| d.described == "trigger main.on_insert on main.BASE_V")
        .expect("the trigger on the view is a dependent");
    assert!(
        matches!(&attached.holds, Holds::Unrepresentable(why) if why.contains("dropped with it")),
        "{:?}",
        attached.holds
    );
}

#[test]
fn real_sqlite_file_measurements_confirm_drop_table_column_and_index_rules() {
    let setup = |stmts: &[&str]| {
        let mut db = Fresh::file();
        let fk_on = db
            .connection()
            .query("PRAGMA foreign_keys", &[])
            .expect("fk")[0]
            .integer(0)
            .expect("int");
        assert_eq!(fk_on, 1);
        for sql in stmts {
            db.connection().execute_batch(sql).expect(sql);
        }
        db
    };

    // M1a: DROP TABLE parent with inbound FK, child empty -> SQLite permits,
    // PRAGMA foreign_key_check returns 0 rows, next INSERT fails with no such table.
    {
        let mut db = setup(&[
            "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id))",
        ]);
        let c = db.connection();
        c.execute_batch("DROP TABLE parent")
            .expect("permitted when child empty");
        assert!(
            c.query("PRAGMA foreign_key_check", &[])
                .expect("fkc")
                .is_empty()
        );
        for ins in [
            "INSERT INTO child VALUES (1, 1)",
            "INSERT INTO child VALUES (2, NULL)",
        ] {
            let err = c.execute_batch(ins).expect_err(ins).to_string();
            assert!(err.contains("no such table: main.parent"), "{ins}: {err}");
        }
    }

    // M1b: DROP TABLE parent with inbound FK, child has referencing row -> SQLite refuses.
    {
        let mut db = setup(&[
            "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id))",
            "INSERT INTO parent VALUES (1)",
            "INSERT INTO child VALUES (1, 1)",
        ]);
        let err = db
            .connection()
            .execute_batch("DROP TABLE parent")
            .unwrap_err()
            .to_string();
        assert!(err.contains("FOREIGN KEY constraint failed"), "{err}");
    }

    // M1c: DROP TABLE parent with child row having NULL FK -> SQLite permits, next non-NULL INSERT fails.
    {
        let mut db = setup(&[
            "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id))",
            "INSERT INTO child VALUES (1, NULL)",
        ]);
        let c = db.connection();
        c.execute_batch("DROP TABLE parent")
            .expect("permitted with NULL FK");
        assert!(
            c.query("PRAGMA foreign_key_check", &[])
                .expect("fkc")
                .is_empty()
        );
        let err = c
            .execute_batch("INSERT INTO child VALUES (2, 1)")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such table: main.parent"), "{err}");
    }

    // M1d: DROP TABLE with self-referencing FK and row -> SQLite permits.
    {
        let mut db = setup(&[
            "CREATE TABLE node (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES node(id))",
            "INSERT INTO node VALUES (1, 1)",
        ]);
        db.connection()
            .execute_batch("DROP TABLE node")
            .expect("self-FK does not block DROP TABLE");
    }

    // M2a/M2b: ALTER TABLE parent DROP COLUMN c after dropping backing UNIQUE INDEX ->
    // SQLite permits EVEN WITH ROWS in child, leaving `foreign key mismatch`.
    for with_rows in [false, true] {
        let mut stmts = vec![
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, c INTEGER)",
            "CREATE UNIQUE INDEX uq_c ON parent(c)",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pc INTEGER REFERENCES parent(c))",
        ];
        if with_rows {
            stmts.push("INSERT INTO parent VALUES (1, 10)");
            stmts.push("INSERT INTO child VALUES (1, 10)");
        }
        stmts.push("DROP INDEX uq_c");
        let mut db = setup(&stmts);
        let c = db.connection();
        c.execute_batch("ALTER TABLE parent DROP COLUMN c")
            .expect("SQLite permits drop");
        let err = c
            .query("PRAGMA foreign_key_check", &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("foreign key mismatch"), "{err}");
        let ins = c
            .execute_batch("INSERT INTO child VALUES (2, 10)")
            .unwrap_err()
            .to_string();
        assert!(ins.contains("foreign key mismatch"), "{ins}");
    }

    // M2c: inline UNIQUE column -> SQLite refuses DROP COLUMN directly.
    {
        let mut db = setup(&[
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, c INTEGER UNIQUE)",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pc INTEGER REFERENCES parent(c))",
        ]);
        let err = db
            .connection()
            .execute_batch("ALTER TABLE parent DROP COLUMN c")
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot drop UNIQUE column"), "{err}");
    }

    // M3a/M3c: DROP INDEX of sole standalone full UNIQUE INDEX behind inbound FK ->
    // SQLite permits (even if a partial unique index exists), next write fails with foreign key mismatch.
    for with_partial in [false, true] {
        let mut stmts = vec![
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, k INTEGER)",
            "CREATE UNIQUE INDEX uq_k ON parent(k)",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pk INTEGER REFERENCES parent(k))",
            "INSERT INTO parent VALUES (1, 10)",
            "INSERT INTO child VALUES (1, 10)",
        ];
        if with_partial {
            stmts.push("CREATE UNIQUE INDEX uq_part ON parent(k) WHERE k > 0");
        }
        let mut db = setup(&stmts);
        let c = db.connection();
        c.execute_batch("DROP INDEX uq_k")
            .expect("SQLite permits DROP INDEX");
        let err = c
            .execute_batch("INSERT INTO child VALUES (2, 10)")
            .unwrap_err()
            .to_string();
        assert!(err.contains("foreign key mismatch"), "{err}");
    }

    // M3b: Another full UNIQUE INDEX covering the same column set (in reversed order) keeps FK valid.
    {
        let mut db = setup(&[
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER)",
            "CREATE UNIQUE INDEX uq_xy ON parent(x, y)",
            "CREATE UNIQUE INDEX uq_yx ON parent(y, x)",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, px INTEGER, py INTEGER, FOREIGN KEY (px, py) REFERENCES parent(x, y))",
            "INSERT INTO parent VALUES (1, 10, 20)",
            "INSERT INTO child VALUES (1, 10, 20)",
        ]);
        let c = db.connection();
        c.execute_batch("DROP INDEX uq_xy")
            .expect("drop first unique index");
        assert!(
            c.query("PRAGMA foreign_key_check", &[])
                .expect("fkc")
                .is_empty()
        );
        c.execute_batch("INSERT INTO child VALUES (2, 10, 20)")
            .expect("FK still works");
    }

    // M4a/M4b: DROP TABLE t while view or trigger on another table reads t ->
    // SQLite permits, breaking the reader AND subsequent ALTER TABLE DROP/RENAME on unrelated tables.
    {
        let mut db = setup(&[
            "CREATE TABLE t (a INTEGER)",
            "CREATE TABLE unrelated (x INTEGER, y INTEGER)",
            "CREATE VIEW v AS SELECT a FROM t",
        ]);
        let c = db.connection();
        c.execute_batch("DROP TABLE t").expect("permitted");
        assert!(
            c.query("SELECT * FROM v", &[])
                .unwrap_err()
                .to_string()
                .contains("no such table")
        );
        c.execute_batch("ALTER TABLE unrelated ADD COLUMN z INTEGER")
            .expect("ADD COLUMN still works");
        for stmt in [
            "ALTER TABLE unrelated DROP COLUMN y",
            "ALTER TABLE unrelated RENAME COLUMN x TO x2",
            "ALTER TABLE unrelated RENAME TO unrelated2",
        ] {
            let err = c.execute_batch(stmt).expect_err(stmt).to_string();
            assert!(err.contains("error in view v"), "{stmt}: {err}");
        }
    }
    {
        let mut db = setup(&[
            "CREATE TABLE t (a INTEGER)",
            "CREATE TABLE other (id INTEGER)",
            "CREATE TABLE log (x INTEGER)",
            "CREATE TABLE unrelated (x INTEGER, y INTEGER)",
            "CREATE TRIGGER tr_reads_t AFTER INSERT ON other BEGIN INSERT INTO log SELECT a FROM t; END",
        ]);
        let c = db.connection();
        c.execute_batch("DROP TABLE t").expect("permitted");
        assert!(
            c.execute_batch("INSERT INTO other VALUES (1)")
                .unwrap_err()
                .to_string()
                .contains("no such table")
        );
        let err = c
            .execute_batch("ALTER TABLE unrelated DROP COLUMN y")
            .unwrap_err()
            .to_string();
        assert!(err.contains("error in trigger tr_reads_t"), "{err}");
    }

    // M4c: DROP TABLE t with trigger ON t silently drops the trigger.
    {
        let mut db = setup(&[
            "CREATE TABLE t (a INTEGER)",
            "CREATE TABLE log (x INTEGER)",
            "CREATE TRIGGER tr_on_t AFTER INSERT ON t BEGIN INSERT INTO log VALUES (NEW.a); END",
        ]);
        let c = db.connection();
        c.execute_batch("DROP TABLE t").expect("permitted");
        let rows = c
            .query(
                "SELECT name FROM main.sqlite_schema WHERE type = 'trigger' AND name = 'tr_on_t'",
                &[],
            )
            .expect("q");
        assert!(rows.is_empty(), "SQLite silently dropped trigger ON t");
    }

    // M5a–M5e: DROP COLUMN refused by SQLite for trigger (ON t or ON other), generated columns, expression index.
    for (label, stmts, expected) in [
        (
            "trigger on t",
            vec![
                "CREATE TABLE t (a INTEGER, b INTEGER)",
                "CREATE TABLE log (x INTEGER)",
                "CREATE TRIGGER tr_on_t AFTER INSERT ON t BEGIN INSERT INTO log VALUES (NEW.b); END",
            ],
            "error in trigger tr_on_t",
        ),
        (
            "trigger on other",
            vec![
                "CREATE TABLE t (a INTEGER, b INTEGER)",
                "CREATE TABLE other (id INTEGER)",
                "CREATE TABLE log (x INTEGER)",
                "CREATE TRIGGER tr_other AFTER INSERT ON other BEGIN INSERT INTO log SELECT b FROM t; END",
            ],
            "error in trigger tr_other",
        ),
        (
            "virtual generated",
            vec![
                "CREATE TABLE t (a INTEGER, b INTEGER, gv INTEGER GENERATED ALWAYS AS (b + 1) VIRTUAL)",
            ],
            "error in table t",
        ),
        (
            "stored generated",
            vec![
                "CREATE TABLE t (a INTEGER, b INTEGER, gs INTEGER GENERATED ALWAYS AS (b + 1) STORED)",
            ],
            "error in table t",
        ),
        (
            "expression index",
            vec![
                "CREATE TABLE t (a INTEGER, b TEXT)",
                "CREATE INDEX ix_expr ON t(lower(b))",
            ],
            "error in index ix_expr",
        ),
    ] {
        let mut db = setup(&stmts);
        let err = db
            .connection()
            .execute_batch("ALTER TABLE t DROP COLUMN b")
            .expect_err(label)
            .to_string();
        assert!(err.contains(expected), "{label}: {err}");
    }
}

fn ids_after_plan(stored_ids: &IdsFile, changes: &[Change]) -> IdsFile {
    let mut out = stored_ids.clone();
    for ch in changes {
        if let Change::Table(TableChange::RenameTable { uid, from, to, .. }) = ch {
            out.tables.insert(uid.clone(), to.clone());
            for col in out.columns.values_mut() {
                if col.table == *from {
                    col.table = to.clone();
                }
            }
        } else if let Change::Table(TableChange::DropTable { uid, name, .. }) = ch {
            out.tables.remove(uid);
            out.columns.retain(|_, col| col.table != *name);
        } else if let Change::Column(ColumnChange::DropColumn { uid, .. }) = ch {
            out.columns.remove(uid);
        }
    }
    out
}

fn drop_tbl(name: &str) -> Change {
    Change::Table(TableChange::DropTable {
        uid: uid(UidKind::Table, &format!("main.{name}")),
        name: TableName::new("main", name),
        detach_from: None,
    })
}

fn drop_col(table: &str, col: &str) -> Change {
    Change::Column(ColumnChange::DropColumn {
        uid: uid(UidKind::Column, &format!("main.{table}.{col}")),
        column: TableName::new("main", table).column(col),
    })
}

fn drop_idx(table: &str, name: &str) -> Change {
    Change::Index(IndexChange::DropIndex {
        table: TableName::new("main", table),
        name: name.into(),
    })
}

fn seed_stored(specs: &[(&str, &[&str])]) -> (Schema, IdsFile) {
    let (mut schema, mut ids) = (Schema::default(), IdsFile::default());
    for &(tname, cols) in specs {
        let tn = TableName::new("main", tname);
        schema.tables.insert(tn.clone(), plain_table(cols));
        ids.tables
            .insert(uid(UidKind::Table, &format!("main.{tname}")), tn.clone());
        for &col in cols {
            ids.columns.insert(
                uid(UidKind::Column, &format!("main.{tname}.{col}")),
                tn.column(col),
            );
        }
    }
    (schema, ids)
}

fn drop_eval(
    c: &Connection,
    stored: &Schema,
    stored_ids: &IdsFile,
    changes: Vec<Change>,
) -> Result<Vec<pbps_db::impact::DropReport>, DbError> {
    let desired_ids = ids_after_plan(stored_ids, &changes);
    let cs = ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    };
    let names = PlanNames::of(&cs.changes, &desired_ids).expect("names");
    let sources = pbps_model::physical_sources::PhysicalSources::of(names, stored, stored_ids)
        .expect("sources");
    let inv = read_for_drops(c, &sources)?;
    drop_blockers(&inv, &sources)
}

#[test]
fn drop_blockers_allows_unreferenced_drops_and_self_fks_and_clears_earlier_plan_steps() {
    use pbps_model::ConstraintChange;

    let mut db = Fresh::file();
    let c = db.connection();
    c.execute_batch(
        "CREATE TABLE solo (id INTEGER PRIMARY KEY, extra INTEGER); \
         CREATE INDEX ix_extra ON solo(extra); \
         CREATE TABLE self_ref (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES self_ref(id)); \
         CREATE TABLE parent (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER, k INTEGER); \
         CREATE UNIQUE INDEX uq_xy ON parent(x, y); \
         CREATE UNIQUE INDEX uq_yx ON parent(y, x); \
         CREATE UNIQUE INDEX uq_pk ON parent(id); \
         CREATE UNIQUE INDEX uq_k ON parent(k); \
         CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER, px INTEGER, py INTEGER, pk INTEGER, \
           CONSTRAINT fk_p FOREIGN KEY (pid) REFERENCES parent(id), \
           CONSTRAINT fk_xy FOREIGN KEY (px, py) REFERENCES parent(x, y), \
           CONSTRAINT fk_k FOREIGN KEY (pk) REFERENCES parent(k)); \
         CREATE VIEW v_solo AS SELECT extra FROM solo; \
         CREATE VIEW v_safe AS SELECT id FROM solo; \
         PRAGMA writable_schema = ON; \
         INSERT INTO sqlite_schema(type, name, tbl_name, rootpage, sql) VALUES \
           ('table', 'ghost_vtab', 'ghost_vtab', 0, 'CREATE VIRTUAL TABLE ghost_vtab USING missing_mod()'); \
         PRAGMA writable_schema = OFF",
    )
    .expect("setup");
    let (stored, stored_ids) = seed_stored(&[
        ("solo", &["id", "extra"]),
        ("self_ref", &["id", "pid"]),
        ("parent", &["id", "x", "y", "k"]),
        ("child", &["id", "pid", "px", "py", "pk"]),
    ]);

    // 1. Unreferenced index, PK-covered / sibling-covered unique index, self-FK table drop,
    //    and column drop after dropping reading view (while `v_safe` stays active) all pass.
    let v_solo = ModuleId::Named(TableName::new("main", "v_solo"));
    let reports = drop_eval(
        c,
        &stored,
        &stored_ids,
        vec![
            drop_idx("solo", "ix_extra"),
            drop_idx("parent", "uq_pk"),
            drop_idx("parent", "uq_xy"),
            Change::Module(ModuleChange::DropModule {
                id: v_solo.clone(),
                kind: ModuleKind::View,
            }),
            drop_col("solo", "extra"),
            drop_tbl("self_ref"),
        ],
    )
    .expect("read and evaluate");
    assert!(
        reports.iter().all(|r| r.blocking.is_empty()),
        "{reports:#?}"
    );

    // 2. Dropping child table before parent clears inbound FKs; dropping named FK clears it too.
    let by_tbl = drop_eval(
        c,
        &stored,
        &stored_ids,
        vec![drop_tbl("child"), drop_tbl("parent")],
    )
    .expect("child before parent");
    assert!(by_tbl.iter().all(|r| r.blocking.is_empty()), "{by_tbl:#?}");

    let by_fk = drop_eval(
        c,
        &stored,
        &stored_ids,
        vec![
            Change::Constraint(ConstraintChange::DropForeignKey {
                table: TableName::new("main", "child"),
                name: "fk_k".into(),
            }),
            drop_idx("parent", "uq_k"),
            drop_col("parent", "k"),
        ],
    )
    .expect("fk before unique index and column");
    assert!(by_fk.iter().all(|r| r.blocking.is_empty()), "{by_fk:#?}");

    // 3. Altering a view to stop reading the table clears it for a later DropTable.
    let by_alter = drop_eval(
        c,
        &stored,
        &stored_ids,
        vec![
            Change::Module(ModuleChange::AlterModule {
                id: v_solo,
                module: Box::new(Module {
                    kind: ModuleKind::View,
                    description: None,
                    definition: "SELECT 1".into(),
                    options: Default::default(),
                }),
            }),
            Change::Module(ModuleChange::DropModule {
                id: ModuleId::Named(TableName::new("main", "v_safe")),
                kind: ModuleKind::View,
            }),
            drop_tbl("solo"),
        ],
    )
    .expect("alter view before drop table");
    assert!(
        by_alter.iter().all(|r| r.blocking.is_empty()),
        "{by_alter:#?}"
    );
}

#[test]
fn drop_blockers_catches_every_permitted_but_broken_blocker_wrong_order_and_renamed_targets() {
    let mut db = Fresh::file();
    let c = db.connection();
    c.execute_batch(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, c INTEGER, k INTEGER, u INTEGER UNIQUE); \
         CREATE UNIQUE INDEX uq_k ON parent(k); \
         CREATE UNIQUE INDEX uq_k_part ON parent(k) WHERE k > 0; \
         CREATE UNIQUE INDEX uq_c ON parent(c); \
         CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id), \
           pc INTEGER REFERENCES parent(c), pk INTEGER REFERENCES parent(k)); \
         CREATE TABLE other (id INTEGER PRIMARY KEY); \
         CREATE VIEW v_star AS SELECT * FROM parent; \
         CREATE TRIGGER tr_on_p AFTER INSERT ON parent BEGIN SELECT NEW.c; END; \
         CREATE TRIGGER tr_reads_p AFTER INSERT ON other BEGIN SELECT c FROM parent; END",
    )
    .expect("setup");
    let (stored, stored_ids) = seed_stored(&[
        ("parent", &["id", "c", "k", "u"]),
        ("child", &["id", "pid", "pc", "pk"]),
    ]);

    // 1. DropTable(parent) BEFORE DropTable(child) refuses naming child FK, v_star, tr_on_p, tr_reads_p.
    let table_reps = drop_eval(
        c,
        &stored,
        &stored_ids,
        vec![drop_tbl("parent"), drop_tbl("child")],
    )
    .expect("reports");
    let joined = table_reps[0].blocking.join("\n");
    for pat in [
        "main.child",
        "main.parent",
        "view `main.v_star`",
        "trigger `main.tr_on_p`",
        "dropped with it",
        "trigger `main.tr_reads_p`",
    ] {
        assert!(joined.contains(pat), "missing `{pat}` in:\n{joined}");
    }

    // 2. Sole unique index (with partial sibling) refused; autoindex refused.
    let auto_name = c
        .query(
            "SELECT name FROM main.sqlite_schema WHERE type = 'index' AND tbl_name = 'parent' AND name LIKE 'sqlite_autoindex_%'",
            &[],
        )
        .expect("autoindex")[0]
        .text(0)
        .expect("name")
        .to_owned();
    let idx_reps = drop_eval(
        c,
        &stored,
        &stored_ids,
        vec![drop_idx("parent", "uq_k"), drop_idx("parent", &auto_name)],
    )
    .expect("idx reports");
    let k_msg = idx_reps[0].blocking.join("\n");
    assert!(
        k_msg.contains("main.child") && k_msg.contains("uq_k") && k_msg.contains("mismatch"),
        "{k_msg}"
    );
    assert!(
        idx_reps[1]
            .blocking
            .join("\n")
            .contains("PRIMARY KEY or UNIQUE"),
        "{:?}",
        idx_reps[1]
    );

    // 3. DropColumn(parent.c) after dropping uq_c still blocked by inbound FK, v_star (`*`), and both triggers;
    //    DropColumn(parent.id) blocked by PK; DropColumn(child.pid) blocked by outbound FK.
    let col_reps = drop_eval(
        c,
        &stored,
        &stored_ids,
        vec![
            drop_idx("parent", "uq_c"),
            drop_col("parent", "c"),
            drop_col("parent", "id"),
            drop_col("child", "pid"),
        ],
    )
    .expect("col reports");
    let c_msg = col_reps[1].blocking.join("\n");
    for pat in [
        "main.child",
        "main.parent.c",
        "view `main.v_star`",
        "trigger `main.tr_on_p`",
        "trigger `main.tr_reads_p`",
    ] {
        assert!(c_msg.contains(pat), "missing `{pat}` in:\n{c_msg}");
    }
    assert!(
        col_reps[2].blocking.join("\n").contains("primary key"),
        "{:?}",
        col_reps[2]
    );
    assert!(
        col_reps[3].blocking.join("\n").contains("main.child"),
        "{:?}",
        col_reps[3]
    );

    // 4. Renamed table before drop resolves via PhysicalSources to catalog name `parent`.
    let p_uid = uid(UidKind::Table, "main.parent");
    let ren_reps = drop_eval(
        c,
        &stored,
        &stored_ids,
        vec![
            Change::Table(TableChange::RenameTable {
                uid: p_uid.clone(),
                from: TableName::new("main", "parent"),
                to: TableName::new("main", "parent_renamed"),
                defaults: Vec::new(),
            }),
            Change::Table(TableChange::DropTable {
                uid: p_uid,
                name: TableName::new("main", "parent_renamed"),
                detach_from: None,
            }),
        ],
    )
    .expect("renamed drop");
    assert!(
        ren_reps[0]
            .blocking
            .iter()
            .any(|b| b.contains("main.child")),
        "{:?}",
        ren_reps[0]
    );
}

#[test]
fn drop_blockers_refuses_rebuild_changes_absent_targets_and_unreadable_or_missing_metadata() {
    use pbps_model::ConstraintChange;

    let mut db = Fresh::file();
    db.connection()
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)")
        .expect("setup");
    // Ledger also records `ghost` / `t.ghost` dropped out-of-band in SQLite.
    let (stored, stored_ids) = seed_stored(&[("t", &["id", "a", "ghost"]), ("ghost", &["id"])]);
    let t = TableName::new("main", "t");
    let t_uid = uid(UidKind::Table, "main.t");
    let a_uid = uid(UidKind::Column, "main.t.a");

    // 1. Rebuild-requiring and unsupported changes refuse naming SQLite.
    for ch in [
        Change::Column(ColumnChange::AlterColumnType {
            uid: a_uid.clone(),
            column: t.column("a"),
            from: "integer".parse().expect("ty"),
            to: "text".parse().expect("ty"),
            from_nullable: true,
            to_nullable: true,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
            using: None,
        }),
        Change::Constraint(ConstraintChange::DropUnique {
            table: t.clone(),
            name: "uq_a".into(),
        }),
        Change::Constraint(ConstraintChange::SetPrimaryKey {
            table: t.clone(),
            from: Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
                filegroup: None,
            }),
            to: None,
            nonclustered: false,
        }),
        Change::Column(ColumnChange::AlterColumnNumbering {
            uid: a_uid,
            column: t.column("a"),
            ty: "integer".parse().expect("ty"),
            from: pbps_model::ValueSource::None,
            to: pbps_model::ValueSource::None,
        }),
        Change::Table(TableChange::DropTable {
            uid: t_uid,
            name: t.clone(),
            detach_from: Some(TableName::new("main", "p")),
        }),
    ] {
        let desired_ids = ids_after_plan(&stored_ids, std::slice::from_ref(&ch));
        let cs = ChangeSet {
            changes: vec![PlannedChange::new(ch)],
        };
        assert!(has_drop_blocker_target(&cs));
        let names = PlanNames::of(&cs.changes, &desired_ids).expect("names");
        let sources =
            pbps_model::physical_sources::PhysicalSources::of(names, &stored, &stored_ids)
                .expect("sources");
        let err = drop_blockers(&Inventory::default(), &sources).unwrap_err();
        assert!(err.to_string().contains("SQLite"), "{err}");
    }

    // 2. Absent table, column or index refuses (never "no blockers").
    for ch in [
        drop_tbl("ghost"),
        drop_col("t", "ghost"),
        drop_idx("t", "ix_ghost"),
    ] {
        let err = drop_eval(db.connection(), &stored, &stored_ids, vec![ch]).unwrap_err();
        assert!(err.to_string().contains("not in SQLite's catalog"), "{err}");
    }

    // 3. Unread metadata (`foreign_keys: None`, NULL view `sql`) errors.
    let drop_t_ch = drop_tbl("t");
    let desired_ids = ids_after_plan(&stored_ids, std::slice::from_ref(&drop_t_ch));
    let drop_t = ChangeSet {
        changes: vec![PlannedChange::new(drop_t_ch)],
    };
    let names = PlanNames::of(&drop_t.changes, &desired_ids).expect("names");
    let sources = pbps_model::physical_sources::PhysicalSources::of(names, &stored, &stored_ids)
        .expect("sources");
    assert!(matches!(
        drop_blockers(&Inventory::default(), &sources),
        Err(DbError::BadRow(_))
    ));

    let null_view = Inventory {
        entries: vec![SchemaEntry {
            kind: EntryKind::View,
            name: "broken_v".into(),
            table: "broken_v".into(),
            sql: None,
        }],
        foreign_keys: Some(BTreeMap::new()),
        ..Inventory::default()
    };
    assert!(matches!(
        drop_blockers(&null_view, &sources),
        Err(DbError::BadRow(_))
    ));

    // 4. Exclusively locked file errors on read_for_drops, never "no blockers".
    let locker = Connection::open(db.path(), OpenMode::ExistingOnly).expect("second conn");
    locker.execute_batch("BEGIN EXCLUSIVE").expect("lock");
    let lock_err = read_for_drops(db.connection(), &sources).unwrap_err();
    assert_eq!(
        lock_err.server_error_code().as_deref(),
        Some("5"),
        "{lock_err}"
    );
    locker.execute_batch("ROLLBACK").expect("unlock");
}

#[test]
fn a_sibling_unique_index_covers_whatever_the_case_of_its_collation_name() {
    let mut db = Fresh::file();
    let c = db.connection();
    c.execute_batch(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, k TEXT COLLATE NOCASE, b TEXT); \
         CREATE UNIQUE INDEX uq_upper ON parent(k COLLATE NOCASE); \
         CREATE UNIQUE INDEX uq_lower ON parent(k COLLATE nocase); \
         CREATE UNIQUE INDEX uq_b ON parent(b COLLATE NOCASE); \
         CREATE UNIQUE INDEX uq_b_binary ON parent(b); \
         CREATE TABLE child (id INTEGER PRIMARY KEY, pk TEXT, pb TEXT, \
           CONSTRAINT fk_k FOREIGN KEY (pk) REFERENCES parent(k), \
           CONSTRAINT fk_b FOREIGN KEY (pb) REFERENCES parent(b))",
    )
    .expect("setup");
    let (stored, stored_ids) = seed_stored(&[
        ("parent", &["id", "k", "b"]),
        ("child", &["id", "pk", "pb"]),
    ]);
    let reports = drop_eval(
        c,
        &stored,
        &stored_ids,
        vec![drop_idx("parent", "uq_upper")],
    )
    .expect("the catalog is readable");
    assert!(reports[0].blocking.is_empty(), "{:?}", reports[0].blocking);
    // Negative: another collation is another key, so it does not cover.
    let reports = drop_eval(
        c,
        &stored,
        &stored_ids,
        vec![drop_idx("parent", "uq_b_binary")],
    )
    .expect("the catalog is readable");
    assert!(
        reports[0].blocking.iter().any(|b| b.contains("fk_b")),
        "{:?}",
        reports[0].blocking
    );
}
