//! Readiness questions only a connection can answer (SPEC §14.1).
//!
//! # Why the permissions are asked for rather than tried
//!
//! The obvious way to find out whether the deployment role can create the
//! ledger is to create it. That is a write, and `doctor` is the command someone
//! runs when they are not yet sure what they are pointed at — quite possibly
//! production. So this asks the server what the role is allowed to do and
//! reports the gap, which costs one read and cannot change anything.
//!
//! # Why this list is not the SQL Server list with the words changed
//!
//! **On this engine, DDL is authorized by ownership and not by a privilege**,
//! and that is not a detail — it is the difference between a readiness check
//! and a readiness check that always says yes. Measured on 18.6, as a role
//! holding `GRANT ALL PRIVILEGES ON own.t`:
//!
//! ```text
//! has_table_privilege('own.t', 'SELECT,INSERT,UPDATE,DELETE,REFERENCES,TRIGGER')  ->  t
//! ALTER TABLE own.t ADD COLUMN c int   ->  42501: must be owner of table t
//! CREATE INDEX ix_t ON own.t (v)       ->  42501: must be owner of table t
//! DROP TABLE own.t                     ->  42501: must be owner of table t
//! ```
//!
//! Every privilege PostgreSQL has to give was held, and not one of the three
//! statements a plan is made of could run. A list ported from the other dialect
//! would ask `has_table_privilege` about a vocabulary that is real here, get
//! `true` for all of it, and report an environment ready that cannot alter a
//! single table. So the question this module asks about a table pbps manages is
//! `pg_has_role(current_user, relowner, 'USAGE')` — is this role the owner, or
//! a member of the role that owns it — and the privileges are asked for where
//! privileges are what the engine really checks: reading rows, and writing the
//! ledger.
//!
//! # What the catalog will and will not hide
//!
//! The other engine's catalog hides an object from a login with no permission
//! on it, which is why its ledger probe has to be a statement rather than a
//! lookup (DECISIONS 219). This one does not: `pg_class` and `pg_namespace` are
//! world-readable, so `present` below is truthful even for a schema this role
//! cannot enter — measured, `hidden.t` reads back `present = true,
//! owned = false, SELECT = false` with no error at all, while *querying* it is
//! `42501: permission denied for schema hidden`. That is what lets this module
//! report "the object is there and you may not touch it" apart from "it is not
//! there", which are two different remedies.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError, Param};
use pbps_model::ObjectName;

use crate::quote;
use crate::state::{LEDGER_SCHEMA, LOCK_TABLE, STATE_TABLE};

/// Where a permission has to be held for a deployment to succeed.
///
/// The scopes are this engine's, not the other's. There is no database-scoped
/// `CREATE TABLE` here, no `ALTER` on a schema, and no permission at all that
/// authorizes changing a table — see the module header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needed {
    /// Held on every schema pbps manages.
    ///
    /// `USAGE` and `CREATE` are the only two privileges a PostgreSQL schema
    /// takes, and both are needed for different halves of a deployment:
    /// `USAGE` to name anything inside the schema at all, `CREATE` to add a
    /// table, an index or a constraint to it.
    ManagedSchema,

    /// Held on each table pbps manages that already exists.
    ///
    /// Ownership, which is not a privilege and cannot be granted — it is
    /// transferred (`ALTER TABLE ... OWNER TO`) or inherited through role
    /// membership (`GRANT <owner> TO <deployer>`). This is the entry the other
    /// dialect has no counterpart for, and the one that makes this check worth
    /// running.
    Ownership,

    /// Held on each table pbps manages that already exists, as a privilege.
    ///
    /// Reading rows really is a privilege here, and the pre-flight probes
    /// (step 9) are `SELECT`s. An owner holds it implicitly, so this is only
    /// ever a gap for an owner who revoked it from themselves — which is rare,
    /// legal, and cheap to ask about.
    ManagedTable,

    /// Held on the ledger's schema, and only while the ledger is not there.
    ///
    /// Once the two tables exist the creation privilege is spent, and
    /// demanding it forever would ask every deployment for `CREATE` on
    /// `public` for the rest of the project's life.
    LedgerCreation,

    /// Held on the ledger's schema for as long as the ledger is used.
    ///
    /// `USAGE`, which is not spent when the tables are created and is not
    /// implied by any grant on them. **Measured on 18.6**, with `USAGE` on
    /// `public` revoked from a role holding `SELECT`, `INSERT` and `DELETE` on
    /// both ledger tables: `has_table_privilege` still answers `t` — the
    /// question is asked by oid and never resolves the name — while every
    /// statement is `42501: permission denied for schema public`. So a
    /// requirement that asked only about the two objects reported an
    /// environment ready in which no ledger statement can run.
    LedgerSchema,

    /// Held on the ledger and the lock themselves, once they exist.
    ///
    /// Asked on those two **objects**, not on their schema: a careful DBA
    /// grants `INSERT` and `DELETE` on exactly those two tables and nowhere
    /// else, and a schema-scoped question would report that as a gap.
    Ledger,

    /// Held on a table a declared foreign key *points at* which lies outside
    /// the schemas this project manages.
    ///
    /// `REFERENCES` is authorized on the referenced table, and the pre-flight
    /// probe for an added foreign key reads it — so both are needed there, and
    /// neither is covered by anything asked about the managed schemas.
    Referenced,

    /// Held on the schema a referenced table lives in.
    ///
    /// [`Needed::LedgerSchema`]'s rule, one securable out and found by sweeping
    /// for it: a grant on `shared.parent` reaches nothing without `USAGE` on
    /// `shared`, and nothing asked about the *managed* schemas can see that.
    ReferencedSchema,
}

