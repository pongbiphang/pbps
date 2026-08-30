//! The T-SQL emitter: the one place in the codebase where SQL is written.
//!
//! # Two rules that shape almost every statement here
//!
//! **`ALTER COLUMN` restates the whole column definition.** There is no way to
//! change only the type or only the nullability, and an omitted `NULL` /
//! `NOT NULL` is read as `NULL` — so a type change written without the
//! nullability silently drops a `NOT NULL`. That is why the change set carries
//! both (see [`pbps_model::Change::AlterColumnType`]).
//!
//! **A default is a constraint, not a column property.** Changing or removing one
//! means dropping a constraint whose name we may not know: a column created
//! outside `pbps` has a server-generated name like `DF__customer__stat__3B75D760`.
//! Those statements are therefore emitted as a small block that looks the name up
//! and drops it through `QUOTENAME`, which is also why they are marked
//! [`Statement::own_batch`] — the block declares a variable, and two of them in
//! one batch would collide on the name.

use pbps_dialect::{DialectError, Statement};
use pbps_model::{
    Change, Column, ForeignKey, Index, PrimaryKey, ReferentialAction, Table, TableName,
    UniqueConstraint,
};

use crate::ident::{literal, quote};
use crate::types::{self, DIALECT};

type Sql = Result<Vec<Statement>, DialectError>;

/// `[schema].[table]`.
fn qualified(t: &TableName) -> Result<String, DialectError> {
    Ok(format!("{}.{}", quote(&t.schema)?, quote(&t.name)?))
}

/// The name `pbps` gives a default constraint it creates.
///
/// SQL Server will invent one if we do not, but an invented name is unstable
/// across environments, and a plan that reads `DROP CONSTRAINT
/// DF__customer__stat__3B75D760` tells its reviewer nothing.
fn default_constraint_name(table: &TableName, column: &str) -> String {
    format!("DF_{}_{}", table.name, column)
}

