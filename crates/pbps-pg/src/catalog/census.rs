//! Report-only census, sampled with the ordinary catalog batch (DEC-2444.1).
use pbps_db::DbError;
use serde_json::Value;

// Object addresses let PostgreSQL render identities uniformly, including
// operator signatures and objects without a namespace. Large objects use
// pg_largeobject's address, although only its readable metadata is queried.
pub(super) const QUERY: &str = concat!(
    r#"
WITH RECURSIVE used_types(oid) AS (
    SELECT atttypid FROM pg_catalog.pg_attribute WHERE attnum > 0 AND NOT attisdropped
    UNION
    SELECT d.refobjid FROM used_types u JOIN pg_catalog.pg_depend d
      ON d.classid = 'pg_catalog.pg_type'::regclass AND d.objid = u.oid
     AND d.refclassid = 'pg_catalog.pg_type'::regclass
), extension_members(classid, objid) AS (
    SELECT classid, objid FROM pg_catalog.pg_depend WHERE deptype = 'e'
    UNION
    SELECT d.classid, d.objid FROM extension_members e JOIN pg_catalog.pg_depend d
      ON d.refclassid = e.classid AND d.refobjid = e.objid AND d.deptype IN ('i', 'a')
), objects(classid, oid, kind, model_owned) AS (
    SELECT 'pg_catalog.pg_extension'::regclass, oid, 'extension', false FROM pg_catalog.pg_extension
    UNION ALL
    SELECT 'pg_catalog.pg_extension'::regclass, e.oid, 'comment on extension', false
      FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_description d
        ON d.classoid = 'pg_catalog.pg_extension'::regclass AND d.objoid = e.oid AND d.objsubid = 0
    UNION ALL
    SELECT 'pg_catalog.pg_publication'::regclass, oid, 'publication', false FROM pg_catalog.pg_publication
    UNION ALL
    SELECT 'pg_catalog.pg_subscription'::regclass, oid, 'subscription', false FROM pg_catalog.pg_subscription
     WHERE subdbid = (SELECT oid FROM pg_catalog.pg_database WHERE datname = pg_catalog.current_database())
    UNION ALL
    SELECT 'pg_catalog.pg_class'::regclass, c.oid, 'standalone sequence', EXISTS (
        SELECT 1 FROM pg_catalog.pg_depend d WHERE d.classid = 'pg_catalog.pg_class'::regclass
         AND d.objid = c.oid AND d.refclassid = 'pg_catalog.pg_class'::regclass
         AND d.refobjsubid > 0 AND d.deptype IN ('a', 'i'))
      FROM pg_catalog.pg_class c WHERE c.relkind = 'S'
    UNION ALL
    SELECT 'pg_catalog.pg_operator'::regclass, oid, 'operator', false FROM pg_catalog.pg_operator
    UNION ALL
    SELECT 'pg_catalog.pg_opclass'::regclass, oid, 'operator class', false FROM pg_catalog.pg_opclass
    UNION ALL
    SELECT 'pg_catalog.pg_statistic_ext'::regclass, oid, 'extended statistics', false FROM pg_catalog.pg_statistic_ext
    UNION ALL
    SELECT 'pg_catalog.pg_event_trigger'::regclass, oid, 'event trigger', false FROM pg_catalog.pg_event_trigger
    UNION ALL
    SELECT 'pg_catalog.pg_cast'::regclass, oid, 'cast', false FROM pg_catalog.pg_cast
    UNION ALL
    SELECT 'pg_catalog.pg_collation'::regclass, oid, 'collation', false FROM pg_catalog.pg_collation
    UNION ALL
    SELECT 'pg_catalog.pg_type'::regclass, t.oid,
           CASE t.typtype WHEN 'r' THEN 'range type' WHEN 'm' THEN 'multirange type' ELSE 'base type' END,
           "#,
    crate::user_types::array_companion_sql!(),
    r#" OR EXISTS (SELECT 1 FROM used_types u WHERE u.oid = t.oid)
      FROM pg_catalog.pg_type t WHERE t.typtype IN ('r', 'm', 'b')
    UNION ALL
    SELECT 'pg_catalog.pg_transform'::regclass, oid, 'transform', false FROM pg_catalog.pg_transform
    UNION ALL
    SELECT 'pg_catalog.pg_largeobject'::regclass, oid, 'large object', false FROM pg_catalog.pg_largeobject_metadata
    UNION ALL
    SELECT 'pg_catalog.pg_tablespace'::regclass, oid, 'tablespace', false FROM pg_catalog.pg_tablespace
)
SELECT o.kind, i.identity,
       CASE WHEN o.classid = 'pg_extension'::regclass AND o.kind = 'extension' THEN
         (SELECT extname::text FROM pg_catalog.pg_extension WHERE oid = o.oid) END AS extension_name,
       (o.oid < 16384 AND o.classid <> 'pg_catalog.pg_largeobject'::regclass) AS system,
       EXISTS (SELECT 1 FROM extension_members e WHERE e.classid = o.classid AND e.objid = o.oid) AS extension_member,
       o.model_owned
  FROM objects o CROSS JOIN LATERAL pg_catalog.pg_identify_object(o.classid, o.oid, 0) i
"#
);

