//! Probes derived from the plan itself (SPEC §7.5).
//!
//! Each risky change knows how it can fail, so each one can be turned into a
//! query that counts the rows that would break it. On a non-zero count `apply`
//! aborts **before the first statement**, reporting the real number rather than
//! letting the engine discover it halfway through: "4,213 rows violate
//! ck_customer_amount" is actionable, and "the ALTER failed" is not.
//!
//! # What this is not
//!
//! It is not a second risk classifier. Classification is static and happens
//! offline (§7.2); these run at apply time, where a connection is guaranteed and
//! reading the data is the job.
//!
//! # Where a probe is deliberately absent
//!
//! An empty result means "nothing was checked", never "nothing is wrong", and
//! the cases are named rather than left to be discovered:
//!
//! - **Renames** carry a dependency risk, not a data one. They are answered by
//!   [`crate::impact`], which asks the catalog instead.
//! - **Drops** cannot be probed: every row is "affected", and counting them
//!   would produce a number that always looks alarming and never decides
//!   anything.
//! - **Reducing a `decimal`'s scale** rounds rather than failing, so no count
//!   exists to take. The `narrowing` class still flags it for approval.

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::{DialectError, Probe};
use pbps_model::{Change, ChangeSet, ColumnRef, ColumnType, RowKey, TableName, TypeArg};

use crate::emit::qualified;
use crate::ident::{literal, quote};
use crate::types;

/// Every probe a plan implies.
///
/// Errors in quoting are swallowed on purpose: an identifier this dialect
/// cannot write has already stopped the plan in [`crate::emit`], and a probe
/// list is not the place to report it a second time.
pub fn probes(changes: &ChangeSet) -> Vec<Probe> {
    let names = AsStored::of(changes);
    changes
        .changes
        .iter()
        .filter_map(|p| build(&p.change, &names).ok())
        .flatten()
        .collect()
}

/// Translates the names a plan uses into the names the database still has.
///
/// Probes run before the first statement, so every object they mention has to
/// be named as the catalog knows it *now*. A plan that renames `email` to
/// `contact_email` and then tightens it to NOT NULL describes that column by
/// its new name, and a probe built from the change alone would ask about a
/// column that does not exist yet — silently skipping the very check that
/// mattered.
#[derive(Debug, Default)]
struct AsStored {
    /// New table name → the name it currently has.
    tables: BTreeMap<TableName, TableName>,
    /// New column ref → the column name it currently has.
    columns: BTreeMap<ColumnRef, String>,
    /// Tables this plan creates. They are empty, so nothing in them can violate
    /// anything, and probing them would only produce "invalid object name".
    created: BTreeSet<TableName>,
    /// The declared rows this plan updates or deletes, by table: the key
    /// column and the keys. A row the plan itself moves off a parent that is
    /// going away must not be counted as still pointing at it (see
    /// [`delete_probe`]).
    moved: BTreeMap<TableName, (String, BTreeSet<RowKey>)>,
}

impl AsStored {
    fn of(changes: &ChangeSet) -> Self {
        let mut this = Self::default();
        for p in &changes.changes {
            match &p.change {
                Change::RenameTable { from, to, .. } => {
                    this.tables.insert(to.clone(), from.clone());
                }
                Change::RenameColumn {
                    table, from, to, ..
                } => {
                    this.columns.insert(table.column(to), from.clone());
                }
                Change::CreateTable { name, .. } => {
                    this.created.insert(name.clone());
                }
                Change::UpdateRow {
                    table,
                    key_column,
                    key,
                    ..
                }
                | Change::DeleteRow {
                    table,
                    key_column,
                    key,
                    ..
                } => {
                    this.moved
                        .entry(table.clone())
                        .or_insert_with(|| (key_column.clone(), BTreeSet::new()))
                        .1
                        .insert(key.clone());
                }
                // Exhaustive rather than `_`: a change added later that moves a
                // name has to be reflected here, or every probe downstream of
                // it would quietly query the wrong object.
                Change::DropTable { .. }
                | Change::AddColumn { .. }
                | Change::DropColumn { .. }
                | Change::AlterColumnType { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddForeignKey { .. }
                | Change::DropForeignKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
                | Change::DropIndex { .. }
                // A module carries no data and no identity, so nothing here
                // applies to one.
                | Change::CreateModule { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                // A row change moves no name. It does *use* one — the table it
                // writes to — but that table is already translated through
                // `table()` below like every other name in a probe.
                | Change::InsertRow { .. }
                | Change::SetDataMode { .. }
                // A role is a principal, not an object: nothing here names a
                // table or a column, and nothing here moves a name a probe
                // could ask about.
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. } => {}
            }
        }
        this
    }

    /// `None` when the object does not exist yet, and so cannot be probed.
    fn table(&self, name: &TableName) -> Option<TableName> {
        if self.created.contains(name) {
            return None;
        }
        Some(
            self.tables
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.clone()),
        )
    }

