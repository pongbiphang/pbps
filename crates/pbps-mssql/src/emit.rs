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

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::{DialectError, Statement};
use pbps_model::{
    Cell, Change, Column, ForeignKey, GrantTarget, Index, Module, ModuleKind, ObjectName,
    Permission, PrimaryKey, ReferentialAction, Row, RowKey, Strategy, Table, TableName,
    UniqueConstraint, Value,
};

use crate::ident::{literal, quote};
use crate::types::{self, DIALECT};

type Sql = Result<Vec<Statement>, DialectError>;

/// `[schema].[table]`.
pub(crate) fn qualified(t: &TableName) -> Result<String, DialectError> {
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

/// The `WITH (ONLINE = ON)` suffix, where the statement takes one.
///
/// # Why an unsupported edition is not caught here
///
/// ONLINE index operations are Enterprise-only, and the emitter is offline: it
/// cannot know which edition this plan will meet. `plan --db` reads
/// `SERVERPROPERTY('Edition')` and refuses there (see [`crate::edition`]),
/// where the answer is a fact rather than a guess (ADR-0003 decision 3).
fn online(strategy: Strategy) -> &'static str {
    if strategy.online {
        " WITH (ONLINE = ON)"
    } else {
        ""
    }
}

/// Whether this change's statements would actually carry `WITH (ONLINE = ON)`.
///
/// # Why this is asked of the emitter rather than listed
///
/// Only some statements take the clause: a UNIQUE constraint is index-backed
/// and does, a foreign key and a check are metadata only and it is a syntax
/// error there. A second list of change kinds saying so would be a copy of
/// knowledge that lives above, and the two would drift — with the cost falling
/// on [`crate::edition::online_not_supported`], which would refuse a plan the
/// server would have run happily. Emitting is pure and cheap, so it answers for
/// itself.
///
/// A change the emitter cannot express is not online: it will fail the plan for
/// its own reasons, with its own error.
///
/// The two emissions are compared rather than the text searched. Searching
/// would read the user's own SQL — a column default of `'ONLINE = ON'`, a check
/// comparing against that string — and report an online clause the statement
/// does not have, refusing a valid plan on Standard edition. What differs
/// between the two emissions is exactly what the strategy added, and nothing
/// else can get into that difference.
pub fn takes_online(change: &Change) -> bool {
    let sql = |online| {
        emit(change, Strategy { online }).map(|stmts| {
            stmts
                .into_iter()
                .map(|s| s.sql)
                .collect::<Vec<_>>()
                .join("\n")
        })
    };
    match (sql(true), sql(false)) {
        (Ok(with), Ok(without)) => with != without,
        _ => false,
    }
}

