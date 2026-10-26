//! One inventory serves initial emptiness and final cleanup. Catalog reads
//! are never allowed to fail into an empty result (DEC-2236.1). Built-ins
//! follow the rehearsal and separation sets, not a build snapshot (DEC-2263.1).

use super::{ident, literal, text};
use pbps_db::transport::{ExecuteConnection, QueryConnection};
use pbps_db::{DbError, Row};
use std::collections::{BTreeMap, BTreeSet};

/// Explicit ownership lives in these catalogs when they expose principal_id.
pub(super) const OWNERSHIP_CATALOGS: &[&str] = &[
    "schemas",
    "objects",
    "database_principals",
    "types",
    "assemblies",
    "asymmetric_keys",
    "certificates",
    "symmetric_keys",
    "service_contracts",
    "service_message_types",
    "services",
    "routes",
    "remote_service_bindings",
    "fulltext_catalogs",
    "fulltext_stoplists",
    "registered_search_property_lists",
    "xml_schema_collections",
    "external_languages",
    "external_models",
    "external_libraries",
];

const CLASS_CATALOGS: &[(&str, &str)] = &[
    ("APPLICATION ROLE", "database_principals"),
    ("ASSEMBLY", "assemblies"),
    ("ASYMMETRIC KEY", "asymmetric_keys"),
    ("CERTIFICATE", "certificates"),
    ("CONTRACT", "service_contracts"),
    ("DATABASE", "databases"),
    ("DATABASE SCOPED CREDENTIAL", "database_scoped_credentials"),
    ("EXTERNAL LANGUAGE", "external_languages"),
    ("EXTERNAL MODEL", "external_models"),
    ("FULLTEXT CATALOG", "fulltext_catalogs"),
    ("FULLTEXT STOPLIST", "fulltext_stoplists"),
    ("MESSAGE TYPE", "service_message_types"),
    ("OBJECT", "objects"),
    ("REMOTE SERVICE BINDING", "remote_service_bindings"),
    ("ROLE", "database_principals"),
    ("ROUTE", "routes"),
    ("SCHEMA", "schemas"),
    ("SEARCH PROPERTY LIST", "registered_search_property_lists"),
    ("SERVICE", "services"),
    ("SYMMETRIC KEY", "symmetric_keys"),
    ("TYPE", "types"),
    ("USER", "database_principals"),
    ("XML SCHEMA COLLECTION", "xml_schema_collections"),
];

const FIXED: &[&str] = &[
    "dbo",
    "guest",
    "INFORMATION_SCHEMA",
    "sys",
    "db_owner",
    "db_accessadmin",
    "db_securityadmin",
    "db_ddladmin",
    "db_backupoperator",
    "db_datareader",
    "db_datawriter",
    "db_denydatareader",
    "db_denydatawriter",
];

#[derive(Debug)]
pub struct Entry {
    pub name: String,
    pub(crate) staged_root: Option<super::super::capture::ObjectIdentity>,
    pub(crate) staged_parent: Option<super::super::capture::ObjectIdentity>,
    drop: Option<String>,
    rank: u16,
    key: Option<Securable>,
}

