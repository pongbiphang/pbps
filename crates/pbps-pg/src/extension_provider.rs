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
  LEFT JOIN LATERAL pg_catalog.pg_identify_object_as_address(m.classid, m.objid, m.objsubid) a ON true";

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
