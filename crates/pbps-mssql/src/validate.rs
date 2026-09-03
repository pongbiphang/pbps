//! What SQL Server will refuse, checked before anything is generated.
//!
//! These are the errors worth catching in `pbps validate`, where the user sees
//! the file and the line, rather than at apply time as a message from the server
//! about a table it half-created. Every check here is a rule of the engine, not a
//! matter of taste — style opinions belong in `fmt`, not in an error.

use pbps_dialect::DialectError;
use pbps_model::{GrantTarget, Module, ModuleKind, ObjectName, Role, Table, TableName};

use crate::ident;
use crate::types::{self, DIALECT};

/// SQL Server's limit on the number of key columns in one index.
const MAX_INDEX_KEY_COLUMNS: usize = 32;

fn invalid(message: impl Into<String>) -> DialectError {
    DialectError::Invalid {
        dialect: DIALECT,
        message: message.into(),
    }
}

/// Every problem with a role (ADR-0005): the names it uses have to be ones
/// this dialect can write into `GRANT` and `CREATE ROLE`. `public` and the
/// fixed database roles are the engine's own and cannot be created, dropped or
/// renamed; declaring one would plan a statement the engine refuses.
pub fn role(name: &str, role: &Role) -> Vec<DialectError> {
    let mut errs = Vec::new();
    if let Err(e) = ident::quote(name) {
        errs.push(e);
    }
    if FIXED_ROLES.iter().any(|f| f.eq_ignore_ascii_case(name)) {
        errs.push(invalid(format!(
            "`{name}` is a built-in database role, which cannot be created, dropped or renamed; \
             declare a role of your own and grant to that"
        )));
    }
    for target in role.grants.keys() {
        let parts: Vec<&str> = match target {
            GrantTarget::Object(o) => vec![&o.schema, &o.name],
            GrantTarget::Schema(s) => vec![s],
        };
        for part in parts {
            if let Err(e) = ident::quote(part) {
                errs.push(e);
            }
        }
    }
    errs
}

/// The roles every SQL Server database has, which no declaration may claim.
pub const FIXED_ROLES: [&str; 10] = [
    "public",
    "db_owner",
    "db_accessadmin",
    "db_securityadmin",
    "db_ddladmin",
    "db_backupoperator",
    "db_datareader",
    "db_datawriter",
    "db_denydatareader",
    "db_denydatawriter",
];

/// Every problem with a module, not just the first (ADR-0002).
///
/// The checks are few on purpose. Whether the body compiles is the engine's
/// question, and asking it here would mean parsing T-SQL — which this tool does
/// not do. What is checked is what the *emitter* needs to be true in order to
/// produce a statement at all, plus the two shapes that would otherwise become
/// a puzzling engine error on a database that is already half-changed.
pub fn module(name: &ObjectName, module: &Module) -> Vec<DialectError> {
    let mut errs = Vec::new();

    for part in [&name.schema, &name.name] {
        if let Err(e) = ident::quote(part) {
            errs.push(e);
        }
    }

    let body = module.definition.trim();
    if body.is_empty() {
        errs.push(invalid(format!(
            "{} `{name}` has an empty definition",
            module.kind
        )));
    }

    // A `GO` is not T-SQL: it is a batch separator the client interprets. One
    // inside a definition would be sent to the server verbatim and rejected —
    // and a user who wrote it meant to split the object into pieces that
    // `CREATE OR ALTER` cannot express.
    //
    // Read off the code, not the raw text: T-SQL allows the word inside a
    // literal or a comment, and a procedure that returns or documents a script
    // is a perfectly ordinary thing to want to manage.
    if pbps_model::module::code_only(body)
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("go"))
    {
        errs.push(invalid(format!(
            "{} `{name}` contains a `GO` batch separator; a module is one batch, and `GO` is a \
             client instruction rather than something the server understands",
            module.kind
        )));
    }

    // An encrypted module has no readable definition, so pbps could never
    // compare it and would re-state it on every plan. Saying so at validate
    // time is better than a drift report that never goes quiet.
    // Matched on the collapsed text, not the literal string: `WITH\nENCRYPTION`
    // and `WITH  ENCRYPTION` are the same option, and a declaration that slipped
    // past this check would be applied and then come back with a NULL
    // definition — the module would drop out of every snapshot and every later
    // plan would try to create it again.
    if pbps_dialect::Dialect::normalize_definition(&crate::Mssql, body)
        .to_ascii_uppercase()
        .contains("WITH ENCRYPTION")
    {
        errs.push(invalid(format!(
            "{} `{name}` is declared WITH ENCRYPTION, whose definition cannot be read back; \
             pbps cannot manage it (ADR-0002)",
            module.kind
        )));
    }

    match (module.kind, &module.on) {
        (ModuleKind::Trigger, None) => errs.push(invalid(format!(
            "trigger `{name}` does not say which table it is on (`on:`)"
        ))),
        (ModuleKind::Trigger, Some(table)) => {
            for part in [&table.schema, &table.name] {
                if let Err(e) = ident::quote(part) {
                    errs.push(e);
                }
            }
        }
        (kind, Some(table)) => errs.push(invalid(format!(
            "`{name}` is a {kind} and cannot be `on: {table}`; only a trigger names a table"
        ))),
        (_, None) => {}
    }

    errs
}

