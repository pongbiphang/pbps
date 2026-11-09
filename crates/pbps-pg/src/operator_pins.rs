//! The pins of the operators a plan's exclusion constraints name (DEC-2447.14, #2448 C1b).
//!
//! E's route for a type and G2's for a class, run for an operator through the kernel in
//! [`crate::provider`]: seal at plan, check before every statement, a placeholder for an
//! operator of a package the plan itself brings and the proof of it after the rehearsal.
//! What is an operator's own is how one is read ([`crate::operator_read`]) and where a plan
//! names one: a key's `operator:` of an exclusion constraint, which the engine writes
//! `OPERATOR(schema.symbol)`. An operator of `pg_catalog` is the engine's and needs no pin.

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError};
use pbps_model::{
    Change, ChangeSet, ProviderKind, ProviderMember, ProviderPin, Schema, Table, TypeProvider,
};

use crate::operator_read::{self, Operator, OperatorRead, Wanted};
use crate::pins::Within;
use crate::provider::{self, Noun, PinKind};

/// The words of a refusal about an operator.
pub const OPERATOR: Noun = Noun {
    object: "operator",
    consumer: "an exclusion constraint",
};

/// One place a package's operator is named, and what holds the key.
#[derive(Debug, Clone)]
pub struct Use {
    pub wanted: Wanted,
    pub holder: String,
}

/// The package operators a table's exclusion constraints name.
pub(crate) fn uses_in_table(name: &pbps_model::TableName, table: &Table, out: &mut Vec<Use>) {
    for (constraint, x) in &table.exclude {
        for key in &x.keys {
            let Some(schema) = key.operator.schema() else {
                continue;
            };
            out.push(Use {
                wanted: Wanted {
                    schema: schema.to_owned(),
                    symbol: key.operator.as_symbol().to_owned(),
                },
                holder: format!(
                    "exclusion constraint `{}` on `{}`",
                    constraint.shown(),
                    name.shown()
                ),
            });
        }
    }
}

fn uses(schemas: &[&Schema]) -> Vec<Use> {
    let mut out = Vec::new();
    if let Some(desired) = schemas.last() {
        for (name, table) in &desired.tables {
            uses_in_table(name, table, &mut out);
        }
    }
    out
}

/// The operators the changes of a plan name, which a sealed plan must carry a pin for.
// Only these two changes carry an exclusion, or a table with exclusions.
#[allow(clippy::wildcard_enum_match_arm)]
fn named_by(changes: &ChangeSet) -> BTreeSet<Wanted> {
    let mut out = Vec::new();
    for planned in &changes.changes {
        match &planned.change {
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
            Change::CreateTable { name, table, .. } => uses_in_table(name, table, &mut out),
            _ => {}
        }
    }
    out.into_iter().map(|u| u.wanted).collect()
}

/// A refusal about an operator, with the first constraint that names it.
fn held_by(uses: &[Use], operator: &Wanted, why: String) -> String {
    match uses.iter().find(|u| &u.wanted == operator) {
        Some(used) => format!("{why} (named by {})", used.holder),
        None => why,
    }
}

/// The kind of pin an operator is, for the kernel in [`crate::provider`].
pub(crate) struct OperatorPins;

impl PinKind for OperatorPins {
    type Wanted = Wanted;
    type Item = Operator;
    type Read = OperatorRead;
    type Use = Use;
    const NOUN: Noun = OPERATOR;
    const KIND: ProviderKind = ProviderKind::Operator;

    fn uses(schemas: &[&Schema]) -> Vec<Use> {
        uses(schemas)
    }
    fn used(used: &Use) -> &Wanted {
        &used.wanted
    }
    fn named_by(changes: &ChangeSet) -> BTreeSet<Wanted> {
        named_by(changes)
    }
    async fn read(conn: &mut Conn, within: Within, wanted: &[Wanted]) -> Vec<OperatorRead> {
        operator_read::read(conn, within, wanted).await
    }
    fn present(read: &OperatorRead) -> Option<&Operator> {
        match read {
            OperatorRead::Present(op) => Some(op),
            OperatorRead::Absent | OperatorRead::Unreadable(_) => None,
        }
    }
    fn is_absent(read: &OperatorRead) -> bool {
        matches!(read, OperatorRead::Absent)
    }
    fn refusal(read: &OperatorRead, wanted: &Wanted) -> Option<String> {
        read.refusal(wanted)
    }
    fn item_wanted(op: &Operator) -> &Wanted {
        &op.wanted
    }
    fn extension(op: &Operator) -> Option<&(String, String)> {
        op.extension.as_ref()
    }
    fn pin(op: &Operator) -> ProviderPin {
        ProviderPin {
            kind: ProviderKind::Operator,
            schema: op.wanted.schema.clone(),
            name: op.wanted.symbol.clone(),
            method: String::new(),
            oid: op.oid,
            digest: op.digest.clone(),
            planned: false,
            provider: op
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
            kind: ProviderKind::Operator,
            schema: wanted.schema.clone(),
            name: wanted.symbol.clone(),
            method: String::new(),
            oid: 0,
            digest: String::new(),
            planned: true,
            provider: Some(provider),
        }
    }
    fn wanted_of(pin: &ProviderPin) -> Wanted {
        Wanted {
            schema: pin.schema.clone(),
            symbol: pin.name.clone(),
        }
    }
    fn in_schema(wanted: &Wanted, schema: &str) -> Wanted {
        Wanted {
            schema: schema.to_owned(),
            ..wanted.clone()
        }
    }
    fn qualified(wanted: &Wanted) -> (&str, &str) {
        (&wanted.schema, &wanted.symbol)
    }
    fn held_by(uses: &[Use], wanted: &Wanted, why: String) -> String {
        held_by(uses, wanted, why)
    }
    /// The manifest lists an operator with its operand types, which the pin does not hold, so
    /// any overload of the name stands in.
    fn is_member(pin: &ProviderPin, member: &ProviderMember) -> bool {
        member.class == "operator" && member.names == [pin.schema.clone(), pin.name.clone()]
    }
}