pub fn emit(change: &Change, strategy: Strategy) -> Sql {
    match change {
        Change::CreateTable { name, table, .. } => create_table(name, table),

        Change::DropTable { name, .. } => one(format!("DROP TABLE {};", qualified(name)?)),

        // Reference data (ADR-0004). The only DML this tool emits, and it
        // reaches here only for a table that declared a `data:` block.
        Change::InsertRow {
            table,
            key_column,
            identity_key,
            key,
            row,
        } => insert_row(table, key_column, *identity_key, key, row),

        Change::UpdateRow {
            table,
            key_column,
            key,
            columns,
        } => update_row(table, key_column, key, columns),

        Change::DeleteRow {
            table,
            key_column,
            key,
            ..
        } => one(format!(
            "DELETE FROM {} WHERE {} = {};",
            qualified(table)?,
            quote(key_column)?,
            row_key(key)
        )),

        // The mode is a property of the declaration, not of the database: it
        // decides what future plans do about undeclared rows. The row changes
        // it implies are separate entries in this same plan.
        Change::SetDataMode { .. } => Ok(Vec::new()),

        // Roles (ADR-0005). `ALTER ROLE ... WITH NAME` keeps the membership,
        // which is the reason a role rename is intent rather than drop + add.
        Change::CreateRole { name, .. } => one(format!("CREATE ROLE {};", quote(name)?)),
        Change::DropRole { name, .. } => one(format!("DROP ROLE {};", quote(name)?)),
        Change::RenameRole { from, to, .. } => one(format!(
            "ALTER ROLE {} WITH NAME = {};",
            quote(from)?,
            quote(to)?
        )),
        Change::Grant {
            role,
            target,
            permissions,
        } => one(format!(
            "GRANT {} ON {} TO {};",
            permission_list(permissions),
            securable(target)?,
            quote(role)?
        )),
        Change::Revoke {
            role,
            target,
            permissions,
        } => one(format!(
            "REVOKE {} ON {} FROM {};",
            permission_list(permissions),
            securable(target)?,
            quote(role)?
        )),

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
                "ALTER TABLE {} ALTER COLUMN {} {} {}{};",
                qualified(&column.table)?,
                quote(&column.name)?,
                normalized,
                null_clause(*to_nullable),
                online(strategy)
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
                "ALTER TABLE {} ALTER COLUMN {} {} {}{};",
                qualified(&column.table)?,
                quote(&column.name)?,
                normalized,
                null_clause(*to_nullable),
                online(strategy)
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
                    "ALTER TABLE {} ADD {}{};",
                    qualified(table)?,
                    primary_key_clause(pk)?,
                    online(strategy)
                )));
            }
            Ok(out)
        }

        Change::AddUnique {
            table,
            name,
            constraint,
        } => one(format!(
            "ALTER TABLE {} ADD CONSTRAINT {} UNIQUE ({}){};",
            qualified(table)?,
            quote(name)?,
            column_list(&constraint.columns)?,
            online(strategy)
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

        // No ONLINE clause on any of the three, for two different reasons. A
        // foreign key and a check are metadata only, where the clause is a
        // syntax error rather than a no-op. A UNIQUE constraint *is* backed by
        // an index, but dropping one takes the option only when that index is
        // clustered — and this emitter writes no CLUSTERED, so every constraint
        // it creates is nonclustered and the statement would be rejected even on
        // Enterprise. Building one online is a different matter: see AddUnique.
        Change::DropUnique { table, name }
        | Change::DropForeignKey { table, name }
        | Change::DropCheck { table, name } => one(format!(
            "ALTER TABLE {} DROP CONSTRAINT {};",
            qualified(table)?,
            quote(name)?
        )),

        Change::AddIndex { table, name, index } => one(create_index(table, name, index, strategy)?),

        // No ONLINE clause, deliberately. SQL Server accepts `WITH (ONLINE =
        // ON)` on a drop only for a **clustered** index, where the drop rebuilds
        // the table as a heap and there is something to do online; every index
        // this emitter creates is nonclustered (introspection excludes clustered
        // ones), so the clause would be rejected even on Enterprise. Dropping a
        // nonclustered index is metadata anyway, which is why nothing is lost.
        Change::DropIndex { table, name } => one(format!(
            "DROP INDEX {} ON {};",
            quote(name)?,
            qualified(table)?
        )),

        // `CREATE OR ALTER` (2016 SP1+) rather than drop + create, and not only
        // because it is idempotent: it **preserves the permissions** granted on
        // the object, which drop + create silently destroys (ADR-0002).
        Change::CreateModule { name, module } | Change::AlterModule { name, module } => Ok(vec![
            Statement::new(module_definition(name, module)?).own_batch(),
        ]),

        Change::DropModule { name, kind } => {
            one(format!("DROP {} {};", keyword(*kind), qualified(name)?))
        }
    }
}

/// The T-SQL keyword for a module kind.
const fn keyword(kind: ModuleKind) -> &'static str {
    match kind {
        ModuleKind::View => "VIEW",
        ModuleKind::Procedure => "PROCEDURE",
        ModuleKind::Function => "FUNCTION",
        ModuleKind::Trigger => "TRIGGER",
    }
}

/// The whole `CREATE OR ALTER` statement for a module.
///
/// The emitter composes the prefix and the declaration holds the body, so SQL
/// still appears exactly once here — and the text this produces is the text the
/// engine stores verbatim in `sys.sql_modules`, which is what makes the
/// round trip of [`crate::introspect::split_module`] exact for anything pbps
/// wrote (ADR-0002).
///
/// It is [`Statement::own_batch`] because T-SQL requires it: `CREATE VIEW`,
/// `CREATE PROCEDURE`, `CREATE FUNCTION` and `CREATE TRIGGER` must each be the
/// only statement in their batch.
pub fn module_definition(name: &ObjectName, module: &Module) -> Result<String, DialectError> {
    let body = module.definition.trim();
    if body.is_empty() {
        return Err(DialectError::Invalid {
            dialect: DIALECT,
            message: format!("module `{name}` has an empty definition"),
        });
    }
    let head = format!(
        "CREATE OR ALTER {} {}",
        keyword(module.kind),
        qualified(name)?
    );
    Ok(match module.kind {
        // The `AS` is the emitter's, so a view's definition is just its query —
        // which is what a reader of the declarations wants to see.
        ModuleKind::View => format!("{head}\nAS\n{body}"),
        ModuleKind::Trigger => {
            let on = module.on.as_ref().ok_or_else(|| DialectError::Invalid {
                dialect: DIALECT,
                message: format!("trigger `{name}` does not say which table it is on"),
            })?;
            format!("{head}\nON {}\n{body}", qualified(on)?)
        }
        // A parameter list is part of the object's contract, and modelling
        // T-SQL parameter syntax would mean parsing SQL. So everything after
        // the name is the user's.
        ModuleKind::Procedure | ModuleKind::Function => format!("{head}\n{body}"),
    })
}

