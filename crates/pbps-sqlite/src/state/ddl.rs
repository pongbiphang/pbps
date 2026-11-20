//! The recipe the ledger's tables are created from, and creating them.

use std::time::Duration;

use pbps_db::DbError;
use pbps_db::ledger::STATE_TABLE_NAME;
use pbps_db::sqlite::{Connection, Value};
use pbps_model::escape::Escaped;

use super::busy::{self, BUSY_WAIT};

/// One table of the ledger: its name and the text of its `CREATE` from the name
/// on.
///
/// The body, and not a whole statement, because that is what the engine keeps:
/// it stores the text of `CREATE TABLE IF NOT EXISTS main.t (...)` as
/// `CREATE TABLE t (...)`, with neither `IF NOT EXISTS` nor `main.` (measured).
/// So [`Recipe::stored`] is what the catalog must read back for a table this tool
/// made, and a comparison of the two is a string equality.
pub(crate) struct Recipe {
    pub(crate) name: &'static str,
    pub(crate) body: &'static str,
}

impl Recipe {
    /// The statement that creates the table when it is not there.
    pub(crate) fn create(&self) -> String {
        format!("CREATE TABLE IF NOT EXISTS main.{}", self.body)
    }

    /// The text the engine stores for the table once created. Read only by the
    /// tests until the integrity check compares live tables with it.
    #[cfg(test)]
    pub(crate) fn stored(&self) -> String {
        format!("CREATE TABLE {}", self.body)
    }
}

/// `AUTOINCREMENT` so that an id is never reused: `latest`, the revision the
/// plan pins and the prune boundary all read a bigger id as a newer entry, and a
/// plain integer key hands the id of a deleted newest row to the next insert
/// (measured).
///
/// `TEXT` with a `strftime` default for the time: UTC, ISO 8601 with three
/// decimals, which is the text the other engines return, so nothing converts it
/// and no date library is needed. Epoch integers would need a render step and
/// could not be read by a person. Two statements in one transaction can share a
/// millisecond, so `applied_at` is a record of when and `id` is the order.
///
/// The widths are `CHECK`s on characters (see `REASON_CHARS`): SQLite ignores the
/// length in a type. `plan_checksum` is at most 64, not exactly 64, as on
/// PostgreSQL, so a shorter value is taken where that engine takes it.
const STATE_BODY: &str = "__pbps_state (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    applied_at       TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
    kind             TEXT    NOT NULL CHECK (length(kind) <= 16),
    git_sha          TEXT    CHECK (length(git_sha) <= 40),
    plan_checksum    TEXT    CHECK (length(plan_checksum) <= 64),
    state_json       TEXT    NOT NULL,
    operator         TEXT    NOT NULL CHECK (length(operator) <= 128),
    reason           TEXT    CHECK (length(reason) <= 1000),
    state_version    INTEGER,
    tables_count     INTEGER,
    modules_count    INTEGER,
    staged_completed INTEGER,
    staged_total     INTEGER
)";

/// Every table of the ledger. A later part adds a table here and
/// [`ensure_tables`], [`is_initialized`] and the refusal below take it without
/// another code path.
pub(crate) const RECIPE: [Recipe; 1] = [Recipe {
    name: STATE_TABLE_NAME,
    body: STATE_BODY,
}];

/// The name of the savepoint every unit of ledger work runs under.
const UNIT: &str = "pbps_ledger";

/// Whether the ledger's state table is there.
///
/// `true` for an ordinary table, `false` for none, and an error when something
/// else holds the name: a view there is not an absent ledger that a `bootstrap`
/// could create, and reading it as one is the direction that ends in writing
/// over an object this tool did not make.
///
/// Asked of the catalog and not by trying a statement, because the engine has no
/// code for "no such table" (it is the generic error 1, with text only), and
/// `main.sqlite_schema` is always readable.
pub fn is_initialized(c: &Connection) -> Result<bool, DbError> {
    busy::wait_up_to(c, BUSY_WAIT)?;
    let held = occupants(c, &[STATE_TABLE_NAME]).map_err(|e| busy::explained(e, BUSY_WAIT))?;
    refuse_a_stranger(&held)?;
    Ok(!held.is_empty())
}

/// Creates the ledger's tables that are not there yet.
///
/// **The catalog is asked first, and the DDL is sent only when something is
/// missing.** The probe is needed for the check below whatever else it does, and
/// it reads `main.sqlite_schema`, which takes no lock. It is not needed to keep
/// `CREATE TABLE IF NOT EXISTS` from writing: on the bundled 3.54.1 that statement
/// on a table that is there takes no write lock (measured), though on 3.46.1 and
/// 3.53.1 it does and fails while another connection is writing. So no test can
/// tell a skipped statement from a sent one here, and nothing relies on it. A
/// creator that raced this one between the probe and the statement is harmless:
/// `IF NOT EXISTS` makes the second a no-op.
///
/// **A name held by anything but an ordinary table is refused**, after the same
/// probe: `CREATE TABLE IF NOT EXISTS` over a view of the name does nothing and
/// reports no error (measured), so without the check this would return `Ok` over
/// a database with no ledger.
///
/// The missing tables are created in one savepoint, so a half-made ledger cannot
/// exist, and a savepoint and not `BEGIN` so that this behaves the same inside an
/// apply's transaction (a `record` runs inside it) and outside one (a `lock`
/// runs before it), and neither commits what the caller has not.
pub fn ensure_tables(c: &Connection) -> Result<(), DbError> {
    ensure_tables_within(c, BUSY_WAIT)
}

