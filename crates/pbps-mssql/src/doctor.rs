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
use pbps_model::ObjectName;

use crate::catalog::get;

// What `doctor` asks about is read off the declarations and is `pbps-db`'s;
// what this engine answers, and how, is below (DECISIONS 417).
pub use pbps_db::doctor::{DataDemand, DataTables, DeclaredKeys, GrantTargets, ReferencedColumns};

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
    /// Needed on every schema pbps manages.
    Managed,
    /// Probe reads on each managed table, including recorded tables awaiting
    /// a drop. Existing tables accept object or complete column grants;
    /// missing tables retain the schema grant needed before their creation.
    ManagedTable,
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

    /// `ALTER` on an existing state ledger, only while its timeline columns
    /// still need migration (DEC-416.1, applying DECISIONS 435).
    LedgerMigration,

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

    /// Needed on a table a declared foreign key *points at* which this project
    /// does not declare.
    ///
    /// `REFERENCES` is authorized on the referenced table, and the pre-flight
    /// probe for an added foreign key reads it (`NOT EXISTS (SELECT 1 FROM
    /// <parent> ...)`) — so both are needed there, and neither is covered by
    /// anything asked about the managed schemas. `validate` accepts a foreign
    /// key whose target is undeclared, and the emitter really does write
    /// `REFERENCES [shared].[parent]`, so this is reachable from an ordinary
    /// project rather than a contrived one.
    ///
    /// Asked at **object** scope, and of every undeclared target — including
    /// one that shares a schema with the declarations, which
    /// [`Needed::ManagedTable`] does not reach (DECISIONS 509). Asking at
    /// object scope does not report a covered half twice:
    /// `HAS_PERMS_BY_NAME` accounts for inheritance, so the schema's own
    /// `REFERENCES` answers 1 for a table under it. Demanding anything on the
    /// whole of someone else's schema stays the over-demand this enum exists
    /// to avoid.
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

    /// Needed on each **table** whose declaration would have a row inserted
    /// into it (ADR-0004, DECISIONS 235).
    ///
    /// `ALTER ON SCHEMA` confers no DML. A `data:` block makes the emitter
    /// write `INSERT INTO <managed table>` and `UPDATE <managed table> SET`,
    /// and nothing else in this list is asked at a scope that covers them —
    /// the `INSERT` above is `Needed::Ledger`, on the two `dbo` tables. So an
    /// account granted exactly what `doctor` printed passed readiness, `apply`
    /// took the lock and ran the DDL, and the first row died on "INSERT
    /// permission was denied". Under `--staged` the earlier checkpoints have
    /// already committed, which is the failure this command exists to prevent.
    ///
    /// Asked on the **object**, like the ledger's writes and for the same
    /// reason, measured on the pinned image: a login holding
    /// `GRANT INSERT ON app.t` and nothing wider answers 1 at object scope, 0
    /// at schema scope, and the `INSERT` really runs — so a schema-scoped
    /// question calls a careful DBA's grant a gap. The converse is worse:
    /// `GRANT INSERT ON SCHEMA::app` with `DENY INSERT ON app.t` answers 1 at
    /// schema scope, 0 at object scope, and the `INSERT` really fails — the
    /// under-demand this variant exists to remove, reintroduced one securable
    /// out.
    ///
    /// Before the table exists there is no object to ask about, so the
    /// question falls back to its schema — which is the only place a grant
    /// *can* sit in advance of the deployment that creates the table.
    ///
    /// Demanded of a project that declares rows and of no other, for the
    /// reason `RoleAdmin` is: whether the project needs it is visible in the
    /// declarations `doctor` already reads, and DML on a table someone else's
    /// application also writes to is not a permission to ask for on spec.
    DataInsert,

    /// Needed on each table whose declaration would have a row **corrected**
    /// in it (DECISIONS 235).
    ///
    /// Separate from [`Needed::DataInsert`] because a declaration can insert
    /// without ever updating. An `UPDATE` is built only from the columns a row
    /// can hold a value in — everything but the key and the engine's own
    /// `IDENTITY`s ([`Table::row_columns`]) — and emitted only if that is not
    /// empty. A table whose only column is its primary key is the commonest
    /// reference-data shape there is, and it can never produce one; demanding
    /// `UPDATE` of it reports a gap against an account that can run every
    /// statement the declaration can produce.
    ///
    /// Asked at the same scope, and of the same tables, as
    /// [`Needed::DataInsert`] — and, when the object answer is 0, on each of
    /// those columns in turn: SQL Server takes `GRANT UPDATE` on a column, and
    /// an account granted exactly the columns it writes holds nothing at
    /// object scope. See [`Columns`] for the measurement and for why the
    /// declared list is asked rather than the catalog's.
    DataUpdate,

    /// Needed on each table whose declaration would have a row **removed**
    /// from it — `mode: exact`, and no other (DECISIONS 235).
    ///
    /// Split from the other two because the modes differ in exactly this.
    /// `exact` says the declared rows are the whole table, so an undeclared
    /// row is a `DELETE`; `ensure` "never emits a DELETE — that is the promise
    /// the mode makes to a table the application also writes to", which is
    /// `DeleteCause::Undeclared`'s own wording in `pbps-model` and the
    /// condition the differ really tests. Asking for `DELETE` on an `ensure`
    /// table would demand row-removal rights on the very table the mode was
    /// chosen to keep pbps out of, which is the over-demand this enum exists
    /// to avoid.
    ///
    /// Asked at the same scope, and of the same tables, as
    /// [`Needed::DataInsert`].
    DataDelete,

    /// Needed on each table whose declaration carries rows, for the read that
    /// closes an apply (issue #516).
    ///
    /// `apply` reads the managed rows back before it records and commits, and
    /// [`crate::rows::query`] projects the columns the **declaration** names —
    /// the key and every writable cell ([`DataDemand::data_columns`]) — not
    /// the ones the catalog holds now. Those two lists differ in exactly the
    /// case this entry exists for: a plan that adds a column reads it back in
    /// the same run that creates it.
    ///
    /// `SELECT` is already demanded of every managed table
    /// ([`Needed::ManagedTable`]), but over the catalog's columns, so an
    /// account granted `SELECT` on each column that exists today answers 1
    /// there and the closing read still fails. Measured on the pinned image
    /// (17.0.4075.5) with `app.t(code, label)` and a declaration adding
    /// `extra`: the column-only grantee answers 1 on `code` and `label`, 0 on
    /// `extra` — a column the catalog does not have yet answers 0 rather than
    /// NULL — and its `SELECT code, label, extra` fails with error 230. An
    /// object-level grantee answers 1 at object scope, which the question
    /// takes before it reaches any column, and its read runs. So the remedy
    /// this reports is the grant that actually covers a column added later.
    ///
    /// Asked of every table in [`Held::data_tables`], because each of them is
    /// read back: `exact` reads every row whether or not one is declared, and
    /// an `ensure` block with no row demands nothing at all and is absent from
    /// that map. DECISIONS 510.
    DataRead,

    /// Needed on each table an `exact` declaration's delete count would read
    /// (issue #515).
    ///
    /// Before removing an undeclared row, `preflight::delete_probe` counts the
    /// rows still pointing at it — one `SELECT COUNT(*)` per table with an
    /// **enabled** foreign key into the parent, found in `sys.foreign_keys` at
    /// run time. Those children are not declared, need not be, and need not
    /// live in a schema this project manages, so nothing else in this list
    /// covers them: [`Needed::ManagedTable`] asks about the declared and
    /// recorded tables, and [`Needed::Referenced`] about the targets the
    /// declarations point *out* at, which is the opposite direction.
    ///
    /// Measured on the pinned image (17.0.4075.5): with `app.t` declared
    /// `mode: exact` and an undeclared `app.unmanaged(id)` referencing it, a
    /// deployer holding everything else the list asks for passed `doctor` with
    /// no gaps, and the probe's own count failed with error 229. Discovery
    /// measured on the same fixture: the query finds the child in the managed
    /// schema and one in a schema the project does not manage, and leaves out
    /// a child whose constraint is `NOCHECK`ed and a table with no key into
    /// the parent at all.
    ///
    /// Asked over the child's **foreign-key columns**, not its whole catalog,
    /// the way an external target is ([`Columns::Referenced`]). The count
    /// names the child only in the tuple the catalog gives it — `ch.<key
    /// column> = p.<referenced column>` — and the fragments that name a
    /// child's own key exist only for a child the *plan* moves, which is a
    /// declared table and therefore never in this list. Measured: a login
    /// holding `SELECT` on nothing but the foreign-key column is refused a
    /// plain `SELECT COUNT(*) FROM app.kid` (error 230, on the key column the
    /// engine picks for `COUNT(*)`) and **runs the count the probe actually
    /// writes**. So demanding every catalog column would report a gap against
    /// an account that can run every statement the declaration produces.
    ///
    /// A child the managed question already asks about is not asked again —
    /// see [`Held::delete_children`]. DECISIONS 511.
    DeleteChild,

    /// Needed at the **database**, and only when the declarations would have a
    /// reference-data row removed: the count that precedes such a delete
    /// refuses to run until it can prove the row-level security policy catalog
    /// readable (DECISIONS 468, 505).
    ///
    /// Not covered by anything above it. `Needed::Managed`'s `VIEW DEFINITION`
    /// is asked on the schemas this project manages, which is what SPEC §9.5
    /// asks for and is right for reading the catalog — and it is not what the
    /// delete count demands. A security policy can live in a schema this
    /// project does not manage and does not declare, so a complete answer
    /// about "is any enabled FILTER predicate on the child I am about to
    /// cascade into" is a **database**-wide question. Measured on the pinned
    /// image, a parent-only deployer can see neither the foreign key nor the
    /// policy and its `DELETE` still cascades into the hidden child, which is
    /// why `counting_statement` asks before it discovers keys rather than
    /// after.
    ///
    /// So an account holding everything else in this list passed readiness and
    /// then met `Cannot count referencing rows: inspecting row-level security
    /// policies requires database VIEW DEFINITION.` at the first `mode: exact`
    /// delete. The count fails safely — this is a readiness diagnostic gap,
    /// not an accepted destructive plan — which is exactly what this command
    /// exists to remove.
    ///
    /// Demanded of a project that removes reference-data rows and of no other,
    /// for the reason [`Needed::RoleAdmin`] gives: database-wide `VIEW
    /// DEFINITION` is a broad ask, whether the project needs it is visible in
    /// the declarations `doctor` already reads, and `ensure` never emits a
    /// `DELETE`.
    DeleteCatalog,

    /// The other half of the same proof, one securable in: an **effective**
    /// object or schema metadata `DENY` that this account cannot see through.
    ///
    /// A database grant loses to it, so database `VIEW DEFINITION` answering 1
    /// is not the whole answer (DECISIONS 460). `counting_statement` refuses on
    /// the same condition with `an effective object/schema metadata DENY
    /// prevents a complete view of row-level security policies`, and a
    /// readiness command that reported only the database permission would send
    /// an operator to grant something they already hold.
    ///
    /// Reported as the permission that is effectively missing **on that
    /// securable**, because that is both the truth and the remedy: the probe's
    /// own test is `HAS_PERMS_BY_NAME(<that object or schema>, …, 'VIEW
    /// DEFINITION') <> 1`. Gathered only when a reference-data delete is
    /// declared, so a project that removes no rows is neither asked nor told.
    DeleteCatalogDenied,
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

/// The schema the ledger and the lock live in, asked of the module whose
/// statements put them there rather than spelled a second time here.
pub use crate::state::LEDGER_SCHEMA;

/// The tables `state::ensure_tables` creates, and therefore the ones whose
/// absence still requires the create-time permission.
pub const LEDGER_TABLES: [&str; 2] = [crate::state::STATE_TABLE, crate::state::LOCK_TABLE];

pub const REQUIRED: [Requirement; 25] = [
    req(
        "ALTER",
        "adding the timeline columns to an existing pre-migration state ledger",
        Needed::LedgerMigration,
    ),
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
        Needed::ManagedTable,
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
    // Reference data (ADR-0004), demanded only of the tables that declare
    // rows. `ALTER ON SCHEMA` confers none of these.
    req(
        "INSERT",
        "writing a declared row this table does not have",
        Needed::DataInsert,
    ),
    req(
        "UPDATE",
        "correcting a declared row whose values have drifted",
        Needed::DataUpdate,
    ),
    req(
        "DELETE",
        "removing an undeclared row from a table declared `mode: exact`",
        Needed::DataDelete,
    ),
    // The read that closes the apply, and the entry `SELECT` needs a third
    // time: the probes read the catalog's columns, the ledger read is two
    // tables in `dbo`, and this one reads the columns the declaration names —
    // including the one the same plan adds.
    req(
        "SELECT",
        "the row read-back that closes an apply, which projects the declared columns",
        Needed::DataRead,
    ),
    req(
        "SELECT",
        "the count before removing a row, which reads every table with an enabled foreign key \
         into it",
        Needed::DeleteChild,
    ),
    // The count that precedes that DELETE, which refuses to run until it can
    // prove the policy catalog readable. Two entries for one proof, because
    // the two halves are missing in two different places and a `GRANT` fixes
    // only the first.
    req(
        "VIEW DEFINITION",
        "the count before removing a row, which must prove no enabled row-level security \
         FILTER predicate can hide a child it would cascade into",
        Needed::DeleteCatalog,
    ),
    req(
        "VIEW DEFINITION",
        "the same count, which an effective metadata DENY on this securable stops from \
         seeing the whole policy catalog",
        Needed::DeleteCatalogDenied,
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
    /// which quietly demanded `ALTER` on `dbo` from a
    /// project that manages only `app` and never touches a `dbo` table.
    pub schemas: BTreeMap<String, BTreeSet<String>>,

    /// Probe rights at the securable that answers each managed table: the
    /// current object, or its creation schema before it exists. Keying by
    /// securable deduplicates shared fallbacks without merging the distinct
    /// needs of a renamed table and a new table reusing its old name.
    pub managed_tables: BTreeMap<Securable, BTreeSet<String>>,

    /// Managed schemas the database does not have — and schemas a managed
    /// role is granted on, for the same reason: `GRANT ... ON SCHEMA::x`
    /// fails on a schema that is not there.
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
    pub ledger_objects: BTreeMap<ObjectName, BTreeSet<String>>,

    /// The existing state table lacks the columns `ensure_tables` adds.
    pub ledger_migration_needed: bool,

    /// Per foreign-key target the declarations do not hold, the permissions
    /// effective on that **object**.
    ///
    /// A target the database does not have is absent from this map rather than
    /// present and empty: there is no securable to ask about, and reporting a
    /// gap there would fire on every project whose referenced table is created
    /// by something else's deployment. That the table is missing at all is a
    /// question for `plan --db`, which sees the change; `doctor` sees no plan.
    pub referenced_objects: BTreeMap<ObjectName, BTreeSet<String>>,

    /// Per table an `exact` declaration's delete count would read, the
    /// permissions effective on that **object** ([`Needed::DeleteChild`]).
    ///
    /// Empty for a project that removes no row: the count never runs, and the
    /// discovery query is not asked either.
    ///
    /// A child that is already a managed table is left out rather than
    /// answered here. `SELECT` on it is demanded by
    /// [`Needed::ManagedTable`] at the same securable, and two entries would
    /// print the operator the same `GRANT` twice.
    ///
    /// Asked for every discovered name, present or invisible, and over that
    /// child's foreign-key columns alone — the way the foreign-key targets
    /// are, and for both of their reasons (see [`Existing`] and
    /// [`Columns::Referenced`]): the name came out of the catalog, so an
    /// object question that answered nothing would drop a demand the count
    /// really makes, and the count reads no other column of it.
    pub delete_children: BTreeMap<Securable, BTreeSet<String>>,

    /// Whether the project has any role at all — declared, recorded, or being
    /// dropped. `false` switches the role requirements off rather than
    /// reporting them as gaps.
    pub roles_declared: bool,

    /// The securables carrying an **effective** metadata `DENY` this account
    /// cannot see through — the exact condition `counting_statement` refuses
    /// on before a reference-data delete (DECISIONS 460, 505).
    ///
    /// Empty and *not asked about* are the same thing here on purpose: the
    /// question is only put when the declarations would remove a row, so a
    /// project that removes none is neither asked nor told. That is the
    /// `roles_declared` bargain, and the read it saves is a database-wide scan
    /// of `sys.database_permissions`.
    pub metadata_denials: BTreeSet<Securable>,

    /// Per object, the permissions effective on it — asked about every named
    /// object, absent or invisible included, for the same reason
    /// `referenced_objects` is.
    ///
    /// Keyed by the name the object was actually asked about: the current
    /// physical name for a declared target this resolution judged safe, or
    /// the name as recorded or discovered for a target sourced from this
    /// environment's own recorded grants or the catalog, which already name
    /// the object as it stands and need no resolution.
    ///
    /// A declared target this resolution could not safely ask about at all —
    /// because the name its own resolution would use is already confirmed,
    /// by this environment's own recorded ids, to belong to a *different*
    /// identity — is not in this map. It is in
    /// [`Held::granted_unresolvable`] instead, which `missing` reports as an
    /// unconditional gap: GRANTED has no schema-scope fallback to fall back
    /// to (issue #133 round 2).
    pub granted_objects: BTreeMap<ObjectName, BTreeSet<String>>,

    /// A declared grant target whose current physical name this environment
    /// could not safely determine: the name its own resolution would use is
    /// already confirmed, by this environment's own recorded ids, to be a
    /// *different* identity's object. Asking under that name would read the
    /// other identity's permissions, not this target's — so it is not asked
    /// at all, and `missing` reports every GRANTED requirement against it as
    /// an unconditional gap instead.
    ///
    /// This is the same collision `Held::data_tables`'s declared-name keying
    /// exists to survive; GRANTED has no schema-scope fallback to fall back
    /// to, unlike the data requirements, so an unresolvable target cannot be
    /// silently skipped (issue #133 round 2).
    pub granted_unresolvable: BTreeSet<ObjectName>,

    /// Per schema a declared role is granted on (`schema::x`), the
    /// schema-scoped permissions effective on it. A schema the database does
    /// not have is absent, like a managed schema that does not exist yet.
    pub granted_schemas: BTreeMap<String, BTreeSet<String>>,

    /// Per table that declares rows, what those rows would have written to
    /// it (ADR-0004).
    ///
    /// A *predicate*, not a set of holdings: the permissions come from
    /// [`Held::data_objects`], keyed by [`Held::data_securable`]'s current
    /// name for the table, or from [`Held::schemas`] where the table does not
    /// exist yet or its current name could not be safely determined. A table
    /// that declares no row at all is absent, and so is one whose
    /// declaration can produce no statement — `mode: ensure` with no
    /// declared row manages nothing, so it is asked for nothing.
    ///
    /// Keyed by the **declared** name, not a resolved one: each of a plan's
    /// declared tables is unique by construction, and keying this map by a
    /// resolved physical name instead once let two declarations that resolve
    /// to the same name — a rename freeing a name a new declaration reuses in
    /// the same plan — collapse into one map entry, silently dropping one of
    /// the two demands (issue #133 round 2).
    pub data_tables: DataTables,

    /// Per declared table, the current physical name this resolution judged
    /// safe to ask about — absent for a table whose resolved name is already
    /// confirmed, by this environment's own recorded ids, to belong to a
    /// *different* declared table. `data_gaps` reads a table missing here as
    /// unresolved and falls back to the schema answer rather than ask under a
    /// name that would read someone else's object (issue #133 round 2).
    pub data_securable: BTreeMap<ObjectName, ObjectName>,

    /// Per current physical name named in [`Held::data_securable`] that the
    /// catalog shows, the permissions effective on that **object**.
    ///
    /// Empty for a table the deployment has still to create, in which case
    /// [`missing`] falls back to the schema answer — the ledger's shape, for
    /// the ledger's reason.
    pub data_objects: BTreeMap<ObjectName, BTreeSet<String>>,
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Securable {
    Database,
    Schema(String),
    Object(ObjectName),
    /// A securable whose name this account cannot read, carried by the id the
    /// permission row gave.
    ///
    /// `sys.objects` and `sys.schemas` are subject to metadata visibility, and
    /// the very `DENY` this reports is what takes that visibility away:
    /// measured on the pinned image, an object carrying an effective
    /// `DENY VIEW DEFINITION` answers NULL to both `OBJECT_SCHEMA_NAME` and
    /// `OBJECT_NAME`, and a `SELECT` grant on its schema does not bring the
    /// name back. A denied *schema* is still named, so this is in practice the
    /// object case (`introspect::Securable::Unreadable` records the same
    /// finding for `pull`).
    ///
    /// The id rather than an invented name, because [`Gap::securable`] offers
    /// its output as the thing a statement names: a placeholder that looks
    /// pasteable and is not would be worse than none, while an id is one query
    /// away from the name for whoever holds the `DENY` — and absent, empty and
    /// unreadable are three different things.
    Unreadable {
        class: &'static str,
        id: i32,
    },
}

impl std::fmt::Display for Securable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Securable::Database => f.write_str("the database"),
            Securable::Schema(s) => write!(f, "SCHEMA::{}", spelled(s)),
            // Each part quoted on its own and joined with a dot, the spelling
            // `emit::qualified` gives a table: `OBJECT::[dbo].[a.b]` is one
            // object, while `OBJECT::dbo.a.b` is a `b` in some schema `dbo.a`
            // — a different securable, and the one the reader would grant on.
            Securable::Object(o) => {
                write!(f, "OBJECT::{}.{}", spelled(&o.schema), spelled(&o.name))
            }
            // Deliberately not bracket-quoted: this is not a name, and
            // nothing good comes of its looking like one.
            Securable::Unreadable { class, id } => write!(f, "{class}::<unnameable: id {id}>"),
        }
    }
}

