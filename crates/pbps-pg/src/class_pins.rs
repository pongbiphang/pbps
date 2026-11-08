//! The pins of the operator classes a plan's indexes name (DEC-2447.14).
//!
//! E's route for a type, run for a class through the kernel in
//! [`crate::provider`]: seal at plan, check before every statement, a
//! placeholder for a class of a package the plan itself brings and the proof of
//! it after the rehearsal. What is a class's own is how one is read
//! ([`crate::class_read`]), where a plan names one (an index or exclusion key),
//! and the two refusals only a class has: the default class spelled out and a
//! class for another type.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError};
use pbps_model::escape::Escaped;
use pbps_model::{
    Change, ChangeSet, ColumnType, IndexColumn, IndexKey, ProviderKind, ProviderMember,
    ProviderPin, Schema, Table, TypeProvider,
};

use crate::class_read::{self, Class, ClassRead, Wanted};
use crate::pins::Within;
use crate::provider::{self, Noun, Standing};

/// The words of a refusal about a class.
pub const CLASS: Noun = Noun {
    object: "operator class",
    consumer: "an index",
};

fn failed(why: impl Into<String>) -> DbError {
    DbError::BadRow(why.into())
}

/// One place a qualified class is named: the class and the type of the column
/// it is the key of, where the key is a plain column.
#[derive(Debug, Clone)]
pub struct Use {
    pub wanted: Wanted,
    pub column: Option<ColumnType>,
    pub shown: String,
}

fn uses_in_keys(keys: &[IndexColumn], method: &str, table: &Table, out: &mut Vec<Use>) {
    for key in keys {
        let Some(class) = &key.opclass else { continue };
        let Some(schema) = class.schema() else {
            continue;
        };
        out.push(Use {
            wanted: Wanted {
                method: method.to_owned(),
                schema: schema.to_owned(),
                name: class.name().to_owned(),
            },
            column: match &key.key {
                IndexKey::Column(c) => table.columns.get(c).map(|c| c.ty.clone()),
                IndexKey::Expression(_) => None,
            },
            shown: class.shown().to_string(),
        });
    }
}

/// The qualified classes a table's indexes and exclusions name.
pub(crate) fn uses_in_table(table: &Table, out: &mut Vec<Use>) {
    for index in table.indexes.values() {
        uses_in_keys(&index.columns, index.method.as_str(), table, out);
    }
    for constraint in table.exclude.values() {
        let keys: Vec<_> = constraint.keys.iter().map(|k| k.key.clone()).collect();
        uses_in_keys(&keys, constraint.method.as_str(), table, out);
    }
}

/// Every qualified class the desired schema names. The last schema is the
/// desired one; a class only the base names belongs to an index the plan drops.
#[must_use]
pub fn uses(schemas: &[&Schema]) -> Vec<Use> {
    let mut out = Vec::new();
    if let Some(desired) = schemas.last() {
        desired
            .tables
            .values()
            .for_each(|t| uses_in_table(t, &mut out));
    }
    out
}

/// The classes the changes of a plan name, which a sealed plan must carry a pin
/// for (the check refuses a plan that names one it has no pin for).
// Only these three changes carry an index, or a table with indexes.
#[allow(clippy::wildcard_enum_match_arm)]
fn named_by(changes: &ChangeSet) -> BTreeSet<Wanted> {
    let mut out = Vec::new();
    for planned in &changes.changes {
        match &planned.change {
            Change::AddIndex { index, .. } => {
                uses_in_keys(
                    &index.columns,
                    index.method.as_str(),
                    &Table::default(),
                    &mut out,
                );
            }
            Change::CreateTable { table, .. } => uses_in_table(table, &mut out),
            Change::BuildPartitionedIndex(build) => {
                if let pbps_model::PartitionIndexDefinition::Index(index) = &build.definition {
                    uses_in_keys(
                        &index.columns,
                        index.method.as_str(),
                        &Table::default(),
                        &mut out,
                    );
                }
            }
            _ => {}
        }
    }
    out.into_iter().map(|u| u.wanted).collect()
}

fn pin(class: &Class) -> ProviderPin {
    ProviderPin {
        kind: ProviderKind::Class,
        schema: class.wanted.schema.clone(),
        name: class.wanted.name.clone(),
        method: class.wanted.method.clone(),
        oid: class.oid,
        digest: class.digest.clone(),
        planned: false,
        provider: class
            .extension
            .as_ref()
            .map(|(extension, version)| TypeProvider {
                extension: extension.clone(),
                version: version.clone(),
                moved_to: None,
            }),
    }
}

