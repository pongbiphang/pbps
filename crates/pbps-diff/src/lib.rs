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

pub use identity::{Blocker, Context, Resolution, intent_is_absorbed, resolve};
pub use managed::{Scoped, observed_ids, scope};
pub use schema_diff::{DiffError, Diffed, Side, diff, diff_partial};

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