/// A permission pbps needs, what needs it, and where it has to be held.
///
/// Named individually rather than as "make it a superuser": an organization
/// that grants the deployment role exactly what it needs should be able to see
/// the list, and "just use `postgres`" is the advice that makes every such
/// organization say no to the tool.
pub struct Requirement {
    pub name: &'static str,
    pub why: &'static str,
    pub needed: Needed,
}

const fn req(name: &'static str, why: &'static str, needed: Needed) -> Requirement {
    Requirement { name, why, needed }
}

/// The word this module uses for ownership where the others use a privilege.
///
/// Not a PostgreSQL keyword, and deliberately not spelled like one: no `GRANT`
/// can produce it, so a report that offered `GRANT OWNERSHIP ON ...` would be
/// a copy-pastable line that fails. The remedy is `ALTER TABLE ... OWNER TO`
/// or `GRANT <owning role> TO <deployer>`, and the caller writes it.
pub const OWNERSHIP: &str = "OWNERSHIP";

pub const REQUIRED: [Requirement; 12] = [
    req(
        "USAGE",
        "naming anything inside a schema pbps manages",
        Needed::ManagedSchema,
    ),
    req(
        "CREATE",
        "creating a declared table, and the indexes and constraints on it",
        Needed::ManagedSchema,
    ),
    // The entry with no counterpart on the other engine. See the module
    // header for the measurement.
    req(
        OWNERSHIP,
        "altering or dropping a table pbps manages: this engine authorizes that by ownership, \
         and no privilege confers it",
        Needed::Ownership,
    ),
    req(
        "SELECT",
        "the pre-flight probes, which count rows that would break",
        Needed::ManagedTable,
    ),
    req(
        "CREATE",
        "creating __pbps_state and __pbps_lock in their schema on first use",
        Needed::LedgerCreation,
    ),
    req(
        "USAGE",
        "naming the ledger and the lock at all, which no grant on them confers",
        Needed::LedgerSchema,
    ),
    req(
        "SELECT",
        "reading the recorded state and the deployment lock",
        Needed::Ledger,
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
    req(
        "REFERENCES",
        "adding a foreign key into a table this project does not manage, which the engine \
         authorizes on the referenced table",
        Needed::Referenced,
    ),
    // The other half of that foreign key, and the entry the SQL Server list
    // already carries: the probe for an added key reads the referenced table
    // (`NOT EXISTS (SELECT 1 FROM <parent> ...)`), and `REFERENCES` does not
    // confer a read.
    // The other half of that foreign key, and the entry the SQL Server list
    // already carries: the probe for an added key reads the referenced table
    // (`NOT EXISTS (SELECT 1 FROM <parent> ...)`), and `REFERENCES` does not
    // confer a read.
    req(
        "SELECT",
        "the pre-flight probe for that foreign key, which reads the referenced table",
        Needed::Referenced,
    ),
    req(
        "USAGE",
        "naming that referenced table at all, which no grant on it confers",
        Needed::ReferencedSchema,
    ),
];

// # A demand deliberately absent: `CREATE` on the database
//
// Creating a *schema* needs it, and the emitter never writes `CREATE SCHEMA` —
// a managed schema that is not there is reported as absent
// ([`Held::absent_schemas`]) rather than created, exactly as on the other
// engine. `CREATE` on the database also covers creating any schema at all,
// which is a much wider grant than a deployment needs, so asking for it on
// spec would be the "just make it an owner" pressure this list refuses to
// apply. It is also why this dialect's ledger lives in `public` rather than in
// a `pbps` schema of its own (see [`crate::state`]).

/// What a role holds on one schema. Two privileges, because two is all a
/// PostgreSQL schema has.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SchemaRights {
    pub usage: bool,
    pub create: bool,
}

impl SchemaRights {
    fn holds(&self, permission: &str) -> bool {
        match permission {
            "USAGE" => self.usage,
            "CREATE" => self.create,
            // A requirement scoped to a schema that names some other word is a
            // mistake in [`REQUIRED`], not a gap in the environment. Reporting
            // it as held would hide the mistake; reporting it as missing would
            // print a `GRANT` that PostgreSQL refuses.
            _ => unreachable!("a schema takes only USAGE and CREATE, not {permission}"),
        }
    }
}