fn placeholder(wanted: &Wanted, provider: TypeProvider) -> ProviderPin {
    ProviderPin {
        kind: ProviderKind::Class,
        schema: wanted.schema.clone(),
        name: wanted.name.clone(),
        method: wanted.method.clone(),
        oid: 0,
        digest: String::new(),
        planned: true,
        provider: Some(provider),
    }
}

fn wanted_of(pin: &ProviderPin) -> Wanted {
    Wanted {
        method: pin.method.clone(),
        schema: pin.schema.clone(),
        name: pin.name.clone(),
    }
}

impl<'a> From<&'a ProviderPin> for Standing<'a> {
    fn from(pin: &'a ProviderPin) -> Self {
        Standing {
            schema: &pin.schema,
            name: &pin.name,
            planned: pin.planned,
            provider: pin.provider.as_ref(),
        }
    }
}

/// Types the class cannot take for certain: a plain scalar column under a class
/// for another concrete type. The engine's own answer ("does not accept data
/// type") comes only at the statement; this names it before any, and says
/// nothing for a type it cannot be sure of (a domain, an array, a polymorphic
/// class, a `varchar` under a `text` class, which is binary-coercible).
fn type_mismatch(class: &Class, column: &ColumnType) -> Option<String> {
    const SCALARS: &[&str] = &[
        "smallint",
        "integer",
        "bigint",
        "boolean",
        "text",
        "uuid",
        "jsonb",
        "json",
        "numeric",
        "real",
        "double precision",
        "date",
        "timestamp without time zone",
        "timestamp with time zone",
    ];
    let normal = crate::types::normalize(column).ok()?;
    if normal.is_array() || class.input_type.starts_with("any") {
        return None;
    }
    let base = normal.base.to_string();
    (SCALARS.contains(&base.as_str())
        && SCALARS.contains(&class.input_type.as_str())
        && base != class.input_type)
        .then_some(base)
}

/// What a key may not do with a class that stands: spell out the default the
/// catalog cannot tell from the omission, or name a class for another type.
fn refuse_use(class: &Class, used: &Use) -> Result<(), String> {
    if class.is_default {
        return Err(format!(
            "the operator class `{}` is the default for `{}` under {}, and the catalog \
             prints a default and its omission alike; leave the class out of the key",
            used.shown,
            Escaped(&class.input_type),
            Escaped(&class.wanted.method)
        ));
    }
    if let Some(column) = &used.column
        && let Some(base) = type_mismatch(class, column)
    {
        return Err(format!(
            "the operator class `{}` is for `{}` and the key is a `{}` column; the engine \
             would refuse it at the statement",
            used.shown,
            Escaped(&class.input_type),
            Escaped(&base)
        ));
    }
    Ok(())
}

