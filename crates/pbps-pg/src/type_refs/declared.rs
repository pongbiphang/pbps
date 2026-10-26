//! Declared definitions are approved evidence; bound code is vetted in apply.

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError};
use pbps_model::{DeclaredTypeBefore, DeclaredTypePin, IdsFile, ManagedType, Schema, TypeRef};

use super::{failed, input_routines};
use crate::pins::Within;
use pbps_diff::identity::type_refs::{TypeResolver, TypeSide, after_changes, before_changes};

pub fn seal(
    base: &Schema,
    base_ids: &IdsFile,
    desired: &Schema,
    ids: &IdsFile,
) -> Vec<DeclaredTypePin> {
    let resolver = TypeResolver::new(base_ids, ids);
    ids.types
        .iter()
        .filter_map(|(uid, name)| {
            let definition = desired.types.get(name)?.clone();
            let before = resolver.name(TypeSide::Base, uid).and_then(|before_name| {
                base.types
                    .get(before_name)
                    .map(|definition| DeclaredTypeBefore {
                        name: before_name.clone(),
                        definition: definition.clone(),
                    })
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

/// Follow the emitter's uid annotation through the executed prefix, including
/// SET SCHEMA's intermediate name. A final name can still hold another uid.
#[cfg(test)]
fn current<'a>(
    pin: &'a DeclaredTypePin,
    executed: &'a [pbps_dialect::Statement],
) -> Option<(&'a TypeRef, &'a ManagedType, bool)> {
    if let Some(step) = executed
        .iter()
        .rev()
        .filter_map(|s| s.declared_type.as_ref())
        .find(|s| s.uid == pin.uid)
    {
        step.definition.as_ref().map(|_| {
            // A rename writes no expression. Its original catalog definition
            // remains pinned after the move; only CREATE uses authored text.
            match &pin.before {
                Some(before) => (&step.name, &before.definition, true),
                None => (&step.name, &pin.definition, false),
            }
        })
    } else {
        pin.before.as_ref().map(|b| (&b.name, &b.definition, true))
    }
}

// A pending creation may still see the approved predecessor. Once the uid's
// CREATE has run, it must be checked even when its name was handed over.
#[allow(clippy::wildcard_enum_match_arm)] // Only these changes release a pg_type slot.
fn pending_predecessor(
    pin: &DeclaredTypePin,
    live: &Schema,
    changes: &pbps_model::ChangeSet,
    executed: &[pbps_dialect::Statement],
) -> bool {
    changes.changes.iter().any(|step| match &step.change {
        pbps_model::Change::DropType { uid, name, .. }
        | pbps_model::Change::RenameType {
            uid, from: name, ..
        } => {
            name == &pin.name
                && !executed
                    .iter()
                    .filter_map(|s| s.declared_type.as_ref())
                    .any(|s| &s.uid == uid)
        }
        pbps_model::Change::DropTable { name, .. }
        | pbps_model::Change::RenameTable { from: name, .. } => {
            name.schema == pin.name.schema()
                && name.name == pin.name.name()
                && live.tables.contains_key(name)
        }
        pbps_model::Change::DropModule {
            id,
            kind: pbps_model::ModuleKind::View,
        } => {
            id.schema() == pin.name.schema()
                && id.name() == pin.name.name()
                && live
                    .modules
                    .get(id)
                    .is_some_and(|m| m.kind == pbps_model::ModuleKind::View)
        }
        _ => false,
    })
}

pub(super) fn identities(
    pins: &[DeclaredTypePin],
    changes: &pbps_model::ChangeSet,
) -> (IdsFile, IdsFile) {
    let declared = IdsFile {
        types: pins
            .iter()
            .map(|p| (p.uid.clone(), p.name.clone()))
            .collect(),
        ..Default::default()
    };
    // Bootstrap and per-statement rechecks carry external pins alone. Their
    // approved type operations still establish the desired uid map; requiring
    // a catalog pin to discover a CREATE would demand evidence of an absent type.
    let desired = after_changes(&declared, changes);
    let mut base = before_changes(&desired, changes);
    for pin in pins {
        if let Some(before) = &pin.before {
            base.types.insert(pin.uid.clone(), before.name.clone());
        } else {
            base.types.remove(&pin.uid);
        }
    }
    (base, desired)
}

/// A domain whose base is the replacement of a rebuilt type, read as built on
/// the type's own name: the replacement is that type once the swap has run.
fn over_the_final_name(
    definition: &mut ManagedType,
    rebuilt: &std::collections::BTreeMap<TypeRef, TypeRef>,
) {
    if let ManagedType::Domain { base, .. } = definition
        && let Some(name) = base.type_ref().and_then(|at| rebuilt.get(at))
    {
        let named = pbps_model::ColumnType::reference(name.clone());
        *base = if base.is_array() {
            named.into_array()
        } else {
            named
        };
    }
}

fn check_definitions(
    live: &Schema,
    pins: &[DeclaredTypePin],
    complete: bool,
    changes: &pbps_model::ChangeSet,
    executed: &[pbps_dialect::Statement],
) -> Result<(Vec<TypeRef>, Vec<TypeRef>), DbError> {
    let (base, desired) = identities(pins, changes);
    let mut positioned = base.clone();
    for step in executed.iter().filter_map(|s| s.declared_type.as_ref()) {
        if step.definition.is_some() {
            positioned.types.insert(step.uid.clone(), step.name.clone());
        } else {
            positioned.types.remove(&step.uid);
        }
    }
    let resolver = TypeResolver::new(&base, &positioned);
    let desired_resolver = TypeResolver::new(&desired, &positioned);
    let renames = super::renames::TypeRenames::resolved(&base, &positioned);
    // The replacement each rebuilt type holds until its swap (DEC-1635.7).
    let rebuilt_tmp: std::collections::BTreeMap<TypeRef, TypeRef> = changes
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::EnumRebuild {
                name,
                tmp,
                step: pbps_model::EnumRebuildStep::Create { .. },
                ..
            } = &p.change
            {
                Some((tmp.clone(), name.clone()))
            } else {
                None
            }
        })
        .collect();
    let mut present = Vec::new();
    let mut absent = Vec::new();
    for pin in pins {
        if let Some(name) = resolver.name(TypeSide::Desired, &pin.uid) {
            let latest = executed
                .iter()
                .filter_map(|s| s.declared_type.as_ref())
                .rev()
                .find(|s| s.uid == pin.uid);
            // A domain the plan has yet to alter, once a label of an enum has
            // been renamed or added: the engine now spells the labels its
            // default and checks name as they stand, so their text no longer
            // equals the catalog text of before the plan. What a label change
            // cannot touch, the structure, is still held (DEC-1635.8). Not
            // "no statement of this domain has run": a rename of the domain
            // itself changes only its name, which the uid already resolves,
            // and an alteration that has run is held by its own structure
            // below.
            let label_moved = !executed
                .iter()
                .filter_map(|s| s.declared_type.as_ref())
                .any(|s| s.uid == pin.uid && s.altered)
                && executed.iter().any(|s| {
                    s.declared_type
                        .as_ref()
                        .is_some_and(|t| matches!(t.definition, Some(ManagedType::Enum { .. })))
                })
                && changes.changes.iter().any(|p| {
                    matches!(&p.change, pbps_model::Change::AlterDomain { uid, .. } if uid == &pin.uid)
                });
            let (definition, before) = match latest.and_then(|s| s.definition.as_ref()) {
                Some(definition @ ManagedType::Enum { .. }) => (definition, true),
                // A domain the plan altered in place is held to the structure
                // its latest alteration left, its default and checks by
                // presence, for the reason the catalog text of before the
                // plan is no longer there to compare (DEC-1635.8).
                Some(definition @ ManagedType::Domain { .. })
                    if latest.is_some_and(|s| s.altered) =>
                {
                    (definition, false)
                }
                _ => match &pin.before {
                    Some(before) => (&before.definition, true),
                    None => (&pin.definition, false),
                },
            };
            let actual = live.types.get(name).ok_or_else(|| {
                failed(format!(
                    "declared type `{}` is absent; plan again",
                    name.shown()
                ))
            })?;
            // A domain built on a type this plan rebuilds stands over the
            // replacement until the swap names it back: both sides are read
            // as over the final name (DEC-1635.9).
            let mut named = actual.clone();
            over_the_final_name(&mut named, &rebuilt_tmp);
            let actual = &named;
            let mut expected = definition.clone();
            if label_moved && let Some(was) = &pin.before {
                let mut was = was.definition.clone();
                renames.definition(&mut was);
                if !matches_declaration(&was, actual) {
                    return Err(failed(format!(
                        "declared type `{}` changed since planning; plan again",
                        name.shown()
                    )));
                }
                present.push(name.clone());
                continue;
            }
            if before {
                // Pins hold only the saved catalog text. Follow exactly the
                // approved moves already executed, including expression casts.
                renames.definition(&mut expected);
            } else if let ManagedType::Domain { base, .. } = &mut expected {
                desired_resolver.remap(base, TypeSide::Base, TypeSide::Desired);
            }
            // After the remap, which resolves a base by uid to where it stands.
            over_the_final_name(&mut expected, &rebuilt_tmp);
            let mut comparable = actual.clone();
            super::renames::TypeRenames::default().definition(&mut comparable);
            if (before && comparable != expected)
                || (!before && !matches_declaration(&expected, actual))
            {
                return Err(failed(format!(
                    "declared type `{}` changed since planning; plan again",
                    name.shown()
                )));
            }
            if complete
                && (matches!(pin.definition, ManagedType::Enum { .. })
                    || latest.is_some_and(|s| s.altered))
                && !matches_declaration(&pin.definition, actual)
            {
                return Err(failed(format!(
                    "declared type `{}` does not match the completed plan",
                    pin.name.shown()
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
            if complete {
                return Err(failed(format!(
                    "declared type `{}` was not created by its plan",
                    pin.name.shown()
                )));
            }
            if !pending_predecessor(pin, live, changes, executed) {
                if live.types.contains_key(&pin.name) {
                    return Err(failed(format!(
                        "declared type `{}` must be absent before creation",
                        pin.name.shown()
                    )));
                }
                absent.push(pin.name.clone());
            }
        }
    }
    Ok((present, absent))
}

/// Check each uid where the executed statements leave it. Untouched definitions
/// retain catalog-to-catalog drift checks; emitted definitions use SPEC §7.6.
pub async fn check(
    conn: &mut Conn,
    pins: &[DeclaredTypePin],
    trusted: &BTreeSet<pbps_model::RoleName>,
    within: Within,
    complete: bool,
    changes: &pbps_model::ChangeSet,
    executed: &[pbps_dialect::Statement],
) -> Result<(), DbError> {
    if pins.is_empty() {
        return Ok(());
    }
    let live = definitions(conn, within).await?;
    let (present, absent) = check_definitions(&live, pins, complete, changes, executed)?;
    for name in absent {
        let reads = crate::user_types::read(conn, within, std::slice::from_ref(&name)).await;
        if !matches!(reads.as_slice(), [crate::user_types::TypeRead::Absent]) {
            return Err(failed(format!(
                "declared type `{}` must be absent before creation",
                name.shown()
            )));
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
            // A check added `NOT VALID` binds its expression and evaluates no
            // row; the scan that does comes only after the vet above has read
            // the bound code (DEC-1635.8).
            if let Some(tail) = &statement.after_vet {
                conn.execute(tail).await?;
            }
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

    fn enumeration(value: &str) -> ManagedType {
        ManagedType::Enum {
            values: vec![value.into()],
        }
    }

    fn renamed_pin(seed: &str, from: &str, to: &str, definition: ManagedType) -> DeclaredTypePin {
        DeclaredTypePin {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Type, seed, 0),
            name: to.parse().unwrap(),
            definition: definition.clone(),
            before: Some(DeclaredTypeBefore {
                name: from.parse().unwrap(),
                definition,
            }),
        }
    }

    fn wrote(pin: &DeclaredTypePin, name: &str) -> pbps_dialect::Statement {
        let mut statement = pbps_dialect::Statement::new("unused");
        statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
            uid: pin.uid.clone(),
            name: name.parse().unwrap(),
            definition: Some(pin.definition.clone()),
            altered: false,
            after_vet: None,
        });
        statement
    }

    #[test]
    fn a_new_domain_pin_resolves_its_base_on_the_desired_side_at_each_position() {
        for array in ["", "[]"] {
            let mut domain = renamed_pin(
                "domain2321",
                "app.d",
                "app.d",
                ManagedType::Domain {
                    base: format!("app.b{array}").parse().unwrap(),
                    not_null: false,
                    default: None,
                    checks: Default::default(),
                    collation: None,
                },
            );
            domain.before = None;
            let pins = [
                renamed_pin("a2321", "app.a", "app.b", enumeration("a")),
                renamed_pin("b2321", "app.b", "app.c", enumeration("b")),
                domain,
            ];
            let statement = wrote(&pins[2], "app.d");
            let mut actual = pins[2].definition.clone();
            let ManagedType::Domain { base, .. } = &mut actual else {
                unreachable!()
            };
            *base = format!("app.a{array}").parse().unwrap();
            let mut live = Schema {
                types: [
                    ("app.a".parse().unwrap(), enumeration("a")),
                    ("app.b".parse().unwrap(), enumeration("b")),
                    ("app.d".parse().unwrap(), actual),
                ]
                .into(),
                ..Default::default()
            };
            check_definitions(
                &live,
                &pins,
                false,
                &Default::default(),
                std::slice::from_ref(&statement),
            )
            .unwrap();
            live.types
                .insert("app.d".parse().unwrap(), pins[2].definition.clone());
            assert!(
                check_definitions(&live, &pins, false, &Default::default(), &[statement]).is_err()
            );
        }
    }

    #[test]
    fn rename_chains_check_each_uid_at_every_executed_boundary_and_refuse_holder_drift() {
        let pins = [
            renamed_pin("a", "app.a", "app.b", enumeration("a")),
            renamed_pin("b", "app.b", "app.c", enumeration("b")),
        ];
        let statements = [wrote(&pins[1], "app.c"), wrote(&pins[0], "app.b")];
        let mut live = Schema {
            types: [
                ("app.a".parse().unwrap(), enumeration("a")),
                ("app.b".parse().unwrap(), enumeration("b")),
            ]
            .into(),
            ..Default::default()
        };
        for completed in 0..=statements.len() {
            let executed = &statements[..completed];
            check_definitions(
                &live,
                &pins,
                completed == statements.len(),
                &Default::default(),
                executed,
            )
            .unwrap();
            for pin in &pins {
                let name = current(pin, executed).unwrap().0;
                let mut drift = live.clone();
                drift.types.insert(name.clone(), enumeration("manual"));
                assert!(
                    check_definitions(&drift, &pins, false, &Default::default(), executed).is_err(),
                    "holder {} after {completed} steps",
                    name.shown()
                );
            }
            if let Some(statement) = statements.get(completed) {
                let step = statement.declared_type.as_ref().unwrap();
                let pin = pins.iter().find(|p| p.uid == step.uid).unwrap();
                let old = current(pin, executed).unwrap().0;
                let definition = live.types.remove(old).unwrap();
                live.types.insert(step.name.clone(), definition);
            }
        }
        assert!(check_definitions(&live, &pins, true, &Default::default(), &[]).is_err());
    }

    #[test]
    fn kept_domain_expressions_follow_executed_type_moves_and_refuse_real_drift() {
        let domain = ManagedType::Domain {
            base: "app.mood".parse().unwrap(),
            not_null: false,
            default: Some("'ok'::app.mood".into()),
            checks: [("allowed".into(), "VALUE <> 'sad'::app.mood".into())].into(),
            collation: None,
        };
        let pins = [
            renamed_pin("mood", "app.mood", "moved.feeling", enumeration("ok")),
            renamed_pin("kept", "app.kept", "app.kept", domain.clone()),
        ];
        let statements = [
            wrote(&pins[0], "moved.mood"),
            wrote(&pins[0], "moved.feeling"),
        ];
        for completed in 0..=statements.len() {
            let name = current(&pins[0], &statements[..completed]).unwrap().0;
            let spelling = ["app.mood", "moved.mood", "moved.feeling"][completed];
            let actual = ManagedType::Domain {
                base: pbps_model::ColumnType::reference(name.clone()),
                not_null: false,
                default: Some(format!("'ok'::{spelling}")),
                checks: [("allowed".into(), format!("VALUE <> 'sad'::{spelling}"))].into(),
                collation: None,
            };
            let live = Schema {
                types: [
                    (name.clone(), enumeration("ok")),
                    ("app.kept".parse().unwrap(), actual),
                ]
                .into(),
                ..Default::default()
            };
            check_definitions(
                &live,
                &pins,
                completed == 2,
                &Default::default(),
                &statements[..completed],
            )
            .unwrap();
            for check in [false, true] {
                let mut drift = live.clone();
                let ManagedType::Domain {
                    default, checks, ..
                } = drift.types.get_mut(&"app.kept".parse().unwrap()).unwrap()
                else {
                    unreachable!()
                };
                if check {
                    checks.insert("allowed".into(), "VALUE IS NOT NULL".into());
                } else {
                    *default = Some(format!("'sad'::{spelling}"));
                }
                assert!(
                    check_definitions(
                        &drift,
                        &pins,
                        false,
                        &Default::default(),
                        &statements[..completed]
                    )
                    .is_err()
                );
            }
        }
    }

    fn domain(not_null: bool, default: Option<&str>, checks: &[(&str, &str)]) -> ManagedType {
        ManagedType::Domain {
            base: "text".parse().unwrap(),
            not_null,
            default: default.map(str::to_owned),
            checks: checks
                .iter()
                .map(|(name, text)| ((*name).into(), (*text).to_owned()))
                .collect(),
            collation: None,
        }
    }

    /// DEC-1635.8: after an in-place alteration the live domain is held to the
    /// structure that alteration left (name set, NOT NULL, base, collation),
    /// and its default and check texts by presence; before any statement it is
    /// still held to the catalog text of the plan's baseline.
    #[test]
    fn an_altered_domain_is_held_to_the_structure_each_alteration_left() {
        let from = domain(false, None, &[("c1", "(VALUE <> ''::text)")]);
        let to = domain(
            true,
            Some("'a'"),
            &[("c1", "VALUE <> ''"), ("c2", "length(VALUE) < 9")],
        );
        let pin = DeclaredTypePin {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Type, "altered", 0),
            name: "app.d".parse().unwrap(),
            definition: to.clone(),
            before: Some(DeclaredTypeBefore {
                name: "app.d".parse().unwrap(),
                definition: from.clone(),
            }),
        };
        let statement = |definition: ManagedType, altered: bool| {
            let mut statement = pbps_dialect::Statement::new("unused");
            statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
                uid: pin.uid.clone(),
                name: "app.d".parse().unwrap(),
                definition: Some(definition),
                altered,
                after_vet: None,
            });
            statement
        };
        let live = |definition: ManagedType| Schema {
            types: [("app.d".parse().unwrap(), definition)].into(),
            ..Default::default()
        };
        let check = |live: &Schema, complete: bool, executed: &[pbps_dialect::Statement]| {
            check_definitions(
                live,
                std::slice::from_ref(&pin),
                complete,
                &Default::default(),
                executed,
            )
        };
        // Before any statement: the baseline's catalog text, exactly.
        check(&live(from.clone()), false, &[]).unwrap();
        assert!(
            check(
                &live(domain(false, None, &[("c1", "(VALUE <> 'x'::text)")])),
                false,
                &[]
            )
            .is_err()
        );
        // The loosening half left no default and the one check, as `from`
        // had it, and NOT NULL still off: held by structure, not by text.
        let loosened = domain(false, Some("'a'"), &[("c1", "VALUE <> ''")]);
        let after_loosen = live(domain(
            false,
            Some("'a'::text"),
            &[("c1", "(VALUE <> ''::text)")],
        ));
        check(&after_loosen, false, &[statement(loosened.clone(), true)]).unwrap();
        // Negative: the same live state, from a statement that altered
        // nothing, is drift from the baseline.
        assert!(check(&after_loosen, false, &[statement(loosened.clone(), false)]).is_err());
        // The tightening half left both checks and NOT NULL.
        let tightened = live(domain(
            true,
            Some("'a'::text"),
            &[("c1", "(VALUE <> ''::text)"), ("c2", "(length(VALUE) < 9)")],
        ));
        let executed = [statement(loosened, true), statement(to.clone(), true)];
        check(&tightened, true, &executed).unwrap();
        // Negatives, each a structural field: a check missing or extra,
        // NOT NULL, the default, the base.
        for wrong in [
            domain(true, Some("'a'"), &[("c1", "x")]),
            domain(true, Some("'a'"), &[("c1", "x"), ("c2", "x"), ("c3", "x")]),
            domain(false, Some("'a'"), &[("c1", "x"), ("c2", "x")]),
            domain(true, None, &[("c1", "x"), ("c2", "x")]),
        ] {
            assert!(
                check(&live(wrong.clone()), true, &executed).is_err(),
                "{wrong:?}"
            );
            assert!(check(&live(wrong), false, &executed).is_err());
        }
        let mut other_base = domain(true, Some("'a'"), &[("c1", "x"), ("c2", "x")]);
        if let ManagedType::Domain { base, .. } = &mut other_base {
            *base = "integer".parse().unwrap();
        }
        assert!(check(&live(other_base), true, &executed).is_err());
    }

    /// A label renamed before the plan alters the domain that names it leaves
    /// the domain's catalog text changed (`'a'` deparses as `'b'`). Its
    /// structure is still held, and only while the plan has an alteration of
    /// it to come (DEC-1635.8).
    #[test]
    fn a_label_change_before_the_alteration_does_not_make_the_domain_drift() {
        let from = domain(
            false,
            Some("'a'::app.e"),
            &[("c1", "(VALUE <> 'a'::app.e)")],
        );
        let to = domain(false, Some("'b'"), &[("c1", "VALUE <> 'b'")]);
        let uid = pbps_model::Uid::derived(pbps_model::UidKind::Type, "labelled", 0);
        let pin = DeclaredTypePin {
            uid: uid.clone(),
            name: "app.d".parse().unwrap(),
            definition: to.clone(),
            before: Some(DeclaredTypeBefore {
                name: "app.d".parse().unwrap(),
                definition: from.clone(),
            }),
        };
        let renamed_label = {
            let mut statement = pbps_dialect::Statement::new("unused");
            statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
                uid: pbps_model::Uid::derived(pbps_model::UidKind::Type, "enum", 0),
                name: "app.e".parse().unwrap(),
                definition: Some(ManagedType::Enum {
                    values: vec!["b".into()],
                }),
                altered: false,
                after_vet: None,
            });
            statement
        };
        let altering = |half| pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::AlterDomain {
                    uid: uid.clone(),
                    name: "app.d".parse().unwrap(),
                    from: Box::new(from.clone()),
                    to: Box::new(to.clone()),
                    half,
                },
            )],
        };
        let live = |definition: ManagedType| Schema {
            types: [("app.d".parse().unwrap(), definition)].into(),
            ..Default::default()
        };
        let check = |live: &Schema,
                     changes: &pbps_model::ChangeSet,
                     executed: &[pbps_dialect::Statement]| {
            check_definitions(live, std::slice::from_ref(&pin), false, changes, executed)
        };
        let respelled = live(domain(
            false,
            Some("'b'::app.e"),
            &[("c1", "(VALUE <> 'b'::app.e)")],
        ));
        let alter = altering(pbps_model::DomainHalf::Loosen);
        check(&respelled, &alter, std::slice::from_ref(&renamed_label)).unwrap();
        // Negatives: the text differs with no label change executed, or with
        // no alteration to come, is drift as ever.
        assert!(check(&respelled, &alter, &[]).is_err());
        assert!(
            check(
                &respelled,
                &Default::default(),
                std::slice::from_ref(&renamed_label)
            )
            .is_err()
        );
        // A structural difference is still drift: a check missing, NOT NULL,
        // the default gone.
        for wrong in [
            domain(false, Some("'b'::app.e"), &[]),
            domain(true, Some("'b'::app.e"), &[("c1", "x")]),
            domain(false, None, &[("c1", "x")]),
        ] {
            assert!(check(&live(wrong), &alter, std::slice::from_ref(&renamed_label)).is_err());
        }
    }

    /// The domain is renamed first, then a label of its base enum, and the
    /// alteration that sets a default using the new label is still ahead:
    /// the rename of the domain changes only its name, so the structural
    /// allowance holds (DEC-1635.8).
    #[test]
    fn a_renamed_domain_is_still_one_the_plan_has_yet_to_alter() {
        let from = domain(
            false,
            Some("'a'::app.e"),
            &[("c1", "(VALUE <> 'a'::app.e)")],
        );
        let to = domain(false, Some("'b'"), &[("c1", "VALUE <> 'b'")]);
        let uid = pbps_model::Uid::derived(pbps_model::UidKind::Type, "renamed-labelled", 0);
        let pin = DeclaredTypePin {
            uid: uid.clone(),
            name: "app.renamed".parse().unwrap(),
            definition: to.clone(),
            before: Some(DeclaredTypeBefore {
                name: "app.d".parse().unwrap(),
                definition: from.clone(),
            }),
        };
        let statement = |uid: &pbps_model::Uid, name: &str, definition: ManagedType, altered| {
            let mut statement = pbps_dialect::Statement::new("unused");
            statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
                uid: uid.clone(),
                name: name.parse().unwrap(),
                definition: Some(definition),
                altered,
                after_vet: None,
            });
            statement
        };
        // The domain's rename, then the base enum's label rename.
        let domain_renamed = statement(&uid, "app.renamed", from.clone(), false);
        let label_renamed = statement(
            &pbps_model::Uid::derived(pbps_model::UidKind::Type, "enum", 0),
            "app.e",
            enumeration("b"),
            false,
        );
        let altering = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::AlterDomain {
                    uid: uid.clone(),
                    name: "app.renamed".parse().unwrap(),
                    from: Box::new(from.clone()),
                    to: Box::new(to.clone()),
                    half: pbps_model::DomainHalf::Loosen,
                },
            )],
        };
        let live = |definition: ManagedType| Schema {
            types: [("app.renamed".parse().unwrap(), definition)].into(),
            ..Default::default()
        };
        let check = |live: &Schema,
                     changes: &pbps_model::ChangeSet,
                     executed: &[pbps_dialect::Statement]| {
            check_definitions(live, std::slice::from_ref(&pin), false, changes, executed)
        };
        let respelled = live(domain(
            false,
            Some("'b'::app.e"),
            &[("c1", "(VALUE <> 'b'::app.e)")],
        ));
        let both = [domain_renamed.clone(), label_renamed.clone()];
        check(&respelled, &altering, &both).unwrap();
        // The label renamed first and the domain after it comes out the same.
        let reversed = [label_renamed.clone(), domain_renamed.clone()];
        check(&respelled, &altering, &reversed).unwrap();
        // Negatives: no alteration ahead, no enum statement run, and the
        // domain's rename alone leave the respelled text as drift.
        assert!(check(&respelled, &Default::default(), &both).is_err());
        assert!(check(&respelled, &altering, std::slice::from_ref(&domain_renamed)).is_err());
        // A structural change is drift whatever ran: a check name, NOT NULL,
        // the default's presence.
        for wrong in [
            domain(false, Some("'b'::app.e"), &[("c2", "x")]),
            domain(true, Some("'b'::app.e"), &[("c1", "x")]),
            domain(false, None, &[("c1", "x")]),
        ] {
            assert!(check(&live(wrong), &altering, &both).is_err());
        }
    }

    #[test]
    fn renamed_domains_keep_exact_catalog_expressions_after_their_rename() {
        let domain = ManagedType::Domain {
            base: "app.mood".parse().unwrap(),
            not_null: false,
            default: Some("'ok'::app.mood".into()),
            checks: [("allowed".into(), "VALUE <> 'sad'::app.mood".into())].into(),
            collation: None,
        };
        let mut pins = [
            renamed_pin("mood", "app.mood", "app.feeling", enumeration("ok")),
            renamed_pin("domain", "app.kept", "app.renamed", domain.clone()),
        ];
        // The author record can have a different spelling from the catalog;
        // RENAME preserves the catalog tree rather than installing that text.
        let ManagedType::Domain { base, default, .. } = &mut pins[1].definition else {
            unreachable!()
        };
        *base = "app.feeling".parse().unwrap();
        *default = Some("'ok' :: app.mood".into());
        let statements = [
            wrote(&pins[0], "app.feeling"),
            wrote(&pins[1], "app.renamed"),
        ];
        let actual = ManagedType::Domain {
            base: "app.feeling".parse().unwrap(),
            not_null: false,
            default: Some("'ok'::app.feeling".into()),
            checks: [("allowed".into(), "VALUE <> 'sad'::app.feeling".into())].into(),
            collation: None,
        };
        let live = Schema {
            types: [
                ("app.feeling".parse().unwrap(), enumeration("ok")),
                ("app.renamed".parse().unwrap(), actual),
            ]
            .into(),
            ..Default::default()
        };
        check_definitions(&live, &pins, true, &Default::default(), &statements).unwrap();
        for check in [false, true] {
            let mut drift = live.clone();
            let ManagedType::Domain {
                default, checks, ..
            } = drift
                .types
                .get_mut(&"app.renamed".parse().unwrap())
                .unwrap()
            else {
                unreachable!()
            };
            if check {
                checks.insert("allowed".into(), "VALUE IS NOT NULL".into());
            } else {
                *default = Some("'sad'::app.feeling".into());
            }
            assert!(
                check_definitions(&drift, &pins, true, &Default::default(), &statements).is_err()
            );
        }
    }

    #[test]
    fn domain_bases_follow_their_uid_even_when_the_final_name_has_another_holder() {
        let mut pins = vec![
            renamed_pin("a", "app.a", "app.b", enumeration("a")),
            renamed_pin("b", "app.b", "app.c", enumeration("b")),
        ];
        for (seed, from, to) in [("da", "app.a", "app.b"), ("db", "app.b", "app.c")] {
            let domain = |name: &str| ManagedType::Domain {
                base: format!("{name}[]").parse().unwrap(),
                not_null: false,
                default: Some("'{}'".into()),
                checks: Default::default(),
                collation: None,
            };
            let name = format!("app.{seed}");
            let mut pin = renamed_pin(seed, &name, &name, domain(from));
            pin.definition = domain(to);
            pins.push(pin);
        }
        let statements = [wrote(&pins[1], "app.c"), wrote(&pins[0], "app.b")];
        for completed in 0..=2 {
            let executed = &statements[..completed];
            let mut live = Schema::default();
            for pin in &pins {
                let (name, mut definition, _) = {
                    let (name, definition, before) = current(pin, executed).unwrap();
                    (name, definition.clone(), before)
                };
                if let ManagedType::Domain { base, .. } = &mut definition {
                    let base_pin = pins
                        .iter()
                        .find(|p| p.before.as_ref().unwrap().name == *base.type_ref().unwrap())
                        .unwrap();
                    *base = pbps_model::ColumnType::reference(
                        current(base_pin, executed).unwrap().0.clone(),
                    )
                    .into_array();
                }
                live.types.insert(name.clone(), definition);
            }
            check_definitions(&live, &pins, completed == 2, &Default::default(), executed).unwrap();
            let ManagedType::Domain { default, .. } =
                live.types.get_mut(&"app.da".parse().unwrap()).unwrap()
            else {
                unreachable!()
            };
            *default = Some("'manual'".into());
            assert!(check_definitions(&live, &pins, false, &Default::default(), executed).is_err());
        }
    }

    #[test]
    fn a_new_uid_does_not_adopt_the_definition_of_a_name_predecessor() {
        let old = renamed_pin("old", "app.x", "app.x", enumeration("old"));
        let new = renamed_pin("new", "app.x", "app.x", enumeration("new"));
        let base = Schema {
            types: [(old.name.clone(), old.definition.clone())].into(),
            ..Default::default()
        };
        let desired = Schema {
            types: [(new.name.clone(), new.definition.clone())].into(),
            ..Default::default()
        };
        let base_ids = IdsFile {
            types: [(old.uid.clone(), old.name.clone())].into(),
            ..Default::default()
        };
        let ids = IdsFile {
            types: [(new.uid.clone(), new.name.clone())].into(),
            ..Default::default()
        };
        let pins = seal(&base, &base_ids, &desired, &ids);
        assert!(pins[0].before.is_none());
        let changes = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::DropType {
                    uid: old.uid.clone(),
                    name: old.name.clone(),
                    definition: Box::new(old.definition.clone()),
                },
            )],
        };
        check_definitions(&base, &pins, false, &changes, &[]).unwrap();
        let mut drop = wrote(&old, "app.x");
        drop.declared_type.as_mut().unwrap().definition = None;
        assert!(check_definitions(&base, &pins, false, &changes, &[drop.clone()]).is_err());
        check_definitions(&Schema::default(), &pins, false, &changes, &[drop.clone()]).unwrap();
        let executed = [drop, wrote(&new, "app.x")];
        check_definitions(&desired, &pins, true, &changes, &executed).unwrap();
        assert!(check_definitions(&base, &pins, false, &changes, &executed).is_err());
    }

    #[test]
    fn an_intermediate_name_held_by_another_uid_does_not_certify_a_schema_move() {
        let pins = [
            renamed_pin("a", "app.a", "dst.b", enumeration("a")),
            renamed_pin("b", "dst.a", "dst.c", enumeration("b")),
        ];
        let statements = [
            wrote(&pins[1], "dst.c"),
            wrote(&pins[0], "dst.a"),
            wrote(&pins[0], "dst.b"),
        ];
        let mut live = Schema {
            types: [
                ("app.a".parse().unwrap(), enumeration("a")),
                ("dst.a".parse().unwrap(), enumeration("b")),
            ]
            .into(),
            ..Default::default()
        };
        for completed in 0..=statements.len() {
            check_definitions(
                &live,
                &pins,
                completed == statements.len(),
                &Default::default(),
                &statements[..completed],
            )
            .unwrap();
            if let Some(step) = statements
                .get(completed)
                .and_then(|s| s.declared_type.as_ref())
            {
                let pin = pins.iter().find(|p| p.uid == step.uid).unwrap();
                let name = current(pin, &statements[..completed]).unwrap().0;
                let definition = live.types.remove(name).unwrap();
                live.types.insert(step.name.clone(), definition);
            }
        }
    }

    #[test]
    fn a_table_predecessor_is_checked_for_absence_after_release_and_vetted_after_creation() {
        let mut pin = renamed_pin("new", "app.x", "app.x", enumeration("new"));
        pin.before = None;
        let table = pbps_model::TableName::new("app", "x");
        let changes = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::DropTable {
                    uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "x", 0),
                    name: table.clone(),
                    detach_from: None,
                },
            )],
        };
        let live = Schema {
            tables: [(table, Default::default())].into(),
            ..Default::default()
        };
        assert!(
            check_definitions(&live, std::slice::from_ref(&pin), false, &changes, &[])
                .unwrap()
                .1
                .is_empty()
        );
        assert_eq!(
            check_definitions(
                &Schema::default(),
                std::slice::from_ref(&pin),
                false,
                &changes,
                &[]
            )
            .unwrap()
            .1,
            vec![pin.name.clone()]
        );
        let created = wrote(&pin, "app.x");
        let mut live = Schema {
            types: [(pin.name.clone(), pin.definition.clone())].into(),
            ..Default::default()
        };
        check_definitions(
            &live,
            std::slice::from_ref(&pin),
            false,
            &changes,
            std::slice::from_ref(&created),
        )
        .unwrap();
        live.types.insert(pin.name.clone(), enumeration("manual"));
        assert!(check_definitions(&live, &[pin], false, &changes, &[created]).is_err());
    }

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

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)] // Non-label fixtures are unreachable.
mod label_tests {
    use super::*;
    use pbps_model::{Change, ChangeSet, EnumPlacement, PlannedChange};

