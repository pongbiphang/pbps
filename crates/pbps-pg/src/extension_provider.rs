//! What the engine says about extension packages: which are installed, which
//! could be installed and under what controls, and what an installed one owns
//! (DEC-2447.4).
//!
//! Every read here is one static statement, so one snapshot, and decodes each
//! field strictly. Absent, empty and unreadable stay three different results:
//! a package with no `requires` has a row that says so, a package the server
//! does not list has no row, and a row that cannot be decoded is an error, never
//! an empty list.

use pbps_db::transport::QueryConnection;
use pbps_db::{DbError, Row};
use pbps_model::escape::Escaped;
use pbps_model::{Extension, ExtensionDropFacts, ExtensionProvider, ProviderMember};
use std::collections::{BTreeMap, BTreeSet};

/// Every installed package, with its schema and exact version. The namespace is
/// a LEFT JOIN: an inner join would drop a package whose schema row cannot be
/// read, and an incomplete inventory must not look complete.
pub(crate) const INSTALLED: &str = "SELECT e.extname::text AS name, n.nspname::text AS schema, e.extversion::text AS version FROM pg_catalog.pg_extension e LEFT JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace";

/// The installable versions, from the view the server documents for it, never
/// from `pg_extension_update_paths`: a path can end at a version that has no
/// installation script (measured: PG 18 pgcrypto lists 1.0 paths and refuses
/// `CREATE EXTENSION ... VERSION '1.0'`).
const AVAILABLE: &str = "SELECT name::text AS name, version::text AS version, installed::text AS installed, superuser::text AS superuser, trusted::text AS trusted, relocatable::text AS relocatable, schema::text AS schema, pg_catalog.array_to_json(requires)::text AS requires FROM pg_catalog.pg_available_extension_versions";

/// Direct members (`e` edges) and their internal and automatic subsidiaries,
/// the same closure the census projects out of independent declarations. An
/// `x` edge (a user routine marked `DEPENDS ON EXTENSION`) is deliberately not
/// membership: it is a user object that `DROP EXTENSION` happens to remove.
/// Identities come from the engine's own address renderer, which holds no
/// object number.
///
/// A TOAST table and its index are left out, though the closure reaches them
/// (an `i` edge from the owning table, measured on PG 16 and 18). They are
/// physical storage the engine creates for any table with a toastable column,
/// named `pg_toast_<oid of the owner>`, so the same package in two databases
/// would differ by a number that means nothing and `vet` would refuse a valid
/// install. The logical owner stays a member, and it is the owner that decides
/// whether storage exists. They are found by what they are (`relkind = 't'`, or
/// an index on such a relation), not by the shape of their names.
const PROVIDERS: &str = "\
WITH RECURSIVE members(ext, classid, objid, objsubid) AS (
    SELECT d.refobjid, d.classid, d.objid, d.objsubid
      FROM pg_catalog.pg_depend d
     WHERE d.refclassid = 'pg_catalog.pg_extension'::regclass AND d.deptype = 'e'
    UNION
    SELECT m.ext, d.classid, d.objid, d.objsubid
      FROM members m JOIN pg_catalog.pg_depend d
        ON d.refclassid = m.classid AND d.refobjid = m.objid AND d.deptype IN ('i', 'a')
)
SELECT 'extension' AS kind, e.extname::text AS name, n.nspname::text AS one,
       e.extversion::text AS two, NULL::text AS three
  FROM pg_catalog.pg_extension e LEFT JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
UNION ALL
SELECT 'requires', e.extname::text, r.extname::text, NULL, NULL
  FROM pg_catalog.pg_depend d
  JOIN pg_catalog.pg_extension e ON e.oid = d.objid
  JOIN pg_catalog.pg_extension r ON r.oid = d.refobjid
 WHERE d.classid = 'pg_catalog.pg_extension'::regclass
   AND d.refclassid = 'pg_catalog.pg_extension'::regclass AND d.deptype = 'n'
UNION ALL
SELECT 'member', e.extname::text, a.type, pg_catalog.array_to_json(a.object_names)::text,
       pg_catalog.array_to_json(a.object_args)::text
  FROM members m
  JOIN pg_catalog.pg_extension e ON e.oid = m.ext
  LEFT JOIN LATERAL pg_catalog.pg_identify_object_as_address(m.classid, m.objid, m.objsubid) a ON true
 WHERE NOT (m.classid = 'pg_catalog.pg_class'::regclass AND EXISTS (
         SELECT 1 FROM pg_catalog.pg_class c
          WHERE c.oid = m.objid
            AND (c.relkind = 't' OR EXISTS (
                 SELECT 1 FROM pg_catalog.pg_index x
                   JOIN pg_catalog.pg_class t ON t.oid = x.indrelid
                  WHERE x.indexrelid = c.oid AND t.relkind = 't'))))";

/// Everything `DROP EXTENSION` would delete or be blocked by, per package
/// (DEC-2447.8): the objects reachable from the package through the dependency
/// edges the engine follows when it removes something (`e` members, `x`
/// `DEPENDS ON EXTENSION` marks, and the automatic and internal subsidiaries of
/// each), and the objects outside that set which depend on it normally, which
/// make the engine refuse. The extension's own row is in the set, so a package
/// that requires another is a blocker of it by its `n` edge, and an object that
/// belongs to *another* package's reach (a member of a package that requires
/// this one, which uses a member of this one) is reported as that package, so
/// that dropping both clears it and dropping one does not.
macro_rules! reach {
    () => {
        "WITH RECURSIVE reach(ext, classid, objid, objsubid) AS (
    SELECT e.oid, 'pg_catalog.pg_extension'::regclass::oid, e.oid, 0 FROM pg_catalog.pg_extension e
    UNION
    SELECT d.refobjid, d.classid, d.objid, d.objsubid
      FROM pg_catalog.pg_depend d
     WHERE d.refclassid = 'pg_catalog.pg_extension'::regclass AND d.deptype IN ('e', 'x')
    UNION
    SELECT r.ext, d.classid, d.objid, d.objsubid
      FROM reach r JOIN pg_catalog.pg_depend d
        ON d.refclassid = r.classid AND d.refobjid = r.objid
       AND d.deptype IN ('a', 'i', 'x', 'P', 'S')
)"
    };
}

/// The same storage exclusion as [`PROVIDERS`]: a TOAST table and its index are
/// physical storage named by a number, reached from their owner, and not an
/// object a person would name.
const NOT_STORAGE: &str = "NOT (r.classid = 'pg_catalog.pg_class'::regclass AND EXISTS (
         SELECT 1 FROM pg_catalog.pg_class c
          WHERE c.oid = r.objid
            AND (c.relkind = 't' OR EXISTS (
                 SELECT 1 FROM pg_catalog.pg_index x
                   JOIN pg_catalog.pg_class t ON t.oid = x.indrelid
                  WHERE x.indexrelid = c.oid AND t.relkind = 't'))))";

const DROP_READ: &str = concat!(
    reach!(),
    "
SELECT 'reach' AS kind, e.extname::text AS name, a.type, pg_catalog.array_to_json(a.object_names)::text AS names,
       pg_catalog.array_to_json(a.object_args)::text AS args
  FROM reach r
  JOIN pg_catalog.pg_extension e ON e.oid = r.ext
  LEFT JOIN LATERAL pg_catalog.pg_identify_object_as_address(r.classid, r.objid, r.objsubid) a ON true
 WHERE NOT (r.classid = 'pg_catalog.pg_extension'::regclass AND r.objid = r.ext)
   AND ",
    "{NOT_STORAGE}",
    "
UNION ALL
SELECT 'blocker', e.extname::text, a.type, pg_catalog.array_to_json(a.object_names)::text,
       pg_catalog.array_to_json(a.object_args)::text
  FROM reach r
  JOIN pg_catalog.pg_extension e ON e.oid = r.ext
  JOIN pg_catalog.pg_depend d
    ON d.refclassid = r.classid AND d.refobjid = r.objid AND d.deptype = 'n'
  LEFT JOIN LATERAL pg_catalog.pg_identify_object_as_address(d.classid, d.objid, d.objsubid) a ON true
 WHERE NOT EXISTS (SELECT 1 FROM reach o WHERE o.classid = d.classid AND o.objid = d.objid)
UNION ALL
SELECT 'blocker', e.extname::text, 'extension', pg_catalog.array_to_json(ARRAY[o.name])::text, '[]'
  FROM reach r
  JOIN pg_catalog.pg_extension e ON e.oid = r.ext
  JOIN pg_catalog.pg_depend d
    ON d.refclassid = r.classid AND d.refobjid = r.objid AND d.deptype = 'n'
  JOIN (SELECT o.ext, o.classid, o.objid, x.extname::text AS name
          FROM reach o JOIN pg_catalog.pg_extension x ON x.oid = o.ext) o
    ON o.classid = d.classid AND o.objid = d.objid AND o.ext <> r.ext
 WHERE NOT EXISTS (SELECT 1 FROM reach m
                    WHERE m.ext = r.ext AND m.classid = d.classid AND m.objid = d.objid)"
);

/// What dropping each installed package reaches, for the packages in
/// `providers` (read just before, so members and the rest are one story). A
/// member is what the manifest already holds; the rest of the reach is
/// `outside`. An object the engine can no longer name is unreadable evidence,
/// never a smaller set.
pub async fn drop_facts(
    conn: &mut impl QueryConnection,
    providers: &BTreeMap<String, ExtensionProvider>,
) -> Result<BTreeMap<String, ExtensionDropFacts>, DbError> {
    let mut out: BTreeMap<String, ExtensionDropFacts> = providers
        .keys()
        .map(|name| (name.clone(), ExtensionDropFacts::default()))
        .collect();
    let sql = DROP_READ.replace("{NOT_STORAGE}", NOT_STORAGE);
    for row in conn.query(&sql).await? {
        let name = text(&row, "name")?;
        let class = row.try_get::<&str>("type")?.ok_or_else(|| {
            DbError::Refused(format!(
                "the catalog changed while what dropping extension `{}` reaches was read: an \
                 object was there when the dependencies were walked and gone when it was named. \
                 Run the command again when the other change has finished.",
                Escaped(&name)
            ))
        })?;
        let member = ProviderMember {
            class: class.to_owned(),
            names: names(row.try_get::<&str>("names")?, "an object's names")?,
            args: names(row.try_get::<&str>("args")?, "an object's arguments")?,
        };
        let facts = out.get_mut(&name).ok_or_else(|| {
            DbError::BadRow(format!(
                "a row names extension `{}`, which the same read does not list",
                Escaped(&name)
            ))
        })?;
        match row.try_get::<&str>("kind")? {
            Some("reach") => {
                // A member the manifest holds is not outside it.
                if !providers[&name].members.contains(&member) {
                    facts.outside.insert(member);
                }
            }
            Some("blocker") => {
                facts.blockers.insert(member);
            }
            _ => {
                return Err(DbError::BadRow(
                    "an extension drop read returned an unknown row kind".into(),
                ));
            }
        }
    }
    Ok(out)
}

/// Why the connected role could not drop each of `names`, one sentence each:
/// measured on 16.15 and 18.6, `DROP EXTENSION` needs ownership of the package
/// (a superuser, its owner, or a member of the owner that inherits its
/// privileges) and nothing else; `CREATE` on the database is not asked.
pub async fn drop_problems(
    conn: &mut impl QueryConnection,
    names: &BTreeSet<String>,
) -> Result<Vec<String>, DbError> {
    ownership_problems(conn, names, "drop").await
}

/// The ownership rule `ALTER EXTENSION` and `DROP EXTENSION` share, for the
/// action `verb` names (measured on 16.15 and 18.6: a stranger and a member
/// that does not inherit get 42501, "must be owner of extension").
async fn ownership_problems(
    conn: &mut impl QueryConnection,
    names: &BTreeSet<String>,
    verb: &str,
) -> Result<Vec<String>, DbError> {
    const WHO: &str = "SELECT e.extname::text AS name, pg_catalog.pg_get_userbyid(e.extowner)::text AS owner, (r.rolsuper OR pg_catalog.pg_has_role(current_user, e.extowner, 'USAGE'))::text AS allowed, current_user::text AS me FROM pg_catalog.pg_extension e CROSS JOIN pg_catalog.pg_roles r WHERE r.rolname = current_user";
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for row in conn.query(WHO).await? {
        let name = text(&row, "name")?;
        if !names.contains(&name) {
            continue;
        }
        if !flag(&row, "allowed")? {
            out.push(format!(
                "extension `{}` is owned by `{}`, and the connected role `{}` is neither a superuser, that owner, nor a member of it that inherits its privileges, so the engine would refuse the {verb} (42501)",
                Escaped(&name),
                Escaped(&text(&row, "owner")?),
                Escaped(&text(&row, "me")?)
            ));
        }
        seen.insert(name);
    }
    for missing in names.difference(&seen) {
        out.push(format!(
            "extension `{}` is not installed on the target, so there is nothing to {verb}",
            Escaped(missing)
        ));
    }
    Ok(out)
}

