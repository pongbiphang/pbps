//! The role questions only a connection can answer
//! ([ADR-0010](../../../docs/ADR-0010-postgres-privileges.md) §3, §4, §6).
//!
//! Three of ADR-0010's rules cannot be checked against a declaration, and each
//! is here for a different reason:
//!
//! - **Does the cluster have this role?** The principal is not this tool's to
//!   create (`Dialect::manages_roles` is `false`), so a declared role has to
//!   exist already — and `pg_authid` is shared, so only a connection can say.
//! - **What is stopping a `DROP ROLE`?** More than ownership, and more than
//!   this database: measured, the engine refuses with `1 object in database
//!   otherdb` when every grant *here* has been revoked (§4).
//! - **Does the server have `maintain`?** It arrived in PostgreSQL 17, and the
//!   model holds no server version, so the gate is here rather than in
//!   `validate_role` — the same shape as `plan --db` gating on SQL Server's
//!   edition (ADR-0010 amendment).

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError, Param, Row};
use pbps_dialect::DialectError;
use pbps_model::{Permission, Role};

use crate::types::DIALECT;

/// The release that added `MAINTAIN`, as `server_version_num` spells it.
///
/// **Measured on both sides.** PostgreSQL 16.15 (`160015`):
/// `GRANT MAINTAIN ON t16 TO r16` is
/// `ERROR: unrecognized privilege type "maintain"`, and the owner's default
/// relation ACL is `{postgres=arwdDxt/postgres}` — no `m`. PostgreSQL 18.6
/// (`180006`) accepts the grant and the default is `arwdDxtm`.
pub const MAINTAIN_ARRIVED_IN: i64 = 170_000;

fn invalid(message: impl Into<String>) -> DialectError {
    DialectError::Invalid {
        dialect: DIALECT,
        message: message.into(),
    }
}

fn text(row: &Row, column: &str) -> Result<String, DbError> {
    optional_text(row, column)?.ok_or_else(|| null_column(column))
}

fn optional_text(row: &Row, column: &str) -> Result<Option<String>, DbError> {
    Ok(row.try_get::<&str>(column)?.map(str::to_owned))
}

fn number(row: &Row, column: &str) -> Result<i64, DbError> {
    row.try_get::<i64>(column)?
        .ok_or_else(|| null_column(column))
}

fn null_column(column: &str) -> DbError {
    DbError::BadRow(format!("column `{column}` is unexpectedly NULL"))
}

/// The connected server's `server_version_num`: `180006` for 18.6.
///
/// One integer rather than the three-part string, because the only questions
/// asked of it are comparisons and `version()` is prose.
pub async fn server_version_num(conn: &mut Conn) -> Result<i64, DbError> {
    let rows = conn
        .query("SELECT current_setting('server_version_num')::int8 AS num")
        .await?;
    number(rows.first().ok_or_else(|| null_column("num"))?, "num")
}

/// Every permission a declaration names that **this** server does not have.
///
/// Today that is `maintain` below PostgreSQL 17, and the list is written as a
/// list so that the next one is an entry rather than a rewrite. A format check
/// cannot do this: the model holds no server version, and the same declaration
/// is valid against one server and refused by the next.
///
/// Checked here rather than left to the apply because of what an apply is: the
/// grants sort after the objects they name, so the failing `GRANT` would run
/// on a database the rest of the plan had already changed.
pub fn unsupported_permissions(
    server_version_num: i64,
    name: &str,
    role: &Role,
) -> Vec<DialectError> {
    let mut errs = Vec::new();
    if server_version_num >= MAINTAIN_ARRIVED_IN {
        return errs;
    }
    for (target, permissions) in &role.grants {
        if permissions.contains(&Permission::Maintain) {
            errs.push(invalid(format!(
                "role `{name}`: `maintain` on `{target}` needs PostgreSQL 17 or later, and this \
                 server is {}; on an older one the engine answers `unrecognized privilege type \
                 \"maintain\"` — measured on 16.15 — which is a statement that would fail after \
                 everything ordered before it had run. Remove it, or upgrade the server",
                rendered(server_version_num)
            )));
        }
    }
    errs
}

/// `180006` as `18.6`, for a message.
fn rendered(server_version_num: i64) -> String {
    let major = server_version_num / 10_000;
    let minor = server_version_num % 10_000;
    format!("{major}.{minor}")
}

