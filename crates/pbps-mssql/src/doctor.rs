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
    /// Needed on the ledger's schema, but only while the ledger does not exist.
    ///
    /// `CREATE TABLE` at the database is not enough to create
    /// `dbo.__pbps_state`: SQL Server also wants `ALTER` on the schema the
    /// table lands in. Removing the ledger's schema from the managed set (which
    /// it had to be, for a project that declares nothing in `dbo`) took that
    /// check away with it — so an account could pass `doctor` and then fail
    /// inside `ensure_tables` on its very first deployment. An over-demand
    /// turned into an under-demand, which is the worse direction: the first
    /// annoys, the second says "ready" and then breaks.
    ///
    /// Only while the tables do not exist. Once they do, writing rows needs
    /// `INSERT` and `DELETE`, not `ALTER`, and demanding it forever would be
    /// the over-demand coming back.
    LedgerCreation,

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

    /// Needed on a table a declared foreign key *points at* which lies outside
    /// the schemas this project manages.
    ///
    /// `REFERENCES` is authorized on the referenced table, and the pre-flight
    /// probe for an added foreign key reads it (`NOT EXISTS (SELECT 1 FROM
    /// <parent> ...)`) — so both are needed there, and neither is covered by
    /// anything asked about the managed schemas. `validate` accepts a foreign
    /// key whose target is undeclared, and the emitter really does write
    /// `REFERENCES [shared].[parent]`, so this is reachable from an ordinary
    /// project rather than a contrived one.
    ///
    /// Asked at **object** scope, and only for targets outside the managed
    /// schemas: one inside them is already covered by the `Managed` entries,
    /// and demanding anything on the whole of someone else's schema is the
    /// over-demand this enum exists to avoid.
    Referenced,

    /// Needed at the database, and only when the declarations have roles
    /// (ADR-0005): creating, renaming and dropping a database role.
    ///
    /// The first entry in this list that depends on what the project
    /// declares. Demanding it of every project would ask an estate that
    /// declares no role to hold `ALTER ANY ROLE`, which is a security-shaped
    /// permission nobody hands out for nothing; asking only when a role file
    /// exists costs one read of the declarations `doctor` already does.
    RoleAdmin,

    /// Needed on every object or schema a declared role is granted on.
    ///
    /// `GRANT` is authorized on the securable itself — `CONTROL` on it, or
    /// ownership, or that very permission held `WITH GRANT OPTION` — and
    /// `ALTER` on the schema, which the managed entries already demand,
    /// implies none of those. So an account holding everything above passes
    /// readiness and fails on the first `GRANT`. Asked at object scope for an
    /// object target and at schema scope for a `schema::` one; `CONTROL` is
    /// what is asked for, because `HAS_PERMS_BY_NAME` cannot ask "held with
    /// grant option".
    Granted,
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

/// The tables `state::ensure_tables` creates, and therefore the ones whose
/// absence still requires the create-time permission.
pub const LEDGER_TABLES: [&str; 2] = [pbps_db::ledger::STATE_TABLE, pbps_db::ledger::LOCK_TABLE];

