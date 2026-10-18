//! The catalog components, one statement each (DEC-619.1).
//!
//! Every value is converted to text in the query and parsed here, so a
//! column's driver type never decides what a fact reads as. A row that does
//! not parse is incomplete input, never a default.

use super::CaptureError;
use pbps_db::transport::QueryConnection;
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Schema {
    pub id: i64,
    pub name: String,
    pub owner: i64,
}

pub(super) struct Principal {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub default_schema: Option<String>,
    pub owner: Option<i64>,
}

pub(super) struct Object {
    pub id: i64,
    pub schema: i64,
    pub name: String,
    pub kind: String,
    pub parent: Option<i64>,
    /// `NULL` means the schema's owner owns it.
    pub owner: Option<i64>,
}

pub(super) struct Module {
    pub definition: Option<String>,
    pub ansi_nulls: bool,
    pub quoted_identifier: bool,
    pub schema_bound: bool,
    /// `NULL` is the caller, -2 the owner, any other the principal's id
    /// (`SELF` is stored as its creator's), measured on #2141.
    pub execute_as: Option<i64>,
}

pub(super) struct Column {
    pub object: i64,
    pub id: i64,
    pub name: String,
    pub user_type: i64,
    pub max_length: i64,
    pub precision: i64,
    pub scale: i64,
    pub collation: Option<String>,
    pub nullable: bool,
    pub identity: bool,
    /// An identity's seed, increment and `NOT FOR REPLICATION`, as text.
    pub identity_spec: Option<(String, String, bool)>,
    pub computed: Option<String>,
    pub persisted: bool,
}

pub(super) struct Parameter {
    pub object: i64,
    pub id: i64,
    pub name: String,
    pub user_type: i64,
    pub max_length: i64,
    pub precision: i64,
    pub scale: i64,
    pub output: bool,
    pub has_default: bool,
    pub readonly: bool,
}

/// A PRIMARY KEY or UNIQUE constraint's ordered key columns and its index's
/// kind: a foreign key binds the key's index (`docs/RESOLVER-MSSQL.md` §8).
#[derive(Default)]
pub(super) struct Key {
    pub index: String,
    /// `ALTER INDEX ... DISABLE` leaves the key's columns and stops it
    /// enforcing anything.
    pub disabled: bool,
    pub columns: Vec<(i64, String, bool)>,
}

/// A foreign key: the table and the key it binds, its column pairs and its
/// actions.
pub(super) struct ForeignKey {
    pub referenced: i64,
    /// The key constraint whose index the key binds, or `None` when it binds
    /// a unique index that is no constraint, named then by `index`.
    pub key: Option<i64>,
    pub index: String,
    pub on_delete: String,
    pub on_update: String,
    pub disabled: bool,
    pub trusted: bool,
    pub not_for_replication: bool,
    pub columns: Vec<(i64, String, String)>,
}

/// One column of a user-defined table type, keyed by the type.
pub(super) struct TypeColumn {
    pub id: i64,
    pub name: String,
    pub user_type: i64,
    pub max_length: i64,
    pub precision: i64,
    pub scale: i64,
    pub collation: Option<String>,
    pub nullable: bool,
    pub identity: bool,
    /// As a table column's: seed, increment and `NOT FOR REPLICATION`.
    pub identity_spec: Option<(String, String, bool)>,
    pub computed: Option<String>,
    pub default: Option<String>,
}

/// One key column of a table type's primary key or unique constraint.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub(super) struct TypeKey {
    pub index: i64,
    pub primary: bool,
    pub ordinal: i64,
    pub column: String,
    pub descending: bool,
}

/// What a user-defined table type is made of. Its own table object is the
/// engine's (`is_ms_shipped = 1`, measured on #2141), so the ordinary object
/// reads never list it.
#[derive(Default)]
pub(super) struct TableType {
    pub columns: Vec<TypeColumn>,
    pub checks: Vec<String>,
    pub keys: Vec<TypeKey>,
}

pub(super) struct Type {
    pub id: i64,
    pub schema: i64,
    pub name: String,
    pub base: i64,
    pub max_length: i64,
    pub precision: i64,
    pub scale: i64,
    pub collation: Option<String>,
    pub nullable: bool,
    pub user_defined: bool,
    pub table_type: bool,
}

/// A DEFAULT or CHECK constraint's text and the column it is on (0 for a
/// table-level CHECK).
pub(super) struct Constraint {
    pub column: i64,
    pub definition: String,
    /// A CHECK's state, `None` for a DEFAULT.
    pub disabled: Option<bool>,
    pub trusted: Option<bool>,
    /// A CHECK's `NOT FOR REPLICATION`: replication agents' writes skip it.
    pub not_for_replication: Option<bool>,
}

pub(super) struct Dependency {
    pub referencing: i64,
    pub referencing_column: i64,
    pub class: i64,
    pub server: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub entity: String,
    pub referenced: Option<i64>,
    pub referenced_column: i64,
    pub schema_bound: bool,
    pub caller_dependent: bool,
}

/// The connection's own session inputs that a statement-level name or a
/// module's fixed options depend on, observed before and after the reads.
#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub(super) struct Session {
    pub user: String,
    pub default_schema: String,
    pub ansi_nulls: String,
    pub quoted_identifier: String,
}

/// The pbps ledger's newest recorded state, read with the catalog.
#[derive(Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status")]
pub(super) enum Baseline {
    Absent,
    Empty,
    Recorded { entry: i64, state: String },
}

/// One row of a view read whole: each column as the engine reports it, as
/// text, `None` for NULL.
pub(super) type WholeRow = BTreeMap<String, Option<String>>;

