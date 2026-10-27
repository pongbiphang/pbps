//! Declaration identities are made before selection, under the producer's
//! fixed environment key (DEC-2375.1). Source never inhabits a public address.

use super::{
    ObjectIdentity, Uncovered,
    read::{Catalog, Object},
};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use std::collections::BTreeMap;

// Deliberately neither Debug nor Serialize: even a duplicate refusal must not
// reveal a confidential literal from an external CHECK.
#[derive(PartialEq, Eq)]
struct Declaration {
    kind: &'static str,
    table: ObjectIdentity,
    parts: Vec<String>,
}

#[derive(Clone)]
pub(super) enum Producer {
    Environment(EnvironmentFingerprintKey),
    // Pure catalog unit tests on platforms that cannot qualify key files
    // never persist evidence. This variant does not exist in production.
    #[cfg(all(test, not(unix)))]
    Process,
}

impl Producer {
    pub(super) fn new(key: &EnvironmentFingerprintKey) -> Self {
        Self::Environment(key.clone())
    }
    pub(super) fn key(&self) -> &EnvironmentFingerprintKey {
        match self {
            Self::Environment(key) => key,
            #[cfg(all(test, not(unix)))]
            Self::Process => panic!("a pure catalog fixture cannot persist environment evidence"),
        }
    }
    fn identity(&self, source: &Declaration) -> ObjectIdentity {
        let mut bytes = Vec::new();
        for part in source.table.name.iter().chain(&source.parts) {
            bytes.extend_from_slice(&(part.len() as u64).to_be_bytes());
            bytes.extend_from_slice(part.as_bytes());
        }
        let token = match self {
            Self::Environment(key) => {
                key.fingerprint("pbps/mssql-constraint-identity/v1", source.kind, &bytes)
            }
            #[cfg(all(test, not(unix)))]
            Self::Process => pbps_db::fingerprint::FingerprintKey::process().fingerprint(
                "pbps/mssql-constraint-identity/v1",
                source.kind,
                &bytes,
            ),
        };
        ObjectIdentity {
            class: format!("pbps-unnamed-{}", source.kind),
            name: vec![token.iter().map(|b| format!("{b:02x}")).collect()],
            signature: vec![source.table.clone()],
        }
    }
}

pub(super) struct Identities {
    declarations: BTreeMap<i64, Declaration>,
    public: BTreeMap<i64, ObjectIdentity>,
}

fn number(row: &super::read::WholeRow, name: &str) -> Option<i64> {
    row.get(name)?.as_ref()?.parse().ok()
}

