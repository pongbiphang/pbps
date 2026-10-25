//! Complete naming domains and a non-executable caller of the clone allocator.
use super::clone_name;
use pbps_db::{Conn, DbError, Param};
use pbps_dialect::slots::{Namespace, TableSpace};
use pbps_diff::order::{catalog::graph::*, slots::Slots};
use pbps_model::{
    operation::{NameKind, ObjectKey},
    *,
};
use std::collections::{BTreeMap, BTreeSet};

pub async fn collect_names(conn: &mut Conn, schemas: &BTreeSet<String>) -> Result<Naming, DbError> {
    crate::modules::require_the_callers_transaction(conn).await?;
    let mut out = BTreeMap::new();
    for schema in schemas {
        let rows = conn.query_with(r#"
SELECT EXISTS(SELECT FROM pg_catalog.pg_namespace WHERE nspname=$1) AS exists,
 (SELECT COALESCE(json_agg(x),'[]')::text FROM (
   SELECT c.oid::int8 AS id,c.relname AS name,'pg_class' AS catalog
   FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1
   UNION ALL SELECT t.oid::int8,t.typname,'pg_type'
   FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid=t.typnamespace WHERE n.nspname=$1
   UNION ALL SELECT c.oid::int8,c.conname,'pg_constraint'
   FROM pg_catalog.pg_constraint c JOIN pg_catalog.pg_namespace n ON n.oid=c.connamespace WHERE n.nspname=$1
 ) x) AS objects"#, &[Param::Str(schema)]).await.map_err(|e| {
            let code=e.server_error_code().unwrap_or_else(||"unavailable".into());
            e.context(format!("{}: rebuild namespace occupancy is unreadable; SQLSTATE {code}", escape::Escaped(schema)))
        })?;
        let row = rows.first().ok_or_else(|| {
            DbError::Refused(format!(
                "{}: namespace read is absent",
                escape::Escaped(schema)
            ))
        })?;
        let mut found = NamespaceNames {
            exists: row
                .try_get::<bool>("exists")?
                .ok_or_else(|| DbError::BadRow("missing namespace existence".into()))?,
            ..Default::default()
        };
        #[derive(serde::Deserialize)]
        struct Entry {
            id: u64,
            name: ObjectLocalName,
            catalog: String,
        }
        let json = row
            .try_get::<&str>("objects")?
            .ok_or_else(|| DbError::BadRow("missing namespace objects".into()))?;
        for e in serde_json::from_str::<Vec<Entry>>(json)
            .map_err(|_| DbError::BadRow("invalid namespace objects".into()))?
        {
            let id = Object {
                catalog: e.catalog.clone(),
                id: e.id,
                attribute: 0,
            };
            match e.catalog.as_str() {
                "pg_class" => {
                    found.relations.insert(e.name, id);
                }
                "pg_type" => {
                    found.types.insert(e.name, id);
                }
                "pg_constraint" => {
                    found.constraints.entry(e.name).or_default().insert(id);
                }
                _ => return Err(DbError::Refused("unsupported namespace catalog".into())),
            }
        }
        out.insert(schema.clone(), found);
    }
    Ok(Fact::Present(out))
}

pub struct Allocation<'a> {
    root: &'a RebuildPartitionedTable,
    catalog: &'a Catalog,
    naming: &'a Naming,
    slots: &'a Slots,
    identities: &'a IdsFile,
    released: &'a BTreeSet<ObjectKey>,
    reserved: BTreeMap<String, BTreeSet<String>>,
}
impl<'a> Allocation<'a> {
    pub fn new(
        root: &'a RebuildPartitionedTable,
        catalog: &'a Catalog,
        naming: &'a Naming,
        slots: &'a Slots,
        identities: &'a IdsFile,
        released: &'a BTreeSet<ObjectKey>,
    ) -> Self {
        Self {
            root,
            catalog,
            naming,
            slots,
            identities,
            released,
            reserved: BTreeMap::new(),
        }
    }
    fn table(&self, key: &ObjectKey) -> Option<&TableName> {
        let ObjectKey::Table(uid) = key else {
            return None;
        };
        self.identities
            .tables
            .get(uid.uid())
            .or_else(|| self.root.intent.source.tables.get(uid.uid()))
    }