/// One cell as a T-SQL literal.
///
/// The literal is rendered from what was *written*, never from the column's
/// type — the emitter is handed a change and nothing else, which is the same
/// reason a saved plan can be applied on a host with no checkout. The engine's
/// implicit conversion is what places the value in the column: `N'1.50'` into a
/// `decimal(5,2)` and `1` into a `varchar` both do the right thing, and a
/// value that genuinely cannot convert is rejected by the server inside the
/// plan's transaction, which rolls the whole plan back.
///
/// Text goes out as an `N` literal so that a label outside the code page
/// survives; it converts down to `varchar` without complaint.
fn value_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_owned(),
        // Never `0`/`1`: SQL Server has no boolean, and which of `bit`,
        // `varchar` or `int` the column is decides what the right spelling is.
        // The engine converts `'true'` into a `bit` correctly, and a `1` into a
        // `varchar` column would silently store "1" where the declaration said
        // "true".
        Value::Bool(b) => literal(if *b { "true" } else { "false" }),
        Value::Int(i) => i.to_string(),
        Value::Text(t) => literal(t),
    }
}

/// A row key as a T-SQL literal.
///
/// Always a string literal, because [`RowKey`] is always text (it is a map key,
/// and JSON has no others). An `int` primary key therefore gets `= N'7'`, which
/// the engine converts to the integer — the comparison is correct, and at
/// reference-data size the lost index seek is not a cost anyone can measure.
fn row_key(key: &RowKey) -> String {
    literal(key.as_str())
}

/// One `INSERT`, naming the key column explicitly.
///
/// The column list is always written out. An `INSERT` without one depends on
/// the table's column order, which is exactly what a later `AddColumn` changes
/// — a plan saved today would then insert into the wrong columns.
///
/// An `IDENTITY` key is pinned by wrapping the insert in `SET IDENTITY_INSERT
/// ... ON` / `OFF` (ADR-0004). The three go out as **one statement** and the
/// switch is turned off in the same one: it is a session setting, at most one
/// table may hold it at a time, and a plan that left it on would make the
/// next table's insert fail with an error about a table it never mentioned.
fn insert_row(
    table: &TableName,
    key_column: &str,
    identity_key: bool,
    key: &RowKey,
    row: &Row,
) -> Sql {
    let mut columns = vec![quote(key_column)?];
    let mut values = vec![row_key(key)];
    for (column, v) in row.columns() {
        columns.push(quote(column)?);
        values.push(value_literal(v));
    }
    let table = qualified(table)?;
    let insert = format!(
        "INSERT INTO {table} ({}) VALUES ({});",
        columns.join(", "),
        values.join(", ")
    );
    if identity_key {
        one(format!(
            "SET IDENTITY_INSERT {table} ON;\n{insert}\nSET IDENTITY_INSERT {table} OFF;"
        ))
    } else {
        one(insert)
    }
}

fn update_row(
    table: &TableName,
    key_column: &str,
    key: &RowKey,
    columns: &BTreeMap<String, (Cell, Cell)>,
) -> Sql {
    let mut sets = Vec::with_capacity(columns.len());
    for (column, (_, to)) in columns {
        // `DEFAULT` is the keyword: it asks the engine to evaluate the
        // column's default, which is the one thing a literal cannot say.
        let rhs = match to {
            Cell::Value(v) => value_literal(v),
            Cell::Default(_) => "DEFAULT".to_owned(),
        };
        sets.push(format!("{} = {}", quote(column)?, rhs));
    }
    // An empty SET is not valid T-SQL, and the differ never produces one — it
    // emits an `UpdateRow` only for columns that differ. Refusing rather than
    // writing `UPDATE t SET WHERE ...` keeps that guarantee checkable.
    if sets.is_empty() {
        return Err(DialectError::Invalid {
            dialect: DIALECT,
            message: format!("{table}: a row update with no changed column"),
        });
    }
    one(format!(
        "UPDATE {} SET {} WHERE {} = {};",
        qualified(table)?,
        sets.join(", "),
        quote(key_column)?,
        row_key(key)
    ))
}