pub(super) struct Catalog {
    pub major: u32,
    pub session: Session,
    pub baseline: Baseline,
    pub schemas: Vec<Schema>,
    pub principals: Vec<Principal>,
    pub objects: Vec<Object>,
    pub modules: BTreeMap<i64, Module>,
    pub columns: Vec<Column>,
    pub parameters: Vec<Parameter>,
    pub types: Vec<Type>,
    pub synonyms: BTreeMap<i64, String>,
    /// Each DML trigger's state, by its object id: whether it is disabled,
    /// which leaves its text and stops it firing, and whether it runs
    /// instead of its statement.
    pub triggers: BTreeMap<i64, (bool, bool, bool)>,
    /// Each key constraint's key, by the constraint's object id.
    pub keys: BTreeMap<i64, Key>,
    /// Each table's unique indexes that are no constraint, by the table's
    /// object id: a foreign key may bind one as it binds a key.
    pub unique_indexes: BTreeMap<i64, BTreeMap<String, Key>>,
    /// Each foreign key, by its object id.
    pub foreign_keys: BTreeMap<i64, ForeignKey>,
    /// Every row of each view in `columns::VIEWS` that describes something
    /// this database's own objects or types hold, read whole (#2153).
    pub whole: BTreeMap<&'static str, Vec<WholeRow>>,
    /// Full-text catalogs, stoplists and search property lists, by kind and
    /// id: a full-text index names each by number. A stoplist carries its
    /// words and a property list its properties, which decide what the index
    /// holds while neither name changes.
    pub fulltext: BTreeMap<(&'static str, i64), serde_json::Value>,
    /// The procedures with a numbered group (`proc;2`), which the capture
    /// does not read and refuses.
    pub numbered: BTreeSet<i64>,
    /// Each table type's own table object and the constraint objects on it,
    /// by the type's `user_type_id`. They are the engine's
    /// (`is_ms_shipped = 1`), so the ordinary object read never lists them.
    pub table_type_objects: BTreeMap<i64, BTreeSet<i64>>,
    /// Filegroups and partition schemes, by `data_space_id`, each as what it
    /// is: a filegroup by name, a scheme with its function, boundaries and
    /// destinations, since `SPLIT RANGE` or `NEXT USED` moves rows without
    /// renaming anything.
    pub data_spaces: BTreeMap<i64, serde_json::Value>,
    /// XML schema collections, by id: schema, name and the schemas it holds.
    /// A typed `xml` column validates against those, so `ALTER XML SCHEMA
    /// COLLECTION ... ADD` changes what it accepts.
    pub xml_collections: BTreeMap<i64, (String, String, String)>,
    /// Partition functions, by id, each with its parameter types and
    /// boundaries: a module calling `$PARTITION` binds one (class 21).
    pub partition_functions: BTreeMap<i64, serde_json::Value>,
    /// Column encryption keys, by id, each with its values and the master
    /// keys they are encrypted under: `ADD VALUE` or `DROP VALUE` changes
    /// what can unwrap the data without renaming the key.
    pub encryption_keys: BTreeMap<i64, serde_json::Value>,
    /// Each user-defined table type's structure, by its `user_type_id`.
    pub table_types: BTreeMap<i64, TableType>,
    pub constraints: BTreeMap<i64, Constraint>,
    pub dependencies: Vec<Dependency>,
    /// What each requested `(schema, name)` names in `sys.objects`, matched
    /// by the engine under the database's collation.
    pub objects_named: BTreeMap<(String, String), BTreeSet<i64>>,
    /// The same, in `sys.types`.
    pub types_named: BTreeMap<(String, String), BTreeSet<i64>>,
    /// The procedures `sys` holds under each requested name, as the catalog
    /// spells them: a one-part `EXEC` finds them first.
    pub system_named: BTreeMap<String, BTreeSet<String>>,
}

/// The names the scope asks the engine to look up. The engine matches them,
/// not this code: a name is compared under the database's collation, which
/// is usually case-insensitive and may ignore accents, and a match folded by
/// hand would answer differently from the resolution it stands for.
#[derive(Default)]
pub(super) struct Lookups {
    pub objects: BTreeSet<(String, String)>,
    pub types: BTreeSet<(String, String)>,
    pub system: BTreeSet<String>,
}

// `sys.sql_expression_dependencies` needs both: without SELECT on it the read
// is refused (229), and with SELECT but without VIEW DEFINITION on the
// database it reads no rows at all, which no answer can tell from no
// references. Measured on the pinned build (#2141), as a `db_ddladmin`.
const PERMISSIONS: &str = "SELECT \
    CONVERT(nvarchar(10), ISNULL(HAS_PERMS_BY_NAME(NULL, N'DATABASE', N'VIEW DEFINITION'), 0)) AS view_definition, \
    CONVERT(nvarchar(10), ISNULL(HAS_PERMS_BY_NAME(N'sys.sql_expression_dependencies', N'OBJECT', N'SELECT'), 0)) AS dependencies;";

const SESSION: &str = "SELECT \
    CONVERT(nvarchar(128), USER_NAME()) AS [user], \
    CONVERT(nvarchar(128), SCHEMA_NAME()) AS default_schema, \
    CONVERT(nvarchar(10), SESSIONPROPERTY(N'ANSI_NULLS')) AS ansi_nulls, \
    CONVERT(nvarchar(10), SESSIONPROPERTY(N'QUOTED_IDENTIFIER')) AS quoted_identifier, \
    CONVERT(nvarchar(10), SERVERPROPERTY(N'ProductMajorVersion')) AS major;";

const SCHEMAS: &str = "SELECT CONVERT(nvarchar(20), schema_id) AS id, \
    CONVERT(nvarchar(128), name) AS name, CONVERT(nvarchar(20), principal_id) AS owner \
    FROM sys.schemas;";

const PRINCIPALS: &str = "SELECT CONVERT(nvarchar(20), principal_id) AS id, \
    CONVERT(nvarchar(128), name) AS name, CONVERT(nvarchar(10), type) AS kind, \
    CONVERT(nvarchar(128), default_schema_name) AS default_schema, \
    CONVERT(nvarchar(20), owning_principal_id) AS owner \
    FROM sys.database_principals;";

const OBJECTS: &str = "SELECT CONVERT(nvarchar(20), object_id) AS id, \
    CONVERT(nvarchar(20), schema_id) AS [schema], CONVERT(nvarchar(128), name) AS name, \
    CONVERT(nvarchar(10), RTRIM(type)) AS kind, \
    CONVERT(nvarchar(20), NULLIF(parent_object_id, 0)) AS parent, \
    CONVERT(nvarchar(20), principal_id) AS owner \
    FROM sys.objects WHERE is_ms_shipped = 0;";

const MODULES: &str = "SELECT CONVERT(nvarchar(20), m.object_id) AS id, m.definition, \
    CONVERT(nvarchar(1), m.uses_ansi_nulls) AS ansi_nulls, \
    CONVERT(nvarchar(1), m.uses_quoted_identifier) AS quoted_identifier, \
    CONVERT(nvarchar(1), m.is_schema_bound) AS schema_bound, \
    CONVERT(nvarchar(20), m.execute_as_principal_id) AS execute_as \
    FROM sys.sql_modules m JOIN sys.objects o ON o.object_id = m.object_id \
    WHERE o.is_ms_shipped = 0;";

const COLUMNS: &str = "SELECT CONVERT(nvarchar(20), c.object_id) AS object, \
    CONVERT(nvarchar(20), c.column_id) AS id, CONVERT(nvarchar(128), c.name) AS name, \
    CONVERT(nvarchar(20), c.user_type_id) AS user_type, \
    CONVERT(nvarchar(20), c.max_length) AS max_length, \
    CONVERT(nvarchar(20), c.precision) AS precision, CONVERT(nvarchar(20), c.scale) AS scale, \
    CONVERT(nvarchar(128), c.collation_name) AS collation, \
    CONVERT(nvarchar(1), c.is_nullable) AS nullable, CONVERT(nvarchar(1), c.is_identity) AS [identity], \
    cc.definition AS computed, CONVERT(nvarchar(1), ISNULL(cc.is_persisted, 0)) AS persisted, \
    CONVERT(nvarchar(40), ic.seed_value) AS seed, CONVERT(nvarchar(40), ic.increment_value) AS increment, \
    CONVERT(nvarchar(1), ic.is_not_for_replication) AS identity_not_for_replication \
    FROM sys.columns c JOIN sys.objects o ON o.object_id = c.object_id \
    LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
    LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
    WHERE o.is_ms_shipped = 0;";

const PARAMETERS: &str = "SELECT CONVERT(nvarchar(20), p.object_id) AS object, \
    CONVERT(nvarchar(20), p.parameter_id) AS id, CONVERT(nvarchar(128), p.name) AS name, \
    CONVERT(nvarchar(20), p.user_type_id) AS user_type, \
    CONVERT(nvarchar(20), p.max_length) AS max_length, \
    CONVERT(nvarchar(20), p.precision) AS precision, CONVERT(nvarchar(20), p.scale) AS scale, \
    CONVERT(nvarchar(1), p.is_output) AS [output], \
    CONVERT(nvarchar(1), p.has_default_value) AS has_default, \
    CONVERT(nvarchar(1), p.is_readonly) AS readonly \
    FROM sys.parameters p JOIN sys.objects o ON o.object_id = p.object_id \
    WHERE o.is_ms_shipped = 0;";

const TYPES: &str = "SELECT CONVERT(nvarchar(20), user_type_id) AS id, \
    CONVERT(nvarchar(20), schema_id) AS [schema], CONVERT(nvarchar(128), name) AS name, \
    CONVERT(nvarchar(20), system_type_id) AS base, CONVERT(nvarchar(20), max_length) AS max_length, \
    CONVERT(nvarchar(20), precision) AS precision, CONVERT(nvarchar(20), scale) AS scale, \
    CONVERT(nvarchar(128), collation_name) AS collation, \
    CONVERT(nvarchar(1), is_nullable) AS nullable, \
    CONVERT(nvarchar(1), is_user_defined) AS user_defined, \
    CONVERT(nvarchar(1), is_table_type) AS table_type \
    FROM sys.types;";

const TRIGGERS: &str = "SELECT CONVERT(nvarchar(20), object_id) AS id, \
    CONVERT(nvarchar(1), is_disabled) AS disabled, CONVERT(nvarchar(1), is_instead_of_trigger) AS instead_of, \
    CONVERT(nvarchar(1), is_not_for_replication) AS not_for_replication \
    FROM sys.triggers WHERE parent_class = 1 AND is_ms_shipped = 0;";

const KEYS: &str = "SELECT CONVERT(nvarchar(20), kc.object_id) AS id, \
    CONVERT(nvarchar(60), i.type_desc) AS [index], CONVERT(nvarchar(1), i.is_disabled) AS disabled, \
    CONVERT(nvarchar(20), ic.key_ordinal) AS ordinal, \
    CONVERT(nvarchar(128), c.name) AS [column], CONVERT(nvarchar(1), ic.is_descending_key) AS descending \
    FROM sys.key_constraints kc \
    JOIN sys.indexes i ON i.object_id = kc.parent_object_id AND i.index_id = kc.unique_index_id \
    JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.key_ordinal > 0 \
    JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
    WHERE kc.is_ms_shipped = 0;";

const UNIQUE_INDEXES: &str = "SELECT CONVERT(nvarchar(20), i.object_id) AS id, \
    CONVERT(nvarchar(128), i.name) AS name, CONVERT(nvarchar(60), i.type_desc) AS [index], \
    CONVERT(nvarchar(1), i.is_disabled) AS disabled, \
    CONVERT(nvarchar(20), ic.key_ordinal) AS ordinal, CONVERT(nvarchar(128), c.name) AS [column], \
    CONVERT(nvarchar(1), ic.is_descending_key) AS descending \
    FROM sys.indexes i JOIN sys.objects o ON o.object_id = i.object_id AND o.is_ms_shipped = 0 \
    JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.key_ordinal > 0 \
    JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
    WHERE i.is_unique = 1 AND i.is_primary_key = 0 AND i.is_unique_constraint = 0;";

const FOREIGN_KEYS: &str = "SELECT CONVERT(nvarchar(20), fk.object_id) AS id, \
    CONVERT(nvarchar(20), fk.referenced_object_id) AS referenced, \
    CONVERT(nvarchar(20), kc.object_id) AS [key], CONVERT(nvarchar(128), i.name) AS [index], \
    CONVERT(nvarchar(60), fk.delete_referential_action_desc) AS on_delete, \
    CONVERT(nvarchar(60), fk.update_referential_action_desc) AS on_update, \
    CONVERT(nvarchar(1), fk.is_disabled) AS disabled, CONVERT(nvarchar(1), fk.is_not_trusted) AS not_trusted, \
    CONVERT(nvarchar(1), fk.is_not_for_replication) AS not_for_replication \
    FROM sys.foreign_keys fk \
    JOIN sys.indexes i ON i.object_id = fk.referenced_object_id AND i.index_id = fk.key_index_id \
    LEFT JOIN sys.key_constraints kc ON kc.parent_object_id = fk.referenced_object_id \
        AND kc.unique_index_id = fk.key_index_id \
    WHERE fk.is_ms_shipped = 0;";

const FOREIGN_KEY_COLUMNS: &str = "SELECT CONVERT(nvarchar(20), fkc.constraint_object_id) AS id, \
    CONVERT(nvarchar(20), fkc.constraint_column_id) AS ordinal, \
    CONVERT(nvarchar(128), pc.name) AS [column], CONVERT(nvarchar(128), rc.name) AS referenced \
    FROM sys.foreign_key_columns fkc \
    JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
    JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id;";

const TABLE_TYPE_COLUMNS: &str = "SELECT CONVERT(nvarchar(20), tt.user_type_id) AS type, \
    CONVERT(nvarchar(20), c.column_id) AS id, CONVERT(nvarchar(128), c.name) AS name, \
    CONVERT(nvarchar(20), c.user_type_id) AS user_type, \
    CONVERT(nvarchar(20), c.max_length) AS max_length, \
    CONVERT(nvarchar(20), c.precision) AS precision, CONVERT(nvarchar(20), c.scale) AS scale, \
    CONVERT(nvarchar(128), c.collation_name) AS collation, \
    CONVERT(nvarchar(1), c.is_nullable) AS nullable, CONVERT(nvarchar(1), c.is_identity) AS [identity], \
    cc.definition AS computed, dc.definition AS [default], \
    CONVERT(nvarchar(40), ic.seed_value) AS seed, CONVERT(nvarchar(40), ic.increment_value) AS increment, \
    CONVERT(nvarchar(1), ic.is_not_for_replication) AS identity_not_for_replication \
    FROM sys.table_types tt JOIN sys.columns c ON c.object_id = tt.type_table_object_id \
    LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
    LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
    LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id;";

const TABLE_TYPE_CHECKS: &str = "SELECT CONVERT(nvarchar(20), tt.user_type_id) AS type, ck.definition \
    FROM sys.table_types tt JOIN sys.check_constraints ck ON ck.parent_object_id = tt.type_table_object_id;";

const TABLE_TYPE_KEYS: &str = "SELECT CONVERT(nvarchar(20), tt.user_type_id) AS type, \
    CONVERT(nvarchar(20), i.index_id) AS [index], CONVERT(nvarchar(1), i.is_primary_key) AS [primary], \
    CONVERT(nvarchar(20), ic.key_ordinal) AS ordinal, CONVERT(nvarchar(128), c.name) AS [column], \
    CONVERT(nvarchar(1), ic.is_descending_key) AS descending \
    FROM sys.table_types tt JOIN sys.indexes i ON i.object_id = tt.type_table_object_id \
    JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.key_ordinal > 0 \
    JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
    WHERE i.is_primary_key = 1 OR i.is_unique_constraint = 1;";

const SYNONYMS: &str = "SELECT CONVERT(nvarchar(20), object_id) AS id, \
    CONVERT(nvarchar(1035), base_object_name) AS base FROM sys.synonyms;";

// A DEFAULT is never disabled or untrusted; a CHECK can be both, with its
// text unchanged (`ALTER TABLE ... NOCHECK CONSTRAINT`).
const CONSTRAINTS: &str = "SELECT CONVERT(nvarchar(20), object_id) AS id, \
    CONVERT(nvarchar(20), parent_column_id) AS [column], definition, \
    CONVERT(nvarchar(1), NULL) AS disabled, CONVERT(nvarchar(1), NULL) AS not_trusted, \
    CONVERT(nvarchar(1), NULL) AS not_for_replication \
    FROM sys.default_constraints \
    UNION ALL SELECT CONVERT(nvarchar(20), object_id), CONVERT(nvarchar(20), parent_column_id), \
    definition, CONVERT(nvarchar(1), is_disabled), CONVERT(nvarchar(1), is_not_trusted), \
    CONVERT(nvarchar(1), is_not_for_replication) \
    FROM sys.check_constraints;";

const DEPENDENCIES: &str = "SELECT CONVERT(nvarchar(20), referencing_id) AS referencing, \
    CONVERT(nvarchar(20), referencing_minor_id) AS referencing_column, \
    CONVERT(nvarchar(20), referenced_class) AS class, \
    CONVERT(nvarchar(128), referenced_server_name) AS server, \
    CONVERT(nvarchar(128), referenced_database_name) AS [database], \
    CONVERT(nvarchar(128), referenced_schema_name) AS [schema], \
    CONVERT(nvarchar(128), referenced_entity_name) AS entity, \
    CONVERT(nvarchar(20), referenced_id) AS referenced, \
    CONVERT(nvarchar(20), referenced_minor_id) AS referenced_column, \
    CONVERT(nvarchar(1), is_schema_bound_reference) AS schema_bound, \
    CONVERT(nvarchar(1), is_caller_dependent) AS caller_dependent \
    FROM sys.sql_expression_dependencies WHERE referencing_class = 1;";

/// Which rows of a view describe this database's own objects: those of its
/// user objects, and of its table types' own table objects, which the engine
/// marks as its own (`is_ms_shipped = 1`).
fn whole_filter(view: &str) -> String {
    const OWN: &str = "(SELECT object_id FROM sys.objects WHERE is_ms_shipped = 0 \
        UNION ALL SELECT type_table_object_id FROM sys.table_types)";
    match view {
        "types" | "table_types" => String::new(),
        "foreign_key_columns" => format!(" WHERE constraint_object_id IN {OWN}"),
        "triggers" => " WHERE parent_class = 1 AND is_ms_shipped = 0".to_owned(),
        _ => format!(" WHERE object_id IN {OWN}"),
    }
}

/// Every column of `sys.<view>`, from the engine's own list, so a column a
/// later build adds is read without anyone naming it.
fn whole_columns(view: &str) -> String {
    format!(
        "SELECT CONVERT(nvarchar(128), name) AS name, CONVERT(nvarchar(128), TYPE_NAME(system_type_id)) AS type \
         FROM sys.all_columns WHERE object_id = OBJECT_ID(N'sys.{view}') ORDER BY column_id;"
    )
}

fn whole_select(view: &str, columns: &[(String, String)]) -> String {
    let list = columns
        .iter()
        .map(|(name, kind)| {
            let quoted = format!("[{}]", name.replace(']', "]]"));
            // Each in a style that loses nothing: the default style keeps a
            // `datetime` to the minute and a `float` to six digits. A binary
            // value is read as hex.
            let style = match kind.as_str() {
                "binary" | "varbinary" | "image" | "timestamp" => ", 1",
                "datetime" | "datetime2" | "smalldatetime" | "date" | "time" | "datetimeoffset" => {
                    ", 126"
                }
                "float" | "real" => ", 3",
                // A variant holds any of those: 126 keeps a date to the
                // millisecond and a float whole, and leaves the rest as is.
                "sql_variant" => ", 126",
                _ => "",
            };
            format!("CONVERT(nvarchar(max), {quoted}{style}) AS {quoted}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT {list} FROM sys.{view}{};", whole_filter(view))
}

const TABLE_TYPE_OBJECTS: &str = "SELECT CONVERT(nvarchar(20), tt.user_type_id) AS type, \
    CONVERT(nvarchar(20), o.object_id) AS id FROM sys.table_types tt \
    JOIN sys.objects o ON o.object_id = tt.type_table_object_id OR o.parent_object_id = tt.type_table_object_id;";

const FULLTEXT_NAMES: &[(&str, &str)] = &[
    (
        "catalog",
        "SELECT CONVERT(nvarchar(20), fulltext_catalog_id) AS id, CONVERT(nvarchar(128), name) AS name FROM sys.fulltext_catalogs;",
    ),
    (
        "stoplist",
        "SELECT CONVERT(nvarchar(20), stoplist_id) AS id, CONVERT(nvarchar(128), name) AS name FROM sys.fulltext_stoplists;",
    ),
    (
        "property list",
        "SELECT CONVERT(nvarchar(20), property_list_id) AS id, CONVERT(nvarchar(128), name) AS name FROM sys.registered_search_property_lists;",
    ),
];

/// A stoplist's words and a property list's properties, by the list's id,
/// each row's members as text.
const FULLTEXT_MEMBERS: &[(&str, &str)] = &[
    (
        "catalog",
        "SELECT CONVERT(nvarchar(20), fulltext_catalog_id) AS id, CONVERT(nvarchar(4000), CONCAT(N'accent_sensitivity:', is_accent_sensitivity_on, N';default:', is_default)) AS member FROM sys.fulltext_catalogs;",
    ),
    (
        "stoplist",
        "SELECT CONVERT(nvarchar(20), stoplist_id) AS id, CONVERT(nvarchar(4000), CONCAT(language_id, N':', stopword)) AS member FROM sys.fulltext_stopwords;",
    ),
    (
        "property list",
        "SELECT CONVERT(nvarchar(20), property_list_id) AS id, CONVERT(nvarchar(4000), CONCAT(property_set_guid, N':', property_int_id, N':', property_name)) AS member FROM sys.registered_search_properties;",
    ),
];

const NUMBERED: &str = "SELECT DISTINCT CONVERT(nvarchar(20), object_id) AS id \
    FROM sys.numbered_procedures WHERE procedure_number > 1;";

const DATA_SPACES: &str = "SELECT CONVERT(nvarchar(20), data_space_id) AS id, \
    CONVERT(nvarchar(128), name) AS name, CONVERT(nvarchar(10), type) AS type FROM sys.data_spaces;";

const PARTITION_FUNCTIONS: &str = "SELECT CONVERT(nvarchar(20), function_id) AS function_id, \
    CONVERT(nvarchar(128), name) AS name, CONVERT(nvarchar(10), type) AS type, \
    CONVERT(nvarchar(1), boundary_value_on_right) AS on_right FROM sys.partition_functions;";

const PARTITION_SCHEMES: &str = "SELECT CONVERT(nvarchar(20), data_space_id) AS id, \
    CONVERT(nvarchar(20), function_id) AS function_id FROM sys.partition_schemes;";

const PARTITION_PARAMETERS: &str = "SELECT CONVERT(nvarchar(20), function_id) AS function_id, \
    CONVERT(nvarchar(20), parameter_id) AS parameter, CONVERT(nvarchar(20), system_type_id) AS system_type, \
    CONVERT(nvarchar(20), max_length) AS max_length, CONVERT(nvarchar(20), precision) AS precision, \
    CONVERT(nvarchar(20), scale) AS scale, CONVERT(nvarchar(128), collation_name) AS collation \
    FROM sys.partition_parameters;";

const PARTITION_VALUES: &str = "SELECT CONVERT(nvarchar(20), function_id) AS function_id, \
    CONVERT(nvarchar(20), boundary_id) AS boundary, CONVERT(nvarchar(20), parameter_id) AS parameter, \
    CONVERT(nvarchar(max), value, 126) AS value FROM sys.partition_range_values;";

const PARTITION_DESTINATIONS: &str = "SELECT CONVERT(nvarchar(20), partition_scheme_id) AS id, \
    CONVERT(nvarchar(20), destination_id) AS destination, CONVERT(nvarchar(20), data_space_id) AS data_space \
    FROM sys.destination_data_spaces;";

const MASTER_KEYS: &str = "SELECT CONVERT(nvarchar(20), column_master_key_id) AS id, \
    CONVERT(nvarchar(128), name) AS name, CONVERT(nvarchar(128), key_store_provider_name) AS provider, \
    CONVERT(nvarchar(4000), key_path) AS path, CONVERT(nvarchar(1), allow_enclave_computations) AS enclave, \
    CONVERT(nvarchar(max), signature, 1) AS signature FROM sys.column_master_keys;";

const ENCRYPTION_KEY_VALUES: &str = "SELECT CONVERT(nvarchar(20), column_encryption_key_id) AS id, \
    CONVERT(nvarchar(20), column_master_key_id) AS master, \
    CONVERT(nvarchar(128), encryption_algorithm_name) AS algorithm, \
    CONVERT(nvarchar(max), encrypted_value, 1) AS value FROM sys.column_encryption_key_values;";

const XML_COLLECTIONS: &str = "SELECT CONVERT(nvarchar(20), xml_collection_id) AS id, \
    CONVERT(nvarchar(128), SCHEMA_NAME(schema_id)) AS [schema], CONVERT(nvarchar(128), name) AS name, \
    CONVERT(nvarchar(max), XML_SCHEMA_NAMESPACE(SCHEMA_NAME(schema_id), name)) AS definition \
    FROM sys.xml_schema_collections WHERE xml_collection_id > 1;";

const ENCRYPTION_KEYS: &str = "SELECT CONVERT(nvarchar(20), column_encryption_key_id) AS id, \
    CONVERT(nvarchar(128), name) AS name FROM sys.column_encryption_keys;";

/// Lookups go in chunks, so one statement's text stays bounded however large
/// the scope is.
const LOOKUP_CHUNK: usize = 500;

fn values(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(schema, name)| {
            format!(
                "({}, {})",
                crate::ident::literal(schema),
                crate::ident::literal(name)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn object_lookup(pairs: &[(String, String)]) -> String {
    format!(
        "SELECT q.s AS requested_schema, q.n AS requested_name, CONVERT(nvarchar(20), o.object_id) AS id \
         FROM (VALUES {}) AS q(s, n) \
         JOIN sys.objects o ON o.name = q.n COLLATE DATABASE_DEFAULT \
         JOIN sys.schemas s ON s.schema_id = o.schema_id AND s.name = q.s COLLATE DATABASE_DEFAULT \
         WHERE o.is_ms_shipped = 0;",
        values(pairs)
    )
}

fn type_lookup(pairs: &[(String, String)]) -> String {
    format!(
        "SELECT q.s AS requested_schema, q.n AS requested_name, CONVERT(nvarchar(20), t.user_type_id) AS id \
         FROM (VALUES {}) AS q(s, n) \
         JOIN sys.types t ON t.name = q.n COLLATE DATABASE_DEFAULT \
         JOIN sys.schemas s ON s.schema_id = t.schema_id AND s.name = q.s COLLATE DATABASE_DEFAULT;",
        values(pairs)
    )
}

// The name is compared under the database's collation, as the `EXEC` of a
// one-part name inside one of its modules is.
fn system_lookup(names: &[String]) -> String {
    format!(
        "SELECT q.n AS requested_name, CONVERT(nvarchar(128), o.name) AS name \
         FROM (VALUES {}) AS q(n) \
         JOIN sys.all_objects o ON o.name = q.n COLLATE DATABASE_DEFAULT \
         WHERE o.schema_id = SCHEMA_ID(N'sys') AND o.type IN ('P', 'X', 'PC', 'RF');",
        names
            .iter()
            .map(|name| format!("({})", crate::ident::literal(name)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

const LEDGER_PRESENT: &str = "SELECT CONVERT(nvarchar(1), CASE WHEN OBJECT_ID(N'dbo.__pbps_state', N'U') IS NULL THEN 0 ELSE 1 END) AS present;";

const LEDGER_LATEST: &str = "SELECT TOP (1) CONVERT(nvarchar(20), id) AS id, state_json \
    FROM dbo.__pbps_state ORDER BY id DESC;";

/// One row's columns as text, by name.
struct Text<'a>(&'a pbps_db::Row);

impl Text<'_> {
    fn optional(&self, column: &str) -> Result<Option<String>, CaptureError> {
        self.0
            .try_get::<&str>(column)
            .map(|value| value.map(str::to_owned))
            .map_err(|_| CaptureError::Incomplete)
    }
    fn required(&self, column: &str) -> Result<String, CaptureError> {
        self.optional(column)?.ok_or(CaptureError::Incomplete)
    }
    fn number(&self, column: &str) -> Result<i64, CaptureError> {
        self.required(column)?
            .parse()
            .map_err(|_| CaptureError::Incomplete)
    }
    fn optional_number(&self, column: &str) -> Result<Option<i64>, CaptureError> {
        self.optional(column)?
            .map(|value| value.parse().map_err(|_| CaptureError::Incomplete))
            .transpose()
    }
    fn flag(&self, column: &str) -> Result<bool, CaptureError> {
        match self.required(column)?.as_str() {
            "1" => Ok(true),
            "0" => Ok(false),
            _ => Err(CaptureError::Incomplete),
        }
    }
}

/// An identity's seed, increment and `NOT FOR REPLICATION`, or `None` for a
/// column with no identity; one without the other is incomplete input.
fn identity_spec(row: &Text<'_>) -> Result<Option<(String, String, bool)>, CaptureError> {
    match (row.optional("seed")?, row.optional("increment")?) {
        (Some(seed), Some(increment)) => Ok(Some((
            seed,
            increment,
            row.flag("identity_not_for_replication")?,
        ))),
        (None, None) => Ok(None),
        _ => Err(CaptureError::Incomplete),
    }
}

async fn rows(
    conn: &mut impl QueryConnection,
    sql: &str,
) -> Result<Vec<pbps_db::Row>, CaptureError> {
    conn.query(sql).await.map_err(|_| CaptureError::Read)
}

async fn session(conn: &mut impl QueryConnection) -> Result<(Session, u32), CaptureError> {
    let rows = rows(conn, SESSION).await?;
    let [row] = rows.as_slice() else {
        return Err(CaptureError::Incomplete);
    };
    let row = Text(row);
    let major = row
        .required("major")?
        .parse()
        .map_err(|_| CaptureError::Version)?;
    Ok((
        Session {
            user: row.required("user")?,
            default_schema: row.required("default_schema")?,
            ansi_nulls: row.required("ansi_nulls")?,
            quoted_identifier: row.required("quoted_identifier")?,
        },
        major,
    ))
}

/// The permissions without which the dependency catalog reads short, asked
/// before it is read. A missing one refuses by name.
async fn qualify(conn: &mut impl QueryConnection) -> Result<(), CaptureError> {
    let rows = rows(conn, PERMISSIONS).await?;
    let [row] = rows.as_slice() else {
        return Err(CaptureError::Incomplete);
    };
    let row = Text(row);
    if row.required("view_definition")? != "1" {
        return Err(CaptureError::Coverage(super::Uncovered::class(
            "sys.sql_expression_dependencies",
            "the capture needs VIEW DEFINITION on the database: without it the dependency catalog reads empty",
        )));
    }
    if row.required("dependencies")? != "1" {
        return Err(CaptureError::Coverage(super::Uncovered::class(
            "sys.sql_expression_dependencies",
            "the capture needs SELECT on sys.sql_expression_dependencies, which only db_owner holds by default",
        )));
    }
    Ok(())
}

async fn baseline(conn: &mut impl QueryConnection) -> Result<Baseline, CaptureError> {
    let present = rows(conn, LEDGER_PRESENT).await?;
    let [present] = present.as_slice() else {
        return Err(CaptureError::Incomplete);
    };
    if !Text(present).flag("present")? {
        return Ok(Baseline::Absent);
    }
    let latest = rows(conn, LEDGER_LATEST).await?;
    match latest.as_slice() {
        [] => Ok(Baseline::Empty),
        [row] => {
            let row = Text(row);
            Ok(Baseline::Recorded {
                entry: row.number("id")?,
                state: row.required("state_json")?,
            })
        }
        _ => Err(CaptureError::Incomplete),
    }
}

/// The data spaces and column encryption keys, each as the definition it
/// names rather than its name alone.
async fn data_spaces_and_keys(
    conn: &mut impl QueryConnection,
) -> Result<
    (
        BTreeMap<i64, serde_json::Value>,
        BTreeMap<i64, serde_json::Value>,
        BTreeMap<i64, serde_json::Value>,
    ),
    CaptureError,
> {
    use serde_json::json;
    let mut names = BTreeMap::new();
    let mut kinds = BTreeMap::new();
    for row in rows(conn, DATA_SPACES).await? {
        let row = Text(&row);
        let id = row.number("id")?;
        names.insert(id, row.required("name")?);
        kinds.insert(id, row.required("type")?);
    }
    let mut parameters: BTreeMap<i64, Vec<(i64, serde_json::Value)>> = BTreeMap::new();
    for row in rows(conn, PARTITION_PARAMETERS).await? {
        let row = Text(&row);
        parameters
            .entry(row.number("function_id")?)
            .or_default()
            .push((
                row.number("parameter")?,
                json!({
                    "system_type": row.required("system_type")?,
                    "max_length": row.required("max_length")?,
                    "precision": row.required("precision")?,
                    "scale": row.required("scale")?,
                    "collation": row.optional("collation")?,
                }),
            ));
    }
    let mut values: BTreeMap<i64, Vec<(i64, i64, Option<String>)>> = BTreeMap::new();
    for row in rows(conn, PARTITION_VALUES).await? {
        let row = Text(&row);
        values.entry(row.number("function_id")?).or_default().push((
            row.number("boundary")?,
            row.number("parameter")?,
            row.optional("value")?,
        ));
    }
    let mut destinations: BTreeMap<i64, Vec<(i64, i64)>> = BTreeMap::new();
    for row in rows(conn, PARTITION_DESTINATIONS).await? {
        let row = Text(&row);
        destinations
            .entry(row.number("id")?)
            .or_default()
            .push((row.number("destination")?, row.number("data_space")?));
    }
    // Each function whole first: two schemes may share one.
    let mut functions = BTreeMap::new();
    for row in rows(conn, PARTITION_FUNCTIONS).await? {
        let row = Text(&row);
        let function = row.number("function_id")?;
        let mut parameters = parameters.remove(&function).unwrap_or_default();
        parameters.sort_by_key(|(id, _)| *id);
        let mut boundaries = values.remove(&function).unwrap_or_default();
        boundaries.sort();
        functions.insert(
            function,
            json!({
                "name": row.required("name")?,
                "type": row.required("type")?,
                "boundary_on_right": row.flag("on_right")?,
                "parameters": parameters.into_iter().map(|(_, p)| p).collect::<Vec<_>>(),
                "boundaries": boundaries.into_iter().map(|(_, p, v)| json!([p, v])).collect::<Vec<_>>(),
            }),
        );
    }
    let mut schemes = BTreeMap::new();
    for row in rows(conn, PARTITION_SCHEMES).await? {
        let row = Text(&row);
        let function = functions
            .get(&row.number("function_id")?)
            .cloned()
            .ok_or(CaptureError::Incomplete)?;
        let id = row.number("id")?;
        let mut to = destinations.remove(&id).unwrap_or_default();
        to.sort();
        let to = to
            .into_iter()
            .map(|(_, space)| names.get(&space).cloned().ok_or(CaptureError::Incomplete))
            .collect::<Result<Vec<_>, _>>()?;
        schemes.insert(id, json!({"function": function, "destinations": to}));
    }
    let spaces = names
        .iter()
        .map(|(id, name)| {
            (
                *id,
                json!({"name": name, "type": kinds.get(id), "scheme": schemes.get(id)}),
            )
        })
        .collect();

    let mut masters = BTreeMap::new();
    for row in rows(conn, MASTER_KEYS).await? {
        let row = Text(&row);
        masters.insert(
            row.number("id")?,
            json!({
                "name": row.required("name")?,
                "provider": row.required("provider")?,
                "path": row.required("path")?,
                "enclave": row.flag("enclave")?,
                "signature": row.optional("signature")?,
            }),
        );
    }
    let mut key_values: BTreeMap<i64, Vec<serde_json::Value>> = BTreeMap::new();
    for row in rows(conn, ENCRYPTION_KEY_VALUES).await? {
        let row = Text(&row);
        let master = masters
            .get(&row.number("master")?)
            .cloned()
            .ok_or(CaptureError::Incomplete)?;
        key_values
            .entry(row.number("id")?)
            .or_default()
            .push(json!({
                "master_key": master,
                "algorithm": row.required("algorithm")?,
                "value": row.required("value")?,
            }));
    }
    let mut keys = BTreeMap::new();
    for row in rows(conn, ENCRYPTION_KEYS).await? {
        let row = Text(&row);
        let id = row.number("id")?;
        let mut held = key_values.remove(&id).unwrap_or_default();
        held.sort_by_key(|value| value.to_string());
        keys.insert(id, json!({"name": row.required("name")?, "values": held}));
    }
    Ok((spaces, keys, functions))
}

/// A full-text catalog, stoplist or property list as compared: its name and
/// what it holds.
pub(super) fn fulltext_entry(name: String, members: Option<BTreeSet<String>>) -> serde_json::Value {
    serde_json::json!({"name": name, "members": members.unwrap_or_default()})
}

/// Every component, in turn, between two observations of the session. A
/// session input that moved while the components were read refuses: the
/// reads would then belong to two different sessions.
pub(super) async fn read(
    conn: &mut impl QueryConnection,
    lookups: &Lookups,
) -> Result<Catalog, CaptureError> {
    let (session_before, major) = session(conn).await?;
    qualify(conn).await?;

    let mut schemas = Vec::new();
    for row in rows(conn, SCHEMAS).await? {
        let row = Text(&row);
        schemas.push(Schema {
            id: row.number("id")?,
            name: row.required("name")?,
            owner: row.number("owner")?,
        });
    }
    let mut principals = Vec::new();
    for row in rows(conn, PRINCIPALS).await? {
        let row = Text(&row);
        principals.push(Principal {
            id: row.number("id")?,
            name: row.required("name")?,
            kind: row.required("kind")?,
            default_schema: row.optional("default_schema")?,
            owner: row.optional_number("owner")?,
        });
    }
    let mut objects = Vec::new();
    for row in rows(conn, OBJECTS).await? {
        let row = Text(&row);
        objects.push(Object {
            id: row.number("id")?,
            schema: row.number("schema")?,
            name: row.required("name")?,
            kind: row.required("kind")?,
            parent: row.optional_number("parent")?,
            owner: row.optional_number("owner")?,
        });
    }
    let mut modules = BTreeMap::new();
    for row in rows(conn, MODULES).await? {
        let row = Text(&row);
        modules.insert(
            row.number("id")?,
            Module {
                definition: row.optional("definition")?,
                ansi_nulls: row.flag("ansi_nulls")?,
                quoted_identifier: row.flag("quoted_identifier")?,
                schema_bound: row.flag("schema_bound")?,
                execute_as: row.optional_number("execute_as")?,
            },
        );
    }
    let mut columns = Vec::new();
    for row in rows(conn, COLUMNS).await? {
        let row = Text(&row);
        columns.push(Column {
            object: row.number("object")?,
            id: row.number("id")?,
            name: row.required("name")?,
            user_type: row.number("user_type")?,
            max_length: row.number("max_length")?,
            precision: row.number("precision")?,
            scale: row.number("scale")?,
            collation: row.optional("collation")?,
            nullable: row.flag("nullable")?,
            identity: row.flag("identity")?,
            identity_spec: identity_spec(&row)?,
            computed: row.optional("computed")?,
            persisted: row.flag("persisted")?,
        });
    }
    let mut parameters = Vec::new();
    for row in rows(conn, PARAMETERS).await? {
        let row = Text(&row);
        parameters.push(Parameter {
            object: row.number("object")?,
            id: row.number("id")?,
            name: row.optional("name")?.unwrap_or_default(),
            user_type: row.number("user_type")?,
            max_length: row.number("max_length")?,
            precision: row.number("precision")?,
            scale: row.number("scale")?,
            output: row.flag("output")?,
            has_default: row.flag("has_default")?,
            readonly: row.flag("readonly")?,
        });
    }
    let mut types = Vec::new();
    for row in rows(conn, TYPES).await? {
        let row = Text(&row);
        types.push(Type {
            id: row.number("id")?,
            schema: row.number("schema")?,
            name: row.required("name")?,
            base: row.number("base")?,
            max_length: row.number("max_length")?,
            precision: row.number("precision")?,
            scale: row.number("scale")?,
            collation: row.optional("collation")?,
            nullable: row.flag("nullable")?,
            user_defined: row.flag("user_defined")?,
            table_type: row.flag("table_type")?,
        });
    }
    let mut triggers = BTreeMap::new();
    for row in rows(conn, TRIGGERS).await? {
        let row = Text(&row);
        triggers.insert(
            row.number("id")?,
            (
                row.flag("disabled")?,
                row.flag("instead_of")?,
                row.flag("not_for_replication")?,
            ),
        );
    }
    let mut keys: BTreeMap<i64, Key> = BTreeMap::new();
    for row in rows(conn, KEYS).await? {
        let row = Text(&row);
        let key = keys.entry(row.number("id")?).or_default();
        key.index = row.required("index")?;
        key.disabled = row.flag("disabled")?;
        key.columns.push((
            row.number("ordinal")?,
            row.required("column")?,
            row.flag("descending")?,
        ));
    }
    let mut unique_indexes: BTreeMap<i64, BTreeMap<String, Key>> = BTreeMap::new();
    for row in rows(conn, UNIQUE_INDEXES).await? {
        let row = Text(&row);
        let index = unique_indexes
            .entry(row.number("id")?)
            .or_default()
            .entry(row.required("name")?)
            .or_default();
        index.index = row.required("index")?;
        index.disabled = row.flag("disabled")?;
        index.columns.push((
            row.number("ordinal")?,
            row.required("column")?,
            row.flag("descending")?,
        ));
    }
    let mut foreign_keys = BTreeMap::new();
    for row in rows(conn, FOREIGN_KEYS).await? {
        let row = Text(&row);
        foreign_keys.insert(
            row.number("id")?,
            ForeignKey {
                referenced: row.number("referenced")?,
                key: row.optional_number("key")?,
                index: row.required("index")?,
                on_delete: row.required("on_delete")?,
                on_update: row.required("on_update")?,
                disabled: row.flag("disabled")?,
                trusted: !row.flag("not_trusted")?,
                not_for_replication: row.flag("not_for_replication")?,
                columns: Vec::new(),
            },
        );
    }
    for row in rows(conn, FOREIGN_KEY_COLUMNS).await? {
        let row = Text(&row);
        foreign_keys
            .get_mut(&row.number("id")?)
            .ok_or(CaptureError::Incomplete)?
            .columns
            .push((
                row.number("ordinal")?,
                row.required("column")?,
                row.required("referenced")?,
            ));
    }
    let mut whole = BTreeMap::new();
    for &view in super::columns::VIEWS {
        let mut columns = Vec::new();
        for row in rows(conn, &whole_columns(view)).await? {
            let row = Text(&row);
            columns.push((row.required("name")?, row.required("type")?));
        }
        if columns.is_empty() {
            // A view this build does not have is no view read as empty.
            return Err(CaptureError::Coverage(super::Uncovered::class(
                "sys.all_columns",
                "a catalog view the capture reads whole reports no columns",
            )));
        }
        let mut read = Vec::new();
        for row in rows(conn, &whole_select(view, &columns)).await? {
            let row = Text(&row);
            let mut values = WholeRow::new();
            for (name, _) in &columns {
                values.insert(name.clone(), row.optional(name)?);
            }
            read.push(values);
        }
        whole.insert(view, read);
    }
    let mut data_spaces = BTreeMap::new();
    let mut encryption_keys = BTreeMap::new();
    let mut partition_functions = BTreeMap::new();
    let mut fulltext = BTreeMap::new();
    let mut members: BTreeMap<(&str, i64), BTreeSet<String>> = BTreeMap::new();
    for &(kind, sql) in FULLTEXT_MEMBERS {
        for row in rows(conn, sql).await? {
            let row = Text(&row);
            members
                .entry((kind, row.number("id")?))
                .or_default()
                .insert(row.required("member")?);
        }
    }
    for &(kind, sql) in FULLTEXT_NAMES {
        for row in rows(conn, sql).await? {
            let row = Text(&row);
            let id = row.number("id")?;
            fulltext.insert(
                (kind, id),
                fulltext_entry(row.required("name")?, members.remove(&(kind, id))),
            );
        }
    }
    let mut numbered = BTreeSet::new();
    for row in rows(conn, NUMBERED).await? {
        numbered.insert(Text(&row).number("id")?);
    }
    let mut table_type_objects: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    for row in rows(conn, TABLE_TYPE_OBJECTS).await? {
        let row = Text(&row);
        table_type_objects
            .entry(row.number("type")?)
            .or_default()
            .insert(row.number("id")?);
    }
    data_spaces_and_keys(conn)
        .await
        .map(|(spaces, keys, functions)| {
            data_spaces = spaces;
            encryption_keys = keys;
            partition_functions = functions;
        })?;
    let mut xml_collections = BTreeMap::new();
    for row in rows(conn, XML_COLLECTIONS).await? {
        let row = Text(&row);
        xml_collections.insert(
            row.number("id")?,
            (
                row.required("schema")?,
                row.required("name")?,
                row.required("definition")?,
            ),
        );
    }
    let mut table_types: BTreeMap<i64, TableType> = BTreeMap::new();
    for row in rows(conn, TABLE_TYPE_COLUMNS).await? {
        let row = Text(&row);
        table_types
            .entry(row.number("type")?)
            .or_default()
            .columns
            .push(TypeColumn {
                id: row.number("id")?,
                name: row.required("name")?,
                user_type: row.number("user_type")?,
                max_length: row.number("max_length")?,
                precision: row.number("precision")?,
                scale: row.number("scale")?,
                collation: row.optional("collation")?,
                nullable: row.flag("nullable")?,
                identity: row.flag("identity")?,
                identity_spec: identity_spec(&row)?,
                computed: row.optional("computed")?,
                default: row.optional("default")?,
            });
    }
    for row in rows(conn, TABLE_TYPE_CHECKS).await? {
        let row = Text(&row);
        table_types
            .entry(row.number("type")?)
            .or_default()
            .checks
            .push(row.required("definition")?);
    }
    for row in rows(conn, TABLE_TYPE_KEYS).await? {
        let row = Text(&row);
        table_types
            .entry(row.number("type")?)
            .or_default()
            .keys
            .push(TypeKey {
                index: row.number("index")?,
                primary: row.flag("primary")?,
                ordinal: row.number("ordinal")?,
                column: row.required("column")?,
                descending: row.flag("descending")?,
            });
    }
    let mut synonyms = BTreeMap::new();
    for row in rows(conn, SYNONYMS).await? {
        let row = Text(&row);
        synonyms.insert(row.number("id")?, row.required("base")?);
    }
    let mut constraints = BTreeMap::new();
    for row in rows(conn, CONSTRAINTS).await? {
        let row = Text(&row);
        constraints.insert(
            row.number("id")?,
            Constraint {
                column: row.number("column")?,
                definition: row.required("definition")?,
                disabled: row.optional("disabled")?.map(|v| v == "1"),
                trusted: row.optional("not_trusted")?.map(|v| v != "1"),
                not_for_replication: row.optional("not_for_replication")?.map(|v| v == "1"),
            },
        );
    }
    let mut dependencies = Vec::new();
    for row in rows(conn, DEPENDENCIES).await? {
        let row = Text(&row);
        dependencies.push(Dependency {
            referencing: row.number("referencing")?,
            referencing_column: row.number("referencing_column")?,
            class: row.number("class")?,
            server: row.optional("server")?,
            database: row.optional("database")?,
            schema: row.optional("schema")?,
            entity: row.required("entity")?,
            referenced: row.optional_number("referenced")?,
            referenced_column: row.number("referenced_column")?,
            schema_bound: row.flag("schema_bound")?,
            caller_dependent: row.flag("caller_dependent")?,
        });
    }
    let mut objects_named: BTreeMap<_, BTreeSet<i64>> = lookups
        .objects
        .iter()
        .map(|pair| (pair.clone(), BTreeSet::new()))
        .collect();
    let pairs: Vec<_> = lookups.objects.iter().cloned().collect();
    for chunk in pairs.chunks(LOOKUP_CHUNK) {
        for row in rows(conn, &object_lookup(chunk)).await? {
            let row = Text(&row);
            let key = (
                row.required("requested_schema")?,
                row.required("requested_name")?,
            );
            objects_named
                .get_mut(&key)
                .ok_or(CaptureError::Incomplete)?
                .insert(row.number("id")?);
        }
    }
    let mut types_named: BTreeMap<_, BTreeSet<i64>> = lookups
        .types
        .iter()
        .map(|pair| (pair.clone(), BTreeSet::new()))
        .collect();
    let pairs: Vec<_> = lookups.types.iter().cloned().collect();
    for chunk in pairs.chunks(LOOKUP_CHUNK) {
        for row in rows(conn, &type_lookup(chunk)).await? {
            let row = Text(&row);
            let key = (
                row.required("requested_schema")?,
                row.required("requested_name")?,
            );
            types_named
                .get_mut(&key)
                .ok_or(CaptureError::Incomplete)?
                .insert(row.number("id")?);
        }
    }
    let mut system_named: BTreeMap<_, BTreeSet<String>> = lookups
        .system
        .iter()
        .map(|name| (name.clone(), BTreeSet::new()))
        .collect();
    let names: Vec<_> = lookups.system.iter().cloned().collect();
    for chunk in names.chunks(LOOKUP_CHUNK) {
        for row in rows(conn, &system_lookup(chunk)).await? {
            let row = Text(&row);
            system_named
                .get_mut(&row.required("requested_name")?)
                .ok_or(CaptureError::Incomplete)?
                .insert(row.required("name")?);
        }
    }
    let baseline = baseline(conn).await?;

    let (session_after, major_after) = session(conn).await?;
    if session_after != session_before || major_after != major {
        return Err(CaptureError::EnvironmentChanged);
    }
    Ok(Catalog {
        major,
        session: session_before,
        baseline,
        schemas,
        principals,
        objects,
        modules,
        columns,
        parameters,
        types,
        synonyms,
        triggers,
        keys,
        unique_indexes,
        foreign_keys,
        whole,
        fulltext,
        numbered,
        table_type_objects,
        data_spaces,
        partition_functions,
        xml_collections,
        encryption_keys,
        table_types,
        constraints,
        dependencies,
        objects_named,
        types_named,
        system_named,
    })
}
