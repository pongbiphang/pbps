//! What a system-versioned table may carry on the connected server (#1502).
//!
//! SQL Server 2016 has system versioning but neither history retention nor a
//! cascading foreign key from a system-versioned table; 2017 added both. The
//! emitter writes what the declaration says, so on a 2016 target a finite
//! `retention` or such a key reaches the engine and is refused there, after
//! the statements ordered before it. Like the edition (ADR-0003), this is a
//! fact only a connection has, so `plan --db`, `apply` and `bootstrap` ask the
//! server before the first statement.
//!
//! The server is asked the reader's own question, whether `sys.tables` has
//! the retention column, rather than read off its version banner: Azure's
//! banners say 12.x on engines that have it (see `catalog::introspect`).

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError};
use pbps_model::{Change, ChangeSet, ReferentialAction, TableName};

use crate::catalog::get;

/// The probe the reader asks before it reads the retention columns, shared so
/// that the reader and this check cannot come to disagree about one server.
pub(crate) const RETENTION_PROBE: &str = "CASE WHEN COL_LENGTH('sys.tables', \
     'history_retention_period') IS NULL THEN 0 ELSE 1 END";

fn has_retention_query() -> String {
    format!("SELECT CONVERT(bit, {RETENTION_PROBE}) AS has_retention;")
}

/// Whether the server keeps a finite history retention, and with it takes a
/// cascading foreign key from a system-versioned table: false on 2016.
pub async fn has_history_retention(conn: &mut Conn) -> Result<bool, DbError> {
    let rows = conn.query(&has_retention_query()).await?;
    let row = rows
        .first()
        .ok_or_else(|| DbError::BadRow("the retention probe returned no row".into()))?;
    get(row, "has_retention")
}

/// The system-versioned tables the database holds now, by name.
pub async fn system_versioned_tables(conn: &mut Conn) -> Result<BTreeSet<TableName>, DbError> {
    let rows = conn
        .query(
            "SELECT SCHEMA_NAME(t.schema_id) AS schema_name, t.name AS table_name
               FROM sys.tables t
              WHERE t.temporal_type = 2;",
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(TableName::new(
                get::<&str>(row, "schema_name")?,
                get::<&str>(row, "table_name")?,
            ))
        })
        .collect()
}

/// Whether the plan asks anything of a server without retention, so that a
/// plan with no system-versioned table pays for no catalog read.
pub fn needs_the_question(changes: &ChangeSet) -> bool {
    changes.changes.iter().any(|p| {
        if let Change::CreateTable { table, .. } = &p.change {
            table
                .system_time
                .as_ref()
                .and_then(|st| st.versioning.as_ref())
                .is_some_and(|v| v.retention.is_some())
                || table.foreign_keys.values().any(cascades)
        } else if let Change::AddForeignKey { constraint, .. } = &p.change {
            cascades(constraint)
        } else {
            false
        }
    })
}

/// Whether the plan's statements need the cascading-key question answered
/// from the catalog: a cascading key added to a table this plan does not
/// create.
pub fn needs_the_catalog(changes: &ChangeSet) -> bool {
    let created = created_tables(changes);
    changes.changes.iter().any(|p| {
        matches!(&p.change, Change::AddForeignKey { table, constraint, .. }
            if cascades(constraint) && !created.contains_key(table))
    })
}

