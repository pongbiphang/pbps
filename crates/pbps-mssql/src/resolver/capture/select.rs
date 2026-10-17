//! The scope's closure, each object's properties and bindings, and the
//! candidates of every name the scope asks about.
//!
//! A dependency row is one of four things (`docs/RESOLVER-MSSQL.md` §1, §2;
//! measured on #2141):
//!
//! - **a name in another database or server**, which refuses: the capture
//!   reads one database;
//! - **a one-part `EXEC`** (`is_caller_dependent`), resolved when the module
//!   runs in `sys`, then the executing principal's default schema, then
//!   `dbo`. A call to a system procedure leaves no row at all;
//! - **a one-part name in a body that is not schema-bound**, resolved when
//!   the module runs in its own schema, then `dbo`. The row's id is what it
//!   resolves to *now*, which is no binding: the candidates are;
//! - **an id-bound reference**: schema-bound, or a two-part name. That one is
//!   a binding. With no id, the name does not exist yet (deferred name
//!   resolution), and nothing can say what it will bind, so it refuses.

use super::read::{Catalog, Lookups, Object};
use super::{CandidateClass, CandidateSet, CaptureScope, ObjectIdentity, Uncovered, class};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// One id-bound reference: where it sits in its object, and what it names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub(super) struct Binding {
    pub node: String,
    pub path: Vec<String>,
    pub target: ObjectIdentity,
}

/// A name a module resolves only when it runs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub(super) enum RuntimeName {
    /// An object reference: the module's schema, then `dbo`.
    Object(String),
    /// A procedure call: `sys`, then the executing principal's default
    /// schema, then `dbo`.
    Call(String),
    /// A type, such as a local variable's: the module's schema, then `dbo`.
    Type(String),
}

#[derive(Clone)]
pub(super) struct Input {
    pub properties: BTreeMap<String, Value>,
    pub bindings: BTreeSet<Binding>,
}

pub(super) struct Selected {
    pub inputs: BTreeMap<ObjectIdentity, Input>,
    pub candidates: BTreeMap<CandidateSet, BTreeSet<ObjectIdentity>>,
    /// Modules with a runtime-bound name, and the names.
    pub runtime: BTreeMap<ObjectIdentity, BTreeSet<RuntimeName>>,
}

/// The names `scope` asks the engine to match.
pub(super) fn lookups(scope: &CaptureScope) -> Lookups {
    let mut lookups = Lookups::default();
    for object in &scope.retained {
        if let [schema, name] = object.name.as_slice() {
            match object.class.as_str() {
                class::OBJECT => {
                    lookups.objects.insert((schema.clone(), name.clone()));
                }
                class::TYPE => {
                    lookups.types.insert((schema.clone(), name.clone()));
                }
                _ => {}
            }
        }
    }
    for candidate in &scope.candidates {
        match candidate.class {
            CandidateClass::Object => {
                lookups
                    .objects
                    .insert((candidate.schema.clone(), candidate.name.clone()));
            }
            CandidateClass::SystemProcedure => {
                lookups.system.insert(candidate.name.clone());
            }
            CandidateClass::Type => {
                lookups
                    .types
                    .insert((candidate.schema.clone(), candidate.name.clone()));
            }
        }
    }
    lookups
}

fn identity(class: &str, name: Vec<String>, signature: Vec<ObjectIdentity>) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name,
        signature,
    }
}

struct Index<'a> {
    catalog: &'a Catalog,
    schemas: BTreeMap<i64, &'a str>,
    principals: BTreeMap<i64, &'a str>,
    objects: BTreeMap<i64, &'a Object>,
    types: BTreeMap<i64, &'a super::read::Type>,
}

impl<'a> Index<'a> {
    fn new(catalog: &'a Catalog) -> Self {
        Self {
            catalog,
            schemas: catalog
                .schemas
                .iter()
                .map(|s| (s.id, s.name.as_str()))
                .collect(),
            principals: catalog
                .principals
                .iter()
                .map(|p| (p.id, p.name.as_str()))
                .collect(),
            objects: catalog.objects.iter().map(|o| (o.id, o)).collect(),
            types: catalog.types.iter().map(|t| (t.id, t)).collect(),
        }
    }

