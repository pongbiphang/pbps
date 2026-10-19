//! Which captured objects a declaration owns, in the producer's fresh read
//! (the lesson of DEC-614.2: ownership is qualified from the start).
//!
//! As on PostgreSQL, a reference to an object is never ownership of it. A
//! declared name the target holds as another kind of object refuses rather
//! than lending its surface to whatever has the spelling. A name is matched
//! as the catalog spells it: a declaration whose spelling differs owns
//! nothing, which leaves its record unqualified, never another's.

use super::{CapturedInputs, ObjectIdentity, Uncovered, class};
use pbps_model::resolver::{ObjectOwnership, Surface};
use pbps_model::{IdsFile, ModuleId, ModuleKind, Schema, TableName, ValueSource};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The declarations and their recorded identities, as the producer holds
/// them. Neither a serialized manifest nor a guessed name may supply them.
pub struct RecordedOwnership<'a> {
    pub schema: &'a Schema,
    pub ids: &'a IdsFile,
    /// Schemas the scope expressly includes. They have no table UID.
    pub namespaces: &'a BTreeSet<String>,
}

fn object(name: &TableName, own: &str) -> ObjectIdentity {
    super::object(&name.schema, own)
}

fn column(table: &ObjectIdentity, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class::COLUMN.into(),
        name: vec![name.into()],
        signature: vec![table.clone()],
    }
}

fn property<'a>(
    capture: &'a CapturedInputs,
    object: &ObjectIdentity,
    key: &str,
) -> Option<&'a Value> {
    capture
        .inputs
        .get(object)
        .and_then(|input| input.properties.get(key))
}

fn kind<'a>(capture: &'a CapturedInputs, object: &ObjectIdentity) -> Result<&'a str, Uncovered> {
    property(capture, object, "type")
        .and_then(Value::as_str)
        .ok_or_else(|| Uncovered::object(object, "declared catalog kind is unreadable"))
}