    fn column(&self, column: &ColumnRef) -> Option<ColumnRef> {
        let table = self.table(&column.table)?;
        let name = self
            .columns
            .get(column)
            .cloned()
            .unwrap_or_else(|| column.name.clone());
        Some(table.column(&name))
    }
}

fn build(change: &Change, names: &AsStored) -> Result<Vec<Probe>, DialectError> {
    match change {
        Change::AlterColumnNullability {
            column,
            to_nullable: false,
            ..
        } => match names.column(column) {
            Some(stored) => Ok(vec![null_probe(column, &stored)?]),
            None => Ok(Vec::new()),
        },

        Change::AlterColumnType {
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            ..
        } => {
            let Some(stored) = names.column(column) else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            // A type change folds a nullability change into itself (§12), so it
            // has to carry that change's probe too.
            if *from_nullable && !*to_nullable {
                out.push(null_probe(column, &stored)?);
            }
            if types::change_risk(from, to).risk_class().is_some() {
                out.extend(conversion_probe(column, &stored, to)?);
            }
            Ok(out)
        }

        Change::AddCheck {
            table,
            name,
            constraint,
        } => {
            let Some(stored) = names.table(table) else {
                return Ok(Vec::new());
            };
            Ok(vec![Probe::new(
                format!("rows that violate the new check {name}"),
                // A CHECK rejects a row only when its predicate is FALSE;
                // UNKNOWN passes. `WHERE NOT (expr)` has exactly that
                // behaviour, so the count matches what the engine will refuse.
                //
                // The expression is not rewritten for renames: rewriting SQL
                // text by substitution is how a tool that promised never to
                // parse SQL starts parsing it badly. Such a probe fails to run
                // and is reported as unchecked, which is honest.
                format!(
                    "SELECT COUNT(*) AS n FROM {} WHERE NOT ({});",
                    qualified(&stored)?,
                    constraint.expression
                ),
            )])
        }

        Change::AddUnique {
            table,
            name,
            constraint,
        } => match stored_columns(names, table, &constraint.columns) {
            Some((stored, columns)) => Ok(vec![duplicate_probe(
                &stored,
                &columns,
                &format!("rows that would collide under the new unique constraint {name}"),
            )?]),
            None => Ok(Vec::new()),
        },

        Change::SetPrimaryKey {
            table,
            to: Some(pk),
            ..
        } => {
            let Some((stored, columns)) = stored_columns(names, table, &pk.columns) else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            for (declared, current) in pk.columns.iter().zip(&columns) {
                out.push(null_probe(
                    &table.column(declared),
                    &stored.column(current),
                )?);
            }
            out.push(duplicate_probe(
                &stored,
                &columns,
                "rows that would collide under the new primary key",
            )?);
            Ok(out)
        }

        Change::AddForeignKey {
            table,
            name,
            constraint,
        } => {
            let Some((child_table, child_columns)) =
                stored_columns(names, table, &constraint.columns)
            else {
                return Ok(Vec::new());
            };
            let Some((parent_table, parent_columns)) = stored_columns(
                names,
                &constraint.references_table,
                &constraint.references_columns,
            ) else {
                return Ok(Vec::new());
            };

            // A row with any NULL in the key is exempt from the constraint
            // (MATCH SIMPLE, which is what SQL Server implements), so excluding
            // them is not leniency — it is the rule.
            let not_null: Vec<String> = child_columns
                .iter()
                .map(|c| Ok(format!("c.{} IS NOT NULL", quote(c)?)))
                .collect::<Result<_, DialectError>>()?;
            let join: Vec<String> = child_columns
                .iter()
                .zip(&parent_columns)
                .map(|(child, parent)| Ok(format!("p.{} = c.{}", quote(parent)?, quote(child)?)))
                .collect::<Result<_, DialectError>>()?;

            Ok(vec![Probe::new(
                format!("rows with no matching parent for the new foreign key {name}"),
                format!(
                    "SELECT COUNT(*) AS n FROM {} AS c\n WHERE {}\n   AND NOT EXISTS (SELECT 1 FROM {} AS p WHERE {});",
                    qualified(&child_table)?,
                    not_null.join("\n   AND "),
                    qualified(&parent_table)?,
                    join.join(" AND ")
                ),
            )])
        }

        // Everything else either cannot fail on data or cannot be counted; see
        // the module docs.
        Change::CreateTable { .. }
        | Change::DropTable { .. }
        | Change::RenameTable { .. }
        | Change::AddColumn { .. }
        | Change::DropColumn { .. }
        | Change::RenameColumn { .. }
        | Change::AlterColumnNullability {
            to_nullable: true, ..
        }
        | Change::AlterColumnDefault { .. }
        // A module holds no rows. A definition the engine will not compile
        // fails inside the plan's transaction, where the rollback is total.
        | Change::CreateModule { .. }
        | Change::AlterModule { .. }
        | Change::DropModule { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { to: None, .. }
        | Change::DropUnique { .. }
        | Change::DropForeignKey { .. }
        | Change::DropCheck { .. }
        | Change::AddIndex { .. }
        | Change::DropIndex { .. }
        // Inserting and updating a declared row need no probe: the row's whole
        // content is in the plan, and anything the engine refuses about it
        // rolls the plan back.
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::SetDataMode { .. }
        // A permission change fails on nothing in the data; the engine refuses
        // an impossible grant inside the transaction.
        | Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. } => Ok(Vec::new()),

        Change::DeleteRow {
            table,
            key_column,
            key,
            ..
        } => match names.column(&table.column(key_column)) {
            Some(stored) => Ok(vec![delete_probe(table, key, &stored, names)?]),
            None => Ok(Vec::new()),
        },
    }
}