impl Identities {
    pub(super) fn new(catalog: &Catalog, producer: &Producer) -> Result<Self, Uncovered> {
        let mut declarations = BTreeMap::new();
        let mut public = BTreeMap::new();
        for (view, kind) in [
            ("key_constraints", "KEY"),
            ("check_constraints", "CHECK"),
            ("default_constraints", "DEFAULT"),
            ("foreign_keys", "FOREIGN KEY"),
        ] {
            for row in catalog.whole.get(view).into_iter().flatten() {
                if row.get("is_system_named").and_then(Option::as_deref) != Some("1") {
                    continue;
                }
                let object = number(row, "object_id")
                    .and_then(|id| catalog.objects.iter().find(|o| o.id == id));
                // Table-type implementation objects belong to the type's
                // private structure and are not independent catalog roots.
                let Some(object) = object else {
                    continue;
                };
                let table = object
                    .parent
                    .and_then(|id| catalog.objects.iter().find(|o| o.id == id))
                    .ok_or_else(|| {
                        Uncovered::class(
                            "sys.objects",
                            "an unnamed constraint's table is unreadable",
                        )
                    })?;
                let schema = catalog
                    .schemas
                    .iter()
                    .find(|s| s.id == table.schema)
                    .ok_or_else(|| {
                        Uncovered::class(
                            "sys.schemas",
                            "an unnamed constraint's schema is unreadable",
                        )
                    })?;
                let table_id = super::object(&schema.name, &table.name);
                let missing = || {
                    Uncovered::object(
                        &table_id,
                        "an unnamed constraint's declaration is unreadable",
                    )
                };
                let mut parts = Vec::new();
                let kind = match (kind, object.kind.as_str()) {
                    ("KEY", "PK") => "PRIMARY KEY",
                    ("KEY", "UQ") => {
                        let index = number(row, "unique_index_id").ok_or_else(missing)?;
                        let mut columns: Vec<_> = catalog
                            .whole
                            .get("index_columns")
                            .into_iter()
                            .flatten()
                            .filter(|c| {
                                number(c, "object_id") == Some(table.id)
                                    && number(c, "index_id") == Some(index)
                            })
                            .collect();
                        columns.sort_by_key(|c| {
                            (number(c, "key_ordinal"), number(c, "index_column_id"))
                        });
                        if columns.is_empty() {
                            return Err(missing());
                        }
                        for column in columns {
                            let id = number(column, "column_id").ok_or_else(missing)?;
                            let name = catalog
                                .columns
                                .iter()
                                .find(|c| c.object == table.id && c.id == id)
                                .ok_or_else(missing)?;
                            parts.push(name.name.clone());
                            for flag in ["key_ordinal", "is_descending_key", "is_included_column"] {
                                parts.push(
                                    column
                                        .get(flag)
                                        .and_then(Clone::clone)
                                        .ok_or_else(missing)?,
                                );
                            }
                        }
                        "UNIQUE"
                    }
                    ("CHECK", _) => {
                        parts.push(
                            catalog
                                .constraints
                                .get(&object.id)
                                .ok_or_else(missing)?
                                .definition
                                .clone(),
                        );
                        "CHECK"
                    }
                    ("DEFAULT", _) => {
                        let column = catalog
                            .constraints
                            .get(&object.id)
                            .ok_or_else(missing)?
                            .column;
                        parts.push(
                            catalog
                                .columns
                                .iter()
                                .find(|c| c.object == table.id && c.id == column)
                                .ok_or_else(missing)?
                                .name
                                .clone(),
                        );
                        "DEFAULT"
                    }
                    ("FOREIGN KEY", _) => {
                        let foreign = catalog.foreign_keys.get(&object.id).ok_or_else(missing)?;
                        let target = catalog
                            .objects
                            .iter()
                            .find(|o| o.id == foreign.referenced)
                            .ok_or_else(missing)?;
                        parts.push(
                            catalog
                                .schemas
                                .iter()
                                .find(|s| s.id == target.schema)
                                .ok_or_else(missing)?
                                .name
                                .clone(),
                        );
                        parts.push(target.name.clone());
                        let mut columns = foreign.columns.clone();
                        columns.sort();
                        for (_, column, target) in columns {
                            parts.extend([column, target]);
                        }
                        parts.extend([foreign.on_delete.clone(), foreign.on_update.clone()]);
                        "FOREIGN KEY"
                    }
                    _ => return Err(missing()),
                };
                let source = Declaration {
                    kind,
                    table: table_id,
                    parts,
                };
                public.insert(object.id, producer.identity(&source));
                declarations.insert(object.id, source);
            }
        }
        Ok(Self {
            declarations,
            public,
        })
    }

    pub(super) fn object(&self, object: &Object) -> Result<Option<ObjectIdentity>, Uncovered> {
        let Some(source) = self.declarations.get(&object.id) else {
            return Ok(None);
        };
        let id = &self.public[&object.id];
        if self
            .declarations
            .iter()
            .any(|(other, declaration)| *other != object.id && declaration == source)
        {
            return Err(Uncovered::object(
                id,
                "two identical unnamed declarations on one table cannot be distinguished",
            ));
        }
        Ok(Some(id.clone()))
    }
}

pub(super) fn scope(
    catalog: &Catalog,
    requested: &super::CaptureScope,
    producer: &Producer,
) -> Result<super::CaptureScope, Uncovered> {
    let identities = Identities::new(catalog, producer)?;
    let mut scope = requested.clone();
    scope.retained.clear();
    for root in &requested.retained {
        let mut found = false;
        if root.class == super::class::OBJECT
            && root.signature.is_empty()
            && let [schema, name] = root.name.as_slice()
        {
            for id in catalog
                .objects_named
                .get(&(schema.clone(), name.clone()))
                .into_iter()
                .flatten()
            {
                if let Some(object) = catalog.objects.iter().find(|o| o.id == *id)
                    && let Some(canonical) = identities.object(object)?
                {
                    scope.retained.insert(canonical);
                    found = true;
                }
            }
        }
        if !found {
            scope.retained.insert(root.clone());
        }
    }
    Ok(scope)
}
