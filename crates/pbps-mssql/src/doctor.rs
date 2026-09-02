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

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError, Param};

use crate::catalog::get;

/// Where a permission has to be held for a deployment to succeed.
///
/// # Why this exists at all
///
/// The first version of this module asked
/// `sys.fn_my_permissions(NULL, 'DATABASE')` and compared the answer against
/// one flat list. That query returns the permissions effective **on the
/// database securable**, so a login granted `ALTER` on the schemas it manages
/// and `SELECT` on the ledger — the exact shape of a least-privilege
/// deployment account — came back holding none of them. `doctor` then reported
/// gaps the account did not have and exited 2 on a setup that deploys fine.
///
/// That is not a cosmetic false positive. The remedy an operator reaches for
/// when told they lack `ALTER` on the database is a database-wide grant, so
/// the check was pushing people towards exactly the "just make it db_owner"
/// outcome this list exists to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needed {
    /// Grantable only at the database, so that is the only place to ask.
    Database,
    /// Needed on every schema pbps manages, and on `dbo`, where the ledger
    /// tables live.
    Managed,
    /// Needed only on the ledger and the lock themselves.
    ///
    /// Asked for on those two **objects**, not on their schema. A careful DBA
    /// grants `INSERT` and `DELETE` on `dbo.__pbps_state` and `dbo.__pbps_lock`
    /// and nowhere else, and a schema-scoped question reports that as missing —
    /// the same over-demand this enum was introduced to remove, one level
    /// further down. `HAS_PERMS_BY_NAME` accounts for inheritance, so a grant
    /// on the schema or the database still answers 1 at object scope.
    ///
    /// Before the tables exist there is nothing to ask about, so the question
    /// falls back to the schema — which is the only place a grant *can* sit in
    /// advance of a first deployment.
    Ledger,
}

/// A permission pbps needs, what needs it, and where it has to be held.
///
/// Named individually rather than as "db_owner": an organization that grants
/// the deployment account exactly what it needs should be able to see the list,
/// and "make it an owner" is the advice that makes every such organization say
/// no to the tool.
pub struct Requirement {
    pub name: &'static str,
    pub why: &'static str,
    pub needed: Needed,
}

const fn req(name: &'static str, why: &'static str, needed: Needed) -> Requirement {
    Requirement { name, why, needed }
}

/// The schema the ledger and the lock live in.
pub const LEDGER_SCHEMA: &str = "dbo";

pub const REQUIRED: [Requirement; 9] = [
    req(
        "VIEW DEFINITION",
        "reading the catalog: pull, plan --db, verify",
        Needed::Managed,
    ),
    req(
        "SELECT",
        "the pre-flight probes, which count rows that would break, and reading the ledger",
        Needed::Managed,
    ),
    req(
        "CREATE TABLE",
        "creating __pbps_state and __pbps_lock on first use",
        Needed::Database,
    ),
    req(
        "ALTER",
        "every change to a table in a schema pbps manages",
        Needed::Managed,
    ),
    req(
        "INSERT",
        "recording a state snapshot, and taking the deployment lock",
        Needed::Ledger,
    ),
    // The worst gap to be missing, and the easiest to overlook: `apply` takes
    // the lock with INSERT and releases it with DELETE. Without this the schema
    // change commits and *then* the release fails, leaving a stale lock that
    // blocks the next pipeline — which is precisely the failure `doctor` exists
    // to catch beforehand. `state prune` needs it too.
    req(
        "DELETE",
        "releasing the deployment lock after an apply, and `state prune`",
        Needed::Ledger,
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
    req(
        "CREATE VIEW",
        "creating or restating a declared view",
        Needed::Database,
    ),
    req(
        "CREATE PROCEDURE",
        "creating or restating a declared stored procedure",
        Needed::Database,
    ),
    req(
        "CREATE FUNCTION",
        "creating or restating a declared function",
        Needed::Database,
    ),
];

/// What the connected account effectively holds, per securable.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Held {
    /// Permissions effective on the database securable.
    pub database: BTreeSet<String>,
    /// Per schema, the schema-scoped permissions effective on it.
    ///
    /// A schema absent from this map was **not asked about**, which is not the
    /// same as holding nothing there — see [`missing`].
    pub schemas: BTreeMap<String, BTreeSet<String>>,

    /// Per ledger object, the permissions effective on it.
    ///
    /// Empty when the ledger does not exist yet, in which case [`missing`]
    /// falls back to the schema answer.
    pub ledger_objects: BTreeMap<String, BTreeSet<String>>,
}