/// Reads and checks every package operator the desired schema names; one missing from the
/// target in the schema of exactly one declared package is that package's placeholder
/// (DEC-2447.12).
pub async fn seal(
    conn: &mut Conn,
    schemas: &[&Schema],
    changes: &ChangeSet,
) -> Result<Vec<ProviderPin>, DbError> {
    provider::seal_kind::<OperatorPins>(conn, schemas, changes).await
}

/// The proof after the rehearsal for each placeholder: the package the plan creates, or
/// moves and updates, must list an operator of exactly that schema and symbol among its
/// sealed members.
pub fn proof(
    pins: &[ProviderPin],
    created: &[pbps_model::ExtensionPin],
    changed: &[pbps_model::ExtensionChangePin],
) -> Result<(), String> {
    provider::proof_kind::<OperatorPins>(pins, created, changed)
}

/// Refuses a sealed plan whose operators are not what it was sealed over: one the plan names
/// with no pin, an operator that is gone, or one that changed.
pub async fn check(
    conn: &mut Conn,
    within: Within,
    expected: &[ProviderPin],
    changes: &ChangeSet,
) -> Result<(), DbError> {
    provider::check_kind::<OperatorPins>(conn, within, expected, changes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{ExclusionConstraint, ExclusionKey, ExclusionTiming, IndexColumn, IndexKey};

    fn exclusion(operators: &[Option<&str>]) -> ExclusionConstraint {
        ExclusionConstraint {
            method: pbps_model::IndexMethod::Gist,
            keys: operators
                .iter()
                .map(|schema| ExclusionKey {
                    key: IndexColumn {
                        key: IndexKey::Column("body".into()),
                        descending: false,
                        opclass: None,
                        collation: None,
                    },
                    operator: match schema {
                        Some(s) => pbps_model::OperatorRef::in_schema(s, "%").unwrap(),
                        None => pbps_model::OperatorRef::symbol("=").unwrap(),
                    },
                })
                .collect(),
            include: vec![],
            filter: None,
            storage_parameters: Default::default(),
            timing: ExclusionTiming::Immediate,
        }
    }

    /// Only a package's operator is named for a pin: the engine's own `pg_catalog` operator
    /// is not, a new constraint and a new table both name theirs, and a change that drops one
    /// names none.
    #[test]
    fn a_plan_names_the_package_operators_of_a_new_exclusion_and_never_the_engines() {
        let table = pbps_model::TableName::new("app", "docs");
        let set = |from, to| Change::SetExclusion {
            table: table.clone(),
            name: "no_dup".into(),
            from,
            to,
        };
        let planned = |c| ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(c)],
        };
        let named = |c| {
            named_by(&planned(c))
                .into_iter()
                .map(|w| format!("{}.{}", w.schema, w.symbol))
                .collect::<Vec<_>>()
        };
        let both = exclusion(&[Some("ext"), None]);
        assert_eq!(named(set(None, Some(Box::new(both.clone())))), ["ext.%"]);
        assert!(named(set(None, Some(Box::new(exclusion(&[None]))))).is_empty());
        assert!(named(set(Some(Box::new(both.clone())), None)).is_empty());
        let mut created = Table::default();
        created.exclude.insert("no_dup".into(), both);
        assert_eq!(
            named(Change::CreateTable {
                uid: "t_000001".parse().unwrap(),
                name: table,
                table: Box::new(created),
                beside_default: None,
                moves_at_most: None,
            }),
            ["ext.%"]
        );
    }

    /// A placeholder is proved only by an operator of that schema and symbol among the
    /// package's sealed members, whatever its operand types; a class, or another schema's
    /// operator of the same symbol, does not stand in.
    #[test]
    fn a_placeholder_is_proved_by_a_member_operator_of_that_schema_and_symbol_only() {
        let pin = OperatorPins::placeholder(
            &Wanted {
                schema: "ext".into(),
                symbol: "%".into(),
            },
            TypeProvider {
                extension: "pg_trgm".into(),
                version: "1.6".into(),
                moved_to: None,
            },
        );
        let member = |class: &str, schema: &str, args: &[&str]| ProviderMember {
            class: class.into(),
            names: vec![schema.into(), "%".into()],
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        };
        let created = |members: Vec<ProviderMember>| {
            vec![pbps_model::ExtensionPin {
                uid: "e_000001".parse().unwrap(),
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
                    members: members.into_iter().collect(),
                },
            }]
        };
        let proved = |members| proof(std::slice::from_ref(&pin), &created(members), &[]);
        // Any overload of the symbol stands in, whatever its operand types.
        proved(vec![member("operator", "ext", &["text", "text"])]).unwrap();
        proved(vec![
            member("operator", "ext", &["text", "text"]),
            member("operator", "ext", &["citext", "citext"]),
        ])
        .unwrap();
        // Negatives: another schema's operator, a member of another class, and no member.
        assert!(proved(vec![member("operator", "other", &["text", "text"])]).is_err());
        assert!(proved(vec![member("operator class", "ext", &[])]).is_err());
        assert!(proved(vec![]).is_err());
        // And a package the plan neither creates nor changes proves nothing.
        assert!(proof(std::slice::from_ref(&pin), &[], &[]).is_err());
    }
}
