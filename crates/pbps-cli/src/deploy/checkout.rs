//! Inventory management removals before resolving local identity (DEC-2397.1).
use crate::baseline::ancestry::{self, Evidence};
use crate::output::Finding;
use pbps_model::{IdsFile, Intent, Schema, StateSnapshot};

// The differ owns this inventory so ancestry and drop-intent resolution
// cannot disagree about what disappeared (DEC-2397.1, DEC-2414.1).
fn removals(
    recorded: &IdsFile,
    recorded_schema: &Schema,
    local: &IdsFile,
    schema: &Schema,
    intents: &[Intent],
) -> Vec<String> {
    let mut removed: Vec<_> = pbps_diff::identity::removals::inventory(
        pbps_diff::Side {
            schema: recorded_schema,
            ids: recorded,
        },
        local,
        schema,
        intents,
    )
    .into_iter()
    .map(|removal| removal.to_string())
    .collect();
    removed.sort();
    removed
}

pub(super) fn inventory(
    snapshot: &StateSnapshot,
    ids: &IdsFile,
    schema: &Schema,
    intents: &[Intent],
) -> Vec<String> {
    removals(&snapshot.ids, &snapshot.schema, ids, schema, intents)
}

pub(super) fn assess(
    root: &std::path::Path,
    target: &str,
    revision: Option<&pbps_db::ledger::StateRevision>,
    removals: &[String],
) -> anyhow::Result<Option<Finding>> {
    use pbps_model::escape::Escaped;
    let target = Escaped(target);
    let unchecked = |reason: String| {
        Finding::note(
            "plan.checkout-unchecked",
            format!(
                "Checkout ancestry was not checked for `{target}`: {reason}. Planning continues without checkout-ancestry protection."
            ),
        )
    };
    let Some(revision) = revision else {
        return Ok(Some(unchecked(
            "the ledger has no retained schema-establishing entry".into(),
        )));
    };
    let Some(sha) = revision.git_sha.as_ref() else {
        return Ok(Some(unchecked(format!(
            "ledger entry #{} has no Git commit",
            revision.id
        ))));
    };
    let evidence = ancestry::check(root, sha.raw());
    // Missing or unreadable Git history must not block an additive plan, while
    // a known absence of a checkout still deserves the no-protection note.
    if removals.is_empty() && !matches!(evidence, Evidence::NoCheckout | Evidence::GitUnavailable) {
        return Ok(None);
    }
    let unreadable = matches!(evidence, Evidence::Unreadable(_));
    let sha = sha.shown();
    let (reason, remedy) = match evidence {
        Evidence::Contains => return Ok(None),
        Evidence::NoCheckout => {
            return Ok(Some(unchecked(
                "the planning project has no Git checkout".into(),
            )));
        }
        Evidence::GitUnavailable => {
            return Ok(Some(unchecked("the Git executable is unavailable".into())));
        }
        Evidence::Diverged => (
            format!("this checkout does not contain deployed commit `{sha}`."),
            format!("Merge or rebase onto `{sha}`, then plan again."),
        ),
        Evidence::Incomplete => (
            format!(
                "the available Git history does not establish deployed commit `{sha}` as an ancestor."
            ),
            format!(
                "Fetch the missing history, then merge or rebase onto `{sha}` and plan again.{}",
                ancestry::SHALLOW_REMEDY
            ),
        ),
        Evidence::Missing { shallow } => (
            format!("deployed commit `{sha}` is not a commit this repository has."),
            format!(
                "Fetch the missing history, then merge or rebase onto `{sha}` and plan again.{}",
                if shallow {
                    ancestry::SHALLOW_REMEDY
                } else {
                    ""
                }
            ),
        ),
        Evidence::Unreadable(message) => (
            "local Git history could not be read.".into(),
            format!(
                "Repair the repository or its access permissions, then plan again.\nGit reported: {}",
                Escaped(&message)
            ),
        ),
    };
    let message = format!(
        "Cannot plan deletions against `{target}`: {reason}\nDeletions:\n  {}\n{remedy}",
        removals.join("\n  ")
    );
    // A known ancestry mismatch is a finding; a Git read failure could not
    // answer the question, so keep the ordinary exit-1 / JSON failure route.
    if unreadable {
        anyhow::bail!("{message}");
    }
    Ok(Some(Finding::error("plan.checkout-ancestry", message)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, ColumnType, Role, Table, Uid, UidKind};

    fn fixture() -> (IdsFile, Schema) {
        let mut ids = IdsFile::default();
        let table = "app.customer".parse().unwrap();
        ids.tables.insert(Uid::generate(UidKind::Table), table);
        ids.columns.insert(
            Uid::generate(UidKind::Column),
            "app.customer.id".parse().unwrap(),
        );
        ids.roles
            .insert(Uid::generate(UidKind::Role), "reader".into());
        let mut schema = Schema::default();
        schema.tables.insert(
            "app.customer".parse().unwrap(),
            Table {
                columns: [("id".into(), Column::new(ColumnType::new("integer", vec![])))].into(),
                ..Default::default()
            },
        );
        schema.roles.insert("reader".into(), Role::default());
        (ids, schema)
    }

    #[test]
    fn inventory_is_complete_sorted_and_counts_a_removed_table_once() {
        let (ids, schema) = fixture();
        assert_eq!(
            removals(&ids, &schema, &IdsFile::default(), &Schema::default(), &[]),
            ["role reader", "table app.customer"]
        );
        let mut desired = schema.clone();
        desired.tables.values_mut().next().unwrap().columns.clear();
        desired.roles.clear();
        assert_eq!(
            removals(&ids, &schema, &ids, &desired, &[]),
            ["column app.customer.id", "role reader"]
        );
        assert!(removals(&ids, &schema, &ids, &schema, &[]).is_empty());
        // A same-name replacement uid removes the recorded identity too.
        let (replacement, _) = fixture();
        assert_eq!(
            removals(&ids, &schema, &replacement, &schema, &[]),
            ["role reader", "table app.customer"]
        );
    }

    #[test]
    fn retained_and_annotated_renames_do_not_become_removals() {
        let (ids, schema) = fixture();
        let mut desired = schema.clone();
        let mut table = desired
            .tables
            .remove(&"app.customer".parse().unwrap())
            .unwrap();
        let col = table.columns.shift_remove("id").unwrap();
        table.columns.insert("key".into(), col);
        desired.tables.insert("app.client".parse().unwrap(), table);
        let role = desired.roles.remove("reader").unwrap();
        desired.roles.insert("writer".into(), role);
        let intents = [
            Intent::RenameTable {
                from: "app.customer".parse().unwrap(),
                to: "app.client".parse().unwrap(),
            },
            Intent::RenameColumn {
                table: "app.client".parse().unwrap(),
                from: "id".into(),
                to: "key".into(),
            },
            Intent::RenameRole {
                from: "reader".into(),
                to: "writer".into(),
            },
        ];
        assert!(removals(&ids, &schema, &ids, &desired, &intents).is_empty());
        let mut reversed = intents.clone();
        reversed.reverse();
        assert!(removals(&ids, &schema, &ids, &desired, &reversed).is_empty());
        let mut removing_column = desired.clone();
        removing_column
            .tables
            .values_mut()
            .next()
            .unwrap()
            .columns
            .clear();
        assert_eq!(
            removals(&ids, &schema, &ids, &removing_column, &intents),
            ["column app.client.id"]
        );
        let mut renamed = ids.clone();
        *renamed.tables.values_mut().next().unwrap() = "app.client".parse().unwrap();
        *renamed.columns.values_mut().next().unwrap() = "app.client.key".parse().unwrap();
        *renamed.roles.values_mut().next().unwrap() = "writer".into();
        assert!(removals(&ids, &schema, &renamed, &desired, &[]).is_empty());
        // A stale retained id for an already absent column is not a deletion.
        let mut absent = schema.clone();
        absent.tables.values_mut().next().unwrap().columns.clear();
        assert!(removals(&ids, &absent, &ids, &absent, &[]).is_empty());
    }

    #[test]
    fn absorbed_annotations_preserve_reused_names_and_do_not_hide_their_removal() {
        let (mut ids, mut schema) = fixture();
        let original = ids.tables.values_mut().next().unwrap();
        *original = "app.client".parse().unwrap();
        ids.columns.values_mut().next().unwrap().table = original.clone();
        let table = schema
            .tables
            .remove(&"app.customer".parse().unwrap())
            .unwrap();
        schema.tables.insert(original.clone(), table);
        ids.tables.insert(
            Uid::generate(UidKind::Table),
            "app.customer".parse().unwrap(),
        );
        ids.columns.insert(
            Uid::generate(UidKind::Column),
            "app.customer.reused".parse().unwrap(),
        );
        schema.tables.insert(
            "app.customer".parse().unwrap(),
            Table {
                columns: [(
                    "reused".into(),
                    Column::new(ColumnType::new("integer", vec![])),
                )]
                .into(),
                ..Default::default()
            },
        );
        let renamed_column = schema
            .tables
            .get_mut(&"app.client".parse().unwrap())
            .unwrap();
        renamed_column.columns.insert(
            "key".into(),
            Column::new(ColumnType::new("integer", vec![])),
        );
        ids.columns.insert(
            Uid::generate(UidKind::Column),
            "app.client.key".parse().unwrap(),
        );
        ids.roles
            .insert(Uid::generate(UidKind::Role), "writer".into());
        schema.roles.insert("writer".into(), Role::default());
        let intents = [
            Intent::RenameTable {
                from: "app.customer".parse().unwrap(),
                to: "app.client".parse().unwrap(),
            },
            Intent::RenameColumn {
                table: "app.client".parse().unwrap(),
                from: "id".into(),
                to: "key".into(),
            },
            Intent::RenameRole {
                from: "reader".into(),
                to: "writer".into(),
            },
        ];
        assert!(removals(&ids, &schema, &ids, &schema, &intents).is_empty());
        let mut desired = schema.clone();
        desired.tables.remove(&"app.customer".parse().unwrap());
        desired
            .tables
            .get_mut(&"app.client".parse().unwrap())
            .unwrap()
            .columns
            .shift_remove("id");
        desired.roles.remove("reader");
        assert_eq!(
            removals(&ids, &schema, &ids, &desired, &intents),
            ["column app.client.id", "role reader", "table app.customer"]
        );
    }

    #[test]
    fn computed_column_removal_is_guarded_but_expression_changes_are_not() {
        let (ids, mut schema) = fixture();
        schema.tables.values_mut().next().unwrap().computed.insert(
            "total".into(),
            pbps_model::ComputedColumn {
                expression: "id + 1".into(),
                persisted: false,
                not_null: false,
            },
        );
        let mut desired = schema.clone();
        desired
            .tables
            .values_mut()
            .next()
            .unwrap()
            .computed
            .get_mut("total")
            .unwrap()
            .expression = "id + 2".into();
        assert!(removals(&ids, &schema, &ids, &desired, &[]).is_empty());
        desired.tables.values_mut().next().unwrap().computed.clear();
        assert_eq!(
            removals(&ids, &schema, &ids, &desired, &[]),
            ["column app.customer.total"]
        );
    }

    #[test]
    fn attaching_a_table_keeps_its_columns_under_the_parent_identity() {
        let (ids, schema) = fixture();
        let mut local = ids.clone();
        local.columns.clear();
        let mut desired = schema.clone();
        let table = desired.tables.values_mut().next().unwrap();
        table.columns.clear();
        table.partition_of = Some(pbps_model::PartitionOf {
            parent: "app.parent".parse().unwrap(),
            bound: pbps_model::PartitionBound::Default,
            columns: Default::default(),
        });
        assert!(removals(&ids, &schema, &local, &desired, &[]).is_empty());
    }

    #[test]
    fn the_complete_missing_commit_refusal_shows_hostile_operands_once() {
        struct Repo(std::path::PathBuf);
        impl Drop for Repo {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Repo(
            std::env::temp_dir().join(format!("pbps-checkout-{:032x}", rand::random::<u128>())),
        );
        std::fs::create_dir(&root.0).unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&root.0)
                .args(["init", "-q"])
                .output()
                .unwrap()
                .status
                .success()
        );
        let mut ids = IdsFile::default();
        let mut schema = Schema::default();
        let table = pbps_model::TableName::new("app", "t\n\x1b\u{202e}");
        ids.tables
            .insert(Uid::generate(UidKind::Table), table.clone());
        ids.columns.insert(
            Uid::generate(UidKind::Column),
            table.column("c\n\x1b\u{202e}"),
        );
        ids.roles
            .insert(Uid::generate(UidKind::Role), "r\n\x1b\u{202e}".into());
        schema.tables.insert(
            table,
            Table {
                columns: [(
                    "c\n\x1b\u{202e}".into(),
                    Column::new(ColumnType::new("integer", vec![])),
                )]
                .into(),
                ..Default::default()
            },
        );
        schema
            .roles
            .insert("r\n\x1b\u{202e}".into(), Role::default());
        let mut local = ids.clone();
        local.columns.clear();
        local.roles.clear();
        let removed = removals(&ids, &schema, &local, &schema, &[]);
        let sha = "0".repeat(40);
        let revision = pbps_db::ledger::StateRevision {
            id: 1,
            git_sha: Some(sha.clone().into()),
        };
        assert_eq!(
            assess(&root.0, "db\n\x1b\u{202e}", Some(&revision), &removed)
                .unwrap()
                .unwrap()
                .message,
            format!(
                "Cannot plan deletions against `db\\n\\u{{1b}}\\u{{202e}}`: deployed commit `{sha}` is not a commit this repository has.\nDeletions:\n  column app.t\\n\\u{{1b}}\\u{{202e}}.c\\n\\u{{1b}}\\u{{202e}}\n  role r\\n\\u{{1b}}\\u{{202e}}\nFetch the missing history, then merge or rebase onto `{sha}` and plan again."
            )
        );
    }

    #[test]
    fn no_evidence_notes_are_exact_and_hostile_names_are_shown_once() {
        let root = std::env::temp_dir();
        let revision = pbps_db::ledger::StateRevision {
            id: 7,
            git_sha: None,
        };
        assert_eq!(
            assess(&root, "db\n\x1b\u{202e}", Some(&revision), &[])
                .unwrap()
                .unwrap()
                .message,
            "Checkout ancestry was not checked for `db\\n\\u{1b}\\u{202e}`: ledger entry #7 has no Git commit. Planning continues without checkout-ancestry protection."
        );
        assert_eq!(
            assess(&root, "db", None, &[]).unwrap().unwrap().message,
            "Checkout ancestry was not checked for `db`: the ledger has no retained schema-establishing entry. Planning continues without checkout-ancestry protection."
        );
        let mut revision = revision;
        revision.git_sha = Some("a".repeat(40).into());
        assert_eq!(
            assess(&root, "db", Some(&revision), &[])
                .unwrap()
                .unwrap()
                .message,
            "Checkout ancestry was not checked for `db`: the planning project has no Git checkout. Planning continues without checkout-ancestry protection."
        );
        let mut ids = IdsFile::default();
        let mut schema = Schema::default();
        for n in 0..200 {
            let role: pbps_model::RoleName = format!("r{n:03}\n\x1b\u{202e}").into();
            ids.roles.insert(Uid::generate(UidKind::Role), role.clone());
            schema.roles.insert(role, Role::default());
        }
        let removed = removals(&ids, &schema, &IdsFile::default(), &Schema::default(), &[]);
        assert_eq!(removed.len(), 200);
        assert_eq!(removed[0], "role r000\\n\\u{1b}\\u{202e}");
        assert_eq!(removed[199], "role r199\\n\\u{1b}\\u{202e}");
    }
}
