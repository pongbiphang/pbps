//! The SQL Server 17 OBJECTPROPERTYEX manifest (DEC-2268.1).
//! The API enumerates no property names. Extend this manifest deliberately;
//! unlike scalar catalog columns, future names are not discovered automatically.

use super::{CaptureError, read::Object};
use pbps_db::transport::QueryConnection;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyDisposition {
    Compare,
    Owner,
    Schema,
    FulltextCatalog,
    Column,
    /// Redundant child summaries; retained keys and cascade barriers are
    /// compared separately, including when a summary mixes kept and skipped children.
    ChildSummary,
    /// Data and progress never enter a canonical capture.
    Data,
}

pub const OBJECT_PROPERTIES: &[&str] = &[
    "BaseType",
    "CnstIsClustKey",
    "CnstIsColumn",
    "CnstIsDeleteCascade",
    "CnstIsDisabled",
    "CnstIsNonclustKey",
    "CnstIsNotRepl",
    "CnstIsNotTrusted",
    "CnstIsUpdateCascade",
    "ExecIsAfterTrigger",
    "ExecIsAnsiNullsOn",
    "ExecIsDeleteTrigger",
    "ExecIsFirstDeleteTrigger",
    "ExecIsFirstInsertTrigger",
    "ExecIsFirstUpdateTrigger",
    "ExecIsInsertTrigger",
    "ExecIsInsteadOfTrigger",
    "ExecIsLastDeleteTrigger",
    "ExecIsLastInsertTrigger",
    "ExecIsLastUpdateTrigger",
    "ExecIsQuotedIdentOn",
    "ExecIsStartup",
    "ExecIsTriggerDisabled",
    "ExecIsTriggerNotForRepl",
    "ExecIsUpdateTrigger",
    "ExecIsWithNativeCompilation",
    "HasAfterTrigger",
    "HasDeleteTrigger",
    "HasInsertTrigger",
    "HasInsteadOfTrigger",
    "HasUpdateTrigger",
    "IsAnsiNullsOn",
    "IsCheckCnst",
    "IsConstraint",
    "IsDefault",
    "IsDefaultCnst",
    "IsDeterministic",
    "IsEncrypted",
    "IsExecuted",
    "IsExtendedProc",
    "IsForeignKey",
    "IsIndexed",
    "IsIndexable",
    "IsInlineFunction",
    "IsMSShipped",
    "IsPrecise",
    "IsPrimaryKey",
    "IsProcedure",
    "IsQuotedIdentOn",
    "IsQueue",
    "IsReplProc",
    "IsRule",
    "IsScalarFunction",
    "IsSchemaBound",
    "IsSystemTable",
    "IsSystemVerified",
    "IsTable",
    "IsTableFunction",
    "IsTrigger",
    "IsUniqueCnst",
    "IsUserTable",
    "IsView",
    "OwnerId",
    "SchemaId",
    "SystemDataAccess",
    "TableDeleteTrigger",
    "TableDeleteTriggerCount",
    "TableFullTextMergeStatus",
    "TableFullTextBackgroundUpdateIndexOn",
    "TableFulltextCatalogId",
    "TableFullTextChangeTrackingOn",
    "TableFulltextDocsProcessed",
    "TableFulltextFailCount",
    "TableFulltextItemCount",
    "TableFulltextKeyColumn",
    "TableFulltextPendingChanges",
    "TableFulltextPopulateStatus",
    "TableFullTextSemanticExtraction",
    "TableHasActiveFulltextIndex",
    "TableHasCheckCnst",
    "TableHasClustIndex",
    "TableHasDefaultCnst",
    "TableHasDeleteTrigger",
    "TableHasForeignKey",
    "TableHasForeignRef",
    "TableHasIdentity",
    "TableHasIndex",
    "TableHasInsertTrigger",
    "TableHasNonclustIndex",
    "TableHasPrimaryKey",
    "TableHasRowGuidCol",
    "TableHasTextImage",
    "TableHasTimestamp",
    "TableHasUniqueCnst",
    "TableHasUpdateTrigger",
    "TableHasVarDecimalStorageFormat",
    "TableInsertTrigger",
    "TableInsertTriggerCount",
    "TableIsFake",
    "TableIsLockedOnBulkLoad",
    "TableIsMemoryOptimized",
    "TableIsPinned",
    "TableTextInRowLimit",
    "TableUpdateTrigger",
    "TableUpdateTriggerCount",
    "UserDataAccess",
    "TableHasColumnSet",
    "Cardinality",
    "TableTemporalType",
];

