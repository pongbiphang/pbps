//! The property `pull` stands on: what it writes must read back as exactly the
//! schema it saw.
//!
//! If this round trip is lossy, the first `plan` after a pull manufactures
//! changes against the very database the declarations came from — the worst
//! possible first impression, and with drops in it, a dangerous one.

use std::path::Path;

use pbps_mssql::introspect::{
    IndexKind, ModuleKind, RawCatalog, RawCheck, RawColumn, RawForeignKeyColumn, RawIndexColumn,
    RawKeyColumn, RawModule, RawTable, assemble,
};

/// A catalog exercising every construct the model can express.
fn full_catalog() -> RawCatalog {
    let col = |object_id, name: &str, type_name: &str| RawColumn {
        object_id,
        name: name.into(),
        type_name: type_name.into(),
        max_length: 8,
        precision: 0,
        scale: 0,
        is_nullable: true,
        is_computed: false,
        is_user_defined_type: false,
        identity: None,
        default: None,
        default_constraint: None,
        collation: None,
    };

    let mut id = col(1, "id", "bigint");
    id.is_nullable = false;
    id.identity = Some((1, 1));
    let mut email = col(1, "email", "nvarchar");
    email.max_length = 510;
    let mut status = col(1, "status", "tinyint");
    status.is_nullable = false;
    status.default = Some("((0))".into());
    let mut price = col(1, "price", "numeric");
    (price.precision, price.scale) = (18, 2);
    let mut region = col(2, "region_id", "int");
    region.is_nullable = false;

    RawCatalog {
        roles: Vec::new(),
        permissions: Vec::new(),
        tables: vec![
            raw_table(1, "dbo", "customer"),
            raw_table(2, "app", "region"),
        ],
        columns: vec![id, email, status, price, region],
        key_columns: vec![
            RawKeyColumn {
                object_id: 1,
                constraint_name: "pk_customer".into(),
                is_primary: true,
                column: "id".into(),
            },
            RawKeyColumn {
                object_id: 1,
                constraint_name: "uq_customer_email".into(),
                is_primary: false,
                column: "email".into(),
            },
            RawKeyColumn {
                object_id: 2,
                constraint_name: "pk_region".into(),
                is_primary: true,
                column: "region_id".into(),
            },
        ],
        foreign_key_columns: vec![RawForeignKeyColumn {
            object_id: 1,
            constraint_name: "fk_customer_region".into(),
            ref_schema: "app".into(),
            ref_table: "region".into(),
            column: "status".into(),
            ref_column: "region_id".into(),
            on_delete: 0,
            on_update: 1,
        }],
        checks: vec![RawCheck {
            object_id: 1,
            constraint_object_id: 12,
            name: "ck_price".into(),
            definition: "([price]>(0))".into(),
        }],
        index_columns: vec![
            RawIndexColumn {
                object_id: 1,
                index_name: "ix_customer_email".into(),
                is_unique: false,
                kind: IndexKind::Nonclustered,
                filter: Some("([email] IS NOT NULL)".into()),
                column: "email".into(),
                is_included: false,
                is_descending: true,
            },
            RawIndexColumn {
                object_id: 1,
                index_name: "ix_customer_email".into(),
                is_unique: false,
                kind: IndexKind::Nonclustered,
                filter: Some("([email] IS NOT NULL)".into()),
                column: "status".into(),
                is_included: true,
                is_descending: false,
            },
        ],
        modules: vec![
            RawModule {
                object_id: 20,
                parent_object_id: None,
                schema: "dbo".into(),
                name: "v_active".into(),
                kind: ModuleKind::View,
                definition: Some(
                    "CREATE OR ALTER VIEW [dbo].[v_active]\nAS\nSELECT id\nFROM dbo.customer\nWHERE status = 0"
                        .into(),
                ),
                parent: None,
                requires_bound_references: true,
                default_set_options: true,
            },
            RawModule {
                object_id: 21,
                parent_object_id: Some(1),
                schema: "dbo".into(),
                name: "tr_customer".into(),
                kind: ModuleKind::Trigger,
                definition: Some(
                    "CREATE TRIGGER dbo.tr_customer ON dbo.customer AFTER INSERT AS SELECT 1;"
                        .into(),
                ),
                parent: Some(("dbo".into(), "customer".into())),
                requires_bound_references: false,
                default_set_options: true,
            },
        ],
        object_dependencies: Vec::new(),
        // None of this fixture's columns carry a collation, so this baseline
        // is never compared against; it is a real collation name rather than
        // an empty string only so the fixture reads like the catalog it
        // stands in for.
        database_collation: "SQL_Latin1_General_CP1_CI_AS".into(),
    }
}

fn raw_table(object_id: i32, schema: &str, name: &str) -> RawTable {
    RawTable {
        object_id,
        schema: schema.into(),
        name: name.into(),
        temporal_type: 0,
        has_period: false,
    }
}

#[test]
fn what_pull_writes_reads_back_as_the_same_schema() {
    let pulled = assemble(&full_catalog());
    assert_eq!(pulled.warnings, Vec::<String>::new());
    assert_eq!(pulled.schema.tables.len(), 2);

    for (name, table) in &pulled.schema.tables {
        let yaml = pbps_load::render(name, table, &[], None);
        let loaded = pbps_load::load_table_str(Path::new("pulled.yml"), &yaml)
            .unwrap_or_else(|e| panic!("{name}: pulled YAML does not parse: {e:?}"));
        assert_eq!(&loaded.name, name);
        assert_eq!(
            &loaded.table, table,
            "{name}: the pulled declaration is lossy\n---\n{yaml}"
        );
        assert!(loaded.intents.is_empty());
    }
}

/// The same property for modules: what `pull` writes has to read back as the
/// module it saw, definition and all. If it did not, the first plan after a
/// pull would propose re-stating every view in the database.
#[test]
fn what_pull_writes_for_a_module_reads_back_as_the_same_module() {
    let pulled = assemble(&full_catalog());
    assert!(
        pulled.unmanaged_modules.is_empty(),
        "{:?}",
        pulled.unmanaged_modules
    );
    assert_eq!(pulled.schema.modules.len(), 2);

    for (id, module) in &pulled.schema.modules {
        let yaml = pbps_load::render_module(id, module, &Default::default());
        let loaded = pbps_load::load_module_str(Path::new("pulled.yml"), &yaml)
            .unwrap_or_else(|e| panic!("{id}: pulled YAML does not parse: {e:?}"));
        // The identity too, not only the body: a trigger's file writes
        // `trigger: dbo.tr_customer` beside `on: dbo.customer`, and the two
        // have to fold back into the one identity the catalog gave
        // (ADR-0009 §1).
        assert_eq!(&loaded.id, id);
        assert_eq!(
            &loaded.module, module,
            "{id}: the pulled declaration is lossy\n---\n{yaml}"
        );
    }
}
