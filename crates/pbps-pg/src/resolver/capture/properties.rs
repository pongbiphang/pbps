//! Versioned property coverage for PostgreSQL resolution prerequisites.
//!
//! Every catalog field has an explicit disposition: retained scalar, logical
//! object reference, ACL, engine-rendered expression, or physical bookkeeping.
//! The last category is intentionally narrow; candidate identity alone is
//! never the property's fingerprint (ADR-0016, decision 9).

use super::logical::{self, Catalog, Row};
use pbps_db::resolver::capture::ObjectIdentity;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) const RULE: &str = "postgres-catalog-inputs-v1";
pub(super) const CLASSES: &[&str] = &[
    "pg_namespace",
    "pg_class",
    "pg_attribute",
    "pg_type",
    "pg_proc",
    "pg_operator",
    "pg_cast",
    "pg_collation",
    "pg_language",
    "pg_extension",
    "pg_roles",
    "pg_auth_members",
    "pg_db_role_setting",
    "pg_default_acl",
    "pg_rewrite",
    "pg_attrdef",
    "pg_constraint",
    "pg_index",
    "pg_sequence",
    "pg_range",
    "pg_enum",
    "pg_aggregate",
    "pg_am",
    "pg_amop",
    "pg_amproc",
    "pg_opclass",
    "pg_opfamily",
    "pg_partitioned_table",
    "pg_policy",
    "pg_trigger",
    "pg_database",
    "pg_parameter_acl",
    "pg_tablespace",
    "pg_inherits",
    "pg_depend",
    "pg_shdepend",
    "pg_init_privs",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Field {
    Scalar,
    Reference(&'static str),
    References(&'static str),
    Columns(&'static str),
    Column(&'static str),
    Acl,
    Definition,
    Physical,
    Address,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Uncovered {
    Version,
    Class,
    Field,
    Reference,
    Definition,
}

type Result<T> = std::result::Result<T, Uncovered>;

impl From<logical::Uncovered> for Uncovered {
    fn from(_: logical::Uncovered) -> Self {
        Self::Reference
    }
}

pub(super) fn fields(class: &str, major: u32) -> Result<&'static [(&'static str, &'static str)]> {
    if !matches!(major, 16 | 18) {
        return Err(Uncovered::Version);
    }
    layout(class, major).ok_or(Uncovered::Class)
}

/// Selecting known columns alone cannot notice a newly added catalog
/// property. Compare the snapshot's own complete relation descriptors before
/// accepting any property fingerprint, including changes within a major.
pub(super) fn qualify_layout(catalog: &Catalog, major: u32) -> Result<()> {
    let namespaces = catalog.rows.get("pg_namespace").ok_or(Uncovered::Class)?;
    let namespace = namespaces
        .iter()
        .find(|row| logical::string(row, "nspname") == Ok("pg_catalog"))
        .ok_or(Uncovered::Class)?;
    let namespace = logical::number(namespace, "oid")?;
    let relations = catalog.rows.get("pg_class").ok_or(Uncovered::Class)?;
    let attributes = catalog.rows.get("pg_attribute").ok_or(Uncovered::Class)?;
    let mut columns: BTreeMap<u32, Vec<(i32, String, String, u32)>> = BTreeMap::new();
    for attribute in attributes {
        let number = logical::signed(attribute, "attnum")?;
        if number <= 0 || attribute.get("attisdropped") == Some(&Value::Bool(true)) {
            continue;
        }
        let type_row = catalog.row("pg_type", logical::number(attribute, "atttypid")?)?;
        columns
            .entry(logical::number(attribute, "attrelid")?)
            .or_default()
            .push((
                number,
                logical::string(attribute, "attname")?.into(),
                logical::string(type_row, "typname")?.into(),
                logical::number(type_row, "typnamespace")?,
            ));
    }
    for entries in columns.values_mut() {
        entries.sort();
    }
    for &class in CLASSES {
        let mut found = relations.iter().filter(|row| {
            logical::number(row, "relnamespace") == Ok(namespace)
                && logical::string(row, "relname") == Ok(class)
        });
        let relation = found.next().ok_or(Uncovered::Class)?;
        if found.next().is_some() {
            return Err(Uncovered::Class);
        }
        let actual = columns
            .get(&logical::number(relation, "oid")?)
            .ok_or(Uncovered::Class)?;
        let expected = fields(class, major)?;
        if actual.len() != expected.len()
            || actual
                .iter()
                .zip(expected)
                .any(|((_, name, kind, ns), (n, k))| name != n || kind != k || *ns != namespace)
        {
            return Err(Uncovered::Field);
        }
    }
    Ok(())
}

/// Field names come from a qualified layout, never a target-supplied SQL
/// identifier. The reference map is semantic: an `oid` suffix is no proof
/// that a number refers to an object, nor which catalog owns it.
pub(super) fn field(class: &str, name: &str, kind: &str) -> Result<Field> {
    use Field::*;
    if name == "oid" {
        return Ok(Physical);
    }
    let physical = match class {
        // Storage locations, planner statistics, vacuum horizons and hints
        // change without changing resolution. Live membership is captured
        // separately; relnatts includes dropped physical attribute slots.
        "pg_class" => [
            "relfilenode",
            "relpages",
            "reltuples",
            "relallvisible",
            "relallfrozen",
            "reltoastrelid",
            "relnatts",
            "relchecks",
            "relhasindex",
            "relhasrules",
            "relhastriggers",
            "relhassubclass",
            "relrewrite",
            "relfrozenxid",
            "relminmxid",
        ]
        .contains(&name),
        "pg_attribute" => ["attnum", "attcacheoff", "attstattarget"].contains(&name),
        "pg_database" => ["datfrozenxid", "datminmxid"].contains(&name),
        // This is always a masked placeholder in the public view. Reading
        // pg_authid's password hash is neither required nor permitted here.
        "pg_roles" => name == "rolpassword",
        "pg_index" => name == "indcheckxmin",
        _ => false,
    };
    if physical {
        return Ok(Physical);
    }
    if kind == "_aclitem" {
        return Ok(Acl);
    }
    if kind == "pg_node_tree" {
        return Ok(Definition);
    }
    if matches!(class, "pg_depend" | "pg_shdepend" | "pg_init_privs") {
        return Ok(Address);
    }
    let reference = match (class, name) {
        ("pg_namespace", "nspowner")
        | ("pg_class", "relowner")
        | ("pg_type", "typowner")
        | ("pg_proc", "proowner")
        | ("pg_operator", "oprowner")
        | ("pg_collation", "collowner")
        | ("pg_language", "lanowner")
        | ("pg_extension", "extowner")
        | ("pg_opclass", "opcowner")
        | ("pg_opfamily", "opfowner")
        | ("pg_tablespace", "spcowner")
        | ("pg_database", "datdba")
        | ("pg_default_acl", "defaclrole")
        | ("pg_auth_members", "roleid" | "member" | "grantor")
        | ("pg_db_role_setting", "setrole") => Some("pg_authid"),
        ("pg_class", "relnamespace")
        | ("pg_type", "typnamespace")
        | ("pg_proc", "pronamespace")
        | ("pg_operator", "oprnamespace")
        | ("pg_collation", "collnamespace")
        | ("pg_extension", "extnamespace")
        | ("pg_constraint", "connamespace")
        | ("pg_opclass", "opcnamespace")
        | ("pg_opfamily", "opfnamespace")
        | ("pg_default_acl", "defaclnamespace") => Some("pg_namespace"),
        ("pg_class", "reltype" | "reloftype")
        | ("pg_attribute", "atttypid")
        | ("pg_type", "typelem" | "typarray" | "typbasetype")
        | ("pg_proc", "prorettype" | "provariadic")
        | ("pg_operator", "oprleft" | "oprright" | "oprresult")
        | ("pg_cast", "castsource" | "casttarget")
        | ("pg_constraint", "contypid")
        | ("pg_sequence", "seqtypid")
        | ("pg_range", "rngtypid" | "rngsubtype" | "rngmultitypid")
        | ("pg_enum", "enumtypid")
        | ("pg_aggregate", "aggtranstype" | "aggmtranstype")
        | ("pg_amop", "amoplefttype" | "amoprighttype")
        | ("pg_amproc", "amproclefttype" | "amprocrighttype")
        | ("pg_opclass", "opcintype" | "opckeytype") => Some("pg_type"),
        ("pg_inherits", "inhrelid" | "inhparent")
        | ("pg_attribute", "attrelid")
        | ("pg_type", "typrelid")
        | ("pg_rewrite", "ev_class")
        | ("pg_attrdef", "adrelid")
        | ("pg_constraint", "conrelid" | "conindid" | "confrelid")
        | ("pg_index", "indexrelid" | "indrelid")
        | ("pg_sequence", "seqrelid")
        | ("pg_partitioned_table", "partrelid" | "partdefid")
        | ("pg_policy", "polrelid")
        | ("pg_trigger", "tgrelid" | "tgconstrrelid" | "tgconstrindid") => Some("pg_class"),
        (
            "pg_type",
            "typsubscript" | "typinput" | "typoutput" | "typreceive" | "typsend" | "typmodin"
            | "typmodout" | "typanalyze",
        )
        | ("pg_proc", "prosupport")
        | ("pg_operator", "oprcode" | "oprrest" | "oprjoin")
        | ("pg_cast", "castfunc")
        | ("pg_language", "lanplcallfoid" | "laninline" | "lanvalidator")
        | ("pg_range", "rngcanonical" | "rngsubdiff")
        | (
            "pg_aggregate",
            "aggfnoid" | "aggtransfn" | "aggfinalfn" | "aggcombinefn" | "aggserialfn"
            | "aggdeserialfn" | "aggmtransfn" | "aggminvtransfn" | "aggmfinalfn",
        )
        | ("pg_am", "amhandler")
        | ("pg_amproc", "amproc")
        | ("pg_trigger", "tgfoid") => Some("pg_proc"),
        ("pg_operator", "oprcom" | "oprnegate")
        | ("pg_aggregate", "aggsortop")
        | ("pg_amop", "amopopr") => Some("pg_operator"),
        ("pg_attribute", "attcollation")
        | ("pg_type", "typcollation")
        | ("pg_range", "rngcollation") => Some("pg_collation"),
        ("pg_proc", "prolang") => Some("pg_language"),
        ("pg_class", "relam")
        | ("pg_amop", "amopmethod")
        | ("pg_opclass", "opcmethod")
        | ("pg_opfamily", "opfmethod") => Some("pg_am"),
        ("pg_amop", "amopfamily" | "amopsortfamily")
        | ("pg_amproc", "amprocfamily")
        | ("pg_opclass", "opcfamily") => Some("pg_opfamily"),
        ("pg_range", "rngsubopc") => Some("pg_opclass"),
        ("pg_constraint", "conparentid") | ("pg_trigger", "tgconstraint") => Some("pg_constraint"),
        ("pg_trigger", "tgparentid") => Some("pg_trigger"),
        ("pg_db_role_setting", "setdatabase") => Some("pg_database"),
        // Placement references retain their logical identity. Whether scratch
        // can reproduce a nondefault location is a reconstruction question.
        ("pg_class", "reltablespace") | ("pg_database", "dattablespace") => Some("pg_tablespace"),
        _ => None,
    };
    if let Some(class) = reference {
        return Ok(Reference(class));
    }
    match (class, name) {
        ("pg_proc", "proargtypes" | "proallargtypes" | "protrftypes") => Ok(References("pg_type")),
        ("pg_extension", "extconfig") => Ok(References("pg_class")),
        ("pg_constraint", "conpfeqop" | "conppeqop" | "conffeqop" | "conexclop") => {
            Ok(References("pg_operator"))
        }
        ("pg_index", "indcollation") | ("pg_partitioned_table", "partcollation") => {
            Ok(References("pg_collation"))
        }
        ("pg_index", "indclass") | ("pg_partitioned_table", "partclass") => {
            Ok(References("pg_opclass"))
        }
        ("pg_policy", "polroles") => Ok(References("pg_authid")),
        ("pg_constraint", "conkey") => Ok(Columns("conrelid")),
        ("pg_constraint", "confkey" | "confdelsetcols") => Ok(Columns("confrelid")),
        ("pg_index", "indkey") => Ok(Columns("indrelid")),
        // Per-key DESC / NULLS FIRST flags, not attribute numbers.
        ("pg_index", "indoption") => Ok(Scalar),
        ("pg_partitioned_table", "partattrs") => Ok(Columns("partrelid")),
        ("pg_trigger", "tgattr") => Ok(Columns("tgrelid")),
        ("pg_attrdef", "adnum") => Ok(Column("adrelid")),
        _ if matches!(
            kind,
            "oid" | "regproc" | "oidvector" | "_oid" | "int2vector"
        ) =>
        {
            Err(Uncovered::Field)
        }
        _ => Ok(Scalar),
    }
}

fn reference(catalog: &Catalog, class: &str, value: &Value) -> Result<Value> {
    let oid = value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(Uncovered::Reference)?;
    if oid == 0 {
        return Ok(Value::Null);
    }
    // The built-in default tablespace is an engine-defined logical name;
    // capture reads pg_tablespace before using this path, like other classes.
    Ok(serde_json::to_value(catalog.object(class, oid)?).expect("object identity serializes"))
}

fn column(catalog: &Catalog, row: &Row, owner: &str, value: &Value) -> Result<Value> {
    let attribute = value
        .as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .ok_or(Uncovered::Reference)?;
    if attribute == 0 {
        return Ok(Value::Null);
    } // index expression slot
    Ok(
        serde_json::to_value(catalog.column(logical::number(row, owner)?, attribute)?)
            .expect("column identity serializes"),
    )
}

pub(super) fn normalize(
    catalog: &Catalog,
    class: &str,
    row: &Row,
    major: u32,
) -> Result<BTreeMap<String, Value>> {
    let layout = fields(class, major)?;
    if row.len() != layout.len() + 1 || !row.contains_key("__definitions") {
        return Err(Uncovered::Field);
    }
    let definitions = row["__definitions"]
        .as_object()
        .ok_or(Uncovered::Definition)?;
    let mut expected_definitions: Vec<_> = layout
        .iter()
        .filter(|(_, kind)| *kind == "pg_node_tree")
        .map(|(name, _)| *name)
        .collect();
    let complete = matches!(
        class,
        "pg_proc" | "pg_rewrite" | "pg_constraint" | "pg_index" | "pg_trigger"
    );
    if complete {
        expected_definitions.push("complete");
    }
    let provider = matches!(class, "pg_collation" | "pg_database");
    if provider {
        expected_definitions.push("actual_provider_version");
    }
    if definitions.len() != expected_definitions.len()
        || expected_definitions
            .iter()
            .any(|key| !definitions.contains_key(*key))
    {
        return Err(Uncovered::Definition);
    }
    let mut output = BTreeMap::new();
    if provider {
        let actual = &definitions["actual_provider_version"];
        if !actual.is_null() && !actual.as_str().is_some_and(|value| !value.is_empty()) {
            return Err(Uncovered::Definition);
        }
        output.insert("actual_provider_version".into(), actual.clone());
    }
    if class == "pg_class" {
        // SELECT * and named-composite input depend on live column order.
        // Dropped physical slots do not; ordinal OIDs/attnums never survive.
        output.insert(
            "column_order".into(),
            serde_json::to_value(catalog.ordered_columns(logical::number(row, "oid")?)?)
                .expect("column identities serialize"),
        );
    }
    if complete {
        let definition = &definitions["complete"];
        let required = class != "pg_proc" || matches!(logical::string(row, "prokind")?, "f" | "p");
        if required && !definition.as_str().is_some_and(|s| !s.is_empty()) {
            return Err(Uncovered::Definition);
        }
        if !required && !definition.is_null() {
            return Err(Uncovered::Definition);
        }
        output.insert("engine_definition".to_owned(), definition.clone());
    }
    for &(name, kind) in layout {
        let value = row.get(name).ok_or(Uncovered::Field)?;
        let normalized = match field(class, name, kind)? {
            Field::Physical => continue,
            Field::Scalar => {
                // The raw pass uses a boolean presence marker instead of
                // invoking an unqualified array element's output function.
                // That marker is not a canonical property or a verifier.
                if kind == "anyarray" && !value.is_null() && !value.is_array() {
                    return Err(Uncovered::Definition);
                }
                value.clone()
            }
            Field::Reference(class) => reference(catalog, class, value)?,
            Field::References(class) => {
                if value.is_null() {
                    Value::Null
                } else {
                    Value::Array(
                        value
                            .as_array()
                            .ok_or(Uncovered::Reference)?
                            .iter()
                            .map(|v| reference(catalog, class, v))
                            .collect::<Result<Vec<_>>>()?,
                    )
                }
            }
            Field::Column(owner) => column(catalog, row, owner, value)?,
            Field::Columns(owner) => {
                if value.is_null() {
                    Value::Null
                } else {
                    Value::Array(
                        value
                            .as_array()
                            .ok_or(Uncovered::Reference)?
                            .iter()
                            .map(|v| column(catalog, row, owner, v))
                            .collect::<Result<Vec<_>>>()?,
                    )
                }
            }
            Field::Acl => acl(catalog, value)?,
            Field::Definition => {
                let definition = definitions.get(name).ok_or(Uncovered::Definition)?;
                // A present private node must have its complete engine output.
                // pg_node_tree's <> is the engine's explicit null node.
                let absent = value.is_null() || value.as_str() == Some("<>");
                if !absent && !definition.as_str().is_some_and(|s| !s.is_empty()) {
                    return Err(Uncovered::Definition);
                }
                if absent && !definition.is_null() {
                    return Err(Uncovered::Definition);
                }
                definition.clone()
            }
            Field::Address => return Err(Uncovered::Class),
        };
        output.insert(name.to_owned(), normalized);
    }
    Ok(output)
}

/// Dependency and extension-membership identities are composed from logical
/// object/subobject addresses. Catalog OIDs and object OIDs are only lookup
/// keys and do not survive normalization.
pub(super) fn address_row(
    catalog: &Catalog,
    class: &str,
    row: &Row,
    major: u32,
) -> Result<(ObjectIdentity, BTreeMap<String, Value>)> {
    let layout = fields(class, major)?;
    if row.len() != layout.len() + 1
        || row.get("__definitions") != Some(&json!({}))
        || layout.iter().any(|(field, _)| !row.contains_key(*field))
    {
        return Err(Uncovered::Field);
    }
    let (object, reference, kind) = match class {
        "pg_depend" => (
            catalog.address(
                logical::number(row, "classid")?,
                logical::number(row, "objid")?,
                logical::signed(row, "objsubid")?,
            )?,
            Some(catalog.address(
                logical::number(row, "refclassid")?,
                logical::number(row, "refobjid")?,
                logical::signed(row, "refobjsubid")?,
            )?),
            logical::string(row, "deptype")?,
        ),
        "pg_shdepend" => (
            catalog.address(
                logical::number(row, "classid")?,
                logical::number(row, "objid")?,
                logical::signed(row, "objsubid")?,
            )?,
            Some(catalog.address(
                logical::number(row, "refclassid")?,
                logical::number(row, "refobjid")?,
                0,
            )?),
            logical::string(row, "deptype")?,
        ),
        "pg_init_privs" => (
            catalog.address(
                logical::number(row, "classoid")?,
                logical::number(row, "objoid")?,
                logical::signed(row, "objsubid")?,
            )?,
            None,
            logical::string(row, "privtype")?,
        ),
        _ => return Err(Uncovered::Class),
    };
    let mut signature = vec![object];
    signature.extend(reference);
    let identity = ObjectIdentity {
        class: class.into(),
        name: vec![kind.into()],
        signature,
    };
    let mut properties = BTreeMap::new();
    if class == "pg_init_privs" {
        properties.insert(
            "privileges".into(),
            acl(catalog, row.get("initprivs").ok_or(Uncovered::Field)?)?,
        );
    }
    Ok((identity, properties))
}

fn acl(catalog: &Catalog, value: &Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let mut result = Vec::new();
    for item in value.as_array().ok_or(Uncovered::Reference)? {
        let row = item.as_object().ok_or(Uncovered::Reference)?;
        if row.len() != 4 {
            return Err(Uncovered::Field);
        }
        if logical::number(row, "grantor")? == 0 {
            return Err(Uncovered::Reference);
        }
        let grantor = reference(
            catalog,
            "pg_authid",
            row.get("grantor").ok_or(Uncovered::Field)?,
        )?;
        let grantee_oid = row.get("grantee").ok_or(Uncovered::Field)?;
        let grantee = if grantee_oid.as_u64() == Some(0) {
            json!(ObjectIdentity {
                class: "public-principal".into(),
                name: vec!["PUBLIC".into()],
                signature: vec![]
            })
        } else {
            reference(catalog, "pg_authid", grantee_oid)?
        };
        let privilege = logical::string(row, "privilege_type")?;
        let option = row
            .get("is_grantable")
            .and_then(Value::as_bool)
            .ok_or(Uncovered::Field)?;
        result.push(json!({"grantor":grantor,"grantee":grantee,"privilege":privilege,"grant_option":option}));
    }
    result.sort_by_cached_key(Value::to_string);
    Ok(Value::Array(result))
}

include!("catalog_fields.rs");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_qualified_reference_column_has_an_explicit_disposition() {
        for major in [16, 18] {
            for class in CLASSES {
                for &(name, kind) in fields(class, major).unwrap() {
                    assert!(
                        field(class, name, kind).is_ok(),
                        "PG{major} {class}.{name} ({kind})"
                    );
                }
            }
        }
        assert_eq!(field("pg_proc", "future_ref", "oid"), Err(Uncovered::Field));
        assert_eq!(fields("pg_proc", 17), Err(Uncovered::Version));
        assert_eq!(fields("pg_future", 18), Err(Uncovered::Class));
    }

    #[test]
    fn default_acl_and_an_explicit_empty_acl_are_distinct() {
        let catalog = Catalog::new(BTreeMap::new()).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(acl(&catalog, &Value::Null), Ok(Value::Null));
        assert_eq!(acl(&catalog, &json!([])), Ok(json!([])));
        assert!(acl(&catalog, &json!([{}])).is_err());
        assert!(
            acl(
                &catalog,
                &json!([{"grantor":0,"grantee":0,"privilege_type":"SELECT","is_grantable":false}])
            )
            .is_err()
        );
    }
}
