//! Declared definitions are approved evidence; bound code is vetted in apply.

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError};
use pbps_model::{DeclaredTypeBefore, DeclaredTypePin, IdsFile, ManagedType, Schema, TypeRef};

use super::{failed, input_routines};
use crate::pins::Within;

pub fn seal(
    base: &Schema,
    base_ids: &IdsFile,
    desired: &Schema,
    ids: &IdsFile,
) -> Vec<DeclaredTypePin> {
    ids.types
        .iter()
        .filter_map(|(uid, name)| {
            let definition = desired.types.get(name)?.clone();
            let before_name = base_ids.types.get(uid).unwrap_or(name);
            let before = base
                .types
                .get(before_name)
                .map(|definition| DeclaredTypeBefore {
                    name: before_name.clone(),
                    definition: definition.clone(),
                });
            Some(DeclaredTypePin {
                uid: uid.clone(),
                name: name.clone(),
                definition,
                before,
            })
        })
        .collect()
}

/// SPEC §7.6 compares rewritten expression text by presence: the checksum-pinned
/// statement supplied it in this uncommitted transaction. Structural fields
/// compare exactly (after base-type normalization); bound code is vetted below.
/// Catalog-to-catalog before-state drift still compares the entire definition.
pub fn matches_declaration(wrote: &ManagedType, live: &ManagedType) -> bool {
    use pbps_dialect::Dialect;
    match (wrote, live) {
        (ManagedType::Enum { values: a }, ManagedType::Enum { values: b }) => a == b,
        (
            ManagedType::Domain {
                base: a,
                not_null: an,
                default: ad,
                checks: ac,
                collation: al,
            },
            ManagedType::Domain {
                base: b,
                not_null: bn,
                default: bd,
                checks: bc,
                collation: bl,
            },
        ) => {
            let pg = crate::Postgres::new();
            matches!((pg.normalize_type(a), pg.normalize_type(b)), (Ok(a), Ok(b)) if a == b)
                && an == bn
                && ad.is_some() == bd.is_some()
                && al == bl
                && ac.keys().eq(bc.keys())
        }
        _ => false,
    }
}

async fn definitions(conn: &mut Conn, within: Within) -> Result<Schema, DbError> {
    let read = match within {
        Within::OwnTransaction => crate::catalog::introspect(conn).await?,
        Within::CallersTransaction => crate::catalog::introspect_within_transaction(conn).await?,
    };
    Ok(read.schema)
}

/// `initial` checks the approved before-state. Subsequent boundaries accept
/// an already-created, vetted step on resume, and re-vet it before use.
pub async fn check(
    conn: &mut Conn,
    pins: &[DeclaredTypePin],
    trusted: &BTreeSet<pbps_model::RoleName>,
    within: Within,
    initial: bool,
    complete: bool,
    changes: &pbps_model::ChangeSet,
) -> Result<(), DbError> {
    if pins.is_empty() {
        return Ok(());
    }
    let live = definitions(conn, within).await?;
    let mut present = Vec::new();
    for pin in pins {
        // Only operations releasing the new name can be its slot predecessor.
        #[allow(clippy::wildcard_enum_match_arm)]
        let released = changes.changes.iter().any(|step| match &step.change {
            pbps_model::Change::DropType { name, .. }
            | pbps_model::Change::RenameType { from: name, .. } => name == &pin.name,
            pbps_model::Change::DropTable { name, .. }
            | pbps_model::Change::RenameTable { from: name, .. } => {
                name.schema == pin.name.schema() && name.name == pin.name.name()
            }
            pbps_model::Change::DropModule {
                id,
                kind: pbps_model::ModuleKind::View,
            } => id.schema() == pin.name.schema() && id.name() == pin.name.name(),
            _ => false,
        });
        if pin.before.is_none() && released && !complete {
            continue;
        }
        let intermediate = pin
            .before
            .as_ref()
            .and_then(|before| TypeRef::new(pin.name.schema(), before.name.name()).ok());
        let expected = if initial {
            pin.before.as_ref().map(|b| (&b.name, &b.definition, true))
        } else if live.types.contains_key(&pin.name) {
            Some((&pin.name, &pin.definition, false))
        } else if let Some(name) = intermediate
            .as_ref()
            .filter(|n| live.types.contains_key(*n))
        {
            Some((name, &pin.definition, false))
        } else {
            pin.before.as_ref().map(|b| (&b.name, &b.definition, true))
        };
        if let Some((name, definition, before)) = expected {
            let actual = live.types.get(name).ok_or_else(|| {
                failed(format!(
                    "declared type `{}` is absent; plan again",
                    name.shown()
                ))
            })?;
            let mut expected = definition.clone();
            // A declared base follows its uid through an earlier/later rename.
            // Before statements, retain the exact catalog-to-catalog comparison.
            if !initial
                && let ManagedType::Domain { base, .. } = &mut expected
                && let Some(reference) = base.type_ref()
                && let Some(base_pin) = pins.iter().find(|p| &p.name == reference)
                && !live.types.contains_key(reference)
                && let Some(before) = &base_pin.before
            {
                let intermediate = TypeRef::new(base_pin.name.schema(), before.name.name()).ok();
                let current = intermediate
                    .as_ref()
                    .filter(|name| live.types.contains_key(*name))
                    .unwrap_or(&before.name);
                let ty = pbps_model::ColumnType::reference(current.clone());
                *base = if base.is_array() { ty.into_array() } else { ty };
            }
            if (initial && before && actual != definition)
                || (!(initial && before) && !matches_declaration(&expected, actual))
            {
                return Err(failed(format!(
                    "declared type `{}` changed since planning; plan again",
                    name.shown()
                )));
            }
            if complete && name != &pin.name {
                return Err(failed(format!(
                    "declared type `{}` was not renamed by its plan",
                    pin.name.shown()
                )));
            }
            present.push(name.clone());
        } else {
            let reads =
                crate::user_types::read(conn, within, std::slice::from_ref(&pin.name)).await;
            if !matches!(reads.as_slice(), [crate::user_types::TypeRead::Absent]) {
                return Err(failed(format!(
                    "declared type `{}` must be absent before creation",
                    pin.name.shown()
                )));
            }
            if complete {
                return Err(failed(format!(
                    "declared type `{}` was not created by its plan",
                    pin.name.shown()
                )));
            }
        }
    }
    input_routines(conn, within, &present, trusted, false).await?;
    Ok(())
}