impl Entry {
    pub(crate) fn staging_key(&self) -> String {
        format!("{:?}:{:?}:{}", self.key, self.staged_parent, self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Securable {
    Object(i32),
    Principal(i32),
    Certificate(i32),
    AsymmetricKey(i32),
    SymmetricKey(i32),
    Type(i32),
    Credential(i32),
    ExternalModel(i32),
    Assembly(i32),
}

type Identity = fn(i32) -> Securable;

#[derive(Debug, Default)]
pub struct Inventory {
    pub entries: Vec<Entry>,
    dependencies: BTreeSet<(Securable, Securable)>,
    type_permissions: BTreeMap<Securable, Vec<String>>,
}

impl Inventory {
    pub fn names(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.name.clone())
            .chain(self.type_permissions.values().flatten().cloned())
            .collect()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.type_permissions.is_empty()
    }
    fn type_permission(
        &mut self,
        type_id: i32,
        parent: super::super::capture::ObjectIdentity,
        name: String,
    ) {
        if self
            .entries
            .iter()
            .any(|entry| entry.key == Some(Securable::Type(type_id)) && entry.drop.is_some())
        {
            // On 17.0, CASCADE can remove another recorded grantor's grant
            // option, making its later REVOKE fail with 15151 (DEC-2269.2).
            // DROP TYPE removes all class-6 rows without replay-order assumptions.
            // Only our recorded type drop owns these rows; the final read
            // still refuses a failed drop or surviving ACL.
            self.type_permissions
                .entry(Securable::Type(type_id))
                .or_default()
                .push(name);
            return;
        }
        self.add(name, None, 5, None);
        self.entries.last_mut().unwrap().staged_parent = Some(parent);
    }
    fn add(&mut self, name: String, drop: Option<String>, rank: u16, key: Option<Securable>) {
        self.entries.push(Entry {
            name,
            staged_root: None,
            staged_parent: None,
            drop,
            rank,
            key,
        });
    }

    fn unrestorable(&mut self, name: String, rank: u16) {
        self.add(
            format!(
                "{name}: this database cannot be restored to a fresh state; \
                 supply a new scratch database"
            ),
            None,
            rank,
            None,
        );
    }
}

async fn rows(
    conn: &mut impl QueryConnection,
    catalog: &str,
    sql: &str,
) -> Result<Vec<Row>, DbError> {
    conn.query(sql).await.map_err(|error| {
        DbError::Refused(format!(
            "the scratch inventory cannot read sys.{catalog}: {error}"
        ))
    })
}

/// Fixed database-level grants, read separately from system-object SELECTs.
fn database_grants() -> BTreeSet<String> {
    BTreeSet::from([
        "dbo|dbo|CONNECT|GRANT".into(),
        "public|dbo|VIEW ANY COLUMN ENCRYPTION KEY DEFINITION|GRANT".into(),
        "public|dbo|VIEW ANY COLUMN MASTER KEY DEFINITION|GRANT".into(),
    ])
}

pub async fn read(
    conn: &mut impl QueryConnection,
    sandbox: Option<&str>,
) -> Result<Inventory, DbError> {
    super::require_owner(conn).await?;
    let mut result = Inventory::default();
    let database = rows(
        conn,
        "databases",
        "SELECT database_id FROM sys.databases WHERE database_id=DB_ID();",
    )
    .await?;
    if database.len() != 1 {
        return Err(DbError::Refused(
            "sys.databases does not expose the scratch database to its inventory".into(),
        ));
    }
    for row in rows(
        conn,
        "fn_builtin_permissions",
        "SELECT DISTINCT CONVERT(nvarchar(60),class_desc) AS class FROM \
         sys.fn_builtin_permissions(DEFAULT) WHERE class_desc=N'DATABASE' OR \
         parent_class_desc IN(N'DATABASE',N'SCHEMA');",
    )
    .await?
    {
        let class = text(&row, "class")?;
        if !CLASS_CATALOGS.iter().any(|(known, _)| *known == class) {
            return Err(DbError::Refused(format!(
                "the scratch inventory has no catalog for securable class {class}"
            )));
        }
    }
    for row in rows(
        conn,
        "schemas",
        "SELECT name, USER_NAME(principal_id) AS owner FROM sys.schemas;",
    )
    .await?
    {
        let name = text(&row, "name")?;
        if !FIXED.contains(&name.as_str()) {
            result.add(
                format!("SCHEMA {name}"),
                Some(format!("DROP SCHEMA {};", ident(&name))),
                90,
                None,
            );
            result.entries.last_mut().unwrap().staged_root =
                Some(super::super::capture::ObjectIdentity {
                    class: super::super::capture::class::SCHEMA.into(),
                    name: vec![name.clone()],
                    signature: vec![],
                });
        } else if text(&row, "owner")? != name {
            result.add(
                format!("SCHEMA {name} owner"),
                Some(format!(
                    "ALTER AUTHORIZATION ON SCHEMA::{} TO {};",
                    ident(&name),
                    ident(&name)
                )),
                89,
                None,
            );
        }
    }
    for name in FIXED {
        let found = rows(
            conn,
            "schemas",
            &format!("SELECT name FROM sys.schemas WHERE name={};", literal(name)),
        )
        .await?;
        if found.len() != 1 {
            result.unrestorable(format!("missing SCHEMA {name}"), 90);
        }
    }
    let mut principals = BTreeSet::new();
    for row in rows(
        conn,
        "database_principals",
        "SELECT name,type,principal_id,owning_principal_id,USER_NAME(owning_principal_id) AS \
         owner,default_schema_name,allow_encrypted_value_modifications \
         FROM sys.database_principals;",
    )
    .await?
    {
        let name = text(&row, "name")?;
        principals.insert(name.clone());
        if FIXED.contains(&name.as_str()) || name == "public" {
            if text(&row, "type")? == "R" && row.try_get::<i32>("owning_principal_id")? != Some(1) {
                result.add(
                    format!("ROLE {name} owner {}", text(&row, "owner")?),
                    Some(format!(
                        "ALTER AUTHORIZATION ON ROLE::{} TO dbo;",
                        ident(&name)
                    )),
                    89,
                    None,
                );
            }
            if name == "guest" {
                if row.try_get::<&str>("default_schema_name")? != Some("guest") {
                    result.add(
                        "USER guest default schema".into(),
                        Some("ALTER USER guest WITH DEFAULT_SCHEMA=guest;".into()),
                        89,
                        None,
                    );
                }
                if row.try_get::<bool>("allow_encrypted_value_modifications")? != Some(false) {
                    result.add(
                        "USER guest ALLOW_ENCRYPTED_VALUE_MODIFICATIONS".into(),
                        Some(
                            "ALTER USER guest WITH ALLOW_ENCRYPTED_VALUE_MODIFICATIONS=OFF;".into(),
                        ),
                        89,
                        None,
                    );
                }
            }
            continue;
        }
        if sandbox == Some(name.as_str()) {
            continue;
        }
        let kind = match text(&row, "type")?.as_str() {
            "R" => "ROLE",
            "A" => "APPLICATION ROLE",
            "S" | "U" | "G" | "E" | "X" | "C" | "K" => "USER",
            _ => "UNKNOWN PRINCIPAL",
        };
        let principal = row
            .try_get::<i32>("principal_id")?
            .map(Securable::Principal);
        // Roles may own other roles. A rank alone cannot order a chain of
        // principals, just as it cannot order schema-bound modules.
        if let (Some(principal), Some(owner)) =
            (principal, row.try_get::<i32>("owning_principal_id")?)
        {
            result
                .dependencies
                .insert((principal, Securable::Principal(owner)));
        }
        result.add(
            format!("{kind} {name}"),
            if kind == "UNKNOWN PRINCIPAL" {
                None
            } else {
                Some(format!("DROP {kind} {};", ident(&name)))
            },
            100,
            principal,
        );
    }
    for name in FIXED.iter().copied().chain(["public"]) {
        if !principals.contains(name) {
            result.unrestorable(format!("missing principal {name}"), 100);
        }
    }
    let mut memberships = BTreeSet::new();
    for row in rows(
        conn,
        "database_role_members",
        "SELECT USER_NAME(role_principal_id) AS role,USER_NAME(member_principal_id) AS \
         member FROM sys.database_role_members;",
    )
    .await?
    {
        let role = text(&row, "role")?;
        let member = text(&row, "member")?;
        memberships.insert((role.clone(), member.clone()));
        if role == "db_owner" && (member == "dbo" || sandbox == Some(member.as_str())) {
            continue;
        }
        result.add(
            format!("membership {role}<-{member}"),
            Some(format!(
                "ALTER ROLE {} DROP MEMBER {};",
                ident(&role),
                ident(&member)
            )),
            10,
            None,
        );
    }
    if !memberships.contains(&("db_owner".into(), "dbo".into())) {
        result.unrestorable("missing membership db_owner<-dbo".into(), 10);
    }
    for row in rows(
        conn,
        "objects",
        "SELECT o.object_id,o.parent_object_id,o.type,o.name,SCHEMA_NAME(o.schema_id) AS \
         schema_name, OBJECT_NAME(o.parent_object_id) AS parent_name, \
         CONVERT(bit,CASE WHEN EXISTS(SELECT 1 FROM sys.external_tables e WHERE \
         e.object_id=o.object_id) THEN 1 ELSE 0 END) AS is_external FROM sys.objects o \
         WHERE o.is_ms_shipped=0 AND o.type<>N'TT';",
    )
    .await?
    {
        let name = text(&row, "name")?;
        let schema = text(&row, "schema_name")?;
        let qualified = format!("{}.{}", ident(&schema), ident(&name));
        let kind = text(&row, "type")?;
        let (drop, rank) = match kind.trim() {
            "U" => (
                Some(format!(
                    "DROP {}TABLE {qualified};",
                    if row.try_get::<bool>("is_external")? == Some(true) {
                        "EXTERNAL "
                    } else {
                        ""
                    }
                )),
                50,
            ),
            "SP" => (Some(format!("DROP SECURITY POLICY {qualified};")), 19),
            "V" => (Some(format!("DROP VIEW {qualified};")), 40),
            "P" | "PC" => (Some(format!("DROP PROCEDURE {qualified};")), 40),
            "FN" | "IF" | "TF" | "FS" | "FT" => (Some(format!("DROP FUNCTION {qualified};")), 60),
            "AF" => (Some(format!("DROP AGGREGATE {qualified};")), 40),
            "SN" => (Some(format!("DROP SYNONYM {qualified};")), 40),
            "SO" => (Some(format!("DROP SEQUENCE {qualified};")), 60),
            "SQ" => (Some(format!("DROP QUEUE {qualified};")), 50),
            "TR" | "TA" => (Some(format!("DROP TRIGGER {qualified};")), 30),
            "C" | "D" | "F" | "PK" | "UQ" | "EC" => {
                let parent = row.try_get::<&str>("parent_name")?;
                (
                    if kind.trim() == "D" && parent.is_none() {
                        Some(format!("DROP DEFAULT {qualified};"))
                    } else {
                        parent.map(|parent| {
                            format!(
                                "ALTER TABLE {}.{} DROP CONSTRAINT {};",
                                ident(&schema),
                                ident(parent),
                                ident(&name)
                            )
                        })
                    },
                    if kind.trim() == "F" { 18 } else { 20 },
                )
            }
            "R" => (Some(format!("DROP RULE {qualified};")), 60),
            _ => (None, 50),
        };
        result.add(
            format!("OBJECT {} {schema}.{name}", kind.trim()),
            drop,
            rank,
            row.try_get::<i32>("object_id")?.map(Securable::Object),
        );
        let entry = result.entries.last_mut().unwrap();
        entry.staged_root = Some(super::super::capture::object(&schema, &name));
        entry.staged_parent = row
            .try_get::<&str>("parent_name")?
            .map(|parent| super::super::capture::object(&schema, parent));
    }
    // Child catalogs are read even when sys.objects is empty: a DENY must
    // refuse by catalog name, rather than certify a partial inventory.
    for catalog in [
        "columns",
        "parameters",
        "indexes",
        "check_constraints",
        "default_constraints",
        "foreign_keys",
        "key_constraints",
        "sql_modules",
    ] {
        rows(
            conn,
            catalog,
            &format!(
                "SELECT c.object_id FROM sys.{catalog} c JOIN sys.objects o ON \
                 o.object_id=c.object_id WHERE o.is_ms_shipped=0;"
            ),
        )
        .await?;
    }
    rows(
        conn,
        "foreign_key_columns",
        "SELECT c.constraint_object_id FROM sys.foreign_key_columns c JOIN sys.objects o ON \
         o.object_id=c.constraint_object_id WHERE o.is_ms_shipped=0;",
    )
    .await?;
    for row in rows(
        conn,
        "sql_expression_dependencies",
        "SELECT referencing_id,referenced_id FROM sys.sql_expression_dependencies WHERE \
         is_schema_bound_reference=1 AND referencing_id<>referenced_id;",
    )
    .await?
    {
        if let (Some(a), Some(b)) = (
            row.try_get::<i32>("referencing_id")?,
            row.try_get::<i32>("referenced_id")?,
        ) {
            result
                .dependencies
                .insert((Securable::Object(a), Securable::Object(b)));
        }
    }
    for (catalog, kind, filter, rank, qualified) in [
        ("types", "TYPE", "is_user_defined=1", 65, true),
        ("assemblies", "ASSEMBLY", "is_user_defined=1", 70, false),
        ("asymmetric_keys", "ASYMMETRIC KEY", "1=1", 80, false),
        ("certificates", "CERTIFICATE", "1=1", 80, false),
        ("symmetric_keys", "SYMMETRIC KEY", "1=1", 70, false),
        (
            "service_contracts",
            "CONTRACT",
            "service_contract_id>6",
            45,
            false,
        ),
        (
            "service_message_types",
            "MESSAGE TYPE",
            "message_type_id>14",
            55,
            false,
        ),
        ("services", "SERVICE", "service_id>3", 25, false),
        (
            "conversation_priorities",
            "BROKER PRIORITY",
            "1=1",
            20,
            false,
        ),
        (
            "external_data_sources",
            "EXTERNAL DATA SOURCE",
            "1=1",
            65,
            false,
        ),
        (
            "external_file_formats",
            "EXTERNAL FILE FORMAT",
            "1=1",
            65,
            false,
        ),
        ("external_libraries", "EXTERNAL LIBRARY", "1=1", 65, false),
        ("routes", "ROUTE", "name<>N'AutoCreatedLocal'", 45, false),
        (
            "remote_service_bindings",
            "REMOTE SERVICE BINDING",
            "1=1",
            25,
            false,
        ),
        (
            "database_scoped_credentials",
            "DATABASE SCOPED CREDENTIAL",
            "1=1",
            70,
            false,
        ),
        (
            "external_languages",
            "EXTERNAL LANGUAGE",
            "external_language_id NOT IN(1,2)",
            70,
            false,
        ),
        ("external_models", "EXTERNAL MODEL", "1=1", 70, false),
        ("fulltext_catalogs", "FULLTEXT CATALOG", "1=1", 70, false),
        ("fulltext_stoplists", "FULLTEXT STOPLIST", "1=1", 70, false),
        (
            "registered_search_property_lists",
            "SEARCH PROPERTY LIST",
            "1=1",
            70,
            false,
        ),
        (
            "xml_schema_collections",
            "XML SCHEMA COLLECTION",
            "schema_id<>4",
            70,
            true,
        ),
        ("partition_schemes", "PARTITION SCHEME", "1=1", 75, false),
        (
            "partition_functions",
            "PARTITION FUNCTION",
            "1=1",
            80,
            false,
        ),
        ("plan_guides", "PLAN GUIDE", "1=1", 30, false),
        (
            "event_notifications",
            "EVENT NOTIFICATION",
            "1=1",
            25,
            false,
        ),
        (
            "database_audit_specifications",
            "DATABASE AUDIT SPECIFICATION",
            "1=1",
            30,
            false,
        ),
        (
            "column_encryption_keys",
            "COLUMN ENCRYPTION KEY",
            "1=1",
            70,
            false,
        ),
        ("column_master_keys", "COLUMN MASTER KEY", "1=1", 75, false),
    ] {
        let projection = if qualified {
            "name,SCHEMA_NAME(schema_id) AS schema_name"
        } else if catalog == "external_languages" {
            "language AS name"
        } else {
            "name"
        };
        let identity: Option<(&str, Identity)> = match catalog {
            "types" => Some(("user_type_id", Securable::Type)),
            "assemblies" => Some(("assembly_id", Securable::Assembly)),
            "certificates" => Some(("certificate_id", Securable::Certificate)),
            "asymmetric_keys" => Some(("asymmetric_key_id", Securable::AsymmetricKey)),
            "symmetric_keys" => Some(("symmetric_key_id", Securable::SymmetricKey)),
            "database_scoped_credentials" => Some(("credential_id", Securable::Credential)),
            "external_models" => Some(("external_model_id", Securable::ExternalModel)),
            _ => None,
        };
        let crypto = matches!(
            catalog,
            "certificates" | "asymmetric_keys" | "symmetric_keys"
        );
        let mut projection = projection.to_owned();
        if let Some((column, _)) = identity {
            projection.push_str(&format!(",{column} AS inventory_id"));
        }
        if crypto {
            projection.push_str(",principal_id");
        }
        for row in rows(
            conn,
            catalog,
            &format!("SELECT {projection} FROM sys.{catalog} WHERE {filter};"),
        )
        .await?
        {
            let name = text(&row, "name")?;
            let name = if qualified {
                format!("{}.{}", ident(&text(&row, "schema_name")?), ident(&name))
            } else {
                ident(&name)
            };
            let drop = match kind {
                "PLAN GUIDE" => Some(format!(
                    "EXEC sys.sp_control_plan_guide N'DROP',{};",
                    literal(&text(&row, "name")?)
                )),
                "EVENT NOTIFICATION" => {
                    Some(format!("DROP EVENT NOTIFICATION {name} ON DATABASE;"))
                }
                "DATABASE AUDIT SPECIFICATION" => Some(format!(
                    "ALTER DATABASE AUDIT SPECIFICATION {name} WITH (STATE=OFF); DROP \
                     DATABASE AUDIT SPECIFICATION {name};"
                )),
                "SYMMETRIC KEY" if text(&row, "name")? == "##MS_DatabaseMasterKey##" => {
                    Some("DROP MASTER KEY;".into())
                }
                _ => Some(format!("DROP {kind} {name};")),
            };
            let rank =
                if kind == "SYMMETRIC KEY" && text(&row, "name")? == "##MS_DatabaseMasterKey##" {
                    85
                } else {
                    rank
                };
            let key = identity
                .map(|(_, make)| {
                    row.try_get::<i32>("inventory_id")?
                        .map(make)
                        .ok_or_else(|| DbError::BadRow(format!("sys.{catalog} has no identity")))
                })
                .transpose()?;
            if crypto && let (Some(key), Some(owner)) = (key, row.try_get::<i32>("principal_id")?) {
                // A mapped user can hold this key past another user's rank.
                result
                    .dependencies
                    .insert((key, Securable::Principal(owner)));
            }
            result.add(format!("{kind} {name}"), drop, rank, key);
            if catalog == "types" {
                result.entries.last_mut().unwrap().staged_root =
                    Some(super::super::capture::ObjectIdentity {
                        class: super::super::capture::class::TYPE.into(),
                        name: vec![text(&row, "schema_name")?, text(&row, "name")?],
                        signature: vec![],
                    });
            }
        }
    }
    rows(
        conn,
        "column_encryption_key_values",
        "SELECT column_encryption_key_id FROM sys.column_encryption_key_values;",
    )
    .await?;
    rows(
        conn,
        "database_audit_specification_details",
        "SELECT database_specification_id FROM sys.database_audit_specification_details;",
    )
    .await?;
    for row in rows(
        conn,
        "triggers",
        "SELECT name FROM sys.triggers WHERE parent_class=0 AND is_ms_shipped=0;",
    )
    .await?
    {
        let name = text(&row, "name")?;
        result.add(
            format!("database trigger {name}"),
            Some(format!("DROP TRIGGER {} ON DATABASE;", ident(&name))),
            15,
            None,
        );
    }
    for row in rows(
        conn,
        "database_scoped_configurations",
        "SELECT name FROM sys.database_scoped_configurations WHERE is_value_default=0;",
    )
    .await?
    {
        result.add(
            format!("database scoped configuration {}", text(&row, "name")?),
            None,
            80,
            None,
        );
    }
    for row in rows(
        conn,
        "filegroups",
        "SELECT name FROM sys.filegroups WHERE data_space_id<>1 OR name<>N'PRIMARY' OR \
         is_default<>1 OR is_read_only<>0;",
    )
    .await?
    {
        let name = text(&row, "name")?;
        if name == "PRIMARY" {
            result.unrestorable(format!("FILEGROUP {name}"), 85);
        } else {
            result.add(
                format!("FILEGROUP {name}"),
                Some(format!(
                    "ALTER DATABASE CURRENT REMOVE FILEGROUP {};",
                    ident(&name)
                )),
                85,
                None,
            );
        }
    }
    for row in rows(
        conn,
        "database_files",
        "SELECT name,file_id FROM sys.database_files WHERE file_id NOT IN(1,2) OR state<>0 OR \
         is_read_only<>0;",
    )
    .await?
    {
        let name = text(&row, "name")?;
        if matches!(row.try_get::<i32>("file_id")?, Some(1 | 2)) {
            result.unrestorable(format!("FILE {name}"), 82);
            continue;
        }
        result.add(
            format!("FILE {name}"),
            Some(format!(
                "ALTER DATABASE CURRENT REMOVE FILE {};",
                ident(&name)
            )),
            82,
            None,
        );
    }
    builtins(conn, &mut result).await?;
    references(conn, &mut result).await?;
    nonsecurables(conn, &mut result).await?;
    properties(conn, &mut result).await?;
    permissions(
        conn,
        &mut result,
        sandbox.filter(|name| principals.contains(*name)),
    )
    .await?;
    Ok(result)
}

// Engine-owned definitions that reject ALTER/DROP need existence checks, not
// a snapshot of their version-dependent IDs, timestamps or CLR identity.
// Mutable in-set columns below instead have fixed definitions (DEC-2263.1).
async fn builtins(conn: &mut impl QueryConnection, result: &mut Inventory) -> Result<(), DbError> {
    let fixed: &[(&str, &str, &str, &[&str])] = &[
        (
            "service_message_types",
            "MESSAGE TYPE",
            "message_type_id<=14",
            &[
                "http://schemas.microsoft.com/SQL/ServiceBroker/Error",
                "http://schemas.microsoft.com/SQL/ServiceBroker/EndDialog",
                "http://schemas.microsoft.com/SQL/Notifications/QueryNotification",
                "http://schemas.microsoft.com/SQL/Notifications/EventNotification",
                "http://schemas.microsoft.com/SQL/ServiceBroker/DialogTimer",
                "http://schemas.microsoft.com/SQL/ServiceBroker/BrokerConfigurationNotice/MissingRoute",
                "http://schemas.microsoft.com/SQL/ServiceBroker/BrokerConfigurationNotice/FailedRoute",
                "http://schemas.microsoft.com/SQL/ServiceBroker/BrokerConfigurationNotice/MissingRemoteServiceBinding",
                "http://schemas.microsoft.com/SQL/ServiceBroker/BrokerConfigurationNotice/FailedRemoteServiceBinding",
                "http://schemas.microsoft.com/SQL/ServiceBroker/ServiceEcho/Echo",
                "http://schemas.microsoft.com/SQL/ServiceBroker/ServiceDiagnostic/Query",
                "http://schemas.microsoft.com/SQL/ServiceBroker/ServiceDiagnostic/Status",
                "http://schemas.microsoft.com/SQL/ServiceBroker/ServiceDiagnostic/Description",
                "DEFAULT",
            ],
        ),
        (
            "service_contracts",
            "CONTRACT",
            "service_contract_id<=6",
            &[
                "DEFAULT",
                "http://schemas.microsoft.com/SQL/Notifications/PostEventNotification",
                "http://schemas.microsoft.com/SQL/Notifications/PostQueryNotification",
                "http://schemas.microsoft.com/SQL/ServiceBroker/BrokerConfigurationNotice",
                "http://schemas.microsoft.com/SQL/ServiceBroker/ServiceDiagnostic",
                "http://schemas.microsoft.com/SQL/ServiceBroker/ServiceEcho",
            ],
        ),
        (
            "services",
            "SERVICE",
            "service_id<=3",
            &[
                "http://schemas.microsoft.com/SQL/Notifications/EventNotificationService",
                "http://schemas.microsoft.com/SQL/Notifications/QueryNotificationService",
                "http://schemas.microsoft.com/SQL/ServiceBroker/ServiceBroker",
            ],
        ),
        (
            "service_queues",
            "QUEUE",
            "is_ms_shipped=1",
            &[
                "QueryNotificationErrorsQueue",
                "EventNotificationErrorsQueue",
                "ServiceBrokerQueue",
            ],
        ),
        (
            "assemblies",
            "ASSEMBLY",
            "is_user_defined=0",
            &["Microsoft.SqlServer.Types"],
        ),
        (
            "xml_schema_collections",
            "XML SCHEMA COLLECTION",
            "schema_id=4",
            &["sys"],
        ),
        ("filegroups", "FILEGROUP", "data_space_id=1", &["PRIMARY"]),
    ];
    for (catalog, kind, filter, expected) in fixed {
        let found = rows(
            conn,
            catalog,
            &format!("SELECT name FROM sys.{catalog} WHERE {filter};"),
        )
        .await?
        .iter()
        .map(|row| text(row, "name"))
        .collect::<Result<BTreeSet<_>, _>>()?;
        for name in *expected {
            if !found.contains(*name) {
                result.unrestorable(format!("missing {kind} {name}"), 80);
            }
        }
        for name in &found {
            if !expected.contains(&name.as_str()) {
                result.unrestorable(format!("unknown built-in {kind} {name}"), 80);
            }
        }
    }
    for id in [1, 2] {
        if rows(
            conn,
            "database_files",
            &format!("SELECT name FROM sys.database_files WHERE file_id={id};"),
        )
        .await?
        .len()
            != 1
        {
            result.unrestorable(format!("missing FILE file_id={id}"), 82);
        }
    }
    for (catalog, kind, name_column, filter, expected) in [
        (
            "assemblies",
            "ASSEMBLY",
            "name",
            "is_user_defined=0",
            &["Microsoft.SqlServer.Types"][..],
        ),
        (
            "external_languages",
            "EXTERNAL LANGUAGE",
            "language",
            "external_language_id IN(1,2)",
            &["R", "Python"][..],
        ),
    ] {
        let found = rows(conn, catalog, &format!(
            "SELECT {name_column} AS name,USER_NAME(principal_id) AS owner FROM sys.{catalog} WHERE {filter};"
        )).await?;
        let names = found
            .iter()
            .map(|row| text(row, "name"))
            .collect::<Result<BTreeSet<_>, _>>()?;
        for name in expected {
            if !names.contains(*name) {
                result.unrestorable(format!("missing {kind} {name}"), 70);
            }
        }
        for row in found {
            let name = text(&row, "name")?;
            let owner = text(&row, "owner")?;
            if !expected.contains(&name.as_str()) {
                result.unrestorable(
                    format!("unknown built-in {kind} {} owner {owner}", ident(&name)),
                    70,
                );
            } else if owner != "sys" {
                // Measured: even db_owner cannot assign ownership back to sys
                // (15151). Dropping these system objects is forbidden too.
                result.unrestorable(
                    format!("{kind} {} owner {owner} (expected sys)", ident(&name)),
                    70,
                );
            }
        }
    }
    let route = rows(
        conn,
        "routes",
        "SELECT address,USER_NAME(principal_id) AS owner,CONVERT(bit,CASE WHEN \
         remote_service_name IS NULL AND broker_instance IS NULL AND lifetime IS NULL \
         AND mirror_address IS NULL THEN 1 ELSE 0 END) AS fixed_destination FROM sys.routes \
         WHERE name=N'AutoCreatedLocal';",
    )
    .await?;
    if route.len() != 1 {
        result.unrestorable("missing ROUTE AutoCreatedLocal".into(), 45);
    } else {
        let row = &route[0];
        let owner = text(row, "owner")?;
        if row.try_get::<bool>("fixed_destination")? != Some(true) {
            // ALTER ROUTE cannot remove a service, broker, lifetime or mirror
            // clause to recover the fixed NULL definition. Never recreate it.
            result.unrestorable(
                format!("ROUTE AutoCreatedLocal definition owner {owner}"),
                45,
            );
        } else {
            if text(row, "address")? != "LOCAL" {
                result.add(
                    "ROUTE AutoCreatedLocal address".into(),
                    Some("ALTER ROUTE AutoCreatedLocal WITH ADDRESS=N'LOCAL';".into()),
                    45,
                    None,
                );
            }
            if owner != "dbo" {
                result.add(
                    format!("ROUTE AutoCreatedLocal owner {owner}"),
                    Some("ALTER AUTHORIZATION ON ROUTE::AutoCreatedLocal TO dbo;".into()),
                    45,
                    None,
                );
            }
        }
    }
    Ok(())
}

// A reference can hold a low-rank parent until after a high-rank child.
// IDs keep that order independent of catalog iteration and object names.
async fn references(
    conn: &mut impl QueryConnection,
    result: &mut Inventory,
) -> Result<(), DbError> {
    use Securable::*;
    let relations: &[(&str, &str, Identity, Identity)] = &[
        (
            "certificates",
            "SELECT p.principal_id AS dependent_id,c.certificate_id AS provider_id \
          FROM sys.database_principals p JOIN sys.certificates c ON p.sid=c.sid WHERE p.type='C';",
            Principal,
            Certificate,
        ),
        (
            "asymmetric_keys",
            "SELECT p.principal_id AS dependent_id,a.asymmetric_key_id AS provider_id \
          FROM sys.database_principals p JOIN sys.asymmetric_keys a ON p.sid=a.sid WHERE p.type='K';",
            Principal,
            AsymmetricKey,
        ),
        (
            "key_encryptions",
            "SELECT e.key_id AS dependent_id,c.certificate_id AS provider_id FROM sys.key_encryptions e \
          JOIN sys.certificates c ON e.thumbprint=c.thumbprint WHERE e.crypt_type IN('EPUC','C256');",
            SymmetricKey,
            Certificate,
        ),
        (
            "key_encryptions",
            "SELECT e.key_id AS dependent_id,a.asymmetric_key_id AS provider_id FROM sys.key_encryptions e \
          JOIN sys.asymmetric_keys a ON e.thumbprint=a.thumbprint WHERE e.crypt_type IN('EPUA','A256');",
            SymmetricKey,
            AsymmetricKey,
        ),
        (
            "key_encryptions",
            "SELECT e.key_id AS dependent_id,s.symmetric_key_id AS provider_id FROM sys.key_encryptions e \
          JOIN sys.symmetric_keys s ON e.thumbprint=CONVERT(varbinary(32),s.key_guid) \
          WHERE e.crypt_type='ESKS';",
            SymmetricKey,
            SymmetricKey,
        ),
        (
            "certificates",
            "SELECT c.certificate_id AS dependent_id,m.symmetric_key_id AS provider_id \
          FROM sys.certificates c CROSS JOIN sys.symmetric_keys m \
          WHERE c.pvt_key_encryption_type='MK' AND m.name=N'##MS_DatabaseMasterKey##';",
            Certificate,
            SymmetricKey,
        ),
        (
            "asymmetric_keys",
            "SELECT a.asymmetric_key_id AS dependent_id,m.symmetric_key_id AS provider_id \
          FROM sys.asymmetric_keys a CROSS JOIN sys.symmetric_keys m \
          WHERE a.pvt_key_encryption_type='MK' AND m.name=N'##MS_DatabaseMasterKey##';",
            AsymmetricKey,
            SymmetricKey,
        ),
        (
            "database_scoped_credentials",
            "SELECT c.credential_id AS dependent_id,m.symmetric_key_id AS provider_id \
          FROM sys.database_scoped_credentials c CROSS JOIN sys.symmetric_keys m \
          WHERE m.name=N'##MS_DatabaseMasterKey##';",
            Credential,
            SymmetricKey,
        ),
        (
            "crypt_properties",
            "SELECT p.major_id AS dependent_id,c.certificate_id AS provider_id FROM sys.crypt_properties p \
          JOIN sys.certificates c ON p.thumbprint=c.thumbprint \
          WHERE p.class=1 AND p.crypt_type IN('SPVC','CPVC');",
            Object,
            Certificate,
        ),
        (
            "crypt_properties",
            "SELECT p.major_id AS dependent_id,a.asymmetric_key_id AS provider_id \
          FROM sys.crypt_properties p JOIN sys.asymmetric_keys a ON p.thumbprint=a.thumbprint \
          WHERE p.class=1 AND p.crypt_type IN('SPVA','CPVA');",
            Object,
            AsymmetricKey,
        ),
        (
            "column_type_usages",
            "SELECT t.user_type_id AS dependent_id,c.user_type_id AS provider_id \
          FROM sys.column_type_usages c JOIN sys.table_types t ON t.type_table_object_id=c.object_id \
          WHERE t.user_type_id<>c.user_type_id;",
            Type,
            Type,
        ),
        (
            "types",
            "SELECT user_type_id AS dependent_id,default_object_id AS provider_id FROM sys.types \
          WHERE is_user_defined=1 AND default_object_id<>0;",
            Type,
            Object,
        ),
        (
            "types",
            "SELECT user_type_id AS dependent_id,rule_object_id AS provider_id FROM sys.types \
          WHERE is_user_defined=1 AND rule_object_id<>0;",
            Type,
            Object,
        ),
        (
            "columns",
            "SELECT c.object_id AS dependent_id,d.object_id AS provider_id FROM sys.columns c \
          JOIN sys.objects d ON d.object_id=c.default_object_id \
          WHERE d.type='D' AND d.parent_object_id=0;",
            Object,
            Object,
        ),
        (
            "columns",
            "SELECT c.object_id AS dependent_id,r.object_id AS provider_id FROM sys.columns c \
          JOIN sys.objects r ON r.object_id=c.rule_object_id WHERE r.type='R';",
            Object,
            Object,
        ),
        (
            "external_models",
            "SELECT external_model_id AS dependent_id,credential_id AS provider_id FROM sys.external_models \
          WHERE credential_id IS NOT NULL;",
            ExternalModel,
            Credential,
        ),
        (
            "assembly_references",
            "SELECT assembly_id AS dependent_id,referenced_assembly_id AS provider_id \
          FROM sys.assembly_references;",
            Assembly,
            Assembly,
        ),
    ];
    for (catalog, sql, dependent, provider) in relations {
        for row in rows(conn, catalog, sql).await? {
            let id = |name| {
                row.try_get::<i32>(name)?
                    .ok_or_else(|| DbError::BadRow(format!("sys.{catalog} has no {name}")))
            };
            result
                .dependencies
                .insert((dependent(id("dependent_id")?), provider(id("provider_id")?)));
        }
    }
    Ok(())
}

// These persistent database settings and broker/Query Store records have no
// securable class or sys.objects row (DEC-2236.1).
async fn nonsecurables(
    conn: &mut impl QueryConnection,
    result: &mut Inventory,
) -> Result<(), DbError> {
    for row in rows(
        conn,
        "conversation_endpoints",
        "SELECT CONVERT(nvarchar(36),conversation_handle) AS name FROM \
         sys.conversation_endpoints WHERE is_system=0;",
    )
    .await?
    {
        let name = text(&row, "name")?;
        result.add(
            format!("conversation endpoint {name}"),
            Some(format!(
                "DECLARE @handle uniqueidentifier={}; END CONVERSATION @handle WITH CLEANUP;",
                literal(&name)
            )),
            16,
            None,
        );
    }
    // An orphan has no safe END CONVERSATION target. Do not let a parent join
    // hide it, or count an unreadable child catalog as empty.
    for (catalog, projection, join) in [
        (
            "conversation_groups",
            "CONVERT(nvarchar(36),c.conversation_group_id)",
            "e.conversation_group_id=c.conversation_group_id",
        ),
        (
            "transmission_queue",
            "CONVERT(nvarchar(36),c.conversation_handle)",
            "e.conversation_handle=c.conversation_handle",
        ),
    ] {
        for row in rows(
            conn,
            catalog,
            &format!(
                "SELECT {projection} AS name FROM sys.{catalog} c WHERE NOT EXISTS(SELECT 1 \
             FROM sys.conversation_endpoints e WHERE {join});"
            ),
        )
        .await?
        {
            result.add(format!("{catalog} {}", text(&row, "name")?), None, 17, None);
        }
    }
    for row in rows(
        conn,
        "change_tracking_tables",
        "SELECT SCHEMA_NAME(o.schema_id) AS schema_name,o.name FROM sys.change_tracking_tables c \
         JOIN sys.objects o ON o.object_id=c.object_id;",
    )
    .await?
    {
        let name = format!(
            "{}.{}",
            ident(&text(&row, "schema_name")?),
            ident(&text(&row, "name")?)
        );
        // Its primary key cannot be removed while tracking is enabled (3735).
        result.add(
            format!("table change tracking {name}"),
            Some(format!("ALTER TABLE {name} DISABLE CHANGE_TRACKING;")),
            17,
            None,
        );
    }
    for (catalog, sql, name, cleanup) in [
        (
            "change_tracking_databases",
            "SELECT database_id FROM sys.change_tracking_databases WHERE database_id=DB_ID();",
            "database change tracking",
            "ALTER DATABASE CURRENT SET CHANGE_TRACKING=OFF;",
        ),
        (
            "database_filestream_options",
            "SELECT database_id FROM sys.database_filestream_options WHERE database_id=DB_ID() \
          AND (non_transacted_access<>0 OR directory_name IS NOT NULL);",
            "database FILESTREAM options",
            "ALTER DATABASE CURRENT SET FILESTREAM(NON_TRANSACTED_ACCESS=OFF,DIRECTORY_NAME=NULL);",
        ),
        (
            "database_automatic_tuning_mode",
            "SELECT desired_state FROM sys.database_automatic_tuning_mode \
             WHERE desired_state_desc<>N'AUTO';",
            "database automatic tuning mode",
            "ALTER DATABASE CURRENT SET AUTOMATIC_TUNING=AUTO;",
        ),
    ] {
        if !rows(conn, catalog, sql).await?.is_empty() {
            result.add(
                name.into(),
                Some(cleanup.into()),
                if catalog == "database_automatic_tuning_mode" {
                    78
                } else {
                    80
                },
                None,
            );
        }
    }
    for row in rows(
        conn,
        "database_automatic_tuning_options",
        "SELECT name FROM sys.database_automatic_tuning_options \
         WHERE desired_state_desc<>N'DEFAULT';",
    )
    .await?
    {
        let name = text(&row, "name")?;
        result.add(
            format!("automatic tuning option {name}"),
            (name == "FORCE_LAST_GOOD_PLAN").then(|| {
                "ALTER DATABASE CURRENT SET AUTOMATIC_TUNING(FORCE_LAST_GOOD_PLAN=DEFAULT);".into()
            }),
            79,
            None,
        );
    }
    for row in rows(
        conn,
        "database_ledger_digest_locations",
        "SELECT path FROM sys.database_ledger_digest_locations;",
    )
    .await?
    {
        // Historical endpoints survive disabling uploads; no supported catalog
        // deletion can erase this ledger history.
        result.add(
            format!("ledger digest location {}", text(&row, "path")?),
            None,
            80,
            None,
        );
    }
    let mut queries: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for (catalog, projection, filter) in [
        ("query_store_query_hints", "query_id", "source=0"),
        ("query_store_plan", "query_id,plan_id", "is_forced_plan=1"),
        (
            "database_automatic_tuning_configurations",
            "CONVERT(bigint,type_value) AS query_id",
            "1=1",
        ),
    ] {
        for row in rows(
            conn,
            catalog,
            &format!("SELECT DISTINCT {projection} FROM sys.{catalog} WHERE {filter};"),
        )
        .await?
        {
            let id = row
                .try_get::<i64>("query_id")?
                .ok_or_else(|| DbError::BadRow(format!("sys.{catalog} has no query_id")))?;
            let cleanup = queries.entry(id).or_default();
            if catalog == "query_store_query_hints" {
                // SQL Server refuses removal of a hinted query (12456).
                cleanup.push(format!(
                    "EXEC sys.sp_query_store_clear_hints @query_id={id};"
                ));
            } else if catalog == "query_store_plan" {
                let plan = row
                    .try_get::<i64>("plan_id")?
                    .ok_or_else(|| DbError::BadRow("a forced plan has no plan_id".into()))?;
                cleanup.push(format!(
                    "EXEC sys.sp_query_store_unforce_plan @query_id={id},@plan_id={plan};"
                ));
            }
        }
    }
    for (id, mut cleanup) in queries {
        cleanup.push(format!(
            "EXEC sys.sp_query_store_remove_query @query_id={id};"
        ));
        result.add(
            format!("Query Store intent query {id}"),
            Some(cleanup.join(" ")),
            30,
            None,
        );
    }
    Ok(())
}

/// The two database properties a run writes to be found again if it is killed
/// (`recovery`, #2410) are the run's own record, not something foreign in the
/// database, and are left out.
async fn properties(
    conn: &mut impl QueryConnection,
    result: &mut Inventory,
) -> Result<(), DbError> {
    for row in rows(
        conn,
        "extended_properties",
        "SELECT e.class,e.name,e.minor_id,SCHEMA_NAME(o.schema_id) AS schema_name,o.name AS \
         object_name, COL_NAME(e.major_id,e.minor_id) AS column_name,SCHEMA_NAME(e.major_id) \
         AS property_schema FROM sys.extended_properties e LEFT JOIN sys.objects o ON \
         e.class=1 AND o.object_id=e.major_id WHERE NOT (e.class=0 AND (e.name = N'pbps_run' \
         OR e.name LIKE N'pbps[_]companion[_]%'));",
    )
    .await?
    {
        let name = text(&row, "name")?;
        let class = row
            .try_get::<u8>("class")?
            .ok_or_else(|| DbError::BadRow("an extended property has no class".into()))?;
        let suffix = match class {
            0 => Some(String::new()),
            1 => {
                let schema = text(&row, "schema_name")?;
                let object = text(&row, "object_name")?;
                // sp_dropextendedproperty requires the object's actual kind;
                // SQL Server accepts TABLE for its table properties only.
                let kind = rows(
                    conn,
                    "objects",
                    &format!(
                        "SELECT type FROM sys.objects WHERE object_id=OBJECT_ID({});",
                        literal(&format!("{}.{}", ident(&schema), ident(&object)))
                    ),
                )
                .await?;
                let kind = kind
                    .first()
                    .map(|row| text(row, "type"))
                    .transpose()?
                    .unwrap_or_default();
                let level = match kind.trim() {
                    "U" => Some("TABLE"),
                    "V" => Some("VIEW"),
                    "P" => Some("PROCEDURE"),
                    "FN" | "IF" | "TF" => Some("FUNCTION"),
                    "SO" => Some("SEQUENCE"),
                    _ => None,
                };
                let column = row
                    .try_get::<&str>("column_name")?
                    .map(|column| format!(",@level2type=N'COLUMN',@level2name={}", literal(column)))
                    .unwrap_or_default();
                level.map(|kind| {
                    format!(
                        ",@level0type=N'SCHEMA',@level0name={},\
                         @level1type=N'{kind}',@level1name={}{}",
                        literal(&schema),
                        literal(&object),
                        column
                    )
                })
            }
            3 => Some(format!(
                ",@level0type=N'SCHEMA',@level0name={}",
                literal(&text(&row, "property_schema")?)
            )),
            _ => None,
        };
        result.add(
            format!("extended property class {class} {name}"),
            suffix.map(|suffix| {
                format!(
                    "EXEC sys.sp_dropextendedproperty @name={}{suffix};",
                    literal(&name)
                )
            }),
            0,
            None,
        );
    }
    Ok(())
}

async fn permissions(
    conn: &mut impl QueryConnection,
    result: &mut Inventory,
    sandbox: Option<&str>,
) -> Result<(), DbError> {
    let mut database = BTreeSet::new();
    let mut expected = database_grants();
    if let Some(sandbox) = sandbox {
        // CREATE USER supplies CONNECT automatically. This is run-owned
        // setup, like its db_owner membership, and vanishes with the user.
        expected.insert(format!("{sandbox}|dbo|CONNECT|GRANT"));
    }
    for row in rows(
        conn,
        "database_permissions",
        "SELECT p.class,p.class_desc,p.permission_name,p.state_desc,p.major_id,\
         USER_NAME(p.grantee_principal_id) AS grantee, USER_NAME(p.grantor_principal_id) AS \
         grantor,OBJECT_SCHEMA_NAME(p.major_id) AS schema_name, OBJECT_NAME(p.major_id) AS \
         object_name,COL_NAME(p.major_id,p.minor_id) AS column_name, SCHEMA_NAME(p.major_id) \
         AS on_schema,USER_NAME(p.major_id) AS on_principal, CONVERT(int,\
         OBJECTPROPERTYEX(p.major_id,N'IsMSShipped')) AS shipped, CONVERT(int,\
         OBJECTPROPERTYEX(p.major_id,N'IsView')) AS is_view,\
         SCHEMA_NAME(t.schema_id) AS type_schema,t.name AS type_name \
         FROM sys.database_permissions p LEFT JOIN sys.types t \
         ON p.class=6 AND t.user_type_id=p.major_id;",
    )
    .await?
    {
        let permission = text(&row, "permission_name")?;
        let grantee = text(&row, "grantee")?;
        let grantor = text(&row, "grantor")?;
        let state = text(&row, "state_desc")?;
        let class = row
            .try_get::<u8>("class")?
            .ok_or_else(|| DbError::BadRow("a permission has no class".into()))?;
        if class == 1
            && permission == "SELECT"
            && row.try_get::<i32>("shipped")? == Some(1)
            && row.try_get::<i32>("is_view")? == Some(1)
        {
            // Measured: these SELECTs do not participate in creation, cannot
            // be schema-bound, and the sandbox can change them itself.
            continue;
        }
        let key = format!("{grantee}|{grantor}|{permission}|{state}");
        if class == 0 {
            database.insert(key.clone());
            if expected.contains(&key) {
                continue;
            }
        }
        if class == 6 {
            let type_id = row
                .try_get::<i32>("major_id")?
                .ok_or_else(|| DbError::BadRow("a type permission has no type id".into()))?;
            let parent = super::super::capture::ObjectIdentity {
                class: super::super::capture::class::TYPE.into(),
                name: vec![text(&row, "type_schema")?, text(&row, "type_name")?],
                signature: vec![],
            };
            result.type_permission(
                type_id,
                parent,
                format!("permission {} {key}", text(&row, "class_desc")?),
            );
            continue;
        }
        let on = match class {
            0 => Some(String::new()),
            1 => {
                let schema = text(&row, "schema_name")?;
                let object = text(&row, "object_name")?;
                Some(format!(
                    " ON OBJECT::{}.{}{}",
                    ident(&schema),
                    ident(&object),
                    row.try_get::<&str>("column_name")?
                        .map(|column| format!(" ({})", ident(column)))
                        .unwrap_or_default()
                ))
            }
            3 => Some(format!(" ON SCHEMA::{}", ident(&text(&row, "on_schema")?))),
            // Other permissions vanish with their securable or principal;
            // retaining an unknown one refuses rather than guessing its SQL.
            _ => None,
        };
        result.add(
            format!("permission {} {key}", text(&row, "class_desc")?),
            on.map(|on| {
                format!(
                    "REVOKE {permission}{on} FROM {} CASCADE AS {};",
                    ident(&grantee),
                    ident(&grantor)
                )
            }),
            5,
            None,
        );
        if class == 1 {
            // Object and column ACL rows belong to their created root. The
            // adapter subsequently replays and verifies them as the deployer;
            // they are not independent catalog objects to silently discard.
            result.entries.last_mut().unwrap().staged_parent = Some(super::super::capture::object(
                &text(&row, "schema_name")?,
                &text(&row, "object_name")?,
            ));
        } else if class == 3 {
            result.entries.last_mut().unwrap().staged_parent =
                Some(super::super::capture::ObjectIdentity {
                    class: super::super::capture::class::SCHEMA.into(),
                    name: vec![text(&row, "on_schema")?],
                    signature: vec![],
                });
        }
    }
    for missing in expected.difference(&database) {
        // A revoked fixed grant is missing built-in state, not a changed
        // column of a present built-in. Never recreate it (DEC-2263.1).
        result.unrestorable(format!("missing permission {missing}"), 5);
    }
    Ok(())
}

pub async fn cleanup(
    conn: &mut impl ExecuteConnection,
    sandbox: Option<&str>,
) -> Result<(), Vec<String>> {
    let guard = sandbox.map(|sandbox| {
        format!(
            "IF USER_NAME() IS NULL OR USER_NAME() COLLATE Latin1_General_BIN2 <> {} COLLATE \
             Latin1_General_BIN2 THROW 51000, 'scratch cleanup must execute as the sandbox', \
             1;",
            literal(sandbox)
        )
    });
    if let Some(guard) = &guard {
        conn.execute(guard)
            .await
            .map_err(|error| vec![error.to_string()])?;
    }
    let mut inventory = read(conn, sandbox)
        .await
        .map_err(|error| vec![error.to_string()])?;
    let mut failed = Vec::new();
    while !inventory.entries.is_empty() {
        let position = inventory
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.key.is_none_or(|key| {
                    !inventory
                        .dependencies
                        .iter()
                        .any(|(dependent, dependency)| {
                            *dependency == key
                                && inventory
                                    .entries
                                    .iter()
                                    .any(|other| other.key == Some(*dependent))
                        })
                })
            })
            .min_by_key(|(_, entry)| entry.rank)
            .map(|(position, _)| position);
        let Some(position) = position else {
            failed.extend(inventory.names());
            break;
        };
        let entry = inventory.entries.remove(position);
        match entry.drop {
            Some(sql) => {
                // A database DDL trigger is user code too. Every cleanup
                // statement keeps the irreversible sandbox context; only
                // dropping the sandbox itself may use the fresh account.
                let sql = guard
                    .as_ref()
                    .map(|guard| format!("{guard} EXEC({});", literal(&sql)))
                    .unwrap_or(sql);
                if let Err(error) = conn.execute(&sql).await {
                    failed.push(format!("{} ({error})", entry.name));
                }
            }
            None => failed.push(format!("{}: no cleanup rule", entry.name)),
        }
    }
    match read(conn, sandbox).await {
        Ok(after) => failed.extend(after.names()),
        Err(error) => failed.push(error.to_string()),
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed)
    }
}