pub fn emit(change: &Change) -> Sql {
    match change {
        Change::CreateTable { name, table, .. } => create_table(name, table),

        Change::DropTable { name, .. } => one(format!("DROP TABLE {};", qualified(name)?)),

        Change::RenameTable { from, to, .. } => rename_table(from, to),

        Change::AddColumn {
            table,
            name,
            column,
            ..
        } => {
            // Its own batch: a statement later in the same batch that mentions
            // the new column fails to compile, because the batch is parsed
            // before any of it runs.
            Ok(vec![
                Statement::new(format!(
                    "ALTER TABLE {} ADD {};",
                    qualified(table)?,
                    column_definition(table, name, column)?
                ))
                .own_batch(),
            ])
        }

        Change::DropColumn { column, .. } => Ok(vec![
            Statement::new(format!(
                "{}\nALTER TABLE {} DROP COLUMN {};",
                drop_default_block(&column.table, &column.name)?,
                qualified(&column.table)?,
                quote(&column.name)?
            ))
            .own_batch(),
        ]),

        Change::RenameColumn {
            table, from, to, ..
        } => Ok(vec![
            Statement::new(format!(
                "EXEC sp_rename {}, {}, 'COLUMN';",
                literal(&format!("{}.{}", qualified(table)?, quote(from)?)),
                literal(to)
            ))
            .own_batch(),
        ]),

        Change::AlterColumnType {
            column,
            to,
            to_nullable,
            ..
        } => {
            let normalized = types::normalize(to)?;
            one(format!(
                "ALTER TABLE {} ALTER COLUMN {} {} {};",
                qualified(&column.table)?,
                quote(&column.name)?,
                normalized,
                null_clause(*to_nullable)
            ))
        }

        Change::AlterColumnNullability {
            column,
            ty,
            to_nullable,
            ..
        } => {
            let normalized = types::normalize(ty)?;
            one(format!(
                "ALTER TABLE {} ALTER COLUMN {} {} {};",
                qualified(&column.table)?,
                quote(&column.name)?,
                normalized,
                null_clause(*to_nullable)
            ))
        }

        Change::AlterColumnDefault {
            column, from, to, ..
        } => {
            let table = qualified(&column.table)?;
            let mut sql = String::new();
            if from.is_some() {
                sql.push_str(&drop_default_block(&column.table, &column.name)?);
                sql.push('\n');
            }
            if let Some(expr) = to {
                sql.push_str(&format!(
                    "ALTER TABLE {table} ADD CONSTRAINT {} DEFAULT ({expr}) FOR {};",
                    quote(&default_constraint_name(&column.table, &column.name))?,
                    quote(&column.name)?
                ));
            }
            let sql = sql.trim_end().to_owned();
            // Dropping used a variable, so the batch has to stand alone.
            Ok(vec![if from.is_some() {
                Statement::new(sql).own_batch()
            } else {
                Statement::new(sql)
            }])
        }

        // Deprecation is a fact about the declarations, not about the database.
        // Recording it as an extended property is a Phase 5 decision; until then
        // the honest output is nothing at all, rather than a statement that
        // pretends to do something.
        Change::SetColumnDeprecated { .. } => Ok(Vec::new()),

        Change::SetPrimaryKey { table, from, to } => {
            let mut out = Vec::new();
            if let Some(pk) = from {
                out.push(drop_primary_key(table, pk)?);
            }
            if let Some(pk) = to {
                out.push(Statement::new(format!(
                    "ALTER TABLE {} ADD {};",
                    qualified(table)?,
                    primary_key_clause(pk)?
                )));
            }
            Ok(out)
        }

        Change::AddUnique {
            table,
            name,
            constraint,
        } => one(format!(
            "ALTER TABLE {} ADD CONSTRAINT {} UNIQUE ({});",
            qualified(table)?,
            quote(name)?,
            column_list(&constraint.columns)?
        )),

        Change::AddForeignKey {
            table,
            name,
            constraint,
        } => one(format!(
            "ALTER TABLE {} ADD {};",
            qualified(table)?,
            foreign_key_clause(name, constraint)?
        )),

        Change::AddCheck {
            table,
            name,
            constraint,
        } => one(format!(
            "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({});",
            qualified(table)?,
            quote(name)?,
            constraint.expression
        )),

        // All three are the same statement; the risk they carry differs, but the
        // engine has one way to remove a table-level constraint.
        Change::DropUnique { table, name }
        | Change::DropForeignKey { table, name }
        | Change::DropCheck { table, name } => one(format!(
            "ALTER TABLE {} DROP CONSTRAINT {};",
            qualified(table)?,
            quote(name)?
        )),

        Change::AddIndex { table, name, index } => one(create_index(table, name, index)?),

        Change::DropIndex { table, name } => one(format!(
            "DROP INDEX {} ON {};",
            quote(name)?,
            qualified(table)?
        )),
    }
}

fn one(sql: String) -> Sql {
    Ok(vec![Statement::new(sql)])
}

fn null_clause(nullable: bool) -> &'static str {
    if nullable { "NULL" } else { "NOT NULL" }
}

fn column_list(columns: &[String]) -> Result<String, DialectError> {
    Ok(columns
        .iter()
        .map(|c| quote(c))
        .collect::<Result<Vec<_>, _>>()?
        .join(", "))
}

/// One line of a `CREATE TABLE` column list, or the body of an `ALTER TABLE ADD`.
fn column_definition(
    table: &TableName,
    name: &str,
    column: &Column,
) -> Result<String, DialectError> {
    let mut s = format!("{} {}", quote(name)?, types::normalize(&column.ty)?);
    if let Some(id) = column.identity {
        s.push_str(&format!(" IDENTITY({},{})", id.seed, id.increment));
    }
    s.push(' ');
    s.push_str(null_clause(column.nullable));
    if let Some(expr) = &column.default {
        s.push_str(&format!(
            " CONSTRAINT {} DEFAULT ({expr})",
            quote(&default_constraint_name(table, name))?
        ));
    }
    Ok(s)
}

fn primary_key_clause(pk: &PrimaryKey) -> Result<String, DialectError> {
    let cols = column_list(&pk.columns)?;
    Ok(match &pk.name {
        Some(n) => format!("CONSTRAINT {} PRIMARY KEY ({cols})", quote(n)?),
        // Unnamed leaves the server to invent one. That is a real choice a user
        // can make, so it is emitted faithfully rather than named on their behalf.
        None => format!("PRIMARY KEY ({cols})"),
    })
}

fn unique_clause(name: &str, u: &UniqueConstraint) -> Result<String, DialectError> {
    Ok(format!(
        "CONSTRAINT {} UNIQUE ({})",
        quote(name)?,
        column_list(&u.columns)?
    ))
}