/// One part of a securable's name, bracket-quoted the way the emitter spells
/// every identifier.
///
/// The report offers this label as the securable a `GRANT` names, and a reader
/// pastes it. Bare, a name holding a `]`, a space or a hyphen ends the
/// statement early or is a syntax error, and a name holding a `.` names
/// something else entirely.
///
/// `ident::quote` refuses the three names the server itself will not take:
/// empty, holding a NUL, and longer than `MAX_IDENT_CHARS`. No statement can
/// carry those, so the label keeps the name and says it is not one, rather
/// than printing something that looks pasteable and is not — the same choice
/// the `schema.absent` remedy makes by offering no command at all.
fn spelled(part: &str) -> String {
    crate::ident::quote(part).unwrap_or_else(|_| format!("<unquotable: {part}>"))
}

impl Gap {
    /// How the securable is named in a `GRANT`, which is how the report names it.
    pub fn securable(&self) -> String {
        self.securable.to_string()
    }
}

/// The ledger tables as objects, from the names `pbps-db` owns.
pub fn ledger_tables() -> [ObjectName; 2] {
    LEDGER_TABLES.map(|t| {
        t.parse()
            .expect("the ledger table names are this crate's own `schema.table` constants")
    })
}

/// Whether the object-scope question is put to every named object or only
/// to those the catalog shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Existing {
    /// Joined against `OBJECT_ID`: an object that is not there is unasked,
    /// and the caller falls back to the schema answer. Right for the ledger,
    /// which does not exist before the first deployment.
    Only,
    /// Every named object, present or not. Metadata visibility hides an
    /// object from a principal with no permission on it, so `OBJECT_ID`
    /// cannot tell "not there" from "not allowed to see"; asking anyway
    /// lands both as a gap rather than as silence. Right for the foreign-key
    /// targets and the grant targets, where silence would under-report the
    /// one case a readiness check is for.
    OrNot,
}

/// The permissions SQL Server authorizes column by column, and therefore the
/// only ones whose object-scope answer has a column-scope fallback.
///
/// Measured on the pinned image, not read off the enum: `GRANT INSERT ON
/// app.t(label)` is a *syntax* error ("Sub-entity lists … cannot be specified
/// for entity-level permissions"), and `HAS_PERMS_BY_NAME(…, 'INSERT', 'label',
/// 'COLUMN')` answers NULL — as does `DELETE`. A NULL read as "not held" would
/// turn the fallback into a gap on every table, so the list is a fact about the
/// engine and is spelled here rather than being tried and recovered from.
const COLUMN_SCOPED: [&str; 3] = ["SELECT", "UPDATE", "REFERENCES"];

/// Whether the column-scope question may be put for this permission at all.
fn column_scoped(permission: &str) -> bool {
    COLUMN_SCOPED
        .iter()
        .any(|c| c.eq_ignore_ascii_case(permission.trim()))
}

/// Which columns a permission that is not held on the object is looked for on.
///
/// SQL Server takes `GRANT SELECT | UPDATE | REFERENCES` on a *column*, and an
/// account granted exactly the columns it writes holds nothing at object scope:
/// `GRANT UPDATE ON app.t(label)` answers 0 for `HAS_PERMS_BY_NAME(app.t,
/// 'OBJECT', 'UPDATE')` and 1 for the same question naming `label`, and the
/// `UPDATE` really runs (measured on the pinned image). So the object answer
/// alone reports a gap the account does not have, and the remedy it prints
/// widens a deliberately narrow grant.
///
/// The right question is not "on some column" but **on every column the
/// statement could name** — `doctor` never sees a plan, so it cannot know
/// which one an `UPDATE` will set. Held on the object still answers first:
/// an object-level grant answers 1 at column scope too, including for a column
/// added after the grant, so `object OR every-column` is never narrower than
/// what the account has.
///
/// The object answer remains sufficient; otherwise the complete required
/// column set must be known and granted. An empty or unreadable set cannot
/// stand in for permission evidence.
#[derive(Debug, Clone, Copy)]
enum Columns<'a> {
    /// The columns the catalog shows, asked in the server — no parameters, and
    /// right for the ledger tables and securables a role is granted on.
    /// Foreign-key targets instead use their named subset; these callers
    /// retain the whole visible set as their column-level fallback.
    ///
    /// An object with no visible column keeps the object answer: metadata
    /// visibility hides every column from a principal with no permission on
    /// the object, and "no column is ungranted" over an empty list would
    /// read that silence as ready.
    Catalog,
    /// The **declared** columns, per object, for the tables whose rows this
    /// tool writes (ADR-0004).
    ///
    /// Not the catalog's, and that difference is the point twice over. The
    /// declared list is the columns an `UPDATE` could **set** — it leaves out
    /// the key column and the engine's own `IDENTITY`s
    /// ([`Table::row_columns`]), which no emitted `UPDATE` ever names; the
    /// catalog's list holds them, and demanding `UPDATE` on a primary key
    /// would refuse every column-level grant a careful DBA would actually
    /// write. And a column the next
    /// plan adds is not in the catalog yet, so a catalog-sourced list would
    /// answer "every existing column is granted" and call the account ready
    /// for an `UPDATE` that will name a column it holds nothing on. Asked as
    /// declared, that column answers 0 — and only an object-level grant
    /// rescues it, which is exactly the grant that does cover a column added
    /// later. Both halves measured on the pinned image: after `ALTER TABLE app.t
    /// ADD extra`, a column-only grantee answers 0 on `extra` and its `UPDATE`
    /// is denied; an object-level grantee answers 1 and its `UPDATE` runs.
    Declared(&'a BTreeMap<ObjectName, Vec<String>>),
    /// Only the union of columns named by external foreign keys. This is not
    /// a complete declaration of the target; unrelated columns need no grant.
    Referenced(&'a ReferencedColumns),
}

impl<'a> Columns<'a> {
    /// How many parameters one object costs beyond its own two slots.
    fn parameters_for(self, object: &ObjectName) -> usize {
        match self {
            // The column list is `sys.columns`, joined in the server.
            Self::Catalog => 0,
            // One per column: the name. What ties the column to its object is
            // the object's **position in this statement**, written as a
            // literal on both sides, so the two parts of the name are bound
            // once however wide the table is. Bound per column instead, at
            // three slots each, a table of 698 columns alone crossed
            // `MAX_PARAMETERS` — a table SQL Server takes without complaint,
            // and `doctor` would have failed the whole permission read on it.
            Self::Declared(declared) => declared.get(object).map_or(0, Vec::len),
            Self::Referenced(referenced) => referenced.get(object).map_or(0, BTreeSet::len),
        }
    }

    fn named(self, object: &ObjectName) -> Vec<&'a str> {
        match self {
            Self::Declared(declared) => declared
                .get(object)
                .into_iter()
                .flatten()
                .map(String::as_str)
                .collect(),
            Self::Referenced(referenced) => referenced
                .get(object)
                .into_iter()
                .flatten()
                .map(String::as_str)
                .collect(),
            Self::Catalog => Vec::new(),
        }
    }
}

/// The object-scope question, spelled once for the three lists that ask it.
///
/// Each object is bound as its two parts and the securable is assembled by
/// the server, `QUOTENAME` on each part, the way the schema queries spell
/// theirs. Joined on the client with a dot and passed as one string, the
/// name was read back through the engine's own name parser: `dbo.a.b` split
/// at the wrong dot and answered 0, and `dbo.x]y` did not parse and answered
/// NULL — both read as a gap on a permission the account held (measured on
/// the pinned image, as `sa`). The **requested** parts come back as the key,
/// not the catalog's spelling, for the reason the schema query gives.
///
/// The answer is the object-level one **or** the column-level one over every
/// column [`Columns`] names, and the ordering of the `CASE` is what makes the
/// cost bearable: the column subqueries are reached only for a permission the
/// engine takes at column scope and only when the object answer was not 1.
fn object_permissions_sql<'a>(
    objects: &'a [ObjectName],
    perms: &[&'a str],
    existing: Existing,
    columns: Columns<'a>,
) -> (String, Vec<Param<'a>>) {
    let mut params: Vec<Param<'a>> = Vec::new();
    let mut perm_slots = Vec::new();
    for p in perms {
        params.push(Param::from(*p));
        // The flag is a literal, not a parameter: it is this crate's reading
        // of `COLUMN_SCOPED`, not a value from the caller, and it costs a
        // parameter slot the wide lists cannot spare.
        perm_slots.push(format!(
            "(@P{}, {})",
            params.len(),
            i32::from(column_scoped(p))
        ));
    }
    let mut object_slots = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        params.push(Param::from(o.schema.as_str()));
        params.push(Param::from(o.name.as_str()));
        object_slots.push(format!("(@P{}, @P{}, {i})", params.len() - 1, params.len()));
    }
    let (any_column, ungranted_column) = match columns {
        Columns::Catalog => (
            "EXISTS (SELECT 1 FROM sys.columns AS c WHERE c.object_id = OBJECT_ID(x.q))".to_owned(),
            "EXISTS (SELECT 1 FROM sys.columns AS c WHERE c.object_id = OBJECT_ID(x.q) \
             AND HAS_PERMS_BY_NAME(x.q, 'OBJECT', p.n, c.name, 'COLUMN') = 0)"
                .to_owned(),
        ),
        Columns::Declared(_) | Columns::Referenced(_) => {
            let mut column_slots = Vec::new();
            for (i, o) in objects.iter().enumerate() {
                for column in columns.named(o) {
                    params.push(Param::from(column));
                    // The object is named by its position in this statement,
                    // a literal on both sides. Binding its two parts again
                    // per column is what a table of a few hundred columns
                    // cannot afford; a position costs nothing and cannot be
                    // spelled wrongly.
                    column_slots.push(format!("({i}, @P{})", params.len()));
                }
            }
            // `VALUES ()` is not T-SQL, and a chunk whose objects declare no
            // required column has nothing to fall back to: the object answer
            // stands, which is what an always-false predicate leaves.
            if column_slots.is_empty() {
                ("1 = 0".to_owned(), "1 = 0".to_owned())
            } else {
                // The same `@P` names in both predicates. A parameter may be
                // read as often as the statement likes, so spelling the list
                // twice costs no second binding — which is what keeps a wide
                // table at one slot per column rather than two.
                let list = format!("(VALUES {}) AS c(i, col)", column_slots.join(", "));
                let ungranted = if matches!(columns, Columns::Referenced(_)) {
                    // A named external column can be absent or invisible.
                    // NULL is not evidence that its permission is held.
                    "COALESCE(HAS_PERMS_BY_NAME(x.q, 'OBJECT', p.n, c.col, 'COLUMN'), 0) <> 1"
                } else {
                    "HAS_PERMS_BY_NAME(x.q, 'OBJECT', p.n, c.col, 'COLUMN') = 0"
                };
                (
                    format!("EXISTS (SELECT 1 FROM {list} WHERE c.i = o.i)"),
                    format!(
                        "EXISTS (SELECT 1 FROM {list} WHERE c.i = o.i \
                         AND {ungranted})"
                    ),
                )
            }
        }
    };
    let filter = match existing {
        Existing::Only => " WHERE OBJECT_ID(x.q, N'U') IS NOT NULL",
        Existing::OrNot => "",
    };
    let sql = format!(
        "SELECT o.s AS [schema], o.n AS [object], p.n AS permission, \
         CASE WHEN HAS_PERMS_BY_NAME(x.q, 'OBJECT', p.n) = 1 THEN 1 \
              WHEN p.col = 0 THEN 0 \
              WHEN {ungranted_column} THEN 0 \
              WHEN {any_column} THEN 1 \
              ELSE 0 END AS held \
         FROM (VALUES {}) AS o(s, n, i) \
         CROSS APPLY (VALUES (QUOTENAME(o.s) + N'.' + QUOTENAME(o.n))) AS x(q) \
         CROSS JOIN (VALUES {}) AS p(n, col){filter};",
        object_slots.join(", "),
        perm_slots.join(", ")
    );
    (sql, params)
}

/// The most parameters one bound statement may carry.
///
/// SQL Server refuses an RPC with more than 2,100 parameters, and a bound
/// statement travels as `sp_executesql`, which spends two of those on its
/// own `@stmt` and `@params`: 2,098 user parameters are accepted and 2,099
/// are refused, measured on the pinned image (the live test
/// `a_query_may_bind_two_fewer_parameters_than_the_server_names`). Named
/// here rather than read off the driver, which documents the server's
/// number, because the limit is the server's and the driver is not this
/// crate's to name (constraint 9).
///
/// `pub(crate)`, not private: `crate::state`'s legacy-row fallback query
/// binds one parameter per id with the same `sp_executesql` overhead and
/// shares this ceiling rather than re-deriving it (a round-1 review finding
/// on #103's own PR — the two callers must not drift onto two different
/// numbers for the one thing the server actually enforces).
pub(crate) const MAX_PARAMETERS: usize = 2098;

/// The schema list cut into pieces each of which fits one statement beside
/// `perms` permission names: every schema costs one slot, and every piece
/// carries the whole permission list again.
///
/// Never an empty piece, and never an empty list of pieces for a non-empty
/// list: a schema dropped from the question reads as a schema with nothing
/// missing.
fn schema_statements<'a, 'b>(schemas: &'a [&'b str], perms: usize) -> Vec<&'a [&'b str]> {
    let per = MAX_PARAMETERS.saturating_sub(perms).max(1);
    schemas.chunks(per).collect()
}

/// One schema-scope permission statement for `schemas`.
///
/// Both lists are bound, not pasted. The permission names are this crate's own
/// constants and the schema names come from declarations or state, but SQL
/// built by concatenation is the habit this codebase does not have.
///
/// The **requested** spelling comes back, not the catalog's. Matching is the
/// server's job — `w.n = s.name` compares under the database's collation, so a
/// case-insensitive database matches `App` to its `app` — but the caller then
/// looks the answer up by the name it asked with. Selecting `s.name` returned
/// `app` for a request of `App`, so the Rust-side lookup missed, and `doctor`
/// reported the schema absent and advised creating one that already exists.
///
/// `QUOTENAME(s.name)` stays the catalog's spelling: that argument names a real
/// securable, not a map key.
fn schema_permissions_sql<'a>(schemas: &[&'a str], perms: &[&'a str]) -> (String, Vec<Param<'a>>) {
    let mut params: Vec<Param<'a>> = Vec::new();
    let mut perm_slots = Vec::new();
    for p in perms {
        params.push(Param::from(*p));
        perm_slots.push(format!("(@P{})", params.len()));
    }
    let mut schema_slots = Vec::new();
    for s in schemas {
        params.push(Param::from(*s));
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
    (sql, params)
}

/// The object list cut into pieces each of which fits one statement.
///
/// Every object list is asked in pieces: a foreign-key list or a role's grants
/// past about a thousand objects made one statement that the server refused,
/// and `doctor` reported an estate it could have checked as unreadable.
///
/// Packed rather than divided, because an object no longer costs a fixed two
/// slots — a named column set costs one more per required
/// column, so one wide table can be worth a hundred narrow ones and a fixed
/// chunk size would be sized for either the widest table or none of them.
///
/// Ordinary table widths fit on their own, but a supplied column set can be
/// wider than a real table. An oversized object is asked alone: the server
/// refuses that statement loudly rather than losing the object's demand,
/// which would read as ready.
fn object_statements<'a>(
    objects: &'a [ObjectName],
    perms: usize,
    columns: Columns<'_>,
) -> Vec<&'a [ObjectName]> {
    let budget = MAX_PARAMETERS.saturating_sub(perms);
    let mut out = Vec::new();
    let mut start = 0;
    let mut spent = 0;
    for (i, object) in objects.iter().enumerate() {
        let cost = 2 + columns.parameters_for(object);
        // Never an empty piece: an object whose own cost exceeds the budget
        // is asked alone and fails loudly on the server, rather than looping
        // forever here or being dropped — a dropped object reads as ready.
        if spent + cost > budget && i > start {
            out.push(&objects[start..i]);
            start = i;
            spent = 0;
        }
        spent += cost;
    }
    if start < objects.len() {
        out.push(&objects[start..]);
    }
    out
}