/// Called on a newly bound supplied-account session only after sandboxed
/// cleanup has removed every other object, including database DDL triggers.
pub async fn drop_sandbox(
    conn: &mut impl ExecuteConnection,
    sandbox: &str,
) -> Result<(), Vec<String>> {
    let before = read(conn, Some(sandbox))
        .await
        .map_err(|error| vec![error.to_string()])?;
    if !before.is_empty() {
        return Err(before.names());
    }
    conn.execute(&format!(
        "IF USER_ID({0}) IS NOT NULL BEGIN IF IS_ROLEMEMBER(N'db_owner',{0})=1 ALTER ROLE \
         db_owner DROP MEMBER {1}; DROP USER {1}; END;",
        literal(sandbox),
        ident(sandbox)
    ))
    .await
    .map_err(|error| vec![format!("sandbox {sandbox} ({error})")])?;
    let after = read(conn, None)
        .await
        .map_err(|error| vec![error.to_string()])?;
    if after.is_empty() {
        Ok(())
    } else {
        Err(after.names())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_acl_cleanup_requires_its_own_recorded_type_drop() {
        let mut inventory = Inventory::default();
        inventory.add(
            "object".into(),
            Some("DROP TABLE t;".into()),
            40,
            Some(Securable::Object(9)),
        );
        inventory.add(
            "undroppable type".into(),
            None,
            65,
            Some(Securable::Type(10)),
        );
        inventory.add(
            "droppable type".into(),
            Some("DROP TYPE t;".into()),
            65,
            Some(Securable::Type(11)),
        );
        for id in [9, 10, 11, 12] {
            inventory.type_permission(
                id,
                super::super::super::capture::ObjectIdentity {
                    class: super::super::super::capture::class::TYPE.into(),
                    name: vec!["dbo".into(), format!("t{id}")],
                    signature: vec![],
                },
                format!("type permission {id}"),
            );
        }
        let permissions: Vec<_> = inventory
            .entries
            .iter()
            .filter(|entry| entry.name.starts_with("type permission "))
            .collect();
        assert_eq!(
            permissions
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            [
                "type permission 9",
                "type permission 10",
                "type permission 12"
            ]
        );
        assert!(permissions.iter().all(|entry| entry.drop.is_none()));
        assert_eq!(
            inventory
                .type_permissions
                .get(&Securable::Type(11))
                .unwrap(),
            &["type permission 11"]
        );
        assert!(
            inventory
                .names()
                .iter()
                .any(|name| name == "type permission 11")
        );
        assert!(
            permissions
                .iter()
                .all(|entry| entry.staged_parent.as_ref().unwrap().class
                    == super::super::super::capture::class::TYPE)
        );
        inventory.entries.clear();
        assert!(
            !inventory.is_empty(),
            "carried ACL evidence is not an empty inventory"
        );
        assert_eq!(inventory.names(), ["type permission 11"]);
    }
}
