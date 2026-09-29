//! UID-backed PostgreSQL catalog ownership within the producer's fresh read.
//!
//! A catalog dependency is evidence about an engine-owned child only after a
//! declared root has been identified. In particular, an automatic dependency
//! from an ordinary index to a table or column does not make that index part
//! of the declaration (DEC-1274.1).

use super::{CapturedInputs, DroppedSignature, Uncovered};
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_model::resolver::{ObjectOwnership, Surface};
use pbps_model::{IdsFile, ModuleId, ModuleKind, Schema, TableName};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Input supplied by the authorized fresh-read producer. The routine roots
/// come from its compiled routine identities and same-snapshot DROP lookups;
/// neither serialized manifests nor guessed signatures may supply them.
pub struct RecordedOwnership<'a> {
    pub schema: &'a Schema,
    pub ids: &'a IdsFile,
    pub routines: &'a BTreeMap<ModuleId, ObjectIdentity>,
    pub dropped: &'a BTreeMap<ModuleId, DroppedSignature>,
    /// Schemas the measured scope expressly includes. They have no table UID.
    pub namespaces: &'a BTreeSet<String>,
}

fn identity(class: &str, name: Vec<String>, signature: Vec<ObjectIdentity>) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name,
        signature,
    }
}

fn relation(name: &TableName) -> ObjectIdentity {
    identity(
        "pg_class",
        vec![name.schema.clone(), name.name.clone()],
        Vec::new(),
    )
}

fn property<'a>(
    capture: &'a CapturedInputs,
    object: &ObjectIdentity,
    key: &str,
) -> Result<Option<&'a str>, Uncovered> {
    capture
        .inputs
        .get(object)
        .map(|input| {
            input
                .properties
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| Uncovered::object(object, "declared catalog kind is unreadable"))
        })
        .transpose()
}

fn assign(
    owned: &mut BTreeMap<ObjectIdentity, Surface>,
    object: ObjectIdentity,
    surface: Surface,
) -> Result<(), Uncovered> {
    if let Some(previous) = owned.insert(object.clone(), surface.clone()) {
        if previous != surface {
            return Err(Uncovered::object(
                &object,
                "catalog record has conflicting declared owners",
            ));
        }
    }
    Ok(())
}

fn direct(
    capture: &CapturedInputs,
    owned: &mut BTreeMap<ObjectIdentity, Surface>,
    object: ObjectIdentity,
    kind_field: &str,
    accepted: &[&str],
    surface: Surface,
) -> Result<(), Uncovered> {
    if let Some(kind) = property(capture, &object, kind_field)? {
        if !accepted.contains(&kind) {
            return Err(Uncovered::object(
                &object,
                "declared name has a different catalog kind",
            ));
        }
        assign(owned, object, surface)?;
    }
    Ok(())
}