    fn schema(&self, id: i64) -> Result<&'a str, Uncovered> {
        self.schemas.get(&id).copied().ok_or_else(|| {
            Uncovered::class(class::SCHEMA, "an object's schema is not in the catalog")
        })
    }

    fn principal(&self, id: i64) -> Result<ObjectIdentity, Uncovered> {
        let name = self
            .principals
            .get(&id)
            .ok_or_else(|| Uncovered::class(class::PRINCIPAL, "an owner is not in the catalog"))?;
        Ok(identity(class::PRINCIPAL, vec![(*name).into()], Vec::new()))
    }

    fn object(&self, object: &Object) -> Result<ObjectIdentity, Uncovered> {
        Ok(identity(
            class::OBJECT,
            vec![self.schema(object.schema)?.into(), object.name.clone()],
            Vec::new(),
        ))
    }

    fn type_identity(&self, id: i64) -> Result<ObjectIdentity, Uncovered> {
        let found = self
            .types
            .get(&id)
            .ok_or_else(|| Uncovered::class(class::TYPE, "a type is not in the catalog"))?;
        Ok(identity(
            class::TYPE,
            vec![self.schema(found.schema)?.into(), found.name.clone()],
            Vec::new(),
        ))
    }

    fn column(&self, object: &Object, column: i64) -> Result<ObjectIdentity, Uncovered> {
        let parent = self.object(object)?;
        let found = self
            .catalog
            .columns
            .iter()
            .find(|c| c.object == object.id && c.id == column)
            .ok_or_else(|| {
                Uncovered::object(&parent, "a referenced column is not in the catalog")
            })?;
        Ok(identity(
            class::COLUMN,
            vec![found.name.clone()],
            vec![parent],
        ))
    }

    /// Who owns an object: its own principal, else its parent's owner for a
    /// constraint or a trigger, else its schema's owner.
    fn owner(&self, object: &Object) -> Result<ObjectIdentity, Uncovered> {
        if let Some(owner) = object.owner {
            return self.principal(owner);
        }
        if let Some(parent) = object.parent.and_then(|id| self.objects.get(&id)) {
            return self.owner(parent);
        }
        let schema = self
            .catalog
            .schemas
            .iter()
            .find(|s| s.id == object.schema)
            .ok_or_else(|| {
                Uncovered::class(class::SCHEMA, "an object's schema is not in the catalog")
            })?;
        self.principal(schema.owner)
    }
}

struct Builder<'a> {
    index: Index<'a>,
    pending: VecDeque<i64>,
    seen: BTreeSet<i64>,
    types: BTreeSet<i64>,
    /// The engine's own objects something names: in scope by identity only,
    /// since nothing a plan does can change them.
    engine: BTreeSet<ObjectIdentity>,
    result: Selected,
}