/// What a role holds on one table.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TableRights {
    /// Whether this role owns the table, or is a member of the role that does.
    ///
    /// The question `pg_has_role(current_user, relowner, 'USAGE')` asks, which
    /// is the same one the engine asks before it accepts an `ALTER TABLE`.
    pub owned: bool,

    /// The privileges effective on it, of the ones [`REQUIRED`] asks about.
    pub privileges: BTreeSet<String>,
}

/// What the connected role effectively holds, per securable.
///
/// A securable that is **absent** is absent from these maps rather than
/// present and empty. The two answers have different remedies — grant
/// something, or create something — and flattening them would let `doctor`
/// print a `GRANT` on an object that is not there.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Held {
    /// Per managed schema that exists, what is held on it.
    pub schemas: BTreeMap<String, SchemaRights>,

    /// Managed schemas the database does not have.
    ///
    /// Not a permission problem, and not nothing either: the emitter never
    /// writes `CREATE SCHEMA`, so a plan declaring `app.customer` against a
    /// database with no `app` fails on its first statement.
    pub absent_schemas: BTreeSet<String>,

    /// What is held on the ledger's own schema.
    ///
    /// Kept apart from `schemas` because it answers a different question: it
    /// is the fallback for the ledger's requirements while the ledger does not
    /// exist, not a managed schema in its own right. `None` means the schema
    /// itself is not there — on this engine that is reachable, because
    /// `public` can be dropped.
    pub ledger_schema: Option<SchemaRights>,

    /// Per ledger table that exists, what is held on it. Empty before the
    /// first deployment, in which case [`missing`] falls back to the schema.
    pub ledger_objects: BTreeMap<ObjectName, TableRights>,

    /// Per managed table that exists, what is held on it.
    pub tables: BTreeMap<ObjectName, TableRights>,

    /// Managed tables the database does not have yet.
    ///
    /// Carried rather than dropped so that a caller can say how much of the
    /// answer is about tables that do not exist: a first deployment creates
    /// them, and the role that creates a table owns it, so no ownership gap
    /// can be reported against one.
    pub absent_tables: BTreeSet<ObjectName>,

    /// Per schema a referenced target lives in, what is held on it. A schema
    /// that is not there is absent from the map, like a managed one.
    pub referenced_schemas: BTreeMap<String, SchemaRights>,

    /// Per foreign-key target outside the managed schemas that exists, what is
    /// held on it. A target the database does not have is absent from this map
    /// rather than present and empty: there is no securable to ask about, and
    /// reporting a gap would fire on every project whose referenced table is
    /// created by somebody else's deployment.
    pub referenced_objects: BTreeMap<ObjectName, TableRights>,
}

/// Where a [`Gap`] is, kept typed so the report cannot spell one of them
/// wrongly and so a caller can tell them apart without parsing prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Securable {
    Schema(String),
    Object(ObjectName),
}

impl std::fmt::Display for Securable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Securable::Schema(s) => write!(f, "SCHEMA {}", spelled(s)),
            // Each part quoted on its own and joined with a dot: `TABLE
            // "app"."a.b"` is one table, while `TABLE app.a.b` is a `b` in some
            // schema `app.a` — a different securable, and the one the reader
            // would grant on.
            Securable::Object(o) => write!(f, "TABLE {}.{}", spelled(&o.schema), spelled(&o.name)),
        }
    }
}

/// One part of a securable's name, quoted the way the emitter spells every
/// identifier.
///
/// The report offers this label as the securable a `GRANT` names, and a reader
/// pastes it. **Always quoted, even where it would not have to be**: this
/// engine folds an unquoted name to lower case, so `GRANT ... ON TABLE
/// app.Customer` names `app.customer` — a securable the gap is not about — and
/// a name holding a space or a `"` is a syntax error. Quoting every part is the
/// rule the emitter follows for the same reason.
///
/// A name [`quote`] refuses is one no statement can carry, so the label keeps
/// the name and says it is not one rather than printing something that looks
/// pasteable and is not.
fn spelled(part: &str) -> String {
    quote(part).unwrap_or_else(|_| format!("<unquotable: {part}>"))
}

/// A permission the deployment needs and the connected role does not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    pub permission: &'static str,
    pub why: &'static str,
    /// Where it is missing, already spelled the way a `GRANT` names it.
    pub securable: Securable,
}

impl Gap {
    /// How the securable is named in a `GRANT`, which is how the report names
    /// it.
    pub fn securable(&self) -> String {
        self.securable.to_string()
    }
}

/// The ledger tables as objects, from the names [`crate::state`] owns.
pub fn ledger_tables() -> [ObjectName; 2] {
    [STATE_TABLE, LOCK_TABLE].map(|t| {
        t.parse()
            .expect("the ledger table names are this crate's own `schema.table` constants")
    })
}

/// What `doctor` asks about, which is what the project declares.
///
/// A borrowed struct rather than three positional arguments: the three lists
/// are all `&[ObjectName]`-shaped, and a caller that swapped two of them would
/// compile and ask the wrong questions.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    /// Every schema the project manages.
    pub managed_schemas: &'a [String],
    /// Every table the project declares, whether or not it exists yet.
    pub managed_tables: &'a [ObjectName],
    /// Every table a declared foreign key points at from outside the managed
    /// schemas.
    pub referenced: &'a [ObjectName],
}