pub fn property_disposition(name: &str) -> Option<PropertyDisposition> {
    use PropertyDisposition::*;
    Some(match name {
        "BaseType"
        | "CnstIsClustKey"
        | "CnstIsColumn"
        | "CnstIsDeleteCascade"
        | "CnstIsDisabled"
        | "CnstIsNonclustKey"
        | "CnstIsNotRepl"
        | "CnstIsNotTrusted"
        | "CnstIsUpdateCascade"
        | "ExecIsAfterTrigger"
        | "ExecIsAnsiNullsOn"
        | "ExecIsDeleteTrigger"
        | "ExecIsFirstDeleteTrigger"
        | "ExecIsFirstInsertTrigger"
        | "ExecIsFirstUpdateTrigger"
        | "ExecIsInsertTrigger"
        | "ExecIsInsteadOfTrigger"
        | "ExecIsLastDeleteTrigger"
        | "ExecIsLastInsertTrigger"
        | "ExecIsLastUpdateTrigger"
        | "ExecIsQuotedIdentOn"
        | "ExecIsStartup"
        | "ExecIsTriggerDisabled"
        | "ExecIsTriggerNotForRepl"
        | "ExecIsUpdateTrigger"
        | "ExecIsWithNativeCompilation"
        | "IsAnsiNullsOn"
        | "IsCheckCnst"
        | "IsConstraint"
        | "IsDefault"
        | "IsDefaultCnst"
        | "IsDeterministic"
        | "IsEncrypted"
        | "IsExecuted"
        | "IsExtendedProc"
        | "IsForeignKey"
        | "IsIndexable"
        | "IsInlineFunction"
        | "IsMSShipped"
        | "IsPrecise"
        | "IsPrimaryKey"
        | "IsProcedure"
        | "IsQuotedIdentOn"
        | "IsQueue"
        | "IsReplProc"
        | "IsRule"
        | "IsScalarFunction"
        | "IsSchemaBound"
        | "IsSystemTable"
        | "IsSystemVerified"
        | "IsTable"
        | "IsTableFunction"
        | "IsTrigger"
        | "IsUniqueCnst"
        | "IsUserTable"
        | "IsView"
        | "SystemDataAccess"
        | "TableFullTextBackgroundUpdateIndexOn"
        | "TableFullTextChangeTrackingOn"
        | "TableFullTextSemanticExtraction"
        | "TableHasActiveFulltextIndex"
        | "TableHasIdentity"
        | "TableHasPrimaryKey"
        | "TableHasRowGuidCol"
        | "TableHasTextImage"
        | "TableHasTimestamp"
        | "TableHasUniqueCnst"
        | "TableHasVarDecimalStorageFormat"
        | "TableIsFake"
        | "TableIsMemoryOptimized"
        | "TableIsPinned"
        | "TableTextInRowLimit"
        | "UserDataAccess"
        | "TableHasColumnSet"
        | "TableTemporalType" => Compare,
        "OwnerId" => Owner,
        "SchemaId" => Schema,
        "TableFulltextCatalogId" => FulltextCatalog,
        "TableFulltextKeyColumn" => Column,
        "HasAfterTrigger"
        | "HasDeleteTrigger"
        | "HasInsertTrigger"
        | "HasInsteadOfTrigger"
        | "HasUpdateTrigger"
        | "IsIndexed"
        | "TableDeleteTrigger"
        | "TableDeleteTriggerCount"
        | "TableHasCheckCnst"
        | "TableHasClustIndex"
        | "TableHasDefaultCnst"
        | "TableHasDeleteTrigger"
        | "TableHasForeignKey"
        | "TableHasForeignRef"
        | "TableHasIndex"
        | "TableHasInsertTrigger"
        | "TableHasNonclustIndex"
        | "TableHasUpdateTrigger"
        | "TableInsertTrigger"
        | "TableInsertTriggerCount"
        | "TableUpdateTrigger"
        | "TableUpdateTriggerCount" => ChildSummary,
        "Cardinality"
        | "TableIsLockedOnBulkLoad"
        | "TableFullTextMergeStatus"
        | "TableFulltextDocsProcessed"
        | "TableFulltextFailCount"
        | "TableFulltextItemCount"
        | "TableFulltextPendingChanges"
        | "TableFulltextPopulateStatus" => Data,
        _ => return None,
    })
}

