//! Measured offline ALTER COLUMN costs, never correctness risk or approval.
//!
//! SQL Server updates rows in place: unchanged allocation IDs do not prove a
//! metadata-only operation, and unchanged bytes can suppress log writes even
//! when every row is processed. The live catalogue matrix measures the update
//! and scan paths alongside log volume. Compression changes those paths.
//!
//! Only column type/nullability changes have measurements here. ONLINE and
//! unmeasured catalog shapes remain unknown. A whole plan supplies catalog
//! identities before renames and distinguishes newly created objects (D478).

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError};
pub use pbps_dialect::estimate::{Reads, Rewrite};
use pbps_model::{Change, ChangeSet, ColumnType, TableName, TypeArg};

/// The two measured row-storage encodings; PAGE uses the row encoding too.
#[derive(Debug, Clone, Copy)]
pub enum RowStorage {
    Uncompressed,
    Compressed,
}

/// The row work of a successful, offline ALTER COLUMN on an ordinary rowstore
/// table without column indexes or constraint proofs. Catalog validation and
/// planned identity belong to [`planned_estimates`] and [`against`].
pub fn column_work(
    from: &ColumnType,
    to: &ColumnType,
    from_nullable: bool,
    to_nullable: bool,
    storage: RowStorage,
) -> (Rewrite, Reads) {
    let (Ok(mut from), Ok(mut to)) = (crate::types::normalize(from), crate::types::normalize(to))
    else {
        return unknown("this dialect does not know one of the two types");
    };
    if from.base == "timestamp" || to.base == "timestamp" {
        return unknown("SQL Server refuses ALTER COLUMN involving timestamp/rowversion");
    }
    // sysname keeps its declaration spelling but has nvarchar(128)'s storage.
    for ty in [&mut from, &mut to] {
        if ty.base == "sysname" {
            *ty = ColumnType::new("nvarchar", vec![TypeArg::Int(128)]);
        }
    }
    let bounded_widening = matches!(
        (from.args.as_slice(), to.args.as_slice()),
        ([TypeArg::Int(a)], [TypeArg::Int(b)]) if b >= a
    );
    let metadata = (from == to && !matches!(from.base.as_str(), "geography" | "geometry"))
        || (from.base == to.base
            && matches!(from.base.as_str(), "varchar" | "nvarchar" | "varbinary")
            && bounded_widening)
        || (matches!(to.args.as_slice(), [TypeArg::Max])
            && matches!(
                (from.base.as_str(), to.base.as_str()),
                ("text", "varchar") | ("ntext", "nvarchar") | ("image", "varbinary")
            ))
        || (matches!(storage, RowStorage::Compressed)
            && (matches!(
                (from.base.as_str(), to.base.as_str()),
                ("smallint", "int" | "bigint") | ("int", "bigint") | ("smallmoney", "money")
            ) || (bounded_widening
                && matches!(
                    (from.base.as_str(), to.base.as_str()),
                    ("char", "char" | "varchar")
                        | ("nchar", "nchar" | "nvarchar")
                        | ("binary", "binary" | "varbinary")
                ))));
    // Unlike PostgreSQL's SET NOT NULL, this engine takes its row-update path
    // when tightening nullability, including a type widening otherwise free.
    if metadata && !(from_nullable && !to_nullable) {
        (Rewrite::No, Reads::Nothing)
    } else {
        (Rewrite::Yes, Reads::EveryRow)
    }
}

fn unknown(why: &str) -> (Rewrite, Reads) {
    (
        Rewrite::Unknown(why.to_owned()),
        Reads::Unknown(why.to_owned()),
    )
}

#[derive(Debug, Clone)]
pub struct Estimate {
    pub about: String,
    pub table: TableName,
    pub rewrite: Rewrite,
    pub reads: Reads,
    pub lock: &'static str,
    pub blocks: &'static str,
    pub rows: Option<i64>,
    pub rows_unknown: Option<String>,
    // A created identity or an earlier retype has no measured stored context.
    source: Option<(TableName, String)>,
    from: ColumnType,
    to: ColumnType,
    from_nullable: bool,
    to_nullable: bool,
    online: bool,
}