/// [`ensure_tables`] with the wait given, so a test need not wait five seconds.
pub(crate) fn ensure_tables_within(c: &Connection, wait: Duration) -> Result<(), DbError> {
    busy::wait_up_to(c, wait)?;
    let names: Vec<&str> = RECIPE.iter().map(|r| r.name).collect();
    let held = occupants(c, &names).map_err(|e| busy::explained(e, wait))?;
    refuse_a_stranger(&held)?;
    let missing: Vec<&Recipe> = RECIPE
        .iter()
        .filter(|r| {
            !held
                .iter()
                .any(|(name, _)| crate::ident::same(name, r.name))
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    create(c, &missing).map_err(|e| busy::explained(e, wait))
}

/// Creates `missing` in one unit, and confirms inside it that each is now an
/// ordinary table.
///
/// The probe that found them missing took no lock, so another connection can
/// have made a view of a name since, and `CREATE TABLE IF NOT EXISTS` over a
/// view succeeds and makes nothing. The catalog is therefore read again after
/// the statements, in the same unit: a stranger is refused as it is when found
/// first, and the unit is undone, so no part of a ledger is left beside it.
pub(super) fn create(c: &Connection, missing: &[&Recipe]) -> Result<(), DbError> {
    in_a_unit(c, || {
        for recipe in missing {
            c.execute_batch(&recipe.create())?;
        }
        let names: Vec<&str> = missing.iter().map(|r| r.name).collect();
        let held = occupants(c, &names)?;
        refuse_a_stranger(&held)?;
        match names.iter().find(|name| {
            !held
                .iter()
                .any(|(found, _)| crate::ident::same(found, name))
        }) {
            None => Ok(()),
            Some(name) => Err(DbError::BadRow(format!(
                "the ledger table `{}` was created and the catalog does not list it",
                Escaped(name)
            ))),
        }
    })
}

/// Runs `work` under a savepoint: released when it succeeds, undone when it does
/// not, and the connection left as the caller had it either way.
///
/// Outside a transaction a savepoint opens one and releasing it commits, so a
/// failed release there (a reader in the journal mode where readers block a
/// commit) leaves a transaction this function opened; it is rolled back. Inside
/// one it only nests, and undoing it leaves the caller's transaction open.
pub(super) fn in_a_unit<T>(
    c: &Connection,
    work: impl FnOnce() -> Result<T, DbError>,
) -> Result<T, DbError> {
    let nested = c.in_transaction();
    c.execute_batch(&format!("SAVEPOINT {UNIT}"))?;
    let outcome = work().and_then(|value| {
        c.execute_batch(&format!("RELEASE {UNIT}"))?;
        Ok(value)
    });
    if outcome.is_err() {
        // Its own failure is dropped so it cannot replace the one that caused it.
        let _ = if nested {
            c.execute_batch(&format!("ROLLBACK TO {UNIT}; RELEASE {UNIT}"))
        } else {
            c.execute_batch("ROLLBACK")
        };
    }
    outcome
}

/// The `(name, kind)` of every catalog entry of `main` that has one of `names`,
/// ASCII case ignored as SQLite ignores it, sorted by name.
fn occupants(c: &Connection, names: &[&str]) -> Result<Vec<(String, String)>, DbError> {
    let slots = (1..=names.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let params: Vec<Value> = names.iter().map(|n| Value::Text((*n).to_owned())).collect();
    c.query(
        &format!(
            "SELECT name, type FROM main.sqlite_schema \
             WHERE name COLLATE NOCASE IN ({slots}) ORDER BY name"
        ),
        &params,
    )?
    .iter()
    .map(|row| Ok((row.text(0)?.to_owned(), row.text(1)?.to_owned())))
    .collect()
}

/// Refuses when any of `held` is not an ordinary table.
///
/// This catches a view, an index, a trigger and the like. A virtual table is
/// also `table` here and is a difference the integrity check compares by text.
fn refuse_a_stranger(held: &[(String, String)]) -> Result<(), DbError> {
    let strangers: Vec<String> = held
        .iter()
        .filter(|(_, kind)| kind != "table")
        .map(|(name, kind)| {
            format!(
                "`{}` is {} {}, not an ordinary table",
                Escaped(name),
                if kind == "index" { "an" } else { "a" },
                Escaped(kind)
            )
        })
        .collect();
    if strangers.is_empty() {
        return Ok(());
    }
    Err(DbError::Refused(format!(
        "{}.\nA ledger name is occupied by an object this tool did not create, and a ledger \
         cannot be made beside it. Rename or drop that object, or point this tool at a \
         database where the ledger names are free.",
        strangers.join("; ")
    )))
}