/// The tables a `mode: exact` delete count would read, found in the catalog.
///
/// # Why the catalog and not the declarations
///
/// `preflight::delete_probe` builds its count from `sys.foreign_keys` at run
/// time and deliberately does not trust the declarations to list the children:
/// "a foreign key someone added by hand is exactly the one that will refuse the
/// delete". So a child does not have to be declared, or even live in a schema
/// this project manages, and the readiness question has to find them the same
/// way the probe does. Asked only of the parents whose declaration can remove a
/// row; a project that removes none runs no count and this query does not run
/// (issue #515).
///
/// Returns the columns each child's keys name as well as the child, because
/// that is the width of the demand: the count compares the child's foreign-key
/// tuple against the parent row and reads nothing else of it. Unioned across
/// every enabled key from that child into any of these parents, which is what
/// `sys.foreign_key_columns` gives the probe at run time.
///
/// `is_disabled = 0` matches the probe exactly. `NOCHECK CONSTRAINT` leaves the
/// constraint in the catalog and stops the engine enforcing it, the probe skips
/// those children (DECISIONS 144), and demanding a read of one would be asking
/// for a permission no statement uses.
///
/// # What it cannot see
///
/// A child whose metadata this login cannot see produces no row here, and the
/// report is silent about it. That is the boundary of a read-only readiness
/// check rather than a gap it could report: the account cannot be told to grant
/// itself something on a table the catalog will not name for it, and the
/// count's own `VIEW DEFINITION` demands ([`Needed::DeleteCatalog`]) are what
/// report the visibility half.
/// The discovery statement for one chunk of parents.
///
/// Each parent is bound as its two parts and the securable assembled by the
/// server, for the reason [`object_permissions_sql`] gives: joined here with a
/// dot, a name holding one of its own resolves to the wrong object or to none.
fn delete_children_sql(parents: &[ObjectName]) -> (String, Vec<Param<'_>>) {
    let mut params: Vec<Param<'_>> = Vec::new();
    let mut slots = Vec::new();
    for o in parents {
        params.push(Param::from(o.schema.as_str()));
        params.push(Param::from(o.name.as_str()));
        slots.push(format!("(@P{}, @P{})", params.len() - 1, params.len()));
    }
    let sql = format!(
        "SELECT DISTINCT cs.name AS [schema], ct.name AS [object], cc.name AS [column] \
         FROM (VALUES {}) AS p(s, n) \
         CROSS APPLY (VALUES (QUOTENAME(p.s) + N'.' + QUOTENAME(p.n))) AS x(q) \
         JOIN sys.foreign_keys fk ON fk.referenced_object_id = OBJECT_ID(x.q, N'U') \
         JOIN sys.tables ct ON ct.object_id = fk.parent_object_id \
         JOIN sys.schemas cs ON cs.schema_id = ct.schema_id \
         JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
         JOIN sys.columns cc ON cc.object_id = fkc.parent_object_id \
                            AND cc.column_id = fkc.parent_column_id \
         WHERE fk.is_disabled = 0;",
        slots.join(", ")
    );
    (sql, params)
}

async fn delete_children(
    conn: &mut Conn,
    parents: &[ObjectName],
) -> Result<ReferencedColumns, DbError> {
    let mut out = ReferencedColumns::new();
    // Two slots per parent, and the same reason `object_statements` exists: a
    // list past about a thousand objects is a statement the server refuses.
    for chunk in parents.chunks(MAX_PARAMETERS / 2) {
        let (sql, params) = delete_children_sql(chunk);
        for row in &conn.query_with(&sql, &params).await? {
            let schema: &str = get(row, "schema")?;
            let object: &str = get(row, "object")?;
            let column: &str = get(row, "column")?;
            out.entry(ObjectName::new(schema, object))
                .or_default()
                .insert(column.to_owned());
        }
    }
    Ok(out)
}

async fn object_permissions(
    conn: &mut Conn,
    objects: &[ObjectName],
    perms: &[&str],
    existing: Existing,
    columns: Columns<'_>,
) -> Result<BTreeMap<ObjectName, BTreeSet<String>>, DbError> {
    let mut out: BTreeMap<ObjectName, BTreeSet<String>> = BTreeMap::new();
    // `VALUES ()` is not T-SQL: nothing to ask when nothing is named — and
    // an empty list is cut into no pieces at all, so the loop agrees.
    for chunk in object_statements(objects, perms.len(), columns) {
        let (sql, params) = object_permissions_sql(chunk, perms, existing, columns);
        for row in &conn.query_with(&sql, &params).await? {
            let schema: &str = get(row, "schema")?;
            let object: &str = get(row, "object")?;
            let permission: &str = get(row, "permission")?;
            // A NULL means the securable did not parse, which `QUOTENAME` on
            // each part rules out for a name the catalog can hold. Read as
            // "not held" rather than skipped, for the reason the schema query
            // gives.
            let held: i32 = row.try_get("held")?.unwrap_or(0);
            let entry = out.entry(ObjectName::new(schema, object)).or_default();
            if held != 0 {
                entry.insert(permission.trim().to_ascii_uppercase());
            }
        }
    }
    Ok(out)
}

/// Resolves each of `wanted`'s declared names to the physical name this
/// environment currently has it under, refusing a resolution that would
/// misattribute a *different* identity's object.
///
/// # Why a resolved name can be unsafe to ask under
///
/// `IdsFile::resolved_in` answers per declared name in isolation: given one
/// name, what does its uid currently go by here. Asked across a whole batch
/// at once, two different declared names can answer with the *same* physical
/// name — a rename frees the name the departing identity carried, and the
/// same plan can declare a new identity under that freed name before the
/// rename has actually run against this environment. Asking under the shared
/// name and keying the answer by it would collapse the two declarations'
/// distinct demands into one map entry (issue #133 round 2); keying instead
/// by the declared name and asking under the resolved one just moves the
/// danger from "one demand disappears" to "one demand reads the other
/// identity's permissions", which is worse because nothing about the answer
/// looks wrong.
///
/// A resolution is trusted only when it is **confirmed**: the environment's
/// own recorded ids name that uid under that name right now. An unresolved
/// fallback — no uid, or a uid this environment has not recorded — asserts
/// nothing the environment itself has said; it is just the declared name,
/// unchanged. When a fallback's candidate name is one the recorded ids
/// confirm belongs to some other uid, the fallback loses: that physical
/// object is already, confirmedly, someone else's. Two declared names cannot
/// both hold a confirmed claim on the same name — the environment's own
/// recorded ids name each uid once — so this rule never has to choose between
/// two confirmed claims.
///
/// # Why the claim comparison is the engine's
///
/// A valid plan can rename `app.Old` to `app.new` and declare a new
/// `app.old` in the same revision: three different Rust strings, `Old`
/// among them, so an exact-`Eq` claim check never sees the second door this
/// collision reaches through. Measured on the pinned image (issue #133
/// round 3): its default collation is `SQL_Latin1_General_CP1_CI_AS`, and
/// `CI` means SQL Server itself reads `app.Old` and `app.old` as one
/// securable. Asking `HAS_PERMS_BY_NAME`/`OBJECT_ID` under either spelling
/// then answers about the departing identity's object, and the arriving
/// one silently inherits its permissions answer: the exact misattribution
/// this function exists to refuse, reached past a claim check that never
/// fires.
///
/// Which names are one securable is the database collation's answer, not a
/// fold this code can reproduce: `to_lowercase` covered case and nothing
/// else, so an accent-insensitive (`_AI`), width-insensitive or
/// kana-insensitive database still read `app.café` and `app.cafe` as one
/// object where the fold kept them apart (#384). So the candidates are
/// asked of the engine instead ([`claimed_elsewhere`], through
/// `catalog::matching_table_names`, DECISIONS 119, 142), and this function
/// takes the engine's answer as `colliding`. On a case-sensitive database
/// that answer keeps `app.Old` and `app.old` apart, as the server does.
fn resolve_for_query<'a>(
    wanted: impl Iterator<Item = &'a ObjectName>,
    project_ids: &pbps_model::IdsFile,
    recorded_ids: &pbps_model::IdsFile,
    colliding: &BTreeSet<ObjectName>,
) -> (BTreeMap<ObjectName, ObjectName>, BTreeSet<ObjectName>) {
    let mut safe: BTreeMap<ObjectName, ObjectName> = BTreeMap::new();
    let mut unresolvable: BTreeSet<ObjectName> = BTreeSet::new();
    for declared in wanted {
        let (query, confirmed) = resolution(declared, project_ids, recorded_ids);
        if confirmed || !colliding.contains(&query) {
            safe.insert(declared.clone(), query);
        } else {
            unresolvable.insert(declared.clone());
        }
    }
    (safe, unresolvable)
}

/// Each of `columns` of the declared table `declared`, with the name it has
/// in this environment and whether the recorded ids gave it that name (#676).
///
/// A column a plan is about to rename is still under its recorded name here,
/// and `sp_rename` keeps its column grants with it (measured on the pinned
/// image, 17.0.4075.5), so the question is put under that name. A column with
/// no recorded uid is one this deployment adds: it keeps its declared name,
/// which the catalog does not hold yet and answers 0 for, so it still demands
/// a covering grant (`Needed::DataRead`, DECISIONS 510).
fn resolve_columns(
    declared: &ObjectName,
    columns: &[String],
    project_ids: &pbps_model::IdsFile,
    recorded_ids: &pbps_model::IdsFile,
) -> Vec<(String, bool)> {
    columns
        .iter()
        .map(|column| {
            let wanted = pbps_model::ColumnRef {
                table: declared.clone(),
                name: column.clone(),
            };
            project_ids
                .column_uid(&wanted)
                .and_then(|uid| recorded_ids.columns.get(uid))
                .map_or_else(|| (column.clone(), false), |r| (r.name.clone(), true))
        })
        .collect()
}

/// The column lists for the data tables, each column under the name this
/// environment has for it (#676), keyed like `data_securable`'s values.
///
/// A rename can free a name the same plan reuses: `label` renamed to `caption`
/// beside a new `label`. The new column would then be asked about under the
/// renamed column's physical name and inherit its answer. The same holds for
/// any column the environment records on the table, projected by the data
/// demand or not. The list is
/// positional in the generated statement, so that collision is settled before
/// it is built: such a table gets no column list, and only an object-level
/// grant counts, the one grant that can cover a column that does not exist
/// yet. Which names are one column is the database collation's answer, asked
/// of the engine (`catalog::tables_reusing_a_column_name`), as the table path
/// asks it (#384).
async fn resolved_column_lists(
    conn: &mut Conn,
    data_securable: &BTreeMap<ObjectName, ObjectName>,
    data: &BTreeMap<ObjectName, pbps_db::doctor::DataDemand>,
    project_ids: &pbps_model::IdsFile,
    recorded_ids: &pbps_model::IdsFile,
) -> Result<
    (
        BTreeMap<ObjectName, Vec<String>>,
        BTreeMap<ObjectName, Vec<String>>,
    ),
    DbError,
> {
    let mut rows = Vec::new();
    let mut added = Vec::new();
    let mut recorded = Vec::new();
    for (i, (declared, query)) in data_securable.iter().enumerate() {
        let Some(demand) = data.get(declared) else {
            continue;
        };
        let row = resolve_columns(declared, demand.row_columns(), project_ids, recorded_ids);
        let read = resolve_columns(declared, demand.data_columns(), project_ids, recorded_ids);
        for (name, known) in row.iter().chain(&read) {
            if !*known {
                added.push((i, name.clone()));
            }
        }
        // Every column the environment records on this table, not only the
        // ones the data demand projects: a non-key identity column or one the
        // plan drops still holds its name, and its grants, until the plan
        // runs.
        recorded.extend(
            recorded_ids
                .columns
                .values()
                .filter(|c| &c.table == query)
                .map(|c| (i, c.name.clone())),
        );
        rows.push((i, query.clone(), row, read));
    }
    let reused = crate::catalog::tables_reusing_a_column_name(conn, &added, &recorded).await?;
    let names = |list: Vec<(String, bool)>, i: usize| -> Vec<String> {
        if reused.contains(&i) {
            Vec::new()
        } else {
            list.into_iter().map(|(name, _)| name).collect()
        }
    };
    let mut row_lists = BTreeMap::new();
    let mut read_lists = BTreeMap::new();
    for (i, query, row, read) in rows {
        row_lists.insert(query.clone(), names(row, i));
        read_lists.insert(query, names(read, i));
    }
    Ok((row_lists, read_lists))
}

/// Where the project now declares a recorded table, when that is in another
/// schema: an identity-preserving move rather than a drop (#352).
///
/// Such a table does not keep its source schema managed. The move is `ALTER
/// SCHEMA dest TRANSFER`, which needs `CONTROL` on the table and `ALTER` on
/// the destination, and nothing on the source schema. Demanding the managed
/// schema set there asked for access the plan never uses. SPEC §9.5 leaves the
/// transfer's own `CONTROL` demand unmodelled, and this does not change that.
///
/// A recorded table the project no longer declares at all is a drop, and it
/// keeps its schema managed until the drop is recorded, which is the
/// recorded-management rule. So does a table whose uid the recorded ids do not
/// name: without the identity there is no move to recognise, and the answer
/// errs towards asking.
fn moved_to<'a>(
    recorded: &ObjectName,
    project_ids: &'a pbps_model::IdsFile,
    recorded_ids: &pbps_model::IdsFile,
) -> Option<&'a ObjectName> {
    let uid = recorded_ids.table_uid(recorded)?;
    project_ids
        .tables
        .get(uid)
        .filter(|declared| declared.schema != recorded.schema)
}

/// The name `declared` has in this environment, and whether the environment's
/// own recorded ids confirm it.
fn resolution(
    declared: &ObjectName,
    project_ids: &pbps_model::IdsFile,
    recorded_ids: &pbps_model::IdsFile,
) -> (ObjectName, bool) {
    let query = project_ids.resolved_in(declared, recorded_ids);
    let confirmed = project_ids
        .table_uid(declared)
        .and_then(|uid| recorded_ids.tables.get(uid))
        == Some(&query);
    (query, confirmed)
}