fn one(sql: String) -> Sql {
    Ok(vec![Statement::new(sql)])
}

/// A grant target as T-SQL spells a securable: `OBJECT::[s].[o]` or
/// `SCHEMA::[s]`. The class is written out even for an object, where the
/// engine would accept the bare name, so a reader never has to guess which
/// kind of thing a permission landed on.
fn securable(target: &GrantTarget) -> Result<String, DialectError> {
    Ok(match target {
        GrantTarget::Object(o) => format!("OBJECT::{}", qualified(o)?),
        GrantTarget::Schema(s) => format!("SCHEMA::{}", quote(s)?),
    })
}

/// The permission names as the engine spells them, in the model's order.
fn permission_list(permissions: &BTreeSet<Permission>) -> String {
    permissions
        .iter()
        .map(|p| permission_sql(*p))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn permission_sql(p: Permission) -> &'static str {
    match p {
        Permission::Select => "SELECT",
        Permission::Insert => "INSERT",
        Permission::Update => "UPDATE",
        Permission::Delete => "DELETE",
        Permission::References => "REFERENCES",
        Permission::Execute => "EXECUTE",
        Permission::Alter => "ALTER",
        Permission::ViewDefinition => "VIEW DEFINITION",
    }
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

fn create_index(
    table: &TableName,
    name: &str,
    index: &Index,
    strategy: Strategy,
) -> Result<String, DialectError> {
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
    s.push_str(online(strategy));
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
        // No ONLINE here: the table was created by the statement above it and
        // holds no rows, so there is nothing for an online build to spare.
        out.push(Statement::new(create_index(
            name,
            n,
            idx,
            Strategy::default(),
        )?));
    }
    Ok(out)
}

fn rename_table(from: &TableName, to: &TableName) -> Sql {
    let mut out = Vec::new();
    // `sp_rename` cannot move a table between schemas, and `ALTER SCHEMA
    // TRANSFER` cannot rename it. A rename that does both therefore needs both,
    // in this order: transfer first, then rename inside the new schema.
    // Each statement declares what it does to the name (`Statement::renaming`).
    // Between the two the table is at `[new schema].[old name]`, which is in
    // neither the baseline nor the plan — and a staged apply checkpoints there.
    let mut current = from.clone();
    if from.schema != to.schema {
        let moved = TableName::new(to.schema.clone(), current.name.clone());
        out.push(
            Statement::new(format!(
                "ALTER SCHEMA {} TRANSFER {};",
                quote(&to.schema)?,
                qualified(&current)?
            ))
            .own_batch()
            .renaming(current.clone(), moved.clone()),
        );
        current = moved;
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
            .own_batch()
            .renaming(current.clone(), to.clone()),
        );
    }
    Ok(out)
}

