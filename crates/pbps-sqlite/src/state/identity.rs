//! The environment identifier: which lineage of a database this is.
//!
//! A path cannot name an environment, because a file is copied and moved, so the
//! identifier is written into the file when its ledger is created and read back
//! from there. It is one row of its own table, never altered, and not a column of
//! the state table (which `prune` trims and which would need an `ALTER` to gain
//! one) and not `application_id` or `user_version` (a 31-bit number each, and the
//! fields an application already uses for itself).
//!
//! **It follows a copy.** A byte copy of the file, a `VACUUM INTO` and a restore
//! from a backup all carry the row, so two copies of one database share an
//! identifier until someone makes one a new environment. It names a lineage, not
//! a file.
//!
//! It is made by SQLite (`randomblob(16)`), 32 lowercase hexadecimal characters,
//! so no dependency or clock is involved.

use pbps_db::DbError;
use pbps_db::sqlite::Connection;

use super::busy::{BUSY_WAIT, explained, waiting};
use super::ddl::{occupants, refuse_a_stranger};
use super::problems::refuse_untrusted;
use super::{ENVIRONMENT_TABLE, ENVIRONMENT_TABLE_NAME};

/// The identifier's length, in characters.
pub const ENVIRONMENT_ID_CHARS: usize = 32;

/// This database's environment identifier.
///
/// - `Ok(Some(id))`: it has one.
/// - `Ok(None)`: none has been written, because there is no ledger yet or because
///   the ledger was made before the identifier existed. A read never invents it:
///   the next write that creates the missing table gives the ledger one.
/// - `Err`: the table is there and is not what it should be (no row, a value that
///   is not an identifier, something that is not a table in its name). That is
///   damage and is never read as absence.
pub fn environment_identity(c: &Connection) -> Result<Option<String>, DbError> {
    waiting(c, BUSY_WAIT, || {
        // This read asks the integrity check, though the state reads do not: a
        // user's own table of this name with this column would otherwise answer
        // as the identity (measured), and that answer is what an environment is
        // known by.
        refuse_untrusted(c, BUSY_WAIT)?;
        let held = occupants(c, &[ENVIRONMENT_TABLE_NAME]).map_err(|e| explained(e, BUSY_WAIT))?;
        refuse_a_stranger(&held)?;
        if held.is_empty() {
            return Ok(None);
        }
        let rows = c
            .query(
                &format!("SELECT environment_id FROM {ENVIRONMENT_TABLE} WHERE id = 1"),
                &[],
            )
            .map_err(|e| explained(e, BUSY_WAIT))?;
        let Some(row) = rows.first() else {
            return Err(DbError::BadRow(
                "the environment table has no row: it was emptied by hand, and this tool will \
                 not invent a new identity for a database that had one"
                    .into(),
            ));
        };
        let id = row.text(0)?;
        let readable = id.len() == ENVIRONMENT_ID_CHARS
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !readable {
            return Err(DbError::BadRow(
                "the environment table holds a value that is not an identifier".into(),
            ));
        }
        Ok(Some(id.to_owned()))
    })
}
