//! A foreign key this plan introduces or retypes into unjoinable types
//! (#2359, #2395, DEC-2359.1, DEC-2395.1).
//!
//! A type change on a column that takes part in a foreign key is either made
//! with the key down (the dialect's `retype_dependents` says when, and
//! `recreate_retyped_dependents` drops and adds it again around the change),
//! or it is refused here. What is refused is the key whose two declared types
//! the engine cannot join once the plan has run: one end retyped and the
//! other left as it was, or both retyped to types that do not meet. Such a plan
//! applies up to the key and is rolled back at the statement that adds it
//! again, so it is named at plan time instead, with the next step. Added keys
//! and inline keys of created tables ask the same question of the after-state.
//!
//! Nothing here reads a database. The answer is the dialect's own table
//! (`Dialect::foreign_key_types_compatible`), and a dialect with no table says
//! nothing, which refuses nothing.

use std::collections::BTreeSet;

use pbps_dialect::Dialect;
use pbps_model::{Change, ColumnRef, ColumnType, ObjectLocalName, Schema, TableName};

/// A foreign key, as the declarations leave it, whose two ends the engine
/// cannot join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unjoinable {
    pub table: TableName,
    pub constraint: ObjectLocalName,
    /// The referencing column and its declared type, in the dialect's
    /// spelling.
    pub referencing: (ColumnRef, ColumnType),
    /// The referenced column and its declared type.
    pub referenced: (ColumnRef, ColumnType),
    /// Endpoints selected by the existing repair diagnostic. Retypes select
    /// the changed ends so it can name the one left behind. For a new key
    /// with neither end retyped,
    /// both are true to select the existing neutral "declare types that join"
    /// repair rather than inventing an end the plan changed.
    pub referencing_retyped: bool,
    pub referenced_retyped: bool,
}

fn declared_type(
    schema: &Schema,
    column: &ColumnRef,
    dialect: &(impl Dialect + ?Sized),
) -> Option<ColumnType> {
    let ty = &schema
        .tables
        .get(&column.table)?
        .columns
        .get(&column.name)?
        .ty;
    dialect.normalize_type(ty).ok()
}