/// What a server without history retention refuses in this plan, each named.
///
/// `versioned` is the system-versioned tables the database holds before the
/// plan runs, by their names then. A key the plan adds runs after its renames
/// (ORDERING, class 13) under the table's final name, so that name is walked
/// back through the plan's renames to the one the catalog holds. A table the
/// plan creates answers from its own declaration instead.
pub fn refused_without_retention(
    changes: &ChangeSet,
    versioned: &BTreeSet<TableName>,
) -> Vec<String> {
    let created = created_tables(changes);
    let renames: Vec<(&TableName, &TableName)> = changes
        .changes
        .iter()
        .filter_map(|p| {
            let Change::RenameTable { from, to, .. } = &p.change else {
                return None;
            };
            Some((from, to))
        })
        .collect();
    let is_versioned = |name: &TableName| {
        if let Some(versioned) = created.get(name) {
            return *versioned;
        }
        let mut was = name;
        for (from, to) in renames.iter().rev() {
            if was == *to {
                was = from;
            }
        }
        versioned.contains(was)
    };
    let mut problems = Vec::new();
    for p in &changes.changes {
        if let Change::CreateTable { name, table, .. } = &p.change {
            if let Some(retention) = table
                .system_time
                .as_ref()
                .and_then(|st| st.versioning.as_ref())
                .and_then(|v| v.retention)
            {
                problems.push(format!(
                    "`{name}` declares a history retention of {} {}, and this server has \
                     none: SQL Server 2016 keeps every history row; leave `retention` out \
                     for this target",
                    retention.count,
                    retention.unit.keyword().to_ascii_lowercase()
                ));
            }
            for (key, fk) in &table.foreign_keys {
                if cascades(fk) && is_versioned(name) {
                    problems.push(cascade_refusal(name, key));
                }
            }
        } else if let Change::AddForeignKey {
            table,
            name,
            constraint,
        } = &p.change
            && cascades(constraint)
            && is_versioned(table)
        {
            problems.push(cascade_refusal(table, name));
        }
    }
    problems
}

fn cascade_refusal(table: &TableName, key: &str) -> String {
    format!(
        "foreign key `{key}` on `{table}` cascades, and `{table}` is system-versioned: SQL \
         Server 2016 refuses `ON DELETE CASCADE` and `ON UPDATE CASCADE` on a key from a \
         system-versioned table; use `no_action` for this target"
    )
}

fn cascades(fk: &pbps_model::ForeignKey) -> bool {
    fk.on_delete == ReferentialAction::Cascade || fk.on_update == ReferentialAction::Cascade
}