/// Drops the primary key, looking its name up when the declarations do not
/// carry one.
/// No ONLINE clause: the option is accepted on a constraint drop only when the
/// constraint's index is clustered, and the model does not record clusteredness
/// — [`crate::introspect`] does not read it back, so a primary key adopted as
/// `NONCLUSTERED` is indistinguishable here from a clustered one. Emitting the
/// hint on a guess would produce a statement the server rejects outright, which
/// is worse than an offline drop of a key that was going to be rebuilt anyway.
fn drop_primary_key(table: &TableName, pk: &PrimaryKey) -> Result<Statement, DialectError> {
    let q = qualified(table)?;
    Ok(match &pk.name {
        Some(n) => Statement::new(format!("ALTER TABLE {q} DROP CONSTRAINT {};", quote(n)?)),
        None => Statement::new(format!(
            "DECLARE @pk sysname = (\n    SELECT name FROM sys.key_constraints\n     WHERE parent_object_id = OBJECT_ID({}) AND type = 'PK');\nIF @pk IS NOT NULL\nBEGIN\n    DECLARE @sql nvarchar(max) = N'ALTER TABLE {q} DROP CONSTRAINT ' + QUOTENAME(@pk);\n    EXEC(@sql);\nEND",
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
        "DECLARE @df sysname = (\n    SELECT dc.name FROM sys.default_constraints dc\n      JOIN sys.columns c ON c.object_id = dc.parent_object_id\n                        AND c.column_id = dc.parent_column_id\n     WHERE dc.parent_object_id = OBJECT_ID({}) AND c.name = {});\nIF @df IS NOT NULL\nBEGIN\n    DECLARE @sql nvarchar(max) = N'ALTER TABLE {q} DROP CONSTRAINT ' + QUOTENAME(@df);\n    EXEC(@sql);\nEND",
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
        emit(c, Strategy::default())
            .unwrap()
            .into_iter()
            .map(|s| s.sql)
            .collect()
    }

    fn online_sql_of(c: &Change) -> Vec<String> {
        emit(c, Strategy { online: true })
            .unwrap()
            .into_iter()
            .map(|s| s.sql)
            .collect()
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

    /// Each statement declares what it does to the name, so nothing downstream
    /// has to re-derive the emitter's statement order. A staged apply
    /// checkpoints between these two, and `[app].[old_name]` is the only name
    /// under which the table can be found at that moment.
    #[test]
    fn a_cross_schema_rename_declares_both_halves_of_the_move() {
        let stmts = emit(
            &Change::RenameTable {
                uid: uid("t_k7x2mq"),
                from: tname("dbo.old_name"),
                to: tname("app.new_name"),
            },
            Strategy::default(),
        )
        .expect("emit");
        assert_eq!(
            stmts[0].renames,
            [(tname("dbo.old_name"), tname("app.old_name"))]
        );
        assert_eq!(
            stmts[1].renames,
            [(tname("app.old_name"), tname("app.new_name"))]
        );

        // A rename within one schema is one statement and has no intermediate.
        let stmts = emit(
            &Change::RenameTable {
                uid: uid("t_k7x2mq"),
                from: tname("dbo.old_name"),
                to: tname("dbo.new_name"),
            },
            Strategy::default(),
        )
        .expect("emit");
        assert_eq!(stmts.len(), 1);
        assert_eq!(
            stmts[0].renames,
            [(tname("dbo.old_name"), tname("dbo.new_name"))]
        );

        // And a statement that renames nothing says nothing.
        assert!(
            emit(&an_index(), Strategy::default()).expect("emit")[0]
                .renames
                .is_empty()
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
        let stmts = emit(
            &Change::DropColumn {
                uid: uid("c_k7x2mq"),
                column: cref("dbo.t.legacy"),
            },
            Strategy::default(),
        )
        .unwrap();
        assert!(stmts[0].own_batch);
    }

    #[test]
    fn a_table_with_no_columns_is_refused() {
        let r = emit(
            &Change::CreateTable {
                uid: uid("t_k7x2mq"),
                name: tname("dbo.empty"),
                table: Box::new(Table::default()),
            },
            Strategy::default(),
        );
        assert!(r.is_err());
    }

    // ---- strategy: online (ADR-0003) ----

    fn an_index() -> Change {
        Change::AddIndex {
            table: tname("dbo.order_line"),
            name: "ix_order_line_order".into(),
            index: Box::new(Index {
                columns: vec![IndexColumn {
                    name: "order_id".into(),
                    descending: false,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
            }),
        }
    }

    /// The hint is what a user wrote to keep a large table in service; if the
    /// emitter dropped it, the tool would report an online rebuild and take an
    /// exclusive lock instead.
    #[test]
    fn online_reaches_the_statements_that_take_it() {
        assert_eq!(
            online_sql_of(&an_index()),
            [
                "CREATE INDEX [ix_order_line_order] ON [dbo].[order_line] ([order_id] ASC) WITH (ONLINE = ON);"
            ]
        );
        assert_eq!(
            online_sql_of(&Change::AlterColumnNullability {
                uid: uid("c_k7x2mq"),
                column: cref("dbo.order_line.note"),
                ty: ty("nvarchar(100)"),
                to_nullable: false,
            }),
            [
                "ALTER TABLE [dbo].[order_line] ALTER COLUMN [note] nvarchar(100) NOT NULL WITH (ONLINE = ON);"
            ]
        );
        assert_eq!(
            online_sql_of(&Change::AddUnique {
                table: tname("dbo.order_line"),
                name: "uq_line".into(),
                constraint: UniqueConstraint {
                    columns: vec!["order_id".into()],
                },
            }),
            [
                "ALTER TABLE [dbo].[order_line] ADD CONSTRAINT [uq_line] UNIQUE ([order_id]) WITH (ONLINE = ON);"
            ]
        );
    }

    /// Whether a change carries the clause is decided by comparing the two
    /// emissions, never by searching the text: a default or a check the user
    /// wrote can contain those very words, and a search would refuse a valid
    /// plan on Standard edition over a string literal.
    #[test]
    fn an_expression_that_mentions_online_is_not_an_online_statement() {
        let mut column = Column::new(ty("nvarchar(20)"));
        column.default = Some("'ONLINE = ON'".into());
        assert!(!takes_online(&Change::AddColumn {
            uid: uid("c_k7x2mq"),
            table: tname("dbo.order_line"),
            name: "note".into(),
            column: Box::new(column),
        }));
        assert!(!takes_online(&Change::AddCheck {
            table: tname("dbo.order_line"),
            name: "ck_note".into(),
            constraint: pbps_model::CheckConstraint {
                expression: "note <> 'ONLINE = ON'".into(),
            },
        }));
        // And a statement that really takes it still says so.
        assert!(takes_online(&an_index()));
    }

    /// `WITH (ONLINE = ON)` is a syntax error on a statement that is metadata
    /// only, so a hint that reached one would turn a performance annotation
    /// into a failed deployment.
    #[test]
    fn online_is_left_off_the_statements_that_cannot_take_it() {
        for change in [
            // The two nonclustered drops belong here too: the option exists on
            // a drop only for a clustered index, and the emitter creates no
            // clustered ones.
            Change::DropUnique {
                table: tname("dbo.order_line"),
                name: "uq_line".into(),
            },
            Change::DropIndex {
                table: tname("dbo.order_line"),
                name: "ix_old".into(),
            },
            Change::DropForeignKey {
                table: tname("dbo.order_line"),
                name: "fk_line_order".into(),
            },
            Change::DropCheck {
                table: tname("dbo.order_line"),
                name: "ck_line_qty".into(),
            },
            Change::AddCheck {
                table: tname("dbo.order_line"),
                name: "ck_line_qty".into(),
                constraint: CheckConstraint {
                    expression: "qty > 0".into(),
                },
            },
        ] {
            for sql in online_sql_of(&change) {
                assert!(!sql.contains("ONLINE"), "{sql}");
            }
        }
    }

    /// A table created by the same plan holds no rows, so an online build would
    /// buy nothing and would only add noise to the plan a human reads.
    #[test]
    fn a_new_tables_indexes_are_not_built_online() {
        let mut t = Table::default();
        t.columns
            .insert("order_id".into(), Column::new(ty("bigint")).not_null());
        t.indexes.insert(
            "ix_new".into(),
            Index {
                columns: vec![IndexColumn {
                    name: "order_id".into(),
                    descending: false,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
            },
        );
        let sql = online_sql_of(&Change::CreateTable {
            uid: uid("t_k7x2mq"),
            name: tname("dbo.order_line"),
            table: Box::new(t),
        });
        assert!(sql.iter().all(|s| !s.contains("ONLINE")), "{sql:?}");
    }

    /// Without the hint nothing changes at all: `strategy:` is opt-in, and a
    /// project that never writes one must get byte-identical SQL.
    #[test]
    fn without_the_hint_the_sql_is_untouched() {
        assert_eq!(
            sql_of(&an_index()),
            ["CREATE INDEX [ix_order_line_order] ON [dbo].[order_line] ([order_id] ASC);"]
        );
    }

    // ---- Reference data (ADR-0004) ----

    fn row(cells: &[(&str, Value)]) -> Row {
        cells
            .iter()
            .map(|(c, v)| ((*c).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn an_insert_names_its_columns_and_leads_with_the_key() {
        let sql = sql_of(&Change::InsertRow {
            table: tname("dbo.order_status"),
            key_column: "code".to_owned(),
            identity_key: false,
            key: RowKey::from("new"),
            row: row(&[("label", Value::Text("New".to_owned()))]),
        });
        assert_eq!(
            sql,
            ["INSERT INTO [dbo].[order_status] ([code], [label]) VALUES (N'new', N'New');"]
        );
    }

    /// An `IDENTITY` key can only be pinned with the switch on, and the switch
    /// must be off again before the next table's insert: it is a session
    /// setting and at most one table may hold it.
    #[test]
    fn an_identity_key_is_pinned_inside_one_statement_that_turns_the_switch_off_again() {
        let sql = sql_of(&Change::InsertRow {
            table: tname("dbo.t"),
            key_column: "id".to_owned(),
            identity_key: true,
            key: RowKey::from("7"),
            row: row(&[("label", Value::Text("Seven".to_owned()))]),
        });
        assert_eq!(
            sql,
            [concat!(
                "SET IDENTITY_INSERT [dbo].[t] ON;\n",
                "INSERT INTO [dbo].[t] ([id], [label]) VALUES (N'7', N'Seven');\n",
                "SET IDENTITY_INSERT [dbo].[t] OFF;"
            )]
        );
    }

    /// The negative case: a key that is not an identity gets no switch, which
    /// would itself be an error on a table with no identity column.
    #[test]
    fn a_plain_key_gets_no_identity_switch() {
        let sql = sql_of(&Change::InsertRow {
            table: tname("dbo.t"),
            key_column: "code".to_owned(),
            identity_key: false,
            key: RowKey::from("a"),
            row: Row::default(),
        });
        assert!(!sql[0].contains("IDENTITY_INSERT"), "{sql:?}");
    }

    /// An `INSERT` without a column list depends on the table's column order,
    /// which a later `AddColumn` changes — a plan saved today would then insert
    /// into the wrong columns.
    #[test]
    fn an_insert_always_writes_the_column_list() {
        let sql = sql_of(&Change::InsertRow {
            table: tname("dbo.t"),
            key_column: "code".to_owned(),
            identity_key: false,
            key: RowKey::from("a"),
            row: Row::default(),
        });
        assert!(sql[0].contains("([code])"), "{sql:?}");
    }

    #[test]
    fn an_update_restates_only_the_changed_columns() {
        let sql = sql_of(&Change::UpdateRow {
            table: tname("dbo.order_status"),
            key_column: "code".to_owned(),
            key: RowKey::from("new"),
            columns: [(
                "label".to_owned(),
                (
                    Cell::Value(Value::Text("New".to_owned())),
                    Cell::Value(Value::Text("Opened".to_owned())),
                ),
            )]
            .into_iter()
            .collect(),
        });
        assert_eq!(
            sql,
            ["UPDATE [dbo].[order_status] SET [label] = N'Opened' WHERE [code] = N'new';"]
        );
    }

    /// An omitted column means the declared default, and only the keyword can
    /// ask the engine for it. Writing NULL instead fails a NOT NULL column that
    /// `validate` had passed, and stores NULL in a nullable one where the
    /// declaration said "the default".
    #[test]
    fn an_update_to_the_default_says_default_not_null() {
        let sql = sql_of(&Change::UpdateRow {
            table: tname("dbo.t"),
            key_column: "code".to_owned(),
            key: RowKey::from("a"),
            columns: [(
                "sort".to_owned(),
                (Cell::Value(Value::Int(3)), Cell::Default("0".to_owned())),
            )]
            .into_iter()
            .collect(),
        });
        assert_eq!(
            sql,
            ["UPDATE [dbo].[t] SET [sort] = DEFAULT WHERE [code] = N'a';"]
        );
        assert!(!sql[0].contains("NULL"), "{sql:?}");
    }

    /// `UPDATE t SET WHERE ...` is not T-SQL. The differ never produces an
    /// empty column set, and refusing here is what makes that guarantee
    /// checkable rather than assumed.
    #[test]
    fn an_update_with_no_changed_column_is_refused() {
        assert!(
            emit(
                &Change::UpdateRow {
                    table: tname("dbo.t"),
                    key_column: "code".to_owned(),
                    key: RowKey::from("a"),
                    columns: BTreeMap::new(),
                },
                Strategy::default()
            )
            .is_err()
        );
    }

    #[test]
    fn a_delete_is_keyed_on_the_primary_key_column() {
        let sql = sql_of(&Change::DeleteRow {
            table: tname("dbo.order_status"),
            key_column: "code".to_owned(),
            key: RowKey::from("old"),
            cause: pbps_model::change::DeleteCause::Undeclared,
        });
        assert_eq!(
            sql,
            ["DELETE FROM [dbo].[order_status] WHERE [code] = N'old';"]
        );
    }

    // ---- roles (ADR-0005) ----

    fn perms(list: &[Permission]) -> BTreeSet<Permission> {
        list.iter().copied().collect()
    }

    #[test]
    fn a_role_is_created_dropped_and_renamed_in_place() {
        let uid: pbps_model::Uid = "r_aaaaaa".parse().unwrap();
        assert_eq!(
            sql_of(&Change::CreateRole {
                uid: uid.clone(),
                name: "app_reader".into()
            }),
            ["CREATE ROLE [app_reader];"]
        );
        assert_eq!(
            sql_of(&Change::DropRole {
                uid: uid.clone(),
                name: "app_reader".into()
            }),
            ["DROP ROLE [app_reader];"]
        );
        // ALTER, never drop + add: the membership has to survive.
        let sql = sql_of(&Change::RenameRole {
            uid,
            from: "reader".into(),
            to: "app_reader".into(),
        });
        assert_eq!(sql, ["ALTER ROLE [reader] WITH NAME = [app_reader];"]);
        assert!(!sql[0].contains("DROP"), "{sql:?}");
    }

    #[test]
    fn grants_name_the_securable_class_and_spell_permissions_the_engines_way() {
        let sql = sql_of(&Change::Grant {
            role: "app_reader".into(),
            target: "dbo.customer".parse().unwrap(),
            permissions: perms(&[Permission::ViewDefinition, Permission::Select]),
        });
        assert_eq!(
            sql,
            ["GRANT SELECT, VIEW DEFINITION ON OBJECT::[dbo].[customer] TO [app_reader];"]
        );
        let sql = sql_of(&Change::Revoke {
            role: "app_reader".into(),
            target: "schema::app".parse().unwrap(),
            permissions: perms(&[Permission::Execute]),
        });
        assert_eq!(sql, ["REVOKE EXECUTE ON SCHEMA::[app] FROM [app_reader];"]);
    }

    /// The same rule identifiers follow: a value can never end its own literal.
    /// If this regresses, reference data becomes an injection point — and it is
    /// the one place in this tool where user *data* is written into SQL text.
    #[test]
    fn a_quote_in_a_value_cannot_escape_the_literal() {
        let sql = sql_of(&Change::InsertRow {
            table: tname("dbo.t"),
            key_column: "code".to_owned(),
            identity_key: false,
            key: RowKey::from("o'brien"),
            row: row(&[(
                "label",
                Value::Text("'); DROP TABLE [dbo].[t]; --".to_owned()),
            )]),
        });
        // Pinned exactly rather than by substring: what matters is that the
        // injected text is *inside* the literal, and only the whole statement
        // shows that. Every quote the value contained is doubled, so none of it
        // closes the literal early and none of it becomes statement text.
        assert_eq!(
            sql,
            [concat!(
                "INSERT INTO [dbo].[t] ([code], [label]) ",
                r"VALUES (N'o''brien', N'''); DROP TABLE [dbo].[t]; --');"
            )]
        );
    }

    /// `Bool` is never `0`/`1`. SQL Server has no boolean, so which spelling is
    /// right depends on the column — and the engine converts `'true'` into a
    /// `bit` correctly, while a bare `1` in a `varchar` column would silently
    /// store "1" where the declaration said "true".
    #[test]
    fn a_boolean_goes_out_as_a_word_and_an_integer_bare() {
        let sql = sql_of(&Change::InsertRow {
            table: tname("dbo.t"),
            key_column: "code".to_owned(),
            identity_key: false,
            key: RowKey::from("a"),
            row: row(&[
                ("flag", Value::Bool(true)),
                ("n", Value::Int(-7)),
                ("nothing", Value::Null),
            ]),
        });
        assert!(sql[0].contains("N'true'"), "{sql:?}");
        assert!(sql[0].contains(", -7,"), "{sql:?}");
        assert!(sql[0].contains("NULL"), "{sql:?}");
        // NULL is the keyword, never the string.
        assert!(!sql[0].contains("N'NULL'"), "{sql:?}");
    }

    /// The mode is a property of the declaration, not of the database: it
    /// decides what *future* plans do about undeclared rows.
    #[test]
    fn setting_the_data_mode_emits_nothing() {
        assert!(
            sql_of(&Change::SetDataMode {
                table: tname("dbo.t"),
                from: Some(pbps_model::DataMode::Ensure),
                to: Some(pbps_model::DataMode::Exact),
            })
            .is_empty()
        );
    }

    /// None of the DML takes `WITH (ONLINE = ON)`; it is a syntax error there.
    #[test]
    fn row_changes_are_never_online() {
        for c in [
            Change::InsertRow {
                table: tname("dbo.t"),
                key_column: "code".to_owned(),
                identity_key: false,
                key: RowKey::from("a"),
                row: Row::default(),
            },
            Change::DeleteRow {
                table: tname("dbo.t"),
                key_column: "code".to_owned(),
                key: RowKey::from("a"),
                cause: pbps_model::change::DeleteCause::Undeclared,
            },
        ] {
            assert!(!takes_online(&c), "{c:?}");
        }
    }
}
