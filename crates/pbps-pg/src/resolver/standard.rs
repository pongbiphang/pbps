//! The standard state of a scratch database (#1708, DEC-1708.1).
//!
//! A supplied scratch database is reused from one run to the next, so the
//! state a run leaves is the state the next one compiles in. Cleanup used to
//! be defined by its mechanism, `DROP OWNED`, plus one check per change a
//! review found it missed. It is defined here by the state instead: the
//! scratch is put into this state at the start of a run, which repairs what
//! an interrupted run left, and put back and re-read at release.
//!
//! The state is finite because a persistent change needs ownership of the
//! changed object, or a grant option on it. The database owner acts as owner
//! of exactly four things in its database: the objects it creates, which
//! `DROP OWNED` removes and the inventory proves gone; initdb's `public`,
//! through `pg_database_owner`, the only low-OID object of any owner-bearing
//! catalog not owned by the bootstrap superuser (measured on 16 and 18, and
//! pinned by a live test against every supported server); the database
//! itself; and its own role, whose defaults the run compares. The last two
//! halves are this module: `public` and the database's own properties.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::transport::{ExecuteConnection, QueryConnection};
use pbps_db::{DbError, Row};

use super::authorization::setting_literal as literal;
use super::authorization::{LIST_QUOTE_SETTINGS, setting_value};

/// initdb's `public` has this OID in every database it creates, which keys
/// it through a rename.
const PUBLIC_OID: u32 = 2200;

/// The role that owns `public` in a database created from `template0`.
const PUBLIC_OWNER: &str = "pg_database_owner";

/// The comment initdb gives `public`, read back the same on 16 and 18. It
/// comes from the catalog data initdb loads, which no query of another
/// database can read, so it is spelled here and a live test compares it
/// with a fresh database on every supported server.
pub const PUBLIC_COMMENT: &str = "standard public schema";

/// The written spelling of the role `PUBLIC` as a grantee.
const PUBLIC_GRANTEE: &str = "PUBLIC";

/// One entry of an ACL, as `aclexplode` reads it with the defaults filled
/// in: equal ACLs compare equal however they are spelled.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entry {
    /// A role name, or `PUBLIC`.
    pub grantee: String,
    pub grantor: String,
    pub privilege: String,
    pub grantable: bool,
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} to {}{} granted by {}",
            self.privilege,
            self.grantee,
            if self.grantable {
                " with grant option"
            } else {
                ""
            },
            self.grantor
        )
    }
}

/// The state a scratch database is put into: initdb's, unless the project
/// declared otherwise on its resolver entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standard {
    /// `ALTER DATABASE ... SET`, name to value as the engine stores it. The
    /// name is lowercase: the engine stores a setting under its own spelling
    /// (`TimeZone`) whatever the statement wrote, and matches names
    /// case-insensitively.
    pub settings: BTreeMap<String, String>,
    pub comment: Option<String>,
    /// `-1` for no limit.
    pub connection_limit: i32,
    pub public_comment: Option<String>,
    /// `(role, privilege)` granted on `public` by its owner, beyond initdb's.
    pub public_grants: BTreeSet<(String, String)>,
}

impl Default for Standard {
    /// What `CREATE DATABASE ... TEMPLATE template0` produces.
    fn default() -> Self {
        Self {
            settings: BTreeMap::new(),
            comment: None,
            connection_limit: -1,
            public_comment: Some(PUBLIC_COMMENT.to_owned()),
            public_grants: BTreeSet::new(),
        }
    }
}

impl Standard {
    /// `public`'s ACL: initdb's, which has its owner hold `USAGE` and
    /// `CREATE` and `PUBLIC` hold `USAGE`, plus the declared grants, all
    /// made by the owner.
    fn public_acl(&self) -> BTreeSet<Entry> {
        let entry = |grantee: &str, privilege: &str| Entry {
            grantee: grantee.to_owned(),
            grantor: PUBLIC_OWNER.to_owned(),
            privilege: privilege.to_owned(),
            grantable: false,
        };
        [
            entry(PUBLIC_OWNER, "USAGE"),
            entry(PUBLIC_OWNER, "CREATE"),
            entry(PUBLIC_GRANTEE, "USAGE"),
        ]
        .into_iter()
        .chain(
            self.public_grants
                .iter()
                .map(|(role, privilege)| entry(role, privilege)),
        )
        .collect()
    }
}

/// initdb's `public`, found by its OID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Public {
    pub name: String,
    pub owner: String,
    pub acl: BTreeSet<Entry>,
    pub comment: Option<String>,
}

/// What the standard covers, as the connected database holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub database: String,
    pub owner: String,
    /// `None` once `public` was dropped: nothing the run can put back.
    pub public: Option<Public>,
    pub connection_limit: i32,
    pub template: bool,
    pub comment: Option<String>,
    /// The database's own settings, for every role, keyed by lowercase name
    /// as [`Standard::settings`] is.
    pub settings: BTreeMap<String, String>,
    /// The session login's settings in this database.
    pub login_settings: BTreeSet<String>,
    /// The database's ACL. Kept as a snapshot rather than standardized:
    /// operators harden it, and an ACL is awkward to declare.
    pub acl: BTreeSet<Entry>,
}

fn text(row: &Row, field: &str) -> Result<String, DbError> {
    row.try_get::<&str>(field)?
        .map(str::to_owned)
        .ok_or_else(|| DbError::BadRow(format!("{field} was null")))
}

fn optional(row: &Row, field: &str) -> Result<Option<String>, DbError> {
    Ok(row.try_get::<&str>(field)?.map(str::to_owned))
}

