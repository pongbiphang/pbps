//! Properties of objects the pull holds that the declarations cannot express
//! (#168), named in the report-only notices beside the census (DEC-168.1).
//!
//! A notice never leaves the object out of the pull and never becomes a
//! managed-set limitation: a tuned database commonly sets a statistics target
//! or clusters a table, `pbps` never resets these on an existing database,
//! and refusing such a table would block adoption for a property nothing
//! here would change.
use pbps_db::DbError;
use serde_json::Value;
use std::collections::BTreeMap;

/// One row per property, in one statement with the rest of the catalog batch.
/// `identity` and `type` are the engine's own (`pg_identify_object`);
/// `detail` is the value that is not held, when there is one.
///
/// What is not reported: system objects (`oid < 16384`), temporary and TOAST
/// schemas, and the members of an extension (the package owns them, which the
/// census already names). The member closure is the census's, restated here
/// because the two statements are separate parts of the batch.
pub(super) const QUERY: &str = r#"
WITH RECURSIVE extension_members(classid, objid) AS (
    SELECT classid, objid FROM pg_catalog.pg_depend WHERE deptype = 'e'
    UNION
    SELECT d.classid, d.objid FROM extension_members e JOIN pg_catalog.pg_depend d
      ON d.refclassid = e.classid AND d.refobjid = e.objid AND d.deptype IN ('i', 'a')
), relations(oid, relkind, relpersistence) AS (
    SELECT c.oid, c.relkind, c.relpersistence
      FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     WHERE c.oid >= 16384 AND n.nspname !~ '^pg_(toast|temp)'
       AND NOT EXISTS (SELECT 1 FROM extension_members e
                        WHERE e.classid = 'pg_catalog.pg_class'::regclass AND e.objid = c.oid)
), found(kind, classid, objid, objsubid, detail) AS (
    -- S1, S5: a column's, or an index column's, statistics target. 16 stores
    -- the default as -1, 18 as NULL.
    SELECT 'statistics', 'pg_catalog.pg_class'::regclass, a.attrelid, a.attnum::int4,
           NULLIF(a.attstattarget, -1)::text
      FROM pg_catalog.pg_attribute a JOIN relations r ON r.oid = a.attrelid
     WHERE a.attnum > 0 AND NOT a.attisdropped AND r.relkind IN ('r', 'p', 'i', 'I')
       AND NULLIF(a.attstattarget, -1) IS NOT NULL
    UNION ALL
    -- S2: the index CLUSTER follows.
    SELECT 'clustered index', 'pg_catalog.pg_class'::regclass, x.indexrelid, 0, NULL
      FROM pg_catalog.pg_index x JOIN relations r ON r.oid = x.indexrelid
     WHERE x.indisclustered
    UNION ALL
    -- S3: column options such as n_distinct.
    SELECT 'column options', 'pg_catalog.pg_class'::regclass, a.attrelid, a.attnum::int4,
           pg_catalog.array_to_string(a.attoptions, ', ')
      FROM pg_catalog.pg_attribute a JOIN relations r ON r.oid = a.attrelid
     WHERE a.attnum > 0 AND NOT a.attisdropped AND r.relkind IN ('r', 'p')
       AND a.attoptions IS NOT NULL
    UNION ALL
    -- S4: an UNLOGGED sequence that belongs to a column (a standalone one is
    -- the census's).
    SELECT 'unlogged sequence', 'pg_catalog.pg_class'::regclass, r.oid, 0, NULL
      FROM relations r
     WHERE r.relkind = 'S' AND r.relpersistence = 'u'
       AND EXISTS (SELECT 1 FROM pg_catalog.pg_depend d
                    WHERE d.classid = 'pg_catalog.pg_class'::regclass AND d.objid = r.oid
                      AND d.refclassid = 'pg_catalog.pg_class'::regclass
                      AND d.refobjsubid > 0 AND d.deptype IN ('a', 'i'))
    UNION ALL
    -- S6: ALTER ... DEPENDS ON EXTENSION.
    SELECT 'depends on extension', d.classid, d.objid, d.objsubid, e.extname::text
      FROM pg_catalog.pg_depend d
      JOIN pg_catalog.pg_extension e
        ON d.refclassid = 'pg_catalog.pg_extension'::regclass AND e.oid = d.refobjid
     WHERE d.deptype = 'x' AND d.objid >= 16384
    UNION ALL
    -- S11: comments (the census names an extension's own).
    SELECT 'comment', d.classoid, d.objoid, d.objsubid, NULL
      FROM pg_catalog.pg_description d
     WHERE d.objoid >= 16384 AND d.classoid <> 'pg_catalog.pg_extension'::regclass
    UNION ALL
    -- S12: security labels.
    SELECT 'security label', l.classoid, l.objoid, l.objsubid, l.provider
      FROM pg_catalog.pg_seclabel l
     WHERE l.objoid >= 16384
)
SELECT f.kind, i.type, i.identity, f.detail
  FROM found f CROSS JOIN LATERAL pg_catalog.pg_identify_object(f.classid, f.objid, f.objsubid) i
 WHERE NOT EXISTS (SELECT 1 FROM extension_members e WHERE e.classid = f.classid AND e.objid = f.objid)
   AND NOT (f.classid = 'pg_catalog.pg_class'::regclass AND f.kind IN ('comment', 'security label')
            AND NOT EXISTS (SELECT 1 FROM relations r WHERE r.oid = f.objid))
"#;

/// How many examples a grouped notice names before it counts the rest.
const EXAMPLES: usize = 3;

/// The notices for the rows [`QUERY`] returned: one per object for a property
/// of an object, one per object type for comments and security labels (a
/// documented database has thousands, and a line each would bury the rest).
pub(super) fn notices(rows: &[Value]) -> Result<Vec<String>, DbError> {
    let mut out = Vec::new();
    let mut grouped: BTreeMap<(&'static str, String), Vec<String>> = BTreeMap::new();
    for row in rows {
        // Every field is decoded for every row: an unreadable row is never an
        // empty report.
        let kind = super::text(row, "kind")?;
        let ty = super::text(row, "type")?;
        let detail = super::optional_text(row, "detail")?;
        let identity = super::optional_text(row, "identity")?.ok_or_else(|| {
            DbError::Refused(format!(
                "the catalog changed while it was being read: an object with {kind} \
                 was there when the catalog was scanned and gone when its identity was resolved.\n\
                 Something applied DDL to this database during the pull. \
                 Run it again when the other change has finished.",
                kind = pbps_model::escape::Escaped(&kind),
            ))
        })?;
        let id = pbps_model::escape::Escaped(&identity);
        let detail_of = || {
            detail
                .clone()
                .map(|d| pbps_model::escape::Escaped(&d).to_string())
                .ok_or_else(|| DbError::BadRow(format!("a `{kind}` row without its value")))
        };
        match kind.as_str() {
            "statistics" => out.push(format!(
                "{ty} `{id}` has STATISTICS {}, which the declarations do not hold",
                detail_of()?
            )),
            "clustered index" => out.push(format!(
                "index `{id}` is the one CLUSTER follows, which the declarations do not hold"
            )),
            "column options" => out.push(format!(
                "{ty} `{id}` has the options `{}`, which the declarations do not hold",
                detail_of()?
            )),
            "unlogged sequence" => out.push(format!(
                "sequence `{id}` is UNLOGGED, which the declarations do not hold: its position \
                 is reset after a crash, and one created from them would keep it"
            )),
            "depends on extension" => out.push(format!(
                "{ty} `{id}` depends on the extension `{}` (DEPENDS ON EXTENSION), which the \
                 declarations do not hold: dropping the extension would drop it",
                detail_of()?
            )),
            "comment" => grouped
                .entry(("comment", ty))
                .or_default()
                .push(identity.clone()),
            "security label" => grouped
                .entry(("security label", ty))
                .or_default()
                .push(identity.clone()),
            other => {
                return Err(DbError::BadRow(format!(
                    "an unheld-property row of the unknown kind `{}`",
                    pbps_model::escape::Escaped(other)
                )));
            }
        }
    }
    for ((kind, ty), mut identities) in grouped {
        identities.sort();
        identities.dedup();
        let shown: Vec<_> = identities
            .iter()
            .take(EXAMPLES)
            .map(|i| format!("`{}`", pbps_model::escape::Escaped(i)))
            .collect();
        let more = match identities.len().saturating_sub(EXAMPLES) {
            0 => String::new(),
            n => format!(" and {n} more"),
        };
        let issue = if kind == "comment" {
            "; they are not written back (#2729)"
        } else {
            ""
        };
        out.push(format!(
            "{} {kind}(s) on {ty}: {}{more} are not held by this model{issue}",
            identities.len(),
            shown.join(", "),
        ));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(kind: &str, ty: &str, identity: &str, detail: Option<&str>) -> Value {
        json!({"kind": kind, "type": ty, "identity": identity, "detail": detail})
    }

    #[test]
    fn each_property_is_named_with_its_value_and_the_object_stays_pulled() {
        let found = notices(&[
            row("statistics", "table column", "app.t.v", Some("500")),
            row("clustered index", "index", "app.t_v", None),
            row(
                "column options",
                "table column",
                "app.t.v",
                Some("n_distinct=100"),
            ),
            row("unlogged sequence", "sequence", "app.t_id_seq", None),
            row(
                "depends on extension",
                "function",
                "app.f()",
                Some("pgcrypto"),
            ),
        ])
        .unwrap();
        let all = found.join("\n");
        for needle in [
            "table column `app.t.v` has STATISTICS 500, which the declarations do not hold",
            "index `app.t_v` is the one CLUSTER follows",
            "has the options `n_distinct=100`",
            "its position is reset after a crash",
            "depends on the extension `pgcrypto`",
        ] {
            assert!(all.contains(needle), "{needle}\n{all}");
        }
        assert!(
            !all.contains("left out"),
            "a property is named, an object is not dropped:\n{all}"
        );
    }

    #[test]
    fn comments_and_labels_are_counted_per_type_with_a_few_examples() {
        let rows: Vec<_> = (0..5)
            .map(|n| row("comment", "table", &format!("app.t{n}"), None))
            .chain([row("security label", "table", "app.t0", Some("selinux"))])
            .collect();
        let found = notices(&rows).unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found.iter().any(|m| m.contains("5 comment(s) on table")
                && m.contains("`app.t0`, `app.t1`, `app.t2` and 2 more")
                && m.contains("#2729")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|m| m.contains("1 security label(s) on table"))
        );
    }

    #[test]
    fn an_unreadable_or_unknown_row_is_an_error_not_an_empty_report() {
        assert!(notices(&[]).unwrap().is_empty());
        let good = row("statistics", "table column", "app.t.v", Some("500"));
        for key in ["kind", "type", "identity", "detail"] {
            let mut missing = good.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(notices(&[missing]).is_err(), "{key} missing");
        }
        let mut lost = good.clone();
        lost["identity"] = Value::Null;
        assert!(
            notices(&[lost])
                .unwrap_err()
                .to_string()
                .contains("the catalog changed while it was being read")
        );
        let mut no_value = good.clone();
        no_value["detail"] = Value::Null;
        assert!(
            notices(&[no_value]).is_err(),
            "a statistics row needs its value"
        );
        assert!(notices(&[row("future", "table", "app.t", None)]).is_err());
    }
    #[tokio::test]
    #[ignore = "needs live PostgreSQL 16; set PBPS_TEST_PG_OLD_DB"]
    async fn every_unheld_property_is_named_and_its_object_still_pulled_on_pg16() {
        named_and_pulled("PBPS_TEST_PG_OLD_DB").await;
    }

    #[tokio::test]
    #[ignore = "needs live PostgreSQL 18; set PBPS_TEST_PG_DB"]
    async fn every_unheld_property_is_named_and_its_object_still_pulled_on_pg18() {
        named_and_pulled("PBPS_TEST_PG_DB").await;
    }

    /// One probe per note: each property is created on the engine, the whole
    /// database is pulled, and the notice must be printed while the table it
    /// belongs to is still in the pull and none of it became a limitation.
    /// (A security label needs a label provider the fixtures do not load; its
    /// text is pinned by the unit test above.)
    async fn named_and_pulled(variable: &str) {
        use pbps_db::{Conn, Driver};

        // (setup, text of the notice, the table that must still be pulled)
        let cases: &[(&str, &str, Option<&str>)] = &[
            (
                "CREATE TABLE app.s1 (id int PRIMARY KEY, v text);
                 ALTER TABLE app.s1 ALTER COLUMN v SET STATISTICS 500",
                "table column `app.s1.v` has STATISTICS 500, which the declarations do not hold",
                Some("s1"),
            ),
            (
                "CREATE TABLE app.s5 (id int PRIMARY KEY, v text);
                 CREATE INDEX s5_e ON app.s5 ((v || 'x'));
                 ALTER INDEX app.s5_e ALTER COLUMN 1 SET STATISTICS 400",
                "has STATISTICS 400, which the declarations do not hold",
                Some("s5"),
            ),
            (
                "CREATE TABLE app.s2 (id int PRIMARY KEY, v int);
                 CREATE INDEX s2_v ON app.s2 (v); CLUSTER app.s2 USING s2_v",
                "index `app.s2_v` is the one CLUSTER follows",
                Some("s2"),
            ),
            (
                "CREATE TABLE app.s3 (id int PRIMARY KEY, v int);
                 ALTER TABLE app.s3 ALTER COLUMN v SET (n_distinct = 100)",
                "`app.s3.v` has the options `n_distinct=100`",
                Some("s3"),
            ),
            (
                "CREATE TABLE app.s4 (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY);
                 ALTER SEQUENCE app.s4_id_seq SET UNLOGGED",
                "sequence `app.s4_id_seq` is UNLOGGED, which the declarations do not hold: its \
                 position is reset after a crash",
                Some("s4"),
            ),
            (
                "CREATE FUNCTION app.s6() RETURNS int LANGUAGE sql AS 'select 1';
                 ALTER FUNCTION app.s6() DEPENDS ON EXTENSION plpgsql",
                "depends on the extension `plpgsql`",
                None,
            ),
            (
                "CREATE TEXT SEARCH CONFIGURATION app.s7c (COPY = pg_catalog.simple);
                 CREATE TEXT SEARCH DICTIONARY app.s7d (TEMPLATE = pg_catalog.simple)",
                "text search configuration `app.s7c` is not held by this model",
                None,
            ),
            (
                "SELECT 1",
                "text search dictionary `app.s7d` is not held by this model; it is left out of \
                 the pull and schema comparison; a declaration that names it cannot be created \
                 until they are supported (#2728)",
                None,
            ),
            (
                "CREATE CONVERSION app.s8 FOR 'UTF8' TO 'LATIN1' FROM utf8_to_iso8859_1",
                "conversion `app.s8` is not held by this model",
                None,
            ),
            (
                "CREATE FOREIGN DATA WRAPPER s9_fdw; CREATE SERVER s9_srv FOREIGN DATA WRAPPER s9_fdw;
                 CREATE USER MAPPING FOR CURRENT_USER SERVER s9_srv",
                "foreign data wrapper `s9_fdw` is not held by this model",
                None,
            ),
            (
                "SELECT 1",
                "foreign server `s9_srv` is not held by this model",
                None,
            ),
            (
                "CREATE LANGUAGE s10_lang HANDLER plpgsql_call_handler",
                "procedural language `s10_lang` is not held by this model",
                None,
            ),
            (
                "CREATE TABLE app.s11 (id int PRIMARY KEY, v int);
                 COMMENT ON TABLE app.s11 IS 'a'; COMMENT ON COLUMN app.s11.v IS 'b'",
                "comment(s) on table: `app.s11` are not held by this model; they are not \
                 written back (#2729)",
                Some("s11"),
            ),
        ];
        let server = std::env::var(variable).expect("the pinned PostgreSQL fixture");
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        let database = format!("pbps_s168_{}", std::process::id());
        admin
            .execute(&format!("CREATE DATABASE {database} TEMPLATE template0"))
            .await
            .unwrap();
        let mut conn = Conn::connect(Driver::Postgres, &format!("{server} dbname={database}"))
            .await
            .unwrap();
        let outcome = async {
            conn.execute("CREATE SCHEMA app").await.unwrap();
            conn.execute("CREATE TABLE app.plain (id int PRIMARY KEY, v text)")
                .await
                .unwrap();
            for (setup, ..) in cases {
                conn.execute(setup)
                    .await
                    .unwrap_or_else(|e| panic!("{setup}: {e:?}"));
            }
            super::super::introspect(&mut conn).await
        }
        .await;
        admin
            .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
            .await
            .unwrap();
        let pulled = outcome.unwrap();
        let notices = pulled.catalog_notices.join("\n");
        for &(_, notice, table) in cases {
            assert!(
                notices.contains(notice),
                "{variable}: missing `{notice}` in\n{notices}"
            );
            if let Some(table) = table {
                let name = pbps_model::TableName::new("app", table);
                assert!(
                    pulled.schema.tables.contains_key(&name),
                    "{variable}: `app.{table}` must still be pulled"
                );
            }
        }
        assert!(
            !notices.contains("`app.plain"),
            "a table without the properties draws no notice:\n{notices}"
        );
        // A note, never a managed-set limitation: no plan may refuse a table for it.
        assert!(
            !pulled.warnings.iter().any(|w| w.contains("STATISTICS")
                || w.contains("CLUSTER")
                || w.contains("UNLOGGED")),
            "{:?}",
            pulled.warnings
        );
    }
}