/// The server's version, both as a person reads it and as a comparison does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// `18.6 (Debian 18.6-1.pgdg13+2)` — what `doctor` prints.
    pub text: String,
    /// `180006` — `server_version_num`, which is major * 10000 + minor.
    ///
    /// The number, not the text, is what a gate compares. ADR-0010's amendment
    /// puts `maintain` at PostgreSQL 17 and up as a *connected* check, because
    /// the model has no server version; #81 writes that refusal and reads this.
    /// Parsing the text for it would mean parsing `18.6 (Debian ...)`, and the
    /// engine already offers the number.
    pub number: i32,
}

const SERVER_VERSION: &str = "\
SELECT current_setting('server_version') AS version_text,
       current_setting('server_version_num')::int AS version_number";

/// What the server says it is.
pub async fn server_version(conn: &mut Conn) -> Result<Version, DbError> {
    let rows = conn.query(SERVER_VERSION).await?;
    let row = rows
        .first()
        .ok_or_else(|| DbError::BadRow("the server reported no version".into()))?;
    Ok(Version {
        text: row
            .try_get::<&str>("version_text")?
            .ok_or_else(|| DbError::BadRow("`server_version` is unexpectedly NULL".into()))?
            .to_owned(),
        number: row
            .try_get::<i32>("version_number")?
            .ok_or_else(|| DbError::BadRow("`server_version_num` is unexpectedly NULL".into()))?,
    })
}

/// The schema question, for one list of schemas.
///
/// The names are **bound**, not pasted, and the rows are matched back in Rust:
/// a schema called `a.b` or one holding a quote is a name the catalog can hold,
/// and building an `IN` list out of such names is how a readiness check ends up
/// asking about the wrong securable — or ends the statement early.
///
/// `LEFT JOIN`, so that a schema which is not there comes back as a row saying
/// so. An inner join would return nothing for it, and "no row" is how an absent
/// schema turns into "nothing to report".
fn schema_question(count: usize) -> String {
    format!(
        "WITH wanted(schema_name) AS (VALUES {})
SELECT w.schema_name,
       n.oid IS NOT NULL AS present,
       pg_catalog.has_schema_privilege(n.oid, 'USAGE') AS usage_ok,
       pg_catalog.has_schema_privilege(n.oid, 'CREATE') AS create_ok
  FROM wanted w
  LEFT JOIN pg_catalog.pg_namespace n ON n.nspname = w.schema_name",
        values_list(count, 1)
    )
}

/// The table question, for one list of `schema.name` pairs.
///
/// `pg_has_role(current_user, c.relowner, 'USAGE')` is the ownership question,
/// asked the way the engine asks it: `USAGE` on a role means holding that
/// role's privileges without `SET ROLE`, which is what makes a member of the
/// owning role able to `ALTER` the table.
///
/// The kinds are the caller's, because the two lists ask about different things.
///
/// A **managed** table is an ordinary one (`r`), for the reason
/// [`crate::catalog`] filters on it: an index and a sequence are rows in
/// `pg_class` too, and a declared table whose name collides with one of them
/// must read as absent rather than as a table this role does not own. A
/// partitioned table is not a table this model can hold at all — the pull names
/// it as unexpressible rather than reading it.
///
/// A **referenced** target is not this project's, so what the model can hold
/// says nothing about it. Measured on 18.6, a foreign key into a partitioned
/// table is an ordinary declaration and is authorized on the parent:
///
/// ```text
/// CREATE TABLE app.child (..., pid integer REFERENCES shared.parent(id))
///   with shared.parent PARTITION BY RANGE (id)  ->  42501: permission denied for table parent
///   once REFERENCES is granted on it            ->  CREATE TABLE
/// ```
///
/// Read at `r` alone, that target came back *absent*, so no gap was reported
/// for a grant the very next `apply` needs — the under-demand this list exists
/// to remove.
fn table_question(count: usize, kinds: &[&str]) -> String {
    let kinds = kinds
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "WITH wanted(schema_name, table_name) AS (VALUES {})
SELECT w.schema_name, w.table_name,
       c.oid IS NOT NULL AS present,
       pg_catalog.pg_has_role(current_user, c.relowner, 'USAGE') AS owned,
       pg_catalog.has_table_privilege(c.oid, 'SELECT') AS select_ok,
       pg_catalog.has_table_privilege(c.oid, 'INSERT') AS insert_ok,
       pg_catalog.has_table_privilege(c.oid, 'DELETE') AS delete_ok,
       pg_catalog.has_table_privilege(c.oid, 'REFERENCES') AS references_ok
  FROM wanted w
  LEFT JOIN pg_catalog.pg_namespace n ON n.nspname = w.schema_name
  LEFT JOIN pg_catalog.pg_class c
         ON c.relnamespace = n.oid AND c.relname = w.table_name AND c.relkind IN ({kinds})",
        values_list(count, 2)
    )
}