/// Every problem with the table, not just the first.
///
/// Stopping at the first would turn fixing a table into as many round trips as
/// it has mistakes.
pub fn table(name: &TableName, table: &Table) -> Vec<DialectError> {
    let mut errs = Vec::new();

    for part in [&name.schema, &name.name] {
        if let Err(e) = ident::quote(part) {
            errs.push(e);
        }
    }

    let mut identity_columns = Vec::new();
    for (col_name, col) in &table.columns {
        if let Err(e) = ident::quote(col_name) {
            errs.push(e);
        }
        match types::normalize(&col.ty) {
            Ok(_) => {}
            Err(e) => {
                errs.push(e);
                // Everything below asks questions about the type, and asking
                // them of a type that does not exist only produces noise on top
                // of the real error.
                continue;
            }
        }
        if let Some(identity) = col.identity {
            identity_columns.push(col_name.clone());
            if !types::can_be_identity(&col.ty) {
                errs.push(invalid(format!(
                    "column `{col_name}` is IDENTITY, which needs an integer type or a decimal with scale 0, not `{}`",
                    col.ty
                )));
            }
            if col.nullable {
                errs.push(invalid(format!(
                    "column `{col_name}` is IDENTITY, so it cannot be nullable"
                )));
            }
            if identity.increment == 0 {
                errs.push(invalid(format!(
                    "column `{col_name}` has an IDENTITY increment of 0, which never advances"
                )));
            }
            if col.default.is_some() {
                errs.push(invalid(format!(
                    "column `{col_name}` is IDENTITY, so it cannot also have a default"
                )));
            }
        }
    }
    if identity_columns.len() > 1 {
        errs.push(invalid(format!(
            "a table may have only one IDENTITY column, but `{}` are all marked",
            identity_columns.join("`, `")
        )));
    }

    if let Some(pk) = &table.primary_key {
        if let Some(n) = &pk.name
            && let Err(e) = ident::quote(n)
        {
            errs.push(e);
        }
        errs.extend(key_columns("primary key", &pk.columns, table));
        for c in &pk.columns {
            if table.columns.get(c).is_some_and(|c| c.nullable) {
                errs.push(invalid(format!(
                    "primary key column `{c}` is nullable; a primary key column must be NOT NULL"
                )));
            }
        }
    }

    for (n, u) in &table.unique {
        if let Err(e) = ident::quote(n) {
            errs.push(e);
        }
        errs.extend(key_columns(
            &format!("unique constraint `{n}`"),
            &u.columns,
            table,
        ));
    }

    for (n, fk) in &table.foreign_keys {
        if let Err(e) = ident::quote(n) {
            errs.push(e);
        }
        errs.extend(key_columns(
            &format!("foreign key `{n}`"),
            &fk.columns,
            table,
        ));
        if fk.columns.len() != fk.references_columns.len() {
            errs.push(invalid(format!(
                "foreign key `{n}` has {} column(s) but references {}; the two sides must line up",
                fk.columns.len(),
                fk.references_columns.len()
            )));
        }
        if fk.references_columns.is_empty() {
            errs.push(invalid(format!(
                "foreign key `{n}` names no columns on the referenced table"
            )));
        }
    }

    for (n, c) in &table.checks {
        if let Err(e) = ident::quote(n) {
            errs.push(e);
        }
        if c.expression.trim().is_empty() {
            errs.push(invalid(format!(
                "check constraint `{n}` has an empty expression"
            )));
        }
    }

    for (n, idx) in &table.indexes {
        if let Err(e) = ident::quote(n) {
            errs.push(e);
        }
        let keys: Vec<String> = idx.columns.iter().map(|c| c.name.clone()).collect();
        errs.extend(key_columns(&format!("index `{n}`"), &keys, table));
        if keys.len() > MAX_INDEX_KEY_COLUMNS {
            errs.push(invalid(format!(
                "index `{n}` has {} key columns; SQL Server allows at most {MAX_INDEX_KEY_COLUMNS}",
                keys.len()
            )));
        }
        for inc in &idx.include {
            if !table.columns.contains_key(inc) {
                errs.push(invalid(format!(
                    "index `{n}` includes `{inc}`, which is not a column of this table"
                )));
            }
            if keys.contains(inc) {
                errs.push(invalid(format!(
                    "index `{n}` has `{inc}` both as a key column and as an included column"
                )));
            }
        }
        if idx.filter.as_ref().is_some_and(|f| f.trim().is_empty()) {
            errs.push(invalid(format!(
                "index `{n}` has an empty filter expression"
            )));
        }
    }

    errs
}