impl Builder<'_> {
    fn enqueue(&mut self, id: i64) {
        if self.seen.insert(id) {
            self.pending.push_back(id);
        }
    }

    fn insert(&mut self, object: ObjectIdentity, input: Input) -> Result<(), Uncovered> {
        if self.result.inputs.insert(object.clone(), input).is_some() {
            return Err(Uncovered::object(
                &object,
                "two catalog rows share one identity",
            ));
        }
        Ok(())
    }

    fn type_ref(&mut self, id: i64) -> Result<ObjectIdentity, Uncovered> {
        self.types.insert(id);
        self.index.type_identity(id)
    }

    fn object(&mut self, id: i64) -> Result<(), Uncovered> {
        let catalog = self.index.catalog;
        let object = *self.index.objects.get(&id).ok_or_else(|| {
            Uncovered::class(class::OBJECT, "a selected object is not in the catalog")
        })?;
        let me = self.index.object(object)?;
        let mut properties = BTreeMap::new();
        properties.insert("type".into(), json!(object.kind));
        properties.insert("owner".into(), json!(self.index.owner(object)?));
        let parent = match object.parent {
            Some(parent) => {
                let found = *self.index.objects.get(&parent).ok_or_else(|| {
                    Uncovered::object(&me, "an object's parent is not in the catalog")
                })?;
                self.enqueue(parent);
                Some(self.index.object(found)?)
            }
            None => None,
        };
        properties.insert("parent".into(), json!(parent));
        if let Some(module) = catalog.modules.get(&id) {
            // `WITH ENCRYPTION` hides the text from every reader, `sa`
            // included: an unreadable definition is not an empty one.
            if module.definition.is_none() {
                return Err(Uncovered::object(
                    &me,
                    "an encrypted module's definition cannot be read",
                ));
            }
            let execute_as = match module.execute_as {
                None => json!("caller"),
                Some(-2) => json!("owner"),
                Some(principal) => json!(self.index.principal(principal)?),
            };
            properties.insert("definition".into(), json!(module.definition));
            properties.insert("uses_ansi_nulls".into(), json!(module.ansi_nulls));
            properties.insert(
                "uses_quoted_identifier".into(),
                json!(module.quoted_identifier),
            );
            properties.insert("is_schema_bound".into(), json!(module.schema_bound));
            properties.insert("execute_as".into(), execute_as);
        }
        if let Some(base) = catalog.synonyms.get(&id) {
            // A synonym binds its name when it is created and its target only
            // when it runs (§1): the target is a property, not a binding.
            properties.insert("base_object_name".into(), json!(base));
        }
        if let Some(constraint) = catalog.constraints.get(&id) {
            let column = match (constraint.column, object.parent) {
                (0, _) => Value::Null,
                (column, Some(parent)) => {
                    let parent = *self.index.objects.get(&parent).ok_or_else(|| {
                        Uncovered::object(&me, "a constraint's table is not in the catalog")
                    })?;
                    json!(self.index.column(parent, column)?)
                }
                (_, None) => {
                    return Err(Uncovered::object(&me, "a column constraint has no table"));
                }
            };
            properties.insert("definition".into(), json!(constraint.definition));
            properties.insert("disabled".into(), json!(constraint.disabled));
            properties.insert("trusted".into(), json!(constraint.trusted));
            properties.insert("column".into(), column);
        }
        let mut bindings = BTreeSet::new();
        if let Some((disabled, instead_of)) = catalog.triggers.get(&id) {
            properties.insert("disabled".into(), json!(disabled));
            properties.insert("instead_of".into(), json!(instead_of));
        }
        if let Some(key) = catalog.keys.get(&id) {
            let mut columns = key.columns.clone();
            columns.sort();
            let columns: Vec<_> = columns
                .into_iter()
                .map(|(_, column, descending)| json!({"column": column, "descending": descending}))
                .collect();
            properties.insert(
                "key".into(),
                json!({"index": key.index, "disabled": key.disabled, "columns": columns}),
            );
        }
        if let Some(indexes) = catalog.unique_indexes.get(&id) {
            let indexes: BTreeMap<_, _> = indexes
                .iter()
                .map(|(name, index)| {
                    let mut columns = index.columns.clone();
                    columns.sort();
                    let columns: Vec<_> = columns
                        .into_iter()
                        .map(|(_, column, descending)| {
                            json!({"column": column, "descending": descending})
                        })
                        .collect();
                    (
                        name.clone(),
                        json!({"index": index.index, "disabled": index.disabled, "columns": columns}),
                    )
                })
                .collect();
            properties.insert("unique_indexes".into(), json!(indexes));
        }
        if let Some(foreign) = catalog.foreign_keys.get(&id) {
            // A foreign key binds its referenced table's key by index id: the
            // table and the key are bindings, and are read too.
            let table = *self.index.objects.get(&foreign.referenced).ok_or_else(|| {
                Uncovered::object(
                    &me,
                    "a foreign key's referenced table is not in the catalog",
                )
            })?;
            self.enqueue(foreign.referenced);
            let referenced = self.index.object(table)?;
            bindings.insert(Binding {
                node: "sys.foreign_keys".into(),
                path: vec!["table".into()],
                target: referenced.clone(),
            });
            let key = match foreign.key {
                Some(key) => {
                    let found = *self.index.objects.get(&key).ok_or_else(|| {
                        Uncovered::object(
                            &me,
                            "a foreign key's referenced key is not in the catalog",
                        )
                    })?;
                    self.enqueue(key);
                    let key = self.index.object(found)?;
                    bindings.insert(Binding {
                        node: "sys.foreign_keys".into(),
                        path: vec!["key".into()],
                        target: key.clone(),
                    });
                    json!(key)
                }
                None => json!({"unique_index": foreign.index}),
            };
            let mut columns = foreign.columns.clone();
            columns.sort();
            let columns: Vec<_> = columns
                .into_iter()
                .map(|(_, column, target)| json!([column, target]))
                .collect();
            properties.insert(
                "foreign_key".into(),
                json!({
                    "references": referenced,
                    "key": key,
                    "columns": columns,
                    "on_delete": foreign.on_delete,
                    "on_update": foreign.on_update,
                    "disabled": foreign.disabled,
                    "trusted": foreign.trusted,
                }),
            );
        }
        let mut parameters = Vec::new();
        let mut owned_parameters: Vec<_> = catalog
            .parameters
            .iter()
            .filter(|p| p.object == id)
            .collect();
        owned_parameters.sort_by_key(|p| p.id);
        for parameter in owned_parameters {
            let target = self.type_ref(parameter.user_type)?;
            // Parameter 0 is a scalar function's return.
            let slot = if parameter.id == 0 {
                "return".to_owned()
            } else {
                parameter.name.clone()
            };
            parameters.push(json!({
                "name": slot,
                "type": target,
                "max_length": parameter.max_length,
                "precision": parameter.precision,
                "scale": parameter.scale,
                "output": parameter.output,
                "has_default": parameter.has_default,
                "readonly": parameter.readonly,
            }));
            bindings.insert(Binding {
                node: "sys.parameters".into(),
                path: vec![slot],
                target,
            });
        }
        if !parameters.is_empty() {
            properties.insert("parameters".into(), Value::Array(parameters));
        }
        let mut column_bindings: BTreeMap<i64, BTreeSet<Binding>> = BTreeMap::new();
        for column in catalog.columns.iter().filter(|c| c.object == id) {
            let target = self.type_ref(column.user_type)?;
            column_bindings
                .entry(column.id)
                .or_default()
                .insert(Binding {
                    node: "sys.columns".into(),
                    path: Vec::new(),
                    target,
                });
        }
        for dependency in catalog.dependencies.iter().filter(|d| d.referencing == id) {
            if dependency.server.is_some() || dependency.database.is_some() {
                return Err(Uncovered::object(
                    &me,
                    "a reference names an object in another database or server",
                ));
            }
            if dependency.caller_dependent {
                self.result
                    .runtime
                    .entry(me.clone())
                    .or_default()
                    .insert(RuntimeName::Call(dependency.entity.clone()));
                continue;
            }
            if dependency.schema.is_none() && !dependency.schema_bound {
                // Its id is what the name resolves to now, by class.
                let name = match dependency.class {
                    1 => RuntimeName::Object(dependency.entity.clone()),
                    6 => RuntimeName::Type(dependency.entity.clone()),
                    _ => {
                        return Err(Uncovered::object(
                            &me,
                            "a reference is of a class this capture does not read",
                        ));
                    }
                };
                self.result
                    .runtime
                    .entry(me.clone())
                    .or_default()
                    .insert(name);
                continue;
            }
            let Some(referenced) = dependency.referenced else {
                return Err(Uncovered::object(
                    &me,
                    "a reference names an object that does not exist yet (deferred name resolution)",
                ));
            };
            let target = match dependency.class {
                1 => match self.index.objects.get(&referenced).copied() {
                    Some(found) => {
                        self.enqueue(referenced);
                        if dependency.referenced_column > 0 {
                            self.index.column(found, dependency.referenced_column)?
                        } else {
                            self.index.object(found)?
                        }
                    }
                    // The engine's own object: named as spelled, read no further.
                    None => match &dependency.schema {
                        Some(schema) => {
                            let engine = identity(
                                class::OBJECT,
                                vec![schema.clone(), dependency.entity.clone()],
                                Vec::new(),
                            );
                            self.engine.insert(engine.clone());
                            engine
                        }
                        None => {
                            return Err(Uncovered::object(
                                &me,
                                "a reference names an object the capture cannot identify",
                            ));
                        }
                    },
                },
                6 => self.type_ref(referenced)?,
                _ => {
                    return Err(Uncovered::object(
                        &me,
                        "a reference is of a class this capture does not read",
                    ));
                }
            };
            let binding = Binding {
                node: "sys.sql_expression_dependencies".into(),
                path: vec![
                    dependency.schema.clone().unwrap_or_default(),
                    dependency.entity.clone(),
                ],
                target,
            };
            if dependency.referencing_column > 0 {
                column_bindings
                    .entry(dependency.referencing_column)
                    .or_default()
                    .insert(binding);
            } else {
                bindings.insert(binding);
            }
        }
        for column in catalog.columns.iter().filter(|c| c.object == id) {
            let column_id = identity(class::COLUMN, vec![column.name.clone()], vec![me.clone()]);
            let target = self.index.type_identity(column.user_type)?;
            let properties = BTreeMap::from([
                ("column_id".to_owned(), json!(column.id)),
                ("type".to_owned(), json!(target)),
                ("max_length".to_owned(), json!(column.max_length)),
                ("precision".to_owned(), json!(column.precision)),
                ("scale".to_owned(), json!(column.scale)),
                ("collation".to_owned(), json!(column.collation)),
                ("nullable".to_owned(), json!(column.nullable)),
                ("identity".to_owned(), json!(column.identity)),
                ("computed".to_owned(), json!(column.computed)),
                ("persisted".to_owned(), json!(column.persisted)),
            ]);
            let bindings = column_bindings.remove(&column.id).unwrap_or_default();
            self.insert(
                column_id,
                Input {
                    properties,
                    bindings,
                },
            )?;
        }
        if !column_bindings.is_empty() {
            return Err(Uncovered::object(
                &me,
                "a reference sits on a column that is not in the catalog",
            ));
        }
        // A constraint or trigger is part of its table: its CHECK or DEFAULT
        // binds what the table's rows run.
        let children: Vec<i64> = catalog
            .objects
            .iter()
            .filter(|o| o.parent == Some(id))
            .map(|o| o.id)
            .collect();
        for child in children {
            self.enqueue(child);
        }
        self.insert(
            me,
            Input {
                properties,
                bindings,
            },
        )
    }

    fn finish_types(&mut self) -> Result<(), Uncovered> {
        let mut pending: VecDeque<i64> = self.types.iter().copied().collect();
        let mut done = BTreeSet::new();
        while let Some(id) = pending.pop_front() {
            if !done.insert(id) {
                continue;
            }
            let found = *self
                .index
                .types
                .get(&id)
                .ok_or_else(|| Uncovered::class(class::TYPE, "a type is not in the catalog"))?;
            let me = self.index.type_identity(id)?;
            // An alias type's base is the system type whose id is its
            // `system_type_id`.
            let base = if found.user_defined && !found.table_type {
                pending.push_back(found.base);
                Some(self.index.type_identity(found.base)?)
            } else {
                None
            };
            let mut bindings: BTreeSet<Binding> = base
                .iter()
                .map(|target| Binding {
                    node: "sys.types".into(),
                    path: Vec::new(),
                    target: target.clone(),
                })
                .collect();
            // A table type is its columns, CHECKs and keys: a procedure's
            // table-valued parameter binds that shape.
            let mut structure = None;
            if found.table_type {
                let table = self.index.catalog.table_types.get(&id).ok_or_else(|| {
                    Uncovered::object(&me, "a table type's columns are not in the catalog")
                })?;
                let mut columns = Vec::new();
                let mut ordered: Vec<_> = table.columns.iter().collect();
                ordered.sort_by_key(|c| c.id);
                for column in ordered {
                    pending.push_back(column.user_type);
                    let target = self.index.type_identity(column.user_type)?;
                    bindings.insert(Binding {
                        node: "sys.table_types".into(),
                        path: vec![column.name.clone()],
                        target: target.clone(),
                    });
                    columns.push(json!({
                        "name": column.name,
                        "column_id": column.id,
                        "type": target,
                        "max_length": column.max_length,
                        "precision": column.precision,
                        "scale": column.scale,
                        "collation": column.collation,
                        "nullable": column.nullable,
                        "identity": column.identity,
                        "computed": column.computed,
                        "default": column.default,
                    }));
                }
                let mut checks = table.checks.clone();
                checks.sort();
                let mut keys = table.keys.clone();
                keys.sort();
                structure = Some(json!({"columns": columns, "checks": checks, "keys": keys}));
            }
            let properties = BTreeMap::from([
                ("structure".to_owned(), json!(structure)),
                ("base".to_owned(), json!(base)),
                ("max_length".to_owned(), json!(found.max_length)),
                ("precision".to_owned(), json!(found.precision)),
                ("scale".to_owned(), json!(found.scale)),
                ("collation".to_owned(), json!(found.collation)),
                ("nullable".to_owned(), json!(found.nullable)),
                ("user_defined".to_owned(), json!(found.user_defined)),
                ("table_type".to_owned(), json!(found.table_type)),
            ]);
            self.insert(
                me,
                Input {
                    properties,
                    bindings,
                },
            )?;
        }
        Ok(())
    }
}

