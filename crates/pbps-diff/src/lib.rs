//! Comparison of declared states.
//!
//! This currently covers **identity resolution** ([`identity`]): mapping the names
//! in the declarations back to UIDs, deciding what was added, renamed and
//! dropped, and which situations a human has to adjudicate.
//!
//! This layer produces no SQL and touches no database (constraint 3 in
//! CLAUDE.md).

pub mod identity;
pub mod managed;
pub mod schema_diff;

pub use identity::{Blocker, Context, RenameSide, Resolution, intent_is_absorbed, resolve};
pub use managed::{Scoped, observed_ids, scope};
pub use schema_diff::{DiffError, Diffed, Side, diff, diff_partial, order_role_drops};

#[cfg(test)]
// In tests, a catch-all arm with a panic is the right way to say "this should be
// unreachable". The value of this lint is exhaustiveness in product code, where
// it forces a new Change variant to be handled.
#[allow(clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use pbps_model::{Column, ColumnType, IdsFile, Intent, Schema, Table, TableName};

    fn ctx() -> Context {
        Context {
            operator: "leon".into(),
            today: "2026-08-30".into(),
        }
    }

    fn t(s: &str) -> TableName {
        s.parse().unwrap()
    }

    /// Builds a schema quickly from `table -> [columns]`. Every type is int,
    /// because identity resolution does not look at attributes.
    fn schema(spec: &[(&str, &[&str])]) -> Schema {
        let mut s = Schema::default();
        for (name, cols) in spec {
            let mut columns = IndexMap::new();
            for c in *cols {
                columns.insert(
                    (*c).to_string(),
                    Column::new("int".parse::<ColumnType>().unwrap()),
                );
            }
            s.tables.insert(
                t(name),
                Table {
                    columns,
                    ..Default::default()
                },
            );
        }
        s
    }

    /// Registers a schema into an identity file first, as the "last known
    /// state".
    fn baseline(spec: &[(&str, &[&str])]) -> (Schema, IdsFile) {
        let s = schema(spec);
        let r = resolve(&s, &IdsFile::default(), &[], &ctx()).unwrap();
        (s, r.ids)
    }

    // ---- first run ----

    #[test]
    fn first_run_creates_everything() {
        let s = schema(&[("dbo.customer", &["id", "email"])]);
        let r = resolve(&s, &IdsFile::default(), &[], &ctx()).unwrap();

        assert_eq!(r.created_tables.len(), 1);
        assert_eq!(r.added_columns.len(), 2);
        assert!(r.dropped_columns.is_empty());
        assert_eq!(r.ids.tables.len(), 1);
        assert_eq!(r.ids.columns.len(), 2);
        r.ids.validate().unwrap();
    }

    /// No change must produce no identity-level action at all, or every run would
    /// show phantom changes.
    #[test]
    fn unchanged_schema_produces_nothing() {
        let (s, ids) = baseline(&[("dbo.customer", &["id", "email"])]);
        let r = resolve(&s, &ids, &[], &ctx()).unwrap();

        assert_eq!(
            r,
            Resolution {
                ids: ids.clone(),
                ..Default::default()
            }
        );
        assert_eq!(
            r.ids, ids,
            "the identity file must not be rewritten needlessly"
        );
    }

    // ---- additions and deletions ----

    #[test]
    fn pure_addition_needs_no_intent() {
        let (_, ids) = baseline(&[("dbo.customer", &["id"])]);
        let s = schema(&[("dbo.customer", &["id", "mobile"])]);
        let r = resolve(&s, &ids, &[], &ctx()).unwrap();

        assert_eq!(r.added_columns.len(), 1);
        assert_eq!(r.added_columns[0].1.name, "mobile");
    }

    /// Deletion is unambiguous as an operation, but the tombstone has to answer
    /// "why", and no algorithm can produce that.
    #[test]
    fn deletion_without_a_reason_is_blocked() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "legacy"])]);
        let s = schema(&[("dbo.customer", &["id"])]);
        let errs = resolve(&s, &ids, &[], &ctx()).unwrap_err();

        assert_eq!(errs.len(), 1);
        assert!(matches!(
            &errs[0],
            Blocker::DropColumnNeedsReason { column } if column.name == "legacy"
        ));
    }

    #[test]
    fn deletion_with_a_reason_produces_a_tombstone() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "national_id"])]);
        let s = schema(&[("dbo.customer", &["id"])]);
        let intent = Intent::DropColumn {
            column: "dbo.customer.national_id".parse().unwrap(),
            reason: "REG-2026-042 PII erasure request".into(),
        };
        let r = resolve(&s, &ids, &[intent], &ctx()).unwrap();

        assert_eq!(r.dropped_columns.len(), 1);
        assert_eq!(r.ids.tombstones.len(), 1);
        let tomb = r.ids.tombstones.values().next().unwrap();
        assert_eq!(tomb.was, "dbo.customer.national_id");
        assert_eq!(tomb.reason, "REG-2026-042 PII erasure request");
        assert_eq!(tomb.operator, "leon");
        assert_eq!(tomb.dropped_at, "2026-08-30");
        r.ids.validate().unwrap();
    }

    /// Tombstones stay in the identity file, so the declarations only ever hold
    /// what you want and never accumulate zombie columns.
    #[test]
    fn tombstoned_column_does_not_reappear_as_a_change() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "national_id"])]);
        let s = schema(&[("dbo.customer", &["id"])]);
        let intent = Intent::DropColumn {
            column: "dbo.customer.national_id".parse().unwrap(),
            reason: "REG-1".into(),
        };
        let after = resolve(&s, &ids, &[intent], &ctx()).unwrap().ids;

        let again = resolve(&s, &after, &[], &ctx()).unwrap();
        assert!(again.dropped_columns.is_empty());
        assert!(again.added_columns.is_empty());
    }

    // ---- renames ----

    /// This is the whole reason the tool exists: with no intent, never guess.
    #[test]
    fn rename_without_intent_is_ambiguous() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let s = schema(&[("dbo.customer", &["id", "full_name"])]);
        let errs = resolve(&s, &ids, &[], &ctx()).unwrap_err();

        assert_eq!(errs.len(), 1);
        match &errs[0] {
            Blocker::AmbiguousColumns {
                table,
                disappeared,
                appeared,
            } => {
                assert_eq!(table, &t("dbo.customer"));
                assert_eq!(disappeared, &["customer_name"]);
                assert_eq!(appeared, &["full_name"]);
            }
            other => panic!("expected a column ambiguity, got {other:?}"),
        }
    }

    #[test]
    fn rename_with_intent_preserves_the_uid() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let before_uid = ids
            .column_uid(&"dbo.customer.customer_name".parse().unwrap())
            .unwrap()
            .clone();

        let s = schema(&[("dbo.customer", &["id", "full_name"])]);
        let intent = Intent::RenameColumn {
            table: t("dbo.customer"),
            from: "customer_name".into(),
            to: "full_name".into(),
        };
        let r = resolve(&s, &ids, &[intent], &ctx()).unwrap();

        assert_eq!(r.renamed_columns.len(), 1);
        let (uid, from, to) = &r.renamed_columns[0];
        assert_eq!(
            uid, &before_uid,
            "a rename must preserve the original identity"
        );
        assert_eq!(from.name, "customer_name");
        assert_eq!(to.name, "full_name");
        assert!(
            r.added_columns.is_empty(),
            "a rename must not also count as an addition"
        );
        assert!(
            r.dropped_columns.is_empty(),
            "a rename must not also count as a deletion"
        );
        assert!(
            r.ids.tombstones.is_empty(),
            "a rename must not leave a tombstone"
        );
    }

    /// A rename and an addition in one revision must be told apart correctly.
    #[test]
    fn rename_and_addition_together() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let s = schema(&[("dbo.customer", &["id", "full_name", "mobile"])]);
        let intent = Intent::RenameColumn {
            table: t("dbo.customer"),
            from: "customer_name".into(),
            to: "full_name".into(),
        };
        let r = resolve(&s, &ids, &[intent], &ctx()).unwrap();

        assert_eq!(r.renamed_columns.len(), 1);
        assert_eq!(r.added_columns.len(), 1);
        assert_eq!(r.added_columns[0].1.name, "mobile");
    }

    // ---- table level ----

    #[test]
    fn table_rename_moves_its_columns() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "email"])]);
        let col_uid = ids
            .column_uid(&"dbo.customer.email".parse().unwrap())
            .unwrap()
            .clone();

        let s = schema(&[("dbo.client", &["id", "email"])]);
        let intent = Intent::RenameTable {
            from: t("dbo.customer"),
            to: t("dbo.client"),
        };
        let r = resolve(&s, &ids, &[intent], &ctx()).unwrap();

        assert_eq!(r.renamed_tables.len(), 1);
        assert!(
            r.added_columns.is_empty() && r.dropped_columns.is_empty(),
            "a table rename must not make its columns look brand new"
        );
        assert_eq!(
            r.ids.columns.get(&col_uid).unwrap().to_string(),
            "dbo.client.email",
            "a column's qualified name must follow the table rename"
        );
    }

    #[test]
    fn table_drop_tombstones_its_columns_too() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "email"])]);
        let s = Schema::default();
        let intent = Intent::DropTable {
            table: t("dbo.customer"),
            reason: "no longer in use".into(),
        };
        let r = resolve(&s, &ids, &[intent], &ctx()).unwrap();

        assert_eq!(r.dropped_tables.len(), 1);
        assert_eq!(
            r.ids.tombstones.len(),
            3,
            "the table itself plus its two columns"
        );
        assert!(r.ids.tables.is_empty());
        assert!(r.ids.columns.is_empty());
        r.ids.validate().unwrap();
    }

    #[test]
    fn table_rename_without_intent_is_ambiguous() {
        let (_, ids) = baseline(&[("dbo.customer", &["id"])]);
        let s = schema(&[("dbo.client", &["id"])]);
        let errs = resolve(&s, &ids, &[], &ctx()).unwrap_err();
        assert!(matches!(errs[0], Blocker::AmbiguousTables { .. }));
    }

    // ---- faulty intents ----

    /// A mistyped intent, if silently ignored, leaves the user facing an
    /// ambiguity error they cannot explain.
    #[test]
    fn a_typo_in_an_intent_is_reported() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let s = schema(&[("dbo.customer", &["id", "full_name"])]);
        let intent = Intent::RenameColumn {
            table: t("dbo.customer"),
            from: "custmer_name".into(), // typo
            to: "full_name".into(),
        };
        let errs = resolve(&s, &ids, std::slice::from_ref(&intent), &ctx()).unwrap_err();

        assert!(
            errs.iter()
                .any(|b| matches!(b, Blocker::UnusedIntent { intent: i } if i == &intent)),
            "the intent matching nothing should be reported: {errs:?}"
        );
    }

    /// Intents must be idempotent: after a successful rename, the renamed_from
    /// annotation still sitting in the file must not fail the next run.
    #[test]
    fn an_already_applied_intent_is_not_an_error() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let s = schema(&[("dbo.customer", &["id", "full_name"])]);
        let intent = Intent::RenameColumn {
            table: t("dbo.customer"),
            from: "customer_name".into(),
            to: "full_name".into(),
        };
        let after = resolve(&s, &ids, std::slice::from_ref(&intent), &ctx())
            .unwrap()
            .ids;

        // The annotation is still in the file; run again.
        let again = resolve(&s, &after, std::slice::from_ref(&intent), &ctx()).unwrap();
        assert!(
            again.renamed_columns.is_empty(),
            "the rename must not be applied twice"
        );
    }

    /// `pbps fmt` keeps or strips a `renamed_from` annotation on exactly this
    /// predicate, so its two answers are pinned: pending stays, absorbed goes.
    #[test]
    fn an_intent_counts_as_absorbed_only_after_it_took_effect() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let intent = Intent::RenameColumn {
            table: t("dbo.customer"),
            from: "customer_name".into(),
            to: "full_name".into(),
        };
        assert!(
            !intent_is_absorbed(&intent, &ids),
            "a pending rename must not count as absorbed, or fmt would strip it early"
        );

        let s = schema(&[("dbo.customer", &["id", "full_name"])]);
        let after = resolve(&s, &ids, std::slice::from_ref(&intent), &ctx())
            .unwrap()
            .ids;
        assert!(
            intent_is_absorbed(&intent, &after),
            "once the ids file has the fact, the annotation is redundant"
        );
    }

    #[test]
    fn an_already_applied_drop_is_not_an_error() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "gone"])]);
        let s = schema(&[("dbo.customer", &["id"])]);
        let intent = Intent::DropColumn {
            column: "dbo.customer.gone".parse().unwrap(),
            reason: "REG-1".into(),
        };
        let after = resolve(&s, &ids, std::slice::from_ref(&intent), &ctx())
            .unwrap()
            .ids;
        resolve(&s, &after, std::slice::from_ref(&intent), &ctx())
            .expect("a drop intent that already took effect must not error");
    }

    // ---- invariants ----

    /// The resolved identity file must be self-consistent, or the next comparison
    /// would build on a broken baseline.
    #[test]
    fn resulting_ids_file_is_always_valid() {
        let (_, ids) = baseline(&[("dbo.a", &["x", "y"]), ("dbo.b", &["z"])]);
        // dbo.b stays, dbo.c is a pure addition, and dbo.a has a column rename.
        let s = schema(&[
            ("dbo.a", &["x", "y2"]),
            ("dbo.b", &["z"]),
            ("dbo.c", &["w"]),
        ]);
        let intents = vec![Intent::RenameColumn {
            table: t("dbo.a"),
            from: "y".into(),
            to: "y2".into(),
        }];
        let r = resolve(&s, &ids, &intents, &ctx()).unwrap();
        r.ids.validate().unwrap();
    }

    /// Resolving the same declarations again after applying once must do nothing
    /// at all — convergence.
    #[test]
    fn resolution_converges() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let s = schema(&[("dbo.customer", &["id", "full_name", "mobile"])]);
        let intents = vec![Intent::RenameColumn {
            table: t("dbo.customer"),
            from: "customer_name".into(),
            to: "full_name".into(),
        }];
        let after = resolve(&s, &ids, &intents, &ctx()).unwrap().ids;

        let again = resolve(&s, &after, &[], &ctx()).unwrap();
        assert_eq!(
            again,
            Resolution {
                ids: after.clone(),
                ..Default::default()
            },
            "the second resolution must produce no actions"
        );
    }

    /// Every problem should be reported at once, not fix-one-run-again.
    #[test]
    fn multiple_blockers_are_all_reported() {
        let (_, ids) = baseline(&[("dbo.a", &["x", "gone"]), ("dbo.b", &["y", "old"])]);
        let s = schema(&[("dbo.a", &["x"]), ("dbo.b", &["y", "new"])]);
        let errs = resolve(&s, &ids, &[], &ctx()).unwrap_err();

        assert_eq!(
            errs.len(),
            2,
            "the missing drop reason and the column ambiguity should both be reported: {errs:?}"
        );
    }

    // ---- rename intents that claim one name ----

    /// The one that was silent. Two intents naming one source: the first to be
    /// reached consumed it, the second fell through, and because its target had
    /// meanwhile been minted into the ids file the absorbed check read it as
    /// already done. `resolve` returned `Ok`.
    ///
    /// Measured before the fix: the column that held the data was renamed to
    /// `aaa` — a name the author did not choose for it — and `zzz` was created
    /// as a brand-new empty column beside it. Which of the two won was
    /// declaration order.
    #[test]
    fn two_rename_intents_naming_one_source_column_are_refused() {
        let (_, ids) = baseline(&[("dbo.t", &["id", "old"])]);
        let s = schema(&[("dbo.t", &["id", "aaa", "zzz"])]);
        let errs = resolve(
            &s,
            &ids,
            &[
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "old".into(),
                    to: "aaa".into(),
                },
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "old".into(),
                    to: "zzz".into(),
                },
            ],
            &ctx(),
        )
        .unwrap_err();

        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            matches!(&errs[0], Blocker::ConflictingRenameIntents { side, name, intents }
                if *side == RenameSide::Source && name == "dbo.t.old" && intents.len() == 2),
            "{errs:?}"
        );
    }

    /// The mirror: two sources renamed onto one name. Only one object can end
    /// up with it.
    ///
    /// This one already reached `Err` — but as an `UnusedIntent`, "matches
    /// nothing in either the declarations or the identity file, likely a typo",
    /// about an intent whose every name exists. The blocker has to say what is
    /// actually wrong or the user goes looking for a misspelling there is none
    /// of.
    #[test]
    fn two_rename_intents_naming_one_target_column_are_refused() {
        let (_, ids) = baseline(&[("dbo.t", &["id", "old1", "old2"])]);
        let s = schema(&[("dbo.t", &["id", "new"])]);
        let errs = resolve(
            &s,
            &ids,
            &[
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "old1".into(),
                    to: "new".into(),
                },
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "old2".into(),
                    to: "new".into(),
                },
            ],
            &ctx(),
        )
        .unwrap_err();

        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            matches!(&errs[0], Blocker::ConflictingRenameIntents { side, name, intents }
                if *side == RenameSide::Target && name == "dbo.t.new" && intents.len() == 2),
            "{errs:?}"
        );
    }

    /// The same shape one level up. `resolve_tables` is the same loop over a
    /// different type, so fixing the columns alone would be fixing none
    /// (`docs/PITFALLS.md`, "One rule, spelled in three places").
    #[test]
    fn two_rename_intents_naming_one_source_table_are_refused() {
        let (_, ids) = baseline(&[("dbo.old", &["id"])]);
        let s = schema(&[("dbo.aaa", &["id"]), ("dbo.zzz", &["id"])]);
        let errs = resolve(
            &s,
            &ids,
            &[
                Intent::RenameTable {
                    from: t("dbo.old"),
                    to: t("dbo.aaa"),
                },
                Intent::RenameTable {
                    from: t("dbo.old"),
                    to: t("dbo.zzz"),
                },
            ],
            &ctx(),
        )
        .unwrap_err();

        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            matches!(&errs[0], Blocker::ConflictingRenameIntents { side, name, .. }
                if *side == RenameSide::Source && name == "dbo.old"),
            "{errs:?}"
        );
    }

    /// And roles, where the consequence is the worst of the three: a role's
    /// membership follows it through a rename and is destroyed by a drop and
    /// add, so a role renamed to a name nobody chose takes its members with it.
    #[test]
    fn two_rename_intents_naming_one_source_role_are_refused() {
        let s = with_roles(&[("dbo.t", &["id"])], &["old"]);
        let ids = resolve(&s, &IdsFile::default(), &[], &ctx()).unwrap().ids;
        let s = with_roles(&[("dbo.t", &["id"])], &["aaa", "zzz"]);
        let errs = resolve(
            &s,
            &ids,
            &[
                Intent::RenameRole {
                    from: "old".into(),
                    to: "aaa".into(),
                },
                Intent::RenameRole {
                    from: "old".into(),
                    to: "zzz".into(),
                },
            ],
            &ctx(),
        )
        .unwrap_err();

        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            matches!(&errs[0], Blocker::ConflictingRenameIntents { side, name, .. }
                if *side == RenameSide::Source && name == "old"),
            "{errs:?}"
        );
    }

    /// A table and a role may share a name, and two intents renaming them are
    /// not in conflict. The key carries the kind so that they are not read as
    /// one claim — the negative case for the grouping, and the one a key of
    /// bare strings would get wrong.
    #[test]
    fn a_table_and_a_role_of_one_name_are_two_claims_not_one() {
        let s = with_roles(&[("dbo.thing", &["id"])], &["thing"]);
        let ids = resolve(&s, &IdsFile::default(), &[], &ctx()).unwrap().ids;
        let s = with_roles(&[("dbo.renamed", &["id"])], &["renamed"]);
        let r = resolve(
            &s,
            &ids,
            &[
                Intent::RenameTable {
                    from: t("dbo.thing"),
                    to: t("dbo.renamed"),
                },
                Intent::RenameRole {
                    from: "thing".into(),
                    to: "renamed".into(),
                },
            ],
            &ctx(),
        )
        .unwrap();

        assert_eq!(r.renamed_tables.len(), 1);
        assert_eq!(r.renamed_roles.len(), 1);
    }

    /// One rename stated twice is redundant, not ambiguous. The same intent
    /// reaches `resolve` twice whenever a `renamed_from` annotation is also
    /// answered at the interactive prompt, and refusing that would refuse a
    /// valid plan for saying one true thing twice.
    #[test]
    fn one_rename_stated_twice_is_not_a_conflict() {
        let (_, ids) = baseline(&[("dbo.t", &["id", "old"])]);
        let s = schema(&[("dbo.t", &["id", "new"])]);
        let intent = Intent::RenameColumn {
            table: t("dbo.t"),
            from: "old".into(),
            to: "new".into(),
        };
        let r = resolve(&s, &ids, &[intent.clone(), intent], &ctx()).unwrap();

        assert_eq!(r.renamed_columns.len(), 1);
        assert_eq!(r.renamed_columns[0].2.name, "new");
    }

    /// Two renames in one table that contend for nothing are still two
    /// renames. The guard must not fire on a plan whose only crime is being
    /// more than one rename long.
    #[test]
    fn two_unrelated_renames_in_one_table_are_not_a_conflict() {
        let (_, ids) = baseline(&[("dbo.t", &["id", "a", "b"])]);
        let s = schema(&[("dbo.t", &["id", "x", "y"])]);
        let r = resolve(
            &s,
            &ids,
            &[
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "a".into(),
                    to: "x".into(),
                },
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "b".into(),
                    to: "y".into(),
                },
            ],
            &ctx(),
        )
        .unwrap();

        assert_eq!(r.renamed_columns.len(), 2);
    }

    /// A chain — `a -> b` beside `b -> c` — claims no name twice on either
    /// side, so this guard is silent about it. It is refused anyway, because
    /// `b` is in the declarations and in the ids file and so is in neither
    /// `appeared` nor `disappeared`; both intents go unmatched. Pinned so that
    /// nobody widens the guard to cover a case that is already covered.
    #[test]
    fn a_rename_chain_is_refused_by_the_existing_rule_not_by_this_guard() {
        let (_, ids) = baseline(&[("dbo.t", &["id", "a", "b"])]);
        let s = schema(&[("dbo.t", &["id", "b", "c"])]);
        let errs = resolve(
            &s,
            &ids,
            &[
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "a".into(),
                    to: "b".into(),
                },
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "b".into(),
                    to: "c".into(),
                },
            ],
            &ctx(),
        )
        .unwrap_err();

        assert!(
            errs.iter()
                .all(|b| !matches!(b, Blocker::ConflictingRenameIntents { .. })),
            "{errs:?}"
        );
        assert!(
            errs.iter()
                .any(|b| matches!(b, Blocker::UnusedIntent { .. })),
            "{errs:?}"
        );
    }

    /// Two intents claiming one column name in *different* tables are two
    /// claims. The column's key carries its table, and a key of bare column
    /// names would refuse this valid plan.
    #[test]
    fn one_column_name_in_two_tables_is_two_claims() {
        let (_, ids) = baseline(&[("dbo.a", &["old"]), ("dbo.b", &["old"])]);
        let s = schema(&[("dbo.a", &["new"]), ("dbo.b", &["new"])]);
        let r = resolve(
            &s,
            &ids,
            &[
                Intent::RenameColumn {
                    table: t("dbo.a"),
                    from: "old".into(),
                    to: "new".into(),
                },
                Intent::RenameColumn {
                    table: t("dbo.b"),
                    from: "old".into(),
                    to: "new".into(),
                },
            ],
            &ctx(),
        )
        .unwrap();

        assert_eq!(r.renamed_columns.len(), 2);
    }

    /// A stale annotation whose vacated source name has since been reused is
    /// not contending with the live rename of the object now holding it.
    ///
    /// A `renamed_from` lives on until `pbps fmt` strips it, which is what
    /// `intent_is_absorbed` exists for — so an annotation recording a rename
    /// that already happened is *expected* to be sitting in the file. Here
    /// `dbo.zzz` still says `renamed_from: dbo.old`, a new `dbo.old` has since
    /// been created, and this revision renames it to `dbo.aaa`. Grouped by
    /// their shared source the two look like a contest; they are not, because
    /// the stale one cannot match — `dbo.zzz` is on both sides of the
    /// declarations and so is not in `appeared`.
    ///
    /// Measured: the first form of the guard refused this, which is a valid
    /// plan refused for an annotation `fmt` has not got to yet.
    #[test]
    fn a_stale_annotation_does_not_contend_with_a_live_rename() {
        let (_, ids) = baseline(&[("dbo.old", &["id"]), ("dbo.zzz", &["id"])]);
        let s = schema(&[("dbo.aaa", &["id"]), ("dbo.zzz", &["id"])]);
        let r = resolve(
            &s,
            &ids,
            &[
                Intent::RenameTable {
                    from: t("dbo.old"),
                    to: t("dbo.aaa"),
                },
                Intent::RenameTable {
                    from: t("dbo.old"),
                    to: t("dbo.zzz"),
                },
            ],
            &ctx(),
        )
        .unwrap();

        assert_eq!(r.renamed_tables.len(), 1);
        assert_eq!(r.renamed_tables[0].2, t("dbo.aaa"));
    }

    /// The same for a column, where the reused name is far likelier: a column
    /// name vacated by a rename is exactly the name a later revision reaches
    /// for.
    #[test]
    fn a_stale_column_annotation_does_not_contend_with_a_live_rename() {
        let (_, ids) = baseline(&[("dbo.t", &["id", "old", "zzz"])]);
        let s = schema(&[("dbo.t", &["id", "aaa", "zzz"])]);
        let r = resolve(
            &s,
            &ids,
            &[
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "old".into(),
                    to: "aaa".into(),
                },
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "old".into(),
                    to: "zzz".into(),
                },
            ],
            &ctx(),
        )
        .unwrap();

        assert_eq!(r.renamed_columns.len(), 1);
        assert_eq!(r.renamed_columns[0].2.name, "aaa");
    }

    /// And for a role.
    #[test]
    fn a_stale_role_annotation_does_not_contend_with_a_live_rename() {
        let s = with_roles(&[("dbo.t", &["id"])], &["old", "zzz"]);
        let ids = resolve(&s, &IdsFile::default(), &[], &ctx()).unwrap().ids;
        let s = with_roles(&[("dbo.t", &["id"])], &["aaa", "zzz"]);
        let r = resolve(
            &s,
            &ids,
            &[
                Intent::RenameRole {
                    from: "old".into(),
                    to: "aaa".into(),
                },
                Intent::RenameRole {
                    from: "old".into(),
                    to: "zzz".into(),
                },
            ],
            &ctx(),
        )
        .unwrap();

        assert_eq!(r.renamed_roles.len(), 1);
        assert_eq!(r.renamed_roles[0].2, "aaa");
    }

    /// A contested rename reports the contest and nothing else. The
    /// contending intents are marked used by the guard, or the sweep at the end
    /// of `resolve` would report each of them a second time as "matches
    /// nothing … likely a typo" — the opposite of what is wrong with them.
    #[test]
    fn a_contested_rename_is_not_also_reported_as_an_unused_intent() {
        let (_, ids) = baseline(&[("dbo.t", &["id", "old"])]);
        let s = schema(&[("dbo.t", &["id", "aaa", "zzz"])]);
        let errs = resolve(
            &s,
            &ids,
            &[
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "old".into(),
                    to: "aaa".into(),
                },
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "old".into(),
                    to: "zzz".into(),
                },
            ],
            &ctx(),
        )
        .unwrap_err();

        assert!(
            errs.iter()
                .all(|b| matches!(b, Blocker::ConflictingRenameIntents { .. })),
            "{errs:?}"
        );
    }

    /// A conflict does not make a typo of the intents beside it.
    ///
    /// The guard raises the contest and the resolver carries on. Skipping the
    /// rest of the kind was the obvious move and the wrong one: `resolve`
    /// discards the whole `Resolution` when it returns `Err`, so the loop's
    /// order-dependent decision goes nowhere — but every unrelated intent it
    /// skipped reaches the sweep unmatched and is reported as "matches nothing
    /// … likely a typo". Two blockers, and the second one sends the user to
    /// look for a misspelling in an annotation that is exactly right.
    #[test]
    fn a_conflict_does_not_make_a_typo_of_the_renames_beside_it() {
        let (_, ids) = baseline(&[("dbo.old", &["id"]), ("dbo.x", &["id"])]);
        let s = schema(&[("dbo.a", &["id"]), ("dbo.b", &["id"]), ("dbo.y", &["id"])]);
        let errs = resolve(
            &s,
            &ids,
            &[
                Intent::RenameTable {
                    from: t("dbo.old"),
                    to: t("dbo.a"),
                },
                Intent::RenameTable {
                    from: t("dbo.old"),
                    to: t("dbo.b"),
                },
                // Untouched by the contest, and perfectly matchable.
                Intent::RenameTable {
                    from: t("dbo.x"),
                    to: t("dbo.y"),
                },
            ],
            &ctx(),
        )
        .unwrap_err();

        assert!(
            errs.iter()
                .all(|b| matches!(b, Blocker::ConflictingRenameIntents { .. })),
            "{errs:?}"
        );
    }

    /// The same for a drop, which the skipped loop would also have matched. Its
    /// reason is recorded and its object really is disappearing; nothing about
    /// it is a typo.
    #[test]
    fn a_conflict_does_not_make_a_typo_of_the_drops_beside_it() {
        let (_, ids) = baseline(&[("dbo.old", &["id"]), ("dbo.gone", &["id"])]);
        let s = schema(&[("dbo.a", &["id"]), ("dbo.b", &["id"])]);
        let errs = resolve(
            &s,
            &ids,
            &[
                Intent::RenameTable {
                    from: t("dbo.old"),
                    to: t("dbo.a"),
                },
                Intent::RenameTable {
                    from: t("dbo.old"),
                    to: t("dbo.b"),
                },
                Intent::DropTable {
                    table: t("dbo.gone"),
                    reason: "superseded".into(),
                },
            ],
            &ctx(),
        )
        .unwrap_err();

        assert!(
            errs.iter()
                .all(|b| matches!(b, Blocker::ConflictingRenameIntents { .. })),
            "{errs:?}"
        );
    }

    /// And in the other direction: a conflict in one table says nothing about
    /// another table's rename.
    ///
    /// This one passed with the early exit too — `resolve_columns` walks the
    /// declared tables, so its `continue` only ever skipped the contested one.
    /// Kept anyway, and said so here: the two above show the defect, this one
    /// holds the boundary the fix must not move, and a reader who finds it
    /// green under a revert should not conclude the revert was harmless.
    #[test]
    fn a_conflict_in_one_table_leaves_another_tables_rename_alone() {
        let (_, ids) = baseline(&[("dbo.t", &["id", "old"]), ("dbo.u", &["id", "x"])]);
        let s = schema(&[("dbo.t", &["id", "a", "b"]), ("dbo.u", &["id", "y"])]);
        let errs = resolve(
            &s,
            &ids,
            &[
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "old".into(),
                    to: "a".into(),
                },
                Intent::RenameColumn {
                    table: t("dbo.t"),
                    from: "old".into(),
                    to: "b".into(),
                },
                Intent::RenameColumn {
                    table: t("dbo.u"),
                    from: "x".into(),
                    to: "y".into(),
                },
            ],
            &ctx(),
        )
        .unwrap_err();

        assert!(
            errs.iter()
                .all(|b| matches!(b, Blocker::ConflictingRenameIntents { .. })),
            "{errs:?}"
        );
    }

    /// A repeated intent stays harmless when it is *inside* a contest.
    ///
    /// Saying one true thing twice is not a conflict, and the guard collapses
    /// the repeats before counting claimants — but collapsing them for the
    /// count must not collapse them for the accounting. Here `old1 -> new` is
    /// stated twice and loses the contest to `old2 -> new`, so `old1` is still
    /// in the identity file when the sweep runs; the copy whose index was
    /// dropped reached it and was reported as a likely typo beside the
    /// conflict.
    #[test]
    fn a_repeated_intent_inside_a_contest_is_not_also_a_typo() {
        let (_, ids) = baseline(&[("dbo.t", &["id", "old1", "old2"])]);
        let s = schema(&[("dbo.t", &["id", "new"])]);
        let claim = |from: &str| Intent::RenameColumn {
            table: t("dbo.t"),
            from: from.into(),
            to: "new".into(),
        };
        let errs = resolve(
            &s,
            &ids,
            &[claim("old2"), claim("old1"), claim("old1")],
            &ctx(),
        )
        .unwrap_err();

        assert!(
            errs.iter()
                .all(|b| matches!(b, Blocker::ConflictingRenameIntents { .. })),
            "{errs:?}"
        );
        // And the contest is still reported once, over the distinct claimants:
        // the repeat is one statement, however many times it was written.
        assert!(
            matches!(&errs[0], Blocker::ConflictingRenameIntents { intents, .. }
                if intents.len() == 2),
            "{errs:?}"
        );
    }

    // ---- roles (ADR-0005) ----

    fn with_roles(spec: &[(&str, &[&str])], roles: &[&str]) -> Schema {
        let mut s = schema(spec);
        for r in roles {
            s.roles
                .insert((*r).to_string(), pbps_model::Role::default());
        }
        s
    }

    #[test]
    fn a_new_role_is_minted_an_r_uid_and_an_unchanged_one_is_left_alone() {
        let s = with_roles(&[("dbo.customer", &["id"])], &["app_reader"]);
        let r = resolve(&s, &IdsFile::default(), &[], &ctx()).unwrap();
        assert_eq!(r.created_roles.len(), 1);
        assert_eq!(r.created_roles[0].1, "app_reader");
        assert_eq!(r.created_roles[0].0.kind(), pbps_model::UidKind::Role);
        r.ids.validate().unwrap();

        let again = resolve(&s, &r.ids, &[], &ctx()).unwrap();
        assert_eq!(again.ids, r.ids, "nothing changed, so nothing is rewritten");
        assert!(again.created_roles.is_empty());
    }

    /// The whole reason roles carry identity: a role that lost its name and
    /// one that gained a name in the same revision is not decidable, because
    /// drop + add would destroy membership pbps cannot restore.
    #[test]
    fn a_role_rename_without_intent_is_ambiguous_and_with_intent_keeps_the_uid() {
        let s = with_roles(&[("dbo.customer", &["id"])], &["reader"]);
        let ids = resolve(&s, &IdsFile::default(), &[], &ctx()).unwrap().ids;
        let before = ids.role_uid("reader").unwrap().clone();

        let s = with_roles(&[("dbo.customer", &["id"])], &["app_reader"]);
        let errs = resolve(&s, &ids, &[], &ctx()).unwrap_err();
        assert!(
            matches!(&errs[0], Blocker::AmbiguousRoles { disappeared, appeared }
                if disappeared == &["reader".to_string()] && appeared == &["app_reader".to_string()]),
            "{errs:?}"
        );

        let r = resolve(
            &s,
            &ids,
            &[Intent::RenameRole {
                from: "reader".into(),
                to: "app_reader".into(),
            }],
            &ctx(),
        )
        .unwrap();
        assert_eq!(r.renamed_roles.len(), 1);
        assert_eq!(r.ids.role_uid("app_reader"), Some(&before));
        assert_eq!(r.ids.role_uid("reader"), None);
        // Absorbed: the same annotation left in the file is not a typo.
        assert!(intent_is_absorbed(
            &Intent::RenameRole {
                from: "reader".into(),
                to: "app_reader".into()
            },
            &r.ids
        ));
    }

    #[test]
    fn a_role_drop_needs_a_reason_and_leaves_a_tombstone() {
        let s = with_roles(&[("dbo.customer", &["id"])], &["legacy"]);
        let ids = resolve(&s, &IdsFile::default(), &[], &ctx()).unwrap().ids;
        let uid = ids.role_uid("legacy").unwrap().clone();

        let s = schema(&[("dbo.customer", &["id"])]);
        let errs = resolve(&s, &ids, &[], &ctx()).unwrap_err();
        assert!(
            matches!(&errs[0], Blocker::DropRoleNeedsReason { role } if role == "legacy"),
            "{errs:?}"
        );

        let r = resolve(
            &s,
            &ids,
            &[Intent::DropRole {
                role: "legacy".into(),
                reason: "SEC-7: retired".into(),
            }],
            &ctx(),
        )
        .unwrap();
        assert_eq!(r.dropped_roles, vec![(uid.clone(), "legacy".to_string())]);
        assert_eq!(r.ids.tombstones[&uid].was, "legacy");
        assert_eq!(r.ids.tombstones[&uid].reason, "SEC-7: retired");
        r.ids.validate().unwrap();
        // And a stale annotation for it is absorbed, not a blocker.
        assert!(intent_is_absorbed(
            &Intent::DropRole {
                role: "legacy".into(),
                reason: "x".into()
            },
            &r.ids
        ));
    }

    /// A role and a table may share a word: their namespaces are separate.
    #[test]
    fn a_role_named_like_a_table_is_not_the_table() {
        let s = with_roles(&[("dbo.customer", &["id"])], &["customer"]);
        let r = resolve(&s, &IdsFile::default(), &[], &ctx()).unwrap();
        assert_eq!(r.created_tables.len(), 1);
        assert_eq!(r.created_roles.len(), 1);
        r.ids.validate().unwrap();
    }
}