    fn released(
        &self,
        id: &Object,
        schema: &str,
        name: &ObjectLocalName,
        constraint: bool,
    ) -> Result<bool, String> {
        let spellings = self
            .catalog
            .spellings
            .require("index and constraint spellings")?;
        let Some(s) = spellings.get(id) else {
            if !constraint && let Some(relation) = self.catalog.relations.get(id) {
                if relation.name.schema != schema || relation.name.name != name.raw() {
                    return Ok(false);
                }
                let uid = self
                    .root
                    .intent
                    .source
                    .as_ids()
                    .table_uid(&relation.name)
                    .cloned()
                    .or_else(|| self.identities.table_uid(&relation.name).cloned());
                return Ok(self.slots.effects.iter().flat_map(|fx| &fx.releases).any(
                    |(slot, holder)| {
                        matches!(holder,ObjectKey::Table(t) if Some(t.uid())==uid.as_ref())
                            && self.released.contains(holder)
                            && slot.namespace == Namespace::Relations(schema.into())
                            && slot.name == name.raw()
                    },
                ));
            }
            return Ok(false);
        };
        if s.table.schema != schema
            || s.name != *name
            || s.kind
                != if constraint {
                    NameKind::Constraint
                } else {
                    NameKind::Index
                }
        {
            return Ok(false);
        }
        Ok(self.slots.catalog_owned.iter().any(|(slot, holder)| {
            let same_domain = if constraint {
                matches!(&slot.namespace, Namespace::Table(key,TableSpace::Constraints)
                    if self.table(key)==Some(&s.table))
            } else {
                slot.namespace == Namespace::Relations(schema.into())
            };
            same_domain
                && slot.name == name.raw()
                && self.released.contains(holder)
                && self
                    .slots
                    .effects
                    .iter()
                    .any(|fx| fx.releases.contains(&(slot.clone(), holder.clone())))
        }))
    }
    /// Names use final leaf spellings. The factory later binds temporary addresses
    /// to these reserved UIDs; allocation itself never claims execution authority.
    pub fn clones(
        &mut self,
        part: &IndexPart,
        definition: &PartitionIndexDefinition,
    ) -> Result<Vec<PartitionIndexClone>, String> {
        let label = format!("{} {part:?}", self.root.table.shown());
        let attributes = self.attributes(part, definition)?;
        let domains = self.naming.require(&label)?;
        let replacements = self.root.intent.replacement.as_ids();
        let replacement_index = replacements.index();
        let mut out = Vec::new();
        let mut allocated = self.reserved.clone();
        for (leaf, shape) in &self.root.after {
            if shape.partition_of.is_none() {
                continue;
            }
            let domain = domains
                .get(&leaf.schema)
                .ok_or_else(|| format!("{}: namespace occupancy is absent", leaf.shown()))?;
            let mut unavailable = BTreeSet::new();
            for (name, id) in &domain.relations {
                if !self.released(id, &leaf.schema, name, false)? {
                    unavailable.insert(name.raw().to_owned());
                }
            }
            // ChooseRelationName checks every constraint in the schema, not only
            // constraints on this leaf. Domain constraints compete here too.
            for (name, ids) in &domain.constraints {
                let mut free = true;
                for id in ids {
                    free &= self.released(id, &leaf.schema, name, true)?;
                }
                if !free {
                    unavailable.insert(name.raw().to_owned());
                }
            }
            for (slot, _) in self.slots.effects.iter().flat_map(|fx| &fx.claims) {
                let schema = match &slot.namespace {
                    Namespace::Relations(s) => Some(s),
                    Namespace::Table(uid, TableSpace::Constraints) => Some(
                        &self
                            .table(uid)
                            .ok_or_else(|| format!("{label}: claim owner has no namespace"))?
                            .schema,
                    ),
                    Namespace::Types(_)
                    | Namespace::Table(_, _)
                    | Namespace::Routines(_)
                    | Namespace::Roles => None,
                };
                if schema == Some(&leaf.schema) {
                    unavailable.insert(slot.name.clone());
                }
            }
            // Reserve declared own parts even before their operations exist.
            for (table, shape) in &self.root.after {
                if table.schema == leaf.schema {
                    unavailable.extend(
                        shape
                            .indexes
                            .keys()
                            .chain(shape.unique.keys())
                            .chain(shape.checks.keys())
                            .map(|n| n.raw().to_owned()),
                    );
                    unavailable.extend(
                        shape
                            .primary_key
                            .iter()
                            .filter_map(|k| k.name.as_ref())
                            .map(|n| n.raw().to_owned()),
                    );
                }
            }
            let reserved = allocated.entry(leaf.schema.clone()).or_default();
            unavailable.extend(reserved.iter().cloned());
            let name = clone_name(leaf, part, &Fact::Present(attributes.clone()), &unavailable)?;
            reserved.insert(name.raw().to_owned());
            let uid = replacement_index
                .table_uid(leaf)
                .cloned()
                .ok_or_else(|| format!("{}: no reserved replacement identity", leaf.shown()))?;
            out.push(PartitionIndexClone {
                leaf: RebuildRelation {
                    uid,
                    name: leaf.clone(),
                },
                name,
            });
        }
        self.reserved = allocated;
        Ok(out)
    }
    fn attributes(
        &self,
        part: &IndexPart,
        definition: &PartitionIndexDefinition,
    ) -> Result<Vec<ColumnName>, String> {
        let keys = match (part, definition) {
            (IndexPart::PrimaryKey, PartitionIndexDefinition::PrimaryKey(k)) => k.columns.clone(),
            (IndexPart::Unique(_), PartitionIndexDefinition::Unique(k)) => k.columns.clone(),
            (IndexPart::Index(name), PartitionIndexDefinition::Index(index)) => {
                if index.holds_expression() {
                    let label = format!("{} index {}", self.root.table.shown(), name.shown());
                    if self.root.before[&self.root.table].indexes.get(name) != Some(index) {
                        return Err(format!(
                            "{label}: new or changed expression labels require engine evidence; rebuild is refused"
                        ));
                    }
                    return self
                        .catalog
                        .spellings
                        .require(&label)?
                        .values()
                        .find(|s| {
                            s.table == self.root.table
                                && s.name == *name
                                && s.kind == NameKind::Index
                        })
                        .and_then(|s| s.index_attributes.clone())
                        .ok_or_else(|| format!("{label}: index attribute labels are absent"));
                }
                index
                    .column_keys()
                    .unwrap()
                    .into_iter()
                    .chain(index.include.iter().cloned())
                    .collect()
            }
            (IndexPart::PrimaryKey | IndexPart::Unique(_) | IndexPart::Index(_), _) => {
                return Err(format!(
                    "{}: partition index kind disagrees",
                    self.root.table.shown()
                ));
            }
        };
        let mut names = Vec::<ColumnName>::new();
        for key in keys {
            let mut name = key.clone();
            let mut suffix = 0;
            while names.contains(&name) {
                suffix += 1;
                let digits = suffix.to_string();
                name = format!(
                    "{}{}",
                    super::prefix(key.raw(), (63 - digits.len()).min(key.raw().len())),
                    digits
                )
                .into();
            }
            names.push(name);
        }
        Ok(names)
    }
}