    fn enumeration(values: &[&str]) -> ManagedType {
        ManagedType::Enum {
            values: values.iter().map(|v| (*v).into()).collect(),
        }
    }

    #[test]
    fn enum_pins_accept_every_executed_definition_and_refuse_skipped_steps_and_drift() {
        let uid = "y_000001".parse().unwrap();
        let name: TypeRef = "app.e".parse().unwrap();
        let pin = DeclaredTypePin {
            uid,
            name: name.clone(),
            definition: enumeration(&["blue", "x", "ok", "y"]),
            before: Some(DeclaredTypeBefore {
                name: name.clone(),
                definition: enumeration(&["sad", "ok"]),
            }),
        };
        let changes = ChangeSet {
            changes: vec![
                Change::RenameEnumValue {
                    uid: pin.uid.clone(),
                    name: name.clone(),
                    from: "sad".into(),
                    to: "blue".into(),
                    ordinal: 0,
                    definition: Box::new(enumeration(&["blue", "ok"])),
                },
                Change::AddEnumValue {
                    uid: pin.uid.clone(),
                    name: name.clone(),
                    value: "x".into(),
                    placement: Some(EnumPlacement::After("blue".into())),
                    ordinal: 1,
                    definition: Box::new(enumeration(&["blue", "x", "ok"])),
                },
                Change::AddEnumValue {
                    uid: pin.uid.clone(),
                    name: name.clone(),
                    value: "y".into(),
                    placement: Some(EnumPlacement::After("ok".into())),
                    ordinal: 2,
                    definition: Box::new(pin.definition.clone()),
                },
            ]
            .into_iter()
            .map(PlannedChange::new)
            .collect(),
        };
        let mut live = Schema {
            types: [(
                name.clone(),
                pin.before.as_ref().unwrap().definition.clone(),
            )]
            .into(),
            ..Default::default()
        };
        let mut executed = Vec::new();
        check_definitions(
            &live,
            std::slice::from_ref(&pin),
            false,
            &changes,
            &executed,
        )
        .unwrap();
        assert!(
            check_definitions(&live, std::slice::from_ref(&pin), true, &changes, &executed)
                .is_err()
        );
        for (i, step) in changes.changes.iter().enumerate() {
            let definition = match &step.change {
                Change::AddEnumValue { definition, .. }
                | Change::RenameEnumValue { definition, .. } => definition.as_ref(),
                _ => unreachable!(),
            };
            live.types.insert(name.clone(), definition.clone());
            assert!(
                check_definitions(
                    &live,
                    std::slice::from_ref(&pin),
                    false,
                    &changes,
                    &executed
                )
                .is_err(),
                "unexecuted step {i}"
            );
            let mut statement = pbps_dialect::Statement::new("metadata only");
            statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
                uid: pin.uid.clone(),
                name: name.clone(),
                definition: Some(definition.clone()),
                altered: false,
                after_vet: None,
            });
            executed.push(statement);
            check_definitions(
                &live,
                std::slice::from_ref(&pin),
                i == 2,
                &changes,
                &executed,
            )
            .unwrap();
            // A resumed check carries the same committed prefix and evidence.
            check_definitions(
                &live,
                std::slice::from_ref(&pin),
                i == 2,
                &changes,
                &executed.clone(),
            )
            .unwrap();
            let mut drift = live.clone();
            drift.types.insert(name.clone(), enumeration(&["rogue"]));
            assert!(
                check_definitions(
                    &drift,
                    std::slice::from_ref(&pin),
                    false,
                    &changes,
                    &executed
                )
                .is_err()
            );
        }
    }

    #[test]
    fn a_rebuilds_swap_is_accepted_at_every_boundary_and_each_drift_or_skip_is_refused() {
        let uid: pbps_model::Uid = "y_000001".parse().unwrap();
        let name: TypeRef = "app.e".parse().unwrap();
        let tmp: TypeRef = "app.pbps_rebuild_y_000001".parse().unwrap();
        let pin = DeclaredTypePin {
            uid: uid.clone(),
            name: name.clone(),
            definition: enumeration(&["ok"]),
            before: Some(DeclaredTypeBefore {
                name: name.clone(),
                definition: enumeration(&["sad", "ok"]),
            }),
        };
        let none = pbps_model::ChangeSet::default();
        let certify = |at: &TypeRef| {
            let mut statement = pbps_dialect::Statement::new("metadata only");
            statement.declared_type = Some(pbps_dialect::DeclaredTypeStatement {
                uid: uid.clone(),
                name: at.clone(),
                definition: Some(pin.definition.clone()),
                altered: false,
                after_vet: None,
            });
            statement
        };
        let (dropped, renamed) = (certify(&tmp), certify(&name));
        let live = |types: &[(&TypeRef, ManagedType)]| Schema {
            types: types
                .iter()
                .map(|(n, d)| ((*n).clone(), d.clone()))
                .collect(),
            ..Default::default()
        };
        let pins = std::slice::from_ref(&pin);
        // Before any swap statement: the old type stands, and the replacement
        // and the retyped columns are nobody's declared evidence.
        let standing = live(&[
            (&name, enumeration(&["sad", "ok"])),
            (&tmp, enumeration(&["ok"])),
        ]);
        check_definitions(&standing, pins, false, &none, &[]).unwrap();
        // After the drop: the uid lives at the replacement, whose labels are the final ones.
        let moved = live(&[(&tmp, enumeration(&["ok"]))]);
        check_definitions(&moved, pins, false, &none, std::slice::from_ref(&dropped)).unwrap();
        // After the rename: complete.
        let done = live(&[(&name, enumeration(&["ok"]))]);
        let both = [dropped.clone(), renamed.clone()];
        check_definitions(&done, pins, true, &none, &both).unwrap();
        // A resumed check carries the same committed prefix.
        check_definitions(&done, pins, true, &none, &both.clone()).unwrap();
        // The negatives: the replacement holding other labels, a plan called complete before the
        // rename, and an old type that drifted before any statement.
        let drifted = live(&[(&tmp, enumeration(&["rogue"]))]);
        assert!(
            check_definitions(&drifted, pins, false, &none, std::slice::from_ref(&dropped))
                .is_err()
        );
        assert!(
            check_definitions(&moved, pins, true, &none, std::slice::from_ref(&dropped)).is_err()
        );
        let rogue = live(&[(&name, enumeration(&["rogue"]))]);
        assert!(check_definitions(&rogue, pins, false, &none, &[]).is_err());
    }
}
