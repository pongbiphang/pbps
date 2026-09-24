//! Resolve snapshot-local numbers using only snapshot rows. In particular,
//! regtype/pg_get_userbyid and other syscache lookups cannot supply names for
//! an older catalog snapshot (the rendering limit in docs/PITFALLS.md).

use pbps_db::resolver::capture::ObjectIdentity;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub(super) type Row = Map<String, Value>;

// No Debug or Serialize: rows include complete source and private properties.
pub(super) struct Catalog {
    pub rows: BTreeMap<String, Vec<Row>>,
    by_oid: BTreeMap<(String, u32), usize>,
    by_attribute: BTreeMap<(u32, i32), usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Uncovered {
    MissingClass,
    MissingObject,
    MissingProperty,
    MalformedProperty,
    DuplicateObject,
    UnsupportedClass,
    DroppedColumn,
}

type Result<T> = std::result::Result<T, Uncovered>;

impl Catalog {
    pub fn new(rows: BTreeMap<String, Vec<Row>>) -> Result<Self> {
        let mut by_oid = BTreeMap::new();
        for (class, members) in &rows {
            for (index, row) in members.iter().enumerate() {
                if row.contains_key("oid") {
                    let oid = number(row, "oid")?;
                    if oid == 0 || by_oid.insert((class.clone(), oid), index).is_some() {
                        return Err(Uncovered::DuplicateObject);
                    }
                }
            }
        }
        let mut by_attribute = BTreeMap::new();
        if let Some(attributes) = rows.get("pg_attribute") {
            for (index, row) in attributes.iter().enumerate() {
                let key = (number(row, "attrelid")?, signed(row, "attnum")?);
                if by_attribute.insert(key, index).is_some() {
                    return Err(Uncovered::DuplicateObject);
                }
            }
        }
        Ok(Self {
            rows,
            by_oid,
            by_attribute,
        })
    }

    pub fn row(&self, class: &str, oid: u32) -> Result<&Row> {
        // pg_roles deliberately exposes no password material. Its public OID
        // is the logical pg_authid object's OID, not a different object.
        let class = if class == "pg_authid" {
            "pg_roles"
        } else {
            class
        };
        let rows = self.rows.get(class).ok_or(Uncovered::MissingClass)?;
        let index = self
            .by_oid
            .get(&(class.to_owned(), oid))
            .ok_or(Uncovered::MissingObject)?;
        rows.get(*index).ok_or(Uncovered::MissingObject)
    }

    pub fn object(&self, class: &str, oid: u32) -> Result<ObjectIdentity> {
        self.identity(class, self.row(class, oid)?)
    }

