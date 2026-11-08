//! `COMMENT ON` for a declared `description` (#2729, DEC-2729.1).
//!
//! **Managed only where declared.** A table or column with no `description` key has no
//! comment this tool compares, changes or removes; `description: ""` is managed as "no
//! comment" (the engine stores an empty comment as none, measured on 16 and 18), so it is
//! compared as `None` and planned as `IS NULL`.
//!
//! One pass over the declared tables, after every other pass has made its changes: an
//! object the plan creates, or a column it adds, has no comment to compare with, so its
//! declared text is written after the create (the drop and create of a column, and a
//! replaced table, are one of these). Anything else is compared with what the base holds,
//! under the object's uid, so a rename keeps its comment and a rename beside an edit
//! plans only the edit, under the final name. A drop plans nothing: it takes the comment
//! with it.

use crate::schema_diff::Side;
use pbps_model::{Change, CommentTarget, Text};
use std::collections::BTreeSet;

/// What a description says about the comment, once the engine's reading is applied: an
/// empty text is no comment at all.
fn comment(description: Option<&Text>) -> Option<&Text> {
    description.filter(|text| !text.is_empty())
}

/// The `SetComment` changes the declared descriptions need, given the changes already
/// planned. Empty for a dialect that does not hold descriptions: the caller asks first.
pub(crate) fn changes(base: Side<'_>, declared: Side<'_>, planned: &[Change]) -> Vec<Change> {
    let created: BTreeSet<_> = planned
        .iter()
        .filter_map(|c| {
            if let Change::CreateTable { name, .. } = c {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    let added: BTreeSet<_> = planned
        .iter()
        .filter_map(|c| {
            if let Change::AddColumn { table, name, .. } = c {
                Some((table, name))
            } else {
                None
            }
        })
        .collect();
    let declared_ids = declared.ids.index();
    let mut out = Vec::new();
    for (name, table) in &declared.schema.tables {
        let Some(uid) = declared_ids.table_uid(name) else {
            continue;
        };
        let was = base
            .ids
            .tables
            .get(uid)
            .and_then(|old| base.schema.tables.get(old))
            .filter(|_| !created.contains(name));
        if let Some(want) = &table.description {
            let want = comment(Some(want));
            if want != was.and_then(|t| comment(t.description.as_ref())) {
                out.push(Change::SetComment {
                    target: CommentTarget::Table {
                        uid: uid.clone(),
                        table: name.clone(),
                    },
                    text: want.cloned(),
                });
            }
        }
        for (column, spec) in &table.columns {
            let Some(want) = &spec.description else {
                continue;
            };
            let Some(column_uid) = declared_ids.column_uid(&name.column(column)) else {
                continue;
            };
            let have = (!added.contains(&(name, column)))
                .then(|| {
                    let old = base.ids.columns.get(column_uid)?;
                    was?.columns.get(&old.name)
                })
                .flatten()
                .and_then(|c| comment(c.description.as_ref()));
            let want = comment(Some(want));
            if want != have {
                out.push(Change::SetComment {
                    target: CommentTarget::Column {
                        uid: column_uid.clone(),
                        column: name.column(column),
                    },
                    text: want.cloned(),
                });
            }
        }
    }
    out
}

/// How many comments the database holds on declared tables and columns whose declaration
/// has no `description` key. Not drift and not a change: the plan counts them in one
/// informational line so an unmanaged comment is not invisible (DEC-2729.1).
pub fn undeclared(live: &pbps_model::Schema, declared: &pbps_model::Schema) -> usize {
    let mut count = 0;
    for (name, table) in &declared.tables {
        let Some(held) = live.tables.get(name) else {
            continue;
        };
        if table.description.is_none() && comment(held.description.as_ref()).is_some() {
            count += 1;
        }
        count += table
            .columns
            .iter()
            .filter(|(column, spec)| {
                spec.description.is_none()
                    && held
                        .columns
                        .get(*column)
                        .is_some_and(|c| comment(c.description.as_ref()).is_some())
            })
            .count();
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Context;
    use crate::schema_diff::diff;
    use indexmap::IndexMap;
    use pbps_dialect::{DescribingDialect, MinimalDialect};
    use pbps_model::{Column, Hints, IdsFile, Intent, Schema, Table};

    fn ctx() -> Context {
        Context {
            operator: "leon".into(),
            today: "2026-08-30".into(),
        }
    }

    fn column(description: Option<&str>) -> Column {
        let mut c = Column::new("int".parse().unwrap());
        c.description = description.map(Into::into);
        c
    }

    fn schema(name: &str, table: Option<&str>, column_text: Option<&str>) -> Schema {
        let mut columns = IndexMap::new();
        columns.insert("a".into(), column(column_text));
        let table = Table {
            description: table.map(Into::into),
            columns,
            ..Default::default()
        };
        let mut s = Schema::default();
        s.tables.insert(name.parse().unwrap(), table);
        s
    }

    /// The comment changes of `base` -> `declared`, in plan order, under `dialect`; the
    /// plan's other changes are returned beside them.
    fn plan(
        dialect: &(impl pbps_dialect::Dialect + ?Sized),
        base: &Schema,
        declared: &Schema,
        intents: &[Intent],
    ) -> Vec<Change> {
        let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let declared_ids = crate::resolve(declared, &base_ids, intents, &ctx())
            .unwrap()
            .ids;
        diff(
            Side {
                schema: base,
                ids: &base_ids,
            },
            Side {
                schema: declared,
                ids: &declared_ids,
            },
            dialect,
            &Hints::default(),
        )
        .unwrap()
        .changes()
        .changes
        .iter()
        .map(|p| p.change.clone())
        .collect()
    }

    /// Sorted: the order among comments is the plan's sort, not a property under test.
    fn comments(changes: &[Change]) -> Vec<(String, Option<String>)> {
        let mut found: Vec<_> = changes
            .iter()
            .filter_map(|c| {
                let Change::SetComment { target, text } = c else {
                    return None;
                };
                Some((
                    match target.column() {
                        Some(column) => column.raw().to_string(),
                        None => target.table().raw().to_string(),
                    },
                    text.as_ref().map(|t| t.raw().to_owned()),
                ))
            })
            .collect();
        found.sort();
        found
    }

    /// No `description` key is a comment nobody manages: nothing is planned, whatever the
    /// database holds. Negative: the same comment, once declared with another text, is.
    #[test]
    fn an_undeclared_comment_is_left_alone_and_a_declared_change_is_planned() {
        let base = schema("app.t", Some("held"), Some("held column"));
        let undeclared = schema("app.t", None, None);
        assert!(comments(&plan(&DescribingDialect, &base, &undeclared, &[])).is_empty());
        let edited = schema("app.t", Some("new"), Some("held column"));
        assert_eq!(
            comments(&plan(&DescribingDialect, &base, &edited, &[])),
            [("app.t".to_owned(), Some("new".to_owned()))]
        );
        let edited = schema("app.t", Some("held"), Some("new"));
        assert_eq!(
            comments(&plan(&DescribingDialect, &base, &edited, &[])),
            [("app.t.a".to_owned(), Some("new".to_owned()))]
        );
        // Negative: the declaration equal to the database plans nothing.
        let same = schema("app.t", Some("held"), Some("held column"));
        assert!(comments(&plan(&DescribingDialect, &base, &same, &[])).is_empty());
    }

    /// `""` is managed as no comment: it removes one, and plans nothing where there is none
    /// (the engine stores an empty comment as none).
    #[test]
    fn an_empty_description_removes_a_comment_and_plans_nothing_where_there_is_none() {
        let base = schema("app.t", Some("held"), Some("held column"));
        let empty = schema("app.t", Some(""), Some(""));
        assert_eq!(
            comments(&plan(&DescribingDialect, &base, &empty, &[])),
            [("app.t".to_owned(), None), ("app.t.a".to_owned(), None)]
        );
        let bare = schema("app.t", None, None);
        assert!(comments(&plan(&DescribingDialect, &bare, &empty, &[])).is_empty());
    }

    /// A created table and an added column have no comment to compare with: the declared
    /// text is written after the create. Negative: a table that stands and a column that
    /// stands are not written again.
    #[test]
    fn a_created_table_and_an_added_column_get_their_declared_comment() {
        let empty = Schema::default();
        let created = schema("app.t", Some("a table"), Some("a column"));
        let changes = plan(&DescribingDialect, &empty, &created, &[]);
        assert_eq!(
            comments(&changes),
            [
                ("app.t".to_owned(), Some("a table".to_owned())),
                ("app.t.a".to_owned(), Some("a column".to_owned()))
            ]
        );
        let create = changes
            .iter()
            .position(|c| matches!(c, Change::CreateTable { .. }))
            .unwrap();
        assert!(
            changes
                .iter()
                .enumerate()
                .all(|(at, c)| !matches!(c, Change::SetComment { .. }) || at > create)
        );
        let mut grown = schema("app.t", Some("a table"), Some("a column"));
        grown
            .tables
            .get_mut(&"app.t".parse::<pbps_model::TableName>().unwrap())
            .unwrap()
            .columns
            .insert("b".into(), column(Some("added")));
        let base = schema("app.t", Some("a table"), Some("a column"));
        assert_eq!(
            comments(&plan(&DescribingDialect, &base, &grown, &[])),
            [("app.t.b".to_owned(), Some("added".to_owned()))]
        );
    }

    /// An object the plan replaces, or a column it drops and adds again, loses its comment with
    /// the drop (measured on 16 and 18): the declared text is written again after the create,
    /// though the database held the same text a moment before.
    #[test]
    fn a_replaced_table_and_a_readded_column_get_their_declared_comment_again() {
        let base = schema("app.t", Some("same"), Some("same column"));
        let declared = base.clone();
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let run = |declared_ids: &IdsFile| {
            diff(
                Side {
                    schema: &base,
                    ids: &base_ids,
                },
                Side {
                    schema: &declared,
                    ids: declared_ids,
                },
                &DescribingDialect,
                &Hints::default(),
            )
            .unwrap()
            .changes()
            .changes
            .iter()
            .map(|p| p.change.clone())
            .collect::<Vec<_>>()
        };
        // Same identities: nothing to write.
        assert!(comments(&run(&base_ids)).is_empty());
        // A new identity for the table: dropped and created, both comments written again.
        let fresh = crate::resolve(&declared, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let changes = run(&fresh);
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, Change::DropTable { .. }))
        );
        assert_eq!(
            comments(&changes),
            [
                ("app.t".to_owned(), Some("same".to_owned())),
                ("app.t.a".to_owned(), Some("same column".to_owned()))
            ]
        );
        // The same table with a new identity for its column: dropped and added, one comment.
        let mut readded = base_ids.clone();
        let old = readded.columns.keys().next().unwrap().clone();
        let name = readded.columns.remove(&old).unwrap();
        readded.columns.insert("c_zzzzzz".parse().unwrap(), name);
        let changes = run(&readded);
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, Change::AddColumn { .. }))
        );
        assert_eq!(
            comments(&changes),
            [("app.t.a".to_owned(), Some("same column".to_owned()))]
        );
    }

    /// A comment follows its object's identity: a rename keeps it, so a pure rename plans no
    /// comment; a rename beside an edit plans one, under the final name. A comment is not
    /// rename intent either way.
    #[test]
    fn a_rename_keeps_the_comment_and_a_rename_with_an_edit_plans_one_under_the_final_name() {
        let base = schema("app.old", Some("held"), Some("held column"));
        let renamed = schema("app.new", Some("held"), Some("held column"));
        let intent = [Intent::RenameTable {
            from: "app.old".parse().unwrap(),
            to: "app.new".parse().unwrap(),
        }];
        let changes = plan(&DescribingDialect, &base, &renamed, &intent);
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, Change::RenameTable { .. }))
        );
        assert!(comments(&changes).is_empty(), "{changes:?}");
        let edited = schema("app.new", Some("edited"), Some("held column"));
        let changes = plan(&DescribingDialect, &base, &edited, &intent);
        assert_eq!(
            comments(&changes),
            [("app.new".to_owned(), Some("edited".to_owned()))]
        );
        let rename = changes
            .iter()
            .position(|c| matches!(c, Change::RenameTable { .. }))
            .unwrap();
        let comment = changes
            .iter()
            .position(|c| matches!(c, Change::SetComment { .. }))
            .unwrap();
        assert!(rename < comment, "{changes:?}");
    }

    /// An engine that does not hold descriptions plans none, whatever the declarations say
    /// (SQL Server stays as it was).
    #[test]
    fn an_engine_that_does_not_hold_descriptions_plans_no_comment() {
        let base = schema("app.t", Some("held"), Some("held column"));
        let edited = schema("app.t", Some("new"), Some("new"));
        assert!(comments(&plan(&MinimalDialect, &base, &edited, &[])).is_empty());
        assert!(!comments(&plan(&DescribingDialect, &base, &edited, &[])).is_empty());
    }

    /// The count the plan reports: a comment the database holds on a declared object whose
    /// declaration has no `description`. Negative: a declared one, an empty one, and one on an
    /// object nothing declares are not counted.
    #[test]
    fn undeclared_comments_are_counted_only_on_declared_objects() {
        let live = schema("app.t", Some("held"), Some("held column"));
        assert_eq!(undeclared(&live, &schema("app.t", None, None)), 2);
        assert_eq!(undeclared(&live, &schema("app.t", Some(""), None)), 1);
        assert_eq!(undeclared(&live, &schema("app.t", Some("x"), Some("y"))), 0);
        assert_eq!(undeclared(&live, &schema("app.other", None, None)), 0);
        let quiet = schema("app.t", Some(""), Some(""));
        assert_eq!(undeclared(&quiet, &schema("app.t", None, None)), 0);
    }
}