/// The foreign keys of `declared` that `changes` introduces or retypes and
/// whose two declared column types the dialect knows it cannot join.
///
/// Every key is read from the declarations and not from the plan's own drop
/// and add of it: the plan may keep a key it also rebuilds around the change,
/// and what has to hold is the key that stands at the end. An existing key
/// with no retyped column and no introducing change is left alone: the engine
/// already holds it. The column names are the ones the plan ends
/// with, as a change spells them.
pub fn unjoinable(
    changes: &[Change],
    declared: &Schema,
    dialect: &(impl Dialect + ?Sized),
) -> Vec<Unjoinable> {
    let retyped: BTreeSet<&ColumnRef> = changes
        .iter()
        .filter_map(|change| {
            if let Change::AlterColumnType { column, .. } = change {
                Some(column)
            } else {
                None
            }
        })
        .collect();
    let mut introduced = BTreeSet::new();
    for change in changes {
        if let Change::AddForeignKey { table, name, .. } = change {
            introduced.insert((table, name));
        }
        if let Change::CreateTable { name, table, .. } = change {
            introduced.extend(table.foreign_keys.keys().map(|key| (name, key)));
        }
    }
    if retyped.is_empty() && introduced.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (table, t) in &declared.tables {
        for (constraint, fk) in &t.foreign_keys {
            let new_key = introduced.contains(&(table, constraint));
            for (local, remote) in fk.columns.iter().zip(&fk.references_columns) {
                let referencing = table.column(local);
                let referenced = fk.references_table.column(remote);
                let referencing_retyped = retyped.contains(&referencing);
                let referenced_retyped = retyped.contains(&referenced);
                if !referencing_retyped && !referenced_retyped && !new_key {
                    continue;
                }
                let (Some(child), Some(parent)) = (
                    declared_type(declared, &referencing, dialect),
                    declared_type(declared, &referenced, dialect),
                ) else {
                    continue;
                };
                if dialect.foreign_key_types_compatible(&child, &parent) == Some(false) {
                    let neutral_repair = !referencing_retyped && !referenced_retyped;
                    out.push(Unjoinable {
                        table: table.clone(),
                        constraint: constraint.clone(),
                        referencing: (referencing, child),
                        referenced: (referenced, parent),
                        // Reuse the neutral repair when no type change can
                        // identify a "left behind" end (DEC-2395.1).
                        referencing_retyped: referencing_retyped || neutral_repair,
                        referenced_retyped: referenced_retyped || neutral_repair,
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Context, DiffError, Side, diff};
    use pbps_dialect::{Dialect, DialectError, MinimalDialect, RetypeDependents};
    use pbps_model::{Column, ForeignKey, Hints, IdsFile, Table};

    /// `MinimalDialect` with the two answers a dialect gives about keys: which
    /// types a key can join (the whole numbers with one another, the strings
    /// with one another, nothing across), and that a change across the two
    /// takes the keys over its column down. A type of another name is one it
    /// cannot place, and a change to or from one leaves the keys to the
    /// engine, as PostgreSQL's does.
    struct Keyed;

    fn group(ty: &ColumnType) -> Option<u8> {
        match ty.base.as_str() {
            "integer" | "bigint" => Some(0),
            "text" | "varchar" => Some(1),
            _ => None,
        }
    }

    impl Dialect for Keyed {
        fn name(&self) -> &'static str {
            "keyed"
        }
        fn records_conversions(&self) -> bool {
            true
        }
        fn retype_dependents(&self, from: &ColumnType, to: &ColumnType) -> RetypeDependents {
            RetypeDependents {
                foreign_keys: matches!((group(from), group(to)), (Some(a), Some(b)) if a != b),
                ..RetypeDependents::default()
            }
        }
        fn foreign_key_types_compatible(
            &self,
            referencing: &ColumnType,
            referenced: &ColumnType,
        ) -> Option<bool> {
            Some(group(referencing)? == group(referenced)?)
        }
        fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
            MinimalDialect.quote_ident(ident)
        }
        fn intermediate_tables(
            &self,
            change: &Change,
            strategy: pbps_model::Strategy,
        ) -> Result<Vec<TableName>, DialectError> {
            MinimalDialect.intermediate_tables(change, strategy)
        }
        fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
            Ok(ty.clone())
        }
        fn type_change_risk(
            &self,
            from: &ColumnType,
            to: &ColumnType,
        ) -> pbps_dialect::TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(ident)
        }
        fn lexicon(&self) -> pbps_dialect::Lexicon {
            MinimalDialect.lexicon()
        }
        fn validate_table(&self, name: &TableName, table: &Table) -> Vec<DialectError> {
            MinimalDialect.validate_table(name, table)
        }
        fn transaction_framing(
            &self,
            zone: &pbps_model::SessionZone,
        ) -> pbps_dialect::TransactionFraming {
            MinimalDialect.transaction_framing(zone)
        }
        fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
            MinimalDialect.probe_framing()
        }
    }

    /// `app.p(id)` and `app.c(pid)` with a key from the second to the first,
    /// each column of the type given.
    fn keyed(parent: &str, child: &str) -> Schema {
        let table = |column: &str, ty: &str| {
            let mut table = Table::default();
            table
                .columns
                .insert(column.into(), Column::new(ty.parse().unwrap()));
            table
        };
        let mut schema = Schema::default();
        schema
            .tables
            .insert("app.p".parse().unwrap(), table("id", parent));
        let mut c = table("pid", child);
        c.foreign_keys.insert(
            "fk_c_p".into(),
            ForeignKey {
                columns: vec!["pid".into()],
                references_table: "app.p".parse().unwrap(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        schema.tables.insert("app.c".parse().unwrap(), c);
        schema
    }

    fn plan(
        base: &Schema,
        declared: &Schema,
        dialect: &(impl Dialect + ?Sized),
    ) -> Result<Vec<Change>, Vec<DiffError>> {
        let ids = crate::resolve(
            base,
            &IdsFile::default(),
            &[],
            &Context {
                operator: "leon".into(),
                today: "2026-10-26".into(),
            },
        )
        .unwrap()
        .ids;
        let declared_ids = crate::resolve(
            declared,
            &ids,
            &[],
            &Context {
                operator: "leon".into(),
                today: "2026-10-26".into(),
            },
        )
        .unwrap()
        .ids;
        diff(
            Side {
                schema: base,
                ids: &ids,
            },
            Side {
                schema: declared,
                ids: &declared_ids,
            },
            dialect,
            &Hints::default(),
        )
        .map(|plan| {
            plan.changes()
                .changes
                .iter()
                .map(|p| p.change.clone())
                .collect()
        })
    }

    fn refusals(errors: Vec<DiffError>) -> Vec<String> {
        errors
            .into_iter()
            .filter(|e| matches!(e, DiffError::ForeignKeyTypesDiffer { .. }))
            .map(|e| e.to_string())
            .collect()
    }

    fn keys_rebuilt(changes: &[Change]) -> (usize, usize) {
        (
            changes
                .iter()
                .filter(|c| matches!(c, Change::DropForeignKey { .. }))
                .count(),
            changes
                .iter()
                .filter(|c| matches!(c, Change::AddForeignKey { .. }))
                .count(),
        )
    }

    #[test]
    fn created_and_added_keys_with_unjoinable_types_use_the_existing_neutral_repair() {
        let declared = keyed("text", "integer");
        let mut existing = declared.clone();
        existing
            .tables
            .get_mut(&"app.c".parse::<TableName>().unwrap())
            .unwrap()
            .foreign_keys
            .clear();
        for base in [Schema::default(), existing] {
            let errors = plan(&base, &declared, &Keyed).unwrap_err();
            let messages = refusals(errors);
            assert_eq!(messages.len(), 1, "{messages:?}");
            for needed in [
                "fk_c_p",
                "app.c.pid",
                "app.p.id",
                "`integer`",
                "`text`",
                "Declare types for app.c.pid and app.p.id that a key can join",
            ] {
                assert!(messages[0].contains(needed), "{needed}: {}", messages[0]);
            }
        }
    }

    #[test]
    fn created_and_added_joinable_or_unplaced_keys_still_plan() {
        for (parent, child) in [("bigint", "integer"), ("numeric", "integer")] {
            let declared = keyed(parent, child);
            let mut existing = declared.clone();
            existing
                .tables
                .get_mut(&"app.c".parse::<TableName>().unwrap())
                .unwrap()
                .foreign_keys
                .clear();
            for base in [Schema::default(), existing] {
                let changes = plan(&base, &declared, &Keyed).unwrap();
                assert!(
                    !changes.is_empty(),
                    "the fixture must plan an introducing change"
                );
            }
        }
        assert!(
            plan(
                &Schema::default(),
                &keyed("text", "integer"),
                &MinimalDialect
            )
            .is_ok()
        );
    }

    #[test]
    fn every_pair_of_a_new_composite_key_uses_the_declared_after_state() {
        let mut declared = keyed("bigint", "integer");
        let parent = declared
            .tables
            .get_mut(&"app.p".parse::<TableName>().unwrap())
            .unwrap();
        parent
            .columns
            .insert("other".into(), Column::new("text".parse().unwrap()));
        let child = declared
            .tables
            .get_mut(&"app.c".parse::<TableName>().unwrap())
            .unwrap();
        child
            .columns
            .insert("other".into(), Column::new("integer".parse().unwrap()));
        let key = child.foreign_keys.get_mut("fk_c_p").unwrap();
        key.columns.push("other".into());
        key.references_columns.push("other".into());
        let messages = refusals(plan(&Schema::default(), &declared, &Keyed).unwrap_err());
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(messages[0].contains("app.c.other") && messages[0].contains("app.p.other"));
    }

    #[test]
    fn a_key_left_between_types_that_do_not_meet_is_refused_by_name_with_the_next_step() {
        // (parent, child) once the plan has run: one end left behind each way.
        for (retyped, ends) in [
            ("child", ("integer", "text")),
            ("parent", ("text", "integer")),
        ] {
            let base = keyed("integer", "integer");
            let errors = plan(&base, &keyed(ends.0, ends.1), &Keyed).unwrap_err();
            let [message] = refusals(errors)
                .try_into()
                .expect("exactly one key is named");
            for needed in [
                "fk_c_p",
                "app.c",
                "app.c.pid",
                "app.p.id",
                "`integer`",
                "`text`",
                "pbps convert",
            ] {
                assert!(message.contains(needed), "{retyped}: {needed} in {message}");
            }
        }
    }

    #[test]
    fn the_next_step_names_the_end_the_plan_left_behind() {
        let child = refusals(
            plan(
                &keyed("integer", "integer"),
                &keyed("integer", "text"),
                &Keyed,
            )
            .unwrap_err(),
        );
        assert!(
            child[0].contains("Change app.p.id to a type a key can join")
                && child[0].contains("declare app.c.pid with the type it has today"),
            "{}",
            child[0]
        );
        let parent = refusals(
            plan(
                &keyed("integer", "integer"),
                &keyed("text", "integer"),
                &Keyed,
            )
            .unwrap_err(),
        );
        assert!(
            parent[0].contains("Change app.c.pid to a type a key can join")
                && parent[0].contains("declare app.p.id with the type it has today"),
            "{}",
            parent[0]
        );
    }

    #[test]
    fn both_ends_changed_to_types_that_do_not_meet_are_asked_to_agree() {
        let base = keyed("integer", "integer");
        let mut declared = keyed("text", "bigint");
        // Neither end is left behind, so the next step names both.
        let message = refusals(plan(&base, &declared, &Keyed).unwrap_err()).remove(0);
        assert!(
            message.contains("Declare types for app.c.pid and app.p.id that a key can join"),
            "{message}"
        );
        declared = keyed("text", "text");
        assert!(plan(&base, &declared, &Keyed).is_ok());
    }

    #[test]
    fn both_ends_changed_together_rebuild_the_key_instead_of_refusing_it() {
        let changes = plan(&keyed("integer", "integer"), &keyed("text", "text"), &Keyed)
            .expect("a pair of types that meet again is planned");
        assert_eq!(keys_rebuilt(&changes), (1, 1));
        assert_eq!(
            changes
                .iter()
                .filter(|c| matches!(c, Change::AlterColumnType { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn a_change_inside_a_group_of_types_a_key_joins_leaves_the_key_to_the_engine() {
        for declared in [keyed("integer", "bigint"), keyed("bigint", "integer")] {
            let changes = plan(&keyed("integer", "integer"), &declared, &Keyed)
                .expect("the engine rebuilds the key itself");
            assert_eq!(keys_rebuilt(&changes), (0, 0));
        }
    }

    #[test]
    fn a_key_the_plan_does_not_retype_is_not_judged() {
        // Deliberately unjoinable, so judging this unchanged fixture key
        // would refuse an unrelated change. No database is asked to hold it.
        let mut base = keyed("text", "integer");
        let mut other = Table::default();
        other
            .columns
            .insert("n".into(), Column::new("integer".parse().unwrap()));
        base.tables.insert("app.o".parse().unwrap(), other);
        let mut declared = base.clone();
        declared
            .tables
            .get_mut(&"app.o".parse::<TableName>().unwrap())
            .unwrap()
            .columns
            .insert("n".into(), Column::new("text".parse().unwrap()));
        let changes = plan(&base, &declared, &Keyed).expect("an unrelated retype is planned");
        assert_eq!(keys_rebuilt(&changes), (0, 0));
    }

    #[test]
    fn a_dialect_that_cannot_place_the_types_refuses_nothing() {
        // `numeric` is in neither group: no answer, so no refusal and no
        // rebuilt key, and the engine says what it thinks at the apply.
        let changes = plan(
            &keyed("integer", "integer"),
            &keyed("integer", "numeric"),
            &Keyed,
        )
        .expect("an unplaced type is not guessed at");
        assert_eq!(keys_rebuilt(&changes), (0, 0));
        // And a dialect with no table at all asks nothing of any pair.
        let changes = plan(
            &keyed("integer", "integer"),
            &keyed("integer", "text"),
            &MinimalDialect,
        )
        .expect("a dialect with no table refuses no key");
        assert_eq!(keys_rebuilt(&changes), (0, 0));
    }
}