pub(super) fn select(catalog: &Catalog, scope: &CaptureScope) -> Result<Selected, Uncovered> {
    let mut builder = Builder {
        index: Index::new(catalog),
        pending: VecDeque::new(),
        seen: BTreeSet::new(),
        types: BTreeSet::new(),
        engine: BTreeSet::new(),
        result: Selected {
            inputs: BTreeMap::new(),
            candidates: BTreeMap::new(),
            runtime: BTreeMap::new(),
        },
    };
    // Schemas and principals are complete sets: a procedure call's candidates
    // are the principals' default schemas, and every owner is a principal.
    for schema in &catalog.schemas {
        let me = identity(class::SCHEMA, vec![schema.name.clone()], Vec::new());
        let owner = builder.index.principal(schema.owner)?;
        builder.insert(
            me,
            Input {
                properties: BTreeMap::from([("owner".to_owned(), json!(owner))]),
                bindings: BTreeSet::new(),
            },
        )?;
    }
    for principal in &catalog.principals {
        let me = identity(class::PRINCIPAL, vec![principal.name.clone()], Vec::new());
        let owner = principal
            .owner
            .map(|id| builder.index.principal(id))
            .transpose()?;
        builder.insert(
            me,
            Input {
                properties: BTreeMap::from([
                    ("type".to_owned(), json!(principal.kind)),
                    ("default_schema".to_owned(), json!(principal.default_schema)),
                    ("owner".to_owned(), json!(owner)),
                ]),
                bindings: BTreeSet::new(),
            },
        )?;
    }
    for object in &scope.retained {
        if let [schema, name] = object.name.as_slice() {
            let key = (schema.clone(), name.clone());
            match object.class.as_str() {
                class::OBJECT => {
                    for id in catalog.objects_named.get(&key).into_iter().flatten() {
                        builder.enqueue(*id);
                    }
                }
                class::TYPE => {
                    for id in catalog.types_named.get(&key).into_iter().flatten() {
                        builder.types.insert(*id);
                    }
                }
                _ => {}
            }
        }
    }
    for candidate in &scope.candidates {
        let members = match candidate.class {
            CandidateClass::Object => {
                let key = (candidate.schema.clone(), candidate.name.clone());
                let mut members = BTreeSet::new();
                for id in catalog.objects_named.get(&key).into_iter().flatten() {
                    builder.enqueue(*id);
                    let found = *builder.index.objects.get(id).ok_or_else(|| {
                        Uncovered::class(class::OBJECT, "a candidate is not in the catalog")
                    })?;
                    members.insert(builder.index.object(found)?);
                }
                members
            }
            CandidateClass::SystemProcedure => {
                let members: BTreeSet<_> = catalog
                    .system_named
                    .get(&candidate.name)
                    .into_iter()
                    .flatten()
                    .map(|name| {
                        identity(class::OBJECT, vec!["sys".into(), name.clone()], Vec::new())
                    })
                    .collect();
                builder.engine.extend(members.iter().cloned());
                members
            }
            CandidateClass::Type => {
                let key = (candidate.schema.clone(), candidate.name.clone());
                let mut members = BTreeSet::new();
                for id in catalog.types_named.get(&key).into_iter().flatten() {
                    builder.types.insert(*id);
                    members.insert(builder.index.type_identity(*id)?);
                }
                members
            }
        };
        builder.result.candidates.insert(candidate.clone(), members);
    }
    while let Some(id) = builder.pending.pop_front() {
        builder.object(id)?;
    }
    builder.finish_types()?;
    for engine in std::mem::take(&mut builder.engine) {
        builder
            .result
            .inputs
            .entry(engine)
            .or_insert_with(|| Input {
                properties: BTreeMap::from([("engine".to_owned(), json!(true))]),
                bindings: BTreeSet::new(),
            });
    }
    Ok(builder.result)
}