/// The check that runs in the statement's own transaction, *before* a
/// `DROP EXTENSION`: the closure the engine holds now is the one the plan was
/// approved over, nothing outside it depends on the package that the plan did
/// not accept, and what is about to go is remembered (in a temporary table that
/// ends with the transaction) so that [`confirm_dropped`] can prove it went.
pub async fn vet_before_drop(
    conn: &mut pbps_db::Conn,
    name: &str,
    pin: &pbps_model::ExtensionDropPin,
) -> Result<(), DbError> {
    let all = providers(conn).await?;
    let Some(actual) = all.get(name) else {
        return Err(DbError::Refused(format!(
            "extension `{}` is no longer installed",
            Escaped(name)
        )));
    };
    let differences = pin.manifest.differences(actual);
    if !differences.is_empty() {
        return Err(DbError::Refused(format!(
            "extension `{}` is not what the plan was approved to drop (its members changed since \
             it was planned): {}",
            Escaped(name),
            differences.join("; ")
        )));
    }
    let facts = drop_facts(conn, &all)
        .await?
        .remove(name)
        .unwrap_or_default();
    let list = |set: &BTreeSet<ProviderMember>| {
        set.iter()
            .map(|m| format!("`{m}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !facts.blockers.is_empty() {
        return Err(DbError::Refused(format!(
            "extension `{}` still has dependents the engine would refuse to drop: {}",
            Escaped(name),
            list(&facts.blockers)
        )));
    }
    let unaccepted: BTreeSet<_> = facts.outside.difference(&pin.outside).cloned().collect();
    if !unaccepted.is_empty() {
        return Err(DbError::Refused(format!(
            "extension `{}` has objects the engine would delete with it that the plan did not accept: {}",
            Escaped(name),
            list(&unaccepted)
        )));
    }
    conn.execute(
        "CREATE TEMPORARY TABLE IF NOT EXISTS pg_temp.pbps_drop_reach \
         (ext text, classid oid, objid oid, objsubid int4) ON COMMIT DROP",
    )
    .await?;
    conn.execute(&format!(
        "{} INSERT INTO pg_temp.pbps_drop_reach \
         SELECT e.extname, r.classid, r.objid, r.objsubid FROM reach r \
           JOIN pg_catalog.pg_extension e ON e.oid = r.ext WHERE e.extname = {}",
        reach!(),
        crate::emit::literal(name),
    ))
    .await?;
    Ok(())
}

/// The check that runs in the same transaction right *after* the drop: the
/// package is gone, and so is every object that was reachable from it when
/// [`vet_before_drop`] looked. An object the engine can still name means the
/// drop did not do what it was approved to do, and the transaction rolls back.
pub async fn confirm_dropped(conn: &mut impl QueryConnection, name: &str) -> Result<(), DbError> {
    if installed(conn).await?.contains_key(name) {
        return Err(DbError::Refused(format!(
            "extension `{}` is still installed after its drop statement",
            Escaped(name)
        )));
    }
    let left = conn
        .query(&format!(
            "SELECT count(*)::text AS n FROM pg_temp.pbps_drop_reach t WHERE t.ext = {} \
             AND pg_catalog.pg_describe_object(t.classid, t.objid, t.objsubid) IS NOT NULL",
            crate::emit::literal(name)
        ))
        .await?;
    let n = left
        .first()
        .map(|r| text(r, "n"))
        .transpose()?
        .ok_or_else(|| DbError::BadRow("the drop check read no count".into()))?;
    if n != "0" {
        return Err(DbError::Refused(format!(
            "{n} object(s) reachable from extension `{}` are still there after its drop",
            Escaped(name)
        )));
    }
    Ok(())
}

/// One installable version of one package, as its control file declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    /// The version row that is installed now, which is not what a new install
    /// asks for.
    pub installed: bool,
    pub superuser: bool,
    pub trusted: bool,
    pub relocatable: bool,
    /// A package with a fixed schema installs there only.
    pub schema: Option<String>,
    pub requires: BTreeSet<String>,
}

fn text(row: &Row, field: &str) -> Result<String, DbError> {
    row.try_get::<&str>(field)?
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| DbError::BadRow(format!("the extension catalog read no `{field}`")))
}

fn flag(row: &Row, field: &str) -> Result<bool, DbError> {
    match text(row, field)?.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(DbError::BadRow(format!(
            "extension field `{field}` is `{}`, not a boolean",
            Escaped(other)
        ))),
    }
}

fn names(json: Option<&str>, what: &str) -> Result<Vec<String>, DbError> {
    json.map_or(Ok(Vec::new()), |json| {
        serde_json::from_str(json).map_err(|error| {
            DbError::BadRow(format!("{what} is not a JSON array of names: {error}"))
        })
    })
}

/// Every installed package. Duplicate or empty fields refuse.
pub async fn installed(
    conn: &mut impl QueryConnection,
) -> Result<BTreeMap<String, Extension>, DbError> {
    let mut out = BTreeMap::new();
    for row in conn.query(INSTALLED).await? {
        let name = text(&row, "name")?;
        let definition = Extension {
            schema: text(&row, "schema")?,
            version: text(&row, "version")?,
        };
        if out.insert(name.clone(), definition).is_some() {
            return Err(DbError::BadRow(format!(
                "extension `{}` has duplicate inventory rows",
                Escaped(&name)
            )));
        }
    }
    Ok(out)
}

/// Every installable `(name, version)` with its controls.
pub async fn available(
    conn: &mut impl QueryConnection,
) -> Result<BTreeMap<(String, String), Control>, DbError> {
    let mut out = BTreeMap::new();
    for row in conn.query(AVAILABLE).await? {
        let key = (text(&row, "name")?, text(&row, "version")?);
        let control = Control {
            installed: flag(&row, "installed")?,
            superuser: flag(&row, "superuser")?,
            trusted: flag(&row, "trusted")?,
            relocatable: flag(&row, "relocatable")?,
            // NULL is the control file naming no fixed schema.
            schema: row.try_get::<&str>("schema")?.map(str::to_owned),
            // The view row exists, so NULL here is a package that needs none.
            requires: names(row.try_get::<&str>("requires")?, "requires")?
                .into_iter()
                .collect(),
        };
        if out.insert(key.clone(), control).is_some() {
            return Err(DbError::BadRow(format!(
                "extension `{}` version `{}` is listed twice",
                Escaped(&key.0),
                Escaped(&key.1)
            )));
        }
    }
    Ok(out)
}

/// Every installed package with its prerequisites and member closure, from one
/// snapshot. A member whose identity the engine can no longer render was there
/// when the closure was walked and gone when it was named: that is unreadable
/// evidence, never a smaller closure.
pub async fn providers(
    conn: &mut impl QueryConnection,
) -> Result<BTreeMap<String, ExtensionProvider>, DbError> {
    let mut out: BTreeMap<String, ExtensionProvider> = BTreeMap::new();
    let rows = conn.query(PROVIDERS).await?;
    // The package rows first, so a prerequisite or member row for a package the
    // snapshot does not list is an inconsistency and not a new package.
    for row in rows.iter().filter(|r| kind(r) == Some("extension")) {
        let name = text(row, "name")?;
        let provider = ExtensionProvider {
            definition: Extension {
                schema: text(row, "one")?,
                version: text(row, "two")?,
            },
            requires: BTreeSet::new(),
            members: BTreeSet::new(),
        };
        if out.insert(name.clone(), provider).is_some() {
            return Err(DbError::BadRow(format!(
                "extension `{}` has duplicate inventory rows",
                Escaped(&name)
            )));
        }
    }
    for row in &rows {
        let name = text(row, "name")?;
        let unlisted = || {
            DbError::BadRow(format!(
                "a row names extension `{}`, which the same snapshot does not list",
                Escaped(&name)
            ))
        };
        match kind(row) {
            Some("extension") => {}
            Some("requires") => {
                let provider = out.get_mut(&name).ok_or_else(unlisted)?;
                provider.requires.insert(text(row, "one")?);
            }
            Some("member") => {
                let provider = out.get_mut(&name).ok_or_else(unlisted)?;
                let class = row.try_get::<&str>("one")?.ok_or_else(|| {
                    DbError::Refused(format!(
                        "the catalog changed while the members of extension `{}` were read: \
                         an object was there when the closure was walked and gone when it was \
                         named. Run the command again when the other change has finished.",
                        Escaped(&name)
                    ))
                })?;
                provider.members.insert(ProviderMember {
                    class: class.to_owned(),
                    names: names(row.try_get::<&str>("two")?, "a member's names")?,
                    args: names(row.try_get::<&str>("three")?, "a member's arguments")?,
                });
            }
            _ => {
                return Err(DbError::BadRow(
                    "an extension read returned an unknown row kind".into(),
                ));
            }
        }
    }
    Ok(out)
}

fn kind(row: &Row) -> Option<&str> {
    row.try_get::<&str>("kind").ok().flatten()
}

/// Why `wanted` cannot be installed at `place`, one sentence each. `known` is
/// every package the install may rest on: those declared with a pin, and those
/// already installed. A prerequisite outside it would have to be chosen by
/// `CREATE EXTENSION ... CASCADE`, which picks a default version the
/// declaration never approved, so it is a refusal instead.
pub fn requirement_problems(
    place: &str,
    name: &str,
    wanted: &Extension,
    available: &BTreeMap<(String, String), Control>,
    known: &BTreeMap<String, Extension>,
) -> Vec<String> {
    requirement_problems_through(place, name, wanted, available, known, &BTreeSet::new())
}

/// The prerequisites of every version an update walks, the target's own included
/// (measured on 16.15 and 18.6: the engine reads the control file of each version
/// it applies, so a version in the middle of the path that requires a package
/// the final one no longer does still fails without it, and passes once it is
/// created earlier in the transaction). A version no control file or script
/// lists adds nothing here, and takes its `requires` from the package's main
/// control file, which any listed version that does not override it shows.
pub fn path_requires(
    available: &BTreeMap<(String, String), Control>,
    name: &str,
    path: &[String],
) -> BTreeSet<String> {
    path.iter()
        .filter_map(|version| available.get(&(name.to_owned(), version.clone())))
        .flat_map(|control| control.requires.iter().cloned())
        .collect()
}