/// A permission that is needed and not held, and the securable it is missing on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    pub permission: &'static str,
    pub why: &'static str,
    /// Where it is missing, already spelled the way a `GRANT` names it.
    pub securable: Securable,
}

/// The securable a [`Gap`] is about, kept typed so the report cannot spell one
/// of them wrongly and so a caller can tell them apart without parsing prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Securable {
    Database,
    Schema(String),
    Object(String),
}

impl std::fmt::Display for Securable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Securable::Database => f.write_str("the database"),
            Securable::Schema(s) => write!(f, "SCHEMA::{s}"),
            Securable::Object(o) => write!(f, "OBJECT::{o}"),
        }
    }
}

impl Gap {
    /// How the securable is named in a `GRANT`, which is how the report names it.
    pub fn securable(&self) -> String {
        self.securable.to_string()
    }
}

/// The permissions the connected account effectively holds.
///
/// `schemas` are the schemas the project manages; `dbo` is added because the
/// ledger lives there whether or not anything is declared in it.
///
/// Two queries, because they answer two different questions.
/// `fn_my_permissions` gives the database-scoped set and is the effective one —
/// it already accounts for role membership, so `db_owner` comes back holding
/// everything rather than holding one role name this code would have to
/// interpret. `HAS_PERMS_BY_NAME` answers the per-schema question, and it
/// likewise accounts for inheritance: a database-wide `GRANT ALTER` returns 1
/// on every schema, so asking at the narrower scope never reports a gap the
/// broader grant covers.
///
/// # Why only schemas that already exist are asked about
///
/// `HAS_PERMS_BY_NAME` on a securable that does not exist answers nothing
/// useful, and a first deployment is exactly the case where the declared
/// schemas are not there yet. Joining against `sys.schemas` leaves those
/// unasked rather than reported as gaps — the alternative would fire on the
/// most common first run there is.
pub async fn permissions(conn: &mut Conn, schemas: &[String]) -> Result<Held, DbError> {
    let rows = conn
        .query("SELECT permission_name AS name FROM sys.fn_my_permissions(NULL, 'DATABASE');")
        .await?;
    let mut database = BTreeSet::new();
    for row in &rows {
        let name: &str = get(row, "name")?;
        database.insert(name.trim().to_ascii_uppercase());
    }

    let mut wanted: BTreeSet<&str> = schemas.iter().map(String::as_str).collect();
    wanted.insert(LEDGER_SCHEMA);
    let wanted: Vec<&str> = wanted.into_iter().collect();

    // Ledger permissions are still asked at schema scope as well: that is the
    // fallback for a database where the ledger does not exist yet, which is
    // every first deployment.
    let schema_perms: Vec<&str> = REQUIRED
        .iter()
        .filter(|r| matches!(r.needed, Needed::Managed | Needed::Ledger))
        .map(|r| r.name)
        .collect();
    let ledger_perms: Vec<&str> = REQUIRED
        .iter()
        .filter(|r| matches!(r.needed, Needed::Ledger))
        .map(|r| r.name)
        .collect();

    // Both lists are bound, not pasted. The permission names are this crate's
    // own constants and the schema names come from the declarations, but SQL
    // built by concatenation is the habit this codebase does not have.
    let mut params: Vec<Param<'_>> = Vec::new();
    let mut perm_slots = Vec::new();
    for p in &schema_perms {
        params.push(Param::from(*p));
        perm_slots.push(format!("(@P{})", params.len()));
    }
    let mut schema_slots = Vec::new();
    for s in &wanted {
        params.push(Param::from(*s));
        schema_slots.push(format!("@P{}", params.len()));
    }
    let sql = format!(
        "SELECT s.name AS [schema], p.n AS permission, \
         HAS_PERMS_BY_NAME(QUOTENAME(s.name), 'SCHEMA', p.n) AS held \
         FROM sys.schemas AS s CROSS JOIN (VALUES {}) AS p(n) WHERE s.name IN ({});",
        perm_slots.join(", "),
        schema_slots.join(", ")
    );

    let mut per_schema: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in &conn.query_with(&sql, &params).await? {
        let schema: &str = get(row, "schema")?;
        let permission: &str = get(row, "permission")?;
        // A NULL means the class or the permission name was rejected, which
        // for this crate's own constants would be a bug here. Read as "not
        // held" rather than skipped: a gap the operator can check is a better
        // wrong answer than a silence that reads as ready.
        let held: i32 = row.try_get("held")?.unwrap_or(0);
        let entry = per_schema.entry(schema.to_owned()).or_default();
        if held != 0 {
            entry.insert(permission.trim().to_ascii_uppercase());
        }
    }

    // The ledger and the lock at object scope. A careful DBA grants INSERT and
    // DELETE on exactly these two tables and nowhere else, and only this
    // question can see that grant. `sys.objects` filters to what exists: before
    // the first deployment there is nothing to ask about, and `missing` then
    // falls back to the schema answer above.
    let mut params: Vec<Param<'_>> = Vec::new();
    let mut perm_slots = Vec::new();
    for p in &ledger_perms {
        params.push(Param::from(*p));
        perm_slots.push(format!("(@P{})", params.len()));
    }
    let mut object_slots = Vec::new();
    for t in [pbps_db::ledger::STATE_TABLE, pbps_db::ledger::LOCK_TABLE] {
        params.push(Param::from(t));
        object_slots.push(format!("@P{}", params.len()));
    }
    let sql = format!(
        "SELECT o.n AS [object], p.n AS permission, \
         HAS_PERMS_BY_NAME(o.n, 'OBJECT', p.n) AS held \
         FROM (VALUES {}) AS o(n) CROSS JOIN (VALUES {}) AS p(n) \
         WHERE OBJECT_ID(o.n, N'U') IS NOT NULL;",
        object_slots
            .iter()
            .map(|s| format!("({s})"))
            .collect::<Vec<_>>()
            .join(", "),
        perm_slots.join(", ")
    );

    let mut ledger_objects: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in &conn.query_with(&sql, &params).await? {
        let object: &str = get(row, "object")?;
        let permission: &str = get(row, "permission")?;
        let held: i32 = row.try_get("held")?.unwrap_or(0);
        let entry = ledger_objects.entry(object.to_owned()).or_default();
        if held != 0 {
            entry.insert(permission.trim().to_ascii_uppercase());
        }
    }

    Ok(Held {
        database,
        schemas: per_schema,
        ledger_objects,
    })
}

