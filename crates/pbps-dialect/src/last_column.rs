//! Replacing every column of a table on an engine that will not drop the last
//! one (#2963, DEC-2963.1).
//!
//! The shared classes drop a column before they add one. SQLite
//! ("no other columns exist") and SQL Server (Msg 4923, "the only data column")
//! refuse to drop a table's last ordinary column, so `t(a)` to `t(b)`, a new
//! column identity, fails at its first statement. PostgreSQL allows a table
//! with no column, and the order does not show there.
//!
//! One drop of such a table moves behind one add. This module chooses the pair,
//! and the pair has to be one the engine performs: the add runs while the drop
//! is still there, and SQL Server holds one IDENTITY column per table, so an
//! IDENTITY drop is never paired with an IDENTITY add.
//! The differ moves the drop by it, and each engine's `engine_effects` requires
//! the add before the drop by it, so the order and its audit are the answer of
//! one function and never two that agree by accident.

use pbps_model::operation::{
    Effects, Incarnation, Operation, OwnerError, Owners, Requirement, Snapshot, column_key,
};
use pbps_model::{
    Change, Column, ColumnChange, ColumnName, IdsIndex, Schema, TableName, Uid, ValueSource,
};

use crate::Dialect;

/// How a table that replaces all of its ordinary columns is ordered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replacement {
    /// `drop` is the one drop that runs after the add `after`. `follows` are
    /// the adds that cannot run while `drop` is there, in declared order: the
    /// one that takes `drop`'s name, and the IDENTITY one when `drop` is an
    /// IDENTITY column. Each of them has to follow the drop.
    Order {
        drop: Uid,
        after: Uid,
        follows: Vec<Uid>,
    },
    /// No drop of the table has an add to wait for that the engine accepts.
    /// `column` is the drop that was last tried.
    Unorderable { column: ColumnName, block: Block },
}

/// Why no order of a table's replacement is one the engine performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    /// The table's only column is replaced by one of the same name. Dropping
    /// first leaves no column, and adding first repeats the name.
    SameName,
    /// An IDENTITY column is replaced by a new IDENTITY column. Dropping first
    /// leaves no column, and adding first holds two IDENTITY columns, which
    /// SQL Server refuses (`pbps-mssql` validate: "only one IDENTITY column";
    /// SQLite declares none, and PostgreSQL does not use this module).
    Identity,
}

/// A column the engine counts as one that stays. A generated column is not one
/// on either engine (SQLite: "at least one non-generated column"; SQL Server
/// counts data columns, and keeps computed ones apart in the model).
fn ordinary(column: &Column) -> bool {
    !matches!(column.value_source, ValueSource::Generated(_))
}

/// Whether the column is numbered by an IDENTITY, which a table holds one of
/// on the engines that reach this module.
fn identity(column: &Column) -> bool {
    matches!(column.value_source, ValueSource::Identity(_))
}