/// Of the unconfirmed resolutions of `wanted`, the ones the database's
/// collation reads as a name the recorded ids already give an identity: the
/// `colliding` answer [`resolve_for_query`] takes (#384). One round trip,
/// and none when nothing is unconfirmed or nothing is recorded.
async fn claimed_elsewhere<'a>(
    conn: &mut Conn,
    wanted: impl Iterator<Item = &'a ObjectName>,
    project_ids: &pbps_model::IdsFile,
    recorded_ids: &pbps_model::IdsFile,
) -> Result<BTreeSet<ObjectName>, DbError> {
    let candidates: Vec<ObjectName> = wanted
        .map(|declared| resolution(declared, project_ids, recorded_ids))
        .filter(|(_, confirmed)| !confirmed)
        .map(|(query, _)| query)
        .collect();
    let claimed: Vec<ObjectName> = recorded_ids.tables.values().cloned().collect();
    Ok(
        crate::catalog::matching_table_names(conn, &candidates, &claimed)
            .await?
            .into_iter()
            .collect(),
    )
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
///
/// # Why declared managed, data and grant targets are resolved before asking
///
/// `managed_tables`, `data` and `granted.objects` name objects as the declarations
/// do. Until a pending rename reaches *this* environment, the object there
/// still answers to its old name, and `sp_rename` keeps a `GRANT` or a `DENY`
/// with the object rather than with the name (measured on the pinned image,
/// issue #133). Asking `HAS_PERMS_BY_NAME` under the declared name finds
/// nothing there, so the object question silently falls back to the schema —
/// a careful DBA's object-level `GRANT` reads as a gap, and an object-level
/// `DENY` that really blocks the deployment does not.
///
/// `project_ids` is the project's own identity mapping (declared name ->
/// uid); `recorded_ids`, read below alongside `recorded` for the same reason
/// the role question already reads it, is this **environment's** own
/// mapping (uid -> the name it currently has). Resolving through both, per
/// object, turns "the name the declarations call it" into "the name this
/// database calls it right now" — which is what a `GRANT` issued today has
/// to name (DECISIONS 439).
///
/// `referenced` is deliberately left unresolved: those tables lie outside the
/// managed schemas, and pbps never renames an object it does not manage.
///
/// # Why the resolution can refuse to answer
///
/// Resolving each declared name in isolation is not enough once more than one
/// is asked about together: a rename frees the name the departing identity
/// carried, and the same plan can declare a *new* table under that freed name
/// before the rename has actually run against this environment. Two declared
/// names then resolve to the same physical name, and naively asking under it
/// and keying the answer by it collapsed the two declarations' distinct
/// demands into one map entry (issue #133 round 2). `resolve_for_query`
/// trusts a resolution only when the environment's own recorded ids confirm
/// it; an unresolved fallback that a confirmed resolution has already claimed
/// is refused rather than asked about, because asking would read the other
/// identity's permissions instead. `Held::data_tables` is keyed by the
/// declared name for the same reason and survives the refusal by falling back
/// to the schema answer; `Held::granted_objects` has no such fallback, so a
/// refused grant target is reported through `Held::granted_unresolvable` as
/// an unconditional gap instead.
//
// The parameters are the fields of `Ask` this engine's permission model
// actually answers, spelled out rather than taken as the whole ask — the
// PostgreSQL side takes `Ask` because it reads all of it. Each is a distinct
// type, so the count is not somewhere a miscounted call can hide: swapping any
// two of them fails to compile.
#[allow(clippy::too_many_arguments)]
pub async fn permissions(
    conn: &mut Conn,
    managed_tables: &[ObjectName],
    schemas: &[String],
    referenced: &ReferencedColumns,
    granted: &GrantTargets,
    data: &DataTables,
    declared_keys: &DeclaredKeys,
    project_ids: &pbps_model::IdsFile,
) -> Result<Held, DbError> {
    let rows = conn
        .query("SELECT permission_name AS name FROM sys.fn_my_permissions(NULL, 'DATABASE');")
        .await?;
    let mut database = BTreeSet::new();
    for row in &rows {
        let name: &str = get(row, "name")?;
        database.insert(name.trim().to_ascii_uppercase());
    }

    // Recorded tables and modules remain managed until their drop is
    // applied, even when the declarations no longer name their schema
    // (DEC-356.1). Tombstones are permanent history and must not keep these
    // requirements switched on.
    //
    // `recorded_ids` comes from the same read: an unreadable ledger must not
    // be papered over by inventing a resolution from the declarations, so a
    // failed or empty read leaves it as `IdsFile::default()` — which has no
    // uid for anything, so every resolution below falls through to the name
    // it was asked with, exactly like an environment that never had the
    // object. The permission gap this read's own failure causes is reported
    // by the ledger permission rows, not by this fallback.
    let (recorded, recorded_ids) = match crate::state::latest(conn).await {
        Ok(Some(entry)) => (entry.snapshot.schema, entry.snapshot.ids),
        _ => (
            pbps_model::Schema::default(),
            pbps_model::IdsFile::default(),
        ),
    };
    let mut managed: BTreeSet<&str> = schemas.iter().map(String::as_str).collect();
    managed.extend(
        recorded
            .tables
            .keys()
            .filter(|table| moved_to(table, project_ids, &recorded_ids).is_none())
            .map(|table| table.schema.as_str()),
    );
    // Recorded modules too (#355, DEC-356.1): a view, procedure or function
    // removed from the declarations is a pending `DROP` in its schema just as
    // a table is, and the drop needs `ALTER` there. A module has no uid to
    // move under, so every recorded one keeps its schema managed until its
    // drop is recorded.
    managed.extend(recorded.modules.keys().map(pbps_model::ModuleId::schema));

    // The ledger's schema is queried alongside the managed ones because it is
    // the fallback for the ledger requirements before those tables exist — but
    // its answer is kept in its own field, not folded into the managed set.
    let mut wanted = managed.clone();
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
                Needed::Managed
                    | Needed::ManagedTable
                    | Needed::Ledger
                    | Needed::LedgerCreation
                    | Needed::DataInsert
                    | Needed::DataUpdate
                    | Needed::DataDelete
                    | Needed::DataRead
            )
        })
        .map(|r| r.name)
        .collect();
    let ledger_perms: Vec<&str> = REQUIRED
        .iter()
        .filter(|r| matches!(r.needed, Needed::Ledger | Needed::LedgerMigration))
        .map(|r| r.name)
        .collect();

    // In pieces, as the object reads below are (#351): one statement for every
    // declared and recorded schema passed the server's parameter ceiling on a
    // large estate, and the whole schema answer came back as unreadable.
    let mut per_schema: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for chunk in schema_statements(&wanted, schema_perms.len()) {
        let (sql, params) = schema_permissions_sql(chunk, &schema_perms);
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
    }

    // The ledger and the lock at object scope. A careful DBA grants INSERT and
    // DELETE on exactly these two tables and nowhere else, and only this
    // question can see that grant. Only where they exist: before the first
    // deployment there is nothing to ask about, and `missing` then falls back
    // to the schema answer above.
    // `Columns::Catalog`: the ledger's shape is this tool's own, and a
    // `SELECT` it cannot run on one of those columns is a gap however narrowly
    // the grant was written.
    let ledger_objects = object_permissions(
        conn,
        &ledger_tables(),
        &ledger_perms,
        Existing::Only,
        Columns::Catalog,
    )
    .await?;
    // Only a visible existing table can need this migration. Absent or hidden
    // ledger objects retain their existing creation/DML readiness reports.
    let ledger_migration_needed = ledger_objects.contains_key(&ledger_tables()[0])
        && !crate::state::timeline_columns_present(conn).await?;

    let colliding =
        claimed_elsewhere(conn, managed_tables.iter(), project_ids, &recorded_ids).await?;
    let (managed_securable, _) = resolve_for_query(
        managed_tables.iter(),
        project_ids,
        &recorded_ids,
        &colliding,
    );
    // Recorded names already belong to this environment. Resolve only the
    // declarations: a departing table and a new declaration reusing its name
    // must retain both the existing-object and future-schema questions.
    let managed_names: BTreeSet<ObjectName> = managed_securable
        .values()
        .cloned()
        .chain(recorded.tables.keys().cloned())
        .collect();
    let managed_perms: Vec<&str> = REQUIRED
        .iter()
        .filter(|r| matches!(r.needed, Needed::ManagedTable))
        .map(|r| r.name)
        .collect();
    let managed_objects = object_permissions(
        conn,
        &managed_names.iter().cloned().collect::<Vec<_>>(),
        &managed_perms,
        Existing::Only,
        Columns::Catalog,
    )
    .await?;
    let managed_tables = managed_table_rights(
        managed_tables,
        &managed_securable,
        recorded.tables.keys(),
        &managed_objects,
        &per_schema,
    );

    // The declared data tables at object scope, `Existing::Only` like the
    // ledger and for the ledger's reason: a table this deployment has still to
    // create has no object to ask about, and `HAS_PERMS_BY_NAME` on a name the
    // catalog does not hold answers 0 — so asking anyway would report a gap on
    // every first deployment of a project that seeds rows. `missing` falls back
    // to the schema, which is the only place a grant can sit that early.
    let data_perms: Vec<&str> = REQUIRED
        .iter()
        .filter(|r| {
            matches!(
                r.needed,
                Needed::DataInsert | Needed::DataUpdate | Needed::DataDelete
            )
        })
        .map(|r| r.name)
        .collect();
    // Resolved to the name this environment currently has, per table — see
    // the module-level note above and `resolve_for_query`'s. A table a
    // rename has not reached here yet is asked about under the name it still
    // carries; one this deployment has still to create has no uid recorded
    // anywhere and keeps its declared name, which is the
    // object-does-not-exist-yet path this whole question already falls back
    // from. A table `resolve_for_query` could not safely resolve is simply
    // absent from `data_securable` — no object is asked about, and
    // `data_gaps` falls back to the schema answer, exactly like a table this
    // deployment has still to create.
    let colliding = claimed_elsewhere(conn, data.keys(), project_ids, &recorded_ids).await?;
    let (data_securable, _) =
        resolve_for_query(data.keys(), project_ids, &recorded_ids, &colliding);
    let data_names: Vec<ObjectName> = data_securable.values().cloned().collect();
    // The declared row columns, not the catalog's: see `Columns::Declared`.
    // `UPDATE` is the only one of the three the engine takes at column scope,
    // so it is the only one this list can change the answer for. Keyed by the
    // same resolved name as `data_names`: `Columns::Declared` matches a
    // column to its object by that list's position. `resolve_for_query`
    // guarantees the values in `data_securable` are pairwise distinct, so
    // each declared table's columns land under their own key here rather
    // than overwriting another's.
    // And each column under the name this environment has for it (#676).
    let (data_columns, read_columns) =
        resolved_column_lists(conn, &data_securable, data, project_ids, &recorded_ids).await?;
    let mut data_objects = object_permissions(
        conn,
        &data_names,
        &data_perms,
        Existing::Only,
        Columns::Declared(&data_columns),
    )
    .await?;
    // The closing read's own columns, asked separately because they are a
    // different list: `SELECT` covers the key, which no emitted `UPDATE` ever
    // names and which a careful DBA's column grants would not carry
    // (`Needed::DataRead`, `Needed::DataUpdate`). One column list per object
    // is all the statement can bind, so the two demands are two statements
    // and their answers are merged — `Existing::Only` on the same names, so
    // an object present for one is present for the other, and a table this
    // deployment has still to create is absent from both and falls back to
    // the schema.
    let read_perms: Vec<&str> = REQUIRED
        .iter()
        .filter(|r| matches!(r.needed, Needed::DataRead))
        .map(|r| r.name)
        .collect();
    for (object, granted) in object_permissions(
        conn,
        &data_names,
        &read_perms,
        Existing::Only,
        Columns::Declared(&read_columns),
    )
    .await?
    {
        data_objects.entry(object).or_default().extend(granted);
    }

    // The children an `exact` declaration's delete count would read, found in
    // the catalog rather than in the declarations (`delete_children`). Asked
    // only when a row could be removed at all: the discovery query costs a
    // round trip, and a project that never deletes never runs the count.
    // Deduplicated against the managed names, which already demand `SELECT` on
    // the same securable — a declared child needs no second entry, and a
    // self-referencing key would otherwise report the parent as its own child.
    // That dedupe is also what makes the narrow column list right: the
    // fragments naming a child's own key are written only for a child the plan
    // moves, and such a child is declared, so everything left here is read
    // through its foreign-key tuple and nothing else.
    //
    // **With one exception, and it is the managed child that moves.** The
    // guard a row delete carries (`preflight::still_referenced`) discovers the
    // surviving keys and reads the child *inside the delete's transaction*,
    // and a table rename is `order_key` 1 while a row delete is 12 — so the
    // transfer has already run, and with it every permission on that object.
    // The managed question answers for the source, and this child has no
    // `data:` block for `data_gaps` to answer for its destination, so nothing
    // else covers the read. Its destination schema is demanded here instead.
    //
    // **And the exception has an exception**, which is the key that does not
    // survive the plan. The guard discovers its children from the catalog
    // *inside* the delete's transaction, by which time a `DropForeignKey` has
    // already run (`order_key` 2 against 12), so the catalog no longer names
    // that child and nothing reads it. `doctor` never looks at a plan, but it
    // does not have to: a key the environment holds and the declarations do
    // not name is one the next apply takes away, and the declarations are
    // already in hand (`declared_keys`). Demanding the destination for it
    // would report a gap against a plan this account can run (DECISIONS 513).
    let removable: BTreeMap<&ObjectName, ObjectName> = data
        .iter()
        .filter(|(_, demand)| demand.removes())
        .filter_map(|(declared, _)| Some((declared, data_securable.get(declared)?.clone())))
        .collect();
    let removable_names: Vec<ObjectName> = removable.values().cloned().collect();
    // The managed tables this plan moves between schemas, by the name the
    // environment has for them now — which is the name the discovery query
    // answers with, since it reads the catalog.
    let moved: BTreeMap<&ObjectName, &ObjectName> = managed_securable
        .iter()
        .filter(|(declared, query)| declared.schema != query.schema)
        .map(|(declared, query)| (query, declared))
        .collect();
    let mut delete_children: BTreeMap<Securable, BTreeSet<String>> = BTreeMap::new();
    if !removable.is_empty() {
        let mut columns = ReferencedColumns::new();
        let mut destinations: BTreeSet<String> = BTreeSet::new();
        let found = self::delete_children(conn, &removable_names).await?;
        // The children come from the catalog, spelled as the catalog stores
        // them; the managed names come from the declarations and the recorded
        // ids. Which of them are one securable is the collation's answer, so a
        // child the managed question already asks about under another spelling
        // is not asked twice (#673).
        let found_names: Vec<ObjectName> = found.keys().cloned().collect();
        let managed_list: Vec<ObjectName> = managed_names.iter().cloned().collect();
        let managed_children: BTreeSet<ObjectName> =
            crate::catalog::matching_table_names(conn, &found_names, &managed_list)
                .await?
                .into_iter()
                .collect();
        for (child, named) in found {
            if !managed_children.contains(&child) {
                columns.insert(child, named);
            } else if let Some(declared) = moved.get(&child)
                && declared_keys
                    .get(*declared)
                    .is_some_and(|targets| targets.iter().any(|t| removable.contains_key(t)))
            {
                destinations.insert(declared.schema.clone());
            }
        }
        let child_perms: Vec<&str> = REQUIRED
            .iter()
            .filter(|r| matches!(r.needed, Needed::DeleteChild))
            .map(|r| r.name)
            .collect();
        let children: Vec<ObjectName> = columns.keys().cloned().collect();
        for (object, granted) in object_permissions(
            conn,
            &children,
            &child_perms,
            Existing::OrNot,
            Columns::Referenced(&columns),
        )
        .await?
        {
            delete_children.insert(Securable::Object(object), granted);
        }
        // Asked at the destination *schema*, the only securable a grant for a
        // not-yet-existing object can sit on, and only if that schema was
        // asked about at all — one the database does not have is reported by
        // `absent_schemas`, not invented as a gap here.
        for schema in destinations {
            if let Some(granted) = per_schema.get(&schema) {
                delete_children.insert(Securable::Schema(schema), granted.clone());
            }
        }
    }

    // Foreign-key targets the declarations do not hold, also at object scope —
    // but every named one, present or not, and that difference from the
    // ledger question is deliberate (see `Existing`). For the ledger a hidden
    // table falls back to the schema question, which still reports a gap;
    // here it would drop the object from the map and `missing` would say
    // nothing. Absent and invisible both fail the apply, and the operator
    // can tell which from the name.
    let referenced_perms: Vec<&str> = REQUIRED
        .iter()
        .filter(|r| matches!(r.needed, Needed::Referenced))
        .map(|r| r.name)
        .collect();
    // A target the CLI kept because no declaration spells it exactly may still
    // be a managed table under the database's collation: `app.parent` beside a
    // declared `app.Parent` is one securable on a case-insensitive database,
    // already asked about as a managed table, and was reported twice under two
    // spellings (#673). The CLI reads the declarations offline and serves
    // PostgreSQL too, so the collation is asked here, where the engine is known.
    let candidates: Vec<ObjectName> = referenced.keys().cloned().collect();
    let managed_list: Vec<ObjectName> = managed_names.iter().cloned().collect();
    let own: BTreeSet<ObjectName> =
        crate::catalog::matching_table_names(conn, &candidates, &managed_list)
            .await?
            .into_iter()
            .collect();
    let referenced_names: Vec<ObjectName> = candidates
        .into_iter()
        .filter(|name| !own.contains(name))
        .collect();
    let referenced_objects = object_permissions(
        conn,
        &referenced_names,
        &referenced_perms,
        Existing::OrNot,
        Columns::Referenced(referenced),
    )
    .await?;

    // What the declared roles are granted on (ADR-0005). Objects the way the
    // foreign-key targets are asked — every named one, so absent and
    // invisible both land as a gap — and schemas through `sys.schemas`, so a
    // schema that does not exist yet is unasked rather than reported.
    let mut granted_objects: BTreeMap<ObjectName, BTreeSet<String>> = BTreeMap::new();
    let mut granted_schemas: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    // A declared target `resolve_for_query` could not safely ask about — see
    // its own doc comment. Reported by `missing` as an unconditional gap:
    // GRANTED has no schema-scope fallback the way the data requirements do.
    let mut granted_unresolvable: BTreeSet<ObjectName> = BTreeSet::new();
    // A schema a role is granted on that the database does not have: the
    // `GRANT ... ON SCHEMA::x` fails at apply, and pbps never creates a
    // schema, so it is reported like a managed schema that is missing rather
    // than dropped by the join below.
    let mut absent_granted: BTreeSet<String> = BTreeSet::new();
    // The managed roles: the project's, plus every role the environment's
    // recorded state holds — a role pbps applied is a role pbps manages,
    // whether or not the declarations still name it (a `drop-role` removes
    // it from the ids file before the plan that drops it runs). Tombstones
    // are not consulted: they are permanent, and a drop applied long ago
    // would keep the role requirements switched on forever.
    let mut roles: BTreeSet<String> = granted.roles.iter().cloned().collect();
    roles.extend(recorded.roles.keys().cloned());
    // Any role-shaped demand switches the role requirements on: a managed
    // role by name, or a grant target the caller asks about.
    let roles_declared =
        !roles.is_empty() || !granted.objects.is_empty() || !granted.schemas.is_empty();
    if roles_declared {
        let targets = granted;
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
        // Resolved like the data tables above, and for the same reason: a
        // declared grant target is a table this project manages, and a
        // pending rename this environment has not caught up to yet leaves it
        // answering to its old name. `recorded.roles`' own targets, added
        // below, need no such resolution — they are read back from this
        // environment's own last-recorded state, so they already name
        // whatever it currently calls the object. A target
        // `resolve_for_query` judges unsafe to resolve is not asked about at
        // all — see `granted_unresolvable`'s doc comment — rather than risk
        // reading a different identity's object under its name.
        let colliding =
            claimed_elsewhere(conn, targets.objects.iter(), project_ids, &recorded_ids).await?;
        let (safe_targets, unresolvable_targets) = resolve_for_query(
            targets.objects.iter(),
            project_ids,
            &recorded_ids,
            &colliding,
        );
        granted_unresolvable.extend(unresolvable_targets);
        let mut objects: BTreeSet<ObjectName> = safe_targets.into_values().collect();
        let mut schemas_wanted: BTreeSet<String> = targets.schemas.iter().cloned().collect();
        for role in recorded.roles.values() {
            for target in role.grants.keys() {
                match target {
                    pbps_model::GrantTarget::Object(o) => {
                        objects.insert(o.clone());
                    }
                    // The engine knows the routine by its bare name, so that
                    // is what a permission read asks about — and a recorded
                    // state on this dialect never holds one anyway
                    // (ADR-0009 §1).
                    pbps_model::GrantTarget::Routine(r) => {
                        objects.insert(r.name.clone());
                    }
                    pbps_model::GrantTarget::Schema(s) => {
                        schemas_wanted.insert(s.clone());
                    }
                }
            }
        }
        // In pieces for the same ceiling as every other list here (#351).
        // `IN ()` is not T-SQL, and an empty list is cut into no pieces, so
        // nothing is asked when no role is named.
        let roles: Vec<&String> = roles.iter().collect();
        for chunk in roles.chunks(MAX_PARAMETERS) {
            let mut params: Vec<Param<'_>> = Vec::new();
            let mut role_slots = Vec::new();
            for r in chunk {
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
                        objects.insert(ObjectName::new(schema, object));
                    }
                    (3, Some(schema), _) => {
                        schemas_wanted.insert(schema.to_owned());
                    }
                    _ => {}
                }
            }
        }
        let objects: Vec<ObjectName> = objects.into_iter().collect();
        let schemas_wanted: Vec<String> = schemas_wanted.into_iter().collect();
        granted_objects = object_permissions(
            conn,
            &objects,
            &granted_perms,
            Existing::OrNot,
            Columns::Catalog,
        )
        .await?;
        if !schemas_wanted.is_empty() {
            // The same statement as the managed-schema read, in the same
            // pieces (#351): a role granted on more schemas than one
            // statement can bind would otherwise read as unreadable.
            let wanted: Vec<&str> = schemas_wanted.iter().map(String::as_str).collect();
            for chunk in schema_statements(&wanted, granted_perms.len()) {
                let (sql, params) = schema_permissions_sql(chunk, &granted_perms);
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
            absent_granted.extend(
                schemas_wanted
                    .iter()
                    .filter(|s| !granted_schemas.contains_key(*s))
                    .cloned(),
            );
        }
    }

    // The other half of the delete count's proof, asked only of a project that
    // removes rows (DECISIONS 505). This is `counting_statement`'s own second
    // check, read-only and word for word: a metadata `DENY` that reaches this
    // principal directly or through a role, on an object or a schema whose
    // effective `VIEW DEFINITION` is therefore 0. A database grant does not
    // win against it (460), so asking `HAS_PERMS_BY_NAME` at the database
    // alone would answer 1 and still leave the count refusing.
    let mut metadata_denials: BTreeSet<Securable> = BTreeSet::new();
    if data.values().any(DataDemand::removes) {
        // `sys.database_permissions.class` is a `tinyint`; the driver hands
        // that back as a `u8` and reading it as an `i32` fails outright, so
        // the widening happens in the engine where the column is named.
        let sql = "SELECT CONVERT(int, dp.class) AS class, dp.major_id AS major_id, \
                   OBJECT_SCHEMA_NAME(dp.major_id) AS obj_schema, \
                   OBJECT_NAME(dp.major_id) AS obj_name, \
                   SCHEMA_NAME(dp.major_id) AS sch_name \
                   FROM sys.database_permissions dp \
                   WHERE dp.state = N'D' \
                     AND dp.permission_name IN (N'VIEW DEFINITION', N'CONTROL') \
                     AND (dp.grantee_principal_id = USER_ID() \
                          OR IS_MEMBER(USER_NAME(dp.grantee_principal_id)) = 1) \
                     AND ((dp.class = 1 AND dp.minor_id = 0 \
                           AND COALESCE(HAS_PERMS_BY_NAME( \
                                 QUOTENAME(OBJECT_SCHEMA_NAME(dp.major_id)) + N'.' \
                                 + QUOTENAME(OBJECT_NAME(dp.major_id)), \
                                 N'OBJECT', N'VIEW DEFINITION'), 0) <> 1) \
                       OR (dp.class = 3 \
                           AND COALESCE(HAS_PERMS_BY_NAME(SCHEMA_NAME(dp.major_id), \
                                 N'SCHEMA', N'VIEW DEFINITION'), 0) <> 1));";
        for row in &conn.query(sql).await? {
            let class: i32 = row.try_get("class")?.unwrap_or(0);
            let id: i32 = row.try_get("major_id")?.unwrap_or(0);
            // A securable the account cannot name is still a denial it cannot
            // see through, so an unread name travels as its id rather than
            // being dropped: silence here would read as "no DENY", and the
            // `DENY` this reports is itself what hides the name.
            let securable = if class == 3 {
                match row.try_get::<&str>("sch_name")? {
                    Some(name) => Securable::Schema(name.to_owned()),
                    None => Securable::Unreadable {
                        class: "SCHEMA",
                        id,
                    },
                }
            } else {
                match (
                    row.try_get::<&str>("obj_schema")?,
                    row.try_get::<&str>("obj_name")?,
                ) {
                    (Some(schema), Some(name)) => Securable::Object(ObjectName::new(schema, name)),
                    _ => Securable::Unreadable {
                        class: "OBJECT",
                        id,
                    },
                }
            };
            metadata_denials.insert(securable);
        }
    }

    let ledger_schema = per_schema.get(LEDGER_SCHEMA).cloned().unwrap_or_default();
    // Asked for and not returned by `sys.schemas` means the database does not
    // have it. The ledger's schema is excluded: `dbo` always exists, and if it
    // somehow did not, that is not a declaration problem.
    let mut absent_schemas: BTreeSet<String> = managed
        .iter()
        .filter(|name| **name != LEDGER_SCHEMA && !per_schema.contains_key(**name))
        .map(|name| (*name).to_owned())
        .collect();
    absent_schemas.extend(absent_granted);
    // `dbo` stays only if declared or recorded tables make it managed.
    per_schema.retain(|name, _| managed.contains(name.as_str()));

    Ok(Held {
        database,
        schemas: per_schema,
        managed_tables,
        absent_schemas,
        ledger_schema,
        ledger_objects,
        ledger_migration_needed,
        referenced_objects,
        delete_children,
        roles_declared,
        metadata_denials,
        granted_objects,
        granted_unresolvable,
        granted_schemas,
        // Keyed by the declared name, unlike `data_objects` — see
        // `Held::data_tables`'s doc comment for why.
        data_tables: data.clone(),
        data_securable,
        data_objects,
    })
}