fn referential_action(a: ReferentialAction) -> &'static str {
    match a {
        ReferentialAction::NoAction => "NO ACTION",
        ReferentialAction::Cascade => "CASCADE",
        ReferentialAction::SetNull => "SET NULL",
        ReferentialAction::SetDefault => "SET DEFAULT",
    }
}

fn foreign_key_clause(name: &str, fk: &ForeignKey) -> Result<String, DialectError> {
    let mut s = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        quote(name)?,
        column_list(&fk.columns)?,
        qualified(&fk.references_table)?,
        column_list(&fk.references_columns)?
    );
    // NO ACTION is the default, and spelling out a default adds noise to a plan
    // a human has to read at a deployment gate.
    if fk.on_delete != ReferentialAction::NoAction {
        s.push_str(&format!(" ON DELETE {}", referential_action(fk.on_delete)));
    }
    if fk.on_update != ReferentialAction::NoAction {
        s.push_str(&format!(" ON UPDATE {}", referential_action(fk.on_update)));
    }
    Ok(s)
}

fn create_index(table: &TableName, name: &str, index: &Index) -> Result<String, DialectError> {
    let keys = index
        .columns
        .iter()
        .map(|c| {
            Ok(format!(
                "{} {}",
                quote(&c.name)?,
                if c.descending { "DESC" } else { "ASC" }
            ))
        })
        .collect::<Result<Vec<_>, DialectError>>()?
        .join(", ");

    let mut s = format!(
        "CREATE {}INDEX {} ON {} ({keys})",
        if index.unique { "UNIQUE " } else { "" },
        quote(name)?,
        qualified(table)?
    );
    if !index.include.is_empty() {
        s.push_str(&format!(" INCLUDE ({})", column_list(&index.include)?));
    }
    if let Some(filter) = &index.filter {
        s.push_str(&format!(" WHERE ({filter})"));
    }
    s.push(';');
    Ok(s)
}

fn create_table(name: &TableName, table: &Table) -> Sql {
    if table.columns.is_empty() {
        return Err(DialectError::Invalid {
            dialect: DIALECT,
            message: format!("table `{name}` has no columns"),
        });
    }
    let qualified_name = qualified(name)?;

    let mut body: Vec<String> = Vec::new();
    for (col_name, column) in &table.columns {
        body.push(column_definition(name, col_name, column)?);
    }
    // The primary key goes inline; every other constraint is added afterwards,
    // so that creating a table and altering one take the same code path and cannot
    // drift apart.
    if let Some(pk) = &table.primary_key {
        body.push(primary_key_clause(pk)?);
    }

    let mut out = vec![Statement::new(format!(
        "CREATE TABLE {qualified_name} (\n    {}\n);",
        body.join(",\n    ")
    ))];

    for (n, u) in &table.unique {
        out.push(Statement::new(format!(
            "ALTER TABLE {qualified_name} ADD {};",
            unique_clause(n, u)?
        )));
    }
    for (n, c) in &table.checks {
        out.push(Statement::new(format!(
            "ALTER TABLE {qualified_name} ADD CONSTRAINT {} CHECK ({});",
            quote(n)?,
            c.expression
        )));
    }
    for (n, fk) in &table.foreign_keys {
        out.push(Statement::new(format!(
            "ALTER TABLE {qualified_name} ADD {};",
            foreign_key_clause(n, fk)?
        )));
    }
    for (n, idx) in &table.indexes {
        out.push(Statement::new(create_index(name, n, idx)?));
    }
    Ok(out)
}

fn rename_table(from: &TableName, to: &TableName) -> Sql {
    let mut out = Vec::new();
    // `sp_rename` cannot move a table between schemas, and `ALTER SCHEMA
    // TRANSFER` cannot rename it. A rename that does both therefore needs both,
    // in this order: transfer first, then rename inside the new schema.
    let mut current = from.clone();
    if from.schema != to.schema {
        out.push(
            Statement::new(format!(
                "ALTER SCHEMA {} TRANSFER {};",
                quote(&to.schema)?,
                qualified(&current)?
            ))
            .own_batch(),
        );
        current = TableName::new(to.schema.clone(), current.name.clone());
    }
    if current.name != to.name {
        out.push(
            Statement::new(format!(
                "EXEC sp_rename {}, {}, 'OBJECT';",
                // The old name is qualified so the right table is found; the new
                // name must not be, or the schema ends up inside the name.
                literal(&qualified(&current)?),
                literal(&to.name)
            ))
            .own_batch(),
        );
    }
    Ok(out)
}