/// The checks shared by every construct that builds a key out of columns.
fn key_columns(what: &str, columns: &[String], table: &Table) -> Vec<DialectError> {
    let mut errs = Vec::new();
    if columns.is_empty() {
        errs.push(invalid(format!("{what} names no columns")));
    }
    let mut seen = Vec::new();
    for c in columns {
        match table.columns.get(c) {
            None => errs.push(invalid(format!(
                "{what} references `{c}`, which is not a column of this table"
            ))),
            Some(col) if !types::is_indexable(&col.ty) => errs.push(invalid(format!(
                "{what} uses `{c}`, whose type `{}` cannot be part of a key",
                col.ty
            ))),
            Some(_) => {}
        }
        if seen.contains(&c) {
            errs.push(invalid(format!("{what} names `{c}` twice")));
        }
        seen.push(c);
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{
        CheckConstraint, Column, ColumnType, ForeignKey, Identity, Index, IndexColumn, PrimaryKey,
        UniqueConstraint,
    };

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    fn base_table() -> (TableName, Table) {
        let mut t = Table::default();
        t.columns
            .insert("id".into(), Column::new(ty("bigint")).not_null());
        t.columns
            .insert("email".into(), Column::new(ty("nvarchar(255)")));
        t.columns
            .insert("body".into(), Column::new(ty("nvarchar(max)")));
        (TableName::new("dbo", "customer"), t)
    }

    fn messages(errs: &[DialectError]) -> String {
        errs.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_well_formed_table_produces_no_errors() {
        let (name, mut t) = base_table();
        t.primary_key = Some(PrimaryKey {
            name: Some("pk_customer".into()),
            columns: vec!["id".into()],
        });
        t.unique.insert(
            "uq_email".into(),
            UniqueConstraint {
                columns: vec!["email".into()],
            },
        );
        assert_eq!(messages(&table(&name, &t)), "");
    }

    #[test]
    fn a_key_over_a_missing_column_is_reported() {
        let (name, mut t) = base_table();
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["nope".into()],
        });
        let errs = table(&name, &t);
        assert!(
            messages(&errs).contains("not a column of this table"),
            "{}",
            messages(&errs)
        );
    }

    /// The engine refuses a nullable PK column at CREATE time; catching it at
    /// validate time points at the file instead of at a failed apply.
    #[test]
    fn a_nullable_primary_key_column_is_reported() {
        let (name, mut t) = base_table();
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["email".into()],
        });
        assert!(messages(&table(&name, &t)).contains("must be NOT NULL"));
    }

    #[test]
    fn a_key_over_an_unindexable_type_is_reported() {
        let (name, mut t) = base_table();
        t.unique.insert(
            "uq_body".into(),
            UniqueConstraint {
                columns: vec!["body".into()],
            },
        );
        assert!(messages(&table(&name, &t)).contains("cannot be part of a key"));
    }

    #[test]
    fn identity_rules_are_enforced() {
        let (name, mut t) = base_table();
        let mut c = Column::new(ty("nvarchar(10)"));
        c.identity = Some(Identity {
            seed: 1,
            increment: 0,
        });
        t.columns.insert("seq".into(), c);
        let msg = messages(&table(&name, &t));
        assert!(msg.contains("needs an integer type"), "{msg}");
        assert!(msg.contains("cannot be nullable"), "{msg}");
        assert!(msg.contains("never advances"), "{msg}");
    }

    #[test]
    fn two_identity_columns_are_reported() {
        let (name, mut t) = base_table();
        for col in ["a", "b"] {
            let mut c = Column::new(ty("int")).not_null();
            c.identity = Some(Identity {
                seed: 1,
                increment: 1,
            });
            t.columns.insert(col.into(), c);
        }
        assert!(messages(&table(&name, &t)).contains("only one IDENTITY column"));
    }

    #[test]
    fn a_foreign_key_with_mismatched_sides_is_reported() {
        let (name, mut t) = base_table();
        t.foreign_keys.insert(
            "fk_x".into(),
            ForeignKey {
                columns: vec!["id".into(), "email".into()],
                references_table: TableName::new("dbo", "other"),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        assert!(messages(&table(&name, &t)).contains("must line up"));
    }

    #[test]
    fn an_index_including_one_of_its_own_keys_is_reported() {
        let (name, mut t) = base_table();
        t.indexes.insert(
            "ix_email".into(),
            Index {
                columns: vec![IndexColumn {
                    name: "email".into(),
                    descending: false,
                }],
                include: vec!["email".into()],
                unique: false,
                filter: None,
            },
        );
        let msg = messages(&table(&name, &t));
        assert!(
            msg.contains("both as a key column and as an included column"),
            "{msg}"
        );
    }

    #[test]
    fn empty_expressions_are_reported() {
        let (name, mut t) = base_table();
        t.checks.insert(
            "ck".into(),
            CheckConstraint {
                expression: "  ".into(),
            },
        );
        assert!(messages(&table(&name, &t)).contains("empty expression"));
    }

    /// One pass must surface every problem: the loop must not stop at the first.
    #[test]
    fn all_problems_are_reported_in_one_pass() {
        let (name, mut t) = base_table();
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["nope".into()],
        });
        t.checks.insert(
            "ck".into(),
            CheckConstraint {
                expression: "".into(),
            },
        );
        assert!(table(&name, &t).len() >= 2);
    }

    // ---- modules ----

    fn a_module(kind: ModuleKind, definition: &str) -> Module {
        Module {
            kind,
            description: None,
            on: None,
            definition: definition.to_owned(),
        }
    }

    fn module_errors(name: &str, m: &Module) -> String {
        module(&name.parse().unwrap(), m)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_well_formed_view_produces_no_errors() {
        assert!(
            module(
                &"dbo.active_customer".parse().unwrap(),
                &a_module(ModuleKind::View, "SELECT customer_id FROM dbo.customer")
            )
            .is_empty()
        );
    }

    /// `GO` is a client instruction, not T-SQL. Sent to the server it is a
    /// syntax error, and the user who wrote it meant something the emitter
    /// cannot express as one CREATE OR ALTER.
    #[test]
    fn a_batch_separator_inside_a_definition_is_refused() {
        let e = module_errors(
            "dbo.v",
            &a_module(ModuleKind::View, "SELECT 1\nGO\nSELECT 2"),
        );
        assert!(e.contains("GO"), "{e}");
    }

    /// An encrypted module cannot be read back, so every plan would re-state
    /// it and every drift check would fire.
    #[test]
    fn an_encrypted_module_is_refused() {
        let e = module_errors(
            "dbo.v",
            &a_module(ModuleKind::View, "WITH ENCRYPTION AS SELECT 1"),
        );
        assert!(e.contains("cannot be read back"), "{e}");
    }

    /// `GO` is a client instruction, but the two letters are ordinary text
    /// inside a literal or a comment — and a procedure that returns or
    /// documents a deployment script is a perfectly ordinary thing to manage.
    #[test]
    fn go_inside_a_literal_or_a_comment_is_not_a_batch_separator() {
        for body in [
            "AS SELECT 'first line\nGO\nsecond line' AS script",
            "AS /* the caller runs\nGO\nafterwards */ SELECT 1",
            "AS SELECT 1 -- GO",
        ] {
            let e = module_errors("dbo.p", &a_module(ModuleKind::Procedure, body));
            assert!(!e.contains("batch separator"), "{body}: {e}");
        }
        // A real one is still refused.
        let e = module_errors(
            "dbo.p",
            &a_module(ModuleKind::Procedure, "AS SELECT 1\nGO\nSELECT 2"),
        );
        assert!(e.contains("batch separator"), "{e}");
    }

    /// The option is tokens, not one exact string. A spelling that slipped
    /// through would be applied and then read back as NULL, so the module would
    /// drop out of every snapshot and every later plan would create it again.
    #[test]
    fn encryption_is_recognised_however_it_is_spaced() {
        for body in [
            "WITH  ENCRYPTION AS SELECT 1",
            "WITH\nENCRYPTION AS SELECT 1",
            "WITH\t ENCRYPTION\n AS SELECT 1",
        ] {
            let e = module_errors("dbo.v", &a_module(ModuleKind::View, body));
            assert!(e.contains("cannot be read back"), "{body}: {e}");
        }
        // And a body that merely mentions the words apart is not the option.
        let e = module_errors(
            "dbo.v",
            &a_module(ModuleKind::View, "AS SELECT 'encryption' AS with_note"),
        );
        assert!(!e.contains("cannot be read back"), "{e}");
    }

    #[test]
    fn a_trigger_needs_a_table_and_nothing_else_may_have_one() {
        let e = module_errors(
            "dbo.trg",
            &a_module(ModuleKind::Trigger, "AFTER INSERT AS SELECT 1"),
        );
        assert!(e.contains("which table"), "{e}");

        let mut view = a_module(ModuleKind::View, "SELECT 1");
        view.on = Some("dbo.customer".parse().unwrap());
        let e = module_errors("dbo.v", &view);
        assert!(e.contains("only a trigger"), "{e}");
    }

    #[test]
    fn an_empty_definition_is_refused() {
        let e = module_errors("dbo.v", &a_module(ModuleKind::View, "  \n "));
        assert!(e.contains("empty definition"), "{e}");
    }

    // ---- roles (ADR-0005) ----

    #[test]
    fn a_built_in_role_cannot_be_declared_and_an_ordinary_one_can() {
        let mut role = Role::default();
        role.grants.insert(
            "dbo.customer".parse().unwrap(),
            [pbps_model::Permission::Select].into_iter().collect(),
        );
        assert!(super::role("app_reader", &role).is_empty());
        let errs = super::role("db_datareader", &role);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].to_string().contains("built-in"), "{errs:?}");
        // Case is the engine's, not the file's.
        assert!(!super::role("PUBLIC", &role).is_empty());
        // And a target that cannot be quoted is refused where the name is.
        let mut bad = Role::default();
        bad.grants.insert(
            GrantTarget::Schema("a\0b".into()),
            [pbps_model::Permission::Select].into_iter().collect(),
        );
        assert!(!super::role("ok", &bad).is_empty());
    }
}