/// The ordering of the table the baseline holds as `was` and the declaration
/// as `now`, or `None` when it keeps a baseline column, adds none, or the
/// engine allows a table with no column.
///
/// By identity, so a renamed column is a kept one and a new column of an old
/// name is an add and a drop.
pub fn replacement(
    dialect: &(impl Dialect + ?Sized),
    (base, base_ids, was): (&Schema, &IdsIndex<'_>, &TableName),
    (declared, declared_ids, now): (&Schema, &IdsIndex<'_>, &TableName),
) -> Option<Replacement> {
    if !dialect.refuses_dropping_last_column() {
        return None;
    }
    let (old, new) = (base.tables.get(was)?, declared.tables.get(now)?);
    let held: Vec<(&Uid, &ColumnName)> = base_ids
        .columns_of(was)
        .filter(|(_, r)| old.columns.contains_key(&r.name))
        .map(|(uid, r)| (uid, &r.name))
        .collect();
    // In the baseline's column order, so the choice never rests on a uid.
    let mut drops: Vec<(&Uid, &ColumnName)> = held
        .iter()
        .copied()
        .filter(|(_, name)| old.columns.get(*name).is_some_and(ordinary))
        .collect();
    drops.sort_by_key(|(_, name)| old.columns.get_index_of(*name));
    let stays = declared_ids.columns_of(now).any(|(uid, r)| {
        new.columns.contains_key(&r.name) && drops.iter().any(|(held, _)| *held == uid)
    });
    if drops.is_empty() || stays {
        return None;
    }
    // In the declared order, which is the column order a pull reads (#2080).
    let mut adds: Vec<(&Uid, &ColumnName)> = declared_ids
        .columns_of(now)
        .filter(|(uid, r)| {
            !held.iter().any(|(was, _)| was == uid)
                && new.columns.get(&r.name).is_some_and(ordinary)
        })
        .map(|(uid, r)| (uid, &r.name))
        .collect();
    adds.sort_by_key(|(_, name)| new.columns.get_index_of(*name));
    if adds.is_empty() {
        return None;
    }
    // Equal under the engine's own folding, which on SQL Server is exact
    // equality: the database's collation is not known to a plan made offline,
    // so a pair that differs only by case is not refused (DEC-2963.1).
    let same =
        |a: &ColumnName, b: &ColumnName| dialect.fold_ident(a.raw()) == dialect.fold_ident(b.raw());
    // An add whose name no baseline column has even in another case never meets
    // a case-insensitive database's duplicate-name refusal.
    let safe = |name: &ColumnName| {
        !old.columns
            .keys()
            .any(|taken| taken.raw().eq_ignore_ascii_case(name.raw()))
    };
    let reuse = |drop: &ColumnName| adds.iter().position(|(_, name)| same(drop, name));
    let identity_add = |i: usize| new.columns.get(adds[i].1).is_some_and(identity);
    // A drop that no add takes the name of goes last when there is one, then
    // the baseline's last column.
    let (free, taken): (Vec<_>, Vec<_>) = drops
        .iter()
        .rev()
        .copied()
        .partition(|(_, name)| reuse(name).is_none());
    for (drop, name) in free.into_iter().chain(taken) {
        let reused = reuse(name);
        // The add runs while this drop still stands, so it cannot be an
        // IDENTITY one when the drop is.
        let numbered = old.columns.get(name).is_some_and(identity);
        let others: Vec<usize> = (0..adds.len())
            .filter(|&i| Some(i) != reused && !(numbered && identity_add(i)))
            .collect();
        let preferred: Vec<usize> = others
            .iter()
            .copied()
            .filter(|&i| safe(adds[i].1))
            .collect();
        let choices = if preferred.is_empty() {
            others
        } else {
            preferred
        };
        // Behind the add that follows the drop's reuse, so the add that takes
        // its name stays in the declared order; otherwise the last one.
        let at = match reused {
            Some(j) => choices
                .iter()
                .rev()
                .find(|&&i| i < j)
                .or(choices.first())
                .copied(),
            None => choices.last().copied(),
        };
        if let Some(at) = at {
            // What cannot run beside the drop, in the declared order.
            let follows = (0..adds.len())
                .filter(|&i| Some(i) == reused || (numbered && identity_add(i)))
                .map(|i| adds[i].0.clone())
                .collect();
            return Some(Replacement::Order {
                drop: drop.clone(),
                after: adds[at].0.clone(),
                follows,
            });
        }
    }
    let (_, last) = drops.last()?;
    // With no add left to pair, the name taking the drop's own is the reason
    // when it is all there is, and the two IDENTITY columns when others were
    // there and the drop is one.
    let block = if reuse(last).is_some() && adds.len() == 1 {
        Block::SameName
    } else {
        Block::Identity
    };
    Some(Replacement::Unorderable {
        column: (*last).clone(),
        block,
    })
}

/// What a drop requires on an engine that refuses the last column: the add
/// that `replacement` puts before it. What an add that has to follow that drop
/// requires: the drop is gone. Nothing for any other step.
pub fn requirements(
    dialect: &(impl Dialect + ?Sized),
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
) -> Result<Effects, OwnerError> {
    let mut fx = Effects::default();
    if !dialect.refuses_dropping_last_column() {
        return Ok(fx);
    }
    let (uid, column, follower) =
        if let Change::Column(ColumnChange::DropColumn { uid, column, .. }) = op.change() {
            (uid, &column.table, false)
        } else if let Change::Column(ColumnChange::AddColumn { uid, table, .. }) = op.change() {
            (uid, table, true)
        } else {
            return Ok(fx);
        };
    let table = owners.table(column, op.origin().spelling())?;
    let (Some(was), Some(now)) = (before.ids.tables.get(&table), after.ids.tables.get(&table))
    else {
        return Ok(fx);
    };
    let (base_ids, declared_ids) = (before.ids.index(), after.ids.index());
    if let Some(Replacement::Order {
        drop,
        after: add,
        follows,
    }) = replacement(
        dialect,
        (before.schema, &base_ids, was),
        (after.schema, &declared_ids, now),
    ) {
        if !follower && drop == *uid {
            fx.requires
                .push(Requirement::Is(column_key(&add), Incarnation::Final));
        } else if follower && follows.contains(uid) {
            // An IDENTITY add beside the IDENTITY drop is refused by the
            // engine, and the name of the other is taken until it goes.
            fx.requires.push(Requirement::Absent(column_key(&drop)));
        }
    }
    Ok(fx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MinimalDialect;
    use pbps_model::{IdsFile, Table};

    /// `MinimalDialect` under an engine that refuses the last column.
    struct Refuses;

    impl Dialect for Refuses {
        fn name(&self) -> &'static str {
            "refuses"
        }
        fn quote_ident(&self, ident: &str) -> Result<String, crate::DialectError> {
            MinimalDialect.quote_ident(ident)
        }
        fn intermediate_tables(
            &self,
            change: &Change,
            strategy: pbps_model::Strategy,
        ) -> Result<Vec<TableName>, crate::DialectError> {
            MinimalDialect.intermediate_tables(change, strategy)
        }
        fn normalize_type(
            &self,
            ty: &pbps_model::ColumnType,
        ) -> Result<pbps_model::ColumnType, crate::DialectError> {
            MinimalDialect.normalize_type(ty)
        }
        fn type_change_risk(
            &self,
            from: &pbps_model::ColumnType,
            to: &pbps_model::ColumnType,
        ) -> crate::TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(ident)
        }
        fn lexicon(&self) -> crate::Lexicon {
            MinimalDialect.lexicon()
        }
        fn validate_table(&self, name: &TableName, table: &Table) -> Vec<crate::DialectError> {
            MinimalDialect.validate_table(name, table)
        }
        fn transaction_framing(&self, zone: &pbps_model::SessionZone) -> crate::TransactionFraming {
            MinimalDialect.transaction_framing(zone)
        }
        fn probe_framing(&self) -> Option<crate::TransactionFraming> {
            MinimalDialect.probe_framing()
        }
        fn refuses_dropping_last_column(&self) -> bool {
            true
        }
    }

    /// Like `Refuses`, and the engine folds case, as SQLite does.
    struct Folds;

    impl Dialect for Folds {
        fn name(&self) -> &'static str {
            "folds"
        }
        fn quote_ident(&self, ident: &str) -> Result<String, crate::DialectError> {
            Refuses.quote_ident(ident)
        }
        fn intermediate_tables(
            &self,
            change: &Change,
            strategy: pbps_model::Strategy,
        ) -> Result<Vec<TableName>, crate::DialectError> {
            Refuses.intermediate_tables(change, strategy)
        }
        fn normalize_type(
            &self,
            ty: &pbps_model::ColumnType,
        ) -> Result<pbps_model::ColumnType, crate::DialectError> {
            Refuses.normalize_type(ty)
        }
        fn type_change_risk(
            &self,
            from: &pbps_model::ColumnType,
            to: &pbps_model::ColumnType,
        ) -> crate::TypeChangeRisk {
            Refuses.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            std::borrow::Cow::Owned(ident.to_ascii_lowercase())
        }
        fn lexicon(&self) -> crate::Lexicon {
            Refuses.lexicon()
        }
        fn validate_table(&self, name: &TableName, table: &Table) -> Vec<crate::DialectError> {
            Refuses.validate_table(name, table)
        }
        fn transaction_framing(&self, zone: &pbps_model::SessionZone) -> crate::TransactionFraming {
            Refuses.transaction_framing(zone)
        }
        fn probe_framing(&self) -> Option<crate::TransactionFraming> {
            Refuses.probe_framing()
        }
        fn refuses_dropping_last_column(&self) -> bool {
            true
        }
    }

    fn uid(n: u32) -> Uid {
        format!("c_{n:06}").parse().unwrap()
    }

    /// One table `app.t` with the columns `(uid number, name)` in order, and
    /// the identity file that records them.
    fn side(columns: &[(u32, &str)]) -> (Schema, IdsFile) {
        let name: TableName = "app.t".parse().unwrap();
        let mut table = Table::default();
        let mut ids = IdsFile::default();
        ids.tables.insert("t_000001".parse().unwrap(), name.clone());
        for (n, column) in columns {
            table
                .columns
                .insert((*column).into(), Column::new("integer".parse().unwrap()));
            ids.columns
                .insert(uid(*n), pbps_model::ColumnRef::new(name.clone(), *column));
        }
        let mut schema = Schema::default();
        schema.tables.insert(name, table);
        (schema, ids)
    }

    fn choose(
        dialect: &impl Dialect,
        was: &[(u32, &str)],
        now: &[(u32, &str)],
    ) -> Option<Replacement> {
        let ((base, base_ids), (declared, declared_ids)) = (side(was), side(now));
        let table: TableName = "app.t".parse().unwrap();
        replacement(
            dialect,
            (&base, &base_ids.index(), &table),
            (&declared, &declared_ids.index(), &table),
        )
    }

    fn order(drop: u32, after: u32, follows: &[u32]) -> Option<Replacement> {
        Some(Replacement::Order {
            drop: uid(drop),
            after: uid(after),
            follows: follows.iter().copied().map(uid).collect(),
        })
    }

    #[test]
    fn the_only_column_is_dropped_after_its_replacement() {
        assert_eq!(choose(&Refuses, &[(1, "a")], &[(2, "b")]), order(1, 2, &[]));
    }

    #[test]
    fn one_drop_of_two_waits_for_the_last_add() {
        // The later baseline column goes last; the earlier one keeps its place.
        assert_eq!(
            choose(&Refuses, &[(1, "a"), (2, "b")], &[(3, "c"), (4, "d")]),
            order(2, 4, &[])
        );
    }

    #[test]
    fn a_table_that_keeps_a_baseline_column_is_not_reordered() {
        assert_eq!(
            choose(&Refuses, &[(1, "a"), (2, "b")], &[(2, "b"), (3, "c")]),
            None
        );
        // A renamed column is a kept one: the same uid under another name.
        assert_eq!(choose(&Refuses, &[(1, "a")], &[(1, "b")]), None);
        assert_eq!(
            choose(&Refuses, &[(1, "a"), (2, "b")], &[(1, "c"), (3, "d")]),
            None
        );
    }

    #[test]
    fn an_engine_that_allows_a_table_with_no_column_is_not_reordered() {
        assert_eq!(choose(&MinimalDialect, &[(1, "a")], &[(2, "b")]), None);
        assert_eq!(choose(&MinimalDialect, &[(1, "a")], &[(2, "a")]), None);
    }

    #[test]
    fn a_table_with_nothing_added_is_left_alone() {
        assert_eq!(choose(&Refuses, &[(1, "a")], &[]), None);
    }

    #[test]
    fn the_add_that_takes_a_dropped_name_follows_that_drop() {
        // `a` is replaced by a new `a`, and `b` by `c`: `b` goes last, after `c`.
        assert_eq!(
            choose(&Refuses, &[(1, "a"), (2, "b")], &[(3, "a"), (4, "c")]),
            order(2, 4, &[])
        );
        // Every name reused: the drop goes between the adds, after `a`'s and
        // before the add that takes its own name.
        assert_eq!(
            choose(&Refuses, &[(1, "a"), (2, "b")], &[(3, "a"), (4, "b")]),
            order(2, 3, &[4])
        );
    }

    #[test]
    fn the_only_column_replaced_by_the_same_name_has_no_order() {
        assert_eq!(
            choose(&Refuses, &[(1, "a")], &[(2, "a")]),
            Some(Replacement::Unorderable {
                column: "a".into(),
                block: Block::SameName
            })
        );
        // Equal under the engine's folding, not under another's.
        assert_eq!(
            choose(&Folds, &[(1, "a")], &[(2, "A")]),
            Some(Replacement::Unorderable {
                column: "a".into(),
                block: Block::SameName
            })
        );
    }

    #[test]
    fn names_that_differ_only_by_case_are_ordered_add_first_where_the_engine_does_not_fold() {
        // A case-sensitive database takes it; a case-insensitive one refuses
        // the add with its own duplicate-name error (DEC-2963.1).
        assert_eq!(choose(&Refuses, &[(1, "a")], &[(2, "A")]), order(1, 2, &[]));
    }

    #[test]
    fn an_add_that_no_baseline_name_equals_in_any_case_is_preferred() {
        // `A` differs from `a` by case only; `c` is the better one to add first.
        assert_eq!(
            choose(&Refuses, &[(1, "a")], &[(2, "A"), (3, "c")]),
            order(1, 3, &[])
        );
        assert_eq!(
            choose(&Refuses, &[(1, "a")], &[(2, "c"), (3, "A")]),
            order(1, 2, &[])
        );
    }

    #[test]
    fn a_generated_column_is_neither_one_that_stays_nor_one_that_is_dropped_last() {
        let ((mut base, base_ids), (declared, declared_ids)) =
            (side(&[(1, "a"), (2, "g")]), side(&[(3, "b")]));
        let table: TableName = "app.t".parse().unwrap();
        base.tables
            .get_mut(&table)
            .unwrap()
            .columns
            .get_mut("g")
            .unwrap()
            .value_source = ValueSource::Generated(pbps_model::Generated {
            expression: "1".into(),
            stored: true,
        });
        let chosen = replacement(
            &Refuses,
            (&base, &base_ids.index(), &table),
            (&declared, &declared_ids.index(), &table),
        );
        assert_eq!(chosen, order(1, 3, &[]));
    }

    /// `side`, with each named column numbered by an IDENTITY.
    fn numbered(mut side: (Schema, IdsFile), names: &[&str]) -> (Schema, IdsFile) {
        let table: TableName = "app.t".parse().unwrap();
        for name in names {
            side.0
                .tables
                .get_mut(&table)
                .unwrap()
                .columns
                .get_mut(*name)
                .unwrap()
                .value_source = ValueSource::Identity(pbps_model::Identity::new(1, 1));
        }
        side
    }

    fn choose_numbered(
        (was, was_ids): (&[(u32, &str)], &[&str]),
        (now, now_ids): (&[(u32, &str)], &[&str]),
    ) -> Option<Replacement> {
        let ((base, base_ids), (declared, declared_ids)) =
            (numbered(side(was), was_ids), numbered(side(now), now_ids));
        let table: TableName = "app.t".parse().unwrap();
        replacement(
            &Refuses,
            (&base, &base_ids.index(), &table),
            (&declared, &declared_ids.index(), &table),
        )
    }

    #[test]
    fn an_identity_drop_is_paired_with_an_add_that_is_not_one() {
        // `b` (identity) is added last by preference, but the drop `a` is an
        // identity too, and the engine holds one: `c` goes first, and `b`
        // follows the drop.
        assert_eq!(
            choose_numbered((&[(1, "a")], &["a"]), (&[(2, "b"), (3, "c")], &["b"])),
            order(1, 3, &[2])
        );
        assert_eq!(
            choose_numbered((&[(1, "a")], &["a"]), (&[(2, "c"), (3, "b")], &["b"])),
            order(1, 2, &[3])
        );
    }

    #[test]
    fn another_drop_is_tried_when_the_last_one_has_no_add_it_may_wait_for() {
        // The last column `b` is the identity, and the only add is one too:
        // the first column is the drop that has a valid add.
        assert_eq!(
            choose_numbered((&[(1, "a"), (2, "b")], &["b"]), (&[(3, "c")], &["c"])),
            order(1, 3, &[])
        );
    }

    #[test]
    fn an_identity_replaced_by_an_identity_with_nothing_else_to_add_has_no_order() {
        assert_eq!(
            choose_numbered((&[(1, "a")], &["a"]), (&[(2, "b")], &["b"])),
            Some(Replacement::Unorderable {
                column: "a".into(),
                block: Block::Identity
            })
        );
        // A second add that is no identity is what makes it orderable.
        assert!(
            choose_numbered((&[(1, "a")], &["a"]), (&[(2, "b"), (3, "c")], &["b"]))
                .is_some_and(|r| matches!(r, Replacement::Order { .. }))
        );
    }

    #[test]
    fn an_identity_drop_whose_name_is_taken_by_an_identity_add_needs_another_add() {
        // `a` is replaced by a new identity `a`: that add follows the drop for
        // its name, and the other add, which is no identity, goes first.
        assert_eq!(
            choose_numbered((&[(1, "a")], &["a"]), (&[(2, "a"), (3, "c")], &["a"])),
            order(1, 3, &[2])
        );
        assert_eq!(
            choose_numbered((&[(1, "a")], &["a"]), (&[(2, "a")], &["a"])),
            Some(Replacement::Unorderable {
                column: "a".into(),
                block: Block::SameName
            })
        );
    }

    #[test]
    fn a_drop_that_is_no_identity_keeps_the_preferred_pair_beside_an_identity_add() {
        // The identity is on the add side only: nothing stands that numbers.
        assert_eq!(
            choose_numbered((&[(1, "a")], &[]), (&[(2, "b"), (3, "c")], &["c"])),
            order(1, 3, &[])
        );
        // Neither is one.
        assert_eq!(
            choose_numbered((&[(1, "a")], &[]), (&[(2, "b")], &[])),
            order(1, 2, &[])
        );
    }
}
