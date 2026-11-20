//! Put the drop of a table's last column behind the add that replaces it
//! (#2963, DEC-2963.1).
//!
//! The classes drop a column before they add one. On an engine that refuses to
//! drop a table's last ordinary column (`Dialect::refuses_dropping_last_column`)
//! that makes `t(a)` to `t(b)` a plan whose first statement fails. The pair of
//! changes to swap is `pbps_dialect::last_column::replacement`'s answer, the
//! same one each engine's `engine_effects` requires, so the audit verifies the
//! order by the rule that made it.
//!
//! A pass after the sort and not a rank in it: the drop has to sit between two
//! adds when the second takes its name, which no class can say, and the
//! refusal of the one case no order performs lives where the pair is chosen.
//! It runs inside the differ, so a release build orders by it too.

use std::collections::BTreeSet;

use pbps_dialect::Dialect;
use pbps_dialect::last_column::{Block, Replacement, replacement};
use pbps_model::{Change, ColumnChange, ColumnName, PlannedChange, TableName, Uid};

use crate::Side;

fn drop_at(planned: &[PlannedChange], wanted: &Uid) -> Option<usize> {
    planned.iter().position(
        |p| matches!(&p.change, Change::Column(ColumnChange::DropColumn { uid, .. }) if uid == wanted),
    )
}

fn add_at(planned: &[PlannedChange], wanted: &Uid) -> Option<usize> {
    planned.iter().position(
        |p| matches!(&p.change, Change::Column(ColumnChange::AddColumn { uid, .. }) if uid == wanted),
    )
}