pub const REQUIRED: [Requirement; 17] = [
    req(
        "VIEW DEFINITION",
        "reading the catalog: pull, plan --db, verify",
        Needed::Managed,
    ),
    // `SELECT` twice, because it is needed in two places for two reasons and a
    // single entry made the wrong demand in both directions. The probes count
    // rows in the *managed* tables; reading the ledger is a read of two tables
    // in `dbo`. A project that manages only `app` can legitimately hold the
    // first on `SCHEMA::app` and the second on the two ledger objects — and one
    // `Needed::Managed` entry reported `SELECT on SCHEMA::dbo` missing for it.
    req(
        "SELECT",
        "the pre-flight probes, which count rows that would break",
        Needed::Managed,
    ),
    req(
        "SELECT",
        "reading the recorded state and the deployment lock",
        Needed::Ledger,
    ),
    req(
        "ALTER",
        "creating __pbps_state and __pbps_lock in their schema on first use",
        Needed::LedgerCreation,
    ),
    req(
        "CREATE TABLE",
        "creating __pbps_state and __pbps_lock on first use",
        Needed::Database,
    ),
    // "most", not "every": a rename that moves a table *between* schemas is
    // emitted as `ALTER SCHEMA ... TRANSFER`, which wants `CONTROL` on the
    // table itself on top of `ALTER` on the destination. That is deliberately
    // not demanded — see the note below `REQUIRED` — so this entry must not
    // claim to cover it.
    req(
        "ALTER",
        "most changes to a table in a schema pbps manages",
        Needed::Managed,
    ),
    // A foreign key is `ALTER TABLE ... REFERENCES ...`, and SQL Server wants
    // `REFERENCES` on the *referenced* table for it. `ALTER` on the schema does
    // not imply it, so an account holding exactly the rest of this list passed
    // readiness and failed on one of the commonest changes there is.
    req(
        "REFERENCES",
        "adding a foreign key, which the engine authorizes on the referenced table",
        Needed::Managed,
    ),
    // The same two, one securable further out. A foreign key into a schema the
    // project does not manage is authorized on *that* table, and the probe for
    // it reads *that* table — neither of which any question about the managed
    // schemas can see.
    req(
        "REFERENCES",
        "a foreign key into a table this project does not manage",
        Needed::Referenced,
    ),
    req(
        "SELECT",
        "the pre-flight probe for that foreign key, which reads the referenced table",
        Needed::Referenced,
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
    // Roles (ADR-0005), demanded only of a project that declares one.
    req(
        "CREATE ROLE",
        "creating a declared database role",
        Needed::RoleAdmin,
    ),
    req(
        "ALTER ANY ROLE",
        "renaming or dropping a declared database role",
        Needed::RoleAdmin,
    ),
    req(
        "CONTROL",
        "granting a declared role a permission here, which the engine authorizes on the \
         securable itself",
        Needed::Granted,
    ),
];

// # A permission deliberately absent: `CONTROL` on the managed schemas
//
// (It *is* demanded, per securable, on what a declared role is granted on —
// `Needed::Granted` — because a `GRANT` is authorized there and nothing else
// in this list covers it. That is a narrow ask on named objects a project
// chose to grant on, not the broad one refused below.)
//
// A rename that moves a table between schemas is emitted as
// `ALTER SCHEMA ... TRANSFER`, which the engine authorizes with `CONTROL` on
// the transferred table — not with the `ALTER` above. So an account holding
// everything in this list can still fail on that one statement.
//
// It is not demanded, and the reason is the same one this whole list exists
// for. `doctor` never sees a plan, so it would have to require `CONTROL` on
// every managed schema from every project, always. `CONTROL` on a schema is
// close to owning it, cross-schema renames are rare, and demanding ownership
// up front to cover a rare statement is exactly the "just make it db_owner"
// pressure this list refuses to apply.
//
// The honest version is therefore a narrower claim rather than a broader
// demand: `ALTER` says "most changes", not "every change". Catching the real
// case belongs in the plan-aware pre-flight, which does see the statements —
// it is recorded in SPEC §9.5 as a known gap rather than silently covered.
//

/// What the connected account effectively holds, per securable.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Held {
    /// Permissions effective on the database securable.
    pub database: BTreeSet<String>,
    /// Per **managed** schema, the schema-scoped permissions effective on it.
    ///
    /// A schema absent from this map was **not asked about**, which is not the
    /// same as holding nothing there — see [`missing`].
    ///
    /// The ledger's schema is deliberately not forced in here. It used to be,
    /// which quietly demanded `ALTER` and the probes' `SELECT` on `dbo` from a
    /// project that manages only `app` and never touches a `dbo` table.
    pub schemas: BTreeMap<String, BTreeSet<String>>,

    /// Managed schemas the database does not have.
    ///
    /// Not a permission problem, and not nothing either. The emitter never
    /// writes `CREATE SCHEMA` — a plan declaring `app.customer` against a
    /// database with no `app` emits `CREATE TABLE [app].[customer]` and fails —
    /// so an absent managed schema is a readiness problem in its own right.
    /// Leaving it merely *unasked* (which is right for the permission
    /// question, since there is no securable to ask about) let `doctor` exit 0
    /// immediately before the deployment failed.
    pub absent_schemas: BTreeSet<String>,

    /// The schema-scoped permissions effective on the ledger's schema.
    ///
    /// Kept apart from `schemas` because it answers a different question:
    /// it is the fallback for the ledger requirements before the ledger tables
    /// exist, not a managed schema in its own right. When the project *does*
    /// manage `dbo`, it appears in both.
    pub ledger_schema: BTreeSet<String>,

    /// Per ledger object, the permissions effective on it.
    ///
    /// Empty when the ledger does not exist yet, in which case [`missing`]
    /// falls back to the schema answer.
    pub ledger_objects: BTreeMap<String, BTreeSet<String>>,

    /// Per foreign-key target outside the managed schemas, the permissions
    /// effective on that **object**.
    ///
    /// A target the database does not have is absent from this map rather than
    /// present and empty: there is no securable to ask about, and reporting a
    /// gap there would fire on every project whose referenced table is created
    /// by something else's deployment. That the table is missing at all is a
    /// question for `plan --db`, which sees the change; `doctor` sees no plan.
    pub referenced_objects: BTreeMap<String, BTreeSet<String>>,

    /// Whether the project has any role at all — declared, recorded, or being
    /// dropped. `false` switches the role requirements off rather than
    /// reporting them as gaps.
    pub roles_declared: bool,

    /// Per object a declared role is granted on, the permissions effective on
    /// it — asked about every named object, absent or invisible included, for
    /// the same reason `referenced_objects` is.
    pub granted_objects: BTreeMap<String, BTreeSet<String>>,

    /// Per schema a declared role is granted on (`schema::x`), the
    /// schema-scoped permissions effective on it. A schema the database does
    /// not have is absent, like a managed schema that does not exist yet.
    pub granted_schemas: BTreeMap<String, BTreeSet<String>>,
}