/// Which of [`REQUIRED`] the account does not hold, and where.
///
/// `CONTROL` on the database short-circuits the whole list: it implies every
/// permission below it, and an account that holds it would otherwise be
/// reported as missing all of them while being able to do all of them.
///
/// A schema that produced no row is left alone: it does not exist yet, so
/// nothing can be said about permissions on it, and saying it anyway would
/// report a gap on every first deployment.
pub fn missing(held: &Held) -> Vec<Gap> {
    if held.database.contains("CONTROL") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for r in &REQUIRED {
        match r.needed {
            Needed::Database => {
                if !held.database.contains(r.name) {
                    out.push(Gap {
                        permission: r.name,
                        why: r.why,
                        securable: Securable::Database,
                    });
                }
            }
            Needed::Managed => {
                for (schema, granted) in &held.schemas {
                    if !granted.contains(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: Securable::Schema(schema.clone()),
                        });
                    }
                }
            }
            // Object scope where the tables exist, because that is the
            // narrowest place a grant can sit and the only question that can
            // see one. Where they do not exist yet, the schema is the only
            // place a grant *can* be, so that is what is asked instead.
            Needed::Ledger if !held.ledger_objects.is_empty() => {
                for (object, granted) in &held.ledger_objects {
                    if !granted.contains(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: Securable::Object(object.clone()),
                        });
                    }
                }
            }
            Needed::Ledger => {
                if let Some(granted) = held.schemas.get(LEDGER_SCHEMA)
                    && !granted.contains(r.name)
                {
                    out.push(Gap {
                        permission: r.name,
                        why: r.why,
                        securable: Securable::Schema(LEDGER_SCHEMA.to_owned()),
                    });
                }
            }
        }
    }
    out
}

