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

use pbps_dialect::{DialectError, Probe};
use pbps_model::{Change, ColumnRef, ColumnType, TableName, TypeArg};

use crate::emit::qualified;
use crate::ident::quote;
use crate::types;

/// Every probe this change implies. Errors in quoting are swallowed here on
/// purpose: an identifier this dialect cannot write has already stopped the
/// plan in [`crate::emit`], and a probe list is not the place to report it a
/// second time.
pub fn probes(change: &Change) -> Vec<Probe> {
    build(change).unwrap_or_default()
}

fn build(change: &Change) -> Result<Vec<Probe>, DialectError> {
    match change {
        Change::AlterColumnNullability {
            column,
            to_nullable: false,
            ..
        } => Ok(vec![null_probe(column)?]),

        Change::AlterColumnType {
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            ..
        } => {
            let mut out = Vec::new();
            // A type change folds a nullability change into itself (§12), so it
            // has to carry that change's probe too.
            if *from_nullable && !*to_nullable {
                out.push(null_probe(column)?);
            }
            if types::change_risk(from, to).risk_class().is_some() {
                out.extend(conversion_probe(column, to)?);
            }
            Ok(out)
        }

        Change::AddCheck {
            table,
            name,
            constraint,
        } => Ok(vec![Probe::new(
            format!("rows that violate the new check {name}"),
            // A CHECK rejects a row only when its predicate is FALSE; UNKNOWN
            // passes. `WHERE NOT (expr)` has exactly that behaviour, so the
            // count matches what the engine will refuse.
            format!(
                "SELECT COUNT(*) AS n FROM {} WHERE NOT ({});",
                qualified(table)?,
                constraint.expression
            ),
        )]),

        Change::AddUnique {
            table,
            name,
            constraint,
        } => Ok(vec![duplicate_probe(
            table,
            &constraint.columns,
            &format!("rows that would collide under the new unique constraint {name}"),
        )?]),

        Change::SetPrimaryKey {
            table,
            to: Some(pk),
            ..
        } => {
            let mut out = Vec::new();
            for column in &pk.columns {
                out.push(null_probe(&table.column(column))?);
            }
            out.push(duplicate_probe(
                table,
                &pk.columns,
                "rows that would collide under the new primary key",
            )?);
            Ok(out)
        }

        Change::AddForeignKey {
            table,
            name,
            constraint,
        } => {
            // A row with any NULL in the key is exempt from the constraint
            // (MATCH SIMPLE, which is what SQL Server implements), so excluding
            // them is not leniency — it is the rule.
            let not_null: Vec<String> = constraint
                .columns
                .iter()
                .map(|c| Ok(format!("c.{} IS NOT NULL", quote(c)?)))
                .collect::<Result<_, DialectError>>()?;
            let join: Vec<String> = constraint
                .columns
                .iter()
                .zip(&constraint.references_columns)
                .map(|(child, parent)| Ok(format!("p.{} = c.{}", quote(parent)?, quote(child)?)))
                .collect::<Result<_, DialectError>>()?;

            Ok(vec![Probe::new(
                format!("rows with no matching parent for the new foreign key {name}"),
                format!(
                    "SELECT COUNT(*) AS n FROM {} AS c\n WHERE {}\n   AND NOT EXISTS (SELECT 1 FROM {} AS p WHERE {});",
                    qualified(table)?,
                    not_null.join("\n   AND "),
                    qualified(&constraint.references_table)?,
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
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { to: None, .. }
        | Change::DropUnique { .. }
        | Change::DropForeignKey { .. }
        | Change::DropCheck { .. }
        | Change::AddIndex { .. }
        | Change::DropIndex { .. } => Ok(Vec::new()),
    }
}

fn null_probe(column: &ColumnRef) -> Result<Probe, DialectError> {
    Ok(Probe::new(
        format!("existing NULLs in {column}, which NOT NULL would reject"),
        format!(
            "SELECT COUNT(*) AS n FROM {} WHERE {} IS NULL;",
            qualified(&column.table)?,
            quote(&column.name)?
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
fn conversion_probe(column: &ColumnRef, to: &ColumnType) -> Result<Vec<Probe>, DialectError> {
    let table = qualified(&column.table)?;
    let col = quote(&column.name)?;

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
        CheckConstraint, ForeignKey, PrimaryKey, ReferentialAction, UniqueConstraint,
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
    fn sql_of(change: &Change) -> Vec<String> {
        probes(change).into_iter().map(|p| p.sql).collect()
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

    /// Every probe must be a counting query returning one column called `n`;
    /// the executor reads it positionally and a probe that returned rows
    /// instead of a count would be read as garbage.
    #[test]
    fn every_probe_counts() {
        let changes = [
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
        ];
        for change in &changes {
            for probe in probes(change) {
                assert!(probe.sql.contains(" AS n"), "{probe:?}");
                assert!(probe.sql.trim_end().ends_with(';'), "{probe:?}");
                assert!(!probe.description.is_empty(), "{probe:?}");
            }
        }
    }
}
