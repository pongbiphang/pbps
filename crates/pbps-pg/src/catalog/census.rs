//! Report-only census, sampled with the ordinary catalog batch (DEC-2444.1).
use pbps_db::DbError;
use serde_json::Value;

// Object addresses let PostgreSQL render identities uniformly, including
// operator signatures and objects without a namespace. Large objects use
// pg_largeobject's address, although only its readable metadata is queried.
pub(super) const QUERY: &str = r#"
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
           t.typelem <> 0 OR EXISTS (SELECT 1 FROM used_types u WHERE u.oid = t.oid)
      FROM pg_catalog.pg_type t WHERE t.typtype IN ('r', 'm', 'b')
    UNION ALL
    SELECT 'pg_catalog.pg_transform'::regclass, oid, 'transform', false FROM pg_catalog.pg_transform
    UNION ALL
    SELECT 'pg_catalog.pg_largeobject'::regclass, oid, 'large object', false FROM pg_catalog.pg_largeobject_metadata
    UNION ALL
    SELECT 'pg_catalog.pg_tablespace'::regclass, oid, 'tablespace', false FROM pg_catalog.pg_tablespace
)
SELECT o.kind, i.identity,
       (o.oid < 16384 AND o.classid <> 'pg_catalog.pg_largeobject'::regclass) AS system,
       EXISTS (SELECT 1 FROM extension_members e WHERE e.classid = o.classid AND e.objid = o.oid) AS extension_member,
       o.model_owned
  FROM objects o CROSS JOIN LATERAL pg_catalog.pg_identify_object(o.classid, o.oid, 0) i
"#;

pub(super) fn notices(rows: &[Value]) -> Result<Vec<String>, DbError> {
    let mut out = Vec::new();
    for row in rows {
        // Decode every field even when a flag would exclude the object:
        // an unreadable row is never an empty census.
        let kind = super::text(row, "kind")?;
        let identity = super::text(row, "identity")?;
        let system = super::flag(row, "system")?;
        let extension = super::flag(row, "extension_member")?;
        let owned = super::flag(row, "model_owned")?;
        if !system && !extension && !owned {
            out.push(format!(
                "{kind} `{}` is not held by this model; it is left out of the pull and schema comparison",
                pbps_model::escape::Escaped(&identity),
            ));
        }
    }
    out.sort();
    out.dedup();
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
                    assert_eq!(!found.is_empty(), !system && !extension && !owned);
                    if let Some(message) = found.first() {
                        assert!(message.contains("operator `app.===(integer,integer)`"));
                    }
                }
            }
        }
        assert!(notices(&[]).unwrap().is_empty());
    }

    #[test]
    fn excluded_and_malformed_rows_cannot_be_read_as_an_empty_census() {
        let row = serde_json::json!({"kind":"extension","identity":"odd\nname", "system":true,
            "extension_member":false,"model_owned":false});
        for key in [
            "kind",
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
        assert!(notices(&[user]).unwrap()[0].contains("odd\\nname"));
    }
}