/// Drops the primary key, looking its name up when the declarations do not
/// carry one.
fn drop_primary_key(table: &TableName, pk: &PrimaryKey) -> Result<Statement, DialectError> {
    let q = qualified(table)?;
    Ok(match &pk.name {
        Some(n) => Statement::new(format!("ALTER TABLE {q} DROP CONSTRAINT {};", quote(n)?)),
        None => Statement::new(format!(
            "DECLARE @pk sysname = (\n    SELECT name FROM sys.key_constraints\n     WHERE parent_object_id = OBJECT_ID({}) AND type = 'PK');\nIF @pk IS NOT NULL EXEC(N'ALTER TABLE {q} DROP CONSTRAINT ' + QUOTENAME(@pk));",
            literal(&q)
        ))
        .own_batch(),
    })
}

/// Drops whatever default constraint the column currently has.
///
/// The name is looked up rather than assumed: only defaults `pbps` created carry
/// a predictable name, and the whole point of `pull` is to adopt databases it did
/// not create. `QUOTENAME` does the quoting, so a server-generated name
/// containing a bracket cannot break out of the dynamic statement.
fn drop_default_block(table: &TableName, column: &str) -> Result<String, DialectError> {
    let q = qualified(table)?;
    Ok(format!(
        "DECLARE @df sysname = (\n    SELECT dc.name FROM sys.default_constraints dc\n      JOIN sys.columns c ON c.object_id = dc.parent_object_id\n                        AND c.column_id = dc.parent_column_id\n     WHERE dc.parent_object_id = OBJECT_ID({}) AND c.name = {});\nIF @df IS NOT NULL EXEC(N'ALTER TABLE {q} DROP CONSTRAINT ' + QUOTENAME(@df));",
        literal(&q),
        literal(column)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{CheckConstraint, ColumnRef, ColumnType, Identity, IndexColumn, Uid};

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }
    fn uid(s: &str) -> Uid {
        s.parse().unwrap()
    }
    fn tname(s: &str) -> TableName {
        s.parse().unwrap()
    }
    fn cref(s: &str) -> ColumnRef {
        s.parse().unwrap()
    }
    fn sql_of(c: &Change) -> Vec<String> {
        emit(c).unwrap().into_iter().map(|s| s.sql).collect()
    }

    #[test]
    fn create_table_lists_columns_in_declaration_order() {
        let mut t = Table::default();
        t.columns
            .insert("id".into(), Column::new(ty("bigint")).not_null());
        t.columns
            .insert("email".into(), Column::new(ty("nvarchar(255)")));
        t.primary_key = Some(PrimaryKey {
            name: Some("pk_customer".into()),
            columns: vec!["id".into()],
        });
        let sql = sql_of(&Change::CreateTable {
            uid: uid("t_k7x2mq"),
            name: tname("dbo.customer"),
            table: Box::new(t),
        });
        assert_eq!(
            sql[0],
            "CREATE TABLE [dbo].[customer] (\n    [id] bigint NOT NULL,\n    [email] nvarchar(255) NULL,\n    CONSTRAINT [pk_customer] PRIMARY KEY ([id])\n);"
        );
    }

    /// Nullability is never implicit: an omitted clause means NULL to the
    /// server, so every column definition must say which it is.
    #[test]
    fn every_column_definition_states_its_nullability() {
        let mut t = Table::default();
        t.columns.insert("a".into(), Column::new(ty("int")));
        t.columns
            .insert("b".into(), Column::new(ty("int")).not_null());
        let sql = sql_of(&Change::CreateTable {
            uid: uid("t_k7x2mq"),
            name: tname("dbo.t"),
            table: Box::new(t),
        });
        assert!(sql[0].contains("[a] int NULL"));
        assert!(sql[0].contains("[b] int NOT NULL"));
    }

    #[test]
    fn identity_and_default_are_rendered_in_the_column() {
        let mut c = Column::new(ty("bigint")).not_null();
        c.identity = Some(Identity {
            seed: 1,
            increment: 1,
        });
        let mut t = Table::default();
        t.columns.insert("id".into(), c);
        let mut status = Column::new(ty("tinyint")).not_null();
        status.default = Some("0".into());
        t.columns.insert("status".into(), status);
        let sql = sql_of(&Change::CreateTable {
            uid: uid("t_k7x2mq"),
            name: tname("dbo.t"),
            table: Box::new(t),
        });
        assert!(sql[0].contains("[id] bigint IDENTITY(1,1) NOT NULL"));
        assert!(
            sql[0].contains("[status] tinyint NOT NULL CONSTRAINT [DF_t_status] DEFAULT (0)"),
            "{}",
            sql[0]
        );
    }

    /// The type in the emitted SQL is the normalized one — `integer` in the YAML
    /// must not leak through into a statement.
    #[test]
    fn emitted_types_are_normalized() {
        let sql = sql_of(&Change::AddColumn {
            uid: uid("c_p3n8vd"),
            table: tname("dbo.t"),
            name: "n".into(),
            column: Box::new(Column::new(ty("integer"))),
        });
        assert_eq!(sql[0], "ALTER TABLE [dbo].[t] ADD [n] int NULL;");
    }

    /// The core of the whole crate: a type change must restate the nullability,
    /// or SQL Server reads the omission as NULL and drops the NOT NULL.
    #[test]
    fn a_type_change_restates_not_null() {
        let sql = sql_of(&Change::AlterColumnType {
            uid: uid("c_k7x2mq"),
            column: cref("dbo.t.amount"),
            from: ty("int"),
            to: ty("bigint"),
            from_nullable: false,
            to_nullable: false,
        });
        assert_eq!(
            sql,
            ["ALTER TABLE [dbo].[t] ALTER COLUMN [amount] bigint NOT NULL;"]
        );
    }

    #[test]
    fn a_nullability_change_restates_the_type() {
        let sql = sql_of(&Change::AlterColumnNullability {
            uid: uid("c_k7x2mq"),
            column: cref("dbo.t.email"),
            ty: ty("nvarchar(255)"),
            to_nullable: false,
        });
        assert_eq!(
            sql,
            ["ALTER TABLE [dbo].[t] ALTER COLUMN [email] nvarchar(255) NOT NULL;"]
        );
    }

    #[test]
    fn renames_go_through_sp_rename() {
        let sql = sql_of(&Change::RenameColumn {
            uid: uid("c_k7x2mq"),
            table: tname("dbo.customer"),
            from: "customer_name".into(),
            to: "full_name".into(),
        });
        assert_eq!(
            sql,
            ["EXEC sp_rename N'[dbo].[customer].[customer_name]', N'full_name', 'COLUMN';"]
        );
    }

    /// The new name must be bare: qualifying it makes SQL Server store the
    /// qualification as part of the name.
    #[test]
    fn the_new_name_in_a_table_rename_is_not_qualified() {
        let sql = sql_of(&Change::RenameTable {
            uid: uid("t_k7x2mq"),
            from: tname("dbo.old_name"),
            to: tname("dbo.new_name"),
        });
        assert_eq!(
            sql,
            ["EXEC sp_rename N'[dbo].[old_name]', N'new_name', 'OBJECT';"]
        );
    }

    /// Moving schema and renaming are different operations in T-SQL, so a rename
    /// that does both needs both, transfer first.
    #[test]
    fn a_cross_schema_rename_transfers_then_renames() {
        let sql = sql_of(&Change::RenameTable {
            uid: uid("t_k7x2mq"),
            from: tname("dbo.old_name"),
            to: tname("app.new_name"),
        });
        assert_eq!(
            sql,
            [
                "ALTER SCHEMA [app] TRANSFER [dbo].[old_name];",
                "EXEC sp_rename N'[app].[old_name]', N'new_name', 'OBJECT';"
            ]
        );
    }

    /// A column drop must clear its default constraint first, and the name is
    /// looked up because a column pbps did not create carries a generated one.
    #[test]
    fn dropping_a_column_clears_its_default_first() {
        let sql = sql_of(&Change::DropColumn {
            uid: uid("c_k7x2mq"),
            column: cref("dbo.t.legacy"),
        });
        assert!(sql[0].contains("sys.default_constraints"), "{}", sql[0]);
        assert!(sql[0].contains("QUOTENAME(@df)"), "{}", sql[0]);
        assert!(
            sql[0].ends_with("ALTER TABLE [dbo].[t] DROP COLUMN [legacy];"),
            "{}",
            sql[0]
        );
    }

    #[test]
    fn changing_a_default_drops_the_old_one_and_adds_a_named_one() {
        let sql = sql_of(&Change::AlterColumnDefault {
            uid: uid("c_k7x2mq"),
            column: cref("dbo.t.status"),
            from: Some("0".into()),
            to: Some("1".into()),
        });
        assert!(sql[0].contains("sys.default_constraints"));
        assert!(sql[0].contains("ADD CONSTRAINT [DF_t_status] DEFAULT (1) FOR [status]"));

        let add_only = sql_of(&Change::AlterColumnDefault {
            uid: uid("c_k7x2mq"),
            column: cref("dbo.t.status"),
            from: None,
            to: Some("1".into()),
        });
        assert!(
            !add_only[0].contains("sys.default_constraints"),
            "nothing to drop"
        );
    }

    #[test]
    fn deprecation_emits_nothing() {
        assert!(
            sql_of(&Change::SetColumnDeprecated {
                uid: uid("c_k7x2mq"),
                column: cref("dbo.t.x"),
                reason: Some("gone".into()),
            })
            .is_empty()
        );
    }

    /// An unnamed primary key has a server-generated name, so dropping it has to
    /// look the name up rather than guess.
    #[test]
    fn dropping_an_unnamed_primary_key_looks_the_name_up() {
        let sql = sql_of(&Change::SetPrimaryKey {
            table: tname("dbo.t"),
            from: Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
            }),
            to: None,
        });
        assert!(sql[0].contains("sys.key_constraints"), "{}", sql[0]);

        let named = sql_of(&Change::SetPrimaryKey {
            table: tname("dbo.t"),
            from: Some(PrimaryKey {
                name: Some("pk_t".into()),
                columns: vec!["id".into()],
            }),
            to: None,
        });
        assert_eq!(named, ["ALTER TABLE [dbo].[t] DROP CONSTRAINT [pk_t];"]);
    }

    #[test]
    fn indexes_render_direction_include_and_filter() {
        let sql = sql_of(&Change::AddIndex {
            table: tname("dbo.t"),
            name: "ix_t_a".into(),
            index: Box::new(Index {
                columns: vec![
                    IndexColumn {
                        name: "a".into(),
                        descending: false,
                    },
                    IndexColumn {
                        name: "b".into(),
                        descending: true,
                    },
                ],
                include: vec!["c".into()],
                unique: true,
                filter: Some("a IS NOT NULL".into()),
            }),
        });
        assert_eq!(
            sql,
            [
                "CREATE UNIQUE INDEX [ix_t_a] ON [dbo].[t] ([a] ASC, [b] DESC) INCLUDE ([c]) WHERE (a IS NOT NULL);"
            ]
        );
    }

    #[test]
    fn check_constraints_render_their_expression_verbatim() {
        let sql = sql_of(&Change::AddCheck {
            table: tname("dbo.t"),
            name: "ck_positive".into(),
            constraint: CheckConstraint {
                expression: "amount > 0".into(),
            },
        });
        assert_eq!(
            sql,
            ["ALTER TABLE [dbo].[t] ADD CONSTRAINT [ck_positive] CHECK (amount > 0);"]
        );
    }

    /// A hostile identifier must come out bracket-quoted, never able to end the
    /// statement early.
    #[test]
    fn identifiers_cannot_break_out_of_their_quoting() {
        let sql = sql_of(&Change::DropTable {
            uid: uid("t_k7x2mq"),
            name: TableName::new("dbo", "x]; DROP TABLE users; --"),
        });
        assert_eq!(sql, ["DROP TABLE [dbo].[x]]; DROP TABLE users; --];"]);
    }

    #[test]
    fn statements_that_declare_variables_get_their_own_batch() {
        let stmts = emit(&Change::DropColumn {
            uid: uid("c_k7x2mq"),
            column: cref("dbo.t.legacy"),
        })
        .unwrap();
        assert!(stmts[0].own_batch);
    }

    #[test]
    fn a_table_with_no_columns_is_refused() {
        let r = emit(&Change::CreateTable {
            uid: uid("t_k7x2mq"),
            name: tname("dbo.empty"),
            table: Box::new(Table::default()),
        });
        assert!(r.is_err());
    }
}