/// Classify exact roots before raw kind/provenance properties are erased by
/// sealing. Absence stays absence; a present root of the wrong kind refuses.
/// Missing or corrupt recorded UID authority also refuses, rather than
/// granting ownership to whatever currently holds the declared spelling.
pub(super) fn classify(
    capture: &CapturedInputs,
    recorded: &RecordedOwnership<'_>,
) -> Result<BTreeMap<ObjectIdentity, ObjectOwnership>, Uncovered> {
    recorded
        .ids
        .validate()
        .map_err(|_| Uncovered::class("recorded-uid", "recorded managed identities are invalid"))?;
    let mut owned = BTreeMap::new();
    for schema in recorded.namespaces {
        let namespace = identity("pg_namespace", vec![schema.clone()], Vec::new());
        if capture.inputs.contains_key(&namespace) {
            assign(&mut owned, namespace, Surface::Namespace(schema.clone()))?;
        }
    }
    for (name, table) in &recorded.schema.tables {
        if recorded.ids.table_uid(name).is_none() {
            return Err(Uncovered::class(
                "recorded-uid",
                "declared table lacks its recorded UID",
            ));
        }
        let table_id = relation(name);
        direct(
            capture,
            &mut owned,
            table_id.clone(),
            "relkind",
            &["r", "p"],
            Surface::Table(name.clone()),
        )?;
        for (column, declaration) in &table.columns {
            let reference = name.column(column);
            if recorded.ids.column_uid(&reference).is_none() {
                return Err(Uncovered::class(
                    "recorded-uid",
                    "declared column lacks its recorded UID",
                ));
            }
            // The captured pg_attribute row has a logical `column` address;
            // its raw catalog class is not the address used by pg_attrdef.
            let column_id = identity("column", vec![column.clone()], vec![table_id.clone()]);
            if capture.inputs.contains_key(&column_id) {
                if !owned.contains_key(&table_id) {
                    return Err(Uncovered::object(
                        &column_id,
                        "declared column has no qualified table root",
                    ));
                }
                assign(
                    &mut owned,
                    column_id.clone(),
                    Surface::Column(reference.clone()),
                )?;
            }
            let default_id = identity("pg_attrdef", Vec::new(), vec![column_id]);
            if capture.inputs.contains_key(&default_id) && declaration.default.is_some() {
                assign(&mut owned, default_id, Surface::Default(reference))?;
            }
        }
        for check in table.checks.keys() {
            let found = capture
                .inputs
                .keys()
                .filter(|object| {
                    object.class == "pg_constraint"
                        && object.name.len() == 1
                        && object.name[0] == *check
                        && object.signature.get(1) == Some(&table_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            for object in found {
                direct(
                    capture,
                    &mut owned,
                    object,
                    "contype",
                    &["c"],
                    Surface::Check {
                        table: name.clone(),
                        name: check.clone(),
                    },
                )?;
            }
        }
        for index in table.indexes.keys() {
            let object = identity(
                "pg_class",
                vec![name.schema.clone(), index.clone()],
                Vec::new(),
            );
            if capture.inputs.contains_key(&object) {
                let metadata = identity("pg_index", Vec::new(), vec![object.clone()]);
                let parent = capture
                    .inputs
                    .get(&metadata)
                    .and_then(|row| row.properties.get("indrelid"))
                    .and_then(|value| serde_json::from_value::<ObjectIdentity>(value.clone()).ok())
                    .ok_or_else(|| {
                        Uncovered::object(&object, "declared index parent is unreadable")
                    })?;
                if parent != table_id {
                    return Err(Uncovered::object(
                        &object,
                        "declared index belongs to a different table",
                    ));
                }
            }
            direct(
                capture,
                &mut owned,
                object,
                "relkind",
                &["i", "I"],
                Surface::Index {
                    table: name.clone(),
                    name: index.clone(),
                },
            )?;
        }
        // A key constraint is independently declared. Its automatic table
        // edge does not qualify any other constraint or ordinary index.
        for (constraint_name, kind) in table
            .unique
            .keys()
            .map(|name| (name.as_str(), "u"))
            .chain(table.foreign_keys.keys().map(|name| (name.as_str(), "f")))
            .chain(
                table
                    .primary_key
                    .as_ref()
                    .and_then(|key| key.name.as_deref())
                    .map(|name| (name, "p")),
            )
        {
            let found = capture
                .inputs
                .keys()
                .filter(|object| {
                    object.class == "pg_constraint"
                        && object.name.len() == 1
                        && object.name[0] == constraint_name
                        && object.signature.get(1) == Some(&table_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            for object in found {
                direct(
                    capture,
                    &mut owned,
                    object,
                    "contype",
                    &[kind],
                    Surface::Table(name.clone()),
                )?;
            }
        }
    }
    for (id, module) in &recorded.schema.modules {
        match (id, module.kind) {
            (ModuleId::Named(name), ModuleKind::View) => {
                direct(
                    capture,
                    &mut owned,
                    relation(name),
                    "relkind",
                    &["v"],
                    Surface::Module(id.clone()),
                )?;
            }
            (ModuleId::Routine(_), ModuleKind::Procedure | ModuleKind::Function) => {
                // A target DROP lookup is resolved in this snapshot. A
                // scratch-created identity is never authority for a target
                // overload with the same declared signature.
                let root = if let Some(signature) = recorded.dropped.get(id) {
                    capture.dropped().get(signature).and_then(Option::as_ref)
                } else {
                    recorded.routines.get(id)
                };
                if let Some(root) = root {
                    let expected = if module.kind == ModuleKind::Procedure {
                        "p"
                    } else {
                        "f"
                    };
                    direct(
                        capture,
                        &mut owned,
                        root.clone(),
                        "prokind",
                        &[expected],
                        Surface::Module(id.clone()),
                    )?;
                }
            }
            (ModuleId::Trigger { on, name }, ModuleKind::Trigger) => {
                let object = identity("pg_trigger", vec![name.clone()], vec![relation(on)]);
                if capture.inputs.contains_key(&object) {
                    assign(&mut owned, object, Surface::Module(id.clone()))?;
                }
            }
            _ => {
                return Err(Uncovered::class(
                    "managed-module",
                    "declared module identity and kind disagree",
                ));
            }
        }
    }

    // Only measured, kind-specific internal edges inherit root authority.
    // A normal reference never does, and automatic links need an independently
    // declared exact surface above. Repeated passes cover row type -> array.
    loop {
        let mut changed = false;
        for dependency in capture.inputs.keys().filter(|id| id.class == "pg_depend") {
            let ([kind], [made, maker]) =
                (dependency.name.as_slice(), dependency.signature.as_slice())
            else {
                continue;
            };
            if !matches!(kind.as_str(), "i" | "a")
                || owned.contains_key(made)
                || !capture.inputs.contains_key(made)
            {
                continue;
            }
            let Some(owner) = owned.get(maker).cloned() else {
                continue;
            };
            let allowed = if kind == "a" {
                // PostgreSQL 18's not-null constraint has an automatic edge
                // to the exact column. Other automatic edges, including
                // ordinary indexes and triggers, never convey ownership.
                made.class == "pg_constraint"
                    && maker.class == "column"
                    && matches!(owner, Surface::Column(_))
                    && property(capture, made, "contype")? == Some("n")
                    && made.signature.get(1) == maker.signature.first()
            } else {
                match (made.class.as_str(), maker.class.as_str(), &owner) {
                    (
                        "pg_type",
                        "pg_class" | "pg_type",
                        Surface::Table(_) | Surface::Module(ModuleId::Named(_)),
                    ) => true,
                    ("pg_rewrite", "pg_class", Surface::Module(ModuleId::Named(_))) => {
                        made.name.len() == 1
                            && made.name[0] == "_RETURN"
                            && made.signature.first() == Some(maker)
                    }
                    ("pg_class", "column", Surface::Column(_)) => {
                        property(capture, made, "relkind")? == Some("S")
                    }
                    ("pg_class", "pg_constraint", Surface::Table(_)) => {
                        matches!(property(capture, made, "relkind")?, Some("i" | "I"))
                    }
                    ("pg_attrdef", "column", Surface::Column(_)) => true,
                    _ => false,
                }
            };
            if allowed {
                assign(&mut owned, made.clone(), owner)?;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // Metadata rows describe their exact owned subject. A dependency row is
    // owned by its subject, not by the object it merely references.
    for object in capture.inputs.keys() {
        if owned.contains_key(object) {
            continue;
        }
        let subject = match object.class.as_str() {
            // Defaults and rewrite rules need their own independently proven
            // surface or measured internal edge above. Their containing
            // column/relation is insufficient authority.
            "pg_index" | "pg_sequence" | "pg_init_privs" | "pg_depend" | "pg_shdepend" => {
                object.signature.first()
            }
            _ => None,
        };
        if let Some(owner) = subject.and_then(|subject| owned.get(subject)).cloned() {
            assign(&mut owned, object.clone(), owner)?;
        }
    }
    Ok(capture
        .inputs
        .keys()
        .map(|object| {
            (
                object.clone(),
                owned
                    .get(object)
                    .cloned()
                    .map_or(ObjectOwnership::Unqualified, ObjectOwnership::Surface),
            )
        })
        .collect())
}