#[derive(Debug, Default)]
pub(super) struct Notices {
    pub objects: Vec<String>,
    pub extensions: std::collections::BTreeMap<pbps_model::ObjectLocalName, String>,
}

pub(super) fn notices(rows: &[Value]) -> Result<Notices, DbError> {
    let mut out = Notices::default();
    for row in rows {
        // Decode every field even when a flag would exclude the object:
        // an unreadable row is never an empty census.
        let kind = super::text(row, "kind")?;
        let identity = super::optional_text(row, "identity")?;
        let system = super::flag(row, "system")?;
        let extension = super::flag(row, "extension_member")?;
        let owned = super::flag(row, "model_owned")?;
        // A lost identity is unreadable evidence even for an excluded object
        // (DEC-2558.1). Missing keys and malformed flags remain shape errors.
        let identity = identity.ok_or_else(|| {
            DbError::Refused(format!(
                "the catalog changed while it was being read: a census object (kind `{}`) \
                 was there when the catalog was scanned and gone when its identity was resolved.\n\
                 Something applied DDL to this database during the pull. \
                 Run it again when the other change has finished.",
                pbps_model::escape::Escaped(&kind),
            ))
        })?;
        let name = if kind == "extension" {
            let name = super::text(row, "extension_name")?;
            if name.is_empty() {
                return Err(DbError::BadRow("extension census name is empty".into()));
            }
            Some(name)
        } else {
            None
        };
        if !system && !extension && !owned {
            let message = format!(
                "{kind} `{}` is not held by this model; it is left out of the pull and schema comparison",
                pbps_model::escape::Escaped(&identity),
            );
            if let Some(name) = name {
                out.extensions.insert(name.into(), message);
            } else {
                out.objects.push(message);
            }
        }
    }
    out.objects.sort();
    out.objects.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_classification_excludes_system_members_and_model_owned_objects() {
        for system in [false, true] {
            for extension in [false, true] {
                for owned in [false, true] {
                    let row = serde_json::json!({"kind":"operator", "identity":"app.===(integer,integer)",
                        "system":system,"extension_member":extension,"model_owned":owned});
                    let found = notices(&[row]).unwrap();
                    assert_eq!(!found.objects.is_empty(), !system && !extension && !owned);
                    if let Some(message) = found.objects.first() {
                        assert!(message.contains("operator `app.===(integer,integer)`"));
                    }
                }
            }
        }
        assert!(notices(&[]).unwrap().objects.is_empty());
    }

    #[test]
    fn excluded_and_malformed_rows_cannot_be_read_as_an_empty_census() {
        let row = serde_json::json!({"kind":"extension","extension_name":"odd\nname","identity":"odd\nname", "system":true,
            "extension_member":false,"model_owned":false});
        for key in [
            "kind",
            "extension_name",
            "identity",
            "system",
            "extension_member",
            "model_owned",
        ] {
            let mut missing = row.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(notices(&[missing]).is_err(), "{key}");
            let mut unreadable = row.clone();
            unreadable[key] = Value::Null;
            assert!(notices(&[unreadable]).is_err(), "{key}");
        }
        let mut user = row;
        user["system"] = false.into();
        assert!(
            notices(&[user])
                .unwrap()
                .extensions
                .values()
                .next()
                .unwrap()
                .contains("odd\\nname")
        );
    }

    #[test]
    fn census2558_null_identity_is_catalog_movement_for_every_classification() {
        for system in [false, true] {
            for extension in [false, true] {
                for owned in [false, true] {
                    let row = serde_json::json!({"kind":"base type", "identity":null,
                        "system":system, "extension_member":extension, "model_owned":owned});
                    assert_catalog_moved(&[row]);
                }
            }
        }
    }

    #[test]
    fn census2558_missing_and_wrong_fields_stay_shape_errors_with_null_identity() {
        let row = serde_json::json!({"kind":"base type", "identity":null,
            "system":true, "extension_member":true, "model_owned":true});
        for key in [
            "kind",
            "identity",
            "system",
            "extension_member",
            "model_owned",
        ] {
            let mut missing = row.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                matches!(notices(&[missing]), Err(DbError::BadRow(_))),
                "{key}"
            );
            let mut wrong = row.clone();
            wrong[key] = serde_json::json!(7);
            assert!(
                matches!(notices(&[wrong]), Err(DbError::BadRow(_))),
                "{key}"
            );
            if key != "identity" {
                let mut unreadable = row.clone();
                unreadable[key] = Value::Null;
                assert!(
                    matches!(notices(&[unreadable]), Err(DbError::BadRow(_))),
                    "{key}"
                );
            }
        }
    }

    #[test]
    fn census2558_an_empty_rowset_remains_empty() {
        let found = notices(&[]).unwrap();
        assert!(found.objects.is_empty() && found.extensions.is_empty());
    }

    fn assert_catalog_moved(rows: &[Value]) {
        let error = notices(rows).unwrap_err();
        assert!(matches!(&error, DbError::Refused(_)), "{error}");
        let message = error.to_string();
        assert!(
            message.contains("the catalog changed while it was being read"),
            "{message}"
        );
        assert!(message.contains("kind `base type`"), "{message}");
        assert!(message.contains("Run it again"), "{message}");
        assert!(!message.contains("gone out of step"), "{message}");
    }

    #[tokio::test]
    #[ignore = "needs live PostgreSQL 16; set PBPS_TEST_PG_OLD_DB"]
    async fn census2558_dropped_identity_is_catalog_movement_on_pg16() {
        dropped_identity("PBPS_TEST_PG_OLD_DB").await;
    }

    #[tokio::test]
    #[ignore = "needs live PostgreSQL 18; set PBPS_TEST_PG_DB"]
    async fn census2558_dropped_identity_is_catalog_movement_on_pg18() {
        dropped_identity("PBPS_TEST_PG_DB").await;
    }

    async fn dropped_identity(variable: &str) {
        use pbps_db::{Conn, Driver};
        use std::time::{Duration, Instant};

        let server = std::env::var(variable).expect("the pinned PostgreSQL fixture");
        let mut reader = Conn::connect(Driver::Postgres, &server).await.unwrap();
        let mut dropper = Conn::connect(Driver::Postgres, &server).await.unwrap();
        let schema = format!("census2558_{}", std::process::id());
        dropper
            .execute(&format!(
                "CREATE SCHEMA {schema}; CREATE TYPE {schema}.e AS ENUM ('x')"
            ))
            .await
            .unwrap();
        let rows = dropper
            .query(&format!(
                "SELECT typarray::int8 AS oid FROM pg_catalog.pg_type \
             WHERE oid = '{schema}.e'::regtype"
            ))
            .await
            .unwrap();
        let array_oid = super::super::number(&rows[0], "oid").unwrap();
        let rows = reader
            .query("SELECT pg_catalog.pg_backend_pid()::int8 AS pid")
            .await
            .unwrap();
        let pid = super::super::number(&rows[0], "pid").unwrap();
        let rows = dropper
            .query(&format!(
                "SELECT identity FROM pg_catalog.pg_identify_object(\
             'pg_catalog.pg_type'::regclass, {array_oid}::oid, 0)"
            ))
            .await
            .unwrap();
        assert_eq!(
            super::super::text(&rows[0], "identity").unwrap(),
            format!("{schema}.e[]")
        );
        dropper
            .query(&format!(
                "SELECT pg_catalog.pg_advisory_lock(2558, {pid}::int)"
            ))
            .await
            .unwrap();

        reader
            .execute("BEGIN ISOLATION LEVEL READ COMMITTED")
            .await
            .unwrap();

        // The materialized row must pass the advisory barrier before the
        // syscache lookup. A queued lock proves the scan/deparse gap without
        // relying on a sleep or changing the production census query.
        let query = format!(
            "WITH scanned AS MATERIALIZED (\
               SELECT oid FROM pg_catalog.pg_type WHERE oid = {array_oid}::oid\
             ), held AS MATERIALIZED (\
               SELECT oid, pg_catalog.pg_advisory_xact_lock(2558, {pid}::int) FROM scanned\
             ) SELECT jsonb_build_object(\
                 'kind', 'base type', 'identity', i.identity,\
                 'system', false, 'extension_member', false, 'model_owned', true)::text AS payload \
               FROM held o CROSS JOIN LATERAL pg_catalog.pg_identify_object(\
                 'pg_catalog.pg_type'::regclass, o.oid, 0) i"
        );
        let (rows, ()) = tokio::join!(reader.query(&query), async {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let waiting = dropper
                    .query(&format!(
                        "SELECT EXISTS (SELECT FROM pg_catalog.pg_locks \
                     WHERE pid = {pid} AND locktype = 'advisory' AND NOT granted) AS waiting"
                    ))
                    .await
                    .unwrap();
                if waiting[0].try_get::<bool>("waiting").unwrap() == Some(true) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the statement never reached its held row"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            dropper
                .execute(&format!("DROP TYPE {schema}.e"))
                .await
                .unwrap();
            let unlocked = dropper
                .query(&format!(
                    "SELECT pg_catalog.pg_advisory_unlock(2558, {pid}::int) AS unlocked"
                ))
                .await
                .unwrap();
            assert_eq!(unlocked[0].try_get::<bool>("unlocked").unwrap(), Some(true));
        });
        // Clean the fixture before checking the decoder so the revert witness
        // also leaves no schema or session lock behind on its expected failure.
        reader.execute("ROLLBACK").await.unwrap();
        dropper
            .execute(&format!("DROP SCHEMA {schema}"))
            .await
            .unwrap();
        let rows = rows.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the held scan must retain the dropped address"
        );
        let payload = super::super::text(&rows[0], "payload").unwrap();
        let row: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            row.get("identity"),
            Some(&Value::Null),
            "the identity lookup must observe the drop: {row}"
        );
        assert_catalog_moved(&[row]);
    }
}