/// Runs immediately after the emitter's type statement, before any dependent
/// statement and before the creating transaction can commit.
pub async fn vet(
    conn: &mut Conn,
    statement: &pbps_dialect::DeclaredTypeStatement,
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> Result<(), DbError> {
    let live = definitions(conn, Within::CallersTransaction).await?;
    match &statement.definition {
        Some(definition) => {
            let actual = live.types.get(&statement.name).ok_or_else(|| {
                failed(format!(
                    "declared type `{}` is absent after its statement",
                    statement.name.shown()
                ))
            })?;
            if !matches_declaration(definition, actual) {
                return Err(failed(format!(
                    "declared type `{}` does not match its approved definition",
                    statement.name.shown()
                )));
            }
            input_routines(
                conn,
                Within::CallersTransaction,
                std::slice::from_ref(&statement.name),
                trusted,
                false,
            )
            .await?;
        }
        None if live.types.contains_key(&statement.name) => {
            return Err(failed(format!(
                "declared type `{}` remains after its drop",
                statement.name.shown()
            )));
        }
        None => {}
    }
    Ok(())
}

pub fn names(pins: &[DeclaredTypePin]) -> BTreeSet<TypeRef> {
    pins.iter()
        .flat_map(|pin| {
            let mut names = vec![pin.name.clone()];
            if let Some(before) = &pin.before {
                names.push(before.name.clone());
                // SET SCHEMA and RENAME are individually committed staged steps.
                // The same uid also certifies the emitter's intermediate name.
                if let Ok(intermediate) = TypeRef::new(pin.name.schema(), before.name.name()) {
                    names.push(intermediate);
                }
            }
            names
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readback_expressions_compare_by_presence_while_structural_fields_compare_exactly() {
        let declared = ManagedType::Domain {
            base: "integer".parse().unwrap(),
            not_null: true,
            default: Some("1".into()),
            checks: [("positive".into(), "VALUE > 0".into())].into(),
            collation: None,
        };
        let mut readback = declared.clone();
        if let ManagedType::Domain {
            base,
            default,
            checks,
            ..
        } = &mut readback
        {
            *base = "int4".parse().unwrap();
            *default = Some("(1)".into());
            checks.insert("positive".into(), "(VALUE > 0)".into());
        }
        assert!(matches_declaration(&declared, &readback));
        for field in 0..5 {
            let mut changed = readback.clone();
            let ManagedType::Domain {
                base,
                not_null,
                default,
                checks,
                collation,
            } = &mut changed
            else {
                unreachable!()
            };
            match field {
                0 => *base = "bigint".parse().unwrap(),
                1 => *not_null = false,
                2 => *default = None,
                3 => {
                    checks.clear();
                    checks.insert("different".into(), "VALUE > 0".into());
                }
                _ => *collation = Some("pg_catalog.\"C\"".parse().unwrap()),
            }
            assert!(!matches_declaration(&declared, &changed), "field {field}");
        }
        let enumeration = ManagedType::Enum {
            values: vec!["a".into(), "b".into()],
        };
        assert!(!matches_declaration(
            &enumeration,
            &ManagedType::Enum {
                values: vec!["b".into(), "a".into()]
            }
        ));
        assert!(!matches_declaration(&enumeration, &declared));
    }

    #[test]
    fn declared_evidence_follows_the_uid_and_records_absence_without_a_catalog_pin() {
        let uid: pbps_model::Uid = "y_aaaaaa".parse().unwrap();
        let old: TypeRef = "app.old".parse().unwrap();
        let new: TypeRef = "app.new".parse().unwrap();
        let definition = ManagedType::Enum {
            values: vec!["ok".into()],
        };
        let base = Schema {
            types: [(old.clone(), definition.clone())].into(),
            ..Default::default()
        };
        let base_ids = IdsFile {
            types: [(uid.clone(), old.clone())].into(),
            ..Default::default()
        };
        let desired = Schema {
            types: [(new.clone(), definition.clone())].into(),
            ..Default::default()
        };
        let ids = IdsFile {
            types: [(uid.clone(), new.clone())].into(),
            ..Default::default()
        };
        let pins = seal(&base, &base_ids, &desired, &ids);
        assert_eq!(pins[0].uid, uid);
        assert_eq!(pins[0].before.as_ref().unwrap().name, old);
        assert_eq!(pins[0].name, new);
        assert!(
            seal(&Schema::default(), &IdsFile::default(), &desired, &ids)[0]
                .before
                .is_none()
        );
        assert!(seal(&base, &base_ids, &Schema::default(), &ids).is_empty());
        assert!(!matches_declaration(
            &definition,
            &ManagedType::Enum {
                values: vec!["changed".into()]
            }
        ));
    }
}