/// Keep each declaration's fallback independent from the recorded names that
/// still exist, and choose the narrowest securable whose answer was read.
///
/// # Why a cross-schema move is *not* answered here
///
/// `ALTER SCHEMA <dest> TRANSFER` drops every permission on the object it
/// moves, so the source answer says nothing about any statement that runs
/// afterwards (DECISIONS 512). This requirement is not one of them: it is the
/// pre-flight probes' `SELECT`, and `preflight` runs before
/// `execute_statements`, so the probes read the table where it still is. The
/// only read that happens *after* the transfer is the row read-back, which is
/// scoped to the plan's data tables — so the destination is demanded by
/// `data_gaps`, of the tables that are really read there, and not of every
/// managed table (issue #517, and the round that narrowed it).
fn managed_table_rights<'a>(
    declared: &[ObjectName],
    resolved: &BTreeMap<ObjectName, ObjectName>,
    recorded: impl Iterator<Item = &'a ObjectName>,
    objects: &BTreeMap<ObjectName, BTreeSet<String>>,
    schemas: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<Securable, BTreeSet<String>> {
    let mut rights = BTreeMap::new();
    for (table, query) in declared
        .iter()
        .map(|table| (table, resolved.get(table)))
        .chain(recorded.map(|table| (table, Some(table))))
    {
        if let Some((query, granted)) = query.and_then(|q| objects.get(q).map(|g| (q, g))) {
            rights.insert(Securable::Object(query.clone()), granted.clone());
        } else if let Some(granted) = schemas.get(&table.schema) {
            rights.insert(Securable::Schema(table.schema.clone()), granted.clone());
        }
    }
    rights
}

