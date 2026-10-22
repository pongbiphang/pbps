#[derive(serde::Deserialize)]
pub struct Cases {
    pub restore: Vec<(String, String)>,
    pub refuse: Vec<(String, String)>,
    pub residual: Vec<(String, String)>,
}

pub fn cases() -> Cases {
    serde_json::from_str(include_str!("../live/scratch2236-catalogs.json")).unwrap()
}

// Compare the retained definitions as well as inventory names: a failed
// cleanup must not drop a system language or partially alter its owner.
pub const BUILTIN_STATE: &str = "SELECT (SELECT \
 JSON_QUERY((SELECT * FROM sys.routes ORDER BY name FOR JSON PATH,INCLUDE_NULL_VALUES)) AS routes,\
 JSON_QUERY((SELECT * FROM sys.assemblies ORDER BY name FOR JSON PATH,INCLUDE_NULL_VALUES)) AS assemblies,\
 JSON_QUERY((SELECT * FROM sys.external_languages ORDER BY language FOR JSON PATH,INCLUDE_NULL_VALUES)) AS languages,\
 JSON_QUERY((SELECT * FROM sys.schemas ORDER BY name FOR JSON PATH,INCLUDE_NULL_VALUES)) AS schemas,\
 JSON_QUERY((SELECT * FROM sys.database_permissions ORDER BY class,major_id,minor_id,\
 grantee_principal_id,grantor_principal_id,permission_name,state FOR JSON PATH,INCLUDE_NULL_VALUES)) AS permissions \
 FOR JSON PATH,INCLUDE_NULL_VALUES,WITHOUT_ARRAY_WRAPPER) AS state;";