/// Whether this server accepts `CREATE OR ALTER`, which every module statement
/// the emitter writes depends on (ADR-0002: SQL Server 2016 SP1+).
///
/// # Why the edition is part of the question
///
/// Azure SQL Database and Managed Instance report `ProductVersion` **12.0.x**
/// and have supported `CREATE OR ALTER` since long before this tool existed.
/// A check on the version number alone would refuse the two targets a
/// cloud-native user is most likely to have, which is a worse failure than the
/// gap it closes — so Azure is answered by its edition, not its version.
///
/// # Why "cannot tell" means yes
///
/// A version string this cannot parse must not manufacture a refusal. The
/// consequence of a wrong "no" is a readiness error on a server that would have
/// worked; the consequence of a wrong "yes" is the failure that exists today.
/// Only the first of those is caused by this function, so it says yes when it
/// does not know.
pub fn supports_create_or_alter(product_version: &str, edition_raw: &str) -> bool {
    if edition_raw.to_ascii_lowercase().contains("azure") {
        return true;
    }
    let mut parts = product_version.split('.');
    let (Some(Ok(major)), Some(_minor), Some(Ok(build))) = (
        parts.next().map(str::parse::<u32>),
        parts.next(),
        parts.next().map(str::parse::<u32>),
    ) else {
        return true;
    };
    match major {
        // 2017 and later.
        m if m > 13 => true,
        // 2016: RTM is 13.0.1601, SP1 is 13.0.4001. The feature arrived in SP1.
        13 => build >= 4001,
        // 2014 and earlier, on a non-Azure server.
        _ => false,
    }
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

    /// Every schema-scoped permission on every asked schema, and every
    /// database-scoped one on the database.
    fn everything(schemas: &[&str]) -> Held {
        let schema_perms: BTreeSet<String> = REQUIRED
            .iter()
            .filter(|r| !matches!(r.needed, Needed::Database))
            .map(|r| r.name.to_owned())
            .collect();
        Held {
            database: REQUIRED
                .iter()
                .filter(|r| matches!(r.needed, Needed::Database))
                .map(|r| r.name.to_owned())
                .collect(),
            schemas: schemas
                .iter()
                .map(|s| ((*s).to_owned(), schema_perms.clone()))
                .collect(),
            // The ledger not existing yet is the default here, so `missing`
            // falls back to the schema. `with_ledger_objects` below is the
            // other half.
            ledger_objects: BTreeMap::new(),
        }
    }

    /// The same holdings, but with the ledger tables present and carrying
    /// exactly the ledger permissions at object scope — and *nothing* at schema
    /// scope, which is the least-privilege shape this must accept.
    fn ledger_granted_on_the_objects_only(schemas: &[&str]) -> Held {
        let ledger: BTreeSet<String> = REQUIRED
            .iter()
            .filter(|r| matches!(r.needed, Needed::Ledger))
            .map(|r| r.name.to_owned())
            .collect();
        let mut held = everything(schemas);
        for granted in held.schemas.values_mut() {
            for p in &ledger {
                granted.remove(p);
            }
        }
        held.ledger_objects = [
            (pbps_db::ledger::STATE_TABLE.to_owned(), ledger.clone()),
            (pbps_db::ledger::LOCK_TABLE.to_owned(), ledger),
        ]
        .into_iter()
        .collect();
        held
    }

    #[test]
    fn an_account_holding_everything_is_missing_nothing() {
        assert!(missing(&everything(&["dbo", "app"])).is_empty());
    }

    /// The finding this scoping exists for. A least-privilege login holds its
    /// table permissions on the schemas it manages, not on the database, so the
    /// old database-only query reported gaps it did not have — and the remedy
    /// an operator reaches for when told they lack `ALTER` on the *database* is
    /// a database-wide grant, which is the outcome this list exists to avoid.
    #[test]
    fn a_grant_on_the_schema_satisfies_a_schema_scoped_requirement() {
        let held = everything(&["dbo", "app"]);
        // Nothing at all at the database level except the four CREATEs, which
        // are the only permissions that cannot be granted lower.
        assert!(!held.database.contains("ALTER"));
        assert!(missing(&held).is_empty());
    }

    /// The report names what is missing, *where*, and *what it is for*: "you
    /// lack ALTER" sends someone to a DBA, "you lack ALTER on SCHEMA::app,
    /// which every table change needs" lets them ask for the right thing once.
    #[test]
    fn a_missing_permission_is_reported_with_its_securable_and_its_reason() {
        let mut held = everything(&["dbo", "app"]);
        held.schemas.get_mut("app").unwrap().remove("ALTER");
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].permission, "ALTER");
        assert_eq!(gaps[0].securable(), "SCHEMA::app");
        assert!(!gaps[0].why.is_empty());
    }

    /// An account that can take the lock but not release it is the dangerous
    /// shape: `apply` commits the schema change and only then fails, leaving a
    /// stale lock. `doctor` has to name it before the deployment, not after.
    #[test]
    fn holding_insert_without_delete_is_still_a_gap() {
        let mut held = everything(&["dbo"]);
        held.schemas.get_mut("dbo").unwrap().remove("DELETE");
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].permission, "DELETE");
        assert_eq!(gaps[0].securable(), "SCHEMA::dbo");
    }

    /// The ledger and the lock live in `dbo`. Demanding INSERT and DELETE on an
    /// application schema would send an organization to grant a write
    /// permission the deployment never uses there — and this list only stays
    /// credible if every entry on it is really needed.
    #[test]
    fn the_ledger_writes_are_not_demanded_on_an_application_schema() {
        let mut held = everything(&["dbo", "app"]);
        let app = held.schemas.get_mut("app").unwrap();
        app.remove("INSERT");
        app.remove("DELETE");
        assert!(missing(&held).is_empty());
    }

    /// A declared schema that does not exist yet produces no row, and a first
    /// deployment is exactly when that happens. Reporting a gap there would
    /// fire on the most common first run there is.
    #[test]
    fn a_schema_that_does_not_exist_yet_is_not_reported_as_a_gap() {
        // Only `dbo` came back from `sys.schemas`; `app` is still to be created.
        let held = everything(&["dbo"]);
        assert!(missing(&held).is_empty());
    }

    /// The narrowest least-privilege shape there is: `INSERT` and `DELETE`
    /// granted on `dbo.__pbps_state` and `dbo.__pbps_lock` themselves and
    /// nowhere else. A schema-scoped question reports that as missing — the
    /// same over-demand the `Needed` split was introduced to remove, one level
    /// further down.
    #[test]
    fn a_grant_on_the_ledger_objects_alone_satisfies_the_ledger_requirements() {
        let held = ledger_granted_on_the_objects_only(&["dbo", "app"]);
        assert!(
            !held.schemas["dbo"].contains("INSERT"),
            "the test's premise is wrong if the schema still carries it"
        );
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
    }

    /// Still the dangerous shape, now at object scope: it can take the lock and
    /// not release it, so `apply` commits the schema change and only then fails.
    #[test]
    fn losing_delete_on_the_lock_object_is_still_a_gap() {
        let mut held = ledger_granted_on_the_objects_only(&["dbo"]);
        held.ledger_objects
            .get_mut(pbps_db::ledger::LOCK_TABLE)
            .unwrap()
            .remove("DELETE");
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "DELETE");
        assert_eq!(gaps[0].securable(), "OBJECT::dbo.__pbps_lock");
    }

    /// Before a first deployment the ledger does not exist, so there is no
    /// object to ask about and the schema is the only place a grant can sit.
    /// Asking at object scope anyway would report a gap on every first run.
    #[test]
    fn without_a_ledger_yet_the_question_falls_back_to_the_schema() {
        let held = everything(&["dbo"]);
        assert!(held.ledger_objects.is_empty());
        assert!(missing(&held).is_empty());

        let mut held = everything(&["dbo"]);
        held.schemas.get_mut("dbo").unwrap().remove("INSERT");
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].securable(), "SCHEMA::dbo");
    }

    /// A trigger is authorized by ALTER on the table it is on, not by a CREATE
    /// of its own. Demanding one would send an organization to grant a
    /// permission its deployment does not use.
    #[test]
    fn only_the_three_module_kinds_that_need_a_create_have_one() {
        let creates: Vec<&str> = REQUIRED
            .iter()
            .map(|r| r.name)
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

    /// The four CREATEs are the only ones SQL Server will not grant below the
    /// database, so they are the only ones asked for there. Asking for any of
    /// the others at database scope is what caused the false gaps.
    #[test]
    fn only_permissions_that_cannot_be_granted_lower_are_asked_at_database_scope() {
        let db_scoped: Vec<&str> = REQUIRED
            .iter()
            .filter(|r| matches!(r.needed, Needed::Database))
            .map(|r| r.name)
            .collect();
        assert!(db_scoped.iter().all(|n| n.starts_with("CREATE ")));
        assert_eq!(db_scoped.len(), 4);
    }

    /// CONTROL implies the rest. Reporting an owner as missing every entry
    /// would be the check crying wolf on the most common setup there is.
    #[test]
    fn control_alone_satisfies_the_list() {
        let held = Held {
            database: set(&["CONTROL"]),
            ..Held::default()
        };
        assert!(missing(&held).is_empty());
    }

    /// The negative case: an empty answer is a real state — a login mapped to
    /// no user in this database — and must not be mistaken for "fine". `dbo`
    /// always exists, so it is always asked about and always reported.
    #[test]
    fn holding_nothing_is_reported_as_missing_everything() {
        let held = Held {
            database: BTreeSet::new(),
            schemas: [("dbo".to_owned(), BTreeSet::new())].into_iter().collect(),
            ledger_objects: BTreeMap::new(),
        };
        assert_eq!(missing(&held).len(), REQUIRED.len());
    }

    #[test]
    fn the_create_or_alter_floor_is_2016_sp1() {
        assert!(!supports_create_or_alter(
            "13.0.1601.5",
            "Developer Edition"
        ));
        assert!(supports_create_or_alter("13.0.4001.0", "Developer Edition"));
        assert!(supports_create_or_alter("16.0.1000.6", "Standard Edition"));
        assert!(!supports_create_or_alter("11.0.7001.0", "Standard Edition"));
    }

    /// Azure SQL Database and Managed Instance report 12.0.x and have supported
    /// `CREATE OR ALTER` throughout. Refusing them on the version number would
    /// break the two targets a cloud-native user is most likely to have — a
    /// worse failure than the gap the check closes.
    #[test]
    fn azure_is_answered_by_its_edition_not_its_version() {
        assert!(supports_create_or_alter("12.0.2000.8", "SQL Azure"));
        assert!(supports_create_or_alter(
            "12.0.2000.8",
            "SQL Azure Managed Instance"
        ));
        // The same version on a real on-premises server is 2014, which cannot.
        assert!(!supports_create_or_alter("12.0.2000.8", "Standard Edition"));
    }

    /// A version string this cannot read must not manufacture a refusal: a
    /// wrong "no" blocks a server that would have worked, and this function
    /// would be the only cause of it.
    #[test]
    fn an_unreadable_version_is_not_treated_as_too_old() {
        assert!(supports_create_or_alter("", ""));
        assert!(supports_create_or_alter("unknown", "Developer Edition"));
        assert!(supports_create_or_alter("13", "Developer Edition"));
    }
}