/// The candidate sets a captured module's runtime-bound names need (§2):
///
/// - an object reference: the name in the module's schema and in `dbo`;
/// - a procedure call: whether `sys` holds the name, and the name in `dbo`
///   and in the default schema of each principal the module can execute as.
///   A fixed `EXECUTE AS` has that one principal (the module's owner for
///   `OWNER`); any other module has every database principal, since anyone
///   may call it. The principals themselves, with their default schemas,
///   are in every capture.
///
/// Whether `sys` holds the name is the first set's membership, read with the
/// rest, so the other sets are asked for whatever it says: the assessment
/// reads them in the engine's order.
pub(super) fn runtime_candidates(
    inputs: &BTreeMap<ObjectIdentity, Input>,
    runtime: &BTreeMap<ObjectIdentity, BTreeSet<RuntimeName>>,
) -> BTreeSet<CandidateSet> {
    let set = |class, schema: &str, name: &str| CandidateSet {
        class,
        schema: schema.into(),
        name: name.into(),
    };
    let default_schema = |principal: &ObjectIdentity| {
        inputs
            .get(principal)
            .and_then(|input| input.properties.get("default_schema"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let mut every: BTreeSet<String> = inputs
        .keys()
        .filter(|id| id.class == class::PRINCIPAL)
        .filter_map(default_schema)
        .collect();
    every.insert("dbo".into());
    let mut candidates = BTreeSet::new();
    for (module, names) in runtime {
        let Some(own) = module.name.first() else {
            continue;
        };
        let properties = inputs.get(module).map(|input| &input.properties);
        for name in names {
            match name {
                RuntimeName::Type(name) => {
                    candidates.insert(set(CandidateClass::Type, own, name));
                    candidates.insert(set(CandidateClass::Type, "dbo", name));
                }
                RuntimeName::Object(name) => {
                    candidates.insert(set(CandidateClass::Object, own, name));
                    candidates.insert(set(CandidateClass::Object, "dbo", name));
                }
                RuntimeName::Call(name) => {
                    candidates.insert(set(CandidateClass::SystemProcedure, "sys", name));
                    let fixed = properties
                        .and_then(|p| p.get("execute_as"))
                        .and_then(|value| match value.as_str() {
                            Some("caller") => None,
                            Some("owner") => properties
                                .and_then(|p| p.get("owner"))
                                .and_then(|owner| serde_json::from_value(owner.clone()).ok()),
                            _ => serde_json::from_value::<ObjectIdentity>(value.clone()).ok(),
                        });
                    let schemas = match fixed {
                        Some(principal) => default_schema(&principal)
                            .into_iter()
                            .chain(["dbo".to_owned()])
                            .collect(),
                        None => every.clone(),
                    };
                    for schema in schemas {
                        candidates.insert(set(CandidateClass::Object, &schema, name));
                    }
                }
            }
        }
    }
    candidates
}
