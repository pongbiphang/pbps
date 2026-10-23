//! Engine-resolved identities for staged comparison (DEC-2268.1).
//! Resolution proves presence; it never licenses changing a stored spelling.

use super::{CaptureError, ObjectIdentity, class};
use pbps_db::transport::QueryConnection;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const INDEX: &str = "sys.indexes";

pub(super) fn member(class: &str, parent: &ObjectIdentity, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name: vec![name.into()],
        signature: vec![parent.clone()],
    }
}

pub(super) struct Names(BTreeMap<ObjectIdentity, ObjectIdentity>);

impl Names {
    pub(super) fn get(&self, requested: &ObjectIdentity) -> Option<&ObjectIdentity> {
        self.0.get(requested)
    }

    pub(super) async fn read(
        conn: &mut impl QueryConnection,
        requested: &BTreeSet<ObjectIdentity>,
    ) -> Result<Self, CaptureError> {
        let requested: Vec<_> = requested.iter().collect();
        let mut found = BTreeMap::new();
        for chunk in requested.chunks(128) {
            let values = chunk
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let (kind, schema, object, member) = parts(id)?;
                    Ok(format!(
                        "({i}, {kind}, {}, {}, {})",
                        crate::ident::literal(schema),
                        crate::ident::literal(object),
                        crate::ident::literal(member),
                    ))
                })
                .collect::<Result<Vec<_>, CaptureError>>()?
                .join(", ");
            // QUOTENAME keeps dots inside components. The engine, including
            // its accent and case rules, resolves every namespace separately.
            let sql = format!(
                "SELECT CONVERT(nvarchar(20), q.seq) AS seq, s.name AS schema_name, \
                 CASE WHEN q.kind IN (2,5,6) THEN t.name ELSE o.name END AS object_name, \
                 CASE WHEN q.kind IN (3,5) THEN c.name ELSE i.name END AS member_name \
                 FROM (VALUES {values}) q(seq,kind,s,n,m) \
                 LEFT JOIN sys.schemas s ON s.schema_id=SCHEMA_ID(q.s) \
                 LEFT JOIN sys.objects o ON q.kind IN (1,3,4) \
                   AND o.object_id=OBJECT_ID(QUOTENAME(q.s)+N'.'+QUOTENAME(q.n)) \
                 LEFT JOIN sys.types t ON q.kind IN (2,5,6) \
                   AND t.user_type_id=TYPE_ID(QUOTENAME(q.s)+N'.'+QUOTENAME(q.n)) \
                 LEFT JOIN sys.table_types tt ON tt.user_type_id=t.user_type_id \
                 LEFT JOIN sys.columns c ON q.kind IN (3,5) \
                   AND c.object_id=COALESCE(o.object_id,tt.type_table_object_id) \
                   AND c.name=q.m COLLATE DATABASE_DEFAULT \
                 LEFT JOIN sys.indexes i ON q.kind IN (4,6) \
                   AND i.object_id=COALESCE(o.object_id,tt.type_table_object_id) \
                   AND i.name=q.m COLLATE DATABASE_DEFAULT;"
            );
            let rows = conn.query(&sql).await.map_err(|_| CaptureError::Read)?;
            let mut seen = BTreeSet::new();
            for row in rows {
                let text = |key| {
                    row.try_get::<&str>(key)
                        .map_err(|_| CaptureError::Incomplete)
                };
                let seq: usize = text("seq")?
                    .ok_or(CaptureError::Incomplete)?
                    .parse()
                    .map_err(|_| CaptureError::Incomplete)?;
                let id = chunk.get(seq).copied().ok_or(CaptureError::Incomplete)?;
                if !seen.insert(seq) {
                    return Err(CaptureError::Incomplete);
                }
                let Some(schema) = text("schema_name")? else {
                    continue;
                };
                let mut actual = id.clone();
                if id.class == class::SCHEMA {
                    actual.name = vec![schema.into()];
                } else {
                    let Some(object) = text("object_name")? else {
                        continue;
                    };
                    if id.signature.is_empty() {
                        actual.name = vec![schema.into(), object.into()];
                    } else {
                        let Some(member) = text("member_name")? else {
                            continue;
                        };
                        actual.name = vec![member.into()];
                        actual.signature[0].name = vec![schema.into(), object.into()];
                    }
                }
                found.insert(id.clone(), actual);
            }
            // A NULL resolved name is absence. A missing or duplicate result
            // row is unreadable evidence and must not become absence.
            if seen.len() != chunk.len() {
                return Err(CaptureError::Incomplete);
            }
        }
        Ok(Self(found))
    }
}

fn parts(id: &ObjectIdentity) -> Result<(u8, &str, &str, &str), CaptureError> {
    match (
        id.class.as_str(),
        id.name.as_slice(),
        id.signature.as_slice(),
    ) {
        (class::SCHEMA, [schema], []) => Ok((0, schema, "", "")),
        (class::OBJECT, [schema, name], []) => Ok((1, schema, name, "")),
        (class::TYPE, [schema, name], []) => Ok((2, schema, name, "")),
        (class::COLUMN | INDEX, [name], [parent]) if parent.signature.is_empty() => {
            let [schema, object] = parent.name.as_slice() else {
                return Err(CaptureError::Incomplete);
            };
            let kind = match (id.class.as_str(), parent.class.as_str()) {
                (class::COLUMN, class::OBJECT) => 3,
                (INDEX, class::OBJECT) => 4,
                (class::COLUMN, class::TYPE) => 5,
                (INDEX, class::TYPE) => 6,
                _ => return Err(CaptureError::Incomplete),
            };
            Ok((kind, schema, object, name))
        }
        _ => Err(CaptureError::Incomplete),
    }
}