/// The rows in other tables that still point at a row about to be deleted
/// (ADR-0004).
///
/// # Why the referencing tables are found at run time
///
/// A probe is built from the plan and nothing else, and the plan does not know
/// which tables reference this one — nor should it trust the declarations to
/// say: a foreign key someone added by hand is exactly the one that will refuse
/// the delete. So the statement asks `sys.foreign_keys` for every column that
/// references the key column, builds one `COUNT(*)` per referencing table, and
/// runs the result through `sp_executesql`. Nothing user-written reaches the
/// dynamic text: the table and column names come from the catalog and go
/// through `QUOTENAME`, and the key is bound as a parameter.
///
/// # Why `ON DELETE CASCADE` counts too
///
/// The engine would not refuse such a delete — it would take the child rows
/// with it, silently. A reference row's delete cascading into an application
/// table is the disaster the `data-delete` gate exists for, so those rows are
/// counted and the apply stops the same way.
///
/// # Rows the plan itself moves
///
/// A child row that this plan updates or deletes is left out of the count. The
/// ordinary shape is a child moved to a new parent in the same revision, and
/// the plan runs that update *before* the delete precisely so the engine
/// accepts it — a probe that ran before the first statement would otherwise
/// refuse every such plan. The exclusion is by key, not by which column the
/// update touches, so it can over-exclude a row whose update leaves it still
/// pointing at the doomed parent; that row is then refused by the engine inside
/// the transaction, where the rollback is total. Under-counting fails loudly,
/// which is the direction to be wrong in.
fn delete_probe(
    table: &TableName,
    key: &RowKey,
    stored: &ColumnRef,
    names: &AsStored,
) -> Result<Probe, DialectError> {
    // Per referencing table, the keys this plan moves in it — spelled as the
    // literal list the generated `NOT IN` will carry, under the name the
    // database has for the table now. Doubled quotes, because the fragment is
    // itself inside a T-SQL string literal.
    let mut exclusions = Vec::new();
    for (child, (child_key, keys)) in &names.moved {
        let Some(stored_child) = names.table(child) else {
            continue;
        };
        let Some(stored_key) = names.column(&child.column(child_key)) else {
            continue;
        };
        let list: Vec<String> = keys.iter().map(|k| literal(k.as_str())).collect();
        let clause = format!(
            " AND {} NOT IN ({})",
            quote(&stored_key.name)?,
            list.join(", ")
        );
        exclusions.push(format!(
            "WHEN s.name = {} AND t.name = {} THEN {}",
            literal(&stored_child.schema),
            literal(&stored_child.name),
            literal(&clause)
        ));
    }
    let exclusion = if exclusions.is_empty() {
        "N''".to_owned()
    } else {
        format!("CASE {} ELSE N'' END", exclusions.join(" "))
    };

    Ok(Probe::new(
        format!(
            "rows in other tables that still reference {table} row `{key}`, which its delete \
             would orphan or cascade into"
        ),
        format!(
            "DECLARE @n int = 0, @sql nvarchar(max);\n\
             SELECT @sql = STRING_AGG(CONVERT(nvarchar(max),\n\
                 N'SELECT @n += (SELECT COUNT(*) FROM ' + QUOTENAME(s.name) + N'.' + QUOTENAME(t.name)\n\
                 + N' WHERE ' + QUOTENAME(c.name) + N' = @key' + {exclusion} + N');'), N' ')\n\
               FROM sys.foreign_keys fk\n\
               JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id\n\
               JOIN sys.tables t ON t.object_id = fk.parent_object_id\n\
               JOIN sys.schemas s ON s.schema_id = t.schema_id\n\
               JOIN sys.columns c ON c.object_id = fkc.parent_object_id AND c.column_id = fkc.parent_column_id\n\
               JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id\n\
              WHERE fk.referenced_object_id = OBJECT_ID({}) AND rc.name = {};\n\
             IF @sql IS NOT NULL EXEC sp_executesql @sql, N'@key nvarchar(max), @n int OUTPUT', @key = {}, @n = @n OUTPUT;\n\
             SELECT @n AS n;",
            literal(&qualified(&stored.table)?),
            literal(&stored.name),
            literal(key.as_str())
        ),
    ))
}