    fn named(
        &self,
        class: &str,
        row: &Row,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<ObjectIdentity> {
        let mut parts = Vec::new();
        if let Some(namespace) = namespace {
            let ns = self.row("pg_namespace", number(row, namespace)?)?;
            parts.push(string(ns, "nspname")?.to_owned());
        }
        parts.push(string(row, name)?.to_owned());
        Ok(ObjectIdentity {
            class: class.to_owned(),
            name: parts,
            signature: Vec::new(),
        })
    }

    fn related(
        &self,
        class: &str,
        row: &Row,
        references: &[(&str, &str)],
        labels: &[&str],
    ) -> Result<ObjectIdentity> {
        let mut signature = Vec::new();
        for &(field, target) in references {
            let oid = number(row, field)?;
            // Optional schema on a default ACL means database-wide, not an
            // unreadable schema. Keep that explicit slot in its identity.
            signature.push(if oid == 0 {
                let optional = matches!(
                    (class, field),
                    ("pg_default_acl", "defaclnamespace")
                        | ("pg_db_role_setting", "setdatabase" | "setrole")
                        | ("pg_operator", "oprleft" | "oprright")
                        | ("pg_constraint", "conrelid" | "contypid")
                );
                if !optional {
                    return Err(Uncovered::MissingObject);
                }
                ObjectIdentity {
                    class: target.to_owned(),
                    name: Vec::new(),
                    signature: Vec::new(),
                }
            } else {
                self.object(target, oid)?
            });
        }
        let mut name = Vec::new();
        for label in labels {
            name.push(scalar(row.get(*label).ok_or(Uncovered::MissingProperty)?)?);
        }
        Ok(ObjectIdentity {
            class: class.to_owned(),
            name,
            signature,
        })
    }

    pub fn identity(&self, class: &str, row: &Row) -> Result<ObjectIdentity> {
        let class = if class == "pg_roles" {
            "pg_authid"
        } else {
            class
        };
        let id = match class {
            "pg_namespace" => self.named(class, row, "nspname", None)?,
            "pg_authid" => self.named(class, row, "rolname", None)?,
            "pg_database" => self.named(class, row, "datname", None)?,
            "pg_type" => self.named(class, row, "typname", Some("typnamespace"))?,
            "pg_class" => self.named(class, row, "relname", Some("relnamespace"))?,
            "pg_extension" => self.named(class, row, "extname", None)?,
            "pg_language" => self.named(class, row, "lanname", None)?,
            "pg_parameter_acl" => self.named(class, row, "parname", None)?,
            "pg_am" => self.named(class, row, "amname", None)?,
            "pg_tablespace" => self.named(class, row, "spcname", None)?,
            "pg_inherits" => self.related(
                class,
                row,
                &[("inhrelid", "pg_class"), ("inhparent", "pg_class")],
                &[],
            )?,
            "pg_proc" => {
                let mut id = self.named(class, row, "proname", Some("pronamespace"))?;
                for oid in numbers(row, "proargtypes")? {
                    id.signature.push(self.object("pg_type", oid)?);
                }
                id
            }
            "pg_operator" => {
                let mut id = self.named(class, row, "oprname", Some("oprnamespace"))?;
                id.signature = self
                    .related(
                        class,
                        row,
                        &[("oprleft", "pg_type"), ("oprright", "pg_type")],
                        &[],
                    )?
                    .signature;
                id
            }
            "pg_collation" => {
                let mut id = self.named(class, row, "collname", Some("collnamespace"))?;
                id.name.push(scalar(
                    row.get("collencoding").ok_or(Uncovered::MissingProperty)?,
                )?);
                id
            }
            "pg_opclass" | "pg_opfamily" => {
                let (name, namespace, method) = if class == "pg_opclass" {
                    ("opcname", "opcnamespace", "opcmethod")
                } else {
                    ("opfname", "opfnamespace", "opfmethod")
                };
                let mut id = self.named(class, row, name, Some(namespace))?;
                id.signature
                    .push(self.object("pg_am", number(row, method)?)?);
                id
            }
            "pg_cast" => self.related(
                class,
                row,
                &[("castsource", "pg_type"), ("casttarget", "pg_type")],
                &[],
            )?,
            "pg_enum" => self.related(class, row, &[("enumtypid", "pg_type")], &["enumlabel"])?,
            "pg_range" => self.related(class, row, &[("rngtypid", "pg_type")], &[])?,
            "pg_sequence" => self.related(class, row, &[("seqrelid", "pg_class")], &[])?,
            "pg_aggregate" => self.related(class, row, &[("aggfnoid", "pg_proc")], &[])?,
            "pg_index" => self.related(class, row, &[("indexrelid", "pg_class")], &[])?,
            "pg_partitioned_table" => {
                self.related(class, row, &[("partrelid", "pg_class")], &[])?
            }
            "pg_rewrite" => self.related(class, row, &[("ev_class", "pg_class")], &["rulename"])?,
            "pg_trigger" => self.related(class, row, &[("tgrelid", "pg_class")], &["tgname"])?,
            "pg_policy" => self.related(class, row, &[("polrelid", "pg_class")], &["polname"])?,
            "pg_constraint" => self.related(
                class,
                row,
                &[
                    ("connamespace", "pg_namespace"),
                    ("conrelid", "pg_class"),
                    ("contypid", "pg_type"),
                ],
                &["conname"],
            )?,
            "pg_attribute" => self.column(number(row, "attrelid")?, signed(row, "attnum")?)?,
            "pg_attrdef" => {
                let column = self.column(number(row, "adrelid")?, signed(row, "adnum")?)?;
                ObjectIdentity {
                    class: class.to_owned(),
                    name: Vec::new(),
                    signature: vec![column],
                }
            }
            "pg_default_acl" => self.related(
                class,
                row,
                &[
                    ("defaclrole", "pg_authid"),
                    ("defaclnamespace", "pg_namespace"),
                ],
                &["defaclobjtype"],
            )?,
            "pg_auth_members" => self.related(
                class,
                row,
                &[
                    ("roleid", "pg_authid"),
                    ("member", "pg_authid"),
                    ("grantor", "pg_authid"),
                ],
                &[],
            )?,
            "pg_db_role_setting" => self.related(
                class,
                row,
                &[("setdatabase", "pg_database"), ("setrole", "pg_authid")],
                &[],
            )?,
            "pg_amop" => self.related(
                class,
                row,
                &[
                    ("amopfamily", "pg_opfamily"),
                    ("amoplefttype", "pg_type"),
                    ("amoprighttype", "pg_type"),
                ],
                &["amopstrategy", "amoppurpose"],
            )?,
            "pg_amproc" => self.related(
                class,
                row,
                &[
                    ("amprocfamily", "pg_opfamily"),
                    ("amproclefttype", "pg_type"),
                    ("amprocrighttype", "pg_type"),
                ],
                &["amprocnum"],
            )?,
            _ => return Err(Uncovered::UnsupportedClass),
        };
        Ok(id)
    }

    pub fn column(&self, relation: u32, attribute: i32) -> Result<ObjectIdentity> {
        let relation_id = self.object("pg_class", relation)?;
        let attributes = self
            .rows
            .get("pg_attribute")
            .ok_or(Uncovered::MissingClass)?;
        let index = self
            .by_attribute
            .get(&(relation, attribute))
            .ok_or(Uncovered::MissingObject)?;
        let row = attributes.get(*index).ok_or(Uncovered::MissingObject)?;
        match row.get("attisdropped").and_then(Value::as_bool) {
            Some(false) => {}
            Some(true) => return Err(Uncovered::DroppedColumn),
            None => return Err(Uncovered::MissingProperty),
        }
        Ok(ObjectIdentity {
            class: "column".to_owned(),
            name: vec![string(row, "attname")?.to_owned()],
            signature: vec![relation_id],
        })
    }

    pub fn ordered_columns(&self, relation: u32) -> Result<Vec<ObjectIdentity>> {
        let rows = self
            .rows
            .get("pg_attribute")
            .ok_or(Uncovered::MissingClass)?;
        let mut columns = Vec::new();
        for (&(_, attribute), &index) in self
            .by_attribute
            .range((relation, 1)..=(relation, i32::MAX))
        {
            let row = rows.get(index).ok_or(Uncovered::MissingObject)?;
            match row.get("attisdropped").and_then(Value::as_bool) {
                Some(true) => continue,
                Some(false) => columns.push(self.column(relation, attribute)?),
                None => return Err(Uncovered::MissingProperty),
            }
        }
        Ok(columns)
    }

    pub fn address(&self, class_oid: u32, oid: u32, subobject: i32) -> Result<ObjectIdentity> {
        let catalog = self.row("pg_class", class_oid)?;
        let namespace = self.row("pg_namespace", number(catalog, "relnamespace")?)?;
        if string(namespace, "nspname")? != "pg_catalog" {
            return Err(Uncovered::UnsupportedClass);
        }
        let class = string(catalog, "relname")?;
        if subobject != 0 {
            if class != "pg_class" {
                return Err(Uncovered::UnsupportedClass);
            }
            self.column(oid, subobject)
        } else {
            self.object(class, oid)
        }
    }
}

pub(super) fn string<'a>(row: &'a Row, field: &str) -> Result<&'a str> {
    row.get(field)
        .ok_or(Uncovered::MissingProperty)?
        .as_str()
        .ok_or(Uncovered::MalformedProperty)
}