impl Estimate {
    fn unknown(&mut self, why: &str) {
        (self.rewrite, self.reads) = unknown(why);
    }
}

/// Keep original change indices, catalog names and each statement's strategy.
pub fn planned_estimates(changes: &ChangeSet) -> Vec<(usize, Estimate)> {
    let mut identities = BTreeMap::new();
    let mut columns = BTreeMap::new();
    for p in &changes.changes {
        if let Change::CreateTable { uid, .. } = &p.change {
            identities.insert(uid, None);
        }
        if let Change::AddColumn { uid, .. } = &p.change {
            columns.insert(uid, None);
        }
    }
    for p in &changes.changes {
        if let Change::RenameTable { uid, from, .. } = &p.change {
            identities.entry(uid).or_insert_with(|| Some(from.clone()));
        }
        if let Change::RenameColumn { uid, from, .. } = &p.change {
            columns.entry(uid).or_insert_with(|| Some(from.clone()));
        }
    }
    let mut tables = BTreeMap::new();
    for p in &changes.changes {
        if let Change::CreateTable { uid, name, .. } | Change::RenameTable { uid, to: name, .. } =
            &p.change
        {
            tables.insert(name, identities[uid].clone());
        }
    }
    let mut retyped = BTreeSet::new();
    changes
        .changes
        .iter()
        .enumerate()
        .filter_map(|(index, p)| {
            let (uid, column, from, to, from_nullable, to_nullable) = match &p.change {
                Change::AlterColumnType {
                    uid,
                    column,
                    from,
                    to,
                    from_nullable,
                    to_nullable,
                } => (uid, column, from, to, *from_nullable, *to_nullable),
                Change::AlterColumnNullability {
                    uid,
                    column,
                    ty,
                    to_nullable,
                } => (uid, column, ty, ty, !*to_nullable, *to_nullable),
                Change::CreateTable { .. }
                | Change::DropTable { .. }
                | Change::RenameTable { .. }
                | Change::AddColumn { .. }
                | Change::DropColumn { .. }
                | Change::RenameColumn { .. }
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
                | Change::InsertRow { .. }
                | Change::UpdateRow { .. }
                | Change::DeleteRow { .. }
                | Change::SetDataMode { .. }
                | Change::CreateModule { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. }
                | Change::PublicExecution { .. } => return None,
            };
            let source = tables
                .get(&column.table)
                .cloned()
                .unwrap_or_else(|| Some(column.table.clone()))
                .zip(
                    columns
                        .get(uid)
                        .cloned()
                        .unwrap_or_else(|| Some(column.name.clone())),
                )
                .filter(|_| retyped.insert(uid));
            let (rewrite, reads) =
                unknown("the table's stored row encoding has not been measured yet");
            Some((
                index,
                Estimate {
                    about: format!(
                        "changing {column} to {to} {}",
                        if to_nullable { "NULL" } else { "NOT NULL" }
                    ),
                    table: column.table.clone(),
                    rewrite,
                    reads,
                    lock: if p.strategy.online {
                        "unknown (ONLINE ALTER COLUMN is unmeasured)"
                    } else {
                        "Sch-M"
                    },
                    blocks: if p.strategy.online {
                        "unknown"
                    } else {
                        "reads and writes"
                    },
                    rows: None,
                    rows_unknown: None,
                    source,
                    from: from.clone(),
                    to: to.clone(),
                    from_nullable,
                    to_nullable,
                    online: p.strategy.online,
                },
            ))
        })
        .collect()
}

