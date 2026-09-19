//! The catalog half of an analysis scope's facts, read over any connection.
//!
//! Everything here is what SQL can see: versions, locale, extensions and the
//! native libraries they name, collations with the version their provider
//! reports *now*, effective settings with where they came from, and the
//! engine's own answer to "which schemas does this principal see on this
//! path". What SQL cannot see — the content of the executables a backend
//! runs — is read from the kernel by the CLI's native observer and joined
//! to these facts there.
//!
//! A NULL column stays `NotReported`; a failed query is an error. Neither is
//! ever an empty value that happens to equal an empty value on the other
//! side (AGENTS.md: absent, empty and unreadable are three different things).

use super::compatibility::{OPTIONAL_SETTINGS, SETTINGS};
use pbps_db::DbError;
use pbps_db::resolver::Observation;
use pbps_db::resolver::environment::{
    CatalogFacts, CollationFact, ExtensionFact, SettingFact, render_visibility,
};
use pbps_db::transport::QueryConnection;
use std::collections::BTreeMap;

/// The schemas a plan's write path starts with, and the extras the dialect
/// appends after each (SPEC §7.3: `SET search_path = "<schema>", <extras>,
/// "pg_temp"`). Visibility is evaluated for exactly these paths, so what is
/// compared is the path deployment statements actually run under.
pub struct Scope<'a> {
    pub schemas: &'a [String],
    pub write_path_extras: &'a [String],
}

// Version-dependent locale fields are projected by name from the catalog
// row (PostgreSQL 17 renamed daticulocale to datlocale); a missing field is
// NULL and stays unreported. `pg_database_collation_actual_version` is what
// the provider reports today, which is what comparisons actually use, as
// opposed to `datcollversion`, which is what was recorded at creation.
const DATABASE: &str = "\
SELECT pg_catalog.current_setting('server_version') AS server_version,
       pg_catalog.current_setting('server_version_num') AS server_version_num,
       pg_catalog.pg_encoding_to_char(d.encoding) AS database_encoding,
       d.datcollate AS database_collate,
       d.datctype AS database_ctype,
       pg_catalog.to_jsonb(d)->>'datlocprovider' AS database_locale_provider,
       COALESCE(pg_catalog.to_jsonb(d)->>'datlocale',
                pg_catalog.to_jsonb(d)->>'daticulocale') AS database_locale,
       pg_catalog.to_jsonb(d)->>'daticurules' AS database_icu_rules,
       pg_catalog.to_jsonb(d)->>'datcollversion' AS database_collation_recorded_version,
       pg_catalog.pg_database_collation_actual_version(d.oid) AS database_collation_actual_version
FROM pg_catalog.pg_database d
WHERE d.datname = pg_catalog.current_database()";

const DATABASE_FIELDS: &[&str] = &[
    "server_version",
    "server_version_num",
    "database_encoding",
    "database_collate",
    "database_ctype",
    "database_locale_provider",
    "database_locale",
    "database_icu_rules",
    "database_collation_recorded_version",
    "database_collation_actual_version",
];

// User-defined collations only: the catalog's own thousand rows are a
// property of the build already compared through the executable, and the
// database default is read from pg_database above. PostgreSQL 17 renamed
// colliculocale to colllocale; both are projected by name.
const COLLATIONS: &str = "\
SELECT n.nspname::text || '.' || c.collname::text AS key,
       c.collprovider::text AS provider,
       COALESCE(pg_catalog.to_jsonb(c)->>'colllocale',
                pg_catalog.to_jsonb(c)->>'colliculocale') AS locale,
       pg_catalog.to_jsonb(c)->>'collicurules' AS rules,
       c.collversion AS recorded_version,
       pg_catalog.pg_collation_actual_version(c.oid) AS actual_version
FROM pg_catalog.pg_collation c
JOIN pg_catalog.pg_namespace n ON n.oid = c.collnamespace
WHERE n.nspname NOT IN ('pg_catalog', 'pg_toast')
ORDER BY 1";

// LEFT JOINs keep an extension whose namespace or version row could not be
// read; an inner join would certify an incomplete inventory. The native
// libraries are the `probin` of the extension's C-language functions — what
// a resolver has to load, not what the extension is called.
const EXTENSIONS: &str = "\
SELECT e.extname::text AS name,
       e.extversion AS version,
       n.nspname::text AS schema,
       (SELECT pg_catalog.json_agg(r ORDER BY r)::text FROM pg_catalog.unnest(v.requires) AS r) AS requires,
       (SELECT pg_catalog.json_agg(DISTINCT p.probin ORDER BY p.probin)::text
        FROM pg_catalog.pg_depend d
        JOIN pg_catalog.pg_proc p ON p.oid = d.objid
        JOIN pg_catalog.pg_language l ON l.oid = p.prolang
        WHERE d.refclassid = 'pg_catalog.pg_extension'::regclass
          AND d.refobjid = e.oid
          AND d.classid = 'pg_catalog.pg_proc'::regclass
          AND l.lanname = 'c'
          AND p.probin IS NOT NULL) AS libraries