/// The declared roles the cluster does not have, in the order given.
///
/// The whole reason this is a connected question: `pg_authid` is shared by
/// every database in the cluster, so "does this role exist" is not something
/// this database's catalog owns, and creating one is not something a tool
/// whose blast radius is one database may do (ADR-0010 §3, DECISIONS 211).
///
/// `pg_roles` rather than `pg_authid`, so that a least-privileged deployment
/// account can ask: the second holds the password hashes and is
/// superuser-only.
pub async fn missing_roles(
    conn: &mut Conn,
    declared: &BTreeSet<&str>,
) -> Result<Vec<String>, DbError> {
    let mut out = Vec::new();
    for name in declared {
        let rows = conn
            .query_with(
                "SELECT 1 AS found FROM pg_catalog.pg_roles WHERE rolname = $1",
                &[Param::Str(name)],
            )
            .await?;
        if rows.is_empty() {
            out.push((*name).to_owned());
        }
    }
    Ok(out)
}

/// The refusal a caller renders for a role the cluster lacks, with the
/// statement to run.
///
/// Spelled out rather than described: "create the role first" sends whoever is
/// holding the plan to the manual, and the `CREATE ROLE` they can paste is the
/// difference between a refusal that blocks and one that unblocks.
pub fn refuse_missing(name: &str) -> DialectError {
    match crate::quote(name) {
        Ok(quoted) => invalid(format!(
            "role `{name}` is declared here and the cluster does not have it. A PostgreSQL role \
             is a cluster object, so pbps manages what a role is granted in this database and \
             not whether the role exists (ADR-0010 §3). Run it by hand, then plan again:\n\n    \
             CREATE ROLE {quoted};"
        )),
        Err(e) => e,
    }
}

/// What the cluster says about a role the declarations have renamed.
///
/// The elision in `pbps_diff` — no `RenameRole` where the principal is the
/// cluster's — is sound only behind this. Name existence is **not** identity:
/// if the old name is still there, `new` is some other principal, and a plan
/// that emitted nothing would leave the old role holding everything pbps was
/// managing while the declared one holds nothing, and then record the
/// declared one as holding it all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameEvidence {
    /// The old name is gone and the new one is there. The rename happened —
    /// and if instead the old role was dropped and a new one created, its
    /// grants went with it, so the pull shows the new role holding nothing and
    /// every declared grant is planned. Either way the plan is right.
    Done,
    /// Both names exist. Two principals, and pbps has no way to tell which one
    /// its declarations mean.
    BothPresent,
    /// Neither exists. Nothing to rename and nothing to grant on.
    NeitherPresent,
    /// The old name is there and the new one is not: the rename has not been
    /// run.
    NotRunYet,
}

/// Which of the four the cluster is in, for a declared rename `from` -> `to`.
pub async fn rename_evidence(
    conn: &mut Conn,
    from: &str,
    to: &str,
) -> Result<RenameEvidence, DbError> {
    let present = |missing: &[String], name: &str| !missing.iter().any(|m| m == name);
    let missing = missing_roles(conn, &[from, to].into_iter().collect()).await?;
    Ok(match (present(&missing, from), present(&missing, to)) {
        (false, true) => RenameEvidence::Done,
        (true, true) => RenameEvidence::BothPresent,
        (false, false) => RenameEvidence::NeitherPresent,
        (true, false) => RenameEvidence::NotRunYet,
    })
}

/// The refusal a caller renders for a rename the cluster has not performed.
///
/// `None` where the evidence is [`RenameEvidence::Done`] — there is nothing to
/// refuse, and the differ's silence about the rename is then correct.
#[must_use]
pub fn refuse_rename(from: &str, to: &str, evidence: RenameEvidence) -> Option<DialectError> {
    let (quoted_from, quoted_to) = match (crate::quote(from), crate::quote(to)) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(e), _) | (_, Err(e)) => return Some(e),
    };
    let why = match evidence {
        RenameEvidence::Done => return None,
        // The case that makes this check exist rather than `missing_roles`
        // alone: `to` exists, so a check that asked only "is the new name
        // there" would pass — and `to` is a different principal.
        RenameEvidence::BothPresent => format!(
            "both `{from}` and `{to}` exist in the cluster, so `{to}` is a different principal \
             and not `{from}` under a new name. pbps does not own the principal (ADR-0010 §3), \
             and it cannot tell which of the two its declarations mean: renaming the role would \
             move the grants, while granting to `{to}` would leave `{from}` holding everything \
             it holds now. Rename the role, or drop `{to}` if it was created by mistake"
        ),
        RenameEvidence::NeitherPresent => format!(
            "neither `{from}` nor `{to}` is in the cluster, so there is no principal to rename \
             and none to grant on (ADR-0010 §3)"
        ),
        RenameEvidence::NotRunYet => format!(
            "`{from}` is still in the cluster and `{to}` is not: the rename this revision \
             declares has not been run. A PostgreSQL role is a cluster object and pbps does not \
             own it (ADR-0010 §3); the grants follow the role's oid, so nothing has to be \
             re-granted afterwards"
        ),
    };
    Some(invalid(format!(
        "{why}. Run it by hand, then plan again:\n\n    ALTER ROLE {quoted_from} RENAME TO \
         {quoted_to};"
    )))
}

