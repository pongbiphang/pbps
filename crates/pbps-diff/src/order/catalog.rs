//! Pure facts supplied by the connected engine; no catalog reads live here.
use pbps_model::{ModuleId, Schema, TableName, ValueSource};

pub mod graph;

/// What a dependent is, in terms this project can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holds {
    /// A module: a view, a routine or a trigger, keyed the way the
    /// declarations key it.
    Module(ModuleId),
    /// A part of a table: a check constraint, a column default or an index.
    /// Managed when the table declares that part, and dropped and restored
    /// around the rebuild when it is.
    TablePart { table: TableName, part: Part },
    /// Something no declaration can hold, whatever this project declares.
    ///
    /// "Managed" means **restorable around the rebuild**, and one of the four
    /// objects ADR-0009 §4 measured is not: a generated column. `Column` holds
    /// its expression since DEC-1168.1, but no statement takes the expression
    /// off and puts it back — `DROP EXPRESSION` leaves an ordinary column, and
    /// `SET EXPRESSION` is refused on one — so promising to restore it would
    /// be promising to emit a statement pbps cannot write. An expression index was the second until an index
    /// key could be an expression (DEC-1169.2): a declared one is now a table
    /// part like any other index, and one the model still cannot hold is
    /// never declared, so it is refused as unmanaged.
    Unrepresentable(String),
}

/// Which part of a table a dependent is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// The observed source key, including its actual constraint name.
    PrimaryKey(pbps_model::PrimaryKey),
    Unique(pbps_model::ObjectLocalName),
    ForeignKey(pbps_model::ObjectLocalName),
    Check(pbps_model::ObjectLocalName),
    Default(pbps_model::ColumnName),
    Index(pbps_model::ObjectLocalName),
    /// A generated column's expression. Never restorable: no statement takes
    /// the expression off and puts it back. A plan that changes the
    /// expression releases the dependency instead, when that change runs
    /// before the module's drop (DEC-1168.1).
    Generated(pbps_model::ColumnName),
    /// Never automatically rebuilt until #1173. A whole table drop owns it.
    Policy(String),
}

/// One object that depends on a module, established by the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependent {
    /// `pg_describe_object`'s own words, which is what a refusal shows: the
    /// engine names the object better than this crate can, and the same text
    /// appears in the `DETAIL` of the refusal the engine would give.
    pub described: String,
    pub holds: Holds,
}

impl Dependent {
    /// Whether the declarations hold this, so that a plan could drop it before
    /// the rebuild and put it back after.
    #[must_use]
    pub fn managed(&self, declared: &Schema) -> bool {
        match &self.holds {
            Holds::Module(id) => declared.modules.contains_key(id),
            Holds::TablePart { table, part } => declared.tables.get(table).is_some_and(|t| {
                match part {
                    Part::PrimaryKey(_) => t.primary_key.is_some(),
                    Part::Unique(name) => t.unique.contains_key(name),
                    Part::ForeignKey(name) => t.foreign_keys.contains_key(name),
                    Part::Check(name) => t.checks.contains_key(name),
                    Part::Index(name) => t.indexes.contains_key(name),
                    // A default is the column's, and the column is what the
                    // declaration names. A partition's is its own where it
                    // declares one, and otherwise the copy of its parent's
                    // the engine gave it (#1588).
                    Part::Default(column) => {
                        t.columns
                            .get(column.raw())
                            .is_some_and(|c| match &c.value_source {
                                ValueSource::Default(_) => true,
                                ValueSource::None
                                | ValueSource::Identity(_)
                                | ValueSource::Sequence(_)
                                | ValueSource::Generated(_) => false,
                            })
                            || t.partition_of.as_ref().is_some_and(|of| {
                                of.columns
                                    .get(column.raw())
                                    .is_some_and(|o| o.default.is_some())
                                    || declared
                                        .tables
                                        .get(&of.parent)
                                        .and_then(|parent| parent.columns.get(column.raw()))
                                        .is_some_and(|c| match &c.value_source {
                                            ValueSource::Default(_) => true,
                                            ValueSource::None
                                            | ValueSource::Identity(_)
                                            | ValueSource::Sequence(_)
                                            | ValueSource::Generated(_) => false,
                                        })
                            })
                    }
                    // Nothing can put it back, so it is never kept across a
                    // rebuild; only an expression change releases it.
                    Part::Generated(_) | Part::Policy(_) => false,
                }
            }),
            Holds::Unrepresentable(_) => false,
        }
    }
}