/// The declared data tables that are missing `r`, at the securable each of
/// them can be answered at.
///
/// Spelled once for the three data requirements: they ask the same question of
/// the same tables and differ only in which demand switches a table on, which
/// is what `wanted` names.
///
/// The scope is chosen per table, exactly as the ledger's is. A table the
/// catalog shows can only be answered at object scope, because that is where a
/// careful DBA's grant sits and where a `DENY` on it would be; a table this
/// deployment has still to create has no object to ask about, and the grant
/// that will cover it is the one on its schema.
///
/// Deduplicated against this requirement's own gaps, like the ledger's: five
/// tables in one schema that all fall back to it need one `GRANT`, not five
/// identical lines telling the operator to run it five times.
///
/// # Why a cross-schema move is answered twice here too
///
/// The object answer is the *source's*, and `ALTER SCHEMA ... TRANSFER` drops
/// every permission on the object it moves — not only the read (512). Measured
/// on the pinned image: a login holding `SELECT, INSERT, UPDATE, DELETE` on
/// `app.old_name` and `SELECT` on `SCHEMA::dest` ran the transfer and was then
/// refused its `INSERT` with error 229, while `HAS_PERMS_BY_NAME` answered 1
/// for the destination's `SELECT` and 0 for its `INSERT`. So demanding the
/// destination for the read alone would have reported a remedy that makes
/// `doctor` go green on an environment where the first row still fails — the
/// misleading all-clear being worse than the silence it replaced.
///
/// The destination is therefore asked for **every** data requirement, at its
/// schema, which is the only securable a grant for a not-yet-existing object
/// can sit on. The source answer is kept as well: the rows are written where
/// the table is now when no transfer is in the plan, and `doctor` never sees a
/// plan.
fn data_gaps(held: &Held, r: &Requirement, wanted: fn(&DataDemand) -> bool, out: &mut Vec<Gap>) {
    let mut reported: Vec<Securable> = Vec::new();
    for (table, demand) in &held.data_tables {
        if !wanted(demand) {
            continue;
        }
        // The current physical name, if `permissions` judged one safe to ask
        // about (`Held::data_securable`'s doc comment) and the catalog shows
        // an object under it. Either way, no object, so the schema — and
        // only if *it* was asked about. A schema the database does not have
        // produced no row from `sys.schemas`, which is not the same as
        // holding nothing there; `absent_schemas` reports it, and inventing a
        // gap on a securable no `GRANT` can name yet would fire on every
        // first deployment.
        let (granted, securable) = match held
            .data_securable
            .get(table)
            .and_then(|query| held.data_objects.get(query).map(|g| (g, query)))
        {
            Some((granted, query)) => (granted, Securable::Object(query.clone())),
            None => match held.schemas.get(&table.schema) {
                Some(granted) => (granted, Securable::Schema(table.schema.clone())),
                None => continue,
            },
        };
        if !granted.contains(r.name) && !reported.contains(&securable) {
            reported.push(securable.clone());
            out.push(Gap {
                permission: r.name,
                why: r.why,
                securable,
            });
        }
        // The destination of a move between schemas, when the resolution
        // crossed one. See the note above: the source's answer says nothing
        // about any statement that runs after the transfer.
        let Some(query) = held.data_securable.get(table) else {
            continue;
        };
        if query.schema == table.schema {
            continue;
        }
        let destination = Securable::Schema(table.schema.clone());
        if let Some(granted) = held.schemas.get(&table.schema)
            && !granted.contains(r.name)
            && !reported.contains(&destination)
        {
            reported.push(destination.clone());
            out.push(Gap {
                permission: r.name,
                why: r.why,
                securable: destination,
            });
        }
    }
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
            Needed::ManagedTable => {
                for (securable, granted) in &held.managed_tables {
                    if !granted.contains(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: securable.clone(),
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
            Needed::LedgerMigration => {
                let state = &ledger_tables()[0];
                if held.ledger_migration_needed
                    && let Some(granted) = held.ledger_objects.get(state)
                    && !granted.contains(r.name)
                {
                    out.push(Gap {
                        permission: r.name,
                        why: r.why,
                        securable: Securable::Object(state.clone()),
                    });
                }
            }
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
                // Never asked about at all — see `Held::granted_unresolvable`'s
                // doc comment. GRANTED has no schema-scope fallback, so an
                // unconditional gap is the only honest answer: this
                // resolution cannot say the account holds it.
                for object in &held.granted_unresolvable {
                    out.push(Gap {
                        permission: r.name,
                        why: r.why,
                        securable: Securable::Object(object.clone()),
                    });
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
            Needed::DataInsert => data_gaps(held, r, DataDemand::inserts, &mut out),
            Needed::DataUpdate => data_gaps(held, r, DataDemand::corrects, &mut out),
            // `exact` only, for the reason the variant gives: `ensure` never
            // emits a DELETE, and demanding one would be asking for the
            // permission that mode was chosen to withhold.
            Needed::DataDelete => data_gaps(held, r, DataDemand::removes, &mut out),
            // Every table that declares rows: see the variant. The predicate
            // the other three carry is what distinguishes them, and this one
            // has nothing to distinguish.
            Needed::DataRead => data_gaps(held, r, |_| true, &mut out),
            // Per child, because that is where the `GRANT` goes. Empty unless
            // the declarations can remove a row, so a project that only
            // inserts and corrects is never told to hold this.
            Needed::DeleteChild => {
                for (securable, granted) in &held.delete_children {
                    if !granted.contains(r.name) {
                        out.push(Gap {
                            permission: r.name,
                            why: r.why,
                            securable: securable.clone(),
                        });
                    }
                }
            }
            // Only when a row would be removed, and asked at the database
            // because a policy can live in a schema this project does not
            // manage: `Needed::Managed`'s own `VIEW DEFINITION` cannot see it
            // (DECISIONS 505). `ensure` never emits a DELETE, so a project
            // that only inserts and corrects is never told to hold this.
            Needed::DeleteCatalog => {
                if held.data_tables.values().any(DataDemand::removes)
                    && !held.database.contains(r.name)
                {
                    out.push(Gap {
                        permission: r.name,
                        why: r.why,
                        securable: Securable::Database,
                    });
                }
            }
            // Reported per securable, because that is where the remedy is: the
            // database grant above can be held in full and the count still
            // refuse. Gathered only under the same condition, so this is empty
            // for a project that removes no rows.
            Needed::DeleteCatalogDenied => {
                for securable in &held.metadata_denials {
                    out.push(Gap {
                        permission: r.name,
                        why: r.why,
                        securable: securable.clone(),
                    });
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
                for table in ledger_tables() {
                    let (granted, securable) = match held.ledger_objects.get(&table) {
                        Some(granted) => (granted, Securable::Object(table)),
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
    use pbps_model::schema::Table;

    #[test]
    fn migration_alter_is_scoped_to_the_existing_state_table() {
        let mut held = everything(&[]);
        held.ledger_migration_needed = true;
        for existing in [
            None,
            Some(ledger_tables()[1].clone()),
            Some(ledger_tables()[0].clone()),
        ] {
            held.ledger_objects.clear();
            if let Some(object) = existing.clone() {
                held.ledger_objects.insert(
                    object,
                    ["SELECT", "INSERT", "DELETE"]
                        .map(str::to_owned)
                        .into_iter()
                        .collect(),
                );
            }
            let gaps = missing(&held);
            assert_eq!(
                gaps.len(),
                usize::from(existing == Some(ledger_tables()[0].clone())),
                "{gaps:?}"
            );
        }
        held.ledger_migration_needed = false;
        assert!(missing(&held).is_empty());
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
            // `DeleteCatalog` is database-scoped like `Needed::Database` and
            // belongs in a helper that means "this account holds everything":
            // leaving it out would put one extra gap into every test about a
            // `mode: exact` table, where the property under test is which DML
            // that table is asked for. The requirement has its own tests.
            // (`RoleAdmin` stays out because the helper leaves
            // `roles_declared` false, so it is never asked about at all.)
            database: REQUIRED
                .iter()
                .filter(|r| matches!(r.needed, Needed::Database | Needed::DeleteCatalog))
                .map(|r| r.name.to_owned())
                .collect(),
            schemas: schemas
                .iter()
                .map(|s| ((*s).to_owned(), schema_perms.clone()))
                .collect(),
            managed_tables: BTreeMap::new(),
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
            ledger_migration_needed: false,
            referenced_objects: BTreeMap::new(),
            delete_children: BTreeMap::new(),
            roles_declared: false,
            metadata_denials: BTreeSet::new(),
            granted_objects: BTreeMap::new(),
            granted_unresolvable: BTreeSet::new(),
            granted_schemas: BTreeMap::new(),
            // No `data:` block is the overwhelmingly common case, so this
            // helper declares none; the tests that need one add it.
            data_tables: DataTables::new(),
            data_securable: BTreeMap::new(),
            data_objects: BTreeMap::new(),
        }
    }

    /// The securable handed to `HAS_PERMS_BY_NAME` and to `OBJECT_ID` is
    /// assembled by the server from the two bound parts, `QUOTENAME` on
    /// each. Passed as one `schema.object` string, a name holding a `.` or a
    /// `]` was read back through the engine's name parser and answered 0 or
    /// NULL — a gap on a permission the account held.
    #[test]
    fn the_object_securable_is_quoted_by_the_server_from_two_bound_parts() {
        let objects = [ObjectName::new("dbo", "a.b"), ObjectName::new("app", "x]y")];
        for existing in [Existing::Only, Existing::OrNot] {
            let (sql, params) =
                object_permissions_sql(&objects, &["SELECT"], existing, Columns::Catalog);
            // Assembled once, in the server, and every question asks the same
            // assembled name.
            assert!(
                sql.contains(
                    "CROSS APPLY (VALUES (QUOTENAME(o.s) + N'.' + QUOTENAME(o.n))) AS x(q)"
                ),
                "{sql}"
            );
            assert!(
                sql.contains("HAS_PERMS_BY_NAME(x.q, 'OBJECT', p.n)"),
                "{sql}"
            );
            assert!(!sql.contains("HAS_PERMS_BY_NAME(o.n"), "{sql}");
            assert!(sql.contains("AS o(s, n, i)"), "{sql}");
            // One permission, then two slots per object: the parts, never
            // the joined name.
            assert_eq!(params.len(), 1 + 2 * objects.len(), "{sql}");
            assert!(sql.contains("(@P2, @P3, 0), (@P4, @P5, 1)"), "{sql}");
            assert_eq!(
                sql.contains("WHERE OBJECT_ID(x.q, N'U') IS NOT NULL"),
                existing == Existing::Only,
                "{sql}"
            );
        }
    }

    /// The column-scope fallback is put only for the permissions SQL Server
    /// takes at column scope. `INSERT` and `DELETE` answer NULL there, and a
    /// NULL read as "not held" would turn the fallback into a gap on every
    /// table that has one — the under-reported over-demand this whole enum
    /// exists to remove, one securable further down.
    #[test]
    fn only_the_permissions_the_engine_takes_at_column_scope_carry_the_fallback() {
        let objects = [ObjectName::new("app", "t")];
        let perms = ["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES"];
        let (sql, params) =
            object_permissions_sql(&objects, &perms, Existing::OrNot, Columns::Catalog);
        // The flag is a literal beside each bound name, in the order asked.
        assert!(
            sql.contains("(@P1, 1), (@P2, 0), (@P3, 1), (@P4, 0), (@P5, 1)"),
            "{sql}"
        );
        assert!(sql.contains("WHEN p.col = 0 THEN 0"), "{sql}");
        // And the flag costs no parameter of its own: the wide lists cannot
        // spare one per permission.
        assert_eq!(params.len(), perms.len() + 2 * objects.len(), "{sql}");
        for p in perms {
            assert_eq!(
                column_scoped(p),
                COLUMN_SCOPED.contains(&p),
                "the engine's list, not this test's: {p}"
            );
        }
        // Lower case and a stray space are the same permission to the engine.
        assert!(column_scoped(" update "), "trimmed and case-folded");
        assert!(!column_scoped("CONTROL"), "not a column-scope permission");
    }

    /// The declared column list is bound three slots to a column — the
    /// object's two parts and the name — and read twice from the same
    /// parameters, so a wide table does not cost six.
    #[test]
    fn the_declared_columns_are_bound_once_and_read_twice() {
        let objects = [ObjectName::new("app", "t"), ObjectName::new("app", "u")];
        let declared: BTreeMap<ObjectName, Vec<String>> = [
            (
                objects[0].clone(),
                vec!["label".to_owned(), "note".to_owned()],
            ),
            // A table whose only column is its key declares none, and asking
            // about no column must not make the object's own answer vanish.
            (objects[1].clone(), Vec::new()),
        ]
        .into_iter()
        .collect();
        let (sql, params) = object_permissions_sql(
            &objects,
            &["UPDATE"],
            Existing::Only,
            Columns::Declared(&declared),
        );
        // One slot a column, and the object's two parts bound once each.
        assert_eq!(params.len(), 1 + 2 * 2 + 2, "{sql}");
        assert!(sql.contains("AS c(i, col)"), "{sql}");
        assert!(
            sql.contains("HAS_PERMS_BY_NAME(x.q, 'OBJECT', p.n, c.col, 'COLUMN') = 0"),
            "{sql}"
        );
        // The object is named by its position, a literal on both sides.
        assert!(sql.contains("(@P2, @P3, 0), (@P4, @P5, 1)"), "{sql}");
        // Twice in the statement, and the same slots both times.
        assert_eq!(sql.matches("(0, @P6), (0, @P7)").count(), 2, "{sql}");
        assert_eq!(sql.matches("WHERE c.i = o.i").count(), 2, "{sql}");
        // Nothing declared at all leaves the object answer standing rather
        // than an empty `VALUES`, which is not T-SQL.
        let empty: BTreeMap<ObjectName, Vec<String>> = BTreeMap::new();
        let (sql, params) = object_permissions_sql(
            &objects,
            &["UPDATE"],
            Existing::Only,
            Columns::Declared(&empty),
        );
        assert!(!sql.contains("AS c(i, col)"), "{sql}");
        assert!(sql.contains("WHEN 1 = 0 THEN"), "{sql}");
        assert_eq!(params.len(), 1 + 2 * 2, "{sql}");
    }

    /// The schema read in pieces (#351): each piece fits beside the whole
    /// permission list, every schema is asked once across them, and a list
    /// that fits exactly is still one statement.
    #[test]
    fn schema_permission_reads_are_cut_to_the_parameter_ceiling() {
        let perms = ["ALTER", "SELECT", "INSERT", "UPDATE", "DELETE"];
        let names: Vec<String> = (0..5_000).map(|i| format!("s{i}")).collect();
        let schemas: Vec<&str> = names.iter().map(String::as_str).collect();
        let chunks = schema_statements(&schemas, perms.len());
        assert!(chunks.len() > 1);
        let mut seen = Vec::new();
        for chunk in &chunks {
            assert!(!chunk.is_empty());
            let (sql, params) = schema_permissions_sql(chunk, &perms);
            assert!(params.len() <= MAX_PARAMETERS, "{} params", params.len());
            assert_eq!(params.len(), perms.len() + chunk.len());
            assert!(sql.contains(&format!("(@P{})", params.len())));
            seen.extend(chunk.iter().copied());
        }
        assert_eq!(seen, schemas, "every schema once, in order");
        let per = MAX_PARAMETERS - perms.len();
        assert_eq!(schema_statements(&schemas[..per], perms.len()).len(), 1);
        assert_eq!(schema_statements(&schemas[..per + 1], perms.len()).len(), 2);
        assert_eq!(schema_statements(&schemas[..1], perms.len()).len(), 1);
    }

    /// One statement per chunk, each within the server's parameter limit,
    /// and every object asked about exactly once across the chunks. At two
    /// slots per object, a list past about a thousand names used to become
    /// one statement the server refused.
    #[test]
    fn a_long_object_list_is_asked_in_statements_within_the_parameter_limit() {
        let perms = ["SELECT", "REFERENCES", "CONTROL"];
        let objects: Vec<ObjectName> = (0..1_100)
            .map(|i| ObjectName::new("app", format!("t{i}")))
            .collect();
        let chunks = object_statements(&objects, perms.len(), Columns::Catalog);
        assert!(chunks.len() > 1, "1,100 objects must not fit one statement");
        let mut seen = 0;
        for chunk in &chunks {
            let (sql, params) =
                object_permissions_sql(chunk, &perms, Existing::OrNot, Columns::Catalog);
            assert!(
                params.len() <= MAX_PARAMETERS,
                "{} params: {sql}",
                params.len()
            );
            assert_eq!(params.len(), perms.len() + 2 * chunk.len());
            seen += chunk.len();
        }
        assert_eq!(seen, objects.len(), "every object is in exactly one chunk");
        // And the largest chunk is as large as the limit allows: one object
        // more would cross it.
        let per = chunks[0].len();
        assert!(perms.len() + 2 * (per + 1) > MAX_PARAMETERS, "per={per}");
        // A list that fits is one statement, as before.
        assert_eq!(
            object_statements(&objects[..10], perms.len(), Columns::Catalog).len(),
            1
        );
    }

    /// The declared columns are what a chunk is packed against, not the
    /// object count: at three slots a column, one wide table is worth
    /// hundreds of narrow ones, and a fixed chunk size would be sized either
    /// for the widest table in the estate or for none of them.
    #[test]
    fn declared_columns_are_counted_into_the_parameter_limit() {
        let perms = ["INSERT", "UPDATE", "DELETE"];
        // The widest table SQL Server will hold is 1,024 columns, so 1,023 is
        // the most an `UPDATE` could ever have to be held on. It has to fit
        // one statement *by itself*: bound at three slots a column, a table
        // of 698 crossed the limit, and `doctor` failed the whole permission
        // read on a table the engine takes without complaint.
        let widest: Vec<String> = (0..1_023).map(|i| format!("c{i}")).collect();
        let one = [ObjectName::new("app", "wide")];
        let declared: BTreeMap<ObjectName, Vec<String>> =
            [(one[0].clone(), widest)].into_iter().collect();
        let columns = Columns::Declared(&declared);
        assert_eq!(object_statements(&one, perms.len(), columns).len(), 1);
        let (_, params) = object_permissions_sql(&one, &perms, Existing::Only, columns);
        assert!(params.len() <= MAX_PARAMETERS, "{} params", params.len());

        // Across objects the packing still bites, and it is the columns that
        // decide it: the same forty tables fit one statement when the column
        // list comes from the catalog.
        let wide: Vec<String> = (0..100).map(|i| format!("c{i}")).collect();
        let objects: Vec<ObjectName> = (0..40)
            .map(|i| ObjectName::new("app", format!("t{i}")))
            .collect();
        let declared: BTreeMap<ObjectName, Vec<String>> =
            objects.iter().map(|o| (o.clone(), wide.clone())).collect();
        let columns = Columns::Declared(&declared);
        let chunks = object_statements(&objects, perms.len(), columns);
        assert!(chunks.len() > 1, "{} chunks", chunks.len());
        assert_eq!(
            object_statements(&objects, perms.len(), Columns::Catalog).len(),
            1,
            "the same forty objects fit one statement when the columns come \
             from the catalog"
        );
        let mut seen = 0;
        for chunk in &chunks {
            let (_, params) = object_permissions_sql(chunk, &perms, Existing::Only, columns);
            assert!(params.len() <= MAX_PARAMETERS, "{} params", params.len());
            seen += chunk.len();
        }
        assert_eq!(seen, objects.len(), "every table is in exactly one chunk");
        // A declaration wider than any table the engine would create is asked
        // alone and fails loudly on the server, rather than being dropped —
        // a dropped table reads as ready.
        let huge: BTreeMap<ObjectName, Vec<String>> = [(
            objects[0].clone(),
            (0..4_000).map(|i| format!("c{i}")).collect(),
        )]
        .into_iter()
        .collect();
        let chunks = object_statements(&objects[..1], perms.len(), Columns::Declared(&huge));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 1);
    }

    #[test]
    fn referenced_column_unions_fit_the_rpc_parameter_budget_without_losing_targets() {
        let objects: Vec<_> = (0..40)
            .map(|i| ObjectName::new("shared", format!("parent{i}")))
            .collect();
        let referenced: ReferencedColumns = objects
            .iter()
            .map(|o| (o.clone(), (0..100).map(|i| format!("c{i}")).collect()))
            .collect();
        let columns = Columns::Referenced(&referenced);
        let perms = ["SELECT", "REFERENCES"];
        let chunks = object_statements(&objects, perms.len(), columns);
        assert!(
            chunks.len() > 1,
            "one statement would exceed the RPC budget"
        );
        let mut seen = Vec::new();
        for chunk in chunks {
            let (_, params) = object_permissions_sql(chunk, &perms, Existing::OrNot, columns);
            assert!(params.len() <= MAX_PARAMETERS, "{} params", params.len());
            seen.extend_from_slice(chunk);
        }
        assert_eq!(
            seen, objects,
            "every target is asked once, including the final chunk"
        );
        // Missing subset entries still carry their object-level demand.
        let empty = ReferencedColumns::new();
        let chunks = object_statements(&objects, perms.len(), Columns::Referenced(&empty));
        assert_eq!(chunks, vec![objects.as_slice()]);
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

    /// A declared table as `DataDemand::of` reads one: a single-column primary
    /// key named `code`, one extra column per name in `extra`, and a `data:`
    /// block in `mode` carrying one row per key in `rows`.
    ///
    /// Built rather than hand-written because `DataDemand` has one
    /// constructor: what a declaration demands is derived in a single place,
    /// so a test that asserted on a hand-made value would be asserting about a
    /// value the tool never produces.
    fn declared(mode: pbps_model::DataMode, rows: &[&str], extra: &[&str]) -> Table {
        let mut t = Table {
            primary_key: Some(pbps_model::schema::PrimaryKey {
                name: None,
                columns: vec!["code".to_owned()],
            }),
            data: Some(pbps_model::TableData {
                mode,
                rows: rows
                    .iter()
                    .map(|k| {
                        (
                            pbps_model::RowKey((*k).to_owned()),
                            pbps_model::Row(BTreeMap::new()),
                        )
                    })
                    .collect(),
            }),
            ..Table::default()
        };
        let ty: pbps_model::ColumnType = "varchar(20)".parse().expect("a type this crate spells");
        t.columns
            .insert("code".to_owned(), pbps_model::Column::new(ty.clone()));
        for name in extra {
            t.columns
                .insert((*name).to_owned(), pbps_model::Column::new(ty.clone()));
        }
        t
    }

    /// What a table demands, or the panic that says it demands nothing.
    fn demand(mode: pbps_model::DataMode, rows: &[&str], extra: &[&str]) -> DataDemand {
        DataDemand::of(&declared(mode, rows, extra))
            .unwrap_or_else(|| panic!("{mode} with {} row(s) demands nothing", rows.len()))
    }

    fn exact_table() -> DataDemand {
        demand(pbps_model::DataMode::Exact, &["a"], &["label"])
    }

    fn ensure_table() -> DataDemand {
        demand(pbps_model::DataMode::Ensure, &["a"], &["label"])
    }

    /// The same holdings with the DML struck out of every schema, which is
    /// what an account granted exactly the list `doctor` used to print holds.
    fn everything_but_the_dml(schemas: &[&str]) -> Held {
        let mut held = everything(schemas);
        for granted in held.schemas.values_mut() {
            for p in ["INSERT", "UPDATE", "DELETE"] {
                granted.remove(p);
            }
        }
        held
    }

    fn table(name: &str) -> ObjectName {
        name.parse().expect("a `schema.table` constant")
    }

    #[test]
    fn managed_probe_rights_keep_the_existing_object_and_future_name_distinct() {
        let old = table("app.old");
        let renamed = table("app.renamed");
        let schemas = [("app".to_owned(), BTreeSet::new())].into_iter().collect();
        let objects = [(old.clone(), ["SELECT".to_owned()].into_iter().collect())]
            .into_iter()
            .collect();
        let resolved = [(renamed.clone(), old.clone())].into_iter().collect();
        let mut held = everything(&["app"]);
        held.managed_tables = managed_table_rights(
            &[renamed, old.clone()],
            &resolved,
            std::iter::once(&old),
            &objects,
            &schemas,
        );
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "SELECT");
        assert_eq!(gaps[0].securable, Securable::Schema("app".into()));
        assert!(held.managed_tables[&Securable::Object(old)].contains("SELECT"));
    }

    #[test]
    fn managed_probe_rights_keep_recorded_tables_and_do_not_invent_schema_gaps() {
        let old = table("app.old");
        let schemas = [(
            "app".to_owned(),
            ["SELECT".to_owned()].into_iter().collect(),
        )]
        .into_iter()
        .collect();
        let objects = [(old.clone(), BTreeSet::new())].into_iter().collect();
        let mut held = everything(&["app"]);
        held.managed_tables = managed_table_rights(
            &[],
            &BTreeMap::new(),
            std::iter::once(&old),
            &objects,
            &schemas,
        );
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].securable, Securable::Object(old));
        // Nothing declared or recorded must not inherit demands from unrelated
        // visible tables; a missing schema is diagnosed separately.
        assert!(
            managed_table_rights(
                &[table("absent.future")],
                &BTreeMap::new(),
                std::iter::empty(),
                &objects,
                &schemas,
            )
            .is_empty()
        );
        let rights = managed_table_rights(
            &[table("app.future_a"), table("app.future_b")],
            &BTreeMap::new(),
            std::iter::empty(),
            &objects,
            &schemas,
        );
        assert_eq!(
            rights.len(),
            1,
            "one schema fallback for both absent tables"
        );
        assert!(rights[&Securable::Schema("app".into())].contains("SELECT"));
    }

    /// The collision `resolve_for_query` exists for: a rename frees the name
    /// the departing identity carried, and the same plan declares a *new*
    /// table under that freed name before the rename has run against this
    /// environment. Both declared names resolve to `app.old` — naively
    /// asking under it and keying by it once collapsed one of the two
    /// declarations' distinct demands into the other's map entry (issue #133
    /// round 2, `Preserve distinct demands when a rename source is reused`).
    #[test]
    fn only_a_recorded_table_declared_in_another_schema_is_a_move() {
        let uid: pbps_model::Uid = "t_aaa111".parse().expect("a well-formed table uid");
        let mut recorded_ids = pbps_model::IdsFile::default();
        recorded_ids.tables.insert(uid.clone(), table("legacy.t"));
        let moved = |declared: Option<&str>| {
            let mut project_ids = pbps_model::IdsFile::default();
            if let Some(name) = declared {
                project_ids.tables.insert(uid.clone(), table(name));
            }
            moved_to(&table("legacy.t"), &project_ids, &recorded_ids).cloned()
        };
        assert_eq!(moved(Some("modern.t")), Some(table("modern.t")));
        // Renamed within its schema: the schema is still the table's.
        assert_eq!(moved(Some("legacy.renamed")), None);
        // No longer declared: a drop, which keeps its schema managed.
        assert_eq!(moved(None), None);
        // A recorded table the recorded ids do not name is no move either.
        let mut project_ids = pbps_model::IdsFile::default();
        project_ids.tables.insert(uid, table("modern.t"));
        assert_eq!(
            moved_to(&table("legacy.other"), &project_ids, &recorded_ids),
            None
        );
    }

    #[test]
    fn resolve_for_query_keeps_the_confirmed_claim_and_refuses_the_colliding_fallback() {
        let uid1: pbps_model::Uid = "t_aaa111".parse().expect("a well-formed table uid");
        let uid2: pbps_model::Uid = "t_bbb222".parse().expect("a well-formed table uid");
        // The environment's own recorded ids: uid1 is `app.old` here, still.
        // uid2 has no entry at all — the table this uid names has not been
        // created against this environment yet.
        let mut recorded_ids = pbps_model::IdsFile::default();
        recorded_ids.tables.insert(uid1.clone(), table("app.old"));
        // The project's own ids: uid1 has moved to `app.new`, and uid2 is
        // freshly declared under the name uid1 just vacated.
        let mut project_ids = pbps_model::IdsFile::default();
        project_ids.tables.insert(uid1, table("app.new"));
        project_ids.tables.insert(uid2, table("app.old"));

        let wanted = [table("app.new"), table("app.old")];
        // What the engine answers for this pair: the unconfirmed `app.old`
        // names the object the recorded ids give uid1.
        let colliding = [table("app.old")].into_iter().collect();
        let (safe, unresolvable) =
            resolve_for_query(wanted.iter(), &project_ids, &recorded_ids, &colliding);

        assert_eq!(
            safe.get(&table("app.new")),
            Some(&table("app.old")),
            "the confirmed resolution — this environment's own recorded ids \
             say so — is trusted: {safe:?}"
        );
        assert!(
            !safe.contains_key(&table("app.old")),
            "the colliding fallback must not be asked about under a name \
             the recorded ids already give to a different identity: {safe:?}"
        );
        assert_eq!(
            unresolvable,
            [table("app.old")].into_iter().collect(),
            "the declaration that could not be safely resolved is reported, \
             not silently dropped"
        );
    }

    /// The ordinary case, with nothing renamed and nothing reused: every
    /// declared name resolves to itself and none collide.
    /// #676: a declared column resolves to the name this environment has for
    /// it. A pending rename resolves to the recorded name, and a column with no
    /// recorded uid keeps its declared name and is marked as added: it is the
    /// candidate `resolved_column_lists` asks the engine about.
    #[test]
    fn declared_columns_resolve_to_the_names_the_environment_has() {
        use pbps_model::{ColumnRef, IdsFile, Uid, UidKind};
        let table: ObjectName = "app.t".parse().unwrap();
        let column = |name: &str| ColumnRef {
            table: table.clone(),
            name: name.to_owned(),
        };
        let (code, label) = (
            Uid::generate(UidKind::Column),
            Uid::generate(UidKind::Column),
        );
        let mut recorded = IdsFile::default();
        recorded.columns.insert(code.clone(), column("code"));
        recorded.columns.insert(label.clone(), column("label"));
        let mut project = IdsFile::default();
        project.columns.insert(code, column("code"));
        project.columns.insert(label, column("caption"));
        project
            .columns
            .insert(Uid::generate(UidKind::Column), column("note"));
        let declared: Vec<String> = ["code", "caption", "note"]
            .iter()
            .map(|c| (*c).to_owned())
            .collect();
        assert_eq!(
            resolve_columns(&table, &declared, &project, &recorded),
            [
                ("code".to_owned(), true),
                ("label".to_owned(), true),
                ("note".to_owned(), false)
            ]
        );
        // Nothing recorded: every name stays as declared, and is new.
        assert_eq!(
            resolve_columns(
                &table,
                &["code".to_owned()],
                &IdsFile::default(),
                &IdsFile::default()
            ),
            [("code".to_owned(), false)]
        );
    }

    #[test]
    fn resolve_for_query_resolves_every_name_when_nothing_collides() {
        let project_ids = pbps_model::IdsFile::default();
        let recorded_ids = pbps_model::IdsFile::default();
        let wanted = [table("app.a"), table("app.b")];
        let (safe, unresolvable) =
            resolve_for_query(wanted.iter(), &project_ids, &recorded_ids, &BTreeSet::new());
        assert_eq!(safe.get(&table("app.a")), Some(&table("app.a")));
        assert_eq!(safe.get(&table("app.b")), Some(&table("app.b")));
        assert!(unresolvable.is_empty(), "{unresolvable:?}");
    }

    /// The second door into the same collision, round 2's own review found
    /// (issue #133 round 3): a rename to `app.new` frees `app.Old`, and the
    /// same plan declares a new `app.old`. Whether those are one securable is
    /// the database collation's answer, not a fold in Rust (#384), so the
    /// same ids are refused when the engine says they collide and resolved
    /// when it says they do not, as it does on a case-sensitive database.
    #[test]
    fn the_engines_answer_decides_whether_a_differently_spelled_claim_collides() {
        let uid1: pbps_model::Uid = "t_ccc333".parse().expect("a well-formed table uid");
        let uid2: pbps_model::Uid = "t_ddd444".parse().expect("a well-formed table uid");
        let mut recorded_ids = pbps_model::IdsFile::default();
        recorded_ids.tables.insert(uid1.clone(), table("app.Old"));
        let mut project_ids = pbps_model::IdsFile::default();
        project_ids.tables.insert(uid1, table("app.new"));
        project_ids.tables.insert(uid2, table("app.old"));
        let wanted = [table("app.new"), table("app.old")];

        let collides = [table("app.old")].into_iter().collect();
        let (safe, unresolvable) =
            resolve_for_query(wanted.iter(), &project_ids, &recorded_ids, &collides);
        assert_eq!(
            safe.get(&table("app.new")),
            Some(&table("app.Old")),
            "the confirmed resolution keeps the recorded spelling: {safe:?}"
        );
        assert!(
            !safe.contains_key(&table("app.old")),
            "a fallback the engine reads as the claimed securable is not asked \
             about under it: {safe:?}"
        );
        assert_eq!(
            unresolvable,
            [table("app.old")].into_iter().collect(),
            "{unresolvable:?}"
        );

        let (safe, unresolvable) =
            resolve_for_query(wanted.iter(), &project_ids, &recorded_ids, &BTreeSet::new());
        assert_eq!(
            safe.get(&table("app.old")),
            Some(&table("app.old")),
            "where the engine keeps the spellings apart, so does this: {safe:?}"
        );
        assert!(unresolvable.is_empty(), "{unresolvable:?}");
    }

    /// Every DML permission, on one object.
    fn dml() -> BTreeSet<String> {
        ["INSERT", "UPDATE", "DELETE"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// The read `Needed::DataRead` asks for on a data table's own object: the
    /// row read-back that closes an apply, which every declaration carrying
    /// rows makes. Present wherever the premise is an account that holds
    /// everything but the DML, so that the gaps a test names are its own.
    fn read() -> BTreeSet<String> {
        ["SELECT".to_owned()].into_iter().collect()
    }

    /// Both, for a table whose object answer is meant to leave no gap.
    fn dml_and_read() -> BTreeSet<String> {
        dml().union(&read()).cloned().collect()
    }

    /// A declaration that could emit nothing is not a demand of nothing — it
    /// is no entry at all, so `missing` is never asked about it and the three
    /// axes can never all be false.
    #[test]
    fn a_declaration_that_can_emit_nothing_yields_no_demand() {
        // No `data:` block: the overwhelmingly common table.
        assert_eq!(DataDemand::of(&Table::default()), None);
        // `ensure` with no declared row manages no row at all.
        assert_eq!(
            DataDemand::of(&declared(pbps_model::DataMode::Ensure, &[], &["label"])),
            None
        );
        // A `data:` block the differ refuses outright: no single-column key,
        // so no row statement can name one. Reported by `validate`, which
        // `doctor` runs beside this — never silence.
        let mut keyless = declared(pbps_model::DataMode::Exact, &["a"], &["label"]);
        keyless.primary_key = None;
        assert_eq!(DataDemand::of(&keyless), None);
    }

    /// `ALTER ON SCHEMA` confers no DML, and a `data:` block makes the emitter
    /// write `INSERT`, `UPDATE` and `DELETE` against the **managed** tables. An
    /// account holding everything else passed readiness with exit 0, `apply`
    /// took the lock and ran the DDL, and the first row died on "INSERT
    /// permission was denied" — under `--staged`, after earlier checkpoints
    /// had committed.
    #[test]
    fn a_table_declaring_rows_is_asked_for_the_dml_alter_does_not_confer() {
        let mut held = everything_but_the_dml(&["app"]);
        let gaps = missing(&held);
        assert!(
            gaps.is_empty(),
            "a project declaring no row must not be asked for DML: {gaps:?}"
        );

        held.data_tables.insert(table("app.t"), exact_table());
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(table("app.t"), read());
        let gaps = missing(&held);
        let mut named: Vec<String> = gaps
            .iter()
            .map(|g| format!("{} on {}", g.permission, g.securable()))
            .collect();
        named.sort();
        assert_eq!(
            named,
            [
                "DELETE on OBJECT::[app].[t]",
                "INSERT on OBJECT::[app].[t]",
                "UPDATE on OBJECT::[app].[t]",
            ],
            "{gaps:?}"
        );
        // Each says what it is for: the report is "you lack this, for that",
        // and a DML gap indistinguishable from the ledger's would send the
        // operator to grant it on the ledger's schema instead.
        assert!(
            gaps.iter().all(|g| !g.why.contains("lock")),
            "the ledger's reasons must not be reused for reference data: {gaps:?}"
        );

        held.data_objects.insert(table("app.t"), dml_and_read());
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
    }

    /// The finding this scope exists for, measured on the pinned image (see
    /// the live test): a careful DBA grants `INSERT` on the one table that
    /// carries declared rows and nowhere else. That grant answers 1 at object
    /// scope and 0 at schema scope, and the statement really runs — so a
    /// schema-scoped question reports a gap the account does not have, which
    /// is the over-demand this whole list refuses.
    #[test]
    fn a_grant_on_the_table_alone_satisfies_the_check() {
        let mut held = everything_but_the_dml(&["app"]);
        held.data_tables.insert(table("app.t"), exact_table());
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(table("app.t"), dml_and_read());
        assert!(
            held.schemas["app"].is_disjoint(&dml()),
            "the premise: nothing is held on the schema"
        );
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
    }

    /// And the converse, which is the worse direction: `GRANT` on the schema
    /// with a `DENY` on the table answers 1 at schema scope, 0 at object
    /// scope, and the statement really fails. Asked at schema scope, `doctor`
    /// would report ready and `apply` would die on the first row — the bug
    /// this requirement was added to prevent, one securable out.
    #[test]
    fn a_deny_on_the_table_is_a_gap_though_the_schema_grant_stands() {
        let mut held = everything(&["app"]);
        assert!(
            dml().is_subset(&held.schemas["app"]),
            "the premise: the whole schema is granted"
        );
        held.data_tables.insert(table("app.t"), ensure_table());
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(
            table("app.t"),
            ["UPDATE".to_owned(), "SELECT".to_owned()]
                .into_iter()
                .collect(),
        );
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "INSERT");
        assert_eq!(gaps[0].securable(), "OBJECT::[app].[t]");
    }

    /// A table this deployment has still to create has no object to ask
    /// about — `HAS_PERMS_BY_NAME` on a name the catalog does not hold
    /// answers 0 — so the question falls back to its schema, the only place a
    /// grant can sit that early. This is every first deployment of a project
    /// that seeds rows.
    #[test]
    fn a_data_table_that_does_not_exist_yet_is_asked_of_its_schema() {
        let mut held = everything_but_the_dml(&["app"]);
        held.data_tables.insert(table("app.t"), exact_table());
        // Resolved, like `permissions` would resolve it, but the catalog
        // shows no object under that name — the same shape a table this
        // deployment has still to create leaves behind.
        held.data_securable.insert(table("app.t"), table("app.t"));
        let mut named: Vec<String> = missing(&held)
            .iter()
            .map(|g| format!("{} on {}", g.permission, g.securable()))
            .collect();
        named.sort();
        assert_eq!(
            named,
            [
                "DELETE on SCHEMA::[app]",
                "INSERT on SCHEMA::[app]",
                "UPDATE on SCHEMA::[app]",
            ],
            "{:?}",
            missing(&held)
        );

        // And five such tables in one schema are still one line per
        // permission: the operator runs one `GRANT`, not five identical ones.
        for n in ["u", "v", "w", "x"] {
            held.data_tables
                .insert(table(&format!("app.{n}")), exact_table());
            held.data_securable
                .insert(table(&format!("app.{n}")), table(&format!("app.{n}")));
        }
        assert_eq!(missing(&held).len(), 3, "{:?}", missing(&held));

        held.schemas
            .get_mut("app")
            .expect("the managed schema is in the map")
            .extend(dml());
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
    }

    /// `ensure` never emits a `DELETE` — that is the promise the mode makes to
    /// a table the application also writes to. Demanding it anyway would ask a
    /// DBA for row-removal rights on the very table the mode was chosen to
    /// keep pbps out of, which is the over-demand this list refuses.
    #[test]
    fn an_ensure_table_is_not_asked_for_delete() {
        let mut held = everything_but_the_dml(&["app"]);
        held.data_tables.insert(table("app.t"), ensure_table());
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(table("app.t"), read());
        let mut named: Vec<&str> = missing(&held).iter().map(|g| g.permission).collect();
        named.sort_unstable();
        assert_eq!(named, ["INSERT", "UPDATE"], "{:?}", missing(&held));
    }

    /// `mode: exact` with no declared row means "this table must be empty":
    /// every surviving row is a `DELETE`, and there is nothing to write. The
    /// mirror of `ensure` with no rows, which demands nothing at all and is
    /// therefore never handed to `missing` in the first place.
    #[test]
    fn an_empty_exact_table_is_asked_for_delete_and_nothing_else() {
        let mut held = everything_but_the_dml(&["app"]);
        held.data_tables.insert(
            table("app.t"),
            demand(pbps_model::DataMode::Exact, &[], &["label"]),
        );
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(table("app.t"), read());
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "DELETE");
        assert_eq!(gaps[0].securable(), "OBJECT::[app].[t]");
    }

    /// The count that runs *before* that `DELETE` needs database-wide `VIEW
    /// DEFINITION`, and holding it on every managed schema is not the same
    /// thing: a security policy can sit in a schema this project does not
    /// manage, and `counting_statement` refuses until it can prove the policy
    /// catalog readable (DECISIONS 468, 505). An account holding the rest of
    /// this list passed readiness and met that refusal on its first exact
    /// delete.
    #[test]
    fn a_declaration_that_removes_rows_is_asked_for_the_database_catalog_read() {
        let mut held = everything(&["app"]);
        held.database.remove("VIEW DEFINITION");
        held.data_tables.insert(
            table("app.t"),
            demand(pbps_model::DataMode::Exact, &["a"], &[]),
        );
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(
            table("app.t"),
            ["DELETE".to_owned(), "INSERT".to_owned()]
                .into_iter()
                .collect(),
        );

        let gaps = missing(&held);
        let asked: Vec<_> = gaps
            .iter()
            .filter(|g| g.permission == "VIEW DEFINITION" && g.securable() == "the database")
            .collect();
        assert_eq!(asked.len(), 1, "{gaps:?}");
        assert!(asked[0].why.contains("FILTER predicate"), "{:?}", asked[0]);
    }

    /// And of no other project. `ensure` never emits a `DELETE`, so the count
    /// never runs and database-wide `VIEW DEFINITION` — a broad ask — is not
    /// demanded. The over-demand this whole enum exists to avoid, and the
    /// reason the requirement is gated on the declarations rather than asked
    /// unconditionally.
    #[test]
    fn a_declaration_that_removes_no_rows_is_not_asked_for_the_database_catalog_read() {
        for demanded in [
            Some(demand(pbps_model::DataMode::Ensure, &["a"], &["label"])),
            None,
        ] {
            let mut held = everything(&["app"]);
            held.database.remove("VIEW DEFINITION");
            if let Some(d) = demanded {
                held.data_tables.insert(table("app.t"), d);
                held.data_securable.insert(table("app.t"), table("app.t"));
                held.data_objects.insert(
                    table("app.t"),
                    ["INSERT".to_owned(), "UPDATE".to_owned()]
                        .into_iter()
                        .collect(),
                );
            }
            let gaps = missing(&held);
            assert!(
                !gaps
                    .iter()
                    .any(|g| g.permission == "VIEW DEFINITION" && g.securable() == "the database"),
                "{gaps:?}"
            );
        }
    }

    /// The database grant can be held in full and the count still refuse: an
    /// effective object or schema metadata `DENY` beats it (DECISIONS 460), and
    /// the remedy is on that securable rather than on the database. Reported
    /// where it is, so an operator is not sent to grant something they already
    /// hold.
    #[test]
    fn an_effective_metadata_denial_is_reported_on_the_securable_that_carries_it() {
        let mut held = everything(&["app"]);
        held.data_tables.insert(
            table("app.t"),
            demand(pbps_model::DataMode::Exact, &["a"], &[]),
        );
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(
            table("app.t"),
            ["DELETE".to_owned(), "INSERT".to_owned()]
                .into_iter()
                .collect(),
        );
        // The database permission is held; only the denials are the problem.
        assert!(held.database.contains("VIEW DEFINITION"));
        held.metadata_denials
            .insert(Securable::Object(table("sec.pol")));
        held.metadata_denials
            .insert(Securable::Schema("sec".to_owned()));
        // The case the engine actually produces for an object: the `DENY`
        // being reported is what hides the name, so the id is all there is.
        held.metadata_denials.insert(Securable::Unreadable {
            class: "OBJECT",
            id: 1_221_579_390,
        });

        let gaps = missing(&held);
        let where_denied: Vec<String> = gaps
            .iter()
            .filter(|g| g.permission == "VIEW DEFINITION")
            .map(Gap::securable)
            .collect();
        assert_eq!(
            where_denied,
            // `Securable`'s own order: the schema, the object in it, then
            // the one that could not be named.
            vec![
                "SCHEMA::[sec]".to_owned(),
                "OBJECT::[sec].[pol]".to_owned(),
                "OBJECT::<unnameable: id 1221579390>".to_owned(),
            ],
            "{gaps:?}"
        );
        assert!(
            !where_denied.contains(&"the database".to_owned()),
            "{gaps:?}"
        );

        // The negative case beside it: no denial, nothing said.
        held.metadata_denials.clear();
        assert!(
            !missing(&held)
                .iter()
                .any(|g| g.permission == "VIEW DEFINITION"),
            "a project with no denial is told nothing about the policy catalog"
        );
    }

    /// An enumeration table whose only column is its code — the commonest
    /// reference-data shape there is — inserts and deletes and can never
    /// update: the differ builds an `UPDATE` only from the columns a row can
    /// hold a value in, and emits it only if that is not empty. Demanding
    /// `UPDATE` of it reports a gap against an account that can run every
    /// statement the declaration can produce.
    #[test]
    fn a_key_only_table_is_not_asked_for_update() {
        let mut held = everything_but_the_dml(&["app"]);
        held.data_tables.insert(
            table("app.t"),
            demand(pbps_model::DataMode::Exact, &["a"], &[]),
        );
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(table("app.t"), read());
        let mut named: Vec<&str> = missing(&held).iter().map(|g| g.permission).collect();
        named.sort_unstable();
        assert_eq!(named, ["DELETE", "INSERT"], "{:?}", missing(&held));
    }

    /// A non-key `IDENTITY` is the engine's: never written by a row and never
    /// read back, so it is not a cell that can differ. A table whose only
    /// non-key column is one can no more update than a key-only table can.
    #[test]
    fn a_table_whose_only_other_column_is_an_identity_is_not_asked_for_update() {
        let mut t = declared(pbps_model::DataMode::Exact, &["a"], &["seq"]);
        t.columns.get_mut("seq").expect("the extra column").identity =
            Some(pbps_model::schema::Identity {
                seed: 1,
                increment: 1,
            });
        let mut held = everything_but_the_dml(&["app"]);
        held.data_tables.insert(
            table("app.t"),
            DataDemand::of(&t).expect("it still inserts"),
        );
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(table("app.t"), read());
        let mut named: Vec<&str> = missing(&held).iter().map(|g| g.permission).collect();
        named.sort_unstable();
        assert_eq!(named, ["DELETE", "INSERT"], "{:?}", missing(&held));
    }

    /// The demand is per table, not per estate: a table declaring nothing is
    /// asked for none of the three even when the table beside it declares
    /// rows, which is the whole point of making this depend on the
    /// declarations.
    #[test]
    fn the_dml_demanded_is_per_table_and_not_estate_wide() {
        let mut held = everything_but_the_dml(&["app", "ref"]);
        held.data_tables.insert(table("app.seeded"), exact_table());
        held.data_securable
            .insert(table("app.seeded"), table("app.seeded"));
        held.data_objects.insert(table("app.seeded"), read());
        held.data_objects.insert(table("app.plain"), read());
        held.data_tables.insert(table("ref.lookup"), ensure_table());
        held.data_securable
            .insert(table("ref.lookup"), table("ref.lookup"));
        held.data_objects.insert(table("ref.lookup"), read());
        let mut named: Vec<String> = missing(&held)
            .iter()
            .map(|g| format!("{} on {}", g.permission, g.securable()))
            .collect();
        named.sort();
        assert_eq!(
            named,
            [
                "DELETE on OBJECT::[app].[seeded]",
                "INSERT on OBJECT::[app].[seeded]",
                "INSERT on OBJECT::[ref].[lookup]",
                "UPDATE on OBJECT::[app].[seeded]",
                "UPDATE on OBJECT::[ref].[lookup]",
            ],
            "{:?}",
            missing(&held)
        );
    }

    /// Two declared tables that resolve to the same physical name — the
    /// collision `resolve_for_query` exists for — must keep two distinct
    /// demands in `Held::data_tables`, keyed by the declared name. Keying by
    /// the resolved name instead, as an earlier version of this fix did,
    /// silently dropped one of the two on `.collect()` into the map (issue
    /// #133 round 2).
    #[test]
    fn a_table_whose_resolution_collides_with_anothers_keeps_its_own_demand() {
        let mut held = everything_but_the_dml(&["app"]);
        // `app.new` resolved safely to the object this environment still has
        // under `app.old`, and that object is fully granted.
        held.data_tables.insert(table("app.new"), exact_table());
        held.data_securable
            .insert(table("app.new"), table("app.old"));
        held.data_objects.insert(table("app.old"), dml_and_read());
        // `app.old` is declared too — the new table a plan can declare under
        // the name the rename above just freed. Its own resolution collided
        // with `app.new`'s confirmed claim on `app.old`, so `permissions`
        // left it out of `data_securable` entirely: no object to ask about,
        // so it falls back to the schema, which this helper strips of DML.
        held.data_tables.insert(table("app.old"), ensure_table());

        let mut named: Vec<String> = missing(&held)
            .iter()
            .map(|g| format!("{} on {}", g.permission, g.securable()))
            .collect();
        named.sort();
        assert_eq!(
            named,
            // `app.new`'s own demand (INSERT, UPDATE, DELETE) is fully
            // satisfied by the object grant its resolved name reads —
            // proof the entry under the *declared* key `app.new` was not
            // collapsed into `app.old`'s. `app.old`'s own demand (INSERT,
            // UPDATE — `ensure_table` never demands DELETE) could not be
            // asked about at object scope at all, so it falls back to the
            // only securable left safe to ask it at.
            ["INSERT on SCHEMA::[app]", "UPDATE on SCHEMA::[app]"],
            "{named:?}"
        );
    }

    /// The same collision, reachable through `grant_targets()` instead of
    /// `data_tables()`: a declared grant target whose resolution collides
    /// with another identity's confirmed claim has no schema-scope fallback,
    /// so it must be reported as an unconditional gap rather than silently
    /// dropped or, worse, answered by the wrong object's permissions (issue
    /// #133 round 2).
    #[test]
    fn a_granted_target_that_cannot_be_safely_resolved_is_an_unconditional_gap() {
        let mut held = everything(&["app"]);
        held.roles_declared = true;
        held.database.insert("CREATE ROLE".into());
        held.database.insert("ALTER ANY ROLE".into());
        // The confirmed claim: some other declared target already resolved
        // safely to `app.old` and is fully granted there. `CONTROL` is the
        // only permission `Needed::Granted` asks for.
        held.granted_objects.insert(
            table("app.old"),
            ["CONTROL".to_owned()].into_iter().collect(),
        );
        // The target this test is about could not be resolved without
        // colliding with that claim, so `permissions` put it here instead of
        // asking under `app.old` and risking the other identity's answer.
        held.granted_unresolvable.insert(table("app.new"));

        let gaps = missing(&held);
        let named: Vec<String> = gaps
            .iter()
            .filter(|g| g.securable() == "OBJECT::[app].[new]")
            .map(|g| g.permission.to_owned())
            .collect();
        assert!(
            !named.is_empty(),
            "an unresolvable grant target must be reported missing, never \
             silently read as ready: {gaps:?}"
        );
        assert!(
            !gaps.iter().any(|g| g.securable() == "OBJECT::[app].[old]"),
            "the confirmed claim's own object is fully granted and must not \
             appear as a gap: {gaps:?}"
        );
    }

    /// A declared schema the database does not have produced no row from
    /// `sys.schemas`, so nothing was asked about it and nothing can be said —
    /// it is reported by `absent_schemas` instead. Inventing a gap there would
    /// name a securable no grant can reach yet.
    #[test]
    fn a_data_table_whose_schema_is_absent_is_reported_absent_and_not_as_a_gap() {
        let mut held = everything_but_the_dml(&["app"]);
        held.schemas.remove("app");
        held.absent_schemas.insert("app".to_owned());
        held.data_tables.insert(table("app.t"), exact_table());
        held.data_securable.insert(table("app.t"), table("app.t"));
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
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
            .insert("app.customer".parse().unwrap(), BTreeSet::new());
        held.granted_schemas
            .insert("app".into(), ["CONTROL".to_owned()].into_iter().collect());
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "CONTROL");
        assert_eq!(
            gaps[0].securable,
            Securable::Object("app.customer".parse().unwrap())
        );
        held.granted_objects.insert(
            "app.customer".parse().unwrap(),
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
        let [state, lock] = ledger_tables();
        held.ledger_objects = [(state, ledger.clone()), (lock, ledger)]
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
        assert_eq!(gaps[0].securable(), "SCHEMA::[app]");
        assert!(!gaps[0].why.is_empty());
    }

    /// That label is pasted into a `GRANT`, so it carries the emitter's
    /// quoting, part by part. Bare, the dotted name below reads as a table
    /// `b` in a schema `dbo.a` — a securable the operator would then grant on
    /// instead — and the second one ends the identifier at its `]`.
    ///
    /// Built with `ObjectName::new` rather than parsed: `FromStr` splits on
    /// every dot and refuses a name holding one, so today the label is the
    /// only place such a name could arrive from (a catalog read, a state
    /// snapshot). The spelling has to be right when it does.
    #[test]
    fn an_irregular_object_name_is_quoted_part_by_part() {
        let dotted = ObjectName::new("dbo", "a.b");
        assert_eq!(
            Securable::Object(dotted.clone()).to_string(),
            "OBJECT::[dbo].[a.b]"
        );
        assert_eq!(
            Securable::Object(ObjectName::new("dbo", "x]y")).to_string(),
            "OBJECT::[dbo].[x]]y]"
        );
        // The emitter's own spelling of the same object, which is the point:
        // the label names the securable the `GRANT` would land on, or it is
        // worse than no label at all.
        assert_eq!(
            Securable::Object(dotted.clone()).to_string(),
            format!("OBJECT::{}", crate::emit::qualified(&dotted).unwrap())
        );
    }

    /// A schema name needs no dot to need quoting: `my-schema` is a
    /// subtraction bare, and `[my-schema]` is a name.
    #[test]
    fn an_irregular_schema_name_is_bracket_quoted() {
        assert_eq!(
            Securable::Schema("my-schema".to_owned()).to_string(),
            "SCHEMA::[my-schema]"
        );
        assert_eq!(
            Securable::Schema("dbo".to_owned()).to_string(),
            "SCHEMA::[dbo]"
        );
    }

    /// Negative: the three names the server itself refuses have no `GRANT`
    /// spelling at all. The label names them and says so, rather than putting
    /// brackets round them and reading as a statement that would run.
    #[test]
    fn a_name_no_statement_can_carry_is_not_offered_as_one() {
        assert_eq!(
            Securable::Schema(String::new()).to_string(),
            "SCHEMA::<unquotable: >"
        );
        assert_eq!(
            Securable::Object(ObjectName::new("dbo", "a\0b")).to_string(),
            "OBJECT::[dbo].<unquotable: a\0b>"
        );
        let long = "x".repeat(crate::ident::MAX_IDENT_CHARS + 1);
        let label = Securable::Object(ObjectName::new("dbo", long.clone())).to_string();
        assert_eq!(label, format!("OBJECT::[dbo].<unquotable: {long}>"));
        // Not bracket-quoted anywhere in the part: the reader must not be able
        // to paste it and be told by the server that it worked.
        assert!(!label.ends_with(']'), "{label}");
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
        assert_eq!(gaps[0].securable(), "SCHEMA::[dbo]");
    }

    /// The ledger and the lock live in `dbo`. Demanding INSERT and DELETE on an
    /// application schema *for their sake* would send an organization to grant
    /// a write permission the deployment never uses there — and this list only
    /// stays credible if every entry on it is really needed.
    ///
    /// A `data:` block does make the deployment write rows in an application
    /// schema, and that is asked for separately (`Needed::DataInsert`) and only of a
    /// project that declares one; this holds nothing back from that. The
    /// project here declares no row, which is what keeps the demand at nil.
    #[test]
    fn the_ledger_writes_are_not_demanded_on_an_application_schema() {
        let mut held = everything(&["dbo", "app"]);
        assert!(held.data_tables.is_empty(), "no row is declared here");
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
        assert_eq!(gaps[0].securable(), "SCHEMA::[dbo]");
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
        assert_eq!(gaps[0].securable(), "SCHEMA::[dbo]");
        assert!(gaps[0].why.contains("first use"), "{gaps:?}");
    }

    /// One surviving ledger table is not a created ledger. `ensure_tables`
    /// recreates whichever is missing, so reading `ledger_objects` as a single
    /// yes/no let an account with the lock table but not the state table pass
    /// readiness and then fail on its next `record`.
    #[test]
    fn a_half_present_ledger_still_needs_the_creation_permission() {
        let mut held = ledger_granted_on_the_objects_only(&["app"]);
        held.ledger_objects.remove(&ledger_tables()[0]);
        assert_eq!(held.ledger_objects.len(), 1, "exactly one survives");
        held.ledger_schema.clear();

        let gaps = missing(&held);
        assert!(
            gaps.iter()
                .any(|g| g.permission == "ALTER" && g.securable() == "SCHEMA::[dbo]"),
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
        assert_eq!(gaps[0].securable(), "SCHEMA::[app]");
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
            .get_mut(&ledger_tables()[1])
            .unwrap()
            .remove("DELETE");
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "DELETE");
        assert_eq!(gaps[0].securable(), "OBJECT::[dbo].[__pbps_lock]");
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
        assert_eq!(gaps[0].securable(), "SCHEMA::[dbo]");
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
        held.ledger_objects.remove(&ledger_tables()[0]);
        // The creation permission is the other half of this shape and has its
        // own test; granted here so the gaps below are only the DML ones.
        held.ledger_schema.insert("ALTER".to_owned());

        let gaps = missing(&held);
        for permission in ["SELECT", "INSERT", "DELETE"] {
            assert!(
                gaps.iter()
                    .any(|g| g.permission == permission && g.securable() == "SCHEMA::[dbo]"),
                "{permission} on the table still to be created was not asked for: {gaps:?}"
            );
        }
        // And the surviving table is still answered where its grant actually
        // sits, or the same account would be told to re-grant what it holds.
        assert!(
            !gaps
                .iter()
                .any(|g| g.securable() == "OBJECT::[dbo].[__pbps_lock]"),
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
        assert_eq!(gaps[0].securable(), "SCHEMA::[dbo]");
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
        assert_eq!(gaps[0].securable(), "SCHEMA::[app]");
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
            managed_tables: BTreeMap::new(),
            absent_schemas: BTreeSet::new(),
            ledger_schema: BTreeSet::new(),
            ledger_objects: BTreeMap::new(),
            ledger_migration_needed: false,
            referenced_objects: BTreeMap::new(),
            delete_children: BTreeMap::new(),
            roles_declared: false,
            metadata_denials: BTreeSet::new(),
            granted_objects: BTreeMap::new(),
            granted_unresolvable: BTreeSet::new(),
            granted_schemas: BTreeMap::new(),
            data_tables: DataTables::new(),
            data_securable: BTreeMap::new(),
            data_objects: BTreeMap::new(),
        };
        // Not the ones that depend on what the project declares: no foreign
        // key out of the managed schemas, no role, and no declared row means
        // none of those are asked about at all — including the two halves of
        // the delete count's catalog proof, which no row removal demands here,
        // the read-back that closes an apply, and the children that count
        // would read.
        let applicable = REQUIRED
            .iter()
            .filter(|r| {
                !matches!(
                    r.needed,
                    Needed::Referenced
                        | Needed::ManagedTable
                        | Needed::LedgerMigration
                        | Needed::RoleAdmin
                        | Needed::Granted
                        | Needed::DataInsert
                        | Needed::DataUpdate
                        | Needed::DataDelete
                        | Needed::DataRead
                        | Needed::DeleteChild
                        | Needed::DeleteCatalog
                        | Needed::DeleteCatalogDenied
                )
            })
            .count();
        assert_eq!(missing(&held).len(), applicable);
    }

    /// A table that moves between schemas and carries **no rows** is not asked
    /// about its destination at all. The probes' `SELECT` reads it before the
    /// transfer — `preflight` runs before `execute_statements` — and the only
    /// read that happens afterwards is the row read-back, which is scoped to
    /// the plan's data tables. Demanding the destination of every moving table
    /// refused a deployment that would have run (issue #517, round 5).
    #[test]
    fn a_cross_schema_move_without_rows_is_not_asked_about_its_destination() {
        let declared = table("dest.old_name");
        let recorded = table("app.old_name");
        let resolved = [(declared.clone(), recorded.clone())].into_iter().collect();
        // The source object is readable; the destination schema is not, and
        // nothing about this table is read there.
        let objects = [(recorded.clone(), read())].into_iter().collect();
        let schemas = [
            ("app".to_owned(), read()),
            ("dest".to_owned(), BTreeSet::new()),
        ]
        .into_iter()
        .collect();

        let mut held = everything(&["app", "dest"]);
        held.managed_tables = managed_table_rights(
            &[declared],
            &resolved,
            std::iter::empty(),
            &objects,
            &schemas,
        );
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
        assert_eq!(
            held.managed_tables.keys().collect::<Vec<_>>(),
            [&Securable::Object(recorded)],
            "one securable, the source: {:?}",
            held.managed_tables
        );
    }

    /// And the move takes the table's **writes** with it, not only its read.
    /// The transfer drops every permission on the object, so a report that
    /// demanded the destination for `SELECT` alone would go green once that
    /// was granted and the first row would still fail (issue #517, round 3).
    #[test]
    fn a_cross_schema_move_demands_the_destinations_dml_as_well_as_its_read() {
        let declared = table("dest.t");
        let recorded = table("app.t");
        let mut held = everything(&["app", "dest"]);
        held.data_tables.insert(declared.clone(), exact_table());
        held.data_securable
            .insert(declared.clone(), recorded.clone());
        // Everything on the source object, which is where a grant issued
        // before the move sits, and nothing on the destination schema.
        held.data_objects.insert(recorded, dml_and_read());
        // The destination keeps what a deployer needs to *create* there —
        // `ALTER`, `REFERENCES`, `VIEW DEFINITION` — and holds none of the
        // rights the rows need, which is the shape the operator is left in
        // after granting only what the schema questions asked for.
        held.schemas
            .get_mut("dest")
            .expect("the managed schema is in the map")
            .retain(|p| !dml_and_read().contains(p));

        let mut named: Vec<String> = missing(&held)
            .iter()
            .map(|g| format!("{} on {}", g.permission, g.securable()))
            .collect();
        named.sort();
        named.dedup();
        assert_eq!(
            named,
            [
                "DELETE on SCHEMA::[dest]",
                "INSERT on SCHEMA::[dest]",
                "SELECT on SCHEMA::[dest]",
                "UPDATE on SCHEMA::[dest]",
            ],
            "every data demand is asked at the destination: {:?}",
            missing(&held)
        );

        // Granted there, and the report is clean — the source answer still
        // carries the statements that run before the transfer.
        let full = everything(&["dest"]).schemas["dest"].clone();
        held.schemas.insert("dest".to_owned(), full);
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
    }

    /// A data table that is not moving is asked at one securable, as before:
    /// the second question exists for the move and must not fire without one.
    #[test]
    fn a_data_table_staying_put_is_not_asked_about_a_second_schema() {
        let name = table("app.t");
        let mut held = everything(&["app"]);
        held.data_tables.insert(name.clone(), exact_table());
        held.data_securable.insert(name.clone(), name.clone());
        held.data_objects.insert(name, BTreeSet::new());
        assert!(
            !missing(&held)
                .iter()
                .any(|g| g.securable().starts_with("SCHEMA::")),
            "{:?}",
            missing(&held)
        );
    }

    /// An ordinary rename stays one securable: the name moves, the schema does
    /// not, and the object the grant sits on is the one that answers.
    #[test]
    fn a_same_schema_rename_is_still_asked_at_one_securable() {
        let declared = table("app.renamed");
        let recorded = table("app.old");
        let resolved = [(declared.clone(), recorded.clone())].into_iter().collect();
        let objects = [(recorded.clone(), read())].into_iter().collect();
        let schemas = [("app".to_owned(), BTreeSet::new())].into_iter().collect();

        let rights = managed_table_rights(
            &[declared],
            &resolved,
            std::iter::empty(),
            &objects,
            &schemas,
        );
        assert_eq!(
            rights.keys().collect::<Vec<_>>(),
            [&Securable::Object(recorded)],
            "{rights:?}"
        );
    }

    /// `apply` reads the managed rows back before it records, and the read
    /// projects the columns the **declaration** names — including one the same
    /// plan adds, which no catalog-sourced question can see. `SELECT` on the
    /// table is therefore asked for on its own, beside the DML (issue #516).
    #[test]
    fn a_table_declaring_rows_is_asked_for_the_read_that_closes_the_apply() {
        let mut held = everything(&["app"]);
        held.data_tables.insert(table("app.t"), exact_table());
        held.data_securable.insert(table("app.t"), table("app.t"));
        // Every DML, and no read: the shape a column-level `GRANT SELECT` on
        // the columns that exist today leaves once the declaration adds one.
        held.data_objects.insert(table("app.t"), dml());
        let gaps = missing(&held);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].permission, "SELECT");
        assert_eq!(gaps[0].securable(), "OBJECT::[app].[t]");
        assert!(gaps[0].why.contains("read-back"), "{gaps:?}");

        held.data_objects.insert(table("app.t"), dml_and_read());
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
    }

    /// And of every table that declares rows, not only the ones that remove
    /// them: `ensure` is read back too.
    #[test]
    fn an_ensure_table_is_asked_for_the_read_as_well() {
        let mut held = everything(&["app"]);
        held.data_tables.insert(table("app.t"), ensure_table());
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(table("app.t"), dml());
        let named: Vec<&str> = missing(&held).iter().map(|g| g.permission).collect();
        assert_eq!(named, ["SELECT"], "{:?}", missing(&held));
    }

    /// The count before an `exact` delete reads every table with an enabled
    /// foreign key into the parent, and those children are found in the
    /// catalog: they need not be declared, or even live in a managed schema
    /// (issue #515).
    #[test]
    fn the_delete_count_asks_about_each_child_it_would_read() {
        let mut held = everything(&["app"]);
        held.data_tables.insert(table("app.t"), exact_table());
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(table("app.t"), dml_and_read());
        held.delete_children
            .insert(Securable::Object(table("app.unmanaged")), BTreeSet::new());
        held.delete_children
            .insert(Securable::Object(table("other.far")), read());

        let named: Vec<String> = missing(&held)
            .iter()
            .map(|g| format!("{} on {}", g.permission, g.securable()))
            .collect();
        assert_eq!(named, ["SELECT on OBJECT::[app].[unmanaged]"], "{named:?}");
        // And nothing wider: the child's schema is somebody else's.
        assert!(
            !missing(&held)
                .iter()
                .any(|g| g.securable().starts_with("SCHEMA::")),
            "{:?}",
            missing(&held)
        );
    }

    /// A managed child that this plan moves between schemas is the one case
    /// the dedupe must not simply drop. The guard a row delete carries reads
    /// the surviving children inside the delete's own transaction, and a table
    /// rename is `order_key` 1 while a row delete is 12 — so the transfer has
    /// already run and taken the object's permissions with it. The managed
    /// question answers for the source, and a child with no `data:` block has
    /// nothing in `data_gaps` to answer for its destination (issue #515,
    /// round 6).
    #[test]
    fn a_delete_count_child_that_moves_is_asked_at_its_destination() {
        let mut held = everything(&["app", "dest"]);
        held.data_tables.insert(table("app.t"), exact_table());
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(table("app.t"), dml_and_read());
        // The child is managed, so it is not asked about at its object here;
        // it is moving, so its destination schema is, and that schema holds
        // nothing the count needs.
        held.delete_children
            .insert(Securable::Schema("dest".to_owned()), BTreeSet::new());

        let named: Vec<String> = missing(&held)
            .iter()
            .map(|g| format!("{} on {}", g.permission, g.securable()))
            .collect();
        assert_eq!(named, ["SELECT on SCHEMA::[dest]"], "{named:?}");

        // Granted there, and nothing is reported: the source answer already
        // carries every read that happens before the transfer.
        held.delete_children
            .insert(Securable::Schema("dest".to_owned()), read());
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
    }

    /// A project that removes no row runs no count, so nothing is discovered
    /// and nothing is asked for — the same rule the delete's own catalog proof
    /// follows.
    #[test]
    fn a_project_that_removes_no_row_is_asked_about_no_child() {
        let mut held = everything(&["app"]);
        held.data_tables.insert(table("app.t"), ensure_table());
        held.data_securable.insert(table("app.t"), table("app.t"));
        held.data_objects.insert(table("app.t"), dml_and_read());
        assert!(
            held.delete_children.is_empty(),
            "an `ensure` declaration discovers no child"
        );
        assert!(missing(&held).is_empty(), "{:?}", missing(&held));
    }

    /// The discovery statement names each parent as two bound parts, takes
    /// only the enabled constraints, and asks about the *referencing* table.
    #[test]
    fn the_child_discovery_asks_the_catalog_the_probes_own_question() {
        let parents = [table("app.t"), ObjectName::new("dbo", "a.b")];
        let (sql, params) = delete_children_sql(&parents);
        assert!(
            sql.contains("QUOTENAME(p.s) + N'.' + QUOTENAME(p.n)"),
            "the securable is assembled by the server: {sql}"
        );
        assert!(
            sql.contains("fk.is_disabled = 0"),
            "a `NOCHECK`ed constraint is not enforced and its child is not \
             counted (DECISIONS 144): {sql}"
        );
        assert!(
            sql.contains("ct.object_id = fk.parent_object_id"),
            "the child is the referencing table, not the referenced one: {sql}"
        );
        // And the columns the count compares, so the demand is the probe's
        // width rather than the child's whole catalog.
        assert!(
            sql.contains("fkc.constraint_object_id = fk.object_id")
                && sql.contains("cc.column_id = fkc.parent_column_id"),
            "the child's own foreign-key columns come back with it: {sql}"
        );
        assert_eq!(params.len(), 4, "two bound parts per parent");
    }

    /// A foreign key into a schema this project does not manage. `REFERENCES`
    /// is authorized on the referenced table and the pre-flight probe reads it,
    /// and neither is covered by anything asked about the managed schemas — so
    /// a login could pass readiness and fail during `apply`.
    #[test]
    fn a_foreign_key_out_of_the_managed_schemas_is_asked_about_its_target() {
        let mut held = everything(&["app"]);
        held.referenced_objects
            .insert("shared.parent".parse().unwrap(), BTreeSet::new());

        let gaps = missing(&held);
        for permission in ["REFERENCES", "SELECT"] {
            assert!(
                gaps.iter()
                    .any(|g| g.permission == permission
                        && g.securable() == "OBJECT::[shared].[parent]"),
                "{permission} on the referenced table was not asked for: {gaps:?}"
            );
        }
        // And nothing wider: demanding anything on the whole of somebody else's
        // schema is the over-demand this check exists to avoid.
        assert!(
            !gaps.iter().any(|g| g.securable() == "SCHEMA::[shared]"),
            "{gaps:?}"
        );
    }

    /// Held on the target, so nothing is reported — the ordinary case for a
    /// deployment account a DBA has granted correctly.
    #[test]
    fn a_granted_foreign_key_target_reports_nothing() {
        let mut held = everything(&["app"]);
        held.referenced_objects.insert(
            "shared.parent".parse().unwrap(),
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
