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
                    hidden: 0
                },
                ColumnEntry {
                    name: "b".into(),
                    hidden: 0
                },
                ColumnEntry {
                    name: "gv".into(),
                    hidden: 2
                },
                ColumnEntry {
                    name: "gs".into(),
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
                hidden: 0
            },
            ColumnEntry {
                name: "a".into(),
                hidden: 0
            },
            ColumnEntry {
                name: "g".into(),
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
            },
            SchemaEntry {
                kind: EntryKind::Table,
                name: "café".into(),
                table: "café".into(),
            },
            SchemaEntry {
                kind: EntryKind::View,
                name: "unmanaged_v".into(),
                table: "unmanaged_v".into(),
            },
        ],
        columns: BTreeMap::new(),
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
            },
            SchemaEntry {
                kind: EntryKind::Index,
                name: "ix_owned".into(),
                table: "existing".into(),
            },
            SchemaEntry {
                kind: EntryKind::Table,
                name: "__pbps_state".into(),
                table: "__pbps_state".into(),
            },
            SchemaEntry {
                kind: EntryKind::Trigger,
                name: "tr_only".into(),
                table: "existing".into(),
            },
        ],
        columns: [(
            "existing".to_owned(),
            TableColumns::Columns(vec![
                ColumnEntry {
                    name: "id".into(),
                    hidden: 0,
                },
                ColumnEntry {
                    name: "gen_c".into(),
                    hidden: 2,
                },
            ]),
        )]
        .into(),
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