/// A table this project manages, or one of the two the tool owns: an ordinary
/// table, and nothing else.
const MANAGED_KINDS: [&str; 1] = ["r"];

/// A table a declared foreign key points at: ordinary or partitioned, which are
/// the two kinds this engine lets a key reference.
const REFERENCED_KINDS: [&str; 2] = ["r", "p"];

/// `($1::text), ($2::text)` — or `($1::text, $2::text), ($3::text, $4::text)`
/// for pairs.
///
/// The cast is not decoration: an untyped parameter inside a `VALUES` list has
/// no type the planner can infer, and the server refuses the statement with
/// `could not determine data type of parameter $1`.
fn values_list(rows: usize, columns: usize) -> String {
    (0..rows)
        .map(|row| {
            let cells: Vec<String> = (0..columns)
                .map(|col| format!("${}::text", row * columns + col + 1))
                .collect();
            format!("({})", cells.join(", "))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Everything [`missing`] needs, in as few round trips as the questions allow.
///
/// Three reads: the schemas (managed, plus the ledger's), the tables (managed,
/// plus the ledger's two), and the referenced targets. Each is one statement
/// with every name bound.
pub async fn permissions(conn: &mut Conn, ask: &Ask<'_>) -> Result<Held, DbError> {
    let mut held = Held::default();

    // The ledger's schema is asked about alongside the managed ones and kept
    // apart in the answer: a project that manages `public` needs both answers,
    // and they are not the same question.
    let mut schemas: Vec<String> = ask.managed_schemas.to_vec();
    if !schemas.contains(&LEDGER_SCHEMA.to_owned()) {
        schemas.push(LEDGER_SCHEMA.to_owned());
    }
    // The schemas the referenced targets live in, asked about for the reason
    // `Needed::ReferencedSchema` gives: a grant on the table reaches nothing
    // without `USAGE` on the schema around it.
    for object in ask.referenced {
        if !schemas.contains(&object.schema) {
            schemas.push(object.schema.clone());
        }
    }
    let referenced_schemas: BTreeSet<&String> = ask.referenced.iter().map(|o| &o.schema).collect();
    for (name, present, rights) in read_schemas(conn, &schemas).await? {
        let managed = ask.managed_schemas.contains(&name);
        if name == LEDGER_SCHEMA {
            held.ledger_schema = present.then_some(rights);
        }
        if present && referenced_schemas.contains(&name) && !managed {
            held.referenced_schemas.insert(name.clone(), rights);
        }
        if !managed {
            continue;
        }
        if present {
            held.schemas.insert(name, rights);
        } else {
            held.absent_schemas.insert(name);
        }
    }

    let ledger = ledger_tables();
    let mut tables: Vec<ObjectName> = ask.managed_tables.to_vec();
    tables.extend(ledger.iter().cloned());
    for (object, present, rights) in read_tables(conn, &tables, &MANAGED_KINDS).await? {
        let is_ledger = ledger.contains(&object);
        match (present, is_ledger) {
            (true, true) => {
                held.ledger_objects.insert(object, rights);
            }
            (true, false) => {
                held.tables.insert(object, rights);
            }
            // A ledger table that is not there is not an absent *managed*
            // table: `ensure_tables` creates it, and what that needs is
            // `Needed::LedgerCreation` on the schema.
            (false, true) => {}
            (false, false) => {
                held.absent_tables.insert(object);
            }
        }
    }

    for (object, present, rights) in read_tables(conn, ask.referenced, &REFERENCED_KINDS).await? {
        if present {
            held.referenced_objects.insert(object, rights);
        }
    }

    Ok(held)
}

async fn read_schemas(
    conn: &mut Conn,
    schemas: &[String],
) -> Result<Vec<(String, bool, SchemaRights)>, DbError> {
    if schemas.is_empty() {
        return Ok(Vec::new());
    }
    let params: Vec<Param<'_>> = schemas.iter().map(|s| Param::Str(s.as_str())).collect();
    let rows = conn
        .query_with(&schema_question(schemas.len()), &params)
        .await?;
    rows.iter()
        .map(|row| {
            let name = text(row, "schema_name")?;
            let present = flag(row, "present")?;
            Ok((
                name,
                present,
                SchemaRights {
                    // NULL where the schema is absent, because the privilege
                    // functions are strict and were handed a NULL oid. Read as
                    // "not held", which is true, and never as the absence
                    // itself — `present` is what says that.
                    usage: optional_flag(row, "usage_ok")?.unwrap_or(false),
                    create: optional_flag(row, "create_ok")?.unwrap_or(false),
                },
            ))
        })
        .collect()
}

async fn read_tables(
    conn: &mut Conn,
    objects: &[ObjectName],
    kinds: &[&str],
) -> Result<Vec<(ObjectName, bool, TableRights)>, DbError> {
    if objects.is_empty() {
        return Ok(Vec::new());
    }
    let params: Vec<Param<'_>> = objects
        .iter()
        .flat_map(|o| [Param::Str(o.schema.as_str()), Param::Str(o.name.as_str())])
        .collect();
    let rows = conn
        .query_with(&table_question(objects.len(), kinds), &params)
        .await?;
    rows.iter()
        .map(|row| {
            let object = ObjectName {
                schema: text(row, "schema_name")?,
                name: text(row, "table_name")?,
            };
            let present = flag(row, "present")?;
            let mut privileges = BTreeSet::new();
            for (column, name) in [
                ("select_ok", "SELECT"),
                ("insert_ok", "INSERT"),
                ("delete_ok", "DELETE"),
                ("references_ok", "REFERENCES"),
            ] {
                if optional_flag(row, column)?.unwrap_or(false) {
                    privileges.insert(name.to_owned());
                }
            }
            Ok((
                object,
                present,
                TableRights {
                    owned: optional_flag(row, "owned")?.unwrap_or(false),
                    privileges,
                },
            ))
        })
        .collect()
}

/// Which requirements are not met, as gaps a report can print.
///
/// A securable that was **not asked about** produces no gap. An empty map means
/// "nothing to ask", which happens on a first deployment for every table the
/// project has still to create — and a table that does not exist has no owner
/// to be wrong, so demanding ownership of it would report a gap against every
/// project's first run.
pub fn missing(held: &Held) -> Vec<Gap> {
    let mut out = Vec::new();
    for r in &REQUIRED {
        match r.needed {
            Needed::ManagedSchema => {
                for (schema, rights) in &held.schemas {
                    if !rights.holds(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: Securable::Schema(schema.clone()),
                        });
                    }
                }
            }
            Needed::Ownership => {
                for (object, rights) in &held.tables {
                    if !rights.owned {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: Securable::Object(object.clone()),
                        });
                    }
                }
            }
            Needed::ManagedTable => {
                for (object, rights) in &held.tables {
                    if !rights.privileges.contains(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: Securable::Object(object.clone()),
                        });
                    }
                }
            }
            // Only while the ledger is still to be created, and per table
            // rather than "either of them exists": `ensure_tables` creates
            // whichever is missing, so one surviving table does not mean the
            // creation privilege is spent.
            Needed::LedgerCreation if held.ledger_objects.len() < ledger_tables().len() => {
                // `None` is the ledger's schema being absent, which is a
                // different report and not a gap in a grant — there is no
                // securable to grant on. It is carried on [`Held`] for the
                // caller to say so.
                if let Some(rights) = held.ledger_schema
                    && !rights.holds(r.name)
                {
                    out.push(Gap {
                        permission: r.name,
                        why: r.why,
                        securable: Securable::Schema(LEDGER_SCHEMA.to_owned()),
                    });
                }
            }
            Needed::LedgerCreation => {}
            // Not spent when the ledger is created, unlike the branch above:
            // every statement that names the ledger needs it, for as long as
            // the ledger is used.
            Needed::LedgerSchema => {
                if let Some(rights) = held.ledger_schema
                    && !rights.holds(r.name)
                {
                    out.push(Gap {
                        permission: r.name,
                        why: r.why,
                        securable: Securable::Schema(LEDGER_SCHEMA.to_owned()),
                    });
                }
            }
            Needed::Ledger => {
                for (object, rights) in &held.ledger_objects {
                    if !rights.privileges.contains(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: Securable::Object(object.clone()),
                        });
                    }
                }
            }
            Needed::Referenced => {
                for (object, rights) in &held.referenced_objects {
                    if !rights.privileges.contains(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: Securable::Object(object.clone()),
                        });
                    }
                }
            }
            Needed::ReferencedSchema => {
                for (schema, rights) in &held.referenced_schemas {
                    if !rights.holds(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: Securable::Schema(schema.clone()),
                        });
                    }
                }
            }
        }
    }
    out
}