/// One reason the cluster will refuse `DROP ROLE`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DropBlocker {
    pub role: String,
    /// The database the dependent objects are in. `None` is a dependency on a
    /// shared object — a database, a tablespace — which belongs to no one
    /// database.
    pub database: Option<String>,
    /// `pg_shdepend.deptype`: `o` owns, `a` is granted, `r` is named by a
    /// row-level security policy, `t` a tablespace default, `i` an initial
    /// privilege recorded for an extension.
    pub deptype: char,
    pub objects: i64,
}

impl DropBlocker {
    /// What this blocker is, and what removes it — one sentence each, because
    /// the engine's own `DETAIL` says only `1 object in database otherdb`.
    ///
    /// `this_database` is `None` for a caller whose rows are all from
    /// elsewhere by construction — the pull's, which excludes this database's
    /// own — rather than a caller that does not know which database it is on.
    /// An empty string would have read as the second.
    #[must_use]
    pub fn rendered(&self, this_database: Option<&str>) -> String {
        let where_ = match (&self.database, this_database) {
            (Some(db), Some(here)) if db == here => "in this database".to_owned(),
            (Some(db), _) => format!("in database `{db}`, which this connection cannot read"),
            (None, _) => "on a shared object, which belongs to no one database".to_owned(),
        };
        let (what, remedy) = match self.deptype {
            'o' => (
                "owns",
                "`REASSIGN OWNED BY` to move them to another role, or `DROP OWNED BY` to destroy \
                 them",
            ),
            'a' => (
                "is granted a permission on",
                "`REVOKE` those permissions, or `DROP OWNED BY`, which removes a role's grants as \
                 well as what it owns",
            ),
            'r' => (
                "is named by a row-level security policy on",
                "`ALTER POLICY` to name another role, or drop the policy",
            ),
            _ => (
                "is depended on by",
                "`DROP OWNED BY`, which removes every dependency a role has in one database",
            ),
        };
        format!(
            "`{}` {what} {} {where_}; run {remedy} — in that database, since neither statement \
             reaches beyond the one it runs in (ADR-0010 §4)",
            self.role,
            plural(self.objects),
        )
    }
}

fn plural(n: i64) -> String {
    if n == 1 {
        "1 object".to_owned()
    } else {
        format!("{n} objects")
    }
}

