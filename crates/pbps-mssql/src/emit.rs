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

use pbps_dialect::{Created, DialectError, Statement};
use pbps_model::{
    Cell, Change, Column, ColumnType, ForeignKey, GrantTarget, Index, Module, ModuleId, ModuleKind,
    Permission, PrimaryKey, ReferentialAction, Row, RowKey, Strategy, Table, TableName,
    UniqueConstraint, Value,
};

use crate::ident::{MAX_IDENT_CHARS, literal, quote};
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
///
/// # Why it can be shortened, and why it must be
///
/// A table name and a column name may each legally be 128 characters, so
/// `DF_{table}_{column}` can exceed the limit while every name the user wrote
/// is valid. Refusing there refuses a valid plan, naming an identifier that
/// appears nowhere in the user's YAML and that they cannot shorten without
/// renaming their table.
///
/// This name is `pbps`'s own, so its shape is `pbps`'s to choose, and nothing
/// downstream reads it back: [`drop_default_block`] looks the name up from the
/// catalog rather than reconstructing it, precisely because a column adopted
/// through `pull` carries a name `pbps` never chose. A constraint created by
/// an older version keeps whatever name it was given.
fn default_constraint_name(table: &TableName, column: &str) -> String {
    let full = format!("DF_{}_{}", table.name, column);
    if utf16_units(&full) <= MAX_IDENT_CHARS {
        return full;
    }
    shortened_default_constraint_name(table, column)
}

/// An identifier's length in the unit the server measures it in.
///
/// `MAX_IDENT_CHARS` is the number 128; the unit is **UTF-16 code units**, not
/// characters. `sysname` is `nvarchar(128)`, and `nvarchar` counts units — a
/// character above U+FFFF occupies two. This is measured rather than assumed:
/// [`crate::state::truncate_reason`] records an `NVARCHAR(4)` refusing two
/// emoji plus one ASCII letter with Msg 2628.
///
/// Every name here is budgeted in that stricter unit, so a name this module
/// generates is one both [`quote`] and the server accept. Counting characters
/// instead would have made the shortening *worse* than the refusal it
/// replaces: a table and a column of 64 emoji each fit `sysname` on their own,
/// so the joined name reaches the cut, and a 128-character cut of it is about
/// 235 units — accepted by `plan` and refused at `apply`, which moves the
/// failure from the cheap end to the expensive one.
///
/// `quote` applies the same unit to names the *user* wrote, so generated and
/// user-written identifiers meet one measured boundary.
fn utf16_units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The longest prefix of `s` that fits `units`, never splitting a character.
fn cut_to_utf16(s: &str, units: usize) -> String {
    let mut used = 0;
    s.chars()
        .take_while(|ch| {
            used += ch.len_utf16();
            used <= units
        })
        .collect()
}

/// The digest, in hex characters, appended to a shortened name.
///
/// 64 bits rather than the 32 that would do: two truncations colliding would
/// be two constraints asking for one name, which the server refuses at apply
/// with a duplicate-object error — a loud failure, but one produced by a plan
/// that looked fine to its reviewer. The four characters this costs come out
/// of parts that are already truncated.
const DEFAULT_NAME_DIGEST_CHARS: usize = 16;

/// `DF_<table>_<column>_<digest>`, cut to fit and stable across runs.
///
/// The digest is over the **qualified** column, so the two halves of the name
/// that were truncated away still separate two columns that now share a
/// prefix. It is seeded with NUL between the parts: NUL is the one character
/// [`quote`] refuses outright, so no name can contain one and no two different
/// triples can spell the same seed.
fn shortened_default_constraint_name(table: &TableName, column: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(table.schema.as_bytes());
    hasher.update([0]);
    hasher.update(table.name.as_bytes());
    hasher.update([0]);
    hasher.update(column.as_bytes());
    let digest = hasher.finalize();
    let digest: String = digest
        .iter()
        .take(DEFAULT_NAME_DIGEST_CHARS / 2)
        .map(|b| format!("{b:02x}"))
        .collect();

    // "DF_" + table + "_" + column + "_" + digest. The digest and the fixed
    // parts are ASCII, so their unit count is their length.
    let budget = MAX_IDENT_CHARS - "DF_".len() - 2 - DEFAULT_NAME_DIGEST_CHARS;
    let (table_units, column_units) = share(budget, utf16_units(&table.name), utf16_units(column));

    let short_table = cut_to_utf16(&table.name, table_units);
    let short_column = cut_to_utf16(column, column_units);
    format!("DF_{short_table}_{short_column}_{digest}")
}