/// The table and columns as the database currently names them, or `None` when
/// the table does not exist yet.
fn stored_columns(
    names: &AsStored,
    table: &TableName,
    columns: &[String],
) -> Option<(TableName, Vec<String>)> {
    let stored = names.table(table)?;
    let columns = columns
        .iter()
        .map(|c| names.column(&table.column(c)).map(|r| r.name))
        .collect::<Option<Vec<_>>>()?;
    Some((stored, columns))
}

/// `column` names it as the plan does — for the message a human reads — and
/// `stored` as the database does, for the query.
fn null_probe(column: &ColumnRef, stored: &ColumnRef) -> Result<Probe, DialectError> {
    Ok(Probe::new(
        format!("existing NULLs in {column}, which NOT NULL would reject"),
        format!(
            "SELECT COUNT(*) AS n FROM {} WHERE {} IS NULL;",
            qualified(&stored.table)?,
            quote(&stored.name)?
        ),
    ))
}

fn duplicate_probe(
    table: &TableName,
    columns: &[String],
    description: &str,
) -> Result<Probe, DialectError> {
    let list = columns
        .iter()
        .map(|c| quote(c))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    Ok(Probe::new(
        description,
        // The rows, not the groups: "3 duplicate groups" makes an operator do
        // arithmetic before they know how much data is involved. `GROUP BY`
        // treats NULLs as equal, which is also how SQL Server's UNIQUE treats
        // them, so the count matches what the engine will refuse.
        format!(
            "SELECT ISNULL(SUM(c), 0) AS n FROM (\n    SELECT COUNT(*) AS c FROM {}\n     GROUP BY {} HAVING COUNT(*) > 1) AS dup;",
            qualified(table)?,
            list
        ),
    ))
}