fn one(rows: Vec<Row>, what: &str) -> Result<Row, DbError> {
    let mut rows = rows.into_iter();
    match (rows.next(), rows.next()) {
        (Some(row), None) => Ok(row),
        _ => Err(DbError::BadRow(format!("{what} did not read as one row"))),
    }
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The grantee as a statement names it.
fn grantee(name: &str) -> String {
    if name == PUBLIC_GRANTEE {
        PUBLIC_GRANTEE.to_owned()
    } else {
        ident(name)
    }
}

/// `name=value`, as `pg_db_role_setting` stores a setting, split at the
/// first `=`: a name holds none.
fn setting(entry: &str) -> (String, String) {
    match entry.split_once('=') {
        Some((name, value)) => (name.to_owned(), value.to_owned()),
        None => (entry.to_owned(), String::new()),
    }
}

async fn acl(conn: &mut impl QueryConnection, from: &str) -> Result<BTreeSet<Entry>, DbError> {
    conn.query(&format!(
        "SELECT CASE WHEN x.grantee = 0 THEN 'PUBLIC' \
                     ELSE pg_catalog.pg_get_userbyid(x.grantee)::text END AS grantee, \
                pg_catalog.pg_get_userbyid(x.grantor)::text AS grantor, \
                x.privilege_type::text AS privilege, \
                x.is_grantable::text AS grantable \
           FROM {from}"
    ))
    .await?
    .iter()
    .map(|row| {
        Ok(Entry {
            grantee: text(row, "grantee")?,
            grantor: text(row, "grantor")?,
            privilege: text(row, "privilege")?,
            grantable: text(row, "grantable")? == "true",
        })
    })
    .collect()
}

/// Reads what the standard covers in the connected database.
pub async fn read_state(conn: &mut impl QueryConnection) -> Result<State, DbError> {
    let row = one(
        conn.query(
            "SELECT d.datname::text AS database, \
                    pg_catalog.pg_get_userbyid(d.datdba)::text AS owner, \
                    d.datconnlimit::text AS connection_limit, \
                    d.datistemplate::text AS template, \
                    pg_catalog.shobj_description(d.oid, 'pg_database') AS comment \
               FROM pg_catalog.pg_database d \
              WHERE d.datname = pg_catalog.current_database()",
        )
        .await?,
        "the scratch database",
    )?;
    let connection_limit = text(&row, "connection_limit")?
        .parse()
        .map_err(|_| DbError::BadRow("a connection limit was not a number".into()))?;
    let mut settings = BTreeMap::new();
    let mut login_settings = BTreeSet::new();
    for row in conn
        .query(
            "SELECT (s.setrole = 0)::text AS everyone, e.entry AS entry \
               FROM pg_catalog.pg_db_role_setting s \
               CROSS JOIN LATERAL pg_catalog.unnest(s.setconfig) AS e(entry) \
              WHERE s.setdatabase = (SELECT d.oid FROM pg_catalog.pg_database d \
                                      WHERE d.datname = pg_catalog.current_database()) \
                AND (s.setrole = 0 OR s.setrole = (SELECT r.oid FROM pg_catalog.pg_roles r \
                                                    WHERE r.rolname = session_user))",
        )
        .await?
    {
        let entry = text(&row, "entry")?;
        if text(&row, "everyone")? == "true" {
            let (name, value) = setting(&entry);
            settings.insert(name.to_lowercase(), value);
        } else {
            login_settings.insert(entry);
        }
    }
    let database_acl = acl(
        conn,
        "pg_catalog.pg_database d, \
         pg_catalog.aclexplode(COALESCE(d.datacl, pg_catalog.acldefault('d', d.datdba))) x \
         WHERE d.datname = pg_catalog.current_database()",
    )
    .await?;
    let public = conn
        .query(&format!(
            "SELECT n.nspname::text AS name, \
                    pg_catalog.pg_get_userbyid(n.nspowner)::text AS owner, \
                    pg_catalog.obj_description(n.oid, 'pg_namespace') AS comment \
               FROM pg_catalog.pg_namespace n WHERE n.oid = {PUBLIC_OID}"
        ))
        .await?;
    let public = match public.first() {
        None => None,
        Some(found) => Some(Public {
            name: text(found, "name")?,
            owner: text(found, "owner")?,
            comment: optional(found, "comment")?,
            acl: acl(
                conn,
                &format!(
                    "pg_catalog.pg_namespace n, \
                     pg_catalog.aclexplode(COALESCE(n.nspacl, \
                         pg_catalog.acldefault('n', n.nspowner))) x \
                     WHERE n.oid = {PUBLIC_OID}"
                ),
            )
            .await?,
        }),
    };
    Ok(State {
        database: text(&row, "database")?,
        owner: text(&row, "owner")?,
        public,
        connection_limit,
        template: text(&row, "template")? == "true",
        comment: optional(&row, "comment")?,
        settings,
        login_settings,
        acl: database_acl,
    })
}

/// Each way `state` is not `standard`, named. `acl` is the database ACL the
/// run started with, or `None` before it has one to keep.
pub fn differences(
    state: &State,
    standard: &Standard,
    acl: Option<&BTreeSet<Entry>>,
) -> Vec<String> {
    let mut found = Vec::new();
    match &state.public {
        None => found.push(format!("schema public (OID {PUBLIC_OID}) is missing")),
        Some(public) => {
            if public.name != "public" {
                found.push(format!("schema public is named {}", public.name));
            }
            if public.owner != PUBLIC_OWNER {
                found.push(format!("schema public is owned by {}", public.owner));
            }
            let expected = standard.public_acl();
            found.extend(
                public
                    .acl
                    .difference(&expected)
                    .map(|entry| format!("schema public grants {entry}")),
            );
            found.extend(
                expected
                    .difference(&public.acl)
                    .map(|entry| format!("schema public lacks {entry}")),
            );
            if public.comment != standard.public_comment {
                found.push(format!(
                    "schema public's comment is {}",
                    shown(public.comment.as_deref())
                ));
            }
        }
    }
    if state.connection_limit != standard.connection_limit {
        found.push(format!(
            "the connection limit is {} instead of {}",
            state.connection_limit, standard.connection_limit
        ));
    }
    if state.template {
        found.push("the database is a template".to_owned());
    }
    if state.comment != standard.comment {
        found.push(format!(
            "the database's comment is {}",
            shown(state.comment.as_deref())
        ));
    }
    for (name, value) in &state.settings {
        match standard.settings.get(name) {
            Some(expected) if same_value(name, value, expected) => {}
            _ => found.push(format!("the database sets {name}={value}")),
        }
    }
    for (name, value) in &standard.settings {
        if !state.settings.contains_key(name) {
            found.push(format!("the database does not set {name}={value}"));
        }
    }
    found.extend(
        state
            .login_settings
            .iter()
            .map(|entry| format!("the scratch account sets {entry} in this database")),
    );
    if let Some(kept) = acl {
        found.extend(
            state
                .acl
                .difference(kept)
                .map(|entry| format!("the database grants {entry}")),
        );
        found.extend(
            kept.difference(&state.acl)
                .map(|entry| format!("the database lacks {entry}")),
        );
    }
    found
}

fn shown(comment: Option<&str>) -> String {
    comment.map_or_else(|| "none".to_owned(), |comment| format!("{comment:?}"))
}

/// What the session cannot apply of a declared standard, named, before it
/// writes anything: a grant to a role that does not exist, and a setting it
/// may not store on its database. A setting already stored as declared
/// needs no statement and is not asked about.
pub async fn unappliable(
    conn: &mut impl QueryConnection,
    standard: &Standard,
) -> Result<Vec<String>, DbError> {
    let state = read_state(conn).await?;
    let mut found = Vec::new();
    let roles: BTreeSet<&String> = standard
        .public_grants
        .iter()
        .map(|(role, _)| role)
        // `PUBLIC` is the engine's pseudo-role, never a row of `pg_roles`.
        .filter(|role| role.as_str() != PUBLIC_GRANTEE)
        .collect();
    for role in roles {
        let rows = conn
            .query(&format!(
                "SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = {}",
                literal(role)
            ))
            .await?;
        if rows.is_empty() {
            found.push(format!(
                "the declared grant on schema public to {role}, a role that does not exist"
            ));
        }
    }
    for (name, value) in &standard.settings {
        if state
            .settings
            .get(name)
            .is_some_and(|stored| same_value(name, stored, value))
        {
            continue;
        }
        if setting_value(name, value).is_err() {
            found.push(format!(
                "the declared setting {name}={value}, which is not a list the engine can read"
            ));
            continue;
        }
        let rows = conn
            .query(&format!(
                "SELECT s.context::text AS context, \
                        ((SELECT r.rolsuper FROM pg_catalog.pg_roles r \
                           WHERE r.rolname = session_user) \
                         OR s.context IN ('user', 'backend') \
                         OR (s.context IN ('superuser', 'superuser-backend') \
                             AND pg_catalog.has_parameter_privilege(session_user, s.name, 'SET')) \
                        )::text AS may \
                   FROM pg_catalog.pg_settings s WHERE pg_catalog.lower(s.name) = {}",
                literal(name)
            ))
            .await?;
        match rows.first() {
            // A custom placeholder, such as `app.mode`: anyone may set it.
            None if name.contains('.') => {}
            None => found.push(format!(
                "the declared setting {name}, which this server does not know"
            )),
            Some(row) if text(row, "may")? != "true" => found.push(format!(
                "the declared setting {name}, which the scratch account may not set on its \
                 database ({} context)",
                text(row, "context")?
            )),
            Some(_) => {}
        }
    }
    Ok(found)
}

/// Puts the connected database into `standard`, and its ACL back to `acl`
/// when one is kept. Issues only the statements a difference needs, so a
/// database already standard is left untouched. Returns what it could not
/// put back, each with the engine's reason; the caller re-reads with
/// [`differences`], which is the verdict. `Err` is a failed read.
///
/// Every statement runs as the session's login, never under `SET ROLE`:
/// the database owner acts for `pg_database_owner`, which owns `public`,
/// while renaming a schema also needs `CREATE` on the database, which
/// `pg_database_owner` lacks (measured on 16 and 18). Each statement runs
/// on its own, so one the engine refuses does not undo the others.
pub async fn enforce(
    conn: &mut impl ExecuteConnection,
    standard: &Standard,
    acl: Option<&BTreeSet<Entry>>,
) -> Result<Vec<String>, DbError> {
    enforce_in(conn, standard, acl, false).await
}

/// Puts `standard` on inside a transaction that is always rolled back, and
/// reads the database back there: whatever the engine refuses (a value it
/// cannot parse, a privilege the login lacks) and whatever reads back
/// otherwise than declared is named, and nothing is written. The engine
/// judges each declared value, not a model of it (#1708 review). Each
/// statement runs under its own savepoint, so one refusal does not hide the
/// next. Empty when the declared state can be put on whole.
pub async fn trial(
    conn: &mut impl ExecuteConnection,
    standard: &Standard,
) -> Result<Vec<String>, DbError> {
    conn.execute("BEGIN").await?;
    let tried = match enforce_in(conn, standard, None, true).await {
        Ok(failed) => match verify(conn, standard, None).await {
            Ok(left) => Ok((failed, left)),
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    let ended = conn.execute("ROLLBACK").await;
    let (failed, left) = tried?;
    ended?;
    Ok(if failed.is_empty() { left } else { failed })
}

async fn enforce_in(
    conn: &mut impl ExecuteConnection,
    standard: &Standard,
    acl: Option<&BTreeSet<Entry>>,
    trial: bool,
) -> Result<Vec<String>, DbError> {
    let mut failed = Vec::new();
    let state = read_state(conn).await?;
    let database = ident(&state.database);
    if let Some(public) = &state.public {
        if public.name != "public" {
            run(
                conn,
                trial,
                &mut failed,
                format!("ALTER SCHEMA {} RENAME TO public", ident(&public.name)),
            )
            .await;
        }
        if public.owner != PUBLIC_OWNER {
            // Only the login's own ownership can be handed back: another
            // owner's is not the login's to change, and stays named.
            run(
                conn,
                trial,
                &mut failed,
                format!("ALTER SCHEMA public OWNER TO {PUBLIC_OWNER}"),
            )
            .await;
        }
    }
    // Re-read: an owner change rewrites the old owner's entries as the new
    // owner's.
    let state = read_state(conn).await?;
    if let Some(public) = &state.public
        && public.owner == PUBLIC_OWNER
    {
        let expected = standard.public_acl();
        failed.extend(
            put_acl(
                conn,
                trial,
                &public.acl,
                &expected,
                &format!("SCHEMA {}", ident(&public.name)),
                PUBLIC_OWNER,
            )
            .await,
        );
        if public.comment != standard.public_comment {
            run(
                conn,
                trial,
                &mut failed,
                format!(
                    "COMMENT ON SCHEMA {} IS {}",
                    ident(&public.name),
                    standard
                        .public_comment
                        .as_deref()
                        .map_or_else(|| "NULL".to_owned(), literal)
                ),
            )
            .await;
        }
    }
    if state.connection_limit != standard.connection_limit {
        run(
            conn,
            trial,
            &mut failed,
            format!(
                "ALTER DATABASE {database} CONNECTION LIMIT {}",
                standard.connection_limit
            ),
        )
        .await;
    }
    if state.template {
        run(
            conn,
            trial,
            &mut failed,
            format!("ALTER DATABASE {database} IS_TEMPLATE false"),
        )
        .await;
    }
    if state.comment != standard.comment {
        run(
            conn,
            trial,
            &mut failed,
            format!(
                "COMMENT ON DATABASE {database} IS {}",
                standard
                    .comment
                    .as_deref()
                    .map_or_else(|| "NULL".to_owned(), literal)
            ),
        )
        .await;
    }
    // One setting at a time: `RESET ALL` skips a setting the session may not
    // remove without saying so (measured on 16 and 18).
    for (name, value) in &state.settings {
        if !standard.settings.contains_key(name) {
            run(
                conn,
                trial,
                &mut failed,
                format!("ALTER DATABASE {database} RESET {}", ident(name)),
            )
            .await;
        } else if !same_value(name, value, &standard.settings[name]) {
            run(
                conn,
                trial,
                &mut failed,
                set(&database, name, &standard.settings[name]),
            )
            .await;
        }
    }
    for (name, value) in &standard.settings {
        if !state.settings.contains_key(name) {
            run(conn, trial, &mut failed, set(&database, name, value)).await;
        }
    }
    if !state.login_settings.is_empty() {
        run(
            conn,
            trial,
            &mut failed,
            format!("ALTER ROLE SESSION_USER IN DATABASE {database} RESET ALL"),
        )
        .await;
    }
    if let Some(kept) = acl {
        failed.extend(
            put_acl(
                conn,
                trial,
                &state.acl,
                kept,
                &format!("DATABASE {database}"),
                &state.owner,
            )
            .await,
        );
    }
    Ok(failed)
}

/// Only the declared connection limit, on a database whose other declared
/// state is already in place and has since been built on: the run-owned
/// layout sets it once its sessions are open, and a second [`enforce`]
/// there would undo what the run reproduced in between, such as a rebuilt
/// `public` and the target's database defaults (#1708 review).
pub async fn put_connection_limit(
    conn: &mut impl ExecuteConnection,
    database: &str,
    standard: &Standard,
) -> Result<(), DbError> {
    if standard.connection_limit == -1 {
        return Ok(());
    }
    conn.execute(&format!(
        "ALTER DATABASE {} CONNECTION LIMIT {}",
        ident(database),
        standard.connection_limit
    ))
    .await
}

/// Runs one statement of [`enforce`], noting the engine's refusal instead
/// of stopping.
async fn run(
    conn: &mut impl ExecuteConnection,
    trial: bool,
    failed: &mut Vec<String>,
    sql: String,
) {
    if let Err(error) = execute(conn, trial, &sql).await {
        failed.push(format!("{sql}: {error}"));
    }
}

/// One statement; in a trial, under a savepoint, so a refused statement
/// leaves the transaction usable for the next.
async fn execute(conn: &mut impl ExecuteConnection, trial: bool, sql: &str) -> Result<(), DbError> {
    if !trial {
        return conn.execute(sql).await;
    }
    conn.execute("SAVEPOINT pbps_standard_trial").await?;
    match conn.execute(sql).await {
        Ok(()) => conn.execute("RELEASE SAVEPOINT pbps_standard_trial").await,
        Err(error) => {
            conn.execute("ROLLBACK TO SAVEPOINT pbps_standard_trial")
                .await?;
            Err(error)
        }
    }
}

/// A list setting such as `search_path` goes element by element: as one
/// literal the engine stores a single quoted identifier, which never reads
/// back as the declared list (measured on 16 and 18). [`unappliable`] has
/// refused a declared list the engine cannot read before any write.
///
/// An empty list has no spelling after `TO`: nothing there is a syntax
/// error, and `''` stores `""`, one empty name. `FROM CURRENT` copies the
/// session's text verbatim, so the empty list is set for the one batch's
/// implicit transaction and copied from there, and the session keeps its
/// own value; it stores `name=` (measured on 16 and 18).
fn set(database: &str, name: &str, value: &str) -> String {
    match setting_value(name, value) {
        Ok(list) if list.is_empty() => format!(
            "SELECT pg_catalog.set_config({}, '', true); \
             ALTER DATABASE {database} SET {} FROM CURRENT",
            literal(name),
            ident(name)
        ),
        Ok(list) => format!("ALTER DATABASE {database} SET {} TO {list}", ident(name)),
        Err(_) => format!(
            "ALTER DATABASE {database} SET {} TO {}",
            ident(name),
            literal(value)
        ),
    }
}

/// Whether a stored setting is the declared one. A list setting is
/// compared element by element, because the engine stores it in its own
/// spelling (`"$user", public`), not as written.
fn same_value(name: &str, stored: &str, declared: &str) -> bool {
    if LIST_QUOTE_SETTINGS.contains(&name) {
        let list = pbps_db::resolver::environment::guc_list;
        if let (Some(stored), Some(declared)) = (list(stored), list(declared)) {
            return stored == declared;
        }
    }
    stored == declared
}

/// Makes the ACL of `object` equal `expected`: revokes each entry it should
/// not hold, then grants each it lacks, as the login, which acts for
/// `owner`. An entry another role granted is not the login's to revoke, and
/// may carry grants that depend on it; then nothing is changed and each
/// such entry is named, so the operator removes it.
async fn put_acl(
    conn: &mut impl ExecuteConnection,
    trial: bool,
    current: &BTreeSet<Entry>,
    expected: &BTreeSet<Entry>,
    object: &str,
    owner: &str,
) -> Vec<String> {
    let extra: Vec<&Entry> = current.difference(expected).collect();
    let foreign: Vec<String> = extra
        .iter()
        .filter(|entry| entry.grantor != owner)
        .map(|entry| {
            format!(
                "{object} grants {entry}, which only {} can revoke",
                entry.grantor
            )
        })
        .collect();
    if !foreign.is_empty() {
        return foreign;
    }
    let mut failed = Vec::new();
    let statements = extra
        .into_iter()
        .map(|entry| {
            format!(
                "REVOKE {} ON {object} FROM {}",
                entry.privilege,
                grantee(&entry.grantee)
            )
        })
        .chain(expected.difference(current).map(|entry| {
            format!(
                "GRANT {} ON {object} TO {}{}",
                entry.privilege,
                grantee(&entry.grantee),
                if entry.grantable {
                    " WITH GRANT OPTION"
                } else {
                    ""
                }
            )
        }));
    for sql in statements {
        if let Err(error) = execute(conn, trial, &sql).await {
            failed.push(format!("{sql}: {error}"));
        }
    }
    failed
}

/// Re-reads the database and names each way it is not `standard`.
pub async fn verify(
    conn: &mut impl QueryConnection,
    standard: &Standard,
    acl: Option<&BTreeSet<Entry>>,
) -> Result<Vec<String>, DbError> {
    Ok(differences(&read_state(conn).await?, standard, acl))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(grantee: &str, privilege: &str, grantor: &str, grantable: bool) -> Entry {
        Entry {
            grantee: grantee.into(),
            grantor: grantor.into(),
            privilege: privilege.into(),
            grantable,
        }
    }

    fn standard_state() -> State {
        State {
            database: "s".into(),
            owner: "login".into(),
            public: Some(Public {
                name: "public".into(),
                owner: PUBLIC_OWNER.into(),
                acl: Standard::default().public_acl(),
                comment: Some(PUBLIC_COMMENT.into()),
            }),
            connection_limit: -1,
            template: false,
            comment: None,
            settings: BTreeMap::new(),
            login_settings: BTreeSet::new(),
            acl: [entry("login", "CONNECT", "login", false)].into(),
        }
    }

    #[test]
    fn a_database_as_initdb_made_it_is_standard() {
        let state = standard_state();
        assert!(differences(&state, &Standard::default(), Some(&state.acl)).is_empty());
    }

    #[test]
    fn every_covered_change_is_named() {
        let mut state = standard_state();
        let kept = state.acl.clone();
        let public = state.public.as_mut().unwrap();
        public.name = "elsewhere".into();
        public.owner = "login".into();
        public.comment = None;
        public
            .acl
            .insert(entry("other", "USAGE", PUBLIC_OWNER, true));
        public
            .acl
            .remove(&entry(PUBLIC_GRANTEE, "USAGE", PUBLIC_OWNER, false));
        state.connection_limit = 0;
        state.template = true;
        state.comment = Some("c".into());
        state
            .settings
            .insert("statement_timeout".into(), "7s".into());
        state.login_settings.insert("work_mem=8MB".into());
        state.acl.insert(entry("PUBLIC", "CONNECT", "login", false));
        let found = differences(&state, &Standard::default(), Some(&kept));
        for expected in [
            "schema public is named elsewhere",
            "schema public is owned by login",
            "schema public grants USAGE to other with grant option granted by pg_database_owner",
            "schema public lacks USAGE to PUBLIC granted by pg_database_owner",
            "schema public's comment is none",
            "the connection limit is 0 instead of -1",
            "the database is a template",
            "the database's comment is \"c\"",
            "the database sets statement_timeout=7s",
            "the scratch account sets work_mem=8MB in this database",
            "the database grants CONNECT to PUBLIC granted by login",
        ] {
            assert!(
                found.iter().any(|f| f == expected),
                "{expected} in {found:#?}"
            );
        }
        assert_eq!(found.len(), 11, "{found:#?}");
    }

    #[test]
    fn a_declared_standard_is_what_the_database_is_compared_with() {
        let declared = Standard {
            settings: [("statement_timeout".into(), "5min".into())].into(),
            comment: Some("scratch".into()),
            connection_limit: 10,
            public_comment: Some("ours".into()),
            public_grants: [("ci_reader".into(), "USAGE".into())].into(),
        };
        let mut state = standard_state();
        state
            .settings
            .insert("statement_timeout".into(), "5min".into());
        state.comment = Some("scratch".into());
        state.connection_limit = 10;
        let public = state.public.as_mut().unwrap();
        public.comment = Some("ours".into());
        public
            .acl
            .insert(entry("ci_reader", "USAGE", PUBLIC_OWNER, false));
        assert!(differences(&state, &declared, None).is_empty());
        // Negative: the built-in standard names each declared item as a
        // difference, and the declared one names a value that moved.
        assert_eq!(differences(&state, &Standard::default(), None).len(), 5);
        state
            .settings
            .insert("statement_timeout".into(), "1s".into());
        assert_eq!(
            differences(&state, &declared, None),
            vec!["the database sets statement_timeout=1s".to_owned()]
        );
    }

    #[test]
    fn a_missing_public_and_an_unkept_acl_are_told_apart() {
        let mut state = standard_state();
        state.public = None;
        state.acl.clear();
        // No ACL kept yet: the database ACL is not compared.
        assert_eq!(
            differences(&state, &Standard::default(), None),
            vec!["schema public (OID 2200) is missing".to_owned()]
        );
        // Kept: its loss is named.
        let kept = standard_state().acl;
        assert_eq!(
            differences(&state, &Standard::default(), Some(&kept)).len(),
            2
        );
    }

    #[test]
    fn a_stored_setting_splits_at_its_first_equals_sign() {
        assert_eq!(
            setting("search_path=\"a=b\", public"),
            ("search_path".into(), "\"a=b\", public".into())
        );
        assert_eq!(setting("odd"), ("odd".into(), String::new()));
    }

    /// A login, another role, and a database the login owns, from
    /// `template0`, on the server `variable` names; dropped by [`Fixture::drop`].
    struct Fixture {
        admin: pbps_db::Conn,
        base: String,
        name: String,
        password: String,
    }

    impl Fixture {
        async fn new(variable: &str) -> Self {
            use pbps_db::{Conn, Driver};
            let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
            let name = format!(
                "pbps_std1708_{}",
                crate::catalog::probe_token().replace('-', "_")
            );
            let password = crate::catalog::probe_token().replace('-', "");
            let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
            for statement in [
                format!(
                    "CREATE ROLE {name} LOGIN PASSWORD '{password}' NOSUPERUSER NOCREATEDB NOCREATEROLE"
                ),
                format!("CREATE ROLE {name}_o NOLOGIN"),
                format!("CREATE ROLE {name}_n LOGIN PASSWORD '{password}'"),
                format!("CREATE DATABASE {name} OWNER {name} TEMPLATE template0"),
            ] {
                admin.execute(&statement).await.unwrap();
            }
            Self {
                admin,
                base,
                name,
                password,
            }
        }

        async fn connect(&self, login: &str) -> pbps_db::Conn {
            pbps_db::Conn::connect(
                pbps_db::Driver::Postgres,
                &format!(
                    "{} dbname={} user={login} password={}",
                    self.base, self.name, self.password
                ),
            )
            .await
            .unwrap()
        }

        async fn drop(mut self) {
            let name = &self.name;
            for statement in [
                format!("DROP DATABASE {name} WITH (FORCE)"),
                format!("DROP ROLE {name}"),
                format!("DROP ROLE {name}_o"),
                format!("DROP ROLE {name}_n"),
            ] {
                self.admin.execute(&statement).await.unwrap();
            }
        }
    }

    const SERVERS: [&str; 2] = ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"];

    /// The four-item argument's premise, on every supported server: in a
    /// database made from `template0`, the only initdb object of any
    /// owner-bearing catalog not owned by the bootstrap superuser is
    /// `public`. The catalogs are found by their owner column, so a release
    /// that adds one, or an object in one, fails this test rather than the
    /// guarantee. And the built-in standard is what such a database is.
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn a_fresh_database_is_the_built_in_standard_and_public_is_its_only_owned_initdb_object()
    {
        for variable in SERVERS {
            let fixture = Fixture::new(variable).await;
            let mut login = fixture.connect(&fixture.name).await;
            let catalogs = login
                .query(
                    "SELECT c.relname::text AS catalog, a.attname::text AS owner \
                       FROM pg_catalog.pg_class c \
                       JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
                      WHERE c.relnamespace = 'pg_catalog'::pg_catalog.regnamespace \
                        AND c.relkind = 'r' AND NOT a.attisdropped \
                        AND a.atttypid = 'pg_catalog.oid'::pg_catalog.regtype \
                        AND (a.attname ~ 'owner$' OR a.attname = 'datdba') \
                        AND EXISTS (SELECT FROM pg_catalog.pg_attribute o \
                                     WHERE o.attrelid = c.oid AND o.attname = 'oid')",
                )
                .await
                .unwrap();
            let union = catalogs
                .iter()
                .map(|row| {
                    let (catalog, owner) =
                        (text(row, "catalog").unwrap(), text(row, "owner").unwrap());
                    format!(
                        "SELECT '{catalog}' AS catalog, o.oid::text AS object \
                           FROM pg_catalog.{catalog} o \
                          WHERE o.oid < 16384 AND o.{owner} <> 10"
                    )
                })
                .collect::<Vec<_>>()
                .join(" UNION ALL ");
            let owned = login
                .query(&format!(
                    "SELECT catalog, object FROM ({union}) f ORDER BY 1, 2"
                ))
                .await
                .unwrap()
                .iter()
                .map(|row| (text(row, "catalog").unwrap(), text(row, "object").unwrap()))
                .collect::<Vec<_>>();
            let found = verify(&mut login, &Standard::default(), None)
                .await
                .unwrap();
            drop(login);
            fixture.drop().await;
            assert!(
                catalogs.len() > 10,
                "{variable}: the owner columns were found"
            );
            assert_eq!(
                owned,
                [("pg_namespace".to_owned(), PUBLIC_OID.to_string())],
                "{variable}"
            );
            assert!(found.is_empty(), "{variable}: {found:?}");
        }
    }

    /// Everything the standard covers, changed as a run's login can change
    /// it, is put back, and the database ACL back to the one kept.
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn enforce_puts_back_every_covered_change_and_the_kept_database_acl() {
        for variable in SERVERS {
            let fixture = Fixture::new(variable).await;
            let (name, other) = (fixture.name.clone(), format!("{}_o", fixture.name));
            let mut login = fixture.connect(&name).await;
            let kept = read_state(&mut login).await.unwrap().acl;
            for statement in [
                "ALTER SCHEMA public RENAME TO elsewhere".to_owned(),
                format!("ALTER SCHEMA elsewhere OWNER TO {name}"),
                "COMMENT ON SCHEMA elsewhere IS 'changed'".to_owned(),
                format!("GRANT USAGE ON SCHEMA elsewhere TO {other} WITH GRANT OPTION"),
                format!("GRANT CREATE ON SCHEMA elsewhere TO {other}"),
                "REVOKE USAGE ON SCHEMA elsewhere FROM PUBLIC".to_owned(),
                format!("ALTER DATABASE {name} CONNECTION LIMIT 5"),
                format!("ALTER DATABASE {name} IS_TEMPLATE true"),
                format!("ALTER DATABASE {name} SET statement_timeout = '7s'"),
                format!("ALTER ROLE CURRENT_USER IN DATABASE {name} SET work_mem = '8MB'"),
                format!("COMMENT ON DATABASE {name} IS 'changed'"),
                format!("REVOKE CONNECT ON DATABASE {name} FROM PUBLIC"),
                format!("GRANT CREATE ON DATABASE {name} TO {other}"),
            ] {
                login.execute(&statement).await.unwrap();
            }
            let before = verify(&mut login, &Standard::default(), Some(&kept))
                .await
                .unwrap();
            let failed = enforce(&mut login, &Standard::default(), Some(&kept))
                .await
                .unwrap();
            let after = verify(&mut login, &Standard::default(), Some(&kept))
                .await
                .unwrap();
            drop(login);
            fixture.drop().await;
            assert!(before.len() >= 12, "{variable}: {before:#?}");
            assert!(failed.is_empty(), "{variable}: {failed:#?}");
            assert!(after.is_empty(), "{variable}: {after:#?}");
        }
    }

    /// A database already standard costs no statement: a session that owns
    /// nothing and may change nothing enforces it without a refusal.
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn enforce_issues_no_statement_on_a_standard_database() {
        for variable in SERVERS {
            let fixture = Fixture::new(variable).await;
            let mut stranger = fixture.connect(&format!("{}_n", fixture.name)).await;
            let kept = read_state(&mut stranger).await.unwrap().acl;
            let failed = enforce(&mut stranger, &Standard::default(), Some(&kept))
                .await
                .unwrap();
            // Negative: the same session changing anything is refused, so an
            // issued statement would have shown.
            let refused = stranger
                .execute(&format!("COMMENT ON DATABASE {} IS 'x'", fixture.name))
                .await;
            drop(stranger);
            fixture.drop().await;
            assert!(failed.is_empty(), "{variable}: {failed:#?}");
            assert!(refused.is_err(), "{variable}");
        }
    }

    /// An entry on `public` another role granted, with a grant that depends
    /// on it, is not the login's to revoke: it is named, and the ACL is left
    /// as it was rather than half put back.
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn a_public_entry_another_role_granted_is_named_and_the_acl_is_left_alone() {
        for variable in SERVERS {
            let fixture = Fixture::new(variable).await;
            let (name, other) = (fixture.name.clone(), format!("{}_o", fixture.name));
            let stranger = format!("{name}_n");
            let mut admin = pbps_db::Conn::connect(
                pbps_db::Driver::Postgres,
                &format!("{} dbname={name}", fixture.base),
            )
            .await
            .unwrap();
            for statement in [
                format!("GRANT USAGE ON SCHEMA public TO {other} WITH GRANT OPTION"),
                format!("SET ROLE {other}"),
                format!("GRANT USAGE ON SCHEMA public TO {stranger}"),
                "RESET ROLE".to_owned(),
            ] {
                admin.execute(&statement).await.unwrap();
            }
            drop(admin);
            let mut login = fixture.connect(&name).await;
            let before = read_state(&mut login).await.unwrap().public;
            let failed = enforce(&mut login, &Standard::default(), None)
                .await
                .unwrap();
            let after = read_state(&mut login).await.unwrap().public;
            drop(login);
            fixture.drop().await;
            assert_eq!(before, after, "{variable}: the ACL was left alone");
            assert_eq!(
                failed,
                [format!(
                    "SCHEMA \"public\" grants USAGE to {stranger} granted by {other}, which only \
                     {other} can revoke"
                )],
                "{variable}"
            );
        }
    }

    /// A declared standard the engine stores in its own spelling is still
    /// the declared one: a setting the engine names in mixed case
    /// (`TimeZone`), a list setting stored element by element, and a grant
    /// to `PUBLIC`, which is no row of `pg_roles`. Applied once, it reads
    /// back as declared, and the next run finds nothing to apply.
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn a_declared_standard_in_the_engines_own_spelling_reads_back_as_declared() {
        for variable in SERVERS {
            let fixture = Fixture::new(variable).await;
            let mut standard = Standard::default();
            for (name, value) in [
                ("timezone", "UTC"),
                ("datestyle", "ISO, MDY"),
                // Written without the spaces the engine stores between elements.
                ("search_path", "\"$user\",public,pg_catalog"),
                // An empty list, which has no spelling after `TO`.
                ("temp_tablespaces", ""),
            ] {
                standard.settings.insert(name.into(), value.into());
            }
            standard
                .public_grants
                .insert((PUBLIC_GRANTEE.into(), "CREATE".into()));
            let mut login = fixture.connect(&fixture.name).await;
            login
                .execute("SET temp_tablespaces = pg_default")
                .await
                .unwrap();
            let refused = unappliable(&mut login, &standard).await.unwrap();
            let failed = enforce(&mut login, &standard, None).await.unwrap();
            // The empty list is copied from a transaction-local value; the
            // session's own settings are not the standard's to change.
            let session = login.query("SHOW temp_tablespaces").await.unwrap();
            let session = text(&session[0], "temp_tablespaces").unwrap();
            let left = verify(&mut login, &standard, None).await.unwrap();
            let again = unappliable(&mut login, &standard).await.unwrap();
            // Negative: an unknown setting and a list the engine cannot read
            // are still named before any write.
            let mut wrong = standard.clone();
            wrong.settings.insert("no_such_setting".into(), "1".into());
            wrong
                .settings
                .insert("search_path".into(), "\"unterminated".into());
            let named = unappliable(&mut login, &wrong).await.unwrap();
            drop(login);
            fixture.drop().await;
            assert!(refused.is_empty(), "{variable}: {refused:#?}");
            assert!(failed.is_empty(), "{variable}: {failed:#?}");
            assert!(left.is_empty(), "{variable}: {left:#?}");
            assert!(again.is_empty(), "{variable}: {again:#?}");
            assert_eq!(named.len(), 2, "{variable}: {named:#?}");
            assert_eq!(session, "pg_default", "{variable}");
        }
    }

    #[tokio::test]
    #[ignore = "requires the pinned PostgreSQL servers"]
    async fn a_trial_names_what_the_engine_refuses_and_writes_nothing() {
        // The engine, not a model of it, judges a declared value: a value
        // it cannot parse and a comment it stores otherwise are named, every
        // refusal and not only the first, and the database is left as it
        // was (#1708 review).
        for variable in SERVERS {
            let fixture = Fixture::new(variable).await;
            let mut login = fixture.connect(&fixture.name).await;
            let wrong = Standard {
                settings: [
                    ("statement_timeout".to_owned(), "nonsense".to_owned()),
                    ("work_mem".to_owned(), "lots".to_owned()),
                ]
                .into(),
                comment: Some("ours".into()),
                ..Standard::default()
            };
            // Taken by the engine, but read back otherwise than declared: it
            // removes a comment given as the empty string.
            let unreadable = Standard {
                public_comment: Some(String::new()),
                ..Standard::default()
            };
            let before = read_state(&mut login).await.unwrap();
            let refused = trial(&mut login, &wrong).await.unwrap();
            let differs = trial(&mut login, &unreadable).await.unwrap();
            let after_refused = read_state(&mut login).await.unwrap();
            // Negative: a declaration the engine takes whole is tried clean,
            // and still written only by `enforce`.
            let right = Standard {
                settings: [("statement_timeout".to_owned(), "5min".to_owned())].into(),
                comment: Some("ours".into()),
                ..Standard::default()
            };
            let clean = trial(&mut login, &right).await.unwrap();
            let after_clean = read_state(&mut login).await.unwrap();
            let failed = enforce(&mut login, &right, None).await.unwrap();
            let left = verify(&mut login, &right, None).await.unwrap();
            drop(login);
            fixture.drop().await;
            assert_eq!(refused.len(), 2, "{variable}: {refused:#?}");
            assert!(
                refused.iter().any(|r| r.contains("statement_timeout"))
                    && refused.iter().any(|r| r.contains("work_mem")),
                "{variable}: {refused:#?}"
            );
            assert!(
                differs.len() == 1 && differs[0].contains("public"),
                "{variable}: {differs:#?}"
            );
            assert_eq!(before, after_refused, "{variable}: nothing was written");
            assert!(clean.is_empty(), "{variable}: {clean:#?}");
            assert_eq!(before, after_clean, "{variable}: a trial writes nothing");
            assert!(
                failed.is_empty() && left.is_empty(),
                "{variable}: {failed:#?} {left:#?}"
            );
        }
    }
}