/// Each table the plan creates, by name, and whether it is system-versioned.
fn created_tables(changes: &ChangeSet) -> std::collections::BTreeMap<&TableName, bool> {
    changes
        .changes
        .iter()
        .filter_map(|p| {
            let Change::CreateTable { name, table, .. } = &p.change else {
                return None;
            };
            Some((
                name,
                table
                    .system_time
                    .as_ref()
                    .is_some_and(|st| st.versioning.is_some()),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{
        ForeignKey, PlannedChange, Retention, RetentionUnit, SystemTime, SystemVersioning, Table,
    };

    fn tname(s: &str) -> TableName {
        s.parse().unwrap()
    }

    fn versioned(retention: Option<Retention>) -> Table {
        Table {
            system_time: Some(SystemTime {
                start: "vf".into(),
                end: "vt".into(),
                hidden: false,
                versioning: Some(SystemVersioning {
                    history: tname("dbo.t_history"),
                    retention,
                }),
            }),
            ..Default::default()
        }
    }

    fn create(name: &str, table: Table) -> PlannedChange {
        PlannedChange::new(Change::CreateTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: tname(name),
            table: Box::new(table),
        })
    }

    fn key(table: &str, on_delete: ReferentialAction) -> PlannedChange {
        PlannedChange::new(Change::AddForeignKey {
            table: tname(table),
            name: "fk_t_p".into(),
            constraint: Box::new(ForeignKey {
                columns: vec!["p_id".into()],
                references_table: tname("dbo.p"),
                references_columns: vec!["id".into()],
                on_delete,
                on_update: ReferentialAction::NoAction,
            }),
        })
    }

    fn plan(changes: Vec<PlannedChange>) -> ChangeSet {
        ChangeSet { changes }
    }

    /// A finite retention is refused by name on a server without it, and an
    /// INFINITE one, which is what 2016 keeps, is not (#1502).
    #[test]
    fn only_a_finite_retention_is_refused_without_retention() {
        let finite = plan(vec![create(
            "dbo.t",
            versioned(Some(Retention {
                count: 3,
                unit: RetentionUnit::Day,
            })),
        )]);
        assert!(needs_the_question(&finite));
        let problems = refused_without_retention(&finite, &BTreeSet::new());
        assert!(
            problems.len() == 1
                && problems[0].contains("`dbo.t` declares a history retention of 3 day")
                && problems[0].contains("leave `retention` out"),
            "{problems:?}"
        );
        let infinite = plan(vec![create("dbo.t", versioned(None))]);
        assert!(!needs_the_question(&infinite));
        assert!(refused_without_retention(&infinite, &BTreeSet::new()).is_empty());
    }

    /// A cascading key from a system-versioned table is refused, whether the
    /// plan creates the table or the database already holds it under a name
    /// the plan's renames change; a non-cascading key there, and a cascading
    /// one from a plain table, are not (#1502).
    #[test]
    fn only_a_cascading_key_from_a_versioned_table_is_refused() {
        use ReferentialAction::{Cascade, NoAction, SetNull};
        // Created by this plan.
        let created = plan(vec![
            create("dbo.t", versioned(None)),
            key("dbo.t", Cascade),
        ]);
        assert!(needs_the_question(&created) && !needs_the_catalog(&created));
        let problems = refused_without_retention(&created, &BTreeSet::new());
        assert!(
            problems.len() == 1 && problems[0].contains("`fk_t_p` on `dbo.t` cascades"),
            "{problems:?}"
        );
        for action in [NoAction, SetNull] {
            let ok = plan(vec![create("dbo.t", versioned(None)), key("dbo.t", action)]);
            assert!(refused_without_retention(&ok, &BTreeSet::new()).is_empty());
        }
        let plain = plan(vec![
            create("dbo.t", Table::default()),
            key("dbo.t", Cascade),
        ]);
        assert!(refused_without_retention(&plain, &BTreeSet::new()).is_empty());

        // Already in the database, renamed by this plan: the chain
        // a -> b, b -> c leaves `b` naming the table that was `a`.
        let rename = |uid: &str, from: &str, to: &str| {
            PlannedChange::new(Change::RenameTable {
                uid: uid.parse().unwrap(),
                from: tname(from),
                to: tname(to),
                defaults: Vec::new(),
            })
        };
        let renamed = plan(vec![
            rename("t_bbbbbb", "dbo.b", "dbo.c"),
            rename("t_cccccc", "dbo.a", "dbo.b"),
            key("dbo.b", Cascade),
        ]);
        assert!(needs_the_catalog(&renamed));
        let versioned_a = BTreeSet::from([tname("dbo.a")]);
        assert_eq!(refused_without_retention(&renamed, &versioned_a).len(), 1);
        let versioned_b = BTreeSet::from([tname("dbo.b")]);
        assert!(
            refused_without_retention(&renamed, &versioned_b).is_empty(),
            "the original b is c by then"
        );
        // A created table at a name an existing versioned one vacated is the
        // created table's own answer.
        let reused = plan(vec![
            rename("t_bbbbbb", "dbo.b", "dbo.c"),
            create("dbo.b", Table::default()),
            key("dbo.b", Cascade),
        ]);
        assert!(refused_without_retention(&reused, &BTreeSet::from([tname("dbo.b")])).is_empty());
    }

    /// No 2016 server is in the live matrix, so the probe's text is pinned:
    /// it is the reader's own, and asks the catalog rather than the banner.
    #[test]
    fn the_capability_probe_is_the_readers_retention_probe() {
        let query = has_retention_query();
        assert!(
            query.contains("COL_LENGTH('sys.tables', 'history_retention_period') IS NULL"),
            "{query}"
        );
        assert!(query.contains("AS has_retention"), "{query}");
    }
}