/// [`requirement_problems`] for a version that is reached through others: `also`
/// are the prerequisites of the versions on the way (see [`path_requires`]),
/// which are held to the same rule as the target's own.
pub fn requirement_problems_through(
    place: &str,
    name: &str,
    wanted: &Extension,
    available: &BTreeMap<(String, String), Control>,
    known: &BTreeMap<String, Extension>,
    also: &BTreeSet<String>,
) -> Vec<String> {
    let at = |detail: String| {
        format!(
            "extension `{}` version `{}` {detail}",
            Escaped(name),
            Escaped(&wanted.version)
        )
    };
    let Some(control) = available.get(&(name.to_owned(), wanted.version.clone())) else {
        let listed: Vec<&str> = available
            .keys()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect();
        return vec![at(format!(
            "is not installable on the {place} ({}); provision the package outside pbps or \
             change the declaration",
            if listed.is_empty() {
                "the package is not available at all".to_owned()
            } else {
                format!(
                    "available versions: {}",
                    listed
                        .iter()
                        .map(|v| format!("`{}`", Escaped(v)))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        ))];
    };
    let mut out = Vec::new();
    if let Some(fixed) = &control.schema
        && *fixed != wanted.schema
    {
        out.push(at(format!(
            "installs only into schema `{}`, not the declared `{}`",
            Escaped(fixed),
            Escaped(&wanted.schema)
        )));
    }
    let needed: BTreeSet<&String> = control.requires.iter().chain(also).collect();
    for required in needed.into_iter().filter(|r| !known.contains_key(*r)) {
        out.push(at(format!(
            "requires extension `{}`, which is neither declared with a pinned version nor \
             installed; declare it first (pbps never installs a prerequisite by default)",
            Escaped(required)
        )));
    }
    out
}

/// What decides whether the connected role can install a package on the target
/// now: measured on PostgreSQL 16.15 and 18.6, a superuser always can; any
/// other role can install a package whose control file does not ask for a
/// superuser (`superuser = false`) with no further privilege, and one marked
/// `trusted` only with `CREATE` on the database; an untrusted package that asks
/// for a superuser is refused. `CREATE` on the schema is not asked for by any
/// of the three, which the engine does not check either.
#[derive(Debug, Clone)]
pub struct Target {
    pub installed: BTreeMap<String, Extension>,
    pub available: BTreeMap<(String, String), Control>,
    pub schemas: BTreeSet<String>,
    pub superuser: bool,
    pub database_create: bool,
}

const WHO: &str = "SELECT r.rolsuper::text AS superuser, pg_catalog.has_database_privilege(pg_catalog.current_database(), 'CREATE')::text AS database_create FROM pg_catalog.pg_roles r WHERE r.rolname = current_user";
const SCHEMAS: &str = "SELECT nspname::text AS name FROM pg_catalog.pg_namespace";

/// The facts [`create_problems`] judges, read on the connection that will run
/// the statement.
pub async fn target(conn: &mut impl QueryConnection) -> Result<Target, DbError> {
    let who = conn.query(WHO).await?;
    let [who] = who.as_slice() else {
        return Err(DbError::BadRow(
            "the connected role has no pg_roles row".into(),
        ));
    };
    let mut schemas = BTreeSet::new();
    for row in conn.query(SCHEMAS).await? {
        schemas.insert(text(&row, "name")?);
    }
    Ok(Target {
        installed: installed(conn).await?,
        available: available(conn).await?,
        schemas,
        superuser: flag(who, "superuser")?,
        database_create: flag(who, "database_create")?,
    })
}

/// Why `CREATE EXTENSION` of `wanted` would fail or surprise on `target`, one
/// sentence each. `pinned` are the packages the declaration pins that this plan
/// installs: with the installed ones they are what a prerequisite may be.
pub fn create_problems(
    target: &Target,
    name: &str,
    wanted: &Extension,
    pinned: &BTreeMap<String, Extension>,
) -> Vec<String> {
    let at = |detail: String| {
        format!(
            "extension `{}` version `{}` {detail}",
            Escaped(name),
            Escaped(&wanted.version)
        )
    };
    let mut out = Vec::new();
    if let Some(held) = target.installed.get(name) {
        out.push(at(format!(
            "is already installed on the target (version `{}` in schema `{}`); a create never adopts a package that stands: select it with `pbps pull --extension {}` and adopt it with `pbps baseline`",
            Escaped(&held.version),
            Escaped(&held.schema),
            Escaped(name)
        )));
    }
    let mut known = target.installed.clone();
    known.extend(pinned.iter().map(|(n, d)| (n.clone(), d.clone())));
    out.extend(requirement_problems(
        "target",
        name,
        wanted,
        &target.available,
        &known,
    ));
    if !target.schemas.contains(&wanted.schema) {
        out.push(at(format!(
            "installs into schema `{}`, which the target does not have; create the schema first",
            Escaped(&wanted.schema)
        )));
    }
    out.extend(script_privilege(
        target,
        name,
        wanted,
        "install",
        "installed",
    ));
    out
}

/// Whether the connected role may run the version's script at all: the same
/// rule for an install and for an update (measured on 16.15 and 18.6, where an
/// owner without `CREATE` on the database is refused a real update with 42501
/// and a non-superuser is refused an untrusted package that asks for one).
fn script_privilege(
    target: &Target,
    name: &str,
    version: &Extension,
    noun: &str,
    verb: &str,
) -> Option<String> {
    let control = target
        .available
        .get(&(name.to_owned(), version.version.clone()))?;
    if target.superuser || !control.superuser {
        return None;
    }
    let at = |detail: String| {
        format!(
            "extension `{}` version `{}` {detail}",
            Escaped(name),
            Escaped(&version.version)
        )
    };
    Some(if !control.trusted {
        at(format!(
            "is not trusted and its {noun} script needs a superuser; the connected role is not one"
        ))
    } else if !target.database_create {
        at(format!(
            "is {verb} by a role with CREATE on the database, which the connected role lacks"
        ))
    } else {
        return None;
    })
}

/// The versions the engine would walk from `from` to `to`, from its own
/// `pg_extension_update_paths`: `None` is no path (a downgrade, an unknown
/// version, the current one), which is a refusal and never a default. A path
/// can end at a version that has no install script, so this says nothing about
/// whether a version is installable.
pub async fn update_path(
    conn: &mut impl QueryConnection,
    name: &str,
    from: &str,
    to: &str,
) -> Result<Option<Vec<String>>, DbError> {
    // The function raises (0A000) for a package the server does not list at all,
    // which is no path and is named by `requirement_problems`.
    let listed = conn
        .query(&format!(
            "SELECT 1 AS listed FROM pg_catalog.pg_available_extensions WHERE name = {}",
            crate::emit::literal(name)
        ))
        .await?;
    if listed.is_empty() {
        return Ok(None);
    }
    let rows = conn
        .query(&format!(
            "SELECT path::text AS path FROM pg_catalog.pg_extension_update_paths({}) \
              WHERE source = {} AND target = {}",
            crate::emit::literal(name),
            crate::emit::literal(from),
            crate::emit::literal(to)
        ))
        .await?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => Ok(row
            .try_get::<&str>("path")?
            .map(|path| path.split("--").map(str::to_owned).collect())),
        _ => Err(DbError::BadRow(format!(
            "extension `{}` has two update paths from `{}` to `{}`",
            Escaped(name),
            Escaped(from),
            Escaped(to)
        ))),
    }
}

/// Why each update in `updates` (name to the version held and the version
/// wanted) would fail or surprise on the target now, one sentence each
/// (DEC-2447.10): installed at the version the plan assumed, the engine has a
/// path, the target version is installable and its prerequisites are held, and
/// the connected role may run it. `creating` are the packages this plan
/// installs, which a prerequisite may be.
pub async fn update_problems(
    conn: &mut impl QueryConnection,
    updates: &BTreeMap<String, (Extension, Extension)>,
    creating: &BTreeMap<String, Extension>,
) -> Result<Vec<String>, DbError> {
    if updates.is_empty() {
        return Ok(Vec::new());
    }
    let target = target(conn).await?;
    let names: BTreeSet<String> = updates.keys().cloned().collect();
    let mut out = ownership_problems(conn, &names, "update").await?;
    let mut known = target.installed.clone();
    known.extend(creating.iter().map(|(n, d)| (n.clone(), d.clone())));
    for (name, (from, to)) in updates {
        if target.installed.get(name).is_some_and(|held| held != from) {
            out.push(format!(
                "extension `{}` is not installed as the plan assumed (it is `{}` in schema `{}`, not `{}` in schema `{}`)",
                Escaped(name),
                Escaped(&target.installed[name].version),
                Escaped(&target.installed[name].schema),
                Escaped(&from.version),
                Escaped(&from.schema)
            ));
        }
        let path = update_path(conn, name, &from.version, &to.version).await?;
        if path.is_none() {
            out.push(format!(
                "extension `{}` has no update path from version `{}` to `{}`; the engine applies only the versions the package ships scripts between, and pbps never guesses a downgrade or drops and recreates the package",
                Escaped(name),
                Escaped(&from.version),
                Escaped(&to.version)
            ));
        }
        let through = path
            .as_deref()
            .map(|path| path_requires(&target.available, name, path))
            .unwrap_or_default();
        out.extend(requirement_problems_through(
            "target",
            name,
            to,
            &target.available,
            &known,
            &through,
        ));
        out.extend(script_privilege(&target, name, to, "update", "updated"));
    }
    Ok(out)
}

/// The check that runs in the statement's own transaction *before* an
/// `ALTER EXTENSION ... UPDATE TO` (DEC-2447.10): the package stands as the
/// plan assumed, its members are the ones the plan was approved over, and the
/// engine would walk the pinned versions. What depends on the package from
/// outside it (a column default calling a member, a view, an object marked
/// `DEPENDS ON EXTENSION`) is remembered in a temporary table that ends with the
/// transaction, so that [`confirm_updated`] can prove each is still there: a
/// script that drops a member `CASCADE` takes its dependents with it, which the
/// engine does not refuse.
pub async fn vet_before_update(
    conn: &mut pbps_db::Conn,
    name: &str,
    pin: &pbps_model::ExtensionChangePin,
) -> Result<(), DbError> {
    let all = providers(conn).await?;
    let Some(actual) = all.get(name) else {
        return Err(DbError::Refused(format!(
            "extension `{}` is no longer installed",
            Escaped(name)
        )));
    };
    if actual.definition != pin.from {
        return Err(DbError::Refused(format!(
            "extension `{}` is `{}` in schema `{}`, not the `{}` in schema `{}` the plan was approved to update",
            Escaped(name),
            Escaped(&actual.definition.version),
            Escaped(&actual.definition.schema),
            Escaped(&pin.from.version),
            Escaped(&pin.from.schema)
        )));
    }
    let differences = pin.before.differences(actual);
    if !differences.is_empty() {
        return Err(DbError::Refused(format!(
            "extension `{}` is not what the plan was approved to update (its members changed since it was planned): {}",
            Escaped(name),
            differences.join("; ")
        )));
    }
    if update_path(conn, name, &pin.from.version, &pin.to.version)
        .await?
        .as_ref()
        != Some(&pin.path)
    {
        return Err(DbError::Refused(format!(
            "extension `{}` no longer updates from `{}` to `{}` by the versions the plan was approved over ({})",
            Escaped(name),
            Escaped(&pin.from.version),
            Escaped(&pin.to.version),
            pin.path.join(" -> ")
        )));
    }
    conn.execute(
        "CREATE TEMPORARY TABLE IF NOT EXISTS pg_temp.pbps_update_keep \
         (ext text, classid oid, objid oid, objsubid int4) ON COMMIT DROP",
    )
    .await?;
    conn.execute(&format!(
        "{} INSERT INTO pg_temp.pbps_update_keep \
         SELECT e.extname, d.classid, d.objid, d.objsubid FROM reach r \
           JOIN pg_catalog.pg_extension e ON e.oid = r.ext \
           JOIN pg_catalog.pg_depend d ON d.refclassid = r.classid AND d.refobjid = r.objid AND d.deptype = 'n' \
          WHERE e.extname = {name} \
            AND NOT EXISTS (SELECT 1 FROM reach o WHERE o.classid = d.classid AND o.objid = d.objid) \
         UNION \
         SELECT e.extname, d.classid, d.objid, d.objsubid FROM pg_catalog.pg_depend d \
           JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid \
          WHERE d.refclassid = 'pg_catalog.pg_extension'::regclass AND d.deptype = 'x' AND e.extname = {name}",
        reach!(),
        name = crate::emit::literal(name),
    ))
    .await?;
    Ok(())
}

/// The check right *after* the update, before its transaction commits: the
/// package is at the target version with the members the rehearsal read, and
/// every object that depended on it from outside is still there. Anything else
/// rolls the update back with the rest.
pub async fn confirm_updated(
    conn: &mut impl QueryConnection,
    name: &str,
    pin: &pbps_model::ExtensionChangePin,
) -> Result<(), DbError> {
    vet(conn, name, &pin.after).await?;
    let gone = conn
        .query(&format!(
            "SELECT count(*)::text AS n FROM pg_temp.pbps_update_keep t WHERE t.ext = {} \
             AND pg_catalog.pg_describe_object(t.classid, t.objid, t.objsubid) IS NULL",
            crate::emit::literal(name)
        ))
        .await?;
    let n = gone
        .first()
        .map(|r| text(r, "n"))
        .transpose()?
        .ok_or_else(|| DbError::BadRow("the update check read no count".into()))?;
    if n != "0" {
        return Err(DbError::Refused(format!(
            "{n} object(s) that depended on extension `{}` are gone after its update script ran",
            Escaped(name)
        )));
    }
    Ok(())
}

/// The vet that runs inside the transaction that installed a package: the
/// closure the engine now holds must be the one the plan was approved over.
pub async fn vet(
    conn: &mut impl QueryConnection,
    name: &str,
    expected: &ExtensionProvider,
) -> Result<(), DbError> {
    let all = providers(conn).await?;
    let Some(actual) = all.get(name) else {
        return Err(DbError::Refused(format!(
            "extension `{}` is absent after its statement",
            Escaped(name)
        )));
    };
    let differences = expected.differences(actual);
    if differences.is_empty() {
        return Ok(());
    }
    Err(DbError::Refused(format!(
        "extension `{}` does not match what was approved: {}",
        Escaped(name),
        differences.join("; ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(schema: Option<&str>, requires: &[&str]) -> Control {
        Control {
            installed: false,
            superuser: true,
            trusted: true,
            relocatable: true,
            schema: schema.map(str::to_owned),
            requires: requires.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    fn catalog() -> BTreeMap<(String, String), Control> {
        [
            (("citext".into(), "1.6".into()), control(None, &[])),
            (("citext".into(), "1.4".into()), control(None, &[])),
            (("fixed".into(), "1.0".into()), control(Some("fixed"), &[])),
            (
                ("earthdistance".into(), "1.1".into()),
                control(None, &["cube"]),
            ),
        ]
        .into()
    }

    fn wanted(schema: &str, version: &str) -> Extension {
        Extension {
            schema: schema.into(),
            version: version.into(),
        }
    }

    fn problems(name: &str, w: &Extension, known: &[&str]) -> Vec<String> {
        let known = known
            .iter()
            .map(|k| ((*k).to_owned(), wanted("ext", "1.0")))
            .collect();
        requirement_problems("target", name, w, &catalog(), &known)
    }

    #[test]
    fn an_installable_pinned_package_has_no_problems() {
        assert!(problems("citext", &wanted("ext", "1.6"), &[]).is_empty());
        assert!(problems("fixed", &wanted("fixed", "1.0"), &[]).is_empty());
        assert!(problems("earthdistance", &wanted("ext", "1.1"), &["cube"]).is_empty());
    }

    #[test]
    fn a_version_the_server_does_not_list_is_named_with_the_ones_it_does() {
        let found = problems("citext", &wanted("ext", "9.9"), &[]);
        assert_eq!(found.len(), 1);
        assert!(
            found[0].contains("`9.9`") && found[0].contains("`1.4`, `1.6`"),
            "{found:?}"
        );
        let found = problems("nosuch", &wanted("ext", "1.0"), &[]);
        assert!(found[0].contains("not available at all"), "{found:?}");
        // The default version is not a stand-in: an empty pin matches nothing.
        assert_eq!(problems("citext", &wanted("ext", ""), &[]).len(), 1);
    }

    #[test]
    fn a_fixed_schema_and_an_unpinned_prerequisite_are_each_refused() {
        let found = problems("fixed", &wanted("other", "1.0"), &[]);
        assert!(
            found[0].contains("installs only into schema `fixed`"),
            "{found:?}"
        );
        let found = problems("earthdistance", &wanted("ext", "1.1"), &[]);
        assert!(found[0].contains("requires extension `cube`"), "{found:?}");
        assert!(
            found[0].contains("never installs a prerequisite by default"),
            "{found:?}"
        );
        // Both facts are reported together, not one per attempt.
        let mut both = catalog();
        both.insert(
            ("both".into(), "1".into()),
            control(Some("fixed"), &["cube"]),
        );
        let found = requirement_problems(
            "scratch",
            "both",
            &wanted("other", "1"),
            &both,
            &BTreeMap::new(),
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| !f.contains("target")));
    }

    fn facts(superuser: bool, database_create: bool) -> Target {
        let mut available = catalog();
        // `superuser` and `trusted` as a control file spells them: the three
        // kinds the engine tells apart (measured on PostgreSQL 16.15 and 18.6).
        for (key, superuser, trusted) in [
            (("citext", "1.6"), true, true),
            (("fixed", "1.0"), true, false),
            (("earthdistance", "1.1"), false, false),
        ] {
            let c = available.get_mut(&(key.0.into(), key.1.into())).unwrap();
            c.superuser = superuser;
            c.trusted = trusted;
        }
        Target {
            installed: [("held".to_owned(), wanted("ext", "1.0"))].into(),
            available,
            schemas: ["ext".to_owned(), "fixed".to_owned()].into(),
            superuser,
            database_create,
        }
    }

    fn create(target: &Target, name: &str, w: &Extension) -> Vec<String> {
        create_problems(target, name, w, &BTreeMap::new())
    }

    #[test]
    fn what_the_connected_role_may_install_follows_the_control_file() {
        let (plain, creator) = (facts(false, false), facts(false, true));
        let (citext, fixed) = (wanted("ext", "1.6"), wanted("fixed", "1.0"));
        // A trusted package needs CREATE on the database, and only that.
        assert!(create(&creator, "citext", &citext).is_empty());
        let refused = create(&plain, "citext", &citext);
        assert!(refused[0].contains("CREATE on the database"), "{refused:?}");
        // An untrusted one that asks for a superuser is refused whatever else
        // the role holds; a superuser installs it.
        let refused = create(&creator, "fixed", &fixed);
        assert!(refused[0].contains("needs a superuser"), "{refused:?}");
        assert!(create(&facts(true, false), "fixed", &fixed).is_empty());
        // A package that does not ask for a superuser needs no privilege.
        assert!(create(&plain, "earthdistance", &wanted("ext", "1.1")).len() == 1);
        let mut easy = plain.clone();
        easy.installed.insert("cube".into(), wanted("ext", "1.0"));
        assert!(create(&easy, "earthdistance", &wanted("ext", "1.1")).is_empty());
    }

    #[test]
    fn an_installed_name_or_a_missing_schema_is_refused_before_a_statement() {
        let target = facts(true, true);
        let held = create(&target, "held", &wanted("ext", "1.0"));
        assert!(
            held.iter().any(|p| p.contains("already installed")),
            "{held:?}"
        );
        let absent = create(&target, "citext", &wanted("nowhere", "1.6"));
        assert_eq!(absent.len(), 1, "{absent:?}");
        assert!(absent[0].contains("does not have"), "{absent:?}");
        // The same facts with the schema and a free name have no problem.
        assert!(create(&target, "citext", &wanted("ext", "1.6")).is_empty());
        // A pinned prerequisite of the same plan is a known package, as an
        // installed one is.
        let pinned = [("cube".to_owned(), wanted("ext", "1.5"))].into();
        assert!(
            create_problems(&target, "earthdistance", &wanted("ext", "1.1"), &pinned).is_empty()
        );
    }

    /// A drop is exactly `DROP EXTENSION name RESTRICT` with the boundary the
    /// executor vets before and confirms after, and through the real differ and
    /// orderer it waits for what it must: the requirer of its package, and the
    /// module the plan drops that the package's removal would delete. The
    /// requirer sorts after it by name, so only the edge and the rank can put it first.
    #[test]
    fn a_drop_is_exactly_the_restrict_statement_and_waits_for_its_requirer_and_its_modules() {
        use pbps_model::{Change, IdsFile, Schema};
        let pg = crate::Postgres::new();
        let drop = Change::DropExtension {
            uid: "e_000001".parse().unwrap(),
            name: "odd \"name".into(),
            definition: wanted("ext\"s", "1.0"),
            requirers: Default::default(),
            releases: Default::default(),
        };
        let statements = crate::emit::emit(&pg, &drop, Default::default()).unwrap();
        assert_eq!(statements.len(), 1);
        assert_eq!(
            statements[0].sql,
            "DROP EXTENSION \"odd \"\"name\" RESTRICT;"
        );
        for forbidden in ["IF EXISTS", "CASCADE"] {
            assert!(!statements[0].sql.contains(forbidden));
        }
        let boundary = statements[0].extension.as_ref().unwrap();
        assert!(
            boundary.phase == pbps_dialect::ExtensionPhase::Drop
                && boundary.name.raw() == "odd \"name"
        );

        // `aaa` is required by `zzz` (dropped by the plan, sorts last), and a
        // function marked as depending on `aaa` goes with the plan.
        let held = Schema {
            extensions: [("aaa", "1.0"), ("zzz", "1.0")]
                .map(|(n, v)| (n.into(), wanted("ext", v)))
                .into(),
            modules: [(
                "app.marked()".parse().unwrap(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::Function,
                    description: None,
                    definition: "() RETURNS int LANGUAGE sql RETURN 1".into(),
                    options: Default::default(),
                },
            )]
            .into(),
            ..Default::default()
        };
        let held_ids = IdsFile {
            extensions: [("e_000001", "aaa"), ("e_000002", "zzz")]
                .map(|(u, n)| (u.parse().unwrap(), n.into()))
                .into(),
            ..Default::default()
        };
        let mut gone_ids = IdsFile::default();
        for (uid, name) in [("e_000001", "aaa"), ("e_000002", "zzz")] {
            gone_ids.tombstones.insert(
                uid.parse().unwrap(),
                pbps_model::Tombstone {
                    require: None,
                    was: name.into(),
                    dropped_at: "2026-11-02".into(),
                    reason: "retired".into(),
                    operator: "test".into(),
                },
            );
        }
        let gone = Schema::default();
        let facts = [
            (
                "aaa".into(),
                pbps_model::ExtensionDropFacts {
                    outside: [ProviderMember {
                        class: "function".into(),
                        names: vec!["app".into(), "marked".into()],
                        args: Vec::new(),
                    }]
                    .into(),
                    blockers: [ProviderMember {
                        class: "extension".into(),
                        names: vec!["zzz".into()],
                        args: Vec::new(),
                    }]
                    .into(),
                },
            ),
            ("zzz".into(), Default::default()),
        ]
        .into();
        let hints = pbps_model::Hints {
            extension_drops: facts,
            ..Default::default()
        };
        let planned = pbps_diff::diff(
            pbps_diff::Side {
                schema: &held,
                ids: &held_ids,
            },
            pbps_diff::Side {
                schema: &gone,
                ids: &gone_ids,
            },
            &pg,
            &hints,
        )
        .unwrap();
        // The differ's own order, which its order shadow holds to the effects
        // graph in debug builds, so a draft that disagreed with an edge would
        // have refused the plan here.
        let order: Vec<String> = planned
            .changes()
            .changes
            .iter()
            .map(|p| {
                if let Change::DropModule { id, .. } = &p.change {
                    format!("module {}", id.shown())
                } else if let Change::DropExtension { name, .. } = &p.change {
                    format!("package {}", name.shown())
                } else {
                    panic!("{:?}", p.change)
                }
            })
            .collect();
        // `zzz` sorts last and is dropped first: only the edge says so.
        assert_eq!(
            order,
            ["module app.marked()", "package zzz", "package aaa"],
            "{order:?}"
        );
    }

    #[test]
    fn a_create_is_exactly_the_pinned_statement_and_waits_for_its_prerequisite() {
        use pbps_model::{Change, IdsFile, Schema};
        let pg = crate::Postgres::new();
        // `aaa` sorts first by name and requires `zzz`: only the edge can put
        // `zzz` ahead, so the order is not an accident of the alphabet.
        let declared = Schema {
            extensions: [("aaa", "1.0"), ("zzz", "1.0"), ("quiet", "2.0")]
                .map(|(n, v)| (n.into(), wanted("ext", v)))
                .into(),
            ..Default::default()
        };
        let ids = IdsFile {
            extensions: [
                ("e_000001", "aaa"),
                ("e_000002", "zzz"),
                ("e_000003", "quiet"),
            ]
            .map(|(u, n)| (u.parse().unwrap(), n.into()))
            .into(),
            ..Default::default()
        };
        let empty = Schema::default();
        let none = IdsFile::default();
        let hints = pbps_model::Hints {
            extension_requires: [
                ("aaa".into(), ["zzz".into(), "installed".into()].into()),
                ("quiet".into(), Default::default()),
            ]
            .into(),
            ..Default::default()
        };
        let planned = pbps_diff::diff(
            pbps_diff::Side {
                schema: &empty,
                ids: &none,
            },
            pbps_diff::Side {
                schema: &declared,
                ids: &ids,
            },
            &pg,
            &hints,
        )
        .unwrap();
        let order: Vec<_> = planned
            .changes()
            .changes
            .iter()
            .filter_map(|p| {
                if let Change::CreateExtension { name, requires, .. } = &p.change {
                    Some((name.raw().to_owned(), requires.len()))
                } else {
                    None
                }
            })
            .collect();
        let at = |n: &str| order.iter().position(|(name, _)| name == n).unwrap();
        // One edge, to the prerequisite the plan creates; `installed` adds none.
        assert_eq!(order[at("aaa")].1, 1, "{order:?}");
        assert!(at("zzz") < at("aaa"), "{order:?}");
        // Negative: with no fact about a prerequisite the plan has no edge, and
        // the packages are in the differ's own order.
        let blind = pbps_diff::diff(
            pbps_diff::Side {
                schema: &empty,
                ids: &none,
            },
            pbps_diff::Side {
                schema: &declared,
                ids: &ids,
            },
            &pg,
            &pbps_model::Hints::default(),
        )
        .unwrap();
        assert!(blind.changes().changes.iter().all(|p| !matches!(
            &p.change,
            Change::CreateExtension { requires, .. } if !requires.is_empty()
        )));
        // The statement is the pin and nothing else, with the install boundary
        // the executor vets, and no option that would choose for the author.
        let create = Change::CreateExtension {
            uid: "e_000001".parse().unwrap(),
            name: "odd \"name".into(),
            definition: wanted("ext\"s", "1.0'x"),
            requires: Default::default(),
        };
        let statements = crate::emit::emit(&pg, &create, Default::default()).unwrap();
        assert_eq!(statements.len(), 1);
        assert_eq!(
            statements[0].sql,
            "CREATE EXTENSION \"odd \"\"name\" SCHEMA \"ext\"\"s\" VERSION '1.0''x';"
        );
        for forbidden in ["IF NOT EXISTS", "CASCADE"] {
            assert!(!statements[0].sql.contains(forbidden));
        }
        let boundary = statements[0].extension.as_ref().unwrap();
        assert_eq!(boundary.name.raw(), "odd \"name");
        assert!(statements[0].declared_type.is_none());
    }

    #[test]
    fn a_name_with_a_line_break_cannot_forge_a_line() {
        let found = problems("a\nforged", &wanted("ext", "1.0"), &[]);
        assert!(!found[0].contains('\n'), "{found:?}");
    }

    /// An update is exactly `ALTER EXTENSION name UPDATE TO 'version'` with the
    /// boundary the executor vets before and after, and through the real differ
    /// and orderer it waits for the package of its plan that its target version
    /// requires and for the module its plan drops that uses a member of it. The
    /// prerequisite sorts after it by name, so only the edge and the rank can put
    /// it first.
    #[test]
    fn an_update_is_exactly_the_named_version_statement_and_waits_for_its_prerequisite_and_modules()
    {
        use pbps_model::{Change, IdsFile, Schema};
        let pg = crate::Postgres::new();
        let update = Change::UpdateExtension {
            uid: "e_000001".parse().unwrap(),
            name: "odd \"name".into(),
            from: wanted("ext\"s", "1.0"),
            to: wanted("ext\"s", "1.0'x"),
            requires: Default::default(),
            releases: Default::default(),
        };
        let statements = crate::emit::emit(&pg, &update, Default::default()).unwrap();
        assert_eq!(statements.len(), 1);
        assert_eq!(
            statements[0].sql,
            "ALTER EXTENSION \"odd \"\"name\" UPDATE TO '1.0''x';"
        );
        for forbidden in ["CASCADE", "IF EXISTS", "SET SCHEMA", "DROP"] {
            assert!(!statements[0].sql.contains(forbidden));
        }
        let boundary = statements[0].extension.as_ref().unwrap();
        assert_eq!(
            boundary.phase,
            pbps_dialect::ExtensionPhase::Update {
                from: wanted("ext\"s", "1.0")
            }
        );

        let base = Schema {
            extensions: [("aaa".into(), wanted("ext", "1.0"))].into(),
            modules: [(
                "app.marked()".parse().unwrap(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::Function,
                    description: None,
                    definition: "() RETURNS int LANGUAGE sql RETURN 1".into(),
                    options: Default::default(),
                },
            )]
            .into(),
            ..Default::default()
        };
        let desired = Schema {
            extensions: [("aaa", "1.1"), ("zzz", "1.0")]
                .map(|(n, v)| (n.into(), wanted("ext", v)))
                .into(),
            ..Default::default()
        };
        let ids = |both: bool| IdsFile {
            extensions: [("e_000001", "aaa"), ("e_000002", "zzz")]
                .into_iter()
                .take(if both { 2 } else { 1 })
                .map(|(u, n)| (u.parse().unwrap(), n.into()))
                .collect(),
            ..Default::default()
        };
        let facts = pbps_model::ExtensionUpdateFacts {
            requires: ["zzz".into()].into(),
            reach: pbps_model::ExtensionDropFacts {
                outside: Default::default(),
                blockers: [ProviderMember {
                    class: "function".into(),
                    names: vec!["app".into(), "marked".into()],
                    args: Vec::new(),
                }]
                .into(),
            },
        };
        let hints = pbps_model::Hints {
            extension_updates: [("aaa".into(), facts)].into(),
            extension_requires: [("zzz".into(), Default::default())].into(),
            ..Default::default()
        };
        let planned = pbps_diff::diff(
            pbps_diff::Side {
                schema: &base,
                ids: &ids(false),
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &ids(true),
            },
            &pg,
            &hints,
        )
        .unwrap();
        // The differ's own order, which its order shadow holds to the effects
        // graph in debug builds, so a draft that disagreed with an edge would
        // have refused the plan here.
        let order: Vec<String> = planned
            .changes()
            .changes
            .iter()
            .filter_map(|p| {
                if let Change::DropModule { id, .. } = &p.change {
                    Some(format!("drop module {}", id.shown()))
                } else if let Change::UpdateExtension { name, to, .. } = &p.change {
                    Some(format!("update {} to {}", name.shown(), to.version))
                } else if let Change::CreateExtension { name, .. } = &p.change {
                    Some(format!("create {}", name.shown()))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(planned.changes().changes.len(), order.len(), "{order:?}");
        assert_eq!(
            order,
            [
                "drop module app.marked()",
                "create zzz",
                "update aaa to 1.1"
            ]
        );
        let Change::UpdateExtension {
            requires, releases, ..
        } = &planned
            .changes()
            .changes
            .iter()
            .map(|p| &p.change)
            .find(|c| matches!(c, Change::UpdateExtension { .. }))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!((requires.len(), releases.len()), (1, 1));

        // Negative: with no facts (offline) the update is refused, not planned
        // blind, and an unchanged version plans nothing.
        let offline = pbps_diff::diff(
            pbps_diff::Side {
                schema: &base,
                ids: &ids(false),
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &ids(true),
            },
            &pg,
            &pbps_model::Hints::default(),
        )
        .unwrap_err();
        assert!(
            offline
                .iter()
                .any(|e| e.to_string().contains("only against a database")),
            "{offline:?}"
        );
    }

    /// What may update a package follows the control file of the version it
    /// goes to, the same rule as an install (measured on 16.15 and 18.6).
    /// An update is held to the prerequisites of every version it walks, not
    /// only the one it ends at: a version in the middle that requires a package
    /// the final one does not still fails without it (measured on 16.15 and 18.6).
    #[test]
    fn an_update_is_held_to_the_prerequisites_of_every_version_on_its_path() {
        let mut available = catalog();
        for (version, requires) in [("1.0", &[][..]), ("1.1", &["cube"][..]), ("1.2", &[][..])] {
            available.insert(
                ("mid".to_owned(), version.to_owned()),
                control(None, requires),
            );
        }
        let path = |versions: &[&str]| -> Vec<String> {
            versions.iter().map(|v| (*v).to_owned()).collect()
        };
        // The union, from the first version to the last.
        assert_eq!(
            path_requires(&available, "mid", &path(&["1.0", "1.1", "1.2"])),
            ["cube".to_owned()].into()
        );
        // Negatives: a path that skips the middle, a version nothing lists, and
        // another package's versions add nothing.
        assert!(path_requires(&available, "mid", &path(&["1.0", "1.2"])).is_empty());
        assert!(path_requires(&available, "mid", &path(&["1.0", "1.5", "1.2"])).is_empty());
        assert!(path_requires(&available, "other", &path(&["1.0", "1.1"])).is_empty());
        // Judged: the final version needs nothing, yet the middle one's
        // prerequisite is refused by name unless the target holds or creates it.
        let wanted = wanted("ext", "1.2");
        let through = path_requires(&available, "mid", &path(&["1.0", "1.1", "1.2"]));
        let bare = BTreeMap::new();
        let found =
            requirement_problems_through("target", "mid", &wanted, &available, &bare, &through);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("`mid`") && found[0].contains("requires extension `cube`"),
            "{found:?}"
        );
        assert!(requirement_problems("target", "mid", &wanted, &available, &bare).is_empty());
        let known: BTreeMap<String, Extension> =
            [("cube".to_owned(), self::wanted("ext", "1.5"))].into();
        assert!(
            requirement_problems_through("target", "mid", &wanted, &available, &known, &through)
                .is_empty()
        );
    }

    #[test]
    fn what_the_connected_role_may_update_follows_the_target_versions_control_file() {
        let (plain, creator) = (facts(false, false), facts(false, true));
        let (citext, fixed) = (wanted("ext", "1.6"), wanted("fixed", "1.0"));
        let ask =
            |t: &Target, n: &str, v: &Extension| script_privilege(t, n, v, "update", "updated");
        assert!(ask(&creator, "citext", &citext).is_none());
        let refused = ask(&plain, "citext", &citext).unwrap();
        assert!(
            refused.contains("is updated by a role with CREATE on the database"),
            "{refused}"
        );
        let refused = ask(&creator, "fixed", &fixed).unwrap();
        assert!(
            refused.contains("update script needs a superuser"),
            "{refused}"
        );
        assert!(ask(&facts(true, false), "fixed", &fixed).is_none());
        // A package that asks for no superuser needs nothing, and a version the
        // server does not list is for `requirement_problems` to name.
        assert!(ask(&plain, "earthdistance", &wanted("ext", "1.1")).is_none());
        assert!(ask(&plain, "nosuch", &wanted("ext", "1.0")).is_none());
    }

    /// A database of its own on each pinned server, so no other test sees the
    /// package. Returns the admin connection, the database name and a
    /// connection to it.
    async fn fresh(variable: &str, tag: &str) -> (pbps_db::Conn, String, pbps_db::Conn) {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        let mut admin = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &base)
            .await
            .unwrap();
        let db = format!(
            "pbps_ext2447_{tag}_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        admin
            .execute(&format!("CREATE DATABASE {db} TEMPLATE template0"))
            .await
            .unwrap();
        let conn =
            pbps_db::Conn::connect(pbps_db::Driver::Postgres, &format!("{base} dbname={db}"))
                .await
                .unwrap();
        (admin, db, conn)
    }

    async fn discard(mut admin: pbps_db::Conn, db: String, conn: pbps_db::Conn) {
        drop(conn);
        admin
            .execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn the_engine_reports_what_is_installable_and_what_a_package_owns() {
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let (admin, db, mut conn) = fresh(variable, "owns").await;
            conn.execute("CREATE SCHEMA ext").await.unwrap();
            conn.execute("CREATE EXTENSION citext SCHEMA ext VERSION '1.4'")
                .await
                .unwrap();
            // A user object in the package's schema and a user routine that
            // merely depends on it are not members.
            conn.execute("CREATE TABLE ext.plain (id integer)")
                .await
                .unwrap();
            conn.execute(
                "CREATE FUNCTION public.mine() RETURNS integer LANGUAGE sql AS 'SELECT 1'",
            )
            .await
            .unwrap();
            conn.execute("ALTER FUNCTION public.mine() DEPENDS ON EXTENSION citext")
                .await
                .unwrap();
            let all = providers(&mut conn).await.unwrap();
            let citext = &all["citext"];
            assert_eq!(citext.definition, wanted("ext", "1.4"));
            assert!(citext.requires.is_empty());
            let has = |class: &str, name: &str| {
                citext
                    .members
                    .iter()
                    .any(|m| m.class == class && m.names.last().map(String::as_str) == Some(name))
            };
            assert!(has("type", "ext.citext"), "{:?}", citext.members);
            // The array type is an internal subsidiary of its element type; the
            // engine renders a type as one qualified string.
            assert!(has("type", "ext.citext[]"), "{:?}", citext.members);
            assert!(!has("table", "plain") && !has("function", "mine"));
            // plpgsql is installed everywhere and listed like any other.
            assert_eq!(all["plpgsql"].definition.schema, "pg_catalog");
            // Reading twice is stable, so a manifest compares across servers.
            assert_eq!(providers(&mut conn).await.unwrap()["citext"], *citext);
            assert_eq!(
                installed(&mut conn).await.unwrap()["citext"],
                wanted("ext", "1.4")
            );
            let versions = available(&mut conn).await.unwrap();
            assert!(versions[&("citext".into(), "1.4".into())].installed);
            assert!(!versions[&("citext".into(), "1.6".into())].installed);
            // Negative: a version the server does not ship is not listed.
            assert!(!versions.contains_key(&("citext".into(), "definitely-not-shipped".into())));
            discard(admin, db, conn).await;
        }
    }

    /// The name of the TOAST table of `ext.owned`.
    async fn toast_name(conn: &mut pbps_db::Conn) -> String {
        let rows = conn
            .query(
                "SELECT t.relname::text AS name FROM pg_catalog.pg_class t \
                 JOIN pg_catalog.pg_class o ON o.reltoastrelid = t.oid \
                 WHERE o.oid = 'ext.owned'::regclass",
            )
            .await
            .unwrap();
        rows[0].try_get::<&str>("name").unwrap().unwrap().to_owned()
    }

    /// The physical storage an owned table brings (its TOAST table and index)
    /// is named after an object number of the database it is in, so it must not
    /// be part of what two databases are compared on.
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn a_member_table_with_toast_storage_compares_equal_across_databases() {
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let (admin, db, mut target) = fresh(variable, "toast").await;
            let (admin2, db2, mut other) = fresh(variable, "toastother").await;
            // Objects the first database holds and the second does not, so the
            // two number their tables differently.
            for n in 0..3 {
                target
                    .execute(&format!("CREATE TABLE public.shift{n} (id integer)"))
                    .await
                    .unwrap();
            }
            for conn in [&mut target, &mut other] {
                conn.execute("CREATE SCHEMA ext").await.unwrap();
                conn.execute("CREATE EXTENSION citext SCHEMA ext VERSION '1.4'")
                    .await
                    .unwrap();
                // A package-owned table with a toastable column: the same
                // `e` edge on the table and `i` edge to its TOAST table that a
                // package shipping one has.
                conn.execute("CREATE TABLE ext.owned (id integer PRIMARY KEY, body text)")
                    .await
                    .unwrap();
                conn.execute("ALTER EXTENSION citext ADD TABLE ext.owned")
                    .await
                    .unwrap();
            }
            // The premise: the storage names really do differ, so equal
            // manifests mean the names are not read.
            assert_ne!(toast_name(&mut target).await, toast_name(&mut other).await);
            let first = providers(&mut target).await.unwrap()["citext"].clone();
            let second = providers(&mut other).await.unwrap()["citext"].clone();
            let owned = |p: &ExtensionProvider| {
                p.members
                    .iter()
                    .any(|m| m.class == "table" && m.names == ["ext", "owned"])
            };
            assert!(owned(&first) && owned(&second), "{:?}", first.members);
            assert!(
                first
                    .members
                    .iter()
                    .all(|m| !m.names.iter().any(|n| n.contains("pg_toast"))),
                "{:?}",
                first.members
            );
            assert_eq!(first, second);
            vet(&mut target, "citext", &second).await.unwrap();

            // Negative: dropping the storage filter must not hide a real
            // difference. One more owned table in one database is named.
            other
                .execute("CREATE TABLE ext.extra (id integer, body text)")
                .await
                .unwrap();
            other
                .execute("ALTER EXTENSION citext ADD TABLE ext.extra")
                .await
                .unwrap();
            let grown = providers(&mut other).await.unwrap()["citext"].clone();
            assert_ne!(first, grown);
            let error = vet(&mut target, "citext", &grown).await.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("missing members: `table ext.extra`"),
                "{error}"
            );
            discard(admin, db, target).await;
            discard(admin2, db2, other).await;
        }
    }

    /// The rule `create_problems` applies to the connected role is what the
    /// engine does, for each kind of control file, with and without `CREATE` on
    /// the database. A rule that said yes where the engine says no would plan an
    /// install that fails at apply; one that said no would refuse a valid plan.
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn the_privilege_rule_is_what_the_engine_does_for_each_kind_of_control_file() {
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
            let (mut admin, db, mut owner) = fresh(variable, "priv").await;
            let role = format!("pbps_priv_{}", db.rsplit('_').next().unwrap());
            admin
                .execute(&format!(
                    "CREATE ROLE {role} LOGIN PASSWORD 'priv' NOSUPERUSER NOCREATEDB; \
                     GRANT CONNECT ON DATABASE {db} TO {role}"
                ))
                .await
                .unwrap();
            owner
                .execute(&format!("CREATE SCHEMA ext AUTHORIZATION {role}"))
                .await
                .unwrap();
            // The three kinds: asks for no superuser, trusted, and untrusted
            // with a superuser (a contrib package that is neither).
            let packages = ["nonsu2447", "citext", "file_fdw"];
            let mut agreed = 0;
            for create_on_database in [false, true] {
                admin
                    .execute(&format!(
                        "{} CREATE ON DATABASE {db} {} {role}",
                        if create_on_database {
                            "GRANT"
                        } else {
                            "REVOKE"
                        },
                        if create_on_database { "TO" } else { "FROM" }
                    ))
                    .await
                    .unwrap();
                let words: Vec<_> = base
                    .split_whitespace()
                    .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
                    .collect();
                let as_role = format!("{} user={role} password=priv dbname={db}", words.join(" "));
                let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &as_role)
                    .await
                    .unwrap();
                let facts = target(&mut conn).await.unwrap();
                assert!(!facts.superuser);
                assert_eq!(facts.database_create, create_on_database);
                for name in packages {
                    let version = facts
                        .available
                        .keys()
                        .filter(|(n, _)| n == name)
                        .map(|(_, v)| v.clone())
                        .next_back()
                        .unwrap();
                    let wanted = Extension {
                        schema: "ext".into(),
                        version,
                    };
                    let predicted = create_problems(&facts, name, &wanted, &BTreeMap::new());
                    let tried = conn
                        .execute(&format!(
                            "CREATE EXTENSION {name} SCHEMA ext VERSION '{}'",
                            wanted.version
                        ))
                        .await;
                    assert_eq!(
                        predicted.is_empty(),
                        tried.is_ok(),
                        "{variable} {name} database CREATE {create_on_database}: \
                         rule {predicted:?}, engine {tried:?}"
                    );
                    if tried.is_ok() {
                        conn.execute(&format!("DROP EXTENSION {name}"))
                            .await
                            .unwrap();
                    }
                    agreed += 1;
                }
            }
            assert_eq!(agreed, 6);
            // Negative: a superuser is never refused for privilege, whatever
            // the control file says.
            let facts = target(&mut owner).await.unwrap();
            assert!(facts.superuser);
            let untrusted = facts
                .available
                .keys()
                .filter(|(n, _)| n == "file_fdw")
                .map(|(_, v)| v.clone())
                .next_back()
                .unwrap();
            let wanted = Extension {
                schema: "ext".into(),
                version: untrusted,
            };
            assert!(create_problems(&facts, "file_fdw", &wanted, &BTreeMap::new()).is_empty());
            admin
                .execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
                .await
                .unwrap();
            admin.execute(&format!("DROP OWNED BY {role}")).await.ok();
            admin.execute(&format!("DROP ROLE {role}")).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn a_closure_vets_across_databases_and_prerequisites_are_read_not_guessed() {
        use crate::resolver::scratch::{extension_manifests, extension_requirements};
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let (admin, db, mut target) = fresh(variable, "vet").await;
            let (admin2, db2, mut scratch) = fresh(variable, "vetscratch").await;
            for conn in [&mut target, &mut scratch] {
                conn.execute("CREATE SCHEMA ext").await.unwrap();
                conn.execute("SET search_path = ext, public").await.unwrap();
            }
            // The control file says what `CREATE` needs; the catalog agrees
            // once installed, so the two reads are one fact.
            let versions = available(&mut target).await.unwrap();
            let earth = versions
                .iter()
                .find(|((name, _), _)| name == "earthdistance")
                .expect("earthdistance ships with the image");
            assert_eq!(earth.1.requires, ["cube".to_owned()].into());
            let name: pbps_model::ObjectLocalName = "earthdistance".into();
            let pin = wanted("ext", &earth.0.1);
            let alone = [(name.clone(), pin.clone())].into();
            let missing = extension_requirements(&mut scratch, &alone).await.unwrap();
            assert_eq!(missing.len(), 1, "{missing:?}");
            assert!(
                missing[0].contains("requires extension `cube`"),
                "{missing:?}"
            );
            let cube_version = versions
                .keys()
                .find(|(name, _)| name == "cube")
                .map(|(_, version)| version.clone())
                .expect("cube ships with the image");
            let both = [
                (name.clone(), pin.clone()),
                ("cube".into(), wanted("ext", &cube_version)),
            ]
            .into();
            assert!(
                extension_requirements(&mut scratch, &both)
                    .await
                    .unwrap()
                    .is_empty()
            );
            // Negative: a version the scratch cannot install is named.
            let bad = [(name.clone(), wanted("ext", "0.0-nope"))].into();
            let found = extension_requirements(&mut scratch, &bad).await.unwrap();
            assert!(
                found[0].contains("not installable on the scratch"),
                "{found:?}"
            );

            let install = format!(
                "CREATE EXTENSION cube SCHEMA ext VERSION '{cube_version}'; \
                 CREATE EXTENSION earthdistance SCHEMA ext VERSION '{}'",
                earth.0.1
            );
            target.execute(&install).await.unwrap();
            let approved = providers(&mut target).await.unwrap()["earthdistance"].clone();
            assert_eq!(approved.requires, ["cube".to_owned()].into());
            // The scratch is captured after the same statements ran there.
            assert!(extension_manifests(&mut scratch, &[&name]).await.is_err());
            scratch.execute(&install).await.unwrap();
            let captured = extension_manifests(&mut scratch, &[&name]).await.unwrap();
            assert_eq!(captured[&name], approved);
            vet(&mut target, "earthdistance", &captured[&name])
                .await
                .unwrap();
            // Negative: any difference in what was approved refuses.
            let mut other = approved.clone();
            other.definition.version.push('x');
            other.members.insert(ProviderMember {
                class: "function".into(),
                names: vec!["ext".into(), "forged".into()],
                args: Vec::new(),
            });
            let error = vet(&mut target, "earthdistance", &other).await.unwrap_err();
            let text = error.to_string();
            assert!(
                text.contains("became") && text.contains("missing member: `function ext.forged`"),
                "{text}"
            );
            let error = vet(&mut target, "nosuch", &approved).await.unwrap_err();
            assert!(error.to_string().contains("absent after its statement"));
            discard(admin, db, target).await;
            discard(admin2, db2, scratch).await;
        }
    }

    /// Three packages and the objects that hang off them in each of the ways
    /// the engine follows when it drops one: `x` marks of every kind, a user
    /// index on a member table, a table added to a package, and normal
    /// dependents that make the drop refuse.
    const DROP_WORLD: &str = "CREATE SCHEMA ext; CREATE SCHEMA app; \
        CREATE EXTENSION zzz_base SCHEMA ext; CREATE EXTENSION aaa_dep SCHEMA ext; \
        CREATE EXTENSION tabled2447 SCHEMA ext; \
        CREATE TABLE app.t (id int, b text); \
        CREATE FUNCTION app.x_fn(a int, b text) RETURNS int LANGUAGE sql AS 'SELECT 1'; \
        ALTER FUNCTION app.x_fn(int, text) DEPENDS ON EXTENSION zzz_base; \
        CREATE PROCEDURE app.x_proc() LANGUAGE sql AS 'SELECT 1'; \
        ALTER PROCEDURE app.x_proc() DEPENDS ON EXTENSION zzz_base; \
        CREATE FUNCTION app.trg() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'; \
        CREATE TRIGGER x_trg BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.trg(); \
        ALTER TRIGGER x_trg ON app.t DEPENDS ON EXTENSION zzz_base; \
        CREATE MATERIALIZED VIEW app.x_mv AS SELECT 1 AS one; \
        ALTER MATERIALIZED VIEW app.x_mv DEPENDS ON EXTENSION zzz_base; \
        CREATE INDEX x_idx ON app.t (id); ALTER INDEX app.x_idx DEPENDS ON EXTENSION zzz_base; \
        CREATE INDEX plain_idx ON app.t (b); \
        CREATE INDEX on_member ON ext.tabled2447_t (body); \
        CREATE TABLE app.born (id int PRIMARY KEY); ALTER EXTENSION tabled2447 ADD TABLE app.born; \
        CREATE TABLE app.uses (n int DEFAULT ext.zzz_base_one()); \
        CREATE VIEW app.v_uses AS SELECT ext.zzz_base_one() AS one";

    /// Whether `set` holds an object of this class and name. The arguments are
    /// not compared: how a built-in type is spelled in them differs by engine.
    fn has(set: &BTreeSet<ProviderMember>, class: &str, names: &[&str]) -> bool {
        set.iter().any(|m| {
            m.class == class && m.names.iter().map(String::as_str).eq(names.iter().copied())
        })
    }

    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn a_drop_reaches_marked_dependents_and_is_blocked_by_normal_ones() {
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let (admin, db, mut conn) = fresh(variable, "dropreach").await;
            conn.execute(DROP_WORLD).await.unwrap();
            let all = providers(&mut conn).await.unwrap();
            let facts = drop_facts(&mut conn, &all).await.unwrap();
            let base = &facts["zzz_base"];
            // Everything marked `DEPENDS ON EXTENSION`, of every class, is
            // outside the package and would be deleted with it.
            for (class, names) in [
                ("function", &["app", "x_fn"][..]),
                ("procedure", &["app", "x_proc"][..]),
                ("trigger", &["app", "t", "x_trg"][..]),
                ("materialized view", &["app", "x_mv"][..]),
                ("index", &["app", "x_idx"][..]),
            ] {
                assert!(
                    has(&base.outside, class, names),
                    "{variable} {class}: {:?}",
                    base.outside
                );
            }
            // Negative: an unmarked index, the member function and the
            // objects that merely depend on it are not outside deletions.
            assert!(!has(&base.outside, "index", &["app", "plain_idx"]));
            assert!(!has(&base.outside, "function", &["ext", "zzz_base_one"]));
            assert!(
                base.outside
                    .iter()
                    .all(|m| m.names.first().map(String::as_str) != Some("uses"))
            );
            // A default and a view that use a member function, and the package
            // that requires this one, make the engine refuse.
            assert!(
                has(&base.blockers, "default value", &["app", "uses", "n"]),
                "{:?}",
                base.blockers
            );
            assert!(
                has(&base.blockers, "rule", &["app", "v_uses", "_RETURN"]),
                "{:?}",
                base.blockers
            );
            assert!(
                has(&base.blockers, "extension", &["aaa_dep"]),
                "{:?}",
                base.blockers
            );
            // The package that nothing depends on has neither.
            assert_eq!(
                facts["aaa_dep"],
                ExtensionDropFacts::default(),
                "{variable}"
            );
            // A package with a member table, its TOAST storage, an index a user
            // put on it and a table a user added to it: all members of the
            // manifest, so none is outside, and nothing blocks the drop.
            let tabled = &facts["tabled2447"];
            assert_eq!(
                *tabled,
                ExtensionDropFacts::default(),
                "{variable}: {tabled:?}"
            );
            for (class, names) in [
                ("table", &["app", "born"][..]),
                ("index", &["ext", "on_member"][..]),
            ] {
                assert!(
                    has(
                        &all["tabled2447"].members.iter().cloned().collect(),
                        class,
                        names
                    ),
                    "{variable} {class}"
                );
            }
            discard(admin, db, conn).await;
        }
    }

    /// The drop is vetted before it runs and confirmed after, each against what
    /// the plan was approved over (DEC-2447.8).
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn a_drop_is_vetted_before_it_runs_and_confirmed_after_it_ran() {
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let (admin, db, mut conn) = fresh(variable, "dropvet").await;
            conn.execute(DROP_WORLD).await.unwrap();
            // Settle the world so `tabled2447` has nothing outside it, and
            // seal what the plan would.
            let pin_of = |all: &BTreeMap<String, ExtensionProvider>,
                          facts: &mut BTreeMap<String, ExtensionDropFacts>,
                          name: &str| pbps_model::ExtensionDropPin {
                uid: "e_aaaaaa".parse().unwrap(),
                name: name.into(),
                definition: all[name].definition.clone(),
                manifest: all[name].clone(),
                outside: facts.remove(name).unwrap().outside,
            };
            let all = providers(&mut conn).await.unwrap();
            let mut facts = drop_facts(&mut conn, &all).await.unwrap();
            let pin = pin_of(&all, &mut facts, "tabled2447");

            // Confirmed before the drop ran: the package is still there.
            conn.execute("BEGIN").await.unwrap();
            vet_before_drop(&mut conn, "tabled2447", &pin)
                .await
                .unwrap();
            let error = confirm_dropped(&mut conn, "tabled2447").await.unwrap_err();
            assert!(error.to_string().contains("still installed"), "{error}");
            conn.execute("DROP EXTENSION tabled2447 RESTRICT")
                .await
                .unwrap();
            confirm_dropped(&mut conn, "tabled2447").await.unwrap();
            conn.execute("ROLLBACK").await.unwrap();

            // An object that survives the statement is named: the package is
            // gone but a table sealed into its reach is not (here it was taken
            // out of the package first, so the drop leaves it standing).
            conn.execute("BEGIN").await.unwrap();
            vet_before_drop(&mut conn, "tabled2447", &pin)
                .await
                .unwrap();
            conn.execute("ALTER EXTENSION tabled2447 DROP TABLE ext.tabled2447_t")
                .await
                .unwrap();
            conn.execute("DROP EXTENSION tabled2447 RESTRICT")
                .await
                .unwrap();
            let error = confirm_dropped(&mut conn, "tabled2447").await.unwrap_err();
            assert!(
                error.to_string().contains("still there after its drop"),
                "{variable}: {error}"
            );
            conn.execute("ROLLBACK").await.unwrap();

            // A member added since the plan stops it before the statement.
            conn.execute(
                "CREATE TABLE app.later (id int); ALTER EXTENSION tabled2447 ADD TABLE app.later",
            )
            .await
            .unwrap();
            conn.execute("BEGIN").await.unwrap();
            let error = vet_before_drop(&mut conn, "tabled2447", &pin)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("members changed")
                    && error.to_string().contains("app.later"),
                "{variable}: {error}"
            );
            conn.execute("ROLLBACK").await.unwrap();
            conn.execute("ALTER EXTENSION tabled2447 DROP TABLE app.later")
                .await
                .unwrap();

            // An object marked as depending on it since the plan is not one it
            // accepted.
            conn.execute(
                "CREATE FUNCTION app.newcomer() RETURNS int LANGUAGE sql AS 'SELECT 1'; \
                 ALTER FUNCTION app.newcomer() DEPENDS ON EXTENSION tabled2447",
            )
            .await
            .unwrap();
            conn.execute("BEGIN").await.unwrap();
            let error = vet_before_drop(&mut conn, "tabled2447", &pin)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("did not accept")
                    && error.to_string().contains("newcomer"),
                "{variable}: {error}"
            );
            conn.execute("ROLLBACK").await.unwrap();
            // ... but one the plan accepted (and removed first) is fine, and a
            // newcomer already gone is no problem.
            let mut accepting = pin.clone();
            accepting.outside.insert(ProviderMember {
                class: "function".into(),
                names: vec!["app".into(), "newcomer".into()],
                args: Vec::new(),
            });
            conn.execute("BEGIN").await.unwrap();
            vet_before_drop(&mut conn, "tabled2447", &accepting)
                .await
                .unwrap();
            conn.execute("ROLLBACK").await.unwrap();
            conn.execute("DROP FUNCTION app.newcomer()").await.unwrap();

            // A dependent that would make the engine refuse is named first.
            let blocked = pin_of(&all, &mut facts, "zzz_base");
            conn.execute("BEGIN").await.unwrap();
            let error = vet_before_drop(&mut conn, "zzz_base", &blocked)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("would refuse to drop"),
                "{variable}: {error}"
            );
            conn.execute("ROLLBACK").await.unwrap();

            // A package already gone is named, and the transaction is not left
            // holding a half-built check.
            conn.execute("BEGIN").await.unwrap();
            let error = vet_before_drop(&mut conn, "nosuch", &pin)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("no longer installed"), "{error}");
            conn.execute("ROLLBACK").await.unwrap();
            discard(admin, db, conn).await;
        }
    }

    /// `DROP EXTENSION` needs ownership of the package and nothing else: the
    /// rule `drop_problems` states is what the engine does for a superuser, the
    /// owner, a member that inherits the owner's privileges, one that does not,
    /// and a stranger (measured on 16.15 and 18.6).
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn the_connected_role_may_drop_what_it_owns_and_nothing_else() {
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let (mut admin, db, mut conn) = fresh(variable, "dropown").await;
            let tag = db.rsplit('_').next().unwrap().to_owned();
            let [own, member, loose, stranger] =
                ["own", "member", "loose", "stranger"].map(|r| format!("pbps_drop_{r}_{tag}"));
            admin
                .execute(&format!(
                    "CREATE ROLE {own} NOLOGIN; CREATE ROLE {member} NOLOGIN; \
                     CREATE ROLE {loose} NOLOGIN; CREATE ROLE {stranger} NOLOGIN; \
                     GRANT {own} TO {member}; GRANT {own} TO {loose} WITH INHERIT FALSE"
                ))
                .await
                .unwrap();
            conn.execute(&format!(
                "CREATE SCHEMA ext; GRANT ALL ON SCHEMA ext TO {own}; GRANT CREATE ON DATABASE {db} TO {own}; \
                 SET ROLE {own}; CREATE EXTENSION nonsu2447 SCHEMA ext; RESET ROLE"
            ))
            .await
            .unwrap();
            let names: BTreeSet<String> = ["nonsu2447".to_owned()].into();
            // The superuser connection first.
            assert!(drop_problems(&mut conn, &names).await.unwrap().is_empty());
            for (role, allowed) in [
                (&own, true),
                (&member, true),
                (&loose, false),
                (&stranger, false),
            ] {
                conn.execute(&format!("SET ROLE {role}")).await.unwrap();
                let problems = drop_problems(&mut conn, &names).await.unwrap();
                conn.execute("BEGIN").await.unwrap();
                let tried = conn.execute("DROP EXTENSION nonsu2447 RESTRICT").await;
                conn.execute("ROLLBACK").await.unwrap();
                conn.execute("RESET ROLE").await.unwrap();
                assert_eq!(
                    problems.is_empty(),
                    allowed,
                    "{variable} {role}: {problems:?}"
                );
                assert_eq!(
                    tried.is_ok(),
                    allowed,
                    "{variable} {role}: the engine said {tried:?}"
                );
                if !allowed {
                    assert!(
                        problems[0].contains("42501") && problems[0].contains(&own),
                        "{problems:?}"
                    );
                }
            }
            // Negative: a package that is not installed is named.
            let missing: BTreeSet<String> = ["absent2447".to_owned()].into();
            let problems = drop_problems(&mut conn, &missing).await.unwrap();
            assert!(problems[0].contains("not installed"), "{problems:?}");
            drop(conn);
            admin
                .execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
                .await
                .unwrap();
            admin
                .execute(&format!(
                    "DROP OWNED BY {own}, {member}, {loose}, {stranger}"
                ))
                .await
                .ok();
            admin
                .execute(&format!("DROP ROLE {own}, {member}, {loose}, {stranger}"))
                .await
                .unwrap();
        }
    }
    /// A pin for `name`, `from` to `to`, built the way the planner does: the
    /// manifest the target holds, and the one an engine that ran the update
    /// holds afterwards (here the same engine, inside a transaction it rolls
    /// back).
    async fn change_pin(
        conn: &mut pbps_db::Conn,
        name: &str,
        from: &str,
        to: &str,
    ) -> pbps_model::ExtensionChangePin {
        let before = providers(conn).await.unwrap().remove(name).unwrap();
        let path = update_path(conn, name, from, to).await.unwrap().unwrap();
        conn.execute("BEGIN").await.unwrap();
        conn.execute(&format!("ALTER EXTENSION \"{name}\" UPDATE TO '{to}'"))
            .await
            .unwrap();
        let after = providers(conn).await.unwrap().remove(name).unwrap();
        conn.execute("ROLLBACK").await.unwrap();
        pbps_model::ExtensionChangePin {
            uid: "e_aaaaaa".parse().unwrap(),
            name: name.into(),
            from: before.definition.clone(),
            to: after.definition.clone(),
            path,
            before,
            after,
        }
    }

    const UPDATE_WORLD: &str = "CREATE SCHEMA ext; \
         CREATE EXTENSION upd2447 SCHEMA ext VERSION '1.0'; \
         CREATE EXTENSION updcascade2447 SCHEMA ext VERSION '1.0'; \
         CREATE EXTENSION updretire2447 SCHEMA ext VERSION '1.0'";

    /// The engine's own update paths: the versions it walks, and no path for a
    /// downgrade, the current version or an unknown one (measured on 16.15 and
    /// 18.6: a path can also end at a version that cannot be installed, which is
    /// why installability is read from the versions view and never from here).
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn the_engine_names_the_versions_an_update_walks_and_no_path_for_anything_else() {
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let (admin, db, mut conn) = fresh(variable, "path").await;
            let path = |from: &'static str, to: &'static str| (from, to);
            for (name, (from, to), want) in [
                (
                    "hstore",
                    path("1.4", "1.8"),
                    Some(vec!["1.4", "1.5", "1.6", "1.7", "1.8"]),
                ),
                ("upd2447", path("1.0", "1.1"), Some(vec!["1.0", "1.1"])),
                ("upd2447", path("1.1", "1.0"), None),
                ("upd2447", path("1.0", "1.0"), None),
                ("upd2447", path("1.0", "9.9"), None),
                ("nosuch", path("1.0", "1.1"), None),
            ] {
                let got = update_path(&mut conn, name, from, to).await.unwrap();
                let want = want.map(|v| v.into_iter().map(str::to_owned).collect::<Vec<_>>());
                assert_eq!(got, want, "{variable} {name} {from} -> {to}");
            }
            discard(admin, db, conn).await;
        }
    }

    /// An update is vetted before it runs and confirmed after it ran, in the
    /// statement's own transaction: a package that is not where the plan left
    /// it, members that changed, a path the engine no longer walks, and an
    /// update script that takes an outside dependent along (`CASCADE`, which the
    /// engine does not refuse) are each named, and an ordinary update passes.
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn an_update_is_vetted_before_it_runs_and_confirmed_after_it_ran() {
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let (admin, db, mut conn) = fresh(variable, "updvet").await;
            conn.execute(UPDATE_WORLD).await.unwrap();
            let pin = change_pin(&mut conn, "upd2447", "1.0", "1.1").await;
            assert!(pin.after.members.len() > pin.before.members.len());

            // Before the statement the result is not there yet; after it, it is.
            conn.execute("BEGIN").await.unwrap();
            vet_before_update(&mut conn, "upd2447", &pin).await.unwrap();
            let error = confirm_updated(&mut conn, "upd2447", &pin)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("does not match"),
                "{variable}: {error}"
            );
            conn.execute("ALTER EXTENSION upd2447 UPDATE TO '1.1'")
                .await
                .unwrap();
            confirm_updated(&mut conn, "upd2447", &pin).await.unwrap();
            conn.execute("ROLLBACK").await.unwrap();

            // Not where the plan left it.
            let mut elsewhere = pin.clone();
            elsewhere.from.version = "0.9".into();
            let error = vet_before_update(&mut conn, "upd2447", &elsewhere)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("not the `0.9`"),
                "{variable}: {error}"
            );
            let error = vet_before_update(&mut conn, "nosuch", &pin)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("no longer installed"), "{error}");

            // A path the engine no longer walks.
            let mut rerouted = pin.clone();
            rerouted.path = vec!["1.0".into(), "0.5".into(), "1.1".into()];
            let error = vet_before_update(&mut conn, "upd2447", &rerouted)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("no longer updates"),
                "{variable}: {error}"
            );

            // A member added since the plan stops it before the statement.
            conn.execute(
                "CREATE FUNCTION public.later() RETURNS int LANGUAGE sql AS 'SELECT 1'; \
                 ALTER EXTENSION upd2447 ADD FUNCTION public.later()",
            )
            .await
            .unwrap();
            let error = vet_before_update(&mut conn, "upd2447", &pin)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("members changed")
                    && error.to_string().contains("later"),
                "{variable}: {error}"
            );
            conn.execute("ALTER EXTENSION upd2447 DROP FUNCTION public.later(); DROP FUNCTION public.later()")
                .await
                .unwrap();

            // A script that drops a member with CASCADE takes an outside
            // dependent along, which the engine allows and the confirm refuses.
            let cascade = change_pin(&mut conn, "updcascade2447", "1.0", "1.1").await;
            conn.execute("CREATE TABLE public.dep (x int DEFAULT ext.updcascade2447_gone())")
                .await
                .unwrap();
            conn.execute("BEGIN").await.unwrap();
            vet_before_update(&mut conn, "updcascade2447", &cascade)
                .await
                .unwrap();
            conn.execute("ALTER EXTENSION updcascade2447 UPDATE TO '1.1'")
                .await
                .unwrap();
            let error = confirm_updated(&mut conn, "updcascade2447", &cascade)
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("depended on extension `updcascade2447`"),
                "{variable}: {error}"
            );
            conn.execute("ROLLBACK").await.unwrap();
            // Negative: with nothing depending on the member, the same update
            // passes.
            conn.execute("DROP TABLE public.dep").await.unwrap();
            conn.execute("BEGIN").await.unwrap();
            vet_before_update(&mut conn, "updcascade2447", &cascade)
                .await
                .unwrap();
            conn.execute("ALTER EXTENSION updcascade2447 UPDATE TO '1.1'")
                .await
                .unwrap();
            confirm_updated(&mut conn, "updcascade2447", &cascade)
                .await
                .unwrap();
            conn.execute("ROLLBACK").await.unwrap();

            // And an update the engine itself refuses (a retired member that a
            // dependent uses, no CASCADE) never reaches the confirm: it fails
            // inside the statement, with the dependent named.
            conn.execute("CREATE TABLE public.dep (x int DEFAULT ext.updretire2447_gone())")
                .await
                .unwrap();
            conn.execute("BEGIN").await.unwrap();
            let error = conn
                .execute("ALTER EXTENSION updretire2447 UPDATE TO '1.1'")
                .await
                .unwrap_err();
            assert!(error.to_string().contains("depend"), "{variable}: {error}");
            conn.execute("ROLLBACK").await.unwrap();
            discard(admin, db, conn).await;
        }
    }

    /// An update needs ownership of the package and nothing else when the
    /// control file asks for no superuser: the rule `update_problems` states is
    /// what the engine does for a superuser, the owner, a member that inherits
    /// the owner's privileges, one that does not, and a stranger (measured on
    /// 16.15 and 18.6), and it names a package that is not at the version the
    /// plan assumed, one with no path and one that is absent.
    #[tokio::test]
    #[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn the_connected_role_may_update_what_it_owns_and_nothing_else() {
        for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
            let (mut admin, db, mut conn) = fresh(variable, "updown").await;
            let tag = db.rsplit('_').next().unwrap().to_owned();
            let [own, member, loose, stranger] =
                ["own", "member", "loose", "stranger"].map(|r| format!("pbps_upd_{r}_{tag}"));
            admin
                .execute(&format!(
                    "CREATE ROLE {own} NOLOGIN; CREATE ROLE {member} NOLOGIN; \
                     CREATE ROLE {loose} NOLOGIN; CREATE ROLE {stranger} NOLOGIN; \
                     GRANT {own} TO {member}; GRANT {own} TO {loose} WITH INHERIT FALSE"
                ))
                .await
                .unwrap();
            conn.execute(&format!(
                "CREATE SCHEMA ext; GRANT ALL ON SCHEMA ext TO {own}; GRANT CREATE ON DATABASE {db} TO {own}; \
                 SET ROLE {own}; CREATE EXTENSION upd2447 SCHEMA ext VERSION '1.0'; RESET ROLE"
            ))
            .await
            .unwrap();
            let (from, to) = (wanted("ext", "1.0"), wanted("ext", "1.1"));
            let updates: BTreeMap<String, (Extension, Extension)> =
                [("upd2447".to_owned(), (from.clone(), to.clone()))].into();
            let none = BTreeMap::new();
            assert!(
                update_problems(&mut conn, &updates, &none)
                    .await
                    .unwrap()
                    .is_empty()
            );
            for (role, allowed) in [
                (&own, true),
                (&member, true),
                (&loose, false),
                (&stranger, false),
            ] {
                conn.execute(&format!("SET ROLE {role}")).await.unwrap();
                let problems = update_problems(&mut conn, &updates, &none).await.unwrap();
                conn.execute("BEGIN").await.unwrap();
                let tried = conn
                    .execute("ALTER EXTENSION upd2447 UPDATE TO '1.1'")
                    .await;
                conn.execute("ROLLBACK").await.unwrap();
                conn.execute("RESET ROLE").await.unwrap();
                assert_eq!(
                    problems.is_empty(),
                    allowed,
                    "{variable} {role}: {problems:?}"
                );
                assert_eq!(
                    tried.is_ok(),
                    allowed,
                    "{variable} {role}: the engine said {tried:?}"
                );
                if !allowed {
                    assert!(
                        problems[0].contains("42501") && problems[0].contains(&own),
                        "{problems:?}"
                    );
                }
            }
            // A trusted package that asks for a superuser needs `CREATE` on the
            // database as well, for a real update: the engine refuses the owner
            // without it (42501), and the rule says so first.
            conn.execute(&format!(
                "SET ROLE {own}; CREATE EXTENSION hstore SCHEMA ext VERSION '1.4'; RESET ROLE"
            ))
            .await
            .unwrap();
            let trusted: BTreeMap<String, (Extension, Extension)> = [(
                "hstore".to_owned(),
                (wanted("ext", "1.4"), wanted("ext", "1.5")),
            )]
            .into();
            for (create, allowed) in [(true, true), (false, false)] {
                if !create {
                    conn.execute(&format!("REVOKE CREATE ON DATABASE {db} FROM {own}"))
                        .await
                        .unwrap();
                }
                conn.execute(&format!("SET ROLE {own}")).await.unwrap();
                let problems = update_problems(&mut conn, &trusted, &none).await.unwrap();
                conn.execute("BEGIN").await.unwrap();
                let tried = conn.execute("ALTER EXTENSION hstore UPDATE TO '1.5'").await;
                conn.execute("ROLLBACK").await.unwrap();
                conn.execute("RESET ROLE").await.unwrap();
                assert_eq!(
                    problems.is_empty(),
                    allowed,
                    "{variable} create={create}: {problems:?}"
                );
                assert_eq!(
                    tried.is_ok(),
                    allowed,
                    "{variable} create={create}: the engine said {tried:?}"
                );
                if !allowed {
                    assert!(
                        problems[0].contains("CREATE on the database"),
                        "{problems:?}"
                    );
                }
            }
            // Negatives: not at the assumed version, no path, absent.
            let held: BTreeMap<String, (Extension, Extension)> =
                [("upd2447".to_owned(), (wanted("ext", "1.1"), to.clone()))].into();
            let found = update_problems(&mut conn, &held, &none).await.unwrap();
            assert!(
                found
                    .iter()
                    .any(|p| p.contains("not installed as the plan assumed")),
                "{found:?}"
            );
            let down: BTreeMap<String, (Extension, Extension)> =
                [("upd2447".to_owned(), (from.clone(), wanted("ext", "1.0")))].into();
            let found = update_problems(&mut conn, &down, &none).await.unwrap();
            assert!(
                found.iter().any(|p| p.contains("no update path")),
                "{found:?}"
            );
            let absent: BTreeMap<String, (Extension, Extension)> =
                [("absent2447".to_owned(), (from.clone(), to.clone()))].into();
            let found = update_problems(&mut conn, &absent, &none).await.unwrap();
            assert!(
                found.iter().any(|p| p.contains("not installed")),
                "{found:?}"
            );
            drop(conn);
            admin
                .execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
                .await
                .unwrap();
            admin
                .execute(&format!(
                    "DROP OWNED BY {own}, {member}, {loose}, {stranger}"
                ))
                .await
                .ok();
            admin
                .execute(&format!("DROP ROLE {own}, {member}, {loose}, {stranger}"))
                .await
                .unwrap();
        }
    }
}
