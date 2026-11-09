//! The pins of the operator classes a plan's indexes name (DEC-2447.14).
//!
//! E's route for a type, run for a class through the kernel in
//! [`crate::provider`]: seal at plan, check before every statement, a
//! placeholder for a class of a package the plan itself brings and the proof of
//! it after the rehearsal. What is a class's own is how one is read
//! ([`crate::class_read`]), where a plan names one (an index or exclusion key),
//! and the two refusals only a class has: the default class spelled out and a
//! class for another type.

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError};
use pbps_model::escape::Escaped;
use pbps_model::{
    Change, ChangeSet, ColumnType, IndexColumn, IndexKey, ProviderKind, ProviderMember,
    ProviderPin, Schema, Table, TypeProvider,
};

use crate::class_read::{self, Class, ClassRead, Wanted};
use crate::pins::Within;
use crate::provider::{self, Noun, PinKind};

/// The words of a refusal about a class.
pub const CLASS: Noun = Noun {
    object: "operator class",
    consumer: "an index",
};

/// One place a qualified class is named: the class and the type of the column
/// it is the key of, where the key is a plain column.
#[derive(Debug, Clone)]
pub struct Use {
    pub wanted: Wanted,
    pub column: Option<ColumnType>,
    pub shown: String,
    /// What holds the key, as a message names it: "index `ix` on `app.docs`".
    pub holder: String,
}

fn uses_in_keys(
    keys: &[IndexColumn],
    method: &str,
    (holder, table): (String, &Table),
    out: &mut Vec<Use>,
) {
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
            holder: holder.clone(),
        });
    }
}

/// The qualified classes a table's indexes and exclusions name.
pub(crate) fn uses_in_table(name: &pbps_model::TableName, table: &Table, out: &mut Vec<Use>) {
    for (index_name, index) in &table.indexes {
        let holder = format!("index `{}` on `{}`", index_name.shown(), name.shown());
        uses_in_keys(&index.columns, index.method.as_str(), (holder, table), out);
    }
    for (constraint_name, constraint) in &table.exclude {
        let holder = format!(
            "exclusion constraint `{}` on `{}`",
            constraint_name.shown(),
            name.shown()
        );
        let keys: Vec<_> = constraint.keys.iter().map(|k| k.key.clone()).collect();
        uses_in_keys(&keys, constraint.method.as_str(), (holder, table), out);
    }
}

/// Every qualified class the desired schema names. The last schema is the
/// desired one; a class only the base names belongs to an index the plan drops.
#[must_use]
pub fn uses(schemas: &[&Schema]) -> Vec<Use> {
    let mut out = Vec::new();
    if let Some(desired) = schemas.last() {
        for (name, table) in &desired.tables {
            uses_in_table(name, table, &mut out);
        }
    }
    out
}