fn parent(capture: &CapturedInputs, object: &ObjectIdentity) -> Option<ObjectIdentity> {
    property(capture, object, "parent").and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn assign(
    owned: &mut BTreeMap<ObjectIdentity, Surface>,
    object: ObjectIdentity,
    surface: Surface,
) -> Result<(), Uncovered> {
    if let Some(previous) = owned.insert(object.clone(), surface.clone())
        && previous != surface
    {
        return Err(Uncovered::object(
            &object,
            "catalog record has conflicting declared owners",
        ));
    }
    Ok(())
}

/// Assign `surface` to `object` if the capture holds it, and refuse if it
/// holds it as another kind. `parent` is the table a child must be on.
fn direct(
    capture: &CapturedInputs,
    owned: &mut BTreeMap<ObjectIdentity, Surface>,
    object: ObjectIdentity,
    accepted: &[&str],
    on: Option<&ObjectIdentity>,
    surface: Surface,
) -> Result<bool, Uncovered> {
    if !capture.inputs.contains_key(&object) {
        return Ok(false);
    }
    if !accepted.contains(&kind(capture, &object)?) {
        return Err(Uncovered::object(
            &object,
            "declared name has a different catalog kind",
        ));
    }
    if let Some(on) = on
        && parent(capture, &object).as_ref() != Some(on)
    {
        return Err(Uncovered::object(
            &object,
            "declared object belongs to a different table",
        ));
    }
    assign(owned, object, surface)?;
    Ok(true)
}

/// Every column of `parent` the capture holds takes `surface`.
fn columns_of(
    capture: &CapturedInputs,
    owned: &mut BTreeMap<ObjectIdentity, Surface>,
    parent: &ObjectIdentity,
    surface: &Surface,
) -> Result<(), Uncovered> {
    let children: Vec<_> = capture
        .inputs
        .keys()
        .filter(|id| id.class == class::COLUMN && id.signature.first() == Some(parent))
        .cloned()
        .collect();
    for child in children {
        assign(owned, child, surface.clone())?;
    }
    Ok(())
}

pub(super) fn classify(
    capture: &CapturedInputs,
    recorded: &RecordedOwnership<'_>,
) -> Result<BTreeMap<ObjectIdentity, ObjectOwnership>, Uncovered> {
    recorded
        .ids
        .validate()
        .map_err(|_| Uncovered::class("recorded-uid", "recorded managed identities are invalid"))?;
    let names = recorded.ids.index();
    let mut owned = BTreeMap::new();
    for schema in recorded.namespaces {
        let namespace = ObjectIdentity {
            class: class::SCHEMA.into(),
            name: vec![schema.clone()],
            signature: Vec::new(),
        };
        if capture.inputs.contains_key(&namespace) {
            assign(&mut owned, namespace, Surface::Namespace(schema.clone()))?;
        }
    }
    for (name, table) in &recorded.schema.tables {
        if names.table_uid(name).is_none() {
            return Err(Uncovered::class(
                "recorded-uid",
                "declared table lacks its recorded UID",
            ));
        }
        let table_id = object(name, &name.name);
        let table_owner = Surface::Table(name.clone());
        let present = direct(
            capture,
            &mut owned,
            table_id.clone(),
            &["U"],
            None,
            table_owner.clone(),
        )?;
        for (declared, declaration) in &table.columns {
            let reference = name.column(declared);
            if names.column_uid(&reference).is_none() {
                return Err(Uncovered::class(
                    "recorded-uid",
                    "declared column lacks its recorded UID",
                ));
            }
            let column_id = column(&table_id, declared.raw());
            if capture.inputs.contains_key(&column_id) {
                assign(
                    &mut owned,
                    column_id.clone(),
                    Surface::Column(reference.clone()),
                )?;
            }
            if present && let ValueSource::Default(_) = &declaration.value_source {
                // The column's DEFAULT constraint, whatever its name: the one
                // on this table whose column is this one.
                let defaults: Vec<_> = capture
                    .inputs
                    .keys()
                    .filter(|id| {
                        id.class == class::OBJECT
                            && property(capture, id, "type").and_then(Value::as_str) == Some("D")
                            && parent(capture, id).as_ref() == Some(&table_id)
                            && property(capture, id, "column")
                                == serde_json::to_value(&column_id).ok().as_ref()
                    })
                    .cloned()
                    .collect();
                for default in defaults {
                    assign(&mut owned, default, Surface::Default(reference.clone()))?;
                }
            }
        }
        for computed in table.computed.keys() {
            let column_id = column(&table_id, computed.raw());
            if capture.inputs.contains_key(&column_id) {
                assign(
                    &mut owned,
                    column_id,
                    Surface::Column(name.column(computed)),
                )?;
            }
        }
        for check in table.checks.keys() {
            direct(
                capture,
                &mut owned,
                object(name, check),
                &["C"],
                Some(&table_id),
                Surface::Check {
                    table: name.clone(),
                    name: check.clone(),
                },
            )?;
        }
        // A key constraint is declared with its table: its surface is the
        // table's, as on PostgreSQL.
        let keys = table
            .unique
            .keys()
            .map(|key| (key.as_str(), "UQ"))
            .chain(table.foreign_keys.keys().map(|key| (key.as_str(), "F")))
            .chain(
                table
                    .primary_key
                    .as_ref()
                    .and_then(|key| key.name.as_deref())
                    .map(|key| (key, "PK")),
            );
        for (key, expected) in keys {
            direct(
                capture,
                &mut owned,
                object(name, key),
                &[expected],
                Some(&table_id),
                table_owner.clone(),
            )?;
        }
        if present
            && table
                .primary_key
                .as_ref()
                .is_some_and(|key| key.name.is_none())
        {
            // The engine names an unnamed primary key; the table has one.
            let primary: Vec<_> = capture
                .inputs
                .keys()
                .filter(|id| {
                    id.class == class::OBJECT
                        && property(capture, id, "type").and_then(Value::as_str) == Some("PK")
                        && parent(capture, id).as_ref() == Some(&table_id)
                })
                .cloned()
                .collect();
            match primary.as_slice() {
                [key] => assign(&mut owned, key.clone(), table_owner.clone())?,
                [] => {
                    return Err(Uncovered::object(
                        &table_id,
                        "declared unnamed primary key is absent",
                    ));
                }
                _ => {
                    return Err(Uncovered::object(
                        &table_id,
                        "declared unnamed primary key has multiple catalog roots",
                    ));
                }
            }
        }
    }
    for (id, module) in &recorded.schema.modules {
        let surface = Surface::Module(id.clone());
        match (id, module.kind) {
            (ModuleId::Named(name), ModuleKind::View) => {
                let root = object(name, &name.name);
                if direct(
                    capture,
                    &mut owned,
                    root.clone(),
                    &["V"],
                    None,
                    surface.clone(),
                )? {
                    // A view's result columns are replaced with it.
                    columns_of(capture, &mut owned, &root, &surface)?;
                }
            }
            (ModuleId::Routine(routine), ModuleKind::Procedure) => {
                direct(
                    capture,
                    &mut owned,
                    object(&routine.name, &routine.name.name),
                    &["P"],
                    None,
                    surface,
                )?;
            }
            (ModuleId::Routine(routine), ModuleKind::Function) => {
                let root = object(&routine.name, &routine.name.name);
                if direct(
                    capture,
                    &mut owned,
                    root.clone(),
                    &["FN", "IF", "TF"],
                    None,
                    surface.clone(),
                )? {
                    columns_of(capture, &mut owned, &root, &surface)?;
                }
            }
            (ModuleId::Trigger { on, name }, ModuleKind::Trigger) => {
                let table = object(on, &on.name);
                direct(
                    capture,
                    &mut owned,
                    object(on, name),
                    &["TR"],
                    Some(&table),
                    surface,
                )?;
            }
            _ => {
                return Err(Uncovered::class(
                    "managed-module",
                    "declared module identity and kind disagree",
                ));
            }
        }
    }
    Ok(owned
        .into_iter()
        .map(|(object, surface)| (object, ObjectOwnership::Surface(surface)))
        .collect())
}