/// The refusal for dependents this project could not put back, or `None`.
///
/// An affected dependent that pbps does not manage cannot be recreated from
/// anything the project holds, so dropping it would destroy an object with no
/// way back — the same shape as ADR-0005 note 10's refusal to drop a role that
/// owns something. And `DROP … CASCADE` is not the way out: it is the shortest
/// path and it destroys objects nobody reviewed (SPEC 14.3).
#[must_use]
pub fn unmanaged_refusal(
    id: &ModuleId,
    dependents: &[Dependent],
    declared: &Schema,
) -> Option<String> {
    let blocked: Vec<String> = dependents
        .iter()
        .filter(|d| !d.managed(declared))
        .map(|d| match &d.holds {
            Holds::Unrepresentable(why) => format!("- {} -- {why}", d.described),
            Holds::TablePart {
                part: Part::Policy(_),
                ..
            } => format!(
                "- {} -- explicitly remove or replace this policy in the declarations",
                pbps_model::escape::Escaped(&d.described)
            ),
            Holds::TablePart {
                part: Part::Generated(_),
                ..
            } => format!(
                "- {} -- a generated column, whose expression no statement can take off and put \
                 back around the rebuild; a plan that changes the expression so it no longer \
                 calls `{id}` releases it",
                d.described,
                id = id.shown()
            ),
            Holds::Module(_) | Holds::TablePart { .. } => {
                format!("- {} -- this project does not declare it", d.described)
            }
        })
        .collect();
    if blocked.is_empty() {
        return None;
    }
    Some(format!(
        "`{id}` cannot be rebuilt: this engine has no `CREATE OR ALTER`, so the change is a drop \
         and a create (ADR-0009 section 3), and these objects depend on it and this project could not \
         put them back:\n{}\nThe plan names every object it drops, or it does not drop \
         (SPEC 14.3), so `DROP ... CASCADE` is not offered. Declare them, or remove the \
         dependency, and plan again.",
        blocked.join("\n"),
        id = id.shown()
    ))
}

/// Catalog-proven policy dependents. Relation-only edges (including EXISTS
/// SELECT FROM) do not block a column alteration, measured on 16 and 18, and
/// must not be expanded into every attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDependent {
    pub table: TableName,
    pub name: String,
}

impl PolicyDependent {
    /// Named membership in the already scoped schema, not binding proof.
    pub fn is_declared_in(&self, schema: &Schema) -> bool {
        schema
            .tables
            .get(&self.table)
            .is_some_and(|table| table.row_policies.contains_key(&self.name))
    }
}

#[cfg(test)]
mod policy_message_tests {
    use super::*;

    #[test]
    fn unmanaged_policy_refusals_show_descriptions_once_and_keep_raw_keys() {
        let id: ModuleId = "app.gate()".parse().unwrap();
        for (raw, shown) in [
            (
                "policy\n\u{1b}[31m\u{202e}名前",
                r"policy\n\u{1b}[31m\u{202e}名前",
            ),
            (
                r"policy\n\u{1b}[31m\u{202e}名前",
                r"policy\\n\\u{1b}[31m\\u{202e}名前",
            ),
            ("名前_ünï", "名前_ünï"),
        ] {
            let dependent = Dependent {
                described: raw.into(),
                holds: Holds::TablePart {
                    table: pbps_model::TableName::new("app", "t"),
                    part: Part::Policy(raw.into()),
                },
            };
            assert_eq!(
                unmanaged_refusal(&id, std::slice::from_ref(&dependent), &Schema::default())
                    .unwrap(),
                format!(
                    "`app.gate()` cannot be rebuilt: this engine has no `CREATE OR ALTER`, so the change is a drop \
                     and a create (ADR-0009 section 3), and these objects depend on it and this project could not \
                     put them back:\n- {shown} -- explicitly remove or replace this policy in the declarations\nThe plan names every object it drops, or it does not drop \
                     (SPEC 14.3), so `DROP ... CASCADE` is not offered. Declare them, or remove the \
                     dependency, and plan again."
                )
            );
            assert_eq!(dependent.described, raw);
            assert!(
                matches!(&dependent.holds, Holds::TablePart { part: Part::Policy(name), .. } if name == raw)
            );
        }
        assert!(unmanaged_refusal(&id, &[], &Schema::default()).is_none());
    }
}