FROM pg_catalog.pg_extension e
LEFT JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
LEFT JOIN pg_catalog.pg_available_extension_versions v
       ON v.name = e.extname AND v.version = e.extversion
ORDER BY 1";

const AVAILABLE: &str = "\
SELECT name::text AS name, version::text AS version
FROM pg_catalog.pg_available_extension_versions
ORDER BY 1, 2";

pub async fn read(
    conn: &mut impl QueryConnection,
    scope: &Scope<'_>,
) -> Result<CatalogFacts, DbError> {
    let rows = conn.query(DATABASE).await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "analysis-scope facts expected one current database row".into(),
        ));
    };
    let mut observations = BTreeMap::new();
    for &field in DATABASE_FIELDS {
        observations.insert(
            field.to_owned(),
            Observation::reported(row.try_get::<&str>(field)?),
        );
    }
    let mut collations = vec![CollationFact {
        key: "default".into(),
        provider: required(
            row,
            "database_locale_provider",
            "the database's locale provider",
        )?,
        locale: observations["database_locale"].clone(),
        rules: observations["database_icu_rules"].clone(),
        recorded_version: observations["database_collation_recorded_version"].clone(),
        actual_version: observations["database_collation_actual_version"].clone(),
    }];
    for row in conn.query(COLLATIONS).await? {
        collations.push(CollationFact {
            key: required(&row, "key", "a collation's name")?,
            provider: required(&row, "provider", "a collation's provider")?,
            locale: Observation::reported(row.try_get::<&str>("locale")?),
            rules: Observation::reported(row.try_get::<&str>("rules")?),
            recorded_version: Observation::reported(row.try_get::<&str>("recorded_version")?),
            actual_version: Observation::reported(row.try_get::<&str>("actual_version")?),
        });
    }

    let mut extensions = Vec::new();
    for row in conn.query(EXTENSIONS).await? {
        let name = required(&row, "name", "an extension's name")?;
        let list = |field: &str| json_list(row.try_get::<&str>(field)?, &name, field);
        extensions.push(ExtensionFact {
            version: required(&row, "version", "an extension's version")?,
            schema: required(&row, "schema", "an extension's namespace")?,
            requires: list("requires")?,
            libraries: list("libraries")?,
            name,
        });
    }

    let mut available_extensions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in conn.query(AVAILABLE).await? {
        available_extensions
            .entry(required(&row, "name", "an available extension's name")?)
            .or_default()
            .push(required(
                &row,
                "version",
                "an available extension's version",
            )?);
    }

    let names: Vec<String> = SETTINGS
        .iter()
        .chain(OPTIONAL_SETTINGS)
        .map(|name| format!("'{name}'"))
        .collect();
    let mut settings = BTreeMap::new();
    for row in conn
        .query(&format!(
            "SELECT name, setting, source, context FROM pg_catalog.pg_settings WHERE name IN ({})",
            names.join(", ")
        ))
        .await?
    {
        settings.insert(
            required(&row, "name", "a setting's name")?,
            SettingFact {
                value: required(&row, "setting", "a setting's value")?,
                source: required(&row, "source", "a setting's source")?,
                context: required(&row, "context", "a setting's context")?,
            },
        );
    }

    let mut visibility = BTreeMap::new();
    for schema in scope.schemas {
        visibility.insert(
            schema.clone(),
            Observation::reported(Some(
                &effective_schemas(conn, schema, scope.write_path_extras).await?,
            )),
        );
    }

    Ok(CatalogFacts {
        observations,
        extensions,
        available_extensions,
        collations,
        settings,
        visibility,
    })
}

/// The engine's own effective schema order for one write path, as the
/// current principal: `current_schemas(true)` after `set_config(...,
/// is_local = true)`, taken as a JSON array so that a name containing a comma
/// or a quote, or spelling the array NULL sentinel, is one element and not a
/// parse of the engine's array text (finding on #688). The engine applies the
/// USAGE filter and drops schemas that do not exist; re-deriving that from
/// ACLs would be a second implementation of `recomputeNamespacePath` to keep
/// in step. Local scope means the setting ends with the enclosing
/// transaction — the statement's own outside one, the snapshot's inside
/// `scope_facts` — so the connection's search path is not left changed for
/// what runs after (measured on 16 and 18).
async fn effective_schemas(
    conn: &mut impl QueryConnection,
    schema: &str,
    extras: &[String],
) -> Result<String, DbError> {
    let rows = conn
        .query(&format!(
            "SELECT pg_catalog.array_to_json(pg_catalog.current_schemas(true))::text AS path \
             FROM (SELECT pg_catalog.set_config('search_path', '{}', true)) AS s",
            render_path(schema, extras).replace('\'', "''")
        ))
        .await?;
    let [row] = rows.as_slice() else {
        return Err(DbError::BadRow(
            "effective schema order expected one row".into(),
        ));
    };
    let path = required(row, "path", "the effective schema order")?;
    let elements: Vec<String> = serde_json::from_str(&path).map_err(|error| {
        DbError::BadRow(format!(
            "the effective schema order is not a JSON array of names: {error}"
        ))
    })?;
    Ok(render_visibility(&without_temp_schemas(elements)))
}

