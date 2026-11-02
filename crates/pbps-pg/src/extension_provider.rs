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
use pbps_model::{Extension, ExtensionProvider, ProviderMember};
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
    for required in control.requires.iter().filter(|r| !known.contains_key(*r)) {
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
    if let Some(control) = target
        .available
        .get(&(name.to_owned(), wanted.version.clone()))
        && !target.superuser
        && control.superuser
    {
        if !control.trusted {
            out.push(at(
                "is not trusted and its install script needs a superuser; the connected role is not one".into(),
            ));
        } else if !target.database_create {
            out.push(at(
                "is installed by a role with CREATE on the database, which the connected role lacks".into(),
            ));
        }
    }
    out
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
}