/// The classes the changes of a plan name, which a sealed plan must carry a pin
/// for (the check refuses a plan that names one it has no pin for).
// Only these four changes carry an index or an exclusion, or a table with them.
#[allow(clippy::wildcard_enum_match_arm)]
fn named_by(changes: &ChangeSet) -> BTreeSet<Wanted> {
    let mut out = Vec::new();
    for planned in &changes.changes {
        match &planned.change {
            Change::AddIndex { index, .. } => {
                uses_in_keys(
                    &index.columns,
                    index.method.as_str(),
                    (String::new(), &Table::default()),
                    &mut out,
                );
            }
            Change::CreateTable { name, table, .. } => uses_in_table(name, table, &mut out),
            // A new exclusion constraint names the classes of its keys as an index does.
            Change::SetExclusion {
                table,
                name,
                to: Some(x),
                ..
            } => {
                let mut one = Table::default();
                one.exclude.insert(name.clone(), (**x).clone());
                uses_in_table(table, &one, &mut out);
            }
            Change::BuildPartitionedIndex(build) => {
                if let pbps_model::PartitionIndexDefinition::Index(index) = &build.definition {
                    uses_in_keys(
                        &index.columns,
                        index.method.as_str(),
                        (String::new(), &Table::default()),
                        &mut out,
                    );
                }
            }
            _ => {}
        }
    }
    out.into_iter().map(|u| u.wanted).collect()
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

/// A refusal about a class, with the first index or constraint that names it, so
/// that a plan which drops a package its declared index still uses says which one.
fn held_by(uses: &[Use], class: &Wanted, why: String) -> String {
    let mut holders: Vec<&str> = Vec::new();
    for used in uses
        .iter()
        .filter(|u| &u.wanted == class && !u.holder.is_empty())
    {
        if !holders.contains(&used.holder.as_str()) {
            holders.push(&used.holder);
        }
    }
    match holders.as_slice() {
        [] => why,
        [only] => format!("{why} (named by {only})"),
        [first, second] => {
            format!("{why} (named by {first} and {second})")
        }
        [first, second, rest @ ..] => {
            format!("{why} (named by {first}, {second} and {} more)", rest.len())
        }
    }
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

/// The kind of pin a class is, for the kernel in [`crate::provider`].
pub(crate) struct ClassPins;

impl PinKind for ClassPins {
    type Wanted = Wanted;
    type Item = Class;
    type Read = ClassRead;
    type Use = Use;
    const NOUN: Noun = CLASS;
    const KIND: ProviderKind = ProviderKind::Class;

    fn uses(schemas: &[&Schema]) -> Vec<Use> {
        uses(schemas)
    }
    fn used(used: &Use) -> &Wanted {
        &used.wanted
    }
    fn named_by(changes: &ChangeSet) -> BTreeSet<Wanted> {
        named_by(changes)
    }
    async fn read(conn: &mut Conn, within: Within, wanted: &[Wanted]) -> Vec<ClassRead> {
        class_read::read(conn, within, wanted).await
    }
    fn present(read: &ClassRead) -> Option<&Class> {
        match read {
            ClassRead::Present(class) => Some(class),
            ClassRead::Absent | ClassRead::Unreadable(_) => None,
        }
    }
    fn is_absent(read: &ClassRead) -> bool {
        matches!(read, ClassRead::Absent)
    }
    fn refusal(read: &ClassRead, wanted: &Wanted) -> Option<String> {
        read.refusal(wanted)
    }
    fn item_wanted(class: &Class) -> &Wanted {
        &class.wanted
    }
    fn extension(class: &Class) -> Option<&(String, String)> {
        class.extension.as_ref()
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
    fn in_schema(wanted: &Wanted, schema: &str) -> Wanted {
        Wanted {
            schema: schema.to_owned(),
            ..wanted.clone()
        }
    }
    fn qualified(wanted: &Wanted) -> (&str, &str) {
        (&wanted.schema, &wanted.name)
    }
    fn held_by(uses: &[Use], wanted: &Wanted, why: String) -> String {
        held_by(uses, wanted, why)
    }
    fn refuse_uses(classes: &[&Class], uses: &[Use]) -> Result<(), String> {
        for used in uses {
            if let Some(class) = classes.iter().find(|c| c.wanted == used.wanted) {
                refuse_use(class, used).map_err(|why| {
                    if used.holder.is_empty() {
                        why
                    } else {
                        format!("{why} (named by {})", used.holder)
                    }
                })?;
            }
        }
        Ok(())
    }
    fn is_member(pin: &ProviderPin, member: &ProviderMember) -> bool {
        member.class == "operator class"
            && member.args.is_empty()
            && member.names == [pin.method.clone(), pin.schema.clone(), pin.name.clone()]
    }
}

/// Reads and checks every class the desired schema names, and every package operator an
/// exclusion constraint names, which are pinned beside them (DEC-2447.14, #2448 C1b): one list
/// in the plan, one set of calls for the engine.
pub async fn seal(
    conn: &mut Conn,
    schemas: &[&Schema],
    changes: &ChangeSet,
) -> Result<Vec<ProviderPin>, DbError> {
    let mut pins = provider::seal_kind::<ClassPins>(conn, schemas, changes).await?;
    pins.extend(crate::operator_pins::seal(conn, schemas, changes).await?);
    Ok(pins)
}

/// The proof after the rehearsal for each placeholder of a class or an operator
/// (DEC-2447.12).
pub fn proof(
    pins: &[ProviderPin],
    created: &[pbps_model::ExtensionPin],
    changed: &[pbps_model::ExtensionChangePin],
) -> Result<(), String> {
    crate::operator_pins::proof(pins, created, changed)?;
    provider::proof_kind::<ClassPins>(pins, created, changed)
}

/// Refuses a sealed plan whose classes or operators are not what it was sealed over.
pub async fn check(
    conn: &mut Conn,
    within: Within,
    expected: &[ProviderPin],
    changes: &ChangeSet,
) -> Result<(), DbError> {
    crate::operator_pins::check(conn, within, expected, changes).await?;
    provider::check_kind::<ClassPins>(conn, within, expected, changes).await
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

    /// A new exclusion constraint names the classes of its keys as an index does (#2448 C1b):
    /// a package's class is pinned, an engine's or absent class is not, and a constraint the
    /// plan drops names none.
    #[test]
    fn a_new_exclusion_names_the_package_classes_of_its_keys() {
        let key = |class: Option<&str>| pbps_model::ExclusionKey {
            key: IndexColumn {
                key: IndexKey::Column("body".into()),
                descending: false,
                opclass: class.map(|c| c.parse().unwrap()),
                collation: None,
            },
            operator: pbps_model::OperatorRef::symbol("=").unwrap(),
        };
        let exclusion = |classes: &[Option<&str>]| pbps_model::ExclusionConstraint {
            method: pbps_model::IndexMethod::Gist,
            keys: classes.iter().map(|c| key(*c)).collect(),
            include: vec![],
            filter: None,
            storage_parameters: Default::default(),
            timing: pbps_model::ExclusionTiming::Immediate,
        };
        let set = |from, to| ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(Change::SetExclusion {
                table: pbps_model::TableName::new("app", "docs"),
                name: "no_similar".into(),
                from,
                to,
            })],
        };
        let added = exclusion(&[Some("ext.gist_trgm_ops"), None]);
        let wanted = named_by(&set(None, Some(Box::new(added.clone()))));
        assert_eq!(wanted.len(), 1);
        let one = wanted.iter().next().unwrap();
        assert_eq!(
            (one.method.as_str(), one.schema.as_str(), one.name.as_str()),
            ("gist", "ext", "gist_trgm_ops")
        );
        assert!(named_by(&set(None, Some(Box::new(exclusion(&[None]))))).is_empty());
        assert!(named_by(&set(Some(Box::new(added)), None)).is_empty());
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
            holder: String::new(),
        }
    }

    /// A refusal about a class names the first index or constraint that uses it, so
    /// that dropping a package its declared index still names says which index.
    #[test]
    fn a_refusal_names_the_index_that_uses_the_class() {
        let wanted = class("text").wanted;
        let uses = [Use {
            holder: "index `docs_trgm` on `app.docs`".into(),
            ..used(&class("text"), "text")
        }];
        let named = held_by(&uses, &wanted, "no.".into());
        assert_eq!(named, "no. (named by index `docs_trgm` on `app.docs`)");
        // Every holder is named, the first two in full and the rest counted.
        let more = |names: &[&str]| -> Vec<Use> {
            names
                .iter()
                .map(|n| Use {
                    holder: format!("index `{n}`"),
                    ..used(&class("text"), "text")
                })
                .collect()
        };
        assert_eq!(
            held_by(&more(&["a", "b"]), &wanted, "no.".into()),
            "no. (named by index `a` and index `b`)"
        );
        assert_eq!(
            held_by(&more(&["a", "b", "c", "d"]), &wanted, "no.".into()),
            "no. (named by index `a`, index `b` and 2 more)"
        );
        // The same holder twice (two keys of one index) is named once.
        assert_eq!(
            held_by(&more(&["a", "a"]), &wanted, "no.".into()),
            "no. (named by index `a`)"
        );
        // Negative: a class no use names, and a use with no holder, add nothing.
        let other = Wanted {
            name: "other_ops".into(),
            ..wanted.clone()
        };
        assert_eq!(held_by(&uses, &other, "no.".into()), "no.");
        assert_eq!(
            held_by(&[used(&class("text"), "text")], &wanted, "no.".into()),
            "no."
        );
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
        let pin = ClassPins::placeholder(
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