/// The probe for narrowing a column's type, or `None` when no count exists.
fn conversion_probe(
    column: &ColumnRef,
    stored: &ColumnRef,
    to: &ColumnType,
) -> Result<Vec<Probe>, DialectError> {
    let table = qualified(&stored.table)?;
    let col = quote(&stored.name)?;

    // A bounded string or binary target truncates silently under CONVERT, so
    // `TRY_CONVERT` would return a value and report nothing. Length is the only
    // question that has an answer here.
    let bound = match to.args.first() {
        Some(TypeArg::Int(n)) => Some(*n),
        Some(TypeArg::Max) | Some(TypeArg::Ident(_)) | None => None,
    };
    if let Some(n) = bound {
        match to.base.as_str() {
            // LEN ignores trailing blanks, and so does the engine when it
            // shortens a character column — counting them would report rows
            // that convert perfectly well.
            "char" | "nchar" | "varchar" | "nvarchar" => {
                return Ok(vec![Probe::new(
                    format!("values in {column} too long for {to}"),
                    format!(
                        "SELECT COUNT(*) AS n FROM {table} WHERE {col} IS NOT NULL AND LEN({col}) > {n};"
                    ),
                )]);
            }
            "binary" | "varbinary" => {
                return Ok(vec![Probe::new(
                    format!("values in {column} too long for {to}"),
                    format!(
                        "SELECT COUNT(*) AS n FROM {table} WHERE {col} IS NOT NULL AND DATALENGTH({col}) > {n};"
                    ),
                )]);
            }
            _ => {}
        }
    }

    // Anything else: let the engine answer. `TRY_CONVERT` returns NULL exactly
    // where `CONVERT` would raise, which is the definition of the rows that
    // would break the ALTER.
    //
    // The type is interpolated, not bound: `TRY_CONVERT`'s first argument is a
    // type, and a type is not a value a parameter can carry. It comes from the
    // dialect's own catalogue, never from user text — `normalize` has already
    // rejected anything that is not a type this dialect knows.
    if types::normalize(to).is_err() {
        return Ok(Vec::new());
    }
    Ok(vec![Probe::new(
        format!("values in {column} that cannot become {to}"),
        format!(
            "SELECT COUNT(*) AS n FROM {table}\n WHERE {col} IS NOT NULL AND TRY_CONVERT({to}, {col}) IS NULL;"
        ),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{
        CheckConstraint, ForeignKey, PlannedChange, PrimaryKey, ReferentialAction, UniqueConstraint,
    };

    fn cref(s: &str) -> ColumnRef {
        s.parse().unwrap()
    }
    fn tname(s: &str) -> TableName {
        s.parse().unwrap()
    }
    fn ty(s: &str) -> ColumnType {
        types::normalize(&s.parse::<ColumnType>().unwrap()).unwrap()
    }
    fn uid(s: &str) -> pbps_model::Uid {
        s.parse().unwrap()
    }
    fn plan(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        }
    }
    fn sql_of(change: &Change) -> Vec<String> {
        probes(&plan(vec![change.clone()]))
            .into_iter()
            .map(|p| p.sql)
            .collect()
    }

    #[test]
    fn tightening_nullability_counts_the_nulls() {
        let sql = sql_of(&Change::AlterColumnNullability {
            uid: uid("c_aaaaaa"),
            column: cref("dbo.customer.email"),
            ty: ty("nvarchar(255)"),
            to_nullable: false,
        });
        assert_eq!(
            sql,
            ["SELECT COUNT(*) AS n FROM [dbo].[customer] WHERE [email] IS NULL;"]
        );
    }

    /// Relaxing to nullable cannot fail, and a probe that always returns zero
    /// is noise an operator learns to skip.
    #[test]
    fn relaxing_nullability_needs_no_probe() {
        assert!(
            sql_of(&Change::AlterColumnNullability {
                uid: uid("c_aaaaaa"),
                column: cref("dbo.customer.email"),
                ty: ty("nvarchar(255)"),
                to_nullable: true,
            })
            .is_empty()
        );
    }

    /// The differ folds a type change and a nullability change into one change
    /// (§12); folding must not lose the probe the separate change would have
    /// carried.
    #[test]
    fn a_type_change_that_also_tightens_nullability_probes_both() {
        let sql = sql_of(&Change::AlterColumnType {
            uid: uid("c_aaaaaa"),
            column: cref("dbo.customer.email"),
            from: ty("nvarchar(255)"),
            to: ty("nvarchar(50)"),
            from_nullable: true,
            to_nullable: false,
        });
        assert_eq!(sql.len(), 2, "{sql:?}");
        assert!(sql[0].contains("IS NULL"), "{sql:?}");
        assert!(sql[1].contains("LEN([email]) > 50"), "{sql:?}");
    }

    /// A shorter string truncates silently under CONVERT, so TRY_CONVERT would
    /// report nothing at all — length is the only question with an answer.
    #[test]
    fn shortening_a_string_is_probed_by_length_not_conversion() {
        let sql = sql_of(&Change::AlterColumnType {
            uid: uid("c_aaaaaa"),
            column: cref("dbo.customer.email"),
            from: ty("nvarchar(255)"),
            to: ty("nvarchar(50)"),
            from_nullable: true,
            to_nullable: true,
        });
        assert_eq!(sql.len(), 1);
        assert!(sql[0].contains("LEN([email]) > 50"), "{sql:?}");
        assert!(!sql[0].contains("TRY_CONVERT"), "{sql:?}");
    }

    #[test]
    fn narrowing_a_number_is_probed_by_conversion() {
        let sql = sql_of(&Change::AlterColumnType {
            uid: uid("c_aaaaaa"),
            column: cref("dbo.customer.balance"),
            from: ty("bigint"),
            to: ty("int"),
            from_nullable: true,
            to_nullable: true,
        });
        assert_eq!(sql.len(), 1);
        assert!(
            sql[0].contains("TRY_CONVERT(int, [balance]) IS NULL"),
            "{sql:?}"
        );
    }

    /// Widening cannot fail, so it is not probed — the same principle as
    /// relaxing nullability.
    #[test]
    fn widening_a_type_needs_no_probe() {
        assert!(
            sql_of(&Change::AlterColumnType {
                uid: uid("c_aaaaaa"),
                column: cref("dbo.customer.balance"),
                from: ty("int"),
                to: ty("bigint"),
                from_nullable: true,
                to_nullable: true,
            })
            .is_empty()
        );
    }

    #[test]
    fn a_check_counts_the_rows_it_would_reject() {
        let sql = sql_of(&Change::AddCheck {
            table: tname("dbo.customer"),
            name: "ck_customer_amount".into(),
            constraint: CheckConstraint {
                expression: "[amount] >= 0".into(),
            },
        });
        assert_eq!(
            sql,
            ["SELECT COUNT(*) AS n FROM [dbo].[customer] WHERE NOT ([amount] >= 0);"]
        );
    }

    #[test]
    fn a_unique_constraint_counts_the_colliding_rows() {
        let sql = sql_of(&Change::AddUnique {
            table: tname("dbo.customer"),
            name: "uq_customer_email".into(),
            constraint: UniqueConstraint {
                columns: vec!["email".into(), "tenant".into()],
            },
        });
        assert_eq!(sql.len(), 1);
        assert!(sql[0].contains("GROUP BY [email], [tenant]"), "{sql:?}");
        assert!(sql[0].contains("HAVING COUNT(*) > 1"), "{sql:?}");
        // The rows, not the groups.
        assert!(sql[0].contains("SUM(c)"), "{sql:?}");
    }

    #[test]
    fn a_foreign_key_counts_the_orphans() {
        let sql = sql_of(&Change::AddForeignKey {
            table: tname("dbo.customer"),
            name: "fk_customer_region".into(),
            constraint: Box::new(ForeignKey {
                columns: vec!["region_id".into()],
                references_table: tname("dbo.region"),
                references_columns: vec!["region_id".into()],
                on_delete: ReferentialAction::NoAction,
                on_update: ReferentialAction::NoAction,
            }),
        });
        assert_eq!(sql.len(), 1);
        assert!(sql[0].contains("NOT EXISTS"), "{sql:?}");
        assert!(sql[0].contains("[dbo].[region] AS p"), "{sql:?}");
        // A NULL key is exempt from the constraint, so it must be exempt from
        // the probe too, or every optional relationship reports false orphans.
        assert!(sql[0].contains("c.[region_id] IS NOT NULL"), "{sql:?}");
    }

    #[test]
    fn a_new_primary_key_probes_nulls_and_duplicates() {
        let sql = sql_of(&Change::SetPrimaryKey {
            table: tname("dbo.customer"),
            from: None,
            to: Some(PrimaryKey {
                name: Some("pk_customer".into()),
                columns: vec!["id".into()],
            }),
        });
        assert_eq!(sql.len(), 2, "{sql:?}");
        assert!(sql[0].contains("[id] IS NULL"), "{sql:?}");
        assert!(sql[1].contains("GROUP BY [id]"), "{sql:?}");
    }

    /// Named rather than left to be discovered: these carry no data question a
    /// count can answer (see the module docs).
    #[test]
    fn drops_and_renames_carry_no_probe() {
        for change in [
            Change::DropTable {
                uid: uid("t_aaaaaa"),
                name: tname("dbo.customer"),
            },
            Change::DropColumn {
                uid: uid("c_aaaaaa"),
                column: cref("dbo.customer.email"),
            },
            Change::RenameColumn {
                uid: uid("c_aaaaaa"),
                table: tname("dbo.customer"),
                from: "email".into(),
                to: "contact_email".into(),
            },
            Change::SetPrimaryKey {
                table: tname("dbo.customer"),
                from: Some(PrimaryKey {
                    name: None,
                    columns: vec!["id".into()],
                }),
                to: None,
            },
        ] {
            assert!(sql_of(&change).is_empty(), "{change:?}");
        }
    }

    /// The connected half of ADR-0004: a row delete counts what still points
    /// at the row, finds the referencing tables in the catalog at run time, and
    /// leaves out the rows this plan itself moves — under the names the
    /// database has for them now.
    #[test]
    fn deleting_a_row_counts_the_rows_that_still_reference_it() {
        let cs = plan(vec![Change::DeleteRow {
            table: tname("dbo.status"),
            key_column: "code".into(),
            key: RowKey::from("old"),
            cause: pbps_model::change::DeleteCause::Undeclared,
        }]);
        let p = probes(&cs);
        assert_eq!(p.len(), 1, "{p:?}");
        let sql = &p[0].sql;
        assert!(sql.contains("OBJECT_ID(N'[dbo].[status]')"), "{sql}");
        assert!(sql.contains("rc.name = N'code'"), "{sql}");
        assert!(sql.contains("@key = N'old'"), "{sql}");
        assert!(sql.contains("sys.foreign_keys"), "{sql}");
        assert!(sql.contains("QUOTENAME"), "{sql}");
        // The deleted row itself is excluded from a self-referencing count: a
        // row pointing at itself is gone with the delete, not orphaned by it.
        assert!(
            sql.contains(
                "WHEN s.name = N'dbo' AND t.name = N'status' THEN N' AND [code] NOT IN (N''old'')'"
            ),
            "{sql}"
        );
        assert!(p[0].description.contains("dbo.status"), "{:?}", p[0]);
        assert!(p[0].description.contains("`old`"), "{:?}", p[0]);
    }

    /// The live test's own shape: the child moves to a new parent in the same
    /// plan, before the delete. Counting it would refuse the one plan the
    /// ordering was designed to make acceptable.
    #[test]
    fn a_child_row_the_plan_moves_is_not_counted_against_the_delete() {
        let cs = plan(vec![
            Change::UpdateRow {
                table: tname("dbo.kind"),
                key_column: "id".into(),
                key: RowKey::from("7"),
                columns: [(
                    "status_code".to_owned(),
                    (
                        pbps_model::Cell::Value(pbps_model::Value::Text("old".into())),
                        pbps_model::Cell::Value(pbps_model::Value::Text("new".into())),
                    ),
                )]
                .into_iter()
                .collect(),
            },
            Change::DeleteRow {
                table: tname("dbo.status"),
                key_column: "code".into(),
                key: RowKey::from("old"),
                cause: pbps_model::change::DeleteCause::Undeclared,
            },
        ]);
        let p = probes(&cs);
        assert_eq!(p.len(), 1, "{p:?}");
        let sql = &p[0].sql;
        // Inside the dynamic text, so the quotes are doubled.
        assert!(
            sql.contains(
                "WHEN s.name = N'dbo' AND t.name = N'kind' THEN N' AND [id] NOT IN (N''7'')'"
            ),
            "{sql}"
        );
    }

    /// The delete names the row's table as the plan does, after any rename in
    /// the same plan; the catalog still has the old name when the probe runs.
    #[test]
    fn a_row_delete_is_probed_under_the_table_name_the_database_still_has() {
        let cs = plan(vec![
            Change::RenameTable {
                uid: uid("t_aaaaaa"),
                from: tname("dbo.state"),
                to: tname("dbo.status"),
            },
            Change::DeleteRow {
                table: tname("dbo.status"),
                key_column: "code".into(),
                key: RowKey::from("old"),
                cause: pbps_model::change::DeleteCause::Undeclared,
            },
        ]);
        let p = probes(&cs);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            p[0].sql.contains("OBJECT_ID(N'[dbo].[state]')"),
            "{}",
            p[0].sql
        );
        assert!(p[0].description.contains("dbo.status"), "{:?}", p[0]);
    }

    /// A table this plan creates has no rows for anything to reference; the
    /// same rule as every other probe.
    #[test]
    fn a_row_delete_in_a_table_created_by_this_plan_is_not_probed() {
        let mut table = pbps_model::Table::default();
        table
            .columns
            .insert("code".into(), pbps_model::Column::new(ty("varchar(20)")));
        let cs = plan(vec![
            Change::CreateTable {
                uid: uid("t_aaaaaa"),
                name: tname("dbo.status"),
                table: Box::new(table),
            },
            Change::DeleteRow {
                table: tname("dbo.status"),
                key_column: "code".into(),
                key: RowKey::from("old"),
                cause: pbps_model::change::DeleteCause::Undeclared,
            },
        ]);
        assert!(probes(&cs).is_empty());
    }

    /// Every probe must be a counting query returning one column called `n`;
    /// the executor reads it positionally and a probe that returned rows
    /// instead of a count would be read as garbage.
    #[test]
    fn every_probe_counts() {
        let changes = plan(vec![
            Change::AlterColumnNullability {
                uid: uid("c_aaaaaa"),
                column: cref("dbo.customer.email"),
                ty: ty("nvarchar(255)"),
                to_nullable: false,
            },
            Change::AddCheck {
                table: tname("dbo.customer"),
                name: "ck".into(),
                constraint: CheckConstraint {
                    expression: "[amount] >= 0".into(),
                },
            },
            Change::AddUnique {
                table: tname("dbo.customer"),
                name: "uq".into(),
                constraint: UniqueConstraint {
                    columns: vec!["email".into()],
                },
            },
        ]);
        let all = probes(&changes);
        assert!(!all.is_empty());
        for probe in all {
            assert!(probe.sql.contains(" AS n"), "{probe:?}");
            assert!(probe.sql.trim_end().ends_with(';'), "{probe:?}");
            assert!(!probe.description.is_empty(), "{probe:?}");
        }
    }

    /// The bug this whole `AsStored` layer exists for: a plan that renames a
    /// column and then tightens it must probe the name the database still has.
    /// Built change-by-change, the probe asked about a column that did not
    /// exist yet, and the one check that mattered was silently skipped.
    #[test]
    fn a_probe_uses_the_name_the_database_still_has() {
        let cs = plan(vec![
            Change::RenameColumn {
                uid: uid("c_aaaaaa"),
                table: tname("dbo.customer"),
                from: "email".into(),
                to: "contact_email".into(),
            },
            Change::AlterColumnNullability {
                uid: uid("c_aaaaaa"),
                column: cref("dbo.customer.contact_email"),
                ty: ty("nvarchar(255)"),
                to_nullable: false,
            },
        ]);
        let p = probes(&cs);
        assert_eq!(p.len(), 1);
        assert!(p[0].sql.contains("[email] IS NULL"), "{:?}", p[0]);
        assert!(!p[0].sql.contains("contact_email"), "{:?}", p[0]);
        // The message names it as the plan does; the reader is looking at the
        // plan, not at the catalog.
        assert!(p[0].description.contains("contact_email"), "{:?}", p[0]);
    }

    #[test]
    fn a_renamed_table_is_probed_under_its_old_name() {
        let cs = plan(vec![
            Change::RenameTable {
                uid: uid("t_aaaaaa"),
                from: tname("dbo.client"),
                to: tname("dbo.customer"),
            },
            Change::AddCheck {
                table: tname("dbo.customer"),
                name: "ck".into(),
                constraint: CheckConstraint {
                    expression: "[amount] >= 0".into(),
                },
            },
        ]);
        let p = probes(&cs);
        assert_eq!(p.len(), 1);
        assert!(p[0].sql.contains("[dbo].[client]"), "{:?}", p[0]);
    }

    /// A table this plan creates is empty, so nothing in it can violate
    /// anything — and probing it would only produce "invalid object name",
    /// which reads like a failure rather than the non-question it is.
    #[test]
    fn a_table_created_by_this_plan_is_not_probed() {
        let mut table = pbps_model::Table::default();
        table
            .columns
            .insert("id".into(), pbps_model::Column::new(ty("int")));
        let cs = plan(vec![
            Change::CreateTable {
                uid: uid("t_aaaaaa"),
                name: tname("dbo.brand_new"),
                table: Box::new(table),
            },
            Change::AddUnique {
                table: tname("dbo.brand_new"),
                name: "uq".into(),
                constraint: UniqueConstraint {
                    columns: vec!["id".into()],
                },
            },
        ]);
        assert!(probes(&cs).is_empty());
    }
}