// Catalog views suffice for the product. Diagnostic update/log counters belong
// to the live measurement only, so planning needs no server-performance grant.
const SHAPE: &str = "\
SELECT CONVERT(int, SERVERPROPERTY('ProductMajorVersion')) AS major,
       t.is_memory_optimized, t.temporal_type, c.column_id,
       (SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id=t.object_id AND p.index_id IN (0,1)) AS row_count,
       (SELECT COUNT_BIG(*) FROM sys.partitions p WHERE p.object_id=t.object_id AND p.index_id IN (0,1)) AS partitions,
       (SELECT MAX(p.data_compression) FROM sys.partitions p WHERE p.object_id=t.object_id AND p.index_id IN (0,1)) AS compression,
       CONVERT(bit, CASE WHEN EXISTS (SELECT 1 FROM sys.indexes i WHERE i.object_id=t.object_id AND i.type NOT IN (0,1,2))
                         THEN 1 ELSE 0 END) AS special_index,
       CONVERT(bit, CASE WHEN EXISTS (SELECT 1 FROM sys.index_columns i WHERE i.object_id=t.object_id AND i.column_id=c.column_id)
                          OR EXISTS (SELECT 1 FROM sys.check_constraints k WHERE k.parent_object_id=t.object_id)
                          OR EXISTS (SELECT 1 FROM sys.foreign_key_columns f WHERE (f.parent_object_id=t.object_id AND f.parent_column_id=c.column_id)
                                      OR (f.referenced_object_id=t.object_id AND f.referenced_column_id=c.column_id))
                         THEN 1 ELSE 0 END) AS dependencies
  FROM sys.tables t LEFT JOIN sys.columns c ON c.object_id=t.object_id AND c.name=@P2
 WHERE t.object_id=OBJECT_ID(@P1);";

/// Fill advisory catalog context. Failure is reported by the caller as an
/// unavailable estimate; it must never refuse a valid plan (SPEC 14.1).
pub async fn against(conn: &mut Conn, estimate: &mut Estimate) -> Result<(), DbError> {
    let Some((table, column)) = &estimate.source else {
        let why =
            "this plan creates or already retypes this identity; its future storage is unmeasured";
        estimate.rows_unknown = Some(why.to_owned());
        estimate.unknown(why);
        return Ok(());
    };
    let relation = match crate::emit::qualified(table) {
        Ok(name) => name,
        Err(_) => {
            estimate.unknown("this table's name cannot be written as an identifier");
            return Ok(());
        }
    };
    let rows = conn
        .query_with(SHAPE, &[relation.as_str().into(), column.as_str().into()])
        .await?;
    let Some(row) = rows.first() else {
        let why = "this database has no visible table by that name to measure";
        estimate.rows_unknown = Some(why.to_owned());
        estimate.unknown(why);
        return Ok(());
    };
    estimate.rows = row.try_get::<i64>("row_count")?;
    if estimate.rows.is_none() {
        estimate.rows_unknown = Some("no catalog row estimate is available".to_owned());
    }
    let compression = row.try_get::<u8>("compression")?;
    let why = if estimate.online {
        Some("ONLINE ALTER COLUMN has not been measured")
    } else if row.try_get::<i32>("major")? != Some(17) {
        Some(
            "this SQL Server version has not been measured; the catalogue matrix covers version 17",
        )
    } else if row.try_get::<i32>("column_id")?.is_none() {
        Some("this database has no visible column by that name to measure")
    } else if row.try_get::<bool>("is_memory_optimized")? != Some(false)
        || row.try_get::<u8>("temporal_type")? != Some(0)
        || row.try_get::<bool>("special_index")? != Some(false)
        || row.try_get::<i64>("partitions")? != Some(1)
        || !matches!(compression, Some(0..=2))
    {
        Some(
            "this table's storage or partitioning is outside the measured ordinary rowstore shapes",
        )
    } else if row.try_get::<bool>("dependencies")? != Some(false) {
        Some("the catalog has index or constraint context not covered by the column measurements")
    } else {
        None
    };
    if let Some(why) = why {
        estimate.unknown(why);
    } else {
        let storage = if compression == Some(0) {
            RowStorage::Uncompressed
        } else {
            RowStorage::Compressed
        };
        (estimate.rewrite, estimate.reads) = column_work(
            &estimate.from,
            &estimate.to,
            estimate.from_nullable,
            estimate.to_nullable,
            storage,
        );
        if estimate.rewrite == Rewrite::Yes {
            estimate.about.push_str(" (in-place row updates)");
        }
    }
    Ok(())
}