pub(super) fn number(row: &Row, field: &str) -> Result<u32> {
    row.get(field)
        .ok_or(Uncovered::MissingProperty)?
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(Uncovered::MalformedProperty)
}

pub(super) fn signed(row: &Row, field: &str) -> Result<i32> {
    row.get(field)
        .ok_or(Uncovered::MissingProperty)?
        .as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .ok_or(Uncovered::MalformedProperty)
}

pub(super) fn numbers(row: &Row, field: &str) -> Result<Vec<u32>> {
    row.get(field)
        .ok_or(Uncovered::MissingProperty)?
        .as_array()
        .ok_or(Uncovered::MalformedProperty)?
        .iter()
        .map(|v| {
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(Uncovered::MalformedProperty)
        })
        .collect()
}

fn scalar(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => {
            Err(Uncovered::MalformedProperty)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog(offset: u32) -> Catalog {
        let object = |value: Value| value.as_object().unwrap().clone();
        let rows = BTreeMap::from([
            (
                "pg_namespace".into(),
                vec![object(json!({"oid":1+offset,"nspname":"a.b"}))],
            ),
            (
                "pg_type".into(),
                vec![object(
                    json!({"oid":2+offset,"typname":"t","typnamespace":1+offset}),
                )],
            ),
            (
                "pg_proc".into(),
                vec![object(
                    json!({"oid":3+offset,"proname":"f","pronamespace":1+offset,"proargtypes":[2+offset]}),
                )],
            ),
            (
                "pg_class".into(),
                vec![object(
                    json!({"oid":4+offset,"relname":"r","relnamespace":1+offset}),
                )],
            ),
            (
                "pg_attribute".into(),
                vec![object(
                    json!({"attrelid":4+offset,"attnum":2,"attname":"x.y","attisdropped":false}),
                )],
            ),
        ]);
        Catalog::new(rows).unwrap_or_else(|e| panic!("{e:?}"))
    }

    #[test]
    fn database_local_numbers_never_enter_logical_signatures() {
        let left = catalog(0);
        let right = catalog(1000);
        assert_eq!(left.object("pg_proc", 3), right.object("pg_proc", 1003));
        assert_eq!(left.column(4, 2), right.column(1004, 2));
        let routine = left.object("pg_proc", 3).unwrap();
        assert_eq!(routine.name, ["a.b", "f"]);
        assert_eq!(routine.signature[0].name, ["a.b", "t"]);
    }

    #[test]
    fn a_column_is_named_even_when_its_physical_position_changes() {
        let left = catalog(0);
        let mut rows = catalog(1000).rows;
        rows.get_mut("pg_attribute").unwrap()[0].insert("attnum".into(), json!(7));
        let right = Catalog::new(rows).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(left.column(4, 2), right.column(1004, 7));
        assert_eq!(right.column(1004, 2), Err(Uncovered::MissingObject));
    }

    #[test]
    fn unreadable_and_duplicate_references_cannot_become_absence() {
        let catalog = catalog(0);
        assert_eq!(catalog.object("pg_proc", 99), Err(Uncovered::MissingObject));
        assert_eq!(
            catalog.object("pg_extension", 99),
            Err(Uncovered::MissingClass)
        );
        let mut rows = catalog.rows;
        rows.get_mut("pg_proc").unwrap()[0].remove("proargtypes");
        let incomplete = Catalog::new(rows.clone()).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            incomplete.object("pg_proc", 3),
            Err(Uncovered::MissingProperty)
        );
        let duplicate = rows["pg_proc"][0].clone();
        rows.get_mut("pg_proc").unwrap().push(duplicate);
        assert!(matches!(
            Catalog::new(rows),
            Err(Uncovered::DuplicateObject)
        ));
    }

    #[test]
    fn a_missing_cast_endpoint_is_not_an_optional_identity_slot() {
        let catalog = catalog(0);
        let row = json!({"castsource":0,"casttarget":2})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            catalog.identity("pg_cast", &row),
            Err(Uncovered::MissingObject)
        );
        let row = json!({"castsource":2,"casttarget":2})
            .as_object()
            .unwrap()
            .clone();
        assert!(catalog.identity("pg_cast", &row).is_ok());
    }

    #[test]
    fn live_column_order_survives_without_physical_slots() {
        let mut rows = catalog(0).rows;
        let first = rows["pg_attribute"][0].clone();
        let mut second = first.clone();
        second.insert("attname".into(), json!("second"));
        second.insert("attnum".into(), json!(5));
        rows.get_mut("pg_attribute").unwrap().push(second);
        let original = Catalog::new(rows.clone()).unwrap_or_else(|e| panic!("{e:?}"));
        rows.get_mut("pg_attribute").unwrap()[1].insert("attnum".into(), json!(9));
        let gap = Catalog::new(rows.clone()).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(original.ordered_columns(4), gap.ordered_columns(4));
        rows.get_mut("pg_attribute").unwrap()[0].insert("attnum".into(), json!(10));
        let reordered = Catalog::new(rows).unwrap_or_else(|e| panic!("{e:?}"));
        assert_ne!(original.ordered_columns(4), reordered.ordered_columns(4));
    }

    #[test]
    fn dropped_columns_are_not_referenced_as_live_objects() {
        let mut rows = catalog(0).rows;
        rows.get_mut("pg_attribute").unwrap()[0].insert("attisdropped".into(), json!(true));
        let catalog = Catalog::new(rows).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(catalog.column(4, 2), Err(Uncovered::DroppedColumn));
    }
}
