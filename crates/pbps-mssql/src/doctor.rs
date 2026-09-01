//! Readiness questions only a connection can answer (SPEC §14.1).
//!
//! # Why the permissions are asked for rather than tried
//!
//! The obvious way to find out whether the deployment account can create the
//! ledger is to create it. That is a write, and `doctor` is the command someone
//! runs when they are not yet sure what they are pointed at — quite possibly
//! production. So this asks the server what the account is allowed to do and
//! reports the gap, which costs one read and cannot change anything.
//!
//! # Why the list is here and not in the CLI
//!
//! Which permissions a deployment needs is dialect knowledge: PostgreSQL's
//! answer is a different vocabulary against a different catalog. The CLI asks
//! "is this environment ready", and each dialect answers in its own terms.

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError};

use crate::catalog::get;

/// A database-scoped permission pbps needs, and the command that needs it.
///
/// Named individually rather than as "db_owner": an organization that grants
/// the deployment account exactly what it needs should be able to see the list,
/// and "make it an owner" is the advice that makes every such organization say
/// no to the tool.
pub const REQUIRED: [(&str, &str); 9] = [
    (
        "VIEW DEFINITION",
        "reading the catalog: pull, plan --db, verify",
    ),
    (
        "SELECT",
        "the pre-flight probes, which count rows that would break",
    ),
    (
        "CREATE TABLE",
        "creating __pbps_state and __pbps_lock on first use",
    ),
    ("ALTER", "every change to a table in a schema pbps manages"),
    (
        "INSERT",
        "recording a state snapshot, and taking the deployment lock",
    ),
    // The worst gap to be missing, and the easiest to overlook: `apply` takes
    // the lock with INSERT and releases it with DELETE. Without this the schema
    // change commits and *then* the release fails, leaving a stale lock that
    // blocks the next pipeline — which is precisely the failure `doctor` exists
    // to catch beforehand. `state prune` needs it too.
    (
        "DELETE",
        "releasing the deployment lock after an apply, and `state prune`",
    ),
    // SQL Server gates each module kind on its own database-level CREATE, on
    // top of ALTER on the schema, so `CREATE OR ALTER VIEW` needs CREATE VIEW
    // even when the object already exists. A trigger is the exception and is
    // deliberately absent: a DML trigger is authorized by ALTER on the table it
    // is on, which is already required above.
    //
    // Demanded even of a project that declares no modules. `doctor` answers
    // "can I deploy from here", and the cost of asking for a permission that
    // goes unused is one line in a grant script; the cost of the other mistake
    // is an apply that fails on the day someone adds their first view.
    ("CREATE VIEW", "creating or restating a declared view"),
    (
        "CREATE PROCEDURE",
        "creating or restating a declared stored procedure",
    ),
    (
        "CREATE FUNCTION",
        "creating or restating a declared function",
    ),
];

/// The database-scoped permissions the connected account effectively holds.
///
/// `fn_my_permissions` is the effective set — it already accounts for role
/// membership, so an account that is `db_owner` comes back holding everything
/// rather than holding one role name this code would then have to interpret.
pub async fn permissions(conn: &mut Conn) -> Result<BTreeSet<String>, DbError> {
    let rows = conn
        .query("SELECT permission_name AS name FROM sys.fn_my_permissions(NULL, 'DATABASE');")
        .await?;
    let mut out = BTreeSet::new();
    for row in &rows {
        let name: &str = get(row, "name")?;
        out.insert(name.trim().to_ascii_uppercase());
    }
    Ok(out)
}

/// Which of [`REQUIRED`] the account does not hold.
///
/// `CONTROL` short-circuits the whole list: it implies every permission below
/// it, and an account that holds it would otherwise be reported as missing the
/// whole list while being able to do all of it.
pub fn missing(held: &BTreeSet<String>) -> Vec<(&'static str, &'static str)> {
    if held.contains("CONTROL") {
        return Vec::new();
    }
    REQUIRED
        .into_iter()
        .filter(|(name, _)| !held.contains(*name))
        .collect()
}

/// The server's own version banner, for the report.
pub async fn server_version(conn: &mut Conn) -> Result<String, DbError> {
    let rows = conn
        .query(
            "SELECT CONVERT(nvarchar(128), SERVERPROPERTY('ProductVersion')) AS version, \
             CONVERT(nvarchar(128), SERVERPROPERTY('ProductLevel')) AS level;",
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(DbError::BadRow("`server_version` returned no row".into()));
    };
    let version: &str = get(row, "version")?;
    let level: &str = get(row, "level")?;
    Ok(format!("{} {}", version.trim(), level.trim())
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn an_account_holding_everything_is_missing_nothing() {
        let held: BTreeSet<String> = REQUIRED.iter().map(|(n, _)| (*n).to_owned()).collect();
        assert!(missing(&held).is_empty());
    }

    /// The report names what is missing *and what it is for*: "you lack ALTER"
    /// sends someone to a DBA, "you lack ALTER, which every table change needs"
    /// lets them ask for the right thing once.
    #[test]
    fn a_missing_permission_is_reported_with_its_reason() {
        let held = set(&["VIEW DEFINITION", "SELECT"]);
        let gaps = missing(&held);
        assert_eq!(gaps.len(), REQUIRED.len() - 2);
        assert!(gaps.iter().any(|(n, why)| *n == "ALTER" && !why.is_empty()));
    }

    /// An account that can take the lock but not release it is the dangerous
    /// shape: `apply` commits the schema change and only then fails, leaving a
    /// stale lock. `doctor` has to name it before the deployment, not after.
    #[test]
    fn holding_insert_without_delete_is_still_a_gap() {
        let mut held: BTreeSet<String> = REQUIRED.iter().map(|(n, _)| (*n).to_owned()).collect();
        held.remove("DELETE");
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].0, "DELETE");
    }

    /// A trigger is authorized by ALTER on the table it is on, not by a CREATE
    /// of its own. Demanding one would send an organization to grant a
    /// permission its deployment does not use — and this list only stays
    /// credible if every entry on it is really needed.
    #[test]
    fn only_the_three_module_kinds_that_need_a_create_have_one() {
        let creates: Vec<&str> = REQUIRED
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| n.starts_with("CREATE "))
            .collect();
        assert_eq!(
            creates,
            [
                "CREATE TABLE",
                "CREATE VIEW",
                "CREATE PROCEDURE",
                "CREATE FUNCTION"
            ]
        );
    }

    /// CONTROL implies the rest. Reporting an owner as missing every entry
    /// would be the check crying wolf on the most common setup there is.
    #[test]
    fn control_alone_satisfies_the_list() {
        assert!(missing(&set(&["CONTROL"])).is_empty());
    }

    /// The negative case: an empty answer is a real state — a login mapped to
    /// no user in this database — and must not be mistaken for "fine".
    #[test]
    fn holding_nothing_is_reported_as_missing_everything() {
        assert_eq!(missing(&BTreeSet::new()).len(), REQUIRED.len());
    }
}