/// Drops the session's own temporary schemas from an effective order.
/// Measured on 16 and 18: with `pg_temp` on the path, `current_schemas(true)`
/// names the backend's actual temporary namespace (`pg_temp_3`) once one
/// exists, and that number is per backend, so two sessions that see exactly
/// the same schemas would otherwise compare as different. `pg_temp` is on
/// every write path by construction (SPEC §7.3), so its presence is never
/// the visibility difference this fact exists to catch. Whole elements only:
/// a schema whose name merely contains such a fragment is kept.
fn without_temp_schemas(elements: Vec<String>) -> Vec<String> {
    elements
        .into_iter()
        .filter(|part| {
            let temp = |prefix: &str| {
                part.strip_prefix(prefix)
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            };
            !(temp("pg_temp_") || temp("pg_toast_temp_"))
        })
        .collect()
}

/// The write path as the dialect renders it: every part quoted, so a schema
/// name containing a quote or a comma can neither split the path nor end
/// the literal it is placed in.
fn render_path(schema: &str, extras: &[String]) -> String {
    std::iter::once(schema)
        .chain(extras.iter().map(String::as_str))
        .chain(std::iter::once("pg_temp"))
        .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A name list the catalog query aggregated as a JSON array, so that a
/// library path or extension name containing a comma stays one element: a
/// comma-joined `string_agg` had no way to tell `$libdir/foo,bar` from two
/// libraries, and the two names it produced resolved to nothing, so a scope
/// whose extension the scratch engine had just as well was refused as
/// unknown (finding on #688). An aggregate over no rows is NULL, and for
/// `requires` and `libraries` that is a measured "none" (hstore has
/// neither).
fn json_list(text: Option<&str>, extension: &str, field: &str) -> Result<Vec<String>, DbError> {
    text.map_or(Ok(Vec::new()), |text| {
        serde_json::from_str(text).map_err(|error| {
            DbError::BadRow(format!(
                "extension {extension}'s {field} is not a JSON array of names: {error}"
            ))
        })
    })
}

fn required(row: &pbps_db::Row, field: &str, what: &str) -> Result<String, DbError> {
    row.try_get::<&str>(field)?
        .map(str::to_owned)
        .ok_or_else(|| DbError::BadRow(format!("analysis-scope facts did not report {what}")))
}

#[cfg(test)]
mod tests {
    use super::{json_list, render_path, without_temp_schemas};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_sessions_own_temporary_schema_is_not_part_of_its_visibility() {
        assert_eq!(
            without_temp_schemas(names(&["pg_catalog", "pg_temp_3"])),
            names(&["pg_catalog"])
        );
        assert_eq!(
            without_temp_schemas(names(&[
                "pg_catalog",
                "b",
                "pg_toast_temp_12",
                "pg_temp_12"
            ])),
            names(&["pg_catalog", "b"])
        );
        // Only the numbered temporary namespaces go; a user schema that
        // merely starts with the prefix stays, as does order, and so does a
        // name that only contains such a fragment (finding on #688).
        assert_eq!(
            without_temp_schemas(names(&["pg_temp_archive", "b", "a", "x,pg_temp_3,y"])),
            names(&["pg_temp_archive", "b", "a", "x,pg_temp_3,y"])
        );
        assert_eq!(without_temp_schemas(Vec::new()), Vec::<String>::new());
    }

    #[test]
    fn a_library_name_is_one_element_whatever_it_contains() {
        assert_eq!(
            json_list(
                Some(r#"["$libdir/foo,bar","$libdir/hstore"]"#),
                "x",
                "libraries"
            )
            .unwrap(),
            names(&["$libdir/foo,bar", "$libdir/hstore"])
        );
        // No rows aggregated is the measured "none", not an error; text
        // that is not a JSON array of names is an error, not an empty list.
        assert_eq!(
            json_list(None, "x", "requires").unwrap(),
            Vec::<String>::new()
        );
        for broken in ["$libdir/hstore", "[1]", "[null]", ""] {
            let error = json_list(Some(broken), "x", "libraries")
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("extension x's libraries"),
                "{broken:?}: {error}"
            );
        }
    }

    #[test]
    fn the_evaluated_path_quotes_every_part() {
        let extras = ["ext".to_owned(), "odd\"name".to_owned()];
        assert_eq!(
            render_path("app, public", &extras),
            "\"app, public\", \"ext\", \"odd\"\"name\", \"pg_temp\""
        );
        assert_eq!(render_path("s", &[]), "\"s\", \"pg_temp\"");
    }
}