fn missing_column(column: &str) -> DbError {
    DbError::BadRow(format!(
        "the readiness query returned no `{column}`, which means the query and this code have \
         gone out of step"
    ))
}

fn text(row: &pbps_db::Row, column: &str) -> Result<String, DbError> {
    Ok(row
        .try_get::<&str>(column)?
        .ok_or_else(|| missing_column(column))?
        .to_owned())
}

fn flag(row: &pbps_db::Row, column: &str) -> Result<bool, DbError> {
    optional_flag(row, column)?.ok_or_else(|| missing_column(column))
}

fn optional_flag(row: &pbps_db::Row, column: &str) -> Result<Option<bool>, DbError> {
    row.try_get::<bool>(column)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(schema: &str, name: &str) -> ObjectName {
        ObjectName {
            schema: schema.to_owned(),
            name: name.to_owned(),
        }
    }

    fn rights(owned: bool, privileges: &[&str]) -> TableRights {
        TableRights {
            owned,
            privileges: privileges.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    /// The measurement in the module header, as a rule the code has to keep: a
    /// table this role holds every privilege on and does not own is a gap,
    /// because none of those privileges lets it run an `ALTER TABLE`.
    #[test]
    fn every_privilege_and_no_ownership_is_still_a_gap() {
        let mut held = Held::default();
        held.tables.insert(
            object("app", "customer"),
            rights(false, &["SELECT", "INSERT", "DELETE", "REFERENCES"]),
        );
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, OWNERSHIP);
        assert_eq!(gaps[0].securable(), "TABLE \"app\".\"customer\"");
    }

    /// And the converse, so the rule is not "always report ownership": the
    /// owner of a table it can read has nothing missing on it.
    #[test]
    fn an_owner_that_can_read_its_own_table_has_no_gap() {
        let mut held = Held::default();
        held.tables
            .insert(object("app", "customer"), rights(true, &["SELECT"]));
        assert_eq!(missing(&held), Vec::new());
    }

    /// A table that does not exist yet is not a table without an owner. The
    /// deployment that creates it owns it, so a gap reported here would fire on
    /// every project's first run.
    #[test]
    fn a_table_the_deployment_has_still_to_create_is_not_an_ownership_gap() {
        let mut held = Held::default();
        held.absent_tables.insert(object("app", "customer"));
        assert_eq!(missing(&held), Vec::new());
    }

    /// The create-time privilege is asked for while the ledger is not there and
    /// not once it is. Demanding it forever would ask every deployment to keep
    /// `CREATE` on the ledger's schema for the life of the project.
    #[test]
    fn the_ledgers_create_privilege_is_asked_for_only_until_the_ledger_exists() {
        let mut held = Held {
            ledger_schema: Some(SchemaRights {
                usage: true,
                create: false,
            }),
            ..Held::default()
        };
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "CREATE");
        assert_eq!(gaps[0].securable(), "SCHEMA \"public\"");

        // Both tables there: the privilege is spent, and what is asked for
        // instead is what writing a row needs.
        for table in ledger_tables() {
            held.ledger_objects
                .insert(table, rights(true, &["SELECT", "INSERT", "DELETE"]));
        }
        assert_eq!(missing(&held), Vec::new());
    }

    /// One ledger table present and one missing is still "the ledger has to be
    /// created": `ensure_tables` creates whichever is absent, and reading the
    /// pair as a single yes/no let an account with the lock table and not the
    /// state table pass readiness and fail on the next `record`.
    #[test]
    fn one_ledger_table_of_two_does_not_spend_the_create_privilege() {
        let mut held = Held {
            ledger_schema: Some(SchemaRights {
                usage: true,
                create: false,
            }),
            ..Held::default()
        };
        held.ledger_objects.insert(
            ledger_tables()[1].clone(),
            rights(true, &["SELECT", "INSERT", "DELETE"]),
        );
        let gaps = missing(&held);
        assert!(
            gaps.iter()
                .any(|g| g.permission == "CREATE" && g.securable() == "SCHEMA \"public\""),
            "{gaps:?}"
        );
    }

    /// A ledger table that exists and cannot be written to is the gap that
    /// leaves a stale lock behind: the DDL commits, the release fails, and the
    /// next pipeline is blocked by a lock nobody holds.
    #[test]
    fn a_ledger_this_role_cannot_delete_from_is_reported_before_the_apply() {
        let mut held = Held {
            ledger_schema: Some(SchemaRights {
                usage: true,
                create: true,
            }),
            ..Held::default()
        };
        for table in ledger_tables() {
            held.ledger_objects
                .insert(table, rights(true, &["SELECT", "INSERT"]));
        }
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        assert!(gaps.iter().all(|g| g.permission == "DELETE"), "{gaps:?}");
    }

    /// The ledger's schema is asked about for as long as the ledger is used,
    /// not only while it is being created. Measured on 18.6, a role holding
    /// every privilege on both tables and no `USAGE` on their schema cannot run
    /// one ledger statement — and `has_table_privilege` says nothing about it,
    /// because it is asked by oid and never resolves the name.
    #[test]
    fn a_ledger_whose_schema_this_role_cannot_enter_is_a_gap_after_it_exists() {
        let mut held = Held {
            ledger_schema: Some(SchemaRights {
                usage: false,
                create: true,
            }),
            ..Held::default()
        };
        for table in ledger_tables() {
            held.ledger_objects
                .insert(table, rights(true, &["SELECT", "INSERT", "DELETE"]));
        }
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "USAGE");
        assert_eq!(gaps[0].securable(), "SCHEMA \"public\"");
    }

    /// A foreign key into a table this project does not manage needs the read
    /// as well as the reference: the probe for the key counts rows in that
    /// table, and `REFERENCES` confers no read.
    #[test]
    fn a_referenced_table_has_to_be_readable_as_well_as_referable() {
        let mut held = Held::default();
        held.referenced_objects
            .insert(object("shared", "parent"), rights(false, &["REFERENCES"]));
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "SELECT");
        assert_eq!(gaps[0].securable(), "TABLE \"shared\".\"parent\"");
    }

    /// And the schema around it: the same rule as the ledger's, one securable
    /// out, which is where a sweep for the shape found it.
    #[test]
    fn a_referenced_tables_schema_has_to_be_enterable() {
        let mut held = Held::default();
        held.referenced_objects.insert(
            object("shared", "parent"),
            rights(false, &["REFERENCES", "SELECT"]),
        );
        held.referenced_schemas.insert(
            "shared".to_owned(),
            SchemaRights {
                usage: false,
                create: false,
            },
        );
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "USAGE");
        assert_eq!(gaps[0].securable(), "SCHEMA \"shared\"");
    }

    /// A schema that is not there produces no `GRANT` advice, because there is
    /// nothing to grant on. It is carried on [`Held::absent_schemas`] instead,
    /// where the caller reports it as what it is.
    #[test]
    fn an_absent_schema_is_not_reported_as_a_permission_gap() {
        let mut held = Held::default();
        held.absent_schemas.insert("app".to_owned());
        assert_eq!(missing(&held), Vec::new());
        assert!(held.schemas.is_empty());
    }

    /// The securable is spelled as a `GRANT` names it, quoted per part. An
    /// unquoted name would fold to lower case here — `TABLE app.Customer`
    /// grants on `app.customer` — and a name holding a dot would name a
    /// different object altogether.
    #[test]
    fn a_securable_is_spelled_the_way_a_grant_names_it() {
        assert_eq!(
            Securable::Object(object("app", "Customer")).to_string(),
            "TABLE \"app\".\"Customer\""
        );
        assert_eq!(
            Securable::Object(object("app", "a.b")).to_string(),
            "TABLE \"app\".\"a.b\""
        );
        assert_eq!(
            Securable::Schema("Reporting".to_owned()).to_string(),
            "SCHEMA \"Reporting\""
        );
    }

    /// Every parameter in a generated `VALUES` list is numbered and cast. A
    /// missing cast is `could not determine data type of parameter $1` from the
    /// server, and a misnumbered one asks about the wrong name.
    #[test]
    fn the_bound_names_are_numbered_in_the_order_they_are_passed() {
        assert_eq!(values_list(1, 1), "($1::text)");
        assert_eq!(values_list(2, 1), "($1::text), ($2::text)");
        assert_eq!(
            values_list(2, 2),
            "($1::text, $2::text), ($3::text, $4::text)"
        );
    }

    /// No name reaches the server as text pasted into a statement. A schema
    /// called `a'; DROP` is a name this catalog can hold, and the only place a
    /// name appears in these statements is a placeholder.
    #[test]
    fn every_name_a_question_asks_about_is_bound() {
        for (sql, columns) in [
            (schema_question(3), 1),
            (table_question(3, &MANAGED_KINDS), 2),
        ] {
            let cells = values_list(3, columns);
            assert!(!cells.contains('\''), "{cells}");
            assert!(sql.contains(&cells), "{sql}");
            // Nothing but the generated cells sits between `VALUES` and the
            // end of the CTE, so there is no room for a pasted name.
            let after = sql.split("VALUES ").nth(1).expect("a VALUES list");
            assert!(after.starts_with(&cells), "{sql}");
        }
    }

    /// Both questions `LEFT JOIN`, so that a securable which is not there comes
    /// back as a row saying so. An inner join would drop it, and an absent
    /// schema would arrive as "nothing to report" — which is the direction this
    /// project keeps getting bitten by.
    #[test]
    fn a_securable_that_is_absent_still_comes_back_as_a_row() {
        for sql in [schema_question(1), table_question(1, &MANAGED_KINDS)] {
            assert!(sql.contains("LEFT JOIN"), "{sql}");
            assert!(sql.contains("IS NOT NULL AS present"), "{sql}");
        }
    }

    /// The two lists ask about different kinds, and the difference is measured:
    /// a partitioned table is not a table this model can hold, and it is a
    /// perfectly ordinary thing for somebody else's foreign-key target to be.
    /// Asked at `r` alone, such a target read as absent and no grant was
    /// demanded for it.
    #[test]
    fn a_referenced_target_may_be_partitioned_where_a_managed_table_may_not() {
        assert!(
            table_question(1, &MANAGED_KINDS).contains("c.relkind IN ('r')"),
            "{}",
            table_question(1, &MANAGED_KINDS)
        );
        assert!(
            table_question(1, &REFERENCED_KINDS).contains("c.relkind IN ('r', 'p')"),
            "{}",
            table_question(1, &REFERENCED_KINDS)
        );
    }

    /// Ownership is asked as the engine asks it, and about the table's own
    /// owner. Asking `pg_has_role(..., 'MEMBER')` instead would answer about a
    /// role this one can `SET ROLE` to without holding its privileges.
    #[test]
    fn ownership_is_asked_as_the_engine_asks_it() {
        let sql = table_question(1, &MANAGED_KINDS);
        assert!(
            sql.contains("pg_has_role(current_user, c.relowner, 'USAGE') AS owned"),
            "{sql}"
        );
    }
}