/// Everything in the cluster that holds `role`, wherever it is.
///
/// **Measured, and this is the whole of §4.** With every grant *this* database
/// holds revoked, the engine still refuses:
///
/// ```text
/// DROP ROLE gr_reader;
///     ERROR:  role "gr_reader" cannot be dropped because some objects depend on it
///     DETAIL:  1 object in database otherdb
/// ```
///
/// and `pg_shdepend` is where that came from: one row per dependency, keyed by
/// the database it is in. The objects in another database cannot be named from
/// here — their oids belong to that database's catalog — so they are counted
/// and the database is named, which is exactly as far as one connection can
/// see. Reporting "nothing is stopping it" from this database's view would be
/// the answer that reads as good news and is wrong.
///
/// `pg_shdepend` is world-readable, so a least-privileged deployment account
/// gets the same answer a superuser does.
pub async fn drop_blockers(conn: &mut Conn, role: &str) -> Result<Vec<DropBlocker>, DbError> {
    let mut out = Vec::new();
    for row in conn
        .query_with(
            "SELECT d.datname AS in_database,
                    sd.deptype::text AS deptype,
                    count(*)::int8 AS objects
               FROM pg_catalog.pg_shdepend sd
               LEFT JOIN pg_catalog.pg_database d ON d.oid = sd.dbid
              WHERE sd.refclassid = 'pg_catalog.pg_authid'::regclass
                AND sd.refobjid = (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = $1)
              GROUP BY 1, 2
              ORDER BY 1, 2",
            &[Param::Str(role)],
        )
        .await?
    {
        out.push(DropBlocker {
            role: role.to_owned(),
            database: optional_text(&row, "in_database")?,
            deptype: text(&row, "deptype")?.chars().next().unwrap_or('?'),
            objects: number(&row, "objects")?,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::GrantTarget;

    fn role_with(permission: Permission) -> Role {
        let mut role = Role::default();
        role.grants.insert(
            GrantTarget::Schema("app".to_owned()),
            [permission].into_iter().collect(),
        );
        role
    }

    /// The gate is the server's, not the declaration's: one text, two servers,
    /// two answers. Both numbers are measured — 16.15 refuses the word and
    /// 18.6 takes it.
    #[test]
    fn maintain_is_refused_below_seventeen_and_taken_at_or_above_it() {
        let role = role_with(Permission::Maintain);
        let refused = unsupported_permissions(160_015, "app_reader", &role);
        assert_eq!(refused.len(), 1, "{refused:?}");
        let message = refused[0].to_string();
        assert!(message.contains("PostgreSQL 17"), "{message}");
        assert!(message.contains("16.15"), "{message}");
        assert!(
            unsupported_permissions(170_000, "app_reader", &role).is_empty(),
            "17.0 is the release that added it"
        );
        assert!(unsupported_permissions(180_006, "app_reader", &role).is_empty());
    }

    /// The negative case: an old server refuses `maintain` and nothing else.
    /// A gate that refused every word on an old server would be indisting-
    /// uishable from one that refused the connection.
    #[test]
    fn an_old_server_refuses_only_the_word_it_lacks() {
        for permission in [
            Permission::Select,
            Permission::Usage,
            Permission::Truncate,
            Permission::Trigger,
            Permission::Execute,
        ] {
            let errs = unsupported_permissions(160_015, "app_reader", &role_with(permission));
            assert!(errs.is_empty(), "{permission}: {errs:?}");
        }
    }

    /// A blocker in another database says so, and says it cannot look — the
    /// difference between "nothing is stopping this" and "I cannot see what
    /// is".
    #[test]
    fn a_blocker_outside_this_database_names_the_database_and_says_it_cannot_read_it() {
        let blocker = DropBlocker {
            role: "app_reader".to_owned(),
            database: Some("otherdb".to_owned()),
            deptype: 'a',
            objects: 1,
        };
        let message = blocker.rendered(Some("appdb"));
        assert!(message.contains("`otherdb`"), "{message}");
        assert!(message.contains("cannot read"), "{message}");
        assert!(message.contains("REVOKE"), "{message}");
        assert!(message.contains("1 object"), "{message}");
    }

    /// Ownership and a grant are different blockers with different remedies,
    /// and a message that named one for the other would send an operator to
    /// run a `REVOKE` that changes nothing.
    #[test]
    fn ownership_and_a_grant_name_different_remedies() {
        let owns = DropBlocker {
            role: "app_owner".to_owned(),
            database: Some("appdb".to_owned()),
            deptype: 'o',
            objects: 3,
        }
        .rendered(Some("appdb"));
        assert!(owns.contains("REASSIGN OWNED BY"), "{owns}");
        assert!(owns.contains("3 objects"), "{owns}");
        assert!(owns.contains("in this database"), "{owns}");
        assert!(!owns.contains("REVOKE"), "{owns}");
    }

    /// Name existence is not identity. The case that matters is the one a
    /// "does the new name exist" check would wave through: both names there,
    /// two principals, and an empty plan that records the declared role as
    /// holding what the old one still holds.
    #[test]
    fn a_rename_is_refused_unless_the_old_name_is_gone() {
        assert!(refuse_rename("old", "new", RenameEvidence::Done).is_none());
        let both = refuse_rename("old", "new", RenameEvidence::BothPresent)
            .expect("two principals")
            .to_string();
        assert!(both.contains("different principal"), "{both}");
        assert!(
            both.contains(r#"ALTER ROLE "old" RENAME TO "new";"#),
            "{both}"
        );
        let not_yet = refuse_rename("old", "new", RenameEvidence::NotRunYet)
            .expect("the rename has not been run")
            .to_string();
        assert!(not_yet.contains("has not been run"), "{not_yet}");
        let neither = refuse_rename("old", "new", RenameEvidence::NeitherPresent)
            .expect("no principal at all")
            .to_string();
        assert!(neither.contains("no principal to rename"), "{neither}");
    }

    /// The refusal carries the statement, not a description of it.
    #[test]
    fn a_missing_role_is_refused_with_the_create_role_to_run() {
        let message = refuse_missing("app reader").to_string();
        assert!(
            message.contains(r#"CREATE ROLE "app reader";"#),
            "{message}"
        );
    }
}