/// What the managed roles are granted on, as `doctor` has to ask about it
/// (ADR-0005). `None` at the call site means the project has no role at all
/// — none declared, none recorded, none being dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantTargets {
    /// `schema.object`, qualified the way `HAS_PERMS_BY_NAME` takes it, as
    /// the declarations grant them.
    pub objects: Vec<String>,
    pub schemas: Vec<String>,
    /// The managed roles by name: declared, recorded in the ids file, or
    /// tombstoned by a `drop-role` not yet applied. Whatever these hold — in
    /// the recorded state, and in the catalog where this login can see it —
    /// is asked about too, because a grant that is gone from the
    /// declarations is a `REVOKE` the plan will write, and the securable it
    /// names is where `CONTROL` has to be held; the declarations alone
    /// cannot see it.
    pub roles: Vec<String>,
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
pub async fn permissions(
    conn: &mut Conn,
    schemas: &[String],
    referenced: &[String],
    granted: Option<&GrantTargets>,
) -> Result<Held, DbError> {
    let rows = conn
        .query("SELECT permission_name AS name FROM sys.fn_my_permissions(NULL, 'DATABASE');")
        .await?;
    let mut database = BTreeSet::new();
    for row in &rows {
        let name: &str = get(row, "name")?;
        database.insert(name.trim().to_ascii_uppercase());
    }

    // The ledger's schema is queried alongside the managed ones because it is
    // the fallback for the ledger requirements before those tables exist — but
    // its answer is kept in its own field, not folded into the managed set.
    let mut wanted: BTreeSet<&str> = schemas.iter().map(String::as_str).collect();
    wanted.insert(LEDGER_SCHEMA);
    let wanted: Vec<&str> = wanted.into_iter().collect();

    // Ledger permissions are still asked at schema scope as well: that is the
    // fallback for a database where the ledger does not exist yet, which is
    // every first deployment.
    let schema_perms: Vec<&str> = REQUIRED
        .iter()
        .filter(|r| {
            matches!(
                r.needed,
                Needed::Managed | Needed::Ledger | Needed::LedgerCreation
            )
        })
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
        schema_slots.push(format!("(@P{})", params.len()));
    }
    // The **requested** spelling comes back, not the catalog's. Matching is the
    // server's job — `w.n = s.name` compares under the database's collation, so
    // a case-insensitive database matches `App` to its `app` — but the caller
    // then looks the answer up by the name it asked with. Selecting `s.name`
    // returned `app` for a request of `App`, so the Rust-side lookup missed,
    // and `doctor` reported the schema absent and advised creating one that
    // already exists.
    //
    // `QUOTENAME(s.name)` stays the catalog's spelling: that argument names a
    // real securable, not a map key.
    let sql = format!(
        "SELECT w.n AS [schema], p.n AS permission, \
         HAS_PERMS_BY_NAME(QUOTENAME(s.name), 'SCHEMA', p.n) AS held \
         FROM (VALUES {}) AS w(n) \
         JOIN sys.schemas AS s ON s.name = w.n \
         CROSS JOIN (VALUES {}) AS p(n);",
        schema_slots.join(", "),
        perm_slots.join(", ")
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
    for t in LEDGER_TABLES {
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

    // Foreign-key targets outside the managed schemas, also at object scope —
    // but **without** the `OBJECT_ID(...) IS NOT NULL` filter the ledger query
    // uses, and that difference is deliberate.
    //
    // Metadata visibility hides an object from a principal with no permission
    // on it, so that filter cannot tell "not there" from "not allowed to see".
    // For the ledger that is harmless: a hidden table falls back to the schema
    // question, which still reports a gap. Here it would drop the object from
    // the map entirely and `missing` would say nothing — under-reporting the
    // one case that matters. So every named target is asked about, and
    // `HAS_PERMS_BY_NAME` answering 0 becomes a gap whether the table is absent
    // or invisible. Both of those fail the apply, and the operator can tell
    // which from the name.
    let mut referenced_objects: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if !referenced.is_empty() {
        let referenced_perms: Vec<&str> = REQUIRED
            .iter()
            .filter(|r| matches!(r.needed, Needed::Referenced))
            .map(|r| r.name)
            .collect();
        let mut params: Vec<Param<'_>> = Vec::new();
        let mut perm_slots = Vec::new();
        for p in &referenced_perms {
            params.push(Param::from(*p));
            perm_slots.push(format!("(@P{})", params.len()));
        }
        let mut object_slots = Vec::new();
        for t in referenced {
            params.push(Param::from(t.as_str()));
            object_slots.push(format!("(@P{})", params.len()));
        }
        let sql = format!(
            "SELECT o.n AS [object], p.n AS permission, \
             HAS_PERMS_BY_NAME(o.n, 'OBJECT', p.n) AS held \
             FROM (VALUES {}) AS o(n) CROSS JOIN (VALUES {}) AS p(n);",
            object_slots.join(", "),
            perm_slots.join(", ")
        );
        for row in &conn.query_with(&sql, &params).await? {
            let object: &str = get(row, "object")?;
            let permission: &str = get(row, "permission")?;
            let held: i32 = row.try_get("held")?.unwrap_or(0);
            let entry = referenced_objects.entry(object.to_owned()).or_default();
            if held != 0 {
                entry.insert(permission.trim().to_ascii_uppercase());
            }
        }
    }

    // What the declared roles are granted on (ADR-0005). Objects the way the
    // foreign-key targets are asked — every named one, so absent and
    // invisible both land as a gap — and schemas through `sys.schemas`, so a
    // schema that does not exist yet is unasked rather than reported.
    let mut granted_objects: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut granted_schemas: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if let Some(targets) = granted {
        let granted_perms: Vec<&str> = REQUIRED
            .iter()
            .filter(|r| matches!(r.needed, Needed::Granted))
            .map(|r| r.name)
            .collect();
        // The declared targets, plus everything the managed roles hold: a
        // grant the declarations dropped is a REVOKE on that securable, and a
        // role being dropped is a REVOKE of each of its grants. Two sources,
        // because neither is complete alone. The **recorded state** is what
        // the next plan revokes against, and the ledger is readable by any
        // account that can deploy at all; the **catalog** also shows grants
        // adopted by hand, but only on securables this login already has
        // some permission on (metadata visibility), which is precisely not
        // the ones a readiness check is for. A ledger this account cannot
        // read is a gap of its own, reported by the ledger rows.
        let mut objects: BTreeSet<String> = targets.objects.iter().cloned().collect();
        let mut schemas_wanted: BTreeSet<String> = targets.schemas.iter().cloned().collect();
        if !targets.roles.is_empty()
            && let Ok(Some(entry)) = crate::state::latest(conn).await
        {
            for (name, role) in &entry.snapshot.schema.roles {
                if !targets.roles.contains(name) {
                    continue;
                }
                for target in role.grants.keys() {
                    match target {
                        pbps_model::GrantTarget::Object(o) => {
                            objects.insert(format!("{}.{}", o.schema, o.name));
                        }
                        pbps_model::GrantTarget::Schema(s) => {
                            schemas_wanted.insert(s.clone());
                        }
                    }
                }
            }
        }
        if !targets.roles.is_empty() {
            let mut params: Vec<Param<'_>> = Vec::new();
            let mut role_slots = Vec::new();
            for r in &targets.roles {
                params.push(Param::from(r.as_str()));
                role_slots.push(format!("@P{}", params.len()));
            }
            let sql = format!(
                "SELECT CAST(dp.class AS int) AS class, s.name AS [schema], o.name AS [object] \
                 FROM sys.database_permissions AS dp \
                 JOIN sys.database_principals AS r \
                   ON r.principal_id = dp.grantee_principal_id AND r.type = 'R' \
                 LEFT JOIN sys.objects AS o ON dp.class = 1 AND o.object_id = dp.major_id \
                 LEFT JOIN sys.schemas AS s \
                   ON s.schema_id = CASE dp.class WHEN 1 THEN o.schema_id WHEN 3 THEN dp.major_id END \
                 WHERE dp.class IN (1, 3) AND r.name IN ({});",
                role_slots.join(", ")
            );
            for row in &conn.query_with(&sql, &params).await? {
                let class: i32 = row.try_get("class")?.unwrap_or(0);
                let schema: Option<&str> = row.try_get("schema")?;
                let object: Option<&str> = row.try_get("object")?;
                match (class, schema, object) {
                    (1, Some(schema), Some(object)) => {
                        objects.insert(format!("{schema}.{object}"));
                    }
                    (3, Some(schema), _) => {
                        schemas_wanted.insert(schema.to_owned());
                    }
                    _ => {}
                }
            }
        }
        let objects: Vec<String> = objects.into_iter().collect();
        let schemas_wanted: Vec<String> = schemas_wanted.into_iter().collect();
        if !objects.is_empty() {
            let mut params: Vec<Param<'_>> = Vec::new();
            let mut perm_slots = Vec::new();
            for p in &granted_perms {
                params.push(Param::from(*p));
                perm_slots.push(format!("(@P{})", params.len()));
            }
            let mut object_slots = Vec::new();
            for t in &objects {
                params.push(Param::from(t.as_str()));
                object_slots.push(format!("(@P{})", params.len()));
            }
            let sql = format!(
                "SELECT o.n AS [object], p.n AS permission, \
                 HAS_PERMS_BY_NAME(o.n, 'OBJECT', p.n) AS held \
                 FROM (VALUES {}) AS o(n) CROSS JOIN (VALUES {}) AS p(n);",
                object_slots.join(", "),
                perm_slots.join(", ")
            );
            for row in &conn.query_with(&sql, &params).await? {
                let object: &str = get(row, "object")?;
                let permission: &str = get(row, "permission")?;
                let held: i32 = row.try_get("held")?.unwrap_or(0);
                let entry = granted_objects.entry(object.to_owned()).or_default();
                if held != 0 {
                    entry.insert(permission.trim().to_ascii_uppercase());
                }
            }
        }
        if !schemas_wanted.is_empty() {
            let mut params: Vec<Param<'_>> = Vec::new();
            let mut perm_slots = Vec::new();
            for p in &granted_perms {
                params.push(Param::from(*p));
                perm_slots.push(format!("(@P{})", params.len()));
            }
            let mut schema_slots = Vec::new();
            for s in &schemas_wanted {
                params.push(Param::from(s.as_str()));
                schema_slots.push(format!("(@P{})", params.len()));
            }
            let sql = format!(
                "SELECT w.n AS [schema], p.n AS permission, \
                 HAS_PERMS_BY_NAME(QUOTENAME(s.name), 'SCHEMA', p.n) AS held \
                 FROM (VALUES {}) AS w(n) \
                 JOIN sys.schemas AS s ON s.name = w.n \
                 CROSS JOIN (VALUES {}) AS p(n);",
                schema_slots.join(", "),
                perm_slots.join(", ")
            );
            for row in &conn.query_with(&sql, &params).await? {
                let schema: &str = get(row, "schema")?;
                let permission: &str = get(row, "permission")?;
                let held: i32 = row.try_get("held")?.unwrap_or(0);
                let entry = granted_schemas.entry(schema.to_owned()).or_default();
                if held != 0 {
                    entry.insert(permission.trim().to_ascii_uppercase());
                }
            }
        }
    }

    let ledger_schema = per_schema.get(LEDGER_SCHEMA).cloned().unwrap_or_default();
    // Asked for and not returned by `sys.schemas` means the database does not
    // have it. The ledger's schema is excluded: `dbo` always exists, and if it
    // somehow did not, that is not a declaration problem.
    let absent_schemas: BTreeSet<String> = schemas
        .iter()
        .filter(|name| name.as_str() != LEDGER_SCHEMA && !per_schema.contains_key(*name))
        .cloned()
        .collect();
    // Managed means declared. `dbo` stays only if the project actually declares
    // something in it.
    let managed: BTreeSet<&str> = schemas.iter().map(String::as_str).collect();
    per_schema.retain(|name, _| managed.contains(name.as_str()));

    Ok(Held {
        database,
        schemas: per_schema,
        absent_schemas,
        ledger_schema,
        ledger_objects,
        referenced_objects,
        roles_declared: granted.is_some(),
        granted_objects,
        granted_schemas,
    })
}

/// Which of [`REQUIRED`] the account does not hold, and where.
///
/// # Why `CONTROL` on the database is not a shortcut
///
/// It was one, and that was wrong. The reasoning — `CONTROL` implies every
/// permission below it, so an owner would otherwise be reported as missing
/// everything — mistook the *inputs* for raw grants. They are not:
/// [`permissions`] asks `HAS_PERMS_BY_NAME` at each securable, which already
/// accounts for inheritance, so an owner's answers come back full without any
/// help here. The shortcut therefore bought nothing in the case it was written
/// for, and threw away the only answer that matters in the case it was not.
///
/// That case is `DENY`, which beats an inherited `CONTROL` at the narrower
/// securable and can arrive through any role the principal is in. Measured
/// against a real server rather than reasoned about: with `CONTROL` on the
/// database and `DENY ALTER ON SCHEMA::app`, `sys.fn_my_permissions` still
/// lists `CONTROL`, `HAS_PERMS_BY_NAME` correctly answers 0 for that `ALTER`,
/// and `CREATE TABLE app.t` really does fail. The short-circuit read the first
/// of those three and called the account ready.
///
/// A schema that produced no row is left alone: it does not exist yet, so
/// nothing can be said about permissions on it, and saying it anyway would
/// report a gap on every first deployment.
pub fn missing(held: &Held) -> Vec<Gap> {
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
            // Only while the ledger is still to be created. Once it exists the
            // creation permission is spent, and asking for it forever would be
            // the over-demand this whole enum exists to remove.
            // Per table, not "any of them exists". `ensure_tables` recreates
            // whichever is missing, so one surviving table does not mean the
            // creation permission is spent — and reading `ledger_objects` as a
            // single yes/no let an account with the lock table but not the
            // state table pass readiness and fail on the next `record`.
            Needed::LedgerCreation if held.ledger_objects.len() < LEDGER_TABLES.len() => {
                if !held.ledger_schema.contains(r.name) {
                    out.push(Gap {
                        permission: r.name,
                        why: r.why,
                        securable: Securable::Schema(LEDGER_SCHEMA.to_owned()),
                    });
                }
            }
            Needed::LedgerCreation => {}
            // The scope is chosen per table, not once for the pair. A table
            // that exists can only be answered at object scope, because that is
            // where a careful DBA's grant sits; a table that does not exist yet
            // has no object to ask about, and the grant that will cover it once
            // `ensure_tables` creates it is the one on the schema.
            //
            // Asking once for the pair — "any object row came back, so use
            // object scope" — let an account holding object grants on
            // `__pbps_lock` alone pass readiness. `ensure_tables` then created
            // `__pbps_state`, and the next state read was denied by a
            // permission `doctor` had never asked about, at either scope.
            // Every named target, present in the map or not: `permissions`
            // asks about all of them precisely so that "absent" and "invisible"
            // both land here as a gap rather than as silence.
            Needed::Referenced => {
                for (object, granted) in &held.referenced_objects {
                    if !granted.contains(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: Securable::Object(object.clone()),
                        });
                    }
                }
            }
            // Only when a role is declared: the permission is security-shaped,
            // and asking an estate with no role to hold it is the over-demand
            // this list refuses everywhere else.
            Needed::RoleAdmin => {
                if held.roles_declared && !held.database.contains(r.name) {
                    out.push(Gap {
                        permission: r.name,
                        why: r.why,
                        securable: Securable::Database,
                    });
                }
            }
            Needed::Granted => {
                for (object, granted) in &held.granted_objects {
                    if !granted.contains(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: Securable::Object(object.clone()),
                        });
                    }
                }
                for (schema, granted) in &held.granted_schemas {
                    if !granted.contains(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: Securable::Schema(schema.clone()),
                        });
                    }
                }
            }
            Needed::Ledger => {
                // Both tables missing means both fall back to the same schema,
                // and the operator needs one `GRANT`, not two identical lines
                // telling them to run it twice.
                //
                // Deduplicated against *this* requirement's own gaps rather
                // than everything reported so far: `SELECT` is in the list
                // twice on purpose — the probes read managed tables and the
                // ledger read is two tables in `dbo` — and a project that
                // manages `dbo` would otherwise have the second one swallowed
                // by the first, losing the reason it is needed.
                let mut reported: Vec<Securable> = Vec::new();
                for table in LEDGER_TABLES {
                    let (granted, securable) = match held.ledger_objects.get(table) {
                        Some(granted) => (granted, Securable::Object(table.to_owned())),
                        None => (
                            &held.ledger_schema,
                            Securable::Schema(LEDGER_SCHEMA.to_owned()),
                        ),
                    };
                    if !granted.contains(r.name) && !reported.contains(&securable) {
                        reported.push(securable.clone());
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable,
                        });
                    }
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
            // Everything asked about exists in this helper; the absent case has
            // its own test below.
            absent_schemas: BTreeSet::new(),
            // The ledger not existing yet is the default here, so `missing`
            // falls back to the ledger *schema*, which therefore carries the
            // ledger permissions. `ledger_granted_on_the_objects_only` below is
            // the other half.
            ledger_schema: REQUIRED
                .iter()
                .filter(|r| matches!(r.needed, Needed::Ledger | Needed::LedgerCreation))
                .map(|r| r.name.to_owned())
                .collect(),
            ledger_objects: BTreeMap::new(),
            referenced_objects: BTreeMap::new(),
            roles_declared: false,
            granted_objects: BTreeMap::new(),
            granted_schemas: BTreeMap::new(),
        }
    }

    /// The role permissions are asked for only of a project that declares a
    /// role (ADR-0005): `ALTER ANY ROLE` is security-shaped, and demanding it
    /// of every estate would be the over-demand this list refuses elsewhere.
    #[test]
    fn role_permissions_are_demanded_only_when_a_role_is_declared() {
        let mut held = everything(&["app"]);
        assert!(missing(&held).is_empty());
        held.roles_declared = true;
        let gaps = missing(&held);
        let names: Vec<&str> = gaps.iter().map(|g| g.permission).collect();
        assert_eq!(names, ["CREATE ROLE", "ALTER ANY ROLE"], "{gaps:?}");
        assert!(gaps.iter().all(|g| g.securable == Securable::Database));
        held.database.insert("CREATE ROLE".into());
        held.database.insert("ALTER ANY ROLE".into());
        assert!(missing(&held).is_empty());
    }

    /// A `GRANT` is authorized on the securable itself, which `ALTER` on the
    /// schema does not cover: the gap is reported at the object or the schema
    /// the role is granted on, never on the database.
    #[test]
    fn a_grant_target_without_control_is_a_gap_on_that_securable() {
        let mut held = everything(&["app"]);
        held.roles_declared = true;
        held.database.insert("CREATE ROLE".into());
        held.database.insert("ALTER ANY ROLE".into());
        held.granted_objects
            .insert("app.customer".into(), BTreeSet::new());
        held.granted_schemas
            .insert("app".into(), ["CONTROL".to_owned()].into_iter().collect());
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "CONTROL");
        assert_eq!(gaps[0].securable, Securable::Object("app.customer".into()));
        held.granted_objects.insert(
            "app.customer".into(),
            ["CONTROL".to_owned()].into_iter().collect(),
        );
        assert!(missing(&held).is_empty());
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
        held.ledger_schema.clear();
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
        held.ledger_schema.remove("DELETE");
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

    /// The finding this split exists for. A project that manages only `app`
    /// never touches a `dbo` table, so demanding the probes' `SELECT` — or
    /// `ALTER`, or `VIEW DEFINITION` — on `SCHEMA::dbo` asks for access the
    /// deployment does not use. Forcing the ledger's schema into the managed
    /// set did exactly that.
    #[test]
    fn an_unmanaged_ledger_schema_is_not_asked_for_the_managed_permissions() {
        let held = everything(&["app"]);
        assert!(
            !held.schemas.contains_key("dbo"),
            "dbo is not managed here and must not be in the managed set"
        );
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));

        // But the ledger's own requirements are still checked there.
        let mut held = everything(&["app"]);
        held.ledger_schema.remove("SELECT");
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "SELECT");
        assert_eq!(gaps[0].securable(), "SCHEMA::dbo");
        // The *ledger* SELECT, not the probes' one: the two entries exist to be
        // told apart, and the reason is what tells them apart in the report.
        assert!(gaps[0].why.contains("recorded state"), "{gaps:?}");
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

    /// The regression the previous round's fix introduced, in the worse
    /// direction. `CREATE TABLE` at the database does not by itself let an
    /// account create `dbo.__pbps_state`: SQL Server also wants `ALTER` on the
    /// schema the table lands in. Dropping the ledger's schema from the managed
    /// set — which it had to be, for a project declaring nothing in `dbo` —
    /// took that check with it, so `doctor` said ready and `ensure_tables`
    /// failed on the first deployment.
    #[test]
    fn creating_the_ledger_needs_alter_on_its_schema() {
        let mut held = everything(&["app"]);
        assert!(
            held.ledger_objects.is_empty(),
            "this is the before-first-deployment case"
        );
        held.ledger_schema.remove("ALTER");
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "ALTER");
        assert_eq!(gaps[0].securable(), "SCHEMA::dbo");
        assert!(gaps[0].why.contains("first use"), "{gaps:?}");
    }

    /// One surviving ledger table is not a created ledger. `ensure_tables`
    /// recreates whichever is missing, so reading `ledger_objects` as a single
    /// yes/no let an account with the lock table but not the state table pass
    /// readiness and then fail on its next `record`.
    #[test]
    fn a_half_present_ledger_still_needs_the_creation_permission() {
        let mut held = ledger_granted_on_the_objects_only(&["app"]);
        held.ledger_objects.remove(pbps_db::ledger::STATE_TABLE);
        assert_eq!(held.ledger_objects.len(), 1, "exactly one survives");
        held.ledger_schema.clear();

        let gaps = missing(&held);
        assert!(
            gaps.iter()
                .any(|g| g.permission == "ALTER" && g.securable() == "SCHEMA::dbo"),
            "the table still to be created needs the creation permission: {gaps:?}"
        );
    }

    /// A foreign key is authorized on the *referenced* table, and `ALTER` on
    /// the schema does not imply it — so an account holding everything else
    /// passed readiness and failed on one of the commonest changes there is.
    #[test]
    fn adding_a_foreign_key_needs_references_on_the_managed_schema() {
        let mut held = everything(&["app"]);
        held.schemas.get_mut("app").unwrap().remove("REFERENCES");
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "REFERENCES");
        assert_eq!(gaps[0].securable(), "SCHEMA::app");
    }

    /// `CONTROL` is deliberately absent (see the note above `Held`), so the
    /// `ALTER` entry must not claim to cover a cross-schema transfer. This
    /// pins the claim, because a later edit widening it back would make the
    /// list assert something the engine does not honour.
    #[test]
    fn the_alter_requirement_does_not_claim_to_cover_every_change() {
        let alter = REQUIRED
            .iter()
            .find(|r| r.name == "ALTER" && matches!(r.needed, Needed::Managed))
            .expect("managed ALTER");
        assert!(alter.why.contains("most"), "{}", alter.why);
        assert!(!alter.why.contains("every"), "{}", alter.why);
        assert!(
            !REQUIRED
                .iter()
                .any(|r| r.name == "CONTROL" && !matches!(r.needed, Needed::Granted)),
            "CONTROL is demanded only on the securables a declared role is granted on; see the \
             note above `Held`"
        );
    }

    /// And spent once the tables exist: writing rows needs `INSERT` and
    /// `DELETE`, not `ALTER`. Demanding it forever would be the over-demand
    /// coming back by another route.
    #[test]
    fn an_existing_ledger_no_longer_needs_alter_on_its_schema() {
        let mut held = ledger_granted_on_the_objects_only(&["app"]);
        held.ledger_schema.clear();
        assert!(!held.ledger_objects.is_empty());
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
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
            !held.ledger_schema.contains("INSERT"),
            "the test's premise is wrong if the ledger schema still carries it"
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
        held.ledger_schema.remove("INSERT");
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].securable(), "SCHEMA::dbo");
    }

    /// The scope is chosen per ledger table, not once for the pair. An account
    /// with object grants on `__pbps_lock` alone passed readiness: the surviving
    /// object made the whole question object-scoped, so the DML `__pbps_state`
    /// would need once `ensure_tables` recreated it was asked about at neither
    /// scope — and the next state read was denied by a permission `doctor` had
    /// reported nothing about.
    #[test]
    fn a_half_present_ledger_asks_the_missing_table_at_schema_scope() {
        let mut held = ledger_granted_on_the_objects_only(&["app"]);
        held.ledger_objects.remove(pbps_db::ledger::STATE_TABLE);
        // The creation permission is the other half of this shape and has its
        // own test; granted here so the gaps below are only the DML ones.
        held.ledger_schema.insert("ALTER".to_owned());

        let gaps = missing(&held);
        for permission in ["SELECT", "INSERT", "DELETE"] {
            assert!(
                gaps.iter()
                    .any(|g| g.permission == permission && g.securable() == "SCHEMA::dbo"),
                "{permission} on the table still to be created was not asked for: {gaps:?}"
            );
        }
        // And the surviving table is still answered where its grant actually
        // sits, or the same account would be told to re-grant what it holds.
        assert!(
            !gaps
                .iter()
                .any(|g| g.securable() == "OBJECT::dbo.__pbps_lock"),
            "{gaps:?}"
        );
    }

    /// The other direction, and the reason the fallback is deduplicated: with
    /// neither table present both fall back to the same schema, and an operator
    /// needs one `GRANT` line, not the same one twice.
    #[test]
    fn a_missing_ledger_reports_each_schema_permission_once() {
        let mut held = everything(&["dbo"]);
        assert!(held.ledger_objects.is_empty(), "the premise: no ledger yet");
        held.ledger_schema.remove("SELECT");

        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "SELECT");
        assert_eq!(gaps[0].securable(), "SCHEMA::dbo");
    }

    /// A trigger is authorized by ALTER on the table it is on, not by a CREATE
    /// of its own. Demanding one would send an organization to grant a
    /// permission its deployment does not use.
    #[test]
    fn only_the_three_module_kinds_that_need_a_create_have_one() {
        let creates: Vec<&str> = REQUIRED
            .iter()
            // `CREATE ROLE` is a role requirement, switched on by the
            // declarations (ADR-0005), not a module kind.
            .filter(|r| matches!(r.needed, Needed::Database))
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

    /// An owner is recognised by its *answers*, not by the `CONTROL` token.
    ///
    /// `HAS_PERMS_BY_NAME` accounts for inheritance, so a `CONTROL` holder's
    /// per-securable answers come back full on their own — which is why the
    /// short-circuit that used to sit at the top of `missing` bought nothing
    /// here, while costing the whole answer in the test below.
    #[test]
    fn an_owner_is_missing_nothing_without_a_control_shortcut() {
        let mut held = everything(&["dbo", "app"]);
        held.database.insert("CONTROL".to_owned());
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
    }

    /// And the case the shortcut hid. `DENY` beats an inherited `CONTROL` at
    /// the narrower securable and can arrive through any role the principal is
    /// in, so the two signals genuinely disagree — measured against a real
    /// server: with `CONTROL` on the database and `DENY ALTER ON SCHEMA::app`,
    /// `sys.fn_my_permissions` still lists `CONTROL`, `HAS_PERMS_BY_NAME`
    /// answers 0, and `CREATE TABLE app.t` fails. Reading the first of those
    /// three and returning early called that account ready.
    #[test]
    fn a_deny_at_a_narrower_scope_is_a_gap_even_with_control() {
        let mut held = everything(&["app"]);
        held.database.insert("CONTROL".to_owned());
        held.schemas.get_mut("app").unwrap().remove("ALTER");

        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "ALTER");
        assert_eq!(gaps[0].securable(), "SCHEMA::app");
    }

    /// The negative case: an empty answer is a real state — a login mapped to
    /// no user in this database — and must not be mistaken for "fine". `dbo`
    /// always exists, so it is always asked about and always reported.
    ///
    /// Counted against the requirements that *apply* rather than against
    /// `REQUIRED.len()`. A project with no foreign key out of its own schemas
    /// has nothing for the `Referenced` entries to be missing on, and a raw
    /// length equality would have made adding them look like a regression here
    /// while saying nothing about what this test is for.
    #[test]
    fn holding_nothing_is_reported_as_missing_everything() {
        let held = Held {
            database: BTreeSet::new(),
            schemas: [("dbo".to_owned(), BTreeSet::new())].into_iter().collect(),
            absent_schemas: BTreeSet::new(),
            ledger_schema: BTreeSet::new(),
            ledger_objects: BTreeMap::new(),
            referenced_objects: BTreeMap::new(),
            roles_declared: false,
            granted_objects: BTreeMap::new(),
            granted_schemas: BTreeMap::new(),
        };
        // Not the ones that depend on what the project declares: no foreign
        // key out of the managed schemas, and no role, means none of those
        // are asked about at all.
        let applicable = REQUIRED
            .iter()
            .filter(|r| {
                !matches!(
                    r.needed,
                    Needed::Referenced | Needed::RoleAdmin | Needed::Granted
                )
            })
            .count();
        assert_eq!(missing(&held).len(), applicable);
    }

    /// A foreign key into a schema this project does not manage. `REFERENCES`
    /// is authorized on the referenced table and the pre-flight probe reads it,
    /// and neither is covered by anything asked about the managed schemas — so
    /// a login could pass readiness and fail during `apply`.
    #[test]
    fn a_foreign_key_out_of_the_managed_schemas_is_asked_about_its_target() {
        let mut held = everything(&["app"]);
        held.referenced_objects
            .insert("shared.parent".to_owned(), BTreeSet::new());

        let gaps = missing(&held);
        for permission in ["REFERENCES", "SELECT"] {
            assert!(
                gaps.iter().any(|g| g.permission == permission
                    && g.securable() == "OBJECT::shared.parent"),
                "{permission} on the referenced table was not asked for: {gaps:?}"
            );
        }
        // And nothing wider: demanding anything on the whole of somebody else's
        // schema is the over-demand this check exists to avoid.
        assert!(
            !gaps.iter().any(|g| g.securable() == "SCHEMA::shared"),
            "{gaps:?}"
        );
    }

    /// Held on the target, so nothing is reported — the ordinary case for a
    /// deployment account a DBA has granted correctly.
    #[test]
    fn a_granted_foreign_key_target_reports_nothing() {
        let mut held = everything(&["app"]);
        held.referenced_objects.insert(
            "shared.parent".to_owned(),
            ["REFERENCES", "SELECT"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
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