/// Moves each replaced table's last drop behind its add, and returns the
/// tables no order of whose replacement the engine performs, as the table, the
/// column and the reason, for the caller to refuse.
///
/// Nothing moves earlier, and nothing but the drop (and the add that takes its
/// name, when that is already ahead of it) moves at all.
pub(crate) fn order(
    planned: &mut Vec<PlannedChange>,
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
) -> Vec<(TableName, ColumnName, Block)> {
    if !dialect.refuses_dropping_last_column() {
        return Vec::new();
    }
    // The tables with a column drop: the baseline's name of each, from the
    // dropped column's identity, and the declared one the drop carries.
    let tables: BTreeSet<(TableName, TableName)> = planned
        .iter()
        .filter_map(|p| {
            let Change::Column(ColumnChange::DropColumn { uid, column }) = &p.change else {
                return None;
            };
            Some((
                base.ids.columns.get(uid)?.table.clone(),
                column.table.clone(),
            ))
        })
        .collect();
    if tables.is_empty() {
        return Vec::new();
    }
    let (base_ids, declared_ids) = (base.ids.index(), declared.ids.index());
    let mut refused = Vec::new();
    for (was, now) in tables {
        match replacement(
            dialect,
            (base.schema, &base_ids, &was),
            (declared.schema, &declared_ids, &now),
        ) {
            None => {}
            Some(Replacement::Unorderable { column, block }) => refused.push((now, column, block)),
            Some(Replacement::Order {
                drop,
                after,
                follows,
            }) => {
                if let (Some(d), Some(x)) = (drop_at(planned, &drop), add_at(planned, &after))
                    && d < x
                {
                    // Removing the drop shifts the add one place up, so its old
                    // index is the place right behind it.
                    let moved = planned.remove(d);
                    planned.insert(x, moved);
                }
                // An add that cannot run beside the drop (it takes the drop's
                // name, or it is the IDENTITY one beside an IDENTITY drop)
                // follows it, unless it already does. The last one goes
                // first, so that the adds keep their declared order behind
                // the drop.
                for follower in follows.iter().rev() {
                    if let (Some(d), Some(y)) = (drop_at(planned, &drop), add_at(planned, follower))
                        && y < d
                    {
                        let moved = planned.remove(y);
                        planned.insert(d, moved);
                    }
                }
            }
        }
    }
    refused
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Context, DiffError, resolve};
    use pbps_dialect::MinimalDialect;
    use pbps_model::Hints;
    use pbps_model::{Column, IdsFile, Intent, Schema, Table};

    /// `MinimalDialect`, which allows a table with no column, or an engine
    /// that does not, with or without a case-folding comparison of names.
    struct Engine {
        refuses: bool,
        folds: bool,
    }

    const PERMISSIVE: Engine = Engine {
        refuses: false,
        folds: false,
    };
    const REFUSES: Engine = Engine {
        refuses: true,
        folds: false,
    };
    const REFUSES_AND_FOLDS: Engine = Engine {
        refuses: true,
        folds: true,
    };

    impl Dialect for Engine {
        fn name(&self) -> &'static str {
            "engine"
        }
        fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
            MinimalDialect.quote_ident(ident)
        }
        fn intermediate_tables(
            &self,
            change: &Change,
            strategy: pbps_model::Strategy,
        ) -> Result<Vec<TableName>, pbps_dialect::DialectError> {
            MinimalDialect.intermediate_tables(change, strategy)
        }
        fn normalize_type(
            &self,
            ty: &pbps_model::ColumnType,
        ) -> Result<pbps_model::ColumnType, pbps_dialect::DialectError> {
            MinimalDialect.normalize_type(ty)
        }
        fn type_change_risk(
            &self,
            from: &pbps_model::ColumnType,
            to: &pbps_model::ColumnType,
        ) -> pbps_dialect::TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            if self.folds {
                std::borrow::Cow::Owned(ident.to_ascii_lowercase())
            } else {
                MinimalDialect.fold_ident(ident)
            }
        }
        fn lexicon(&self) -> pbps_dialect::Lexicon {
            MinimalDialect.lexicon()
        }
        fn validate_table(
            &self,
            name: &TableName,
            table: &Table,
        ) -> Vec<pbps_dialect::DialectError> {
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
        fn refuses_dropping_last_column(&self) -> bool {
            self.refuses
        }
    }

    fn ctx() -> Context {
        Context {
            operator: "operator".into(),
            today: "2026-11-20".into(),
        }
    }

    fn schema(columns: &[&str]) -> Schema {
        let mut table = Table::default();
        for column in columns {
            table
                .columns
                .insert((*column).into(), Column::new("integer".parse().unwrap()));
        }
        let mut schema = Schema::default();
        schema.tables.insert("app.t".parse().unwrap(), table);
        schema
    }

    /// Identities for `was`, and for `now` where each name in `renewed` is a
    /// new identity (the old one dropped by intent) and each pair in `renames`
    /// keeps its column under a new name. Every other name keeps its identity.
    fn identities(
        was: &[&str],
        now: &[&str],
        renewed: &[&str],
        renames: &[(&str, &str)],
    ) -> (Schema, IdsFile, Schema, IdsFile) {
        let (base, declared) = (schema(was), schema(now));
        let base_ids = resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let mut intents: Vec<Intent> = renames
            .iter()
            .map(|(from, to)| Intent::RenameColumn {
                table: "app.t".parse().unwrap(),
                from: (*from).into(),
                to: (*to).into(),
            })
            .collect();
        for column in was {
            if !now.contains(column) && !renames.iter().any(|(from, _)| from == column) {
                intents.push(Intent::DropColumn {
                    column: format!("app.t.{column}").parse().unwrap(),
                    reason: "gone".parse().unwrap(),
                });
            }
        }
        // `resolve` matches a declared name to the baseline's column of that
        // name, so a new identity under an old name is made by hand.
        let mut declared_ids = resolve(&declared, &base_ids, &intents, &ctx())
            .map(|r| r.ids)
            .unwrap_or_else(|blocked| panic!("{blocked:?}"));
        for (n, name) in renewed.iter().enumerate() {
            let at = pbps_model::ColumnRef::new("app.t".parse().unwrap(), *name);
            let old = declared_ids.column_uid(&at).cloned().unwrap();
            declared_ids.columns.remove(&old);
            declared_ids
                .columns
                .insert(format!("c_new{n:03}").parse().unwrap(), at);
            declared_ids.tombstones.insert(
                old,
                pbps_model::Tombstone {
                    was: format!("app.t.{name}"),
                    dropped_at: "2026-11-20".into(),
                    reason: "gone".into(),
                    operator: "operator".into(),
                    require: None,
                },
            );
        }
        (base, base_ids, declared, declared_ids)
    }

    fn diffed(
        dialect: &Engine,
        (base, base_ids, declared, declared_ids): &(Schema, IdsFile, Schema, IdsFile),
    ) -> Result<Vec<PlannedChange>, Vec<DiffError>> {
        crate::diff(
            crate::Side {
                schema: base,
                ids: base_ids,
            },
            crate::Side {
                schema: declared,
                ids: declared_ids,
            },
            dialect,
            &Hints::default(),
        )
        .map(|plan| plan.changes().changes.clone())
    }

    /// The column changes as short names: `+x` an add, `-x` a drop, `x>y` a rename.
    fn steps(plan: &[PlannedChange]) -> Vec<String> {
        plan.iter()
            .filter_map(|p| {
                if let Change::Column(ColumnChange::AddColumn { name, .. }) = &p.change {
                    Some(["+", name.raw()].concat())
                } else if let Change::Column(ColumnChange::DropColumn { column, .. }) = &p.change {
                    Some(["-", column.name.raw()].concat())
                } else if let Change::Column(ColumnChange::RenameColumn { from, to, .. }) =
                    &p.change
                {
                    Some([from.raw(), ">", to.raw()].concat())
                } else {
                    None
                }
            })
            .collect()
    }

    fn plan(dialect: &Engine, was: &[&str], now: &[&str]) -> Vec<String> {
        steps(&diffed(dialect, &identities(was, now, &[], &[])).unwrap())
    }

    /// The fewest ordinary columns the table holds between two statements.
    fn fewest_columns(was: usize, steps: &[String]) -> usize {
        let (mut held, mut fewest) = (was, was);
        for step in steps {
            if step.starts_with('+') {
                held += 1;
            } else if step.starts_with('-') {
                held -= 1;
            }
            fewest = fewest.min(held);
        }
        fewest
    }

    #[test]
    fn the_only_column_is_dropped_after_its_replacement_on_an_engine_that_refuses() {
        let steps = plan(&REFUSES, &["a"], &["b"]);
        assert_eq!(steps, ["+b", "-a"]);
        assert!(fewest_columns(1, &steps) >= 1);
    }

    #[test]
    fn replacing_two_columns_never_leaves_the_table_without_one() {
        let steps = plan(&REFUSES, &["a", "b"], &["c", "d"]);
        // The first drop keeps its place; only the last waits for an add.
        assert_eq!(steps, ["-a", "+c", "+d", "-b"]);
        assert!(fewest_columns(2, &steps) >= 1);
    }

    #[test]
    fn an_engine_that_allows_a_table_with_no_column_keeps_the_drop_first() {
        assert_eq!(plan(&PERMISSIVE, &["a"], &["b"]), ["-a", "+b"]);
        // Both drops come first; which of the two is first is the uids' order.
        let steps = plan(&PERMISSIVE, &["a", "b"], &["c", "d"]);
        assert!(steps[..2].iter().all(|s| s.starts_with('-')), "{steps:?}");
        assert_eq!(steps[2..], ["+c", "+d"]);
    }

    #[test]
    fn a_table_that_keeps_a_baseline_column_keeps_the_order_it_has() {
        for (was, now, renames) in [
            (vec!["a", "b"], vec!["b", "c"], vec![]),
            (vec!["a", "b"], vec!["a", "c"], vec![]),
            // A renamed column is a kept one, and so is a lone renamed column.
            (vec!["a", "b"], vec!["c", "d"], vec![("a", "c")]),
            (vec!["a"], vec!["b"], vec![("a", "b")]),
        ] {
            let ids = identities(&was, &now, &[], &renames);
            let refusing = steps(&diffed(&REFUSES, &ids).unwrap());
            assert_eq!(
                refusing,
                steps(&diffed(&PERMISSIVE, &ids).unwrap()),
                "{was:?} to {now:?}"
            );
            assert!(!refusing.is_empty(), "{was:?} to {now:?} plans something");
        }
    }

    #[test]
    fn a_new_column_that_takes_a_dropped_name_follows_that_drop() {
        // `a` is replaced by a new `a`, and `b` by `c`: `b` goes last, after `c`,
        // and the new `a` still comes after the old one is gone.
        let ids = identities(&["a", "b"], &["a", "c"], &["a"], &[]);
        let steps = steps(&diffed(&REFUSES, &ids).unwrap());
        assert_eq!(steps, ["-a", "+a", "+c", "-b"]);
        assert!(fewest_columns(2, &steps) >= 1);
    }

    #[test]
    fn a_drop_between_two_adds_when_every_name_is_taken_again() {
        let ids = identities(&["a", "b"], &["a", "b"], &["a", "b"], &[]);
        let steps = steps(&diffed(&REFUSES, &ids).unwrap());
        // `b`'s drop waits for the first add, and the add of `b` for the drop.
        assert_eq!(steps, ["-a", "+a", "-b", "+b"]);
        assert!(fewest_columns(2, &steps) >= 1);
    }

    #[test]
    fn an_add_that_takes_the_name_of_the_only_dropped_column_waits_behind_the_drop() {
        // `a` is replaced by a new `a`, declared before `c`: no add can come
        // first but `c`, so the new `a` moves behind the drop, and the table
        // never lacks a column.
        let ids = identities(&["a"], &["a", "c"], &["a"], &[]);
        let steps = steps(&diffed(&REFUSES, &ids).unwrap());
        assert_eq!(steps, ["+c", "-a", "+a"]);
        assert!(fewest_columns(1, &steps) >= 1);
    }

    #[test]
    fn the_only_column_replaced_by_the_same_name_is_refused_naming_the_table_and_column() {
        let ids = identities(&["a"], &["a"], &["a"], &[]);
        let errors = diffed(&REFUSES, &ids).unwrap_err();
        let refused: Vec<String> = errors
            .iter()
            .filter(|e| matches!(e, DiffError::LastColumnReplacementUnorderable { .. }))
            .map(ToString::to_string)
            .collect();
        assert_eq!(refused.len(), 1, "{errors:?}");
        assert!(refused[0].contains("app.t"), "{}", refused[0]);
        assert!(refused[0].contains("`a`"), "{}", refused[0]);
        assert!(refused[0].contains("two revisions"), "{}", refused[0]);
        // An engine that allows a table with no column plans it.
        assert_eq!(steps(&diffed(&PERMISSIVE, &ids).unwrap()), ["-a", "+a"]);
    }

    #[test]
    fn names_that_differ_only_by_case_are_refused_only_where_the_engine_folds_them() {
        let ids = identities(&["a"], &["A"], &[], &[]);
        // Exact comparison, as on SQL Server: the database may be case
        // sensitive, so the add goes first and the engine decides.
        assert_eq!(steps(&diffed(&REFUSES, &ids).unwrap()), ["+A", "-a"]);
        // An engine that folds the names knows them to be one.
        let errors = diffed(&REFUSES_AND_FOLDS, &ids).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, DiffError::LastColumnReplacementUnorderable { .. })),
            "{errors:?}"
        );
    }

    #[test]
    fn an_add_that_equals_no_baseline_name_in_any_case_goes_first() {
        let ids = identities(&["a"], &["A", "c"], &[], &[]);
        let steps = steps(&diffed(&REFUSES, &ids).unwrap());
        // `A` differs from `a` by case only, so `c` is the better one to add first.
        assert_eq!(steps, ["+A", "+c", "-a"]);
    }

    /// `ids` with each named column of the baseline (`was`) and of the
    /// declaration (`now`) numbered by an IDENTITY.
    fn numbered(
        (mut base, base_ids, mut declared, declared_ids): (Schema, IdsFile, Schema, IdsFile),
        (was, now): (&[&str], &[&str]),
    ) -> (Schema, IdsFile, Schema, IdsFile) {
        let table: pbps_model::TableName = "app.t".parse().unwrap();
        for (schema, names) in [(&mut base, was), (&mut declared, now)] {
            for name in names {
                schema
                    .tables
                    .get_mut(&table)
                    .unwrap()
                    .columns
                    .get_mut(*name)
                    .unwrap()
                    .value_source =
                    pbps_model::ValueSource::Identity(pbps_model::Identity::new(1, 1));
            }
        }
        (base, base_ids, declared, declared_ids)
    }

    #[test]
    fn an_identity_replaced_by_an_identity_adds_a_column_that_is_not_one_first() {
        // `b` is declared first, but the engine holds one IDENTITY column, so
        // `c` goes first and `b` follows the drop.
        let ids = numbered(identities(&["a"], &["b", "c"], &[], &[]), (&["a"], &["b"]));
        let steps = steps(&diffed(&REFUSES, &ids).unwrap());
        assert_eq!(steps, ["+c", "-a", "+b"]);
        assert!(fewest_columns(1, &steps) >= 1);
        // Declared the other way round, nothing has to move but the drop.
        let ids = numbered(identities(&["a"], &["c", "b"], &[], &[]), (&["a"], &["b"]));
        assert_eq!(steps_of(&ids), ["+c", "-a", "+b"]);
    }

    fn steps_of(ids: &(Schema, IdsFile, Schema, IdsFile)) -> Vec<String> {
        steps(&diffed(&REFUSES, ids).unwrap())
    }

    #[test]
    fn another_drop_is_chosen_when_the_last_one_is_an_identity_with_only_an_identity_to_add() {
        // `b` is the last column and an IDENTITY, and so is `c`: the first
        // column is the one to drop last, after `c`.
        let ids = numbered(identities(&["a", "b"], &["c"], &[], &[]), (&["b"], &["c"]));
        let steps = steps_of(&ids);
        assert_eq!(steps, ["-b", "+c", "-a"]);
        assert!(fewest_columns(2, &steps) >= 1);
    }

    #[test]
    fn an_identity_replaced_by_an_identity_with_nothing_else_to_add_is_refused() {
        let ids = numbered(identities(&["a"], &["b"], &[], &[]), (&["a"], &["b"]));
        let errors = diffed(&REFUSES, &ids).unwrap_err();
        let refused: Vec<String> = errors
            .iter()
            .filter(|e| matches!(e, DiffError::LastColumnReplacementUnorderable { .. }))
            .map(ToString::to_string)
            .collect();
        assert_eq!(refused.len(), 1, "{errors:?}");
        for part in [
            "app.t",
            "`a`",
            "IDENTITY",
            "two revisions",
            "another kind or name",
        ] {
            assert!(refused[0].contains(part), "{part}: {}", refused[0]);
        }
        assert!(!refused[0].contains("same name"), "{}", refused[0]);
        // An engine that allows a table with no column plans it.
        assert_eq!(steps(&diffed(&PERMISSIVE, &ids).unwrap()), ["-a", "+b"]);
    }

    #[test]
    fn an_identity_on_one_side_only_keeps_the_order_the_replacement_has() {
        // The add is the IDENTITY one, the drop is not: nothing numbers the
        // column that stands while it runs.
        let ids = numbered(identities(&["a"], &["b"], &[], &[]), (&[], &["b"]));
        assert_eq!(steps_of(&ids), ["+b", "-a"]);
        // The drop is one, the add is not.
        let ids = numbered(identities(&["a"], &["b"], &[], &[]), (&["a"], &[]));
        assert_eq!(steps_of(&ids), ["+b", "-a"]);
    }

    #[test]
    fn adds_that_wait_behind_the_drop_keep_their_declared_order() {
        // The new `a` waits for its name and the IDENTITY `b` for the drop's
        // IDENTITY; `c` goes first, and the two follow the drop as declared.
        let ids = numbered(
            identities(&["a"], &["a", "b", "c"], &["a"], &[]),
            (&["a"], &["b"]),
        );
        let steps = steps_of(&ids);
        assert_eq!(steps, ["+c", "-a", "+a", "+b"]);
        assert!(fewest_columns(1, &steps) >= 1);
    }
}