pub(super) async fn read(
    conn: &mut impl QueryConnection,
    objects: &[&Object],
) -> Result<BTreeMap<i64, BTreeMap<String, Value>>, CaptureError> {
    let names = OBJECT_PROPERTIES
        .iter()
        .map(|name| format!("(N'{name}')"))
        .collect::<Vec<_>>()
        .join(",");
    let mut out: BTreeMap<i64, BTreeMap<String, Value>> = BTreeMap::new();
    // One row per property even for NULL. The independently qualified catalog
    // inventory distinguishes a disappearing/unreadable object from non-applicability.
    for chunk in objects.chunks(128) {
        let ids = chunk
            .iter()
            .map(|o| o.id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT CONVERT(nvarchar(20),o.object_id) AS id,p.name, \
             CONVERT(nvarchar(128),SQL_VARIANT_PROPERTY(v.value,'BaseType')) AS kind, \
             CONVERT(nvarchar(max),v.value) AS value, \
             CONVERT(nvarchar(1),HAS_PERMS_BY_NAME(QUOTENAME(SCHEMA_NAME(o.schema_id))+N'.'+QUOTENAME(o.name),'OBJECT','VIEW DEFINITION')) AS visible \
             FROM sys.objects o CROSS JOIN (VALUES {names}) p(name) \
             CROSS APPLY (VALUES(OBJECTPROPERTYEX(o.object_id,p.name))) v(value) \
             WHERE o.object_id IN ({ids});"
        );
        for row in conn.query(&sql).await.map_err(|_| CaptureError::Read)? {
            let text = |field| -> Result<Option<String>, CaptureError> {
                row.try_get::<&str>(field)
                    .map(|v| v.map(str::to_owned))
                    .map_err(|_| CaptureError::Incomplete)
            };
            if text("visible")?.as_deref() != Some("1") {
                return Err(CaptureError::Incomplete);
            }
            let id = text("id")?
                .and_then(|v| v.parse().ok())
                .ok_or(CaptureError::Incomplete)?;
            let name = text("name")?.ok_or(CaptureError::Incomplete)?;
            property_disposition(&name).ok_or(CaptureError::Incomplete)?;
            let value = match (text("kind")?, text("value")?) {
                (None, None) => Value::Null,
                (Some(kind), Some(value)) => match kind.as_str() {
                    "int" | "bigint" | "smallint" | "tinyint" => {
                        json!(value.parse::<i64>().map_err(|_| CaptureError::Incomplete)?)
                    }
                    // BaseType is char(2); padding is not part of its value.
                    "char" | "varchar" | "nvarchar" | "nchar" => json!(value.trim_end()),
                    _ => return Err(CaptureError::Incomplete),
                },
                _ => return Err(CaptureError::Incomplete),
            };
            if out.entry(id).or_default().insert(name, value).is_some() {
                return Err(CaptureError::Incomplete);
            }
        }
        for object in chunk {
            if out.get(&object.id).map(BTreeMap::len) != Some(OBJECT_PROPERTIES.len()) {
                return Err(CaptureError::Incomplete);
            }
        }
    }
    for properties in out.values_mut() {
        properties.retain(|name, _| property_disposition(name) != Some(PropertyDisposition::Data));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_manifest_name_has_one_explicit_disposition() {
        let unique: std::collections::BTreeSet<_> = OBJECT_PROPERTIES.iter().collect();
        assert_eq!(unique.len(), OBJECT_PROPERTIES.len());
        for name in OBJECT_PROPERTIES {
            assert!(property_disposition(name).is_some(), "{name}");
        }
        assert_eq!(
            property_disposition("TableFullTextFutureConfiguration"),
            None
        );
        assert_eq!(
            property_disposition("IsDeterministic"),
            Some(PropertyDisposition::Compare)
        );
        assert_eq!(
            property_disposition("TableFullTextChangeTrackingOn"),
            Some(PropertyDisposition::Compare)
        );
        assert_eq!(
            property_disposition("TableFulltextPopulateStatus"),
            Some(PropertyDisposition::Data)
        );
    }
}