/// Reads and checks every class the desired schema names; a class missing from
/// the target in the schema of exactly one declared package is that package's
/// placeholder (DEC-2447.12).
pub async fn seal(
    conn: &mut Conn,
    schemas: &[&Schema],
    changes: &ChangeSet,
) -> Result<Vec<ProviderPin>, DbError> {
    let uses = uses(schemas);
    let wanted: Vec<Wanted> = uses
        .iter()
        .map(|u| u.wanted.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let declared = provider::packages_by_schema(schemas);
    let reads = class_read::read(conn, Within::OwnTransaction, &wanted).await;
    let mut pins = Vec::new();
    let mut classes = Vec::new();
    for (one, read) in wanted.iter().zip(&reads) {
        let shown = format!("{}.{}", Escaped(&one.schema), Escaped(&one.name));
        match read {
            ClassRead::Absent if declared.contains_key(one.schema.as_str()) => {
                let package = provider::defer_to_package(&declared, &one.schema, &shown, CLASS)
                    .expect("the schema holds a declared package")
                    .map_err(failed)?;
                pins.push(placeholder(one, package));
            }
            ClassRead::Present(class) => {
                if let Some(why) = read.refusal(one) {
                    return Err(failed(why));
                }
                let (extension, version) = class
                    .extension
                    .as_ref()
                    .expect("a refusal-free class has a package");
                provider::check_package(schemas, extension, version, &shown, CLASS)
                    .map_err(failed)?;
                classes.push(class);
                pins.push(pin(class));
            }
            absent_or_unreadable @ (ClassRead::Absent | ClassRead::Unreadable(_)) => {
                return Err(failed(
                    absent_or_unreadable
                        .refusal(one)
                        .expect("a non-present class refuses"),
                ));
            }
        }
    }
    for used in &uses {
        if let Some(class) = classes.iter().find(|c| c.wanted == used.wanted) {
            refuse_use(class, used).map_err(failed)?;
        }
    }
    provider::refuse_update_beside_provided(
        |extension| {
            classes
                .iter()
                .find(|c| c.extension.as_ref().is_some_and(|(e, _)| e == extension))
                .map(|c| format!("{}.{}", Escaped(&c.wanted.schema), Escaped(&c.wanted.name)))
        },
        changes,
        CLASS,
    )
    .map_err(failed)?;
    for pin in pins.iter_mut().filter(|p| !p.planned) {
        if let Some(package) = &mut pin.provider {
            package.moved_to = provider::moved_to(&package.extension, &pin.schema, changes);
        }
    }
    Ok(pins)
}

/// The proof after the rehearsal for each placeholder: the package the plan
/// creates, or moves and updates, must list that exact class among its sealed
/// members, so a class of the same spelling from anywhere else does not stand
/// in for the one the order assumed (DEC-2447.12).
pub fn proof(
    pins: &[ProviderPin],
    created: &[pbps_model::ExtensionPin],
    changed: &[pbps_model::ExtensionChangePin],
) -> Result<(), String> {
    for pin in pins.iter().filter(|p| p.planned) {
        let member = ProviderMember {
            class: "operator class".to_owned(),
            names: vec![pin.method.clone(), pin.schema.clone(), pin.name.clone()],
            args: Vec::new(),
        };
        provider::proof(Standing::from(pin), &member, CLASS, created, changed)?;
    }
    Ok(())
}

/// A pin read again is the pin that was sealed, except that a class its
/// package's move carried is in the schema the plan said it would be.
fn same(sealed: &ProviderPin, now: &ProviderPin) -> bool {
    provider::same_pin(
        sealed,
        now,
        |p| {
            (
                p.schema.clone(),
                p.provider.as_ref().and_then(|x| x.moved_to.clone()),
            )
        },
        |p| {
            let mut p = p.clone();
            if let Some(x) = &mut p.provider {
                x.moved_to = None;
            }
            p
        },
        |p, schema| ProviderPin {
            schema: schema.to_owned(),
            ..p
        },
    )
}

/// Refuses a sealed plan whose classes are not what it was sealed over: one the
/// plan names with no pin, a class that is gone, or one that changed.
pub async fn check(
    conn: &mut Conn,
    within: Within,
    expected: &[ProviderPin],
    changes: &ChangeSet,
) -> Result<(), DbError> {
    let pinned: BTreeSet<Wanted> = expected.iter().map(wanted_of).collect();
    for wanted in named_by(changes) {
        if !pinned.contains(&wanted) {
            return Err(failed(format!(
                "plan has no pin for the operator class `{}.{}`; plan again against the database",
                Escaped(&wanted.schema),
                Escaped(&wanted.name)
            )));
        }
    }
    let live: Vec<&ProviderPin> = expected
        .iter()
        .filter(|p| p.kind == ProviderKind::Class && !p.planned)
        .collect();
    // Where each class is now: under its own name or the name its package's
    // move gave it; it keeps its OID, so exactly one is there.
    let mut candidates = Vec::new();
    for pin in &live {
        candidates.push(wanted_of(pin));
        if let Some(schema) = pin.provider.as_ref().and_then(|p| p.moved_to.as_deref()) {
            candidates.push(Wanted {
                schema: schema.to_owned(),
                ..wanted_of(pin)
            });
        }
    }
    let reads: BTreeMap<Wanted, ClassRead> = candidates
        .iter()
        .cloned()
        .zip(class_read::read(conn, within, &candidates).await)
        .collect();
    for pin in live {
        let own = wanted_of(pin);
        let moved = pin
            .provider
            .as_ref()
            .and_then(|p| p.moved_to.as_deref())
            .map(|schema| Wanted {
                schema: schema.to_owned(),
                ..own.clone()
            });
        let now = moved
            .as_ref()
            .and_then(|m| reads.get(m).filter(|r| matches!(r, ClassRead::Present(_))))
            .or_else(|| reads.get(&own));
        let changed = || {
            failed(format!(
                "the operator class `{}.{}` changed since planning; plan again",
                Escaped(&pin.schema),
                Escaped(&pin.name)
            ))
        };
        match now {
            Some(ClassRead::Present(class)) if same(pin, &self::pin(class)) => {}
            _ => return Err(changed()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(input: &str) -> Class {
        Class {
            wanted: Wanted {
                method: "gin".into(),
                schema: "ext".into(),
                name: "gin_trgm_ops".into(),
            },
            oid: 1,
            is_default: false,
            input_type: input.into(),
            extension: Some(("pg_trgm".into(), "1.6".into())),
            family_extension: Some("pg_trgm".into()),
            digest: "d".into(),
        }
    }

    fn column(text: &str) -> ColumnType {
        text.parse().unwrap()
    }

    #[test]
    fn a_scalar_under_a_class_for_another_scalar_is_named_and_a_coercible_one_is_not() {
        let trgm = class("text");
        assert_eq!(
            type_mismatch(&trgm, &column("integer")).as_deref(),
            Some("integer")
        );
        assert_eq!(type_mismatch(&trgm, &column("text")), None);
        // Binary-coercible to `text`: the engine takes it.
        assert_eq!(type_mismatch(&trgm, &column("varchar(20)")), None);
        // An array, or a class over any type, is the engine's to answer.
        assert_eq!(type_mismatch(&trgm, &column("integer[]")), None);
        assert_eq!(type_mismatch(&class("anyarray"), &column("integer")), None);
    }

    #[test]
    fn a_class_no_package_owns_is_refused_whatever_its_name() {
        let wanted = class("text").wanted;
        let mut lookalike = class("text");
        lookalike.extension = None;
        let refusal = ClassRead::Present(Box::new(lookalike))
            .refusal(&wanted)
            .unwrap();
        assert!(
            refusal.contains("not a member of any extension"),
            "{refusal}"
        );
        let mut other_family = class("text");
        other_family.family_extension = Some("other".into());
        assert!(
            ClassRead::Present(Box::new(other_family))
                .refusal(&wanted)
                .is_some()
        );
        assert_eq!(
            ClassRead::Present(Box::new(class("text"))).refusal(&wanted),
            None
        );
    }

    fn used(class: &Class, column: &str) -> Use {
        Use {
            wanted: class.wanted.clone(),
            column: Some(column.parse().unwrap()),
            shown: "ext.gin_trgm_ops".into(),
        }
    }

    #[test]
    fn a_default_class_spelled_out_and_a_class_for_another_type_are_refused_by_name() {
        let mut trgm = class("text");
        assert_eq!(refuse_use(&trgm, &used(&trgm, "text")), Ok(()));
        let why = refuse_use(&trgm, &used(&trgm, "integer")).unwrap_err();
        assert!(why.contains("is for `text`"), "{why}");
        trgm.is_default = true;
        let why = refuse_use(&trgm, &used(&trgm, "text")).unwrap_err();
        assert!(why.contains("leave the class out"), "{why}");
    }

    /// A placeholder is proved by the exact `{method, schema, name}` member of the
    /// package the plan creates, and by nothing that only shares a spelling.
    #[test]
    fn a_class_placeholder_is_proved_by_the_exact_member_of_the_package() {
        let pin = placeholder(
            &class("text").wanted,
            TypeProvider {
                extension: "pg_trgm".into(),
                version: "1.6".into(),
                moved_to: None,
            },
        );
        let manifest = |members: &[[&str; 3]]| pbps_model::ExtensionPin {
            uid: "e_aaaaaa".parse().unwrap(),
            name: "pg_trgm".into(),
            definition: pbps_model::Extension {
                schema: "ext".into(),
                version: "1.6".into(),
            },
            manifest: pbps_model::ExtensionProvider {
                definition: pbps_model::Extension {
                    schema: "ext".into(),
                    version: "1.6".into(),
                },
                requires: Default::default(),
                members: members
                    .iter()
                    .map(|m| ProviderMember {
                        class: "operator class".into(),
                        names: m.iter().map(|n| (*n).to_owned()).collect(),
                        args: Vec::new(),
                    })
                    .collect(),
            },
        };
        let listed = [manifest(&[["gin", "ext", "gin_trgm_ops"]])];
        assert_eq!(proof(std::slice::from_ref(&pin), &listed, &[]), Ok(()));
        // Negatives: another method, another schema, another name, an empty manifest.
        for other in [
            [["gist", "ext", "gin_trgm_ops"]],
            [["gin", "other", "gin_trgm_ops"]],
            [["gin", "ext", "gist_trgm_ops"]],
        ] {
            let why = proof(std::slice::from_ref(&pin), &[manifest(&other)], &[]).unwrap_err();
            assert!(why.contains("is not a member of the extension"), "{why}");
        }
        assert!(proof(std::slice::from_ref(&pin), &[], &[]).is_err());
        // A pin that is not a placeholder asks nothing of the manifest.
        let mut real = pin;
        real.planned = false;
        assert_eq!(proof(&[real], &[], &[]), Ok(()));
    }
}