/// Splits `budget` UTF-16 units between two parts that do not both fit.
///
/// A part shorter than its half is kept whole and lends the surplus to the
/// other, so a 120-character table with a 3-character column keeps 112
/// characters of table rather than being cut to half the budget for no reason.
fn share(budget: usize, first: usize, second: usize) -> (usize, usize) {
    if first + second <= budget {
        return (first, second);
    }
    let half = budget / 2;
    if first <= half {
        (first, budget - first)
    } else if second <= half {
        (budget - second, second)
    } else {
        (half, budget - half)
    }
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
        // The first statement is the one that brings the table into being;
        // it says so, and a staged checkpoint adopts the table from there.
        Change::CreateTable { name, table, .. } => {
            let mut out = create_table(name, table)?;
            if let Some(first) = out.first_mut() {
                first.creates.push(Created::Table(name.clone()));
            }
            Ok(out)
        }

        Change::DropTable { name, .. } => one(format!("DROP TABLE {};", qualified(name)?)),

        // Reference data (ADR-0004). The only DML this tool emits, and it
        // reaches here only for a table that declared a `data:` block.
        Change::InsertRow {
            table,
            key_column,
            identity_key,
            key,
            row,
            defaults,
            types,
        } => insert_row(table, key_column, *identity_key, key, row, defaults, types),

        Change::UpdateRow {
            table,
            key_column,
            key,
            columns,
            unchanged,
            types,
            after_types,
        } => update_row(
            table,
            key_column,
            key,
            columns,
            unchanged,
            types,
            after_types,
        ),

        // The row's content is not carried (the pinned baseline holds it),
        // so the delete holds the row to its existence: a row already gone
        // is a baseline this plan was not reviewed against (DECISIONS 122).
        Change::DeleteRow {
            table,
            key_column,
            key,
            row,
            types,
            after_types,
            ..
        } => {
            // Keyed *and* held to the row the plan recorded. The checksum
            // pins the state only up to the moment `apply` reads it, so a
            // key-only DELETE removes whatever an application session left
            // under that key in between, and `@@ROWCOUNT = 1` calls the loss
            // a success. Each recorded cell is compared the way the read-back
            // rendered it, exactly as an update's precondition does; a cell
            // whose type has no comparison is carried and not held
            // (DECISIONS 143).
            let mut predicates = vec![format!("{} = {}", quote(key_column)?, row_key(key))];
            for (column, cell) in row {
                predicates.extend(recorded_cell(
                    column,
                    cell,
                    Held::of(types.get(column), after_types.get(column)),
                )?);
            }
            one(atomically(&format!(
                // The guard, the delete and the checks after it are one
                // transaction of their own (`atomically`): the range locks the
                // guard takes have to be held through the delete they protect,
                // and a staged apply runs each statement outside a transaction.
                "{}\n\
                 DELETE FROM {} WHERE {};\n\
                 {}\n\
                 {}",
                crate::preflight::still_referenced(table, key_column, key)?,
                qualified(table)?,
                predicates.join(" AND "),
                exactly_one_row(table, key),
                // And the row stayed gone: a trigger that put it back would
                // otherwise be read back and recorded as this plan's result.
                gone_row(table, key, key_column)?
            )))
        }

        // The mode is a property of the declaration, not of the database: it
        // decides what future plans do about undeclared rows. The row changes
        // it implies are separate entries in this same plan.
        Change::SetDataMode { .. } => Ok(Vec::new()),

        // Roles (ADR-0005). `ALTER ROLE ... WITH NAME` keeps the membership,
        // which is the reason a role rename is intent rather than drop + add.
        Change::CreateRole { name, .. } => Ok(vec![
            Statement::new(format!("CREATE ROLE {};", quote(name)?))
                .creating(Created::Role(name.clone())),
        ]),
        // The members go first, each in a statement of its own, and the role
        // last: the engine refuses to drop a role that still has members, and
        // a plan that listed them is a plan the reviewer saw.
        Change::DropRole { name, members, .. } => {
            let mut out = Vec::new();
            for member in members {
                out.push(Statement::new(format!(
                    "ALTER ROLE {} DROP MEMBER {};",
                    quote(name)?,
                    quote(member)?
                )));
            }
            out.push(Statement::new(format!("DROP ROLE {};", quote(name)?)));
            Ok(out)
        }
        // The statement says what it does to the name (`Statement::renaming_role`),
        // as a table rename does, so a staged checkpoint finds the role again.
        Change::RenameRole { from, to, .. } => Ok(vec![
            Statement::new(format!(
                "ALTER ROLE {} WITH NAME = {};",
                quote(from)?,
                quote(to)?
            ))
            .renaming_role(from.clone(), to.clone()),
        ]),
        Change::Grant {
            role,
            target,
            permissions,
        } => one(format!(
            "GRANT {} ON {} TO {};",
            permission_list(permissions)?,
            securable(target)?,
            quote(role)?
        )),
        Change::Revoke {
            role,
            target,
            permissions,
        } => one(format!(
            "REVOKE {} ON {} FROM {};",
            permission_list(permissions)?,
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
                .own_batch()
                .creating(Created::Column(table.clone(), name.clone())),
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
            // `CREATE TABLE` accepts nullable timestamp / rowversion, but the
            // engine refuses every ALTER COLUMN that names either spelling —
            // including one whose only change is nullability (DECISIONS 298).
            if types::alter_column_is_refused(ty) {
                return Err(DialectError::Invalid {
                    dialect: DIALECT,
                    message: format!(
                        "column `{column}` has type `{ty}`, which SQL Server refuses to name in \
                         ALTER COLUMN; its nullability cannot be changed"
                    ),
                });
            }
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
                    "ALTER TABLE {table} ADD CONSTRAINT {} DEFAULT ({}) FOR {};",
                    quote(&default_constraint_name(&column.table, &column.name))?,
                    verbatim(expr),
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
            verbatim(&constraint.expression)
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
        // this emitter creates is nonclustered (introspection adopts no other
        // physical kind), so the clause would be rejected even on Enterprise.
        // Dropping a nonclustered index is metadata anyway, which is why
        // nothing is lost.
        Change::DropIndex { table, name } => one(format!(
            "DROP INDEX {} ON {};",
            quote(name)?,
            qualified(table)?
        )),

        // `CREATE OR ALTER` (2016 SP1+) rather than drop + create, and not only
        // because it is idempotent: it **preserves the permissions** granted on
        // the object, which drop + create silently destroys (ADR-0002).
        Change::CreateModule { id, module } | Change::AlterModule { id, module } => Ok(vec![
            Statement::new(module_definition(id, module)?).own_batch(),
        ]),

        // The object's own name, without a signature: nothing overloads on
        // this engine (ADR-0009 §1), so `DROP FUNCTION app.f` names exactly
        // one object — and a signature in the statement is a syntax error.
        Change::DropModule { id, kind } => one(format!(
            "DROP {} {};",
            keyword(*kind),
            qualified(&id.object_name())?
        )),
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
pub fn module_definition(id: &ModuleId, module: &Module) -> Result<String, DialectError> {
    let body = module.definition.trim();
    if body.is_empty() {
        return Err(DialectError::Invalid {
            dialect: DIALECT,
            message: format!("module `{id}` has an empty definition"),
        });
    }
    let head = format!(
        "CREATE OR ALTER {} {}",
        keyword(module.kind),
        qualified(&id.object_name())?
    );
    Ok(match module.kind {
        // The `AS` is the emitter's, so a view's definition is just its query —
        // which is what a reader of the declarations wants to see.
        ModuleKind::View => format!("{head}\nAS\n{body}"),
        ModuleKind::Trigger => {
            // The table is in the identity now, so a trigger without one is
            // not a module this emitter can be handed: the type says so.
            let on = id.attached_to().ok_or_else(|| DialectError::Invalid {
                dialect: DIALECT,
                message: format!("trigger `{id}` does not say which table it is on"),
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
    defaults: &BTreeMap<String, String>,
    types: &BTreeMap<String, ColumnType>,
) -> Sql {
    let mut columns = vec![quote(key_column)?];
    let mut values = vec![row_key(key)];
    for (column, v) in row.columns() {
        columns.push(quote(column)?);
        values.push(value_literal(v));
    }
    // What the row must hold afterwards: a trigger that deleted it again,
    // or wrote something else, would otherwise be read back and recorded as
    // the plan's own result (`wrote_the_row`). Each cell by the rendering
    // that reads it back, under a binary collation, as an update holds its
    // cells — the column's own collation would call `New` and `new` equal,
    // and a trailing space nothing, and the read-back would then record the
    // rewrite (DECISIONS 137). A plan made before the types travelled
    // compares as the engine compares, which is the check it always had.
    let mut cells = Vec::new();
    for (column, v) in row.columns() {
        let held = recorded_cell(
            column,
            &Cell::Value(v.clone()),
            types.get(column).map(Held::same),
        )?;
        cells.push(match (held, v) {
            (Some(held), _) => held,
            (None, Value::Null) => format!("{} IS NULL", quote(column)?),
            (None, v @ (Value::Bool(_) | Value::Int(_) | Value::Text(_))) => {
                format!("{} = {}", quote(column)?, value_literal(v))
            }
        });
    }
    // And the columns the row left to the table: a trigger rewriting one of
    // those is the same silence, so a *constant* default is compared against
    // itself, and a column the table gives no default is held to the NULL
    // the insert left there (DECISIONS 133, 136). Anything the engine would
    // have to run to answer — `NEWID()`, `NEXT VALUE FOR` — is not asked: it
    // has no value before it runs, and asking would consume a sequence
    // value. `types` names every non-key column, the spelled ones held
    // above; a plan made before it travelled names none, and holds nothing
    // here.
    for (column, ty) in types {
        if row.get(column).is_some() {
            continue;
        }
        cells.extend(match defaults.get(column) {
            Some(default) => defaulted_cell(column, default, Some(Held::same(ty)))?,
            None => Some(format!("{} IS NULL", quote(column)?)),
        });
    }
    let wrote = wrote_the_row(table, key, key_column, &cells)?;
    let table = qualified(table)?;
    let insert = format!(
        "INSERT INTO {table} ({}) VALUES ({});",
        columns.join(", "),
        values.join(", ")
    );
    // The `IDENTITY_INSERT` goes off before the check can throw: it is a
    // session setting, not a transactional one, and a rollback would leave
    // it on for the rest of the connection.
    let body = if identity_key {
        format!(
            "SET IDENTITY_INSERT {table} ON;\n{insert}\nSET IDENTITY_INSERT {table} OFF;\n{wrote}"
        )
    } else {
        format!("{insert}\n{wrote}")
    };
    one(atomically(&body))
}

/// One `UPDATE`, holding the row to what the plan recorded.
///
/// The plan was reviewed against a recorded state, and the checksum pins
/// that state up to the moment `apply` reads it — not to the moment this
/// statement runs. A row changed or deleted in between would be overwritten,
/// or missed with the statement still counting as success, and the read-back
/// would record the result as if the reviewed plan had done it. So each
/// `before` cell the base holds goes into the predicate, compared by the very
/// rendering that read it (`rows::read_expr`, the column's type's), and the
/// statement throws unless exactly one row was updated (DECISIONS 122). A
/// `before` at a default is compared as the read-back compared it, and only
/// where the read-back did — a literal default on a type with `=`; a default
/// the engine would have to run has no value to hold the row to.
///
/// The cells the plan leaves alone are held the same way, before and after:
/// the declaration claims them as much as the changed ones, and an `UPDATE`
/// that checked only what it set would let a trigger rewrite the rest of the
/// row — or a hand edit since the plan was made stand — and have the result
/// read back as the plan's own (DECISIONS 136). They are never restated in
/// `SET`, for the reason `UpdateRow` gives.
///
/// The two checks read a cell by two types, not one. The precondition asks
/// what the *recorded* state holds, so a column that state lacks is held to
/// nothing. The postcondition asks what the row holds once the statement has
/// run, by which time this plan's `AddColumn` and `AlterColumnType` have
/// already run — so every declared cell is held, the added column included
/// (DECISIONS 140).
#[allow(clippy::too_many_arguments)]
fn update_row(
    table: &TableName,
    key_column: &str,
    key: &RowKey,
    columns: &BTreeMap<String, (Cell, Cell)>,
    unchanged: &BTreeMap<String, Cell>,
    types: &BTreeMap<String, ColumnType>,
    after_types: &BTreeMap<String, ColumnType>,
) -> Sql {
    // The type the *precondition* reads a cell by is the one the recorded
    // state holds it in; the type the *postcondition* reads it by is the one
    // the column has once this plan's column changes have run, which sort
    // before the row changes. Two lookups rather than one, so neither check
    // can quietly borrow the other's type (DECISIONS 140).
    // The precondition reads a cell by the type the recorded state held it in
    // *and* the type the column has by the time the `UPDATE` runs, which are
    // two types where this plan retypes the column (DECISIONS 149). The
    // postcondition asks only what the row holds afterwards, which is one.
    let before_ty = |column: &String| Held::of(types.get(column), after_types.get(column));
    let after_ty = |column: &String| {
        after_types
            .get(column)
            .or_else(|| types.get(column))
            .map(Held::same)
    };
    let mut sets = Vec::with_capacity(columns.len());
    let mut recorded = Vec::new();
    for (column, (from, to)) in columns {
        let quoted = quote(column)?;
        // `DEFAULT` is the keyword: it asks the engine to evaluate the
        // column's default, which is the one thing a literal cannot say.
        let rhs = match to {
            Cell::Value(v) => value_literal(v),
            Cell::Default(_) => "DEFAULT".to_owned(),
        };
        sets.push(format!("{quoted} = {rhs}"));
        recorded.extend(recorded_cell(column, from, before_ty(column))?);
    }
    for (column, held) in unchanged {
        recorded.extend(recorded_cell(column, held, before_ty(column))?);
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
    let mut sql = format!(
        "UPDATE {} SET {} WHERE {} = {}",
        qualified(table)?,
        sets.join(", "),
        quote(key_column)?,
        row_key(key)
    );
    for r in &recorded {
        sql.push_str(" AND ");
        sql.push_str(r);
    }
    sql.push_str(";\n");
    sql.push_str(&exactly_one_row(table, key));
    // And what the row holds afterwards: each cell the plan spells and each
    // it leaves alone, compared by the rendering that read it, so a trigger
    // that rewrote the row — or took it away — rolls this statement back
    // instead of being read back as the plan's own result. A cell set to
    // `DEFAULT` is held to that default where it is a constant, exactly as
    // the `before` side is; a column the declaration does not name is the
    // application's business.
    let mut cells = Vec::new();
    for (column, (_, to)) in columns {
        cells.extend(recorded_cell(column, to, after_ty(column))?);
    }
    for (column, held) in unchanged {
        cells.extend(recorded_cell(column, held, after_ty(column))?);
    }
    sql.push('\n');
    sql.push_str(&wrote_the_row(table, key, key_column, &cells)?);
    one(atomically(&sql))
}

/// One cell as a predicate holding the row to it, by the rendering that read
/// it back (DECISIONS 122): a NULL as `IS NULL`, a value under a binary
/// collation so a change of case alone is a change — the drift check
/// compares the recorded text the same way — and a default as
/// `defaulted_cell` holds it.
///
/// A column whose type the caller does not supply holds nothing, NULL
/// included. That is the *precondition*'s case for a column the base state
/// lacks: its `before` is what this plan's `AddColumn` left there — NULL by
/// the differ's convention, but the default on a `NOT NULL` add — not a
/// recorded cell. The postcondition always has a type for a declared column
/// and never takes this path (DECISIONS 140).
fn recorded_cell(
    column: &str,
    cell: &Cell,
    ty: Option<Held<'_>>,
) -> Result<Option<String>, DialectError> {
    let quoted = quote(column)?;
    let Some(ty) = ty else {
        return Ok(None);
    };
    let now = ty.now();
    Ok(match cell {
        // A NULL converts to a NULL whatever the two types are, so the one
        // predicate covers a retyped column as it covers any other.
        Cell::Value(Value::Null) => Some(format!("{quoted} IS NULL")),
        Cell::Value(v) => {
            let recorded = literal(&recorded_text(v));
            let Some(expected) = ty.as_stored(&recorded) else {
                return Ok(None);
            };
            Some(format!(
                "{} = {} COLLATE Latin1_General_BIN2",
                crate::rows::read_expr(&quoted, &now.base),
                expected
            ))
        }
        Cell::Default(d) => defaulted_cell(column, d, Some(ty))?,
    })
}

/// The two types one recorded cell is measured by: `read` is the type whose
/// rendering produced the recorded text, and `now` the type the column has
/// when the statement runs. They are the same type for every column this plan
/// leaves alone, and differ only where it retypes one — whose
/// `AlterColumnType` sorts before every row change (DECISIONS 149).
///
/// A pair rather than two arguments because the two are the same type and
/// transposing them compiles: `read` and `now` the wrong way round would
/// hold a row to the conversion run backwards, which fails on exactly the
/// rows that are *not* stale.
#[derive(Clone, Copy)]
struct Held<'a> {
    read: &'a ColumnType,
    now: Option<&'a ColumnType>,
}

impl<'a> Held<'a> {
    /// Recorded and held by one type: nothing about the column changes here.
    fn same(ty: &'a ColumnType) -> Self {
        Held {
            read: ty,
            now: None,
        }
    }

    /// Recorded by `read`, held by `now` where this plan gives it a different
    /// one. `None` is not "no type" — it is "the same one".
    fn of(read: Option<&'a ColumnType>, now: Option<&'a ColumnType>) -> Option<Self> {
        let read = read?;
        Some(Held {
            read,
            now: now.filter(|n| *n != read),
        })
    }

    /// Whether this plan retypes the column between the read and the write.
    fn retyped(self) -> bool {
        self.now.is_some()
    }

    /// The type the recorded text was rendered in, normalized for spelling.
    fn read(self) -> ColumnType {
        normalized(self.read)
    }

    /// The type the column has when the statement runs.
    fn now(self) -> ColumnType {
        normalized(self.now.unwrap_or(self.read))
    }

    /// Whether a retyped column can be held at all: `xml`, `text` and the
    /// spatial types have no comparison to give, so asking either end for one
    /// would be an error rather than a false answer — the same reason
    /// [`defaulted_cell`] asks first. An unretyped column needs nothing of
    /// its type but the rendering, and never comes here.
    fn comparable(self) -> bool {
        crate::rows::comparable(&self.read().base) && crate::rows::comparable(&self.now().base)
    }

    /// `value`, an expression of the type the cell was recorded in, as the
    /// column holds it now.
    ///
    /// For everything this plan leaves alone that is `value` itself. For a
    /// column it retypes it is the conversion the `AlterColumnType` already
    /// ran, asked of the engine rather than computed here: the tool has no
    /// business knowing that a `decimal(5,2)` holding `1.50` becomes `1`.
    /// `TRY_CONVERT` so that a recorded value the new type cannot hold reads
    /// as "not what the plan recorded" rather than raising Msg 245 from
    /// inside the write.
    fn converted(self, value: &str) -> String {
        match self.retyped() {
            false => value.to_owned(),
            true => format!("TRY_CONVERT({}, {value})", self.now()),
        }
    }

    /// The recorded text as the column stores it now, rendered the way the
    /// read-back renders that column — the right-hand side of the predicate.
    ///
    /// Where nothing was retyped this is the recorded text itself: the text
    /// *is* what the rendering produced. Where the column was retyped, the
    /// text goes back through the old type with the style that wrote it and
    /// then through the conversion above.
    ///
    /// One thing it cannot see, and no predicate could: an edit the
    /// conversion erases. Measured — a cell moved from `1.50` to `1.99`
    /// before a `decimal(5,2)` becomes `int` reads back as `1` either way,
    /// and the column no longer holds what would tell them apart.
    fn as_stored(self, recorded: &str) -> Option<String> {
        if !self.retyped() {
            return Some(recorded.to_owned());
        }
        if !self.comparable() {
            return None;
        }
        let now = self.now();
        Some(crate::rows::read_expr(
            &self.converted(&crate::rows::from_text(recorded, &self.read())),
            &now.base,
        ))
    }
}

/// A type spelled the way this dialect spells it, falling back to the spelling
/// the plan carries. A type the dialect cannot parse has already stopped the
/// plan elsewhere; here it would only cost the comparison.
fn normalized(ty: &ColumnType) -> ColumnType {
    crate::types::normalize(ty).unwrap_or_else(|_| ty.clone())
}

/// A recorded cell as the read-back's text: what `rows::value_of` decoded.
fn recorded_text(v: &Value) -> String {
    match v {
        Value::Text(t) => t.clone(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => (if *b { "1" } else { "0" }).to_owned(),
        Value::Null => "NULL".to_owned(),
    }
}

/// The check after a row's `UPDATE` or `DELETE`: the statement reached one
/// row, the one the plan recorded. In the same batch, because `@@ROWCOUNT`
/// is the last statement's. Measured with an `AFTER` trigger on the table:
/// the count is the statement's own, not the trigger's.
/// The check after a row's `DELETE`: it is still gone once the statement has
/// run. A trigger that reinserted it would otherwise be read back and
/// recorded as this plan's own result (DECISIONS 132).
fn gone_row(table: &TableName, key: &RowKey, key_column: &str) -> Result<String, DialectError> {
    Ok(format!(
        "IF EXISTS (SELECT 1 FROM {} WHERE {} = {})\n  THROW 50000, {}, 1;",
        qualified(table)?,
        quote(key_column)?,
        row_key(key),
        literal(&format!(
            "{table} row `{key}` is back after this plan deleted it — a trigger on the table, \
             or another writer inside it. Nothing was applied."
        ))
    ))
}

/// A row statement and the postconditions it holds itself to, as one
/// transaction of its own.
///
/// A staged apply runs each statement outside a transaction (SPEC §7.5), so
/// a postcondition that merely threw would leave the write it rejected
/// committed. Inside the transactional apply this nests, where `COMMIT` only
/// decrements the count and the outer transaction still decides everything.
/// The `CATCH` rolls back and rethrows, so no staged run is left holding an
/// open transaction (DECISIONS 129).
fn atomically(body: &str) -> String {
    format!(
        "BEGIN TRANSACTION;\n\
         BEGIN TRY\n\
         {body}\n\
         COMMIT TRANSACTION;\n\
         END TRY\n\
         BEGIN CATCH\n\
         IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;\n\
         THROW;\n\
         END CATCH"
    )
}

/// A column left to a default, as a predicate holding it to that default —
/// or nothing, where there is no answer the engine can give without running
/// something.
///
/// The type decides whether the comparison exists at all: `xml`, `text` and
/// the spatial types have no `=`, and asking for one is an error rather than
/// a false answer. A plan made before the types travelled carries none, and
/// checks nothing here — the same as an older plan's `UpdateRow`.
///
/// Both sides are rendered as the read-back renders the column, and compared
/// under a binary collation: the default is first converted to the column's
/// type, so a `'2026-01-01'` default on a `datetime2` column renders as the
/// stored value does, and then `New` against `new` is a difference the
/// column's own collation would have hidden (DECISIONS 137).
fn defaulted_cell(
    column: &str,
    default: &str,
    ty: Option<Held<'_>>,
) -> Result<Option<String>, DialectError> {
    let quoted = quote(column)?;
    let Some(ty) = ty else {
        return Ok(None);
    };
    if !ty.comparable() || !crate::rows::is_constant(default) {
        return Ok(None);
    }
    let (read, now) = (ty.read(), ty.now());
    // A default of `NULL` references nothing and compares to nothing; both
    // halves are spelled so the one predicate covers it.
    Ok(Some(format!(
        "({} = {} COLLATE Latin1_General_BIN2 OR ({quoted} IS NULL AND ({default}) IS NULL))",
        crate::rows::read_expr(&quoted, &now.base),
        // The default converted to the type the column had when the row was
        // written, and then — where this plan retypes it — the way the
        // `ALTER` converted the column itself.
        crate::rows::read_expr(
            &ty.converted(&format!("CONVERT({read}, {default})")),
            &now.base
        ),
    )))
}

/// What a row write holds itself to once it has run: the row is there, and
/// it holds what the plan wrote.
///
/// The engine reporting a successful `INSERT` or `UPDATE` is not the same as
/// the row being what the plan says. An `AFTER` trigger runs inside the
/// statement and may delete the row again or rewrite what it holds, and the
/// apply would then read the result back, record it, and report success —
/// leaving `verify` clean against a state nobody declared and the next
/// connected plan proposing the same change forever. Checked here, inside
/// the write's own transaction, so the plan rolls back instead
/// (DECISIONS 132).
///
/// Only the cells the plan spells are checked, by the rendering that reads
/// them back. A cell left to a default has no value in the plan to hold the
/// row to, and a column the plan never names is the application's business,
/// not this statement's.
///
/// A *connected* plan cannot fail this on spelling alone: `plan --db` refuses
/// a declaration the engine reads back differently before the plan exists
/// (DECISIONS 101). An offline plan carries no such promise, and a value the
/// engine stores differently from the way it is declared stops here rather
/// than being applied, recorded, and proposed again by every plan after it —
/// which is what the message names alongside a trigger.
fn wrote_the_row(
    table: &TableName,
    key: &RowKey,
    key_column: &str,
    cells: &[String],
) -> Result<String, DialectError> {
    let mut predicate = vec![format!("{} = {}", quote(key_column)?, row_key(key))];
    predicate.extend(cells.iter().cloned());
    Ok(format!(
        "IF NOT EXISTS (SELECT 1 FROM {} WHERE {})\n  THROW 50000, {}, 1;",
        qualified(table)?,
        predicate.join(" AND "),
        literal(&format!(
            "{table} row `{key}` is not what this plan wrote once the statement had run — a \
             trigger on the table, another writer inside it, or a value the engine stores \
             differently from the way it is declared. Nothing was applied; `pbps plan --db` \
             says which."
        ))
    ))
}

fn exactly_one_row(table: &TableName, key: &RowKey) -> String {
    format!(
        "IF @@ROWCOUNT <> 1 THROW 50000, {}, 1;",
        literal(&format!(
            "{table} row `{key}` is not as the plan recorded it: changed or deleted since the \
             plan was made. Plan again."
        ))
    )
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
        // Refused rather than written without its arguments: T-SQL has no
        // spelling for one overload, because it has no overloads (ADR-0009
        // §1). `validate::role` says the same thing before a plan exists;
        // this is the emitter's own guard for a plan that arrived some other
        // way.
        GrantTarget::Routine(r) => {
            return Err(DialectError::Invalid {
                dialect: DIALECT,
                message: format!(
                    "`{r}` names an argument list; SQL Server identifies a routine by name alone"
                ),
            });
        }
        GrantTarget::Schema(s) => format!("SCHEMA::{}", quote(s)?),
    })
}

/// The permission names as the engine spells them, in the model's order.
fn permission_list(permissions: &BTreeSet<Permission>) -> Result<String, DialectError> {
    Ok(permissions
        .iter()
        .map(|p| permission_sql(*p))
        .collect::<Result<Vec<_>, _>>()?
        .join(", "))
}

/// The engine's spelling of `p`, or `Unsupported` for a word the model holds
/// for the other engine (ADR-0010 §6): `validate` refuses those first, and
/// this is the second lock on the same door — a statement the engine's parser
/// would stop at (Msg 102) is never rendered, so it cannot be the statement
/// that fails halfway through a staged apply.
pub(crate) fn permission_sql(p: Permission) -> Result<&'static str, DialectError> {
    Ok(match p {
        Permission::Select => "SELECT",
        Permission::Insert => "INSERT",
        Permission::Update => "UPDATE",
        Permission::Delete => "DELETE",
        Permission::References => "REFERENCES",
        Permission::Execute => "EXECUTE",
        Permission::Alter => "ALTER",
        Permission::ViewDefinition => "VIEW DEFINITION",
        Permission::Usage
        | Permission::Create
        | Permission::Truncate
        | Permission::Trigger
        | Permission::Maintain => {
            return Err(DialectError::Unsupported {
                dialect: types::DIALECT,
                feature: format!(
                    "the `{}` permission, which is PostgreSQL's (ADR-0010 §6); this engine takes {}",
                    p.as_str(),
                    crate::validate::permission_words()
                ),
            });
        }
    })
}

fn null_clause(nullable: bool) -> &'static str {
    if nullable { "NULL" } else { "NOT NULL" }
}

/// A declared expression, followed by the newline that closes any comment in
/// it.
///
/// This dialect writes three things verbatim — a column default, a check
/// expression, an index filter (ADR-0013 §3) — and its own syntax follows them
/// on the same line. A line comment at the end of the user's text then takes
/// that syntax away. **Measured** on SQL Server 2025, where the declaration is
/// valid and only the emitted statement is not:
///
/// ```text
/// CREATE TABLE dbo.t (n int, CONSTRAINT ck CHECK (n > 0 -- reason));
///   -> Incorrect syntax near '0'.
/// CREATE TABLE dbo.t (a int CONSTRAINT df DEFAULT (1 -- why), b int);
///   -> Incorrect syntax near '1'.
/// CREATE TABLE dbo.t (a int CONSTRAINT df DEFAULT (1 -- why ⏎ ), b int);
///   -> accepted
/// ```
///
/// The same fix the PostgreSQL emitter took, for the same reason, and it lives
/// in one helper rather than at each site so that the next site has somewhere
/// to reach for (DECISIONS 281). A carriage return would end the comment too,
/// but that direction needs no thought here: the newline is the emitter's own.
fn verbatim(expression: &str) -> String {
    format!("{expression}\n")
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
            " CONSTRAINT {} DEFAULT ({})",
            quote(&default_constraint_name(table, name))?,
            verbatim(expr)
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
        s.push_str(&format!(" WHERE ({})", verbatim(filter)));
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
            verbatim(&c.expression)
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
        // The statement being built is a *string*, so the table name inside it
        // is in literal position and not in code position. `quote` is the
        // wrong tool there: it doubles `]`, and an apostrophe passes through
        // it untouched and closes the literal early. So the whole prefix is
        // handed to `literal`, the way `rows.rs` builds its dynamic statement
        // — which also means the name can never be interpolated raw again.
        None => Statement::new(format!(
            "DECLARE @pk sysname = (\n    SELECT name FROM sys.key_constraints\n     WHERE parent_object_id = OBJECT_ID({}) AND type = 'PK');\nIF @pk IS NOT NULL\nBEGIN\n    DECLARE @sql nvarchar(max) = {} + QUOTENAME(@pk);\n    EXEC(@sql);\nEND",
            literal(&q),
            literal(&format!("ALTER TABLE {q} DROP CONSTRAINT "))
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
        "DECLARE @df sysname = (\n    SELECT dc.name FROM sys.default_constraints dc\n      JOIN sys.columns c ON c.object_id = dc.parent_object_id\n                        AND c.column_id = dc.parent_column_id\n     WHERE dc.parent_object_id = OBJECT_ID({}) AND c.name = {});\nIF @df IS NOT NULL\nBEGIN\n    DECLARE @sql nvarchar(max) = {} + QUOTENAME(@df);\n    EXEC(@sql);\nEND",
        literal(&q),
        literal(column),
        // Literal position, not code position — see `drop_primary_key`.
        literal(&format!("ALTER TABLE {q} DROP CONSTRAINT "))
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
            sql[0].contains("[status] tinyint NOT NULL CONSTRAINT [DF_t_status] DEFAULT (0\n)"),
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
    fn a_timestamp_nullability_change_is_refused_before_sql() {
        for spelling in ["timestamp", "rowversion"] {
            let change = Change::AlterColumnNullability {
                uid: uid("c_k7x2mq"),
                column: cref("dbo.t.version"),
                ty: ty(spelling),
                to_nullable: true,
            };
            let msg = emit(&change, Strategy::default()).unwrap_err().to_string();
            assert!(msg.contains("`dbo.t.version`"), "{spelling}: {msg}");
            assert!(msg.contains(&format!("`{spelling}`")), "{spelling}: {msg}");
            assert!(
                msg.contains("nullability cannot be changed"),
                "{spelling}: {msg}"
            );
        }

        assert_eq!(
            sql_of(&Change::AlterColumnNullability {
                uid: uid("c_k7x2mq"),
                column: cref("dbo.t.version"),
                ty: ty("varbinary(8)"),
                to_nullable: false,
            }),
            ["ALTER TABLE [dbo].[t] ALTER COLUMN [version] varbinary(8) NOT NULL;"]
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

    /// A table name and a column name may each legally be 128 characters, so
    /// the name built from both can exceed the limit while every name the user
    /// wrote is valid. Refusing there refused a valid plan over an identifier
    /// that appears nowhere in their YAML.
    #[test]
    fn a_generated_default_name_too_long_to_quote_is_shortened_not_refused() {
        let long_table = "t".repeat(MAX_IDENT_CHARS);
        let long_column = "c".repeat(MAX_IDENT_CHARS);
        let table = TableName::new("dbo", long_table.clone());

        let name = default_constraint_name(&table, &long_column);
        assert!(name.chars().count() <= MAX_IDENT_CHARS, "{name}");
        // The point of the length check is that `quote` accepts it. Asserting
        // the count alone would pass for a name `quote` still refuses.
        assert!(quote(&name).is_ok(), "{name}");
        assert!(name.starts_with("DF_"), "{name}");

        // Both call sites reach it: the column list of a `CREATE TABLE`, and
        // the `ADD CONSTRAINT` of a default change.
        let mut column = Column::new(ty("int")).not_null();
        column.default = Some("0".into());
        let mut t = Table::default();
        t.columns.insert(long_column.clone(), column);
        let created = sql_of(&Change::CreateTable {
            uid: uid("t_k7x2mq"),
            name: table.clone(),
            table: Box::new(t),
        });
        assert!(created[0].contains(&format!("CONSTRAINT [{name}] DEFAULT (0\n)")));

        let altered = sql_of(&Change::AlterColumnDefault {
            uid: uid("c_k7x2mq"),
            column: ColumnRef::new(table.clone(), long_column.clone()),
            from: Some("0".into()),
            to: Some("1".into()),
        });
        assert!(
            altered[0].contains(&format!("ADD CONSTRAINT [{name}] DEFAULT (1\n) FOR")),
            "{}",
            altered[0]
        );
    }

    /// The limit is the width of `sysname`, and `nvarchar` counts UTF-16 code
    /// units, so a character above U+FFFF costs two. Counting characters made
    /// the shortening *worse* than the refusal it replaces: the cut name was
    /// accepted by `plan` and refused by the server at `apply`, which is the
    /// expensive end to learn it at.
    #[test]
    fn a_shortened_name_fits_the_width_the_server_measures_not_the_character_count() {
        // Each of these is 64 characters and 128 UTF-16 units, so each fits
        // `sysname` on its own and the joined name reaches the cut.
        let table = TableName::new("dbo", "\u{1f600}".repeat(64));
        let column = "\u{1f601}".repeat(64);

        let name = default_constraint_name(&table, &column);
        assert!(quote(&name).is_ok(), "{name}");
        assert!(
            name.encode_utf16().count() <= MAX_IDENT_CHARS,
            "{} units",
            name.encode_utf16().count()
        );
        // The character count alone would have passed here while the name was
        // nearly twice the width the server allows.
        assert!(name.chars().count() <= MAX_IDENT_CHARS, "{name}");

        // The cut never splits a character, so what is left is still the
        // characters it came from.
        assert!(name.starts_with("DF_\u{1f600}"), "{name}");

        // And the entry check is in units too: this one is 69 characters, so
        // a character count would have returned it whole at 133 units.
        let short_column = default_constraint_name(&table, "a");
        assert!(
            short_column.encode_utf16().count() <= MAX_IDENT_CHARS,
            "{} units",
            short_column.encode_utf16().count()
        );
        assert!(
            format!("DF_{}_a", table.name).chars().count() <= MAX_IDENT_CHARS,
            "the character count has to be under the limit, or this proves nothing"
        );
    }

    /// Shortening is only worth having if the name is the same every time and
    /// different for different columns: an unstable name would change the plan
    /// its checksum pins, and a shared one would be two constraints asking for
    /// one name, refused at apply.
    #[test]
    fn a_shortened_default_name_is_stable_and_distinguishes_what_it_truncates() {
        let table = TableName::new("dbo", "t".repeat(MAX_IDENT_CHARS));
        let first = format!("{}_a", "c".repeat(MAX_IDENT_CHARS - 2));
        let second = format!("{}_b", "c".repeat(MAX_IDENT_CHARS - 2));

        let a = default_constraint_name(&table, &first);
        assert_eq!(a, default_constraint_name(&table, &first));
        // The characters that differ are past the cut, so only the digest
        // separates these two.
        assert_ne!(a, default_constraint_name(&table, &second));

        // The same table name in another schema is another table, and its
        // constraint is another object.
        let elsewhere = TableName::new("app", "t".repeat(MAX_IDENT_CHARS));
        assert_ne!(a, default_constraint_name(&elsewhere, &first));
    }

    /// The shortening must not reach a name that already fits, or every
    /// existing plan would be rewritten by upgrading.
    #[test]
    fn a_generated_default_name_that_fits_is_left_exactly_as_it_was() {
        assert_eq!(
            default_constraint_name(&tname("dbo.customer"), "status"),
            "DF_customer_status"
        );
        // Exactly at the limit: "DF_" + 62 + "_" + 62 == 128.
        let table = TableName::new("dbo", "t".repeat(62));
        let column = "c".repeat(62);
        let name = default_constraint_name(&table, &column);
        assert_eq!(name.chars().count(), MAX_IDENT_CHARS);
        assert_eq!(name, format!("DF_{}_{}", "t".repeat(62), "c".repeat(62)));

        // One more character, and it is cut rather than refused.
        let over = default_constraint_name(&table, &"c".repeat(63));
        assert!(over.chars().count() <= MAX_IDENT_CHARS, "{over}");
        assert!(over.len() < format!("DF_{}_{}", "t".repeat(62), "c".repeat(63)).len());
    }

    /// A part shorter than its half keeps all of itself. Halving both would
    /// throw away 50 characters of a table name to make room for a column
    /// name that is three characters long.
    #[test]
    fn shortening_spends_the_budget_on_the_part_that_needs_it() {
        let table = TableName::new("dbo", "t".repeat(MAX_IDENT_CHARS));
        let name = default_constraint_name(&table, "id");
        assert!(name.chars().count() <= MAX_IDENT_CHARS, "{name}");
        assert!(name.contains("_id_"), "{name}");
        // Far more than half the budget went to the table.
        let kept = name.chars().filter(|c| *c == 't').count();
        assert!(kept > MAX_IDENT_CHARS / 2, "{name}");
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
        assert!(sql[0].contains("ADD CONSTRAINT [DF_t_status] DEFAULT (1\n) FOR [status]"));

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

    /// Every site that writes a declared expression puts the emitter's own
    /// syntax on the next line, so a trailing line comment cannot swallow it.
    /// The declaration is valid — the engine takes the same expression with its
    /// closer on the next line — so without the newline a valid plan produces a
    /// statement that cannot run.
    #[test]
    fn a_line_comment_at_the_end_of_an_expression_keeps_the_syntax_behind_it() {
        // A column default, in a column list: `,` or `)` follows it.
        let mut status = Column::new(ty("tinyint")).not_null();
        status.default = Some("0 -- why".into());
        let mut t = Table::default();
        t.columns.insert("status".into(), status);
        t.checks.insert(
            "ck_positive".into(),
            CheckConstraint {
                expression: "amount > 0 -- reason".into(),
            },
        );
        let created = sql_of(&Change::CreateTable {
            uid: uid("t_k7x2mq"),
            name: tname("dbo.t"),
            table: Box::new(t),
        });
        assert!(
            created[0].contains("DEFAULT (0 -- why\n)"),
            "{}",
            created[0]
        );
        // And a check written in the same `CREATE TABLE`, which is emitted as a
        // statement of its own.
        assert!(
            created
                .iter()
                .any(|s| s.contains("CHECK (amount > 0 -- reason\n);")),
            "{created:?}"
        );

        // A default added on its own: `) FOR [column];` follows it.
        let altered = sql_of(&Change::AlterColumnDefault {
            uid: uid("c_k7x2mq"),
            column: cref("dbo.t.status"),
            from: None,
            to: Some("1 -- why".into()),
        });
        assert!(
            altered[0].contains("DEFAULT (1 -- why\n) FOR [status];"),
            "{}",
            altered[0]
        );

        // A check added on its own.
        let check = sql_of(&Change::AddCheck {
            table: tname("dbo.t"),
            name: "ck_positive".into(),
            constraint: CheckConstraint {
                expression: "amount > 0 -- reason".into(),
            },
        });
        assert_eq!(
            check,
            ["ALTER TABLE [dbo].[t] ADD CONSTRAINT [ck_positive] CHECK (amount > 0 -- reason\n);"]
        );

        // An index filter, which has both the closing parenthesis and whatever
        // `ONLINE` clause the strategy asks for behind it.
        let index = sql_of(&Change::AddIndex {
            table: tname("dbo.t"),
            name: "ix_t_a".into(),
            index: Box::new(Index {
                columns: vec![IndexColumn {
                    name: "a".into(),
                    descending: false,
                }],
                include: Vec::new(),
                unique: false,
                filter: Some("a IS NOT NULL -- only the live ones".into()),
            }),
        });
        assert_eq!(
            index,
            [
                "CREATE INDEX [ix_t_a] ON [dbo].[t] ([a] ASC) WHERE (a IS NOT NULL -- only the live ones\n);"
            ]
        );
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
                "CREATE UNIQUE INDEX [ix_t_a] ON [dbo].[t] ([a] ASC, [b] DESC) INCLUDE ([c]) WHERE (a IS NOT NULL\n);"
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
            ["ALTER TABLE [dbo].[t] ADD CONSTRAINT [ck_positive] CHECK (amount > 0\n);"]
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

    /// The other half of the same property, for the statements that are built
    /// as a *string* and run through `EXEC`. Brackets stop a name from ending
    /// an identifier; they do nothing to stop it ending the literal the
    /// identifier is sitting inside, and an apostrophe is a legal character in
    /// a SQL Server name, so `pull` can adopt one. Both looked-up drops are
    /// checked, because the shape is written out twice.
    #[test]
    fn a_name_with_an_apostrophe_cannot_break_out_of_a_dynamic_statement() {
        let looked_up_pk = sql_of(&Change::SetPrimaryKey {
            table: tname("dbo.o'brien"),
            from: Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
            }),
            to: None,
        });
        let looked_up_default = sql_of(&Change::DropColumn {
            uid: uid("c_k7x2mq"),
            column: cref("dbo.o'brien.legacy"),
        });

        for sql in [&looked_up_pk[0], &looked_up_default[0]] {
            // Every line that is building a string — the `OBJECT_ID` argument
            // and the dynamic statement — carries the doubled spelling, and
            // none of them carries the raw one. Asserting only that the
            // escaped form is present would still pass with a raw copy left
            // beside it, which is the bug that was here.
            let building = sql.lines().filter(|l| l.contains("N'")).collect::<Vec<_>>();
            assert!(!building.is_empty(), "{sql}");
            for line in building {
                assert!(!line.contains("[o'brien]"), "{sql}");
            }
            assert!(
                sql.contains("N'ALTER TABLE [dbo].[o''brien] DROP CONSTRAINT '"),
                "{sql}"
            );
        }

        // The undoubled spelling is not wrong everywhere: the plain
        // `ALTER TABLE` that follows the lookup is code, not a string, and
        // there the name must *not* be doubled. One name, two positions, two
        // spellings — which is why `quote` in literal position was the bug.
        assert!(
            looked_up_default[0].ends_with("ALTER TABLE [dbo].[o'brien] DROP COLUMN [legacy];"),
            "{}",
            looked_up_default[0]
        );
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
            defaults: Default::default(),
            types: Default::default(),
            row: row(&[("label", Value::Text("New".to_owned()))]),
        });
        assert_eq!(sql.len(), 1, "{sql:?}");
        assert!(
            sql[0].contains(
                "INSERT INTO [dbo].[order_status] ([code], [label]) VALUES (N'new', N'New');"
            ),
            "{}",
            sql[0]
        );
        // And the row is held to what was written: a trigger that took it
        // away again would otherwise be recorded as this plan's own result
        // (DECISIONS 132).
        assert!(
            sql[0].contains(
                "IF NOT EXISTS (SELECT 1 FROM [dbo].[order_status] \
                 WHERE [code] = N'new' AND [label] = N'New')"
            ),
            "{}",
            sql[0]
        );
    }

    /// A column the row leaves to the table is checked too, where the default
    /// is a constant: a trigger rewriting one of those is the same silence as
    /// a trigger rewriting a spelled cell (DECISIONS 133).
    #[test]
    fn an_insert_holds_the_columns_it_left_to_their_defaults() {
        let sql = sql_of(&Change::InsertRow {
            table: tname("dbo.t"),
            key_column: "code".to_owned(),
            identity_key: false,
            key: RowKey::from("a"),
            row: row(&[("label", Value::Text("New".to_owned()))]),
            defaults: [
                ("sort".to_owned(), "((0))".to_owned()),
                ("note".to_owned(), "(NULL)".to_owned()),
                // Nothing the engine has to run is asked about: it has no
                // value before it runs, and a sequence would be consumed.
                ("seq".to_owned(), "(NEXT VALUE FOR dbo.s)".to_owned()),
                ("stamp".to_owned(), "(getdate())".to_owned()),
                // No `=` exists for the type, so no comparison does either.
                ("doc".to_owned(), "('<a/>')".to_owned()),
                // A plan made before the types travelled carries none.
                ("old".to_owned(), "((1))".to_owned()),
            ]
            .into_iter()
            .collect(),
            types: [
                // Spelled: held by the rendering that reads it back, under a
                // binary collation, so a rewrite the column's collation calls
                // equal is still a rewrite (DECISIONS 137).
                ("label", "nvarchar(50)"),
                ("sort", "int"),
                ("note", "nvarchar(50)"),
                ("seq", "int"),
                ("stamp", "datetime2"),
                ("doc", "xml"),
                // No default at all: the insert leaves NULL, and the row is
                // held to that (DECISIONS 136) — on a type without `=` too,
                // since `IS NULL` needs none.
                ("rank", "int"),
                ("body", "xml"),
            ]
            .into_iter()
            .map(|(c, t)| (c.to_owned(), ty(t)))
            .collect(),
        });
        let sql = &sql[0];
        for held in [
            "CONVERT(nvarchar(max), [label]) = N'New' COLLATE Latin1_General_BIN2",
            // A constant default: both sides rendered as the read-back
            // renders the column, the default converted to its type first.
            "(CONVERT(nvarchar(max), [sort]) = CONVERT(nvarchar(max), CONVERT(int, ((0)))) COLLATE Latin1_General_BIN2 OR ([sort] IS NULL AND (((0))) IS NULL))",
            "(CONVERT(nvarchar(max), [note]) = CONVERT(nvarchar(max), CONVERT(nvarchar(50), (NULL))) COLLATE Latin1_General_BIN2 OR ([note] IS NULL AND ((NULL)) IS NULL))",
            " AND [rank] IS NULL",
            " AND [body] IS NULL",
        ] {
            assert!(sql.contains(held), "{held}\n{sql}");
        }
        // A spelled column is held once, to what was spelled — never also
        // to NULL as a column the row left out.
        for absent in [
            "[seq]",
            "[stamp]",
            "[doc]",
            "[old]",
            "[label] = N'New'",
            "[label] IS NULL",
        ] {
            assert!(!sql.contains(absent), "{absent}\n{sql}");
        }
        // And the insert itself names only what the row spells.
        assert!(
            sql.contains("INSERT INTO [dbo].[t] ([code], [label]) VALUES (N'a', N'New');"),
            "{sql}"
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
            defaults: Default::default(),
            types: Default::default(),
            row: row(&[("label", Value::Text("Seven".to_owned()))]),
        });
        assert_eq!(sql.len(), 1, "{sql:?}");
        assert!(
            sql[0].contains(concat!(
                "SET IDENTITY_INSERT [dbo].[t] ON;\n",
                "INSERT INTO [dbo].[t] ([id], [label]) VALUES (N'7', N'Seven');\n",
                "SET IDENTITY_INSERT [dbo].[t] OFF;"
            )),
            "{}",
            sql[0]
        );
        // The switch goes off before the postcondition can throw: it is a
        // session setting, and a rollback would leave it on.
        assert!(
            sql[0].find("SET IDENTITY_INSERT [dbo].[t] OFF;") < sql[0].find("IF NOT EXISTS"),
            "{}",
            sql[0]
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
            defaults: Default::default(),
            types: Default::default(),
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
            defaults: Default::default(),
            types: Default::default(),
            row: Row::default(),
        });
        assert!(sql[0].contains("([code])"), "{sql:?}");
    }

    #[test]
    fn an_update_restates_only_the_changed_columns() {
        let sql = sql_of(&Change::UpdateRow {
            unchanged: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
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
        // No type carried (an older plan): the key alone holds the row, and
        // the count still has to be one. The postcondition is the key alone
        // for the same reason — a cell with no type has no rendering to be
        // compared by.
        assert_eq!(
            sql,
            [atomically(&format!(
                "UPDATE [dbo].[order_status] SET [label] = N'Opened' WHERE [code] = N'new';\n{}\n\
                 IF NOT EXISTS (SELECT 1 FROM [dbo].[order_status] WHERE [code] = N'new')\n  \
                 THROW 50000, N'dbo.order_status row `new` is not what this plan wrote once the \
                 statement had run — a trigger on the table, another writer inside it, or a \
                 value the engine stores differently from the way it is declared. Nothing was \
                 applied; `pbps plan --db` says which.', 1;",
                stale("dbo.order_status", "new")
            ))]
        );
    }

    /// The check every row `UPDATE` and `DELETE` ends with (DECISIONS 122).
    fn stale(table: &str, key: &str) -> String {
        format!(
            "IF @@ROWCOUNT <> 1 THROW 50000, N'{table} row `{key}` is not as the plan recorded \
             it: changed or deleted since the plan was made. Plan again.', 1;"
        )
    }

    /// The plan is reviewed against a recorded state, and the checksum pins
    /// it only up to the moment `apply` reads it. Each recorded cell goes
    /// into the predicate, compared by the rendering that read it — the
    /// column's type's — and a NULL as `IS NULL`; a literal default as the
    /// read-back compared it; a default the engine would have to run, a
    /// type without `=`, and a column the base does not have hold nothing.
    #[test]
    fn an_update_holds_the_row_to_what_the_plan_recorded() {
        let cell = |from: Cell, to: Cell| (from, to);
        let text = |s: &str| Cell::Value(Value::Text(s.to_owned()));
        let sql = sql_of(&Change::UpdateRow {
            table: tname("dbo.t"),
            key_column: "code".to_owned(),
            key: RowKey::from("a"),
            columns: [
                ("label".to_owned(), cell(text("Old"), text("New"))),
                (
                    "since".to_owned(),
                    cell(text("2026-09-03"), text("2026-09-04")),
                ),
                (
                    "flag".to_owned(),
                    cell(
                        Cell::Value(Value::Bool(true)),
                        Cell::Value(Value::Bool(false)),
                    ),
                ),
                (
                    "rank".to_owned(),
                    cell(Cell::Value(Value::Null), Cell::Value(Value::Int(2))),
                ),
                (
                    "sort".to_owned(),
                    cell(
                        Cell::Default("((0))".to_owned()),
                        Cell::Value(Value::Int(3)),
                    ),
                ),
                (
                    "stamp".to_owned(),
                    cell(Cell::Default("(getdate())".to_owned()), text("x")),
                ),
                (
                    "doc".to_owned(),
                    cell(Cell::Default("('')".to_owned()), text("<a/>")),
                ),
                (
                    "added".to_owned(),
                    cell(Cell::Value(Value::Null), text("y")),
                ),
            ]
            .into_iter()
            .collect(),
            unchanged: Default::default(),
            types: [
                ("label", "nvarchar(50)"),
                ("since", "date"),
                ("flag", "bit"),
                ("rank", "int"),
                ("sort", "int"),
                ("stamp", "datetime2"),
                ("doc", "xml"),
            ]
            .into_iter()
            .map(|(c, t)| (c.to_owned(), t.parse::<ColumnType>().unwrap()))
            .collect(),
            after_types: Default::default(),
        });
        let sql = &sql[0];
        let body = sql
            .strip_prefix("BEGIN TRANSACTION;\nBEGIN TRY\n")
            .expect("the write and its checks are one transaction");
        let mut lines = body.split('\n');
        let update = lines.next().expect("the update");
        assert_eq!(lines.next(), Some(stale("dbo.t", "a").as_str()));
        // And the row holds what the plan wrote, by the same rendering
        // (DECISIONS 132).
        let wrote = lines.next().expect("the postcondition");
        for held in [
            "[code] = N'a'",
            "CONVERT(nvarchar(max), [label]) = N'New' COLLATE Latin1_General_BIN2",
            "CONVERT(nvarchar(max), [rank]) = N'2' COLLATE Latin1_General_BIN2",
            "CONVERT(nvarchar(max), [since], 126) = N'2026-09-04' COLLATE Latin1_General_BIN2",
        ] {
            assert!(wrote.contains(held), "{held}\n{wrote}");
        }
        // The column with no type carried on either side has no rendering to
        // be compared by. (A column the base merely lacks *does* have one
        // after the plan runs; that is `after_types`, and the test below.)
        assert!(!wrote.contains("[added]"), "{wrote}");
        assert!(
            update.starts_with(
                "UPDATE [dbo].[t] SET [added] = N'y', [doc] = N'<a/>', [flag] = N'false', \
                 [label] = N'New', [rank] = 2, [since] = N'2026-09-04', [sort] = 3, \
                 [stamp] = N'x' WHERE [code] = N'a'"
            ),
            "{update}"
        );
        for held in [
            // The rendering that read it, and a change of case is a change.
            " AND CONVERT(nvarchar(max), [label]) = N'Old' COLLATE Latin1_General_BIN2",
            " AND CONVERT(nvarchar(max), [since], 126) = N'2026-09-03' COLLATE Latin1_General_BIN2",
            " AND CONVERT(nvarchar(max), [flag]) = N'1' COLLATE Latin1_General_BIN2",
            " AND [rank] IS NULL",
            " AND (CONVERT(nvarchar(max), [sort]) = CONVERT(nvarchar(max), CONVERT(int, ((0)))) COLLATE Latin1_General_BIN2 OR ([sort] IS NULL AND (((0))) IS NULL))",
        ] {
            assert!(update.contains(held), "{held}\n{update}");
        }
        for not_held in ["[stamp] =", "[doc] =", "[added] IS NULL"] {
            let after_where = update.split_once(" WHERE ").unwrap().1;
            assert!(!after_where.contains(not_held), "{not_held}\n{update}");
        }
        assert!(update.ends_with(';'), "{update}");
    }

    /// The cells the plan leaves alone are held too, before and after — but
    /// never restated in `SET`. A trigger rewriting a cell the plan did not
    /// touch, or a hand edit to it since the plan was made, is otherwise
    /// read back as the plan's own result (DECISIONS 136). A cell without a
    /// carried type holds nothing, as a changed one does not.
    #[test]
    fn an_update_holds_the_cells_it_leaves_alone_too() {
        let text = |s: &str| Cell::Value(Value::Text(s.to_owned()));
        let sql = sql_of(&Change::UpdateRow {
            table: tname("dbo.t"),
            key_column: "code".to_owned(),
            key: RowKey::from("a"),
            columns: [("label".to_owned(), (text("Old"), text("New")))]
                .into_iter()
                .collect(),
            unchanged: [
                ("note".to_owned(), text("kept")),
                ("rank".to_owned(), Cell::Value(Value::Null)),
                ("sort".to_owned(), Cell::Default("((0))".to_owned())),
                ("stamp".to_owned(), Cell::Default("(getdate())".to_owned())),
                ("added".to_owned(), text("z")),
            ]
            .into_iter()
            .collect(),
            types: [
                ("label", "nvarchar(50)"),
                ("note", "nvarchar(50)"),
                ("rank", "int"),
                ("sort", "int"),
                ("stamp", "datetime2"),
            ]
            .into_iter()
            .map(|(c, t)| (c.to_owned(), ty(t)))
            .collect(),
            after_types: Default::default(),
        });
        let sql = &sql[0];
        let update = sql
            .lines()
            .find(|l| l.starts_with("UPDATE "))
            .expect("the update");
        let wrote = sql
            .lines()
            .find(|l| l.starts_with("IF NOT EXISTS"))
            .expect("the postcondition");
        // Only the changed column is set.
        assert!(
            update.starts_with("UPDATE [dbo].[t] SET [label] = N'New' WHERE [code] = N'a'"),
            "{update}"
        );
        for held in [
            " AND CONVERT(nvarchar(max), [note]) = N'kept' COLLATE Latin1_General_BIN2",
            " AND [rank] IS NULL",
            " AND (CONVERT(nvarchar(max), [sort]) = CONVERT(nvarchar(max), CONVERT(int, ((0)))) COLLATE Latin1_General_BIN2 OR ([sort] IS NULL AND (((0))) IS NULL))",
        ] {
            assert!(update.contains(held), "{held}\n{update}");
            assert!(wrote.contains(held), "{held}\n{wrote}");
        }
        for not_held in ["[stamp]", "[added]"] {
            assert!(!update.contains(not_held), "{not_held}\n{update}");
            assert!(!wrote.contains(not_held), "{not_held}\n{wrote}");
        }
    }

    /// A column this plan adds and populates in the same revision has no
    /// recorded cell to hold the row to *before* the write — but it has one
    /// after: `AddColumn` and `AlterColumnType` both sort ahead of the row
    /// changes, so the `UPDATE` meets the declared type. Held to the base
    /// type alone, the added cell was checked by nothing, and an `AFTER
    /// UPDATE` trigger rewriting it was recorded as the plan's own result
    /// (DECISIONS 140).
    #[test]
    fn a_column_the_plan_adds_is_held_after_the_write_but_not_before() {
        let text = |s: &str| Cell::Value(Value::Text(s.to_owned()));
        let sql = sql_of(&Change::UpdateRow {
            table: tname("dbo.t"),
            key_column: "code".to_owned(),
            key: RowKey::from("a"),
            columns: [
                ("label".to_owned(), (text("Old"), text("New"))),
                // Added by this same plan: the differ's `before` is NULL by
                // convention, not something the base recorded.
                ("added".to_owned(), (Cell::Value(Value::Null), text("y"))),
                // Retyped by this same plan: `varchar` before, `date` after,
                // and the two render differently.
                ("since".to_owned(), (text("2026-09-03"), text("2026-09-04"))),
            ]
            .into_iter()
            .collect(),
            unchanged: [("note".to_owned(), text("kept"))].into_iter().collect(),
            types: [
                ("label", "nvarchar(50)"),
                ("note", "nvarchar(50)"),
                ("since", "varchar(10)"),
            ]
            .into_iter()
            .map(|(c, t)| (c.to_owned(), ty(t)))
            .collect(),
            after_types: [("added", "nvarchar(50)"), ("since", "date")]
                .into_iter()
                .map(|(c, t)| (c.to_owned(), ty(t)))
                .collect(),
        });
        let sql = &sql[0];
        let update = sql
            .lines()
            .find(|l| l.starts_with("UPDATE "))
            .expect("the update");
        let wrote = sql
            .lines()
            .find(|l| l.starts_with("IF NOT EXISTS"))
            .expect("the postcondition");
        let precondition = update.split_once(" WHERE ").expect("the key predicate").1;
        // Nothing recorded the added column, so nothing holds it beforehand.
        assert!(!precondition.contains("[added]"), "{update}");
        // But the row is answerable for it afterwards, by the type the
        // column will have.
        assert!(
            wrote.contains("CONVERT(nvarchar(max), [added]) = N'y' COLLATE Latin1_General_BIN2"),
            "{wrote}"
        );
        // A retyped column is held by both of its types before the write:
        // the recorded text goes back through `varchar(10)`, which is what
        // rendered it, and then through the conversion the `ALTER` ran, and
        // both sides are read in style 126 because that is how a `date`
        // reads back. Comparing the recorded text against the column
        // directly — either side's rendering, one type — is what
        // DECISIONS 146 could not make work and 149 stopped attempting.
        assert!(
            precondition.contains(
                "CONVERT(nvarchar(max), [since], 126) = \
                 CONVERT(nvarchar(max), TRY_CONVERT(date, \
                 TRY_CONVERT(varchar(10), N'2026-09-03')), 126) \
                 COLLATE Latin1_General_BIN2"
            ),
            "{update}"
        );
        assert!(
            wrote.contains(
                "CONVERT(nvarchar(max), [since], 126) = N'2026-09-04' COLLATE Latin1_General_BIN2"
            ),
            "{wrote}"
        );
        // A column neither added nor retyped reads the same on both sides.
        let held = "CONVERT(nvarchar(max), [note]) = N'kept' COLLATE Latin1_General_BIN2";
        assert!(precondition.contains(held), "{held}\n{update}");
        assert!(wrote.contains(held), "{held}\n{wrote}");
    }

    /// An omitted column means the declared default, and only the keyword can
    /// ask the engine for it. Writing NULL instead fails a NOT NULL column that
    /// `validate` had passed, and stores NULL in a nullable one where the
    /// declaration said "the default".
    #[test]
    fn an_update_to_the_default_says_default_not_null() {
        let sql = sql_of(&Change::UpdateRow {
            unchanged: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
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
        assert!(
            sql[0].contains("UPDATE [dbo].[t] SET [sort] = DEFAULT WHERE [code] = N'a';"),
            "{}",
            sql[0]
        );
        assert!(sql[0].contains(&stale("dbo.t", "a")), "{}", sql[0]);
        // `NOT EXISTS` is the postcondition's own; nothing here writes NULL,
        // and nothing compares against one.
        assert!(!sql[0].contains("= NULL"), "{}", sql[0]);
        assert!(!sql[0].contains("IS NULL"), "{}", sql[0]);
    }

    /// `UPDATE t SET WHERE ...` is not T-SQL. The differ never produces an
    /// empty column set, and refusing here is what makes that guarantee
    /// checkable rather than assumed.
    #[test]
    fn an_update_with_no_changed_column_is_refused() {
        assert!(
            emit(
                &Change::UpdateRow {
                    unchanged: Default::default(),
                    types: Default::default(),
                    after_types: Default::default(),
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

    /// The checksum pins the state up to the moment `apply` reads it, so the
    /// row the plan recorded travels into the predicate: a row an application
    /// rewrote in between is not the row that was reviewed (DECISIONS 143).
    #[test]
    fn a_delete_holds_the_row_to_what_the_plan_recorded() {
        let sql = sql_of(&Change::DeleteRow {
            table: tname("dbo.order_status"),
            key_column: "code".to_owned(),
            key: RowKey::from("old"),
            cause: pbps_model::change::DeleteCause::Undeclared,
            dropped: [
                (
                    "label".to_owned(),
                    Cell::Value(Value::Text("Dropped baseline".into())),
                ),
                ("dropped_only".to_owned(), Cell::Value(Value::Null)),
            ]
            .into_iter()
            .collect(),
            row: [
                (
                    "label".to_owned(),
                    Cell::Value(Value::Text("Old".to_owned())),
                ),
                ("rank".to_owned(), Cell::Value(Value::Null)),
                // No type, so nothing to compare it by: carried, not held.
                (
                    "shape".to_owned(),
                    Cell::Value(Value::Text("POINT (1 1)".to_owned())),
                ),
            ]
            .into_iter()
            .collect(),
            types: [
                ("label".to_owned(), ty("nvarchar(50)")),
                ("rank".to_owned(), ty("int")),
            ]
            .into_iter()
            .collect(),
            after_types: Default::default(),
        });
        assert_eq!(sql.len(), 1, "{sql:?}");
        assert!(!sql[0].contains("Dropped baseline"), "{sql:?}");
        assert!(!sql[0].contains("dropped_only"), "{sql:?}");
        let sql = &sql[0];
        assert!(
            sql.contains("DELETE FROM [dbo].[order_status] WHERE [code] = N'old' AND "),
            "{sql}"
        );
        // Each cell by the rendering that read it, and a NULL as IS NULL.
        assert!(sql.contains("COLLATE Latin1_General_BIN2"), "{sql}");
        assert!(sql.contains("[rank] IS NULL"), "{sql}");
        assert!(!sql.contains("[shape]"), "{sql}");
        // And still keyed, guarded and checked as before.
        assert!(sql.contains(&stale("dbo.order_status", "old")), "{sql}");
        assert!(sql.contains("WITH (HOLDLOCK)"), "{sql}");
    }

    #[test]
    fn a_delete_is_keyed_on_the_primary_key_column() {
        let sql = sql_of(&Change::DeleteRow {
            table: tname("dbo.order_status"),
            key_column: "code".to_owned(),
            key: RowKey::from("old"),
            cause: pbps_model::change::DeleteCause::Undeclared,
            dropped: Default::default(),
            row: BTreeMap::new(),
            types: BTreeMap::new(),
            after_types: Default::default(),
        });
        assert_eq!(sql.len(), 1, "{sql:?}");
        let sql = &sql[0];
        assert!(
            sql.contains("DELETE FROM [dbo].[order_status] WHERE [code] = N'old';"),
            "{sql}"
        );
        // And held to the row's existence: one gone already is a baseline
        // this plan was not reviewed against.
        assert!(sql.contains(&stale("dbo.order_status", "old")), "{sql}");
        // And to nothing referencing it *now*: the preflight probe counted
        // before the plan ran, and this keeps what it counted (DECISIONS 129).
        assert!(
            sql.contains("WITH (HOLDLOCK)") && sql.contains("IF @n > 0 THROW"),
            "{sql}"
        );
        // The guard and the delete stand or fall together even where the
        // apply runs statements outside a transaction.
        assert!(sql.starts_with("BEGIN TRANSACTION;\nBEGIN TRY\n"), "{sql}");
        assert!(
            sql.contains("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;"),
            "{sql}"
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
                name: "app_reader".into(),
                members: Vec::new(),
            }),
            ["DROP ROLE [app_reader];"]
        );
        // Members first, by name, then the role: the engine refuses the drop
        // while any remain, and the plan says exactly who is removed.
        assert_eq!(
            sql_of(&Change::DropRole {
                uid: uid.clone(),
                name: "app_reader".into(),
                members: vec!["app_svc".into(), "reporting".into()],
            }),
            [
                "ALTER ROLE [app_reader] DROP MEMBER [app_svc];",
                "ALTER ROLE [app_reader] DROP MEMBER [reporting];",
                "DROP ROLE [app_reader];"
            ]
        );
        // ALTER, never drop + add: the membership has to survive.
        let sql = sql_of(&Change::RenameRole {
            uid: uid.clone(),
            from: "reader".into(),
            to: "app_reader".into(),
        });
        assert_eq!(sql, ["ALTER ROLE [reader] WITH NAME = [app_reader];"]);
        assert!(!sql[0].contains("DROP"), "{sql:?}");
        // And the statement says what it did to the name, as a table rename's
        // do, so a staged checkpoint finds the role again (DECISIONS 93).
        let stmts = emit(
            &Change::RenameRole {
                uid: uid.clone(),
                from: "reader".into(),
                to: "app_reader".into(),
            },
            Strategy::default(),
        )
        .unwrap();
        assert_eq!(
            stmts[0].role_renames,
            [("reader".to_owned(), "app_reader".to_owned())]
        );
        assert!(stmts[0].renames.is_empty());
        // And a created role says so, for the checkpoint to adopt it
        // (DECISIONS 100).
        let created = emit(
            &Change::CreateRole {
                uid: uid.clone(),
                name: "auditors".into(),
            },
            Strategy::default(),
        )
        .unwrap();
        assert_eq!(created[0].creates, [Created::Role("auditors".into())]);
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

    /// A word the model holds for PostgreSQL (ADR-0010 §6) is never rendered
    /// into a statement this engine's parser would stop at (Msg 102): the
    /// change is refused as unsupported, naming the word, on a grant and on
    /// a revoke — even beside words the engine has.
    #[test]
    fn a_permission_this_engine_lacks_is_refused_not_rendered() {
        for change in [
            Change::Grant {
                role: "app_reader".into(),
                target: "dbo.customer".parse().unwrap(),
                permissions: perms(&[Permission::Select, Permission::Usage]),
            },
            Change::Revoke {
                role: "app_reader".into(),
                target: "schema::app".parse().unwrap(),
                permissions: perms(&[Permission::Truncate]),
            },
        ] {
            let e = emit(&change, Strategy::default()).unwrap_err();
            let DialectError::Unsupported { feature, .. } = &e else {
                panic!("not unsupported: {e:?}");
            };
            assert!(
                feature.contains("usage") || feature.contains("truncate"),
                "{feature}"
            );
            assert!(feature.contains("PostgreSQL"), "{feature}");
        }
        for p in [
            Permission::Usage,
            Permission::Create,
            Permission::Truncate,
            Permission::Trigger,
            Permission::Maintain,
        ] {
            assert!(permission_sql(p).is_err(), "{p:?}");
        }
        assert_eq!(
            permission_sql(Permission::ViewDefinition).unwrap(),
            "VIEW DEFINITION"
        );
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
            defaults: Default::default(),
            types: Default::default(),
            row: row(&[(
                "label",
                Value::Text("'); DROP TABLE [dbo].[t]; --".to_owned()),
            )]),
        });
        // Pinned exactly rather than by substring: what matters is that the
        // injected text is *inside* the literal, and only the whole statement
        // shows that. Every quote the value contained is doubled, so none of it
        // closes the literal early and none of it becomes statement text — in
        // the postcondition the write holds itself to as much as in the
        // `INSERT` (DECISIONS 132).
        assert_eq!(
            sql,
            [concat!(
                "BEGIN TRANSACTION;\nBEGIN TRY\n",
                "INSERT INTO [dbo].[t] ([code], [label]) ",
                r"VALUES (N'o''brien', N'''); DROP TABLE [dbo].[t]; --');",
                "\nIF NOT EXISTS (SELECT 1 FROM [dbo].[t] WHERE [code] = N'o''brien' ",
                r"AND [label] = N'''); DROP TABLE [dbo].[t]; --')",
                "\n  THROW 50000, N'dbo.t row `o''brien` is not what this plan wrote once ",
                "the statement had run — a trigger on the table, another writer inside it, ",
                "or a value the engine stores differently from the way it is declared. ",
                "Nothing was applied; `pbps plan --db` says which.', 1;",
                "\nCOMMIT TRANSACTION;\nEND TRY\nBEGIN CATCH\n",
                "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;\nTHROW;\nEND CATCH"
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
            defaults: Default::default(),
            types: Default::default(),
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
                defaults: Default::default(),
                types: Default::default(),
                row: Row::default(),
            },
            Change::DeleteRow {
                table: tname("dbo.t"),
                key_column: "code".to_owned(),
                key: RowKey::from("a"),
                cause: pbps_model::change::DeleteCause::Undeclared,
                dropped: Default::default(),
                row: BTreeMap::new(),
                types: BTreeMap::new(),
                after_types: Default::default(),
            },
        ] {
            assert!(!takes_online(&c), "{c:?}");
        }
    }
}
