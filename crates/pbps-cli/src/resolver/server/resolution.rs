//! Typed creation-time surfaces from the two qualified manifests that gave
//! the verdict. No later catalog read and no name-only ownership inference.

use super::Error;
use pbps_db::resolver::capture::{Assessment, Verdict};
use pbps_model::resolver::{
    BoundSurface, InputManifest, ObjectOwnership, Surface, SurfaceResolution,
};
use pbps_model::{ModuleId, Schema};
use pbps_pg::resolver::capture::BindingRecord;
use std::collections::{BTreeMap, BTreeSet};

fn required(schema: &Schema) -> BTreeSet<Surface> {
    let mut surfaces = BTreeSet::new();
    for (table, definition) in &schema.tables {
        for (column, spec) in &definition.columns {
            // Both declarations bind through pg_attrdef; their expressions
            // remain distinct model fields (DEC-1168.1).
            if spec.default.is_some() || spec.generated.is_some() {
                surfaces.insert(Surface::Default(table.column(column)));
            }
        }
        for name in definition.checks.keys() {
            surfaces.insert(Surface::Check {
                table: table.clone(),
                name: name.clone(),
            });
        }
        for (name, index) in &definition.indexes {
            if index.holds_expression() {
                surfaces.insert(Surface::Index {
                    table: table.clone(),
                    name: name.clone(),
                });
            }
        }
    }
    surfaces.extend(schema.modules.keys().cloned().map(Surface::Module));
    surfaces
}

fn binding_class(surface: &Surface) -> &'static str {
    match surface {
        Surface::Default(_) => "pg_attrdef",
        Surface::Check { .. } => "pg_constraint",
        Surface::Index { .. } => "pg_index",
        Surface::Module(ModuleId::Named(_)) => "pg_rewrite",
        Surface::Module(ModuleId::Routine(_)) => "pg_proc",
        Surface::Module(ModuleId::Trigger { .. }) => "pg_trigger",
        Surface::Namespace(_) | Surface::Table(_) | Surface::Column(_) => {
            unreachable!("not a creation-time binding surface")
        }
    }
}

pub(super) fn records(manifest: &InputManifest) -> Vec<BindingRecord> {
    manifest
        .prerequisites()
        .iter()
        .map(|entry| BindingRecord {
            object: entry.object.clone(),
            ownership: entry.ownership.clone(),
            bindings: entry.bindings.clone(),
        })
        .collect()
}

fn bound(records: &[BindingRecord], surface: &Surface) -> Result<BoundSurface, Error> {
    let owners: BTreeMap<_, _> = records
        .iter()
        .map(|entry| (&entry.object, &entry.ownership))
        .collect();
    let mut records = records.iter().filter(|entry| {
        entry.ownership == ObjectOwnership::Surface(surface.clone())
            && entry.object.class == binding_class(surface)
    });
    let record = records.next().ok_or_else(|| {
        Error::Binding(format!(
            "the qualified catalog has no creation-time record for {surface:?}"
        ))
    })?;
    if records.next().is_some() {
        return Err(Error::Binding(
            "a declared surface has multiple catalog binding records".into(),
        ));
    }
    let managed_inputs = record
        .bindings
        .iter()
        .filter_map(|binding| match owners.get(&binding.target) {
            Some(ObjectOwnership::Surface(surface)) => Some(surface.clone()),
            _ => None,
        })
        .collect();
    Ok(BoundSurface {
        object: record.object.clone(),
        bindings: record.bindings.clone(),
        managed_inputs,
    })
}

pub(super) fn from_records(
    base: &Schema,
    desired: &Schema,
    opening: &[BindingRecord],
    compiled: &[BindingRecord],
    assessment: &Assessment,
) -> Result<Vec<SurfaceResolution>, Error> {
    let before = required(base);
    let after = required(desired);
    let mut resolved = Vec::new();
    for surface in before.union(&after) {
        let current = before
            .contains(surface)
            .then(|| bound(opening, surface))
            .transpose()?;
        let desired = after
            .contains(surface)
            .then(|| bound(compiled, surface))
            .transpose()?;
        // Every desired surface needs its verdict: one the target holds
        // under the same identity is compared, and one it does not hold yet
        // (created, or relocated by a parent rename) has its candidates
        // checked, since its creation binds against the target's.
        if let Some(desired) = &desired {
            let kept = current
                .as_ref()
                .is_some_and(|current| current.object == desired.object);
            match assessment.surfaces.get(&desired.object) {
                Some(Verdict::Unaffected | Verdict::Rebuild) => {}
                Some(Verdict::Created) if !kept => {}
                // A surface that binds nothing at creation, such as a
                // routine whose string body binds only at run time, has no
                // lookup a candidate could change: the assessment gives it
                // no verdict, for the same reason (#1303 review).
                None if desired.bindings.is_empty() => {}
                Some(Verdict::Unresolved { condition }) => {
                    return Err(Error::Binding(format!(
                        "the declared surface is unresolved: {condition}"
                    )));
                }
                Some(Verdict::Created) | None => {
                    return Err(Error::Binding(
                        "the declared surface lacks its binding verdict".into(),
                    ));
                }
            }
        }
        resolved.push(SurfaceResolution {
            surface: surface.clone(),
            current,
            desired,
        });
    }
    Ok(resolved)
}
